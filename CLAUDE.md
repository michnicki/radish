# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

CI (`.github/workflows/ci.yml`) runs all four of these on stable Rust; run them before calling work done:

```bash
cargo build --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
```

Narrower test runs:

```bash
cargo test --lib matcher::tests          # unit tests of one module (library crate)
cargo test --lib some_test_name          # unit tests whose name contains the string
cargo test --bin radish                  # unit tests in src/main.rs (CLI parsing and printing)
cargo test --test cli some_test_name     # one integration test file in tests/
```

The full suite takes about 40 seconds. The single `#[ignore]`d test is a live Discogs smoke test (`RADISH_DISCOGS_TOKEN=… RADISH_DISCOGS_RELEASE_ID=… cargo test --lib live_smoke -- --ignored`); no other test needs the network or credentials.

Manual runs: `cargo run -- --db /tmp/radish-dev.db import /path/to/music`, then `list`, `search`, `releases`, `match`, or `watch` with the same `--db` (default `./radish.db`).

## Architecture

All logic lives in the library crate (`src/lib.rs`). `src/main.rs` holds the Clap CLI, `.env` loading, and the `list`/`releases`/`folder` printing. Two pipelines share the SQLite store:

**Import and watch (always offline).** `scanner` walks the directory and reads tags with Lofty on Tokio's blocking pool. `importer::sync_directory` is the only import pipeline; `import` and the watcher's initial sync both use it to write observed metadata through `store`. `store::rebuild_local_releases` then regroups files into *local releases*, keyed by canonical directory plus normalized album title, or one per merged unit (`grouping`: a folder, or a folder with two or more disc subfolders, merged by a stored decision in `folder_groupings` or by the automatic rule; `album_title_norm` is `*`). `watcher` collects Notify events for 500 ms, then handles each affected path according to what is on disk, not the event kind.

**Matching (`match`, `import --match`).** `runner::run_match` calls `run_with_policy`, which takes three provider factory closures (MusicBrainz, Discogs, Deezer) so tests can inject fakes. For each local release:

1. `discovery` runs bounded searches and lookups through the `providers::MetadataProvider` trait (`search` and `fetch_release`).
2. `matcher` maps tracks, scores candidates, and applies the acceptance gates. It is pure: no network, no database.
3. `store` accepts the match in one transaction, or queues a review per provider.

The providers run as a chain: MusicBrainz, then Discogs, then Deezer when the run asks for it (`--deezer`). Each later stage runs only when the stages before it do not settle the release (`runner::settlement`), and its candidate is checked against every earlier stage's (`matcher::fallback_check`). Deezer is also skipped when it cannot change the outcome (`runner::deezer_skip_reason`). Discogs is constructed only when a stage needs it.

**Offline matching (`index build`, `match --offline`).** `index/` builds one SQLite index per provider from the official MusicBrainz and Discogs dumps, then serves it through `MetadataProvider`: `runner::run_match_offline` opens an index for MusicBrainz and Discogs when the active policy may attempt them and passes those adapters, with the network Deezer adapter (Deezer has no dump, so `--offline --deezer` is the one offline run that makes requests), into the same `run_with_policy` path, so discovery, scoring, and acceptance are unchanged. The MusicBrainz builder synthesizes the `/ws/2/release` document and runs the existing `musicbrainz::decode_release_document`, which keeps offline results identical to the network adapter by construction. Offline runs match up to `MatchOptions::jobs` releases at once (`--jobs`, default one per core up to 8) with `buffer_unordered` on one task: nearly all the time is SQLite work on sqlx's connection threads. Provider clients are shared through `runner::Clients`, and each `LocalIndex` remembers its last 256 searches.

**Web interface (`serve`).** `web.rs` is an axum server without JavaScript over the same store: Status (`store::library_status`), Tracks (`store::track_page`, the query behind `list` and `search`), Releases (`store::list_local_releases`) and Folders (`store::list_folder_units`, `store::folder_view`). Its one write is `POST /folder` (a folder decision), which needs the process's form token, refuses `Sec-Fetch-Site` other than `same-origin`, and is refused (`403`) unless the server is bound to a loopback address; the route exists on every bind. Pages are built by pure `render_*` functions, and every untrusted value goes through `web::escape_html` (`output::Clean`, then HTML escaping). It binds to loopback by default, and a `guard` middleware rejects foreign `Host` headers (DNS rebinding) and adds the security headers to every response. `layout.rs` holds the plain-text helpers shared by the CLI listings and the pages (`plural`, `Table`, `format_duration`, `track_number`).

Layering rules:
- Adapters (`musicbrainz.rs`, `discogs.rs`, `deezer.rs`) convert service payloads into `matcher::RemoteRelease`; provider types never reach `matcher` or `store`. Typed identities (`ReleaseId`, `Mbid`, `ProviderKind`) live in `providers.rs` and are stored as `(provider, TEXT id)`.
- Matching never overwrites scanned metadata or writes audio files. Accepted metadata is a separate overlay (`release_matches` and `release_track_matches`, at most one per local release across providers). `list` and `search` show the effective value: the overlay where present, scanned tags otherwise. Each local release's `release-input-v2:` source revision decides when a match goes stale.

`README.md` is the detailed behavior spec: thresholds, gates, CLI flag constraints, staleness, and import/watch edge cases. Read the relevant section before changing behavior, and update it with any CLI or behavior change. Design specs and implementation plans are in `docs/superpowers/`.

## Database and migrations

- `src/schema.sql` is the frozen pre-migration baseline, applied with `IF NOT EXISTS` on every connect. Put schema changes in a **new** timestamped file in `migrations/`; never edit an applied migration or `schema.sql`. `sqlx::migrate!` embeds migrations at compile time, and `build.rs` triggers a rebuild when they change.
- Paths are case-insensitive: `COLLATE NOCASE` in the schema, and `store::path_key` / `store::key_within` for comparisons and directory-scope checks in Rust. Keep this in new queries.
- Start write transactions with `store::begin_write` (`BEGIN IMMEDIATE`). Helpers called inside a transaction must take `&mut SqliteConnection`, not the pool: the in-memory test pool (`db::connect_in_memory`) has exactly one connection, so acquiring another hangs.
- Import deletes rows only when the walk shows positively that a file is gone. Unreadable paths, and walks that find no audio at all, keep their rows.

## Conventions that are easy to miss

- Write to stdout with the `outln!` / `out!` macros, not `println!`, so `radish list | head` exits cleanly. Wrap any path, tag value, or provider response, on stdout or stderr, in `output::Clean` / `output::clean_path` so control and bidirectional characters are escaped.
- In `runner.rs`, a release's progress goes through its `Transcript` with `outln_to!` / `eprintln_to!`, not `outln!` / `eprintln!`: releases matched at once hold their lines and print them together when they finish.
- `.cargo/config.toml` builds the bundled SQLite without `SQLITE_ENABLE_MEMORY_MANAGEMENT` (via `LIBSQLITE3_FLAGS`). That option puts every connection's page cache behind one global mutex, which kept parallel offline matching from scaling past four jobs.
- Reject invalid flag combinations before the database is opened or any client is built: Clap constraints, `parse_direct`, `runner::resolve_policy` and `runner::check_offline` in `main.rs::run`.
- `.env` is read from the current directory only, and only for the keys in `DOTENV_KEYS` (`src/main.rs`); add new config keys there. The `RADISH_*_BASE_URL` overrides are for tests only, accept loopback hosts only (`providers::loopback_override`), and are never read from `.env`.
- A scoped rebuild goes through `grouping::unit_scope` (a disc-subfolder root widens to the folder above it), or it regroups half a unit; scoped loads and listings instead add the merged release at `grouping::disc_parent_of`.
- Decisions are versioned. `matcher::MUSICBRAINZ_ALGORITHM_VERSION` / `DISCOGS_ALGORITHM_VERSION` are stored with accepted matches.

## Tests

- Unit tests live in module-local `#[cfg(test)] mod tests` blocks and use `src/test_support.rs` (`ScratchDir`, `write_wav`, `write_tagged_wav`) and `db::connect_in_memory()`.
- Integration tests in `tests/*.rs` run the built binary (`env!("CARGO_BIN_EXE_radish")`). `tests/support/mod.rs` provides a loopback `MockServer`, synthetic WAV writers, a MusicBrainz dump archive writer, and canned provider JSON; more fixtures are in `tests/fixtures/musicbrainz/`. Tests point `RADISH_*_BASE_URL` at the mock and `env_remove` credentials, so no test contacts a real service.
- Commit subjects use `type(scope): summary`, e.g. `fix(watcher): …`, `refactor: …`, `docs: …`.
