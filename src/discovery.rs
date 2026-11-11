//! Bounded, provider-neutral candidate discovery.
//!
//! Discovery only issues searches and full-release lookups. Scoring and
//! acceptance stay in [`crate::matcher`]. Every identifier question is answered
//! from the summaries that were actually returned — including ones that were
//! never hydrated — so a partial result set can never manufacture uniqueness.

use crate::matcher::{
    self, DiscoveryEvidence, LocalRelease, RemoteRelease, SearchSummary, StrategyOutcome,
    normalize_barcode, normalize_identifier,
};
use crate::providers::{
    MetadataProvider, ProviderError, ProviderKind, ReleaseId, SearchRequest, SearchStrategy,
};
use std::collections::{HashMap, HashSet};

/// Lowest [`matcher::rank_search_summary`] score that earns a detailed lookup
/// for a title or artist search hit.
///
/// The rank adds up to 0.5 for the title, 0.3 for the artist, 0.1 for the year
/// and 0.4 for each matching identifier. Unrelated results land near 0.1
/// because only a lucky year match and a little character overlap score, while
/// an exact artist match alone reaches 0.3. Discogs titles carry the artist
/// (`Artist - Title`), which lowers their title similarity, so the floor stays
/// well under a typical true hit. Hits from a barcode or catalog number search
/// are always looked up, and so is everything when no hit reaches the floor.
const MIN_DETAIL_RANK: f64 = 0.25;

/// Per-provider search and lookup limits. Weak title hits are skipped before
/// they can use up `detail_attempts`; see [`MIN_DETAIL_RANK`].
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryBudget {
    pub max_strategies: usize,
    pub pages_per_strategy: u16,
    pub page_size: u8,
    pub detail_attempts: usize,
}

impl DiscoveryBudget {
    pub fn for_provider(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::Discogs => Self {
                max_strategies: 3,
                pages_per_strategy: 2,
                page_size: 50,
                detail_attempts: 10,
            },
            ProviderKind::MusicBrainz => Self {
                max_strategies: 4,
                pages_per_strategy: 2,
                page_size: 50,
                detail_attempts: 10,
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct DiscoveryResult {
    pub releases: Vec<RemoteRelease>,
    /// Candidate-specific evidence. Absent candidates were never hydrated.
    pub evidence: HashMap<ReleaseId, DiscoveryEvidence>,
    pub notes: Vec<String>,
}

impl DiscoveryResult {
    pub fn is_empty(&self) -> bool {
        self.releases.is_empty()
    }
}

/// Strategies in provider-specific order. Identifiers come first so a unique
/// barcode or catalog number can settle identity before title searches.
pub fn strategies_for(
    provider: ProviderKind,
    local: &LocalRelease,
) -> Vec<(String, SearchStrategy)> {
    let mut strategies = Vec::new();
    let release_group = local
        .embedded_evidence
        .release_group_id
        .filter(|_| provider == ProviderKind::MusicBrainz);

    let barcode = local
        .barcode
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let catalog = local
        .catalog_number
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match provider {
        ProviderKind::MusicBrainz => {
            if let Some(barcode) = barcode {
                strategies.push((
                    "barcode".to_string(),
                    SearchStrategy::Barcode(barcode.to_string()),
                ));
            }
            if let Some(catalog) = catalog {
                strategies.push((
                    "catalog number".to_string(),
                    SearchStrategy::CatalogNumber(catalog.to_string()),
                ));
            }
            if let Some(title) = local
                .album_title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
            {
                strategies.push((
                    "title and artist".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: local.album_artist.clone(),
                        release_group,
                    },
                ));
                let sparse_artist = local.album_artist.is_none()
                    || local.is_compilation
                    || matches!(local.artist_kind, matcher::ArtistKind::Unknown);
                if sparse_artist {
                    strategies.push((
                        "title only".to_string(),
                        SearchStrategy::TitleArtist {
                            title: title.to_string(),
                            artist: None,
                            release_group,
                        },
                    ));
                }
            }
        }
        ProviderKind::Discogs => {
            if let Some(barcode) = barcode {
                strategies.push((
                    "identifier".to_string(),
                    SearchStrategy::Barcode(barcode.to_string()),
                ));
            } else if let Some(catalog) = catalog {
                strategies.push((
                    "identifier".to_string(),
                    SearchStrategy::CatalogNumber(catalog.to_string()),
                ));
            }
            if let Some(title) = local
                .album_title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
            {
                strategies.push((
                    "title".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: local.album_artist.clone(),
                        release_group: None,
                    },
                ));
                if local.is_compilation && local.album_artist.is_some() {
                    strategies.push((
                        "relaxed".to_string(),
                        SearchStrategy::TitleArtist {
                            title: title.to_string(),
                            artist: None,
                            release_group: None,
                        },
                    ));
                }
            }
        }
    }

    strategies
}

fn embedded_release_id(provider: ProviderKind, local: &LocalRelease) -> Option<ReleaseId> {
    match provider {
        ProviderKind::Discogs => local.embedded_discogs_id.map(ReleaseId::Discogs),
        ProviderKind::MusicBrainz => local
            .embedded_evidence
            .release_id
            .map(ReleaseId::MusicBrainz),
    }
}

/// True when the local identifiers are satisfied by this release.
fn identifiers_match(local: &LocalRelease, release: &RemoteRelease) -> bool {
    let barcode_match = local.barcode.as_deref().is_some_and(|barcode| {
        let normalized = normalize_barcode(barcode);
        !normalized.is_empty()
            && release
                .barcodes
                .iter()
                .any(|candidate| normalize_barcode(candidate) == normalized)
    });

    let catalog_match = local.catalog_number.as_deref().is_some_and(|catalog| {
        let normalized = normalize_identifier(catalog);
        !normalized.is_empty()
            && release
                .catalog_numbers
                .iter()
                .any(|candidate| normalize_identifier(candidate) == normalized)
    });

    barcode_match || catalog_match
}

/// Identifier uniqueness is derived from every identifier summary, not from the
/// candidates that happened to be hydrated.
fn identifier_is_unique(
    local: &LocalRelease,
    releases: &HashMap<ReleaseId, RemoteRelease>,
    identifier_ids: &HashSet<ReleaseId>,
    unhydrated: &HashSet<ReleaseId>,
    failed: &HashSet<ReleaseId>,
    complete: bool,
) -> bool {
    if !complete || identifier_ids.len() != 1 {
        return false;
    }
    if identifier_ids
        .iter()
        .any(|id| unhydrated.contains(id) || failed.contains(id))
    {
        return false;
    }
    let matching: Vec<&ReleaseId> = releases
        .iter()
        .filter(|(_, release)| identifiers_match(local, release))
        .map(|(id, _)| id)
        .collect();
    matching.len() == 1 && identifier_ids.contains(matching[0])
}

/// Discovers and hydrates candidates for one local release.
///
/// A successful empty result is distinct from a failed stage: when nothing could
/// be fetched and at least one stage failed, the first provider error is
/// returned so the policy layer can fall back or report a failure.
pub async fn discover<P: MetadataProvider>(
    provider: &P,
    local: &LocalRelease,
    direct: Option<ReleaseId>,
) -> Result<DiscoveryResult, ProviderError> {
    let provider_kind = provider.kind();
    let budget = DiscoveryBudget::for_provider(provider_kind);
    let mut notes = Vec::new();

    if let Some(id) = direct {
        if id.provider() != provider_kind {
            return Err(ProviderError::Permanent {
                provider: provider_kind,
                status: None,
                message: format!("release {id} belongs to another provider"),
            });
        }
        let release = provider.fetch_release(id).await?;
        let mut evidence = DiscoveryEvidence::default();
        evidence.blockers.push(
            "the release was selected explicitly; edition uniqueness was not searched".to_string(),
        );
        let mut result = DiscoveryResult {
            releases: vec![release.clone()],
            evidence: HashMap::new(),
            notes,
        };
        result.evidence.insert(release.id.clone(), evidence);
        return Ok(result);
    }

    let mut releases: HashMap<ReleaseId, RemoteRelease> = HashMap::new();
    let mut failed_ids: HashSet<ReleaseId> = HashSet::new();
    let mut unhydrated_ids: HashSet<ReleaseId> = HashSet::new();
    let mut failures: Vec<ProviderError> = Vec::new();
    let mut lookup_slots = 0usize;
    let mut embedded_resolved = false;

    if let Some(id) = embedded_release_id(provider_kind, local) {
        lookup_slots += 1;
        match provider.fetch_release(id.clone()).await {
            Ok(release) => {
                embedded_resolved = release.id == id || release.lookup_aliases.contains(&id);
                releases.insert(release.id.clone(), release);
                notes.push(format!("embedded release {id} was looked up directly"));
            }
            Err(error) if error.is_auth() => return Err(error),
            Err(error) => {
                notes.push(format!(
                    "embedded release {id} could not be fetched: {error}"
                ));
                failures.push(error);
            }
        }
    }

    let mut summaries: HashMap<ReleaseId, SearchSummary> = HashMap::new();
    let mut identifier_ids: HashSet<ReleaseId> = HashSet::new();
    let identifier_applicable = local.barcode.is_some() || local.catalog_number.is_some();
    let mut identifier_attempted = false;
    let mut identifier_complete = identifier_applicable;
    let mut outcomes: Vec<StrategyOutcome> = Vec::new();

    for (index, (label, strategy)) in strategies_for(provider_kind, local).into_iter().enumerate() {
        if index >= budget.max_strategies {
            notes.push(format!(
                "omitted strategies after the first {}",
                budget.max_strategies
            ));
            if identifier_applicable && !identifier_attempted {
                identifier_complete = false;
            }
            break;
        }

        let is_identifier = matches!(
            strategy,
            SearchStrategy::Barcode(_) | SearchStrategy::CatalogNumber(_)
        );
        if is_identifier {
            identifier_attempted = true;
        }

        let mut outcome = StrategyOutcome::new(label.clone());
        outcome.attempted = true;
        let mut offset = 0u16;
        let mut pages = 0u16;
        let mut complete = false;

        loop {
            let request = SearchRequest {
                strategy: strategy.clone(),
                offset,
                limit: budget.page_size,
            };

            match provider.search(request).await {
                Ok(page) => {
                    outcome.successful = true;
                    pages += 1;
                    for hit in page.hits {
                        if is_identifier {
                            identifier_ids.insert(hit.id.clone());
                        }
                        summaries.entry(hit.id.clone()).or_insert(hit);
                    }

                    match page.next_offset {
                        Some(next) if pages < budget.pages_per_strategy => offset = next,
                        Some(_) => {
                            notes.push(format!("{label} search stopped after {pages} page(s)"));
                            break;
                        }
                        None => {
                            complete = true;
                            break;
                        }
                    }
                }
                Err(error) if error.is_auth() => return Err(error),
                Err(error) => {
                    notes.push(format!("{label} search failed: {error}"));
                    failures.push(error);
                    break;
                }
            }
        }

        outcome.complete = complete;
        if is_identifier && !complete {
            identifier_complete = false;
        }
        outcomes.push(outcome);
    }

    if identifier_applicable && !identifier_attempted {
        identifier_complete = false;
    }
    if !identifier_applicable {
        identifier_complete = false;
    }

    let mut ranked: Vec<(f64, SearchSummary)> = summaries
        .into_values()
        .map(|summary| (matcher::rank_search_summary(local, &summary), summary))
        .collect();
    ranked
        .sort_by(|(a_rank, a), (b_rank, b)| b_rank.total_cmp(a_rank).then_with(|| a.id.cmp(&b.id)));
    // When nothing clears the floor, the ranking carries no signal (for
    // example, sparse local tags), so every summary stays eligible.
    let skip_weak = ranked.iter().any(|(rank, _)| *rank >= MIN_DETAIL_RANK);
    let mut skipped_weak = 0usize;

    for (rank, summary) in ranked {
        if releases.contains_key(&summary.id) {
            continue;
        }
        if skip_weak && rank < MIN_DETAIL_RANK && !identifier_ids.contains(&summary.id) {
            skipped_weak += 1;
            continue;
        }
        if lookup_slots >= budget.detail_attempts {
            unhydrated_ids.insert(summary.id.clone());
            continue;
        }
        lookup_slots += 1;

        match provider.fetch_release(summary.id.clone()).await {
            Ok(release) => {
                releases.insert(release.id.clone(), release);
            }
            Err(error) if error.is_auth() => return Err(error),
            Err(error) => {
                notes.push(format!("could not fetch release {}: {error}", summary.id));
                failures.push(error);
                failed_ids.insert(summary.id.clone());
            }
        }
    }

    if skipped_weak > 0 {
        notes.push(format!(
            "skipped detailed lookups for {skipped_weak} weakly matching search result(s)"
        ));
    }

    if unhydrated_ids.is_empty() && lookup_slots >= budget.detail_attempts {
        notes.push(format!(
            "stopped after {} detailed release lookups",
            budget.detail_attempts
        ));
    }

    if releases.is_empty() {
        if let Some(error) = failures.into_iter().next() {
            return Err(error);
        }
        return Ok(DiscoveryResult {
            releases: Vec::new(),
            evidence: HashMap::new(),
            notes,
        });
    }

    let unique = identifier_is_unique(
        local,
        &releases,
        &identifier_ids,
        &unhydrated_ids,
        &failed_ids,
        identifier_complete,
    );

    let mut evidence: HashMap<ReleaseId, DiscoveryEvidence> = HashMap::new();
    for id in releases.keys() {
        let mut candidate_evidence = DiscoveryEvidence {
            identifier_unique: unique && identifiers_match(local, &releases[id]),
            identifier_search_truncated: identifier_applicable && !identifier_complete,
            embedded_release_resolved: embedded_resolved,
            identifier_summary_ids: identifier_ids.clone(),
            unhydrated_ids: unhydrated_ids.clone(),
            failed_ids: failed_ids.clone(),
            strategies: outcomes.clone(),
            blockers: Vec::new(),
        };

        if identifier_applicable {
            if !identifier_complete {
                candidate_evidence
                    .blockers
                    .push("identifier searches did not complete".to_string());
            }
            if identifier_ids.len() > 1 {
                candidate_evidence.blockers.push(format!(
                    "{} releases match the searched identifiers",
                    identifier_ids.len()
                ));
            }
            if identifier_ids.iter().any(|identifier| {
                unhydrated_ids.contains(identifier) || failed_ids.contains(identifier)
            }) {
                candidate_evidence.blockers.push(
                    "some releases sharing the searched identifiers were not hydrated".to_string(),
                );
            }
        }

        evidence.insert(id.clone(), candidate_evidence);
    }

    let mut releases: Vec<RemoteRelease> = releases.into_values().collect();
    releases.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(DiscoveryResult {
        releases,
        evidence,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{
        ArtistKind, EmbeddedReleaseEvidence, LocalTrack, RemoteTrack, TrackEntryType,
    };
    use crate::providers::{Mbid, SearchPage};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeProvider {
        kind: ProviderKind,
        pages: Mutex<VecDeque<Result<SearchPage, ProviderError>>>,
        fetch_results: Mutex<VecDeque<Result<RemoteRelease, ProviderError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProvider {
        fn new(
            kind: ProviderKind,
            pages: Vec<Result<SearchPage, ProviderError>>,
            fetch_results: Vec<Result<RemoteRelease, ProviderError>>,
        ) -> Self {
            Self {
                kind,
                pages: Mutex::new(pages.into()),
                fetch_results: Mutex::new(fetch_results.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl MetadataProvider for FakeProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        async fn search(&self, request: SearchRequest) -> Result<SearchPage, ProviderError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("search:{:?}@{}", request.strategy, request.offset));
            self.pages
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(SearchPage {
                    hits: Vec::new(),
                    total: Some(0),
                    next_offset: None,
                }))
        }

        async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
            self.calls.lock().unwrap().push(format!("fetch:{id}"));
            self.fetch_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(ProviderError::NotFound {
                        provider: self.kind,
                        id: id.storage_id(),
                    })
                })
        }
    }

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    fn local() -> LocalRelease {
        LocalRelease {
            id: 1,
            directory: "/music/blue".to_string(),
            album_title: Some("Blue".to_string()),
            album_artist: Some("Test Artist".to_string()),
            artist_kind: ArtistKind::AlbumArtist,
            year: Some(2001),
            barcode: Some("0123456789012".to_string()),
            catalog_number: None,
            embedded_discogs_id: None,
            embedded_evidence: EmbeddedReleaseEvidence::default(),
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
                release_track_id: None,
                recording_id: None,
                identifier_problems: Vec::new(),
            }],
        }
    }

    fn remote(id: &str) -> RemoteRelease {
        RemoteRelease {
            id: ReleaseId::MusicBrainz(mbid(id)),
            group_id: None,
            title: "Blue".to_string(),
            artists: vec!["Test Artist".to_string()],
            year: Some(2001),
            country: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            formats: Vec::new(),
            barcodes: vec!["0123456789012".to_string()],
            tracklist: vec![RemoteTrack {
                position: Some("1".to_string()),
                title: "One".to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: Some(1),
                track_position: Some(1),
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            }],
            url: String::new(),
            lookup_aliases: Vec::new(),
            artist_credit: None,
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }

    fn summary(id: &str) -> SearchSummary {
        SearchSummary {
            id: ReleaseId::MusicBrainz(mbid(id)),
            title: "Blue".to_string(),
            artists: vec!["Test Artist".to_string()],
            artist_credit: None,
            year: Some(2001),
            country: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            formats: Vec::new(),
            barcodes: vec!["0123456789012".to_string()],
            result_type: "release".to_string(),
        }
    }

    #[tokio::test]
    async fn two_hydrated_editions_sharing_a_barcode_are_not_unique() {
        let chosen = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let page = SearchPage {
            hits: vec![summary(chosen), summary(other)],
            total: Some(2),
            next_offset: None,
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(page)],
            vec![Ok(remote(chosen)), Ok(remote(other))],
        );

        let result = discover(&provider, &local(), None).await.unwrap();
        let chosen_id = ReleaseId::MusicBrainz(mbid(chosen));
        let other_id = ReleaseId::MusicBrainz(mbid(other));
        assert_eq!(result.evidence[&chosen_id].identifier_summary_ids.len(), 2);
        assert!(!result.evidence[&chosen_id].identifier_unique);
        assert!(result.evidence[&chosen_id].unhydrated_ids.is_empty());
        assert!(result.evidence[&chosen_id].failed_ids.is_empty());
        assert!(!result.evidence[&other_id].identifier_unique);
    }

    #[tokio::test]
    async fn identifier_uniqueness_is_not_copied_to_a_candidate_without_the_identifier() {
        let barcoded = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let barcode_page = SearchPage {
            hits: vec![summary(barcoded)],
            total: Some(1),
            next_offset: None,
        };
        let mut other_summary = summary(other);
        other_summary.barcodes.clear();
        let title_page = SearchPage {
            hits: vec![other_summary],
            total: Some(1),
            next_offset: None,
        };
        let mut other_release = remote(other);
        other_release.barcodes.clear();
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(barcode_page), Ok(title_page)],
            vec![Ok(remote(barcoded)), Ok(other_release)],
        );

        let local = local();
        let result = discover(&provider, &local, None).await.unwrap();
        let barcoded_id = ReleaseId::MusicBrainz(mbid(barcoded));
        let other_id = ReleaseId::MusicBrainz(mbid(other));
        assert!(result.evidence[&barcoded_id].identifier_unique);
        assert!(!result.evidence[&other_id].identifier_unique);

        let release = result
            .releases
            .iter()
            .find(|release| release.id == other_id)
            .unwrap()
            .clone();
        let candidate = matcher::Candidate::new(&local, release);
        let evaluation = matcher::evaluate(&local, &candidate, None, &result.evidence[&other_id]);
        assert!(!evaluation.auto_accept, "{:?}", evaluation.evidence);
        assert!(
            !evaluation
                .evidence
                .iter()
                .any(|line| line.contains("uniquely identifies"))
        );
    }

    #[tokio::test]
    async fn one_failed_detail_keeps_the_summary_visible() {
        let chosen = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let page = SearchPage {
            hits: vec![summary(chosen), summary(other)],
            total: Some(2),
            next_offset: None,
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(page)],
            vec![
                Ok(remote(chosen)),
                Err(ProviderError::Network {
                    provider: ProviderKind::MusicBrainz,
                    message: "timeout".to_string(),
                }),
            ],
        );

        let result = discover(&provider, &local(), None).await.unwrap();
        let chosen_id = ReleaseId::MusicBrainz(mbid(chosen));
        let other_id = ReleaseId::MusicBrainz(mbid(other));
        assert_eq!(result.evidence[&chosen_id].identifier_summary_ids.len(), 2);
        assert!(!result.evidence[&chosen_id].identifier_unique);
        assert!(result.evidence[&chosen_id].failed_ids.contains(&other_id));
    }

    #[tokio::test]
    async fn a_completed_second_page_clears_the_truncation_flag() {
        let id = "11111111-1111-1111-1111-111111111111";
        let first = SearchPage {
            hits: vec![summary(id)],
            total: Some(2),
            next_offset: Some(50),
        };
        let second = SearchPage {
            hits: vec![summary("22222222-2222-2222-2222-222222222222")],
            total: Some(2),
            next_offset: None,
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(first), Ok(second)],
            vec![
                Ok(remote(id)),
                Ok(remote("22222222-2222-2222-2222-222222222222")),
            ],
        );

        let result = discover(&provider, &local(), None).await.unwrap();
        let evidence = &result.evidence[&ReleaseId::MusicBrainz(mbid(id))];
        let barcode = evidence
            .strategies
            .iter()
            .find(|outcome| outcome.label == "barcode")
            .unwrap();
        assert!(barcode.complete);
        assert!(!evidence.identifier_search_truncated);
        let barcode_searches = provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("search:Barcode"))
            .count();
        assert_eq!(barcode_searches, 2);
    }

    #[tokio::test]
    async fn successful_empty_results_are_distinct_from_failures() {
        let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
        let result = discover(&provider, &local(), None).await.unwrap();
        assert!(result.is_empty());

        let failing = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Err(ProviderError::Network {
                provider: ProviderKind::MusicBrainz,
                message: "down".to_string(),
            })],
            Vec::new(),
        );
        let error = discover(&failing, &local(), None).await.unwrap_err();
        assert!(matches!(error, ProviderError::Network { .. }));
    }

    #[tokio::test]
    async fn direct_selection_is_review_only_and_issues_one_lookup() {
        let id = "11111111-1111-1111-1111-111111111111";
        let provider =
            FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), vec![Ok(remote(id))]);
        let release_id = ReleaseId::MusicBrainz(mbid(id));
        let result = discover(&provider, &local(), Some(release_id.clone()))
            .await
            .unwrap();
        assert_eq!(result.releases.len(), 1);
        assert!(!result.evidence[&release_id].blockers.is_empty());
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn weak_title_results_do_not_use_detail_lookups_but_identifier_hits_do() {
        let uuid = |n: u32| format!("{n:08x}-0000-0000-0000-000000000000");
        let weak = |n: u32| {
            let mut hit = summary(&uuid(n));
            hit.title = "Zzzz Qqqq".to_string();
            hit.artists = vec!["Nobody Else".to_string()];
            hit.year = Some(1975);
            hit.barcodes.clear();
            hit
        };
        let good = |n: u32| {
            let mut hit = summary(&uuid(n));
            hit.barcodes.clear();
            hit
        };

        // The barcode search returns one weak summary; it is still an
        // identifier hit and must be hydrated.
        let barcode_page = SearchPage {
            hits: vec![weak(100)],
            total: Some(1),
            next_offset: None,
        };
        let mut title_hits = vec![good(1), good(2)];
        title_hits.extend((10..20).map(weak));
        let title_page = SearchPage {
            hits: title_hits,
            total: Some(12),
            next_offset: None,
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(barcode_page), Ok(title_page)],
            vec![
                Ok(remote(&uuid(1))),
                Ok(remote(&uuid(2))),
                Ok(remote(&uuid(100))),
            ],
        );

        let result = discover(&provider, &local(), None).await.unwrap();
        let fetched: Vec<String> = provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("fetch:"))
            .cloned()
            .collect();
        assert_eq!(fetched.len(), 3, "{fetched:?}");
        assert!(fetched.iter().any(|call| call.ends_with(&uuid(100))));
        assert!(fetched.iter().any(|call| call.ends_with(&uuid(1))));
        assert!(fetched.iter().any(|call| call.ends_with(&uuid(2))));
        assert_eq!(result.releases.len(), 3);
    }

    #[tokio::test]
    async fn weak_results_are_still_fetched_when_nothing_clears_the_rank_floor() {
        let weak_id = "11111111-1111-1111-1111-111111111111";
        let mut hit = summary(weak_id);
        hit.title = "Zzzz Qqqq".to_string();
        hit.artists = vec!["Nobody Else".to_string()];
        hit.year = Some(1975);
        hit.barcodes.clear();
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![
                Ok(SearchPage {
                    hits: Vec::new(),
                    total: Some(0),
                    next_offset: None,
                }),
                Ok(SearchPage {
                    hits: vec![hit],
                    total: Some(1),
                    next_offset: None,
                }),
            ],
            vec![Ok(remote(weak_id))],
        );
        let result = discover(&provider, &local(), None).await.unwrap();
        assert_eq!(result.releases.len(), 1);
    }

    #[tokio::test]
    async fn deterministic_ranking_prefers_matching_identifiers() {
        let matching = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let mut unrelated = summary(other);
        unrelated.barcodes = vec!["9999999999999".to_string()];
        unrelated.title = "Something Else".to_string();
        let page = SearchPage {
            hits: vec![unrelated, summary(matching)],
            total: Some(2),
            next_offset: None,
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![Ok(page)],
            vec![Ok(remote(matching)), Ok(remote(other))],
        );

        let result = discover(&provider, &local(), None).await.unwrap();
        let calls = provider.calls.lock().unwrap().clone();
        let matching_position = calls
            .iter()
            .position(|call| call == &format!("fetch:musicbrainz {matching}"))
            .expect("matching release was hydrated");
        let other_position = calls
            .iter()
            .position(|call| call == &format!("fetch:musicbrainz {other}"))
            .expect("other release was hydrated");
        assert!(matching_position < other_position);
        assert!(!result.evidence.is_empty());
    }

    #[test]
    fn musicbrainz_strategies_use_group_evidence_and_title_fallback() {
        let mut local = local();
        local.album_artist = None;
        local.embedded_evidence.release_group_id =
            Some(mbid("4642ee19-7790-3c8d-ab5e-d133de942db6"));
        let strategies = strategies_for(ProviderKind::MusicBrainz, &local);
        let labels: Vec<&str> = strategies.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["barcode", "title and artist", "title only"]);
        match &strategies[1].1 {
            SearchStrategy::TitleArtist { release_group, .. } => {
                assert!(release_group.is_some());
            }
            _ => panic!("expected a title strategy"),
        }
    }

    #[test]
    fn discogs_keeps_its_bounded_strategy_order() {
        let strategies = strategies_for(ProviderKind::Discogs, &local());
        let labels: Vec<&str> = strategies.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["identifier", "title"]);
    }
}
