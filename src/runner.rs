//! CLI orchestration: select local releases, discover Discogs candidates,
//! decide, review, and persist.

use crate::discogs::{AdapterError, DiscogsAdapter, release_search_params};
use crate::matcher::{
    self, Candidate, DiscoveryEvidence, Evaluation, LocalRelease, RemoteRelease, SearchSummary,
};
use crate::store::{self, AcceptanceMethod, StoredCandidate};
use anyhow::{Context, Result, anyhow, bail};
use discogger::SearchParams;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

/// Strategies, pages, and detailed fetches are bounded so one release cannot
/// exhaust the Discogs rate limit.
const MAX_STRATEGIES: usize = 3;
const MAX_PAGES: u32 = 2;
const MAX_DETAIL_FETCHES: usize = 10;

#[derive(Debug, Clone, Default)]
pub struct MatchOptions {
    pub scope: Option<PathBuf>,
    pub local_release: Option<i64>,
    pub discogs_release: Option<u64>,
    pub review: bool,
    pub dry_run: bool,
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
    Accept(Box<Candidate>),
    Skip,
    Quit,
}

struct Discovery {
    candidates: Vec<Candidate>,
    evaluations: Vec<Evaluation>,
    notes: Vec<String>,
}

pub async fn run_match(pool: &SqlitePool, options: MatchOptions) -> Result<MatchSummary> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if options.review && !interactive {
        bail!("--review requires an interactive terminal");
    }
    if options.discogs_release.is_some() && options.local_release.is_none() {
        bail!("--discogs-release requires --local-release");
    }

    // Grouping is derived identity, not a matching result, so it is refreshed
    // even for a dry run. Matching results and overlays are never written.
    store::rebuild_local_releases(pool, options.scope.as_deref()).await?;

    let statuses = store::status_by_id(pool).await?;
    let mut releases = store::load_local_releases(pool, options.scope.as_deref()).await?;

    if let Some(id) = options.local_release {
        releases.retain(|release| release.id == id);
        if releases.is_empty() {
            bail!("local release {id} was not found");
        }
    }

    if !options.review {
        // Ordinary matching only processes unmatched and stale releases.
        releases.retain(|release| {
            matches!(
                statuses
                    .get(&release.id)
                    .map(String::as_str)
                    .unwrap_or(store::STATUS_UNMATCHED),
                store::STATUS_UNMATCHED | store::STATUS_STALE
            )
        });
    }

    let mut summary = MatchSummary {
        dry_run: options.dry_run,
        ..MatchSummary::default()
    };

    if releases.is_empty() {
        println!("Nothing to match.");
        return Ok(summary);
    }

    let adapter = DiscogsAdapter::from_env()
        .map_err(|error| anyhow!("{error}"))
        .context("Discogs matching needs a personal access token")?;

    for release in &releases {
        let label = format!(
            "{} — {}",
            release
                .album_artist
                .as_deref()
                .unwrap_or("(unknown artist)"),
            release.album_title.as_deref().unwrap_or("(no album tag)")
        );
        println!("\nMatching local release {} ({label})", release.id);

        let outcome = process_release(pool, &adapter, release, &options, interactive).await;

        match outcome {
            Ok(Outcome::Accepted) => summary.accepted += 1,
            Ok(Outcome::Queued) => summary.queued += 1,
            Ok(Outcome::Unmatched) => summary.unmatched += 1,
            Ok(Outcome::Skipped) => summary.skipped += 1,
            Ok(Outcome::Failed) => summary.failed += 1,
            Ok(Outcome::Quit) => return Ok(summary),
            Err(error) => {
                if is_auth_error(&error) {
                    return Err(error.context("Discogs authentication failed; stopping matching"));
                }
                summary.failed += 1;
                eprintln!("failed to match local release {}: {error:#}", release.id);
            }
        }
    }

    Ok(summary)
}

fn is_auth_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<AdapterError>()
            .map(AdapterError::is_auth)
            .unwrap_or(false)
    })
}

async fn process_release(
    pool: &SqlitePool,
    adapter: &DiscogsAdapter,
    release: &LocalRelease,
    options: &MatchOptions,
    interactive: bool,
) -> Result<Outcome> {
    let discovery = discover(adapter, release, options.discogs_release).await?;

    for note in &discovery.notes {
        eprintln!("  note: {note}");
    }

    if discovery.candidates.is_empty() {
        println!("  no Discogs candidates found");
        return Ok(Outcome::Unmatched);
    }

    let best = &discovery.candidates[0];
    let best_evaluation = &discovery.evaluations[0];

    if !options.review && options.discogs_release.is_none() && best_evaluation.auto_accept {
        return accept(
            pool,
            release,
            best,
            best_evaluation,
            AcceptanceMethod::Automatic,
            options.dry_run,
        )
        .await;
    }

    if options.dry_run {
        print_decision(release, &discovery);
        return Ok(if best_evaluation.auto_accept {
            Outcome::Accepted
        } else {
            Outcome::Queued
        });
    }

    // `--discogs-release` and `--review` never auto-accept; both go through the
    // same review path when a terminal is available.
    if interactive {
        match review_release(adapter, release, &discovery)? {
            ReviewOutcome::Accept(candidate) => {
                let candidate = *candidate;
                let evaluation = discovery
                    .candidates
                    .iter()
                    .position(|candidate_entry| candidate_entry.release.id == candidate.release.id)
                    .map(|index| discovery.evaluations[index].clone())
                    .unwrap_or_else(|| {
                        matcher::evaluate(release, &candidate, None, &DiscoveryEvidence::default())
                    });
                accept(
                    pool,
                    release,
                    &candidate,
                    &evaluation,
                    AcceptanceMethod::Manual,
                    false,
                )
                .await
            }
            ReviewOutcome::Skip => Ok(Outcome::Skipped),
            ReviewOutcome::Quit => Ok(Outcome::Quit),
        }
    } else {
        queue_for_review(pool, release, &discovery).await?;
        Ok(Outcome::Queued)
    }
}

async fn accept(
    pool: &SqlitePool,
    release: &LocalRelease,
    candidate: &Candidate,
    evaluation: &Evaluation,
    method: AcceptanceMethod,
    dry_run: bool,
) -> Result<Outcome> {
    if !evaluation.can_apply(candidate) {
        println!(
            "  cannot accept Discogs release {}: track mapping is invalid or incomplete ({})",
            candidate.release.id,
            candidate.mapping.notes.join("; ")
        );
        return Ok(Outcome::Skipped);
    }

    if dry_run {
        println!(
            "  would accept Discogs release {} (score {:.3}) as {}",
            candidate.release.id,
            candidate.score.total,
            method.as_str()
        );
        return Ok(Outcome::Accepted);
    }

    match store::accept_match(pool, release.id, candidate, method, &release.revision).await {
        Ok(()) => {
            println!(
                "  accepted Discogs release {} (score {:.3}, {})",
                candidate.release.id,
                candidate.score.total,
                method.as_str()
            );
            Ok(Outcome::Accepted)
        }
        Err(error) => {
            eprintln!("  could not apply the match: {error:#}");
            Ok(Outcome::Failed)
        }
    }
}

async fn queue_for_review(
    pool: &SqlitePool,
    release: &LocalRelease,
    discovery: &Discovery,
) -> Result<Outcome> {
    let stored: Vec<StoredCandidate> = discovery
        .candidates
        .iter()
        .zip(discovery.evaluations.iter())
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
        .collect();

    let reasons = discovery.evaluations[0].blockers.clone();
    store::queue_review(
        pool,
        release.id,
        &release.revision,
        &stored,
        &discovery.candidates[0].mapping,
        &reasons,
    )
    .await?;

    println!("  queued for review: {}", reasons.join("; "));
    Ok(Outcome::Queued)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

async fn discover(
    adapter: &DiscogsAdapter,
    local: &LocalRelease,
    direct: Option<u64>,
) -> Result<Discovery> {
    let mut notes = Vec::new();
    let mut fetched: HashMap<u64, RemoteRelease> = HashMap::new();

    if let Some(id) = direct {
        let release = adapter
            .fetch_release(id)
            .await
            .map_err(anyhow::Error::new)?;
        fetched.insert(release.id, release);
        return Ok(finish(local, fetched, DiscoveryEvidence::default(), notes));
    }

    if let Some(id) = local.embedded_discogs_id {
        match adapter.fetch_release(id).await {
            Ok(release) => {
                fetched.insert(release.id, release);
            }
            Err(error) if error.is_auth() => return Err(anyhow::Error::new(error)),
            Err(error) => notes.push(format!(
                "embedded Discogs release {id} could not be fetched: {error}"
            )),
        }
    }

    let mut ranked: Vec<(f64, SearchSummary)> = Vec::new();
    let mut identifier_truncated = false;
    let mut identifier_searched = false;

    for (strategy_index, (name, params)) in discover_strategies(local).into_iter().enumerate() {
        if strategy_index >= MAX_STRATEGIES {
            break;
        }

        let mut page = 1;
        let mut strategy_truncated = false;
        loop {
            let result = match adapter.search(params.clone(), page).await {
                Ok(result) => result,
                Err(error) if error.is_auth() => return Err(anyhow::Error::new(error)),
                Err(error) => {
                    notes.push(format!("{name} search failed: {error}"));
                    break;
                }
            };

            if result.truncated() {
                strategy_truncated = true;
            }

            if name == "identifier" {
                identifier_searched = true;
                if result.truncated() {
                    identifier_truncated = true;
                }
            }

            for hit in &result.hits {
                ranked.push((matcher::rank_search_summary(local, hit), hit.clone()));
            }

            if page >= MAX_PAGES || page >= result.pages || result.hits.is_empty() {
                break;
            }
            page += 1;
        }

        if strategy_truncated {
            notes.push(format!(
                "{name} search returned more results than the {MAX_PAGES} pages fetched"
            ));
        }
    }

    ranked
        .sort_by(|(rank_a, a), (rank_b, b)| rank_b.total_cmp(rank_a).then_with(|| a.id.cmp(&b.id)));

    let mut seen: Vec<u64> = Vec::new();
    for (_, summary) in ranked {
        if fetched.contains_key(&summary.id) || seen.contains(&summary.id) {
            continue;
        }
        seen.push(summary.id);
        if fetched.len() >= MAX_DETAIL_FETCHES {
            notes.push(format!(
                "stopped after {MAX_DETAIL_FETCHES} detailed release fetches"
            ));
            break;
        }

        match adapter.fetch_release(summary.id).await {
            Ok(release) => {
                fetched.insert(release.id, release);
            }
            Err(error) if error.is_auth() => return Err(anyhow::Error::new(error)),
            Err(error) => notes.push(format!(
                "could not fetch Discogs release {}: {error}",
                summary.id
            )),
        }
    }

    let identifier_matches = fetched
        .values()
        .filter(|release| identifier_matches(local, release))
        .count();
    let has_identifier = local.barcode.is_some() || local.catalog_number.is_some();
    let identifier_unique =
        has_identifier && identifier_searched && !identifier_truncated && identifier_matches == 1;

    let evidence = DiscoveryEvidence {
        identifier_search_truncated: identifier_truncated,
        identifier_unique,
    };

    Ok(finish(local, fetched, evidence, notes))
}

fn discover_strategies(local: &LocalRelease) -> Vec<(&'static str, SearchParams)> {
    let mut strategies = Vec::new();

    if local.barcode.is_some() || local.catalog_number.is_some() {
        let mut params = SearchParams::new();
        if let Some(barcode) = &local.barcode {
            params = params.barcode(barcode.clone());
        }
        if let Some(catalog) = &local.catalog_number {
            params = params.catno(catalog.clone());
        }
        strategies.push(("identifier", params));
    }

    if let Some(title) = local
        .album_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        strategies.push((
            "title",
            release_search_params(title, local.album_artist.as_deref()),
        ));

        // Compilations and releases with ambiguous artists relax the artist
        // constraint, because a compilation's artist is not a Discogs artist.
        if local.is_compilation && local.album_artist.is_some() {
            strategies.push(("relaxed", release_search_params(title, None)));
        }
    }

    strategies
}

fn identifier_matches(local: &LocalRelease, release: &RemoteRelease) -> bool {
    let barcode_match = local.barcode.as_ref().is_some_and(|barcode| {
        let normalized = matcher::normalize_barcode(barcode);
        !normalized.is_empty()
            && release
                .barcodes
                .iter()
                .any(|candidate| matcher::normalize_barcode(candidate) == normalized)
    });

    let catalog_match = local.catalog_number.as_ref().is_some_and(|catalog| {
        let normalized = matcher::normalize_identifier(catalog);
        !normalized.is_empty()
            && release
                .catalog_numbers
                .iter()
                .any(|candidate| matcher::normalize_identifier(candidate) == normalized)
    });

    barcode_match || catalog_match
}

fn finish(
    local: &LocalRelease,
    fetched: HashMap<u64, RemoteRelease>,
    evidence: DiscoveryEvidence,
    notes: Vec<String>,
) -> Discovery {
    let mut candidates: Vec<Candidate> = fetched
        .into_values()
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
            let runner_up = if index == 0 { candidates.get(1) } else { None };
            matcher::evaluate(local, candidate, runner_up, &evidence)
        })
        .collect();

    let _ = evidence;

    Discovery {
        candidates,
        evaluations,
        notes,
    }
}

// ---------------------------------------------------------------------------
// Reporting and review
// ---------------------------------------------------------------------------

fn print_decision(local: &LocalRelease, discovery: &Discovery) {
    for (index, (candidate, evaluation)) in discovery
        .candidates
        .iter()
        .zip(discovery.evaluations.iter())
        .enumerate()
        .take(3)
    {
        println!(
            "  {}. Discogs release {} — {} (score {:.3}){}",
            index + 1,
            candidate.release.id,
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
        for note in &candidate.mapping.notes {
            println!("       mapping: {note}");
        }
    }

    if let Some((_, first)) = discovery
        .candidates
        .first()
        .zip(discovery.evaluations.first())
    {
        if first.auto_accept {
            println!(
                "  would accept Discogs release {} (score {:.3})",
                discovery.candidates[0].release.id, discovery.candidates[0].score.total
            );
        } else {
            println!("  would queue for review");
        }
    }
    let _ = local;
}

fn print_release_header(release: &LocalRelease) {
    println!(
        "\nLocal release {} — {}",
        release.id,
        release.album_title.as_deref().unwrap_or("(no album tag)")
    );
    println!(
        "  artist: {}   files: {}",
        release.album_artist.as_deref().unwrap_or("(unknown)"),
        release.tracks.len()
    );
    println!("  folder: {}", release.directory);
}

fn print_candidate(
    local: &LocalRelease,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
) {
    let release = &candidate.release;
    println!(
        "\n  [{}] Discogs release {} — score {:.3}",
        index + 1,
        release.id,
        candidate.score.total
    );
    println!("      {} — {}", release.artist_display(), release.title);
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
    if let Some(catalog) = release.catalog_number() {
        details.push(format!("catno {catalog}"));
    }
    if let Some(format) = release.format_display() {
        details.push(format);
    }
    if !details.is_empty() {
        println!("      {}", details.join(" · "));
    }
    println!("      {}", release.url);

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
        local.tracks.len()
    );
    for (local_index, remote_index) in candidate.mapping.assigned_pairs() {
        let local_title = local.tracks[local_index]
            .title
            .as_deref()
            .unwrap_or(&local.tracks[local_index].path);
        let remote_track = &release.tracklist[remote_index];
        println!(
            "        {local_title}  ->  {}{}",
            remote_track
                .position
                .as_deref()
                .map(|position| format!("{position} "))
                .unwrap_or_default(),
            remote_track.title
        );
    }
    for (index, pair) in candidate.mapping.pairs.iter().enumerate() {
        if pair.is_none() {
            let title = local.tracks[index]
                .title
                .as_deref()
                .unwrap_or(&local.tracks[index].path);
            println!("        missing: {title}");
        }
    }

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
        println!("      mapping note: {note}");
    }
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn review_release(
    adapter: &DiscogsAdapter,
    local: &LocalRelease,
    discovery: &Discovery,
) -> Result<ReviewOutcome> {
    let mut candidates: Vec<(Candidate, Evaluation)> = discovery
        .candidates
        .iter()
        .cloned()
        .zip(discovery.evaluations.iter().cloned())
        .collect();

    print_release_header(local);
    for (index, (candidate, evaluation)) in candidates.iter().enumerate() {
        print_candidate(local, index, candidate, evaluation);
    }

    loop {
        let action = prompt("\n[a]ccept  [i]d  [s]kip  [q]uit > ")?;
        match action.as_str() {
            "a" | "" => {
                let selection = prompt("candidate number [1] > ")?;
                let index = selection
                    .parse::<usize>()
                    .unwrap_or(1)
                    .clamp(1, candidates.len().max(1))
                    - 1;
                let (candidate, evaluation) = &candidates[index];

                if !evaluation.can_apply(candidate) {
                    println!(
                        "cannot accept: the track mapping is invalid or incomplete ({})",
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
                    return Ok(ReviewOutcome::Accept(Box::new(candidate.clone())));
                }
            }
            "i" => {
                let raw = prompt("Discogs release ID > ")?;
                let Ok(id) = raw.parse::<u64>() else {
                    println!("invalid release ID");
                    continue;
                };
                match futures_block_on(adapter, id) {
                    Ok(release) => {
                        let candidate = Candidate::new(local, release);
                        let evaluation = matcher::evaluate(
                            local,
                            &candidate,
                            None,
                            &DiscoveryEvidence::default(),
                        );
                        println!("\nFetched release {id}:");
                        print_candidate(local, candidates.len(), &candidate, &evaluation);
                        candidates.push((candidate, evaluation));
                    }
                    Err(error) => println!("could not fetch release {id}: {error}"),
                }
            }
            "s" => return Ok(ReviewOutcome::Skip),
            "q" => return Ok(ReviewOutcome::Quit),
            _ => println!("unknown action"),
        }
    }
}

/// The review screen is synchronous; this keeps the async fetch local.
fn futures_block_on(adapter: &DiscogsAdapter, id: u64) -> Result<RemoteRelease> {
    tokio::runtime::Handle::current()
        .block_on(adapter.fetch_release(id))
        .map_err(anyhow::Error::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{ALGORITHM_VERSION, ArtistKind, LocalTrack};

    fn local_release() -> LocalRelease {
        LocalRelease {
            id: 1,
            directory: "/music/blue".to_string(),
            album_title: Some("Blue".to_string()),
            album_artist: Some("Test Artist".to_string()),
            artist_kind: ArtistKind::AlbumArtist,
            year: Some(2001),
            barcode: None,
            catalog_number: None,
            embedded_discogs_id: None,
            is_compilation: false,
            has_album_tag: true,
            revision: "rev".to_string(),
            tracks: vec![LocalTrack {
                id: 1,
                path: "/music/blue/01.flac".to_string(),
                title: Some("One".to_string()),
                disc: None,
                track_number: Some(1),
                duration_secs: Some(180.0),
            }],
        }
    }

    #[test]
    fn strategy_order_starts_with_identifiers_then_title() {
        let mut local = local_release();
        local.barcode = Some("0123456789012".to_string());
        let strategies = discover_strategies(&local);
        assert_eq!(strategies[0].0, "identifier");
        assert_eq!(strategies[1].0, "title");
        assert!(strategies.len() <= MAX_STRATEGIES);
    }

    #[test]
    fn compilations_add_a_relaxed_artist_strategy() {
        let mut local = local_release();
        local.is_compilation = true;
        local.artist_kind = ArtistKind::VariousArtists;
        local.album_artist = Some("Various Artists".to_string());
        let names: Vec<&str> = discover_strategies(&local)
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names, vec!["title", "relaxed"]);
    }

    #[test]
    fn sparse_tags_only_use_the_title_strategy() {
        let mut local = local_release();
        local.album_artist = None;
        local.artist_kind = ArtistKind::Unknown;
        let strategies = discover_strategies(&local);
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies[0].0, "title");
    }

    #[test]
    fn identifiers_match_only_when_normalized_values_agree() {
        let mut local = local_release();
        local.barcode = Some("0123 4567 89012".to_string());
        let release = RemoteRelease {
            id: 1,
            master_id: None,
            title: "Blue".to_string(),
            artists: vec!["Test Artist".to_string()],
            year: Some(2001),
            country: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            formats: Vec::new(),
            barcodes: vec!["0123456789012".to_string()],
            tracklist: Vec::new(),
            url: String::new(),
        };
        assert!(identifier_matches(&local, &release));

        let other = RemoteRelease {
            barcodes: vec!["9999999999999".to_string()],
            ..release
        };
        assert!(!identifier_matches(&local, &other));
    }

    #[test]
    fn algorithm_version_is_recorded_for_explainability() {
        assert_eq!(ALGORITHM_VERSION, "discogs-match-v1");
    }
}
