//! Pure matching logic: normalization, track assignment, scoring, and
//! automatic-acceptance gates.
//!
//! Nothing here touches the network or the database. Adapters convert their
//! service responses into [`RemoteRelease`] values, storage loads
//! [`LocalRelease`] values, and this module decides what may be applied.
//! Provider SDK types never reach this module.

use crate::providers::{GroupId, Mbid, ProviderKind, ReleaseId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const DISCOGS_ALGORITHM_VERSION: &str = "discogs-match-v4";
pub const MUSICBRAINZ_ALGORITHM_VERSION: &str = "musicbrainz-match-v3";

pub const SCORE_THRESHOLD: f64 = 0.95;
pub const RUNNER_UP_GAP: f64 = 0.05;
pub const TITLE_SIMILARITY_THRESHOLD: f64 = 0.90;
pub const DURATION_TOLERANCE_SECS: f64 = 3.0;

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
    /// Total tracks across all media, when the service reports it.
    #[serde(default)]
    pub track_count: Option<u32>,
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
    similarity_of_normalized(a, &normalized_chars(a), b, &normalized_chars(b))
}

fn normalized_chars(text: &str) -> Vec<char> {
    normalize_text(text).chars().collect()
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
    pub pair_similarity: Vec<Option<(usize, f64)>>,
}

impl Mapping {
    /// Title similarity between a local track and the remote track it is being
    /// scored against, or `None` when the local file has no title. Reuses the
    /// recorded value only when it was measured for this same remote track, so
    /// edited pairs are recomputed.
    fn title_similarity(
        &self,
        local_index: usize,
        remote_index: usize,
        local_track: &LocalTrack,
        remote_track: &RemoteTrack,
    ) -> Option<f64> {
        let title = local_track.title.as_deref()?;
        if let Some(Some((recorded_remote, recorded))) = self.pair_similarity.get(local_index)
            && *recorded_remote == remote_index
            && (0.0..=1.0).contains(recorded)
        {
            return Some(*recorded);
        }
        Some(similarity(title, &remote_track.title))
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

/// Title similarities for one `map_tracks` call. Each title is normalized once
/// and each pair is compared at most once, however many passes ask for it.
struct TitleMatrix<'a> {
    local: &'a LocalRelease,
    remote: &'a RemoteRelease,
    local_chars: Vec<Vec<char>>,
    remote_chars: Vec<Vec<char>>,
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
            cache: vec![vec![None; remote.tracklist.len()]; local.tracks.len()],
        }
    }

    /// `None` when the local file has no title.
    fn get(&mut self, local_index: usize, remote_index: usize) -> Option<f64> {
        let title = self.local.tracks[local_index].title.as_deref()?;
        if let Some(known) = self.cache[local_index][remote_index] {
            return Some(known);
        }
        let value = similarity_of_normalized(
            title,
            &self.local_chars[local_index],
            &self.remote.tracklist[remote_index].title,
            &self.remote_chars[remote_index],
        );
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
        Some(title) if !title.trim().is_empty() => similarity(title, &remote.title),
        _ => 0.0,
    };

    let album_artist = album_artist_score(local, remote);

    let track_titles = if local.tracks.is_empty() {
        0.0
    } else {
        let mut total = 0.0;
        for (index, track) in local.tracks.iter().enumerate() {
            let Some(remote_index) = mapping.pairs.get(index).and_then(|pair| *pair) else {
                continue;
            };
            let Some(remote_track) = remote.tracklist.get(remote_index) else {
                continue;
            };
            if let Some(similarity) =
                mapping.title_similarity(index, remote_index, track, remote_track)
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
    let mut best = similarity_of_normalized(
        &local_original,
        &local_chars,
        &joined_original,
        &joined_chars,
    );
    for (original, chars) in remote_originals.iter().zip(&remote_chars) {
        best = best.max(similarity_of_normalized(
            &local_original,
            &local_chars,
            original,
            chars,
        ));
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
        rank += 0.5 * similarity(title, &summary.title);
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
        return artist_similarity(local_artist, credit);
    }
    if !summary.artists.is_empty() {
        return artist_similarity(local_artist, &summary.artists.join(" "));
    }
    let (hint, _) = summary
        .title
        .split_once(" - ")
        .unwrap_or(("", &summary.title));
    artist_similarity(local_artist, hint)
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
    evaluate_at_scope(
        local,
        candidate,
        runner_up,
        discovery,
        MatchScope::Edition,
        AcceptancePolicy::Measured,
    )
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
    let mut album = evaluate_at_scope(
        local,
        candidate,
        None,
        discovery,
        MatchScope::Album,
        AcceptancePolicy::Measured,
    );
    for other in alternatives {
        if other.release.id != candidate.release.id
            && within_gap(candidate.score.album_score() - other.score.album_score())
            && !equivalent_audio(candidate, other)
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

/// Applies album gates after a high-confidence semantic agreement.
pub fn evaluate_ai_album(
    local: &LocalRelease,
    candidate: &Candidate,
    alternatives: &[Candidate],
) -> Evaluation {
    let mut album = evaluate_at_scope(
        local,
        candidate,
        None,
        &DiscoveryEvidence::default(),
        MatchScope::Album,
        AcceptancePolicy::Semantic,
    );
    for other in alternatives {
        if (!other.score.album_score().is_finite()
            || within_gap(candidate.score.album_score() - other.score.album_score()))
            && !compatible_album_audio(candidate, other)
        {
            album.blockers.push(format!(
                "close alternative {} does not establish compatible known album audio",
                other.release.id
            ));
        }
    }
    album.auto_accept = album.blockers.is_empty();
    album
}

/// Checks known audio independently of spelling and artist-credit variations.
/// Release and release-track IDs may differ between editions of the same album.
pub fn compatible_album_audio(a: &Candidate, b: &Candidate) -> bool {
    let row_count = a.mapping.pairs.len();
    if !supported_album_mapping(a, row_count)
        || !supported_album_mapping(b, row_count)
        || remote_edition_qualifiers(&a.release) != remote_edition_qualifiers(&b.release)
    {
        return false;
    }
    if a.release.provider() == b.release.provider()
        && a.release.group_id.is_some()
        && b.release.group_id.is_some()
        && a.release.group_id != b.release.group_id
    {
        return false;
    }
    a.mapping
        .assigned_pairs()
        .all(|(local_index, remote_index)| {
            // Both mappings have been checked for bounds, complete coverage, and uniqueness.
            let other_index = b.mapping.pairs[local_index].expect("validated mapping");
            let x = &a.release.tracklist[remote_index];
            let y = &b.release.tracklist[other_index];
            edition_qualifiers(&x.title) == edition_qualifiers(&y.title)
                && matches!((x.duration_secs, y.duration_secs), (Some(x), Some(y))
            if x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0
                && (x - y).abs() <= DURATION_TOLERANCE_SECS)
                && !matches!((x.recording_id, y.recording_id), (Some(x), Some(y)) if x != y)
        })
}

/// Checks alternatives outside the semantic review request budget.
/// Each mapped recording needs matching text and credits or a shared recording
/// ID. Track version qualifiers must agree even when recording IDs match.
pub fn equivalent_unassessed_audio(a: &Candidate, b: &Candidate) -> bool {
    if !compatible_album_audio(a, b) {
        return false;
    }
    a.mapping
        .assigned_pairs()
        .all(|(local_index, remote_index)| {
            let other_index = b.mapping.pairs[local_index].expect("validated mapping");
            let x = &a.release.tracklist[remote_index];
            let y = &b.release.tracklist[other_index];
            matches!((x.recording_id, y.recording_id), (Some(x), Some(y)) if x == y)
                || (normalize_text(&x.title) == normalize_text(&y.title)
                    && normalized_track_credit(&a.release, x)
                        == normalized_track_credit(&b.release, y))
        })
}

fn normalized_track_credit(release: &RemoteRelease, track: &RemoteTrack) -> String {
    let credit = track.artist_credit.clone().unwrap_or_else(|| {
        if track.artists.is_empty() {
            release.artist_display()
        } else {
            track.artist_display()
        }
    });
    normalize_artist_for(release.provider(), &credit)
}

/// Do not trust a serialized mapping's summary flags without checking its pairs.
fn supported_album_mapping(candidate: &Candidate, row_count: usize) -> bool {
    if row_count == 0
        || !candidate.mapping.complete
        || candidate.mapping.ambiguous
        || candidate.release.has_unsupported_entries()
        || candidate.mapping.pairs.len() != row_count
        || candidate.release.playable_tracks().len() != row_count
    {
        return false;
    }
    let mut targets = HashSet::new();
    candidate.mapping.pairs.iter().all(|pair| {
        pair.is_some_and(|index| {
            candidate
                .release
                .tracklist
                .get(index)
                .is_some_and(RemoteTrack::is_playable)
                && targets.insert(index)
        })
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AcceptancePolicy {
    Measured,
    Semantic,
}

impl AcceptancePolicy {
    fn minimum_score(self) -> f64 {
        match self {
            Self::Measured => SCORE_THRESHOLD,
            Self::Semantic => 0.85,
        }
    }

    fn minimum_title_similarity(self) -> f64 {
        match self {
            Self::Measured => TITLE_SIMILARITY_THRESHOLD,
            Self::Semantic => 0.50,
        }
    }
}

/// Track version qualifiers that distinguish recordings. A remastering is a
/// different mastering of the same recording, so it never separates tracks.
fn album_track_qualifiers(title: &str) -> BTreeSet<&'static str> {
    let mut qualifiers = edition_qualifiers(title);
    qualifiers.remove("remastered");
    qualifiers
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
            normalize_text(&x.title) == normalize_text(&y.title)
                && normalize_artist_for(a.release.provider(), &x_artist)
                    == normalize_artist_for(b.release.provider(), &y_artist)
                && matches!((x.duration_secs, y.duration_secs), (Some(x), Some(y))
                if x.is_finite() && y.is_finite() && (x-y).abs() <= DURATION_TOLERANCE_SECS)
                && !matches!((x.recording_id, y.recording_id), (Some(x), Some(y)) if x != y)
        })
}

fn evaluate_at_scope(
    local: &LocalRelease,
    candidate: &Candidate,
    runner_up: Option<&Candidate>,
    discovery: &DiscoveryEvidence,
    match_scope: MatchScope,
    policy: AcceptancePolicy,
) -> Evaluation {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();

    let remote = &candidate.release;
    let provider = remote.provider();
    let minimum_score = policy.minimum_score();
    let minimum_title_similarity = policy.minimum_title_similarity();

    if policy == AcceptancePolicy::Semantic {
        evidence.push("high-confidence semantic agreement on album identity".to_string());
        if !supported_album_mapping(candidate, local.tracks.len()) {
            blockers.push(
                "album matching requires a supported, complete, unambiguous one-to-one mapping"
                    .to_string(),
            );
        }
        if local
            .album_title
            .as_deref()
            .is_none_or(|title| title.trim().is_empty())
            || local
                .album_artist
                .as_deref()
                .is_none_or(|artist| artist.trim().is_empty())
        {
            blockers.push(
                "album matching requires a nonempty local album title and artist".to_string(),
            );
        }
    }

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

    let acceptance_score = match_scope.score(&candidate.score);
    if acceptance_score + 1e-9 < minimum_score
        || (policy == AcceptancePolicy::Semantic && !acceptance_score.is_finite())
    {
        blockers.push(format!(
            "score {:.3} is below the {:.2} acceptance threshold",
            acceptance_score, minimum_score
        ));
    }

    if match_scope == MatchScope::Album
        && (candidate.score.album_title < minimum_title_similarity
            || candidate.score.album_artist < minimum_title_similarity)
    {
        blockers.push("album title or artist does not agree closely enough".to_string());
    }

    if let Some(runner_up) = runner_up {
        let gap = candidate.score.total - runner_up.score.total;
        if gap + 1e-9 < RUNNER_UP_GAP && !embedded_consistent {
            blockers.push(format!(
                "runner-up release {} is only {:.3} behind",
                runner_up.release.id, gap
            ));
        }
    }

    // Per-track evidence.
    let mut worst_title = 1.0f64;
    let mut worst_title_track = String::new();
    let mut worst_duration: Option<f64> = None;
    let mut worst_duration_track = String::new();

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

        if let Some(score) =
            candidate
                .mapping
                .title_similarity(local_index, remote_index, local_track, remote_track)
            && score < worst_title
        {
            worst_title = score;
            worst_title_track = remote_track.title.clone();
        }

        if let (Some(a), Some(b)) = (local_track.duration_secs, remote_track.duration_secs) {
            let difference = (a - b).abs();
            if worst_duration.is_none_or(|worst| difference > worst) {
                worst_duration = Some(difference);
                worst_duration_track = remote_track.title.clone();
            }
        }
    }

    if worst_title < minimum_title_similarity {
        blockers.push(format!(
            "track title '{worst_title_track}' only scores {worst_title:.3} against the local file"
        ));
    }

    if let Some(difference) = worst_duration
        && difference > DURATION_TOLERANCE_SECS
    {
        blockers.push(format!(
            "track '{worst_duration_track}' differs by {difference:.1}s from the {provider} duration"
        ));
    }

    // Embedded identifier contradictions. Provider IDs are only ever compared
    // within their own namespace.
    if provider == ProviderKind::MusicBrainz || policy == AcceptancePolicy::Semantic {
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
            track_count: None,
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
        assert_eq!(algorithm_version(ProviderKind::Discogs), "discogs-match-v4");
        assert_eq!(
            algorithm_version(ProviderKind::MusicBrainz),
            "musicbrainz-match-v3"
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
    fn ai_album_rescues_modest_text_mismatch_without_claiming_an_edition() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_discogs_id = Some(42);
        let mut remote = remote_release(&["Ones", "Two"]);
        remote.title = "Tast Album".to_string();
        remote.artists = vec!["Tast Artist".to_string()];
        let candidate = Candidate::new(&local, remote);
        let original_score = candidate.score;
        assert!(
            !evaluate_for_matching(&local, &candidate, &[], &DiscoveryEvidence::default())
                .auto_accept
        );
        let result = evaluate_ai_album(&local, &candidate, &[]);
        assert!(result.auto_accept, "{:?}", result.blockers);
        assert_eq!(result.match_scope, MatchScope::Album);
        assert!(result.evidence.iter().any(|note| note.contains("semantic")));
        assert!(
            result
                .warnings
                .iter()
                .any(|note| note.contains("exact edition unconfirmed"))
        );
        assert_eq!(candidate.score, original_score);
    }

    #[test]
    fn ai_album_requires_minimum_measured_album_score() {
        let local = local_release(&["One", "Two"]);
        let mut remote = remote_release(&["Ones", "Twos"]);
        remote.title = "Best Albums".to_string();
        remote.artists = vec!["Best Artists".to_string()];
        let candidate = Candidate::new(&local, remote);
        assert!(candidate.score.album_score() < 0.85);
        assert!(!evaluate_ai_album(&local, &candidate, &[]).auto_accept);
    }

    #[test]
    fn ai_album_rejects_low_album_title_and_artist_similarity() {
        let mut local = local_release(&["One"]);
        local.album_title = Some("a".repeat(100));
        local.album_artist = Some("b".repeat(100));
        for change_title in [true, false] {
            let mut remote = remote_release(&["One"]);
            remote.title = if change_title {
                format!("{}{}", "a".repeat(49), "x".repeat(51))
            } else {
                "a".repeat(100)
            };
            remote.artists = vec![if change_title {
                "b".repeat(100)
            } else {
                format!("{}{}", "b".repeat(49), "x".repeat(51))
            }];
            let candidate = Candidate::new(&local, remote);
            assert!(candidate.score.album_score() >= 0.85);
            let result = evaluate_ai_album(&local, &candidate, &[]);
            assert!(!result.auto_accept);
            assert!(
                result
                    .blockers
                    .iter()
                    .any(|reason| reason.contains("album title or artist"))
            );
        }
    }

    #[test]
    fn ai_album_checks_each_title_instead_of_only_the_average() {
        let local = local_release(&["One", "Two", "Three", "Four", "Five", "Six"]);
        let remote = remote_release(&["ZZZ", "Two", "Three", "Four", "Five", "Six"]);
        let candidate = Candidate::new(&local, remote);
        assert!(candidate.score.album_score() > 0.85);
        let result = evaluate_ai_album(&local, &candidate, &[]);
        assert!(!result.auto_accept);
        assert!(
            result
                .blockers
                .iter()
                .any(|reason| reason.contains("track title"))
        );
    }

    #[test]
    fn album_scope_blocks_track_version_qualifier_mismatches() {
        let local = local_release(&["Yesterday", "Michelle"]);
        let remote = remote_release(&["Yesterday (Live)", "Michelle (Live)"]);
        let candidate = Candidate::new(&local, remote);
        let result = evaluate_ai_album(&local, &candidate, &[]);
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
        let result = evaluate_ai_album(&local, &candidate, &[]);
        assert!(result.auto_accept, "{:?}", result.blockers);
    }

    #[test]
    fn ai_album_keeps_every_non_text_gate() {
        for problem in [
            "album missing",
            "artist blank",
            "track title missing",
            "remote title blank",
            "local duration missing",
            "remote duration missing",
            "zero duration",
            "negative duration",
            "nan duration",
            "infinite duration",
            "long duration",
            "incomplete",
            "ambiguous",
            "unsupported",
            "structure",
            "barcode",
            "catalog",
            "qualifier",
            "embedded release",
            "release group",
            "recording",
            "release track",
            "release identifier problem",
            "track identifier problem",
        ] {
            let mut local = local_release(&["One", "Two"]);
            let mut remote = remote_release(&["One", "Two"]);
            let id_a = mbid("11111111-1111-1111-1111-111111111111");
            let id_b = mbid("22222222-2222-2222-2222-222222222222");
            match problem {
                "album missing" => local.album_title = None,
                "artist blank" => local.album_artist = Some("  ".to_string()),
                "track title missing" => local.tracks[0].title = None,
                "remote title blank" => remote.tracklist[0].title = "  ".to_string(),
                "local duration missing" => local.tracks[0].duration_secs = None,
                "remote duration missing" => remote.tracklist[0].duration_secs = None,
                "zero duration" => remote.tracklist[0].duration_secs = Some(0.0),
                "negative duration" => local.tracks[0].duration_secs = Some(-1.0),
                "nan duration" => remote.tracklist[0].duration_secs = Some(f64::NAN),
                "infinite duration" => local.tracks[0].duration_secs = Some(f64::INFINITY),
                "long duration" => remote.tracklist[0].duration_secs = Some(183.01),
                "incomplete" => {
                    remote.tracklist.pop();
                }
                "ambiguous" => {
                    for track in &mut local.tracks {
                        track.title = Some("Intro".to_string());
                        track.track_number = None;
                        track.duration_secs = Some(180.0);
                    }
                    for track in &mut remote.tracklist {
                        track.title = "Intro".to_string();
                        track.position = None;
                        track.duration_secs = Some(180.0);
                    }
                }
                "unsupported" => remote.tracklist[0].entry_type = TrackEntryType::Index,
                "structure" => remote.structure_issues.push("pregap".to_string()),
                "barcode" => local.barcode = Some("9999999999999".to_string()),
                "catalog" => local.catalog_number = Some("OTHER".to_string()),
                "qualifier" => remote.edition_notes.push("Live".to_string()),
                "embedded release" => local.embedded_discogs_id = Some(999),
                "release group" => {
                    remote.id = ReleaseId::MusicBrainz(id_a);
                    remote.group_id = Some(GroupId::MusicBrainzReleaseGroup(id_a));
                    local.embedded_evidence.release_group_id = Some(id_b);
                }
                "recording" => {
                    local.tracks[0].recording_id = Some(id_a);
                    remote.tracklist[0].recording_id = Some(id_b);
                }
                "release track" => {
                    local.tracks[0].release_track_id = Some(id_a);
                    remote.tracklist[0].release_track_id = Some(id_b);
                }
                "release identifier problem" => local
                    .embedded_evidence
                    .problems
                    .push("conflicting observed release IDs".to_string()),
                "track identifier problem" => local.tracks[0]
                    .identifier_problems
                    .push("malformed observed recording ID".to_string()),
                _ => unreachable!(),
            }
            let candidate = Candidate::new(&local, remote);
            assert!(
                !evaluate_ai_album(&local, &candidate, &[]).auto_accept,
                "accepted {problem}"
            );
        }
    }

    #[test]
    fn ai_album_rejects_invalid_mapping_payloads_without_panicking() {
        let local = local_release(&["One", "Two"]);
        let baseline = Candidate::new(&local, remote_release(&["One", "Two"]));
        for pairs in [
            vec![Some(0), Some(0)],
            vec![Some(0), Some(99)],
            vec![Some(0)],
            vec![Some(0), None],
        ] {
            let mut candidate = baseline.clone();
            candidate.mapping.pairs = pairs;
            assert!(!evaluate_ai_album(&local, &candidate, &[]).auto_accept);
            assert!(!compatible_album_audio(&baseline, &candidate));
            assert!(!compatible_album_audio(&candidate, &baseline));
        }
    }

    #[test]
    fn ai_audio_compatibility_allows_credit_spelling_and_cross_provider_group_differences() {
        let local = local_release(&["One", "Two"]);
        let mut first = remote_release(&["One", "Two"]);
        first.group_id = Some(GroupId::DiscogsMaster(1));
        let first = Candidate::new(&local, first);
        let mut other = remote_release(&["Ones", "Twos"]);
        other.id = ReleaseId::MusicBrainz(mbid("11111111-1111-1111-1111-111111111111"));
        other.group_id = Some(GroupId::MusicBrainzReleaseGroup(mbid(
            "22222222-2222-2222-2222-222222222222",
        )));
        other.title = "Tast Album".to_string();
        other.artists = vec!["Tast Artist".to_string()];
        other.tracklist[0].artist_credit = Some("Different spelling".to_string());
        let other = Candidate::new(&local, other);
        assert!(compatible_album_audio(&first, &other));
        assert!(compatible_album_audio(&other, &first));
    }

    #[test]
    fn ai_audio_compatibility_allows_edition_ids_but_checks_recordings() {
        let local = local_release(&["One", "Two"]);
        let id_a = mbid("11111111-1111-1111-1111-111111111111");
        let id_b = mbid("22222222-2222-2222-2222-222222222222");
        let mut first = remote_release(&["One", "Two"]);
        first.id = ReleaseId::MusicBrainz(id_a);
        first.tracklist[0].release_track_id = Some(id_a);
        first.tracklist[0].recording_id = Some(id_a);
        let first = Candidate::new(&local, first);
        let mut other = first.release.clone();
        other.id = ReleaseId::MusicBrainz(id_b);
        other.tracklist[0].release_track_id = Some(id_b);
        let other = Candidate::new(&local, other);
        assert!(compatible_album_audio(&first, &other));
        let mut conflicting = other.clone();
        conflicting.release.tracklist[0].recording_id = Some(id_b);
        assert!(!compatible_album_audio(&first, &conflicting));
    }

    #[test]
    fn ai_audio_compatibility_refuses_unknown_audio_and_edition_conflicts() {
        let local = local_release(&["One", "Two"]);
        let mut release = remote_release(&["One", "Two"]);
        release.group_id = Some(GroupId::DiscogsMaster(1));
        let baseline = Candidate::new(&local, release);
        for problem in [
            "missing duration",
            "zero duration",
            "nan duration",
            "infinite duration",
            "timing",
            "group",
            "qualifier",
            "unsupported",
            "ambiguous",
            "incomplete",
            "row count",
        ] {
            let mut other = baseline.clone();
            match problem {
                "missing duration" => other.release.tracklist[0].duration_secs = None,
                "zero duration" => other.release.tracklist[0].duration_secs = Some(0.0),
                "nan duration" => other.release.tracklist[0].duration_secs = Some(f64::NAN),
                "infinite duration" => {
                    other.release.tracklist[0].duration_secs = Some(f64::INFINITY)
                }
                "timing" => other.release.tracklist[0].duration_secs = Some(183.01),
                "group" => other.release.group_id = Some(GroupId::DiscogsMaster(2)),
                "qualifier" => other.release.formats.push("Remastered".to_string()),
                "unsupported" => other.release.structure_issues.push("pregap".to_string()),
                "ambiguous" => other.mapping.ambiguous = true,
                "incomplete" => other.mapping.complete = false,
                "row count" => {
                    other.mapping.pairs.pop();
                }
                _ => unreachable!(),
            }
            assert!(
                !compatible_album_audio(&baseline, &other),
                "accepted {problem}"
            );
            assert!(
                !compatible_album_audio(&other, &baseline),
                "accepted reversed {problem}"
            );
        }
    }

    #[test]
    fn ai_album_checks_all_close_and_higher_alternatives() {
        let local = local_release(&["One", "Two"]);
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        for score in [1.0, 0.96, 0.95] {
            let mut compatible = first.clone();
            compatible.release.id = ReleaseId::Discogs(43);
            let mut conflicting = first.clone();
            conflicting.release.id = ReleaseId::Discogs(44);
            conflicting.release.tracklist[0].duration_secs = None;
            // Only the score determines whether an alternative is close;
            // its actual incomplete audio evidence must still block acceptance.
            conflicting.score.album_title = score;
            conflicting.score.album_artist = score;
            conflicting.score.track_titles = score;
            conflicting.score.durations = score;
            let result = evaluate_ai_album(&local, &first, &[compatible, conflicting]);
            assert!(!result.auto_accept, "accepted alternative at score {score}");
            assert!(result.blockers.iter().any(|reason| reason.contains("44")));
        }
        let mut selected = first.clone();
        selected.score.album_title = 0.9;
        let mut higher = first.clone();
        higher.release.id = ReleaseId::Discogs(45);
        higher.release.structure_issues.push("pregap".to_string());
        assert!(!evaluate_ai_album(&local, &selected, &[higher]).auto_accept);
    }

    #[test]
    fn ai_album_accepts_compatible_twins_and_ignores_distant_conflicts() {
        let local = local_release(&["One", "Two"]);
        let first = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut twin = first.clone();
        twin.release.id = ReleaseId::Discogs(43);
        twin.release.title = "Tast Album".to_string();
        let mut distant = first.clone();
        distant.release.id = ReleaseId::Discogs(44);
        distant.release.tracklist[0].duration_secs = None;
        distant.score.album_title = 0.8;
        distant.score.album_artist = 0.8;
        distant.score.track_titles = 0.8;
        distant.score.durations = 0.8;
        assert!(evaluate_ai_album(&local, &first, &[twin, distant]).auto_accept);
    }

    #[test]
    fn ai_album_accepts_the_text_floor_and_duration_boundary() {
        let mut local = local_release(&["One", "Two"]);
        local.album_title = Some("aaaaaaaaaa".to_string());
        let mut remote = remote_release(&["One", "Two"]);
        remote.title = "aaaaaxxxxx".to_string();
        let candidate = Candidate::new(&local, remote);
        assert!(evaluate_ai_album(&local, &candidate, &[]).auto_accept);

        let local = local_release(&["One", "Two"]);
        let mut remote = remote_release(&["One", "Two"]);
        remote.tracklist[0].duration_secs = Some(183.0);
        let candidate = Candidate::new(&local, remote);
        assert!(evaluate_ai_album(&local, &candidate, &[]).auto_accept);
        assert!(compatible_album_audio(
            &Candidate::new(&local, remote_release(&["One", "Two"])),
            &candidate,
        ));
    }

    #[test]
    fn ai_album_never_upgrades_to_an_exact_edition() {
        let mut local = local_release(&["One", "Two"]);
        local.embedded_discogs_id = Some(42);
        let candidate = Candidate::new(&local, remote_release(&["One", "Two"]));
        assert!(evaluate(&local, &candidate, None, &DiscoveryEvidence::default()).auto_accept);
        let evaluation = evaluate_ai_album(&local, &candidate, &[]);
        assert!(evaluation.auto_accept);
        assert_eq!(evaluation.match_scope, MatchScope::Album);
    }

    #[test]
    fn ai_audio_compatibility_rejects_mapped_track_version_disagreements() {
        let local = local_release(&["One", "Two"]);
        let recording = mbid("11111111-1111-1111-1111-111111111111");
        for qualifier in ["Live", "Instrumental"] {
            for recording_id in [None, Some(recording)] {
                let mut first = Candidate::new(&local, remote_release(&["One", "Two"]));
                first.release.tracklist[1].recording_id = recording_id;
                let mut other = first.clone();
                other.release.id = ReleaseId::Discogs(43);
                other.release.tracklist[1].title = format!("Two ({qualifier})");
                assert!(
                    !compatible_album_audio(&first, &other),
                    "accepted {qualifier} with {recording_id:?}"
                );
                assert!(!equivalent_unassessed_audio(&first, &other));
            }
        }
    }

    #[test]
    fn ai_album_blocks_close_twelve_track_alternative_with_a_changed_version() {
        let titles: Vec<String> = (1..=12).map(|n| format!("Track {n}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let mut local = local_release(&refs);
        local.tracks[0].title = Some("Track One".to_string());
        let selected = Candidate::new(&local, remote_release(&refs));
        let mut other_release = selected.release.clone();
        other_release.id = ReleaseId::Discogs(43);
        other_release.tracklist[11].title = "Entirely Different Recording (Live)".to_string();
        let other = Candidate::new(&local, other_release);
        assert!(selected.score.album_score() - other.score.album_score() < 0.05);
        assert!(!evaluate_ai_album(&local, &selected, &[other]).auto_accept);
    }

    #[test]
    fn unassessed_audio_requires_text_credit_agreement_or_a_shared_recording() {
        let local = local_release(&["One", "Two"]);
        let baseline = Candidate::new(&local, remote_release(&["One", "Two"]));
        for changed_field in ["title", "credit"] {
            let mut other = baseline.clone();
            other.release.id = ReleaseId::Discogs(43);
            match changed_field {
                "title" => {
                    other.release.tracklist[1].title = "Entirely Different Recording".to_string()
                }
                "credit" => {
                    other.release.tracklist[1].artists = vec!["Another Performer".to_string()]
                }
                _ => unreachable!(),
            }
            assert!(compatible_album_audio(&baseline, &other));
            assert!(
                !equivalent_unassessed_audio(&baseline, &other),
                "accepted unassessed {changed_field}"
            );
            let mut with_recording = baseline.clone();
            let recording = mbid("11111111-1111-1111-1111-111111111111");
            with_recording.release.tracklist[1].recording_id = Some(recording);
            other.release.tracklist[1].recording_id = Some(recording);
            assert!(equivalent_unassessed_audio(&with_recording, &other));
            other.release.tracklist[1].recording_id =
                Some(mbid("22222222-2222-2222-2222-222222222222"));
            assert!(!equivalent_unassessed_audio(&with_recording, &other));
        }
    }

    #[test]
    fn unassessed_audio_compares_normalized_titles_and_effective_credits() {
        let local = local_release(&["One", "Two"]);
        let baseline = Candidate::new(&local, remote_release(&["One", "Two"]));
        let mut other = baseline.clone();
        other.release.id = ReleaseId::Discogs(43);
        other.release.tracklist[0].title = "ONE!".to_string();
        other.release.tracklist[0].artist_credit = Some("Test Artist (2)".to_string());
        other.release.tracklist[1].artists = vec!["TEST ARTIST".to_string()];
        assert!(equivalent_unassessed_audio(&baseline, &other));
        assert!(equivalent_unassessed_audio(&other, &baseline));
        other.release.tracklist[1].artists = vec!["Different Artist".to_string()];
        assert!(!equivalent_unassessed_audio(&baseline, &other));
    }
}
