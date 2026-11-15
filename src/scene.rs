//! Artist and album read from a scene release directory name.
//!
//! Scene releases are named `Artist-Album-(TAGS)-YEAR-GROUP`, with `_` for
//! spaces (or `.` when the name has no other word break). A release whose tags
//! are missing or filler still sits in a directory like that, so the name says
//! what to search for. The text is what discovery searches and ranks with, and a
//! second reading of the album title and artist in scoring; it is never stored.
//!
//! The parser is pure: no IO, no regular expressions, and no guessing. A name
//! it cannot read with certainty is `None`, and an ambiguous split (`Wu-Tang`
//! as a band against an album) is decided only by an artist or album tag the
//! release already carries.

// Rust guideline compliant 2026-08-14

use crate::matcher::{self, LocalRelease, folder_disc_number};

/// Words that name a release type, format or edition rather than an album.
const TAG_WORDS: &[&str] = &[
    "ep",
    "lp",
    "cd",
    "cds",
    "cdm",
    "cdep",
    "cdr",
    "mcd",
    "vls",
    "vinyl",
    "inch",
    "inches",
    "promo",
    "demo",
    "tape",
    "bootleg",
    "boot",
    "retail",
    "advance",
    "proper",
    "reissue",
    "remastered",
    "remaster",
    "ltd",
    "ed",
    "limited",
    "edition",
    "digipak",
    "digipack",
    "split",
    "sampler",
    "single",
    "maxi",
    "import",
    "japanese",
    "japan",
    "release",
    "bonus",
    "track",
    "tracks",
    "read",
    "nfo",
    "trackfix",
    "dirfix",
    "fix",
    "radio",
    "live",
    "ost",
    "soundtrack",
    "dvd",
    "dvda",
    "cassette",
    "flexi",
    "int",
    // Source and packaging tags of later releases (`-WEB-`, `-REPACK-`,
    // `-SAT-`), the most common fields the list above missed in a 263k-release
    // library.
    "web",
    "mag",
    "repack",
    "rerip",
    "sat",
    "sbd",
    "aud",
    "dab",
    "stream",
    "cdrep",
    "mcdr",
    "readnfo",
    "boxset",
];

/// Country codes scene releases carry as a tag (`-SP-`, `-FR-`).
const COUNTRY_CODES: &[&str] = &[
    "de", "sp", "es", "fr", "it", "se", "swe", "nl", "jp", "jpn", "uk", "us", "fi", "no", "dk",
    "be", "at", "ch", "pl", "br", "ca", "au", "mx", "ar", "pt", "cz", "sk", "hu", "ru", "gr", "ie",
    "nz", "gb", "ger", "cn", "kr", "ua", "tr",
];

/// Words dropped from the end of a title (`Take_Care_Of_Home_VLS`).
const TITLE_TAIL_WORDS: &[&str] = &[
    "ep",
    "cds",
    "cdm",
    "cdep",
    "mcd",
    "vls",
    "promo",
    "retail",
    "advance",
    "bootleg",
    "split",
    "vinyl",
    "7inch",
    "10inch",
    "12inch",
    "ost",
    "soundtrack",
    "trackfix",
    "dirfix",
];

/// Words that, with nothing else, name a kind of release and no album: a title
/// made only of them is no title (`Fazed-EP-VLS-DE-2000-GRP`). `Demo` and `Live`
/// are not here, since releases are called that (`DdF-Demo-2001-PMS`).
const TYPE_ONLY_WORDS: &[&str] = &[
    "ep", "split", "vls", "single", "cds", "cdm", "cdep", "mcd", "maxi", "promo",
];

/// Disc and format words that a number in front of makes a tail word (`2cd`,
/// `3CD`, `2LP`).
const NUMBERED_TAIL_WORDS: &[&str] = &["cd", "cds", "lp", "dvd", "dvda", "inch", "inches"];

/// Country and nationality names that, with tag words, make a bracket group on
/// a title a tag (`(German Import)`, `(Japan Import)`).
const COUNTRY_NAMES: &[&str] = &[
    "german",
    "germany",
    "french",
    "france",
    "italian",
    "italy",
    "spanish",
    "spain",
    "swedish",
    "sweden",
    "dutch",
    "netherlands",
    "holland",
    "british",
    "english",
    "american",
    "usa",
    "europe",
    "european",
    "canadian",
    "canada",
    "australian",
    "australia",
    "brazilian",
    "brazil",
    "mexican",
    "mexico",
    "finnish",
    "finland",
    "norwegian",
    "norway",
    "danish",
    "denmark",
    "polish",
    "poland",
    "russian",
    "russia",
    "korean",
    "korea",
    "uk",
];

/// Whole fields that stand for Various Artists, compared by their letters and
/// digits alone (`V.A.`, `Various_Artists`).
const VARIOUS_MARKERS: &[&str] = &["va", "various", "variousartists", "ost", "soundtrack"];

/// Grouping marks that hide a dash inside them.
fn closing_for(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn opening_for(close: char) -> Option<char> {
    match close {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

/// One dash-separated field of a directory name, cleaned of `_` and spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    text: String,
    /// Whether the separator in front of the field is strong: `--` or more, or
    /// `_` or a space on both sides of the dash.
    strong_before: bool,
}

/// A release directory name split into its artist, album and decoration fields.
///
/// Made by [`parse_release_directory`]. The group, years and format tags are
/// already gone; what remains is the artist and the album, whose boundary
/// [`SceneName::split`], [`SceneName::split_matching`] or
/// [`SceneName::split_matching_title`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneName {
    fields: Vec<Field>,
    /// Whether the name marks a split release (a `Split` or `Split EP` tag, or
    /// a title that ends in `Split`), which names several artists and so has no
    /// reading.
    split_release: bool,
}

/// An artist and album title read from a directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneSplit {
    /// `None` for a Various Artists release.
    pub artist: Option<String>,
    /// `None` when what the name leaves is only release-type words (`EP`,
    /// `VLS`, `Single`): they say what kind of release it is, not which album.
    pub title: Option<String>,
    pub various: bool,
}

/// Reads a release directory name.
///
/// The last path component names the release; a disc folder (`CD2`) names the
/// directory above it instead, one level only. A name needs an artist, an album
/// and a group, separated by dashes. Returns `None` for any other shape.
///
/// # Examples
///
/// ```
/// use radish::scene::parse_release_directory;
///
/// let name = parse_release_directory("/mp3/Hidrology-Asleep-2002-AMRC").unwrap();
/// let split = name.split().unwrap();
/// assert_eq!(split.artist.as_deref(), Some("Hidrology"));
/// assert_eq!(split.title.as_deref(), Some("Asleep"));
/// ```
pub fn parse_release_directory(directory: &str) -> Option<SceneName> {
    let name = release_leaf(directory)?;
    if !name.contains('-') {
        return None;
    }
    let dotted = !name.contains(['_', ' ']) && name.contains('.');
    let mut fields = split_fields(name, dotted);
    if fields.len() < 3 {
        return None;
    }
    fields.pop();

    let mut split_release = false;
    let mut year_seen = strip_trailing_fields(&mut fields, true, &mut split_release);
    merge_markers(&mut fields);
    year_seen |= strip_trailing_fields(&mut fields, false, &mut split_release);

    if fields.len() < 2 {
        return None;
    }
    if fields.len() == 2 && !year_seen && is_year_field(&fields[1].text) {
        return None;
    }
    if let Some(last) = fields.last_mut() {
        last.text = strip_title_tail(&last.text, year_seen, &mut split_release);
    }
    Some(SceneName {
        fields,
        split_release,
    })
}

/// Lowest similarity at which a tag decides the cut of an ambiguous name.
///
/// The artist tag is compared with each cut's artist part, and the album tag
/// with each cut's title part; a cut stands only when exactly one fits. High
/// enough that a tag naming a shorter credit (`Stresmatic` against
/// `Stresmatic and Droop-E`) decides nothing, low enough that spelling
/// (`Wu-Tang Clan` against `Wu Tang Clan`) still does.
const TAG_HINT: f64 = 0.90;

/// The artist and album a release's directory name gives, tags breaking ties.
///
/// The cut comes from the name's structure ([`SceneName::split`]), else from
/// the artist tag ([`SceneName::split_matching`]), else from the album tag
/// ([`SceneName::split_matching_title`]). A missing, filler or Various Artists
/// artist tag and a missing or filler album tag decide nothing. A filler title
/// in the name (`Unknown_Title`) is no title. `None` when the name is not a
/// scene release name, marks a split release, or no cut can be decided.
pub(crate) fn directory_split(local: &LocalRelease) -> Option<SceneSplit> {
    let name = parse_release_directory(&local.directory)?;
    let artist_tag = matcher::tag_identity(local).artist;
    let title_tag = local
        .album_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty() && !matcher::is_junk_title(title));
    let mut split = name
        .split()
        .or_else(|| {
            let tag = artist_tag.as_deref()?;
            name.split_matching(|part| matcher::local_artist_similarity(tag, part) >= TAG_HINT)
        })
        .or_else(|| {
            let tag = title_tag?;
            name.split_matching_title(|part| matcher::album_title_similarity(tag, part) >= TAG_HINT)
        })?;
    if split.title.as_deref().is_some_and(matcher::is_junk_title) {
        split.title = None;
    }
    Some(split)
}

/// The path component that names the release.
fn release_leaf(directory: &str) -> Option<&str> {
    let trimmed = directory.trim_end_matches(['/', '\\']);
    let mut parts = trimmed.rsplit(['/', '\\']);
    let last = parts.next()?;
    let name = if is_disc_folder(last) {
        parts.next()?
    } else {
        last
    };
    (!name.trim().is_empty()).then_some(name)
}

/// A disc folder: `CD1`, `disc_2`, or one with a short suffix (`cd2_o`).
fn is_disc_folder(name: &str) -> bool {
    if folder_disc_number(name).is_some() {
        return true;
    }
    name.rsplit_once(['_', '-', ' '])
        .is_some_and(|(head, tail)| {
            !tail.is_empty()
                && tail.len() <= 2
                && tail.chars().all(|ch| ch.is_ascii_alphabetic())
                && folder_disc_number(head).is_some()
        })
}

/// Splits at dashes outside brackets. A run of dashes is one separator.
fn split_fields(name: &str, dotted: bool) -> Vec<Field> {
    let chars: Vec<char> = name.chars().collect();
    let mut fields = Vec::new();
    let mut start = 0;
    let mut strong_before = false;
    let mut depth = 0usize;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if closing_for(ch).is_some() {
            depth += 1;
        } else if opening_for(ch).is_some() {
            depth = depth.saturating_sub(1);
        } else if ch == '-' && depth == 0 {
            let mut end = index;
            while end < chars.len() && chars[end] == '-' {
                end += 1;
            }
            let spaced = |ch: Option<&char>| ch.is_some_and(|ch| matches!(ch, '_' | ' '));
            let strong = end - index >= 2
                || (spaced(index.checked_sub(1).and_then(|before| chars.get(before)))
                    && spaced(chars.get(end)));
            push_field(&mut fields, &chars[start..index], strong_before, dotted);
            strong_before = strong;
            start = end;
            index = end;
            continue;
        }
        index += 1;
    }
    push_field(&mut fields, &chars[start..], strong_before, dotted);
    fields
}

fn push_field(fields: &mut Vec<Field>, raw: &[char], strong_before: bool, dotted: bool) {
    let text = clean_text(&raw.iter().collect::<String>(), dotted);
    if text.is_empty() {
        return;
    }
    let first = fields.is_empty();
    fields.push(Field {
        text,
        strong_before: strong_before && !first,
    });
}

/// `_` (and `.` in a dotted name) read as spaces, with whitespace collapsed.
fn clean_text(raw: &str, dotted: bool) -> String {
    let spaced: String = raw
        .chars()
        .map(|ch| {
            if ch == '_' || (dotted && ch == '.') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drops year and tag fields from the end while more than two fields remain
/// (and years too when `years` is set). Returns whether a year was dropped, and
/// sets `split_release` when a dropped tag field says `Split`.
fn strip_trailing_fields(fields: &mut Vec<Field>, years: bool, split_release: &mut bool) -> bool {
    let mut year_dropped = false;
    while fields.len() > 2 {
        let Some(last) = fields.last() else { break };
        if years && is_year_field(&last.text) {
            year_dropped = true;
        } else if !is_tag_field(&last.text) {
            break;
        } else if has_word(&last.text, "split") {
            *split_release = true;
        }
        fields.pop();
    }
    year_dropped
}

/// `S` and `T`, or `Self` and `Titled`, joined by a weak dash are one field;
/// a field that opens with a bracket belongs to the one before it.
fn merge_markers(fields: &mut Vec<Field>) {
    let mut merged: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields.drain(..) {
        if let Some(previous) = merged.last_mut() {
            let weak = !field.strong_before;
            let pair = |first: &str, second: &str| {
                weak && previous.text.eq_ignore_ascii_case(first)
                    && field.text.eq_ignore_ascii_case(second)
            };
            if pair("s", "t") {
                previous.text = "S/T".to_string();
                continue;
            }
            if pair("self", "titled") {
                previous.text = "Self-Titled".to_string();
                continue;
            }
            if field.text.starts_with(['(', '[']) {
                previous.text = format!("{} {}", previous.text, field.text);
                continue;
            }
        }
        merged.push(field);
    }
    *fields = merged;
}

/// A year: `1999`, `2003`, or a decade (`19x`, `198x`, `19xx`, `20xx`),
/// optionally in `()` or `[]`.
fn is_year_field(text: &str) -> bool {
    let inner = strip_wrapping_brackets(text);
    let lower = inner.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if lower == "19x" {
        return true;
    }
    bytes.len() == 4
        && (lower.starts_with("19") || lower.starts_with("20"))
        && (bytes[2].is_ascii_digit() || bytes[2] == b'x')
        && (bytes[3].is_ascii_digit() || bytes[3] == b'x')
        // `19x7` is no decade: an `x` ends the digits.
        && (bytes[3] == b'x' || bytes[2].is_ascii_digit())
}

fn strip_wrapping_brackets(text: &str) -> &str {
    let text = text.trim();
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(inner) = text
            .strip_prefix(open)
            .and_then(|rest| rest.strip_suffix(close))
        {
            return inner.trim();
        }
    }
    text
}

/// Whether `word` is one of the words of `text`, without case.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|ch: char| !ch.is_alphanumeric())
        .any(|candidate| candidate.eq_ignore_ascii_case(word))
}

/// A field made only of tag words, country codes, numbered tags and numbers,
/// with at least one word that is not a number.
fn is_tag_field(text: &str) -> bool {
    is_tag_text(text, false)
}

/// [`is_tag_field`], also allowing country names when `names` is set.
fn is_tag_text(text: &str, names: bool) -> bool {
    let words: Vec<&str> = text
        .split(|ch: char| {
            matches!(
                ch,
                '(' | ')' | '[' | ']' | '{' | '}' | '-' | '_' | '.' | ' '
            )
        })
        .filter(|word| !word.is_empty())
        .collect();
    let allowed = |word: &str| {
        is_number(word)
            || is_tag_word(word)
            || (names && COUNTRY_NAMES.contains(&word.to_ascii_lowercase().as_str()))
    };
    !words.is_empty()
        && words.iter().all(|word| allowed(word))
        && words.iter().any(|word| !is_number(word))
}

fn is_number(word: &str) -> bool {
    word.bytes().all(|byte| byte.is_ascii_digit())
}

/// A tag word, a country code, or a tag word with a number on either side.
fn is_tag_word(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    let known = |word: &str| {
        !word.is_empty() && (TAG_WORDS.contains(&word) || COUNTRY_CODES.contains(&word))
    };
    known(&lower)
        || known(lower.trim_start_matches(|ch: char| ch.is_ascii_digit()))
        || known(lower.trim_end_matches(|ch: char| ch.is_ascii_digit()))
}

/// Drops decoration from the end of the title field, never the whole title:
/// bracket groups that are tags or years, trailing format words, and a bare
/// trailing year when the name had no year field.
fn strip_title_tail(title: &str, year_seen: bool, split_release: &mut bool) -> String {
    let mut title = title.trim().to_string();
    loop {
        let shorter = strip_bracket_group(&title)
            .or_else(|| strip_tail_word(&title, year_seen))
            .map(|rest| rest.trim().to_string());
        match shorter {
            Some(rest) if rest.chars().any(char::is_alphanumeric) => {
                let dropped = title[rest.len()..].trim();
                if has_word(dropped, "split") {
                    *split_release = true;
                }
                title = rest;
            }
            _ => return title,
        }
    }
}

/// The title without a trailing bracket group that is a tag field or a year.
fn strip_bracket_group(title: &str) -> Option<&str> {
    let close = title.chars().next_back()?;
    let open = opening_for(close)?;
    let mut depth = 0usize;
    for (index, ch) in title.char_indices().rev() {
        if ch == close {
            depth += 1;
        } else if ch == open {
            depth -= 1;
            if depth == 0 {
                let group = &title[index..];
                return (is_tag_text(group, true) || is_year_field(group)).then(|| &title[..index]);
            }
        }
    }
    None
}

/// The title without a trailing format word (`VLS`, `OST`, `2cd`, `Trackfix`,
/// `Read NFO`), or a trailing year.
fn strip_tail_word(title: &str, year_seen: bool) -> Option<&str> {
    let (rest, word) = title.rsplit_once(' ')?;
    if word.eq_ignore_ascii_case("nfo")
        && let Some((before, read)) = rest.rsplit_once(' ')
        && read.eq_ignore_ascii_case("read")
    {
        return Some(before);
    }
    let lower = word.to_ascii_lowercase();
    let digits = lower.len()
        - lower
            .trim_start_matches(|ch: char| ch.is_ascii_digit())
            .len();
    let numbered = digits > 0 && NUMBERED_TAIL_WORDS.contains(&&lower[digits..]);
    let is_year = !year_seen
        && word.len() == 4
        && word.bytes().all(|byte| byte.is_ascii_digit())
        && (word.starts_with("19") || word.starts_with("20"));
    (TITLE_TAIL_WORDS.contains(&lower.as_str()) || numbered || is_year).then_some(rest)
}

/// Whether a field is a Various Artists marker.
fn is_various_marker(text: &str) -> bool {
    let compact: String = text
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    VARIOUS_MARKERS.contains(&compact.as_str())
}

/// Whether a field is a self-titled marker left by the merge step.
fn is_self_titled_marker(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "s/t" | "self-titled" | "self titled"
    )
}

impl SceneName {
    /// The fields from `start` to `end`, rejoined with their original
    /// separators: weak as `-` and strong as ` - `.
    fn join(&self, start: usize, end: usize) -> String {
        let mut joined = String::new();
        for (index, field) in self.fields[start..end].iter().enumerate() {
            if index > 0 {
                joined.push_str(if field.strong_before { " - " } else { "-" });
            }
            joined.push_str(&field.text);
        }
        joined
    }

    fn make_split(
        &self,
        artist: Option<String>,
        title: String,
        various: bool,
    ) -> Option<SceneSplit> {
        let has_text = |text: &str| text.chars().any(char::is_alphanumeric);
        if self.split_release || !has_text(&title) || !artist.as_deref().is_none_or(has_text) {
            return None;
        }
        let type_only = title
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .all(|word| TYPE_ONLY_WORDS.contains(&word.to_ascii_lowercase().as_str()));
        // A title that is only `Split` marks a split release too.
        if type_only && has_word(&title, "split") {
            return None;
        }
        // The artist alone is still a reading, but not for Various Artists,
        // which then says nothing at all.
        if type_only && artist.is_none() {
            return None;
        }
        Some(SceneSplit {
            artist,
            title: (!type_only).then_some(title),
            various,
        })
    }

    /// The split the name's structure decides, or `None` when it is ambiguous.
    ///
    /// A name with two fields, a strong first separator, a Various Artists
    /// marker first or a self-titled marker last has one reading. Anything else
    /// (`Wu-Tang_Clan-Wu-Tang_Forever`) needs [`SceneName::split_matching`] or
    /// [`SceneName::split_matching_title`].
    pub fn split(&self) -> Option<SceneSplit> {
        let count = self.fields.len();
        if is_various_marker(&self.fields[0].text) {
            return self.make_split(None, self.join(1, count), true);
        }
        let last_is_marker = is_self_titled_marker(&self.fields[count - 1].text);
        if count == 2 || self.fields[1].strong_before {
            return self.make_split(Some(self.join(0, 1)), self.join(1, count), false);
        }
        if last_is_marker {
            return self.make_split(
                Some(self.join(0, count - 1)),
                self.join(count - 1, count),
                false,
            );
        }
        None
    }

    /// [`SceneName::split`], resolving an ambiguous name with an artist hint.
    ///
    /// Every cut point is tried; the split stands only when exactly one artist
    /// part satisfies `is_artist`. Never searches several splits.
    pub fn split_matching(&self, is_artist: impl Fn(&str) -> bool) -> Option<SceneSplit> {
        self.split_at_unique_cut(|cut| is_artist(&self.join(0, cut)))
    }

    /// [`SceneName::split`], resolving an ambiguous name with an album title hint.
    ///
    /// Every cut point is tried; the split stands only when exactly one title
    /// part (everything after the cut) satisfies `is_title`.
    pub fn split_matching_title(&self, is_title: impl Fn(&str) -> bool) -> Option<SceneSplit> {
        let count = self.fields.len();
        self.split_at_unique_cut(|cut| is_title(&self.join(cut, count)))
    }

    /// The structural split, or else the one cut point that `fits`.
    fn split_at_unique_cut(&self, fits: impl Fn(usize) -> bool) -> Option<SceneSplit> {
        if let Some(split) = self.split() {
            return Some(split);
        }
        let count = self.fields.len();
        let mut cuts = (1..count).filter(|cut| fits(*cut));
        let cut = cuts.next()?;
        if cuts.next().is_some() {
            return None;
        }
        self.make_split(Some(self.join(0, cut)), self.join(cut, count), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{
        ArtistKind, EmbeddedReleaseEvidence, album_title_similarity, local_artist_similarity,
    };

    fn read(directory: &str) -> Option<(Option<String>, String, bool)> {
        let split = parse_release_directory(directory)?.split()?;
        Some((split.artist, split.title?, split.various))
    }

    fn pair(artist: &str, title: &str) -> Option<(Option<String>, String, bool)> {
        Some((Some(artist.to_string()), title.to_string(), false))
    }

    fn various(title: &str) -> Option<(Option<String>, String, bool)> {
        Some((None, title.to_string(), true))
    }

    #[test]
    fn real_scene_names_split_into_artist_and_title() {
        let cases = [
            (
                "Straight_Faced_-_Broken-1996-SDR",
                "Straight Faced",
                "Broken",
            ),
            ("Adequate7-St-EP-2001-FNT", "Adequate7", "St"),
            (
                "Bruce_Springsteen_-_Nebraska-1982-bkf",
                "Bruce Springsteen",
                "Nebraska",
            ),
            (
                "Rob.zombie-american.made.music.to.strip.by-dvnmp3",
                "Rob zombie",
                "american made music to strip by",
            ),
            ("Hidrology-Asleep-2002-AMRC", "Hidrology", "Asleep"),
            (
                "Buck_65-The_Centaur_(12-Inch)-1999-DSP",
                "Buck 65",
                "The Centaur",
            ),
            (
                "Loquero-Una_Noche_En_Shailot-SP-2001-ATR",
                "Loquero",
                "Una Noche En Shailot",
            ),
            ("Eve.6--Eve.6-1999-UBE", "Eve 6", "Eve 6"),
            (
                "Strike_Anywhere--Change_Is_A_Sound-2001-UBE",
                "Strike Anywhere",
                "Change Is A Sound",
            ),
            ("D.R.I.-Live-1994-rH", "D R I", "Live"),
            ("Plasticland-S-T-1985-SDR", "Plasticland", "S/T"),
            ("H2O-Self-Titled-1996-rH", "H2O", "Self-Titled"),
            (
                "Chik-Chik-Chik-Self-Titled-2000-NuHS",
                "Chik-Chik-Chik",
                "Self-Titled",
            ),
            (
                "Chixdiggit_-_Self_Titled_1996-SYN",
                "Chixdiggit",
                "Self Titled",
            ),
            (
                "Bruce_Dickinson_-_Accident_of_Birth_(1997)_-_RBC",
                "Bruce Dickinson",
                "Accident of Birth",
            ),
            (
                "Agent_Orange-Sonic_Snake_Session_(REMASTERED)-2CD-2003-EGO",
                "Agent Orange",
                "Sonic Snake Session",
            ),
            (
                "Branvan_3000-Discosis-Retail-Read_NFO-2001-JUST",
                "Branvan 3000",
                "Discosis",
            ),
            (
                "Starshooter-Get_Baque-7inch-FR-1978-SDR",
                "Starshooter",
                "Get Baque",
            ),
            (
                "Dave_Hollister-Take_Care_Of_Home_VLS-2001-BKF",
                "Dave Hollister",
                "Take Care Of Home",
            ),
            (
                "Porcupine.Tree--Recordings-Ltd.Ed.-(2001)-iMPG",
                "Porcupine Tree",
                "Recordings",
            ),
            (
                "Janis_Joplin_-_Janis-(1993)_(3_CDs)-STAR",
                "Janis Joplin",
                "Janis",
            ),
            (
                "In_Flames-Orebro_Sweden(02-04-01)-2001-PMS",
                "In Flames",
                "Orebro Sweden(02-04-01)",
            ),
            ("DdF-Demo-2001-PMS", "DdF", "Demo"),
            (
                "Xzibit-Man_Vs_Machine_(Proper_Retail)-2CD-2002-WCR/CD1",
                "Xzibit",
                "Man Vs Machine",
            ),
            ("Rufio-1985-Advance-2003-TWCMP3", "Rufio", "1985"),
        ];
        for (directory, artist, title) in cases {
            assert_eq!(read(directory), pair(artist, title), "{directory}");
        }
        // A path reads by its last component, or the one above a disc folder.
        assert_eq!(
            read("/mnt/mp3/daydirs/2001/Hidrology-Asleep-2002-AMRC/"),
            pair("Hidrology", "Asleep")
        );
        assert_eq!(
            read(r"C:\mp3\Xzibit-Man_Vs_Machine_(Proper_Retail)-2CD-2002-WCR\CD1"),
            pair("Xzibit", "Man Vs Machine")
        );
        assert_eq!(
            read("/x/And_oceans-the_symmetry_of_i_the_circle_o_(2cd)-1999-amrc/cd2_o"),
            pair("And oceans", "the symmetry of i the circle o")
        );
    }

    #[test]
    fn various_artists_markers_give_a_title_and_no_artist() {
        assert_eq!(
            read("VA-Thirteen_Days_OST-2000-RNS"),
            various("Thirteen Days")
        );
        assert_eq!(
            read("OST_-_Star_Wars_-_The_Phantom_Menace-1999-RNS"),
            various("Star Wars - The Phantom Menace")
        );
        assert_eq!(
            read("VA-Projekt-Gothic-2002-SGE"),
            various("Projekt-Gothic")
        );
        assert_eq!(
            read("V.A.-Some.Compilation-2001-GRP"),
            various("Some Compilation")
        );
    }

    fn matching(directory: &str, hint: &str) -> Option<(Option<String>, String)> {
        parse_release_directory(directory)?
            .split_matching(|part| local_artist_similarity(hint, part) >= 0.90)
            .and_then(|split| Some((split.artist, split.title?)))
    }

    #[test]
    fn an_ambiguous_name_needs_an_artist_hint() {
        let wu_tang = "Wu-Tang_Clan-Wu-Tang_Forever-1997-KSi_INT";
        assert_eq!(read(wu_tang), None);
        assert_eq!(
            matching(wu_tang, "Wu-Tang Clan"),
            Some((
                Some("Wu-Tang Clan".to_string()),
                "Wu-Tang Forever".to_string()
            ))
        );
        assert_eq!(matching(wu_tang, "Someone Else"), None);

        let steely = "Steely_Dan-Showbiz_Kids-The_Steely_Dan_Story-2CD-2000-REV/CD2";
        assert_eq!(read(steely), None);
        assert_eq!(
            matching(steely, "Steely Dan"),
            Some((
                Some("Steely Dan".to_string()),
                "Showbiz Kids-The Steely Dan Story".to_string()
            ))
        );

        let fleurety = "Fleurety-last-minute_lies-(cdm)-1999-amrc";
        assert_eq!(read(fleurety), None);
        assert_eq!(
            matching(fleurety, "Fleurety"),
            Some((Some("Fleurety".to_string()), "last-minute lies".to_string()))
        );
        // A hint that decides nothing about structure leaves a decided name alone.
        assert_eq!(
            matching("Hidrology-Asleep-2002-AMRC", "Hidrology"),
            Some((Some("Hidrology".to_string()), "Asleep".to_string()))
        );
        // Two cut points that both satisfy the hint are no answer.
        let doubled = parse_release_directory("A-B-C-D-1999-GRP").unwrap();
        assert_eq!(doubled.split_matching(|_| true), None);
    }

    fn matching_title(directory: &str, hint: &str) -> Option<(Option<String>, String)> {
        parse_release_directory(directory)?
            .split_matching_title(|part| album_title_similarity(hint, part) >= 0.90)
            .and_then(|split| Some((split.artist, split.title?)))
    }

    #[test]
    fn an_ambiguous_name_can_be_settled_by_the_album_tag() {
        let stresmatic = "Stresmatic_and_Droop-E-4005-WEB-2021-RAGEMP3";
        assert_eq!(read(stresmatic), None);
        assert_eq!(
            matching_title(stresmatic, "4005"),
            Some((
                Some("Stresmatic and Droop-E".to_string()),
                "4005".to_string()
            ))
        );
        // A 30-character tag cut off by an ID3v1 writer is the start of the title.
        let long =
            "Stresmatic_and_Droop-E-The_Very_Long_Album_Title_That_Goes_On_And_On-WEB-2021-GRP";
        assert_eq!(
            matching_title(long, "The Very Long Album Title That"),
            Some((
                Some("Stresmatic and Droop-E".to_string()),
                "The Very Long Album Title That Goes On And On".to_string()
            ))
        );
        assert_eq!(matching_title(stresmatic, "Something Else"), None);
        // A name its structure decides keeps that split.
        assert_eq!(
            matching_title("Hidrology-Asleep-2002-AMRC", "Something Else"),
            Some((Some("Hidrology".to_string()), "Asleep".to_string()))
        );
        // Two cut points that both fit are no answer.
        let doubled = parse_release_directory("A-B-C-D-1999-GRP").unwrap();
        assert_eq!(doubled.split_matching_title(|_| true), None);
    }

    #[test]
    fn names_that_are_not_artist_title_group_are_rejected() {
        for directory in [
            "Carv-2001-STA",
            "The_Ataris_Looking_Forward_To_Failure_1999_SYN",
            "/music/album",
            "/music/blue",
            "Test Artist - Blue",
            "CD1",
            "/CD1",
            "",
            "/",
            "radish-it-match_empty-12345",
            "/tmp/radish-scene-12345-3",
            "/tmp/radish-scene-12345-3/album",
            "A-B",
            "-----",
            "---!-?-x",
            "S-T-1999-GRP",
        ] {
            assert_eq!(
                parse_release_directory(directory).and_then(|name| name.split()),
                None,
                "{directory}"
            );
        }
    }

    #[test]
    fn the_title_loses_the_same_decoration_the_tag_fields_do() {
        let cases = [
            ("VA-Chasing_Papi_OST-2003-WCR", "Chasing Papi"),
            ("Foo-Confidence_OST-2001-GRP", "Confidence"),
            ("Bruce_Broughton-Roughing_It_OST-2003-SiRiON", "Roughing It"),
            ("Foo-Some_Film_Soundtrack-2001-GRP", "Some Film"),
            (
                "Springtoifel-5_Mainzeldroogs_In_Berlin_2cd-1995-SDR",
                "5 Mainzeldroogs In Berlin",
            ),
            ("Foo-Bar_3CD-2001-GRP", "Bar"),
            ("Foo-Bar_2LP-2001-GRP", "Bar"),
            ("Foo-Shoot_The_Pup_Trackfix-2001-GRP", "Shoot The Pup"),
            ("Foo-Shoot_The_Pup_Dirfix-2001-GRP", "Shoot The Pup"),
            ("Foo-Bar_Read_NFO-2001-GRP", "Bar"),
            (
                "Foo-Going_Inside_EP_(German_Import)-2001-GRP",
                "Going Inside",
            ),
            ("Foo-Bar_(Japan_Import)-2001-GRP", "Bar"),
            ("Foo-Bar_(UK_CD1_Single)-2001-GRP", "Bar"),
        ];
        for (directory, title) in cases {
            let split = parse_release_directory(directory).unwrap().split().unwrap();
            assert_eq!(split.title.as_deref(), Some(title), "{directory}");
        }
        // Never the whole title, and a word that is a title stays one.
        assert_eq!(read("Foo-OST-2001-GRP"), pair("Foo", "OST"));
        assert_eq!(read("Foo-Read_NFO-2001-GRP"), pair("Foo", "Read NFO"));
        assert_eq!(read("Foo-Please_Read-2001-GRP"), pair("Foo", "Please Read"));
        assert_eq!(
            read("Foo-Bar_(German)_Live-2001-GRP"),
            pair("Foo", "Bar (German) Live")
        );
        assert_eq!(
            read("Foo-Bar_(Berlin_Import)-2001-GRP"),
            pair("Foo", "Bar (Berlin Import)")
        );
    }

    #[test]
    fn decades_are_years() {
        for decade in [
            "198x", "197x", "20xx", "19xx", "200x", "199x", "19x", "(198x)",
        ] {
            assert!(is_year_field(decade), "{decade}");
            let directory = format!("C.A.D.-Fingernails-{decade}-AMRC");
            assert_eq!(
                read(&directory),
                pair("C A D", "Fingernails"),
                "{directory}"
            );
        }
        for not_decade in ["19x7", "1x", "2x", "21xx", "1x99", "x"] {
            assert!(!is_year_field(not_decade), "{not_decade}");
        }
    }

    #[test]
    fn rules_the_real_names_do_not_exercise() {
        // A year-shaped title survives only when a year field was dropped.
        assert_eq!(read("Rufio-1985-2003-TWCMP3"), pair("Rufio", "1985"));
        assert_eq!(read("Rufio-1985-TWCMP3"), None);
        // Years and tags in any bracket shape, and decades.
        assert_eq!(read("Foo-Bar-[1999]-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar-199x-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar-cds2-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar-10inches-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar-2DVDA-GRP"), pair("Foo", "Bar"));
        // A tag field made only of numbers is not a tag; with a real word it is.
        assert_eq!(read("Foo-Bar-02-04-01-GRP"), None);
        // The title tail never takes the whole title.
        assert_eq!(read("Foo-(Demo)-GRP"), None);
        assert_eq!(read("Foo-Bar_EP-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar_(Demo)_(1993)-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-Bar_1996-GRP"), pair("Foo", "Bar"));
        assert_eq!(read("Foo-EP_(Demo)_1996-GRP"), None);
        // A year in the title stays when the name had a year field.
        assert_eq!(read("Foo-Bar_1996-1996-GRP"), pair("Foo", "Bar 1996"));
        // A bracket that does not close keeps the dash inside it.
        assert_eq!(read("Foo-Bar_(live-GRP"), None);
        // A bracket field that is not a tag joins the field before it.
        assert_eq!(
            read("Foo-Bar-(Live_at_X)-1999-GRP"),
            pair("Foo", "Bar (Live at X)")
        );
        assert_eq!(read("Foo-Bar-(Live)-1999-GRP"), pair("Foo", "Bar"));
        // A marker only joins across a weak dash.
        assert_eq!(read("Foo-S_-_T-1999-GRP"), None);
        assert_eq!(read("Foo-Self_Titled-1999-GRP"), pair("Foo", "Self Titled"));
        // The name's first and last separators decide.
        assert_eq!(
            read("Foo_Bar--Baz-Qux-1999-GRP"),
            pair("Foo Bar", "Baz-Qux")
        );
        assert_eq!(read("Foo-Bar-Baz-S-T-1999-GRP"), pair("Foo-Bar-Baz", "S/T"));
        assert_eq!(read("Foo-Bar-Baz-1999-GRP"), None);
        // Text without a letter or digit names nothing.
        assert_eq!(read("Foo-___-GRP"), None);
        assert_eq!(read("-_--Foo-Bar-GRP"), pair("Foo", "Bar"));
    }

    fn split_of(directory: &str) -> Option<SceneSplit> {
        parse_release_directory(directory)?.split()
    }

    #[test]
    fn a_release_type_word_is_never_read_as_the_album() {
        // The artist is still read; the title is not.
        for (directory, artist) in [
            ("Fazed-EP-VLS-DE-2000-GRP", "Fazed"),
            ("hybrid_soul-EP-2001-GRP", "hybrid soul"),
            ("Foo-Single-2001-GRP", "Foo"),
            ("Foo-Maxi_Single-2001-GRP", "Foo"),
            ("Foo-CDS-2001-GRP", "Foo"),
            ("Foo-Promo-2001-GRP", "Foo"),
        ] {
            let split = split_of(directory).unwrap_or_else(|| panic!("{directory}"));
            assert_eq!(split.artist.as_deref(), Some(artist), "{directory}");
            assert_eq!(split.title, None, "{directory}");
        }
        // Words that are real titles stay titles.
        assert_eq!(read("DdF-Demo-2001-PMS"), pair("DdF", "Demo"));
        assert_eq!(read("Foo-Live-2001-GRP"), pair("Foo", "Live"));
        assert_eq!(read("Foo-EP_Two-2001-GRP"), pair("Foo", "EP Two"));
    }

    /// A release with no tracks in `directory`, with the given tags.
    fn tagged(directory: &str, album: Option<&str>, artist: Option<&str>) -> LocalRelease {
        LocalRelease {
            id: 1,
            directory: directory.to_string(),
            album_title: album.map(str::to_string),
            album_artist: artist.map(str::to_string),
            artist_kind: if artist.is_some() {
                ArtistKind::AlbumArtist
            } else {
                ArtistKind::Unknown
            },
            year: None,
            barcode: None,
            catalog_number: None,
            embedded_discogs_id: None,
            embedded_evidence: EmbeddedReleaseEvidence::default(),
            is_compilation: false,
            has_album_tag: album.is_some(),
            revision: "rev".to_string(),
            tracks: Vec::new(),
        }
    }

    type Reading = (Option<String>, Option<String>, bool);

    fn reading(local: &LocalRelease) -> Option<Reading> {
        directory_split(local).map(|split| (split.artist, split.title, split.various))
    }

    fn some(artist: &str, title: &str) -> Option<Reading> {
        Some((Some(artist.to_string()), Some(title.to_string()), false))
    }

    #[test]
    fn the_directory_reading_takes_the_structure_then_the_artist_then_the_album_tag() {
        // The structure decides, whatever the tags say.
        let mobb = "/mp3/Mobb_Deep-Life_Of_The_Infamous-2006-CMS";
        assert_eq!(
            reading(&tagged(mobb, Some("Whatever"), Some("Various Artists"))),
            some("Mobb Deep", "Life Of The Infamous")
        );
        // Then the artist tag.
        let wu = "/mp3/Wu-Tang_Clan-Wu-Tang_Forever-1997-KSi_INT";
        assert_eq!(
            reading(&tagged(wu, None, Some("Wu-Tang Clan"))),
            some("Wu-Tang Clan", "Wu-Tang Forever")
        );
        // Then the album tag, when the artist tag fits no cut.
        let stresmatic = "/mp3/Stresmatic_and_Droop-E-4005-WEB-2021-RAGEMP3";
        assert_eq!(
            reading(&tagged(stresmatic, Some("4005"), Some("Stresmatic"))),
            some("Stresmatic and Droop-E", "4005")
        );
        // The artist tag is asked before the album tag.
        assert_eq!(
            reading(&tagged(
                "/mp3/Alpha-Beta-Gamma-2001-GRP",
                Some("Gamma"),
                Some("Alpha")
            )),
            some("Alpha", "Beta-Gamma")
        );
        // A filler album tag or a Various Artists release decides nothing;
        // nothing decides: no reading.
        assert_eq!(reading(&tagged(stresmatic, None, Some("Stresmatic"))), None);
        assert_eq!(
            reading(&tagged(
                "/mp3/Some-Band-Unknown_Title-2001-GRP",
                Some("Unknown Title"),
                None
            )),
            None
        );
        let mut various = tagged(wu, None, Some("Wu-Tang Clan"));
        various.artist_kind = ArtistKind::VariousArtists;
        assert_eq!(reading(&various), None);
        // A filler title in the name is no title; the artist still reads.
        assert_eq!(
            reading(&tagged(
                "/mp3/Squeezetoy-Unknown_Title-2001-PMS",
                None,
                None
            )),
            Some((Some("Squeezetoy".to_string()), None, false))
        );
        // A Various Artists name.
        assert_eq!(
            reading(&tagged("/mp3/VA-Thirteen_Days_OST-2000-RNS", None, None)),
            Some((None, Some("Thirteen Days".to_string()), true))
        );
        // Not a scene name.
        assert_eq!(
            reading(&tagged("/music/blue", Some("Blue"), Some("Test Artist"))),
            None
        );
    }

    #[test]
    fn a_split_release_is_not_read() {
        // Which band is the artist and what the title is cannot be told.
        for directory in [
            "Rambo_and_Crucial_Unit-Split-EP-2000-GRP",
            "Samael_-_Tiamat-Live_March_Split-2001-GRP",
            "VA-Jane._Vs_Shaft-Chorus_of_Doom-Split_EP-2001-GRP",
            "Foo-Bar-Split-2001-GRP",
            "Foo-Bar_Split_EP-2001-GRP",
            "Foo-Split_EP-VLS-2001-GRP",
        ] {
            assert_eq!(split_of(directory), None, "{directory}");
            let name = parse_release_directory(directory);
            assert_eq!(
                name.and_then(|name| name.split_matching(|_| true)),
                None,
                "{directory}"
            );
        }
    }

    #[test]
    fn source_tags_of_later_releases_are_dropped() {
        for (directory, artist, title) in [
            ("Batushka-MARIA-WEB-2022-ENTiTLED", "Batushka", "MARIA"),
            (
                "Hot_Milk-A_Call_To_The_Void-(WEB)-2023-RUMBLE",
                "Hot Milk",
                "A Call To The Void",
            ),
            ("Foo-Bar-REPACK-2001-GRP", "Foo", "Bar"),
            ("Foo-Bar-(REPACK)-2001-GRP", "Foo", "Bar"),
            ("Foo-Live_In_Bar-SAT-2001-GRP", "Foo", "Live In Bar"),
            ("Foo-Live_In_Bar-SBD-2CD-2001-GRP", "Foo", "Live In Bar"),
            ("Foo-Bar-CN-2001-GRP", "Foo", "Bar"),
            ("Foo-Bar-WEB-KR-2001-GRP", "Foo", "Bar"),
        ] {
            assert_eq!(read(directory), pair(artist, title), "{directory}");
        }
        // A title that is one of these words stays a title.
        assert_eq!(read("Foo-Web-2001-GRP"), pair("Foo", "Web"));
    }
}
