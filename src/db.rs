use anyhow::Result;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::Path;
#[cfg(test)]
use std::str::FromStr;
use std::time::Duration;

/// The original, pre-migration schema. It is applied before migrations so that
/// existing databases keep their current shape and fresh databases start from
/// the same baseline. Everything here is additive (`IF NOT EXISTS`).
const LEGACY_SCHEMA: &str = include_str!("schema.sql");

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// How long a connection waits for another writer's lock before giving up.
///
/// Matching holds no write lock while it talks to providers, so contention is
/// limited to short import, rebuild and accept transactions. Fifteen seconds
/// is long enough for a large import batch to finish and short enough that a
/// wedged process is reported instead of hanging forever.
const BUSY_TIMEOUT: Duration = Duration::from_secs(15);

/// Connections for file databases. Concurrent readers and one writer is the
/// usual shape: the watcher, an import and a match run can overlap.
const FILE_POOL_CONNECTIONS: u32 = 8;

/// Opens the library database at `db_path`, creating it when missing.
///
/// The path is passed as a file name, never spliced into a URL, so characters
/// such as `?`, `#` and `%` are safe. File databases use WAL with
/// `synchronous = NORMAL`, so readers do not block the writer.
pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new()
        .max_connections(FILE_POOL_CONNECTIONS)
        .connect_with(options)
        .await?;
    initialize(&pool).await?;

    Ok(pool)
}

/// Opens an in-memory database, used by tests.
///
/// Every connection to a private in-memory database is a separate database, so
/// the pool holds exactly one connection and never recycles it. WAL does not
/// apply to memory databases, and shared-cache locking reports `SQLITE_LOCKED`,
/// which `busy_timeout` does not wait on.
#[cfg(test)]
pub async fn connect_in_memory() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?
        .foreign_keys(true)
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await?;
    initialize(&pool).await?;

    Ok(pool)
}

/// Brings a database up to the current schema: first the legacy baseline, then
/// versioned migrations. Never scans files or touches the network, so stored
/// metadata stays absent until the next rescan.
pub async fn initialize(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(LEGACY_SCHEMA).execute(pool).await?;
    MIGRATOR.run(pool).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn legacy_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::raw_sql(LEGACY_SCHEMA).execute(&pool).await.unwrap();
        pool
    }

    /// A database that already has the Discogs migration applied and rows in
    /// it. The new migration is applied directly, so the seed never passes
    /// through a schema that already contains it.
    async fn populated_discogs_pool() -> SqlitePool {
        let pool = legacy_pool().await;
        sqlx::raw_sql(include_str!(
            "../migrations/20260101000000_discogs_matching.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO directories (id, path) VALUES (1, '/music');
             INSERT INTO albums (id, title, artist, year) VALUES (1, 'Blue', 'Artist', 2001);
             INSERT INTO local_releases (
                id, directory_path, album_title_norm, album_title, album_artist, artist_kind,
                year, barcode, catalog_number, embedded_discogs_id, is_compilation,
                has_album_tag, track_count, source_revision, status
             ) VALUES (
                1, '/music/blue', 'blue', 'Blue', 'Artist', 'album_artist',
                2001, '0123456789012', 'CAT-1', 42, 0, 1, 1, 'old-revision', 'matched'
             );
             INSERT INTO tracks (
                id, path, title, artist, album_id, directory_id, track_number, duration_secs,
                modified_at, local_release_id, album_artist, disc_number, barcode,
                catalog_number, discogs_release_id
             ) VALUES (
                10, '/music/blue/01.flac', 'Local One', 'Artist', 1, 1, 1, 180.0,
                1, 1, 'Artist', 1, '0123456789012', 'CAT-1', 42
             );
             INSERT INTO discogs_matches (
                id, local_release_id, discogs_release_id, discogs_master_id, release_url,
                title, artist, year, country, label, catalog_number, format, barcode, score,
                algorithm_version, acceptance_method, accepted_at, source_revision, stale,
                metadata_json
             ) VALUES (
                7, 1, 42, 9, 'https://www.discogs.com/release/42', 'Blue', 'Artist', 2001,
                'US', 'Label', 'CAT-1', 'CD', '0123456789012', 0.98,
                'discogs-match-v1', 'manual', '2026-01-02T03:04:05Z', 'old-revision', 0,
                '{\"id\":42,\"master_id\":9,\"title\":\"Blue\"}'
             );
             INSERT INTO discogs_track_matches (
                id, local_release_id, track_id, remote_ordinal, remote_position,
                remote_title, remote_artist, remote_duration_secs
             ) VALUES (3, 1, 10, 1, '1', 'Remote One', 'Artist', 180.0);
             INSERT INTO match_reviews (
                id, local_release_id, candidates_json, proposed_mapping_json, reasons_json,
                source_revision, created_at, updated_at
             ) VALUES (
                5, 1, '[]', '{\"pairs\":[null],\"complete\":false,\"ambiguous\":false,\"notes\":[]}',
                '[\"review me\"]', 'old-revision', '2026-01-03T00:00:00Z', '2026-01-04T00:00:00Z'
             );",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// A scratch directory that is removed when dropped, including when the
    /// test panics. It also removes the `-wal` and `-shm` side files.
    struct ScratchDir(std::path::PathBuf);

    impl std::ops::Deref for ScratchDir {
        type Target = std::path::Path;

        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for ScratchDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A unique scratch directory for file-backed databases.
    fn scratch_dir(label: &str) -> ScratchDir {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radish-db-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ScratchDir(dir)
    }

    // `?` is not a valid character in Windows file names.
    #[cfg(unix)]
    #[tokio::test]
    async fn connect_opens_paths_with_url_metacharacters() {
        let dir = scratch_dir("meta");
        let path = dir.join("lib ?#%.db");

        let pool = connect(&path).await.unwrap();
        sqlx::query("INSERT INTO directories (path) VALUES ('/music')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        assert!(
            path.exists(),
            "the database must be created at the exact path"
        );
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(
            entries.iter().all(|entry| entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("lib ?#%.db")),
            "no stray database files expected: {entries:?}"
        );
    }

    #[tokio::test]
    async fn file_databases_use_wal_and_normal_sync() {
        let dir = scratch_dir("wal");
        let pool = connect(&dir.join("library.db")).await.unwrap();

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(synchronous, 1, "NORMAL");

        pool.close().await;
    }

    #[tokio::test]
    async fn in_memory_pools_hold_a_single_connection() {
        let pool = connect_in_memory().await.unwrap();
        assert_eq!(pool.options().get_max_connections(), 1);
        assert_eq!(pool.options().get_idle_timeout(), None);
        assert_eq!(pool.options().get_max_lifetime(), None);
    }

    #[tokio::test]
    async fn creates_a_fresh_database_with_every_table() {
        let pool = connect_in_memory().await.unwrap();

        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();

        for expected in [
            "albums",
            "directories",
            "local_releases",
            "match_reviews",
            "release_matches",
            "release_track_matches",
            "tracks",
        ] {
            assert!(
                tables.iter().any(|table| table == expected),
                "{expected} missing from {tables:?}"
            );
        }
        // The retired Discogs-specific tables are gone.
        assert!(!tables.iter().any(|table| table == "discogs_matches"));
        assert!(!tables.iter().any(|table| table == "discogs_track_matches"));

        // Running initialization again is a no-op.
        initialize(&pool).await.unwrap();
        let applied: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, 8);
    }

    /// A database migrated up to and including the album-matching migration.
    async fn pool_before_store_integrity() -> SqlitePool {
        let pool = legacy_pool().await;
        for migration in [
            include_str!("../migrations/20260101000000_discogs_matching.sql"),
            include_str!("../migrations/20261111000000_musicbrainz_matching.sql"),
            include_str!("../migrations/20261111000001_album_matching.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn migration_marks_misordered_matches_stale() {
        let pool = pool_before_store_integrity().await;
        let mut seed = String::from(
            "INSERT INTO directories (id, path) VALUES (1, '/m');
             INSERT INTO albums (id, title, artist, year) VALUES (1, 'Blue', 'Artist', 2001);",
        );
        // Release 1: path order differs from track order. Release 2: they agree.
        // Release 3: a single track.
        for (release, dir, tracks) in [
            (1, "one", vec![(10, "A.flac", 2), (11, "B.flac", 1)]),
            (2, "two", vec![(20, "A.flac", 1), (21, "B.flac", 2)]),
            (3, "three", vec![(30, "A.flac", 1)]),
        ] {
            seed.push_str(&format!(
                "INSERT INTO local_releases (id, directory_path, album_title_norm, source_revision, status, track_count)
                 VALUES ({release}, '/m/{dir}', 'blue', 'rev', 'matched', {});
                 INSERT INTO release_matches (id, local_release_id, provider, remote_release_id, release_url, score,
                    algorithm_version, acceptance_method, source_revision, snapshot_version, metadata_json)
                 VALUES ({release}, {release}, 'discogs', '42', 'https://example.test', 1.0, 'v', 'manual', 'rev', 2, '{{}}');",
                tracks.len()
            ));
            for (id, file, number) in tracks {
                seed.push_str(&format!(
                    "INSERT INTO tracks (id, path, album_id, directory_id, track_number, modified_at, local_release_id)
                     VALUES ({id}, '/m/{dir}/{file}', 1, 1, {number}, 1, {release});
                     INSERT INTO release_track_matches (match_id, track_id, remote_ordinal, remote_title)
                     VALUES ({release}, {id}, {number}, 'Remote {number}');"
                ));
            }
        }
        // An overlay row whose track no longer belongs to the match's release.
        seed.push_str(
            "INSERT INTO tracks (id, path, album_id, directory_id, track_number, modified_at, local_release_id)
             VALUES (40, '/m/elsewhere/A.flac', 1, 1, 1, 1, NULL);
             INSERT INTO release_track_matches (match_id, track_id, remote_ordinal, remote_title)
             VALUES (3, 40, 9, 'Orphan');",
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(seed))
            .execute(&pool)
            .await
            .unwrap();

        sqlx::raw_sql(include_str!(
            "../migrations/20261112000000_store_integrity.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let rows: Vec<(i64, String, bool)> = sqlx::query_as(
            "SELECT lr.id, lr.status, m.stale
             FROM local_releases lr JOIN release_matches m ON m.local_release_id = lr.id
             ORDER BY lr.id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "stale".to_string(), true),
                (2, "matched".to_string(), false),
                (3, "matched".to_string(), false),
            ]
        );

        let orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM release_track_matches WHERE remote_title = 'Orphan'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(orphans, 0, "overlay rows for foreign tracks are removed");
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 5);
    }

    #[tokio::test]
    async fn migration_moves_year_to_tracks_and_dedupes_albums() {
        let pool = pool_before_store_integrity().await;
        sqlx::raw_sql(
            "INSERT INTO directories (id, path) VALUES (1, '/m');
             INSERT INTO albums (id, title, artist, year) VALUES (1, 'Blue', NULL, 2001);
             INSERT INTO albums (id, title, artist, year) VALUES (2, 'Blue', NULL, 2002);
             INSERT INTO albums (id, title, artist, year) VALUES (3, 'Blue', '', 1999);
             INSERT INTO albums (id, title, artist, year) VALUES (4, 'Blue', 'Artist', 2010);
             INSERT INTO tracks (id, path, album_id, directory_id, modified_at) VALUES (10, '/m/1.flac', 1, 1, 1);
             INSERT INTO tracks (id, path, album_id, directory_id, modified_at) VALUES (11, '/m/2.flac', 2, 1, 1);
             INSERT INTO tracks (id, path, album_id, directory_id, modified_at) VALUES (12, '/m/3.flac', 3, 1, 1);
             INSERT INTO tracks (id, path, album_id, directory_id, modified_at) VALUES (13, '/m/4.flac', 4, 1, 1);
             INSERT INTO tracks (id, path, album_id, directory_id, modified_at) VALUES (14, '/m/5.flac', NULL, 1, 1);",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../migrations/20261112000001_track_year_album_identity.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let tracks: Vec<(i64, Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT id, album_id, year FROM tracks ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            tracks,
            vec![
                (10, Some(1), Some(2001)),
                (11, Some(1), Some(2002)),
                (12, Some(1), Some(1999)),
                (13, Some(4), Some(2010)),
                (14, None, None),
            ]
        );
        let albums: Vec<(i64, Option<i64>)> =
            sqlx::query_as("SELECT id, year FROM albums ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(albums, vec![(1, None), (4, None)]);

        let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(violations, 0);
        let index: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_albums_identity'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(index, 1);
    }

    #[tokio::test]
    async fn migrates_a_populated_legacy_database() {
        let pool = legacy_pool().await;
        sqlx::raw_sql(
            "INSERT INTO directories (id, path) VALUES (1, '/music');
             INSERT INTO albums (id, title, artist, year) VALUES (1, 'Blue', 'Artist', 2001);
             INSERT INTO albums (id, title, artist, year) VALUES (2, 'Untitled', NULL, NULL);
             INSERT INTO tracks (id, path, title, artist, album_id, directory_id, track_number, duration_secs, modified_at)
                VALUES (10, '/music/01.flac', 'One', 'Artist', 1, 1, 1, 180.0, 1);
             INSERT INTO tracks (id, path, title, artist, album_id, directory_id, track_number, duration_secs, modified_at)
                VALUES (11, '/music/02.flac', NULL, NULL, 2, 1, 2, 190.0, 2);",
        )
        .execute(&pool)
        .await
        .unwrap();

        initialize(&pool).await.unwrap();

        // Existing track IDs and scanned metadata are preserved.
        let row: (i64, String, Option<String>) =
            sqlx::query_as("SELECT id, path, title FROM tracks WHERE id = 11")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, 11);
        assert_eq!(row.1, "/music/02.flac");
        assert_eq!(row.2, None);

        // New observed fields stay absent until a rescan; migration must not
        // scan files or reach the network.
        let album_artist: Option<String> =
            sqlx::query_scalar("SELECT album_artist FROM tracks WHERE id = 10")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(album_artist, None);
        let musicbrainz: String =
            sqlx::query_scalar("SELECT musicbrainz_ids_json FROM tracks WHERE id = 10")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            musicbrainz,
            r#"{"release":[],"release_group":[],"release_track":[],"recording":[]}"#
        );

        let releases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM local_releases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(releases, 0);

        // Legacy album rows remain for scanned-metadata fallback.
        let album: Option<String> = sqlx::query_scalar(
            "SELECT a.title FROM tracks t LEFT JOIN albums a ON a.id = t.album_id WHERE t.id = 10",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(album.as_deref(), Some("Blue"));
    }

    #[tokio::test]
    async fn upgrades_a_populated_discogs_database_without_losing_metadata() {
        let pool = populated_discogs_pool().await;
        sqlx::raw_sql(include_str!(
            "../migrations/20261111000000_musicbrainz_matching.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261111000001_album_matching.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let scope: String = sqlx::query_scalar("SELECT match_scope FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(scope, "edition");

        let match_row: (i64, i64, String, String, Option<String>, String, bool, i64) =
            sqlx::query_as(
                "SELECT id, local_release_id, provider, remote_release_id, remote_group_id,
                        algorithm_version, stale, snapshot_version
                 FROM release_matches",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(match_row.0, 7, "accepted row IDs are preserved");
        assert_eq!(match_row.1, 1);
        assert_eq!(match_row.2, "discogs");
        assert_eq!(match_row.3, "42");
        assert_eq!(match_row.4.as_deref(), Some("9"));
        assert_eq!(match_row.5, "discogs-match-v1");
        assert!(!match_row.6);
        assert_eq!(match_row.7, 1);

        let metadata: String = sqlx::query_scalar("SELECT metadata_json FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(metadata, r#"{"id":42,"master_id":9,"title":"Blue"}"#);
        let accepted_at: String = sqlx::query_scalar("SELECT accepted_at FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(accepted_at, "2026-01-02T03:04:05Z");

        let track_row: (i64, i64, i64, Option<String>) = sqlx::query_as(
            "SELECT match_id, track_id, remote_ordinal, remote_title FROM release_track_matches",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(track_row, (7, 10, 1, Some("Remote One".to_string())));

        let review: (String, i64, Option<i64>, String, String) = sqlx::query_as(
            "SELECT provider, snapshot_version, expected_match_id, reasons_json, created_at
             FROM match_reviews",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(review.0, "discogs");
        assert_eq!(review.1, 1);
        assert_eq!(review.2, Some(7));
        assert_eq!(review.3, r#"["review me"]"#);
        assert_eq!(review.4, "2026-01-03T00:00:00Z");

        // Effective metadata still resolves for list/search.
        let title: Option<String> = sqlx::query_scalar(
            "SELECT tm.remote_title FROM release_track_matches tm WHERE tm.track_id = 10",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(title.as_deref(), Some("Remote One"));

        let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(violations, 0);

        let retired: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('discogs_matches', 'discogs_track_matches')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retired, 0);
    }

    #[tokio::test]
    async fn existing_matches_read_no_medium_and_a_medium_must_be_positive() {
        let pool = populated_discogs_pool().await;
        for migration in [
            include_str!("../migrations/20261111000000_musicbrainz_matching.sql"),
            include_str!("../migrations/20261111000001_album_matching.sql"),
            include_str!("../migrations/20261114000000_medium_matches.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        let medium: Option<i64> = sqlx::query_scalar("SELECT medium_position FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(medium, None);

        sqlx::query("UPDATE release_matches SET medium_position = 2")
            .execute(&pool)
            .await
            .unwrap();
        for invalid in [0, -1] {
            let error = sqlx::query("UPDATE release_matches SET medium_position = ?1")
                .bind(invalid)
                .execute(&pool)
                .await;
            assert!(error.is_err(), "medium_position {invalid} was accepted");
        }
    }

    #[tokio::test]
    async fn a_deleted_match_finds_the_reviews_that_expect_it_by_index() {
        // Deleting a release_matches row sets match_reviews.expected_match_id
        // to NULL wherever it names that match. Without an index SQLite scans
        // every review (and its candidate JSON) once per deleted match.
        let pool = connect_in_memory().await.unwrap();
        let plan: Vec<String> = sqlx::query(
            "EXPLAIN QUERY PLAN SELECT 1 FROM match_reviews WHERE expected_match_id = 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| sqlx::Row::get::<String, _>(row, "detail"))
        .collect();
        assert!(!plan.is_empty());
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN")),
            "{plan:?}"
        );
    }

    #[tokio::test]
    async fn cascade_deletion_removes_overlays_mappings_and_reviews() {
        let pool = populated_discogs_pool().await;
        sqlx::raw_sql(include_str!(
            "../migrations/20261111000000_musicbrainz_matching.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM local_releases WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mappings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        let reviews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM match_reviews")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 0, "release_matches rows were not cascaded");
        assert_eq!(mappings, 0, "release_track_matches rows were not cascaded");
        assert_eq!(reviews, 0, "match_reviews rows were not cascaded");
    }
}
