//! The review pages: the queue of releases waiting for a decision, one
//! release's saved candidates, and the form that accepts one or fetches
//! another by its ID.
//!
//! Accepting and fetching are the server's second write, guarded like the
//! folder form: only a loopback server takes them, with this process's form
//! token, from this site. Every form also carries the release's source
//! revision, so a decision made on an outdated page is refused (`409`).

// Rust guideline compliant 2026-08-14

mod card;

use self::card::{cannot_apply_text, render_card};
use super::{
    AppError, AppState, NOT_WRITABLE, Nav, PAGE_SIZE, cross_site, error_page, escape_html,
    folder_href, page, page_param, pager_with, relative_path, render_match, same_token,
};
use crate::layout::{format_duration, plural, track_number};
use crate::matcher::LocalRelease;
use crate::providers::{ProviderKind, ReleaseId};
use crate::review::{
    self as saved, AcceptOutcome, AddOutcome, GroupState, ReleaseReview, ReviewGroup,
};
use crate::store;
use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

/// The query of `/reviews` and `/review`.
///
/// Values stay text so a malformed one falls back instead of failing the request.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ReviewParams {
    release: Option<String>,
    provider: Option<String>,
    sort: Option<String>,
    show: Option<String>,
    accepted: Option<String>,
    page: Option<String>,
    /// The lead score the release had in the queue: see [`parse_place`].
    at: Option<String>,
}

impl ReviewParams {
    /// The release a review page is about.
    fn release(&self) -> Option<i64> {
        self.release.as_deref()?.trim().parse().ok()
    }

    /// The queue view; anything unknown falls back to the default.
    fn view(&self) -> store::QueueView {
        view_of(
            self.provider.as_deref(),
            self.sort.as_deref(),
            self.show.as_deref(),
        )
    }

    /// The release accepted just before this page was loaded.
    fn accepted(&self) -> Option<i64> {
        self.accepted.as_deref()?.trim().parse().ok()
    }

    /// The 1-based page, defaulting to 1 for anything unusable.
    fn page(&self) -> u64 {
        page_param(self.page.as_deref())
    }

    /// The place the release was found at, when the query names one.
    fn at(&self) -> Option<store::QueuePlace> {
        parse_place(self.at.as_deref()?)
    }
}

/// The `at` value that names `place`: the lead score as Rust prints it, which
/// reads back as the same number, or `none` for the block with no score.
fn place_text(place: store::QueuePlace) -> String {
    place
        .score
        .map_or_else(|| "none".to_string(), |score| score.to_string())
}

/// The `at` value to write for `at` under `view`: only the score order has
/// places, so any other order writes none. The one rule for links and forms.
fn written_place(view: &store::QueueView, at: Option<store::QueuePlace>) -> Option<String> {
    at.filter(|_| view.sort == store::QueueSort::Score)
        .map(place_text)
}

/// The place `text` names; anything that is not `none` or a finite number is
/// no place.
fn parse_place(text: &str) -> Option<store::QueuePlace> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("none") {
        return Some(store::QueuePlace { score: None });
    }
    let score: f64 = text.parse().ok()?;
    score
        .is_finite()
        .then_some(store::QueuePlace { score: Some(score) })
}

/// The queue view named by `provider`, `sort` and `show`; unknown values fall back to the default.
fn view_of(provider: Option<&str>, sort: Option<&str>, show: Option<&str>) -> store::QueueView {
    store::QueueView {
        provider: provider.and_then(ProviderKind::parse),
        sort: match sort.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("score") => store::QueueSort::Score,
            _ => store::QueueSort::Folder,
        },
        acceptable: show.is_some_and(|value| value.trim().eq_ignore_ascii_case("acceptable")),
    }
}

/// The `sort` value that names `sort`; none for the default order.
fn sort_param(sort: store::QueueSort) -> Option<&'static str> {
    match sort {
        store::QueueSort::Folder => None,
        store::QueueSort::Score => Some("score"),
    }
}

/// The `show` value for `acceptable`; none for showing every queued release.
fn show_param(acceptable: bool) -> Option<&'static str> {
    acceptable.then_some("acceptable")
}

/// The query parameters that name `view`, defaults left out.
///
/// The one encoding of a view: links, the pager and the forms' hidden fields
/// all take their values from here and from [`sort_param`] / [`show_param`].
fn view_params(view: &store::QueueView) -> Vec<(&'static str, &'static str)> {
    let mut params = Vec::new();
    if let Some(provider) = view.provider {
        params.push(("provider", provider.as_str()));
    }
    if let Some(sort) = sort_param(view.sort) {
        params.push(("sort", sort));
    }
    if let Some(show) = show_param(view.acceptable) {
        params.push(("show", show));
    }
    params
}

/// `path`, then the release (when given), the view's parameters, the place
/// the release was found at and the release just accepted, as a query string.
///
/// The place is written only in score order, where it means something. Every
/// value is a static string, an integer, a number or `none`, so none needs
/// encoding.
fn href_with(
    path: &str,
    release: Option<i64>,
    view: &store::QueueView,
    at: Option<store::QueuePlace>,
    accepted: Option<i64>,
) -> String {
    let mut parts: Vec<String> = release
        .into_iter()
        .map(|release| format!("release={release}"))
        .collect();
    parts.extend(
        view_params(view)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    if let Some(place) = written_place(view, at) {
        parts.push(format!("at={place}"));
    }
    if let Some(id) = accepted {
        parts.push(format!("accepted={id}"));
    }
    if parts.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", parts.join("&"))
    }
}

/// `/review?release=N`, with the queue view, the place the release was found
/// at (score order only) and the release just accepted.
fn review_href(
    release: i64,
    view: &store::QueueView,
    at: Option<store::QueuePlace>,
    accepted: Option<i64>,
) -> String {
    href_with("/review", Some(release), view, at, accepted)
}

/// `/reviews`, with the queue view and the release just accepted.
fn queue_href(view: &store::QueueView, accepted: Option<i64>) -> String {
    href_with("/reviews", None, view, None, accepted)
}

/// `/reviews`: the queue of releases waiting for a decision.
pub(super) async fn queue_page(
    State(state): State<AppState>,
    Query(params): Query<ReviewParams>,
) -> Result<Html<String>, AppError> {
    let view = params.view();
    let page_number = params.page();
    let offset = (page_number - 1) * PAGE_SIZE as u64;
    // One row past the page tells us whether a next page exists.
    let mut rows =
        store::queue_page(&state.pool, &view, PAGE_SIZE as i64 + 1, offset as i64).await?;
    let has_next = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let total = store::queue_count(&state.pool, &view).await?;
    let accepted = accepted_status(&state, params.accepted()).await?;
    let body = render_queue(
        &rows,
        total,
        &view,
        accepted.as_ref(),
        page_number,
        has_next,
    );
    Ok(Html(page("Review", Some(Nav::Review), &body)))
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
fn render_accepted(status: &store::LocalReleaseStatus, view: &store::QueueView) -> String {
    format!(
        "<p class=\"notice\">Accepted {} for <a href=\"{}\">#{} {} — {}</a>.</p>",
        render_match(status),
        escape_html(&review_href(status.id, view, None, None)),
        status.id,
        escape_html(status.album_artist.as_deref().unwrap_or("(unknown artist)")),
        escape_html(status.album_title.as_deref().unwrap_or("(no album tag)")),
    )
}

/// The Sort, Show and Provider link rows; each link changes one choice of `view`.
fn render_controls(view: &store::QueueView) -> String {
    let row = |label: &str, links: Vec<(String, bool, &str)>| {
        let links: Vec<String> = links
            .into_iter()
            .map(|(href, current, text)| {
                let current = if current {
                    " aria-current=\"true\""
                } else {
                    ""
                };
                format!("<a href=\"{}\"{current}>{text}</a>", escape_html(&href))
            })
            .collect();
        format!(
            "<p class=\"filters\" role=\"group\" aria-label=\"{label}\">\
             <span class=\"muted\">{label}:</span> {}</p>",
            links.join(" · ")
        )
    };
    let with_sort = |sort| queue_href(&store::QueueView { sort, ..*view }, None);
    let with_show = |acceptable| {
        queue_href(
            &store::QueueView {
                acceptable,
                ..*view
            },
            None,
        )
    };
    let with_provider = |provider| queue_href(&store::QueueView { provider, ..*view }, None);
    let sort = row(
        "Sort",
        vec![
            (
                with_sort(store::QueueSort::Folder),
                view.sort == store::QueueSort::Folder,
                "Folder",
            ),
            (
                with_sort(store::QueueSort::Score),
                view.sort == store::QueueSort::Score,
                "Score",
            ),
        ],
    );
    let show = row(
        "Show",
        vec![
            (with_show(false), !view.acceptable, "All"),
            (with_show(true), view.acceptable, "Can be accepted"),
        ],
    );
    let mut providers = vec![(with_provider(None), view.provider.is_none(), "All")];
    providers.extend(ProviderKind::CHAIN.into_iter().map(|provider| {
        (
            with_provider(Some(provider)),
            view.provider == Some(provider),
            provider.label(),
        )
    }));
    format!("{sort}{show}{}", row("Provider", providers))
}

/// The queue page body: notice, controls, the table and the pager.
fn render_queue(
    rows: &[store::QueuedRelease],
    total: i64,
    view: &store::QueueView,
    accepted: Option<&store::LocalReleaseStatus>,
    page_number: u64,
    has_next: bool,
) -> String {
    let notice = accepted
        .map(|status| render_accepted(status, view))
        .unwrap_or_default();
    let controls = render_controls(view);
    if total == 0 {
        let empty = if view.acceptable {
            "No queued release can be accepted."
        } else {
            "Nothing is queued for review."
        };
        return format!("<h1>Review</h1>{notice}{controls}<p>{empty}</p>");
    }
    let body: String = rows.iter().map(|row| queue_row(row, view)).collect();
    let what = if view.acceptable {
        "can be accepted"
    } else {
        "queued for review"
    };
    format!(
        "<h1>Review</h1>{notice}{controls}<p class=\"muted\">{} {what}.</p>\
         <div class=\"table\"><table><thead><tr><th class=\"num\">ID</th><th>Artist</th>\
         <th>Album</th><th class=\"num\">Tracks</th><th>Top</th><th>Reason</th><th>Folder</th>\
         </tr></thead><tbody>{body}</tbody></table></div>{}",
        plural(total.max(0) as u64, "release", "releases"),
        pager_with("/reviews", &view_params(view), page_number, has_next)
    )
}

/// One queued release as a table row; its ID links to its review page.
fn queue_row(row: &store::QueuedRelease, view: &store::QueueView) -> String {
    let top: Vec<String> = row
        .top
        .iter()
        .map(|(provider, score)| match score {
            Some(score) => format!("{provider} {score:.3}"),
            None => format!("{provider} —"),
        })
        .collect();
    let reason = match &row.lead {
        Some(lead) => {
            let more = if lead.blockers > 1 {
                format!(" (+{} more)", lead.blockers - 1)
            } else {
                String::new()
            };
            let mark = if lead.can_apply {
                ""
            } else {
                " <span class=\"blocker\">top candidate can't be accepted</span>"
            };
            format!(
                "{}: {}{more}{mark}",
                lead.provider,
                escape_html(lead.reason.as_deref().unwrap_or(""))
            )
        }
        None => String::new(),
    };
    // Where the release stands in this view, which its page counts "next" from.
    let at = row
        .lead
        .as_ref()
        .map(|lead| store::QueuePlace { score: lead.score });
    format!(
        "<tr><td class=\"num\"><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td>\
         <td class=\"num\">{}</td><td>{}</td><td>{reason}</td>\
         <td class=\"path\"><a href=\"{}\">{}</a></td></tr>",
        escape_html(&review_href(row.id, view, at, None)),
        row.id,
        escape_html(row.album_artist.as_deref().unwrap_or("(unknown artist)")),
        escape_html(row.album_title.as_deref().unwrap_or("(no album tag)")),
        row.track_count,
        top.join(" · "),
        escape_html(&folder_href(&row.unit)),
        escape_html(&row.directory),
    )
}

/// The longest release ID or link the fetch form takes, in characters.
///
/// A provider link with a slug and a query is well under 200 characters;
/// anything longer is not an ID, and the limit keeps the echoed field small.
const MAX_ENTERED_CHARS: usize = 200;

/// A message shown next to the form that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    /// Set for the fetch form, which then shows the message and keeps the
    /// provider and the text entered; the message goes above the candidates
    /// otherwise.
    entered: Option<(ProviderKind, String)>,
    text: String,
}

/// Everything one review page shows.
struct View<'a> {
    review: &'a ReleaseReview,
    /// The queue view carried from `/reviews`; it decides `Next in queue`.
    view: store::QueueView,
    /// The release `Next in queue` leads to, if the queue has another.
    next: Option<store::QueueNext>,
    /// The place the release was found at in the score order; the forms send
    /// it back, so a fetch does not change what comes next.
    at: Option<store::QueuePlace>,
    /// The release accepted just before, now matched.
    accepted: Option<&'a store::LocalReleaseStatus>,
    /// This process's form token, on a server that takes writes.
    token: Option<&'a str>,
    /// A message from the form just sent.
    notice: Option<&'a Notice>,
}

/// `/review?release=N`: one release's saved candidates, or `404`.
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
        params.view(),
        params.at(),
        accepted.as_ref(),
        None,
        StatusCode::OK,
    )
    .await
}

/// The `404` for a release that does not exist.
fn no_release() -> Response {
    queue_error(StatusCode::NOT_FOUND, "No such release.")
}

/// An error page that links back to the review queue.
fn queue_error(status: StatusCode, message: &str) -> Response {
    error_page(
        status,
        Some(Nav::Review),
        message,
        ("/reviews", "Back to the queue"),
    )
}

/// The review page of release `id`, answered with `status`.
async fn render_page(
    state: &AppState,
    id: i64,
    view: store::QueueView,
    at: Option<store::QueuePlace>,
    accepted: Option<&store::LocalReleaseStatus>,
    notice: Option<Notice>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let Some(review) = saved::load(&state.pool, id).await? else {
        return Ok(no_release());
    };
    let next = store::next_queued(&state.pool, id, &view, at).await?;
    let view = View {
        review: &review,
        view,
        next,
        at,
        accepted,
        token: state.writable.then_some(&*state.token),
        notice: notice.as_ref(),
    };
    Ok((
        status,
        Html(page(
            &format!("Review #{id}"),
            Some(Nav::Review),
            &render_release(&view),
        )),
    )
        .into_response())
}

/// The release page body: header, files, one section per provider, fetch form.
fn render_release(view: &View<'_>) -> String {
    let review = view.review;
    let release = &review.release;
    let status = &review.status;
    let next = match view.next {
        Some(next) => format!(
            "<a href=\"{}\">Next in queue →</a>",
            escape_html(&review_href(next.id, &view.view, next.place, None))
        ),
        None => format!(
            "<a href=\"{}\">Back to the queue</a>",
            escape_html(&queue_href(&view.view, None))
        ),
    };
    let jump = if view.token.is_some() {
        " · <a href=\"#enter\">Enter a release by ID</a>"
    } else {
        ""
    };
    let accepted = view
        .accepted
        .map(|status| render_accepted(status, &view.view))
        .unwrap_or_default();
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
    let notice = match view.notice {
        Some(notice) if notice.entered.is_none() => {
            format!("<p class=\"notice\">{}</p>", escape_html(&notice.text))
        }
        _ => String::new(),
    };
    let groups: String = if review.groups.is_empty() {
        format!(
            "<p>No saved candidates. Enter a release below, or run \
             <code>{}</code>.</p>",
            search_command(view)
        )
    } else {
        review
            .groups
            .iter()
            .map(|group| render_group(view, group))
            .collect()
    };
    format!(
        "<p class=\"pager\">{next}{jump}</p>{accepted}<h1>#{} {} — {}</h1>\
         <p class=\"path\"><a href=\"{}\">{}</a></p><p>{} · {}</p>{problems}{current}{notice}\
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

/// The command that searches the release again.
///
/// A matched release is skipped by a plain `match`, so it needs `--review`.
fn search_command(view: &View<'_>) -> String {
    let id = view.review.release.id;
    if view.review.status.status == store::STATUS_MATCHED {
        format!("radish match --review --local-release {id}")
    } else {
        format!("radish match --local-release {id}")
    }
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
    let heading = format!(
        "<h2 id=\"group-{}\">{}</h2>{reasons}",
        group.provider,
        group.provider.label()
    );
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
             <code>{}</code> to search again.</p>{}",
            search_command(view),
            cards(false)
        ),
        GroupState::Unreadable(error) => format!(
            "{heading}<p>This saved review cannot be read: {}</p>",
            escape_html(error)
        ),
    }
}

/// The fields every review form carries: the token, the release, and the
/// guard the page was built from (source revision and accepted match id,
/// empty for none), so a decision on an outdated page is refused.
fn hidden_fields(view: &View<'_>, token: &str) -> String {
    let guard = &view.review.guard;
    let at = written_place(&view.view, view.at)
        .map(|place| {
            format!(
                "<input type=\"hidden\" name=\"at\" value=\"{}\">",
                escape_html(&place)
            )
        })
        .unwrap_or_default();
    format!(
        "<input type=\"hidden\" name=\"token\" value=\"{}\">\
         <input type=\"hidden\" name=\"release\" value=\"{}\">\
         <input type=\"hidden\" name=\"revision\" value=\"{}\">\
         <input type=\"hidden\" name=\"match\" value=\"{}\">\
         <input type=\"hidden\" name=\"filter\" value=\"{}\">\
         <input type=\"hidden\" name=\"sort\" value=\"{}\">\
         <input type=\"hidden\" name=\"show\" value=\"{}\">{at}",
        escape_html(token),
        view.review.release.id,
        escape_html(&guard.source_revision),
        guard
            .accepted_match_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        view.view.provider.map_or("", ProviderKind::as_str),
        sort_param(view.view.sort).unwrap_or(""),
        show_param(view.view.acceptable).unwrap_or(""),
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
    // After a failed fetch the form keeps what was entered, and the provider
    // chosen then wins over the queue's provider.
    let (chosen, text) = match view.notice.and_then(|notice| notice.entered.as_ref()) {
        Some((provider, text)) => (Some(*provider), text.as_str()),
        None => (view.view.provider, ""),
    };
    let notice = match view.notice {
        Some(notice) if notice.entered.is_some() => {
            format!("<p class=\"notice\">{}</p>", escape_html(&notice.text))
        }
        _ => String::new(),
    };
    let options: String = ProviderKind::CHAIN
        .into_iter()
        .map(|provider| {
            let selected = if chosen == Some(provider) {
                " selected"
            } else {
                ""
            };
            format!(
                "<option value=\"{provider}\"{selected}>{}</option>",
                provider.label()
            )
        })
        .collect();
    format!(
        "<h2 id=\"enter\">Enter a release</h2>{notice}<form method=\"post\" action=\"/review#enter\">{}\
         <input type=\"hidden\" name=\"action\" value=\"fetch\">\
         <select name=\"provider\" aria-label=\"Provider\">{options}</select>\
         <input type=\"text\" name=\"id\" value=\"{}\" maxlength=\"{MAX_ENTERED_CHARS}\" required \
         placeholder=\"ID or link\" aria-label=\"Release ID or link\"><button>Fetch</button></form>",
        hidden_fields(view, token),
        escape_html(text),
    )
}

/// What a page built from an outdated release says.
const CHANGED: &str = "This release changed after the page was loaded; review it again.";

/// How long a fetch from the review page may take before the page gives up.
///
/// MusicBrainz's own per-request timeout is also thirty seconds, so one
/// stalled request uses the whole budget. The adapters pace MusicBrainz and
/// retry transient failures, honoring a `Retry-After` of up to two minutes;
/// the limit exists so the browser request does not hang for minutes. A fetch
/// cut off here answers `504` and changes nothing: the release was not stored.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The review form's fields; which ones matter depends on `action`.
///
/// A missing field is empty, so a body without the token reaches the token
/// check and gets its `403` rather than a rejection of the body.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ReviewForm {
    token: String,
    action: String,
    release: String,
    revision: String,
    /// The accepted match id the page showed; empty for none.
    #[serde(rename = "match")]
    accepted_match: String,
    /// The queue view the page was reached from (`filter`, `sort`, `show`);
    /// `provider` is the candidate's provider, not the queue's.
    filter: String,
    /// See `filter`: `score` or empty.
    sort: String,
    /// See `filter`: `acceptable` or empty.
    show: String,
    /// The place the release was found at, as `at` in the query: a score,
    /// `none` or empty.
    at: String,
    /// The provider of the candidate accepted or the release fetched.
    provider: String,
    candidate: String,
    id: String,
}

/// The `403` for a review form from another site or with the wrong token.
///
/// A tab left open across a server restart holds the old process's token, so
/// the body says what to do.
fn stale_review() -> Response {
    queue_error(
        StatusCode::FORBIDDEN,
        "Reload the review page and try again.",
    )
}

/// Accepts a saved candidate, or fetches a release by its ID into the review.
///
/// Refused like the folder form: `403` off loopback, from another site, or
/// without this process's token. Then `400` for an unknown action or provider
/// and `404` for an unknown release. See [`accept`] and [`fetch`] for the rest.
pub(super) async fn action(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<ReviewForm>, FormRejection>,
) -> Result<Response, AppError> {
    if !state.writable {
        return Ok(queue_error(StatusCode::FORBIDDEN, NOT_WRITABLE));
    }
    if cross_site(&headers) {
        return Ok(stale_review());
    }
    let Form(form) = match form {
        Ok(form) => form,
        Err(rejection) => {
            return Ok(queue_error(rejection.status(), &rejection.body_text()));
        }
    };
    if !same_token(&state.token, &form.token) {
        return Ok(stale_review());
    }
    let accepting = match form.action.as_str() {
        "accept" => true,
        "fetch" => false,
        _ => return Ok(queue_error(StatusCode::BAD_REQUEST, "Unknown action.")),
    };
    let Ok(release) = form.release.trim().parse::<i64>() else {
        return Ok(no_release());
    };
    let Some(provider) = ProviderKind::parse(&form.provider) else {
        return Ok(queue_error(StatusCode::BAD_REQUEST, "Unknown provider."));
    };
    let view = view_of(Some(&form.filter), Some(&form.sort), Some(&form.show));
    let at = parse_place(&form.at);
    let revision = form.revision.trim();
    if !accepting {
        return fetch(
            &state,
            release,
            revision,
            view,
            at,
            provider,
            form.id.trim(),
        )
        .await;
    }
    // The page writes these fields itself, so text that does not read is not
    // from a review page.
    let accepted_match_id = match form.accepted_match.trim() {
        "" => None,
        id => match id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => return Ok(queue_error(StatusCode::BAD_REQUEST, "Unknown match.")),
        },
    };
    let Ok(candidate) = saved::parse_bare(provider, form.candidate.trim()) else {
        return Ok(queue_error(StatusCode::BAD_REQUEST, "Unknown candidate."));
    };
    let shown = store::MatchGuard {
        source_revision: revision.to_string(),
        accepted_match_id,
    };
    accept(&state, release, &shown, view, at, &candidate).await
}

/// Accepts saved release `candidate` and goes to the next queued release.
///
/// `shown` is the guard the page was built from. `409` with the page again
/// when the release or its accepted match changed since, or the candidate is
/// no longer saved; `422` when its mapping cannot be applied.
async fn accept(
    state: &AppState,
    release: i64,
    shown: &store::MatchGuard,
    view: store::QueueView,
    at: Option<store::QueuePlace>,
    candidate: &ReleaseId,
) -> Result<Response, AppError> {
    // Found before accepting: accepting deletes the release's reviews, and in
    // score order the release could then not be placed, so the queue would
    // start again from the top and bring back releases already skipped.
    let next = store::next_queued(&state.pool, release, &view, at).await?;
    let (status, text) = match saved::accept_saved(&state.pool, release, candidate, shown).await? {
        AcceptOutcome::Accepted => {
            let target = match next {
                Some(next) => review_href(next.id, &view, next.place, Some(release)),
                None => queue_href(&view, Some(release)),
            };
            return Ok(Redirect::to(&target).into_response());
        }
        AcceptOutcome::Changed => (StatusCode::CONFLICT, CHANGED.to_string()),
        AcceptOutcome::NotFound => (
            StatusCode::CONFLICT,
            "That candidate is no longer saved for this release.".to_string(),
        ),
        AcceptOutcome::CannotApply(notes) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Cannot accept: {}", cannot_apply_text(&notes)),
        ),
    };
    let notice = Notice {
        entered: None,
        text,
    };
    render_page(state, release, view, at, None, Some(notice), status).await
}

/// Fetches the release `entered` names and puts it first in its provider's review.
///
/// The page is checked before any provider is asked: `404` for a release that
/// is gone, `409` for one that changed. Then `400` for text that names no
/// release of `provider`, `502` when the fetch fails (the provider's message,
/// or a missing token or index) and `504` when it takes too long. Text longer
/// than [`MAX_ENTERED_CHARS`] is not echoed in full. A success goes back to the
/// page, at the provider's group of candidates.
async fn fetch(
    state: &AppState,
    release: i64,
    revision: &str,
    view: store::QueueView,
    at: Option<store::QueuePlace>,
    provider: ProviderKind,
    entered: &str,
) -> Result<Response, AppError> {
    let again = |status: StatusCode, text: String| {
        let notice = Notice {
            entered: Some((provider, entered.chars().take(MAX_ENTERED_CHARS).collect())),
            text,
        };
        render_page(state, release, view, at, None, Some(notice), status)
    };
    match saved::current_guard(&state.pool, release).await? {
        Some(guard) if guard.source_revision == revision => {}
        Some(_) => return again(StatusCode::CONFLICT, CHANGED.to_string()).await,
        None => return Ok(no_release()),
    }
    if entered.chars().count() > MAX_ENTERED_CHARS {
        let text = format!("A release ID or link has at most {MAX_ENTERED_CHARS} characters.");
        return again(StatusCode::BAD_REQUEST, text).await;
    }
    let id = match saved::parse_entered(provider, entered) {
        Ok(id) => id,
        Err(error) => return again(StatusCode::BAD_REQUEST, error.to_string()).await,
    };
    let remote = match tokio::time::timeout(FETCH_TIMEOUT, state.fetcher.fetch(id)).await {
        Ok(Ok(remote)) => remote,
        Ok(Err(error)) => return again(StatusCode::BAD_GATEWAY, error.to_string()).await,
        Err(_) => {
            let text = format!(
                "{} did not answer within {} seconds; try again later.",
                provider.label(),
                FETCH_TIMEOUT.as_secs()
            );
            return again(StatusCode::GATEWAY_TIMEOUT, text).await;
        }
    };
    // A page that came without a place gets one now, from before the fetch
    // moves the release, so the redirect keeps it.
    let at = match at {
        None => store::queue_place(&state.pool, release, &view).await?,
        placed => placed,
    };
    match saved::add_fetched(&state.pool, release, revision, remote).await? {
        AddOutcome::Added => {
            let target = format!("{}#group-{provider}", review_href(release, &view, at, None));
            Ok(Redirect::to(&target).into_response())
        }
        AddOutcome::Changed => again(StatusCode::CONFLICT, CHANGED.to_string()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{Candidate, Evaluation, MatchScope, RemoteRelease};
    use crate::test_support::{
        FIXTURE_MBID, OTHER_MBID, local_release, mbid_release, queue, remote_release, seed_release,
    };
    use axum::extract::rejection::FormRejection;
    use axum::http::{HeaderValue, header};
    use std::sync::Arc;

    fn queued(id: i64) -> store::QueuedRelease {
        store::QueuedRelease {
            id,
            directory: "/music/<b>".into(),
            unit: "/music/<b>".into(),
            album_title: Some("<i>Blue</i>".into()),
            album_artist: Some("Mallory".into()),
            track_count: 2,
            top: vec![
                (ProviderKind::MusicBrainz, Some(0.9314)),
                (ProviderKind::Discogs, None),
            ],
            lead: None,
        }
    }

    #[test]
    fn queue_rows_escape_values_and_carry_the_filter() {
        let view = view_of(Some("deezer"), None, None);
        let html = render_queue(&[queued(7)], 1, &view, None, 1, true);
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
        let html = render_queue(
            &[],
            0,
            &store::QueueView::default(),
            Some(&status),
            1,
            false,
        );
        assert!(html.contains("Accepted musicbrainz abc"), "{html}");
        assert!(html.contains("&lt;b&gt;Blue&lt;/b&gt;"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
    }

    #[tokio::test]
    async fn only_a_matched_release_is_reported_as_accepted() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let titles = ["One", "Two"];
        let release = seed_release(&pool, "/music/a", "Blue", &titles).await;
        let state = AppState {
            pool,
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: "t".into(),
            writable: true,
            fetcher: Arc::new(saved::Fetcher::new(saved::Source::Network)),
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

        queue(
            &state.pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
        )
        .await;
        let shown = store::load_match_guard(&state.pool, release.id)
            .await
            .unwrap();
        let outcome =
            saved::accept_saved(&state.pool, release.id, &mbid_release(FIXTURE_MBID), &shown)
                .await
                .unwrap();
        assert_eq!(outcome, saved::AcceptOutcome::Accepted);
        let accepted = accepted_status(&state, Some(release.id))
            .await
            .unwrap()
            .expect("a matched release is reported");
        assert_eq!(accepted.id, release.id);
        assert_eq!(accepted.status, store::STATUS_MATCHED);
    }

    #[test]
    fn an_empty_queue_says_so() {
        let html = render_queue(&[], 0, &store::QueueView::default(), None, 1, false);
        assert!(html.contains("Nothing is queued for review."), "{html}");
    }

    #[test]
    fn review_links_keep_the_view_and_the_accepted_release() {
        let default = store::QueueView::default();
        assert_eq!(review_href(3, &default, None, None), "/review?release=3");
        assert_eq!(queue_href(&default, None), "/reviews");
        assert_eq!(queue_href(&default, Some(2)), "/reviews?accepted=2");
        let params = ReviewParams {
            provider: Some("spotify".into()),
            page: Some("x".into()),
            ..ReviewParams::default()
        };
        assert_eq!((params.view(), params.page()), (default, 1));
    }

    #[test]
    fn a_view_reads_its_query_and_falls_back_on_anything_unknown() {
        let view = view_of(Some("deezer"), Some("score"), Some("acceptable"));
        assert_eq!(
            view,
            store::QueueView {
                provider: Some(ProviderKind::Deezer),
                sort: store::QueueSort::Score,
                acceptable: true,
            }
        );
        assert_eq!(
            view_of(Some("spotify"), Some("random"), Some("some")),
            store::QueueView::default()
        );
        assert_eq!(view_of(None, None, None), store::QueueView::default());
        assert_eq!(
            view_of(None, Some(" Score "), Some("ACCEPTABLE")),
            view_of(None, Some("score"), Some("acceptable"))
        );
    }

    #[test]
    fn the_accepted_notice_links_with_the_view() {
        let view = view_of(None, Some("score"), None);
        let status = store::LocalReleaseStatus {
            id: 4,
            directory: "/music/a".into(),
            unit: "/music/a".into(),
            album_title: None,
            album_artist: None,
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
        let html = render_accepted(&status, &view);
        assert!(
            html.contains("href=\"/review?release=4&amp;sort=score\""),
            "{html}"
        );
    }

    #[test]
    fn links_carry_the_whole_view() {
        let view = view_of(Some("discogs"), Some("score"), Some("acceptable"));
        assert_eq!(
            review_href(3, &view, None, Some(2)),
            "/review?release=3&provider=discogs&sort=score&show=acceptable&accepted=2"
        );
        let at = Some(store::QueuePlace { score: Some(0.97) });
        assert_eq!(
            review_href(3, &view, at, Some(2)),
            "/review?release=3&provider=discogs&sort=score&show=acceptable&at=0.97&accepted=2"
        );
        // The place means nothing in folder order, and the queue never carries it.
        let folder = store::QueueView::default();
        assert_eq!(review_href(3, &folder, at, None), "/review?release=3");
        assert!(!queue_href(&view, None).contains("at="));
        assert_eq!(
            queue_href(&view, None),
            "/reviews?provider=discogs&sort=score&show=acceptable"
        );
        assert_eq!(
            queue_href(&store::QueueView::default(), Some(2)),
            "/reviews?accepted=2"
        );
    }

    #[test]
    fn the_queue_shows_controls_and_each_releases_reason() {
        let view = view_of(None, Some("score"), Some("acceptable"));
        let mut row = queued(7);
        row.lead = Some(store::Lead {
            provider: ProviderKind::Discogs,
            score: Some(0.97),
            can_apply: false,
            reason: Some("<b>edition</b> qualifiers differ".into()),
            blockers: 3,
        });
        let html = render_queue(&[row], 1, &view, None, 1, true);
        assert!(
            html.contains("discogs: &lt;b&gt;edition&lt;/b&gt; qualifiers differ (+2 more)"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"blocker\">top candidate can't be accepted</span>"),
            "{html}"
        );
        assert!(!html.contains("<b>edition"), "{html}");
        assert!(html.contains("1 release can be accepted."), "{html}");
        assert!(html.contains("<th>Reason</th>"), "{html}");
        for label in ["Sort", "Show", "Provider"] {
            assert!(
                html.contains(&format!(
                    "<p class=\"filters\" role=\"group\" aria-label=\"{label}\">"
                )),
                "{html}"
            );
        }
        assert!(html.contains("Folder</a> · <a"), "{html}");
        assert!(
            html.contains(
                "<a href=\"/reviews?sort=score&amp;show=acceptable\" aria-current=\"true\">Can be accepted</a>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/reviews?show=acceptable\">Folder</a>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"muted\">Sort:</span>")
                && html.contains("<span class=\"muted\">Show:</span>")
                && html.contains("<span class=\"muted\">Provider:</span>"),
            "{html}"
        );
        assert!(
            html.contains("href=\"/reviews?sort=score&amp;show=acceptable&amp;page=2\""),
            "{html}"
        );
        assert!(
            html.contains(
                "href=\"/review?release=7&amp;sort=score&amp;show=acceptable&amp;at=0.97\""
            ),
            "{html}"
        );
    }

    #[test]
    fn a_place_reads_a_score_or_none_and_nothing_else() {
        let place = |score| Some(store::QueuePlace { score });
        assert_eq!(parse_place("0.97"), place(Some(0.97)));
        assert_eq!(parse_place(" 1 "), place(Some(1.0)));
        assert_eq!(parse_place("none"), place(None));
        assert_eq!(parse_place("NONE"), place(None));
        for bad in ["", "abc", "nan", "inf", "-inf", "0,9"] {
            assert_eq!(parse_place(bad), None, "{bad}");
        }
        // What the page writes reads back as the same score.
        for score in [0.97, 0.1 + 0.2, 1.0, 0.0, 1e-9, 0.123_456_789_012_345_68] {
            let text = place_text(store::QueuePlace { score: Some(score) });
            assert_eq!(parse_place(&text), place(Some(score)), "{text}");
        }
        let params = ReviewParams {
            at: Some("0.5".into()),
            ..ReviewParams::default()
        };
        assert_eq!(params.at(), place(Some(0.5)));
        assert_eq!(ReviewParams::default().at(), None);
    }

    #[test]
    fn a_score_view_links_each_id_with_its_place() {
        let lead = |score| {
            Some(store::Lead {
                provider: ProviderKind::Discogs,
                score,
                can_apply: true,
                reason: None,
                blockers: 0,
            })
        };
        let mut scored = queued(7);
        scored.lead = lead(Some(0.97));
        let mut unscored = queued(8);
        unscored.lead = lead(None);
        let rows = [scored, unscored];
        let by_score = view_of(None, Some("score"), None);
        let html = render_queue(&rows, 2, &by_score, None, 1, true);
        assert!(
            html.contains("href=\"/review?release=7&amp;sort=score&amp;at=0.97\""),
            "{html}"
        );
        assert!(
            html.contains("href=\"/review?release=8&amp;sort=score&amp;at=none\""),
            "{html}"
        );
        // Controls and pager do not carry a place.
        assert!(!html.contains("/reviews?sort=score&amp;at"), "{html}");
        assert!(!html.contains("/reviews?at"), "{html}");
        assert_eq!(html.matches("at=").count(), 2, "{html}");
        // Folder order never writes one.
        let html = render_queue(&rows, 2, &store::QueueView::default(), None, 1, true);
        assert!(!html.contains("at="), "{html}");
    }

    #[test]
    fn next_in_queue_carries_the_next_releases_place() {
        let review = review_of(Vec::new());
        let mut shown = view(&review, Some("tok"));
        shown.view = view_of(None, Some("score"), None);
        shown.at = Some(store::QueuePlace { score: Some(0.8) });
        shown.next = Some(store::QueueNext {
            id: 9,
            place: Some(store::QueuePlace { score: Some(0.7) }),
        });
        let html = render_release(&shown);
        assert!(
            html.contains("<a href=\"/review?release=9&amp;sort=score&amp;at=0.7\">Next in queue"),
            "{html}"
        );
        // The forms carry this page's own place.
        assert!(html.contains("name=\"at\" value=\"0.8\""), "{html}");
        shown.next = Some(store::QueueNext {
            id: 9,
            place: Some(store::QueuePlace { score: None }),
        });
        assert!(
            render_release(&shown)
                .contains("href=\"/review?release=9&amp;sort=score&amp;at=none\""),
        );
        // Folder order writes no place anywhere on the page.
        shown.view = store::QueueView::default();
        let html = render_release(&shown);
        assert!(!html.contains("at="), "{html}");
        assert!(!html.contains("name=\"at\""), "{html}");
    }

    #[test]
    fn a_lead_that_can_be_accepted_has_no_mark_and_an_empty_view_says_so() {
        let mut row = queued(7);
        row.lead = Some(store::Lead {
            provider: ProviderKind::MusicBrainz,
            score: Some(0.9),
            can_apply: true,
            reason: Some("uncertain".into()),
            blockers: 0,
        });
        let html = render_queue(&[row], 1, &store::QueueView::default(), None, 1, false);
        assert!(html.contains("musicbrainz: uncertain</td>"), "{html}");
        assert!(!html.contains("can't be accepted"), "{html}");
        assert!(html.contains("1 release queued for review."), "{html}");
        let view = view_of(None, None, Some("acceptable"));
        let html = render_queue(&[], 0, &view, None, 1, false);
        assert!(
            html.contains("No queued release can be accepted."),
            "{html}"
        );
    }

    pub(super) const FILES: [&str; 2] = ["<script>x</script>", "Two"];

    pub(super) fn group(
        state: GroupState,
        remotes: Vec<(RemoteRelease, Vec<String>)>,
    ) -> ReviewGroup {
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

    pub(super) fn review_of(groups: Vec<ReviewGroup>) -> ReleaseReview {
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

    pub(super) fn view<'a>(review: &'a ReleaseReview, token: Option<&'a str>) -> View<'a> {
        View {
            review,
            view: store::QueueView::default(),
            next: None,
            at: None,
            accepted: None,
            token,
            notice: None,
        }
    }

    pub(super) fn exact() -> RemoteRelease {
        remote_release(mbid_release(FIXTURE_MBID), "<i>Blue</i>", &FILES)
    }

    #[test]
    fn out_of_date_and_unreadable_groups_offer_no_accept() {
        let review = review_of(vec![
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
        let review = review_of(Vec::new());
        let mut shown = view(&review, Some("tok"));
        shown.next = Some(store::QueueNext {
            id: 9,
            place: Some(store::QueuePlace { score: None }),
        });
        shown.view = view_of(Some("musicbrainz"), Some("score"), Some("acceptable"));
        let html = render_release(&shown);
        assert!(html.contains("No saved candidates."), "{html}");
        assert!(
            html.contains(
                "href=\"/review?release=9&amp;provider=musicbrainz&amp;sort=score&amp;show=acceptable&amp;at=none\""
            ),
            "{html}"
        );
        assert!(
            html.contains("name=\"filter\" value=\"musicbrainz\""),
            "{html}"
        );
        assert!(html.contains("name=\"sort\" value=\"score\""), "{html}");
        assert!(
            html.contains("name=\"show\" value=\"acceptable\""),
            "{html}"
        );
    }

    #[test]
    fn a_matched_release_is_searched_again_with_review() {
        let mut matched = review_of(Vec::new());
        matched.status.status = store::STATUS_MATCHED.into();
        matched.status.provider = Some(ProviderKind::MusicBrainz);
        matched.status.remote_release_id = Some("abc".into());
        let html = render_release(&view(&matched, Some("tok")));
        assert!(
            html.contains("<code>radish match --review --local-release 12</code>"),
            "{html}"
        );
        assert!(
            html.contains(
                "Accepted now: musicbrainz abc. Accepting another candidate replaces it."
            ),
            "{html}"
        );

        let unmatched = review_of(Vec::new());
        let html = render_release(&view(&unmatched, Some("tok")));
        assert!(
            html.contains("<code>radish match --local-release 12</code>"),
            "{html}"
        );
        assert!(!html.contains("Accepted now"), "{html}");

        let mut stale = review_of(vec![group(GroupState::OutOfDate, Vec::new())]);
        stale.status.status = store::STATUS_MATCHED.into();
        let html = render_release(&view(&stale, Some("tok")));
        assert!(
            html.contains("<code>radish match --review --local-release 12</code> to search again"),
            "{html}"
        );
    }

    #[test]
    fn the_fetch_form_opens_on_the_queue_filter() {
        let review = review_of(Vec::new());
        let mut shown = view(&review, Some("tok"));
        let html = render_release(&shown);
        assert!(!html.contains(" selected"), "{html}");
        shown.view.provider = Some(ProviderKind::Discogs);
        let html = render_release(&shown);
        assert!(
            html.contains("<option value=\"discogs\" selected>Discogs</option>"),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"musicbrainz\">MusicBrainz</option>"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn the_page_is_titled_with_its_release_and_a_missing_release_is_a_404() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One"]).await;
        let state = AppState {
            pool,
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: "t".into(),
            writable: true,
            fetcher: Arc::new(saved::Fetcher::new(saved::Source::Network)),
        };
        let params = |id: i64| ReviewParams {
            release: Some(id.to_string()),
            ..ReviewParams::default()
        };
        let Ok(response) = release_page(State(state.clone()), Query(params(release.id))).await
        else {
            panic!("the handler failed");
        };
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(&format!("<title>Review #{} · Radish</title>", release.id)),
            "{html}"
        );
        let Ok(response) = release_page(State(state), Query(params(release.id + 100))).await else {
            panic!("the handler failed");
        };
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    async fn state(writable: bool) -> AppState {
        let bind = if writable {
            "127.0.0.1:8787"
        } else {
            "0.0.0.0:8787"
        };
        AppState {
            pool: crate::db::connect_in_memory().await.unwrap(),
            bind: bind.parse().unwrap(),
            token: "t".into(),
            writable,
            fetcher: Arc::new(saved::Fetcher::new(saved::Source::Network)),
        }
    }

    fn form(
        action: &str,
        release: i64,
        revision: &str,
        token: &str,
    ) -> Result<Form<ReviewForm>, FormRejection> {
        Ok(Form(ReviewForm {
            token: token.into(),
            action: action.into(),
            release: release.to_string(),
            revision: revision.into(),
            accepted_match: String::new(),
            filter: String::new(),
            sort: String::new(),
            show: String::new(),
            at: String::new(),
            provider: "musicbrainz".into(),
            candidate: FIXTURE_MBID.into(),
            id: String::new(),
        }))
    }

    async fn post(
        state: &AppState,
        headers: HeaderMap,
        form: Result<Form<ReviewForm>, FormRejection>,
    ) -> Response {
        match action(State(state.clone()), headers, form).await {
            Ok(response) => response,
            Err(_) => panic!("the handler failed"),
        }
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A writable server with one release queued for MusicBrainz.
    async fn queued_state() -> (AppState, LocalRelease) {
        let state = state(true).await;
        let release = seed_release(&state.pool, "/music/a", "Blue", &["One", "Two"]).await;
        queue(
            &state.pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]),
        )
        .await;
        (state, release)
    }

    #[tokio::test]
    async fn review_posts_need_a_loopback_server_this_site_and_the_token() {
        let off = state(false).await;
        assert_eq!(
            post(&off, HeaderMap::new(), form("accept", 1, "rev", "t"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );

        let on = state(true).await;
        let mut foreign = HeaderMap::new();
        foreign.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        for (headers, token) in [(foreign, "t"), (HeaderMap::new(), "old")] {
            let response = post(&on, headers, form("accept", 1, "rev", token)).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let html = body(response).await;
            assert!(
                html.contains("Reload the review page and try again"),
                "{html}"
            );
        }
        assert_eq!(
            post(&on, HeaderMap::new(), form("bogus", 1, "rev", "t"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(&on, HeaderMap::new(), form("accept", 99, "rev", "t"))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn accepting_redirects_to_the_queue_and_an_outdated_page_is_refused() {
        let (state, release) = queued_state().await;
        let stale = post(
            &state,
            HeaderMap::new(),
            form("accept", release.id, "old", "t"),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert!(
            body(stale)
                .await
                .contains("changed after the page was loaded")
        );

        // A page that showed an accepted match the release no longer has.
        let Ok(Form(mut shown_matched)) = form("accept", release.id, &release.revision, "t") else {
            unreachable!()
        };
        shown_matched.accepted_match = "5".into();
        let refused = post(&state, HeaderMap::new(), Ok(Form(shown_matched))).await;
        assert_eq!(refused.status(), StatusCode::CONFLICT);

        let accepted = post(
            &state,
            HeaderMap::new(),
            form("accept", release.id, &release.revision, "t"),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            accepted.headers()[header::LOCATION],
            format!("/reviews?accepted={}", release.id).as_str()
        );
    }

    #[tokio::test]
    async fn an_accept_keeps_the_view_in_its_redirect() {
        let (state, release) = queued_state().await;
        let Ok(Form(mut fields)) = form("accept", release.id, &release.revision, "t") else {
            unreachable!()
        };
        fields.sort = "score".into();
        fields.show = "acceptable".into();
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!(
                "/reviews?sort=score&show=acceptable&accepted={}",
                release.id
            )
            .as_str()
        );
    }

    #[tokio::test]
    async fn accepting_in_score_order_goes_on_below_the_accepted_release() {
        let state = state(true).await;
        let titles = ["One", "Two"];
        // Folder order is the reverse of score order, so the two orders
        // disagree about every neighbor.
        let mut releases = Vec::new();
        for (dir, score) in [("/music/a", 0.7), ("/music/b", 0.8), ("/music/c", 0.9)] {
            let release = seed_release(&state.pool, dir, "Blue", &titles).await;
            queue(
                &state.pool,
                &release,
                remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
            )
            .await;
            sqlx::query(
                "UPDATE match_reviews SET top_score = ?1, top_can_apply = 1 WHERE local_release_id = ?2",
            )
            .bind(score)
            .bind(release.id)
            .execute(&state.pool)
            .await
            .unwrap();
            releases.push(release);
        }
        let [low, middle, high] = [&releases[0], &releases[1], &releases[2]];
        let Ok(Form(mut fields)) = form("accept", middle.id, &middle.revision, "t") else {
            unreachable!()
        };
        fields.sort = "score".into();
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        // Not `high`, which was above the accepted release and is not next.
        assert_eq!(
            response.headers()[header::LOCATION],
            format!(
                "/review?release={}&sort=score&at=0.7&accepted={}",
                low.id, middle.id
            )
            .as_str(),
            "{} is the release above",
            high.id
        );
    }

    #[tokio::test]
    async fn accepting_after_a_fetch_moved_the_release_still_goes_on_from_its_place() {
        let state = state(true).await;
        let titles = ["One", "Two"];
        let mut releases = Vec::new();
        for (dir, score) in [("/music/a", 0.9), ("/music/b", 0.8), ("/music/c", 0.7)] {
            let release = seed_release(&state.pool, dir, "Blue", &titles).await;
            queue(
                &state.pool,
                &release,
                remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
            )
            .await;
            sqlx::query(
                "UPDATE match_reviews SET top_score = ?1, top_can_apply = 1 WHERE local_release_id = ?2",
            )
            .bind(score)
            .bind(release.id)
            .execute(&state.pool)
            .await
            .unwrap();
            releases.push(release);
        }
        let [top, middle, bottom] = [&releases[0], &releases[1], &releases[2]];
        // A fetch on the 0.8 page rewrote its summary: it now leads the queue.
        sqlx::query("UPDATE match_reviews SET top_score = 0.95 WHERE local_release_id = ?1")
            .bind(middle.id)
            .execute(&state.pool)
            .await
            .unwrap();
        let Ok(Form(mut fields)) = form("accept", middle.id, &middle.revision, "t") else {
            unreachable!()
        };
        fields.sort = "score".into();
        fields.at = "0.8".into();
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        // The release below where the page was found, not the one above it.
        assert_eq!(
            response.headers()[header::LOCATION],
            format!(
                "/review?release={}&sort=score&at=0.7&accepted={}",
                bottom.id, middle.id
            )
            .as_str(),
            "{} is the release above",
            top.id
        );
    }

    /// Queues one release per `(directory, score)` for MusicBrainz, at that lead score.
    async fn queued_at(state: &AppState, places: &[(&str, f64)]) -> Vec<LocalRelease> {
        let titles = ["One", "Two"];
        let mut releases = Vec::new();
        for (dir, score) in places {
            let release = seed_release(&state.pool, dir, "Blue", &titles).await;
            queue(
                &state.pool,
                &release,
                remote_release(mbid_release(FIXTURE_MBID), "Blue", &titles),
            )
            .await;
            sqlx::query(
                "UPDATE match_reviews SET top_score = ?1, top_can_apply = 1 WHERE local_release_id = ?2",
            )
            .bind(score)
            .bind(release.id)
            .execute(&state.pool)
            .await
            .unwrap();
            releases.push(release);
        }
        releases
    }

    #[tokio::test]
    async fn accepting_after_a_downward_move_does_not_land_on_the_same_release() {
        let state = state(true).await;
        let releases = queued_at(
            &state,
            &[("/music/a", 0.9), ("/music/b", 0.8), ("/music/c", 0.7)],
        )
        .await;
        let [middle, bottom] = [&releases[1], &releases[2]];
        // A fetch moved the 0.8 release down to 0.75: still above the 0.7
        // release, but below where its page was found.
        sqlx::query("UPDATE match_reviews SET top_score = 0.75 WHERE local_release_id = ?1")
            .bind(middle.id)
            .execute(&state.pool)
            .await
            .unwrap();
        let Ok(Form(mut fields)) = form("accept", middle.id, &middle.revision, "t") else {
            unreachable!()
        };
        fields.sort = "score".into();
        fields.at = "0.8".into();
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!(
                "/review?release={}&sort=score&at=0.7&accepted={}",
                bottom.id, middle.id
            )
            .as_str()
        );
    }

    async fn get_page(state: &AppState, params: ReviewParams) -> Response {
        match release_page(State(state.clone()), Query(params)).await {
            Ok(response) => response,
            Err(_) => panic!("the handler failed"),
        }
    }

    #[tokio::test]
    async fn a_review_page_passes_its_place_to_next_and_to_its_forms() {
        let state = state(true).await;
        let releases = queued_at(&state, &[("/music/a", 0.8), ("/music/b", 0.7)]).await;
        let params = |at: &str| ReviewParams {
            release: Some(releases[0].id.to_string()),
            sort: Some("score".into()),
            at: Some(at.into()),
            ..ReviewParams::default()
        };
        // Found at 0.95, though it sits at 0.8 now: the forms keep 0.95, and
        // the next release, the one at 0.8's place, is the one below it.
        let response = get_page(&state, params("0.95")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(response).await;
        assert!(html.contains("name=\"at\" value=\"0.95\""), "{html}");
        assert!(
            html.contains(&format!(
                "href=\"/review?release={}&amp;sort=score&amp;at=0.7\">Next in queue",
                releases[1].id
            )),
            "{html}"
        );
        // A place nobody can read is no place: the page counts from where the release sits.
        let html = body(get_page(&state, params("high")).await).await;
        assert!(!html.contains("name=\"at\""), "{html}");
        assert!(
            html.contains("sort=score&amp;at=0.7\">Next in queue"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn a_refused_accept_keeps_the_place() {
        let state = state(true).await;
        let releases = queued_at(&state, &[("/music/a", 0.8), ("/music/b", 0.7)]).await;
        let Ok(Form(mut fields)) = form("accept", releases[0].id, "old", "t") else {
            unreachable!()
        };
        fields.sort = "score".into();
        fields.at = "0.8".into();
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let html = body(response).await;
        assert!(html.contains("name=\"at\" value=\"0.8\""), "{html}");
        assert!(
            html.contains("sort=score&amp;at=0.7\">Next in queue"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn a_fetch_is_checked_before_any_provider_is_asked() {
        let (state, release) = queued_state().await;
        let fetch =
            |revision: &str, provider: &str, id: &str| -> Result<Form<ReviewForm>, FormRejection> {
                let Ok(Form(mut fields)) = form("fetch", release.id, revision, "t") else {
                    unreachable!()
                };
                fields.provider = provider.into();
                fields.id = id.into();
                fields.sort = "score".into();
                fields.show = "acceptable".into();
                fields.at = "0.8".into();
                Ok(Form(fields))
            };
        let kept_view = |html: &str| {
            assert!(html.contains("name=\"at\" value=\"0.8\""), "{html}");
            assert!(html.contains("name=\"sort\" value=\"score\""), "{html}");
            assert!(
                html.contains("name=\"show\" value=\"acceptable\""),
                "{html}"
            );
        };
        let link = format!("https://musicbrainz.org/release/{FIXTURE_MBID}");
        let wrong = post(
            &state,
            HeaderMap::new(),
            fetch(&release.revision, "discogs", &link),
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
        let html = body(wrong).await;
        assert!(
            html.contains("that is a MusicBrainz link; choose MusicBrainz"),
            "{html}"
        );
        assert!(
            html.contains(&format!("value=\"{link}\"")),
            "the entered text is kept: {html}"
        );
        assert!(
            html.contains("<option value=\"discogs\" selected>"),
            "{html}"
        );
        kept_view(&html);

        let stale = post(
            &state,
            HeaderMap::new(),
            fetch("old", "musicbrainz", FIXTURE_MBID),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        kept_view(&body(stale).await);
    }

    #[tokio::test]
    async fn a_fetch_that_cannot_start_answers_502_with_the_page_and_the_entered_text() {
        let (mut state, release) = queued_state().await;
        let missing = crate::test_support::ScratchDir::new("review-no-index");
        state.fetcher = Arc::new(saved::Fetcher::new(saved::Source::Indexes(
            crate::index::IndexPaths::from_dir(missing.path().join("none")),
        )));
        let Ok(Form(mut fields)) = form("fetch", release.id, &release.revision, "t") else {
            unreachable!()
        };
        fields.id = format!(" {FIXTURE_MBID} ");
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let html = body(response).await;
        assert!(html.contains("MusicBrainz is not available"), "{html}");
        assert!(
            html.contains(&format!("value=\"{FIXTURE_MBID}\"")),
            "{html}"
        );
    }

    #[tokio::test]
    async fn an_unsaved_candidate_is_a_409_and_a_second_submit_is_refused() {
        let (state, release) = queued_state().await;
        let Ok(Form(mut other)) = form("accept", release.id, &release.revision, "t") else {
            unreachable!()
        };
        other.candidate = OTHER_MBID.into();
        let response = post(&state, HeaderMap::new(), Ok(Form(other))).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(body(response).await.contains("no longer saved"));

        let send = || form("accept", release.id, &release.revision, "t");
        let first = post(&state, HeaderMap::new(), send()).await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        let second = post(&state, HeaderMap::new(), send()).await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        assert!(
            body(second)
                .await
                .contains("changed after the page was loaded")
        );
    }

    #[tokio::test]
    async fn a_mapping_that_cannot_be_applied_is_a_422() {
        let state = state(true).await;
        let release = seed_release(&state.pool, "/music/a", "Blue", &["One", "Two", "Three"]).await;
        queue(
            &state.pool,
            &release,
            remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]),
        )
        .await;
        let response = post(
            &state,
            HeaderMap::new(),
            form("accept", release.id, &release.revision, "t"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body(response).await.contains("Cannot accept:"));
    }

    #[tokio::test]
    async fn text_that_a_page_never_writes_is_a_400() {
        let (state, release) = queued_state().await;
        for (matched, candidate) in [
            ("x", FIXTURE_MBID),
            ("", "nope"),
            ("", "https://musicbrainz.org/release/x"),
        ] {
            let Ok(Form(mut fields)) = form("accept", release.id, &release.revision, "t") else {
                unreachable!()
            };
            fields.accepted_match = matched.into();
            fields.candidate = candidate.into();
            let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{matched} {candidate}"
            );
        }
        // A link is for the fetch form only: the candidate field holds an ID.
        let link = format!("https://musicbrainz.org/release/{FIXTURE_MBID}");
        let Ok(Form(mut fields)) = form("accept", release.id, &release.revision, "t") else {
            unreachable!()
        };
        fields.candidate = link;
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_over_long_entry_is_refused_and_echoed_only_in_part() {
        let (state, release) = queued_state().await;
        let Ok(Form(mut fields)) = form("fetch", release.id, &release.revision, "t") else {
            unreachable!()
        };
        fields.id = "é".repeat(MAX_ENTERED_CHARS + 50);
        let response = post(&state, HeaderMap::new(), Ok(Form(fields))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let html = body(response).await;
        assert!(html.contains("at most 200 characters"), "{html}");
        assert!(
            html.contains(&format!("value=\"{}\"", "é".repeat(MAX_ENTERED_CHARS))),
            "{html}"
        );
        assert!(!html.contains(&"é".repeat(MAX_ENTERED_CHARS + 1)), "{html}");
    }

    #[test]
    fn the_fetch_form_and_its_notice_can_be_jumped_to() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<h2 id=\"enter\">Enter a release</h2>"),
            "{html}"
        );
        assert!(
            html.contains("<h2 id=\"group-musicbrainz\">MusicBrainz</h2>"),
            "{html}"
        );
        assert!(
            html.contains("<form method=\"post\" action=\"/review#enter\">"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"#enter\">Enter a release by ID</a>"),
            "{html}"
        );
        assert!(html.contains(" required "), "{html}");
        // Accept forms leave the page, so they keep the plain action.
        assert!(
            html.contains("<form method=\"post\" action=\"/review\">"),
            "{html}"
        );
        let html = render_release(&view(&review, None));
        assert!(!html.contains("href=\"#enter\""), "{html}");
    }

    #[test]
    fn a_fetch_error_selects_the_entered_provider_over_the_queue_filter() {
        let review = review_of(Vec::new());
        let notice = Notice {
            entered: Some((ProviderKind::Deezer, "<b>x</b>".into())),
            text: "bad <i>id</i>".into(),
        };
        let mut shown = view(&review, Some("tok"));
        shown.view.provider = Some(ProviderKind::Discogs);
        shown.notice = Some(&notice);
        let html = render_release(&shown);
        assert!(
            html.contains("<option value=\"deezer\" selected>Deezer</option>"),
            "{html}"
        );
        assert!(
            !html.contains("<option value=\"discogs\" selected>"),
            "{html}"
        );
        assert!(html.contains("value=\"&lt;b&gt;x&lt;/b&gt;\""), "{html}");
        assert!(
            html.contains("<p class=\"notice\">bad &lt;i&gt;id&lt;/i&gt;</p>"),
            "{html}"
        );
        // A notice from the fetch form is shown once, beside that form.
        assert_eq!(html.matches("class=\"notice\"").count(), 1, "{html}");
    }
}
