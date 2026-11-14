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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const DISCOGS_ALGORITHM_VERSION: &str = "discogs-match-v27";
pub const MUSICBRAINZ_ALGORITHM_VERSION: &str = "musicbrainz-match-v26";

pub const SCORE_THRESHOLD: f64 = 0.95;
pub const RUNNER_UP_GAP: f64 = 0.05;
pub const TITLE_SIMILARITY_THRESHOLD: f64 = 0.90;
pub const DURATION_TOLERANCE_SECS: f64 = 3.0;

/// The album-title rule: an album title between this floor and
/// [`TITLE_SIMILARITY_THRESHOLD`] may pass when everything else agrees tightly.
/// The rest of the album, scored without the title, must reach
/// `CORROBORATED_SCORE`, every mapped duration must be within
/// `CORROBORATED_DURATION_SECS`, and the mean track-title score must reach
/// `CORROBORATED_MEAN_TITLE`. A local release with no usable album title
/// (missing or filler) skips the title floor and the title condition, and
/// follows the rest of the rule.
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

/// The duration-identity rule: when the album title and artist agree and every
/// length of an album of at least `DURATION_IDENTITY_MIN_TRACKS` tracks is
/// within `DURATION_IDENTITY_GAP_SECS` of the provider's, in track order, the
/// lengths identify the tracks and any number of weak track titles stop
/// blocking. The album score without the track titles must still reach
/// `DURATION_IDENTITY_SCORE`.
pub const DURATION_IDENTITY_MIN_TRACKS: usize = 5;
pub const DURATION_IDENTITY_GAP_SECS: f64 = 1.5;
pub const DURATION_IDENTITY_SCORE: f64 = 0.95;

/// The text-identity rule: when the provider lists no length for some of the
/// tracks of an album of at least `TEXT_IDENTITY_MIN_TRACKS` tracks (at least
/// `TEXT_IDENTITY_MIN_VARIOUS_TRACKS` for Various Artists), and album title,
/// artist and every track title agree, in track order, the titles identify the
/// tracks. The album score without the durations must reach
/// `TEXT_IDENTITY_SCORE`, and every length the provider does list must agree.
pub const TEXT_IDENTITY_MIN_TRACKS: usize = 3;
pub const TEXT_IDENTITY_MIN_VARIOUS_TRACKS: usize = 5;
pub const TEXT_IDENTITY_SCORE: f64 = 0.95;

/// What the warning of an accept under the text-identity rule ends with, so
/// that the runner can print it next to the unconfirmed-edition note.
pub const UNCONFIRMED_LENGTHS_NOTE: &str = "track lengths are unconfirmed";

/// The endings that mark a warning as the note of an acceptance rule, which the
/// runner prints under the accept (see [`Evaluation::rule_notes`]).
const RULE_NOTES: &[&str] = &[UNCONFIRMED_LENGTHS_NOTE];

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

/// The order of a local release's tracks: disc (missing counts as 1), track
/// number (missing sorts last), then case-insensitive path.
///
/// `store::order_release_tracks` sorts with this, and
/// [`mapped_in_local_order`] checks the invariant against it. It lives here
/// because `store` depends on this module and not the other way around.
pub fn local_track_order(a: &LocalTrack, b: &LocalTrack) -> std::cmp::Ordering {
    a.disc
        .unwrap_or(1)
        .cmp(&b.disc.unwrap_or(1))
        .then(
            a.track_number
                .unwrap_or(i64::MAX)
                .cmp(&b.track_number.unwrap_or(i64::MAX)),
        )
        .then_with(|| {
            a.path
                .to_ascii_lowercase()
                .cmp(&b.path.to_ascii_lowercase())
        })
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

    /// The disc this track is on, when its position names one explicitly: a
    /// medium and track position (MusicBrainz) or a Discogs `N-M` position.
    /// `None` for a vinyl side (`A1`), `CD1-1`, and a bare number such as `10`,
    /// which [`Self::position_key`] reads as a track on disc 1 for pairing but
    /// which says nothing about the medium.
    pub fn medium(&self) -> Option<u32> {
        if let (Some(medium), Some(_)) = (self.medium_position, self.track_position) {
            return Some(medium);
        }
        let parsed = parse_position(self.position.as_deref()?);
        if parsed.side.is_some() {
            return None;
        }
        parsed.track.and(parsed.disc)
    }

    /// Whether the position is a bare track number (`15`): no medium or side,
    /// and no explicit medium position.
    fn is_bare_number(&self) -> bool {
        if self.medium_position.is_some() && self.track_position.is_some() {
            return false;
        }
        self.position.as_deref().is_some_and(|raw| {
            let parsed = parse_position(raw);
            parsed.side.is_none() && parsed.track.is_some() && parsed.disc.is_none()
        })
    }

    /// Explicit medium/track position, then a Discogs-style position string.
    pub fn position_key(&self) -> Option<(u32, u32)> {
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
        self.playable_tracks_in(None)
    }

    /// The medium of every tracklist entry, by index. An entry whose position
    /// names its medium ([`RemoteTrack::medium`]) has it. On Discogs, a bare
    /// number (`15`) inherits the medium of the nearest explicit `N-M` entry
    /// before it, since some releases leave the `N-` off a few tracks
    /// (`1-14`, `15`, `16`, `2-1`); a bare number with no explicit entry before
    /// it, and every other entry, has none. Everything that asks which medium a
    /// track is on goes through this, so they agree.
    pub fn track_media(&self) -> Vec<Option<u32>> {
        let discogs = matches!(self.id, ReleaseId::Discogs(_));
        let mut current = None;
        self.tracklist
            .iter()
            .map(|track| {
                if let Some(medium) = track.medium() {
                    current = Some(medium);
                    Some(medium)
                } else if discogs && track.is_bare_number() {
                    current
                } else {
                    None
                }
            })
            .collect()
    }

    /// The medium of the tracklist entry at `index` ([`Self::track_media`]).
    pub fn medium_of(&self, index: usize) -> Option<u32> {
        self.track_media().get(index).copied().flatten()
    }

    /// [`Self::playable_tracks`], limited to one medium when `medium` is given.
    pub fn playable_tracks_in(&self, medium: Option<u32>) -> Vec<(usize, &RemoteTrack)> {
        let media = self.track_media();
        self.tracklist
            .iter()
            .enumerate()
            .filter(|(index, track)| {
                track.is_playable() && medium.is_none_or(|medium| media[*index] == Some(medium))
            })
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

/// The words after which a roman numeral is read as a number.
const NUMBERING_WORDS: &[&str] = &["vol", "vols", "volume", "volumes", "part", "pt", "chapter"];

/// The value of a roman numeral written with `i`, `v` and `x` (1 to 39), or
/// `None` for any other text, including spellings that are not canonical
/// (`iiii`, `vx`).
fn roman_value(letters: &str) -> Option<u32> {
    let value_of = |ch: char| match ch {
        'i' => Some(1i32),
        'v' => Some(5),
        'x' => Some(10),
        _ => None,
    };
    let values: Vec<i32> = letters.chars().map(value_of).collect::<Option<_>>()?;
    let mut total = 0i32;
    for (index, value) in values.iter().enumerate() {
        if values.get(index + 1).is_some_and(|next| next > value) {
            total -= value;
        } else {
            total += value;
        }
    }
    let total = u32::try_from(total).ok().filter(|total| *total >= 1)?;
    let (tens, ones) = (total / 10, total % 10);
    let digit = ["", "i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix"][ones as usize];
    (format!("{}{digit}", "x".repeat(tens as usize)) == letters).then_some(total)
}

/// The disc and format words that a number in front of makes decoration
/// (`2CD`, `2 CD`, `(3 LP)`) in [`title_numbers`].
const COUNTED_FORMAT_WORDS: &[&str] = &["cd", "cds", "lp", "dvd"];

/// The tokens of `text`, with a volume word that is joined to its number by a
/// dot (`Vol.II`, `Pt.2`) split in two.
fn numbering_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    for token in text.split_whitespace() {
        if let Some((word, number)) = token.split_once('.')
            && number.chars().any(char::is_alphanumeric)
            && NUMBERING_WORDS.contains(&alphanumeric_lowercase(word).as_str())
        {
            tokens.push(&token[..=word.len()]);
            tokens.push(number);
        } else {
            tokens.push(token);
        }
    }
    tokens
}

/// The numbers an album title carries, in order, as decimal text without
/// leading zeros (`02` and `2` are the same): its digit runs, and a roman
/// numeral after a volume word ([`NUMBERING_WORDS`], also `Vol.II`), read after
/// the decoration is dropped (`Fools Gold 7"` and `Album CD2` have none). A
/// number in front of a disc or format word (`2CD`, `2 CD`;
/// [`COUNTED_FORMAT_WORDS`]) is decoration too.
fn title_numbers(text: &str) -> Vec<String> {
    let tokens = numbering_tokens(text);
    let spelled: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let numeral = index
                .checked_sub(1)
                .filter(|before| {
                    NUMBERING_WORDS.contains(&alphanumeric_lowercase(tokens[*before]).as_str())
                })
                .and_then(|_| roman_value(&alphanumeric_lowercase(token)));
            numeral.map_or_else(|| (*token).to_string(), |value| value.to_string())
        })
        .collect();
    let spelled = without_counted_formats(&spelled.join(" "));
    let reduced = reduce_album_title(&spelled);
    let mut numbers = Vec::new();
    let mut run = String::new();
    for ch in reduced
        .as_deref()
        .unwrap_or(&spelled)
        .chars()
        .chain(std::iter::once(' '))
    {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else if !run.is_empty() {
            let trimmed = run.trim_start_matches('0');
            numbers.push(if trimmed.is_empty() { "0" } else { trimmed }.to_string());
            run.clear();
        }
    }
    numbers
}

/// `text` without the digit runs that stand in front of a disc or format word
/// ([`COUNTED_FORMAT_WORDS`]), as one token (`2CD`) or two (`2 CD`).
fn without_counted_formats(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut kept = String::new();
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            kept.push(chars[index]);
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        let mut ahead = index;
        while ahead < chars.len() && chars[ahead].is_whitespace() {
            ahead += 1;
        }
        let word_start = ahead;
        while ahead < chars.len() && chars[ahead].is_alphabetic() {
            ahead += 1;
        }
        let word: String = chars[word_start..ahead]
            .iter()
            .flat_map(|ch| ch.to_lowercase())
            .collect();
        let counted = COUNTED_FORMAT_WORDS.contains(&word.as_str())
            && !chars.get(ahead).is_some_and(|ch| ch.is_alphanumeric());
        if !counted {
            kept.extend(&chars[start..index]);
        }
    }
    kept
}

/// Whether `short` is `long` with some numbers left out, in order.
fn is_subsequence(short: &[String], long: &[String]) -> bool {
    let mut rest = long.iter();
    short.iter().all(|number| rest.any(|other| other == number))
}

/// Whether the numbers in the album titles of a text-only match agree (see
/// [`title_numbers`]). They contradict only when each title has a number the
/// other lacks, that is, neither sequence is the other with some numbers left
/// out: a number on one side only (`Album (1999)` against `Album`, `Album Vol. 2
/// (1999)` against `Album Vol. 2`) is no evidence of another release. A `local` title that fills an
/// ID3v1 field and reads as a cut-off copy of `remote` may also lack the numbers
/// at the end of it, as long as the numbers it has are the start of the
/// remote's.
fn album_numbers_agree(local: &str, remote: &str) -> bool {
    let (local_numbers, remote_numbers) = (title_numbers(local), title_numbers(remote));
    if is_subsequence(&local_numbers, &remote_numbers)
        || is_subsequence(&remote_numbers, &local_numbers)
    {
        return true;
    }
    may_be_cut_to_field(local)
        && (cut_off_similarity(local, remote).is_some() || id3v1_prefix(local, remote).is_some())
        && remote_numbers.starts_with(&local_numbers)
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

/// Score of a placeholder title against a remote title that is itself a
/// placeholder (`S/T`, `Self-Titled`) or names the artist: the same reading as
/// the placeholder, at a small cost.
const SELF_TITLED_REMOTE_SCORE: f64 = 0.97;

/// Score of a placeholder title against a remote title of `Untitled`. It sits
/// under the `0.90` title gate, so it can pass only through the weak album
/// title rule, which needs every track and length to agree.
const SELF_TITLED_UNTITLED_SCORE: f64 = 0.80;

/// Whether an album tag is a placeholder for "named after the artist":
/// `Self Titled`, `Self-Titled`, `S/T`, `ST`, `Same` or `Eponymous`, with or
/// without trailing release-type and format words (`Self Titled EP`,
/// `S/T 7inch`, `Same-7 Inch Vinyl`). `Self Titled Demo` is not one, since
/// `demo` says which release it is.
pub fn is_self_titled(title: &str) -> bool {
    let normalized = normalize_text(title);
    // Dropping trailing words keeps the start of the title; a leading `The` or
    // marker dropped as well would make `The Same` a placeholder.
    let reduced = reduce_album_title(title)
        .map(|reduced| normalize_text(&reduced))
        .filter(|reduced| normalized.starts_with(reduced.as_str()));
    let compact: String = reduced
        .as_deref()
        .unwrap_or(&normalized)
        .chars()
        .filter(|ch| *ch != ' ')
        .collect();
    matches!(compact.as_str(), "selftitled" | "st" | "same" | "eponymous")
}

/// Whether an album tag is a writer's filler (`Unknown Title`, `Unknown Album`,
/// `.`) rather than a title. `Untitled` is not: many releases are called that.
pub fn is_junk_title(title: &str) -> bool {
    title.trim() == "."
        || matches!(
            normalize_text(title).as_str(),
            "unknown title" | "unknown album"
        )
}

/// Whether an artist tag is a writer's filler (`Unknown Artist`).
pub fn is_junk_artist(artist: &str) -> bool {
    normalize_text(artist) == "unknown artist"
}

/// What a local release's album and artist tags say, with filler left out and
/// a self-titled placeholder read as the artist's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TagIdentity {
    /// `None` when the artist is missing, junk or Various Artists.
    pub artist: Option<String>,
    /// `None` when the title is missing or junk. A self-titled placeholder reads
    /// as the artist's name, or stays as written when there is no artist to name.
    pub title: Option<String>,
}

pub(crate) fn tag_identity(local: &LocalRelease) -> TagIdentity {
    let artist = local
        .album_artist
        .as_deref()
        .map(str::trim)
        .filter(|artist| {
            !artist.is_empty()
                && !is_junk_artist(artist)
                && !matches!(local.artist_kind, ArtistKind::VariousArtists)
                && !matches!(
                    normalize_text(artist).as_str(),
                    "various" | "various artists"
                )
        })
        .map(str::to_string);
    let title = local
        .album_title
        .as_deref()
        .filter(|title| !title.trim().is_empty() && !is_junk_title(title))
        .map(|title| match &artist {
            Some(artist) if is_self_titled(title) => artist.clone(),
            _ => title.to_string(),
        });
    TagIdentity { artist, title }
}

/// The album title to search with: the tag, or the artist's name for a
/// self-titled placeholder. A junk title is searched as written, since the
/// search may still turn up the artist's releases.
pub(crate) fn search_title(local: &LocalRelease) -> Option<String> {
    tag_identity(local).title.or_else(|| {
        local
            .album_title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(str::to_string)
    })
}

/// Album-title score of a self-titled placeholder against a remote release,
/// beyond the placeholder compared as written: the artist's name as the title
/// (at the release-type cost), a remote title that names one of its credited
/// artists or is a placeholder itself, and `Untitled`.
fn self_titled_score(local: &LocalRelease, remote: &RemoteRelease) -> f64 {
    let remote_title = normalize_text(&remote.title);
    if is_self_titled(&remote.title) {
        return SELF_TITLED_REMOTE_SCORE;
    }
    let mut best = if remote_title == "untitled" {
        SELF_TITLED_UNTITLED_SCORE
    } else {
        0.0
    };
    if let Some(artist) = tag_identity(local).artist {
        best = best
            .max((album_title_similarity(&artist, &remote.title) - ALBUM_TYPE_PENALTY).max(0.0));
        // The credit the title names must be the local artist: `Self Titled` by
        // `Band B` is not `Band A`, however many artists that release credits.
        let provider = remote.provider();
        if !remote_title.is_empty()
            && remote.artists.iter().any(|name| {
                normalize_artist_for(provider, name) == remote_title
                    && local_artist_similarity(
                        &artist,
                        &match provider {
                            ProviderKind::Discogs => strip_disambiguator(name),
                            ProviderKind::MusicBrainz => name.clone(),
                        },
                    ) >= TITLE_SIMILARITY_THRESHOLD
            })
        {
            best = best.max(SELF_TITLED_REMOTE_SCORE);
        }
    }
    best
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
    album_title_similarity_with(local, remote, true)
}

/// [`album_title_similarity`], where `allow_cut_off` is false when the local
/// title was shortened on purpose, so it is never read as a cut-off tag.
fn album_title_similarity_with(local: &str, remote: &str, allow_cut_off: bool) -> f64 {
    let mut best = local_similarity_with(local, remote, allow_cut_off);
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
                let reduced = local_similarity_with(
                    &local_album,
                    &remote_album,
                    allow_cut_off && local_readings.is_empty(),
                );
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

/// The default channel layout, which does not distinguish an edition.
const STEREO: &str = "stereo";

/// True when two qualifier sets describe the same edition.
///
/// `stereo` is the default, so it is ignored in a set that does not also say
/// `mono`. A set with both (`Mono` and `Stereo` formats, or a disambiguation
/// like `mono & stereo`) names a distinct edition and keeps `stereo`, so it
/// agrees only with an identical set. `mono` on one side only is therefore a
/// contradiction. `edition_qualifiers` still reports `stereo`, which keeps it
/// in messages.
fn qualifiers_agree(a: &BTreeSet<&'static str>, b: &BTreeSet<&'static str>) -> bool {
    significant_qualifiers(a) == significant_qualifiers(b)
}

/// The qualifiers of a set that distinguish an edition: all of them, except
/// `stereo` when the set does not also say `mono` (see [`qualifiers_agree`]).
fn significant_qualifiers(set: &BTreeSet<&'static str>) -> BTreeSet<&'static str> {
    let keep_stereo = set.contains("mono");
    set.iter()
        .copied()
        .filter(|qualifier| *qualifier != STEREO || keep_stereo)
        .collect()
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
    /// The one medium of the release that the files were mapped onto, when they
    /// are a single disc of a multi-disc release. `None` maps the whole release,
    /// which is also what a mapping stored before this field existed meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u32>,
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

// ---------------------------------------------------------------------------
// Disc folders
// ---------------------------------------------------------------------------

/// Words that name a disc in a folder or album title: `CD2`, `Disc 2`, `Disk 2`.
const DISC_WORDS: [&str; 3] = ["cd", "disc", "disk"];
/// A disc number has one to three digits (`CD1234` is no disc).
const DISC_NUMBER_MAX_DIGITS: usize = 3;

/// Where a [`DiscHint`] was read from. When several sources are present they
/// agree, and the first of these names the hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscHintSource {
    /// The last component of the folder path (`.../CD2`).
    Folder,
    /// A disc marker ending the album title (`Album (Disc 2)`).
    AlbumTitle,
    /// The disc tag, which every file carries with the same value.
    DiscTags,
}

impl DiscHintSource {
    /// The source as it reads in the evidence line.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Folder => "folder name",
            Self::AlbumTitle => "album title",
            Self::DiscTags => "disc tags",
        }
    }
}

/// Which disc of a multi-disc release the files of a local release claim to be.
///
/// Used only to compare against a candidate's media; nothing is stored for it,
/// since the path, album tag and disc tags it comes from are already part of the
/// local release's source revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscHint {
    pub number: u32,
    pub source: DiscHintSource,
}

/// A disc number of one to three digits, at least 1.
fn disc_number(digits: &str) -> Option<u32> {
    if digits.is_empty()
        || digits.len() > DISC_NUMBER_MAX_DIGITS
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok().filter(|number| *number >= 1)
}

/// The disc a folder is named after: its last path component (split on `/` and
/// `\`) is exactly `cd`, `disc` or `disk` in any case, one optional separator
/// from ` _.-`, and a disc number. `CD1`, `disc_2` and `Disk 2` qualify;
/// `Discography`, `CD1 Bonus` and `DVD` do not.
pub fn folder_disc_number(directory: &str) -> Option<u32> {
    let name = directory
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()?;
    let lower = name.to_ascii_lowercase();
    let rest = DISC_WORDS
        .iter()
        .find_map(|word| lower.strip_prefix(word))?;
    disc_number(rest.strip_prefix([' ', '_', '.', '-']).unwrap_or(rest))
}

/// A disc marker ending an album title, with the album before it: `Album CD2`,
/// `Album Disc 2`, `Album (Disc 2)`, `Album [CD2]`, an unclosed `Album (CD2`, or
/// `Album - CD2`. The marker is a word of its own (`CDS2` and `Hitdisc 2` are
/// not markers) and at least one word must remain.
pub fn title_disc_suffix(title: &str) -> Option<(String, u32)> {
    let mut rest = title.trim_end();
    if let Some(open) = rest.strip_suffix([')', ']']) {
        rest = open.trim_end();
    }
    let digits_start = rest.trim_end_matches(|ch: char| ch.is_ascii_digit()).len();
    let number = disc_number(&rest[digits_start..])?;
    let before = &rest[..digits_start];
    let before = before.strip_suffix(' ').unwrap_or(before);
    let word_start = before
        .trim_end_matches(|ch: char| ch.is_ascii_alphabetic())
        .len();
    if !DISC_WORDS
        .iter()
        .any(|word| before[word_start..].eq_ignore_ascii_case(word))
    {
        return None;
    }
    let head = &before[..word_start];
    if head
        .chars()
        .next_back()
        .is_some_and(|ch| !(ch.is_whitespace() || matches!(ch, '(' | '[' | '-' | '_')))
    {
        return None;
    }
    let album =
        head.trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, '(' | '[' | '-' | '_'));
    album
        .chars()
        .any(char::is_alphanumeric)
        .then(|| (album.to_string(), number))
}

/// The disc every file's tag names, when they all name the same one and it is
/// disc 2 or later. A uniform tag of disc 1 is what a missing tag means and what
/// per-disc rips carry on every disc, so it is no evidence either way.
fn disc_tag_number(local: &LocalRelease) -> Option<u32> {
    let first = local.tracks.first()?.disc?;
    local
        .tracks
        .iter()
        .all(|track| track.disc == Some(first))
        .then(|| u32::try_from(first).ok())
        .flatten()
        .filter(|number| *number >= 2)
}

/// The disc a local release says it is, from its folder name, its album title
/// and its disc tags. The sources that say anything must agree, or there is no
/// hint. Disc tags only count from disc 2 on, because a missing tag already
/// reads as disc 1, so a uniform tag of 1 neither gives a hint nor conflicts with
/// a folder or title that names a disc; mixed or partly tagged files give no tag
/// hint at all.
pub fn local_disc_hint(local: &LocalRelease) -> Option<DiscHint> {
    let sources: Vec<DiscHint> = [
        folder_disc_number(&local.directory).map(|number| (number, DiscHintSource::Folder)),
        local
            .album_title
            .as_deref()
            .and_then(title_disc_suffix)
            .map(|(_, number)| (number, DiscHintSource::AlbumTitle)),
        disc_tag_number(local).map(|number| (number, DiscHintSource::DiscTags)),
    ]
    .into_iter()
    .flatten()
    .map(|(number, source)| DiscHint { number, source })
    .collect();
    let first = *sources.first()?;
    if sources.iter().any(|hint| hint.number != first.number) {
        return None;
    }
    Some(first)
}

/// Whether a Discogs release names a video format (`DVD`, `Blu-ray`, `VHS`).
///
/// Discogs positions do not say which medium is the video one, so such a release
/// cannot be split into discs safely. MusicBrainz reports video media in its
/// structure issues, which block on their own, so it is never refused here.
pub fn lists_video_format(remote: &RemoteRelease) -> bool {
    remote.provider() == ProviderKind::Discogs
        && remote.formats.iter().any(|format| {
            let lower = format.to_ascii_lowercase();
            ["dvd", "blu-ray", "bluray", "vhs"]
                .iter()
                .any(|video| lower.contains(video))
        })
}

/// `disc m of M` for the medium of a release that files were mapped onto, with
/// `M` the highest medium number (numbering may have gaps), or `disc m` when the
/// release does not name its media.
pub fn disc_label(remote: &RemoteRelease, medium: u32) -> String {
    match remote_media(remote).and_then(|media| media.keys().next_back().copied()) {
        Some(last) => format!("disc {medium} of {last}"),
        None => format!("disc {medium}"),
    }
}

/// The playable tracks on each medium of a release, when it has several and
/// every playable track has a medium (see [`RemoteRelease::track_media`]).
/// Vinyl sides (`A1`) and positions such as `CD1-1` name no medium, and a bare
/// Discogs position (`1`, `10`) has one only after an explicit `N-M` entry, so a
/// release with any such track gives `None`: one stray `10-68` entry after plain
/// positions does not make a single CD a set, while `1-14`, `15`, `16`, `2-1`
/// is two discs.
pub fn remote_media(remote: &RemoteRelease) -> Option<BTreeMap<u32, usize>> {
    let track_media = remote.track_media();
    let mut media = BTreeMap::new();
    for (index, _) in remote.playable_tracks() {
        *media.entry(track_media[index]?).or_insert(0usize) += 1;
    }
    (media.len() >= 2).then_some(media)
}

/// Maps local files to remote tracklist entries.
///
/// Strong identifiers anchor first: unique release-track IDs, then unique
/// recording IDs, then unique `(medium, track)` positions. Everything else goes
/// through a minimum-cost one-to-one assignment over normalized title
/// similarity and duration, with ties broken deterministically and flagged as
/// ambiguous. Repeated recording IDs are legal on a release, so they only
/// anchor when both sides are unambiguous.
///
/// The whole release is mapped first. When that is not complete because the
/// counts differ (a structure issue alone does not count) and the files are one
/// disc of a multi-disc release (see [`local_disc_hint`]) that the release has,
/// they are mapped onto that medium alone instead, complete or not, so that a
/// review sees the disc they claim to be. No other medium is tried. A Discogs
/// release that lists a video format ([`lists_video_format`]) is never split
/// into discs, since its tracklist does not say which medium is the video.
pub fn map_tracks(local: &LocalRelease, remote: &RemoteRelease) -> Mapping {
    let whole = map_tracks_within(local, remote, None);
    if whole.complete
        || local.tracks.len() == remote.playable_tracks().len()
        || lists_video_format(remote)
    {
        return whole;
    }
    let Some(hint) = local_disc_hint(local) else {
        return whole;
    };
    match remote_media(remote) {
        Some(media) if media.contains_key(&hint.number) => {
            map_tracks_within(local, remote, Some(hint.number))
        }
        _ => whole,
    }
}

/// [`map_tracks`] onto the whole release (`medium` is `None`) or onto the
/// playable tracks of one medium. Every pass, identifier anchors included, only
/// sees that medium's tracks, and every file is keyed by `(medium, track number)`
/// whatever its own disc tag says: the folder, title or tags said which disc the
/// files are.
fn map_tracks_within(local: &LocalRelease, remote: &RemoteRelease, medium: Option<u32>) -> Mapping {
    let playable = remote.playable_tracks_in(medium);
    // A bare Discogs number sits on the medium it inherits, not on disc 1.
    let track_media = remote.track_media();
    let remote_key = |index: usize| -> Option<(u32, u32)> {
        let track = &remote.tracklist[index];
        let (disc, number) = track.position_key()?;
        match track_media[index] {
            Some(inherited) if track.medium().is_none() => Some((inherited, number)),
            _ => Some((disc, number)),
        }
    };
    let key_of = |track: &LocalTrack, number: i64| -> (u32, u32) {
        (
            medium.unwrap_or_else(|| track.disc.unwrap_or(1).max(0) as u32),
            number.max(0) as u32,
        )
    };
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
            None => {
                let title = local.tracks[local_indexes[0]]
                    .title
                    .as_deref()
                    .unwrap_or(&local.tracks[local_indexes[0]].path);
                // Within one disc, an ID that the release has elsewhere is on another disc.
                let elsewhere = medium.is_some()
                    && remote
                        .tracklist
                        .iter()
                        .any(|track| track.is_playable() && track.release_track_id == Some(*id));
                notes.push(if elsewhere {
                    format!(
                        "release-track ID {id} from '{title}' is on another disc of this release"
                    )
                } else {
                    format!("release-track ID {id} from '{title}' is not on the remote release")
                });
            }
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
            .all(|index| remote_key(*index).is_some());

        if all_keyed {
            let mut local_by_key: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
            for index in &remaining_local {
                let track = &local.tracks[*index];
                let key = key_of(track, track.track_number.unwrap_or_default());
                local_by_key.entry(key).or_default().push(*index);
            }

            let mut remote_by_key: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
            for index in &remaining_remote {
                if let Some(key) = remote_key(*index) {
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
        let Some(found_key) = remote_key(remote_index) else {
            continue;
        };
        let Some(local_number) = local_track.track_number else {
            continue;
        };
        let local_key = key_of(local_track, local_number);
        if local_key != found_key {
            notes.push(format!(
                "identifier for '{}' points to remote {} but its tags say {}-{}",
                local_track.title.as_deref().unwrap_or(&local_track.path),
                remote_track
                    .position
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", found_key.0, found_key.1)),
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
        notes.push(match medium {
            Some(medium) => format!(
                "{} local file(s) versus {} playable track(s) on disc {medium}",
                local.tracks.len(),
                remote_count
            ),
            None => format!(
                "{} local file(s) versus {} playable remote track(s)",
                local.tracks.len(),
                remote_count
            ),
        });
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
        medium,
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

    /// The album score over the components other than the track titles, scaled
    /// so that a perfect rest scores 1.0. Used where the lengths identify the
    /// tracks, so the titles carry no extra evidence.
    pub fn album_score_without_track_titles(&self) -> f64 {
        (WEIGHT_ALBUM_TITLE * self.album_title
            + WEIGHT_ALBUM_ARTIST * self.album_artist
            + WEIGHT_DURATIONS * self.durations)
            / (1.0 - WEIGHT_YEAR - WEIGHT_TRACK_TITLES)
    }

    /// The album score over the components other than the durations, scaled so
    /// that a perfect rest scores 1.0. Used where the provider lists no lengths
    /// and the text alone identifies the tracks.
    pub fn album_score_without_durations(&self) -> f64 {
        (WEIGHT_ALBUM_TITLE * self.album_title
            + WEIGHT_ALBUM_ARTIST * self.album_artist
            + WEIGHT_TRACK_TITLES * self.track_titles)
            / (1.0 - WEIGHT_YEAR - WEIGHT_DURATIONS)
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
        // Filler is not a title, so it is no evidence for or against an album.
        Some(title) if is_junk_title(title) => 0.0,
        Some(title) if !title.trim().is_empty() => {
            let mut value = album_title_similarity(title, &remote.title);
            // A placeholder reads as the artist's name too; the better reading
            // counts, so a real album called `Same` still matches itself.
            if is_self_titled(title) {
                value = value.max(self_titled_score(local, remote));
            }
            // A disc marker on the title of the matched disc is not part of the
            // album's name. Nothing else is dropped: the album part is short on
            // purpose, so it is never read as a cut-off tag.
            if let Some(medium) = mapping.medium
                && let Some((album, number)) = title_disc_suffix(title)
                && number == medium
            {
                value = value.max(
                    (album_title_similarity_with(&album, &remote.title, false)
                        - ALBUM_TYPE_PENALTY)
                        .max(0.0),
                );
            }
            value
        }
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
    if remote.artists.is_empty() || is_junk_artist(artist) {
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

/// What a local release is ranked as: every album title it may go by, and its
/// artist. Built once per release by [`rank_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankIdentity {
    /// A self-titled placeholder is two readings, the artist's name and the tag
    /// as written, since a provider may list the album under either. Junk and
    /// missing titles have none.
    titles: Vec<String>,
    artist: Option<String>,
}

pub(crate) fn rank_identity(local: &LocalRelease) -> RankIdentity {
    let identity = tag_identity(local);
    let mut titles: Vec<String> = identity.title.into_iter().collect();
    if let Some(tag) = local.album_title.as_deref()
        && !is_junk_title(tag)
        && !tag.trim().is_empty()
        && !titles.iter().any(|title| title == tag)
    {
        titles.push(tag.to_string());
    }
    // Not `identity.artist`: a Various Artists release still ranks on its
    // credit, which providers list as `Various Artists`. Only junk is no hint.
    let artist = local
        .album_artist
        .as_deref()
        .filter(|artist| !is_junk_artist(artist))
        .map(str::to_string);
    RankIdentity { titles, artist }
}

impl RankIdentity {
    /// The best [`rank_search_summary_with`] over the title readings.
    pub(crate) fn rank(&self, local: &LocalRelease, summary: &SearchSummary) -> f64 {
        let artist = self.artist.as_deref();
        if self.titles.is_empty() {
            return rank_search_summary_with(local, None, artist, summary);
        }
        self.titles
            .iter()
            .map(|title| rank_search_summary_with(local, Some(title), artist, summary))
            .fold(0.0, f64::max)
    }
}

/// Ranks a search summary before spending a detailed release fetch on it.
///
/// A self-titled placeholder ranks as the better of the artist's name and the
/// tag as written, and junk tags rank as nothing (see [`tag_identity`]).
///
/// A test helper: discovery builds a [`rank_identity`] once per release and ranks
/// every hit with it.
#[cfg(test)]
pub fn rank_search_summary(local: &LocalRelease, summary: &SearchSummary) -> f64 {
    rank_identity(local).rank(local, summary)
}

/// [`rank_search_summary`] for the given album title and artist, which may
/// differ from the tags.
fn rank_search_summary_with(
    local: &LocalRelease,
    title: Option<&str>,
    artist: Option<&str>,
    summary: &SearchSummary,
) -> f64 {
    let mut rank = 0.0;

    if let Some(title) = title {
        rank += 0.5 * album_title_similarity(title, &summary.title);
    }
    if let Some(artist) = artist {
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
    /// Whether the evaluation accepts on the titles alone because the provider
    /// lists no length for some tracks (see [`titles_identify_tracks`]).
    pub fn relies_on_unlisted_lengths(&self) -> bool {
        self.warnings
            .iter()
            .any(|warning| warning.ends_with(UNCONFIRMED_LENGTHS_NOTE))
    }

    /// The warnings that carry the note of an acceptance rule, in order. The
    /// runner prints them under an album-scope accept.
    pub fn rule_notes(&self) -> impl Iterator<Item = &str> {
        self.warnings
            .iter()
            .map(String::as_str)
            .filter(|warning| RULE_NOTES.iter().any(|note| warning.ends_with(note)))
    }

    /// Manual acceptance can override score and identity uncertainty, but never
    /// an incomplete or structurally unsupported mapping.
    pub fn can_apply(&self, candidate: &Candidate) -> bool {
        candidate.mapping.complete && !candidate.release.has_unsupported_entries()
    }
}

/// What another candidate says about the acceptance rules for a candidate.
/// The default lets every rule apply.
#[derive(Debug, Clone)]
struct RuleGuards {
    /// The text-only rule ([`titles_identify_tracks`]) may apply.
    text_identity: bool,
    /// Why the odd-length rule must not apply, naming the alternative release.
    /// `None` lets the rule apply. The rule pushes the text as a blocker, and
    /// only when it would otherwise have applied. Reserved for the odd-length
    /// rule, which is not implemented yet.
    #[expect(
        dead_code,
        reason = "read by the odd-length rule, added in a later step"
    )]
    odd_length_blocker: Option<String>,
    /// Why the leading-tracks rule must not apply, naming the alternative
    /// release. `None` lets the rule apply. The rule pushes the text as a
    /// blocker, and only when it would otherwise have applied. Reserved for the
    /// leading-tracks rule, which is not implemented yet.
    #[expect(
        dead_code,
        reason = "read by the leading-tracks rule, added in a later step"
    )]
    leading_tracks_blocker: Option<String>,
}

impl Default for RuleGuards {
    fn default() -> Self {
        Self {
            text_identity: true,
            odd_length_blocker: None,
            leading_tracks_blocker: None,
        }
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
    // Another edition of the same provider that agrees in text but whose
    // lengths are far from the files contradicts a text-only match: the lengths
    // the candidate lacks may well be those, so the text cannot stand alone.
    let cleaned = cleaned_track_titles(local);
    let text_identity = !alternatives.iter().any(|other| {
        other.release.id != candidate.release.id
            && other.release.provider() == candidate.release.provider()
            && text_agrees_but_lengths_do_not(local, other, &cleaned, SOFT_DURATION_MAX_SECS)
    });
    let mut album = evaluate_with(
        local,
        candidate,
        None,
        discovery,
        MatchScope::Album,
        RuleGuards {
            text_identity,
            ..RuleGuards::default()
        },
    );
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

/// Whether the mapping pairs every local track, in local track order, with the
/// remote playable tracks in tracklist order: complete, unambiguous, as many
/// local tracks as playable remote tracks, and local track `k` mapped to remote
/// playable track `k`.
///
/// Two tracks of similar length cannot swap titles under such a mapping, which
/// is what makes lengths a safe stand-in for junk local titles. `local.tracks`
/// must be in the order of [`local_track_order`].
fn mapped_in_local_order(local: &LocalRelease, candidate: &Candidate) -> bool {
    debug_assert!(
        local
            .tracks
            .is_sorted_by(|a, b| local_track_order(a, b).is_le()),
        "local tracks must be in store::order_release_tracks order"
    );
    let mapping = &candidate.mapping;
    if !mapping.complete || mapping.ambiguous {
        return false;
    }
    // A disc is compared with its own medium's tracks, not with the release's.
    let playable = candidate.release.playable_tracks_in(mapping.medium);
    local.tracks.len() == playable.len()
        && mapping.pairs.len() == playable.len()
        && maps_onto_leading(local, mapping, &playable)
}

/// Whether local track `k` is mapped to `playable[k]` for every local track,
/// with no more local tracks than playable remote tracks. The remote may list
/// more tracks after the last local one. Says nothing about completeness or
/// ambiguity, which the caller checks.
fn maps_onto_leading(
    local: &LocalRelease,
    mapping: &Mapping,
    playable: &[(usize, &RemoteTrack)],
) -> bool {
    local.tracks.len() <= playable.len()
        && mapping.pairs.len() >= local.tracks.len()
        && mapping
            .pairs
            .iter()
            .zip(playable)
            .all(|(pair, (remote_index, _))| *pair == Some(*remote_index))
}

/// Whether the lengths alone identify the tracks of an album-level match: at
/// least [`DURATION_IDENTITY_MIN_TRACKS`] local tracks, album title and artist
/// each at least [`TITLE_SIMILARITY_THRESHOLD`], a mapping that is
/// [`mapped_in_local_order`], a local title and a known positive duration on
/// both sides of every pair with each pair within [`DURATION_IDENTITY_GAP_SECS`],
/// and an album score without the track titles of at least
/// [`DURATION_IDENTITY_SCORE`].
///
/// The track titles then add no evidence, so weak ones no longer block. A weak
/// title is junk only when it names nothing else: a local title that scores
/// below [`TITLE_SIMILARITY_THRESHOLD`] against its mapped remote track but at
/// least that against a different playable remote track is a contradiction
/// (the edition orders its tracks differently), and the rule does not apply.
fn durations_identify_tracks(local: &LocalRelease, candidate: &Candidate) -> bool {
    let score = &candidate.score;
    if local.tracks.len() < DURATION_IDENTITY_MIN_TRACKS
        || score.album_title < TITLE_SIMILARITY_THRESHOLD
        || score.album_artist < TITLE_SIMILARITY_THRESHOLD
        || score.album_score_without_track_titles() + 1e-9 < DURATION_IDENTITY_SCORE
        || !mapped_in_local_order(local, candidate)
    {
        return false;
    }
    let cleaned = cleaned_track_titles(local);
    let playable = candidate.release.playable_tracks();
    candidate.mapping.assigned_pairs().all(|(l, r)| {
        let (Some(local_track), Some(remote_track)) =
            (local.tracks.get(l), candidate.release.tracklist.get(r))
        else {
            return false;
        };
        let Some(title) = local_track
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        else {
            return false;
        };
        let names_another_track = candidate
            .mapping
            .title_similarity(local, &cleaned, l, r, remote_track)
            .is_some_and(|similarity| similarity < TITLE_SIMILARITY_THRESHOLD)
            && playable.iter().any(|(other, track)| {
                *other != r
                    && track_title_similarity(
                        title,
                        cleaned.get(l).and_then(Option::as_ref),
                        &track.title,
                    ) >= TITLE_SIMILARITY_THRESHOLD
            });
        !names_another_track
            && matches!((local_track.duration_secs, remote_track.duration_secs),
                (Some(a), Some(b)) if a > 0.0 && b > 0.0)
            && duration_gap(local, candidate, l, r)
                .is_some_and(|gap| gap <= DURATION_IDENTITY_GAP_SECS)
    })
}

/// Whether a candidate is marked unofficial: a Discogs format that says
/// `Unofficial Release`, or a MusicBrainz release with status `Bootleg`. Such a
/// release is not the edition a library's files come from often enough to take
/// on its text alone.
fn is_unofficial(remote: &RemoteRelease) -> bool {
    remote
        .formats
        .iter()
        .any(|format| format.to_lowercase().contains("unofficial release"))
        || remote
            .provider_metadata
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("bootleg"))
}

/// Whether the text of a candidate agrees with the files in track order: album
/// title and artist each at least [`TITLE_SIMILARITY_THRESHOLD`], a mapping that
/// is [`mapped_in_local_order`], and a local title on every track that scores at
/// least that against its mapped track. Says nothing about lengths. `cleaned` is
/// [`cleaned_track_titles`] of `local`.
fn text_agrees_in_order(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> bool {
    album_text_agrees(candidate)
        && mapped_in_local_order(local, candidate)
        && track_titles_agree(local, candidate, cleaned)
}

/// Whether the album title and the album artist of a candidate each score at
/// least [`TITLE_SIMILARITY_THRESHOLD`] against the files.
fn album_text_agrees(candidate: &Candidate) -> bool {
    let score = &candidate.score;
    score.album_title >= TITLE_SIMILARITY_THRESHOLD
        && score.album_artist >= TITLE_SIMILARITY_THRESHOLD
}

/// Whether every assigned track has a local title that scores at least
/// [`TITLE_SIMILARITY_THRESHOLD`] against its mapped track, wherever the tracks
/// sit in the remote list. `cleaned` is [`cleaned_track_titles`] of `local`.
/// Says nothing about the order of the mapping, the album text or lengths.
fn track_titles_agree(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> bool {
    candidate.mapping.assigned_pairs().all(|(l, r)| {
        let (Some(local_track), Some(remote_track)) =
            (local.tracks.get(l), candidate.release.tracklist.get(r))
        else {
            return false;
        };
        local_track
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty())
            && candidate
                .mapping
                .title_similarity(local, cleaned, l, r, remote_track)
                .is_some_and(|similarity| similarity >= TITLE_SIMILARITY_THRESHOLD)
    })
}

/// Whether the album titles of a text-only match rule out a different release
/// that is named alike. The local title must not call itself a bootleg (the
/// reduction that makes `Album (Bootleg)` match `Album` would otherwise let an
/// official release stand for it), and the numbers in both titles must agree
/// ([`album_numbers_agree`]): `Live in Berlin 1994` is not `Live in Berlin 1995`,
/// and with no lengths to tell them apart nothing else would say so. A
/// placeholder title (`Self Titled`) names no number and is not compared.
fn album_titles_identify(local: &LocalRelease, candidate: &Candidate) -> bool {
    let Some(title) = local.album_title.as_deref() else {
        return false;
    };
    if decoration_names_bootleg(title) {
        return false;
    }
    if is_self_titled(title) {
        return true;
    }
    // A disc marker on the title of the matched disc is not part of the name.
    let disc_album = candidate.mapping.medium.and_then(|medium| {
        title_disc_suffix(title)
            .filter(|(_, number)| *number == medium)
            .map(|(album, _)| album)
    });
    album_numbers_agree(
        disc_album.as_deref().unwrap_or(title),
        &candidate.release.title,
    )
}

/// Whether the decoration run at the end of an album title names a bootleg.
fn decoration_names_bootleg(title: &str) -> bool {
    decoration_run(title).is_some_and(|run| {
        run.units.first().is_some_and(|first| {
            decoration_tokens(&title[first.start..])
                .iter()
                .any(|(_, token)| alphanumeric_lowercase(token) == "bootleg")
        })
    })
}

/// Whether the titles alone identify the tracks of an album-level match whose
/// provider lists no length for some of them: at least
/// [`TEXT_IDENTITY_MIN_TRACKS`] local tracks ([`TEXT_IDENTITY_MIN_VARIOUS_TRACKS`]
/// for Various Artists), a candidate not marked unofficial ([`is_unofficial`]),
/// text that [`text_agrees_in_order`], album titles that [`album_titles_identify`],
/// no local file that is another take than
/// its remote track ([`local_title_is_another_take`]), no local title that scores strictly
/// higher against another playable remote track than against its own, a known
/// positive local length on every track, at least one length the provider does
/// not list, every length it does list within [`DURATION_TOLERANCE_SECS`], and
/// an album score without the durations of at least [`TEXT_IDENTITY_SCORE`].
///
/// A provider that lists no length (most CD, vinyl and tape entries of small
/// labels) cannot confirm one, so the text carries the match and the lengths
/// stay unconfirmed. Unlike the weak-title rules, no near miss and no drift is
/// allowed: with nothing to settle a doubtful title or a long track, any doubt
/// blocks. The better-track check is what stops near-identical titles
/// (`Part 1`, `Part 2`) from passing when the edition orders them differently.
fn titles_identify_tracks(local: &LocalRelease, candidate: &Candidate) -> bool {
    let min_tracks = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        TEXT_IDENTITY_MIN_VARIOUS_TRACKS
    } else {
        TEXT_IDENTITY_MIN_TRACKS
    };
    if local.tracks.len() < min_tracks
        || is_unofficial(&candidate.release)
        || candidate.score.album_score_without_durations() + 1e-9 < TEXT_IDENTITY_SCORE
        || !album_titles_identify(local, candidate)
    {
        return false;
    }
    // Cheap per-pair conditions first: a candidate that lists every length, the
    // usual case, stops here without the all-pairs comparison below.
    let cleaned = cleaned_track_titles(local);
    let mut unlisted = 0usize;
    let lengths_agree = candidate.mapping.assigned_pairs().all(|(l, r)| {
        let (Some(local_track), Some(remote_track)) =
            (local.tracks.get(l), candidate.release.tracklist.get(r))
        else {
            return false;
        };
        let local_length_known = local_track
            .duration_secs
            .is_some_and(|secs| secs.is_finite() && secs > 0.0);
        let remote_length_agrees = match remote_track.duration_secs {
            None => {
                unlisted += 1;
                true
            }
            Some(secs) if secs.is_finite() && secs > 0.0 => duration_gap(local, candidate, l, r)
                .is_some_and(|gap| gap <= DURATION_TOLERANCE_SECS),
            Some(_) => false,
        };
        local_length_known
            && remote_length_agrees
            && !local_title_is_another_take(
                local_track.title.as_deref().unwrap_or_default(),
                cleaned.get(l).and_then(Option::as_ref),
                &remote_track.title,
            )
    });
    if !lengths_agree || unlisted == 0 || !text_agrees_in_order(local, candidate, &cleaned) {
        return false;
    }
    titles_prefer_own_tracks(local, candidate, &cleaned)
}

/// Whether no local title scores strictly higher against another playable
/// remote track than against the track it is mapped to. It is what stops
/// near-identical titles (`Part 1`, `Part 2`) from passing when the edition
/// orders them differently. A mapped track without a local title fails.
/// `cleaned` is [`cleaned_track_titles`] of `local`.
fn titles_prefer_own_tracks(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> bool {
    let playable = candidate.release.playable_tracks();
    candidate.mapping.assigned_pairs().all(|(l, r)| {
        let (Some(local_track), Some(remote_track)) =
            (local.tracks.get(l), candidate.release.tracklist.get(r))
        else {
            return false;
        };
        let (Some(title), Some(own)) = (
            local_track.title.as_deref(),
            candidate
                .mapping
                .title_similarity(local, cleaned, l, r, remote_track),
        ) else {
            return false;
        };
        !playable.iter().any(|(other, track)| {
            *other != r
                && track_title_similarity(
                    title,
                    cleaned.get(l).and_then(Option::as_ref),
                    &track.title,
                ) > own + 1e-9
        })
    })
}

/// Whether a local title is another take than the plain remote title: it ends
/// in a bracketed group that the remote title lacks, and the group names an
/// alternate take, outtake, rehearsal or session, or `take` followed by a
/// number (`Take 2`, `Take Two`), in the raw title or in its cleaned reading
/// (without extension, release tag and number prefix). Title similarity drops a one-sided trailing
/// group at a small cost, which is right for a subtitle but not for the file
/// being a different recording of the song, and a text-only match has no length
/// to tell the two apart.
fn local_title_is_another_take(
    local_title: &str,
    cleaned: Option<&CleanedTitle>,
    remote_title: &str,
) -> bool {
    // An audio extension or a release tag after the bracket hides the group
    // from the raw reading while the cleaned reading, which the similarity
    // takes the better of, still drops it, so both readings are checked.
    names_another_take(local_title, remote_title)
        || cleaned.is_some_and(|cleaned| names_another_take(&cleaned.text, remote_title))
}

fn names_another_take(local_title: &str, remote_title: &str) -> bool {
    const NUMBER_WORDS: &[&str] = &[
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    let local_title = local_title.trim_end();
    let Some(start) = bracketed_suffix_start(local_title) else {
        return false;
    };
    if bracketed_suffix_start(remote_title.trim_end()).is_some() {
        return false;
    }
    let suffix = normalize_text(&local_title[start..]);
    let words: Vec<&str> = suffix.split_whitespace().collect();
    words.iter().any(|word| {
        matches!(
            *word,
            "alternate"
                | "outtake"
                | "outtakes"
                | "rehearsal"
                | "rehearsals"
                | "session"
                | "sessions"
        )
    }) || words.windows(2).any(|pair| {
        (pair[0] == "take"
            && (pair[1].chars().all(|ch| ch.is_ascii_digit()) || NUMBER_WORDS.contains(&pair[1])))
            || (pair[0] == "alt" && pair[1] == "take")
    })
}

/// Whether `other` contradicts the lengths of the files while its text agrees:
/// [`text_agrees_in_order`] and a known mapped length more than
/// `limit_secs` from the local file's. `cleaned` is [`cleaned_track_titles`] of
/// `local`.
fn text_agrees_but_lengths_do_not(
    local: &LocalRelease,
    other: &Candidate,
    cleaned: &[Option<CleanedTitle>],
    limit_secs: f64,
) -> bool {
    text_agrees_in_order(local, other, cleaned)
        && other
            .mapping
            .assigned_pairs()
            .any(|(l, r)| duration_gap(local, other, l, r).is_some_and(|gap| gap > limit_secs))
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
        || !qualifiers_agree(
            &remote_edition_qualifiers(&a.release),
            &remote_edition_qualifiers(&b.release),
        )
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
///
/// A local release with no usable album title (missing or filler, see
/// [`tag_identity`]) has no title to weigh, so the title floor and the title
/// condition do not apply to it; everything else does. Its artist has to be a
/// real tag: a junk or missing one scores zero and keeps the release in review.
fn corroborated_album(local: &LocalRelease, candidate: &Candidate) -> bool {
    let score = &candidate.score;
    let min_tracks = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        CORROBORATED_MIN_VARIOUS_TRACKS
    } else {
        CORROBORATED_MIN_TRACKS
    };
    let title_weighs_in = !lacks_album_title(local);
    if local.tracks.len() < min_tracks
        || (title_weighs_in
            && (score.album_title < CORROBORATED_TITLE_FLOOR
                || score.album_title >= TITLE_SIMILARITY_THRESHOLD))
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

/// Whether the local release has no usable album title: it is missing or
/// filler, and not a self-titled placeholder, which reads as the artist's name.
fn lacks_album_title(local: &LocalRelease) -> bool {
    tag_identity(local).title.is_none()
}

fn evaluate_at_scope(
    local: &LocalRelease,
    candidate: &Candidate,
    runner_up: Option<&Candidate>,
    discovery: &DiscoveryEvidence,
    match_scope: MatchScope,
) -> Evaluation {
    evaluate_with(
        local,
        candidate,
        runner_up,
        discovery,
        match_scope,
        RuleGuards::default(),
    )
}

/// [`evaluate_at_scope`], where `guards` turn off the rules that another
/// candidate makes unsafe.
fn evaluate_with(
    local: &LocalRelease,
    candidate: &Candidate,
    runner_up: Option<&Candidate>,
    discovery: &DiscoveryEvidence,
    match_scope: MatchScope,
    guards: RuleGuards,
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

    let cleaned = cleaned_track_titles(local);

    // Lengths that identify every track in track order stand in for the track
    // titles, so the album score gate does not apply. Album scope only, and not
    // together with the weak album title rule, which needs a title below the
    // minimum that this rule requires.
    let duration_identity = match_scope == MatchScope::Album
        && !corroborated_title
        && durations_identify_tracks(local, candidate);

    // A provider that lists no length for some tracks cannot confirm them, so
    // titles that agree in track order stand in for the lengths and the album
    // score gate, which would count the missing lengths as zero, does not
    // apply. Album scope only. It needs at least one unlisted length, which
    // the duration-identity rule rules out, so the two never overlap.
    let text_identity = match_scope == MatchScope::Album
        && guards.text_identity
        && !corroborated_title
        && titles_identify_tracks(local, candidate);

    let acceptance_score = match_scope.score(&candidate.score);
    if !corroborated_title
        && !duration_identity
        && !text_identity
        && acceptance_score + 1e-9 < minimum_score
    {
        blockers.push(format!(
            "score {:.3} is below the {:.2} acceptance threshold",
            acceptance_score, minimum_score
        ));
    }

    if corroborated_title && lacks_album_title(local) {
        warnings.push(
            "local release has no usable album title, but every track and duration agrees"
                .to_string(),
        );
    } else if corroborated_title {
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

    if let Some(medium) = candidate.mapping.medium {
        let source = local_disc_hint(local)
            .filter(|hint| hint.number == medium)
            .map(|hint| format!(" (disc number from the {})", hint.source.describe()))
            .unwrap_or_default();
        evidence.push(format!(
            "the files are {} of this release{source}",
            disc_label(remote, medium)
        ));
    }

    // Per-track evidence.
    let mut worst_title = 1.0f64;
    let mut worst_title_track = String::new();
    let mut worst_title_duration_gap: Option<f64> = None;
    let mut titles_below_minimum = 0usize;
    let mut worst_duration: Option<f64> = None;
    let mut worst_duration_track = String::new();
    let mut tracks_over_tolerance = 0usize;
    let (mut local_total, mut remote_total) = (0.0f64, 0.0f64);
    // Tracks missing a local title or length always block an album match;
    // tracks the provider lists no length for block unless the titles identify
    // the tracks.
    let mut missing_local = 0usize;
    let mut missing_remote = 0usize;

    for (local_index, remote_index) in candidate.mapping.assigned_pairs() {
        let Some(local_track) = local.tracks.get(local_index) else {
            continue;
        };
        let Some(remote_track) = remote.tracklist.get(remote_index) else {
            continue;
        };

        if match_scope == MatchScope::Album {
            let known =
                |duration: Option<f64>| duration.is_some_and(|secs| secs.is_finite() && secs > 0.0);
            if local_track
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty())
                || !known(local_track.duration_secs)
            {
                missing_local += 1;
            }
            if !known(remote_track.duration_secs) {
                missing_remote += 1;
            }
            if !qualifiers_agree(
                &album_track_qualifiers(local_track.title.as_deref().unwrap_or_default()),
                &album_track_qualifiers(&remote_track.title),
            ) {
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

    if missing_local > 0 || (missing_remote > 0 && !text_identity) {
        blockers
            .push("album matching requires a title and known duration for every track".to_string());
    }
    if text_identity {
        warnings.push(format!(
            "{provider} lists no duration for {missing_remote} of {} tracks; {UNCONFIRMED_LENGTHS_NOTE}",
            local.tracks.len()
        ));
    }

    // One title off by a little, with the right length, is a spelling variant of
    // the same recording rather than a different track. A title off by a lot,
    // on a track whose length agrees closely, in an album of at least five where
    // every other track already agrees, is one mis-tagged or differently named
    // track of the same album: by elimination it is the remaining track. Only for
    // an album-level match. These exceptions do not stack with the weak album
    // title rule: when that rule is in play, which replaces the acceptance score
    // gate, any track title below the minimum blocks. The duration-identity rule
    // accepts any number of weak titles instead, as one warning.
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
        } else if duration_identity {
            // One weak title always takes the lone-title branch above, since the
            // rule's own conditions imply it, so this is always plural.
            warnings.push(format!(
                "{titles_below_minimum} track titles score below {minimum_title_similarity:.2} (lowest '{worst_title_track}' {worst_title:.3}) but every duration agrees within {DURATION_IDENTITY_GAP_SECS}s in track order"
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
    // A missing or junk title says nothing about the edition.
    if tag_identity(local).title.is_some()
        && let Some(local_title) = &local.album_title
    {
        let local_qualifiers = edition_qualifiers(local_title);
        let remote_qualifiers = remote_edition_qualifiers(remote);
        if !qualifiers_agree(&local_qualifiers, &remote_qualifiers) {
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
/// plausible primary candidate. A primary is plausible when its plausibility
/// score reaches [`PLAUSIBLE_PRIMARY_SCORE`] or when its durations identify the
/// tracks ([`durations_identify_tracks`]), so junk local titles do not hide the
/// right primary. Against a primary that is plausible only through its
/// durations, edition qualifiers and durations are compared but titles are not.
///
/// A fallback accepted on its text alone (`text_only_fallback`, because its
/// provider lists no length for some tracks) is also checked against every
/// primary whose text agrees with the files: that primary's lengths must not
/// contradict them.
pub fn fallback_conflicts(
    local: &LocalRelease,
    primary_candidates: &[Candidate],
    fallback: &Candidate,
    embedded_release_resolved: bool,
    text_only_fallback: bool,
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

    // A fallback accepted on its text alone (`text_only_fallback`, see
    // [`Evaluation::relies_on_unlisted_lengths`]) cannot confirm the lengths it
    // lists none for, so a primary that agrees in text must not contradict the
    // files by its own lengths: a live or re-recorded album carries the studio
    // album's name. This holds for a primary that is not plausible by score as
    // well, which is exactly what such lengths cause. A fallback accepted at
    // edition scope, or on lengths it lists, is judged as before.
    let fallback_lists_every_length = fallback_durations.len() == fallback.mapping.assigned_count();
    if text_only_fallback && !fallback_lists_every_length && {
        let cleaned = cleaned_track_titles(local);
        primary_candidates.iter().any(|primary| {
            text_agrees_but_lengths_do_not(local, primary, &cleaned, DURATION_TOLERANCE_SECS)
        })
    } {
        conflicts.push(
            "primary lengths contradict the files while the fallback lists no length for some tracks"
                .to_string(),
        );
    }

    for primary in primary_candidates {
        if !primary.mapping.complete {
            continue;
        }
        // A primary that is plausible only because its durations identify the
        // tracks has titles that carry no evidence, so they are not compared.
        let plausible_by_score =
            plausibility_score(local, primary) + 1e-9 >= PLAUSIBLE_PRIMARY_SCORE;
        if !plausible_by_score && !durations_identify_tracks(local, primary) {
            continue;
        }
        let primary_qualifiers = remote_edition_qualifiers(&primary.release);
        if !qualifiers_agree(&primary_qualifiers, &fallback_qualifiers) {
            conflicts.push(format!(
                "primary candidates describe {:?} but the fallback describes {:?}",
                primary_qualifiers, fallback_qualifiers
            ));
        }
        for (local_index, remote_index) in primary.mapping.assigned_pairs() {
            let Some(primary_track) = primary.release.tracklist.get(remote_index) else {
                continue;
            };
            if plausible_by_score && let Some(fallback_title) = fallback_titles.get(&local_index) {
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
    fn stereo_is_ignored_unless_the_same_set_says_mono() {
        let set = |words: &[&'static str]| words.iter().copied().collect::<BTreeSet<_>>();
        for (a, b) in [
            (set(&[]), set(&["stereo"])),
            (set(&["live"]), set(&["live", "stereo"])),
            (set(&["stereo"]), set(&["stereo"])),
            (set(&["mono", "stereo"]), set(&["mono", "stereo"])),
        ] {
            assert!(qualifiers_agree(&a, &b), "{a:?} versus {b:?}");
            assert!(qualifiers_agree(&b, &a), "{b:?} versus {a:?}");
        }
        for (a, b) in [
            (set(&["mono"]), set(&["stereo"])),
            (set(&[]), set(&["mono"])),
            (set(&[]), set(&["remastered", "stereo"])),
            (set(&["live"]), set(&["stereo"])),
            (set(&["mono"]), set(&["mono", "stereo"])),
            (set(&[]), set(&["mono", "stereo"])),
            (set(&["stereo"]), set(&["mono", "stereo"])),
        ] {
            assert!(!qualifiers_agree(&a, &b), "{a:?} versus {b:?}");
            assert!(!qualifiers_agree(&b, &a), "{b:?} versus {a:?}");
        }
    }

    #[test]
    fn a_stereo_format_does_not_block_a_silent_local_title() {
        let local = local_release(&["One", "Two"]);
        let mut remote = remote_release(&["One", "Two"]);
        remote.formats = vec!["Vinyl (LP, Album, Stereo)".to_string()];
        assert!(remote_edition_qualifiers(&remote).contains("stereo"));
        let candidate = Candidate::new(&local, remote);
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("edition qualifiers")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_live_title_still_agrees_with_live_and_stereo() {
        let mut local = local_release(&["One", "Two"]);
        local.album_title = Some("Test Album (Live)".to_string());
        let mut remote = remote_release(&["One", "Two"]);
        remote.title = "Test Album (Live)".to_string();
        remote.formats = vec!["Vinyl (LP, Album, Stereo)".to_string()];
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("edition qualifiers")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn mono_still_blocks_against_stereo_or_silence() {
        for (local_title, formats, problem) in [
            (
                "Test Album (Mono)",
                &["Vinyl (LP, Album, Stereo)"][..],
                "local mono",
            ),
            ("Test Album", &["Vinyl (LP, Album, Mono)"], "remote mono"),
            (
                "Test Album (Stereo)",
                &["Vinyl (LP, Album, Mono)"],
                "local stereo, remote mono",
            ),
            (
                "Test Album (Mono)",
                &["Vinyl (LP, Album, Mono)", "Vinyl (LP, Album, Stereo)"],
                "local mono, remote mono and stereo",
            ),
        ] {
            let mut local = local_release(&["One", "Two"]);
            local.album_title = Some(local_title.to_string());
            let mut remote = remote_release(&["One", "Two"]);
            remote.formats = formats.iter().map(|format| (*format).to_string()).collect();
            let candidate = Candidate::new(&local, remote);
            let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
            assert!(!evaluation.auto_accept, "accepted {problem}");
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|reason| reason.contains("edition qualifiers differ")),
                "{problem}: {:?}",
                evaluation.blockers
            );
        }
    }

    #[test]
    fn a_remastered_stereo_format_still_blocks_a_silent_title() {
        let local = local_release(&["One", "Two"]);
        let mut remote = remote_release(&["One", "Two"]);
        remote.formats = vec!["Vinyl (LP, Album, Remastered, Stereo)".to_string()];
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|reason| reason.contains("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn track_stereo_qualifier_is_ignored_but_mono_is_not() {
        let blocked = |remote_title: &str| {
            let local = local_release(&["Song", "Other"]);
            let remote = remote_release(&[remote_title, "Other"]);
            let candidate = Candidate::new(&local, remote);
            evaluate_at_scope(
                &local,
                &candidate,
                None,
                &DiscoveryEvidence::default(),
                MatchScope::Album,
            )
            .blockers
            .iter()
            .any(|reason| reason.contains("track version qualifiers"))
        };
        assert!(!blocked("Song (Stereo)"));
        assert!(blocked("Song (Mono)"));
    }

    #[test]
    fn an_alternative_that_differs_only_by_stereo_is_equivalent_audio() {
        let local = local_release(&["One", "Two"]);
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut stereo = remote_release(&["One", "Two"]);
        stereo.id = ReleaseId::Discogs(43);
        stereo.formats = vec!["Vinyl (LP, Album, Stereo)".to_string()];
        let stereo = Candidate::new(&local, stereo);
        assert!(equivalent_audio(&first, &stereo));

        let mut mono = remote_release(&["One", "Two"]);
        mono.id = ReleaseId::Discogs(44);
        mono.formats = vec!["Vinyl (LP, Album, Mono)".to_string()];
        let mono = Candidate::new(&local, mono);
        assert!(!equivalent_audio(&first, &mono));
        assert!(!equivalent_audio(&stereo, &mono));

        let mut both = remote_release(&["One", "Two"]);
        both.id = ReleaseId::Discogs(45);
        both.formats = vec![
            "Vinyl (LP, Album, Mono)".to_string(),
            "CD (Album, Stereo)".to_string(),
        ];
        let both = Candidate::new(&local, both);
        assert!(!equivalent_audio(&mono, &both));
    }

    #[test]
    fn fallback_conflicts_ignore_stereo_but_not_mono() {
        let local = local_release(&["One", "Two"]);
        let with_format = |format: &str| {
            let mut release = remote_release(&["One", "Two"]);
            release.formats = vec![format.to_string()];
            Candidate::new(&local, release)
        };
        let silent = with_format("CD");
        let stereo = with_format("Vinyl (LP, Album, Stereo)");
        let mono = with_format("Vinyl (LP, Album, Mono)");
        assert!(silent.score.total >= PLAUSIBLE_PRIMARY_SCORE);

        let conflicts = fallback_conflicts(&local, &[silent], &stereo, true, false);
        assert!(conflicts.is_empty(), "{conflicts:?}");

        let conflicts = fallback_conflicts(&local, &[mono], &stereo, true, false);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{conflicts:?}"
        );
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
        let conflicts = fallback_conflicts(&local, &[primary], &fallback, false, false);
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
        let conflicts = fallback_conflicts(&local, &[primary], &fallback, true, false);
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
            "discogs-match-v27"
        );
        assert_eq!(
            algorithm_version(ProviderKind::MusicBrainz),
            "musicbrainz-match-v26"
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

    const NO_TITLE_WARNING: &str =
        "local release has no usable album title, but every track and duration agrees";

    /// The six-track release with its album title replaced, and the evaluation
    /// of the matching candidate.
    fn evaluate_untitled(
        title: Option<&str>,
        change: impl Fn(&mut LocalRelease, &mut RemoteRelease),
    ) -> Evaluation {
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        let mut local = local_release(&titles);
        local.album_title = title.map(str::to_string);
        let mut remote = remote_release(&titles);
        change(&mut local, &mut remote);
        let candidate = Candidate::new(&local, remote);
        evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default())
    }

    #[test]
    fn a_missing_album_title_is_carried_by_the_tracks_and_durations() {
        for title in [
            None,
            Some(""),
            Some("   "),
            Some("Unknown Title"),
            Some("Unknown Album"),
            Some("."),
        ] {
            let evaluation = evaluate_untitled(title, |_, _| {});
            assert!(
                evaluation.auto_accept,
                "{title:?}: {:?}",
                evaluation.blockers
            );
            assert!(
                evaluation
                    .warnings
                    .iter()
                    .any(|note| note == NO_TITLE_WARNING),
                "{title:?}: {:?}",
                evaluation.warnings
            );
            // The title is no evidence either way, so it is not reported as a
            // weak title.
            assert!(
                evaluation
                    .warnings
                    .iter()
                    .all(|note| !note.contains("album title scores")),
                "{:?}",
                evaluation.warnings
            );
        }
        // A real title that is simply wrong still blocks.
        let evaluation = evaluate_untitled(Some("Completely Different Thing"), |_, _| {});
        assert!(!evaluation.auto_accept);
        // A real but different title is evidence against the album, so the
        // title floor still blocks it, with everything else agreeing. The same
        // release with no title is accepted.
        let retitle = |_: &mut LocalRelease, remote: &mut RemoteRelease| {
            remote.title = "Self Titled".to_string();
        };
        let evaluation = evaluate_untitled(Some("Something Else"), retitle);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("album title or artist does not agree")),
            "{:?}",
            evaluation.blockers
        );
        assert!(evaluate_untitled(None, retitle).auto_accept);
    }

    #[test]
    fn a_missing_album_title_still_needs_everything_else_to_agree() {
        let blocked = |change: &dyn Fn(&mut LocalRelease, &mut RemoteRelease)| {
            let evaluation = evaluate_untitled(None, change);
            assert!(!evaluation.auto_accept, "{:?}", evaluation.warnings);
            evaluation
        };
        // Too few tracks.
        blocked(&|local, remote| {
            local.tracks.truncate(2);
            remote.tracklist.truncate(2);
        });
        // A length two seconds off: inside the three-second tolerance, outside
        // the one and a half seconds this rule needs.
        blocked(&|local, _| {
            local.tracks[2].duration_secs = Some(local.tracks[2].duration_secs.unwrap() + 2.0);
        });
        // A different artist.
        blocked(&|_, remote| remote.artists = vec!["Another Band".to_string()]);
        // A junk or missing artist tag is no artist evidence.
        for artist in [None, Some("Unknown Artist")] {
            blocked(&|local, _| local.album_artist = artist.map(str::to_string));
        }
        // One track title that is only close.
        let evaluation = blocked(&|local, _| local.tracks[3].title = Some("Fou".to_string()));
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note == NO_TITLE_WARNING)
        );
    }

    #[test]
    fn a_missing_album_title_does_not_stack_with_a_weak_track_title() {
        let mut local = local_release(&TEN_TITLES);
        local.album_title = None;
        let remote = remote_release(&TEN_TITLES);
        // 0.80 against "Seven": the mean stays above 0.97, and the lone-title
        // exceptions do not apply to a release the title does not identify.
        local.tracks[6].title = Some("Seve".to_string());
        let candidate = Candidate::new(&local, remote);
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
    fn a_missing_album_title_needs_five_tracks_for_various_artists() {
        let accepted = |titles: &[&str]| {
            let mut local = local_release(titles);
            local.album_title = None;
            local.artist_kind = ArtistKind::VariousArtists;
            let mut remote = remote_release(titles);
            remote.artists = vec!["Various Artists".to_string()];
            let candidate = Candidate::new(&local, remote);
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        };
        assert!(!accepted(&["One", "Two", "Three", "Four"]));
        assert!(accepted(&["One", "Two", "Three", "Four", "Five"]));
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

        // Two weak titles still block, unless every length agrees closely, which
        // is a different rule: here one other length is two seconds off.
        let mut two = local.clone();
        two.tracks[4].title = Some("Fiv".to_string());
        two.tracks[0].duration_secs = Some(two.tracks[0].duration_secs.unwrap() + 2.0);
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

        // Two different titles still block, unless every length agrees closely,
        // which is a different rule: here one other length is two seconds off.
        let mut two = local.clone();
        two.tracks[4].title = Some("Another one entirely".to_string());
        two.tracks[0].duration_secs = Some(two.tracks[0].duration_secs.unwrap() + 2.0);
        let candidate = Candidate::new(&two, remote.clone());
        let evaluation =
            evaluate_for_matching(&two, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("only scores")),
            "{:?}",
            evaluation.blockers
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

    /// A local release and a remote release over the same tracks whose lengths
    /// are 31 seconds apart, so that no two tracks can be mistaken for each
    /// other by length. `remote_titles` are what the provider calls the tracks
    /// and `local_titles` what the files say.
    fn spaced_pair(local_titles: &[&str], remote_titles: &[&str]) -> (LocalRelease, RemoteRelease) {
        let mut local = local_release(local_titles);
        let mut remote = remote_release(remote_titles);
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.duration_secs = Some(150.0 + 31.0 * index as f64);
        }
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.duration_secs = Some(150.0 + 31.0 * index as f64);
        }
        (local, remote)
    }

    fn album_evaluation(
        local: &LocalRelease,
        remote: &RemoteRelease,
        alternatives: &[Candidate],
    ) -> Evaluation {
        let candidate = Candidate::new(local, remote.clone());
        evaluate_for_matching(
            local,
            &candidate,
            alternatives,
            &DiscoveryEvidence::default(),
        )
    }

    /// Asserts the album is accepted on the lengths alone, with one warning
    /// naming `weak` titles.
    fn assert_accepted_on_durations(local: &LocalRelease, remote: &RemoteRelease, weak: usize) {
        let candidate = Candidate::new(local, remote.clone());
        assert!(
            durations_identify_tracks(local, &candidate),
            "{:?}",
            candidate.score
        );
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        let expected = format!("{weak} track titles score below 0.90 (lowest ");
        let warnings: Vec<_> = evaluation
            .warnings
            .iter()
            .filter(|note| note.contains("every duration agrees within 1.5s in track order"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", evaluation.warnings);
        assert!(warnings[0].starts_with(&expected), "{}", warnings[0]);
    }

    #[test]
    fn placeholder_track_titles_pass_on_exact_lengths() {
        let remote_titles = [
            "Opening",
            "Second Wind",
            "Hollow",
            "Undertow",
            "Lantern",
            "Closing",
        ];
        let local_titles = [
            "Track01", "Track02", "Track03", "Track04", "Track05", "Track06",
        ];
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_accepted_on_durations(&local, &remote, 6);
    }

    #[test]
    fn misspelled_titles_pass_on_exact_lengths() {
        // Meathook Seed, Embedded.
        let remote_titles = [
            "Embedded",
            "A Fumed Grave",
            "A Wilted Remnant",
            "Focal Point Blur",
            "Dead Reckoning",
            "Fracture Lines",
        ];
        let local_titles = [
            "Embedded",
            "A Fumed Game",
            "A Melted Remnant",
            "Jocal Point Bliss",
            "Dead Reckoning",
            "Fracture Lines",
        ];
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_accepted_on_durations(&local, &remote, 3);

        // Beheaded.
        let remote_titles = [
            "Transcendental Iniquity",
            "Subconscious Deliverance",
            "Cadaveric Offering",
            "Rotting Throne",
            "Funeral Mist",
        ];
        let local_titles = [
            "transdental inquiry",
            "subconscious delivery",
            "Cadaveric Offering",
            "Rotting Throne",
            "Funeral Mist",
        ];
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_accepted_on_durations(&local, &remote, 2);
    }

    #[test]
    fn two_shortened_titles_in_a_long_album_pass_on_exact_lengths() {
        // Alice Cooper, Dragontown.
        let remote_titles = [
            "Triumph of the Will",
            "Sex, Death and Money",
            "Somewhere in the Jungle",
            "Sentinel Rising",
            "I Just Wanna Be God",
            "Deeper",
            "The Sentinel",
            "Fantasy Man",
            "It's Me",
            "Disgraceland",
            "Every Woman Has a Name",
            "Dragontown",
        ];
        let mut local_titles = remote_titles;
        local_titles[2] = "Somewhere";
        local_titles[6] = "I Am the Sentinel";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.track_titles < 0.95);
        assert_accepted_on_durations(&local, &remote, 2);
    }

    #[test]
    fn remote_titles_with_an_artist_prefix_pass_on_exact_lengths() {
        let local_titles = [
            "Declare Guerre Nucleaire",
            "Hate to Say I Told You So",
            "Main Offender",
            "Veni Vidi Vicious",
            "Die, All Right!",
            "Supply and Demand",
        ];
        let prefixed: Vec<String> = local_titles
            .iter()
            .map(|title| format!("The Hives - {title}"))
            .collect();
        let remote_titles: Vec<&str> = prefixed.iter().map(String::as_str).collect();
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_accepted_on_durations(&local, &remote, 6);
    }

    /// Six placeholder files over a six-track release with spaced lengths.
    fn placeholder_pair() -> (LocalRelease, RemoteRelease) {
        spaced_pair(
            &[
                "Track01", "Track02", "Track03", "Track04", "Track05", "Track06",
            ],
            &[
                "Opening",
                "Second Wind",
                "Hollow",
                "Undertow",
                "Lantern",
                "Closing",
            ],
        )
    }

    fn assert_blocked_without_duration_identity(local: &LocalRelease, remote: &RemoteRelease) {
        let candidate = Candidate::new(local, remote.clone());
        assert!(!durations_identify_tracks(local, &candidate));
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("every duration agrees")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn one_length_two_seconds_off_blocks_duration_identity() {
        let (mut local, remote) = placeholder_pair();
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 2.0);
        assert_blocked_without_duration_identity(&local, &remote);
    }

    #[test]
    fn tracks_mapped_out_of_local_order_block_duration_identity() {
        // Adjacent remote lengths are one second apart, so every gap stays within
        // 1.5 s after a swap and only the order check can tell.
        let titles = [
            "Opening",
            "Second Wind",
            "Hollow",
            "Undertow",
            "Lantern",
            "Closing",
        ];
        let local = local_release(&[
            "Track01", "Track02", "Track03", "Track04", "Track05", "Track06",
        ]);
        let remote = remote_release(&titles);
        let mut candidate = Candidate::new(&local, remote.clone());
        assert!(mapped_in_local_order(&local, &candidate));
        assert!(durations_identify_tracks(&local, &candidate));

        candidate.mapping.pairs.swap(1, 2);
        let gaps_agree = candidate
            .mapping
            .assigned_pairs()
            .all(|(l, r)| duration_gap(&local, &candidate, l, r).is_some_and(|gap| gap <= 1.5));
        assert!(gaps_agree);
        assert!(candidate.mapping.complete && !candidate.mapping.ambiguous);
        assert!(!mapped_in_local_order(&local, &candidate));
        assert!(!durations_identify_tracks(&local, &candidate));

        let evaluation = evaluate_at_scope(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence::default(),
            MatchScope::Album,
        );
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("only scores")),
            "{:?}",
            evaluation.blockers
        );
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("acceptance threshold")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_mapping_in_local_order_needs_matching_counts_and_no_ambiguity() {
        let (local, remote) = placeholder_pair();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(mapped_in_local_order(&local, &candidate));

        let mut ambiguous = candidate.clone();
        ambiguous.mapping.ambiguous = true;
        assert!(!mapped_in_local_order(&local, &ambiguous));

        let mut incomplete = candidate.clone();
        incomplete.mapping.complete = false;
        assert!(!mapped_in_local_order(&local, &incomplete));

        let mut longer = remote;
        let extra = longer.tracklist[0].clone();
        longer.tracklist.push(extra);
        let longer = Candidate::new(&local, longer);
        assert!(!mapped_in_local_order(&local, &longer));
    }

    #[test]
    fn four_tracks_block_duration_identity() {
        let (local, remote) = spaced_pair(
            &["Track01", "Track02", "Track03", "Track04"],
            &["Opening", "Second Wind", "Hollow", "Undertow"],
        );
        assert_blocked_without_duration_identity(&local, &remote);
    }

    #[test]
    fn a_weak_album_title_or_artist_blocks_duration_identity() {
        let (mut local, mut remote) = placeholder_pair();
        local.album_title = Some("Exclusive Metal Sampler".to_string());
        remote.title = "Exclusive Metal Radio Sampler".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!((0.70..0.90).contains(&candidate.score.album_title));
        assert_blocked_without_duration_identity(&local, &remote);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("album title or artist does not agree")),
            "{:?}",
            evaluation.blockers
        );

        let (mut local, remote) = placeholder_pair();
        local.album_artist = Some("Another Band".to_string());
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_artist < 0.90);
        assert_blocked_without_duration_identity(&local, &remote);
    }

    #[test]
    fn a_title_and_artist_at_the_minimum_leave_too_little_score_for_duration_identity() {
        let (mut local, mut remote) = placeholder_pair();
        local.album_title = Some("Test Album".to_string());
        remote.title = "Test Albun".to_string();
        local.album_artist = Some("Artist Nam".to_string());
        remote.artists = vec!["Artist Nan".to_string()];
        for (l, r) in local.tracks.iter_mut().zip(&remote.tracklist) {
            l.duration_secs = r.duration_secs;
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!((candidate.score.album_title - 0.90).abs() < 1e-9);
        assert!((candidate.score.album_artist - 0.90).abs() < 1e-9);
        assert!(candidate.score.album_score_without_track_titles() < DURATION_IDENTITY_SCORE);
        assert_blocked_without_duration_identity(&local, &remote);
    }

    #[test]
    fn a_track_version_qualifier_still_blocks_duration_identity() {
        let (local, mut remote) = placeholder_pair();
        for track in &mut remote.tracklist {
            track.title = format!("{} (Live)", track.title);
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!(durations_identify_tracks(&local, &candidate));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("track version qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_missing_title_or_length_blocks_duration_identity() {
        let (mut local, remote) = placeholder_pair();
        local.tracks[2].title = None;
        assert_blocked_without_duration_identity(&local, &remote);

        let (local, mut remote) = placeholder_pair();
        remote.tracklist[4].duration_secs = None;
        assert_blocked_without_duration_identity(&local, &remote);

        let (mut local, remote) = placeholder_pair();
        local.tracks[1].duration_secs = None;
        assert_blocked_without_duration_identity(&local, &remote);
    }

    #[test]
    fn titles_that_name_other_tracks_block_duration_identity() {
        // The files are tagged correctly, but the edition orders tracks 2 and 3
        // the other way round and the two lengths are a second apart: each file
        // would take the other's title.
        let titles = [
            "Opening",
            "Second Wind",
            "Hollow",
            "Undertow",
            "Lantern",
            "Closing",
        ];
        let (mut local, mut remote) = spaced_pair(&titles, &titles);
        local.tracks[1].duration_secs = Some(200.0);
        local.tracks[2].duration_secs = Some(201.0);
        remote.tracklist[1].duration_secs = Some(200.0);
        remote.tracklist[2].duration_secs = Some(201.0);
        let control = Candidate::new(&local, remote.clone());
        assert!(!control.mapping.ambiguous && control.mapping.complete);
        assert_eq!(control.score.track_titles, 1.0);

        remote.tracklist.swap(1, 2);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.position = Some((index + 1).to_string());
        }
        remote.tracklist[1].duration_secs = Some(200.0);
        remote.tracklist[2].duration_secs = Some(201.0);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(mapped_in_local_order(&local, &candidate));
        assert!(candidate.score.track_titles < 0.9);
        assert!(candidate.mapping.assigned_pairs().all(|(l, r)| {
            duration_gap(&local, &candidate, l, r).is_some_and(|gap| gap <= 1.5)
        }));
        assert!(!durations_identify_tracks(&local, &candidate));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("every duration agrees")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn a_close_alternative_of_another_album_still_blocks_duration_identity() {
        let (local, mut remote) = placeholder_pair();
        remote.group_id = Some(GroupId::DiscogsMaster(900));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // The same lengths and titles filed under another album are not
        // equivalent audio, and the lengths cannot tell the two apart.
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.group_id = Some(GroupId::DiscogsMaster(901));
        let other = Candidate::new(&local, other);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(!equivalent_audio(&candidate, &other));
        let evaluation = album_evaluation(&local, &remote, &[other]);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("close alternative")),
            "{:?}",
            evaluation.blockers
        );
    }

    /// A local release and a remote release over the same tracks and titles,
    /// where the provider lists no length at all.
    fn unlisted_pair(titles: &[&str]) -> (LocalRelease, RemoteRelease) {
        let local = local_release(titles);
        let mut remote = remote_release(titles);
        for track in &mut remote.tracklist {
            track.duration_secs = None;
        }
        (local, remote)
    }

    const TITLES_6: [&str; 6] = [
        "Opening",
        "Second Wind",
        "Hollow",
        "Undertow",
        "Lantern",
        "Closing",
    ];

    /// Asserts the album is accepted on the text alone, with one warning that
    /// the provider lists no duration for `unlisted` of the tracks.
    fn assert_accepted_on_text(local: &LocalRelease, remote: &RemoteRelease, unlisted: usize) {
        let candidate = Candidate::new(local, remote.clone());
        assert!(
            titles_identify_tracks(local, &candidate),
            "{:?}",
            candidate.score
        );
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        let expected = format!(
            "{} lists no duration for {unlisted} of {} tracks; {UNCONFIRMED_LENGTHS_NOTE}",
            remote.provider(),
            local.tracks.len()
        );
        let warnings: Vec<_> = evaluation
            .warnings
            .iter()
            .filter(|note| note.contains("lists no duration"))
            .collect();
        assert_eq!(warnings, [&expected], "{:?}", evaluation.warnings);
    }

    fn assert_blocked_without_text_identity(local: &LocalRelease, remote: &RemoteRelease) {
        let candidate = Candidate::new(local, remote.clone());
        assert!(!titles_identify_tracks(local, &candidate));
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("lists no duration")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn exact_titles_without_any_remote_length_pass_at_album_scope() {
        let (local, remote) = unlisted_pair(&["One", "Two", "Three"]);
        assert_accepted_on_text(&local, &remote, 3);
    }

    #[test]
    fn exact_titles_pass_on_a_musicbrainz_release_too() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        assert_accepted_on_text(&local, &remote, 6);
    }

    #[test]
    fn the_lengths_a_provider_does_list_must_agree_but_may_be_a_few_seconds_off() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        for index in 0..4 {
            remote.tracklist[index].duration_secs = local.tracks[index].duration_secs;
        }
        assert_accepted_on_text(&local, &remote, 2);

        remote.tracklist[1].duration_secs = Some(local.tracks[1].duration_secs.unwrap() + 2.5);
        assert_accepted_on_text(&local, &remote, 2);

        remote.tracklist[1].duration_secs = Some(local.tracks[1].duration_secs.unwrap() + 4.0);
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn text_identity_does_not_apply_when_every_remote_length_is_known() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        for (track, local_track) in remote.tracklist.iter_mut().zip(&local.tracks) {
            track.duration_secs = local_track.duration_secs;
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!(!titles_identify_tracks(&local, &candidate));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("lists no duration")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn two_tracks_block_text_identity() {
        let (local, remote) = unlisted_pair(&["One", "Two"]);
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn various_artists_need_five_tracks_for_text_identity() {
        let (mut local, mut remote) = unlisted_pair(&["One", "Two", "Three", "Four"]);
        local.artist_kind = ArtistKind::VariousArtists;
        local.album_artist = Some("Various Artists".to_string());
        remote.artists = vec!["Various Artists".to_string()];
        assert_blocked_without_text_identity(&local, &remote);

        let (mut local, mut remote) = unlisted_pair(&["One", "Two", "Three", "Four", "Five"]);
        local.artist_kind = ArtistKind::VariousArtists;
        local.album_artist = Some("Various Artists".to_string());
        remote.artists = vec!["Various Artists".to_string()];
        assert_accepted_on_text(&local, &remote, 5);
    }

    #[test]
    fn one_weak_track_title_blocks_text_identity() {
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[1].title = Some("Secund Wond".to_string());
        let weak = similarity("Secund Wond", "Second Wind");
        assert!((0.75..0.90).contains(&weak), "{weak}");
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn a_weak_album_title_blocks_text_identity() {
        let (mut local, mut remote) = unlisted_pair(&TITLES_6);
        local.album_title = Some("Midnight Garden Songs".to_string());
        remote.title = "Midnight Garden Soundz".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!((0.80..0.90).contains(&candidate.score.album_title));
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn tracks_mapped_out_of_local_order_block_text_identity() {
        // Two tracks share a title, so the swapped pairing scores exactly as the
        // right one does and nothing but the order check can tell them apart.
        let (local, remote) = unlisted_pair(&["Intro", "Intro", "Closing"]);
        let mut candidate = Candidate::new(&local, remote);
        candidate.mapping.ambiguous = false;
        candidate.mapping.pairs = vec![Some(1), Some(0), Some(2)];
        assert!(candidate.mapping.complete);
        assert!(candidate.score.album_score_without_durations() >= TEXT_IDENTITY_SCORE);
        assert!(!mapped_in_local_order(&local, &candidate));
        assert!(!titles_identify_tracks(&local, &candidate));
        let evaluation = evaluate_at_scope(
            &local,
            &candidate,
            None,
            &DiscoveryEvidence::default(),
            MatchScope::Album,
        );
        assert!(!evaluation.auto_accept);
    }

    #[test]
    fn a_missing_local_title_or_length_blocks_text_identity() {
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[2].duration_secs = None;
        assert_blocked_without_text_identity(&local, &remote);

        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[2].title = None;
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn an_extra_remote_track_blocks_text_identity() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        let mut extra = remote.tracklist[5].clone();
        extra.title = "Bonus".to_string();
        remote.tracklist.push(extra);
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn text_identity_needs_album_scope() {
        let (local, remote) = unlisted_pair(&TITLES_6);
        let candidate = Candidate::new(&local, remote);
        assert!(titles_identify_tracks(&local, &candidate));
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("lists no duration")),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn a_close_alternative_of_another_album_still_blocks_text_identity() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.group_id = Some(GroupId::DiscogsMaster(900));
        assert_accepted_on_text(&local, &remote, 6);

        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.group_id = Some(GroupId::DiscogsMaster(901));
        let other = Candidate::new(&local, other);
        let evaluation = album_evaluation(&local, &remote, &[other]);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("close alternative")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn the_known_duration_blocker_appears_once_per_release() {
        // A weak title makes the rule fail, leaving six unknown remote lengths.
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[1].title = Some("Secund Wond".to_string());
        let evaluation = album_evaluation(&local, &remote, &[]);
        let requires = evaluation
            .blockers
            .iter()
            .filter(|note| note.contains("requires a title and known duration"))
            .count();
        assert_eq!(requires, 1, "{:?}", evaluation.blockers);

        // Several offending tracks, local and remote, still give one blocker.
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[0].duration_secs = None;
        local.tracks[1].title = None;
        let evaluation = album_evaluation(&local, &remote, &[]);
        let requires = evaluation
            .blockers
            .iter()
            .filter(|note| note.contains("requires a title and known duration"))
            .count();
        assert_eq!(requires, 1, "{:?}", evaluation.blockers);
    }

    #[test]
    fn near_identical_titles_in_a_different_order_block_text_identity() {
        // The remote swaps parts 1 and 2 but keeps numeric positions, so the
        // positions pair file 1 with `Song Part 2`, which scores 0.909.
        let local_titles = ["Song Part 1", "Song Part 2", "Song Part 3"];
        let (local, mut remote) = unlisted_pair(&["Song Part 2", "Song Part 1", "Song Part 3"]);
        let local_titles_release = local_release(&local_titles);
        let candidate = Candidate::new(&local_titles_release, remote.clone());
        assert_eq!(candidate.mapping.pairs, [Some(0), Some(1), Some(2)]);
        assert!(similarity("Song Part 1", "Song Part 2") >= TITLE_SIMILARITY_THRESHOLD);
        assert!(candidate.score.album_score_without_durations() >= TEXT_IDENTITY_SCORE);
        assert_blocked_without_text_identity(&local_titles_release, &remote);

        // The same release in the right order passes.
        remote.tracklist.swap(0, 1);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.position = Some((index + 1).to_string());
        }
        assert_accepted_on_text(&local_titles_release, &remote, 3);
        drop(local);

        // Roman numerals one letter apart.
        let local = local_release(&["Opening", "Interlude I", "Interlude II", "Closing"]);
        let (_, remote) = unlisted_pair(&["Opening", "Interlude II", "Interlude I", "Closing"]);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            candidate.score.album_score_without_durations() >= TEXT_IDENTITY_SCORE,
            "{:?}",
            candidate.score
        );
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn titles_that_leave_too_little_score_without_the_durations_block_text_identity() {
        let (mut local, mut remote) = unlisted_pair(&TITLES_6);
        local.album_title = Some("Test Album".to_string());
        remote.title = "Test Albun".to_string();
        local.album_artist = Some("Artist Nam".to_string());
        remote.artists = vec!["Artist Nan".to_string()];
        let candidate = Candidate::new(&local, remote.clone());
        assert!((candidate.score.album_title - 0.90).abs() < 1e-9);
        assert!((candidate.score.album_artist - 0.90).abs() < 1e-9);
        assert!(candidate.score.album_score_without_durations() < TEXT_IDENTITY_SCORE);
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn a_zero_or_non_finite_remote_length_is_not_an_unlisted_one() {
        for bad in [0.0, f64::NAN, f64::INFINITY, -3.0] {
            let (local, mut remote) = unlisted_pair(&TITLES_6);
            remote.tracklist[2].duration_secs = Some(bad);
            assert_blocked_without_text_identity(&local, &remote);
        }
    }

    #[test]
    fn an_unofficial_release_never_passes_on_the_text_alone() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.formats = vec!["CDr (Album)".to_string()];
        assert_accepted_on_text(&local, &remote, 6);

        remote.formats = vec!["CDr (Album, Unofficial Release)".to_string()];
        assert_blocked_without_text_identity(&local, &remote);

        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        remote.provider_metadata = serde_json::json!({"status": "Official"});
        assert_accepted_on_text(&local, &remote, 6);

        remote.provider_metadata = serde_json::json!({"status": "Bootleg"});
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn a_local_file_that_is_another_take_blocks_text_identity() {
        for suffix in [
            "(Alternate Take)",
            "(Peel Session)",
            "(Outtake)",
            "(Rehearsal)",
            "[Take 2]",
            "(Take Two)",
            "(Alt Take)",
            "(BBC Sessions)",
        ] {
            let (mut local, remote) = unlisted_pair(&TITLES_6);
            local.tracks[2].title = Some(format!("Hollow {suffix}"));
            assert_blocked_without_text_identity(&local, &remote);
        }

        // An extension or a release tag after the bracket does not hide it.
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[2].title = Some("Hollow (Alternate Take).flac".to_string());
        assert_blocked_without_text_identity(&local, &remote);

        let (mut local, remote) = unlisted_pair(&TITLES_6);
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.title = Some(format!("{}-amrc", TITLES_6[index]));
        }
        local.tracks[2].title = Some("Hollow (Alternate Take)-amrc".to_string());
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(
            cleaned[2].as_ref().map(|title| title.text.as_str()),
            Some("Hollow (Alternate Take)")
        );
        assert_blocked_without_text_identity(&local, &remote);

        // An ordinary subtitle that starts with the word is not another take,
        // and a suffix on both sides is not one-sided.
        let (mut local, remote) = unlisted_pair(&TITLES_6);
        local.tracks[2].title = Some("Hollow (Take Off Your Clothes)".to_string());
        assert_accepted_on_text(&local, &remote, 6);
        let (mut local, mut remote) = unlisted_pair(&TITLES_6);
        local.tracks[2].title = Some("Hollow (Peel Session)".to_string());
        remote.tracklist[2].title = "Hollow (Peel Session)".to_string();
        assert_accepted_on_text(&local, &remote, 6);
    }

    #[test]
    fn a_remote_session_suffix_or_take_in_a_subtitle_keeps_its_old_score() {
        // The provider's subtitle was always dropped at a small cost.
        let base = "Spank-Master";
        let subtitled = "Spank-Master (Take Off Your Clothes)";
        for (local, remote) in [(base, subtitled), (subtitled, base)] {
            let value = track_title_similarity(local, None, remote);
            assert!((value - 0.95).abs() < 1e-9, "{local} / {remote}: {value}");
        }

        // `X (Peel session)` on the remote side against plain local tags still
        // passes the normal gates.
        let local_titles = ["Opening", "Second Wind", "Hollow", "Undertow"];
        let remote_titles = [
            "Opening",
            "Second Wind (Peel session)",
            "Hollow (Peel session)",
            "Undertow",
        ];
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            candidate.score.track_titles >= 0.95,
            "{:?}",
            candidate.score
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn an_edition_of_the_same_album_whose_lengths_contradict_the_files_blocks_text_identity() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.group_id = Some(GroupId::DiscogsMaster(900));

        // Another edition of the album lists every length 30 s off.
        let mut off = remote.clone();
        off.id = ReleaseId::Discogs(43);
        for (track, local_track) in off.tracklist.iter_mut().zip(&local.tracks) {
            track.duration_secs = local_track.duration_secs.map(|secs| secs + 30.0);
        }
        let off = Candidate::new(&local, off);
        assert!(off.score.album_title >= 0.90 && off.score.track_titles >= 0.90);
        let evaluation = album_evaluation(&local, &remote, std::slice::from_ref(&off));
        assert!(!evaluation.auto_accept);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.contains("lists no duration")),
            "{:?}",
            evaluation.warnings
        );

        // Lengths that are only a few seconds off do not contradict the files.
        let mut near = remote.clone();
        near.id = ReleaseId::Discogs(44);
        for (track, local_track) in near.tracklist.iter_mut().zip(&local.tracks) {
            track.duration_secs = local_track.duration_secs.map(|secs| secs + 5.0);
        }
        let evaluation = album_evaluation(&local, &remote, &[Candidate::new(&local, near)]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn a_same_album_edition_without_lengths_does_not_block_text_identity() {
        let (local, mut remote) = unlisted_pair(&TITLES_6);
        remote.group_id = Some(GroupId::DiscogsMaster(900));
        let mut twin = remote.clone();
        twin.id = ReleaseId::Discogs(43);
        let evaluation = album_evaluation(&local, &remote, &[Candidate::new(&local, twin)]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn masterless_releases_are_not_one_album() {
        // Two releases with no master and the same text are not known to be the
        // same album, so each blocks the other as a close alternative.
        let (local, remote) = unlisted_pair(&TITLES_6);
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.formats = vec!["CDr (Album, Unofficial Release)".to_string()];
        let evaluation = album_evaluation(&local, &remote, &[Candidate::new(&local, other)]);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|note| note.contains("close alternative")),
            "{:?}",
            evaluation.blockers
        );
    }

    fn musicbrainz_primary(local: &LocalRelease, offset: f64) -> Candidate {
        let (_, mut release) = unlisted_pair(&TITLES_6);
        release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        for (track, local_track) in release.tracklist.iter_mut().zip(&local.tracks) {
            track.duration_secs = local_track.duration_secs.map(|secs| secs + offset);
        }
        Candidate::new(local, release)
    }

    #[test]
    fn the_fallback_check_sees_primary_lengths_that_contradict_the_files() {
        let (local, fallback_release) = unlisted_pair(&TITLES_6);
        let fallback = Candidate::new(&local, fallback_release);

        // Same text, every length 30 s off: not plausible by score, but a live
        // recording of the same album would look like this.
        let primary = musicbrainz_primary(&local, 30.0);
        assert!(plausibility_score(&local, &primary) < PLAUSIBLE_PRIMARY_SCORE);
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            true,
        );
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary lengths contradict the files")),
            "{conflicts:?}"
        );

        // Lengths that agree raise nothing.
        let primary = musicbrainz_primary(&local, 0.5);
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            true,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");

        // A primary that also lists no length cannot contradict anything.
        let (_, mut bare) = unlisted_pair(&TITLES_6);
        bare.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let conflicts = fallback_conflicts(
            &local,
            &[Candidate::new(&local, bare)],
            &fallback,
            true,
            true,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");

        // A fallback that lists every length is judged as before.
        let (_, mut listed) = unlisted_pair(&TITLES_6);
        for (track, local_track) in listed.tracklist.iter_mut().zip(&local.tracks) {
            track.duration_secs = local_track.duration_secs;
        }
        let listed = Candidate::new(&local, listed);
        let primary = musicbrainz_primary(&local, 30.0);
        let conflicts = fallback_conflicts(&local, &[primary], &listed, true, true);
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    #[test]
    fn the_fallback_check_leaves_an_edition_scope_fallback_with_missing_lengths_alone() {
        // An embedded ID can settle the edition with a length or two missing; the
        // primary's length for that track is then not a contradiction to rule on.
        let (local, mut fallback_release) = unlisted_pair(&TITLES_6);
        for (index, (track, local_track)) in fallback_release
            .tracklist
            .iter_mut()
            .zip(&local.tracks)
            .enumerate()
        {
            if index != 3 {
                track.duration_secs = local_track.duration_secs;
            }
        }
        let fallback = Candidate::new(&local, fallback_release);
        let mut primary = musicbrainz_primary(&local, 0.0);
        primary.release.tracklist[3].duration_secs =
            local.tracks[3].duration_secs.map(|secs| secs + 30.0);
        let primary = Candidate::new(&local, primary.release);
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");

        // The same pair is a conflict when the fallback's accept is text-only.
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            true,
        );
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary lengths contradict the files")),
            "{conflicts:?}"
        );
    }

    #[test]
    fn the_fallback_check_ignores_titles_of_a_primary_plausible_only_by_durations() {
        // Romanized local titles, a Discogs fallback that agrees with them, and
        // a MusicBrainz primary titled in Japanese.
        let romanized = [
            "Aoi Sora",
            "Yoru no Uta",
            "Kaze no Machi",
            "Tsuki no Hikari",
            "Hana Fubuki",
            "Owari no Hi",
        ];
        let japanese = [
            "\u{9752}\u{3044}\u{7a7a}",
            "\u{591c}\u{306e}\u{6b4c}",
            "\u{98a8}\u{306e}\u{8857}",
            "\u{6708}\u{306e}\u{5149}",
            "\u{82b1}\u{5439}\u{96ea}",
            "\u{7d42}\u{308f}\u{308a}\u{306e}\u{65e5}",
        ];
        let (local, primary_release) = spaced_pair(&romanized, &japanese);
        let mut primary_release = primary_release;
        primary_release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let primary = Candidate::new(&local, primary_release);
        assert!(plausibility_score(&local, &primary) < PLAUSIBLE_PRIMARY_SCORE);
        assert!(durations_identify_tracks(&local, &primary));

        let (_, fallback_release) = spaced_pair(&romanized, &romanized);
        let fallback = Candidate::new(&local, fallback_release);
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");

        // Edition qualifiers are still compared against such a primary.
        let mut live = primary.release.clone();
        live.edition_notes = vec!["Live".to_string()];
        let live = Candidate::new(&local, live);
        let conflicts = fallback_conflicts(&local, &[live], &fallback, true, false);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{conflicts:?}"
        );
        assert!(
            !conflicts
                .iter()
                .any(|conflict| conflict.contains("track titles differ")),
            "{conflicts:?}"
        );
    }

    #[test]
    fn the_fallback_check_trusts_a_primary_whose_durations_identify_the_tracks() {
        let (local, remote) = placeholder_pair();
        let mut primary_release = remote.clone();
        primary_release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        primary_release.edition_notes = vec!["Live".to_string()];
        let primary = Candidate::new(&local, primary_release);
        assert!(plausibility_score(&local, &primary) < PLAUSIBLE_PRIMARY_SCORE);
        assert!(durations_identify_tracks(&local, &primary));

        let fallback = Candidate::new(&local, remote.clone());
        let conflicts = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{conflicts:?}"
        );

        // A length that is off leaves the primary implausible, so it is ignored.
        let mut off = local.clone();
        off.tracks[3].duration_secs = Some(off.tracks[3].duration_secs.unwrap() + 2.0);
        let primary = Candidate::new(&off, primary.release.clone());
        assert!(plausibility_score(&off, &primary) < PLAUSIBLE_PRIMARY_SCORE);
        assert!(!durations_identify_tracks(&off, &primary));
        let fallback = Candidate::new(&off, remote);
        let conflicts = fallback_conflicts(&off, &[primary], &fallback, true, false);
        assert!(conflicts.is_empty(), "{conflicts:?}");
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

    // -----------------------------------------------------------------------
    // Disc folders
    // -----------------------------------------------------------------------

    /// A release of several media in MusicBrainz style: medium `m` (from 1) holds
    /// `counts[m - 1]` tracks titled `Song m-k`, `200 + 10m + k` seconds long.
    fn disc_set(counts: &[usize]) -> RemoteRelease {
        let mut remote = remote_release(&[]);
        for (index, count) in counts.iter().enumerate() {
            let medium = index as u32 + 1;
            for position in 1..=*count as u32 {
                remote.tracklist.push(RemoteTrack {
                    position: Some(format!("{medium}-{position}")),
                    title: format!("Song {medium}-{position}"),
                    artists: Vec::new(),
                    duration_secs: Some(f64::from(200 + 10 * medium + position)),
                    entry_type: TrackEntryType::Track,
                    medium_position: Some(medium),
                    track_position: Some(position),
                    release_track_id: None,
                    recording_id: None,
                    artist_credit: None,
                });
            }
        }
        remote
    }

    /// The files of one disc of `disc_set`, numbered from 1, untagged for disc.
    fn disc_folder(directory: &str, medium: u32, count: usize) -> LocalRelease {
        let titles: Vec<String> = (1..=count).map(|k| format!("Song {medium}-{k}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let mut local = local_release(&titles);
        local.directory = directory.to_string();
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.path = format!("{directory}/{:02}.flac", index + 1);
            track.duration_secs = Some(f64::from(200 + 10 * medium + index as u32 + 1));
        }
        local
    }

    #[test]
    fn disc_hints_come_from_folder_names_titles_and_disc_tags() {
        let hint = |directory: &str, album: &str, discs: &[Option<i64>]| {
            let mut local = local_release(&["One", "Two"]);
            local.directory = directory.to_string();
            local.album_title = Some(album.to_string());
            for (track, disc) in local.tracks.iter_mut().zip(discs) {
                track.disc = *disc;
            }
            local_disc_hint(&local).map(|hint| (hint.number, hint.source))
        };
        let none = [None, None];
        let folder = |number| Some((number, DiscHintSource::Folder));

        for (name, number) in [
            ("CD1", 1),
            ("Cd1", 1),
            ("cd02", 2),
            ("Disc_2", 2),
            ("Disk 2", 2),
            ("CD 2", 2),
            ("disc-3", 3),
            ("CD.4", 4),
            ("CD12", 12),
            ("CD123", 123),
        ] {
            assert_eq!(
                hint(&format!("/music/Set-2CD/{name}"), "Set", &none),
                folder(number),
                "{name}"
            );
        }
        assert_eq!(hint("C:\\Music\\Set\\CD2", "Set", &none), folder(2));
        assert_eq!(hint("/music/Set/CD2/", "Set", &none), folder(2));
        for name in [
            "Discography",
            "Disco 2",
            "Disco2",
            "CD1 Bonus",
            "DVD",
            "DVD2",
            "CD",
            "CD0",
            "CD1234",
            "CDS2",
            "Disc2x",
            "Bonus",
        ] {
            assert_eq!(
                hint(&format!("/music/Set/{name}"), "Set", &none),
                None,
                "{name}"
            );
        }

        // A disc marker ending the album title.
        let title = |album: &str| hint("/music/Set", album, &none);
        let titled = |number| Some((number, DiscHintSource::AlbumTitle));
        for (album, number) in [
            ("Set CD2", 2),
            ("Set Cd 2", 2),
            ("Set Disc2", 2),
            ("Set (Disc 2)", 2),
            ("Set [CD2]", 2),
            ("Set (CD2", 2),
            ("Set - CD2", 2),
            ("Set Disk 3", 3),
        ] {
            assert_eq!(title(album), titled(number), "{album}");
        }
        for album in [
            "Set CDS2",
            "Hitdisc 434D",
            "Hit Disc 54 June 2002",
            "CD2",
            "(Disc 2)",
            "Set CD",
            "Set CD0",
            "Set CD1234",
            "Set Discs 2",
            "Set",
        ] {
            assert_eq!(title(album), None, "{album}");
        }
        assert_eq!(title_disc_suffix("Set (Disc 2)"), Some(("Set".into(), 2)));
        assert_eq!(title_disc_suffix("Set - CD2"), Some(("Set".into(), 2)));
        assert_eq!(title_disc_suffix("Set (CD2"), Some(("Set".into(), 2)));
        assert_eq!(title_disc_suffix("Hitdisc 434D"), None);

        // The folder and the title say the same, or they conflict.
        assert_eq!(hint("/music/Set/CD2", "Set CD2", &none), folder(2));
        assert_eq!(hint("/music/Set/CD2", "Set CD1", &none), None);

        // Disc tags alone count from disc 2 on; disc 1 is what an absent tag means.
        assert_eq!(hint("/music/Set", "Set", &[Some(1), Some(1)]), None);
        assert_eq!(
            hint("/music/Set", "Set", &[Some(2), Some(2)]),
            Some((2, DiscHintSource::DiscTags))
        );
        // Mixed or partly tagged files give no tag hint at all.
        assert_eq!(hint("/music/Set", "Set", &[Some(1), Some(2)]), None);
        assert_eq!(hint("/music/Set", "Set", &[Some(2), None]), None);
        assert_eq!(hint("/music/Set", "Set", &[Some(0), Some(0)]), None);
        // The sources must agree, but a uniform tag of disc 1 is what per-disc
        // rips carry on every disc, so it never vetoes a folder or title.
        assert_eq!(
            hint("/music/Set/CD2", "Set", &[Some(1), Some(1)]),
            folder(2)
        );
        assert_eq!(
            hint("/music/Set", "Set CD2", &[Some(1), Some(1)]),
            Some((2, DiscHintSource::AlbumTitle))
        );
        assert_eq!(hint("/music/Set/CD2", "Set", &[Some(3), Some(3)]), None);
        assert_eq!(
            hint("/music/Set/CD1", "Set", &[Some(1), Some(1)]),
            folder(1)
        );
        assert_eq!(
            hint("/music/Set/CD2", "Set", &[Some(2), Some(2)]),
            folder(2)
        );
        assert_eq!(
            hint("/music/Set", "Set CD2", &[Some(2), Some(2)]),
            Some((2, DiscHintSource::AlbumTitle))
        );
        assert_eq!(hint("/music/Set", "Set CD2", &[Some(3), Some(3)]), None);
    }

    #[test]
    fn a_disc_folder_maps_onto_its_own_medium() {
        let remote = disc_set(&[16, 17]);
        let local = disc_folder("/music/Set-2CD/CD2", 2, 17);

        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, Some(2));
        assert!(mapping.complete, "{:?}", mapping.notes);
        assert!(!mapping.ambiguous, "{:?}", mapping.notes);
        assert_eq!(mapping.assigned_count(), 17);
        for (local_index, remote_index) in mapping.assigned_pairs() {
            assert_eq!(remote.tracklist[remote_index].medium(), Some(2));
            assert_eq!(
                remote.tracklist[remote_index].track_position,
                Some(local_index as u32 + 1)
            );
        }
        // The same files in a folder that names no disc stay incomplete.
        let mut unnumbered = local.clone();
        unnumbered.directory = "/music/Set-2CD/Bonus".to_string();
        let mapping = map_tracks(&unnumbered, &remote);
        assert_eq!(mapping.medium, None);
        assert!(!mapping.complete);

        // A disc anchors on identifiers within its medium as well.
        let mut anchored = disc_set(&[16, 17]);
        let ids: Vec<Mbid> = (1..=33)
            .map(|n| mbid(&format!("{n:08}-1111-4111-8111-{n:012}")))
            .collect();
        for (track, id) in anchored.tracklist.iter_mut().zip(&ids) {
            track.release_track_id = Some(*id);
        }
        let mut local = disc_folder("/music/Set-2CD/CD2", 2, 17);
        for (track, id) in local.tracks.iter_mut().zip(&ids[16..]) {
            track.release_track_id = Some(*id);
        }
        let mapping = map_tracks(&local, &anchored);
        assert_eq!(mapping.medium, Some(2));
        assert!(mapping.complete, "{:?}", mapping.notes);
        assert!(!mapping.ambiguous, "{:?}", mapping.notes);
        assert_eq!(mapping.pairs[0], Some(16));
    }

    #[test]
    fn a_complete_whole_release_mapping_wins_over_a_medium() {
        let remote = disc_set(&[2, 2]);
        let titles = ["Song 1-1", "Song 1-2", "Song 2-1", "Song 2-2"];
        let mut local = local_release(&titles);
        local.directory = "/music/Set/CD2".to_string();
        for (track, (disc, number)) in local
            .tracks
            .iter_mut()
            .zip([(1, 1), (1, 2), (2, 1), (2, 2)])
        {
            track.disc = Some(disc);
            track.track_number = Some(number);
            track.duration_secs = Some(200.0 + 10.0 * disc as f64 + number as f64);
        }
        assert_eq!(local_disc_hint(&local).map(|hint| hint.number), Some(2));
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, None);
        assert!(mapping.complete, "{:?}", mapping.notes);
    }

    #[test]
    fn a_disc_with_a_different_track_count_stays_incomplete_on_its_medium() {
        let remote = disc_set(&[16, 17]);
        let local = disc_folder("/music/Set-2CD/CD2", 2, 16);
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, Some(2));
        assert!(!mapping.complete);
        assert!(
            mapping
                .notes
                .contains(&"16 local file(s) versus 17 playable track(s) on disc 2".to_string()),
            "{:?}",
            mapping.notes
        );
        // Every file still maps within the medium, for review.
        assert!(
            mapping
                .assigned_pairs()
                .all(|(_, remote_index)| remote.tracklist[remote_index].medium() == Some(2))
        );
    }

    #[test]
    fn a_disc_number_the_release_does_not_have_falls_back_to_the_whole_release() {
        let remote = disc_set(&[3, 3]);
        let local = disc_folder("/music/Set/CD3", 2, 3);
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, None);
        assert!(!mapping.complete);
        assert!(
            mapping
                .notes
                .contains(&"3 local file(s) versus 6 playable remote track(s)".to_string()),
            "{:?}",
            mapping.notes
        );

        // A release of one medium has no disc to choose.
        let single = disc_set(&[3]);
        let local = disc_folder("/music/Set/CD2", 1, 2);
        assert_eq!(map_tracks(&local, &single).medium, None);
    }

    #[test]
    fn unsupported_media_elsewhere_still_block_a_disc_match() {
        let mut remote = disc_set(&[2, 2]);
        remote
            .structure_issues
            .push("medium 3 uses unsupported format 'DVD-Video'".to_string());
        let local = disc_folder("/music/Set/CD2", 2, 2);
        let candidate = Candidate::new(&local, remote);
        assert_eq!(candidate.mapping.medium, Some(2));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(!evaluation.can_apply(&candidate));
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("unsupported")),
            "{:?}",
            evaluation.blockers
        );

        // An index track on another disc blocks as well.
        let mut remote = disc_set(&[2, 2]);
        remote.tracklist[0].entry_type = TrackEntryType::Index;
        let candidate = Candidate::new(&local, remote);
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.can_apply(&candidate));
    }

    #[test]
    fn discogs_disc_positions_split_media_but_vinyl_sides_do_not() {
        let with_positions = |positions: &[&str]| {
            let mut remote = remote_release(&vec!["Song"; positions.len()]);
            for (track, position) in remote.tracklist.iter_mut().zip(positions) {
                track.position = Some((*position).to_string());
            }
            remote
        };
        assert_eq!(
            remote_media(&with_positions(&["1-1", "1-2", "2-1"])),
            Some(BTreeMap::from([(1, 2), (2, 1)]))
        );
        assert_eq!(
            remote_media(&with_positions(&["A1", "A2", "B1", "D4"])),
            None
        );
        assert_eq!(
            remote_media(&with_positions(&["CD1-1", "CD1-2", "CD2-1"])),
            None
        );
        // One medium is no choice, and a track without a position is no medium.
        assert_eq!(remote_media(&with_positions(&["1", "2", "3"])), None);
        assert_eq!(remote_media(&with_positions(&["1-1", "1-2"])), None);
        let mut missing = with_positions(&["1-1", "2-1", "2-2"]);
        missing.tracklist[1].position = None;
        assert_eq!(remote_media(&missing), None);
        // Headings are not playable and need no medium.
        let mut heading = with_positions(&["1-1", "", "2-1"]);
        heading.tracklist[1].entry_type = TrackEntryType::Heading;
        assert_eq!(
            remote_media(&heading),
            Some(BTreeMap::from([(1, 1), (2, 1)]))
        );

        // A Discogs release of `N-M` positions maps a disc folder onto its disc.
        let mut remote = with_positions(&["1-1", "1-2", "2-1", "2-2"]);
        for (track, title) in remote
            .tracklist
            .iter_mut()
            .zip(["Song 1-1", "Song 1-2", "Song 2-1", "Song 2-2"])
        {
            track.title = title.to_string();
        }
        let local = disc_folder("/music/Set/Disc 2", 2, 2);
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, Some(2));
        assert_eq!(mapping.pairs, vec![Some(2), Some(3)]);
        assert!(mapping.complete, "{:?}", mapping.notes);
    }

    #[test]
    fn a_disc_marker_in_the_album_title_drops_only_for_the_matched_medium() {
        let remote = disc_set(&[3, 3]);
        let mut local = disc_folder("/music/Set/Disc 2", 2, 3);
        local.album_title = Some("Test Album Disc 2".to_string());

        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, Some(2));
        assert!(
            candidate.score.album_title >= 0.96,
            "{}",
            candidate.score.album_title
        );
        assert!(candidate.score.album_title < 1.0);

        // A mapping of another medium, or of none, leaves the title as it is.
        let mut other = candidate.mapping.clone();
        other.medium = Some(1);
        let kept = score(&local, &remote, &other);
        assert!(kept.album_title < 0.9, "{}", kept.album_title);
        other.medium = None;
        assert_eq!(score(&local, &remote, &other).album_title, kept.album_title);

        // The marker also names the disc when the folder does not.
        let mut loose = local.clone();
        loose.directory = "/music/Set".to_string();
        let hint = local_disc_hint(&loose).unwrap();
        assert_eq!(hint.source, DiscHintSource::AlbumTitle);
        assert_eq!(map_tracks(&loose, &remote).medium, Some(2));
    }

    #[test]
    fn one_complete_disc_is_accepted_as_an_album_match() {
        let remote = disc_set(&[4, 4]);
        let local = disc_folder("/music/Set-2CD/CD2", 2, 4);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, Some(2));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(evaluation.match_scope, MatchScope::Album);
        assert!(
            evaluation.evidence.contains(
                &"the files are disc 2 of 2 of this release (disc number from the folder name)"
                    .to_string()
            ),
            "{:?}",
            evaluation.evidence
        );
        assert!(evaluation.can_apply(&candidate));

        // Disc tags name their source too.
        let mut tagged = local.clone();
        tagged.directory = "/music/Set".to_string();
        for track in &mut tagged.tracks {
            track.disc = Some(2);
        }
        let candidate = Candidate::new(&tagged, remote.clone());
        let evaluation =
            evaluate_for_matching(&tagged, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            evaluation
                .evidence
                .iter()
                .any(|line| line.ends_with("(disc number from the disc tags)")),
            "{:?}",
            evaluation.evidence
        );

        // The same files in a folder that names no disc are not accepted.
        let mut unnumbered = local.clone();
        unnumbered.directory = "/music/Set-2CD/Bonus".to_string();
        let candidate = Candidate::new(&unnumbered, remote);
        let evaluation =
            evaluate_for_matching(&unnumbered, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .contains(&"track mapping is incomplete".to_string())
        );
    }

    #[test]
    fn fallback_conflicts_compare_disc_matches_track_by_track() {
        let local = disc_folder("/music/Set/CD2", 2, 4);
        let mut primary_release = disc_set(&[4, 4]);
        primary_release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let primary = Candidate::new(&local, primary_release);
        assert_eq!(primary.mapping.medium, Some(2));

        let fallback = Candidate::new(&local, disc_set(&[4, 4]));
        assert_eq!(fallback.mapping.medium, Some(2));
        let agreeing = fallback_conflicts(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert!(agreeing.is_empty(), "{agreeing:?}");

        // A length of the fallback's disc 2 that the primary's disc 2 contradicts.
        let mut longer = disc_set(&[4, 4]);
        longer.tracklist[6].duration_secs = Some(longer.tracklist[6].duration_secs.unwrap() + 5.0);
        let fallback = Candidate::new(&local, longer);
        let conflicts = fallback_conflicts(&local, &[primary], &fallback, true, false);
        assert_eq!(
            conflicts,
            vec!["primary and fallback durations differ for local track 3".to_string()]
        );
    }

    #[test]
    fn mapping_medium_round_trips_and_defaults_to_the_whole_release() {
        let remote = disc_set(&[2, 2]);
        let local = disc_folder("/music/Set/CD2", 2, 2);
        let disc = map_tracks(&local, &remote);
        assert_eq!(disc.medium, Some(2));
        let json = serde_json::to_string(&disc).unwrap();
        assert!(json.contains("\"medium\":2"), "{json}");
        let decoded: Mapping = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.medium, Some(2));
        assert_eq!(decoded.pairs, disc.pairs);

        let whole = map_tracks(
            &local_release(&["One", "Two"]),
            &remote_release(&["One", "Two"]),
        );
        assert_eq!(whole.medium, None);
        let json = serde_json::to_string(&whole).unwrap();
        assert!(!json.contains("medium"), "{json}");

        // A mapping stored before the field existed is a whole-release mapping.
        let old: Mapping =
            serde_json::from_str(r#"{"pairs":[0,1],"complete":true,"ambiguous":false,"notes":[]}"#)
                .unwrap();
        assert_eq!(old.medium, None);
    }

    #[test]
    fn lengths_identify_the_tracks_of_a_disc_in_medium_order() {
        let remote = disc_set(&[6, 6]);
        let mut local = disc_folder("/music/Set/CD2", 2, 6);
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.title = Some(format!("Track{:02}", index + 1));
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, Some(2));
        assert!(candidate.mapping.complete, "{:?}", candidate.mapping.notes);
        assert!(mapped_in_local_order(&local, &candidate));
        assert!(durations_identify_tracks(&local, &candidate));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        // Pairs on the wrong tracks of the medium are not in order.
        let mut swapped = candidate.clone();
        swapped.mapping.pairs.swap(0, 1);
        assert!(!mapped_in_local_order(&local, &swapped));
    }

    #[test]
    fn a_structure_issue_alone_keeps_the_whole_release_mapping() {
        // Four files fill the four tracks of the release, but a medium that is not
        // a CD makes the release unusable: the review shows the 4/4 mapping.
        let mut remote = disc_set(&[2, 2]);
        remote
            .structure_issues
            .push("medium 3 uses unsupported format 'DVD-Video'".to_string());
        let mut local = local_release(&["Song 1-1", "Song 1-2", "Song 2-1", "Song 2-2"]);
        local.directory = "/music/Set/CD2".to_string();
        for (track, (disc, number)) in local
            .tracks
            .iter_mut()
            .zip([(1, 1), (1, 2), (2, 1), (2, 2)])
        {
            track.disc = Some(disc);
            track.track_number = Some(number);
            track.duration_secs = Some(200.0 + 10.0 * disc as f64 + number as f64);
        }
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, None);
        assert_eq!(mapping.assigned_count(), 4);
        assert!(!mapping.complete);

        // With a differing count the disc mapping is still chosen.
        let disc = disc_folder("/music/Set/CD2", 2, 2);
        assert_eq!(map_tracks(&disc, &remote).medium, Some(2));
    }

    #[test]
    fn disc_labels_use_the_highest_medium_number() {
        let remote = disc_set(&[2, 2]);
        assert_eq!(disc_label(&remote, 2), "disc 2 of 2");
        // Numbering with a gap: media 1 and 3.
        let mut gap = disc_set(&[2, 2]);
        for track in gap.tracklist.iter_mut().skip(2) {
            track.medium_position = Some(3);
            track.position = Some(format!("3-{}", track.track_position.unwrap()));
        }
        assert_eq!(disc_label(&gap, 3), "disc 3 of 3");
        let local = disc_folder("/music/Set/CD3", 2, 2);
        let candidate = Candidate::new(&local, gap.clone());
        assert_eq!(candidate.mapping.medium, Some(3));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(
            evaluation
                .evidence
                .iter()
                .any(|line| line.starts_with("the files are disc 3 of 3 of this release")),
            "{:?}",
            evaluation.evidence
        );
        // A release that does not name its media has no total.
        assert_eq!(disc_label(&remote_release(&["One"]), 2), "disc 2");
    }

    #[test]
    fn a_discogs_release_with_a_video_format_is_not_split_into_discs() {
        let local = disc_folder("/music/Set/CD2", 2, 2);
        let mut discogs = disc_set(&[2, 2]);
        assert_eq!(map_tracks(&local, &discogs).medium, Some(2));
        for format in ["DVD (NTSC)", "Blu-ray", "VHS", "dvd video"] {
            discogs.formats = vec!["CD (Album)".to_string(), format.to_string()];
            assert!(lists_video_format(&discogs), "{format}");
            let mapping = map_tracks(&local, &discogs);
            assert_eq!(mapping.medium, None, "{format}");
            assert!(!mapping.complete);
        }
        // MusicBrainz reports its video media as structure issues instead.
        let mut musicbrainz = discogs.clone();
        musicbrainz.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        assert!(!lists_video_format(&musicbrainz));
        assert_eq!(map_tracks(&local, &musicbrainz).medium, Some(2));
    }

    #[test]
    fn an_identifier_from_another_disc_is_reported_as_such() {
        let mut remote = disc_set(&[2, 2]);
        let ids = [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "33333333-3333-4333-8333-333333333333",
        ];
        for (track, id) in remote.tracklist.iter_mut().zip(ids) {
            track.release_track_id = Some(mbid(id));
        }
        let mut local = disc_folder("/music/Set/CD2", 2, 2);
        // The first file carries an ID of disc 1, the second one the release lacks.
        local.tracks[0].release_track_id = Some(mbid(ids[0]));
        local.tracks[1].release_track_id = Some(mbid("99999999-9999-4999-8999-999999999999"));
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, Some(2));
        assert!(
            mapping.notes.contains(
                &"release-track ID 11111111-1111-4111-8111-111111111111 from 'Song 2-1' \
                  is on another disc of this release"
                    .to_string()
            ),
            "{:?}",
            mapping.notes
        );
        assert!(
            mapping.notes.contains(
                &"release-track ID 99999999-9999-4999-8999-999999999999 from 'Song 2-2' \
                  is not on the remote release"
                    .to_string()
            ),
            "{:?}",
            mapping.notes
        );
    }

    // -----------------------------------------------------------------------
    // Self-titled placeholders and junk tags
    // -----------------------------------------------------------------------

    const SELF_TITLED_TRACKS: [&str; 6] = ["One", "Two", "Three", "Four", "Five", "Six"];

    fn self_titled_pair(artist: &str, remote_title: &str) -> (LocalRelease, RemoteRelease) {
        let mut local = local_release(&SELF_TITLED_TRACKS);
        local.album_title = Some("Self Titled".to_string());
        local.album_artist = Some(artist.to_string());
        let mut remote = remote_release(&SELF_TITLED_TRACKS);
        remote.title = remote_title.to_string();
        remote.artists = vec![artist.to_string()];
        (local, remote)
    }

    fn album_title_score(local: &LocalRelease, remote: &RemoteRelease) -> f64 {
        Candidate::new(local, remote.clone()).score.album_title
    }

    #[test]
    fn recognizes_self_titled_placeholders() {
        for title in [
            "Self Titled",
            "Self-Titled",
            "self titled",
            "S/T",
            "s/t",
            "ST",
            "st",
            "Same",
            "Eponymous",
            "Self Titled EP",
            "S/T 7inch",
            "Same-7 Inch Vinyl",
        ] {
            assert!(is_self_titled(title), "{title}");
        }
        for title in [
            "Self Titled Demo",
            "Untitled",
            "Stage",
            "Stay",
            "Samey",
            "The Same Old Story",
            "",
        ] {
            assert!(!is_self_titled(title), "{title}");
        }
    }

    #[test]
    fn recognizes_junk_titles_and_artists() {
        for title in ["Unknown Title", "unknown album", " . ", "."] {
            assert!(is_junk_title(title), "{title}");
        }
        for title in ["Untitled", "Unknown Pleasures", "Self Titled", ""] {
            assert!(!is_junk_title(title), "{title}");
        }
        assert!(is_junk_artist("Unknown Artist"));
        assert!(is_junk_artist("  unknown artist "));
        assert!(!is_junk_artist("Unknown Mortal Orchestra"));
    }

    #[test]
    fn the_tag_identity_names_the_artist_for_a_placeholder() {
        let (mut local, _) = self_titled_pair("Goldfinger", "Goldfinger");
        let identity = tag_identity(&local);
        assert_eq!(identity.artist.as_deref(), Some("Goldfinger"));
        assert_eq!(identity.title.as_deref(), Some("Goldfinger"));

        // A real title stays, a junk or missing one is no title.
        local.album_title = Some("Hello".to_string());
        assert_eq!(tag_identity(&local).title.as_deref(), Some("Hello"));
        local.album_title = Some("Unknown Title".to_string());
        assert_eq!(tag_identity(&local).title, None);
        local.album_title = None;
        assert_eq!(tag_identity(&local).title, None);

        // No artist to name: the placeholder stays as written.
        local.album_title = Some("Self Titled".to_string());
        local.artist_kind = ArtistKind::VariousArtists;
        let identity = tag_identity(&local);
        assert_eq!(identity.artist, None);
        assert_eq!(identity.title.as_deref(), Some("Self Titled"));
        local.artist_kind = ArtistKind::AlbumArtist;
        local.album_artist = Some("Unknown Artist".to_string());
        let identity = tag_identity(&local);
        assert_eq!(identity.artist, None);
        assert_eq!(identity.title.as_deref(), Some("Self Titled"));
        local.album_artist = None;
        assert_eq!(tag_identity(&local).title.as_deref(), Some("Self Titled"));
    }

    #[test]
    fn a_self_titled_release_compares_with_the_artist_name() {
        let (local, remote) = self_titled_pair("Goldfinger", "Goldfinger");
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (candidate.score.album_title - 0.97).abs() < 1e-9,
            "{}",
            candidate.score.album_title
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(evaluation.match_scope, MatchScope::Album);
    }

    #[test]
    fn a_self_titled_reading_never_outweighs_the_tracks() {
        // The 1999-era EP tracklist against the 2001 album, retitled
        // `Jimmy Eat World` after its first release.
        let local_titles = [
            "Christmas Card",
            "Rockstar",
            "Sweetheart",
            "Lucky Denver Mint",
        ];
        let remote_titles = [
            "Bleed American",
            "A Praise Chorus",
            "The Middle",
            "Sweetness",
        ];
        let mut local = local_release(&local_titles);
        local.album_title = Some("Self Titled".to_string());
        local.album_artist = Some("Jimmy Eat World".to_string());
        let mut remote = remote_release(&remote_titles);
        remote.title = "Jimmy Eat World".to_string();
        remote.artists = vec!["Jimmy Eat World".to_string()];
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_title >= 0.96);
        assert!(!album_evaluation(&local, &remote, &[]).auto_accept);
    }

    #[test]
    fn a_remote_placeholder_matches_but_untitled_needs_every_track() {
        let (local, mut remote) = self_titled_pair("Goldfinger", "S/T");
        assert!((album_title_score(&local, &remote) - 0.97).abs() < 1e-9);
        // Spelled differently from the tag, a placeholder still reads as one;
        // spelled the same, the literal comparison is exact.
        remote.title = "Eponymous".to_string();
        assert!((album_title_score(&local, &remote) - 0.97).abs() < 1e-9);
        remote.title = "Self-Titled".to_string();
        assert!(album_title_score(&local, &remote) >= 0.97);
        assert!(album_evaluation(&local, &remote, &[]).auto_accept);

        for title in ["Untitled", "[untitled]"] {
            remote.title = title.to_string();
            assert!(
                (album_title_score(&local, &remote) - 0.80).abs() < 1e-9,
                "{title}"
            );
            let evaluation = album_evaluation(&local, &remote, &[]);
            assert!(evaluation.auto_accept, "{title}: {:?}", evaluation.blockers);
            assert!(
                evaluation
                    .warnings
                    .iter()
                    .any(|note| note.contains("album title")),
                "{:?}",
                evaluation.warnings
            );

            let mut drifting = local.clone();
            drifting.tracks[2].duration_secs =
                Some(drifting.tracks[2].duration_secs.unwrap() + 2.0);
            assert!(
                !album_evaluation(&drifting, &remote, &[]).auto_accept,
                "{title}"
            );
        }
    }

    #[test]
    fn a_real_album_called_same_matches_itself() {
        let (mut local, mut remote) = self_titled_pair("Goldfinger", "Same");
        local.album_title = Some("Same".to_string());
        assert_eq!(album_title_score(&local, &remote), 1.0);
        // Another artist's name is not what a placeholder reads as here.
        remote.title = "Other".to_string();
        assert!(album_title_score(&local, &remote) < 0.5);
    }

    #[test]
    fn a_numbered_discogs_credit_is_the_artist_name_for_a_self_titled_release() {
        let (local, mut remote) = self_titled_pair("Name", "Name");
        remote.artists = vec!["Name (3)".to_string()];
        assert!((album_title_score(&local, &remote) - 0.97).abs() < 1e-9);
        // The credit the title names must be the local artist's.
        let (local, mut remote) = self_titled_pair("Other Band", "Name");
        remote.artists = vec!["Name (3)".to_string()];
        assert!(album_title_score(&local, &remote) < 0.5);
        assert!(!album_evaluation(&local, &remote, &[]).auto_accept);
    }

    #[test]
    fn a_shared_credit_is_not_the_artist_the_title_names() {
        // `Self Titled` by Band B against a release titled `Band A`, credited
        // to both: the title names the other band.
        let (local, mut remote) = self_titled_pair("Band B", "Band A");
        remote.artists = vec!["Band A".to_string(), "Band B".to_string()];
        let score = album_title_score(&local, &remote);
        assert!(score < TITLE_SIMILARITY_THRESHOLD, "{score}");
        // The same release titled for the local band's own name is the album.
        remote.title = "Band B".to_string();
        assert!((album_title_score(&local, &remote) - 0.97).abs() < 1e-9);
    }

    #[test]
    fn no_artist_name_stands_in_for_a_various_or_missing_artist() {
        let (mut local, remote) = self_titled_pair("Goldfinger", "Goldfinger");
        local.artist_kind = ArtistKind::VariousArtists;
        assert!(album_title_score(&local, &remote) < 0.5);
        local.artist_kind = ArtistKind::Unknown;
        local.album_artist = None;
        assert!(album_title_score(&local, &remote) < 0.5);
    }

    #[test]
    fn junk_tags_are_not_album_evidence() {
        let (mut local, mut remote) = self_titled_pair("Goldfinger", "Unknown Title");
        local.album_title = Some("Unknown Title".to_string());
        assert_eq!(album_title_score(&local, &remote), 0.0);
        remote.title = "Goldfinger".to_string();
        assert_eq!(album_title_score(&local, &remote), 0.0);
        remote.title = "Unknown Album".to_string();
        local.album_title = Some("Unknown Album".to_string());
        assert_eq!(album_title_score(&local, &remote), 0.0);

        let (mut local, remote) = self_titled_pair("Goldfinger", "Goldfinger");
        local.album_title = Some("Goldfinger".to_string());
        assert_eq!(
            Candidate::new(&local, remote.clone()).score.album_artist,
            1.0
        );
        local.album_artist = Some("Unknown Artist".to_string());
        let mut remote = remote;
        remote.artists = vec!["Unknown Artist".to_string()];
        assert_eq!(Candidate::new(&local, remote).score.album_artist, 0.0);
    }

    #[test]
    fn untitled_stays_a_literal_title() {
        let (mut local, mut remote) = self_titled_pair("Goldfinger", "Untitled");
        local.album_title = Some("Untitled".to_string());
        assert_eq!(album_title_score(&local, &remote), 1.0);
        remote.title = "Goldfinger".to_string();
        assert!(album_title_score(&local, &remote) < 0.5);
    }

    #[test]
    fn a_stray_disc_style_position_does_not_make_a_single_cd_a_set() {
        // Discogs 549611 shape: plain positions 1..10 and a filler `10-68` entry.
        let titles: Vec<String> = (1..=10).map(|n| format!("Song {n}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let mut remote = remote_release(&titles);
        let mut filler = remote.tracklist[9].clone();
        filler.position = Some("10-68".to_string());
        filler.title = "(no audio)".to_string();
        filler.duration_secs = None;
        remote.tracklist.push(filler);
        assert_eq!(remote_media(&remote), None);

        let mut local = local_release(&titles);
        local.directory = "/music/Undertow/CD1".to_string();
        assert_eq!(local_disc_hint(&local).map(|hint| hint.number), Some(1));
        let mapping = map_tracks(&local, &remote);
        assert_eq!(mapping.medium, None);
        assert_eq!(mapping.assigned_count(), 10);
        assert_eq!(mapping.pairs[9], Some(9));
    }

    /// Discogs 21454897 shape: `1-1`..`1-14`, bare `15` and `16`, then `2-1`..`2-17`.
    fn precious_funk() -> RemoteRelease {
        let mut remote = remote_release(&[]);
        let positions: Vec<(u32, u32, String)> = (1..=14)
            .map(|k| (1, k, format!("1-{k}")))
            .chain((15..=16).map(|k| (1, k, k.to_string())))
            .chain((1..=17).map(|k| (2, k, format!("2-{k}"))))
            .collect();
        for (medium, k, position) in positions {
            let mut track = remote_release(&["Song"]).tracklist.remove(0);
            track.position = Some(position);
            track.title = format!("Song {medium}-{k}");
            track.duration_secs = Some(f64::from(200 + 10 * medium + k));
            remote.tracklist.push(track);
        }
        remote
    }

    #[test]
    fn a_bare_discogs_position_inherits_the_medium_before_it() {
        let remote = precious_funk();
        assert_eq!(
            remote_media(&remote),
            Some(BTreeMap::from([(1, 16), (2, 17)]))
        );
        assert_eq!(remote.medium_of(14), Some(1));
        assert_eq!(remote.medium_of(15), Some(1));
        assert_eq!(remote.medium_of(16), Some(2));
        assert_eq!(disc_label(&remote, 2), "disc 2 of 2");
        assert_eq!(remote.playable_tracks_in(Some(1)).len(), 16);
        assert_eq!(remote.playable_tracks_in(Some(2)).len(), 17);

        for (medium, count) in [(1, 16), (2, 17)] {
            let local = disc_folder(&format!("/music/Set/CD{medium}"), medium, count);
            let mapping = map_tracks(&local, &remote);
            assert_eq!(mapping.medium, Some(medium));
            assert!(mapping.complete, "{:?}", mapping.notes);
            assert!(!mapping.ambiguous, "{:?}", mapping.notes);
            assert_eq!(mapping.assigned_count(), count);
            for (_, remote_index) in mapping.assigned_pairs() {
                assert_eq!(remote.medium_of(remote_index), Some(medium));
            }
        }

        // Inheritance is for Discogs; a bare number on another provider stays
        // medium-less, and a fully explicit release is unchanged.
        let mut musicbrainz = precious_funk();
        musicbrainz.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        assert_eq!(remote_media(&musicbrainz), None);
        assert_eq!(
            remote_media(&disc_set(&[2, 2])),
            Some(BTreeMap::from([(1, 2), (2, 2)]))
        );
    }

    #[test]
    fn a_bare_position_before_any_explicit_one_stays_medium_less() {
        let mut remote = precious_funk();
        // The bare tracks now come first, as in the Undertow shape.
        remote.tracklist.swap(0, 14);
        assert_eq!(remote_media(&remote), None);
    }

    /// A text-only pair whose album titles are `local_title` and `remote_title`.
    fn titled_unlisted_pair(
        local_title: &str,
        remote_title: &str,
    ) -> (LocalRelease, RemoteRelease) {
        let (mut local, mut remote) = unlisted_pair(&TITLES_6);
        local.album_title = Some(local_title.to_string());
        remote.title = remote_title.to_string();
        (local, remote)
    }

    #[test]
    fn numbers_in_the_album_titles_must_agree_for_text_identity() {
        for (local, remote) in [
            ("Live in Berlin 1994", "Live in Berlin 1995"),
            ("Live at the BBC 1994", "Live at the BBC 1995"),
            ("Tour Edition 2", "Tour Edition 3"),
            ("Hits Vol. II", "Hits Vol. III"),
            ("Hits Part 1", "Hits Part 2"),
            ("Now Music 4", "Now Music 45"),
            ("Live 1994 Vol. 2", "Live 1994 Vol. 1"),
            ("Album 2CD Vol. 3", "Album Vol. 2"),
        ] {
            let (local, remote) = titled_unlisted_pair(local, remote);
            assert_blocked_without_text_identity(&local, &remote);
        }
        for (local, remote) in [
            ("Live in Berlin 1994", "Live in Berlin 1994"),
            ("Hits Vol. 2", "Hits Volume 2"),
            ("Fools Gold 12 Inch", "Fools Gold"),
            ("Fools Gold 7\"", "Fools Gold"),
            ("Fools Gold CD2", "Fools Gold"),
            ("Test Album", "Test Album"),
            // A number on one side only is no contradiction.
            ("Test Album (1999)", "Test Album"),
            ("Test Album [2004]", "Test Album"),
            ("Test Album (2CD)", "Test Album"),
            ("Test Album (Disc 1)", "Test Album"),
            ("Test Album II", "Test Album Vol. II"),
            ("Test Album Vol. 2 (1999)", "Test Album Vol. 2"),
            ("Test Hits 1 (1999)", "Test Hits 1"),
        ] {
            let (local, remote) = titled_unlisted_pair(local, remote);
            assert_accepted_on_text(&local, &remote, 6);
        }
    }

    #[test]
    fn title_numbers_contradict_only_when_each_title_has_a_number_the_other_lacks() {
        for (local, remote) in [
            ("Album Vol. 2 (1999)", "Album Vol. 2"),
            ("Hits 1 (1999)", "Hits 1"),
            ("Top 40 Hits Vol. 3 (2001)", "Top 40 Hits, Volume 3"),
            ("Album (Disc 1) 2004", "Album 2004"),
            ("Album (1999)", "Album"),
            ("Album", "Album 1999"),
        ] {
            assert!(album_numbers_agree(local, remote), "{local} / {remote}");
        }
        for (local, remote) in [
            ("Live in Berlin 1994", "Live in Berlin 1995"),
            ("Now Music 4", "Now Music 45"),
            ("Live 1994 Vol. 2", "Live 1994 Vol. 1"),
            ("Album 2CD Vol. 3", "Album Vol. 2"),
            ("Album 2 1999", "Album 1999 2"),
        ] {
            assert!(!album_numbers_agree(local, remote), "{local} / {remote}");
        }
    }

    #[test]
    fn title_numbers_read_digits_and_roman_numerals_after_a_volume_word() {
        assert_eq!(title_numbers("Live in Berlin 1994"), ["1994"]);
        assert_eq!(title_numbers("Album 02 Part 007"), ["2", "7"]);
        assert_eq!(title_numbers("Hits Vol. II"), ["2"]);
        assert_eq!(title_numbers("Hits Volume III"), ["3"]);
        assert_eq!(
            title_numbers("Hits Vol. 2"),
            title_numbers("Hits Volume II")
        );
        assert_eq!(title_numbers("Chapter IV: Part ix"), ["4", "9"]);
        // No space after the dot of the volume word.
        assert_eq!(title_numbers("Hits Vol.II"), ["2"]);
        assert_eq!(title_numbers("Hits Pt.II"), ["2"]);
        assert_eq!(title_numbers("Hits Vol.2"), ["2"]);
        // A number in front of a disc or format word is decoration.
        for title in [
            "Album (2CD)",
            "Album 2CD",
            "Album 2 CD",
            "Album [3 LP]",
            "Album 2 DVD",
            "Album 2cds",
        ] {
            assert!(title_numbers(title).is_empty(), "{title}");
        }
        assert_eq!(title_numbers("Album 2 CDs Live 1994"), ["1994"]);
        // A roman numeral counts only after a volume word.
        assert!(title_numbers("Mix I Love").is_empty());
        assert!(title_numbers("Part Civic").is_empty());
        // Decoration is not part of the title.
        assert!(title_numbers("Fools Gold 7\"").is_empty());
        assert!(title_numbers("Fools Gold 12 Inch").is_empty());
        assert!(title_numbers("Fools Gold CD2").is_empty());
        assert!(title_numbers("No Numbers Here").is_empty());
    }

    #[test]
    fn a_cut_off_local_title_may_lack_trailing_numbers() {
        let cut = "Live at the Hammersmith Odeon";
        assert_eq!(cut.chars().count(), 29);
        let (local, remote) = titled_unlisted_pair(cut, "Live at the Hammersmith Odeon 1994");
        assert_accepted_on_text(&local, &remote, 6);

        // A title that is not 29 or 30 characters is complete as written.
        let (local, remote) = titled_unlisted_pair(
            "Live at the Hammersmith Odeo",
            "Live at the Hammersmith Odeo 1994",
        );
        assert_blocked_without_text_identity(&local, &remote);

        // The numbers a cut-off title does have must still be the start of the
        // remote's sequence.
        let cut = "Live at Hammersmith Odeon 1994";
        assert_eq!(cut.chars().count(), 30);
        let (local, remote) = titled_unlisted_pair(cut, "Live at Hammersmith Odeon 1994 Vol 2");
        assert_accepted_on_text(&local, &remote, 6);
        let (local, remote) = titled_unlisted_pair(
            "Live at Hammersmith Odeon 1995",
            "Live at Hammersmith Odeon 1994 Vol 2",
        );
        assert_blocked_without_text_identity(&local, &remote);
    }

    #[test]
    fn a_local_bootleg_decoration_blocks_text_identity_only() {
        let (local, remote) = titled_unlisted_pair("Test Album (Bootleg)", "Test Album");
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_title >= 0.96);
        assert_blocked_without_text_identity(&local, &remote);
        let (local, remote) = titled_unlisted_pair("Test Album-BOOTLEG", "Test Album");
        assert_blocked_without_text_identity(&local, &remote);

        // With lengths the normal gates decide, as before.
        let mut local = local_release(&TITLES_6);
        local.album_title = Some("Test Album (Bootleg)".to_string());
        let remote = remote_release(&TITLES_6);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn a_junk_or_missing_local_title_is_no_edition_evidence() {
        let edition_blocked = |local_title: Option<&str>, remote_title: &str| {
            let mut local = local_release(&TITLES_6);
            local.album_title = local_title.map(str::to_string);
            let mut remote = remote_release(&TITLES_6);
            remote.title = remote_title.to_string();
            album_evaluation(&local, &remote, &[])
                .blockers
                .iter()
                .any(|note| note.contains("edition qualifiers differ"))
        };
        assert!(!edition_blocked(None, "Test Album (Remastered)"));
        assert!(!edition_blocked(
            Some("Unknown Album"),
            "Test Album (Remastered)"
        ));
        assert!(!edition_blocked(Some("Unknown Album"), "Test Album Live"));
        // A real title that names no qualifier still conflicts.
        assert!(edition_blocked(
            Some("Test Album"),
            "Test Album (Remastered)"
        ));
    }
}
