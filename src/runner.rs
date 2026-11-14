//! CLI orchestration: select local releases, run the provider policy, decide,
//! review, and persist.
//!
//! MusicBrainz is attempted first. Discogs is created lazily, only after an
//! unsatisfactory MusicBrainz stage and only when a token is available. An
//! explicit provider or direct selector uses one service and no fallback.

use crate::discogs::{AdapterError, DiscogsAdapter};
use crate::discovery::{self, DiscoveryResult};
use crate::index::{IndexKind, IndexPaths, local::LocalIndex};
use crate::matcher::{
    self, Candidate, DiscoveryEvidence, Evaluation, LocalRelease, MatchScope, RemoteRelease,
};
use crate::musicbrainz::MusicBrainzAdapter;
use crate::output::Clean;
use crate::providers::{MetadataProvider, ProviderError, ProviderKind, ProviderPolicy, ReleaseId};
use crate::store::{self, AcceptanceMethod, MatchGuard, StoredCandidate};
use crate::{out, outln};
use anyhow::{Context, Result, bail};
use futures_util::stream::{self, StreamExt};
use sqlx::SqlitePool;
use std::cell::{OnceCell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

/// [`outln!`] into a [`Transcript`].
macro_rules! outln_to {
    ($transcript:expr, $($arg:tt)*) => {
        $transcript.out(format_args!($($arg)*))
    };
}

/// [`eprintln!`] into a [`Transcript`].
macro_rules! eprintln_to {
    ($transcript:expr, $($arg:tt)*) => {
        $transcript.err(format_args!($($arg)*))
    };
}

/// How many candidates per provider a queued review snapshot keeps.
const REVIEW_CANDIDATE_LIMIT: usize = 5;

/// The most releases one run matches at once.
///
/// Each release in flight keeps at most one query open per provider, so the
/// read pools of the offline indexes are sized for this many readers.
pub const MAX_JOBS: u16 = 16;

#[derive(Debug, Clone, Default)]
pub struct MatchOptions {
    pub scope: Option<PathBuf>,
    pub local_release: Option<i64>,
    pub direct_release: Option<ReleaseId>,
    pub review: bool,
    pub dry_run: bool,
    pub policy: ProviderPolicy,
    /// How many releases to match at once; 0 and 1 both mean one at a time.
    /// Interactive review always takes one release at a time.
    pub jobs: usize,
}

/// Where one release's progress lines go.
///
/// A release matched on its own prints as it goes. Releases matched at once
/// hold their lines until they finish, so the output of two releases never
/// interleaves.
#[derive(Debug)]
struct Transcript {
    /// `None` prints each line at once; otherwise the lines so far, in order.
    held: Option<Vec<Line>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Out(String),
    Err(String),
}

impl Transcript {
    /// Prints every line as it is written.
    fn direct() -> Self {
        Self { held: None }
    }

    /// Holds every line until [`Transcript::flush`].
    fn held() -> Self {
        Self {
            held: Some(Vec::new()),
        }
    }

    /// Writes one line to stdout, now or when flushed.
    fn out(&mut self, args: fmt::Arguments<'_>) {
        match &mut self.held {
            None => crate::output::write_line(args),
            Some(lines) => lines.push(Line::Out(args.to_string())),
        }
    }

    /// Writes one line to stderr, now or when flushed.
    fn err(&mut self, args: fmt::Arguments<'_>) {
        match &mut self.held {
            None => eprintln!("{args}"),
            Some(lines) => lines.push(Line::Err(args.to_string())),
        }
    }

    /// Prints the held lines in the order they were written.
    fn flush(self) {
        for line in self.held.into_iter().flatten() {
            match line {
                Line::Out(text) => crate::output::write_line(format_args!("{text}")),
                Line::Err(text) => eprintln!("{text}"),
            }
        }
    }
}

/// The provider clients of one run, each built when a release first needs it
/// and shared by every release after that.
struct Clients<'a, M, D> {
    musicbrainz: OnceCell<M>,
    discogs: OnceCell<D>,
    /// Why Discogs is off for the rest of the run, once it is.
    discogs_disabled: RefCell<Option<ProviderError>>,
    make_musicbrainz: &'a dyn Fn() -> Result<M>,
    make_discogs: &'a dyn Fn() -> Result<D>,
}

impl<M, D> Clients<'_, M, D> {
    /// The MusicBrainz client, built now if no release has needed it yet.
    fn musicbrainz(&self) -> Result<&M> {
        if let Some(client) = self.musicbrainz.get() {
            return Ok(client);
        }
        let client = (self.make_musicbrainz)()?;
        Ok(self.musicbrainz.get_or_init(|| client))
    }

    /// The Discogs client, built now if no release has needed it yet.
    fn discogs(&self) -> Result<&D> {
        if let Some(client) = self.discogs.get() {
            return Ok(client);
        }
        let client = (self.make_discogs)()?;
        Ok(self.discogs.get_or_init(|| client))
    }

    fn discogs_disabled(&self) -> Option<ProviderError> {
        self.discogs_disabled.borrow().clone()
    }

    fn disable_discogs(&self, error: ProviderError) {
        *self.discogs_disabled.borrow_mut() = Some(error);
    }
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
    /// Failures discovery worked around because other results arrived.
    pub partial_failures: Vec<ProviderError>,
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
            partial_failures: Vec::new(),
            embedded_release_resolved: false,
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
                    .then_with(|| a.mapping.medium.is_none().cmp(&b.mapping.medium.is_none()))
                    .then_with(|| b.release.id.cmp(&a.release.id))
            })
            .map(|candidate| candidate.release.id.clone());
        candidates.sort_by(|a, b| candidate_order(exact_id.as_ref(), a, b));

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
            partial_failures: result.failures,
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

/// Matches against locally built dump indexes instead of the network.
///
/// Every provider the active policy may attempt must have an index. That is
/// checked once, before any release is processed, so a half-built setup fails
/// up front instead of silently losing its fallback stage. No credential is
/// read and no request is made on this path.
pub async fn run_match_offline(
    pool: &SqlitePool,
    options: MatchOptions,
    index_dir: &std::path::Path,
) -> Result<MatchSummary> {
    let paths = IndexPaths::from_dir(index_dir);
    let mut musicbrainz_index: Option<LocalIndex> = None;
    let mut discogs_index: Option<LocalIndex> = None;

    for provider in options.policy.order() {
        let kind = IndexKind::for_provider(provider);
        let index = LocalIndex::open(&paths, kind)
            .await
            .with_context(|| format!("offline matching needs the {kind} index"))?;
        match provider {
            ProviderKind::MusicBrainz => musicbrainz_index = Some(index),
            ProviderKind::Discogs => discogs_index = Some(index),
        }
    }

    // The policy layer builds each adapter at most once per run, so each cell
    // is handed out exactly once; a second request is a programming error.
    let musicbrainz = std::cell::RefCell::new(musicbrainz_index);
    let discogs = std::cell::RefCell::new(discogs_index);

    run_with_policy(
        pool,
        &options,
        || {
            musicbrainz
                .borrow_mut()
                .take()
                .context("the MusicBrainz index was already opened")
        },
        || {
            discogs
                .borrow_mut()
                .take()
                .context("the Discogs index is not part of the selected policy")
        },
    )
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

    let clients = Clients {
        musicbrainz: OnceCell::new(),
        discogs: OnceCell::new(),
        discogs_disabled: RefCell::new(None),
        make_musicbrainz: &make_musicbrainz,
        make_discogs: &make_discogs,
    };
    // A MusicBrainz adapter that cannot be built is a configuration error, not
    // an outage: every release would hit it again, and a Discogs fallback would
    // look like a recovery from a stage that never ran. Fail once, up front.
    // Explicit review works offline from saved reviews, so it builds lazily.
    let needs_musicbrainz = !options.review
        && selected.iter().any(|release| {
            refresh_source(options, &statuses, &existing_matches, release.id)
                .map_or(options.policy, ProviderPolicy::Only)
                .allows(ProviderKind::MusicBrainz)
        });
    if needs_musicbrainz && let Err(error) = clients.musicbrainz() {
        bail!("MusicBrainz is unusable: {}", Clean(&format!("{error:#}")));
    }

    // Each release waits on SQLite or a provider almost all of the time, so
    // several in flight on this one task keep that many queries running.
    let jobs = if options.review {
        1
    } else {
        options.jobs.max(1)
    };
    let mut outcomes = stream::iter(&selected)
        .map(|release| {
            let (statuses, existing_matches, clients) = (&statuses, &existing_matches, &clients);
            async move {
                let mut transcript = if jobs > 1 {
                    Transcript::held()
                } else {
                    Transcript::direct()
                };
                let outcome = match_release(
                    pool,
                    release,
                    options,
                    statuses,
                    existing_matches,
                    clients,
                    &mut transcript,
                )
                .await;
                (outcome, transcript)
            }
        })
        .buffer_unordered(jobs);

    while let Some((outcome, transcript)) = outcomes.next().await {
        transcript.flush();
        match outcome? {
            Outcome::Accepted => summary.accepted += 1,
            Outcome::Queued => summary.queued += 1,
            Outcome::Unmatched => summary.unmatched += 1,
            Outcome::Skipped => summary.skipped += 1,
            Outcome::Failed => summary.failed += 1,
            Outcome::Quit => return Ok(summary),
        }
    }

    Ok(summary)
}

/// Matches or reviews one selected release, writing its progress to
/// `transcript`.
///
/// # Errors
///
/// Only when the release's stored state cannot be read; every other failure
/// is reported in the transcript and returned as [`Outcome::Failed`].
async fn match_release<M, D>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    statuses: &HashMap<i64, String>,
    existing_matches: &HashMap<i64, Option<ProviderKind>>,
    clients: &Clients<'_, M, D>,
    transcript: &mut Transcript,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
{
    let label = format!(
        "{} — {}",
        release
            .album_artist
            .as_deref()
            .unwrap_or("(unknown artist)"),
        release.album_title.as_deref().unwrap_or("(no album tag)")
    );
    outln_to!(
        transcript,
        "\nMatching local release {} ({})",
        release.id,
        Clean(&label)
    );

    let mut release_policy = options.policy;
    let mut refreshing = false;
    if let Some(provider) = refresh_source(options, statuses, existing_matches, release.id) {
        outln_to!(
            transcript,
            "  refreshing the stale {provider} match from its original source"
        );
        release_policy = ProviderPolicy::Only(provider);
        refreshing = true;
    }

    let guard = match store::load_match_guard(pool, release.id).await {
        Ok(guard) => guard,
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
            outln_to!(
                transcript,
                "  skipping local release {}: it no longer exists; run match again",
                release.id
            );
            return Ok(Outcome::Skipped);
        }
        Err(error) => return Err(error),
    };
    // The release was loaded before provider calls began. If its files
    // changed since, any candidate evaluated against the old snapshot
    // would map onto the wrong files, so do not contact a provider.
    if guard.source_revision != release.revision {
        outln_to!(
            transcript,
            "  skipping local release {}: its files changed since matching started; run match again",
            release.id
        );
        return Ok(Outcome::Skipped);
    }

    // Explicit review stays offline. Normal matching rediscovers candidates
    // so an old edition-only review does not permanently block automation.
    if options.review
        && let Some(saved) = load_saved_reviews(pool, release, &guard, release_policy).await?
    {
        return Ok(
            match review_saved_release(pool, release, saved, options.dry_run, &guard).await {
                Ok(
                    outcome @ (Outcome::Accepted
                    | Outcome::Queued
                    | Outcome::Skipped
                    | Outcome::Quit),
                ) => outcome,
                Ok(_) => Outcome::Failed,
                Err(error) => {
                    eprintln_to!(
                        transcript,
                        "failed to review local release {}: {}",
                        release.id,
                        Clean(&format!("{error:#}"))
                    );
                    Outcome::Failed
                }
            },
        );
    }

    let outcome = process_release(
        pool,
        release,
        options,
        release_policy,
        clients,
        refreshing,
        &guard,
        transcript,
    )
    .await;
    Ok(outcome.unwrap_or_else(|error| {
        eprintln_to!(
            transcript,
            "failed to match local release {}: {}",
            release.id,
            Clean(&format!("{error:#}"))
        );
        Outcome::Failed
    }))
}

/// The provider a stale match is refreshed from, when this run refreshes it.
///
/// A stale match is refreshed from the provider that accepted it; a different
/// source requires deliberate review.
fn refresh_source(
    options: &MatchOptions,
    statuses: &HashMap<i64, String>,
    existing_matches: &HashMap<i64, Option<ProviderKind>>,
    release_id: i64,
) -> Option<ProviderKind> {
    if options.review
        || options.policy != ProviderPolicy::MusicBrainzThenDiscogs
        || statuses.get(&release_id).map(String::as_str) != Some(store::STATUS_STALE)
    {
        return None;
    }
    existing_matches
        .get(&release_id)
        .and_then(|provider| *provider)
}

/// A primary stage that failed for a reason that says nothing about the
/// release means a fallback candidate was never compared against what the
/// primary would have found, so it must be reviewed rather than accepted.
///
/// Failed lookups that discovery worked around count too: the release that
/// failed to load may have been the right one.
fn primary_failed_transiently(attempts: &[ProviderAttempt]) -> bool {
    attempts.first().is_some_and(|attempt| {
        attempt
            .error
            .iter()
            .chain(&attempt.partial_failures)
            .any(ProviderError::is_transient)
    })
}

/// Starts a line about a primary-versus-fallback disagreement that the files
/// settled, in the transcript and in a review's notes.
const SETTLED_NOTE_PREFIX: &str = "fallback conflict settled by the files: ";

const TRANSIENT_PRIMARY_NOTE: &str =
    "MusicBrainz stage failed transiently; fallback candidates need review";

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

#[expect(
    clippy::too_many_arguments,
    reason = "one release's whole decision; the arguments have no natural grouping"
)]
async fn process_release<M, D>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    policy: ProviderPolicy,
    clients: &Clients<'_, M, D>,
    refreshing: bool,
    guard: &MatchGuard,
    transcript: &mut Transcript,
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
            outln_to!(transcript, "  Discogs fallback: {}", Clean(&reason));
        }

        match provider {
            ProviderKind::MusicBrainz => {
                let client = match clients.musicbrainz() {
                    Ok(client) => client,
                    Err(error) => {
                        attempts.push(ProviderAttempt::failed(
                            ProviderKind::MusicBrainz,
                            provider_error_from(ProviderKind::MusicBrainz, error),
                        ));
                        continue;
                    }
                };
                attempts.push(attempt_provider(client, release, direct.clone()).await);
            }
            ProviderKind::Discogs => {
                if let Some(error) = clients.discogs_disabled() {
                    if policy.is_explicit() && !refreshing {
                        // The user asked for Discogs, so a release it cannot
                        // serve is a failure, whichever release met the error.
                        attempts.push(ProviderAttempt::failed(ProviderKind::Discogs, error));
                    } else {
                        fallback_skip = Some(format!(
                            "Discogs is disabled for the rest of this run: {error}"
                        ));
                    }
                    break;
                }
                let client = match clients.discogs() {
                    Ok(client) => client,
                    Err(error) => {
                        let missing = is_missing_discogs_token(&error);
                        if refreshing && missing {
                            // The release is not at fault and nothing was
                            // tried, so this is neither a failure nor a miss.
                            outln_to!(
                                transcript,
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
                            fallback_skip = Some(format!("Discogs fallback was skipped: {error}"));
                            clients.disable_discogs(error);
                            eprintln_to!(
                                transcript,
                                "  Disabling Discogs fallback for the rest of this run"
                            );
                        }
                        break;
                    }
                };
                let attempt = attempt_provider(client, release, direct.clone()).await;
                // A rejected credential rejects every later release too.
                if let Some(error) = attempt.error.as_ref().filter(|error| error.is_auth()) {
                    clients.disable_discogs(error.clone());
                    eprintln_to!(transcript, "  Disabling Discogs for the rest of this run");
                }
                attempts.push(attempt);
            }
        }
    }

    for attempt in &attempts {
        for note in &attempt.notes {
            eprintln_to!(transcript, "  note ({}): {}", attempt.provider, Clean(note));
        }
        if let Some(error) = &attempt.error {
            eprintln_to!(
                transcript,
                "  {} stage error: {}",
                attempt.provider,
                Clean(&error.to_string())
            );
        }
    }
    if let Some(reason) = &fallback_skip {
        outln_to!(transcript, "  {}", Clean(reason));
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
                clients.musicbrainz.get(),
                clients.discogs.get(),
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
            transcript,
        )
        .await;
    }

    // A fallback that would have settled is held for review, and says why.
    if attempts.len() > 1
        && attempts[1]
            .evaluations
            .first()
            .is_some_and(|evaluation| evaluation.auto_accept)
        && primary_failed_transiently(&attempts)
    {
        outln_to!(transcript, "  {TRANSIENT_PRIMARY_NOTE}");
        attempts[1].notes.push(TRANSIENT_PRIMARY_NOTE.to_string());
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
        let matcher::FallbackCheck { conflicts, settled } = matcher::fallback_check(
            release,
            &primary.candidates,
            candidate,
            primary.embedded_release_resolved,
            evaluation.relies_on_unlisted_lengths(),
        );
        if conflicts.is_empty() && direct.is_none() && !options.review {
            if let Some(error) = &primary.error {
                outln_to!(
                    transcript,
                    "  recovered the release from a failed MusicBrainz stage: {}",
                    Clean(&error.to_string())
                );
            }
            for line in &settled {
                outln_to!(transcript, "  {SETTLED_NOTE_PREFIX}{}", Clean(line));
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
                transcript,
            )
            .await;
        }
        for conflict in &conflicts {
            outln_to!(transcript, "  fallback conflict: {}", Clean(conflict));
        }
        attempts[1].notes.extend(conflicts);
        // The reviewer still sees what the files settled.
        attempts[1].notes.extend(
            settled
                .into_iter()
                .map(|line| format!("{SETTLED_NOTE_PREFIX}{line}")),
        );
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
                print_decision(attempt, transcript);
            }
            return Ok(Outcome::Queued);
        }
        if options.review {
            return review_attempts(
                pool,
                release,
                &attempts,
                guard,
                options.dry_run,
                clients.musicbrainz.get(),
                clients.discogs.get(),
            )
            .await;
        }
        queue_attempts(pool, release, &attempts, guard, transcript).await?;
        return Ok(Outcome::Queued);
    }

    // No usable candidates. A completed-empty stage means no match; an
    // unrecovered attempted-stage failure must not be disguised as one.
    if let Some(error) = attempts.iter().find_map(|attempt| attempt.error.clone()) {
        eprintln_to!(transcript, "  {}", Clean(&error.to_string()));
        if fallback_failed {
            eprintln_to!(
                transcript,
                "  the fallback stage also failed; the release outcome is a failure"
            );
        }
        return Ok(Outcome::Failed);
    }

    // Nothing was attempted (the reason was printed above), so there is no
    // search result to call a miss.
    let Some(first) = attempts.first() else {
        return Ok(Outcome::Skipped);
    };
    outln_to!(transcript, "  no {} candidates found", first.provider);
    Ok(Outcome::Unmatched)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the decision, its provenance and where to report it travel together"
)]
async fn accept(
    pool: &SqlitePool,
    release: &LocalRelease,
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
    method: AcceptanceMethod,
    dry_run: bool,
    guard: &MatchGuard,
    transcript: &mut Transcript,
) -> Result<Outcome> {
    if !evaluation.can_apply(candidate) {
        outln_to!(
            transcript,
            "  cannot accept {provider} release {}: the track mapping is invalid or incomplete ({})",
            candidate.release.id,
            Clean(&candidate.mapping.notes.join("; "))
        );
        return Ok(Outcome::Skipped);
    }

    if dry_run {
        print_decision_for(provider, candidate, evaluation, transcript);
        outln_to!(
            transcript,
            "  would accept {provider} release {} (score {:.3}) as {}",
            candidate.release.id,
            evaluation.match_scope.score(&candidate.score),
            method.as_str()
        );
        return Ok(Outcome::Accepted);
    }

    let result = store::accept_match(
        pool,
        release,
        candidate,
        method,
        evaluation.match_scope,
        guard,
    )
    .await;
    match result {
        Ok(()) => {
            outln_to!(
                transcript,
                "  accepted {provider} release {} (score {:.3}, {})",
                candidate.release.id,
                evaluation.match_scope.score(&candidate.score),
                method.as_str()
            );
            outln_to!(
                transcript,
                "    {} {}",
                provider,
                candidate.release.id.storage_id()
            );
            outln_to!(transcript, "    {}", Clean(&candidate.release.url));
            if let Some(medium) = candidate.mapping.medium {
                outln_to!(
                    transcript,
                    "    matched {}",
                    matcher::disc_label(&candidate.release, medium)
                );
            }
            if evaluation.match_scope == MatchScope::Album {
                outln_to!(
                    transcript,
                    "    album and tracks matched; exact edition unconfirmed"
                );
                for note in evaluation.rule_notes() {
                    outln_to!(transcript, "    {}", Clean(note));
                }
            }
            Ok(Outcome::Accepted)
        }
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
            outln_to!(transcript, "  skipping: {error}; run match again");
            Ok(Outcome::Skipped)
        }
        Err(error) => {
            eprintln_to!(
                transcript,
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
        .map(|(candidate, evaluation)| StoredCandidate {
            release: candidate.release.clone(),
            mapping: candidate.mapping.clone(),
            score: candidate.score,
            auto_accept: evaluation.auto_accept,
            match_scope: evaluation.match_scope,
            blockers: evaluation.blockers.clone(),
            warnings: evaluation.warnings.clone(),
            evidence: evaluation.evidence.clone(),
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
    transcript: &mut Transcript,
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
    outln_to!(transcript, "  queued for review: {}", Clean(&reason));
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
                // Interactive review handles one release at a time.
                &mut Transcript::direct(),
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
                // Interactive review handles one release at a time.
                &mut Transcript::direct(),
            )
            .await
        }
        ReviewOutcome::Skip => Ok(Outcome::Skipped),
        ReviewOutcome::Quit => Ok(Outcome::Quit),
    }
}

fn print_release_header(release: &LocalRelease) {
    out!("{}", render_release_header(release));
}

fn render_release_header(release: &LocalRelease) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "  local release {} — {} ({} file(s))",
        release.id,
        Clean(release.album_title.as_deref().unwrap_or("(no album tag)")),
        release.tracks.len()
    );
    let _ = writeln!(
        text,
        "  artist: {}   folder: {}",
        Clean(release.album_artist.as_deref().unwrap_or("(unknown)")),
        Clean(&release.directory)
    );
    if !release.embedded_evidence.problems.is_empty() {
        for problem in &release.embedded_evidence.problems {
            let _ = writeln!(text, "  identifier problem: {}", Clean(problem));
        }
    }
    text
}

fn print_candidate(
    local: &LocalRelease,
    provider: ProviderKind,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
    stage_error: Option<&ProviderError>,
) {
    out!(
        "{}",
        render_candidate(local, provider, index, candidate, evaluation, stage_error)
    );
}

/// The order candidates are shown and judged in: the proven edition first, then
/// album score, then total score. On a full tie a release the files make up whole
/// comes before one of whose discs they are only one, then the release ID.
fn candidate_order(exact_id: Option<&ReleaseId>, a: &Candidate, b: &Candidate) -> Ordering {
    (exact_id == Some(&b.release.id))
        .cmp(&(exact_id == Some(&a.release.id)))
        .then_with(|| b.score.album_score().total_cmp(&a.score.album_score()))
        .then_with(|| b.score.total.total_cmp(&a.score.total))
        .then_with(|| b.mapping.medium.is_none().cmp(&a.mapping.medium.is_none()))
        .then_with(|| a.release.id.cmp(&b.release.id))
}

fn render_candidate(
    local: &LocalRelease,
    provider: ProviderKind,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
    stage_error: Option<&ProviderError>,
) -> String {
    let mut text = String::new();
    let release = &candidate.release;
    let _ = writeln!(
        text,
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
    let _ = writeln!(text, "      {}", Clean(&release.artist_display()));
    let _ = writeln!(text, "      {}", Clean(&release.url));
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
        let _ = writeln!(text, "      {}", Clean(&details.join(" · ")));
    }
    if !release.structure_issues.is_empty() {
        for issue in &release.structure_issues {
            let _ = writeln!(text, "      structure: {}", Clean(issue));
        }
    }
    if !release.lookup_aliases.is_empty() {
        let _ = writeln!(
            text,
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
    let _ = writeln!(text, "      score: {}", components.join("  "));
    let disc = candidate
        .mapping
        .medium
        .map(|medium| format!(" to {}", matcher::disc_label(release, medium)))
        .unwrap_or_default();
    let _ = writeln!(
        text,
        "      mapping: {}/{} files mapped{disc}",
        candidate.mapping.assigned_count(),
        local.tracks.len()
    );

    for note in &evaluation.evidence {
        let _ = writeln!(text, "      evidence: {}", Clean(note));
    }
    for blocker in &evaluation.blockers {
        let _ = writeln!(text, "      blocker: {}", Clean(blocker));
    }
    for warning in &evaluation.warnings {
        let _ = writeln!(text, "      warning: {}", Clean(warning));
    }
    for note in &candidate.mapping.notes {
        let _ = writeln!(text, "      mapping: {}", Clean(note));
    }
    if let Some(error) = stage_error {
        let _ = writeln!(text, "      stage error: {}", Clean(&error.to_string()));
    }
    text
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

fn print_decision(attempt: &ProviderAttempt, transcript: &mut Transcript) {
    for (index, (candidate, evaluation)) in attempt
        .candidates
        .iter()
        .zip(attempt.evaluations.iter())
        .enumerate()
        .take(3)
    {
        outln_to!(
            transcript,
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
            outln_to!(transcript, "       blocked: {}", Clean(blocker));
        }
        for warning in &evaluation.warnings {
            outln_to!(transcript, "       note: {}", Clean(warning));
        }
    }
    if let (Some(candidate), Some(evaluation)) =
        (attempt.candidates.first(), attempt.evaluations.first())
    {
        if evaluation.auto_accept {
            outln_to!(
                transcript,
                "  would accept {} release {} (score {:.3})",
                attempt.provider,
                candidate.release.id.storage_id(),
                evaluation.match_scope.score(&candidate.score)
            );
        } else {
            outln_to!(transcript, "  would queue for review");
        }
    }
}

fn print_decision_for(
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
    transcript: &mut Transcript,
) {
    print_decision(
        &ProviderAttempt {
            provider,
            candidates: vec![candidate.clone()],
            evaluations: vec![evaluation.clone()],
            notes: Vec::new(),
            error: None,
            partial_failures: Vec::new(),
            embedded_release_resolved: false,
        },
        transcript,
    );
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

    fn album_fixture() -> (LocalRelease, RemoteRelease) {
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
        let (local, mut remote) = album_fixture();
        // One mismatched title is settled by its duration; two still need review,
        // unless every length agrees within 1.5 s, so one other length is 2 s off.
        remote.tracklist[1].title = "A different song".into();
        remote.tracklist[5].duration_secs = Some(188.0);
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
        let (local, mut remote) = album_fixture();
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

    /// Holds the first `gate` searches until all of them are waiting at once,
    /// then answers every search with no results.
    struct GatedProvider {
        barrier: std::sync::Arc<tokio::sync::Barrier>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        gate: usize,
    }

    impl MetadataProvider for GatedProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::MusicBrainz
        }

        async fn search(&self, _request: SearchRequest) -> Result<SearchPage, ProviderError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < self.gate {
                self.barrier.wait().await;
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
    async fn jobs_match_that_many_releases_at_once() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let albums = ["Alpha", "Beta", "Gamma", "Delta"];
        let tracks: Vec<_> = albums
            .iter()
            .map(|album| scanned_track(&format!("/music/{album}/01.flac"), album, 1))
            .collect();
        store::upsert_tracks(&pool, &tracks, directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(albums.len()));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_provider = || -> Result<GatedProvider> {
            Ok(GatedProvider {
                barrier: barrier.clone(),
                calls: calls.clone(),
                gate: albums.len(),
            })
        };
        let options = MatchOptions {
            policy: ProviderPolicy::Only(ProviderKind::MusicBrainz),
            jobs: albums.len(),
            ..MatchOptions::default()
        };

        // One release at a time would wait at the barrier forever.
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_with_policy(
                &pool,
                &options,
                make_provider,
                || -> Result<GatedProvider> { bail!("Discogs is not used in this test") },
            ),
        )
        .await
        .expect("the releases were not matched at the same time")
        .unwrap();

        assert_eq!(summary.unmatched, albums.len(), "{summary:?}");
        assert_eq!(summary.failed, 0, "{summary:?}");
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
    async fn accept_prints_that_a_text_only_album_match_has_unconfirmed_lengths() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=3)
            .map(|number| scanned_track(&format!("/music/a/{number:02}.flac"), "Album", number))
            .collect();
        store::upsert_tracks(&pool, &files, directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();
        let release = store::load_local_releases(&pool, None)
            .await
            .unwrap()
            .remove(0);
        let guard = store::load_match_guard(&pool, release.id).await.unwrap();

        // The provider lists the same three titles and no length at all.
        let (_, mut remote) = album_fixture();
        remote.tracklist.truncate(3);
        for track in &mut remote.tracklist {
            track.duration_secs = None;
        }
        let candidate = Candidate::new(&release, remote);
        let evaluation = matcher::evaluate_for_matching(
            &release,
            &candidate,
            &[],
            &matcher::DiscoveryEvidence::default(),
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        let mut transcript = Transcript::held();
        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            &candidate,
            &evaluation,
            AcceptanceMethod::Automatic,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        let lines: Vec<&str> = transcript
            .held
            .iter()
            .flatten()
            .filter_map(|line| match line {
                Line::Out(text) => Some(text.as_str()),
                Line::Err(_) => None,
            })
            .collect();
        let unconfirmed = lines
            .iter()
            .position(|line| line.contains("exact edition unconfirmed"))
            .expect("the album scope note");
        assert_eq!(
            lines.get(unconfirmed + 1),
            Some(&"    discogs lists no duration for 3 of 3 tracks; track lengths are unconfirmed")
        );
    }

    #[tokio::test]
    async fn accept_prints_that_a_one_sided_label_was_read_as_a_labelling_difference() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=5)
            .map(|number| scanned_track(&format!("/music/a/{number:02}.flac"), "Album", number))
            .collect();
        store::upsert_tracks(&pool, &files, directory_id)
            .await
            .unwrap();
        store::rebuild_local_releases(&pool, None).await.unwrap();
        let release = store::load_local_releases(&pool, None)
            .await
            .unwrap()
            .remove(0);
        let guard = store::load_match_guard(&pool, release.id).await.unwrap();

        // The provider lists the same five titles and lengths, and calls the
        // release a live one, which the files never say.
        let (_, mut remote) = album_fixture();
        remote.tracklist.truncate(5);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.title = format!("Track {}", index + 1);
            track.duration_secs = Some(180.0);
        }
        remote.edition_notes = vec!["Live".to_string()];
        let candidate = Candidate::new(&release, remote);
        let evaluation = matcher::evaluate_for_matching(
            &release,
            &candidate,
            &[],
            &matcher::DiscoveryEvidence::default(),
        );
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);

        let mut transcript = Transcript::held();
        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            &candidate,
            &evaluation,
            AcceptanceMethod::Automatic,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        let lines: Vec<&str> = transcript
            .held
            .iter()
            .flatten()
            .filter_map(|line| match line {
                Line::Out(text) => Some(text.as_str()),
                Line::Err(_) => None,
            })
            .collect();
        let unconfirmed = lines
            .iter()
            .position(|line| line.contains("exact edition unconfirmed"))
            .expect("the album scope note");
        assert_eq!(
            lines.get(unconfirmed + 1),
            Some(
                &"    edition qualifiers differ (local {} versus discogs {\"live\"}) but every title and every duration within 1.5s agree in track order; read as a labelling difference"
            )
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

        let (_, mut remote) = album_fixture();
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
            &mut Transcript::direct(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Skipped), "{outcome:?}");
    }

    #[test]
    fn only_outage_failures_count_as_a_transient_primary_failure() {
        let failed = |error| [ProviderAttempt::failed(ProviderKind::MusicBrainz, error)];
        let provider = ProviderKind::MusicBrainz;
        assert!(primary_failed_transiently(&failed(
            ProviderError::Network {
                provider,
                message: "down".into(),
            }
        )));
        assert!(primary_failed_transiently(&failed(
            ProviderError::RateLimited {
                provider,
                message: "slow down".into(),
                retry_after: None,
            }
        )));
        assert!(primary_failed_transiently(&failed(
            ProviderError::Unavailable {
                provider,
                retry_after: std::time::Duration::from_secs(5),
            }
        )));
        // A malformed response is deterministic for that release, and starts no cooldown.
        assert!(!primary_failed_transiently(&failed(
            ProviderError::Malformed {
                provider,
                message: "bad json".into(),
            }
        )));
        assert!(!primary_failed_transiently(&failed(
            ProviderError::Permanent {
                provider,
                status: Some(400),
                message: "bad".into(),
            }
        )));
        assert!(!primary_failed_transiently(&[]));
    }

    #[test]
    fn worked_around_transient_lookup_failures_count_as_a_transient_primary_failure() {
        let provider = ProviderKind::MusicBrainz;
        let mut attempt = ProviderAttempt::failed(
            provider,
            ProviderError::Network {
                provider,
                message: "unused".into(),
            },
        );
        attempt.error = None;
        assert!(!primary_failed_transiently(std::slice::from_ref(&attempt)));
        attempt.partial_failures = vec![ProviderError::Malformed {
            provider,
            message: "bad json".into(),
        }];
        assert!(!primary_failed_transiently(std::slice::from_ref(&attempt)));
        attempt.partial_failures.push(ProviderError::Unavailable {
            provider,
            retry_after: std::time::Duration::from_secs(5),
        });
        assert!(primary_failed_transiently(std::slice::from_ref(&attempt)));
    }

    #[test]
    fn a_candidate_for_one_disc_names_its_disc() {
        let (mut local, mut remote) = album_fixture();
        let whole = Candidate::new(&local, remote.clone());
        let evaluation =
            matcher::evaluate(&local, &whole, None, &matcher::DiscoveryEvidence::default());
        let text = render_candidate(&local, ProviderKind::Discogs, 0, &whole, &evaluation, None);
        assert!(
            text.contains("      mapping: 12/12 files mapped\n"),
            "{text}"
        );

        // The files are the second disc of a release of two six-track discs.
        local.directory = "/music/album/CD2".into();
        local.tracks.retain(|track| track.id > 6);
        for track in &mut local.tracks {
            track.track_number = track.track_number.map(|number| number - 6);
        }
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(if index < 6 { 1 } else { 2 });
            track.track_position = Some(index as u32 % 6 + 1);
        }
        let disc = Candidate::new(&local, remote);
        assert_eq!(disc.mapping.medium, Some(2));
        let evaluation =
            matcher::evaluate(&local, &disc, None, &matcher::DiscoveryEvidence::default());
        let text = render_candidate(&local, ProviderKind::Discogs, 0, &disc, &evaluation, None);
        assert!(
            text.contains("      mapping: 6/6 files mapped to disc 2 of 2\n"),
            "{text}"
        );
    }

    #[test]
    fn rendered_candidates_and_headers_escape_every_untrusted_field() {
        let (mut local, mut remote) = album_fixture();
        local.album_title = Some("title\x1b[2Jlocal".into());
        local.album_artist = Some("artist\x1b[3Jlocal".into());
        local.directory = "/music/\x1b]0;folder\x07".into();
        remote.artists = vec!["remote\x1b[31martist".into()];
        remote.url = "https://example.invalid/\x1b]0;url\x07".into();
        remote.country = Some("country\x1b[4J".into());
        remote.edition_notes = vec!["edition\u{202e}note".into()];
        remote.structure_issues = vec!["structure\x1b[5J".into()];
        let candidate = Candidate::new(&local, remote);
        let mut evaluation = matcher::evaluate(
            &local,
            &candidate,
            None,
            &matcher::DiscoveryEvidence::default(),
        );
        evaluation.blockers.push("blocker\x1b[6J".into());
        let error = ProviderError::Network {
            provider: ProviderKind::Discogs,
            message: "stage\x1b[7J".into(),
        };

        let text = format!(
            "{}{}",
            render_release_header(&local),
            render_candidate(
                &local,
                ProviderKind::Discogs,
                0,
                &candidate,
                &evaluation,
                Some(&error)
            )
        );

        for (line_start, escaped) in [
            ("      remote", "remote\\u{1b}[31martist"),
            (
                "      https://",
                "https://example.invalid/\\u{1b}]0;url\\u{7}",
            ),
            ("  artist: ", "artist\\u{1b}[3Jlocal"),
            ("  local release", "title\\u{1b}[2Jlocal"),
            ("      structure:", "structure\\u{1b}[5J"),
            ("      blocker:", "blocker\\u{1b}[6J"),
            ("      stage error:", "stage\\u{1b}[7J"),
        ] {
            assert!(
                text.lines()
                    .any(|line| line.starts_with(line_start) && line.contains(escaped)),
                "no escaped {line_start:?} line in {text:?}"
            );
        }
        assert!(
            !text.chars().any(|c| c != '\n' && c.is_control()),
            "{text:?}"
        );
        assert!(!text.contains('\u{202e}'), "{text:?}");
    }

    #[test]
    fn a_full_tie_prefers_the_release_the_files_make_up_whole() {
        use crate::matcher::{RemoteTrack, TrackEntryType};
        let (mut local, whole_release) = album_fixture();
        // The six files are the first of two six-track discs, and also all of
        // a six-track release whose ID sorts after the two-disc release's.
        let mut two_discs = whole_release.clone();
        two_discs.id = ReleaseId::Discogs(7);
        for (index, track) in two_discs.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(if index < 6 { 1 } else { 2 });
            track.track_position = Some(index as u32 % 6 + 1);
        }
        local.directory = "/music/album/CD1".into();
        let first_disc: Vec<RemoteTrack> = two_discs.tracklist[..6].to_vec();
        local.tracks.truncate(6);
        let mut whole = whole_release;
        whole.id = ReleaseId::Discogs(9);
        whole.tracklist = first_disc
            .into_iter()
            .map(|track| RemoteTrack {
                entry_type: TrackEntryType::Track,
                medium_position: None,
                track_position: None,
                ..track
            })
            .collect();

        let disc = Candidate::new(&local, two_discs);
        let whole = Candidate::new(&local, whole);
        assert_eq!(disc.mapping.medium, Some(1));
        assert_eq!(whole.mapping.medium, None);
        let mut disc = disc;
        disc.score = whole.score;

        let mut candidates = [disc, whole];
        candidates.sort_by(|a, b| candidate_order(None, a, b));
        let order: Vec<ReleaseId> = candidates.iter().map(|c| c.release.id.clone()).collect();
        assert_eq!(order, vec![ReleaseId::Discogs(9), ReleaseId::Discogs(7)]);
        candidates.reverse();
        candidates.sort_by(|a, b| candidate_order(None, a, b));
        assert_eq!(candidates[0].release.id, ReleaseId::Discogs(9));
    }
}
