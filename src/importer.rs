//! The one import pipeline, shared by `radish import` and the watcher's
//! initial sync.

use crate::progress::ImportProgress;
use crate::scanner::ScanResult;
use crate::{scanner, store};
use anyhow::{Result, bail};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What a [`sync_directory`] run did.
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    /// Files whose tags were read.
    pub scanned: usize,
    /// Files skipped because their modification time matched the database.
    pub unchanged: usize,
    /// Track rows inserted or changed.
    pub saved: u64,
    /// Track rows deleted because their files are gone.
    pub removed: u64,
    pub rebuild: store::RebuildSummary,
}

/// Brings the database in line with the audio files under `root`.
///
/// Files whose modification time matches the stored one are not read again,
/// and rows whose files no longer exist are deleted. Local releases are
/// rebuilt for `root` afterwards.
///
/// # Errors
///
/// Returns an error when `root` is not a readable directory, or when the walk
/// or a database write fails.
pub async fn sync_directory(
    pool: &SqlitePool,
    root: &Path,
    progress: &ImportProgress,
) -> Result<ImportSummary> {
    // A missing root would otherwise look like every file was deleted.
    if !tokio::fs::metadata(root).await?.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let directory_id = store::upsert_directory(pool, root).await?;
    let known = store::known_modified_times(pool, root).await?;
    let ScanResult { tracks, unchanged } = scanner::scan_directory_incremental(
        root,
        |path, modified_at| known.get(&store::path_key(path)) == Some(&modified_at),
        |event| progress.scan_event(event),
    )
    .await?;

    progress.saving(tracks.len());
    let mut saved = 0;
    for chunk in tracks.chunks(store::UPSERT_BATCH_SIZE) {
        saved += store::upsert_tracks(pool, chunk, directory_id).await?;
        for _ in chunk {
            progress.saved();
        }
    }

    // Anything stored but not met by the walk is gone from disk.
    let seen: HashSet<String> = tracks
        .iter()
        .map(|track| track.path.as_path())
        .chain(unchanged.iter().map(PathBuf::as_path))
        .map(store::path_key)
        .collect();
    let mut removed = 0;
    for key in known.keys().filter(|key| !seen.contains(*key)) {
        // Keys are lowercased, which removal matches case-insensitively.
        removed += store::remove_tracks_under(pool, Path::new(key)).await?;
    }

    progress.grouping();
    let rebuild = store::rebuild_local_releases(pool, Some(root)).await?;
    Ok(ImportSummary {
        scanned: tracks.len(),
        unchanged: unchanged.len(),
        saved,
        removed,
        rebuild,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// A scratch directory removed when dropped, including when a test panics.
    pub(crate) struct ScratchDir(PathBuf);

    impl ScratchDir {
        pub(crate) fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "radish-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // Canonical, so paths compare equal to what the importer stores.
            Self(dir.canonicalize().unwrap())
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Writes a minimal valid PCM WAV file.
    pub(crate) fn write_wav(path: &Path) {
        let data = [0u8; 32];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&44100u32.to_le_bytes());
        bytes.extend_from_slice(&88200u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{ScratchDir, write_wav};
    use super::*;
    use crate::db;

    async fn revisions(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT source_revision FROM local_releases ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn second_import_of_unchanged_files_reads_no_tags() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-unchanged");
        write_wav(&music.path().join("a.wav"));
        write_wav(&music.path().join("b.wav"));
        let progress = ImportProgress::new();

        let first = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();
        assert_eq!((first.scanned, first.unchanged), (2, 0));
        let before = revisions(&pool).await;
        assert!(!before.is_empty());

        let second = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();
        assert_eq!(second.unchanged, 2, "{second:?}");
        assert_eq!(second.scanned, 0, "{second:?}");
        assert_eq!(second.saved, 0, "{second:?}");
        assert!(!second.rebuild.changed(), "{second:?}");
        assert_eq!(revisions(&pool).await, before);
    }

    #[tokio::test]
    async fn import_removes_files_deleted_since_the_last_import() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-removed");
        write_wav(&music.path().join("Keep.wav"));
        write_wav(&music.path().join("Gone.wav"));
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();
        std::fs::remove_file(music.path().join("Gone.wav")).unwrap();

        let summary = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        assert_eq!(summary.removed, 1, "{summary:?}");
        let tracks = store::list_tracks(&pool).await.unwrap();
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].path.ends_with("Keep.wav"), "{:?}", tracks[0].path);
    }
}
