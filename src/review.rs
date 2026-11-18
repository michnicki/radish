//! Saved reviews: what the review prompt and the web review page act on.
//!
//! A release that matching cannot settle is queued with up to five candidates
//! per provider (`match_reviews`). This module holds what both review front
//! ends need: reading what a person typed as a release ID or link
//! ([`parse_entered`]), loading a release's saved reviews ([`load`]), the text
//! a candidate card shows ([`edition_details`], [`score_parts`]), and
//! accepting one saved candidate as the page showed it ([`accept_saved`]).
//!
//! A release fetched by its ID is added to a provider's saved review with
//! [`add_fetched`].
//!
//! [`Fetcher`] fetches such a release for the web server, from the network or
//! from the local dump indexes.
//!
//! The terminal prompt in [`crate::runner`] and the pages in [`crate::web`]
//! both use it, so a candidate is judged and accepted the same way in both.

// Rust guideline compliant 2026-08-14

use crate::deezer::DeezerAdapter;
use crate::discogs::DiscogsAdapter;
use crate::index::local::LocalIndex;
use crate::index::{IndexKind, IndexPaths};
use crate::matcher::{
    self, Candidate, DiscoveryEvidence, Evaluation, LocalRelease, MatchScope, RemoteRelease,
};
use crate::musicbrainz::MusicBrainzAdapter;
use crate::providers::{MetadataProvider, ProviderError, ProviderKind, ReleaseId};
use crate::store::{self, AcceptanceMethod, LocalReleaseStatus, MatchGuard, StoredCandidate};
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use tokio::sync::OnceCell;

/// The release ID a person entered for `provider`: a bare ID or a link.
///
/// MusicBrainz takes an MBID or a `musicbrainz.org` link with `release/<mbid>`
/// in its path; Discogs a number or a `discogs.com` link with
/// `release/<number>`, optionally followed by `-slug`; Deezer a number or a
/// `deezer.com` link with `album/<number>`. A language prefix in the path
/// (`/de/release/…`), surrounding whitespace, a missing `https://`, a trailing
/// slash, and any query or fragment are ignored.
///
/// # Errors
///
/// When the text is neither, when the link belongs to another provider
/// (`that is a Discogs link; choose Discogs`) or to no provider, and for a
/// Discogs master link.
pub fn parse_entered(provider: ProviderKind, raw: &str) -> Result<ReleaseId> {
    let raw = raw.trim();
    if raw.contains("://") {
        return parse_link(provider, raw);
    }
    // A link pasted without its scheme: `www.discogs.com/release/249504`.
    if raw.contains('/') {
        return parse_link(provider, &format!("https://{raw}"));
    }
    parse_bare(provider, raw)
}

/// A bare release ID for `provider`, as CLI flags and stored reviews hold it.
///
/// Unlike [`parse_entered`], it takes no link.
///
/// # Errors
///
/// When `raw` is not an ID of that provider.
pub fn parse_bare(provider: ProviderKind, raw: &str) -> Result<ReleaseId> {
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
        ProviderKind::Deezer => {
            let id = raw
                .parse::<u64>()
                .with_context(|| format!("'{raw}' is not a Deezer album ID"))?;
            Ok(ReleaseId::Deezer(id))
        }
    }
}

/// The provider whose site `host` is: the domain itself or a subdomain of it.
fn link_provider(host: &str) -> Option<ProviderKind> {
    let host = host.to_ascii_lowercase();
    let within = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if within("musicbrainz.org") {
        Some(ProviderKind::MusicBrainz)
    } else if within("discogs.com") {
        Some(ProviderKind::Discogs)
    } else if within("deezer.com") {
        Some(ProviderKind::Deezer)
    } else {
        None
    }
}

/// The release a provider link names, read from its path.
fn parse_link(provider: ProviderKind, raw: &str) -> Result<ReleaseId> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("'{raw}' is not a link"))?;
    match url.host_str().and_then(link_provider) {
        Some(owner) if owner == provider => {}
        Some(owner) => bail!("that is a {} link; choose {}", owner.label(), owner.label()),
        None => bail!("that is not a {} link", provider.label()),
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    if provider == ProviderKind::Discogs && segments.contains(&"master") {
        bail!("a Discogs master is not a release; open one of its releases and use that link");
    }
    let (kind, noun) = match provider {
        ProviderKind::MusicBrainz | ProviderKind::Discogs => ("release", "a release"),
        ProviderKind::Deezer => ("album", "an album"),
    };
    let Some(value) = segments
        .iter()
        .position(|segment| *segment == kind)
        .and_then(|position| segments.get(position + 1))
    else {
        bail!("that {} link does not name {noun}", provider.label());
    };
    // Discogs appends a slug: `release/249504-Artist-Title`. An MBID has
    // hyphens of its own, so only Discogs is cut.
    let value = match provider {
        ProviderKind::Discogs => value.split('-').next().unwrap_or(value),
        ProviderKind::MusicBrainz | ProviderKind::Deezer => value,
    };
    parse_bare(provider, value)
}

/// Whether a saved review can still be acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupState {
    /// Saved for the release as it is now.
    Current,
    /// Saved before the files or the accepted match changed; its mapping
    /// indexes files that may have moved, so it cannot be accepted.
    OutOfDate,
    /// The stored snapshot cannot be decoded; the message says why.
    Unreadable(String),
}

/// One provider's saved candidates for a release.
#[derive(Debug, Clone)]
pub struct ReviewGroup {
    /// The provider that proposed the candidates.
    pub provider: ProviderKind,
    /// Whether the group can still be accepted from.
    pub state: GroupState,
    /// Why matching queued the release instead of accepting it.
    pub reasons: Vec<String>,
    /// Each candidate with its evaluation as it was stored, best first.
    pub candidates: Vec<(Candidate, Evaluation)>,
}

/// One release with everything its review page shows.
#[derive(Debug, Clone)]
pub struct ReleaseReview {
    /// The release as matching loads it.
    pub release: LocalRelease,
    /// The status label and accepted match, as `releases` shows them.
    pub status: LocalReleaseStatus,
    /// The release's current source revision and accepted match.
    pub guard: MatchGuard,
    /// Saved reviews in the order matching tries providers.
    pub groups: Vec<ReviewGroup>,
}

/// Loads release `local_id` with each provider's saved review.
///
/// A group whose stored guard differs from the release's current one is
/// [`GroupState::OutOfDate`]; one that cannot be decoded is
/// [`GroupState::Unreadable`], and the others still load.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn load(pool: &SqlitePool, local_id: i64) -> Result<Option<ReleaseReview>> {
    let Some(release) = load_release(pool, local_id).await? else {
        return Ok(None);
    };
    let Some(status) = store::release_status(pool, local_id).await? else {
        return Ok(None);
    };
    let Some(guard) = current_guard(pool, local_id).await? else {
        return Ok(None);
    };
    let mut groups = Vec::new();
    for provider in ProviderKind::CHAIN {
        match store::load_review(pool, local_id, provider).await {
            Ok(None) => {}
            Ok(Some(review)) => groups.push(ReviewGroup {
                provider,
                state: if review.guard == guard && guard.source_revision == release.revision {
                    GroupState::Current
                } else {
                    GroupState::OutOfDate
                },
                reasons: review.reasons,
                candidates: candidate_pairs(&review.candidates),
            }),
            Err(error) if error.downcast_ref::<sqlx::Error>().is_some() => return Err(error),
            Err(error) => groups.push(ReviewGroup {
                provider,
                state: GroupState::Unreadable(format!("{error:#}")),
                reasons: Vec::new(),
                candidates: Vec::new(),
            }),
        }
    }
    Ok(Some(ReleaseReview {
        release,
        status,
        guard,
        groups,
    }))
}

/// The local release `local_id` as matching loads it, if it exists.
async fn load_release(pool: &SqlitePool, local_id: i64) -> Result<Option<LocalRelease>> {
    Ok(store::load_selected_releases(pool, None, Some(local_id))
        .await?
        .into_iter()
        .next())
}

/// Saved candidates as the matcher's types, each with its stored evaluation.
pub fn candidate_pairs(stored: &[StoredCandidate]) -> Vec<(Candidate, Evaluation)> {
    stored.iter().cloned().map(pair_of).collect()
}

/// One saved candidate as the matcher's types, with its stored evaluation.
fn pair_of(saved: StoredCandidate) -> (Candidate, Evaluation) {
    (
        Candidate {
            release: saved.release,
            mapping: saved.mapping,
            score: saved.score,
        },
        Evaluation {
            auto_accept: saved.auto_accept,
            match_scope: saved.match_scope,
            blockers: saved.blockers,
            warnings: saved.warnings,
            evidence: saved.evidence,
        },
    )
}

/// `candidate` and its `evaluation` as a saved review stores them.
pub fn stored_candidate(candidate: &Candidate, evaluation: &Evaluation) -> StoredCandidate {
    StoredCandidate {
        release: candidate.release.clone(),
        mapping: candidate.mapping.clone(),
        score: candidate.score,
        auto_accept: evaluation.auto_accept,
        match_scope: evaluation.match_scope,
        blockers: evaluation.blockers.clone(),
        warnings: evaluation.warnings.clone(),
        evidence: evaluation.evidence.clone(),
    }
}

/// The edition facts of `release`, in the order a card lists them.
///
/// These are year, country, labels, catalog numbers, formats and edition
/// notes, each present only when known.
pub fn edition_details(release: &RemoteRelease) -> Vec<String> {
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
    details
}

/// Each score component as `name value×weight`, as the candidate was judged.
///
/// An album match does not count the year, so it is left out and the other
/// weights are scaled to sum to one again.
pub fn score_parts(candidate: &Candidate, evaluation: &Evaluation) -> Vec<String> {
    let album = evaluation.match_scope == MatchScope::Album;
    candidate
        .score
        .components()
        .iter()
        .filter(|(name, _, _)| !(album && *name == "year"))
        .map(|(name, value, weight)| {
            let weight = if album {
                weight / (1.0 - matcher::WEIGHT_YEAR)
            } else {
                *weight
            };
            format!("{name} {value:.2}×{weight:.2}")
        })
        .collect()
}

/// What became of an accept made from a review page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// The candidate is the release's accepted match now.
    Accepted,
    /// The release is gone, or it or its accepted match changed after the page
    /// was built; the page must be loaded again.
    Changed,
    /// The provider's saved review does not hold that release.
    NotFound,
    /// The mapping cannot be written; the mapping notes say why.
    CannotApply(Vec<String>),
}

/// Accepts the saved candidate `id` for release `local_id`, as the page showed it.
///
/// `shown` is the guard the page was built from: the release, the provider's
/// saved review and the accepted match must all still be as they were then. The
/// decision is stored as `manual`, at the scope it was judged at, and clears the
/// release's saved reviews.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn accept_saved(
    pool: &SqlitePool,
    local_id: i64,
    id: &ReleaseId,
    shown: &MatchGuard,
) -> Result<AcceptOutcome> {
    let Some(release) = load_release(pool, local_id).await? else {
        return Ok(AcceptOutcome::Changed);
    };
    let Some(guard) = current_guard(pool, local_id).await? else {
        return Ok(AcceptOutcome::Changed);
    };
    if guard != *shown || release.revision != shown.source_revision {
        return Ok(AcceptOutcome::Changed);
    }
    let Some(review) = store::load_review(pool, local_id, id.provider()).await? else {
        return Ok(AcceptOutcome::NotFound);
    };
    if review.guard != guard {
        return Ok(AcceptOutcome::Changed);
    }
    let Some(saved) = review
        .candidates
        .into_iter()
        .find(|saved| saved.release.id == *id)
    else {
        return Ok(AcceptOutcome::NotFound);
    };
    let (candidate, evaluation) = pair_of(saved);
    if !evaluation.can_apply(&release, &candidate) {
        return Ok(AcceptOutcome::CannotApply(candidate.mapping.notes));
    }
    match store::accept_match(
        pool,
        &release,
        &candidate,
        AcceptanceMethod::Manual,
        evaluation.match_scope,
        &guard,
    )
    .await
    {
        Ok(()) => Ok(AcceptOutcome::Accepted),
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
            Ok(AcceptOutcome::Changed)
        }
        Err(error) => {
            // Another decision may have got in between; `accept_match` refuses
            // that with a plain error, so look at the guard again.
            if current_guard(pool, local_id).await?.as_ref() == Some(&guard) {
                Err(error)
            } else {
                Ok(AcceptOutcome::Changed)
            }
        }
    }
}

/// The reason stored with a review that holds only a release entered by hand.
pub const ENTERED_REASON: &str = "entered by hand";

/// What became of a release fetched from a review page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// The release is the first candidate of its provider's saved review.
    Added,
    /// The release is gone or changed after the page was built.
    Changed,
}

/// Adds `remote`, fetched by its ID, to release `local_id`'s saved review.
///
/// `remote` is mapped and judged as a direct selection (the way `[i]d` judges
/// it), then stored first in its provider's review through
/// [`store::add_review_candidate`]. A release whose mapping cannot be applied
/// is stored all the same, so the page can show why.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn add_fetched(
    pool: &SqlitePool,
    local_id: i64,
    revision: &str,
    remote: RemoteRelease,
) -> Result<AddOutcome> {
    let Some(release) = load_release(pool, local_id).await? else {
        return Ok(AddOutcome::Changed);
    };
    if release.revision != revision {
        return Ok(AddOutcome::Changed);
    }
    let candidate = Candidate::new(&release, remote);
    let evaluation = matcher::evaluate(&release, &candidate, None, &DiscoveryEvidence::default());
    let stored = stored_candidate(&candidate, &evaluation);
    let added = store::add_review_candidate(pool, &release, stored, ENTERED_REASON).await?;
    Ok(if added {
        AddOutcome::Added
    } else {
        AddOutcome::Changed
    })
}

/// Where a [`Fetcher`] reads MusicBrainz and Discogs releases from.
#[derive(Debug, Clone)]
pub enum Source {
    /// The services' web APIs; Discogs needs `RADISH_DISCOGS_TOKEN`.
    Network,
    /// The local dump indexes in this directory (`serve --offline`).
    Indexes(IndexPaths),
}

/// A MusicBrainz or Discogs client: the network adapter or a local index.
enum Client<A> {
    Network(A),
    Index(LocalIndex),
}

impl<A: MetadataProvider> Client<A> {
    async fn fetch(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
        match self {
            Self::Network(adapter) => adapter.fetch_release(id).await,
            Self::Index(index) => index.fetch_release(id).await,
        }
    }
}

/// Why a fetch from a review page failed.
#[derive(Debug, Clone)]
pub enum FetchError {
    /// The provider's client cannot be built: no token, or a missing index.
    Unavailable {
        /// The provider whose client is missing.
        provider: ProviderKind,
        /// Why it cannot be built.
        message: String,
    },
    /// The provider answered with an error.
    Provider(ProviderError),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { provider, message } => {
                write!(f, "{} is not available: {message}", provider.label())
            }
            Self::Provider(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FetchError {}

/// Fetches one release by its ID for the web review page.
///
/// Each provider's client is built the first time a release of that provider
/// is fetched, so a server where nobody enters an ID contacts and opens
/// nothing. A client that cannot be built is not remembered, so an index that
/// is built after a failed fetch is used by the next one. A new Discogs token
/// or a rebuilt index needs a restart of `serve`: the token is read from the
/// process environment, and a client that opened an index keeps it. Deezer has
/// no dump, so it always uses the network.
pub struct Fetcher {
    source: Source,
    musicbrainz: OnceCell<Client<MusicBrainzAdapter>>,
    discogs: OnceCell<Client<DiscogsAdapter>>,
    deezer: OnceCell<DeezerAdapter>,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Fetcher {
    /// A fetcher reading from `source`; nothing is built yet.
    pub fn new(source: Source) -> Self {
        Self {
            source,
            musicbrainz: OnceCell::new(),
            discogs: OnceCell::new(),
            deezer: OnceCell::new(),
        }
    }

    /// Fetches release `id` from its provider.
    ///
    /// # Errors
    ///
    /// [`FetchError::Unavailable`] when the provider's client cannot be built,
    /// and [`FetchError::Provider`] when the provider fails or does not know
    /// the release.
    pub async fn fetch(&self, id: ReleaseId) -> Result<RemoteRelease, FetchError> {
        let provider = id.provider();
        let unavailable = |error: anyhow::Error| FetchError::Unavailable {
            provider,
            message: format!("{error:#}"),
        };
        let fetched = match provider {
            ProviderKind::MusicBrainz => {
                let client = self
                    .musicbrainz
                    .get_or_try_init(|| async {
                        match &self.source {
                            Source::Network => MusicBrainzAdapter::new().map(Client::Network),
                            Source::Indexes(paths) => {
                                LocalIndex::open(paths, IndexKind::MusicBrainz)
                                    .await
                                    .map(Client::Index)
                            }
                        }
                    })
                    .await
                    .map_err(unavailable)?;
                client.fetch(id).await
            }
            ProviderKind::Discogs => {
                let client = self
                    .discogs
                    .get_or_try_init(|| async {
                        match &self.source {
                            Source::Network => DiscogsAdapter::from_env()
                                .map_err(anyhow::Error::new)
                                .map(Client::Network),
                            Source::Indexes(paths) => LocalIndex::open(paths, IndexKind::Discogs)
                                .await
                                .map(Client::Index),
                        }
                    })
                    .await
                    .map_err(unavailable)?;
                client.fetch(id).await
            }
            ProviderKind::Deezer => {
                let client = self
                    .deezer
                    .get_or_try_init(|| async { DeezerAdapter::new() })
                    .await
                    .map_err(unavailable)?;
                client.fetch_release(id).await
            }
        };
        fetched.map_err(FetchError::Provider)
    }
}

/// The release's current guard, or `None` when the release is gone.
///
/// # Errors
///
/// When the database cannot be read.
pub async fn current_guard(pool: &SqlitePool, local_id: i64) -> Result<Option<MatchGuard>> {
    match store::load_match_guard(pool, local_id).await {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexPaths;
    use crate::test_support::{
        FIXTURE_MBID, OTHER_MBID, ScratchDir, mbid_release, queue, remote_release, seed_release,
    };

    const MBID: &str = FIXTURE_MBID;

    #[test]
    fn entered_ids_take_bare_ids_and_links() {
        let expected = ReleaseId::MusicBrainz(MBID.parse().unwrap());
        let musicbrainz = [
            MBID.to_string(),
            format!(" https://musicbrainz.org/release/{MBID} "),
            format!("https://beta.musicbrainz.org/release/{MBID}/discids?x=1#top"),
            format!("musicbrainz.org/release/{MBID}"),
        ];
        for raw in &musicbrainz {
            assert_eq!(
                parse_entered(ProviderKind::MusicBrainz, raw).unwrap(),
                expected,
                "{raw}"
            );
        }
        for raw in [
            "249504",
            "https://www.discogs.com/release/249504-Artist-Title",
            "https://www.discogs.com/de/release/249504",
            "HTTPS://WWW.DISCOGS.COM/release/249504?ev=rb",
            "www.discogs.com/release/249504",
        ] {
            assert_eq!(
                parse_entered(ProviderKind::Discogs, raw).unwrap(),
                ReleaseId::Discogs(249_504),
                "{raw}"
            );
        }
        for raw in [
            "302127",
            "https://www.deezer.com/album/302127",
            "https://www.deezer.com/en/album/302127?utm=x",
            "https://www.deezer.com/album/302127/",
        ] {
            assert_eq!(
                parse_entered(ProviderKind::Deezer, raw).unwrap(),
                ReleaseId::Deezer(302_127),
                "{raw}"
            );
        }
    }

    #[test]
    fn entered_links_must_belong_to_the_chosen_provider() {
        let link = format!("https://musicbrainz.org/release/{MBID}");
        assert_eq!(
            parse_entered(ProviderKind::Discogs, &link)
                .unwrap_err()
                .to_string(),
            "that is a MusicBrainz link; choose MusicBrainz"
        );
        assert_eq!(
            parse_entered(ProviderKind::MusicBrainz, "https://example.org/release/x")
                .unwrap_err()
                .to_string(),
            "that is not a MusicBrainz link"
        );
        // A host that only ends with the name is not the provider.
        assert_eq!(
            parse_entered(ProviderKind::Deezer, "https://evildeezer.com/album/1")
                .unwrap_err()
                .to_string(),
            "that is not a Deezer link"
        );
        let master = parse_entered(
            ProviderKind::Discogs,
            "https://www.discogs.com/master/1234-Title",
        )
        .unwrap_err()
        .to_string();
        assert!(master.contains("master"), "{master}");
        assert!(parse_entered(ProviderKind::MusicBrainz, "not-an-mbid").is_err());
        assert_eq!(
            parse_entered(ProviderKind::Deezer, "https://www.deezer.com/artist/27")
                .unwrap_err()
                .to_string(),
            "that Deezer link does not name an album"
        );
        assert!(parse_entered(ProviderKind::Discogs, "https://www.discogs.com/release/").is_err());
        assert_eq!(
            parse_entered(ProviderKind::Discogs, "abc")
                .unwrap_err()
                .to_string(),
            "'abc' is not a Discogs release ID"
        );
    }

    #[tokio::test]
    async fn load_marks_each_saved_group_current_out_of_date_or_unreadable() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(ReleaseId::Discogs(42), "Blue", &titles),
        )
        .await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;

        let review = load(&pool, release.id).await.unwrap().unwrap();
        let providers: Vec<ProviderKind> =
            review.groups.iter().map(|group| group.provider).collect();
        assert_eq!(
            providers,
            vec![ProviderKind::MusicBrainz, ProviderKind::Discogs]
        );
        assert!(
            review
                .groups
                .iter()
                .all(|group| group.state == GroupState::Current)
        );
        assert_eq!(review.groups[0].candidates.len(), 1);
        assert_eq!(review.groups[0].reasons, vec!["uncertain".to_string()]);
        assert_eq!(review.status.id, release.id);
        assert_eq!(review.guard.source_revision, release.revision);

        sqlx::query("UPDATE match_reviews SET source_revision = 'old' WHERE provider = 'discogs'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE match_reviews SET candidates_json = 'nonsense' WHERE provider = 'musicbrainz'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert!(matches!(review.groups[0].state, GroupState::Unreadable(_)));
        assert_eq!(review.groups[1].state, GroupState::OutOfDate);

        assert!(load(&pool, release.id + 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_review_saved_under_another_accepted_match_is_out_of_date() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();
        let outcome = accept_saved(&pool, release.id, &mbid_release(FIXTURE_MBID), &shown)
            .await
            .unwrap();
        assert_eq!(outcome, AcceptOutcome::Accepted);
        queue(
            &pool,
            &release,
            remote_release(ReleaseId::Discogs(42), "Blue", &titles),
        )
        .await;
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(review.groups[0].state, GroupState::Current);

        sqlx::query("UPDATE match_reviews SET expected_match_id = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(review.groups[0].provider, ProviderKind::Discogs);
        assert_eq!(review.groups[0].state, GroupState::OutOfDate);
    }

    #[test]
    fn card_text_lists_edition_details_and_weighted_scores() {
        use crate::matcher::{Candidate, Evaluation, MatchScope};
        use crate::test_support::local_release;
        let local = local_release(1, "/music/a", "Blue", &["One"]);
        let remote = remote_release(ReleaseId::Discogs(42), "Blue", &["One"]);
        assert_eq!(
            edition_details(&remote),
            vec!["year 2001", "US", "Label", "catno CAT-1", "CD"]
        );
        let candidate = Candidate::new(&local, remote);
        let mut evaluation = Evaluation {
            auto_accept: false,
            match_scope: MatchScope::Edition,
            blockers: Vec::new(),
            warnings: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(
            score_parts(&candidate, &evaluation)
                .iter()
                .any(|part| part.starts_with("year "))
        );
        evaluation.match_scope = MatchScope::Album;
        assert!(
            !score_parts(&candidate, &evaluation)
                .iter()
                .any(|part| part.starts_with("year "))
        );
    }

    #[tokio::test]
    async fn accepting_a_saved_candidate_matches_the_release_and_clears_its_queue() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();

        let outcome = accept_saved(&pool, release.id, &mbid_release(FIXTURE_MBID), &shown)
            .await
            .unwrap();
        assert_eq!(outcome, AcceptOutcome::Accepted);
        let status = store::release_status(&pool, release.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.status, store::STATUS_MATCHED);
        assert_eq!(status.provider, Some(ProviderKind::MusicBrainz));
        let (reviews, method): (i64, String) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM match_reviews), acceptance_method FROM release_matches",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((reviews, method.as_str()), (0, "manual"));

        // A candidate saved after the match replaces it.
        queue(
            &pool,
            &release,
            remote_release(ReleaseId::Discogs(42), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();
        let outcome = accept_saved(&pool, release.id, &ReleaseId::Discogs(42), &shown)
            .await
            .unwrap();
        assert_eq!(outcome, AcceptOutcome::Accepted);
        let status = store::release_status(&pool, release.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.provider, Some(ProviderKind::Discogs));
    }

    #[tokio::test]
    async fn accepting_refuses_what_the_page_no_longer_shows() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();
        let musicbrainz = mbid_release(FIXTURE_MBID);

        let other_revision = MatchGuard {
            source_revision: "other".to_string(),
            accepted_match_id: None,
        };
        assert_eq!(
            accept_saved(&pool, release.id, &musicbrainz, &other_revision)
                .await
                .unwrap(),
            AcceptOutcome::Changed
        );
        assert_eq!(
            accept_saved(&pool, release.id, &mbid_release(OTHER_MBID), &shown)
                .await
                .unwrap(),
            AcceptOutcome::NotFound
        );
        assert_eq!(
            accept_saved(&pool, release.id, &ReleaseId::Deezer(7), &shown)
                .await
                .unwrap(),
            AcceptOutcome::NotFound
        );
        assert_eq!(
            accept_saved(&pool, release.id + 100, &musicbrainz, &shown)
                .await
                .unwrap(),
            AcceptOutcome::Changed
        );
        sqlx::query("UPDATE match_reviews SET source_revision = 'old'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            accept_saved(&pool, release.id, &musicbrainz, &shown)
                .await
                .unwrap(),
            AcceptOutcome::Changed
        );
        let status = store::release_status(&pool, release.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.status,
            store::STATUS_UNMATCHED,
            "nothing was accepted"
        );
    }

    #[tokio::test]
    async fn a_page_built_before_another_decision_cannot_replace_it() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        queue(
            &pool,
            &release,
            remote_release(ReleaseId::Discogs(42), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();

        // Another page accepts the Discogs candidate first.
        let outcome = accept_saved(&pool, release.id, &ReleaseId::Discogs(42), &shown)
            .await
            .unwrap();
        assert_eq!(outcome, AcceptOutcome::Accepted);

        let outcome = accept_saved(&pool, release.id, &mbid_release(FIXTURE_MBID), &shown)
            .await
            .unwrap();
        assert_eq!(outcome, AcceptOutcome::Changed);
        let status = store::release_status(&pool, release.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.provider, Some(ProviderKind::Discogs), "not replaced");
    }

    #[tokio::test]
    async fn a_mapping_that_cannot_be_applied_is_refused() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One", "Two", "Three"]).await;
        // Two provider tracks for three files: the mapping is incomplete.
        queue(
            &pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]),
        )
        .await;
        let shown = store::load_match_guard(&pool, release.id).await.unwrap();
        let outcome = accept_saved(&pool, release.id, &mbid_release(FIXTURE_MBID), &shown)
            .await
            .unwrap();
        match outcome {
            AcceptOutcome::CannotApply(notes) => assert!(!notes.is_empty()),
            other => panic!("expected CannotApply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fetched_release_goes_first_in_its_providers_review() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        let other = || remote_release(mbid_release(OTHER_MBID), "Blue", &titles);
        let first = || remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles);
        queue(&pool, &release, first()).await;
        let ids = |review: &ReleaseReview, index: usize| -> Vec<String> {
            review.groups[index]
                .candidates
                .iter()
                .map(|(candidate, _)| candidate.release.id.storage_id())
                .collect()
        };

        let added = add_fetched(&pool, release.id, &release.revision, other())
            .await
            .unwrap();
        assert_eq!(added, AddOutcome::Added);
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(ids(&review, 0), vec![OTHER_MBID, FIXTURE_MBID]);
        assert_eq!(review.groups[0].reasons, vec!["uncertain".to_string()]);

        // Fetching a saved one again moves it to the front without a copy.
        add_fetched(&pool, release.id, &release.revision, first())
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(ids(&review, 0), vec![FIXTURE_MBID, OTHER_MBID]);

        // Another provider gets a group of its own.
        let discogs = remote_release(ReleaseId::Discogs(42), "Blue", &titles);
        add_fetched(&pool, release.id, &release.revision, discogs)
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(ids(&review, 1), vec!["42"]);
        assert_eq!(review.groups[1].reasons, vec![ENTERED_REASON.to_string()]);
        assert_eq!(review.groups[1].state, GroupState::Current);

        // An out-of-date group is replaced by the fetched release alone.
        sqlx::query(
            "UPDATE match_reviews SET source_revision = 'old' WHERE provider = 'musicbrainz'",
        )
        .execute(&pool)
        .await
        .unwrap();
        add_fetched(&pool, release.id, &release.revision, other())
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(ids(&review, 0), vec![OTHER_MBID]);
        assert_eq!(review.groups[0].state, GroupState::Current);
        assert_eq!(review.groups[0].reasons, vec![ENTERED_REASON.to_string()]);

        // An unreadable group is replaced too, with the entered reason.
        sqlx::query(
            "UPDATE match_reviews SET candidates_json = 'nonsense' WHERE provider = 'musicbrainz'",
        )
        .execute(&pool)
        .await
        .unwrap();
        add_fetched(&pool, release.id, &release.revision, other())
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        assert_eq!(ids(&review, 0), vec![OTHER_MBID]);
        assert_eq!(review.groups[0].state, GroupState::Current);
        assert_eq!(review.groups[0].reasons, vec![ENTERED_REASON.to_string()]);

        // A release that cannot be applied is still stored, so the page can say why.
        let short = remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One"]);
        add_fetched(&pool, release.id, &release.revision, short)
            .await
            .unwrap();
        let review = load(&pool, release.id).await.unwrap().unwrap();
        let (candidate, evaluation) = &review.groups[0].candidates[0];
        assert!(!evaluation.can_apply(&review.release, candidate));

        assert_eq!(
            add_fetched(&pool, release.id, "stale", other())
                .await
                .unwrap(),
            AddOutcome::Changed
        );
        assert_eq!(
            add_fetched(&pool, release.id + 100, &release.revision, other())
                .await
                .unwrap(),
            AddOutcome::Changed
        );
    }

    #[tokio::test]
    async fn a_missing_index_is_reported_and_tried_again_next_time() {
        let dir = ScratchDir::new("fetcher-missing-index");
        let fetcher = Fetcher::new(Source::Indexes(IndexPaths::from_dir(dir.path())));
        for _ in 0..2 {
            let error = fetcher.fetch(mbid_release(FIXTURE_MBID)).await.unwrap_err();
            assert!(
                matches!(
                    &error,
                    FetchError::Unavailable {
                        provider: ProviderKind::MusicBrainz,
                        ..
                    }
                ),
                "{error}"
            );
            assert!(error.to_string().contains("index is missing"), "{error}");
            assert!(fetcher.musicbrainz.get().is_none(), "the failure was kept");
        }
    }

    #[test]
    fn fetching_can_run_on_the_web_servers_tasks() {
        fn shared<T: Send + Sync>(_: &T) {}
        fn sendable<T: Send>(_: T) {}
        let fetcher = Fetcher::new(Source::Network);
        shared(&fetcher);
        // Building the future is enough to check it; it is never polled.
        sendable(fetcher.fetch(ReleaseId::Deezer(1)));
    }
}
