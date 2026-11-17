//! `radish serve` over real HTTP against a real database.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use support::{db_path, temp_dir, write_tagged_wav};

/// How long to wait for the server to announce its address.
const START_TIMEOUT: Duration = Duration::from_secs(20);

fn radish(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}

/// A running `radish serve`, killed when dropped.
struct Server {
    child: Child,
    addr: String,
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Server {
    fn start(db: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_radish"))
            .arg("--db")
            .arg(db)
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send(line);
        });
        let line = receiver
            .recv_timeout(START_TIMEOUT)
            .unwrap_or_else(|_| panic!("the server did not announce an address"));
        let Some(addr) = line
            .strip_prefix("Serving http://")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            let _ = child.kill();
            let mut stderr = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
            panic!("unexpected startup output {line:?}; stderr: {stderr}");
        };
        Self {
            addr: addr.to_string(),
            child,
        }
    }

    /// Sends one request with the given `Host` header (the real address when `None`).
    fn request(&self, method: &str, path: &str, host: Option<&str>) -> Reply {
        let host = host.unwrap_or(&self.addr);
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        read_reply(stream)
    }

    /// Sends a form `POST`, with extra header lines such as `Sec-Fetch-Site: same-origin`.
    fn post(&self, path: &str, body: &str, headers: &[&str]) -> Reply {
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let extra: String = headers.iter().map(|line| format!("{line}\r\n")).collect();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {}\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
             {extra}Connection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        )
        .unwrap();
        read_reply(stream)
    }

    fn get(&self, path: &str) -> Reply {
        self.request("GET", path, None)
    }
}

fn read_reply(mut stream: TcpStream) -> Reply {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Two tagged albums, one with markup in its tags.
fn seeded_library(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let db = db_path(name);
    let music = temp_dir(&format!("{name}-music"));
    let blue = music.join("Blue");
    let bold = music.join("Bold");
    std::fs::create_dir_all(&blue).unwrap();
    std::fs::create_dir_all(&bold).unwrap();
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
    write_tagged_wav(
        &bold.join("1.wav"),
        "<b>Bold</b>",
        "Mallory",
        "<script>alert(1)</script>",
        1,
        2002,
    );
    let import = radish(&db, &["import", music.to_str().unwrap()]);
    assert!(import.status.success(), "{import:?}");
    (db, music)
}

#[test]
fn status_page_shows_library_counts_and_security_headers() {
    let (db, music) = seeded_library("web-status");
    let server = Server::start(&db);

    let reply = server.get("/");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.headers.contains("content-type: text/html"),
        "{}",
        reply.headers
    );
    assert!(
        reply
            .headers
            .contains("content-security-policy: default-src 'none'"),
        "{}",
        reply.headers
    );
    assert!(reply.headers.contains("x-content-type-options: nosniff"));
    assert!(reply.headers.contains("referrer-policy: no-referrer"));
    // Nothing may frame a page: `frame-ancestors` has no `default-src` fallback.
    assert!(
        reply.headers.contains("frame-ancestors 'none'"),
        "{}",
        reply.headers
    );
    assert!(
        reply.headers.contains("x-frame-options: deny"),
        "{}",
        reply.headers
    );
    assert!(
        reply.body.contains("<b>3</b><span>Tracks</span>"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("<b>2</b><span>Local releases</span>"));
    assert!(reply.body.contains("<b>2</b><span>Unmatched</span>"));
    assert!(
        reply.body.contains(music.to_str().unwrap()),
        "{}",
        reply.body
    );
}

#[test]
fn an_empty_library_says_so() {
    let server = Server::start(&db_path("web-empty"));
    let reply = server.get("/");
    assert_eq!(reply.status, 200);
    assert!(
        reply.body.contains("Nothing imported yet"),
        "{}",
        reply.body
    );
    assert!(server.get("/tracks").body.contains("No tracks found."));
    assert!(
        server
            .get("/releases")
            .body
            .contains("No local releases found.")
    );
}

#[test]
fn tracks_page_lists_searches_and_escapes_markup() {
    let (db, _music) = seeded_library("web-tracks");
    let server = Server::start(&db);

    let all = server.get("/tracks");
    assert_eq!(all.status, 200);
    assert!(all.body.contains("Blue Moon") && all.body.contains("Other Song"));

    let found = server.get("/tracks?q=moon");
    assert!(found.body.contains("Blue Moon"), "{}", found.body);
    assert!(!found.body.contains("Other Song"), "{}", found.body);
    assert!(found.body.contains("value=\"moon\""), "{}", found.body);

    // Markup in a tag and in the search text both arrive escaped.
    let markup = server.get("/tracks?q=%3Cscript%3E");
    assert!(
        markup
            .body
            .contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{}",
        markup.body
    );
    assert!(
        markup.body.contains("value=\"&lt;script&gt;\""),
        "{}",
        markup.body
    );
    assert!(!markup.body.contains("<script"), "{}", markup.body);
    assert!(!all.body.contains("<b>Bold</b>"), "{}", all.body);
}

#[test]
fn releases_page_lists_each_local_release() {
    let (db, music) = seeded_library("web-releases");
    let server = Server::start(&db);

    let reply = server.get("/releases");
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("Test Artist") && reply.body.contains("Blue"));
    assert!(reply.body.contains("unmatched"), "{}", reply.body);
    assert!(
        reply.body.contains("&lt;b&gt;Bold&lt;/b&gt;"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains(music.join("Blue").to_str().unwrap()));
    assert!(
        reply.body.contains("2 releases on this page."),
        "{}",
        reply.body
    );
}

#[test]
fn tracks_are_paged_and_links_keep_the_search() {
    let db = db_path("web-paging");
    let music = temp_dir("web-paging-music");
    for n in 0..101 {
        let title = format!("Song {n:03}");
        write_tagged_wav(
            &music.join(format!("{n:03}.wav")),
            "Bulk",
            "Bulk Artist",
            &title,
            1,
            2001,
        );
    }
    let import = radish(&db, &["import", music.to_str().unwrap()]);
    assert!(import.status.success(), "{import:?}");
    let server = Server::start(&db);

    let rows = |reply: &Reply| reply.body.matches("<tr><td").count();
    let first = server.get("/tracks");
    assert_eq!(rows(&first), 100, "{}", first.body);
    assert!(
        first.body.contains("href=\"/tracks?page=2\">Next"),
        "{}",
        first.body
    );
    assert!(first.body.contains("<span>Previous</span>"));

    let second = server.get("/tracks?page=2");
    assert_eq!(rows(&second), 1, "{}", second.body);
    assert!(
        second.body.contains("href=\"/tracks\">Previous"),
        "{}",
        second.body
    );
    assert!(second.body.contains("<span>Next</span>"));

    // A search keeps its text in the page links.
    let searched = server.get("/tracks?q=bulk&page=1");
    assert_eq!(rows(&searched), 100, "{}", searched.body);
    assert!(
        searched
            .body
            .contains("href=\"/tracks?q=bulk&amp;page=2\">Next"),
        "{}",
        searched.body
    );

    // Bad page numbers fall back to the first page instead of failing.
    assert_eq!(rows(&server.get("/tracks?page=abc")), 100);
    assert_eq!(rows(&server.get("/tracks?page=0")), 100);
    assert_eq!(rows(&server.get("/tracks?page=9999")), 0);
}

#[test]
fn foreign_hosts_unknown_paths_and_writes_are_refused() {
    let server = Server::start(&db_path("web-guard"));

    let rebound = server.request("GET", "/", Some("evil.example"));
    assert_eq!(rebound.status, 403, "{}", rebound.body);
    assert!(rebound.headers.contains("content-security-policy"));
    assert!(!rebound.body.contains("Library status"));
    assert_eq!(server.request("GET", "/", Some("localhost:1")).status, 200);

    let missing = server.get("/nope");
    assert_eq!(missing.status, 404);
    assert!(missing.headers.contains("x-content-type-options: nosniff"));
    assert!(missing.headers.contains("x-frame-options: deny"));
    assert!(rebound.headers.contains("x-frame-options: deny"));

    let post = server.request("POST", "/", None);
    assert_eq!(post.status, 405);
}

#[test]
fn a_bad_bind_address_is_rejected_before_the_database_opens() {
    let db = db_path("web-bad-bind");
    let output = radish(&db, &["serve", "--bind", "not-an-address"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--bind"), "{stderr}");
    assert!(!db.exists(), "the database must not be created");
}

/// One folder whose two files carry unrelated album tags: two releases.
fn mixed_folder_library(name: &str) -> std::path::PathBuf {
    folder_library(name, false)
}

/// [`mixed_folder_library`], and with `discs` also real `CD1` and `CD2`
/// subfolders under it, each with its own album title, so they stay apart in
/// the same unit.
fn folder_library(name: &str, discs: bool) -> std::path::PathBuf {
    let db = db_path(name);
    let music = temp_dir(&format!("{name}-music"));
    let mixed = music.join("Mixed");
    std::fs::create_dir_all(&mixed).unwrap();
    write_tagged_wav(
        &mixed.join("1.wav"),
        "Blue",
        "Test Artist",
        "Blue Moon",
        1,
        2001,
    );
    write_tagged_wav(
        &mixed.join("2.wav"),
        "Rouge",
        "Test Artist",
        "Red Sky",
        2,
        2001,
    );
    if discs {
        for (folder, album) in [("CD1", "Alpha"), ("CD2", "Beta")] {
            let disc = mixed.join(folder);
            std::fs::create_dir_all(&disc).unwrap();
            write_tagged_wav(&disc.join("1.wav"), album, "Test Artist", "Song", 1, 2001);
        }
    }
    let output = radish(&db, &["import", music.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    db
}

/// The text between `start` and the next `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let rest = &text[text
        .find(start)
        .unwrap_or_else(|| panic!("no {start} in {text}"))
        + start.len()..];
    &rest[..rest.find(end).unwrap()]
}

/// The text of an HTML attribute value, with the entities `escape_html` writes undone.
fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn form_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[test]
fn a_folder_form_merges_only_with_the_token_from_this_site() {
    let db = folder_library("web-folder", true);
    let server = Server::start(&db);

    let folders = server.get("/folders");
    assert_eq!(folders.status, 200);
    assert!(
        folders.body.contains("automatic: separate"),
        "{}",
        folders.body
    );
    let link = format!(
        "/folder?path={}",
        between(&folders.body, "href=\"/folder?path=", "\"")
    );

    let page = server.get(&link);
    assert_eq!(page.status, 200);
    let token = between(&page.body, "name=\"token\" value=\"", "\"").to_string();
    let path = unescape_html(between(&page.body, "name=\"path\" value=\"", "\""));
    let body = |action: &str, token: &str, path: &str| {
        format!("path={}&action={action}&token={token}", form_encode(path))
    };

    let wrong = server.post(
        "/folder",
        &body("merge", "nope", &path),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(wrong.status, 403);
    let foreign = server.post(
        "/folder",
        &body("merge", &token, &path),
        &["Sec-Fetch-Site: cross-site"],
    );
    assert_eq!(foreign.status, 403);
    let bogus = server.post(
        "/folder",
        &body("bogus", &token, &path),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(bogus.status, 400);
    let missing = server.post(
        "/folder",
        &body("merge", &token, "/nowhere/at/all"),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(missing.status, 404);
    // Only the unit's own path is one: a real disc subfolder of it is not.
    let subfolder = server.post(
        "/folder",
        &body("merge", &token, &format!("{path}/CD1")),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(subfolder.status, 404);
    // A body over the limit is refused before it is read in full.
    let huge = server.post(
        "/folder",
        &format!("path={}&action=merge&token={token}", "x".repeat(1 << 20)),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(huge.status, 413);

    // A GET with the right fields changes nothing.
    let get = server.get(&format!(
        "/folder?path={}&action=merge&token={token}",
        form_encode(&path)
    ));
    assert_eq!(get.status, 200);
    assert!(server.get("/folders").body.contains("automatic: separate"));
    // Nor did any of the refusals.
    assert!(!server.get("/folders").body.contains("merged by you"));

    let merged = server.post(
        "/folder",
        &body("merge", &token, &path),
        &["Sec-Fetch-Site: same-origin"],
    );
    assert_eq!(merged.status, 303, "{}", merged.body);
    assert!(
        merged.headers.contains("location: /folder?path="),
        "{}",
        merged.headers
    );
    assert!(server.get("/folders").body.contains("merged by you"));
}

#[test]
fn a_browser_that_sends_no_fetch_metadata_still_posts_with_the_token() {
    let db = mixed_folder_library("web-folder-token-only");
    let server = Server::start(&db);
    let folders = server.get("/folders");
    let link = format!(
        "/folder?path={}",
        between(&folders.body, "href=\"/folder?path=", "\"")
    );
    let page = server.get(&link);
    let token = between(&page.body, "name=\"token\" value=\"", "\"");
    let path = unescape_html(between(&page.body, "name=\"path\" value=\"", "\""));

    let wrong = server.post(
        "/folder",
        &format!("path={}&action=merge&token=nope", form_encode(&path)),
        &[],
    );
    assert_eq!(wrong.status, 403);
    assert!(
        wrong.body.contains("reload the folder page"),
        "{}",
        wrong.body
    );

    let merged = server.post(
        "/folder",
        &format!("path={}&action=merge&token={token}", form_encode(&path)),
        &[],
    );
    assert_eq!(merged.status, 303, "{}", merged.body);
    assert!(server.get("/folders").body.contains("merged by you"));
}

#[test]
fn an_unknown_folder_page_is_not_found() {
    let (db, _music) = seeded_library("web-folder-404");
    let server = Server::start(&db);
    assert_eq!(server.get("/folder?path=%2Fnowhere").status, 404);
    assert_eq!(server.get("/folder").status, 404);
}
