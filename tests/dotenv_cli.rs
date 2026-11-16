//! Startup configuration uses isolated working directories and fake credentials.
//!
//! The probe is the Discogs personal token: `match --provider discogs` sends it
//! in the `Authorization` header, which the loopback mock records, so each test
//! observes exactly which value, if any, the process picked up.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::store;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::{MockServer, musicbrainz_empty_search, musicbrainz_release_json, temp_dir};

const MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
const FILE_TOKEN: &str = "synthetic-file-token";
const ENV_TOKEN: &str = "synthetic-env-token";

async fn seed(db: &Path) {
    let pool = radish::db::connect(db).await.unwrap();
    let directory = Path::new("/synthetic/dotenv-album");
    let directory_id = store::upsert_directory(&pool, directory).await.unwrap();
    for number in 1..=2 {
        store::upsert_track(
            &pool,
            &ScannedTrack {
                path: directory.join(format!("{number}.flac")),
                title: Some(format!("Unresolved local title {number}")),
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
    pool.close().await;
}

fn server() -> MockServer {
    MockServer::start(|target| {
        if target.contains("/database/search") {
            return (
                200,
                r#"{"pagination":{"page":1,"pages":1,"per_page":50,"items":0},"results":[]}"#
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

fn command(cwd: &Path, db: &Path, server: &MockServer) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_radish"));
    command
        .current_dir(cwd)
        .arg("--db")
        .arg(db)
        .env_remove("RADISH_DISCOGS_TOKEN")
        // A developer's proxy exemptions must not let a test reach a real host.
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        // Pin provider services even before .env support exists: failing tests
        // must also remain completely local.
        .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .env("RADISH_DEEZER_BASE_URL", &server.url)
        .stdin(Stdio::null());
    command
}

/// A Discogs-only match run, which searches Discogs when a token is available.
/// Proxy variables are cleared so a `.env` value could only come from the file.
fn discogs_match(cwd: &Path, db: &Path, server: &MockServer) -> Command {
    let mut command = command(cwd, db, server);
    command
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .args(["match", "--provider", "discogs"]);
    command
}

/// Every Discogs search carried exactly this token and no other.
fn assert_token_used(output: &Output, server: &MockServer, token: &str) {
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    let searches = server.requests_matching("/database/search");
    assert!(
        searches >= 1,
        "no Discogs search was made: {text}\n{errors}"
    );
    assert_eq!(
        server.requests_with_header(&format!("Discogs token={token}")),
        searches,
        "{:?}",
        server.targets()
    );
    assert_eq!(server.requests_with_header("token="), searches);
    // The token never reaches the terminal.
    assert!(!text.contains(token), "{text}");
    assert!(!errors.contains(token), "{errors}");
}

#[tokio::test]
async fn loads_the_discogs_token_from_working_directory_dotenv() {
    let cwd = temp_dir("dotenv-current");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(
        cwd.join(".env"),
        format!("RADISH_DISCOGS_TOKEN={FILE_TOKEN}\n"),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_token_used(&output, &server, FILE_TOKEN);
}

#[tokio::test]
async fn ignores_dotenv_in_parent_of_working_directory() {
    let parent = temp_dir("dotenv-parent");
    let cwd = parent.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let db = parent.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(
        parent.join(".env"),
        format!("RADISH_DISCOGS_TOKEN={FILE_TOKEN}\n"),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server).output().unwrap();
    // Without a token Discogs cannot start, so nothing is requested at all.
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(server.request_count(), 0, "{text}\n{errors}");
    assert!(!text.contains(FILE_TOKEN), "{text}");
    assert!(!errors.contains(FILE_TOKEN), "{errors}");
}

#[tokio::test]
async fn dotenv_loads_only_allowed_keys() {
    let cwd = temp_dir("dotenv-allowlist");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    let decoy = MockServer::start(|_| (200, String::new()));
    // HTTP_PROXY is not an allowed key. If the file's value were applied, the
    // Discogs requests to the plain-HTTP mock would be sent through the decoy.
    std::fs::write(
        cwd.join(".env"),
        format!(
            "RADISH_DISCOGS_TOKEN={FILE_TOKEN}\nHTTP_PROXY={}\nhttp_proxy={}\n",
            decoy.url, decoy.url
        ),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    // The allowed key took effect, the other did not.
    assert_token_used(&output, &server, FILE_TOKEN);
    assert_eq!(decoy.request_count(), 0, "{:?}", decoy.targets());
}

#[tokio::test]
async fn dotenv_cannot_redirect_any_provider_endpoint() {
    for (provider, variable, default_host, origin_path) in [
        (
            "musicbrainz",
            "RADISH_MUSICBRAINZ_BASE_URL",
            "musicbrainz.org",
            "/ws/2/",
        ),
        (
            "discogs",
            "RADISH_DISCOGS_BASE_URL",
            "api.discogs.com",
            "/database/",
        ),
        (
            "deezer",
            "RADISH_DEEZER_BASE_URL",
            "api.deezer.com",
            "/search/",
        ),
    ] {
        let cwd = temp_dir(&format!("dotenv-endpoint-{provider}"));
        let db = cwd.join("library.db");
        seed(&db).await;
        let server = server();
        // The file points at a loopback mock, which is a valid override value:
        // only the allowlist keeps it out. Without it, the provider falls back
        // to its public host, reached here only through the local proxy.
        std::fs::write(cwd.join(".env"), format!("{variable}={}\n", server.url)).unwrap();
        let output = command(&cwd, &db, &server)
            .env_remove("RADISH_MUSICBRAINZ_BASE_URL")
            .env_remove("RADISH_DISCOGS_BASE_URL")
            .env_remove("RADISH_DEEZER_BASE_URL")
            .env("RADISH_DISCOGS_TOKEN", "synthetic-env-token")
            .env("HTTPS_PROXY", &server.url)
            .env("https_proxy", &server.url)
            .args(["match", "--provider", provider])
            .output()
            .unwrap();
        let targets = server.targets();
        assert!(
            !targets.iter().any(|target| target.contains(origin_path)),
            "{provider}: the file's endpoint override was applied: {targets:?}"
        );
        assert!(
            targets
                .iter()
                .any(|target| target.starts_with(default_host)),
            "{provider}: expected a proxied request to {default_host}: {targets:?} {output:?}"
        );
    }
}

/// Runs `radish list` in `cwd`, failing instead of hanging when it never exits.
fn list_with_timeout(cwd: &Path, db: &Path, server: &MockServer) -> Option<Output> {
    let mut child = command(cwd, db, server)
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            return Some(child.wait_with_output().unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn a_dotenv_directory_is_ignored_instead_of_hanging() {
    let cwd = temp_dir("dotenv-directory");
    std::fs::create_dir(cwd.join(".env")).unwrap();
    let server = server();

    let output = list_with_timeout(&cwd, &cwd.join("library.db"), &server)
        .expect("radish did not exit with a .env directory present");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("No tracks found."));
}

#[tokio::test]
async fn a_byte_order_mark_does_not_hide_the_first_dotenv_key() {
    let cwd = temp_dir("dotenv-bom");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    // PowerShell 5 writes a UTF-8 BOM; the token is the first line.
    std::fs::write(
        cwd.join(".env"),
        format!("\u{feff}RADISH_DISCOGS_TOKEN={FILE_TOKEN}\n"),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_token_used(&output, &server, FILE_TOKEN);
}

#[tokio::test]
async fn a_malformed_dotenv_line_does_not_stop_later_keys_loading() {
    let cwd = temp_dir("dotenv-bad-line");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(
        cwd.join(".env"),
        format!("this line is not an assignment\nRADISH_DISCOGS_TOKEN={FILE_TOKEN}\n"),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_token_used(&output, &server, FILE_TOKEN);
}

#[tokio::test]
async fn process_environment_overrides_conflicting_dotenv_values() {
    let cwd = temp_dir("dotenv-precedence");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(
        cwd.join(".env"),
        format!("RADISH_DISCOGS_TOKEN={FILE_TOKEN}\n"),
    )
    .unwrap();
    let output = discogs_match(&cwd, &db, &server)
        .env("RADISH_DISCOGS_TOKEN", ENV_TOKEN)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_token_used(&output, &server, ENV_TOKEN);
    assert_eq!(server.requests_with_header(FILE_TOKEN), 0);
}

#[test]
fn missing_or_malformed_dotenv_preserves_offline_commands_and_private_errors() {
    for malformed in [false, true] {
        let cwd = temp_dir(if malformed {
            "dotenv-malformed"
        } else {
            "dotenv-absent"
        });
        let db = cwd.join("library.db");
        let server = server();
        if malformed {
            std::fs::write(
                cwd.join(".env"),
                "PRIVATE_MARKER=\"unterminated secret-file-value",
            )
            .unwrap();
        }
        let output = command(&cwd, &db, &server).arg("list").output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("No tracks found."));
        assert!(output.stderr.is_empty(), "{output:?}");
        assert!(!format!("{output:?}").contains("secret-file-value"));
        assert_eq!(server.request_count(), 0);

        let unopened = cwd.join("invalid.db");
        let output = command(&cwd, &unopened, &server)
            .args([
                "match",
                "--provider",
                "discogs",
                "--local-release",
                "1",
                "--musicbrainz-release",
                MBID,
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!unopened.exists());
        assert!(!format!("{output:?}").contains("secret-file-value"));
        assert_eq!(server.request_count(), 0);
    }
}
