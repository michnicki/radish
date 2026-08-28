# Radish

A blazing-fast, compiled CLI for music library management and tagging — a modern alternative to [beets](https://beets.io/).

![Rust](https://img.shields.io/badge/rust-stable-orange)
![License](https://img.shields.io/badge/license-MIT-green)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

## Why

Beets is a great music library tool, but it's Python: slow to scan huge libraries, requires managing a Python environment, and doesn't notice when a file's tags change outside of it. Radish is a single compiled binary that:

- **Scans in parallel** — tag reading is spread across a thread pool, so large libraries import in seconds, not hours.
- **Stays in sync** — `watch` uses OS filesystem events to pick up tag edits and file deletions live, no need to re-run anything.
- **Has no runtime dependencies** — one binary, no interpreter, no package conflicts.
- **Is case-insensitive on paths** — avoids duplicate imports when the same file shows up with different path casing (e.g. from a network drive).

## Install

Download a prebuilt binary from the [Releases](../../releases) page for your platform, or build from source:

```bash
git clone https://github.com/reveler-hub/radish
cd radish
cargo build --release
```

The binary is at `target/release/radish`.

## Usage

```bash
# Scan a directory and add every track to the library
radish import ~/Music

# Watch a directory and keep the library in sync live
radish watch ~/Music

# List everything in the library
radish list

# Search by title, artist, or album
radish search "pierce the veil"
```

All commands accept `--db PATH` to choose the SQLite database file (defaults to `./radish.db`).

## How it works

Radish keeps a SQLite database with three tables: `directories`, `albums`, and `tracks`. `import` walks a directory, reads each audio file's tags with [lofty](https://github.com/Serial-ATA/lofty-rs), and upserts the results. `watch` does an initial import, then listens for filesystem events via [notify](https://github.com/notify-rs/notify) and keeps the database current as files are added, edited, or removed.

Supported formats: mp3, flac, m4a, ogg, opus, wav, aac, wma.

## License

MIT — see [LICENSE](LICENSE).
