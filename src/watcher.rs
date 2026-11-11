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
    let directory_id = store::upsert_directory(&pool, &root).await?;

    // Start watching first so changes made during the initial sync are queued.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    outln!("Doing initial scan of {}...", root.display());
    let progress = ImportProgress::new();
    let summary = importer::sync_directory(&pool, &root, &progress).await?;
    drop(progress);
    outln!(
        "Synced {} tracks ({} unchanged, {} removed). Watching for changes... (Ctrl+C to stop)",
        summary.scanned + summary.unchanged,
        summary.unchanged,
        summary.removed
    );

    while let Some(batch) = collect_batch(&mut rx, DEBOUNCE).await {
        let mut events = Vec::new();
        for res in batch {
            match res {
                Ok(event) => events.push(event),
                Err(err) => eprintln!("watch error: {err}"),
            }
        }
        match handle_events(&pool, &scope, directory_id, &events).await {
            Ok(outcome) if outcome.root_gone => {
                outln!("{} was removed; no longer watching.", root.display());
                break;
            }
            Ok(_) => {}
            Err(err) => eprintln!("warning: failed to process filesystem events: {err}"),
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
        key == self.root_key
            || key
                .strip_prefix(&self.root_key)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    fn is_ignored(&self, key: &str) -> bool {
        self.ignored.iter().any(|ignored| ignored == key)
    }
}

#[derive(Default)]
struct Plan {
    /// Paths worth looking at, once each, keyed by their comparison key.
    paths: BTreeMap<String, PathBuf>,
    /// The backend lost track of events, so only a full sync is safe.
    rescan: bool,
}

/// Reduces a batch of raw events to the paths that matter.
///
/// Reads, metadata-only changes, the database's own files and anything outside
/// the root are dropped, and repeated events for one path collapse into one.
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
                plan.paths.insert(key, path.clone());
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

/// What one batch of events changed.
#[derive(Debug, Default)]
struct BatchOutcome {
    synced: u64,
    removed: u64,
    /// Set only when the batch changed rows and local releases were rebuilt.
    rebuild: Option<store::RebuildSummary>,
    root_gone: bool,
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
) -> Result<BatchOutcome> {
    let plan = plan_batch(scope, events);
    let mut outcome = BatchOutcome::default();
    if plan.rescan {
        // Events were lost, so a full sync is the only way to be sure.
        let progress = ImportProgress::new();
        let summary = importer::sync_directory(pool, &scope.root, &progress).await?;
        outcome.synced = summary.saved;
        outcome.removed = summary.removed;
        outcome.rebuild = Some(summary.rebuild);
        if summary.saved + summary.removed > 0 {
            outln!(
                "synced: {} ({} changed, {} removed)",
                scope.root.display(),
                summary.saved,
                summary.removed
            );
        }
        return Ok(outcome);
    }

    for (key, path) in &plan.paths {
        match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.is_dir() => {
                let summary = importer::sync_files(pool, path, directory_id, None).await?;
                outcome.synced += summary.saved;
                outcome.removed += summary.removed;
                if summary.saved > 0 {
                    outln!("synced: {} ({} tracks)", path.display(), summary.saved);
                }
                if summary.removed > 0 {
                    outln!("removed: {} ({} tracks)", path.display(), summary.removed);
                }
            }
            Ok(metadata) if metadata.is_file() && scanner::is_audio_file(path) => {
                let read = tokio::task::spawn_blocking({
                    let path = path.clone();
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
                            outln!("synced: {}", path.display());
                        }
                    }
                    Err(err) => eprintln!("warning: failed to tag {}: {err}", path.display()),
                }
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if *key == scope.root_key {
                    // Keep the rows: an unmounted library should not empty the DB.
                    outcome.root_gone = true;
                    return Ok(outcome);
                }
                let removed = store::remove_tracks_under(pool, path).await?;
                outcome.removed += removed;
                if removed > 0 {
                    outln!("removed: {}", path.display());
                }
            }
            Err(err) => eprintln!("warning: failed to inspect {}: {err}", path.display()),
        }
    }

    // Refresh grouping so membership changes mark accepted matches stale while
    // leaving their overlay in place. Nothing changed means nothing to refresh.
    if outcome.synced + outcome.removed > 0 {
        outcome.rebuild = Some(store::rebuild_local_releases(pool, Some(&scope.root)).await?);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::importer::test_support::{ScratchDir, write_wav};
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
            handle_events(&self.pool, &self.scope, self.directory_id, events)
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
}
