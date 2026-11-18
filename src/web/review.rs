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
    AppError, AppState, Nav, PAGE_SIZE, escape_html, folder_href, is_web_url, page, page_param,
    pager_with, relative_path, render_match,
};
use crate::layout::{format_duration, plural, track_number};
use crate::matcher::{self, Candidate, Evaluation, LocalRelease, RemoteTrack};
use crate::providers::{ProviderKind, ProviderPolicy, ReleaseId};
use crate::review::{self as saved, GroupState, ReleaseReview, ReviewGroup};
use crate::store;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

/// The query of `/reviews` and `/review`.
///
/// Values stay text so a malformed one falls back instead of failing the request.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ReviewParams {
    release: Option<String>,
    provider: Option<String>,
    accepted: Option<String>,
    page: Option<String>,
}

impl ReviewParams {
    /// The release a review page is about.
    fn release(&self) -> Option<i64> {
        self.release.as_deref()?.trim().parse().ok()
    }

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
            " aria-current=\"true\""
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
    format!("<p class=\"filters\">{}</p>", links.join(" "))
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

/// The longest release ID or link the fetch form takes, in characters.
///
/// A provider link with a slug and a query is well under 200 characters;
/// anything longer is not an ID, and the limit keeps the echoed field small.
const MAX_ENTERED_CHARS: usize = 200;

/// Everything one review page shows.
struct View<'a> {
    review: &'a ReleaseReview,
    /// The queue filter carried from `/reviews`; it decides `Next in queue`.
    filter: Option<ProviderKind>,
    next: Option<i64>,
    /// The release accepted just before, now matched.
    accepted: Option<&'a store::LocalReleaseStatus>,
    /// This process's form token, on a server that takes writes.
    token: Option<&'a str>,
}

pub(super) async fn release_page(
    State(state): State<AppState>,
    Query(params): Query<ReviewParams>,
) -> Result<Response, AppError> {
    let Some(id) = params.release() else {
        return Ok(no_release());
    };
    let accepted = accepted_status(&state, params.accepted()).await?;
    render_page(
        &state,
        id,
        params.filter(),
        accepted.as_ref(),
        StatusCode::OK,
    )
    .await
}

/// The `404` for a release that does not exist.
fn no_release() -> Response {
    (StatusCode::NOT_FOUND, "no such release").into_response()
}

/// The review page of release `id`, answered with `status`.
async fn render_page(
    state: &AppState,
    id: i64,
    filter: Option<ProviderKind>,
    accepted: Option<&store::LocalReleaseStatus>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let Some(review) = saved::load(&state.pool, id).await? else {
        return Ok(no_release());
    };
    let next = store::next_queued(&state.pool, id, filter).await?;
    let view = View {
        review: &review,
        filter,
        next,
        accepted,
        token: state.writable.then_some(&*state.token),
    };
    Ok((
        status,
        Html(page("Review", Nav::Review, &render_release(&view))),
    )
        .into_response())
}

fn render_release(view: &View<'_>) -> String {
    let review = view.review;
    let release = &review.release;
    let status = &review.status;
    let next = match view.next {
        Some(next) => format!(
            "<a href=\"{}\">Next in queue →</a>",
            escape_html(&review_href(next, view.filter, None))
        ),
        None => format!(
            "<a href=\"{}\">Back to the queue</a>",
            escape_html(&queue_href(view.filter, None))
        ),
    };
    let accepted = view.accepted.map(render_accepted).unwrap_or_default();
    let problems: String = release
        .embedded_evidence
        .problems
        .iter()
        .map(|problem| format!("<li>identifier problem: {}</li>", escape_html(problem)))
        .collect();
    let problems = if problems.is_empty() {
        String::new()
    } else {
        format!("<ul class=\"muted\">{problems}</ul>")
    };
    let current = if status.provider.is_some() {
        format!(
            "<p>Accepted now: {}. Accepting another candidate replaces it.</p>",
            render_match(status)
        )
    } else {
        String::new()
    };
    let groups: String = if review.groups.is_empty() {
        format!(
            "<p>No saved candidates. Enter a release below, or run \
             <code>radish match --local-release {}</code>.</p>",
            release.id
        )
    } else {
        review
            .groups
            .iter()
            .map(|group| render_group(view, group))
            .collect()
    };
    format!(
        "<p class=\"pager\">{next}</p>{accepted}<h1>#{} {} — {}</h1>\
         <p class=\"path\"><a href=\"{}\">{}</a></p><p>{} · {}</p>{problems}{current}\
         <h2>Files</h2>{}{groups}{}",
        release.id,
        escape_html(
            release
                .album_artist
                .as_deref()
                .unwrap_or("(unknown artist)")
        ),
        escape_html(release.album_title.as_deref().unwrap_or("(no album tag)")),
        escape_html(&folder_href(&status.unit)),
        escape_html(&release.directory),
        plural(release.tracks.len() as u64, "file", "files"),
        escape_html(&status.status_label()),
        render_files(release),
        render_entry(view),
    )
}

/// The release's files: number, title, length and file name.
fn render_files(release: &LocalRelease) -> String {
    let rows: String = release
        .tracks
        .iter()
        .map(|track| {
            format!(
                "<tr><td class=\"num\">{}</td><td>{}</td><td class=\"num\">{}</td>\
                 <td class=\"path\">{}</td></tr>",
                track_number(track.disc, track.track_number),
                escape_html(track.title.as_deref().unwrap_or("")),
                format_duration(track.duration_secs),
                escape_html(relative_path(&release.directory, &track.path)),
            )
        })
        .collect();
    format!(
        "<div class=\"table\"><table><thead><tr><th class=\"num\">#</th><th>Title</th>\
         <th class=\"num\">Time</th><th>File</th></tr></thead><tbody>{rows}</tbody></table></div>"
    )
}

/// One provider's saved candidates, with accept buttons only while current.
fn render_group(view: &View<'_>, group: &ReviewGroup) -> String {
    let reasons = if group.reasons.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"muted\">Queued: {}</p>",
            escape_html(&group.reasons.join("; "))
        )
    };
    let heading = format!("<h2>{}</h2>{reasons}", group.provider.label());
    let cards = |current: bool| -> String {
        group
            .candidates
            .iter()
            .enumerate()
            .map(|(index, (candidate, evaluation))| {
                render_card(view, group.provider, index, candidate, evaluation, current)
            })
            .collect()
    };
    match &group.state {
        GroupState::Current => format!("{heading}{}", cards(true)),
        GroupState::OutOfDate => format!(
            "{heading}<p>The files changed after these candidates were found. Run \
             <code>radish match --local-release {}</code> to search again.</p>{}",
            view.review.release.id,
            cards(false)
        ),
        GroupState::Unreadable(error) => format!(
            "{heading}<p>This saved review cannot be read: {}</p>",
            escape_html(error)
        ),
    }
}

/// One candidate as a card. Only a `current` card has a track table and an
/// accept section: an out-of-date mapping indexes files that may have moved.
fn render_card(
    view: &View<'_>,
    provider: ProviderKind,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
    current: bool,
) -> String {
    let release = &candidate.release;
    let local = &view.review.release;
    let id = escape_html(&release.id.storage_id());
    let id = if is_web_url(&release.url) {
        format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">{id}</a>",
            escape_html(&release.url)
        )
    } else {
        id
    };
    let verdict = if evaluation.auto_accept {
        "auto-acceptable"
    } else {
        "review"
    };
    let details = saved::edition_details(release);
    let details = if details.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"muted\">{}</p>",
            escape_html(&details.join(" · "))
        )
    };
    let mut notes: Vec<String> = Vec::new();
    notes.extend(
        release
            .structure_issues
            .iter()
            .map(|note| format!("structure: {note}")),
    );
    if !release.lookup_aliases.is_empty() {
        let aliases: Vec<String> = release
            .lookup_aliases
            .iter()
            .map(ReleaseId::storage_id)
            .collect();
        notes.push(format!("aliases: {}", aliases.join(", ")));
    }
    notes.extend(
        evaluation
            .evidence
            .iter()
            .map(|note| format!("evidence: {note}")),
    );
    notes.extend(
        evaluation
            .warnings
            .iter()
            .map(|note| format!("warning: {note}")),
    );
    notes.extend(
        candidate
            .mapping
            .notes
            .iter()
            .map(|note| format!("mapping: {note}")),
    );
    let notes: String = notes
        .iter()
        .map(|note| format!("<li>{}</li>", escape_html(note)))
        .collect();
    let disc = candidate
        .mapping
        .medium
        .map(|medium| {
            format!(
                "<p>The files map onto {}.</p>",
                escape_html(&matcher::disc_label(release, medium))
            )
        })
        .unwrap_or_default();
    let (tracks, action) = if current {
        (
            render_mapping(local, candidate),
            render_accept(view, provider, candidate, evaluation),
        )
    } else {
        (String::new(), String::new())
    };
    format!(
        "<section class=\"card\"><h3>[{}] {provider} {id} — score {:.3} \
         <span class=\"muted\">{verdict}</span></h3><p>{} — {}</p>{details}\
         <p class=\"muted\">score: {}</p><p class=\"muted\">mapping: {}/{} files mapped</p>\
         <ul>{notes}</ul>{disc}{tracks}{action}</section>",
        index + 1,
        evaluation.match_scope.score(&candidate.score),
        escape_html(&release.artist_display()),
        escape_html(&release.title),
        escape_html(&saved::score_parts(candidate, evaluation).join("  ")),
        candidate.mapping.assigned_count(),
        local.tracks.len(),
    )
}

/// The files side by side with the provider tracks they map to, then the
/// provider's playable tracks that no file maps to (on the mapped disc only).
fn render_mapping(local: &LocalRelease, candidate: &Candidate) -> String {
    let remote = &candidate.release.tracklist;
    let media = candidate.release.track_media();
    let mut shown = vec![false; remote.len()];
    let mut rows = String::new();
    for (index, track) in local.tracks.iter().enumerate() {
        let paired = candidate
            .mapping
            .pairs
            .get(index)
            .copied()
            .flatten()
            .filter(|&remote_index| remote_index < remote.len());
        if let Some(remote_index) = paired {
            shown[remote_index] = true;
        }
        rows.push_str(&format!(
            "<tr><td class=\"num\">{}</td><td>{}</td><td class=\"num\">{}</td>{}</tr>",
            track_number(track.disc, track.track_number),
            escape_html(track.title.as_deref().unwrap_or("")),
            format_duration(track.duration_secs),
            remote_cells(paired.map(|remote_index| &remote[remote_index])),
        ));
    }
    for (remote_index, track) in remote.iter().enumerate() {
        let on_disc = candidate
            .mapping
            .medium
            .is_none_or(|medium| media.get(remote_index).copied().flatten() == Some(medium));
        if shown[remote_index] || !track.is_playable() || !on_disc {
            continue;
        }
        rows.push_str(&format!(
            "<tr><td></td><td class=\"muted\">not in the files</td><td></td>{}</tr>",
            remote_cells(Some(track))
        ));
    }
    format!(
        "<div class=\"table\"><table><thead><tr><th class=\"num\">#</th><th>File title</th>\
         <th class=\"num\">Time</th><th>Pos</th><th>{} title</th><th class=\"num\">Time</th>\
         </tr></thead><tbody>{rows}</tbody></table></div>",
        candidate.release.provider().label()
    )
}

/// The provider side of a track table row, or an empty one.
fn remote_cells(track: Option<&RemoteTrack>) -> String {
    match track {
        Some(track) => format!(
            "<td>{}</td><td>{}</td><td class=\"num\">{}</td>",
            escape_html(track.position.as_deref().unwrap_or("")),
            escape_html(&track.title),
            format_duration(track.duration_secs),
        ),
        None => "<td>—</td><td></td><td></td>".to_string(),
    }
}

/// The text after `Cannot accept:` for a mapping that cannot be applied.
fn cannot_apply_text(notes: &[String]) -> String {
    if notes.is_empty() {
        "the track mapping is invalid, incomplete, or structurally unsupported".to_string()
    } else {
        notes.join("; ")
    }
}

/// The blockers, then the accept button, the command that does the same,
/// or why the candidate cannot be accepted.
fn render_accept(
    view: &View<'_>,
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
) -> String {
    let local = &view.review.release;
    let blockers: String = evaluation
        .blockers
        .iter()
        .map(|blocker| format!("<li>{}</li>", escape_html(blocker)))
        .collect();
    if !evaluation.can_apply(local, candidate) {
        let blockers = if blockers.is_empty() {
            String::new()
        } else {
            format!("<ul class=\"blocker\">{blockers}</ul>")
        };
        return format!(
            "{blockers}<p class=\"blocker\">Cannot accept: {}</p>",
            escape_html(&cannot_apply_text(&candidate.mapping.notes))
        );
    }
    let blockers = if blockers.is_empty() {
        String::new()
    } else {
        format!("<p>Accepting overrides these checks:</p><ul class=\"blocker\">{blockers}</ul>")
    };
    let Some(token) = view.token else {
        return format!(
            "{blockers}<p class=\"muted\">This server listens on a non-loopback address, so it \
             changes nothing. Run <code>radish match --review --local-release {}</code> with the \
             same <code>--db</code> as this server.</p>",
            local.id
        );
    };
    let label = match evaluation.blockers.len() {
        0 => "Accept".to_string(),
        count => format!(
            "Accept, overriding {}",
            plural(count as u64, "check", "checks")
        ),
    };
    format!(
        "{blockers}<form method=\"post\" action=\"/review\">{}\
         <input type=\"hidden\" name=\"action\" value=\"accept\">\
         <input type=\"hidden\" name=\"provider\" value=\"{provider}\">\
         <input type=\"hidden\" name=\"candidate\" value=\"{}\"><button>{label}</button></form>",
        hidden_fields(view, token),
        escape_html(&candidate.release.id.storage_id()),
    )
}

/// The fields every review form carries: the token, the release, and the
/// guard the page was built from (source revision and accepted match id,
/// empty for none), so a decision on an outdated page is refused.
fn hidden_fields(view: &View<'_>, token: &str) -> String {
    let guard = &view.review.guard;
    format!(
        "<input type=\"hidden\" name=\"token\" value=\"{}\">\
         <input type=\"hidden\" name=\"release\" value=\"{}\">\
         <input type=\"hidden\" name=\"revision\" value=\"{}\">\
         <input type=\"hidden\" name=\"match\" value=\"{}\">\
         <input type=\"hidden\" name=\"filter\" value=\"{}\">",
        escape_html(token),
        view.review.release.id,
        escape_html(&guard.source_revision),
        guard
            .accepted_match_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        view.filter.map_or("", ProviderKind::as_str),
    )
}

/// The form that fetches a release by its ID or link, or the commands that do.
fn render_entry(view: &View<'_>) -> String {
    let id = view.review.release.id;
    let Some(token) = view.token else {
        return format!(
            "<h2>Enter a release</h2><p class=\"muted\">This server listens on a non-loopback \
             address, so it changes nothing. Run one of these with the same <code>--db</code> as \
             this server:</p><ul>\
             <li><code>radish match --review --local-release {id} --musicbrainz-release MBID</code></li>\
             <li><code>radish match --review --local-release {id} --discogs-release ID</code></li>\
             <li><code>radish match --review --local-release {id} --deezer-release ID</code></li></ul>"
        );
    };
    let options: String = ProviderPolicy::Chain { deezer: true }
        .order()
        .into_iter()
        .map(|provider| format!("<option value=\"{provider}\">{}</option>", provider.label()))
        .collect();
    format!(
        "<h2>Enter a release</h2><form method=\"post\" action=\"/review\">{}\
         <input type=\"hidden\" name=\"action\" value=\"fetch\">\
         <select name=\"provider\" aria-label=\"Provider\">{options}</select>\
         <input type=\"text\" name=\"id\" value=\"\" maxlength=\"{MAX_ENTERED_CHARS}\" \
         placeholder=\"ID or link\" aria-label=\"Release ID or link\"><button>Fetch</button></form>",
        hidden_fields(view, token),
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
            html.contains("<a href=\"/reviews?provider=deezer\" aria-current=\"true\">Deezer</a>"),
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
    fn the_notice_shows_the_accepted_match_with_the_album_escaped() {
        let status = store::LocalReleaseStatus {
            id: 4,
            directory: "/music/a".into(),
            unit: "/music/a".into(),
            album_title: Some("<b>Blue</b>".into()),
            album_artist: Some("Artist".into()),
            track_count: 2,
            status: store::STATUS_MATCHED.into(),
            provider: Some(ProviderKind::MusicBrainz),
            remote_release_id: Some("abc".into()),
            remote_url: None,
            remote_title: None,
            match_scope: None,
            medium: None,
            stale: false,
            queued_providers: Vec::new(),
            merged: None,
        };
        let html = render_queue(&[], 0, None, Some(&status), 1, false);
        assert!(html.contains("Accepted musicbrainz abc"), "{html}");
        assert!(html.contains("&lt;b&gt;Blue&lt;/b&gt;"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
    }

    #[tokio::test]
    async fn only_a_matched_release_is_reported_as_accepted() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let release = crate::test_support::seed_release(&pool, "/music/a", "Blue", &["One"]).await;
        let state = AppState {
            pool,
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: "t".into(),
            writable: true,
        };
        assert!(accepted_status(&state, None).await.unwrap().is_none());
        assert!(
            accepted_status(&state, Some(release.id))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            accepted_status(&state, Some(release.id + 100))
                .await
                .unwrap()
                .is_none()
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

    use crate::matcher::{Candidate, Evaluation, MatchScope, RemoteRelease};
    use crate::review::{GroupState, ReleaseReview, ReviewGroup};
    use crate::test_support::{
        FIXTURE_MBID, OTHER_MBID, local_release, mbid_release, remote_release,
    };

    const FILES: [&str; 2] = ["<script>x</script>", "Two"];

    fn group(state: GroupState, remotes: Vec<(RemoteRelease, Vec<String>)>) -> ReviewGroup {
        let local = local_release(12, "/music/<b>", "<i>Blue</i>", &FILES);
        ReviewGroup {
            provider: ProviderKind::MusicBrainz,
            state,
            reasons: vec!["<b>close</b> alternative".into()],
            candidates: remotes
                .into_iter()
                .map(|(remote, blockers)| {
                    let candidate = Candidate::new(&local, remote);
                    let evaluation = Evaluation {
                        auto_accept: false,
                        match_scope: MatchScope::Edition,
                        blockers,
                        warnings: Vec::new(),
                        evidence: vec!["<u>barcode</u>".into()],
                    };
                    (candidate, evaluation)
                })
                .collect(),
        }
    }

    fn review(groups: Vec<ReviewGroup>) -> ReleaseReview {
        let release = local_release(12, "/music/<b>", "<i>Blue</i>", &FILES);
        ReleaseReview {
            status: store::LocalReleaseStatus {
                id: 12,
                directory: "/music/<b>".into(),
                unit: "/music/<b>".into(),
                album_title: Some("<i>Blue</i>".into()),
                album_artist: Some("Artist".into()),
                track_count: 2,
                status: store::STATUS_UNMATCHED.into(),
                provider: None,
                remote_release_id: None,
                remote_url: None,
                remote_title: None,
                match_scope: None,
                medium: None,
                stale: false,
                queued_providers: vec![ProviderKind::MusicBrainz],
                merged: None,
            },
            guard: store::MatchGuard {
                source_revision: release.revision.clone(),
                accepted_match_id: None,
            },
            release,
            groups,
        }
    }

    fn view<'a>(review: &'a ReleaseReview, token: Option<&'a str>) -> View<'a> {
        View {
            review,
            filter: None,
            next: None,
            accepted: None,
            token,
        }
    }

    fn exact() -> RemoteRelease {
        remote_release(mbid_release(FIXTURE_MBID), "<i>Blue</i>", &FILES)
    }

    fn longer() -> RemoteRelease {
        remote_release(
            mbid_release(OTHER_MBID),
            "Blue",
            &["<script>x</script>", "Two", "Bonus"],
        )
    }

    #[test]
    fn a_candidate_card_escapes_everything_and_maps_track_by_track() {
        let review = review(vec![group(
            GroupState::Current,
            vec![
                (exact(), vec!["<b>close</b>".into()]),
                (longer(), Vec::new()),
            ],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        for raw in ["<script", "<i>", "<b>", "<u>"] {
            assert!(!html.contains(raw), "{raw} in {html}");
        }
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
        assert!(html.contains("Accepting overrides these checks:"), "{html}");
        assert!(
            html.contains("<button>Accept, overriding 1 check</button>"),
            "{html}"
        );
        assert!(
            html.contains(&format!("name=\"candidate\" value=\"{FIXTURE_MBID}\"")),
            "{html}"
        );
        assert!(
            !html.contains(&format!("name=\"candidate\" value=\"{OTHER_MBID}\"")),
            "{html}"
        );
        assert!(html.contains("Cannot accept:"), "{html}");
        assert!(html.contains("not in the files"), "{html}");
        assert!(html.contains("name=\"revision\" value=\"rev\""), "{html}");
        assert!(html.contains("name=\"match\" value=\"\""), "{html}");
        assert!(html.contains("name=\"token\" value=\"tok\""), "{html}");
        assert!(
            html.contains("Queued: &lt;b&gt;close&lt;/b&gt; alternative"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/reviews\">Back to the queue</a>"),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"musicbrainz\">MusicBrainz</option>"),
            "{html}"
        );
    }

    #[test]
    fn a_server_without_writes_shows_commands_instead_of_forms() {
        let review = review(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        let html = render_release(&view(&review, None));
        assert!(!html.contains("<form"), "{html}");
        assert!(
            html.contains("radish match --review --local-release 12"),
            "{html}"
        );
        assert!(html.contains("--musicbrainz-release MBID"), "{html}");
    }

    #[test]
    fn out_of_date_and_unreadable_groups_offer_no_accept() {
        let review = review(vec![
            group(GroupState::Unreadable("bad <json>".into()), Vec::new()),
            group(GroupState::OutOfDate, vec![(exact(), Vec::new())]),
        ]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(!html.contains("value=\"accept\""), "{html}");
        assert!(
            html.contains("This saved review cannot be read: bad &lt;json&gt;"),
            "{html}"
        );
        assert!(
            html.contains("The files changed after these candidates were found."),
            "{html}"
        );
        // The fetch form still works on such a page.
        assert!(html.contains("value=\"fetch\""), "{html}");
    }

    #[test]
    fn a_release_without_saved_candidates_says_how_to_get_some() {
        let review = review(Vec::new());
        let mut shown = view(&review, Some("tok"));
        shown.next = Some(9);
        shown.filter = Some(ProviderKind::MusicBrainz);
        let html = render_release(&shown);
        assert!(html.contains("No saved candidates."), "{html}");
        assert!(
            html.contains("href=\"/review?release=9&amp;provider=musicbrainz\""),
            "{html}"
        );
        assert!(
            html.contains("name=\"filter\" value=\"musicbrainz\""),
            "{html}"
        );
    }
}
