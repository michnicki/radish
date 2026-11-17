//! Which files of a folder form one local release.
//!
//! A *unit* is a folder, or a folder together with its disc subfolders
//! (`CD1`, `Disc_2`) when it has two or more of them. Today's grouping splits
//! a unit by album title. A unit can instead be *merged* into one release,
//! either by the user's decision (`folder_groupings`) or by [`auto_merges`].
//! Everything here is pure: the store gathers the files and applies the result.

// Rust guideline compliant 2026-08-14

use crate::matcher;
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
}
