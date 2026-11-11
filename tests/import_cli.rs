//! `radish import` against a real directory and database.

mod support;

use std::process::Command;
use support::{db_path, temp_dir, write_wav};

fn radish(db: &std::path::Path, args: &[&std::path::Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn importing_an_emptied_directory_warns_and_keeps_the_library() {
    let db = db_path("import-emptied");
    let music = temp_dir("import-emptied-music");
    write_wav(&music.join("a.wav"));
    write_wav(&music.join("b.wav"));
    let first = radish(&db, &["import".as_ref(), music.as_ref()]);
    assert!(first.status.success(), "{first:?}");

    std::fs::remove_file(music.join("a.wav")).unwrap();
    std::fs::remove_file(music.join("b.wav")).unwrap();
    let second = radish(&db, &["import".as_ref(), music.as_ref()]);

    assert!(second.status.success(), "{second:?}");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("0 removed, 2 kept (unreadable)"),
        "{stdout}"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("warning: no audio files found under")
            && stderr.contains("keeping 2 stored tracks (is the volume mounted?)"),
        "{stderr}"
    );
    let list = radish(&db, &["list".as_ref()]);
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("2 track(s)"),
        "{list:?}"
    );
}

/// Paths come from untrusted folder and file names, so no output may carry a
/// raw escape byte.
#[cfg(unix)]
#[test]
fn importing_paths_with_escape_bytes_prints_no_raw_escape() {
    let db = db_path("import-escape");
    let music = temp_dir("import-escape-music").join("dir\x1b[2Jname");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("ok\x1b]0;title\x07.wav"));
    std::fs::write(music.join("bad\x1b[31m.wav"), b"not really audio").unwrap();
    let dangling = music.join("dangling\x1b[1m.wav");
    std::os::unix::fs::symlink(music.join("missing\x1b.wav"), &dangling).unwrap();

    let first = radish(&db, &["import".as_ref(), music.as_ref()]);
    assert!(first.status.success(), "{first:?}");
    // The folder is now empty of audio, which triggers the "keeping" warning.
    for entry in std::fs::read_dir(&music).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let second = radish(&db, &["import".as_ref(), music.as_ref()]);
    assert!(second.status.success(), "{second:?}");
    let missing = radish(
        &db,
        &["import".as_ref(), music.join("gone\x1b[0m").as_ref()],
    );
    assert!(!missing.status.success());
    // An error context that names a path is printed by `main`.
    let unresolved = radish(
        &db,
        &[
            "releases".as_ref(),
            "--path".as_ref(),
            music.join("nope\x1b[0m").as_ref(),
        ],
    );
    assert!(!unresolved.status.success());
    assert!(
        String::from_utf8_lossy(&unresolved.stderr).contains("cannot resolve"),
        "{unresolved:?}"
    );

    let mut saw_path_text = false;
    for output in [&first, &second, &missing, &unresolved] {
        for (name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            assert!(
                !bytes.contains(&0x1b),
                "raw ESC in {name}: {:?}",
                String::from_utf8_lossy(bytes)
            );
            saw_path_text |= String::from_utf8_lossy(bytes).contains("\\u{1b}");
        }
    }
    assert!(saw_path_text, "the escaped path should still be shown");
}

/// Polls `path` until it contains `needle`, for up to 20 seconds.
#[cfg(unix)]
fn wait_for_text(path: &std::path::Path, needle: &str) -> bool {
    for _ in 0..200 {
        if std::fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

#[cfg(unix)]
#[test]
fn watching_paths_with_escape_bytes_prints_no_raw_escape() {
    let db = db_path("watch-escape");
    let out_dir = temp_dir("watch-escape-out");
    let music = temp_dir("watch-escape-music").join("lib\x1b[2J");
    std::fs::create_dir(&music).unwrap();
    let stdout_path = out_dir.join("stdout.txt");
    let stderr_path = out_dir.join("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_radish"))
        .arg("--db")
        .arg(&db)
        .arg("watch")
        .arg(&music)
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();

    let started = wait_for_text(&stdout_path, "Watching for changes");
    if started {
        write_wav(&music.join("new\x1b[31m.wav"));
        std::fs::write(music.join("bad\x1b[32m.wav"), b"not really audio").unwrap();
        let synced = wait_for_text(&stdout_path, "synced:");
        let warned = wait_for_text(&stderr_path, "warning");
        std::fs::remove_dir_all(&music).unwrap();
        let removed = wait_for_text(&stdout_path, "no longer watching");
        assert!(
            synced && warned && removed,
            "synced={synced} warned={warned} removed={removed}"
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(started, "the watcher did not start");

    for path in [&stdout_path, &stderr_path] {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !bytes.contains(&0x1b),
            "raw ESC in {}: {:?}",
            path.display(),
            String::from_utf8_lossy(&bytes)
        );
    }
}
