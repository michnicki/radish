//! End-to-end CLI behavior against a mock Discogs server.
//!
//! These tests never touch the network: the adapter's test-server override
//! points at a local listener, and the token is a placeholder.

use radish::scanner::ScannedTrack;
use radish::store;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

struct MockServer {
    url: String,
    shutdown: Arc<AtomicBool>,
}

impl MockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock discogs");
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();

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

                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                let body = if path.contains("/database/search") {
                    search_json()
                } else if path.contains("/releases/42") {
                    release_json()
                } else {
                    "{}".to_string()
                };

                let payload = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(payload.as_bytes());
            }
        });

        Self {
            url: format!("http://{address}"),
            shutdown,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

fn search_json() -> String {
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

fn release_json() -> String {
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

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("radish-cli-{name}-{}", std::process::id()))
}

fn db_path(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("radish.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("radish.db-wal"));
    let _ = std::fs::remove_file(dir.join("radish.db-shm"));
    path
}

/// Seeds two tagged tracks so the library has one local release with two files.
async fn seed_library(db: &Path) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new("/music/blue");
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();

    for number in 1..=2i64 {
        let track = ScannedTrack {
            path: directory.join(format!("0{number}.flac")),
            title: Some(format!("Track {number}")),
            artist: Some("Test Artist".to_string()),
            album: Some("Blue".to_string()),
            album_artist: Some("Test Artist".to_string()),
            year: Some(2001),
            track_number: Some(number),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            duration_secs: Some(180.0 + number as f64),
            modified_at: 1,
        };
        store::upsert_track(&pool, &track, directory_id)
            .await
            .unwrap();
    }

    store::rebuild_local_releases(&pool, None).await.unwrap();
    let release_id = store::list_local_releases(&pool, None).await.unwrap()[0].id;
    pool.close().await;
    release_id
}

fn run(db: &Path, server: &MockServer, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .args(args)
        .env("RADISH_DISCOGS_TOKEN", "test-token")
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .stdin(Stdio::null())
        .output()
        .expect("run radish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

async fn match_row_count(db: &Path, table: &str) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let count: i64 = match table {
        "discogs_matches" => sqlx::query_scalar("SELECT COUNT(*) FROM discogs_matches")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "match_reviews" => sqlx::query_scalar("SELECT COUNT(*) FROM match_reviews")
            .fetch_one(&pool)
            .await
            .unwrap(),
        other => panic!("unexpected table {other}"),
    };
    pool.close().await;
    count
}

#[tokio::test]
async fn noninteractive_match_queues_candidates_instead_of_accepting() {
    let db = db_path("queue");
    seed_library(&db).await;
    let server = MockServer::start();

    let output = run(&db, &server, &["match"]);
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Queued for review"), "{text}");
    assert_eq!(match_row_count(&db, "discogs_matches").await, 0);
    assert_eq!(match_row_count(&db, "match_reviews").await, 1);
}

#[tokio::test]
async fn dry_run_reports_without_persisting() {
    let db = db_path("dry-run");
    seed_library(&db).await;
    let server = MockServer::start();

    let output = run(&db, &server, &["match", "--dry-run"]);
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Dry run"), "{text}");
    assert!(text.contains("would"), "{text}");
    assert_eq!(match_row_count(&db, "discogs_matches").await, 0);
    assert_eq!(match_row_count(&db, "match_reviews").await, 0);
}

#[tokio::test]
async fn direct_release_selection_still_requires_review() {
    let db = db_path("direct");
    let release_id = seed_library(&db).await;
    let server = MockServer::start();

    let output = run(
        &db,
        &server,
        &[
            "match",
            "--local-release",
            &release_id.to_string(),
            "--discogs-release",
            "42",
        ],
    );
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Queued for review"), "{text}");
    assert_eq!(match_row_count(&db, "discogs_matches").await, 0);
}

#[tokio::test]
async fn import_is_kept_when_matching_fails() {
    let db = db_path("import");
    let music = temp_dir("import").join("music");
    std::fs::create_dir_all(&music).unwrap();

    // A minimal PCM WAV file; no tags, but a valid audio file for the scanner.
    let bytes: Vec<u8> = {
        let data = [0u8; 32];
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
        bytes
    };
    std::fs::write(music.join("01.wav"), &bytes).unwrap();

    // No token: matching fails before any network call, after the import.
    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("import")
        .arg(&music)
        .arg("--match")
        .env_remove("RADISH_DISCOGS_TOKEN")
        .stdin(Stdio::null())
        .output()
        .expect("run radish");

    assert!(!output.status.success(), "matching failure must be nonzero");
    let text = stdout(&output);
    assert!(text.contains("Imported 1 tracks"), "{text}");

    let list = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("list")
        .stdin(Stdio::null())
        .output()
        .expect("run radish list");
    assert!(list.status.success());
    assert!(stdout(&list).contains("1 track(s)"), "{}", stdout(&list));
}
