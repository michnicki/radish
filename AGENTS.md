# Repository Guidelines

## Project Structure & Module Organization

Radish is a Rust 2024 CLI for music library management. `src/main.rs` defines the Clap commands and output formatting. `src/scanner.rs` reads audio tags using Lofty; `src/watcher.rs` handles filesystem events using Notify. `src/store.rs` contains SQL queries, while `src/db.rs` opens SQLite connections and applies the embedded `src/schema.sql`. Keep changes in the corresponding module. `.github/workflows/` contains CI and platform release workflows. There are currently no dedicated test or asset directories.

## Build, Test, and Development Commands

Use stable Rust and run commands from the repository root:

- `cargo build` — compile the development binary; CI uses `cargo build --verbose`.
- `cargo build --release` — build the optimized binary at `target/release/radish`.
- `cargo run -- --db /tmp/radish-dev.db import /path/to/music` — import a local music directory into a development database.
- `cargo run -- --db /tmp/radish-dev.db list` — inspect imported tracks. Other commands include `search "artist"` and `watch /path/to/music`.
- `cargo test --verbose` — run Rust tests, matching CI.
- `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` — recommended local formatting and lint checks; CI currently runs only build and tests.

## Coding Style & Naming Conventions

Use standard rustfmt formatting with four-space indentation. Name modules, functions, and variables in `snake_case`, types in `PascalCase`, and constants in `SCREAMING_SNAKE_CASE`. Follow existing `anyhow::Result` and `?` error propagation patterns. Bind SQL parameters through SQLx. Preserve case-insensitive path matching in schema and query changes.

## Testing Guidelines

No automated tests or coverage threshold are currently defined. Add focused Rust unit tests in module-local `#[cfg(test)] mod tests` blocks, using `#[test]` or `#[tokio::test]` for asynchronous cases. Name tests after behavior, such as `accepts_uppercase_audio_extensions`. Use temporary SQLite databases and small, redistributable audio fixtures. Cover scanner filtering, database upserts, search, and watcher behavior when changing those areas.

## Commit & Pull Request Guidelines

History currently contains one commit, `Initial implementation of Radish`; no broader message convention is established. Use concise, descriptive subjects. Keep pull requests focused, describe behavior changes, link relevant issues, and report validation commands and results. Include sample CLI output when output changes. Update `README.md` for command or usage changes. Keep generated databases, `target/`, and downloaded music out of commits.
