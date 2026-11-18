//! The one import pipeline, shared by `radish import` and the watcher's
//! initial sync.

use crate::layout::plural_grouped;
use crate::output::clean_path;
use crate::progress::ImportProgress;
use crate::scanner::{ScanEvent, ScanResult};
use crate::{outln, scanner, store};
use anyhow::{Result, bail};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What a [`sync_directory`] run did.
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    /// The library directory row of the root, which every saved track points to.
    pub directory_id: i64,
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
    /// Stored rows kept because the walk found no audio files at all, which
    /// looks like an unmounted volume rather than a deleted library.
    pub kept_no_audio: usize,
    pub rebuild: store::RebuildSummary,
    /// Imported roots inside this one that it took over, in path order.
    pub absorbed_roots: Vec<String>,
}

/// The warnings a rebuild deserves: accepted matches it dropped or staled.
pub fn rebuild_warnings(rebuild: &store::RebuildSummary) -> Vec<String> {
    let mut warnings = Vec::new();
    if rebuild.dropped_matches > 0 {
        warnings.push(format!(
            "warning: {} accepted match(es) were dropped because their files were split or removed",
            rebuild.dropped_matches
        ));
    }
    if rebuild.newly_stale > 0 {
        warnings.push(format!(
            "warning: {} accepted match(es) are now stale because their files changed",
            rebuild.newly_stale
        ));
    }
    warnings
}

/// Prints [`rebuild_warnings`] to stdout, one line each.
pub fn report_rebuild(rebuild: &store::RebuildSummary) {
    for warning in rebuild_warnings(rebuild) {
        outln!("{warning}");
    }
}

/// Brings the database in line with the audio files under `root`.
///
/// Files whose modification time matches the stored one are not read again
/// unless `force` is set, and rows whose files no longer exist are deleted.
/// Local releases are rebuilt for `root` afterwards.
///
/// # Errors
///
/// Returns an error when `root` is not a readable directory, or when the walk
/// or a database write fails.
pub async fn sync_directory(
    pool: &SqlitePool,
    root: &Path,
    progress: &ImportProgress,
    force: bool,
) -> Result<ImportSummary> {
    // Claim the root only once it is known to be a directory: claiming can
    // take over other roots, and a mistyped path should leave no row behind.
    if !tokio::fs::metadata(root).await?.is_dir() {
        bail!("{} is not a directory", clean_path(root));
    }
    let claim = store::claim_root(pool, root).await?;
    let mut summary = sync_files(pool, root, claim.id, Some(progress), force).await?;
    summary.absorbed_roots = claim.absorbed;
    progress.grouping();
    summary.rebuild = store::rebuild_local_releases(pool, Some(root)).await?;
    Ok(summary)
}

/// Prints which imported roots an import or watch took over, if any.
pub fn report_roots(summary: &ImportSummary, root: &Path) {
    let count = summary.absorbed_roots.len();
    if count > 0 {
        outln!(
            "Merged {} into {}.",
            plural_grouped(count as u64, "imported directory", "imported directories"),
            clean_path(root)
        );
    }
}

/// Sends a scan event to the progress display, or prints its warning when
/// there is none.
fn emit(progress: Option<&ImportProgress>, event: ScanEvent) {
    match progress {
        Some(progress) => progress.scan_event(event),
        None => scanner::eprint_warning(event),
    }
}

/// The warning for a file skipped because another file differs from it only in
/// letter case. Shared by the import and the watcher.
pub(crate) fn case_collision_warning(path: &Path) -> String {
    format!(
        "warning: skipping {}: another file differs only in letter case",
        clean_path(path)
    )
}

/// Drops all but one of the scanned files that share a [`store::path_key`] and
/// returns the dropped paths in sorted order.
///
/// The file already stored wins, so an import never swaps one for the other;
/// when neither is stored, the first in sorted order does.
fn skip_case_collisions(
    tracks: &mut Vec<scanner::ScannedTrack>,
    unchanged: &mut Vec<PathBuf>,
    known: &HashMap<String, store::KnownTrack>,
) -> Vec<PathBuf> {
    let mut groups: HashMap<String, Vec<&Path>> = HashMap::new();
    for path in tracks
        .iter()
        .map(|track| track.path.as_path())
        .chain(unchanged.iter().map(PathBuf::as_path))
    {
        groups.entry(store::path_key(path)).or_default().push(path);
    }
    let mut skipped = HashSet::new();
    for (key, mut paths) in groups {
        if paths.len() < 2 {
            continue;
        }
        paths.sort();
        let keep = known
            .get(&key)
            .and_then(|stored| {
                paths
                    .iter()
                    .position(|path| Path::new(&stored.path) == *path)
            })
            .unwrap_or(0);
        paths.remove(keep);
        skipped.extend(paths.into_iter().map(Path::to_path_buf));
    }
    tracks.retain(|track| !skipped.contains(&track.path));
    unchanged.retain(|path| !skipped.contains(path));
    let mut skipped: Vec<PathBuf> = skipped.into_iter().collect();
    skipped.sort();
    skipped
}

/// [`sync_directory`] without the rebuild: syncs the files under `dir` into
/// the rows of the library directory `directory_id`. The watcher uses it for
/// a folder that appeared inside the watched root, and rebuilds once per batch.
/// Without a `progress` display, warnings go to stderr. With `force`, every
/// file is read again whatever its modification time says.
pub(crate) async fn sync_files(
    pool: &SqlitePool,
    dir: &Path,
    directory_id: i64,
    progress: Option<&ImportProgress>,
    force: bool,
) -> Result<ImportSummary> {
    // A missing directory would otherwise look like every file was deleted.
    if !tokio::fs::metadata(dir).await?.is_dir() {
        bail!("{} is not a directory", clean_path(dir));
    }
    let known = store::known_tracks(pool, dir).await?;
    let ScanResult {
        mut tracks,
        mut unchanged,
        unreadable,
        quietly_skipped,
    } = scanner::scan_directory_incremental(
        dir,
        |path, modified_at| {
            // The stored path must match exactly, not just case-insensitively:
            // a case-only rename has to be read and saved again.
            !force
                && known.get(&store::path_key(path)).is_some_and(|known| {
                    known.modified_at == modified_at && Path::new(&known.path) == path
                })
        },
        |event| emit(progress, event),
    )
    .await?;

    // `tracks.path` ignores letter case, so files that differ only in case would
    // overwrite each other's row on every import.
    for path in skip_case_collisions(&mut tracks, &mut unchanged, &known) {
        let message = case_collision_warning(&path);
        emit(progress, ScanEvent::Warning { message });
    }

    // An empty answer for a library that has rows is more likely an unmounted
    // volume or a wiped share than a deliberate delete-everything.
    if tracks.is_empty() && unchanged.is_empty() && unreadable.is_empty() && !known.is_empty() {
        let message = format!(
            "warning: no audio files found under {}; keeping {} stored tracks (is the volume mounted?)",
            clean_path(dir),
            known.len()
        );
        emit(progress, ScanEvent::Warning { message });
        return Ok(ImportSummary {
            directory_id,
            kept_no_audio: known.len(),
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
        unreadable_keys
            .iter()
            .any(|blocked| store::key_within(key, blocked))
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

    // The scan stays quiet about a dangling link, so say what it is protecting.
    for path in &quietly_skipped {
        let blocked = store::path_key(path).trim_end_matches('/').to_string();
        let protected = known
            .keys()
            .filter(|key| !seen.contains(*key))
            .filter(|key| store::key_within(key, &blocked))
            .count();
        if protected > 0 {
            let message = format!(
                "warning: kept {protected} stored tracks under {}: the folder is unreachable (broken link?)",
                clean_path(path)
            );
            emit(progress, ScanEvent::Warning { message });
        }
    }

    Ok(ImportSummary {
        directory_id,
        scanned: tracks.len(),
        unchanged: unchanged.len(),
        saved,
        removed,
        kept,
        kept_no_audio: 0,
        rebuild: store::RebuildSummary::default(),
        absorbed_roots: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_support::{ScratchDir, write_tagged_wav, write_wav};

    async fn revisions(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT source_revision FROM local_releases ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_summary_names_the_library_directory_of_the_root() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-directory-id");
        let progress = ImportProgress::new();

        let summary = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        let stored = store::upsert_directory(&pool, music.path()).await.unwrap();
        assert_eq!(summary.directory_id, stored);
    }

    #[tokio::test]
    async fn importing_a_parent_takes_over_its_imported_subfolders() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-parent-root");
        let progress = ImportProgress::new();
        for year in ["1998", "1999"] {
            let folder = music.path().join(year);
            std::fs::create_dir_all(&folder).unwrap();
            write_wav(&folder.join("01.wav"));
            sync_directory(&pool, &folder, &progress, false)
                .await
                .unwrap();
        }

        let summary = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        let year = |name: &str| music.path().join(name).to_string_lossy().to_string();
        assert_eq!(summary.absorbed_roots, [year("1998"), year("1999")]);
        assert_eq!(summary.unchanged, 2);
        assert_eq!(summary.scanned, 0);
        let roots: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM directories")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            roots,
            [(
                summary.directory_id,
                music.path().to_string_lossy().to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_missing_directory_adds_no_root() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-missing-root");
        let progress = ImportProgress::new();

        let missing = music.path().join("gone");
        assert!(
            sync_directory(&pool, &missing, &progress, false)
                .await
                .is_err()
        );

        let roots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directories")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(roots, 0);
    }

    #[test]
    fn rebuild_warnings_name_dropped_and_stale_matches() {
        assert!(rebuild_warnings(&store::RebuildSummary::default()).is_empty());
        let rebuild = store::RebuildSummary {
            dropped_matches: 2,
            newly_stale: 3,
            ..store::RebuildSummary::default()
        };

        let warnings = rebuild_warnings(&rebuild);

        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("2 accepted match(es) were dropped"));
        assert!(warnings[1].contains("3 accepted match(es) are now stale"));
    }

    #[tokio::test]
    async fn second_import_of_unchanged_files_reads_no_tags() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-unchanged");
        write_wav(&music.path().join("a.wav"));
        write_wav(&music.path().join("b.wav"));
        let progress = ImportProgress::new();

        let first = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();
        assert_eq!((first.scanned, first.unchanged), (2, 0));
        let before = revisions(&pool).await;
        assert!(!before.is_empty());

        let second = sync_directory(&pool, music.path(), &progress, false)
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
        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();
        std::fs::remove_file(music.path().join("Gone.wav")).unwrap();

        let summary = sync_directory(&pool, music.path(), &progress, false)
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
        sync_directory(&pool, music.path(), &progress, false)
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
        let summary = sync_directory(&pool, music.path(), &progress, false)
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
        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        std::fs::set_permissions(&album, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = RestorePermissions(album.clone());
        if std::fs::read_dir(&album).is_ok() {
            return; // running as root: permissions do not apply
        }
        let summary = sync_directory(&pool, music.path(), &progress, false)
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
        let first = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();
        assert_eq!(first.scanned, 3);

        std::fs::rename(&share, holder.path().join("gone")).unwrap();
        let summary = sync_directory(&pool, music.path(), &progress, false)
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
        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        for name in ["a.wav", "b.wav", "c.wav"] {
            std::fs::remove_file(music.path().join(name)).unwrap();
        }
        let summary = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(summary.kept_no_audio, 3, "{summary:?}");
        assert_eq!(summary.kept, 0, "{summary:?}");
        assert_eq!(paths(&pool).await.len(), 3);
    }

    #[tokio::test]
    async fn a_case_only_rename_is_reflected_after_import() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-case-rename");
        write_wav(&music.path().join("01.wav"));
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        std::fs::rename(music.path().join("01.wav"), music.path().join("01.WAV")).unwrap();
        let summary = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        assert_eq!(summary.removed, 0, "{summary:?}");
        assert_eq!(
            paths(&pool).await,
            [music.path().join("01.WAV").display().to_string()]
        );
    }

    /// A database as the pre-migration binary left it: only the original schema.
    async fn legacy_pool() -> SqlitePool {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn mtime_secs(path: &Path) -> i64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[tokio::test]
    async fn an_upgraded_database_rereads_files_so_polluted_years_are_replaced() {
        let music = ScratchDir::new("importer-upgrade-year");
        let file = music.path().join("01.wav");
        write_tagged_wav(&file, "Blue", 2005);
        // The old binary stored the year on the shared album row, so this track
        // carries whichever year was written last, with the file's current mtime.
        let pool = legacy_pool().await;
        sqlx::query("INSERT INTO directories (id, path) VALUES (1, ?1)")
            .bind(music.path().display().to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO albums (id, title, artist, year) VALUES (1, 'Blue', NULL, 1971)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tracks (id, path, album_id, directory_id, modified_at)
             VALUES (10, ?1, 1, 1, ?2)",
        )
        .bind(file.display().to_string())
        .bind(mtime_secs(&file))
        .execute(&pool)
        .await
        .unwrap();

        db::initialize(&pool).await.unwrap();
        let before: Option<i64> = sqlx::query_scalar("SELECT year FROM tracks WHERE id = 10")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            before,
            Some(1971),
            "the migration carries the old year over"
        );
        let summary = sync_directory(&pool, music.path(), &ImportProgress::new(), false)
            .await
            .unwrap();

        assert_eq!(summary.scanned, 1, "{summary:?}");
        let after: Option<i64> = sqlx::query_scalar("SELECT year FROM tracks WHERE id = 10")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, Some(2005));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn case_colliding_files_keep_one_stable_row_across_imports() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-case-collision");
        write_wav(&music.path().join("x.wav"));
        if music.path().join("X.wav").exists() {
            return; // case-insensitive filesystem: the two names cannot coexist
        }
        write_wav(&music.path().join("X.wav"));
        let progress = ImportProgress::new();

        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();
        let stored = paths(&pool).await;
        let revision = revisions(&pool).await;
        assert_eq!(stored.len(), 1, "{stored:?}");
        for _ in 0..2 {
            let summary = sync_directory(&pool, music.path(), &progress, false)
                .await
                .unwrap();
            assert_eq!(summary.saved, 0, "{summary:?}");
            assert_eq!(paths(&pool).await, stored);
            assert_eq!(revisions(&pool).await, revision);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_case_colliding_file_does_not_displace_the_stored_one() {
        let pool = db::connect_in_memory().await.unwrap();
        let music = ScratchDir::new("importer-case-stored");
        write_wav(&music.path().join("x.wav"));
        if music.path().join("X.wav").exists() {
            return; // case-insensitive filesystem
        }
        let progress = ImportProgress::new();
        sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();
        let stored = paths(&pool).await;

        // "X.wav" sorts before "x.wav", but the stored file still wins.
        write_wav(&music.path().join("X.wav"));
        let summary = sync_directory(&pool, music.path(), &progress, false)
            .await
            .unwrap();

        assert_eq!(summary.saved, 0, "{summary:?}");
        assert_eq!(paths(&pool).await, stored);
    }
}
