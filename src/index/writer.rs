//! Writing an index file: one schema, batched rows, atomic publication.
//!
//! Both build commands share this writer, so the columns a search reads are
//! always computed by the same code. A build writes `<name>.db.tmp` in the
//! target directory and renames it over the real file only after the FTS index
//! is built, so an interrupted build never damages a working index.

use crate::index::{INDEX_SCHEMA_VERSION, IndexKind, IndexPaths};
use crate::matcher::{
    RemoteRelease, SearchSummary, normalize_barcode, normalize_identifier, normalize_text,
};
use crate::output::clean_path;
use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;

/// The index layout. It is created by a build, never by a migration: the file
/// is a derived artifact and `meta.schema_version` guards it.
const INDEX_SCHEMA: &str = r#"
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE releases (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    title_norm TEXT NOT NULL,
    artist_norm TEXT NOT NULL,
    year INTEGER,
    country TEXT,
    label_norm TEXT NOT NULL,
    release_group_id TEXT,
    document BLOB NOT NULL,
    summary BLOB NOT NULL
);

CREATE TABLE identifiers (
    release_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_identifiers_lookup ON identifiers(kind, value, release_id);
CREATE INDEX idx_identifiers_release ON identifiers(release_id);
CREATE INDEX idx_releases_group ON releases(release_group_id);

CREATE TABLE redirects (
    gid TEXT PRIMARY KEY,
    release_id TEXT NOT NULL
);

CREATE VIRTUAL TABLE releases_fts USING fts5(
    title_norm,
    artist_norm,
    content='releases',
    content_rowid='rowid'
);
"#;

/// Upper bound on one decompressed document. The largest real releases are a
/// few hundred kilobytes; the cap only stops a corrupt blob from ballooning.
const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Serializes a release the way `releases.document` stores it: JSON, then zlib.
///
/// Documents are about 80% of an index and compress roughly 4:1 (measured on
/// the November 2026 dumps), and they are read at most a few times per release
/// being matched, so the decode cost is irrelevant next to the disk it saves.
pub(crate) fn encode_document(release: &RemoteRelease) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(release).context("cannot serialize a release document")?;
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(json.len() / 3), Compression::default());
    encoder
        .write_all(&json)
        .context("cannot compress a release document")?;
    encoder
        .finish()
        .context("cannot compress a release document")
}

/// Reads back what [`encode_document`] wrote.
pub(crate) fn decode_document(bytes: &[u8]) -> Result<RemoteRelease> {
    let mut json = Vec::new();
    ZlibDecoder::new(bytes)
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut json)
        .context("cannot decompress a release document")?;
    anyhow::ensure!(
        json.len() as u64 <= MAX_DOCUMENT_BYTES,
        "a release document exceeds {MAX_DOCUMENT_BYTES} bytes"
    );
    serde_json::from_slice(&json).context("cannot decode a release document")
}

/// One release and the columns that answer searches for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub id: String,
    pub title: String,
    pub title_norm: String,
    pub artist_norm: String,
    pub year: Option<i64>,
    pub country: Option<String>,
    pub label_norm: String,
    pub release_group_id: Option<String>,
    pub document: Vec<u8>,
    pub summary: Vec<u8>,
    /// Normalized `(kind, value)` pairs; `kind` is `barcode` or `catalog`.
    pub identifiers: Vec<(String, String)>,
}

impl IndexRow {
    /// Builds the search columns from the same document the matcher receives.
    ///
    /// Normalization goes through `matcher` so an index lookup asks exactly the
    /// question identifier evidence asks at scoring time.
    pub fn new(release: &RemoteRelease, summary: &SearchSummary) -> Result<Self> {
        let mut identifiers = Vec::new();
        let mut seen = HashSet::new();

        for barcode in &release.barcodes {
            let value = normalize_barcode(barcode);
            if !value.is_empty() && seen.insert(("barcode".to_string(), value.clone())) {
                identifiers.push(("barcode".to_string(), value));
            }
        }
        for catalog in &release.catalog_numbers {
            let value = normalize_identifier(catalog);
            if !value.is_empty() && seen.insert(("catalog".to_string(), value.clone())) {
                identifiers.push(("catalog".to_string(), value));
            }
        }

        Ok(Self {
            id: release.id.storage_id(),
            title: release.title.clone(),
            title_norm: normalize_text(&release.title),
            artist_norm: normalize_text(&release.artist_display()),
            year: release.year,
            country: release.country.clone(),
            label_norm: normalize_text(&release.labels.join(" ")),
            release_group_id: release.group_id.as_ref().map(|group| group.storage_id()),
            document: encode_document(release)?,
            summary: serde_json::to_vec(summary).context("cannot serialize a search summary")?,
            identifiers,
        })
    }
}

/// A release's stored summary, as a search page returns it.
pub fn summary_for(release: &RemoteRelease) -> SearchSummary {
    SearchSummary {
        id: release.id.clone(),
        title: release.title.clone(),
        artists: release.artists.clone(),
        artist_credit: release.artist_credit.clone(),
        year: release.year,
        country: release.country.clone(),
        labels: release.labels.clone(),
        catalog_numbers: release.catalog_numbers.clone(),
        formats: release.formats.clone(),
        barcodes: release.barcodes.clone(),
        result_type: "release".to_string(),
    }
}

pub struct IndexWriter {
    pool: SqlitePool,
    temporary: PathBuf,
    target: PathBuf,
}

impl IndexWriter {
    /// Starts a build, replacing any leftover temporary file.
    pub async fn create(paths: &IndexPaths, kind: IndexKind) -> Result<Self> {
        std::fs::create_dir_all(paths.dir()).with_context(|| {
            format!(
                "cannot create the index directory {}",
                clean_path(paths.dir())
            )
        })?;

        let temporary = paths.temporary(kind);
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot replace {}", clean_path(&temporary)));
            }
        }

        let options = SqliteConnectOptions::new()
            .filename(&temporary)
            .create_if_missing(true)
            // The file is derived and unpublished until the rename, so there is
            // nothing worth journalling or syncing during the build.
            .journal_mode(SqliteJournalMode::Off)
            .synchronous(SqliteSynchronous::Off)
            .pragma("temp_store", "FILE")
            .pragma("cache_size", "-131072");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("cannot create {}", clean_path(&temporary)))?;

        sqlx::raw_sql(INDEX_SCHEMA)
            .execute(&pool)
            .await
            .with_context(|| format!("cannot initialize {}", clean_path(&temporary)))?;

        let writer = Self {
            pool,
            temporary,
            target: paths.file(kind),
        };
        writer
            .set_meta("schema_version", &INDEX_SCHEMA_VERSION.to_string())
            .await?;
        writer
            .set_meta("provider", kind.provider().as_str())
            .await?;
        Ok(writer)
    }

    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .with_context(|| format!("cannot record index metadata {key}"))?;
        Ok(())
    }

    pub async fn insert(&self, row: &IndexRow) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO releases(
                id, title, title_norm, artist_norm, year, country, label_norm,
                release_group_id, document, summary
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.title)
        .bind(&row.title_norm)
        .bind(&row.artist_norm)
        .bind(row.year)
        .bind(row.country.as_deref())
        .bind(&row.label_norm)
        .bind(row.release_group_id.as_deref())
        .bind(row.document.as_slice())
        .bind(row.summary.as_slice())
        .execute(&self.pool)
        .await
        .with_context(|| format!("cannot store release {}", crate::output::Clean(&row.id)))?;

        for (kind, value) in &row.identifiers {
            sqlx::query(
                "INSERT OR IGNORE INTO identifiers(release_id, kind, value) VALUES (?, ?, ?)",
            )
            .bind(&row.id)
            .bind(kind)
            .bind(value)
            .execute(&self.pool)
            .await
            .with_context(|| {
                format!(
                    "cannot store identifiers for {}",
                    crate::output::Clean(&row.id)
                )
            })?;
        }

        Ok(())
    }

    /// Records a merged MusicBrainz ID so a stale MBID still resolves.
    pub async fn insert_redirect(&self, gid: &str, release_id: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO redirects(gid, release_id) VALUES (?, ?)")
            .bind(gid)
            .bind(release_id)
            .execute(&self.pool)
            .await
            .context("cannot store redirects")?;
        Ok(())
    }

    /// Finishes the search index and publishes the file in one rename.
    pub async fn finish(self) -> Result<()> {
        sqlx::raw_sql("INSERT INTO releases_fts(releases_fts) VALUES('rebuild');")
            .execute(&self.pool)
            .await
            .context("cannot build the search index")?;
        sqlx::raw_sql("ANALYZE;")
            .execute(&self.pool)
            .await
            .context("cannot analyze the index")?;
        self.pool.close().await;

        std::fs::rename(&self.temporary, &self.target)
            .with_context(|| format!("cannot publish {}", clean_path(&self.target)))?;
        Ok(())
    }
}

impl Drop for IndexWriter {
    fn drop(&mut self) {
        // A successful `finish` renamed the file, so this only removes an
        // abandoned build.
        let _ = std::fs::remove_file(&self.temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{INDEX_SCHEMA_VERSION, IndexState, inspect, open_read_only};
    use crate::matcher::{RemoteTrack, TrackEntryType};
    use crate::providers::{GroupId, Mbid, ReleaseId};
    use crate::test_support::ScratchDir;

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    fn release() -> RemoteRelease {
        RemoteRelease {
            id: ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd")),
            group_id: Some(GroupId::MusicBrainzReleaseGroup(mbid(
                "4642ee19-7790-3c8d-ab5e-d133de942db6",
            ))),
            title: "Sigur Rós (Deluxe)".to_string(),
            artists: vec!["Sigur Rós".to_string()],
            year: Some(2001),
            country: Some("Iceland".to_string()),
            labels: vec!["Label".to_string()],
            catalog_numbers: vec![" CAT-1 ".to_string()],
            formats: vec!["CD".to_string()],
            barcodes: vec!["0123456789012".to_string()],
            tracklist: vec![RemoteTrack {
                position: Some("1".to_string()),
                title: "One".to_string(),
                artists: vec!["Sigur Rós".to_string()],
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: Some(1),
                track_position: Some(1),
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            }],
            url: "https://musicbrainz.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd".to_string(),
            lookup_aliases: Vec::new(),
            artist_credit: Some("Sigur Rós".to_string()),
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn documents_round_trip_and_refuse_damaged_or_oversized_input() {
        let release = release();
        let bytes = encode_document(&release).unwrap();
        assert_eq!(decode_document(&bytes).unwrap(), release);

        assert!(decode_document(b"").is_err());
        assert!(decode_document(b"\x00").is_err());
        assert!(
            decode_document(&bytes[..bytes.len() / 2]).is_err(),
            "truncated"
        );

        // A zlib stream that inflates past the cap is refused, not buffered.
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        let zeros = vec![0_u8; 1024 * 1024];
        for _ in 0..=(MAX_DOCUMENT_BYTES / zeros.len() as u64) {
            encoder.write_all(&zeros).unwrap();
        }
        let bomb = encoder.finish().unwrap();
        let error = decode_document(&bomb).unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");
    }

    #[test]
    fn search_columns_are_normalized_the_way_evidence_is() {
        let release = release();
        let row = IndexRow::new(&release, &summary_for(&release)).unwrap();
        assert_eq!(row.id, "76df3287-6cda-33eb-8e9a-044b5e15ffdd");
        assert_eq!(row.title, "Sigur Rós (Deluxe)");
        assert_eq!(row.title_norm, "sigur ros deluxe");
        assert_eq!(row.artist_norm, "sigur ros");
        assert_eq!(row.label_norm, "label");
        assert_eq!(
            row.release_group_id.as_deref(),
            Some("4642ee19-7790-3c8d-ab5e-d133de942db6")
        );
        assert_eq!(
            row.identifiers,
            vec![
                ("barcode".to_string(), "0123456789012".to_string()),
                ("catalog".to_string(), "cat1".to_string()),
            ]
        );
        let stored = decode_document(&row.document).unwrap();
        assert_eq!(stored, release);
        assert!(
            row.document.len() < serde_json::to_vec(&release).unwrap().len(),
            "documents are stored compressed"
        );
        let summary: SearchSummary = serde_json::from_slice(&row.summary).unwrap();
        assert_eq!(summary.result_type, "release");
    }

    #[tokio::test]
    async fn a_finished_build_publishes_one_file_with_metadata() {
        let dir = ScratchDir::new("index-writer");
        let paths = IndexPaths::from_dir(dir.path().join("indexes"));
        let writer = IndexWriter::create(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        let release = release();
        let row = IndexRow::new(&release, &summary_for(&release)).unwrap();
        writer.insert(&row).await.unwrap();
        writer.insert(&row).await.unwrap();
        writer
            .insert_redirect("11111111-1111-1111-1111-111111111111", &row.id)
            .await
            .unwrap();
        writer.set_meta("indexed_releases", "1").await.unwrap();
        writer.finish().await.unwrap();

        assert!(paths.file(IndexKind::MusicBrainz).exists());
        assert!(!paths.temporary(IndexKind::MusicBrainz).exists());

        let (pool, meta) = open_read_only(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        assert_eq!(meta.schema_version, INDEX_SCHEMA_VERSION);
        assert_eq!(meta.provider, "musicbrainz");
        assert_eq!(meta.indexed_releases, 1);

        let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM releases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(releases, 1);
        let identifiers: i64 = sqlx::query_scalar("SELECT count(*) FROM identifiers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identifiers, 2);
        let ranked: Vec<String> =
            sqlx::query_scalar("SELECT title FROM releases WHERE id IN (SELECT id FROM releases)")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ranked.len(), 1);
        let fts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM releases_fts WHERE releases_fts MATCH 'title_norm : (deluxe)'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn a_missing_index_reports_the_build_command() {
        let dir = ScratchDir::new("index-missing");
        let paths = IndexPaths::from_dir(dir.path().join("indexes"));
        assert_eq!(
            inspect(&paths, IndexKind::Discogs).await.unwrap(),
            IndexState::Missing
        );
        let error = open_read_only(&paths, IndexKind::Discogs)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("discogs-index.db"), "{error}");
        assert!(error.contains("radish index build --discogs"), "{error}");
    }

    #[tokio::test]
    async fn an_index_from_another_schema_version_is_refused() {
        let dir = ScratchDir::new("index-version");
        let paths = IndexPaths::from_dir(dir.path());
        let writer = IndexWriter::create(&paths, IndexKind::Discogs)
            .await
            .unwrap();
        writer.set_meta("schema_version", "999").await.unwrap();
        writer.finish().await.unwrap();

        match inspect(&paths, IndexKind::Discogs).await.unwrap() {
            IndexState::Present(meta) => assert_eq!(meta.schema_version, 999),
            IndexState::Missing => panic!("the index exists"),
        }
        let error = open_read_only(&paths, IndexKind::Discogs)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema 999"), "{error}");
        assert!(error.contains("rebuild"), "{error}");
    }
}
