//! Which files of a folder form one local release.
//!
//! A *unit* is a folder, or a folder together with its disc subfolders
//! (`CD1`, `Disc_2`) when it has two or more of them. Today's grouping splits
//! a unit by album title. A unit can instead be *merged* into one release,
//! either by the user's decision (`folder_groupings`) or by [`auto_merges`].
//! Everything here is pure: the store gathers the files and applies the result.

// Rust guideline compliant 2026-08-14

use crate::matcher;
use crate::scene;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The `album_title_norm` of a merged release. Title keys are
/// `normalize_text` output (letters, digits and spaces) or start with `~`, so
/// no album title can produce it.
pub const MERGED_KEY: &str = "*";

/// A user's grouping decision for a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderMode {
    /// Every file of the unit forms one release.
    Merge,
    /// Files group by folder and album title, as without merging.
    Separate,
}

impl FolderMode {
    /// The value stored in `folder_groupings.mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Separate => "separate",
        }
    }

    /// Reads a stored mode; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "merge" => Some(Self::Merge),
            "separate" => Some(Self::Separate),
            _ => None,
        }
    }
}

/// What the user asks for a unit: a decision, or none (the automatic rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderAction {
    /// Store a decision to merge the unit into one release.
    Merge,
    /// Store a decision to keep the unit's releases apart.
    Separate,
    /// Remove the decision, so the automatic rule applies.
    Auto,
}

impl FolderAction {
    /// Every action, in the order the web interface offers them.
    pub const ALL: [Self; 3] = [Self::Merge, Self::Separate, Self::Auto];

    /// The decision this action stores; `None` removes the decision.
    pub fn mode(self) -> Option<FolderMode> {
        match self {
            Self::Merge => Some(FolderMode::Merge),
            Self::Separate => Some(FolderMode::Separate),
            Self::Auto => None,
        }
    }

    /// The form value: `merge`, `separate` or `auto`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Separate => "separate",
            Self::Auto => "auto",
        }
    }

    /// Reads a form value; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }

    /// The `radish folder` subcommand that does the same.
    pub fn cli_verb(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Separate => "split",
            Self::Auto => "auto",
        }
    }

    /// The button text on the folder page.
    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge into one release",
            Self::Separate => "Keep separate",
            Self::Auto => "Let Radish decide",
        }
    }
}

/// Disc and format words that, with a number in front, state how many discs a
/// folder holds (`2CD`, `3DVD`, `2disc`).
const DISC_COUNT_WORDS: &[&str] = &["cd", "dvd", "disc", "discs", "disk", "disks"];

/// The folder a file path is in, as grouping and the store compare it.
pub(crate) fn directory_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The folder above `directory`, or `None` at the top.
fn parent_of(directory: &str) -> Option<String> {
    let parent = Path::new(directory).parent()?.to_string_lossy().to_string();
    (!parent.is_empty()).then_some(parent)
}

/// The units of a set of folders: which folders hold two or more disc
/// subfolders (`matcher::folder_disc_number` names a disc).
#[derive(Debug, Default, Clone)]
pub struct Units {
    /// Disc parents by lowercased path, with the spelling first seen.
    disc_parents: HashMap<String, String>,
}

impl Units {
    /// The units of the folders `directories` names; order decides only
    /// which spelling of a folder is kept.
    pub fn new<'a>(directories: impl IntoIterator<Item = &'a str>) -> Self {
        let mut subfolders: HashMap<String, (String, HashSet<String>)> = HashMap::new();
        for directory in directories {
            if matcher::folder_disc_number(directory).is_none() {
                continue;
            }
            let Some(parent) = parent_of(directory) else {
                continue;
            };
            subfolders
                .entry(parent.to_ascii_lowercase())
                .or_insert_with(|| (parent, HashSet::new()))
                .1
                .insert(directory.to_ascii_lowercase());
        }
        let disc_parents = subfolders
            .into_iter()
            .filter(|(_, (_, discs))| discs.len() >= 2)
            .map(|(key, (spelling, _))| (key, spelling))
            .collect();
        Self { disc_parents }
    }

    /// The unit `directory` belongs to: the folder above it when it is a disc
    /// subfolder of a unit, otherwise the folder itself.
    pub fn unit_of(&self, directory: &str) -> String {
        if matcher::folder_disc_number(directory).is_some()
            && let Some(parent) = parent_of(directory)
            && let Some(spelling) = self.disc_parents.get(&parent.to_ascii_lowercase())
        {
            return spelling.clone();
        }
        directory.to_string()
    }
}

/// The folder above `root` when `root` itself is a disc subfolder (`CD1`).
///
/// Loads and listings use it to add the merged release of the folder above a
/// disc-subfolder scope without widening the scope itself. `None` when the last
/// component of `root` names no disc, or the parent is empty.
pub fn disc_parent_of(root: &Path) -> Option<PathBuf> {
    matcher::folder_disc_number(&root.to_string_lossy())?;
    root.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// The root a regroup or a load must cover to see whole units: a root that is
/// a disc subfolder widens, one level, to the folder above it.
pub fn unit_scope(root: &Path) -> PathBuf {
    disc_parent_of(root).unwrap_or_else(|| root.to_path_buf())
}

/// The disc a file is on inside a merged release of `unit`: its disc
/// subfolder's number, else the disc marker ending its own album tag
/// (`Shine 6 CD2`), else its disc tag. A folder or title number wins over a
/// conflicting tag, because per-disc rips often tag every disc as disc 1.
pub fn effective_disc(
    unit: &str,
    directory: &str,
    album: Option<&str>,
    disc_tag: Option<i64>,
) -> Option<i64> {
    if !directory.eq_ignore_ascii_case(unit)
        && let Some(number) = matcher::folder_disc_number(directory)
    {
        return Some(i64::from(number));
    }
    if let Some((_, number)) = album.and_then(matcher::title_disc_suffix) {
        return Some(i64::from(number));
    }
    disc_tag
}

/// How many discs a folder's name says it holds, from a tag word such as
/// `2CD` or `3DVD`, when it names exactly one count.
pub fn folder_disc_count(directory: &str) -> Option<u32> {
    let name = directory
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()?;
    let mut counts = BTreeSet::new();
    for word in name.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let lower = word.to_ascii_lowercase();
        let digits = lower.len()
            - lower
                .trim_start_matches(|ch: char| ch.is_ascii_digit())
                .len();
        if digits == 0 || digits > 2 || !DISC_COUNT_WORDS.contains(&&lower[digits..]) {
            continue;
        }
        if let Ok(count) = lower[..digits].parse::<u32>()
            && count >= 1
        {
            counts.insert(count);
        }
    }
    if counts.len() == 1 {
        counts.into_iter().next()
    } else {
        None
    }
}

/// The lowest title similarity, in either direction, at which two album titles
/// of one unit name the same album. The matcher's `0.90` gate misses one-letter
/// typos in short titles (`augmentes` scores `0.889` against `augmented`); the
/// number, edition and version checks in [`same_album`] keep `Shine 6` apart
/// from `Shine 7` all the same.
const SAME_ALBUM_SIMILARITY: f64 = 0.85;

/// A file as the automatic rule sees it.
#[derive(Debug, Clone)]
pub struct RuleFile<'a> {
    /// The file's path.
    pub path: &'a str,
    /// The album tag as read.
    pub album: Option<&'a str>,
    /// The disc number tag.
    pub disc_tag: Option<i64>,
    /// The track number tag.
    pub track_number: Option<i64>,
    /// The track title tag.
    pub title: Option<&'a str>,
    /// The track artist tag.
    pub artist: Option<&'a str>,
    /// The length in seconds, when known.
    pub duration_secs: Option<f64>,
}

/// The files of a unit that today's grouping puts in one release (one folder,
/// one album title), with the title as tagged.
#[derive(Debug, Clone)]
pub struct TitleGroup<'a> {
    pub title: &'a str,
    /// In path order.
    pub files: Vec<RuleFile<'a>>,
}

/// The title a merged release shows: the one held by the most files across
/// all groups, comparing titles by their normalized form. On a tie, the title
/// whose earliest file path (case-insensitive) sorts first wins. Returns the
/// spelling of the first group carrying the winning title.
pub fn main_title<'a>(groups: &[TitleGroup<'a>]) -> Option<&'a str> {
    struct Tally<'a> {
        key: String,
        title: &'a str,
        files: usize,
        first_path: String,
    }

    let mut tallies: Vec<Tally<'a>> = Vec::new();
    for group in groups {
        let mut key = matcher::normalize_text(group.title);
        if key.is_empty() {
            key = group.title.trim().to_lowercase();
        }
        let first_path = group
            .files
            .iter()
            .map(|file| file.path.to_ascii_lowercase())
            .min()
            .unwrap_or_default();
        match tallies.iter_mut().find(|tally| tally.key == key) {
            Some(tally) => {
                tally.files += group.files.len();
                if first_path < tally.first_path {
                    tally.first_path = first_path;
                }
            }
            None => tallies.push(Tally {
                key,
                title: group.title,
                files: group.files.len(),
                first_path,
            }),
        }
    }

    let mut best: Option<&Tally<'a>> = None;
    for tally in &tallies {
        let better = best.is_none_or(|current| {
            tally.files > current.files
                || (tally.files == current.files && tally.first_path < current.first_path)
        });
        if better {
            best = Some(tally);
        }
    }
    best.map(|tally| tally.title)
}

/// The album title of a merged release: the main title ([`main_title`])
/// without a trailing disc marker. `None` only when `groups` is empty.
pub fn merged_title(groups: &[TitleGroup<'_>]) -> Option<String> {
    let title = main_title(groups)?;
    Some(
        matcher::title_disc_suffix(title)
            .map_or_else(|| title.trim().to_string(), |(album, _)| album),
    )
}

/// A title reduced to the album it names: no trailing disc marker, then no
/// trailing release-type or format words.
fn reduced(title: &str) -> String {
    let base = matcher::title_disc_suffix(title)
        .map_or_else(|| title.trim().to_string(), |(album, _)| album);
    matcher::reduce_album_title(&base).unwrap_or(base)
}

/// Whether two album titles of one unit name the same album: similar enough
/// ([`SAME_ALBUM_SIMILARITY`]), with numbers that agree and the same edition
/// and version words.
pub fn same_album(a: &str, b: &str) -> bool {
    let (a, b) = (reduced(a), reduced(b));
    let similarity =
        matcher::album_title_similarity(&a, &b).max(matcher::album_title_similarity(&b, &a));
    similarity >= SAME_ALBUM_SIMILARITY
        && (matcher::album_numbers_agree(&a, &b) || matcher::album_numbers_agree(&b, &a))
        && matcher::edition_qualifiers(&a) == matcher::edition_qualifiers(&b)
        && matcher::version_words(&a) == matcher::version_words(&b)
}

/// The longest two lengths may differ by, in seconds, for two files with one
/// track title and artist to count as copies of each other.
const COPY_DURATION_TOLERANCE_SECS: f64 = 1.0;

/// A tag value as copies compare it: normalized, or when that leaves nothing
/// (`!!!`), trimmed and lowercased.
fn copy_key(text: &str) -> String {
    let key = matcher::normalize_text(text);
    if key.is_empty() {
        text.trim().to_lowercase()
    } else {
        key
    }
}

/// Whether two files share a normalized track title and track artist and have
/// lengths within [`COPY_DURATION_TOLERANCE_SECS`]. A missing artist matches
/// only a missing artist. Files without a usable title or without a known
/// length (positive and finite; the scanner stores 0.0 for an unmeasured file)
/// never count. Different mixes of one title differ in length, and
/// different artists who happen to share a title and length (`Freestyle`) are
/// no copies.
fn has_copies<'f, 'a: 'f>(files: impl Iterator<Item = &'f RuleFile<'a>>) -> bool {
    let mut lengths: HashMap<(String, Option<String>), Vec<f64>> = HashMap::new();
    for file in files {
        let (Some(title), Some(secs)) = (file.title, file.duration_secs) else {
            continue;
        };
        // The scanner stores 0.0 for a length it could not measure.
        if !(secs.is_finite() && secs > 0.0) {
            continue;
        }
        let title = copy_key(title);
        if title.is_empty() {
            continue;
        }
        let artist = file
            .artist
            .map(copy_key)
            .filter(|artist| !artist.is_empty());
        let seen = lengths.entry((title, artist)).or_default();
        if seen
            .iter()
            .any(|other| (other - secs).abs() <= COPY_DURATION_TOLERANCE_SECS)
        {
            return true;
        }
        seen.push(secs);
    }
    false
}

/// Whether the automatic rule merges a unit, given its groups with a usable
/// album title (untagged and junk-tagged files are not passed: they never
/// merge automatically). The caller has already checked that no release of
/// the unit holds an accepted match.
///
/// The unit merges when there are at least two groups and:
/// 1. every title names the same album as the main title, or both name the
///    same album as the title in the unit folder's scene name;
/// 2. no two files share an effective disc and track number, or the same
///    track title, artist and length (within a second): two copies of one file;
/// 3. when the files span more than one disc, the effective discs run from 1
///    with no gap; disc subfolders always name different discs;
/// 4. a disc count in the folder name (`2CD`) is met exactly.
pub fn auto_merges(unit: &str, groups: &[TitleGroup<'_>]) -> bool {
    if groups.len() < 2 {
        return false;
    }
    let Some(main) = main_title(groups) else {
        return false;
    };
    let folder_title = scene::parse_release_directory(unit)
        .and_then(|name| name.split())
        .and_then(|split| split.title);
    let names_main = |title: &str| {
        same_album(title, main)
            || folder_title
                .as_deref()
                .is_some_and(|folder| same_album(title, folder) && same_album(main, folder))
    };
    if !groups.iter().all(|group| names_main(group.title)) {
        return false;
    }

    let files: Vec<(&RuleFile<'_>, i64)> = groups
        .iter()
        .flat_map(|group| &group.files)
        .map(|file| {
            let disc = effective_disc(unit, &directory_of(file.path), file.album, file.disc_tag)
                .unwrap_or(1);
            (file, disc)
        })
        .collect();

    let mut positions = HashSet::new();
    for (file, disc) in &files {
        if let Some(track) = file.track_number
            && !positions.insert((*disc, track))
        {
            return false;
        }
    }

    if has_copies(files.iter().map(|(file, _)| *file)) {
        return false;
    }

    let discs: BTreeSet<i64> = files.iter().map(|(_, disc)| *disc).collect();
    if discs.len() > 1 && discs.iter().copied().ne(1..=discs.len() as i64) {
        return false;
    }
    let subfolders: HashSet<String> = files
        .iter()
        .map(|(file, _)| directory_of(file.path))
        .filter(|directory| !directory.eq_ignore_ascii_case(unit))
        .map(|directory| directory.to_ascii_lowercase())
        .collect();
    let subfolder_discs: HashSet<u32> = subfolders
        .iter()
        .filter_map(|directory| matcher::folder_disc_number(directory))
        .collect();
    if subfolder_discs.len() != subfolders.len() {
        return false;
    }

    if let Some(count) = folder_disc_count(unit)
        && discs.len() != count as usize
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn modes_and_actions_round_trip_their_names() {
        for mode in [FolderMode::Merge, FolderMode::Separate] {
            assert_eq!(FolderMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(FolderMode::parse("auto"), None);
        for action in FolderAction::ALL {
            assert_eq!(FolderAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(FolderAction::parse("split"), None);
        assert_eq!(FolderAction::Auto.mode(), None);
        assert_eq!(FolderAction::Separate.cli_verb(), "split");
    }

    #[test]
    fn disc_subfolders_form_a_unit_only_in_pairs() {
        let units = Units::new(["/m/X-2CD/cd1", "/m/X-2CD/CD2", "/m/Y/cd1", "/m/Z"]);
        assert_eq!(units.unit_of("/m/X-2CD/cd1"), "/m/X-2CD");
        assert_eq!(units.unit_of("/m/X-2CD/CD2"), "/m/X-2CD");
        assert_eq!(
            units.unit_of("/m/x-2cd/Cd1"),
            "/m/X-2CD",
            "paths compare without case"
        );
        assert_eq!(
            units.unit_of("/m/Y/cd1"),
            "/m/Y/cd1",
            "a lone disc folder is its own unit"
        );
        assert_eq!(units.unit_of("/m/Z"), "/m/Z");
        assert_eq!(units.unit_of("/m/X-2CD"), "/m/X-2CD");
    }

    #[test]
    fn only_a_disc_folder_scope_widens_and_only_one_level() {
        assert_eq!(
            unit_scope(Path::new("/m/X-2CD/cd1")),
            PathBuf::from("/m/X-2CD")
        );
        assert_eq!(
            unit_scope(Path::new("/m/X-2CD/Disc 2/extra")),
            PathBuf::from("/m/X-2CD/Disc 2/extra")
        );
        assert_eq!(
            unit_scope(Path::new("/mnt/disk1/music/A")),
            PathBuf::from("/mnt/disk1/music/A")
        );
        assert_eq!(unit_scope(Path::new("/m/X-2CD")), PathBuf::from("/m/X-2CD"));
        assert_eq!(
            unit_scope(Path::new("/m/Discography")),
            PathBuf::from("/m/Discography")
        );
    }

    #[test]
    fn a_disc_parent_exists_only_for_a_disc_folder_root() {
        assert_eq!(
            disc_parent_of(Path::new("/m/X-2CD/CD2")),
            Some(PathBuf::from("/m/X-2CD"))
        );
        assert_eq!(disc_parent_of(Path::new("/m/X-2CD")), None);
        assert_eq!(disc_parent_of(Path::new("/m/X-2CD/Disc 2/extra")), None);
        assert_eq!(
            disc_parent_of(Path::new("cd1")),
            None,
            "no parent to widen to"
        );
    }

    #[test]
    fn effective_discs_come_from_the_subfolder_then_the_title_then_the_tag() {
        assert_eq!(
            effective_disc("/m/X", "/m/X/cd2", Some("X"), Some(1)),
            Some(2)
        );
        assert_eq!(
            effective_disc("/m/X", "/m/X", Some("Shine 6 CD2"), None),
            Some(2)
        );
        assert_eq!(
            effective_disc("/m/X", "/m/X", Some("Shine 6"), Some(3)),
            Some(3)
        );
        assert_eq!(effective_disc("/m/X", "/m/X", None, None), None);
        // The unit's own folder is no disc subfolder, whatever its name.
        assert_eq!(effective_disc("/m/CD1", "/m/CD1", None, Some(4)), Some(4));
    }

    #[test]
    fn folder_names_state_a_disc_count_as_a_tag_word() {
        assert_eq!(
            folder_disc_count("/m/Depeche_Mode-Broadcast_Singles_Tour-2CD-1999-RNS"),
            Some(2)
        );
        assert_eq!(folder_disc_count("/m/VA-Shine_6_2CD-1996-NuHS"), Some(2));
        assert_eq!(
            folder_disc_count("/m/Bruce_Dickinson-Anthology-(DVD)-PROPER-3DVD-2006-LzY"),
            Some(3)
        );
        assert_eq!(
            folder_disc_count("/m/And_oceans-the_symmetry_(2cd)-1999-amrc"),
            Some(2)
        );
        assert_eq!(
            folder_disc_count("/m/Chemical_brothers-augmented-1999-apc"),
            None
        );
        assert_eq!(
            folder_disc_count("/m/Big_Daddy_Kane-The_Big_Daddy_Anthem_(12_Inch)-1998-DSP"),
            None
        );
        assert_eq!(
            folder_disc_count("/m/X-2CD-3CD-2001-GRP"),
            None,
            "two counts state none"
        );
    }

    fn file<'a>(path: &'a str, album: &'a str, track: i64) -> RuleFile<'a> {
        RuleFile {
            path,
            album: Some(album),
            disc_tag: None,
            track_number: Some(track),
            title: None,
            artist: None,
            duration_secs: None,
        }
    }

    /// A file with a track title, artist and length, and no track number.
    fn timed<'a>(
        path: &'a str,
        album: &'a str,
        title: &'a str,
        artist: &'a str,
        secs: f64,
    ) -> RuleFile<'a> {
        RuleFile {
            track_number: None,
            title: Some(title),
            artist: Some(artist),
            duration_secs: Some(secs),
            ..file(path, album, 1)
        }
    }

    fn group<'a>(title: &'a str, files: Vec<RuleFile<'a>>) -> TitleGroup<'a> {
        TitleGroup { title, files }
    }

    #[test]
    fn typos_and_release_type_words_name_the_same_album() {
        assert!(same_album("augmented", "augmentes"));
        assert!(same_album(
            "Peformance and Cocktails",
            "Performance and Cocktails"
        ));
        assert!(same_album("symbol of triumph", "symbol of tirumph"));
        assert!(same_album("Creep", "Creep CDM"));
        assert!(same_album("Bionic", "Bionic (12-inch)"));
        assert!(same_album("Shine 6 CD1", "Shine 6 CD2"));
    }

    #[test]
    fn numbers_versions_and_other_words_keep_titles_apart() {
        assert!(!same_album(
            "12-29-96 US Air Arena Washingt",
            "12-26-96 US Air Arena Washingt"
        ));
        assert!(!same_album("Shine 6", "Shine 7"));
        assert!(!same_album(
            "Big Daddy Anthem Accappella",
            "Big Daddy Anthem instr."
        ));
        assert!(!same_album("SOVAZ", "SOVAZ (Deluxe Edition)"));
        assert!(!same_album("cater to the dj", "fat jack"));
        assert!(!same_album("Floodland", "Vision Thing"));
    }

    #[test]
    fn the_main_title_is_the_one_most_files_carry() {
        let groups = [
            group("augmentes", vec![file("/m/a/03.mp3", "augmentes", 3)]),
            group(
                "augmented",
                vec![
                    file("/m/a/01.mp3", "augmented", 1),
                    file("/m/a/02.mp3", "augmented", 2),
                ],
            ),
        ];
        assert_eq!(main_title(&groups), Some("augmented"));
        let discs = [
            group("Shine 6 CD1", vec![file("/m/s/01.mp3", "Shine 6 CD1", 1)]),
            group("Shine 6 CD2", vec![file("/m/s/02.mp3", "Shine 6 CD2", 1)]),
        ];
        assert_eq!(
            merged_title(&discs).as_deref(),
            Some("Shine 6"),
            "a tie goes to the first path"
        );
        assert_eq!(merged_title(&[]), None);
    }

    #[test]
    fn the_main_title_counts_files_across_folders() {
        // Album x2 (cd1) + Album x2 (cd2) = 4 files beat Albun x3 in cd2.
        let groups = [
            group(
                "Album",
                vec![
                    file("/m/u/cd1/01.mp3", "Album", 1),
                    file("/m/u/cd1/02.mp3", "Album", 2),
                ],
            ),
            group(
                "Album",
                vec![
                    file("/m/u/cd2/01.mp3", "Album", 1),
                    file("/m/u/cd2/02.mp3", "Album", 2),
                ],
            ),
            group(
                "Albun",
                vec![
                    file("/m/u/cd2/03.mp3", "Albun", 3),
                    file("/m/u/cd2/04.mp3", "Albun", 4),
                    file("/m/u/cd2/05.mp3", "Albun", 5),
                ],
            ),
        ];
        assert_eq!(main_title(&groups), Some("Album"));
    }

    #[test]
    fn a_typo_in_one_file_merges_the_folder() {
        let unit = "/m/Chemical_brothers-augmented-1999-apc";
        let one = format!("{unit}/01.mp3");
        let two = format!("{unit}/02.mp3");
        let three = format!("{unit}/03.mp3");
        let groups = [
            group(
                "augmented",
                vec![file(&one, "augmented", 1), file(&two, "augmented", 2)],
            ),
            group("augmentes", vec![file(&three, "augmentes", 3)]),
        ];
        assert!(auto_merges(unit, &groups));
    }

    #[test]
    fn disc_folders_merge_when_their_titles_and_numbers_agree() {
        let unit = "/m/Depeche_Mode-Broadcast_Singles_Tour-2CD-1999-RNS";
        let paths: Vec<String> = ["cd1/01", "cd1/02", "cd2/01", "cd2/02"]
            .iter()
            .map(|name| format!("{unit}/{name}.mp3"))
            .collect();
        let groups = [
            group(
                "Broadcast Singles Tour",
                vec![
                    file(&paths[0], "Broadcast Singles Tour", 1),
                    file(&paths[1], "Broadcast Singles Tour", 2),
                ],
            ),
            group(
                "Broadcast_Singles_Tour",
                vec![
                    file(&paths[2], "Broadcast_Singles_Tour", 1),
                    file(&paths[3], "Broadcast_Singles_Tour", 2),
                ],
            ),
        ];
        assert!(auto_merges(unit, &groups));

        // A folder that says three discs but holds two stays apart.
        let three = "/m/Depeche_Mode-Broadcast_Singles_Tour-3CD-1999-RNS";
        let paths3: Vec<String> = paths.iter().map(|path| path.replace(unit, three)).collect();
        let groups3 = [
            group(
                "Broadcast Singles Tour",
                vec![file(&paths3[0], "Broadcast Singles Tour", 1)],
            ),
            group(
                "Broadcast Singles Tour",
                vec![file(&paths3[2], "Broadcast Singles Tour", 1)],
            ),
        ];
        assert!(!auto_merges(three, &groups3));
    }

    #[test]
    fn duplicates_gaps_and_different_albums_stay_apart() {
        // Two copies of track 1 in one folder.
        let copies = [
            group("augmented", vec![file("/m/c/01.mp3", "augmented", 1)]),
            group("augmentes", vec![file("/m/c/01b.mp3", "augmentes", 1)]),
        ];
        assert!(!auto_merges("/m/c", &copies));
        // The same titles with distinct track numbers merge, so the duplicate
        // position is what blocked the pair above.
        let distinct = [
            group("augmented", vec![file("/m/c/01.mp3", "augmented", 1)]),
            group("augmentes", vec![file("/m/c/02.mp3", "augmentes", 2)]),
        ];
        assert!(auto_merges("/m/c", &distinct));
        // Discs 1 and 3.
        let gap = [
            group("Album", vec![file("/m/g/cd1/01.mp3", "Album", 1)]),
            group("Album", vec![file("/m/g/cd3/01.mp3", "Album", 1)]),
        ];
        assert!(!auto_merges("/m/g", &gap));
        // A box of different albums.
        let boxed = [
            group("Floodland", vec![file("/m/b/01.mp3", "Floodland", 1)]),
            group("Vision Thing", vec![file("/m/b/02.mp3", "Vision Thing", 2)]),
        ];
        assert!(!auto_merges("/m/b", &boxed));
        // One title is nothing to merge.
        let single = [group("Album", vec![file("/m/s/01.mp3", "Album", 1)])];
        assert!(!auto_merges("/m/s", &single));
    }

    #[test]
    fn titles_that_each_name_the_scene_folder_title_merge() {
        // Each title is close enough to the folder's title, but not to the other.
        let unit = "/m/VA-Deathrow-1999-ERB";
        let (a, b) = ("Dathrow", "Deathro");
        assert!(same_album(a, "Deathrow") && same_album(b, "Deathrow"));
        assert!(
            !same_album(a, b),
            "the pair needs the folder title to merge"
        );
        let one = format!("{unit}/01.mp3");
        let two = format!("{unit}/02.mp3");
        let groups = [
            group(a, vec![file(&one, a, 1)]),
            group(b, vec![file(&two, b, 2)]),
        ];
        assert!(auto_merges(unit, &groups));
        // Another folder name offers no such bridge.
        let other = "/m/VA-Something_Else-1999-ERB";
        let one = format!("{other}/01.mp3");
        let two = format!("{other}/02.mp3");
        let elsewhere = [
            group(a, vec![file(&one, a, 1)]),
            group(b, vec![file(&two, b, 2)]),
        ];
        assert!(!auto_merges(other, &elsewhere));
    }

    #[test]
    fn two_disc_subfolders_naming_the_same_disc_stay_apart() {
        let unit = "/m/Album-2001-GRP";
        let groups = [
            group(
                "Album",
                vec![file("/m/Album-2001-GRP/cd1/01.mp3", "Album", 1)],
            ),
            group(
                "Album",
                vec![file("/m/Album-2001-GRP/disc 1/02.mp3", "Album", 2)],
            ),
        ];
        assert!(!auto_merges(unit, &groups));
    }

    #[test]
    fn disc_marked_titles_in_one_folder_merge_into_a_two_disc_release() {
        let unit = "/m/VA-Shine_6_2CD-1996-NuHS";
        let one = format!("{unit}/101.mp3");
        let two = format!("{unit}/201.mp3");
        let groups = [
            group("Shine 6 CD1", vec![file(&one, "Shine 6 CD1", 1)]),
            group("Shine 6 CD2", vec![file(&two, "Shine 6 CD2", 1)]),
        ];
        assert!(auto_merges(unit, &groups));
    }

    #[test]
    fn two_copies_of_one_file_stay_apart_without_track_numbers() {
        let unit = "/m/Rammstein-Live_In_NY_(07-18-01)_Bootleg-2001-PMS";
        let album = "Live In NY";
        let path = |name: &str| format!("{unit}/{name}");
        let (cd1a, cd1b) = (path("CD1/01.mp3"), path("CD1/02.mp3"));
        let (cd2a, cd2b) = (path("CD2/a.mp3"), path("CD2/b.mp3"));
        let (copy_a, copy_b) = (path("a.mp3"), path("b.mp3"));
        let groups = [
            group(album, vec![file(&cd1a, album, 1), file(&cd1b, album, 2)]),
            group(
                album,
                vec![
                    timed(&cd2a, album, "encore (live)", "Rammstein", 226.0),
                    timed(&cd2b, album, "sonne (live)", "Rammstein", 305.0),
                ],
            ),
            group(
                album,
                vec![
                    timed(&copy_a, album, "encore (live)", "Rammstein", 226.0),
                    timed(&copy_b, album, "sonne (live)", "Rammstein", 305.4),
                ],
            ),
        ];
        assert!(!auto_merges(unit, &groups));
        // Without the copies in the unit folder the same discs merge.
        assert!(auto_merges(unit, &groups[..2]));
    }

    #[test]
    fn one_title_on_two_discs_with_different_lengths_still_merges() {
        let unit = "/m/Album-Remixes-2CD-2001-GRP";
        let album = "Remixes";
        let one = format!("{unit}/CD1/a.mp3");
        let two = format!("{unit}/CD2/a.mp3");
        let groups = [
            group(album, vec![timed(&one, album, "Intro Mix", "DJ A", 180.0)]),
            group(album, vec![timed(&two, album, "Intro Mix", "DJ A", 240.0)]),
        ];
        assert!(auto_merges(unit, &groups));
    }

    #[test]
    fn one_title_by_different_artists_with_alike_lengths_still_merges() {
        // A mixtape of freestyles: the title and the length alike by chance.
        let unit = "/m/VA-Streetsweepers_Mixtape-2CD-2003-GRP";
        let album = "Streetsweepers Mixtape";
        let one = format!("{unit}/CD1/a.mp3");
        let two = format!("{unit}/CD2/a.mp3");
        let groups = [
            group(
                album,
                vec![timed(&one, album, "Freestyle", "Remy Martin", 100.3)],
            ),
            group(
                album,
                vec![timed(&two, album, "Freestyle", "Malicious", 100.2)],
            ),
        ];
        assert!(auto_merges(unit, &groups));
        // The same artist makes them copies.
        let same = [
            group(
                album,
                vec![timed(&one, album, "Freestyle", "Malicious", 100.3)],
            ),
            group(
                album,
                vec![timed(&two, album, "Freestyle", "malicious", 100.2)],
            ),
        ];
        assert!(!auto_merges(unit, &same));
    }

    #[test]
    fn unmeasured_lengths_never_make_copies() {
        let unit = "/m/Album-Singles-2CD-2001-GRP";
        let album = "Singles";
        let one = format!("{unit}/CD1/a.mp3");
        let two = format!("{unit}/CD2/a.mp3");
        let groups = [
            group(album, vec![timed(&one, album, "Intro", "DJ A", 0.0)]),
            group(album, vec![timed(&two, album, "Intro", "DJ A", 0.0)]),
        ];
        assert!(auto_merges(unit, &groups));
        // A measured pair of the same title and artist is still a pair of copies.
        let measured = [
            group(album, vec![timed(&one, album, "Intro", "DJ A", 90.0)]),
            group(album, vec![timed(&two, album, "Intro", "DJ A", 90.0)]),
        ];
        assert!(!auto_merges(unit, &measured));
    }
}
