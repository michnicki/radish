//! The review queue as the web pages read it, and the one write that adds a
//! candidate to a saved review.
//!
//! A release is *queued* when it is not matched and has at least one
//! `match_reviews` row. [`add_review_candidate`] puts a release fetched by its
//! ID in front of a provider's saved candidates, in one write transaction so a
//! concurrent `match` cannot interleave with it.

// Rust guideline compliant 2026-08-14

use super::{MatchGuard, StoredCandidate, begin_write, decode_stored_candidates, queue_review_in};
use anyhow::Result;
use sqlx::SqlitePool;

/// Adds `candidate` in front of its provider's saved review of a release.
///
/// The release must still have source `revision`. A saved review made for the
/// release as it is now keeps its other candidates (without an earlier entry
/// for the same release) and its reasons; any other saved review is replaced
/// by `candidate` alone, with `reason`. The review's guard is the release's
/// current revision and accepted match.
///
/// Returns `false`, writing nothing, when the release is gone or changed.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn add_review_candidate(
    pool: &SqlitePool,
    local_release_id: i64,
    revision: &str,
    candidate: StoredCandidate,
    reason: &str,
) -> Result<bool> {
    let provider = candidate.release.provider();
    let mut tx = begin_write(pool).await?;
    let current: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT lr.source_revision,
                (SELECT id FROM release_matches m WHERE m.local_release_id = lr.id)
         FROM local_releases lr WHERE lr.id = ?1",
    )
    .bind(local_release_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((source_revision, accepted_match_id)) = current else {
        return Ok(false);
    };
    if source_revision != revision {
        return Ok(false);
    }
    let guard = MatchGuard {
        source_revision,
        accepted_match_id,
    };

    let existing: Option<(i64, Option<i64>, String, String, String)> = sqlx::query_as(
        "SELECT snapshot_version, expected_match_id, candidates_json, reasons_json, source_revision
         FROM match_reviews WHERE local_release_id = ?1 AND provider = ?2",
    )
    .bind(local_release_id)
    .bind(provider.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let kept = existing.and_then(|(version, expected, json, reasons, saved_revision)| {
        let saved = MatchGuard {
            source_revision: saved_revision,
            accepted_match_id: expected,
        };
        if saved != guard {
            return None;
        }
        let candidates = decode_stored_candidates(version, provider, &json).ok()?;
        let reasons: Vec<String> = serde_json::from_str(&reasons).unwrap_or_default();
        Some((candidates, reasons))
    });
    let (mut candidates, reasons) = kept.unwrap_or_else(|| (Vec::new(), vec![reason.to_string()]));
    candidates.retain(|saved| saved.release.id != candidate.release.id);
    candidates.insert(0, candidate);

    queue_review_in(
        &mut tx,
        local_release_id,
        provider,
        &guard.source_revision,
        guard.accepted_match_id,
        &candidates,
        &candidates[0].mapping,
        &reasons,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}
