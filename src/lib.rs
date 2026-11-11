//! Radish: local music library indexing with optional Discogs matching.
//!
//! Modules are separated along the provider boundary described in
//! `discogs_plan.md`:
//!
//! - [`discogs`]: provider access, credentials, retries, and DTO conversion.
//! - [`matcher`]: pure normalization, mapping, scoring, and acceptance rules.
//! - [`store`]: grouping, revisions, review queue, and transactional overlay.
//! - [`runner`]: CLI orchestration and the interactive review screen.

pub mod db;
pub mod discogs;
pub mod matcher;
pub mod runner;
pub mod scanner;
pub mod store;
pub mod watcher;
