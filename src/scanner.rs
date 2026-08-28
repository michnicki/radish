use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus", "wav", "aac", "wma"];

#[derive(Debug, Clone)]
pub struct ScannedTrack {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub track_number: Option<i64>,
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

pub fn read_tags(path: &Path) -> Result<ScannedTrack> {
    let tagged_file = Probe::open(path)?.read()?;
    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());

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
        year,
        track_number,
        duration_secs,
        modified_at,
    })
}

/// Walks `root` and reads tags for every audio file found, spreading the
/// (CPU-bound) tag parsing across the tokio blocking thread pool so large
/// libraries scan in seconds instead of hours.
pub async fn scan_directory(root: &Path) -> Result<Vec<ScannedTrack>> {
    let files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_audio_file(entry.path()))
        .map(|entry| entry.path().to_path_buf())
        .collect();

    let mut set = tokio::task::JoinSet::new();
    for path in files {
        set.spawn_blocking(move || read_tags(&path));
    }

    let mut tracks = Vec::new();
    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok(track)) => tracks.push(track),
            Ok(Err(err)) => eprintln!("warning: failed to read tags: {err}"),
            Err(err) => eprintln!("warning: scan task panicked: {err}"),
        }
    }

    Ok(tracks)
}
