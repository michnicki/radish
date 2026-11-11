use crate::{scanner, store};
use anyhow::Result;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{EventKind, RecursiveMode, Watcher};
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::mpsc;

/// Watches `root` for filesystem changes and keeps the database in sync in
/// both directions: edits or new files get (re-)tagged into the DB, and
/// deletions remove the corresponding track rows.
///
/// Observed metadata is refreshed per event. Accepted Discogs metadata lives in
/// separate tables, so refreshes never overwrite an accepted match.
pub async fn watch_directory(pool: SqlitePool, root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let directory_id = store::upsert_directory(&pool, &root).await?;

    println!("Doing initial scan of {}...", root.display());
    let tracks = scanner::scan_directory(&root).await?;
    for track in &tracks {
        store::upsert_track(&pool, track, directory_id).await?;
    }
    store::rebuild_local_releases(&pool, Some(&root)).await?;
    println!(
        "Synced {} tracks. Watching for changes... (Ctrl+C to stop)",
        tracks.len()
    );

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    for res in rx {
        match res {
            Ok(event) => {
                if let Err(err) = handle_event(&pool, directory_id, &root, event).await {
                    eprintln!("warning: failed to process filesystem event: {err}");
                }
            }
            Err(err) => eprintln!("watch error: {err}"),
        }
    }

    Ok(())
}

async fn handle_event(
    pool: &SqlitePool,
    directory_id: i64,
    root: &Path,
    event: notify::Event,
) -> Result<()> {
    match event.kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Name(_)) => {
            for path in event.paths {
                if path.is_file() && scanner::is_audio_file(&path) {
                    match scanner::read_tags(&path) {
                        Ok(track) => {
                            store::upsert_track(pool, &track, directory_id).await?;
                            println!("synced: {}", path.display());
                        }
                        Err(err) => eprintln!("warning: failed to tag {}: {err}", path.display()),
                    }
                }
            }
        }
        EventKind::Remove(RemoveKind::File) | EventKind::Remove(RemoveKind::Any) => {
            for path in event.paths {
                store::remove_track_by_path(pool, &path).await?;
                println!("removed: {}", path.display());
            }
        }
        _ => {}
    }

    // Refresh grouping so membership changes mark accepted matches stale while
    // leaving their overlay in place.
    store::rebuild_local_releases(pool, Some(root)).await?;

    Ok(())
}
