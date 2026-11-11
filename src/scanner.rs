use crate::providers::Mbid;
use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    let modified_at = std::fs::metadata(path)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs() as i64;

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

/// Reads audio tags in parallel, printing warnings for failed reads.
pub async fn scan_directory(root: &Path) -> Result<Vec<ScannedTrack>> {
    scan_directory_with_progress(root, |event| match event {
        ScanEvent::Warning { message }
        | ScanEvent::FileFinished {
            warning: Some(message),
            ..
        } => eprintln!("{message}"),
        ScanEvent::Discovering | ScanEvent::Discovered { .. } | ScanEvent::FileFinished { .. } => {}
    })
    .await
}

/// Walks `root` and reads audio tags on the Tokio blocking thread pool.
///
/// The callback receives discovery and read-completion events, and owns warning
/// output. No warnings are printed separately by this entrypoint.
pub async fn scan_directory_with_progress(
    root: &Path,
    mut on_progress: impl FnMut(ScanEvent),
) -> Result<Vec<ScannedTrack>> {
    on_progress(ScanEvent::Discovering);
    let mut files: Vec<PathBuf> = Vec::new();
    // Following links lets symlinked libraries import; walkdir reports loops as errors.
    for entry in WalkDir::new(root).follow_links(true) {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                files.push(entry.into_path());
            }
            Ok(_) => {}
            Err(err) => {
                let path = err.path().unwrap_or(root);
                on_progress(ScanEvent::Warning {
                    message: format!("warning: skipped {}: {err}", path.display()),
                });
            }
        }
    }
    let total = files.len();
    on_progress(ScanEvent::Discovered { total });

    let mut set = tokio::task::JoinSet::new();
    let mut paths_by_task = HashMap::new();
    for path in files {
        let task_path = path.clone();
        let handle = set
            .spawn_blocking(move || read_tags(&path).with_context(|| path.display().to_string()));
        paths_by_task.insert(handle.id(), task_path);
    }

    let mut tracks = Vec::new();
    let mut completed = 0;
    while let Some(result) = set.join_next_with_id().await {
        let warning = match result {
            Ok((_, Ok(track))) => {
                tracks.push(track);
                None
            }
            Ok((_, Err(err))) => Some(format!("warning: failed to read tags: {err:#}")),
            Err(err) => {
                let path = paths_by_task
                    .get(&err.id())
                    .map_or_else(String::new, |path| format!("{}: ", path.display()));
                Some(format!("warning: scan task panicked: {path}{err}"))
            }
        };
        completed += 1;
        on_progress(ScanEvent::FileFinished {
            completed,
            total,
            warning,
        });
    }

    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

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
