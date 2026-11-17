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

/// A box of two albums, and a folder with a typo in one album tag. None of
/// these paths exists on this machine.
async fn seed(db: &Path) {
    let tracks = [
        track("/library/box/01.flac", "Floodland", 1),
        track("/library/box/02.flac", "Floodland", 2),
        track("/library/box/03.flac", "Vision Thing", 1),
        track("/library/box/04.flac", "Vision Thing", 2),
        track("/library/aug/01.flac", "augmented", 1),
        track("/library/aug/02.flac", "augmented", 2),
        track("/library/aug/03.flac", "augmentes", 3),
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
    assert!(text.ends_with("2 folders.\n"), "{text}");

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
