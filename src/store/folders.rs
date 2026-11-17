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
