mod support;

use radish::scanner::{ScanEvent, scan_directory, scan_directory_with_progress};
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
