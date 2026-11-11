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

pub const DISCOGS_ALGORITHM_VERSION: &str = "discogs-match-v2";
pub const MUSICBRAINZ_ALGORITHM_VERSION: &str = "musicbrainz-match-v1";

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
pub fn similarity(a: &str, b: &str) -> f64 {
    let a = normalize_text(a);
    let b = normalize_text(b);
    similarity_normalized(&a, &b)
}

fn similarity_normalized(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let distance = levenshtein(&a_chars, &b_chars);
    let longest = a_chars.len().max(b_chars.len()) as f64;

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
    similarity_normalized(&normalize_artist(a), &normalize_artist(b))
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
}

impl Mapping {
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

    Mapping {
        pairs,
        complete,
        ambiguous,
        notes,
    }
}

fn pair_cost(local: &LocalTrack, remote: &RemoteTrack) -> f64 {
    let title = match &local.title {
        Some(title) => similarity(title, &remote.title),
        None => 0.0,
    };

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
            if let Some(title) = &track.title {
                total += similarity(title, &remote_track.title);
            }
        }
        total / local.tracks.len() as f64
    };

    let mut duration_total = 0.0;
    let mut duration_count = 0usize;
    for (local_index, remote_index) in mapping.assigned_pairs() {
        let Some(local_track) = local.tracks.get(local_index) else {
            continue;
        };
        let Some(remote_track) = remote.tracklist.get(remote_index) else {
            continue;
        };
        if let (Some(a), Some(b)) = (local_track.duration_secs, remote_track.duration_secs) {
            duration_total += duration_similarity(a, b);
            duration_count += 1;
        }
    }
    let durations = if duration_count == 0 {
        0.0
    } else {
        duration_total / duration_count as f64
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
    let local_normalized = normalize_artist_for(provider, artist);
    let remote_normalized: Vec<String> = remote
        .artists
        .iter()
        .map(|name| normalize_artist_for(provider, name))
        .collect();

    let various = matches!(local.artist_kind, ArtistKind::VariousArtists)
        && remote_normalized
            .iter()
            .any(|name| name == "various" || name == "various artists");
    if various {
        return 1.0;
    }

    let joined = remote_normalized.join(" ");
    let mut best = similarity_normalized(&local_normalized, &joined);
    for name in &remote_normalized {
        best = best.max(similarity_normalized(&local_normalized, name));
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub auto_accept: bool,
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
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();

    let remote = &candidate.release;
    let provider = remote.provider();

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

    if candidate.score.total + 1e-9 < SCORE_THRESHOLD {
        blockers.push(format!(
            "score {:.3} is below the {:.2} acceptance threshold",
            candidate.score.total, SCORE_THRESHOLD
        ));
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
        let local_track = &local.tracks[local_index];
        let remote_track = &remote.tracklist[remote_index];

        if let Some(title) = &local_track.title {
            let score = similarity(title, &remote_track.title);
            if score < worst_title {
                worst_title = score;
                worst_title_track = remote_track.title.clone();
            }
        }

        if let (Some(a), Some(b)) = (local_track.duration_secs, remote_track.duration_secs) {
            let difference = (a - b).abs();
            if worst_duration.is_none_or(|worst| difference > worst) {
                worst_duration = Some(difference);
                worst_duration_track = remote_track.title.clone();
            }
        }
    }

    if worst_title < TITLE_SIMILARITY_THRESHOLD {
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

    if let Some(barcode) = &local.barcode {
        let local_digits = normalize_barcode(barcode);
        let remote_digits: Vec<String> = remote
            .barcodes
            .iter()
            .map(|b| normalize_barcode(b))
            .collect();
        if !local_digits.is_empty() && !remote_digits.is_empty() {
            if remote_digits.contains(&local_digits) {
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
    if embedded_consistent {
        // Already recorded as evidence above.
    } else if discovery.identifier_unique {
        evidence.push("barcode or catalog number uniquely identifies this release".to_string());
    } else {
        blockers.push(
            "no embedded ID, barcode, or catalog number distinguishes this edition".to_string(),
        );
    }

    if discovery.identifier_search_truncated {
        blockers.push("identifier search results were truncated".to_string());
    }

    for blocker in &discovery.blockers {
        blockers.push(blocker.clone());
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
        blockers,
        warnings,
        evidence,
    }
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
        if !primary.mapping.complete || primary.score.total + 1e-9 < PLAUSIBLE_PRIMARY_SCORE {
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
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
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
        let local = local_release(&["One", "Two"]);
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
        let local = local_release(&["One", "Two"]);
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
    fn algorithm_versions_are_provider_specific() {
        assert_eq!(algorithm_version(ProviderKind::Discogs), "discogs-match-v2");
        assert_eq!(
            algorithm_version(ProviderKind::MusicBrainz),
            "musicbrainz-match-v1"
        );
    }
}
