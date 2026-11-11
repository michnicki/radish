use crate::matcher::{
    self, ArtistKind, Candidate, EmbeddedReleaseEvidence, LocalRelease, LocalTrack, Mapping,
    MatchScope, RemoteRelease, RemoteTrack,
};
use crate::providers::{GroupId, Mbid, ProviderKind, ReleaseId};
use crate::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use anyhow::{Result, bail};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const STATUS_UNMATCHED: &str = "unmatched";
pub const STATUS_MATCHED: &str = "matched";
pub const STATUS_STALE: &str = "stale";

/// Version stamped on new snapshots. Version 1 is the historical Discogs shape.
pub const SNAPSHOT_VERSION: i64 = 2;

// ---------------------------------------------------------------------------
// Scanned-metadata storage
// ---------------------------------------------------------------------------

/// Starts a write transaction that takes the database write lock immediately.
///
/// A deferred transaction that reads first and writes later can fail with
/// `SQLITE_BUSY` the moment it tries to upgrade, which `busy_timeout` cannot
/// retry. Taking the lock up front makes concurrent writers queue instead.
/// Helpers called inside the transaction must take the connection, never the
/// pool: the in-memory test pool has exactly one connection.
async fn begin_write(pool: &SqlitePool) -> Result<Transaction<'static, Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

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

/// Finds or creates the album row for a title and artist. A missing and an
/// empty artist are the same artist. The year is a per-file tag and lives on
/// the track, never on the shared album row.
///
/// Takes a connection so it can run inside the caller's write transaction.
async fn upsert_album(
    conn: &mut SqliteConnection,
    title: &str,
    artist: Option<&str>,
) -> Result<i64> {
    let artist = artist.filter(|artist| !artist.is_empty());
    // The no-op update makes `RETURNING` yield the existing row on conflict.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO albums (title, artist) VALUES (?1, ?2)
         ON CONFLICT(title, IFNULL(artist, '')) DO UPDATE SET title = excluded.title
         RETURNING id",
    )
    .bind(title)
    .bind(artist)
    .fetch_one(conn)
    .await?;
    Ok(id)
}

/// How many tracks one `upsert_tracks` transaction covers. One transaction per
/// file is dominated by commit cost on large imports; one for the whole library
/// would hold the write lock for minutes. Five hundred keeps each lock hold
/// short while amortizing the commit.
pub const UPSERT_BATCH_SIZE: usize = 500;

/// Writes observed tag metadata for a batch of files in one transaction, and
/// returns the number of rows that changed. Accepted provider metadata lives in
/// separate tables and is never touched here, so re-importing keeps an accepted
/// match.
pub async fn upsert_tracks(
    pool: &SqlitePool,
    tracks: &[ScannedTrack],
    directory_id: i64,
) -> Result<u64> {
    let mut tx = begin_write(pool).await?;
    // Albums repeat across a folder, so remember each one for this batch only.
    // The cache dies with the transaction, so a rollback can never leave a
    // stale ID behind.
    let mut albums: HashMap<(String, Option<String>), i64> = HashMap::new();
    let mut changed = 0;
    for track in tracks {
        changed += write_track(&mut tx, track, directory_id, &mut albums).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Writes observed tag metadata for one file. See [`upsert_tracks`].
pub async fn upsert_track(
    pool: &SqlitePool,
    track: &ScannedTrack,
    directory_id: i64,
) -> Result<()> {
    upsert_tracks(pool, std::slice::from_ref(track), directory_id).await?;
    Ok(())
}

async fn write_track(
    conn: &mut SqliteConnection,
    track: &ScannedTrack,
    directory_id: i64,
    albums: &mut HashMap<(String, Option<String>), i64>,
) -> Result<u64> {
    let album_id = match &track.album {
        Some(title) if !title.is_empty() => {
            let artist = track.artist.clone().filter(|artist| !artist.is_empty());
            let key = (title.clone(), artist);
            let id = match albums.get(&key) {
                Some(id) => *id,
                None => {
                    let id = upsert_album(&mut *conn, title, key.1.as_deref()).await?;
                    albums.insert(key, id);
                    id
                }
            };
            Some(id)
        }
        _ => None,
    };

    let musicbrainz_json = serde_json::to_string(&track.musicbrainz)?;
    let path_str = track.path.to_string_lossy().to_string();
    let result = sqlx::query(
        "INSERT INTO tracks (
            path, title, artist, album_id, directory_id, track_number, duration_secs, modified_at,
            album_artist, disc_number, barcode, catalog_number, discogs_release_id,
            musicbrainz_ids_json, year
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            artist = excluded.artist,
            album_id = excluded.album_id,
            directory_id = excluded.directory_id,
            track_number = excluded.track_number,
            duration_secs = excluded.duration_secs,
            modified_at = excluded.modified_at,
            album_artist = excluded.album_artist,
            disc_number = excluded.disc_number,
            barcode = excluded.barcode,
            catalog_number = excluded.catalog_number,
            discogs_release_id = excluded.discogs_release_id,
            musicbrainz_ids_json = excluded.musicbrainz_ids_json,
            year = excluded.year
         WHERE (
            tracks.title, tracks.artist, tracks.album_id, tracks.directory_id,
            tracks.track_number, tracks.duration_secs, tracks.modified_at, tracks.album_artist,
            tracks.disc_number, tracks.barcode, tracks.catalog_number, tracks.discogs_release_id,
            tracks.musicbrainz_ids_json, tracks.year
         ) IS NOT (
            excluded.title, excluded.artist, excluded.album_id, excluded.directory_id,
            excluded.track_number, excluded.duration_secs, excluded.modified_at,
            excluded.album_artist, excluded.disc_number, excluded.barcode,
            excluded.catalog_number, excluded.discogs_release_id,
            excluded.musicbrainz_ids_json, excluded.year
         )",
    )
    .bind(&path_str)
    .bind(&track.title)
    .bind(&track.artist)
    .bind(album_id)
    .bind(directory_id)
    .bind(track.track_number)
    .bind(track.duration_secs)
    .bind(track.modified_at)
    .bind(&track.album_artist)
    .bind(track.disc_number)
    .bind(&track.barcode)
    .bind(&track.catalog_number)
    .bind(track.discogs_release_id)
    .bind(musicbrainz_json)
    .bind(track.year)
    .execute(&mut *conn)
    .await?;

    Ok(result.rows_affected())
}

pub async fn remove_track_by_path(pool: &SqlitePool, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    sqlx::query("DELETE FROM tracks WHERE path = ?1 COLLATE NOCASE")
        .bind(path_str)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Observed identifier helpers
// ---------------------------------------------------------------------------

fn split_ids(values: &[String]) -> (Vec<Mbid>, Vec<String>) {
    let mut valid: Vec<Mbid> = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    for value in values {
        match value.parse::<Mbid>() {
            Ok(id) => valid.push(id),
            Err(_) => invalid.push(value.clone()),
        }
    }
    valid.sort();
    valid.dedup();
    invalid.sort();
    invalid.dedup();
    (valid, invalid)
}

/// One valid observed identifier, or an explicit problem. Never silently drops
/// a conflicting value.
fn single_identifier(values: &[String], label: &str, path: &str) -> (Option<Mbid>, Vec<String>) {
    let (valid, invalid) = split_ids(values);
    let mut problems = Vec::new();
    if !invalid.is_empty() {
        problems.push(format!(
            "file '{path}' carries invalid {label} value(s): {}",
            invalid.join(", ")
        ));
    }
    if valid.len() > 1 {
        problems.push(format!(
            "file '{path}' carries {} different {label} values",
            valid.len()
        ));
    }
    let id = if valid.len() == 1 && invalid.is_empty() {
        Some(valid[0])
    } else {
        None
    };
    (id, problems)
}

/// Release-level evidence from every member file.
///
/// A release ID must be the single valid value on every member. A group ID may
/// narrow search with partial coverage, but never establishes an edition.
fn release_musicbrainz_evidence(
    members: &[&ObservedTrack],
) -> (Option<Mbid>, Option<Mbid>, Vec<String>) {
    let mut problems = Vec::new();
    let mut per_file_release: Vec<Vec<Mbid>> = Vec::new();
    let mut any_release_observation = false;
    let mut group_values: Vec<Mbid> = Vec::new();
    let mut group_invalid: Vec<String> = Vec::new();

    for track in members {
        let ids = track.musicbrainz();
        let (valid, invalid) = split_ids(&ids.release);
        if !valid.is_empty() || !invalid.is_empty() {
            any_release_observation = true;
        }
        if !invalid.is_empty() {
            problems.push(format!(
                "file '{}' carries invalid MusicBrainz release ID value(s): {}",
                track.path,
                invalid.join(", ")
            ));
        }
        per_file_release.push(valid);

        let (group_valid, group_bad) = split_ids(&ids.release_group);
        group_values.extend(group_valid);
        group_invalid.extend(group_bad);
    }

    if !group_invalid.is_empty() {
        group_invalid.sort();
        group_invalid.dedup();
        problems.push(format!(
            "invalid MusicBrainz release-group ID value(s): {}",
            group_invalid.join(", ")
        ));
    }

    let release_id = if per_file_release.iter().all(|values| values.len() == 1) {
        let first = per_file_release[0][0];
        if per_file_release.iter().all(|values| values[0] == first) {
            Some(first)
        } else {
            problems.push("local files disagree about the MusicBrainz release ID".to_string());
            None
        }
    } else if any_release_observation {
        problems
            .push("not every file carries exactly one valid MusicBrainz release ID".to_string());
        None
    } else {
        None
    };

    group_values.sort();
    group_values.dedup();
    let release_group_id = match group_values.len() {
        0 => None,
        1 => Some(group_values[0]),
        _ => {
            problems
                .push("local files disagree about the MusicBrainz release-group ID".to_string());
            None
        }
    };

    problems.sort();
    problems.dedup();

    (release_id, release_group_id, problems)
}

// ---------------------------------------------------------------------------
// Local release identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ObservedTrack {
    pub id: i64,
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i64>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub duration_secs: Option<f64>,
    pub barcode: Option<String>,
    pub catalog_number: Option<String>,
    pub discogs_release_id: Option<i64>,
    pub musicbrainz_ids_json: String,
    /// The local release this file was assigned to by the last rebuild.
    pub local_release_id: Option<i64>,
    /// `musicbrainz_ids_json`, decoded once when the row is loaded.
    #[sqlx(skip)]
    pub musicbrainz_ids: ObservedMusicBrainzIds,
}

impl ObservedTrack {
    pub fn musicbrainz(&self) -> &ObservedMusicBrainzIds {
        &self.musicbrainz_ids
    }
}

#[derive(Debug, Clone)]
pub struct GroupedRelease {
    pub directory: String,
    pub album_title: Option<String>,
    pub album_title_norm: String,
    pub album_artist: Option<String>,
    pub artist_kind: ArtistKind,
    pub year: Option<i64>,
    pub barcode: Option<String>,
    pub catalog_number: Option<String>,
    pub embedded_discogs_id: Option<u64>,
    pub embedded_musicbrainz_release_id: Option<Mbid>,
    pub embedded_musicbrainz_release_group_id: Option<Mbid>,
    pub musicbrainz_problems: Vec<String>,
    pub is_compilation: bool,
    pub has_album_tag: bool,
    pub track_count: i64,
    pub revision: String,
    pub track_ids: Vec<i64>,
}

fn directory_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The key two album titles or artist names are compared by.
///
/// Blank values have no identity. Names that normalize to nothing, such as
/// "÷" or "!!!", keep a distinct key built from the lowercased,
/// whitespace-collapsed original. That key starts with `~`, which
/// `normalize_text` never emits, so it cannot collide with a normal key.
fn identity_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = matcher::normalize_text(trimmed);
    if !normalized.is_empty() {
        return Some(normalized);
    }
    let collapsed = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    Some(format!("~{collapsed}"))
}

fn most_common_value(values: impl Iterator<Item = String>) -> Option<(String, String)> {
    let mut counts: HashMap<String, (usize, String)> = HashMap::new();
    for value in values {
        let Some(normalized) = identity_key(&value) else {
            continue;
        };
        let entry = counts
            .entry(normalized)
            .or_insert_with(|| (0, value.clone()));
        entry.0 += 1;
    }

    counts
        .into_iter()
        .max_by(|(key_a, (count_a, _)), (key_b, (count_b, _))| {
            count_a.cmp(count_b).then_with(|| key_b.cmp(key_a))
        })
        .map(|(normalized, (_, original))| (normalized, original))
}

/// A single shared value: every non-empty observation agrees. Disagreement is
/// a conflict, not a majority vote, because it can identify different editions.
fn consistent_value<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut distinct: Vec<String> = values
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() == 1 {
        distinct.pop()
    } else {
        None
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic revision over group membership and every observed
/// matching-relevant field, including the year and all four MusicBrainz
/// identifier lists. Modification time and accepted overlay values are
/// excluded. The `release-input-v2:` prefix makes the one-time revision
/// upgrade explicit.
fn revision_of(tracks: &[&ObservedTrack]) -> String {
    let mut rows: Vec<&ObservedTrack> = tracks.to_vec();
    rows.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });

    let mut payload = String::from("release-input-v2:");
    for track in rows {
        let ids = track.musicbrainz();
        let fields = [
            track.path.to_ascii_lowercase(),
            track.title.clone().unwrap_or_default(),
            track.artist.clone().unwrap_or_default(),
            track.album.clone().unwrap_or_default(),
            track.album_artist.clone().unwrap_or_default(),
            track.year.map(|year| year.to_string()).unwrap_or_default(),
            track
                .track_number
                .map(|n| n.to_string())
                .unwrap_or_default(),
            track.disc_number.map(|n| n.to_string()).unwrap_or_default(),
            track
                .duration_secs
                .map(|secs| format!("{:.3}", secs))
                .unwrap_or_default(),
            track.barcode.clone().unwrap_or_default(),
            track.catalog_number.clone().unwrap_or_default(),
            track
                .discogs_release_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            serde_json::to_string(&ids).unwrap_or_default(),
        ];
        payload.push_str(&fields.join("\u{1f}"));
        payload.push('\u{1e}');
    }

    format!("release-input-v2:{:016x}", fnv1a64(payload.as_bytes()))
}

/// Groups observed tracks into local releases.
///
/// Files group by directory plus normalized album title. Separate directories
/// never merge, even with identical tags, so compilations and copies in
/// different folders stay distinct. Disc subdirectories stay separate too.
pub fn group_tracks(tracks: &[ObservedTrack]) -> Vec<GroupedRelease> {
    let mut groups: BTreeMap<(String, String), Vec<&ObservedTrack>> = BTreeMap::new();

    for track in tracks {
        let directory = directory_of(&track.path);
        let album_title = track
            .album
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let album_title_norm = album_title.and_then(identity_key).unwrap_or_default();

        groups
            .entry((directory.to_ascii_lowercase(), album_title_norm))
            .or_default()
            .push(track);
    }

    groups
        .into_iter()
        .map(|((directory_key, album_title_norm), members)| {
            let directory = members
                .first()
                .map(|track| directory_of(&track.path))
                .unwrap_or(directory_key);

            let album_title = members
                .iter()
                .filter_map(|track| track.album.as_deref())
                .map(str::trim)
                .find(|title| identity_key(title).as_deref() == Some(album_title_norm.as_str()))
                .map(str::to_string);

            let album_artist_tag = most_common_value(
                members
                    .iter()
                    .filter_map(|track| track.album_artist.clone()),
            );

            let track_artists: Vec<String> = members
                .iter()
                .filter_map(|track| track.artist.clone())
                .filter(|artist| !artist.trim().is_empty())
                .collect();
            let distinct_track_artists: Vec<String> = {
                let mut distinct: Vec<String> = track_artists
                    .iter()
                    .filter_map(|artist| identity_key(artist))
                    .collect();
                distinct.sort();
                distinct.dedup();
                distinct
            };

            let (album_artist, artist_kind, is_compilation) = match album_artist_tag {
                Some((normalized, original)) => {
                    let compilation =
                        normalized.starts_with("various artist") || normalized == "various";
                    (
                        Some(original),
                        if compilation {
                            ArtistKind::VariousArtists
                        } else {
                            ArtistKind::AlbumArtist
                        },
                        compilation,
                    )
                }
                None => match distinct_track_artists.len() {
                    0 => (None, ArtistKind::Unknown, false),
                    1 => (
                        most_common_value(track_artists.iter().cloned())
                            .map(|(_, original)| original),
                        ArtistKind::CommonArtist,
                        false,
                    ),
                    _ => (
                        Some("Various Artists".to_string()),
                        ArtistKind::VariousArtists,
                        true,
                    ),
                },
            };

            let year = members
                .iter()
                .filter_map(|track| track.year)
                .fold(HashMap::<i64, usize>::new(), |mut counts, year| {
                    *counts.entry(year).or_default() += 1;
                    counts
                })
                .into_iter()
                .max_by(|(year_a, count_a), (year_b, count_b)| {
                    count_a.cmp(count_b).then_with(|| year_b.cmp(year_a))
                })
                .map(|(year, _)| year);

            let barcode =
                consistent_value(members.iter().filter_map(|track| track.barcode.as_deref()));
            let catalog_number = consistent_value(
                members
                    .iter()
                    .filter_map(|track| track.catalog_number.as_deref()),
            );
            let embedded_values: Vec<String> = members
                .iter()
                .filter_map(|track| track.discogs_release_id)
                .map(|id| id.to_string())
                .collect();
            let embedded_discogs_id = consistent_value(embedded_values.iter().map(String::as_str))
                .and_then(|value| value.parse::<u64>().ok());

            let (
                embedded_musicbrainz_release_id,
                embedded_musicbrainz_release_group_id,
                musicbrainz_problems,
            ) = release_musicbrainz_evidence(&members);

            let revision = revision_of(&members);
            let has_album_tag = !album_title_norm.is_empty();

            let mut track_ids: Vec<i64> = members.iter().map(|track| track.id).collect();
            track_ids.sort_unstable();

            GroupedRelease {
                directory,
                album_title,
                album_title_norm,
                album_artist,
                artist_kind,
                year,
                barcode,
                catalog_number,
                embedded_discogs_id,
                embedded_musicbrainz_release_id,
                embedded_musicbrainz_release_group_id,
                musicbrainz_problems,
                is_compilation,
                has_album_tag,
                track_count: members.len() as i64,
                revision,
                track_ids,
            }
        })
        .collect()
}

/// Normalizes a scope root for the SQL path filters: forward slashes, no
/// trailing slash. `None` means no filtering, which is also what an empty root
/// (the filesystem root) means.
fn scope_param(root: &Path) -> Option<String> {
    let normalized = root.to_string_lossy().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

async fn observed_tracks(
    conn: &mut SqliteConnection,
    scope: Option<&str>,
) -> Result<Vec<ObservedTrack>> {
    // Whole path components only (`/music/Blue` never matches
    // `/music/Bluegrass`), compared case-insensitively like the path columns.
    let mut rows = sqlx::query_as::<_, ObservedTrack>(
        "SELECT t.id AS id, t.path AS path, t.title AS title, t.artist AS artist,
                a.title AS album, t.album_artist AS album_artist, t.year AS year,
                t.track_number AS track_number, t.disc_number AS disc_number,
                t.duration_secs AS duration_secs, t.barcode AS barcode,
                t.catalog_number AS catalog_number, t.discogs_release_id AS discogs_release_id,
                t.musicbrainz_ids_json AS musicbrainz_ids_json,
                t.local_release_id AS local_release_id
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         WHERE (?1 IS NULL
                OR substr(replace(t.path, '\\', '/'), 1, length(?1) + 1) = (?1 || '/') COLLATE NOCASE)
         ORDER BY t.path",
    )
    .bind(scope)
    .fetch_all(conn)
    .await?;

    // Decode the identifier JSON once per file, not once per use.
    for row in &mut rows {
        row.musicbrainz_ids = serde_json::from_str(&row.musicbrainz_ids_json).unwrap_or_default();
    }

    Ok(rows)
}

/// True when `candidate` is `root` or lives under it, comparing whole path
/// components only so `/music/Blue` never matches `/music/Bluegrass`.
pub fn path_within(candidate: &str, root: &Path) -> bool {
    let normalize = |value: &str| {
        value
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    let candidate = normalize(candidate);
    let root = normalize(&root.to_string_lossy());
    if root.is_empty() {
        return true;
    }
    candidate == root || candidate.starts_with(&format!("{root}/"))
}

/// What one `rebuild_local_releases` call did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RebuildSummary {
    /// Local releases that exist in the scope after the rebuild.
    pub groups: usize,
    pub created: usize,
    /// Existing releases whose stored fields or members changed.
    pub updated: usize,
    /// Releases that kept their ID while their album title changed.
    pub rekeyed: usize,
    pub unchanged: usize,
    /// Releases deleted because no group claims them any more.
    pub removed: usize,
    /// Accepted matches newly marked stale.
    pub newly_stale: usize,
    /// Accepted matches deleted together with their release.
    pub dropped_matches: usize,
}

impl RebuildSummary {
    /// True when the rebuild wrote anything to the release tables.
    pub fn changed(&self) -> bool {
        self.created + self.updated + self.rekeyed + self.removed > 0
    }
}

/// Every persisted column a rebuild derives from the files. Comparing the
/// stored and the freshly grouped value decides whether a row needs a write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseFields {
    directory: String,
    album_title_norm: String,
    album_title: Option<String>,
    album_artist: Option<String>,
    artist_kind: String,
    year: Option<i64>,
    barcode: Option<String>,
    catalog_number: Option<String>,
    embedded_discogs_id: Option<i64>,
    is_compilation: bool,
    has_album_tag: bool,
    track_count: i64,
    revision: String,
    embedded_musicbrainz_release_id: Option<String>,
    embedded_musicbrainz_release_group_id: Option<String>,
    musicbrainz_problems_json: String,
}

impl ReleaseFields {
    fn of_group(group: &GroupedRelease) -> Result<Self> {
        Ok(Self {
            directory: group.directory.clone(),
            album_title_norm: group.album_title_norm.clone(),
            album_title: group.album_title.clone(),
            album_artist: group.album_artist.clone(),
            artist_kind: group.artist_kind.as_str().to_string(),
            year: group.year,
            barcode: group.barcode.clone(),
            catalog_number: group.catalog_number.clone(),
            embedded_discogs_id: group.embedded_discogs_id.map(|id| id as i64),
            is_compilation: group.is_compilation,
            has_album_tag: group.has_album_tag,
            track_count: group.track_count,
            revision: group.revision.clone(),
            embedded_musicbrainz_release_id: group
                .embedded_musicbrainz_release_id
                .map(|id| id.to_string()),
            embedded_musicbrainz_release_group_id: group
                .embedded_musicbrainz_release_group_id
                .map(|id| id.to_string()),
            musicbrainz_problems_json: serde_json::to_string(&group.musicbrainz_problems)?,
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StoredReleaseRow {
    id: i64,
    directory_path: String,
    album_title_norm: String,
    album_title: Option<String>,
    album_artist: Option<String>,
    artist_kind: String,
    year: Option<i64>,
    barcode: Option<String>,
    catalog_number: Option<String>,
    embedded_discogs_id: Option<i64>,
    is_compilation: bool,
    has_album_tag: bool,
    track_count: i64,
    source_revision: String,
    embedded_musicbrainz_release_id: Option<String>,
    embedded_musicbrainz_release_group_id: Option<String>,
    musicbrainz_problems_json: String,
    has_match: bool,
}

/// A persisted local release as seen by the rebuild planner.
#[derive(Debug, Clone)]
struct StoredRelease {
    id: i64,
    fields: ReleaseFields,
    has_match: bool,
}

impl From<StoredReleaseRow> for StoredRelease {
    fn from(row: StoredReleaseRow) -> Self {
        Self {
            id: row.id,
            has_match: row.has_match,
            fields: ReleaseFields {
                directory: row.directory_path,
                album_title_norm: row.album_title_norm,
                album_title: row.album_title,
                album_artist: row.album_artist,
                artist_kind: row.artist_kind,
                year: row.year,
                barcode: row.barcode,
                catalog_number: row.catalog_number,
                embedded_discogs_id: row.embedded_discogs_id,
                is_compilation: row.is_compilation,
                has_album_tag: row.has_album_tag,
                track_count: row.track_count,
                revision: row.source_revision,
                embedded_musicbrainz_release_id: row.embedded_musicbrainz_release_id,
                embedded_musicbrainz_release_group_id: row.embedded_musicbrainz_release_group_id,
                musicbrainz_problems_json: row.musicbrainz_problems_json,
            },
        }
    }
}

/// Loads the stored releases whose directory is the scope or under it.
async fn existing_releases(
    conn: &mut SqliteConnection,
    scope: Option<&str>,
) -> Result<Vec<StoredRelease>> {
    let rows = sqlx::query_as::<_, StoredReleaseRow>(
        "SELECT lr.id, lr.directory_path, lr.album_title_norm, lr.album_title, lr.album_artist,
                lr.artist_kind, lr.year, lr.barcode, lr.catalog_number, lr.embedded_discogs_id,
                lr.is_compilation, lr.has_album_tag, lr.track_count, lr.source_revision,
                lr.embedded_musicbrainz_release_id, lr.embedded_musicbrainz_release_group_id,
                lr.musicbrainz_problems_json,
                EXISTS (SELECT 1 FROM release_matches m WHERE m.local_release_id = lr.id)
                    AS has_match
         FROM local_releases lr
         WHERE (?1 IS NULL
                OR replace(lr.directory_path, '\\', '/') = ?1 COLLATE NOCASE
                OR substr(replace(lr.directory_path, '\\', '/'), 1, length(?1) + 1)
                    = (?1 || '/') COLLATE NOCASE)
         ORDER BY lr.id",
    )
    .bind(scope)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(StoredRelease::from).collect())
}

type ReleaseKey = (String, String);

/// Stored releases are keyed by directory (ASCII case-insensitive, as the
/// column's collation compares) and normalized album title.
fn release_key(directory: &str, album_title_norm: &str) -> ReleaseKey {
    (directory.to_ascii_lowercase(), album_title_norm.to_string())
}

/// What a freshly grouped release is stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The stored row with the same key.
    Existing(i64),
    /// A stored row whose files all moved to this group under a new title. It
    /// keeps its ID, and with it the accepted match.
    Rekey(i64),
    New,
}

#[derive(Debug, PartialEq, Eq)]
struct RebuildPlan {
    /// One target per group, in group order.
    targets: Vec<Target>,
    /// Stored rows no group claims, in ID order.
    removed: Vec<i64>,
}

/// Decides which stored release each group becomes, without touching the
/// database.
///
/// 1. A group whose key matches a stored row is that row.
/// 2. A stored row whose key no group has is "emptied".
/// 3. An emptied row whose members all moved into one group that has no row of
///    its own is a re-key candidate for that group (an album retag). Several
///    candidates (a merge) compete: a row with an accepted match wins, then the
///    one with more members, then the lower ID.
/// 4. Every other emptied row is removed: merge losers, rows whose files split
///    across several groups, and rows with no surviving files.
///
/// The `UNIQUE(directory_path, album_title_norm)` constraint cannot fire while
/// applying the plan. A re-key target key is held by no row, or step 1 would
/// have matched it, and an emptied row's old key is no group's key.
fn plan_rebuild(
    groups: &[GroupedRelease],
    stored: &[StoredRelease],
    members: &HashMap<i64, Vec<i64>>,
) -> RebuildPlan {
    let group_keys: Vec<ReleaseKey> = groups
        .iter()
        .map(|group| release_key(&group.directory, &group.album_title_norm))
        .collect();
    let stored_by_key: HashMap<ReleaseKey, i64> = stored
        .iter()
        .map(|row| {
            (
                release_key(&row.fields.directory, &row.fields.album_title_norm),
                row.id,
            )
        })
        .collect();

    let mut targets: Vec<Target> = group_keys
        .iter()
        .map(|key| {
            stored_by_key
                .get(key)
                .map_or(Target::New, |id| Target::Existing(*id))
        })
        .collect();

    let claimed: std::collections::HashSet<&ReleaseKey> = group_keys.iter().collect();
    let group_of_track: HashMap<i64, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.track_ids.iter().map(move |id| (*id, index)))
        .collect();

    let mut removed: Vec<i64> = Vec::new();
    let mut candidates: BTreeMap<usize, Vec<&StoredRelease>> = BTreeMap::new();
    for row in stored {
        let key = release_key(&row.fields.directory, &row.fields.album_title_norm);
        if claimed.contains(&key) {
            continue;
        }
        let row_members = members.get(&row.id).map(Vec::as_slice).unwrap_or_default();
        let mut destinations = row_members
            .iter()
            .map(|track_id| group_of_track.get(track_id).copied());
        let destination = match destinations.next() {
            Some(Some(first)) if destinations.all(|next| next == Some(first)) => Some(first),
            _ => None,
        };
        match destination {
            Some(index) if targets[index] == Target::New => {
                candidates.entry(index).or_default().push(row);
            }
            _ => removed.push(row.id),
        }
    }

    for (index, mut rows) in candidates {
        rows.sort_by(|a, b| {
            let member_count = |row: &StoredRelease| members.get(&row.id).map_or(0, Vec::len);
            b.has_match
                .cmp(&a.has_match)
                .then_with(|| member_count(b).cmp(&member_count(a)))
                .then_with(|| a.id.cmp(&b.id))
        });
        targets[index] = Target::Rekey(rows[0].id);
        removed.extend(rows[1..].iter().map(|row| row.id));
    }
    removed.sort_unstable();

    RebuildPlan { targets, removed }
}

/// Recomputes grouping for the selected scope and persists it. Existing track
/// IDs are preserved, and unchanged revisions keep accepted matches intact.
///
/// Rows are only written when something differs, so rebuilding an unchanged
/// library writes nothing. A release whose files were all retagged to one new
/// album title keeps its ID and accepted match, marked stale. Splitting a
/// release across several new titles still drops its match; the summary
/// reports it in `dropped_matches`.
pub async fn rebuild_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<RebuildSummary> {
    let scope = scope.and_then(scope_param);
    let scope = scope.as_deref();

    // Observe and write inside one write transaction, so a concurrent writer
    // can never leave this rebuild grouping stale data.
    let mut tx = begin_write(pool).await?;
    let tracks = observed_tracks(&mut tx, scope).await?;
    let groups = group_tracks(&tracks);
    let stored = existing_releases(&mut tx, scope).await?;

    let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
    for track in &tracks {
        if let Some(release_id) = track.local_release_id {
            members.entry(release_id).or_default().push(track.id);
        }
    }
    for ids in members.values_mut() {
        ids.sort_unstable();
    }

    let plan = plan_rebuild(&groups, &stored, &members);
    let stored_by_id: HashMap<i64, &StoredRelease> =
        stored.iter().map(|row| (row.id, row)).collect();

    let mut summary = RebuildSummary {
        groups: groups.len(),
        ..RebuildSummary::default()
    };

    for (group, target) in groups.iter().zip(&plan.targets) {
        let fields = ReleaseFields::of_group(group)?;
        let (id, previous_revision) = match *target {
            Target::Existing(id) => {
                let row = stored_by_id[&id];
                let same_members = members.get(&id).map(Vec::as_slice).unwrap_or_default()
                    == group.track_ids.as_slice();
                if row.fields == fields && same_members {
                    summary.unchanged += 1;
                    continue;
                }
                update_release(&mut tx, id, &fields).await?;
                summary.updated += 1;
                (id, Some(row.fields.revision.as_str()))
            }
            Target::Rekey(id) => {
                update_release(&mut tx, id, &fields).await?;
                summary.rekeyed += 1;
                (id, Some(stored_by_id[&id].fields.revision.as_str()))
            }
            Target::New => {
                let id = insert_release(&mut tx, &fields).await?;
                summary.created += 1;
                (id, None)
            }
        };

        if previous_revision.is_some_and(|previous| previous != fields.revision) {
            let marked = sqlx::query(
                "UPDATE release_matches SET stale = 1
                 WHERE local_release_id = ?1 AND stale = 0",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            summary.newly_stale += marked.rows_affected() as usize;
        }

        let track_ids = serde_json::to_string(&group.track_ids)?;
        sqlx::query(
            "UPDATE tracks SET local_release_id = ?1
             WHERE id IN (SELECT value FROM json_each(?2)) AND local_release_id IS NOT ?1",
        )
        .bind(id)
        .bind(&track_ids)
        .execute(&mut *tx)
        .await?;
        // A file that moved here may still carry an overlay row from the match
        // of the release it left.
        sqlx::query(
            "DELETE FROM release_track_matches
             WHERE track_id IN (SELECT value FROM json_each(?2))
               AND match_id NOT IN (SELECT id FROM release_matches WHERE local_release_id = ?1)",
        )
        .bind(id)
        .bind(&track_ids)
        .execute(&mut *tx)
        .await?;
    }

    if !plan.removed.is_empty() {
        summary.removed = plan.removed.len();
        summary.dropped_matches = plan
            .removed
            .iter()
            .filter(|id| stored_by_id[*id].has_match)
            .count();
        sqlx::query("DELETE FROM local_releases WHERE id IN (SELECT value FROM json_each(?1))")
            .bind(serde_json::to_string(&plan.removed)?)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    Ok(summary)
}

async fn insert_release(conn: &mut SqliteConnection, fields: &ReleaseFields) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO local_releases (
            directory_path, album_title_norm, album_title, album_artist, artist_kind, year,
            barcode, catalog_number, embedded_discogs_id, is_compilation, has_album_tag,
            track_count, source_revision, status, embedded_musicbrainz_release_id,
            embedded_musicbrainz_release_group_id, musicbrainz_problems_json
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         RETURNING id",
    )
    .bind(&fields.directory)
    .bind(&fields.album_title_norm)
    .bind(&fields.album_title)
    .bind(&fields.album_artist)
    .bind(&fields.artist_kind)
    .bind(fields.year)
    .bind(&fields.barcode)
    .bind(&fields.catalog_number)
    .bind(fields.embedded_discogs_id)
    .bind(fields.is_compilation)
    .bind(fields.has_album_tag)
    .bind(fields.track_count)
    .bind(&fields.revision)
    .bind(STATUS_UNMATCHED)
    .bind(&fields.embedded_musicbrainz_release_id)
    .bind(&fields.embedded_musicbrainz_release_group_id)
    .bind(&fields.musicbrainz_problems_json)
    .fetch_one(conn)
    .await?;
    Ok(id)
}

/// Rewrites every derived column, the key included, so one statement serves
/// both a changed group and a re-keyed one. A matched or stale release whose
/// revision changed becomes stale; an unmatched one stays unmatched.
async fn update_release(
    conn: &mut SqliteConnection,
    id: i64,
    fields: &ReleaseFields,
) -> Result<()> {
    sqlx::query(
        "UPDATE local_releases SET
            directory_path = ?2,
            album_title_norm = ?3,
            album_title = ?4,
            album_artist = ?5,
            artist_kind = ?6,
            year = ?7,
            barcode = ?8,
            catalog_number = ?9,
            embedded_discogs_id = ?10,
            is_compilation = ?11,
            has_album_tag = ?12,
            track_count = ?13,
            embedded_musicbrainz_release_id = ?15,
            embedded_musicbrainz_release_group_id = ?16,
            musicbrainz_problems_json = ?17,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
            status = CASE
                WHEN source_revision <> ?14 AND status IN ('matched', 'stale') THEN 'stale'
                ELSE status
            END,
            source_revision = ?14
         WHERE id = ?1",
    )
    .bind(id)
    .bind(&fields.directory)
    .bind(&fields.album_title_norm)
    .bind(&fields.album_title)
    .bind(&fields.album_artist)
    .bind(&fields.artist_kind)
    .bind(fields.year)
    .bind(&fields.barcode)
    .bind(&fields.catalog_number)
    .bind(fields.embedded_discogs_id)
    .bind(fields.is_compilation)
    .bind(fields.has_album_tag)
    .bind(fields.track_count)
    .bind(&fields.revision)
    .bind(&fields.embedded_musicbrainz_release_id)
    .bind(&fields.embedded_musicbrainz_release_group_id)
    .bind(&fields.musicbrainz_problems_json)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LocalReleaseRow {
    id: i64,
    directory_path: String,
    album_title: Option<String>,
    album_artist: Option<String>,
    artist_kind: String,
    year: Option<i64>,
    barcode: Option<String>,
    catalog_number: Option<String>,
    embedded_discogs_id: Option<i64>,
    embedded_musicbrainz_release_id: Option<String>,
    embedded_musicbrainz_release_group_id: Option<String>,
    musicbrainz_problems_json: String,
    is_compilation: bool,
    has_album_tag: bool,
    source_revision: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LocalTrackRow {
    id: i64,
    path: String,
    title: Option<String>,
    disc_number: Option<i64>,
    track_number: Option<i64>,
    duration_secs: Option<f64>,
    local_release_id: i64,
    musicbrainz_ids_json: String,
}

impl LocalTrackRow {
    fn into_local_track(self) -> LocalTrack {
        let ids: ObservedMusicBrainzIds =
            serde_json::from_str(&self.musicbrainz_ids_json).unwrap_or_default();
        let (release_track_id, mut problems) = single_identifier(
            &ids.release_track,
            "MusicBrainz release-track ID",
            &self.path,
        );
        let (recording_id, recording_problems) =
            single_identifier(&ids.recording, "MusicBrainz recording ID", &self.path);
        problems.extend(recording_problems);
        problems.sort();
        problems.dedup();

        LocalTrack {
            id: self.id,
            path: self.path,
            title: self.title,
            disc: self.disc_number,
            track_number: self.track_number,
            duration_secs: self.duration_secs,
            release_track_id,
            recording_id,
            identifier_problems: problems,
        }
    }
}

/// Puts a release's tracks in the one order every consumer indexes by: disc
/// (missing counts as 1), track number (missing sorts last), then
/// case-insensitive path. Mapping pairs are positions in this order, so the
/// loader and the accept transaction must agree on it.
pub fn order_release_tracks(tracks: &mut [LocalTrack]) {
    tracks.sort_by(|a, b| {
        a.disc
            .unwrap_or(1)
            .cmp(&b.disc.unwrap_or(1))
            .then(
                a.track_number
                    .unwrap_or(i64::MAX)
                    .cmp(&b.track_number.unwrap_or(i64::MAX)),
            )
            .then_with(|| {
                a.path
                    .to_ascii_lowercase()
                    .cmp(&b.path.to_ascii_lowercase())
            })
    });
}

fn parse_musicbrainz_problems(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Loads persisted local releases (with their tracks) for matching.
pub async fn load_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalRelease>> {
    let release_rows = sqlx::query_as::<_, LocalReleaseRow>(
        "SELECT id, directory_path, album_title, album_artist, artist_kind, year, barcode,
                catalog_number, embedded_discogs_id, embedded_musicbrainz_release_id,
                embedded_musicbrainz_release_group_id, musicbrainz_problems_json,
                is_compilation, has_album_tag, source_revision
         FROM local_releases
         ORDER BY directory_path, album_title_norm",
    )
    .fetch_all(pool)
    .await?;

    let track_rows = sqlx::query_as::<_, LocalTrackRow>(
        "SELECT id, path, title, disc_number, track_number, duration_secs, local_release_id,
                musicbrainz_ids_json
         FROM tracks
         WHERE local_release_id IS NOT NULL
         ORDER BY path",
    )
    .fetch_all(pool)
    .await?;

    let mut tracks_by_release: HashMap<i64, Vec<LocalTrack>> = HashMap::new();
    for row in track_rows {
        tracks_by_release
            .entry(row.local_release_id)
            .or_default()
            .push(row.into_local_track());
    }

    let mut releases = Vec::new();
    for row in release_rows {
        if let Some(scope) = scope
            && !path_within(&row.directory_path, scope)
        {
            continue;
        }

        let mut tracks = tracks_by_release.remove(&row.id).unwrap_or_default();
        order_release_tracks(&mut tracks);

        let embedded_evidence = EmbeddedReleaseEvidence {
            release_id: row
                .embedded_musicbrainz_release_id
                .as_deref()
                .and_then(|value| value.parse::<Mbid>().ok()),
            release_group_id: row
                .embedded_musicbrainz_release_group_id
                .as_deref()
                .and_then(|value| value.parse::<Mbid>().ok()),
            problems: parse_musicbrainz_problems(&row.musicbrainz_problems_json),
        };

        releases.push(LocalRelease {
            id: row.id,
            directory: row.directory_path,
            album_title: row.album_title,
            album_artist: row.album_artist,
            artist_kind: ArtistKind::parse(&row.artist_kind),
            year: row.year,
            barcode: row.barcode,
            catalog_number: row.catalog_number,
            embedded_discogs_id: row.embedded_discogs_id.map(|id| id as u64),
            embedded_evidence,
            is_compilation: row.is_compilation,
            has_album_tag: row.has_album_tag,
            revision: row.source_revision,
            tracks,
        });
    }

    Ok(releases)
}

/// Provider of every accepted overlay, keyed by local release.
pub async fn accepted_matches_by_release(
    pool: &SqlitePool,
) -> Result<HashMap<i64, Option<ProviderKind>>> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT local_release_id, provider FROM release_matches",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, provider)| (id, ProviderKind::parse(&provider)))
        .collect())
}

pub async fn status_by_id(pool: &SqlitePool) -> Result<HashMap<i64, String>> {
    let rows = sqlx::query_as::<_, (i64, String)>("SELECT id, status FROM local_releases")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

#[derive(Debug, Clone)]
pub struct LocalReleaseStatus {
    pub id: i64,
    pub directory: String,
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub track_count: i64,
    pub status: String,
    pub provider: Option<ProviderKind>,
    pub remote_release_id: Option<String>,
    pub remote_url: Option<String>,
    pub remote_title: Option<String>,
    pub match_scope: Option<String>,
    pub stale: bool,
    pub queued_providers: Vec<ProviderKind>,
}

impl LocalReleaseStatus {
    pub fn queued(&self) -> bool {
        !self.queued_providers.is_empty()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StatusRow {
    id: i64,
    directory_path: String,
    album_title: Option<String>,
    album_artist: Option<String>,
    track_count: i64,
    status: String,
    provider: Option<String>,
    remote_release_id: Option<String>,
    remote_url: Option<String>,
    remote_title: Option<String>,
    match_scope: Option<String>,
    stale: Option<i64>,
}

pub async fn list_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalReleaseStatus>> {
    let rows = sqlx::query_as::<_, StatusRow>(
        "SELECT lr.id, lr.directory_path, lr.album_title, lr.album_artist, lr.track_count,
                lr.status, m.provider AS provider, m.remote_release_id AS remote_release_id,
                m.release_url AS remote_url, m.title AS remote_title, m.stale AS stale, m.match_scope
         FROM local_releases lr
         LEFT JOIN release_matches m ON m.local_release_id = lr.id
         ORDER BY lr.directory_path, lr.album_title_norm",
    )
    .fetch_all(pool)
    .await?;

    let queued = sqlx::query_as::<_, (i64, String)>(
        "SELECT local_release_id, provider FROM match_reviews ORDER BY local_release_id, provider",
    )
    .fetch_all(pool)
    .await?;
    let mut queued_by_release: HashMap<i64, Vec<ProviderKind>> = HashMap::new();
    for (local_release_id, provider) in queued {
        if let Some(provider) = ProviderKind::parse(&provider) {
            queued_by_release
                .entry(local_release_id)
                .or_default()
                .push(provider);
        }
    }

    Ok(rows
        .into_iter()
        .filter(|row| match scope {
            Some(scope) => path_within(&row.directory_path, scope),
            None => true,
        })
        .map(|row| LocalReleaseStatus {
            id: row.id,
            directory: row.directory_path,
            album_title: row.album_title,
            album_artist: row.album_artist,
            track_count: row.track_count,
            status: row.status,
            provider: row.provider.as_deref().and_then(ProviderKind::parse),
            remote_release_id: row.remote_release_id,
            remote_url: row.remote_url,
            remote_title: row.remote_title,
            match_scope: row.match_scope,
            stale: row.stale.unwrap_or(0) != 0,
            queued_providers: queued_by_release.remove(&row.id).unwrap_or_default(),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Accepted overlay and review queue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceMethod {
    Automatic,
    Manual,
}

impl AcceptanceMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredCandidate {
    pub release: RemoteRelease,
    pub mapping: Mapping,
    pub score: matcher::ScoreBreakdown,
    pub auto_accept: bool,
    #[serde(default)]
    pub match_scope: MatchScope,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub evidence: Vec<String>,
    /// Optional advisory evidence, independent of deterministic acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_advice: Option<crate::clef::Advice>,
}

/// Immutable inputs an acceptance decision was based on. Reloading them inside
/// the transaction prevents a stale review from replacing another decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchGuard {
    pub source_revision: String,
    pub accepted_match_id: Option<i64>,
}

/// The local release an acceptance decision was based on no longer looks the
/// way it did when it was loaded: it was removed, regrouped, or its files
/// changed. The caller should re-run matching rather than retry.
#[derive(Debug)]
pub struct SnapshotChanged {
    pub local_release_id: i64,
    pub reason: String,
}

impl std::fmt::Display for SnapshotChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for SnapshotChanged {}

fn snapshot_changed(local_release_id: i64, reason: String) -> anyhow::Error {
    SnapshotChanged {
        local_release_id,
        reason,
    }
    .into()
}

pub async fn load_match_guard(pool: &SqlitePool, local_id: i64) -> Result<MatchGuard> {
    let revision: Option<String> =
        sqlx::query_scalar("SELECT source_revision FROM local_releases WHERE id = ?1")
            .bind(local_id)
            .fetch_optional(pool)
            .await?;
    let Some(source_revision) = revision else {
        return Err(snapshot_changed(
            local_id,
            format!("local release {local_id} no longer exists"),
        ));
    };
    let accepted_match_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM release_matches WHERE local_release_id = ?1")
            .bind(local_id)
            .fetch_optional(pool)
            .await?;

    Ok(MatchGuard {
        source_revision,
        accepted_match_id,
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyRemoteTrack {
    #[serde(default)]
    position: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    duration_secs: Option<f64>,
    #[serde(default = "default_entry_type")]
    entry_type: matcher::TrackEntryType,
}

fn default_entry_type() -> matcher::TrackEntryType {
    matcher::TrackEntryType::Track
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyRemoteRelease {
    id: u64,
    #[serde(default)]
    master_id: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    catalog_numbers: Vec<String>,
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    barcodes: Vec<String>,
    #[serde(default)]
    tracklist: Vec<LegacyRemoteTrack>,
    #[serde(default)]
    url: String,
}

impl From<LegacyRemoteRelease> for RemoteRelease {
    fn from(legacy: LegacyRemoteRelease) -> Self {
        Self {
            id: ReleaseId::Discogs(legacy.id),
            group_id: legacy.master_id.map(GroupId::DiscogsMaster),
            title: legacy.title,
            artists: legacy.artists,
            year: legacy.year,
            country: legacy.country,
            labels: legacy.labels,
            catalog_numbers: legacy.catalog_numbers,
            formats: legacy.formats,
            barcodes: legacy.barcodes,
            tracklist: legacy
                .tracklist
                .into_iter()
                .map(|track| RemoteTrack {
                    position: track.position,
                    title: track.title,
                    artists: track.artists,
                    duration_secs: track.duration_secs,
                    entry_type: track.entry_type,
                    medium_position: None,
                    track_position: None,
                    release_track_id: None,
                    recording_id: None,
                    artist_credit: None,
                })
                .collect(),
            url: legacy.url,
            lookup_aliases: Vec::new(),
            artist_credit: None,
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyStoredCandidate {
    release: LegacyRemoteRelease,
    mapping: Mapping,
    score: matcher::ScoreBreakdown,
    #[serde(default)]
    auto_accept: bool,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    evidence: Vec<String>,
}

/// Decodes a stored release snapshot. Version 1 is the historical numeric
/// Discogs shape; version 2 is provider-qualified.
pub fn decode_release_snapshot(
    version: i64,
    provider: ProviderKind,
    json: &str,
) -> Result<RemoteRelease> {
    match version {
        1 => {
            if provider != ProviderKind::Discogs {
                bail!("snapshot version 1 is only valid for Discogs rows");
            }
            let legacy: LegacyRemoteRelease = serde_json::from_str(json)?;
            Ok(legacy.into())
        }
        2 => {
            let release: RemoteRelease = serde_json::from_str(json)?;
            if release.provider() != provider {
                bail!(
                    "snapshot says {} but the row is {}",
                    release.provider(),
                    provider
                );
            }
            Ok(release)
        }
        other => bail!("unknown snapshot version {other}"),
    }
}

/// Decodes a stored review candidate list, including version-1 Discogs rows.
pub fn decode_stored_candidates(
    version: i64,
    provider: ProviderKind,
    json: &str,
) -> Result<Vec<StoredCandidate>> {
    match version {
        1 => {
            if provider != ProviderKind::Discogs {
                bail!("review version 1 is only valid for Discogs rows");
            }
            let legacy: Vec<LegacyStoredCandidate> = serde_json::from_str(json)?;
            Ok(legacy
                .into_iter()
                .map(|candidate| StoredCandidate {
                    release: candidate.release.into(),
                    mapping: candidate.mapping,
                    score: candidate.score,
                    auto_accept: candidate.auto_accept,
                    match_scope: MatchScope::Edition,
                    blockers: candidate.blockers,
                    warnings: candidate.warnings,
                    evidence: candidate.evidence,
                    ai_advice: None,
                })
                .collect())
        }
        2 => Ok(serde_json::from_str(json)?),
        other => bail!("unknown review snapshot version {other}"),
    }
}

/// Remote values that are missing stay absent so the overlay falls back to the
/// scanned tag instead of showing an empty string.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Applies an accepted match atomically. The whole overlay is replaced, so a
/// failure leaves no partial state behind.
pub async fn accept_match(
    pool: &SqlitePool,
    release: &LocalRelease,
    candidate: &Candidate,
    method: AcceptanceMethod,
    match_scope: MatchScope,
    guard: &MatchGuard,
) -> Result<()> {
    accept_recorded_match(pool, release, candidate, method, match_scope, guard, None).await
}

/// Applies an AI-supported album overlay while retaining the decision evidence.
pub async fn accept_ai_match(
    pool: &SqlitePool,
    release: &LocalRelease,
    candidate: &Candidate,
    guard: &MatchGuard,
    advice: &crate::clef::Advice,
) -> Result<()> {
    accept_recorded_match(
        pool,
        release,
        candidate,
        AcceptanceMethod::Automatic,
        MatchScope::Album,
        guard,
        Some(advice),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn accept_recorded_match(
    pool: &SqlitePool,
    release: &LocalRelease,
    candidate: &Candidate,
    method: AcceptanceMethod,
    match_scope: MatchScope,
    guard: &MatchGuard,
    ai_advice: Option<&crate::clef::Advice>,
) -> Result<()> {
    let local_release_id = release.id;
    if !candidate.mapping.complete {
        bail!("refusing to accept an incomplete track mapping");
    }
    if candidate.release.has_unsupported_entries() {
        bail!("refusing to accept a release with unsupported structure");
    }
    if candidate.mapping.pairs.len() != release.tracks.len() {
        return Err(snapshot_changed(
            local_release_id,
            format!(
                "local release {local_release_id} has {} tracks but the mapping covers {}",
                release.tracks.len(),
                candidate.mapping.pairs.len()
            ),
        ));
    }
    if guard.source_revision != release.revision {
        return Err(snapshot_changed(
            local_release_id,
            format!(
                "local release {local_release_id} changed since it was evaluated (revision {} != {})",
                release.revision, guard.source_revision
            ),
        ));
    }

    let mut tx = begin_write(pool).await?;

    let current: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT lr.source_revision,
                (SELECT id FROM release_matches m WHERE m.local_release_id = lr.id)
         FROM local_releases lr WHERE lr.id = ?1",
    )
    .bind(local_release_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((current_revision, current_match_id)) = current else {
        return Err(snapshot_changed(
            local_release_id,
            format!("local release {local_release_id} no longer exists"),
        ));
    };
    if current_revision != guard.source_revision {
        return Err(snapshot_changed(
            local_release_id,
            format!(
                "local release {local_release_id} changed since it was evaluated (revision {current_revision} != {})",
                guard.source_revision
            ),
        ));
    }
    if current_match_id != guard.accepted_match_id {
        bail!(
            "local release {local_release_id} was accepted by another decision while this one was pending"
        );
    }

    // Revisions hash tag values and paths, not track IDs. A file that was
    // removed and re-added keeps the revision but gets a new ID, and the
    // mapping indexes the old snapshot, so membership must match exactly.
    let mut current_members: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM tracks WHERE local_release_id = ?1")
            .bind(local_release_id)
            .fetch_all(&mut *tx)
            .await?;
    current_members.sort_unstable();
    let mut snapshot_members: Vec<i64> = release.tracks.iter().map(|track| track.id).collect();
    snapshot_members.sort_unstable();
    if current_members != snapshot_members {
        return Err(snapshot_changed(
            local_release_id,
            format!("local release {local_release_id} changed membership since it was evaluated"),
        ));
    }

    let mut seen_ordinals: Vec<usize> = Vec::new();
    for (_, remote_index) in candidate.mapping.assigned_pairs() {
        seen_ordinals.push(remote_index);
    }
    seen_ordinals.sort_unstable();
    seen_ordinals.dedup();
    if seen_ordinals.len() != candidate.mapping.assigned_count() {
        bail!("refusing to accept a mapping that assigns one remote track twice");
    }

    let remote = &candidate.release;
    let edition = (match_scope == MatchScope::Edition).then_some(remote);
    let provider = remote.provider();
    let mut metadata = serde_json::to_value(remote)?;
    let algorithm_version = if let Some(advice) = ai_advice {
        metadata["radish_ai_decision"] = serde_json::json!({
            "policy_version": "clef-album-v1", "advice": advice,
        });
        format!("{provider}-clef-album-v1")
    } else {
        matcher::algorithm_version(provider).to_string()
    };
    let metadata_json = serde_json::to_string(&metadata)?;

    sqlx::query("DELETE FROM release_matches WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM match_reviews WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    // A member may still carry an overlay row from a match for a release it
    // used to belong to; `track_id` is unique, so clear those before inserting.
    let member_ids = serde_json::to_string(
        &release
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
    )?;
    sqlx::query(
        "DELETE FROM release_track_matches WHERE track_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(&member_ids)
    .execute(&mut *tx)
    .await?;

    let match_id: i64 = sqlx::query_scalar(
        "INSERT INTO release_matches (
            local_release_id, provider, remote_release_id, remote_group_id, release_url, title,
            artist, year, country, label, catalog_number, format, barcode, score,
            algorithm_version, acceptance_method, source_revision, stale, snapshot_version,
            metadata_json, match_scope
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, 0, ?18, ?19, ?20)
         RETURNING id",
    )
    .bind(local_release_id)
    .bind(provider.as_str())
    .bind(remote.id.storage_id())
    .bind(remote.group_id.as_ref().map(GroupId::storage_id))
    .bind(&remote.url)
    .bind(non_empty(&remote.title))
    .bind(non_empty(&remote.artist_display()))
    .bind(edition.and_then(|release| release.year))
    .bind(edition.and_then(|release| release.country.as_ref()))
    .bind(edition.and_then(RemoteRelease::label_display))
    .bind(edition.and_then(RemoteRelease::catalog_number))
    .bind(edition.and_then(RemoteRelease::format_display))
    .bind(edition.and_then(|release| release.barcodes.first()))
    .bind(match_scope.score(&candidate.score))
    .bind(&algorithm_version)
    .bind(method.as_str())
    .bind(&guard.source_revision)
    .bind(SNAPSHOT_VERSION)
    .bind(&metadata_json)
    .bind(match_scope.as_str())
    .fetch_one(&mut *tx)
    .await?;

    let tracks = &release.tracks;
    let mut ordinals: HashMap<usize, i64> = HashMap::new();
    let playable = remote.playable_tracks();
    for (ordinal, (tracklist_index, _)) in playable.iter().enumerate() {
        ordinals.insert(*tracklist_index, ordinal as i64 + 1);
    }

    for (local_index, remote_index) in candidate.mapping.assigned_pairs() {
        let Some(local_track_id) = tracks.get(local_index).map(|track| track.id) else {
            bail!("mapping refers to a local track that is not part of the release");
        };
        let Some(remote_track) = remote.tracklist.get(remote_index) else {
            bail!("mapping refers to a remote track that does not exist");
        };
        let edition_track = (match_scope == MatchScope::Edition).then_some(remote_track);
        let ordinal = ordinals
            .get(&remote_index)
            .copied()
            .unwrap_or(remote_index as i64 + 1);
        let track_artist = if remote_track.artists.is_empty() {
            remote.artist_display()
        } else {
            remote_track.artist_display()
        };

        sqlx::query(
            "INSERT INTO release_track_matches (
                match_id, track_id, remote_ordinal, remote_position, remote_title,
                remote_artist, remote_duration_secs, medium_position, track_position,
                remote_track_id, remote_recording_id
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(match_id)
        .bind(local_track_id)
        .bind(ordinal)
        .bind(edition_track.and_then(|track| track.position.as_ref()))
        .bind(non_empty(&remote_track.title))
        .bind(non_empty(&track_artist))
        .bind(remote_track.duration_secs)
        .bind(edition_track.and_then(|track| track.medium_position.map(i64::from)))
        .bind(edition_track.and_then(|track| track.track_position.map(i64::from)))
        .bind(edition_track.and_then(|track| track.release_track_id.map(|id| id.to_string())))
        .bind(remote_track.recording_id.map(|id| id.to_string()))
        .execute(&mut *tx)
        .await?;
    }

    // Files in the group must all participate in the accepted mapping.
    for track in tracks {
        let mapped: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM release_track_matches WHERE match_id = ?1 AND track_id = ?2",
        )
        .bind(match_id)
        .bind(track.id)
        .fetch_one(&mut *tx)
        .await?;
        if mapped == 0 {
            bail!("refusing to accept a mapping that leaves local files unmatched");
        }
    }

    sqlx::query(
        "UPDATE local_releases SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?2",
    )
    .bind(STATUS_MATCHED)
    .bind(local_release_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

/// Removes the accepted overlay and all provider reviews, showing scanned
/// metadata again. Never contacts a provider.
pub async fn clear_match(pool: &SqlitePool, local_release_id: i64) -> Result<bool> {
    let mut tx = begin_write(pool).await?;

    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM release_matches WHERE local_release_id = ?1")
            .bind(local_release_id)
            .fetch_optional(&mut *tx)
            .await?;

    sqlx::query("DELETE FROM release_matches WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM match_reviews WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE local_releases SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?2",
    )
    .bind(STATUS_UNMATCHED)
    .bind(local_release_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(existing.is_some())
}

/// Stores candidate summaries for later interactive review, one group per
/// provider so a fallback group never overwrites the primary group.
#[allow(clippy::too_many_arguments)]
pub async fn queue_review(
    pool: &SqlitePool,
    local_release_id: i64,
    provider: ProviderKind,
    source_revision: &str,
    expected_match_id: Option<i64>,
    candidates: &[StoredCandidate],
    proposed_mapping: &Mapping,
    reasons: &[String],
) -> Result<()> {
    let candidates_json = serde_json::to_string(candidates)?;
    let mapping_json = serde_json::to_string(proposed_mapping)?;
    let reasons_json = serde_json::to_string(reasons)?;

    sqlx::query(
        "INSERT INTO match_reviews (
            local_release_id, provider, snapshot_version, expected_match_id, candidates_json,
            proposed_mapping_json, reasons_json, source_revision
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(local_release_id, provider) DO UPDATE SET
            snapshot_version = excluded.snapshot_version,
            expected_match_id = excluded.expected_match_id,
            candidates_json = excluded.candidates_json,
            proposed_mapping_json = excluded.proposed_mapping_json,
            reasons_json = excluded.reasons_json,
            source_revision = excluded.source_revision,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(local_release_id)
    .bind(provider.as_str())
    .bind(SNAPSHOT_VERSION)
    .bind(expected_match_id)
    .bind(candidates_json)
    .bind(mapping_json)
    .bind(reasons_json)
    .bind(source_revision)
    .execute(pool)
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
pub struct StoredReview {
    pub provider: ProviderKind,
    pub snapshot_version: i64,
    pub candidates: Vec<StoredCandidate>,
    pub reasons: Vec<String>,
    pub guard: MatchGuard,
}

pub async fn load_review(
    pool: &SqlitePool,
    local_id: i64,
    provider: ProviderKind,
) -> Result<Option<StoredReview>> {
    let row: Option<(i64, Option<i64>, String, String, String)> = sqlx::query_as(
        "SELECT snapshot_version, expected_match_id, candidates_json, reasons_json, source_revision
         FROM match_reviews WHERE local_release_id = ?1 AND provider = ?2",
    )
    .bind(local_id)
    .bind(provider.as_str())
    .fetch_optional(pool)
    .await?;

    let Some((snapshot_version, expected_match_id, candidates_json, reasons_json, source_revision)) =
        row
    else {
        return Ok(None);
    };

    let candidates = decode_stored_candidates(snapshot_version, provider, &candidates_json)?;
    let reasons: Vec<String> = serde_json::from_str(&reasons_json).unwrap_or_default();

    Ok(Some(StoredReview {
        provider,
        snapshot_version,
        candidates,
        reasons,
        guard: MatchGuard {
            source_revision,
            accepted_match_id: expected_match_id,
        },
    }))
}

// ---------------------------------------------------------------------------
// Effective metadata (accepted provider values with scanned fallback)
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
pub struct TrackRow {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub duration_secs: Option<f64>,
    pub stale: bool,
}

pub async fn list_tracks(pool: &SqlitePool) -> Result<Vec<TrackRow>> {
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path,
                COALESCE(tm.remote_title, t.title) AS title,
                COALESCE(tm.remote_artist, t.artist) AS artist,
                COALESCE(m.title, a.title) AS album,
                CASE WHEN m.match_scope = 'album' THEN t.track_number
                     ELSE COALESCE(tm.track_position, tm.remote_ordinal, t.track_number) END AS track_number,
                COALESCE(tm.medium_position, t.disc_number) AS disc_number,
                t.duration_secs AS duration_secs,
                (m.stale = 1) AS stale
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         LEFT JOIN release_matches m ON m.local_release_id = t.local_release_id
         LEFT JOIN release_track_matches tm ON tm.track_id = t.id AND tm.match_id = m.id
         ORDER BY album, disc_number, track_number, title",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A `LIKE` pattern matching `query` as a literal substring. The backslash is
/// escaped first so the escapes added for `%` and `_` stay intact; the query
/// must use `ESCAPE '\'`.
fn like_contains(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for ch in query.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

pub async fn search_tracks(pool: &SqlitePool, query: &str) -> Result<Vec<TrackRow>> {
    let pattern = like_contains(query);
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path,
                COALESCE(tm.remote_title, t.title) AS title,
                COALESCE(tm.remote_artist, t.artist) AS artist,
                COALESCE(m.title, a.title) AS album,
                CASE WHEN m.match_scope = 'album' THEN t.track_number
                     ELSE COALESCE(tm.track_position, tm.remote_ordinal, t.track_number) END AS track_number,
                COALESCE(tm.medium_position, t.disc_number) AS disc_number,
                t.duration_secs AS duration_secs,
                (m.stale = 1) AS stale
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         LEFT JOIN release_matches m ON m.local_release_id = t.local_release_id
         LEFT JOIN release_track_matches tm ON tm.track_id = t.id AND tm.match_id = m.id
         WHERE COALESCE(tm.remote_title, t.title) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
            OR COALESCE(tm.remote_artist, t.artist) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
            OR COALESCE(m.title, a.title) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
         ORDER BY album, disc_number, track_number, title",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{RemoteTrack, ScoreBreakdown, TrackEntryType};
    use crate::scanner;
    use std::path::PathBuf;

    fn scanned(path: &str, album: Option<&str>, artist: Option<&str>, track: i64) -> ScannedTrack {
        ScannedTrack {
            path: PathBuf::from(path),
            title: Some(format!("Track {track}")),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            album_artist: artist.map(str::to_string),
            year: Some(2001),
            track_number: Some(track),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz: ObservedMusicBrainzIds::default(),
            duration_secs: Some(180.0),
            modified_at: 1,
        }
    }

    async fn test_pool() -> SqlitePool {
        crate::db::connect_in_memory().await.unwrap()
    }

    async fn insert(pool: &SqlitePool, root: &str, tracks: &[ScannedTrack]) {
        insert_summary(pool, root, tracks).await;
    }

    async fn insert_summary(
        pool: &SqlitePool,
        root: &str,
        tracks: &[ScannedTrack],
    ) -> RebuildSummary {
        let directory_id = upsert_directory(pool, Path::new(root)).await.unwrap();
        for track in tracks {
            upsert_track(pool, track, directory_id).await.unwrap();
        }
        rebuild_local_releases(pool, None).await.unwrap()
    }

    /// Accepts `titles` as the remote tracklist for a release found by album.
    async fn accept_manually(pool: &SqlitePool, album: &str, titles: &[&str]) -> i64 {
        let release = load_local_releases(pool, None)
            .await
            .unwrap()
            .into_iter()
            .find(|release| release.album_title.as_deref() == Some(album))
            .unwrap();
        let candidate = Candidate::new(&release, remote_with(titles));
        let guard = load_match_guard(pool, release.id).await.unwrap();
        accept_match(
            pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();
        release.id
    }

    async fn release_ids(pool: &SqlitePool) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM local_releases ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    fn retitled(track: &ScannedTrack, album: &str) -> ScannedTrack {
        let mut moved = track.clone();
        moved.album = Some(album.to_string());
        moved.modified_at += 1;
        moved
    }

    /// A unique scratch directory for file-backed databases. Removing it also
    /// removes the `-wal` and `-shm` side files.
    fn scratch_dir(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radish-store-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    fn remote_release() -> RemoteRelease {
        RemoteRelease {
            id: ReleaseId::Discogs(42),
            group_id: None,
            title: "Blue".to_string(),
            artists: vec!["Artist".to_string()],
            year: Some(2001),
            country: Some("US".to_string()),
            labels: vec!["Label".to_string()],
            catalog_numbers: vec!["CAT-1".to_string()],
            formats: vec!["CD".to_string()],
            barcodes: vec!["0123456789012".to_string()],
            tracklist: vec![RemoteTrack {
                position: Some("1".to_string()),
                title: "Remote Track 1".to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: None,
                track_position: None,
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            }],
            url: "https://www.discogs.com/release/42".to_string(),
            lookup_aliases: Vec::new(),
            artist_credit: None,
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }

    fn musicbrainz_release() -> RemoteRelease {
        let id = mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd");
        RemoteRelease {
            id: ReleaseId::MusicBrainz(id),
            group_id: None,
            title: "Blue".to_string(),
            artists: vec!["Artist".to_string()],
            year: Some(2001),
            country: Some("US".to_string()),
            labels: vec!["Label".to_string()],
            catalog_numbers: vec!["CAT-1".to_string()],
            formats: vec!["CD".to_string()],
            barcodes: vec!["0123456789012".to_string()],
            tracklist: vec![RemoteTrack {
                position: Some("1".to_string()),
                title: "MusicBrainz Track 1".to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: Some(1),
                track_position: Some(1),
                release_track_id: Some(mbid("11111111-1111-1111-1111-111111111111")),
                recording_id: Some(mbid("22222222-2222-2222-2222-222222222222")),
                artist_credit: Some("Artist".to_string()),
            }],
            url: format!("https://musicbrainz.org/release/{id}"),
            lookup_aliases: Vec::new(),
            artist_credit: Some("Artist".to_string()),
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::json!({ "schema_version": 1 }),
        }
    }

    #[tokio::test]
    async fn separate_directories_are_separate_local_releases() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;
        insert(
            &pool,
            "/music/b",
            &[scanned("/music/b/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 2);
    }

    #[tokio::test]
    async fn compilation_artists_and_missing_albums_are_grouped() {
        let pool = test_pool().await;
        let mut first = scanned("/music/comp/01.flac", Some("Hits"), Some("One"), 1);
        first.album_artist = None;
        let mut second = scanned("/music/comp/02.flac", Some("Hits"), Some("Two"), 2);
        second.album_artist = None;
        insert(&pool, "/music/comp", &[first, second]).await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].album_artist.as_deref(), Some("Various Artists"));
        assert_eq!(releases[0].artist_kind, ArtistKind::VariousArtists);
        assert!(releases[0].is_compilation);

        insert(
            &pool,
            "/music/untagged",
            &[scanned("/music/untagged/01.flac", None, None, 1)],
        )
        .await;
        let releases = load_local_releases(&pool, None).await.unwrap();
        let untagged = releases
            .iter()
            .find(|release| release.directory == "/music/untagged")
            .unwrap();
        assert!(!untagged.has_album_tag);
        assert_eq!(untagged.artist_kind, ArtistKind::Unknown);
    }

    #[tokio::test]
    async fn uppercase_paths_keep_one_release_and_one_track() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/Music/Album",
            &[scanned(
                "/Music/Album/01.flac",
                Some("Album"),
                Some("Artist"),
                1,
            )],
        )
        .await;
        insert(
            &pool,
            "/music/album",
            &[scanned(
                "/music/album/01.flac",
                Some("Album"),
                Some("Artist"),
                1,
            )],
        )
        .await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 1);
    }

    #[tokio::test]
    async fn disc_numbers_are_preserved_within_one_directory() {
        let pool = test_pool().await;
        let mut disc_one = scanned("/music/set/1-01.flac", Some("Set"), Some("Artist"), 1);
        disc_one.disc_number = Some(1);
        let mut disc_two = scanned("/music/set/2-01.flac", Some("Set"), Some("Artist"), 1);
        disc_two.disc_number = Some(2);
        insert(&pool, "/music/set", &[disc_one, disc_two]).await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 1);
        let discs: Vec<Option<i64>> = releases[0].tracks.iter().map(|track| track.disc).collect();
        assert_eq!(discs, vec![Some(1), Some(2)]);
    }

    #[tokio::test]
    async fn revision_is_stable_across_unchanged_reimports() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let first = load_local_releases(&pool, None).await.unwrap()[0]
            .revision
            .clone();

        let directory_id = upsert_directory(&pool, Path::new("/music/a"))
            .await
            .unwrap();
        upsert_track(
            &pool,
            &scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            directory_id,
        )
        .await
        .unwrap();
        rebuild_local_releases(&pool, None).await.unwrap();

        let second = load_local_releases(&pool, None).await.unwrap()[0]
            .revision
            .clone();
        assert_eq!(first, second);
        assert!(first.starts_with("release-input-v2:"));
    }

    #[tokio::test]
    async fn revision_changes_with_the_year_and_identifier_evidence() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let first = load_local_releases(&pool, None).await.unwrap()[0]
            .revision
            .clone();

        let mut retagged = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        retagged.year = Some(2002);
        retagged.musicbrainz.recording = vec!["76df3287-6cda-33eb-8e9a-044b5e15ffdd".to_string()];
        insert(&pool, "/music/a", &[retagged]).await;

        let second = load_local_releases(&pool, None).await.unwrap()[0]
            .revision
            .clone();
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn consistent_release_ids_become_release_evidence() {
        let pool = test_pool().await;
        let id = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
        let mut first = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        first.musicbrainz.release = vec![id.to_string()];
        first.musicbrainz.recording = vec!["22222222-2222-2222-2222-222222222222".to_string()];
        let mut second = scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2);
        second.musicbrainz.release = vec![id.to_string()];
        insert(&pool, "/music/a", &[first, second]).await;

        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        assert_eq!(release.embedded_evidence.release_id, Some(mbid(id)));
        assert!(release.embedded_evidence.problems.is_empty());
        assert_eq!(
            release.tracks[0].recording_id,
            Some(mbid("22222222-2222-2222-2222-222222222222"))
        );
    }

    #[tokio::test]
    async fn conflicting_release_ids_are_reported_never_silently_dropped() {
        let pool = test_pool().await;
        let mut first = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        first.musicbrainz.release = vec!["76df3287-6cda-33eb-8e9a-044b5e15ffdd".to_string()];
        let mut second = scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2);
        second.musicbrainz.release = vec!["11111111-1111-1111-1111-111111111111".to_string()];
        insert(&pool, "/music/a", &[first, second]).await;

        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        assert_eq!(release.embedded_evidence.release_id, None);
        assert!(
            release
                .embedded_evidence
                .problems
                .iter()
                .any(|problem| problem.contains("disagree"))
        );
    }

    #[tokio::test]
    async fn acceptance_survives_reimport_and_clear_restores_scanned_values() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;

        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Remote Track 1"));
        assert!(!rows[0].stale);

        // Re-import with unchanged tags and rebuild grouping.
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 1);
        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Remote Track 1"));

        assert!(clear_match(&pool, release.id).await.unwrap());
        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Track 1"));
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 0);
        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(release.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_UNMATCHED);
    }

    #[tokio::test]
    async fn changed_membership_marks_the_match_stale_and_keeps_the_overlay() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        // Add another file to the same group, then refresh grouping.
        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;

        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(release.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_STALE);

        let rows = list_tracks(&pool).await.unwrap();
        assert!(rows.iter().all(|row| row.stale));
    }

    #[tokio::test]
    async fn stale_guard_rejects_a_second_acceptance_without_changing_the_overlay() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);

        let mut first_release = remote_release();
        first_release.tracklist[0].title = "First".to_string();
        let first = Candidate::new(&release, first_release);
        let mut second_release = musicbrainz_release();
        second_release.tracklist[0].title = "Second".to_string();
        let second = Candidate::new(&release, second_release);

        let before = list_tracks(&pool).await.unwrap();
        let old_guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &first,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &old_guard,
        )
        .await
        .unwrap();
        assert!(
            accept_match(
                &pool,
                &release,
                &second,
                AcceptanceMethod::Manual,
                MatchScope::Edition,
                &old_guard
            )
            .await
            .is_err()
        );

        let after = list_tracks(&pool).await.unwrap();
        assert_eq!(
            after[0].title,
            Some(first.release.tracklist[0].title.clone())
        );
        assert_eq!(before[0].path, after[0].path);
    }

    #[tokio::test]
    async fn revision_change_during_review_rejects_application() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();

        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;

        let error = accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("changed since it was evaluated"));

        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 0);
    }

    #[tokio::test]
    async fn incomplete_mapping_is_never_applied() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        assert!(!candidate.mapping.complete);

        assert!(
            accept_match(
                &pool,
                &release,
                &candidate,
                AcceptanceMethod::Manual,
                MatchScope::Edition,
                &guard
            )
            .await
            .is_err()
        );
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 0);
    }

    #[tokio::test]
    async fn accepted_musicbrainz_overlay_replaces_discogs_atomically() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);

        let discogs = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &discogs,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        let guard = load_match_guard(&pool, release.id).await.unwrap();
        let musicbrainz = Candidate::new(&release, musicbrainz_release());
        accept_match(
            &pool,
            &release,
            &musicbrainz,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        let provider: String = sqlx::query_scalar("SELECT provider FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(provider, "musicbrainz");
        let track: (Option<i64>, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT medium_position, track_position, remote_track_id, remote_recording_id
             FROM release_track_matches",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(track.0, Some(1));
        assert_eq!(track.1, Some(1));
        assert_eq!(
            track.2.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            track.3.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
    }

    #[tokio::test]
    async fn search_uses_accepted_overlay_then_falls_back() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        assert_eq!(
            search_tracks(&pool, "Remote Track 1").await.unwrap().len(),
            1
        );
        assert!(!search_tracks(&pool, "Blue").await.unwrap().is_empty());
        assert!(search_tracks(&pool, "nothing").await.unwrap().is_empty());

        clear_match(&pool, release.id).await.unwrap();
        assert!(
            search_tracks(&pool, "Remote Track 1")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(search_tracks(&pool, "Track 1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reviews_are_stored_per_provider_and_decode_offline() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let stored = StoredCandidate {
            release: candidate.release.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: false,
            match_scope: MatchScope::Edition,
            blockers: vec!["review".to_string()],
            warnings: Vec::new(),
            evidence: Vec::new(),
            ai_advice: None,
        };

        queue_review(
            &pool,
            release.id,
            ProviderKind::Discogs,
            &release.revision,
            None,
            std::slice::from_ref(&stored),
            &candidate.mapping,
            &["primary uncertain".to_string()],
        )
        .await
        .unwrap();
        queue_review(
            &pool,
            release.id,
            ProviderKind::MusicBrainz,
            &release.revision,
            None,
            std::slice::from_ref(&stored),
            &candidate.mapping,
            &["fallback uncertain".to_string()],
        )
        .await
        .unwrap();

        let queued = list_local_releases(&pool, None).await.unwrap();
        assert_eq!(
            queued[0].queued_providers,
            vec![ProviderKind::Discogs, ProviderKind::MusicBrainz]
        );

        let review = load_review(&pool, release.id, ProviderKind::Discogs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(review.snapshot_version, SNAPSHOT_VERSION);
        assert_eq!(review.candidates.len(), 1);
        assert_eq!(review.reasons, vec!["primary uncertain".to_string()]);
        assert_eq!(review.guard.source_revision, release.revision);
    }

    #[test]
    fn legacy_snapshots_decode_through_the_discogs_codec() {
        let legacy = r#"{
            "id": 42,
            "master_id": 7,
            "title": "Blue",
            "artists": ["Artist"],
            "year": 2001,
            "country": "US",
            "labels": ["Label"],
            "catalog_numbers": ["CAT-1"],
            "formats": ["CD"],
            "barcodes": ["0123456789012"],
            "tracklist": [
                {"position": "1", "title": "One", "artists": [], "duration_secs": 180.0, "entry_type": "track"}
            ],
            "url": "https://www.discogs.com/release/42"
        }"#;
        let release = decode_release_snapshot(1, ProviderKind::Discogs, legacy).unwrap();
        assert_eq!(release.id, ReleaseId::Discogs(42));
        assert_eq!(release.group_id, Some(GroupId::DiscogsMaster(7)));
        assert_eq!(release.tracklist[0].title, "One");
        assert!(release.tracklist[0].release_track_id.is_none());
        assert_eq!(release.provider_metadata, serde_json::Value::Null);
        assert!(decode_release_snapshot(1, ProviderKind::MusicBrainz, legacy).is_err());
    }

    #[test]
    fn round_trips_version_two_snapshots_and_rejects_mismatched_providers() {
        let release = musicbrainz_release();
        let json = serde_json::to_string(&release).unwrap();
        let decoded = decode_release_snapshot(2, ProviderKind::MusicBrainz, &json).unwrap();
        assert_eq!(decoded.id, release.id);
        assert_eq!(
            decoded.tracklist[0].release_track_id,
            release.tracklist[0].release_track_id
        );
        assert!(decode_release_snapshot(2, ProviderKind::Discogs, &json).is_err());
        assert!(decode_release_snapshot(9, ProviderKind::MusicBrainz, &json).is_err());
    }

    #[test]
    fn legacy_candidates_decode_without_losing_scores() {
        let legacy = r#"[{
            "release": {
                "id": 42,
                "master_id": null,
                "title": "Blue",
                "artists": ["Artist"],
                "year": 2001,
                "country": "US",
                "labels": [],
                "catalog_numbers": [],
                "formats": [],
                "barcodes": [],
                "tracklist": [],
                "url": "https://www.discogs.com/release/42"
            },
            "mapping": {"pairs": [], "complete": false, "ambiguous": false, "notes": []},
            "score": {"album_title": 1.0, "album_artist": 1.0, "track_titles": 0.0, "durations": 0.0, "year": 1.0, "total": 0.5},
            "auto_accept": false,
            "blockers": ["queue"],
            "warnings": [],
            "evidence": []
        }]"#;
        let candidates = decode_stored_candidates(1, ProviderKind::Discogs, legacy).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].release.id, ReleaseId::Discogs(42));
        assert_eq!(
            candidates[0].score,
            ScoreBreakdown {
                album_title: 1.0,
                album_artist: 1.0,
                track_titles: 0.0,
                durations: 0.0,
                year: 1.0,
                total: 0.5,
            }
        );
    }

    #[test]
    fn path_within_respects_component_boundaries() {
        assert!(path_within("/music/Blue", Path::new("/music")));
        assert!(path_within("/MUSIC/blue", Path::new("/music/")));
        assert!(!path_within("/music/Bluegrass", Path::new("/music/Blue")));
        assert!(!path_within("/other/Blue", Path::new("/music")));
    }

    fn write_wav(path: &Path) {
        let data = [0u8; 32];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&44100u32.to_le_bytes());
        bytes.extend_from_slice(&88200u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }

    #[tokio::test]
    async fn audio_files_are_untouched_by_import_matching_and_clearing() {
        let dir = std::env::temp_dir().join(format!("radish-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("01.wav");
        write_wav(&file);
        let original = std::fs::read(&file).unwrap();

        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, &dir).await.unwrap();
        let scanned_track = scanner::read_tags(&file).unwrap();
        upsert_track(&pool, &scanned_track, directory_id)
            .await
            .unwrap();
        rebuild_local_releases(&pool, Some(&dir)).await.unwrap();

        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), original);

        clear_match(&pool, release.id).await.unwrap();
        let rescanned = scanner::read_tags(&file).unwrap();
        upsert_track(&pool, &rescanned, directory_id).await.unwrap();
        rebuild_local_releases(&pool, None).await.unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), original);

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn rebuild_waits_for_a_concurrent_writer_and_sees_its_rows() {
        let dir = scratch_dir("rebuild-wait");
        let path = dir.join("library.db");
        let pool_a = crate::db::connect(&path).await.unwrap();
        let pool_b = crate::db::connect(&path).await.unwrap();

        let directory_id = upsert_directory(&pool_a, Path::new("/music/a"))
            .await
            .unwrap();
        upsert_track(
            &pool_a,
            &scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            directory_id,
        )
        .await
        .unwrap();
        rebuild_local_releases(&pool_a, None).await.unwrap();

        // Writer A holds the write lock and has an uncommitted second track.
        let mut writer = begin_write(&pool_a).await.unwrap();
        sqlx::query(
            "INSERT INTO tracks (path, title, album_id, directory_id, track_number, modified_at)
             SELECT '/music/a/02.flac', 'Track 2', album_id, directory_id, 2, 1
             FROM tracks WHERE path = '/music/a/01.flac'",
        )
        .execute(&mut *writer)
        .await
        .unwrap();

        let rebuilding = {
            let pool_b = pool_b.clone();
            tokio::spawn(async move { rebuild_local_releases(&pool_b, None).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        writer.commit().await.unwrap();

        rebuilding.await.unwrap().unwrap();
        let track_count: i64 = sqlx::query_scalar("SELECT track_count FROM local_releases")
            .fetch_one(&pool_b)
            .await
            .unwrap();
        assert_eq!(track_count, 2, "the rebuild must observe the committed row");

        pool_a.close().await;
        pool_b.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn remote_with(titles: &[&str]) -> RemoteRelease {
        let mut release = remote_release();
        release.tracklist = titles
            .iter()
            .enumerate()
            .map(|(index, title)| RemoteTrack {
                position: Some((index + 1).to_string()),
                title: title.to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: None,
                track_position: None,
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            })
            .collect();
        release
    }

    #[tokio::test]
    async fn accepted_overlay_follows_track_numbers_not_path_order() {
        let pool = test_pool().await;
        let mut intro = scanned("/music/a/Intro.flac", Some("Blue"), Some("Artist"), 1);
        intro.title = Some("Intro".into());
        let mut song = scanned("/music/a/Song.flac", Some("Blue"), Some("Artist"), 2);
        song.title = Some("Song".into());
        let mut outro = scanned("/music/a/Outro.flac", Some("Blue"), Some("Artist"), 3);
        outro.title = Some("Outro".into());
        insert(&pool, "/music/a", &[intro, song, outro]).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_with(&["Intro", "Song", "Outro"]));
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        let rows = list_tracks(&pool).await.unwrap();
        for (file, title) in [("Intro", "Intro"), ("Song", "Song"), ("Outro", "Outro")] {
            let row = rows
                .iter()
                .find(|row| row.path.ends_with(&format!("{file}.flac")))
                .unwrap();
            assert_eq!(row.title.as_deref(), Some(title), "{file}.flac");
        }
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn acceptance_rejects_a_snapshot_whose_member_ids_changed() {
        let pool = test_pool().await;
        let file = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        insert(&pool, "/music/a", std::slice::from_ref(&file)).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_release());
        let guard = load_match_guard(&pool, release.id).await.unwrap();

        // The same file is removed and re-added: same revision, new track ID.
        remove_track_by_path(&pool, Path::new("/music/a/01.flac"))
            .await
            .unwrap();
        insert(&pool, "/music/a", &[file]).await;
        let current = load_local_releases(&pool, None).await.unwrap().remove(0);
        assert_eq!(current.revision, release.revision);
        assert_ne!(current.tracks[0].id, release.tracks[0].id);

        let error = accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap_err();
        assert!(
            error.downcast_ref::<SnapshotChanged>().is_some(),
            "expected SnapshotChanged, got: {error:#}"
        );
        assert_eq!(count(&pool, "release_matches").await, 0);
        assert_eq!(count(&pool, "release_track_matches").await, 0);
    }

    #[tokio::test]
    async fn acceptance_rejects_a_snapshot_older_than_its_guard() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let old = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&old, remote_release());

        let mut retagged = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        retagged.title = Some("Renamed".into());
        insert(&pool, "/music/a", &[retagged]).await;
        // The guard is loaded after the change, but the release is the old one.
        let guard = load_match_guard(&pool, old.id).await.unwrap();

        let error = accept_match(
            &pool,
            &old,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap_err();
        assert!(
            error.downcast_ref::<SnapshotChanged>().is_some(),
            "{error:#}"
        );
        assert_eq!(count(&pool, "release_matches").await, 0);
    }

    #[tokio::test]
    async fn guard_for_a_missing_release_is_a_snapshot_change() {
        let pool = test_pool().await;
        let error = load_match_guard(&pool, 99).await.unwrap_err();
        assert!(
            error.downcast_ref::<SnapshotChanged>().is_some(),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn moved_tracks_drop_the_old_overlay_and_can_be_accepted_again() {
        let pool = test_pool().await;
        let one = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        let two = scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2);
        insert(&pool, "/music/a", &[one.clone(), two.clone()]).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_with(&["R1", "R2"]));
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        let mut moved = two.clone();
        moved.album = Some("Other".into());
        moved.modified_at = 2;
        insert(&pool, "/music/a", &[one, moved]).await;

        let rows = list_tracks(&pool).await.unwrap();
        let moved_row = rows.iter().find(|r| r.path.ends_with("02.flac")).unwrap();
        assert_eq!(moved_row.title.as_deref(), Some("Track 2"));
        assert!(!moved_row.stale);
        let kept_row = rows.iter().find(|r| r.path.ends_with("01.flac")).unwrap();
        assert_eq!(kept_row.title.as_deref(), Some("R1"));
        assert!(kept_row.stale);

        // The moved file can be matched as part of its new release.
        let other = load_local_releases(&pool, None)
            .await
            .unwrap()
            .into_iter()
            .find(|release| release.album_title.as_deref() == Some("Other"))
            .unwrap();
        let candidate = Candidate::new(&other, remote_with(&["Moved"]));
        let guard = load_match_guard(&pool, other.id).await.unwrap();
        accept_match(
            &pool,
            &other,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();
        let rows = list_tracks(&pool).await.unwrap();
        let moved_row = rows.iter().find(|r| r.path.ends_with("02.flac")).unwrap();
        assert_eq!(moved_row.title.as_deref(), Some("Moved"));
    }

    #[tokio::test]
    async fn search_treats_percent_and_underscore_literally() {
        let pool = test_pool().await;
        let titled = |path: &str, title: &str, number: i64| {
            let mut track = scanned(path, Some("Blue"), Some("Artist"), number);
            track.title = Some(title.to_string());
            track
        };
        insert(
            &pool,
            "/music/a",
            &[
                titled("/music/a/01.flac", "100% Pure", 1),
                titled("/music/a/02.flac", "1000 Pure", 2),
                titled("/music/a/03.flac", "a_b", 3),
                titled("/music/a/04.flac", "axb", 4),
                titled("/music/a/05.flac", r"back\slash", 5),
            ],
        )
        .await;

        let titles = |rows: Vec<TrackRow>| -> Vec<String> {
            rows.into_iter().filter_map(|row| row.title).collect()
        };
        assert_eq!(
            titles(search_tracks(&pool, "100%").await.unwrap()),
            vec!["100% Pure"]
        );
        assert_eq!(
            titles(search_tracks(&pool, "A_B").await.unwrap()),
            vec!["a_b"]
        );
        assert_eq!(
            titles(search_tracks(&pool, r"k\s").await.unwrap()),
            vec![r"back\slash"]
        );
    }

    #[tokio::test]
    async fn year_belongs_to_each_folder_and_reimports_keep_revisions() {
        let pool = test_pool().await;
        let mut first = scanned("/music/x/01.flac", Some("Blue"), Some("Artist"), 1);
        first.year = Some(2001);
        let mut second = scanned("/music/y/01.flac", Some("Blue"), Some("Artist"), 1);
        second.year = Some(2005);
        insert(&pool, "/music/x", std::slice::from_ref(&first)).await;
        insert(&pool, "/music/y", std::slice::from_ref(&second)).await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        let year_of = |directory: &str| {
            releases
                .iter()
                .find(|release| release.directory == directory)
                .unwrap()
                .year
        };
        assert_eq!(year_of("/music/x"), Some(2001));
        assert_eq!(year_of("/music/y"), Some(2005));

        let release = releases
            .iter()
            .find(|release| release.directory == "/music/x")
            .unwrap();
        let candidate = Candidate::new(release, remote_with(&["Track 1"]));
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        // Re-import both folders in the opposite order, with unchanged tags.
        insert(&pool, "/music/y", &[second]).await;
        insert(&pool, "/music/x", &[first]).await;

        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(release.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_MATCHED);
        assert!(list_tracks(&pool).await.unwrap().iter().all(|r| !r.stale));
    }

    #[tokio::test]
    async fn reimport_in_another_order_keeps_the_match_current() {
        let pool = test_pool().await;
        let one = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        let mut two = scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2);
        two.year = None;
        insert(&pool, "/music/a", &[one.clone(), two.clone()]).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, remote_with(&["Track 1", "Track 2"]));
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();

        // Same tags, different completion order.
        insert(&pool, "/music/a", &[two, one]).await;
        assert!(list_tracks(&pool).await.unwrap().iter().all(|r| !r.stale));
    }

    #[tokio::test]
    async fn albums_without_artist_are_not_duplicated() {
        let pool = test_pool().await;
        let mut anonymous = scanned("/music/a/01.flac", Some("Blue"), None, 1);
        anonymous.album_artist = None;
        let mut blank = scanned("/music/a/02.flac", Some("Blue"), Some(""), 2);
        blank.album_artist = None;
        insert(&pool, "/music/a", &[anonymous.clone(), blank.clone()]).await;
        insert(&pool, "/music/a", &[anonymous, blank]).await;

        assert_eq!(count(&pool, "albums").await, 1);
    }

    #[tokio::test]
    async fn batched_upsert_writes_all_tracks_and_skips_unchanged_rows() {
        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("/music/a"))
            .await
            .unwrap();
        let mut tracks: Vec<ScannedTrack> = (1..=3)
            .map(|n| {
                scanned(
                    &format!("/music/a/0{n}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    n,
                )
            })
            .collect();

        assert_eq!(
            upsert_tracks(&pool, &tracks, directory_id).await.unwrap(),
            3
        );
        assert_eq!(count(&pool, "tracks").await, 3);
        assert_eq!(count(&pool, "albums").await, 1);

        assert_eq!(
            upsert_tracks(&pool, &tracks, directory_id).await.unwrap(),
            0,
            "unchanged rows must not be rewritten"
        );

        tracks[1].title = Some("Renamed".into());
        assert_eq!(
            upsert_tracks(&pool, &tracks, directory_id).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn batched_upsert_is_atomic() {
        let pool = test_pool().await;
        let tracks: Vec<ScannedTrack> = (1..=2)
            .map(|n| {
                scanned(
                    &format!("/music/a/0{n}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    n,
                )
            })
            .collect();

        // Directory 999 does not exist, so the foreign key rejects every row.
        assert!(upsert_tracks(&pool, &tracks, 999).await.is_err());
        assert_eq!(count(&pool, "tracks").await, 0);
        assert_eq!(count(&pool, "albums").await, 0);
    }

    #[tokio::test]
    async fn symbol_only_titles_and_artists_keep_their_identity() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("÷"), Some("!!!"), 1),
                scanned("/music/a/02.flac", None, None, 2),
            ],
        )
        .await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 2);
        let tagged = releases
            .iter()
            .find(|release| release.has_album_tag)
            .expect("the symbol-titled album keeps its album tag");
        assert_eq!(tagged.album_title.as_deref(), Some("÷"));
        assert_eq!(tagged.album_artist.as_deref(), Some("!!!"));
        assert_eq!(tagged.tracks.len(), 1);
    }

    #[test]
    fn identity_keys_keep_symbol_only_names_apart() {
        assert_eq!(identity_key("   "), None);
        assert_eq!(identity_key("Blue"), Some(matcher::normalize_text("Blue")));
        assert_eq!(identity_key("÷"), Some("~÷".to_string()));
        assert_eq!(identity_key("!!!"), identity_key("  !!!  "));
        assert_ne!(identity_key("!!!"), identity_key("???"));
    }

    #[tokio::test]
    async fn retagged_album_title_keeps_the_accepted_match_and_marks_it_stale() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
        ];
        insert(&pool, "/music/a", &files).await;
        let id = accept_manually(&pool, "Blue", &["R1", "R2"]).await;

        let retagged: Vec<ScannedTrack> = files
            .iter()
            .map(|track| retitled(track, "Blue (Remaster)"))
            .collect();
        let summary = insert_summary(&pool, "/music/a", &retagged).await;
        assert_eq!(summary.rekeyed, 1);
        assert_eq!(summary.newly_stale, 1);
        assert_eq!(summary.dropped_matches, 0);

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].id, id);
        assert_eq!(releases[0].album_title.as_deref(), Some("Blue (Remaster)"));
        let status: String = sqlx::query_scalar("SELECT status FROM local_releases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_STALE);
        let matches: Vec<(i64, bool, String)> = sqlx::query_as(
            "SELECT local_release_id, stale, acceptance_method FROM release_matches",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(matches, vec![(id, true, "manual".to_string())]);
    }

    #[tokio::test]
    async fn partially_retagged_album_keeps_the_match_on_the_remaining_files() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            scanned("/music/a/03.flac", Some("Blue"), Some("Artist"), 3),
        ];
        insert(&pool, "/music/a", &files).await;
        let id = accept_manually(&pool, "Blue", &["R1", "R2", "R3"]).await;

        let moved = retitled(&files[2], "Other");
        insert(
            &pool,
            "/music/a",
            &[files[0].clone(), files[1].clone(), moved],
        )
        .await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(releases.len(), 2);
        let blue = releases
            .iter()
            .find(|release| release.album_title.as_deref() == Some("Blue"))
            .unwrap();
        assert_eq!(blue.id, id);
        assert_eq!(blue.tracks.len(), 2);
        let other = releases
            .iter()
            .find(|release| release.album_title.as_deref() == Some("Other"))
            .unwrap();
        assert_ne!(other.id, id);

        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_STALE);
        let rows = list_tracks(&pool).await.unwrap();
        let moved_row = rows.iter().find(|r| r.path.ends_with("03.flac")).unwrap();
        assert_eq!(moved_row.title.as_deref(), Some("Track 3"));
        let kept_row = rows.iter().find(|r| r.path.ends_with("01.flac")).unwrap();
        assert_eq!(kept_row.title.as_deref(), Some("R1"));
    }

    #[tokio::test]
    async fn merged_releases_reuse_the_matched_row() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("A"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("A"), Some("Artist"), 2),
            scanned("/music/a/03.flac", Some("B"), Some("Artist"), 3),
        ];
        insert(&pool, "/music/a", &files).await;
        let matched = accept_manually(&pool, "A", &["R1", "R2"]).await;
        let other = release_ids(&pool)
            .await
            .into_iter()
            .find(|id| *id != matched)
            .unwrap();

        let merged: Vec<ScannedTrack> = files.iter().map(|track| retitled(track, "C")).collect();
        let summary = insert_summary(&pool, "/music/a", &merged).await;

        assert_eq!(release_ids(&pool).await, vec![matched]);
        assert!(!release_ids(&pool).await.contains(&other));
        assert_eq!(summary.dropped_matches, 0);
        assert_eq!(summary.removed, 1);
        assert_eq!(count(&pool, "release_matches").await, 1);
        let overlays: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM release_track_matches tm
             JOIN tracks t ON t.id = tm.track_id WHERE t.local_release_id <> (
                SELECT local_release_id FROM release_matches WHERE id = tm.match_id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(overlays, 0);
    }

    #[tokio::test]
    async fn merged_releases_that_were_both_matched_keep_the_lower_id() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("A"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("B"), Some("Artist"), 2),
        ];
        insert(&pool, "/music/a", &files).await;
        let first = accept_manually(&pool, "A", &["R1"]).await;
        let second = accept_manually(&pool, "B", &["R2"]).await;
        assert!(first < second);

        let merged: Vec<ScannedTrack> = files.iter().map(|track| retitled(track, "C")).collect();
        let summary = insert_summary(&pool, "/music/a", &merged).await;

        assert_eq!(release_ids(&pool).await, vec![first]);
        assert_eq!(summary.dropped_matches, 1);
    }

    #[tokio::test]
    async fn split_into_two_new_titles_drops_the_release() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
        ];
        insert(&pool, "/music/a", &files).await;
        let id = accept_manually(&pool, "Blue", &["R1", "R2"]).await;

        let split = [retitled(&files[0], "X"), retitled(&files[1], "Y")];
        let summary = insert_summary(&pool, "/music/a", &split).await;

        assert_eq!(summary.dropped_matches, 1);
        assert_eq!(summary.created, 2);
        assert!(!release_ids(&pool).await.contains(&id));
        assert_eq!(release_ids(&pool).await.len(), 2);
        assert_eq!(count(&pool, "release_matches").await, 0);
    }

    #[tokio::test]
    async fn rebuild_without_changes_writes_nothing() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[
                scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;
        insert(
            &pool,
            "/music/b",
            &[scanned("/music/b/01.flac", Some("Red"), Some("Artist"), 1)],
        )
        .await;
        accept_manually(&pool, "Blue", &["R1", "R2"]).await;

        let before: i64 = sqlx::query_scalar("SELECT total_changes()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let summary = rebuild_local_releases(&pool, None).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT total_changes()")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(after, before, "an unchanged rebuild must not write");
        assert!(!summary.changed(), "{summary:?}");
        assert_eq!(summary.groups, 2);
        assert_eq!(summary.unchanged, summary.groups);
    }

    #[tokio::test]
    async fn scoped_rebuild_ignores_sibling_prefix_directories() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/Blue",
            &[scanned(
                "/music/Blue/01.flac",
                Some("Blue"),
                Some("Artist"),
                1,
            )],
        )
        .await;
        insert(
            &pool,
            "/music/Bluegrass",
            &[scanned(
                "/music/Bluegrass/01.flac",
                Some("Grass"),
                Some("Artist"),
                1,
            )],
        )
        .await;

        let summary = rebuild_local_releases(&pool, Some(Path::new("/MUSIC/blue")))
            .await
            .unwrap();
        assert_eq!(summary.groups, 1);
        assert_eq!(summary.unchanged, 1);
    }

    fn planned_group(directory: &str, norm: &str, track_ids: &[i64]) -> GroupedRelease {
        GroupedRelease {
            directory: directory.to_string(),
            album_title: Some(norm.to_string()),
            album_title_norm: norm.to_string(),
            album_artist: None,
            artist_kind: ArtistKind::Unknown,
            year: None,
            barcode: None,
            catalog_number: None,
            embedded_discogs_id: None,
            embedded_musicbrainz_release_id: None,
            embedded_musicbrainz_release_group_id: None,
            musicbrainz_problems: Vec::new(),
            is_compilation: false,
            has_album_tag: true,
            track_count: track_ids.len() as i64,
            revision: String::new(),
            track_ids: track_ids.to_vec(),
        }
    }

    fn planned_row(id: i64, directory: &str, norm: &str, has_match: bool) -> StoredRelease {
        StoredRelease {
            id,
            has_match,
            fields: ReleaseFields::of_group(&planned_group(directory, norm, &[])).unwrap(),
        }
    }

    fn membership(entries: &[(i64, &[i64])]) -> HashMap<i64, Vec<i64>> {
        entries
            .iter()
            .map(|(id, tracks)| (*id, tracks.to_vec()))
            .collect()
    }

    #[test]
    fn planner_maps_exact_keys_case_insensitively_and_creates_the_rest() {
        let groups = [
            planned_group("/Music/A", "blue", &[1]),
            planned_group("/music/a", "red", &[2]),
        ];
        let stored = [planned_row(7, "/music/a", "blue", false)];
        let plan = plan_rebuild(&groups, &stored, &membership(&[(7, &[1])]));
        assert_eq!(plan.targets, vec![Target::Existing(7), Target::New]);
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn planner_rekeys_a_row_whose_files_all_moved_to_one_group() {
        let groups = [planned_group("/m", "blue remaster", &[1, 2])];
        let stored = [planned_row(7, "/m", "blue", true)];
        let plan = plan_rebuild(&groups, &stored, &membership(&[(7, &[1, 2])]));
        assert_eq!(plan.targets, vec![Target::Rekey(7)]);
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn planner_prefers_a_matched_row_then_more_members_then_the_lower_id() {
        let groups = [planned_group("/m", "c", &[1, 2, 3, 4])];

        let stored = [
            planned_row(1, "/m", "a", false),
            planned_row(2, "/m", "b", true),
        ];
        let plan = plan_rebuild(&groups, &stored, &membership(&[(1, &[1, 2, 3]), (2, &[4])]));
        assert_eq!(plan.targets, vec![Target::Rekey(2)]);
        assert_eq!(plan.removed, vec![1]);

        let stored = [
            planned_row(1, "/m", "a", true),
            planned_row(2, "/m", "b", true),
        ];
        let plan = plan_rebuild(&groups, &stored, &membership(&[(1, &[1]), (2, &[2, 3, 4])]));
        assert_eq!(plan.targets, vec![Target::Rekey(2)]);
        assert_eq!(plan.removed, vec![1]);

        let plan = plan_rebuild(&groups, &stored, &membership(&[(1, &[1, 2]), (2, &[3, 4])]));
        assert_eq!(plan.targets, vec![Target::Rekey(1)]);
        assert_eq!(plan.removed, vec![2]);
    }

    #[test]
    fn planner_removes_split_empty_and_outcompeted_rows() {
        let groups = [
            planned_group("/m", "x", &[1]),
            planned_group("/m", "y", &[2]),
            planned_group("/m", "kept", &[3]),
        ];
        let stored = [
            planned_row(1, "/m", "blue", true),
            planned_row(2, "/m", "gone", false),
            // Its only group already has a row of its own, so no re-key.
            planned_row(3, "/m", "kept", false),
            planned_row(4, "/m", "moved", true),
        ];
        let plan = plan_rebuild(
            &groups,
            &stored,
            &membership(&[(1, &[1, 2]), (3, &[3]), (4, &[3])]),
        );
        assert_eq!(
            plan.targets,
            vec![Target::New, Target::New, Target::Existing(3)]
        );
        assert_eq!(plan.removed, vec![1, 2, 4]);
    }
}
