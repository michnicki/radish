//! The review queue as the web pages read it, and the one write that adds a
//! candidate to a saved review.
//!
//! A release is *queued* when it is not matched and has at least one
//! `match_reviews` row. [`add_review_candidate`] puts a release fetched by its
//! ID in front of a provider's saved candidates, in one write transaction so a
//! concurrent `match` cannot interleave with it.

// Rust guideline compliant 2026-08-14

use super::{MatchGuard, StoredCandidate, begin_write, decode_stored_candidates, queue_review_in};
use crate::providers::{ProviderKind, ProviderPolicy};
use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;

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

/// The condition that puts release `lr` in the review queue: not matched,
/// with a saved review of provider `?1` (any provider when `?1` is `NULL`).
///
/// SQLite 3.51 (bundled through libsqlite3-sys 0.37) rewrites a plain
/// correlated `EXISTS` into a join. Without the `LIMIT 1` that join yields a
/// release once per review row and uses up `OFFSET` on each of those rows, so
/// pages start too early and overlap. The `LIMIT 1` keeps the subquery
/// correlated, so the listing index still serves the order and no page sorts
/// the whole queue.
macro_rules! queued_where {
    () => {
        "lr.status <> 'matched'
         AND EXISTS (SELECT 1 FROM match_reviews r
                     WHERE r.local_release_id = lr.id AND (?1 IS NULL OR r.provider = ?1)
                     LIMIT 1)"
    };
}

/// [`queued_where!`] for counting, where the `IN` form is faster.
macro_rules! queued_count_where {
    () => {
        "lr.status <> 'matched'
         AND lr.id IN (SELECT r.local_release_id FROM match_reviews r
                       WHERE ?1 IS NULL OR r.provider = ?1)"
    };
}

/// One queued release as the queue page lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedRelease {
    /// The local release's ID.
    pub id: i64,
    /// The folder the release's files are in.
    pub directory: String,
    /// The unit the release belongs to, which its folder link names.
    pub unit: String,
    /// The scanned album title, when the files agree on one.
    pub album_title: Option<String>,
    /// The scanned album artist, when the files agree on one.
    pub album_artist: Option<String>,
    /// How many tracks the release has.
    pub track_count: i64,
    /// Each queued provider with its first candidate's score, in the order
    /// matching tries providers; `None` when that snapshot cannot be read or
    /// holds no candidate.
    pub best: Vec<(ProviderKind, Option<f64>)>,
}

/// A queue row: ID, folder, unit, album title and artist, track count.
type QueueRow = (i64, String, String, Option<String>, Option<String>, i64);

/// One page of the review queue, in the order of the releases listing.
///
/// `provider` keeps only releases queued for that provider. Only the first
/// candidate of each review is read (`->`), so a page does not
/// decode megabytes of snapshots.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn queue_page(
    pool: &SqlitePool,
    provider: Option<ProviderKind>,
    limit: i64,
    offset: i64,
) -> Result<Vec<QueuedRelease>> {
    let filter = provider.map(ProviderKind::as_str);
    let mut tx = pool.begin().await?;
    let rows: Vec<QueueRow> = sqlx::query_as(concat!(
        "SELECT lr.id, lr.directory_path, lr.unit_path, lr.album_title, lr.album_artist,
                    lr.track_count
             FROM local_releases lr WHERE ",
        queued_where!(),
        " ORDER BY lr.directory_path, lr.album_title_norm LIMIT ?2 OFFSET ?3"
    ))
    .bind(filter)
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    let ids = serde_json::to_string(&rows.iter().map(|row| row.0).collect::<Vec<_>>())?;
    // `->` fails the whole query on malformed JSON, so a corrupt snapshot is
    // skipped (no score) instead of breaking the page. It returns the element
    // as JSON text, so a first element that is not an object decodes (and
    // fails) here, not in the query.
    let firsts: Vec<(i64, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT local_release_id, provider, snapshot_version,
                CASE WHEN json_valid(candidates_json)
                     THEN candidates_json -> '$[0]' END
         FROM match_reviews WHERE local_release_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let order = ProviderPolicy::Chain { deezer: true }.order();
    let mut best: HashMap<i64, Vec<(ProviderKind, Option<f64>)>> = HashMap::new();
    for (id, provider, version, first) in firsts {
        let Some(provider) = ProviderKind::parse(&provider) else {
            continue;
        };
        let score = first
            .and_then(|json| decode_stored_candidates(version, provider, &format!("[{json}]")).ok())
            .and_then(|candidates| candidates.into_iter().next())
            .map(|candidate| candidate.match_scope.score(&candidate.score));
        best.entry(id).or_default().push((provider, score));
    }
    for scores in best.values_mut() {
        scores.sort_by_key(|(provider, _)| order.iter().position(|ordered| ordered == provider));
    }
    Ok(rows
        .into_iter()
        .map(
            |(id, directory, unit, album_title, album_artist, track_count)| QueuedRelease {
                id,
                directory,
                unit,
                album_title,
                album_artist,
                track_count,
                best: best.remove(&id).unwrap_or_default(),
            },
        )
        .collect())
}

/// How many releases are queued, for `provider` or for any.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn queue_count(pool: &SqlitePool, provider: Option<ProviderKind>) -> Result<i64> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM local_releases lr WHERE ",
        queued_count_where!()
    ))
    .bind(provider.map(ProviderKind::as_str))
    .fetch_one(pool)
    .await?)
}

/// The first queued release after release `after`, in queue order.
///
/// `after` need not be queued itself. When it no longer exists, the queue
/// starts again from the top.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn next_queued(
    pool: &SqlitePool,
    after: i64,
    provider: Option<ProviderKind>,
) -> Result<Option<i64>> {
    let filter = provider.map(ProviderKind::as_str);
    let position: Option<(String, String)> =
        sqlx::query_as("SELECT directory_path, album_title_norm FROM local_releases WHERE id = ?1")
            .bind(after)
            .fetch_optional(pool)
            .await?;
    let next = match position {
        // Directory paths compare without case, as the listing orders them;
        // (directory, title) is unique, so the order is total.
        Some((directory, title)) => {
            sqlx::query_scalar(concat!(
                "SELECT lr.id FROM local_releases lr WHERE ",
                queued_where!(),
                " AND (lr.directory_path, lr.album_title_norm) > (?2, ?3)
                 ORDER BY lr.directory_path, lr.album_title_norm LIMIT 1"
            ))
            .bind(filter)
            .bind(directory)
            .bind(title)
            .fetch_optional(pool)
            .await?
        }
        None => {
            sqlx::query_scalar(concat!(
                "SELECT lr.id FROM local_releases lr WHERE ",
                queued_where!(),
                " ORDER BY lr.directory_path, lr.album_title_norm LIMIT 1"
            ))
            .bind(filter)
            .fetch_optional(pool)
            .await?
        }
    };
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderKind, ReleaseId};
    use crate::test_support::{FIXTURE_MBID, mbid_release, queue, remote_release, seed_release};

    /// Releases a to d; a is queued for MusicBrainz and Discogs, b for
    /// MusicBrainz, d for Deezer, and c not at all.
    async fn library() -> (SqlitePool, [i64; 4]) {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let a = seed_release(&pool, "/music/a", "Blue", &titles).await;
        let b = seed_release(&pool, "/music/b", "Red", &titles).await;
        let c = seed_release(&pool, "/music/c", "Green", &titles).await;
        let d = seed_release(&pool, "/music/d", "Gold", &titles).await;
        queue(
            &pool,
            &b,
            remote_release(mbid_release(FIXTURE_MBID), "Red", &titles),
        )
        .await;
        queue(
            &pool,
            &a,
            remote_release(ReleaseId::Discogs(42), "Blue", &titles),
        )
        .await;
        queue(
            &pool,
            &a,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        queue(
            &pool,
            &d,
            remote_release(ReleaseId::Deezer(7), "Gold", &titles),
        )
        .await;
        (pool, [a.id, b.id, c.id, d.id])
    }

    #[tokio::test]
    async fn folders_order_without_regard_to_case() {
        let (pool, [a, b, c, d]) = library().await;
        let titles = ["One", "Two"];
        // Sorts between /music/b and /music/c only when case is ignored.
        let mixed = seed_release(&pool, "/music/B2", "Pink", &titles).await;
        queue(
            &pool,
            &mixed,
            remote_release(mbid_release(FIXTURE_MBID), "Pink", &titles),
        )
        .await;
        let page = queue_page(&pool, None, 10, 0).await.unwrap();
        assert_eq!(ids(&page), vec![a, b, mixed.id, d]);
        assert_eq!(next_queued(&pool, b, None).await.unwrap(), Some(mixed.id));
        assert_eq!(next_queued(&pool, mixed.id, None).await.unwrap(), Some(d));
        assert_eq!(next_queued(&pool, c, None).await.unwrap(), Some(d));
    }

    fn ids(rows: &[QueuedRelease]) -> Vec<i64> {
        rows.iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn the_queue_lists_unmatched_releases_with_reviews_in_folder_order() {
        let (pool, [a, b, _, d]) = library().await;
        let page = queue_page(&pool, None, 10, 0).await.unwrap();
        assert_eq!(ids(&page), vec![a, b, d]);
        let providers: Vec<ProviderKind> =
            page[0].best.iter().map(|(provider, _)| *provider).collect();
        assert_eq!(
            providers,
            vec![ProviderKind::MusicBrainz, ProviderKind::Discogs]
        );
        assert!(page[0].best.iter().all(|(_, score)| score.is_some()));
        assert_eq!(page[0].album_title.as_deref(), Some("Blue"));
        assert_eq!(page[0].track_count, 2);

        assert_eq!(ids(&queue_page(&pool, None, 1, 1).await.unwrap()), vec![b]);
        assert_eq!(
            ids(&queue_page(&pool, Some(ProviderKind::Deezer), 10, 0)
                .await
                .unwrap()),
            vec![d]
        );
        assert_eq!(queue_count(&pool, None).await.unwrap(), 3);
        assert_eq!(
            queue_count(&pool, Some(ProviderKind::MusicBrainz))
                .await
                .unwrap(),
            2
        );

        // A matched release with a leftover review is not queued.
        sqlx::query("UPDATE local_releases SET status = 'matched' WHERE id = ?1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            ids(&queue_page(&pool, None, 10, 0).await.unwrap()),
            vec![a, d]
        );

        // A snapshot with no candidate, a scalar one, or a corrupt one, reads as no score.
        for json in ["[]", "[1]", "nonsense"] {
            sqlx::query("UPDATE match_reviews SET candidates_json = ?1 WHERE provider = 'deezer'")
                .bind(json)
                .execute(&pool)
                .await
                .unwrap();
            let page = queue_page(&pool, Some(ProviderKind::Deezer), 10, 0)
                .await
                .unwrap();
            assert_eq!(page[0].best, vec![(ProviderKind::Deezer, None)], "{json}");
        }
    }

    #[tokio::test]
    async fn next_in_queue_follows_folder_order_and_the_filter() {
        let (pool, [a, b, c, d]) = library().await;
        assert_eq!(next_queued(&pool, a, None).await.unwrap(), Some(b));
        assert_eq!(next_queued(&pool, b, None).await.unwrap(), Some(d));
        assert_eq!(next_queued(&pool, d, None).await.unwrap(), None);
        // From a release that is not queued, the next queued one after it.
        assert_eq!(next_queued(&pool, c, None).await.unwrap(), Some(d));
        assert_eq!(
            next_queued(&pool, a, Some(ProviderKind::Deezer))
                .await
                .unwrap(),
            Some(d)
        );
        // A release that is gone starts the queue again from the top.
        assert_eq!(next_queued(&pool, d + 100, None).await.unwrap(), Some(a));
    }
}
