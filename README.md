# Radish

A compiled CLI for music library management. Radish reads the tags in your audio files, indexes them in a local SQLite database, and lets you browse, search, and watch your collection. It can also match local releases against Discogs on demand and layer the accepted metadata over your scanned tags without touching the audio files.

![Rust](https://img.shields.io/badge/rust-stable-orange)
![License](https://img.shields.io/badge/license-MIT-green)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

## What it does

- **Imports recursively** — discovers audio files in a directory and its subdirectories.
- **Reads tags in parallel** — uses Tokio's blocking thread pool to parse files with [Lofty](https://github.com/Serial-ATA/lofty-rs).
- **Searches locally** — queries stored titles, artists, and albums without rescanning your files.
- **Watches for changes** — uses filesystem events to refresh tags and remove deleted file entries while running.
- **Matches Discogs releases (opt-in)** — finds candidate releases, compares tracklists, auto-accepts only clearly supported matches, and offers interactive review for ambiguous editions.
- **Stores everything in SQLite** — runs as a compiled executable without a separate database server or Python environment.

Supported file extensions: `mp3`, `flac`, `m4a`, `ogg`, `opus`, `wav`, `aac`, and `wma`. Extension checks ignore case, so `.FLAC` is accepted too. Files must also be readable by Lofty.

Radish does not fingerprint audio, download artwork, write tags back to files, or reorganize your library. Accepted metadata lives only in the SQLite database.

## Install

Download a prebuilt binary from the [Releases](../../releases) page for your platform, or build from source with a stable Rust toolchain supporting the 2024 edition:

```bash
git clone https://github.com/reveler-hub/radish
cd radish
cargo build --release
```

The binary is at `target/release/radish` (`radish.exe` on Windows). Add it to your `PATH`, or run it directly from that location.

## Usage

```bash
# Import a music directory and its subdirectories
radish import ~/Music

# List stored tracks
radish list

# Search by title, artist, or album
radish search "pierce the veil"

# Import, then watch for file changes until Ctrl+C
radish watch ~/Music
```

All commands accept `--db PATH`. It defaults to `./radish.db`, relative to the current working directory, and the database is created automatically. Use the same database path across commands to access the same library:

```bash
radish --db /path/to/library.db import ~/Music
radish --db /path/to/library.db search "pierce the veil"
```

Create the database's parent directory first. Run `radish --help` or `radish match --help` for command help.

## Discogs matching

Matching is explicit and never runs during plain `import`, `watch`, `list`, or `search`. Those commands work fully offline and never require a token.

### Set up a token

Create a personal access token in your Discogs account settings (`Settings → Developers`) and export it:

```bash
export RADISH_DISCOGS_TOKEN="your-token"
```

Radish reads the token only when a network operation is needed, rejects empty values, and redacts it from output and errors. Never commit the token to a file in this repository.

### Commands

```bash
# List local releases, their folders, and match status
radish releases
radish releases --path ~/Music/SomeArtist

# Match every unmatched or stale release in the library
radish match

# Match only one directory tree (path-component boundaries, so
# /music/Blue does not include /music/Bluegrass)
radish match ~/Music/SomeAlbum

# Match one local release and show what would happen without storing it
radish match --local-release 12 --dry-run

# Fetch a specific Discogs release directly, bypassing search
radish match --local-release 12 --discogs-release 249504

# Review candidates interactively, including already matched releases
radish match --review

# Remove an accepted overlay and show scanned metadata again
radish match --clear 12

# Import first, then run the same matching pipeline for that scope
radish import ~/Music --match
```

`--discogs-release` requires `--local-release`. `--review` requires an interactive terminal. `--clear`, `--review`, and a path cannot be combined. Matching failures never roll back a successful import, and `--match` exits nonzero after the import is kept.

### Local releases

Radish groups files into **local releases** that are separate from the legacy `albums` table:

- Files group by canonical containing directory plus normalized album title.
- Separate directories stay separate even when their tags are identical, so compilations and duplicate copies do not merge.
- The release artist comes from the album-artist tag; otherwise the common track artist; otherwise `Various Artists`.
- A missing album title still forms a directory-based group, flagged for review.
- Disc subdirectories stay separate in this version. A group containing one disc of a multi-disc release can be matched for suggestions but cannot be accepted as a complete release.
- Multi-disc releases whose files share one directory are supported through disc and track numbers.
- Grouping is refreshed by import, watch events, and `match`; it never scans files or reaches the network on its own.

### Candidate discovery

For each local release, Radish:

1. Fetches a consistent embedded Discogs release ID first, when the files carry one.
2. Searches concrete releases by barcode and/or catalog number.
3. Searches by album title and album artist.
4. Relaxes the artist constraint for compilations and sparse tags.

Searches are bounded to three strategies, two pages of 50 results per strategy, and ten detailed release fetches per local release. Summaries are ranked before fetching details. When a result set is truncated, Radish reports it, and the identifier search can no longer support automatic acceptance.

Search results are only summaries; a decision always uses a full release fetch. Masters may appear in Discogs data, but accepted identities always reference concrete releases.

### Track mapping

- Text is compared after Unicode normalization, case folding, punctuation-to-space conversion, and whitespace collapsing. Diacritics compare equal to their base letter. Stored and displayed strings are never rewritten.
- Discogs numeric artist disambiguators such as `Nirvana (2)` are stripped for comparison only.
- Edition qualifiers such as `live`, `remix`, `remastered`, `deluxe`, and `instrumental` stay meaningful.
- Tracklist headings are ignored. Nested index tracks are not flattened or dropped: they block acceptance until a complete supported mapping exists.
- Unique disc/track positions are matched first. Remaining files are assigned one-to-one by minimum cost over title and duration, with duration treated as neutral evidence during assignment when unknown. Ties are broken deterministically and marked ambiguous.
- Remote positions such as `A1` or `2-03` are preserved, and track-specific Discogs artists are used when present.

A mapping must be complete and one-to-one before anything can be applied. Missing or extra tracks, duplicate assignments, and unsupported structures keep a candidate a suggestion only.

### Score and automatic acceptance

Scores rank candidates; they are not probabilities.

| Component | Weight |
| --- | --- |
| Album title | 25% |
| Album artist | 20% |
| Mapped track titles | 35% |
| Durations | 15% |
| Year | 5% |

Text uses normalized Levenshtein similarity. Durations use `max(0, 1 − difference_seconds / 10)`. A missing component contributes zero rather than being renormalized into false confidence.

A candidate is accepted automatically only when all of these hold:

- Score is at least `0.95`.
- The runner-up is at least `0.05` behind, unless a consistent embedded release ID identifies the chosen release.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every mapped title scores at least `0.90`, and every known duration differs by at most three seconds.
- No embedded ID, barcode, catalog number, or edition qualifier contradicts the candidate.
- Edition identity is supported by a consistent embedded ID, or by a barcode/catalog number that uniquely distinguishes the candidate among retrieved results.
- Identifier discovery was not truncated.

Everything else is queued or reviewed. Identical tracklists across editions are never accepted on advantage alone. In review you may override score or edition uncertainty after seeing the warnings, but never an invalid or incomplete mapping. The algorithm version is stored with each accepted match.

### Review

On a terminal, ambiguous candidates open a review screen that shows the score and its component breakdown, the Discogs release ID and link, artist/title, year, country, label, catalog number, format, edition evidence and contradictions, the file-to-track mapping, missing/extra tracks, and metadata differences.

Actions are: accept the selected candidate, choose another, enter a release ID, skip, or quit. Without a terminal, ambiguous candidates are stored in the review queue instead.

### Overlay, staleness, and clearing

- `list` and `search` use accepted Discogs values where present and fall back to scanned tags otherwise, including for sorting.
- Missing remote values never erase scanned values. File paths, measured durations, and modification times always come from local observations.
- Import and watcher updates refresh observed metadata without overwriting accepted metadata.
- A deterministic revision covers group membership and matching-relevant fields, so re-reading unchanged tags keeps a match valid.
- Changed membership or matching inputs marks the accepted match stale: its overlay stays visible with a `[stale match]` marker until it is reviewed or cleared.
- Accepting or clearing updates the match, track mappings, review queue, and release status in one transaction. A revision change detected during review aborts the application, and a failed transaction leaves no partial overlay.
- `radish match --clear ID` removes the accepted overlay and restores scanned values.

### Known limits

- **Embedded Discogs IDs:** Lofty 0.25 exposes no tag key for Discogs identifiers, so an embedded release ID is currently always absent. Matching therefore relies on barcode, catalog number, titles, artists, years, and durations. The schema and matcher already support consistent embedded IDs.
- **Edition twins:** releases with identical tracklists and no distinguishing identifier reach review rather than being accepted.
- **Rate limits and retries:** each request attempt times out after 30 seconds, outgoing attempts are spaced at least one second apart, transient transport errors and HTTP 5xx responses retry twice (2s, then 4s), and rate-limit responses wait 60 seconds and retry at most twice. Discogs' `Retry-After` header is not used. Authentication failures stop the run immediately.
- **Network scope:** no general-purpose API cache is kept between runs; only accepted metadata and review candidates are persisted.

## How matching works without Discogs

### Tracks: file paths

A track's identity is its stored file path. The import root is resolved to an absolute, canonical path before scanning. Importing a path already in the database updates its metadata and directory association; importing the same recording at a different path creates another track.

Track and directory paths use SQLite's `NOCASE` collation, which ignores ASCII letter case. For example, paths ending in `Song.FLAC` and `song.flac` match the same database entry. This also means distinct files whose paths differ only in ASCII case share an entry, even on a case-sensitive filesystem. There is no content-based deduplication.

### Albums: title and artist tags

Albums are grouped by the pair `(album title, artist)`. The artist comes from the track's **artist** tag; album-artist tags are read for local releases but do not change the legacy album grouping. For an existing pair, import updates the stored album year.

- Album titles and artists match exactly, including case and whitespace. `Blue` and `blue` form separate albums.
- Tracks with different artists form separate album entries even if their album titles match. This affects compilations.
- A missing or empty album title leaves the track without an album association.
- A missing artist is stored as SQL `NULL`. SQLite permits multiple album rows with the same title and a `NULL` artist, including on repeated imports.

The `albums` table remains the scanned-metadata fallback for `list` and `search` when no Discogs match is accepted.

### Search: partial metadata matches

`radish search "veil"` searches the effective **track title**, **track artist**, and **album title** (accepted Discogs values when present, scanned values otherwise). Internally, the query becomes a bound SQLite `LIKE` pattern, `%veil%`, and a match in any one field includes the track.

- Matching is case-insensitive for ASCII letters: `VEIL` and `veil` return the same matches. General Unicode case folding is not configured.
- The query is one pattern; words are not split or matched independently across fields.
- SQL wildcards remain active: `%` matches any sequence of characters and `_` matches one character. They are not escaped as literal characters.
- Filenames and paths are not searched. A filename shown in place of a missing title is only a display fallback.
- Results use the same ordering as `list`: album title, track number, then track title. There is no relevance ranking or fuzzy matching.

## Importing and watching

### Import

`import` walks the selected directory, filters files by extension, and reads each file's primary tag, falling back to its first available tag. It stores title, artist, album, album-artist, year, track number, disc number, barcode, catalog number, duration, and modification time. Missing tag fields are stored as SQL `NULL`. Format inference from the audio codec is not performed.

Tag parsing runs in parallel; database updates happen afterward. A file that fails tag reading produces a warning and is skipped while other files continue importing. Every import rereads discovered files; the stored modification time is not used to skip unchanged files.

Import adds and refreshes entries and refreshes local-release grouping. It does **not** remove database rows for files that have disappeared.

### Watch

`watch` performs an initial import, then subscribes to recursive filesystem events using [Notify](https://github.com/notify-rs/notify). Handled file-creation, data-change, and rename events trigger tag reads and database updates. File-removal events delete the matching stored path. After each event, local-release grouping is recomputed so accepted matches become stale when membership changes. Stop watching with `Ctrl+C`.

Current synchronization limits:

- Changes made while the watcher is stopped are not replayed. Starting it again refreshes existing files through the initial import, but missing files remain in the database.
- A rename can add the new path while leaving the old entry behind unless a separate removal event clears it.
- Deleting a directory does not recursively remove its stored tracks.
- Watch registration happens after the initial scan, so changes during startup can be missed.

## Storage and output

The SQLite database contains these tables:

- `directories` — imported roots.
- `albums` — scanned album metadata.
- `tracks` — file paths, observed tag metadata, and local-release association.
- `local_releases` — grouping identity, observed release metadata, source revision, and status.
- `discogs_matches` — one accepted match per local release with a normalized metadata snapshot.
- `discogs_track_matches` — the file-to-track mapping with Discogs positions and accepted track metadata.
- `match_reviews` — queued candidate summaries, proposed mappings, and reasons.

The base schema is embedded in the binary and initialized on connection; additive changes are applied through versioned SQLx migrations in [`migrations/`](migrations). Existing databases keep their rows and IDs, and new observed fields stay absent until the next rescan. See [the schema](src/schema.sql) and [storage queries](src/store.rs) for exact matching rules.

`list` and `search` read the database without checking the audio files. Output includes artist, title, available album and track number, rounded duration in `minutes:seconds`, a `[stale match]` marker when accepted metadata no longer matches the group, and a final track count. A missing title displays the filename; a missing artist displays `(unknown artist)`. An empty result prints `No tracks found.`

`radish releases` prints each local release's ID, artist, title, track count, status (`unmatched`, `matched`, `stale`, plus `queued`), folder, and the accepted Discogs release when present.

## Development

```bash
cargo build --locked
cargo test --verbose --locked
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

CI runs the same build, test, formatting, and lint checks on stable Rust and never needs Discogs availability or real credentials. Adapter tests use a local mock HTTP server, and the CLI tests point `RADISH_DISCOGS_BASE_URL` at one. A live smoke test is intentionally not part of CI.

The CLI is in [`src/main.rs`](src/main.rs), matching orchestration and review in [`src/runner.rs`](src/runner.rs), pure matching in [`src/matcher.rs`](src/matcher.rs), the Discogs adapter in [`src/discogs.rs`](src/discogs.rs), tag scanning in [`src/scanner.rs`](src/scanner.rs), storage in [`src/store.rs`](src/store.rs), and filesystem watching in [`src/watcher.rs`](src/watcher.rs).

## License

MIT — see [LICENSE](LICENSE).
