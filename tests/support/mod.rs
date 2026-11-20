//! Shared offline test harness: a loopback HTTP server plus synthetic fixtures.
//!
//! Every test points the provider base URLs here. No public service is ever
//! contacted, and no real credential is required.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

pub struct MockServer {
    pub url: String,
    shutdown: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    /// The raw header block of each request, in the same order as `requests`.
    headers: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let flag = shutdown.clone();
        let recorded = requests.clone();
        let recorded_headers = headers.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut header_block = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    header_block.push_str(&line);
                }

                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                recorded.lock().unwrap().push(target.clone());
                recorded_headers.lock().unwrap().push(header_block);

                let (status, body) = handler(&target);
                let payload = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(payload.as_bytes());
            }
        });

        Self {
            url: format!("http://{address}"),
            shutdown,
            requests,
            headers,
        }
    }

    pub fn targets(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// How many requests carried a header block containing `needle`.
    pub fn requests_with_header(&self, needle: &str) -> usize {
        self.headers
            .lock()
            .unwrap()
            .iter()
            .filter(|block| block.contains(needle))
            .count()
    }

    pub fn requests_matching(&self, needle: &str) -> usize {
        self.targets()
            .iter()
            .filter(|target| target.contains(needle))
            .count()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

pub fn musicbrainz_empty_search() -> String {
    r#"{"count":0,"offset":0,"releases":[]}"#.to_string()
}

pub fn discogs_search_json() -> String {
    r#"{
        "pagination": {"page": 1, "pages": 1, "per_page": 50, "items": 1},
        "results": [{
            "id": 42,
            "type": "release",
            "title": "Test Artist - Blue",
            "year": "2001",
            "country": "US",
            "format": ["CD"],
            "label": ["Label"],
            "catno": "CAT-1",
            "barcode": []
        }]
    }"#
    .to_string()
}

pub fn discogs_release_json() -> String {
    r#"{
        "id": 42,
        "title": "Blue",
        "year": 2001,
        "country": "US",
        "artists": [{"id": 1, "name": "Test Artist"}],
        "labels": [{"id": 2, "name": "Label", "catno": "CAT-1"}],
        "formats": [{"name": "CD", "descriptions": ["Album"]}],
        "identifiers": [],
        "tracklist": [
            {"position": "1", "title": "Track 1", "duration": "3:00", "type_": "track"},
            {"position": "2", "title": "Track 2", "duration": "3:01", "type_": "track"}
        ]
    }"#
    .to_string()
}

/// The Deezer album id the canned Deezer answers use.
pub const DEEZER_ALBUM_ID: u64 = 302127;

/// `/album/{id}` for `Blue` by `Test Artist`, released 2001-05-01, with
/// `track_count` tracks, as Deezer sends it (embedded tracks left out).
pub fn deezer_album_json(id: u64, track_count: usize) -> String {
    serde_json::json!({
        "id": id,
        "title": "Blue",
        "upc": "724384960650",
        "link": format!("https://www.deezer.com/album/{id}"),
        "label": "Label",
        "nb_tracks": track_count,
        "release_date": "2001-05-01",
        "record_type": "album",
        "explicit_lyrics": false,
        "contributors": [{"id": 1, "name": "Test Artist", "role": "Main"}],
        "artist": {"id": 1, "name": "Test Artist"}
    })
    .to_string()
}

/// One `/album/{id}/tracks` page holding every track: `titles` with
/// whole-second `durations`, on disc 1.
pub fn deezer_tracks_json(titles: &[&str], durations: &[u64]) -> String {
    let data: Vec<serde_json::Value> = titles
        .iter()
        .zip(durations)
        .enumerate()
        .map(|(index, (title, duration))| {
            serde_json::json!({
                "id": 1000 + index,
                "title": title,
                "title_short": title,
                "title_version": "",
                "duration": duration,
                "track_position": index + 1,
                "disk_number": 1,
                "isrc": "",
                "artist": {"id": 1, "name": "Test Artist"}
            })
        })
        .collect();
    serde_json::json!({"data": data, "total": titles.len()}).to_string()
}

/// A `/search/album` page holding album `id`.
pub fn deezer_search_json(id: u64) -> String {
    serde_json::json!({
        "data": [{
            "id": id,
            "title": "Blue",
            "nb_tracks": 2,
            "record_type": "album",
            "artist": {"id": 1, "name": "Test Artist"}
        }],
        "total": 1
    })
    .to_string()
}

/// Deezer's answer when nothing matches: HTTP 200 with error code 800.
pub fn deezer_no_data() -> String {
    r#"{"error":{"type":"DataException","message":"no data","code":800}}"#.to_string()
}

/// Answers the Deezer requests for album [`DEEZER_ALBUM_ID`] holding
/// `titles` with `durations`: every search finds it, and its lookup and tracks
/// are served. Any other album or barcode is no data; any other path is
/// `None`, left to the caller.
pub fn deezer_route(target: &str, titles: &[&str], durations: &[u64]) -> Option<(u16, String)> {
    let id = DEEZER_ALBUM_ID;
    if target.starts_with("/search/album") {
        Some((200, deezer_search_json(id)))
    } else if target.starts_with(&format!("/album/{id}/tracks")) {
        Some((200, deezer_tracks_json(titles, durations)))
    } else if target == format!("/album/{id}") {
        Some((200, deezer_album_json(id, titles.len())))
    } else if target.starts_with("/album/") {
        Some((200, deezer_no_data()))
    } else {
        None
    }
}

/// A complete synthetic MusicBrainz release document. Track titles deliberately
/// carry punctuation so an accepted overlay is visible while its normalized
/// comparison stays exact.
pub fn musicbrainz_release_json(mbid: &str, titles: &[&str], durations: &[f64]) -> String {
    let tracks: Vec<String> = titles
        .iter()
        .zip(durations.iter())
        .enumerate()
        .map(|(index, (title, duration))| {
            let track_id = format!("{:08}-aaaa-4aaa-8aaa-{:012}", index + 1, index + 1);
            let recording_id = format!("{:08}-bbbb-4bbb-8bbb-{:012}", index + 1, index + 1);
            format!(
                r#"{{
                    "id": "{track_id}",
                    "number": "{}",
                    "title": "{title}",
                    "length": {},
                    "artist-credit": [{{"name": "Test Artist", "joinphrase": ""}}],
                    "recording": {{"id": "{recording_id}", "title": "{title}", "length": {}}}
                }}"#,
                index + 1,
                (duration * 1000.0) as u64,
                (duration * 1000.0) as u64
            )
        })
        .collect();

    format!(
        r#"{{
            "id": "{mbid}",
            "title": "Blue",
            "status": "Official",
            "disambiguation": "",
            "date": "2001-05-01",
            "country": "US",
            "barcode": "",
            "artist-credit": [{{"name": "Test Artist", "joinphrase": ""}}],
            "release-group": {{
                "id": "4642ee19-7790-3c8d-ab5e-d133de942db6",
                "title": "Blue",
                "primary-type": "Album",
                "secondary-types": [],
                "first-release-date": "2001"
            }},
            "label-info": [{{"catalog-number": "CAT-1", "label": {{"name": "Label"}}}}],
            "release-events": [{{"date": "2001-05-01", "area": {{"name": "United States"}}}}],
            "media": [{{
                "position": 1,
                "format": "CD",
                "track-count": {},
                "tracks": [{}]
            }}]
        }}"#,
        titles.len(),
        tracks.join(",")
    )
}

/// A synthetic MusicBrainz release of two CDs, one document with two media.
/// `titles[d]` and `durations[d]` describe medium `d + 1`; track IDs and
/// recording IDs are unique across the whole release.
pub fn musicbrainz_two_disc_release_json(
    mbid: &str,
    titles: [&[&str]; 2],
    durations: [&[f64]; 2],
) -> String {
    let mut ordinal = 0usize;
    let media: Vec<String> = titles
        .iter()
        .zip(durations.iter())
        .enumerate()
        .map(|(medium, (titles, durations))| {
            let tracks: Vec<String> = titles
                .iter()
                .zip(durations.iter())
                .enumerate()
                .map(|(index, (title, duration))| {
                    ordinal += 1;
                    let track_id = format!("{ordinal:08}-aaaa-4aaa-8aaa-{ordinal:012}");
                    let recording_id = format!("{ordinal:08}-bbbb-4bbb-8bbb-{ordinal:012}");
                    let length = (duration * 1000.0) as u64;
                    format!(
                        r#"{{
                            "id": "{track_id}",
                            "number": "{}",
                            "title": "{title}",
                            "length": {length},
                            "artist-credit": [{{"name": "Test Artist", "joinphrase": ""}}],
                            "recording": {{"id": "{recording_id}", "title": "{title}", "length": {length}}}
                        }}"#,
                        index + 1
                    )
                })
                .collect();
            format!(
                r#"{{"position": {}, "format": "CD", "track-count": {}, "tracks": [{}]}}"#,
                medium + 1,
                titles.len(),
                tracks.join(",")
            )
        })
        .collect();

    format!(
        r#"{{
            "id": "{mbid}",
            "title": "Blue",
            "status": "Official",
            "disambiguation": "",
            "date": "2001-05-01",
            "country": "US",
            "barcode": "",
            "artist-credit": [{{"name": "Test Artist", "joinphrase": ""}}],
            "release-group": {{
                "id": "4642ee19-7790-3c8d-ab5e-d133de942db6",
                "title": "Blue",
                "primary-type": "Album",
                "secondary-types": [],
                "first-release-date": "2001"
            }},
            "label-info": [{{"catalog-number": "CAT-1", "label": {{"name": "Label"}}}}],
            "release-events": [{{"date": "2001-05-01", "area": {{"name": "United States"}}}}],
            "media": [{}]
        }}"#,
        media.join(",")
    )
}

/// Writes a WAV file carrying the tags the grouper and matcher read.
pub fn write_tagged_wav(
    path: &Path,
    album: &str,
    album_artist: &str,
    title: &str,
    track: u16,
    year: u16,
) {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::items::Timestamp;
    use lofty::tag::{Accessor, ItemKey, Tag};

    // One second of 16-bit 44.1 kHz mono, so a real duration is measurable.
    write_wav_with_data(path, 88_200);
    let mut file = lofty::probe::Probe::open(path).unwrap().read().unwrap();
    let mut tag = Tag::new(file.primary_tag_type());
    tag.set_album(album.to_string());
    tag.set_artist(album_artist.to_string());
    tag.insert_text(ItemKey::AlbumArtist, album_artist.to_string());
    tag.set_title(title.to_string());
    tag.set_track(u32::from(track));
    tag.set_date(Timestamp {
        year,
        ..Timestamp::default()
    });
    file.insert_tag(tag);
    file.save_to_path(path, WriteOptions::default()).unwrap();
}

/// Writes a `mbdump.tar.bz2` for fixtures, laid out like the real dump.
pub fn write_mb_archive(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("mbdump.tar.bz2");
    let file = std::fs::File::create(&path).unwrap();
    let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::best());
    let mut builder = tar::Builder::new(encoder);

    for (name, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("mbdump/{name}"), contents.as_bytes())
            .unwrap();
    }

    builder.into_inner().unwrap().finish().unwrap();
    path
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radish-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn db_path(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    let path = dir.join("radish.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("radish.db-wal"));
    let _ = std::fs::remove_file(dir.join("radish.db-shm"));
    path
}

/// Minimal valid PCM WAV bytes for scanner tests.
pub fn write_wav(path: &Path) {
    write_wav_with_data(path, 32);
}

/// Like [`write_wav`], with `data_len` bytes of audio so the duration differs.
pub fn write_wav_with_data(path: &Path, data_len: usize) {
    let data = vec![0u8; data_len];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&44100u32.to_le_bytes());
    bytes.extend_from_slice(&88200u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    std::fs::write(path, bytes).unwrap();
}

/// How long to wait for a server to announce its address.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// A running `radish serve` or `radish subsonic`, killed when dropped.
pub struct Server {
    child: Child,
    pub addr: String,
}

/// One HTTP reply. `headers` is the header block in lowercase; `body` is the
/// body as lossy UTF-8, and `bytes` as sent.
pub struct Reply {
    pub status: u16,
    pub headers: String,
    pub body: String,
    pub bytes: Vec<u8>,
}

impl Server {
    /// Starts `serve` on a free loopback port.
    pub fn start(db: &Path) -> Self {
        Self::start_with(db, &[], &[])
    }

    /// Starts `serve` with extra arguments and environment.
    pub fn start_with(db: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = vec!["serve", "--bind", "127.0.0.1:0"];
        command.extend_from_slice(args);
        Self::spawn(db, &command, env)
    }

    /// Starts `radish --db DB <command_args>` from the database's directory,
    /// so a `.env` in the checkout is never read. The service credentials of
    /// the calling environment are removed and `env` is added. The command
    /// must print `Serving http://ADDR` as its first line.
    pub fn spawn(db: &Path, command_args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_radish"));
        command
            .arg("--db")
            .arg(db)
            .args(command_args)
            .current_dir(db.parent().unwrap())
            .env_remove("RADISH_DISCOGS_TOKEN")
            .env_remove("RADISH_SUBSONIC_USER")
            .env_remove("RADISH_SUBSONIC_PASSWORD")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();

        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send(line);
        });
        let Ok(line) = receiver.recv_timeout(START_TIMEOUT) else {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
            panic!("the server did not announce an address; stderr: {stderr}");
        };
        let Some(addr) = line
            .strip_prefix("Serving http://")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            let _ = child.kill();
            let mut stderr = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
            panic!("unexpected startup output {line:?}; stderr: {stderr}");
        };
        Self {
            addr: addr.to_string(),
            child,
        }
    }

    /// Sends one request with the given `Host` header (the real address when `None`).
    pub fn request(&self, method: &str, path: &str, host: Option<&str>) -> Reply {
        self.send(method, path, host, &[])
    }

    /// Sends one request with extra header lines such as `Range: bytes=0-9`.
    pub fn request_with(&self, method: &str, path: &str, headers: &[&str]) -> Reply {
        self.send(method, path, None, headers)
    }

    fn send(&self, method: &str, path: &str, host: Option<&str>, headers: &[&str]) -> Reply {
        let host = host.unwrap_or(&self.addr);
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let extra: String = headers.iter().map(|line| format!("{line}\r\n")).collect();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n"
        )
        .unwrap();
        read_reply(stream)
    }

    /// Sends a form `POST`, with extra header lines such as `Sec-Fetch-Site: same-origin`.
    pub fn post(&self, path: &str, body: &str, headers: &[&str]) -> Reply {
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let extra: String = headers.iter().map(|line| format!("{line}\r\n")).collect();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {}\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
             {extra}Connection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        )
        .unwrap();
        read_reply(stream)
    }

    pub fn get(&self, path: &str) -> Reply {
        self.request("GET", path, None)
    }

    /// Stops the server and returns everything it wrote to stderr.
    pub fn stop(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_string(&mut stderr).unwrap();
        }
        stderr
    }
}

/// Reads a whole reply. The servers send `Content-Length`, never chunks.
fn read_reply(mut stream: TcpStream) -> Reply {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let bytes = raw[split + 4..].to_vec();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        headers: head.to_ascii_lowercase(),
        body: String::from_utf8_lossy(&bytes).into_owned(),
        bytes,
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
