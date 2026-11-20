//! The browsing, list and search methods, and the objects they return.
//!
//! Every album object also carries the fields of a folder entry (`isDir`,
//! `title`, `parent`). That lets the folder-style methods (`getIndexes`,
//! `getMusicDirectory`, `getAlbumList`, `search2`) return the same objects
//! as the ID3 ones.

use super::Params;
use super::ids::Id;
use super::response::{ApiError, Payload};
use crate::store::{self, LibraryArtist, LibraryRelease, LibraryTrack, ReleaseOrder};
use serde::Serialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Leading words left out when artists are sorted and indexed.
pub(crate) const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// The ID of the one music folder: the whole library.
pub(crate) const MUSIC_FOLDER_ID: i64 = 1;

/// An album artist (ArtistID3).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Artist {
    id: String,
    name: String,
    album_count: i64,
}

impl Artist {
    fn of(artist: &LibraryArtist) -> Self {
        Self {
            id: Id::artist(&artist.artist),
            name: store::display_artist(&artist.artist).to_string(),
            album_count: artist.album_count,
        }
    }
}

/// An artist with its albums.
#[derive(Debug, Serialize)]
struct ArtistWithAlbums {
    #[serde(flatten)]
    artist: Artist,
    album: Vec<Album>,
}

/// An album (AlbumID3) that can also stand as a folder entry (Child).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Album {
    id: String,
    parent: String,
    is_dir: bool,
    title: String,
    name: String,
    album: String,
    artist: String,
    artist_id: String,
    song_count: i64,
    duration: i64,
    created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<i64>,
    cover_art: String,
    media_type: &'static str,
    music_brainz_id: String,
}

impl Album {
    fn of(release: &LibraryRelease) -> Self {
        let name = release.display_name();
        let artist_id = Id::artist(&release.artist);
        Self {
            id: Id::album(release.id),
            parent: artist_id.clone(),
            is_dir: true,
            title: name.clone(),
            album: name.clone(),
            name,
            artist: release.display_artist().to_string(),
            artist_id,
            song_count: release.track_count,
            duration: release.duration_secs.round() as i64,
            created: release.created_at.clone(),
            year: release.year,
            cover_art: Id::album(release.id),
            media_type: "album",
            music_brainz_id: release.musicbrainz_id.clone().unwrap_or_default(),
        }
    }
}

/// An album with its songs.
#[derive(Debug, Serialize)]
struct AlbumWithSongs {
    #[serde(flatten)]
    album: Album,
    song: Vec<Song>,
}

/// A song (Child).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Song {
    id: String,
    parent: String,
    is_dir: bool,
    title: String,
    album: String,
    artist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    track: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disc_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<i64>,
    cover_art: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    content_type: &'static str,
    suffix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bit_rate: Option<i64>,
    path: String,
    album_id: String,
    artist_id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    created: String,
    media_type: &'static str,
    music_brainz_id: String,
}

impl Song {
    /// `size` is the file's length, when it was looked up; only small
    /// responses do that (see [`file_sizes`]).
    fn of(track: &LibraryTrack, size: Option<u64>) -> Self {
        let suffix = suffix(&track.path);
        let album_artist = store::display_artist(&track.album_artist);
        Self {
            id: Id::song(track.id),
            parent: Id::album(track.release_id),
            is_dir: false,
            title: track.display_title(),
            album: track.display_album(),
            artist: track
                .artist
                .clone()
                .unwrap_or_else(|| album_artist.to_string()),
            track: track.track_number,
            disc_number: track.disc_number,
            year: track.year,
            cover_art: Id::album(track.release_id),
            size,
            content_type: content_type(&suffix),
            suffix,
            duration: track.duration_secs.map(|secs| secs.round() as i64),
            // Tags and embedded pictures count too, so this runs a little high.
            bit_rate: size
                .zip(track.duration_secs)
                .filter(|(_, secs)| *secs > 0.0)
                .map(|(bytes, secs)| (bytes as f64 * 8.0 / secs / 1000.0).round() as i64),
            path: relative_path(track),
            album_id: Id::album(track.release_id),
            artist_id: Id::artist(&track.album_artist),
            kind: "music",
            created: track.created_at.clone(),
            media_type: "song",
            music_brainz_id: track.musicbrainz_id.clone().unwrap_or_default(),
        }
    }
}

/// A file's extension in lowercase, `""` when it has none.
pub(crate) fn suffix(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The MIME type of an audio file with `suffix`, for the scanner's extensions.
pub(crate) fn content_type(suffix: &str) -> &'static str {
    match suffix {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    }
}

/// The track's path under its imported root, which clients use to name cached
/// files; its file name when it isn't under the root.
fn relative_path(track: &LibraryTrack) -> String {
    let path = Path::new(&track.path);
    track
        .root
        .as_deref()
        .and_then(|root| path.strip_prefix(root).ok())
        .or_else(|| path.file_name().map(Path::new))
        .map_or_else(
            || track.path.clone(),
            |relative| relative.to_string_lossy().into_owned(),
        )
}

/// The length of each track's file, `None` for a file that is gone. Runs on
/// the blocking pool; used only for one album or one song, never for pages
/// of a client's full sync, which would stat millions of files.
async fn file_sizes(tracks: &[LibraryTrack]) -> Result<Vec<Option<u64>>, ApiError> {
    let paths: Vec<String> = tracks.iter().map(|track| track.path.clone()).collect();
    tokio::task::spawn_blocking(move || {
        paths
            .iter()
            .map(|path| {
                std::fs::metadata(path)
                    .ok()
                    .filter(std::fs::Metadata::is_file)
                    .map(|metadata| metadata.len())
            })
            .collect()
    })
    .await
    .map_err(|error| ApiError::from(anyhow::Error::from(error)))
}

/// `getMusicFolders`: one folder, the whole library.
pub(crate) fn music_folders() -> Payload {
    Payload::new(
        "musicFolders",
        json!({"musicFolder": [{"id": MUSIC_FOLDER_ID, "name": "Music"}]}),
    )
}

/// Which artist list `getArtists` or `getIndexes` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtistList {
    /// `getArtists`.
    Artists,
    /// `getIndexes`.
    Indexes,
}

/// The body of `getArtists` and `getIndexes`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtistIndexes {
    ignored_articles: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified: Option<i64>,
    index: Vec<Index>,
}

/// The artists filed under one letter.
#[derive(Debug, Serialize)]
struct Index {
    name: String,
    artist: Vec<Artist>,
}

/// `getArtists` and `getIndexes`: every album artist, under the first letter
/// of its sort name.
pub(crate) async fn artists(pool: &SqlitePool, list: ArtistList) -> Result<Payload, ApiError> {
    let artists = store::library_artists(pool, None, -1, 0).await?;
    let (key, last_modified) = match list {
        ArtistList::Artists => ("artists", None),
        // Always now, so clients never keep a stale index.
        ArtistList::Indexes => ("indexes", Some(now_millis())),
    };
    Ok(Payload::new(
        key,
        ArtistIndexes {
            ignored_articles: IGNORED_ARTICLES,
            last_modified,
            index: artist_index(&artists),
        },
    ))
}

/// Artists grouped by [`index_letter`], each group sorted by [`sort_name`].
fn artist_index(artists: &[LibraryArtist]) -> Vec<Index> {
    let mut groups: BTreeMap<String, Vec<(String, Artist)>> = BTreeMap::new();
    for artist in artists {
        let artist = Artist::of(artist);
        let sort = sort_name(&artist.name).to_lowercase();
        groups
            .entry(index_letter(&sort))
            .or_default()
            .push((sort, artist));
    }
    groups
        .into_iter()
        .map(|(name, mut members)| {
            members.sort_by(|a, b| a.0.cmp(&b.0));
            Index {
                name,
                artist: members.into_iter().map(|(_, artist)| artist).collect(),
            }
        })
        .collect()
}

/// The name without a leading ignored article: "The Cure" sorts as "Cure".
fn sort_name(name: &str) -> &str {
    for article in IGNORED_ARTICLES.split(' ') {
        let Some(head) = name.get(..article.len()) else {
            continue;
        };
        if head.eq_ignore_ascii_case(article)
            && let Some(rest) = name[article.len()..].strip_prefix(' ')
            && !rest.trim().is_empty()
        {
            return rest.trim_start();
        }
    }
    name
}

/// The index a sort name is filed under: its first letter, upper-cased, or
/// `#` when it doesn't start with a letter.
fn index_letter(sort_name: &str) -> String {
    match sort_name.chars().next() {
        Some(first) if first.is_alphabetic() => first.to_uppercase().collect(),
        _ => "#".to_string(),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `getArtist`: an album artist with its albums.
pub(crate) async fn artist(pool: &SqlitePool, params: &Params) -> Result<Payload, ApiError> {
    let (key, releases) = artist_releases(pool, params).await?;
    let artist = ArtistWithAlbums {
        artist: Artist {
            id: Id::artist(&key),
            name: artist_name(&releases),
            album_count: i64::try_from(releases.len()).unwrap_or(i64::MAX),
        },
        album: releases.iter().map(Album::of).collect(),
    };
    Ok(Payload::new("artist", artist))
}

/// The name the artist list shows for these releases: the smallest of their
/// keys, which differ at most in case. `releases` is not empty.
fn artist_name(releases: &[LibraryRelease]) -> String {
    let key = releases
        .iter()
        .map(|release| release.artist.as_str())
        .min()
        .unwrap_or_default();
    store::display_artist(key).to_string()
}

/// The artist key the `id` parameter names, and the artist's releases (at
/// least one).
async fn artist_releases(
    pool: &SqlitePool,
    params: &Params,
) -> Result<(String, Vec<LibraryRelease>), ApiError> {
    let Some(Id::Artist(key)) = Id::parse(params.required("id")?) else {
        return Err(ApiError::not_found("Artist"));
    };
    let releases = store::artist_releases(pool, &key).await?;
    if releases.is_empty() {
        return Err(ApiError::not_found("Artist"));
    }
    Ok((key, releases))
}

/// `getAlbum`: an album with its songs.
pub(crate) async fn album(pool: &SqlitePool, params: &Params) -> Result<Payload, ApiError> {
    let (release, songs) = album_songs(pool, params).await?;
    Ok(Payload::new(
        "album",
        AlbumWithSongs {
            album: Album::of(&release),
            song: songs,
        },
    ))
}

/// The release the `id` parameter names, and its songs with their sizes.
async fn album_songs(
    pool: &SqlitePool,
    params: &Params,
) -> Result<(LibraryRelease, Vec<Song>), ApiError> {
    let Some(Id::Album(id)) = Id::parse(params.required("id")?) else {
        return Err(ApiError::not_found("Album"));
    };
    let release = store::library_release(pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found("Album"))?;
    let tracks = store::release_tracks(pool, id).await?;
    let sizes = file_sizes(&tracks).await?;
    let songs = tracks
        .iter()
        .zip(sizes)
        .map(|(track, size)| Song::of(track, size))
        .collect();
    Ok((release, songs))
}

/// `getSong`.
pub(crate) async fn song(pool: &SqlitePool, params: &Params) -> Result<Payload, ApiError> {
    let Some(Id::Song(id)) = Id::parse(params.required("id")?) else {
        return Err(ApiError::not_found("Song"));
    };
    let track = store::library_track(pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let size = file_sizes(std::slice::from_ref(&track))
        .await?
        .pop()
        .flatten();
    Ok(Payload::new("song", Song::of(&track, size)))
}

/// `getMusicDirectory`: an artist's albums or an album's songs, as folders.
pub(crate) async fn music_directory(
    pool: &SqlitePool,
    params: &Params,
) -> Result<Payload, ApiError> {
    match Id::parse(params.required("id")?) {
        Some(Id::Artist(_)) => {
            let (key, releases) = artist_releases(pool, params).await?;
            let albums: Vec<Album> = releases.iter().map(Album::of).collect();
            Ok(Payload::value(
                "directory",
                json!({
                    "id": Id::artist(&key),
                    "name": artist_name(&releases),
                    "child": albums,
                }),
            ))
        }
        Some(Id::Album(_)) => {
            let (release, songs) = album_songs(pool, params).await?;
            Ok(Payload::value(
                "directory",
                json!({
                    "id": Id::album(release.id),
                    "name": release.display_name(),
                    "parent": Id::artist(&release.artist),
                    "child": songs,
                }),
            ))
        }
        _ => Err(ApiError::not_found("Directory")),
    }
}

/// `getAlbumList` (`key` = `albumList`) and `getAlbumList2`
/// (`key` = `albumList2`). Lists that need genres, ratings or play history
/// are empty, since the server keeps none.
pub(crate) async fn album_list(
    pool: &SqlitePool,
    params: &Params,
    key: &str,
) -> Result<Payload, ApiError> {
    let size = params.count("size", 10)?;
    let offset = params.offset("offset")?;
    let order = match params.required("type")? {
        "random" => Some(ReleaseOrder::Random),
        "newest" => Some(ReleaseOrder::Newest),
        "alphabeticalByName" => Some(ReleaseOrder::Name),
        "alphabeticalByArtist" => Some(ReleaseOrder::Artist),
        "byYear" => {
            let from = params
                .integer("fromYear")?
                .ok_or_else(|| ApiError::missing("fromYear"))?;
            let to = params
                .integer("toYear")?
                .ok_or_else(|| ApiError::missing("toYear"))?;
            Some(ReleaseOrder::Years { from, to })
        }
        "byGenre" | "frequent" | "recent" | "starred" | "highest" => None,
        _ => return Err(ApiError::invalid("type")),
    };
    let releases = match order {
        Some(order) if size > 0 => store::library_releases(pool, order, size, offset).await?,
        _ => Vec::new(),
    };
    let albums: Vec<Album> = releases.iter().map(Album::of).collect();
    Ok(Payload::value(key, json!({ "album": albums })))
}

/// `getRandomSongs`.
pub(crate) async fn random_songs(pool: &SqlitePool, params: &Params) -> Result<Payload, ApiError> {
    let size = usize::try_from(params.count("size", 10)?).unwrap_or(0);
    let tracks = store::random_tracks(
        pool,
        size,
        params.integer("fromYear")?,
        params.integer("toYear")?,
    )
    .await?;
    let songs: Vec<Song> = tracks.iter().map(|track| Song::of(track, None)).collect();
    Ok(Payload::value("randomSongs", json!({ "song": songs })))
}

/// `search2` (`key` = `searchResult2`) and `search3` (`key` = `searchResult3`):
/// artists, albums and songs whose name, title or artist contains the query.
/// An empty query returns everything in a stable order, which clients use to
/// sync the whole library page by page.
pub(crate) async fn search(
    pool: &SqlitePool,
    params: &Params,
    key: &str,
) -> Result<Payload, ApiError> {
    let query = search_text(params.get("query").unwrap_or(""));
    let artist_count = params.count("artistCount", 20)?;
    let artist_offset = params.offset("artistOffset")?;
    let album_count = params.count("albumCount", 20)?;
    let album_offset = params.offset("albumOffset")?;
    let song_count = params.count("songCount", 20)?;
    let song_offset = params.offset("songOffset")?;
    let (artists, releases, tracks) = tokio::try_join!(
        async {
            if artist_count == 0 {
                Ok(Vec::new())
            } else {
                store::library_artists(pool, query, artist_count, artist_offset).await
            }
        },
        async {
            match query {
                _ if album_count == 0 => Ok(Vec::new()),
                None => {
                    store::library_releases(pool, ReleaseOrder::Id, album_count, album_offset).await
                }
                Some(query) => {
                    store::search_library_releases(pool, query, album_count, album_offset).await
                }
            }
        },
        async {
            if song_count == 0 {
                Ok(Vec::new())
            } else {
                store::library_tracks(pool, query, song_count, song_offset).await
            }
        },
    )?;
    let artists: Vec<Artist> = artists.iter().map(Artist::of).collect();
    let albums: Vec<Album> = releases.iter().map(Album::of).collect();
    let songs: Vec<Song> = tracks.iter().map(|track| Song::of(track, None)).collect();
    Ok(Payload::value(
        key,
        json!({ "artist": artists, "album": albums, "song": songs }),
    ))
}

/// The text a search looks for: trimmed and without surrounding quotes.
/// `None`, meaning everything, when nothing is left or it is `*`.
fn search_text(query: &str) -> Option<&str> {
    let text = query.trim().trim_matches('"').trim();
    (!text.is_empty() && text != "*").then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::test_util::{call_json, call_xml, state};
    use crate::test_support::{ScratchDir, scanned, seed_release, store_tracks};
    use serde_json::Value;

    #[test]
    fn sort_names_drop_a_leading_article() {
        assert_eq!(sort_name("The Cure"), "Cure");
        assert_eq!(sort_name("los Lobos"), "Lobos");
        assert_eq!(sort_name("The"), "The");
        assert_eq!(sort_name("The "), "The ");
        assert_eq!(sort_name("Theatre"), "Theatre");
        assert_eq!(index_letter("cure"), "C");
        assert_eq!(index_letter("ärzte"), "Ä");
        assert_eq!(index_letter("10cc"), "#");
        assert_eq!(index_letter(""), "#");
    }

    #[tokio::test]
    async fn artists_are_indexed_by_letter_with_their_album_counts() {
        let state = state().await;
        seed_release(&state.pool, "/music/a", "Blue", &["One", "Two"]).await;
        store_tracks(
            &state.pool,
            "/other",
            &[scanned(
                "/other/c/01.mp3",
                "Head",
                Some("The Cure"),
                "x",
                1,
                1985,
            )],
        )
        .await;

        let reply = call_json(&state, "getArtists", &[]).await;
        assert_eq!(reply["artists"]["ignoredArticles"], IGNORED_ARTICLES);
        let index = reply["artists"]["index"].as_array().unwrap();
        let letters: Vec<&str> = index
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(letters, ["A", "C"]);
        assert_eq!(
            index[0]["artist"][0],
            json!({"id": Id::artist("Artist"), "name": "Artist", "albumCount": 1})
        );
        assert_eq!(index[1]["artist"][0]["name"], "The Cure");

        let indexes = call_json(&state, "getIndexes", &[]).await;
        assert_eq!(indexes["indexes"]["ignoredArticles"], IGNORED_ARTICLES);
        assert!(indexes["indexes"]["lastModified"].as_i64().unwrap() > 0);
        assert_eq!(indexes["indexes"]["index"], reply["artists"]["index"]);

        let folders = call_json(&state, "getMusicFolders", &[]).await;
        assert_eq!(
            folders["musicFolders"]["musicFolder"],
            json!([{"id": 1, "name": "Music"}])
        );
    }

    #[tokio::test]
    async fn an_artist_leads_to_its_albums_and_an_album_to_its_songs() {
        let state = state().await;
        let release = seed_release(&state.pool, "/music/a", "Blue", &["One", "Two"]).await;
        let artist_id = Id::artist("Artist");
        let album_id = Id::album(release.id);

        let artist = call_json(&state, "getArtist", &[("id", &artist_id)]).await;
        assert_eq!(artist["artist"]["id"], artist_id);
        assert_eq!(artist["artist"]["name"], "Artist");
        assert_eq!(artist["artist"]["albumCount"], 1);
        let album = &artist["artist"]["album"][0];
        assert_eq!(album["id"], album_id);
        assert_eq!(album["name"], "Blue");
        assert_eq!(album["title"], "Blue");
        assert_eq!(album["isDir"], true);
        assert_eq!(album["parent"], artist_id);
        assert_eq!(album["artistId"], artist_id);
        assert_eq!(album["songCount"], 2);
        assert_eq!(album["duration"], 360);
        assert_eq!(album["year"], 2001);
        assert_eq!(album["coverArt"], album_id);
        assert_eq!(album["mediaType"], "album");
        assert_eq!(album["musicBrainzId"], "");
        assert!(album["created"].as_str().unwrap().ends_with('Z'));

        let reply = call_json(&state, "getAlbum", &[("id", &album_id)]).await;
        assert_eq!(reply["album"]["name"], "Blue");
        let songs = reply["album"]["song"].as_array().unwrap();
        assert_eq!(songs.len(), 2);
        let song = &songs[0];
        assert_eq!(song["title"], "One");
        assert_eq!(song["isDir"], false);
        assert_eq!(song["parent"], album_id);
        assert_eq!(song["albumId"], album_id);
        assert_eq!(song["artistId"], artist_id);
        assert_eq!(song["album"], "Blue");
        assert_eq!(song["artist"], "Artist");
        assert_eq!(song["track"], 1);
        assert_eq!(song["duration"], 180);
        assert_eq!(song["suffix"], "flac");
        assert_eq!(song["contentType"], "audio/flac");
        assert_eq!(song["path"], "01.flac");
        assert_eq!(song["type"], "music");
        assert_eq!(song["mediaType"], "song");
        assert_eq!(song["musicBrainzId"], "");
        assert_eq!(song["coverArt"], album_id);
        assert!(song.get("size").is_none(), "the file does not exist");

        let one = call_json(&state, "getSong", &[("id", song["id"].as_str().unwrap())]).await;
        assert_eq!(one["song"]["title"], "One");

        let by_artist = call_json(&state, "getMusicDirectory", &[("id", &artist_id)]).await;
        assert_eq!(by_artist["directory"]["name"], "Artist");
        assert_eq!(by_artist["directory"]["child"][0]["id"], album_id);
        let by_album = call_json(&state, "getMusicDirectory", &[("id", &album_id)]).await;
        assert_eq!(by_album["directory"]["name"], "Blue");
        assert_eq!(by_album["directory"]["parent"], artist_id);
        assert_eq!(by_album["directory"]["child"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn songs_of_files_on_disk_carry_their_size_and_bit_rate() {
        let dir = ScratchDir::new("subsonic-sizes");
        let path = dir.path().join("a").join("01.mp3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 4.5 MB over the fixture's 180 s is 200 kbps.
        std::fs::write(&path, vec![0u8; 4_500_000]).unwrap();
        let state = state().await;
        let root = dir.path().to_str().unwrap();
        let track = scanned(
            path.to_str().unwrap(),
            "Blue",
            Some("Artist"),
            "One",
            1,
            2001,
        );
        store_tracks(&state.pool, root, &[track]).await;
        let id = store::library_tracks(&state.pool, None, 1, 0)
            .await
            .unwrap()[0]
            .id;

        let reply = call_json(&state, "getSong", &[("id", &Id::song(id))]).await;
        assert_eq!(reply["song"]["size"], 4_500_000);
        assert_eq!(reply["song"]["bitRate"], 200);
        assert_eq!(reply["song"]["contentType"], "audio/mpeg");
        assert_eq!(reply["song"]["path"], "a/01.mp3");

        // Only the calls that return one album or one song stat the files.
        let album_id = Id::album(
            store::library_tracks(&state.pool, None, 1, 0)
                .await
                .unwrap()[0]
                .release_id,
        );
        let album = call_json(&state, "getAlbum", &[("id", &album_id)]).await;
        assert_eq!(album["album"]["song"][0]["size"], 4_500_000);
        let directory = call_json(&state, "getMusicDirectory", &[("id", &album_id)]).await;
        assert_eq!(directory["directory"]["child"][0]["size"], 4_500_000);

        let search = call_json(&state, "search3", &[("query", "One")]).await;
        let songs = search["searchResult3"]["song"].as_array().unwrap();
        assert_eq!(songs.len(), 1);
        assert!(songs[0].get("size").is_none(), "search does not stat files");
        assert!(songs[0].get("bitRate").is_none());
        let random = call_json(&state, "getRandomSongs", &[]).await;
        let songs = random["randomSongs"]["song"].as_array().unwrap();
        assert_eq!(songs.len(), 1);
        assert!(
            songs[0].get("size").is_none(),
            "random songs do not stat files"
        );
        let list = call_json(&state, "getAlbumList2", &[("type", "random")]).await;
        let albums = list["albumList2"]["album"].as_array().unwrap();
        assert_eq!(albums.len(), 1);
        assert!(albums[0].get("song").is_none() && albums[0].get("size").is_none());
    }

    #[tokio::test]
    async fn an_artist_has_the_same_name_in_the_list_and_on_its_pages() {
        let state = state().await;
        // The earlier year puts the release with the larger key first.
        store_tracks(
            &state.pool,
            "/music",
            &[scanned(
                "/music/a/01.mp3",
                "Early",
                Some("$hVN"),
                "x",
                1,
                1990,
            )],
        )
        .await;
        store_tracks(
            &state.pool,
            "/music",
            &[scanned(
                "/music/b/01.mp3",
                "Late",
                Some("$HVN"),
                "x",
                1,
                2000,
            )],
        )
        .await;

        let list = call_json(&state, "getArtists", &[]).await;
        let entry = &list["artists"]["index"][0]["artist"];
        assert_eq!(entry.as_array().unwrap().len(), 1);
        assert_eq!(entry[0]["name"], "$HVN");
        assert_eq!(entry[0]["albumCount"], 2);
        let id = entry[0]["id"].as_str().unwrap();

        let artist = call_json(&state, "getArtist", &[("id", id)]).await;
        assert_eq!(artist["artist"]["name"], "$HVN");
        assert_eq!(artist["artist"]["albumCount"], 2);
        assert_eq!(artist["artist"]["album"][0]["name"], "Early");
        let directory = call_json(&state, "getMusicDirectory", &[("id", id)]).await;
        assert_eq!(directory["directory"]["name"], "$HVN");
    }

    #[tokio::test]
    async fn xml_responses_carry_albums_with_song_children_and_attributes() {
        let state = state().await;
        let release = seed_release(&state.pool, "/music/a", "Blue", &["One", "Two"]).await;
        let text = call_xml(&state, "getAlbum", &[("id", &Id::album(release.id))]).await;

        assert!(
            text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<subsonic-response ")
        );
        assert!(text.contains(" xmlns=\"http://subsonic.org/restapi\""));
        assert!(text.contains(" status=\"ok\""));
        assert!(text.ends_with("</album></subsonic-response>"), "{text}");
        let album = &text[text.find("<album ").expect("an album element")..];
        let (open, rest) = album.split_once('>').unwrap();
        assert!(!open.ends_with('/'), "the album has children: {open}");
        assert!(open.contains(" name=\"Blue\""));
        assert!(open.contains(" isDir=\"true\""));
        let songs: Vec<&str> = rest
            .split("<song ")
            .skip(1)
            .map(|song| song.split_once('>').unwrap().0)
            .collect();
        assert_eq!(songs.len(), 2, "{rest}");
        for song in &songs {
            assert!(song.ends_with('/'), "a song is an empty element: {song}");
            assert!(song.contains(" isDir=\"false\""), "{song}");
        }
        assert!(songs[0].contains(" title=\"One\"") && songs[0].contains(" track=\"1\""));
        assert!(songs[1].contains(" title=\"Two\"") && songs[1].contains(" track=\"2\""));
    }

    #[tokio::test]
    async fn unknown_or_malformed_ids_are_not_found() {
        let state = state().await;
        seed_release(&state.pool, "/music/a", "Blue", &["One"]).await;
        let code = |reply: Value| reply["error"]["code"].clone();
        assert_eq!(
            code(call_json(&state, "getAlbum", &[("id", "al-999")]).await),
            70
        );
        assert_eq!(
            code(call_json(&state, "getAlbum", &[("id", &Id::song(1))]).await),
            70
        );
        assert_eq!(
            code(call_json(&state, "getArtist", &[("id", &Id::artist("Nobody"))]).await),
            70
        );
        assert_eq!(
            code(call_json(&state, "getSong", &[("id", "tr-x")]).await),
            70
        );
        assert_eq!(
            code(call_json(&state, "getMusicDirectory", &[("id", &Id::song(1))]).await),
            70
        );
        assert_eq!(code(call_json(&state, "getArtist", &[]).await), 10);
    }

    fn names(reply: &Value, key: &str) -> Vec<String> {
        reply[key]["album"]
            .as_array()
            .unwrap()
            .iter()
            .map(|album| album["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn album_lists_cover_every_type() {
        let state = state().await;
        // Stored in this order, so the IDs ascend Alpha, Zeta, Beta. The orders
        // all differ: by title, by artist (Aaron, Abba, Zed), by year and by ID.
        for (dir, album, artist, year) in [
            ("a", "Alpha", "Abba", 2005),
            ("b", "Zeta", "Aaron", 1990),
            ("c", "Beta", "Zed", 1999),
        ] {
            let path = format!("/music/{dir}/01.mp3");
            let track = scanned(&path, album, Some(artist), "x", 1, year);
            store_tracks(&state.pool, "/music", &[track]).await;
        }
        let by_id = store::library_releases(&state.pool, ReleaseOrder::Id, 10, 0)
            .await
            .unwrap();
        let by_id: Vec<&str> = by_id
            .iter()
            .map(|release| release.name.as_deref().unwrap())
            .collect();
        assert_eq!(by_id, ["Alpha", "Zeta", "Beta"]);
        let list = |pairs: &'static [(&'static str, &'static str)]| {
            let state = &state;
            async move {
                names(
                    &call_json(state, "getAlbumList2", pairs).await,
                    "albumList2",
                )
            }
        };

        assert_eq!(
            list(&[("type", "alphabeticalByName")]).await,
            ["Alpha", "Beta", "Zeta"]
        );
        assert_eq!(
            list(&[("type", "alphabeticalByArtist")]).await,
            ["Zeta", "Alpha", "Beta"]
        );
        assert_eq!(
            list(&[("type", "byYear"), ("fromYear", "1980"), ("toYear", "2020")]).await,
            ["Zeta", "Beta", "Alpha"]
        );
        assert_eq!(
            list(&[("type", "byYear"), ("fromYear", "2020"), ("toYear", "1980")]).await,
            ["Alpha", "Beta", "Zeta"]
        );
        assert_eq!(
            list(&[("type", "byYear"), ("fromYear", "1995"), ("toYear", "2000")]).await,
            ["Beta"]
        );
        assert_eq!(
            list(&[("type", "alphabeticalByName"), ("offset", "1")]).await,
            ["Beta", "Zeta"]
        );
        assert_eq!(
            list(&[("type", "newest")]).await,
            ["Beta", "Zeta", "Alpha"],
            "the most recently stored first"
        );
        assert_eq!(list(&[("type", "newest"), ("size", "1")]).await, ["Beta"]);
        assert_eq!(list(&[("type", "random")]).await.len(), 3);
        for empty in ["starred", "frequent", "recent", "highest", "byGenre"] {
            let reply = call_json(&state, "getAlbumList2", &[("type", empty)]).await;
            assert!(names(&reply, "albumList2").is_empty(), "{empty}");
        }
        let old = call_json(&state, "getAlbumList", &[("type", "alphabeticalByName")]).await;
        assert_eq!(names(&old, "albumList"), ["Alpha", "Beta", "Zeta"]);

        let code = |reply: Value| reply["error"]["code"].clone();
        let partial = [("type", "byYear"), ("fromYear", "1990")];
        assert_eq!(code(call_json(&state, "getAlbumList2", &partial).await), 10);
        assert_eq!(code(call_json(&state, "getAlbumList2", &[]).await), 10);
        assert_eq!(
            code(call_json(&state, "getAlbumList2", &[("type", "sideways")]).await),
            0
        );
    }

    #[tokio::test]
    async fn search_finds_artists_albums_and_songs_or_everything() {
        let state = state().await;
        seed_release(&state.pool, "/music/a", "Blue", &["One", "Two"]).await;
        seed_release(&state.pool, "/music/b", "Red", &["Three"]).await;
        let counts = |reply: &Value, key: &str| {
            let result = &reply[key];
            ["artist", "album", "song"].map(|kind| result[kind].as_array().unwrap().len())
        };

        let blue = call_json(&state, "search3", &[("query", "blue")]).await;
        assert_eq!(counts(&blue, "searchResult3"), [0, 1, 0]);
        let artist = call_json(&state, "search3", &[("query", "\"artist\"")]).await;
        assert_eq!(counts(&artist, "searchResult3"), [1, 2, 3]);
        for everything in ["", "\"\"", "*"] {
            let all = call_json(&state, "search3", &[("query", everything)]).await;
            assert_eq!(counts(&all, "searchResult3"), [1, 2, 3], "{everything:?}");
        }
        let all = call_json(&state, "search3", &[]).await;
        assert_eq!(counts(&all, "searchResult3"), [1, 2, 3]);
        let paged = call_json(
            &state,
            "search3",
            &[
                ("query", ""),
                ("artistCount", "0"),
                ("albumCount", "1"),
                ("albumOffset", "1"),
                ("songCount", "2"),
                ("songOffset", "2"),
            ],
        )
        .await;
        assert_eq!(counts(&paged, "searchResult3"), [0, 1, 1]);
        assert!(paged["searchResult3"]["song"][0].get("size").is_none());
        let old = call_json(&state, "search2", &[("query", "three")]).await;
        assert_eq!(old["searchResult2"]["song"][0]["title"], "Three");
    }

    #[tokio::test]
    async fn random_songs_honour_size_and_years() {
        let state = state().await;
        seed_release(&state.pool, "/music/a", "Blue", &["One", "Two", "Three"]).await;
        let songs = |reply: Value| reply["randomSongs"]["song"].as_array().unwrap().len();
        assert_eq!(songs(call_json(&state, "getRandomSongs", &[]).await), 3);
        assert_eq!(
            songs(call_json(&state, "getRandomSongs", &[("size", "2")]).await),
            2
        );
        assert_eq!(
            songs(call_json(&state, "getRandomSongs", &[("fromYear", "2010")]).await),
            0
        );
    }
}
