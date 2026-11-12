//! Provider indexes built from the official MusicBrainz and Discogs dumps.
//!
//! Matching is slow because providers throttle requests, not because scoring
//! is slow. An index makes `search` an FTS or exact-identifier query and
//! `fetch_release` one row read, behind the same
//! [`crate::providers::MetadataProvider`] seam the HTTP adapters use.
//!
//! Index files are disposable derived artifacts. `radish index build` creates
//! them and they carry a schema version in their own `meta` table rather than
//! a SQLx migration, so the library database and its migrations are untouched.

pub mod cli;
pub mod copy;
pub mod discogs;
pub mod local;
pub mod musicbrainz;
pub mod writer;

use crate::output::clean_path;
use crate::providers::ProviderKind;
use anyhow::{Context, Result, bail};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where `radish index build` writes by default, and the environment override.
pub const INDEX_DIR_ENV: &str = "RADISH_INDEX_DIR";
pub const DEFAULT_INDEX_DIR: &str = "radish-index";

/// Bumped whenever the stored document shape or the DDL changes: a serialized
/// `RemoteRelease` is only trusted by the schema version that wrote it, so a
/// mismatch asks for a rebuild instead of decoding a stale shape.
///
/// Version 2 stores `releases.document` as zlib-compressed JSON.
pub const INDEX_SCHEMA_VERSION: i64 = 2;

/// How a long build reports where it is.
///
/// The two phases interleave differently per provider: MusicBrainz stages the
/// whole dump before it indexes anything, while Discogs parses and indexes in
/// one pass, so the two events are separate methods rather than one enum.
/// Builders call these periodically, never once per row.
pub trait BuildProgress: Sync {
    /// Rows staged from one dump table so far.
    fn staging(&self, table: &str, rows: u64);
    /// Releases written to the index so far.
    fn indexing(&self, releases: u64);
}

/// Silences progress in unit tests.
#[cfg(test)]
pub(crate) struct NoProgress;

#[cfg(test)]
impl BuildProgress for NoProgress {
    fn staging(&self, _table: &str, _rows: u64) {}

    fn indexing(&self, _releases: u64) {}
}

/// What a finished build produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BuildReport {
    pub indexed: u64,
    pub skipped: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    MusicBrainz,
    Discogs,
}

impl IndexKind {
    pub const ALL: [Self; 2] = [Self::MusicBrainz, Self::Discogs];

    pub fn provider(self) -> ProviderKind {
        match self {
            Self::MusicBrainz => ProviderKind::MusicBrainz,
            Self::Discogs => ProviderKind::Discogs,
        }
    }

    pub fn for_provider(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::MusicBrainz => Self::MusicBrainz,
            ProviderKind::Discogs => Self::Discogs,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Self::MusicBrainz => "musicbrainz-index.db",
            Self::Discogs => "discogs-index.db",
        }
    }

    /// The `index build` flag that rebuilds this side, used in hints.
    pub fn build_flag(self) -> &'static str {
        match self {
            Self::MusicBrainz => "--musicbrainz",
            Self::Discogs => "--discogs",
        }
    }
}

impl std::fmt::Display for IndexKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MusicBrainz => "MusicBrainz",
            Self::Discogs => "Discogs",
        })
    }
}

/// The directory holding both index files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPaths {
    dir: PathBuf,
}

impl IndexPaths {
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn file(&self, kind: IndexKind) -> PathBuf {
        self.dir.join(kind.file_name())
    }

    /// The in-progress file a build writes before renaming it into place.
    pub fn temporary(&self, kind: IndexKind) -> PathBuf {
        self.dir.join(format!("{}.tmp", kind.file_name()))
    }

    /// The throwaway SQLite file a MusicBrainz build stages dump rows in.
    pub fn staging(&self, kind: IndexKind) -> PathBuf {
        self.dir
            .join(format!("{}-staging.db", kind.provider().as_str()))
    }
}

/// `RADISH_INDEX_DIR` as a directory; unset or blank means the default.
///
/// Split from the environment so the rule is testable without mutating
/// process state.
pub fn index_dir_value(raw: Option<&str>) -> PathBuf {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INDEX_DIR))
}

/// What a build recorded about its source and result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexMeta {
    pub schema_version: i64,
    pub provider: String,
    pub source_file: Option<String>,
    pub source_checksum: Option<String>,
    /// `verified`, `not published`, or absent when a build records nothing.
    pub checksum: Option<String>,
    pub dump_date: Option<String>,
    pub imported_at: Option<String>,
    pub indexed_releases: i64,
    pub skipped_rows: i64,
}

/// The result of looking at an index path without requiring it to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    Missing,
    Present(IndexMeta),
}

impl IndexState {
    pub fn meta(&self) -> Option<&IndexMeta> {
        match self {
            Self::Missing => None,
            Self::Present(meta) => Some(meta),
        }
    }
}

/// Reads the index metadata without judging its version.
///
/// `index status` must be able to report a stale or mismatched index instead of
/// failing, so validation belongs to [`open_read_only`].
pub async fn inspect(paths: &IndexPaths, kind: IndexKind) -> Result<IndexState> {
    let path = paths.file(kind);
    if !path.exists() {
        return Ok(IndexState::Missing);
    }
    let pool = open_pool(&path, kind).await?;
    let rows = sqlx::query("SELECT key, value FROM meta")
        .fetch_all(&pool)
        .await
        .with_context(|| {
            format!(
                "{} is not a Radish index; rebuild it with `radish index build {} PATH`",
                clean_path(&path),
                kind.build_flag()
            )
        })?;
    pool.close().await;

    let mut values = HashMap::new();
    for row in rows {
        values.insert(row.get::<String, _>("key"), row.get::<String, _>("value"));
    }
    Ok(IndexState::Present(meta_from_values(values, kind, &path)?))
}

/// Opens an index for matching, refusing anything this build cannot trust.
///
/// The returned pool is read-only, so a shared index never grows `-wal` or
/// `-shm` files next to it while several runs read it.
pub async fn open_read_only(
    paths: &IndexPaths,
    kind: IndexKind,
) -> Result<(SqlitePool, IndexMeta)> {
    let meta = match inspect(paths, kind).await? {
        IndexState::Missing => bail!(
            "the {} index is missing at {}; build it with `radish index build {} PATH`",
            kind,
            clean_path(&paths.file(kind)),
            kind.build_flag()
        ),
        IndexState::Present(meta) => meta,
    };

    if meta.provider != kind.provider().as_str() {
        bail!(
            "{} holds a {} index, not {}; rebuild it with `radish index build {} PATH`",
            clean_path(&paths.file(kind)),
            meta.provider,
            kind,
            kind.build_flag()
        );
    }
    if meta.schema_version != INDEX_SCHEMA_VERSION {
        bail!(
            "{} was built with index schema {} but this Radish reads {}; rebuild it with `radish index build {} PATH`",
            clean_path(&paths.file(kind)),
            meta.schema_version,
            INDEX_SCHEMA_VERSION,
            kind.build_flag()
        );
    }

    let pool = open_pool(&paths.file(kind), kind).await?;
    Ok((pool, meta))
}

async fn open_pool(path: &Path, kind: IndexKind) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .with_context(|| format!("cannot open the {kind} index at {}", clean_path(path)))
}

fn meta_from_values(
    values: HashMap<String, String>,
    kind: IndexKind,
    path: &Path,
) -> Result<IndexMeta> {
    let schema_version = values
        .get("schema_version")
        .and_then(|value| value.parse::<i64>().ok())
        .with_context(|| {
            format!(
                "{} has no readable index schema version; rebuild it with `radish index build {} PATH`",
                clean_path(path),
                kind.build_flag()
            )
        })?;
    let number = |key: &str| {
        values
            .get(key)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or_default()
    };

    Ok(IndexMeta {
        schema_version,
        provider: values
            .get("provider")
            .cloned()
            .unwrap_or_else(|| kind.provider().as_str().to_string()),
        source_file: values.get("source_file").cloned(),
        source_checksum: values.get("source_checksum").cloned(),
        checksum: values.get("checksum").cloned(),
        dump_date: values.get("dump_date").cloned(),
        imported_at: values.get("imported_at").cloned(),
        indexed_releases: number("indexed_releases"),
        skipped_rows: number("skipped_rows"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_index_directory_falls_back_to_the_default() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(index_dir_value(raw), PathBuf::from(DEFAULT_INDEX_DIR));
        }
        assert_eq!(
            index_dir_value(Some(" /data/indexes ")),
            PathBuf::from("/data/indexes")
        );
    }

    #[test]
    fn index_kinds_name_their_files_and_rebuild_flags() {
        let paths = IndexPaths::from_dir("/data/indexes");
        assert_eq!(
            paths.file(IndexKind::MusicBrainz),
            PathBuf::from("/data/indexes/musicbrainz-index.db")
        );
        assert_eq!(
            paths.file(IndexKind::Discogs),
            PathBuf::from("/data/indexes/discogs-index.db")
        );
        assert_eq!(
            paths.temporary(IndexKind::Discogs),
            PathBuf::from("/data/indexes/discogs-index.db.tmp")
        );
        assert_eq!(
            paths.staging(IndexKind::MusicBrainz),
            PathBuf::from("/data/indexes/musicbrainz-staging.db")
        );
        assert!(IndexKind::MusicBrainz.build_flag().contains("musicbrainz"));
    }
}
