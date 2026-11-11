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
- **Matches MusicBrainz first (opt-in)** — compares complete tracklists and automatically matches strongly supported albums and tracks, even when the exact pressing is unknown. Uncertain results are queued; `--review` opens interactive review. Discogs runs once as a fallback only when MusicBrainz cannot settle the match and a token is available.
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

Already imported your library? Run `radish --db /path/to/radish.db match`. Strong album/track matches no longer need a barcode, catalog number, or embedded release ID. Ordinary runs search previously queued releases again; `--review` still loads unchanged saved candidates offline.

### Provider order and fallback

For each selected release Radish evaluates MusicBrainz first. If its best candidate passes every automatic gate, Discogs is never initialized and its token is never read. Otherwise Radish tries Discogs once, when a token is available, for reasons such as:

- no candidates, or candidates that do not settle the album and tracks;
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

Scores rank candidates; they are not probabilities. Album matching uses the first four components below, divided by their combined weight (`0.95`). The edition year does not affect album confidence. Exact-edition matching uses all five components.

| Component | Weight |
| --- | --- |
| Album title | 25% |
| Album artist | 20% |
| Mapped track titles | 35% |
| Durations | 15% |
| Year | 5% |

Text uses normalized Levenshtein similarity. Durations use `max(0, 1 − difference_seconds / 10)`. A missing component contributes zero rather than being renormalized into false confidence. MusicBrainz uses the concrete release year; the release-group first-release year is not substituted.

A candidate can be accepted automatically as an **album match** when:

- Its album score is at least `0.95`, with album title and artist similarity each at least `0.90`.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every track has a local title and a positive, known duration on both sides. Each title scores at least `0.90`, and durations differ by at most three seconds.
- Observed identifiers and edition qualifiers do not contradict the candidate.
- Every alternative within `0.05` of its album score describes equivalent audio: matching normalized album/artist and mapped track titles/credits, compatible durations, no conflicting known recording IDs or release groups, and a complete supported mapping.

Equivalent editions can share an album match. The chosen release is a deterministic metadata reference; it does not establish a pressing. Output says `album and tracks matched; exact edition unconfirmed`, and SQLite records `match_scope = 'album'`. Barcode, catalog number, country, label, format, edition year, release-track IDs, and edition-specific track positions are left absent from the applied overlay. The full candidate snapshot is retained for inspection.

Radish prefers an **exact-edition match** when all of these hold:

- Score is at least `0.95`.
- The runner-up within the same provider is at least `0.05` behind, unless a consistent embedded release ID identifies the chosen release.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every mapped title scores at least `0.90`, and every known duration differs by at most three seconds.
- No embedded ID, barcode, catalog number, release group, recording, or edition qualifier contradicts the candidate.
- Edition identity comes from a consistent embedded release ID or a fully resolved barcode/catalog search that uniquely identifies the release. A release group, a recording, or an explicitly entered ID never counts as edition evidence on its own.
- Identifier discovery was not truncated or left unhydrated.

Everything else is queued or reviewed. In review you may override score or identity uncertainty after seeing the warnings, but never an invalid, incomplete, or unsupported mapping. New decisions store `musicbrainz-match-v2` or `discogs-match-v3`; historical algorithm versions and accepted matches are preserved.

### Review

Normal matching never prompts, including in an interactive terminal. Unresolved groups are queued per provider, MusicBrainz first, and summary output explains why. With `--review`, the screen shows the policy, each provider's stage notes and errors, release ID and URL, exact credit, edition date/country/labels/catalog numbers, media/format/status/disambiguation, score breakdown, evidence, contradictions, mapping, and the current source when replacing an overlay.

Actions are: accept a candidate, choose another provider group, enter a release ID (with a provider choice in default mode), skip, or quit. Saved queues are reviewed offline without fetching missing provider groups; an entered ID may use an existing lazily-created client. A saved proposal whose revision or accepted-match guard changed can no longer be applied. Acceptance, replacement, and clearing each update the overlay, mappings, status, and reviews in one transaction.

### Optional automatic AI matching with Clef

Use Cloudflare's [Clef](https://developers.cloudflare.com/workers-ai/models/clef/) or [Clef Flash](https://developers.cloudflare.com/workers-ai/models/clef-flash/) to resolve strongly supported album matches that ordinary text comparison leaves uncertain. Configure your Cloudflare account ID and an API token with Workers AI access. Radish loads `.env` from the current directory or its nearest parent; exported environment variables take precedence. For example, put these keys in `target/release/.env` when running the binary from that directory, or export them:

```bash
export RADISH_CLOUDFLARE_ACCOUNT_ID="your-account-id"
export RADISH_CLOUDFLARE_API_TOKEN="your-api-token"
export RADISH_CLOUDFLARE_MODEL="clef" # Optional: clef or clef-flash (default)

radish match --ai-match
radish match --local-release 12 --ai-match --dry-run
radish import ~/Music --match --ai-match

# Explicit human review remains available, including saved reviews offline
radish match --review
```

`--ai-review` remains an alias for `--ai-match`. **Behavior changed:** AI recommendations can now accept a strongly supported album automatically. Ordinary matching never asks for confirmation; uncertain cases stay queued until you explicitly run `--review`. Combining `--review` with `--ai-match` keeps decisions manual. Direct release selectors also remain review-only.

Clef considers up to five candidates from each attempted provider. Requests share repeated metadata and use compact track tables. If the full set is still too large, it selects fewer complete candidates, prioritizing supported mappings and stronger album scores while retaining provider coverage where possible. Output reports when fewer candidates were assessed. It may recommend a provider-qualified release, no match, or equivalent editions. An automatic AI match requires all of these checks:

- At least `0.98` probability for the selected option and `0.95` reported model confidence, using the current album-matching prompt.
- An album score of at least `0.85`, with album title, artist, and every mapped track title each at least `0.50` similar. These lower text floors allow semantic agreement to resolve spelling, numeric-word, and credit-formatting differences; unrelated metadata stays blocked.
- A nonempty local album title and artist, a title and finite positive duration for every track on both sides, complete unambiguous one-to-one mapping, and durations within three seconds.
- No unsupported structures or observed identifier, barcode, catalog-number, recording, release-group, or edition-qualifier conflicts. Every close or higher-scoring alternative must have compatible known audio, including alternatives outside the model's request budget. Existing cross-provider fallback checks still apply.

For close alternatives outside the AI request, matching durations alone are insufficient: every mapped track needs matching normalized titles and credits or a shared recording ID. Track-version qualifiers must agree even when recording IDs match.

Equivalent editions can match automatically when every offered candidate passes the album checks and describes compatible audio. AI establishes an **album match**, never an exact pressing. Edition-specific fields stay absent from the overlay. For example:

```text
  AI advice: clef-flash: recommend musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd (99% model confidence)
  accepted musicbrainz release 76df3287-6cda-33eb-8e9a-044b5e15ffdd (score 0.877, automatic)
    AI-supported album match; decision evidence saved
    album and tracks matched; exact edition unconfirmed

Accepted: 1
Queued for review: 0
```

When an AI recommendation cannot be applied, output and the saved queue explain the failed acceptance checks. Model confidence is an uncalibrated signal, not a measured success rate; the thresholds above are conservative heuristics rather than an accuracy guarantee. Scores retain their original text/duration values. Accepted AI matches store a distinct algorithm version (`musicbrainz-clef-album-v1` or `discogs-clef-album-v1`) and the model, prompt version, choice, probabilities, and confidence in the accepted snapshot. Scanned tags and audio remain unchanged. Use `radish match --clear ID` to remove an accepted overlay.

Unresolved recommendations are saved with the queue. `radish match --review` displays them offline without Cloudflare credentials; saved reviews stay offline even with `--ai-match`. Run normal matching with `--ai-match` to refresh an unresolved assessment. Old advisory prompt results are never reused for automatic acceptance.

AI matching sends album and artist tags, track titles and durations, observed identifiers, candidate metadata and tracklists, and mapping/check diagnostics to Cloudflare. Audio and local file paths are excluded. Each request stays within a conservative 16,000-byte application budget. Complete tracklists and mapping evidence are retained for every assessed candidate; omitted candidates remain subject to ambiguity checks. If even one complete candidate cannot fit, output reports its measured size and AI matching is skipped. Cloudflare usage is billed to your account; see its current model pricing.

Cloudflare is contacted only with `--ai-match` (or its alias), after normal matching cannot settle a release and candidates exist. Settled matches make no AI request. Missing credentials, oversized evidence, timeouts, service errors, invalid responses, and low-confidence answers preserve the normal review queue without opening a prompt. A dry run may incur Cloudflare usage but saves no recommendation or match. Plain import, watch, list, search, releases, and clear remain unchanged.

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
- **Edition twins:** equivalent editions may be accepted as an album match, but their exact pressing stays unconfirmed. Conflicting recordings or tracklists still require review.
- **Incomplete editions:** a single disc from a larger set, pregaps, data/video tracks, and unsupported media produce suggestions only.
- **MusicBrainz MBIDs:** only hyphenated non-nil UUIDs are accepted. Nil IDs and compact 32-character forms are rejected everywhere, including stored snapshots.
- **Rate limits and retries:** MusicBrainz attempts share a scheduler with at least 1.1 seconds between starts, including redirect hops. Every attempt has a 30-second timeout and a logical request gets at most three actual attempts. Transport failures, 429, and 5xx retry with 2s/4s backoff and honor both `Retry-After` forms; a suggested wait above 120 seconds is reported instead of slept. Exhausted transient failures and excessive `Retry-After` suggestions pause uncached MusicBrainz requests for 60 seconds (up to 120 seconds with `Retry-After`), allowing Discogs fallback to proceed without repeated retry loops. Cached successful results remain available, and requests resume after the cooldown. Discogs keeps its own existing spacing and retries.
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

Tag parsing runs in parallel; database updates happen afterward. A file that fails tag reading produces a warning and is skipped while other files continue importing. Files whose modification time matches the stored one are not read again, so a repeat import of an unchanged library reads no tags.

When stderr is an interactive terminal, import shows a spinner during file discovery, completed/total counters while reading tags and saving tracks, and a spinner while rebuilding local-release groups. Failed tag reads count toward scan progress and still produce warnings. Live progress is hidden when stderr is redirected; stdout messages and the final import summary are unchanged. Progress clears before optional matching starts.

Import adds and refreshes entries, removes the rows of files that have disappeared from the imported directory, and refreshes local-release grouping. It prints how many tracks were new or changed, unchanged, and removed, and warns when an accepted match was dropped or became stale.

### Watch

`watch` performs the same incremental import as `import`, then subscribes to recursive filesystem events using [Notify](https://github.com/notify-rs/notify), and starts listening before the initial import so changes made during it are not missed. After the first event it collects events for 500 ms and handles them as one batch. What is on disk decides what happens to each affected path, whatever the event kind: a directory is scanned, an audio file has its tags read, and a missing path has its stored tracks removed (every track under it, for a directory). Renames, folders moved in or out, and the generic event kinds some platforms report are handled the same way. Reads, metadata-only changes, and the database's own `-wal`, `-shm` and `-journal` files are ignored, so a database inside the watched directory is safe. Local-release grouping is recomputed once per batch, and only when rows changed, so accepted matches become stale when matching inputs change. If the backend reports that it lost events, the watcher runs a full sync. If the watched directory itself is removed, the watcher says so and exits. Stop watching with `Ctrl+C`.

The initial import uses the same terminal progress display as `import`. Ongoing watch events print `synced:` and `removed:` lines for real changes only.

Current synchronization limits:

- Changes made while the watcher is stopped are replayed by the initial import, which also removes rows for files that disappeared.
- A change that only touches a file's modification time is not picked up until the next import, because metadata-only events are ignored.

## Storage and output

The SQLite database contains these tables:

- `directories` — imported roots.
- `albums` — scanned album metadata.
- `tracks` — file paths, observed tag metadata, observed MusicBrainz identifier JSON, and local-release association.
- `local_releases` — grouping identity, observed release metadata, embedded MusicBrainz evidence and problems, source revision, and status.
- `release_matches` — the single accepted overlay per local release, qualified by `provider` and provider-local TEXT release ID, with `match_scope` (`album` or `edition`), snapshot version and normalized metadata snapshot. Existing accepted decisions retain edition scope on upgrade.
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
