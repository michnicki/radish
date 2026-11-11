mod support;

use radish::scanner::{
    ScanEvent, scan_directory, scan_directory_incremental, scan_directory_with_progress,
};
use support::{temp_dir, write_wav};

#[tokio::test]
async fn empty_directory_reports_discovery_without_file_completions() {
    let root = temp_dir("scan-progress-empty");
    let mut events = Vec::new();
    let tracks = scan_directory_with_progress(&root, |event| events.push(event))
        .await
        .unwrap();

    assert!(tracks.is_empty());
    assert_eq!(
        events,
        [ScanEvent::Discovering, ScanEvent::Discovered { total: 0 }]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn scan_counts_successful_reads_and_ignores_non_audio_files() {
    let root = temp_dir("scan-progress-valid");
    std::fs::create_dir(root.join("nested")).unwrap();
    write_wav(&root.join("01.WAV"));
    write_wav(&root.join("nested/02.wav"));
    std::fs::write(root.join("cover.jpg"), b"not audio").unwrap();
    let mut events = Vec::new();
    let tracks = scan_directory_with_progress(&root, |event| events.push(event))
        .await
        .unwrap();

    assert_eq!(tracks.len(), 2);
    assert_eq!(
        events,
        [
            ScanEvent::Discovering,
            ScanEvent::Discovered { total: 2 },
            ScanEvent::FileFinished {
                completed: 1,
                total: 2,
                warning: None
            },
            ScanEvent::FileFinished {
                completed: 2,
                total: 2,
                warning: None
            },
        ]
    );
    // Existing library callers retain the original entrypoint.
    assert_eq!(scan_directory(&root).await.unwrap().len(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_tag_reads_advance_progress_and_leave_successful_tracks() {
    let root = temp_dir("scan-progress-mixed");
    write_wav(&root.join("good.wav"));
    std::fs::write(root.join("broken.mp3"), b"not audio").unwrap();
    let mut events = Vec::new();
    let tracks = scan_directory_with_progress(&root, |event| events.push(event))
        .await
        .unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].path, root.join("good.wav"));
    assert_eq!(events[0], ScanEvent::Discovering);
    assert_eq!(events[1], ScanEvent::Discovered { total: 2 });
    let mut warnings = Vec::new();
    for (index, event) in events[2..].iter().enumerate() {
        let ScanEvent::FileFinished {
            completed,
            total,
            warning,
        } = event
        else {
            panic!("expected a file completion, got {event:?}");
        };
        assert_eq!(*completed, index + 1);
        assert_eq!(*total, 2);
        if let Some(warning) = warning {
            warnings.push(warning);
        }
    }
    assert_eq!(events.len(), 4);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("failed to read tags"));
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unreadable_directories_produce_a_warning_naming_the_path() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_dir("scan-unreadable-dir");
    let locked = root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    write_wav(&locked.join("hidden.wav"));
    write_wav(&root.join("visible.wav"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores directory permissions, so the walk cannot fail there.
    let blocked = std::fs::read_dir(&locked).is_err();

    let mut events = Vec::new();
    let result = scan_directory_with_progress(&root, |event| events.push(event)).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tracks = result.unwrap();

    if blocked {
        let warning = events
            .iter()
            .find_map(|event| match event {
                ScanEvent::Warning { message } => Some(message),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a walk warning, got {events:?}"));
        assert!(warning.starts_with("warning: skipped "), "{warning}");
        assert!(warning.contains(locked.to_str().unwrap()), "{warning}");
        assert_eq!(tracks.len(), 1);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_audio_files_are_imported() {
    let root = temp_dir("scan-symlink-file");
    let outside = temp_dir("scan-symlink-file-target");
    write_wav(&outside.join("real.wav"));
    std::os::unix::fs::symlink(outside.join("real.wav"), root.join("link.wav")).unwrap();

    let tracks = scan_directory_with_progress(&root, |_| {}).await.unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].path, root.join("link.wav"));
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[tokio::test]
async fn corrupt_files_produce_a_warning_containing_their_path() {
    let root = temp_dir("scan-corrupt-flac");
    let broken = root.join("broken.flac");
    std::fs::write(&broken, b"not audio").unwrap();
    let mut events = Vec::new();

    let tracks = scan_directory_with_progress(&root, |event| events.push(event))
        .await
        .unwrap();

    assert!(tracks.is_empty());
    let warning = events
        .iter()
        .find_map(|event| match event {
            ScanEvent::FileFinished {
                warning: Some(warning),
                ..
            } => Some(warning),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a read warning, got {events:?}"));
    assert!(warning.contains(broken.to_str().unwrap()), "{warning}");
    std::fs::remove_dir_all(root).unwrap();
}

fn mtime_secs(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn unchanged_files_are_skipped_but_still_counted_as_finished() {
    let root = temp_dir("scan-incremental");
    let known = root.join("known.wav");
    let fresh = root.join("fresh.wav");
    write_wav(&known);
    write_wav(&fresh);
    let known_mtime = mtime_secs(&known);
    let mut events = Vec::new();

    let result = scan_directory_incremental(
        &root,
        |path, mtime| path == known && mtime == known_mtime,
        |event| events.push(event),
    )
    .await
    .unwrap();

    assert_eq!(result.unchanged, std::slice::from_ref(&known));
    assert_eq!(result.tracks.len(), 1);
    assert_eq!(result.tracks[0].path, fresh);
    assert_eq!(events[1], ScanEvent::Discovered { total: 2 });
    let finished: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::FileFinished {
                completed, total, ..
            } => {
                assert_eq!(*total, 2);
                Some(*completed)
            }
            _ => None,
        })
        .collect();
    assert_eq!(finished, [1, 2]);

    // A different modification time means the file is read again.
    let stale = scan_directory_incremental(&root, |_, mtime| mtime == known_mtime + 1, |_| {})
        .await
        .unwrap();
    assert!(stale.unchanged.is_empty());
    assert_eq!(stale.tracks.len(), 2);
    std::fs::remove_dir_all(root).unwrap();
}
