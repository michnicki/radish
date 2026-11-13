//! Pure matching logic: normalization, track assignment, scoring, and
//! automatic-acceptance gates.
//!
//! Nothing here touches the network or the database. Adapters convert their
//! service responses into [`RemoteRelease`] values, storage loads
//! [`LocalRelease`] values, and this module decides what may be applied.
//! Provider SDK types never reach this module.

use crate::providers::{GroupId, Mbid, ProviderKind, ReleaseId};
use crate::scanner::AUDIO_EXTENSIONS;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const DISCOGS_ALGORITHM_VERSION: &str = "discogs-match-v20";
pub const MUSICBRAINZ_ALGORITHM_VERSION: &str = "musicbrainz-match-v19";

pub const SCORE_THRESHOLD: f64 = 0.95;
pub const RUNNER_UP_GAP: f64 = 0.05;
pub const TITLE_SIMILARITY_THRESHOLD: f64 = 0.90;
pub const DURATION_TOLERANCE_SECS: f64 = 3.0;

/// The album-title rule: an album title between this floor and
/// [`TITLE_SIMILARITY_THRESHOLD`] may pass when everything else agrees tightly.
/// The rest of the album, scored without the title, must reach
/// `CORROBORATED_SCORE`, every mapped duration must be within
/// `CORROBORATED_DURATION_SECS`, and the mean track-title score must reach
/// `CORROBORATED_MEAN_TITLE`.
pub const CORROBORATED_TITLE_FLOOR: f64 = 0.70;
pub const CORROBORATED_SCORE: f64 = 0.97;
pub const CORROBORATED_DURATION_SECS: f64 = 1.5;
pub const CORROBORATED_MEAN_TITLE: f64 = 0.97;

/// The fewest local tracks the album-title rule accepts. Fewer tracks are too
/// little evidence to outweigh a title that does not agree.
pub const CORROBORATED_MIN_TRACKS: usize = 3;

/// The fewest local tracks the album-title rule accepts for a Various Artists
/// release. Its artist component is 1.0 by construction and so is no evidence,
/// which the track count has to make up for.
pub const CORROBORATED_MIN_VARIOUS_TRACKS: usize = 5;

/// The lowest title score a single track may have, with a matching duration,
/// and still not block an album-level match.
pub const NEAR_MISS_TITLE_FLOOR: f64 = 0.75;

/// The tightest duration agreement that lets a single track of *any* title
/// stand on its length alone, and the fewest tracks that may carry the album
/// when it does.
pub const LONE_TRACK_DURATION_GAP_SECS: f64 = 1.5;
pub const LONE_TRACK_MIN_ALBUM_TRACKS: usize = 5;

/// How far a few tracks may drift from the provider's durations, as a different
/// master or a trailing silence does, while the album still agrees as a whole:
/// at most this many tracks, each within this many seconds, and the summed
/// length within this fraction. Needs at least this many tracks in the album.
pub const SOFT_DURATION_MAX_TRACKS: usize = 2;
pub const SOFT_DURATION_MAX_SECS: f64 = 10.0;
pub const SOFT_DURATION_TOTAL_RATIO: f64 = 0.02;
pub const SOFT_DURATION_MIN_ALBUM_TRACKS: usize = 4;

/// The fewest local tracks that let a candidate's agreeing durations set a
/// close alternative aside: with fewer, a matching length is too common.
pub const DURATION_FIT_MIN_TRACKS: usize = 3;

/// A primary candidate this close to the fallback is plausible evidence that
/// both services describe the same edition, so discrepancies matter.
pub const PLAUSIBLE_PRIMARY_SCORE: f64 = 0.95;

pub const WEIGHT_ALBUM_TITLE: f64 = 0.25;
pub const WEIGHT_ALBUM_ARTIST: f64 = 0.20;
pub const WEIGHT_TRACK_TITLES: f64 = 0.35;
pub const WEIGHT_DURATIONS: f64 = 0.15;
pub const WEIGHT_YEAR: f64 = 0.05;

/// Cost difference below which two track pairings count as a tie.
const TIE_EPSILON: f64 = 1e-9;
/// Similarity assigned to a duration comparison used only for assignment cost,
/// where an unknown duration is neutral evidence rather than a penalty.
const NEUTRAL_DURATION_SIMILARITY: f64 = 0.5;

/// Bumped whenever normalization, mapping, scoring, or gates change, so stored
/// decisions stay explainable. Historical `discogs-match-v1` rows keep their
/// recorded value.
pub fn algorithm_version(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Discogs => DISCOGS_ALGORITHM_VERSION,
        ProviderKind::MusicBrainz => MUSICBRAINZ_ALGORITHM_VERSION,
    }
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtistKind {
    /// Taken from album-artist tags.
    AlbumArtist,
    /// All tracks share one artist and no album-artist tag exists.
    CommonArtist,
    /// Track artists differ, or a compilation was detected.
    VariousArtists,
    /// Nothing usable was tagged.
    Unknown,
}

impl ArtistKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlbumArtist => "album_artist",
            Self::CommonArtist => "common_artist",
            Self::VariousArtists => "various_artists",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "album_artist" => Self::AlbumArtist,
            "common_artist" => Self::CommonArtist,
            "various_artists" => Self::VariousArtists,
            _ => Self::Unknown,
        }
    }
}

/// MusicBrainz evidence embedded in the local files of one release.
///
/// A release ID present on every file identifies an edition. A group ID may
/// narrow search but never establishes an edition. Problems retain malformed or
/// conflicting observations for review instead of silently dropping them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbeddedReleaseEvidence {
    pub release_id: Option<Mbid>,
    pub release_group_id: Option<Mbid>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LocalTrack {
    pub id: i64,
    pub path: String,
    pub title: Option<String>,
    pub disc: Option<i64>,
    pub track_number: Option<i64>,
    pub duration_secs: Option<f64>,
    /// Set only when this file has exactly one valid observed value.
    pub release_track_id: Option<Mbid>,
    pub recording_id: Option<Mbid>,
    /// Malformed or conflicting observed identifiers for this file.
    pub identifier_problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LocalRelease {
    pub id: i64,
    pub directory: String,
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub artist_kind: ArtistKind,
    pub year: Option<i64>,
    pub barcode: Option<String>,
    pub catalog_number: Option<String>,
    /// Embedded Discogs release ID, present only when every file agrees on one.
    pub embedded_discogs_id: Option<u64>,
    pub embedded_evidence: EmbeddedReleaseEvidence,
    pub is_compilation: bool,
    pub has_album_tag: bool,
    pub revision: String,
    /// In the order of `store::order_release_tracks`. `Mapping::pairs` is
    /// indexed by position in this list.
    pub tracks: Vec<LocalTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackEntryType {
    Track,
    Heading,
    Index,
    Other,
}

impl TrackEntryType {
    /// Discogs uses `track` for playable entries; everything else is structure.
    pub fn from_discogs(raw: Option<&str>) -> Self {
        match raw {
            Some("heading") => Self::Heading,
            Some("index") => Self::Index,
            Some("track") | None => Self::Track,
            Some(_) => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteTrack {
    pub position: Option<String>,
    pub title: String,
    pub artists: Vec<String>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    pub entry_type: TrackEntryType,
    #[serde(default)]
    pub medium_position: Option<u32>,
    #[serde(default)]
    pub track_position: Option<u32>,
    #[serde(default)]
    pub release_track_id: Option<Mbid>,
    #[serde(default)]
    pub recording_id: Option<Mbid>,
    #[serde(default)]
    pub artist_credit: Option<String>,
}

impl RemoteTrack {
    pub fn is_playable(&self) -> bool {
        self.entry_type == TrackEntryType::Track
    }

    /// Prefers the exact service credit text, which keeps join phrases intact.
    pub fn artist_display(&self) -> String {
        if let Some(credit) = self.artist_credit.as_deref()
            && !credit.trim().is_empty()
        {
            return credit.to_string();
        }
        self.artists.join(", ")
    }

    /// Explicit medium/track position, then a Discogs-style position string.
    fn position_key(&self) -> Option<(u32, u32)> {
        if let (Some(medium), Some(position)) = (self.medium_position, self.track_position) {
            return Some((medium, position));
        }
        let raw = self.position.as_deref()?;
        let parsed = parse_position(raw);
        if parsed.side.is_some() {
            return None;
        }
        parsed
            .track
            .map(|number| (parsed.disc.unwrap_or(1), number))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRelease {
    pub id: ReleaseId,
    pub group_id: Option<GroupId>,
    pub title: String,
    pub artists: Vec<String>,
    pub year: Option<i64>,
    pub country: Option<String>,
    pub labels: Vec<String>,
    pub catalog_numbers: Vec<String>,
    pub formats: Vec<String>,
    pub barcodes: Vec<String>,
    pub tracklist: Vec<RemoteTrack>,
    pub url: String,
    /// Verified aliases (for example a merged release ID that redirects).
    #[serde(default)]
    pub lookup_aliases: Vec<ReleaseId>,
    #[serde(default)]
    pub artist_credit: Option<String>,
    /// Structural findings from the raw document that block acceptance.
    #[serde(default)]
    pub structure_issues: Vec<String>,
    /// Edition qualifiers taken from disambiguation and format descriptions.
    #[serde(default)]
    pub edition_notes: Vec<String>,
    /// Radish-owned provider extension; null for Discogs snapshots.
    #[serde(default)]
    pub provider_metadata: serde_json::Value,
}

impl RemoteRelease {
    pub fn artist_display(&self) -> String {
        if let Some(credit) = self.artist_credit.as_deref()
            && !credit.trim().is_empty()
        {
            return credit.to_string();
        }
        self.artists.join(", ")
    }

    pub fn label_display(&self) -> Option<String> {
        if self.labels.is_empty() {
            None
        } else {
            Some(self.labels.join(", "))
        }
    }

    pub fn catalog_number(&self) -> Option<String> {
        self.catalog_numbers.first().cloned()
    }

    pub fn format_display(&self) -> Option<String> {
        if self.formats.is_empty() {
            None
        } else {
            Some(self.formats.join(", "))
        }
    }

    /// Playable entries in release order, paired with their tracklist index.
    pub fn playable_tracks(&self) -> Vec<(usize, &RemoteTrack)> {
        self.tracklist
            .iter()
            .enumerate()
            .filter(|(_, track)| track.is_playable())
            .collect()
    }

    /// Index entries, unknown structures, and raw-document findings cannot be
    /// flattened or dropped, so they block acceptance of a complete mapping.
    pub fn has_unsupported_entries(&self) -> bool {
        !self.structure_issues.is_empty()
            || self.tracklist.iter().any(|track| {
                matches!(
                    track.entry_type,
                    TrackEntryType::Index | TrackEntryType::Other
                )
            })
    }

    pub fn provider(&self) -> ProviderKind {
        self.id.provider()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchSummary {
    pub id: ReleaseId,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub artist_credit: Option<String>,
    pub year: Option<i64>,
    pub country: Option<String>,
    pub labels: Vec<String>,
    pub catalog_numbers: Vec<String>,
    pub formats: Vec<String>,
    pub barcodes: Vec<String>,
    pub result_type: String,
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Unicode-normalizes, case-folds, converts punctuation to spaces, and
/// collapses whitespace. Original strings are always kept for display.
///
/// Decomposing first means diacritics compare equal to their base letter, so
/// `Sigur Rós` and `sigur ros` normalize the same way.
pub fn normalize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;

    for ch in input.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            pending_space = true;
        }
    }

    out
}

/// Latin combining diacritical marks produced by decomposition.
fn is_combining_mark(ch: char) -> bool {
    matches!(ch, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

/// Discogs disambiguates artists with numeric suffixes like `Nirvana (2)`.
/// Only comparison drops them; display and storage keep the original text.
pub fn strip_disambiguator(input: &str) -> String {
    let trimmed = input.trim_end();
    if !trimmed.ends_with(')') {
        return input.to_string();
    }
    if let Some(open) = trimmed.rfind(" (") {
        let inner = &trimmed[open + 2..trimmed.len() - 1];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
            return trimmed[..open].trim_end().to_string();
        }
    }
    input.to_string()
}

pub fn normalize_artist(input: &str) -> String {
    normalize_text(&strip_disambiguator(input))
}

/// MusicBrainz credits are compared as written; numeric disambiguation is a
/// Discogs-only convention.
pub fn normalize_artist_for(provider: ProviderKind, input: &str) -> String {
    match provider {
        ProviderKind::Discogs => normalize_artist(input),
        ProviderKind::MusicBrainz => normalize_text(input),
    }
}

/// Normalized Levenshtein similarity in `0.0..=1.0`.
///
/// Text that normalizes to nothing, such as `÷`, is compared by its original
/// characters instead, so distinct symbol-only titles do not look identical.
pub fn similarity(a: &str, b: &str) -> f64 {
    comparison_similarity(a, &normalized_chars(a), b, &normalized_chars(b))
}

/// [`similarity`] over titles whose apostrophe-joined text the caller already
/// has: the better of that and the apostrophe-as-space reading.
///
/// A tag may write `Dont`, `Don t`, or `Don't` (or `A&B` for `A B`) for the
/// same thing, so neither reading alone is right; taking the better of the two never scores a pair
/// lower than either one would.
fn comparison_similarity(a: &str, a_chars: &[char], b: &str, b_chars: &[char]) -> f64 {
    let joined = similarity_of_normalized(a, a_chars, b, b_chars);
    if joined >= 1.0 || !(needs_plain_reading(a) || needs_plain_reading(b)) {
        return joined;
    }
    let spaced = similarity_of_normalized(
        a,
        &normalize_text(a).chars().collect::<Vec<_>>(),
        b,
        &normalize_text(b).chars().collect::<Vec<_>>(),
    );
    joined.max(spaced)
}

/// Whether [`normalized_chars`] reads `text` differently from [`normalize_text`].
fn needs_plain_reading(text: &str) -> bool {
    text.chars().any(|ch| ch == '&' || is_apostrophe(ch))
}

/// Text for comparison: [`normalize_text`] with apostrophes joined, not split.
///
/// `Don't`, `Don’t` and a tag's `Dont` all compare as `dont`. Only comparison
/// does this: [`normalize_text`] also feeds the search indexes and the stored
/// album key, so changing it would desynchronize both. `&` reads as `and`.
/// [`comparison_similarity`] also tries the plain [`normalize_text`] reading, in
/// which an apostrophe or `&` is a space.
fn normalized_chars(text: &str) -> Vec<char> {
    let joined: String = text.chars().filter(|ch| !is_apostrophe(*ch)).collect();
    // `Seek & Destroy` and `Seek And Destroy` are the same words.
    let spelled = joined.replace('&', " and ");
    normalize_text(&spelled).chars().collect()
}

fn is_apostrophe(ch: char) -> bool {
    matches!(
        ch,
        '\'' | '\u{2019}' | '\u{2018}' | '\u{02BC}' | '`' | '\u{00B4}'
    )
}

/// Width of an ID3v1 text field. Software that wrote those tags cut longer
/// titles and artists to this many characters, and the library keeps the value
/// as it was read, padded or cut.
const ID3V1_FIELD_CHARS: usize = 30;

/// The first [`ID3V1_FIELD_CHARS`] characters of `remote`, when `local` fills a
/// whole ID3v1 field and `remote` is longer than one.
///
/// A local value of exactly that width may be a cut-off copy of `remote`, which
/// no full-length comparison can ever match. Shorter local values are complete
/// as written. A padded short value is also a full-width one, but it is still
/// far from any unrelated prefix, so it stays well below the match threshold.
fn id3v1_prefix<'a>(local: &str, remote: &'a str) -> Option<&'a str> {
    if local.chars().count() != ID3V1_FIELD_CHARS {
        return None;
    }
    let (end, _) = remote.char_indices().nth(ID3V1_FIELD_CHARS)?;
    Some(&remote[..end])
}

/// Taken off a comparison that read a local value as a cut-off copy, so it
/// never scores as an exact match.
const CUT_OFF_PENALTY: f64 = 0.03;

/// The fewest comparison characters a cut-off copy may have. Shorter text is a
/// prefix of too many unrelated titles to say anything.
const CUT_OFF_MIN_CHARS: usize = 20;

/// `1 - CUT_OFF_PENALTY` when `local` may be `remote` cut to an ID3v1 field.
///
/// A value of 29 or 30 characters whose comparison text (punctuation dropped,
/// apostrophes joined) is a strict prefix of the remote's is read as cut off.
/// At 29 the remote must continue with a space: the lost 30th character was
/// that space, and the writers that cut text to fit drop a trailing space. At
/// 30 the cut may fall inside a word. Comparing normalized text, not the
/// remote's first 30 raw characters, also covers a local copy that lost its
/// punctuation, which shifts the cut.
fn cut_off_similarity(local: &str, remote: &str) -> Option<f64> {
    cut_off_similarity_at(local.chars().count(), local, remote)
}

/// [`cut_off_similarity`] for a `local` text that came from a tag `width`
/// characters wide. A title read after its track number was removed is shorter
/// than the field it was cut to, so the width is the raw tag's, not its own.
fn cut_off_similarity_at(width: usize, local: &str, remote: &str) -> Option<f64> {
    if width != ID3V1_FIELD_CHARS && width != ID3V1_FIELD_CHARS - 1 {
        return None;
    }
    let (local_chars, remote_chars) = (normalized_chars(local), normalized_chars(remote));
    if local_chars.len() < CUT_OFF_MIN_CHARS
        || remote_chars.len() <= local_chars.len()
        || !remote_chars.starts_with(&local_chars)
    {
        return None;
    }
    if width == ID3V1_FIELD_CHARS - 1 && remote_chars[local_chars.len()] != ' ' {
        return None;
    }
    Some(1.0 - CUT_OFF_PENALTY)
}

/// [`similarity`] for a value read from a local file: the better of the full
/// comparison, a comparison with the remote's first 30 characters for a
/// 30-character local value, and the cut-off reading of
/// [`cut_off_similarity`].
pub fn local_similarity(local: &str, remote: &str) -> f64 {
    local_similarity_with(local, remote, true)
}

/// [`local_similarity`] with the cut-off and ID3v1-prefix readings switched on
/// or off. They only apply to text that is the raw tag: a title that was
/// shortened for comparison says nothing about how long the tag was.
fn local_similarity_with(local: &str, remote: &str, allow_cut_off: bool) -> f64 {
    let mut full = similarity(local, remote);
    if let Some(value) = suffix_tolerant_similarity(local, remote) {
        full = full.max(value);
    }
    if !allow_cut_off {
        return full;
    }
    if let Some(value) = cut_off_similarity(local, remote) {
        full = full.max(value);
    }
    id3v1_prefix(local, remote).map_or(full, |prefix| full.max(similarity(local, prefix)))
}

/// Taken off a comparison that needed a suffix removed, so it never scores as
/// an exact match.
const SUFFIX_PENALTY: f64 = 0.05;

/// Words that make a bracketed suffix part of the recording or edition rather
/// than a subtitle, so it is never removed for comparison.
const VERSION_WORDS: &[&str] = &[
    "acoustic",
    "clean",
    "cover",
    "demo",
    "deluxe",
    "dub",
    "edit",
    "explicit",
    "extended",
    "instrumental",
    "instrumentals",
    "live",
    "mix",
    "mono",
    "radio",
    "remaster",
    "remastered",
    "remix",
    "remixes",
    "reprise",
    "stereo",
    "unplugged",
    "uncensored",
    "censored",
    "version",
];

/// `text` without one trailing suffix that is a subtitle or credit: a bracketed
/// group (`(I'll Be Anything)`, `[Ramones]`, or one cut off before it closed), a
/// `feat.`/`ft.`/`featuring` tail, or a `part N` marker. `None` when there is
/// no such suffix, it names a version, or nothing would be left.
fn strip_title_suffix(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let lower = trimmed.to_lowercase();

    let mut cut = bracketed_suffix_start(trimmed)
        .or_else(|| featured_suffix_start(&lower))
        .or_else(|| part_suffix_start(&lower))?;
    // Positions came from the lowercased text; lowercasing can change byte
    // lengths for a few characters, so keep only a boundary of `trimmed`.
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    let (base, suffix) = trimmed.split_at(cut);
    let suffix_lower = suffix.to_lowercase();
    if normalize_text(suffix)
        .split_whitespace()
        .any(|word| VERSION_WORDS.contains(&word))
        || suffix_lower.contains("remaster")
    {
        return None;
    }
    // The separator before the suffix goes with it (`Song, Part II`).
    let base = base.trim_end_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '-' | ':' | ';' | '\u{2013}' | '\u{2014}')
    });
    base.chars()
        .any(char::is_alphanumeric)
        .then(|| base.to_string())
}

/// Byte offset of a trailing `(...)` or `[...]` group, or of an opening bracket
/// that was never closed (a tag cut off mid-subtitle).
fn bracketed_suffix_start(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut open = None;
    for (position, ch) in text.char_indices() {
        match ch {
            '(' | '[' => {
                if depth == 0 {
                    open = Some(position);
                }
                depth += 1;
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && position + ch.len_utf8() < text.len() {
                    // Closed with more text after it: not a trailing group.
                    open = None;
                }
            }
            _ => {}
        }
    }
    open
}

/// Byte offset of a trailing `feat.`, `ft.` or `featuring` credit.
fn featured_suffix_start(lower: &str) -> Option<usize> {
    ["featuring ", "feat. ", "feat ", "ft. ", "ft "]
        .iter()
        .filter_map(|marker| {
            lower.rfind(marker).filter(|&at| {
                at > 0
                    && !lower[..at]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric)
            })
        })
        .min()
}

/// Byte offset of a trailing `part N` or `pt. N` marker, where `N` is a number
/// or a roman numeral.
fn part_suffix_start(lower: &str) -> Option<usize> {
    let words: Vec<(usize, &str)> = lower
        .split_whitespace()
        .map(|word| (word.as_ptr() as usize - lower.as_ptr() as usize, word))
        .collect();
    let [.., (at, marker), (_, number)] = words.as_slice() else {
        return None;
    };
    let marker = marker.trim_end_matches(['.', ',']);
    let number = number.trim_matches(|ch: char| !ch.is_alphanumeric());
    let numeric = !number.is_empty()
        && (number.chars().all(|ch| ch.is_ascii_digit())
            || number.chars().all(|ch| matches!(ch, 'i' | 'v' | 'x')));
    (matches!(marker, "part" | "pt") && numeric && *at > 0).then_some(*at)
}

/// Comparison that ignores a suffix present on one side only, less
/// [`SUFFIX_PENALTY`]. `None` when both or neither side has a removable suffix:
/// two different subtitles are two different titles (`Intro (Part 1)` and
/// `Intro (Part 2)`).
fn suffix_tolerant_similarity(local: &str, remote: &str) -> Option<f64> {
    let (local_base, remote_base) = (strip_title_suffix(local), strip_title_suffix(remote));
    let value = match (local_base, remote_base) {
        (Some(base), None) => similarity(&base, remote),
        (None, Some(base)) => similarity(local, &base),
        _ => return None,
    };
    Some((value - SUFFIX_PENALTY).max(0.0))
}

/// Taken off a track-title reading that needed junk removed (an audio
/// extension, a shared release tag or a track number), so a cleaned title never
/// scores as an exact match.
const TRACK_TITLE_CLEANUP_PENALTY: f64 = 0.02;

/// The fewest titles that must end in the whole token to show it is a release
/// tag. On fewer, titles that end alike are a coincidence.
const SHARED_TOKEN_MIN_TRACKS: usize = 3;

/// The shortest and longest release tag (`SYN`, `amrc`, `1up`), in characters.
/// A single character is too often part of the title, and a longer word is
/// more likely a subtitle than a scene group.
const SHARED_TOKEN_MIN_CHARS: usize = 2;
const SHARED_TOKEN_MAX_CHARS: usize = 8;

/// The most digits a track number prefix has. A longer run is a number that
/// belongs to the title (`1000 Ways`, `1979`).
const NUMBER_PREFIX_MAX_DIGITS: usize = 3;

/// A local track title read for comparison only, with the junk a tagger left
/// in it removed. Stored tags never change.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanedTitle {
    text: String,
    /// The width of the raw tag, when the cut-off reading may be applied to
    /// `text`: only a track number was removed, so the tag was cut to its field
    /// width with the number still in it. `None` once an extension or release
    /// tag was removed as well, because those say nothing about where the cut
    /// fell.
    cut_width: Option<usize>,
}

/// `title` without a trailing audio extension (`Cheap_Wine-SYN.mp3`). `None`
/// when it has none or nothing but punctuation would be left.
fn strip_audio_extension(title: &str) -> Option<&str> {
    let (stem, extension) = title.rsplit_once('.')?;
    let known = AUDIO_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension));
    (known && stem.chars().any(char::is_alphanumeric)).then_some(stem)
}

/// `title` split at its last `-` or `_` into the text before and after it, when
/// no space comes before the separator (`Song - Seattle` is a subtitle) and the
/// text before it holds a letter or digit.
fn split_trailing_token(title: &str) -> Option<(&str, &str)> {
    let at = title.rfind(['-', '_'])?;
    let (base, token) = (&title[..at], &title[at + 1..]);
    (!base.ends_with(char::is_whitespace) && base.chars().any(char::is_alphanumeric))
        .then_some((base, token))
}

/// Abbreviations and loose words for a version of the recording, which are
/// never a release tag even though [`VERSION_WORDS`] does not spell them. A
/// vocal release whose every title ends `-Inst` is not tagged by a group: the
/// files are a different version, and reading the suffix as junk would score
/// them as the vocal tracks. Compared without case and without trailing digits
/// (`take2`, `rmx2`, `inst2`), and no entry is longer than a tag can be.
const NON_TAG_WORDS: &[&str] = &[
    "inst", "instr", "instru", "karaoke", "acapella", "acapela", "tv", "acap", "offvocal", "rmx",
    "remixed", "mixed", "vip", "vocal", "club", "dirty", "ext", "orig", "original", "alt", "take",
    "session", "outtake", "bonus", "ver", "vers",
];

/// Whether `token` is written in Title Case: an uppercase first letter and at
/// least one more letter, all of them lowercase (`Inst`, `Hop`). Scene group
/// tags are lowercase (`amrc`), uppercase (`SYN`) or mixed (`ViLE`, `iTS`), so a
/// Title Case word at the end of every title is an English word.
fn is_title_case(token: &str) -> bool {
    let mut letters = token.chars().filter(char::is_ascii_alphabetic);
    letters
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && {
            let rest: Vec<char> = letters.collect();
            !rest.is_empty() && rest.iter().all(char::is_ascii_lowercase)
        }
}

/// Whether `token` has the shape of a release tag: 2 to 8 ASCII letters and
/// digits with at least one letter, and not a version word or version
/// abbreviation ([`VERSION_WORDS`], [`NON_TAG_WORDS`] with any trailing digits
/// dropped, or `v` and digits alone such as `v2`), which say something about
/// the recording. How the titles spell it is checked by
/// [`shared_trailing_token`].
fn is_release_tag(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let word = lower.trim_end_matches(|ch: char| ch.is_ascii_digit());
    let version_marker = lower
        .strip_prefix('v')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()));
    (SHARED_TOKEN_MIN_CHARS..=SHARED_TOKEN_MAX_CHARS).contains(&token.len())
        && token.chars().all(|ch| ch.is_ascii_alphanumeric())
        && token.chars().any(|ch| ch.is_ascii_alphabetic())
        && !version_marker
        && !VERSION_WORDS.contains(&word)
        && !NON_TAG_WORDS.contains(&word)
}

/// Whether a title of this many characters may have been cut to an ID3v1
/// field, which loses the end of a release tag or all of it.
fn may_be_cut_to_field(title: &str) -> bool {
    let width = title.chars().count();
    width == ID3V1_FIELD_CHARS || width == ID3V1_FIELD_CHARS - 1
}

/// The release tag the titles share (`-amrc`, `_SYN`), compared without case.
///
/// At least [`SHARED_TOKEN_MIN_TRACKS`] titles must end in the whole token, and
/// no title may contradict it. None may spell it in Title Case (see
/// [`is_title_case`]), whichever title comes first. A title of 29 or 30 characters may have been
/// cut to an ID3v1 field, so it may end in the whole token, in the start of it,
/// or lack it. Every other title, shorter or longer, must end in the whole
/// token: a longer one cannot have been cut, and a shorter one has room for it.
/// The token is read from the first title that is not of 29 or 30 characters.
fn shared_trailing_token<'a>(titles: &[&'a str]) -> Option<&'a str> {
    let first = titles.iter().find(|title| !may_be_cut_to_field(title))?;
    let (_, token) = split_trailing_token(first)?;
    if !is_release_tag(token) {
        return None;
    }
    let mut ending_in_token = 0;
    for title in titles {
        let tail = split_trailing_token(title).map(|(_, tail)| tail);
        if let Some(tail) = tail.filter(|tail| tail.eq_ignore_ascii_case(token)) {
            if is_title_case(tail) {
                return None;
            }
            ending_in_token += 1;
        } else if !may_be_cut_to_field(title) {
            return None;
        }
    }
    (ending_in_token >= SHARED_TOKEN_MIN_TRACKS).then_some(token)
}

/// `title` without the trailing `-token` or `_token` that
/// [`shared_trailing_token`] found. A title of 29 or 30 characters may end in
/// the start of the token instead (`...sky-am` for `-amrc`), which goes too.
fn strip_shared_token<'a>(title: &'a str, token: &str) -> Option<&'a str> {
    let (base, tail) = split_trailing_token(title)?;
    let partial = may_be_cut_to_field(title)
        && !tail.is_empty()
        && tail.len() < token.len()
        && token.as_bytes()[..tail.len()].eq_ignore_ascii_case(tail.as_bytes());
    (tail.eq_ignore_ascii_case(token) || partial).then_some(base)
}

/// Characters that may follow a track number prefix.
fn is_number_separator(ch: char) -> bool {
    matches!(ch, ' ' | '-' | '.' | '_' | ')' | ']' | ':')
}

/// `title` without a leading track number that equals `number`: an optional
/// `(` or `[`, 1 to [`NUMBER_PREFIX_MAX_DIGITS`] digits, at least one separator
/// and then text with a letter or digit in it. The digit run is maximal, so
/// `1000 Ways` does not start with 10 and `1979` is not a prefix at all.
fn strip_track_number_prefix(title: &str, number: i64) -> Option<&str> {
    let body = title.strip_prefix(['(', '[']).unwrap_or(title);
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=NUMBER_PREFIX_MAX_DIGITS).contains(&digits) || body[..digits].parse() != Ok(number) {
        return None;
    }
    let after_digits = &body[digits..];
    let rest = after_digits.trim_start_matches(is_number_separator);
    (rest.len() < after_digits.len() && rest.chars().any(char::is_alphanumeric)).then_some(rest)
}

/// The track number a title's prefix has to equal: the tagged one, or for an
/// untagged track its 1-based position on its disc, counted in local order. A
/// track with no disc is on disc 1, as in `store::order_release_tracks`.
fn expected_track_number(local: &LocalRelease, index: usize) -> Option<i64> {
    let track = local.tracks.get(index)?;
    track.track_number.or_else(|| {
        let earlier = local.tracks[..index]
            .iter()
            .filter(|other| other.disc.unwrap_or(1) == track.disc.unwrap_or(1))
            .count();
        i64::try_from(earlier + 1).ok()
    })
}

/// One extra reading of each local track title for comparison, or `None` for a
/// track with no title or nothing to remove. In this order, on the title with
/// its trailing spaces removed (ID3v1 pads with them):
///
/// 1. a trailing audio extension (`02-Cheap_Wine-SYN.mp3`);
/// 2. the release tag every title of the release ends with
///    ([`shared_trailing_token`]);
/// 3. a leading number equal to the track number ([`strip_track_number_prefix`]).
///
/// Only comparison reads this. Stored tags, version qualifiers and everything
/// else keep the raw title.
fn cleaned_track_titles(local: &LocalRelease) -> Vec<Option<CleanedTitle>> {
    let stems: Vec<Option<(&str, bool)>> = local
        .tracks
        .iter()
        .map(|track| {
            let title = track.title.as_deref()?.trim_end_matches(' ');
            Some(strip_audio_extension(title).map_or((title, false), |stem| (stem, true)))
        })
        .collect();
    let titles: Vec<&str> = stems.iter().flatten().map(|(stem, _)| *stem).collect();
    let token = shared_trailing_token(&titles);

    stems
        .iter()
        .enumerate()
        .map(|(index, stem)| {
            let (stem, extension_removed) = (*stem)?;
            let untagged = token.and_then(|token| strip_shared_token(stem, token));
            let number_free = untagged.unwrap_or(stem);
            let number_removed = expected_track_number(local, index)
                .and_then(|number| strip_track_number_prefix(number_free, number));
            if !extension_removed && untagged.is_none() && number_removed.is_none() {
                return None;
            }
            let only_number = !extension_removed && untagged.is_none();
            let raw_width = local.tracks[index].title.as_deref()?.chars().count();
            Some(CleanedTitle {
                text: number_removed.unwrap_or(number_free).to_string(),
                cut_width: only_number.then_some(raw_width),
            })
        })
        .collect()
}

/// The best reading of a cleaned title, less [`TRACK_TITLE_CLEANUP_PENALTY`]:
/// the full comparison, and the cut-off reading at the raw tag's width when
/// [`CleanedTitle::cut_width`] allows it.
fn cleaned_title_similarity(cleaned: &CleanedTitle, remote: &str) -> f64 {
    let mut value = local_similarity_with(&cleaned.text, remote, false);
    if let Some(cut_off) = cleaned
        .cut_width
        .and_then(|width| cut_off_similarity_at(width, &cleaned.text, remote))
    {
        value = value.max(cut_off);
    }
    (value - TRACK_TITLE_CLEANUP_PENALTY).max(0.0)
}

/// [`local_similarity`] for a track title: the better of the raw reading and
/// the `cleaned` one, when the title has one.
fn track_title_similarity(raw: &str, cleaned: Option<&CleanedTitle>, remote: &str) -> f64 {
    let value = local_similarity(raw, remote);
    cleaned.map_or(value, |cleaned| {
        value.max(cleaned_title_similarity(cleaned, remote))
    })
}

/// Taken off an album-title comparison that needed a leading `The`, `O.S.T.`
/// marker or trailing release-type word dropped.
const ALBUM_TYPE_PENALTY: f64 = 0.03;

/// Words a tag or a store appends to an album title to say what kind of release
/// it is, not which album: `No Distance Left to Run EP`, `... OST`, `... Japan`.
/// Versions (live, remix, remastered, and the rest of [`VERSION_WORDS`]) are not
/// here, so they are never dropped. The one exception is `explicit`, which
/// [`RELEASE_TYPE_COMPANIONS`] drops next to a release-type word.
const RELEASE_TYPE_WORDS: &[&str] = &[
    "advance",
    "aust",
    "australian",
    "bonus",
    "bootleg",
    "cd",
    "cdep",
    "cdm",
    "cds",
    "digipack",
    "digipak",
    "disc",
    "edition",
    "ep",
    "import",
    "japan",
    "japanese",
    "limited",
    "lp",
    "maxi",
    "mcd",
    "ost",
    "promo",
    "reissue",
    "retail",
    "single",
    "soundtrack",
    "st",
    "vinyl",
    "vls",
];

/// Words that are only a decoration next to a release-type word or a record
/// size: `Explicit`, `Proper` and `Tape` are dropped from
/// `The Truth (RETAIL EXPLICIT)`, but `Album (Explicit)` and `Demo Tape` keep
/// theirs. In front of the other decoration they are dropped only when
/// [`DecorationUnit::marked`] says they are one too (`Democrazy-TAPE ADVANCE`),
/// since `Sex Tape Retail` is not `Sex`.
const RELEASE_TYPE_COMPANIONS: &[&str] = &["explicit", "proper", "tape"];

/// The record sizes, in inches, that a scene tag writes after a title
/// (`7"`, `12 Inch`, `10inch`).
const RECORD_SIZES: &[&str] = &["7", "10", "12"];

/// The spellings of the inch unit that follow a record size.
const INCH_UNITS: &[&str] = &["in", "inch", "inches"];

/// `text` without a leading `O.S.T.`/`V.A.` marker (`O.S.T. Mary Reilly`,
/// `v.a-smells like team spirit`).
fn without_leading_marker(text: &str) -> &str {
    let trimmed = text.trim_start();
    let lower = trimmed.to_lowercase();
    for marker in ["o.s.t.", "o.s.t", "v.a.", "v.a", "ost"] {
        let Some(rest) = lower.strip_prefix(marker) else {
            continue;
        };
        let boundary = rest
            .chars()
            .next()
            .is_none_or(|ch| ch.is_whitespace() || matches!(ch, '-' | ':' | '.' | '\u{2013}'));
        if boundary {
            // The marker is ASCII, so its length is the same in `trimmed`.
            return trimmed[marker.len()..].trim_start_matches(|ch: char| {
                ch.is_whitespace() || matches!(ch, '-' | ':' | '.' | '\u{2013}')
            });
        }
    }
    trimmed
}

/// Whether `word` is a release-type word with a disc or part number on it
/// (`CDS2`, `CD2`).
fn numbered_release_type(word: &str) -> bool {
    let stem = word.trim_end_matches(|ch: char| ch.is_ascii_digit());
    stem.len() < word.len() && RELEASE_TYPE_WORDS.contains(&stem)
}

/// `text` as lowercase letters and digits only.
fn alphanumeric_lowercase(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `ch` splits a title into the tokens of a decoration run.
fn is_decoration_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '-' | '_')
}

/// The tokens of `text` split at whitespace, `-` and `_`, each with the byte
/// offset where it starts.
fn decoration_tokens(text: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if is_decoration_separator(ch) {
            if let Some(from) = start.take() {
                tokens.push((from, &text[from..index]));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        tokens.push((from, &text[from..]));
    }
    tokens
}

/// Whether `letters` (a token's letters and digits) is a record size in one
/// piece: `7inch`, `12in`.
fn is_sized_unit(letters: &str) -> bool {
    RECORD_SIZES.iter().any(|size| {
        letters
            .strip_prefix(size)
            .is_some_and(|unit| INCH_UNITS.contains(&unit))
    })
}

/// Whether `token` marks its number as a size in inches: the number is
/// directly followed by `"`, `''`, `”` or `″` (`7"`, `12''`, `(10″)`). A quote
/// before the number or further away is not a mark (`"7"`).
fn carries_inch_mark(token: &str) -> bool {
    let token = token.trim_start_matches(is_opening_bracket);
    if !token.starts_with(|ch: char| ch.is_ascii_digit()) {
        return false;
    }
    let after_number = token.trim_start_matches(|ch: char| ch.is_ascii_digit());
    after_number.starts_with(['"', '\u{201D}', '\u{2033}']) || after_number.starts_with("''")
}

/// Whether `ch` opens a bracketed group.
fn is_opening_bracket(ch: char) -> bool {
    matches!(ch, '(' | '[' | '{')
}

/// What one token of a decoration run is.
enum DecorationToken {
    /// A release-type word, or a record size.
    Word,
    /// A numbered release-type word (`cd2`).
    Numbered,
    /// A companion word.
    Companion,
}

/// One word or size of a decoration run: a record size written as two tokens
/// (`7 Inch`) is one unit.
#[derive(Debug, PartialEq, Eq)]
struct DecorationUnit {
    /// Byte offset where the unit starts.
    start: usize,
    /// Whether the unit is a companion word.
    companion: bool,
    /// For a companion word: whether it is joined to the text before it by `-`,
    /// `_` or an opening bracket, or written in capitals after text that has a
    /// lowercase letter, so that it is read as decoration even in front of the
    /// other decoration (`Democrazy-TAPE ADVANCE`, `Breakin News PROPER
    /// RETAIL`, `Boy Hits Car (Explicit Retail)`). Capitals say nothing in a
    /// title that is all capitals.
    marked: bool,
}

/// The decoration at the end of an album title: release-type words and sizes
/// that say what kind of release it is, not which album.
#[derive(Debug, PartialEq, Eq)]
struct DecorationRun {
    /// The units of the run, in the order they are written.
    units: Vec<DecorationUnit>,
    /// The numbered release-type words in the run (`cd2`).
    numbered: BTreeSet<String>,
}

/// What `token` is in a decoration run, given its `letters`, or `None` when it
/// ends the run. A unit on its own (`Inch`) is not decided here, since it needs
/// the size before it.
fn classify_decoration_token(token: &str, letters: &str) -> Option<DecorationToken> {
    if numbered_release_type(letters) {
        Some(DecorationToken::Numbered)
    } else if RELEASE_TYPE_COMPANIONS.contains(&letters) {
        Some(DecorationToken::Companion)
    } else if RELEASE_TYPE_WORDS.contains(&letters)
        || is_sized_unit(letters)
        || (RECORD_SIZES.contains(&letters) && carries_inch_mark(token))
    {
        Some(DecorationToken::Word)
    } else {
        None
    }
}

/// Whether the companion word `token`, which starts at byte `start` of `text`,
/// is [marked](DecorationUnit::marked).
fn is_marked_companion(text: &str, start: usize, token: &str) -> bool {
    let joined = text[..start]
        .trim_end()
        .chars()
        .next_back()
        .is_some_and(|ch| matches!(ch, '-' | '_') || is_opening_bracket(ch))
        || token.starts_with(is_opening_bracket);
    let capitals = token
        .chars()
        .filter(|ch| ch.is_alphabetic())
        .all(char::is_uppercase)
        && text[..start].chars().any(char::is_lowercase);
    joined || capitals
}

/// The run of decoration tokens at the end of `text`, or `None` when it does
/// not end in one. Tokens are split at whitespace, `-` and `_`, so
/// `Clocks-RETAIL CDS` and `Mandatory Music - Vinyl` are read as well as
/// `Boombox VLS`, and brackets around a token do not matter (`(Vinyl)`).
/// Scanning from the end, punctuation-only tokens are skipped and the run
/// stops at the first other token. A token is part of the run when it is:
///
/// - a release-type word ([`RELEASE_TYPE_WORDS`]) or a numbered one (`CD2`);
/// - a companion word ([`RELEASE_TYPE_COMPANIONS`]);
/// - a record size: `7`, `10` or `12` with an inch unit in the same token
///   (`7inch`) or the token after it (`7 Inch`), or with an inch mark directly
///   after the number (`7"`, `12''`).
fn decoration_run(text: &str) -> Option<DecorationRun> {
    let tokens = decoration_tokens(text);
    let mut units = Vec::new();
    let mut numbered = BTreeSet::new();
    let mut index = tokens.len();
    while index > 0 {
        index -= 1;
        let (mut start, token) = tokens[index];
        let letters = alphanumeric_lowercase(token);
        if letters.is_empty() {
            continue;
        }
        let kind = if INCH_UNITS.contains(&letters.as_str()) {
            // The size is the nearest token before the unit that has letters or
            // digits.
            let size = (0..index).rev().find_map(|before| {
                let size = alphanumeric_lowercase(tokens[before].1);
                (!size.is_empty()).then_some((before, size))
            });
            match size {
                Some((before, size)) if RECORD_SIZES.contains(&size.as_str()) => {
                    index = before;
                    start = tokens[before].0;
                    Some(DecorationToken::Word)
                }
                _ => None,
            }
        } else {
            classify_decoration_token(token, &letters)
        };
        let Some(kind) = kind else {
            break;
        };
        let companion = matches!(kind, DecorationToken::Companion);
        units.push(DecorationUnit {
            start,
            companion,
            marked: companion && is_marked_companion(text, start, token),
        });
        if matches!(kind, DecorationToken::Numbered) {
            numbered.insert(letters);
        }
    }
    units.reverse();
    (!units.is_empty()).then_some(DecorationRun { units, numbered })
}

/// The numbered release-type words (`cd1`, `cds2`) in the decoration run at the
/// end of `title`, as lowercase letters and digits.
fn numbered_release_types(title: &str) -> BTreeSet<String> {
    decoration_run(title).map_or_else(BTreeSet::new, |run| run.numbered)
}

/// Whether `word` is a volume number: digits or a roman numeral.
fn is_volume_number(word: &str) -> bool {
    let word = alphanumeric_lowercase(word);
    !word.is_empty()
        && (word.chars().all(|ch| ch.is_ascii_digit())
            || word.chars().all(|ch| matches!(ch, 'i' | 'v' | 'x')))
}

/// Words that are no album title on their own: kept text that is only one of
/// them (`The` from `The-Bootleg`) means the decoration was the title.
const BARE_ARTICLES: &[&str] = &["a", "an", "the"];

/// The byte offset in `body` where its decoration run is cut off, or `None`
/// when the run is not dropped: it needs a token other than a companion word,
/// and some letter or digit that is not a bare article must remain before it.
///
/// A title made only of decoration (`Japan EP`) keeps its first unit, so a
/// single unit (`12 Inch`, `Vinyl`) is never dropped. A companion word in front
/// of the other decoration goes with it only when it is
/// [marked](DecorationUnit::marked) or `keep_companions` is false; with
/// `keep_companions` the cut is at the first other unit.
fn decoration_cut(body: &str, run: &DecorationRun, keep_companions: bool) -> Option<usize> {
    let mut units = run.units.as_slice();
    if units.len() >= 2 && !body[..units[0].start].contains(char::is_alphanumeric) {
        units = &units[1..];
    }
    let first = units.iter().position(|unit| !unit.companion)?;
    let mut cut = first;
    while !keep_companions && cut > 0 && units[cut - 1].marked {
        cut -= 1;
    }
    let start = units[cut].start;
    let album = alphanumeric_lowercase(&body[..start]);
    (!album.is_empty() && !BARE_ARTICLES.contains(&album.as_str())).then_some(start)
}

/// An album title reduced to the album: no leading `The` or `O.S.T.`/`V.A.`
/// marker, no trailing decoration run ([`decoration_run`]: release-type words,
/// also numbered ones such as `CDS2`, record sizes, and companion words that
/// come with them), and no `Vol.`/`Volume` word in front of a final number.
/// `None` when nothing is removed or nothing would be left.
///
/// With `keep_companions`, a companion word in front of the other decoration
/// always stays (`Sex TAPE Retail` becomes `Sex TAPE`, not `Sex`).
fn reduce_album_title_with(text: &str, keep_companions: bool) -> Option<String> {
    let mut body = without_leading_marker(text);
    if let Some((first, rest)) = body.split_once(char::is_whitespace)
        && first.eq_ignore_ascii_case("the")
        && !rest.trim().is_empty()
    {
        body = rest.trim_start();
    }
    if let Some(run) = decoration_run(body)
        && let Some(cut) = decoration_cut(body, &run, keep_companions)
    {
        body = body[..cut]
            .trim_end_matches(|ch: char| is_decoration_separator(ch) || is_opening_bracket(ch));
    }
    let mut words: Vec<&str> = body.split_whitespace().collect();
    if words.len() > 2 && is_volume_number(words[words.len() - 1]) {
        let marker = alphanumeric_lowercase(words[words.len() - 2]);
        if matches!(marker.as_str(), "vol" | "vols" | "volume" | "volumes") {
            words.remove(words.len() - 2);
        }
    }
    let reduced = words.join(" ");
    (reduced != text.trim() && reduced.chars().any(char::is_alphanumeric)).then_some(reduced)
}

/// [`reduce_album_title_with`] dropping every companion word the run allows.
fn reduce_album_title(text: &str) -> Option<String> {
    reduce_album_title_with(text, false)
}

/// The reduced readings of an album title: the one that drops the most, and
/// the one that keeps the companion words in front of the other decoration
/// when that differs. Empty when the title is not reduced.
fn album_readings(text: &str) -> Vec<String> {
    let mut readings: Vec<String> = reduce_album_title(text).into_iter().collect();
    if let Some(reading) = reduce_album_title_with(text, true)
        && !readings.contains(&reading)
    {
        readings.push(reading);
    }
    readings
}

/// Comparison text with the spaces taken out, for `Rock 'n' Roll` against
/// `Rocknroll`.
fn compact_chars(text: &str) -> Vec<char> {
    normalized_chars(text)
        .into_iter()
        .filter(|ch| *ch != ' ')
        .collect()
}

/// [`local_similarity`] for an album title, also comparing the albums behind
/// the titles (`Iron Icon EP` against `The Iron Icon`, `Vol. 2` against
/// `Volume 2`) and the titles with their spaces removed (`Rock 'n' Roll`
/// against `Rocknroll`), each at a small cost. A title with companion words in
/// front of its decoration has two readings (`Sex Tape Retail`: `Sex` and
/// `Sex Tape`), and the better pair counts.
///
/// Two different numbered release-type words in the titles' decoration runs
/// (`CD1` against `CD2`, `Album-CD2` against `Album CD1`) are different discs,
/// so nothing is stripped from either title. A title that was shortened is not
/// read as a cut-off tag.
pub fn album_title_similarity(local: &str, remote: &str) -> f64 {
    let mut best = local_similarity(local, remote);
    let (local_discs, remote_discs) = (
        numbered_release_types(local),
        numbered_release_types(remote),
    );
    let different_discs =
        !local_discs.is_empty() && !remote_discs.is_empty() && local_discs != remote_discs;
    let (local_readings, remote_readings) = if different_discs {
        (Vec::new(), Vec::new())
    } else {
        (album_readings(local), album_readings(remote))
    };
    let albums = |readings: &'_ [String], raw: &'_ str| -> Vec<String> {
        if readings.is_empty() {
            vec![raw.to_string()]
        } else {
            readings.to_vec()
        }
    };
    let reduced_any = !local_readings.is_empty() || !remote_readings.is_empty();
    for local_album in albums(&local_readings, local) {
        for remote_album in albums(&remote_readings, remote) {
            if reduced_any {
                // The cut-off readings only apply when the local text is the raw tag.
                let reduced =
                    local_similarity_with(&local_album, &remote_album, local_readings.is_empty());
                best = best.max((reduced - ALBUM_TYPE_PENALTY).max(0.0));
            }
            let compact =
                similarity_chars(&compact_chars(&local_album), &compact_chars(&remote_album));
            best = best.max((compact - ALBUM_TYPE_PENALTY).max(0.0));
        }
    }
    best
}

/// [`artist_similarity`] with the same allowance for a cut-off local artist.
pub fn local_artist_similarity(local: &str, remote: &str) -> f64 {
    let mut full = artist_similarity(local, remote).max(folded_artist_similarity(local, remote));
    if let Some(value) = cut_off_similarity(local, remote) {
        full = full.max(value);
    }
    id3v1_prefix(local, remote).map_or(full, |prefix| {
        full.max(artist_similarity(local, prefix))
            .max(folded_artist_similarity(local, prefix))
    })
}

/// Taken off a comparison that needed a leading `The` or an `&` folded away, so
/// it never scores as a perfect match.
const ARTIST_FOLD_PENALTY: f64 = 0.02;

/// Artist text for the folded comparison: `&` read as `and` and one leading
/// `the` dropped, as in `Belle & Sebastian` against `Belle and Sebastian`, or
/// `Sisters of Mercy` against `The Sisters of Mercy`.
fn folded_artist_chars(normalize: impl Fn(&str) -> String, text: &str) -> Vec<char> {
    let spelled = text.replace('&', " and ");
    let normalized = normalize(&spelled);
    normalized
        .strip_prefix("the ")
        .unwrap_or(&normalized)
        .chars()
        .collect()
}

/// [`artist_similarity`] after folding, less [`ARTIST_FOLD_PENALTY`], or zero
/// when folding changes neither side.
fn folded_artist_similarity(a: &str, b: &str) -> f64 {
    let (a_folded, b_folded) = (
        folded_artist_chars(normalize_artist, a),
        folded_artist_chars(normalize_artist, b),
    );
    if a_folded == normalized_artist_chars(a) && b_folded == normalized_artist_chars(b) {
        return 0.0;
    }
    let value = similarity_of_normalized(
        &strip_disambiguator(a),
        &a_folded,
        &strip_disambiguator(b),
        &b_folded,
    );
    (value - ARTIST_FOLD_PENALTY).max(0.0)
}

fn normalized_artist_chars(text: &str) -> Vec<char> {
    normalize_artist(text).chars().collect()
}

/// [`similarity`] over titles that were normalized once by the caller.
fn similarity_of_normalized(a: &str, a_chars: &[char], b: &str, b_chars: &[char]) -> f64 {
    if a_chars.is_empty() || b_chars.is_empty() {
        return same_stripped_text(a, b);
    }
    similarity_chars(a_chars, b_chars)
}

/// 1.0 when both strings are non-empty and equal ignoring whitespace and case.
fn same_stripped_text(a: &str, b: &str) -> f64 {
    let strip = |text: &str| -> String {
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let (a, b) = (strip(a), strip(b));
    if !a.is_empty() && a == b { 1.0 } else { 0.0 }
}

/// Empty text carries no evidence, so it is never similar to anything.
fn similarity_chars(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let distance = levenshtein(a, b);
    let longest = a.len().max(b.len()) as f64;

    (1.0 - distance as f64 / longest).clamp(0.0, 1.0)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];

    for (i, a_char) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, b_char) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(a_char != b_char);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b.len()]
}

pub fn artist_similarity(a: &str, b: &str) -> f64 {
    let a_chars: Vec<char> = normalize_artist(a).chars().collect();
    let b_chars: Vec<char> = normalize_artist(b).chars().collect();
    similarity_of_normalized(
        &strip_disambiguator(a),
        &a_chars,
        &strip_disambiguator(b),
        &b_chars,
    )
}

/// `max(0, 1 - absolute_difference_seconds / 10)`.
pub fn duration_similarity(a: f64, b: f64) -> f64 {
    (1.0 - (a - b).abs() / 10.0).clamp(0.0, 1.0)
}

/// Parses Discogs duration strings such as `3:32` or `1:02:03`.
pub fn parse_duration(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() > 3 {
        return None;
    }

    let mut total = 0.0;
    for part in &parts {
        let value: f64 = part.trim().parse().ok()?;
        if value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
    }

    if total > 0.0 { Some(total) } else { None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParsedPosition {
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub side: Option<char>,
}

/// Parses Discogs position strings: `A1` (vinyl side + index), `2-03`
/// (disc + track), and plain `07`.
pub fn parse_position(raw: &str) -> ParsedPosition {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ParsedPosition::default();
    }

    if let Some((left, right)) = trimmed.split_once('-')
        && !left.is_empty()
        && left.chars().all(|c| c.is_ascii_digit())
        && let Some(track) = leading_number(right)
    {
        return ParsedPosition {
            disc: left.parse().ok(),
            track: Some(track),
            side: None,
        };
    }

    let mut chars = trimmed.chars();
    if let Some(first) = chars.next()
        && first.is_ascii_alphabetic()
    {
        let rest = chars.as_str();
        let side = Some(first.to_ascii_uppercase());
        if rest.is_empty() {
            return ParsedPosition {
                disc: None,
                track: None,
                side,
            };
        }
        if let Some(track) = leading_number(rest) {
            return ParsedPosition {
                disc: None,
                track: Some(track),
                side,
            };
        }
        return ParsedPosition::default();
    }

    ParsedPosition {
        disc: None,
        track: leading_number(trimmed),
        side: None,
    }
}

fn leading_number(raw: &str) -> Option<u32> {
    let digits: String = raw
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Edition words that change what a release is. They are meaningful, so a
/// mismatch is a contradiction rather than noise.
pub fn edition_qualifiers(text: &str) -> BTreeSet<&'static str> {
    const QUALIFIERS: &[&str] = &[
        "acoustic",
        "deluxe",
        "demo",
        "instrumental",
        "live",
        "mono",
        "remastered",
        "remix",
        "remixes",
        "stereo",
    ];

    let normalized = normalize_text(text);
    QUALIFIERS
        .iter()
        .copied()
        .filter(|qualifier| normalized.split_whitespace().any(|word| word == *qualifier))
        .collect()
}

/// Every qualifier a remote release asserts, across its title, formats, and
/// typed edition notes.
fn remote_edition_qualifiers(remote: &RemoteRelease) -> BTreeSet<&'static str> {
    let mut qualifiers = edition_qualifiers(&remote.title);
    for format in &remote.formats {
        qualifiers.extend(edition_qualifiers(format));
    }
    for note in &remote.edition_notes {
        qualifiers.extend(edition_qualifiers(note));
    }
    qualifiers
}

// ---------------------------------------------------------------------------
// Track mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    /// For each local track (in local order), the index into
    /// `RemoteRelease::tracklist`, if assigned.
    pub pairs: Vec<Option<usize>>,
    /// Every local file maps uniquely and counts agree.
    pub complete: bool,
    /// Two pairings were indistinguishable, so the result needs review.
    pub ambiguous: bool,
    pub notes: Vec<String>,
    /// Per local track, the assigned remote index and the title similarity
    /// that `map_tracks` measured for it. A cache only: never serialized, so
    /// decoded mappings start empty and readers recompute.
    #[serde(skip)]
    pair_similarity: Vec<Option<(usize, f64)>>,
}

impl Mapping {
    /// Title similarity between a local track and the remote track it is being
    /// scored against, or `None` when the local file has no title. Reuses the
    /// recorded value only when it was measured for this same remote track, so
    /// edited pairs are recomputed. A recomputed value reads the title the way
    /// [`TitleMatrix`] does, including its cleaned reading, so a mapping decoded
    /// from JSON scores exactly like a fresh one. `cleaned` is
    /// [`cleaned_track_titles`] of `local`, which the caller works out once for
    /// all its pairs.
    fn title_similarity(
        &self,
        local: &LocalRelease,
        cleaned: &[Option<CleanedTitle>],
        local_index: usize,
        remote_index: usize,
        remote_track: &RemoteTrack,
    ) -> Option<f64> {
        let title = local.tracks.get(local_index)?.title.as_deref()?;
        if let Some(Some((recorded_remote, recorded))) = self.pair_similarity.get(local_index)
            && *recorded_remote == remote_index
            && (0.0..=1.0).contains(recorded)
        {
            return Some(*recorded);
        }
        Some(track_title_similarity(
            title,
            cleaned.get(local_index).and_then(Option::as_ref),
            &remote_track.title,
        ))
    }

    pub fn assigned_pairs(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.pairs
            .iter()
            .enumerate()
            .filter_map(|(local, remote)| remote.map(|remote| (local, remote)))
    }

    pub fn assigned_count(&self) -> usize {
        self.pairs.iter().filter(|pair| pair.is_some()).count()
    }
}

/// Maps local files to remote tracklist entries.
///
/// Strong identifiers anchor first: unique release-track IDs, then unique
/// recording IDs, then unique `(medium, track)` positions. Everything else goes
/// through a minimum-cost one-to-one assignment over normalized title
/// similarity and duration, with ties broken deterministically and flagged as
/// ambiguous. Repeated recording IDs are legal on a release, so they only
/// anchor when both sides are unambiguous.
pub fn map_tracks(local: &LocalRelease, remote: &RemoteRelease) -> Mapping {
    let playable = remote.playable_tracks();
    let mut pairs: Vec<Option<usize>> = vec![None; local.tracks.len()];
    let mut used: Vec<bool> = vec![false; remote.tracklist.len()];
    let mut notes = Vec::new();
    let mut titles = TitleMatrix::new(local, remote);

    // Pass 1: unique release-track IDs.
    let mut local_release_ids: HashMap<Mbid, Vec<usize>> = HashMap::new();
    for (index, track) in local.tracks.iter().enumerate() {
        if let Some(id) = track.release_track_id {
            local_release_ids.entry(id).or_default().push(index);
        }
    }
    let mut remote_release_ids: HashMap<Mbid, Vec<usize>> = HashMap::new();
    for (release_index, track) in &playable {
        if let Some(id) = track.release_track_id {
            remote_release_ids
                .entry(id)
                .or_default()
                .push(*release_index);
        }
    }
    let mut anchors: Vec<(usize, usize)> = Vec::new();
    for (id, local_indexes) in &local_release_ids {
        if local_indexes.len() != 1 {
            notes.push(format!(
                "several local files claim release-track ID {id}; it cannot anchor a mapping"
            ));
            continue;
        }
        match remote_release_ids.get(id) {
            Some(remote_indexes) if remote_indexes.len() == 1 => {
                anchors.push((local_indexes[0], remote_indexes[0]));
            }
            Some(_) => notes.push(format!(
                "release-track ID {id} appears on several remote tracks"
            )),
            None => notes.push(format!(
                "release-track ID {id} from '{}' is not on the remote release",
                local.tracks[local_indexes[0]]
                    .title
                    .as_deref()
                    .unwrap_or(&local.tracks[local_indexes[0]].path)
            )),
        }
    }
    anchors.sort_unstable();
    for (local_index, remote_index) in anchors {
        pairs[local_index] = Some(remote_index);
        used[remote_index] = true;
    }

    // Pass 2: unique recording IDs among what is left.
    let mut local_recordings: HashMap<Mbid, Vec<usize>> = HashMap::new();
    for (index, track) in local.tracks.iter().enumerate() {
        if pairs[index].is_none()
            && let Some(id) = track.recording_id
        {
            local_recordings.entry(id).or_default().push(index);
        }
    }
    let mut remote_recordings: HashMap<Mbid, Vec<usize>> = HashMap::new();
    for (release_index, track) in &playable {
        if used[*release_index] {
            continue;
        }
        if let Some(id) = track.recording_id {
            remote_recordings
                .entry(id)
                .or_default()
                .push(*release_index);
        }
    }
    let mut recording_anchors: Vec<(usize, usize)> = Vec::new();
    for (id, local_indexes) in &local_recordings {
        if local_indexes.len() != 1 {
            continue;
        }
        if let Some(remote_indexes) = remote_recordings.get(id)
            && remote_indexes.len() == 1
        {
            recording_anchors.push((local_indexes[0], remote_indexes[0]));
        }
    }
    recording_anchors.sort_unstable();
    for (local_index, remote_index) in recording_anchors {
        pairs[local_index] = Some(remote_index);
        used[remote_index] = true;
    }

    // Pass 3: unique explicit medium/track positions.
    let remaining_local: Vec<usize> = (0..local.tracks.len())
        .filter(|index| pairs[*index].is_none())
        .collect();
    if !remaining_local.is_empty() {
        let remaining_remote: Vec<usize> = playable
            .iter()
            .map(|(index, _)| *index)
            .filter(|index| !used[*index])
            .collect();

        let all_keyed = remaining_local.iter().all(|index| {
            let track = &local.tracks[*index];
            track.track_number.is_some()
        }) && remaining_remote
            .iter()
            .all(|index| remote.tracklist[*index].position_key().is_some());

        if all_keyed {
            let mut local_by_key: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
            for index in &remaining_local {
                let track = &local.tracks[*index];
                let disc = track.disc.unwrap_or(1).max(0) as u32;
                let number = track.track_number.unwrap_or_default() as u32;
                local_by_key.entry((disc, number)).or_default().push(*index);
            }

            let mut remote_by_key: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
            for index in &remaining_remote {
                if let Some(key) = remote.tracklist[*index].position_key() {
                    remote_by_key.entry(key).or_default().push(*index);
                }
            }

            let mut position_pairs: Vec<((u32, u32), usize, usize)> = Vec::new();
            for (key, local_indexes) in &local_by_key {
                if local_indexes.len() != 1 {
                    continue;
                }
                let Some(remote_indexes) = remote_by_key.get(key) else {
                    continue;
                };
                if remote_indexes.len() != 1 {
                    continue;
                }
                position_pairs.push((*key, local_indexes[0], remote_indexes[0]));
            }
            position_pairs.sort_by_key(|(key, _, _)| *key);

            for (_, local_index, remote_index) in position_pairs {
                pairs[local_index] = Some(remote_index);
                used[remote_index] = true;
            }
        }
    }

    // Identifier anchors must not silently contradict tagged positions.
    for (local_index, remote_index) in pairs
        .iter()
        .enumerate()
        .filter_map(|(local, remote)| remote.map(|remote| (local, remote)))
    {
        let local_track = &local.tracks[local_index];
        let remote_track = &remote.tracklist[remote_index];
        let Some(remote_key) = remote_track.position_key() else {
            continue;
        };
        let Some(local_number) = local_track.track_number else {
            continue;
        };
        let local_key = (
            local_track.disc.unwrap_or(1).max(0) as u32,
            local_number.max(0) as u32,
        );
        if local_key != remote_key {
            notes.push(format!(
                "identifier for '{}' points to remote {} but its tags say {}-{}",
                local_track.title.as_deref().unwrap_or(&local_track.path),
                remote_track
                    .position
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", remote_key.0, remote_key.1)),
                local_key.0,
                local_key.1
            ));
        }
    }

    // Pass 4: minimum-cost assignment over what is left.
    let remaining_local: Vec<usize> = (0..local.tracks.len())
        .filter(|index| pairs[*index].is_none())
        .collect();
    let remaining_remote: Vec<usize> = playable
        .iter()
        .map(|(index, _)| *index)
        .filter(|index| !used[*index])
        .collect();

    if !remaining_local.is_empty() && !remaining_remote.is_empty() {
        let mut cost = Vec::with_capacity(remaining_local.len());
        for local_index in &remaining_local {
            let mut row = Vec::with_capacity(remaining_remote.len());
            for remote_index in &remaining_remote {
                row.push(pair_cost(
                    titles.get(*local_index, *remote_index),
                    &local.tracks[*local_index],
                    &remote.tracklist[*remote_index],
                ));
            }
            cost.push(row);
        }

        let assignment = solve_assignment(&cost);
        for (row, local_index) in remaining_local.iter().enumerate() {
            if let Some(column) = assignment.get(row).copied().flatten() {
                let remote_index = remaining_remote[column];
                pairs[*local_index] = Some(remote_index);
                used[remote_index] = true;
            }
        }

        // Ties: another remote entry is indistinguishable from the chosen one.
        for (row, local_index) in remaining_local.iter().enumerate() {
            let Some(chosen) = assignment.get(row).copied().flatten() else {
                continue;
            };
            let chosen_cost = cost[row][chosen];
            let tied: Vec<usize> = (0..remaining_remote.len())
                .filter(|column| {
                    *column != chosen && (cost[row][*column] - chosen_cost).abs() < TIE_EPSILON
                })
                .map(|column| remaining_remote[column])
                .collect();
            if !tied.is_empty() {
                notes.push(format!(
                    "track '{}' ties with {} other remote entr{}",
                    local.tracks[*local_index]
                        .title
                        .as_deref()
                        .unwrap_or(&local.tracks[*local_index].path),
                    tied.len(),
                    if tied.len() == 1 { "y" } else { "ies" }
                ));
            }
        }
    }

    let ambiguous = !notes.is_empty();
    let remote_count = playable.len();
    let assigned = pairs.iter().filter(|pair| pair.is_some()).count();
    let mut complete = assigned == local.tracks.len() && assigned == remote_count;

    let mut unique_targets: Vec<usize> = pairs.iter().filter_map(|pair| *pair).collect();
    unique_targets.sort_unstable();
    unique_targets.dedup();
    if unique_targets.len() != assigned {
        complete = false;
        notes.push("two files were mapped to the same remote track".to_string());
    }

    if remote.has_unsupported_entries() {
        complete = false;
        notes.push(
            "release contains index, unsupported, or structurally incomplete tracklist entries"
                .to_string(),
        );
    }

    if local.tracks.len() != remote_count {
        notes.push(format!(
            "{} local file(s) versus {} playable remote track(s)",
            local.tracks.len(),
            remote_count
        ));
    }

    let pair_similarity = pairs
        .iter()
        .enumerate()
        .map(|(local_index, pair)| {
            let remote_index = (*pair)?;
            Some((remote_index, titles.get(local_index, remote_index)?))
        })
        .collect();

    Mapping {
        pairs,
        complete,
        ambiguous,
        notes,
        pair_similarity,
    }
}

/// Title similarities for one `map_tracks` call. Each title is normalized and
/// cleaned once and each pair is compared at most once, however many passes ask
/// for it. A pair scores the better of the raw and the cleaned reading, as
/// [`track_title_similarity`] does.
struct TitleMatrix<'a> {
    local: &'a LocalRelease,
    remote: &'a RemoteRelease,
    local_chars: Vec<Vec<char>>,
    remote_chars: Vec<Vec<char>>,
    /// The cleaned reading of each local title, worked out once per release.
    cleaned: Vec<Option<CleanedTitle>>,
    cache: Vec<Vec<Option<f64>>>,
}

impl<'a> TitleMatrix<'a> {
    fn new(local: &'a LocalRelease, remote: &'a RemoteRelease) -> Self {
        Self {
            local,
            remote,
            local_chars: local
                .tracks
                .iter()
                .map(|track| normalized_chars(track.title.as_deref().unwrap_or_default()))
                .collect(),
            remote_chars: remote
                .tracklist
                .iter()
                .map(|track| normalized_chars(&track.title))
                .collect(),
            cleaned: cleaned_track_titles(local),
            cache: vec![vec![None; remote.tracklist.len()]; local.tracks.len()],
        }
    }

    /// `None` when the local file has no title.
    fn get(&mut self, local_index: usize, remote_index: usize) -> Option<f64> {
        let title = self.local.tracks[local_index].title.as_deref()?;
        if let Some(known) = self.cache[local_index][remote_index] {
            return Some(known);
        }
        let remote_title = &self.remote.tracklist[remote_index].title;
        let mut value = comparison_similarity(
            title,
            &self.local_chars[local_index],
            remote_title,
            &self.remote_chars[remote_index],
        );
        if let Some(suffixed) = suffix_tolerant_similarity(title, remote_title) {
            value = value.max(suffixed);
        }
        if let Some(cut_off) = cut_off_similarity(title, remote_title) {
            value = value.max(cut_off);
        }
        if let Some(prefix) = id3v1_prefix(title, remote_title) {
            value = value.max(comparison_similarity(
                title,
                &self.local_chars[local_index],
                prefix,
                &normalized_chars(prefix),
            ));
        }
        if let Some(cleaned) = &self.cleaned[local_index] {
            value = value.max(cleaned_title_similarity(cleaned, remote_title));
        }
        self.cache[local_index][remote_index] = Some(value);
        Some(value)
    }
}

fn pair_cost(title_similarity: Option<f64>, local: &LocalTrack, remote: &RemoteTrack) -> f64 {
    let title = title_similarity.unwrap_or(0.0);

    let duration = match (local.duration_secs, remote.duration_secs) {
        (Some(a), Some(b)) => duration_similarity(a, b),
        _ => NEUTRAL_DURATION_SIMILARITY,
    };

    1.0 - (0.75 * title + 0.25 * duration)
}

/// Hungarian algorithm (potentials form) for a rectangular cost matrix.
/// Returns, for each row, the assigned column when one exists.
fn solve_assignment(cost: &[Vec<f64>]) -> Vec<Option<usize>> {
    const INF: f64 = f64::INFINITY;

    let rows = cost.len();
    if rows == 0 {
        return Vec::new();
    }
    let columns = cost[0].len();
    if columns == 0 {
        return vec![None; rows];
    }

    let size = rows.max(columns);
    let mut matrix = vec![vec![0.0f64; size]; size];
    for (i, row) in matrix.iter_mut().enumerate() {
        for (j, value) in row.iter_mut().enumerate() {
            *value = if i < rows && j < columns {
                cost[i][j]
            } else {
                // Dummy rows/columns are free, so the real assignment stays
                // optimal while allowing rectangular inputs.
                0.0
            };
        }
    }

    let mut u = vec![0.0f64; size + 1];
    let mut v = vec![0.0f64; size + 1];
    let mut p = vec![0usize; size + 1];
    let mut way = vec![0usize; size + 1];

    for i in 1..=size {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![INF; size + 1];
        let mut used = vec![false; size + 1];

        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = INF;
            let mut j1 = 0usize;

            for j in 1..=size {
                if used[j] {
                    continue;
                }
                let current = matrix[i0 - 1][j - 1] - u[i0] - v[j];
                if current < minv[j] {
                    minv[j] = current;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                }
            }

            if !delta.is_finite() {
                // No augmenting path; leave the remaining rows unassigned.
                break;
            }

            for j in 0..=size {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }

            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }

        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    let mut result = vec![None; rows];
    for (j, row) in p.iter().enumerate().take(size + 1).skip(1) {
        if *row >= 1 && *row <= rows && j <= columns {
            result[*row - 1] = Some(j - 1);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub album_title: f64,
    pub album_artist: f64,
    pub track_titles: f64,
    pub durations: f64,
    pub year: f64,
    pub total: f64,
}

impl ScoreBreakdown {
    /// Album identity does not depend on a particular pressing's year.
    pub fn album_score(&self) -> f64 {
        (WEIGHT_ALBUM_TITLE * self.album_title
            + WEIGHT_ALBUM_ARTIST * self.album_artist
            + WEIGHT_TRACK_TITLES * self.track_titles
            + WEIGHT_DURATIONS * self.durations)
            / (1.0 - WEIGHT_YEAR)
    }

    /// The album score over the components other than the album title, scaled
    /// so that a perfect rest scores 1.0. Used where the title carries no extra
    /// evidence because every track already agrees.
    pub fn album_score_without_title(&self) -> f64 {
        (WEIGHT_ALBUM_ARTIST * self.album_artist
            + WEIGHT_TRACK_TITLES * self.track_titles
            + WEIGHT_DURATIONS * self.durations)
            / (1.0 - WEIGHT_YEAR - WEIGHT_ALBUM_TITLE)
    }

    /// Component values paired with their weights, for the review screen.
    pub fn components(&self) -> [(&'static str, f64, f64); 5] {
        [
            ("album title", self.album_title, WEIGHT_ALBUM_TITLE),
            ("album artist", self.album_artist, WEIGHT_ALBUM_ARTIST),
            ("track titles", self.track_titles, WEIGHT_TRACK_TITLES),
            ("durations", self.durations, WEIGHT_DURATIONS),
            ("year", self.year, WEIGHT_YEAR),
        ]
    }
}

/// Deterministic ranking score in `0..=1`. Missing components contribute zero;
/// nothing is renormalized into false confidence.
pub fn score(local: &LocalRelease, remote: &RemoteRelease, mapping: &Mapping) -> ScoreBreakdown {
    let album_title = match &local.album_title {
        Some(title) if !title.trim().is_empty() => album_title_similarity(title, &remote.title),
        _ => 0.0,
    };

    let album_artist = album_artist_score(local, remote);

    let track_titles = if local.tracks.is_empty() {
        0.0
    } else {
        let cleaned = cleaned_track_titles(local);
        let mut total = 0.0;
        for index in 0..local.tracks.len() {
            let Some(remote_index) = mapping.pairs.get(index).and_then(|pair| *pair) else {
                continue;
            };
            let Some(remote_track) = remote.tracklist.get(remote_index) else {
                continue;
            };
            if let Some(similarity) =
                mapping.title_similarity(local, &cleaned, index, remote_index, remote_track)
            {
                total += similarity;
            }
        }
        total / local.tracks.len() as f64
    };

    let mut duration_total = 0.0;
    for (local_index, remote_index) in mapping.assigned_pairs() {
        let Some(local_track) = local.tracks.get(local_index) else {
            continue;
        };
        let Some(remote_track) = remote.tracklist.get(remote_index) else {
            continue;
        };
        if let (Some(a), Some(b)) = (local_track.duration_secs, remote_track.duration_secs) {
            duration_total += duration_similarity(a, b);
        }
    }
    // Unknown durations count as zero, so a release with few known durations
    // cannot outscore one whose durations were all measured.
    let durations = if local.tracks.is_empty() {
        0.0
    } else {
        duration_total / local.tracks.len() as f64
    };

    let year = match (local.year, remote.year) {
        (Some(a), Some(b)) if a == b => 1.0,
        (Some(a), Some(b)) if (a - b).abs() == 1 => 0.5,
        _ => 0.0,
    };

    let total = WEIGHT_ALBUM_TITLE * album_title
        + WEIGHT_ALBUM_ARTIST * album_artist
        + WEIGHT_TRACK_TITLES * track_titles
        + WEIGHT_DURATIONS * durations
        + WEIGHT_YEAR * year;

    ScoreBreakdown {
        album_title,
        album_artist,
        track_titles,
        durations,
        year,
        total,
    }
}

fn album_artist_score(local: &LocalRelease, remote: &RemoteRelease) -> f64 {
    let Some(artist) = &local.album_artist else {
        return 0.0;
    };
    if remote.artists.is_empty() {
        return 0.0;
    }

    let provider = remote.provider();
    // Names that normalize to nothing (such as `!!!`) fall back to their
    // original text, with the Discogs disambiguator removed as normalization
    // would have done.
    let comparable = |name: &str| match provider {
        ProviderKind::Discogs => strip_disambiguator(name),
        ProviderKind::MusicBrainz => name.to_string(),
    };
    let local_original = comparable(artist);
    let local_chars: Vec<char> = normalize_artist_for(provider, artist).chars().collect();
    let remote_originals: Vec<String> =
        remote.artists.iter().map(|name| comparable(name)).collect();
    let remote_chars: Vec<Vec<char>> = remote
        .artists
        .iter()
        .map(|name| normalize_artist_for(provider, name).chars().collect())
        .collect();

    let various = matches!(local.artist_kind, ArtistKind::VariousArtists)
        && remote_chars.iter().any(|name| {
            let name: String = name.iter().collect();
            name == "various" || name == "various artists"
        });
    if various {
        return 1.0;
    }

    let joined_original = remote_originals.join(" ");
    let joined_chars: Vec<char> = remote_chars.join(&' ');
    // A 30-character local artist may be a cut-off copy of a longer credit.
    let local_folded = folded_artist_chars(|text| normalize_artist_for(provider, text), artist);
    let compare = |original: &str, chars: &[char]| {
        let mut value = similarity_of_normalized(&local_original, &local_chars, original, chars);
        // `The Sisters of Mercy` and `Sisters of Mercy`, `Belle & Sebastian` and
        // `Belle and Sebastian`: the same artist, at a small cost.
        let remote_folded =
            folded_artist_chars(|text| normalize_artist_for(provider, text), original);
        if local_folded != local_chars || remote_folded != chars {
            value = value.max(
                (similarity_of_normalized(
                    &local_original,
                    &local_folded,
                    original,
                    &remote_folded,
                ) - ARTIST_FOLD_PENALTY)
                    .max(0.0),
            );
        }
        if let Some(prefix) = id3v1_prefix(artist, original) {
            let prefix_chars: Vec<char> = normalize_artist_for(provider, prefix).chars().collect();
            value = value.max(similarity_of_normalized(
                &local_original,
                &local_chars,
                prefix,
                &prefix_chars,
            ));
        }
        if let Some(cut_off) = cut_off_similarity(artist, original) {
            value = value.max(cut_off);
        }
        value
    };
    let mut best = compare(&joined_original, &joined_chars);
    for (original, chars) in remote_originals.iter().zip(&remote_chars) {
        best = best.max(compare(original, chars));
    }
    best
}

// ---------------------------------------------------------------------------
// Candidate ranking and acceptance
// ---------------------------------------------------------------------------

/// Ranks a search summary before spending a detailed release fetch on it.
pub fn rank_search_summary(local: &LocalRelease, summary: &SearchSummary) -> f64 {
    let mut rank = 0.0;

    if let Some(title) = &local.album_title {
        rank += 0.5 * album_title_similarity(title, &summary.title);
    }
    if let Some(artist) = &local.album_artist {
        rank += 0.3 * summary_artist_hint(artist, summary);
    }
    if let (Some(year), Some(remote_year)) = (local.year, summary.year)
        && year == remote_year
    {
        rank += 0.1;
    }
    if let Some(barcode) = &local.barcode {
        let normalized = normalize_barcode(barcode);
        if !normalized.is_empty()
            && summary
                .barcodes
                .iter()
                .any(|candidate| normalize_barcode(candidate) == normalized)
        {
            rank += 0.4;
        }
    }
    if let Some(catalog) = &local.catalog_number {
        let normalized = normalize_identifier(catalog);
        if !normalized.is_empty()
            && summary
                .catalog_numbers
                .iter()
                .any(|candidate| normalize_identifier(candidate) == normalized)
        {
            rank += 0.4;
        }
    }

    rank
}

/// Structured credits win. Discogs search summaries only carry
/// `Artist - Title` strings, so the fallback keeps ranking useful without
/// pretending that string is structured data.
fn summary_artist_hint(local_artist: &str, summary: &SearchSummary) -> f64 {
    if let Some(credit) = summary
        .artist_credit
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        return local_artist_similarity(local_artist, credit);
    }
    if !summary.artists.is_empty() {
        return local_artist_similarity(local_artist, &summary.artists.join(" "));
    }
    let (hint, _) = summary
        .title
        .split_once(" - ")
        .unwrap_or(("", &summary.title));
    local_artist_similarity(local_artist, hint)
}

pub fn normalize_barcode(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

pub fn normalize_identifier(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub release: RemoteRelease,
    pub mapping: Mapping,
    pub score: ScoreBreakdown,
}

impl Candidate {
    pub fn new(local: &LocalRelease, release: RemoteRelease) -> Self {
        let mapping = map_tracks(local, &release);
        let score = score(local, &release, &mapping);
        Self {
            release,
            mapping,
            score,
        }
    }
}

/// One discovery strategy's outcome, kept candidate-specific so an incomplete
/// identifier search cannot be mistaken for a resolved identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyOutcome {
    pub label: String,
    pub attempted: bool,
    pub successful: bool,
    pub complete: bool,
}

impl StrategyOutcome {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            attempted: false,
            successful: false,
            complete: false,
        }
    }
}

/// How candidate discovery went for one local release. Identifier discovery
/// that was cut short, unhydrated summaries, or identifiers that do not separate
/// editions keep a candidate out of automatic acceptance.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryEvidence {
    /// Derived from the richer evidence, never supplied by callers of `evaluate`.
    pub identifier_unique: bool,
    pub identifier_search_truncated: bool,
    pub embedded_release_resolved: bool,
    pub identifier_summary_ids: HashSet<ReleaseId>,
    pub unhydrated_ids: HashSet<ReleaseId>,
    pub failed_ids: HashSet<ReleaseId>,
    pub strategies: Vec<StrategyOutcome>,
    /// Explicit reasons discovery could not settle identity questions.
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchScope {
    Album,
    #[default]
    Edition,
}

impl MatchScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Edition => "edition",
        }
    }

    pub fn score(self, score: &ScoreBreakdown) -> f64 {
        match self {
            Self::Album => score.album_score(),
            Self::Edition => score.total,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub auto_accept: bool,
    #[serde(default)]
    pub match_scope: MatchScope,
    /// Reasons the candidate may not be accepted automatically.
    pub blockers: Vec<String>,
    /// Non-blocking notes, including contradictions shown during review.
    pub warnings: Vec<String>,
    /// Positive evidence supporting the match.
    pub evidence: Vec<String>,
}

impl Evaluation {
    /// Manual acceptance can override score and identity uncertainty, but never
    /// an incomplete or structurally unsupported mapping.
    pub fn can_apply(&self, candidate: &Candidate) -> bool {
        candidate.mapping.complete && !candidate.release.has_unsupported_entries()
    }
}

pub fn evaluate(
    local: &LocalRelease,
    candidate: &Candidate,
    runner_up: Option<&Candidate>,
    discovery: &DiscoveryEvidence,
) -> Evaluation {
    evaluate_at_scope(local, candidate, runner_up, discovery, MatchScope::Edition)
}

/// Prefer a proven edition, otherwise accept strong album/track evidence.
/// Close alternatives must describe equivalent audio, not just score similarly.
pub fn evaluate_for_matching(
    local: &LocalRelease,
    candidate: &Candidate,
    alternatives: &[Candidate],
    discovery: &DiscoveryEvidence,
) -> Evaluation {
    let runner_up = alternatives
        .iter()
        .filter(|other| other.release.id != candidate.release.id)
        .max_by(|a, b| a.score.total.total_cmp(&b.score.total));
    let exact = evaluate(local, candidate, runner_up, discovery);
    if exact.auto_accept {
        return exact;
    }
    let mut album = evaluate_at_scope(local, candidate, None, discovery, MatchScope::Album);
    for other in alternatives {
        if other.release.id != candidate.release.id
            && within_gap(candidate.score.album_score() - other.score.album_score())
            && !(in_same_release_group(candidate, other)
                && !conflicting_recordings(candidate, other))
            && !equivalent_audio(candidate, other)
            && !fits_durations_better(local, candidate, other)
        {
            album.blockers.push(format!(
                "close alternative {} does not establish the same album and recordings",
                other.release.id
            ));
        }
    }
    album.auto_accept = album.blockers.is_empty();
    album
}

/// True when a score lead is too small to separate two alternatives. The
/// boundary itself counts as close.
pub(crate) fn within_gap(diff: f64) -> bool {
    diff <= RUNNER_UP_GAP + TIE_EPSILON
}

/// Track version qualifiers that distinguish recordings. A remastering is a
/// different mastering of the same recording, so it never separates tracks.
fn album_track_qualifiers(title: &str) -> BTreeSet<&'static str> {
    let mut qualifiers = edition_qualifiers(title);
    qualifiers.remove("remastered");
    qualifiers
}

/// The distance in seconds between a local track and the remote track it maps
/// to, or `None` when either duration is unknown.
fn duration_gap(
    local: &LocalRelease,
    candidate: &Candidate,
    local_index: usize,
    remote_index: usize,
) -> Option<f64> {
    let a = local.tracks.get(local_index)?.duration_secs?;
    let b = candidate
        .release
        .tracklist
        .get(remote_index)?
        .duration_secs?;
    (a.is_finite() && b.is_finite()).then(|| (a - b).abs())
}

/// Whether `candidate` is the pressing the local files came from and `other`
/// is not, judged by length alone: with at least [`DURATION_FIT_MIN_TRACKS`]
/// local tracks, every mapped duration of `candidate` is known and within
/// [`DURATION_TOLERANCE_SECS`], while `other` does not map every file or has a
/// known mapped duration beyond that. An alternative whose durations also agree
/// is a retitled or re-credited edition and is not set aside, and neither is
/// one that only maps ambiguously: that mapping is still one to one.
fn fits_durations_better(local: &LocalRelease, candidate: &Candidate, other: &Candidate) -> bool {
    if local.tracks.len() < DURATION_FIT_MIN_TRACKS
        || !candidate.mapping.complete
        || candidate.mapping.ambiguous
    {
        return false;
    }
    let candidate_fits = candidate.mapping.assigned_pairs().all(|(l, r)| {
        duration_gap(local, candidate, l, r).is_some_and(|gap| gap <= DURATION_TOLERANCE_SECS)
    });
    if !candidate_fits {
        return false;
    }
    let other_misses = other.mapping.assigned_pairs().any(|(l, r)| {
        duration_gap(local, other, l, r).is_some_and(|gap| gap > DURATION_TOLERANCE_SECS)
    });
    !other.mapping.complete || other_misses
}

/// Whether two candidates are editions the provider files under one album: the
/// same MusicBrainz release group or Discogs master.
///
/// An album-level match asks which album a folder is, so another edition of the
/// same album cannot contradict it, however many bonus tracks it adds or how it
/// spells a title. Only a close alternative from a different album can.
fn in_same_release_group(a: &Candidate, b: &Candidate) -> bool {
    a.release.provider() == b.release.provider()
        && matches!(
            (&a.release.group_id, &b.release.group_id),
            (Some(x), Some(y)) if x == y
        )
}

/// Whether any local track maps to a different known recording in each
/// candidate, as a remaster or re-recording would.
///
/// Those editions share an album but not the audio, so which one a folder holds
/// cannot be settled from the album alone.
fn conflicting_recordings(a: &Candidate, b: &Candidate) -> bool {
    a.mapping
        .assigned_pairs()
        .any(|(local_index, remote_index)| {
            let Some(other_index) = b.mapping.pairs.get(local_index).and_then(|pair| *pair) else {
                return false;
            };
            matches!(
                (
                    a.release.tracklist[remote_index].recording_id,
                    b.release.tracklist[other_index].recording_id,
                ),
                (Some(x), Some(y)) if x != y
            )
        })
}

fn equivalent_audio(a: &Candidate, b: &Candidate) -> bool {
    if !b.mapping.complete || b.mapping.ambiguous || b.release.has_unsupported_entries() {
        return false;
    }
    if a.release.group_id.is_some()
        && b.release.group_id.is_some()
        && a.release.group_id != b.release.group_id
    {
        return false;
    }
    if normalize_text(&a.release.title) != normalize_text(&b.release.title)
        || normalize_artist_for(a.release.provider(), &a.release.artist_display())
            != normalize_artist_for(b.release.provider(), &b.release.artist_display())
        || remote_edition_qualifiers(&a.release) != remote_edition_qualifiers(&b.release)
        || a.mapping.pairs.len() != b.mapping.pairs.len()
    {
        return false;
    }
    a.mapping
        .assigned_pairs()
        .all(|(local_index, remote_index)| {
            let Some(other_index) = b.mapping.pairs.get(local_index).and_then(|pair| *pair) else {
                return false;
            };
            let x = &a.release.tracklist[remote_index];
            let y = &b.release.tracklist[other_index];
            let x_artist = x.artist_credit.clone().unwrap_or_else(|| {
                if x.artists.is_empty() {
                    a.release.artist_display()
                } else {
                    x.artist_display()
                }
            });
            let y_artist = y.artist_credit.clone().unwrap_or_else(|| {
                if y.artists.is_empty() {
                    b.release.artist_display()
                } else {
                    y.artist_display()
                }
            });
            // One shared recording is the same performance whatever an edition
            // calls the artist (`Covenant` became `The Kovenant` on a reissue).
            let same_recording =
                matches!((x.recording_id, y.recording_id), (Some(x), Some(y)) if x == y);
            normalize_text(&x.title) == normalize_text(&y.title)
                && (same_recording
                    || normalize_artist_for(a.release.provider(), &x_artist)
                        == normalize_artist_for(b.release.provider(), &y_artist))
                && matches!((x.duration_secs, y.duration_secs), (Some(x), Some(y))
                if x.is_finite() && y.is_finite() && (x-y).abs() <= DURATION_TOLERANCE_SECS)
                && !matches!((x.recording_id, y.recording_id), (Some(x), Some(y)) if x != y)
        })
}

/// Whether an album title that falls short of [`TITLE_SIMILARITY_THRESHOLD`]
/// is outweighed by the rest of the album: the title is at least
/// [`CORROBORATED_TITLE_FLOOR`], the artist agrees, every local file maps one to
/// one with every duration within [`CORROBORATED_DURATION_SECS`], and the track
/// titles and the rest of the score are strong. The title then adds no
/// evidence, so it no longer gates the album. The release needs at least
/// [`CORROBORATED_MIN_TRACKS`] local tracks, or
/// [`CORROBORATED_MIN_VARIOUS_TRACKS`] for Various Artists.
fn corroborated_album(local: &LocalRelease, candidate: &Candidate) -> bool {
    let score = &candidate.score;
    let min_tracks = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        CORROBORATED_MIN_VARIOUS_TRACKS
    } else {
        CORROBORATED_MIN_TRACKS
    };
    if local.tracks.len() < min_tracks
        || score.album_title < CORROBORATED_TITLE_FLOOR
        || score.album_title >= TITLE_SIMILARITY_THRESHOLD
        || score.album_artist < TITLE_SIMILARITY_THRESHOLD
        || score.track_titles < CORROBORATED_MEAN_TITLE
        || score.album_score_without_title() + 1e-9 < CORROBORATED_SCORE
        || !candidate.mapping.complete
        || candidate.mapping.ambiguous
        || local.tracks.is_empty()
        || candidate.mapping.assigned_count() != local.tracks.len()
    {
        return false;
    }
    candidate.mapping.assigned_pairs().all(|(l, r)| {
        duration_gap(local, candidate, l, r).is_some_and(|gap| gap <= CORROBORATED_DURATION_SECS)
    })
}

fn evaluate_at_scope(
    local: &LocalRelease,
    candidate: &Candidate,
    runner_up: Option<&Candidate>,
    discovery: &DiscoveryEvidence,
    match_scope: MatchScope,
) -> Evaluation {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();

    let remote = &candidate.release;
    let provider = remote.provider();
    let minimum_score = SCORE_THRESHOLD;
    let minimum_title_similarity = TITLE_SIMILARITY_THRESHOLD;

    let embedded_consistent = match &remote.id {
        ReleaseId::Discogs(id) => local.embedded_discogs_id == Some(*id),
        ReleaseId::MusicBrainz(id) => local.embedded_evidence.release_id == Some(*id),
    };

    if !candidate.mapping.complete {
        blockers.push("track mapping is incomplete".to_string());
    }
    if remote.has_unsupported_entries() {
        blockers.push(
            "release contains index, unsupported, or structurally incomplete tracklist entries that cannot be flattened"
                .to_string(),
        );
    }
    if candidate.mapping.ambiguous {
        blockers.push("track mapping has indistinguishable alternatives".to_string());
    }

    // A weak album title outweighed by tracks and durations that all agree.
    // Album scope only.
    let corroborated_title =
        match_scope == MatchScope::Album && corroborated_album(local, candidate);

    let acceptance_score = match_scope.score(&candidate.score);
    if !corroborated_title && acceptance_score + 1e-9 < minimum_score {
        blockers.push(format!(
            "score {:.3} is below the {:.2} acceptance threshold",
            acceptance_score, minimum_score
        ));
    }

    if corroborated_title {
        warnings.push(format!(
            "album title scores {:.3} but every track and duration agrees",
            candidate.score.album_title
        ));
    } else if match_scope == MatchScope::Album
        && (candidate.score.album_title < minimum_title_similarity
            || candidate.score.album_artist < minimum_title_similarity)
    {
        blockers.push("album title or artist does not agree closely enough".to_string());
    }

    if let Some(runner_up) = runner_up {
        let gap = candidate.score.total - runner_up.score.total;
        if within_gap(gap) && !embedded_consistent {
            blockers.push(format!(
                "runner-up release {} is only {:.3} behind",
                runner_up.release.id, gap
            ));
        }
    }

    // Per-track evidence.
    let cleaned = cleaned_track_titles(local);
    let mut worst_title = 1.0f64;
    let mut worst_title_track = String::new();
    let mut worst_title_duration_gap: Option<f64> = None;
    let mut titles_below_minimum = 0usize;
    let mut worst_duration: Option<f64> = None;
    let mut worst_duration_track = String::new();
    let mut tracks_over_tolerance = 0usize;
    let (mut local_total, mut remote_total) = (0.0f64, 0.0f64);

    for (local_index, remote_index) in candidate.mapping.assigned_pairs() {
        let Some(local_track) = local.tracks.get(local_index) else {
            continue;
        };
        let Some(remote_track) = remote.tracklist.get(remote_index) else {
            continue;
        };

        if match_scope == MatchScope::Album {
            if local_track
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty())
                || !matches!((local_track.duration_secs, remote_track.duration_secs),
                    (Some(a), Some(b)) if a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0)
            {
                blockers.push(
                    "album matching requires a title and known duration for every track"
                        .to_string(),
                );
            }
            if album_track_qualifiers(local_track.title.as_deref().unwrap_or_default())
                != album_track_qualifiers(&remote_track.title)
            {
                blockers.push(format!(
                    "track version qualifiers differ for '{}'",
                    remote_track.title
                ));
            }
            if matches!((local_track.recording_id, remote_track.recording_id),
                (Some(a), Some(b)) if a != b)
                || matches!((local_track.release_track_id, remote_track.release_track_id),
                    (Some(a), Some(b)) if a != b)
            {
                blockers.push(
                    "observed track identifiers contradict the proposed recording".to_string(),
                );
            }
        }

        if let Some(score) = candidate.mapping.title_similarity(
            local,
            &cleaned,
            local_index,
            remote_index,
            remote_track,
        ) {
            if score < minimum_title_similarity {
                titles_below_minimum += 1;
            }
            if score < worst_title {
                worst_title = score;
                worst_title_track = remote_track.title.clone();
                worst_title_duration_gap =
                    match (local_track.duration_secs, remote_track.duration_secs) {
                        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => Some((a - b).abs()),
                        _ => None,
                    };
            }
        }

        if let (Some(a), Some(b)) = (local_track.duration_secs, remote_track.duration_secs) {
            let difference = (a - b).abs();
            if difference > DURATION_TOLERANCE_SECS {
                tracks_over_tolerance += 1;
            }
            if a.is_finite() && b.is_finite() {
                local_total += a;
                remote_total += b;
            }
            if worst_duration.is_none_or(|worst| difference > worst) {
                worst_duration = Some(difference);
                worst_duration_track = remote_track.title.clone();
            }
        }
    }

    // One title off by a little, with the right length, is a spelling variant of
    // the same recording rather than a different track. A title off by a lot,
    // on a track whose length agrees closely, in an album of at least five where
    // every other track already agrees, is one mis-tagged or differently named
    // track of the same album: by elimination it is the remaining track. Only for
    // an album-level match. These exceptions do not stack with the weak album
    // title rule: when that rule is in play, which replaces the acceptance score
    // gate, any track title below the minimum blocks.
    let lone_weak_title =
        match_scope == MatchScope::Album && !corroborated_title && titles_below_minimum == 1;
    let near_miss_title = lone_weak_title
        && worst_title >= NEAR_MISS_TITLE_FLOOR
        && worst_title_duration_gap.is_some_and(|gap| gap <= DURATION_TOLERANCE_SECS);
    let duration_confirmed_title = lone_weak_title
        && local.tracks.len() >= LONE_TRACK_MIN_ALBUM_TRACKS
        && worst_title_duration_gap.is_some_and(|gap| gap <= LONE_TRACK_DURATION_GAP_SECS);
    if worst_title < minimum_title_similarity {
        if near_miss_title || duration_confirmed_title {
            warnings.push(format!(
                "track title '{worst_title_track}' scores {worst_title:.3} but its duration agrees"
            ));
        } else {
            blockers.push(format!(
                "track title '{worst_title_track}' only scores {worst_title:.3} against the local file"
            ));
        }
    }

    if let Some(difference) = worst_duration
        && difference > DURATION_TOLERANCE_SECS
    {
        // A different master or a trailing silence moves a track or two by a few
        // seconds without changing the album, which shows in the summed length.
        // Only for an album-level match, never for an exact edition.
        let drifted_album = match_scope == MatchScope::Album
            && local.tracks.len() >= SOFT_DURATION_MIN_ALBUM_TRACKS
            && tracks_over_tolerance <= SOFT_DURATION_MAX_TRACKS
            && difference <= SOFT_DURATION_MAX_SECS
            && remote_total > 0.0
            && (local_total - remote_total).abs() / remote_total <= SOFT_DURATION_TOTAL_RATIO;
        if drifted_album {
            warnings.push(format!(
                "track '{worst_duration_track}' differs by {difference:.1}s from the {provider} duration but the album length agrees"
            ));
        } else {
            blockers.push(format!(
                "track '{worst_duration_track}' differs by {difference:.1}s from the {provider} duration"
            ));
        }
    }

    // Embedded identifier contradictions. Provider IDs are only ever compared
    // within their own namespace.
    if provider == ProviderKind::MusicBrainz {
        for problem in &local.embedded_evidence.problems {
            blockers.push(problem.clone());
        }
        for track in &local.tracks {
            for problem in &track.identifier_problems {
                blockers.push(problem.clone());
            }
        }
    }

    match &remote.id {
        ReleaseId::Discogs(id) => {
            if let Some(embedded) = local.embedded_discogs_id {
                if embedded == *id {
                    evidence.push(format!("embedded Discogs release ID {embedded} matches"));
                } else {
                    blockers.push(format!(
                        "embedded Discogs release ID {embedded} conflicts with release {id}"
                    ));
                }
            }
        }
        ReleaseId::MusicBrainz(id) => {
            if let Some(embedded) = local.embedded_evidence.release_id {
                if embedded == *id {
                    evidence.push(format!(
                        "embedded MusicBrainz release ID {embedded} matches"
                    ));
                } else {
                    blockers.push(format!(
                        "embedded MusicBrainz release ID {embedded} conflicts with release {id}"
                    ));
                }
            }
            if let (Some(observed), Some(GroupId::MusicBrainzReleaseGroup(remote_group))) =
                (local.embedded_evidence.release_group_id, &remote.group_id)
            {
                if observed == *remote_group {
                    evidence.push(format!("observed release group {observed} matches"));
                } else {
                    blockers.push(format!(
                        "observed release group {observed} conflicts with release group {remote_group}"
                    ));
                }
            }
        }
    }

    let mut identifier_matched = false;
    if let Some(barcode) = &local.barcode {
        let local_digits = normalize_barcode(barcode);
        let remote_digits: Vec<String> = remote
            .barcodes
            .iter()
            .map(|b| normalize_barcode(b))
            .collect();
        if !local_digits.is_empty() && !remote_digits.is_empty() {
            if remote_digits.contains(&local_digits) {
                identifier_matched = true;
                evidence.push(format!("barcode {barcode} matches the release"));
            } else {
                blockers.push(format!(
                    "barcode {barcode} conflicts with release {}",
                    remote.id
                ));
            }
        }
    }

    if let Some(catalog) = &local.catalog_number {
        let local_key = normalize_identifier(catalog);
        let remote_keys: Vec<String> = remote
            .catalog_numbers
            .iter()
            .map(|value| normalize_identifier(value))
            .collect();
        if !local_key.is_empty() && !remote_keys.is_empty() {
            if remote_keys.contains(&local_key) {
                identifier_matched = true;
                evidence.push(format!("catalog number {catalog} matches the release"));
            } else {
                blockers.push(format!(
                    "catalog number {catalog} conflicts with release {}",
                    remote.id
                ));
            }
        }
    }

    // Edition qualifiers must agree; they change what the release is.
    if let Some(local_title) = &local.album_title {
        let local_qualifiers = edition_qualifiers(local_title);
        let remote_qualifiers = remote_edition_qualifiers(remote);
        if local_qualifiers != remote_qualifiers {
            blockers.push(format!(
                "edition qualifiers differ: local {:?} versus {provider} {:?}",
                local_qualifiers, remote_qualifiers
            ));
        }
    }

    // Evidence identifying this specific release rather than an edition twin.
    // A release group or an explicitly entered ID never counts on its own.
    if match_scope == MatchScope::Album {
        warnings.push("album and tracks matched; exact edition unconfirmed".to_string());
    } else if embedded_consistent {
        // Already recorded as evidence above.
    } else if discovery.identifier_unique && identifier_matched {
        evidence.push("barcode or catalog number uniquely identifies this release".to_string());
    } else {
        blockers.push(
            "no embedded ID, barcode, or catalog number distinguishes this edition".to_string(),
        );
    }

    if match_scope == MatchScope::Edition && discovery.identifier_search_truncated {
        blockers.push("identifier search results were truncated".to_string());
    }

    if match_scope == MatchScope::Edition {
        blockers.extend(discovery.blockers.iter().cloned());
    }

    if local.is_compilation {
        warnings.push("local release looks like a compilation".to_string());
    }
    if !local.has_album_tag {
        warnings.push("local release has no album title tag".to_string());
    }
    if remote.year.is_none() {
        warnings.push(format!("{provider} release has no year"));
    }

    Evaluation {
        auto_accept: blockers.is_empty(),
        match_scope,
        blockers,
        warnings,
        evidence,
    }
}

/// Primary score for judging whether a primary candidate is plausible enough
/// to guard a fallback. Durations a release never published are unknown, not
/// wrong, so they count as full agreement here; known mismatches still cost.
fn plausibility_score(local: &LocalRelease, primary: &Candidate) -> f64 {
    if local.tracks.is_empty() {
        return primary.score.total;
    }
    let unknown = local
        .tracks
        .iter()
        .enumerate()
        .filter(|(index, track)| {
            let remote_duration = primary
                .mapping
                .pairs
                .get(*index)
                .and_then(|pair| *pair)
                .and_then(|remote_index| primary.release.tracklist.get(remote_index))
                .and_then(|remote| remote.duration_secs);
            track.duration_secs.is_none() || remote_duration.is_none()
        })
        .count();
    primary.score.total + WEIGHT_DURATIONS * unknown as f64 / local.tracks.len() as f64
}

/// Cross-provider checks applied before automatic fallback acceptance.
///
/// Scores, runner-up gaps, and mappings stay within one provider. This function
/// only asks whether the fallback contradicts the local edition or a complete,
/// plausible primary candidate.
pub fn fallback_conflicts(
    local: &LocalRelease,
    primary_candidates: &[Candidate],
    fallback: &Candidate,
    embedded_release_resolved: bool,
) -> Vec<String> {
    let mut conflicts = Vec::new();

    for problem in &local.embedded_evidence.problems {
        conflicts.push(problem.clone());
    }

    if local.embedded_evidence.release_id.is_some() && !embedded_release_resolved {
        conflicts.push(
            "an observed MusicBrainz release ID was not resolved, so another provider cannot settle the edition"
                .to_string(),
        );
    }

    let fallback_qualifiers = remote_edition_qualifiers(&fallback.release);
    let mut fallback_durations: HashMap<usize, f64> = HashMap::new();
    let mut fallback_titles: HashMap<usize, String> = HashMap::new();
    for (local_index, remote_index) in fallback.mapping.assigned_pairs() {
        if let Some(track) = fallback.release.tracklist.get(remote_index) {
            fallback_titles.insert(local_index, track.title.clone());
            if let Some(duration) = track.duration_secs {
                fallback_durations.insert(local_index, duration);
            }
        }
    }

    for primary in primary_candidates {
        if !primary.mapping.complete
            || plausibility_score(local, primary) + 1e-9 < PLAUSIBLE_PRIMARY_SCORE
        {
            continue;
        }
        let primary_qualifiers = remote_edition_qualifiers(&primary.release);
        if primary_qualifiers != fallback_qualifiers {
            conflicts.push(format!(
                "primary candidates describe {:?} but the fallback describes {:?}",
                primary_qualifiers, fallback_qualifiers
            ));
        }
        for (local_index, remote_index) in primary.mapping.assigned_pairs() {
            let Some(primary_track) = primary.release.tracklist.get(remote_index) else {
                continue;
            };
            if let Some(fallback_title) = fallback_titles.get(&local_index) {
                let similarity = similarity(&primary_track.title, fallback_title);
                if similarity < TITLE_SIMILARITY_THRESHOLD {
                    conflicts.push(format!(
                        "primary and fallback track titles differ for local track {} ('{}' versus '{}')",
                        local_index + 1,
                        primary_track.title,
                        fallback_title
                    ));
                }
            }
            if let (Some(primary_duration), Some(fallback_duration)) = (
                primary_track.duration_secs,
                fallback_durations.get(&local_index).copied(),
            ) && (primary_duration - fallback_duration).abs() > DURATION_TOLERANCE_SECS
            {
                conflicts.push(format!(
                    "primary and fallback durations differ for local track {}",
                    local_index + 1
                ));
            }
        }
    }

    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_release(titles: &[&str]) -> LocalRelease {
        let tracks = titles
            .iter()
            .enumerate()
            .map(|(index, title)| LocalTrack {
                id: index as i64 + 1,
                path: format!("/music/album/{:02} {}.flac", index + 1, title),
                title: Some((*title).to_string()),
                disc: None,
                track_number: Some(index as i64 + 1),
                duration_secs: Some(180.0 + index as f64),
                release_track_id: None,
                recording_id: None,
                identifier_problems: Vec::new(),
            })
            .collect();

        LocalRelease {
            id: 1,
            directory: "/music/album".to_string(),
            album_title: Some("Test Album".to_string()),
            album_artist: Some("Test Artist".to_string()),
            artist_kind: ArtistKind::AlbumArtist,
            year: Some(2001),
            barcode: None,
            catalog_number: None,
            embedded_discogs_id: None,
            embedded_evidence: EmbeddedReleaseEvidence::default(),
            is_compilation: false,
            has_album_tag: true,
            revision: "rev-1".to_string(),
            tracks,
        }
    }

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    fn remote_release(titles: &[&str]) -> RemoteRelease {
        RemoteRelease {
            id: ReleaseId::Discogs(42),
            group_id: None,
            title: "Test Album".to_string(),
            artists: vec!["Test Artist".to_string()],
            year: Some(2001),
            country: Some("US".to_string()),
            labels: vec!["Test Label".to_string()],
            catalog_numbers: vec!["CAT-1".to_string()],
            formats: vec!["CD".to_string()],
            barcodes: vec!["0123456789012".to_string()],
            tracklist: titles
                .iter()
                .enumerate()
                .map(|(index, title)| RemoteTrack {
                    position: Some(format!("{}", index + 1)),
                    title: (*title).to_string(),
                    artists: Vec::new(),
                    duration_secs: Some(180.0 + index as f64),
                    entry_type: TrackEntryType::Track,
                    medium_position: None,
                    track_position: None,
                    release_track_id: None,
                    recording_id: None,
                    artist_credit: None,
                })
                .collect(),
            url: "https://www.discogs.com/release/42".to_string(),
            lookup_aliases: Vec::new(),
            artist_credit: None,
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn normalizes_case_punctuation_and_whitespace() {
        assert_eq!(normalize_text("  The   BEATLES!! "), "the beatles");
        assert_eq!(normalize_text("AC/DC"), "ac dc");
        assert_eq!(similarity("Sigur Rós", "sigur ros"), 1.0);
    }

    #[test]
    fn apostrophes_join_letters_for_comparison_but_not_for_normalization() {
        // A tag without the apostrophe and either apostrophe style in the
        // dump are the same words.
        assert_eq!(
            similarity("I Dont Want to Grow Up", "I Don't Want to Grow Up"),
            1.0
        );
        assert_eq!(
            similarity("I Dont Want to Grow Up", "I Don\u{2019}t Want to Grow Up"),
            1.0
        );
        assert_eq!(similarity("Rock 'n' Roll", "Rock n Roll"), 1.0);
        assert_eq!(similarity("Don`t", "Dont"), 1.0);
        // A tag that spells the break as a space still matches the apostrophe.
        assert_eq!(similarity("Jo anna", "Jo'anna"), 1.0);
        assert_eq!(similarity("Jo'anna", "Jo anna"), 1.0);
        // Different words stay different.
        assert!(similarity("Dont Stop", "Dont Go") < 0.7);
        // Search indexes and album keys still see the apostrophe as a break.
        assert_eq!(normalize_text("Don't"), "don t");
    }

    #[test]
    fn an_ampersand_reads_as_and_in_comparison_but_not_in_normalization() {
        assert_eq!(similarity("Seek & Destroy", "Seek And Destroy"), 1.0);
        assert_eq!(similarity("Rock & Roll Girl", "Rock and Roll Girl"), 1.0);
        assert_eq!(similarity("Hits&Misses", "Hits and Misses"), 1.0);
        // A tag that wrote the break as a space still matches the `&`.
        assert_eq!(similarity("Black White", "Black & White"), 1.0);
        assert_eq!(similarity("A&B", "A B"), 1.0);
        // Different words stay different.
        assert!(similarity("Black & Blue", "Black & White") < 0.8);
        // Search indexes and album keys still treat `&` as a break.
        assert_eq!(normalize_text("Seek & Destroy"), "seek destroy");
    }

    #[test]
    fn a_full_width_local_value_may_be_a_cut_off_copy() {
        let remote = "The Mouth Licking What You\u{2019}ve Bled";
        let cut = "the mouth licking what you've ";
        assert_eq!(cut.chars().count(), ID3V1_FIELD_CHARS);
        assert!(similarity(cut, remote) < 0.9);
        assert_eq!(local_similarity(cut, remote), 1.0);

        // Cut mid-word, as old taggers did.
        let mid_word = "Zion I-Sunshine, Beats, and Rh";
        assert_eq!(mid_word.chars().count(), ID3V1_FIELD_CHARS);
        assert_eq!(
            local_similarity(mid_word, "Zion I-Sunshine, Beats, and Rhymes"),
            1.0
        );
    }

    #[test]
    fn the_cut_off_allowance_does_not_make_other_values_match() {
        // Padded to the field width but complete: still nowhere near a match.
        let padded = format!("{:<30}", "Intro");
        assert_eq!(padded.chars().count(), ID3V1_FIELD_CHARS);
        let remote = "Intro: A Much Longer Title That Goes On";
        assert!(local_similarity(&padded, remote) < 0.3);

        // Shorter than a field: complete as written, never treated as cut.
        let short = "Zion I-Sunshine, Beats, and R";
        assert_eq!(short.chars().count(), ID3V1_FIELD_CHARS - 1);
        let remote = "Zion I-Sunshine, Beats, and Rhymes";
        assert_eq!(local_similarity(short, remote), similarity(short, remote));

        // A full-width value that differs inside the first 30 characters.
        let other = "Zion I-Moonlight, Beats, and R";
        assert_eq!(other.chars().count(), ID3V1_FIELD_CHARS);
        assert!(local_similarity(other, remote) < 0.9);

        // A remote value no longer than a field has no prefix to compare.
        assert_eq!(id3v1_prefix(&padded, "Intro"), None);
        assert_eq!(id3v1_prefix(&padded, &"x".repeat(30)), None);
        assert_eq!(
            id3v1_prefix(&padded, &"x".repeat(31)),
            Some("x".repeat(30).as_str())
        );
    }

    #[test]
    fn the_prefix_is_cut_on_a_character_boundary() {
        let remote = format!("{}\u{00E9}\u{00E9}", "a".repeat(29));
        assert_eq!(remote.chars().count(), 31);
        let local = format!("{}\u{00E9}", "a".repeat(29));
        assert_eq!(id3v1_prefix(&local, &remote), Some(local.as_str()));
    }

    #[test]
    fn a_title_cut_after_a_word_matches_its_full_form() {
        // 29 characters: the trailing space of the 30-character field was lost.
        let cut = "Detailed Instructions For The";
        assert_eq!(cut.chars().count(), 29);
        let value = local_similarity(cut, "Detailed Instructions For The Self Involved");
        assert!((0.96..1.0).contains(&value), "{value}");

        let cut = "Three Years Two Months Eleven";
        assert_eq!(cut.chars().count(), 29);
        let value = album_title_similarity(cut, "Three Years Two Months Eleven Days");
        assert!((0.96..1.0).contains(&value), "{value}");
    }

    #[test]
    fn a_cut_off_title_that_lost_its_punctuation_still_matches() {
        // Exactly 30 characters, but the provider's first 30 characters include
        // the punctuation the local copy lost, so the raw prefix is misaligned.
        let cut = "Schools Out Surfs Up Lets Fall";
        assert_eq!(cut.chars().count(), 30);
        let remote = "School's Out, Surf's Up, Let's Fall in Love";
        let value = album_title_similarity(cut, remote);
        assert!((0.96..1.0).contains(&value), "{value}");
    }

    #[test]
    fn a_cut_off_reading_needs_a_real_prefix() {
        let cut = "Detailed Instructions For The";
        // A different word, a different spelling, or a cut inside a word at 29
        // characters are not the same text continued.
        for remote in [
            "Detailed Instructions For Them All And More",
            "Detailed Instruction For The Self Involved",
            "Detailed Instructions For The",
        ] {
            if remote == cut {
                assert_eq!(local_similarity(cut, remote), 1.0);
            } else {
                assert!(
                    cut_off_similarity(cut, remote).is_none(),
                    "{remote:?} read as a cut-off"
                );
            }
        }
        // A 28-character value is complete as written, even when it is a prefix.
        let short = "The Mind Is A Terrible Thing";
        assert_eq!(short.chars().count(), 28);
        let remote = "The Mind Is A Terrible Thing To Taste";
        assert_eq!(local_similarity(short, remote), similarity(short, remote));
        // A padded short value has too little text to be a cut-off.
        let padded = format!("{:<29}", "Intro");
        assert_eq!(
            cut_off_similarity(&padded, "Intro And A Much Longer Title"),
            None
        );
    }

    #[test]
    fn a_cut_off_track_title_matches_in_the_mapping() {
        let cut = "Alpha Bravo Charlie Delta Fox";
        assert_eq!(cut.chars().count(), 29);
        let titles = ["One", "Two", "Three", "Four", "Five", cut];
        let local = local_release(&titles);
        let remote = remote_release(&[
            "One",
            "Two",
            "Three",
            "Four",
            "Five",
            "Alpha Bravo Charlie Delta Fox Trot Band",
        ]);
        let candidate = Candidate::new(&local, remote.clone());
        let value = candidate
            .mapping
            .title_similarity(
                &local,
                &cleaned_track_titles(&local),
                5,
                5,
                &remote.tracklist[5],
            )
            .unwrap();
        assert!((0.96..1.0).contains(&value), "{value}");
    }

    #[test]
    fn a_cut_off_artist_matches_its_full_credit() {
        let cut = "Alpha Bravo Charlie Delta Fox";
        assert_eq!(cut.chars().count(), 29);
        let mut local = local_release(&["One"]);
        local.album_artist = Some(cut.to_string());
        let mut remote = remote_release(&["One"]);
        remote.artists = vec!["Alpha Bravo Charlie Delta Fox Trot Band".to_string()];
        let value = album_artist_score(&local, &remote);
        assert!((0.96..1.0).contains(&value), "{value}");

        remote.artists = vec!["Alpha Bravo Charlie Delta Foxes Band".to_string()];
        assert!(album_artist_score(&local, &remote) < 0.96);
    }

    #[test]
    fn a_leading_the_and_an_ampersand_fold_away_at_a_small_cost() {
        for (local, remote) in [
            ("Sisters Of Mercy", "The Sisters of Mercy"),
            ("The Sisters Of Mercy", "Sisters of Mercy"),
            ("Belle And Sebastian", "Belle & Sebastian"),
            ("Jesus and Mary Chain", "The Jesus And Mary Chain"),
        ] {
            let value = local_artist_similarity(local, remote);
            assert!(
                (0.97..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
        // Different artists stay different, and a name that is only `The`
        // does not fold to nothing.
        assert!(local_artist_similarity("Sisters Of Mercy", "The Sisterhood") < 0.8);
        assert!(local_artist_similarity("The", "Mercy") < 0.5);
        assert_eq!(local_artist_similarity("The Who", "The Who"), 1.0);
    }

    #[test]
    fn the_album_artist_score_folds_the_article_for_either_provider() {
        let mut local = local_release(&["One", "Two"]);
        local.album_artist = Some("Sisters Of Mercy".to_string());
        let mut remote = remote_release(&["One", "Two"]);
        remote.artists = vec!["The Sisters of Mercy".to_string()];
        let value = album_artist_score(&local, &remote);
        assert!((0.97..1.0).contains(&value), "{value}");

        local.album_artist = Some("Different Band".to_string());
        assert!(album_artist_score(&local, &remote) < 0.6);
    }

    #[test]
    fn a_subtitle_present_on_one_side_costs_a_little_instead_of_blocking() {
        for (local, remote) in [
            (
                "Tell Me What You Want",
                "Tell Me What You Want (I'll Be Anything)",
            ),
            ("Pinhead [Ramones]", "Pinhead"),
            ("Gypsy (bonus)", "Gypsy"),
            ("Song", "Song feat. Someone"),
            ("Song ft. Someone", "Song"),
            ("Interlude", "Interlude, Part 2"),
            ("Goop All Over My Phone (Pleasa", "Goop All Over My Phone"),
        ] {
            let value = local_similarity(local, remote);
            assert!(
                (0.9..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
    }

    #[test]
    fn versions_and_different_subtitles_are_never_folded_away() {
        // A version is part of the recording.
        for (local, remote) in [
            ("Song", "Song (live)"),
            ("Song", "Song (Remix)"),
            ("Song", "Song (Radio Edit)"),
            ("Song", "Song (Instrumental)"),
            ("Song", "Song (Clean)"),
            ("Song", "Song (2009 Remastered)"),
            ("Song", "Song [Acoustic]"),
        ] {
            assert!(
                local_similarity(local, remote) < 0.9,
                "{local:?} {remote:?}"
            );
        }
        // Two different subtitles are two different titles.
        assert!(suffix_tolerant_similarity("Intro (Part 1)", "Intro (Part 2)").is_none());
        assert!(suffix_tolerant_similarity("Intro, Part 1", "Intro, Part 2").is_none());
        // Neither side has a suffix to remove.
        assert!(suffix_tolerant_similarity("One", "Two").is_none());
        // A title that is only a suffix is left whole.
        assert_eq!(strip_title_suffix("(Intro)"), None);
        assert_eq!(strip_title_suffix("Song (live)"), None);
        assert_eq!(strip_title_suffix("Song (Part 1) Reprise"), None);
        assert_eq!(strip_title_suffix("Song (demo"), None);
    }

    #[test]
    fn title_suffixes_are_found_where_beets_finds_them() {
        assert_eq!(
            strip_title_suffix("Song (Something)").as_deref(),
            Some("Song")
        );
        assert_eq!(
            strip_title_suffix("Song [Something]").as_deref(),
            Some("Song")
        );
        assert_eq!(
            strip_title_suffix("Song (Something").as_deref(),
            Some("Song")
        );
        assert_eq!(
            strip_title_suffix("Song feat. Someone").as_deref(),
            Some("Song")
        );
        assert_eq!(
            strip_title_suffix("Song (feat. Someone)").as_deref(),
            Some("Song")
        );
        assert_eq!(strip_title_suffix("Song, Part II").as_deref(), Some("Song"));
        assert_eq!(strip_title_suffix("Song Pt. 3").as_deref(), Some("Song"));
        assert_eq!(strip_title_suffix("Perfect Song"), None);
        assert_eq!(strip_title_suffix("Shift"), None);
    }

    #[test]
    fn numbered_release_types_volume_words_and_spacing_do_not_change_the_album() {
        for (local, remote) in [
            ("These Wooden Ideas CDS2", "These Wooden Ideas"),
            ("Another Pearl CDS2", "Another Pearl"),
            ("Love & Basketball ST", "Love & Basketball"),
            ("Some Record Aust", "Some Record"),
            ("Some Record Australian", "Some Record"),
            ("Incomplete Crap Vol. 2", "Incomplete Crap, Volume 2"),
            ("Analogue Bubblebath 3", "Analogue Bubblebath Vol. 3"),
            ("Maximum Rock 'n' Roll", "Maximum Rocknroll"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
    }

    #[test]
    fn a_shortened_album_title_is_not_read_as_a_cut_off() {
        let remote = "Detailed Instructions For The Self Involved";
        for local in [
            "Detailed Instructions For The EP",
            "The Detailed Instructions For The",
        ] {
            let value = album_title_similarity(local, remote);
            assert!(value < 0.90, "{local:?} scores {value}");
        }
        // The raw tag may still be a cut-off copy.
        let cut = "Detailed Instructions For The";
        assert!(album_title_similarity(cut, remote) >= 0.96);
    }

    #[test]
    fn different_disc_numbers_are_never_stripped() {
        for (local, remote) in [
            ("Album CD1", "Album CD2"),
            ("Greatest Hits CD1", "Greatest Hits CD2"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(value < 0.95, "{local:?} against {remote:?}: {value}");
            assert_eq!(value, local_similarity(local, remote));
        }
        let value = album_title_similarity("These Wooden Ideas CDS2", "These Wooden Ideas");
        assert!((0.95..1.0).contains(&value), "{value}");
        assert_eq!(album_title_similarity("Album CD1", "Album CD1"), 1.0);
        assert_eq!(
            numbered_release_types("Album CD1"),
            BTreeSet::from(["cd1".to_string()])
        );
        assert_eq!(
            numbered_release_types("Album CDM_CD2"),
            BTreeSet::from(["cd2".to_string()])
        );
        assert!(numbered_release_types("Album").is_empty());
    }

    #[test]
    fn the_new_album_title_readings_keep_different_albums_apart() {
        // The new readings do not raise the score of a different volume, and
        // different numbers stay different.
        assert_eq!(
            album_title_similarity("Hits Vol. 2", "Hits Vol. 3"),
            local_similarity("Hits Vol. 2", "Hits Vol. 3")
        );
        assert!(album_title_similarity("Album 2", "Album 3") < 0.9);
        // A decorated title keeps its core.
        assert!(album_title_similarity("Another Pearl CDS2", "Other Pearl") < 0.9);
        // Nothing is left to strip, or nothing would remain.
        assert_eq!(reduce_album_title("CDS2"), None);
        assert_eq!(reduce_album_title("Volume 2"), None);
        // Versions are still never dropped.
        assert!(album_title_similarity("Album", "Album Live 2") < 0.9);
    }

    #[test]
    fn release_type_words_and_a_leading_the_do_not_change_the_album() {
        for (local, remote) in [
            ("No Distance Left to Run EP", "No Distance Left To Run"),
            (
                "Deuce Bigalow Male Gigolo OST",
                "Deuce Bigalow, Male Gigolo",
            ),
            ("5150 Home 4 Tha Sick EP", "5150 Home 4 Tha Sick"),
            ("donkey rhubarb e.p.", "Donkey Rhubarb"),
            ("O.S.T. Mary Reilly", "Mary Reilly"),
            ("v.a-smells like team spirit", "Smells Like Team Spirit"),
            ("Iron Icon", "The Iron Icon"),
            ("The May Queens", "May Queens"),
            (
                "The Fruit That Ate Itself Japanese",
                "The Fruit That Ate Itself",
            ),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
    }

    #[test]
    fn album_title_reduction_never_drops_versions_or_the_whole_title() {
        // A version stays part of the title.
        for remote in [
            "Album Live",
            "Album Remix",
            "Album Remastered",
            "Album Demo",
        ] {
            assert!(album_title_similarity("Album", remote) < 0.9, "{remote}");
        }
        // Different albums stay different.
        assert!(album_title_similarity("Ostrich", "Rich") < 0.6);
        assert!(album_title_similarity("Some EP", "Other EP") < 0.7);
        // Nothing is left to reduce, or nothing would remain.
        assert_eq!(reduce_album_title("EP"), None);
        assert_eq!(reduce_album_title("The"), None);
        assert_eq!(reduce_album_title("Plain Title"), None);
        assert_eq!(reduce_album_title("O.S.T."), None);
        assert_eq!(reduce_album_title("Blue Single").as_deref(), Some("Blue"));
        assert_eq!(reduce_album_title("Ostrich Eggs"), None);
        assert_eq!(reduce_album_title("The Cure").as_deref(), Some("Cure"));
    }

    #[test]
    fn scene_decorations_on_an_album_title_do_not_change_the_album() {
        for (local, remote) in [
            ("Eyes Bleed Silver 7inch", "Eyes Bleed Silver"),
            ("Fat Club 7 Inch", "Fat Club"),
            ("Fools Gold 7\"", "Fools Gold"),
            ("Whenever You See Fit 12''", "Whenever You See Fit"),
            ("Whenever You See Fit 12\u{2033} EP", "Whenever You See Fit"),
            ("The Racer 100 EP 12 Inch", "The Racer 100 EP"),
            ("Fefe Dobson-RETAIL", "Fefe Dobson"),
            ("Name-RETAIL CDS", "Name"),
            ("Be A Man-PROMO CDM", "Be A Man"),
            ("Hello-CD Single", "Hello"),
            ("The Truth (RETAIL EXPLICIT)", "The Truth"),
            ("Boy Hits Car (Explicit Retail)", "Boy Hits Car"),
            ("Album PROPER RETAIL", "Album"),
            ("Title-TAPE ADVANCE", "Title"),
            ("Beneath The Surface -ADVANCE-", "Beneath The Surface"),
            ("Craig Olive EP (Vinyl)", "Craig Olive"),
            ("Mandatory Music - Vinyl", "Mandatory Music"),
            ("Chief Rocka VLS", "Chief Rocka"),
            ("Pain I Feel promo 12inch", "Pain I Feel"),
            ("Old School Recess - 7 Inch", "Old School Recess"),
            ("Tapin' Up My Heart (7-Inch)", "Tapin' Up My Heart"),
            ("The Best Of Offspring Bootleg", "The Best Of Offspring"),
            ("Psoriasis Reissue", "Psoriasis"),
            ("Name CDM_CD2", "Name"),
            ("Everyone Says \"hi\" CDM_CD2", "Everyone Says \"hi\""),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
    }

    #[test]
    fn a_decoration_next_to_a_non_ascii_letter_is_read_without_slicing_inside_it() {
        for (local, remote) in [
            ("\u{00C9}t\u{00E9} 12''", "\u{00C9}t\u{00E9}"),
            (
                "\u{00C9}ire\u{2019}s Best-RETAIL",
                "\u{00C9}ire\u{2019}s Best",
            ),
            (
                "Ça va\u{2019}s \u{00C9}P (Vinyl)",
                "Ça va\u{2019}s \u{00C9}P",
            ),
            ("Caf\u{00E9} \u{2013} Reissue", "Caf\u{00E9}"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
        assert_eq!(
            reduce_album_title("Caf\u{00E9}-RETAIL").as_deref(),
            Some("Caf\u{00E9}")
        );
    }

    #[test]
    fn a_decoration_joined_to_the_title_keeps_the_title_whole() {
        assert_eq!(reduce_album_title("Jay-Z EP").as_deref(), Some("Jay-Z"));
        assert_eq!(
            reduce_album_title("Beneath The Surface -ADVANCE-").as_deref(),
            Some("Beneath The Surface")
        );
        assert_eq!(
            reduce_album_title("Foo (Bar)-RETAIL").as_deref(),
            Some("Foo (Bar)")
        );
        assert_eq!(reduce_album_title("Blink-182"), None);
        assert!(album_title_similarity("Blink-182", "Blink") < 0.9);
    }

    #[test]
    fn a_companion_word_is_dropped_only_with_a_release_type_word() {
        assert_eq!(reduce_album_title("Album (Explicit)"), None);
        assert_eq!(reduce_album_title("Demo Tape"), None);
        assert_eq!(reduce_album_title("Album Proper"), None);
        assert_eq!(
            reduce_album_title("Album PROPER RETAIL").as_deref(),
            Some("Album")
        );
        assert_eq!(
            reduce_album_title("Democrazy-TAPE ADVANCE").as_deref(),
            Some("Democrazy")
        );
        // A companion after a size goes with it, one in front of it needs a mark.
        assert_eq!(
            reduce_album_title("Album 7\" Proper").as_deref(),
            Some("Album")
        );
        assert_eq!(
            reduce_album_title("Album Proper 7\"").as_deref(),
            Some("Album Proper")
        );
        assert_eq!(
            reduce_album_title("Album PROPER 7\"").as_deref(),
            Some("Album")
        );
        // A companion that is part of the title and written like it stays.
        assert_eq!(
            reduce_album_title("Album Proper Retail").as_deref(),
            Some("Album Proper")
        );
    }

    #[test]
    fn a_companion_word_in_the_title_is_not_read_as_decoration() {
        for (local, remote) in [
            ("Demo Tape EP", "Demo Tape"),
            ("Demo Tape", "Demo Tape EP"),
            ("Prim and Proper EP", "Prim and Proper"),
            ("Sexually Explicit EP", "Sexually Explicit"),
            ("Sex Tape Single", "Sex Tape"),
            ("Sex Tape-RETAIL", "Sex Tape"),
            // Written in capitals, the title's own word is still found.
            ("Sex TAPE EP", "Sex TAPE"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..=1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
        for (local, remote) in [
            ("Sex Tape Retail", "Sex"),
            ("Mix Tape Retail", "Mix"),
            // Capitals mean nothing in a title that is all capitals.
            ("DEMO TAPE EP", "Demo"),
            ("PRIM AND PROPER EP", "Prim and"),
            ("SEXUALLY EXPLICIT SINGLE", "Sexually"),
            ("SEX TAPE RETAIL", "Sex"),
            ("X PROPER EP", "X TAPE EP"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(value < 0.9, "{local:?} against {remote:?}: {value}");
        }
        let value = album_title_similarity("Breakin News PROPER RETAIL", "Breakin News");
        assert!((0.95..=1.0).contains(&value), "{value}");
        // An all-caps title keeps its companion word, so only the release-type
        // word goes.
        assert_eq!(
            reduce_album_title("BREAKIN NEWS PROPER RETAIL").as_deref(),
            Some("BREAKIN NEWS PROPER")
        );
        assert_eq!(
            reduce_album_title("X PROPER EP").as_deref(),
            Some("X PROPER")
        );
    }

    #[test]
    fn a_title_made_only_of_decoration_keeps_its_first_unit() {
        for (local, remote) in [
            ("Japan EP", "Japan"),
            ("Bonus EP", "Bonus"),
            ("The Promo EP", "Promo"),
            ("Vinyl Single", "Vinyl"),
            ("Limited Edition", "Limited"),
            ("The Bootleg EP", "The Bootleg"),
        ] {
            let value = album_title_similarity(local, remote);
            assert!(
                (0.95..=1.0).contains(&value),
                "{local:?} against {remote:?}: {value}"
            );
        }
        assert_eq!(reduce_album_title("Japan EP").as_deref(), Some("Japan"));
        assert_eq!(reduce_album_title("12 Inch"), None);
        assert_eq!(reduce_album_title("Vinyl"), None);
    }

    #[test]
    fn a_bare_article_is_not_an_album() {
        assert_eq!(reduce_album_title("The-Bootleg"), None);
        assert_eq!(reduce_album_title("A Bootleg"), None);
        assert!(album_title_similarity("The-Bootleg", "The") < 0.9);
        assert!(album_title_similarity("A Bootleg", "A") < 0.9);
    }

    #[test]
    fn an_inch_mark_must_follow_the_number() {
        assert_eq!(reduce_album_title("Number \"7\""), None);
        assert_eq!(reduce_album_title("Seven \"7"), None);
        assert_eq!(
            reduce_album_title("Fools Gold (7\")").as_deref(),
            Some("Fools Gold")
        );
        assert_eq!(
            reduce_album_title("Fools Gold 10\u{2033}").as_deref(),
            Some("Fools Gold")
        );
    }

    #[test]
    fn a_bare_number_is_not_a_size_and_versions_stop_the_decoration_run() {
        assert!(album_title_similarity("Album 7", "Album") < 0.9);
        assert!(album_title_similarity("Album Inch", "Album") < 0.9);
        assert!(album_title_similarity("Album 8 Inch", "Album") < 0.9);
        assert!(album_title_similarity("Album-LIVE", "Album") < 0.9);
        assert!(album_title_similarity("Album Live Bootleg", "Album") < 0.9);
        assert_eq!(
            reduce_album_title("Album Live Bootleg").as_deref(),
            Some("Album Live")
        );
        // Nothing would remain.
        assert_eq!(reduce_album_title("12 Inch"), None);
        assert_eq!(reduce_album_title("Vinyl"), None);
    }

    #[test]
    fn a_decorated_title_is_not_read_as_an_id3v1_cut_off() {
        let remote = "Detailed Instructions For The Self Involved";
        assert!(album_title_similarity("Detailed Instructions For The-RETAIL", remote) < 0.9);
        let cut = "Detailed Instructions For The";
        assert!(album_title_similarity(cut, remote) >= 0.96);
    }

    #[test]
    fn different_disc_numbers_in_a_decoration_run_are_never_stripped() {
        for local in ["Album-CD2", "Album CD2 Retail", "Album CDM_CD2"] {
            let value = album_title_similarity(local, "Album CD1");
            assert!(value < 0.95, "{local:?}: {value}");
            assert_eq!(value, local_similarity(local, "Album CD1"), "{local:?}");
        }
        let value = album_title_similarity("Album CDM_CD2", "Album CD2");
        assert!((0.95..=1.0).contains(&value), "{value}");
    }

    #[test]
    fn a_cut_off_artist_matches_the_full_credit() {
        let remote = "Everything But The Girl & Tracey Thorn";
        let cut: String = remote.chars().take(ID3V1_FIELD_CHARS).collect();
        assert!(artist_similarity(&cut, remote) < 0.9);
        assert_eq!(local_artist_similarity(&cut, remote), 1.0);
    }

    #[test]
    fn cut_off_titles_score_as_the_full_titles_when_ranking_and_mapping() {
        let long = [
            "The Mouth Licking What You\u{2019}ve Bled",
            "Little Rich Working Class Oi Boys",
        ];
        let cut: Vec<String> = long
            .iter()
            .map(|title| title.chars().take(ID3V1_FIELD_CHARS).collect())
            .collect();
        let cut: Vec<&str> = cut.iter().map(String::as_str).collect();
        let local = local_release(&cut);
        let remote = remote_release(&long);

        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete, "{mapping:?}");
        let breakdown = score(&local, &remote, &mapping);
        assert!(breakdown.track_titles > 0.99, "{breakdown:?}");
    }

    #[test]
    fn strips_numeric_artist_disambiguators_for_comparison_only() {
        assert_eq!(strip_disambiguator("Nirvana (2)"), "Nirvana");
        assert_eq!(
            strip_disambiguator("Blink-182 (Deluxe Edition)"),
            "Blink-182 (Deluxe Edition)"
        );
        assert_eq!(normalize_artist("Nirvana (2)"), normalize_artist("nirvana"));
        // MusicBrainz credits keep their exact text.
        assert_ne!(
            normalize_artist_for(ProviderKind::MusicBrainz, "Nirvana (2)"),
            normalize_artist_for(ProviderKind::MusicBrainz, "Nirvana")
        );
    }

    #[test]
    fn titles_that_normalize_to_nothing_compare_by_their_original_text() {
        assert_eq!(similarity("÷", "×"), 0.0);
        assert_eq!(similarity("÷", "÷"), 1.0);
        assert_eq!(similarity("÷", " ÷ "), 1.0);
        assert_eq!(similarity("", ""), 0.0);
        assert_eq!(similarity("", "Intro"), 0.0);
        assert_eq!(artist_similarity("!!!", "???"), 0.0);
        assert_eq!(artist_similarity("!!!", "!!!"), 1.0);
    }

    #[test]
    fn symbol_only_album_artists_compare_by_their_original_text() {
        let mut local = local_release(&["One"]);
        local.album_artist = Some("!!!".to_string());
        let mut remote = remote_release(&["One"]);
        remote.artists = vec!["!!!".to_string()];
        assert_eq!(
            score(&local, &remote, &map_tracks(&local, &remote)).album_artist,
            1.0
        );

        // Discogs numbers its disambiguations; they never count as text.
        remote.artists = vec!["!!! (2)".to_string()];
        assert_eq!(
            score(&local, &remote, &map_tracks(&local, &remote)).album_artist,
            1.0
        );
        assert_eq!(artist_similarity("!!! (2)", "!!!"), 1.0);
        assert_eq!(artist_similarity("!!! (2)", "???"), 0.0);

        remote.artists = vec!["???".to_string()];
        assert_eq!(
            score(&local, &remote, &map_tracks(&local, &remote)).album_artist,
            0.0
        );
    }

    #[test]
    fn parses_durations_and_treats_invalid_as_missing() {
        assert_eq!(parse_duration("3:32"), Some(212.0));
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("0:00"), None);
    }

    #[test]
    fn parses_vinyl_and_multi_disc_positions() {
        assert_eq!(
            parse_position("A1"),
            ParsedPosition {
                disc: None,
                track: Some(1),
                side: Some('A')
            }
        );
        assert_eq!(
            parse_position("2-03"),
            ParsedPosition {
                disc: Some(2),
                track: Some(3),
                side: None
            }
        );
        assert_eq!(parse_position("07").track, Some(7));
        assert_eq!(parse_position(""), ParsedPosition::default());
    }

    #[test]
    fn keeps_edition_qualifiers_meaningful() {
        assert!(edition_qualifiers("Test Album (Live)").contains("live"));
        assert!(edition_qualifiers("Test Album").is_empty());
        assert_ne!(
            edition_qualifiers("Test Album (Remastered)"),
            edition_qualifiers("Test Album (Live)")
        );
    }

    #[test]
    fn maps_an_exact_release() {
        let local = local_release(&["One", "Two", "Three"]);
        let remote = remote_release(&["One", "Two", "Three"]);
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);
        assert!(!mapping.ambiguous);
        assert_eq!(mapping.assigned_count(), 3);
    }

    #[test]
    fn maps_reordered_tracks_when_positions_are_absent() {
        let mut local = local_release(&["Three", "One", "Two"]);
        for track in &mut local.tracks {
            track.track_number = None;
        }
        let mut remote = remote_release(&["One", "Two", "Three"]);
        for track in &mut remote.tracklist {
            track.position = None;
        }
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);
        assert_eq!(mapping.pairs, vec![Some(2), Some(0), Some(1)]);
    }

    #[test]
    fn unique_positions_win_over_titles() {
        let local = local_release(&["Three", "One", "Two"]);
        let remote = remote_release(&["One", "Two", "Three"]);
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);
        assert_eq!(mapping.pairs, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn release_track_ids_anchor_reordered_files_across_two_discs() {
        // Remote order: One (1-1), Two (1-2), Three (2-1), Four (2-2).
        // Local files carry the release-track IDs of remote indexes 2, 0, 3, 1.
        let mut local = local_release(&["Three", "One", "Four", "Two"]);
        let ids = [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
            "33333333-3333-3333-3333-333333333333",
            "44444444-4444-4444-4444-444444444444",
        ];
        let local_remote_indexes = [2, 0, 3, 1];
        for (index, track) in local.tracks.iter_mut().enumerate() {
            let remote_index = local_remote_indexes[index];
            track.release_track_id = Some(mbid(ids[remote_index]));
            track.disc = Some(if remote_index < 2 { 1 } else { 2 });
            track.track_number = Some((remote_index % 2 + 1) as i64);
        }

        let mut remote = remote_release(&["One", "Two", "Three", "Four"]);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.release_track_id = Some(mbid(ids[index]));
            track.medium_position = Some(if index < 2 { 1 } else { 2 });
            track.track_position = Some((index % 2 + 1) as u32);
            track.position = None;
        }

        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete, "{:?}", mapping.notes);
        assert!(!mapping.ambiguous, "{:?}", mapping.notes);
        assert_eq!(mapping.pairs, vec![Some(2), Some(0), Some(3), Some(1)]);
    }

    #[test]
    fn repeated_recording_ids_do_not_collapse_duplicate_assignments() {
        let mut local = local_release(&["One", "Two"]);
        let recording = mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd");
        for track in &mut local.tracks {
            track.recording_id = Some(recording);
            track.track_number = None;
        }

        let mut remote = remote_release(&["One", "Two"]);
        for track in &mut remote.tracklist {
            track.recording_id = Some(recording);
            track.position = None;
        }

        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);
        let mut targets: Vec<usize> = mapping.pairs.iter().filter_map(|p| *p).collect();
        targets.sort_unstable();
        targets.dedup();
        assert_eq!(targets.len(), 2, "both files must map to distinct tracks");
    }

    #[test]
    fn identifier_anchors_keep_position_conflicts_visible() {
        let mut local = local_release(&["One", "Two"]);
        // Second local file claims the release-track ID of remote track 1.
        local.tracks[1].release_track_id = Some(mbid("11111111-1111-1111-1111-111111111111"));
        let mut remote = remote_release(&["One", "Two"]);
        remote.tracklist[0].release_track_id = Some(mbid("11111111-1111-1111-1111-111111111111"));

        let mapping = map_tracks(&local, &remote);
        assert!(mapping.ambiguous, "{:?}", mapping.notes);
        assert!(
            mapping.notes.iter().any(|note| note.contains("tags say")),
            "{:?}",
            mapping.notes
        );
    }

    #[test]
    fn ignores_headings_and_refuses_index_entries() {
        let local = local_release(&["One", "Two"]);
        let mut remote = remote_release(&["One", "Two"]);
        remote.tracklist.insert(
            0,
            RemoteTrack {
                position: None,
                title: "Side A".to_string(),
                artists: Vec::new(),
                duration_secs: None,
                entry_type: TrackEntryType::Heading,
                medium_position: None,
                track_position: None,
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            },
        );
        assert_eq!(remote.playable_tracks().len(), 2);
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);

        remote.tracklist.push(RemoteTrack {
            position: None,
            title: "Hidden Index".to_string(),
            artists: Vec::new(),
            duration_secs: None,
            entry_type: TrackEntryType::Index,
            medium_position: None,
            track_position: None,
            release_track_id: None,
            recording_id: None,
            artist_credit: None,
        });
        let mapping = map_tracks(&local, &remote);
        assert!(!mapping.complete);
        assert!(remote.has_unsupported_entries());
    }

    #[test]
    fn structure_issues_block_acceptance() {
        let mut local = local_release(&["One"]);
        local.embedded_evidence.release_id = Some(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let mut remote = remote_release(&["One"]);
        remote.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        remote.structure_issues = vec!["medium 1 has a pregap track".to_string()];
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(!evaluation.can_apply(&candidate));
    }

    #[test]
    fn extra_tracks_make_the_mapping_incomplete() {
        let local = local_release(&["One", "Two"]);
        let remote = remote_release(&["One", "Two", "Bonus"]);
        let mapping = map_tracks(&local, &remote);
        assert!(!mapping.complete);
    }

    #[test]
    fn duplicate_titles_produce_an_ambiguous_tie() {
        let local = local_release(&["Intro", "Intro"]);
        let remote = remote_release(&["Intro", "Intro"]);
        // Equal titles, durations, and no positions leave a true tie.
        let mut local = local;
        for track in &mut local.tracks {
            track.duration_secs = Some(100.0);
            track.track_number = None;
        }
        let mut remote = remote;
        for track in &mut remote.tracklist {
            track.duration_secs = Some(100.0);
            track.position = None;
        }
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.ambiguous);
        assert!(mapping.complete);
    }

    #[test]
    fn vinyl_positions_fall_back_to_assignment() {
        let mut local = local_release(&["One", "Two", "Three", "Four"]);
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.disc = Some(1);
            track.track_number = Some(index as i64 + 1);
        }
        let mut remote = remote_release(&["One", "Two", "Three", "Four"]);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.position = Some(format!(
                "{}{}",
                if index < 2 { 'A' } else { 'B' },
                index % 2 + 1
            ));
        }
        let mapping = map_tracks(&local, &remote);
        assert!(mapping.complete);
        assert_eq!(mapping.assigned_count(), 4);
    }

    #[test]
    fn missing_durations_are_missing_evidence() {
        let local = local_release(&["One"]);
        let mut remote = remote_release(&["One"]);
        remote.tracklist[0].duration_secs = None;
        let breakdown = score(&local, &remote, &map_tracks(&local, &remote));
        assert_eq!(breakdown.durations, 0.0);
        assert!(breakdown.total < 1.0);
    }

    #[test]
    fn missing_durations_lower_the_duration_score_instead_of_being_skipped() {
        let titles: Vec<String> = (1..=12).map(|n| format!("Song {n}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let local = local_release(&titles);

        let mut sparse = remote_release(&titles);
        for track in sparse.tracklist.iter_mut().skip(1) {
            track.duration_secs = None;
        }
        let mut complete = remote_release(&titles);
        for track in &mut complete.tracklist {
            track.duration_secs = track.duration_secs.map(|secs| secs + 2.0);
        }

        let sparse = Candidate::new(&local, sparse);
        let complete = Candidate::new(&local, complete);
        assert!(
            complete.score.durations > sparse.score.durations,
            "complete {} vs sparse {}",
            complete.score.durations,
            sparse.score.durations
        );
    }

    #[test]
    fn mapping_records_the_similarity_of_each_assigned_pair() {
        let local = local_release(&["One", "Two", "Three"]);
        let remote = remote_release(&["Three", "Two", "Won"]);
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.pair_similarity.len(), mapping.pairs.len());
        for (local_index, remote_index) in mapping.assigned_pairs() {
            let expected = similarity(
                local.tracks[local_index].title.as_deref().unwrap(),
                &remote.tracklist[remote_index].title,
            );
            assert_eq!(
                mapping.pair_similarity[local_index],
                Some((remote_index, expected))
            );
        }
    }

    #[test]
    fn mappings_round_trip_without_recorded_similarities_and_score_the_same() {
        let local = local_release(&["One", "Two", "Three"]);
        let remote = remote_release(&["One", "Two", "Tree"]);
        let candidate = Candidate::new(&local, remote.clone());

        let json = serde_json::to_value(&candidate.mapping).unwrap();
        assert!(json.get("pair_similarity").is_none(), "{json}");
        let decoded: Mapping = serde_json::from_value(json).unwrap();
        assert!(decoded.pair_similarity.is_empty());
        assert_eq!(score(&local, &remote, &decoded), candidate.score);
    }

    #[test]
    fn recorded_similarities_are_recomputed_when_the_pairs_change() {
        let mut local = local_release(&["Alpha", "Bravo"]);
        local.barcode = Some("0123456789012".to_string());
        let remote = remote_release(&["Alpha", "Bravo"]);
        let mut candidate = Candidate::new(&local, remote.clone());
        candidate.mapping.pairs = vec![Some(1), Some(0)];

        let rescored = score(&local, &remote, &candidate.mapping);
        assert!(rescored.track_titles < 0.5, "{}", rescored.track_titles);
        let evaluation = evaluate(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence {
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(!evaluation.auto_accept, "{:?}", evaluation.evidence);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("track title")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn close_alternative_gap_includes_the_exact_boundary() {
        assert!(within_gap(0.0));
        assert!(within_gap(RUNNER_UP_GAP - 0.001));
        assert!(within_gap(RUNNER_UP_GAP));
        assert!(!within_gap(RUNNER_UP_GAP + 0.001));
    }

    #[test]
    fn scores_components_with_fixed_weights() {
        let local = local_release(&["One", "Two"]);
        let remote = remote_release(&["One", "Two"]);
        let breakdown = score(&local, &remote, &map_tracks(&local, &remote));
        assert!((breakdown.total - 1.0).abs() < 1e-9);
        assert_eq!(breakdown.album_title, 1.0);
        assert_eq!(breakdown.album_artist, 1.0);
        assert_eq!(breakdown.track_titles, 1.0);
        assert_eq!(breakdown.durations, 1.0);
        assert_eq!(breakdown.year, 1.0);
    }

    #[test]
    fn exact_release_with_embedded_id_is_accepted() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_discogs_id = Some(42);
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn identical_editions_without_evidence_need_review() {
        let local = local_release(&["One", "Two"]);
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("distinguishes this edition"))
        );
    }

    #[test]
    fn barcode_evidence_allows_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.barcode = Some("0123456789012".to_string());
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence {
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn unique_identifier_flag_needs_a_matching_local_identifier() {
        let local = local_release(&["One", "Two"]);
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence {
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .evidence
                .iter()
                .any(|line| line.contains("uniquely identifies"))
        );
    }

    #[test]
    fn conflicting_barcode_blocks_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.barcode = Some("9999999999999".to_string());
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn truncated_identifier_search_blocks_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_discogs_id = Some(42);
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence {
                identifier_search_truncated: true,
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn runner_up_gap_blocks_acceptance_unless_embedded_id_agrees() {
        let mut local = local_release(&["One", "Two"]);
        local.barcode = Some("0123456789012".to_string());
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut second_release = remote_release(&["One", "Two"]);
        second_release.id = ReleaseId::Discogs(43);
        let second = Candidate::new(&local, second_release);

        let evaluation = evaluate(
            &local,
            &first,
            Some(&second),
            &DiscoveryEvidence {
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(!evaluation.auto_accept);

        let mut embedded = local.clone();
        embedded.embedded_discogs_id = Some(42);
        let evaluation = evaluate(
            &embedded,
            &first,
            Some(&second),
            &DiscoveryEvidence::default(),
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn a_runner_up_exactly_at_the_gap_blocks_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.barcode = Some("0123456789012".to_string());
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let discovery = DiscoveryEvidence {
            identifier_unique: true,
            ..DiscoveryEvidence::default()
        };
        let mut second = first.clone();
        second.release.id = ReleaseId::Discogs(43);

        second.score.total = first.score.total - (RUNNER_UP_GAP + 0.001);
        let clear = evaluate(&local, &first, Some(&second), &discovery);
        assert!(clear.auto_accept, "{:?}", clear.blockers);

        second.score.total = first.score.total - RUNNER_UP_GAP;
        let boundary = evaluate(&local, &first, Some(&second), &discovery);
        assert!(!boundary.auto_accept);
        assert!(
            boundary
                .blockers
                .iter()
                .any(|blocker| blocker.contains("runner-up")),
            "{:?}",
            boundary.blockers
        );
    }

    #[test]
    fn edition_qualifier_mismatch_blocks_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.album_title = Some("Test Album (Live)".to_string());
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn compilation_artist_matches_various_artists() {
        let mut local = local_release(&["One", "Two"]);
        local.album_artist = Some("Various Artists".to_string());
        local.artist_kind = ArtistKind::VariousArtists;
        local.is_compilation = true;
        let mut remote = remote_release(&["One", "Two"]);
        remote.artists = vec!["Various".to_string()];
        let breakdown = score(&local, &remote, &map_tracks(&local, &remote));
        assert_eq!(breakdown.album_artist, 1.0);
    }

    #[test]
    fn search_summaries_rank_identifier_hits_above_unrelated_titles() {
        let mut local = local_release(&["One"]);
        local.barcode = Some("0123456789012".to_string());
        let matching = SearchSummary {
            id: ReleaseId::Discogs(42),
            title: "Test Artist - Test Album".to_string(),
            artists: Vec::new(),
            artist_credit: None,
            year: Some(2001),
            country: Some("US".to_string()),
            labels: vec!["Test Label".to_string()],
            catalog_numbers: vec!["CAT-1".to_string()],
            formats: vec!["CD".to_string()],
            barcodes: vec!["0123456789012".to_string()],
            result_type: "release".to_string(),
        };
        let other = SearchSummary {
            title: "Someone Else - Another Album".to_string(),
            ..matching.clone()
        };
        assert!(rank_search_summary(&local, &matching) > rank_search_summary(&local, &other));
    }

    #[test]
    fn musicbrainz_credits_keep_join_phrases() {
        let track = RemoteTrack {
            position: None,
            title: "One".to_string(),
            artists: vec!["A".to_string(), "B".to_string()],
            duration_secs: None,
            entry_type: TrackEntryType::Track,
            medium_position: Some(1),
            track_position: Some(1),
            release_track_id: None,
            recording_id: None,
            artist_credit: Some("A feat. B".to_string()),
        };
        assert_eq!(track.artist_display(), "A feat. B");
    }

    #[test]
    fn release_group_identity_is_not_edition_evidence() {
        let mut local = local_release(&["One", "Two"]);
        let group = mbid("4642ee19-7790-3c8d-ab5e-d133de942db6");
        local.embedded_evidence.release_group_id = Some(group);

        let mut remote = remote_release(&["One", "Two"]);
        remote.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        remote.group_id = Some(GroupId::MusicBrainzReleaseGroup(group));
        remote.url =
            "https://musicbrainz.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd".to_string();

        let candidate = Candidate::new(&local, remote);
        let group_only_evaluation =
            evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!group_only_evaluation.auto_accept);
        assert!(
            group_only_evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("distinguishes this edition"))
        );
    }

    #[test]
    fn edition_twins_and_incomplete_mappings_need_review() {
        let mut local = local_release(&["One", "Two"]);
        local.barcode = Some("0123456789012".to_string());
        let twin_release = |id: &str| {
            let mut release = remote_release(&["One", "Two"]);
            release.id = ReleaseId::MusicBrainz(mbid(id));
            release.url = format!("https://musicbrainz.org/release/{id}");
            release
        };
        let first = Candidate::new(&local, twin_release("11111111-1111-1111-1111-111111111111"));
        let second = Candidate::new(&local, twin_release("22222222-2222-2222-2222-222222222222"));
        let twin_evaluation = evaluate(
            &local,
            &first,
            Some(&second),
            &DiscoveryEvidence {
                identifier_unique: true,
                ..DiscoveryEvidence::default()
            },
        );
        assert!(!twin_evaluation.auto_accept);

        let incomplete_candidate = Candidate::new(&local, remote_release(&["One"]));
        let incomplete_evaluation = evaluate(
            &local,
            &incomplete_candidate,
            None,
            &DiscoveryEvidence::default(),
        );
        assert!(!incomplete_evaluation.can_apply(&incomplete_candidate));
    }

    #[test]
    fn embedded_musicbrainz_release_id_is_edition_evidence() {
        let mut local = local_release(&["One", "Two"]);
        let id = mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd");
        local.embedded_evidence.release_id = Some(id);

        let mut release = remote_release(&["One", "Two"]);
        release.id = ReleaseId::MusicBrainz(id);
        release.url = format!("https://musicbrainz.org/release/{id}");
        let embedded_candidate = Candidate::new(&local, release);
        let embedded_evaluation = evaluate(
            &local,
            &embedded_candidate,
            None,
            &DiscoveryEvidence::default(),
        );
        assert!(
            embedded_evaluation.auto_accept,
            "{:?}",
            embedded_evaluation.blockers
        );
    }

    #[test]
    fn malformed_observed_identifiers_block_automatic_acceptance() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_evidence.release_id = Some(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        local.embedded_evidence.problems =
            vec!["observed release ID 'not-an-mbid' is malformed".to_string()];

        let mut release = remote_release(&["One", "Two"]);
        release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let candidate = Candidate::new(&local, release);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn fallback_conflicts_report_edition_and_timing_disagreements() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_evidence.release_id = Some(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));

        let mut primary_release = remote_release(&["One", "Two"]);
        primary_release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        primary_release.edition_notes = vec!["Live".to_string()];
        let primary = Candidate::new(&local, primary_release);
        assert!(primary.score.total >= PLAUSIBLE_PRIMARY_SCORE);

        let fallback = Candidate::new(&local, remote_release(&["One", "Two"]));
        let conflicts = fallback_conflicts(&local, &[primary], &fallback, false);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("not resolved"))
        );
    }

    #[test]
    fn fallback_guard_treats_missing_primary_durations_as_neutral() {
        let local = local_release(&["One", "Two", "Three", "Four"]);
        let mut primary_release = remote_release(&["One", "Two", "Three", "Four"]);
        primary_release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        primary_release.edition_notes = vec!["Live".to_string()];
        for track in primary_release.tracklist.iter_mut().skip(2) {
            track.duration_secs = None;
        }
        let primary = Candidate::new(&local, primary_release);
        assert!(primary.score.total < PLAUSIBLE_PRIMARY_SCORE);

        let fallback = Candidate::new(&local, remote_release(&["One", "Two", "Three", "Four"]));
        let conflicts = fallback_conflicts(&local, &[primary], &fallback, true);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{conflicts:?}"
        );
    }

    #[test]
    fn algorithm_versions_are_provider_specific() {
        assert_eq!(
            algorithm_version(ProviderKind::Discogs),
            "discogs-match-v20"
        );
        assert_eq!(
            algorithm_version(ProviderKind::MusicBrainz),
            "musicbrainz-match-v19"
        );
    }

    #[test]
    fn album_matching_keeps_track_and_identity_checks() {
        let baseline = local_release(&["One", "Two"]);
        for problem in [
            "missing title",
            "missing duration",
            "incomplete",
            "different version",
            "conflicting barcode",
            "unknown artist",
        ] {
            let mut local = baseline.clone();
            let mut remote = remote_release(&["One", "Two"]);
            match problem {
                "missing title" => local.tracks[0].title = None,
                "missing duration" => remote.tracklist[0].duration_secs = None,
                "incomplete" => {
                    remote.tracklist.pop();
                }
                "different version" => remote.edition_notes.push("Live".to_string()),
                "conflicting barcode" => local.barcode = Some("9999999999999".to_string()),
                "unknown artist" => local.album_artist = None,
                _ => unreachable!(),
            }
            let candidate = Candidate::new(&local, remote);
            let evaluation =
                evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
            assert!(!evaluation.auto_accept, "accepted {problem}");
        }
    }

    #[test]
    fn a_shared_recording_makes_editions_equivalent_despite_a_renamed_artist() {
        let local = local_release(&["One", "Two"]);
        let recordings = [
            mbid("aaaaaaaa-1111-1111-1111-111111111111"),
            mbid("bbbbbbbb-2222-2222-2222-222222222222"),
        ];
        let mut original = remote_release(&["One", "Two"]);
        for (track, recording) in original.tracklist.iter_mut().zip(recordings) {
            track.recording_id = Some(recording);
        }
        let mut reissue = original.clone();
        reissue.id = ReleaseId::Discogs(43);
        for track in &mut reissue.tracklist {
            track.artist_credit = Some("Renamed Artist".to_string());
        }
        let first = Candidate::new(&local, original.clone());
        let other = Candidate::new(&local, reissue.clone());
        assert!(equivalent_audio(&first, &other));

        // Without a shared recording the changed credit still separates them.
        for track in &mut reissue.tracklist {
            track.recording_id = None;
        }
        let unproven = Candidate::new(&local, reissue.clone());
        assert!(!equivalent_audio(&first, &unproven));

        // A different recording is never excused by anything else.
        reissue.tracklist[0].recording_id = Some(mbid("cccccccc-3333-3333-3333-333333333333"));
        reissue.tracklist[1].recording_id = original.tracklist[1].recording_id;
        let conflicting = Candidate::new(&local, reissue);
        assert!(!equivalent_audio(&first, &conflicting));
    }

    #[test]
    fn another_edition_of_the_same_album_does_not_block_an_album_match() {
        let local = local_release(&["One", "Two"]);
        let group = Some(GroupId::DiscogsMaster(900));
        let mut best = remote_release(&["One", "Two"]);
        best.group_id = group.clone();
        let first = Candidate::new(&local, best.clone());

        // A bonus-track reissue in the same group is the same album.
        let mut reissue = remote_release(&["One", "Two", "Bonus"]);
        reissue.id = ReleaseId::Discogs(43);
        reissue.group_id = group.clone();
        let reissue = Candidate::new(&local, reissue);
        let evaluation =
            evaluate_for_matching(&local, &first, &[reissue], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // The same tracklist filed under a different album still blocks.
        let mut elsewhere = remote_release(&["One", "Two", "Bonus"]);
        elsewhere.id = ReleaseId::Discogs(44);
        elsewhere.group_id = Some(GroupId::DiscogsMaster(901));
        let elsewhere = Candidate::new(&local, elsewhere);
        let evaluation =
            evaluate_for_matching(&local, &first, &[elsewhere], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);

        // Editions of one album whose mapped tracks are different recordings (a
        // remaster) cannot be told apart from the album alone.
        let mut remaster = remote_release(&["One", "Two"]);
        remaster.id = ReleaseId::Discogs(45);
        remaster.group_id = group;
        let mut original = best.clone();
        original.tracklist[0].recording_id = Some(mbid("aaaaaaaa-1111-1111-1111-111111111111"));
        remaster.tracklist[0].recording_id = Some(mbid("bbbbbbbb-2222-2222-2222-222222222222"));
        let first = Candidate::new(&local, original);
        let remaster = Candidate::new(&local, remaster);
        let evaluation =
            evaluate_for_matching(&local, &first, &[remaster], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);

        // And with no group on either side there is nothing to rely on.
        let ungrouped = Candidate::new(&local, remote_release(&["One", "Two"]));
        assert!(!in_same_release_group(&ungrouped, &ungrouped));
    }

    #[test]
    fn an_alternative_that_misses_the_durations_does_not_block_the_candidate() {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let local = local_release(&titles);
        let group = Some(GroupId::DiscogsMaster(900));
        let mut best = remote_release(&titles);
        best.group_id = group.clone();
        best.tracklist[0].recording_id = Some(mbid("aaaaaaaa-1111-1111-1111-111111111111"));
        let first = Candidate::new(&local, best);

        // The same album in the same group, but another recording of the first
        // track and a length that is ten seconds off: not what the folder holds.
        let mut other = remote_release(&titles);
        other.id = ReleaseId::Discogs(45);
        other.group_id = group.clone();
        other.tracklist[0].recording_id = Some(mbid("bbbbbbbb-2222-2222-2222-222222222222"));
        other.tracklist[0].duration_secs = Some(190.0);
        let other = Candidate::new(&local, other);
        let evaluation =
            evaluate_for_matching(&local, &first, &[other], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // The same alternative with every duration agreeing cannot be told
        // apart, so it still blocks.
        let mut twin = remote_release(&titles);
        twin.id = ReleaseId::Discogs(46);
        twin.group_id = group.clone();
        twin.tracklist[0].recording_id = Some(mbid("bbbbbbbb-2222-2222-2222-222222222222"));
        let twin = Candidate::new(&local, twin);
        let evaluation =
            evaluate_for_matching(&local, &first, &[twin], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);

        // An unknown duration is no evidence either way.
        let mut unknown = remote_release(&titles);
        unknown.id = ReleaseId::Discogs(47);
        unknown.group_id = group;
        unknown.tracklist[0].recording_id = Some(mbid("bbbbbbbb-2222-2222-2222-222222222222"));
        unknown.tracklist[0].duration_secs = None;
        let unknown = Candidate::new(&local, unknown);
        let evaluation =
            evaluate_for_matching(&local, &first, &[unknown], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn a_release_with_extra_tracks_in_another_group_does_not_block_a_fitting_candidate() {
        let titles = ["One", "Two", "Three", "Four"];
        let local = local_release(&titles);
        let first = Candidate::new(&local, remote_release(&titles));

        let mut longer = remote_release(&["One", "Two", "Three", "Four", "Bonus"]);
        longer.id = ReleaseId::Discogs(48);
        longer.group_id = Some(GroupId::DiscogsMaster(901));
        let longer = Candidate::new(&local, longer);
        let evaluation =
            evaluate_for_matching(&local, &first, &[longer], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // Two local tracks are too little evidence to set an alternative aside.
        let local = local_release(&["One", "Two"]);
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut longer = remote_release(&["One", "Two", "Bonus"]);
        longer.id = ReleaseId::Discogs(49);
        longer.group_id = Some(GroupId::DiscogsMaster(902));
        let longer = Candidate::new(&local, longer);
        let evaluation =
            evaluate_for_matching(&local, &first, &[longer], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn a_weak_album_title_passes_when_every_track_and_duration_agrees() {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let mut local = local_release(&titles);
        local.album_title = Some("Exclusive Metal Sampler".to_string());
        let mut remote = remote_release(&titles);
        remote.title = "Exclusive Metal Radio Sampler".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (0.70..0.90).contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            evaluation
                .warnings
                .iter()
                .any(|note| note.contains("album title")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn a_weak_album_title_still_blocks_without_full_agreement() {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let mut local = local_release(&titles);
        local.album_title = Some("Exclusive Metal Sampler".to_string());
        let mut remote = remote_release(&titles);
        remote.title = "Exclusive Metal Radio Sampler".to_string();
        let blocked = |local: &LocalRelease, remote: &RemoteRelease| {
            let candidate = Candidate::new(local, remote.clone());
            !evaluate_for_matching(local, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        };

        // One duration two seconds off: inside the three-second tolerance, but
        // outside the one and a half seconds this rule needs.
        let mut drifting = local.clone();
        drifting.tracks[2].duration_secs = Some(drifting.tracks[2].duration_secs.unwrap() + 2.0);
        assert!(blocked(&drifting, &remote));

        // A track title that is only close: the mean falls under 0.97.
        let mut spelled = local.clone();
        spelled.tracks[3].title = Some("Fou".to_string());
        assert!(blocked(&spelled, &remote));

        // A title that is really different.
        let mut other = remote.clone();
        other.title = "Completely Different Thing".to_string();
        assert!(blocked(&local, &other));

        // A different artist.
        let mut artist = remote.clone();
        artist.artists = vec!["Another Band".to_string()];
        assert!(blocked(&local, &artist));
    }

    const TEN_TITLES: [&str; 10] = [
        "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
    ];

    #[test]
    fn a_weak_album_title_does_not_stack_with_a_weak_track_title() {
        let mut local = local_release(&TEN_TITLES);
        let mut remote = remote_release(&TEN_TITLES);
        remote.title = "Exclusive Metal Radio Sampler".to_string();
        // One short typo: 0.75 against "Four", mean still above 0.97.
        local.tracks[3].title = Some("Fou".to_string());

        // The setup is valid: with an exact album title the near-miss rule
        // accepts the lone weak track.
        let candidate = Candidate::new(&local, remote_release(&TEN_TITLES));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // With the weak album title the weak track title blocks.
        local.album_title = Some("Exclusive Metal Sampler".to_string());
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (0.70..0.90).contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert!(
            candidate.score.track_titles >= CORROBORATED_MEAN_TITLE,
            "{}",
            candidate.score.track_titles
        );
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("only scores")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_weak_album_title_needs_enough_tracks() {
        let weak_title_blocks = |titles: &[&str], various: bool| {
            let mut local = local_release(titles);
            local.album_title = Some("Exclusive Metal Sampler".to_string());
            let mut remote = remote_release(titles);
            remote.title = "Exclusive Metal Radio Sampler".to_string();
            if various {
                local.artist_kind = ArtistKind::VariousArtists;
                remote.artists = vec!["Various Artists".to_string()];
            }
            let candidate = Candidate::new(&local, remote);
            if various {
                assert_eq!(candidate.score.album_artist, 1.0);
            }
            let evaluation =
                evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
            if evaluation.auto_accept {
                return false;
            }
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|note| note.contains("album title or artist does not agree")),
                "{:?}",
                evaluation.blockers
            );
            true
        };
        assert!(weak_title_blocks(&["One"], false));
        assert!(weak_title_blocks(&["One", "Two"], false));
        // Three tracks are enough for an ordinary artist.
        assert!(!weak_title_blocks(&["One", "Two", "Three"], false));
        // A Various Artists release has no artist evidence: it needs five.
        assert!(weak_title_blocks(&["One", "Two", "Three", "Four"], true));
        assert!(!weak_title_blocks(
            &["One", "Two", "Three", "Four", "Five"],
            true
        ));
    }

    #[test]
    fn an_ambiguous_alternative_is_not_set_aside_by_durations() {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let local = local_release(&titles);
        let candidate = Candidate::new(&local, remote_release(&titles));
        let mut other_release = remote_release(&titles);
        other_release.id = ReleaseId::Discogs(46);
        let mut other = Candidate::new(&local, other_release);
        // Complete and within tolerance: nothing sets it aside.
        assert!(!fits_durations_better(&local, &candidate, &other));
        // Still one to one, only ambiguous: not set aside either.
        other.mapping.ambiguous = true;
        assert!(other.mapping.complete);
        assert!(!fits_durations_better(&local, &candidate, &other));

        // An incomplete mapping or a duration beyond tolerance still is.
        let mut short_release = remote_release(&titles[..4]);
        short_release.id = ReleaseId::Discogs(47);
        let short = Candidate::new(&local, short_release);
        assert!(!short.mapping.complete);
        assert!(fits_durations_better(&local, &candidate, &short));
        let mut off_release = remote_release(&titles);
        off_release.id = ReleaseId::Discogs(48);
        off_release.tracklist[0].duration_secs = Some(190.0);
        let off = Candidate::new(&local, off_release);
        assert!(fits_durations_better(&local, &candidate, &off));
    }

    #[test]
    fn one_slightly_different_title_with_the_right_duration_does_not_block_an_album() {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let mut local = local_release(&titles);
        local.tracks[2].title = Some("Thre".to_string()); // 0.80 against "Three"
        let remote = remote_release(&titles);
        let candidate = Candidate::new(&local, remote.clone());
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            evaluation
                .warnings
                .iter()
                .any(|note| note.contains("Three")),
            "{:?}",
            evaluation.warnings
        );

        // The same title with a duration well off is a different track.
        let mut off = local.clone();
        off.tracks[2].duration_secs = Some(off.tracks[2].duration_secs.unwrap() + 10.0);
        let candidate = Candidate::new(&off, remote.clone());
        assert!(
            !evaluate_for_matching(&off, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );

        // Two weak titles still block.
        let mut two = local.clone();
        two.tracks[4].title = Some("Fiv".to_string());
        let candidate = Candidate::new(&two, remote.clone());
        assert!(
            !evaluate_for_matching(&two, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );
    }

    #[test]
    fn one_track_of_any_title_passes_on_a_close_duration_in_a_larger_album() {
        let titles = [
            "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
            "Eleven", "Twelve",
        ];
        let remote = remote_release(&titles);

        // A wholly different title on a track whose length agrees.
        let mut local = local_release(&titles);
        local.tracks[2].title = Some("Something else entirely".to_string());
        let candidate = Candidate::new(&local, remote.clone());
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // The length must agree closely: two seconds off is not enough.
        let mut off = local.clone();
        off.tracks[2].duration_secs = Some(off.tracks[2].duration_secs.unwrap() + 2.0);
        let candidate = Candidate::new(&off, remote.clone());
        assert!(
            !evaluate_for_matching(&off, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );

        // Two different titles still block.
        let mut two = local.clone();
        two.tracks[4].title = Some("Another one entirely".to_string());
        let candidate = Candidate::new(&two, remote.clone());
        assert!(
            !evaluate_for_matching(&two, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );

        // A short album cannot carry a track on its length alone.
        let short_titles = ["One", "Two", "Three", "Four"];
        let mut short = local_release(&short_titles);
        short.tracks[2].title = Some("Something else entirely".to_string());
        let candidate = Candidate::new(&short, remote_release(&short_titles));
        assert!(
            !evaluate_for_matching(&short, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );
    }

    #[test]
    fn a_few_drifting_durations_pass_only_while_the_album_length_agrees() {
        let titles = [
            "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
            "Eleven", "Twelve",
        ];
        let remote = remote_release(&titles);
        let accepts = |local: &LocalRelease, remote: &RemoteRelease| {
            let candidate = Candidate::new(local, remote.clone());
            evaluate_for_matching(local, &candidate, &[], &DiscoveryEvidence::default()).auto_accept
        };

        // One track six seconds long.
        let mut local = local_release(&titles);
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 6.0);
        assert!(accepts(&local, &remote));

        // Two tracks: still within the allowance.
        local.tracks[7].duration_secs = Some(local.tracks[7].duration_secs.unwrap() - 8.0);
        assert!(accepts(&local, &remote));

        // Three is a different pressing, not drift.
        let mut three = local.clone();
        three.tracks[9].duration_secs = Some(three.tracks[9].duration_secs.unwrap() + 5.0);
        assert!(!accepts(&three, &remote));

        // A track more than ten seconds off is not drift.
        let mut far = local_release(&titles);
        far.tracks[3].duration_secs = Some(far.tracks[3].duration_secs.unwrap() + 11.0);
        assert!(!accepts(&far, &remote));

        // On a short album the same drift is too much of the whole.
        let short_titles = ["One", "Two", "Three", "Four"];
        let short_remote = remote_release(&short_titles);
        let mut short = local_release(&short_titles);
        short.tracks[0].duration_secs = Some(short.tracks[0].duration_secs.unwrap() + 9.0);
        short.tracks[1].duration_secs = Some(short.tracks[1].duration_secs.unwrap() + 9.0);
        assert!(!accepts(&short, &short_remote));

        // Too few tracks to judge the album by its length.
        let tiny_titles = ["One", "Two", "Three"];
        let mut tiny = local_release(&tiny_titles);
        tiny.tracks[0].duration_secs = Some(tiny.tracks[0].duration_secs.unwrap() + 5.0);
        assert!(!accepts(&tiny, &remote_release(&tiny_titles)));
    }

    #[test]
    fn album_matching_checks_every_close_alternative() {
        let local = local_release(&["One", "Two"]);
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut twin = remote_release(&["One", "Two"]);
        twin.id = ReleaseId::Discogs(43);
        let mut different = twin.clone();
        different.id = ReleaseId::Discogs(44);
        different.tracklist[0].artists = vec!["Different performer".to_string()];
        let candidates = [
            Candidate::new(&local, twin),
            Candidate::new(&local, different),
        ];
        let evaluation =
            evaluate_for_matching(&local, &first, &candidates, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("44"))
        );
    }

    #[test]
    fn album_scope_blocks_track_version_qualifier_mismatches() {
        let local = local_release(&["Yesterday", "Michelle"]);
        let remote = remote_release(&["Yesterday (Live)", "Michelle (Live)"]);
        let candidate = Candidate::new(&local, remote);
        let result = evaluate_at_scope(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence::default(),
            MatchScope::Album,
        );
        assert!(!result.auto_accept);
        assert!(
            result
                .blockers
                .iter()
                .any(|reason| reason.contains("track version qualifiers")),
            "{:?}",
            result.blockers
        );
    }

    #[test]
    fn album_scope_ignores_remastered_in_track_titles() {
        let local = local_release(&["Here Comes the Sun", "Something"]);
        let remote = remote_release(&["Here Comes the Sun (Remastered)", "Something"]);
        let candidate = Candidate::new(&local, remote);
        let result = evaluate_at_scope(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence::default(),
            MatchScope::Album,
        );
        // The measured text gates may still object to the changed title; the
        // qualifier rule itself must not.
        assert!(
            !result
                .blockers
                .iter()
                .any(|reason| reason.contains("track version qualifiers")),
            "{:?}",
            result.blockers
        );
    }

    /// A local release whose titles are `titles`, tagged with track numbers.
    fn local_titled(titles: &[String]) -> LocalRelease {
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        local_release(&refs)
    }

    /// `titles` with `NN - ` in front of each, as scene tags often have them.
    fn numbered(titles: &[&str]) -> Vec<String> {
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| format!("{:02} - {title}", index + 1))
            .collect()
    }

    fn album_scope_evaluation(local: &LocalRelease, remote: &RemoteRelease) -> Evaluation {
        let candidate = Candidate::new(local, remote.clone());
        evaluate_at_scope(
            local,
            &candidate,
            None,
            &DiscoveryEvidence::default(),
            MatchScope::Album,
        )
    }

    /// The similarity the mapping records or computes for local track `index`
    /// against the remote track at the same index.
    fn pair_similarity_at(local: &LocalRelease, remote: &RemoteRelease, index: usize) -> f64 {
        let candidate = Candidate::new(local, remote.clone());
        candidate
            .mapping
            .title_similarity(
                local,
                &cleaned_track_titles(local),
                index,
                index,
                &remote.tracklist[index],
            )
            .unwrap()
    }

    const CLEAN_TITLES: [&str; 6] = [
        "Soul Pilgrim",
        "Far From Home",
        "Parallax",
        "Cheap Wine",
        "Temptress Luna",
        "Night Shift",
    ];

    #[test]
    fn a_number_prefix_equal_to_the_track_number_is_dropped_for_comparison() {
        let local = local_titled(&numbered(&CLEAN_TITLES));
        let remote = remote_release(&CLEAN_TITLES);
        assert!(local_similarity("01 - Soul Pilgrim", "Soul Pilgrim") < 0.9);
        for index in 0..CLEAN_TITLES.len() {
            let value = pair_similarity_at(&local, &remote, index);
            assert!((value - 0.98).abs() < 1e-9, "track {index}: {value}");
        }
        let evaluation = album_scope_evaluation(&local, &remote);
        assert!(
            evaluation.auto_accept,
            "{:?} {:?}",
            evaluation.blockers, evaluation.evidence
        );
    }

    #[test]
    fn a_cleaned_near_miss_title_passes_through_the_near_miss_exception() {
        // Four tracks with distinct titles: the single-wrong-track exception
        // needs five, so only the near-miss rule can pass the misspelt title.
        let remote = remote_release(&["Far From Home", "Soul Pilgrim", "Parallax", "Cheap Wine"]);
        let with_title = |first: &str| {
            local_titled(&[
                first.to_string(),
                "Soul Pilgrim".to_string(),
                "Parallax".to_string(),
                "Cheap Wine".to_string(),
            ])
        };

        let local = with_title("01-Far form Home");
        let raw = local_similarity("01-Far form Home", "Far From Home");
        assert!(raw < NEAR_MISS_TITLE_FLOOR, "{raw}");
        let cleaned = pair_similarity_at(&local, &remote, 0);
        assert!(
            (NEAR_MISS_TITLE_FLOOR..TITLE_SIMILARITY_THRESHOLD).contains(&cleaned),
            "{cleaned}"
        );
        let evaluation = album_scope_evaluation(&local, &remote);
        assert!(
            evaluation.auto_accept,
            "{:?} {:?}",
            evaluation.blockers, evaluation.evidence
        );
        assert!(
            evaluation
                .warnings
                .iter()
                .any(|warning| warning.contains("its duration agrees")),
            "{:?}",
            evaluation.warnings
        );

        // The number does not match track 1, so nothing is cleaned and the raw
        // reading alone blocks.
        let local = with_title("09-Far form Home");
        assert!(cleaned_track_titles(&local)[0].is_none());
        let evaluation = album_scope_evaluation(&local, &remote);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("only scores")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_release_tag_and_extension_are_dropped_along_with_the_number() {
        let junk = [
            "01-Soul_Pilgrim-SYN.mp3",
            "02-Far_From_Home-SYN.mp3",
            "03-Parallax-SYN.mp3",
            "04-Cheap_Wine-SYN.mp3",
        ];
        let local = local_titled(&junk.map(str::to_string));
        let remote = remote_release(&CLEAN_TITLES[..4]);
        for index in 0..junk.len() {
            let value = pair_similarity_at(&local, &remote, index);
            assert!((value - 0.98).abs() < 1e-9, "track {index}: {value}");
        }
        assert!(album_scope_evaluation(&local, &remote).auto_accept);
    }

    #[test]
    fn a_release_tag_shared_by_every_title_is_dropped_even_when_one_is_cut_off() {
        let cut = "Under The Midnight Sky Dive-am";
        assert_eq!(cut.chars().count(), 30);
        let titles = [
            "Soul Pilgrim-amrc",
            "Far From Home-amrc",
            "Parallax-amrc",
            cut,
            "Temptress Luna-amrc",
        ];
        let local = local_release(&titles);
        let remote = remote_release(&[
            "Soul Pilgrim",
            "Far From Home",
            "Parallax",
            "Under The Midnight Sky Dive",
            "Temptress Luna",
        ]);
        assert!(local_similarity("Soul Pilgrim-amrc", "Soul Pilgrim") < 0.8);
        for index in 0..titles.len() {
            let value = pair_similarity_at(&local, &remote, index);
            assert!((value - 0.98).abs() < 1e-9, "track {index}: {value}");
        }
        assert!(album_scope_evaluation(&local, &remote).auto_accept);
    }

    #[test]
    fn a_tag_title_that_lost_the_shared_token_entirely_does_not_block_it() {
        let lost = "Alpha Bravo Charlie Delta Echo";
        assert_eq!(lost.chars().count(), 30);
        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc", lost]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[0].as_ref().unwrap().text, "One");
        assert_eq!(cleaned[2].as_ref().unwrap().text, "Three");
        assert!(cleaned[3].is_none());

        // A title under 29 characters without it does.
        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc", "Four"]);
        assert!(cleaned_track_titles(&local).iter().all(Option::is_none));
    }

    #[test]
    fn an_untagged_track_takes_its_number_from_its_position() {
        let titles = numbered(&CLEAN_TITLES);
        let mut local = local_titled(&titles);
        for track in &mut local.tracks {
            track.track_number = None;
        }
        let remote = remote_release(&CLEAN_TITLES);
        for index in 0..CLEAN_TITLES.len() {
            let value = pair_similarity_at(&local, &remote, index);
            assert!((value - 0.98).abs() < 1e-9, "track {index}: {value}");
        }
    }

    #[test]
    fn an_untagged_track_counts_only_the_earlier_tracks_on_its_own_disc() {
        let mut local = local_release(&["01 - Alpha", "02 - Bravo", "01 - Charlie", "02 - Delta"]);
        for (track, disc) in local.tracks.iter_mut().zip([1, 1, 2, 2]) {
            track.track_number = None;
            track.disc = Some(disc);
        }
        let texts: Vec<_> = cleaned_track_titles(&local)
            .into_iter()
            .map(|cleaned| cleaned.unwrap().text)
            .collect();
        assert_eq!(texts, ["Alpha", "Bravo", "Charlie", "Delta"]);

        // The position on the disc, not in the release.
        local.tracks[2].title = Some("03 - Charlie".to_string());
        assert!(cleaned_track_titles(&local)[2].is_none());
    }

    #[test]
    fn a_cut_off_title_keeps_its_cut_off_reading_after_the_number_is_dropped() {
        let cut = "05 - Tonight Can't Come Too So";
        assert_eq!(cut.chars().count(), 30);
        let local = local_release(&["One", "Two", "Three", "Four", cut]);
        let remote =
            remote_release(&["One", "Two", "Three", "Four", "Tonight Can't Come Too Soon"]);
        assert!(local_similarity(cut, "Tonight Can't Come Too Soon") < 0.9);
        let value = pair_similarity_at(&local, &remote, 4);
        assert!((value - 0.95).abs() < 1e-9, "{value}");
    }

    #[test]
    fn the_cut_off_reading_is_not_allowed_once_a_tag_or_extension_was_removed() {
        let cleaned = |width| CleanedTitle {
            text: "Tonight Can't Come Too So".to_string(),
            cut_width: width,
        };
        let remote = "Tonight Can't Come Too Soon Tomorrow";
        let with = track_title_similarity("junk", Some(&cleaned(Some(30))), remote);
        let without = track_title_similarity("junk", Some(&cleaned(None)), remote);
        assert!((with - 0.95).abs() < 1e-9, "{with}");
        assert!(without < 0.9, "{without}");

        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc"]);
        assert_eq!(
            cleaned_track_titles(&local)[0].as_ref().unwrap().cut_width,
            None
        );
        let local = local_release(&["01 - One", "Two"]);
        assert_eq!(
            cleaned_track_titles(&local)[0].as_ref().unwrap().cut_width,
            Some(8)
        );
    }

    #[test]
    fn a_number_prefix_must_match_the_track_and_end_in_a_separator() {
        for (title, number, expected) in [
            ("01 - Soul Pilgrim", 1, Some("Soul Pilgrim")),
            ("03. Parallax", 3, Some("Parallax")),
            ("(02) Foo", 2, Some("Foo")),
            ("[03] Foo", 3, Some("Foo")),
            ("7_Seven Nation", 7, Some("Seven Nation")),
            ("12: Twelve", 12, Some("Twelve")),
            ("007 - Bond", 7, Some("Bond")),
            ("07 - Song", 3, None),
            ("1000 Ways", 10, None),
            ("100 Percent", 10, None),
            ("1979", 19, None),
            ("1979", 1979, None),
            ("19 79", 1, None),
            ("13", 13, None),
            ("13 -", 13, None),
            ("13 - ", 13, None),
            ("(13", 13, None),
            ("Song 13 - X", 13, None),
        ] {
            assert_eq!(
                strip_track_number_prefix(title, number),
                expected,
                "{title}"
            );
        }
    }

    #[test]
    fn a_wrong_or_bare_number_is_left_alone_in_a_release() {
        // Four tracks: the lone-track exception needs five.
        let mut titles = numbered(&CLEAN_TITLES[..4]);
        titles[2] = "07 - Song".to_string();
        let local = local_titled(&titles);
        assert!(cleaned_track_titles(&local)[2].is_none());
        let mut remote_titles = CLEAN_TITLES[..4].to_vec();
        remote_titles[2] = "Song";
        let remote = remote_release(&remote_titles);
        assert!(pair_similarity_at(&local, &remote, 2) < 0.75);
        assert!(!album_scope_evaluation(&local, &remote).auto_accept);

        let mut ten: Vec<&str> = vec!["One", "Two", "Three", "Four", "Five"];
        ten.extend(["Six", "Seven", "Eight", "Nine", "1000 Ways"]);
        let local = local_release(&ten);
        assert!(cleaned_track_titles(&local)[9].is_none());
        ten[9] = "100 Percent";
        let local = local_release(&ten);
        assert!(cleaned_track_titles(&local)[9].is_none());
        ten[9] = "1979";
        let local = local_release(&ten);
        assert!(cleaned_track_titles(&local)[9].is_none());
    }

    #[test]
    fn a_title_that_starts_like_its_own_number_keeps_its_raw_score() {
        let titles = ["a", "b", "c", "d", "e", "f", "7 Seconds"];
        let local = local_release(&titles);
        let remote = remote_release(&titles);
        assert_eq!(
            cleaned_track_titles(&local)[6].as_ref().unwrap().text,
            "Seconds"
        );
        assert_eq!(pair_similarity_at(&local, &remote, 6), 1.0);
    }

    #[test]
    fn a_trailing_token_needs_three_tracks_that_all_share_it() {
        let none = |titles: &[&str]| cleaned_track_titles(&local_release(titles));

        // Only two of three end with it.
        assert!(
            none(&["A Song-amrc", "B Song-amrc", "C Song"])
                .iter()
                .all(Option::is_none)
        );
        // Two tracks are too few.
        assert!(
            none(&["A Song-amrc", "B Song-amrc"])
                .iter()
                .all(Option::is_none)
        );
        // A different token on one track.
        assert!(
            none(&["A Song-amrc", "B Song-amrc", "C Song-xyz"])
                .iter()
                .all(Option::is_none)
        );
        // A space before the separator is a subtitle, not a tag.
        assert!(
            none(&["One - Seattle", "Two - Seattle", "Three - Seattle"])
                .iter()
                .all(Option::is_none)
        );

        let cleaned = none(&["A Song-AMRC", "B Song_amrc", "C Song-amrc"]);
        let texts: Vec<_> = cleaned
            .iter()
            .map(|c| c.as_ref().unwrap().text.as_str())
            .collect();
        assert_eq!(texts, ["A Song", "B Song", "C Song"]);
    }

    #[test]
    fn a_trailing_token_has_the_shape_of_a_release_tag() {
        let shared = |token: &str| {
            let titles = [
                format!("One-{token}"),
                format!("Two-{token}"),
                format!("Three-{token}"),
            ];
            let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
            shared_trailing_token(&refs).map(str::to_string)
        };
        assert_eq!(shared("SYN").as_deref(), Some("SYN"));
        assert_eq!(shared("1up").as_deref(), Some("1up"));
        assert_eq!(shared("ab").as_deref(), Some("ab"));
        assert_eq!(shared("abcdefgh").as_deref(), Some("abcdefgh"));
        assert_eq!(shared("a"), None);
        assert_eq!(shared("abcdefghi"), None);
        // At least one letter.
        assert_eq!(shared("2000"), None);
        // Not ASCII alphanumerics.
        assert_eq!(shared("caf\u{e9}"), None);
        // Version and edition words are meaningful.
        for word in [
            "live",
            "demo",
            "remix",
            "remastered",
            "Instrumental",
            "deluxe",
            "edit",
            "acoustic",
        ] {
            assert_eq!(shared(word), None, "{word}");
        }
    }

    #[test]
    fn a_trailing_audio_extension_is_dropped() {
        assert_eq!(strip_audio_extension("Song.mp3"), Some("Song"));
        assert_eq!(strip_audio_extension("Song.FLAC"), Some("Song"));
        assert_eq!(strip_audio_extension("A.B.ogg"), Some("A.B"));
        assert_eq!(strip_audio_extension("Song.txt"), None);
        assert_eq!(strip_audio_extension("Song"), None);
        assert_eq!(strip_audio_extension("Song.mp3x"), None);
        assert_eq!(strip_audio_extension(".mp3"), None);
        assert_eq!(strip_audio_extension("Mr. Mp3"), None);

        let local = local_release(&["Alpha.mp3 ", "Bravo.flac"]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[0].as_ref().unwrap().text, "Alpha");
        assert_eq!(cleaned[1].as_ref().unwrap().text, "Bravo");
    }

    #[test]
    fn trailing_padding_is_ignored_before_cleaning() {
        let local = local_release(&["01 - Alpha            ", "Bravo"]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[0].as_ref().unwrap().text, "Alpha");
    }

    #[test]
    fn version_words_are_never_stripped_as_a_release_tag() {
        for word in ["live", "demo"] {
            let titles: Vec<String> = ["Yesterday", "Michelle", "Girl"]
                .iter()
                .map(|title| format!("{title}-{word}"))
                .collect();
            let local = local_titled(&titles);
            assert!(
                cleaned_track_titles(&local).iter().all(Option::is_none),
                "{word}"
            );
            let remote = remote_release(&["Yesterday", "Michelle", "Girl"]);
            let evaluation = album_scope_evaluation(&local, &remote);
            assert!(!evaluation.auto_accept, "{word}");
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|reason| reason.contains("track version qualifiers")),
                "{word}: {:?}",
                evaluation.blockers
            );
        }
    }

    #[test]
    fn a_decoded_mapping_scores_junk_titles_like_the_fresh_one() {
        let junk = [
            "01-Soul_Pilgrim-SYN.mp3",
            "02-Far_From_Home-SYN.mp3",
            "03-Parallax-SYN.mp3",
            "04-Cheap_Wine-SYN.mp3",
        ];
        let local = local_release(&junk);
        let remote = remote_release(&CLEAN_TITLES[..4]);
        let candidate = Candidate::new(&local, remote.clone());

        let json = serde_json::to_value(&candidate.mapping).unwrap();
        let decoded: Mapping = serde_json::from_value(json).unwrap();
        assert!(decoded.pair_similarity.is_empty());
        let rescored = score(&local, &remote, &decoded);
        assert_eq!(rescored, candidate.score);
        assert!((rescored.track_titles - 0.98).abs() < 1e-9, "{rescored:?}");
        let cleaned = cleaned_track_titles(&local);
        for index in 0..junk.len() {
            assert_eq!(
                decoded.title_similarity(&local, &cleaned, index, index, &remote.tracklist[index]),
                candidate.mapping.title_similarity(
                    &local,
                    &cleaned,
                    index,
                    index,
                    &remote.tracklist[index]
                ),
            );
        }
    }

    #[test]
    fn version_abbreviations_are_never_a_release_tag() {
        let shared = |token: &str| {
            let titles = [
                format!("One-{token}"),
                format!("Two-{token}"),
                format!("Three-{token}"),
            ];
            let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
            shared_trailing_token(&refs).map(str::to_string)
        };
        for word in NON_TAG_WORDS {
            for token in [
                word.to_string(),
                word.to_uppercase(),
                format!("{}{}", word[..1].to_uppercase(), &word[1..]),
            ] {
                if (SHARED_TOKEN_MIN_CHARS..=SHARED_TOKEN_MAX_CHARS).contains(&token.len()) {
                    assert_eq!(shared(&token), None, "{token}");
                }
            }
        }
        for token in ["Rmx", "Karaoke", "Inst", "Vocal", "Hop", "Ab"] {
            assert_eq!(shared(token), None, "{token}");
        }
        // Scene tags: lowercase, uppercase and mixed case.
        for token in ["amrc", "SYN", "LATW", "ViLE", "iTS", "rH", "1up"] {
            assert_eq!(shared(token).as_deref(), Some(token), "{token}");
        }
        assert!(VERSION_WORDS.contains(&"instrumentals"));
        // Digits after a version word, `v` and digits, and further spellings.
        for token in [
            "inst2", "Inst2", "rmx2", "take2", "ver2", "v2", "V10", "acapela", "tv", "TV",
        ] {
            assert_eq!(shared(token), None, "{token}");
        }
        // Words that merely start with `v`, or are digits after a tag.
        for token in ["vile", "amrc2", "vx"] {
            assert_eq!(shared(token).as_deref(), Some(token), "{token}");
        }
        for word in NON_TAG_WORDS {
            assert!(word.len() <= SHARED_TOKEN_MAX_CHARS, "{word}");
        }
    }

    #[test]
    fn a_title_case_spelling_anywhere_rules_the_token_out() {
        for titles in [
            ["One-Hop", "Two-HOP", "Three-HOP"],
            ["One-HOP", "Two-Hop", "Three-Hop"],
            ["One-HOP", "Two-HOP", "Three-Hop"],
        ] {
            assert_eq!(shared_trailing_token(&titles), None, "{titles:?}");
        }
        assert_eq!(
            shared_trailing_token(&["One-HOP", "Two-HOP", "Three-hop"]),
            Some("HOP")
        );
    }

    #[test]
    fn an_instrumental_release_tagged_inst_stays_blocked_against_the_vocal_release() {
        let vocal = ["Yesterday", "Michelle", "Girl", "Help"];
        let remote = remote_release(&vocal);
        for suffix in ["Inst", "inst", "INST", "Rmx", "Karaoke", "Vocal"] {
            let titles: Vec<String> = vocal
                .iter()
                .map(|title| format!("{title}-{suffix}"))
                .collect();
            let local = local_titled(&titles);
            assert!(
                cleaned_track_titles(&local).iter().all(Option::is_none),
                "{suffix}"
            );
            let evaluation = album_scope_evaluation(&local, &remote);
            assert!(
                !evaluation.auto_accept,
                "{suffix}: {:?}",
                evaluation.evidence
            );
            assert!(!evaluation.blockers.is_empty(), "{suffix}");
        }
    }

    #[test]
    fn one_title_cannot_establish_a_shared_token() {
        let long_one = "A Rather Long Title That Goes On And On";
        let long_two = "Another Rather Long Title That Goes On And On";
        assert!(long_one.chars().count() > ID3V1_FIELD_CHARS);
        assert!(long_two.chars().count() > ID3V1_FIELD_CHARS);
        // Only one title ends in `hop`; the long ones cannot have lost it.
        let local = local_release(&["Hip-hop", long_one, long_two]);
        assert!(cleaned_track_titles(&local).iter().all(Option::is_none));
        let local = local_release(&["Hip-Hop", long_one, long_two]);
        assert!(cleaned_track_titles(&local).iter().all(Option::is_none));

        // Two of three, the third a 30-character title without it.
        let lost = "Alpha Bravo Charlie Delta Echo";
        assert_eq!(lost.chars().count(), 30);
        let local = local_release(&["One-amrc", "Two-amrc", lost]);
        assert!(cleaned_track_titles(&local).iter().all(Option::is_none));
    }

    #[test]
    fn a_title_too_long_to_have_been_cut_must_end_in_the_token() {
        let long = "Alpha Bravo Charlie Delta Echo Foxtrot";
        assert!(long.chars().count() > 30);
        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc", long]);
        assert!(cleaned_track_titles(&local).iter().all(Option::is_none));

        let tagged = format!("{long}-amrc");
        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc", &tagged]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[3].as_ref().unwrap().text, long);
    }

    #[test]
    fn three_full_token_titles_and_a_cut_off_one_without_it_still_strip() {
        let lost = "Alpha Bravo Charlie Delta Echo";
        assert_eq!(lost.chars().count(), 30);
        let local = local_release(&["One-amrc", "Two-amrc", "Three-amrc", lost]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[0].as_ref().unwrap().text, "One");
        assert!(cleaned[3].is_none());

        // A 30-character title with the whole token counts as one of the three.
        let full = "Alpha Bravo Charlie Delta-amrc";
        assert_eq!(full.chars().count(), 30);
        let local = local_release(&["One-amrc", "Two-amrc", full]);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(
            cleaned[2].as_ref().unwrap().text,
            "Alpha Bravo Charlie Delta"
        );
    }

    #[test]
    fn a_track_with_no_disc_counts_as_disc_one_when_numbering_by_position() {
        let mut local = local_release(&["01 - Alpha", "02 - Bravo", "03 - Charlie"]);
        for (track, disc) in local.tracks.iter_mut().zip([Some(1), Some(1), None]) {
            track.track_number = None;
            track.disc = disc;
        }
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(cleaned[2].as_ref().unwrap().text, "Charlie");
    }
}
