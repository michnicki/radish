//! CLI orchestration: select local releases, run the provider policy, decide,
//! review, and persist.
//!
//! The default chain is MusicBrainz, then Discogs, then Deezer when the run
//! asks for it (`--deezer`). Each later stage runs only when the stages before
//! it do not settle the release, and Discogs only when a token is available.
//! An explicit provider or direct selector uses one service and no fallback.

use crate::deezer::DeezerAdapter;
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
use std::borrow::Cow;
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

/// Writes a run-wide notice to stderr through a [`Transcript`].
///
/// Unlike [`eprintln_to!`], a notice survives [`Transcript::into_notices`].
macro_rules! notice_to {
    ($transcript:expr, $($arg:tt)*) => {
        $transcript.notice(format_args!($($arg)*))
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
    /// A stderr line about the whole run, which a discarded pass still owes.
    Notice(String),
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

    /// Writes one run-wide line to stderr, now or when flushed.
    ///
    /// Use it for what holds beyond the release being matched, such as Discogs
    /// being switched off for the rest of the run.
    fn notice(&mut self, args: fmt::Arguments<'_>) {
        match &mut self.held {
            None => eprintln!("{args}"),
            Some(lines) => lines.push(Line::Notice(args.to_string())),
        }
    }

    /// Replays the lines of `other` here, in the order they were written.
    fn append(&mut self, other: Transcript) {
        for line in other.held.into_iter().flatten() {
            match line {
                Line::Out(text) => self.out(format_args!("{text}")),
                Line::Err(text) => self.err(format_args!("{text}")),
                Line::Notice(text) => self.notice(format_args!("{text}")),
            }
        }
    }

    /// Drops every line except the run-wide notices.
    ///
    /// A discarded pass keeps these: they report what the pass did to the run,
    /// which the next pass does not repeat.
    fn into_notices(self) -> Transcript {
        let notices = self
            .held
            .into_iter()
            .flatten()
            .filter(|line| matches!(line, Line::Notice(_)))
            .collect();
        Transcript {
            held: Some(notices),
        }
    }

    /// Prints the held lines in the order they were written.
    fn flush(self) {
        for line in self.held.into_iter().flatten() {
            match line {
                Line::Out(text) => crate::output::write_line(format_args!("{text}")),
                Line::Err(text) | Line::Notice(text) => eprintln!("{text}"),
            }
        }
    }
}

/// The provider clients of one run, shared by every release.
///
/// MusicBrainz and Deezer are built up front when a selected release may attempt
/// them, so that a client that cannot be built fails the run once. Discogs is
/// built when a release first needs it, since a missing token only skips it.
struct Clients<'a, M, D, Z> {
    musicbrainz: OnceCell<M>,
    discogs: OnceCell<D>,
    deezer: OnceCell<Z>,
    /// Why Discogs is off for the rest of the run, once it is.
    discogs_disabled: RefCell<Option<ProviderError>>,
    make_musicbrainz: &'a dyn Fn() -> Result<M>,
    make_discogs: &'a dyn Fn() -> Result<D>,
    make_deezer: &'a dyn Fn() -> Result<Z>,
}

impl<M, D, Z> Clients<'_, M, D, Z> {
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

    /// The Deezer client, built now if no release has needed it yet.
    fn deezer(&self) -> Result<&Z> {
        if let Some(client) = self.deezer.get() {
            return Ok(client);
        }
        let client = (self.make_deezer)()?;
        Ok(self.deezer.get_or_init(|| client))
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

/// Resolves the CLI selection into a provider policy.
///
/// A direct release selector implies its provider. `deezer` adds Deezer to the
/// default chain.
///
/// # Errors
///
/// When an explicit provider contradicts a direct selector, or `deezer` comes
/// with either: both select one service. Checked before any database or
/// network work.
pub fn resolve_policy(
    explicit: Option<ProviderKind>,
    direct: Option<&ReleaseId>,
    deezer: bool,
) -> Result<ProviderPolicy> {
    match (explicit, direct) {
        (None, None) => Ok(ProviderPolicy::Chain { deezer }),
        _ if deezer => {
            bail!(
                "--deezer adds Deezer to the default chain; it cannot be combined with a selected provider or release"
            )
        }
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

/// Rejects an `--offline` run whose policy is Deezer alone.
///
/// Deezer has no dump index, so `--provider deezer` and `--deezer-release`
/// always use the network; `--offline --deezer` keeps the indexes for
/// MusicBrainz and Discogs and adds Deezer after them.
///
/// # Errors
///
/// For [`ProviderPolicy::Only`] Deezer.
pub fn check_offline(policy: ProviderPolicy) -> Result<()> {
    if policy == ProviderPolicy::Only(ProviderKind::Deezer) {
        bail!(
            "--offline cannot match with Deezer alone: Deezer has no local index (drop --offline, or use --offline --deezer)"
        );
    }
    Ok(())
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
    /// The candidate [`prefer_complete`] moved to the front, and the one it
    /// displaced. Printed with the release's other progress lines.
    promotion: Option<Promotion>,
    /// The exact edition [`prefer_complete`] refused to rank first because it
    /// is a worse reference than the candidate that sorted first.
    refusal: Option<Refusal>,
}

/// A complete edition ranked before the incomplete one that sorted first.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Promotion {
    promoted: ReleaseId,
    previous: ReleaseId,
    /// Set when the exact edition was refused for its fit and ranked first
    /// after all, because the first candidate kept ahead of it is not accepted
    /// (see [`fall_back_to_refused_exact`]).
    fallback: bool,
}

/// An edition that lists exactly the files but was not ranked first, because
/// it is a worse reference than the first candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refusal {
    first: ReleaseId,
    exact: ReleaseId,
    reason: String,
}

/// What [`prefer_complete`] did to the order of the candidates.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Reranking {
    promotion: Option<Promotion>,
    /// The first exact edition refused as a worse reference, whether or not a
    /// later one was promoted past it.
    refusal: Option<Refusal>,
    /// Every exact edition refused as a worse reference, in rank order, for
    /// [`fall_back_to_refused_exact`].
    refused: Vec<ReleaseId>,
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
            promotion: None,
            refusal: None,
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
        let Reranking {
            mut promotion,
            refusal,
            refused,
        } = rank_candidates(local, &mut candidates, exact_id.as_ref());

        let mut evaluations = evaluate_candidates(local, &candidates, &result.evidence, direct);
        if !direct
            && exact_id.is_none()
            && evaluations
                .first()
                .is_some_and(|evaluation| !evaluation.auto_accept)
            && let Some(fallback) = fall_back_to_refused_exact(&mut candidates, &refused)
        {
            promotion = Some(fallback);
            evaluations = evaluate_candidates(local, &candidates, &result.evidence, direct);
        }

        Self {
            provider,
            candidates,
            evaluations,
            notes: result.notes,
            error,
            partial_failures: result.failures,
            embedded_release_resolved,
            promotion,
            refusal,
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
    run_with_policy(
        pool,
        &options,
        MusicBrainzAdapter::new,
        || DiscogsAdapter::from_env().map_err(anyhow::Error::new),
        DeezerAdapter::new,
    )
    .await
}

/// Matches against locally built dump indexes instead of the network.
///
/// Every MusicBrainz or Discogs stage the active policy may attempt must have
/// an index. That is checked once, before any release is processed, so a
/// half-built setup fails up front instead of silently losing its fallback
/// stage. No credential is read on this path. Deezer has no index: a policy
/// that includes it (`--offline --deezer`) reaches it over the network, and
/// that is the only request such a run makes.
pub async fn run_match_offline(
    pool: &SqlitePool,
    options: MatchOptions,
    index_dir: &std::path::Path,
) -> Result<MatchSummary> {
    let paths = IndexPaths::from_dir(index_dir);
    let mut musicbrainz_index: Option<LocalIndex> = None;
    let mut discogs_index: Option<LocalIndex> = None;

    for provider in options.policy.order() {
        // Deezer has no dump, so no index to open.
        let Some(kind) = IndexKind::for_provider(provider) else {
            continue;
        };
        let index = LocalIndex::open(&paths, kind)
            .await
            .with_context(|| format!("offline matching needs the {kind} index"))?;
        match kind {
            IndexKind::MusicBrainz => musicbrainz_index = Some(index),
            IndexKind::Discogs => discogs_index = Some(index),
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
        DeezerAdapter::new,
    )
    .await
}

pub async fn run_with_policy<M, D, Z>(
    pool: &SqlitePool,
    options: &MatchOptions,
    make_musicbrainz: impl Fn() -> Result<M>,
    make_discogs: impl Fn() -> Result<D>,
    make_deezer: impl Fn() -> Result<Z>,
) -> Result<MatchSummary>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
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
                    // A stale match is refreshed from the provider that made it,
                    // and Deezer is contacted only in a run that asks for it.
                    if accepted == Some(ProviderKind::Deezer)
                        && !options.policy.allows(ProviderKind::Deezer)
                    {
                        outln!(
                            "\nSkipping local release {}: its stale deezer match needs --deezer to refresh",
                            release.id
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
        deezer: OnceCell::new(),
        discogs_disabled: RefCell::new(None),
        make_musicbrainz: &make_musicbrainz,
        make_discogs: &make_discogs,
        make_deezer: &make_deezer,
    };
    // A MusicBrainz adapter that cannot be built is a configuration error, not
    // an outage: every release would hit it again, and a Discogs fallback would
    // look like a recovery from a stage that never ran. Fail once, up front.
    // Explicit review works offline from saved reviews, so it builds lazily.
    let needs = |provider| {
        !options.review
            && selected.iter().any(|release| {
                refresh_source(options, &statuses, &existing_matches, release.id)
                    .map_or(options.policy, ProviderPolicy::Only)
                    .allows(provider)
            })
    };
    if needs(ProviderKind::MusicBrainz)
        && let Err(error) = clients.musicbrainz()
    {
        bail!("MusicBrainz is unusable: {}", Clean(&format!("{error:#}")));
    }
    // Deezer needs no credential either, so a client that cannot be built is a
    // configuration error too.
    if needs(ProviderKind::Deezer)
        && let Err(error) = clients.deezer()
    {
        bail!("Deezer is unusable: {}", Clean(&format!("{error:#}")));
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
async fn match_release<M, D, Z>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    statuses: &HashMap<i64, String>,
    existing_matches: &HashMap<i64, Option<ProviderKind>>,
    clients: &Clients<'_, M, D, Z>,
    transcript: &mut Transcript,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
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
            match review_saved_release(pool, release, saved, options.dry_run, &guard, clients).await
            {
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
        || options.policy.is_explicit()
        || statuses.get(&release_id).map(String::as_str) != Some(store::STATUS_STALE)
    {
        return None;
    }
    existing_matches
        .get(&release_id)
        .and_then(|provider| *provider)
}

/// Whether a stage failed for a reason that says nothing about the release.
///
/// A later candidate was then never compared against what this stage would
/// have found, so it must be reviewed rather than accepted. Failed lookups
/// that discovery worked around count too: the release that failed to load may
/// have been the right one.
fn failed_transiently(attempt: &ProviderAttempt) -> bool {
    attempt
        .error
        .iter()
        .chain(&attempt.partial_failures)
        .any(ProviderError::is_transient)
}

/// The first later stage whose best candidate passes its own gates.
///
/// Returned with the first stage before it that failed transiently, if any:
/// the stage settles the release only without one (see [`settlement`]), and is
/// held for review otherwise. Stages are indexes into `attempts`.
fn first_settling_fallback(attempts: &[ProviderAttempt]) -> Option<(usize, Option<usize>)> {
    let stage = (1..attempts.len()).find(|&stage| {
        attempts[stage]
            .evaluations
            .first()
            .is_some_and(|evaluation| evaluation.auto_accept)
    })?;
    let failed = attempts[..stage].iter().position(failed_transiently);
    Some((stage, failed))
}

/// Starts a line in the transcript about a primary-versus-fallback disagreement
/// that the files settled. It is printed whether or not the fallback is
/// accepted, and is never a review reason.
const SETTLED_NOTE_PREFIX: &str = "fallback conflict settled by the files: ";

/// Says that a fallback which would have settled is held for review, because
/// the stage `failed` (MusicBrainz, or Discogs before Deezer) failed transiently.
fn transient_note(failed: ProviderKind) -> String {
    format!(
        "{} stage failed transiently; fallback candidates need review",
        failed.label()
    )
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

/// Says why a release is matched a second time, with the directory name.
const DIRECTORY_PASS_NOTE: &str =
    "the tags alone did not settle the release; matched again with the directory name";

/// What the provider stages produced for one release, or how it ended early.
enum Gathered {
    /// Every stage the policy ran, primary first; none when nothing was tried.
    ///
    /// A stage that failed is still an attempt, with its error recorded.
    Attempts(Vec<ProviderAttempt>),
    /// The release is done before any decision: nothing was attempted.
    Ended(Outcome),
}

/// How gathered attempts settle a release without a person.
enum Settlement {
    /// The primary provider's best candidate passed its own gates.
    Primary,
    /// The best candidate of the later stage `stage` (an index into the
    /// attempts) passed its own gates and was compared with the candidates of
    /// every stage before it; any conflicts in the check still stop an
    /// automatic accept.
    Fallback {
        stage: usize,
        check: matcher::FallbackCheck,
    },
}

impl Settlement {
    /// Whether the release is accepted at once rather than reviewed or queued.
    ///
    /// A fallback is accepted only when nothing conflicts and no person
    /// selected a release. `--review` accepts nothing on its own.
    fn is_automatic(&self, has_direct: bool, review: bool) -> bool {
        !review
            && match self {
                Self::Primary => true,
                Self::Fallback { check, .. } => check.conflicts.is_empty() && !has_direct,
            }
    }
}

/// Whether `attempts` settle `release` through the primary or a later stage.
///
/// Pure: the one place the settling conditions are written. A primary settles
/// when its best candidate auto-accepts. Otherwise the first later stage whose
/// best candidate auto-accepts settles when no stage before it failed
/// transiently ([`first_settling_fallback`]), with its check against the
/// candidates of every stage before it: Deezer is checked against MusicBrainz
/// and Discogs. Whether an observed release ID was resolved comes from the
/// MusicBrainz attempt.
fn settlement(
    release: &LocalRelease,
    attempts: &[ProviderAttempt],
    has_direct: bool,
) -> Option<Settlement> {
    let primary = attempts.first()?;
    if primary.settled(has_direct) {
        return Some(Settlement::Primary);
    }
    let (stage, None) = first_settling_fallback(attempts)? else {
        return None;
    };
    let fallback = &attempts[stage];
    let (candidate, evaluation) = (fallback.candidates.first()?, fallback.evaluations.first()?);
    // One earlier stage, the usual case, lends its candidates as they are.
    let earlier: Cow<'_, [Candidate]> = match &attempts[..stage] {
        [only] => Cow::Borrowed(&only.candidates),
        several => Cow::Owned(
            several
                .iter()
                .flat_map(|attempt| attempt.candidates.iter().cloned())
                .collect(),
        ),
    };
    let embedded_release_resolved = attempts
        .iter()
        .find(|attempt| attempt.provider == ProviderKind::MusicBrainz)
        .is_some_and(|attempt| attempt.embedded_release_resolved);
    let mut check = matcher::fallback_check(
        release,
        &earlier,
        candidate,
        embedded_release_resolved,
        evaluation.relies_on_unlisted_lengths(),
    );
    // As with an observed MusicBrainz release ID, a Discogs ID the files carry
    // that no Discogs stage resolved (it may not have run) means Deezer cannot
    // settle the edition. Discogs and MusicBrainz stages are judged as before.
    if fallback.provider == ProviderKind::Deezer
        && release.embedded_discogs_id.is_some()
        && !attempts.iter().any(|attempt| {
            attempt.provider == ProviderKind::Discogs && attempt.embedded_release_resolved
        })
    {
        check.conflicts.push(
            "an observed Discogs release ID was not resolved, so another provider cannot settle the edition"
                .to_string(),
        );
    }
    Some(Settlement::Fallback { stage, check })
}

/// Whether the stages so far settle `release` without a person, so no later
/// stage needs to run. A run with `--review` stops at the same stage.
fn stages_settle(release: &LocalRelease, attempts: &[ProviderAttempt], has_direct: bool) -> bool {
    settlement(release, attempts, has_direct)
        .is_some_and(|settlement| settlement.is_automatic(has_direct, false))
}

/// The line that says a later stage failed too. With only MusicBrainz and
/// Discogs, the later stage is just "the fallback stage"; once Deezer can be
/// in the chain, the stage is named.
fn fallback_failure_line(failed: ProviderKind, attempts: usize) -> String {
    if failed == ProviderKind::Discogs && attempts == 2 {
        "the fallback stage also failed; the release outcome is a failure".to_string()
    } else {
        format!(
            "the {} stage also failed; the release outcome is a failure",
            failed.label()
        )
    }
}

/// Why the Deezer stage cannot change the outcome, when it cannot.
///
/// Settlement takes the first later stage that auto-accepts, conflicted or not,
/// so a Deezer candidate is never the one accepted once an earlier stage has
/// such a candidate. After a transient failure of an earlier stage a Deezer
/// candidate would only be held for review.
fn deezer_skip_reason(attempts: &[ProviderAttempt]) -> Option<String> {
    if let Some((stage, _)) = first_settling_fallback(attempts) {
        return Some(format!(
            "the {} candidate already passes its gates",
            attempts[stage].provider.label()
        ));
    }
    attempts
        .iter()
        .find(|attempt| failed_transiently(attempt))
        .map(|attempt| {
            format!(
                "the {} stage failed transiently, so a Deezer candidate would need review",
                attempt.provider.label()
            )
        })
}

/// Why the next stage runs: what the stage before it left open, with the first
/// stage called the primary.
fn fallback_reason(attempts: &[ProviderAttempt]) -> String {
    let Some(last) = attempts.last() else {
        return "no earlier stage ran".to_string();
    };
    let stage = if attempts.len() == 1 {
        "primary".to_string()
    } else {
        last.provider.label().to_string()
    };
    if let Some(error) = &last.error {
        format!("{stage} stage failed: {error}")
    } else if last.candidates.is_empty() {
        format!("{stage} stage found no candidates")
    } else {
        format!("{stage} candidates did not settle the edition")
    }
}

/// Matches one release against the providers and records the decision.
///
/// An automatic match (no `--review`, no direct release) never reads the
/// directory name in a single pass. When the name adds something to the tags
/// (see [`discovery::directory_adds_anything`]), it runs in two passes. The
/// first ignores the directory name beyond filling a missing or junk tag, and
/// its output is held. When its attempts settle the release (see
/// [`settlement`]), it is accepted from them and the name is never read.
/// Otherwise the first pass is dropped, apart from run-wide notices, the pass
/// note is written, and the release is gathered again with the directory name
/// and decided from that. When the name adds nothing, one pass runs on the
/// tags alone. `--review` and a direct selection run once with the directory
/// name.
///
/// A first pass that ends the release early ([`Gathered::Ended`], such as a
/// stale Discogs match with no token) ends it there, with no second pass.
///
/// # Errors
///
/// When the release's stored state cannot be written, or an interactive
/// `--review` prompt fails to read or write. Provider failures are reported in
/// the transcript and decided as [`Outcome::Failed`].
#[expect(
    clippy::too_many_arguments,
    reason = "one release's whole decision; the arguments have no natural grouping"
)]
async fn process_release<M, D, Z>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    policy: ProviderPolicy,
    clients: &Clients<'_, M, D, Z>,
    refreshing: bool,
    guard: &MatchGuard,
    transcript: &mut Transcript,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    let direct = options
        .direct_release
        .clone()
        .filter(|id| policy.allows(id.provider()));
    let has_direct = direct.is_some();
    // The release the last pass matches: the one with the directory reading,
    // unless an automatic match found nothing in the name to read.
    let mut subject = release;
    let tags_only = (!options.review && !has_direct).then(|| LocalRelease {
        reads_directory: false,
        ..release.clone()
    });

    // The directory name is a second reading, so an automatic match tries the
    // tags alone first and keeps that result when it settles the release.
    if let Some(tags_only) = &tags_only {
        if !discovery::directory_adds_anything(release) {
            subject = tags_only;
        } else {
            let mut held = Transcript::held();
            match gather_attempts(
                tags_only,
                policy,
                direct.clone(),
                clients,
                refreshing,
                &mut held,
            )
            .await
            {
                Gathered::Ended(outcome) => {
                    transcript.append(held);
                    return Ok(outcome);
                }
                Gathered::Attempts(attempts) => {
                    let settles =
                        settlement(tags_only, &attempts, has_direct).is_some_and(|settlement| {
                            settlement.is_automatic(has_direct, options.review)
                        });
                    if settles {
                        transcript.append(held);
                        return act_on_attempts(
                            pool, tags_only, options, has_direct, clients, guard, attempts,
                            transcript,
                        )
                        .await;
                    }
                }
            }
            transcript.append(held.into_notices());
            outln_to!(transcript, "  {DIRECTORY_PASS_NOTE}");
        }
    }

    match gather_attempts(subject, policy, direct, clients, refreshing, transcript).await {
        Gathered::Ended(outcome) => Ok(outcome),
        Gathered::Attempts(attempts) => {
            act_on_attempts(
                pool, subject, options, has_direct, clients, guard, attempts, transcript,
            )
            .await
        }
    }
}

/// Runs the provider stages for `release` in policy order.
///
/// The stages and what they report (promotions, notes, stage errors, skipped
/// fallbacks) are written to `transcript`.
async fn gather_attempts<M, D, Z>(
    release: &LocalRelease,
    policy: ProviderPolicy,
    direct: Option<ReleaseId>,
    clients: &Clients<'_, M, D, Z>,
    refreshing: bool,
    transcript: &mut Transcript,
) -> Gathered
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    let mut attempts: Vec<ProviderAttempt> = Vec::new();
    let mut fallback_skip: Option<String> = None;
    // The attempt whose rejected credential switched Discogs off, if any.
    let mut switched_off: Option<usize> = None;

    let order = policy.order();
    for (index, provider) in order.iter().copied().enumerate() {
        if index > 0 {
            if stages_settle(release, &attempts, direct.is_some()) {
                break;
            }
            // Deezer is the optional last stage: its requests are spared when
            // it cannot change the outcome. Discogs always runs.
            if provider == ProviderKind::Deezer
                && let Some(reason) = deezer_skip_reason(&attempts)
            {
                outln_to!(transcript, "  Deezer fallback: skipped: {}", Clean(&reason));
                break;
            }
            outln_to!(
                transcript,
                "  {} fallback: {}",
                provider.label(),
                Clean(&fallback_reason(&attempts))
            );
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
                    // A later stage (Deezer) may still settle the release.
                    continue;
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
                            return Gathered::Ended(Outcome::Skipped);
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
                            notice_to!(
                                transcript,
                                "  Disabling Discogs fallback for the rest of this run"
                            );
                        }
                        continue;
                    }
                };
                let attempt = attempt_provider(client, release, direct.clone()).await;
                // A rejected credential rejects every later release too.
                if let Some(error) = attempt.error.as_ref().filter(|error| error.is_auth()) {
                    clients.disable_discogs(error.clone());
                    switched_off = Some(attempts.len());
                    notice_to!(transcript, "  Disabling Discogs for the rest of this run");
                }
                attempts.push(attempt);
            }
            ProviderKind::Deezer => {
                let client = match clients.deezer() {
                    Ok(client) => client,
                    Err(error) => {
                        attempts.push(ProviderAttempt::failed(
                            ProviderKind::Deezer,
                            provider_error_from(ProviderKind::Deezer, error),
                        ));
                        continue;
                    }
                };
                attempts.push(attempt_provider(client, release, direct.clone()).await);
            }
        }
    }

    // An explicit Discogs policy records a disabled Discogs as a failed attempt
    // with the same error, so a later pass reports it again by itself.
    let reported_again = policy.is_explicit() && !refreshing;
    for (index, attempt) in attempts.iter().enumerate() {
        print_promotion(attempt, transcript);
        for note in &attempt.notes {
            eprintln_to!(transcript, "  note ({}): {}", attempt.provider, Clean(note));
        }
        if let Some(error) = &attempt.error {
            // Only the error that switched Discogs off can be a notice, so a
            // discarded pass still owes it to the run. Any other attempt, and
            // the copy a later pass makes of this one, is an ordinary line.
            let line = format_args!(
                "  {} stage error: {}",
                attempt.provider,
                Clean(&error.to_string())
            );
            if switched_off == Some(index) && !reported_again {
                transcript.notice(line);
            } else {
                transcript.err(line);
            }
        }
    }
    if let Some(reason) = &fallback_skip {
        outln_to!(transcript, "  {}", Clean(reason));
    }

    Gathered::Attempts(attempts)
}

/// Accepts, reviews, queues or fails `release` from its gathered `attempts`.
///
/// `release` is the one the attempts were gathered for.
#[expect(
    clippy::too_many_arguments,
    reason = "one release's whole decision; the arguments have no natural grouping"
)]
async fn act_on_attempts<M, D, Z>(
    pool: &SqlitePool,
    release: &LocalRelease,
    options: &MatchOptions,
    has_direct: bool,
    clients: &Clients<'_, M, D, Z>,
    guard: &MatchGuard,
    mut attempts: Vec<ProviderAttempt>,
    transcript: &mut Transcript,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    let verdict = settlement(release, &attempts, has_direct);

    // A settled primary result needed no fallback, so no later stage was asked.
    if let Some(found @ Settlement::Primary) = &verdict {
        if !found.is_automatic(has_direct, options.review) {
            return review_attempts(pool, release, &attempts, guard, options.dry_run, clients)
                .await;
        }

        return accept(
            pool,
            release,
            attempts[0].provider,
            &attempts[0].candidates[0],
            &attempts[0].evaluations[0],
            AcceptanceMethod::Automatic,
            options.dry_run,
            guard,
            transcript,
        )
        .await;
    }

    // A fallback that would have settled is held for review, and says why.
    if let Some((stage, Some(failed))) = first_settling_fallback(&attempts) {
        let note = transient_note(attempts[failed].provider);
        outln_to!(transcript, "  {note}");
        attempts[stage].notes.push(note);
    }

    // A fallback candidate is accepted only when it settles its own gates and
    // does not contradict the local edition or a plausible earlier candidate.
    if let Some(found @ Settlement::Fallback { stage, check }) = &verdict {
        let stage = *stage;
        if found.is_automatic(has_direct, options.review) {
            for earlier in &attempts[..stage] {
                if let Some(error) = &earlier.error {
                    outln_to!(
                        transcript,
                        "  recovered the release from a failed {} stage: {}",
                        earlier.provider.label(),
                        Clean(&error.to_string())
                    );
                }
            }
            for line in &check.settled {
                outln_to!(transcript, "  {SETTLED_NOTE_PREFIX}{}", Clean(line));
            }
            let fallback = &attempts[stage];
            return accept(
                pool,
                release,
                fallback.provider,
                &fallback.candidates[0],
                &fallback.evaluations[0],
                AcceptanceMethod::Automatic,
                options.dry_run,
                guard,
                transcript,
            )
            .await;
        }
        // What the files settled is reported, as in the accept above, but it is
        // no reason for the review, so it stays out of the stored reasons.
        for line in &check.settled {
            outln_to!(transcript, "  {SETTLED_NOTE_PREFIX}{}", Clean(line));
        }
        for conflict in &check.conflicts {
            outln_to!(transcript, "  fallback conflict: {}", Clean(conflict));
        }
        attempts[stage]
            .notes
            .extend(check.conflicts.iter().cloned());
    }

    let has_candidates = attempts
        .iter()
        .any(|attempt| !attempt.candidates.is_empty());
    let failed_fallback = attempts
        .iter()
        .skip(1)
        .find(|attempt| attempt.error.is_some());

    if has_candidates {
        if options.dry_run {
            for attempt in &attempts {
                print_decision(attempt, transcript);
            }
            return Ok(Outcome::Queued);
        }
        if options.review {
            return review_attempts(pool, release, &attempts, guard, options.dry_run, clients)
                .await;
        }
        queue_attempts(pool, release, &attempts, guard, transcript).await?;
        return Ok(Outcome::Queued);
    }

    // No usable candidates. A completed-empty stage means no match; an
    // unrecovered attempted-stage failure must not be disguised as one.
    if let Some(error) = attempts.iter().find_map(|attempt| attempt.error.clone()) {
        eprintln_to!(transcript, "  {}", Clean(&error.to_string()));
        if let Some(failed) = failed_fallback {
            eprintln_to!(
                transcript,
                "  {}",
                fallback_failure_line(failed.provider, attempts.len())
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
    if !evaluation.can_apply(release, candidate) {
        outln_to!(
            transcript,
            "  cannot accept {provider} release {}: the track mapping is invalid or incomplete ({})",
            candidate.release.id,
            Clean(&candidate.mapping.notes.join("; "))
        );
        return Ok(Outcome::Skipped);
    }

    if dry_run {
        print_candidate_for(provider, candidate, evaluation, transcript);
        outln_to!(
            transcript,
            "  would accept {provider} release {} (score {:.3}) as {}",
            candidate.release.id.storage_id(),
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

/// The candidates and evaluations of saved reviews as the review prompt takes
/// them, one group per provider, each evaluation as it was decided and stored.
fn review_groups(
    saved: &[store::StoredReview],
) -> Vec<(ProviderKind, Vec<Candidate>, Vec<Evaluation>)> {
    saved
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
        .collect()
}

async fn review_saved_release<M, D, Z>(
    pool: &SqlitePool,
    release: &LocalRelease,
    saved: Vec<store::StoredReview>,
    dry_run: bool,
    guard: &MatchGuard,
    clients: &Clients<'_, M, D, Z>,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    let mut groups = review_groups(&saved);

    print_release_header(release);
    for (provider, candidates, evaluations) in &groups {
        for (index, (candidate, evaluation)) in
            candidates.iter().zip(evaluations.iter()).enumerate()
        {
            print_candidate(release, *provider, index, candidate, evaluation, None);
        }
    }

    match review_prompt(release, &mut groups, clients).await? {
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

async fn review_attempts<M, D, Z>(
    pool: &SqlitePool,
    release: &LocalRelease,
    attempts: &[ProviderAttempt],
    guard: &MatchGuard,
    dry_run: bool,
    clients: &Clients<'_, M, D, Z>,
) -> Result<Outcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
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

    match review_prompt(release, &mut groups, clients).await? {
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

/// Sorts `candidates` into the order they are judged in, then lets
/// [`prefer_complete`] correct it unless an exact edition was selected.
fn rank_candidates(
    local: &LocalRelease,
    candidates: &mut [Candidate],
    exact_id: Option<&ReleaseId>,
) -> Reranking {
    candidates.sort_by(|a, b| candidate_order(exact_id, a, b));
    if exact_id.is_some() {
        return Reranking::default();
    }
    prefer_complete(local, candidates)
}

/// Moves the first candidate that lists exactly the files, and is no worse a
/// reference for them, to the front when the first candidate's mapping is
/// incomplete.
///
/// Scores average over the local tracks, so an edition with bonus tracks ties
/// with the one that lists exactly the files and can sort first, yet its mapping
/// is incomplete and can be applied only when the files are its first tracks
/// (see [`matcher::leading_tracks`]), and then only with the warning that it
/// lists more. Only the first candidate decides, so the complete edition is
/// ranked ahead of it, unless it is a worse reference
/// ([`matcher::worse_reference`]): then the first candidate stays first, and the
/// refusals are returned, so that the run can say so and
/// [`fall_back_to_refused_exact`] can undo the preference if it would cost an accept. This is a pass over the sorted
/// list rather than part of [`candidate_order`], because "within the score gap"
/// is not transitive and a comparator built on it is not a total order. The
/// others keep their order, and the promoted candidate still has to pass every
/// gate.
fn prefer_complete(local: &LocalRelease, candidates: &mut [Candidate]) -> Reranking {
    let Some(first) = candidates.first() else {
        return Reranking::default();
    };
    if first.mapping.complete {
        return Reranking::default();
    }
    let mut refusal = None;
    let mut refused = Vec::new();
    let mut promoted = None;
    for (offset, other) in candidates.iter().enumerate().skip(1) {
        if !matcher::lists_exactly_the_files_shape(local, first, other) {
            continue;
        }
        match matcher::worse_reference(local, first, other) {
            None => {
                promoted = Some(offset);
                break;
            }
            Some(reason) => {
                refused.push(other.release.id.clone());
                refusal.get_or_insert_with(|| Refusal {
                    first: first.release.id.clone(),
                    exact: other.release.id.clone(),
                    reason,
                });
            }
        }
    }
    let Some(index) = promoted else {
        return Reranking {
            promotion: None,
            refusal,
            refused,
        };
    };
    let previous = first.release.id.clone();
    candidates[..=index].rotate_right(1);
    Reranking {
        promotion: Some(Promotion {
            promoted: candidates[0].release.id.clone(),
            previous,
            fallback: false,
        }),
        refusal,
        refused,
    }
}

/// Evaluates every candidate in its final order, so that each evaluation stays
/// paired with its candidate whatever moved it.
fn evaluate_candidates(
    local: &LocalRelease,
    candidates: &[Candidate],
    evidence: &HashMap<ReleaseId, DiscoveryEvidence>,
    direct: bool,
) -> Vec<Evaluation> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let evidence = evidence
                .get(&candidate.release.id)
                .cloned()
                .unwrap_or_default();
            let runner_up = if index == 0 { candidates.get(1) } else { None };
            if direct {
                matcher::evaluate(local, candidate, runner_up, &evidence)
            } else {
                matcher::evaluate_for_matching(local, candidate, candidates, &evidence)
            }
        })
        .collect()
}

/// Moves the first exact edition that [`prefer_complete`] refused, for any
/// reason, to the front, after the candidate ranked first turned out not to be
/// accepted. `refused` lists the refused editions in rank order. Done at most
/// once.
///
/// The gate only decides which edition is preferred when the preferred one is
/// accepted: keeping a first candidate ahead of a worse exact edition, or
/// promoting a better one past it, must never cost an accept that ranking the
/// exact edition first would have made. A first candidate that fails another
/// gate (a close alternative, an edition qualifier, a provider conflict) would
/// send the release to review, or on to a worse listing of the other provider,
/// where the refused edition could have been accepted.
fn fall_back_to_refused_exact(
    candidates: &mut [Candidate],
    refused: &[ReleaseId],
) -> Option<Promotion> {
    let first = candidates.first()?;
    let index = candidates
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, other)| refused.contains(&other.release.id).then_some(index))?;
    let previous = first.release.id.clone();
    candidates[..=index].rotate_right(1);
    Some(Promotion {
        promoted: candidates[0].release.id.clone(),
        previous,
        fallback: true,
    })
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

/// The providers of the review groups, in the order they first appear.
fn group_providers(
    groups: &[(ProviderKind, Vec<Candidate>, Vec<Evaluation>)],
) -> Vec<ProviderKind> {
    let mut providers = Vec::new();
    for (provider, _, _) in groups {
        if !providers.contains(provider) {
            providers.push(*provider);
        }
    }
    providers
}

/// The `[i]d` question, numbering each provider: `provider [1]=musicbrainz
/// [2]=discogs > `.
fn provider_prompt(providers: &[ProviderKind]) -> String {
    let choices: Vec<String> = providers
        .iter()
        .enumerate()
        .map(|(index, provider)| format!("[{}]={provider}", index + 1))
        .collect();
    format!("provider {} > ", choices.join(" "))
}

/// The `[a]ccept` question, which numbers the review groups. Two groups keep
/// the long-standing text; more name their providers like the `[i]d` question.
fn group_prompt(groups: &[(ProviderKind, Vec<Candidate>, Vec<Evaluation>)]) -> String {
    if groups.len() == 2 {
        return "provider [1]=first [2]=second > ".to_string();
    }
    let providers: Vec<ProviderKind> = groups.iter().map(|(provider, _, _)| *provider).collect();
    provider_prompt(&providers)
}

/// The review group a reply to the `[a]ccept` question picks, if it picks one.
///
/// Two groups keep the long-standing reading: a number, clamped, with the first
/// group for anything else. With more groups a number picks that group and a
/// provider name (any case) picks that provider's first group; any other reply
/// picks none.
fn chosen_group(
    groups: &[(ProviderKind, Vec<Candidate>, Vec<Evaluation>)],
    reply: &str,
) -> Option<usize> {
    if groups.len() == 2 {
        return Some(reply.parse::<usize>().unwrap_or(1).clamp(1, groups.len()) - 1);
    }
    if let Ok(number) = reply.parse::<usize>() {
        return (1..=groups.len()).contains(&number).then(|| number - 1);
    }
    let provider = ProviderKind::parse(reply)?;
    groups.iter().position(|(group, _, _)| *group == provider)
}

/// The provider a reply names by number or by name; the first one otherwise.
fn chosen_provider(providers: &[ProviderKind], reply: &str) -> ProviderKind {
    reply
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| providers.get(index).copied())
        .or_else(|| ProviderKind::parse(reply).filter(|provider| providers.contains(provider)))
        .or_else(|| providers.first().copied())
        .unwrap_or(ProviderKind::MusicBrainz)
}

/// Fetches the release a person entered, building its client first if needed.
///
/// A saved review contacts no provider until an ID is entered, so the client
/// comes from the run's own factories. MusicBrainz and Discogs come from the
/// network adapter or, with `--offline`, the local index; Deezer always comes
/// from the network. A client that cannot be built comes back as a
/// [`ProviderError`] (a missing token, say), which the prompt prints before
/// asking again.
async fn fetch_entered<M, D, Z>(
    provider: ProviderKind,
    id: ReleaseId,
    clients: &Clients<'_, M, D, Z>,
) -> Result<RemoteRelease, ProviderError>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    match provider {
        ProviderKind::MusicBrainz => {
            clients
                .musicbrainz()
                .map_err(|error| provider_error_from(provider, error))?
                .fetch_release(id)
                .await
        }
        ProviderKind::Discogs => {
            clients
                .discogs()
                .map_err(|error| provider_error_from(provider, error))?
                .fetch_release(id)
                .await
        }
        ProviderKind::Deezer => {
            clients
                .deezer()
                .map_err(|error| provider_error_from(provider, error))?
                .fetch_release(id)
                .await
        }
    }
}

async fn review_prompt<M, D, Z>(
    local: &LocalRelease,
    groups: &mut Vec<(ProviderKind, Vec<Candidate>, Vec<Evaluation>)>,
    clients: &Clients<'_, M, D, Z>,
) -> Result<ReviewOutcome>
where
    M: MetadataProvider,
    D: MetadataProvider,
    Z: MetadataProvider,
{
    loop {
        let action = prompt("\n[a]ccept  [i]d  [s]kip  [q]uit > ")?;
        match action.as_str() {
            "a" | "" => {
                let group_index = if groups.len() > 1 {
                    let choice = prompt(&group_prompt(groups))?;
                    let Some(group_index) = chosen_group(groups, &choice) else {
                        outln!("invalid provider choice: {}", Clean(&choice));
                        continue;
                    };
                    group_index
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

                if !evaluation.can_apply(local, candidate) {
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

                // With more than two groups the providers were chosen by name,
                // so say which release is about to be accepted.
                if groups.len() > 2 {
                    outln!("{provider} {}", Clean(&candidate.release.id.storage_id()));
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
                let providers = group_providers(groups);
                let provider = if providers.len() > 1 {
                    let choice = prompt(&provider_prompt(&providers))?;
                    chosen_provider(&providers, &choice)
                } else {
                    groups[0].0
                };
                let raw = prompt("release ID > ")?;
                let id = match crate::review::parse_entered(provider, &raw) {
                    Ok(id) => id,
                    Err(error) => {
                        outln!("invalid release ID: {}", Clean(&error.to_string()));
                        continue;
                    }
                };
                match fetch_entered(provider, id, clients).await {
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

/// Says that a complete edition was ranked before the incomplete one that
/// sorted first, or that one was kept behind it as a worse reference, so an
/// audit can find the releases this changed.
fn print_promotion(attempt: &ProviderAttempt, transcript: &mut Transcript) {
    if let Some(Promotion {
        promoted,
        previous,
        fallback: false,
    }) = &attempt.promotion
    {
        outln_to!(
            transcript,
            "  ranked {} before {}: it lists exactly the files",
            Clean(&promoted.to_string()),
            Clean(&previous.to_string())
        );
    }
    // The refusal is said when no later edition was promoted past it: nothing
    // moved, or the fallback is about to put the first candidate that was kept
    // behind the edition that was refused.
    let nothing_promoted = match (&attempt.promotion, &attempt.refusal) {
        (None, _) => true,
        (Some(promotion), Some(refusal)) => {
            promotion.fallback && promotion.previous == refusal.first
        }
        (Some(_), None) => false,
    };
    if let Some(Refusal {
        first,
        exact,
        reason,
    }) = &attempt.refusal
        && nothing_promoted
    {
        outln_to!(
            transcript,
            "  kept {} first: {} lists exactly the files but {}",
            Clean(&first.to_string()),
            Clean(&exact.to_string()),
            Clean(reason)
        );
    }
    if let Some(Promotion {
        promoted,
        previous,
        fallback: true,
    }) = &attempt.promotion
    {
        outln_to!(
            transcript,
            "  ranked {} before {}: it lists exactly the files and {} is not accepted",
            Clean(&promoted.to_string()),
            Clean(&previous.to_string()),
            Clean(&previous.to_string())
        );
    }
}

fn print_decision(attempt: &ProviderAttempt, transcript: &mut Transcript) {
    print_candidates(attempt, transcript);
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

/// Lists an attempt's top candidates without a verdict, for callers that
/// print their own.
fn print_candidates(attempt: &ProviderAttempt, transcript: &mut Transcript) {
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
}

/// Lists the one candidate `accept_candidate` is about to accept; the caller
/// prints the verdict.
fn print_candidate_for(
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
    transcript: &mut Transcript,
) {
    print_candidates(
        &ProviderAttempt {
            provider,
            candidates: vec![candidate.clone()],
            evaluations: vec![evaluation.clone()],
            notes: Vec::new(),
            error: None,
            partial_failures: Vec::new(),
            embedded_release_resolved: false,
            promotion: None,
            refusal: None,
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
            resolve_policy(None, None, false).unwrap(),
            ProviderPolicy::Chain { deezer: false }
        );
        assert_eq!(
            resolve_policy(None, Some(&id), false).unwrap(),
            ProviderPolicy::Only(ProviderKind::MusicBrainz)
        );
        assert_eq!(
            resolve_policy(Some(ProviderKind::Discogs), None, false).unwrap(),
            ProviderPolicy::Only(ProviderKind::Discogs)
        );
        assert_eq!(
            resolve_policy(Some(ProviderKind::MusicBrainz), None, false).unwrap(),
            ProviderPolicy::Only(ProviderKind::MusicBrainz)
        );
        assert_eq!(
            resolve_policy(None, Some(&ReleaseId::Discogs(42)), false).unwrap(),
            ProviderPolicy::Only(ProviderKind::Discogs)
        );
        assert!(resolve_policy(Some(ProviderKind::Discogs), Some(&id), false).is_err());
        assert_eq!(
            resolve_policy(None, None, true).unwrap(),
            ProviderPolicy::Chain { deezer: true }
        );
        assert!(resolve_policy(Some(ProviderKind::Discogs), None, true).is_err());
        assert!(resolve_policy(None, Some(&id), true).is_err());
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
            reads_directory: true,
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
                || -> Result<GatedProvider> { bail!("Deezer is not used in this test") },
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
            || -> Result<RetaggingProvider> { bail!("Deezer is not used in this test") },
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
                Line::Err(_) | Line::Notice(_) => None,
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
                Line::Err(_) | Line::Notice(_) => None,
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
    async fn accept_prints_that_one_odd_length_was_read_as_agreeing() {
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

        // The provider lists the same five titles and lengths, except that its
        // last track is 80 seconds shorter than the file.
        let (_, mut remote) = album_fixture();
        remote.tracklist.truncate(5);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.title = format!("Track {}", index + 1);
            track.duration_secs = Some(180.0);
        }
        remote.tracklist[4].duration_secs = Some(100.0);
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
                Line::Err(_) | Line::Notice(_) => None,
            })
            .collect();
        let unconfirmed = lines
            .iter()
            .position(|line| line.contains("exact edition unconfirmed"))
            .expect("the album scope note");
        assert_eq!(
            lines.get(unconfirmed + 1),
            Some(
                &"    track 'Track 5' is 80.0s longer in the files than the discogs duration; every title and every other length agrees"
            )
        );
    }

    #[tokio::test]
    async fn accept_prints_that_the_tracklist_identified_the_album() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=5)
            .map(|number| {
                scanned_track(
                    &format!("/music/a/{number:02}.flac"),
                    "With Open & Loving Arms",
                    number,
                )
            })
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

        // The provider lists the same five titles and lengths under an album
        // title that words the same title another way.
        let (_, mut remote) = album_fixture();
        remote.title = "With Loving and Open Arms".into();
        remote.tracklist.truncate(5);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.title = format!("Track {}", index + 1);
            track.duration_secs = Some(180.0);
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
                Line::Err(_) | Line::Notice(_) => None,
            })
            .collect();
        let unconfirmed = lines
            .iter()
            .position(|line| line.contains("exact edition unconfirmed"))
            .expect("the album scope note");
        assert_eq!(
            lines.get(unconfirmed + 1),
            Some(
                &"    album title scores 0.680 and artist 1.000, but all 5 track titles and lengths agree in track order"
            )
        );
    }

    #[tokio::test]
    async fn accept_prints_that_the_files_are_the_first_tracks() {
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

        let attempt = attempt_over(&release, vec![edition(10, 6, None)]);
        let (candidate, evaluation) = (&attempt.candidates[0], &attempt.evaluations[0]);
        assert!(!candidate.mapping.complete);
        assert!(evaluation.auto_accept, "{:?}", evaluation.blockers);
        assert!(attempt.settled(false));

        let mut transcript = Transcript::held();
        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            candidate,
            evaluation,
            AcceptanceMethod::Automatic,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        let lines = out_lines(&transcript);
        let unconfirmed = lines
            .iter()
            .position(|line| line.contains("exact edition unconfirmed"))
            .expect("the album scope note");
        assert_eq!(
            lines.get(unconfirmed + 1),
            Some(
                &"    the files are the first 5 of 6 tracks; discogs lists more tracks after the files' last"
            )
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_track_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 5);
    }

    #[test]
    fn the_longer_edition_names_the_exact_one_that_blocks_it() {
        let mut local = ten_files();
        local.tracks[0].title = Some("Track 1".into());
        let attempt = attempt_over(
            &local,
            vec![edition(10, 12, Some(1)), edition(11, 10, Some(1))],
        );
        assert_eq!(ids(&attempt), vec![11, 10]);
        assert!(attempt.evaluations[0].auto_accept);
        assert!(
            attempt.evaluations[1]
                .blockers
                .contains(&"release discogs 11 lists exactly the local tracks".to_string()),
            "{:?}",
            attempt.evaluations[1].blockers
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
        assert!(evaluation.can_apply(&release, &candidate));

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
    fn only_outage_failures_count_as_a_transient_stage_failure() {
        let failed = |error| ProviderAttempt::failed(ProviderKind::MusicBrainz, error);
        let provider = ProviderKind::MusicBrainz;
        assert!(failed_transiently(&failed(ProviderError::Network {
            provider,
            message: "down".into(),
        })));
        assert!(failed_transiently(&failed(ProviderError::RateLimited {
            provider,
            message: "slow down".into(),
            retry_after: None,
        })));
        assert!(failed_transiently(&failed(ProviderError::Unavailable {
            provider,
            retry_after: std::time::Duration::from_secs(5),
        })));
        // A malformed response is deterministic for that release, and starts no cooldown.
        assert!(!failed_transiently(&failed(ProviderError::Malformed {
            provider,
            message: "bad json".into(),
        })));
        assert!(!failed_transiently(&failed(ProviderError::Permanent {
            provider,
            status: Some(400),
            message: "bad".into(),
        })));
    }

    #[test]
    fn worked_around_transient_lookup_failures_count_as_a_transient_stage_failure() {
        let provider = ProviderKind::MusicBrainz;
        let mut attempt = ProviderAttempt::failed(
            provider,
            ProviderError::Network {
                provider,
                message: "unused".into(),
            },
        );
        attempt.error = None;
        assert!(!failed_transiently(&attempt));
        attempt.partial_failures = vec![ProviderError::Malformed {
            provider,
            message: "bad json".into(),
        }];
        assert!(!failed_transiently(&attempt));
        attempt.partial_failures.push(ProviderError::Unavailable {
            provider,
            retry_after: std::time::Duration::from_secs(5),
        });
        assert!(failed_transiently(&attempt));
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

    /// An edition of the fixture album with `count` tracks of 180 seconds, in
    /// the release group `group`.
    fn edition(id: u64, count: usize, group: Option<u64>) -> RemoteRelease {
        let (_, mut remote) = album_fixture();
        remote.id = ReleaseId::Discogs(id);
        remote.group_id = group.map(crate::providers::GroupId::DiscogsMaster);
        remote.tracklist.truncate(count);
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.title = format!("Track {}", index + 1);
            track.duration_secs = Some(180.0);
        }
        remote
    }

    /// The fixture album's first ten files, all 180 seconds long.
    fn ten_files() -> LocalRelease {
        let (mut local, _) = album_fixture();
        local.tracks.truncate(10);
        for track in &mut local.tracks {
            track.duration_secs = Some(180.0);
        }
        local
    }

    fn attempt_over(local: &LocalRelease, releases: Vec<RemoteRelease>) -> ProviderAttempt {
        ProviderAttempt::from_discovery(
            ProviderKind::Discogs,
            local,
            DiscoveryResult {
                releases,
                ..Default::default()
            },
            None,
            false,
        )
    }

    fn ids(attempt: &ProviderAttempt) -> Vec<u64> {
        attempt
            .candidates
            .iter()
            .map(|candidate| match candidate.release.id {
                ReleaseId::Discogs(id) => id,
                ReleaseId::MusicBrainz(_) | ReleaseId::Deezer(_) => unreachable!("a discogs test"),
            })
            .collect()
    }

    fn out_lines(transcript: &Transcript) -> Vec<&str> {
        transcript
            .held
            .iter()
            .flatten()
            .filter_map(|line| match line {
                Line::Out(text) => Some(text.as_str()),
                Line::Err(_) | Line::Notice(_) => None,
            })
            .collect()
    }

    #[test]
    fn an_appended_transcript_replays_its_lines_in_order() {
        let mut first = Transcript::held();
        outln_to!(first, "one");
        let mut second = Transcript::held();
        outln_to!(second, "two");
        eprintln_to!(second, "oops");
        outln_to!(second, "three");
        first.append(second);
        assert_eq!(
            first.held,
            Some(vec![
                Line::Out("one".to_string()),
                Line::Out("two".to_string()),
                Line::Err("oops".to_string()),
                Line::Out("three".to_string()),
            ])
        );
    }

    #[test]
    fn a_discarded_transcript_keeps_only_its_run_wide_notices() {
        let mut pass = Transcript::held();
        outln_to!(pass, "ordinary");
        eprintln_to!(pass, "ordinary error");
        notice_to!(pass, "Discogs is off");
        let mut kept = Transcript::held();
        kept.append(pass.into_notices());
        assert_eq!(
            kept.held,
            Some(vec![Line::Notice("Discogs is off".to_string())])
        );
    }

    #[test]
    fn a_fallback_with_conflicts_or_a_review_run_is_not_automatic() {
        let clean = Settlement::Fallback {
            stage: 1,
            check: matcher::FallbackCheck::default(),
        };
        assert!(clean.is_automatic(false, false));
        assert!(!clean.is_automatic(true, false));
        assert!(!clean.is_automatic(false, true));
        let conflicted = Settlement::Fallback {
            stage: 1,
            check: matcher::FallbackCheck {
                conflicts: vec!["edition differs".to_string()],
                settled: Vec::new(),
            },
        };
        assert!(!conflicted.is_automatic(false, false));
        assert!(Settlement::Primary.is_automatic(false, false));
        assert!(!Settlement::Primary.is_automatic(false, true));
    }

    /// A provider of one kind that offers `release` to every search (none when
    /// it is `None`, or `error` when that is set) and counts its searches.
    struct OneRelease {
        kind: ProviderKind,
        release: Option<RemoteRelease>,
        error: Option<ProviderError>,
        searches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl OneRelease {
        fn new(kind: ProviderKind, release: Option<RemoteRelease>) -> Self {
            Self {
                kind,
                release,
                error: None,
                searches: std::sync::Arc::default(),
            }
        }

        fn searches(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
            self.searches.clone()
        }
    }

    impl MetadataProvider for OneRelease {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        async fn search(&self, _request: SearchRequest) -> Result<SearchPage, ProviderError> {
            self.searches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            let hits: Vec<matcher::SearchSummary> = self
                .release
                .iter()
                .map(|release| matcher::SearchSummary {
                    id: release.id.clone(),
                    title: release.title.clone(),
                    artists: release.artists.clone(),
                    artist_credit: None,
                    year: release.year,
                    country: None,
                    labels: Vec::new(),
                    catalog_numbers: Vec::new(),
                    formats: release.formats.clone(),
                    barcodes: Vec::new(),
                    result_type: "release".into(),
                })
                .collect();
            Ok(SearchPage {
                total: Some(hits.len() as u64),
                hits,
                next_offset: None,
            })
        }

        async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
            self.release
                .clone()
                .filter(|release| release.id == id)
                .ok_or(ProviderError::NotFound {
                    provider: self.kind,
                    id: id.storage_id(),
                })
        }
    }

    #[tokio::test]
    async fn an_entered_id_builds_its_client_only_when_needed() {
        let built = std::cell::Cell::new(0);
        let release = ten_track_edition(ReleaseId::Discogs(42));
        let make_discogs = || {
            built.set(built.get() + 1);
            Ok(OneRelease::new(
                ProviderKind::Discogs,
                Some(release.clone()),
            ))
        };
        let make_none = || -> Result<OneRelease> { bail!("not this one") };
        let clients = Clients {
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
            discogs_disabled: RefCell::new(None),
            make_musicbrainz: &make_none,
            make_discogs: &make_discogs,
            make_deezer: &make_none,
        };
        assert_eq!(built.get(), 0, "nothing is built before an ID is entered");

        for _ in 0..2 {
            let fetched = fetch_entered(ProviderKind::Discogs, ReleaseId::Discogs(42), &clients)
                .await
                .unwrap();
            assert_eq!(fetched.id, ReleaseId::Discogs(42));
        }
        assert_eq!(built.get(), 1, "the client is built once and kept");

        let error = fetch_entered(ProviderKind::MusicBrainz, musicbrainz_id(), &clients)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderError::Configuration { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("not this one"), "{error}");
    }

    /// [`edition`] of the ten files, all ten tracks, under `id`.
    fn ten_track_edition(id: ReleaseId) -> RemoteRelease {
        let mut release = edition(10, 10, None);
        release.id = id;
        release
    }

    /// Runs the MusicBrainz, Discogs and Deezer chain over [`ten_files`] and
    /// returns the attempts, the transcript and each provider's search count.
    async fn run_chain(
        providers: [OneRelease; 3],
    ) -> (Vec<ProviderAttempt>, Transcript, [usize; 3]) {
        let counts = providers.each_ref().map(OneRelease::searches);
        let cells = providers.map(|provider| RefCell::new(Some(provider)));
        let take = |index: usize| {
            cells[index]
                .borrow_mut()
                .take()
                .context("a client was built twice")
        };
        let (make_musicbrainz, make_discogs, make_deezer) = (|| take(0), || take(1), || take(2));
        let clients = Clients {
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
            discogs_disabled: RefCell::new(None),
            make_musicbrainz: &make_musicbrainz,
            make_discogs: &make_discogs,
            make_deezer: &make_deezer,
        };
        let mut transcript = Transcript::held();
        let Gathered::Attempts(attempts) = gather_attempts(
            &ten_files(),
            ProviderPolicy::Chain { deezer: true },
            None,
            &clients,
            false,
            &mut transcript,
        )
        .await
        else {
            panic!("the chain ended before any stage ran");
        };
        let counts = counts.map(|count| count.load(std::sync::atomic::Ordering::SeqCst));
        (attempts, transcript, counts)
    }

    fn musicbrainz_id() -> ReleaseId {
        ReleaseId::MusicBrainz(mbid("76df3287-6cda-33eb-8e9a-044b5e15ffdd"))
    }

    #[tokio::test]
    async fn a_settling_musicbrainz_stage_runs_no_later_stage() {
        let (attempts, _, counts) = run_chain([
            OneRelease::new(
                ProviderKind::MusicBrainz,
                Some(ten_track_edition(musicbrainz_id())),
            ),
            OneRelease::new(
                ProviderKind::Discogs,
                Some(ten_track_edition(ReleaseId::Discogs(42))),
            ),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert_eq!(attempts.len(), 1);
        assert!(counts[0] > 0);
        assert_eq!(counts[1..], [0, 0]);
        assert!(matches!(
            settlement(&ten_files(), &attempts, false),
            Some(Settlement::Primary)
        ));
    }

    #[tokio::test]
    async fn a_settling_discogs_stage_keeps_deezer_out() {
        let (attempts, transcript, counts) = run_chain([
            OneRelease::new(ProviderKind::MusicBrainz, None),
            OneRelease::new(
                ProviderKind::Discogs,
                Some(ten_track_edition(ReleaseId::Discogs(42))),
            ),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert_eq!(attempts.len(), 2);
        assert_eq!(counts[2], 0, "Deezer must not be searched");
        assert!(
            out_lines(&transcript)
                .contains(&"  Discogs fallback: primary stage found no candidates"),
            "{:?}",
            out_lines(&transcript)
        );
        let Some(found @ Settlement::Fallback { stage: 1, .. }) =
            settlement(&ten_files(), &attempts, false)
        else {
            panic!("Discogs did not settle the release");
        };
        assert!(found.is_automatic(false, false));
    }

    #[tokio::test]
    async fn deezer_settles_a_release_neither_earlier_stage_settles() {
        let (attempts, transcript, counts) = run_chain([
            OneRelease::new(ProviderKind::MusicBrainz, None),
            OneRelease::new(ProviderKind::Discogs, None),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert_eq!(attempts.len(), 3);
        assert!(counts.iter().all(|count| *count > 0), "{counts:?}");
        assert!(
            out_lines(&transcript)
                .contains(&"  Deezer fallback: Discogs stage found no candidates"),
            "{:?}",
            out_lines(&transcript)
        );
        let Some(found @ Settlement::Fallback { stage: 2, .. }) =
            settlement(&ten_files(), &attempts, false)
        else {
            panic!("Deezer did not settle the release");
        };
        assert!(found.is_automatic(false, false));
    }

    #[tokio::test]
    async fn a_plausible_contradicting_discogs_candidate_blocks_deezer() {
        // Discogs lists the mono edition: plausible by score, not accepted, and
        // its edition contradicts Deezer's.
        let mut mono = ten_track_edition(ReleaseId::Discogs(42));
        mono.formats = vec!["Vinyl (LP, Album, Mono)".to_string()];
        let (attempts, _, _) = run_chain([
            OneRelease::new(ProviderKind::MusicBrainz, None),
            OneRelease::new(ProviderKind::Discogs, Some(mono)),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert_eq!(attempts.len(), 3);
        assert!(!attempts[1].evaluations[0].auto_accept);
        let Some(Settlement::Fallback { stage: 2, check }) =
            settlement(&ten_files(), &attempts, false)
        else {
            panic!("Deezer's candidate did not pass its own gates");
        };
        // Any conflict stops the automatic accept (`Settlement::is_automatic`).
        assert!(
            check
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("primary candidates describe")),
            "{:?}",
            check.conflicts
        );
    }

    #[tokio::test]
    async fn a_transient_earlier_failure_skips_deezer_but_not_discogs() {
        let mut down = OneRelease::new(ProviderKind::MusicBrainz, None);
        down.error = Some(ProviderError::Network {
            provider: ProviderKind::MusicBrainz,
            message: "HTTP 503".into(),
        });
        let (attempts, transcript, counts) = run_chain([
            down,
            OneRelease::new(ProviderKind::Discogs, None),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert!(
            counts[1] > 0,
            "Discogs still runs after a transient failure"
        );
        assert_eq!(counts[2], 0, "Deezer must not be searched");
        assert_eq!(attempts.len(), 2);
        assert!(
            out_lines(&transcript).contains(
                &"  Deezer fallback: skipped: the MusicBrainz stage failed transiently, so a Deezer candidate would need review"
            ),
            "{:?}",
            out_lines(&transcript)
        );
    }

    #[tokio::test]
    async fn a_discogs_candidate_that_passes_its_gates_skips_deezer_even_when_conflicted() {
        // MusicBrainz lists the mono edition: plausible by score, not accepted,
        // and contradicting Discogs's plain edition, which passes its own gates.
        let mut mono = ten_track_edition(musicbrainz_id());
        mono.formats = vec!["Vinyl (LP, Album, Mono)".to_string()];
        let (attempts, transcript, counts) = run_chain([
            OneRelease::new(ProviderKind::MusicBrainz, Some(mono)),
            OneRelease::new(
                ProviderKind::Discogs,
                Some(ten_track_edition(ReleaseId::Discogs(42))),
            ),
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert_eq!(counts[2], 0, "Deezer must not be searched");
        assert_eq!(attempts.len(), 2);
        let Some(found @ Settlement::Fallback { stage: 1, .. }) =
            settlement(&ten_files(), &attempts, false)
        else {
            panic!("Discogs did not settle the release");
        };
        assert!(!found.is_automatic(false, false), "the conflict holds it");
        assert!(
            out_lines(&transcript).contains(
                &"  Deezer fallback: skipped: the Discogs candidate already passes its gates"
            ),
            "{:?}",
            out_lines(&transcript)
        );
    }

    /// A stored local release of the ten files, as the database holds it.
    async fn stored_ten_files() -> (SqlitePool, LocalRelease, MatchGuard) {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=10)
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
        (pool, release, guard)
    }

    /// Decides `release` from `attempts` as a run with Deezer would.
    async fn decide(
        pool: &SqlitePool,
        release: &LocalRelease,
        guard: &MatchGuard,
        attempts: Vec<ProviderAttempt>,
        dry_run: bool,
    ) -> (Outcome, Transcript) {
        let unused = || -> Result<OneRelease> { bail!("no client is built here") };
        let clients = Clients {
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
            discogs_disabled: RefCell::new(None),
            make_musicbrainz: &unused,
            make_discogs: &unused,
            make_deezer: &unused,
        };
        let options = MatchOptions {
            policy: ProviderPolicy::Chain { deezer: true },
            dry_run,
            ..MatchOptions::default()
        };
        let mut transcript = Transcript::held();
        let outcome = act_on_attempts(
            pool,
            release,
            &options,
            false,
            &clients,
            guard,
            attempts,
            &mut transcript,
        )
        .await
        .unwrap();
        (outcome, transcript)
    }

    /// The attempt of `kind` over `releases`, as discovery would report it.
    fn attempt_of(
        kind: ProviderKind,
        local: &LocalRelease,
        releases: Vec<RemoteRelease>,
    ) -> ProviderAttempt {
        ProviderAttempt::from_discovery(
            kind,
            local,
            DiscoveryResult {
                releases,
                ..Default::default()
            },
            None,
            false,
        )
    }

    #[tokio::test]
    async fn a_transient_discogs_failure_holds_a_deezer_candidate_for_review() {
        let (pool, release, guard) = stored_ten_files().await;
        let attempts = vec![
            attempt_of(ProviderKind::MusicBrainz, &release, Vec::new()),
            ProviderAttempt::failed(
                ProviderKind::Discogs,
                ProviderError::Network {
                    provider: ProviderKind::Discogs,
                    message: "HTTP 503".into(),
                },
            ),
            attempt_of(
                ProviderKind::Deezer,
                &release,
                vec![ten_track_edition(ReleaseId::Deezer(7))],
            ),
        ];
        assert!(attempts[2].settled(false));
        assert_eq!(first_settling_fallback(&attempts), Some((2, Some(1))));
        assert!(settlement(&release, &attempts, false).is_none());
        assert_eq!(
            transient_note(ProviderKind::MusicBrainz),
            "MusicBrainz stage failed transiently; fallback candidates need review"
        );
        assert_eq!(
            transient_note(ProviderKind::Discogs),
            "Discogs stage failed transiently; fallback candidates need review"
        );

        let (outcome, transcript) = decide(&pool, &release, &guard, attempts, false).await;
        assert!(matches!(outcome, Outcome::Queued), "{outcome:?}");
        let lines = out_lines(&transcript);
        assert!(
            lines.contains(&"  Discogs stage failed transiently; fallback candidates need review"),
            "{lines:?}"
        );
        // The note is on the Deezer attempt, so it is among the stored reasons.
        let review = store::load_review(&pool, release.id, ProviderKind::Deezer)
            .await
            .unwrap()
            .expect("the Deezer candidate is queued");
        assert!(
            review.reasons.contains(
                &"Discogs stage failed transiently; fallback candidates need review".to_string()
            ),
            "{:?}",
            review.reasons
        );
        assert!(
            store::load_review(&pool, release.id, ProviderKind::Discogs)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_discogs_auth_failure_is_named_when_deezer_recovers_the_release() {
        let mut broken = OneRelease::new(ProviderKind::Discogs, None);
        broken.error = Some(ProviderError::Authentication {
            provider: ProviderKind::Discogs,
            message: "token rejected".into(),
        });
        let (attempts, transcript, counts) = run_chain([
            OneRelease::new(ProviderKind::MusicBrainz, None),
            broken,
            OneRelease::new(
                ProviderKind::Deezer,
                Some(ten_track_edition(ReleaseId::Deezer(7))),
            ),
        ])
        .await;
        assert!(counts[2] > 0, "an auth failure does not stop Deezer");
        assert_eq!(attempts.len(), 3);
        assert!(matches!(
            settlement(&ten_files(), &attempts, false),
            Some(Settlement::Fallback { stage: 2, .. })
        ));
        let pool = crate::db::connect_in_memory().await.unwrap();
        let guard = MatchGuard {
            source_revision: String::new(),
            accepted_match_id: None,
        };
        let (outcome, decided) = decide(&pool, &ten_files(), &guard, attempts, true).await;
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        let lines = out_lines(&decided);
        assert!(
            lines
                .iter()
                .any(|line| line
                    .starts_with("  recovered the release from a failed Discogs stage: ")),
            "{lines:?} {:?}",
            out_lines(&transcript)
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("failed MusicBrainz stage")),
            "{lines:?}"
        );
    }

    /// The conflict a Deezer stage gets for a Discogs release ID in the tags
    /// that no Discogs stage resolved.
    const UNRESOLVED_DISCOGS_ID: &str = "an observed Discogs release ID was not resolved, so another provider cannot settle the edition";

    fn embedded_conflict(attempts: &[ProviderAttempt], local: &LocalRelease) -> Option<bool> {
        match settlement(local, attempts, false)? {
            Settlement::Fallback { check, .. } => Some(
                check
                    .conflicts
                    .iter()
                    .any(|conflict| conflict == UNRESOLVED_DISCOGS_ID),
            ),
            Settlement::Primary => None,
        }
    }

    #[test]
    fn an_unresolved_embedded_discogs_id_stops_a_deezer_accept_only() {
        let mut local = ten_files();
        local.embedded_discogs_id = Some(5);
        let empty = |kind| attempt_of(kind, &local, Vec::new());
        let deezer = || {
            attempt_of(
                ProviderKind::Deezer,
                &local,
                vec![ten_track_edition(ReleaseId::Deezer(7))],
            )
        };

        // No Discogs stage ran (no token): Deezer cannot settle.
        let attempts = [empty(ProviderKind::MusicBrainz), deezer()];
        assert_eq!(embedded_conflict(&attempts, &local), Some(true));
        let found = settlement(&local, &attempts, false).unwrap();
        assert!(!found.is_automatic(false, false));

        // A Discogs stage that did not resolve it: the same.
        let attempts = [
            empty(ProviderKind::MusicBrainz),
            empty(ProviderKind::Discogs),
            deezer(),
        ];
        assert_eq!(embedded_conflict(&attempts, &local), Some(true));

        // A Discogs stage that resolved it: no such conflict.
        let mut resolved = empty(ProviderKind::Discogs);
        resolved.embedded_release_resolved = true;
        let attempts = [empty(ProviderKind::MusicBrainz), resolved, deezer()];
        assert_eq!(embedded_conflict(&attempts, &local), Some(false));
        assert!(
            settlement(&local, &attempts, false)
                .unwrap()
                .is_automatic(false, false)
        );

        // Without an embedded Discogs ID there is nothing to resolve.
        let mut plain = ten_files();
        plain.embedded_discogs_id = None;
        let attempts = [
            empty(ProviderKind::MusicBrainz),
            attempt_of(
                ProviderKind::Deezer,
                &plain,
                vec![ten_track_edition(ReleaseId::Deezer(7))],
            ),
        ];
        assert_eq!(embedded_conflict(&attempts, &plain), Some(false));

        // The Discogs stage itself, here the release the tags name, is judged as before.
        let attempts = [
            empty(ProviderKind::MusicBrainz),
            attempt_of(
                ProviderKind::Discogs,
                &local,
                vec![ten_track_edition(ReleaseId::Discogs(5))],
            ),
        ];
        assert_eq!(embedded_conflict(&attempts, &local), Some(false));
    }

    #[tokio::test]
    async fn a_missing_discogs_token_with_an_embedded_discogs_id_leaves_deezer_in_review() {
        let mut local = ten_files();
        local.embedded_discogs_id = Some(5);
        let musicbrainz = OneRelease::new(ProviderKind::MusicBrainz, None);
        let deezer = OneRelease::new(
            ProviderKind::Deezer,
            Some(ten_track_edition(ReleaseId::Deezer(7))),
        );
        let (musicbrainz, deezer) = (RefCell::new(Some(musicbrainz)), RefCell::new(Some(deezer)));
        let make_musicbrainz = || musicbrainz.borrow_mut().take().context("built twice");
        let make_discogs =
            || -> Result<OneRelease> { Err(anyhow::Error::new(AdapterError::MissingToken)) };
        let make_deezer = || deezer.borrow_mut().take().context("built twice");
        let clients = Clients {
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
            discogs_disabled: RefCell::new(None),
            make_musicbrainz: &make_musicbrainz,
            make_discogs: &make_discogs,
            make_deezer: &make_deezer,
        };
        let mut transcript = Transcript::held();
        let Gathered::Attempts(attempts) = gather_attempts(
            &local,
            ProviderPolicy::Chain { deezer: true },
            None,
            &clients,
            false,
            &mut transcript,
        )
        .await
        else {
            panic!("the chain ended before any stage ran");
        };
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[1].provider, ProviderKind::Deezer);
        let Some(found) = settlement(&local, &attempts, false) else {
            panic!("Deezer's candidate did not pass its own gates");
        };
        assert!(!found.is_automatic(false, false));

        let pool = crate::db::connect_in_memory().await.unwrap();
        let guard = MatchGuard {
            source_revision: String::new(),
            accepted_match_id: None,
        };
        let (outcome, decided) = decide(&pool, &local, &guard, attempts, true).await;
        assert!(matches!(outcome, Outcome::Queued), "{outcome:?}");
        assert!(
            out_lines(&decided)
                .iter()
                .any(|line| line.contains(UNRESOLVED_DISCOGS_ID)),
            "{:?}",
            out_lines(&decided)
        );
    }

    #[test]
    fn fallback_reasons_name_earlier_stages_by_label() {
        let local = ten_files();
        let musicbrainz = attempt_of(ProviderKind::MusicBrainz, &local, Vec::new());
        assert_eq!(
            fallback_reason(std::slice::from_ref(&musicbrainz)),
            "primary stage found no candidates"
        );
        let discogs = attempt_of(ProviderKind::Discogs, &local, Vec::new());
        assert_eq!(
            fallback_reason(&[musicbrainz, discogs]),
            "Discogs stage found no candidates"
        );
        let failed = ProviderAttempt::failed(
            ProviderKind::Discogs,
            ProviderError::Malformed {
                provider: ProviderKind::Discogs,
                message: "bad json".into(),
            },
        );
        assert!(
            fallback_reason(&[
                attempt_of(ProviderKind::MusicBrainz, &local, Vec::new()),
                failed
            ])
            .starts_with("Discogs stage failed: ")
        );
    }

    #[test]
    fn the_accept_prompt_reads_a_number_or_a_provider_name() {
        let (local, remote) = album_fixture();
        let group = |provider| {
            let candidate = Candidate::new(&local, remote.clone());
            (provider, vec![candidate], Vec::new())
        };
        let three = [
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
            group(ProviderKind::Deezer),
        ];
        assert_eq!(chosen_group(&three, "3"), Some(2));
        assert_eq!(chosen_group(&three, "1"), Some(0));
        assert_eq!(chosen_group(&three, "deezer"), Some(2));
        assert_eq!(chosen_group(&three, "Discogs"), Some(1));
        assert_eq!(chosen_group(&three, "MUSICBRAINZ"), Some(0));
        // Nothing else picks a group, so nothing is silently accepted.
        assert_eq!(chosen_group(&three, ""), None);
        assert_eq!(chosen_group(&three, "0"), None);
        assert_eq!(chosen_group(&three, "4"), None);
        assert_eq!(chosen_group(&three, "second"), None);
        // A provider with no group of its own is not one to pick.
        let two_providers = [
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
            group(ProviderKind::Discogs),
        ];
        assert_eq!(chosen_group(&two_providers, "deezer"), None);
        assert_eq!(chosen_group(&two_providers, "discogs"), Some(1));
        // Two groups read as they always did: clamped, the first otherwise.
        let two = [
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
        ];
        assert_eq!(chosen_group(&two, "2"), Some(1));
        assert_eq!(chosen_group(&two, "9"), Some(1));
        assert_eq!(chosen_group(&two, "0"), Some(0));
        assert_eq!(chosen_group(&two, ""), Some(0));
        assert_eq!(chosen_group(&two, "discogs"), Some(0));
    }

    #[tokio::test]
    async fn a_failed_deezer_stage_is_named_in_the_outcome() {
        let (pool, release, guard) = stored_ten_files().await;
        let failure = |provider| ProviderError::Network {
            provider,
            message: "HTTP 503".into(),
        };
        for (attempts, expected) in [
            (
                vec![
                    attempt_of(ProviderKind::MusicBrainz, &release, Vec::new()),
                    attempt_of(ProviderKind::Discogs, &release, Vec::new()),
                    ProviderAttempt::failed(ProviderKind::Deezer, failure(ProviderKind::Deezer)),
                ],
                "  the Deezer stage also failed; the release outcome is a failure",
            ),
            (
                vec![
                    attempt_of(ProviderKind::MusicBrainz, &release, Vec::new()),
                    ProviderAttempt::failed(ProviderKind::Discogs, failure(ProviderKind::Discogs)),
                ],
                "  the fallback stage also failed; the release outcome is a failure",
            ),
        ] {
            let (outcome, transcript) = decide(&pool, &release, &guard, attempts, true).await;
            assert!(matches!(outcome, Outcome::Failed), "{outcome:?}");
            let errors: Vec<&str> = transcript
                .held
                .iter()
                .flatten()
                .filter_map(|line| match line {
                    Line::Err(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert!(errors.contains(&expected), "{errors:?}");
        }
    }

    #[test]
    fn a_failed_later_stage_is_named_once_the_chain_has_three_stages() {
        // MusicBrainz then Discogs: the long-standing text.
        assert_eq!(
            fallback_failure_line(ProviderKind::Discogs, 2),
            "the fallback stage also failed; the release outcome is a failure"
        );
        // Deezer, after Discogs or after a Discogs that was skipped.
        assert_eq!(
            fallback_failure_line(ProviderKind::Deezer, 3),
            "the Deezer stage also failed; the release outcome is a failure"
        );
        assert_eq!(
            fallback_failure_line(ProviderKind::Deezer, 2),
            "the Deezer stage also failed; the release outcome is a failure"
        );
        assert_eq!(
            fallback_failure_line(ProviderKind::Discogs, 3),
            "the Discogs stage also failed; the release outcome is a failure"
        );
    }

    #[test]
    fn the_accept_prompt_names_three_or_more_groups() {
        let (local, remote) = album_fixture();
        let group = |provider| {
            let candidate = Candidate::new(&local, remote.clone());
            (provider, vec![candidate], Vec::new())
        };
        let two = [
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
        ];
        assert_eq!(group_prompt(&two), "provider [1]=first [2]=second > ");
        let three = [
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
            group(ProviderKind::Deezer),
        ];
        assert_eq!(
            group_prompt(&three),
            "provider [1]=musicbrainz [2]=discogs [3]=deezer > "
        );
    }

    #[tokio::test]
    async fn a_missing_discogs_token_goes_on_to_deezer() {
        let musicbrainz = OneRelease::new(ProviderKind::MusicBrainz, None);
        let deezer = OneRelease::new(
            ProviderKind::Deezer,
            Some(ten_track_edition(ReleaseId::Deezer(7))),
        );
        let searches = deezer.searches();
        let (musicbrainz, deezer) = (RefCell::new(Some(musicbrainz)), RefCell::new(Some(deezer)));
        let make_musicbrainz = || musicbrainz.borrow_mut().take().context("built twice");
        let make_discogs =
            || -> Result<OneRelease> { Err(anyhow::Error::new(AdapterError::MissingToken)) };
        let make_deezer = || deezer.borrow_mut().take().context("built twice");
        let clients = Clients {
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
            discogs_disabled: RefCell::new(None),
            make_musicbrainz: &make_musicbrainz,
            make_discogs: &make_discogs,
            make_deezer: &make_deezer,
        };
        let mut transcript = Transcript::held();
        let Gathered::Attempts(attempts) = gather_attempts(
            &ten_files(),
            ProviderPolicy::Chain { deezer: true },
            None,
            &clients,
            false,
            &mut transcript,
        )
        .await
        else {
            panic!("the chain ended before any stage ran");
        };
        let providers: Vec<ProviderKind> =
            attempts.iter().map(|attempt| attempt.provider).collect();
        assert_eq!(providers, [ProviderKind::MusicBrainz, ProviderKind::Deezer]);
        assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            out_lines(&transcript)
                .contains(&"  Discogs token is not set; optional fallback was skipped"),
            "{:?}",
            out_lines(&transcript)
        );
        assert!(matches!(
            settlement(&ten_files(), &attempts, false),
            Some(Settlement::Fallback { stage: 1, .. })
        ));
    }

    #[test]
    fn the_id_prompt_names_each_provider_of_the_review() {
        let (local, remote) = album_fixture();
        let group = |provider| {
            let candidate = Candidate::new(&local, remote.clone());
            (provider, vec![candidate], Vec::new())
        };
        let groups = vec![
            group(ProviderKind::MusicBrainz),
            group(ProviderKind::Discogs),
            group(ProviderKind::Deezer),
            group(ProviderKind::Discogs),
        ];
        let providers = group_providers(&groups);
        assert_eq!(
            providers,
            [
                ProviderKind::MusicBrainz,
                ProviderKind::Discogs,
                ProviderKind::Deezer
            ]
        );
        assert_eq!(
            provider_prompt(&providers[..2]),
            "provider [1]=musicbrainz [2]=discogs > "
        );
        assert_eq!(
            provider_prompt(&providers),
            "provider [1]=musicbrainz [2]=discogs [3]=deezer > "
        );
        assert_eq!(chosen_provider(&providers, "3"), ProviderKind::Deezer);
        assert_eq!(chosen_provider(&providers, "deezer"), ProviderKind::Deezer);
        assert_eq!(chosen_provider(&providers, "2"), ProviderKind::Discogs);
        assert_eq!(chosen_provider(&providers, ""), ProviderKind::MusicBrainz);
        assert_eq!(chosen_provider(&providers, "9"), ProviderKind::MusicBrainz);
        assert_eq!(
            chosen_provider(&providers[..2], "deezer"),
            ProviderKind::MusicBrainz
        );
    }

    #[tokio::test]
    async fn the_exact_count_edition_goes_first_and_is_accepted() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=10)
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

        // The twelve-track edition ties with the ten-track one of its group and
        // has the lower release ID, so the sort alone puts it first.
        let attempt = attempt_over(
            &release,
            vec![edition(10, 12, Some(1)), edition(11, 10, Some(1))],
        );
        assert!(attempt.candidates[0].mapping.complete && !attempt.candidates[1].mapping.complete);
        assert_eq!(ids(&attempt), vec![11, 10]);
        assert!(
            attempt.settled(false),
            "{:?}",
            attempt.evaluations[0].blockers
        );
        assert_eq!(
            attempt.evaluations[0].blockers,
            Vec::<String>::new(),
            "the evaluation is the one computed for the promoted candidate"
        );

        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            &attempt.candidates[0],
            &attempt.evaluations[0],
            AcceptanceMethod::Automatic,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        assert!(
            out_lines(&transcript)
                .contains(&"  ranked discogs 11 before discogs 10: it lists exactly the files"),
            "{:?}",
            out_lines(&transcript)
        );
    }

    #[test]
    fn a_first_candidate_with_a_complete_mapping_is_left_alone() {
        let local = ten_files();
        let attempt = attempt_over(
            &local,
            vec![edition(10, 10, Some(1)), edition(11, 10, Some(1))],
        );
        assert!(attempt.candidates[0].mapping.complete);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert_eq!(attempt.promotion, None);
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert!(out_lines(&transcript).is_empty());
    }

    /// [`songs_on_edition`] with no length on any track.
    fn lengthless_edition(id: u64, count: usize, group: Option<u64>) -> RemoteRelease {
        let mut remote = songs_on_edition(id, count, group);
        for track in &mut remote.tracklist {
            track.duration_secs = None;
        }
        remote
    }

    #[test]
    fn a_leading_first_candidate_stays_ahead_of_a_worse_exact_edition() {
        let local = songs_on_ten_files();
        let mut candidates: Vec<Candidate> = [
            songs_on_edition(10, 12, Some(1)),
            lengthless_edition(11, 10, Some(1)),
        ]
        .into_iter()
        .map(|release| Candidate::new(&local, release))
        .collect();
        candidates.sort_by(|a, b| candidate_order(None, a, b));
        assert_eq!(candidates[0].release.id, ReleaseId::Discogs(10));
        assert!(matcher::leading_tracks(&local, &candidates[0]).is_some());
        assert!(candidates[1].mapping.complete);

        let reranking = prefer_complete(&local, &mut candidates);
        assert_eq!(reranking.promotion, None);
        assert_eq!(candidates[0].release.id, ReleaseId::Discogs(10));
        assert_eq!(
            reranking.refusal,
            Some(Refusal {
                first: ReleaseId::Discogs(10),
                exact: ReleaseId::Discogs(11),
                reason: "it lists no length for a track whose length discogs 10 lists".to_string(),
            })
        );

        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                lengthless_edition(11, 10, Some(1)),
            ],
        );
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert_eq!(attempt.promotion, None);
        // The first candidate is accepted through the first-tracks rule.
        assert!(
            attempt.evaluations[0].auto_accept,
            "{:?}",
            attempt.evaluations[0].blockers
        );
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec![
                "  kept discogs 10 first: discogs 11 lists exactly the files but it lists no length for a track whose length discogs 10 lists"
            ]
        );
    }

    #[test]
    fn a_later_exact_edition_that_is_no_worse_is_promoted_past_a_worse_one() {
        let local = songs_on_ten_files();
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                lengthless_edition(11, 10, Some(1)),
                songs_on_edition(12, 10, Some(1)),
            ],
        );
        assert_eq!(ids(&attempt)[0], 12);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(12),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
        assert_eq!(attempt.refusal, None);
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec!["  ranked discogs 12 before discogs 10: it lists exactly the files"]
        );
    }

    #[test]
    fn a_kept_first_candidate_that_is_not_accepted_falls_back_to_the_refused_exact_edition() {
        let local = songs_on_ten_files();
        // Another album within the score gap, one of whose lengths is far off,
        // makes the incomplete first candidate a close alternative that does
        // not establish the same recordings, while the exact edition's lengths
        // are within three seconds and set that album aside.
        let mut other_album = songs_on_edition(12, 10, Some(2));
        other_album.tracklist[6].title = "Harbor Lighta".to_string();
        other_album.tracklist[2].duration_secs = Some(190.0);
        let mut exact = songs_on_edition(11, 10, Some(1));
        for track in &mut exact.tracklist {
            track.duration_secs = Some(182.5);
        }
        let releases = vec![songs_on_edition(10, 12, Some(1)), exact, other_album];
        let attempt = attempt_over(&local, releases.clone());
        assert_eq!(ids(&attempt)[0], 11);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(10),
                fallback: true,
            })
        );
        assert!(attempt.refusal.is_some());
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec![
                "  kept discogs 10 first: discogs 11 lists exactly the files but its lengths fit the files worse (up to 2.50s against 0.00s)",
                "  ranked discogs 11 before discogs 10: it lists exactly the files and discogs 10 is not accepted",
            ]
        );
        assert!(
            attempt.evaluations[0].auto_accept,
            "{:?}",
            attempt.evaluations[0].blockers
        );
    }

    #[test]
    fn a_kept_first_candidate_that_is_accepted_does_not_fall_back() {
        let local = songs_on_ten_files();
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                lengthless_edition(11, 10, Some(1)),
            ],
        );
        assert!(attempt.evaluations[0].auto_accept);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert_eq!(attempt.promotion, None);
        assert!(attempt.refusal.is_some());
    }

    /// An unofficial exact edition of the group and another album within the
    /// score gap, which blocks any candidate that is not the one it fits worse.
    fn unofficial_exact_and_close_album() -> (RemoteRelease, RemoteRelease) {
        let mut bootleg = songs_on_edition(11, 10, Some(1));
        bootleg.formats = vec!["CD, Unofficial Release".to_string()];
        let mut other_album = songs_on_edition(13, 10, Some(2));
        other_album.tracklist[6].title = "Harbor Lighta".to_string();
        (bootleg, other_album)
    }

    #[test]
    fn an_unofficial_exact_edition_falls_back_when_the_kept_first_candidate_is_not_accepted() {
        let local = songs_on_ten_files();
        let (bootleg, other_album) = unofficial_exact_and_close_album();
        let attempt = attempt_over(
            &local,
            vec![songs_on_edition(10, 12, Some(1)), bootleg, other_album],
        );
        assert_eq!(ids(&attempt)[0], 11);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(10),
                fallback: true,
            })
        );
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec![
                "  kept discogs 10 first: discogs 11 lists exactly the files but it is unofficial",
                "  ranked discogs 11 before discogs 10: it lists exactly the files and discogs 10 is not accepted",
            ]
        );
    }

    #[test]
    fn a_refused_edition_is_tried_when_the_exact_edition_promoted_past_it_is_not_accepted() {
        let local = songs_on_ten_files();
        let (bootleg, other_album) = unofficial_exact_and_close_album();
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                bootleg,
                songs_on_edition(12, 10, Some(1)),
                other_album,
            ],
        );
        // The official edition was promoted past the unofficial one, is a close
        // alternative of the other album, and gives way to the refused one.
        assert_eq!(&ids(&attempt)[..2], &[11, 12]);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(12),
                fallback: true,
            })
        );
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec![
                "  ranked discogs 11 before discogs 12: it lists exactly the files and discogs 12 is not accepted"
            ],
            "a refusal that a later edition was promoted past is not said"
        );
    }

    #[test]
    fn an_accepted_promoted_edition_never_falls_back_to_a_refused_one() {
        let local = songs_on_ten_files();
        let mut bootleg = songs_on_edition(11, 10, Some(1));
        bootleg.formats = vec!["CD, Unofficial Release".to_string()];
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                bootleg,
                songs_on_edition(12, 10, Some(1)),
            ],
        );
        assert_eq!(ids(&attempt)[0], 12);
        assert!(attempt.evaluations[0].auto_accept);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(12),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
        let mut transcript = Transcript::held();
        print_promotion(&attempt, &mut transcript);
        assert_eq!(
            out_lines(&transcript),
            vec!["  ranked discogs 12 before discogs 10: it lists exactly the files"]
        );
    }

    #[test]
    fn an_exact_edition_selection_is_never_reordered() {
        let local = ten_files();
        let mut candidates: Vec<Candidate> = [edition(10, 12, Some(1)), edition(11, 10, Some(1))]
            .into_iter()
            .map(|release| Candidate::new(&local, release))
            .collect();
        assert!(!candidates[0].mapping.complete && candidates[1].mapping.complete);
        let selected = ReleaseId::Discogs(10);
        assert_eq!(
            rank_candidates(&local, &mut candidates, Some(&selected)),
            Reranking::default()
        );
        assert_eq!(candidates[0].release.id, selected);

        // Without the selection the same candidates are reordered.
        let promotion = rank_candidates(&local, &mut candidates, None)
            .promotion
            .expect("a promotion");
        assert_eq!(promotion.promoted, ReleaseId::Discogs(11));
        assert_eq!(promotion.previous, ReleaseId::Discogs(10));
    }

    #[test]
    fn a_complete_candidate_of_another_group_outside_the_gap_is_not_promoted() {
        let local = ten_files();
        let mut other_album = edition(11, 10, Some(2));
        for track in &mut other_album.tracklist {
            track.title = "Something else entirely".into();
        }
        let attempt = attempt_over(&local, vec![edition(10, 12, Some(1)), other_album]);
        let first = &attempt.candidates[0];
        let second = &attempt.candidates[1];
        assert_eq!(first.release.id, ReleaseId::Discogs(10));
        assert!(!first.mapping.complete);
        assert!(second.mapping.complete);
        assert!(!matcher::within_gap(
            first.score.album_score() - second.score.album_score()
        ));
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert_eq!(attempt.promotion, None);
    }

    const SONGS: [&str; 12] = [
        "Opening",
        "Second Wind",
        "Hollow",
        "Undertow",
        "Lantern",
        "Closing",
        "Harbor Lights",
        "Paper Crowns",
        "Slow Rain",
        "Iron Garden",
        "Weather Vane",
        "Last Orders",
    ];

    /// The fixture's first ten files, titled by [`SONGS`].
    fn songs_on_ten_files() -> LocalRelease {
        let mut local = ten_files();
        for (track, song) in local.tracks.iter_mut().zip(SONGS) {
            track.title = Some(song.to_string());
        }
        local
    }

    /// [`edition`] titled by [`SONGS`].
    fn songs_on_edition(id: u64, count: usize, group: Option<u64>) -> RemoteRelease {
        let mut remote = edition(id, count, group);
        for (track, song) in remote.tracklist.iter_mut().zip(SONGS) {
            track.title = song.to_string();
        }
        remote
    }

    #[test]
    fn an_album_the_files_open_is_not_displaced_by_an_exact_edition_of_another_album() {
        let local = songs_on_ten_files();
        // The files are the first ten of twelve tracks. Another release group
        // lists ten tracks, one of them at a title similarity near 0.87, which
        // is within the score gap of the first candidate.
        let leading = songs_on_edition(10, 12, Some(1));
        let mut other = songs_on_edition(11, 10, Some(2));
        other.tracklist[6].title = "Harbor Lighta".to_string();
        let attempt = attempt_over(&local, vec![leading, other]);
        let first = &attempt.candidates[0];
        let second = &attempt.candidates[1];
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert!(matcher::leading_tracks(&local, first).is_some());
        assert!(second.mapping.complete);
        assert!(matcher::within_gap(
            first.score.album_score() - second.score.album_score()
        ));
        assert_eq!(attempt.promotion, None);
        // The album the files open stays the one judged. The other album is a
        // close alternative that does not establish the same recordings, so
        // the first candidate is not accepted on its own, but it is the one
        // reviewed first rather than the one promoted past it.
        assert_eq!(
            attempt.evaluations[0].blockers,
            vec![
                "close alternative discogs 11 does not establish the same album and recordings"
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn a_reviewed_album_the_files_open_is_applied_with_the_mapped_files_only() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let directory_id = store::upsert_directory(&pool, std::path::Path::new("/music"))
            .await
            .unwrap();
        let files: Vec<_> = (1..=10)
            .map(|number| {
                let mut file =
                    scanned_track(&format!("/music/a/{number:02}.flac"), "Album", number);
                file.title = Some(SONGS[number as usize - 1].to_string());
                file
            })
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
        let track_rows = || async {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM release_track_matches")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        // The direct path (`i`) judges a fetched release at edition scope,
        // which never applies a partial mapping.
        let fetched = Candidate::new(&release, songs_on_edition(10, 12, Some(1)));
        assert!(matcher::leading_tracks(&release, &fetched).is_some());
        let entered = matcher::evaluate(&release, &fetched, None, &DiscoveryEvidence::default());
        assert!(!entered.can_apply(&release, &fetched));
        let mut transcript = Transcript::held();
        let outcome = accept(
            &pool,
            &release,
            ProviderKind::Discogs,
            &fetched,
            &entered,
            AcceptanceMethod::Manual,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Skipped), "{outcome:?}");
        assert_eq!(track_rows().await, 0);

        // Queue the album-scope review of the twelve-track release: another
        // album with ten tracks is a close alternative, so it is not accepted
        // automatically.
        let mut other = songs_on_edition(11, 10, Some(2));
        other.tracklist[6].title = "Harbor Lighta".to_string();
        let attempt = attempt_over(&release, vec![songs_on_edition(10, 12, Some(1)), other]);
        assert!(!attempt.evaluations[0].auto_accept);
        assert_eq!(attempt.evaluations[0].match_scope, MatchScope::Album);
        queue_attempts(&pool, &release, &[attempt], &guard, &mut transcript)
            .await
            .unwrap();

        // Interactive review loads it back and may accept it.
        let saved = store::load_review(&pool, release.id, ProviderKind::Discogs)
            .await
            .unwrap()
            .expect("a saved review");
        let groups = review_groups(&[saved]);
        let (provider, candidates, evaluations) = &groups[0];
        assert_eq!(candidates[0].release.id, ReleaseId::Discogs(10));
        assert!(evaluations[0].can_apply(&release, &candidates[0]));
        let outcome = accept(
            &pool,
            &release,
            *provider,
            &candidates[0],
            &evaluations[0],
            AcceptanceMethod::Manual,
            false,
            &guard,
            &mut transcript,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Accepted), "{outcome:?}");
        // Ten files are mapped and the release's two later tracks have no rows.
        assert_eq!(track_rows().await, 10);
        let ordinals: Vec<i64> = sqlx::query_scalar(
            "SELECT remote_ordinal FROM release_track_matches ORDER BY remote_ordinal",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ordinals, (1..=10).collect::<Vec<i64>>());
    }

    #[test]
    fn an_exact_edition_of_the_same_group_still_displaces_the_album_the_files_open() {
        let local = songs_on_ten_files();
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                songs_on_edition(11, 10, Some(1)),
            ],
        );
        assert!(matcher::leading_tracks(&local, &attempt.candidates[1]).is_some());
        assert_eq!(ids(&attempt), vec![11, 10]);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
        assert!(
            attempt.settled(false),
            "{:?}",
            attempt.evaluations[0].blockers
        );
    }

    #[test]
    fn a_masterless_exact_edition_goes_before_the_longer_edition_of_a_master() {
        let local = songs_on_ten_files();
        // The files are the first ten of twelve tracks, but a release with no
        // master lists the same title, artist, tracks and lengths and exactly
        // ten of them: it is the same audio, and it can be applied whole.
        let attempt = attempt_over(
            &local,
            vec![
                songs_on_edition(10, 12, Some(1)),
                songs_on_edition(11, 10, None),
            ],
        );
        let longer = attempt
            .candidates
            .iter()
            .find(|candidate| candidate.release.id == ReleaseId::Discogs(10))
            .unwrap();
        assert!(matcher::leading_tracks(&local, longer).is_some());
        assert_eq!(ids(&attempt), vec![11, 10]);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
        assert!(
            attempt.settled(false),
            "{:?}",
            attempt.evaluations[0].blockers
        );
        assert!(
            !attempt.evaluations[0]
                .warnings
                .iter()
                .any(|warning| warning.ends_with(matcher::MORE_TRACKS_NOTE)),
            "{:?}",
            attempt.evaluations[0].warnings
        );
    }

    #[test]
    fn a_masterless_edition_that_is_not_the_same_audio_does_not_displace_the_album_the_files_open()
    {
        let local = songs_on_ten_files();
        let leading = songs_on_edition(10, 12, Some(1));
        let mut other = songs_on_edition(11, 10, None);
        other.tracklist[6].title = "Harbor Lighta".to_string();
        let attempt = attempt_over(&local, vec![leading, other]);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert!(matcher::within_gap(
            attempt.candidates[0].score.album_score() - attempt.candidates[1].score.album_score()
        ));
        assert_eq!(attempt.promotion, None);
    }

    /// The titles of a fourteen-track edition: [`SONGS`] and two more.
    const LONG_SONGS: [&str; 14] = [
        "Opening",
        "Second Wind",
        "Hollow",
        "Undertow",
        "Lantern",
        "Closing",
        "Harbor Lights",
        "Paper Crowns",
        "Slow Rain",
        "Iron Garden",
        "Weather Vane",
        "Last Orders",
        "Night Ferry",
        "Encore",
    ];

    /// [`edition`] with up to fourteen tracks titled by [`LONG_SONGS`].
    fn long_songs_edition(id: u64, count: usize, group: Option<u64>) -> RemoteRelease {
        let mut remote = edition(id, 12, group);
        for number in 13..=14 {
            let mut track = remote.tracklist[11].clone();
            track.position = Some(number.to_string());
            remote.tracklist.push(track);
        }
        remote.tracklist.truncate(count);
        for (track, song) in remote.tracklist.iter_mut().zip(LONG_SONGS) {
            track.title = song.to_string();
        }
        remote
    }

    #[test]
    fn another_album_with_a_different_artist_does_not_displace_the_album_the_files_open() {
        let local = songs_on_ten_files();
        // The files are the first ten of fourteen tracks (71%, so not the
        // leading-tracks shape). Another album lists the same ten tracks under
        // an artist that scores 0.857, which is within the score gap.
        let mut other = long_songs_edition(11, 10, Some(2));
        other.artists = vec!["Artiste".to_string()];
        let attempt = attempt_over(&local, vec![long_songs_edition(10, 14, Some(1)), other]);
        let (first, second) = (&attempt.candidates[0], &attempt.candidates[1]);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert!(matcher::leading_tracks(&local, first).is_none());
        assert!(second.mapping.complete);
        assert!(matcher::within_gap(
            first.score.album_score() - second.score.album_score()
        ));
        assert_eq!(attempt.promotion, None);
        assert!(!attempt.evaluations[0].auto_accept);
        assert!(!attempt.settled(false));
    }

    #[test]
    fn files_that_open_the_first_album_stop_the_promotion_of_another_album_at_any_count() {
        let local = songs_on_ten_files();
        // The other album agrees in text this time, but the files are still the
        // first ten tracks of the first candidate, so it cannot be told which
        // album they come from: the first candidate keeps its place.
        let attempt = attempt_over(
            &local,
            vec![
                long_songs_edition(10, 14, Some(1)),
                long_songs_edition(11, 10, Some(2)),
            ],
        );
        let (first, second) = (&attempt.candidates[0], &attempt.candidates[1]);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert!(matcher::leading_tracks(&local, first).is_none());
        assert!(second.mapping.complete);
        assert!(matcher::within_gap(
            first.score.album_score() - second.score.album_score()
        ));
        assert_eq!(attempt.promotion, None);
        assert!(!attempt.evaluations[0].auto_accept);
    }

    /// Ten files numbered 1-4 and 6-11, a twelve-track edition that lists a
    /// track at 5 the files lack, and another album that lists exactly the
    /// files, numbered the same way: the files are not the first candidate's
    /// leading tracks.
    fn files_that_skip_a_track() -> (LocalRelease, RemoteRelease, RemoteRelease) {
        let mut local = songs_on_ten_files();
        for track in &mut local.tracks[4..] {
            track.track_number = track.track_number.map(|number| number + 1);
        }
        let mut first = long_songs_edition(10, 12, Some(1));
        let titles = LONG_SONGS[..4]
            .iter()
            .chain(&["Interlude"])
            .chain(&LONG_SONGS[4..11]);
        for (track, title) in first.tracklist.iter_mut().zip(titles) {
            track.title = title.to_string();
        }
        let mut other = long_songs_edition(11, 10, Some(2));
        for (index, track) in other.tracklist.iter_mut().enumerate().skip(4) {
            track.position = Some((index + 2).to_string());
        }
        (local, first, other)
    }

    #[test]
    fn a_complete_album_that_agrees_in_text_displaces_a_first_candidate_the_files_do_not_open() {
        let (local, first, other) = files_that_skip_a_track();
        let attempt = attempt_over(&local, vec![first, other]);
        let first = attempt
            .candidates
            .iter()
            .find(|candidate| candidate.release.id == ReleaseId::Discogs(10))
            .unwrap();
        assert!(!first.mapping.complete);
        assert_eq!(
            first.mapping.pairs[4],
            Some(5),
            "the files skip the extra track"
        );
        assert_eq!(ids(&attempt), vec![11, 10]);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(11),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
    }

    #[test]
    fn a_complete_album_with_a_different_artist_does_not_displace_a_first_candidate_the_files_do_not_open()
     {
        let (local, first, mut other) = files_that_skip_a_track();
        other.artists = vec!["Artiste".to_string()];
        let attempt = attempt_over(&local, vec![first, other]);
        let (first, second) = (&attempt.candidates[0], &attempt.candidates[1]);
        assert_eq!(ids(&attempt), vec![10, 11]);
        assert!(!first.mapping.complete && second.mapping.complete);
        assert!(matcher::within_gap(
            first.score.album_score() - second.score.album_score()
        ));
        assert_eq!(attempt.promotion, None);
    }

    #[test]
    fn the_first_qualifying_candidate_is_promoted_and_the_rest_keep_their_order() {
        let local = ten_files();
        // The second candidate is incomplete too and the third is of another
        // album outside the gap; the fourth and fifth both qualify.
        let mut other_album = edition(12, 10, Some(2));
        for track in &mut other_album.tracklist {
            track.title = "Something else entirely".into();
        }
        let attempt = attempt_over(
            &local,
            vec![
                edition(10, 12, Some(1)),
                edition(11, 11, Some(1)),
                other_album,
                edition(13, 10, Some(1)),
                edition(14, 10, Some(1)),
            ],
        );
        assert_eq!(ids(&attempt), vec![13, 10, 11, 14, 12]);
        assert_eq!(
            attempt.promotion,
            Some(Promotion {
                promoted: ReleaseId::Discogs(13),
                previous: ReleaseId::Discogs(10),
                fallback: false,
            })
        );
    }
}
