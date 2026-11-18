//! Saved reviews: what the review prompt and the web review page act on.
//!
//! A release that matching cannot settle is queued with up to five candidates
//! per provider (`match_reviews`). This module holds what both review front
//! ends need: reading what a person typed as a release ID or link
//! ([`parse_entered`]), loading a release's saved reviews ([`load`]), the text
//! a candidate card shows ([`edition_details`], [`score_parts`]), and
//! accepting one saved candidate as the page showed it ([`accept_saved`]).
//!
//! The terminal prompt in [`crate::runner`] and the pages in [`crate::web`]
//! both use it, so a candidate is judged and accepted the same way in both.

// Rust guideline compliant 2026-08-14

use crate::matcher::{self, Candidate, Evaluation, LocalRelease, MatchScope, RemoteRelease};
use crate::providers::{ProviderKind, ProviderPolicy, ReleaseId};
use crate::store::{self, LocalReleaseStatus, MatchGuard, StoredCandidate};
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;

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

/// A bare ID for `provider`, as the CLI flags take it.
fn parse_bare(provider: ProviderKind, raw: &str) -> Result<ReleaseId> {
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
    let guard = match store::load_match_guard(pool, local_id).await {
        Ok(guard) => guard,
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut groups = Vec::new();
    for provider in (ProviderPolicy::Chain { deezer: true }).order() {
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
    stored
        .iter()
        .map(|saved| {
            (
                Candidate {
                    release: saved.release.clone(),
                    mapping: saved.mapping.clone(),
                    score: saved.score,
                },
                Evaluation {
                    auto_accept: saved.auto_accept,
                    match_scope: saved.match_scope,
                    blockers: saved.blockers.clone(),
                    warnings: saved.warnings.clone(),
                    evidence: saved.evidence.clone(),
                },
            )
        })
        .collect()
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

/// The edition facts of `release`: year, country, labels, catalog numbers,
/// formats and edition notes, in that order, each present only when known.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FIXTURE_MBID, mbid_release, queue, remote_release, seed_release};

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
}
