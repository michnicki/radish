use crate::scanner::ScannedTrack;
use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;

pub async fn upsert_directory(pool: &SqlitePool, path: &Path) -> Result<i64> {
    let path_str = path.to_string_lossy().to_string();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO directories (path) VALUES (?1)
         ON CONFLICT(path) DO UPDATE SET path = excluded.path
         RETURNING id",
    )
    .bind(&path_str)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn upsert_album(
    pool: &SqlitePool,
    title: &str,
    artist: Option<&str>,
    year: Option<i64>,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO albums (title, artist, year) VALUES (?1, ?2, ?3)
         ON CONFLICT(title, artist) DO UPDATE SET year = excluded.year
         RETURNING id",
    )
    .bind(title)
    .bind(artist)
    .bind(year)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn upsert_track(pool: &SqlitePool, track: &ScannedTrack, directory_id: i64) -> Result<()> {
    let album_id = match &track.album {
        Some(title) if !title.is_empty() => {
            Some(upsert_album(pool, title, track.artist.as_deref(), track.year).await?)
        }
        _ => None,
    };

    let path_str = track.path.to_string_lossy().to_string();
    sqlx::query(
        "INSERT INTO tracks (path, title, artist, album_id, directory_id, track_number, duration_secs, modified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            artist = excluded.artist,
            album_id = excluded.album_id,
            directory_id = excluded.directory_id,
            track_number = excluded.track_number,
            duration_secs = excluded.duration_secs,
            modified_at = excluded.modified_at",
    )
    .bind(&path_str)
    .bind(&track.title)
    .bind(&track.artist)
    .bind(album_id)
    .bind(directory_id)
    .bind(track.track_number)
    .bind(track.duration_secs)
    .bind(track.modified_at)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn remove_track_by_path(pool: &SqlitePool, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    sqlx::query("DELETE FROM tracks WHERE path = ?1 COLLATE NOCASE")
        .bind(path_str)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
pub struct TrackRow {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<i64>,
    pub duration_secs: Option<f64>,
}

pub async fn list_tracks(pool: &SqlitePool) -> Result<Vec<TrackRow>> {
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path, t.title AS title, t.artist AS artist,
                a.title AS album, t.track_number AS track_number, t.duration_secs AS duration_secs
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         ORDER BY a.title, t.track_number, t.title",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn search_tracks(pool: &SqlitePool, query: &str) -> Result<Vec<TrackRow>> {
    let pattern = format!("%{query}%");
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path, t.title AS title, t.artist AS artist,
                a.title AS album, t.track_number AS track_number, t.duration_secs AS duration_secs
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         WHERE t.title LIKE ?1 COLLATE NOCASE
            OR t.artist LIKE ?1 COLLATE NOCASE
            OR a.title LIKE ?1 COLLATE NOCASE
         ORDER BY a.title, t.track_number, t.title",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
