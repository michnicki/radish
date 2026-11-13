//! Building an index from a synthetic dump and matching from it with no
//! network at all.

mod support;

use std::path::Path;
use std::process::{Command, Output};
use support::{db_path, temp_dir, write_mb_archive, write_tagged_wav};

const RELEASE_GID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
const GROUP_GID: &str = "4642ee19-7790-3c8d-ab5e-d133de942db6";
const ARTIST_GID: &str = "0f2b0c7d-2222-4c1d-9a51-1d2f2b0a5a11";
const TRACK_ONE_GID: &str = "22222222-2222-2222-2222-222222222222";
const TRACK_TWO_GID: &str = "33333333-3333-3333-3333-333333333333";
const RECORDING_ONE_GID: &str = "aaaaaaaa-1111-1111-1111-111111111111";
const RECORDING_TWO_GID: &str = "bbbbbbbb-2222-2222-2222-222222222222";

/// The minimum columns the builder reads, in the dump's own shape.
fn schema() -> &'static str {
    "CREATE TABLE release (id SERIAL, gid UUID NOT NULL, name VARCHAR NOT NULL, artist_credit INTEGER NOT NULL, release_group INTEGER NOT NULL, status INTEGER, barcode VARCHAR, comment VARCHAR(255) NOT NULL DEFAULT '');\n\
     CREATE TABLE release_group (id SERIAL, gid UUID NOT NULL, name VARCHAR NOT NULL);\n\
     CREATE TABLE medium (id SERIAL, release INTEGER NOT NULL, position INTEGER NOT NULL, format INTEGER, track_count INTEGER NOT NULL DEFAULT 0);\n\
     CREATE TABLE medium_format (id SERIAL, name VARCHAR NOT NULL);\n\
     CREATE TABLE track (id SERIAL, gid UUID NOT NULL, recording INTEGER NOT NULL, medium INTEGER NOT NULL, position INTEGER NOT NULL, number TEXT NOT NULL, name VARCHAR NOT NULL, artist_credit INTEGER NOT NULL, length INTEGER, is_data_track BOOLEAN NOT NULL DEFAULT FALSE);\n\
     CREATE TABLE recording (id SERIAL, gid UUID NOT NULL, name VARCHAR NOT NULL, length INTEGER, video BOOLEAN NOT NULL DEFAULT FALSE);\n\
     CREATE TABLE artist (id SERIAL, gid UUID NOT NULL);\n\
     CREATE TABLE artist_credit_name (artist_credit INTEGER NOT NULL, position INTEGER NOT NULL, artist INTEGER NOT NULL, name VARCHAR NOT NULL, join_phrase TEXT NOT NULL DEFAULT '');\n\
     CREATE TABLE release_label (id SERIAL, release INTEGER NOT NULL, label INTEGER, catalog_number VARCHAR);\n\
     CREATE TABLE label (id SERIAL, name VARCHAR NOT NULL);\n\
     CREATE TABLE release_country (release INTEGER NOT NULL, country INTEGER NOT NULL, date_year SMALLINT, date_month SMALLINT, date_day SMALLINT);\n\
     CREATE TABLE area (id SERIAL, name VARCHAR NOT NULL);\n\
     CREATE TABLE release_status (id SERIAL, name VARCHAR NOT NULL);\n"
}

fn musicbrainz_archive(dir: &Path) -> std::path::PathBuf {
    musicbrainz_archive_for(dir, "Blue", "Test Artist", ["One", "Two"])
}

/// The one-release dump again, describing a different album, so a test can make
/// MusicBrainz miss a library release that another index holds.
fn musicbrainz_archive_for(
    dir: &Path,
    album: &str,
    artist: &str,
    tracks: [&str; 2],
) -> std::path::PathBuf {
    let rows = [
        ("CreateTables.sql", schema().to_string()),
        ("release", format!("1\t{RELEASE_GID}\t{album}\t1\t1\t1\t\t")),
        ("release_group", format!("1\t{GROUP_GID}\t{album}")),
        ("medium", "1\t1\t1\t1\t2".to_string()),
        ("medium_format", "1\tCD".to_string()),
        (
            "track",
            format!(
                "1\t{TRACK_ONE_GID}\t1\t1\t1\t1\t{one}\t1\t1000\tf\n\
                 2\t{TRACK_TWO_GID}\t2\t1\t2\t2\t{two}\t2\t1000\tf",
                one = tracks[0],
                two = tracks[1],
            ),
        ),
        (
            "recording",
            format!(
                "1\t{RECORDING_ONE_GID}\t{one}\t0\tf\n\
                 2\t{RECORDING_TWO_GID}\t{two}\t0\tf",
                one = tracks[0],
                two = tracks[1],
            ),
        ),
        ("artist", format!("1\t{ARTIST_GID}")),
        (
            "artist_credit_name",
            format!("1\t0\t1\t{artist}\t\n2\t0\t1\t{artist}\t"),
        ),
        ("release_label", "1\t1\t1\tCAT-1".to_string()),
        ("label", "1\tLabel".to_string()),
        ("release_country", "1\t1\t2001\t1\t1".to_string()),
        ("area", "1\tUS".to_string()),
        ("release_status", "1\tOfficial".to_string()),
    ];
    let files: Vec<(&str, &str)> = rows
        .iter()
        .map(|(name, body)| (*name, body.as_str()))
        .collect();
    write_mb_archive(dir, &files)
}

/// Runs the built binary with every provider endpoint pointed at a closed
/// loopback port, so any network attempt would fail instead of passing quietly.
fn radish(db: &Path, index_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .env("RADISH_INDEX_DIR", index_dir)
        .env("RADISH_MUSICBRAINZ_BASE_URL", "http://127.0.0.1:9")
        .env("RADISH_DISCOGS_BASE_URL", "http://127.0.0.1:9")
        .env_remove("RADISH_DISCOGS_TOKEN")
        .env_remove("RADISH_CLOUDFLARE_ACCOUNT_ID")
        .env_remove("RADISH_CLOUDFLARE_API_TOKEN")
        .args(args)
        .output()
        .unwrap()
}

fn library(name: &str) -> std::path::PathBuf {
    let music = temp_dir(name);
    write_album(&music, "Blue", 2001);
    music
}

/// Writes (or rewrites, which is how a test retags) one two-track album folder.
fn write_album(music: &Path, album: &str, year: u16) {
    let folder = music.join(album);
    std::fs::create_dir_all(&folder).unwrap();
    for (position, title) in [(1, "One"), (2, "Two")] {
        write_tagged_wav(
            &folder.join(format!("0{position} {title}.wav")),
            album,
            "Test Artist",
            title,
            position,
            year,
        );
    }
}

/// A plain-XML Discogs dump holding one two-track, one-second-per-track release
/// per `(id, album)`, matching what [`write_album`] puts on disk.
fn discogs_dump(dir: &Path, releases: &[(u32, &str)]) -> std::path::PathBuf {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<releases>\n");
    for (id, album) in releases {
        xml.push_str(&format!(
            "<release id=\"{id}\" status=\"Accepted\"><title>{album}</title>\
             <artists><artist id=\"1\"><name>Test Artist</name><anv/><join/></artist></artists>\
             <labels><label name=\"Label\" catno=\"CAT-{id}\"/></labels>\
             <formats><format name=\"CD\" qty=\"1\"><descriptions><description>Album</description></descriptions></format></formats>\
             <country>US</country><released>2001-01-01</released>\
             <tracklist>\
             <track><position>1</position><title>One</title><duration>0:01</duration></track>\
             <track><position>2</position><title>Two</title><duration>0:01</duration></track>\
             </tracklist></release>\n"
        ));
    }
    xml.push_str("</releases>\n");
    let path = dir.join(format!("discogs-{}.xml", releases.len()));
    std::fs::write(&path, xml).unwrap();
    path
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `releases` block whose heading names `album`: its lines up to the next
/// blank line, joined with newlines.
fn release_block(listing: &str, album: &str) -> String {
    let heading = format!("— {album}");
    listing
        .lines()
        .skip_while(|line| !line.contains(&heading))
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_built_index_matches_a_library_without_any_network() {
    let db = db_path("offline-match");
    let index_dir = temp_dir("offline-match-index");
    let dumps = temp_dir("offline-match-dumps");
    let archive = musicbrainz_archive(&dumps);
    let music = library("offline-match-music");

    let build = radish(
        &db,
        &index_dir,
        &["index", "build", "--musicbrainz", archive.to_str().unwrap()],
    );
    assert!(
        build.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let build_stdout = String::from_utf8_lossy(&build.stdout);
    assert!(!build_stdout.contains("covered by"), "{build_stdout}");

    let status = radish(&db, &index_dir, &["index", "status"]);
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(status.status.success(), "{status:?}");
    assert!(
        status_stdout.contains("MusicBrainz: 1 release(s)"),
        "{status_stdout}"
    );

    let import = radish(&db, &index_dir, &["import", music.to_str().unwrap()]);
    assert!(
        import.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&import.stdout),
        String::from_utf8_lossy(&import.stderr)
    );

    let matched = radish(
        &db,
        &index_dir,
        &["match", "--offline", "--provider", "musicbrainz"],
    );
    let stdout = String::from_utf8_lossy(&matched.stdout);
    let stderr = String::from_utf8_lossy(&matched.stderr);
    assert!(matched.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("Accepted: 1"), "stdout={stdout}");
    assert!(stderr.is_empty(), "stderr={stderr}");

    let releases = radish(&db, &index_dir, &["releases"]);
    let releases_stdout = String::from_utf8_lossy(&releases.stdout);
    assert!(releases_stdout.contains(RELEASE_GID), "{releases_stdout}");
    assert!(releases_stdout.contains("matched"), "{releases_stdout}");
}

#[test]
fn offline_matching_without_an_index_names_the_path_and_the_build_command() {
    let db = db_path("offline-missing-index");
    let index_dir = temp_dir("offline-missing-index-dir");

    let output = radish(
        &db,
        &index_dir,
        &["match", "--offline", "--provider", "musicbrainz"],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("musicbrainz-index.db"), "{stderr}");
    assert!(stderr.contains("radish index build"), "{stderr}");
}

#[test]
fn the_default_policy_needs_both_indexes_before_any_release_is_processed() {
    let db = db_path("offline-both-indexes");
    let index_dir = temp_dir("offline-both-indexes-dir");
    let dumps = temp_dir("offline-both-indexes-dumps");
    let archive = musicbrainz_archive(&dumps);

    let build = radish(
        &db,
        &index_dir,
        &["index", "build", "--musicbrainz", archive.to_str().unwrap()],
    );
    assert!(build.status.success(), "{build:?}");

    let output = radish(&db, &index_dir, &["match", "--offline"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("discogs-index.db"), "{stderr}");
    assert!(stderr.contains("--discogs"), "{stderr}");
}

#[test]
fn index_build_requires_at_least_one_dump() {
    let db = db_path("offline-no-dump");
    let index_dir = temp_dir("offline-no-dump-dir");
    let output = radish(&db, &index_dir, &["index", "build"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("at least one dump"), "{stderr}");
}

#[test]
fn the_default_policy_falls_back_to_the_discogs_index_when_musicbrainz_misses() {
    let db = db_path("offline-fallback");
    let index_dir = temp_dir("offline-fallback-index");
    let dumps = temp_dir("offline-fallback-dumps");
    // MusicBrainz holds only an unrelated album; Discogs holds the library's.
    let musicbrainz = musicbrainz_archive_for(&dumps, "Unrelated", "Nobody", ["Alpha", "Beta"]);
    let discogs = discogs_dump(&dumps, &[(42, "Blue")]);
    let music = library("offline-fallback-music");

    let build = radish(
        &db,
        &index_dir,
        &[
            "index",
            "build",
            "--musicbrainz",
            musicbrainz.to_str().unwrap(),
            "--discogs",
            discogs.to_str().unwrap(),
        ],
    );
    assert!(build.status.success(), "{build:?}");
    let import = radish(&db, &index_dir, &["import", music.to_str().unwrap()]);
    assert!(import.status.success(), "{import:?}");

    let matched = radish(&db, &index_dir, &["match", "--offline"]);
    let (stdout, stderr) = (stdout_of(&matched), stderr_of(&matched));
    assert!(matched.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("Accepted: 1"), "{stdout}");
    assert!(stdout.contains("Discogs fallback"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");

    let releases = stdout_of(&radish(&db, &index_dir, &["releases"]));
    assert!(releases.contains("matched"), "{releases}");
    assert!(releases.contains("discogs"), "{releases}");
    assert!(!releases.contains(RELEASE_GID), "{releases}");
}

#[test]
fn a_stale_match_is_refreshed_from_the_local_index() {
    let db = db_path("offline-refresh");
    let index_dir = temp_dir("offline-refresh-index");
    let dumps = temp_dir("offline-refresh-dumps");
    let archive = musicbrainz_archive(&dumps);
    let music = library("offline-refresh-music");
    let music_arg = music.to_str().unwrap();
    let build = radish(
        &db,
        &index_dir,
        &["index", "build", "--musicbrainz", archive.to_str().unwrap()],
    );
    assert!(build.status.success(), "{build:?}");
    assert!(
        radish(&db, &index_dir, &["import", music_arg])
            .status
            .success()
    );
    let first = radish(
        &db,
        &index_dir,
        &["match", "--offline", "--provider", "musicbrainz"],
    );
    assert!(stdout_of(&first).contains("Accepted: 1"), "{first:?}");

    // Retagging the year changes the match inputs, so the match goes stale.
    write_album(&music, "Blue", 2002);
    assert!(
        radish(&db, &index_dir, &["import", "--rescan", music_arg])
            .status
            .success()
    );
    let stale = stdout_of(&radish(&db, &index_dir, &["releases"]));
    assert!(stale.contains("stale"), "{stale}");

    let refreshed = radish(
        &db,
        &index_dir,
        &["match", "--offline", "--provider", "musicbrainz"],
    );
    let (stdout, stderr) = (stdout_of(&refreshed), stderr_of(&refreshed));
    assert!(
        refreshed.status.success(),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("Accepted: 1"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
    let after = stdout_of(&radish(&db, &index_dir, &["releases"]));
    assert!(!after.contains("stale"), "{after}");
    assert!(after.contains(RELEASE_GID), "{after}");
}

#[test]
fn a_stale_discogs_match_missing_from_the_index_stays_stale_while_the_run_continues() {
    let db = db_path("offline-vanished");
    let index_dir = temp_dir("offline-vanished-dumps-index");
    let dumps = temp_dir("offline-vanished-dumps");
    let music = temp_dir("offline-vanished-music");
    write_album(&music, "Blue", 2001);
    let music_arg = music.to_str().unwrap();

    // The default policy needs both indexes; MusicBrainz holds nothing relevant.
    let musicbrainz = musicbrainz_archive_for(&dumps, "Unrelated", "Nobody", ["Alpha", "Beta"]);
    let first_dump = discogs_dump(&dumps, &[(42, "Blue")]);
    let build = radish(
        &db,
        &index_dir,
        &[
            "index",
            "build",
            "--musicbrainz",
            musicbrainz.to_str().unwrap(),
            "--discogs",
            first_dump.to_str().unwrap(),
        ],
    );
    assert!(build.status.success(), "{build:?}");
    assert!(
        radish(&db, &index_dir, &["import", music_arg])
            .status
            .success()
    );
    let first = radish(
        &db,
        &index_dir,
        &["match", "--offline", "--provider", "discogs"],
    );
    assert!(stdout_of(&first).contains("Accepted: 1"), "{first:?}");

    // A newer dump no longer holds release 42 but does hold a second album, and
    // the retag makes the accepted match stale.
    let newer_dump = discogs_dump(&dumps, &[(77, "Green"), (78, "Other")]);
    let rebuild = radish(
        &db,
        &index_dir,
        &["index", "build", "--discogs", newer_dump.to_str().unwrap()],
    );
    assert!(rebuild.status.success(), "{rebuild:?}");
    write_album(&music, "Blue", 2002);
    write_album(&music, "Green", 2001);
    assert!(
        radish(&db, &index_dir, &["import", "--rescan", music_arg])
            .status
            .success()
    );

    let output = radish(&db, &index_dir, &["match", "--offline"]);
    let (stdout, stderr) = (stdout_of(&output), stderr_of(&output));
    // Like the network search, a release the provider no longer returns is a
    // miss, not a failure: the run succeeds and the other album is matched.
    assert!(output.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("refreshing the stale discogs match"),
        "{stdout}"
    );
    assert!(stdout.contains("Accepted: 1"), "{stdout}");
    assert!(stdout.contains("Unmatched: 1"), "{stdout}");
    assert!(stdout.contains("Failed: 0"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");

    // The vanished release keeps its old overlay, still marked stale, because a
    // miss never erases an accepted match.
    let after = stdout_of(&radish(&db, &index_dir, &["releases"]));
    let blue = release_block(&after, "Blue");
    assert!(
        blue.contains("stale") && blue.contains("discogs 42"),
        "{after}"
    );
    let green = release_block(&after, "Green");
    assert!(
        green.contains("matched") && green.contains("discogs 77"),
        "{after}"
    );
}
