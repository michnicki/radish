//! Subsonic IDs: `tr-` songs, `al-` albums and `ar-` artists.

use super::{decode_hex, hex};

/// What a Subsonic ID names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Id {
    /// A track, by `tracks.id`. It changes only when the file moves.
    Song(i64),
    /// A local release, by `local_releases.id`.
    Album(i64),
    /// An album artist, by its key with ASCII letters lowercased: the part of
    /// the key `COLLATE NOCASE` compares.
    Artist(String),
}

impl Id {
    pub(crate) fn song(id: i64) -> String {
        format!("tr-{id}")
    }

    pub(crate) fn album(id: i64) -> String {
        format!("al-{id}")
    }

    /// The artist ID of a release's artist key. Every key of a
    /// `COLLATE NOCASE` group gets the same ID, so an album's `artistId`
    /// always equals its artist's.
    pub(crate) fn artist(key: &str) -> String {
        format!("ar-{}", hex(key.to_ascii_lowercase().as_bytes()))
    }

    /// Reads an ID; `None` for anything malformed.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (kind, rest) = text.split_once('-')?;
        match kind {
            "tr" => number(rest).map(Self::Song),
            "al" => number(rest).map(Self::Album),
            "ar" => String::from_utf8(decode_hex(rest)?)
                .ok()
                .filter(|key| !key.is_empty())
                .map(Self::Artist),
            _ => None,
        }
    }
}

/// A row ID: ASCII digits only, so `+1` and ` 1` are refused.
fn number(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        assert_eq!(Id::parse(&Id::song(42)), Some(Id::Song(42)));
        assert_eq!(Id::parse(&Id::album(7)), Some(Id::Album(7)));
        assert_eq!(
            Id::parse(&Id::artist("Björk")),
            Some(Id::Artist("björk".into()))
        );
        assert_eq!(Id::artist("Nirvana"), Id::artist("NIRVANA"));
        assert_ne!(Id::artist("Ä"), Id::artist("ä"), "NOCASE folds ASCII only");
    }

    #[test]
    fn malformed_ids_are_refused() {
        for text in [
            "", "12", "tr-", "tr-+1", "tr- 1", "tr-1x", "xx-1", "ar-", "ar-zz", "ar-c3",
        ] {
            assert_eq!(Id::parse(text), None, "{text:?}");
        }
    }
}
