use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// The original, pre-migration schema. It is applied before migrations so that
/// existing databases keep their current shape and fresh databases start from
/// the same baseline. Everything here is additive (`IF NOT EXISTS`).
const LEGACY_SCHEMA: &str = include_str!("schema.sql");

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .foreign_keys(true);

    connect_with_options(options).await
}

/// Opens an in-memory database, used by tests.
#[cfg(test)]
pub async fn connect_in_memory() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?
        .foreign_keys(true)
        .create_if_missing(true);

    connect_with_options(options).await
}

async fn connect_with_options(options: SqliteConnectOptions) -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(if options.get_filename() == Path::new(":memory:") {
            1
        } else {
            8
        })
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
        assert_eq!(applied, 2);
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
