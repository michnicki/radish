//! `radish subsonic` over real HTTP against a real database.

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::{Reply, Server, db_path, temp_dir, write_tagged_wav};

/// The OpenSubsonic spec's token example: md5("sesame" + "c19b2d").
const AUTH: &str = "u=joe&t=26719a1196d2a940705a59634eb18eab&s=c19b2d&v=1.16.1&c=test&f=json";

const CREDENTIALS: [(&str, &str); 2] = [
    ("RADISH_SUBSONIC_USER", "joe"),
    ("RADISH_SUBSONIC_PASSWORD", "sesame"),
];

const JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
];

fn radish(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .args(args)
        .current_dir(db.parent().unwrap())
        .output()
        .unwrap()
}

/// One album of two tracks with a cover, imported.
fn library(name: &str) -> (PathBuf, PathBuf) {
    let db = db_path(name);
    let music = temp_dir(&format!("{name}-music"));
    let blue = music.join("Blue");
    std::fs::create_dir_all(&blue).unwrap();
    write_tagged_wav(
        &blue.join("1.wav"),
        "Blue",
        "Test Artist",
        "Blue Moon",
        1,
        2001,
    );
    write_tagged_wav(
        &blue.join("2.wav"),
        "Blue",
        "Test Artist",
        "Other Song",
        2,
        2001,
    );
    std::fs::write(blue.join("cover.jpg"), JPEG).unwrap();
    let import = radish(&db, &["import", music.to_str().unwrap()]);
    assert!(import.status.success(), "{import:?}");
    (db, music)
}

fn start(db: &Path) -> Server {
    Server::spawn(db, &["subsonic", "--bind", "127.0.0.1:0"], &CREDENTIALS)
}

/// The envelope of a JSON reply.
fn envelope(reply: &Reply) -> Value {
    assert_eq!(reply.status, 200, "{}", reply.body);
    let value: Value = serde_json::from_str(&reply.body).unwrap();
    value["subsonic-response"].clone()
}

#[test]
fn logins_are_checked_and_the_extensions_are_public() {
    let (db, _music) = library("subsonic-auth");
    let mut server = start(&db);

    assert_eq!(
        envelope(&server.get(&format!("/rest/ping?{AUTH}")))["status"],
        "ok"
    );
    assert_eq!(
        envelope(&server.get(&format!("/rest/ping.view?{AUTH}")))["status"],
        "ok"
    );
    let wrong = envelope(&server.get("/rest/ping?u=joe&p=wrong&f=json"));
    assert_eq!(wrong["error"]["code"], 40);
    let public = envelope(&server.get("/rest/getOpenSubsonicExtensions?f=json"));
    assert_eq!(public["openSubsonicExtensions"][0]["name"], "formPost");
    assert_eq!(
        envelope(&server.post("/rest/ping", AUTH, &[]))["status"],
        "ok"
    );

    let xml = server.get("/rest/ping?u=joe&p=sesame");
    assert!(
        xml.headers.contains("content-type: text/xml"),
        "{}",
        xml.headers
    );
    assert!(xml.body.contains("<subsonic-response "));
    assert!(xml.body.contains("xmlns=\"http://subsonic.org/restapi\""));
    assert!(xml.headers.contains("x-content-type-options: nosniff"));
    assert!(xml.headers.contains("referrer-policy: no-referrer"));
    assert!(
        xml.headers
            .contains("cross-origin-resource-policy: same-origin")
    );
    assert_eq!(server.get("/nope").status, 404);

    let stderr = server.stop();
    let refused: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("login refused"))
        .collect();
    assert_eq!(
        refused,
        [
            "login refused: user \"joe\", password, from 127.0.0.1, method ping: \
          wrong password (code 40)"
        ],
        "{stderr}"
    );
}

#[test]
fn a_client_browses_to_a_track_streams_it_and_fetches_the_cover() {
    let (db, music) = library("subsonic-browse");
    let server = start(&db);

    let artists = envelope(&server.get(&format!("/rest/getArtists?{AUTH}")));
    let artist = &artists["artists"]["index"][0]["artist"][0];
    assert_eq!(artist["name"], "Test Artist");
    let artist_id = artist["id"].as_str().unwrap();
    let artist = envelope(&server.get(&format!("/rest/getArtist?{AUTH}&id={artist_id}")));
    let album_id = artist["artist"]["album"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let album = envelope(&server.get(&format!("/rest/getAlbum?{AUTH}&id={album_id}")));
    let song = &album["album"]["song"][0];
    assert_eq!(song["title"], "Blue Moon");
    let file = std::fs::read(music.join("Blue").join("1.wav")).unwrap();
    assert_eq!(song["size"], file.len());
    let song_id = song["id"].as_str().unwrap();

    let whole = server.get(&format!("/rest/stream?{AUTH}&id={song_id}"));
    assert_eq!(whole.status, 200);
    assert!(
        whole.headers.contains("content-type: audio/wav"),
        "{}",
        whole.headers
    );
    assert!(whole.headers.contains("accept-ranges: bytes"));
    assert_eq!(whole.bytes, file);
    assert!(whole.headers.contains("x-content-type-options: nosniff"));
    assert!(
        whole
            .headers
            .contains("content-security-policy: default-src 'none'; sandbox")
    );
    assert!(whole.headers.contains("referrer-policy: no-referrer"));
    assert!(
        whole
            .headers
            .contains("cross-origin-resource-policy: same-origin")
    );
    let part = server.request_with(
        "GET",
        &format!("/rest/stream?{AUTH}&id={song_id}"),
        &["Range: bytes=10-19"],
    );
    assert_eq!(part.status, 206);
    assert_eq!(part.bytes, &file[10..20]);
    let head = server.request("HEAD", &format!("/rest/stream?{AUTH}&id={song_id}"), None);
    assert_eq!(head.status, 200);
    assert!(
        head.headers
            .contains(&format!("content-length: {}", file.len())),
        "{}",
        head.headers
    );
    assert!(head.headers.contains("accept-ranges: bytes"));
    assert!(head.bytes.is_empty());

    let cover = server.get(&format!("/rest/getCoverArt?{AUTH}&id={album_id}"));
    assert_eq!(cover.status, 200);
    assert!(cover.headers.contains("content-type: image/jpeg"));
    assert_eq!(cover.bytes, JPEG);
    assert!(cover.headers.contains("x-content-type-options: nosniff"));
    assert!(
        cover
            .headers
            .contains("content-security-policy: default-src 'none'; sandbox")
    );
    assert!(cover.headers.contains("referrer-policy: no-referrer"));
    assert!(
        cover
            .headers
            .contains("cross-origin-resource-policy: same-origin")
    );

    let everything = envelope(&server.get(&format!("/rest/search3?{AUTH}&query=")));
    assert_eq!(
        everything["searchResult3"]["song"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn missing_credentials_stop_the_command_before_the_database_is_created() {
    let db = db_path("subsonic-no-credentials");
    let output = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .args(["subsonic", "--bind", "127.0.0.1:0"])
        .current_dir(db.parent().unwrap())
        .env_remove("RADISH_SUBSONIC_USER")
        .env_remove("RADISH_SUBSONIC_PASSWORD")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("RADISH_SUBSONIC_USER"), "{stderr}");
    assert!(!db.exists());
}
