//! AI matching against loopback services, with no real credentials.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::{providers::ProviderKind, store};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::*;

const MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
const TOKEN: &str = "synthetic-cloudflare-token";

async fn seed(db: &Path, exact_titles: bool) -> i64 {
    seed_in(db, "/music/private-library/blue", exact_titles).await
}

async fn seed_in(db: &Path, directory: &str, exact_titles: bool) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new(directory);
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();
    for number in 1..=2 {
        let title = if exact_titles {
            format!("Track {number}")
        } else {
            format!("Completely different title {number}")
        };
        store::upsert_track(
            &pool,
            &ScannedTrack {
                path: directory.join(format!("{number}.flac")),
                title: Some(title),
                artist: Some("Test Artist".into()),
                album: Some("Blue".into()),
                album_artist: Some("Test Artist".into()),
                year: Some(2001),
                track_number: Some(number),
                disc_number: None,
                barcode: None,
                catalog_number: None,
                discogs_release_id: None,
                musicbrainz: ObservedMusicBrainzIds {
                    release: vec![MBID.into()],
                    ..Default::default()
                },
                duration_secs: Some(179.0 + number as f64),
                modified_at: 1,
            },
            directory_id,
        )
        .await
        .unwrap();
    }
    store::rebuild_local_releases(&pool, None).await.unwrap();
    let id = store::list_local_releases(&pool, None).await.unwrap()[0].id;
    pool.close().await;
    id
}

fn server(ai_status: u16, choice: &str) -> MockServer {
    server_with_probability(ai_status, choice, 1.0, 1.0)
}

fn server_with_probability(
    ai_status: u16,
    choice: &str,
    probability: f64,
    confidence: f64,
) -> MockServer {
    let choice = choice.to_string();
    MockServer::start(move |target| {
        if target.contains("/ai/run/") {
            let candidate = format!("musicbrainz:{MBID}");
            let probabilities = serde_json::json!({
                candidate.clone(): if choice == candidate { probability } else { 0.0 },
                "none": if choice == "none" { probability } else { 1.0 - probability },
                "equivalent_editions": if choice == "equivalent_editions" { probability } else { 0.0 }
            });
            return (
                ai_status,
                serde_json::json!({
                    "success": ai_status == 200,
                    "result": {
                        "model": "clef-flash",
                        "answers": {"match": {
                            "type": "choice", "choice": choice,
                            "probabilities": probabilities, "confidence": confidence
                        }},
                        "usage": {"input_tokens": 1000, "output_tokens": 0}
                    },
                    "errors": [{"message": TOKEN}]
                })
                .to_string(),
            );
        }
        if target.contains(&format!("/ws/2/release/{MBID}")) {
            return (
                200,
                musicbrainz_release_json(MBID, &["Track 1!", "Track 2!"], &[180.0, 181.0]),
            );
        }
        (200, musicbrainz_empty_search())
    })
}

fn run(db: &Path, server: &MockServer, args: &[&str], credentials: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_radish"));
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .env("RADISH_CLOUDFLARE_MODEL", "clef-flash")
        .env_remove("RADISH_DISCOGS_TOKEN")
        .env_remove("RADISH_CLOUDFLARE_ACCOUNT_ID")
        .env_remove("RADISH_CLOUDFLARE_API_TOKEN")
        .stdin(Stdio::null());
    if credentials {
        command
            .env(
                "RADISH_CLOUDFLARE_ACCOUNT_ID",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .env("RADISH_CLOUDFLARE_API_TOKEN", TOKEN);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn ai_recommendation_is_saved_without_accepting_the_match() {
    let db = db_path("clef-advisory");
    let id = seed(&db, false).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("AI advice"), "{text}");
    assert!(text.contains("Accepted: 0"), "{text}");
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert_eq!(server.requests_matching("/ai/run/"), 1);
    let pool = radish::db::connect(&db).await.unwrap();
    let review = store::load_review(&pool, id, ProviderKind::MusicBrainz)
        .await
        .unwrap()
        .unwrap();
    let stored = serde_json::to_value(&review.candidates[0]).unwrap();
    assert_eq!(stored["ai_advice"]["confidence"], 1.0);
    assert_eq!(stored["ai_advice"]["choice"]["type"], "candidate");
    assert!(!review.candidates[0].auto_accept);
    assert!(
        store::accepted_matches_by_release(&pool)
            .await
            .unwrap()
            .get(&id)
            .is_none_or(Option::is_none)
    );
}

#[tokio::test]
async fn ai_can_abstain_without_turning_review_into_unmatched() {
    for choice in ["none", "equivalent_editions"] {
        let db = db_path(&format!("clef-{choice}"));
        let id = seed(&db, false).await;
        let server = server(200, choice);
        let output = run(
            &db,
            &server,
            &["match", "--provider", "musicbrainz", "--ai-review"],
            true,
        );
        assert!(output.status.success(), "{output:?}");
        assert!(stdout(&output).contains("Queued for review: 1"));
        let pool = radish::db::connect(&db).await.unwrap();
        let review = store::load_review(&pool, id, ProviderKind::MusicBrainz)
            .await
            .unwrap()
            .unwrap();
        let stored = serde_json::to_value(&review.candidates[0]).unwrap();
        assert_eq!(
            stored["ai_advice"]["choice"]["type"],
            if choice == "none" { "no_match" } else { choice }
        );
    }
}

#[tokio::test]
async fn settled_matches_do_not_contact_ai() {
    let db = db_path("clef-settled");
    seed(&db, true).await;
    let server = server(500, "none");
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        false,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Accepted: 1"));
    assert!(!stdout(&output).contains("AI review unavailable"));
    assert_eq!(server.requests_matching("/ai/run/"), 0);
}

#[tokio::test]
async fn ai_requires_explicit_opt_in_even_when_credentials_exist() {
    let db = db_path("clef-disabled");
    let id = seed(&db, false).await;
    let server = server(200, "none");
    let output = run(&db, &server, &["match", "--provider", "musicbrainz"], true);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(server.requests_matching("/ai/run/"), 0);
    let pool = radish::db::connect(&db).await.unwrap();
    let review = store::load_review(&pool, id, ProviderKind::MusicBrainz)
        .await
        .unwrap()
        .unwrap();
    let stored = serde_json::to_value(&review.candidates[0]).unwrap();
    assert!(
        stored
            .get("ai_advice")
            .is_none_or(serde_json::Value::is_null)
    );
}

#[tokio::test]
async fn ai_failure_is_nonfatal_and_never_prints_response_secrets() {
    let db = db_path("clef-failure");
    seed(&db, false).await;
    let server = server(503, "none");
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Queued for review: 1"));
    assert!(stdout(&output).contains("AI review unavailable"));
    assert!(!format!("{output:?}").contains(TOKEN));
}

#[tokio::test]
async fn missing_ai_credentials_preserve_the_review_queue() {
    let db = db_path("clef-missing-credentials");
    seed(&db, false).await;
    let server = server(200, "none");
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        false,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("AI review disabled for this run"));
    assert!(stdout(&output).contains("Queued for review: 1"));
    assert_eq!(server.requests_matching("/ai/run/"), 0);
}

#[tokio::test]
async fn a_rejected_ai_credential_disables_ai_for_the_rest_of_the_run() {
    let db = db_path("clef-run-shutoff");
    seed_in(&db, "/music/private-library/blue", false).await;
    seed_in(&db, "/music/private-library/blue-again", false).await;
    let server = server(401, "none");
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Queued for review: 2"), "{text}");
    assert_eq!(
        text.matches("AI review disabled for this run").count(),
        1,
        "{text}"
    );
    assert_eq!(server.requests_matching("/ai/run/"), 1, "{text}");
    assert!(!format!("{output:?}").contains(TOKEN));
}

#[tokio::test]
async fn missing_ai_credentials_are_reported_once_per_run() {
    let db = db_path("clef-missing-credentials-once");
    seed_in(&db, "/music/private-library/blue", false).await;
    seed_in(&db, "/music/private-library/blue-again", false).await;
    let server = server(200, "none");
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-review"],
        false,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Queued for review: 2"), "{text}");
    assert_eq!(
        text.matches("AI review disabled for this run").count(),
        1,
        "{text}"
    );
    assert_eq!(server.requests_matching("/ai/run/"), 0);
}

#[tokio::test]
async fn unchanged_queued_releases_reuse_saved_advice_instead_of_paying_again() {
    let db = db_path("clef-reuse");
    let id = seed(&db, false).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let args = ["match", "--provider", "musicbrainz", "--ai-review"];

    let first = run(&db, &server, &args, true);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(server.requests_matching("/ai/run/"), 1);

    let second = run(&db, &server, &args, true);
    assert!(second.status.success(), "{second:?}");
    let text = stdout(&second);
    assert_eq!(server.requests_matching("/ai/run/"), 1, "{text}");
    assert!(text.contains("AI advice"), "{text}");
    assert!(text.contains("saved"), "{text}");
    let pool = radish::db::connect(&db).await.unwrap();
    let review = store::load_review(&pool, id, ProviderKind::MusicBrainz)
        .await
        .unwrap()
        .unwrap();
    assert!(review.candidates[0].ai_advice.is_some());
}

#[tokio::test]
async fn changed_files_invalidate_saved_advice() {
    let db = db_path("clef-reuse-changed");
    seed(&db, false).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let args = ["match", "--provider", "musicbrainz", "--ai-review"];
    assert!(run(&db, &server, &args, true).status.success());
    assert_eq!(server.requests_matching("/ai/run/"), 1);

    // A retag changes the release revision, so the old answer no longer applies.
    let pool = radish::db::connect(&db).await.unwrap();
    let directory = Path::new("/music/private-library/blue");
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();
    store::upsert_track(
        &pool,
        &ScannedTrack {
            path: directory.join("1.flac"),
            title: Some("Another title".into()),
            artist: Some("Test Artist".into()),
            album: Some("Blue".into()),
            album_artist: Some("Test Artist".into()),
            year: Some(2001),
            track_number: Some(1),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz: ObservedMusicBrainzIds {
                release: vec![MBID.into()],
                ..Default::default()
            },
            duration_secs: Some(180.0),
            modified_at: 2,
        },
        directory_id,
    )
    .await
    .unwrap();
    store::rebuild_local_releases(&pool, None).await.unwrap();
    pool.close().await;

    assert!(run(&db, &server, &args, true).status.success());
    assert_eq!(server.requests_matching("/ai/run/"), 2);
}

#[tokio::test]
async fn ai_dry_run_shows_advice_without_saving_it() {
    let db = db_path("clef-dry-run");
    let id = seed(&db, false).await;
    let server = server(200, "none");
    let output = run(
        &db,
        &server,
        &[
            "match",
            "--provider",
            "musicbrainz",
            "--ai-review",
            "--dry-run",
        ],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("AI advice"));
    let pool = radish::db::connect(&db).await.unwrap();
    assert!(
        store::load_review(&pool, id, ProviderKind::MusicBrainz)
            .await
            .unwrap()
            .is_none()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM release_matches")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn seed_soft_mismatch(db: &Path) -> i64 {
    let id = seed(db, false).await;
    let pool = radish::db::connect(db).await.unwrap();
    sqlx::query(
        "UPDATE tracks SET title = CASE track_number WHEN 1 THEN 'Track One' ELSE 'Track Two' END",
    )
    .execute(&pool)
    .await
    .unwrap();
    store::rebuild_local_releases(&pool, None).await.unwrap();
    pool.close().await;
    id
}

#[tokio::test]
async fn strong_ai_match_accepts_album_metadata_with_auditable_evidence() {
    let db = db_path("clef-auto-album");
    let id = seed_soft_mismatch(&db).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-match"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("Accepted: 1"),
        "{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains("accept this release?"));
    let pool = radish::db::connect(&db).await.unwrap();
    let row: (String, String, String, Option<i64>, Option<String>, String) = sqlx::query_as(
        "SELECT match_scope, algorithm_version, acceptance_method, year, country, metadata_json FROM release_matches"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, "album");
    assert_eq!(row.1, "musicbrainz-clef-album-v2");
    assert_eq!(row.2, "automatic");
    assert_eq!(row.3, None);
    assert_eq!(row.4, None);
    let metadata: serde_json::Value = serde_json::from_str(&row.5).unwrap();
    assert_eq!(metadata["radish_ai_decision"]["advice"]["confidence"], 1.0);
    assert_eq!(
        metadata["radish_ai_decision"]["policy_version"],
        "clef-album-v2"
    );
    assert_eq!(
        metadata["radish_ai_decision"]["advice"]["prompt_version"],
        "radish-clef-album-v3"
    );
    assert!(
        store::load_review(&pool, id, ProviderKind::MusicBrainz)
            .await
            .unwrap()
            .is_none()
    );
    // Clearing is the existing reversible escape hatch for an incorrect decision.
    assert!(store::clear_match(&pool, id).await.unwrap());
    let title: String = sqlx::query_scalar("SELECT title FROM tracks WHERE track_number = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(title, "Track One");
}

#[tokio::test]
async fn uncertain_ai_answers_remain_queued_despite_plausible_track_evidence() {
    for (label, probability, confidence) in [("probability", 0.8, 1.0), ("confidence", 1.0, 0.8)] {
        let db = db_path(&format!("clef-auto-{label}"));
        seed_soft_mismatch(&db).await;
        let server =
            server_with_probability(200, &format!("musicbrainz:{MBID}"), probability, confidence);
        let output = run(
            &db,
            &server,
            &["match", "--provider", "musicbrainz", "--ai-match"],
            true,
        );
        assert!(output.status.success(), "{output:?}");
        assert!(stdout(&output).contains("Accepted: 0"));
        assert!(stdout(&output).contains("Queued for review: 1"));
        assert!(
            stdout(&output)
                .contains("AI match not applied: model confidence or selected probability")
        );
    }
}

#[tokio::test]
async fn strong_ai_cannot_override_a_duration_conflict() {
    let db = db_path("clef-auto-duration");
    seed_soft_mismatch(&db).await;
    let pool = radish::db::connect(&db).await.unwrap();
    sqlx::query("UPDATE tracks SET duration_secs = duration_secs + 20")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-match"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Accepted: 0"));
    assert!(stdout(&output).contains("Queued for review: 1"));
    let text = stdout(&output);
    let reason = text
        .lines()
        .find(|line| line.contains("AI match not applied:"))
        .unwrap();
    assert!(reason.contains("duration"), "{reason}");
    let pool = radish::db::connect(&db).await.unwrap();
    let reasons: String = sqlx::query_scalar("SELECT reasons_json FROM match_reviews")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(reasons.contains("AI match not applied:"), "{reasons}");
}

#[tokio::test]
async fn direct_selection_still_requires_explicit_review_with_strong_ai() {
    let db = db_path("clef-auto-direct");
    let id = seed_soft_mismatch(&db).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let output = run(
        &db,
        &server,
        &[
            "match",
            "--local-release",
            &id.to_string(),
            "--musicbrainz-release",
            MBID,
            "--ai-match",
        ],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Accepted: 0"));
    assert!(stdout(&output).contains("Queued for review: 1"));
}

#[tokio::test]
async fn automatic_ai_dry_run_does_not_store_a_match_or_review() {
    let db = db_path("clef-auto-dry");
    let id = seed_soft_mismatch(&db).await;
    let server = server(200, &format!("musicbrainz:{MBID}"));
    let output = run(
        &db,
        &server,
        &[
            "match",
            "--provider",
            "musicbrainz",
            "--ai-match",
            "--dry-run",
        ],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Accepted: 1"));
    let pool = radish::db::connect(&db).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM release_matches")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        store::load_review(&pool, id, ProviderKind::MusicBrainz)
            .await
            .unwrap()
            .is_none()
    );
}

const TWIN_MBID: &str = "11111111-1111-1111-1111-111111111111";

fn twin_server(equivalent: bool, conflicting_recordings: bool) -> MockServer {
    MockServer::start(move |target| {
        if target.contains("/ai/run/") {
            let choice = if equivalent {
                "equivalent_editions".to_string()
            } else {
                format!("musicbrainz:{MBID}")
            };
            return (
                200,
                serde_json::json!({"success":true,"result":{
                    "model":"clef-flash", "answers":{"match":{
                        "type":"choice", "choice":choice, "confidence":1.0,
                        "probabilities":{
                            format!("musicbrainz:{MBID}"):if equivalent {0.0} else {1.0},
                            format!("musicbrainz:{TWIN_MBID}"):0.0,
                            "none":0.0,"equivalent_editions":if equivalent {1.0} else {0.0}
                        }
                    }}, "usage":{"input_tokens":2000,"output_tokens":0}
                }})
                .to_string(),
            );
        }
        for id in [MBID, TWIN_MBID] {
            if target.contains(&format!("/ws/2/release/{id}")) {
                let mut document: serde_json::Value = serde_json::from_str(
                    &musicbrainz_release_json(id, &["Track 1!", "Track 2!"], &[180.0, 181.0]),
                )
                .unwrap();
                if id == TWIN_MBID && conflicting_recordings {
                    document["media"][0]["tracks"][0]["recording"]["id"] =
                        serde_json::json!("99999999-9999-4999-8999-999999999999");
                }
                return (200, document.to_string());
            }
        }
        let releases: Vec<_> = [MBID, TWIN_MBID]
            .into_iter()
            .map(|id| {
                serde_json::json!({
                    "id":id,"title":"Blue","date":"2001-05-01",
                    "artist-credit":[{"name":"Test Artist","joinphrase":""}]
                })
            })
            .collect();
        (
            200,
            serde_json::json!({"count":2,"offset":0,"releases":releases}).to_string(),
        )
    })
}

async fn seed_soft_without_edition_ids(db: &Path) -> i64 {
    let id = seed_soft_mismatch(db).await;
    let pool = radish::db::connect(db).await.unwrap();
    sqlx::query("UPDATE tracks SET musicbrainz_ids_json = '{}'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    id
}

#[tokio::test]
async fn equivalent_ai_editions_can_match_the_album_automatically() {
    let db = db_path("clef-auto-equivalent");
    seed_soft_without_edition_ids(&db).await;
    let server = twin_server(true, false);
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-match"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("Accepted: 1"),
        "{}",
        stdout(&output)
    );
    let pool = radish::db::connect(&db).await.unwrap();
    let scope: String = sqlx::query_scalar("SELECT match_scope FROM release_matches")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(scope, "album");
}

#[tokio::test]
async fn contradictory_close_recordings_still_need_review_despite_strong_ai() {
    let db = db_path("clef-auto-recording-conflict");
    seed_soft_without_edition_ids(&db).await;
    let server = twin_server(false, true);
    let output = run(
        &db,
        &server,
        &["match", "--provider", "musicbrainz", "--ai-match"],
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Accepted: 0"));
    assert!(stdout(&output).contains("Queued for review: 1"));
}
