//! The review pages: the queue of releases waiting for a decision, one
//! release's saved candidates, and the form that accepts one or fetches
//! another by its ID.
//!
//! Accepting and fetching are the server's second write, guarded like the
//! folder form: only a loopback server takes them, with this process's form
//! token, from this site. Every form also carries the release's source
//! revision, so a decision made on an outdated page is refused (`409`).

// Rust guideline compliant 2026-08-14

use super::{
    AppError, AppState, Nav, PAGE_SIZE, escape_html, folder_href, page, page_param, pager_with,
    render_match,
};
use crate::layout::plural;
use crate::providers::{ProviderKind, ProviderPolicy};
use crate::store;
use axum::extract::{Query, State};
use axum::response::Html;
use serde::Deserialize;

/// The query of `/reviews` and `/review`.
///
/// Values stay text so a malformed one falls back instead of failing the request.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ReviewParams {
    provider: Option<String>,
    accepted: Option<String>,
    page: Option<String>,
}

impl ReviewParams {
    /// The queue filter; an unknown provider is ignored.
    fn filter(&self) -> Option<ProviderKind> {
        ProviderKind::parse(self.provider.as_deref()?)
    }

    /// The release accepted just before this page was loaded.
    fn accepted(&self) -> Option<i64> {
        self.accepted.as_deref()?.trim().parse().ok()
    }

    /// The 1-based page, defaulting to 1 for anything unusable.
    fn page(&self) -> u64 {
        page_param(self.page.as_deref())
    }
}

/// `/review?release=N`, with the queue filter and the release just accepted.
fn review_href(release: i64, filter: Option<ProviderKind>, accepted: Option<i64>) -> String {
    let mut href = format!("/review?release={release}");
    if let Some(provider) = filter {
        href.push_str(&format!("&provider={provider}"));
    }
    if let Some(id) = accepted {
        href.push_str(&format!("&accepted={id}"));
    }
    href
}

/// `/reviews`, with the queue filter and the release just accepted.
fn queue_href(filter: Option<ProviderKind>, accepted: Option<i64>) -> String {
    let mut params = Vec::new();
    if let Some(provider) = filter {
        params.push(format!("provider={provider}"));
    }
    if let Some(id) = accepted {
        params.push(format!("accepted={id}"));
    }
    if params.is_empty() {
        "/reviews".to_string()
    } else {
        format!("/reviews?{}", params.join("&"))
    }
}

/// `/reviews`: the queue of releases waiting for a decision.
pub(super) async fn queue_page(
    State(state): State<AppState>,
    Query(params): Query<ReviewParams>,
) -> Result<Html<String>, AppError> {
    let filter = params.filter();
    let page_number = params.page();
    let offset = (page_number - 1) * PAGE_SIZE as u64;
    // One row past the page tells us whether a next page exists.
    let mut rows =
        store::queue_page(&state.pool, filter, PAGE_SIZE as i64 + 1, offset as i64).await?;
    let has_next = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let total = store::queue_count(&state.pool, filter).await?;
    let accepted = accepted_status(&state, params.accepted()).await?;
    let body = render_queue(
        &rows,
        total,
        filter,
        accepted.as_ref(),
        page_number,
        has_next,
    );
    Ok(Html(page("Review", Nav::Review, &body)))
}

/// The release accepted just before, when it is matched now.
async fn accepted_status(
    state: &AppState,
    id: Option<i64>,
) -> anyhow::Result<Option<store::LocalReleaseStatus>> {
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(store::release_status(&state.pool, id)
        .await?
        .filter(|status| status.status == store::STATUS_MATCHED))
}

/// `Accepted musicbrainz <id> (title) for #12 Artist — Album.`
fn render_accepted(status: &store::LocalReleaseStatus) -> String {
    format!(
        "<p class=\"notice\">Accepted {} for <a href=\"{}\">#{} {} — {}</a>.</p>",
        render_match(status),
        escape_html(&review_href(status.id, None, None)),
        status.id,
        escape_html(status.album_artist.as_deref().unwrap_or("(unknown artist)")),
        escape_html(status.album_title.as_deref().unwrap_or("(no album tag)")),
    )
}

/// Links that switch the queue between all providers and one.
fn render_filters(filter: Option<ProviderKind>) -> String {
    let link = |value: Option<ProviderKind>, label: &str| {
        let current = if value == filter {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!(
            "<a href=\"{}\"{current}>{label}</a>",
            escape_html(&queue_href(value, None))
        )
    };
    let mut links = vec![link(None, "All")];
    links.extend(
        ProviderPolicy::Chain { deezer: true }
            .order()
            .into_iter()
            .map(|provider| link(Some(provider), provider.label())),
    );
    format!("<p class=\"filters\">{}</p>", links.join(""))
}

/// The queue page body: notice, filters, the table and the pager.
fn render_queue(
    rows: &[store::QueuedRelease],
    total: i64,
    filter: Option<ProviderKind>,
    accepted: Option<&store::LocalReleaseStatus>,
    page_number: u64,
    has_next: bool,
) -> String {
    let notice = accepted.map(render_accepted).unwrap_or_default();
    let filters = render_filters(filter);
    if total == 0 {
        return format!("<h1>Review</h1>{notice}{filters}<p>Nothing is queued for review.</p>");
    }
    let body: String = rows.iter().map(|row| queue_row(row, filter)).collect();
    format!(
        "<h1>Review</h1>{notice}{filters}<p class=\"muted\">{} queued for review.</p>\
         <div class=\"table\"><table><thead><tr><th class=\"num\">ID</th><th>Artist</th>\
         <th>Album</th><th class=\"num\">Tracks</th><th>Best</th><th>Folder</th></tr></thead>\
         <tbody>{body}</tbody></table></div>{}",
        plural(total.max(0) as u64, "release", "releases"),
        pager_with(
            "/reviews",
            filter.map(|provider| ("provider", provider.as_str())),
            page_number,
            has_next
        )
    )
}

/// One queued release as a table row; its ID links to its review page.
fn queue_row(row: &store::QueuedRelease, filter: Option<ProviderKind>) -> String {
    let best: Vec<String> = row
        .best
        .iter()
        .map(|(provider, score)| match score {
            Some(score) => format!("{provider} {score:.3}"),
            None => format!("{provider} —"),
        })
        .collect();
    format!(
        "<tr><td class=\"num\"><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td>\
         <td class=\"num\">{}</td><td>{}</td><td class=\"path\"><a href=\"{}\">{}</a></td></tr>",
        escape_html(&review_href(row.id, filter, None)),
        row.id,
        escape_html(row.album_artist.as_deref().unwrap_or("(unknown artist)")),
        escape_html(row.album_title.as_deref().unwrap_or("(no album tag)")),
        row.track_count,
        best.join(" · "),
        escape_html(&folder_href(&row.unit)),
        escape_html(&row.directory),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(id: i64) -> store::QueuedRelease {
        store::QueuedRelease {
            id,
            directory: "/music/<b>".into(),
            unit: "/music/<b>".into(),
            album_title: Some("<i>Blue</i>".into()),
            album_artist: Some("Mallory".into()),
            track_count: 2,
            best: vec![
                (ProviderKind::MusicBrainz, Some(0.9314)),
                (ProviderKind::Discogs, None),
            ],
        }
    }

    #[test]
    fn queue_rows_escape_values_and_carry_the_filter() {
        let html = render_queue(&[queued(7)], 1, Some(ProviderKind::Deezer), None, 1, true);
        assert!(
            html.contains("&lt;i&gt;Blue&lt;/i&gt;") && !html.contains("<i>"),
            "{html}"
        );
        assert!(
            html.contains("href=\"/review?release=7&amp;provider=deezer\""),
            "{html}"
        );
        assert!(html.contains("musicbrainz 0.931 · discogs —"), "{html}");
        assert!(
            html.contains("href=\"/reviews?provider=deezer&amp;page=2\""),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/reviews?provider=deezer\" aria-current=\"page\">Deezer</a>"),
            "{html}"
        );
        assert!(html.contains("<a href=\"/reviews\">All</a>"), "{html}");
        assert!(html.contains("1 release queued for review."), "{html}");
        assert!(
            html.contains("href=\"/folder?path=%2Fmusic%2F%3Cb%3E\""),
            "{html}"
        );
    }

    #[test]
    fn an_empty_queue_says_so() {
        let html = render_queue(&[], 0, None, None, 1, false);
        assert!(html.contains("Nothing is queued for review."), "{html}");
    }

    #[test]
    fn review_links_keep_the_filter_and_the_accepted_release() {
        assert_eq!(review_href(3, None, None), "/review?release=3");
        assert_eq!(
            review_href(3, Some(ProviderKind::Discogs), Some(2)),
            "/review?release=3&provider=discogs&accepted=2"
        );
        assert_eq!(queue_href(None, None), "/reviews");
        assert_eq!(queue_href(None, Some(2)), "/reviews?accepted=2");
        let params = ReviewParams {
            provider: Some("spotify".into()),
            page: Some("x".into()),
            ..ReviewParams::default()
        };
        assert_eq!((params.filter(), params.page()), (None, 1));
    }
}
