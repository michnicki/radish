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
use sqlx::SqlitePool;
use std::io::SeekFrom;
use std::path::Path;
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

/// Reads one `bytes=` range of a file of `len` bytes. Several ranges, other
/// units and malformed values get the whole file, as RFC 9110 allows.
fn parse_range(header: &str, len: u64) -> ByteRange {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
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
        if suffix == 0 || len == 0 {
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
    } else {
        match digits(last) {
            Some(end) if end >= start => Some(end),
            _ => return ByteRange::Full,
        }
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

/// The file at `path` as a response, honouring one `Range`. Any `If-Range`
/// gets the whole file: the server keeps no validators to check it against.
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
    let range = match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if !headers.contains_key(header::IF_RANGE) => parse_range(value, len),
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
    if let Ok(modified) = metadata.modified()
        && let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(modified))
    {
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
        assert_eq!(parse_range("bytes=-1", 0), ByteRange::Unsatisfiable);
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
}
