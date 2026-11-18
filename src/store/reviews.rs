//! The review queue as the web pages read it, and the one write that adds a
//! candidate to a saved review.
//!
//! A release is *queued* when it is not matched and has at least one
//! `match_reviews` row. [`add_review_candidate`] puts a release fetched by its
//! ID in front of a provider's saved candidates, in one write transaction so a
//! concurrent `match` cannot interleave with it.

// Rust guideline compliant 2026-08-14

use super::{MatchGuard, StoredCandidate, begin_write, decode_stored_candidates, queue_review_in};
use crate::layout::grouped;
use crate::matcher::{self, Candidate, LocalRelease};
use crate::providers::ProviderKind;
use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;

/// What the queue shows of a saved review: its first candidate, summarized.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewSummary {
    /// The first candidate's score at the scope it was judged at; `None`
    /// without a candidate.
    pub score: Option<f64>,
    /// Whether the first candidate's mapping can be accepted.
    pub can_apply: bool,
    /// The first candidate's first blocker, else the review's first reason.
    pub reason: Option<String>,
    /// How many blockers the first candidate has.
    pub blockers: i64,
}

/// Summarizes the first of `candidates`, saved for `release`, as the page shows it.
///
/// `can_apply` is [`matcher::mapping_can_apply`], the check the Accept button
/// uses, so the queue's "can be accepted" never disagrees with the page.
pub fn summarize(
    release: &LocalRelease,
    candidates: &[StoredCandidate],
    reasons: &[String],
) -> ReviewSummary {
    let Some(first) = candidates.first() else {
        return ReviewSummary {
            score: None,
            can_apply: false,
            reason: reasons.first().cloned(),
            blockers: 0,
        };
    };
    let candidate = Candidate {
        release: first.release.clone(),
        mapping: first.mapping.clone(),
        score: first.score,
    };
    ReviewSummary {
        score: Some(first.match_scope.score(&first.score)),
        can_apply: matcher::mapping_can_apply(release, &candidate, first.match_scope),
        reason: first.blockers.first().or_else(|| reasons.first()).cloned(),
        blockers: first.blockers.len() as i64,
    }
}

/// Adds `candidate` in front of its provider's saved review of a release.
///
/// The release must still have the source revision `release` was loaded with.
/// A saved review made for the release as it is now keeps its other candidates (without an earlier entry
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
    release: &LocalRelease,
    candidate: StoredCandidate,
    reason: &str,
) -> Result<bool> {
    let provider = candidate.release.provider();
    let local_release_id = release.id;
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
    if source_revision != release.revision {
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
        release,
        provider,
        guard.accepted_match_id,
        &candidates,
        &candidates[0].mapping,
        &reasons,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Releases whose saved reviews one chunk of [`fill_review_summaries`] reads.
///
/// 2,000 releases hold about 21,000 tracks and 4,500 first candidates (about
/// 30 MB of JSON), so memory stays bounded; each chunk is one write
/// transaction, so a concurrent writer waits at most for one chunk.
const SUMMARY_CHUNK: usize = 2_000;

/// Summarizes every saved review that has no summary yet; returns how many.
///
/// Runs after migrations on every open and returns at once when nothing is
/// left (an index-only check). Reviews saved before the summary columns
/// existed are summarized here, once: only each review's first candidate is
/// read, the releases are loaded in chunks of [`SUMMARY_CHUNK`], and progress
/// goes to stderr. A review that cannot be decoded, or that was saved before
/// its files changed, is summarized as one that cannot be accepted, so no row
/// is left for the next open.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn fill_review_summaries(pool: &SqlitePool) -> Result<u64> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM match_reviews WHERE top_can_apply IS NULL)",
    )
    .fetch_one(pool)
    .await?;
    if !pending {
        return Ok(0);
    }
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT local_release_id FROM match_reviews
         WHERE top_can_apply IS NULL ORDER BY local_release_id",
    )
    .fetch_all(pool)
    .await?;
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM match_reviews WHERE top_can_apply IS NULL")
            .fetch_one(pool)
            .await?;
    let total = total.max(0).unsigned_abs();
    eprintln!(
        "Summarizing {} saved reviews for the review queue\u{2026}",
        grouped(total)
    );

    let mut done = 0u64;
    for chunk in ids.chunks(SUMMARY_CHUNK) {
        let releases: HashMap<i64, LocalRelease> = super::load_releases_by_id(pool, chunk)
            .await?
            .into_iter()
            .map(|release| (release.id, release))
            .collect();
        let rows: Vec<PendingRow> = sqlx::query_as(
            "SELECT local_release_id, provider, snapshot_version, source_revision, reasons_json,
                    json_valid(candidates_json),
                    CASE WHEN json_valid(candidates_json) THEN candidates_json -> '$[0]' END
             FROM match_reviews
             WHERE top_can_apply IS NULL
               AND local_release_id IN (SELECT value FROM json_each(?1))",
        )
        .bind(serde_json::to_string(chunk)?)
        .fetch_all(pool)
        .await?;
        let mut tx = begin_write(pool).await?;
        for row in &rows {
            let summary = summary_of_row(releases.get(&row.0), row);
            sqlx::query(
                "UPDATE match_reviews
                 SET top_score = ?1, top_can_apply = ?2, top_reason = ?3, top_blockers = ?4
                 WHERE local_release_id = ?5 AND provider = ?6 AND top_can_apply IS NULL",
            )
            .bind(summary.score)
            .bind(i64::from(summary.can_apply))
            .bind(summary.reason)
            .bind(summary.blockers)
            .bind(row.0)
            .bind(&row.1)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        done += rows.len() as u64;
        eprintln!("  {} of {}", grouped(done), grouped(total));
    }
    Ok(done)
}

/// One unsummarized review: release, provider, snapshot version, source
/// revision, reasons, whether the candidates are valid JSON, first candidate.
type PendingRow = (i64, String, i64, String, String, bool, Option<String>);

/// The summary of one unsummarized review, given its release if it exists.
fn summary_of_row(release: Option<&LocalRelease>, row: &PendingRow) -> ReviewSummary {
    let (_, provider, version, source_revision, reasons_json, valid, first) = row;
    let reasons: Vec<String> = serde_json::from_str(reasons_json).unwrap_or_default();
    let cannot = |score: Option<f64>, reason: &str, blockers: i64| ReviewSummary {
        score,
        can_apply: false,
        reason: Some(reason.to_string()),
        blockers,
    };
    let Some(provider) = ProviderKind::parse(provider) else {
        return cannot(None, "saved review cannot be read", 0);
    };
    let candidate = match first {
        Some(json) => match decode_stored_candidates(*version, provider, &format!("[{json}]")) {
            Ok(mut decoded) => decoded.pop(),
            Err(_) => return cannot(None, "saved review cannot be read", 0),
        },
        None if *valid => None,
        None => return cannot(None, "saved review cannot be read", 0),
    };
    let candidates: Vec<StoredCandidate> = candidate.into_iter().collect();
    let score = candidates
        .first()
        .map(|first| first.match_scope.score(&first.score));
    let blockers = candidates
        .first()
        .map_or(0, |first| first.blockers.len() as i64);
    match release {
        None => cannot(score, "local release is gone", blockers),
        Some(release) if release.revision != *source_revision => {
            cannot(score, "saved before the files changed", blockers)
        }
        Some(release) => summarize(release, &candidates, &reasons),
    }
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
///
/// Keep the two in step: they must select the same releases.
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
    /// Each queued provider with its top candidate's score, in the order
    /// matching tries providers; `None` when that snapshot cannot be read or
    /// holds no candidate.
    ///
    /// The top candidate is the first in the saved list: after a fetch on the
    /// web, the release entered there, which need not be the best-scoring.
    pub top: Vec<(ProviderKind, Option<f64>)>,
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

    let order = ProviderKind::CHAIN;
    let mut top: HashMap<i64, Vec<(ProviderKind, Option<f64>)>> = HashMap::new();
    for (id, provider, version, first) in firsts {
        let Some(provider) = ProviderKind::parse(&provider) else {
            continue;
        };
        let score = first
            .and_then(|json| decode_stored_candidates(version, provider, &format!("[{json}]")).ok())
            .and_then(|candidates| candidates.into_iter().next())
            .map(|candidate| candidate.match_scope.score(&candidate.score));
        top.entry(id).or_default().push((provider, score));
    }
    for scores in top.values_mut() {
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
                top: top.remove(&id).unwrap_or_default(),
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
    use crate::matcher::MatchScope;
    use crate::providers::{ProviderKind, ReleaseId};
    use crate::review::stored_candidate;
    use crate::test_support::{
        FIXTURE_MBID, OTHER_MBID, local_release, mbid_release, queue, remote_release, seed_release,
    };

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
            page[0].top.iter().map(|(provider, _)| *provider).collect();
        assert_eq!(
            providers,
            vec![ProviderKind::MusicBrainz, ProviderKind::Discogs]
        );
        assert!(page[0].top.iter().all(|(_, score)| score.is_some()));
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
            assert_eq!(page[0].top, vec![(ProviderKind::Deezer, None)], "{json}");
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

    fn stored(
        local: &LocalRelease,
        remote: crate::matcher::RemoteRelease,
        scope: MatchScope,
        blockers: &[&str],
    ) -> StoredCandidate {
        let candidate = Candidate::new(local, remote);
        let mut evaluation = matcher::evaluate(
            local,
            &candidate,
            None,
            &matcher::DiscoveryEvidence::default(),
        );
        evaluation.match_scope = scope;
        evaluation.blockers = blockers.iter().map(|text| (*text).to_string()).collect();
        stored_candidate(&candidate, &evaluation)
    }

    #[test]
    fn a_summary_reads_the_first_candidate_as_the_page_shows_it() {
        let titles = ["One", "Two"];
        let local = local_release(1, "/music/a", "Blue", &titles);
        let exact = remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles);
        let first = stored(
            &local,
            exact,
            MatchScope::Edition,
            &["edition qualifiers differ", "close alternative"],
        );
        let summary = summarize(
            &local,
            std::slice::from_ref(&first),
            &["uncertain".to_string()],
        );
        assert_eq!(summary.score, Some(first.score.total));
        assert!(summary.can_apply);
        assert_eq!(summary.reason.as_deref(), Some("edition qualifiers differ"));
        assert_eq!(summary.blockers, 2);

        // No blockers: the review's first reason explains it.
        let plain = stored(
            &local,
            remote_release(mbid_release(OTHER_MBID), "Blue", &titles),
            MatchScope::Album,
            &[],
        );
        let summary = summarize(
            &local,
            std::slice::from_ref(&plain),
            &["uncertain".to_string()],
        );
        assert_eq!(summary.reason.as_deref(), Some("uncertain"));
        assert_eq!(summary.score, Some(plain.score.album_score()));

        // An incomplete mapping at edition scope cannot be applied.
        let longer = stored(
            &local,
            remote_release(ReleaseId::Discogs(9), "Blue", &["One", "Two", "Three"]),
            MatchScope::Edition,
            &["track mapping is incomplete"],
        );
        assert!(!summarize(&local, std::slice::from_ref(&longer), &[]).can_apply);

        // No candidates at all.
        let empty = summarize(&local, &[], &["nothing found".to_string()]);
        assert_eq!(
            (empty.score, empty.can_apply, empty.blockers),
            (None, false, 0)
        );
        assert_eq!(empty.reason.as_deref(), Some("nothing found"));
    }

    #[test]
    fn the_first_tracks_rule_counts_as_applicable_at_album_scope_only() {
        // Seven files that are the first seven of an eight-track release.
        let files = ["One", "Two", "Three", "Four", "Five", "Six", "Seven"];
        let local = local_release(1, "/music/a", "Blue", &files);
        let listed = [
            "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Bonus",
        ];
        let remote = remote_release(mbid_release(FIXTURE_MBID), "Blue", &listed);
        let album = stored(&local, remote.clone(), MatchScope::Album, &[]);
        let edition = stored(&local, remote, MatchScope::Edition, &[]);
        assert!(summarize(&local, &[album], &[]).can_apply);
        assert!(!summarize(&local, &[edition], &[]).can_apply);
    }

    #[tokio::test]
    async fn every_writer_stores_the_first_candidates_summary() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        let row: (Option<f64>, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT top_score, top_can_apply, top_blockers FROM match_reviews")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(row.0.is_some());
        assert_eq!(row.1, Some(1));

        // A fetched release put first replaces the summary: three provider
        // tracks for two files cannot be applied.
        let longer = remote_release(mbid_release(OTHER_MBID), "Blue", &["One", "Two", "Three"]);
        let candidate = Candidate::new(&release, longer);
        let evaluation = matcher::evaluate(
            &release,
            &candidate,
            None,
            &matcher::DiscoveryEvidence::default(),
        );
        assert!(
            add_review_candidate(
                &pool,
                &release,
                stored_candidate(&candidate, &evaluation),
                "entered by hand"
            )
            .await
            .unwrap()
        );
        let can_apply: Option<i64> = sqlx::query_scalar("SELECT top_can_apply FROM match_reviews")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(can_apply, Some(0));
    }

    #[tokio::test]
    async fn the_one_time_pass_summarizes_every_unsummarized_review() {
        let (pool, [a, b, _, d]) = library().await;
        // As if the reviews were saved before the summary columns existed.
        sqlx::query(
            "UPDATE match_reviews
             SET top_score = NULL, top_can_apply = NULL, top_reason = NULL, top_blockers = NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE match_reviews SET candidates_json = 'nonsense' WHERE local_release_id = ?1",
        )
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE match_reviews SET source_revision = 'old' WHERE local_release_id = ?1")
            .bind(d)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(fill_review_summaries(&pool).await.unwrap(), 4);
        let rows: Vec<(i64, Option<f64>, i64, Option<String>)> = sqlx::query_as(
            "SELECT local_release_id, top_score, top_can_apply, top_reason
             FROM match_reviews ORDER BY local_release_id, provider",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for (id, score, can_apply, reason) in &rows {
            if *id == a {
                assert!(score.is_some() && *can_apply == 1, "{rows:?}");
            } else if *id == b {
                assert_eq!(
                    (score, *can_apply, reason.as_deref()),
                    (&None, 0, Some("saved review cannot be read"))
                );
            } else if *id == d {
                assert!(score.is_some(), "{rows:?}");
                assert_eq!(
                    (*can_apply, reason.as_deref()),
                    (0, Some("saved before the files changed"))
                );
            }
        }
        // Nothing is left for the next open.
        assert_eq!(fill_review_summaries(&pool).await.unwrap(), 0);
    }
}
