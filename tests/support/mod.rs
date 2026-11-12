//! Shared offline test harness: a loopback HTTP server plus synthetic fixtures.
//!
//! Every test points both provider base URLs here. No public service is ever
//! contacted, and no real credential is required.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct MockServer {
    pub url: String,
    shutdown: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
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
        let flag = shutdown.clone();
        let recorded = requests.clone();

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
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }

                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                recorded.lock().unwrap().push(target.clone());

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
        }
    }

    pub fn targets(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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
