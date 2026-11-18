use crate::output::{Clean, clean_path};
use crate::providers::Mbid;
use anyhow::{Context, Result};
use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagType};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Cursor, ErrorKind, Read, Seek};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub(crate) const AUDIO_EXTENSIONS: &[&str] =
    &["mp3", "flac", "m4a", "ogg", "opus", "wav", "aac", "wma"];

/// Observed MusicBrainz identifiers, kept as raw strings so malformed and
/// conflicting values survive for diagnostics instead of being dropped.
///
/// Valid UUIDs are canonicalized (lowercase, hyphenated) and the lists are
/// sorted and deduplicated. Invalid values stay trimmed and unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMusicBrainzIds {
    #[serde(default)]
    pub release: Vec<String>,
    #[serde(default)]
    pub release_group: Vec<String>,
    #[serde(default)]
    pub release_track: Vec<String>,
    #[serde(default)]
    pub recording: Vec<String>,
}

impl ObservedMusicBrainzIds {
    pub fn is_empty(&self) -> bool {
        self.release.is_empty()
            && self.release_group.is_empty()
            && self.release_track.is_empty()
            && self.recording.is_empty()
    }
}

/// Canonicalizes valid UUIDs and keeps every other observation verbatim.
fn canonicalize(values: Vec<String>) -> Vec<String> {
    let mut values: Vec<String> = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| match value.parse::<Mbid>() {
            Ok(id) => id.to_string(),
            Err(_) => value,
        })
        .collect();
    values.sort();
    values.dedup();
    values
}

#[derive(Debug, Clone)]
pub struct ScannedTrack {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i64>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub barcode: Option<String>,
    pub catalog_number: Option<String>,
    pub discogs_release_id: Option<i64>,
    pub musicbrainz: ObservedMusicBrainzIds,
    pub duration_secs: Option<f64>,
    pub modified_at: i64,
}

/// Case-insensitive extension check so imports from network drives with
/// inconsistent casing don't get skipped or double-imported.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Reads every value for Lofty's four MusicBrainz keys. Recording and
/// release-track identifiers are distinct concepts and never merged.
pub fn extract_musicbrainz_ids(tag: &Tag) -> ObservedMusicBrainzIds {
    let mut ids = ObservedMusicBrainzIds::default();

    for item in tag.items() {
        let ItemValue::Text(value) = item.value() else {
            continue;
        };
        match item.key() {
            ItemKey::MusicBrainzReleaseId => ids.release.push(value.clone()),
            ItemKey::MusicBrainzReleaseGroupId => ids.release_group.push(value.clone()),
            ItemKey::MusicBrainzTrackId => ids.release_track.push(value.clone()),
            ItemKey::MusicBrainzRecordingId => ids.recording.push(value.clone()),
            _ => {}
        }
    }

    ids.release = canonicalize(ids.release);
    ids.release_group = canonicalize(ids.release_group);
    ids.release_track = canonicalize(ids.release_track);
    ids.recording = canonicalize(ids.recording);

    ids
}

/// Modification time as whole seconds since the Unix epoch.
///
/// Shared by [`read_tags`] and the incremental scan so a stored value always
/// compares equal to the one computed during the pre-read stat.
fn modified_at(metadata: &std::fs::Metadata) -> Result<i64> {
    Ok(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

pub fn read_tags(path: &Path) -> Result<ScannedTrack> {
    read_tags_noting(path).map(|(track, _)| track)
}

/// What a read had to leave out of a file to read it at all.
#[derive(Debug)]
struct Dropped {
    /// What was left out, as the import's note names it.
    what: String,
    /// Why: the error of the read that could not leave it in.
    error: anyhow::Error,
}

impl Dropped {
    /// Fields of a tag that Lofty's relaxed mode skipped.
    fn fields(error: anyhow::Error) -> Self {
        Self {
            what: "malformed tag fields".to_string(),
            error,
        }
    }
}

/// Reads a file's tags with Lofty's default parsing, or leniently when that
/// fails.
///
/// One malformed frame, such as a year written `199x`, makes the default mode
/// give up on the whole tag and so on every field in it. The relaxed mode
/// drops only the fields it cannot parse. A tag whose header says ID3v2.3 over
/// ID3v2.2 frames is read as the ID3v2.2 tag it is, see [`mislabeled_id3v22`].
/// A file those reads refuse gets the further reads of [`read_damaged`]. All
/// of them run only after the default read failed, so every file the default
/// mode can read keeps exactly its values.
fn read_file(path: &Path) -> Result<(TaggedFile, Option<Dropped>)> {
    let strict = match Probe::open(path)?.read() {
        Ok(file) => return Ok((file, None)),
        Err(error) => anyhow::Error::from(error),
    };
    // The type the extension names, as `Probe::open` uses.
    let named = FileType::from_path(path);
    let relabeled = relabeled_id3v22(path);
    let read = match &relabeled {
        Some(bytes) => read_leniently(|| Ok(probe_bytes(bytes, named))),
        None => read_relaxed(Probe::open(path)?, strict),
    };
    match read {
        Ok(read) => Ok(read),
        Err(error) => read_damaged(path, named, relabeled, error),
    }
}

/// Reads a file that Lofty refused as the type its extension names.
///
/// A file whose first bytes carry the signature of another type is read as
/// that type: an Ogg Vorbis file named `.mp3` looks to Lofty like an MP3
/// without any audio. An MPEG file is then read with the tags Lofty refused
/// hidden from it, see [`read_mpeg_hiding_tags`]. When every read fails,
/// gives the error that says most about the file: that of the type its
/// signature names, or else `error`.
fn read_damaged(
    path: &Path,
    named: Option<FileType>,
    relabeled: Option<Vec<u8>>,
    mut error: anyhow::Error,
) -> Result<(TaggedFile, Option<Dropped>)> {
    let signed = signed_type(path);
    if let Some(signed) = signed
        && Some(signed) != named
    {
        match read_leniently(|| Ok(Probe::open(path)?.set_file_type(signed))) {
            Ok(read) => return Ok(read),
            Err(signed_error) => error = signed_error,
        }
    }
    if signed.or(named) != Some(FileType::Mpeg) {
        return Err(error);
    }
    match relabeled.or_else(|| std::fs::read(path).ok()) {
        Some(bytes) => read_mpeg_hiding_tags(bytes, error),
        None => Err(error),
    }
}

/// How many bytes Lofty reads to tell a file's type by its signature. An Ogg
/// file names its codec within them.
const SIGNATURE_LEN: u64 = 36;

/// The type a file's first bytes name by a signature Lofty knows, such as
/// `OggS` or `fLaC`.
///
/// Lofty's own guess goes further, past an ID3v2 tag or other bytes: it
/// takes a file for MPEG when its first kilobyte there holds two bytes that
/// look like the start of an MPEG frame header, as many headers of other
/// formats do by chance. Such a guess is not reason enough to read a file as
/// another type than its extension names.
fn signed_type(path: &Path) -> Option<FileType> {
    let mut start = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SIGNATURE_LEN)
        .read_to_end(&mut start)
        .ok()?;
    FileType::from_buffer(&start)
}

/// A probe that reads `bytes` as `file_type`.
fn probe_bytes(bytes: &[u8], file_type: Option<FileType>) -> Probe<Cursor<&[u8]>> {
    let probe = Probe::new(Cursor::new(bytes));
    match file_type {
        Some(file_type) => probe.set_file_type(file_type),
        None => probe,
    }
}

/// Reads a file with Lofty's default parsing, or leniently when that fails.
///
/// `probe` makes a fresh probe for each read. When both reads fail, gives
/// the default read's error.
fn read_leniently<R: Read + Seek>(
    probe: impl Fn() -> Result<Probe<R>>,
) -> Result<(TaggedFile, Option<Dropped>)> {
    match probe()?.read() {
        Ok(file) => Ok((file, None)),
        Err(error) => read_relaxed(probe()?, error.into()),
    }
}

/// Reads `probe` in Lofty's relaxed mode, after the default mode failed with
/// `strict`.
fn read_relaxed<R: Read + Seek>(
    probe: Probe<R>,
    strict: anyhow::Error,
) -> Result<(TaggedFile, Option<Dropped>)> {
    let relaxed = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    match probe.options(relaxed).read() {
        Ok(file) => Ok((file, Some(Dropped::fields(strict)))),
        // The default mode's error says more about the file.
        Err(_) => Err(strict),
    }
}

/// How an ID3v2.3 tag header starts: `ID3`, major version 3, revision 0 and
/// no flags.
///
/// Flags would announce unsynchronisation, an extended header or (from v2.4)
/// a footer, which change how the frames are stored; ID3v2.2 has none of
/// them, so a tag with flags is never taken for one.
const ID3V23_HEADER_START: &[u8] = b"ID3\x03\x00\x00";

/// Length of an ID3v2 tag header: [`ID3V23_HEADER_START`] and the tag size.
const ID3V2_HEADER_LEN: usize = 10;

/// Where an ID3v2 tag header holds its major version, right after `ID3`.
const ID3V2_VERSION_AT: usize = 3;

/// Whether `bytes` begin with an ID3v2.3 header over ID3v2.2 frames.
///
/// Some taggers from around 2002 wrote ID3v2.2 frames, a three-character ID
/// and a three-byte size with no flags, under a header that says ID3v2.3.
/// Read as v2.3, every frame header runs four bytes into the frame, so
/// sizes and flags come from the text: Lofty saw the "ar" of "Various
/// Artists" as an encryption flag, or a frame size that swallowed the rest
/// of the tag.
///
/// The frames must fill the tag exactly, up to zero padding. Real v2.3
/// frames do not: read as a v2.2 size, the fourth character of a v2.3 frame
/// ID makes the first frame at least three megabytes long.
fn mislabeled_id3v22(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..ID3V2_HEADER_LEN) else {
        return false;
    };
    if !header.starts_with(ID3V23_HEADER_START) {
        return false;
    }
    // The tag size is synchsafe: seven bits per byte, the top bit clear.
    let size = header[ID3V23_HEADER_START.len()..]
        .iter()
        .try_fold(0usize, |size, &byte| {
            (byte < 0x80).then_some(size << 7 | usize::from(byte))
        });
    let Some(mut rest) = size.and_then(|size| bytes.get(ID3V2_HEADER_LEN..ID3V2_HEADER_LEN + size))
    else {
        return false;
    };

    let mut frames = 0;
    loop {
        rest = match rest {
            // Padding, if any, fills the rest of the tag.
            [] | [0, ..] => return frames > 0 && rest.iter().all(|&byte| byte == 0),
            // A frame ID of three capitals or digits, then a big-endian size.
            [a, b, c, s2, s1, s0, content @ ..]
                if [a, b, c]
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) =>
            {
                let len = usize::from(*s2) << 16 | usize::from(*s1) << 8 | usize::from(*s0);
                match content.get(len..) {
                    Some(next) => next,
                    None => return false,
                }
            }
            _ => return false,
        };
        frames += 1;
    }
}

/// The file's bytes, relabeled as the ID3v2.2 tag they hold when
/// [`mislabeled_id3v22`].
///
/// Any I/O error also gives `None`: the caller then reports why Lofty could
/// not read the file.
fn relabeled_id3v22(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    // Only a file that starts like an ID3v2.3 tag is read in full.
    let mut bytes = vec![0; ID3V2_HEADER_LEN];
    file.read_exact(&mut bytes).ok()?;
    if !bytes.starts_with(ID3V23_HEADER_START) {
        return None;
    }
    file.read_to_end(&mut bytes).ok()?;
    if !mislabeled_id3v22(&bytes) {
        return None;
    }
    bytes[ID3V2_VERSION_AT] = 2;
    Some(bytes)
}

/// How an ID3v2 tag header starts.
const ID3V2_MAGIC: &[u8] = b"ID3";

/// The ID3v2 major versions Lofty reads. It refuses a file with any other
/// version in a tag header where it expects one.
const ID3V2_VERSIONS: std::ops::RangeInclusive<u8> = 2..=4;

/// Where an ID3v2 tag header holds its flags, after the version.
const ID3V2_FLAGS_AT: usize = 5;

/// The ID3v2 header flag for a footer: a copy of the header, of the same
/// length, after the tag.
const ID3V2_FOOTER_FLAG: u8 = 0x10;

/// Where an ID3v2 tag header holds its synchsafe size, after the flags.
const ID3V2_SIZE_AT: usize = 6;

/// How an ID3v1 tag starts.
const ID3V1_MAGIC: &[u8] = b"TAG";

/// Length of an ID3v1 tag, which ends the file.
const ID3V1_LEN: usize = 128;

/// How the footer (and the header) of an APE tag starts.
const APE_MAGIC: &[u8] = b"APETAGEX";

/// Length of an APE tag footer, which ends the tag, and of the header that
/// may start it.
const APE_FOOTER_LEN: usize = 32;

/// Where an APE tag footer holds the size of the items and the footer, as a
/// little-endian `u32`.
const APE_SIZE_AT: usize = 12;

/// Where an MPEG file holds the tags Lofty reads.
#[derive(Debug, Default, PartialEq, Eq)]
struct MpegTags {
    /// Where each ID3v2 tag at the start of the file begins. Lofty reads them
    /// one after another and merges them.
    id3v2: Vec<usize>,
    /// Where an `ID3` of a version Lofty does not know begins, right after
    /// those tags. Lofty takes it for a tag header and refuses the whole
    /// file.
    unknown_id3v2: Option<usize>,
    /// The whole of an APE tag, which Lofty reads from its footer.
    ape: Option<Range<usize>>,
}

/// Finds the tags of an MPEG file where Lofty looks for them.
///
/// Lofty reads ID3v2 tags from the start of the file, past any zero bytes,
/// for as long as one follows another. It reads an APE tag whose footer ends
/// the file or comes right before the ID3v1 tag; it checks for a Lyrics3v2
/// block there too, but compares nine bytes with eight and never finds one.
fn find_mpeg_tags(bytes: &[u8]) -> MpegTags {
    let mut tags = MpegTags::default();
    let mut at = bytes
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(bytes.len());
    while let Some(header) = bytes
        .get(at..at + ID3V2_HEADER_LEN)
        .filter(|header| header.starts_with(ID3V2_MAGIC))
    {
        let version = header[ID3V2_VERSION_AT];
        if !ID3V2_VERSIONS.contains(&version) {
            tags.unknown_id3v2 = Some(at);
            break;
        }
        tags.id3v2.push(at);
        // Lofty ignores the top bit of each size byte rather than refuse it.
        let size = header[ID3V2_SIZE_AT..]
            .iter()
            .fold(0, |size, &byte| size << 7 | usize::from(byte & 0x7f));
        // Lofty skips a footer in an ID3v2.3 tag too.
        let footer = if version >= 3 && header[ID3V2_FLAGS_AT] & ID3V2_FOOTER_FLAG != 0 {
            ID3V2_HEADER_LEN
        } else {
            0
        };
        at += ID3V2_HEADER_LEN + size + footer;
    }
    let end = match bytes.len().checked_sub(ID3V1_LEN) {
        Some(id3v1) if bytes[id3v1..].starts_with(ID3V1_MAGIC) => id3v1,
        _ => bytes.len(),
    };
    tags.ape = end
        .checked_sub(APE_FOOTER_LEN)
        .filter(|&footer| bytes[footer..].starts_with(APE_MAGIC))
        .map(|footer| ape_start(bytes, footer)..footer + APE_FOOTER_LEN);
    tags
}

/// Where the APE tag whose footer is at `footer` starts: at its header, if
/// it has one, or else at its first item.
///
/// A size in the footer that does not fit in front of it is not trusted, and
/// the tag is taken to be the footer alone.
fn ape_start(bytes: &[u8], footer: usize) -> usize {
    let size = bytes
        .get(footer + APE_SIZE_AT..footer + APE_SIZE_AT + 4)
        .and_then(|size| size.try_into().ok())
        .map(u32::from_le_bytes)
        .and_then(|size| usize::try_from(size).ok());
    let Some(items) = size
        .filter(|&size| size >= APE_FOOTER_LEN)
        .and_then(|size| (footer + APE_FOOTER_LEN).checked_sub(size))
    else {
        return footer;
    };
    match items.checked_sub(APE_FOOTER_LEN) {
        Some(header) if bytes[header..].starts_with(APE_MAGIC) => header,
        _ => items,
    }
}

/// Reads an MPEG file with the tags Lofty refused hidden from it.
///
/// Lofty gives up on the whole file when one of its tags fails to parse, even
/// in relaxed mode, and so loses the tags it could read and the duration
/// too. This reads `bytes` again with tags zeroed, so that Lofty does not see
/// them: an `ID3` of an unknown version, which holds nothing it could read,
/// and then also the APE tag, the ID3v2 tags, or both, whichever first lets
/// Lofty read the file. Of an ID3v2 tag only the magic bytes are zeroed,
/// since its header may claim a size that runs into the audio; Lofty then
/// finds the audio by its frame headers. An APE tag is zeroed whole, so that
/// nothing in it counts as audio when Lofty searches backwards for the last
/// frame. When no read succeeds, gives `error`.
fn read_mpeg_hiding_tags(
    mut bytes: Vec<u8>,
    error: anyhow::Error,
) -> Result<(TaggedFile, Option<Dropped>)> {
    let tags = find_mpeg_tags(&bytes);
    let magic = |at: usize| at..at + ID3V2_MAGIC.len();
    if let Some(at) = tags.unknown_id3v2 {
        bytes[magic(at)].fill(0);
    }
    let id3v2: Vec<Range<usize>> = tags.id3v2.iter().map(|&at| magic(at)).collect();
    for (what, hide_id3v2, hide_ape) in [
        (None, false, false),
        (Some("the unreadable APE tag"), false, true),
        (Some("the unreadable ID3v2 tag"), true, false),
        (Some("the unreadable ID3v2 and APE tags"), true, true),
    ] {
        let unchanged = tags.unknown_id3v2.is_none() && !hide_id3v2 && !hide_ape;
        if unchanged || hide_id3v2 && id3v2.is_empty() || hide_ape && tags.ape.is_none() {
            continue;
        }
        let hidden: Vec<Range<usize>> = id3v2
            .iter()
            .filter(|_| hide_id3v2)
            .chain(tags.ape.iter().filter(|_| hide_ape))
            .cloned()
            .collect();
        let kept: Vec<Vec<u8>> = hidden
            .iter()
            .map(|range| bytes[range.clone()].to_vec())
            .collect();
        for range in &hidden {
            bytes[range.clone()].fill(0);
        }
        if let Ok((file, fields)) = read_leniently(|| Ok(probe_bytes(&bytes, Some(FileType::Mpeg))))
        {
            let dropped = match (what, fields) {
                (None, fields) => fields,
                (Some(what), None) => Some(Dropped {
                    what: what.to_string(),
                    error,
                }),
                (Some(what), Some(_)) => Some(Dropped {
                    what: format!("{what} and malformed tag fields"),
                    error,
                }),
            };
            return Ok((file, dropped));
        }
        // Put the tags back for the next attempt.
        for (range, kept) in hidden.iter().zip(kept) {
            bytes[range.clone()].copy_from_slice(&kept);
        }
    }
    Err(error)
}

/// Sets an absent or blank `field` to the ID3v1 `value`, if that has text.
///
/// ID3v1 pads each field to a fixed width, with NUL bytes or with spaces;
/// Lofty drops only the NUL bytes, so the spaces are trimmed here.
fn fill(field: &mut Option<String>, value: Option<Cow<'_, str>>) {
    if !field.as_deref().is_none_or(|text| text.trim().is_empty()) {
        return;
    }
    if let Some(text) = value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        *field = Some(text.to_string());
    }
}

/// [`read_tags`], plus what the read had to drop, when it had to.
fn read_tags_noting(path: &Path) -> Result<(ScannedTrack, Option<Dropped>)> {
    let (tagged_file, dropped) = read_file(path)?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let (mut title, mut artist, mut album, mut year, mut track_number) = match tag {
        Some(tag) => (
            tag.title().map(|s| s.to_string()),
            tag.artist().map(|s| s.to_string()),
            tag.album().map(|s| s.to_string()),
            tag.date().map(|ts| ts.year as i64),
            tag.track().map(|t| t as i64),
        ),
        None => (None, None, None, None, None),
    };
    // Old taggers often wrote a field only to the ID3v1 tag at the end of the
    // file, so it fills what the primary tag lacks but never replaces a value.
    if let Some(id3v1) = tagged_file.tag(TagType::Id3v1) {
        fill(&mut title, id3v1.title());
        fill(&mut artist, id3v1.artist());
        fill(&mut album, id3v1.album());
        year = year.or_else(|| id3v1.date().map(|ts| i64::from(ts.year)));
        track_number = track_number.or_else(|| id3v1.track().map(i64::from));
    }

    let album_artist = tag.and_then(|tag| {
        tag.get_string(ItemKey::AlbumArtist)
            .or_else(|| tag.get_string(ItemKey::AlbumArtists))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    let disc_number = tag
        .and_then(|tag| tag.disk())
        .map(i64::from)
        .filter(|disk| *disk > 0);
    let barcode = tag.and_then(|tag| {
        tag.get_string(ItemKey::Barcode)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    let catalog_number = tag.and_then(|tag| {
        tag.get_string(ItemKey::CatalogNumber)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    // Lofty 0.25 has no item key for Discogs identifiers, so an embedded
    // release ID stays absent until a format-specific reader supplies it.
    let discogs_release_id = None;
    let musicbrainz = tag.map(extract_musicbrainz_ids).unwrap_or_default();

    let duration_secs = Some(tagged_file.properties().duration().as_secs_f64());
    let modified_at = modified_at(&std::fs::metadata(path)?)?;

    let track = ScannedTrack {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        album_artist,
        year,
        track_number,
        disc_number,
        barcode,
        catalog_number,
        discogs_release_id,
        musicbrainz,
        duration_secs,
        modified_at,
    };
    Ok((track, dropped))
}

/// Progress notifications, delivered synchronously by the collecting task.
#[derive(Debug, PartialEq, Eq)]
pub enum ScanEvent {
    Discovering,
    /// The walk entered a folder. Sent once per folder, the root included.
    Walking {
        /// Audio files found so far.
        files: usize,
        /// Folders entered so far, this one included.
        folders: usize,
        /// The folder, relative to the root; empty for the root itself.
        folder: PathBuf,
    },
    /// The walk is done.
    Discovered {
        /// Audio files found.
        total: usize,
        /// Of those, the files whose tags need no read.
        unchanged: usize,
        /// Folders the walk entered, the root included.
        folders: usize,
    },
    /// A path the walk could not visit; it is not counted in the totals.
    Warning {
        message: String,
    },
    /// A file's tags were read. Every attempted read counts, including
    /// failures and panicked tasks; `total` is the number of files to read,
    /// without the unchanged ones.
    FileFinished {
        completed: usize,
        total: usize,
        warning: Option<String>,
    },
}

/// Prints the warning a scan event carries, if any, to stderr. For callers
/// that have no progress display to route warnings through.
pub fn eprint_warning(event: ScanEvent) {
    match event {
        ScanEvent::Warning { message }
        | ScanEvent::FileFinished {
            warning: Some(message),
            ..
        } => eprintln!("{}", Clean(&message)),
        ScanEvent::Discovering
        | ScanEvent::Walking { .. }
        | ScanEvent::Discovered { .. }
        | ScanEvent::FileFinished { .. } => {}
    }
}

/// Reads audio tags in parallel, printing warnings for failed reads.
pub async fn scan_directory(root: &Path) -> Result<Vec<ScannedTrack>> {
    scan_directory_with_progress(root, eprint_warning).await
}

/// Outcome of an incremental scan.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Tracks whose tags were read.
    pub tracks: Vec<ScannedTrack>,
    /// Files skipped because `is_unchanged` accepted their modification time.
    pub unchanged: Vec<PathBuf>,
    /// Paths the scan could not get an answer for: files whose tags failed to
    /// read, and every file or folder the walk could not visit, including
    /// ones skipped without a warning. A caller must not conclude that
    /// anything at or below these paths was deleted.
    pub unreadable: Vec<PathBuf>,
    /// The subset of `unreadable` that was skipped without a warning: dangling
    /// links to non-audio paths. A caller that finds stored rows beneath one
    /// should say so, since the scan would not.
    pub quietly_skipped: Vec<PathBuf>,
}

/// Walks `root` and reads audio tags on the Tokio blocking thread pool.
///
/// The callback receives discovery and read-completion events, and owns warning
/// output. No warnings are printed separately by this entrypoint.
pub async fn scan_directory_with_progress(
    root: &Path,
    on_progress: impl FnMut(ScanEvent),
) -> Result<Vec<ScannedTrack>> {
    Ok(scan_directory_incremental(root, |_, _| false, on_progress)
        .await?
        .tracks)
}

/// Like [`scan_directory_with_progress`], but skips files that are unchanged.
///
/// Each file is stat-ed during the walk and `is_unchanged(path, modified_at)`
/// is asked before any tags are read, where `modified_at` is the value
/// [`read_tags`] would store. Skipped files land in [`ScanResult::unchanged`]
/// and are counted in [`ScanEvent::Discovered`], not in the read progress. At
/// most [`read_limit`] reads run at once.
pub async fn scan_directory_incremental(
    root: &Path,
    is_unchanged: impl Fn(&Path, i64) -> bool,
    mut on_progress: impl FnMut(ScanEvent),
) -> Result<ScanResult> {
    on_progress(ScanEvent::Discovering);
    let mut result = ScanResult::default();
    let mut files: Vec<(PathBuf, Option<i64>)> = Vec::new();
    let mut folders = 0;
    // Following links lets symlinked libraries import; walkdir reports loops as errors.
    for entry in WalkDir::new(root).follow_links(true) {
        match entry {
            Ok(entry) if entry.file_type().is_dir() => {
                folders += 1;
                let folder = entry.path().strip_prefix(root).unwrap_or(entry.path());
                on_progress(ScanEvent::Walking {
                    files: files.len(),
                    folders,
                    folder: folder.to_path_buf(),
                });
            }
            Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                // A failed stat is not fatal: the read reports the real error.
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| modified_at(&metadata).ok());
                files.push((entry.into_path(), mtime));
            }
            Ok(_) => {}
            Err(err) => {
                let path = err.path().unwrap_or(root);
                result.unreadable.push(path.to_path_buf());
                let io_error = err.io_error();
                // A dangling link to a non-audio file (a cover image) is not worth a warning.
                if io_error.is_some_and(|e| e.kind() == ErrorKind::NotFound) && !is_audio_file(path)
                {
                    result.quietly_skipped.push(path.to_path_buf());
                    continue;
                }
                // walkdir's own text repeats the path for I/O errors, so show only the cause.
                let message = match io_error {
                    Some(io_error) => {
                        format!("warning: skipped {}: {io_error}", clean_path(path))
                    }
                    None => format!("warning: skipped {}: {err}", clean_path(path)),
                };
                on_progress(ScanEvent::Warning { message });
            }
        }
    }
    let found = files.len();
    let mut to_read = Vec::new();
    for (path, mtime) in files {
        if mtime.is_some_and(|mtime| is_unchanged(&path, mtime)) {
            result.unchanged.push(path);
        } else {
            to_read.push(path);
        }
    }
    on_progress(ScanEvent::Discovered {
        total: found,
        unchanged: result.unchanged.len(),
        folders,
    });

    let total = to_read.len();
    let mut completed = 0;

    run_bounded(
        to_read,
        read_limit(),
        |path| read_tags_noting(path).with_context(|| clean_path(path)),
        |path, outcome| {
            let warning = match outcome {
                Ok(Ok((track, dropped))) => {
                    result.tracks.push(track);
                    dropped.map(|dropped| {
                        format!(
                            "note: dropped {} from {}: {:#}",
                            dropped.what,
                            clean_path(path),
                            dropped.error
                        )
                    })
                }
                Ok(Err(err)) => {
                    result.unreadable.push(path.to_path_buf());
                    Some(format!("warning: failed to read tags: {err:#}"))
                }
                Err(err) => {
                    result.unreadable.push(path.to_path_buf());
                    Some(format!(
                        "warning: scan task panicked: {}: {err}",
                        clean_path(path)
                    ))
                }
            };
            completed += 1;
            on_progress(ScanEvent::FileFinished {
                completed,
                total,
                warning,
            });
        },
    )
    .await;

    Ok(result)
}

/// Maximum concurrent tag reads: twice the CPU count, or 8 when unknown.
///
/// Reads mostly wait on disk, so a small multiple of the core count keeps
/// storage busy without queueing thousands of open files.
fn read_limit() -> usize {
    std::thread::available_parallelism().map_or(8, |count| count.get() * 2)
}

/// Runs `work` for every path on the blocking pool, at most `limit` at a time.
///
/// `on_done` runs on the calling task, in completion order, and receives the
/// path of each job even when its task panicked.
async fn run_bounded<T, W>(
    paths: Vec<PathBuf>,
    limit: usize,
    work: W,
    mut on_done: impl FnMut(&Path, Result<T, tokio::task::JoinError>),
) where
    T: Send + 'static,
    W: Fn(&Path) -> T + Send + Sync + 'static,
{
    let work = Arc::new(work);
    let mut pending = paths.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let mut in_flight = HashMap::new();
    loop {
        while in_flight.len() < limit.max(1) {
            let Some(path) = pending.next() else { break };
            let task_path = path.clone();
            let work = Arc::clone(&work);
            let handle = set.spawn_blocking(move || work(&path));
            in_flight.insert(handle.id(), task_path);
        }
        let Some(joined) = set.join_next_with_id().await else {
            break;
        };
        let (id, outcome) = match joined {
            Ok((id, value)) => (id, Ok(value)),
            Err(err) => (err.id(), Err(err)),
        };
        let path = in_flight
            .remove(&id)
            .expect("every spawned task is tracked");
        on_done(&path, outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn panicked_reads_report_their_path() {
        let paths = vec![PathBuf::from("ok.wav"), PathBuf::from("bad.wav")];
        let mut seen = Vec::new();

        run_bounded(
            paths,
            2,
            |path| {
                if path == Path::new("bad.wav") {
                    panic!("corrupt decoder state");
                }
            },
            |path, outcome| seen.push((path.to_path_buf(), outcome.is_err())),
        )
        .await;

        seen.sort();
        assert_eq!(
            seen,
            [
                (PathBuf::from("bad.wav"), true),
                (PathBuf::from("ok.wav"), false)
            ]
        );
    }

    #[tokio::test]
    async fn bounded_runner_never_exceeds_its_limit() {
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let paths: Vec<PathBuf> = (0..24).map(|i| PathBuf::from(format!("{i}.wav"))).collect();
        let (running, highest) = (Arc::clone(&current), Arc::clone(&peak));
        let mut finished = 0;

        run_bounded(
            paths,
            3,
            move |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                highest.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                running.fetch_sub(1, Ordering::SeqCst);
            },
            |_, outcome| {
                assert!(outcome.is_ok());
                finished += 1;
            },
        )
        .await;

        assert_eq!(finished, 24);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }

    fn tagged_mp3(path: &Path, year: &str) {
        crate::test_support::write_id3v23_mp3(
            path,
            &[
                ("TIT2", "Song", 0),
                ("TPE1", "Artist", 0),
                ("TALB", "Album", 0),
                ("TRCK", "3", 0),
                ("TYER", year, 0),
            ],
        );
    }

    #[test]
    fn a_malformed_year_costs_only_the_year() {
        let dir = crate::test_support::ScratchDir::new("malformed-year");
        // As found in the wild: a placeholder digit, a stray dash, only spaces.
        for year in ["199x", "-1999", "    "] {
            let path = dir.path().join("song.mp3");
            tagged_mp3(&path, year);

            let track = read_tags(&path).unwrap_or_else(|error| panic!("{year:?}: {error:#}"));
            assert_eq!(track.title.as_deref(), Some("Song"), "{year:?}");
            assert_eq!(track.artist.as_deref(), Some("Artist"), "{year:?}");
            assert_eq!(track.album.as_deref(), Some("Album"), "{year:?}");
            assert_eq!(track.track_number, Some(3), "{year:?}");
            assert_eq!(track.year, None, "{year:?}");
            assert!(
                track.duration_secs.is_some_and(|secs| secs > 0.9),
                "{year:?}"
            );
        }

        let path = dir.path().join("song.mp3");
        tagged_mp3(&path, "1999");
        assert_eq!(read_tags(&path).unwrap().year, Some(1999));
    }

    /// What an import of `dir`, which holds one file it can read, prints
    /// about that file.
    async fn import_warnings(dir: &Path) -> Vec<String> {
        let mut warnings = Vec::new();
        let result = scan_directory_incremental(
            dir,
            |_, _| false,
            |event| {
                if let ScanEvent::FileFinished {
                    warning: Some(warning),
                    ..
                } = event
                {
                    warnings.push(warning);
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(result.tracks.len(), 1);
        assert!(result.unreadable.is_empty(), "{:?}", result.unreadable);
        warnings
    }

    #[tokio::test]
    async fn an_import_notes_the_tag_fields_it_dropped() {
        let dir = crate::test_support::ScratchDir::new("malformed-year-scan");
        tagged_mp3(&dir.path().join("song.mp3"), "199x");

        let warnings = import_warnings(dir.path()).await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("note: dropped malformed tag fields from ")
                && warnings[0].contains("song.mp3")
                && warnings[0].contains("unexpected character 'x'"),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn an_import_notes_the_tag_it_dropped() {
        let dir = crate::test_support::ScratchDir::new("dropped-tag-scan");
        let path = dir.path().join("song.mp3");
        tagged_mp3(&path, "1999");
        crate::test_support::append_ape(&path, &[("Album", b"4\xe8me")]);

        let warnings = import_warnings(dir.path()).await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("note: dropped the unreadable APE tag from ")
                && warnings[0].contains("song.mp3")
                && warnings[0].contains("APE tag item 'Album'"),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_id3v2_tag_with_an_encrypted_frame_without_its_length_is_dropped() {
        use crate::test_support::{id3v23_frames, id3v23_tag};
        let dir = crate::test_support::ScratchDir::new("encrypted-frame");
        let path = dir.path().join("song.mp3");
        let frames = [("TIT2", "Song", 0), ("TENC", "Encoder", 0x0040)];
        crate::test_support::write_id3v23_mp3(&path, &frames);
        // Lofty refuses this in every parsing mode, so the file is read
        // without its ID3v2 tag.
        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.title, None);
        assert!(track.duration_secs.is_some_and(|secs| secs > 0.9));
        let dropped = dropped.unwrap();
        assert_eq!(dropped.what, "the unreadable ID3v2 tag");
        let error = format!("{:#}", dropped.error);
        assert!(error.contains("encrypted frame"), "{error}");

        // With no audio either, nothing is left to read: the file stays
        // unreadable, its stored row, if any, is kept, and the warning says
        // why the tag failed.
        std::fs::write(&path, id3v23_tag(&id3v23_frames(&frames))).unwrap();
        let error = format!("{:#}", read_tags(&path).unwrap_err());
        assert!(error.contains("encrypted frame"), "{error}");
    }

    #[test]
    fn a_file_in_another_format_than_its_extension_names_is_read_as_that_format() {
        let dir = crate::test_support::ScratchDir::new("other-format");
        // As found in the wild: an Ogg Vorbis file named `.mp3`. A tagged WAV
        // file stands in for it.
        let wav = dir.path().join("song.wav");
        crate::test_support::write_tagged_wav(&wav, "Tools Of The Trade", 1992);
        let path = dir.path().join("song.mp3");
        std::fs::rename(&wav, &path).unwrap();
        assert!(Probe::open(&path).unwrap().read().is_err());

        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.album.as_deref(), Some("Tools Of The Trade"));
        assert_eq!(track.year, Some(1992));
        assert!(track.duration_secs.is_some());
        assert!(dropped.is_none(), "{dropped:?}");

        // A file that fails as that format too is reported as that format.
        std::fs::write(&path, b"RIFF\x04\x00\x00\x00WAVE").unwrap();
        let error = format!("{:#}", read_tags(&path).unwrap_err());
        assert!(error.contains("Wav"), "{error}");
    }

    #[test]
    fn a_format_lofty_only_guesses_from_frame_sync_bytes_is_not_trusted() {
        let dir = crate::test_support::ScratchDir::new("guessed-format");
        let path = dir.path().join("song.wma");
        // Lofty cannot read WMA, and takes any file whose first kilobyte
        // holds bytes that look like an MPEG frame header for MPEG. Many WMA
        // headers do by chance; here, real MPEG frames after a few bytes of
        // something else stand in for one.
        crate::test_support::write_mp3(&path, b"\x30\x26\xb2\x75\x8e\x66\xcf\x11");
        assert!(read_tags(&path).is_err());
    }

    #[test]
    fn an_empty_id3_header_after_the_tag_is_passed_over() {
        use crate::test_support::{id3v23_frames, id3v23_tag, write_mp3};
        let dir = crate::test_support::ScratchDir::new("empty-id3-header");
        let path = dir.path().join("song.mp3");
        // As found in the wild: a padded ID3v2.3 tag, then `ID3` and eight
        // zero bytes, then the audio. Lofty takes those for the header of a
        // tag of version 0 and refuses the whole file.
        let frames = id3v23_frames(&[
            ("TPE1", "Morrissey", 0),
            ("TALB", "Who Put the M in Manchester", 0),
            ("TIT2", "First Of The Gang To Die", 0),
            ("TRCK", "1/24", 0),
            ("TYER", "2005", 0),
        ]);
        let tag = id3v23_tag(&[frames, vec![0; 64]].concat());
        write_mp3(&path, &[tag, b"ID3".to_vec(), vec![0; 8]].concat());

        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.artist.as_deref(), Some("Morrissey"));
        assert_eq!(track.album.as_deref(), Some("Who Put the M in Manchester"));
        assert_eq!(track.title.as_deref(), Some("First Of The Gang To Die"));
        assert_eq!(track.track_number, Some(1));
        assert_eq!(track.year, Some(2005));
        assert!(track.duration_secs.is_some_and(|secs| secs > 0.9));
        // The header holds nothing, so nothing was dropped.
        assert!(dropped.is_none(), "{dropped:?}");
    }

    #[test]
    fn an_ape_tag_lofty_refuses_is_dropped_and_the_other_tags_read() {
        use crate::test_support::{append_ape, append_id3v1, write_id3v23_mp3};
        let dir = crate::test_support::ScratchDir::new("latin1-ape");
        let path = dir.path().join("song.mp3");
        write_id3v23_mp3(
            &path,
            &[
                ("TIT2", "Sabotage", 0),
                ("TPE1", "Severin 24", 0),
                ("TALB", "Rencontre Du 4eme Type", 0),
                ("TYER", "2005", 0),
            ],
        );
        // As found in the wild: APE text must be UTF-8, but this album title
        // is Latin-1, its `è` a single byte.
        append_ape(
            &path,
            &[
                ("Title", b"Sabotage"),
                ("Album", b"Rencontre Du 4\xe8me Type"),
            ],
        );
        append_id3v1(
            &path,
            ["Sabotage", "Severin 24", "Rencontre Du 4eme Type", "2005"],
            1,
        );

        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("Sabotage"));
        assert_eq!(track.artist.as_deref(), Some("Severin 24"));
        assert_eq!(track.album.as_deref(), Some("Rencontre Du 4eme Type"));
        assert_eq!(track.year, Some(2005));
        assert!(track.duration_secs.is_some_and(|secs| secs > 0.9));
        let dropped = dropped.unwrap();
        assert_eq!(dropped.what, "the unreadable APE tag");
        let error = format!("{:#}", dropped.error);
        assert!(error.contains("APE tag item 'Album'"), "{error}");
    }

    #[test]
    fn a_dropped_ape_tag_does_not_count_toward_the_duration() {
        use crate::test_support::{append_ape, write_id3v23_mp3};
        let dir = crate::test_support::ScratchDir::new("ape-duration");
        let path = dir.path().join("song.mp3");
        write_id3v23_mp3(&path, &[("TIT2", "Song", 0)]);
        // A large item, such as a cover image, that holds bytes that look
        // like one of the file's MPEG frame headers. Lofty finds the end of
        // the audio by searching backwards for such a header.
        let cover = [&[0; 150_000][..], &[0xff, 0xfb, 0x90, 0x00], &[0; 64]].concat();
        append_ape(&path, &[("Cover", &cover)]);

        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("Song"));
        let secs = track.duration_secs.unwrap();
        assert!((1.0..1.1).contains(&secs), "{secs}");
        assert_eq!(dropped.unwrap().what, "the unreadable APE tag");
    }

    #[test]
    fn a_corrupt_id3v2_tag_is_dropped_and_the_id3v1_tag_read() {
        use crate::test_support::{append_id3v1, write_mp3};
        let dir = crate::test_support::ScratchDir::new("corrupt-id3v2");
        let path = dir.path().join("song.mp3");
        // As found in the wild: ID3v2.3 headers that claim thousands of
        // bytes, over a few dozen bytes of mangled frames with ID3v2.2 IDs,
        // then the audio. Lofty reads a letter where a frame's text encoding
        // belongs.
        let public_enemy = b"ID3\x03\x00\x00\x00\x00\x26\x54\
            TT2\x00\x00\x00\x0a\x00\x00\x00k 04\x00TRK\x00\x00\x06\x004/18\x00"
            .to_vec();
        let horse = [
            &b"ID3\x03\x00\x00\x00\x01\x62\x4eCOM\x00\x00\x00\x68\x00\x00\x00TunNORM\x00"[..],
            b" 00001B5B 00001EE2 00005A39 00006E03 00007216 00004D78 00008000 00008000 \
            00004D61 00004D4A\x00",
        ]
        .concat();
        let public_enemy_v1 = [
            "Bring The Noise",
            "Public Enemy",
            "Power To The People And The Be",
            "2005",
        ];
        let horse_v1 = [
            "Opening Montage",
            "HORSE the Band",
            "Effing 69 Tour",
            "2005",
        ];

        // Where each header says its tag ends.
        for (tag, id3v1, number, claimed_end) in [
            (public_enemy, public_enemy_v1, 4, 4958),
            (horse, horse_v1, 1, 29016),
        ] {
            write_mp3(&path, &tag);
            // In real audio, bytes that look like an AAC frame header may
            // follow there, so that Lofty would guess the file is AAC.
            let mut bytes = std::fs::read(&path).unwrap();
            if let Some(sync) = bytes.get_mut(claimed_end..claimed_end + 2) {
                sync.copy_from_slice(&[0xff, 0xf1]);
            }
            std::fs::write(&path, bytes).unwrap();
            append_id3v1(&path, id3v1, number);

            let (track, dropped) =
                read_tags_noting(&path).unwrap_or_else(|error| panic!("{id3v1:?}: {error:#}"));
            assert_eq!(track.title.as_deref(), Some(id3v1[0]));
            assert_eq!(track.artist.as_deref(), Some(id3v1[1]));
            assert_eq!(track.album.as_deref(), Some(id3v1[2]));
            assert_eq!(track.year, Some(2005));
            assert_eq!(track.track_number, Some(i64::from(number)));
            // All of the audio counts, from right after the frames.
            let secs = track.duration_secs.unwrap();
            assert!((1.0..1.1).contains(&secs), "{id3v1:?}: {secs}");
            let dropped = dropped.unwrap();
            assert_eq!(dropped.what, "the unreadable ID3v2 tag");
            let error = format!("{:#}", dropped.error);
            assert!(error.contains("invalid text encoding"), "{error}");
        }
    }

    #[test]
    fn mpeg_tags_are_found_where_lofty_looks_for_them() {
        use crate::test_support::{ape_tag, id3v23_tag};
        let audio = [0xff; 64];
        let tag = id3v23_tag(&[0; 4]);
        // Lofty skips zero bytes, reads ID3v2 tags one after another, and
        // stops at an `ID3` of a version it does not know.
        let unknown = b"ID3\x00\x00\x00\x00\x00\x00\x00";
        let bytes = [&[0, 0][..], &tag, &tag, unknown, &audio].concat();
        assert_eq!(
            find_mpeg_tags(&bytes),
            MpegTags {
                id3v2: vec![2, 16],
                unknown_id3v2: Some(30),
                ape: None,
            }
        );
        // A footer follows the tag; Lofty allows one in ID3v2.3 too.
        for version in [4, 3] {
            let mut footed =
                *b"ID3\x04\x00\x10\x00\x00\x00\x02\x00\x003DI\x04\x00\x10\x00\x00\x00\x02";
            footed[3] = version;
            let bytes = [&footed[..], &tag, &audio].concat();
            assert_eq!(find_mpeg_tags(&bytes).id3v2, [0, 22]);
        }

        // An APE tag ends the file, or comes right before an ID3v1 tag, and
        // may start with a header.
        let ape = ape_tag(&[("Album", b"Album")]);
        let bytes = [&audio[..], &ape].concat();
        assert_eq!(find_mpeg_tags(&bytes).ape, Some(64..bytes.len()));
        let id3v1 = [&b"TAG"[..], &[0; 125]].concat();
        let bytes = [&audio[..], &ape, &id3v1].concat();
        assert_eq!(find_mpeg_tags(&bytes).ape, Some(64..bytes.len() - 128));
        let header = [&b"APETAGEX"[..], &[0; 24]].concat();
        let bytes = [&audio[..], &header, &ape].concat();
        assert_eq!(find_mpeg_tags(&bytes).ape, Some(64..bytes.len()));
        // A footer whose size does not fit covers only itself.
        let mut oversized = ape.clone();
        let size_at = ape.len() - 32 + 12;
        oversized[size_at..size_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let bytes = [&audio[..], &oversized].concat();
        assert_eq!(
            find_mpeg_tags(&bytes).ape,
            Some(bytes.len() - 32..bytes.len())
        );
        // Lofty looks nowhere else.
        let bytes = [&audio[..], &ape, &[0; 16]].concat();
        assert_eq!(find_mpeg_tags(&bytes), MpegTags::default());
    }

    /// Writes an MP3 whose tag header says ID3v2.3 but whose `frames` are
    /// ID3v2.2 frames, followed by zero padding, as some taggers wrote them.
    fn mislabeled_mp3(path: &Path, frames: &[(&str, &str)]) {
        use crate::test_support::{id3v22_frames, id3v23_tag, write_mp3};
        write_mp3(
            path,
            &id3v23_tag(&[id3v22_frames(frames), vec![0; 64]].concat()),
        );
    }

    #[test]
    fn a_v23_header_over_v22_frames_is_read_as_v22() {
        let dir = crate::test_support::ScratchDir::new("mislabeled-v22");
        let path = dir.path().join("song.mp3");
        // As found in the wild. Read as v2.3, each frame header runs into the
        // frame's text, and the "ar" of "Various Artists" sets the
        // encryption flag.
        mislabeled_mp3(
            &path,
            &[("TP1", "Various Artists"), ("TAL", "The Punk Rock Movie")],
        );
        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.artist.as_deref(), Some("Various Artists"));
        assert_eq!(track.album.as_deref(), Some("The Punk Rock Movie"));
        assert!(track.duration_secs.is_some_and(|secs| secs > 0.9));
        assert!(dropped.is_none(), "{dropped:?}");

        // Read as v2.3, the first frame's size swallows the rest of the tag,
        // so a relaxed read would keep no field at all.
        mislabeled_mp3(
            &path,
            &[
                ("TYE", "1999"),
                ("TT2", "Song"),
                ("TP1", "Artist"),
                ("TAL", "Album"),
                ("TRK", "3"),
            ],
        );
        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("Song"));
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.track_number, Some(3));
        assert_eq!(track.year, Some(1999));
        assert!(dropped.is_none(), "{dropped:?}");
    }

    #[test]
    fn only_v22_frames_that_fill_a_v23_tag_count_as_mislabeled() {
        use crate::test_support::{id3v22_frames, id3v23_frames, id3v23_tag};
        let frames = id3v22_frames(&[("TT2", "Song"), ("TP1", "Artist")]);
        let padded = |tail: &[u8]| id3v23_tag(&[&frames[..], tail].concat());

        assert!(mislabeled_id3v22(&padded(&[])));
        assert!(mislabeled_id3v22(&padded(&[0; 16])));
        // Something other than padding after the frames.
        assert!(!mislabeled_id3v22(&padded(&[0, 1])));
        // A frame that runs past the end of the tag.
        assert!(!mislabeled_id3v22(&id3v23_tag(&frames[..frames.len() - 1])));
        // No frames at all.
        assert!(!mislabeled_id3v22(&id3v23_tag(&[0; 16])));
        // Real v2.3 frames, even one Lofty refuses.
        let v23 = id3v23_frames(&[("TIT2", "Song", 0), ("TENC", "Encoder", 0x0040)]);
        assert!(!mislabeled_id3v22(&id3v23_tag(&v23)));
        // A header that already says v2.2, or one with flags, which would
        // move or transform the frames.
        let mut tag = padded(&[]);
        tag[3] = 2;
        assert!(!mislabeled_id3v22(&tag));
        let mut tag = padded(&[]);
        tag[5] = 0x40;
        assert!(!mislabeled_id3v22(&tag));
    }

    #[test]
    fn a_malformed_field_under_a_mislabeled_header_costs_only_that_field() {
        let dir = crate::test_support::ScratchDir::new("mislabeled-v22-malformed");
        let path = dir.path().join("song.mp3");
        mislabeled_mp3(
            &path,
            &[("TT2", "Song"), ("TP1", "Artist"), ("TYE", "199x")],
        );
        let (track, dropped) = read_tags_noting(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("Song"));
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.year, None);
        let dropped = dropped.unwrap();
        assert_eq!(dropped.what, "malformed tag fields");
        let error = format!("{:#}", dropped.error);
        assert!(error.contains("unexpected character 'x'"), "{error}");
    }

    #[test]
    fn fields_the_primary_tag_lacks_come_from_id3v1() {
        use crate::test_support::{append_id3v1, write_id3v23_mp3};
        let dir = crate::test_support::ScratchDir::new("id3v1-fill");
        let path = dir.path().join("song.mp3");
        // As found in the wild: the ID3v2 tag holds only the compilation's
        // artist and album, the ID3v1 tag the track's own fields.
        write_id3v23_mp3(
            &path,
            &[
                ("TPE1", "Various Artists", 0),
                ("TALB", "The Punk Rock Movie", 0),
            ],
        );
        append_id3v1(
            &path,
            [
                "God Save The Queen",
                "Sex Pistols",
                "Punk Rock Movie",
                "1978",
            ],
            1,
        );

        let track = read_tags(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("God Save The Queen"));
        assert_eq!(track.year, Some(1978));
        assert_eq!(track.track_number, Some(1));
        // A value the primary tag holds is never replaced.
        assert_eq!(track.artist.as_deref(), Some("Various Artists"));
        assert_eq!(track.album.as_deref(), Some("The Punk Rock Movie"));
    }

    #[test]
    fn a_blank_primary_field_comes_from_id3v1_without_its_padding() {
        use crate::test_support::{append_id3v1, write_id3v23_mp3};
        let dir = crate::test_support::ScratchDir::new("id3v1-blank");
        let path = dir.path().join("song.mp3");
        write_id3v23_mp3(&path, &[("TIT2", " ", 0), ("TPE1", "Artist", 0)]);
        // Some taggers pad ID3v1 fields with spaces rather than NUL bytes.
        append_id3v1(&path, ["Song      ", "", "Album   ", ""], 0);

        let track = read_tags(&path).unwrap();
        assert_eq!(track.title.as_deref(), Some("Song"));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.year, None);
        assert_eq!(track.track_number, None);
    }

    #[test]
    fn accepts_uppercase_audio_extensions() {
        assert!(is_audio_file(Path::new("song.FLAC")));
        assert!(is_audio_file(Path::new("song.Mp3")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
        assert!(!is_audio_file(Path::new("no-extension")));
    }

    #[test]
    fn keeps_recording_and_release_track_identifiers_separate() {
        let mut tag = lofty::tag::Tag::new(lofty::tag::TagType::VorbisComments);
        let recording = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
        let release_track = "4642ee19-7790-3c8d-ab5e-d133de942db6";
        tag.insert_text(ItemKey::MusicBrainzRecordingId, recording.to_owned());
        tag.insert_text(ItemKey::MusicBrainzTrackId, release_track.to_owned());
        let ids = extract_musicbrainz_ids(&tag);
        assert_eq!(ids.recording, vec![recording.to_owned()]);
        assert_eq!(ids.release_track, vec![release_track.to_owned()]);
    }

    #[test]
    fn canonicalizes_valid_ids_and_keeps_invalid_ones_for_review() {
        let mut tag = lofty::tag::Tag::new(lofty::tag::TagType::VorbisComments);
        tag.insert_text(
            ItemKey::MusicBrainzReleaseId,
            "76DF3287-6CDA-33EB-8E9A-044B5E15FFDD".to_owned(),
        );
        tag.insert_text(ItemKey::MusicBrainzReleaseGroupId, "not-an-mbid".to_owned());

        let ids = extract_musicbrainz_ids(&tag);
        assert_eq!(
            ids.release,
            vec!["76df3287-6cda-33eb-8e9a-044b5e15ffdd".to_string()]
        );
        // Malformed values are retained verbatim, deterministically.
        assert_eq!(ids.release_group, vec!["not-an-mbid".to_string()]);
    }
}
