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
            "discogs_matches",
            "discogs_track_matches",
            "local_releases",
            "match_reviews",
            "tracks",
        ] {
            assert!(
                tables.iter().any(|table| table == expected),
                "{expected} missing from {tables:?}"
            );
        }

        // Running initialization again is a no-op.
        initialize(&pool).await.unwrap();
        let applied: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, 1);
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
}
