//! Radish: local music library indexing with MusicBrainz-first matching and an
//! optional Discogs fallback.
//!
//! Modules follow the provider boundary:
//!
//! - [`providers`]: typed provider identities and the small adapter interface.
//! - [`musicbrainz`]: MusicBrainz transport, query escaping, and conversion.
//! - [`discogs`]: Discogs provider access, credentials, retries, conversion.
//! - [`deezer`]: Deezer transport, free-text search, and conversion.
//! - [`discovery`]: bounded provider-neutral candidate discovery.
//! - [`index`]: local provider indexes built from the official data dumps.
//! - [`matcher`]: pure normalization, mapping, scoring, and acceptance rules.
//! - [`scene`]: artist and album read from a scene release directory name.
//! - [`store`]: grouping, revisions, review queue, and transactional overlay.
//! - [`runner`]: CLI orchestration and the interactive review screen.
//! - [`web`]: the read-only web interface behind `radish serve`.

pub mod db;
pub mod deezer;
pub mod discogs;
pub mod discovery;
pub mod importer;
pub mod index;
pub mod layout;
pub mod matcher;
pub mod musicbrainz;
pub mod output;
pub mod progress;
pub mod providers;
pub mod runner;
pub mod scanner;
pub mod scene;
pub mod store;
#[cfg(test)]
mod test_support;
pub mod watcher;
pub mod web;

/// Prints a line to stdout; a write failure ends the process.
///
/// `println!` panics when the reader exits early (`radish list | head`); CLI
/// output uses this macro instead. On a closed pipe the process exits with
/// status 0, and on any other write error it reports the error and exits 1.
#[macro_export]
macro_rules! outln {
    () => {
        $crate::output::write_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::write_line(format_args!($($arg)*))
    };
}

/// Prints text to stdout without a trailing newline.
///
/// Write failures end the process like [`outln!`]: status 0 on a closed pipe,
/// status 1 on any other error.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::write(format_args!($($arg)*))
    };
}
