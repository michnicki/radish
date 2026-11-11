//! The one import pipeline, shared by `radish import` and the watcher's
//! initial sync.

use crate::progress::ImportProgress;
use crate::scanner::{ScanEvent, ScanResult};
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
    /// Stored rows kept because their files, or a folder above them, could not
    /// be read this time.
    pub kept: usize,
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
    let directory_id = store::upsert_directory(pool, root).await?;
    let mut summary = sync_files(pool, root, directory_id, Some(progress)).await?;
    progress.grouping();
    summary.rebuild = store::rebuild_local_releases(pool, Some(root)).await?;
    Ok(summary)
}

/// [`sync_directory`] without the rebuild: syncs the files under `dir` into
/// the rows of the library directory `directory_id`. The watcher uses it for
/// a folder that appeared inside the watched root, and rebuilds once per batch.
/// Without a `progress` display, warnings go to stderr.
pub(crate) async fn sync_files(
    pool: &SqlitePool,
    dir: &Path,
    directory_id: i64,
    progress: Option<&ImportProgress>,
) -> Result<ImportSummary> {
    // A missing directory would otherwise look like every file was deleted.
    if !tokio::fs::metadata(dir).await?.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let known = store::known_tracks(pool, dir).await?;
    let ScanResult {
        tracks,
        unchanged,
        unreadable,
    } = scanner::scan_directory_incremental(
        dir,
        |path, modified_at| {
            known
                .get(&store::path_key(path))
                .is_some_and(|known| known.modified_at == modified_at)
        },
        |event| match progress {
            Some(progress) => progress.scan_event(event),
            None => match event {
                ScanEvent::Warning { message }
                | ScanEvent::FileFinished {
                    warning: Some(message),
                    ..
                } => eprintln!("{message}"),
                ScanEvent::Discovering
                | ScanEvent::Discovered { .. }
                | ScanEvent::FileFinished { .. } => {}
            },
        },
    )
    .await?;

    // An empty answer for a library that has rows is more likely an unmounted
    // volume or a wiped share than a deliberate delete-everything.
    if tracks.is_empty() && unchanged.is_empty() && unreadable.is_empty() && !known.is_empty() {
        let message = format!(
            "warning: no audio files found under {}; keeping {} stored tracks (is the volume mounted?)",
            dir.display(),
            known.len()
        );
        match progress {
            Some(progress) => progress.scan_event(ScanEvent::Warning { message }),
            None => eprintln!("{message}"),
        }
        return Ok(ImportSummary {
            kept: known.len(),
            ..ImportSummary::default()
        });
    }

    if let Some(progress) = progress {
        progress.saving(tracks.len());
    }
    let mut saved = 0;
    for chunk in tracks.chunks(store::UPSERT_BATCH_SIZE) {
        saved += store::upsert_tracks(pool, chunk, directory_id).await?;
        for _ in chunk {
            if let Some(progress) = progress {
                progress.saved();
            }
        }
    }

    // Anything stored but not met by the walk is gone from disk.
    let seen: HashSet<String> = tracks
        .iter()
        .map(|track| track.path.as_path())
        .chain(unchanged.iter().map(PathBuf::as_path))
        .map(store::path_key)
        .collect();
    let unreadable_keys: Vec<String> = unreadable
        .iter()
        .map(|path| store::path_key(path).trim_end_matches('/').to_string())
        .collect();
    // A file or folder the scan could not read says nothing about its rows.
    let is_unreadable = |key: &str| {
        unreadable_keys.iter().any(|blocked| {
            key == blocked
                || key
                    .strip_prefix(blocked.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut gone = Vec::new();
    let mut kept = 0;
    for (key, known) in &known {
        if seen.contains(key) {
            continue;
        }
        if is_unreadable(key) {
            kept += 1;
        } else {
            gone.push(known.path.clone());
        }
    }
    let removed = store::remove_tracks_by_paths(pool, &gone).await?;

    Ok(ImportSummary {
        scanned: tracks.len(),
        unchanged: unchanged.len(),
        saved,
        removed,
        kept,
        rebuild: store::RebuildSummary::default(),
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

    /// Gives a directory its permissions back so the scratch directory can go.
    #[cfg(unix)]
    struct RestorePermissions(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    async fn paths(pool: &SqlitePool) -> Vec<String> {
        let mut paths: Vec<String> = store::list_tracks(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|track| track.path)
            .collect();
        paths.sort();
        paths
    }

    #[tokio::test]
    async fn a_file_that_stops_reading_keeps_its_row() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-corrupt");
        write_wav(&music.path().join("a.wav"));
        write_wav(&music.path().join("b.wav"));
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        // Corrupt content with a newer modification time, so it is read again.
        let b = music.path().join("b.wav");
        std::fs::write(&b, b"not audio").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let summary = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(summary.kept, 1, "{summary:?}");
        assert_eq!(paths(&pool).await.len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_folder_keeps_its_rows() {
        use std::os::unix::fs::PermissionsExt;
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-chmod");
        let album = music.path().join("Album");
        std::fs::create_dir(&album).unwrap();
        write_wav(&album.join("01.wav"));
        write_wav(&album.join("02.wav"));
        write_wav(&music.path().join("top.wav"));
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        std::fs::set_permissions(&album, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = RestorePermissions(album.clone());
        if std::fs::read_dir(&album).is_ok() {
            return; // running as root: permissions do not apply
        }
        let summary = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(summary.kept, 2, "{summary:?}");
        assert_eq!(paths(&pool).await.len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dangling_linked_folder_keeps_its_rows() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-dangling");
        let holder = ScratchDir::new("importer-dangling-share");
        let share = holder.path().join("share");
        std::fs::create_dir(&share).unwrap();
        write_wav(&share.join("01.wav"));
        write_wav(&share.join("02.wav"));
        write_wav(&music.path().join("top.wav"));
        std::os::unix::fs::symlink(&share, music.path().join("NAS")).unwrap();
        let progress = ImportProgress::new();
        let first = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();
        assert_eq!(first.scanned, 3);

        std::fs::rename(&share, holder.path().join("gone")).unwrap();
        let summary = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        // The share is gone, but nothing says its tracks are: keep them.
        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(summary.kept, 2, "{summary:?}");
    }

    #[tokio::test]
    async fn an_emptied_root_keeps_its_rows() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-emptied");
        for name in ["a.wav", "b.wav", "c.wav"] {
            write_wav(&music.path().join(name));
        }
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        for name in ["a.wav", "b.wav", "c.wav"] {
            std::fs::remove_file(music.path().join(name)).unwrap();
        }
        let summary = sync_directory(&pool, music.path(), &progress)
            .await
            .unwrap();

        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(summary.kept, 3, "{summary:?}");
        assert_eq!(paths(&pool).await.len(), 3);
    }
}
