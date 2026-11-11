use crate::providers::Mbid;
use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus", "wav", "aac", "wma"];

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
    let tagged_file = Probe::open(path)?.read()?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let (title, artist, album, year, track_number) = match tag {
        Some(tag) => (
            tag.title().map(|s| s.to_string()),
            tag.artist().map(|s| s.to_string()),
            tag.album().map(|s| s.to_string()),
            tag.date().map(|ts| ts.year as i64),
            tag.track().map(|t| t as i64),
        ),
        None => (None, None, None, None, None),
    };

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

    Ok(ScannedTrack {
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
    })
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
        } => eprintln!("{message}"),
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
                    continue;
                }
                // walkdir's own text repeats the path for I/O errors, so show only the cause.
                let message = match io_error {
                    Some(io_error) => format!("warning: skipped {}: {io_error}", path.display()),
                    None => format!("warning: skipped {}: {err}", path.display()),
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
        |path| read_tags(path).with_context(|| path.display().to_string()),
        |path, outcome| {
            let warning = match outcome {
                Ok(Ok(track)) => {
                    result.tracks.push(track);
                    None
                }
                Ok(Err(err)) => {
                    result.unreadable.push(path.to_path_buf());
                    Some(format!("warning: failed to read tags: {err:#}"))
                }
                Err(err) => {
                    result.unreadable.push(path.to_path_buf());
                    Some(format!(
                        "warning: scan task panicked: {}: {err}",
                        path.display()
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
