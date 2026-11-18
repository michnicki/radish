use crate::grouping::{self, FolderMode, MERGED_KEY, RuleFile, TitleGroup, Units, directory_of};
use crate::layout::plural;
use crate::matcher::{
    self, ArtistKind, Candidate, EmbeddedReleaseEvidence, LocalRelease, LocalTrack, Mapping,
    MatchScope, RemoteRelease, RemoteTrack,
};
use crate::providers::{GroupId, Mbid, ProviderKind, ReleaseId};
use crate::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use anyhow::{Result, bail};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

mod folders;
pub use folders::*;

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

/// Returns the ID of the imported root that `path` counts toward.
///
/// See [`claim_root`], which this wraps.
///
/// # Errors
///
/// Returns an error when the database cannot be read or written.
pub async fn upsert_directory(pool: &SqlitePool, path: &Path) -> Result<i64> {
    Ok(claim_root(pool, path).await?.id)
}

/// The imported root a path counts toward, and the roots it took over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootClaim {
    /// The `directories` row every track under the path points to.
    pub id: i64,
    /// Roots that were inside it and are now part of it, in path order.
    pub absorbed: Vec<String>,
}

/// Finds or adds the imported root for `path`, so that roots never nest.
///
/// An existing root at or above `path` is used as it is, the outermost one
/// when several are. Otherwise `path` becomes a new root. Either way, every
/// root inside the chosen one is taken over: its tracks point to the chosen
/// root and its row goes. No track is read, changed otherwise, or deleted,
/// and roots play no part in grouping or matching.
///
/// # Errors
///
/// Returns an error when the database cannot be read or written.
pub async fn claim_root(pool: &SqlitePool, path: &Path) -> Result<RootClaim> {
    let root_key = |path: &str| path_key(Path::new(path)).trim_end_matches('/').to_string();
    let path_str = path.to_string_lossy().to_string();
    let key = root_key(&path_str);

    let mut tx = begin_write(pool).await?;
    let mut roots: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM directories")
        .fetch_all(&mut *tx)
        .await?;
    roots.sort_by_key(|(_, path)| root_key(path).len());

    let covering = roots
        .iter()
        .find(|(_, root)| key_within(&key, &root_key(root)));
    let (id, chosen) = match covering {
        Some((id, root)) => (*id, root_key(root)),
        None => {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO directories (path) VALUES (?1)
                 ON CONFLICT(path) DO UPDATE SET path = excluded.path
                 RETURNING id",
            )
            .bind(&path_str)
            .fetch_one(&mut *tx)
            .await?;
            (id, key)
        }
    };

    let mut absorbed = Vec::new();
    for (inner, root) in &roots {
        if *inner == id || !key_within(&root_key(root), &chosen) {
            continue;
        }
        // Tracks first: deleting a root row deletes its tracks.
        sqlx::query("UPDATE tracks SET directory_id = ?1 WHERE directory_id = ?2")
            .bind(id)
            .bind(inner)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM directories WHERE id = ?1")
            .bind(inner)
            .execute(&mut *tx)
            .await?;
        absorbed.push(root.clone());
    }
    tx.commit().await?;

    absorbed.sort_by_key(|root| root_key(root));
    Ok(RootClaim { id, absorbed })
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
            path = excluded.path,
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
         WHERE tracks.path COLLATE BINARY <> excluded.path OR (
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

/// The comparison key for a file path: forward slashes and ASCII lowercase,
/// matching how the path columns compare. Use it to look paths up in the
/// result of [`known_tracks`].
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// True when `key` is `prefix_key` or lives beneath it. Both are [`path_key`]
/// values, and `prefix_key` has no trailing slash. Whole components only, so
/// `/music/blue` does not contain `/music/bluegrass`.
pub(crate) fn key_within(key: &str, prefix_key: &str) -> bool {
    key == prefix_key
        || key
            .strip_prefix(prefix_key)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// True for a normalized scope that covers a whole volume: a bare drive such as
/// `C:`, or a UNC server or share root such as `//server/share`. Pure string
/// logic, so it behaves the same on every platform.
fn is_volume_root(normalized: &str) -> bool {
    let bytes = normalized.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    normalized
        .strip_prefix("//")
        .is_some_and(|unc| unc.split('/').filter(|part| !part.is_empty()).count() <= 2)
}

/// Deletes the track rows for a file, or for every file under a directory,
/// and returns how many were removed. Directories match whole path components
/// only, so `/music/Blue` never touches `/music/Bluegrass`, and matching is
/// case-insensitive. The library root, a bare drive and a UNC share root are
/// refused, so an empty or mistaken path can never wipe the whole library.
pub async fn remove_tracks_under(pool: &SqlitePool, path: &Path) -> Result<u64> {
    let Some(root) = scope_param(path).filter(|root| !is_volume_root(root)) else {
        bail!(
            "refusing to remove every track under {}: it is a filesystem or volume root",
            path.display()
        );
    };
    let result = sqlx::query(
        "DELETE FROM tracks
         WHERE replace(path, '\\', '/') = ?1 COLLATE NOCASE
            OR substr(replace(path, '\\', '/'), 1, length(?1) + 1) = (?1 || '/') COLLATE NOCASE",
    )
    .bind(root)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// What the database remembers about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownTrack {
    /// The path exactly as stored, with its original casing.
    pub path: String,
    pub modified_at: i64,
}

/// The path as stored for this file, with its original casing, when a row
/// exists. The lookup ignores letter case, like the `path` column.
pub async fn stored_path(pool: &SqlitePool, path: &Path) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT path FROM tracks WHERE path = ?1")
            .bind(path.to_string_lossy())
            .fetch_optional(pool)
            .await?,
    )
}

/// Every stored file under `root`, keyed by [`path_key`] of the stored path.
/// An import compares them with the files on disk to find what changed.
pub async fn known_tracks(pool: &SqlitePool, root: &Path) -> Result<HashMap<String, KnownTrack>> {
    let root = scope_param(root);
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT path, modified_at FROM tracks
         WHERE (?1 IS NULL
                OR substr(replace(path, '\\', '/'), 1, length(?1) + 1) = (?1 || '/') COLLATE NOCASE)",
    )
    .bind(root)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, modified_at)| (path_key(Path::new(&path)), KnownTrack { path, modified_at }))
        .collect())
}

/// Deletes the rows with exactly these stored paths and returns how many went.
///
/// Each delete is an index lookup on the unique, case-insensitive `path`
/// column, and every [`UPSERT_BATCH_SIZE`] deletes share one write
/// transaction. Use it when the stored paths are known; [`remove_tracks_under`]
/// scans the table and is meant for one path at a time.
pub async fn remove_tracks_by_paths(pool: &SqlitePool, paths: &[String]) -> Result<u64> {
    let mut removed = 0;
    for chunk in paths.chunks(UPSERT_BATCH_SIZE) {
        let mut tx = begin_write(pool).await?;
        for path in chunk {
            removed += sqlx::query("DELETE FROM tracks WHERE path = ?1")
                .bind(path)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(removed)
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
    pub unit: String,
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
        // Fixed-width tags (ID3v1) pad values with spaces; keep the value
        // without them.
        let entry = counts
            .entry(normalized)
            .or_insert_with(|| (0, value.trim().to_string()));
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

/// FNV-1a over the bytes. Stable across builds and platforms, which a
/// `Hasher` from the standard library does not promise.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
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
/// upgrade explicit. A merged release (`merged`) hashes a marker as well, so
/// merging or splitting a unit always changes the revision, while every
/// unmerged revision stays exactly what it was.
fn revision_of(tracks: &[&ObservedTrack], merged: bool) -> String {
    let mut rows: Vec<&ObservedTrack> = tracks.to_vec();
    rows.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });

    let mut payload = String::from("release-input-v2:");
    if merged {
        payload.push_str("merged:");
    }
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

/// What `group_tracks` needs besides the files.
#[derive(Debug, Clone, Default)]
pub struct GroupingContext {
    /// Folder decisions by lowercased unit path.
    pub decisions: HashMap<String, FolderMode>,
    /// The stored releases in scope before this regroup.
    pub prior: Vec<PriorRelease>,
}

/// A stored release as grouping sees it.
#[derive(Debug, Clone)]
pub struct PriorRelease {
    /// The directory the release is stored under.
    pub directory: String,
    /// Whether it is a merged release (`album_title_norm` is [`MERGED_KEY`]).
    pub merged: bool,
    /// Whether it holds an accepted match.
    pub has_match: bool,
}

/// What a unit's stored releases say about its shape.
#[derive(Debug, Clone, Copy, Default)]
struct PriorShape {
    /// Some release of the unit holds an accepted match.
    has_match: bool,
    /// Some release of the unit is a merged release.
    merged: bool,
}

/// How one unit is grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// By folder and album title.
    Separate,
    /// Every file forms one release: a `merge` decision.
    MergeAll,
    /// Every file with a usable album title forms one release; untagged and
    /// junk-tagged files keep their own groups. The automatic rule's merge.
    MergeTitled,
}

/// How a group came about.
enum GroupKind {
    /// One album title in one folder.
    Title,
    /// A merged unit, with the album title it shows.
    Merged(Option<String>),
}

/// The album title of one of today's groups when it is usable: present and
/// not junk.
fn usable_title<'a>(album_title_norm: &str, members: &[&'a ObservedTrack]) -> Option<&'a str> {
    if album_title_norm.is_empty() {
        return None;
    }
    let first: &'a ObservedTrack = members.first()?;
    let title = first.album.as_deref()?.trim();
    (!matcher::is_junk_title(title)).then_some(title)
}

/// One of today's groups as the automatic rule sees it.
fn title_group<'a>(title: &'a str, members: &[&'a ObservedTrack]) -> TitleGroup<'a> {
    TitleGroup {
        title,
        files: members
            .iter()
            .map(|&track| RuleFile {
                path: &track.path,
                album: track.album.as_deref(),
                disc_tag: track.disc_number,
                track_number: track.track_number,
                title: track.title.as_deref(),
                artist: track.artist.as_deref(),
                duration_secs: track.duration_secs,
            })
            .collect(),
    }
}

/// Groups observed tracks into local releases.
///
/// Today's grouping is by directory plus normalized album title, so separate
/// directories never merge by themselves. A unit (a folder, or a folder with
/// two or more disc subfolders, see [`grouping::Units`]) may instead become one
/// merged release, keyed [`MERGED_KEY`] at the unit's path: by the user's
/// decision in `context`, or by [`grouping::auto_merges`] when no release of
/// the unit holds an accepted match. A unit that holds one keeps the shape it
/// had.
pub fn group_tracks(tracks: &[ObservedTrack], context: &GroupingContext) -> Vec<GroupedRelease> {
    let mut by_title: BTreeMap<(String, String), Vec<&ObservedTrack>> = BTreeMap::new();
    for track in tracks {
        let directory = directory_of(&track.path);
        let album_title = track
            .album
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let album_title_norm = album_title.and_then(identity_key).unwrap_or_default();
        by_title
            .entry((directory.to_ascii_lowercase(), album_title_norm))
            .or_default()
            .push(track);
    }

    let directories: Vec<String> = by_title
        .values()
        .map(|members| directory_of(&members[0].path))
        .collect();
    let units = Units::new(directories.iter().map(String::as_str));

    // Per lowercased unit path. Each stored release is mapped through today's
    // units, and a unit counts as merged when any of its releases is. That is
    // deliberate: a matched merged `CD1` keeps its match when a `CD2` appears.
    let mut prior: HashMap<String, PriorShape> = HashMap::new();
    for release in &context.prior {
        let shape = prior
            .entry(units.unit_of(&release.directory).to_ascii_lowercase())
            .or_default();
        shape.has_match |= release.has_match;
        shape.merged |= release.merged;
    }

    type TitleGroups<'t> = Vec<(String, Vec<&'t ObservedTrack>)>;
    let mut by_unit: BTreeMap<String, (String, TitleGroups<'_>)> = BTreeMap::new();
    for ((_, album_title_norm), members) in by_title {
        let unit = units.unit_of(&directory_of(&members[0].path));
        by_unit
            .entry(unit.to_ascii_lowercase())
            .or_insert_with(|| (unit, Vec::new()))
            .1
            .push((album_title_norm, members));
    }

    let mut groups = Vec::new();
    for (unit_key, (unit, title_groups)) in by_unit {
        let mut titled = HashSet::new();
        let mut rule_groups = Vec::new();
        for (index, (norm, members)) in title_groups.iter().enumerate() {
            if let Some(title) = usable_title(norm, members) {
                titled.insert(index);
                rule_groups.push(title_group(title, members));
            }
        }
        let shape = match context.decisions.get(&unit_key) {
            Some(FolderMode::Merge) => Shape::MergeAll,
            Some(FolderMode::Separate) => Shape::Separate,
            None => match prior.get(&unit_key) {
                Some(&PriorShape {
                    has_match: true,
                    merged: true,
                }) => Shape::MergeTitled,
                Some(&PriorShape {
                    has_match: true,
                    merged: false,
                }) => Shape::Separate,
                _ if grouping::auto_merges(&unit, &rule_groups) => Shape::MergeTitled,
                _ => Shape::Separate,
            },
        };

        let title = if shape == Shape::Separate {
            None
        } else {
            grouping::merged_title(&rule_groups)
        };
        let mut merged: Vec<&ObservedTrack> = Vec::new();
        for (index, (norm, members)) in title_groups.into_iter().enumerate() {
            let joins = match shape {
                Shape::Separate => false,
                Shape::MergeAll => true,
                Shape::MergeTitled => titled.contains(&index),
            };
            if joins {
                merged.extend(members);
            } else {
                let directory = directory_of(&members[0].path);
                groups.push(release_of(
                    directory,
                    unit.clone(),
                    norm,
                    members,
                    GroupKind::Title,
                ));
            }
        }
        if !merged.is_empty() {
            merged.sort_by_cached_key(|track| track.path.to_ascii_lowercase());
            groups.push(release_of(
                unit.clone(),
                unit,
                MERGED_KEY.to_string(),
                merged,
                GroupKind::Merged(title),
            ));
        }
    }
    groups.sort_by(|a, b| {
        release_key(&a.directory, &a.album_title_norm)
            .cmp(&release_key(&b.directory, &b.album_title_norm))
    });
    groups
}

/// The release a group of files forms.
fn release_of(
    directory: String,
    unit: String,
    album_title_norm: String,
    members: Vec<&ObservedTrack>,
    kind: GroupKind,
) -> GroupedRelease {
    let (album_title, merged) = match kind {
        GroupKind::Title => (
            members
                .iter()
                .filter_map(|track| track.album.as_deref())
                .map(str::trim)
                .find(|title| identity_key(title).as_deref() == Some(album_title_norm.as_str()))
                .map(str::to_string),
            false,
        ),
        GroupKind::Merged(title) => (title, true),
    };

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
            let compilation = normalized.starts_with("various artist") || normalized == "various";
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
                most_common_value(track_artists.iter().cloned()).map(|(_, original)| original),
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

    let barcode = consistent_value(members.iter().filter_map(|track| track.barcode.as_deref()));
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

    let revision = revision_of(&members, merged);
    let has_album_tag = if merged {
        album_title.is_some()
    } else {
        !album_title_norm.is_empty()
    };

    let mut track_ids: Vec<i64> = members.iter().map(|track| track.id).collect();
    track_ids.sort_unstable();

    GroupedRelease {
        directory,
        unit,
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

    // The SQL prefix is a coarse filter on slash-normalized paths. A file name
    // that contains a backslash can pass it while its directory is outside the
    // scope, so apply the same directory test the release rows use.
    if let Some(scope) = scope {
        rows.retain(|track| path_within(&directory_of(&track.path), Path::new(scope)));
    }

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
    key_within(&candidate, &root)
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
    unit: String,
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
            unit: group.unit.clone(),
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
    unit_path: String,
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
    has_fresh_match: bool,
}

/// A persisted local release as seen by the rebuild planner.
#[derive(Debug, Clone)]
struct StoredRelease {
    id: i64,
    fields: ReleaseFields,
    has_match: bool,
    /// Whether it holds an accepted match that is not stale yet.
    has_fresh_match: bool,
}

impl From<StoredReleaseRow> for StoredRelease {
    fn from(row: StoredReleaseRow) -> Self {
        Self {
            id: row.id,
            has_match: row.has_match,
            has_fresh_match: row.has_fresh_match,
            fields: ReleaseFields {
                directory: row.directory_path,
                unit: row.unit_path,
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

/// The select list of the stored-release queries, followed by `$tail`.
macro_rules! stored_select {
    ($tail:literal) => {
        concat!(
            "SELECT lr.id, lr.directory_path, lr.unit_path, lr.album_title_norm, lr.album_title,
                    lr.album_artist, lr.artist_kind, lr.year, lr.barcode, lr.catalog_number,
                    lr.embedded_discogs_id, lr.is_compilation, lr.has_album_tag, lr.track_count,
                    lr.source_revision, lr.embedded_musicbrainz_release_id,
                    lr.embedded_musicbrainz_release_group_id, lr.musicbrainz_problems_json,
                    EXISTS (SELECT 1 FROM release_matches m WHERE m.local_release_id = lr.id)
                        AS has_match,
                    EXISTS (SELECT 1 FROM release_matches m
                            WHERE m.local_release_id = lr.id AND m.stale = 0)
                        AS has_fresh_match
             FROM local_releases lr ",
            $tail
        )
    };
}

/// Loads the stored releases whose directory is the scope or under it.
async fn existing_releases(
    conn: &mut SqliteConnection,
    scope: Option<&str>,
) -> Result<Vec<StoredRelease>> {
    let rows = sqlx::query_as::<_, StoredReleaseRow>(stored_select!(
        "WHERE (?1 IS NULL
                OR replace(lr.directory_path, '\\', '/') = ?1 COLLATE NOCASE
                OR substr(replace(lr.directory_path, '\\', '/'), 1, length(?1) + 1)
                    = (?1 || '/') COLLATE NOCASE)
         ORDER BY lr.id"
    ))
    .bind(scope)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(StoredRelease::from).collect())
}

/// Loads the stored releases with the given IDs, in ID order.
async fn stored_releases_by_id(
    conn: &mut SqliteConnection,
    ids: &[i64],
) -> Result<Vec<StoredRelease>> {
    let rows = sqlx::query_as::<_, StoredReleaseRow>(stored_select!(
        "WHERE lr.id IN (SELECT value FROM json_each(?1)) ORDER BY lr.id"
    ))
    .bind(serde_json::to_string(ids)?)
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
///
/// A scope root that is itself a disc subfolder widens to the folder above it
/// ([`grouping::unit_scope`]), so a unit is always regrouped whole.
pub async fn rebuild_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<RebuildSummary> {
    // Observe and write inside one write transaction, so a concurrent writer
    // can never leave this rebuild grouping stale data.
    let mut tx = begin_write(pool).await?;
    let summary = rebuild_in(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(summary)
}

/// The members of each stored release, by release ID, in ID order.
fn release_members(tracks: &[ObservedTrack]) -> HashMap<i64, Vec<i64>> {
    let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
    for track in tracks {
        if let Some(release_id) = track.local_release_id {
            members.entry(release_id).or_default().push(track.id);
        }
    }
    for ids in members.values_mut() {
        ids.sort_unstable();
    }
    members
}

/// The stored releases as grouping sees them.
fn prior_releases(stored: &[StoredRelease]) -> Vec<PriorRelease> {
    stored
        .iter()
        .map(|row| PriorRelease {
            directory: row.fields.directory.clone(),
            merged: row.fields.album_title_norm == MERGED_KEY,
            has_match: row.has_match,
        })
        .collect()
}

/// [`rebuild_local_releases`] inside the caller's write transaction.
async fn rebuild_in(conn: &mut SqliteConnection, scope: Option<&Path>) -> Result<RebuildSummary> {
    let widened = scope.map(grouping::unit_scope);
    let scope = widened.as_deref().and_then(scope_param);
    let scope = scope.as_deref();

    let tracks = observed_tracks(conn, scope).await?;
    let stored = existing_releases(conn, scope).await?;
    let context = GroupingContext {
        decisions: folder_decisions(conn).await?,
        prior: prior_releases(&stored),
    };
    let groups = group_tracks(&tracks, &context);
    let members = release_members(&tracks);

    let plan = plan_rebuild(&groups, &stored, &members);
    let stored_by_id: HashMap<i64, &StoredRelease> =
        stored.iter().map(|row| (row.id, row)).collect();

    let mut summary = RebuildSummary {
        groups: groups.len(),
        ..RebuildSummary::default()
    };

    for (group, target) in groups.iter().zip(&plan.targets) {
        let fields = ReleaseFields::of_group(group)?;
        let mut membership_changed = false;
        let (id, previous_revision) = match *target {
            Target::Existing(id) => {
                let row = stored_by_id[&id];
                let same_members = members.get(&id).map(Vec::as_slice).unwrap_or_default()
                    == group.track_ids.as_slice();
                if row.fields == fields && same_members {
                    summary.unchanged += 1;
                    continue;
                }
                update_release(conn, id, &fields).await?;
                summary.updated += 1;
                // The revision hashes tag values and paths, not track IDs. A
                // file removed and re-added keeps the revision but loses its
                // overlay row, so changed membership alone makes a match stale.
                membership_changed = !same_members && row.has_match;
                (id, Some(row.fields.revision.as_str()))
            }
            Target::Rekey(id) => {
                update_release(conn, id, &fields).await?;
                summary.rekeyed += 1;
                (id, Some(stored_by_id[&id].fields.revision.as_str()))
            }
            Target::New => {
                let id = insert_release(conn, &fields).await?;
                summary.created += 1;
                (id, None)
            }
        };

        if membership_changed {
            sqlx::query(
                "UPDATE local_releases SET status = 'stale' WHERE id = ?1 AND status = 'matched'",
            )
            .bind(id)
            .execute(&mut *conn)
            .await?;
        }
        if membership_changed
            || previous_revision.is_some_and(|previous| previous != fields.revision)
        {
            let marked = sqlx::query(
                "UPDATE release_matches SET stale = 1
                 WHERE local_release_id = ?1 AND stale = 0",
            )
            .bind(id)
            .execute(&mut *conn)
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
        .execute(&mut *conn)
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
        .execute(&mut *conn)
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
            .execute(&mut *conn)
            .await?;
    }

    Ok(summary)
}

async fn insert_release(conn: &mut SqliteConnection, fields: &ReleaseFields) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO local_releases (
            directory_path, album_title_norm, album_title, album_artist, artist_kind, year,
            barcode, catalog_number, embedded_discogs_id, is_compilation, has_album_tag,
            track_count, source_revision, status, embedded_musicbrainz_release_id,
            embedded_musicbrainz_release_group_id, musicbrainz_problems_json, unit_path
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
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
    .bind(&fields.unit)
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
            unit_path = ?18,
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
    .bind(&fields.unit)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LocalReleaseRow {
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
    album: Option<String>,
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
    tracks.sort_by(crate::matcher::local_track_order);
}

fn parse_musicbrainz_problems(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Loads persisted local releases (with their tracks) for matching.
pub async fn load_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalRelease>> {
    load_selected_releases(pool, scope, None).await
}

/// Loads local releases with their tracks, filtered in SQL: only releases whose
/// directory is `scope` or under it, and only the release with ID `id`. Either
/// filter may be absent. A scope that is a disc subfolder also takes the merged
/// release in the folder above it, when that release holds files under the
/// scope. Tracks come back in `order_release_tracks` order.
pub async fn load_selected_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
    id: Option<i64>,
) -> Result<Vec<LocalRelease>> {
    load_releases(pool, scope, id, None).await
}

/// The releases under `scope` that wait for review by one of `providers`.
///
/// A release waits for review when it is not matched and has a saved review.
/// Only reviews of `providers` count, so a run that could not load a release's
/// only review does not select it and search again.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn load_queued_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
    providers: &[ProviderKind],
) -> Result<Vec<LocalRelease>> {
    load_releases(pool, scope, None, Some(providers)).await
}

/// Loads releases and their tracks in one read transaction.
///
/// `queued` limits the result to unmatched releases with a saved review from
/// one of the listed providers; `None` does not look at reviews.
async fn load_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
    id: Option<i64>,
    queued: Option<&[ProviderKind]>,
) -> Result<Vec<LocalRelease>> {
    // The providers whose saved reviews select a release, as a JSON array for
    // `json_each`; `None` selects without looking at reviews.
    let queued = queued
        .map(|providers| {
            serde_json::to_string(&providers.iter().map(|p| p.as_str()).collect::<Vec<_>>())
        })
        .transpose()?;

    // A merged release lives at its unit's folder, above the disc subfolder
    // the scope may name; take that release too, and nothing else from there.
    let merged_parent = scope
        .and_then(grouping::disc_parent_of)
        .and_then(|parent| scope_param(&parent));
    let scope = scope.and_then(scope_param);
    let scope = scope.as_deref();

    // One read transaction, so the releases and their tracks come from the same
    // snapshot even while an import or rebuild commits in between.
    let mut tx = pool.begin().await?;

    let release_rows = sqlx::query_as::<_, LocalReleaseRow>(
        "SELECT lr.id, lr.directory_path, lr.album_title_norm, lr.album_title, lr.album_artist,
                lr.artist_kind, lr.year, lr.barcode, lr.catalog_number, lr.embedded_discogs_id,
                lr.embedded_musicbrainz_release_id, lr.embedded_musicbrainz_release_group_id,
                lr.musicbrainz_problems_json, lr.is_compilation, lr.has_album_tag,
                lr.source_revision
         FROM local_releases lr
         WHERE ((?1 IS NULL
                 OR replace(lr.directory_path, '\\', '/') = ?1 COLLATE NOCASE
                 OR substr(replace(lr.directory_path, '\\', '/'), 1, length(?1) + 1)
                     = (?1 || '/') COLLATE NOCASE)
                OR (lr.album_title_norm = '*'
                    AND replace(lr.directory_path, '\\', '/') = ?3 COLLATE NOCASE
                    AND EXISTS (SELECT 1 FROM tracks m
                                WHERE m.local_release_id = lr.id
                                  AND substr(replace(m.path, '\\', '/'), 1, length(?1) + 1)
                                      = (?1 || '/') COLLATE NOCASE)))
           AND (?2 IS NULL OR lr.id = ?2)
           AND (?4 IS NULL
                OR (lr.status <> 'matched'
                    AND EXISTS (SELECT 1 FROM match_reviews r
                                WHERE r.local_release_id = lr.id
                                  AND r.provider IN (SELECT value FROM json_each(?4)))))
         ORDER BY lr.directory_path, lr.album_title_norm",
    )
    .bind(scope)
    .bind(id)
    .bind(merged_parent.as_deref())
    .bind(queued.as_deref())
    .fetch_all(&mut *tx)
    .await?;

    let track_rows = sqlx::query_as::<_, LocalTrackRow>(
        "SELECT t.id, t.path, t.title, t.disc_number, t.track_number, t.duration_secs,
                t.local_release_id, t.musicbrainz_ids_json, a.title AS album
         FROM tracks t
         JOIN local_releases lr ON lr.id = t.local_release_id
         LEFT JOIN albums a ON a.id = t.album_id
         WHERE ((?1 IS NULL
                 OR replace(lr.directory_path, '\\', '/') = ?1 COLLATE NOCASE
                 OR substr(replace(lr.directory_path, '\\', '/'), 1, length(?1) + 1)
                     = (?1 || '/') COLLATE NOCASE)
                OR (lr.album_title_norm = '*'
                    AND replace(lr.directory_path, '\\', '/') = ?3 COLLATE NOCASE
                    AND EXISTS (SELECT 1 FROM tracks m
                                WHERE m.local_release_id = lr.id
                                  AND substr(replace(m.path, '\\', '/'), 1, length(?1) + 1)
                                      = (?1 || '/') COLLATE NOCASE)))
           AND (?2 IS NULL OR lr.id = ?2)
           AND (?4 IS NULL
                OR (lr.status <> 'matched'
                    AND EXISTS (SELECT 1 FROM match_reviews r
                                WHERE r.local_release_id = lr.id
                                  AND r.provider IN (SELECT value FROM json_each(?4)))))
         ORDER BY t.path",
    )
    .bind(scope)
    .bind(id)
    .bind(merged_parent.as_deref())
    .bind(queued.as_deref())
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    // Inside a merged release, a file's disc comes from its disc subfolder or
    // the disc marker ending its own album tag, before its disc tag.
    let merged_units: HashMap<i64, &str> = release_rows
        .iter()
        .filter(|row| row.album_title_norm == MERGED_KEY)
        .map(|row| (row.id, row.directory_path.as_str()))
        .collect();
    let mut tracks_by_release: HashMap<i64, Vec<LocalTrack>> = HashMap::new();
    for mut row in track_rows {
        if let Some(unit) = merged_units.get(&row.local_release_id) {
            row.disc_number = grouping::effective_disc(
                unit,
                &directory_of(&row.path),
                row.album.as_deref(),
                row.disc_number,
            );
        }
        tracks_by_release
            .entry(row.local_release_id)
            .or_default()
            .push(row.into_local_track());
    }

    let mut releases = Vec::new();
    for row in release_rows {
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
            reads_directory: true,
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
    /// The unit the release belongs to (see [`grouping::Units`]).
    pub unit: String,
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub track_count: i64,
    pub status: String,
    pub provider: Option<ProviderKind>,
    pub remote_release_id: Option<String>,
    pub remote_url: Option<String>,
    pub remote_title: Option<String>,
    pub match_scope: Option<String>,
    /// The disc of the matched release that the files are, when they match one
    /// disc of a multi-disc release rather than all of it.
    pub medium: Option<i64>,
    pub stale: bool,
    pub queued_providers: Vec<ProviderKind>,
    /// How the release was merged, when it is a merged release.
    pub merged: Option<MergeInfo>,
}

/// What a merged release joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeInfo {
    /// Distinct album titles among its files; untagged files are not counted.
    pub titles: usize,
    /// Distinct folders its files are in.
    pub folders: usize,
    /// Whether the user merged the unit, rather than the automatic rule.
    pub manual: bool,
}

impl MergeInfo {
    /// `2 album titles, 1 folder (automatic)`.
    pub fn label(&self) -> String {
        format!(
            "{}, {} ({})",
            plural(self.titles as u64, "album title", "album titles"),
            plural(self.folders as u64, "folder", "folders"),
            if self.manual { "manual" } else { "automatic" }
        )
    }
}

impl LocalReleaseStatus {
    pub fn queued(&self) -> bool {
        !self.queued_providers.is_empty()
    }

    /// The status with its qualifiers, as `releases` and the web UI show it:
    /// `matched, album match; edition unconfirmed, stale, queued (musicbrainz)`.
    pub fn status_label(&self) -> String {
        let mut label = self.status.clone();
        if self.match_scope.as_deref() == Some("album") {
            label.push_str(", album match; edition unconfirmed");
        }
        if self.stale && self.status != STATUS_STALE {
            label.push_str(", stale");
        }
        if self.queued() && self.status != STATUS_MATCHED {
            let providers: Vec<&str> = self
                .queued_providers
                .iter()
                .map(|provider| provider.as_str())
                .collect();
            label.push_str(&format!(", queued ({})", providers.join(", ")));
        }
        label
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StatusRow {
    id: i64,
    directory_path: String,
    unit_path: String,
    album_title_norm: String,
    album_title: Option<String>,
    album_artist: Option<String>,
    track_count: i64,
    status: String,
    provider: Option<String>,
    remote_release_id: Option<String>,
    remote_url: Option<String>,
    remote_title: Option<String>,
    match_scope: Option<String>,
    medium: Option<i64>,
    stale: Option<i64>,
}

pub async fn list_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalReleaseStatus>> {
    // A disc-subfolder scope also lists the merged release above it, when that
    // release holds files under the scope.
    let merged_parent = scope
        .and_then(grouping::disc_parent_of)
        .map(|parent| path_key(&parent));
    let merged_id: Option<i64> = match (&merged_parent, scope.and_then(scope_param)) {
        (Some(parent), Some(scope)) => {
            sqlx::query_scalar(
                "SELECT lr.id FROM local_releases lr
                 WHERE lr.album_title_norm = ?3
                   AND replace(lr.directory_path, '\\', '/') = ?2 COLLATE NOCASE
                   AND EXISTS (SELECT 1 FROM tracks t WHERE t.local_release_id = lr.id
                               AND substr(replace(t.path, '\\', '/'), 1, length(?1) + 1)
                                   = (?1 || '/') COLLATE NOCASE)",
            )
            .bind(scope)
            .bind(parent)
            .bind(MERGED_KEY)
            .fetch_optional(pool)
            .await?
        }
        _ => None,
    };
    let mut conn = pool.acquire().await?;
    let (statuses, _) = status_listing(&mut conn, None).await?;
    drop(conn);

    Ok(statuses
        .into_iter()
        .filter(|status| match scope {
            Some(scope) => path_within(&status.directory, scope) || Some(status.id) == merged_id,
            None => true,
        })
        .collect())
}

/// The select list and joins every status query shares, followed by `$tail`.
macro_rules! status_select {
    ($tail:literal) => {
        concat!(
            "SELECT lr.id, lr.directory_path, lr.unit_path, lr.album_title_norm, lr.album_title,
                    lr.album_artist, lr.track_count,
                    lr.status, m.provider AS provider, m.remote_release_id AS remote_release_id,
                    m.release_url AS remote_url, m.title AS remote_title, m.stale AS stale,
                    m.match_scope, m.medium_position AS medium
             FROM local_releases lr
             LEFT JOIN release_matches m ON m.local_release_id = lr.id ",
            $tail
        )
    };
}

/// The statuses of every release, or of the releases of one `unit`, in listing
/// order, with the folder decisions they were read under.
///
/// A `unit` reads only that unit's rows (by `idx_local_releases_unit`) and the
/// queued reviews and merged files of those rows.
async fn status_listing(
    conn: &mut SqliteConnection,
    unit: Option<&str>,
) -> Result<(Vec<LocalReleaseStatus>, HashMap<String, FolderMode>)> {
    let rows = match unit {
        None => {
            sqlx::query_as::<_, StatusRow>(status_select!(
                "ORDER BY lr.directory_path, lr.album_title_norm"
            ))
            .fetch_all(&mut *conn)
            .await?
        }
        Some(unit) => {
            sqlx::query_as::<_, StatusRow>(status_select!(
                "WHERE lr.unit_path = ?1 ORDER BY lr.directory_path, lr.album_title_norm"
            ))
            .bind(unit)
            .fetch_all(&mut *conn)
            .await?
        }
    };
    let ids = unit
        .map(|_| serde_json::to_string(&rows.iter().map(|row| row.id).collect::<Vec<_>>()))
        .transpose()?;

    let queued =
        match &ids {
            None => sqlx::query_as::<_, (i64, String)>(
                "SELECT local_release_id, provider FROM match_reviews ORDER BY local_release_id",
            )
            .fetch_all(&mut *conn)
            .await?,
            Some(ids) => {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT local_release_id, provider FROM match_reviews
                 WHERE local_release_id IN (SELECT value FROM json_each(?1))
                 ORDER BY local_release_id",
                )
                .bind(ids)
                .fetch_all(&mut *conn)
                .await?
            }
        };
    let mut queued_by_release: HashMap<i64, Vec<ProviderKind>> = HashMap::new();
    for (local_release_id, provider) in queued {
        if let Some(provider) = ProviderKind::parse(&provider) {
            queued_by_release
                .entry(local_release_id)
                .or_default()
                .push(provider);
        }
    }
    // Provider order is the enum's (Discogs, MusicBrainz, Deezer), not the
    // alphabetical order of the stored names.
    for providers in queued_by_release.values_mut() {
        providers.sort();
    }

    let decisions = folder_decisions(conn).await?;
    let merged_files: Vec<(i64, String, Option<String>)> = match &ids {
        None => {
            sqlx::query_as(
                "SELECT t.local_release_id, t.path, a.title
                 FROM tracks t
                 JOIN local_releases lr ON lr.id = t.local_release_id
                 LEFT JOIN albums a ON a.id = t.album_id
                 WHERE lr.album_title_norm = ?1",
            )
            .bind(MERGED_KEY)
            .fetch_all(&mut *conn)
            .await?
        }
        Some(ids) => {
            sqlx::query_as(
                "SELECT t.local_release_id, t.path, a.title
                 FROM tracks t
                 JOIN local_releases lr ON lr.id = t.local_release_id
                 LEFT JOIN albums a ON a.id = t.album_id
                 WHERE lr.album_title_norm = ?1
                   AND t.local_release_id IN (SELECT value FROM json_each(?2))",
            )
            .bind(MERGED_KEY)
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?
        }
    };
    let mut merged_parts: HashMap<i64, (HashSet<String>, HashSet<String>)> = HashMap::new();
    for (release_id, path, album) in merged_files {
        let parts = merged_parts.entry(release_id).or_default();
        if let Some(key) = album.as_deref().and_then(identity_key) {
            parts.0.insert(key);
        }
        parts.1.insert(directory_of(&path).to_ascii_lowercase());
    }

    let statuses = rows
        .into_iter()
        .map(|row| {
            let merged = (row.album_title_norm == MERGED_KEY).then(|| {
                let (titles, folders) = merged_parts.remove(&row.id).unwrap_or_default();
                MergeInfo {
                    titles: titles.len(),
                    folders: folders.len(),
                    manual: decisions.get(&row.unit_path.to_ascii_lowercase())
                        == Some(&FolderMode::Merge),
                }
            });
            LocalReleaseStatus {
                id: row.id,
                directory: row.directory_path,
                unit: row.unit_path,
                album_title: row.album_title,
                album_artist: row.album_artist,
                track_count: row.track_count,
                status: row.status,
                provider: row.provider.as_deref().and_then(ProviderKind::parse),
                remote_release_id: row.remote_release_id,
                remote_url: row.remote_url,
                remote_title: row.remote_title,
                match_scope: row.match_scope,
                medium: row.medium,
                stale: row.stale.unwrap_or(0) != 0,
                queued_providers: queued_by_release.remove(&row.id).unwrap_or_default(),
                merged,
            }
        })
        .collect();
    Ok((statuses, decisions))
}

/// One imported root directory and how many tracks are stored under it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DirectorySummary {
    pub path: String,
    pub tracks: i64,
}

/// What the library holds right now, derived from the stored rows.
///
/// This is a snapshot, not a history: the database does not record when an
/// import ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryStatus {
    pub directories: Vec<DirectorySummary>,
    pub tracks: i64,
    pub releases: i64,
    pub matched: i64,
    pub unmatched: i64,
    pub stale: i64,
    /// Units holding two or more releases, a merged release or a decision.
    pub folders: i64,
    /// Releases with a queued review that are not already matched.
    pub queued: i64,
    /// The newest file modification time stored, in seconds since the epoch.
    pub newest_modified: Option<i64>,
}

/// Reads the [`LibraryStatus`] without changing anything.
pub async fn library_status(pool: &SqlitePool) -> Result<LibraryStatus> {
    let directories = sqlx::query_as::<_, DirectorySummary>(
        "SELECT d.path AS path, COUNT(t.id) AS tracks
         FROM directories d
         LEFT JOIN tracks t ON t.directory_id = d.id
         GROUP BY d.id
         ORDER BY d.path",
    )
    .fetch_all(pool)
    .await?;

    let (tracks, newest_modified): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), MAX(modified_at) FROM tracks")
            .fetch_one(pool)
            .await?;

    let by_status = sqlx::query_as::<_, (String, i64)>(
        "SELECT status, COUNT(*) FROM local_releases GROUP BY status",
    )
    .fetch_all(pool)
    .await?;

    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT r.local_release_id)
         FROM match_reviews r
         JOIN local_releases lr ON lr.id = r.local_release_id
         WHERE lr.status <> ?1",
    )
    .bind(STATUS_MATCHED)
    .fetch_one(pool)
    .await?;

    let folders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
             SELECT unit_path FROM local_releases GROUP BY unit_path
             HAVING COUNT(*) >= 2 OR MAX(album_title_norm = ?1)
                 OR unit_path IN (SELECT directory_path FROM folder_groupings)
         )",
    )
    .bind(MERGED_KEY)
    .fetch_one(pool)
    .await?;

    let mut status = LibraryStatus {
        directories,
        tracks,
        folders,
        queued,
        newest_modified,
        ..LibraryStatus::default()
    };
    for (name, count) in by_status {
        status.releases += count;
        match name.as_str() {
            STATUS_MATCHED => status.matched = count,
            STATUS_UNMATCHED => status.unmatched = count,
            STATUS_STALE => status.stale = count,
            _ => {}
        }
    }
    Ok(status)
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
            group_id: legacy.master_id.and_then(GroupId::discogs_master),
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

/// Reads a stored Discogs master `0`, which Discogs reports for a release that
/// has no master, as no group (the rule of [`GroupId::discogs_master`]), so an
/// old snapshot never makes every masterless release one album.
fn normalize_stored_group(release: &mut RemoteRelease) {
    if release.group_id == Some(GroupId::DiscogsMaster(0)) {
        release.group_id = None;
    }
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
            let mut release: RemoteRelease = serde_json::from_str(json)?;
            normalize_stored_group(&mut release);
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
                })
                .collect())
        }
        2 => {
            let mut candidates: Vec<StoredCandidate> = serde_json::from_str(json)?;
            for candidate in &mut candidates {
                normalize_stored_group(&mut candidate.release);
            }
            Ok(candidates)
        }
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

/// A tag as the listings show it. Fixed-width tags (ID3v1) pad values with
/// spaces, and a blank value counts as missing. The stored tags keep the
/// padding, since the source revision hashes them.
fn shown_tag(value: Option<String>) -> Option<String> {
    value.as_deref().and_then(non_empty)
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
    let local_release_id = release.id;
    if candidate.release.has_unsupported_entries() {
        bail!("refusing to accept a release with unsupported structure");
    }
    // Only the files being the first tracks of an album may leave tracks of the
    // release unmatched, and only at album scope. Those tracks get no row.
    if !matcher::mapping_can_apply(release, candidate, match_scope) {
        bail!("refusing to accept an incomplete track mapping");
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

    // The files are one disc of the release. Every track they map to must be on
    // it; checked before the write lock is taken.
    if let Some(medium) = candidate.mapping.medium {
        let track_media = candidate.release.track_media();
        for (_, remote_index) in candidate.mapping.assigned_pairs() {
            if track_media.get(remote_index).copied().flatten() != Some(medium) {
                bail!("refusing to accept a mapping with a track outside disc {medium}");
            }
        }
    }

    let mut tx = begin_write(pool).await?;

    let current: Option<(String, Option<i64>, String)> = sqlx::query_as(
        "SELECT lr.source_revision,
                (SELECT id FROM release_matches m WHERE m.local_release_id = lr.id),
                lr.album_title_norm
         FROM local_releases lr WHERE lr.id = ?1",
    )
    .bind(local_release_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((current_revision, current_match_id, album_title_norm)) = current else {
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
    let medium = candidate.mapping.medium;
    let edition = (match_scope == MatchScope::Edition).then_some(remote);
    let provider = remote.provider();
    let metadata = serde_json::to_value(remote)?;
    let algorithm_version = matcher::algorithm_version(provider).to_string();
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
            metadata_json, match_scope, medium_position
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, 0, ?18, ?19, ?20, ?21)
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
    .bind(medium.map(i64::from))
    .fetch_one(&mut *tx)
    .await?;

    let merged = album_title_norm == MERGED_KEY;
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
        // A disc matched on its own keeps its disc number at album scope too: the
        // folder said which disc it is. A file of a merged release keeps the
        // disc its folder or title gave it. Its track number at edition scope
        // comes from the disc's own position, which a Discogs release only
        // spells in its position string.
        .bind(
            medium
                .or_else(|| edition_track.and_then(|track| track.medium_position))
                .map(i64::from)
                .or_else(|| {
                    if merged {
                        tracks.get(local_index).and_then(|track| track.disc)
                    } else {
                        None
                    }
                }),
        )
        .bind(
            edition_track
                .and_then(|track| {
                    track.track_position.or_else(|| {
                        medium.and_then(|_| track.position_key().map(|(_, position)| position))
                    })
                })
                .map(i64::from),
        )
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

impl TrackRow {
    /// The title, or the file name when the track has none.
    pub fn display_title(&self) -> String {
        match &self.title {
            Some(title) => title.clone(),
            None => Path::new(&self.path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| self.path.clone()),
        }
    }
}

pub async fn list_tracks(pool: &SqlitePool) -> Result<Vec<TrackRow>> {
    track_rows(pool, None, NO_LIMIT, 0).await
}

/// SQLite treats a negative `LIMIT` as no limit.
const NO_LIMIT: i64 = -1;

/// One page of the effective track listing, optionally filtered like
/// [`search_tracks`]. Ask for `limit + 1` rows to learn whether a next page
/// exists.
pub async fn track_page(
    pool: &SqlitePool,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrackRow>> {
    track_rows(pool, query.map(like_contains), limit, offset).await
}

/// The one query behind `list`, `search` and the web track page.
///
/// `pattern` is a `LIKE` pattern from [`like_contains`], or `None` for every
/// track. The final `t.id` makes the order total, so pages never overlap.
async fn track_rows(
    pool: &SqlitePool,
    pattern: Option<String>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrackRow>> {
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
         WHERE ?1 IS NULL
            OR COALESCE(tm.remote_title, t.title) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
            OR COALESCE(tm.remote_artist, t.artist) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
            OR COALESCE(m.title, a.title) LIKE ?1 COLLATE NOCASE ESCAPE '\\'
         ORDER BY album, disc_number, track_number, title, t.id
         LIMIT ?2 OFFSET ?3",
    )
    .bind(pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| TrackRow {
            title: shown_tag(row.title),
            artist: shown_tag(row.artist),
            album: shown_tag(row.album),
            ..row
        })
        .collect())
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
    track_rows(pool, Some(like_contains(query)), NO_LIMIT, 0).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grouping::FolderAction;
    use crate::matcher::{RemoteTrack, ScoreBreakdown, TrackEntryType};
    use crate::scanner;
    use std::path::PathBuf;

    #[test]
    fn key_within_matches_the_prefix_and_whole_components_beneath_it() {
        assert!(key_within("/music/blue", "/music/blue"));
        assert!(key_within("/music/blue/01.flac", "/music/blue"));
        assert!(!key_within("/music/bluegrass/01.flac", "/music/blue"));
        assert!(!key_within("/music", "/music/blue"));
        // A filesystem root has an empty key once its trailing slash is trimmed.
        assert!(key_within("/music/blue", ""));
    }

    #[test]
    fn fnv1a64_matches_the_published_test_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

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

    fn observed(path: &str, album: &str) -> ObservedTrack {
        ObservedTrack {
            id: 1,
            path: path.to_string(),
            title: Some("One".to_string()),
            artist: Some("Artist".to_string()),
            album: Some(album.to_string()),
            album_artist: None,
            year: Some(2001),
            track_number: Some(1),
            disc_number: None,
            duration_secs: Some(180.0),
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz_ids_json: "{}".to_string(),
            local_release_id: None,
            musicbrainz_ids: ObservedMusicBrainzIds::default(),
        }
    }

    #[test]
    fn unmerged_revisions_are_unchanged_by_folder_merging() {
        let track = observed("/music/a/01.flac", "Blue");
        assert_eq!(
            revision_of(&[&track], false),
            "release-input-v2:7be3de220539888a"
        );
        assert_ne!(revision_of(&[&track], true), revision_of(&[&track], false));
        assert!(revision_of(&[&track], true).starts_with("release-input-v2:"));
    }

    #[tokio::test]
    async fn folder_groupings_accept_one_known_mode_per_folder() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO folder_groupings (directory_path, mode) VALUES ('/m/a', 'merge')")
            .execute(&pool)
            .await
            .unwrap();
        let same_folder = sqlx::query(
            "INSERT INTO folder_groupings (directory_path, mode) VALUES ('/M/A', 'separate')",
        )
        .execute(&pool)
        .await;
        assert!(same_folder.is_err(), "folder paths compare without case");
        let unknown = sqlx::query(
            "INSERT INTO folder_groupings (directory_path, mode) VALUES ('/m/b', 'maybe')",
        )
        .execute(&pool)
        .await;
        assert!(unknown.is_err());
        sqlx::query("SELECT unit_path FROM local_releases")
            .fetch_all(&pool)
            .await
            .unwrap();
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
            "radish-store-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ScratchDir(dir)
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
    async fn track_listings_drop_the_padding_of_fixed_width_tags() {
        let pool = test_pool().await;
        // ID3v1 fields are 30 characters wide and often padded with spaces.
        let album = "Blue                          ";
        let artist = "Artist                        ";
        let mut padded = scanned("/music/a/01.mp3", Some(album), Some(artist), 1);
        padded.title = Some("One                           ".to_string());
        let mut blank = scanned("/music/a/02.mp3", Some(album), Some(artist), 2);
        blank.title = Some(" ".repeat(30));
        insert(&pool, "/music/a", &[padded, blank]).await;

        for rows in [
            list_tracks(&pool).await.unwrap(),
            search_tracks(&pool, "Blue").await.unwrap(),
        ] {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].title.as_deref(), Some("One"));
            assert_eq!(rows[0].artist.as_deref(), Some("Artist"));
            assert_eq!(rows[0].album.as_deref(), Some("Blue"));
            assert_eq!(rows[1].title, None);
            assert_eq!(rows[1].display_title(), "02.mp3");
        }

        let view = folder_view(&pool, "/music/a").await.unwrap().unwrap();
        assert_eq!(view.files[0].album.as_deref(), Some("Blue"));
        assert_eq!(view.files[0].title.as_deref(), Some("One"));
        assert_eq!(view.files[1].title, None);
    }

    #[tokio::test]
    async fn release_artists_drop_the_padding_of_fixed_width_tags() {
        let pool = test_pool().await;
        // ID3v1 fields are 30 characters wide and often padded with spaces.
        let mut tag = scanned("/music/tag/01.mp3", Some("Blue"), None, 1);
        tag.album_artist = Some("Album Artist                  ".to_string());
        tag.artist = Some("Track Artist                  ".to_string());
        let mut untagged = scanned("/music/untagged/01.mp3", Some("Red"), None, 1);
        untagged.artist = Some("Stereophonics                 ".to_string());
        insert(&pool, "/music/tag", &[tag]).await;
        insert(&pool, "/music/untagged", &[untagged]).await;

        let releases = load_local_releases(&pool, None).await.unwrap();
        let artists: Vec<_> = releases
            .iter()
            .map(|release| release.album_artist.as_deref())
            .collect();
        assert_eq!(artists, [Some("Album Artist"), Some("Stereophonics")]);
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
    async fn the_first_tracks_of_a_release_are_applied_at_album_scope_only() {
        let pool = test_pool().await;
        let files: Vec<ScannedTrack> = (1..=5)
            .map(|number| {
                scanned(
                    &format!("/music/a/{number:02}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    number,
                )
            })
            .collect();
        insert(&pool, "/music/a", &files).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        let titles: Vec<String> = (1..=6).map(|number| format!("Track {number}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let candidate = Candidate::new(&release, remote_with(&titles));
        assert!(!candidate.mapping.complete);
        assert!(matcher::leading_tracks(&release, &candidate).is_some());

        let rows = |pool: SqlitePool| async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM release_track_matches")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        // An edition match claims the release's own tracklist: refused.
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
        assert!(error.to_string().contains("incomplete"), "{error}");
        assert_eq!(rows(pool.clone()).await, 0);

        // An album match applies the five files and nothing for the sixth track.
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Manual,
            MatchScope::Album,
            &guard,
        )
        .await
        .unwrap();
        assert_eq!(rows(pool.clone()).await, 5);
        let ordinals: Vec<i64> = sqlx::query_scalar(
            "SELECT remote_ordinal FROM release_track_matches ORDER BY remote_ordinal",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ordinals, vec![1, 2, 3, 4, 5]);
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 1);
    }

    #[tokio::test]
    async fn another_subset_of_a_release_is_never_applied() {
        let pool = test_pool().await;
        let files: Vec<ScannedTrack> = (1..=3)
            .map(|number| {
                scanned(
                    &format!("/music/a/{number:02}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    number,
                )
            })
            .collect();
        insert(&pool, "/music/a", &files).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        let titles: Vec<String> = (1..=6).map(|number| format!("Track {number}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let candidate = Candidate::new(&release, remote_with(&titles));
        assert!(!candidate.mapping.complete);
        // Three of six: the leading shape, but too few files and too small a share.
        assert!(matcher::leading_tracks(&release, &candidate).is_none());

        for scope in [MatchScope::Album, MatchScope::Edition] {
            let result = accept_match(
                &pool,
                &release,
                &candidate,
                AcceptanceMethod::Manual,
                scope,
                &guard,
            )
            .await;
            assert!(result.is_err(), "{scope:?}");
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn a_subset_with_a_missing_middle_track_is_never_applied() {
        let pool = test_pool().await;
        // Six of eight tracks, enough files and enough of the release, but the
        // fourth track has no file: not the first tracks.
        let files: Vec<ScannedTrack> = [1, 2, 3, 5, 6, 7]
            .into_iter()
            .map(|number| {
                scanned(
                    &format!("/music/a/{number:02}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    number,
                )
            })
            .collect();
        insert(&pool, "/music/a", &files).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        let titles: Vec<String> = (1..=8).map(|number| format!("Track {number}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let candidate = Candidate::new(&release, remote_with(&titles));
        assert!(!candidate.mapping.complete && !candidate.mapping.ambiguous);
        assert_eq!(release.tracks.len(), 6);
        assert!(matcher::leading_tracks(&release, &candidate).is_none());

        for scope in [MatchScope::Album, MatchScope::Edition] {
            let error = accept_match(
                &pool,
                &release,
                &candidate,
                AcceptanceMethod::Manual,
                scope,
                &guard,
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("incomplete"),
                "{scope:?}: {error}"
            );
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
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

    #[tokio::test]
    async fn a_queued_deezer_review_is_listed_after_the_others() {
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
        };
        // Queued out of order, and `deezer` sorts before `discogs` as text.
        for provider in [
            ProviderKind::Deezer,
            ProviderKind::MusicBrainz,
            ProviderKind::Discogs,
        ] {
            queue_review(
                &pool,
                release.id,
                provider,
                &release.revision,
                None,
                std::slice::from_ref(&stored),
                &candidate.mapping,
                &["uncertain".to_string()],
            )
            .await
            .unwrap();
        }

        let listed = list_local_releases(&pool, None).await.unwrap();
        assert_eq!(
            listed[0].queued_providers,
            vec![
                ProviderKind::Discogs,
                ProviderKind::MusicBrainz,
                ProviderKind::Deezer
            ]
        );
        assert_eq!(
            listed[0].status_label(),
            "unmatched, queued (discogs, musicbrainz, deezer)"
        );
    }

    #[tokio::test]
    async fn saved_reviews_written_with_ai_advice_still_decode() {
        // Snapshots queued by versions that could ask Clef for advice carry an
        // `ai_advice` object inside the candidate. Decoding must ignore it.
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
        };
        let mut value = serde_json::to_value(&stored).unwrap();
        value["ai_advice"] = serde_json::json!({
            "model": "clef-flash",
            "prompt_version": "radish-clef-album-v3",
            "choice": {"type": "no_match"},
            "probabilities": {"none": 1.0},
            "confidence": 1.0,
        });
        let json = serde_json::to_string(&vec![value]).unwrap();

        let decoded =
            decode_stored_candidates(SNAPSHOT_VERSION, ProviderKind::Discogs, &json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].blockers, vec!["review".to_string()]);
    }

    #[tokio::test]
    async fn library_status_counts_what_is_stored() {
        let pool = test_pool().await;
        assert_eq!(
            library_status(&pool).await.unwrap(),
            LibraryStatus::default()
        );

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
            &[scanned(
                "/music/b/01.flac",
                Some("Green"),
                Some("Artist"),
                1,
            )],
        )
        .await;
        insert(
            &pool,
            "/music/c",
            &[scanned("/music/c/01.flac", Some("Red"), Some("Artist"), 1)],
        )
        .await;
        accept_manually(&pool, "Green", &["Track 1"]).await;

        let blue = load_local_releases(&pool, None)
            .await
            .unwrap()
            .into_iter()
            .find(|release| release.album_title.as_deref() == Some("Blue"))
            .unwrap();
        let candidate = Candidate::new(&blue, remote_release());
        let stored = StoredCandidate {
            release: candidate.release.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: false,
            match_scope: MatchScope::Edition,
            blockers: vec!["review".to_string()],
            warnings: Vec::new(),
            evidence: Vec::new(),
        };
        queue_review(
            &pool,
            blue.id,
            ProviderKind::Discogs,
            &blue.revision,
            None,
            std::slice::from_ref(&stored),
            &candidate.mapping,
            &["uncertain".to_string()],
        )
        .await
        .unwrap();

        let status = library_status(&pool).await.unwrap();
        let directories: Vec<(&str, i64)> = status
            .directories
            .iter()
            .map(|dir| (dir.path.as_str(), dir.tracks))
            .collect();
        assert_eq!(
            directories,
            [("/music/a", 2), ("/music/b", 1), ("/music/c", 1)]
        );
        assert_eq!(status.tracks, 4);
        assert_eq!(status.releases, 3);
        assert_eq!(status.matched, 1);
        assert_eq!(status.unmatched, 2);
        assert_eq!(status.stale, 0);
        assert_eq!(status.queued, 1);
        assert_eq!(status.newest_modified, Some(1));
    }

    #[tokio::test]
    async fn track_pages_do_not_overlap_and_filter_like_search() {
        let pool = test_pool().await;
        let tracks: Vec<ScannedTrack> = (1..=5)
            .map(|n| {
                scanned(
                    &format!("/music/a/{n:02}.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    n,
                )
            })
            .collect();
        insert(&pool, "/music/a", &tracks).await;

        let all: Vec<String> = list_tracks(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.path)
            .collect();
        let first = track_page(&pool, None, 3, 0).await.unwrap();
        let second = track_page(&pool, None, 3, 3).await.unwrap();
        assert_eq!((first.len(), second.len()), (3, 2));
        let paged: Vec<String> = first
            .into_iter()
            .chain(second)
            .map(|row| row.path)
            .collect();
        assert_eq!(paged, all);

        let found = track_page(&pool, Some("track 2"), 10, 0).await.unwrap();
        assert_eq!(found.len(), 1);
        // `%` is a literal here, never a wildcard.
        assert!(
            track_page(&pool, Some("%"), 10, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            search_tracks(&pool, "track 2").await.unwrap().len(),
            found.len()
        );
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

    #[tokio::test]
    async fn version_two_snapshots_read_discogs_master_zero_as_no_group() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let local = load_local_releases(&pool, None).await.unwrap().remove(0);
        let mut remote = remote_release();
        remote.group_id = Some(GroupId::DiscogsMaster(0));

        let json = serde_json::to_string(&remote).unwrap();
        let decoded = decode_release_snapshot(2, ProviderKind::Discogs, &json).unwrap();
        assert_eq!(decoded.group_id, None);

        let candidate = Candidate::new(&local, remote.clone());
        let stored = StoredCandidate {
            release: remote.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: false,
            match_scope: MatchScope::Edition,
            blockers: Vec::new(),
            warnings: Vec::new(),
            evidence: Vec::new(),
        };
        let json = serde_json::to_string(&vec![stored]).unwrap();
        let decoded = decode_stored_candidates(2, ProviderKind::Discogs, &json).unwrap();
        assert_eq!(decoded[0].release.group_id, None);

        remote.group_id = Some(GroupId::DiscogsMaster(7));
        let json = serde_json::to_string(&remote).unwrap();
        let decoded = decode_release_snapshot(2, ProviderKind::Discogs, &json).unwrap();
        assert_eq!(decoded.group_id, Some(GroupId::DiscogsMaster(7)));
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
        remove_tracks_under(&pool, Path::new("/music/a/01.flac"))
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

    const AUGMENTED: &str = "/music/Chemical_brothers-augmented-1999-apc";
    const BROADCAST: &str = "/music/Depeche_Mode-Broadcast_Singles_Tour-2CD-1999-RNS";

    /// 18 + 1 files in real life: two with the right title, one with a typo.
    fn augmented_files() -> Vec<ScannedTrack> {
        vec![
            scanned(
                &format!("{AUGMENTED}/01.mp3"),
                Some("augmented"),
                Some("Chemical Brothers"),
                1,
            ),
            scanned(
                &format!("{AUGMENTED}/02.mp3"),
                Some("augmented"),
                Some("Chemical Brothers"),
                2,
            ),
            scanned(
                &format!("{AUGMENTED}/03.mp3"),
                Some("augmentes"),
                Some("Chemical Brothers"),
                3,
            ),
        ]
    }

    /// Two disc folders whose titles differ only by underscores, every file
    /// tagged disc 1 as per-disc rips are.
    fn broadcast_files() -> Vec<ScannedTrack> {
        let mut files = Vec::new();
        for (disc, album) in [(1, "Broadcast Singles Tour"), (2, "Broadcast_Singles_Tour")] {
            for track in 1..=2 {
                let mut file = scanned(
                    &format!("{BROADCAST}/cd{disc}/0{track}.mp3"),
                    Some(album),
                    Some("Depeche Mode"),
                    track,
                );
                file.disc_number = Some(1);
                file.title = Some(format!("Disc {disc} track {track}"));
                files.push(file);
            }
        }
        files
    }

    /// Two albums in one folder, numbered from 1 each, as in a box set.
    fn box_files() -> Vec<ScannedTrack> {
        vec![
            scanned("/music/box/01.flac", Some("Floodland"), Some("Sisters"), 1),
            scanned("/music/box/02.flac", Some("Floodland"), Some("Sisters"), 2),
            scanned(
                "/music/box/03.flac",
                Some("Vision Thing"),
                Some("Sisters"),
                1,
            ),
            scanned(
                "/music/box/04.flac",
                Some("Vision Thing"),
                Some("Sisters"),
                2,
            ),
        ]
    }

    /// `(directory, key, track count, unit)` of every release, in key order.
    async fn release_shapes(pool: &SqlitePool) -> Vec<(String, String, i64, String)> {
        sqlx::query_as(
            "SELECT directory_path, album_title_norm, track_count, unit_path
             FROM local_releases ORDER BY directory_path, album_title_norm",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_typo_in_one_album_tag_merges_the_folder_automatically() {
        let pool = test_pool().await;
        insert(&pool, "/music", &augmented_files()).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![(AUGMENTED.into(), "*".into(), 3, AUGMENTED.into())]
        );
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        assert_eq!(release.album_title.as_deref(), Some("augmented"));
    }

    #[tokio::test]
    async fn untagged_files_never_merge_automatically() {
        let pool = test_pool().await;
        let mut files = augmented_files();
        files.push(scanned(
            &format!("{AUGMENTED}/04.mp3"),
            None,
            Some("Chemical Brothers"),
            4,
        ));
        insert(&pool, "/music", &files).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![
                (AUGMENTED.into(), "".into(), 1, AUGMENTED.into()),
                (AUGMENTED.into(), "*".into(), 3, AUGMENTED.into()),
            ]
        );
    }

    #[tokio::test]
    async fn two_copies_of_one_file_keep_a_unit_from_merging_automatically() {
        const LIVE: &str = "/music/Rammstein-Live_In_NY_(07-18-01)_Bootleg-2001-PMS";
        let pool = test_pool().await;
        let mut files = Vec::new();
        for track in 1..=2 {
            files.push(scanned(
                &format!("{LIVE}/CD1/0{track}.mp3"),
                Some("Live In NY"),
                Some("Rammstein"),
                track,
            ));
        }
        // CD2 carries no track numbers, and its files are copied into the
        // unit folder itself.
        for dir in ["CD2", ""] {
            for (name, title, secs) in [("a", "encore (live)", 226.0), ("b", "sonne (live)", 305.0)]
            {
                let mut file = scanned(
                    &format!("{LIVE}/{dir}/{name}.mp3").replace("//", "/"),
                    Some("Live In NY"),
                    Some("Rammstein"),
                    1,
                );
                file.track_number = None;
                file.title = Some(title.to_string());
                file.duration_secs = Some(secs);
                files.push(file);
            }
        }
        insert(&pool, "/music", &files).await;
        let shapes = release_shapes(&pool).await;
        assert!(
            shapes.iter().all(|row| row.1 != "*"),
            "no merged release: {shapes:?}"
        );
        assert_eq!(shapes.len(), 3);
    }

    #[tokio::test]
    async fn different_albums_in_one_folder_stay_apart() {
        let pool = test_pool().await;
        insert(&pool, "/music", &box_files()).await;
        let keys: Vec<String> = release_shapes(&pool)
            .await
            .into_iter()
            .map(|row| row.1)
            .collect();
        assert_eq!(keys, vec!["floodland", "vision thing"]);
    }

    #[tokio::test]
    async fn disc_folders_merge_into_the_folder_above() {
        let pool = test_pool().await;
        insert(&pool, "/music", &broadcast_files()).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![(BROADCAST.into(), "*".into(), 4, BROADCAST.into())]
        );
    }

    #[tokio::test]
    async fn disc_folders_with_a_gap_stay_apart_in_one_unit() {
        let pool = test_pool().await;
        let files: Vec<ScannedTrack> = broadcast_files()
            .into_iter()
            .map(|mut file| {
                let path = file.path.to_string_lossy().replace("/cd2/", "/cd3/");
                file.path = PathBuf::from(path);
                file
            })
            .collect();
        insert(&pool, "/music", &files).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![
                (
                    format!("{BROADCAST}/cd1"),
                    "broadcast singles tour".into(),
                    2,
                    BROADCAST.into()
                ),
                (
                    format!("{BROADCAST}/cd3"),
                    "broadcast singles tour".into(),
                    2,
                    BROADCAST.into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_matched_folder_keeps_its_releases_apart() {
        let pool = test_pool().await;
        let files = augmented_files();
        insert(&pool, "/music", &files[..2]).await;
        let id = accept_manually(&pool, "augmented", &["R1", "R2"]).await;

        insert(&pool, "/music", &files).await;
        let keys: Vec<String> = release_shapes(&pool)
            .await
            .into_iter()
            .map(|row| row.1)
            .collect();
        assert_eq!(keys, vec!["augmented", "augmentes"]);
        let (status, stale): (String, bool) = sqlx::query_as(
            "SELECT lr.status, m.stale FROM local_releases lr
             JOIN release_matches m ON m.local_release_id = lr.id WHERE lr.id = ?1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), stale), (STATUS_MATCHED, false));
    }

    #[tokio::test]
    async fn a_matched_merged_folder_stays_merged_and_takes_new_files() {
        let pool = test_pool().await;
        let mut files = augmented_files();
        insert(&pool, "/music", &files).await;
        let id = accept_manually(&pool, "augmented", &["R1", "R2", "R3"]).await;

        files.push(scanned(
            &format!("{AUGMENTED}/04.mp3"),
            Some("Remixes"),
            Some("Chemical Brothers"),
            4,
        ));
        insert(&pool, "/music", &files).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![(AUGMENTED.into(), "*".into(), 4, AUGMENTED.into())]
        );
        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_STALE);
    }

    #[tokio::test]
    async fn a_rebuild_scoped_to_a_disc_folder_regroups_the_whole_unit() {
        let pool = test_pool().await;
        insert(&pool, "/music", &broadcast_files()).await;
        // A new file lands in cd2, and only cd1's tree is regrouped.
        let directory_id = upsert_directory(&pool, Path::new("/music")).await.unwrap();
        let mut new_file = scanned(
            &format!("{BROADCAST}/cd2/03.mp3"),
            Some("Broadcast Singles Tour"),
            Some("Depeche Mode"),
            3,
        );
        new_file.disc_number = Some(1);
        upsert_track(&pool, &new_file, directory_id).await.unwrap();
        rebuild_local_releases(&pool, Some(Path::new(&format!("{BROADCAST}/cd1"))))
            .await
            .unwrap();
        assert_eq!(
            release_shapes(&pool).await,
            vec![(BROADCAST.into(), "*".into(), 5, BROADCAST.into())]
        );
    }

    #[tokio::test]
    async fn an_automatic_merge_leaves_a_junk_tagged_file_out() {
        let pool = test_pool().await;
        let mut files = augmented_files();
        files.push(scanned(
            &format!("{AUGMENTED}/04.mp3"),
            Some("Unknown Album"),
            Some("Chemical Brothers"),
            4,
        ));
        insert(&pool, "/music", &files).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![
                (AUGMENTED.into(), "*".into(), 3, AUGMENTED.into()),
                (
                    AUGMENTED.into(),
                    "unknown album".into(),
                    1,
                    AUGMENTED.into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn an_album_tagged_with_the_reserved_key_does_not_collide_with_it() {
        let pool = test_pool().await;
        let files = vec![
            scanned("/music/star/01.mp3", Some("*"), Some("Artist"), 1),
            scanned("/music/star/02.mp3", Some("*"), Some("Artist"), 2),
        ];
        insert(&pool, "/music", &files).await;
        let shapes = release_shapes(&pool).await;
        assert_eq!(shapes.len(), 1);
        assert_ne!(shapes[0].1, MERGED_KEY);
        assert_eq!(shapes[0].2, 2);
    }

    #[tokio::test]
    async fn matched_disc_folders_keep_their_matches_after_the_migration_backfill() {
        let pool = test_pool().await;
        let releases = insert_disc_folders(&pool).await;
        let id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        for release in &releases {
            let candidate = Candidate::new(release, two_disc_release(id.clone()));
            let guard = load_match_guard(&pool, release.id).await.unwrap();
            accept_match(
                &pool,
                release,
                &candidate,
                AcceptanceMethod::Automatic,
                MatchScope::Album,
                &guard,
            )
            .await
            .unwrap();
        }

        // What a migrated database looks like: no decision, and every release
        // carries its own directory as its unit.
        sqlx::query("DELETE FROM folder_groupings")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE local_releases SET unit_path = directory_path")
            .execute(&pool)
            .await
            .unwrap();
        let summary = rebuild_local_releases(&pool, None).await.unwrap();

        assert_eq!(summary.newly_stale, 0);
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT unit_path, status FROM local_releases ORDER BY directory_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                ("/music/Set-2CD".to_string(), STATUS_MATCHED.to_string()),
                ("/music/Set-2CD".to_string(), STATUS_MATCHED.to_string()),
            ]
        );
        let fresh: Vec<bool> = sqlx::query_scalar("SELECT stale FROM release_matches")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(fresh, vec![false, false]);
    }

    fn planned_group(directory: &str, norm: &str, track_ids: &[i64]) -> GroupedRelease {
        GroupedRelease {
            directory: directory.to_string(),
            unit: directory.to_string(),
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
            has_fresh_match: has_match,
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

    #[tokio::test]
    async fn queued_loading_takes_unmatched_releases_reviewed_by_an_allowed_provider() {
        use crate::test_support::{
            FIXTURE_MBID, mbid_release, queue, remote_release, seed_release,
        };
        let pool = test_pool().await;
        let titles = ["One", "Two"];
        let a = seed_release(&pool, "/music/a", "Blue", &titles).await;
        let b = seed_release(&pool, "/music/b", "Red", &titles).await;
        seed_release(&pool, "/music/c", "Green", &titles).await;
        queue(
            &pool,
            &a,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        queue(
            &pool,
            &b,
            remote_release(ReleaseId::Deezer(7), "Red", &titles),
        )
        .await;

        let ids = |releases: Vec<LocalRelease>| -> Vec<i64> {
            releases.into_iter().map(|release| release.id).collect()
        };
        let chain = [ProviderKind::MusicBrainz, ProviderKind::Discogs];
        let with_deezer = [
            ProviderKind::MusicBrainz,
            ProviderKind::Discogs,
            ProviderKind::Deezer,
        ];
        assert_eq!(
            ids(load_queued_releases(&pool, None, &chain).await.unwrap()),
            vec![a.id]
        );
        assert_eq!(
            ids(load_queued_releases(&pool, None, &with_deezer)
                .await
                .unwrap()),
            vec![a.id, b.id]
        );
        let scoped = load_queued_releases(&pool, Some(Path::new("/music/b")), &with_deezer)
            .await
            .unwrap();
        assert_eq!(scoped[0].tracks.len(), 2, "tracks come with the release");
        assert_eq!(ids(scoped), vec![b.id]);

        // A matched release with a leftover review is not queued.
        sqlx::query("UPDATE local_releases SET status = 'matched' WHERE id = ?1")
            .bind(a.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            load_queued_releases(&pool, None, &chain)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn selected_releases_filter_by_scope_and_id_in_sql() {
        let pool = test_pool().await;
        insert(
            &pool,
            "/music/Blue",
            &[
                scanned("/music/Blue/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/music/Blue/02.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;
        insert(
            &pool,
            "/music/blue/sub",
            &[scanned(
                "/music/blue/sub/01.flac",
                Some("Sub"),
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

        let all = load_selected_releases(&pool, None, None).await.unwrap();
        assert_eq!(all.len(), 3);
        let wrapped = load_local_releases(&pool, None).await.unwrap();
        assert_eq!(
            wrapped.iter().map(|release| release.id).collect::<Vec<_>>(),
            all.iter().map(|release| release.id).collect::<Vec<_>>()
        );

        let scoped = load_selected_releases(&pool, Some(Path::new("/MUSIC/blue")), None)
            .await
            .unwrap();
        let mut titles: Vec<_> = scoped
            .iter()
            .map(|release| release.album_title.clone().unwrap())
            .collect();
        titles.sort();
        assert_eq!(titles, vec!["Blue", "Sub"]);
        assert!(scoped.iter().all(|release| !release.tracks.is_empty()));

        let blue = scoped
            .iter()
            .find(|release| release.album_title.as_deref() == Some("Blue"))
            .unwrap();
        assert_eq!(blue.tracks.len(), 2);

        let by_id = load_selected_releases(&pool, None, Some(blue.id))
            .await
            .unwrap();
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].id, blue.id);
        assert_eq!(by_id[0].tracks.len(), 2);

        let outside =
            load_selected_releases(&pool, Some(Path::new("/music/Bluegrass")), Some(blue.id))
                .await
                .unwrap();
        assert!(outside.is_empty());
        assert!(
            load_selected_releases(&pool, None, Some(9999))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn remove_tracks_under_respects_component_boundaries_and_case() {
        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("/music")).await.unwrap();
        let tracks: Vec<ScannedTrack> = [
            "/music/Blue/01.flac",
            "/music/blue/sub/02.flac",
            "/music/Bluegrass/01.flac",
        ]
        .iter()
        .map(|path| scanned(path, Some("Album"), Some("Artist"), 1))
        .collect();
        upsert_tracks(&pool, &tracks, directory_id).await.unwrap();

        assert!(remove_tracks_under(&pool, Path::new("/")).await.is_err());
        assert!(remove_tracks_under(&pool, Path::new("")).await.is_err());
        assert_eq!(count(&pool, "tracks").await, 3);

        assert_eq!(
            remove_tracks_under(&pool, Path::new("/MUSIC/Blue"))
                .await
                .unwrap(),
            2
        );
        let left: Vec<String> = sqlx::query_scalar("SELECT path FROM tracks")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["/music/Bluegrass/01.flac"]);

        assert_eq!(
            remove_tracks_under(&pool, Path::new("/MUSIC/bluegrass/01.FLAC"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(count(&pool, "tracks").await, 0);
    }

    #[tokio::test]
    async fn claiming_a_parent_takes_over_the_roots_inside_it() {
        let pool = test_pool().await;
        let year = |root: &str, count: i64| {
            (1..=count)
                .map(|n| scanned(&format!("{root}/a/{n:02}.mp3"), Some("A"), Some("X"), n))
                .collect::<Vec<_>>()
        };
        insert(&pool, "/m/1998", &year("/m/1998", 1)).await;
        insert(&pool, "/m/1999", &year("/m/1999", 2)).await;
        // A sibling that only shares a prefix, not a path component.
        insert(&pool, "/mbox", &year("/mbox", 1)).await;
        let releases = release_rows(&pool).await;

        let claim = claim_root(&pool, Path::new("/m")).await.unwrap();
        assert_eq!(claim.absorbed, ["/m/1998", "/m/1999"]);
        assert_eq!(
            roots(&pool).await,
            [("/m".to_string(), 3), ("/mbox".to_string(), 1)]
        );
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT DISTINCT directory_id FROM tracks WHERE path LIKE '/m/%'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids, [claim.id]);
        assert_eq!(release_rows(&pool).await, releases);

        let again = claim_root(&pool, Path::new("/m")).await.unwrap();
        assert_eq!(again.id, claim.id);
        assert!(again.absorbed.is_empty());
    }

    #[tokio::test]
    async fn a_folder_inside_a_root_counts_toward_that_root() {
        let pool = test_pool().await;
        let root = claim_root(&pool, Path::new("/m")).await.unwrap();
        let child = claim_root(&pool, Path::new("/M/2026/01")).await.unwrap();
        assert_eq!(child.id, root.id);
        assert!(child.absorbed.is_empty());
        assert_eq!(roots(&pool).await, [("/m".to_string(), 0)]);
    }

    #[tokio::test]
    async fn nested_roots_from_older_versions_join_the_outermost() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO directories (id, path) VALUES (1, '/m'), (2, '/m/1998')")
            .execute(&pool)
            .await
            .unwrap();
        let claim = claim_root(&pool, Path::new("/m/1998")).await.unwrap();
        assert_eq!(claim.id, 1);
        assert_eq!(claim.absorbed, ["/m/1998"]);
        assert_eq!(roots(&pool).await, [("/m".to_string(), 0)]);
    }

    /// The imported roots as `(path, tracks)`, in path order.
    async fn roots(pool: &SqlitePool) -> Vec<(String, i64)> {
        library_status(pool)
            .await
            .unwrap()
            .directories
            .into_iter()
            .map(|root| (root.path, root.tracks))
            .collect()
    }

    /// Every local release's ID, key, revision and status.
    async fn release_rows(pool: &SqlitePool) -> Vec<(i64, String, String, String, String)> {
        sqlx::query_as(
            "SELECT id, directory_path, album_title_norm, source_revision, status
             FROM local_releases ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn known_tracks_lists_files_under_a_root() {
        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("/music")).await.unwrap();
        let mut first = scanned("/music/Blue/01.flac", Some("Album"), Some("Artist"), 1);
        first.modified_at = 11;
        let mut nested = scanned("/music/blue/Sub/02.flac", Some("Album"), Some("Artist"), 2);
        nested.modified_at = 22;
        let sibling = scanned("/music/Bluegrass/01.flac", Some("Album"), Some("Artist"), 1);
        upsert_tracks(&pool, &[first, nested, sibling], directory_id)
            .await
            .unwrap();

        let known = known_tracks(&pool, Path::new("/MUSIC/blue")).await.unwrap();
        assert_eq!(known.len(), 2);
        assert_eq!(known["/music/blue/01.flac"].modified_at, 11);
        assert_eq!(known["/music/blue/01.flac"].path, "/music/Blue/01.flac");
        assert_eq!(known["/music/blue/sub/02.flac"].modified_at, 22);
        assert_eq!(
            known["/music/blue/sub/02.flac"].path,
            "/music/blue/Sub/02.flac"
        );
        assert_eq!(
            path_key(Path::new(r"C:\Music\Blue")),
            "c:/music/blue".to_string()
        );
    }

    #[tokio::test]
    async fn upserting_a_case_changed_path_updates_the_stored_casing() {
        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("/music")).await.unwrap();
        let original = scanned("/music/A/01.wav", Some("Album"), None, 1);
        let renamed = scanned("/music/A/01.WAV", Some("Album"), None, 1);
        upsert_tracks(&pool, &[original], directory_id)
            .await
            .unwrap();

        let changed = upsert_tracks(&pool, std::slice::from_ref(&renamed), directory_id)
            .await
            .unwrap();

        assert_eq!(changed, 1);
        assert_eq!(count(&pool, "tracks").await, 1);
        let stored: String = sqlx::query_scalar("SELECT path FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, "/music/A/01.WAV");
        let again = upsert_tracks(&pool, &[renamed], directory_id)
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn remove_tracks_by_paths_deletes_stored_paths_in_bulk() {
        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("/music")).await.unwrap();
        let tracks: Vec<_> = (0..1200)
            .map(|n| scanned(&format!("/music/A/{n:04}.flac"), Some("Album"), None, 1))
            .collect();
        let keeper = scanned("/music/B/keep.flac", Some("Other"), None, 1);
        upsert_tracks(&pool, &tracks, directory_id).await.unwrap();
        upsert_tracks(&pool, &[keeper], directory_id).await.unwrap();

        // Stored casing is not required: the path column compares NOCASE.
        let doomed: Vec<String> = (0..1200).map(|n| format!("/MUSIC/a/{n:04}.FLAC")).collect();
        let removed = remove_tracks_by_paths(&pool, &doomed).await.unwrap();

        assert_eq!(removed, 1200);
        assert_eq!(count(&pool, "tracks").await, 1);
        assert_eq!(remove_tracks_by_paths(&pool, &[]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn overlay_rows_of_foreign_matches_are_ignored_and_replaced_on_accept() {
        let pool = test_pool().await;
        let mut blue = scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1);
        blue.title = Some("Blue One".into());
        let mut red = scanned("/music/b/01.flac", Some("Red"), Some("Artist"), 1);
        red.title = Some("Red One".into());
        insert(&pool, "/music/a", &[blue]).await;
        insert(&pool, "/music/b", &[red]).await;
        let blue_id = accept_manually(&pool, "Blue", &["Blue Remote"]).await;

        // A leftover overlay row: Red's file is tied to Blue's match.
        sqlx::query(
            "INSERT INTO release_track_matches (match_id, track_id, remote_ordinal, remote_title)
             SELECT m.id, t.id, 9, 'Orphan'
             FROM release_matches m, tracks t
             WHERE m.local_release_id = ?1 AND t.path = '/music/b/01.flac'",
        )
        .bind(blue_id)
        .execute(&pool)
        .await
        .unwrap();

        let listed = list_tracks(&pool).await.unwrap();
        let red_row = listed
            .iter()
            .find(|r| r.path.ends_with("b/01.flac"))
            .unwrap();
        assert_eq!(red_row.title.as_deref(), Some("Red One"));
        let found = search_tracks(&pool, "Orphan").await.unwrap();
        assert!(
            found.is_empty(),
            "the orphan overlay must not be searchable"
        );
        let found = search_tracks(&pool, "Red One").await.unwrap();
        assert_eq!(found.len(), 1);

        // Accepting the track's own release replaces the leftover row.
        accept_manually(&pool, "Red", &["Red Remote"]).await;
        let listed = list_tracks(&pool).await.unwrap();
        let red_row = listed
            .iter()
            .find(|r| r.path.ends_with("b/01.flac"))
            .unwrap();
        assert_eq!(red_row.title.as_deref(), Some("Red Remote"));
    }

    #[tokio::test]
    async fn readded_file_with_unchanged_tags_marks_the_match_stale() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1),
            scanned("/music/a/02.flac", Some("Blue"), Some("Artist"), 2),
        ];
        insert(&pool, "/music/a", &files).await;
        let id = accept_manually(&pool, "Blue", &["R1", "R2"]).await;

        // Same tags and path, new track ID: its overlay row went with the old one.
        remove_tracks_under(&pool, Path::new("/music/a/02.flac"))
            .await
            .unwrap();
        let summary = insert_summary(&pool, "/music/a", &files).await;

        assert_eq!(summary.newly_stale, 1, "{summary:?}");
        let status: String = sqlx::query_scalar("SELECT status FROM local_releases WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, STATUS_STALE);
        let rows = list_tracks(&pool).await.unwrap();
        assert!(rows.iter().all(|row| row.stale));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_rebuild_agrees_with_directories_when_file_names_contain_backslashes() {
        let pool = test_pool().await;
        // On Unix the backslash is part of the file name, so this file lives in
        // `/m`, not in `/m/x`, even though a slash-normalized path would say so.
        insert(
            &pool,
            "/m",
            &[
                scanned("/m/01.flac", Some("Blue"), Some("Artist"), 1),
                scanned("/m/x\\y.flac", Some("Blue"), Some("Artist"), 2),
            ],
        )
        .await;

        let summary = rebuild_local_releases(&pool, Some(Path::new("/m/x")))
            .await
            .unwrap();
        assert_eq!(summary.groups, 0);
        assert!(!summary.changed());
        assert_eq!(release_ids(&pool).await.len(), 1);
    }

    #[tokio::test]
    async fn remove_tracks_under_refuses_volume_roots() {
        for root in ["/", ""] {
            assert_eq!(scope_param(Path::new(root)), None, "{root:?}");
        }
        for root in [
            r"C:\",
            "c:",
            "//server/share",
            r"\\server\share\",
            "//server",
        ] {
            let normalized = scope_param(Path::new(root)).unwrap();
            assert!(is_volume_root(&normalized), "{root:?}");
        }
        for root in ["C:/Music", "//server/share/music", "/music", "/m"] {
            assert!(!is_volume_root(root), "{root:?}");
        }

        let pool = test_pool().await;
        let directory_id = upsert_directory(&pool, Path::new("C:/")).await.unwrap();
        upsert_track(
            &pool,
            &scanned("C:/Music/01.flac", Some("Blue"), Some("Artist"), 1),
            directory_id,
        )
        .await
        .unwrap();
        for root in [r"C:\", "c:", r"\\server\share"] {
            assert!(
                remove_tracks_under(&pool, Path::new(root)).await.is_err(),
                "{root:?}"
            );
        }
        assert_eq!(count(&pool, "tracks").await, 1);
        assert_eq!(
            remove_tracks_under(&pool, Path::new(r"C:\Music"))
                .await
                .unwrap(),
            1
        );
    }

    /// A release of two media with two tracks each, `Song m-k`. A MusicBrainz
    /// release carries explicit medium and track positions; a Discogs one only the
    /// position strings `m-k`.
    fn two_disc_release(id: ReleaseId) -> RemoteRelease {
        let explicit = matches!(id, ReleaseId::MusicBrainz(_));
        let mut release = if explicit {
            musicbrainz_release()
        } else {
            remote_release()
        };
        release.id = id;
        release.tracklist = (1..=2u32)
            .flat_map(|medium| (1..=2u32).map(move |position| (medium, position)))
            .map(|(medium, position)| RemoteTrack {
                position: Some(format!("{medium}-{position}")),
                title: format!("Song {medium}-{position}"),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: explicit.then_some(medium),
                track_position: explicit.then_some(position),
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            })
            .collect();
        release
    }

    /// Two disc folders, both tagged `Blue`, kept apart by a `separate`
    /// decision: they would merge automatically, and these tests are about
    /// each folder matching its own disc.
    async fn insert_disc_folders(pool: &SqlitePool) -> Vec<LocalRelease> {
        sqlx::query(
            "INSERT INTO folder_groupings (directory_path, mode) VALUES ('/music/Set-2CD', 'separate')",
        )
        .execute(pool)
        .await
        .unwrap();
        for disc in 1..=2 {
            let root = format!("/music/Set-2CD/CD{disc}");
            insert(
                pool,
                &root,
                &[
                    scanned(&format!("{root}/01.flac"), Some("Blue"), Some("Artist"), 1),
                    scanned(&format!("{root}/02.flac"), Some("Blue"), Some("Artist"), 2),
                ],
            )
            .await;
        }
        let mut releases = load_local_releases(pool, None).await.unwrap();
        releases.sort_by(|a, b| a.directory.cmp(&b.directory));
        releases
    }

    #[tokio::test]
    async fn accepting_a_disc_records_its_medium_and_disc_numbers() {
        let pool = test_pool().await;
        let releases = insert_disc_folders(&pool).await;
        let disc_two = &releases[1];
        let mbid = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        let candidate = Candidate::new(disc_two, two_disc_release(mbid));
        assert_eq!(candidate.mapping.medium, Some(2));
        assert!(candidate.mapping.complete, "{:?}", candidate.mapping.notes);

        // A pair that lies outside the disc is refused.
        let mut outside = candidate.clone();
        outside.mapping.pairs[0] = Some(0);
        let guard = load_match_guard(&pool, disc_two.id).await.unwrap();
        let error = accept_match(
            &pool,
            disc_two,
            &outside,
            AcceptanceMethod::Automatic,
            MatchScope::Album,
            &guard,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outside disc 2"), "{error:#}");

        accept_match(
            &pool,
            disc_two,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Album,
            &guard,
        )
        .await
        .unwrap();

        let medium: Option<i64> = sqlx::query_scalar("SELECT medium_position FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(medium, Some(2));
        let rows: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT remote_ordinal, medium_position, track_position FROM release_track_matches
             ORDER BY remote_ordinal",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        // The ordinal stays the place in the whole release; the album scope keeps
        // edition track positions absent but the folder said which disc this is.
        assert_eq!(rows, vec![(3, Some(2), None), (4, Some(2), None)]);

        let shown: Vec<(String, Option<i64>, Option<i64>)> = list_tracks(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.display_title(), row.disc_number, row.track_number))
            .collect();
        assert!(
            shown.contains(&("Song 2-1".to_string(), Some(2), Some(1))),
            "{shown:?}"
        );
        assert!(
            shown.contains(&("Song 2-2".to_string(), Some(2), Some(2))),
            "{shown:?}"
        );
        let status = list_local_releases(&pool, None).await.unwrap();
        let status = status.iter().find(|row| row.id == disc_two.id).unwrap();
        assert_eq!(status.medium, Some(2));

        // At edition scope the disc's own positions are shown, for Discogs too.
        let guard = load_match_guard(&pool, disc_two.id).await.unwrap();
        let discogs = Candidate::new(disc_two, two_disc_release(ReleaseId::Discogs(42)));
        assert_eq!(discogs.mapping.medium, Some(2));
        accept_match(
            &pool,
            disc_two,
            &discogs,
            AcceptanceMethod::Manual,
            MatchScope::Edition,
            &guard,
        )
        .await
        .unwrap();
        let rows: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT remote_ordinal, medium_position, track_position FROM release_track_matches
             ORDER BY remote_ordinal",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![(3, Some(2), Some(1)), (4, Some(2), Some(2))]);
        let shown: Vec<(String, Option<i64>, Option<i64>)> = list_tracks(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.display_title(), row.disc_number, row.track_number))
            .collect();
        assert!(
            shown.contains(&("Song 2-1".to_string(), Some(2), Some(1))),
            "{shown:?}"
        );
    }

    #[tokio::test]
    async fn a_whole_release_match_has_no_medium() {
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
        let medium: Option<i64> = sqlx::query_scalar("SELECT medium_position FROM release_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(medium, None);
        let status = list_local_releases(&pool, None).await.unwrap();
        assert_eq!(status[0].medium, None);
    }

    #[tokio::test]
    async fn sibling_discs_can_accept_the_same_remote_release() {
        let pool = test_pool().await;
        let releases = insert_disc_folders(&pool).await;
        let id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        for release in &releases {
            let candidate = Candidate::new(release, two_disc_release(id.clone()));
            let guard = load_match_guard(&pool, release.id).await.unwrap();
            accept_match(
                &pool,
                release,
                &candidate,
                AcceptanceMethod::Automatic,
                MatchScope::Album,
                &guard,
            )
            .await
            .unwrap();
        }
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT remote_release_id, medium_position FROM release_matches
             ORDER BY medium_position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let remote = id.storage_id();
        assert_eq!(
            rows,
            vec![(remote.clone(), Some(1)), (remote.clone(), Some(2))]
        );
        let tracks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tracks, 4);

        // Clearing one disc leaves the other matched.
        assert!(clear_match(&pool, releases[0].id).await.unwrap());
        let rows: Vec<(i64, Option<i64>)> =
            sqlx::query_as("SELECT local_release_id, medium_position FROM release_matches")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![(releases[1].id, Some(2))]);
        let tracks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tracks, 2);
        let statuses = status_by_id(&pool).await.unwrap();
        assert_eq!(statuses[&releases[0].id], STATUS_UNMATCHED);
        assert_eq!(statuses[&releases[1].id], STATUS_MATCHED);
    }

    async fn matches(pool: &SqlitePool) -> Vec<(i64, bool)> {
        sqlx::query_as(
            "SELECT local_release_id, stale FROM release_matches ORDER BY local_release_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn merging_a_folder_keeps_one_match_stale_and_drops_the_others() {
        let pool = test_pool().await;
        insert(&pool, "/music", &box_files()).await;
        let floodland = accept_manually(&pool, "Floodland", &["R1", "R2"]).await;
        accept_manually(&pool, "Vision Thing", &["R3", "R4"]).await;

        let outcome = set_folder_mode(&pool, "/music/box", Some(FolderMode::Merge))
            .await
            .unwrap();
        assert_eq!(outcome.unit, "/music/box");
        assert_eq!(outcome.releases, vec![(floodland, 4)]);
        assert_eq!(outcome.summary.newly_stale, 1);
        assert_eq!(outcome.summary.dropped_matches, 1);
        assert_eq!(matches(&pool).await, vec![(floodland, true)]);
        let mode: String = sqlx::query_scalar("SELECT mode FROM folder_groupings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode, "merge");
    }

    #[tokio::test]
    async fn splitting_a_merged_folder_drops_its_match() {
        let pool = test_pool().await;
        insert(&pool, "/music", &augmented_files()).await;
        accept_manually(&pool, "augmented", &["R1", "R2", "R3"]).await;

        let outcome = set_folder_mode(&pool, AUGMENTED, Some(FolderMode::Separate))
            .await
            .unwrap();
        assert_eq!(outcome.summary.dropped_matches, 1);
        assert_eq!(outcome.releases.len(), 2);
        assert!(matches(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn letting_the_rule_decide_again_removes_the_decision() {
        let pool = test_pool().await;
        insert(&pool, "/music", &augmented_files()).await;
        set_folder_mode(&pool, AUGMENTED, Some(FolderMode::Separate))
            .await
            .unwrap();
        assert_eq!(release_shapes(&pool).await.len(), 2);

        let outcome = set_folder_mode(&pool, AUGMENTED, None).await.unwrap();
        assert_eq!(outcome.releases.len(), 1);
        let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folder_groupings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
    }

    #[tokio::test]
    async fn a_merge_decision_survives_retagging() {
        let pool = test_pool().await;
        let files = box_files();
        insert(&pool, "/music", &files).await;
        set_folder_mode(&pool, "/music/box", Some(FolderMode::Merge))
            .await
            .unwrap();

        let mut retagged = files.clone();
        retagged[3] = retitled(&files[3], "Something Else");
        insert(&pool, "/music", &retagged).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![("/music/box".into(), "*".into(), 4, "/music/box".into())]
        );
    }

    #[tokio::test]
    async fn a_second_decision_in_another_case_updates_the_stored_mode() {
        let pool = test_pool().await;
        insert(&pool, "/music", &augmented_files()).await;
        set_folder_mode(&pool, AUGMENTED, Some(FolderMode::Separate))
            .await
            .unwrap();
        let outcome = set_folder_mode(
            &pool,
            &format!("{}/", AUGMENTED.to_ascii_uppercase()),
            Some(FolderMode::Merge),
        )
        .await
        .unwrap();
        assert_eq!(outcome.releases.len(), 1);
        let decisions: Vec<(String, String)> =
            sqlx::query_as("SELECT directory_path, mode FROM folder_groupings")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].1, "merge");
    }

    /// Two disc folders of one album, in the state of a database copied from
    /// before the folder rules ran: no decision, and every `unit_path` still
    /// the release's own folder.
    async fn copied_disc_folders(pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO folder_groupings (directory_path, mode) VALUES ('/music/Set-2CD', 'separate')",
        )
        .execute(pool)
        .await
        .unwrap();
        for disc in 1..=2 {
            let root = format!("/music/Set-2CD/CD{disc}");
            insert(
                pool,
                &root,
                &[scanned(
                    &format!("{root}/01.flac"),
                    Some("Blue"),
                    Some("Artist"),
                    1,
                )],
            )
            .await;
        }
        sqlx::query("DELETE FROM folder_groupings")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE local_releases SET unit_path = directory_path")
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn disc_folders_resolve_by_structure_before_they_are_regrouped() {
        let pool = test_pool().await;
        copied_disc_folders(&pool).await;
        for path in [
            "/music/Set-2CD",
            "/music/Set-2CD/CD1",
            "/MUSIC/set-2cd/cd2/",
        ] {
            let found = resolve_folder(&pool, &FolderTarget::Path(path.into()))
                .await
                .unwrap();
            assert_eq!(found.as_deref(), Some("/music/Set-2CD"), "{path}");
        }
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM local_releases WHERE directory_path = '/music/Set-2CD/CD1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let found = resolve_folder(&pool, &FolderTarget::Release(id))
            .await
            .unwrap();
        assert_eq!(found.as_deref(), Some("/music/Set-2CD"));
    }

    #[tokio::test]
    async fn a_decision_on_an_ungrouped_disc_folder_lands_on_its_unit() {
        let pool = test_pool().await;
        copied_disc_folders(&pool).await;

        let outcome = set_folder_mode(&pool, "/music/Set-2CD/CD1", Some(FolderMode::Merge))
            .await
            .unwrap();
        assert_eq!(outcome.unit, "/music/Set-2CD");
        assert_eq!(outcome.releases.len(), 1);
        assert_eq!(
            release_shapes(&pool).await,
            vec![(
                "/music/Set-2CD".into(),
                "*".into(),
                2,
                "/music/Set-2CD".into()
            )]
        );
        let decisions: Vec<(String, String)> =
            sqlx::query_as("SELECT directory_path, mode FROM folder_groupings")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            decisions,
            vec![("/music/Set-2CD".to_string(), "merge".to_string())]
        );
    }

    #[tokio::test]
    async fn a_decision_on_no_folder_is_an_error_and_writes_nothing() {
        let pool = test_pool().await;
        insert(&pool, "/music", &box_files()).await;
        let error = set_folder_mode(&pool, "/music/nowhere", Some(FolderMode::Merge))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("/music/nowhere"), "{error:#}");
        let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folder_groupings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
    }

    #[tokio::test]
    async fn an_untagged_file_joins_under_a_manual_merge() {
        let pool = test_pool().await;
        let mut files = augmented_files();
        files.push(scanned(
            &format!("{AUGMENTED}/04.mp3"),
            None,
            Some("Chemical Brothers"),
            4,
        ));
        insert(&pool, "/music", &files).await;
        assert_eq!(release_shapes(&pool).await.len(), 2);

        let outcome = set_folder_mode(&pool, AUGMENTED, Some(FolderMode::Merge))
            .await
            .unwrap();
        assert_eq!(outcome.releases.len(), 1);
        assert_eq!(
            release_shapes(&pool).await,
            vec![(AUGMENTED.into(), "*".into(), 4, AUGMENTED.into())]
        );
    }

    #[tokio::test]
    async fn a_single_release_folder_with_a_merge_decision_uses_the_reserved_key() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/solo/01.flac", Some("Solo"), Some("Artist"), 1),
            scanned("/music/solo/02.flac", Some("Solo"), Some("Artist"), 2),
        ];
        insert(&pool, "/music", &files).await;
        assert_eq!(
            release_shapes(&pool).await,
            vec![("/music/solo".into(), "solo".into(), 2, "/music/solo".into())]
        );

        set_folder_mode(&pool, "/music/solo", Some(FolderMode::Merge))
            .await
            .unwrap();
        assert_eq!(
            release_shapes(&pool).await,
            vec![("/music/solo".into(), "*".into(), 2, "/music/solo".into())]
        );
    }

    #[tokio::test]
    async fn folder_targets_resolve_to_their_unit() {
        let pool = test_pool().await;
        let mut files = box_files();
        files.extend(broadcast_files());
        // A second disc unit with a gap, which stays apart.
        files.push(scanned("/music/gap/cd1/01.mp3", Some("Gap"), Some("A"), 1));
        files.push(scanned("/music/gap/cd3/01.mp3", Some("Gap"), Some("A"), 1));
        insert(&pool, "/music", &files).await;
        let box_id: i64 = sqlx::query_scalar(
            "SELECT id FROM local_releases WHERE directory_path = '/music/box' LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let resolve = |target: FolderTarget| {
            let pool = pool.clone();
            async move { resolve_folder(&pool, &target).await.unwrap() }
        };
        assert_eq!(
            resolve(FolderTarget::Release(box_id)).await.as_deref(),
            Some("/music/box")
        );
        assert_eq!(
            resolve(FolderTarget::Path("/MUSIC/box/".into()))
                .await
                .as_deref(),
            Some("/music/box")
        );
        assert_eq!(
            resolve(FolderTarget::Path(format!("{BROADCAST}/cd2")))
                .await
                .as_deref(),
            Some(BROADCAST)
        );
        assert_eq!(
            resolve(FolderTarget::Path(BROADCAST.into()))
                .await
                .as_deref(),
            Some(BROADCAST)
        );
        assert_eq!(
            resolve(FolderTarget::Path("/music/gap/cd3".into()))
                .await
                .as_deref(),
            Some("/music/gap")
        );
        assert_eq!(
            resolve(FolderTarget::Path("/music/nowhere".into())).await,
            None
        );
        assert_eq!(resolve(FolderTarget::Release(999_999)).await, None);
        // A disc folder that holds none of the merged release's files names nothing.
        assert_eq!(
            resolve(FolderTarget::Path(format!("{BROADCAST}/cd9"))).await,
            None
        );
    }

    /// Two discs of two tracks each, titled to match `broadcast_files`.
    fn two_disc_remote() -> RemoteRelease {
        let mut remote = remote_with(&["Track 1", "Track 2", "Track 1", "Track 2"]);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(index as u32 / 2 + 1);
            track.track_position = Some(index as u32 % 2 + 1);
            track.position = Some(format!("{}-{}", index / 2 + 1, index % 2 + 1));
        }
        remote
    }

    #[tokio::test]
    async fn a_merged_release_keeps_its_discs_in_the_overlay_at_album_scope() {
        let pool = test_pool().await;
        insert(&pool, "/music", &broadcast_files()).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        let candidate = Candidate::new(&release, two_disc_remote());
        let guard = load_match_guard(&pool, release.id).await.unwrap();
        accept_match(
            &pool,
            &release,
            &candidate,
            AcceptanceMethod::Automatic,
            MatchScope::Album,
            &guard,
        )
        .await
        .unwrap();
        let media: Vec<Option<i64>> = sqlx::query_scalar(
            "SELECT tm.medium_position FROM release_track_matches tm
             JOIN tracks t ON t.id = tm.track_id ORDER BY t.path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(media, vec![Some(1), Some(1), Some(2), Some(2)]);
    }

    #[tokio::test]
    async fn a_merged_release_numbers_discs_from_its_subfolders() {
        let pool = test_pool().await;
        insert(&pool, "/music", &broadcast_files()).await;
        let release = load_local_releases(&pool, None).await.unwrap().remove(0);
        assert_eq!(release.directory, BROADCAST);
        assert_eq!(
            release.album_title.as_deref(),
            Some("Broadcast Singles Tour")
        );
        let order: Vec<(Option<i64>, String)> = release
            .tracks
            .iter()
            .map(|track| (track.disc, track.path.replace(BROADCAST, "")))
            .collect();
        assert_eq!(
            order,
            vec![
                (Some(1), "/cd1/01.mp3".to_string()),
                (Some(1), "/cd1/02.mp3".to_string()),
                (Some(2), "/cd2/01.mp3".to_string()),
                (Some(2), "/cd2/02.mp3".to_string()),
            ]
        );
        assert!(matcher::local_disc_hint(&release).is_none());

        // A two-disc release maps onto the files as a whole.
        let candidate = Candidate::new(&release, two_disc_remote());
        assert_eq!(candidate.mapping.medium, None);
        let pairs: Vec<(usize, usize)> = candidate.mapping.assigned_pairs().collect();
        assert_eq!(pairs, vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[tokio::test]
    async fn a_disc_subfolder_scope_finds_its_merged_release() {
        let pool = test_pool().await;
        let mut files = broadcast_files();
        // An untagged file stays its own release, and holds no cd1 file.
        files.push(scanned(
            &format!("{BROADCAST}/extra.mp3"),
            None,
            Some("Depeche Mode"),
            9,
        ));
        insert(&pool, "/music", &files).await;
        assert_eq!(release_ids(&pool).await.len(), 2);
        let cd1 = format!("{BROADCAST}/cd1");
        let loaded = load_selected_releases(&pool, Some(Path::new(&cd1)), None)
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].album_title.as_deref(),
            Some("Broadcast Singles Tour")
        );
        assert_eq!(loaded[0].tracks.len(), 4);
        let listed = list_local_releases(&pool, Some(Path::new(&cd1)))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, loaded[0].id);
        assert_eq!(listed[0].track_count, 4);
    }

    #[tokio::test]
    async fn folder_list_shows_units_with_several_releases_a_merge_or_a_decision() {
        let pool = test_pool().await;
        let mut files = box_files();
        files.extend(augmented_files());
        files.push(scanned(
            "/music/single/01.flac",
            Some("Alone"),
            Some("A"),
            1,
        ));
        insert(&pool, "/music", &files).await;
        set_folder_mode(&pool, "/music/single", Some(FolderMode::Separate))
            .await
            .unwrap();

        let units = list_folder_units(&pool, None).await.unwrap();
        let summary: Vec<(String, FolderState, usize)> = units
            .iter()
            .map(|unit| (unit.path.clone(), unit.state, unit.releases.len()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("/music/box".into(), FolderState::Automatic, 2),
                (AUGMENTED.into(), FolderState::MergedAutomatically, 1),
                ("/music/single".into(), FolderState::SeparateByUser, 1),
            ]
        );
        let merged = units[1].releases[0].merged.clone().unwrap();
        assert_eq!(
            merged,
            MergeInfo {
                titles: 2,
                folders: 1,
                manual: false
            }
        );
        assert_eq!(merged.label(), "2 album titles, 1 folder (automatic)");
        assert!(units[0].releases[0].merged.is_none());
        assert_eq!(library_status(&pool).await.unwrap().folders, 3);
    }

    #[tokio::test]
    async fn a_folder_view_previews_what_each_choice_does_to_matches() {
        let pool = test_pool().await;
        insert(&pool, "/music", &box_files()).await;
        accept_manually(&pool, "Floodland", &["R1", "R2"]).await;

        let view = folder_view(&pool, "/music/box/").await.unwrap().unwrap();
        assert_eq!(view.unit.path, "/music/box");
        assert_eq!(view.unit.state, FolderState::Automatic);
        assert_eq!(view.unit.releases.len(), 2);
        assert_eq!(
            view.choices,
            vec![
                (
                    FolderAction::Merge,
                    MatchEffects {
                        stale: 1,
                        dropped: 0
                    }
                ),
                (FolderAction::Separate, MatchEffects::default()),
            ]
        );
        let files: Vec<&str> = view.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            files,
            [
                "/music/box/01.flac",
                "/music/box/02.flac",
                "/music/box/03.flac",
                "/music/box/04.flac"
            ]
        );
        assert_eq!(
            MatchEffects {
                stale: 1,
                dropped: 2
            }
            .describe()
            .as_deref(),
            Some("1 accepted match becomes stale, 2 accepted matches are dropped")
        );
        assert_eq!(MatchEffects::default().describe(), None);
        // The preview wrote nothing.
        assert_eq!(release_shapes(&pool).await.len(), 2);
        let stale: Vec<bool> = sqlx::query_scalar("SELECT stale FROM release_matches")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(stale, [false]);
        let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folder_groupings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
        assert!(
            folder_view(&pool, "/music/nowhere")
                .await
                .unwrap()
                .is_none()
        );

        // Any path that names the unit shows it: a disc folder shows its parent.
        let pool = test_pool().await;
        insert(&pool, "/music", &broadcast_files()).await;
        let view = folder_view(&pool, &format!("{BROADCAST}/cd1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.unit.path, BROADCAST);
        assert_eq!(view.unit.state, FolderState::MergedAutomatically);
        assert_eq!(view.files.len(), 4);
        let discs: Vec<Option<i64>> = view.files.iter().map(|file| file.disc).collect();
        assert_eq!(discs, [Some(1), Some(1), Some(2), Some(2)]);
        assert!(
            folder_view(&pool, &format!("{BROADCAST}/cd3"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_decision_already_made_is_not_offered_again() {
        let pool = test_pool().await;
        insert(&pool, "/music", &box_files()).await;
        set_folder_mode(&pool, "/music/box", Some(FolderMode::Separate))
            .await
            .unwrap();
        let view = folder_view(&pool, "/music/box").await.unwrap().unwrap();
        assert_eq!(view.unit.state, FolderState::SeparateByUser);
        let actions: Vec<FolderAction> = view.choices.iter().map(|choice| choice.0).collect();
        assert_eq!(actions, [FolderAction::Merge, FolderAction::Auto]);
    }

    /// Builds the database of one preview scenario, returning it with the path
    /// of its unit. Each call builds a fresh database, so a scenario can be
    /// previewed on one and applied to another.
    async fn preview_scenario(which: char) -> (SqlitePool, &'static str) {
        let pool = test_pool().await;
        match which {
            // The box with a fresh match on one album.
            'a' | 'b' => {
                insert(&pool, "/music", &box_files()).await;
                accept_manually(&pool, "Floodland", &["R1", "R2"]).await;
                if which == 'b' {
                    sqlx::query("UPDATE release_matches SET stale = 1")
                        .execute(&pool)
                        .await
                        .unwrap();
                }
                (pool, "/music/box")
            }
            // An automatically merged unit with a match and an untagged file.
            'c' => {
                let mut files = augmented_files();
                files.push(scanned(
                    &format!("{AUGMENTED}/04.mp3"),
                    None,
                    Some("Chemical Brothers"),
                    4,
                ));
                insert(&pool, "/music", &files).await;
                accept_manually(&pool, "augmented", &["R1", "R2", "R3"]).await;
                (pool, AUGMENTED)
            }
            // A unit the user already merged.
            'd' => {
                insert(&pool, "/music", &box_files()).await;
                set_folder_mode(&pool, "/music/box", Some(FolderMode::Merge))
                    .await
                    .unwrap();
                let merged = load_local_releases(&pool, None).await.unwrap().remove(0);
                let title = merged.album_title.unwrap();
                accept_manually(&pool, &title, &["R1", "R2", "R3", "R4"]).await;
                (pool, "/music/box")
            }
            // Both albums matched.
            'e' => {
                insert(&pool, "/music", &box_files()).await;
                accept_manually(&pool, "Floodland", &["R1", "R2"]).await;
                accept_manually(&pool, "Vision Thing", &["R1", "R2"]).await;
                (pool, "/music/box")
            }
            _ => unreachable!("no scenario {which}"),
        }
    }

    #[tokio::test]
    async fn the_preview_agrees_with_what_each_choice_does() {
        for which in ['a', 'b', 'c', 'd', 'e'] {
            let (pool, unit) = preview_scenario(which).await;
            let view = folder_view(&pool, unit).await.unwrap().unwrap();
            assert!(!view.choices.is_empty(), "scenario {which}");
            for (action, effects) in &view.choices {
                let (fresh, unit) = preview_scenario(which).await;
                let outcome = set_folder_mode(&fresh, unit, action.mode()).await.unwrap();
                assert_eq!(
                    (effects.stale, effects.dropped),
                    (outcome.summary.newly_stale, outcome.summary.dropped_matches),
                    "scenario {which}, {action:?}"
                );
            }
        }
        // Scenario b: the merge re-keys the matched release, which is already
        // stale, so nothing newly goes stale.
        let (pool, unit) = preview_scenario('b').await;
        let view = folder_view(&pool, unit).await.unwrap().unwrap();
        assert_eq!(
            view.choices[0],
            (FolderAction::Merge, MatchEffects::default())
        );
        // Scenario d offers Separate and Auto.
        let (pool, unit) = preview_scenario('d').await;
        let view = folder_view(&pool, unit).await.unwrap().unwrap();
        let actions: Vec<FolderAction> = view.choices.iter().map(|choice| choice.0).collect();
        assert_eq!(actions, [FolderAction::Separate, FolderAction::Auto]);
    }

    #[tokio::test]
    async fn a_manual_merge_across_disc_folders_is_labelled_manual() {
        let pool = test_pool().await;
        let files = [
            scanned("/music/set/cd1/01.flac", Some("Alpha"), Some("A"), 1),
            scanned("/music/set/cd2/01.flac", Some("Beta"), Some("A"), 1),
        ];
        insert(&pool, "/music", &files).await;
        assert_eq!(release_ids(&pool).await.len(), 2);
        set_folder_mode(&pool, "/music/set", Some(FolderMode::Merge))
            .await
            .unwrap();

        let units = list_folder_units(&pool, None).await.unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].state, FolderState::MergedByUser);
        let merged = units[0].releases[0].merged.clone().unwrap();
        assert_eq!(
            merged,
            MergeInfo {
                titles: 2,
                folders: 2,
                manual: true
            }
        );
        assert_eq!(merged.label(), "2 album titles, 2 folders (manual)");
    }
}
