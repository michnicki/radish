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
    Merge,
    Separate,
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

/// The root a regroup or a load must cover to see whole units: a root inside
/// a disc subfolder widens to the folder above that subfolder.
pub fn unit_scope(root: &Path) -> PathBuf {
    for ancestor in root.ancestors() {
        if matcher::folder_disc_number(&ancestor.to_string_lossy()).is_some()
            && let Some(parent) = ancestor.parent()
            && !parent.as_os_str().is_empty()
        {
            return parent.to_path_buf();
        }
    }
    root.to_path_buf()
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
    pub path: &'a str,
    pub album: Option<&'a str>,
    pub disc_tag: Option<i64>,
    pub track_number: Option<i64>,
}

/// The files of a unit that today's grouping puts in one release (one folder,
/// one album title), with the title as tagged.
#[derive(Debug, Clone)]
pub struct TitleGroup<'a> {
    pub title: &'a str,
    /// In path order.
    pub files: Vec<RuleFile<'a>>,
}

/// The group whose title a merged release shows: the one with the most files;
/// on a tie, the one whose first file sorts first by case-insensitive path.
pub fn main_group<'g, 'a>(groups: &'g [TitleGroup<'a>]) -> Option<&'g TitleGroup<'a>> {
    let first_path = |group: &TitleGroup<'_>| {
        group
            .files
            .first()
            .map(|file| file.path.to_ascii_lowercase())
            .unwrap_or_default()
    };
    groups.iter().max_by(|a, b| {
        a.files
            .len()
            .cmp(&b.files.len())
            .then_with(|| first_path(b).cmp(&first_path(a)))
    })
}

/// The album title of a merged release: the main group's title without a
/// trailing disc marker. `None` when no group has a title.
pub fn merged_title(groups: &[TitleGroup<'_>]) -> Option<String> {
    let title = main_group(groups)?.title;
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

/// Whether the automatic rule merges a unit, given its groups with a usable
/// album title (untagged and junk-tagged files are not passed: they never
/// merge automatically). The caller has already checked that no release of
/// the unit holds an accepted match.
///
/// The unit merges when there are at least two groups and:
/// 1. every title names the same album as the main title, or both name the
///    same album as the title in the unit folder's scene name;
/// 2. no two files share an effective disc and track number;
/// 3. the effective discs run from 1 with no gap, and disc subfolders name
///    different discs;
/// 4. a disc count in the folder name (`2CD`) is met exactly.
pub fn auto_merges(unit: &str, groups: &[TitleGroup<'_>]) -> bool {
    if groups.len() < 2 {
        return false;
    }
    let Some(main) = main_group(groups) else {
        return false;
    };
    let folder_title = scene::parse_release_directory(unit)
        .and_then(|name| name.split())
        .and_then(|split| split.title);
    let names_main = |title: &str| {
        same_album(title, main.title)
            || folder_title
                .as_deref()
                .is_some_and(|folder| same_album(title, folder) && same_album(main.title, folder))
    };
    if !groups.iter().all(|group| names_main(group.title)) {
        return false;
    }

    let files: Vec<&RuleFile<'_>> = groups.iter().flat_map(|group| &group.files).collect();
    let disc_of = |file: &RuleFile<'_>| {
        effective_disc(unit, &directory_of(file.path), file.album, file.disc_tag).unwrap_or(1)
    };

    let mut positions = HashSet::new();
    for file in &files {
        if let Some(track) = file.track_number
            && !positions.insert((disc_of(file), track))
        {
            return false;
        }
    }

    let discs: BTreeSet<i64> = files.iter().map(|file| disc_of(file)).collect();
    if discs.len() > 1 && discs.iter().copied().ne(1..=discs.len() as i64) {
        return false;
    }
    let subfolders: HashSet<String> = files
        .iter()
        .map(|file| directory_of(file.path))
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

    use std::path::{Path, PathBuf};

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
    fn a_scope_inside_a_disc_folder_widens_to_the_folder_above() {
        assert_eq!(
            unit_scope(Path::new("/m/X-2CD/cd1")),
            PathBuf::from("/m/X-2CD")
        );
        assert_eq!(
            unit_scope(Path::new("/m/X-2CD/Disc 2/extra")),
            PathBuf::from("/m/X-2CD")
        );
        assert_eq!(unit_scope(Path::new("/m/X-2CD")), PathBuf::from("/m/X-2CD"));
        assert_eq!(
            unit_scope(Path::new("/m/Discography")),
            PathBuf::from("/m/Discography")
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
        assert_eq!(
            main_group(&groups).map(|group| group.title),
            Some("augmented")
        );
        let discs = [
            group("Shine 6 CD1", vec![file("/m/s/01.mp3", "Shine 6 CD1", 1)]),
            group("Shine 6 CD2", vec![file("/m/s/02.mp3", "Shine 6 CD2", 1)]),
        ];
        assert_eq!(
            merged_title(&discs).as_deref(),
            Some("Shine 6"),
            "a tie goes to the first path"
        );
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
            group("Album", vec![file("/m/c/01.mp3", "Album", 1)]),
            group("Albun", vec![file("/m/c/01b.mp3", "Albun", 1)]),
        ];
        assert!(!auto_merges("/m/c", &copies));
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
}
