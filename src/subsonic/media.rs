//! The file methods. `stream` and `download` send a track's file as it is,
//! whole or one byte range of it. `getCoverArt` sends an album's picture.

use super::Params;
use super::ids::Id;
use super::library::{content_type, suffix};
use super::response::ApiError;
use crate::output::clean_path;
use crate::store;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;
use sqlx::SqlitePool;
use std::io::{Read, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How much of a file is read at a time while it is sent.
const CHUNK_BYTES: u64 = 64 * 1024;

/// `stream` and `download`: the track's file, whole or the one range asked
/// for. `maxBitRate`, `format` and `timeOffset` are ignored: nothing is
/// transcoded, which the API allows. Neither counts as a play.
pub(crate) async fn track_file(
    pool: &SqlitePool,
    params: &Params,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let Some(Id::Song(id)) = Id::parse(params.required("id")?) else {
        return Err(ApiError::not_found("Song"));
    };
    let track = store::library_track(pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let path = Path::new(&track.path);
    file_response(path, content_type(&suffix(&track.path)), headers)
        .await
        .map_err(|error| {
            eprintln!("error: cannot send {}: {error}", clean_path(path));
            ApiError::not_found("File")
        })
}

/// What a `Range` header asks of a file of a given length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    /// The whole file: no range, or one the server may ignore.
    Full,
    /// Bytes `start..=end`.
    Part { start: u64, end: u64 },
    /// A range that starts past the end.
    Unsatisfiable,
}

/// Reads one `bytes=` range of a file of `len` bytes. The unit is matched
/// without regard to case. Several ranges, other units and malformed values
/// get the whole file, as RFC 9110 allows, and so does an empty file, which
/// has no byte to ask for. An end past what a number holds counts as the end.
fn parse_range(header: &str, len: u64) -> ByteRange {
    if len == 0 {
        return ByteRange::Full;
    }
    let header = header.trim();
    let Some(spec) = header
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &header[6..])
    else {
        return ByteRange::Full;
    };
    if spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last `suffix` bytes.
        let Some(suffix) = digits(last) else {
            return ByteRange::Full;
        };
        if suffix == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Part {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }
    let Some(start) = digits(first) else {
        return ByteRange::Full;
    };
    let end = if last.is_empty() {
        None
    } else if last.bytes().all(|byte| byte.is_ascii_digit()) {
        // Digits too many for a `u64` are an end past any file.
        match last.parse().unwrap_or(u64::MAX) {
            end if end >= start => Some(end),
            _ => return ByteRange::Full,
        }
    } else {
        return ByteRange::Full;
    };
    if start >= len {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Part {
        start,
        end: end.map_or(len - 1, |end| end.min(len - 1)),
    }
}

/// A decimal number of ASCII digits only.
fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The file at `path` as a response, honouring one `Range`. An `If-Range`
/// is honoured only when it is exactly the `Last-Modified` date this
/// response sends: the server keeps no other validators. Otherwise the whole
/// file is sent.
async fn file_response(
    path: &Path,
    content_type: &'static str,
    headers: &HeaderMap,
) -> std::io::Result<Response> {
    let mut file = tokio::fs::File::open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let len = metadata.len();
    let modified = metadata.modified().ok().map(httpdate::fmt_http_date);
    let range_applies = headers.get(header::IF_RANGE).is_none_or(|validator| {
        modified
            .as_deref()
            .is_some_and(|modified| validator.as_bytes() == modified.as_bytes())
    });
    let range = match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if range_applies => parse_range(value, len),
        _ => ByteRange::Full,
    };
    let (status, start, count) = match range {
        ByteRange::Full => (StatusCode::OK, 0, len),
        ByteRange::Part { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        ByteRange::Unsatisfiable => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response());
        }
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let mut response = Response::new(Body::from_stream(chunks(file, count)));
    *response.status_mut() = status;
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(value) =
            HeaderValue::from_str(&format!("bytes {start}-{}/{len}", start + count - 1))
    {
        response_headers.insert(header::CONTENT_RANGE, value);
    }
    if let Some(value) = modified.and_then(|modified| HeaderValue::from_str(&modified).ok()) {
        response_headers.insert(header::LAST_MODIFIED, value);
    }
    Ok(response)
}

/// The next `remaining` bytes of `file`, a chunk at a time. A file that
/// shrinks while it is sent ends the body early.
fn chunks(
    file: tokio::fs::File,
    remaining: u64,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
    futures_util::stream::unfold((file, remaining), |(mut file, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        // At most `CHUNK_BYTES`, so the cast can't truncate.
        let mut buffer = vec![0; remaining.min(CHUNK_BYTES) as usize];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(Bytes::from(buffer)), (file, remaining - read as u64)))
            }
            Err(error) => Some((Err(error), (file, 0))),
        }
    })
}

/// File stems that name a release's cover, ignoring case.
const COVER_STEMS: [&str; 4] = ["cover", "folder", "front", "album"];

/// Extensions of the picture files considered, ignoring case.
const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

/// The largest picture sent, in bytes.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// How many first bytes [`image_type`] reads.
const MAGIC_BYTES: u64 = 12;

/// A picture and its MIME type.
#[derive(Debug)]
struct Image {
    bytes: Vec<u8>,
    content_type: &'static str,
}

/// `getCoverArt`: the picture of the album an `al-` ID names, or of a `tr-`
/// ID's album. `size` is ignored: pictures are sent as they are.
pub(crate) async fn cover_art(pool: &SqlitePool, params: &Params) -> Result<Response, ApiError> {
    let missing = || ApiError::not_found("Cover art");
    let release_id = match Id::parse(params.required("id")?) {
        Some(Id::Album(id)) => id,
        Some(Id::Song(id)) => {
            store::library_track(pool, id)
                .await?
                .ok_or_else(missing)?
                .release_id
        }
        _ => return Err(missing()),
    };
    let release = store::library_release(pool, release_id)
        .await?
        .ok_or_else(missing)?;
    let first_track = store::first_track_path(pool, release_id)
        .await?
        .map(PathBuf::from);
    let directory = PathBuf::from(&release.directory);
    let found = tokio::task::spawn_blocking(move || find_cover(&directory, first_track.as_deref()))
        .await
        .map_err(|error| ApiError::from(anyhow::Error::from(error)))?;
    let image = found.ok_or_else(missing)?;
    Ok((
        [
            (header::CONTENT_TYPE, image.content_type),
            (header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        image.bytes,
    )
        .into_response())
}

/// A release's picture, from its folder and its first track's folder (the
/// disc folder of a merged release): first a file named like a cover, then
/// the front cover embedded in the first track, then any other picture.
/// Only real JPEG, PNG, GIF and WebP data counts, whatever the file name says.
fn find_cover(directory: &Path, first_track: Option<&Path>) -> Option<Image> {
    let mut folders = vec![directory.to_path_buf()];
    if let Some(parent) = first_track.and_then(Path::parent)
        && parent != directory
    {
        folders.push(parent.to_path_buf());
    }
    let pictures: Vec<PathBuf> = folders
        .iter()
        .flat_map(|folder| pictures_in(folder))
        .collect();
    let (named, others): (Vec<&PathBuf>, Vec<&PathBuf>) =
        pictures.iter().partition(|path| is_cover_name(path));
    named
        .into_iter()
        .find_map(|path| read_image(path))
        .or_else(|| first_track.and_then(embedded_cover))
        .or_else(|| others.into_iter().find_map(|path| read_image(path)))
}

/// The picture files directly in `folder`, by name.
fn pictures_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut pictures: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    IMAGE_EXTENSIONS
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(extension))
                })
        })
        .filter(|path| path.is_file())
        .collect();
    pictures.sort();
    pictures
}

fn is_cover_name(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            COVER_STEMS
                .iter()
                .any(|name| name.eq_ignore_ascii_case(stem))
        })
}

/// A picture file of at most [`MAX_IMAGE_BYTES`] whose first bytes are a real
/// picture. The file is opened once and checked through its handle. Its
/// first bytes are read before the rest, so a file that isn't a picture is
/// never read in full, and the rest is read through a limit, so a file that
/// grows after the check is not either.
fn read_image(path: &Path) -> Option<Image> {
    // Opening a FIFO waits for a writer, so a path that has stopped being a
    // regular file since the folder was listed is refused before `open`.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAGIC_BYTES).read_to_end(&mut bytes).ok()?;
    let content_type = image_type(&bytes)?;
    bytes.reserve(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_IMAGE_BYTES + 1 - MAGIC_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    Some(Image {
        bytes,
        content_type,
    })
}

/// The front cover embedded in `track`, else its first picture. The picture
/// is taken out of the parsed tag, so its bytes are not copied.
fn embedded_cover(track: &Path) -> Option<Image> {
    let mut file = Probe::open(track)
        .ok()?
        .options(ParseOptions::new().read_properties(false))
        .read()
        .ok()?;
    let (tag_type, index) = file
        .tags()
        .iter()
        .find_map(|tag| {
            let front = tag
                .pictures()
                .iter()
                .position(|picture| picture.pic_type() == PictureType::CoverFront)?;
            Some((tag.tag_type(), front))
        })
        .or_else(|| {
            file.tags()
                .iter()
                .find(|tag| !tag.pictures().is_empty())
                .map(|tag| (tag.tag_type(), 0))
        })?;
    let picture = file.tag(tag_type)?.pictures().get(index)?;
    if picture.data().len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let content_type = image_type(picture.data())?;
    let bytes = file.tag_mut(tag_type)?.remove_picture(index).into_data();
    Some(Image {
        bytes,
        content_type,
    })
}

/// The MIME type of JPEG, PNG, GIF or WebP data, by its first bytes.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::test_util::{call_json, call_raw, state};
    use crate::test_support::{ScratchDir, scanned, store_tracks};

    async fn read(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), 8 << 20)
            .await
            .unwrap()
            .to_vec()
    }

    /// The `Last-Modified` value a response for `path` carries.
    async fn whole_modified(path: &Path) -> String {
        let response = file_response(path, "audio/mpeg", &HeaderMap::new())
            .await
            .unwrap();
        response.headers()[header::LAST_MODIFIED]
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn an_empty_file_is_sent_whole_whatever_range_is_asked() {
        let dir = ScratchDir::new("subsonic-empty");
        let path = dir.path().join("empty.mp3");
        std::fs::write(&path, b"").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        let response = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(read(response).await.is_empty());
    }

    #[test]
    fn ranges_follow_rfc_9110() {
        let part = |start, end| ByteRange::Part { start, end };
        assert_eq!(parse_range("bytes=0-9", 100), part(0, 9));
        assert_eq!(parse_range("bytes=90-", 100), part(90, 99));
        assert_eq!(parse_range("bytes=-10", 100), part(90, 99));
        assert_eq!(parse_range("bytes=-500", 100), part(0, 99));
        assert_eq!(parse_range("bytes=50-500", 100), part(50, 99));
        assert_eq!(parse_range(" bytes=1-1 ", 100), part(1, 1));
        assert_eq!(parse_range("bytes=100-", 100), ByteRange::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), ByteRange::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), ByteRange::Full, "empty file");
        assert_eq!(parse_range("bytes=0-", 0), ByteRange::Full, "empty file");
        assert_eq!(
            parse_range("Bytes=0-1", 100),
            part(0, 1),
            "unit in any case"
        );
        assert_eq!(parse_range("BYTES=5-", 100), part(5, 99));
        let huge = "99999999999999999999999";
        assert_eq!(parse_range(&format!("bytes=5-{huge}"), 100), part(5, 99));
        assert_eq!(parse_range(&format!("bytes={huge}-"), 100), ByteRange::Full);
        for ignored in [
            "bytes=0-1,5-6",
            "bytes=9-3",
            "bytes=+1-2",
            "items=0-1",
            "bytes=x",
            "bytes=-",
        ] {
            assert_eq!(parse_range(ignored, 100), ByteRange::Full, "{ignored}");
        }
    }

    #[tokio::test]
    async fn a_file_is_sent_whole_or_by_range() {
        let dir = ScratchDir::new("subsonic-range");
        let path = dir.path().join("song.mp3");
        // Several chunks long, with no repeating pattern a short read could hide.
        let data: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let whole = file_response(&path, "audio/mpeg", &HeaderMap::new())
            .await
            .unwrap();
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(whole.headers()[header::CONTENT_LENGTH], "200000");
        assert_eq!(whole.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(whole.headers()[header::CONTENT_TYPE], "audio/mpeg");
        assert!(whole.headers().contains_key(header::LAST_MODIFIED));
        assert_eq!(read(whole).await, data);

        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=70000-70009"));
        let part = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.headers()[header::CONTENT_RANGE],
            "bytes 70000-70009/200000"
        );
        assert_eq!(part.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(read(part).await, &data[70_000..70_010]);

        headers.insert(header::RANGE, HeaderValue::from_static("bytes=300000-"));
        let past = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(past.headers()[header::CONTENT_RANGE], "bytes */200000");

        headers.insert(header::RANGE, HeaderValue::from_static("bytes=10-19"));
        headers.insert(header::IF_RANGE, HeaderValue::from_static("\"etag\""));
        let guarded = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(guarded.status(), StatusCode::OK);

        // An `If-Range` that is this response's `Last-Modified` keeps the range.
        let modified = whole_modified(&path).await;
        headers.insert(header::IF_RANGE, HeaderValue::from_str(&modified).unwrap());
        let kept = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(kept.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(kept.headers()[header::CONTENT_RANGE], "bytes 10-19/200000");
        headers.insert(
            header::IF_RANGE,
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
        );
        let stale = file_response(&path, "audio/mpeg", &headers).await.unwrap();
        assert_eq!(stale.status(), StatusCode::OK);
        assert_eq!(stale.headers()[header::CONTENT_LENGTH], "200000");

        let gone = dir.path().join("gone.mp3");
        assert!(
            file_response(&gone, "audio/mpeg", &HeaderMap::new())
                .await
                .is_err()
        );
        assert!(
            file_response(dir.path(), "audio/mpeg", &HeaderMap::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stream_and_download_send_the_track_file() {
        let dir = ScratchDir::new("subsonic-stream");
        let path = dir.path().join("a").join("01.mp3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..100_000u32).map(|n| (n % 241) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let state = state().await;
        let track = scanned(
            path.to_str().unwrap(),
            "Blue",
            Some("Artist"),
            "One",
            1,
            2001,
        );
        store_tracks(&state.pool, dir.path().to_str().unwrap(), &[track]).await;
        let id = Id::song(
            store::library_tracks(&state.pool, None, 1, 0)
                .await
                .unwrap()[0]
                .id,
        );

        for method in ["stream", "download", "stream.view"] {
            let pairs = [
                ("id", id.as_str()),
                ("maxBitRate", "128"),
                ("format", "opus"),
            ];
            let response = call_raw(&state, method, &pairs, &HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "audio/mpeg",
                "{method}"
            );
            assert_eq!(read(response).await, data, "{method}");
        }

        // A `Range` header reaches the file through `dispatch`.
        let mut ranged = HeaderMap::new();
        ranged.insert(header::RANGE, HeaderValue::from_static("bytes=10-19"));
        let response = call_raw(&state, "stream", &[("id", id.as_str())], &ranged).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            format!("bytes 10-19/{}", data.len())
        );
        assert_eq!(read(response).await, &data[10..20]);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            call_json(&state, "stream", &[("id", &id)]).await["error"]["code"],
            70
        );
        assert_eq!(
            call_json(&state, "stream", &[("id", "al-1")]).await["error"]["code"],
            70
        );
        assert_eq!(call_json(&state, "stream", &[]).await["error"]["code"], 10);
    }

    const JPEG: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ];
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

    /// Writes a WAV file with `picture` embedded as its front cover.
    fn write_wav_with_cover(path: &Path, picture: &[u8]) {
        use lofty::config::WriteOptions;
        use lofty::file::AudioFile;
        use lofty::picture::{MimeType, Picture};
        use lofty::tag::{Tag, TagType};

        crate::test_support::write_wav(path);
        let mut file = Probe::open(path).unwrap().read().unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(
            Picture::unchecked(picture.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
        file.insert_tag(tag);
        file.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn image_types_come_from_the_first_bytes() {
        assert_eq!(image_type(JPEG), Some("image/jpeg"));
        assert_eq!(image_type(PNG), Some("image/png"));
        assert_eq!(image_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            image_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_type(b"<svg onload=alert(1)>"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn covers_are_found_by_name_then_embedded_then_any_picture() {
        let dir = ScratchDir::new("subsonic-covers");
        let album = dir.path().join("album");
        std::fs::create_dir_all(&album).unwrap();
        let track = album.join("01.wav");
        write_wav_with_cover(&track, JPEG);
        std::fs::write(album.join("00-proof.jpg"), b"GIF89a proof").unwrap();
        std::fs::write(album.join("Cover.PNG"), PNG).unwrap();
        std::fs::write(album.join("folder.jpg"), b"not a picture").unwrap();

        let found = find_cover(&album, Some(track.as_path())).unwrap();
        assert_eq!(
            (found.content_type, found.bytes.as_slice()),
            ("image/png", PNG)
        );

        std::fs::remove_file(album.join("Cover.PNG")).unwrap();
        let found = find_cover(&album, Some(track.as_path())).unwrap();
        assert_eq!(
            (found.content_type, found.bytes.as_slice()),
            ("image/jpeg", JPEG),
            "embedded"
        );

        let plain = album.join("02.wav");
        crate::test_support::write_wav(&plain);
        let found = find_cover(&album, Some(plain.as_path())).unwrap();
        assert_eq!(found.content_type, "image/gif", "any picture in the folder");

        std::fs::remove_file(album.join("00-proof.jpg")).unwrap();
        assert!(
            find_cover(&album, Some(plain.as_path())).is_none(),
            "folder.jpg is not a picture"
        );
    }

    #[test]
    fn a_picture_over_the_size_cap_is_skipped() {
        let dir = ScratchDir::new("subsonic-big-cover");
        let album = dir.path().join("album");
        std::fs::create_dir_all(&album).unwrap();
        // "cover" sorts before "folder", so the oversized file is tried first.
        let big = album.join("cover.jpg");
        std::fs::write(&big, JPEG).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        std::fs::write(album.join("folder.png"), PNG).unwrap();
        assert!(read_image(&big).is_none());
        let found = find_cover(&album, None).unwrap();
        assert_eq!(
            (found.content_type, found.bytes.as_slice()),
            ("image/png", PNG)
        );

        // At the cap exactly, the picture is read whole.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES)
            .unwrap();
        let found = read_image(&big).unwrap();
        assert_eq!(found.content_type, "image/jpeg");
        assert_eq!(found.bytes.len() as u64, MAX_IMAGE_BYTES);
    }

    #[test]
    fn a_merged_release_also_looks_in_its_first_track_folder() {
        let dir = ScratchDir::new("subsonic-disc-cover");
        let disc = dir.path().join("CD1");
        std::fs::create_dir_all(&disc).unwrap();
        std::fs::write(disc.join("cover.jpg"), JPEG).unwrap();
        let found = find_cover(dir.path(), Some(disc.join("01.mp3").as_path())).unwrap();
        assert_eq!(found.bytes, JPEG);
    }

    #[tokio::test]
    async fn get_cover_art_serves_the_album_picture_for_album_and_song_ids() {
        let dir = ScratchDir::new("subsonic-cover-art");
        let folder = dir.path().join("a");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("01.mp3");
        std::fs::write(&path, b"audio").unwrap();
        std::fs::write(folder.join("cover.jpg"), JPEG).unwrap();
        let state = state().await;
        let track = scanned(
            path.to_str().unwrap(),
            "Blue",
            Some("Artist"),
            "One",
            1,
            2001,
        );
        store_tracks(&state.pool, dir.path().to_str().unwrap(), &[track]).await;
        let track = store::library_tracks(&state.pool, None, 1, 0)
            .await
            .unwrap()
            .remove(0);

        for id in [Id::album(track.release_id), Id::song(track.id)] {
            let pairs = [("id", id.as_str()), ("size", "300")];
            let response = call_raw(&state, "getCoverArt", &pairs, &HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::OK, "{id}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
            assert_eq!(read(response).await, JPEG);
        }
        let artist = call_json(&state, "getCoverArt", &[("id", &Id::artist("Artist"))]).await;
        assert_eq!(artist["error"]["code"], 70);
        std::fs::remove_file(folder.join("cover.jpg")).unwrap();
        let album = Id::album(track.release_id);
        assert_eq!(
            call_json(&state, "getCoverArt", &[("id", &album)]).await["error"]["code"],
            70
        );
    }
}
