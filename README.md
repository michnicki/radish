# Radish

A compiled CLI for music library management. Radish reads the tags in your audio files, indexes them in a local SQLite database, and lets you browse, search, and watch your collection. On demand it matches local releases against MusicBrainz, with an optional Discogs fallback, and layers accepted metadata over your scanned tags without touching the audio files.

![Rust](https://img.shields.io/badge/rust-stable-orange)
![License](https://img.shields.io/badge/license-MIT-green)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

## What it does

- **Imports recursively** — discovers audio files in a directory and its subdirectories.
- **Reads tags in parallel** — uses Tokio's blocking thread pool to parse files with [Lofty](https://github.com/Serial-ATA/lofty-rs).
- **Searches locally** — queries stored titles, artists, and albums without rescanning your files.
- **Watches for changes** — uses filesystem events to refresh tags and remove deleted file entries while running.
- **Matches MusicBrainz first (opt-in)** — finds candidate releases, compares complete tracklists, auto-accepts only clearly supported editions, and offers interactive review for ambiguous ones. Discogs runs once as a fallback only when MusicBrainz cannot settle the release and a token is available.
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

## MusicBrainz and Discogs matching

Matching is explicit and never runs during plain `import`, `watch`, `list`, `search`, `releases`, or `match --clear`. Those commands work fully offline and never require a token.

**Default behavior changed:** `radish match` and `radish import PATH --match` used to require Discogs. They now try MusicBrainz first and fall back to Discogs only when the MusicBrainz stage cannot settle the release and a token is available. Use `--provider discogs` for the previous Discogs-only workflow.

### Credentials

MusicBrainz release search and lookup need no account. Discogs fallback and explicit Discogs mode need a personal access token from your Discogs account settings (`Settings → Developers`):

```bash
export RADISH_DISCOGS_TOKEN="your-token"
```

Radish reads the token only when a Discogs stage actually runs. A missing or empty token in default mode is an optional-unavailable condition, not an error: MusicBrainz results are kept and fallback is skipped with a note. Explicit Discogs mode treats it as a credential failure. Invalid configured credentials disable fallback for the rest of the run while MusicBrainz continues. Tokens are redacted from output and errors, and never committed.

### Commands

```bash
# List local releases, their provider-qualified match status, and queued reviews
radish releases
radish releases --path ~/Music/SomeArtist

# Default: MusicBrainz first, optional Discogs fallback
radish match
radish match ~/Music/SomeAlbum
radish match --local-release 12 --dry-run

# One service only: no fallback, and no token is read for MusicBrainz-only mode
radish match --provider musicbrainz
radish match --provider discogs

# Fetch a specific release directly, bypassing search (still reviewed)
radish match --local-release 12 --musicbrainz-release 76df3287-6cda-33eb-8e9a-044b5e15ffdd
radish match --local-release 12 --discogs-release 249504

# Revisit a release interactively, including an already matched one
radish match --review
radish match --review --local-release 12

# Remove the accepted overlay and show scanned metadata again
radish match --clear 12

# Import first, then run the same policy for that scope
radish import ~/Music --match
radish import ~/Music --match --provider musicbrainz
```

An explicit `--provider` or a direct release selector restricts matching to that service and disables fallback. A direct selector requires `--local-release`, and the two direct flags conflict. `--provider` on `import` requires `--match`. `--review` requires an interactive terminal. `--clear` conflicts with every matching selector. Invalid combinations and malformed MBIDs are rejected before the database is opened or a client is created. Matching failures never roll back a successful import, and `--match` exits nonzero after the import is kept.

### Provider order and fallback

For each selected release Radish evaluates MusicBrainz first. If its best candidate passes every automatic gate, Discogs is never initialized and its token is never read. Otherwise Radish tries Discogs once, when a token is available, for reasons such as:

- no candidates, or candidates that do not settle the edition;
- incomplete or unsupported mappings;
- exhausted network, rate-limit, or malformed-response retries.

Each provider keeps its own requests, budgets, caches, scores, and runner-up gaps; they are never merged. A fallback candidate must pass its own gates and must not contradict the local edition or a complete, plausible primary candidate on edition qualifiers, mapped titles (below `0.90`), or known durations (more than three seconds apart). An observed but unresolved MusicBrainz release ID blocks automatic fallback. Both groups are kept for review when identity cannot be established.

Intermediate failures are counted once, from the final release outcome. A successful fallback after a MusicBrainz failure is reported as recovered; an attempted stage that failed with no usable result is a failure, not an unmatched release.

### Identity

Radish keeps four MusicBrainz concepts distinct:

- A **release** identifies an edition and may be accepted.
- A **release group** narrows search but never establishes an edition.
- A **release track** identifies a track on a particular medium and anchors mapping.
- A **recording** can appear many times on one release and only anchors mapping when it is unambiguous on both sides.

Radish reads Lofty's `MusicBrainzReleaseId`, `MusicBrainzReleaseGroupId`, `MusicBrainzTrackId`, and `MusicBrainzRecordingId` values. Valid hyphenated UUIDs are canonicalized to lowercase; nil IDs, compact UUIDs, and malformed values are retained as diagnostics. A release ID supports automatic acceptance only when every file in the release carries the same single valid value. Conflicts block automatic acceptance rather than being dropped. Merged-release redirects keep the canonical ID and the requested alias.

### Candidate discovery

For each local release, MusicBrainz discovery runs in this order:

1. A consistent embedded release ID lookup.
2. Barcode search.
3. Catalog-number search.
4. Title and artist search.
5. Title-only search for compilations or sparse artist tags.

A consistent observed release-group ID may be included as a search constraint. Budgets are four strategies, two pages of 50 results per strategy, and ten logical full-release lookups per release (including the embedded lookup). Discogs keeps its existing bounded strategies. Search relevance is only a hint; Radish ranks by local evidence and decides from full release documents.

Identifier uniqueness is computed from every identifier summary, including ones that were never hydrated. If paging, lookup budgets, or errors leave identity questions unresolved, identifier-based automatic acceptance is blocked. Completing page two clears the earlier truncation indication. A direct selection returns review-only evidence.

### Track mapping

- Text compares after Unicode normalization, case folding, punctuation-to-space conversion, and whitespace collapsing. Diacritics compare equal to their base letter. Stored and displayed strings are never rewritten.
- MusicBrainz credited names and join phrases are preserved. Numeric Discogs artist disambiguators such as `Nirvana (2)` are stripped for Discogs comparison only.
- Edition qualifiers such as `live`, `remix`, `remastered`, `deluxe`, and `instrumental` stay meaningful across titles, formats, and MusicBrainz edition notes.
- Mapping anchors unique release-track IDs first, then unique recording IDs, then unique `(medium, track)` positions. Remaining files are assigned one-to-one by minimum cost over title and duration. Ties are deterministic and marked ambiguous.
- Identifier anchors that contradict tagged positions are reported, not silently preferred. Repeated recording IDs never collapse into one assignment.
- Tracklist headings are ignored. Nested index tracks, pregaps, track zero, data or video content, unsupported media, declared-count mismatches, and incomplete structures block acceptance, including manual acceptance.
- Separate disc directories stay separate local releases. Multi-disc releases whose files share one directory map across media positions.

A mapping must be complete and one-to-one before anything can be applied.

### Score and automatic acceptance

Scores rank candidates; they are not probabilities.

| Component | Weight |
| --- | --- |
| Album title | 25% |
| Album artist | 20% |
| Mapped track titles | 35% |
| Durations | 15% |
| Year | 5% |

Text uses normalized Levenshtein similarity. Durations use `max(0, 1 − difference_seconds / 10)`. A missing component contributes zero rather than being renormalized into false confidence. MusicBrainz uses the concrete release year; the release-group first-release year is not substituted.

A candidate is accepted automatically only when all of these hold:

- Score is at least `0.95`.
- The runner-up within the same provider is at least `0.05` behind, unless a consistent embedded release ID identifies the chosen release.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every mapped title scores at least `0.90`, and every known duration differs by at most three seconds.
- No embedded ID, barcode, catalog number, release group, recording, or edition qualifier contradicts the candidate.
- Edition identity comes from a consistent embedded release ID or a fully resolved barcode/catalog search that uniquely identifies the release. A release group, a recording, or an explicitly entered ID never counts as edition evidence on its own.
- Identifier discovery was not truncated or left unhydrated.

Everything else is queued or reviewed. Identical tracklists across editions are never accepted on advantage alone. In review you may override score or edition uncertainty after seeing the warnings, but never an invalid, incomplete, or unsupported mapping. New decisions store `musicbrainz-match-v1` or `discogs-match-v2`; historical `discogs-match-v1` values are preserved.

### Review

Reviews are queued per provider, MusicBrainz first. Without a terminal, unresolved groups are stored and summary output explains why. With `--review`, the screen shows the policy, each provider's stage notes and errors, release ID and URL, exact credit, edition date/country/labels/catalog numbers, media/format/status/disambiguation, score breakdown, evidence, contradictions, mapping, and the current source when replacing an overlay.

Actions are: accept a candidate, choose another provider group, enter a release ID (with a provider choice in default mode), skip, or quit. Saved queues are reviewed offline without fetching missing provider groups; an entered ID may use an existing lazily-created client. A saved proposal whose revision or accepted-match guard changed can no longer be applied. Acceptance, replacement, and clearing each update the overlay, mappings, status, and reviews in one transaction.

### Overlay, staleness, and clearing

- `list` and `search` use one accepted provider's values where present and fall back to scanned tags otherwise, including for sorting.
- Missing remote values never erase scanned values. File paths, measured durations, and modification times always come from local observations.
- Each local release has at most one accepted overlay across all providers. Accepting a different provider replaces the previous overlay atomically.
- Import and watcher updates refresh observed metadata without overwriting accepted metadata.
- A deterministic `release-input-v2:` revision covers membership, year, barcode, catalog number, and all four identifier lists. The first rebuild after upgrade changes older revisions once and marks existing matches stale while keeping their overlays visible.
- Reimporting unchanged tags keeps a match valid. Changed observations mark it stale with a `[stale match]` marker until it is reviewed or cleared. A stale match is refreshed from its accepted provider; preferring a different source requires review.
- Ordinary runs skip current accepted matches. Clearing restores scanned values and contacts no provider.

### Known limits

- **No fingerprinting or AcoustID.** Matching relies on tags, embedded IDs, barcode, catalog number, titles, artists, years, and durations.
- **Edition twins:** releases with identical tracklists and no distinguishing identifier reach review rather than being accepted.
- **Incomplete editions:** a single disc from a larger set, pregaps, data/video tracks, and unsupported media produce suggestions only.
- **MusicBrainz MBIDs:** only hyphenated non-nil UUIDs are accepted. Nil IDs and compact 32-character forms are rejected everywhere, including stored snapshots.
- **Rate limits and retries:** MusicBrainz attempts share a scheduler with at least 1.1 seconds between starts, including redirect hops. Every attempt has a 30-second timeout and a logical request gets at most three actual attempts. Transport failures, 429, and 5xx retry with 2s/4s backoff and honor both `Retry-After` forms; a suggested wait above 120 seconds is reported instead of slept. Discogs keeps its own existing spacing and retries.
- **Network scope:** successful search and release results are cached per run only. Between runs Radish persists accepted metadata and review candidates, never a general HTTP cache.

### Example output

```text
#12 Test Artist — Blue [2 track(s)] matched
    musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd
    https://musicbrainz.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd

Accepted: 1
Queued for review: 0
Unmatched: 0
Skipped: 0
Failed: 0
```

This is illustrative synthetic output; the UUID does not describe a real release.

### Test-server overrides

Automated tests point both providers at loopback servers:

```bash
RADISH_MUSICBRAINZ_BASE_URL=http://127.0.0.1:PORT
RADISH_DISCOGS_BASE_URL=http://127.0.0.1:PORT
```

Neither variable is set by Radish, and neither is required for normal use.

## Local data model

### Tracks: file paths

A track's identity is its stored file path. The import root is resolved to an absolute, canonical path before scanning. Importing a path already in the database updates its metadata and directory association; importing the same recording at a different path creates another track.

Track and directory paths use SQLite's `NOCASE` collation, which ignores ASCII letter case. For example, paths ending in `Song.FLAC` and `song.flac` match the same database entry. This also means distinct files whose paths differ only in ASCII case share an entry, even on a case-sensitive filesystem. There is no content-based deduplication.

### Local releases

Radish groups files into **local releases**:

- Files group by canonical containing directory plus normalized album title.
- Separate directories stay separate even when their tags are identical, so compilations and duplicate copies do not merge.
- The release artist comes from the album-artist tag; otherwise the common track artist; otherwise `Various Artists`.
- A missing album title still forms a directory-based group, flagged for review.
- Disc subdirectories stay separate in this version. A group containing one disc of a multi-disc release can be suggested but cannot be accepted as a complete release.
- Multi-disc releases whose files share one directory are supported through disc and track numbers.
- Grouping is refreshed by import, watch events, and `match`; it never scans files or reaches the network on its own.

### Legacy albums

Albums are also grouped by the pair `(album title, artist)` for scanned-metadata fallback. The artist comes from the track's **artist** tag. Album titles and artists match exactly there, including case and whitespace. A missing or empty album title leaves the track without a legacy album association.

### Search

`radish search "veil"` searches the effective **track title**, **track artist**, and **album title** (accepted values when present, scanned values otherwise). Internally, the query becomes a bound SQLite `LIKE` pattern, `%veil%`, and a match in any one field includes the track.

- Matching is case-insensitive for ASCII letters: `VEIL` and `veil` return the same matches.
- The query is one pattern; words are not split or matched independently across fields.
- SQL wildcards remain active: `%` matches any sequence of characters and `_` matches one character. They are not escaped as literal characters.
- Filenames and paths are not searched. A filename shown in place of a missing title is only a display fallback.
- Results use album, disc, track number, then title. There is no relevance ranking or fuzzy matching.

## Importing and watching

### Import

`import` walks the selected directory, filters files by extension, and reads each file's primary tag, falling back to its first available tag. It stores title, artist, album, album-artist, year, track number, disc number, barcode, catalog number, all four MusicBrainz identifier values, duration, and modification time. Missing tag fields are stored as SQL `NULL`. Format inference from the audio codec is not performed.

Tag parsing runs in parallel; database updates happen afterward. A file that fails tag reading produces a warning and is skipped while other files continue importing. Every import rereads discovered files; the stored modification time is not used to skip unchanged files.

When stderr is an interactive terminal, import shows a spinner during file discovery, completed/total counters while reading tags and saving tracks, and a spinner while rebuilding local-release groups. Failed tag reads count toward scan progress and still produce warnings. Live progress is hidden when stderr is redirected; stdout messages and the final import summary are unchanged. Progress clears before optional matching starts.

Import adds and refreshes entries and refreshes local-release grouping. It does **not** remove database rows for files that have disappeared.

### Watch

`watch` performs an initial import, then subscribes to recursive filesystem events using [Notify](https://github.com/notify-rs/notify). Handled file-creation, data-change, and rename events trigger tag reads and database updates. File-removal events delete the matching stored path. After each event, local-release grouping is recomputed so accepted matches become stale when matching inputs change. Stop watching with `Ctrl+C`.

The initial import uses the same terminal progress display as `import`. Ongoing watch events continue to print individual sync and removal messages.

Current synchronization limits:

- Changes made while the watcher is stopped are not replayed. Starting it again refreshes existing files through the initial import, but missing files remain in the database.
- A rename can add the new path while leaving the old entry behind unless a separate removal event clears it.
- Deleting a directory does not recursively remove its stored tracks.
- Watch registration happens after the initial scan, so changes during startup can be missed.

## Storage and output

The SQLite database contains these tables:

- `directories` — imported roots.
- `albums` — scanned album metadata.
- `tracks` — file paths, observed tag metadata, observed MusicBrainz identifier JSON, and local-release association.
- `local_releases` — grouping identity, observed release metadata, embedded MusicBrainz evidence and problems, source revision, and status.
- `release_matches` — the single accepted overlay per local release, qualified by `provider` and provider-local TEXT release ID, with snapshot version and normalized metadata snapshot.
- `release_track_matches` — one-to-one file-to-track mapping with remote ordinals, medium/track positions, and MusicBrainz track/recording IDs.
- `match_reviews` — candidate summaries, proposed mappings, and reasons keyed by `(local_release_id, provider)`.

The base schema is embedded in the binary and initialized on connection; additive changes are applied through versioned SQLx migrations in [`migrations/`](migrations). The MusicBrainz migration copies existing Discogs accepted rows, track mappings, timestamps, scores, revisions, and review reasons into the provider-qualified tables before retiring the old ones. Historical snapshots stay version 1 and decode through the legacy Discogs codec. Existing databases keep their rows and IDs, and new observed fields stay absent until the next rescan. See [the schema](src/schema.sql) and [storage queries](src/store.rs) for exact matching rules.

`list` and `search` read the database without checking the audio files. Output includes artist, title, available album, disc and track number, rounded duration in `minutes:seconds`, a `[stale match]` marker when accepted metadata no longer matches the group, and a final track count. A missing title displays the filename; a missing artist displays `(unknown artist)`. An empty result prints `No tracks found.`

`radish releases` prints each local release's ID, artist, title, track count, status (`unmatched`, `matched`, `stale`, plus queued providers), folder, and the accepted provider, release ID, and title when present.

## Development

```bash
cargo build --locked
cargo test --verbose --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
```

CI runs build, test, formatting, and lint checks on stable Rust and never needs provider availability or real credentials. Adapter tests use local mock HTTP servers, and the CLI tests point both base-URL overrides at one. No live smoke test is part of CI.

The CLI is in [`src/main.rs`](src/main.rs), policy orchestration and review in [`src/runner.rs`](src/runner.rs), pure matching in [`src/matcher.rs`](src/matcher.rs), provider identities and the adapter interface in [`src/providers.rs`](src/providers.rs), bounded discovery in [`src/discovery.rs`](src/discovery.rs), the MusicBrainz adapter in [`src/musicbrainz.rs`](src/musicbrainz.rs), the Discogs adapter in [`src/discogs.rs`](src/discogs.rs), tag scanning in [`src/scanner.rs`](src/scanner.rs), storage in [`src/store.rs`](src/store.rs), and filesystem watching in [`src/watcher.rs`](src/watcher.rs).

## License

MIT — see [LICENSE](LICENSE).
