//! Folder decisions and the views of units: `radish folder` and the web
//! interface's folder pages.

// Rust guideline compliant 2026-08-14

use super::*;
use crate::grouping::FolderAction;

/// Every folder decision, by lowercased unit path.
pub(super) async fn folder_decisions(
    conn: &mut SqliteConnection,
) -> Result<HashMap<String, FolderMode>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT directory_path, mode FROM folder_groupings")
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(path, mode)| Some((path.to_ascii_lowercase(), FolderMode::parse(&mode)?)))
        .collect())
}

/// What `folder merge`, `split` and `auto` name: a local release, or a folder
/// path as stored (the library may live on another machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderTarget {
    Release(i64),
    Path(String),
}

/// The unit `target` names, spelled as stored (no trailing slash), or `None`
/// when it names none.
///
/// A release names its unit. A path names the unit of the releases in that
/// folder, or the unit at that path, so a disc subfolder names the unit
/// above it. A disc subfolder whose files all went into a merged release still
/// names that release's unit.
pub async fn resolve_folder(pool: &SqlitePool, target: &FolderTarget) -> Result<Option<String>> {
    let path = match target {
        FolderTarget::Release(id) => {
            return Ok(
                sqlx::query_scalar("SELECT unit_path FROM local_releases WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?,
            );
        }
        FolderTarget::Path(path) => path.trim_end_matches(['/', '\\']),
    };
    let found: Option<String> = sqlx::query_scalar(
        "SELECT unit_path FROM local_releases
         WHERE unit_path = ?1 OR directory_path = ?1
         ORDER BY unit_path = ?1 DESC LIMIT 1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    if found.is_some() {
        return Ok(found);
    }
    if let Some(parent) = grouping::disc_parent_of(Path::new(path)) {
        // The folder must hold files of the merged release: a disc folder that
        // does not exist names nothing.
        return Ok(sqlx::query_scalar(
            "SELECT unit_path FROM local_releases
             WHERE unit_path = ?1 AND album_title_norm = ?2
               AND EXISTS (SELECT 1 FROM tracks t
                           WHERE t.local_release_id = local_releases.id
                             AND substr(replace(t.path, '\\', '/'), 1, length(?3) + 1)
                                 = (?3 || '/') COLLATE NOCASE)
             LIMIT 1",
        )
        .bind(parent.to_string_lossy().as_ref())
        .bind(MERGED_KEY)
        .bind(path.replace('\\', "/"))
        .fetch_optional(pool)
        .await?);
    }
    Ok(None)
}

/// What a folder decision did to its unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderOutcome {
    /// The unit's path.
    pub unit: String,
    /// The regroup's counts, including matches marked stale or dropped.
    pub summary: RebuildSummary,
    /// The unit's releases afterwards: ID and track count, in ID order.
    pub releases: Vec<(i64, i64)>,
}

/// Records `mode` for `unit` (`None` removes the decision, which leaves the
/// unit to the automatic rule) and regroups the unit, in one transaction.
pub async fn set_folder_mode(
    pool: &SqlitePool,
    unit: &str,
    mode: Option<FolderMode>,
) -> Result<FolderOutcome> {
    let unit = unit.trim_end_matches(['/', '\\']);
    let mut tx = begin_write(pool).await?;
    match mode {
        Some(mode) => {
            sqlx::query(
                "INSERT INTO folder_groupings (directory_path, mode) VALUES (?1, ?2)
                 ON CONFLICT(directory_path) DO UPDATE SET
                     mode = excluded.mode,
                     decided_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            )
            .bind(unit)
            .bind(mode.as_str())
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM folder_groupings WHERE directory_path = ?1")
                .bind(unit)
                .execute(&mut *tx)
                .await?;
        }
    }
    let summary = rebuild_in(&mut tx, Some(Path::new(unit))).await?;
    let releases: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, track_count FROM local_releases WHERE unit_path = ?1 ORDER BY id",
    )
    .bind(unit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(FolderOutcome {
        unit: unit.to_string(),
        summary,
        releases,
    })
}

/// How a unit is grouped and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderState {
    MergedByUser,
    SeparateByUser,
    MergedAutomatically,
    /// No decision, and the automatic rule keeps the releases apart.
    Automatic,
}

impl FolderState {
    pub(crate) fn of(decision: Option<FolderMode>, merged: bool) -> Self {
        match (decision, merged) {
            (Some(FolderMode::Merge), _) => Self::MergedByUser,
            (Some(FolderMode::Separate), _) => Self::SeparateByUser,
            (None, true) => Self::MergedAutomatically,
            (None, false) => Self::Automatic,
        }
    }

    /// How `folder list` and the web interface say it.
    pub fn label(self) -> &'static str {
        match self {
            Self::MergedByUser => "merged by you",
            Self::SeparateByUser => "kept separate by you",
            Self::MergedAutomatically => "merged automatically",
            Self::Automatic => "automatic: separate",
        }
    }
}

/// A unit with its releases.
#[derive(Debug, Clone)]
pub struct FolderUnit {
    pub path: String,
    pub state: FolderState,
    /// In `list_local_releases` order.
    pub releases: Vec<LocalReleaseStatus>,
}

/// The units under `scope` that hold two or more releases, a merged release or
/// a decision, in path order.
pub async fn list_folder_units(pool: &SqlitePool, scope: Option<&Path>) -> Result<Vec<FolderUnit>> {
    let releases = list_local_releases(pool, scope).await?;
    let decisions = {
        let mut conn = pool.acquire().await?;
        folder_decisions(&mut conn).await?
    };
    Ok(folder_units(releases, &decisions))
}

fn folder_units(
    releases: Vec<LocalReleaseStatus>,
    decisions: &HashMap<String, FolderMode>,
) -> Vec<FolderUnit> {
    let mut by_unit: BTreeMap<String, (String, Vec<LocalReleaseStatus>)> = BTreeMap::new();
    for release in releases {
        by_unit
            .entry(release.unit.to_ascii_lowercase())
            .or_insert_with(|| (release.unit.clone(), Vec::new()))
            .1
            .push(release);
    }
    by_unit
        .into_iter()
        .filter_map(|(key, (path, releases))| {
            let decision = decisions.get(&key).copied();
            let merged = releases.iter().any(|release| release.merged.is_some());
            if releases.len() < 2 && !merged && decision.is_none() {
                return None;
            }
            Some(FolderUnit {
                path,
                state: FolderState::of(decision, merged),
                releases,
            })
        })
        .collect()
}

/// What a folder decision would do to accepted matches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MatchEffects {
    /// Accepted matches that would be marked stale.
    pub stale: usize,
    /// Accepted matches that would be deleted with their release.
    pub dropped: usize,
}

impl MatchEffects {
    /// `1 accepted match becomes stale, 2 accepted matches are dropped`, or
    /// `None` when no match changes.
    pub fn describe(self) -> Option<String> {
        let mut parts = Vec::new();
        if self.stale > 0 {
            parts.push(format!(
                "{} {} stale",
                plural(self.stale as u64, "accepted match", "accepted matches"),
                if self.stale == 1 { "becomes" } else { "become" }
            ));
        }
        if self.dropped > 0 {
            parts.push(format!(
                "{} {} dropped",
                plural(self.dropped as u64, "accepted match", "accepted matches"),
                if self.dropped == 1 { "is" } else { "are" }
            ));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// A file of a unit as the folder page lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderFile {
    pub path: String,
    /// The disc the file is on in a merged release of the unit.
    pub disc: Option<i64>,
    pub track_number: Option<i64>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
}

/// One unit as the web interface shows it.
#[derive(Debug, Clone)]
pub struct FolderView {
    pub unit: FolderUnit,
    /// The unit's files, in path order.
    pub files: Vec<FolderFile>,
    /// Each action that would change the unit's decision, with what it would
    /// do to accepted matches.
    pub choices: Vec<(FolderAction, MatchEffects)>,
}

/// The unit that `path` names (see [`resolve_folder`]), or `None` when it names
/// none.
///
/// Each choice is previewed by grouping and planning the unit's files under
/// that decision, without writing anything. Only the unit is read, in one read
/// transaction. Files not yet assigned to a release (an interrupted import) are
/// not shown until the next regroup.
pub async fn folder_view(pool: &SqlitePool, path: &str) -> Result<Option<FolderView>> {
    let Some(unit) = resolve_folder(pool, &FolderTarget::Path(path.to_string())).await? else {
        return Ok(None);
    };

    let mut tx = pool.begin().await?;
    let (releases, decisions) = status_listing(&mut tx, Some(&unit)).await?;
    let ids: Vec<i64> = releases.iter().map(|release| release.id).collect();
    let tracks = observed_tracks_of(&mut tx, &ids).await?;
    let stored = stored_releases_by_id(&mut tx, &ids).await?;
    tx.commit().await?;
    if releases.is_empty() {
        return Ok(None);
    }

    let unit_key = unit.to_ascii_lowercase();
    let decision = decisions.get(&unit_key).copied();
    let choices = FolderAction::ALL
        .into_iter()
        .filter(|action| action.mode() != decision)
        .map(|action| {
            let mut decisions = decisions.clone();
            match action.mode() {
                Some(mode) => {
                    decisions.insert(unit_key.clone(), mode);
                }
                None => {
                    decisions.remove(&unit_key);
                }
            }
            (action, preview_effects(&tracks, &stored, decisions))
        })
        .collect();
    let files = tracks
        .iter()
        .map(|track| FolderFile {
            path: track.path.clone(),
            disc: grouping::effective_disc(
                &unit,
                &directory_of(&track.path),
                track.album.as_deref(),
                track.disc_number,
            ),
            track_number: track.track_number,
            album: track.album.clone(),
            title: track.title.clone(),
            duration_secs: track.duration_secs,
        })
        .collect();
    let merged = releases.iter().any(|release| release.merged.is_some());
    Ok(Some(FolderView {
        unit: FolderUnit {
            path: unit,
            state: FolderState::of(decision, merged),
            releases,
        },
        files,
        choices,
    }))
}

/// What regrouping `tracks` under `decisions` would do to the accepted matches
/// of `stored`, by the same grouping and plan a rebuild runs.
fn preview_effects(
    tracks: &[ObservedTrack],
    stored: &[StoredRelease],
    decisions: HashMap<String, FolderMode>,
) -> MatchEffects {
    let context = GroupingContext {
        decisions,
        prior: prior_releases(stored),
    };
    let groups = group_tracks(tracks, &context);
    let members = release_members(tracks);
    let plan = plan_rebuild(&groups, stored, &members);
    let stored_by_id: HashMap<i64, &StoredRelease> =
        stored.iter().map(|row| (row.id, row)).collect();

    let mut effects = MatchEffects::default();
    for (group, target) in groups.iter().zip(&plan.targets) {
        let (Target::Existing(id) | Target::Rekey(id)) = *target else {
            continue;
        };
        let row = stored_by_id[&id];
        let same_members =
            members.get(&id).map(Vec::as_slice).unwrap_or_default() == group.track_ids.as_slice();
        // A re-keyed release goes stale on a changed revision alone; a kept
        // one also when its files changed. Only fresh matches are marked.
        let changed = row.fields.revision != group.revision
            || (matches!(target, Target::Existing(_)) && !same_members);
        if row.has_fresh_match && changed {
            effects.stale += 1;
        }
    }
    effects.dropped = plan
        .removed
        .iter()
        .filter(|id| stored_by_id.get(*id).is_some_and(|row| row.has_match))
        .count();
    effects
}

/// The observed tracks of the given releases, in path order.
async fn observed_tracks_of(
    conn: &mut SqliteConnection,
    release_ids: &[i64],
) -> Result<Vec<ObservedTrack>> {
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
         WHERE t.local_release_id IN (SELECT value FROM json_each(?1))
         ORDER BY t.path",
    )
    .bind(serde_json::to_string(release_ids)?)
    .fetch_all(&mut *conn)
    .await?;
    for row in &mut rows {
        row.musicbrainz_ids = serde_json::from_str(&row.musicbrainz_ids_json).unwrap_or_default();
    }
    Ok(rows)
}
