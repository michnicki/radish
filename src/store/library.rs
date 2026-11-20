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
use std::collections::HashSet;
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

/// One page of releases in `$order`, selected by ID first.
///
/// The inner query sorts and pages the IDs alone. Sorting complete rows would
/// compute the duration subquery of every row it skips, which makes deep
/// pages slow. The outer query then reads the columns of the page's releases
/// and repeats `$order` to keep the page's order. `$filter` is empty or a
/// `WHERE` clause. The limit is `?1` and the offset `?2`.
macro_rules! release_page {
    ($filter:expr, $order:expr) => {
        concat!(
            release_select!(),
            "WHERE lr.id IN (SELECT lr.id FROM local_releases lr
                             LEFT JOIN release_matches m ON m.local_release_id = lr.id ",
            $filter,
            " ORDER BY ",
            $order,
            " LIMIT ?1 OFFSET ?2)
             ORDER BY ",
            $order
        )
    };
}

/// The columns of [`LibraryTrack`]. Follow with `FROM tracks t` and
/// `track_joins!`.
macro_rules! track_select {
    () => {
        concat!(
            "SELECT t.id AS id, t.local_release_id AS release_id, t.path AS path, ",
            effective_track_title!(),
            " AS title, ",
            effective_track_artist!(),
            " AS artist, ",
            effective_track_number!(),
            " AS track_number, ",
            effective_disc_number!(),
            " AS disc_number,
                    t.duration_secs AS duration_secs, ",
            release_name!(),
            " AS album, ",
            release_artist!(),
            " AS album_artist, ",
            release_year!(),
            " AS year,
                    lr.created_at AS created_at,
                    lr.directory_path AS directory,
                    d.path AS root,
                    CASE WHEN m.provider = 'musicbrainz' THEN tm.remote_recording_id END
                        AS musicbrainz_id "
        )
    };
}

/// The joins `track_select!` reads, after `tracks t`. Ends with a space.
macro_rules! track_joins {
    () => {
        concat!(
            "JOIN local_releases lr ON lr.id = t.local_release_id
             LEFT JOIN directories d ON d.id = t.directory_id ",
            overlay_joins!(),
            " "
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

/// A track as the API shows it, with the release values a song carries.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LibraryTrack {
    pub id: i64,
    pub release_id: i64,
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub duration_secs: Option<f64>,
    /// The release's effective title.
    pub album: Option<String>,
    /// The release's artist key, as in [`LibraryRelease::artist`].
    pub album_artist: String,
    /// The release's effective year.
    pub year: Option<i64>,
    /// When the release was first stored.
    pub created_at: String,
    /// The release's folder.
    pub directory: String,
    /// The imported root the file is under.
    pub root: Option<String>,
    /// The recording MBID of an accepted MusicBrainz match.
    pub musicbrainz_id: Option<String>,
}

impl LibraryTrack {
    /// The title, or the file name when the track has none.
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| folder_name(&self.path))
    }

    /// The release's title, or its folder name when it has none.
    pub fn display_album(&self) -> String {
        self.album
            .clone()
            .unwrap_or_else(|| folder_name(&self.directory))
    }
}

fn clean_track(mut track: LibraryTrack) -> LibraryTrack {
    track.title = shown_tag(track.title);
    track.artist = shown_tag(track.artist);
    track.album = shown_tag(track.album);
    track
}

/// How many times [`random_tracks`] samples track IDs before it shuffles the
/// whole selection instead.
const SAMPLE_ROUNDS: usize = 4;

/// The most tracks [`random_tracks`] returns, whatever `count` asks for. The
/// API limits a request to the same number.
pub const MAX_RANDOM_TRACKS: usize = 500;

/// How a page of releases is ordered. Every order ends with the release ID,
/// so pages never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOrder {
    /// Oldest stored first: stable while the library grows.
    Id,
    /// Most recently stored first. A release's ID grows with the time it was
    /// stored, so this is the highest ID first.
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
    const BY_ID: &str = release_page!("", "lr.id");
    // `created_at` is set only by its column default when a release is
    // inserted, and IDs are AUTOINCREMENT, so the newest release is the one
    // with the highest ID. Sorting by ID can use the primary key.
    const NEWEST: &str = release_page!("", "lr.id DESC");
    const BY_NAME: &str = release_page!("", concat!(release_name!(), " COLLATE NOCASE, lr.id"));
    const BY_ARTIST: &str = release_page!(
        "",
        concat!(
            release_artist!(),
            " COLLATE NOCASE, ",
            release_year!(),
            ", ",
            release_name!(),
            " COLLATE NOCASE, lr.id"
        )
    );
    const YEARS_UP: &str = release_page!(
        concat!("WHERE ", release_year!(), " BETWEEN ?3 AND ?4"),
        concat!(
            release_year!(),
            ", ",
            release_name!(),
            " COLLATE NOCASE, lr.id"
        )
    );
    const YEARS_DOWN: &str = release_page!(
        concat!("WHERE ", release_year!(), " BETWEEN ?4 AND ?3"),
        concat!(
            release_year!(),
            " DESC, ",
            release_name!(),
            " COLLATE NOCASE, lr.id"
        )
    );
    // A random page can't repeat its order, so it draws the IDs first and
    // reads their rows in that order.
    const RANDOM_IDS: &str = "SELECT id FROM local_releases ORDER BY RANDOM() LIMIT ?1 OFFSET ?2";
    const RANDOM: &str = concat!(
        release_select!(),
        "JOIN json_each(?1) j ON j.value = lr.id ORDER BY j.key"
    );
    let (sql, years) = match order {
        ReleaseOrder::Id => (BY_ID, None),
        ReleaseOrder::Newest => (NEWEST, None),
        ReleaseOrder::Name => (BY_NAME, None),
        ReleaseOrder::Artist => (BY_ARTIST, None),
        ReleaseOrder::Random => {
            let ids: Vec<i64> = sqlx::query_scalar(RANDOM_IDS)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            let rows = sqlx::query_as::<_, LibraryRelease>(RANDOM)
                .bind(serde_json::to_string(&ids)?)
                .fetch_all(pool)
                .await?;
            return Ok(rows.into_iter().map(clean_release).collect());
        }
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
    // The limit is `?1` and the offset `?2`, as `release_page!` expects.
    const SQL: &str = release_page!(
        concat!(
            "WHERE ",
            release_name!(),
            " LIKE ?3 COLLATE NOCASE ESCAPE '\\' OR ",
            release_artist!(),
            " LIKE ?3 COLLATE NOCASE ESCAPE '\\'"
        ),
        concat!(release_name!(), " COLLATE NOCASE, lr.id")
    );
    let rows = sqlx::query_as::<_, LibraryRelease>(SQL)
        .bind(limit)
        .bind(offset)
        .bind(like_contains(query))
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

/// One track, `None` when there is no track `id`.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn library_track(pool: &SqlitePool, id: i64) -> Result<Option<LibraryTrack>> {
    let row = sqlx::query_as::<_, LibraryTrack>(concat!(
        track_select!(),
        "FROM tracks t ",
        track_joins!(),
        "WHERE t.id = ?1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(clean_track))
}

/// The tracks of a release, by disc, number, title and ID.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn release_tracks(pool: &SqlitePool, release_id: i64) -> Result<Vec<LibraryTrack>> {
    let rows = sqlx::query_as::<_, LibraryTrack>(concat!(
        track_select!(),
        "FROM tracks t ",
        track_joins!(),
        "WHERE t.local_release_id = ?1
         ORDER BY disc_number, track_number, title, t.id"
    ))
    .bind(release_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(clean_track).collect())
}

/// The path of the first track of a release, in the order of
/// [`release_tracks`], without reading the rest.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn first_track_path(pool: &SqlitePool, release_id: i64) -> Result<Option<String>> {
    let path = sqlx::query_scalar::<_, String>(concat!(
        "SELECT t.path AS path, ",
        effective_track_title!(),
        " AS title, ",
        effective_track_number!(),
        " AS track_number, ",
        effective_disc_number!(),
        " AS disc_number
         FROM tracks t ",
        track_joins!(),
        "WHERE t.local_release_id = ?1
         ORDER BY disc_number, track_number, title, t.id
         LIMIT 1"
    ))
    .bind(release_id)
    .fetch_optional(pool)
    .await?;
    Ok(path)
}

/// A page of tracks by ID. With `query`, only tracks whose effective title or
/// artist contains it.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn library_tracks(
    pool: &SqlitePool,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<LibraryTrack>> {
    // The IDs are paged alone, so the joins and the columns are read for one
    // page of tracks only, not for every track the offset skips. Tracks
    // without a release have no place in the library.
    const ALL: &str = concat!(
        track_select!(),
        "FROM tracks t ",
        track_joins!(),
        "WHERE t.id IN (SELECT id FROM tracks WHERE local_release_id IS NOT NULL
                        ORDER BY id LIMIT ?1 OFFSET ?2)
         ORDER BY t.id"
    );
    // Two steps, because the effective values can't use an index: first the
    // tracks whose tag or overlay contains the text (plain table scans), then
    // the ones whose effective value still does. On 2.76M tracks this takes
    // 0.9 s where filtering the joined rows takes 3 s. The union yields IDs in
    // order, so ordering by `h.id` (not `t.id`) lets the limit stop the scan
    // early instead of walking every track.
    const SEARCH: &str = concat!(
        "WITH hits(id) AS (
             SELECT id FROM tracks
             WHERE title LIKE ?1 COLLATE NOCASE ESCAPE '\\'
                OR artist LIKE ?1 COLLATE NOCASE ESCAPE '\\'
             UNION
             SELECT track_id FROM release_track_matches
             WHERE remote_title LIKE ?1 COLLATE NOCASE ESCAPE '\\'
                OR remote_artist LIKE ?1 COLLATE NOCASE ESCAPE '\\')
         ",
        track_select!(),
        "FROM hits h JOIN tracks t ON t.id = h.id ",
        track_joins!(),
        "WHERE ",
        effective_track_title!(),
        " LIKE ?1 COLLATE NOCASE ESCAPE '\\' OR ",
        effective_track_artist!(),
        " LIKE ?1 COLLATE NOCASE ESCAPE '\\'
         ORDER BY h.id LIMIT ?2 OFFSET ?3"
    );
    let rows = match query {
        None => {
            sqlx::query_as::<_, LibraryTrack>(ALL)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?
        }
        Some(query) => {
            sqlx::query_as::<_, LibraryTrack>(SEARCH)
                .bind(like_contains(query))
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(clean_track).collect())
}

/// Up to `count` different tracks in random order, from releases of `from`
/// or later and `to` or earlier when given. `count` is capped at
/// [`MAX_RANDOM_TRACKS`], and the result is empty when `from` is after `to`.
///
/// It samples track IDs: `ORDER BY RANDOM()` would sort every track of a
/// large library. It falls back to that when sampling keeps missing,
/// which happens with a narrow year filter.
///
/// # Errors
///
/// Returns an error when the database cannot be read.
pub async fn random_tracks(
    pool: &SqlitePool,
    count: usize,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Vec<LibraryTrack>> {
    // Sampled IDs keep their drawn order through `j.key`.
    const SAMPLED: &str = concat!(
        track_select!(),
        "FROM json_each(?1) j JOIN tracks t ON t.id = j.value ",
        track_joins!(),
        "WHERE (?2 IS NULL OR ",
        release_year!(),
        " >= ?2) AND (?3 IS NULL OR ",
        release_year!(),
        " <= ?3)
         ORDER BY j.key"
    );
    const SHUFFLED: &str = concat!(
        track_select!(),
        "FROM tracks t ",
        track_joins!(),
        "WHERE (?2 IS NULL OR ",
        release_year!(),
        " >= ?2) AND (?3 IS NULL OR ",
        release_year!(),
        " <= ?3)
         ORDER BY RANDOM() LIMIT ?1"
    );
    let count = count.min(MAX_RANDOM_TRACKS);
    if count == 0 || matches!((from, to), (Some(from), Some(to)) if from > to) {
        return Ok(Vec::new());
    }
    let (low, high): (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT MIN(id), MAX(id) FROM tracks")
            .fetch_one(pool)
            .await?;
    let (Some(low), Some(high)) = (low, high) else {
        return Ok(Vec::new());
    };
    let span = high.abs_diff(low).saturating_add(1);
    let mut picked: Vec<LibraryTrack> = Vec::with_capacity(count);
    let mut seen = HashSet::new();
    for _ in 0..SAMPLE_ROUNDS {
        let wanted = count - picked.len();
        if wanted == 0 {
            break;
        }
        // Three draws per wanted track, from one call to the system.
        let mut bytes = vec![0u8; wanted * 3 * 8];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("cannot draw random numbers: {error}"))?;
        let (draws, _) = bytes.as_chunks::<8>();
        let ids: Vec<i64> = draws
            .iter()
            .map(|chunk| {
                // The offset is below `span`, so the sum is an ID in range.
                low.wrapping_add((u64::from_le_bytes(*chunk) % span) as i64)
            })
            .collect();
        let rows = sqlx::query_as::<_, LibraryTrack>(SAMPLED)
            .bind(serde_json::to_string(&ids)?)
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await?;
        for row in rows {
            if picked.len() < count && seen.insert(row.id) {
                picked.push(clean_track(row));
            }
        }
    }
    if picked.len() < count {
        let rows = sqlx::query_as::<_, LibraryTrack>(SHUFFLED)
            .bind(i64::try_from(count)?)
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await?;
        for row in rows {
            if picked.len() < count && seen.insert(row.id) {
                picked.push(clean_track(row));
            }
        }
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_in_memory;
    use crate::test_support::{
        FIXTURE_MBID, OTHER_MBID, accept, mbid_release, remote_release, scanned, seed_release,
        store_tracks,
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
                // SQL `TRIM` leaves a no-break space.
                scanned(
                    "/music/e/01.mp3",
                    "Nbsp",
                    Some("Nbsp\u{a0}"),
                    "Gap",
                    1,
                    2010,
                ),
                scanned("/music/f/01.mp3", "Nobody", None, "Anon", 1, 2011),
            ],
        )
        .await;
        // A key that comes from an accepted match, not from the tags.
        let matched = seed_release(&pool, "/other/g", "Tagged", &["One", "Two"]).await;
        let mut remote = remote_release(mbid_release(FIXTURE_MBID), "Tagged", &["One", "Two"]);
        remote.artists = vec!["Remote Artist".to_string()];
        accept(&pool, &matched, remote).await;

        let artists = library_artists(&pool, None, -1, 0).await.unwrap();
        let shown: Vec<(&str, i64)> = artists
            .iter()
            .map(|artist| (display_artist(&artist.artist), artist.album_count))
            .collect();
        assert_eq!(
            shown,
            [
                (UNKNOWN_ARTIST, 1),
                ("Abba", 1),
                ("Nbsp", 1),
                ("NIRVANA", 2),
                ("Remote Artist", 1),
                ("Tabbed", 1),
            ]
        );
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
        let page = library_artists(&pool, None, 1, 3).await.unwrap();
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

    #[tokio::test]
    async fn release_orders_and_search_use_the_accepted_match() {
        let pool = connect_in_memory().await.unwrap();
        seed_release(&pool, "/music/a", "Alpha", &["One"]).await;
        seed_release(&pool, "/music/b", "Beta", &["One"]).await;
        let gamma = seed_release(&pool, "/music/c", "Gamma", &["One"]).await;
        let mut remote = remote_release(mbid_release(FIXTURE_MBID), "Aaa Remote Title", &["One"]);
        remote.artists = vec!["Aaa Remote Artist".to_string()];
        accept(&pool, &gamma, remote).await;
        let names = |rows: Vec<LibraryRelease>| {
            rows.iter()
                .map(LibraryRelease::display_name)
                .collect::<Vec<_>>()
        };

        // By the tags, Gamma would sort last in both orders.
        let by_name = library_releases(&pool, ReleaseOrder::Name, 10, 0).await;
        assert_eq!(
            names(by_name.unwrap()),
            ["Aaa Remote Title", "Alpha", "Beta"]
        );
        let by_artist = library_releases(&pool, ReleaseOrder::Artist, 10, 0).await;
        assert_eq!(
            names(by_artist.unwrap()),
            ["Aaa Remote Title", "Alpha", "Beta"]
        );

        let found = search_library_releases(&pool, "remote title", 10, 0).await;
        assert_eq!(names(found.unwrap()), ["Aaa Remote Title"]);
        let found = search_library_releases(&pool, "remote artist", 10, 0).await;
        assert_eq!(names(found.unwrap()), ["Aaa Remote Title"]);
        let found = search_library_releases(&pool, "gamma", 10, 0).await;
        assert!(
            found.unwrap().is_empty(),
            "the replaced tag is not searched"
        );
    }

    #[tokio::test]
    async fn the_first_track_path_follows_the_order_of_the_release_tracks() {
        let pool = connect_in_memory().await.unwrap();
        // Stored out of order, with a second disc whose first track sorts first by title.
        let mut disc_two = scanned("/music/a/d2.flac", "Blue", Some("Artist"), "Aaa", 1, 2001);
        disc_two.disc_number = Some(2);
        let tracks = [
            disc_two,
            scanned("/music/a/02.flac", "Blue", Some("Artist"), "Two", 2, 2001),
            scanned("/music/a/01.flac", "Blue", Some("Artist"), "One", 1, 2001),
        ];
        store_tracks(&pool, "/music/a", &tracks).await;
        let release_id = library_tracks(&pool, None, 1, 0).await.unwrap()[0].release_id;

        let all = release_tracks(&pool, release_id).await.unwrap();
        assert_eq!(all[0].path, "/music/a/01.flac");
        assert_eq!(
            first_track_path(&pool, release_id)
                .await
                .unwrap()
                .as_deref(),
            Some(all[0].path.as_str())
        );
        assert_eq!(
            first_track_path(&pool, release_id + 100).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn tracks_show_the_overlay_and_their_release() {
        let pool = connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One", "Two"]).await;
        let mut remote = remote_release(mbid_release(FIXTURE_MBID), "Remote Blue", &["ONE", "Two"]);
        remote.tracklist[0].artists = vec!["Guest".to_string()];
        remote.tracklist[0].recording_id = Some(OTHER_MBID.parse().unwrap());
        accept(&pool, &release, remote).await;

        let tracks = release_tracks(&pool, release.id).await.unwrap();
        let titles: Vec<String> = tracks.iter().map(LibraryTrack::display_title).collect();
        assert_eq!(titles, ["ONE", "Two"]);
        assert_eq!(tracks[0].artist.as_deref(), Some("Guest"));
        assert_eq!(
            tracks[1].artist.as_deref(),
            Some("Artist"),
            "the release artist"
        );
        assert_eq!(tracks[0].musicbrainz_id.as_deref(), Some(OTHER_MBID));
        assert_eq!(tracks[0].album.as_deref(), Some("Remote Blue"));
        assert_eq!(tracks[0].album_artist, "Artist");
        assert_eq!(tracks[0].track_number, Some(1));
        assert_eq!(tracks[0].duration_secs, Some(180.0));
        assert_eq!(tracks[0].release_id, release.id);
        assert_eq!(tracks[0].root.as_deref(), Some("/music/a"));
        assert_eq!(tracks[0].path, "/music/a/01.flac");
        assert_eq!(
            library_track(&pool, tracks[1].id).await.unwrap(),
            Some(tracks[1].clone())
        );
        assert_eq!(library_track(&pool, 999).await.unwrap(), None);
    }

    #[tokio::test]
    async fn song_search_matches_effective_titles_and_artists_only() {
        let pool = connect_in_memory().await.unwrap();
        let blue = seed_release(&pool, "/music/a", "Blue", &["One", "Two"]).await;
        seed_release(&pool, "/music/b", "Red", &["Three"]).await;
        let mut remote = remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]);
        remote.tracklist[1].artists = vec!["Overlay Guest".to_string()];
        accept(&pool, &blue, remote).await;
        let titles = |rows: Vec<LibraryTrack>| {
            rows.iter()
                .map(LibraryTrack::display_title)
                .collect::<Vec<_>>()
        };

        // Found only through the overlay.
        assert_eq!(
            titles(
                library_tracks(&pool, Some("overlay guest"), 10, 0)
                    .await
                    .unwrap()
            ),
            ["Two"]
        );
        // Found only through the tags.
        assert_eq!(
            titles(library_tracks(&pool, Some("three"), 10, 0).await.unwrap()),
            ["Three"]
        );
        // "Two" is tagged "Artist", but its effective artist is the guest.
        assert_eq!(
            titles(library_tracks(&pool, Some("artist"), 10, 0).await.unwrap()),
            ["One", "Three"]
        );
        assert_eq!(
            titles(library_tracks(&pool, None, 10, 0).await.unwrap()),
            ["One", "Two", "Three"]
        );
        assert_eq!(
            titles(library_tracks(&pool, None, 1, 2).await.unwrap()),
            ["Three"]
        );
        // `%` and `_` are plain text: unescaped, each would match every track.
        for wildcard in ["%", "_"] {
            assert!(
                library_tracks(&pool, Some(wildcard), 10, 0)
                    .await
                    .unwrap()
                    .is_empty(),
                "{wildcard:?}"
            );
        }
    }

    #[tokio::test]
    async fn random_tracks_respect_the_count_and_the_years() {
        let pool = connect_in_memory().await.unwrap();
        store_tracks(
            &pool,
            "/music",
            &[
                scanned("/music/a/01.mp3", "Old", Some("X"), "Old One", 1, 1980),
                scanned("/music/a/02.mp3", "Old", Some("X"), "Old Two", 2, 1980),
                scanned("/music/b/01.mp3", "New", Some("Y"), "New One", 1, 2020),
            ],
        )
        .await;

        let all = random_tracks(&pool, 10, None, None).await.unwrap();
        assert_eq!(all.len(), 3);
        let distinct: HashSet<i64> = all.iter().map(|track| track.id).collect();
        assert_eq!(distinct.len(), 3);
        assert_eq!(random_tracks(&pool, 2, None, None).await.unwrap().len(), 2);
        let new = random_tracks(&pool, 10, Some(2000), None).await.unwrap();
        let titles: Vec<String> = new.iter().map(LibraryTrack::display_title).collect();
        assert_eq!(titles, ["New One"]);
        assert_eq!(
            random_tracks(&pool, 10, None, Some(1990))
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            random_tracks(&pool, 0, None, None)
                .await
                .unwrap()
                .is_empty()
        );
        let empty = connect_in_memory().await.unwrap();
        assert!(
            random_tracks(&empty, 5, None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn random_tracks_are_capped_and_empty_for_a_reversed_year_range() {
        let pool = connect_in_memory().await.unwrap();
        let tracks: Vec<_> = (1..=MAX_RANDOM_TRACKS as i64 + 100)
            .map(|number| {
                let path = format!("/music/big/{number:04}.mp3");
                scanned(&path, "Big", Some("X"), "Song", number, 2000)
            })
            .collect();
        store_tracks(&pool, "/music", &tracks).await;

        let capped = random_tracks(&pool, MAX_RANDOM_TRACKS + 50, None, None)
            .await
            .unwrap();
        assert_eq!(capped.len(), MAX_RANDOM_TRACKS);
        let distinct: HashSet<i64> = capped.iter().map(|track| track.id).collect();
        assert_eq!(distinct.len(), MAX_RANDOM_TRACKS);
        assert_eq!(
            random_tracks(&pool, usize::MAX, None, None)
                .await
                .unwrap()
                .len(),
            MAX_RANDOM_TRACKS
        );

        let reversed = random_tracks(&pool, 10, Some(2010), Some(1990)).await;
        assert!(reversed.unwrap().is_empty());
        let same_year = random_tracks(&pool, 10, Some(2000), Some(2000)).await;
        assert_eq!(same_year.unwrap().len(), 10);
    }
}
