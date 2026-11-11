//! Terminal-only progress shared by import and the initial watcher sync.

use crate::output::Clean;
use crate::scanner::ScanEvent;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{self, IsTerminal};
use std::time::Duration;

/// Clears the live display on drop, including when an import returns an error.
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
            ScanEvent::Discovering => self.spinner("Discovering audio files"),
            ScanEvent::Discovered { total } => self.counter("Reading tags", total),
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
        self.counter("Saving tracks", total);
    }

    /// Called only after a successful database upsert.
    pub fn saved(&self) {
        self.bar.inc(1);
    }

    pub fn grouping(&self) {
        self.spinner("Rebuilding local-release groups");
    }

    fn counter(&self, message: &'static str, total: usize) {
        self.bar.set_style(
            ProgressStyle::with_template("{spinner} {msg}: {pos}/{len}")
                .expect("valid counter template"),
        );
        self.bar.set_position(0);
        self.bar.set_length(total as u64);
        self.bar.set_message(message);
    }

    fn spinner(&self, message: &'static str) {
        self.bar.set_style(
            ProgressStyle::with_template("{spinner} {msg}").expect("valid spinner template"),
        );
        self.bar.set_message(message);
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
