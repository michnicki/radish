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

**Import and watch (always offline).** `scanner` walks the directory and reads tags with Lofty on Tokio's blocking pool. `importer::sync_directory` is the only import pipeline; `import` and the watcher's initial sync both use it to write observed metadata through `store`. It claims its root with `store::claim_root`, which keeps imported roots from nesting: a folder inside a root uses that root, and a parent takes over the roots inside it. `store::rebuild_local_releases` then regroups files into *local releases*, keyed by canonical directory plus normalized album title, or one per merged unit (`grouping`: a folder, or a folder with two or more disc subfolders, merged by a stored decision in `folder_groupings` or by the automatic rule; `album_title_norm` is `*`). `watcher` collects Notify events for 500 ms, then handles each affected path according to what is on disk, not the event kind.

**Matching (`match`, `import --match`).** `runner::run_match` calls `run_with_policy`, which takes three provider factory closures (MusicBrainz, Discogs, Deezer) so tests can inject fakes. For each local release:

1. `discovery` runs bounded searches and lookups through the `providers::MetadataProvider` trait (`search` and `fetch_release`).
2. `matcher` maps tracks, scores candidates, and applies the acceptance gates. It is pure: no network, no database.
3. `store` accepts the match in one transaction, or queues a review per provider.

The providers run as a chain: MusicBrainz, then Discogs, then Deezer when the run asks for it (`--deezer`). Each later stage runs only when the stages before it do not settle the release (`runner::settlement`), and its candidate is checked against every earlier stage's (`matcher::fallback_check`). Deezer is also skipped when it cannot change the outcome (`runner::deezer_skip_reason`). Discogs is constructed only when a stage needs it.

`match --queued` selects its releases through `store::load_queued_releases` (not matched, with a saved review from a provider the run may use) instead of the whole scope.

**Offline matching (`index build`, `match --offline`).** `index/` builds one SQLite index per provider from the official MusicBrainz and Discogs dumps, then serves it through `MetadataProvider`: `runner::run_match_offline` opens an index for MusicBrainz and Discogs when the active policy may attempt them and passes those adapters, with the network Deezer adapter (Deezer has no dump, so `--offline --deezer` is the one offline run that makes requests), into the same `run_with_policy` path, so discovery, scoring, and acceptance are unchanged. The MusicBrainz builder synthesizes the `/ws/2/release` document and runs the existing `musicbrainz::decode_release_document`, which keeps offline results identical to the network adapter by construction. Offline runs match up to `MatchOptions::jobs` releases at once (`--jobs`, default one per core up to 8) with `buffer_unordered` on one task: nearly all the time is SQLite work on sqlx's connection threads. Provider clients are shared through `runner::Clients`, and each `LocalIndex` remembers its last 256 searches.

**Saved reviews (`src/review.rs`).** Saved-review logic for the web review pages: `load` (a release with each provider's saved candidates, marked current, out of date or unreadable), `accept_saved` (accepts the candidate a page showed, against the guard it showed: source revision and accepted match), `add_fetched` (a release fetched by ID becomes the first candidate of its provider's review, through `store::add_review_candidate` in one write transaction), `parse_entered` (an ID or a link) and `Fetcher` (the web server's clients, built on first use: network, or the indexes with `serve --offline`). The `--review` prompt in `runner` shares `parse_entered`, the saved-candidate conversion (`stored_candidate`, `candidate_pairs`) and the candidate text (`edition_details`, `score_parts`), builds its `[i]d` client from the run's own factories, and still accepts through `runner::accept`, not `accept_saved`.

**Review summaries (`src/store/reviews.rs`).** Each `match_reviews` row stores a summary of its first candidate (`top_score`, `top_can_apply`, `top_reason`, `top_blockers`), computed by `store::summarize` (the Accept button's `matcher::mapping_can_apply`) and written by every review writer; `store::fill_review_summaries` fills every row whose `top_can_apply` is NULL (saved before the columns existed, or reset by a migration) on each open, from `db::initialize`. The queue's sort and filters (`store::QueueView`: provider, sort, acceptable) read them through `idx_match_reviews_summary`; `Lead` is the review that places a release in a view.

**Web interface (`serve`).** `web.rs` is an axum server without JavaScript over the same store: Status (`store::library_status`), Tracks (`store::track_page`, the query behind `list` and `search`), Releases (`store::list_local_releases`), Review (`web/review.rs`: the queue from `store::queue_page`, one release from `review::load`) and Folders (`store::list_folder_units`, `store::folder_view`). Its two writes are `POST /folder` (a folder decision) and `POST /review` (accept a saved candidate with `review::accept_saved`, or fetch a release by ID with `review::Fetcher` and store it with `review::add_fetched`). Both need the process's form token, refuse `Sec-Fetch-Site` other than `same-origin`, and are refused (`403`) unless the server is bound to a loopback address; the routes exist on every bind. Pages are built by pure `render_*` functions, and every untrusted value goes through `web::escape_html` (`output::Clean`, then HTML escaping). It binds to loopback by default, and a `guard` middleware rejects foreign `Host` headers (DNS rebinding) and adds the security headers to every response. `layout.rs` holds the plain-text helpers shared by the CLI listings and the pages (`plural`, `Table`, `format_duration`, `track_number`).

Layering rules:
- Adapters (`musicbrainz.rs`, `discogs.rs`, `deezer.rs`) convert service payloads into `matcher::RemoteRelease`; provider types never reach `matcher` or `store`. Typed identities (`ReleaseId`, `Mbid`, `ProviderKind`) live in `providers.rs` and are stored as `(provider, TEXT id)`.
- Matching never overwrites scanned metadata or writes audio files. Accepted metadata is a separate overlay (`release_matches` and `release_track_matches`, at most one per local release across providers). `list` and `search` show the effective value: the overlay where present, scanned tags otherwise. Each local release's `release-input-v2:` source revision decides when a match goes stale.

`README.md` is the detailed behavior spec: thresholds, gates, CLI flag constraints, staleness, and import/watch edge cases. Read the relevant section before changing behavior, and update it with any CLI or behavior change. Design specs and implementation plans are in `docs/superpowers/`.

## Database and migrations

- `src/schema.sql` is the frozen pre-migration baseline, applied with `IF NOT EXISTS` on every connect. Put schema changes in a **new** timestamped file in `migrations/`; never edit an applied migration or `schema.sql`. `sqlx::migrate!` embeds migrations at compile time, and `build.rs` triggers a rebuild when they change.
- Paths are case-insensitive: `COLLATE NOCASE` in the schema, and `store::path_key` / `store::key_within` for comparisons and directory-scope checks in Rust. Keep this in new queries.
- Start write transactions with `store::begin_write` (`BEGIN IMMEDIATE`). Helpers called inside a transaction must take `&mut SqliteConnection`, not the pool: the in-memory test pool (`db::connect_in_memory`) has exactly one connection, so acquiring another hangs.
- Columns added to `match_reviews` sit after the large `candidates_json` in each row: sort, filter and check them through `idx_match_reviews_summary` (it covers `top_can_apply`, `top_score`, `source_revision`); read `top_reason`/`top_blockers` from rows only for one page's releases.
- Import deletes rows only when the walk shows positively that a file is gone. Unreadable paths, and walks that find no audio at all, keep their rows.

## Conventions that are easy to miss

- Web form structs are `#[serde(default)]` as a whole, so any form `POST` that lacks fields or the right token gets the token check's `403`, not axum's `422`. An oversized or non-form body still gets `413`/`415` first, and a repeated field `422`.
- Write to stdout with the `outln!` / `out!` macros, not `println!`, so `radish list | head` exits cleanly. Wrap any path, tag value, or provider response, on stdout or stderr, in `output::Clean` / `output::clean_path` so control and bidirectional characters are escaped.
- In `runner.rs`, a release's progress goes through its `Transcript` with `outln_to!` / `eprintln_to!`, not `outln!` / `eprintln!`: releases matched at once hold their lines and print them together when they finish.
- `.cargo/config.toml` builds the bundled SQLite without `SQLITE_ENABLE_MEMORY_MANAGEMENT` (via `LIBSQLITE3_FLAGS`). That option puts every connection's page cache behind one global mutex, which kept parallel offline matching from scaling past four jobs.
- Reject invalid flag combinations before the database is opened or any client is built: Clap constraints, `parse_direct`, `runner::resolve_policy` and `runner::check_offline` in `main.rs::run`.
- `.env` is read from the current directory only, and only for the keys in `DOTENV_KEYS` (`src/main.rs`); add new config keys there. The `RADISH_*_BASE_URL` overrides are for tests only, accept loopback hosts only (`providers::loopback_override`), and are never read from `.env`.
- A scoped rebuild goes through `grouping::unit_scope` (a disc-subfolder root widens to the folder above it), or it regroups half a unit; scoped loads and listings instead add the merged release at `grouping::disc_parent_of`.
- Decisions are versioned. `matcher::MUSICBRAINZ_ALGORITHM_VERSION` / `DISCOGS_ALGORITHM_VERSION` are stored with accepted matches.
- The review summaries (`match_reviews.top_*`) are computed with `matcher::mapping_can_apply` (and `leading_tracks`) and the score weights. A change to any of these ships a new migration that runs `UPDATE match_reviews SET top_can_apply = NULL`, so the next open re-summarizes every review (about 80 s on a 197k-review library).

## Tests

- Unit tests live in module-local `#[cfg(test)] mod tests` blocks and use `src/test_support.rs` (`ScratchDir`, `write_wav`, `write_tagged_wav`) and `db::connect_in_memory()`.
- Integration tests in `tests/*.rs` run the built binary (`env!("CARGO_BIN_EXE_radish")`). `tests/support/mod.rs` provides a loopback `MockServer`, synthetic WAV writers, a MusicBrainz dump archive writer, and canned provider JSON; more fixtures are in `tests/fixtures/musicbrainz/`. Tests point `RADISH_*_BASE_URL` at the mock and `env_remove` credentials, so no test contacts a real service.
- Commit subjects use `type(scope): summary`, e.g. `fix(watcher): …`, `refactor: …`, `docs: …`.
