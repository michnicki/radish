//! End-to-end CLI behavior against a mock Discogs server.
//!
//! These tests never touch the network: both provider base URLs point at local
//! listeners, and Discogs tokens are placeholders supplied only where a test
//! deliberately exercises the fallback or explicit Discogs mode.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::store;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::*;

fn handler(target: &str) -> (u16, String) {
    if target.contains("/database/search") {
        (200, discogs_search_json())
    } else if target.contains("/releases/42") {
        (200, discogs_release_json())
    } else if target.contains("/ws/2/release") {
        (200, musicbrainz_empty_search())
    } else {
        (404, "{}".to_string())
    }
}

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
            musicbrainz: ObservedMusicBrainzIds::default(),
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

fn run(db: &Path, server: &MockServer, args: &[&str], token: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_radish"));
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .stdin(Stdio::null());
    match token {
        Some(token) => {
            command.env("RADISH_DISCOGS_TOKEN", token);
        }
        None => {
            command.env_remove("RADISH_DISCOGS_TOKEN");
        }
    }
    command.output().expect("run radish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

async fn row_count(db: &Path, table: &str) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let count: i64 = match table {
        "release_matches" => sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
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
    let server = MockServer::start(handler);

    let output = run(
        &db,
        &server,
        &["match", "--provider", "discogs"],
        Some("test-token"),
    );
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Queued for review"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(row_count(&db, "match_reviews").await, 1);
    // Explicit provider mode never contacts MusicBrainz.
    assert_eq!(server.requests_matching("/ws/2/"), 0);
}

#[tokio::test]
async fn dry_run_reports_without_persisting() {
    let db = db_path("dry-run");
    seed_library(&db).await;
    let server = MockServer::start(handler);

    let output = run(
        &db,
        &server,
        &["match", "--dry-run", "--provider", "discogs"],
        Some("test-token"),
    );
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Dry run"), "{text}");
    assert!(text.contains("would"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(row_count(&db, "match_reviews").await, 0);
}

#[tokio::test]
async fn direct_release_selection_still_requires_review() {
    let db = db_path("direct");
    let release_id = seed_library(&db).await;
    let server = MockServer::start(handler);

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
        Some("test-token"),
    );
    assert!(output.status.success(), "{:?}", output);

    let text = stdout(&output);
    assert!(text.contains("Queued for review"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(server.requests_matching("/ws/2/"), 0);
}

#[tokio::test]
async fn default_match_skips_fallback_without_a_token_and_never_contacts_discogs() {
    let db = db_path("default-no-token");
    seed_library(&db).await;
    let server = MockServer::start(handler);

    let output = run(&db, &server, &["match"], None);
    assert!(output.status.success(), "{:?}", output);
    let text = stdout(&output);
    assert!(text.contains("Unmatched: 1"), "{text}");
    assert!(text.contains("Discogs token is not set"), "{text}");
    assert_eq!(server.requests_matching("/ws/2/release"), 1);
    assert_eq!(server.requests_matching("/database/search"), 0);
    assert_eq!(server.requests_matching("/releases/"), 0);
}

#[tokio::test]
async fn explicit_discogs_mode_without_a_token_is_a_failure() {
    let db = db_path("explicit-no-token");
    seed_library(&db).await;
    let server = MockServer::start(handler);

    let output = run(&db, &server, &["match", "--provider", "discogs"], None);
    assert!(!output.status.success());
    assert_eq!(server.requests_matching("/database/search"), 0);
}

#[tokio::test]
async fn redirected_import_keeps_warnings_without_live_progress() {
    let db = db_path("import-progress-redirected");
    let music = temp_dir("import-progress-redirected-music");
    write_wav(&music.join("good.wav"));
    std::fs::write(music.join("broken.mp3"), b"not audio").unwrap();
    std::fs::write(music.join("cover.jpg"), b"not audio").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("import")
        .arg(&music)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("Scanning "), "{text}");
    assert!(
        text.contains("Imported 1 tracks into 1 local release(s)."),
        "{text}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("warning: failed to read tags:"),
        "{stderr}"
    );
    assert!(!stderr.contains(['\r', '\x1b']), "{stderr:?}");
    assert!(!text.contains(['\r', '\x1b']), "{text:?}");

    let pool = radish::db::connect(&db).await.unwrap();
    let tracks = store::list_tracks(&pool).await.unwrap();
    assert_eq!(tracks.len(), 1);
    pool.close().await;
}

#[test]
fn redirected_empty_import_has_no_progress_output() {
    let db = db_path("import-progress-empty");
    let music = temp_dir("import-progress-empty-music");
    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .arg("import")
        .arg(music)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(stdout(&output).contains("Imported 0 tracks into 0 local release(s)."));
}

#[tokio::test]
async fn import_is_kept_when_matching_fails() {
    let db = db_path("import");
    let music = temp_dir("import-music");
    let file = music.join("01.wav");
    write_wav(&file);
    let server = MockServer::start(handler);

    // Explicit Discogs mode needs a token: matching fails before any request,
    // after the import is durable.
    let output = run(
        &db,
        &server,
        &[
            "import",
            &music.to_string_lossy(),
            "--match",
            "--provider",
            "discogs",
        ],
        None,
    );

    assert!(!output.status.success(), "matching failure must be nonzero");
    let text = stdout(&output);
    assert!(text.contains("Imported 1 tracks"), "{text}");

    let list = run(&db, &server, &["list"], None);
    assert!(list.status.success());
    assert!(stdout(&list).contains("1 track(s)"), "{}", stdout(&list));
}

#[tokio::test]
async fn default_import_requires_no_discogs_token() {
    let db = db_path("import-no-token");
    let music = temp_dir("import-no-token-music");
    write_wav(&music.join("01.wav"));
    let server = MockServer::start(handler);

    let output = run(
        &db,
        &server,
        &["import", &music.to_string_lossy(), "--match"],
        None,
    );

    assert!(output.status.success(), "{:?}", output);
    let text = stdout(&output);
    assert!(text.contains("Imported 1 tracks"), "{text}");
    assert_eq!(server.requests_matching("/database/search"), 0);
}

#[tokio::test]
async fn releases_reports_provider_qualified_status() {
    let db = db_path("releases");
    seed_library(&db).await;
    let server = MockServer::start(handler);

    let output = run(&db, &server, &["releases"], None);
    assert!(output.status.success(), "{:?}", output);
    let text = stdout(&output);
    assert!(text.contains("local release(s)"), "{text}");
    assert!(text.contains("unmatched"), "{text}");
}
