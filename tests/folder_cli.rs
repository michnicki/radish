//! `radish folder` against a real database whose files live elsewhere.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::store;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use support::db_path;

fn track(path: &str, album: &str, number: i64) -> ScannedTrack {
    ScannedTrack {
        path: PathBuf::from(path),
        title: Some(format!("Track {number}")),
        artist: Some("Test Artist".to_string()),
        album: Some(album.to_string()),
        album_artist: Some("Test Artist".to_string()),
        year: Some(2001),
        track_number: Some(number),
        disc_number: None,
        barcode: None,
        catalog_number: None,
        discogs_release_id: None,
        musicbrainz: ObservedMusicBrainzIds::default(),
        duration_secs: Some(180.0),
        modified_at: 1,
    }
}

/// A box of two albums, a folder with a typo in one album tag, and two disc
/// folders holding different albums. None of these paths exists on this
/// machine.
async fn seed(db: &Path) {
    let tracks = [
        track("/library/box/01.flac", "Floodland", 1),
        track("/library/box/02.flac", "Floodland", 2),
        track("/library/box/03.flac", "Vision Thing", 1),
        track("/library/box/04.flac", "Vision Thing", 2),
        track("/library/aug/01.flac", "augmented", 1),
        track("/library/aug/02.flac", "augmented", 2),
        track("/library/aug/03.flac", "augmentes", 3),
        track("/library/dm/cd1/01.flac", "Singles", 1),
        track("/library/dm/cd2/01.flac", "Remixes", 1),
    ];
    let pool = radish::db::connect(db).await.unwrap();
    let directory_id = store::upsert_directory(&pool, Path::new("/library"))
        .await
        .unwrap();
    store::upsert_tracks(&pool, &tracks, directory_id)
        .await
        .unwrap();
    store::rebuild_local_releases(&pool, None).await.unwrap();
    pool.close().await;
}

fn radish(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .current_dir(db.parent().unwrap())
        .arg("--db")
        .arg(db)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `(directory, key)` of every release.
async fn shapes(db: &Path) -> Vec<(String, String)> {
    let pool = radish::db::connect(db).await.unwrap();
    let rows = sqlx::query_as(
        "SELECT directory_path, album_title_norm FROM local_releases
         ORDER BY directory_path, album_title_norm",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    rows
}

/// The ID of the release whose files sit directly in `directory`.
async fn release_in(db: &Path, directory: &str) -> i64 {
    let pool = radish::db::connect(db).await.unwrap();
    let id = sqlx::query_scalar("SELECT id FROM local_releases WHERE directory_path = ?")
        .bind(directory)
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    id
}

async fn decisions(db: &Path) -> Vec<(String, String)> {
    let pool = radish::db::connect(db).await.unwrap();
    let rows = sqlx::query_as("SELECT directory_path, mode FROM folder_groupings ORDER BY 1")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    rows
}

#[tokio::test]
async fn folder_list_shows_the_box_and_the_merged_typo() {
    let db = db_path("folder-list");
    seed(&db).await;
    let output = radish(&db, &["folder", "list"]);
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("/library/aug\n  merged automatically"),
        "{text}"
    );
    assert!(
        text.contains("/library/box\n  automatic: separate"),
        "{text}"
    );
    assert!(
        text.contains("/library/dm\n  automatic: separate"),
        "{text}"
    );
    assert!(text.ends_with("3 folders.\n"), "{text}");

    let releases = stdout(&radish(&db, &["releases"]));
    assert!(
        releases.contains("merged: 2 album titles, 1 folder (automatic)"),
        "{releases}"
    );
}

#[tokio::test]
async fn merge_split_and_auto_change_the_box() {
    let db = db_path("folder-decide");
    seed(&db).await;

    let output = radish(&db, &["folder", "merge", "/library/box"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).starts_with("Merged: /library/box\n  release #"),
        "{output:?}"
    );
    assert!(
        shapes(&db)
            .await
            .contains(&("/library/box".into(), "*".into()))
    );

    let output = radish(&db, &["folder", "split", "/LIBRARY/box/"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("Kept separate: /library/box"),
        "{output:?}"
    );
    assert_eq!(
        decisions(&db).await,
        vec![("/library/box".to_string(), "separate".to_string())]
    );

    let output = radish(&db, &["folder", "auto", "/library/box"]);
    assert!(output.status.success(), "{output:?}");
    assert!(decisions(&db).await.is_empty());
    let box_keys: Vec<String> = shapes(&db)
        .await
        .into_iter()
        .filter(|(directory, _)| directory == "/library/box")
        .map(|(_, key)| key)
        .collect();
    assert_eq!(box_keys, ["floodland", "vision thing"]);
}

#[tokio::test]
async fn an_unknown_target_fails_before_anything_is_written() {
    let db = db_path("folder-unknown");
    seed(&db).await;
    let output = radish(&db, &["folder", "merge", "/library/box", "999999"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("local release 999999 was not found"),
        "{stderr}"
    );
    assert!(decisions(&db).await.is_empty());
}

#[tokio::test]
async fn targets_in_one_unit_are_decided_once() {
    let db = db_path("folder-dedup");
    seed(&db).await;
    let id = release_in(&db, "/library/dm/cd1").await.to_string();
    // A release in the unit, its disc folder and the unit itself (in another
    // case, with a trailing slash) all name /library/dm.
    let output = radish(
        &db,
        &["folder", "split", &id, "/library/dm/cd1", "/LIBRARY/dm/"],
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(
        text.matches("Kept separate: /library/dm\n").count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("Kept separate:").count(), 1, "{text}");
    assert_eq!(
        decisions(&db).await,
        vec![("/library/dm".to_string(), "separate".to_string())]
    );
}

#[tokio::test]
async fn auto_says_the_automatic_rule_decides_again() {
    let db = db_path("folder-auto");
    seed(&db).await;
    let output = radish(&db, &["folder", "auto", "/library/aug"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).starts_with("Left to the automatic rule: /library/aug\n  release #"),
        "{output:?}"
    );
}

#[tokio::test]
async fn list_path_widens_a_disc_folder_to_its_unit() {
    let db = db_path("folder-list-path");
    seed(&db).await;
    let output = radish(&db, &["folder", "list", "--path", "/library/dm/cd1"]);
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.starts_with("/library/dm\n  automatic: separate\n"),
        "{text}"
    );
    assert!(
        text.contains("Singles") && text.contains("Remixes"),
        "{text}"
    );
    assert!(text.ends_with("1 folder.\n"), "{text}");
}

#[tokio::test]
async fn an_empty_list_says_so() {
    let db = db_path("folder-list-empty");
    seed(&db).await;
    let output = radish(&db, &["folder", "list", "--path", "/library/nowhere"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout(&output),
        "No folder holds several releases, a merged release or a grouping decision.\n"
    );
}

#[tokio::test]
async fn an_unknown_path_is_named_as_given() {
    let db = db_path("folder-unknown-path");
    seed(&db).await;
    let output = radish(&db, &["folder", "merge", "/library/nowhere"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("'/library/nowhere' is not a folder with local releases"),
        "{stderr}"
    );
    assert!(decisions(&db).await.is_empty());
}
