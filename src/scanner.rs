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

/// Reads a file's tags with Lofty's default parsing, or leniently when that
/// fails.
///
/// One malformed frame, such as a year written `199x`, makes the default mode
/// give up on the whole tag and so on every field in it. The relaxed mode
/// drops only the fields it cannot parse. A tag whose header says ID3v2.3 over
/// ID3v2.2 frames is read as the ID3v2.2 tag it is, see [`mislabeled_id3v22`].
/// Both run only after the default read failed, so every file the default
/// mode can read keeps exactly its values.
fn read_file(path: &Path) -> Result<(TaggedFile, Option<anyhow::Error>)> {
    let strict = match Probe::open(path)?.read() {
        Ok(file) => return Ok((file, None)),
        Err(error) => anyhow::Error::from(error),
    };
    let Some(bytes) = relabeled_id3v22(path) else {
        return read_relaxed(Probe::open(path)?, strict);
    };
    let probe = || {
        // The type the extension names, as `Probe::open` does.
        let probe = Probe::new(Cursor::new(bytes.as_slice()));
        match FileType::from_path(path) {
            Some(file_type) => probe.set_file_type(file_type),
            None => probe,
        }
    };
    match probe().read() {
        Ok(file) => Ok((file, None)),
        Err(error) => read_relaxed(probe(), error.into()),
    }
}

/// Reads `probe` in Lofty's relaxed mode, after the default mode failed with
/// `strict`.
fn read_relaxed<R: Read + Seek>(
    probe: Probe<R>,
    strict: anyhow::Error,
) -> Result<(TaggedFile, Option<anyhow::Error>)> {
    let relaxed = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    match probe.options(relaxed).read() {
        Ok(file) => Ok((file, Some(strict))),
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

/// [`read_tags`], plus the error that made the read drop malformed fields,
/// when it had to.
fn read_tags_noting(path: &Path) -> Result<(ScannedTrack, Option<anyhow::Error>)> {
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
    Discovered {
        total: usize,
    },
    /// A path the walk could not visit; it is not counted in the totals.
    Warning {
        message: String,
    },
    /// Every attempted read counts, including failures and panicked tasks.
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
        ScanEvent::Discovering | ScanEvent::Discovered { .. } | ScanEvent::FileFinished { .. } => {}
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
/// and still count as finished in the progress events. At most
/// [`read_limit`] reads run at once.
pub async fn scan_directory_incremental(
    root: &Path,
    is_unchanged: impl Fn(&Path, i64) -> bool,
    mut on_progress: impl FnMut(ScanEvent),
) -> Result<ScanResult> {
    on_progress(ScanEvent::Discovering);
    let mut result = ScanResult::default();
    let mut files: Vec<(PathBuf, Option<i64>)> = Vec::new();
    // Following links lets symlinked libraries import; walkdir reports loops as errors.
    for entry in WalkDir::new(root).follow_links(true) {
        match entry {
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
    let total = files.len();
    on_progress(ScanEvent::Discovered { total });

    let mut completed = 0;
    let mut to_read = Vec::new();
    for (path, mtime) in files {
        if mtime.is_some_and(|mtime| is_unchanged(&path, mtime)) {
            completed += 1;
            on_progress(ScanEvent::FileFinished {
                completed,
                total,
                warning: None,
            });
            result.unchanged.push(path);
        } else {
            to_read.push(path);
        }
    }

    run_bounded(
        to_read,
        read_limit(),
        |path| read_tags_noting(path).with_context(|| clean_path(path)),
        |path, outcome| {
            let warning = match outcome {
                Ok(Ok((track, dropped))) => {
                    result.tracks.push(track);
                    dropped.map(|error| {
                        format!(
                            "note: dropped malformed tag fields from {}: {error:#}",
                            clean_path(path)
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

    #[tokio::test]
    async fn an_import_notes_the_tag_fields_it_dropped() {
        let dir = crate::test_support::ScratchDir::new("malformed-year-scan");
        tagged_mp3(&dir.path().join("song.mp3"), "199x");

        let mut warnings = Vec::new();
        let result = scan_directory_incremental(
            dir.path(),
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
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("note: dropped malformed tag fields from ")
                && warnings[0].contains("song.mp3")
                && warnings[0].contains("unexpected character 'x'"),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_encrypted_frame_without_its_length_still_fails_the_read() {
        let dir = crate::test_support::ScratchDir::new("encrypted-frame");
        let path = dir.path().join("song.mp3");
        crate::test_support::write_id3v23_mp3(
            &path,
            &[("TIT2", "Song", 0), ("TENC", "Encoder", 0x0040)],
        );
        // Lofty refuses this in every parsing mode, so the file stays
        // unreadable and its stored row, if any, is kept.
        let error = read_tags(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("encrypted frame"),
            "{error:#}"
        );
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
        let dropped = format!("{:#}", dropped.unwrap());
        assert!(dropped.contains("unexpected character 'x'"), "{dropped}");
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
