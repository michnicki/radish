//! CLI orchestration: select local releases, run the provider policy, decide,
//! review, and persist.
//!
//! MusicBrainz is attempted first. Discogs is created lazily, only after an
//! unsatisfactory MusicBrainz stage and only when a token is available. An
//! explicit provider or direct selector uses one service and no fallback.

use crate::discogs::{AdapterError, DiscogsAdapter};
use crate::discovery::{self, DiscoveryResult};
use crate::matcher::{self, Candidate, DiscoveryEvidence, Evaluation, LocalRelease, RemoteRelease};
use crate::musicbrainz::MusicBrainzAdapter;
use crate::providers::{MetadataProvider, ProviderError, ProviderKind, ProviderPolicy, ReleaseId};
use crate::store::{self, AcceptanceMethod, MatchGuard, StoredCandidate};
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct MatchOptions {
    pub scope: Option<PathBuf>,
    pub local_release: Option<i64>,
    pub direct_release: Option<ReleaseId>,
    pub review: bool,
    pub dry_run: bool,
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
            println!("\nDry run: no matches were stored and no overlays were applied.");
        }
        println!(
            "\nAccepted: {}\nQueued for review: {}\nUnmatched: {}\nSkipped: {}\nFailed: {}",
            self.accepted, self.queued, self.unmatched, self.skipped, self.failed
        );
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
        }
    }

    fn from_discovery(
        provider: ProviderKind,
        local: &LocalRelease,
        result: DiscoveryResult,
        error: Option<ProviderError>,
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
        candidates.sort_by(|a, b| {
            b.score
                .total
                .total_cmp(&a.score.total)
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
                matcher::evaluate(local, candidate, runner_up, &evidence)
            })
            .collect();

        Self {
            provider,
            candidates,
            evaluations,
            notes: result.notes,
            error,
            embedded_release_resolved,
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
    match discovery::discover(provider, local, direct).await {
        Ok(result) => ProviderAttempt::from_discovery(kind, local, result, None),
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
    let mut releases = store::load_local_releases(pool, options.scope.as_deref()).await?;

    if let Some(id) = options.local_release {
        releases.retain(|release| release.id == id);
        if releases.is_empty() {
            bail!("local release {id} was not found");
        }
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
                        println!(
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
        println!("Nothing to match.");
        return Ok(summary);
    }

    let mut musicbrainz_client: Option<M> = None;
    let mut discogs_client: Option<D> = None;
    let mut discogs_disabled: Option<String> = None;

    for release in &selected {
        let label = format!(
            "{} — {}",
            release
                .album_artist
                .as_deref()
                .unwrap_or("(unknown artist)"),
            release.album_title.as_deref().unwrap_or("(no album tag)")
        );
        println!("\nMatching local release {} ({label})", release.id);

        // A stale match is refreshed from the provider that accepted it; a
        // different source requires deliberate review.
        let mut release_policy = options.policy;
        if !options.review
            && statuses.get(&release.id).map(String::as_str) == Some(store::STATUS_STALE)
            && let Some(provider) = existing_matches.get(&release.id).and_then(|p| *p)
            && release_policy == ProviderPolicy::MusicBrainzThenDiscogs
        {
            println!("  refreshing the stale {provider} match from its original source");
            release_policy = ProviderPolicy::Only(provider);
        }

        let guard = store::load_match_guard(pool, release.id).await?;

        // Unchanged saved queues are reviewed without contacting a provider.
        if let Some(saved) = load_saved_reviews(pool, release, &guard, release_policy).await? {
            if !options.review {
                println!(
                    "  queued for review from saved candidates ({} provider group(s))",
                    saved.len()
                );
                summary.queued += 1;
                continue;
            }
            match review_saved_release(pool, release, saved, options.dry_run, &guard).await {
                Ok(Outcome::Accepted) => summary.accepted += 1,
                Ok(Outcome::Queued) => summary.queued += 1,
                Ok(Outcome::Skipped) => summary.skipped += 1,
                Ok(Outcome::Quit) => return Ok(summary),
                Ok(_) => summary.failed += 1,
                Err(error) => {
                    eprintln!("failed to review local release {}: {error:#}", release.id);
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
            interactive,
            &mut musicbrainz_client,
            &mut discogs_client,
            &mut discogs_disabled,
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
                eprintln!("failed to match local release {}: {error:#}", release.id);
            }
        }
    }

    Ok(summary)
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
    interactive: bool,
    musicbrainz_client: &mut Option<M>,
    discogs_client: &mut Option<D>,
    discogs_disabled: &mut Option<String>,
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
            println!("  Discogs fallback: {reason}");
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
                if let Some(reason) = discogs_disabled.as_ref() {
                    fallback_skip = Some(reason.clone());
                    break;
                }
                if discogs_client.is_none() {
                    match make_discogs() {
                        Ok(client) => *discogs_client = Some(client),
                        Err(error) => {
                            let missing = is_missing_discogs_token(&error);
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
                                let reason = format!(
                                    "configured Discogs credentials are unusable: {error:#}"
                                );
                                *discogs_disabled = Some(reason.clone());
                                fallback_skip = Some(reason);
                                eprintln!("  Disabling Discogs fallback for the rest of this run");
                            }
                            break;
                        }
                    }
                }
                let client = discogs_client.as_ref().expect("client initialized");
                attempts.push(attempt_provider(client, release, direct.clone()).await);
            }
        }
    }

    for attempt in &attempts {
        for note in &attempt.notes {
            eprintln!("  note ({}): {note}", attempt.provider);
        }
        if let Some(error) = &attempt.error {
            eprintln!("  {} stage error: {error}", attempt.provider);
        }
    }
    if let Some(reason) = &fallback_skip {
        println!("  {reason}");
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
    {
        let primary = &attempts[0];
        let conflicts = matcher::fallback_conflicts(
            release,
            &primary.candidates,
            candidate,
            primary.embedded_release_resolved,
        );
        if conflicts.is_empty() && direct.is_none() && !options.review && !options.dry_run {
            if let Some(error) = &primary.error {
                println!("  recovered the release from a failed MusicBrainz stage: {error}");
            }
            return accept(
                pool,
                release,
                fallback.provider,
                candidate,
                evaluation,
                AcceptanceMethod::Automatic,
                false,
                guard,
            )
            .await;
        }
        for conflict in &conflicts {
            println!("  fallback conflict: {conflict}");
        }
    }

    let has_candidates = attempts
        .iter()
        .any(|attempt| !attempt.candidates.is_empty());
    let fallback_failed = attempts
        .get(1)
        .is_some_and(|attempt| attempt.error.is_some());

    if has_candidates {
        if options.dry_run {
            for attempt in &attempts {
                print_decision(attempt);
            }
            return Ok(Outcome::Queued);
        }
        if interactive {
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
        eprintln!("  {error}");
        if fallback_failed {
            eprintln!("  the fallback stage also failed; the release outcome is a failure");
        }
        return Ok(Outcome::Failed);
    }

    println!(
        "  no {} candidates found",
        attempts
            .first()
            .map(|a| a.provider)
            .unwrap_or(ProviderKind::MusicBrainz)
    );
    Ok(Outcome::Unmatched)
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
) -> Result<Outcome> {
    if !evaluation.can_apply(candidate) {
        println!(
            "  cannot accept {provider} release {}: the track mapping is invalid or incomplete ({})",
            candidate.release.id,
            candidate.mapping.notes.join("; ")
        );
        return Ok(Outcome::Skipped);
    }

    if dry_run {
        print_decision_for(provider, candidate, evaluation);
        println!(
            "  would accept {provider} release {} (score {:.3}) as {}",
            candidate.release.id,
            candidate.score.total,
            method.as_str()
        );
        return Ok(Outcome::Accepted);
    }

    match store::accept_match(pool, release.id, candidate, method, guard).await {
        Ok(()) => {
            println!(
                "  accepted {provider} release {} (score {:.3}, {})",
                candidate.release.id,
                candidate.score.total,
                method.as_str()
            );
            println!("    {} {}", provider, candidate.release.id.storage_id());
            println!("    {}", candidate.release.url);
            Ok(Outcome::Accepted)
        }
        Err(error) => {
            eprintln!("  could not apply the match: {error:#}");
            Ok(Outcome::Failed)
        }
    }
}

fn stored_candidates(attempt: &ProviderAttempt) -> Vec<StoredCandidate> {
    attempt
        .candidates
        .iter()
        .zip(attempt.evaluations.iter())
        .take(5)
        .map(|(candidate, evaluation)| StoredCandidate {
            release: candidate.release.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: evaluation.auto_accept,
            blockers: evaluation.blockers.clone(),
            warnings: evaluation.warnings.clone(),
            evidence: evaluation.evidence.clone(),
        })
        .collect()
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

    let reason = attempts
        .first()
        .and_then(|attempt| attempt.evaluations.first())
        .map(|evaluation| evaluation.blockers.join("; "))
        .unwrap_or_default();
    println!("  queued for review: {reason}");
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
                    blockers: stored.blockers.clone(),
                    warnings: stored.warnings.clone(),
                    evidence: stored.evidence.clone(),
                })
                .collect();
            (review.provider, candidates, evaluations)
        })
        .collect();

    print_release_header(release);
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
                println!(
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
                println!(
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
            )
            .await
        }
        ReviewOutcome::Skip => Ok(Outcome::Skipped),
        ReviewOutcome::Quit => Ok(Outcome::Quit),
    }
}

fn print_release_header(release: &LocalRelease) {
    println!(
        "  local release {} — {} ({} file(s))",
        release.id,
        release.album_title.as_deref().unwrap_or("(no album tag)"),
        release.tracks.len()
    );
    println!(
        "  artist: {}   folder: {}",
        release.album_artist.as_deref().unwrap_or("(unknown)"),
        release.directory
    );
    if !release.embedded_evidence.problems.is_empty() {
        for problem in &release.embedded_evidence.problems {
            println!("  identifier problem: {problem}");
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
    println!(
        "\n  [{}] {} release {} — score {:.3}{}",
        index + 1,
        provider,
        release.id.storage_id(),
        candidate.score.total,
        if evaluation.auto_accept {
            " [auto-acceptable]"
        } else {
            " [review]"
        }
    );
    println!("      {}", release.artist_display());
    println!("      {}", release.url);
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
        println!("      {}", details.join(" · "));
    }
    if !release.structure_issues.is_empty() {
        for issue in &release.structure_issues {
            println!("      structure: {issue}");
        }
    }
    if !release.lookup_aliases.is_empty() {
        println!(
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
        .map(|(name, value, weight)| format!("{name} {value:.2}×{weight:.2}"))
        .collect();
    println!("      score: {}", components.join("  "));
    println!(
        "      mapping: {}/{} files mapped",
        candidate.mapping.assigned_count(),
        _local.tracks.len()
    );

    for note in &evaluation.evidence {
        println!("      evidence: {note}");
    }
    for blocker in &evaluation.blockers {
        println!("      blocker: {blocker}");
    }
    for warning in &evaluation.warnings {
        println!("      warning: {warning}");
    }
    for note in &candidate.mapping.notes {
        println!("      mapping: {note}");
    }
    if let Some(error) = stage_error {
        println!("      stage error: {error}");
    }
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
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
                    println!(
                        "cannot accept: the track mapping is invalid, incomplete, or structurally unsupported ({})",
                        candidate.mapping.notes.join("; ")
                    );
                    continue;
                }

                if !evaluation.auto_accept {
                    println!("manual acceptance overrides these automatic checks:");
                    for blocker in &evaluation.blockers {
                        println!("  - {blocker}");
                    }
                }
                for warning in &evaluation.warnings {
                    println!("  warning: {warning}");
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
                        println!("invalid release ID: {error}");
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
                        println!("\nFetched {provider} release:");
                        print_candidate(local, provider, 0, &candidate, &evaluation, None);
                        groups.push((provider, vec![candidate], vec![evaluation]));
                        println!("  candidate added to this review");
                    }
                    Err(error) => println!("could not fetch release: {error}"),
                }
            }
            "s" => return Ok(ReviewOutcome::Skip),
            "q" => return Ok(ReviewOutcome::Quit),
            _ => println!("unknown action"),
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
        println!(
            "  {}. {} release {} — {} (score {:.3}){}",
            index + 1,
            attempt.provider,
            candidate.release.id.storage_id(),
            candidate.release.title,
            candidate.score.total,
            if evaluation.auto_accept {
                " [auto-acceptable]"
            } else {
                " [review]"
            }
        );
        for blocker in &evaluation.blockers {
            println!("       blocked: {blocker}");
        }
    }
    if let (Some(candidate), Some(evaluation)) =
        (attempt.candidates.first(), attempt.evaluations.first())
    {
        if evaluation.auto_accept {
            println!(
                "  would accept {} release {} (score {:.3})",
                attempt.provider,
                candidate.release.id.storage_id(),
                candidate.score.total
            );
        } else {
            println!("  would queue for review");
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Mbid;

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
}
