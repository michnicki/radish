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
use crate::scene;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Lowest [`matcher::rank_identity`] rank that earns a detailed lookup
/// for a title or artist search hit.
///
/// The rank adds up to 0.5 for the title, 0.3 for the artist, 0.1 for the year
/// and 0.4 for each matching identifier. Unrelated results land near 0.1
/// because only a lucky year match and a little character overlap score, while
/// an exact artist match alone reaches 0.3. Discogs titles carry the artist
/// (`Artist - Title`), which lowers their title similarity, so the floor stays
/// well under a typical true hit. Hits from a barcode or catalog number search
/// are always looked up, and so is everything when no hit reaches the floor
/// or the local release lacks an album title or artist.
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
    /// Search and detail-lookup failures that did not stop discovery because
    /// other results arrived. The right release may be among what failed to
    /// load, so a caller must not treat the candidates as the complete answer.
    pub failures: Vec<ProviderError>,
}

impl DiscoveryResult {
    pub fn is_empty(&self) -> bool {
        self.releases.is_empty()
    }
}

/// The release as it is searched and ranked: its tags, with a missing or junk
/// album title or artist read from a scene directory name instead.
///
/// The second value is the note to record when the directory supplied anything.
/// Real tags are never replaced, and a name that cannot be read with certainty
/// (see [`scene`]) leaves the release as it is. [`matcher::score`] sees the
/// release's own tags and reads the directory name again on its own (see
/// [`scene::directory_split`]).
fn search_view(local: &LocalRelease) -> (Cow<'_, LocalRelease>, Option<String>) {
    let identity = matcher::tag_identity(local);
    let artist_unusable = local
        .album_artist
        .as_deref()
        .is_none_or(|artist| artist.trim().is_empty() || matcher::is_junk_artist(artist));
    if identity.title.is_some() && !artist_unusable {
        return (Cow::Borrowed(local), None);
    }
    let Some(split) = scene::directory_split(local) else {
        return (Cow::Borrowed(local), None);
    };

    let mut view = local.clone();
    let mut title_read = None;
    let mut artist_read = None;
    // `directory_split` already reads a filler title as no title.
    if identity.title.is_none()
        && let Some(title) = &split.title
    {
        title_read = Some(title.clone());
        view.album_title = Some(title.clone());
    }
    if artist_unusable {
        artist_read = split.credited_artist();
        if split.various {
            view.artist_kind = matcher::ArtistKind::VariousArtists;
        }
        view.album_artist.clone_from(&artist_read);
    }
    let read = match (&title_read, &artist_read) {
        (Some(title), Some(artist)) => format!("album '{title}' and artist '{artist}'"),
        (Some(title), None) => format!("album '{title}'"),
        (None, Some(artist)) => format!("artist '{artist}'"),
        (None, None) => return (Cow::Borrowed(local), None),
    };
    (
        Cow::Owned(view),
        Some(format!("searched with {read} read from the directory name")),
    )
}

/// Label of the search with what the directory name reads.
const DIRECTORY_LABEL: &str = "directory name";

/// One more title search, with what the directory name reads.
///
/// For a release whose album title or artist tag the directory name disagrees
/// with: a cut-off title, or another artist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectorySearch {
    title: String,
    artist: String,
    /// The run's note naming what was read.
    note: String,
}

/// Text for telling whether two names differ: normalized, `&` and `and`
/// dropped, no spaces.
///
/// `Don't` and `Dont`, `AC/DC` and `ACDC`, and `Method Man & Redman` and
/// `Method Man And Redman` each compare equal.
fn compact(text: &str) -> String {
    matcher::normalize_text(&text.replace('&', " and "))
        .split_whitespace()
        .filter(|word| *word != "and")
        .collect()
}

/// The extra search the directory name asks for, if it adds anything.
///
/// A half (album title or artist) the directory reads differently from `view`
/// is searched with the directory's text. A half it agrees on keeps the tags'
/// spelling, which is the one a provider's index matches. Without a differing
/// half there is no search. A Various Artists name searches as
/// `Various Artists`, and a name with no album title (`Fazed-EP-…`) keeps the
/// tags' title.
fn directory_search(local: &LocalRelease, view: &LocalRelease) -> Option<DirectorySearch> {
    let split = scene::directory_split(local)?;
    let artist = split.credited_artist()?;
    let tag_title = matcher::search_title(view);
    let artist_differs = view
        .album_artist
        .as_deref()
        .is_none_or(|tag| compact(tag) != compact(&artist));
    // The artist the search uses, for a self-titled directory title to name.
    let searched_artist = if artist_differs {
        Some(artist.as_str())
    } else {
        view.album_artist.as_deref()
    };
    // A self-titled placeholder is searched as the artist's name, so the
    // directory agrees with it too.
    let title_keys: Vec<String> = view
        .album_title
        .as_deref()
        .into_iter()
        .chain(tag_title.as_deref())
        .map(compact)
        .collect();
    // A self-titled directory title agrees with a placeholder tag, and with a
    // tag that names the artist.
    let self_titled_agrees = |title: &str| {
        matcher::is_self_titled(title)
            && view.album_title.as_deref().is_some_and(|tag| {
                matcher::is_self_titled(tag)
                    || searched_artist.is_some_and(|artist| compact(tag) == compact(artist))
            })
    };
    let read_title = split
        .title
        .filter(|title| !title_keys.contains(&compact(title)) && !self_titled_agrees(title));
    let mut read = Vec::new();
    if let Some(title) = &read_title {
        read.push(format!("album '{title}'"));
    }
    if artist_differs {
        read.push(format!("artist '{artist}'"));
    }
    if read.is_empty() {
        return None;
    }
    let title = read_title.or(tag_title)?;
    let artist = if artist_differs {
        artist
    } else {
        view.album_artist.clone()?
    };
    Some(DirectorySearch {
        title,
        artist,
        note: format!(
            "also searched with {} read from the directory name",
            read.join(" and ")
        ),
    })
}

/// Strategies in provider-specific order. Identifiers come first so a unique
/// barcode or catalog number can settle identity before title searches.
///
/// A release whose album title or artist is missing or junk is searched with
/// what its directory name says instead, when the name is a scene release name.
///
/// A test helper: production builds the view once with `search_view`, which it
/// also needs for ranking, and calls `strategies_for_view` with it.
#[cfg(test)]
pub fn strategies_for(
    provider: ProviderKind,
    local: &LocalRelease,
) -> Vec<(String, SearchStrategy)> {
    let (view, _) = search_view(local);
    let directory = directory_search(local, &view);
    strategies_for_view(provider, &view, directory.as_ref())
}

/// The searches for one local release, in the order they run.
///
/// `directory` is one more title search, right after the tags' own, so a full
/// budget drops the optional searches after it first.
fn strategies_for_view(
    provider: ProviderKind,
    local: &LocalRelease,
    directory: Option<&DirectorySearch>,
) -> Vec<(String, SearchStrategy)> {
    let mut strategies = Vec::new();
    // A self-titled placeholder is searched as the artist's name first, and then
    // as written, because most providers need every title word to match: the
    // artist-named search never reaches a release titled `Self Titled` or `S/T`.
    let title = matcher::search_title(local);
    let as_tagged = local
        .album_title
        .as_deref()
        .filter(|tag| !tag.trim().is_empty() && title.as_deref() != Some(*tag));
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
            if let Some(title) = title.as_deref() {
                strategies.push((
                    "title and artist".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: local.album_artist.clone(),
                        release_group,
                    },
                ));
            }
            if let Some(directory) = directory {
                strategies.push((
                    DIRECTORY_LABEL.to_string(),
                    SearchStrategy::TitleArtist {
                        title: directory.title.clone(),
                        artist: Some(directory.artist.clone()),
                        release_group,
                    },
                ));
            }
            let sparse_artist = local.album_artist.is_none()
                || local.is_compilation
                || matches!(local.artist_kind, matcher::ArtistKind::Unknown);
            if let Some(title) = title.as_deref()
                && sparse_artist
            {
                strategies.push((
                    "title only".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: None,
                        release_group,
                    },
                ));
            }
            // The last title search, so the budget drops it before any other.
            if let Some(tag) = as_tagged {
                strategies.push((
                    "title as tagged".to_string(),
                    SearchStrategy::TitleArtist {
                        title: tag.to_string(),
                        artist: local.album_artist.clone(),
                        release_group,
                    },
                ));
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
            if let Some(title) = title.as_deref() {
                strategies.push((
                    "title".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: local.album_artist.clone(),
                        release_group: None,
                    },
                ));
            }
            if let Some(directory) = directory {
                strategies.push((
                    DIRECTORY_LABEL.to_string(),
                    SearchStrategy::TitleArtist {
                        title: directory.title.clone(),
                        artist: Some(directory.artist.clone()),
                        release_group: None,
                    },
                ));
            }
            if let Some(title) = title.as_deref()
                && local.is_compilation
                && local.album_artist.is_some()
            {
                strategies.push((
                    "relaxed".to_string(),
                    SearchStrategy::TitleArtist {
                        title: title.to_string(),
                        artist: None,
                        release_group: None,
                    },
                ));
            }
            // The last title search, so the budget drops it before any other.
            if let Some(tag) = as_tagged {
                strategies.push((
                    "title as tagged".to_string(),
                    SearchStrategy::TitleArtist {
                        title: tag.to_string(),
                        artist: local.album_artist.clone(),
                        release_group: None,
                    },
                ));
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
            failures: Vec::new(),
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

    let (view, directory_note) = search_view(local);
    // Ranking, the weak-result floor and the searches all use the same identity.
    let searched: &LocalRelease = &view;
    let directory = directory_search(local, searched);
    let strategies = strategies_for_view(provider_kind, searched, directory.as_ref());
    // The note says what the searches used, so it needs a search that uses it:
    // an identifier search does not.
    if strategies
        .iter()
        .any(|(_, strategy)| matches!(strategy, SearchStrategy::TitleArtist { .. }))
    {
        notes.extend(directory_note);
    }
    // The directory search's own note, when the budget reaches it.
    if let Some(directory) = &directory
        && strategies
            .iter()
            .take(budget.max_strategies)
            .any(|(label, _)| label == DIRECTORY_LABEL)
    {
        notes.push(directory.note.clone());
    }

    for (index, (label, strategy)) in strategies.into_iter().enumerate() {
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

    let rank_identity = matcher::rank_identity(searched);
    let mut ranked: Vec<(f64, SearchSummary)> = summaries
        .into_values()
        .map(|summary| (rank_identity.rank(searched, &summary), summary))
        .collect();
    ranked
        .sort_by(|(a_rank, a), (b_rank, b)| b_rank.total_cmp(a_rank).then_with(|| a.id.cmp(&b.id)));
    // When nothing clears the floor, the ranking carries no signal (for
    // example, sparse local tags), so every summary stays eligible.
    // The rank only measures what the local tags and the directory name
    // provide, so the floor needs both a title and an artist to be meaningful.
    let has_nonempty =
        |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    let skip_weak = has_nonempty(&searched.album_title)
        && has_nonempty(&searched.album_artist)
        && ranked.iter().any(|(rank, _)| *rank >= MIN_DETAIL_RANK);
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
            failures: Vec::new(),
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
        failures,
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
    async fn the_rank_floor_needs_both_a_local_title_and_artist() {
        let strong = "11111111-1111-1111-1111-111111111111";
        let weak = "22222222-2222-2222-2222-222222222222";
        // Discogs-style titles carry the artist, so a correct hit can rank low
        // when the local artist is unknown and cannot be used to rank it.
        let mut weak_hit = summary(weak);
        weak_hit.title = "Test Artist - Blue".to_string();
        weak_hit.artists.clear();
        weak_hit.barcodes.clear();
        let mut strong_hit = summary(strong);
        strong_hit.barcodes.clear();
        let mut local = local();
        local.album_artist = None;
        assert!(matcher::rank_search_summary(&local, &weak_hit) < MIN_DETAIL_RANK);
        let empty = || {
            Ok(SearchPage {
                hits: Vec::new(),
                total: Some(0),
                next_offset: None,
            })
        };
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![
                empty(),
                Ok(SearchPage {
                    hits: vec![strong_hit, weak_hit],
                    total: Some(2),
                    next_offset: None,
                }),
            ],
            vec![Ok(remote(strong)), Ok(remote(weak))],
        );
        let result = discover(&provider, &local, None).await.unwrap();
        assert_eq!(result.releases.len(), 2, "{:?}", result.notes);
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

    fn title_searches(kind: ProviderKind, local: &LocalRelease) -> Vec<(String, String)> {
        strategies_for(kind, local)
            .into_iter()
            .filter_map(|(label, strategy)| match strategy {
                SearchStrategy::TitleArtist { title, .. } => Some((label, title)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_self_titled_release_searches_with_the_artist_as_title() {
        let mut local = local();
        local.album_title = Some("Self Titled".to_string());
        local.barcode = None;
        let expected = |first: &str, second: &str| {
            vec![
                (first.to_string(), "Test Artist".to_string()),
                (second.to_string(), "Self Titled".to_string()),
            ]
        };
        // The artist's name first, then the tag as written.
        assert_eq!(
            title_searches(ProviderKind::MusicBrainz, &local),
            expected("title and artist", "title as tagged")
        );
        assert_eq!(
            title_searches(ProviderKind::Discogs, &local),
            expected("title", "title as tagged")
        );
        for kind in [ProviderKind::MusicBrainz, ProviderKind::Discogs] {
            for (_, strategy) in strategies_for(kind, &local) {
                match strategy {
                    SearchStrategy::TitleArtist { artist, .. } => {
                        assert_eq!(artist.as_deref(), Some("Test Artist"));
                    }
                    other => panic!("expected a title strategy, got {other:?}"),
                }
            }
        }

        // The requests that reach the provider come in that order.
        let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
        let _ = discover(&provider, &local, None).await;
        let calls = provider.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].contains("title: \"Test Artist\""), "{calls:?}");
        assert!(calls[1].contains("title: \"Self Titled\""), "{calls:?}");

        // Various Artists and a missing artist have no name to search with, so the
        // placeholder is searched once, as written.
        let mut various = local.clone();
        various.artist_kind = ArtistKind::VariousArtists;
        various.album_artist = Some("Various Artists".to_string());
        assert_eq!(
            title_searches(ProviderKind::MusicBrainz, &various),
            vec![("title and artist".to_string(), "Self Titled".to_string())]
        );
        let mut missing = local.clone();
        missing.album_artist = None;
        missing.artist_kind = ArtistKind::Unknown;
        assert_eq!(
            title_searches(ProviderKind::Discogs, &missing),
            vec![("title".to_string(), "Self Titled".to_string())]
        );
    }

    #[test]
    fn a_full_budget_drops_the_as_tagged_search_and_nothing_else() {
        let mut local = local();
        local.album_title = Some("Self Titled".to_string());
        local.catalog_number = Some("CAT-1".to_string());
        local.is_compilation = true;
        let labels = |kind| -> Vec<String> {
            strategies_for(kind, &local)
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        };
        let musicbrainz = labels(ProviderKind::MusicBrainz);
        assert_eq!(musicbrainz.len(), 5, "{musicbrainz:?}");
        assert_eq!(musicbrainz.last().unwrap(), "title as tagged");
        assert!(
            musicbrainz.len() - 1
                == DiscoveryBudget::for_provider(ProviderKind::MusicBrainz).max_strategies
        );
        let discogs = labels(ProviderKind::Discogs);
        assert_eq!(
            discogs,
            vec!["identifier", "title", "relaxed", "title as tagged"]
        );
        assert!(
            discogs.len() - 1
                == DiscoveryBudget::for_provider(ProviderKind::Discogs).max_strategies
        );
    }

    #[test]
    fn releases_without_a_placeholder_keep_exactly_their_searches() {
        let local = local();
        let labels = |kind, local: &LocalRelease| -> Vec<String> {
            strategies_for(kind, local)
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        };
        assert_eq!(
            labels(ProviderKind::MusicBrainz, &local),
            vec!["barcode", "title and artist"]
        );
        assert_eq!(
            labels(ProviderKind::Discogs, &local),
            vec!["identifier", "title"]
        );
        // A junk title is searched as written, once.
        let mut junk = local.clone();
        junk.album_title = Some("Unknown Title".to_string());
        assert_eq!(
            title_searches(ProviderKind::MusicBrainz, &junk),
            vec![("title and artist".to_string(), "Unknown Title".to_string())]
        );
        // `Untitled` is a literal title.
        let mut untitled = local.clone();
        untitled.album_title = Some("Untitled".to_string());
        assert_eq!(
            title_searches(ProviderKind::Discogs, &untitled),
            vec![("title".to_string(), "Untitled".to_string())]
        );
    }

    #[tokio::test]
    async fn a_release_literally_titled_as_the_placeholder_is_found() {
        let literal = "11111111-1111-1111-1111-111111111111";
        let mut local = local();
        local.album_title = Some("Self Titled".to_string());
        local.barcode = None;
        let mut hit = summary(literal);
        hit.title = "Self Titled".to_string();
        hit.barcodes.clear();
        let empty = || {
            Ok(SearchPage {
                hits: Vec::new(),
                total: Some(0),
                next_offset: None,
            })
        };
        // The artist-named search finds nothing; the as-tagged one finds the release.
        let provider = FakeProvider::new(
            ProviderKind::MusicBrainz,
            vec![
                empty(),
                Ok(SearchPage {
                    hits: vec![hit.clone()],
                    total: Some(1),
                    next_offset: None,
                }),
            ],
            vec![Ok(remote(literal))],
        );
        // Another band's unrelated release (and year) is below the floor.
        let mut unrelated = hit.clone();
        unrelated.title = "Something Else".to_string();
        unrelated.artists = vec!["Another Band".to_string()];
        unrelated.year = None;
        assert!(matcher::rank_search_summary(&local, &hit) >= MIN_DETAIL_RANK);
        assert!(
            matcher::rank_search_summary(&local, &unrelated) < MIN_DETAIL_RANK,
            "{}",
            matcher::rank_search_summary(&local, &unrelated)
        );
        let result = discover(&provider, &local, None).await.unwrap();
        assert_eq!(result.releases.len(), 1, "{:?}", result.notes);
    }

    const SCENE_DIRECTORY: &str = "/mnt/mp3/2001/Straight_Faced_-_Broken-1996-SDR";

    /// A release with no tags to search with, in a scene-named directory.
    fn untagged() -> LocalRelease {
        let mut local = local();
        local.directory = SCENE_DIRECTORY.to_string();
        local.album_title = None;
        local.album_artist = None;
        local.artist_kind = ArtistKind::Unknown;
        local.barcode = None;
        local.has_album_tag = false;
        local
    }

    type TitleSearch = (String, String, Option<String>);

    fn title_and_artist(kind: ProviderKind, local: &LocalRelease) -> Vec<TitleSearch> {
        strategies_for(kind, local)
            .into_iter()
            .filter_map(|(label, strategy)| match strategy {
                SearchStrategy::TitleArtist { title, artist, .. } => Some((label, title, artist)),
                _ => None,
            })
            .collect()
    }

    fn search(label: &str, title: &str, artist: Option<&str>) -> TitleSearch {
        (
            label.to_string(),
            title.to_string(),
            artist.map(str::to_string),
        )
    }

    #[test]
    fn an_empty_album_tag_searches_with_the_directory_name() {
        let local = untagged();
        // The same strategies a tagged release gets, with the directory's text.
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &local),
            vec![
                search("title and artist", "Broken", Some("Straight Faced")),
                search("title only", "Broken", None),
            ]
        );
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &local),
            vec![search("title", "Broken", Some("Straight Faced"))]
        );

        // An album tag of only spaces is as empty.
        let mut blank = local.clone();
        blank.album_title = Some("  ".to_string());
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &blank),
            vec![search("title", "Broken", Some("Straight Faced"))]
        );
        // A disc folder reads as the release above it.
        let mut disc = local.clone();
        disc.directory = format!("{SCENE_DIRECTORY}/CD2");
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &disc),
            vec![search("title", "Broken", Some("Straight Faced"))]
        );
    }

    #[test]
    fn junk_tags_are_replaced_by_the_directory_name() {
        let mut junk = untagged();
        junk.album_title = Some("Unknown Title".to_string());
        junk.album_artist = Some("Unknown Artist".to_string());
        junk.artist_kind = ArtistKind::AlbumArtist;
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &junk),
            vec![search("title and artist", "Broken", Some("Straight Faced"))]
        );
        // Only the unusable half is replaced: a real artist stays, and a real
        // title stays; the directory's own reading is one more search.
        let mut artist_only = junk.clone();
        artist_only.album_artist = Some("Real Artist".to_string());
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &artist_only),
            vec![
                search("title", "Broken", Some("Real Artist")),
                search("directory name", "Broken", Some("Straight Faced")),
            ]
        );
        let mut title_only = junk.clone();
        title_only.album_title = Some("Real Title".to_string());
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &title_only),
            vec![
                search("title", "Real Title", Some("Straight Faced")),
                search("directory name", "Broken", Some("Straight Faced")),
            ]
        );
        // A title that is filler in the directory too is no title.
        let mut filler = junk.clone();
        filler.directory = "/mp3/Squeezetoy-Unknown_Title-2001-PMS".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &filler),
            vec![search("title", "Unknown Title", Some("Squeezetoy"))]
        );
    }

    #[test]
    fn an_ambiguous_directory_name_is_resolved_by_a_real_artist_tag() {
        let mut local = untagged();
        local.directory = "/mp3/Wu-Tang_Clan-Wu-Tang_Forever-1997-KSi_INT".to_string();
        // Without an artist the split is a guess, so nothing is searched.
        assert!(strategies_for(ProviderKind::Discogs, &local).is_empty());
        local.album_artist = Some("Wu-Tang Clan".to_string());
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &local),
            vec![search("title", "Wu-Tang Forever", Some("Wu-Tang Clan"))]
        );
        // A tag that fits no cut point leaves the release as it was.
        local.album_artist = Some("Someone Else".to_string());
        assert_eq!(title_and_artist(ProviderKind::Discogs, &local), vec![]);
    }

    #[test]
    fn an_album_tag_settles_an_ambiguous_directory_for_a_missing_artist() {
        let mut local = untagged();
        local.directory = "/mp3/Stresmatic_and_Droop-E-4005-WEB-2021-RAGEMP3".to_string();
        local.album_title = Some("4005".to_string());
        local.has_album_tag = true;
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &local),
            vec![search("title", "4005", Some("Stresmatic and Droop-E"))]
        );
    }

    #[test]
    fn a_release_type_word_in_the_directory_is_never_searched_as_the_title() {
        let mut local = untagged();
        for (directory, artist) in [
            ("Fazed-EP-VLS-DE-2000-GRP", "Fazed"),
            ("hybrid_soul-EP-2001-GRP", "hybrid soul"),
        ] {
            local.directory = format!("/mp3/{directory}");
            let (view, note) = search_view(&local);
            assert_eq!(view.album_title, None, "{directory}");
            assert_eq!(view.album_artist.as_deref(), Some(artist), "{directory}");
            assert_eq!(
                note,
                Some(format!(
                    "searched with artist '{artist}' read from the directory name"
                )),
                "{directory}"
            );
            for kind in [ProviderKind::MusicBrainz, ProviderKind::Discogs] {
                assert!(title_and_artist(kind, &local).is_empty(), "{directory}");
            }
        }
        // A split release names several artists: nothing is read from it.
        for directory in [
            "Rambo_and_Crucial_Unit-Split-EP-2000-GRP",
            "Samael_-_Tiamat-Live_March_Split-2001-GRP",
            "VA-Jane._Vs_Shaft-Chorus_of_Doom-Split_EP-2001-GRP",
        ] {
            local.directory = format!("/mp3/{directory}");
            let (view, note) = search_view(&local);
            assert_eq!(note, None, "{directory}");
            assert_eq!(view.album_title, None, "{directory}");
            assert_eq!(view.album_artist, None, "{directory}");
            assert!(strategies_for(ProviderKind::Discogs, &local).is_empty());
        }
        // A real title in the same shape of name is still read.
        local.directory = "/mp3/DdF-Demo-2001-PMS".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &local),
            vec![search("title", "Demo", Some("DdF"))]
        );
    }

    #[test]
    fn a_various_artists_directory_searches_the_title_as_a_compilation() {
        let mut local = untagged();
        local.directory = "/mp3/VA-Thirteen_Days_OST-2000-RNS".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &local),
            vec![search(
                "title and artist",
                "Thirteen Days",
                Some("Various Artists")
            )]
        );
    }

    #[test]
    fn a_self_titled_directory_is_searched_as_the_artists_name() {
        let mut local = untagged();
        local.directory = "/mp3/Plasticland-S-T-1985-SDR".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &local),
            vec![
                search("title", "Plasticland", Some("Plasticland")),
                search("title as tagged", "S/T", Some("Plasticland")),
            ]
        );
    }

    #[test]
    fn a_self_titled_directory_agrees_with_a_placeholder_or_the_artist_named_tag() {
        // A placeholder tag and a placeholder directory name say the same thing.
        let mut placeholder = local();
        placeholder.album_title = Some("S/T".to_string());
        placeholder.album_artist = Some("Plasticland".to_string());
        placeholder.directory = "/mp3/Plasticland-Self-Titled-1985-SDR".to_string();
        assert!(
            title_and_artist(ProviderKind::MusicBrainz, &placeholder)
                .iter()
                .all(|(label, ..)| label != "directory name")
        );
        // A tag naming the artist is what a self-titled directory name means.
        let mut named = local();
        named.album_title = Some("H2O".to_string());
        named.album_artist = Some("H2O".to_string());
        named.directory = "/mp3/H2O-Self-Titled-1996-GRP".to_string();
        assert!(
            title_and_artist(ProviderKind::MusicBrainz, &named)
                .iter()
                .all(|(label, ..)| label != "directory name")
        );
        // Another album title still adds the search.
        named.album_title = Some("Thicker Than Water".to_string());
        assert!(
            title_and_artist(ProviderKind::MusicBrainz, &named)
                .iter()
                .any(|(label, ..)| label == "directory name")
        );
    }

    #[test]
    fn real_tags_search_first_and_a_differing_directory_after() {
        // A real title and artist keep their searches; the directory adds one after them.
        let mut tagged = local();
        tagged.directory = SCENE_DIRECTORY.to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &tagged),
            vec![
                search("title and artist", "Blue", Some("Test Artist")),
                search("directory name", "Broken", Some("Straight Faced")),
            ]
        );
        // A directory that is no scene name changes nothing for an untagged release.
        let mut plain = untagged();
        plain.directory = "/music/blue".to_string();
        assert!(strategies_for(ProviderKind::MusicBrainz, &plain).is_empty());
    }

    #[tokio::test]
    async fn a_directory_search_records_what_it_read() {
        let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
        let result = discover(&provider, &untagged(), None).await.unwrap();
        assert!(
            result.notes.contains(
                &"searched with album 'Broken' and artist 'Straight Faced' read from the directory name"
                    .to_string()
            ),
            "{:?}",
            result.notes
        );
        let calls = provider.calls.lock().unwrap().clone();
        assert!(calls[0].contains("title: \"Broken\""), "{calls:?}");
        assert!(calls[0].contains("Straight Faced"), "{calls:?}");

        // A directory that fills only the artist runs no title search, so it
        // records no note; an identifier search does not use it either.
        let mut artist_only = untagged();
        artist_only.directory = "/mp3/Fazed-EP-VLS-DE-2000-GRP".to_string();
        for barcode in [None, Some("0123456789012".to_string())] {
            artist_only.barcode = barcode.clone();
            let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
            let result = discover(&provider, &artist_only, None).await.unwrap();
            assert!(
                result
                    .notes
                    .iter()
                    .all(|note| !note.contains("directory name")),
                "{barcode:?} {:?}",
                result.notes
            );
        }

        // A tagged release whose directory agrees with its tags records no such note.
        let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
        let mut tagged = local();
        tagged.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        let result = discover(&provider, &tagged, None).await.unwrap();
        assert!(
            result
                .notes
                .iter()
                .all(|note| !note.contains("directory name")),
            "{:?}",
            result.notes
        );
    }

    #[test]
    fn a_directory_that_disagrees_with_real_tags_adds_one_search() {
        // A cut-off album tag: the directory has the whole title.
        let mut cut = local();
        cut.barcode = None;
        cut.album_title = Some("Speak English Or Die (Platinum".to_string());
        cut.album_artist = Some("SOD".to_string());
        cut.directory = "/mp3/SOD-Speak_English_Or_Die_(Platinum_Edition)-2000-AMRC".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &cut),
            vec![
                search(
                    "title and artist",
                    "Speak English Or Die (Platinum",
                    Some("SOD")
                ),
                search(
                    "directory name",
                    "Speak English Or Die (Platinum Edition)",
                    Some("SOD")
                ),
            ]
        );
        // Another artist: a Various Artists tag on one artist's album.
        let mut various = local();
        various.barcode = None;
        various.album_artist = Some("Various Artists".to_string());
        various.artist_kind = ArtistKind::VariousArtists;
        various.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &various),
            vec![
                search("title", "Blue", Some("Various Artists")),
                search("directory name", "Blue", Some("Test Artist")),
            ]
        );
        // A cut-off title with an artist the directory spells differently: the
        // search keeps the tag's artist spelling, which the index matches.
        let mut acdc = local();
        acdc.barcode = None;
        acdc.album_title = Some("For Those About To Rock We S..".to_string());
        acdc.album_artist = Some("AC/DC".to_string());
        acdc.directory =
            "/mp3/ACDC-For_Those_About_To_Rock_We_Salute_You-1981-phEAR_INT".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &acdc),
            vec![
                search(
                    "title and artist",
                    "For Those About To Rock We S..",
                    Some("AC/DC")
                ),
                search(
                    "directory name",
                    "For Those About To Rock We Salute You",
                    Some("AC/DC")
                ),
            ]
        );
        // Spelling differences that are only punctuation, spaces, `&` or `and`
        // add nothing.
        let mut apostrophe = local();
        apostrophe.album_title = Some("Don't Look Back".to_string());
        apostrophe.directory = "/mp3/Test_Artist-Dont_Look_Back-2001-GRP".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &apostrophe),
            vec![search(
                "title and artist",
                "Don't Look Back",
                Some("Test Artist")
            )]
        );
        let mut ampersand = local();
        ampersand.album_artist = Some("Method Man & Redman".to_string());
        ampersand.directory = "/mp3/Method_Man_And_Redman-Blue-2001-GRP".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &ampersand),
            vec![search(
                "title and artist",
                "Blue",
                Some("Method Man & Redman")
            )]
        );
        // A Various Artists directory on one artist's tags is searched as such,
        // and only the artist is a difference.
        let mut soundtrack = local();
        soundtrack.barcode = None;
        soundtrack.album_title = Some("Thirteen Days".to_string());
        soundtrack.album_artist = Some("Trevor Jones".to_string());
        soundtrack.directory = "/mp3/VA-Thirteen_Days_OST-2000-RNS".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::Discogs, &soundtrack),
            vec![
                search("title", "Thirteen Days", Some("Trevor Jones")),
                search("directory name", "Thirteen Days", Some("Various Artists")),
            ]
        );
        let (view, _) = search_view(&soundtrack);
        assert_eq!(
            directory_search(&soundtrack, &view).unwrap().note,
            "also searched with artist 'Various Artists' read from the directory name"
        );
        // A directory with no album title and another artist keeps the tag's title.
        let mut no_title = local();
        no_title.barcode = None;
        no_title.album_artist = Some("Someone Else".to_string());
        no_title.directory = "/mp3/Fazed-EP-VLS-DE-2000-GRP".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &no_title),
            vec![
                search("title and artist", "Blue", Some("Someone Else")),
                search("directory name", "Blue", Some("Fazed")),
            ]
        );
        // A directory that agrees with the tags adds nothing.
        let mut same = local();
        same.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        assert_eq!(
            title_and_artist(ProviderKind::MusicBrainz, &same),
            vec![search("title and artist", "Blue", Some("Test Artist"))]
        );
    }

    #[test]
    fn the_directory_search_comes_right_after_the_tags_title_search() {
        // A full budget drops the optional searches after it first.
        let mut local = local();
        local.catalog_number = Some("CAT-1".to_string());
        local.is_compilation = true;
        local.directory = SCENE_DIRECTORY.to_string();
        let labels = |kind| -> Vec<String> {
            strategies_for(kind, &local)
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        };
        assert_eq!(
            labels(ProviderKind::MusicBrainz),
            [
                "barcode",
                "catalog number",
                "title and artist",
                "directory name",
                "title only"
            ]
        );
        assert_eq!(
            labels(ProviderKind::Discogs),
            ["identifier", "title", "directory name", "relaxed"]
        );
    }

    #[tokio::test]
    async fn a_directory_search_is_noted() {
        let provider = FakeProvider::new(ProviderKind::MusicBrainz, Vec::new(), Vec::new());
        let mut tagged = local();
        tagged.directory = SCENE_DIRECTORY.to_string();
        let result = discover(&provider, &tagged, None).await.unwrap();
        assert!(
            result.notes.contains(
                &"also searched with album 'Broken' and artist 'Straight Faced' read from the directory name"
                    .to_string()
            ),
            "{:?}",
            result.notes
        );
        let calls = provider.calls.lock().unwrap().clone();
        assert!(
            calls.iter().any(|call| call.contains("title: \"Broken\"")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn the_rank_floor_uses_the_directory_identity() {
        let right = "11111111-1111-1111-1111-111111111111";
        let wrong = "22222222-2222-2222-2222-222222222222";
        let mut hit = summary(right);
        hit.title = "Broken".to_string();
        hit.artists = vec!["Straight Faced".to_string()];
        hit.barcodes.clear();
        let mut unrelated = summary(wrong);
        unrelated.title = "Something Else".to_string();
        unrelated.artists = vec!["Another Band".to_string()];
        unrelated.year = None;
        unrelated.barcodes.clear();
        let local = untagged();
        let (view, _) = search_view(&local);
        let identity = matcher::rank_identity(&view);
        assert!(identity.rank(&view, &hit) >= MIN_DETAIL_RANK);
        assert!(identity.rank(&view, &unrelated) < MIN_DETAIL_RANK);

        let provider = FakeProvider::new(
            ProviderKind::Discogs,
            vec![Ok(SearchPage {
                hits: vec![unrelated, hit],
                total: Some(2),
                next_offset: None,
            })],
            vec![Ok(remote(right))],
        );
        let result = discover(&provider, &local, None).await.unwrap();
        assert_eq!(result.releases.len(), 1, "{:?}", result.notes);
        let fetches = provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("fetch:"))
            .count();
        assert_eq!(fetches, 1);
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("skipped detailed lookups for 1")),
            "{:?}",
            result.notes
        );
    }

    #[test]
    fn the_rank_floor_reads_a_directory_that_disagrees_with_the_tags() {
        let mut local = local();
        local.barcode = None;
        local.album_title = Some("Zzzz Qqqq".to_string());
        local.album_artist = Some("Someone Else Entirely".to_string());
        let mut hit = summary("11111111-1111-1111-1111-111111111111");
        hit.year = None;
        hit.barcodes.clear();
        // `Blue` by `Test Artist`: nothing in the tags names it.
        assert!(matcher::rank_search_summary(&local, &hit) < MIN_DETAIL_RANK);
        local.directory = "/mp3/Test_Artist-Blue-2001-GRP".to_string();
        assert!(
            matcher::rank_search_summary(&local, &hit) >= MIN_DETAIL_RANK,
            "{}",
            matcher::rank_search_summary(&local, &hit)
        );
    }

    #[test]
    fn the_rank_floor_uses_the_better_of_the_artist_and_the_tag_for_a_placeholder() {
        let mut local = local();
        local.album_title = Some("Self Titled".to_string());
        let mut named = summary("11111111-1111-1111-1111-111111111111");
        named.title = "Test Artist".to_string();
        named.barcodes.clear();
        let mut literal = named.clone();
        literal.title = "Self Titled".to_string();
        let rank = |hit: &SearchSummary| matcher::rank_search_summary(&local, hit);
        // Either reading of the title clears the floor with the right artist.
        assert!(rank(&named) >= 0.8, "{}", rank(&named));
        assert!(rank(&literal) >= 0.8, "{}", rank(&literal));

        // The tag as written clears the floor on its title alone, whoever made
        // the release; the artist gate decides later. It ranks below the same
        // title by the right artist.
        let mut other = literal.clone();
        other.artists = vec!["Another Band".to_string()];
        assert!(rank(&other) > MIN_DETAIL_RANK, "{}", rank(&other));
        assert!(rank(&other) < rank(&literal));
        // A different title by another artist, from another year, does not.
        let mut unrelated = other.clone();
        unrelated.title = "Something Else".to_string();
        unrelated.year = None;
        assert!(rank(&unrelated) < MIN_DETAIL_RANK, "{}", rank(&unrelated));

        // The ranking is made once per release.
        let identity = matcher::rank_identity(&local);
        assert_eq!(identity.rank(&local, &named), rank(&named));
    }
}
