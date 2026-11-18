//! The review queue as the web pages read it, and the one write that adds a
//! candidate to a saved review.
//!
//! A release is *queued* when it is not matched and has at least one
//! `match_reviews` row. A [`QueueView`] picks a provider, an order (folder or
//! lead score) and whether only releases with an acceptable top candidate
//! count; [`queue_page`], [`queue_count`] and [`next_queued`] answer from the
//! summary columns of `match_reviews`, never from the candidates' JSON.
//! [`add_review_candidate`] puts a release fetched by its
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
pub(crate) struct ReviewSummary {
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

/// Why a review saved against other file contents cannot be accepted.
const OUT_OF_DATE_REASON: &str = "saved before the files changed";

/// Why a review whose saved candidates cannot be decoded cannot be accepted.
const UNREADABLE_REASON: &str = "saved review cannot be read";

/// Why a review whose local release no longer exists cannot be accepted.
const RELEASE_GONE_REASON: &str = "local release is gone";

/// Summarizes the first of `candidates`, saved for `release`, as the page shows it.
///
/// `can_apply` is [`matcher::mapping_can_apply`], the check the Accept button
/// uses, so the queue's "can be accepted" never disagrees with the page.
pub(crate) fn summarize(
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
/// A saved review made for the release as it is now keeps its other
/// candidates (without an earlier entry for the same release) and its
/// reasons; any other saved review is replaced by `candidate` alone, with
/// `reason`. The review's guard is the release's current revision and
/// accepted match.
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

/// Summarizes every saved review that has no summary yet; returns how many
/// rows it filled.
///
/// Runs after migrations on every open and returns at once when nothing is
/// left (an index-only check). Reviews saved before the summary columns
/// existed are summarized here, once: only each review's first candidate is
/// read, the releases are loaded in chunks of [`SUMMARY_CHUNK`], and progress
/// goes to stderr. A review that cannot be decoded, or that was saved before
/// its files changed, is summarized as one that cannot be accepted, so no row
/// is left for the next open. A row that a concurrent writer summarized first
/// is left as that writer saved it, and is not counted.
///
/// Filling the columns rewrites each review row through the WAL, so the pass
/// truncates the WAL afterwards. That final `wal_checkpoint(TRUNCATE)` may
/// wait up to the busy timeout for other processes, and is otherwise ignored
/// when it reports busy.
///
/// # Errors
///
/// When the database cannot be read or written.
pub(crate) async fn fill_review_summaries(pool: &SqlitePool) -> Result<u64> {
    fill_review_summaries_in(pool, SUMMARY_CHUNK).await
}

/// [`fill_review_summaries`] with `chunk` releases per write transaction.
async fn fill_review_summaries_in(pool: &SqlitePool, chunk: usize) -> Result<u64> {
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
    for ids in ids.chunks(chunk) {
        let releases: HashMap<i64, LocalRelease> = super::load_releases_by_id(pool, ids)
            .await?
            .into_iter()
            .map(|release| (release.id, release))
            .collect();
        let rows: Vec<PendingRow> = sqlx::query_as(
            "SELECT local_release_id AS release_id, provider, snapshot_version, source_revision,
                    reasons_json,
                    CASE WHEN json_valid(candidates_json)
                         THEN json_type(candidates_json) END AS candidates_type,
                    CASE WHEN json_valid(candidates_json)
                         THEN candidates_json -> '$[0]' END AS first_candidate
             FROM match_reviews
             WHERE top_can_apply IS NULL
               AND local_release_id IN (SELECT value FROM json_each(?1))",
        )
        .bind(serde_json::to_string(ids)?)
        .fetch_all(pool)
        .await?;
        let mut tx = begin_write(pool).await?;
        for row in &rows {
            let summary = summary_of_row(releases.get(&row.release_id), row);
            done += sqlx::query(
                "UPDATE match_reviews
                 SET top_score = ?1, top_can_apply = ?2, top_reason = ?3, top_blockers = ?4
                 WHERE local_release_id = ?5 AND provider = ?6 AND top_can_apply IS NULL",
            )
            .bind(summary.score)
            .bind(i64::from(summary.can_apply))
            .bind(summary.reason)
            .bind(summary.blockers)
            .bind(row.release_id)
            .bind(&row.provider)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        eprintln!("  {} of {}", grouped(done), grouped(total));
    }
    // The pragma returns a row (busy flag and page counts), so fetch it; a
    // busy checkpoint only leaves the WAL for the next one. Memory databases
    // have no WAL, and it is a no-op there.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_all(pool)
        .await?;
    Ok(done)
}

/// One unsummarized review, as [`fill_review_summaries`] reads it.
#[derive(Debug, sqlx::FromRow)]
struct PendingRow {
    /// The local release the review belongs to.
    release_id: i64,
    /// The review's provider, as stored.
    provider: String,
    /// The snapshot version the candidates were saved in.
    snapshot_version: i64,
    /// The source revision the review was saved for.
    source_revision: String,
    /// The review's reasons, as JSON.
    reasons_json: String,
    /// The JSON type of the candidates; `None` when they are not valid JSON.
    candidates_type: Option<String>,
    /// The first candidate as JSON text, when there is one.
    first_candidate: Option<String>,
}

/// The summary of one unsummarized review, given its release if it exists.
fn summary_of_row(release: Option<&LocalRelease>, row: &PendingRow) -> ReviewSummary {
    let reasons: Vec<String> = serde_json::from_str(&row.reasons_json).unwrap_or_default();
    let cannot = |score: Option<f64>, reason: &str, blockers: i64| ReviewSummary {
        score,
        can_apply: false,
        reason: Some(reason.to_string()),
        blockers,
    };
    let Some(provider) = ProviderKind::parse(&row.provider) else {
        return cannot(None, UNREADABLE_REASON, 0);
    };
    let version = row.snapshot_version;
    let candidate = match &row.first_candidate {
        Some(json) => match decode_stored_candidates(version, provider, &format!("[{json}]")) {
            Ok(mut decoded) => decoded.pop(),
            Err(_) => return cannot(None, UNREADABLE_REASON, 0),
        },
        // An array with no first element holds no candidate, but an unknown
        // snapshot version still makes the review unreadable.
        None if row.candidates_type.as_deref() == Some("array") => {
            match decode_stored_candidates(version, provider, "[]") {
                Ok(mut decoded) => decoded.pop(),
                Err(_) => return cannot(None, UNREADABLE_REASON, 0),
            }
        }
        None => return cannot(None, UNREADABLE_REASON, 0),
    };
    let candidates: Vec<StoredCandidate> = candidate.into_iter().collect();
    let score = candidates
        .first()
        .map(|first| first.match_scope.score(&first.score));
    match release {
        // The replaced reasons explain the whole review, so no blockers
        // are counted behind them.
        None => cannot(score, RELEASE_GONE_REASON, 0),
        Some(release) if release.revision != row.source_revision => {
            cannot(score, OUT_OF_DATE_REASON, 0)
        }
        Some(release) => summarize(release, &candidates, &reasons),
    }
}

/// How the review queue is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueSort {
    /// Folder order, as the releases listing.
    #[default]
    Folder,
    /// By each release's lead score, highest first; no score last.
    Score,
}

/// Which queued releases a queue page lists, and in what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueView {
    /// Only releases queued for this provider.
    pub provider: Option<ProviderKind>,
    /// The order.
    pub sort: QueueSort,
    /// Only releases with a current review whose top candidate can be accepted.
    pub acceptable: bool,
}

/// The review that places a release in the queue: its best-scoring review
/// among those the view allows.
#[derive(Debug, Clone, PartialEq)]
pub struct Lead {
    /// The review's provider.
    pub provider: ProviderKind,
    /// Its top candidate's score.
    pub score: Option<f64>,
    /// Whether that candidate can be accepted.
    pub can_apply: bool,
    /// Why the release is queued: the candidate's first blocker, else the
    /// review's first reason.
    pub reason: Option<String>,
    /// How many blockers the candidate has.
    pub blockers: i64,
}

/// The condition on review `r` of release `lr` that a view allows: its
/// provider (`?1`, any when `NULL`), and when acceptable only (`?2` is 1), a
/// current review whose top candidate can be accepted. Answered from
/// `idx_match_reviews_summary`.
macro_rules! review_allowed {
    () => {
        "(?1 IS NULL OR r.provider = ?1)
         AND (?2 = 0 OR (r.top_can_apply = 1 AND r.source_revision = lr.source_revision))"
    };
}

/// The condition that puts release `lr` in a view (folder order): not
/// matched, with a review the view allows.
///
/// SQLite 3.51 (bundled through libsqlite3-sys 0.37) rewrites a plain
/// correlated `EXISTS` into a join. Without the `LIMIT 1` that join yields a
/// release once per review row and uses up `OFFSET` on each of those rows, so
/// pages start too early and overlap. The `LIMIT 1` keeps the subquery
/// correlated, so the listing index still serves the order and no page sorts
/// the whole queue.
macro_rules! queued_where {
    () => {
        concat!(
            "lr.status <> 'matched'
             AND EXISTS (SELECT 1 FROM match_reviews r
                         WHERE r.local_release_id = lr.id AND ",
            review_allowed!(),
            " LIMIT 1)"
        )
    };
}

/// Each release in a view with its lead score, for the score order: one
/// `GROUP BY` over the summary index, no `EXISTS`. Releases with no score
/// have a `NULL` score. The optional argument adds a condition, such as
/// `"AND lr.id = ?3"` to read one release's lead alone.
macro_rules! lead_scores {
    () => {
        lead_scores!("")
    };
    ($extra:literal) => {
        concat!(
            "SELECT r.local_release_id AS id, MAX(r.top_score) AS score
             FROM match_reviews r JOIN local_releases lr ON lr.id = r.local_release_id
             WHERE lr.status <> 'matched' AND ",
            review_allowed!(),
            " ",
            $extra,
            " GROUP BY r.local_release_id"
        )
    };
}

/// The score order: lead score, highest first, no score last, then folder order.
macro_rules! score_order {
    () => {
        "ORDER BY lead.score IS NULL, lead.score DESC, lr.directory_path, lr.album_title_norm"
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
    /// matching tries providers; `None` when that snapshot holds no candidate,
    /// cannot be read, or its review has not been summarized yet.
    ///
    /// The top candidate is the first in the saved list: after a fetch on the
    /// web, the release entered there, which need not be the best-scoring.
    pub top: Vec<(ProviderKind, Option<f64>)>,
    /// The lead review. Always `Some` for a listed release.
    pub lead: Option<Lead>,
}

/// A queue row: ID, folder, unit, album title and artist, track count.
type QueueRow = (i64, String, String, Option<String>, Option<String>, i64);

/// One review's summary as the queue page reads it.
#[derive(Debug, Clone, PartialEq)]
struct SummaryRow {
    provider: ProviderKind,
    score: Option<f64>,
    can_apply: bool,
    reason: Option<String>,
    blockers: i64,
    current: bool,
}

/// The lead among one release's reviews under `view`: the highest score
/// among the reviews the view allows, ties by provider order.
fn lead_of(rows: &[SummaryRow], view: &QueueView) -> Option<Lead> {
    rows.iter()
        .filter(|row| {
            view.provider
                .is_none_or(|provider| provider == row.provider)
        })
        .filter(|row| !view.acceptable || (row.can_apply && row.current))
        .min_by(|x, y| {
            let rank =
                |row: &SummaryRow| ProviderKind::CHAIN.iter().position(|p| *p == row.provider);
            // `None` orders below every `Some`, so the highest score comes
            // first and no score last.
            y.score
                .partial_cmp(&x.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| rank(x).cmp(&rank(y)))
        })
        .map(|row| Lead {
            provider: row.provider,
            score: row.score,
            // The page offers no Accept for a review saved before the files changed.
            can_apply: row.can_apply && row.current,
            // The replaced reason explains the whole review; a stale review's
            // blockers are not counted behind it.
            reason: if row.current {
                row.reason.clone()
            } else {
                Some(OUT_OF_DATE_REASON.to_owned())
            },
            blockers: if row.current { row.blockers } else { 0 },
        })
}

/// A review's summary row from the database: release, provider, score,
/// whether it can be accepted, reason, blockers, whether it is current.
type SummaryRecord = (
    i64,
    String,
    Option<f64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    bool,
);

/// One page of the review queue under `view`.
///
/// The folder order is that of the releases listing. The score order puts
/// each release by its lead review's score, highest first and no score last,
/// then by folder. `view.provider` keeps only releases queued for that
/// provider, and `view.acceptable` only those with a current review whose top
/// candidate can be accepted. Scores, reasons and blocker counts come from the
/// summary columns of `match_reviews`, never from the candidates' JSON.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn queue_page(
    pool: &SqlitePool,
    view: &QueueView,
    limit: i64,
    offset: i64,
) -> Result<Vec<QueuedRelease>> {
    let provider = view.provider.map(ProviderKind::as_str);
    let acceptable = i64::from(view.acceptable);
    let mut tx = pool.begin().await?;
    let sql = match view.sort {
        QueueSort::Folder => concat!(
            "SELECT lr.id, lr.directory_path, lr.unit_path, lr.album_title, lr.album_artist,
                    lr.track_count
             FROM local_releases lr WHERE ",
            queued_where!(),
            " ORDER BY lr.directory_path, lr.album_title_norm LIMIT ?3 OFFSET ?4"
        ),
        QueueSort::Score => concat!(
            "WITH lead AS (",
            lead_scores!(),
            ")
             SELECT lr.id, lr.directory_path, lr.unit_path, lr.album_title, lr.album_artist,
                    lr.track_count
             FROM lead JOIN local_releases lr ON lr.id = lead.id ",
            score_order!(),
            " LIMIT ?3 OFFSET ?4"
        ),
    };
    let rows: Vec<QueueRow> = sqlx::query_as(sql)
        .bind(provider)
        .bind(acceptable)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
    let ids = serde_json::to_string(&rows.iter().map(|row| row.0).collect::<Vec<_>>())?;
    let summaries: Vec<SummaryRecord> = sqlx::query_as(
        "SELECT r.local_release_id, r.provider, r.top_score, r.top_can_apply, r.top_reason,
                r.top_blockers, COALESCE(r.source_revision = lr.source_revision, 0)
         FROM match_reviews r JOIN local_releases lr ON lr.id = r.local_release_id
         WHERE r.local_release_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut by_release: HashMap<i64, Vec<SummaryRow>> = HashMap::new();
    for (id, provider, score, can_apply, reason, blockers, current) in summaries {
        let Some(provider) = ProviderKind::parse(&provider) else {
            continue;
        };
        by_release.entry(id).or_default().push(SummaryRow {
            provider,
            score,
            can_apply: can_apply == Some(1),
            reason,
            blockers: blockers.unwrap_or(0),
            current,
        });
    }
    for reviews in by_release.values_mut() {
        reviews.sort_by_key(|row| {
            ProviderKind::CHAIN
                .iter()
                .position(|ordered| *ordered == row.provider)
        });
    }
    Ok(rows
        .into_iter()
        .map(
            |(id, directory, unit, album_title, album_artist, track_count)| {
                let reviews = by_release.remove(&id).unwrap_or_default();
                QueuedRelease {
                    id,
                    directory,
                    unit,
                    album_title,
                    album_artist,
                    track_count,
                    top: reviews
                        .iter()
                        .map(|row| (row.provider, row.score))
                        .collect(),
                    lead: lead_of(&reviews, view),
                }
            },
        )
        .collect())
}

/// How many releases `view` lists.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn queue_count(pool: &SqlitePool, view: &QueueView) -> Result<i64> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM local_releases lr WHERE ",
        queued_where!()
    ))
    .bind(view.provider.map(ProviderKind::as_str))
    .bind(i64::from(view.acceptable))
    .fetch_one(pool)
    .await?)
}

/// Where a release sat in the score-sorted queue when the user found it.
///
/// Entering a release by ID rewrites its review summary and so moves it; a
/// place keeps "next" counted from where the user came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueuePlace {
    /// The lead score the release had there; `None` is the block of releases
    /// with no score.
    pub score: Option<f64>,
}

/// The release that follows another in a view, with its place in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueNext {
    /// The release's ID.
    pub id: i64,
    /// Where it stands in the score order, its lead score under the view;
    /// `None` in folder order, where a place means nothing.
    pub place: Option<QueuePlace>,
}

/// Where release `id` stands in `view`'s score order now: its lead score.
///
/// `None` when the release is not in the view (or is gone). The folder order
/// has no places, so it answers `None` too.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn queue_place(
    pool: &SqlitePool,
    id: i64,
    view: &QueueView,
) -> Result<Option<QueuePlace>> {
    if view.sort != QueueSort::Score {
        return Ok(None);
    }
    let score: Option<(Option<f64>,)> = sqlx::query_as(concat!(
        "SELECT lead.score FROM (",
        lead_scores!("AND lr.id = ?3"),
        ") lead"
    ))
    .bind(view.provider.map(ProviderKind::as_str))
    .bind(i64::from(view.acceptable))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(score.map(|(score,)| QueuePlace { score }))
}

/// The first release after release `after` in `view`'s order.
///
/// `after` need not be in the view. In folder order, the next release after
/// its folder position is returned, and when it no longer exists the queue
/// starts again from the top. In score order, a release outside the view (or
/// one that is gone) also starts again from the top; otherwise the next is the
/// first below its lead score, ties in folder order.
///
/// `at` pins the score order's starting point: with `Some`, `after` is taken
/// to sit at that lead score, with its own folder position, wherever its
/// reviews put it now. A release that is gone still starts again from the top.
/// Folder order ignores `at`. The release is never its own next, even when
/// its reviews now put it below `at`.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn next_queued(
    pool: &SqlitePool,
    after: i64,
    view: &QueueView,
    at: Option<QueuePlace>,
) -> Result<Option<QueueNext>> {
    let provider = view.provider.map(ProviderKind::as_str);
    let acceptable = i64::from(view.acceptable);
    let next = match view.sort {
        QueueSort::Folder => {
            let position: Option<(String, String)> = sqlx::query_as(
                "SELECT directory_path, album_title_norm FROM local_releases WHERE id = ?1",
            )
            .bind(after)
            .fetch_optional(pool)
            .await?;
            let id: Option<i64> = match position {
                // Directory paths compare without case, as the listing orders
                // them; (directory, title) is unique, so the order is total.
                Some((directory, title)) => {
                    sqlx::query_scalar(concat!(
                        "SELECT lr.id FROM local_releases lr WHERE ",
                        queued_where!(),
                        " AND (lr.directory_path, lr.album_title_norm) > (?3, ?4)
                         ORDER BY lr.directory_path, lr.album_title_norm LIMIT 1"
                    ))
                    .bind(provider)
                    .bind(acceptable)
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
                    .bind(provider)
                    .bind(acceptable)
                    .fetch_optional(pool)
                    .await?
                }
            };
            id.map(|id| QueueNext { id, place: None })
        }
        QueueSort::Score => {
            // `after`'s place in the view: the pinned one, else its lead score,
            // and its folder key. No place means it is not in the view (or is
            // gone), and no folder key that it is gone.
            let folder: Option<(String, String)> = sqlx::query_as(
                "SELECT directory_path, album_title_norm FROM local_releases WHERE id = ?1",
            )
            .bind(after)
            .fetch_optional(pool)
            .await?;
            let place = match (&folder, at) {
                (None, _) => None,
                (Some(_), Some(place)) => Some(place),
                (Some(_), None) => queue_place(pool, after, view).await?,
            };
            let position = folder
                .zip(place)
                .map(|((directory, title), place)| (place.score, directory, title));
            match position {
                // After `after`: no score sorts last, so a scored release is
                // followed by every release with no score, by lower scores,
                // and by equal scores later in folder order.
                Some((score, directory, title)) => {
                    sqlx::query_as(concat!(
                        "WITH lead AS (",
                        lead_scores!(),
                        ")
                         SELECT lr.id, lead.score
                         FROM lead JOIN local_releases lr ON lr.id = lead.id
                         WHERE lr.id <> ?6
                           AND ((lead.score IS NULL) > (?3 IS NULL)
                             OR (lead.score IS ?3
                                 AND (lr.directory_path, lr.album_title_norm) > (?4, ?5))
                             OR lead.score < ?3) ",
                        score_order!(),
                        " LIMIT 1"
                    ))
                    .bind(provider)
                    .bind(acceptable)
                    .bind(score)
                    .bind(directory)
                    .bind(title)
                    .bind(after)
                    .fetch_optional(pool)
                    .await?
                }
                None => {
                    sqlx::query_as(concat!(
                        "WITH lead AS (",
                        lead_scores!(),
                        ")
                         SELECT lr.id, lead.score
                         FROM lead JOIN local_releases lr ON lr.id = lead.id ",
                        score_order!(),
                        " LIMIT 1"
                    ))
                    .bind(provider)
                    .bind(acceptable)
                    .fetch_optional(pool)
                    .await?
                }
            }
            .map(|(id, score)| QueueNext {
                id,
                place: Some(QueuePlace { score }),
            })
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
        let page = queue_page(&pool, &folder(None), 10, 0).await.unwrap();
        assert_eq!(ids(&page), vec![a, b, mixed.id, d]);
        assert_eq!(next_id(&pool, b, &folder(None)).await, Some(mixed.id));
        assert_eq!(next_id(&pool, mixed.id, &folder(None)).await, Some(d));
        assert_eq!(next_id(&pool, c, &folder(None)).await, Some(d));
    }

    /// The release after `after`, as `next_queued` finds it from where `after` sits now.
    async fn next_id(pool: &SqlitePool, after: i64, view: &QueueView) -> Option<i64> {
        next_queued(pool, after, view, None)
            .await
            .unwrap()
            .map(|next| next.id)
    }

    fn view(provider: Option<ProviderKind>, sort: QueueSort, acceptable: bool) -> QueueView {
        QueueView {
            provider,
            sort,
            acceptable,
        }
    }

    fn folder(provider: Option<ProviderKind>) -> QueueView {
        view(provider, QueueSort::Folder, false)
    }

    /// Sets one review's summary directly, so tests control the order.
    async fn set_summary(
        pool: &SqlitePool,
        id: i64,
        provider: &str,
        score: Option<f64>,
        can_apply: bool,
    ) {
        sqlx::query(
            "UPDATE match_reviews SET top_score = ?1, top_can_apply = ?2, top_reason = 'why', top_blockers = 2
             WHERE local_release_id = ?3 AND provider = ?4",
        )
        .bind(score)
        .bind(i64::from(can_apply))
        .bind(id)
        .bind(provider)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn score_order_follows_each_releases_lead_review() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.70), true).await;
        set_summary(&pool, a, "discogs", Some(0.99), false).await;
        set_summary(&pool, b, "musicbrainz", Some(0.95), true).await;
        set_summary(&pool, d, "deezer", None, false).await;

        let all = queue_page(&pool, &view(None, QueueSort::Score, false), 10, 0)
            .await
            .unwrap();
        assert_eq!(
            ids(&all),
            vec![a, b, d],
            "a leads with discogs 0.99; no score comes last"
        );
        let lead = all[0].lead.as_ref().unwrap();
        assert_eq!(
            (
                lead.provider,
                lead.can_apply,
                lead.reason.as_deref(),
                lead.blockers
            ),
            (ProviderKind::Discogs, false, Some("why"), 2)
        );
        assert_eq!(all[0].top.len(), 2);

        let acceptable = queue_page(&pool, &view(None, QueueSort::Score, true), 10, 0)
            .await
            .unwrap();
        assert_eq!(
            ids(&acceptable),
            vec![b, a],
            "only applicable reviews count"
        );
        assert_eq!(
            acceptable[1].lead.as_ref().unwrap().provider,
            ProviderKind::MusicBrainz
        );
        assert_eq!(
            queue_count(&pool, &view(None, QueueSort::Score, true))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            queue_count(&pool, &view(None, QueueSort::Folder, false))
                .await
                .unwrap(),
            3
        );

        // Out-of-date reviews never count as acceptable.
        sqlx::query("UPDATE match_reviews SET source_revision = 'old' WHERE local_release_id = ?1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        let acceptable = queue_page(&pool, &view(None, QueueSort::Score, true), 10, 0)
            .await
            .unwrap();
        assert_eq!(ids(&acceptable), vec![a]);
        // In the unfiltered view the stale lead says so, as the page offers no Accept.
        let all = queue_page(&pool, &view(None, QueueSort::Score, false), 10, 0)
            .await
            .unwrap();
        let stale = all.iter().find(|row| row.id == b).unwrap();
        let lead = stale.lead.as_ref().unwrap();
        assert!(!lead.can_apply);
        assert_eq!(lead.reason.as_deref(), Some(OUT_OF_DATE_REASON));
        assert_eq!(lead.blockers, 0, "set_summary gave the row two blockers");

        // The provider filter combines with the others.
        let musicbrainz = queue_page(
            &pool,
            &view(Some(ProviderKind::MusicBrainz), QueueSort::Score, false),
            10,
            0,
        )
        .await
        .unwrap();
        assert_eq!(ids(&musicbrainz), vec![b, a]);
        // Folder order with the acceptable filter.
        let folder_order = queue_page(&pool, &view(None, QueueSort::Folder, true), 10, 0)
            .await
            .unwrap();
        assert_eq!(ids(&folder_order), vec![a]);
        // Provider and acceptable together: a's Discogs review cannot be
        // accepted, its MusicBrainz one can.
        for sort in [QueueSort::Folder, QueueSort::Score] {
            let discogs = view(Some(ProviderKind::Discogs), sort, true);
            assert!(queue_page(&pool, &discogs, 10, 0).await.unwrap().is_empty());
            assert_eq!(queue_count(&pool, &discogs).await.unwrap(), 0);
            let musicbrainz = view(Some(ProviderKind::MusicBrainz), sort, true);
            let page = queue_page(&pool, &musicbrainz, 10, 0).await.unwrap();
            assert_eq!(ids(&page), vec![a]);
            assert_eq!(page[0].lead.as_ref().unwrap().score, Some(0.70));
        }
        assert_eq!(
            queue_count(&pool, &view(None, QueueSort::Folder, true))
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn score_pages_do_not_overlap() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.9), true).await;
        set_summary(&pool, a, "discogs", Some(0.8), true).await;
        set_summary(&pool, b, "musicbrainz", Some(0.7), true).await;
        set_summary(&pool, d, "deezer", Some(0.6), true).await;
        let score = view(None, QueueSort::Score, false);
        let mut seen = Vec::new();
        for offset in 0..3 {
            seen.extend(ids(&queue_page(&pool, &score, 1, offset).await.unwrap()));
        }
        assert_eq!(seen, vec![a, b, d]);
    }

    #[tokio::test]
    async fn next_in_score_order_walks_down_the_view() {
        let (pool, [a, b, c, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.90), true).await;
        set_summary(&pool, a, "discogs", Some(0.80), true).await;
        set_summary(&pool, b, "musicbrainz", Some(0.95), true).await;
        set_summary(&pool, d, "deezer", Some(0.90), false).await;
        let score = view(None, QueueSort::Score, false);
        // b 0.95, then a and d tie at 0.90 (folder order), then nothing.
        assert_eq!(next_id(&pool, b, &score).await, Some(a));
        assert_eq!(next_id(&pool, a, &score).await, Some(d));
        assert_eq!(next_id(&pool, d, &score).await, None);
        // A release outside the view (c has no review) starts from the top.
        assert_eq!(next_id(&pool, c, &score).await, Some(b));
        // With the acceptable filter, d drops out and a is last.
        let acceptable = view(None, QueueSort::Score, true);
        assert_eq!(next_id(&pool, a, &acceptable).await, None);
        // A release that just left the queue (its reviews deleted) restarts at the top.
        sqlx::query("DELETE FROM match_reviews WHERE local_release_id = ?1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(next_id(&pool, b, &score).await, Some(a));
    }

    #[tokio::test]
    async fn next_in_score_order_places_releases_without_a_score_last() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", None, false).await;
        set_summary(&pool, a, "discogs", None, false).await;
        set_summary(&pool, b, "musicbrainz", Some(0.5), true).await;
        set_summary(&pool, d, "deezer", None, false).await;
        let score = view(None, QueueSort::Score, false);
        // b scores; then a and d have no score and follow in folder order.
        assert_eq!(next_id(&pool, b, &score).await, Some(a));
        assert_eq!(next_id(&pool, a, &score).await, Some(d));
        assert_eq!(next_id(&pool, d, &score).await, None);
    }

    #[tokio::test]
    async fn equal_scores_keep_folder_order_whatever_the_ids() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        // /music/B2 has the higher id but sorts first without regard to case.
        let c = seed_release(&pool, "/music/c", "Green", &titles).await;
        let b2 = seed_release(&pool, "/music/B2", "Pink", &titles).await;
        for release in [&c, &b2] {
            queue(
                &pool,
                release,
                remote_release(mbid_release(FIXTURE_MBID), "x", &titles),
            )
            .await;
            set_summary(&pool, release.id, "musicbrainz", Some(0.8), true).await;
        }
        assert!(b2.id > c.id);
        let score = view(None, QueueSort::Score, false);
        let page = queue_page(&pool, &score, 10, 0).await.unwrap();
        assert_eq!(ids(&page), vec![b2.id, c.id]);
        assert_eq!(next_id(&pool, b2.id, &score).await, Some(c.id));
        assert_eq!(next_id(&pool, c.id, &score).await, None);
    }

    /// The release after `after` when it is taken to sit at `score`.
    async fn next_at(
        pool: &SqlitePool,
        after: i64,
        view: &QueueView,
        score: Option<f64>,
    ) -> Option<QueueNext> {
        next_queued(pool, after, view, Some(QueuePlace { score }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn next_from_a_pinned_place_ignores_where_the_release_sits_now() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, b, "musicbrainz", Some(0.9), true).await;
        set_summary(&pool, a, "musicbrainz", Some(0.8), true).await;
        set_summary(&pool, a, "discogs", Some(0.7), true).await;
        set_summary(&pool, d, "deezer", Some(0.7), true).await;
        let score = view(None, QueueSort::Score, false);
        // A fetch rewrites a's summary: it now leads the queue.
        sqlx::query("UPDATE match_reviews SET top_score = 0.95 WHERE local_release_id = ?1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        // From where it sits now, a is followed by b; from where it was found, by d.
        assert_eq!(next_id(&pool, a, &score).await, Some(b));
        assert_eq!(
            next_at(&pool, a, &score, Some(0.8)).await,
            Some(QueueNext {
                id: d,
                place: Some(QueuePlace { score: Some(0.7) })
            })
        );
        // Without a pin the answer is the same as from `None`, with its score.
        assert_eq!(
            next_queued(&pool, a, &score, None).await.unwrap(),
            Some(QueueNext {
                id: b,
                place: Some(QueuePlace { score: Some(0.9) })
            })
        );
        // A pin works for a release the view no longer holds, too.
        sqlx::query("DELETE FROM match_reviews WHERE local_release_id = ?1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            next_at(&pool, a, &score, Some(0.8)).await.map(|n| n.id),
            Some(d)
        );
        // A release that is gone starts again from the top.
        assert_eq!(
            next_at(&pool, d + 100, &score, Some(0.8))
                .await
                .map(|n| n.id),
            Some(b)
        );
    }

    #[tokio::test]
    async fn next_from_the_no_score_place_continues_in_that_block() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, b, "musicbrainz", Some(0.5), true).await;
        set_summary(&pool, a, "musicbrainz", None, false).await;
        set_summary(&pool, a, "discogs", None, false).await;
        set_summary(&pool, d, "deezer", None, false).await;
        let score = view(None, QueueSort::Score, false);
        // b was found with no score (a fetch has since given it one): the
        // releases with no score after it in folder order still follow it,
        // and nothing above it comes back.
        assert_eq!(
            next_at(&pool, b, &score, None).await,
            Some(QueueNext {
                id: d,
                place: Some(QueuePlace { score: None })
            })
        );
        assert_eq!(next_at(&pool, a, &score, None).await.map(|n| n.id), Some(d));
        assert_eq!(next_at(&pool, d, &score, None).await, None);
    }

    #[tokio::test]
    async fn a_release_moved_below_its_pin_is_not_its_own_next() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, b, "musicbrainz", Some(0.9), true).await;
        set_summary(&pool, a, "musicbrainz", Some(0.8), true).await;
        set_summary(&pool, a, "discogs", Some(0.8), true).await;
        set_summary(&pool, d, "deezer", Some(0.6), true).await;
        let score = view(None, QueueSort::Score, false);
        // A fetch moved a down to 0.7, still above d but below where it was found.
        sqlx::query("UPDATE match_reviews SET top_score = 0.7 WHERE local_release_id = ?1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        // Found at 0.8, a is followed by d; it is met again on the way down.
        assert_eq!(
            next_at(&pool, a, &score, Some(0.8)).await.map(|n| n.id),
            Some(d)
        );
        // Pinned level with its new score, it is followed by d as well.
        assert_eq!(
            next_at(&pool, a, &score, Some(0.7)).await.map(|n| n.id),
            Some(d)
        );
        // From where it sits now, unpinned, the same.
        assert_eq!(next_id(&pool, a, &score).await, Some(d));
    }

    #[tokio::test]
    async fn a_pinned_tie_continues_in_folder_order() {
        let (pool, [a, b, _, d]) = library().await;
        for (id, provider) in [
            (a, "musicbrainz"),
            (a, "discogs"),
            (b, "musicbrainz"),
            (d, "deezer"),
        ] {
            set_summary(&pool, id, provider, Some(0.8), true).await;
        }
        let score = view(None, QueueSort::Score, false);
        // b was found at 0.8 and has since moved up: the tie continues with d,
        // and a, earlier in folder order, is not brought back.
        sqlx::query("UPDATE match_reviews SET top_score = 0.95 WHERE local_release_id = ?1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            next_at(&pool, b, &score, Some(0.8)).await.map(|n| n.id),
            Some(d)
        );
        // a, moved down, is not its own next either; the tie goes on with b.
        sqlx::query("UPDATE match_reviews SET top_score = 0.7 WHERE local_release_id = ?1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE match_reviews SET top_score = 0.8 WHERE local_release_id = ?1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            next_at(&pool, a, &score, Some(0.8)).await.map(|n| n.id),
            Some(b)
        );
    }

    #[tokio::test]
    async fn the_place_is_the_lead_score_in_score_order_only() {
        let (pool, [a, b, c, _]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.6), true).await;
        set_summary(&pool, a, "discogs", Some(0.8), true).await;
        set_summary(&pool, b, "musicbrainz", None, false).await;
        let score = view(None, QueueSort::Score, false);
        let place = |score| Some(QueuePlace { score });
        assert_eq!(
            queue_place(&pool, a, &score).await.unwrap(),
            place(Some(0.8))
        );
        assert_eq!(queue_place(&pool, b, &score).await.unwrap(), place(None));
        // The provider filter changes the lead; a release outside the view has none.
        let musicbrainz = view(Some(ProviderKind::MusicBrainz), QueueSort::Score, false);
        assert_eq!(
            queue_place(&pool, a, &musicbrainz).await.unwrap(),
            place(Some(0.6))
        );
        assert_eq!(queue_place(&pool, c, &score).await.unwrap(), None);
        assert_eq!(queue_place(&pool, a + 100, &score).await.unwrap(), None);
        assert_eq!(queue_place(&pool, a, &folder(None)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn folder_order_ignores_a_pinned_place() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.1), true).await;
        let folder_view = folder(None);
        assert_eq!(
            next_at(&pool, a, &folder_view, Some(0.1)).await,
            Some(QueueNext { id: b, place: None })
        );
        for score in [None, Some(0.1), Some(0.99)] {
            assert_eq!(
                next_at(&pool, a, &folder_view, score).await.map(|n| n.id),
                Some(b)
            );
            assert_eq!(
                next_at(&pool, b, &folder_view, score).await.map(|n| n.id),
                Some(d)
            );
        }
    }

    #[tokio::test]
    async fn folder_order_next_honors_the_acceptable_filter() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.7), true).await;
        set_summary(&pool, b, "musicbrainz", Some(0.9), false).await;
        set_summary(&pool, d, "deezer", Some(0.5), true).await;
        let acceptable = view(None, QueueSort::Folder, true);
        // b cannot be accepted, so a is followed by d.
        assert_eq!(next_id(&pool, a, &acceptable).await, Some(d));
        assert_eq!(next_id(&pool, d, &acceptable).await, None);
    }

    #[tokio::test]
    async fn score_order_next_honors_the_provider_filter() {
        let (pool, [a, b, _, d]) = library().await;
        set_summary(&pool, a, "musicbrainz", Some(0.60), true).await;
        set_summary(&pool, a, "discogs", Some(0.99), true).await;
        set_summary(&pool, b, "musicbrainz", Some(0.80), true).await;
        set_summary(&pool, d, "deezer", Some(0.95), true).await;
        let musicbrainz = view(Some(ProviderKind::MusicBrainz), QueueSort::Score, false);
        // Only the MusicBrainz reviews count: b 0.80, then a 0.60.
        assert_eq!(next_id(&pool, b, &musicbrainz).await, Some(a));
        assert_eq!(next_id(&pool, a, &musicbrainz).await, None);
        // d has no MusicBrainz review, so it is outside the view.
        assert_eq!(next_id(&pool, d, &musicbrainz).await, Some(b));
    }

    #[tokio::test]
    async fn a_release_sits_at_its_best_allowed_review() {
        let (pool, [a, b, c, d]) = library().await;
        set_summary(&pool, a, "discogs", Some(0.99), false).await;
        set_summary(&pool, a, "musicbrainz", Some(0.70), true).await;
        set_summary(&pool, b, "musicbrainz", Some(0.80), true).await;
        set_summary(&pool, d, "deezer", Some(0.60), true).await;
        let acceptable = view(None, QueueSort::Score, true);
        // a sits at 0.70, not at its unacceptable 0.99: b, a, d.
        let page = queue_page(&pool, &acceptable, 10, 0).await.unwrap();
        assert_eq!(ids(&page), vec![b, a, d]);
        assert_eq!(page[1].lead.as_ref().unwrap().score, Some(0.70));
        assert_eq!(next_id(&pool, b, &acceptable).await, Some(a));
        assert_eq!(next_id(&pool, a, &acceptable).await, Some(d));

        // A release with reviews but none the view allows restarts at the top.
        set_summary(&pool, d, "deezer", Some(0.60), false).await;
        assert_eq!(next_id(&pool, d, &acceptable).await, Some(b));
        // So does one with no reviews at all.
        assert_eq!(next_id(&pool, c, &acceptable).await, Some(b));
    }

    #[test]
    fn the_lead_is_the_best_allowed_review() {
        let row = |provider, score, can_apply, current| SummaryRow {
            provider,
            score,
            can_apply,
            reason: None,
            blockers: 0,
            current,
        };
        let rows = [
            row(ProviderKind::MusicBrainz, Some(0.8), true, false),
            row(ProviderKind::Discogs, Some(0.8), true, true),
            row(ProviderKind::Deezer, None, true, true),
        ];
        let any = QueueView::default();
        // A tie goes to the earlier provider; a missing score loses to any.
        assert_eq!(
            lead_of(&rows, &any).unwrap().provider,
            ProviderKind::MusicBrainz
        );
        let acceptable = QueueView {
            acceptable: true,
            ..any
        };
        assert_eq!(
            lead_of(&rows, &acceptable).unwrap().provider,
            ProviderKind::Discogs
        );
        let deezer = QueueView {
            provider: Some(ProviderKind::Deezer),
            ..any
        };
        assert_eq!(lead_of(&rows, &deezer).unwrap().score, None);
        // The provider filter alone leaves no review for a provider with none.
        let none = QueueView {
            provider: Some(ProviderKind::Discogs),
            ..any
        };
        assert!(lead_of(&rows[..1], &none).is_none());
        // Acceptable alone leaves none when the only review is out of date.
        assert!(lead_of(&rows[..1], &acceptable).is_none());
        // A lead that is out of date cannot be accepted, whatever its summary says.
        let stale = lead_of(&rows[..1], &any).unwrap();
        assert!(!stale.can_apply);
        assert_eq!(stale.reason.as_deref(), Some(OUT_OF_DATE_REASON));
    }

    fn ids(rows: &[QueuedRelease]) -> Vec<i64> {
        rows.iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn the_queue_lists_unmatched_releases_with_reviews_in_folder_order() {
        let (pool, [a, b, _, d]) = library().await;
        let page = queue_page(&pool, &folder(None), 10, 0).await.unwrap();
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

        assert_eq!(
            ids(&queue_page(&pool, &folder(None), 1, 1).await.unwrap()),
            vec![b]
        );
        assert_eq!(
            ids(
                &queue_page(&pool, &folder(Some(ProviderKind::Deezer)), 10, 0)
                    .await
                    .unwrap()
            ),
            vec![d]
        );
        assert_eq!(queue_count(&pool, &folder(None)).await.unwrap(), 3);
        assert_eq!(
            queue_count(&pool, &folder(Some(ProviderKind::MusicBrainz)))
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
            ids(&queue_page(&pool, &folder(None), 10, 0).await.unwrap()),
            vec![a, d]
        );

        // The page reads the summary columns, not the snapshot: a corrupt
        // snapshot changes nothing, and a summary without a score reads as none.
        let before = queue_page(&pool, &folder(Some(ProviderKind::Deezer)), 10, 0)
            .await
            .unwrap();
        assert!(before[0].top[0].1.is_some());
        for json in ["[]", "[1]", "nonsense"] {
            sqlx::query("UPDATE match_reviews SET candidates_json = ?1 WHERE provider = 'deezer'")
                .bind(json)
                .execute(&pool)
                .await
                .unwrap();
            let page = queue_page(&pool, &folder(Some(ProviderKind::Deezer)), 10, 0)
                .await
                .unwrap();
            assert_eq!(page, before, "{json}");
        }
        set_summary(&pool, d, "deezer", None, false).await;
        let page = queue_page(&pool, &folder(Some(ProviderKind::Deezer)), 10, 0)
            .await
            .unwrap();
        assert_eq!(page[0].top, vec![(ProviderKind::Deezer, None)]);
    }

    #[tokio::test]
    async fn next_in_queue_follows_folder_order_and_the_filter() {
        let (pool, [a, b, c, d]) = library().await;
        assert_eq!(next_id(&pool, a, &folder(None)).await, Some(b));
        assert_eq!(next_id(&pool, b, &folder(None)).await, Some(d));
        assert_eq!(next_id(&pool, d, &folder(None)).await, None);
        // From a release that is not queued, the next queued one after it.
        assert_eq!(next_id(&pool, c, &folder(None)).await, Some(d));
        assert_eq!(
            next_id(&pool, a, &folder(Some(ProviderKind::Deezer))).await,
            Some(d)
        );
        // A release that is gone starts the queue again from the top.
        assert_eq!(next_id(&pool, d + 100, &folder(None)).await, Some(a));
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
    fn a_summary_is_of_the_first_candidate_even_when_a_later_one_scores_higher() {
        let titles = ["One", "Two"];
        let local = local_release(1, "/music/a", "Blue", &titles);
        let weak = stored(
            &local,
            remote_release(mbid_release(OTHER_MBID), "Red", &["Uno", "Dos"]),
            MatchScope::Album,
            &["weak"],
        );
        let strong = stored(
            &local,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
            MatchScope::Album,
            &[],
        );
        let weak_score = weak.match_scope.score(&weak.score);
        let strong_score = strong.match_scope.score(&strong.score);
        assert!(weak_score < strong_score, "{weak_score} {strong_score}");
        let summary = summarize(&local, &[weak, strong], &["uncertain".to_string()]);
        assert_eq!(summary.score, Some(weak_score));
        assert_eq!(summary.reason.as_deref(), Some("weak"));
        assert_eq!(summary.blockers, 1);
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
        let first_score = row.0;

        // A fetched release put first replaces the summary: three provider
        // tracks for two files cannot be applied.
        let longer = remote_release(mbid_release(OTHER_MBID), "Orange", &["One", "Two", "Three"]);
        let candidate = Candidate::new(&release, longer);
        let evaluation = matcher::evaluate(
            &release,
            &candidate,
            None,
            &matcher::DiscoveryEvidence::default(),
        );
        let fetched = stored_candidate(&candidate, &evaluation);
        assert!(
            add_review_candidate(&pool, &release, fetched.clone(), "entered by hand")
                .await
                .unwrap()
        );
        let row: (Option<f64>, Option<i64>, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT top_score, top_can_apply, top_reason, top_blockers FROM match_reviews",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let expected = fetched.match_scope.score(&fetched.score);
        assert_eq!(row.0, Some(expected));
        assert_ne!(row.0, first_score, "the score is the fetched candidate's");
        assert_eq!(row.1, Some(0));
        // The fetched candidate's first blocker, else the kept review's reason.
        assert_eq!(
            row.2.as_deref(),
            Some(fetched.blockers.first().map_or("uncertain", String::as_str))
        );
        assert_eq!(row.3, Some(fetched.blockers.len() as i64));
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
                    (&None, 0, Some(UNREADABLE_REASON))
                );
            } else if *id == d {
                assert!(score.is_some(), "{rows:?}");
                assert_eq!(
                    (*can_apply, reason.as_deref()),
                    (0, Some(OUT_OF_DATE_REASON))
                );
            }
        }
        // Nothing is left for the next open.
        assert_eq!(fill_review_summaries(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_stale_review_counts_no_blockers_behind_its_replaced_reason() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One", "Two", "Three"]).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]),
        )
        .await;
        let blockers = |pool: SqlitePool| async move {
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(
                "SELECT top_reason, top_blockers FROM match_reviews",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let (_, current) = blockers(pool.clone()).await;
        assert!(current.unwrap() > 0, "the fixture has a blocker");

        sqlx::query("UPDATE match_reviews SET source_revision = 'old'")
            .execute(&pool)
            .await
            .unwrap();
        forget_summaries(&pool).await;
        fill_review_summaries(&pool).await.unwrap();
        assert_eq!(
            blockers(pool).await,
            (Some(OUT_OF_DATE_REASON.to_string()), Some(0))
        );
    }

    /// Reviews saved before the summaries: every `top_*` column empty.
    async fn forget_summaries(pool: &SqlitePool) {
        sqlx::query(
            "UPDATE match_reviews
             SET top_score = NULL, top_can_apply = NULL, top_reason = NULL, top_blockers = NULL",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn unsummarized(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM match_reviews WHERE top_can_apply IS NULL")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_resumed_pass_leaves_summarized_rows_alone() {
        let (pool, [a, ..]) = library().await;
        forget_summaries(&pool).await;
        sqlx::query(
            "UPDATE match_reviews
             SET top_score = 0.5, top_can_apply = 1, top_reason = 'kept', top_blockers = 0
             WHERE local_release_id = ?1 AND provider = 'musicbrainz'",
        )
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();

        // Four reviews, one already summarized: only three rows are filled.
        assert_eq!(fill_review_summaries(&pool).await.unwrap(), 3);
        assert_eq!(unsummarized(&pool).await, 0);
        let kept: (Option<f64>, Option<String>) = sqlx::query_as(
            "SELECT top_score, top_reason FROM match_reviews
             WHERE local_release_id = ?1 AND provider = 'musicbrainz'",
        )
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, (Some(0.5), Some("kept".to_string())));
    }

    /// Every review's summary columns, in a fixed order.
    async fn summary_rows(
        pool: &SqlitePool,
    ) -> Vec<(
        i64,
        String,
        Option<f64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    )> {
        sqlx::query_as(
            "SELECT local_release_id, provider, top_score, top_can_apply, top_reason, top_blockers
             FROM match_reviews ORDER BY local_release_id, provider",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_pass_summarizes_the_same_over_several_chunks() {
        let (whole, [a, ..]) = library().await;
        forget_summaries(&whole).await;
        assert_eq!(fill_review_summaries(&whole).await.unwrap(), 4);
        let expected = summary_rows(&whole).await;

        // The same library, three releases with reviews, one release per chunk.
        let (chunked, _) = library().await;
        forget_summaries(&chunked).await;
        assert_eq!(fill_review_summaries_in(&chunked, 1).await.unwrap(), 4);
        let rows = summary_rows(&chunked).await;
        assert_eq!(rows, expected);

        // Each chunk read its own releases: a's reviews can be applied.
        assert!(rows.iter().any(|row| row.0 == a && row.3 == Some(1)));
        assert!(
            rows.iter()
                .all(|row| row.4.as_deref() != Some(RELEASE_GONE_REASON))
        );
    }

    #[tokio::test]
    async fn reopening_a_file_database_summarizes_reviews_and_truncates_the_wal() {
        let dir = crate::test_support::ScratchDir::new("summary-reopen");
        let path = dir.path().join("library.db");
        let pool = crate::db::connect(&path).await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        forget_summaries(&pool).await;
        pool.close().await;

        let pool = crate::db::connect(&path).await.unwrap();
        assert_eq!(unsummarized(&pool).await, 0);
        let summarized: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM match_reviews WHERE top_can_apply = 1 AND top_score IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(summarized, 1);
        // Nothing writes after the pass and no reader is open, so the
        // truncating checkpoint leaves the WAL empty (or removes it).
        let wal = std::fs::metadata(path.with_extension("db-wal")).map_or(0, |meta| meta.len());
        assert_eq!(wal, 0, "the WAL holds {wal} bytes");
        pool.close().await;
    }

    #[tokio::test]
    async fn the_one_time_pass_reads_only_an_array_of_candidates() {
        let (pool, [a, b, _, d]) = library().await;
        sqlx::query("UPDATE match_reviews SET top_score = NULL, top_can_apply = NULL")
            .execute(&pool)
            .await
            .unwrap();
        // Valid JSON that is not an array, an empty array, and an empty array
        // under a snapshot version nothing decodes.
        for (id, json, version) in [(a, "{}", 2), (b, "[]", 2), (d, "[]", 99)] {
            sqlx::query(
                "UPDATE match_reviews SET candidates_json = ?2, snapshot_version = ?3
                 WHERE local_release_id = ?1 AND provider = (
                    SELECT provider FROM match_reviews WHERE local_release_id = ?1
                    ORDER BY provider LIMIT 1)",
            )
            .bind(id)
            .bind(json)
            .bind(version)
            .execute(&pool)
            .await
            .unwrap();
        }
        fill_review_summaries(&pool).await.unwrap();
        let reasons: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT local_release_id, top_reason FROM match_reviews
             WHERE local_release_id IN (?1, ?2, ?3) AND top_can_apply = 0
             ORDER BY local_release_id, provider",
        )
        .bind(a)
        .bind(b)
        .bind(d)
        .fetch_all(&pool)
        .await
        .unwrap();
        let unreadable = Some(UNREADABLE_REASON.to_string());
        let reason_of = |id: i64| {
            reasons
                .iter()
                .find(|(row, _)| *row == id)
                .map(|(_, reason)| reason.clone())
        };
        assert_eq!(reason_of(a), Some(unreadable.clone()), "{reasons:?}");
        assert_eq!(reason_of(d), Some(unreadable), "{reasons:?}");
        // An empty array is a review with no candidate: its first reason.
        assert_eq!(
            reason_of(b),
            Some(Some("uncertain".to_string())),
            "{reasons:?}"
        );
    }
}
