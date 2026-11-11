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
