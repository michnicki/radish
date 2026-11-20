//! Effective library metadata for the Subsonic API.
//!
//! Releases are albums and album artists are artists. Every value is the
//! effective one that `list` and `search` show: the accepted match where a
//! release has one, the scanned tags otherwise. Artists have no table. An
//! artist is the value of `release_artist!` that its releases share, compared
//! `COLLATE NOCASE`, which folds ASCII letters only.

use super::{like_contains, shown_tag};
use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;

/// The name a release without any artist is listed under. `release_artist!`
/// spells the same text.
pub const UNKNOWN_ARTIST: &str = "[Unknown Artist]";

/// The artist a release is listed under: the accepted match's, else the
/// release's own. A blank value counts as missing.
macro_rules! release_artist {
    () => {
        "COALESCE(NULLIF(TRIM(COALESCE(m.artist, lr.album_artist)), ''), '[Unknown Artist]')"
    };
}

/// A release's effective title.
macro_rules! release_name {
    () => {
        "COALESCE(m.title, lr.album_title)"
    };
}

/// A release's effective year.
macro_rules! release_year {
    () => {
        "COALESCE(m.year, lr.year)"
    };
}

/// The columns of [`LibraryRelease`], from `local_releases lr` and its
/// accepted match `m`. Ends with a space, ready for a `WHERE` or `ORDER BY`.
macro_rules! release_select {
    () => {
        concat!(
            "SELECT lr.id AS id, ",
            release_name!(),
            " AS name, ",
            release_artist!(),
            " AS artist, ",
            release_year!(),
            " AS year,
                    lr.track_count AS track_count,
                    (SELECT TOTAL(duration_secs) FROM tracks
                     WHERE local_release_id = lr.id) AS duration_secs,
                    lr.created_at AS created_at,
                    lr.directory_path AS directory,
                    CASE WHEN m.provider = 'musicbrainz' THEN m.remote_release_id END
                        AS musicbrainz_id
             FROM local_releases lr
             LEFT JOIN release_matches m ON m.local_release_id = lr.id "
        )
    };
}

/// A local release as the API shows it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LibraryRelease {
    pub id: i64,
    /// The effective title, `None` when the release has none.
    pub name: Option<String>,
    /// The artist key: the value of `release_artist!`. SQL `TRIM` removes
    /// spaces only, so the key may keep a tab or a no-break space; see
    /// [`display_artist`].
    pub artist: String,
    pub year: Option<i64>,
    pub track_count: i64,
    pub duration_secs: f64,
    /// When the release was first stored, as `YYYY-MM-DDTHH:MM:SSZ`.
    pub created_at: String,
    /// The release's folder.
    pub directory: String,
    /// The release MBID of an accepted MusicBrainz match.
    pub musicbrainz_id: Option<String>,
}

impl LibraryRelease {
    /// The title, or the folder name when the release has none.
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| folder_name(&self.directory))
    }

    /// The artist as shown; see [`display_artist`].
    pub fn display_artist(&self) -> &str {
        display_artist(&self.artist)
    }
}

/// An artist key as shown: without surrounding whitespace of any kind.
pub fn display_artist(key: &str) -> &str {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        UNKNOWN_ARTIST
    } else {
        trimmed
    }
}

/// The last component of `directory`, or the whole path when it has none.
fn folder_name(directory: &str) -> String {
    Path::new(directory).file_name().map_or_else(
        || directory.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// An album artist and its number of releases.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LibraryArtist {
    /// The artist key of one of its releases (the smallest in binary order).
    pub artist: String,
    pub album_count: i64,
}

/// How a page of releases is ordered. Every order ends with the release ID,
/// so pages never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOrder {
    /// Oldest stored first: stable while the library grows.
    Id,
    /// Most recently stored first.
    Newest,
    /// By title, ignoring ASCII case.
    Name,
    /// By artist, then year and title.
    Artist,
    /// A fresh shuffle on every call; the offset means nothing.
    Random,
    /// Released from `from` to `to`, oldest first, or newest first when
    /// `from > to`.
    Years { from: i64, to: i64 },
}

fn clean_release(mut release: LibraryRelease) -> LibraryRelease {
    release.name = shown_tag(release.name);
    release
}

/// One release, `None` when there is no release `id`.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn library_release(pool: &SqlitePool, id: i64) -> Result<Option<LibraryRelease>> {
    let row = sqlx::query_as::<_, LibraryRelease>(concat!(release_select!(), "WHERE lr.id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(clean_release))
}

/// A page of releases in `order`. A negative `limit` means no limit.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn library_releases(
    pool: &SqlitePool,
    order: ReleaseOrder,
    limit: i64,
    offset: i64,
) -> Result<Vec<LibraryRelease>> {
    const BY_ID: &str = concat!(release_select!(), "ORDER BY lr.id LIMIT ?1 OFFSET ?2");
    const NEWEST: &str = concat!(
        release_select!(),
        "ORDER BY lr.created_at DESC, lr.id DESC LIMIT ?1 OFFSET ?2"
    );
    const BY_NAME: &str = concat!(
        release_select!(),
        "ORDER BY ",
        release_name!(),
        " COLLATE NOCASE, lr.id LIMIT ?1 OFFSET ?2"
    );
    const BY_ARTIST: &str = concat!(
        release_select!(),
        "ORDER BY ",
        release_artist!(),
        " COLLATE NOCASE, ",
        release_year!(),
        ", ",
        release_name!(),
        " COLLATE NOCASE, lr.id LIMIT ?1 OFFSET ?2"
    );
    const RANDOM: &str = concat!(release_select!(), "ORDER BY RANDOM() LIMIT ?1 OFFSET ?2");
    const YEARS_UP: &str = concat!(
        release_select!(),
        "WHERE ",
        release_year!(),
        " BETWEEN ?3 AND ?4 ORDER BY ",
        release_year!(),
        ", ",
        release_name!(),
        " COLLATE NOCASE, lr.id LIMIT ?1 OFFSET ?2"
    );
    const YEARS_DOWN: &str = concat!(
        release_select!(),
        "WHERE ",
        release_year!(),
        " BETWEEN ?4 AND ?3 ORDER BY ",
        release_year!(),
        " DESC, ",
        release_name!(),
        " COLLATE NOCASE, lr.id LIMIT ?1 OFFSET ?2"
    );
    let (sql, years) = match order {
        ReleaseOrder::Id => (BY_ID, None),
        ReleaseOrder::Newest => (NEWEST, None),
        ReleaseOrder::Name => (BY_NAME, None),
        ReleaseOrder::Artist => (BY_ARTIST, None),
        ReleaseOrder::Random => (RANDOM, None),
        ReleaseOrder::Years { from, to } if from <= to => (YEARS_UP, Some((from, to))),
        ReleaseOrder::Years { from, to } => (YEARS_DOWN, Some((from, to))),
    };
    let mut query = sqlx::query_as::<_, LibraryRelease>(sql)
        .bind(limit)
        .bind(offset);
    if let Some((from, to)) = years {
        query = query.bind(from).bind(to);
    }
    Ok(query
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(clean_release)
        .collect())
}

/// The releases whose artist key equals `key` ignoring ASCII case, by year,
/// then title.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn artist_releases(pool: &SqlitePool, key: &str) -> Result<Vec<LibraryRelease>> {
    let rows = sqlx::query_as::<_, LibraryRelease>(concat!(
        release_select!(),
        "WHERE ",
        release_artist!(),
        " = ?1 COLLATE NOCASE ORDER BY ",
        release_year!(),
        ", ",
        release_name!(),
        " COLLATE NOCASE, lr.id"
    ))
    .bind(key)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(clean_release).collect())
}

/// A page of releases whose title or artist contains `query`, by title.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn search_library_releases(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<LibraryRelease>> {
    let rows = sqlx::query_as::<_, LibraryRelease>(concat!(
        release_select!(),
        "WHERE ",
        release_name!(),
        " LIKE ?1 COLLATE NOCASE ESCAPE '\\' OR ",
        release_artist!(),
        " LIKE ?1 COLLATE NOCASE ESCAPE '\\' ORDER BY ",
        release_name!(),
        " COLLATE NOCASE, lr.id LIMIT ?2 OFFSET ?3"
    ))
    .bind(like_contains(query))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(clean_release).collect())
}

/// A page of album artists, by name ignoring ASCII case, each with its
/// number of releases. With `query`, only artists whose name contains it.
/// A negative `limit` means no limit.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn library_artists(
    pool: &SqlitePool,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<LibraryArtist>> {
    // `k` names the key inside, so GROUP BY and ORDER BY can't mistake the
    // `artist` alias (an aggregate) for it.
    Ok(sqlx::query_as::<_, LibraryArtist>(concat!(
        "SELECT MIN(k) AS artist, COUNT(*) AS album_count
         FROM (SELECT ",
        release_artist!(),
        " AS k
               FROM local_releases lr
               LEFT JOIN release_matches m ON m.local_release_id = lr.id)
         WHERE ?1 IS NULL OR k LIKE ?1 COLLATE NOCASE ESCAPE '\\'
         GROUP BY k COLLATE NOCASE
         ORDER BY MIN(k) COLLATE NOCASE
         LIMIT ?2 OFFSET ?3"
    ))
    .bind(query.map(like_contains))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_in_memory;
    use crate::test_support::{
        FIXTURE_MBID, accept, mbid_release, remote_release, scanned, seed_release, store_tracks,
    };

    #[tokio::test]
    async fn a_release_shows_its_tags_until_a_match_is_accepted() {
        let pool = connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One", "Two"]).await;

        let shown = library_release(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(shown.name.as_deref(), Some("Blue"));
        assert_eq!(shown.artist, "Artist");
        assert_eq!(shown.year, Some(2001));
        assert_eq!(shown.track_count, 2);
        assert_eq!(shown.duration_secs, 360.0);
        assert_eq!(shown.directory, "/music/a");
        assert_eq!(shown.musicbrainz_id, None);

        let mut remote = remote_release(mbid_release(FIXTURE_MBID), "Remote Blue", &["One", "Two"]);
        remote.artists = vec!["Remote Artist".to_string()];
        remote.year = Some(1999);
        accept(&pool, &release, remote).await;

        let shown = library_release(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(shown.name.as_deref(), Some("Remote Blue"));
        assert_eq!(shown.artist, "Remote Artist");
        assert_eq!(shown.year, Some(1999));
        assert_eq!(shown.musicbrainz_id.as_deref(), Some(FIXTURE_MBID));
        assert_eq!(
            library_release(&pool, release.id + 100).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_release_without_artist_or_title_falls_back() {
        let pool = connect_in_memory().await.unwrap();
        let mut bare = scanned("/music/untitled/01.mp3", "", None, "One", 1, 2001);
        bare.album = None;
        store_tracks(&pool, "/music", &[bare]).await;

        let release = library_releases(&pool, ReleaseOrder::Id, 10, 0)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(release.artist, UNKNOWN_ARTIST);
        assert_eq!(release.display_artist(), UNKNOWN_ARTIST);
        assert_eq!(release.name, None);
        assert_eq!(release.display_name(), "untitled");
    }

    #[tokio::test]
    async fn artists_group_ignoring_ascii_case_and_every_key_resolves() {
        let pool = connect_in_memory().await.unwrap();
        store_tracks(
            &pool,
            "/music",
            &[
                scanned(
                    "/music/a/01.mp3",
                    "Bleach",
                    Some("Nirvana"),
                    "Blew",
                    1,
                    1989,
                ),
                scanned(
                    "/music/b/01.mp3",
                    "Nevermind",
                    Some("NIRVANA"),
                    "Breed",
                    1,
                    1991,
                ),
                scanned("/music/c/01.mp3", "Tabs", Some("Tabbed\t"), "Tab", 1, 2000),
                scanned("/music/d/01.mp3", "Arrival", Some("Abba"), "Song", 1, 1975),
            ],
        )
        .await;

        let artists = library_artists(&pool, None, -1, 0).await.unwrap();
        let shown: Vec<(&str, i64)> = artists
            .iter()
            .map(|artist| (display_artist(&artist.artist), artist.album_count))
            .collect();
        assert_eq!(shown, [("Abba", 1), ("NIRVANA", 2), ("Tabbed", 1)]);
        for artist in &artists {
            let releases = artist_releases(&pool, &artist.artist.to_ascii_lowercase())
                .await
                .unwrap();
            assert_eq!(
                releases.len() as i64,
                artist.album_count,
                "{:?}",
                artist.artist
            );
        }

        let found = library_artists(&pool, Some("nirv"), -1, 0).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].album_count, 2);
        let page = library_artists(&pool, None, 1, 1).await.unwrap();
        assert_eq!(display_artist(&page[0].artist), "NIRVANA");

        let nirvana = artist_releases(&pool, "nirvana").await.unwrap();
        let years: Vec<Option<i64>> = nirvana.iter().map(|release| release.year).collect();
        assert_eq!(years, [Some(1989), Some(1991)]);
    }

    #[tokio::test]
    async fn release_orders_are_total_and_pages_do_not_overlap() {
        let pool = connect_in_memory().await.unwrap();
        store_tracks(
            &pool,
            "/music",
            &[
                scanned("/music/a/01.mp3", "b", Some("Zed"), "x", 1, 2003),
                scanned("/music/b/01.mp3", "A", Some("Abba"), "x", 1, 2001),
                scanned("/music/c/01.mp3", "a", Some("abba"), "x", 1, 2002),
            ],
        )
        .await;
        let years = |rows: &[LibraryRelease]| rows.iter().map(|row| row.year).collect::<Vec<_>>();

        let by_name = library_releases(&pool, ReleaseOrder::Name, 10, 0)
            .await
            .unwrap();
        let names: Vec<String> = by_name
            .iter()
            .map(|row| row.display_name().to_lowercase())
            .collect();
        assert_eq!(names, ["a", "a", "b"]);
        assert!(by_name[0].id < by_name[1].id, "the ID breaks the tie");
        for (offset, expected) in by_name.iter().enumerate() {
            let page = library_releases(&pool, ReleaseOrder::Name, 1, offset as i64)
                .await
                .unwrap();
            assert_eq!(page[0].id, expected.id);
        }

        let newest = library_releases(&pool, ReleaseOrder::Newest, 10, 0)
            .await
            .unwrap();
        let ids: Vec<i64> = newest.iter().map(|row| row.id).collect();
        let mut descending = ids.clone();
        descending.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            ids, descending,
            "stored in the same second: highest ID first"
        );

        let by_artist = library_releases(&pool, ReleaseOrder::Artist, 10, 0)
            .await
            .unwrap();
        assert_eq!(years(&by_artist), [Some(2001), Some(2002), Some(2003)]);
        let up = ReleaseOrder::Years {
            from: 2001,
            to: 2002,
        };
        assert_eq!(
            years(&library_releases(&pool, up, 10, 0).await.unwrap()),
            [Some(2001), Some(2002)]
        );
        let down = ReleaseOrder::Years {
            from: 2003,
            to: 2002,
        };
        assert_eq!(
            years(&library_releases(&pool, down, 10, 0).await.unwrap()),
            [Some(2003), Some(2002)]
        );
        assert_eq!(
            library_releases(&pool, ReleaseOrder::Random, 10, 0)
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            library_releases(&pool, ReleaseOrder::Id, -1, 0)
                .await
                .unwrap()
                .len(),
            3
        );

        let found = search_library_releases(&pool, "ZED", 10, 0).await.unwrap();
        assert_eq!(years(&found), [Some(2003)]);
        let found = search_library_releases(&pool, "a", 10, 0).await.unwrap();
        assert_eq!(found.len(), 2, "title A, title a (Zed's title is b)");
    }
}
