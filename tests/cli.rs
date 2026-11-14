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
    seed_tracks(
        db,
        directory,
        album,
        year,
        &["Track 1", "Track 2"],
        &[181.0, 182.0],
    )
    .await
}

async fn seed_tracks(
    db: &Path,
    directory: &str,
    album: &str,
    year: i64,
    titles: &[&str],
    durations: &[f64],
) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new(directory);
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();

    for (index, (title, duration)) in titles.iter().zip(durations).enumerate() {
        let number = index as i64 + 1;
        let track = ScannedTrack {
            path: directory.join(format!("0{number}.flac")),
            title: Some((*title).to_string()),
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
            duration_secs: Some(*duration),
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
        // Radish reads `.env` from the working directory; a developer's real
        // credentials must never reach a test, so run where none exists.
        .current_dir(db.parent().unwrap())
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

const SETTLE_TITLES: [&str; 6] = [
    "Opening",
    "Second Wind",
    "Hollow",
    "Undertow",
    "Lantern",
    "Closing",
];
const SETTLE_DURATIONS: [f64; 6] = [181.0, 212.0, 243.0, 274.0, 305.0, 336.0];

/// Six files, Discogs release 42 titled like them, and MusicBrainz releases
/// of different release groups that all fit the files, so MusicBrainz settles
/// none of them on its own. One lists a placeholder subtitle on track 3 that the
/// files and Discogs do not have, so that release and Discogs disagree on the
/// title; the files side with Discogs. With `live_primary`, another lists track
/// 3 as a live recording, which the files do not rule out.
async fn seed_settling_scenario(db: &Path, live_primary: bool) -> MockServer {
    let titles = SETTLE_TITLES;
    let durations = SETTLE_DURATIONS;
    seed_tracks(db, "/music/blue", "Blue", 2001, &titles, &durations).await;
    let exact = "11111111-1111-1111-1111-111111111111";
    let placeholder_id = "22222222-2222-2222-2222-222222222222";
    let live_id = "33333333-3333-3333-3333-333333333333";
    let mut placeholder = titles;
    placeholder[2] = "Hollow / [unknown]";
    let mut live = titles;
    live[2] = "Hollow (Live)";
    let tracklist: Vec<_> = titles
        .iter()
        .zip(durations)
        .enumerate()
        .map(|(index, (title, duration))| {
            serde_json::json!({
                "position": (index + 1).to_string(),
                "title": title,
                "duration": format!("{}:{:02}", duration as u64 / 60, duration as u64 % 60),
                "type_": "track"
            })
        })
        .collect();
    let discogs_release = serde_json::json!({
        "id": 42, "title": "Blue", "year": 2001, "country": "US",
        "artists": [{"id": 1, "name": "Test Artist"}],
        "labels": [{"id": 2, "name": "Label", "catno": "CAT-1"}],
        "formats": [{"name": "CD", "descriptions": ["Album"]}],
        "identifiers": [],
        "tracklist": tracklist
    })
    .to_string();
    let mut ids = vec![exact, placeholder_id];
    if live_primary {
        ids.push(live_id);
    }
    MockServer::start(move |target| {
        let other_group = |id: &str, tracks: &[&str], group: &str| {
            musicbrainz_release_json(id, tracks, &durations)
                .replace("4642ee19-7790-3c8d-ab5e-d133de942db6", group)
        };
        if target.contains(&format!("/ws/2/release/{exact}")) {
            return (200, musicbrainz_release_json(exact, &titles, &durations));
        }
        if target.contains(&format!("/ws/2/release/{placeholder_id}")) {
            let group = "5753ff2a-8801-4d9e-bc6f-e244ef053ec7";
            return (200, other_group(placeholder_id, &placeholder, group));
        }
        if target.contains(&format!("/ws/2/release/{live_id}")) {
            let group = "6864ab3b-9912-4eaf-8d70-f355f0164fd8";
            return (200, other_group(live_id, &live, group));
        }
        if target.contains("/ws/2/release") {
            let releases: Vec<_> = ids
                .iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id, "title": "Blue", "date": "2001-05-01",
                        "artist-credit": [{"name": "Test Artist", "joinphrase": ""}]
                    })
                })
                .collect();
            return (
                200,
                serde_json::json!({
                    "count": releases.len(), "offset": 0, "releases": releases
                })
                .to_string(),
            );
        }
        if target.contains("/releases/42") {
            return (200, discogs_release.clone());
        }
        handler(target)
    })
}

const SETTLED_TITLE_LINE: &str = "fallback conflict settled by the files: primary and fallback track titles differ for local track 3 ('Hollow / [unknown]' versus 'Hollow'); the file 'Hollow' agrees with the fallback";

#[tokio::test]
async fn the_files_settle_a_placeholder_title_conflict_between_the_providers() {
    let db = db_path("dry-run-fallback-settled");
    let server = seed_settling_scenario(&db, false).await;

    let output = run(&db, &server, &["match", "--dry-run"], Some("test-token"));
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}\n{text}");
    assert!(text.contains("would accept discogs"), "{text}");
    assert!(text.contains(SETTLED_TITLE_LINE), "{text}");
    assert!(!text.contains("  fallback conflict: "), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    assert_eq!(row_count(&db, "match_reviews").await, 0);
}

const LIVE_CONFLICT: &str =
    "track titles differ for local track 3 ('Hollow (Live)' versus 'Hollow')";

#[tokio::test]
async fn a_queued_release_prints_the_conflicts_the_files_settled_and_stores_none_of_them() {
    let db = db_path("fallback-settled-review");
    let server = seed_settling_scenario(&db, true).await;

    // The live primary's conflict is not settled, so the release is queued.
    // What the files settled is printed as its own line, but it is no reason
    // for the review and is not in the summary of why it was queued.
    let output = run(&db, &server, &["match"], Some("test-token"));
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}\n{text}");
    assert!(!text.contains("would accept"), "{text}");
    assert!(
        text.lines()
            .any(|line| line == format!("  {SETTLED_TITLE_LINE}")),
        "{text}"
    );
    assert!(text.contains(LIVE_CONFLICT), "{text}");
    let queued = text
        .lines()
        .find(|line| line.starts_with("  queued for review: "))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(!queued.contains("settled by the files"), "{queued}");
    assert_eq!(row_count(&db, "release_matches").await, 0);
    let pool = radish::db::connect(&db).await.unwrap();
    let reasons: Vec<(String,)> =
        sqlx::query_as("SELECT reasons_json FROM match_reviews WHERE provider = 'discogs'")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    let reasons = reasons.into_iter().map(|row| row.0).collect::<String>();
    assert!(!reasons.contains("settled by the files"), "{reasons}");
    assert!(reasons.contains(LIVE_CONFLICT), "{reasons}");
}

#[tokio::test]
async fn a_dry_run_of_a_queued_release_prints_the_conflicts_the_files_settled() {
    let db = db_path("dry-run-fallback-settled-review");
    let server = seed_settling_scenario(&db, true).await;

    let output = run(&db, &server, &["match", "--dry-run"], Some("test-token"));
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}\n{text}");
    assert!(text.contains("would queue for review"), "{text}");
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert!(
        text.lines()
            .any(|line| line == format!("  {SETTLED_TITLE_LINE}")),
        "{text}"
    );
    assert!(text.contains(LIVE_CONFLICT), "{text}");
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
        text.contains(
            "Imported 1 new or changed track into 1 local release: 0 unchanged, 0 removed."
        ),
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
    assert!(stdout(&output).contains(
        "Imported 0 new or changed tracks into 0 local releases: 0 unchanged, 0 removed."
    ));
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
    assert!(
        text.contains("Imported 1 new or changed track into"),
        "{text}"
    );

    let list = run(&db, &server, &["list"], None);
    assert!(list.status.success());
    assert!(stdout(&list).contains("1 track."), "{}", stdout(&list));
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
    assert!(
        text.contains("Imported 1 new or changed track into"),
        "{text}"
    );
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
    assert!(text.contains("1 local release."), "{text}");
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
    assert!(
        text.contains("MusicBrainz stage failed transiently; fallback candidates need review"),
        "{text}"
    );
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

#[tokio::test]
async fn an_explicit_discogs_run_fails_every_release_after_a_rejected_token() {
    let db = db_path("discogs-explicit-token-rejected");
    seed_directory(&db, "/music/blue", 2001).await;
    seed_directory(&db, "/music/blue-again", 2001).await;
    let server = MockServer::start(|target| {
        if target.contains("/database/search") || target.contains("/releases/") {
            return (401, r#"{"message":"Invalid consumer token."}"#.into());
        }
        handler(target)
    });

    let output = run(
        &db,
        &server,
        &["match", "--provider", "discogs"],
        Some("rejected-token"),
    );

    assert!(!output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Failed: 2"), "{text}");
    assert!(text.contains("Skipped: 0"), "{text}");
    assert_eq!(server.requests_matching("/database/search"), 1, "{text}");
}

#[tokio::test]
async fn a_stale_refresh_after_a_rejected_token_is_skipped_not_called_unmatched() {
    let db = db_path("stale-refresh-token-rejected");
    seed_directory(&db, "/music/blue", 2001).await;
    seed_directory(&db, "/music/blue-again", 2001).await;
    let rejected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = rejected.clone();
    let server = MockServer::start(move |target| {
        if flag.load(std::sync::atomic::Ordering::SeqCst)
            && (target.contains("/database/search") || target.contains("/releases/"))
        {
            return (401, r#"{"message":"Invalid consumer token."}"#.into());
        }
        handler(target)
    });
    let accepted = run(
        &db,
        &server,
        &["match", "--provider", "discogs"],
        Some("good"),
    );
    assert!(stdout(&accepted).contains("Accepted: 2"), "{accepted:?}");
    // Both accepted matches go stale, then the token stops working.
    seed_directory(&db, "/music/blue", 2002).await;
    seed_directory(&db, "/music/blue-again", 2002).await;
    rejected.store(true, std::sync::atomic::Ordering::SeqCst);
    let before = server.request_count();

    let output = run(&db, &server, &["match"], Some("revoked"));

    assert!(!output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Failed: 1"), "{text}");
    assert!(text.contains("Skipped: 1"), "{text}");
    assert!(text.contains("Unmatched: 0"), "{text}");
    assert_eq!(server.request_count() - before, 1, "{text}");
}

#[tokio::test]
async fn a_permanent_musicbrainz_error_still_recovers_through_the_fallback() {
    let db = db_path("permanent-primary-recovers");
    seed_library(&db).await;
    let server = MockServer::start(|target| {
        if target.contains("/ws/2/") {
            return (400, r#"{"error":"bad request"}"#.into());
        }
        handler(target)
    });
    let output = run(&db, &server, &["match"], Some("test-token"));
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("recovered the release"), "{text}");
    assert!(text.contains("Accepted: 1"), "{text}");
    assert_eq!(row_count(&db, "release_matches").await, 1);
}

#[tokio::test]
async fn non_loopback_service_overrides_are_rejected_without_any_request() {
    for (provider, variable) in [
        ("musicbrainz", "RADISH_MUSICBRAINZ_BASE_URL"),
        ("discogs", "RADISH_DISCOGS_BASE_URL"),
    ] {
        let db = db_path(&format!("evil-override-{provider}"));
        seed_library(&db).await;
        let server = MockServer::start(handler);
        // If the override were honored, traffic would flow through this local
        // proxy and be recorded; nothing may reach the network either way.
        let output = Command::new(env!("CARGO_BIN_EXE_radish"))
            .arg("--db")
            .arg(&db)
            .args(["match", "--provider", provider])
            .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
            .env("RADISH_DISCOGS_BASE_URL", &server.url)
            .env(variable, "https://evil.example/secret-path")
            .env("RADISH_DISCOGS_TOKEN", "test-token")
            .env("HTTPS_PROXY", &server.url)
            .env("https_proxy", &server.url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .stdin(Stdio::null())
            .output()
            .expect("run radish");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(all.contains(variable), "{provider}: {all}");
        assert!(!all.contains("evil.example"), "{provider}: {all}");
        assert!(!all.contains("secret-path"), "{provider}: {all}");
        assert_eq!(server.request_count(), 0, "{provider}: {all}");
    }
}

#[tokio::test]
async fn a_rejected_musicbrainz_override_fails_the_run_once_without_a_discogs_fallback() {
    let db = db_path("evil-override-fails-once");
    seed_directory(&db, "/music/blue", 2001).await;
    seed_album(&db, "/music/green", "Green", 2002).await;
    let server = MockServer::start(handler);
    let variable = "RADISH_MUSICBRAINZ_BASE_URL";

    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("match")
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .env(variable, "https://evil.example/secret-path")
        .env("RADISH_DISCOGS_TOKEN", "test-token")
        .stdin(Stdio::null())
        .output()
        .expect("run radish");

    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{all}");
    assert_eq!(all.matches(variable).count(), 1, "{all}");
    assert!(!all.contains("evil.example"), "{all}");
    assert!(!all.contains("secret-path"), "{all}");
    assert!(!all.contains("recovered the release"), "{all}");
    assert_eq!(server.request_count(), 0, "{all}");
}

#[tokio::test]
async fn partial_musicbrainz_lookup_failures_queue_a_discogs_fallback() {
    let db = db_path("partial-outage-fallback-queued");
    seed_library(&db).await;
    // The weak release sorts first, so it is fetched before the failing one
    // (a failed fetch starts a service-wide cooldown).
    let readable = "11111111-1111-1111-1111-111111111111";
    let failing = "ffffffff-ffff-ffff-ffff-ffffffffffff";
    let server = MockServer::start(move |target| {
        if target.contains(&format!("/ws/2/release/{failing}")) {
            return (503, "{}".into());
        }
        if target.contains(&format!("/ws/2/release/{readable}")) {
            // A real but different album: it cannot be accepted.
            return (
                200,
                musicbrainz_release_json(readable, &["Other 1", "Other 2"], &[300.0, 301.0]),
            );
        }
        if target.contains("/ws/2/release") {
            let releases: Vec<_> = [readable, failing]
                .into_iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id, "title": "Blue", "date": "2001-05-01",
                        "artist-credit": [{"name": "Test Artist", "joinphrase": ""}]
                    })
                })
                .collect();
            return (
                200,
                serde_json::json!({"count": 2, "offset": 0, "releases": releases}).to_string(),
            );
        }
        handler(target)
    });

    let output = run(&db, &server, &["match"], Some("test-token"));

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(
        server.requests_matching(&format!("/ws/2/release/{failing}")) > 0,
        "the failing lookup must have been attempted: {:?}",
        server.targets()
    );
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert!(text.contains("Accepted: 0"), "{text}");
    assert!(!text.contains("recovered the release"), "{text}");
    assert!(
        text.contains("MusicBrainz stage failed transiently; fallback candidates need review"),
        "{text}"
    );
    assert_eq!(row_count(&db, "release_matches").await, 0);
    // Each provider's best candidate is queued for the reviewer.
    let pool = radish::db::connect(&db).await.unwrap();
    let providers: Vec<String> =
        sqlx::query_scalar("SELECT provider FROM match_reviews ORDER BY provider")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(providers, ["discogs", "musicbrainz"]);
}
