//! End-to-end MusicBrainz workflows against a loopback server.
//!
//! No public service is contacted and no Discogs token is required.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::store;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::*;

const MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";

fn handler(target: &str) -> (u16, String) {
    if target.contains(&format!("/ws/2/release/{MBID}")) {
        (
            200,
            musicbrainz_release_json(MBID, &["Track 1!", "Track 2!"], &[180.0, 181.0]),
        )
    } else if target.contains("/ws/2/release") {
        (200, musicbrainz_empty_search())
    } else {
        // Any Discogs request is a failure: this workflow must not need one.
        (500, "unexpected Discogs request".to_string())
    }
}

async fn seed(db: &Path, year: i64) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new("/music/blue");
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();

    for number in 1..=2i64 {
        let musicbrainz = ObservedMusicBrainzIds {
            release: vec![MBID.to_string()],
            ..Default::default()
        };
        let track = ScannedTrack {
            path: directory.join(format!("0{number}.flac")),
            title: Some(format!("Track {number}")),
            artist: Some("Test Artist".to_string()),
            album: Some("Blue".to_string()),
            album_artist: Some("Test Artist".to_string()),
            year: Some(year),
            track_number: Some(number),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz,
            duration_secs: Some(179.0 + number as f64),
            modified_at: 1,
        };
        store::upsert_track(&pool, &track, directory_id)
            .await
            .unwrap();
    }

    store::rebuild_local_releases(&pool, None).await.unwrap();
    let id = store::list_local_releases(&pool, None).await.unwrap()[0].id;
    pool.close().await;
    id
}

fn run(db: &Path, server: &MockServer, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .args(args)
        .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .env_remove("RADISH_DISCOGS_TOKEN")
        .stdin(Stdio::null())
        .output()
        .expect("run radish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn accepted_musicbrainz_match_survives_rescan_and_can_be_cleared() {
    let db = db_path("musicbrainz-workflow");
    let release_id = seed(&db, 2001).await;
    let server = MockServer::start(handler);

    let matched = run(&db, &server, &["match"]);
    assert!(matched.status.success(), "{:?}", matched);
    let text = stdout(&matched);
    assert!(text.contains("Accepted: 1"), "{text}");
    assert!(text.contains("musicbrainz"), "{text}");
    assert_eq!(server.requests_matching("/database/search"), 0);
    assert_eq!(server.requests_matching("/ws/2/release/"), 1);

    // The accepted overlay is visible in ordinary list output.
    let listed = run(&db, &server, &["list"]);
    assert!(listed.status.success());
    let text = stdout(&listed);
    assert!(text.contains("Track 1!"), "{text}");

    // An observed tag change makes the match stale but keeps its overlay.
    seed(&db, 2002).await;
    let releases = run(&db, &server, &["releases"]);
    assert!(releases.status.success());
    assert!(stdout(&releases).contains("stale"), "{}", stdout(&releases));
    let listed = run(&db, &server, &["list"]);
    let text = stdout(&listed);
    assert!(text.contains("Track 1!"), "{text}");

    // Clearing restores the observed tag values and contacts no provider.
    let cleared = run(&db, &server, &["match", "--clear", &release_id.to_string()]);
    assert!(cleared.status.success(), "{:?}", cleared);
    assert!(stdout(&cleared).contains("Cleared"));
    let listed = run(&db, &server, &["list"]);
    let text = stdout(&listed);
    assert!(text.contains("Track 1"), "{text}");
    assert!(!text.contains("Track 1!"), "{text}");
}

#[tokio::test]
async fn a_direct_musicbrainz_selection_is_review_only() {
    let db = db_path("musicbrainz-direct");
    let release_id = seed(&db, 2001).await;
    let server = MockServer::start(handler);

    let output = run(
        &db,
        &server,
        &[
            "match",
            "--local-release",
            &release_id.to_string(),
            "--musicbrainz-release",
            MBID,
        ],
    );
    assert!(output.status.success(), "{:?}", output);
    let text = stdout(&output);
    assert!(text.contains("Queued for review"), "{text}");
    assert_eq!(server.requests_matching("/ws/2/release/"), 1);
}

#[tokio::test]
async fn an_invalid_musicbrainz_selector_fails_before_any_request() {
    let db = db_path("musicbrainz-invalid");
    let release_id = seed(&db, 2001).await;
    let server = MockServer::start(handler);

    let output = run(
        &db,
        &server,
        &[
            "match",
            "--local-release",
            &release_id.to_string(),
            "--musicbrainz-release",
            "not-an-mbid",
        ],
    );
    assert!(!output.status.success());
    assert_eq!(server.request_count(), 0);
}

const TWIN_MBID: &str = "11111111-1111-1111-1111-111111111111";

fn album_handler(target: &str) -> (u16, String) {
    for id in [MBID, TWIN_MBID] {
        if target.contains(&format!("/ws/2/release/{id}")) {
            return (
                200,
                musicbrainz_release_json(id, &["Track 1!", "Track 2!"], &[180.0, 181.0]),
            );
        }
    }
    if target.contains("/ws/2/release") {
        let releases: Vec<_> = [MBID, TWIN_MBID]
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
    (500, "unexpected Discogs request".to_string())
}

async fn seed_without_edition_ids(db: &Path) -> i64 {
    let id = seed(db, 1990).await;
    let pool = radish::db::connect(db).await.unwrap();
    sqlx::query("UPDATE tracks SET musicbrainz_ids_json = ?1")
        .bind(r#"{"release":[],"release_group":[],"release_track":[],"recording":[]}"#)
        .execute(&pool)
        .await
        .unwrap();
    store::rebuild_local_releases(&pool, None).await.unwrap();
    pool.close().await;
    id
}

#[tokio::test]
async fn equivalent_editions_match_automatically_without_edition_identifiers() {
    let db = db_path("album-auto");
    seed_without_edition_ids(&db).await;
    let server = MockServer::start(album_handler);

    let output = run(&db, &server, &["match"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("Accepted: 1"), "{text}");
    assert!(text.contains("edition unconfirmed"), "{text}");
    assert_eq!(server.requests_matching("/database/search"), 0);

    let pool = radish::db::connect(&db).await.unwrap();
    let row: (String, Option<i64>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT match_scope, year, country, catalog_number FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("album".to_string(), None, None, None));
    let mappings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mappings, 2);
    pool.close().await;

    let listed = run(&db, &server, &["list"]);
    assert!(stdout(&listed).contains("Track 1!"));
    let releases = run(&db, &server, &["releases"]);
    assert!(stdout(&releases).contains("edition unconfirmed"));
}

#[tokio::test]
async fn normal_match_reconsiders_a_saved_review_queue() {
    let db = db_path("album-reconsider");
    let id = seed_without_edition_ids(&db).await;
    let server = MockServer::start(album_handler);
    let queued = run(
        &db,
        &server,
        &[
            "match",
            "--local-release",
            &id.to_string(),
            "--musicbrainz-release",
            MBID,
        ],
    );
    assert!(queued.status.success(), "{queued:?}");

    let output = run(&db, &server, &["match"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("Accepted: 1"), "{text}");
    let pool = radish::db::connect(&db).await.unwrap();
    let reviews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM match_reviews")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reviews, 0);
    pool.close().await;
}

#[tokio::test]
async fn conflicting_recordings_stay_queued_for_review() {
    let db = db_path("album-recording-conflict");
    seed_without_edition_ids(&db).await;
    let server = MockServer::start(|target| {
        let (status, body) = album_handler(target);
        if target.contains(&format!("/ws/2/release/{TWIN_MBID}")) {
            return (
                status,
                body.replace(
                    "00000001-bbbb-4bbb-8bbb-000000000001",
                    "99999999-bbbb-4bbb-8bbb-000000000001",
                ),
            );
        }
        (status, body)
    });
    let output = run(&db, &server, &["match"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("Accepted: 0"), "{text}");
    assert!(text.contains("Queued for review: 1"), "{text}");
}

#[tokio::test]
async fn album_dry_run_keeps_the_database_unmatched() {
    let db = db_path("album-dry-run");
    seed_without_edition_ids(&db).await;
    let server = MockServer::start(album_handler);
    let output = run(&db, &server, &["match", "--dry-run"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("would accept"), "{text}");
    assert!(text.contains("edition unconfirmed"), "{text}");
    let pool = radish::db::connect(&db).await.unwrap();
    for query in [
        "SELECT COUNT(*) FROM release_matches",
        "SELECT COUNT(*) FROM match_reviews",
    ] {
        let count: i64 = sqlx::query_scalar(query).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 0);
    }
    pool.close().await;
}

#[tokio::test]
async fn album_match_preserves_local_disc_and_track_numbers() {
    let db = db_path("album-track-numbers");
    seed_without_edition_ids(&db).await;
    let pool = radish::db::connect(&db).await.unwrap();
    sqlx::query("UPDATE tracks SET disc_number = 2, track_number = 1 WHERE path LIKE '%02.flac'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let server = MockServer::start(album_handler);
    let output = run(&db, &server, &["match"]);
    assert!(stdout(&output).contains("Accepted: 1"), "{output:?}");
    let pool = radish::db::connect(&db).await.unwrap();
    let tracks = store::list_tracks(&pool).await.unwrap();
    assert_eq!(tracks[1].disc_number, Some(2));
    assert_eq!(tracks[1].track_number, Some(1));
    let searched = store::search_tracks(&pool, "Track 2").await.unwrap();
    assert_eq!(searched[0].track_number, Some(1));
    pool.close().await;
}

#[tokio::test]
async fn a_proven_edition_takes_precedence_over_an_album_score_advantage() {
    let db = db_path("album-exact-precedence");
    seed(&db, 1990).await;
    let server = MockServer::start(|target| {
        let (status, body) = album_handler(target);
        if target.contains(&format!("/ws/2/release/{MBID}")) {
            return (
                status,
                body.replace("2001-05-01", "1990-05-01")
                    .replace("180000", "181000"),
            );
        }
        (status, body)
    });
    let output = run(&db, &server, &["match"]);
    let text = stdout(&output);
    assert!(text.contains("Accepted: 1"), "{text}");
    let pool = radish::db::connect(&db).await.unwrap();
    let row: (String, String) =
        sqlx::query_as("SELECT remote_release_id, match_scope FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (MBID.to_string(), "edition".to_string()));
    pool.close().await;
}
