//! Pure matching logic: normalization, track assignment, scoring, and
//! automatic-acceptance gates.
//!
//! Nothing here touches the network or the database. Adapters convert their
//! service responses into [`RemoteRelease`] values, storage loads
//! [`LocalRelease`] values, and this module decides what may be applied.
//! Provider SDK types never reach this module.

use crate::providers::{GroupId, Mbid, ProviderKind, ReleaseId};
use crate::scanner::AUDIO_EXTENSIONS;
use crate::scene::{self, SceneSplit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const DISCOGS_ALGORITHM_VERSION: &str = "discogs-match-v43";
pub const MUSICBRAINZ_ALGORITHM_VERSION: &str = "musicbrainz-match-v42";
pub const DEEZER_ALGORITHM_VERSION: &str = "deezer-match-v1";

pub const SCORE_THRESHOLD: f64 = 0.95;
pub const RUNNER_UP_GAP: f64 = 0.05;
pub const TITLE_SIMILARITY_THRESHOLD: f64 = 0.90;
pub const DURATION_TOLERANCE_SECS: f64 = 3.0;

/// How much closer to the fallback than to the primary a file's length must be
/// for the files to settle a duration conflict between them (see
/// [`fallback_check`]): the file's gap to the primary must be at least its gap to
/// the fallback plus this margin. Without a margin a file that sits nearly
/// between the two lengths would side with whichever it is a hair nearer to.
/// Raising it settles fewer conflicts; it is independent of
/// [`CORROBORATED_DURATION_SECS`], which says when a length agrees at all.
pub const SETTLE_MARGIN_SECS: f64 = 1.5;

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

/// What the warning of an accept that reads a one-sided version label as a
/// labelling difference ends with (see [`one_sided_labels`]), so that the runner
/// can print it next to the unconfirmed-edition note.
pub const LABEL_DIFFERENCE_NOTE: &str = "read as a labelling difference";

/// The odd-length rule: when album title, artist and every track title agree in
/// track order and every length of an album of at least
/// `ODD_LENGTH_MIN_TRACKS` tracks agrees within [`DURATION_TOLERANCE_SECS`]
/// but exactly one (a hidden track, a vinyl typo, another mix), that track does
/// not block. The album score with the odd pair counted at the tolerance edge
/// (see [`odd_length_edge_score`]) must still reach [`SCORE_THRESHOLD`].
pub const ODD_LENGTH_MIN_TRACKS: usize = 5;

/// The fewest local tracks the odd-length rule accepts for a Various Artists
/// release. Its artist component is 1.0 by construction and track artists are
/// not compared, so the artist is no evidence and the track count has to make
/// up for it. The leading-tracks shape has a minimum of its own for the same
/// reason ([`LEADING_TRACKS_MIN_VARIOUS`]).
pub const ODD_LENGTH_MIN_VARIOUS_TRACKS: usize = 8;

/// What the warning of an accept under the odd-length rule ends with (see
/// [`one_length_differs`]), so that the runner can print it next to the
/// unconfirmed-edition note.
pub const ODD_LENGTH_NOTE: &str = "every title and every other length agrees";

/// The tracklist-identity rule: when exactly one of album title and artist
/// agrees (at least [`TITLE_SIMILARITY_THRESHOLD`]) and the other scores at
/// least `TRACKLIST_IDENTITY_TAG_FLOOR`, an album of at least
/// `TRACKLIST_IDENTITY_MIN_TRACKS` tracks whose every title and length agrees in
/// track order (every length within `TRACKLIST_IDENTITY_GAP_SECS`) is identified
/// by its tracklist. The score over the track titles and durations alone (see
/// [`ScoreBreakdown::tracklist_score`]) must reach `TRACKLIST_IDENTITY_SCORE`.
/// This is the fewest tracks when the album title is the tag that is off; see
/// `TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS` for the artist.
pub const TRACKLIST_IDENTITY_MIN_TRACKS: usize = 5;

/// The fewest local tracks the tracklist-identity rule accepts for a Various
/// Artists release, whose artist is no evidence (see
/// [`ODD_LENGTH_MIN_VARIOUS_TRACKS`]).
pub const TRACKLIST_IDENTITY_MIN_VARIOUS_TRACKS: usize = 8;

/// The fewest local tracks the tracklist-identity rule accepts when the artist
/// is the tag that is off. An artist below [`TITLE_SIMILARITY_THRESHOLD`] is the
/// weakest evidence any rule accepts: a tribute act such as `Brit Floyd` scores
/// 0.60 against `Pink Floyd`, so the tracklist alone must be longer to identify
/// the album than when only its title is off.
pub const TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS: usize = 8;

/// The lowest score the album title or the artist, whichever disagrees, may have
/// under the tracklist-identity rule: below it the tag names something else.
pub const TRACKLIST_IDENTITY_TAG_FLOOR: f64 = 0.50;
pub const TRACKLIST_IDENTITY_GAP_SECS: f64 = 1.5;
pub const TRACKLIST_IDENTITY_SCORE: f64 = 0.95;

/// What the warning of an accept under the tracklist-identity rule ends with
/// (see [`tracklist_identifies_album`]), so that the runner can print it next to
/// the unconfirmed-edition note.
pub const TRACKLIST_IDENTITY_NOTE: &str = "track titles and lengths agree in track order";

/// The leading-tracks rule: when the files are exactly the first tracks of a
/// release that lists more (a bonus track, a hidden track or a remix after the
/// album), in order, the release may be accepted at album scope. The files must
/// be at least `LEADING_TRACKS_MIN` tracks and at least
/// `LEADING_TRACKS_MIN_SHARE` of the release's playable tracks, so that a
/// single is no stand-in for an album and an album is no stand-in for a box set;
/// a Various Artists release needs [`LEADING_TRACKS_MIN_VARIOUS`] files.
/// The same shape is the only partial mapping that can be applied at all (see
/// [`mapping_can_apply`]). Every other condition is in [`leading_tracks_identify`].
pub const LEADING_TRACKS_MIN: usize = 5;
pub const LEADING_TRACKS_MIN_SHARE: f64 = 0.75;

/// The fewest files the leading-tracks shape takes for a Various Artists
/// release, whose artist is no evidence, so the track count has to make up for
/// it (see [`ODD_LENGTH_MIN_VARIOUS_TRACKS`]). It is part of the shape and not of
/// the identity rule, so that a smaller subset cannot be applied by a manual
/// decision either. Compilations are the releases most likely to share a few
/// tracks with another one.
pub const LEADING_TRACKS_MIN_VARIOUS: usize = 8;

/// What the warning of an accept under the leading-tracks rule ends with (see
/// [`leading_tracks_identify`]), so that the runner can print it next to the
/// unconfirmed-edition note.
pub const MORE_TRACKS_NOTE: &str = "lists more tracks after the files' last";

/// Warning endings the runner prints under an accept.
///
/// An ending marks a warning as the note of an acceptance rule, or of what the
/// directory name supplied (see [`Evaluation::rule_notes`]).
const RULE_NOTES: &[&str] = &[
    UNCONFIRMED_LENGTHS_NOTE,
    LABEL_DIFFERENCE_NOTE,
    ODD_LENGTH_NOTE,
    TRACKLIST_IDENTITY_NOTE,
    MORE_TRACKS_NOTE,
    DIRECTORY_NOTE,
];

/// The edition qualifiers that a release may carry on one side only without
/// being a different recording, when every length agrees: a provider's `Live`
/// edition note or a `Remastered` format that the files' tags never mention. The
/// other qualifiers never count: `instrumental`, `karaoke`, `acapella`, `mono`
/// and `remix` name audio that really differs, and `stereo` is already ignored
/// by [`qualifiers_agree`].
const RELEASE_LABEL_QUALIFIERS: &[&str] = &["acoustic", "deluxe", "demo", "live", "remastered"];

/// [`RELEASE_LABEL_QUALIFIERS`] for a single track title. Narrower, since
/// `deluxe` names an edition and a track's `remastered` never counts (see
/// [`album_track_qualifiers`]).
const TRACK_LABEL_QUALIFIERS: &[&str] = &["acoustic", "demo", "live"];

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
        ProviderKind::Deezer => DEEZER_ALGORITHM_VERSION,
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
    /// Whether matching may read the directory name as a second reading.
    ///
    /// The second reading is of the album title and artist, beyond filling a
    /// missing or junk tag. The runner turns it off for its tags-only first
    /// pass.
    pub reads_directory: bool,
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

/// MusicBrainz and Deezer credits are compared as written; numeric
/// disambiguation is a Discogs-only convention.
pub fn normalize_artist_for(provider: ProviderKind, input: &str) -> String {
    match provider {
        ProviderKind::Discogs => normalize_artist(input),
        ProviderKind::MusicBrainz | ProviderKind::Deezer => normalize_text(input),
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
    local_similarity_with(local, remote, true, true)
}

/// [`local_similarity`] with the cut-off and ID3v1-prefix readings switched on
/// or off. They only apply to text that is the raw tag: a title that was
/// shortened for comparison says nothing about how long the tag was.
/// `subtitle` switches on the forgiveness of a subtitle that one side has and
/// the other lacks ([`suffix_tolerant_similarity`]).
fn local_similarity_with(local: &str, remote: &str, allow_cut_off: bool, subtitle: bool) -> f64 {
    let mut full = similarity(local, remote);
    if subtitle && let Some(value) = suffix_tolerant_similarity(local, remote) {
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
/// [`CleanedTitle::cut_width`] allows it. `subtitle` is as for
/// [`local_similarity_with`].
fn cleaned_title_similarity(cleaned: &CleanedTitle, remote: &str, subtitle: bool) -> f64 {
    let mut value = local_similarity_with(&cleaned.text, remote, false, subtitle);
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
    track_title_similarity_with(raw, cleaned, remote, true)
}

/// [`track_title_similarity`] without the forgiveness of a subtitle that only
/// one side has: `I Sold Myself In` does not match `I Sold Myself In / [unknown]`
/// here. The raw, cut-off, ID3v1-prefix and cleaned readings are kept. For
/// deciding whether a file's title agrees with one provider and not another.
fn strict_track_title_similarity(raw: &str, cleaned: Option<&CleanedTitle>, remote: &str) -> f64 {
    track_title_similarity_with(raw, cleaned, remote, false)
}

fn track_title_similarity_with(
    raw: &str,
    cleaned: Option<&CleanedTitle>,
    remote: &str,
    subtitle: bool,
) -> f64 {
    let value = local_similarity_with(raw, remote, true, subtitle);
    cleaned.map_or(value, |cleaned| {
        value.max(cleaned_title_similarity(cleaned, remote, subtitle))
    })
}

/// Taken off an album-title comparison that needed a leading `The`, `O.S.T.`
/// marker or trailing release-type word dropped.
const ALBUM_TYPE_PENALTY: f64 = 0.03;

/// Taken off an album title or artist read from the directory name.
///
/// The same small cost as a dropped release-type word: the tags win a tie, so
/// a directory reading shows only where it does better, and it never scores as
/// an exact match. A whole-title reading (`1.0 - 0.03`) stays well above the
/// 0.90 title and artist gate. A reading that falls short of that gate counts
/// for nothing at all; see [`names_outright`].
const DIRECTORY_PENALTY: f64 = 0.03;

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
pub(crate) fn reduce_album_title(text: &str) -> Option<String> {
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
    self_titled_score_for(tag_identity(local).artist.as_deref(), remote)
}

/// [`self_titled_score`] for a placeholder whose artist is `artist`.
///
/// The artist is the tags' or the directory name's, whichever read the
/// placeholder.
fn self_titled_score_for(artist: Option<&str>, remote: &RemoteRelease) -> f64 {
    let remote_title = normalize_text(&remote.title);
    if is_self_titled(&remote.title) {
        return SELF_TITLED_REMOTE_SCORE;
    }
    let mut best = if remote_title == "untitled" {
        SELF_TITLED_UNTITLED_SCORE
    } else {
        0.0
    };
    if let Some(artist) = artist {
        best =
            best.max((album_title_similarity(artist, &remote.title) - ALBUM_TYPE_PENALTY).max(0.0));
        // The credit the title names must be the local artist: `Self Titled` by
        // `Band B` is not `Band A`, however many artists that release credits.
        let provider = remote.provider();
        if !remote_title.is_empty()
            && remote.artists.iter().any(|name| {
                normalize_artist_for(provider, name) == remote_title
                    && local_artist_similarity(
                        artist,
                        &match provider {
                            ProviderKind::Discogs => strip_disambiguator(name),
                            ProviderKind::MusicBrainz | ProviderKind::Deezer => name.clone(),
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
    let mut best = local_similarity_with(local, remote, allow_cut_off, true);
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
                    true,
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

/// The words that qualify a track title, and an edition as well: the audio of
/// a track or of a release differs when only one side carries one. Matched as
/// whole words, with no spellings folded.
const TRACK_QUALIFIERS: &[&str] = &[
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

/// The edition words that only a release carries: an album title, an edition
/// note or a format. As a track label `(a cappella)` or `(Karaoke)` is one of
/// many version words, and a provider names it as often as the files do, so it
/// is no track qualifier (see [`album_track_qualifiers`]).
const RELEASE_ONLY_QUALIFIERS: &[&str] = &["acapella", "karaoke"];

/// Spellings of a qualifier word that read as that word at release level, and
/// the word each one reads as.
const QUALIFIER_SPELLINGS: &[(&str, &str)] = &[
    ("accapella", "acapella"),
    ("accapellas", "acapella"),
    ("acapela", "acapella"),
    ("acapelas", "acapella"),
    ("acapellas", "acapella"),
    ("acappella", "acapella"),
    ("acappellas", "acapella"),
    ("instrumentals", "instrumental"),
];

/// The words after `a` that spell `a cappella` as two words (`a-cappella`
/// reads as two as well). A bare `cappella` is a word, not the edition, so it
/// needs the `a` before it.
const A_CAPPELLA_SECOND_WORDS: &[&str] = &[
    "capella",
    "capellas",
    "cappella",
    "cappellas",
    "capela",
    "capelas",
];

/// Edition words that change what a release is. They are meaningful, so a
/// mismatch is a contradiction rather than noise. The audio-only editions
/// (`instrumental`, `karaoke`, `a cappella`) are among them, in their plural
/// and alternative spellings (`Instrumentals`, `A Capella`, `Acapela`,
/// `Accapella`, `A-Cappella`), so a provider title `Album (Instrumentals)`
/// never agrees with a silent local title.
pub fn edition_qualifiers(text: &str) -> BTreeSet<&'static str> {
    let normalized = normalize_text(text);
    let mut words: Vec<&str> = Vec::new();
    for word in normalized.split_whitespace() {
        let word = if words.last() == Some(&"a") && A_CAPPELLA_SECOND_WORDS.contains(&word) {
            "acapella"
        } else {
            QUALIFIER_SPELLINGS
                .iter()
                .find_map(|(spelling, canonical)| (*spelling == word).then_some(*canonical))
                .unwrap_or(word)
        };
        words.push(word);
    }
    TRACK_QUALIFIERS
        .iter()
        .chain(RELEASE_ONLY_QUALIFIERS)
        .copied()
        .filter(|qualifier| words.contains(qualifier))
        .collect()
}

/// The fewest letters of a qualifier word that a cut-off album title may end
/// in to read as that word. Shorter starts are the start of too many names.
const CUT_QUALIFIER_MIN_CHARS: usize = 4;

/// [`edition_qualifiers`] for the album title of a local file, given the
/// qualifiers `remote` that the provider's release names.
///
/// A title of exactly [`ID3V1_FIELD_CHARS`] characters may have been cut to an
/// ID3v1 field, which can land inside the last word: `All Saints Collected
/// Instrumen`, `Music for Earthworms (Instrume`. When the title ends in a letter
/// and its last word is the start, of at least [`CUT_QUALIFIER_MIN_CHARS`]
/// letters, of a qualifier word or one of its spellings, the title may carry
/// that qualifier. The reading is a guess, so it counts only when `remote`
/// names the qualifier it completes to: a cut-off word can confirm a
/// provider's qualifier but never contradict one (`Schoolboy's In Disgrace
/// (Remas` agrees with a release that does not say remastered). Only the last
/// word reads this way, and only a title of that width; the remote side is
/// never cut. A value of one character fewer is cut before a space (see
/// [`cut_off_similarity`]), so its last word is whole.
fn local_edition_qualifiers(
    title: &str,
    remote: &BTreeSet<&'static str>,
) -> BTreeSet<&'static str> {
    let mut qualifiers = edition_qualifiers(title);
    if title.chars().count() != ID3V1_FIELD_CHARS
        || !title.chars().next_back().is_some_and(char::is_alphanumeric)
    {
        return qualifiers;
    }
    let normalized = normalize_text(title);
    let Some(last) = normalized.split_whitespace().next_back() else {
        return qualifiers;
    };
    if last.len() < CUT_QUALIFIER_MIN_CHARS || !last.is_ascii() {
        return qualifiers;
    }
    let words = TRACK_QUALIFIERS
        .iter()
        .chain(RELEASE_ONLY_QUALIFIERS)
        .map(|word| (*word, *word))
        .chain(QUALIFIER_SPELLINGS.iter().copied());
    for (word, qualifier) in words {
        // A word that is whole is already read by `edition_qualifiers`.
        if word.len() > last.len() && word.starts_with(last) {
            qualifiers.extend(
                edition_qualifiers(qualifier)
                    .into_iter()
                    .filter(|read| remote.contains(read)),
            );
        }
    }
    qualifiers
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

/// Whether two qualifier sets differ only by version labels: after the stereo
/// rule of [`qualifiers_agree`], one set is a proper subset of the other (an
/// empty set counts) and every word that tells them apart is in `labels`.
/// `{}` and `{live}` pass, as do `{live}` and `{live, remastered}` for
/// [`RELEASE_LABEL_QUALIFIERS`]; `{demo}` and `{live}` do not, nor does `{}`
/// against `{mono}`.
fn one_sided_labels(
    a: &BTreeSet<&'static str>,
    b: &BTreeSet<&'static str>,
    labels: &[&str],
) -> bool {
    let (a, b) = (significant_qualifiers(a), significant_qualifiers(b));
    a != b
        && (a.is_subset(&b) || b.is_subset(&a))
        && a.symmetric_difference(&b).all(|word| labels.contains(word))
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
            value = value.max(cleaned_title_similarity(cleaned, remote_title, true));
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
        self.album_score_with_durations(self.durations)
    }

    /// [`ScoreBreakdown::album_score`] with `durations` in place of the stored
    /// duration component. Used where one track's length is set aside.
    pub fn album_score_with_durations(&self, durations: f64) -> f64 {
        (WEIGHT_ALBUM_TITLE * self.album_title
            + WEIGHT_ALBUM_ARTIST * self.album_artist
            + WEIGHT_TRACK_TITLES * self.track_titles
            + WEIGHT_DURATIONS * durations)
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

    /// The score over the track titles and the durations alone, scaled so that
    /// a perfect pair scores 1.0. Used where the tracklist identifies the album
    /// and neither the album title nor the artist carries evidence.
    pub fn tracklist_score(&self) -> f64 {
        (WEIGHT_TRACK_TITLES * self.track_titles + WEIGHT_DURATIONS * self.durations)
            / (WEIGHT_TRACK_TITLES + WEIGHT_DURATIONS)
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

/// The album title component read from the album tag alone.
fn tag_title_score(local: &LocalRelease, remote: &RemoteRelease, mapping: &Mapping) -> f64 {
    match &local.album_title {
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
    }
}

/// The directory title's similarity to the remote title, before its cost and cut.
///
/// `None` without a directory title. The title is never read as a cut-off tag,
/// because directory names are not shortened. A self-titled placeholder reads as
/// the artist's name, as it would in a tag; the artist is the directory's, else
/// the tags'.
fn directory_title_similarity(
    local: &LocalRelease,
    directory: &SceneSplit,
    remote: &RemoteRelease,
) -> Option<f64> {
    let title = directory.title.as_deref()?;
    let mut value = album_title_similarity_with(title, &remote.title, false);
    if is_self_titled(title) {
        let tag_artist = tag_identity(local).artist;
        let artist = directory.artist.as_deref().or(tag_artist.as_deref());
        value = value.max(self_titled_score_for(artist, remote));
    }
    Some(value)
}

/// The album title component read from the directory name, less the penalty.
///
/// Zero without a directory title, and zero unless the reading reaches
/// [`TITLE_SIMILARITY_THRESHOLD`]: it counts only when it names the album. See
/// [`DIRECTORY_PENALTY`] and [`directory_title_similarity`].
fn directory_title_score(
    local: &LocalRelease,
    directory: Option<&SceneSplit>,
    remote: &RemoteRelease,
) -> f64 {
    directory
        .and_then(|split| directory_title_similarity(local, split, remote))
        .map_or(0.0, names_outright)
}

/// A directory reading's similarity less [`DIRECTORY_PENALTY`], or zero when
/// that falls short of [`TITLE_SIMILARITY_THRESHOLD`].
///
/// A partial directory match must not move a score into the 0.50 to 0.90 bands
/// that other rules read, such as the tracklist identity floor and its weak
/// title band. The directory counts only when it names the album or artist
/// outright.
fn names_outright(similarity: f64) -> f64 {
    let value = (similarity - DIRECTORY_PENALTY).max(0.0);
    if value >= TITLE_SIMILARITY_THRESHOLD {
        value
    } else {
        0.0
    }
}

/// The artist component read from the directory name, less the penalty.
///
/// A Various Artists name scores nothing: that credit is no evidence of the
/// album, and the gates that allow for it read the tags' artist kind. Like the
/// title, the reading counts only when it names the artist outright; see
/// [`names_outright`].
fn directory_artist_score(directory: Option<&SceneSplit>, remote: &RemoteRelease) -> f64 {
    directory
        .and_then(|split| split.artist.as_deref())
        .map_or(0.0, |artist| {
            names_outright(artist_credit_score(artist, false, false, remote))
        })
}

/// Ends the warning for an album title or artist read from the directory name.
const DIRECTORY_NOTE: &str = "read from the directory name";

/// Warnings naming the album title or artist the directory name decided.
fn directory_notes(local: &LocalRelease, candidate: &Candidate) -> Vec<String> {
    let Some(directory) = scene::directory_reading(local) else {
        return Vec::new();
    };
    let remote = &candidate.release;
    let mut notes = Vec::new();
    if let Some(title) = &directory.title
        && directory_title_score(local, Some(&directory), remote)
            > tag_title_score(local, remote, &candidate.mapping) + 1e-9
    {
        notes.push(format!("album title '{title}' {DIRECTORY_NOTE}"));
    }
    if let Some(artist) = &directory.artist
        && directory_artist_score(Some(&directory), remote) > tag_artist_score(local, remote) + 1e-9
    {
        notes.push(format!("artist '{artist}' {DIRECTORY_NOTE}"));
    }
    notes
}

/// Deterministic ranking score in `0..=1`. Missing components contribute zero;
/// nothing is renormalized into false confidence.
///
/// The album title and artist take the better of the tags and the directory
/// name's reading.
pub fn score(local: &LocalRelease, remote: &RemoteRelease, mapping: &Mapping) -> ScoreBreakdown {
    // The tags and the directory name are two readings of the album; the
    // better one counts. No other component reads the directory.
    let directory = scene::directory_reading(local);
    let album_title = tag_title_score(local, remote, mapping).max(directory_title_score(
        local,
        directory.as_ref(),
        remote,
    ));
    let album_artist =
        tag_artist_score(local, remote).max(directory_artist_score(directory.as_ref(), remote));

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

/// The album artist component read from the artist tag alone.
fn tag_artist_score(local: &LocalRelease, remote: &RemoteRelease) -> f64 {
    let Some(artist) = &local.album_artist else {
        return 0.0;
    };
    let various = matches!(local.artist_kind, ArtistKind::VariousArtists);
    artist_credit_score(artist, various, true, remote)
}

/// How well `artist` matches the remote release's credits.
///
/// `various` says the local release is Various Artists, which a Various
/// Artists credit matches outright. `allow_cut_off` lets a 30-character or
/// ID3v1-length artist match the start of a longer credit; tags are cut off
/// that way, directory names are not.
fn artist_credit_score(
    artist: &str,
    various: bool,
    allow_cut_off: bool,
    remote: &RemoteRelease,
) -> f64 {
    if remote.artists.is_empty() || is_junk_artist(artist) {
        return 0.0;
    }

    let provider = remote.provider();
    // Names that normalize to nothing (such as `!!!`) fall back to their
    // original text, with the Discogs disambiguator removed as normalization
    // would have done.
    let comparable = |name: &str| match provider {
        ProviderKind::Discogs => strip_disambiguator(name),
        ProviderKind::MusicBrainz | ProviderKind::Deezer => name.to_string(),
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

    let various_credit = various
        && remote_chars.iter().any(|name| {
            let name: String = name.iter().collect();
            name == "various" || name == "various artists"
        });
    if various_credit {
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
        if allow_cut_off && let Some(prefix) = id3v1_prefix(artist, original) {
            let prefix_chars: Vec<char> = normalize_artist_for(provider, prefix).chars().collect();
            value = value.max(similarity_of_normalized(
                &local_original,
                &local_chars,
                prefix,
                &prefix_chars,
            ));
        }
        if allow_cut_off && let Some(cut_off) = cut_off_similarity(artist, original) {
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

/// What a local release is ranked as: every album title and artist it may go
/// by. Built once per release by [`rank_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankIdentity {
    /// A self-titled placeholder is two readings, the artist's name and the tag
    /// as written, since a provider may list the album under either. The
    /// directory name's title is one more. Junk and missing titles have none.
    titles: Vec<String>,
    /// The artist tag, then the directory name's artist when it differs. Junk
    /// has none.
    artists: Vec<String>,
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
    let mut artists: Vec<String> = local
        .album_artist
        .as_deref()
        .filter(|artist| !is_junk_artist(artist))
        .map(str::to_string)
        .into_iter()
        .collect();
    // The directory name's reading ranks too, so a hit only it names clears
    // the detailed-lookup floor.
    if let Some(directory) = scene::directory_reading(local) {
        if let Some(artist) = directory.credited_artist()
            && !artists.contains(&artist)
        {
            artists.push(artist);
        }
        if let Some(title) = directory.title
            && !titles.contains(&title)
        {
            titles.push(title);
        }
    }
    RankIdentity { titles, artists }
}

impl RankIdentity {
    /// The best [`rank_search_summary_with`] over the title and artist readings.
    pub(crate) fn rank(&self, local: &LocalRelease, summary: &SearchSummary) -> f64 {
        fn readings(values: &[String]) -> Vec<Option<&str>> {
            if values.is_empty() {
                vec![None]
            } else {
                values.iter().map(|value| Some(value.as_str())).collect()
            }
        }
        let artists = readings(&self.artists);
        readings(&self.titles)
            .into_iter()
            .flat_map(|title| artists.iter().map(move |artist| (title, *artist)))
            .map(|(title, artist)| rank_search_summary_with(local, title, artist, summary))
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
    /// a partial mapping other than the files being the first tracks of an album
    /// (see [`mapping_can_apply`]), nor a structurally unsupported one. `local`
    /// is the release the `candidate` was mapped from.
    pub fn can_apply(&self, local: &LocalRelease, candidate: &Candidate) -> bool {
        mapping_can_apply(local, candidate, self.match_scope)
    }
}

/// Whether `candidate`'s mapping may be written as an accepted match at `scope`,
/// by an automatic or a manual decision alike: the release has no unsupported
/// entries and the mapping is complete, or the scope is [`MatchScope::Album`]
/// and the files are the first tracks of the release ([`leading_tracks`]). No
/// other subset of a release can be applied, and an edition match, which
/// claims the release's own tracklist, never applies a partial mapping.
pub fn mapping_can_apply(local: &LocalRelease, candidate: &Candidate, scope: MatchScope) -> bool {
    !candidate.release.has_unsupported_entries()
        && (candidate.mapping.complete
            || (scope == MatchScope::Album && leading_tracks(local, candidate).is_some()))
}

/// What another candidate says about the acceptance rules for a candidate.
/// The default lets every rule apply.
#[derive(Debug, Clone)]
struct RuleGuards {
    /// The text-only rule ([`titles_identify_tracks`]) may apply.
    text_identity: bool,
    /// Why the odd-length rule ([`one_length_differs`]) must not apply, naming
    /// the alternative release. `None` lets the rule apply. The rule pushes the
    /// text as a blocker, and only when it would otherwise have applied.
    odd_length_blocker: Option<String>,
    /// Why the leading-tracks rule ([`leading_tracks_identify`]) must not apply,
    /// naming the alternative release. `None` lets the rule apply. The rule
    /// pushes the text as a blocker, and only when it would otherwise have
    /// applied.
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
    // Another album of the same provider that fits every length is the likelier
    // source of the files than this one with a length that does not fit.
    let odd_length_blocker = if local.tracks.len() >= ODD_LENGTH_MIN_TRACKS {
        alternatives
            .iter()
            .find(|other| {
                other.release.id != candidate.release.id
                    && other.release.provider() == candidate.release.provider()
                    && !in_same_release_group(candidate, other)
                    && fits_every_length(local, other)
            })
            .map(|other| {
                format!(
                    "release {} outside this release group fits every length",
                    other.release.id
                )
            })
    } else {
        None
    };
    // An edition that lists exactly the files is the likelier source of them
    // than one that lists more tracks after them.
    let leading_tracks_blocker = if leading_tracks(local, candidate).is_some() {
        alternatives
            .iter()
            .find(|other| lists_exactly_the_files(local, candidate, other))
            .map(|other| {
                format!(
                    "release {} lists exactly the local tracks",
                    other.release.id
                )
            })
    } else {
        None
    };
    let mut album = evaluate_with(
        local,
        candidate,
        None,
        discovery,
        MatchScope::Album,
        RuleGuards {
            text_identity,
            odd_length_blocker,
            leading_tracks_blocker,
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
///
/// Only [`TRACK_QUALIFIERS`], as whole words: the release-only words (`karaoke`,
/// `a cappella`) and the plural and alternative spellings of
/// [`edition_qualifiers`] are not track qualifiers.
fn album_track_qualifiers(title: &str) -> BTreeSet<&'static str> {
    let mut qualifiers = track_qualifiers(title);
    qualifiers.remove("remastered");
    qualifiers
}

/// The [`TRACK_QUALIFIERS`] among the whole words of `text`.
fn track_qualifiers(text: &str) -> BTreeSet<&'static str> {
    let normalized = normalize_text(text);
    TRACK_QUALIFIERS
        .iter()
        .copied()
        .filter(|qualifier| normalized.split_whitespace().any(|word| word == *qualifier))
        .collect()
}

/// Words beyond [`VERSION_WORDS`] that name another version of a track, in the
/// spellings [`normalize_text`] leaves: `voice-over` and `voiceover`, `en vivo`,
/// `wordless`, `[inst.]`, karaoke, `a cappella`, and duets. `backing` is not
/// listed: backing vocals are a credit as often as a version.
const EXTRA_VERSION_WORDS: &[&str] = &[
    "acapella",
    "duet",
    "inst",
    "karaoke",
    "vivo",
    "voiceover",
    "wordless",
];

/// The version words that name an edition (a mastering, a channel layout, a
/// bonus package) and not another recording, so that a different mastering of
/// the same recording does not count as another version of it.
const EDITION_VERSION_WORDS: &[&str] = &["deluxe", "remaster", "remastered", "stereo"];

/// Whether `token`, which follows `previous` in a normalized title, spells `a
/// cappella`: one of the single-word spellings (`acapella`, `accapella`,
/// `acappella`, `acapela` and their plurals), or `cappella`, `capella` and
/// their plurals after an `a` (`a cappella`, `a-cappella`). A bare `cappella` is
/// a word, not the version, as for [`edition_qualifiers`].
fn spells_a_cappella(previous: &str, token: &str) -> bool {
    token == "acapella"
        || QUALIFIER_SPELLINGS
            .iter()
            .any(|(spelling, canonical)| *canonical == "acapella" && *spelling == token)
        || (previous == "a" && A_CAPPELLA_SECOND_WORDS.contains(&token))
}

/// The version words of a track title: the one vocabulary for what names another
/// version of a recording, whatever the spelling. These are the normalized
/// words that are in [`VERSION_WORDS`] or [`EXTRA_VERSION_WORDS`], with every
/// spelling of `a cappella` read as `acapella` ([`spells_a_cappella`]), `voice
/// over` as `voiceover` and any word containing `remaster` as `remaster`. The
/// edition words ([`EDITION_VERSION_WORDS`]) are included: see
/// [`title_version_words`] for the set without them. Language words are not
/// version words, so `(Spanish version)` and `(English Version)` both carry just
/// `version`.
fn version_words(text: &str) -> BTreeSet<&'static str> {
    let normalized = normalize_text(text);
    let mut words = BTreeSet::new();
    let mut tokens = normalized.split_whitespace().peekable();
    let mut previous = "";
    while let Some(token) = tokens.next() {
        let word = if token == "voice" && tokens.peek() == Some(&"over") {
            tokens.next();
            Some("voiceover")
        } else if spells_a_cappella(previous, token) {
            Some("acapella")
        } else if token.contains("remaster") {
            Some("remaster")
        } else {
            VERSION_WORDS
                .iter()
                .chain(EXTRA_VERSION_WORDS)
                .copied()
                .find(|word| *word == token)
        };
        previous = token;
        words.extend(word);
    }
    words
}

/// The words of a track title that name a version of the recording
/// ([`version_words`]), except the edition words ([`EDITION_VERSION_WORDS`]),
/// which a different mastering of the same recording carries.
fn title_version_words(title: &str) -> BTreeSet<&'static str> {
    let mut words = version_words(title);
    words.retain(|word| !EDITION_VERSION_WORDS.contains(word));
    words
}

/// A duration that is known: finite and positive. A missing, zero, negative or
/// non-finite length says nothing about a track.
fn known_length(duration: Option<f64>) -> Option<f64> {
    duration.filter(|secs| secs.is_finite() && *secs > 0.0)
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

/// Whether `candidate` maps every local track and every mapped length is known
/// and within [`DURATION_TOLERANCE_SECS`]: the mapping is complete and
/// unambiguous and no length is missing or off. Says nothing about titles.
fn fits_every_length(local: &LocalRelease, candidate: &Candidate) -> bool {
    candidate.mapping.complete
        && !candidate.mapping.ambiguous
        && candidate.mapping.assigned_count() > 0
        && candidate.mapping.assigned_pairs().all(|(l, r)| {
            duration_gap(local, candidate, l, r).is_some_and(|gap| gap <= DURATION_TOLERANCE_SECS)
        })
}

/// Whether a few tracks drift from the provider's durations while the album
/// still agrees as a whole (see [`SOFT_DURATION_MAX_TRACKS`]): at least
/// [`SOFT_DURATION_MIN_ALBUM_TRACKS`] local tracks, at most
/// [`SOFT_DURATION_MAX_TRACKS`] of them beyond [`DURATION_TOLERANCE_SECS`], the
/// worst within [`SOFT_DURATION_MAX_SECS`], and the summed lengths within
/// [`SOFT_DURATION_TOTAL_RATIO`] of the provider's. `worst` is the largest
/// difference and the totals are over the pairs with both lengths known.
fn is_length_drift(
    local_tracks: usize,
    over_tolerance: usize,
    worst: f64,
    local_total: f64,
    remote_total: f64,
) -> bool {
    local_tracks >= SOFT_DURATION_MIN_ALBUM_TRACKS
        && over_tolerance <= SOFT_DURATION_MAX_TRACKS
        && worst <= SOFT_DURATION_MAX_SECS
        && remote_total > 0.0
        && (local_total - remote_total).abs() / remote_total <= SOFT_DURATION_TOTAL_RATIO
}

/// Whether a title names a version of a song: it carries any of the
/// [`version_words`] (the edition words, `remaster*` included). A title that
/// does is another recording as easily as the same one, so its length is no
/// odd one out.
fn names_a_version(title: &str) -> bool {
    !version_words(title).is_empty()
}

/// Whether a title names an alternate take, outtake, rehearsal or session in a
/// trailing bracketed group or after a dash or en dash ([`names_take_words`]). `cleaned`
/// is the cleaned reading of a local title, which drops a tag the raw reading
/// hides the group behind. Unlike [`local_title_is_another_take`] it does not
/// care whether the other side has a group too.
fn title_names_a_take(title: &str, cleaned: Option<&CleanedTitle>) -> bool {
    let names_take = |text: &str| {
        let text = text.trim_end();
        bracketed_suffix_start(text).is_some_and(|start| names_take_words(&text[start..]))
            || dash_tail(text).is_some_and(|(_, tail)| names_take_words(tail))
    };
    names_take(title) || cleaned.is_some_and(|cleaned| names_take(&cleaned.text))
}

/// The local index of the one track whose length differs, when `candidate` is
/// the album in every other respect: at least [`ODD_LENGTH_MIN_TRACKS`] local
/// tracks ([`ODD_LENGTH_MIN_VARIOUS_TRACKS`] for Various Artists, whose artist
/// is no evidence), album title and artist that [`album_text_agrees`], a
/// mapping that is [`mapped_in_local_order`], every title at least
/// [`TITLE_SIMILARITY_THRESHOLD`] ([`track_titles_agree`]) and preferring its
/// own track ([`titles_prefer_own_tracks`]), every local and remote length
/// known, finite and positive, and exactly one pair more than
/// [`DURATION_TOLERANCE_SECS`] apart.
///
/// On that pair the titles must agree strictly: at least
/// [`TITLE_SIMILARITY_THRESHOLD`] by [`strict_track_title_similarity`], so a
/// subtitle that only one side has is not forgiven. Neither the local nor the
/// remote title may name a version ([`names_a_version`]) or another take
/// ([`title_names_a_take`]), because a label or take on either side is another
/// recording as easily as the same one. A gap that the drift allowance already
/// covers ([`is_length_drift`]) is not an odd length: it keeps its own warning.
/// `cleaned` is [`cleaned_track_titles`] of `local`.
///
/// The rule also does not apply in two cases where the odd file may be another
/// recording that no label on the odd pair shows. Another playable track on any
/// medium may share the odd pair's remote base title, the title without its
/// trailing version or take group ([`same_song_ignoring_versions`]): the file may be that
/// sibling's recording (`Darts of Pleasure` against `Darts of Pleasure [home
/// Demo]`, where the file is the demo). And a local title of 29 or 30
/// characters ([`may_be_cut_to_field`]) that ends inside an unclosed `(` or `[`
/// group ([`ends_inside_open_group`]) was cut by an ID3v1 field, and the cut
/// group may have named a version or take that the rule cannot see.
///
/// One hidden track, a typo on a vinyl side or another mix does not change the
/// album that every title and every other length identify, but a length alone
/// cannot say which of those it is, so the rule leaves the call to the score
/// ([`odd_length_edge_score`]) and to the alternatives (see
/// [`RuleGuards::odd_length_blocker`]).
fn one_length_differs(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> Option<usize> {
    let min_tracks = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        ODD_LENGTH_MIN_VARIOUS_TRACKS
    } else {
        ODD_LENGTH_MIN_TRACKS
    };
    if local.tracks.len() < min_tracks
        || !album_text_agrees(candidate)
        || !mapped_in_local_order(local, candidate)
        || !track_titles_agree(local, candidate, cleaned)
        || !titles_prefer_own_tracks(local, candidate, cleaned)
    {
        return None;
    }
    let mut odd: Option<(usize, usize, f64)> = None;
    let (mut local_total, mut remote_total) = (0.0f64, 0.0f64);
    for (l, r) in candidate.mapping.assigned_pairs() {
        let a = known_length(local.tracks.get(l)?.duration_secs)?;
        let b = known_length(candidate.release.tracklist.get(r)?.duration_secs)?;
        local_total += a;
        remote_total += b;
        let gap = (a - b).abs();
        if gap > DURATION_TOLERANCE_SECS && odd.replace((l, r, gap)).is_some() {
            return None;
        }
    }
    let (l, r, gap) = odd?;
    if is_length_drift(local.tracks.len(), 1, gap, local_total, remote_total) {
        return None;
    }
    let local_title = local.tracks.get(l)?.title.as_deref()?;
    let remote_title = &candidate.release.tracklist.get(r)?.title;
    let cleaned_title = cleaned.get(l).and_then(Option::as_ref);
    let strictly_the_same = strict_track_title_similarity(local_title, cleaned_title, remote_title)
        >= TITLE_SIMILARITY_THRESHOLD;
    let another_recording = names_a_version(local_title)
        || names_a_version(remote_title)
        || title_names_a_take(local_title, cleaned_title)
        || title_names_a_take(remote_title, None);
    let may_be_a_sibling_recording =
        candidate
            .release
            .playable_tracks()
            .into_iter()
            .any(|(index, track)| {
                index != r && same_song_ignoring_versions(&track.title, remote_title)
            });
    let cut_inside_a_group =
        may_be_cut_to_field(local_title) && ends_inside_open_group(local_title);
    (strictly_the_same && !another_recording && !may_be_a_sibling_recording && !cut_inside_a_group)
        .then_some(l)
}

/// Whether `title` ends inside a `(` or `[` group that it never closes, as a
/// title cut to an ID3v1 field does (`Desperate Mother With A Gun (B`).
fn ends_inside_open_group(title: &str) -> bool {
    [('(', ')'), ('[', ']')].into_iter().any(|(open, close)| {
        title
            .rfind(open)
            .is_some_and(|start| !title[start..].contains(close))
    })
}

/// The album score of an odd-length match (see [`one_length_differs`]), which
/// replaces the album score gate for that rule: the album score with the odd
/// pair's duration similarity replaced by the one the scorer gives a pair
/// exactly [`DURATION_TOLERANCE_SECS`] apart, the edge of what agrees. The odd
/// pair counts as a pair at the edge, not as a perfect one and not as one left
/// out, so a length that is hundreds of seconds off is no worse than one just
/// beyond the tolerance. `odd` is the local index `one_length_differs` returned.
fn odd_length_edge_score(local: &LocalRelease, candidate: &Candidate, odd: usize) -> f64 {
    let tracks = local.tracks.len();
    debug_assert!(
        tracks >= ODD_LENGTH_MIN_TRACKS,
        "the odd-length rule needs at least {ODD_LENGTH_MIN_TRACKS} tracks"
    );
    let odd_similarity = candidate
        .mapping
        .pairs
        .get(odd)
        .copied()
        .flatten()
        .and_then(|r| {
            let a = local.tracks.get(odd)?.duration_secs?;
            let b = candidate.release.tracklist.get(r)?.duration_secs?;
            Some(duration_similarity(a, b))
        })
        .unwrap_or(0.0);
    let edge_similarity = duration_similarity(0.0, DURATION_TOLERANCE_SECS);
    let durations = candidate.score.durations + (edge_similarity - odd_similarity) / tracks as f64;
    candidate.score.album_score_with_durations(durations)
}

/// Whether the tracklist alone identifies the album when exactly one of the
/// album title and the artist is off: the files carry real tags (a usable album
/// title, and a real artist or Various Artists, so that filler stays with the
/// rules for missing tags), one of the two scores below
/// [`TITLE_SIMILARITY_THRESHOLD`] and the other at least that, the one below
/// still at least [`TRACKLIST_IDENTITY_TAG_FLOOR`], and the album titles
/// [`album_titles_identify`] (numbers must not contradict, no bootleg).
///
/// The tracks must then agree in every respect: at least
/// [`TRACKLIST_IDENTITY_MIN_TRACKS`] local tracks
/// ([`TRACKLIST_IDENTITY_MIN_VARIOUS_TRACKS`] for Various Artists, whose artist
/// is no evidence), a mapping that is [`mapped_in_local_order`], every title at
/// least [`TITLE_SIMILARITY_THRESHOLD`] ([`track_titles_agree`], with no near
/// miss and no lone-track exception) and also strictly so
/// ([`strict_track_title_similarity`], so that a subtitle only one side has
/// never helps), each title preferring its own track
/// ([`titles_prefer_own_tracks`]), no track version qualifier that differs on
/// any pair, a known positive length on both sides of every pair with each gap
/// within [`TRACKLIST_IDENTITY_GAP_SECS`], and a
/// [`ScoreBreakdown::tracklist_score`] of at least
/// [`TRACKLIST_IDENTITY_SCORE`], which replaces the album score gate.
///
/// Further conditions keep it no looser than the rules it sits beside: the
/// candidate is not unofficial ([`is_unofficial`]); an artist that is off needs
/// [`TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS`] tracks; an album title off at
/// [`CORROBORATED_TITLE_FLOOR`] or above is never read by this rule, because
/// that band belongs to the weak album title rule alone ([`corroborated_album`]);
/// and the two album titles carry the same [`title_version_words`], so `Album
/// (Radio Edit)` is no reading of `Album`.
///
/// A tracklist of this size that agrees in order and in every length is rarely
/// shared by two albums, so the tags that disagree (`Microphones, The`, a joint
/// credit, `OST` for `The Soundtrack`) do not decide the album. The caller
/// leaves the weak album title rule its precedence. `cleaned` is
/// [`cleaned_track_titles`] of `local`.
fn tracklist_identifies_album(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> bool {
    let score = &candidate.score;
    let various = matches!(local.artist_kind, ArtistKind::VariousArtists);
    if lacks_album_title(local)
        || !(various || tag_identity(local).artist.is_some())
        || is_unofficial(&candidate.release)
    {
        return false;
    }
    let agrees = |value: f64| value >= TITLE_SIMILARITY_THRESHOLD;
    let off = score.album_title.min(score.album_artist);
    if agrees(score.album_title) == agrees(score.album_artist) || off < TRACKLIST_IDENTITY_TAG_FLOOR
    {
        return false;
    }
    let artist_is_off = !agrees(score.album_artist);
    let min_tracks = if various {
        TRACKLIST_IDENTITY_MIN_VARIOUS_TRACKS
    } else if artist_is_off {
        TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS
    } else {
        TRACKLIST_IDENTITY_MIN_TRACKS
    };
    if local.tracks.len() < min_tracks {
        return false;
    }
    // An album title in the weak album title range belongs to that rule alone:
    // its gates are not the ones of this rule (it asks for the score without the
    // title, this one for the tracklist score), so this rule must not read it.
    if !artist_is_off && score.album_title >= CORROBORATED_TITLE_FLOOR {
        return false;
    }
    let local_title = local.album_title.as_deref().unwrap_or_default();
    if title_version_words(local_title) != title_version_words(&candidate.release.title)
        || score.tracklist_score() + 1e-9 < TRACKLIST_IDENTITY_SCORE
        || !album_titles_identify(local, candidate)
        || !mapped_in_local_order(local, candidate)
        || !track_titles_agree(local, candidate, cleaned)
        || !titles_prefer_own_tracks(local, candidate, cleaned)
    {
        return false;
    }
    mapped_pairs_agree_strictly(local, candidate, cleaned, TRACKLIST_IDENTITY_GAP_SECS)
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

/// How many of a release's playable tracks the files are, when they are its
/// first tracks (see [`leading_tracks`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeadingTracks {
    /// The local tracks, all of them mapped to the release's first tracks.
    pub present: usize,
    /// The playable tracks the release lists.
    pub listed: usize,
}

/// Whether the files are exactly the first tracks of a release that lists more:
/// the mapping is not onto one disc, the files name no disc ([`local_disc_hint`]),
/// the mapping is incomplete but unambiguous and the release has no unsupported
/// entries, the release lists more playable tracks than there are files, local
/// track `k` is mapped to playable track `k` for every file
/// ([`maps_onto_leading`]), and the files are at least [`LEADING_TRACKS_MIN`]
/// tracks ([`LEADING_TRACKS_MIN_VARIOUS`] for a Various Artists release) and at
/// least [`LEADING_TRACKS_MIN_SHARE`] of the playable tracks.
///
/// This is the only shape of a partial mapping that can be applied (see
/// [`mapping_can_apply`]). A release of several media is mapped as a whole, so
/// the files may open its first disc and the remote lists the rest after them.
/// Says nothing about the titles or the lengths: see [`leading_tracks_identify`].
pub fn leading_tracks(local: &LocalRelease, candidate: &Candidate) -> Option<LeadingTracks> {
    let mapping = &candidate.mapping;
    if mapping.medium.is_some()
        || mapping.complete
        || mapping.ambiguous
        || candidate.release.has_unsupported_entries()
        || local_disc_hint(local).is_some()
    {
        return None;
    }
    let playable = candidate.release.playable_tracks();
    let (present, listed) = (local.tracks.len(), playable.len());
    let min_present = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        LEADING_TRACKS_MIN_VARIOUS
    } else {
        LEADING_TRACKS_MIN
    };
    if present >= listed
        || present < min_present
        || (present as f64) < LEADING_TRACKS_MIN_SHARE * listed as f64
        || !maps_onto_leading(local, mapping, &playable)
    {
        return None;
    }
    Some(LeadingTracks { present, listed })
}

/// The words that name a short release: the decoration of an album title or a
/// word of a format that says the release is an EP or a single.
const SHORT_RELEASE_WORDS: &[&str] = &["cdep", "cds", "cdm", "ep", "maxi", "mcd", "single", "vls"];

/// Whether `word` (lowercase letters and digits) names a short release, with or
/// without a disc number (`cds2`).
fn is_short_release_word(word: &str) -> bool {
    SHORT_RELEASE_WORDS.contains(&word.trim_end_matches(|ch: char| ch.is_ascii_digit()))
}

/// Whether the decoration at the end of an album title names a short release
/// ([`SHORT_RELEASE_WORDS`]): `Hello EP`, `Hello-CD Single`, `Clocks-RETAIL CDS`.
fn decoration_names_short_release(title: &str) -> bool {
    decoration_run(title).is_some_and(|run| {
        run.units.first().is_some_and(|first| {
            decoration_tokens(&title[first.start..])
                .iter()
                .any(|(_, token)| is_short_release_word(&alphanumeric_lowercase(token)))
        })
    })
}

/// Whether the remote title's decoration or any word of its formats names a
/// short release (`CD (Single, EP)`, `Vinyl (12", Maxi-Single)`). The phrase
/// `Single Sided` (or `Single-sided`) says how a disc is printed and names no
/// release, so its `single` does not count.
fn remote_names_short_release(remote: &RemoteRelease) -> bool {
    decoration_names_short_release(&remote.title)
        || remote.formats.iter().any(|format| {
            let words: Vec<String> = format
                .split(|ch: char| !ch.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect();
            words.iter().enumerate().any(|(index, word)| {
                is_short_release_word(word)
                    && !(word == "single"
                        && words.get(index + 1).is_some_and(|next| next == "sided"))
            })
        })
}

/// Whether every mapped pair agrees strictly and in length: a local title, a
/// score of at least [`TITLE_SIMILARITY_THRESHOLD`] by
/// [`strict_track_title_similarity`] (a subtitle only one side has is not
/// forgiven), the same track version qualifiers on both sides, and a known
/// positive length on both sides within `max_gap_secs`. `cleaned` is
/// [`cleaned_track_titles`] of `local`.
fn mapped_pairs_agree_strictly(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
    max_gap_secs: f64,
) -> bool {
    candidate.mapping.assigned_pairs().all(|(l, r)| {
        let (Some(local_track), Some(remote_track)) =
            (local.tracks.get(l), candidate.release.tracklist.get(r))
        else {
            return false;
        };
        let Some(title) = local_track.title.as_deref() else {
            return false;
        };
        let strictly_the_same = strict_track_title_similarity(
            title,
            cleaned.get(l).and_then(Option::as_ref),
            &remote_track.title,
        ) >= TITLE_SIMILARITY_THRESHOLD;
        let qualifiers_match = qualifiers_agree(
            &album_track_qualifiers(title),
            &album_track_qualifiers(&remote_track.title),
        );
        let lengths_agree = matches!(
            (known_length(local_track.duration_secs), known_length(remote_track.duration_secs)),
            (Some(a), Some(b)) if (a - b).abs() <= max_gap_secs
        );
        strictly_the_same && qualifiers_match && lengths_agree
    })
}

/// The files' place in the release when they are its first tracks
/// ([`leading_tracks`]) and every other condition of the rule holds, or `None`.
///
/// The album title and artist must agree ([`album_text_agrees`], and the titles
/// [`album_titles_identify`]: numbers must not contradict, no bootleg) and the
/// release must not be unofficial ([`is_unofficial`]). Every mapped title must
/// agree by [`track_titles_agree`] and also strictly ([`strict_track_title_similarity`]), so
/// that a subtitle only one side has never helps, and each title must prefer its
/// own track over every playable track of the release, those after the files
/// included ([`titles_prefer_own_tracks`]). Every mapped length must be known on
/// both sides and within [`DURATION_TOLERANCE_SECS`], and no track version
/// qualifier may differ on any pair. The album score must reach
/// [`SCORE_THRESHOLD`] as usual. The files' count is part of the shape
/// ([`leading_tracks`]), which also holds a Various Artists release to
/// [`LEADING_TRACKS_MIN_VARIOUS`] files.
///
/// The release must also not be a longer name for a short one: when the album
/// title ends in a word that names an EP or a single ([`SHORT_RELEASE_WORDS`]),
/// the release's title or its formats must say so as well, because an EP's
/// tracks open many an album and its title is what tells them apart. The tracks
/// after the files are no evidence against the album, since the scores average
/// over the files, and nothing here says why they exist; the warning of the rule
/// says so. `cleaned` is [`cleaned_track_titles`] of `local`.
fn leading_tracks_identify(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> Option<LeadingTracks> {
    let leading = leading_tracks(local, candidate)?;
    let names_short_release = local
        .album_title
        .as_deref()
        .is_some_and(decoration_names_short_release);
    if is_unofficial(&candidate.release)
        || candidate.score.album_score() + 1e-9 < SCORE_THRESHOLD
        || !album_text_agrees(candidate)
        || !album_titles_identify(local, candidate)
        || (names_short_release && !remote_names_short_release(&candidate.release))
        || !track_titles_agree(local, candidate, cleaned)
        || !mapped_pairs_agree_strictly(local, candidate, cleaned, DURATION_TOLERANCE_SECS)
        || !titles_prefer_own_tracks(local, candidate, cleaned)
    {
        return None;
    }
    Some(leading)
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

/// The text after the last ` - ` or ` \u{2013} ` (en dash) separator of `title`,
/// with the byte offset of the separator.
fn dash_tail(title: &str) -> Option<(usize, &str)> {
    [" - ", " \u{2013} "]
        .iter()
        .filter_map(|separator| {
            title
                .rfind(separator)
                .map(|at| (at, &title[at + separator.len()..]))
        })
        .max_by_key(|(at, _)| *at)
}

/// A track title split into its base and the label it ends with, when
/// `names_label` says the label is one: a final `(...)` or `[...]` group, or a
/// ` - ` or en dash tail. A title whose base would be empty does not split.
fn split_label_tail(title: &str, names_label: impl Fn(&str) -> bool) -> Option<(&str, &str)> {
    let title = title.trim();
    let open = match title.chars().next_back() {
        Some(')') => Some('('),
        Some(']') => Some('['),
        _ => None,
    };
    let (cut, label) = if let Some(open) = open {
        let start = title.rfind(open)?;
        (start, &title[start + 1..title.len() - 1])
    } else {
        dash_tail(title)?
    };
    let base = title[..cut].trim_end();
    (!base.is_empty() && names_label(label)).then_some((base, label))
}

/// A track title split into its base and the version label it ends with: a
/// final `(...)` or `[...]` group, or a ` - ` or en dash tail, whose words
/// include an edition qualifier (see [`edition_qualifiers`]). `Clean Slate
/// (live)` gives `("Clean Slate", "live")`; `Live Wire` and `Live Forever` have
/// no group to lose, and `Song (Live) [Bonus Track]` ends with a group that
/// names no qualifier, so none of them splits. A title whose base would be empty
/// does not split either.
fn split_version_label(title: &str) -> Option<(&str, &str)> {
    split_label_tail(title, |label| !track_qualifiers(label).is_empty())
}

/// A track title without the trailing group or dash tail that carries any
/// version word ([`version_words`]) or take word ([`names_take_words`]), or the
/// trimmed title when it has none. The wider reading than
/// [`without_version_label`], for telling whether another track is the same
/// song under another version or take.
fn without_version_or_take_tail(title: &str) -> &str {
    split_label_tail(title, |label| {
        !version_words(label).is_empty() || names_take_words(label)
    })
    .map_or_else(|| title.trim(), |(base, _)| base)
}

/// A track title without the version label it ends with
/// ([`split_version_label`]), or the trimmed title when it has none.
fn without_version_label(title: &str) -> &str {
    split_version_label(title).map_or_else(|| title.trim(), |(base, _)| base)
}

/// Whether `a` and `b` are the same title once each loses its trailing version
/// label ([`without_version_label`]), at least [`TITLE_SIMILARITY_THRESHOLD`].
fn bases_agree(a: &str, b: &str) -> bool {
    track_title_similarity(without_version_label(a), None, without_version_label(b))
        >= TITLE_SIMILARITY_THRESHOLD
}

/// Whether `a` and `b` are the same song once each loses a trailing version or
/// take group ([`without_version_or_take_tail`]), at least
/// [`TITLE_SIMILARITY_THRESHOLD`].
fn same_song_ignoring_versions(a: &str, b: &str) -> bool {
    track_title_similarity(
        without_version_or_take_tail(a),
        None,
        without_version_or_take_tail(b),
    ) >= TITLE_SIMILARITY_THRESHOLD
}

/// The readings of a local title that [`without_version_label`] may strip: the
/// raw tag and, when there is one, the cleaned reading
/// ([`cleaned_track_titles`]), which has the extension and the track number off
/// (`05 - Clean Slate (Live).mp3`).
fn local_title_readings<'a>(title: &'a str, cleaned: Option<&'a CleanedTitle>) -> Vec<&'a str> {
    let mut readings = vec![title];
    readings.extend(cleaned.map(|cleaned| cleaned.text.as_str()));
    readings
}

/// A mapped pair whose titles differ only by a version label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LabelPair {
    local_index: usize,
    remote_index: usize,
    /// The title that carries the extra label, as the files or the provider
    /// spell it.
    labelled_title: String,
}

/// Whether `labelled` is `plain` plus a real version label: it ends in one
/// ([`split_version_label`]) that names no take, session or rehearsal, and its
/// base reads as `plain` once the label (if any) is off, without the
/// forgiveness of a subtitle that only one side has
/// ([`strict_track_title_similarity`]). `Song (Live)` is `Song` plus a label,
/// but neither `Song (Part 2)` nor `Song [Bonus Track]` is, and a title that
/// ends in a bare word (`... Many Words Live`) has no group to lose.
fn is_plain_plus_label(labelled: &str, plain: &str) -> bool {
    let Some((base, label)) = split_version_label(labelled) else {
        return false;
    };
    !names_take_words(label)
        && !split_version_label(plain).is_some_and(|(_, label)| names_take_words(label))
        && strict_track_title_similarity(base, None, without_version_label(plain))
            >= TITLE_SIMILARITY_THRESHOLD
}

/// The local tracks of a candidate that differ from their remote track only by a
/// version label, when every pair of the mapping is either that or agrees
/// outright; `None` when some pair is neither. `Some` of an empty list means
/// every pair agrees outright. `cleaned` is [`cleaned_track_titles`] of `local`.
///
/// A pair agrees outright when its title scores at least
/// [`TITLE_SIMILARITY_THRESHOLD`] and the track qualifiers agree. It is a label
/// pair when the track qualifiers differ by [`TRACK_LABEL_QUALIFIERS`] on one
/// side only ([`one_sided_labels`]), the side with the extra label really ends
/// in one ([`is_plain_plus_label`]), and no twin exists: no other local track,
/// and no other playable track of the mapped medium, has a title agreeing with
/// this pair's once its label is off. A twin would make the label the one thing
/// that tells two recordings apart.
fn label_only_pairs(
    local: &LocalRelease,
    candidate: &Candidate,
    cleaned: &[Option<CleanedTitle>],
) -> Option<Vec<LabelPair>> {
    let mapping = &candidate.mapping;
    let playable = candidate.release.playable_tracks_in(mapping.medium);
    let mut label_pairs = Vec::new();
    for (l, r) in mapping.assigned_pairs() {
        let local_track = local.tracks.get(l)?;
        let remote_track = candidate.release.tracklist.get(r)?;
        let title = local_track.title.as_deref()?;
        let local_qualifiers = album_track_qualifiers(title);
        let remote_qualifiers = album_track_qualifiers(&remote_track.title);
        let similarity = mapping.title_similarity(local, cleaned, l, r, remote_track)?;
        if similarity >= TITLE_SIMILARITY_THRESHOLD
            && qualifiers_agree(&local_qualifiers, &remote_qualifiers)
        {
            continue;
        }
        if !one_sided_labels(
            &local_qualifiers,
            &remote_qualifiers,
            TRACK_LABEL_QUALIFIERS,
        ) {
            return None;
        }
        let local_carries = significant_qualifiers(&local_qualifiers).len()
            > significant_qualifiers(&remote_qualifiers).len();
        let readings = local_title_readings(title, cleaned.get(l).and_then(Option::as_ref));
        let agreeing_reading = readings.iter().copied().find(|reading| {
            if local_carries {
                is_plain_plus_label(reading, &remote_track.title)
            } else {
                is_plain_plus_label(&remote_track.title, reading)
            }
        })?;
        let twin_among_local = local.tracks.iter().enumerate().any(|(other, track)| {
            other != l
                && track.title.as_deref().is_some_and(|other_title| {
                    local_title_readings(other_title, cleaned.get(other).and_then(Option::as_ref))
                        .iter()
                        .any(|reading| bases_agree(reading, agreeing_reading))
                })
        });
        let twin_among_remote = playable
            .iter()
            .any(|(other, track)| *other != r && bases_agree(&track.title, &remote_track.title));
        if twin_among_local || twin_among_remote {
            return None;
        }
        label_pairs.push(LabelPair {
            local_index: l,
            remote_index: r,
            labelled_title: if local_carries {
                title.to_string()
            } else {
                remote_track.title.clone()
            },
        });
    }
    Some(label_pairs)
}

/// Whether the labels of every level say one thing: the release's edition
/// qualifiers, alone and then with the track qualifiers of every label pair, on
/// each side, agree or differ by [`RELEASE_LABEL_QUALIFIERS`] on one side only. Demo tracks
/// against a release noted as live, or a live track against a remastered
/// release, are two different claims and never a single labelling difference.
fn labels_agree_across_levels(
    local: &LocalRelease,
    candidate: &Candidate,
    label_pairs: &[LabelPair],
) -> bool {
    let mut remote_set = remote_edition_qualifiers(&candidate.release);
    let mut local_set = match &local.album_title {
        Some(title) if tag_identity(local).title.is_some() => {
            local_edition_qualifiers(title, &remote_set)
        }
        _ => BTreeSet::new(),
    };
    let at_release_level = qualifiers_agree(&local_set, &remote_set)
        || one_sided_labels(&local_set, &remote_set, RELEASE_LABEL_QUALIFIERS);
    for pair in label_pairs {
        let local_title = local
            .tracks
            .get(pair.local_index)
            .and_then(|track| track.title.as_deref())
            .unwrap_or_default();
        local_set.extend(album_track_qualifiers(local_title));
        if let Some(remote_track) = candidate.release.tracklist.get(pair.remote_index) {
            remote_set.extend(album_track_qualifiers(&remote_track.title));
        }
    }
    at_release_level
        && (qualifiers_agree(&local_set, &remote_set)
            || one_sided_labels(&local_set, &remote_set, RELEASE_LABEL_QUALIFIERS))
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
    let local_title = local_title.trim_end();
    let Some(start) = bracketed_suffix_start(local_title) else {
        return false;
    };
    if bracketed_suffix_start(remote_title.trim_end()).is_some() {
        return false;
    }
    names_take_words(&local_title[start..])
}

/// Whether `text` (the inside or the whole of a bracketed group, or a dash
/// tail) names an alternate take, outtake, rehearsal or session, or `take`
/// followed by a number.
fn names_take_words(text: &str) -> bool {
    const NUMBER_WORDS: &[&str] = &[
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    let suffix = normalize_text(text);
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

/// Whether at least one of two candidates has no release group, so that the
/// provider says nothing about whether they are editions of one album.
fn either_lacks_group(a: &Candidate, b: &Candidate) -> bool {
    a.release.group_id.is_none() || b.release.group_id.is_none()
}

/// Whether `other` lists exactly the files where `first`, ranked ahead of it,
/// does not, and is no worse a reference for them than `first`.
///
/// It must have the shape of [`lists_exactly_the_files_shape`] and
/// [`worse_reference`] must find nothing against it: an exact edition that is
/// unofficial, video only, lists fewer lengths, fits the lengths worse or has
/// worse track titles than `first` does not displace it, so that a longer
/// edition that fits the files better is not set aside for a thin listing.
pub(crate) fn lists_exactly_the_files(
    local: &LocalRelease,
    first: &Candidate,
    other: &Candidate,
) -> bool {
    lists_exactly_the_files_shape(local, first, other)
        && worse_reference(local, first, other).is_none()
}

/// Whether `other` has the shape of an edition that lists exactly the files
/// where `first`, ranked ahead of it, does not, whatever its quality as a
/// reference (see [`worse_reference`]).
///
/// `other` must be a different release whose mapping is complete, unambiguous
/// and free of unsupported entries, and either an edition of `first`'s release
/// group or an album whose score is within [`RUNNER_UP_GAP`] of `first`'s. Scores
/// average over the local tracks, so an edition with bonus tracks ties with the
/// one that lists exactly the files, and only the second can be applied.
///
/// Another album is held to more than a near score, which says nothing about
/// which of the two the files come from. It must agree with the files in album
/// title and artist ([`album_text_agrees`]), so that a reading only the
/// tracklist supports does not displace the first candidate, and it never
/// displaces a first candidate whose leading tracks the files are
/// ([`maps_onto_leading`]), however few of its tracks they make up: that
/// candidate is then judged against the other album as a close alternative.
/// An edition of the same release group has neither restriction, and neither
/// does a release without a group that is the same audio under another name
/// ([`equivalent_audio`]): the provider just does not file it with `first`.
pub(crate) fn lists_exactly_the_files_shape(
    local: &LocalRelease,
    first: &Candidate,
    other: &Candidate,
) -> bool {
    other.release.id != first.release.id
        && other.mapping.complete
        && !other.mapping.ambiguous
        && !other.release.has_unsupported_entries()
        && (in_same_release_group(first, other)
            || (either_lacks_group(first, other) && equivalent_audio(first, other))
            || (within_gap(first.score.album_score() - other.score.album_score())
                && album_text_agrees(other)
                && !maps_onto_leading(local, &first.mapping, &first.release.playable_tracks())))
}

/// Whether every format of a release is a video format (`DVD`, `Blu-ray`,
/// `VHS`), and it names at least one. Unlike [`lists_video_format`] it applies
/// to both providers. A `DVD-Audio` disc is audio, not video.
fn is_video_only(remote: &RemoteRelease) -> bool {
    !remote.formats.is_empty()
        && remote.formats.iter().all(|format| {
            let lower = format.to_ascii_lowercase();
            !lower.contains("dvd-audio")
                && !lower.contains("dvd audio")
                && ["dvd", "blu-ray", "bluray", "vhs"]
                    .iter()
                    .any(|video| lower.contains(video))
        })
}

/// Why `other`, which lists exactly the files, is a worse reference for them
/// than `first`, or `None` when it is not: the first reason of [`worse_kind`],
/// then of [`worse_fit`].
pub(crate) fn worse_reference(
    local: &LocalRelease,
    first: &Candidate,
    other: &Candidate,
) -> Option<String> {
    worse_kind(first, other).or_else(|| worse_fit(local, first, other))
}

/// Why `other` is a worse kind of release than `first`: unofficial where
/// `first` is not, or video only where `first` is not. It says nothing about
/// how well either fits the files.
pub(crate) fn worse_kind(first: &Candidate, other: &Candidate) -> Option<String> {
    if is_unofficial(&other.release) && !is_unofficial(&first.release) {
        return Some("it is unofficial".to_string());
    }
    if is_video_only(&other.release) && !is_video_only(&first.release) {
        return Some("it is a video release".to_string());
    }
    None
}

/// Why `other` fits the files worse than `first`, which only matters when
/// `first` could itself be accepted through the first-tracks rule.
///
/// That is when the files are `first`'s first tracks, `first` is not video only
/// and passes the rule's own checks ([`leading_tracks_identify`]); otherwise
/// keeping `first` first would only send the release to review. Then `other` is
/// worse when it lists no length for a track whose length `first` lists, its
/// largest length gap to the files is more than [`DURATION_IDENTITY_GAP_SECS`]
/// worse than `first`'s, or its track-title score is more than [`RUNNER_UP_GAP`]
/// below `first`'s.
pub(crate) fn worse_fit(
    local: &LocalRelease,
    first: &Candidate,
    other: &Candidate,
) -> Option<String> {
    if is_video_only(&first.release)
        || leading_tracks_identify(local, first, &cleaned_track_titles(local)).is_none()
    {
        return None;
    }
    let mut missing_length = false;
    let mut max_gap_first: Option<f64> = None;
    let mut max_gap_other: Option<f64> = None;
    for local_index in 0..local.tracks.len() {
        let (Some(f), Some(e)) = (
            first.mapping.pairs.get(local_index).copied().flatten(),
            other.mapping.pairs.get(local_index).copied().flatten(),
        ) else {
            continue;
        };
        let first_length = first
            .release
            .tracklist
            .get(f)
            .and_then(|track| known_length(track.duration_secs));
        let other_length = other
            .release
            .tracklist
            .get(e)
            .and_then(|track| known_length(track.duration_secs));
        if first_length.is_some() && other_length.is_none() {
            missing_length = true;
        }
        let local_known = local
            .tracks
            .get(local_index)
            .and_then(|track| known_length(track.duration_secs))
            .is_some();
        if local_known
            && first_length.is_some()
            && other_length.is_some()
            && let (Some(gap_first), Some(gap_other)) = (
                duration_gap(local, first, local_index, f),
                duration_gap(local, other, local_index, e),
            )
        {
            max_gap_first = Some(max_gap_first.map_or(gap_first, |gap| gap.max(gap_first)));
            max_gap_other = Some(max_gap_other.map_or(gap_other, |gap| gap.max(gap_other)));
        }
    }
    if missing_length {
        return Some(format!(
            "it lists no length for a track whose length {} lists",
            first.release.id
        ));
    }
    if let (Some(gap_first), Some(gap_other)) = (max_gap_first, max_gap_other)
        && gap_other > gap_first + DURATION_IDENTITY_GAP_SECS + TIE_EPSILON
    {
        return Some(format!(
            "its lengths fit the files worse (up to {gap_other:.2}s against {gap_first:.2}s)"
        ));
    }
    if other.score.track_titles + RUNNER_UP_GAP < first.score.track_titles - TIE_EPSILON {
        return Some(format!(
            "its track titles fit the files worse ({:.2} against {:.2})",
            other.score.track_titles, first.score.track_titles
        ));
    }
    None
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

/// Whether `b` is the same audio as `a` under another edition's name: the same
/// group when both have one, the same title and artist, every mapped track with
/// the same title, artist and a length within [`DURATION_TOLERANCE_SECS`], and
/// no contradicting recording. Release qualifiers must agree, except that a
/// release label on one side only ([`one_sided_labels`] over
/// [`RELEASE_LABEL_QUALIFIERS`]) is allowed when there are at least
/// [`DURATION_IDENTITY_MIN_TRACKS`] pairs and every pair is within
/// [`DURATION_IDENTITY_GAP_SECS`]. A label in a track title never makes two
/// releases equivalent, since the titles must normalize equal.
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
        || a.mapping.pairs.len() != b.mapping.pairs.len()
    {
        return false;
    }
    let (a_qualifiers, b_qualifiers) = (
        remote_edition_qualifiers(&a.release),
        remote_edition_qualifiers(&b.release),
    );
    let labelled = if qualifiers_agree(&a_qualifiers, &b_qualifiers) {
        false
    } else if one_sided_labels(&a_qualifiers, &b_qualifiers, RELEASE_LABEL_QUALIFIERS)
        && a.mapping.assigned_count() >= DURATION_IDENTITY_MIN_TRACKS
    {
        true
    } else {
        return false;
    };
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
                && (!labelled
                    || matches!((x.duration_secs, y.duration_secs), (Some(x), Some(y))
                    if (x-y).abs() <= DURATION_IDENTITY_GAP_SECS))
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
/// [`tag_identity`]) and none in its directory name has no title to weigh, so
/// the title floor and the title condition do not apply to it; everything else
/// does. A title read from the directory name does weigh in, at its cost and
/// whole, as a tag would ([`weak_rule_title`]). The artist has to come from the
/// tags or the directory name: a junk or missing one scores zero and keeps the
/// release in review.
fn corroborated_album(local: &LocalRelease, candidate: &Candidate) -> bool {
    let score = &candidate.score;
    let min_tracks = if matches!(local.artist_kind, ArtistKind::VariousArtists) {
        CORROBORATED_MIN_VARIOUS_TRACKS
    } else {
        CORROBORATED_MIN_TRACKS
    };
    if local.tracks.len() < min_tracks
        || weak_rule_title(local, candidate).is_some_and(|title| {
            title < CORROBORATED_TITLE_FLOOR || title >= TITLE_SIMILARITY_THRESHOLD
        })
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

/// The album title score the weak album title rule weighs, or `None` when it
/// has no title to weigh.
///
/// A usable album tag weighs as [`ScoreBreakdown::album_title`]. Without one, a
/// title in the directory name weighs at its similarity less
/// [`DIRECTORY_PENALTY`], partial or not: [`names_outright`] zeroes it for every
/// other rule, but this one reads the band it zeroes, as it does for a tag.
fn weak_rule_title(local: &LocalRelease, candidate: &Candidate) -> Option<f64> {
    if !lacks_album_title(local) {
        return Some(candidate.score.album_title);
    }
    let directory = scene::directory_reading(local)?;
    directory_title_similarity(local, &directory, &candidate.release)
        .map(|similarity| (similarity - DIRECTORY_PENALTY).max(0.0))
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
        // No tag carries a Deezer album ID.
        ReleaseId::Deezer(_) => false,
    };

    let cleaned = cleaned_track_titles(local);

    // The files are the first tracks of a release that lists more, and every
    // title, length and tag agrees: the missing tracks after the files do not
    // block. Album scope only. It needs an incomplete mapping, which the rules
    // that follow rule out, since each needs the mapping in local order. Another
    // edition that lists exactly the files switches it off with a blocker.
    let leading = if match_scope == MatchScope::Album {
        leading_tracks_identify(local, candidate, &cleaned)
    } else {
        None
    };
    let leading_rule = leading.filter(|_| guards.leading_tracks_blocker.is_none());
    if !candidate.mapping.complete && leading_rule.is_none() {
        // The rule would have applied but for another edition that lists
        // exactly the files, which says why.
        if leading.is_some()
            && let Some(blocker) = &guards.leading_tracks_blocker
        {
            blockers.push(blocker.clone());
        }
        blockers.push("track mapping is incomplete".to_string());
    }
    if let Some(leading) = leading_rule {
        warnings.push(format!(
            "the files are the first {} of {} tracks; {} {MORE_TRACKS_NOTE}",
            leading.present,
            leading.listed,
            remote.provider()
        ));
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

    // Every title and every length but one agrees: the odd track does not block
    // when the score over the other lengths reaches the gate, which replaces the
    // album score gate. Album scope only. It needs a length above the tolerance,
    // which the duration-identity rule rules out, and every length known, which
    // the text-identity rule rules out, so the three never overlap; and every
    // title at least 0.90, which the weak album title rule rules out. Another
    // album that fits every length switches it off with a blocker.
    let odd_length = if match_scope == MatchScope::Album
        && !corroborated_title
        && !duration_identity
        && !text_identity
    {
        one_length_differs(local, candidate, &cleaned)
            .filter(|odd| odd_length_edge_score(local, candidate, *odd) + 1e-9 >= minimum_score)
    } else {
        None
    };
    let odd_length_rule = odd_length.filter(|_| guards.odd_length_blocker.is_none());

    // One of album title and artist is off, but the tracklist agrees in every
    // title and every length: the tracks identify the album. Album scope only,
    // and not together with the weak album title rule, which keeps precedence.
    // It needs exactly one of title and artist below the minimum, which the
    // duration-identity, text-identity and odd-length rules rule out.
    let tracklist_identity = match_scope == MatchScope::Album
        && !corroborated_title
        && tracklist_identifies_album(local, candidate, &cleaned);

    // A label on one side only is a labelling difference, not another recording,
    // when the lengths identify the tracks and every other title agrees. The
    // gate holds with an empty list when no track carries a label.
    let label_pairs = if duration_identity {
        label_only_pairs(local, candidate, &cleaned)
            .filter(|pairs| labels_agree_across_levels(local, candidate, pairs))
    } else {
        None
    };
    let label_gate = label_pairs.is_some();
    let label_pairs = label_pairs.unwrap_or_default();

    let acceptance_score = match_scope.score(&candidate.score);
    if !corroborated_title
        && !duration_identity
        && !text_identity
        && odd_length_rule.is_none()
        && !tracklist_identity
        && acceptance_score + 1e-9 < minimum_score
    {
        blockers.push(format!(
            "score {:.3} is below the {:.2} acceptance threshold",
            acceptance_score, minimum_score
        ));
    }

    if corroborated_title && weak_rule_title(local, candidate).is_none() {
        warnings.push(
            "local release has no usable album title, but every track and duration agrees"
                .to_string(),
        );
    } else if corroborated_title && lacks_album_title(local) {
        let title = weak_rule_title(local, candidate).unwrap_or_default();
        let name = scene::directory_reading(local)
            .and_then(|split| split.title)
            .unwrap_or_default();
        warnings.push(format!(
            "album title '{name}' {DIRECTORY_NOTE} scores {title:.3} but every track and duration agrees"
        ));
    } else if corroborated_title {
        warnings.push(format!(
            "album title scores {:.3} but every track and duration agrees",
            candidate.score.album_title
        ));
    } else if tracklist_identity {
        warnings.push(format!(
            "album title scores {:.3} and artist {:.3}, but all {} {TRACKLIST_IDENTITY_NOTE}",
            candidate.score.album_title,
            candidate.score.album_artist,
            local.tracks.len()
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

        let label_pair = label_pairs
            .iter()
            .any(|pair| pair.local_index == local_index);

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
            if !label_pair
                && !qualifiers_agree(
                    &album_track_qualifiers(local_track.title.as_deref().unwrap_or_default()),
                    &album_track_qualifiers(&remote_track.title),
                )
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

        if !label_pair
            && let Some(score) = candidate.mapping.title_similarity(
                local,
                &cleaned,
                local_index,
                remote_index,
                remote_track,
            )
        {
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

    if let Some(first) = label_pairs.first() {
        let count = label_pairs.len();
        let (noun, verb) = if count == 1 {
            ("title", "differs")
        } else {
            ("titles", "differ")
        };
        let first = &first.labelled_title;
        warnings.push(format!(
            "{count} track {noun} {verb} from {provider} only by a version label (first '{first}') and every duration agrees within {DURATION_IDENTITY_GAP_SECS}s in track order; {LABEL_DIFFERENCE_NOTE}"
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
    let lone_weak_title = match_scope == MatchScope::Album
        && !corroborated_title
        && !tracklist_identity
        && titles_below_minimum == 1;
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
            && is_length_drift(
                local.tracks.len(),
                tracks_over_tolerance,
                difference,
                local_total,
                remote_total,
            );
        if drifted_album {
            warnings.push(format!(
                "track '{worst_duration_track}' differs by {difference:.1}s from the {provider} duration but the album length agrees"
            ));
        } else if let Some(odd) = odd_length_rule {
            let longer = candidate
                .mapping
                .pairs
                .get(odd)
                .copied()
                .flatten()
                .and_then(|r| {
                    Some(
                        local.tracks.get(odd)?.duration_secs?
                            > remote.tracklist.get(r)?.duration_secs?,
                    )
                })
                .unwrap_or(false);
            let direction = if longer { "longer" } else { "shorter" };
            warnings.push(format!(
                "track '{worst_duration_track}' is {difference:.1}s {direction} in the files than the {provider} duration; {ODD_LENGTH_NOTE}"
            ));
        } else {
            // The rule would have applied but for another album that fits every
            // length, which says why.
            if odd_length.is_some()
                && let Some(blocker) = &guards.odd_length_blocker
            {
                blockers.push(blocker.clone());
            }
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
        ReleaseId::Deezer(_) => {}
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
        let remote_qualifiers = remote_edition_qualifiers(remote);
        let local_qualifiers = local_edition_qualifiers(local_title, &remote_qualifiers);
        if !qualifiers_agree(&local_qualifiers, &remote_qualifiers) {
            if label_gate
                && one_sided_labels(
                    &local_qualifiers,
                    &remote_qualifiers,
                    RELEASE_LABEL_QUALIFIERS,
                )
            {
                let agreement = if label_pairs.is_empty() {
                    format!(
                        "every title and every duration within {DURATION_IDENTITY_GAP_SECS}s agree in track order"
                    )
                } else {
                    format!(
                        "every duration within {DURATION_IDENTITY_GAP_SECS}s agrees in track order and the titles agree apart from their version labels"
                    )
                };
                warnings.push(format!(
                    "edition qualifiers differ (local {:?} versus {provider} {:?}) but {agreement}; {LABEL_DIFFERENCE_NOTE}",
                    local_qualifiers, remote_qualifiers
                ));
            } else {
                blockers.push(format!(
                    "edition qualifiers differ: local {:?} versus {provider} {:?}",
                    local_qualifiers, remote_qualifiers
                ));
            }
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
    warnings.extend(directory_notes(local, candidate));

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

/// The conflicts of [`fallback_check`] alone, for the tests that do not care
/// what the files settled.
#[cfg(test)]
fn fallback_conflicts(
    local: &LocalRelease,
    primary_candidates: &[Candidate],
    fallback: &Candidate,
    embedded_release_resolved: bool,
    text_only_fallback: bool,
) -> Vec<String> {
    fallback_check(
        local,
        primary_candidates,
        fallback,
        embedded_release_resolved,
        text_only_fallback,
    )
    .conflicts
}

/// What [`fallback_check`] found: the disagreements that stop an automatic
/// fallback accept, and the ones the files themselves settled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FallbackCheck {
    /// Disagreements between the fallback and the local edition or a plausible
    /// primary candidate. Any one of them sends the release to review.
    pub conflicts: Vec<String>,
    /// Track title and duration disagreements between a plausible primary and
    /// the fallback where the file agrees with the fallback and not with the
    /// primary. They do not stop the accept; the runner reports them.
    pub settled: Vec<String>,
}

/// Cross-provider checks applied before automatic fallback acceptance, with the
/// conflicts the files settle reported apart from the ones that remain.
///
/// Scores, runner-up gaps, and mappings stay within one provider. The check only
/// asks whether the fallback contradicts the local edition or a plausible primary
/// candidate: one whose mapping is complete or whose first tracks the files are
/// ([`leading_tracks`]), and whose plausibility score reaches
/// [`PLAUSIBLE_PRIMARY_SCORE`] or whose durations identify the tracks
/// ([`durations_identify_tracks`]), so junk local titles do not hide the right
/// primary. Against a primary that is plausible only through its durations,
/// edition qualifiers and durations are compared but titles are not. A fallback
/// accepted on its text alone (`text_only_fallback`, because its provider lists
/// no length for some tracks) is also checked against every primary whose text
/// agrees with the files: that primary's lengths must not contradict them.
///
/// A track title conflict on local track *k* is settled when the file's title
/// is present, reads at least [`TITLE_SIMILARITY_THRESHOLD`] against the
/// fallback's title and below it against the primary's
/// ([`strict_track_title_similarity`], so a subtitle only one side has counts
/// against the match), the file's length and the fallback's are both known,
/// positive and within [`DURATION_TOLERANCE_SECS`], the two titles carry the
/// same track version qualifiers, the primary's title does not name another
/// take ([`local_title_is_another_take`]), and the version words
/// ([`title_version_words`]) agree: the primary's are among the fallback's, and
/// every one the fallback has and the primary lacks is also in the file's title
/// (raw or cleaned reading), so a file that merely lacks a word only the
/// primary names settles nothing. A duration conflict is settled when
/// the file's length is within [`DURATION_TOLERANCE_SECS`] of the fallback's
/// and clearly closer to it than to the primary's (further than the tolerance
/// from the primary's, and the gap to the primary at least the gap to the
/// fallback plus [`SETTLE_MARGIN_SECS`]), and the file's title also reads at
/// least [`TITLE_SIMILARITY_THRESHOLD`] against the fallback's (so a settle
/// never rests on a track the fallback matched through a weak, junk or
/// near-miss title). Nothing is settled when the fallback's album title or
/// artist scores below [`TITLE_SIMILARITY_THRESHOLD`] ([`album_text_agrees`]).
/// A primary that is plausible only
/// through its durations has no title conflicts and no duration conflict that
/// can be settled. An edition qualifier line is settled only when the
/// qualifiers differ by a label on one side ([`one_sided_labels`]) and the
/// durations of both the primary and the fallback identify the files' tracks
/// ([`durations_identify_tracks`]); any other qualifier disagreement stays a
/// conflict. Contradicting primary lengths, embedded
/// evidence and an unresolved release ID are never settled. A settled line
/// that several primaries produce is reported once.
pub fn fallback_check(
    local: &LocalRelease,
    primary_candidates: &[Candidate],
    fallback: &Candidate,
    embedded_release_resolved: bool,
    text_only_fallback: bool,
) -> FallbackCheck {
    let mut conflicts = Vec::new();
    let mut settled = Vec::new();
    // The files only settle a conflict for a fallback whose album is the files'.
    let files_may_settle = album_text_agrees(fallback);
    let cleaned_titles = cleaned_track_titles(local);

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
        // Only a primary that maps the files completely guards the fallback:
        // one whose first tracks the files are, but that failed its own gates,
        // describes a longer edition, so its later tracks, merged hidden tracks
        // and edition notes are not the files.
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
            let line = format!(
                "primary candidates describe {:?} but the fallback describes {:?}",
                primary_qualifiers, fallback_qualifiers
            );
            // A label one provider omits is a labelling difference, not a
            // conflict, when both providers' lengths identify the files'
            // tracks. The files themselves do not decide qualifiers: a silent
            // local title is no evidence of "not live". A text-only fallback
            // never identifies by lengths.
            if !text_only_fallback
                && one_sided_labels(
                    &primary_qualifiers,
                    &fallback_qualifiers,
                    RELEASE_LABEL_QUALIFIERS,
                )
                && durations_identify_tracks(local, primary)
                && durations_identify_tracks(local, fallback)
            {
                settled.push(format!(
                    "{line}; both providers' lengths identify the files' tracks, so this is read as a labelling difference"
                ));
            } else {
                conflicts.push(line);
            }
        }
        for (local_index, remote_index) in primary.mapping.assigned_pairs() {
            let Some(primary_track) = primary.release.tracklist.get(remote_index) else {
                continue;
            };
            let local_track = local.tracks.get(local_index);
            let fallback_duration = fallback_durations.get(&local_index).copied();
            // The file's length when it is known, positive and within
            // [`DURATION_TOLERANCE_SECS`] of the fallback's.
            let file_length_agreeing_with_fallback = local_track
                .and_then(|track| track.duration_secs)
                .zip(fallback_duration)
                .filter(|(file, fallback)| {
                    file.is_finite()
                        && fallback.is_finite()
                        && *file > 0.0
                        && *fallback > 0.0
                        && (file - fallback).abs() <= DURATION_TOLERANCE_SECS
                })
                .map(|(file, _)| file);
            let file_title = local_track.and_then(|track| track.title.as_deref());
            let cleaned = cleaned_titles.get(local_index).and_then(Option::as_ref);
            // Whether the file's title reads like `remote_title`, without the
            // forgiveness of a subtitle that only one side has.
            let file_reads_like = |remote_title: &str| {
                file_title.is_some_and(|title| {
                    strict_track_title_similarity(title, cleaned, remote_title)
                        >= TITLE_SIMILARITY_THRESHOLD
                })
            };
            if plausible_by_score && let Some(fallback_title) = fallback_titles.get(&local_index) {
                let similarity = similarity(&primary_track.title, fallback_title);
                if similarity < TITLE_SIMILARITY_THRESHOLD {
                    let message = format!(
                        "primary and fallback track titles differ for local track {} ('{}' versus '{}')",
                        local_index + 1,
                        primary_track.title,
                        fallback_title
                    );
                    let file_reads_like_fallback = file_reads_like(fallback_title);
                    // The primary's title names another recording of the song
                    // (a different version qualifier, or another take), which
                    // the file's plain title does not rule out.
                    let primary_names_another_version = !qualifiers_agree(
                        &album_track_qualifiers(&primary_track.title),
                        &album_track_qualifiers(fallback_title),
                    ) || local_title_is_another_take(
                        &primary_track.title,
                        None,
                        fallback_title,
                    );
                    // A version word only the primary has is never settled by
                    // the file lacking it: rips rarely carry the word, so that
                    // is absence of evidence. A word only the fallback has is
                    // settled only when the file's title carries it too.
                    let primary_words = title_version_words(&primary_track.title);
                    let fallback_words = title_version_words(fallback_title);
                    let file_words: BTreeSet<&str> = file_title
                        .map(title_version_words)
                        .into_iter()
                        .chain(cleaned.map(|cleaned| title_version_words(&cleaned.text)))
                        .flatten()
                        .collect();
                    let versions_settle = primary_words.is_subset(&fallback_words)
                        && fallback_words
                            .difference(&primary_words)
                            .all(|word| file_words.contains(word));
                    match file_title {
                        Some(file_title)
                            if files_may_settle
                                && file_length_agreeing_with_fallback.is_some()
                                && file_reads_like_fallback
                                && versions_settle
                                && !primary_names_another_version
                                && !file_reads_like(&primary_track.title) =>
                        {
                            settled.push(format!(
                                "{message}; the file '{file_title}' agrees with the fallback"
                            ));
                        }
                        _ => conflicts.push(message),
                    }
                }
            }
            if let (Some(primary_duration), Some(fallback_duration)) =
                (primary_track.duration_secs, fallback_duration)
                && (primary_duration - fallback_duration).abs() > DURATION_TOLERANCE_SECS
            {
                // The file's title must also read like the fallback's, so a
                // settle never rests on a track the fallback itself matched
                // despite a weak, junk or near-miss title.
                let title_agrees_with_fallback = fallback_titles
                    .get(&local_index)
                    .is_some_and(|title| file_reads_like(title));
                match file_length_agreeing_with_fallback {
                    Some(file_duration)
                        if files_may_settle
                            && title_agrees_with_fallback
                            && (file_duration - primary_duration).abs()
                                > DURATION_TOLERANCE_SECS
                            && (file_duration - primary_duration).abs()
                                >= (file_duration - fallback_duration).abs()
                                    + SETTLE_MARGIN_SECS =>
                    {
                        settled.push(format!(
                            "primary and fallback durations differ for local track {} ({primary_duration:.1}s versus {fallback_duration:.1}s); the file's {file_duration:.1}s agrees with the fallback",
                            local_index + 1
                        ));
                    }
                    _ => conflicts.push(format!(
                        "primary and fallback durations differ for local track {}",
                        local_index + 1
                    )),
                }
            }
        }
    }

    // Several primaries can disagree with the fallback in the same words.
    let mut seen = HashSet::new();
    settled.retain(|line| seen.insert(line.clone()));

    FallbackCheck { conflicts, settled }
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
            reads_directory: true,
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
        let value = tag_artist_score(&local, &remote);
        assert!((0.96..1.0).contains(&value), "{value}");

        remote.artists = vec!["Alpha Bravo Charlie Delta Foxes Band".to_string()];
        assert!(tag_artist_score(&local, &remote) < 0.96);
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
        let value = tag_artist_score(&local, &remote);
        assert!((0.97..1.0).contains(&value), "{value}");

        local.album_artist = Some("Different Band".to_string());
        assert!(tag_artist_score(&local, &remote) < 0.6);
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
        assert!(!evaluation.can_apply(&local, &candidate));
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
        assert!(!incomplete_evaluation.can_apply(&local, &incomplete_candidate));
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
            "discogs-match-v43"
        );
        assert_eq!(
            algorithm_version(ProviderKind::MusicBrainz),
            "musicbrainz-match-v42"
        );
        assert_eq!(algorithm_version(ProviderKind::Deezer), "deezer-match-v1");
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

        // Edition qualifiers are still compared against such a primary. A
        // label on one side only is settled by both providers' lengths ...
        let mut live = primary.release.clone();
        live.edition_notes = vec!["Live".to_string()];
        let live = Candidate::new(&local, live);
        assert!(durations_identify_tracks(&local, &fallback));
        let check = fallback_check(&local, &[live], &fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{:?}", check.settled);
        assert!(
            check.settled[0].starts_with("primary candidates describe"),
            "{:?}",
            check.settled
        );

        // ... but `Mono`, and `{demo}` against `{live}`, still conflict.
        let mut mono = primary.release.clone();
        mono.edition_notes = vec!["Mono".to_string()];
        let mono = Candidate::new(&local, mono);
        let conflicts = fallback_conflicts(&local, &[mono], &fallback, true, false);
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

        let mut demo = primary.release.clone();
        demo.edition_notes = vec!["Demo".to_string()];
        let demo = Candidate::new(&local, demo);
        let mut live_fallback = fallback.release.clone();
        live_fallback.edition_notes = vec!["Live".to_string()];
        let live_fallback = Candidate::new(&local, live_fallback);
        let check = fallback_check(&local, &[demo], &live_fallback, true, false);
        assert!(check.settled.is_empty(), "{:?}", check.settled);
        assert!(
            check
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{:?}",
            check.conflicts
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

        // Both providers' lengths identify the files' tracks, so a label on one
        // side only is a labelling difference, not a provider conflict.
        let fallback = Candidate::new(&local, remote.clone());
        assert!(durations_identify_tracks(&local, &fallback));
        let check = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(
            check.settled,
            vec![
                "primary candidates describe {\"live\"} but the fallback describes {}; both providers' lengths identify the files' tracks, so this is read as a labelling difference"
                    .to_string()
            ]
        );

        // The same pair of editions with the label on the fallback's side.
        let mut live_fallback = remote.clone();
        live_fallback.edition_notes = vec!["Live".to_string()];
        let live_fallback = Candidate::new(&local, live_fallback);
        let plain_primary = Candidate::new(&local, {
            let mut release = remote.clone();
            release.id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
            release
        });
        let check = fallback_check(&local, &[plain_primary], &live_fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{:?}", check.settled);

        // Only a label settles: `Mono`, `Instrumental` and `{demo}` against
        // `{live}` stay conflicts although both providers' lengths identify the
        // tracks.
        let qualifier_conflict = |primary_notes: &str, fallback_notes: Option<&str>| {
            let mut primary_release = remote.clone();
            primary_release.id =
                ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
            primary_release.edition_notes = vec![primary_notes.to_string()];
            let mut fallback_release = remote.clone();
            fallback_release.edition_notes =
                fallback_notes.map(str::to_string).into_iter().collect();
            let primary = Candidate::new(&local, primary_release);
            let fallback = Candidate::new(&local, fallback_release);
            assert!(durations_identify_tracks(&local, &primary));
            assert!(durations_identify_tracks(&local, &fallback));
            let check = fallback_check(&local, &[primary], &fallback, true, false);
            assert!(check.settled.is_empty(), "{:?}", check.settled);
            check.conflicts
        };
        for (primary_notes, fallback_notes) in [
            ("Mono", None),
            ("Instrumental", None),
            ("Demo", Some("Live")),
        ] {
            let conflicts = qualifier_conflict(primary_notes, fallback_notes);
            assert!(
                conflicts
                    .iter()
                    .any(|conflict| conflict.contains("primary candidates describe")),
                "{primary_notes}: {conflicts:?}"
            );
        }

        // A fallback with one length 2 s off does not identify the tracks by
        // its lengths, so the label stays a conflict.
        let mut off_fallback = remote.clone();
        off_fallback.tracklist[3].duration_secs =
            Some(off_fallback.tracklist[3].duration_secs.unwrap() + 2.0);
        let off_fallback = Candidate::new(&local, off_fallback);
        assert!(!durations_identify_tracks(&local, &off_fallback));
        let check = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &off_fallback,
            true,
            false,
        );
        assert!(check.settled.is_empty(), "{:?}", check.settled);
        assert!(
            check
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{:?}",
            check.conflicts
        );

        // A text-only fallback lists no lengths and never identifies by them.
        let mut text_only = remote.clone();
        for track in &mut text_only.tracklist {
            track.duration_secs = None;
        }
        let text_only = Candidate::new(&local, text_only);
        assert!(!durations_identify_tracks(&local, &text_only));
        let check = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &text_only,
            true,
            true,
        );
        assert!(check.settled.is_empty(), "{:?}", check.settled);
        assert!(
            check
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{:?}",
            check.conflicts
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

    const MUSICBRAINZ_PRIMARY_ID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";

    /// The titles of [`TITLES_6`] with local track `number` (1-based) renamed.
    fn titles_with(number: usize, title: &'static str) -> [&'static str; 6] {
        let mut titles = TITLES_6;
        titles[number - 1] = title;
        titles
    }

    const TITLES_12: [&str; 12] = [
        "Opening",
        "Second Wind",
        "Hollow",
        "Undertow",
        "Lantern",
        "Closing",
        "Harbor Lights",
        "Paper Crowns",
        "Slow Rain",
        "Iron Garden",
        "Weather Vane",
        "Last Orders",
    ];

    /// The titles of [`TITLES_12`] with local track `number` (1-based) renamed.
    fn titles_12_with(number: usize, title: &'static str) -> [&'static str; 12] {
        let mut titles = TITLES_12;
        titles[number - 1] = title;
        titles
    }

    /// Files titled `file_titles`, a MusicBrainz primary titled
    /// `primary_titles` and a Discogs fallback titled like the files, all with
    /// the same lengths. `adjust` may change the files, the primary or the
    /// fallback before candidates are built. The primary must be plausible by
    /// score, so the title and length checks run against it.
    fn settling_check(
        file_titles: &[&str],
        primary_titles: &[&str],
        adjust: impl FnOnce(&mut LocalRelease, &mut RemoteRelease, &mut RemoteRelease),
    ) -> FallbackCheck {
        let (mut local, mut primary_release) = spaced_pair(file_titles, primary_titles);
        let (_, mut fallback_release) = spaced_pair(file_titles, file_titles);
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        adjust(&mut local, &mut primary_release, &mut fallback_release);
        let primary = Candidate::new(&local, primary_release);
        assert!(
            plausibility_score(&local, &primary) >= PLAUSIBLE_PRIMARY_SCORE,
            "{:?}",
            primary.score
        );
        let fallback = Candidate::new(&local, fallback_release);
        fallback_check(&local, &[primary], &fallback, true, false)
    }

    #[test]
    fn the_files_settle_a_title_conflict_with_a_placeholder_subtitle() {
        let file_titles = titles_with(3, "I Sold Myself In");
        let primary_titles = titles_with(3, "I Sold Myself In / [unknown]");
        // The lenient reading forgives the placeholder, the strict one does not.
        assert!(
            track_title_similarity("I Sold Myself In", None, "I Sold Myself In / [unknown]")
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert!(
            strict_track_title_similarity("I Sold Myself In", None, "I Sold Myself In / [unknown]")
                < TITLE_SIMILARITY_THRESHOLD
        );

        let check = settling_check(&file_titles, &primary_titles, |_, _, _| {});
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(
            check.settled,
            vec![
                "primary and fallback track titles differ for local track 3 ('I Sold Myself In / [unknown]' versus 'I Sold Myself In'); the file 'I Sold Myself In' agrees with the fallback".to_string()
            ]
        );
    }

    #[test]
    fn the_files_settle_a_title_typo_a_number_prefix_and_a_subtitle() {
        // A typo in the primary.
        let check = settling_check(
            &titles_with(3, "Free Like The Wind"),
            &titles_with(3, "Feel Like the Wind"),
            |_, _, _| {},
        );
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{check:?}");
        assert!(check.settled[0].contains("local track 3"), "{check:?}");

        // A track number left in the file's title: the cleaned reading agrees
        // with the fallback.
        let check = settling_check(
            &titles_with(3, "03 - Free Like The Wind"),
            &titles_with(3, "Feel Like the Wind"),
            |_, _, fallback| fallback.tracklist[2].title = "Free Like The Wind".to_string(),
        );
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(
            check.settled,
            vec![
                "primary and fallback track titles differ for local track 3 ('Feel Like the Wind' versus 'Free Like The Wind'); the file '03 - Free Like The Wind' agrees with the fallback".to_string()
            ]
        );

        // A subtitle the file and the fallback carry and the primary lacks.
        let check = settling_check(
            &titles_with(3, "Scary Monsters (and Super Creeps)"),
            &titles_with(3, "Scary Monsters"),
            |_, _, _| {},
        );
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{check:?}");
    }

    #[test]
    fn the_files_settle_a_duration_conflict() {
        // The primary lists 241.0 s for a track the files and the fallback
        // give as about 155 s.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(155.3);
            fallback.tracklist[3].duration_secs = Some(155.2);
            primary.tracklist[3].duration_secs = Some(241.0);
        });
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(
            check.settled,
            vec![
                "primary and fallback durations differ for local track 4 (241.0s versus 155.2s); the file's 155.3s agrees with the fallback".to_string()
            ]
        );

        // An 85.7 s length in the primary.
        let check = settling_check(&TITLES_6, &TITLES_6, |_, primary, _| {
            primary.tracklist[3].duration_secs = Some(85.7);
        });
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{check:?}");
        assert!(check.settled[0].contains("local track 4"), "{check:?}");
    }

    #[test]
    fn two_primaries_are_settled_on_different_tracks() {
        let (local, mut typo) = spaced_pair(&TITLES_6, &titles_with(2, "Secnd Wnd"));
        typo.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let (_, mut short) = spaced_pair(&TITLES_6, &TITLES_6);
        short.id = ReleaseId::MusicBrainz(mbid("1e2a3b4c-6cda-33eb-8e9a-044b5e15ffdd"));
        short.tracklist[4].duration_secs = Some(85.7);
        let primaries = [Candidate::new(&local, typo), Candidate::new(&local, short)];
        for primary in &primaries {
            assert!(plausibility_score(&local, primary) >= PLAUSIBLE_PRIMARY_SCORE);
        }
        let (_, fallback_release) = spaced_pair(&TITLES_6, &TITLES_6);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &primaries, &fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 2, "{check:?}");
        assert!(check.settled[0].contains("local track 2"), "{check:?}");
        assert!(check.settled[1].contains("local track 5"), "{check:?}");
    }

    #[test]
    fn the_files_do_not_settle_a_title_they_do_not_read_like_the_fallback() {
        // A junk file title agrees with neither provider. The primary is
        // plausible by score and not by its lengths, so its titles are compared,
        // and only the file-versus-fallback title guard keeps the title conflict.
        let (local, mut primary_release) = spaced_pair(
            &titles_12_with(3, "Track03"),
            &titles_12_with(3, "Hollow Men"),
        );
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        // Two seconds off the file, which is within the tolerance against the
        // fallback but breaks the duration rule.
        primary_release.tracklist[7].duration_secs = primary_release.tracklist[7]
            .duration_secs
            .map(|secs| secs + 2.0);
        let primary = Candidate::new(&local, primary_release);
        assert!(
            plausibility_score(&local, &primary) >= PLAUSIBLE_PRIMARY_SCORE,
            "{:?}",
            primary.score
        );
        assert!(!durations_identify_tracks(&local, &primary));
        let (_, fallback_release) = spaced_pair(&TITLES_12, &TITLES_12);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(check.conflicts.len(), 1, "{check:?}");
        assert!(check.conflicts[0].contains("local track 3"), "{check:?}");

        // A file that agrees with the primary and not with the fallback.
        let (local, primary_release) = spaced_pair(&TITLES_6, &TITLES_6);
        let mut primary_release = primary_release;
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let primary = Candidate::new(&local, primary_release);
        let (_, mut other) = spaced_pair(&TITLES_6, &titles_with(3, "Hollow Men"));
        other.tracklist[0].duration_secs = Some(160.5);
        let fallback = Candidate::new(&local, other);
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check.settled, Vec::<String>::new());
        assert!(!check.conflicts.is_empty(), "{check:?}");
    }

    #[test]
    fn the_files_do_not_settle_what_they_agree_with_both_providers_on() {
        // The title: the file reads 0.90 or better against both, which differ
        // from each other by more.
        let file = "Mary Jane Last Dance Tonight";
        let primary_title = "Mary Jane Lest Dence Tonight";
        let fallback_title = "Mary Jane Last Dancy Tonighx";
        assert!(
            strict_track_title_similarity(file, None, primary_title) >= TITLE_SIMILARITY_THRESHOLD
        );
        assert!(
            strict_track_title_similarity(file, None, fallback_title) >= TITLE_SIMILARITY_THRESHOLD
        );
        assert!(similarity(primary_title, fallback_title) < TITLE_SIMILARITY_THRESHOLD);
        let check = settling_check(
            &titles_with(3, file),
            &titles_with(3, primary_title),
            |_, _, fallback| fallback.tracklist[2].title = fallback_title.to_string(),
        );
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(check.conflicts.len(), 1, "{check:?}");

        // The length: 200 s in the files, 198 s in the primary, 202 s in the
        // fallback. The files are within 3 s of each.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(200.0);
            primary.tracklist[3].duration_secs = Some(198.0);
            fallback.tracklist[3].duration_secs = Some(202.0);
        });
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(
            check.conflicts,
            vec!["primary and fallback durations differ for local track 4".to_string()]
        );
    }

    #[test]
    fn the_files_do_not_settle_a_conflict_without_a_length_they_can_compare() {
        // A fallback that lists no length for the track: the title agreement
        // rests on text alone, against a primary that names a live version.
        let check = settling_check(
            &titles_with(3, "Hollow"),
            &titles_with(3, "Hollow (Live)"),
            |_, _, fallback| fallback.tracklist[2].duration_secs = None,
        );
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(check.conflicts.len(), 1, "{check:?}");

        // A fallback length 5 s from the file's, in a title conflict and in a
        // duration conflict.
        let check = settling_check(
            &titles_with(3, "I Sold Myself In"),
            &titles_with(3, "I Sold Myself In / [unknown]"),
            |_, _, fallback| {
                fallback.tracklist[2].duration_secs =
                    fallback.tracklist[2].duration_secs.map(|secs| secs + 5.0);
            },
        );
        assert_eq!(check.settled, Vec::<String>::new());
        assert!(check.conflicts.len() >= 2, "{check:?}");
    }

    #[test]
    fn nothing_is_settled_for_a_fallback_with_a_weak_album_title() {
        let (local, primary_release) = spaced_pair(
            &titles_with(3, "I Sold Myself In"),
            &titles_with(3, "I Sold Myself In / [unknown]"),
        );
        let mut primary_release = primary_release;
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let primary = Candidate::new(&local, primary_release);
        let (_, fallback_release) = spaced_pair(
            &titles_with(3, "I Sold Myself In"),
            &titles_with(3, "I Sold Myself In"),
        );
        let mut fallback = Candidate::new(&local, fallback_release);

        let settled = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert_eq!(settled.settled.len(), 1, "{settled:?}");

        fallback.score.album_title = 0.80;
        let weak = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(weak.settled, Vec::<String>::new());
        assert_eq!(weak.conflicts.len(), 1, "{weak:?}");
    }

    #[test]
    fn nothing_is_settled_for_a_fallback_with_a_weak_album_title_by_duration() {
        // A length that the files settle for a fallback whose album title is
        // the files' ...
        let (local, mut primary_release) = spaced_pair(&TITLES_6, &TITLES_6);
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        primary_release.tracklist[3].duration_secs = Some(85.7);
        let primary = Candidate::new(&local, primary_release);
        let (_, fallback_release) = spaced_pair(&TITLES_6, &TITLES_6);
        let mut fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert_eq!(check.settled.len(), 1, "{check:?}");
        assert_eq!(check.conflicts, Vec::<String>::new());

        // ... is a conflict for one whose album title is not.
        fallback.score.album_title = 0.80;
        let weak = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(weak.settled, Vec::<String>::new());
        assert_eq!(
            weak.conflicts,
            vec!["primary and fallback durations differ for local track 4".to_string()]
        );
    }

    #[test]
    fn nothing_is_settled_for_a_fallback_with_an_artist_that_does_not_agree() {
        // The fallback's album title is the files', its artist is not (the
        // shape the tracklist rule accepts), and the primary's artist is right.
        let (local, mut primary_release) = spaced_pair(&TITLES_6, &TITLES_6);
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        primary_release.tracklist[3].duration_secs = Some(85.7);
        let primary = Candidate::new(&local, primary_release);
        assert!(primary.score.album_artist >= TITLE_SIMILARITY_THRESHOLD);
        let (_, fallback_release) = spaced_pair(&TITLES_6, &TITLES_6);
        let mut fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(
            &local,
            std::slice::from_ref(&primary),
            &fallback,
            true,
            false,
        );
        assert_eq!(check.settled.len(), 1, "{check:?}");

        fallback.score.album_artist = 0.60;
        assert!(fallback.score.album_title >= TITLE_SIMILARITY_THRESHOLD);
        let off = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(off.settled, Vec::<String>::new());
        assert_eq!(
            off.conflicts,
            vec!["primary and fallback durations differ for local track 4".to_string()]
        );
    }

    #[test]
    fn the_files_do_not_settle_a_title_when_the_primary_names_another_version() {
        for primary_title in [
            "Hollow (Live)",
            "Hollow (Alternate Take)",
            "Hollow (Take 2)",
            "Hollow (Acoustic)",
        ] {
            let check = settling_check(
                &titles_with(3, "Hollow"),
                &titles_with(3, primary_title),
                |_, _, _| {},
            );
            assert_eq!(check.settled, Vec::<String>::new(), "{primary_title}");
            assert_eq!(check.conflicts.len(), 1, "{primary_title}: {check:?}");
        }
    }

    #[test]
    fn the_files_do_not_settle_a_title_when_the_primary_names_a_version_the_files_lack() {
        // The file's title lacking a version word is absence of evidence: rips
        // rarely carry it. Each primary names a version the fallback and the
        // file do not.
        for primary_title in [
            "Drive (Haeusermann mix)",
            "Threshold (voice-over)",
            "My Girl (en vivo)",
            "Glassy-Dusty (Wordless)",
            "Virus (edit)",
            "Song (7\" version)",
        ] {
            let base = primary_title.split(" (").next().unwrap();
            let check = settling_check(
                &titles_with(3, base),
                &titles_with(3, primary_title),
                |_, _, _| {},
            );
            assert_eq!(check.settled, Vec::<String>::new(), "{primary_title}");
            assert_eq!(check.conflicts.len(), 1, "{primary_title}: {check:?}");
        }
    }

    #[test]
    fn the_files_do_not_settle_a_title_when_the_primary_spells_a_version_word_another_way() {
        for primary_title in [
            "Rage (Accapella)",
            "Rage (A Capella)",
            "Rage (A Cappella)",
            "Rage (Acappella)",
            "Rage (Acapela)",
            "Rage (A-Cappella)",
            "Rage (Voice Over)",
            "Rage (Karaoke)",
            "Rage (Duet)",
            "Rage (Inst.)",
        ] {
            let check = settling_check(
                &titles_with(3, "Rage"),
                &titles_with(3, primary_title),
                |_, _, _| {},
            );
            assert_eq!(check.settled, Vec::<String>::new(), "{primary_title}");
            assert_eq!(check.conflicts.len(), 1, "{primary_title}: {check:?}");
        }
    }

    #[test]
    fn the_files_settle_a_title_when_the_version_words_they_carry_agree_with_the_fallback() {
        // The file and the fallback both name the version the primary omits.
        for (file_title, primary_title) in [
            ("Midnight Cowboy (Album Version)", "Midnight Cowboy"),
            ("Song [inst.]", "Song"),
        ] {
            let check = settling_check(
                &titles_with(3, file_title),
                &titles_with(3, primary_title),
                |_, _, _| {},
            );
            assert_eq!(check.conflicts, Vec::<String>::new(), "{file_title}");
            assert_eq!(check.settled.len(), 1, "{file_title}: {check:?}");
        }

        // The fallback names a version the file lacks while the primary also
        // lacks it: the file does not decide that one.
        let check = settling_check(
            &titles_with(3, "Midnight Cowboy"),
            &titles_with(3, "Midnite Cowboy"),
            |_, _, fallback| {
                fallback.tracklist[2].title = "Midnight Cowboy (Album Version)".to_string();
            },
        );
        assert_eq!(check.settled, Vec::<String>::new(), "{check:?}");

        // A language version is not a version word: both titles carry
        // `version`, and the file says which one it is.
        let check = settling_check(
            &titles_with(3, "Song (English Version)"),
            &titles_with(3, "Song (Spanish version)"),
            |_, _, _| {},
        );
        assert_eq!(check.conflicts, Vec::<String>::new(), "{check:?}");
        assert_eq!(check.settled.len(), 1, "{check:?}");
    }

    #[test]
    fn version_words_of_a_title_are_the_wider_set() {
        let words = |title: &str| title_version_words(title).into_iter().collect::<Vec<_>>();
        assert_eq!(words("Drive (Haeusermann mix)"), ["mix"]);
        assert_eq!(words("Threshold (voice-over)"), ["voiceover"]);
        assert_eq!(words("Threshold (voiceover)"), ["voiceover"]);
        assert_eq!(words("My Girl (en vivo)"), ["vivo"]);
        assert_eq!(words("Song [inst.]"), ["inst"]);
        assert_eq!(words("Song (A Cappella)"), ["acapella"]);
        assert_eq!(words("Song (Radio Edit)"), ["edit", "radio"]);
        assert_eq!(words("Song (Spanish version)"), ["version"]);
        // Remasters, stereo and deluxe are editions, and `backing` is not one.
        assert_eq!(
            words("Song (2011 Remastered Stereo Deluxe)"),
            Vec::<&str>::new()
        );
        assert_eq!(words("Backing Vocals"), Vec::<&str>::new());
    }

    #[test]
    fn the_files_do_not_settle_a_length_less_than_a_margin_closer_to_the_fallback() {
        // 2.4 s from the fallback and 3.6 s from the primary: more than the
        // tolerance from the primary, but a margin of only 1.2 s.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(217.4);
            fallback.tracklist[3].duration_secs = Some(215.0);
            primary.tracklist[3].duration_secs = Some(221.0);
        });
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(
            check.conflicts,
            vec!["primary and fallback durations differ for local track 4".to_string()]
        );

        // A margin of 3.0 s settles: 1.0 s from the fallback, 4.0 s from the
        // primary.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(216.0);
            fallback.tracklist[3].duration_secs = Some(215.0);
            primary.tracklist[3].duration_secs = Some(220.0);
        });
        assert_eq!(check.conflicts, Vec::<String>::new(), "{check:?}");
        assert_eq!(check.settled.len(), 1, "{check:?}");
    }

    #[test]
    fn the_files_settle_a_length_that_is_a_margin_closer_to_the_fallback() {
        // 2.1 s from the fallback and 4.7 s from the primary: a margin of 2.6 s.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(306.1);
            fallback.tracklist[3].duration_secs = Some(304.0);
            primary.tracklist[3].duration_secs = Some(310.8);
        });
        assert_eq!(check.conflicts, Vec::<String>::new(), "{check:?}");
        assert_eq!(check.settled.len(), 1, "{check:?}");

        // A margin of exactly CORROBORATED_DURATION_SECS settles, one under does
        // not: 2.0 s from the fallback and 3.5 s, then 3.4 s, from the primary.
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(306.0);
            fallback.tracklist[3].duration_secs = Some(304.0);
            primary.tracklist[3].duration_secs = Some(309.5);
        });
        assert_eq!(check.settled.len(), 1, "{check:?}");
        let check = settling_check(&TITLES_6, &TITLES_6, |local, primary, fallback| {
            local.tracks[3].duration_secs = Some(306.0);
            fallback.tracklist[3].duration_secs = Some(304.0);
            primary.tracklist[3].duration_secs = Some(309.4);
        });
        assert_eq!(check.settled, Vec::<String>::new(), "{check:?}");
    }

    #[test]
    fn the_files_do_not_settle_a_duration_when_their_title_reads_unlike_the_fallback() {
        // 12 tracks; the file's title on track 5 is junk, the fallback names it
        // by its real title (a lone weak title its own gates tolerate), and the
        // primary lists 85.7 s more than the file and the fallback.
        let check = settling_check(
            &titles_12_with(5, "Track 5"),
            &TITLES_12,
            |_, primary, fallback| {
                fallback.tracklist[4].title = "Lantern".to_string();
                primary.tracklist[4].duration_secs =
                    primary.tracklist[4].duration_secs.map(|secs| secs + 85.7);
            },
        );
        assert_eq!(check.settled, Vec::<String>::new());
        assert_eq!(
            check.conflicts,
            vec!["primary and fallback durations differ for local track 5".to_string()]
        );
    }

    #[test]
    fn a_settled_line_is_reported_once_however_many_primaries_agree() {
        let (local, mut first) = spaced_pair(&TITLES_6, &titles_with(3, "Hollow Men"));
        first.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let (_, mut second) = spaced_pair(&TITLES_6, &titles_with(3, "Hollow Men"));
        second.id = ReleaseId::MusicBrainz(mbid("1e2a3b4c-6cda-33eb-8e9a-044b5e15ffdd"));
        let primaries = [
            Candidate::new(&local, first),
            Candidate::new(&local, second),
        ];
        for primary in &primaries {
            assert!(plausibility_score(&local, primary) >= PLAUSIBLE_PRIMARY_SCORE);
        }
        let (_, fallback_release) = spaced_pair(&TITLES_6, &TITLES_6);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &primaries, &fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 1, "{check:?}");
    }

    #[test]
    fn one_primary_settling_does_not_settle_what_another_primary_conflicts_on() {
        // Both primaries disagree with the fallback on track 3. The files side
        // with the fallback against the first, whose title is a typo, but the
        // second names a live recording, which the files do not rule out.
        let file_titles = titles_with(3, "Free Like The Wind");
        let (local, mut typo) = spaced_pair(&file_titles, &titles_with(3, "Feel Like the Wind"));
        typo.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let (_, mut live) = spaced_pair(&file_titles, &titles_with(3, "Free Like The Wind (Live)"));
        live.id = ReleaseId::MusicBrainz(mbid("1e2a3b4c-6cda-33eb-8e9a-044b5e15ffdd"));
        let primaries = [Candidate::new(&local, typo), Candidate::new(&local, live)];
        for primary in &primaries {
            assert!(plausibility_score(&local, primary) >= PLAUSIBLE_PRIMARY_SCORE);
        }
        let (_, fallback_release) = spaced_pair(&file_titles, &file_titles);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &primaries, &fallback, true, false);
        assert_eq!(check.settled.len(), 1, "{check:?}");
        assert_eq!(check.conflicts.len(), 1, "{check:?}");
        assert!(
            check.conflicts[0].contains("'Free Like The Wind (Live)'"),
            "{check:?}"
        );
    }

    #[test]
    fn the_files_settle_conflicts_on_a_disc_scoped_mapping() {
        // The files are disc 2 of a two-disc set. The primary's titles and
        // lengths on that disc are off for two tracks; the fallback and the
        // files agree.
        let local = disc_folder("/music/Set-2CD/CD2", 2, 12);
        let mut primary_release = disc_set(&[12, 12]);
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        primary_release.tracklist[14].title = "Another Song 2-3".to_string();
        primary_release.tracklist[16].duration_secs = Some(85.7);
        let primary = Candidate::new(&local, primary_release);
        assert_eq!(primary.mapping.medium, Some(2));
        assert!(
            plausibility_score(&local, &primary) >= PLAUSIBLE_PRIMARY_SCORE,
            "{:?}",
            primary.score
        );
        let fallback = Candidate::new(&local, disc_set(&[12, 12]));
        assert_eq!(fallback.mapping.medium, Some(2));
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new());
        assert_eq!(check.settled.len(), 2, "{check:?}");
        assert!(check.settled[0].contains("local track 3"), "{check:?}");
        assert!(check.settled[1].contains("local track 5"), "{check:?}");
    }

    #[test]
    fn a_primary_that_lists_more_tracks_after_the_files_does_not_guard_the_fallback() {
        // The files are the first ten of twelve tracks; the primary says
        // `Mono` and the fallback lists exactly the ten. Only the primary that
        // maps the files completely guards the fallback: the longer edition's
        // notes describe its bonus tracks, not the files.
        let primary_with = |count: usize| {
            let (local, mut primary_release) = spaced_pair(&TITLES_12[..10], &TITLES_12[..count]);
            primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
            primary_release.edition_notes = vec!["Mono".to_string()];
            let primary = Candidate::new(&local, primary_release);
            let (_, fallback_release) = spaced_pair(&TITLES_12[..10], &TITLES_12[..10]);
            let fallback = Candidate::new(&local, fallback_release);
            (local, primary, fallback)
        };
        for count in [10, 12] {
            let (local, primary, fallback) = primary_with(count);
            assert_eq!(primary.mapping.complete, count == 10);
            assert!(plausibility_score(&local, &primary) >= PLAUSIBLE_PRIMARY_SCORE);
            let check = fallback_check(&local, &[primary], &fallback, true, false);
            let expected = if count == 10 {
                vec![
                    "primary candidates describe {\"mono\"} but the fallback describes {}"
                        .to_string(),
                ]
            } else {
                Vec::new()
            };
            assert_eq!(check.conflicts, expected, "{count} tracks: {check:?}");
        }

        // A subset that is not the first tracks does not take part either: the
        // sixth file is missing from the primary's mapping.
        let (mut local, primary, fallback) = primary_with(12);
        local.tracks.remove(5);
        let primary = Candidate::new(&local, primary.release);
        assert!(!primary.mapping.complete && leading_tracks(&local, &primary).is_none());
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check.conflicts, Vec::<String>::new(), "{check:?}");
    }

    #[test]
    fn the_files_do_not_settle_a_primary_that_is_plausible_only_by_durations_or_a_qualifier() {
        // Romanized files, a Japanese-titled primary that only the lengths
        // identify: its titles are not compared, so nothing is conflicting or
        // settled.
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
        let (local, mut primary_release) = spaced_pair(&romanized, &japanese);
        primary_release.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let primary = Candidate::new(&local, primary_release);
        assert!(plausibility_score(&local, &primary) < PLAUSIBLE_PRIMARY_SCORE);
        assert!(durations_identify_tracks(&local, &primary));
        let (_, fallback_release) = spaced_pair(&romanized, &romanized);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check, FallbackCheck::default());

        // A qualifier that is not a label stays a conflict next to a settled
        // title (`Live` would be settled too: both providers' lengths identify
        // the tracks).
        let file_titles = titles_with(3, "I Sold Myself In");
        let (local, mut mono) = spaced_pair(
            &file_titles,
            &titles_with(3, "I Sold Myself In / [unknown]"),
        );
        mono.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        mono.edition_notes = vec!["Mono".to_string()];
        let primary = Candidate::new(&local, mono);
        assert!(plausibility_score(&local, &primary) >= PLAUSIBLE_PRIMARY_SCORE);
        let (_, fallback_release) = spaced_pair(&file_titles, &file_titles);
        let fallback = Candidate::new(&local, fallback_release);
        let check = fallback_check(&local, &[primary], &fallback, true, false);
        assert_eq!(check.conflicts.len(), 1, "{check:?}");
        assert!(
            check.conflicts[0].contains("primary candidates describe"),
            "{check:?}"
        );
        assert_eq!(check.settled.len(), 1, "{check:?}");
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

        // A track more than ten seconds off is not drift. It is the one odd
        // length of an album whose titles and other lengths all agree, which
        // passes with the warning of that rule, and drift's warning is absent.
        let mut far = local_release(&titles);
        far.tracks[3].duration_secs = Some(far.tracks[3].duration_secs.unwrap() + 11.0);
        assert!(accepts(&far, &remote));
        let far_evaluation = album_evaluation(&far, &remote, &[]);
        assert!(
            far_evaluation
                .warnings
                .iter()
                .any(|warning| warning.ends_with(ODD_LENGTH_NOTE)),
            "{:?}",
            far_evaluation.warnings
        );
        assert!(
            !far_evaluation
                .warnings
                .iter()
                .any(|warning| warning.contains("but the album length agrees")),
            "{:?}",
            far_evaluation.warnings
        );

        // Six seconds is drift, and keeps drift's warning, not the odd length's.
        let mut near = local_release(&titles);
        near.tracks[3].duration_secs = Some(near.tracks[3].duration_secs.unwrap() + 6.0);
        let near_evaluation = album_evaluation(&near, &remote, &[]);
        assert!(
            near_evaluation.auto_accept,
            "{:?}",
            near_evaluation.blockers
        );
        assert!(
            near_evaluation
                .warnings
                .iter()
                .any(|warning| warning.contains("but the album length agrees")),
            "{:?}",
            near_evaluation.warnings
        );
        assert!(
            !near_evaluation
                .warnings
                .iter()
                .any(|warning| warning.ends_with(ODD_LENGTH_NOTE)),
            "{:?}",
            near_evaluation.warnings
        );

        // A second track beyond the tolerance makes two odd lengths, not one.
        let mut two = far.clone();
        two.tracks[8].duration_secs = Some(two.tracks[8].duration_secs.unwrap() + 4.0);
        assert!(!accepts(&two, &remote));

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
        assert!(!evaluation.can_apply(&local, &candidate));
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
        assert!(!evaluation.can_apply(&local, &candidate));
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
        assert!(evaluation.can_apply(&local, &candidate));

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

    // ---- R2a: a label on one side only, when every length agrees ----

    const LABEL_TITLES: [&str; 6] = [
        "Opening",
        "Second Wind",
        "Hollow",
        "Undertow",
        "Lantern",
        "Closing",
    ];

    fn qualifier_set(words: &[&'static str]) -> BTreeSet<&'static str> {
        words.iter().copied().collect()
    }

    /// Asserts the album is accepted with exactly one warning that ends with
    /// the label note, and returns that warning.
    fn assert_read_as_label(local: &LocalRelease, remote: &RemoteRelease) -> String {
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        let notes: Vec<_> = evaluation.rule_notes().collect();
        assert_eq!(notes.len(), 1, "{:?}", evaluation.warnings);
        assert!(notes[0].ends_with(LABEL_DIFFERENCE_NOTE));
        notes[0].to_string()
    }

    /// Asserts the album is accepted and that at least one warning is read as a
    /// labelling difference.
    fn assert_labelled_accept(local: &LocalRelease, remote: &RemoteRelease) {
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(
            evaluation.rule_notes().count() >= 1,
            "{:?}",
            evaluation.warnings
        );
    }

    fn assert_not_read_as_label(local: &LocalRelease, remote: &RemoteRelease) -> Evaluation {
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(!evaluation.auto_accept, "{:?}", evaluation.warnings);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|note| note.ends_with(LABEL_DIFFERENCE_NOTE)),
            "{:?}",
            evaluation.warnings
        );
        evaluation
    }

    #[test]
    fn one_sided_labels_need_a_label_on_one_side_only() {
        let release = RELEASE_LABEL_QUALIFIERS;
        for (a, b) in [
            (&[][..], &["live"][..]),
            (&["live"], &["live", "remastered"]),
            (&["acoustic", "live"], &["live"]),
            (&[], &["deluxe"]),
            (&["live"], &["stereo"]),
            (&[], &["remastered", "stereo"]),
        ] {
            let (a, b) = (qualifier_set(a), qualifier_set(b));
            assert!(one_sided_labels(&a, &b, release), "{a:?} versus {b:?}");
            assert!(one_sided_labels(&b, &a, release), "{b:?} versus {a:?}");
        }
        for (a, b) in [
            (&["demo"][..], &["live"][..]),
            (&[], &["mono"]),
            (&[], &["instrumental"]),
            (&[], &["remix"]),
            (&[], &["remixes"]),
            (&["live"], &["live", "mono"]),
            (&[], &[]),
            (&[], &["stereo"]),
            (&["live"], &["live"]),
        ] {
            let (a, b) = (qualifier_set(a), qualifier_set(b));
            assert!(!one_sided_labels(&a, &b, release), "{a:?} versus {b:?}");
            assert!(!one_sided_labels(&b, &a, release), "{b:?} versus {a:?}");
        }
        // A track's labels are narrower than a release's.
        let (empty, deluxe) = (qualifier_set(&[]), qualifier_set(&["deluxe"]));
        assert!(!one_sided_labels(&empty, &deluxe, TRACK_LABEL_QUALIFIERS));
        let live = qualifier_set(&["live"]);
        assert!(one_sided_labels(&empty, &live, TRACK_LABEL_QUALIFIERS));
    }

    #[test]
    fn a_version_label_is_stripped_only_from_the_end() {
        for (title, base) in [
            ("Clean Slate (live)", "Clean Slate"),
            ("Song - Live", "Song"),
            ("Song \u{2013} Demo", "Song"),
            ("Song [Demo Version]", "Song"),
            ("  Song (Acoustic)  ", "Song"),
            ("Live Wire", "Live Wire"),
            ("Live Forever", "Live Forever"),
            ("Song (Live) [Bonus Track]", "Song (Live) [Bonus Track]"),
            ("Song (Part 2)", "Song (Part 2)"),
            ("(Live)", "(Live)"),
            ("Song - Part 2", "Song - Part 2"),
        ] {
            assert_eq!(without_version_label(title), base, "{title}");
        }
    }

    #[test]
    fn a_release_label_on_the_remote_side_alone_is_read_as_a_labelling_difference() {
        for (setup, expected) in [
            ("notes", "Live"),
            ("notes", "Demo"),
            ("format", "CD (Album, Remastered)"),
            ("format", "Vinyl (LP, Album, Remastered, Stereo)"),
        ] {
            let (mut local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
            local.album_title = Some("Tokyo Tales".to_string());
            remote.title = "Tokyo Tales".to_string();
            if setup == "notes" {
                remote.edition_notes = vec![expected.to_string()];
            } else {
                remote.formats = vec![expected.to_string()];
            }
            let note = assert_read_as_label(&local, &remote);
            assert!(
                note.starts_with("edition qualifiers differ (local {} versus discogs {"),
                "{note}"
            );
            assert!(
                note.ends_with(
                    "but every title and every duration within 1.5s agree in track order; read as a labelling difference"
                ),
                "{note}"
            );
        }
    }

    #[test]
    fn a_label_in_the_local_album_title_against_a_silent_remote_is_read_as_a_labelling_difference()
    {
        // The provider's title is the files' title without the word `Live`.
        let (mut local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        local.album_title =
            Some("An Evening with Test Artist Recorded Live at the Royal Albert Hall".to_string());
        remote.title = "An Evening with Test Artist Recorded at the Royal Albert Hall".to_string();
        let note = assert_read_as_label(&local, &remote);
        assert!(
            note.contains("(local {\"live\"} versus discogs {})"),
            "{note}"
        );

        // A short album title cannot lose a word and still agree.
        let (mut local, remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        local.album_title = Some("Test Album (Live)".to_string());
        let evaluation = assert_not_read_as_label(&local, &remote);
        // The tracklist agrees, so the tracklist rule reads the album title as
        // the one off tag and the title blocker gives way to the qualifier one.
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_track_label_on_one_side_is_read_as_a_labelling_difference() {
        // Quicksand, `Clean Slate`.
        let mut local_titles = LABEL_TITLES;
        local_titles[1] = "Clean Slate (live)";
        local_titles[3] = "Backdraft (live)";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[1] = "Clean Slate";
        remote_titles[3] = "Backdraft";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        let note = assert_read_as_label(&local, &remote);
        assert_eq!(
            note,
            "2 track titles differ from discogs only by a version label (first 'Clean Slate (live)') and every duration agrees within 1.5s in track order; read as a labelling difference"
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation.warnings.iter().any(|warning| {
                warning.contains("track title") && !warning.ends_with(LABEL_DIFFERENCE_NOTE)
            }),
            "no weak-title warning for a label pair: {:?}",
            evaluation.warnings
        );
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("version qualifiers")),
            "{:?}",
            evaluation.blockers
        );

        // One label pair reads in the singular, and the label may be the remote's.
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Hollow";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Hollow (Live)";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        let note = assert_read_as_label(&local, &remote);
        assert!(
            note.starts_with(
                "1 track title differs from discogs only by a version label (first 'Hollow (Live)')"
            ),
            "{note}"
        );
    }

    #[test]
    fn several_live_tracks_of_a_studio_album_are_read_as_labelling_differences() {
        // Klasse Kriminale.
        let local_titles = [
            "Opening (Live)",
            "Second Wind (Live)",
            "Hollow",
            "Undertow (Live)",
            "Lantern (Live)",
            "Closing",
        ];
        let (local, remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        let note = assert_read_as_label(&local, &remote);
        assert!(
            note.starts_with("4 track titles differ from discogs"),
            "{note}"
        );
    }

    #[test]
    fn dash_tails_and_file_name_titles_are_read_as_labelling_differences() {
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Song - Live";
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Song";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_read_as_label(&local, &remote);

        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Song (Live)";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Song";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_read_as_label(&local, &remote);

        let mut local_titles = LABEL_TITLES;
        local_titles[4] = "05 - Clean Slate (Live).mp3";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[4] = "Clean Slate";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_read_as_label(&local, &remote);
    }

    #[test]
    fn mono_instrumental_and_remix_notes_are_not_labels() {
        for note in ["Mono", "Instrumental", "Remix"] {
            let (local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
            remote.edition_notes = vec![note.to_string()];
            let evaluation = assert_not_read_as_label(&local, &remote);
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|blocker| blocker.contains("edition qualifiers differ")),
                "{note}: {:?}",
                evaluation.blockers
            );
        }
    }

    #[test]
    fn a_demo_does_not_stand_for_a_live_release() {
        // The album titles agree (the provider's has no `Demo`), so only the
        // demo-versus-live subset rule can block.
        let (mut local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        local.album_title =
            Some("An Evening with Test Artist Recorded Demo at the Royal Albert Hall".to_string());
        remote.title = "An Evening with Test Artist Recorded at the Royal Albert Hall".to_string();
        let silent = assert_read_as_label(&local, &remote);
        assert!(
            silent.contains("(local {\"demo\"} versus discogs {})"),
            "{silent}"
        );
        remote.edition_notes = vec!["Live".to_string()];
        let evaluation = assert_not_read_as_label(&local, &remote);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_label_pair_needs_every_other_title_to_agree() {
        let live_remote = |mut remote: RemoteRelease| {
            remote.edition_notes = vec!["Live".to_string()];
            remote
        };
        // Another title at 0.7x scores below the minimum.
        let mut local_titles = LABEL_TITLES;
        local_titles[3] = "Undertone Rising";
        let (local, remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        let remote = live_remote(remote);
        assert_not_read_as_label(&local, &remote);

        // A label is no excuse for a weak album title (0.70 to 0.90).
        let (mut local, remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        local.album_title = Some("A Test Album".to_string());
        let remote = live_remote(remote);
        let score = Candidate::new(&local, remote.clone()).score.album_title;
        assert!((0.70..0.90).contains(&score), "{score}");
        assert_not_read_as_label(&local, &remote);

        // Four tracks are too few to identify anything by length.
        let (local, remote) = spaced_pair(&LABEL_TITLES[..4], &LABEL_TITLES[..4]);
        let remote = live_remote(remote);
        assert_not_read_as_label(&local, &remote);

        // A remote that lists no length cannot confirm any.
        let (local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        for track in &mut remote.tracklist {
            track.duration_secs = None;
        }
        let remote = live_remote(remote);
        assert_not_read_as_label(&local, &remote);

        // A placeholder title is not a label pair.
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Track03";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Live Wire";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_not_read_as_label(&local, &remote);
    }

    #[test]
    fn a_label_pair_two_seconds_off_is_not_read_as_a_labelling_difference() {
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Hollow (Live)";
        let (mut local, remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        local.tracks[2].duration_secs = Some(local.tracks[2].duration_secs.unwrap() + 2.0);
        let evaluation = assert_not_read_as_label(&local, &remote);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("version qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );

        let (mut local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        remote.edition_notes = vec!["Live".to_string()];
        local.tracks[2].duration_secs = Some(local.tracks[2].duration_secs.unwrap() + 2.0);
        assert_not_read_as_label(&local, &remote);
    }

    #[test]
    fn a_twin_title_keeps_a_label_pair_apart() {
        // A twin among the other files.
        let mut local_titles = LABEL_TITLES;
        local_titles[1] = "Hollow (Live)";
        local_titles[2] = "Hollow";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[1] = "Hollow";
        remote_titles[2] = "Hollow";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_not_read_as_label(&local, &remote);

        // A twin among the other tracks of the release.
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Song (Live)";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Song";
        remote_titles[4] = "Song (Live)";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_not_read_as_label(&local, &remote);

        // A remix is not a label, so `Song` and `Song (Remix)` stay apart.
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Song";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Song (Remix)";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_not_read_as_label(&local, &remote);
    }

    #[test]
    fn a_label_is_not_read_as_a_labelling_difference_at_edition_scope() {
        let (local, mut remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        remote.edition_notes = vec!["Live".to_string()];
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|warning| warning.ends_with(LABEL_DIFFERENCE_NOTE))
        );
    }

    #[test]
    fn an_alternative_that_differs_only_by_a_release_label_is_equivalent_audio() {
        let (local, remote) = spaced_pair(&LABEL_TITLES, &LABEL_TITLES);
        let first = Candidate::new(&local, remote.clone());
        let alternative = |notes: &str, shift: f64| {
            let mut other = remote.clone();
            other.id = ReleaseId::Discogs(43);
            other.formats = vec![notes.to_string()];
            other.tracklist[2].duration_secs = other.tracklist[2].duration_secs.map(|d| d + shift);
            Candidate::new(&local, other)
        };
        assert!(equivalent_audio(
            &first,
            &alternative("CD (Album, Remastered)", 0.0)
        ));
        assert!(equivalent_audio(
            &first,
            &alternative("CD (Album, Remastered)", 1.0)
        ));
        assert!(!equivalent_audio(
            &first,
            &alternative("CD (Album, Remastered)", 2.0)
        ));
        assert!(!equivalent_audio(
            &first,
            &alternative("CD (Album, Mono)", 0.0)
        ));
        assert!(!equivalent_audio(
            &first,
            &alternative("CD (Album, Instrumental)", 0.0)
        ));
        // Without a label, two seconds are still within the pressing tolerance.
        assert!(equivalent_audio(&first, &alternative("CD (Album)", 2.0)));

        // Fewer than five pairs: too little to call it one recording.
        let (short_local, short_remote) = spaced_pair(&LABEL_TITLES[..4], &LABEL_TITLES[..4]);
        let short_first = Candidate::new(&short_local, short_remote.clone());
        let mut short_other = short_remote;
        short_other.id = ReleaseId::Discogs(43);
        short_other.formats = vec!["CD (Album, Remastered)".to_string()];
        let short_other = Candidate::new(&short_local, short_other);
        assert!(!equivalent_audio(&short_first, &short_other));

        // A label in a track title never makes two releases equivalent.
        let mut live_titles = LABEL_TITLES;
        live_titles[2] = "Hollow (Live)";
        let mut live = remote.clone();
        live.id = ReleaseId::Discogs(44);
        live.tracklist[2].title = live_titles[2].to_string();
        assert!(!equivalent_audio(&first, &Candidate::new(&local, live)));
    }

    #[test]
    fn the_labels_of_every_level_must_agree_with_the_release_label() {
        // Demo tracks against a live release are two different claims.
        let local_titles = [
            "Opening",
            "Second Wind (Demo)",
            "Hollow",
            "Undertow (Demo)",
            "Lantern",
            "Closing",
        ];
        let (local, mut remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        let silent = assert_read_as_label(&local, &remote);
        assert!(silent.starts_with("2 track titles differ"), "{silent}");
        remote.edition_notes = vec!["Live".to_string()];
        let evaluation = assert_not_read_as_label(&local, &remote);
        assert!(!evaluation.blockers.is_empty());

        // The same label on both levels agrees, and so does a subset of the
        // release's labels.
        let live_titles = [
            "Opening",
            "Second Wind (Live)",
            "Hollow",
            "Undertow (Live)",
            "Lantern",
            "Closing",
        ];
        let (local, mut remote) = spaced_pair(&live_titles, &LABEL_TITLES);
        remote.edition_notes = vec!["Live".to_string()];
        assert_labelled_accept(&local, &remote);
        remote.formats = vec!["CD (Album, Remastered)".to_string()];
        assert_labelled_accept(&local, &remote);

        // A track label against a release that names another label.
        let (local, mut remote) = spaced_pair(&live_titles, &LABEL_TITLES);
        remote.edition_notes = vec!["Demo".to_string()];
        assert_not_read_as_label(&local, &remote);

        // Labels on the files' side at both levels add up against a silent
        // release; a release that says `Live` contradicts the album's `Demo`.
        let (mut local, mut remote) = spaced_pair(&live_titles, &LABEL_TITLES);
        local.album_title =
            Some("An Evening with Test Artist Recorded Demo at the Royal Albert Hall".to_string());
        remote.title = "An Evening with Test Artist Recorded at the Royal Albert Hall".to_string();
        assert_labelled_accept(&local, &remote);
        remote.edition_notes = vec!["Live".to_string()];
        assert_not_read_as_label(&local, &remote);
        remote.edition_notes = vec!["Remix".to_string()];
        assert_not_read_as_label(&local, &remote);
    }

    #[test]
    fn a_label_pair_must_really_be_a_label() {
        for (local_title, remote_title) in [
            ("Song (Live)", "Song (Part 2)"),
            ("Song (Live)", "Song [Bonus Track]"),
            (
                "Some Extremely Long Song Title With Many Words Live",
                "Some Extremely Long Song Title With Many Words",
            ),
            (
                "Some Extremely Long Song Title With Many Words",
                "Some Extremely Long Song Title With Many Words Live",
            ),
            ("Song (Live Rehearsal)", "Song"),
            ("Song (Demo, Take 2)", "Song"),
            ("Song - Live Session", "Song"),
            ("Song", "Song (Demo, Take 2)"),
            ("Song", "Song [Live Outtake]"),
        ] {
            let mut local_titles = LABEL_TITLES;
            local_titles[2] = local_title;
            let mut remote_titles = LABEL_TITLES;
            remote_titles[2] = remote_title;
            let (local, remote) = spaced_pair(&local_titles, &remote_titles);
            let evaluation = assert_not_read_as_label(&local, &remote);
            assert!(
                !evaluation.blockers.is_empty(),
                "{local_title} versus {remote_title}"
            );
        }
        // Real labels around the same shapes still pass.
        for (local_title, remote_title) in [
            ("Song (Live)", "Song"),
            ("Song", "Song (Live)"),
            ("Song - Live", "Song"),
            ("Song [Demo Version]", "Song"),
        ] {
            let mut local_titles = LABEL_TITLES;
            local_titles[2] = local_title;
            let mut remote_titles = LABEL_TITLES;
            remote_titles[2] = remote_title;
            let (local, remote) = spaced_pair(&local_titles, &remote_titles);
            assert_read_as_label(&local, &remote);
        }
    }

    #[test]
    fn a_deluxe_track_title_is_not_a_track_label() {
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Hollow (Deluxe)";
        let (local, remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        let evaluation = assert_not_read_as_label(&local, &remote);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("version qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn the_title_that_carries_the_extra_label_is_the_one_named() {
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Hollow (Acoustic Live)";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Hollow (Live)";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        let note = assert_read_as_label(&local, &remote);
        assert!(note.contains("(first 'Hollow (Acoustic Live)')"), "{note}");

        let (local, remote) = spaced_pair(&remote_titles_swapped(), &local_titles);
        let note = assert_read_as_label(&local, &remote);
        assert!(note.contains("(first 'Hollow (Acoustic Live)')"), "{note}");
    }

    fn remote_titles_swapped() -> [&'static str; 6] {
        let mut titles = LABEL_TITLES;
        titles[2] = "Hollow (Live)";
        titles
    }

    #[test]
    fn a_release_warning_does_not_claim_every_title_agrees_beside_label_pairs() {
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Hollow (Live)";
        let (mut local, mut remote) = spaced_pair(&local_titles, &LABEL_TITLES);
        local.album_title =
            Some("An Evening with Test Artist Recorded Live at the Royal Albert Hall".to_string());
        remote.title = "An Evening with Test Artist Recorded at the Royal Albert Hall".to_string();
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        let notes: Vec<_> = evaluation.rule_notes().collect();
        assert_eq!(notes.len(), 2, "{:?}", evaluation.warnings);
        let edition = notes
            .iter()
            .find(|note| note.starts_with("edition qualifiers differ"))
            .expect("the release warning");
        assert!(!edition.contains("every title"), "{edition}");
        assert!(edition.ends_with(LABEL_DIFFERENCE_NOTE), "{edition}");
    }

    #[test]
    fn a_remote_twin_alone_keeps_a_label_pair_apart() {
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Song (Live)";
        local_titles[4] = "Song (Acoustic) [Bonus]";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Song";
        remote_titles[4] = "Song (Acoustic)";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert_not_read_as_label(&local, &remote);
    }

    #[test]
    fn a_local_twin_alone_keeps_a_label_pair_apart() {
        // One letter apart is 0.96, three letters 0.88: the files' two titles
        // agree once the label is off, while the provider's two titles, which
        // took one different step each, do not.
        let mut local_titles = LABEL_TITLES;
        local_titles[2] = "Midnight Caravan Of Dreamz (Live)";
        local_titles[4] = "Mxdnight Caravan Of Dreamz";
        let mut remote_titles = LABEL_TITLES;
        remote_titles[2] = "Midnight Caravan Of Dreams";
        remote_titles[4] = "Mxdnight Karavan Of Dreamz";
        let (local, remote) = spaced_pair(&local_titles, &remote_titles);
        assert!(!bases_agree(remote_titles[2], remote_titles[4]));
        assert!(bases_agree(local_titles[2], local_titles[4]));
        assert_not_read_as_label(&local, &remote);
    }

    // -----------------------------------------------------------------------
    // The odd-length rule
    // -----------------------------------------------------------------------

    /// A local release and a remote release over the first `count` titles of
    /// [`TITLES_12`], with the files' track `index` (0-based) `delta` seconds
    /// longer than the provider's.
    fn odd_length_pair(count: usize, index: usize, delta: f64) -> (LocalRelease, RemoteRelease) {
        let mut local = local_release(&TITLES_12[..count]);
        let remote = remote_release(&TITLES_12[..count]);
        local.tracks[index].duration_secs =
            Some(local.tracks[index].duration_secs.unwrap() + delta);
        (local, remote)
    }

    /// The odd-length warning of an evaluation, if it has one.
    fn odd_length_warning(evaluation: &Evaluation) -> Option<&str> {
        evaluation
            .warnings
            .iter()
            .map(String::as_str)
            .find(|warning| warning.ends_with(ODD_LENGTH_NOTE))
    }

    fn assert_blocked_not_odd(local: &LocalRelease, remote: &RemoteRelease) {
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(!evaluation.auto_accept, "{:?}", evaluation.warnings);
        assert_eq!(odd_length_warning(&evaluation), None);
    }

    #[test]
    fn the_last_track_hundreds_of_seconds_long_passes_with_a_warning() {
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(
            odd_length_warning(&evaluation),
            Some(
                "track 'Iron Garden' is 312.0s longer in the files than the discogs duration; every title and every other length agrees"
            )
        );
        assert_eq!(
            evaluation.rule_notes().collect::<Vec<_>>(),
            [odd_length_warning(&evaluation).unwrap()]
        );
    }

    #[test]
    fn a_middle_track_that_is_shorter_in_the_files_passes() {
        let (local, remote) = odd_length_pair(6, 2, -95.0);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(
            odd_length_warning(&evaluation),
            Some(
                "track 'Hollow' is 95.0s shorter in the files than the discogs duration; every title and every other length agrees"
            )
        );
    }

    #[test]
    fn a_remote_length_that_is_longer_reads_as_shorter_in_the_files() {
        let (local, mut remote) = odd_length_pair(8, 4, 0.0);
        remote.tracklist[4].duration_secs = Some(remote.tracklist[4].duration_secs.unwrap() + 50.0);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(
            odd_length_warning(&evaluation),
            Some(
                "track 'Lantern' is 50.0s shorter in the files than the discogs duration; every title and every other length agrees"
            )
        );
    }

    #[test]
    fn the_odd_length_rule_applies_to_musicbrainz() {
        let (local, mut remote) = odd_length_pair(10, 9, 312.0);
        remote.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(
            odd_length_warning(&evaluation),
            Some(
                "track 'Iron Garden' is 312.0s longer in the files than the musicbrainz duration; every title and every other length agrees"
            )
        );
    }

    #[test]
    fn the_odd_length_rule_applies_to_a_disc_folder() {
        let mut local = disc_folder("/music/Set-2CD/CD2", 2, 12);
        local.tracks[11].duration_secs = Some(local.tracks[11].duration_secs.unwrap() + 120.0);
        let remote = disc_set(&[12, 12]);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, Some(2));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(odd_length_warning(&evaluation).is_some());
    }

    #[test]
    fn a_short_gap_beyond_the_album_length_allowance_is_an_odd_length() {
        // Five tracks of half a minute: five seconds is a sixth of the album, so
        // it is not drift, but it is one odd length among agreeing ones.
        let (mut local, mut remote) = odd_length_pair(5, 4, 0.0);
        for (index, track) in local.tracks.iter_mut().enumerate() {
            track.duration_secs = Some(30.0 + index as f64);
        }
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.duration_secs = Some(30.0 + index as f64);
        }
        local.tracks[4].duration_secs = Some(local.tracks[4].duration_secs.unwrap() + 5.0);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(odd_length_warning(&evaluation).is_some());
    }

    #[test]
    fn the_odd_pair_counts_at_the_tolerance_edge_in_the_odd_length_score() {
        // The edge value is what the scorer gives a pair exactly the tolerance
        // apart, and the rule's score is the album score with the odd pair's
        // similarity replaced by it.
        let edge = duration_similarity(0.0, DURATION_TOLERANCE_SECS);
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        let mut candidate = Candidate::new(&local, remote);
        candidate.score.album_title = 0.90;
        candidate.score.album_artist = 0.92;
        let plain = candidate.score.album_score();
        let replaced = odd_length_edge_score(&local, &candidate, 9);
        let by_hand = candidate
            .score
            .album_score_with_durations(candidate.score.durations + edge / 10.0);
        assert!((replaced - by_hand).abs() < 1e-9, "{replaced} {by_hand}");
        // The plain score counts the odd pair as zero, so it is below the gate
        // while the replacement is what reaches it.
        assert!(plain < SCORE_THRESHOLD, "{plain}");
        assert!(replaced >= SCORE_THRESHOLD, "{replaced}");
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(odd_length_warning(&evaluation).is_some());

        // A title mean just under one takes the replacement below the gate.
        candidate.score.track_titles = 0.99;
        assert!(odd_length_edge_score(&local, &candidate, 9) < SCORE_THRESHOLD);
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert_eq!(odd_length_warning(&evaluation), None);
    }

    #[test]
    fn an_odd_pair_counts_as_an_edge_pair_not_a_perfect_one() {
        // Perfect text passes with ten tracks and with five.
        for count in [10, 5] {
            let (local, remote) = odd_length_pair(count, count - 1, 312.0);
            let evaluation = album_evaluation(&local, &remote, &[]);
            assert!(evaluation.auto_accept, "{count}: {:?}", evaluation.blockers);
            assert!(odd_length_warning(&evaluation).is_some());
        }

        // Album title and artist at the minimum used to pass, with the odd
        // pair dropped from the durations. Counted at the edge it scores
        // 0.948, under the gate.
        let (local, remote) = odd_length_pair(10, 9, 11.0);
        let mut candidate = Candidate::new(&local, remote);
        candidate.score.album_title = 0.90;
        candidate.score.album_artist = 0.90;
        assert!(odd_length_edge_score(&local, &candidate, 9) < SCORE_THRESHOLD);
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert_eq!(odd_length_warning(&evaluation), None);
    }

    #[test]
    fn two_odd_lengths_are_not_one() {
        let (mut local, remote) = odd_length_pair(10, 9, 312.0);
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 20.0);
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn four_tracks_are_too_few_for_an_odd_length() {
        let (local, remote) = odd_length_pair(4, 3, 312.0);
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn an_odd_length_does_not_excuse_a_weak_title() {
        // On another track.
        let (mut local, remote) = odd_length_pair(10, 9, 312.0);
        local.tracks[2].title = Some("Something else entirely".to_string());
        assert_blocked_not_odd(&local, &remote);

        // On the odd track itself.
        let (mut local, remote) = odd_length_pair(10, 9, 312.0);
        local.tracks[9].title = Some("Something else entirely".to_string());
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn an_odd_length_on_a_version_of_a_song_is_another_recording() {
        // Both name a mix.
        let (mut local, mut remote) = odd_length_pair(10, 2, 200.0);
        local.tracks[2].title = Some("Hollow (Extended Mix)".to_string());
        remote.tracklist[2].title = "Hollow (Extended Mix)".to_string();
        assert_blocked_not_odd(&local, &remote);

        // Only the provider does, and the files' title agrees with its song.
        let (local, mut remote) = odd_length_pair(10, 2, 200.0);
        remote.tracklist[2].title = "Hollow (Remastered 2011)".to_string();
        assert_blocked_not_odd(&local, &remote);

        // The same mix on an unrelated track does not matter.
        let (mut local, mut remote) = odd_length_pair(10, 9, 200.0);
        local.tracks[2].title = Some("Hollow (Extended Mix)".to_string());
        remote.tracklist[2].title = "Hollow (Extended Mix)".to_string();
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
    }

    #[test]
    fn an_odd_length_on_another_take_is_another_recording() {
        let (mut local, remote) = odd_length_pair(10, 2, 200.0);
        local.tracks[2].title = Some("Hollow (Peel Session)".to_string());
        assert_blocked_not_odd(&local, &remote);

        let (mut local, remote) = odd_length_pair(10, 2, 200.0);
        local.tracks[2].title = Some("Hollow - Take 2".to_string());
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn an_odd_track_without_a_remote_length_is_not_an_odd_length() {
        // The odd track has no remote length to differ from. The titles then
        // carry the match as they always did (the text-identity rule), and the
        // odd-length rule has nothing to say.
        let (local, mut remote) = odd_length_pair(10, 9, 312.0);
        remote.tracklist[9].duration_secs = None;
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(one_length_differs(&local, &candidate, &cleaned), None);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert_eq!(odd_length_warning(&evaluation), None);
        assert!(evaluation.relies_on_unlisted_lengths());

        // Another track without a remote length leaves an odd one that no rule
        // accepts.

        let (local, mut remote) = odd_length_pair(10, 9, 312.0);
        remote.tracklist[4].duration_secs = None;
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn a_local_length_that_is_unknown_is_not_an_odd_length() {
        let (mut local, remote) = odd_length_pair(10, 9, 312.0);
        local.tracks[4].duration_secs = None;
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn another_album_that_fits_every_length_blocks_the_odd_length_rule() {
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.tracklist[9].duration_secs = local.tracks[9].duration_secs;
        let fits = Candidate::new(&local, other.clone());
        assert!(fits.mapping.complete);

        // No group on either side: another album.
        let evaluation = album_evaluation(&local, &remote, std::slice::from_ref(&fits));
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation.blockers.contains(
                &"release discogs 43 outside this release group fits every length".to_string()
            ),
            "{:?}",
            evaluation.blockers
        );
        assert_eq!(odd_length_warning(&evaluation), None);

        // A different master on the alternative.
        let mut first = remote.clone();
        first.group_id = Some(GroupId::DiscogsMaster(7));
        other.group_id = Some(GroupId::DiscogsMaster(8));
        let fits = Candidate::new(&local, other.clone());
        let evaluation = album_evaluation(&local, &first, std::slice::from_ref(&fits));
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("outside this release group fits every length")),
            "{:?}",
            evaluation.blockers
        );

        // The same master is another edition of this album, which does not
        // contradict the files.
        other.group_id = Some(GroupId::DiscogsMaster(7));
        let fits = Candidate::new(&local, other);
        let evaluation = album_evaluation(&local, &first, std::slice::from_ref(&fits));
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(odd_length_warning(&evaluation).is_some());
    }

    #[test]
    fn another_album_that_does_not_fit_every_length_leaves_the_rule_alone() {
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        // It has the same odd length, so it fits no better.
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.title = "A Different Album".to_string();
        let alternative = Candidate::new(&local, other);
        let evaluation = album_evaluation(&local, &remote, std::slice::from_ref(&alternative));
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("fits every length")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_remote_album_title_that_names_another_album_blocks_the_odd_length_rule() {
        let (mut local, mut remote) = odd_length_pair(10, 9, 312.0);
        local.album_title = Some("Exclusive Metal Sampler".to_string());
        remote.title = "Exclusive Radio Sampler".to_string();
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn swapped_part_titles_do_not_pass_with_an_odd_length() {
        let mut titles = TITLES_12[..6].to_vec();
        titles[0] = "Intro (Part 1)";
        titles[1] = "Intro (Part 2)";
        let mut local = local_release(&titles);
        titles[0] = "Intro (Part 2)";
        titles[1] = "Intro (Part 1)";
        let remote = remote_release(&titles);
        local.tracks[5].duration_secs = Some(local.tracks[5].duration_secs.unwrap() + 200.0);
        assert_blocked_not_odd(&local, &remote);
    }

    #[test]
    fn the_odd_length_rule_is_for_album_scope_only() {
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert_eq!(odd_length_warning(&evaluation), None);
    }

    /// Asserts that the odd-length rule does not read the pair as one odd
    /// length, however the rest of the evaluation goes.
    fn assert_no_odd_length(local: &LocalRelease, remote: &RemoteRelease, context: &str) {
        let candidate = Candidate::new(local, remote.clone());
        let cleaned = cleaned_track_titles(local);
        assert_eq!(
            one_length_differs(local, &candidate, &cleaned),
            None,
            "{context}"
        );
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(
            !evaluation.auto_accept,
            "{context}: {:?}",
            evaluation.warnings
        );
        assert_eq!(odd_length_warning(&evaluation), None, "{context}");
    }

    #[test]
    fn an_odd_pair_whose_remote_title_adds_a_label_or_take_is_another_recording() {
        for remote_title in [
            "Hollow (Alternate Take)",
            "Hollow (Take 2)",
            "Hollow (Outtake)",
            "Hollow (Rehearsal)",
            "Hollow (BBC Session)",
            "Hollow (Re-Recorded)",
            "Hollow (Single)",
            "Hollow (Remixed)",
            "Hollow (Acapella)",
            "Hollow - Alternate Take",
            "Hollow [Take 3]",
        ] {
            let (local, mut remote) = odd_length_pair(10, 2, 60.0);
            remote.tracklist[2].title = remote_title.to_string();
            assert_no_odd_length(&local, &remote, remote_title);
        }
    }

    #[test]
    fn an_odd_pair_whose_local_title_adds_a_label_or_take_is_another_recording() {
        for local_title in [
            "Hollow (Remixed)",
            "Hollow (Re-Recorded)",
            "Hollow (Single)",
            "Hollow - Take 2",
            "Hollow (Peel Session)",
        ] {
            let (mut local, remote) = odd_length_pair(10, 2, 60.0);
            local.tracks[2].title = Some(local_title.to_string());
            assert_no_odd_length(&local, &remote, local_title);
        }
    }

    #[test]
    fn an_odd_pair_titled_with_any_spelling_of_a_version_word_is_another_recording() {
        // The same title on both sides, so only the version word can stop the
        // rule.
        for title in [
            "Hollow (Karaoke)",
            "Hollow (Inst.)",
            "Hollow (Instrumentals)",
            "Hollow (A Cappella)",
            "Hollow (A Capella)",
            "Hollow (Acapella)",
            "Hollow (Accapella)",
            "Hollow (Acappella)",
            "Hollow (Acapela)",
            "Hollow (A-Cappella)",
            "Hollow (Duet)",
            "Hollow (Voice Over)",
            "Hollow (Voice-over)",
            "Hollow (Voiceover)",
            "Hollow (En Vivo)",
            "Hollow (Wordless)",
            "Hollow (Radio Edit)",
            "Hollow - Alternate Take",
            "Hollow \u{2013} Alternate Take",
            "Hollow \u{2013} Take 2",
        ] {
            let (local, remote) = titled_odd_pair(title);
            assert_eq!(odd_length_index(&local, &remote), None, "{title}");
        }
    }

    #[test]
    fn the_odd_pair_titles_must_agree_strictly() {
        // A subtitle that only one side has is forgiven in the album score but
        // not for the pair that carries the odd length.
        let titles = {
            let mut titles = TITLES_12[..10].to_vec();
            titles[2] = "I Sold Myself In";
            titles
        };
        let mut local = local_release(&titles);
        local.tracks[2].duration_secs = Some(local.tracks[2].duration_secs.unwrap() + 60.0);
        let mut remote = remote_release(&titles);
        remote.tracklist[2].title = "I Sold Myself In / [unknown]".to_string();
        assert!(
            strict_track_title_similarity("I Sold Myself In", None, &remote.tracklist[2].title)
                < TITLE_SIMILARITY_THRESHOLD
        );
        assert_no_odd_length(&local, &remote, "one-sided subtitle");

        // The same subtitle on another track is not the odd pair's business.
        let (mut local, mut remote) = odd_length_pair(10, 9, 60.0);
        local.tracks[2].title = Some("I Sold Myself In".to_string());
        remote.tracklist[2].title = "I Sold Myself In / [unknown]".to_string();
        let candidate = Candidate::new(&local, remote);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(one_length_differs(&local, &candidate, &cleaned), Some(9));
    }

    #[test]
    fn a_take_word_on_one_side_of_the_odd_pair_is_another_recording() {
        // The titles score at least 0.90 against each other, one letter apart,
        // so only the take word on one side separates them.
        for (take, other) in [
            ("Harbor Lights (Outtake)", "Harbor Lights (Outtaker)"),
            ("Harbor Lights - Outtake", "Harbor Lights - Outtaker"),
            ("Harbor Lights [Rehearsal]", "Harbor Lights [Rehearsel]"),
        ] {
            assert!(
                strict_track_title_similarity(take, None, other) >= TITLE_SIMILARITY_THRESHOLD
                    && strict_track_title_similarity(other, None, take)
                        >= TITLE_SIMILARITY_THRESHOLD,
                "{take} {other}"
            );
            let (mut local, mut remote) = odd_length_pair(10, 2, 200.0);
            local.tracks[2].title = Some(take.to_string());
            remote.tracklist[2].title = other.to_string();
            assert_no_odd_length(&local, &remote, "take word only locally");

            let (mut local, mut remote) = odd_length_pair(10, 2, 200.0);
            local.tracks[2].title = Some(other.to_string());
            remote.tracklist[2].title = take.to_string();
            assert_no_odd_length(&local, &remote, "take word only remotely");
        }
    }

    /// The *Darts of Pleasure* EP: the studio recording and, at the end, the
    /// home demo of the same song. The files' first track is 207.4 seconds, the
    /// demo's length, against the studio track's 182. `last` is the title of
    /// the fifth track on both sides.
    fn darts_pair(last: &str) -> (LocalRelease, RemoteRelease) {
        let titles = [
            "Darts of Pleasure",
            "Van Tango",
            "Shopping for Blood",
            "Love and Destroy",
            last,
        ];
        let lengths = [182.0, 180.0, 165.0, 200.0, 206.0];
        let mut local = local_release(&titles);
        let mut remote = remote_release(&titles);
        for (index, length) in lengths.iter().enumerate() {
            local.tracks[index].duration_secs = Some(*length);
            remote.tracklist[index].duration_secs = Some(*length);
        }
        local.tracks[0].duration_secs = Some(207.4);
        (local, remote)
    }

    fn odd_length_index(local: &LocalRelease, remote: &RemoteRelease) -> Option<usize> {
        let candidate = Candidate::new(local, remote.clone());
        let cleaned = cleaned_track_titles(local);
        one_length_differs(local, &candidate, &cleaned)
    }

    #[test]
    fn an_odd_file_that_may_be_a_same_titled_sibling_recording_is_not_an_odd_length() {
        // The odd file is the home demo, which the release lists last: the
        // studio track's length is not the file's.
        let (local, remote) = darts_pair("Darts of Pleasure [home Demo]");
        assert_no_odd_length(&local, &remote, "same base title sibling");

        // A sibling on another disc counts as well.
        let (local, mut remote) = darts_pair("Darts of Pleasure [home Demo]");
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(if index == 4 { 2 } else { 1 });
        }
        assert_eq!(odd_length_index(&local, &remote), None);
    }

    #[test]
    fn a_sibling_whose_group_names_any_version_or_take_may_be_the_odd_file() {
        // Each last track is the same song under a version or take group, so
        // the odd file may be that recording whatever the group says.
        for last in [
            "Darts of Pleasure (Radio Edit)",
            "Darts of Pleasure (Extended Mix)",
            "Darts of Pleasure (Single Version)",
            "Darts of Pleasure (Dub)",
            "Darts of Pleasure - Radio Edit",
            "Darts of Pleasure \u{2013} Radio Edit",
            "Darts of Pleasure (Karaoke)",
            "Darts of Pleasure (A Cappella)",
            "Darts of Pleasure (Alternate Take)",
            "Darts of Pleasure - Take 2",
        ] {
            let (local, remote) = darts_pair(last);
            assert_no_odd_length(&local, &remote, last);
        }
    }

    #[test]
    fn a_sibling_with_another_base_title_does_not_block_an_odd_length() {
        let (local, remote) = darts_pair("Home Tapes [home Demo]");
        assert_eq!(odd_length_index(&local, &remote), Some(0));

        // The same song without a version group on the odd pair is untouched
        // by a sibling that has another song's title.
        let (local, remote) = darts_pair("Home Tapes");
        assert_eq!(odd_length_index(&local, &remote), Some(0));
    }

    /// An odd pair on the fourth of five tracks, whose files' title is `title`
    /// on both sides.
    fn titled_odd_pair(title: &str) -> (LocalRelease, RemoteRelease) {
        let (mut local, mut remote) = darts_pair("Home Tapes");
        local.tracks[0].duration_secs = Some(182.0);
        local.tracks[3].duration_secs = Some(200.0 + 40.0);
        local.tracks[3].title = Some(title.to_string());
        remote.tracklist[3].title = title.to_string();
        (local, remote)
    }

    #[test]
    fn a_title_cut_inside_a_bracket_group_is_not_an_odd_length() {
        for title in [
            "Desperate Mother With A Gun (B",
            "Desperate Mother With A Gun [B",
            "Desperate Mother With A Gun (",
            "Desperate Mother With A Gun[B",
        ] {
            assert!(may_be_cut_to_field(title), "{title}");
            let (local, remote) = titled_odd_pair(title);
            assert_eq!(odd_length_index(&local, &remote), None, "{title}");
        }
    }

    #[test]
    fn a_closed_or_shorter_bracket_title_keeps_the_odd_length_rule() {
        for title in [
            // Closed.
            "Desperate Mother With A Gun (Bonus)",
            // Not the width of an ID3v1 field.
            "Desperate Mother Gun (B",
            // At the width but the group is closed and no longer open.
            "Desperate Mother With A (Gun)",
            // At the width, no group at all.
            "Desperate Mother With A Gunny",
        ] {
            let (local, remote) = titled_odd_pair(title);
            assert_eq!(odd_length_index(&local, &remote), Some(3), "{title}");
        }
    }

    /// An odd pair on a Various Artists release of `count` tracks.
    fn various_odd_pair(count: usize) -> (LocalRelease, RemoteRelease) {
        let (mut local, mut remote) = odd_length_pair(count, count - 1, 312.0);
        local.artist_kind = ArtistKind::VariousArtists;
        local.album_artist = Some("Various Artists".to_string());
        remote.artists = vec!["Various Artists".to_string()];
        (local, remote)
    }

    #[test]
    fn a_various_artists_release_needs_eight_tracks_for_an_odd_length() {
        let (local, remote) = various_odd_pair(ODD_LENGTH_MIN_VARIOUS_TRACKS - 1);
        assert_no_odd_length(&local, &remote, "seven tracks");

        let (local, remote) = various_odd_pair(ODD_LENGTH_MIN_VARIOUS_TRACKS);
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(odd_length_warning(&evaluation).is_some());
    }

    #[test]
    fn a_length_gap_that_the_drift_allowance_covers_is_not_an_odd_length() {
        // Nine seconds on one of ten tracks is drift. Counted at the edge the
        // odd-length score would reach the gate where the drift score does not,
        // so only the exclusion keeps the odd-length rule from taking it.
        let (local, remote) = odd_length_pair(10, 4, 9.0);
        let mut candidate = Candidate::new(&local, remote);
        candidate.score.album_title = 0.90;
        candidate.score.album_artist = 0.92;
        assert!(odd_length_edge_score(&local, &candidate, 4) >= SCORE_THRESHOLD);
        let cleaned = cleaned_track_titles(&local);
        assert_eq!(one_length_differs(&local, &candidate, &cleaned), None);
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept, "{:?}", evaluation.warnings);
        assert_eq!(odd_length_warning(&evaluation), None);
    }

    #[test]
    fn a_version_word_on_one_side_of_the_odd_pair_is_another_recording() {
        // The titles score at least 0.90 against each other, one letter apart,
        // so only the version word separates them.
        let (mut local, mut remote) = odd_length_pair(10, 2, 200.0);
        local.tracks[2].title = Some("Harbor Lights Mix".to_string());
        remote.tracklist[2].title = "Harbor Lights Mic".to_string();
        let local_side = remote.clone();
        let local_titles = local.clone();
        assert!(
            strict_track_title_similarity("Harbor Lights Mix", None, "Harbor Lights Mic")
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert_no_odd_length(&local_titles, &local_side, "version word only locally");

        let (mut local, mut remote) = odd_length_pair(10, 2, 200.0);
        local.tracks[2].title = Some("Harbor Lights Mic".to_string());
        remote.tracklist[2].title = "Harbor Lights Mix".to_string();
        assert_no_odd_length(&local, &remote, "version word only remotely");
    }

    #[test]
    fn the_odd_length_rule_needs_the_mapping_in_local_order() {
        // The provider lists the first two tracks the other way round, with
        // their lengths: titles and lengths still pair up one to one, but the
        // mapping is not in local order.
        let (local, mut remote) = odd_length_pair(10, 5, 200.0);
        remote.tracklist.swap(0, 1);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.mapping.complete && !candidate.mapping.ambiguous);
        assert!(!mapped_in_local_order(&local, &candidate));
        assert_no_odd_length(&local, &remote, "swapped order");
    }

    #[test]
    fn an_alternative_with_an_ambiguous_mapping_does_not_block_the_odd_length_rule() {
        let (local, remote) = odd_length_pair(10, 9, 312.0);
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.tracklist[9].duration_secs = local.tracks[9].duration_secs;
        let mut fits = Candidate::new(&local, other);
        assert!(fits_every_length(&local, &fits));
        fits.mapping.ambiguous = true;
        assert!(!fits_every_length(&local, &fits));
        let evaluation = album_evaluation(&local, &remote, std::slice::from_ref(&fits));
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("fits every length")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_title_that_names_a_version_is_found_in_any_word() {
        for title in [
            "Hollow (Extended Mix)",
            "Hollow - Live",
            "Hollow (2011 Remastered)",
            "Hollow Remaster",
            "Hollow (Radio Edit)",
            "Hollow Version",
        ] {
            assert!(names_a_version(title), "{title}");
        }
        for title in ["Hollow", "Second Wind", "Paper Crowns"] {
            assert!(!names_a_version(title), "{title}");
        }
    }

    // -----------------------------------------------------------------------
    // The tracklist-identity rule
    // -----------------------------------------------------------------------

    /// A local release and a remote release over the first `count` titles of
    /// [`TITLES_12`], with the tags and credits given: the files' album title
    /// and artist, the provider's album title and artist credits.
    fn tracklist_pair(
        count: usize,
        tags: (&str, &str),
        provider: (&str, &[&str]),
    ) -> (LocalRelease, RemoteRelease) {
        let mut local = local_release(&TITLES_12[..count]);
        local.album_title = Some(tags.0.to_string());
        local.album_artist = Some(tags.1.to_string());
        let mut remote = remote_release(&TITLES_12[..count]);
        remote.title = provider.0.to_string();
        remote.artists = provider.1.iter().map(|name| (*name).to_string()).collect();
        (local, remote)
    }

    /// [`tracklist_pair`] with the files' artist off and the album title right.
    fn tracklist_pair_off_artist(count: usize) -> (LocalRelease, RemoteRelease) {
        tracklist_pair(
            count,
            ("Test Album", "Test Artst Band"),
            ("Test Album", &["Test Artist"]),
        )
    }

    /// [`tracklist_pair`] with the files' album title off and the artist right.
    fn tracklist_pair_off_title(count: usize) -> (LocalRelease, RemoteRelease) {
        tracklist_pair(
            count,
            ("With Open & Loving Arms", "Test Artist"),
            ("With Loving and Open Arms", &["Test Artist"]),
        )
    }

    /// A Various Artists pair of `count` tracks whose album title is off.
    fn tracklist_pair_various(count: usize) -> (LocalRelease, RemoteRelease) {
        let (mut local, remote) = tracklist_pair(
            count,
            ("Blazin OST", "Various Artists"),
            ("Blazin\u{2019}: The Soundtrack", &["Various Artists"]),
        );
        local.artist_kind = ArtistKind::VariousArtists;
        (local, remote)
    }

    /// [`local_release`] of `titles` in `directory`, tagged `album` by `artist`.
    fn in_directory(titles: &[&str], directory: &str, album: &str, artist: &str) -> LocalRelease {
        let mut local = local_release(titles);
        local.directory = directory.to_string();
        local.album_title = Some(album.to_string());
        local.album_artist = Some(artist.to_string());
        local
    }

    /// A cut-off album tag whose directory name holds the whole title.
    const FELLERS: &str =
        "/mp3/Thinking_Fellers_Union_Local_282_-_The_Natural_Finger-7_Inch_Vinyl-1988-NuHS";

    /// The release [`FELLERS`] names, with `titles` for tracks.
    fn fellers_remote(titles: &[&str]) -> RemoteRelease {
        let mut remote = remote_release(titles);
        remote.title = "The Natural Finger".to_string();
        remote.artists = vec!["Thinking Fellers Union Local 282".to_string()];
        remote
    }

    #[test]
    fn a_cut_off_album_tag_scores_the_directory_title() {
        let titles = &TITLES_12[..4];
        let remote = fellers_remote(titles);
        let plain = in_directory(
            titles,
            "/music/album",
            "The Natural Finger 7 Inch Viny",
            "Thinking Fellers Union Local 2",
        );
        let tags = Candidate::new(&plain, remote.clone()).score;
        assert!(
            tags.album_title < CORROBORATED_TITLE_FLOOR,
            "{}",
            tags.album_title
        );

        let mut local = plain.clone();
        local.directory = FELLERS.to_string();
        let score = Candidate::new(&local, remote).score;
        assert!(
            (score.album_title - (1.0 - DIRECTORY_PENALTY)).abs() < 1e-9,
            "{}",
            score.album_title
        );
        // The cut-off artist tag already reads as the whole credit: the tag wins.
        assert_eq!(score.album_artist, tags.album_artist);
    }

    #[test]
    fn a_release_that_does_not_read_its_directory_scores_as_if_it_had_none() {
        let titles = &TITLES_12[..4];
        let remote = fellers_remote(titles);
        let plain = in_directory(
            titles,
            "/music/album",
            "The Natural Finger 7 Inch Viny",
            "Thinking Fellers Union Local 2",
        );
        let mut local = plain.clone();
        local.directory = FELLERS.to_string();
        let read = Candidate::new(&local, remote.clone()).score;
        local.reads_directory = false;
        let off = Candidate::new(&local, remote.clone()).score;
        let tags = Candidate::new(&plain, remote).score;
        assert!(read.album_title > off.album_title, "{read:?} {off:?}");
        assert_eq!(off.album_title, tags.album_title);
        assert_eq!(off.album_artist, tags.album_artist);
    }

    #[test]
    fn a_release_that_does_not_read_its_directory_ranks_on_its_tags() {
        let mut local = in_directory(&["One"], "/music/album", "Zzzz Qqqq", "Someone Else");
        let hit = SearchSummary {
            id: ReleaseId::Discogs(42),
            title: "Blue".to_string(),
            artists: vec!["Test Artist".to_string()],
            artist_credit: None,
            year: None,
            country: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            formats: Vec::new(),
            barcodes: Vec::new(),
            result_type: "release".to_string(),
        };
        let tags = rank_search_summary(&local, &hit);
        local.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        assert!(rank_search_summary(&local, &hit) > tags);
        local.reads_directory = false;
        assert_eq!(rank_search_summary(&local, &hit), tags);
    }

    #[test]
    fn the_directory_changes_only_the_album_title_and_artist() {
        let titles = &TITLES_12[..4];
        let mut remote = remote_release(titles);
        remote.title = "Blue".to_string();
        let plain = in_directory(titles, "/music/album", "Zzzz Qqqq", "Someone Else Entirely");
        let mut named = plain.clone();
        named.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        let before = Candidate::new(&plain, remote.clone());
        let after = Candidate::new(&named, remote);
        assert_eq!(before.mapping.pairs, after.mapping.pairs);
        assert_eq!(before.score.track_titles, after.score.track_titles);
        assert_eq!(before.score.durations, after.score.durations);
        assert_eq!(before.score.year, after.score.year);
        assert!(after.score.album_title > before.score.album_title);
        assert!(after.score.album_artist > before.score.album_artist);
    }

    #[test]
    fn tags_that_score_as_well_as_the_directory_win() {
        let titles = &TITLES_12[..4];
        let local = in_directory(
            titles,
            "/mp3/Test_Artist-Test_Album-2001-GRP",
            "Test Album",
            "Test Artist",
        );
        let score = Candidate::new(&local, remote_release(titles)).score;
        assert_eq!((score.album_title, score.album_artist), (1.0, 1.0));
    }

    #[test]
    fn a_various_artists_directory_adds_nothing_to_the_artist() {
        let titles = &TITLES_12[..4];
        let mut remote = remote_release(titles);
        remote.title = "Thirteen Days".to_string();
        remote.artists = vec!["Various Artists".to_string()];
        let mut local = in_directory(titles, "/music/album", "Thirteen Days", "Trevor Jones");
        let plain = Candidate::new(&local, remote.clone()).score.album_artist;
        local.directory = "/mp3/VA-Thirteen_Days_OST-2000-RNS".to_string();
        assert_eq!(Candidate::new(&local, remote).score.album_artist, plain);
    }

    #[test]
    fn a_directory_title_lets_a_cut_off_tag_pass_the_title_gate() {
        let titles = &TITLES_12[..4];
        let remote = fellers_remote(titles);
        let mut local = in_directory(
            titles,
            "/music/album",
            "The Natural Finger 7 Inch Viny",
            "Thinking Fellers Union Local 2",
        );
        let refused = album_evaluation(&local, &remote, &[]);
        assert!(!refused.auto_accept, "{:?}", refused.warnings);

        local.directory = FELLERS.to_string();
        let accepted = album_evaluation(&local, &remote, &[]);
        assert!(accepted.auto_accept, "{:?}", accepted.blockers);
        assert!(
            accepted.warnings.contains(
                &"album title 'The Natural Finger' read from the directory name".to_string()
            ),
            "{:?}",
            accepted.warnings
        );
        // The artist tag scored better than the directory: no artist warning.
        assert!(
            !accepted
                .warnings
                .iter()
                .any(|warning| warning.starts_with("artist '")),
            "{:?}",
            accepted.warnings
        );
    }

    #[test]
    fn a_various_artists_tag_on_one_artists_album_takes_the_directory_artist() {
        let titles = &TITLES_12[..4];
        let mut remote = remote_release(titles);
        remote.title = "Life of the Infamous: The Best of Mobb Deep".to_string();
        remote.artists = vec!["Mobb Deep".to_string()];
        let mut local = in_directory(
            titles,
            "/music/album",
            "Life of The Infamous...The Best of Mobb Deep",
            "Various Artists",
        );
        local.artist_kind = ArtistKind::VariousArtists;
        let refused = album_evaluation(&local, &remote, &[]);
        assert!(!refused.auto_accept);

        local.directory =
            "/mp3/Mobb_Deep-Life_of_The_Infamous___The_Best_of_Mobb_Deep-2006-CMS".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (candidate.score.album_artist - (1.0 - DIRECTORY_PENALTY)).abs() < 1e-9,
            "{}",
            candidate.score.album_artist
        );
        let accepted = album_evaluation(&local, &remote, &[]);
        assert!(accepted.auto_accept, "{:?}", accepted.blockers);
        assert!(
            accepted
                .warnings
                .contains(&"artist 'Mobb Deep' read from the directory name".to_string()),
            "{:?}",
            accepted.warnings
        );
        // The title tag scored as well as the directory's: no title warning.
        assert!(
            !accepted
                .warnings
                .iter()
                .any(|warning| warning.starts_with("album title '")),
            "{:?}",
            accepted.warnings
        );
    }

    #[test]
    fn tags_as_good_as_the_directory_carry_no_directory_warning() {
        let titles = &TITLES_12[..4];
        let local = in_directory(
            titles,
            "/mp3/Test_Artist-Test_Album-2001-GRP",
            "Test Album",
            "Test Artist",
        );
        let evaluation = album_evaluation(&local, &remote_release(titles), &[]);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|warning| warning.ends_with(DIRECTORY_NOTE)),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn a_partial_directory_reading_counts_for_nothing() {
        let titles = &TITLES_12[..4];
        let remote = remote_release(titles);
        let mut local = in_directory(titles, "/music/album", "Zzzz", "Test Artist");
        let tags = Candidate::new(&local, remote.clone()).score.album_title;
        local.directory = "/mp3/Test_Artist-Test_Album_Two-2001-GRP".to_string();
        let directory = scene::directory_split(&local);
        let raw = album_title_similarity_with(
            directory
                .as_ref()
                .and_then(|split| split.title.as_deref())
                .unwrap(),
            &remote.title,
            false,
        ) - DIRECTORY_PENALTY;
        assert!(
            raw > 0.5 && raw < TITLE_SIMILARITY_THRESHOLD,
            "the directory reading must be partial: {raw}"
        );
        assert_eq!(
            directory_title_score(&local, directory.as_ref(), &remote),
            0.0
        );
        assert_eq!(Candidate::new(&local, remote).score.album_title, tags);
    }

    /// A release with no album, artist or kind in its tags, in `directory`.
    fn untagged_in(directory: &str) -> (LocalRelease, RemoteRelease) {
        let titles = &TITLES_12[..4];
        let mut local = local_release(titles);
        local.directory = directory.to_string();
        local.album_title = None;
        local.has_album_tag = false;
        local.album_artist = None;
        local.artist_kind = ArtistKind::Unknown;
        (local, remote_release(titles))
    }

    #[test]
    fn a_contradicting_directory_title_keeps_a_release_without_an_album_tag_in_review() {
        let (local, remote) = untagged_in("/mp3/Test_Artist-Something_Unrelated-2001-GRP");
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation.auto_accept,
            "{:?} {:?}",
            Candidate::new(&local, remote).score,
            evaluation.warnings
        );
    }

    #[test]
    fn a_self_titled_directory_reads_as_the_artists_name() {
        let titles = &TITLES_12[..4];
        let mut remote = remote_release(titles);
        remote.title = "Test Artist".to_string();
        let (mut local, _) = untagged_in("/music/album");
        local.album_artist = Some("Test Artist".to_string());
        let plain = album_evaluation(&local, &remote, &[]);
        assert!(plain.auto_accept, "{:?}", plain.blockers);

        local.directory = "/mp3/Test_Artist-Self-Titled-2001-GRP".to_string();
        let named = album_evaluation(&local, &remote, &[]);
        assert!(
            named.auto_accept,
            "{:?} {:?}",
            named.blockers,
            Candidate::new(&local, remote).score
        );
    }

    #[test]
    fn a_partial_directory_title_still_passes_the_weak_title_rule() {
        let (mut local, mut remote) = untagged_in("/music/album");
        remote.title = "Exclusive Metal Radio Sampler".to_string();
        let tag = in_directory(
            &TITLES_12[..4],
            "/music/album",
            "Exclusive Metal Sampler",
            "Test Artist",
        );
        let as_tag = album_evaluation(&tag, &remote, &[]);
        assert!(as_tag.auto_accept, "{:?}", as_tag.blockers);

        local.directory = "/mp3/Test_Artist-Exclusive_Metal_Sampler-2001-GRP".to_string();
        let accepted = album_evaluation(&local, &remote, &[]);
        assert!(accepted.auto_accept, "{:?}", accepted.blockers);
        assert!(
            accepted
                .warnings
                .iter()
                .any(|warning| warning.starts_with(
                    "album title 'Exclusive Metal Sampler' read from the directory name scores 0."
                ) && warning.ends_with("but every track and duration agrees")),
            "{:?}",
            accepted.warnings
        );

        local.directory = "/mp3/Test_Artist-Something_Unrelated-2001-GRP".to_string();
        assert!(!album_evaluation(&local, &remote, &[]).auto_accept);
    }

    #[test]
    fn a_directory_artist_lets_a_release_without_any_title_pass_the_weak_title_rule() {
        let (local, remote) = untagged_in("/mp3/Test_Artist-EP-2001-GRP");
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        for expected in [
            "local release has no usable album title, but every track and duration agrees",
            "artist 'Test Artist' read from the directory name",
        ] {
            assert!(
                evaluation
                    .warnings
                    .iter()
                    .any(|warning| warning == expected),
                "{expected}: {:?}",
                evaluation.warnings
            );
        }
    }

    #[test]
    fn a_tie_between_the_tags_and_the_directory_goes_to_the_tags() {
        let titles = &TITLES_12[..4];
        let remote = remote_release(titles);
        let mut local = in_directory(titles, "/music/album", "Test Album EP", "Test Artist");
        let tags = Candidate::new(&local, remote.clone()).score.album_title;
        assert!((tags - (1.0 - ALBUM_TYPE_PENALTY)).abs() < 1e-9, "{tags}");

        local.directory = "/mp3/Test_Artist-Test_Album-2001-GRP".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (candidate.score.album_title - tags).abs() < 1e-9,
            "{}",
            candidate.score.album_title
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation
                .warnings
                .iter()
                .any(|warning| warning.ends_with(DIRECTORY_NOTE)),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn the_directory_does_not_stand_in_for_tracks() {
        let local = in_directory(
            &TITLES_12[..4],
            "/mp3/Test_Artist-Test_Album-2001-GRP",
            "Zzzz",
            "Someone Else",
        );
        let mut remote = remote_release(&TITLES_12[4..8]);
        for track in &mut remote.tracklist {
            track.duration_secs = track.duration_secs.map(|secs| secs + 60.0);
        }
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation.auto_accept,
            "{:?} {:?}",
            Candidate::new(&local, remote).score,
            evaluation.warnings
        );
    }

    #[test]
    fn a_disc_folder_scores_with_its_parents_reading() {
        let titles = &TITLES_12[..4];
        let local = in_directory(
            titles,
            "/mp3/Test_Artist-Test_Album-2CD-2001-GRP/CD2",
            "Zzzz",
            "Test Artist",
        );
        let score = Candidate::new(&local, remote_release(titles)).score;
        assert!(
            (score.album_title - (1.0 - DIRECTORY_PENALTY)).abs() < 1e-9,
            "{}",
            score.album_title
        );
    }

    #[test]
    fn a_cut_off_reading_never_applies_to_a_directory_artist() {
        let cut_off = "Thinking Fellers Union Local 2";
        assert_eq!(cut_off.chars().count(), 30);
        let mut remote = remote_release(&TITLES_12[..4]);
        remote.artists = vec!["Thinking Fellers Union Local 282 And Friends".to_string()];
        let split = SceneSplit {
            artist: Some(cut_off.to_string()),
            title: Some("Test Album".to_string()),
            various: false,
        };
        assert_eq!(directory_artist_score(Some(&split), &remote), 0.0);
        let tag = artist_credit_score(cut_off, false, true, &remote);
        assert!(tag >= TITLE_SIMILARITY_THRESHOLD, "{tag}");
    }

    fn tracklist_warning(evaluation: &Evaluation) -> Option<&str> {
        evaluation
            .warnings
            .iter()
            .map(String::as_str)
            .find(|warning| warning.ends_with(TRACKLIST_IDENTITY_NOTE))
    }

    fn tracklist_identifies(local: &LocalRelease, remote: &RemoteRelease) -> bool {
        let candidate = Candidate::new(local, remote.clone());
        tracklist_identifies_album(local, &candidate, &cleaned_track_titles(local))
    }

    /// Asserts that the rule neither holds nor warns and that the release is not
    /// accepted.
    fn assert_not_tracklist_identity(local: &LocalRelease, remote: &RemoteRelease, context: &str) {
        assert!(!tracklist_identifies(local, remote), "{context}");
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(
            !evaluation.auto_accept,
            "{context}: {:?}",
            evaluation.warnings
        );
        assert_eq!(tracklist_warning(&evaluation), None, "{context}");
    }

    fn assert_tracklist_accept(local: &LocalRelease, remote: &RemoteRelease, warning: &str) {
        assert!(tracklist_identifies(local, remote));
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(tracklist_warning(&evaluation), Some(warning));
        assert_eq!(
            evaluation.rule_notes().collect::<Vec<_>>(),
            [tracklist_warning(&evaluation).unwrap()]
        );
    }

    #[test]
    fn a_reordered_artist_name_is_identified_by_its_tracklist() {
        let (local, remote) = tracklist_pair(
            8,
            ("Test Album", "Microphones, The"),
            ("Test Album", &["The Microphones"]),
        );
        assert_tracklist_accept(
            &local,
            &remote,
            "album title scores 1.000 and artist 0.713, but all 8 track titles and lengths agree in track order",
        );
    }

    #[test]
    fn a_joint_artist_credit_is_identified_by_its_tracklist() {
        let (local, remote) = tracklist_pair(
            8,
            ("Test Album", "Leon Bibb and Eric Bibb"),
            ("Test Album", &["Leon Bibb", "Eric Bibb"]),
        );
        assert_tracklist_accept(
            &local,
            &remote,
            "album title scores 1.000 and artist 0.826, but all 8 track titles and lengths agree in track order",
        );
    }

    #[test]
    fn a_cut_off_featured_credit_is_identified_by_its_tracklist() {
        let (local, remote) = tracklist_pair(
            8,
            ("Test Album", "Basement Jaxx Feat Dizzee Rasc"),
            ("Test Album", &["Basement Jaxx", "Dizzee Rascal"]),
        );
        assert_tracklist_accept(
            &local,
            &remote,
            "album title scores 1.000 and artist 0.767, but all 8 track titles and lengths agree in track order",
        );
    }

    #[test]
    fn a_reworded_album_title_is_identified_by_its_tracklist() {
        let (local, remote) = tracklist_pair(
            6,
            ("With Open & Loving Arms", "Test Artist"),
            ("With Loving and Open Arms", &["Test Artist"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        // Under the weak album title floor, so only this rule can take it.
        assert!(
            candidate.score.album_title < CORROBORATED_TITLE_FLOOR,
            "{}",
            candidate.score.album_title
        );
        assert_tracklist_accept(
            &local,
            &remote,
            "album title scores 0.680 and artist 1.000, but all 6 track titles and lengths agree in track order",
        );
    }

    #[test]
    fn a_various_artists_soundtrack_is_identified_by_eight_tracks() {
        let (local, remote) = tracklist_pair_various(TRACKLIST_IDENTITY_MIN_VARIOUS_TRACKS);
        assert_tracklist_accept(
            &local,
            &remote,
            "album title scores 0.637 and artist 1.000, but all 8 track titles and lengths agree in track order",
        );
    }

    #[test]
    fn the_tracklist_rule_applies_to_musicbrainz() {
        let (local, mut remote) = tracklist_pair_off_artist(8);
        remote.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(tracklist_warning(&evaluation).is_some());
    }

    #[test]
    fn the_weak_album_title_rule_keeps_precedence_over_the_tracklist_rule() {
        // A title in the weak album title range with everything else perfect:
        // that rule reads it, with its own warning.
        let (local, remote) = tracklist_pair(
            6,
            ("Microphones, The", "Test Artist"),
            ("The Microphones", &["Test Artist"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(corroborated_album(&local, &candidate));
        assert!(!tracklist_identifies(&local, &remote));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(tracklist_warning(&evaluation), None);
        assert!(
            evaluation.warnings.contains(
                &"album title scores 0.756 but every track and duration agrees".to_string()
            ),
            "{:?}",
            evaluation.warnings
        );
    }

    #[test]
    fn the_tracklist_score_leaves_out_the_album_tags() {
        let (local, remote) = tracklist_pair_off_artist(8);
        let mut candidate = Candidate::new(&local, remote);
        assert!((candidate.score.tracklist_score() - 1.0).abs() < 1e-9);
        candidate.score.track_titles = 0.99;
        assert!((candidate.score.tracklist_score() - (0.35 * 0.99 + 0.15) / 0.5).abs() < 1e-9);
        // Under the gate: the rule does not hold.
        candidate.score.track_titles = 0.90;
        assert!(candidate.score.tracklist_score() < TRACKLIST_IDENTITY_SCORE);
        let cleaned = cleaned_track_titles(&local);
        assert!(!tracklist_identifies_album(&local, &candidate, &cleaned));
    }

    #[test]
    fn four_tracks_are_too_few_for_the_tracklist_rule() {
        let (local, remote) = tracklist_pair_off_title(TRACKLIST_IDENTITY_MIN_TRACKS - 1);
        assert_not_tracklist_identity(&local, &remote, "four tracks");
        let (local, remote) = tracklist_pair_off_title(TRACKLIST_IDENTITY_MIN_TRACKS);
        assert!(tracklist_identifies(&local, &remote));
    }

    #[test]
    fn a_various_artists_release_needs_eight_tracks_for_the_tracklist_rule() {
        let (local, remote) = tracklist_pair_various(TRACKLIST_IDENTITY_MIN_VARIOUS_TRACKS - 1);
        assert_not_tracklist_identity(&local, &remote, "seven tracks");
    }

    #[test]
    fn one_length_two_seconds_off_stops_the_tracklist_rule() {
        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 2.0);
        assert_not_tracklist_identity(&local, &remote, "2 s off");

        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 1.4);
        assert!(tracklist_identifies(&local, &remote));
    }

    #[test]
    fn a_track_title_at_zero_point_eight_stops_the_tracklist_rule() {
        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.tracks[3].title = Some("Undertw".to_string());
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        let similarity = candidate
            .mapping
            .title_similarity(&local, &cleaned, 3, 3, &remote.tracklist[3])
            .unwrap();
        assert!((0.75..0.90).contains(&similarity), "{similarity}");
        // The score alone would still pass.
        assert!(candidate.score.tracklist_score() >= TRACKLIST_IDENTITY_SCORE);
        assert_not_tracklist_identity(&local, &remote, "a title at 0.8");
    }

    #[test]
    fn the_title_score_recorded_for_a_pair_counts_for_the_tracklist_rule() {
        // The rule reads every pair's title score as the mapping recorded it,
        // as the album score does, and not only the strict reading of the text.
        let (local, remote) = tracklist_pair_off_artist(8);
        let mut candidate = Candidate::new(&local, remote);
        let cleaned = cleaned_track_titles(&local);
        assert!(tracklist_identifies_album(&local, &candidate, &cleaned));
        candidate.mapping.pair_similarity[3] = Some((3, 0.85));
        assert!(!track_titles_agree(&local, &candidate, &cleaned));
        assert!(!tracklist_identifies_album(&local, &candidate, &cleaned));
    }

    #[test]
    fn a_subtitle_on_one_side_of_one_track_stops_the_tracklist_rule() {
        let (mut local, mut remote) = tracklist_pair_off_artist(8);
        local.tracks[2].title = Some("Hollow".to_string());
        remote.tracklist[2].title = "Hollow / [unknown]".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert!(
            candidate
                .mapping
                .title_similarity(&local, &cleaned, 2, 2, &remote.tracklist[2])
                .unwrap()
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert!(
            strict_track_title_similarity("Hollow", None, "Hollow / [unknown]")
                < TITLE_SIMILARITY_THRESHOLD
        );
        assert_not_tracklist_identity(&local, &remote, "one-sided subtitle");
    }

    #[test]
    fn swapped_titles_one_second_apart_stop_the_tracklist_rule() {
        let mut titles = TITLES_12[..8].to_vec();
        titles[0] = "Intro (Part 1)";
        titles[1] = "Intro (Part 2)";
        let mut local = local_release(&titles);
        titles[0] = "Intro (Part 2)";
        titles[1] = "Intro (Part 1)";
        let remote = remote_release(&titles);
        local.artist_kind = ArtistKind::AlbumArtist;
        local.album_artist = Some("Test Artst Band".to_string());
        assert_not_tracklist_identity(&local, &remote, "swapped parts");
    }

    #[test]
    fn a_remote_order_that_differs_stops_the_tracklist_rule() {
        let (local, mut remote) = tracklist_pair_off_artist(8);
        remote.tracklist.swap(0, 1);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.mapping.complete && !candidate.mapping.ambiguous);
        assert!(!mapped_in_local_order(&local, &candidate));
        assert_not_tracklist_identity(&local, &remote, "swapped order");
    }

    #[test]
    fn a_title_nearer_to_another_track_stops_the_tracklist_rule() {
        // Local track 4 reads `Harbor Lights` and maps to the provider's
        // `Harbor Lightz`, but the provider's track 3 is `Harbor Lights`.
        let mut local_titles = TITLES_12[..8].to_vec();
        local_titles[2] = "Harbor Light";
        local_titles[3] = "Harbor Lights";
        let mut remote_titles = TITLES_12[..8].to_vec();
        remote_titles[2] = "Harbor Lights";
        remote_titles[3] = "Harbor Lightz";
        let (mut local, mut remote) = spaced_pair(&local_titles, &remote_titles);
        local.album_artist = Some("Test Artst Band".to_string());
        remote.artists = vec!["Test Artist".to_string()];
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert!(mapped_in_local_order(&local, &candidate));
        assert!(track_titles_agree(&local, &candidate, &cleaned));
        assert!(!titles_prefer_own_tracks(&local, &candidate, &cleaned));
        assert_not_tracklist_identity(&local, &remote, "title nearer another track");
    }

    #[test]
    fn a_length_that_is_missing_or_zero_stops_the_tracklist_rule() {
        let (local, mut remote) = tracklist_pair_off_artist(8);
        remote.tracklist[4].duration_secs = None;
        // The score alone would still pass.
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.tracklist_score() >= TRACKLIST_IDENTITY_SCORE);
        assert_not_tracklist_identity(&local, &remote, "remote length missing");

        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.tracks[4].duration_secs = None;
        assert_not_tracklist_identity(&local, &remote, "local length missing");

        let (mut local, mut remote) = tracklist_pair_off_artist(8);
        local.tracks[4].duration_secs = Some(0.0);
        remote.tracklist[4].duration_secs = Some(0.0);
        assert_not_tracklist_identity(&local, &remote, "zero lengths");
    }

    #[test]
    fn both_album_tags_off_stop_the_tracklist_rule() {
        let (local, remote) = tracklist_pair(
            6,
            ("With Open & Loving Arms", "Test Artst Band"),
            ("With Loving and Open Arms", &["Test Artist"]),
        );
        assert_not_tracklist_identity(&local, &remote, "both tags off");
    }

    #[test]
    fn a_tag_that_names_something_else_stops_the_tracklist_rule() {
        let (local, remote) = tracklist_pair(
            6,
            ("The Complete Studio Recordings", "Test Artist"),
            ("Arrival", &["Test Artist"]),
        );
        assert_not_tracklist_identity(&local, &remote, "title 0.10");

        let (local, remote) =
            tracklist_pair(6, ("Test Album", "Tribute Band"), ("Test Album", &["ABBA"]));
        assert_not_tracklist_identity(&local, &remote, "artist 0.25");
    }

    #[test]
    fn a_title_that_names_another_number_stops_the_tracklist_rule() {
        let (mut local, mut remote) = tracklist_pair_various(8);
        local.album_title = Some("Blazin OST Vol 2".to_string());
        remote.title = "Blazin\u{2019}: The Soundtrack Vol 3".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (TRACKLIST_IDENTITY_TAG_FLOOR..CORROBORATED_TITLE_FLOOR)
                .contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert_not_tracklist_identity(&local, &remote, "part 2 against 3");

        // The same number is the same album.
        remote.title = "Blazin\u{2019}: The Soundtrack Vol 2".to_string();
        assert!(tracklist_identifies(&local, &remote));
    }

    #[test]
    fn a_bootleg_title_stops_the_tracklist_rule() {
        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.album_title = Some("Test Album (Bootleg)".to_string());
        assert_not_tracklist_identity(&local, &remote, "bootleg");
    }

    #[test]
    fn a_junk_or_missing_tag_is_no_real_tag_for_the_tracklist_rule() {
        // The scorer gives filler no credit, so the tag floor and the exactly
        // one off condition already keep these out. The rule still asks for real
        // tags itself, which shows when the score is forced up.
        let identifies = |local: &LocalRelease, remote: &RemoteRelease, title: f64, artist: f64| {
            let mut candidate = Candidate::new(local, remote.clone());
            candidate.score.album_title = title;
            candidate.score.album_artist = artist;
            tracklist_identifies_album(local, &candidate, &cleaned_track_titles(local))
        };
        let (local, remote) = tracklist_pair_off_artist(8);
        assert!(identifies(&local, &remote, 1.0, 0.6));

        let (mut junk_title, remote) = tracklist_pair_off_artist(8);
        for title in ["Unknown Album", "Unknown Title", ".", "", "   "] {
            junk_title.album_title = Some(title.to_string());
            assert!(!identifies(&junk_title, &remote, 1.0, 0.6), "{title:?}");
        }
        junk_title.album_title = None;
        assert!(!identifies(&junk_title, &remote, 1.0, 0.6));

        let (mut local, remote) = tracklist_pair(
            6,
            ("Microphones, The", "Unknown Artist"),
            ("The Microphones", &["Test Artist"]),
        );
        assert!(!identifies(&local, &remote, 0.6, 1.0));
        for artist in [None, Some(""), Some("  ")] {
            local.album_artist = artist.map(str::to_string);
            assert!(!identifies(&local, &remote, 0.6, 1.0), "{artist:?}");
        }
        local.album_artist = Some("Test Artist".to_string());
        assert!(identifies(&local, &remote, 0.6, 1.0));
        // Various Artists is a real credit, with the artist off the album title.
        let (local, remote) = tracklist_pair_various(8);
        assert!(identifies(&local, &remote, 0.6, 1.0));

        // The evaluations of the same releases, as the scorer gives them.
        let (local, remote) = tracklist_pair(
            8,
            ("Unknown Album", "Test Artst Band"),
            ("Unknown Album", &["Test Artist"]),
        );
        assert_not_tracklist_identity(&local, &remote, "junk title");
        let (local, remote) = tracklist_pair(
            6,
            ("Microphones, The", "Unknown Artist"),
            ("The Microphones", &["Unknown Artist"]),
        );
        assert_not_tracklist_identity(&local, &remote, "junk artist");
        let (mut local, remote) = tracklist_pair(
            6,
            ("Microphones, The", "Test Artist"),
            ("The Microphones", &["Test Artist"]),
        );
        local.album_artist = None;
        assert_not_tracklist_identity(&local, &remote, "missing artist");
    }

    #[test]
    fn a_version_qualifier_on_a_pair_stops_the_tracklist_rule() {
        // Every remote title says live, and none of the files' does.
        let (local, mut remote) = tracklist_pair_off_artist(8);
        for track in &mut remote.tracklist {
            track.title = format!("{} (Live)", track.title);
        }
        assert_not_tracklist_identity(&local, &remote, "(Live) on every track");

        // One letter apart, so the titles agree strictly and only the
        // qualifier tells the two apart.
        let (mut local, mut remote) = tracklist_pair_off_artist(8);
        local.tracks[2].title = Some("Harbor Lights Remix".to_string());
        remote.tracklist[2].title = "Harbor Lights Remit".to_string();
        assert!(
            strict_track_title_similarity("Harbor Lights Remix", None, "Harbor Lights Remit")
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert_not_tracklist_identity(&local, &remote, "remix against remit");
    }

    #[test]
    fn another_album_within_the_gap_still_blocks_the_tracklist_rule() {
        let (local, remote) = tracklist_pair_off_artist(8);
        let mut other = remote.clone();
        other.id = ReleaseId::Discogs(43);
        other.tracklist[2].title = "Hollow Ground".to_string();
        let alternative = Candidate::new(&local, other);
        let primary = Candidate::new(&local, remote.clone());
        assert!(within_gap(
            primary.score.album_score() - alternative.score.album_score()
        ));
        let evaluation = album_evaluation(&local, &remote, std::slice::from_ref(&alternative));
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("close alternative")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn the_tracklist_rule_is_for_album_scope_only() {
        let (local, remote) = tracklist_pair_off_artist(8);
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert_eq!(tracklist_warning(&evaluation), None);
    }

    #[test]
    fn an_artist_that_is_off_needs_eight_tracks_for_the_tracklist_rule() {
        let (local, remote) =
            tracklist_pair_off_artist(TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS - 1);
        assert_eq!(local.tracks.len(), 7);
        assert_not_tracklist_identity(&local, &remote, "artist off, seven tracks");

        let (local, remote) = tracklist_pair_off_artist(TRACKLIST_IDENTITY_MIN_ARTIST_OFF_TRACKS);
        assert_eq!(local.tracks.len(), 8);
        assert!(tracklist_identifies(&local, &remote));

        // A title that is off keeps the shorter minimum.
        let (local, remote) = tracklist_pair_off_title(TRACKLIST_IDENTITY_MIN_TRACKS);
        assert!(tracklist_identifies(&local, &remote));
    }

    #[test]
    fn a_tribute_act_artist_needs_the_longer_tracklist() {
        let (local, remote) = tracklist_pair(
            7,
            ("Test Album", "Brit Floyd"),
            ("Test Album", &["Pink Floyd"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (TRACKLIST_IDENTITY_TAG_FLOOR..0.70).contains(&candidate.score.album_artist),
            "{}",
            candidate.score.album_artist
        );
        assert_not_tracklist_identity(&local, &remote, "tribute act, seven tracks");
    }

    #[test]
    fn an_unofficial_release_is_not_identified_by_its_tracklist() {
        let (local, mut remote) = tracklist_pair_off_title(6);
        assert!(tracklist_identifies(&local, &remote));

        remote.formats = vec!["CDr (Album, Unofficial Release)".to_string()];
        assert_not_tracklist_identity(&local, &remote, "unofficial Discogs release");

        let (local, mut remote) = tracklist_pair_off_title(6);
        remote.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        remote.provider_metadata = serde_json::json!({"status": "Official"});
        assert!(tracklist_identifies(&local, &remote));
        remote.provider_metadata = serde_json::json!({"status": "Bootleg"});
        assert_not_tracklist_identity(&local, &remote, "bootleg MusicBrainz release");
    }

    #[test]
    fn the_weak_album_title_band_belongs_to_its_own_rule_alone() {
        // The album title is the off tag at about 0.76 and the artist is at
        // 0.917. Every length is 1.4 s off, so the tracklist score (0.958)
        // passes but the weak album title rule's score without the title
        // (0.946) does not reach its 0.97: only that rule reads this band, and
        // it refuses.
        let (mut local, remote) = tracklist_pair(
            6,
            ("Microphones, The", "Test Artist"),
            ("The Microphones", &["Test Artists"]),
        );
        for track in &mut local.tracks {
            track.duration_secs = track.duration_secs.map(|secs| secs + 1.4);
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (CORROBORATED_TITLE_FLOOR..TITLE_SIMILARITY_THRESHOLD)
                .contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert!(candidate.score.album_artist >= TITLE_SIMILARITY_THRESHOLD);
        assert!(candidate.score.tracklist_score() >= TRACKLIST_IDENTITY_SCORE);
        assert!(!corroborated_album(&local, &candidate));
        assert_not_tracklist_identity(
            &local,
            &remote,
            "title 0.76, artist 0.92, lengths 1.4 s off",
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("below the")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_weak_album_title_with_a_low_mean_title_is_not_read_by_the_tracklist_rule() {
        // An album title between the weak title floor and the minimum, an
        // artist that agrees, and three titles at about 0.92: every title
        // agrees and the tracklist score passes, but the mean title stays
        // below what the weak album title rule asks, and that rule alone
        // reads the band.
        let (mut local, mut remote) = tracklist_pair(
            6,
            ("Test Alb", "Test Artist"),
            ("Test Album", &["Test Artist"]),
        );
        for (index, (file, provider)) in [
            ("Harbor Lights", "Harbour Lights"),
            ("Paper Crowns", "Paper Crown"),
            ("Weather Vane", "Weather Vanes"),
        ]
        .into_iter()
        .enumerate()
        {
            local.tracks[index].title = Some(file.to_string());
            remote.tracklist[index].title = provider.to_string();
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (CORROBORATED_TITLE_FLOOR..TITLE_SIMILARITY_THRESHOLD)
                .contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert!(candidate.score.album_artist >= TITLE_SIMILARITY_THRESHOLD);
        assert!(
            (0.95..CORROBORATED_MEAN_TITLE).contains(&candidate.score.track_titles),
            "{}",
            candidate.score.track_titles
        );
        let cleaned = cleaned_track_titles(&local);
        assert!(track_titles_agree(&local, &candidate, &cleaned));
        assert!(candidate.score.tracklist_score() >= TRACKLIST_IDENTITY_SCORE);
        assert!(!corroborated_album(&local, &candidate));
        assert_not_tracklist_identity(&local, &remote, "weak title, mean 0.96");

        // The same weak title with exact titles is the weak album title
        // rule's, which accepts it with its own warning.
        let (local, mut remote) = tracklist_pair(
            6,
            ("Test Alb", "Test Artist"),
            ("Test Album", &["Test Artist"]),
        );
        remote.title = "Test Album".to_string();
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(tracklist_warning(&evaluation), None);
    }

    #[test]
    fn a_title_below_the_weak_title_range_is_not_held_to_its_mean_title() {
        // Under the weak album title floor only this rule can take it, with
        // titles at about 0.92, and it does.
        let (mut local, mut remote) = tracklist_pair_off_title(6);
        local.tracks[0].title = Some("Harbor Lights".to_string());
        remote.tracklist[0].title = "Harbour Lights".to_string();
        local.tracks[1].title = Some("Paper Crowns".to_string());
        remote.tracklist[1].title = "Paper Crown".to_string();
        local.tracks[2].title = Some("Weather Vane".to_string());
        remote.tracklist[2].title = "Weather Vanes".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_title < CORROBORATED_TITLE_FLOOR);
        assert!(candidate.score.track_titles < CORROBORATED_MEAN_TITLE);
        assert!(tracklist_identifies(&local, &remote));
    }

    #[test]
    fn an_album_title_at_zero_point_four_stays_blocked_and_the_floor_is_pinned() {
        let (local, remote) = tracklist_pair(
            6,
            ("Test Album", "Test Artist"),
            ("Test Collection", &["Test Artist"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (0.30..TRACKLIST_IDENTITY_TAG_FLOOR).contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert_not_tracklist_identity(&local, &remote, "title 0.40");

        let (local, remote) = tracklist_pair(
            6,
            ("Test Album", "Test Artist"),
            ("Test Album Special", &["Test Artist"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (TRACKLIST_IDENTITY_TAG_FLOOR..CORROBORATED_TITLE_FLOOR)
                .contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert!(tracklist_identifies(&local, &remote));

        // The floor itself, on the recorded score.
        let (local, remote) = tracklist_pair_off_title(6);
        let mut candidate = Candidate::new(&local, remote);
        let cleaned = cleaned_track_titles(&local);
        candidate.score.album_title = TRACKLIST_IDENTITY_TAG_FLOOR - 0.01;
        assert!(!tracklist_identifies_album(&local, &candidate, &cleaned));
        candidate.score.album_title = TRACKLIST_IDENTITY_TAG_FLOOR + 0.01;
        assert!(tracklist_identifies_album(&local, &candidate, &cleaned));
    }

    #[test]
    fn one_length_one_point_six_seconds_off_stops_the_tracklist_rule() {
        let (mut local, remote) = tracklist_pair_off_artist(8);
        local.tracks[3].duration_secs = Some(local.tracks[3].duration_secs.unwrap() + 1.6);
        assert_not_tracklist_identity(&local, &remote, "1.6 s off");
    }

    #[test]
    fn an_edition_of_other_audio_never_passes_for_the_vocal_files() {
        for (files, provider) in [
            (
                "Enter the Wu-Tang (36 Chambers)",
                "Enter the Wu-Tang (36 Chambers) (Instrumentals)",
            ),
            (
                "The College Dropout",
                "The College Dropout (Karaoke Version)",
            ),
            ("Album", "Album (A Cappellas)"),
            ("Album", "Album (A Cappella)"),
            ("Album", "Album (Acapella)"),
            ("Album", "Album (Acappella)"),
            ("Album", "Album (Instrumentals)"),
        ] {
            let (local, remote) =
                tracklist_pair(12, (files, "Test Artist"), (provider, &["Test Artist"]));
            let evaluation = album_evaluation(&local, &remote, &[]);
            assert!(
                !evaluation.auto_accept,
                "{provider}: {:?}",
                evaluation.warnings
            );
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|blocker| blocker.starts_with("edition qualifiers differ")),
                "{provider}: {:?}",
                evaluation.blockers
            );
        }
        // The tracklist rule also reads the version words of the album titles.
        for (files, provider) in [
            (
                "Enter the Wu-Tang (36 Chambers)",
                "Enter the Wu-Tang (36 Chambers) (Instrumentals)",
            ),
            (
                "The College Dropout",
                "The College Dropout (Karaoke Version)",
            ),
        ] {
            let (local, remote) =
                tracklist_pair(12, (files, "Test Artist"), (provider, &["Test Artist"]));
            let candidate = Candidate::new(&local, remote.clone());
            assert!(
                (TRACKLIST_IDENTITY_TAG_FLOOR..TITLE_SIMILARITY_THRESHOLD)
                    .contains(&candidate.score.album_title),
                "{provider}: {}",
                candidate.score.album_title
            );
            assert!(!tracklist_identifies(&local, &remote), "{provider}");
        }
    }

    #[test]
    fn instrumental_karaoke_and_a_cappella_are_edition_qualifiers_and_never_labels() {
        for text in [
            "Album (Instrumentals)",
            "Album (Instrumental)",
            "Album (Karaoke Version)",
            "Album (A Cappella)",
            "Album (A Cappellas)",
            "Album (Acapella)",
            "Album (Acappella)",
        ] {
            let qualifiers = edition_qualifiers(text);
            assert_eq!(qualifiers.len(), 1, "{text}: {qualifiers:?}");
            let silent = edition_qualifiers("Album");
            assert!(!qualifiers_agree(&silent, &qualifiers), "{text}");
            assert!(
                !one_sided_labels(&silent, &qualifiers, RELEASE_LABEL_QUALIFIERS),
                "{text}"
            );
            assert!(
                !one_sided_labels(&silent, &qualifiers, TRACK_LABEL_QUALIFIERS),
                "{text}"
            );
        }
        assert_eq!(
            edition_qualifiers("Album (Instrumentals)"),
            edition_qualifiers("Album Instrumental")
        );
        assert_eq!(
            edition_qualifiers("Album (A Cappella)"),
            edition_qualifiers("Album (Acappella)")
        );
        // A bare `cappella` is a word, not the edition.
        assert!(edition_qualifiers("Cappella Gloria").is_empty());
    }

    #[test]
    fn the_audio_only_words_are_no_track_version_qualifiers() {
        // A track's version label is what it was before the release-level
        // words `karaoke` and `a cappella` existed: neither counts.
        for title in [
            "Rage (a cappella)",
            "Rage (Accapella)",
            "Rage (Karaoke)",
            "What I'm After (A Cappella)",
            "Kill You / Twist (a cappella)",
            "Nag Nag Nag (Akufen's Karaoke Slam Mix)",
            "Behind The Karaoke Room",
            "Ribbon In The Sky - A Capella",
        ] {
            assert!(album_track_qualifiers(title).is_empty(), "{title}");
        }
        for (local, remote) in [
            ("Rage (a cappella)", "Rage (Accapella)"),
            ("Rage", "Rage (a cappella)"),
            ("Rage (Accapella)", "Rage"),
            ("Rage", "Rage (Karaoke)"),
            ("Ribbon In The Sky - A Capella", "Ribbon In The Sky"),
        ] {
            assert!(
                qualifiers_agree(
                    &album_track_qualifiers(local),
                    &album_track_qualifiers(remote)
                ),
                "{local} versus {remote}"
            );
        }
        // `instrumental` is a track qualifier as before.
        assert!(album_track_qualifiers("Rage (Instrumental)").contains("instrumental"));
        assert!(!qualifiers_agree(
            &album_track_qualifiers("Rage"),
            &album_track_qualifiers("Rage (Instrumental)")
        ));
    }

    #[test]
    fn every_spelling_of_a_cappella_is_one_release_edition_qualifier() {
        let expected = BTreeSet::from(["acapella"]);
        for text in [
            "Album (A Cappella)",
            "Album (A Capella)",
            "Album (A Cappellas)",
            "Album (Acapella)",
            "Album (Acapela)",
            "Album (Accapella)",
            "Album (Acappella)",
            "Album (A-Cappella)",
            "Album A Capella",
        ] {
            assert_eq!(edition_qualifiers(text), expected, "{text}");
            let (local, remote) =
                tracklist_pair(12, ("Album", "Test Artist"), (text, &["Test Artist"]));
            let evaluation = album_evaluation(&local, &remote, &[]);
            assert!(!evaluation.auto_accept, "{text}: {:?}", evaluation.warnings);
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|blocker| blocker.starts_with("edition qualifiers differ")),
                "{text}: {:?}",
                evaluation.blockers
            );
        }
        assert!(edition_qualifiers("A Capital Idea").is_empty());
    }

    #[test]
    fn a_title_cut_to_an_id3v1_field_in_a_qualifier_word_carries_it() {
        let instrumental = BTreeSet::from(["instrumental"]);
        let cases = [
            ("All Saints Collected Instrumen", instrumental.clone()),
            ("Music for Earthworms (Instrume", instrumental.clone()),
            ("Collected Hits Volumes Instrum", instrumental.clone()),
            (
                "Collected Hits Volumes Remaste",
                BTreeSet::from(["remastered"]),
            ),
        ];
        for (title, expected) in cases {
            let width = title.chars().count();
            assert_eq!(width, ID3V1_FIELD_CHARS, "{title}");
            assert_eq!(
                local_edition_qualifiers(title, &expected),
                expected,
                "{title}"
            );
            // A cut-off reading is a guess: the provider has to name it.
            assert_eq!(
                local_edition_qualifiers(title, &BTreeSet::new()),
                BTreeSet::new(),
                "{title}"
            );
        }
        // A cut that would read as a qualifier the provider does not say
        // (`Remas`, `Remast`, `Delux`) agrees with a release that is silent.
        for title in [
            "Schoolboy's In Disgrace (Remas",
            "Younger Than Yesterday (Remast",
            "The Wild Life (Definitive Rema",
            "Nothing Lasts Forever - Remast",
            "Music For Earth Worms (Remaste",
            "The Longest Barrel Ride (Delux",
        ] {
            assert_eq!(title.chars().count(), ID3V1_FIELD_CHARS, "{title}");
            assert_eq!(
                local_edition_qualifiers(title, &BTreeSet::new()),
                BTreeSet::new(),
                "{title}"
            );
        }
        // A whole last word reads as written, whatever longer word it starts.
        let remix = BTreeSet::from(["remix"]);
        for title in [
            "Intervention: Collection Remix",
            "Obstacle 1 (Arthur Baker Remix",
        ] {
            assert_eq!(title.chars().count(), ID3V1_FIELD_CHARS, "{title}");
            assert_eq!(local_edition_qualifiers(title, &remix), remix, "{title}");
        }
        // The same word on a title that is not 30 characters wide is complete
        // as written (a 29-character value is cut before a space, so its last
        // word is whole), and so is a prefix of fewer than four letters, one
        // that is no qualifier's, a word that is not the last, a title that ends
        // in padding or a bracket, and a title wider than a field.
        for title in [
            "Collected Hits Volume Instrum",
            "Collected Hits Volume Remaste",
            "Plain Title Of Song Instr",
            "Collected Hits Volume Inst",
            "Collected Hits Volume Remast ",
            "Collected Hits Volume Instrux",
            "Instrume Collected Hits Volume",
            "Collected Hits Volume Lis Rem",
            "Collected Hits Volume (Instr)",
            "Unreleased Rarities Compil Instr",
        ] {
            let everything = BTreeSet::from(["instrumental", "remastered", "deluxe"]);
            assert_eq!(
                local_edition_qualifiers(title, &everything),
                BTreeSet::new(),
                "{title}"
            );
        }
        assert_eq!("Plain Title Of Song Instr".chars().count(), 25);
        assert_eq!("Collected Hits Volume Inst".chars().count(), 26);
        assert_eq!("Collected Hits Volume Instrum".chars().count(), 29);
        // Only the local side reads this way.
        assert!(edition_qualifiers("All Saints Collected Instrumen").is_empty());
    }

    #[test]
    fn a_title_cut_in_a_word_the_provider_does_not_say_is_not_a_qualifier_difference() {
        let (local, remote) = tracklist_pair(
            12,
            ("Schoolboy's In Disgrace (Remas", "Test Artist"),
            ("Schoolboy's in Disgrace", &["Test Artist"]),
        );
        assert_eq!(local.album_title.as_deref().unwrap().chars().count(), 30);
        assert!(remote_edition_qualifiers(&remote).is_empty());
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation
                .blockers
                .iter()
                .chain(&evaluation.warnings)
                .any(|reason| reason.contains("edition qualifiers differ")),
            "{evaluation:?}"
        );
    }

    #[test]
    fn files_cut_in_the_word_instrumental_are_not_an_edition_qualifier_difference() {
        let (local, remote) = tracklist_pair(
            12,
            ("All Saints Collected Instrumen", "Test Artist"),
            (
                "All Saints (Collected Instrumentals 1977\u{2013}1999)",
                &["Test Artist"],
            ),
        );
        assert_eq!(local.album_title.as_deref().unwrap().chars().count(), 30);
        assert!(remote_edition_qualifiers(&remote).contains("instrumental"));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation
                .blockers
                .iter()
                .chain(&evaluation.warnings)
                .any(|reason| reason.contains("edition qualifiers differ")),
            "{evaluation:?}"
        );

        // A 25-character title ending in `Instr` is complete as written.
        let (local, remote) = tracklist_pair(
            12,
            ("Plain Title Of Song Instr", "Test Artist"),
            ("Plain Title Of Song Instrumental", &["Test Artist"]),
        );
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_title_that_names_another_version_stops_the_tracklist_rule() {
        // No edition qualifier differs, but the provider's title names a radio
        // edit that the files' title lacks.
        let (local, remote) = tracklist_pair(
            6,
            ("Enter the Wu-Tang", "Test Artist"),
            ("Enter the Wu-Tang (Radio Edit)", &["Test Artist"]),
        );
        assert_eq!(
            edition_qualifiers(local.album_title.as_deref().unwrap()),
            remote_edition_qualifiers(&remote)
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (TRACKLIST_IDENTITY_TAG_FLOOR..TITLE_SIMILARITY_THRESHOLD)
                .contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert_not_tracklist_identity(&local, &remote, "radio edit on one side");

        // Both sides naming the same version still identify.
        let (local, remote) = tracklist_pair(
            6,
            ("Enter the Wu Tang (Radio Edit)", "Test Artist"),
            ("Enter the Wu-Tang (Radio Edit)", &["Test Artist"]),
        );
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_title >= TITLE_SIMILARITY_THRESHOLD);
        assert_eq!(
            title_version_words(local.album_title.as_deref().unwrap()),
            title_version_words(&remote.title)
        );
    }

    // -----------------------------------------------------------------------
    // The leading-tracks rule
    // -----------------------------------------------------------------------

    const BONUS_TITLES: [&str; 12] = [
        "Bonus One",
        "Bonus Two",
        "Bonus Three",
        "Bonus Four",
        "Bonus Five",
        "Bonus Six",
        "Bonus Seven",
        "Bonus Eight",
        "Bonus Nine",
        "Bonus Ten",
        "Bonus Eleven",
        "Bonus Twelve",
    ];

    /// `present` files with the first `present` titles of [`TITLES_12`] and a
    /// release that lists those tracks and then bonus tracks up to `listed`.
    /// Lengths agree exactly.
    fn leading_pair(present: usize, listed: usize) -> (LocalRelease, RemoteRelease) {
        let local = local_release(&TITLES_12[..present]);
        let mut titles: Vec<&str> = TITLES_12[..present].to_vec();
        titles.extend_from_slice(&BONUS_TITLES[..listed - present]);
        (local, remote_release(&titles))
    }

    fn leading_warning(evaluation: &Evaluation) -> Option<&str> {
        evaluation
            .warnings
            .iter()
            .map(String::as_str)
            .find(|warning| warning.ends_with(MORE_TRACKS_NOTE))
    }

    fn leading_identifies(local: &LocalRelease, remote: &RemoteRelease) -> bool {
        let candidate = Candidate::new(local, remote.clone());
        leading_tracks_identify(local, &candidate, &cleaned_track_titles(local)).is_some()
    }

    /// Asserts that the rule does not hold, that the release is not accepted
    /// and that no warning of the rule is printed.
    fn assert_not_leading(local: &LocalRelease, remote: &RemoteRelease, context: &str) {
        assert!(!leading_identifies(local, remote), "{context}");
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(
            !evaluation.auto_accept,
            "{context}: {:?}",
            evaluation.warnings
        );
        assert_eq!(leading_warning(&evaluation), None, "{context}");
    }

    fn assert_leading_accept(local: &LocalRelease, remote: &RemoteRelease, warning: &str) {
        let candidate = Candidate::new(local, remote.clone());
        assert!(leading_tracks(local, &candidate).is_some());
        assert!(leading_identifies(local, remote));
        let evaluation = album_evaluation(local, remote, &[]);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(leading_warning(&evaluation), Some(warning));
        assert_eq!(evaluation.rule_notes().collect::<Vec<_>>(), [warning]);
        assert!(evaluation.can_apply(local, &candidate));
    }

    #[test]
    fn ten_files_that_open_a_twelve_track_release_are_accepted_with_a_warning() {
        let (local, remote) = leading_pair(10, 12);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(!candidate.mapping.complete && !candidate.mapping.ambiguous);
        assert_eq!(
            leading_tracks(&local, &candidate),
            Some(LeadingTracks {
                present: 10,
                listed: 12
            })
        );
        assert_leading_accept(
            &local,
            &remote,
            "the files are the first 10 of 12 tracks; discogs lists more tracks after the files' last",
        );
        // The incomplete mapping is no blocker when the rule applies.
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(
            !evaluation
                .blockers
                .contains(&"track mapping is incomplete".to_string())
        );
    }

    #[test]
    fn the_leading_tracks_rule_applies_to_musicbrainz() {
        let (local, mut remote) = leading_pair(10, 12);
        remote.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        assert_leading_accept(
            &local,
            &remote,
            "the files are the first 10 of 12 tracks; musicbrainz lists more tracks after the files' last",
        );
    }

    #[test]
    fn the_share_of_the_release_that_the_files_make_up_decides() {
        for (present, listed) in [(5, 6), (6, 8), (9, 12), (12, 16)] {
            let (local, remote) = leading_pair(present, listed);
            assert_leading_accept(
                &local,
                &remote,
                &format!(
                    "the files are the first {present} of {listed} tracks; discogs lists more tracks after the files' last"
                ),
            );
        }
        for (present, listed) in [(5, 7), (9, 13)] {
            let (local, remote) = leading_pair(present, listed);
            let candidate = Candidate::new(&local, remote.clone());
            assert_eq!(leading_tracks(&local, &candidate), None);
            assert_not_leading(&local, &remote, &format!("{present} of {listed}"));
            assert!(
                !album_evaluation(&local, &remote, &[]).can_apply(&local, &candidate),
                "{present} of {listed}"
            );
        }
    }

    #[test]
    fn the_files_of_a_multi_disc_release_that_open_its_first_disc_are_accepted() {
        // Twelve files, a release of two media with twelve tracks on the first
        // and three on the second, mapped as a whole.
        let (local, mut remote) = leading_pair(12, 12);
        for (index, title) in BONUS_TITLES[..3].iter().enumerate() {
            remote.tracklist.push(RemoteTrack {
                position: Some(format!("2-{}", index + 1)),
                title: (*title).to_string(),
                artists: Vec::new(),
                duration_secs: Some(300.0),
                entry_type: TrackEntryType::Track,
                medium_position: Some(2),
                track_position: Some(index as u32 + 1),
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            });
        }
        for (index, track) in remote.tracklist.iter_mut().take(12).enumerate() {
            track.position = Some(format!("1-{}", index + 1));
            track.medium_position = Some(1);
            track.track_position = Some(index as u32 + 1);
        }
        assert!(remote_media(&remote).is_some());
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, None);
        assert_leading_accept(
            &local,
            &remote,
            "the files are the first 12 of 15 tracks; discogs lists more tracks after the files' last",
        );
    }

    #[test]
    fn the_leading_tracks_rule_is_for_album_scope_only() {
        let (local, remote) = leading_pair(10, 12);
        let candidate = Candidate::new(&local, remote);
        let evaluation = evaluate(&local, &candidate, None, &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .contains(&"track mapping is incomplete".to_string())
        );
        assert_eq!(leading_warning(&evaluation), None);
        assert!(!evaluation.can_apply(&local, &candidate));
        assert!(mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Edition));
    }

    #[test]
    fn no_other_subset_of_a_release_can_be_applied() {
        // The file for the sixth track is missing: not the first tracks.
        let mut titles = TITLES_12[..10].to_vec();
        titles.remove(5);
        let mut local = local_release(&titles);
        let remote = remote_release(&TITLES_12[..10]);
        // The files keep their track numbers.
        for track in local.tracks.iter_mut().skip(5) {
            track.track_number = track.track_number.map(|number| number + 1);
        }
        let candidate = Candidate::new(&local, remote.clone());
        assert!(!candidate.mapping.complete);
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert_not_leading(&local, &remote, "a missing middle track");
        // Renumbered, the files would sit in the first nine places, but their
        // titles do not agree there.
        let renumbered = local_release(&titles);
        let candidate = Candidate::new(&renumbered, remote.clone());
        assert!(leading_tracks(&renumbered, &candidate).is_some());
        assert_not_leading(&renumbered, &remote, "a missing middle track, renumbered");
        assert!(
            album_evaluation(&local, &remote, &[])
                .blockers
                .contains(&"track mapping is incomplete".to_string())
        );

        // The last tracks of a release, and every track from the second on.
        let mut local = local_release(&TITLES_12[2..]);
        for track in &mut local.tracks {
            track.track_number = track.track_number.map(|number| number + 2);
        }
        let remote = remote_release(&TITLES_12);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert_not_leading(&local, &remote, "the last tracks");

        // Complete mappings stay as they were, and a structural issue blocks.
        let (local, remote) = leading_pair(10, 10);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(mapping_can_apply(&local, &candidate, MatchScope::Edition));
        let (local, mut remote) = leading_pair(10, 12);
        remote.structure_issues = vec!["medium 1 has a pregap track".to_string()];
        let candidate = Candidate::new(&local, remote);
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));

        // A complete mapping of a release that gains an unsupported entry after
        // it was mapped is not applied either.
        let (local, remote) = leading_pair(10, 10);
        let mut candidate = Candidate::new(&local, remote);
        assert!(candidate.mapping.complete);
        candidate.release.structure_issues = vec!["medium 1 has a pregap track".to_string()];
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Edition));
    }

    #[test]
    fn the_shape_needs_an_incomplete_mapping_that_is_not_onto_one_disc() {
        // As many files as tracks, but a mapping that says it is incomplete.
        let (local, remote) = leading_pair(10, 10);
        let mut candidate = Candidate::new(&local, remote);
        candidate.mapping.complete = false;
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));

        // A mapping onto one medium, even for files that name no disc.
        let (local, remote) = leading_pair(10, 12);
        let mut candidate = Candidate::new(&local, remote);
        assert!(leading_tracks(&local, &candidate).is_some());
        candidate.mapping.medium = Some(2);
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
    }

    #[test]
    fn a_score_under_the_gate_stays_blocked() {
        // Four titles at about 0.91 and every length 3 s off: every pair agrees,
        // but the album score is under 0.95.
        let (mut local, mut remote) = leading_pair(10, 12);
        for (index, typo) in [
            (1, "Second Wing"),
            (6, "Harbor Lighta"),
            (7, "Paper Crowna"),
            (9, "Iron Gardew"),
        ] {
            remote.tracklist[index].title = typo.to_string();
        }
        for track in &mut local.tracks {
            track.duration_secs = track.duration_secs.map(|secs| secs + 3.0);
        }
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert!(track_titles_agree(&local, &candidate, &cleaned));
        assert!(candidate.score.album_score() < SCORE_THRESHOLD);
        assert_not_leading(&local, &remote, "album score under 0.95");
    }

    #[test]
    fn too_few_files_stay_blocked() {
        // A three-track single against a twelve-track album.
        let (local, remote) = leading_pair(3, 12);
        assert_not_leading(&local, &remote, "single against album");
        // A five-track EP against twelve tracks, and four of five.
        let (local, remote) = leading_pair(5, 12);
        assert_not_leading(&local, &remote, "EP against album");
        let (local, remote) = leading_pair(4, 5);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert_not_leading(&local, &remote, "four of five");
    }

    #[test]
    fn files_out_of_the_release_order_stay_blocked() {
        let (local, mut remote) = leading_pair(10, 12);
        remote.tracklist.swap(2, 3);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert_not_leading(&local, &remote, "out of order");
    }

    #[test]
    fn an_ambiguous_mapping_stays_blocked() {
        let (local, remote) = leading_pair(10, 12);
        let mut candidate = Candidate::new(&local, remote);
        assert!(leading_tracks(&local, &candidate).is_some());
        candidate.mapping.ambiguous = true;
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        let evaluation =
            evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default());
        assert!(!evaluation.auto_accept);
        assert_eq!(leading_warning(&evaluation), None);
    }

    #[test]
    fn a_disc_folder_is_never_the_first_tracks_of_a_release() {
        // `CD1` of a single-disc release listing two more tracks.
        let (mut local, remote) = leading_pair(10, 12);
        local.directory = "/music/Set/CD1".to_string();
        assert!(local_disc_hint(&local).is_some());
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert_not_leading(&local, &remote, "CD1 folder");

        // The tenth to twelfth... ten files of the twelve on the second disc of
        // a set: mapped onto that disc alone.
        let remote = disc_set(&[4, 12]);
        let local = disc_folder("/music/Set/CD2", 2, 10);
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(candidate.mapping.medium, Some(2));
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(!evaluation.auto_accept);
        assert_eq!(leading_warning(&evaluation), None);
    }

    #[test]
    fn a_disc_named_by_the_album_title_or_the_disc_tags_is_not_the_first_tracks() {
        // `Album (Disc 1)`: the files say they are one disc of a set.
        let (mut local, mut remote) = leading_pair(10, 12);
        local.album_title = Some("Test Album (Disc 1)".to_string());
        remote.title = "Test Album (Disc 1)".to_string();
        assert!(local_disc_hint(&local).is_some());
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert_not_leading(&local, &remote, "disc 1 in the album title");

        // Every file tagged disc 2.
        let (mut local, remote) = leading_pair(10, 12);
        for track in &mut local.tracks {
            track.disc = Some(2);
        }
        assert!(local_disc_hint(&local).is_some());
        let candidate = Candidate::new(&local, remote.clone());
        assert_eq!(leading_tracks(&local, &candidate), None);
        assert!(!mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert_not_leading(&local, &remote, "uniform disc 2 tags");

        // A uniform disc 1 tag is what a missing tag means, so it gives no hint.
        let (mut local, remote) = leading_pair(10, 12);
        for track in &mut local.tracks {
            track.disc = Some(1);
        }
        assert!(local_disc_hint(&local).is_none());
        assert!(leading_identifies(&local, &remote));
    }

    #[test]
    fn the_longer_edition_with_a_release_level_qualifier_stays_blocked() {
        // The files carry no qualifier and the release that lists more tracks
        // is a `Deluxe` edition. The label exception needs lengths that
        // identify a complete mapping, which an incomplete one cannot give.
        let (local, mut remote) = leading_pair(10, 12);
        remote.edition_notes = vec!["Deluxe".to_string()];
        let candidate = Candidate::new(&local, remote.clone());
        assert!(leading_tracks(&local, &candidate).is_some());
        let evaluation = album_evaluation(&local, &remote, &[]);
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("edition qualifiers differ")),
            "{:?}",
            evaluation.blockers
        );
    }

    #[test]
    fn a_weak_title_stays_blocked() {
        let (mut local, mut remote) = leading_pair(10, 12);
        local.tracks[6].title = Some("Harbor Lights".to_string());
        remote.tracklist[6].title = "Harbor Lighters".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        let similarity = candidate
            .mapping
            .title_similarity(&local, &cleaned, 6, 6, &remote.tracklist[6])
            .unwrap();
        assert!(
            (0.75..TITLE_SIMILARITY_THRESHOLD).contains(&similarity),
            "{similarity}"
        );
        assert_not_leading(&local, &remote, "a title at 0.8");
    }

    #[test]
    fn a_subtitle_on_one_side_of_a_mapped_pair_stays_blocked() {
        let (mut local, mut remote) = leading_pair(10, 12);
        local.tracks[2].title = Some("Hollow".to_string());
        remote.tracklist[2].title = "Hollow / [unknown]".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert!(
            candidate
                .mapping
                .title_similarity(&local, &cleaned, 2, 2, &remote.tracklist[2])
                .unwrap()
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert!(
            strict_track_title_similarity("Hollow", None, "Hollow / [unknown]")
                < TITLE_SIMILARITY_THRESHOLD
        );
        assert_not_leading(&local, &remote, "one-sided subtitle");
    }

    #[test]
    fn a_title_nearer_an_extra_track_stays_blocked() {
        // The mapped title is a near match while a track after the files has
        // the file's title exactly.
        let (local, mut remote) = leading_pair(10, 12);
        remote.tracklist[6].title = "Harbor Lightss".to_string();
        remote.tracklist[11].title = "Harbor Lights".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        let cleaned = cleaned_track_titles(&local);
        assert!(leading_tracks(&local, &candidate).is_some());
        assert!(track_titles_agree(&local, &candidate, &cleaned));
        assert!(!titles_prefer_own_tracks(&local, &candidate, &cleaned));
        assert_not_leading(&local, &remote, "a title nearer an extra track");
    }

    #[test]
    fn a_length_that_is_off_or_missing_stays_blocked() {
        // The fifth track is 184 s on both sides to begin with.
        let (mut local, remote) = leading_pair(10, 12);
        local.tracks[4].duration_secs = Some(189.0);
        assert_not_leading(&local, &remote, "5 s off");

        let (mut local, remote) = leading_pair(10, 12);
        local.tracks[4].duration_secs = Some(187.0);
        assert!(leading_identifies(&local, &remote), "exactly 3 s off");
        local.tracks[4].duration_secs = Some(187.5);
        assert_not_leading(&local, &remote, "3.5 s off");

        let (local, mut remote) = leading_pair(10, 12);
        remote.tracklist[4].duration_secs = None;
        assert_not_leading(&local, &remote, "a remote length missing");

        let (mut local, remote) = leading_pair(10, 12);
        local.tracks[4].duration_secs = None;
        assert_not_leading(&local, &remote, "a local length missing");

        // A length missing on a track after the files does not matter.
        let (local, mut remote) = leading_pair(10, 12);
        remote.tracklist[11].duration_secs = None;
        assert!(leading_identifies(&local, &remote));
    }

    #[test]
    fn an_album_title_that_does_not_agree_stays_blocked() {
        let (mut local, remote) = leading_pair(10, 12);
        local.album_title = Some("Test Albumxx".to_string());
        let candidate = Candidate::new(&local, remote.clone());
        assert!(
            (0.80..TITLE_SIMILARITY_THRESHOLD).contains(&candidate.score.album_title),
            "{}",
            candidate.score.album_title
        );
        assert_not_leading(&local, &remote, "album title 0.85");

        let (mut local, mut remote) = leading_pair(10, 12);
        local.album_title = Some("Hits Vol. 2".to_string());
        remote.title = "Hits Vol. 3".to_string();
        let candidate = Candidate::new(&local, remote.clone());
        assert!(candidate.score.album_title >= TITLE_SIMILARITY_THRESHOLD);
        assert_not_leading(&local, &remote, "Vol. 2 against Vol. 3");

        let (mut local, remote) = leading_pair(10, 12);
        local.album_artist = Some("Somebody Else".to_string());
        assert_not_leading(&local, &remote, "another artist");
    }

    #[test]
    fn a_track_version_qualifier_that_differs_stays_blocked() {
        let (mut local, mut remote) = leading_pair(10, 12);
        local.tracks[6].title = Some("Harbor Lights Remix".to_string());
        remote.tracklist[6].title = "Harbor Lights Remit".to_string();
        assert!(
            strict_track_title_similarity("Harbor Lights Remix", None, "Harbor Lights Remit")
                >= TITLE_SIMILARITY_THRESHOLD
        );
        assert_not_leading(&local, &remote, "remix against remit");
    }

    #[test]
    fn an_unofficial_release_stays_blocked() {
        let (local, mut remote) = leading_pair(10, 12);
        remote.formats = vec!["CD (Unofficial Release)".to_string()];
        assert_not_leading(&local, &remote, "unofficial Discogs release");

        let (local, mut remote) = leading_pair(10, 12);
        remote.id = ReleaseId::MusicBrainz(mbid(MUSICBRAINZ_PRIMARY_ID));
        remote.provider_metadata = serde_json::json!({"status": "Bootleg"});
        assert_not_leading(&local, &remote, "bootleg MusicBrainz release");
    }

    #[test]
    fn a_title_that_names_a_short_release_needs_the_release_to_say_so() {
        for word in ["EP", "CDEP", "Single", "CDS", "CDM", "Maxi", "MCD", "VLS"] {
            let (mut local, mut remote) = leading_pair(6, 8);
            local.album_title = Some(format!("Test Album {word}"));
            remote.formats = vec!["CD".to_string()];
            let candidate = Candidate::new(&local, remote.clone());
            assert!(
                candidate.score.album_title >= TITLE_SIMILARITY_THRESHOLD,
                "{word}: {}",
                candidate.score.album_title
            );
            assert_not_leading(&local, &remote, &format!("{word} against CD"));

            // A format that names a short release.
            remote.formats = vec!["CD (Single, EP)".to_string()];
            assert!(
                leading_identifies(&local, &remote),
                "{word} against EP format"
            );
            // A title that does.
            remote.formats = vec!["CD".to_string()];
            remote.title = format!("Test Album {word}");
            assert!(leading_identifies(&local, &remote), "{word} against title");
        }

        // The wording of the format is read word by word.
        let (mut local, mut remote) = leading_pair(6, 8);
        local.album_title = Some("Test Album EP".to_string());
        remote.formats = vec!["Vinyl (12\", Maxi-Single)".to_string()];
        assert!(leading_identifies(&local, &remote));
        remote.formats = vec!["CD (Album)".to_string(), "DVD".to_string()];
        assert_not_leading(&local, &remote, "album and DVD formats");

        // A title that names none needs nothing, and `Epic` is not `EP`.
        let (local, mut remote) = leading_pair(6, 8);
        remote.formats = vec!["CD".to_string()];
        assert!(leading_identifies(&local, &remote));
        let (mut local, mut remote) = leading_pair(6, 8);
        local.album_title = Some("Test Album Epic".to_string());
        remote.title = "Test Album Epic".to_string();
        remote.formats = vec!["CD".to_string()];
        assert!(leading_identifies(&local, &remote));
    }

    #[test]
    fn a_single_sided_format_does_not_name_a_single() {
        let (mut local, mut remote) = leading_pair(6, 8);
        local.album_title = Some("Test Album EP".to_string());
        for format in [
            "CD (Single Sided)",
            "Vinyl (12\", Single-sided)",
            "DVD (single sided)",
        ] {
            remote.formats = vec![format.to_string()];
            assert!(!remote_names_short_release(&remote), "{format}");
            assert_not_leading(&local, &remote, format);
        }
        // A format that also names a short release still does.
        for format in ["CD (Single Sided, EP)", "Vinyl (7\", Single, Single-sided)"] {
            remote.formats = vec![format.to_string()];
            assert!(remote_names_short_release(&remote), "{format}");
            assert!(leading_identifies(&local, &remote), "{format}");
        }
    }

    #[test]
    fn a_various_artists_release_needs_eight_files() {
        let various = |present, listed| {
            let (mut local, mut remote) = leading_pair(present, listed);
            local.artist_kind = ArtistKind::VariousArtists;
            local.album_artist = Some("Various Artists".to_string());
            remote.artists = vec!["Various Artists".to_string()];
            (local, remote)
        };
        // The minimum is part of the shape, so a smaller subset cannot be
        // applied either, not even by a manual decision.
        for (present, listed) in [(5, 6), (7, 9)] {
            let (local, remote) = various(present, listed);
            let candidate = Candidate::new(&local, remote.clone());
            assert_eq!(leading_tracks(&local, &candidate), None, "{present} files");
            assert!(
                !mapping_can_apply(&local, &candidate, MatchScope::Album),
                "{present} files"
            );
            assert_not_leading(
                &local,
                &remote,
                &format!("Various Artists, {present} files"),
            );
        }
        let (local, remote) = various(LEADING_TRACKS_MIN_VARIOUS, 10);
        let candidate = Candidate::new(&local, remote.clone());
        assert!(leading_tracks(&local, &candidate).is_some());
        assert!(mapping_can_apply(&local, &candidate, MatchScope::Album));
        assert!(leading_identifies(&local, &remote));
        // Other artists keep the smaller minimum.
        let (local, remote) = leading_pair(LEADING_TRACKS_MIN, 6);
        let candidate = Candidate::new(&local, remote);
        assert!(mapping_can_apply(&local, &candidate, MatchScope::Album));
    }

    /// A leading first candidate (the files are the first `present` of
    /// `listed` tracks) and an exact edition of the same release group, with
    /// every length a fixed offset from the file's.
    fn reference_pair(
        present: usize,
        first_offset: f64,
        exact_offset: f64,
    ) -> (LocalRelease, Candidate, Candidate) {
        let (local, mut longer) = leading_pair(present, present + 2);
        let (_, mut exact) = leading_pair(present, present);
        longer.group_id = Some(GroupId::DiscogsMaster(7));
        exact.group_id = Some(GroupId::DiscogsMaster(7));
        exact.id = ReleaseId::Discogs(43);
        for track in &mut longer.tracklist {
            track.duration_secs = track.duration_secs.map(|secs| secs + first_offset);
        }
        for track in &mut exact.tracklist {
            track.duration_secs = track.duration_secs.map(|secs| secs + exact_offset);
        }
        let first = Candidate::new(&local, longer);
        let exact = Candidate::new(&local, exact);
        (local, first, exact)
    }

    #[test]
    fn a_lengthless_exact_edition_is_a_worse_reference_than_a_leading_first() {
        let (local, first, mut exact) = reference_pair(10, 0.0, 0.0);
        assert!(leading_tracks(&local, &first).is_some());
        assert!(lists_exactly_the_files_shape(&local, &first, &exact));
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));

        for track in &mut exact.release.tracklist {
            track.duration_secs = None;
        }
        assert!(lists_exactly_the_files_shape(&local, &first, &exact));
        assert_eq!(
            worse_reference(&local, &first, &exact),
            Some("it lists no length for a track whose length discogs 42 lists".to_string())
        );
        assert!(!lists_exactly_the_files(&local, &first, &exact));

        // One missing length is enough, and a zero length is no length.
        let (local, first, mut exact) = reference_pair(10, 0.0, 0.0);
        exact.release.tracklist[3].duration_secs = Some(0.0);
        assert!(
            worse_reference(&local, &first, &exact)
                .is_some_and(|reason| reason.starts_with("it lists no length"))
        );

        // A first candidate with no lengths of its own has none to compare.
        let (local, mut first, mut exact) = reference_pair(10, 0.0, 0.0);
        for track in &mut first.release.tracklist {
            track.duration_secs = None;
        }
        for track in &mut exact.release.tracklist {
            track.duration_secs = None;
        }
        assert_eq!(worse_reference(&local, &first, &exact), None);
    }

    #[test]
    fn an_exact_edition_with_lengths_further_off_is_a_worse_reference() {
        let (local, first, exact) = reference_pair(10, 0.5, 3.0);
        assert!(leading_tracks(&local, &first).is_some());
        assert_eq!(
            worse_reference(&local, &first, &exact),
            Some("its lengths fit the files worse (up to 3.00s against 0.50s)".to_string())
        );
        assert!(!lists_exactly_the_files(&local, &first, &exact));

        // Within the margin of the first candidate's gap it is not.
        let (local, first, exact) = reference_pair(10, 0.2, 1.0);
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));
    }

    #[test]
    fn the_length_margin_is_exactly_one_and_a_half_seconds() {
        // 2.0 s against 0.5 s is exactly the margin.
        let (local, first, exact) = reference_pair(10, 0.5, 2.0);
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));

        // Just past it, and the text says so with two decimals.
        let (local, first, exact) = reference_pair(10, 0.5, 2.01);
        assert_eq!(
            worse_reference(&local, &first, &exact),
            Some("its lengths fit the files worse (up to 2.01s against 0.50s)".to_string())
        );
    }

    #[test]
    fn the_fit_is_compared_only_when_first_could_be_accepted_itself() {
        let lacks_a_length = |exact: &mut Candidate| {
            exact.release.tracklist[3].duration_secs = None;
        };

        // The control: a first candidate the rule accepts keeps its place.
        let (local, first, mut exact) = reference_pair(10, 0.0, 0.0);
        lacks_a_length(&mut exact);
        assert!(
            worse_reference(&local, &first, &exact)
                .is_some_and(|reason| reason.starts_with("it lists no length"))
        );

        // Unofficial: the rule refuses it, so the exact edition is promoted.
        let (local, mut first, mut exact) = reference_pair(10, 0.0, 0.0);
        first.release.formats = vec!["CD, Unofficial Release".to_string()];
        lacks_a_length(&mut exact);
        assert!(leading_tracks(&local, &first).is_some());
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));

        // Video only: never kept first against an audio edition.
        let (local, mut first, mut exact) = reference_pair(10, 0.0, 0.0);
        first.release.formats = vec!["DVD (Album)".to_string()];
        lacks_a_length(&mut exact);
        assert!(leading_tracks(&local, &first).is_some());
        assert_eq!(worse_reference(&local, &first, &exact), None);

        // A mapped length 4.5 s off fails the rule's three second limit.
        let (local, first, mut exact) = reference_pair(10, 4.5, 0.0);
        lacks_a_length(&mut exact);
        assert!(leading_tracks(&local, &first).is_some());
        assert!(leading_tracks_identify(&local, &first, &cleaned_track_titles(&local)).is_none());
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));
    }

    #[test]
    fn a_dvd_audio_disc_is_not_a_video_release() {
        let (_, _, exact) = reference_pair(10, 0.0, 0.0);
        let mut remote = exact.release;
        remote.formats = vec!["DVD (DVD-Audio, Album)".to_string()];
        assert!(!is_video_only(&remote));
        remote.formats = vec!["DVD Audio".to_string()];
        assert!(!is_video_only(&remote));
        remote.formats = vec!["DVD (DVD-Video, Album)".to_string()];
        assert!(is_video_only(&remote));
    }

    #[test]
    fn an_exact_edition_with_worse_track_titles_is_a_worse_reference() {
        let (local, first, mut exact) = reference_pair(10, 0.0, 0.0);
        exact.score.track_titles = first.score.track_titles - 0.2;
        assert_eq!(
            worse_reference(&local, &first, &exact),
            Some(format!(
                "its track titles fit the files worse ({:.2} against {:.2})",
                exact.score.track_titles, first.score.track_titles
            ))
        );
        assert!(!lists_exactly_the_files(&local, &first, &exact));

        // Within 0.05 it is not, and just past it is.
        exact.score.track_titles = first.score.track_titles - RUNNER_UP_GAP;
        assert_eq!(worse_reference(&local, &first, &exact), None);
        exact.score.track_titles = first.score.track_titles - RUNNER_UP_GAP - 0.001;
        assert!(
            worse_reference(&local, &first, &exact)
                .is_some_and(|reason| reason.starts_with("its track titles fit the files worse"))
        );
    }

    #[test]
    fn an_unofficial_or_video_only_exact_edition_is_a_worse_reference_whatever_first_is() {
        // Four files are below the minimum of a leading shape, so only the
        // checks that do not depend on it apply.
        let (local, first, exact) = reference_pair(4, 0.0, 0.0);
        assert!(leading_tracks(&local, &first).is_none());
        let reason = |first: &Candidate, exact: &Candidate| worse_reference(&local, first, exact);

        let mut unofficial = exact.clone();
        unofficial.release.formats = vec!["CD, Unofficial Release".to_string()];
        assert_eq!(
            reason(&first, &unofficial),
            Some("it is unofficial".to_string())
        );
        let mut bootleg = exact.clone();
        bootleg.release.provider_metadata = serde_json::json!({"status": "Bootleg"});
        assert_eq!(
            reason(&first, &bootleg),
            Some("it is unofficial".to_string())
        );
        // Not when the first candidate is just as unofficial.
        let mut unofficial_first = first.clone();
        unofficial_first.release.formats = vec!["CD, Unofficial Release".to_string()];
        assert_eq!(reason(&unofficial_first, &unofficial), None);

        let mut vhs = exact.clone();
        vhs.release.formats = vec!["VHS".to_string()];
        assert_eq!(
            reason(&first, &vhs),
            Some("it is a video release".to_string())
        );
        let mut video_pair = exact.clone();
        video_pair.release.formats = vec!["DVD".to_string(), "Blu-ray".to_string()];
        assert_eq!(
            reason(&first, &video_pair),
            Some("it is a video release".to_string())
        );
        let mut video_first = first.clone();
        video_first.release.formats = vec!["DVD".to_string()];
        assert_eq!(reason(&video_first, &vhs), None);
        let mut cd_and_dvd = exact.clone();
        cd_and_dvd.release.formats = vec!["CD".to_string(), "DVD".to_string()];
        assert_eq!(reason(&first, &cd_and_dvd), None);
        let mut no_format = exact.clone();
        no_format.release.formats = Vec::new();
        assert_eq!(reason(&first, &no_format), None);
        assert!(!is_video_only(&no_format.release));
    }

    #[test]
    fn the_fit_is_not_compared_when_the_files_are_not_the_first_tracks() {
        let (local, first, mut exact) = reference_pair(4, 0.0, 0.0);
        assert!(leading_tracks(&local, &first).is_none());
        for track in &mut exact.release.tracklist {
            track.duration_secs = None;
        }
        exact.score.track_titles = first.score.track_titles - 0.3;
        assert_eq!(worse_reference(&local, &first, &exact), None);
        assert!(lists_exactly_the_files(&local, &first, &exact));
    }

    #[test]
    fn only_a_no_worse_exact_edition_blocks_the_first_tracks_rule() {
        let (local, first, exact) = reference_pair(10, 0.0, 0.0);
        let blocker = "release discogs 43 lists exactly the local tracks".to_string();
        let evaluation = evaluate_for_matching(
            &local,
            &first,
            std::slice::from_ref(&exact),
            &DiscoveryEvidence::default(),
        );
        assert!(evaluation.blockers.contains(&blocker));
        assert!(!evaluation.auto_accept);

        let mut lengthless = exact.clone();
        for track in &mut lengthless.release.tracklist {
            track.duration_secs = None;
        }
        let evaluation = evaluate_for_matching(
            &local,
            &first,
            std::slice::from_ref(&lengthless),
            &DiscoveryEvidence::default(),
        );
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("lists exactly")),
            "{:?}",
            evaluation.blockers
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(leading_warning(&evaluation).is_some());
    }

    #[test]
    fn an_edition_that_lists_exactly_the_files_blocks_the_longer_one() {
        let (local, longer) = leading_pair(10, 12);
        let (_, mut exact) = leading_pair(10, 10);
        exact.id = ReleaseId::Discogs(43);
        let exact_candidate = Candidate::new(&local, exact.clone());
        assert!(exact_candidate.mapping.complete);

        // Another edition of the same group.
        let mut longer_in_group = longer.clone();
        longer_in_group.group_id = Some(GroupId::DiscogsMaster(7));
        let mut exact_in_group = exact.clone();
        exact_in_group.group_id = Some(GroupId::DiscogsMaster(7));
        let exact_in_group = Candidate::new(&local, exact_in_group);
        let evaluation = album_evaluation(
            &local,
            &longer_in_group,
            std::slice::from_ref(&exact_in_group),
        );
        assert!(!evaluation.auto_accept);
        assert!(
            evaluation
                .blockers
                .contains(&"release discogs 43 lists exactly the local tracks".to_string()),
            "{:?}",
            evaluation.blockers
        );
        assert_eq!(leading_warning(&evaluation), None);

        // The exact edition itself is accepted, with no warning.
        let longer_candidate = Candidate::new(&local, longer_in_group);
        let evaluation = album_evaluation(
            &local,
            &exact_in_group.release,
            std::slice::from_ref(&longer_candidate),
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert_eq!(leading_warning(&evaluation), None);

        // An album of another group within the score gap does not: the files
        // are the first tracks of this one, and a near score says nothing about
        // which album they come from. The close-alternative gate still decides.
        let mut longer_of_master = longer.clone();
        longer_of_master.group_id = Some(GroupId::DiscogsMaster(7));
        let mut exact_of_master = exact.clone();
        exact_of_master.group_id = Some(GroupId::DiscogsMaster(8));
        let exact_of_master = Candidate::new(&local, exact_of_master);
        let evaluation = album_evaluation(
            &local,
            &longer_of_master,
            std::slice::from_ref(&exact_of_master),
        );
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("lists exactly")),
            "{:?}",
            evaluation.blockers
        );
        assert!(!lists_exactly_the_files(
            &local,
            &Candidate::new(&local, longer_of_master.clone()),
            &exact_of_master
        ));

        // A release with no group that is the same audio does: the provider
        // just does not file it with the longer one.
        let evaluation = album_evaluation(&local, &longer, std::slice::from_ref(&exact_candidate));
        assert!(
            evaluation
                .blockers
                .contains(&"release discogs 43 lists exactly the local tracks".to_string()),
            "{:?}",
            evaluation.blockers
        );
        // Unless it is not the same audio.
        let mut exact_other_title = exact.clone();
        exact_other_title.title = "Another Album".to_string();
        let other_title = Candidate::new(&local, exact_other_title);
        assert!(!lists_exactly_the_files(
            &local,
            &Candidate::new(&local, longer.clone()),
            &other_title
        ));
        // Not even when the leading shape is refused for another reason: the
        // files still open the first candidate.
        let mut ambiguous = Candidate::new(&local, longer_of_master);
        ambiguous.mapping.ambiguous = true;
        assert!(leading_tracks(&local, &ambiguous).is_none());
        assert!(!lists_exactly_the_files(
            &local,
            &ambiguous,
            &exact_of_master
        ));
        // Without the files opening it, the same pair counts.
        let mut not_leading = ambiguous.clone();
        not_leading.mapping.ambiguous = false;
        not_leading.mapping.pairs.swap(0, 1);
        assert!(leading_tracks(&local, &not_leading).is_none());
        assert!(lists_exactly_the_files(
            &local,
            &not_leading,
            &exact_of_master
        ));
        // And an album that does not agree in text never does.
        let mut off_artist = exact_of_master.clone();
        off_artist.score.album_artist = 0.857;
        assert!(!lists_exactly_the_files(&local, &not_leading, &off_artist));

        // An alternative that does not list the files exactly does not.
        let (_, mut other_longer) = leading_pair(10, 11);
        other_longer.id = ReleaseId::Discogs(44);
        other_longer.group_id = Some(GroupId::DiscogsMaster(7));
        let mut longer_in_group = longer.clone();
        longer_in_group.group_id = Some(GroupId::DiscogsMaster(7));
        let evaluation = album_evaluation(
            &local,
            &longer_in_group,
            &[Candidate::new(&local, other_longer)],
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(leading_warning(&evaluation).is_some());

        // A guard is only a blocker when the rule would have applied.
        let (local, mut longer) = leading_pair(10, 12);
        longer.tracklist[4].duration_secs = Some(190.0);
        let evaluation = album_evaluation(&local, &longer, std::slice::from_ref(&exact_candidate));
        assert!(
            !evaluation
                .blockers
                .iter()
                .any(|blocker| blocker.contains("lists exactly")),
            "{:?}",
            evaluation.blockers
        );
    }
}
