//! Folder decisions and the views of units: `radish folder` and the web
//! interface's folder pages.

// Rust guideline compliant 2026-08-14

use super::*;

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
