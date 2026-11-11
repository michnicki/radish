//! Startup configuration uses isolated working directories and fake credentials.

mod support;

use radish::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use radish::store;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::{MockServer, musicbrainz_empty_search, musicbrainz_release_json, temp_dir};

const MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
const FILE_ACCOUNT: &str = "synthetic-file-account";
const FILE_TOKEN: &str = "synthetic-file-token";

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
        if target.contains("/ai/run/") {
            let model = if target.ends_with("/@cf/cloudflare/clef") {
                "clef"
            } else {
                "clef-flash"
            };
            return (200, serde_json::json!({"success":true,"result":{
                "model":model, "answers":{"match":{
                    "type":"choice", "choice":"none", "confidence":1.0,
                    "probabilities":{format!("musicbrainz:{MBID}"):0.0,"none":1.0,"equivalent_editions":0.0}
                }}
            }}).to_string());
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
        .env_remove("RADISH_CLOUDFLARE_ACCOUNT_ID")
        .env_remove("RADISH_CLOUDFLARE_API_TOKEN")
        .env_remove("RADISH_CLOUDFLARE_BASE_URL")
        .env_remove("RADISH_CLOUDFLARE_MODEL")
        .env_remove("RADISH_DISCOGS_TOKEN")
        // A developer's proxy exemptions must not let a test reach a real host.
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        // Pin provider services even before .env support exists: failing tests
        // must also remain completely local.
        .env("RADISH_MUSICBRAINZ_BASE_URL", &server.url)
        .env("RADISH_DISCOGS_BASE_URL", &server.url)
        .stdin(Stdio::null());
    command
}

fn assert_reviewed(output: &Output, server: &MockServer, account: &str) {
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("AI advice"), "{text}");
    assert!(text.contains("Queued for review: 1"), "{text}");
    assert_eq!(server.requests_matching("/ai/run/"), 1);
    assert_eq!(
        server.requests_matching(&format!("/accounts/{account}/ai/run/")),
        1
    );
    assert!(!text.contains(FILE_TOKEN));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(FILE_TOKEN));
}

#[tokio::test]
async fn loads_fake_ai_credentials_from_working_directory_dotenv() {
    let cwd = temp_dir("dotenv-current");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(cwd.join(".env"), format!(
        "RADISH_CLOUDFLARE_ACCOUNT_ID={FILE_ACCOUNT}\nRADISH_CLOUDFLARE_API_TOKEN={FILE_TOKEN}\nRADISH_CLOUDFLARE_MODEL=clef\n"
    )).unwrap();
    // The base URL is not an allowed .env key, so the mock comes from the real environment.
    let output = command(&cwd, &db, &server)
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    assert_reviewed(&output, &server, FILE_ACCOUNT);
    assert!(
        server
            .targets()
            .iter()
            .any(|target| target.ends_with("/ai/run/@cf/cloudflare/clef"))
    );
}

#[tokio::test]
async fn ignores_dotenv_in_parent_of_working_directory() {
    let parent = temp_dir("dotenv-parent");
    let cwd = parent.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let db = parent.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(parent.join(".env"), format!(
        "RADISH_CLOUDFLARE_ACCOUNT_ID={FILE_ACCOUNT}\nRADISH_CLOUDFLARE_API_TOKEN={FILE_TOKEN}\n"
    )).unwrap();
    let output = command(&cwd, &db, &server)
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    // Without credentials the AI review cannot start, so no AI request is made.
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(server.requests_matching("/ai/run/"), 0, "{text}\n{errors}");
    assert!(!text.contains("AI advice"), "{text}");
}

#[tokio::test]
async fn dotenv_loads_only_allowed_keys_and_ignores_endpoint_overrides() {
    let cwd = temp_dir("dotenv-allowlist");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    // Route HTTPS through the mock: if the file's base URL were ignored, the
    // default Cloudflare origin is contacted via this proxy and recorded here
    // instead of reaching the network.
    std::fs::write(cwd.join(".env"), format!(
        "RADISH_CLOUDFLARE_ACCOUNT_ID={FILE_ACCOUNT}\nRADISH_CLOUDFLARE_API_TOKEN={FILE_TOKEN}\nRADISH_CLOUDFLARE_BASE_URL={}\n", server.url
    )).unwrap();
    let output = command(&cwd, &db, &server)
        .env("HTTPS_PROXY", &server.url)
        .env("https_proxy", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    // The file's endpoint override was not applied: no AI request reached the
    // mock as an origin-form path, only a proxy tunnel to the default origin.
    assert_eq!(server.requests_matching("/ai/run/"), 0, "{text}\n{errors}");
    assert!(
        server
            .targets()
            .iter()
            .any(|target| target.starts_with("api.cloudflare.com")),
        "the credential from .env was not used for the default origin: {:?}",
        server.targets()
    );
    assert!(!text.contains(FILE_TOKEN));
    assert!(!errors.contains(FILE_TOKEN));
}

#[tokio::test]
async fn dotenv_cannot_redirect_the_musicbrainz_or_discogs_endpoints() {
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
    // PowerShell 5 writes a UTF-8 BOM; the account ID is the first line.
    std::fs::write(cwd.join(".env"), format!(
        "\u{feff}RADISH_CLOUDFLARE_ACCOUNT_ID={FILE_ACCOUNT}\nRADISH_CLOUDFLARE_API_TOKEN={FILE_TOKEN}\n"
    )).unwrap();
    let output = command(&cwd, &db, &server)
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    assert_reviewed(&output, &server, FILE_ACCOUNT);
}

#[tokio::test]
async fn a_malformed_dotenv_line_does_not_stop_later_keys_loading() {
    let cwd = temp_dir("dotenv-bad-line");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    std::fs::write(cwd.join(".env"), format!(
        "RADISH_CLOUDFLARE_ACCOUNT_ID={FILE_ACCOUNT}\nthis line is not an assignment\nRADISH_CLOUDFLARE_API_TOKEN={FILE_TOKEN}\n"
    )).unwrap();
    let output = command(&cwd, &db, &server)
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    assert_reviewed(&output, &server, FILE_ACCOUNT);
}

#[tokio::test]
async fn process_environment_overrides_conflicting_dotenv_values() {
    let cwd = temp_dir("dotenv-precedence");
    let db = cwd.join("library.db");
    seed(&db).await;
    let server = server();
    // Each file value would prevent a request if it replaced the process value.
    std::fs::write(cwd.join(".env"), "RADISH_CLOUDFLARE_ACCOUNT_ID=invalid/account\nRADISH_CLOUDFLARE_API_TOKEN=\"invalid\\ntoken\"\nRADISH_CLOUDFLARE_BASE_URL=http://127.0.0.1:1\nRADISH_CLOUDFLARE_MODEL=unsupported\n").unwrap();
    let output = command(&cwd, &db, &server)
        .env("RADISH_CLOUDFLARE_ACCOUNT_ID", "synthetic-env-account")
        .env("RADISH_CLOUDFLARE_API_TOKEN", "synthetic-env-token")
        .env("RADISH_CLOUDFLARE_MODEL", "clef-flash")
        .env("RADISH_CLOUDFLARE_BASE_URL", &server.url)
        .args(["match", "--provider", "musicbrainz", "--ai-review"])
        .output()
        .unwrap();
    assert_reviewed(&output, &server, "synthetic-env-account");
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
