use crate::matcher::{self, ArtistKind, Candidate, LocalRelease, LocalTrack, Mapping};
use crate::scanner::ScannedTrack;
use anyhow::{Result, bail};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const STATUS_UNMATCHED: &str = "unmatched";
pub const STATUS_MATCHED: &str = "matched";
pub const STATUS_STALE: &str = "stale";

// ---------------------------------------------------------------------------
// Scanned-metadata storage
// ---------------------------------------------------------------------------

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

/// Writes observed tag metadata. Accepted Discogs metadata lives in separate
/// tables and is never touched here, so re-importing keeps an accepted match.
pub async fn upsert_track(
    pool: &SqlitePool,
    track: &ScannedTrack,
    directory_id: i64,
) -> Result<()> {
    let album_id = match &track.album {
        Some(title) if !title.is_empty() => {
            Some(upsert_album(pool, title, track.artist.as_deref(), track.year).await?)
        }
        _ => None,
    };

    let path_str = track.path.to_string_lossy().to_string();
    sqlx::query(
        "INSERT INTO tracks (
            path, title, artist, album_id, directory_id, track_number, duration_secs, modified_at,
            album_artist, disc_number, barcode, catalog_number, discogs_release_id
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
            discogs_release_id = excluded.discogs_release_id",
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

fn most_common_value(values: impl Iterator<Item = String>) -> Option<(String, String)> {
    let mut counts: HashMap<String, (usize, String)> = HashMap::new();
    for value in values {
        let normalized = matcher::normalize_text(&value);
        if normalized.is_empty() {
            continue;
        }
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

/// Deterministic revision over group membership and matching-relevant observed
/// fields. Re-reading unchanged tags yields the same revision; adding, removing,
/// or retagging a file changes it.
fn revision_of(tracks: &[&ObservedTrack]) -> String {
    let mut rows: Vec<&ObservedTrack> = tracks.to_vec();
    rows.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });

    let mut payload = String::new();
    for track in rows {
        let fields = [
            track.path.to_ascii_lowercase(),
            track.title.clone().unwrap_or_default(),
            track.artist.clone().unwrap_or_default(),
            track.album.clone().unwrap_or_default(),
            track.album_artist.clone().unwrap_or_default(),
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
        ];
        payload.push_str(&fields.join("\u{1f}"));
        payload.push('\u{1e}');
    }

    format!("{:016x}", fnv1a64(payload.as_bytes()))
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
        let album_title_norm = album_title.map(matcher::normalize_text).unwrap_or_default();

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
                .find(|title| matcher::normalize_text(title) == album_title_norm)
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
                    .map(|artist| matcher::normalize_text(artist))
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
                is_compilation,
                has_album_tag,
                track_count: members.len() as i64,
                revision,
                track_ids,
            }
        })
        .collect()
}

async fn observed_tracks(pool: &SqlitePool, scope: Option<&Path>) -> Result<Vec<ObservedTrack>> {
    let mut rows = sqlx::query_as::<_, ObservedTrack>(
        "SELECT t.id AS id, t.path AS path, t.title AS title, t.artist AS artist,
                a.title AS album, t.album_artist AS album_artist, a.year AS year,
                t.track_number AS track_number, t.disc_number AS disc_number,
                t.duration_secs AS duration_secs, t.barcode AS barcode,
                t.catalog_number AS catalog_number, t.discogs_release_id AS discogs_release_id
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         ORDER BY t.path",
    )
    .fetch_all(pool)
    .await?;

    if let Some(scope) = scope {
        rows.retain(|track| path_within(&directory_of(&track.path), scope));
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

/// Recomputes grouping for the selected scope and persists it. Existing track
/// IDs are preserved, and unchanged revisions keep accepted matches intact.
pub async fn rebuild_local_releases(pool: &SqlitePool, scope: Option<&Path>) -> Result<usize> {
    let tracks = observed_tracks(pool, scope).await?;
    let groups = group_tracks(&tracks);

    let mut tx = pool.begin().await?;
    for group in &groups {
        let existing = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT id, source_revision, status FROM local_releases
             WHERE directory_path = ?1 COLLATE NOCASE AND album_title_norm = ?2",
        )
        .bind(&group.directory)
        .bind(&group.album_title_norm)
        .fetch_optional(&mut *tx)
        .await?;

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO local_releases (
                directory_path, album_title_norm, album_title, album_artist, artist_kind, year,
                barcode, catalog_number, embedded_discogs_id, is_compilation, has_album_tag,
                track_count, source_revision, status
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(directory_path, album_title_norm) DO UPDATE SET
                album_title = excluded.album_title,
                album_artist = excluded.album_artist,
                artist_kind = excluded.artist_kind,
                year = excluded.year,
                barcode = excluded.barcode,
                catalog_number = excluded.catalog_number,
                embedded_discogs_id = excluded.embedded_discogs_id,
                is_compilation = excluded.is_compilation,
                has_album_tag = excluded.has_album_tag,
                track_count = excluded.track_count,
                source_revision = excluded.source_revision,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                status = CASE
                    WHEN local_releases.source_revision <> excluded.source_revision
                         AND local_releases.status IN ('matched', 'stale')
                    THEN 'stale'
                    ELSE local_releases.status
                END
             RETURNING id",
        )
        .bind(&group.directory)
        .bind(&group.album_title_norm)
        .bind(&group.album_title)
        .bind(&group.album_artist)
        .bind(group.artist_kind.as_str())
        .bind(group.year)
        .bind(&group.barcode)
        .bind(&group.catalog_number)
        .bind(group.embedded_discogs_id.map(|id| id as i64))
        .bind(group.is_compilation)
        .bind(group.has_album_tag)
        .bind(group.track_count)
        .bind(&group.revision)
        // Existing status is preserved by the conflict branch above.
        .bind(STATUS_UNMATCHED)
        .fetch_one(&mut *tx)
        .await?;

        if let Some((_, previous_revision, _)) = &existing
            && previous_revision != &group.revision
        {
            sqlx::query(
                "UPDATE discogs_matches SET stale = 1
                 WHERE local_release_id = ?1 AND stale = 0",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("UPDATE tracks SET local_release_id = NULL WHERE local_release_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        for track_id in &group.track_ids {
            sqlx::query("UPDATE tracks SET local_release_id = ?1 WHERE id = ?2")
                .bind(id)
                .bind(track_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    // Releases whose files all disappeared no longer exist.
    sqlx::query(
        "DELETE FROM local_releases
         WHERE id NOT IN (SELECT local_release_id FROM tracks WHERE local_release_id IS NOT NULL)",
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(groups.len())
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
}

/// Loads persisted local releases (with their tracks) for matching.
pub async fn load_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalRelease>> {
    let release_rows = sqlx::query_as::<_, LocalReleaseRow>(
        "SELECT id, directory_path, album_title, album_artist, artist_kind, year, barcode,
                catalog_number, embedded_discogs_id, is_compilation, has_album_tag,
                source_revision
         FROM local_releases
         ORDER BY directory_path, album_title_norm",
    )
    .fetch_all(pool)
    .await?;

    let track_rows = sqlx::query_as::<_, LocalTrackRow>(
        "SELECT id, path, title, disc_number, track_number, duration_secs, local_release_id
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
            .push(LocalTrack {
                id: row.id,
                path: row.path,
                title: row.title,
                disc: row.disc_number,
                track_number: row.track_number,
                duration_secs: row.duration_secs,
            });
    }

    let mut releases = Vec::new();
    for row in release_rows {
        if let Some(scope) = scope
            && !path_within(&row.directory_path, scope)
        {
            continue;
        }

        let mut tracks = tracks_by_release.remove(&row.id).unwrap_or_default();
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
            is_compilation: row.is_compilation,
            has_album_tag: row.has_album_tag,
            revision: row.source_revision,
            tracks,
        });
    }

    Ok(releases)
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
    pub discogs_release_id: Option<i64>,
    pub discogs_title: Option<String>,
    pub queued: bool,
}

pub async fn list_local_releases(
    pool: &SqlitePool,
    scope: Option<&Path>,
) -> Result<Vec<LocalReleaseStatus>> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<String>,
            Option<String>,
            i64,
            String,
            Option<i64>,
            Option<String>,
            i64,
        ),
    >(
        "SELECT lr.id, lr.directory_path, lr.album_title, lr.album_artist, lr.track_count,
                lr.status, m.discogs_release_id, m.title,
                CASE WHEN r.id IS NULL THEN 0 ELSE 1 END AS queued
         FROM local_releases lr
         LEFT JOIN discogs_matches m ON m.local_release_id = lr.id
         LEFT JOIN match_reviews r ON r.local_release_id = lr.id
         ORDER BY lr.directory_path, lr.album_title_norm",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter(|row| match scope {
            Some(scope) => path_within(&row.1, scope),
            None => true,
        })
        .map(
            |(
                id,
                directory,
                album_title,
                album_artist,
                track_count,
                status,
                discogs_release_id,
                discogs_title,
                queued,
            )| LocalReleaseStatus {
                id,
                directory,
                album_title,
                album_artist,
                track_count,
                status,
                discogs_release_id,
                discogs_title,
                queued: queued != 0,
            },
        )
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredCandidate {
    pub release: matcher::RemoteRelease,
    pub mapping: Mapping,
    pub score: matcher::ScoreBreakdown,
    pub auto_accept: bool,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub evidence: Vec<String>,
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
    local_release_id: i64,
    candidate: &Candidate,
    method: AcceptanceMethod,
    expected_revision: &str,
) -> Result<()> {
    if !candidate.mapping.complete {
        bail!("refusing to accept an incomplete track mapping");
    }

    let mut tx = pool.begin().await?;

    let current: Option<String> =
        sqlx::query_scalar("SELECT source_revision FROM local_releases WHERE id = ?1")
            .bind(local_release_id)
            .fetch_optional(&mut *tx)
            .await?;

    let Some(current_revision) = current else {
        bail!("local release {local_release_id} no longer exists");
    };
    if current_revision != expected_revision {
        bail!(
            "local release {local_release_id} changed since it was evaluated (revision {current_revision} != {expected_revision})"
        );
    }

    let mut seen_ordinals: Vec<usize> = Vec::new();
    for (_, remote_index) in candidate.mapping.assigned_pairs() {
        seen_ordinals.push(remote_index);
    }
    seen_ordinals.sort_unstable();
    seen_ordinals.dedup();
    if seen_ordinals.len() != candidate.mapping.assigned_count() {
        bail!("refusing to accept a mapping that assigns one Discogs track twice");
    }

    let metadata_json = serde_json::to_string(&candidate.release)?;
    let remote = &candidate.release;

    sqlx::query("DELETE FROM discogs_matches WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discogs_track_matches WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO discogs_matches (
            local_release_id, discogs_release_id, discogs_master_id, release_url, title, artist,
            year, country, label, catalog_number, format, barcode, score, algorithm_version,
            acceptance_method, source_revision, stale, metadata_json
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 0, ?17)",
    )
    .bind(local_release_id)
    .bind(remote.id as i64)
    .bind(remote.master_id.map(|id| id as i64))
    .bind(&remote.url)
    .bind(non_empty(&remote.title))
    .bind(non_empty(&remote.artist_display()))
    .bind(remote.year)
    .bind(&remote.country)
    .bind(remote.label_display())
    .bind(remote.catalog_number())
    .bind(remote.format_display())
    .bind(remote.barcodes.first())
    .bind(candidate.score.total)
    .bind(matcher::ALGORITHM_VERSION)
    .bind(method.as_str())
    .bind(expected_revision)
    .bind(&metadata_json)
    .execute(&mut *tx)
    .await?;

    let tracks = load_tracks_for_release(&mut tx, local_release_id).await?;
    if tracks.len() != candidate.mapping.pairs.len() {
        bail!("local release membership changed while accepting");
    }
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
            bail!("mapping refers to a Discogs track that does not exist");
        };
        let ordinal = ordinals
            .get(&remote_index)
            .copied()
            .unwrap_or(remote_index as i64 + 1);
        let track_artist = if remote_track.artists.is_empty() {
            remote.artist_display()
        } else {
            remote_track.artists.join(", ")
        };

        sqlx::query(
            "INSERT INTO discogs_track_matches (
                local_release_id, track_id, remote_ordinal, remote_position, remote_title,
                remote_artist, remote_duration_secs
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(local_release_id)
        .bind(local_track_id)
        .bind(ordinal)
        .bind(&remote_track.position)
        .bind(non_empty(&remote_track.title))
        .bind(non_empty(&track_artist))
        .bind(remote_track.duration_secs)
        .execute(&mut *tx)
        .await?;
    }

    // Files in the group must all participate in the accepted mapping.
    for track in &tracks {
        let mapped: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM discogs_track_matches WHERE local_release_id = ?1 AND track_id = ?2",
        )
        .bind(local_release_id)
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

    sqlx::query("DELETE FROM match_reviews WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(())
}

async fn load_tracks_for_release(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    local_release_id: i64,
) -> Result<Vec<LocalTrack>> {
    let rows = sqlx::query_as::<_, LocalTrackRow>(
        "SELECT id, path, title, disc_number, track_number, duration_secs, local_release_id
         FROM tracks WHERE local_release_id = ?1
         ORDER BY path",
    )
    .bind(local_release_id)
    .fetch_all(&mut **tx)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| LocalTrack {
            id: row.id,
            path: row.path,
            title: row.title,
            disc: row.disc_number,
            track_number: row.track_number,
            duration_secs: row.duration_secs,
        })
        .collect())
}

/// Removes the accepted overlay and shows scanned metadata again.
pub async fn clear_match(pool: &SqlitePool, local_release_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;

    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM discogs_matches WHERE local_release_id = ?1")
            .bind(local_release_id)
            .fetch_optional(&mut *tx)
            .await?;

    sqlx::query("DELETE FROM discogs_matches WHERE local_release_id = ?1")
        .bind(local_release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discogs_track_matches WHERE local_release_id = ?1")
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

/// Stores candidate summaries for later interactive review.
pub async fn queue_review(
    pool: &SqlitePool,
    local_release_id: i64,
    source_revision: &str,
    candidates: &[StoredCandidate],
    proposed_mapping: &Mapping,
    reasons: &[String],
) -> Result<()> {
    let candidates_json = serde_json::to_string(candidates)?;
    let mapping_json = serde_json::to_string(proposed_mapping)?;
    let reasons_json = serde_json::to_string(reasons)?;

    sqlx::query(
        "INSERT INTO match_reviews (
            local_release_id, candidates_json, proposed_mapping_json, reasons_json, source_revision
         )
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(local_release_id) DO UPDATE SET
            candidates_json = excluded.candidates_json,
            proposed_mapping_json = excluded.proposed_mapping_json,
            reasons_json = excluded.reasons_json,
            source_revision = excluded.source_revision,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(local_release_id)
    .bind(candidates_json)
    .bind(mapping_json)
    .bind(reasons_json)
    .bind(source_revision)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Effective metadata (accepted Discogs values with scanned fallback)
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
pub struct TrackRow {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<i64>,
    pub duration_secs: Option<f64>,
    pub stale: bool,
}

pub async fn list_tracks(pool: &SqlitePool) -> Result<Vec<TrackRow>> {
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path,
                COALESCE(tm.remote_title, t.title) AS title,
                COALESCE(tm.remote_artist, t.artist) AS artist,
                COALESCE(m.title, a.title) AS album,
                COALESCE(tm.remote_ordinal, t.track_number) AS track_number,
                t.duration_secs AS duration_secs,
                (m.stale = 1) AS stale
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         LEFT JOIN discogs_matches m ON m.local_release_id = t.local_release_id
         LEFT JOIN discogs_track_matches tm ON tm.track_id = t.id
         ORDER BY album, track_number, title",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn search_tracks(pool: &SqlitePool, query: &str) -> Result<Vec<TrackRow>> {
    let pattern = format!("%{query}%");
    let rows = sqlx::query_as::<_, TrackRow>(
        "SELECT t.path AS path,
                COALESCE(tm.remote_title, t.title) AS title,
                COALESCE(tm.remote_artist, t.artist) AS artist,
                COALESCE(m.title, a.title) AS album,
                COALESCE(tm.remote_ordinal, t.track_number) AS track_number,
                t.duration_secs AS duration_secs,
                (m.stale = 1) AS stale
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         LEFT JOIN discogs_matches m ON m.local_release_id = t.local_release_id
         LEFT JOIN discogs_track_matches tm ON tm.track_id = t.id
         WHERE COALESCE(tm.remote_title, t.title) LIKE ?1 COLLATE NOCASE
            OR COALESCE(tm.remote_artist, t.artist) LIKE ?1 COLLATE NOCASE
            OR COALESCE(m.title, a.title) LIKE ?1 COLLATE NOCASE
         ORDER BY album, track_number, title",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{RemoteRelease, RemoteTrack, TrackEntryType};
    use crate::scanner::{self, ScannedTrack};
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
            duration_secs: Some(180.0),
            modified_at: 1,
        }
    }

    async fn test_pool() -> SqlitePool {
        crate::db::connect_in_memory().await.unwrap()
    }

    async fn insert(pool: &SqlitePool, root: &str, tracks: &[ScannedTrack]) {
        let directory_id = upsert_directory(pool, Path::new(root)).await.unwrap();
        for track in tracks {
            upsert_track(pool, track, directory_id).await.unwrap();
        }
        rebuild_local_releases(pool, None).await.unwrap();
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
    }

    fn remote_release() -> RemoteRelease {
        RemoteRelease {
            id: 42,
            master_id: None,
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
                title: "Discogs Track 1".to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
            }],
            url: "https://www.discogs.com/release/42".to_string(),
        }
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
        accept_match(
            &pool,
            release.id,
            &candidate,
            AcceptanceMethod::Automatic,
            &release.revision,
        )
        .await
        .unwrap();

        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Discogs Track 1"));
        assert!(!rows[0].stale);

        // Re-import with unchanged tags and rebuild grouping.
        insert(
            &pool,
            "/music/a",
            &[scanned("/music/a/01.flac", Some("Blue"), Some("Artist"), 1)],
        )
        .await;
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discogs_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 1);
        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Discogs Track 1"));

        assert!(clear_match(&pool, release.id).await.unwrap());
        let rows = list_tracks(&pool).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Track 1"));
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discogs_matches")
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
        accept_match(
            &pool,
            release.id,
            &candidate,
            AcceptanceMethod::Manual,
            &release.revision,
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
            release.id,
            &candidate,
            AcceptanceMethod::Manual,
            &release.revision,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("changed since it was evaluated"));

        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discogs_matches")
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
        assert!(!candidate.mapping.complete);

        assert!(
            accept_match(
                &pool,
                release.id,
                &candidate,
                AcceptanceMethod::Manual,
                &release.revision
            )
            .await
            .is_err()
        );
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discogs_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 0);
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
        accept_match(
            &pool,
            release.id,
            &candidate,
            AcceptanceMethod::Manual,
            &release.revision,
        )
        .await
        .unwrap();

        assert_eq!(
            search_tracks(&pool, "Discogs Track 1").await.unwrap().len(),
            1
        );
        assert!(!search_tracks(&pool, "Blue").await.unwrap().is_empty());
        assert!(search_tracks(&pool, "nothing").await.unwrap().is_empty());

        clear_match(&pool, release.id).await.unwrap();
        assert!(
            search_tracks(&pool, "Discogs Track 1")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(search_tracks(&pool, "Track 1").await.unwrap().len(), 1);
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
        accept_match(
            &pool,
            release.id,
            &candidate,
            AcceptanceMethod::Manual,
            &release.revision,
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
    async fn path_within_respects_component_boundaries() {
        assert!(path_within("/music/Blue", Path::new("/music")));
        assert!(path_within("/MUSIC/blue", Path::new("/music/")));
        assert!(!path_within("/music/Bluegrass", Path::new("/music/Blue")));
        assert!(!path_within("/other/Blue", Path::new("/music")));
    }
}
