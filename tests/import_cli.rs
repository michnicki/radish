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
