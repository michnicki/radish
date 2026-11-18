//! Saved reviews: what the review prompt and the web review page act on.
//!
//! A release that matching cannot settle is queued with up to five candidates
//! per provider (`match_reviews`). This module holds what both review front
//! ends need: reading what a person typed as a release ID or link
//! ([`parse_entered`]), and, in later sections, loading a release's saved
//! reviews, accepting one candidate, and adding a release fetched by its ID.
//!
//! The terminal prompt in [`crate::runner`] and the pages in [`crate::web`]
//! both use it, so a candidate is judged and accepted the same way in both.

// Rust guideline compliant 2026-08-14

use crate::providers::{ProviderKind, ReleaseId};
use anyhow::{Context, Result, bail};

/// The release ID a person entered for `provider`: a bare ID or a link.
///
/// MusicBrainz takes an MBID or a `musicbrainz.org` link with `release/<mbid>`
/// in its path; Discogs a number or a `discogs.com` link with
/// `release/<number>`, optionally followed by `-slug`; Deezer a number or a
/// `deezer.com` link with `album/<number>`. A language prefix in the path
/// (`/de/release/…`), surrounding whitespace, and any query or fragment are
/// ignored.
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
    let kind = match provider {
        ProviderKind::MusicBrainz | ProviderKind::Discogs => "release",
        ProviderKind::Deezer => "album",
    };
    let Some(value) = segments
        .iter()
        .position(|segment| *segment == kind)
        .and_then(|position| segments.get(position + 1))
    else {
        bail!("that {} link does not name a {kind}", provider.label());
    };
    // Discogs appends a slug: `release/249504-Artist-Title`. An MBID has
    // hyphens of its own, so only Discogs is cut.
    let value = match provider {
        ProviderKind::Discogs => value.split('-').next().unwrap_or(value),
        ProviderKind::MusicBrainz | ProviderKind::Deezer => value,
    };
    parse_bare(provider, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";

    #[test]
    fn entered_ids_take_bare_ids_and_links() {
        let expected = ReleaseId::MusicBrainz(MBID.parse().unwrap());
        let musicbrainz = [
            MBID.to_string(),
            format!(" https://musicbrainz.org/release/{MBID} "),
            format!("https://beta.musicbrainz.org/release/{MBID}/discids?x=1#top"),
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
        assert!(parse_entered(ProviderKind::Deezer, "https://www.deezer.com/artist/27").is_err());
        assert_eq!(
            parse_entered(ProviderKind::Discogs, "abc")
                .unwrap_err()
                .to_string(),
            "'abc' is not a Discogs release ID"
        );
    }
}
