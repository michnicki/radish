use crate::layout::{grouped, plural, plural_grouped};
use crate::output::{Clean, clean_path};
use crate::progress::ImportProgress;
use crate::{importer, outln, scanner, store};
use anyhow::Result;
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long the watcher keeps collecting events after the first one.
const DEBOUNCE: Duration = Duration::from_millis(500);

type WatchEvent = notify::Result<notify::Event>;

/// Watches `root` for filesystem changes and keeps the database in sync in
/// both directions: edits or new files get (re-)tagged into the DB, and
/// deletions remove the corresponding track rows.
///
/// `db_path` is the library database. It may live inside `root`, so its own
/// files never count as changes.
///
/// Observed metadata is refreshed per batch of events. Accepted Discogs
/// metadata lives in separate tables, so refreshes never overwrite an accepted
/// match.
pub async fn watch_directory(pool: SqlitePool, root: &Path, db_path: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    // The pool has connected, so the file exists and can be made absolute.
    let scope = Scope::new(&root, &db_path.canonicalize()?);

    // Start watching first so changes made during the initial sync are queued.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    outln!("Doing initial scan of {}...", clean_path(&root));
    let progress = ImportProgress::new();
    let summary = importer::sync_directory(&pool, &root, &progress, false).await?;
    drop(progress);
    importer::report_roots(&summary, &root);
    outln!(
        "Synced {} ({} unchanged, {} removed). Watching for changes... (Ctrl+C to stop)",
        plural_grouped(
            (summary.scanned + summary.unchanged) as u64,
            "track",
            "tracks"
        ),
        grouped(summary.unchanged as u64),
        grouped(summary.removed)
    );
    importer::report_rebuild(&summary.rebuild);
    let directory_id = summary.directory_id;

    let mut state = WatchState::default();
    while let Some(batch) = collect_batch(&mut rx, DEBOUNCE).await {
        if process_batch(&pool, &scope, directory_id, batch, &mut state).await == Flow::RootGone {
            outln!("{} was removed; no longer watching.", clean_path(&root));
            break;
        }
    }

    Ok(())
}

/// Where the watcher works: the watched root and the files it must ignore.
struct Scope {
    root: PathBuf,
    root_key: String,
    ignored: Vec<String>,
}

impl Scope {
    fn new(root: &Path, db_path: &Path) -> Self {
        let ignored = ["", "-wal", "-shm", "-journal"]
            .iter()
            .map(|suffix| {
                let mut name = db_path.as_os_str().to_os_string();
                name.push(suffix);
                store::path_key(Path::new(&name))
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            root_key: store::path_key(root).trim_end_matches('/').to_string(),
            ignored,
        }
    }

    /// True for the root and everything beneath it, compared like the path
    /// columns: forward slashes, ASCII case folded, whole components only.
    fn contains(&self, key: &str) -> bool {
        store::key_within(key, &self.root_key)
    }

    fn is_ignored(&self, key: &str) -> bool {
        self.ignored.iter().any(|ignored| ignored == key)
    }
}

#[derive(Default)]
struct Plan {
    /// Paths worth looking at, once each, grouped by their comparison key.
    /// Paths that differ only in letter case stay separate, in arrival order.
    paths: BTreeMap<String, Vec<PathBuf>>,
    /// The backend lost track of events, so only a full sync is safe.
    rescan: bool,
}

/// Reduces a batch of raw events to the paths that matter.
///
/// Reads, metadata-only changes, the database's own files and anything outside
/// the root are dropped, and repeated events for one path collapse into one.
/// Distinct spellings of one path (a case-only rename) are all kept.
fn plan_batch(scope: &Scope, events: &[notify::Event]) -> Plan {
    let mut plan = Plan::default();
    for event in events {
        if event.need_rescan() {
            plan.rescan = true;
        }
        if matches!(
            event.kind,
            EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
        ) {
            continue;
        }
        for path in &event.paths {
            let key = store::path_key(path);
            if scope.contains(&key) && !scope.is_ignored(&key) {
                let spellings = plan.paths.entry(key).or_default();
                if !spellings.contains(path) {
                    spellings.push(path.clone());
                }
            }
        }
    }
    plan
}

/// Waits for an event, then gathers everything that arrives within `window`.
/// Returns `None` once the channel is closed and drained.
async fn collect_batch(
    rx: &mut mpsc::UnboundedReceiver<WatchEvent>,
    window: Duration,
) -> Option<Vec<WatchEvent>> {
    let mut batch = vec![rx.recv().await?];
    let deadline = tokio::time::sleep(window);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            next = rx.recv() => match next {
                Some(event) => batch.push(event),
                None => break,
            },
        }
    }
    Some(batch)
}

/// What the watcher carries from one batch to the next.
#[derive(Debug, Default)]
struct WatchState {
    /// Something may have been missed, so the next batch starts with a full sync.
    rescan_pending: bool,
}

/// Whether the watch goes on after a batch.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    RootGone,
}

/// Handles one collected batch. Watch errors and failures are reported and
/// never end the watch; only the loss of the watched root does.
///
/// A watch error means events may have been lost, and a database failure means
/// some were not applied. Either way the next sync is a full one.
async fn process_batch(
    pool: &SqlitePool,
    scope: &Scope,
    directory_id: i64,
    batch: Vec<WatchEvent>,
    state: &mut WatchState,
) -> Flow {
    let mut events = Vec::new();
    let mut lost_events = false;
    for res in batch {
        match res {
            Ok(event) => events.push(event),
            Err(err) => {
                eprintln!("watch error: {}", Clean(&err.to_string()));
                lost_events = true;
            }
        }
    }
    let force_rescan = state.rescan_pending || lost_events;
    match handle_events(pool, scope, directory_id, &events, force_rescan).await {
        Ok(outcome) => {
            state.rescan_pending = outcome.failed;
            if outcome.root_gone {
                return Flow::RootGone;
            }
        }
        Err(err) => {
            eprintln!(
                "warning: failed to process filesystem events: {}",
                Clean(&format!("{err:#}"))
            );
            state.rescan_pending = true;
        }
    }
    Flow::Continue
}

/// What one batch of events changed.
#[derive(Debug, Default)]
struct BatchOutcome {
    synced: u64,
    removed: u64,
    /// Set only when the batch changed rows and local releases were rebuilt.
    rebuild: Option<store::RebuildSummary>,
    root_gone: bool,
    /// A database write failed, so the database may have missed changes.
    failed: bool,
}

/// Applies one batch of events to the database.
///
/// What is on disk now decides what happens to each path, whatever the event
/// said: a directory is scanned, an audio file is read, and a missing path
/// loses its rows. That makes renames, moves and the generic event kinds some
/// platforms report all come out right.
async fn handle_events(
    pool: &SqlitePool,
    scope: &Scope,
    directory_id: i64,
    events: &[notify::Event],
    force_rescan: bool,
) -> Result<BatchOutcome> {
    let plan = plan_batch(scope, events);
    let mut outcome = BatchOutcome::default();
    if force_rescan || plan.rescan {
        // Events were lost or not applied, so a full sync is the only way to be sure.
        let progress = ImportProgress::new();
        let synced = importer::sync_directory(pool, &scope.root, &progress, false).await;
        drop(progress);
        let summary = match synced {
            Ok(summary) => summary,
            Err(err) => {
                // The per-path handling that normally notices a deleted root is
                // skipped during a full sync, so check for it here. The rows
                // stay, as in the per-path case.
                if matches!(
                    tokio::fs::metadata(&scope.root).await,
                    Err(error) if error.kind() == ErrorKind::NotFound
                ) {
                    outcome.root_gone = true;
                    return Ok(outcome);
                }
                return Err(err);
            }
        };
        outcome.synced = summary.saved;
        outcome.removed = summary.removed;
        importer::report_rebuild(&summary.rebuild);
        outcome.rebuild = Some(summary.rebuild);
        if summary.saved + summary.removed > 0 {
            outln!(
                "synced: {} ({} changed, {} removed)",
                clean_path(&scope.root),
                summary.saved,
                summary.removed
            );
        }
        return Ok(outcome);
    }

    for (key, spellings) in &plan.paths {
        for path in spellings {
            // One path failing must not cost the rest of the batch.
            if let Err(err) = apply_path(
                pool,
                scope,
                directory_id,
                key,
                path,
                spellings,
                &mut outcome,
            )
            .await
            {
                eprintln!(
                    "warning: failed to sync {}: {}",
                    clean_path(path),
                    Clean(&format!("{err:#}"))
                );
                outcome.failed |= err.downcast_ref::<sqlx::Error>().is_some();
            }
            if outcome.root_gone {
                return Ok(outcome);
            }
        }
    }

    // Refresh grouping so membership changes mark accepted matches stale while
    // leaving their overlay in place. Nothing changed means nothing to refresh.
    if outcome.synced + outcome.removed > 0 {
        match store::rebuild_local_releases(pool, Some(&scope.root)).await {
            Ok(rebuild) => {
                importer::report_rebuild(&rebuild);
                outcome.rebuild = Some(rebuild);
            }
            Err(err) => {
                eprintln!(
                    "warning: failed to refresh local releases: {}",
                    Clean(&format!("{err:#}"))
                );
                outcome.failed = true;
            }
        }
    }
    Ok(outcome)
}

/// Brings the database in line with what is on disk at one path.
///
/// `spellings` are all the paths of this batch that share `path`'s comparison
/// key. A missing path keeps its rows when another spelling exists on disk,
/// because the rows are case-insensitive and that spelling is saved instead.
async fn apply_path(
    pool: &SqlitePool,
    scope: &Scope,
    directory_id: i64,
    key: &str,
    path: &Path,
    spellings: &[PathBuf],
    outcome: &mut BatchOutcome,
) -> Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            let summary = importer::sync_files(pool, path, directory_id, None, false).await?;
            outcome.synced += summary.saved;
            outcome.removed += summary.removed;
            if summary.saved > 0 {
                outln!(
                    "synced: {} ({})",
                    clean_path(path),
                    plural(summary.saved, "track", "tracks")
                );
            }
            if summary.removed > 0 {
                outln!(
                    "removed: {} ({})",
                    clean_path(path),
                    plural(summary.removed, "track", "tracks")
                );
            }
        }
        Ok(metadata) if metadata.is_file() && scanner::is_audio_file(path) => {
            // The stored spelling wins over a file that differs only in case, as
            // in an import, so the row does not move between the two.
            if let Some(stored) = store::stored_path(pool, path).await?
                && Path::new(&stored) != path
                && tokio::fs::try_exists(&stored).await?
            {
                eprintln!("{}", importer::case_collision_warning(path));
                return Ok(());
            }
            let read = tokio::task::spawn_blocking({
                let path = path.to_path_buf();
                move || scanner::read_tags(&path)
            })
            .await?;
            match read {
                Ok(track) => {
                    let changed =
                        store::upsert_tracks(pool, std::slice::from_ref(&track), directory_id)
                            .await?;
                    outcome.synced += changed;
                    if changed > 0 {
                        outln!("synced: {}", clean_path(path));
                    }
                }
                Err(err) => eprintln!(
                    "warning: failed to tag {}: {}",
                    clean_path(path),
                    Clean(&format!("{err:#}"))
                ),
            }
        }
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if key == scope.root_key {
                // Keep the rows: an unmounted library should not empty the DB.
                outcome.root_gone = true;
                return Ok(());
            }
            for other in spellings.iter().filter(|other| other.as_path() != path) {
                if tokio::fs::try_exists(other).await? {
                    return Ok(());
                }
            }
            let removed = store::remove_tracks_under(pool, path).await?;
            outcome.removed += removed;
            if removed > 0 {
                outln!("removed: {}", clean_path(path));
            }
        }
        Err(err) => eprintln!(
            "warning: failed to inspect {}: {}",
            clean_path(path),
            Clean(&err.to_string())
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_support::{ScratchDir, write_wav};
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode,
    };

    fn event(kind: EventKind, paths: &[&Path]) -> notify::Event {
        let mut event = notify::Event::new(kind);
        for path in paths {
            event = event.add_path(path.to_path_buf());
        }
        event
    }

    struct Fixture {
        music: ScratchDir,
        pool: SqlitePool,
        scope: Scope,
        directory_id: i64,
    }

    impl Fixture {
        async fn new(label: &str) -> Self {
            let music = ScratchDir::new(label);
            let pool = db::connect_in_memory().await.unwrap();
            let scope = Scope::new(music.path(), &music.path().join("radish.db"));
            let directory_id = store::upsert_directory(&pool, music.path()).await.unwrap();
            Self {
                music,
                pool,
                scope,
                directory_id,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.music.path().join(name)
        }

        async fn handle(&self, events: &[notify::Event]) -> BatchOutcome {
            handle_events(&self.pool, &self.scope, self.directory_id, events, false)
                .await
                .unwrap()
        }

        async fn track_count(&self) -> usize {
            store::list_tracks(&self.pool).await.unwrap().len()
        }
    }

    #[tokio::test]
    async fn access_and_metadata_events_do_not_write_or_rebuild() {
        let fixture = Fixture::new("watcher-noise").await;
        let file = fixture.path("a.wav");
        write_wav(&file);

        let outcome = fixture
            .handle(&[
                event(EventKind::Access(AccessKind::Read), &[&file]),
                event(
                    EventKind::Access(AccessKind::Close(AccessMode::Write)),
                    &[&file],
                ),
                event(
                    EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                    &[&file],
                ),
            ])
            .await;

        assert!(outcome.rebuild.is_none(), "{outcome:?}");
        assert_eq!((outcome.synced, outcome.removed), (0, 0));
        assert_eq!(fixture.track_count().await, 0);
    }

    #[tokio::test]
    async fn database_and_sidecar_events_are_ignored() {
        let fixture = Fixture::new("watcher-db-files").await;
        let events: Vec<_> = [
            "radish.db",
            "radish.db-wal",
            "radish.db-shm",
            "radish.db-journal",
            "RADISH.DB-WAL",
        ]
        .iter()
        .flat_map(|name| {
            let path = fixture.path(name);
            [
                event(EventKind::Create(CreateKind::File), &[&path]),
                event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                    &[&path],
                ),
                event(EventKind::Remove(RemoveKind::File), &[&path]),
            ]
        })
        .collect();

        let plan = plan_batch(&fixture.scope, &events);

        assert!(plan.paths.is_empty(), "{:?}", plan.paths.keys());
        assert!(fixture.handle(&events).await.rebuild.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_events_is_collected_into_one_batch() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let file = PathBuf::from("/music/a.wav");
        for _ in 0..100 {
            tx.send(Ok(event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&file],
            )))
            .unwrap();
        }

        let batch = collect_batch(&mut rx, DEBOUNCE).await.unwrap();

        assert_eq!(batch.len(), 100);
        assert!(rx.try_recv().is_err());
        drop(tx);
        assert!(collect_batch(&mut rx, DEBOUNCE).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn events_arriving_inside_the_window_join_the_batch() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let file = PathBuf::from("/music/a.wav");
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        tx.send(Ok(event(data, &[&file]))).unwrap();
        let late = tokio::spawn({
            let tx = tx.clone();
            async move {
                tokio::time::sleep(DEBOUNCE / 2).await;
                tx.send(Ok(event(data, &[Path::new("/music/b.wav")])))
                    .unwrap();
                tokio::time::sleep(DEBOUNCE * 2).await;
                tx.send(Ok(event(data, &[Path::new("/music/c.wav")])))
                    .unwrap();
            }
        });

        let first = collect_batch(&mut rx, DEBOUNCE).await.unwrap();
        let second = collect_batch(&mut rx, DEBOUNCE).await.unwrap();

        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 1);
        late.await.unwrap();
    }

    #[tokio::test]
    async fn repeated_events_for_one_file_are_one_change_and_one_rebuild() {
        let fixture = Fixture::new("watcher-dedup").await;
        let file = fixture.path("a.wav");
        write_wav(&file);
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        let events: Vec<_> = (0..100).map(|_| event(data, &[&file])).collect();

        assert_eq!(plan_batch(&fixture.scope, &events).paths.len(), 1);
        let outcome = fixture.handle(&events).await;

        assert_eq!(outcome.synced, 1, "{outcome:?}");
        assert!(outcome.rebuild.is_some_and(|rebuild| rebuild.created == 1));
        assert_eq!(fixture.track_count().await, 1);
    }

    #[tokio::test]
    async fn rebuild_runs_only_when_rows_changed() {
        let fixture = Fixture::new("watcher-changed-only").await;
        let file = fixture.path("a.wav");
        write_wav(&file);
        let created = event(EventKind::Create(CreateKind::File), &[&file]);
        let removed = event(EventKind::Remove(RemoveKind::File), &[&file]);

        assert!(
            fixture
                .handle(std::slice::from_ref(&created))
                .await
                .rebuild
                .is_some()
        );
        let again = fixture.handle(std::slice::from_ref(&created)).await;
        assert!(again.rebuild.is_none(), "unchanged file: {again:?}");

        std::fs::remove_file(&file).unwrap();
        let first_removal = fixture.handle(std::slice::from_ref(&removed)).await;
        assert_eq!(first_removal.removed, 1);
        assert!(first_removal.rebuild.is_some());
        let second_removal = fixture.handle(&[removed]).await;
        assert!(second_removal.rebuild.is_none(), "{second_removal:?}");
    }

    #[tokio::test]
    async fn a_rescan_event_syncs_the_whole_directory() {
        let fixture = Fixture::new("watcher-rescan").await;
        write_wav(&fixture.path("a.wav"));
        write_wav(&fixture.path("b.wav"));
        let lost = event(EventKind::Other, &[]).set_flag(notify::event::Flag::Rescan);

        let outcome = fixture.handle(&[lost]).await;

        assert_eq!(outcome.synced, 2, "{outcome:?}");
        assert!(outcome.rebuild.is_some());
        assert_eq!(fixture.track_count().await, 2);
    }

    async fn track_paths(fixture: &Fixture) -> Vec<String> {
        let mut paths: Vec<String> = store::list_tracks(&fixture.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|track| track.path)
            .collect();
        paths.sort();
        paths
    }

    async fn import(fixture: &Fixture, names: &[&str]) {
        let events: Vec<_> = names
            .iter()
            .map(|name| event(EventKind::Create(CreateKind::File), &[&fixture.path(name)]))
            .collect();
        fixture.handle(&events).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_case_only_rename_reported_new_name_first_keeps_its_row() {
        let fixture = Fixture::new("watcher-case-rename-order").await;
        write_wav(&fixture.path("a.wav"));
        import(&fixture, &["a.wav"]).await;
        let (old, new) = (fixture.path("a.wav"), fixture.path("A.wav"));
        std::fs::rename(&old, &new).unwrap();
        if old.exists() {
            return; // case-insensitive filesystem: the old name still resolves
        }

        // The old name arrives after the new one.
        fixture
            .handle(&[
                event(EventKind::Create(CreateKind::File), &[&new]),
                event(EventKind::Remove(RemoveKind::File), &[&old]),
            ])
            .await;

        assert_eq!(track_paths(&fixture).await, [new.display().to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_case_colliding_file_does_not_displace_the_stored_spelling() {
        let fixture = Fixture::new("watcher-case-collision").await;
        write_wav(&fixture.path("X.wav"));
        if fixture.path("x.wav").exists() {
            return; // case-insensitive filesystem: the two names cannot coexist
        }
        import(&fixture, &["X.wav"]).await;
        let revisions = || async {
            sqlx::query_scalar::<_, String>("SELECT source_revision FROM local_releases")
                .fetch_all(&fixture.pool)
                .await
                .unwrap()
        };
        let before = revisions().await;
        let stored = track_paths(&fixture).await;
        write_wav(&fixture.path("x.wav"));

        let outcome = fixture
            .handle(&[event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&fixture.path("x.wav")],
            )])
            .await;

        assert_eq!(outcome.synced, 0, "{outcome:?}");
        assert_eq!(track_paths(&fixture).await, stored);
        assert_eq!(revisions().await, before);
    }

    #[tokio::test]
    async fn a_rename_leaves_one_row_at_the_new_path() {
        let fixture = Fixture::new("watcher-rename").await;
        write_wav(&fixture.path("old.wav"));
        import(&fixture, &["old.wav"]).await;
        let (old, new) = (fixture.path("old.wav"), fixture.path("new.wav"));
        std::fs::rename(&old, &new).unwrap();

        let outcome = fixture
            .handle(&[event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[&old, &new],
            )])
            .await;

        assert_eq!(track_paths(&fixture).await, [new.display().to_string()]);
        assert!(outcome.rebuild.is_some());
    }

    #[tokio::test]
    async fn a_rename_reported_as_two_events_leaves_one_row() {
        let fixture = Fixture::new("watcher-rename-split").await;
        write_wav(&fixture.path("old.wav"));
        import(&fixture, &["old.wav"]).await;
        let (old, new) = (fixture.path("old.wav"), fixture.path("new.wav"));
        std::fs::rename(&old, &new).unwrap();

        fixture
            .handle(&[
                event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                    &[&old],
                ),
                event(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[&new]),
            ])
            .await;

        assert_eq!(track_paths(&fixture).await, [new.display().to_string()]);
    }

    #[tokio::test]
    async fn a_folder_moved_into_the_root_is_scanned() {
        let fixture = Fixture::new("watcher-folder-in").await;
        let album = fixture.path("Album");
        std::fs::create_dir(&album).unwrap();
        write_wav(&album.join("01.wav"));
        write_wav(&album.join("02.wav"));

        let outcome = fixture
            .handle(&[event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &[&album],
            )])
            .await;

        assert_eq!(outcome.synced, 2, "{outcome:?}");
        assert_eq!(fixture.track_count().await, 2);
        assert!(outcome.rebuild.is_some());
    }

    #[tokio::test]
    async fn a_file_moved_out_of_the_root_is_removed() {
        let fixture = Fixture::new("watcher-file-out").await;
        write_wav(&fixture.path("a.wav"));
        import(&fixture, &["a.wav"]).await;
        std::fs::remove_file(fixture.path("a.wav")).unwrap();

        let outcome = fixture
            .handle(&[event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &[&fixture.path("a.wav")],
            )])
            .await;

        assert_eq!(outcome.removed, 1, "{outcome:?}");
        assert_eq!(fixture.track_count().await, 0);
    }

    #[tokio::test]
    async fn removing_a_directory_removes_every_row_under_it() {
        let fixture = Fixture::new("watcher-dir-removed").await;
        let album = fixture.path("Album");
        std::fs::create_dir(&album).unwrap();
        write_wav(&album.join("01.wav"));
        write_wav(&album.join("02.wav"));
        write_wav(&fixture.path("Album2.wav"));
        fixture
            .handle(&[event(EventKind::Create(CreateKind::Folder), &[&album])])
            .await;
        import(&fixture, &["Album2.wav"]).await;
        assert_eq!(fixture.track_count().await, 3);
        std::fs::remove_dir_all(&album).unwrap();

        // The backend reports the folder in a different letter case.
        let reported = fixture.path("ALBUM");
        let outcome = fixture
            .handle(&[event(EventKind::Remove(RemoveKind::Folder), &[&reported])])
            .await;

        assert_eq!(outcome.removed, 2, "{outcome:?}");
        assert_eq!(
            track_paths(&fixture).await,
            [fixture.path("Album2.wav").display().to_string()]
        );
    }

    #[tokio::test]
    async fn a_create_event_of_unknown_kind_syncs_a_new_file() {
        let fixture = Fixture::new("watcher-create-any").await;
        write_wav(&fixture.path("a.wav"));
        std::fs::write(fixture.path("cover.jpg"), b"not audio").unwrap();

        let outcome = fixture
            .handle(&[
                event(
                    EventKind::Create(CreateKind::Any),
                    &[&fixture.path("a.wav")],
                ),
                event(
                    EventKind::Modify(ModifyKind::Any),
                    &[&fixture.path("cover.jpg")],
                ),
            ])
            .await;

        assert_eq!(outcome.synced, 1, "{outcome:?}");
        assert_eq!(fixture.track_count().await, 1);
    }

    #[tokio::test]
    async fn paths_outside_the_root_are_ignored() {
        let fixture = Fixture::new("watcher-outside").await;
        let sibling = PathBuf::from(format!("{}-other/a.wav", fixture.music.path().display()));

        let plan = plan_batch(
            &fixture.scope,
            &[event(EventKind::Remove(RemoveKind::File), &[&sibling])],
        );

        assert!(plan.paths.is_empty());
    }

    #[tokio::test]
    async fn removing_the_root_stops_the_watch_and_keeps_the_rows() {
        let fixture = Fixture::new("watcher-root-gone").await;
        write_wav(&fixture.path("a.wav"));
        import(&fixture, &["a.wav"]).await;
        std::fs::remove_dir_all(fixture.music.path()).unwrap();

        let outcome = fixture
            .handle(&[event(
                EventKind::Remove(RemoveKind::Folder),
                &[fixture.music.path()],
            )])
            .await;

        assert!(outcome.root_gone, "{outcome:?}");
        assert_eq!(fixture.track_count().await, 1);
    }

    #[tokio::test]
    async fn a_watch_error_in_a_batch_does_not_stop_the_rest() {
        let fixture = Fixture::new("watcher-error-event").await;
        write_wav(&fixture.path("a.wav"));
        // No event is ever reported for b.wav; only a full sync can find it.
        write_wav(&fixture.path("b.wav"));
        let batch = vec![
            Err(notify::Error::generic("queue overflow")),
            Ok(event(
                EventKind::Create(CreateKind::File),
                &[&fixture.path("a.wav")],
            )),
        ];

        let flow = process_batch(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            batch,
            &mut WatchState::default(),
        )
        .await;

        assert_eq!(flow, Flow::Continue);
        assert_eq!(fixture.track_count().await, 2);
    }

    /// Makes saving any file named `bad.wav` fail with a database error.
    async fn fail_saving_bad_files(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TRIGGER reject_bad BEFORE INSERT ON tracks
             WHEN NEW.path LIKE '%bad.wav'
             BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    fn create_events(fixture: &Fixture, names: &[&str]) -> Vec<notify::Event> {
        names
            .iter()
            .map(|name| event(EventKind::Create(CreateKind::File), &[&fixture.path(name)]))
            .collect()
    }

    #[tokio::test]
    async fn one_failing_path_does_not_stop_the_rest_of_the_batch() {
        let fixture = Fixture::new("watcher-path-failure").await;
        let names = ["a-good.wav", "bad.wav", "z-good.wav"];
        for name in names {
            write_wav(&fixture.path(name));
        }
        fail_saving_bad_files(&fixture.pool).await;

        let outcome = handle_events(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            &create_events(&fixture, &names),
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome.synced, 2, "{outcome:?}");
        assert!(outcome.failed, "{outcome:?}");
        assert!(outcome.rebuild.is_some_and(|rebuild| rebuild.groups > 0));
        assert_eq!(fixture.track_count().await, 2);
    }

    #[tokio::test]
    async fn a_database_failure_makes_the_next_batch_a_full_sync() {
        let fixture = Fixture::new("watcher-pending-rescan").await;
        let names = ["a-good.wav", "bad.wav"];
        for name in names {
            write_wav(&fixture.path(name));
        }
        fail_saving_bad_files(&fixture.pool).await;
        let mut state = WatchState::default();
        let batch = create_events(&fixture, &names)
            .into_iter()
            .map(Ok)
            .collect();
        process_batch(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            batch,
            &mut state,
        )
        .await;
        assert!(state.rescan_pending);
        assert_eq!(fixture.track_count().await, 1);

        sqlx::query("DROP TRIGGER reject_bad")
            .execute(&fixture.pool)
            .await
            .unwrap();
        // An unrelated read event: nothing to do for it except the pending sync.
        let quiet = vec![Ok(event(
            EventKind::Access(AccessKind::Read),
            &[&fixture.path("a-good.wav")],
        ))];
        process_batch(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            quiet,
            &mut state,
        )
        .await;

        assert!(!state.rescan_pending);
        assert_eq!(fixture.track_count().await, 2);
    }

    #[tokio::test]
    async fn removing_the_root_ends_the_watch() {
        let fixture = Fixture::new("watcher-root-flow").await;
        std::fs::remove_dir_all(fixture.music.path()).unwrap();
        let batch = vec![Ok(event(
            EventKind::Remove(RemoveKind::Folder),
            &[fixture.music.path()],
        ))];

        let flow = process_batch(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            batch,
            &mut WatchState::default(),
        )
        .await;

        assert_eq!(flow, Flow::RootGone);
    }

    #[tokio::test]
    async fn a_pending_full_sync_notices_that_the_root_is_gone() {
        let fixture = Fixture::new("watcher-root-gone-while-pending").await;
        write_wav(&fixture.path("a.wav"));
        let mut state = WatchState {
            rescan_pending: true,
        };
        std::fs::remove_dir_all(fixture.music.path()).unwrap();
        // An unrelated event: only the pending full sync looks at the root.
        let batch = vec![Ok(event(
            EventKind::Access(AccessKind::Read),
            &[&fixture.path("a.wav")],
        ))];

        let flow = process_batch(
            &fixture.pool,
            &fixture.scope,
            fixture.directory_id,
            batch,
            &mut state,
        )
        .await;

        assert_eq!(flow, Flow::RootGone);
    }

    #[tokio::test(start_paused = true)]
    async fn a_continuous_stream_is_cut_off_at_the_window() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        // One event every 150 ms for 900 ms, longer than the 500 ms window.
        let sender = tokio::spawn(async move {
            for n in 0..6 {
                tx.send(Ok(event(data, &[Path::new(&format!("/music/{n}.wav"))])))
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });

        let first = collect_batch(&mut rx, DEBOUNCE).await.unwrap();
        let second = collect_batch(&mut rx, DEBOUNCE).await.unwrap();

        // Events at 0, 150, 300 and 450 ms make the first batch; the window
        // closes at 500 ms even though events keep coming.
        assert_eq!(first.len(), 4);
        assert_eq!(second.len(), 2);
        sender.await.unwrap();
    }
}
