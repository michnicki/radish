//! Which files of a folder form one local release.
//!
//! A *unit* is a folder, or a folder together with its disc subfolders
//! (`CD1`, `Disc_2`) when it has two or more of them. Today's grouping splits
//! a unit by album title. A unit can instead be *merged* into one release,
//! either by the user's decision (`folder_groupings`) or by [`auto_merges`].
//! Everything here is pure: the store gathers the files and applies the result.

// Rust guideline compliant 2026-08-14

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
}
