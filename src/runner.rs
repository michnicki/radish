//! CLI orchestration: select local releases, run the provider policy, decide,
//! review, and persist.
//!
//! MusicBrainz is attempted first. Discogs is created lazily, only after an
//! unsatisfactory MusicBrainz stage and only when a token is available. An
//! explicit provider or direct selector uses one service and no fallback.

use crate::clef::{Advice, AdviceChoice, ClefClient, ReviewCandidate, choice_key};
use crate::discogs::{AdapterError, DiscogsAdapter};
use crate::discovery::{self, DiscoveryResult};
use crate::matcher::{
    self, Candidate, DiscoveryEvidence, Evaluation, LocalRelease, MatchScope, RemoteRelease,
};
use crate::musicbrainz::MusicBrainzAdapter;
use crate::output::Clean;
use crate::providers::{MetadataProvider, ProviderError, ProviderKind, ProviderPolicy, ReleaseId};
use crate::store::{self, AcceptanceMethod, MatchGuard, StoredCandidate};
use crate::{out, outln};
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

const REVIEW_CANDIDATE_LIMIT: usize = 5;

#[derive(Debug, Clone, Default)]
pub struct MatchOptions {
    pub scope: Option<PathBuf>,
    pub local_release: Option<i64>,
    pub direct_release: Option<ReleaseId>,
    pub review: bool,
    pub dry_run: bool,
    pub ai_review: bool,
    pub policy: ProviderPolicy,
}

#[derive(Debug, Default)]
pub struct MatchSummary {
    pub accepted: usize,
    pub queued: usize,
    pub unmatched: usize,
    pub skipped: usize,
    pub failed: usize,
    pub dry_run: bool,
}

impl MatchSummary {
    pub fn has_failures(&self) -> bool {
        self.failed > 0
    }

    pub fn print_report(&self) {
        if self.dry_run {
            outln!("\nDry run: no matches were stored and no overlays were applied.");
        }
        outln!(
            "\nAccepted: {}\nQueued for review: {}\nUnmatched: {}\nSkipped: {}\nFailed: {}",
            self.accepted,
            self.queued,
            self.unmatched,
            self.skipped,
            self.failed
        );
    }
}

/// The run's single AI client. It is built on first use so a run that never
/// needs AI never reports missing credentials, and a failure that would repeat
/// for every release turns AI off once instead of being retried and re-reported.
enum AiState {
    Unbuilt,
    Ready(ClefClient),
    Disabled,
}

impl AiState {
    /// The client, or `None` once the run has turned AI off. Building can fail
    /// (for example, missing credentials); that is reported once.
    fn client(&mut self) -> Option<&ClefClient> {
        if matches!(self, Self::Unbuilt) {
            *self = match ClefClient::from_env() {
                Ok(client) => Self::Ready(client),
                Err(error) => {
                    outln!(
                        "  AI review disabled for this run: {error}; continuing without AI matching"
                    );
                    Self::Disabled
                }
            };
        }
        match self {
            Self::Ready(client) => Some(client),
            Self::Unbuilt | Self::Disabled => None,
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Accepted,
    Queued,
    Unmatched,
    Skipped,
    Quit,
    Failed,
}

#[derive(Debug)]
enum ReviewOutcome {
    Accept(ProviderKind, Box<Candidate>, Box<Evaluation>),
    Skip,
    Quit,
}

/// Resolves the CLI selection into a provider policy. A direct release selector
/// implies its provider; a contradictory explicit provider is rejected before
/// any database or network work.
pub fn resolve_policy(
    explicit: Option<ProviderKind>,
    direct: Option<&ReleaseId>,
) -> Result<ProviderPolicy> {
    match (explicit, direct) {
        (None, None) => Ok(ProviderPolicy::MusicBrainzThenDiscogs),
        (None, Some(id)) => Ok(ProviderPolicy::Only(id.provider())),
        (Some(provider), None) => Ok(ProviderPolicy::Only(provider)),
        (Some(provider), Some(id)) if provider == id.provider() => {
            Ok(ProviderPolicy::Only(provider))
        }
        (Some(provider), Some(id)) => bail!(
            "--provider {provider} conflicts with a {} release selector",
            id.provider()
        ),
    }
}

/// One provider's discovery and evaluation, with its own evidence and errors.
#[derive(Debug)]
pub struct ProviderAttempt {
    pub provider: ProviderKind,
    pub candidates: Vec<Candidate>,
    pub evaluations: Vec<Evaluation>,
    pub notes: Vec<String>,
    pub error: Option<ProviderError>,
    pub embedded_release_resolved: bool,
    /// Shared model assessment, subject to separate conservative album gates.
    pub ai_advice: Option<Advice>,
}

impl ProviderAttempt {
    fn failed(provider: ProviderKind, error: ProviderError) -> Self {
        Self {
            provider,
            candidates: Vec::new(),
            evaluations: Vec::new(),
            notes: Vec::new(),
            error: Some(error),
            embedded_release_resolved: false,
            ai_advice: None,
        }
    }

    fn from_discovery(
        provider: ProviderKind,
        local: &LocalRelease,
        result: DiscoveryResult,
        error: Option<ProviderError>,
        direct: bool,
    ) -> Self {
        let embedded_release_resolved = result
            .evidence
            .values()
            .next()
            .map(|evidence| evidence.embedded_release_resolved)
            .unwrap_or(false);

        let mut candidates: Vec<Candidate> = result
            .releases
            .into_iter()
            .map(|release| Candidate::new(local, release))
            .collect();
        // Strong edition evidence wins before comparing album confidence.
        let exact_id = candidates
            .iter()
            .filter(|candidate| {
                let runner_up = candidates
                    .iter()
                    .filter(|other| other.release.id != candidate.release.id)
                    .max_by(|a, b| a.score.total.total_cmp(&b.score.total));
                let evidence = result
                    .evidence
                    .get(&candidate.release.id)
                    .cloned()
                    .unwrap_or_default();
                matcher::evaluate(local, candidate, runner_up, &evidence).auto_accept
            })
            .max_by(|a, b| {
                a.score
                    .total
                    .total_cmp(&b.score.total)
                    .then_with(|| b.release.id.cmp(&a.release.id))
            })
            .map(|candidate| candidate.release.id.clone());
        candidates.sort_by(|a, b| {
            (exact_id.as_ref() == Some(&b.release.id))
                .cmp(&(exact_id.as_ref() == Some(&a.release.id)))
                .then_with(|| b.score.album_score().total_cmp(&a.score.album_score()))
                .then_with(|| b.score.total.total_cmp(&a.score.total))
                .then_with(|| a.release.id.cmp(&b.release.id))
        });

        let evaluations = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let evidence = result
                    .evidence
                    .get(&candidate.release.id)
                    .cloned()
                    .unwrap_or_default();
                let runner_up = if index == 0 { candidates.get(1) } else { None };
                if direct {
                    matcher::evaluate(local, candidate, runner_up, &evidence)
                } else {
                    matcher::evaluate_for_matching(local, candidate, &candidates, &evidence)
                }
            })
            .collect();

        Self {
            provider,
            candidates,
            evaluations,
            notes: result.notes,
            error,
            embedded_release_resolved,
            ai_advice: None,
        }
    }

    fn settled(&self, has_direct: bool) -> bool {
        !has_direct
            && self
                .evaluations
                .first()
                .is_some_and(|evaluation| evaluation.auto_accept)
    }
}

pub async fn attempt_provider<P: MetadataProvider>(
    provider: &P,
    local: &LocalRelease,
    direct: Option<ReleaseId>,
) -> ProviderAttempt {
    let kind = provider.kind();
    let is_direct = direct.is_some();
    match discovery::discover(provider, local, direct).await {
        Ok(result) => ProviderAttempt::from_discovery(kind, local, result, None, is_direct),
        Err(error) => ProviderAttempt::failed(kind, error),
    }
}

fn provider_error_from(provider: ProviderKind, error: anyhow::Error) -> ProviderError {
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderError>())
    {
        return error.clone();
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AdapterError>())
    {
        return ProviderError::from(error.clone());
    }
    ProviderError::Configuration {
        provider,
        message: format!("{error:#}"),
    }
}

fn is_missing_discogs_token(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AdapterError>(),
            Some(AdapterError::MissingToken)
        )
    })
}

pub async fn run_match(pool: &SqlitePool, options: MatchOptions) -> Result<MatchSummary> {
    run_with_policy(pool, &options, MusicBrainzAdapter::new, || {
        DiscogsAdapter::from_env().map_err(anyhow::Error::new)
    })
    .await
}

pub async fn run_with_policy<M, D>(
    pool: &SqlitePool,
    options: &MatchOptions,
    make_musicbrainz: impl Fn() -> Result<M>,
    make_discogs: impl Fn() -> Result<D>,
) -> Result<MatchSummary>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if options.review && !interactive {
        bail!("--review requires an interactive terminal");
    }
    if options.direct_release.is_some() && options.local_release.is_none() {
        bail!("a direct release selector requires --local-release");
    }

    // Grouping is derived identity, not a matching result, so it is refreshed
    // even for a dry run. Matching results and overlays are never written.
    store::rebuild_local_releases(pool, options.scope.as_deref()).await?;

    let statuses = store::status_by_id(pool).await?;
    let existing_matches = store::accepted_matches_by_release(pool).await?;
    let releases =
        store::load_selected_releases(pool, options.scope.as_deref(), options.local_release)
            .await?;

    if let Some(id) = options.local_release
        && releases.is_empty()
    {
        bail!("local release {id} was not found");
    }

    let mut summary = MatchSummary {
        dry_run: options.dry_run,
        ..MatchSummary::default()
    };

    let mut selected: Vec<LocalRelease> = Vec::new();
    for release in releases {
        let status = statuses
            .get(&release.id)
            .map(String::as_str)
            .unwrap_or(store::STATUS_UNMATCHED);
        let accepted = existing_matches.get(&release.id).and_then(|p| *p);

        if !options.review {
            match status {
                store::STATUS_UNMATCHED => {}
                store::STATUS_STALE => {
                    if let ProviderPolicy::Only(provider) = options.policy
                        && accepted.is_some_and(|accepted| accepted != provider)
                    {
                        outln!(
                            "\nSkipping local release {}: its stale match belongs to {}, not the explicitly selected {}",
                            release.id,
                            accepted.unwrap(),
                            provider
                        );
                        summary.skipped += 1;
                        continue;
                    }
                }
                _ => {
                    summary.skipped += 1;
                    continue;
                }
            }
        }

        selected.push(release);
    }

    if selected.is_empty() {
        outln!("Nothing to match.");
        return Ok(summary);
    }

    let mut musicbrainz_client: Option<M> = None;
    let mut discogs_client: Option<D> = None;
    let mut discogs_disabled: Option<ProviderError> = None;
    let mut ai = AiState::Unbuilt;

    for release in &selected {
        let label = format!(
            "{} — {}",
            release
                .album_artist
                .as_deref()
                .unwrap_or("(unknown artist)"),
            release.album_title.as_deref().unwrap_or("(no album tag)")
        );
        outln!(
            "\nMatching local release {} ({})",
            release.id,
            Clean(&label)
        );

        // A stale match is refreshed from the provider that accepted it; a
        // different source requires deliberate review.
        let mut release_policy = options.policy;
        if !options.review
            && statuses.get(&release.id).map(String::as_str) == Some(store::STATUS_STALE)
            && let Some(provider) = existing_matches.get(&release.id).and_then(|p| *p)
            && release_policy == ProviderPolicy::MusicBrainzThenDiscogs
        {
            outln!("  refreshing the stale {provider} match from its original source");
            release_policy = ProviderPolicy::Only(provider);
        }

        let guard = match store::load_match_guard(pool, release.id).await {
            Ok(guard) => guard,
            Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
                outln!(
                    "  skipping local release {}: it no longer exists; run match again",
                    release.id
                );
                summary.skipped += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        // The release was loaded before provider calls began. If its files
        // changed since, any candidate evaluated against the old snapshot
        // would map onto the wrong files, so do not contact a provider.
        if guard.source_revision != release.revision {
            outln!(
                "  skipping local release {}: its files changed since matching started; run match again",
                release.id
            );
            summary.skipped += 1;
            continue;
        }

        // Explicit review stays offline. Normal matching rediscovers candidates
        // so an old edition-only review does not permanently block automation.
        if options.review
            && let Some(saved) = load_saved_reviews(pool, release, &guard, release_policy).await?
        {
            match review_saved_release(pool, release, saved, options.dry_run, &guard).await {
                Ok(Outcome::Accepted) => summary.accepted += 1,
                Ok(Outcome::Queued) => summary.queued += 1,
                Ok(Outcome::Skipped) => summary.skipped += 1,
                Ok(Outcome::Quit) => return Ok(summary),
                Ok(_) => summary.failed += 1,
                Err(error) => {
                    eprintln!(
                        "failed to review local release {}: {}",
                        release.id,
                        Clean(&format!("{error:#}"))
                    );
                    summary.failed += 1;
                }
            }
            continue;
        }

        let outcome = process_release(
            pool,
            release,
            options,
            release_policy,
            &mut musicbrainz_client,
            &mut discogs_client,
            &mut discogs_disabled,
            &mut ai,
            release_policy != options.policy,
            &make_musicbrainz,
            &make_discogs,
            &guard,
        )
        .await;

        match outcome {
            Ok(Outcome::Accepted) => summary.accepted += 1,
            Ok(Outcome::Queued) => summary.queued += 1,
            Ok(Outcome::Unmatched) => summary.unmatched += 1,
            Ok(Outcome::Skipped) => summary.skipped += 1,
            Ok(Outcome::Failed) => summary.failed += 1,
            Ok(Outcome::Quit) => return Ok(summary),
            Err(error) => {
                summary.failed += 1;
                eprintln!(
                    "failed to match local release {}: {}",
                    release.id,
                    Clean(&format!("{error:#}"))
                );
            }
        }
    }

    Ok(summary)
}

/// A primary stage that failed for a reason that says nothing about the
/// release means a fallback candidate was never compared against what the
/// primary would have found, so it must be reviewed rather than accepted.
fn primary_failed_transiently(attempts: &[ProviderAttempt]) -> bool {
    attempts
        .first()
        .and_then(|attempt| attempt.error.as_ref())
        .is_some_and(ProviderError::is_transient)
}

async fn load_saved_reviews(
    pool: &SqlitePool,
    release: &LocalRelease,
    guard: &MatchGuard,
    policy: ProviderPolicy,
) -> Result<Option<Vec<store::StoredReview>>> {
    let mut saved = Vec::new();
    for provider in policy.order() {
        if let Some(review) = store::load_review(pool, release.id, provider).await?
            && review.guard == *guard
        {
            saved.push(review);
        }
    }
    Ok((!saved.is_empty()).then_some(saved))
}

#[allow(clippy::too_many_arguments)]
async fn process_release<M, D>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    policy: ProviderPolicy,
    musicbrainz_client: &mut Option<M>,
    discogs_client: &mut Option<D>,
    discogs_disabled: &mut Option<ProviderError>,
    ai: &mut AiState,
    refreshing: bool,
    make_musicbrainz: impl Fn() -> Result<M>,
    make_discogs: impl Fn() -> Result<D>,
    guard: &MatchGuard,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    let direct = options
        .direct_release
        .clone()
        .filter(|id| policy.allows(id.provider()));
    let mut attempts: Vec<ProviderAttempt> = Vec::new();
    let mut fallback_skip: Option<String> = None;

    let order = policy.order();
    for (index, provider) in order.iter().copied().enumerate() {
        if index > 0 {
            let primary = &attempts[0];
            if primary.settled(direct.is_some()) {
                break;
            }
            let reason = if let Some(error) = &primary.error {
                format!("primary stage failed: {error}")
            } else if primary.candidates.is_empty() {
                "primary stage found no candidates".to_string()
            } else {
                "primary candidates did not settle the edition".to_string()
            };
            outln!("  Discogs fallback: {}", Clean(&reason));
        }

        match provider {
            ProviderKind::MusicBrainz => {
                if musicbrainz_client.is_none() {
                    match make_musicbrainz() {
                        Ok(client) => *musicbrainz_client = Some(client),
                        Err(error) => {
                            attempts.push(ProviderAttempt::failed(
                                ProviderKind::MusicBrainz,
                                provider_error_from(ProviderKind::MusicBrainz, error),
                            ));
                            continue;
                        }
                    }
                }
                let client = musicbrainz_client.as_ref().expect("client initialized");
                attempts.push(attempt_provider(client, release, direct.clone()).await);
            }
            ProviderKind::Discogs => {
                if let Some(error) = discogs_disabled.as_ref() {
                    if policy.is_explicit() && !refreshing {
                        // The user asked for Discogs, so a release it cannot
                        // serve is a failure, whichever release met the error.
                        attempts.push(ProviderAttempt::failed(
                            ProviderKind::Discogs,
                            error.clone(),
                        ));
                    } else {
                        fallback_skip = Some(format!(
                            "Discogs is disabled for the rest of this run: {error}"
                        ));
                    }
                    break;
                }
                if discogs_client.is_none() {
                    match make_discogs() {
                        Ok(client) => *discogs_client = Some(client),
                        Err(error) => {
                            let missing = is_missing_discogs_token(&error);
                            if refreshing && missing {
                                // The release is not at fault and nothing was
                                // tried, so this is neither a failure nor a miss.
                                outln!(
                                    "  stale Discogs match needs RADISH_DISCOGS_TOKEN to refresh"
                                );
                                return Ok(Outcome::Skipped);
                            }
                            if policy.is_explicit() {
                                attempts.push(ProviderAttempt::failed(
                                    ProviderKind::Discogs,
                                    provider_error_from(ProviderKind::Discogs, error),
                                ));
                            } else if missing {
                                fallback_skip = Some(
                                    "Discogs token is not set; optional fallback was skipped"
                                        .to_string(),
                                );
                            } else {
                                let error = provider_error_from(ProviderKind::Discogs, error);
                                fallback_skip =
                                    Some(format!("Discogs fallback was skipped: {error}"));
                                *discogs_disabled = Some(error);
                                eprintln!("  Disabling Discogs fallback for the rest of this run");
                            }
                            break;
                        }
                    }
                }
                let client = discogs_client.as_ref().expect("client initialized");
                let attempt = attempt_provider(client, release, direct.clone()).await;
                // A rejected credential rejects every later release too.
                if attempt.error.as_ref().is_some_and(ProviderError::is_auth) {
                    *discogs_disabled = attempt.error.clone();
                    eprintln!("  Disabling Discogs for the rest of this run");
                }
                attempts.push(attempt);
            }
        }
    }

    for attempt in &attempts {
        for note in &attempt.notes {
            eprintln!("  note ({}): {}", attempt.provider, Clean(note));
        }
        if let Some(error) = &attempt.error {
            eprintln!(
                "  {} stage error: {}",
                attempt.provider,
                Clean(&error.to_string())
            );
        }
    }
    if let Some(reason) = &fallback_skip {
        outln!("  {}", Clean(reason));
    }

    // A settled primary result stops discovery and never touches Discogs.
    if let Some(primary) = attempts.first()
        && primary.settled(direct.is_some())
    {
        let candidate = &primary.candidates[0];
        let evaluation = &primary.evaluations[0];

        if options.review {
            return review_attempts(
                pool,
                release,
                &attempts,
                guard,
                options.dry_run,
                musicbrainz_client.as_ref(),
                discogs_client.as_ref(),
            )
            .await;
        }

        return accept(
            pool,
            release,
            primary.provider,
            candidate,
            evaluation,
            AcceptanceMethod::Automatic,
            options.dry_run,
            guard,
            None,
        )
        .await;
    }

    // A fallback candidate is accepted only when it settles its own gates and
    // does not contradict the local edition or a plausible primary candidate.
    if attempts.len() > 1
        && let Some(fallback) = attempts.get(1)
        && let (Some(candidate), Some(evaluation)) =
            (fallback.candidates.first(), fallback.evaluations.first())
        && evaluation.auto_accept
        && !primary_failed_transiently(&attempts)
    {
        let primary = &attempts[0];
        let conflicts = matcher::fallback_conflicts(
            release,
            &primary.candidates,
            candidate,
            primary.embedded_release_resolved,
        );
        if conflicts.is_empty() && direct.is_none() && !options.review {
            if let Some(error) = &primary.error {
                outln!(
                    "  recovered the release from a failed MusicBrainz stage: {}",
                    Clean(&error.to_string())
                );
            }
            return accept(
                pool,
                release,
                fallback.provider,
                candidate,
                evaluation,
                AcceptanceMethod::Automatic,
                options.dry_run,
                guard,
                None,
            )
            .await;
        }
        for conflict in &conflicts {
            outln!("  fallback conflict: {}", Clean(conflict));
        }
        attempts[1].notes.extend(conflicts);
    }

    let has_candidates = attempts
        .iter()
        .any(|attempt| !attempt.candidates.is_empty());
    let fallback_failed = attempts
        .get(1)
        .is_some_and(|attempt| attempt.error.is_some());

    if has_candidates {
        if options.ai_review {
            add_ai_advice(pool, release, guard, &mut attempts, ai).await;
        }
        if !options.review
            && direct.is_none()
            && attempts.iter().any(|attempt| attempt.ai_advice.is_some())
        {
            match choose_ai_match(release, &attempts) {
                Ok((candidate, evaluation, advice)) => {
                    return accept(
                        pool,
                        release,
                        candidate.release.provider(),
                        &candidate,
                        &evaluation,
                        AcceptanceMethod::Automatic,
                        options.dry_run,
                        guard,
                        Some(&advice),
                    )
                    .await;
                }
                Err(reason) => {
                    let note = format!("AI match not applied: {reason}");
                    outln!("  {}", Clean(&note));
                    for attempt in &mut attempts {
                        if !attempt.candidates.is_empty() {
                            attempt.notes.push(note.clone());
                        }
                    }
                }
            }
        }
        if options.dry_run {
            for attempt in &attempts {
                print_decision(attempt);
            }
            return Ok(Outcome::Queued);
        }
        if options.review {
            // Keep paid advisory evidence even when the user skips or quits.
            // Accepting a candidate clears these queued snapshots atomically.
            if attempts.iter().any(|attempt| attempt.ai_advice.is_some()) {
                queue_attempts(pool, release, &attempts, guard).await?;
            }
            return review_attempts(
                pool,
                release,
                &attempts,
                guard,
                options.dry_run,
                musicbrainz_client.as_ref(),
                discogs_client.as_ref(),
            )
            .await;
        }
        queue_attempts(pool, release, &attempts, guard).await?;
        return Ok(Outcome::Queued);
    }

    // No usable candidates. A completed-empty stage means no match; an
    // unrecovered attempted-stage failure must not be disguised as one.
    if let Some(error) = attempts.iter().find_map(|attempt| attempt.error.clone()) {
        eprintln!("  {}", Clean(&error.to_string()));
        if fallback_failed {
            eprintln!("  the fallback stage also failed; the release outcome is a failure");
        }
        return Ok(Outcome::Failed);
    }

    // Nothing was attempted (the reason was printed above), so there is no
    // search result to call a miss.
    let Some(first) = attempts.first() else {
        return Ok(Outcome::Skipped);
    };
    outln!("  no {} candidates found", first.provider);
    Ok(Outcome::Unmatched)
}

async fn add_ai_advice(
    pool: &SqlitePool,
    release: &LocalRelease,
    guard: &MatchGuard,
    attempts: &mut [ProviderAttempt],
    ai: &mut AiState,
) {
    let Some(client) = ai.client() else {
        return;
    };
    let candidates: Vec<ReviewCandidate<'_>> = attempts
        .iter()
        .flat_map(|attempt| {
            attempt
                .candidates
                .iter()
                .zip(&attempt.evaluations)
                .take(REVIEW_CANDIDATE_LIMIT)
                .map(|(candidate, evaluation)| ReviewCandidate {
                    candidate,
                    evaluation,
                })
        })
        .collect();

    // Paid advice is reused while everything it was based on is unchanged.
    let saved = match saved_advice(pool, release, guard, attempts, client, &candidates).await {
        Ok(saved) => saved,
        Err(_) => {
            // A damaged saved review only costs a new request.
            outln!("  saved AI advice could not be read; asking again");
            None
        }
    };
    let result = match saved {
        Some(advice) => {
            outln!("  reusing saved AI advice; no new request was made");
            Ok(advice)
        }
        None => client.advise(release, &candidates).await,
    };
    match result {
        Ok(advice) => {
            print_ai_advice(&advice);
            let assessed = candidates
                .iter()
                .filter(|item| advice_assessed(&advice, item.candidate))
                .count();
            if assessed < candidates.len() {
                outln!(
                    "  AI assessed {assessed} of {} candidates with complete tracklists",
                    candidates.len()
                );
            }
            for attempt in attempts {
                if !attempt.candidates.is_empty() {
                    attempt.ai_advice = Some(advice.clone());
                }
            }
        }
        Err(error) if error.disables_run() => {
            outln!("  AI review disabled for this run: {error}; continuing without AI matching");
            *ai = AiState::Disabled;
        }
        Err(error) => outln!("  AI review unavailable: {error}; continuing without AI matching"),
    }
}

/// Advice stored with a queued review, if it still answers the question that
/// would be asked now: unchanged files and edition, same candidates, same
/// model and prompt.
async fn saved_advice(
    pool: &SqlitePool,
    release: &LocalRelease,
    guard: &MatchGuard,
    attempts: &[ProviderAttempt],
    client: &ClefClient,
    candidates: &[ReviewCandidate<'_>],
) -> Result<Option<Advice>> {
    let Ok(digest) = client.request_digest(release, candidates) else {
        return Ok(None);
    };
    for attempt in attempts {
        let Some(saved) = store::load_review(pool, release.id, attempt.provider).await? else {
            continue;
        };
        if saved.guard != *guard {
            continue;
        }
        let advice = saved
            .candidates
            .into_iter()
            .filter_map(|candidate| candidate.ai_advice)
            .find(|advice| advice.answers(client.model(), &digest));
        if advice.is_some() {
            return Ok(advice);
        }
    }
    Ok(None)
}

fn print_ai_advice(advice: &Advice) {
    outln!("  AI advice: {}", advice.summary());
}

fn advice_assessed(advice: &Advice, candidate: &Candidate) -> bool {
    advice
        .probabilities
        .contains_key(&choice_key(&candidate.release.id))
}

fn choose_ai_match(
    release: &LocalRelease,
    attempts: &[ProviderAttempt],
) -> Result<(Candidate, Evaluation, Advice), String> {
    let advice = attempts
        .iter()
        .find_map(|attempt| attempt.ai_advice.as_ref())
        .ok_or_else(|| "no model assessment is available".to_string())?;
    let primary_provider = attempts.first().map(|attempt| attempt.provider);
    if matches!(advice.choice, AdviceChoice::NoMatch) {
        return Err("the model found no supported match".into());
    }
    if !advice.can_automate() {
        return Err("model confidence or selected probability does not meet the automatic acceptance policy".into());
    }
    // The client may narrow its request to complete candidates that fit. Only
    // actual returned option keys establish which evidence the model assessed.
    let offered: Vec<&Candidate> = attempts
        .iter()
        .flat_map(|attempt| attempt.candidates.iter())
        .filter(|candidate| advice_assessed(advice, candidate))
        .collect();
    let candidate = match &advice.choice {
        AdviceChoice::Candidate { release: id } => *offered
            .iter()
            .find(|candidate| candidate.release.id == *id)
            .ok_or_else(|| "the selected release was not assessed".to_string())?,
        AdviceChoice::EquivalentEditions => {
            let candidate = *offered
                .first()
                .ok_or_else(|| "no candidates were assessed".to_string())?;
            if offered.len() < 2 {
                return Err("equivalent editions requires at least two assessed candidates".into());
            }
            if !offered
                .iter()
                .all(|other| matcher::compatible_album_audio(candidate, other))
            {
                return Err("the assessed editions do not establish equivalent audio".into());
            }
            candidate
        }
        AdviceChoice::NoMatch => unreachable!(),
    };
    if primary_failed_transiently(attempts)
        && Some(candidate.release.provider()) != primary_provider
    {
        return Err(
            "MusicBrainz stage failed transiently; fallback candidates need review".to_string(),
        );
    }
    // Include candidates outside the model budget when checking ambiguity.
    let alternatives: Vec<Candidate> = attempts
        .iter()
        .flat_map(|attempt| attempt.candidates.iter().cloned())
        .collect();
    let evaluation = matcher::evaluate_ai_album(release, candidate, &alternatives);
    if !evaluation.auto_accept {
        return Err(evaluation.blockers.join("; "));
    }
    // The model cannot supply semantic agreement for evidence it never saw.
    if let Some(other) = alternatives.iter().find(|other| {
        !advice_assessed(advice, other)
            && matcher::within_gap(candidate.score.album_score() - other.score.album_score())
            && !matcher::equivalent_unassessed_audio(candidate, other)
    }) {
        return Err(format!(
            "unassessed close alternative {} does not establish the same album and recordings",
            other.release.id
        ));
    }
    if matches!(advice.choice, AdviceChoice::EquivalentEditions) {
        for other in &offered {
            let other_evaluation = matcher::evaluate_ai_album(release, other, &alternatives);
            if !other_evaluation.auto_accept {
                return Err(format!(
                    "equivalent edition {}: {}",
                    other.release.id,
                    other_evaluation.blockers.join("; ")
                ));
            }
        }
    }
    if candidate.release.provider() == ProviderKind::Discogs
        && let Some(primary) = attempts
            .iter()
            .find(|attempt| attempt.provider == ProviderKind::MusicBrainz)
    {
        let conflicts = matcher::fallback_conflicts(
            release,
            &primary.candidates,
            candidate,
            primary.embedded_release_resolved,
        );
        if !conflicts.is_empty() {
            return Err(conflicts.join("; "));
        }
    }
    Ok((candidate.clone(), evaluation, advice.clone()))
}

#[allow(clippy::too_many_arguments)]
async fn accept(
    pool: &SqlitePool,
    release: &LocalRelease,
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
    method: AcceptanceMethod,
    dry_run: bool,
    guard: &MatchGuard,
    ai_advice: Option<&Advice>,
) -> Result<Outcome> {
    if !evaluation.can_apply(candidate) {
        outln!(
            "  cannot accept {provider} release {}: the track mapping is invalid or incomplete ({})",
            candidate.release.id,
            Clean(&candidate.mapping.notes.join("; "))
        );
        return Ok(Outcome::Skipped);
    }

    if dry_run {
        print_decision_for(provider, candidate, evaluation);
        outln!(
            "  would accept {provider} release {} (score {:.3}) as {}",
            candidate.release.id,
            evaluation.match_scope.score(&candidate.score),
            method.as_str()
        );
        return Ok(Outcome::Accepted);
    }

    let result = if let Some(advice) = ai_advice {
        store::accept_ai_match(pool, release, candidate, guard, advice).await
    } else {
        store::accept_match(
            pool,
            release,
            candidate,
            method,
            evaluation.match_scope,
            guard,
        )
        .await
    };
    match result {
        Ok(()) => {
            outln!(
                "  accepted {provider} release {} (score {:.3}, {})",
                candidate.release.id,
                evaluation.match_scope.score(&candidate.score),
                method.as_str()
            );
            outln!("    {} {}", provider, candidate.release.id.storage_id());
            outln!("    {}", Clean(&candidate.release.url));
            if ai_advice.is_some() {
                outln!("    AI-supported album match; decision evidence saved");
            }
            if evaluation.match_scope == MatchScope::Album {
                outln!("    album and tracks matched; exact edition unconfirmed");
            }
            Ok(Outcome::Accepted)
        }
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
            outln!("  skipping: {error}; run match again");
            Ok(Outcome::Skipped)
        }
        Err(error) => {
            eprintln!(
                "  could not apply the match: {}",
                Clean(&format!("{error:#}"))
            );
            Ok(Outcome::Failed)
        }
    }
}

fn stored_candidates(attempt: &ProviderAttempt) -> Vec<StoredCandidate> {
    attempt
        .candidates
        .iter()
        .zip(attempt.evaluations.iter())
        .take(REVIEW_CANDIDATE_LIMIT)
        .enumerate()
        .map(|(index, (candidate, evaluation))| StoredCandidate {
            release: candidate.release.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: evaluation.auto_accept,
            match_scope: evaluation.match_scope,
            blockers: evaluation.blockers.clone(),
            warnings: evaluation.warnings.clone(),
            evidence: evaluation.evidence.clone(),
            ai_advice: (index == 0).then(|| attempt.ai_advice.clone()).flatten(),
        })
        .collect()
}

fn review_reason_summary(attempts: &[ProviderAttempt]) -> String {
    let mut reasons = Vec::new();
    for attempt in attempts {
        let blockers = attempt
            .evaluations
            .first()
            .into_iter()
            .flat_map(|evaluation| &evaluation.blockers);
        for reason in blockers.chain(&attempt.notes) {
            let reason = format!("{}: {reason}", attempt.provider);
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
        if let Some(error) = &attempt.error {
            reasons.push(error.to_string());
        }
    }
    if reasons.is_empty() {
        "candidate selection requires review".into()
    } else {
        reasons.join("; ")
    }
}

async fn queue_attempts(
    pool: &SqlitePool,
    release: &LocalRelease,
    attempts: &[ProviderAttempt],
    guard: &MatchGuard,
) -> Result<()> {
    for attempt in attempts {
        if attempt.candidates.is_empty() {
            continue;
        }
        let stored = stored_candidates(attempt);
        let mut reasons = attempt.evaluations[0].blockers.clone();
        reasons.extend(attempt.notes.iter().cloned());
        if let Some(error) = &attempt.error {
            reasons.push(format!("provider stage error: {error}"));
        }

        store::queue_review(
            pool,
            release.id,
            attempt.provider,
            &release.revision,
            guard.accepted_match_id,
            &stored,
            &attempt.candidates[0].mapping,
            &reasons,
        )
        .await?;
    }

    let reason = review_reason_summary(attempts);
    outln!("  queued for review: {}", Clean(&reason));
    Ok(())
}

async fn review_saved_release(
    pool: &SqlitePool,
    release: &LocalRelease,
    saved: Vec<store::StoredReview>,
    dry_run: bool,
    guard: &MatchGuard,
) -> Result<Outcome> {
    let mut groups: Vec<(ProviderKind, Vec<Candidate>, Vec<Evaluation>)> = saved
        .iter()
        .map(|review| {
            let candidates: Vec<Candidate> = review
                .candidates
                .iter()
                .map(|stored| Candidate {
                    release: stored.release.clone(),
                    mapping: stored.mapping.clone(),
                    score: stored.score,
                })
                .collect();
            let evaluations: Vec<Evaluation> = review
                .candidates
                .iter()
                .map(|stored| Evaluation {
                    auto_accept: stored.auto_accept,
                    match_scope: stored.match_scope,
                    blockers: stored.blockers.clone(),
                    warnings: stored.warnings.clone(),
                    evidence: stored.evidence.clone(),
                })
                .collect();
            (review.provider, candidates, evaluations)
        })
        .collect();

    print_release_header(release);
    if let Some(advice) = saved
        .iter()
        .flat_map(|review| &review.candidates)
        .find_map(|candidate| candidate.ai_advice.as_ref())
    {
        print_ai_advice(advice);
    }
    for (provider, candidates, evaluations) in &groups {
        for (index, (candidate, evaluation)) in
            candidates.iter().zip(evaluations.iter()).enumerate()
        {
            print_candidate(release, *provider, index, candidate, evaluation, None);
        }
    }

    match review_prompt(
        release,
        &mut groups,
        Option::<&MusicBrainzAdapter>::None,
        Option::<&DiscogsAdapter>::None,
    )
    .await?
    {
        ReviewOutcome::Accept(provider, candidate, evaluation) => {
            if dry_run {
                outln!(
                    "  would accept {provider} release {} after review",
                    candidate.release.id
                );
                return Ok(Outcome::Accepted);
            }
            accept(
                pool,
                release,
                provider,
                &candidate,
                &evaluation,
                AcceptanceMethod::Manual,
                false,
                guard,
                None,
            )
            .await
        }
        ReviewOutcome::Skip => Ok(Outcome::Skipped),
        ReviewOutcome::Quit => Ok(Outcome::Quit),
    }
}

#[allow(clippy::too_many_arguments)]
async fn review_attempts<M, D>(
    pool: &SqlitePool,
    release: &LocalRelease,
    attempts: &[ProviderAttempt],
    guard: &MatchGuard,
    dry_run: bool,
    musicbrainz: Option<&M>,
    discogs: Option<&D>,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    let mut groups: Vec<(ProviderKind, Vec<Candidate>, Vec<Evaluation>)> = attempts
        .iter()
        .filter(|attempt| !attempt.candidates.is_empty())
        .map(|attempt| {
            (
                attempt.provider,
                attempt.candidates.clone(),
                attempt.evaluations.clone(),
            )
        })
        .collect();

    if groups.is_empty() {
        return Ok(Outcome::Unmatched);
    }

    print_release_header(release);
    for (provider, candidates, evaluations) in &groups {
        for (index, (candidate, evaluation)) in
            candidates.iter().zip(evaluations.iter()).enumerate()
        {
            let current = attempts
                .iter()
                .find(|attempt| attempt.provider == *provider)
                .and_then(|attempt| attempt.error.as_ref());
            print_candidate(release, *provider, index, candidate, evaluation, current);
        }
    }

    match review_prompt(release, &mut groups, musicbrainz, discogs).await? {
        ReviewOutcome::Accept(provider, candidate, evaluation) => {
            if dry_run {
                outln!(
                    "  would accept {provider} release {} after review",
                    candidate.release.id
                );
                return Ok(Outcome::Accepted);
            }
            accept(
                pool,
                release,
                provider,
                &candidate,
                &evaluation,
                AcceptanceMethod::Manual,
                false,
                guard,
                None,
            )
            .await
        }
        ReviewOutcome::Skip => Ok(Outcome::Skipped),
        ReviewOutcome::Quit => Ok(Outcome::Quit),
    }
}

fn print_release_header(release: &LocalRelease) {
    outln!(
        "  local release {} — {} ({} file(s))",
        release.id,
        Clean(release.album_title.as_deref().unwrap_or("(no album tag)")),
        release.tracks.len()
    );
    outln!(
        "  artist: {}   folder: {}",
        Clean(release.album_artist.as_deref().unwrap_or("(unknown)")),
        Clean(&release.directory)
    );
    if !release.embedded_evidence.problems.is_empty() {
        for problem in &release.embedded_evidence.problems {
            outln!("  identifier problem: {}", Clean(problem));
        }
    }
}

fn print_candidate(
    _local: &LocalRelease,
    provider: ProviderKind,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
    stage_error: Option<&ProviderError>,
) {
    let release = &candidate.release;
    outln!(
        "\n  [{}] {} release {} — score {:.3}{}",
        index + 1,
        provider,
        release.id.storage_id(),
        evaluation.match_scope.score(&candidate.score),
        if evaluation.auto_accept {
            " [auto-acceptable]"
        } else {
            " [review]"
        }
    );
    outln!("      {}", Clean(&release.artist_display()));
    outln!("      {}", Clean(&release.url));
    let mut details = Vec::new();
    if let Some(year) = release.year {
        details.push(format!("year {year}"));
    }
    if let Some(country) = &release.country {
        details.push(country.clone());
    }
    if let Some(label) = release.label_display() {
        details.push(label);
    }
    for catalog in &release.catalog_numbers {
        details.push(format!("catno {catalog}"));
    }
    if let Some(format) = release.format_display() {
        details.push(format);
    }
    if !release.edition_notes.is_empty() {
        details.push(format!("edition: {}", release.edition_notes.join(", ")));
    }
    if !details.is_empty() {
        outln!("      {}", Clean(&details.join(" · ")));
    }
    if !release.structure_issues.is_empty() {
        for issue in &release.structure_issues {
            outln!("      structure: {}", Clean(issue));
        }
    }
    if !release.lookup_aliases.is_empty() {
        outln!(
            "      aliases: {}",
            release
                .lookup_aliases
                .iter()
                .map(ReleaseId::storage_id)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let components: Vec<String> = candidate
        .score
        .components()
        .iter()
        .filter_map(|(name, value, weight)| {
            if evaluation.match_scope == MatchScope::Album && *name == "year" {
                return None;
            }
            let weight = if evaluation.match_scope == MatchScope::Album {
                weight / (1.0 - matcher::WEIGHT_YEAR)
            } else {
                *weight
            };
            Some(format!("{name} {value:.2}×{weight:.2}"))
        })
        .collect();
    outln!("      score: {}", components.join("  "));
    outln!(
        "      mapping: {}/{} files mapped",
        candidate.mapping.assigned_count(),
        _local.tracks.len()
    );

    for note in &evaluation.evidence {
        outln!("      evidence: {}", Clean(note));
    }
    for blocker in &evaluation.blockers {
        outln!("      blocker: {}", Clean(blocker));
    }
    for warning in &evaluation.warnings {
        outln!("      warning: {}", Clean(warning));
    }
    for note in &candidate.mapping.notes {
        outln!("      mapping: {}", Clean(note));
    }
    if let Some(error) = stage_error {
        outln!("      stage error: {}", Clean(&error.to_string()));
    }
}

fn prompt(message: &str) -> Result<String> {
    out!("{message}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn parse_entered_id(provider: ProviderKind, raw: &str) -> Result<ReleaseId> {
    match provider {
        ProviderKind::MusicBrainz => {
            let id = raw
                .parse()
                .with_context(|| format!("'{raw}' is not a MusicBrainz MBID"))?;
            Ok(ReleaseId::MusicBrainz(id))
        }
        ProviderKind::Discogs => {
            let id = raw
                .parse::<u64>()
                .with_context(|| format!("'{raw}' is not a Discogs release ID"))?;
            Ok(ReleaseId::Discogs(id))
        }
    }
}

async fn fetch_entered<M, D>(
    provider: ProviderKind,
    id: ReleaseId,
    musicbrainz: Option<&M>,
    discogs: Option<&D>,
) -> Result<RemoteRelease, ProviderError>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    match provider {
        ProviderKind::MusicBrainz => match musicbrainz {
            Some(client) => client.fetch_release(id).await,
            None => Err(ProviderError::Configuration {
                provider,
                message: "no MusicBrainz session is available in this run".to_string(),
            }),
        },
        ProviderKind::Discogs => match discogs {
            Some(client) => client.fetch_release(id).await,
            None => Err(ProviderError::Configuration {
                provider,
                message: "no Discogs session is available in this run".to_string(),
            }),
        },
    }
}

async fn review_prompt<M, D>(
    local: &LocalRelease,
    groups: &mut Vec<(ProviderKind, Vec<Candidate>, Vec<Evaluation>)>,
    musicbrainz: Option<&M>,
    discogs: Option<&D>,
) -> Result<ReviewOutcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    loop {
        let action = prompt("\n[a]ccept  [i]d  [s]kip  [q]uit > ")?;
        match action.as_str() {
            "a" | "" => {
                let group_index = if groups.len() > 1 {
                    let choice = prompt("provider [1]=first [2]=second > ")?;
                    choice.parse::<usize>().unwrap_or(1).clamp(1, groups.len()) - 1
                } else {
                    0
                };
                let (provider, candidates, evaluations) = &groups[group_index];
                let selection = prompt("candidate number [1] > ")?;
                let index = selection
                    .parse::<usize>()
                    .unwrap_or(1)
                    .clamp(1, candidates.len().max(1))
                    - 1;
                let candidate = &candidates[index];
                let evaluation = &evaluations[index];

                if !evaluation.can_apply(candidate) {
                    outln!(
                        "cannot accept: the track mapping is invalid, incomplete, or structurally unsupported ({})",
                        Clean(&candidate.mapping.notes.join("; "))
                    );
                    continue;
                }

                if !evaluation.auto_accept {
                    outln!("manual acceptance overrides these automatic checks:");
                    for blocker in &evaluation.blockers {
                        outln!("  - {}", Clean(blocker));
                    }
                }
                for warning in &evaluation.warnings {
                    outln!("  warning: {}", Clean(warning));
                }

                let confirm = prompt("accept this release? [y/N] > ")?;
                if matches!(confirm.as_str(), "y" | "yes") {
                    return Ok(ReviewOutcome::Accept(
                        *provider,
                        Box::new(candidate.clone()),
                        Box::new(evaluation.clone()),
                    ));
                }
            }
            "i" => {
                let provider = if groups.len() > 1 {
                    let choice = prompt("provider [1]=musicbrainz [2]=discogs > ")?;
                    match choice.as_str() {
                        "2" | "discogs" => ProviderKind::Discogs,
                        _ => ProviderKind::MusicBrainz,
                    }
                } else {
                    groups[0].0
                };
                let raw = prompt("release ID > ")?;
                let id = match parse_entered_id(provider, &raw) {
                    Ok(id) => id,
                    Err(error) => {
                        outln!("invalid release ID: {}", Clean(&error.to_string()));
                        continue;
                    }
                };
                match fetch_entered(provider, id, musicbrainz, discogs).await {
                    Ok(release) => {
                        let candidate = Candidate::new(local, release);
                        let evaluation = matcher::evaluate(
                            local,
                            &candidate,
                            None,
                            &DiscoveryEvidence::default(),
                        );
                        outln!("\nFetched {provider} release:");
                        print_candidate(local, provider, 0, &candidate, &evaluation, None);
                        groups.push((provider, vec![candidate], vec![evaluation]));
                        outln!("  candidate added to this review");
                    }
                    Err(error) => outln!("could not fetch release: {}", Clean(&error.to_string())),
                }
            }
            "s" => return Ok(ReviewOutcome::Skip),
            "q" => return Ok(ReviewOutcome::Quit),
            _ => outln!("unknown action"),
        }
    }
}

fn print_decision(attempt: &ProviderAttempt) {
    for (index, (candidate, evaluation)) in attempt
        .candidates
        .iter()
        .zip(attempt.evaluations.iter())
        .enumerate()
        .take(3)
    {
        outln!(
            "  {}. {} release {} — {} (score {:.3}){}",
            index + 1,
            attempt.provider,
            candidate.release.id.storage_id(),
            Clean(&candidate.release.title),
            evaluation.match_scope.score(&candidate.score),
            if evaluation.auto_accept {
                " [auto-acceptable]"
            } else {
                " [review]"
            }
        );
        for blocker in &evaluation.blockers {
            outln!("       blocked: {}", Clean(blocker));
        }
        for warning in &evaluation.warnings {
            outln!("       note: {}", Clean(warning));
        }
    }
    if let (Some(candidate), Some(evaluation)) =
        (attempt.candidates.first(), attempt.evaluations.first())
    {
        if evaluation.auto_accept {
            outln!(
                "  would accept {} release {} (score {:.3})",
                attempt.provider,
                candidate.release.id.storage_id(),
                evaluation.match_scope.score(&candidate.score)
            );
        } else {
            outln!("  would queue for review");
        }
    }
}

fn print_decision_for(provider: ProviderKind, candidate: &Candidate, evaluation: &Evaluation) {
    print_decision(&ProviderAttempt {
        provider,
        candidates: vec![candidate.clone()],
        evaluations: vec![evaluation.clone()],
        notes: Vec::new(),
        error: None,
        embedded_release_resolved: false,
        ai_advice: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Mbid, SearchPage, SearchRequest};

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    #[test]
    fn defaults_to_musicbrainz_then_discogs_and_explicit_selectors_disable_fallback() {
        let id = ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        assert_eq!(
            resolve_policy(None, None).unwrap(),
            ProviderPolicy::MusicBrainzThenDiscogs
        );
        assert_eq!(
            resolve_policy(None, Some(&id)).unwrap(),
            ProviderPolicy::Only(ProviderKind::MusicBrainz)
        );
        assert_eq!(
            resolve_policy(Some(ProviderKind::Discogs), None).unwrap(),
            ProviderPolicy::Only(ProviderKind::Discogs)
        );
        assert_eq!(
            resolve_policy(Some(ProviderKind::MusicBrainz), None).unwrap(),
            ProviderPolicy::Only(ProviderKind::MusicBrainz)
        );
        assert_eq!(
            resolve_policy(None, Some(&ReleaseId::Discogs(42))).unwrap(),
            ProviderPolicy::Only(ProviderKind::Discogs)
        );
        assert!(resolve_policy(Some(ProviderKind::Discogs), Some(&id)).is_err());
    }

    fn ai_album_fixture() -> (LocalRelease, RemoteRelease) {
        use crate::matcher::{
            ArtistKind, EmbeddedReleaseEvidence, LocalTrack, RemoteTrack, TrackEntryType,
        };
        let local = LocalRelease {
            id: 1,
            directory: "/music/album".into(),
            album_title: Some("Album".into()),
            album_artist: Some("Artist".into()),
            artist_kind: ArtistKind::AlbumArtist,
            year: Some(2001),
            barcode: None,
            catalog_number: None,
            embedded_discogs_id: None,
            embedded_evidence: EmbeddedReleaseEvidence::default(),
            is_compilation: false,
            has_album_tag: true,
            revision: "rev".into(),
            tracks: (1..=12)
                .map(|number| LocalTrack {
                    id: number,
                    path: format!("/music/album/{number}.flac"),
                    title: Some(if number == 1 {
                        "Track One".into()
                    } else {
                        format!("Track {number}")
                    }),
                    disc: None,
                    track_number: Some(number),
                    duration_secs: Some(180.0 + number as f64),
                    release_track_id: None,
                    recording_id: None,
                    identifier_problems: vec![],
                })
                .collect(),
        };
        let remote = RemoteRelease {
            id: ReleaseId::Discogs(42),
            group_id: None,
            title: "Album".into(),
            artists: vec!["Artist".into()],
            year: Some(2001),
            country: None,
            labels: vec![],
            catalog_numbers: vec![],
            formats: vec!["CD".into()],
            barcodes: vec![],
            url: "https://example.invalid/release/42".into(),
            lookup_aliases: vec![],
            artist_credit: None,
            structure_issues: vec![],
            edition_notes: vec![],
            provider_metadata: serde_json::Value::Null,
            tracklist: (1..=12)
                .map(|number| RemoteTrack {
                    position: Some(number.to_string()),
                    title: format!("Track {number}"),
                    artists: vec![],
                    duration_secs: Some(180.0 + number as f64),
                    entry_type: TrackEntryType::Track,
                    medium_position: None,
                    track_position: None,
                    release_track_id: None,
                    recording_id: None,
                    artist_credit: None,
                })
                .collect(),
        };
        (local, remote)
    }

    #[test]
    fn review_summary_uses_fallback_reasons_when_the_primary_failed() {
        let (local, remote) = ai_album_fixture();
        let primary = ProviderAttempt::failed(
            ProviderKind::MusicBrainz,
            ProviderError::Network {
                provider: ProviderKind::MusicBrainz,
                message: "HTTP 503 Service Unavailable after 3 attempts".into(),
            },
        );
        let fallback = ProviderAttempt::from_discovery(
            ProviderKind::Discogs,
            &local,
            DiscoveryResult {
                releases: vec![remote],
                ..Default::default()
            },
            None,
            false,
        );
        let expected = fallback.evaluations[0].blockers[0].clone();
        let reason = review_reason_summary(&[primary, fallback]);
        assert!(reason.contains(&format!("discogs: {expected}")), "{reason}");
        assert!(
            reason.contains("musicbrainz request failed: HTTP 503"),
            "{reason}"
        );
    }

    #[test]
    fn review_summary_explains_unresolved_selection_without_inventing_conflicts() {
        let (local, mut remote) = ai_album_fixture();
        remote.tracklist[0].title = "Track One".into();
        let attempt = ProviderAttempt::from_discovery(
            ProviderKind::Discogs,
            &local,
            DiscoveryResult {
                releases: vec![remote],
                ..Default::default()
            },
            None,
            false,
        );
        assert!(attempt.evaluations[0].blockers.is_empty());
        assert_eq!(
            review_reason_summary(&[attempt]),
            "candidate selection requires review"
        );
    }

    #[test]
    fn unoffered_close_candidates_cannot_inherit_semantic_agreement() {
        let (local, remote) = ai_album_fixture();
        for change_credit in [false, true] {
            let releases = (42..=47)
                .map(|id| {
                    let mut release = remote.clone();
                    release.id = ReleaseId::Discogs(id);
                    if id == 47 {
                        if change_credit {
                            release.tracklist[11].artists =
                                vec!["Entirely Different Artist".into()];
                        } else {
                            release.tracklist[11].title = "Entirely Different Recording".into();
                        }
                    }
                    release
                })
                .collect();
            let mut attempt = ProviderAttempt::from_discovery(
                ProviderKind::Discogs,
                &local,
                DiscoveryResult {
                    releases,
                    ..Default::default()
                },
                None,
                false,
            );
            assert_eq!(attempt.candidates[5].release.id, ReleaseId::Discogs(47));
            attempt.ai_advice = Some(Advice {
                model: "clef-flash".into(),
                prompt_version: "radish-clef-album-v3".into(),
                choice: AdviceChoice::Candidate {
                    release: ReleaseId::Discogs(42),
                },
                probabilities: (42..=46)
                    .map(|id| (format!("discogs:{id}"), if id == 42 { 1.0 } else { 0.0 }))
                    .chain([("none".into(), 0.0), ("equivalent_editions".into(), 0.0)])
                    .collect(),
                confidence: 1.0,
                request_digest: None,
            });
            assert!(
                choose_ai_match(&local, std::slice::from_ref(&attempt)).is_err(),
                "unassessed text/credit conflict accepted"
            );
            attempt.candidates.pop();
            attempt.evaluations.pop();
            assert!(
                choose_ai_match(&local, std::slice::from_ref(&attempt)).is_ok(),
                "compatible assessed editions must still be eligible"
            );
            // A reduced request may assess fewer than the usual five candidates.
            // Candidates omitted to fit the request must retain the stricter
            // unassessed-evidence check, even when they are in the first five.
            attempt
                .ai_advice
                .as_mut()
                .unwrap()
                .probabilities
                .retain(|key, _| {
                    !key.starts_with("discogs:") || key == "discogs:42" || key == "discogs:43"
                });
            attempt.candidates[3].release.tracklist[11].artists =
                vec!["Entirely Different Artist".into()];
            assert!(
                choose_ai_match(&local, &[attempt]).is_err(),
                "budget-omitted candidates cannot inherit semantic agreement"
            );
        }
    }

    fn scanned_track(path: &str, album: &str, number: i64) -> crate::scanner::ScannedTrack {
        crate::scanner::ScannedTrack {
            path: std::path::PathBuf::from(path),
            title: Some(format!("Track {number}")),
            artist: Some("Artist".into()),
            album: Some(album.into()),
            album_artist: Some("Artist".into()),
            year: Some(2001),
            track_number: Some(number),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz: crate::scanner::ObservedMusicBrainzIds::default(),
            duration_secs: Some(180.0),
            modified_at: 1,
        }
    }

    /// Answers the first search by retagging another release's files, as a
    /// concurrent import would, then reports no results.
    struct RetaggingProvider {
        pool: SqlitePool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        retag: crate::scanner::ScannedTrack,
        directory_id: i64,
    }

    impl MetadataProvider for RetaggingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::MusicBrainz
        }

        async fn search(&self, _request: SearchRequest) -> Result<SearchPage, ProviderError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                store::upsert_tracks(
                    &self.pool,
                    std::slice::from_ref(&self.retag),
                    self.directory_id,
                )
                .await
                .unwrap();
                store::rebuild_local_releases(&self.pool, None)
                    .await
                    .unwrap();
            }
            Ok(SearchPage {
                hits: Vec::new(),
                total: Some(0),
                next_offset: None,
            })
        }

        async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
            Err(ProviderError::Permanent {
                provider: id.provider(),
                status: None,
                message: "not available in this test".into(),
            })
        }
    }

    #[tokio::test]
    async fn release_changed_after_loading_is_skipped_without_contacting_providers() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let first = scanned_track("/music/a/01.flac", "Alpha", 1);
        let second = scanned_track("/music/b/01.flac", "Beta", 1);
        store::upsert_tracks(&pool, &[first, second.clone()], directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_provider = || -> Result<RetaggingProvider> {
            let mut retag = second.clone();
            retag.title = Some("Retitled".into());
            retag.modified_at = 2;
            Ok(RetaggingProvider {
                pool: pool.clone(),
                calls: calls.clone(),
                retag,
                directory_id,
            })
        };
        let options = MatchOptions {
            policy: ProviderPolicy::Only(ProviderKind::MusicBrainz),
            ..MatchOptions::default()
        };

        let summary = run_with_policy(
            &pool,
            &options,
            make_provider,
            || -> Result<RetaggingProvider> { bail!("Discogs is not used in this test") },
        )
        .await
        .unwrap();

        assert_eq!(summary.skipped, 1, "{summary:?}");
        assert_eq!(summary.failed, 0, "{summary:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the first release may contact the provider"
        );
        let beta = store::load_local_releases(&pool, None)
            .await
            .unwrap()
            .into_iter()
            .find(|release| release.album_title.as_deref() == Some("Beta"))
            .unwrap();
        assert!(
            store::load_review(&pool, beta.id, ProviderKind::MusicBrainz)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn accept_turns_a_changed_snapshot_into_a_skip() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let file = scanned_track("/music/a/01.flac", "Alpha", 1);
        store::upsert_tracks(&pool, std::slice::from_ref(&file), directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();
        let release = store::load_local_releases(&pool, None)
            .await
            .unwrap()
            .remove(0);
        let guard = store::load_match_guard(&pool, release.id).await.unwrap();

        let (_, mut remote) = ai_album_fixture();
        remote.tracklist.truncate(1);
        let candidate = Candidate::new(&release, remote);
        let evaluation = matcher::evaluate(
            &release,
            &candidate,
            None,
            &matcher::DiscoveryEvidence::default(),
        );
        assert!(evaluation.can_apply(&candidate));

        // The files change after the release was loaded and the guard taken.
        let mut retagged = file;
        retagged.title = Some("Retitled".into());
        retagged.modified_at = 2;
        store::upsert_tracks(&pool, &[retagged], directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();

        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            &candidate,
            &evaluation,
            AcceptanceMethod::Manual,
            false,
            &guard,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Skipped), "{outcome:?}");
    }
}
