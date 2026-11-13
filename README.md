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

# Read every file again, even when its modification time is unchanged
radish import --rescan ~/Music

# List stored tracks
radish list

# Search by title, artist, or album
radish search "pierce the veil"

# Import, then watch for file changes until Ctrl+C
radish watch ~/Music

# Browse the library in a web browser (read-only), at http://127.0.0.1:8787
radish serve
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

Radish reads the token only when a Discogs stage actually runs. A missing or empty token in default mode is an optional-unavailable condition, not an error: MusicBrainz results are kept and fallback is skipped with a note. Explicit Discogs mode treats it as a credential failure. Invalid configured credentials, or a token Discogs rejects, disable Discogs for the rest of the run while MusicBrainz continues. In explicit Discogs mode every remaining release then fails with the original error instead of contacting Discogs again. Tokens are redacted from output and errors, and never committed.

#### `.env` files

Radish also reads settings from a file named `.env` in the **current directory only**. Parent directories are never searched, so a `.env` dropped into a downloaded album folder or `/tmp` cannot affect a run elsewhere. Only these keys are read from it; every other key in the file is ignored:

- `RADISH_DISCOGS_TOKEN`
- `RADISH_INDEX_DIR`

Variables already set in the environment always win over the file. Only a regular file named `.env` is read: a missing file, a directory of that name (as in a Python virtualenv), or an unreadable file is ignored without a message. Malformed lines are skipped, and valid keys after them still load. A leading UTF-8 byte order mark is ignored. Service endpoints cannot be set from `.env` (see [Test-server overrides](#test-server-overrides)).

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

# Build local indexes from downloaded dumps, then match with no network
radish index build --musicbrainz ~/dumps/mbdump.tar.bz2 --discogs ~/dumps/discogs_20260101_releases.xml.gz
radish index status
radish match --offline
radish import ~/Music --match --offline --provider musicbrainz
```

An explicit `--provider` or a direct release selector restricts matching to that service and disables fallback. A direct selector requires `--local-release`, and the two direct flags conflict. `--provider` on `import` requires `--match`. `--review` requires an interactive terminal. `--clear` conflicts with every matching selector. Invalid combinations and malformed MBIDs are rejected before the database is opened or a client is created. Matching failures never roll back a successful import, and `--match` exits nonzero after the import is kept.

Already imported your library? Run `radish --db /path/to/radish.db match`. Strong album/track matches no longer need a barcode, catalog number, or embedded release ID. Ordinary runs search previously queued releases again; `--review` still loads unchanged saved candidates offline.

### Local dump indexes (offline matching)

Live matching is paced by the providers, not by Radish: MusicBrainz allows about one request per second and discovery can spend more than ten per release. `--offline` answers the same searches and lookups from indexes built once from the official dumps, which are CC0 and need no account:

| Source | File | Approximate size |
| --- | --- | --- |
| [MusicBrainz core dump](https://musicbrainz.org/doc/MusicBrainz_Database/Download) | `mbdump.tar.bz2` | 7 GB |
| [Discogs releases dump](https://data.discogs.com/) | `discogs_*_releases.xml.gz`, or the tar holding it | 11 GB |

Credits, labels, catalog numbers, barcodes, and tracklists are inline in those two files; the separate artist, label, and master dumps are not needed. Radish never downloads or refreshes a dump.

```bash
# The default directory is ./radish-index; RADISH_INDEX_DIR overrides it
radish index build --musicbrainz ~/dumps/mbdump.tar.bz2
radish index build --discogs ~/dumps/discogs_20260101_releases.xml.gz
radish index status

radish match --offline
radish match --offline --provider musicbrainz
radish match --offline --jobs 16
radish import ~/Music --match --offline
```

A build streams the archive into a temporary database, writes `<name>.db.tmp`, and renames it into place only after the search index is complete, so an interrupted build never damages a working index. When the MusicBrainz `MD5SUMS` file sits next to the dump, the compressed bytes are verified while they are read. `index status` reports each index's size, release count, and checksum state, plus the MusicBrainz export date taken from the archive's own `TIMESTAMP` entry (the file's modification time says when it was downloaded, so it is not used; an archive without one shows no date).

The MusicBrainz archive is a PostgreSQL COPY export with no DDL of its own: it names a table layout with `SCHEMA_SEQUENCE` (currently 31) and expects the server's `CreateTables.sql`. Radish carries that layout for the tables it reads and refuses a dump from a different schema instead of misreading every row, so a schema bump needs a Radish update rather than a silently wrong index.

`--offline` needs an index for every provider the active policy may attempt: the default MusicBrainz-then-Discogs policy needs both, while `--offline --provider musicbrainz` needs only the MusicBrainz index. A missing index fails once, before any release is processed, naming the expected file and the rebuild command. Offline matching reads no credential and makes no HTTP request.

Local title searches mirror the network adapters' two steps: every word must match first, and only if that finds nothing does the search accept any word. The any-word step ignores single-character words (`s`, `1`, a stray initial), which match millions of releases and carry no evidence; the every-word step keeps them. Candidates are ranked by relevance inside the search index and only the returned page is read from the table, which keeps even a common-word search under a second on the full dumps. A title with no searchable characters (`-`, `???`) is a miss, as it is on the network, not a failure. Within one run each index remembers its last 256 searches, so a search repeated by the volumes of a series or by two copies of one album is answered from memory.

Offline matching handles several releases at once: one per core, up to 8, unless `--jobs N` (1 to 16) says otherwise. `--jobs` requires `--offline` and cannot be combined with `--review`; matching against the network is paced by the services and always takes one release at a time. Each release's lines are printed together when it finishes, so releases appear in the order they finish rather than in library order. The number of jobs never changes a decision: every release is matched exactly as it would be on its own.

Measured on one machine with the 2026-11 dumps (release build, local NVMe): the MusicBrainz index holds 5.9 million releases, takes about 2 hours to build (with the Discogs build running at the same time), needs about 1.5 GB of memory, and is 15.8 GiB; the Discogs index holds 19.6 million releases, takes about 1 hour, needs under 0.2 GB of memory, and is 24 GiB. Building also needs scratch space: the MusicBrainz staging database was 19 GB while it was being built. Matching a library of 6,819 local releases with `match --offline` (accepted 2,802, queued for review 3,767, unmatched 250, failed 0) took about 2 minutes with the default 8 jobs, 1.5 minutes with `--jobs 16` and 9.5 minutes one release at a time, on an 8-core, 16-thread laptop with no network and under 0.2 GB of memory; the network adapters are limited to about one MusicBrainz request per second and spend more than ten per release on discovery. With the indexes mostly in the page cache, searching was CPU-bound; a cold cache or a slower disk adds to those times.

Dumps are snapshots: MusicBrainz publishes weekly and Discogs monthly. A release newer than the dump is not found and stays unmatched; match those without `--offline`.

### Provider order and fallback

For each selected release Radish evaluates MusicBrainz first. If its best candidate passes every automatic gate, Discogs is never initialized and its token is never read. Otherwise Radish tries Discogs once, when a token is available, for reasons such as:

- no candidates, or candidates that do not settle the album and tracks;
- incomplete or unsupported mappings;
- exhausted network, rate-limit, or malformed-response retries.

Each provider keeps its own requests, budgets, caches, scores, and runner-up gaps; they are never merged. A fallback candidate must pass its own gates and must not contradict the local edition or a complete, plausible primary candidate on edition qualifiers, mapped titles (below `0.90`), or known durations (more than three seconds apart). An observed but unresolved MusicBrainz release ID blocks automatic fallback. Both groups are kept for review when identity cannot be established.

Intermediate failures are counted once, from the final release outcome. A Discogs fallback after a permanent MusicBrainz failure can be accepted as recovered, but after a transient MusicBrainz failure (network error, rate limit, or cooldown) the fallback candidate is queued for review instead, with a note saying why. This also applies when MusicBrainz returned some candidates but a search or release lookup failed transiently along the way, since the release that failed to load may have been the right one. An attempted stage that failed with no usable result is a failure, not an unmatched release.

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

Text uses normalized Levenshtein similarity. When comparing a local title or artist with a provider's, apostrophes are dropped rather than turned into spaces, so `Don't`, `Don’t`, and an old tag's `Dont` are the same, and `&` reads as `and` (`Seek & Destroy` matches `Seek And Destroy`). A subtitle on only one side costs a little instead of blocking: a trailing `(...)` or `[...]` group, a `feat.`/`ft.` credit, or a `part N` marker is dropped for comparison at a 0.05 cost, so `Tell Me What You Want` matches `Tell Me What You Want (I'll Be Anything)` and `Pinhead [Ramones]` matches `Pinhead`. A suffix that names a version (live, remix, instrumental, radio edit, clean, remastered, and similar) is never dropped, and two different suffixes on both sides are never treated as the same title. Album titles are also compared without a leading `The` or `O.S.T.`/`V.A.` marker and without trailing release-type words (`EP`, `OST`, `Single`, `Import`, `Japanese`, `Limited Edition`, and similar), at a 0.03 cost, so `No Distance Left to Run EP` matches `No Distance Left To Run`; version words such as `Live` or `Remastered` are never dropped. A release-type word with a number on it (`CDS2`), `ST`, `Aust` and `Australian` are dropped the same way, a `Vol.` or `Volume` word in front of a final number is dropped on either side (`Incomplete Crap Vol. 2` matches `Incomplete Crap, Volume 2`), and titles are also compared with their spaces removed (`Rock 'n' Roll` matches `Rocknroll`), each at the same 0.03 cost. Scene tags decorate album titles further, so `Advance`, `Bootleg`, `Reissue`, `Vinyl` and `VLS` are release-type words too, and a run of them at the end of the title is dropped together with record sizes (`7`, `10` or `12` followed by `in`, `inch` or `inches`, as `7inch`, `7 Inch` or `(7-Inch)`, or marked with `"`, `''`, `”` or `″`, as `12''`): `Fools Gold 7"` matches `Fools Gold`, `Hello-CD Single` matches `Hello` and `Craig Olive EP (Vinyl)` matches `Craig Olive`. Decorations joined to the title with `-` or `_`, set off by ` - `, or put in brackets are read the same way (`Clocks-RETAIL CDS`, `Name CDM_CD2`, `Beneath The Surface -ADVANCE-`), while a hyphen inside the title stays (`Jay-Z EP` keeps `Jay-Z`). `Explicit`, `Proper` and `Tape` are dropped only together with a release-type word or a record size (`The Truth (RETAIL EXPLICIT)` matches `The Truth`, but `Album (Explicit)` and `Demo Tape` are left as they are), and in front of that word or size only when they are joined to the text before them by `-`, `_` or an opening bracket, or written in capitals after text that has a lowercase letter (`Democrazy-TAPE ADVANCE` and `Breakin News PROPER RETAIL` match `Democrazy` and `Breakin News`, while `Sex Tape Retail` stays `Sex Tape` and in an all-caps title `PROPER` and `TAPE` stay: `BREAKIN NEWS PROPER RETAIL` reads as `BREAKIN NEWS PROPER`); a title that has such a word is also compared with it kept, and the better reading counts. A title made only of decoration keeps its first word (`Japan EP` matches `Japan`, while `Vinyl` and `12 Inch` are left alone), and a bare `The`, `A` or `An` is never what is left (`The-Bootleg` is not reduced). A bare number such as `Album 7` is not a size, and a quote mark only counts when it comes directly after the number (`7"`, not `"7"`). A version word such as `Live` ends the run (`Album Live Bootleg` keeps `Album Live`), and nothing is dropped when no letter or digit would remain. Two different numbered release-type words in these runs (`CD1` against `CD2`, `Album-CD2` against `Album CD1`) are different discs and are never dropped. Artist names are also compared with a leading `The` dropped and `&` read as `and`, at a 0.02 cost, so `Sisters Of Mercy` matches `The Sisters of Mercy` and `Belle & Sebastian` matches `Belle and Sebastian`. Grouping keys and the search indexes keep the punctuation-to-space rule. Old ID3v1 tags hold at most 30 characters and writers cut longer text to fit, so a local title or artist that is exactly 30 characters is also compared with the first 30 characters of the provider's text, and the better of the two scores counts (`Zion I-Sunshine, Beats, and Rh` matches `Zion I-Sunshine, Beats, and Rhymes`). A 29-character value counts too when the provider's text continues with a space after the same words, since the 30th character of the field was often that space and writers drop a trailing space (`Detailed Instructions For The` matches `Detailed Instructions For The Self Involved`). This reading compares punctuation-free text, so a copy that also lost its punctuation matches (`Schools Out Surfs Up Lets Fall` matches `School's Out, Surf's Up, Let's Fall in Love`); it needs at least 20 characters of text and costs 0.03, so it never scores as an exact match. Only the local side is treated this way, every other gate is unchanged, and a value shorter than 29 characters is never assumed to be cut. Scene tags also leave junk in track titles, so each local track title gets one extra, cleaned reading, in this order: a trailing audio extension is dropped (`02-Cheap_Wine-SYN.mp3`); a trailing release tag is dropped (`temptress luna-amrc`) when at least three titles end in the same 2 to 8 ASCII letters and digits (at least one letter) joined by `-` or `_` with no space before it, compared without case, and no title contradicts it: every title shorter than 29 characters, and every title longer than 30, must end in it, while a 29- or 30-character title (which an ID3v1 field may have cut) may end in it, in the start of it (`...sky-am`), or lack it (a version word or abbreviation such as `live`, `demo`, `inst`, `rmx2`, `v2` or `karaoke`, and a Title Case spelling such as `Inst` or `Hop` on any title, are never read as a tag, since scene group tags are lowercase, uppercase or mixed case); and a leading number is dropped when it equals the track number (`01 - Soul Pilgrim`, `03. Parallax`, `(02) Foo`), or, for an untagged track, its position on its disc (a track with no disc is on disc 1). The number is one to three digits followed by at least one of space, `-`, `.`, `_`, `)`, `]` or `:` and then text, so `1000 Ways`, `1979` and a title that is only `13` are left alone. A cleaned reading costs 0.02, the better of the raw and the cleaned reading counts (`7 Seconds` on track 7 still scores `1.0`), and the cut-off reading above also applies to a title whose only cleanup was its number, judged by the raw tag's width. Stored tags are never rewritten, and version qualifiers still read the raw title. Durations use `max(0, 1 − difference_seconds / 10)`. A missing component contributes zero rather than being renormalized into false confidence. MusicBrainz uses the concrete release year; the release-group first-release year is not substituted.

A candidate can be accepted automatically as an **album match** when:

- Its album score is at least `0.95`, with album title and artist similarity each at least `0.90`. One exception: when the artist is at least `0.90`, every local file maps one to one with every duration within 1.5 seconds, the mean track-title score is at least `0.97`, and the album score computed without the album title (artist, track titles and durations, scaled to 1.0) is at least `0.97`, an album title of at least `0.70` also passes, with a warning (`Exclusive Metal Sampler` against `Exclusive Metal Radio Sampler`). The `0.95` album-score gate is replaced by that `0.97` score in this case, and the release needs at least three local tracks (at least five for a Various Artists release, whose artist score is `1.0` by construction and so is no evidence). The tracks and lengths then carry the identity that the title would have.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every track has a local title and a positive, known duration on both sides. Each title scores at least `0.90`, and durations differ by at most three seconds, except that in an album of at least four tracks up to two tracks may differ by as much as ten seconds while the summed length of all mapped tracks is within 2% of the provider's (a different master or a trailing silence, not a different album). One title may score as low as `0.75` if it is the only one under `0.90` and its duration agrees (`Seek & Destroy` against `Seek And Destroy`): with every other track agreeing, a spelling variant on the one remaining track is the same recording. In an album of at least five tracks, one track of any title also passes when its duration is within 1.5 seconds, because by elimination it is the remaining track and the other files are tagged as that release says. Neither exception applies together with the weak album title exception above: then every track title must score at least `0.90`.
- Observed identifiers and edition qualifiers do not contradict the candidate.
- Every alternative within `0.05` of its album score is either another edition of the same album (the same MusicBrainz release group or Discogs master, however many bonus tracks it adds) or describes equivalent audio: matching normalized album/artist and mapped track titles/credits (a credit may differ when the track's MusicBrainz recording is the same), compatible durations, no conflicting known recording IDs or release groups, and a complete supported mapping. An alternative is also set aside when the candidate is clearly the pressing the files came from and the alternative is not: with at least three local tracks, every mapped duration of the candidate is within three seconds, while the alternative does not map every file or has a mapped duration more than three seconds off. An alternative whose durations also agree keeps blocking, and so does one that only maps ambiguously.

Equivalent editions can share an album match. The chosen release is a deterministic metadata reference; it does not establish a pressing. Output says `album and tracks matched; exact edition unconfirmed`, and SQLite records `match_scope = 'album'`. Barcode, catalog number, country, label, format, edition year, release-track IDs, and edition-specific track positions are left absent from the applied overlay. The full candidate snapshot is retained for inspection.

Radish prefers an **exact-edition match** when all of these hold:

- Score is at least `0.95`.
- The runner-up within the same provider is at least `0.05` behind, unless a consistent embedded release ID identifies the chosen release.
- Every local file maps uniquely, track counts agree, and no unsupported structures exist.
- Every mapped title scores at least `0.90`, and every known duration differs by at most three seconds.
- No embedded ID, barcode, catalog number, release group, recording, or edition qualifier contradicts the candidate.
- Edition identity comes from a consistent embedded release ID or a fully resolved barcode/catalog search that uniquely identifies the release. A release group, a recording, or an explicitly entered ID never counts as edition evidence on its own.
- Identifier discovery was not truncated or left unhydrated.

Everything else is queued or reviewed. In review you may override score or identity uncertainty after seeing the warnings, but never an invalid, incomplete, or unsupported mapping. New decisions store `musicbrainz-match-v19` or `discogs-match-v20`; historical algorithm versions and accepted matches are preserved. Earlier versions could ask Cloudflare's Clef model for advice, and matches accepted that way (stored with a `*-clef-album-v2` algorithm version) remain valid and unchanged.

### Review

Normal matching never prompts, including in an interactive terminal. Unresolved groups are queued per provider, MusicBrainz first, and summary output explains why. With `--review`, the screen shows the policy, each provider's stage notes and errors, release ID and URL, exact credit, edition date/country/labels/catalog numbers, media/format/status/disambiguation, score breakdown, evidence, contradictions, mapping, and the current source when replacing an overlay.

Actions are: accept a candidate, choose another provider group, enter a release ID (with a provider choice in default mode), skip, or quit. Saved queues are reviewed offline without fetching missing provider groups; an entered ID may use an existing lazily-created client. A saved proposal whose revision or accepted-match guard changed can no longer be applied. Acceptance, replacement, and clearing each update the overlay, mappings, status, and reviews in one transaction.

### Overlay, staleness, and clearing

- `list` and `search` use one accepted provider's values where present and fall back to scanned tags otherwise, including for sorting.
- Missing remote values never erase scanned values. File paths, measured durations, and modification times always come from local observations.
- Each local release has at most one accepted overlay across all providers. Accepting a different provider replaces the previous overlay atomically.
- Import and watcher updates refresh observed metadata without overwriting accepted metadata.
- A deterministic `release-input-v2:` revision covers membership, year, barcode, catalog number, and all four identifier lists. The first rebuild after upgrade changes older revisions once and marks existing matches stale while keeping their overlays visible.
- Reimporting unchanged tags keeps a match valid. Changed observations mark it stale (`stale match` in `list` and `search`, `stale` in `releases`) until it is reviewed or cleared. A stale match is refreshed from its accepted provider; preferring a different source requires review. Refreshing a stale Discogs match needs `RADISH_DISCOGS_TOKEN`; without one the release is skipped (not failed) with a note. The same applies after Discogs rejected the token earlier in the run: the remaining stale Discogs matches are skipped rather than failed.
- Ordinary runs skip current accepted matches. Clearing restores scanned values and contacts no provider.

### Known limits

- **Splits, moves and folder renames drop accepted matches.** A local release is identified by its containing folder plus its album title. Splitting a release across new album titles, renaming or moving its folder, or moving its files to another folder therefore creates different releases, so the accepted match is dropped (the import output reports it) and the release must be matched again. Renaming files within the same folder keeps the release; its membership changed, so the match is marked stale.
- **Retagging an album title keeps the match.** Changing the album title of a whole release keeps its accepted match and marks it stale, because the files are the same; review or refresh it.
- **No fingerprinting or AcoustID.** Matching relies on tags, embedded IDs, barcode, catalog number, titles, artists, years, and durations.
- **Edition twins:** equivalent editions may be accepted as an album match, but their exact pressing stays unconfirmed. Conflicting recordings or tracklists still require review.
- **Incomplete editions:** a single disc from a larger set, pregaps, data/video tracks, and unsupported media produce suggestions only.
- **MusicBrainz MBIDs:** only hyphenated non-nil UUIDs are accepted. Nil IDs and compact 32-character forms are rejected everywhere, including stored snapshots.
- **Rate limits and retries:** MusicBrainz attempts share a scheduler with at least 1.1 seconds between starts, including redirect hops. Every attempt has a 30-second timeout and a logical request gets at most three actual attempts. Transport failures, 429, and 5xx retry with 2s/4s backoff and honor both `Retry-After` forms; a suggested wait above 120 seconds is reported instead of slept. Exhausted transient failures and excessive `Retry-After` suggestions pause uncached MusicBrainz requests for 60 seconds (up to 120 seconds with `Retry-After`), allowing Discogs fallback to proceed without repeated retry loops. Cached successful results remain available, and requests resume after the cooldown. Discogs keeps its own existing spacing and retries.
- **Network scope:** successful search and release results are cached per run only. Between runs Radish persists accepted metadata and review candidates, never a general HTTP cache.
- **Offline indexes are snapshots.** `--offline` can only find what the dump contains: releases newer than the dump, and Discogs IDs merged or removed since it, are not found. Rebuild from a newer dump, or match without `--offline` when a release is missing.

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

Automated tests point the providers at loopback servers:

```bash
RADISH_MUSICBRAINZ_BASE_URL=http://127.0.0.1:PORT
RADISH_DISCOGS_BASE_URL=http://127.0.0.1:PORT
```

These overrides exist for tests only. An unset or empty variable means the normal public service. Otherwise each accepts only an `http` or `https` URL on `localhost`, `127.0.0.0/8` or `::1`; any other value is rejected with an error that names the variable but not its value, so a stray setting cannot send your credentials to another host. None of them is read from `.env`, set by Radish, or required for normal use.

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

`import` walks the selected directory, filters files by extension, and reads each file's primary tag, falling back to its first available tag. A title, artist, album, year or track number that tag lacks or leaves blank is taken from the file's ID3v1 tag, if it has one: old taggers often wrote a field only there. An ID3v1 value never replaces one from the primary tag, and holds at most 30 characters. It stores title, artist, album, album-artist, year, track number, disc number, barcode, catalog number, all four MusicBrainz identifier values, duration, and modification time. Missing tag fields are stored as SQL `NULL`. Format inference from the audio codec is not performed.

Tag parsing runs in parallel; database updates happen afterward. A tag with one malformed field (a year written `199x`, `-1999` or as spaces) is read a second time, leniently: the fields that cannot be parsed are dropped, the rest of the tag is kept, and import prints a note naming the file (`watch` reads the same way, without the note). Some taggers from around 2002 wrote ID3v2.2 frames (three-character IDs such as `TP1`) under a header that says ID3v2.3; read as v2.3, such a tag looks encrypted or truncated. When the default read fails and the frames fill the tag exactly as v2.2 frames, the tag is read as ID3v2.2 instead, in full and without a note. A file that fails even then, such as one with an encrypted frame that lacks its length or a truncated file, produces a warning and is skipped while other files continue importing; if it was imported before, its stored row is kept rather than deleted. Files whose modification time matches the stored one are not read again, so a repeat import of an unchanged library reads no tags. `radish import --rescan PATH` skips that check and reads every file, which picks up edits made within the same second or by tools that preserve the modification time.

When stderr is an interactive terminal, import shows a spinner during file discovery, completed/total counters while reading tags and saving tracks, and a spinner while rebuilding local-release groups. Failed tag reads count toward scan progress and still produce warnings. Live progress is hidden when stderr is redirected; stdout messages and the final import summary are unchanged. Progress clears before optional matching starts.

Import adds and refreshes entries, removes the rows of files that have disappeared from the imported directory, and refreshes local-release grouping. Rows are only removed when the walk positively found their files missing: anything at or below a path the walk could not read (a failed read, a permission error, a dangling link) is kept and counted as `kept (unreadable)`. A dangling link to a folder that still has stored rows beneath it also produces one warning, `kept N stored tracks under <path>: the folder is unreachable (broken link?)`; a broken link to a non-audio file (a cover image, say) is ignored silently. If the walk finds no audio files at all while the library still has rows under the directory, nothing is removed, the summary counts them as `kept (no audio files found)`, and a warning asks whether the volume is mounted; this protects against an unmounted share, but it also means the last remaining files of a directory cannot be cleared by `import`, and a share mounted *below* the imported directory can still look like deleted files if it is empty. It prints how many tracks were new or changed, unchanged, and removed, and warns when an accepted match was dropped or became stale.

Symbolic links are followed during the walk, which has consequences:

- A link to a sibling folder inside the library makes the same files reachable at two paths, and they are imported twice (one track row per path).
- A link that points outside the library pulls that whole tree in.
- Link loops are reported as skipped paths rather than followed forever.

File and folder names are untrusted text. Everything Radish prints that contains a path, tag value, or provider response, including warnings and errors, shows control characters and bidirectional overrides as `\u{..}` escapes instead of sending them to your terminal.

### Watch

`watch` performs the same incremental import as `import`, then subscribes to recursive filesystem events using [Notify](https://github.com/notify-rs/notify), and starts listening before the initial import so changes made during it are not missed. After the first event it collects events for 500 ms and handles them as one batch. What is on disk decides what happens to each affected path, whatever the event kind: a directory is scanned, an audio file has its tags read, and a missing path has its stored tracks removed (every track under it, for a directory). Renames, folders moved in or out, and the generic event kinds some platforms report are handled the same way. Reads, metadata-only changes, and the database's own `-wal`, `-shm` and `-journal` files are ignored, so a database inside the watched directory is safe. Local-release grouping is recomputed once per batch, and only when rows changed, so accepted matches become stale when matching inputs change. A path that fails is reported as a warning and the rest of the batch still applies. If the backend reports that it lost events, or a database write failed, the watcher runs a full sync (the same incremental import as at startup) with the next batch. Dropped and newly stale accepted matches are reported after every rebuild. If the watched directory itself is removed, the watcher says so and exits, including when it was about to run a full sync. Stop watching with `Ctrl+C`.

The initial import uses the same terminal progress display as `import`. Ongoing watch events print `synced:` and `removed:` lines for real changes only.

Current synchronization limits:

- Like `import`, a full sync keeps rows it cannot positively show to be gone: unreadable files and folders, and a walk that finds no audio files at all, remove nothing.
- Changes made while the watcher is stopped are replayed by the initial import, which also removes rows for files that disappeared.
- A change that only touches a file's modification time is not picked up until the next import, because metadata-only events are ignored.
- If the database lives inside the watched tree, removing the whole tree with `rm -rf` may not raise a root-deleted event, because SQLite's open files pin the directory. Keep the database outside the watched directory if you need the watcher to notice that.

## Web interface

`radish serve` starts a small read-only web server over the library database and prints the address it listens on (default `http://127.0.0.1:8787`; change it with `--bind ADDR`, for example `--bind 127.0.0.1:0` for any free port). It uses the database given by `--db`, so it can run alongside `import`, `watch` and `match`. Stop it with `Ctrl+C`.

| Page | Shows |
| --- | --- |
| `/` Status | Directory, track and local-release totals, releases by status (`matched`, `unmatched`, `stale`), how many unmatched releases have a queued review, the newest file modification time stored, and each imported directory with its track count. |
| `/tracks` | The `list` columns. `?q=TEXT` searches title, artist and album exactly like `search`, and `?page=N` pages through the results, 100 per page. |
| `/releases` | The `releases` data: ID, artist, album, track count, status, accepted match (linked when the provider URL is `http` or `https`) and folder, 100 per page. |

Status is a snapshot of what the database holds, not a history: Radish does not record when an import ran, so the page cannot say when the last one was.

The interface has no authentication and no way to change anything: every request is a `GET` and no JavaScript is served. Keep these defaults in mind:

- It listens on loopback only. Binding any other address works but prints a warning, because the pages show the library's file paths to anyone who can connect.
- Requests whose `Host` header is not `localhost`, a loopback address, or the bind address get `403`, which stops a web page from reaching the server through DNS rebinding. A wildcard bind (`0.0.0.0`) accepts any host.
- Tags, paths and provider titles are untrusted. Control and bidirectional characters are escaped as in terminal output, then HTML escaping is applied. Every response carries a `Content-Security-Policy` that allows nothing but the page's own inline style.

## Storage and output

The SQLite database contains these tables:

- `directories` — imported roots.
- `albums` — scanned album metadata.
- `tracks` — file paths, observed tag metadata, observed MusicBrainz identifier JSON, and local-release association.
- `local_releases` — grouping identity, observed release metadata, embedded MusicBrainz evidence and problems, source revision, and status.
- `release_matches` — the single accepted overlay per local release, qualified by `provider` and provider-local TEXT release ID, with `match_scope` (`album` or `edition`), snapshot version and normalized metadata snapshot. Existing accepted decisions retain edition scope on upgrade.
- `release_track_matches` — one-to-one file-to-track mapping with remote ordinals, medium/track positions, and MusicBrainz track/recording IDs.
- `match_reviews` — candidate summaries, proposed mappings, and reasons keyed by `(local_release_id, provider)`.

The base schema is embedded in the binary and initialized on connection; additive changes are applied through versioned SQLx migrations in [`migrations/`](migrations). The MusicBrainz migration copies existing Discogs accepted rows, track mappings, timestamps, scores, revisions, and review reasons into the provider-qualified tables before retiring the old ones. Historical snapshots stay version 1 and decode through the legacy Discogs codec. Existing databases keep their rows and IDs. The upgrade itself never reads files, so new observed fields start empty; it resets each track's stored modification time, which makes the next `import` or `watch` read every file once and fill them from the real tags. See [the schema](src/schema.sql) and [storage queries](src/store.rs) for exact matching rules.

`list` and `search` read the database without checking the audio files. Output is a header line and one aligned row per track:

```text
ARTIST            TITLE                ALBUM            #   TIME
Test Artist       Blue Moon            Blue             3   3:45
Test Artist       A Much Longer Title  Live in Tokyo  2-1  12:10  stale match
(unknown artist)  untitled.wav         -                -   ?:??

3 tracks.
```

`#` is the track number, or `disc-track` when the disc number is above 1. `TIME` is the rounded duration in `minutes:seconds`. A trailing `stale match` note appears when accepted metadata no longer matches the group. A missing title displays the filename, a missing artist displays `(unknown artist)`, and a missing album or number displays `-`. Columns are padded to the widest value, measured in terminal columns so wide characters line up. When stdout is a terminal, artist, title and album are cut to 40 columns with a trailing `…`; piped or redirected output is never cut. An empty result prints `No tracks found.`

`radish releases` prints one block per local release, followed by a total:

```text
#12  Test Artist — Blue
     2 tracks · matched
     match:  musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd (Blue)
     path:   /music/Test Artist/Blue

1 local release.
```

The heading has the ID, artist, and title. The next line has the track count and status (`unmatched`, `matched`, `stale`, plus queued providers). The `match:` line shows the accepted provider, the full release ID, and the title, and appears only when a match exists. The `path:` line shows the folder. Releases without an artist or album tag show `(unknown artist)` and `(no album tag)`.

File databases use SQLite's WAL journal mode, so `<db>-wal` and `<db>-shm` files appear next to the database while it is open. They are part of the database: keep them with it when you move or back it up, and do not delete them while Radish is running.

### Upgrade notes

- Index files carry a schema version. Version 2 stores each release document zlib-compressed, so an index built by an earlier version is refused with a rebuild hint; run `radish index build` again.
- A migration marks previously accepted multi-track matches stale when their file order differs from their track order. Older versions could apply remote track metadata to the wrong files in that case. The overlay stays visible; the next `radish match` run re-evaluates those releases.
- The year used to be stored on an album row shared by every folder with the same title and artist, and now lives on each track. The upgrade copies each track's old (possibly wrong) year, then forces the next `import` or `watch` to read every file once, which replaces it with the file's own tag. Releases affected by the shared year may go stale at that first re-read. Review or refresh them as usual.

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
