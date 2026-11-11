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
    seed_library_year(db, 2001).await
}

async fn seed_library_year(db: &Path, year: i64) -> i64 {
    seed_directory(db, "/music/blue", year).await
}

async fn seed_directory(db: &Path, directory: &str, year: i64) -> i64 {
    seed_album(db, directory, "Blue", year).await
}

async fn seed_album(db: &Path, directory: &str, album: &str, year: i64) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new(directory);
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();

    for number in 1..=2i64 {
        let track = ScannedTrack {
            path: directory.join(format!("0{number}.flac")),
            title: Some(format!("Track {number}")),
            artist: Some("Test Artist".to_string()),
            album: Some(album.to_string()),
            album_artist: Some("Test Artist".to_string()),
            year: Some(year),
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
async fn noninteractive_discogs_match_accepts_strong_album_evidence() {
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
    assert!(text.contains("Accepted: 1"), "{text}");
    assert!(text.contains("edition unconfirmed"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 1);
    assert_eq!(row_count(&db, "match_reviews").await, 0);
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
async fn dry_run_reports_an_automatic_fallback_album_match() {
    let db = db_path("dry-run-fallback-album");
    seed_library(&db).await;
    let server = MockServer::start(handler);
    let output = run(&db, &server, &["match", "--dry-run"], Some("test-token"));
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("Accepted: 1"), "{text}");
    assert!(text.contains("edition unconfirmed"), "{text}");
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
    // The exact-phrase search finds nothing, so the adapter loosens it once.
    assert_eq!(server.requests_matching("/ws/2/release"), 2);
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

#[tokio::test]
async fn fallback_queue_explains_its_blockers_after_a_musicbrainz_outage() {
    let db = db_path("fallback-queue-explanation");
    seed_library(&db).await;
    let server = MockServer::start(|target| {
        if target.contains("/ws/2/") {
            return (503, "{}".into());
        }
        if target.contains("/releases/42") {
            let mut release: serde_json::Value =
                serde_json::from_str(&discogs_release_json()).unwrap();
            for track in release["tracklist"].as_array_mut().unwrap() {
                track.as_object_mut().unwrap().remove("duration");
            }
            return (200, release.to_string());
        }
        handler(target)
    });
    let output = run(&db, &server, &["match"], Some("test-token"));
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let explanation = text
        .lines()
        .find(|line| line.contains("queued for review:"))
        .unwrap();
    assert!(explanation.contains("discogs:"), "{explanation}");
    assert!(explanation.contains("duration"), "{explanation}");
    assert!(explanation.contains("HTTP 503"), "{explanation}");
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert_eq!(server.requests_matching("/ws/2/"), 3);
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(row_count(&db, "match_reviews").await, 1);
}

#[cfg(target_os = "linux")]
#[test]
fn list_fails_when_stdout_cannot_be_written() {
    let db = db_path("list-dev-full");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");

    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("list")
        .stdin(Stdio::null())
        .stdout(full)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error: failed to write output:"),
        "{stderr}"
    );
}

#[tokio::test]
async fn musicbrainz_outage_queues_a_discogs_fallback_instead_of_accepting_it() {
    let db = db_path("outage-fallback-queued");
    seed_library(&db).await;
    // Discogs offers a complete, otherwise auto-acceptable release.
    let server = MockServer::start(|target| {
        if target.contains("/ws/2/") {
            return (503, "{}".into());
        }
        handler(target)
    });
    let output = run(&db, &server, &["match"], Some("test-token"));
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert!(text.contains("Accepted: 0"), "{text}");
    assert!(!text.contains("recovered the release"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(row_count(&db, "match_reviews").await, 1);
}

#[tokio::test]
async fn refreshing_a_stale_discogs_match_without_a_token_is_skipped_not_failed() {
    let db = db_path("stale-discogs-no-token");
    seed_library(&db).await;
    let server = MockServer::start(handler);
    let accepted = run(
        &db,
        &server,
        &["match", "--provider", "discogs"],
        Some("test-token"),
    );
    assert!(stdout(&accepted).contains("Accepted: 1"), "{accepted:?}");
    // An observed tag change makes the accepted match stale.
    seed_library_year(&db, 2002).await;
    let before = server.request_count();

    let output = run(&db, &server, &["match"], None);

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Skipped: 1"), "{text}");
    assert!(text.contains("Failed: 0"), "{text}");
    assert!(
        text.contains("needs RADISH_DISCOGS_TOKEN to refresh"),
        "{text}"
    );
    assert_eq!(
        server.request_count(),
        before,
        "no provider may be contacted"
    );
}

#[tokio::test]
async fn a_rejected_discogs_token_is_not_retried_for_later_releases() {
    let db = db_path("discogs-token-rejected");
    seed_directory(&db, "/music/blue", 2001).await;
    seed_directory(&db, "/music/blue-again", 2001).await;
    let server = MockServer::start(|target| {
        if target.contains("/database/search") || target.contains("/releases/") {
            return (401, r#"{"message":"Invalid consumer token."}"#.into());
        }
        handler(target)
    });

    let output = run(&db, &server, &["match"], Some("rejected-token"));

    // The release that met the rejection fails; later ones skip the fallback.
    assert!(!output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Failed: 1"), "{text}");
    assert!(text.contains("Unmatched: 1"), "{text}");
    assert_eq!(
        server.requests_matching("/database/search"),
        1,
        "the second release must not contact Discogs: {text}"
    );
}

#[tokio::test]
async fn listings_escape_terminal_control_sequences_from_tags() {
    let db = db_path("hostile-tags");
    let pool = radish::db::connect(&db).await.unwrap();
    let directory = Path::new("/music/hostile\x1b[2J");
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();
    let track = ScannedTrack {
        path: directory.join("01.flac"),
        title: Some("\x1b]0;PWNED\x07".to_string()),
        artist: Some("Artist\x1b[31m".to_string()),
        album: Some("Album\u{202e}".to_string()),
        album_artist: Some("Artist\x1b[31m".to_string()),
        year: Some(2001),
        track_number: Some(1),
        disc_number: None,
        barcode: None,
        catalog_number: None,
        discogs_release_id: None,
        musicbrainz: ObservedMusicBrainzIds::default(),
        duration_secs: Some(180.0),
        modified_at: 1,
    };
    store::upsert_track(&pool, &track, directory_id)
        .await
        .unwrap();
    store::rebuild_local_releases(&pool, None).await.unwrap();
    pool.close().await;
    let server = MockServer::start(handler);

    for args in [&["list"][..], &["search", "Artist"], &["releases"]] {
        let output = run(&db, &server, args, None);
        assert!(output.status.success(), "{output:?}");
        let text = stdout(&output);
        assert!(
            !text.contains(['\x1b', '\x07', '\u{202e}']),
            "{args:?}: {text:?}"
        );
        assert!(text.contains("\\u{1b}"), "{args:?}: {text:?}");
    }
}

#[tokio::test]
async fn matching_output_escapes_control_sequences_from_tags_and_provider_data() {
    let db = db_path("hostile-matching");
    seed_album(&db, "/music/blue", "Blue\x1b]0;PWNED\x07", 2001).await;
    let server = MockServer::start(|target| {
        if target.contains("/releases/42") {
            let mut release: serde_json::Value =
                serde_json::from_str(&discogs_release_json()).unwrap();
            release["title"] = "Blue\u{1b}[2J".into();
            release["tracklist"][0]["title"] = "Track\u{1b}[31m 1".into();
            return (200, release.to_string());
        }
        handler(target)
    });

    for args in [
        &["match", "--dry-run", "--provider", "discogs"][..],
        &["match", "--provider", "discogs"],
    ] {
        let output = run(&db, &server, args, Some("test-token"));
        let all = format!(
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!all.contains(['\x1b', '\x07']), "{args:?}: {all:?}");
        assert!(all.contains("Matching local release"), "{all:?}");
    }
}
