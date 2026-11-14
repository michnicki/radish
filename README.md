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

Each provider keeps its own requests, budgets, caches, scores, and runner-up gaps; they are never merged. A fallback candidate must pass its own gates and must not contradict the local edition or a complete, plausible primary candidate (one whose exact-edition score reaches `0.95`, counting durations it never published as agreeing, or whose durations identify the tracks as described under the album match below, so that junk local titles do not hide the right primary) on edition qualifiers (`stereo` is ignored unless the same release also says `mono`; `mono` on one side only is a contradiction, but a label such as `live` on one side only is not when both providers' lengths identify the tracks, see below), mapped titles (below `0.90`), or known durations (more than three seconds apart). Against a primary that is plausible only through the duration rule, titles are not compared, since that rule treats them as carrying no evidence. An observed but unresolved MusicBrainz release ID blocks automatic fallback. Both groups are kept for review when identity cannot be established.

The files settle a title or duration disagreement between a plausible primary and the fallback when they side with the fallback, so a placeholder (`I Sold Myself In / [unknown]`), a typo or an 85 s length in MusicBrainz does not send a release the files and Discogs agree on to review. A title disagreement on local track *k* is settled when the file's title reads `0.90` or better against the fallback's title and below `0.90` against the primary's, the file's length and the fallback's are both known, positive and within three seconds, the two titles carry the same track version qualifiers (`live`, `acoustic`, `demo`, and so on; `remastered` is ignored), and the primary's title does not name another take (a trailing bracket group with `alternate`, `outtake`, `session`, `rehearsal` or `take` and a number that the fallback's title lacks). So `Hollow (Live)` and `Hollow (Alternate Take)` in MusicBrainz against `Hollow` in the files and Discogs stay conflicts. The two titles' version words must also agree. A title's version words are its words that name another recording: `acoustic`, `clean`, `cover`, `demo`, `dub`, `edit`, `explicit`, `extended`, `instrumental`, `live`, `mix`, `mono`, `radio`, `remix`, `reprise`, `unplugged`, `version` and the other words the title reading treats as part of the recording, plus `voice-over`, `en vivo`, `wordless`, `[inst.]`, `karaoke`, `a cappella` and `duet`; `remaster*`, `stereo` and `deluxe` are editions and not counted, `backing` is not listed, and a language word is not a version word (`(Spanish version)` and `(English Version)` both carry just `version`). A title conflict is settled only when the primary's version words are among the fallback's and every word the fallback has and the primary lacks is also in the file's title (raw or cleaned reading). A file that merely lacks a word only the primary has settles nothing, since rips rarely carry the word: `Drive (Haeusermann mix)`, `Threshold (voice-over)`, `My Girl (en vivo)`, `Glassy-Dusty (Wordless)`, `Virus (edit)` and `Song (7" version)` in MusicBrainz stay conflicts against a plain title in the files and Discogs, while `Midnight Cowboy (Album Version)` or `Song [inst.]` in the files and Discogs against a plain MusicBrainz title settles. The reading is the one used for track titles with one change: a subtitle that only one side has is not forgiven (the lenient reading scores `I Sold Myself In` against `I Sold Myself In / [unknown]` at `0.95`, this one below `0.90`), while the cut-off, ID3v1-prefix and cleaned readings (track number, audio extension, shared release tag) are kept. A duration disagreement is settled when the file's length is within three seconds of the fallback's, more than three seconds from the primary's and at least one tolerance closer to the fallback than to the primary (the file's gap to the primary is at least its gap to the fallback plus three seconds, so a file 2.4 s from the fallback and 3.6 s from the primary settles nothing), and the file's title also reads `0.90` or better against the fallback's title (same strict reading). That last condition means a settle never rests on a track the fallback itself matched through its lone-weak-title, near-miss or junk-title exceptions: a file titled `Track 5` does not settle a length conflict on track 5. Each settled disagreement is printed as `fallback conflict settled by the files: …` before the accept, once however many primaries produce it. When the fallback is not accepted automatically (review mode, a direct selection, or a conflict that remains), the settled lines are added to the review notes with the same prefix, so a reviewer still sees them. Nothing is settled when the fallback's album title scores below `0.90`. A label on one side only (`live`, `demo`, `acoustic`, `deluxe` or `remastered` in one provider's edition and not the other's, never `mono`, `instrumental` or `remix`) is not a contradiction when both providers' lengths identify the files' tracks (the same test as the duration rule below: five or more tracks, album and artist titles at `0.90` or better, in order, every length within `1.5` seconds); it is printed as `fallback conflict settled by the files: primary candidates describe {"live"} but the fallback describes {}; both providers' lengths identify the files' tracks, so this is read as a labelling difference`. The files do not decide qualifiers (a silent file title is no evidence of "not live"), and a text-only fallback, which lists no lengths, never identifies by them. Other edition qualifier disagreements, "primary lengths contradict the files" for a text-only fallback, embedded problems, an unresolved release ID, and disagreements where the file agrees with both providers or with the primary only still send the release to review. A primary that is plausible only through the duration rule produces no title conflicts, and its duration conflicts can never be settled (its lengths already sit within `1.5` seconds of the files', so they cannot be more than three seconds from the files' while the fallback's are within three).

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

A self-titled placeholder album tag (`Self Titled`, `Self-Titled`, `S/T`, `ST`, `Same`, `Eponymous`, with or without trailing release-type words such as `EP` or `7inch`; see the score rules below) on a release with a usable artist (not Various Artists, missing or `Unknown Artist`) is searched twice on each provider: first with the artist's name as the album title, which is what the provider usually calls the album, and then with the tag as written, because a title search needs every title word to match and the artist-named one never reaches a release titled `S/T`, `Self Titled` or `Same`. The as-written search is labelled `title as tagged` and is always the last strategy, so a full budget drops it and nothing else: on MusicBrainz that takes a barcode, a catalog number and a compilation or sparse-artist `title only` search besides the two title searches, and on Discogs an identifier and a compilation's `relaxed` search. Existing labels and budgets are unchanged, and a release that is not a placeholder gets exactly the searches it had. Each hit is ranked for the detailed-lookup floor by the better of the two title readings. Without a usable artist the placeholder is searched once, as written, unless the directory name supplies the artist (below). A junk album tag (`Unknown Title`, `Unknown Album`, `.`) is still searched as written, unless the directory name supplies the title (below), but adds nothing to ranking, and a junk artist (`Unknown Artist`) adds nothing to ranking either (its search is unchanged). Various Artists keeps ranking on its credit.

A release with a **missing or junk album title, or a missing or junk artist** takes the missing half from its directory name, when that is a scene release name: `Artist-Album-(TAGS)-YEAR-GROUP`, with `_` for spaces (or `.` when the name has no other word break), as in `Straight_Faced_-_Broken-1996-SDR` or `Rob.zombie-american.made.music.to.strip.by-dvnmp3`. The last path component is read, or the one above a disc folder (`CD2`). The name is split at dashes outside brackets; the last field is the group, and year fields (`1996`, `199x`, `(2001)`) and tag fields (`EP`, `CDM`, `2CD`, `7inch`, `Retail`, `Read_NFO`, `Ltd.Ed.`, a country code such as `SP` or `FR`) are dropped from the end while an artist and an album remain. A trailing bracket group of tag words, country codes or country names on the album (`_(12-Inch)`, `_(German_Import)`), a trailing format word (`_VLS`, `_OST`, `_2cd`, `_Trackfix`, `_Read_NFO`) and a trailing year are dropped too, never the whole title; the year only when the name has no year field of its own. A decade (`198x`, `19xx`, `20xx`) counts as a year field. A title that is left with only release-type words (`EP`, `Split`, `VLS`, `Single`, `CDS`, `Maxi`, `Promo`; not `Demo` or `Live`, which are real titles) is no title: `Fazed-EP-VLS-DE-2000-GRP` gives the artist `Fazed` and no album. A name that marks a split release (a `Split` or `Split EP` field, or a title ending in `Split`) names several artists, so nothing is read from it. `S-T` and `Self-Titled` read as a self-titled album, so it is searched as the artist's name like a tag would be, and a `VA`, `Various` or `OST` first field makes a Various Artists release (title only). The split has to be certain: two fields, a `--` or ` - ` separator after the artist, a Various Artists first field or a self-titled last field decide it by themselves. Otherwise (`Wu-Tang_Clan-Wu-Tang_Forever-1997-KSi_INT`) the artist tag decides, and only when exactly one cut of the name gives an artist that is at least `0.90` similar to it; an unresolved name is not searched, and only one split of a name is ever searched, never several. The searches use the labels and budgets of a tagged release with the same tags (a release with no artist tag also gets MusicBrainz's `title only` search, and a self-titled directory gets the `title as tagged` search), hits are ranked with the same identity, and the run records the note `searched with album 'X' and artist 'Y' read from the directory name` when a title search uses what was read (an artist-only fill with no title and no identifier runs no search and records none). Real tags are never replaced, and nothing from the directory is scored or stored: it only chooses what to search for and rank by, and a candidate is accepted on its tracks alone (see the weak album title rule below).

Identifier uniqueness is computed from every identifier summary, including ones that were never hydrated. If paging, lookup budgets, or errors leave identity questions unresolved, identifier-based automatic acceptance is blocked. Completing page two clears the earlier truncation indication. A direct selection returns review-only evidence.

### Track mapping

- Text compares after Unicode normalization, case folding, punctuation-to-space conversion, and whitespace collapsing. Diacritics compare equal to their base letter. Stored and displayed strings are never rewritten.
- MusicBrainz credited names and join phrases are preserved. Numeric Discogs artist disambiguators such as `Nirvana (2)` are stripped for Discogs comparison only.
- Edition qualifiers such as `live`, `remix`, `remastered`, `deluxe`, and `instrumental` stay meaningful across titles, formats, and MusicBrainz edition notes. `stereo` is the default, so it is ignored when qualifiers are compared, unless the same release also says `mono` (a Discogs format of `Vinyl (LP, Album, Stereo)` agrees with a title that says nothing); `mono` on one side only is a contradiction, including against a release that says both `mono` and `stereo`. A **version label** (`acoustic`, `deluxe`, `demo`, `live`, `remastered` on a release; `acoustic`, `demo`, `live` on a track title) that one side has and the other lacks is a labelling difference rather than a different recording only when the lengths identify the tracks (see the acceptance rule below). One side's labels must be all of the other's plus more: `{}` against `{live}` and `{live}` against `{live, remastered}` count, `{demo}` against `{live}` does not. `instrumental`, `mono` and `remix` never count as labels, and everywhere else a difference in qualifiers still blocks.
- Mapping anchors unique release-track IDs first, then unique recording IDs, then unique `(medium, track)` positions. Remaining files are assigned one-to-one by minimum cost over title and duration. Ties are deterministic and marked ambiguous.
- Identifier anchors that contradict tagged positions are reported, not silently preferred. Repeated recording IDs never collapse into one assignment.
- Tracklist headings are ignored. Nested index tracks, pregaps, track zero, data or video content, unsupported media, declared-count mismatches, and incomplete structures block acceptance, including manual acceptance.
- Separate disc directories stay separate local releases. Multi-disc releases whose files share one directory map across media positions. A folder that holds one disc of a multi-disc release (`.../The_Clash-SANDINISTA-2CD/CD2`) is mapped onto that disc when the whole release does not map completely because the file count differs from the release's playable track count (a structure issue alone keeps the whole-release mapping). The disc number comes from the folder name (its last component is `CD`, `Disc` or `Disk`, any case, an optional space, `_`, `.` or `-`, and one to three digits: `CD2`, `Disc_2`, `Disk 2`; `Discography`, `CD1 Bonus` and `DVD` name no disc), from a disc marker ending the album title (`Album CD2`, `Album (Disc 2)`, `Album - CD2`; not `CDS2`, not `Hitdisc 434D`), or from disc tags that every file carries with the same value (counted only from disc 2 on, since a missing tag already reads as disc 1: a uniform tag of disc 1 neither gives a hint nor conflicts with a folder or title that names a disc, as on per-disc rips; mixed or partly tagged files give no tag hint). The sources that say anything must agree, or there is no disc number. Nothing is stored for it: the path, album tag and disc tags are already part of the source revision. The release must have a medium with that number and name its media (MusicBrainz media, and Discogs releases where every playable track has a medium: a position of the form `N-M` such as `2-1` names it, and a bare number such as `15` takes the medium of the nearest `N-M` entry before it in the tracklist, since some releases leave the `N-` off a few tracks (`1-14`, `15`, `16`, `2-1` is two discs); vinyl sides such as `A1`, positions such as `CD1-1`, and bare positions with no `N-M` entry before them name no medium, so a single CD with plain positions `1` to `10` and one stray `10-68` entry after them is not a set), and then every pass, identifier anchors included, sees that medium's playable tracks only, every file is keyed by its track number on that disc whatever its own disc tag says (if any file lacks a track number, the position pass is skipped and the title and duration pass maps them all), and the count note reads `N local file(s) versus K playable track(s) on disc 2`. An identifier that belongs to another disc of the release is reported as such. Output names the disc as `disc 2 of 2`, where the second number is the release's highest medium number, so numbering with gaps reads `disc 3 of 3`. A mapping that is complete for the whole release wins over a disc mapping; no other medium is tried; and a disc number the release does not have maps the whole release. What blocks a whole-release mapping blocks a disc mapping too, including manual acceptance: index tracks, and for MusicBrainz any video, data or unsupported medium (the release's structure issues), wherever on the release it is. Discogs does not say which medium is video, so a Discogs release whose formats mention `DVD`, `Blu-ray` or `VHS` is never mapped onto a single disc. A match of one disc records which disc it is (`release_matches.medium_position`); sibling discs may accept the same release, and one disc's match is never evidence for another's.

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

Text uses normalized Levenshtein similarity. When comparing a local title or artist with a provider's, apostrophes are dropped rather than turned into spaces, so `Don't`, `Don’t`, and an old tag's `Dont` are the same, and `&` reads as `and` (`Seek & Destroy` matches `Seek And Destroy`). A subtitle on only one side costs a little instead of blocking: a trailing `(...)` or `[...]` group, a `feat.`/`ft.` credit, or a `part N` marker is dropped for comparison at a 0.05 cost, so `Tell Me What You Want` matches `Tell Me What You Want (I'll Be Anything)` and `Pinhead [Ramones]` matches `Pinhead`. A suffix that names a version (live, remix, instrumental, radio edit, clean, remastered, and similar) is never dropped, and two different suffixes on both sides are never treated as the same title. Album titles are also compared without a leading `The` or `O.S.T.`/`V.A.` marker and without trailing release-type words (`EP`, `OST`, `Single`, `Import`, `Japanese`, `Limited Edition`, and similar), at a 0.03 cost, so `No Distance Left to Run EP` matches `No Distance Left To Run`; version words such as `Live` or `Remastered` are never dropped. A release-type word with a number on it (`CDS2`), `ST`, `Aust` and `Australian` are dropped the same way, a `Vol.` or `Volume` word in front of a final number is dropped on either side (`Incomplete Crap Vol. 2` matches `Incomplete Crap, Volume 2`), and titles are also compared with their spaces removed (`Rock 'n' Roll` matches `Rocknroll`), each at the same 0.03 cost. Scene tags decorate album titles further, so `Advance`, `Bootleg`, `Reissue`, `Vinyl` and `VLS` are release-type words too, and a run of them at the end of the title is dropped together with record sizes (`7`, `10` or `12` followed by `in`, `inch` or `inches`, as `7inch`, `7 Inch` or `(7-Inch)`, or marked with `"`, `''`, `”` or `″`, as `12''`): `Fools Gold 7"` matches `Fools Gold`, `Hello-CD Single` matches `Hello` and `Craig Olive EP (Vinyl)` matches `Craig Olive`. Decorations joined to the title with `-` or `_`, set off by ` - `, or put in brackets are read the same way (`Clocks-RETAIL CDS`, `Name CDM_CD2`, `Beneath The Surface -ADVANCE-`), while a hyphen inside the title stays (`Jay-Z EP` keeps `Jay-Z`). `Explicit`, `Proper` and `Tape` are dropped only together with a release-type word or a record size (`The Truth (RETAIL EXPLICIT)` matches `The Truth`, but `Album (Explicit)` and `Demo Tape` are left as they are), and in front of that word or size only when they are joined to the text before them by `-`, `_` or an opening bracket, or written in capitals after text that has a lowercase letter (`Democrazy-TAPE ADVANCE` and `Breakin News PROPER RETAIL` match `Democrazy` and `Breakin News`, while `Sex Tape Retail` stays `Sex Tape` and in an all-caps title `PROPER` and `TAPE` stay: `BREAKIN NEWS PROPER RETAIL` reads as `BREAKIN NEWS PROPER`); a title that has such a word is also compared with it kept, and the better reading counts. A title made only of decoration keeps its first word (`Japan EP` matches `Japan`, while `Vinyl` and `12 Inch` are left alone), and a bare `The`, `A` or `An` is never what is left (`The-Bootleg` is not reduced). A bare number such as `Album 7` is not a size, and a quote mark only counts when it comes directly after the number (`7"`, not `"7"`). A version word such as `Live` ends the run (`Album Live Bootleg` keeps `Album Live`), and nothing is dropped when no letter or digit would remain. Two different numbered release-type words in these runs (`CD1` against `CD2`, `Album-CD2` against `Album CD1`) are different discs and are never dropped. When the files are mapped onto one disc of a release and the album title ends with that disc's marker (`Album Disc 2`, `Album - CD2`, for disc 2), the title is also compared without the marker at the same 0.03 cost, and the shortened title is never read as a cut-off; a marker for another disc is left alone. Artist names are also compared with a leading `The` dropped and `&` read as `and`, at a 0.02 cost, so `Sisters Of Mercy` matches `The Sisters of Mercy` and `Belle & Sebastian` matches `Belle and Sebastian`. Grouping keys and the search indexes keep the punctuation-to-space rule. Old ID3v1 tags hold at most 30 characters and writers cut longer text to fit, so a local title or artist that is exactly 30 characters is also compared with the first 30 characters of the provider's text, and the better of the two scores counts (`Zion I-Sunshine, Beats, and Rh` matches `Zion I-Sunshine, Beats, and Rhymes`). A 29-character value counts too when the provider's text continues with a space after the same words, since the 30th character of the field was often that space and writers drop a trailing space (`Detailed Instructions For The` matches `Detailed Instructions For The Self Involved`). This reading compares punctuation-free text, so a copy that also lost its punctuation matches (`Schools Out Surfs Up Lets Fall` matches `School's Out, Surf's Up, Let's Fall in Love`); it needs at least 20 characters of text and costs 0.03, so it never scores as an exact match. Only the local side is treated this way, every other gate is unchanged, and a value shorter than 29 characters is never assumed to be cut. Scene tags also leave junk in track titles, so each local track title gets one extra, cleaned reading, in this order: a trailing audio extension is dropped (`02-Cheap_Wine-SYN.mp3`); a trailing release tag is dropped (`temptress luna-amrc`) when at least three titles end in the same 2 to 8 ASCII letters and digits (at least one letter) joined by `-` or `_` with no space before it, compared without case, and no title contradicts it: every title shorter than 29 characters, and every title longer than 30, must end in it, while a 29- or 30-character title (which an ID3v1 field may have cut) may end in it, in the start of it (`...sky-am`), or lack it (a version word or abbreviation such as `live`, `demo`, `inst`, `rmx2`, `v2` or `karaoke`, and a Title Case spelling such as `Inst` or `Hop` on any title, are never read as a tag, since scene group tags are lowercase, uppercase or mixed case); and a leading number is dropped when it equals the track number (`01 - Soul Pilgrim`, `03. Parallax`, `(02) Foo`), or, for an untagged track, its position on its disc (a track with no disc is on disc 1). The number is one to three digits followed by at least one of space, `-`, `.`, `_`, `)`, `]` or `:` and then text, so `1000 Ways`, `1979` and a title that is only `13` are left alone. A cleaned reading costs 0.02, the better of the raw and the cleaned reading counts (`7 Seconds` on track 7 still scores `1.0`), and the cut-off reading above also applies to a title whose only cleanup was its number, judged by the raw tag's width. Stored tags are never rewritten, and version qualifiers still read the raw title. Durations use `max(0, 1 − difference_seconds / 10)`. A missing component contributes zero rather than being renormalized into false confidence. MusicBrainz uses the concrete release year; the release-group first-release year is not substituted.

Some tags are not album evidence. A **self-titled placeholder** is an album tag that, after trailing release-type and format words are dropped (keeping at least one word) and spaces removed, reads `selftitled`, `st`, `same` or `eponymous` (`Self Titled`, `S/T`, `Self Titled EP`, `S/T 7inch`, `Same-7 Inch Vinyl`); `Self Titled Demo` is not one, because `demo` is an edition qualifier, and `Untitled` stays a literal title. Its album title score is the best of: the placeholder compared with the provider's title as written (so a real album called `Same` still scores `1.0` against `Same`); the artist's name compared with the provider's title, at the `0.03` release-type cost; `0.97` when the provider's title is one of its credited artists and that credit is the local artist (similarity at least `0.90`; a Discogs `Name (3)` credit counts as `Name`), so `Self Titled` by `Band B` does not read as `Band A` on a release credited to both; `0.97` when the provider's title is itself a placeholder (`S/T`, `Self-Titled`); and `0.80` when the provider's title is `Untitled` or `[untitled]`, which is under the `0.90` title gate and so passes only through the weak album title exception in the bullets below, with every track and length agreeing. There is no artist reading for a Various Artists release or a missing artist, and edition qualifiers still come from the tag as written. A missing or junk album tag says nothing about the edition, so it is not compared with the provider's qualifiers (`Unknown Album` does not block a remastered or live release). The tracks and lengths gate the match exactly as for any other title, so a placeholder never outweighs them. A **junk** album tag (`Unknown Title`, `Unknown Album`, `.`) scores `0` against every provider title, and a junk artist tag (`Unknown Artist`) scores `0` against every credit: neither is evidence for or against an album.

A candidate can be accepted automatically as an **album match** when:

- Its album score is at least `0.95`, with album title and artist similarity each at least `0.90`. One exception: when the artist is at least `0.90`, every local file maps one to one with every duration within 1.5 seconds, the mean track-title score is at least `0.97`, and the album score computed without the album title (artist, track titles and durations, scaled to 1.0) is at least `0.97`, an album title of at least `0.70` also passes, with a warning (`Exclusive Metal Sampler` against `Exclusive Metal Radio Sampler`). The `0.95` album-score gate is replaced by that `0.97` score in this case, and the release needs at least three local tracks (at least five for a Various Artists release, whose artist score is `1.0` by construction and so is no evidence). The tracks and lengths then carry the identity that the title would have. A local release with no usable album title (missing or junk, and not a self-titled placeholder) takes the same rule without the title: there is no title floor and no title condition, every other condition stays, and the warning reads `local release has no usable album title, but every track and duration agrees`. A missing or junk artist tag scores `0` for the artist, so such a release (and a fully untagged one, matched only by the directory name's search) stays in review with the right candidate on top.
- Every local file maps uniquely, track counts agree with the release (or with the one disc matched, see above), and no unsupported structures exist.
- Every track has a local title and a positive, known duration on both sides (the exception for a provider that lists no lengths is the text-only bullet below). Each title scores at least `0.90`, and durations differ by at most three seconds, except that in an album of at least four tracks up to two tracks may differ by as much as ten seconds while the summed length of all mapped tracks is within 2% of the provider's (a different master or a trailing silence, not a different album; one track that is not drift is the odd-length rule below). One title may score as low as `0.75` if it is the only one under `0.90` and its duration agrees (`Seek & Destroy` against `Seek And Destroy`): with every other track agreeing, a spelling variant on the one remaining track is the same recording. In an album of at least five tracks, one track of any title also passes when its duration is within 1.5 seconds, because by elimination it is the remaining track and the other files are tagged as that release says. Neither exception applies together with the weak album title exception above: then every track title must score at least `0.90`.
- Or, when the lengths identify the tracks, any number of track titles may score below `0.90`. This needs at least five local tracks, album title and artist similarity each at least `0.90`, a complete unambiguous mapping with as many local files as playable provider tracks in which local track `k` (by disc, track number, then path) maps to the provider's playable track `k` (so two tracks of similar length cannot swap titles), a local title and a positive known duration on both sides of every pair, every duration within 1.5 seconds, no local title that scores below `0.90` against its mapped track but at least `0.90` against a different playable track of the release (such a title names another track, and the edition orders its tracks differently), and an album score computed without the track titles (album title, artist and durations, scaled to 1.0) of at least `0.95`. The `0.95` album-score gate then does not apply, and the weak titles become one warning (`3 track titles score below 0.90 (lowest 'Focal Point Blur' 0.765) but every duration agrees within 1.5s in track order`). This covers placeholder titles (`Track01`), misspellings, shortened titles and provider titles with an artist prefix (`The Hives - Declare Guerre Nucleaire`). Version qualifiers, identifiers, edition qualifiers and close alternatives still apply (a version label on one side only is the exception, see the next rule), and the rule does not combine with the weak album title exception (which needs a title below `0.90`) or the drifting-duration allowance (which needs a duration beyond three seconds).
- Or, when the lengths identify the tracks as in the rule above and a version label sits on one side only, the label is read as a labelling difference. The rule above must hold unchanged (at least five local tracks, album title and artist at least `0.90`, mapped in local order, every duration within 1.5 seconds, album score without the track titles at least `0.95`), and every mapped pair must either agree outright (title at least `0.90` and equal track qualifiers) or be a label pair: the track qualifier sets differ by `acoustic`, `demo` or `live` on one side only, the side with the extra label really ends in one, that is, a final `(...)` or `[...]` group, or a ` - ` or ` – ` tail, naming an edition qualifier and no alternate take, outtake, rehearsal or session (`Clean Slate (live)`, `Song - Live` and `Song [Demo Version]` lose theirs; `Live Wire`, `Live Forever`, `Song (Live) [Bonus Track]` and a title that ends in a bare `Live` keep theirs, and `Song (Live Rehearsal)` or `Song (Demo, Take 2)` is another take, not a label), the two titles score at least `0.90` once that label is dropped, compared without the forgiveness of a subtitle that only one side has (`Song (Live)` against `Song (Part 2)` or `Song [Bonus Track]` stays in review), and no twin exists, that is, no other local track and no other playable track of the matched disc or release has a title that agrees with this pair's once its label is dropped (a local `Song (Live)` next to `Song` on the release, or `Song` and `Song (Remix)`, stay in review). The labels of every level must also say one thing: the release's edition qualifiers, with the track qualifiers of every label pair added on each side, agree or differ by one side's labels being all of the other's plus more (local tracks `(Demo)` against a release noted `Live` stay in review). A label pair skips the track version qualifier check and the title checks, so a label pair adds no weak-title, near-miss or lone-track warning of its own; one warning counts them (`2 track titles differ from musicbrainz only by a version label (first 'Clean Slate (live)') and every duration agrees within 1.5s in track order; read as a labelling difference`). The same holds for the edition qualifiers of the release (the next rule): when they differ only by `acoustic`, `deluxe`, `demo`, `live` or `remastered` on one side only (`{}` against `{live}`, `{live}` against `{live, remastered}`; `{demo}` against `{live}`, and a difference in `instrumental`, `mono` or `remix`, do not qualify), the blocker becomes the warning `edition qualifiers differ (local {} versus musicbrainz {"live"}) but every title and every duration within 1.5s agree in track order; read as a labelling difference` (with label pairs present, `every duration within 1.5s agrees in track order and the titles agree apart from their version labels`). `match` prints these warnings, which end in `read as a labelling difference`, under the `exact edition unconfirmed` line. An album title that carries the label while the provider's does not still has to score `0.90`, which only a long title can do, and an edition-scope match never reads a label this way.
- Or, when the provider lists no length for some of the tracks, the titles identify them. Many releases have none (most Discogs CDs and CDrs of small labels, and vinyl and tape entries). This needs at least three local tracks (at least five for a Various Artists release), album title and artist similarity each at least `0.90`, a complete unambiguous mapping with as many local files as playable provider tracks in which local track `k` maps to the provider's playable track `k`, every local title present and scoring at least `0.90` against its mapped track (no near-miss or lone-track exception), no local title that scores higher against a different playable track of the release than against its own (so `Part 1`, `Part 2` and `Interlude I`, `Interlude II` cannot pass when the edition orders them differently), a positive known local duration on every track, at least one provider duration unknown, every provider duration that is known within three seconds (no drift allowance; a zero or non-numeric length counts as wrong, not unknown), album titles whose numbers do not contradict (the digit runs of the local and the provider's album title, each read after the release-type decoration is dropped and without a number in front of `CD`, `CDs`, `LP` or `DVD`, are compared as numbers, `02` being `2`, plus roman numerals after `Vol.`, `Volume`, `Part`, `Pt` or `Chapter`, with or without a space after the dot; they contradict only when each title has a number the other lacks, that is, neither sequence is the other with some numbers left out, in order: a number on one side only, such as `Album (1999)` or `Album (2CD)` against `Album`, or a year beside a shared number (`Album Vol. 2 (1999)` against `Album Vol. 2`), never blocks; so `Live in Berlin 1994` never passes for `Live in Berlin 1995`, `Tour Edition 2` for `Tour Edition 3` or `Vol. II` for `Vol. III`, while `Vol. 2` and `Volume 2` agree; a local title of 29 or 30 characters that reads as a cut-off tag of the provider's, see the ID3v1 rule above, may lack numbers the provider's title ends with, as long as the ones it has are the start of the provider's), a local album title that does not name a bootleg in its decoration (`Test Album (Bootleg)` is read as `Test Album` for scoring, which would let the official release stand for it), and an album score computed without the durations (album title, artist and track titles, scaled to 1.0) of at least `0.95`. The `0.95` album-score gate then does not apply, because it would count the missing lengths as zero (a release without any lengths tops out at about `0.84`), and the match carries a warning (`discogs lists no duration for 8 of 8 tracks; track lengths are unconfirmed`) that `match` prints under the `exact edition unconfirmed` line. A missing local title or length still blocks. A release marked unofficial (a Discogs format that says `Unofficial Release`, a MusicBrainz status of `Bootleg`) never passes this way and goes through the normal gates. Neither does a candidate when another release of the same provider agrees with the files in text, in track order, but lists a length more than ten seconds from a local file's: its lengths may be the ones the candidate lacks. A local title that ends in a bracketed group the provider's title lacks, naming an alternate take, outtake, rehearsal or session (`Alternate Take`, `Alt Take`, `Peel Session`, `Take 2`, `Take Two`), is another take than the provider's track and never passes this way, since a one-sided subtitle is otherwise dropped at a small cost and there is no length to tell the recordings apart (an ordinary subtitle such as `(Take Off Your Clothes)` is not affected, and the other rules still strip such a subtitle as before). Version qualifiers, identifiers, edition qualifiers and close alternatives still apply, and the rule applies only to an album-level match of a release that ranks first within its provider. It does not combine with the weak album title exception or the duration rule above, and when every provider length is known it does not apply at all. A fallback candidate accepted under this rule is also checked against every primary candidate that agrees with the files in text: a primary length more than three seconds from a local file's is a conflict (a live or re-recorded album can carry a studio album's name), even when that primary is not otherwise plausible; a fallback accepted at edition scope, or on lengths it lists, is not checked this way. Otherwise only lengths known on both sides are compared, and a primary that lists no lengths counts them as agreeing when judged plausible.
- Or, when every title and every length but one agrees, the odd track does not block. A hidden track, a typo in a vinyl side's length or another mix leaves the album unchanged, and the titles and the other lengths already identify it. This needs at least five local tracks (at least eight for a Various Artists release, whose artist score is `1.0` by construction and whose track artists are not compared, so the artist is no evidence), album title and artist similarity each at least `0.90`, a complete unambiguous mapping with as many local files as playable provider tracks in which local track `k` maps to the provider's playable track `k`, every local title scoring at least `0.90` against its mapped track (no near-miss or lone-track exception) and no local title that scores higher against a different playable track than against its own, a positive known length on both sides of every pair, and exactly one pair more than three seconds apart with every other pair within three seconds. On the odd pair the two titles must also agree strictly, at least `0.90` compared without the forgiveness of a subtitle that only one side has (`Song` against `Song / [unknown]` or `Song (Single)` stays in review), and neither the local title (as written) nor the provider's may name a version (any word of `acoustic`, `clean`, `cover`, `demo`, `deluxe`, `dub`, `edit`, `explicit`, `extended`, `instrumental`, `instrumentals`, `live`, `mix`, `mono`, `radio`, `remaster`, `remastered`, `remix`, `remixes`, `reprise`, `stereo`, `unplugged`, `uncensored`, `censored`, `version`, or any word containing `remaster`: `Hollow (Extended Mix)` stays in review), and neither title, the local one or the provider's, may be another take (a trailing bracket group or ` - ` tail naming an alternate take, outtake, rehearsal, session or `take` and a number: `Hollow (Peel Session)`, whichever side carries it, and even when both do). A gap that the drifting-duration allowance above already covers keeps that allowance and its warning: at most ten seconds with the summed length within 2% of the provider's, on an album of at least four tracks. The album score with the odd pair counted at the tolerance edge must reach `0.95`: the duration component takes the similarity the scorer gives a pair exactly three seconds apart (`0.70`) in place of the odd pair's own, so the odd pair counts neither as perfect nor as left out, and a length hundreds of seconds off is no worse than one just beyond the tolerance. This score replaces the `0.95` album-score gate for this rule (with album title and artist at `0.90` and perfect track titles, one odd length no longer passes: the score is `0.948`). The rule is switched off when another candidate of the same provider that is not in the same MusicBrainz release group or Discogs master (a candidate with no group counts as outside this release group, which is not necessarily another album) maps every file, unambiguously, with every mapped length known and within three seconds: the files are likelier that release than this one with a length that does not fit, and the blocker reads `release {id} outside this release group fits every length`. It does not combine with the duration rule (which needs every length within 1.5 seconds), the text-only rule (which needs an unlisted length), the weak album title exception (which needs a title below `0.90`) or the near-miss and lone-track exceptions (which need a title below `0.90`), and version qualifiers, identifiers, edition qualifiers and the other close alternatives still apply. The accept carries the warning `track 'Iron Garden' is 312.0s longer in the files than the discogs duration; every title and every other length agrees` (`shorter` when the files' track is the shorter one), which `match` prints under the `exact edition unconfirmed` line. An edition-scope match never reads a length this way.
- Observed identifiers and edition qualifiers do not contradict the candidate. `stereo` is ignored when qualifiers are compared, unless the same release also says `mono`: a local title that says nothing agrees with a release whose format says `Stereo`, while `mono` on one side only (against `Stereo`, silence, or a release that says both) is a contradiction. A version label on one side only is not a contradiction when the lengths identify every track (the rule above).
- Every alternative within `0.05` of its album score is either another edition of the same album (the same MusicBrainz release group or Discogs master, however many bonus tracks it adds) or describes equivalent audio: matching normalized album/artist and mapped track titles/credits (a credit may differ when the track's MusicBrainz recording is the same), compatible durations, no conflicting known recording IDs or release groups, and a complete supported mapping. Release qualifiers must agree, except that a release label on one side only (`remastered`, `live`, `demo`, `acoustic`, `deluxe`) still counts as equivalent audio with at least five pairs, every pair within 1.5 seconds and equal normalized titles; a label in a track title never makes two releases equivalent. An alternative is also set aside when the candidate is clearly the pressing the files came from and the alternative is not: with at least three local tracks, every mapped duration of the candidate is within three seconds, while the alternative does not map every file or has a mapped duration more than three seconds off. An alternative whose durations also agree keeps blocking, and so does one that only maps ambiguously.

Equivalent editions can share an album match. The chosen release is a deterministic metadata reference; it does not establish a pressing. Output says `album and tracks matched; exact edition unconfirmed`, and SQLite records `match_scope = 'album'`. The album score stored with an album match that passed on the durations, on the text alone, on a one-sided label, on one odd length, or on the weak album title exception can be below `0.95` (about `0.84` for a release without any lengths), since those rules replace the score gate. Barcode, catalog number, country, label, format, edition year, release-track IDs, and edition-specific track positions are left absent from the applied overlay. A disc matched on its own keeps its disc number, since the folder said which disc it is: `release_matches.medium_position` and the `medium_position` of its track rows record it, `list` shows `2-1`, and the release-wide ordinal still counts from the start of the whole release. Output says `matched disc 2 of 2`, the evidence line reads `the files are disc 2 of 2 of this release (disc number from the folder name)` (or `album title`, `disc tags`), and review shows `mapping: 18/18 files mapped to disc 2 of 2`. The full candidate snapshot is retained for inspection.

Radish prefers an **exact-edition match** when all of these hold:

- Score is at least `0.95`.
- The runner-up within the same provider is at least `0.05` behind, unless a consistent embedded release ID identifies the chosen release.
- Every local file maps uniquely, track counts agree with the release (or with the one disc matched), and no unsupported structures exist.
- Every mapped title scores at least `0.90`, and every known duration differs by at most three seconds.
- No embedded ID, barcode, catalog number, release group, recording, or edition qualifier contradicts the candidate.
- Edition identity comes from a consistent embedded release ID or a fully resolved barcode/catalog search that uniquely identifies the release. A release group, a recording, or an explicitly entered ID never counts as edition evidence on its own.
- Identifier discovery was not truncated or left unhydrated.

Everything else is queued or reviewed. In review you may override score or identity uncertainty after seeing the warnings, but never an invalid, incomplete, or unsupported mapping. New decisions store `musicbrainz-match-v31` or `discogs-match-v32`; historical algorithm versions and accepted matches are preserved. Earlier versions could ask Cloudflare's Clef model for advice, and matches accepted that way (stored with a `*-clef-album-v2` algorithm version) remain valid and unchanged.

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
- **Directory names are read only in the `Artist-Title-…-GROUP` form.** Names without dashes, with fewer than three fields, or with other shapes (`Artist_Title_1999_GRP`, `Test Artist - Blue`) are not read. A name whose split is ambiguous (a dash inside the artist or the album) needs an artist tag to settle it, and without one the release is not searched; a fully untagged release can therefore be found, but is never accepted on its own.
- **No fingerprinting or AcoustID.** Matching relies on tags, embedded IDs, barcode, catalog number, titles, artists, years, and durations.
- **Text-only album matches leave track lengths unconfirmed.** When the provider lists no lengths and the titles alone identify the tracks, the album is accepted with a warning that its lengths are unconfirmed; a different edition with the same titles and no lengths cannot be told apart by duration.
- **Text-only album matches rely on titles alone.** Only the album and track titles identify the release: the numbers in the album titles must not contradict, but a release that is absent from the provider's catalog under its own name can still be confused with a same-titled one when no lengths exist anywhere (a live album that carries the studio album's name, a reissue entered separately).
- **Text-only album matches compare no per-track artists.** Track artists are not compared at all, so a match that relies on the titles alone cannot tell two recordings of the same titles apart; classical recordings of standard movement titles (`Allegro`, `Adagio`) are the realistic trap.
- **A label is read as a labelling difference only when every length agrees.** A release that really is a live or demo recording and a studio release of the same title, artist and lengths are indistinguishable by length; the rule trusts the lengths and ignores the label. A label inside only one side's album title (a local `Album` against `Album (Live)`) is read this way only when the title is long enough that dropping the label still scores `0.90`, which only a long title can do, and `instrumental`, `mono` and `remix` are never labels.
- **One odd length is read as agreeing, whatever its cause.** A hidden track appended to the last file, a typo in the provider's length and a file that really is another mix (one whose title says nothing of it) are indistinguishable by length; the rule trusts the titles and the other lengths, and only a release outside this release group that fits every length, a version word or a take in either title, titles that do not agree strictly on the odd pair, or an album score that falls under `0.95` with the odd pair counted at the tolerance edge, takes it back. A Various Artists release needs eight tracks for it, since its artist is no evidence and its track artists are not compared.
- **Edition twins:** equivalent editions may be accepted as an album match, but their exact pressing stays unconfirmed. Conflicting recordings or tracklists still require review.
- **Incomplete editions:** a single disc from a larger set produces suggestions only when its disc number cannot be read (no `CD2`-style folder name, album title marker or uniform disc tags), differs from the release's numbering, or the release has unsupported media; pregaps, data/video tracks, and unsupported media produce suggestions only.
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
- A missing album title still forms a directory-based group. Its directory name, when it is a scene release name, says what to search for (see Candidate discovery); the name is never stored or scored. Such a release can be accepted automatically only through the weak album title rule (see Score and automatic acceptance): every track and length agreeing, and a real artist tag.
- Disc subdirectories stay separate local releases. Each one can be matched to its own disc of a multi-disc release when its folder name, album title or disc tags give the disc number (see Track mapping); otherwise a group containing one disc of a multi-disc release can be suggested but cannot be accepted as a complete release.
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

Tag parsing runs in parallel; database updates happen afterward. A tag with one malformed field (a year written `199x`, `-1999` or as spaces) is read a second time, leniently: the fields that cannot be parsed are dropped, the rest of the tag is kept, and import prints a note naming the file (`watch` reads the same way, without the note). Some taggers from around 2002 wrote ID3v2.2 frames (three-character IDs such as `TP1`) under a header that says ID3v2.3; read as v2.3, such a tag looks encrypted or truncated. When the default read fails and the frames fill the tag exactly as v2.2 frames, the tag is read as ID3v2.2 instead, in full and without a note. A file that fails even then gets further reads. A file whose first bytes carry the signature of another format than its extension names (`OggS` in an Ogg Vorbis file named `.mp3`) is read as that format; bytes further in that merely look like an MPEG frame header, as the headers of other formats often do by chance, do not count. In an MP3, one tag that cannot be parsed at all, even leniently, makes Lofty give up on the whole file: an APE tag with text that is not UTF-8 (a Latin-1 `è`), or an ID3v2 tag with a frame whose text encoding byte is invalid or an encrypted frame that lacks its length. Such a file is read again with the tag ignored: the APE tag, the ID3v2 tags, or both, whichever first lets the file read. The other tags are kept, an ID3v1 tag fills fields as above, and the duration comes from the audio, which is found by its frame headers even when a corrupt ID3v2 header claims a tag that runs into it; import prints a note naming the file and the tag it dropped (`watch`, again, without the note). An `ID3` right after the ID3v2 tag whose version is not 2, 3 or 4, such as an empty placeholder of `ID3` and zero bytes, holds nothing and is passed over without a note. A file that fails even then, such as one cut off before its audio, produces a warning and is skipped while other files continue importing; if it was imported before, its stored row is kept rather than deleted. Files whose modification time matches the stored one are not read again, so a repeat import of an unchanged library reads no tags. `radish import --rescan PATH` skips that check and reads every file, which picks up edits made within the same second or by tools that preserve the modification time.

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
| `/releases` | The `releases` data: ID, artist, album, track count, status, accepted match (linked when the provider URL is `http` or `https`, with `disc N` after the title when the files are one disc of the release) and folder, 100 per page. |

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
- `release_matches` — the single accepted overlay per local release, qualified by `provider` and provider-local TEXT release ID, with `match_scope` (`album` or `edition`), `medium_position` (the disc of the release that the files are, absent when they match the whole release), snapshot version and normalized metadata snapshot. Existing accepted decisions retain edition scope on upgrade.
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

The heading has the ID, artist, and title. The next line has the track count and status (`unmatched`, `matched`, `stale`, plus queued providers). The `match:` line shows the accepted provider, the full release ID, and the title, and appears only when a match exists. When the files are one disc of the matched release, the line ends `(Sandinista!, disc 2)`. The `path:` line shows the folder. Releases without an artist or album tag show `(unknown artist)` and `(no album tag)`.

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
