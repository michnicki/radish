//! Terminal-only progress shared by import and the initial watcher sync.

use crate::layout::{format_elapsed, grouped, plural_grouped};
use crate::outln;
use crate::output::Clean;
use crate::scanner::ScanEvent;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use std::fmt::Write as _;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::Duration;

/// The live line during the walk: counts, elapsed time, then the folder,
/// which is cut to fit the terminal.
const WALKING: &str = "{spinner} {prefix} [{took}] {wide_msg}";
/// The live line of a step without a count.
const SPINNER: &str = "{spinner} {prefix} [{took}]";
/// The live line of a counted step.
const COUNTER: &str = "{spinner} {prefix}: {human_pos}/{human_len}{msg} [{took}]";

/// Shows each step of an import on stderr, with its count and running time.
///
/// The live line only appears on a terminal and clears on drop, including
/// when an import returns an error. The line summing up the walk goes to
/// stdout either way and stays.
pub struct ImportProgress {
    bar: ProgressBar,
}

impl ImportProgress {
    pub fn new() -> Self {
        let bar = ProgressBar::new_spinner();
        if !io::stderr().is_terminal() {
            bar.set_draw_target(ProgressDrawTarget::hidden());
        }
        if !bar.is_hidden() {
            bar.enable_steady_tick(Duration::from_millis(100));
        }
        Self { bar }
    }

    pub fn scan_event(&self, event: ScanEvent) {
        match event {
            ScanEvent::Discovering => self.step(WALKING, "Discovering audio files".to_string(), ""),
            ScanEvent::Walking {
                files,
                folders,
                folder,
            } => {
                self.bar.set_prefix(walking(files, folders));
                self.bar.set_message(folder_note(&folder));
            }
            ScanEvent::Discovered {
                total,
                unchanged,
                folders,
            } => {
                let line = walk_summary(total, unchanged, folders, self.bar.elapsed());
                self.bar.suspend(|| outln!("{line}"));
                let note = if unchanged > 0 {
                    format!(", {} unchanged", grouped(unchanged as u64))
                } else {
                    String::new()
                };
                self.counter("Reading tags", total - unchanged, note);
            }
            ScanEvent::Warning { message } => self.warn(&message),
            ScanEvent::FileFinished {
                completed, warning, ..
            } => {
                self.bar.set_position(completed as u64);
                if let Some(warning) = warning {
                    self.warn(&warning);
                }
            }
        }
    }

    fn warn(&self, message: &str) {
        // Unlike ProgressBar::println, this also prints when hidden.
        // Warnings can carry file names, which are untrusted.
        self.bar.suspend(|| eprintln!("{}", Clean(message)));
    }

    pub fn saving(&self, total: usize) {
        self.counter("Saving tracks", total, String::new());
    }

    /// Called only after a successful database upsert.
    pub fn saved(&self) {
        self.bar.inc(1);
    }

    pub fn grouping(&self) {
        self.step(SPINNER, "Rebuilding local-release groups".to_string(), "");
    }

    fn counter(&self, prefix: &str, total: usize, note: String) {
        self.step(COUNTER, prefix.to_string(), note);
        self.bar.set_position(0);
        self.bar.set_length(total as u64);
    }

    /// Starts a step: its own line and its own running time.
    fn step(&self, template: &str, prefix: String, message: impl Into<String>) {
        self.bar.set_style(
            ProgressStyle::with_template(template)
                .expect("valid progress template")
                .with_key("took", took),
        );
        self.bar.set_prefix(prefix);
        self.bar.set_message(message.into());
        self.bar.reset_elapsed();
    }
}

impl Default for ImportProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ImportProgress {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

/// Writes how long the current step has run, for the `{took}` placeholder.
fn took(state: &ProgressState, out: &mut dyn std::fmt::Write) {
    let _ = out.write_str(&format_elapsed(state.elapsed()));
}

/// `Discovering audio files: 812,345 in 52,310 folders`.
fn walking(files: usize, folders: usize) -> String {
    format!(
        "Discovering audio files: {} in {}",
        grouped(files as u64),
        plural_grouped(folders as u64, "folder", "folders")
    )
}

/// `· 2004/2004-06-12`, or nothing for the root. Folder names are untrusted.
fn folder_note(folder: &Path) -> String {
    let mut note = String::new();
    if !folder.as_os_str().is_empty() {
        let _ = write!(note, "· {}", Clean(&folder.to_string_lossy()));
    }
    note
}

/// The stdout line that sums up the walk.
fn walk_summary(total: usize, unchanged: usize, folders: usize, took: Duration) -> String {
    format!(
        "Found {} in {} in {}: {} unchanged, {} to read.",
        plural_grouped(total as u64, "audio file", "audio files"),
        plural_grouped(folders as u64, "folder", "folders"),
        format_elapsed(took),
        grouped(unchanged as u64),
        grouped((total - unchanged) as u64)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_line_counts_files_and_folders() {
        assert_eq!(
            walking(812_345, 52_310),
            "Discovering audio files: 812,345 in 52,310 folders"
        );
        assert_eq!(walking(0, 1), "Discovering audio files: 0 in 1 folder");
    }

    #[test]
    fn the_folder_note_escapes_untrusted_names() {
        assert_eq!(folder_note(Path::new("")), "");
        assert_eq!(
            folder_note(Path::new("2004/2004-06-12")),
            "· 2004/2004-06-12"
        );
        assert_eq!(folder_note(Path::new("a\u{1b}[31m")), "· a\\u{1b}[31m");
    }

    #[test]
    fn the_walk_summary_names_what_is_left_to_read() {
        assert_eq!(
            walk_summary(2_762_328, 2_761_900, 312_004, Duration::from_secs(842)),
            "Found 2,762,328 audio files in 312,004 folders in 14m02s: \
             2,761,900 unchanged, 428 to read."
        );
        assert_eq!(
            walk_summary(1, 0, 1, Duration::ZERO),
            "Found 1 audio file in 1 folder in 0s: 0 unchanged, 1 to read."
        );
    }

    #[test]
    fn every_step_has_a_valid_template() {
        let progress = ImportProgress {
            bar: ProgressBar::hidden(),
        };
        progress.scan_event(ScanEvent::Discovering);
        progress.scan_event(ScanEvent::Walking {
            files: 3,
            folders: 2,
            folder: "a".into(),
        });
        progress.saving(1);
        progress.saved();
        progress.grouping();
    }
}
