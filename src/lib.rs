//! Radish: local music library indexing with MusicBrainz-first matching and an
//! optional Discogs fallback.
//!
//! Modules follow the provider boundary:
//!
//! - [`providers`]: typed provider identities and the small adapter interface.
//! - [`musicbrainz`]: MusicBrainz transport, query escaping, and conversion.
//! - [`discogs`]: Discogs provider access, credentials, retries, conversion.
//! - [`discovery`]: bounded provider-neutral candidate discovery.
//! - [`matcher`]: pure normalization, mapping, scoring, and acceptance rules.
//! - [`store`]: grouping, revisions, review queue, and transactional overlay.
//! - [`runner`]: CLI orchestration and the interactive review screen.

pub mod clef;
pub mod db;
pub mod discogs;
pub mod discovery;
pub mod matcher;
pub mod musicbrainz;
pub mod output;
pub mod progress;
pub mod providers;
pub mod runner;
pub mod scanner;
pub mod store;
pub mod watcher;

/// Prints a line to stdout, treating a closed pipe as a clean end.
///
/// `println!` panics when the reader exits early (`radish list | head`); CLI
/// output uses this macro so a closed pipe just stops the output.
#[macro_export]
macro_rules! outln {
    () => {
        $crate::output::write_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::write_line(format_args!($($arg)*))
    };
}

/// Prints text to stdout without a trailing newline, tolerating a closed pipe.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::write(format_args!($($arg)*))
    };
}
