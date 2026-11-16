# Repository Guidelines

## Project Structure & Module Organization

Radish is a Rust 2024 CLI for music library management. It indexes audio tags into SQLite and, when asked, matches local releases against MusicBrainz, with optional Discogs and Deezer fallbacks. All logic lives in the library crate (`src/lib.rs`); `src/main.rs` holds the Clap commands, `.env` loading, and output formatting. Keep changes in the module that owns the behavior:

- `scanner.rs` reads tags with Lofty. `importer.rs` is the one import pipeline, used by both `import` and the watcher. `watcher.rs` handles Notify events.
- `store.rs` contains all SQL queries. `db.rs` opens connections and applies the baseline `src/schema.sql` and then the SQLx migrations in `migrations/`.
- `providers.rs` defines the `MetadataProvider` trait and typed IDs. `musicbrainz.rs`, `discogs.rs` and `deezer.rs` are the provider adapters. `discovery.rs` runs bounded candidate searches. `matcher.rs` holds pure mapping, scoring, and acceptance logic. `runner.rs` orchestrates matching and the interactive review.
- `index/` builds and reads the local provider indexes used by `--offline`: `copy.rs` reads MusicBrainz dump tables, `musicbrainz.rs` and `discogs.rs` are the builders, `writer.rs` owns the index schema and atomic publish, `local.rs` implements `MetadataProvider` over the index, and `cli.rs` implements `index build` and `index status`.
- `output.rs` handles stdout writes and text escaping; `progress.rs` shows terminal progress.

Integration tests are in `tests/`, with shared helpers in `tests/support/` and provider JSON in `tests/fixtures/`. Design specs and implementation plans are in `docs/superpowers/`. `.github/workflows/` contains CI and the platform release workflow.

## Build, Test, and Development Commands

Use stable Rust and run commands from the repository root. CI runs these four checks, and all must pass:

- `cargo build --verbose --locked`
- `cargo test --verbose --locked` (about 40 seconds)
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`

Other useful commands:

- `cargo test --lib <name>` or `cargo test --test <file> <name>` runs a subset of unit or integration tests.
- `cargo build --release` builds the optimized binary at `target/release/radish`.
- `cargo run -- --db /tmp/radish-dev.db import /path/to/music` imports into a development database. Then run `list`, `search "artist"`, `releases`, `match`, or `watch /path/to/music` with the same `--db`.

## Coding Style & Naming Conventions

Use standard rustfmt formatting with four-space indentation. Name modules, functions, and variables in `snake_case`, types in `PascalCase`, and constants in `SCREAMING_SNAKE_CASE`. Follow existing `anyhow::Result` and `?` error propagation patterns. Bind SQL parameters through SQLx. Preserve case-insensitive path matching (`COLLATE NOCASE`, `store::path_key`) in schema and query changes.

- Put schema changes in a new timestamped file in `migrations/`. Never edit an applied migration or `src/schema.sql`.
- Write stdout through the `outln!` and `out!` macros, not `println!`. Escape paths, tag values, and provider text with `output::Clean` or `output::clean_path`.
- In `runner.rs`, write a release's progress through its `Transcript` (`outln_to!` / `eprintln_to!`), since offline matching runs several releases at once and prints each one's lines together.
- `.cargo/config.toml` builds the bundled SQLite without `SQLITE_ENABLE_MEMORY_MANAGEMENT`; keep it, or parallel offline matching stops scaling.
- Start write transactions with `store::begin_write`. Helpers called inside a transaction take `&mut SqliteConnection`, not the pool.
- Matching must not modify audio files or scanned metadata, and every command except matching must work offline.

## Testing Guidelines

Unit tests go in module-local `#[cfg(test)] mod tests` blocks; CLI integration tests in `tests/*.rs` run the built binary. Use `#[tokio::test]` for asynchronous cases, and name tests after behavior, such as `accepts_uppercase_audio_extensions`.

- Unit tests use `db::connect_in_memory()` and the helpers in `src/test_support.rs`, which provide scratch directories and synthetic WAV files.
- Integration tests use `tests/support/mod.rs`: a loopback mock HTTP server, WAV writers, and canned provider responses. Point the `RADISH_*_BASE_URL` variables at the mock and remove credential variables from the child process.

Tests must not contact real services or need credentials. The only exception is the `#[ignore]`d live Discogs smoke test. When changing scanner filtering, database upserts, release grouping, matching rules, search, or watcher behavior, add tests for the change.

## Commit & Pull Request Guidelines

Commit subjects use `type(scope): summary`, for example `fix(watcher): keep the stored spelling of case-colliding files`, `refactor: …`, `docs: …`, or `build: …`. Keep pull requests focused, describe behavior changes, link relevant issues, and report validation commands and results. Include sample CLI output when output changes. `README.md` is the detailed behavior spec; update it for command, usage, or behavior changes. Keep generated databases (including `-wal` and `-shm` files), `.env` files, `target/`, and downloaded music out of commits.
