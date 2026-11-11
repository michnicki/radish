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

pub mod db;
pub mod discogs;
pub mod discovery;
pub mod matcher;
pub mod musicbrainz;
pub mod providers;
pub mod runner;
pub mod scanner;
pub mod store;
pub mod watcher;
