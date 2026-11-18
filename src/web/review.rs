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
    AppError, AppState, Nav, PAGE_SIZE, cross_site, escape_html, folder_href, is_web_url, page,
    page_param, pager_with, relative_path, render_match, same_token,
};
use crate::layout::{format_duration, plural, track_number};
use crate::matcher::{self, Candidate, Evaluation, LocalRelease, RemoteTrack};
use crate::providers::{ProviderKind, ProviderPolicy, ReleaseId};
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
    /// The queue filter carried from `/reviews`; it decides `Next in queue`.
    filter: Option<ProviderKind>,
    /// The release `Next in queue` leads to, if the queue has another.
    next: Option<i64>,
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
        params.filter(),
        accepted.as_ref(),
        None,
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
    notice: Option<Notice>,
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
        notice: notice.as_ref(),
    };
    Ok((
        status,
        Html(page(
            &format!("Review #{id}"),
            Nav::Review,
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
        "<p class=\"pager\">{next}</p>{accepted}<h1>#{} {} — {}</h1>\
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
    if !current {
        // A current card shows its blockers above the Accept button instead.
        notes.extend(
            evaluation
                .blockers
                .iter()
                .map(|note| format!("blocker: {note}")),
        );
    }
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

/// The most provider tracks no file maps to that a track table lists.
///
/// A release of a few files can map onto a box set of thousands of tracks;
/// listing them all would make the page megabytes long. The rest are counted.
const MAX_UNMAPPED_ROWS: usize = 25;

/// The files side by side with the provider tracks they map to, then the
/// provider's playable tracks that no file maps to (on the mapped disc only,
/// at most [`MAX_UNMAPPED_ROWS`] of them).
fn render_mapping(local: &LocalRelease, candidate: &Candidate) -> String {
    let remote = &candidate.release.tracklist;
    let media = candidate.release.track_media();
    // A position such as `3` only means something with its disc beside it.
    // Counted the way `disc_label` counts, so a CD plus a video DVD, whose
    // tracks are not audio, keeps its plain positions.
    let several_media = matcher::remote_media(&candidate.release).is_some();
    let cells = |remote_index: usize| {
        remote_cells(
            &remote[remote_index],
            media[remote_index].filter(|_| several_media),
        )
    };
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
            paired.map_or_else(
                || "<td>—</td><td class=\"muted\">not mapped</td><td></td>".to_string(),
                cells
            ),
        ));
    }
    let mut unmapped = 0_usize;
    for (remote_index, track) in remote.iter().enumerate() {
        let on_disc = candidate
            .mapping
            .medium
            .is_none_or(|medium| media[remote_index] == Some(medium));
        if shown[remote_index] || !track.is_playable() || !on_disc {
            continue;
        }
        unmapped += 1;
        if unmapped <= MAX_UNMAPPED_ROWS {
            rows.push_str(&format!(
                "<tr><td></td><td class=\"muted\">not in the files</td><td></td>{}</tr>",
                cells(remote_index)
            ));
        }
    }
    if let Some(more) = unmapped
        .checked_sub(MAX_UNMAPPED_ROWS)
        .filter(|&more| more > 0)
    {
        rows.push_str(&format!(
            "<tr><td></td><td class=\"muted\" colspan=\"5\">… and {more} more provider \
             {} not in the files</td></tr>",
            if more == 1 { "track" } else { "tracks" }
        ));
    }
    format!(
        "<div class=\"table\"><table><thead><tr><th class=\"num\">#</th><th>File title</th>\
         <th class=\"num\">File time</th><th>Pos</th><th>{label} title</th>\
         <th class=\"num\">{label} time</th></tr></thead><tbody>{rows}</tbody></table></div>",
        label = candidate.release.provider().label()
    )
}

/// The provider side of a track table row.
///
/// `medium` is the disc to show before the position, for a release of several
/// discs; a position that already names it (`2-3`) is left as it is.
fn remote_cells(track: &RemoteTrack, medium: Option<u32>) -> String {
    let position = track.position.as_deref().unwrap_or("");
    let position = match medium {
        Some(medium) if !position.is_empty() && !position.starts_with(&format!("{medium}-")) => {
            format!("{medium}-{position}")
        }
        _ => position.to_string(),
    };
    format!(
        "<td>{}</td><td>{}</td><td class=\"num\">{}</td>",
        escape_html(&position),
        escape_html(&track.title),
        format_duration(track.duration_secs),
    )
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
    let id = escape_html(&candidate.release.id.storage_id());
    format!(
        "{blockers}<form method=\"post\" action=\"/review\">{}\
         <input type=\"hidden\" name=\"action\" value=\"accept\">\
         <input type=\"hidden\" name=\"provider\" value=\"{provider}\">\
         <input type=\"hidden\" name=\"candidate\" value=\"{id}\">\
         <button aria-label=\"{label}: {provider} {id}\">{label}</button></form>",
        hidden_fields(view, token),
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
    // After a failed fetch the form keeps what was entered, and the provider
    // chosen then wins over the queue's filter.
    let (chosen, text) = match view.notice.and_then(|notice| notice.entered.as_ref()) {
        Some((provider, text)) => (Some(*provider), text.as_str()),
        None => (view.filter, ""),
    };
    let notice = match view.notice {
        Some(notice) if notice.entered.is_some() => {
            format!("<p class=\"notice\">{}</p>", escape_html(&notice.text))
        }
        _ => String::new(),
    };
    let options: String = ProviderPolicy::Chain { deezer: true }
        .order()
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
        "<h2>Enter a release</h2>{notice}<form method=\"post\" action=\"/review\">{}\
         <input type=\"hidden\" name=\"action\" value=\"fetch\">\
         <select name=\"provider\" aria-label=\"Provider\">{options}</select>\
         <input type=\"text\" name=\"id\" value=\"{}\" maxlength=\"{MAX_ENTERED_CHARS}\" \
         placeholder=\"ID or link\" aria-label=\"Release ID or link\"><button>Fetch</button></form>",
        hidden_fields(view, token),
        escape_html(text),
    )
}

/// What a page built from an outdated release says.
const CHANGED: &str = "This release changed after the page was loaded; review it again.";

/// How long a fetch from the review page may take before the page gives up.
///
/// The adapters pace MusicBrainz to about a request a second and retry
/// transient failures, honoring a `Retry-After` of up to two minutes; a
/// browser request should not hang that long. Thirty seconds covers a retried
/// lookup. A fetch cut off here changes nothing: the release was not stored.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The review form's fields; which ones matter depends on `action`.
#[derive(Debug, Deserialize)]
pub(super) struct ReviewForm {
    token: String,
    action: String,
    release: String,
    revision: String,
    /// The accepted match id the page showed; empty for none.
    #[serde(default, rename = "match")]
    accepted_match: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    candidate: String,
    #[serde(default)]
    id: String,
}

/// The `403` for a review form from another site or with the wrong token.
///
/// A tab left open across a server restart holds the old process's token, so
/// the body says what to do.
fn stale_review() -> Response {
    (
        StatusCode::FORBIDDEN,
        "forbidden: reload the review page and try again",
    )
        .into_response()
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
        return Ok((StatusCode::FORBIDDEN, "forbidden").into_response());
    }
    if cross_site(&headers) {
        return Ok(stale_review());
    }
    let Form(form) = match form {
        Ok(form) => form,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    if !same_token(&state.token, &form.token) {
        return Ok(stale_review());
    }
    let accepting = match form.action.as_str() {
        "accept" => true,
        "fetch" => false,
        _ => return Ok((StatusCode::BAD_REQUEST, "unknown action").into_response()),
    };
    let Ok(release) = form.release.trim().parse::<i64>() else {
        return Ok(no_release());
    };
    let Some(provider) = ProviderKind::parse(&form.provider) else {
        return Ok((StatusCode::BAD_REQUEST, "unknown provider").into_response());
    };
    let filter = ProviderKind::parse(&form.filter);
    let revision = form.revision.trim();
    if !accepting {
        return fetch(&state, release, revision, filter, provider, form.id.trim()).await;
    }
    // The page writes these fields itself, so text that does not read is not
    // from a review page.
    let accepted_match_id = match form.accepted_match.trim() {
        "" => None,
        id => match id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => return Ok((StatusCode::BAD_REQUEST, "unknown match").into_response()),
        },
    };
    let Ok(candidate) = saved::parse_entered(provider, form.candidate.trim()) else {
        return Ok((StatusCode::BAD_REQUEST, "unknown candidate").into_response());
    };
    let shown = store::MatchGuard {
        source_revision: revision.to_string(),
        accepted_match_id,
    };
    accept(&state, release, &shown, filter, &candidate).await
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
    filter: Option<ProviderKind>,
    candidate: &ReleaseId,
) -> Result<Response, AppError> {
    let (status, text) = match saved::accept_saved(&state.pool, release, candidate, shown).await? {
        AcceptOutcome::Accepted => {
            let target = match store::next_queued(&state.pool, release, filter).await? {
                Some(next) => review_href(next, filter, Some(release)),
                None => queue_href(filter, Some(release)),
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
    render_page(state, release, filter, None, Some(notice), status).await
}

/// Fetches the release `entered` names and puts it first in its provider's review.
///
/// The page is checked before any provider is asked: `404` for a release that
/// is gone, `409` for one that changed. Then `400` for text that names no
/// release of `provider`, and `502` when the fetch fails (the provider's
/// message, or a missing token or index). A success goes back to the page.
async fn fetch(
    state: &AppState,
    release: i64,
    revision: &str,
    filter: Option<ProviderKind>,
    provider: ProviderKind,
    entered: &str,
) -> Result<Response, AppError> {
    let again = |status: StatusCode, text: String| {
        let notice = Notice {
            entered: Some((provider, entered.to_string())),
            text,
        };
        render_page(state, release, filter, None, Some(notice), status)
    };
    match store::load_match_guard(&state.pool, release).await {
        Ok(guard) if guard.source_revision == revision => {}
        Ok(_) => return again(StatusCode::CONFLICT, CHANGED.to_string()).await,
        Err(error) if error.downcast_ref::<store::SnapshotChanged>().is_some() => {
            return Ok(no_release());
        }
        Err(error) => return Err(error.into()),
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
            return again(StatusCode::BAD_GATEWAY, text).await;
        }
    };
    match saved::add_fetched(&state.pool, release, revision, remote).await? {
        AddOutcome::Added => Ok(Redirect::to(&review_href(release, filter, None)).into_response()),
        AddOutcome::Changed => again(StatusCode::CONFLICT, CHANGED.to_string()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{MatchScope, RemoteRelease};
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

    fn review_of(groups: Vec<ReviewGroup>) -> ReleaseReview {
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
            notice: None,
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
        let review = review_of(vec![group(
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
            html.contains(">Accept, overriding 1 check</button>"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<button aria-label=\"Accept, overriding 1 check: musicbrainz {FIXTURE_MBID}\">"
            )),
            "{html}"
        );
        assert!(html.contains("<th class=\"num\">File time</th>"), "{html}");
        assert!(
            html.contains("<th class=\"num\">MusicBrainz time</th>"),
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
        let review = review_of(vec![group(
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

    /// `remote_release` of `FILES` followed by `extra` more tracks.
    fn remote_with_extra(extra: usize) -> RemoteRelease {
        let more: Vec<String> = (0..extra).map(|index| format!("Extra {index}")).collect();
        let mut titles: Vec<&str> = FILES.to_vec();
        titles.extend(more.iter().map(String::as_str));
        remote_release(mbid_release(FIXTURE_MBID), "<i>Blue</i>", &titles)
    }

    #[test]
    fn unmapped_provider_tracks_are_listed_up_to_a_limit_and_then_counted() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(remote_with_extra(38), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert_eq!(
            html.matches("<td class=\"muted\">not in the files</td>")
                .count(),
            MAX_UNMAPPED_ROWS,
            "{html}"
        );
        assert!(
            html.contains("… and 13 more provider tracks not in the files"),
            "{html}"
        );
        assert!(html.contains("Extra 0"), "{html}");
        assert!(!html.contains("Extra 37"), "{html}");

        // Exactly at the limit there is nothing to count.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(remote_with_extra(MAX_UNMAPPED_ROWS), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(!html.contains("more provider"), "{html}");
    }

    #[test]
    fn unmapped_provider_tracks_come_after_the_mapped_rows() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(longer(), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        let unmapped = html.find("not in the files").expect("an unmapped row");
        let last_mapped = html.rfind("<td>Two</td>").expect("a mapped row");
        assert!(last_mapped < unmapped, "{html}");
        assert!(html.find("<td>Bonus</td>").unwrap() > unmapped, "{html}");
    }

    #[test]
    fn a_mapping_shorter_than_the_files_still_lists_every_file() {
        let mut review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        review.groups[0].candidates[0].0.mapping.pairs.truncate(1);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>Two</td><td class=\"num\">3:00</td><td>—</td>"),
            "{html}"
        );
        assert!(html.contains("not mapped"), "{html}");
    }

    #[test]
    fn positions_name_the_disc_only_on_a_release_of_several_discs() {
        let mut remote = remote_release(
            mbid_release(FIXTURE_MBID),
            "<i>Blue</i>",
            &["A", "B", FILES[0], FILES[1]],
        );
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(index as u32 / 2 + 1);
            track.track_position = Some(index as u32 % 2 + 1);
            track.position = Some((index % 2 + 1).to_string());
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>2-1</td><td>&lt;script&gt;x&lt;/script&gt;</td>"),
            "{html}"
        );
        assert!(html.contains("<td>2-2</td><td>Two</td>"), "{html}");

        // One disc keeps its plain positions.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>1</td><td>&lt;script&gt;x&lt;/script&gt;</td>"),
            "{html}"
        );
        assert!(!html.contains("<td>1-1</td>"), "{html}");
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
    fn an_out_of_date_card_lists_its_blockers_as_notes_without_a_button() {
        let review = review_of(vec![group(
            GroupState::OutOfDate,
            vec![(exact(), vec!["<b>close</b>".into()])],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<li>blocker: &lt;b&gt;close&lt;/b&gt;</li>"),
            "{html}"
        );
        assert!(!html.contains("<button aria-label"), "{html}");

        // A current card keeps them above its button instead.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), vec!["<b>close</b>".into()])],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(!html.contains("blocker: "), "{html}");
    }

    #[test]
    fn a_card_without_blockers_says_plain_accept_and_links_only_web_urls() {
        let mut remote = exact();
        remote.url = "javascript:alert(1)".into();
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(html.contains(">Accept</button>"), "{html}");
        assert!(
            html.contains(&format!(
                "aria-label=\"Accept: musicbrainz {FIXTURE_MBID}\""
            )),
            "{html}"
        );
        assert!(!html.contains("javascript"), "{html}");
        assert!(html.contains(FIXTURE_MBID), "{html}");
    }

    #[test]
    fn the_fetch_form_opens_on_the_queue_filter() {
        let review = review_of(Vec::new());
        let mut shown = view(&review, Some("tok"));
        let html = render_release(&shown);
        assert!(!html.contains(" selected"), "{html}");
        shown.filter = Some(ProviderKind::Discogs);
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

    #[test]
    fn positions_that_already_name_the_disc_are_not_prefixed_again() {
        let mut remote = remote_release(
            mbid_release(FIXTURE_MBID),
            "<i>Blue</i>",
            &["A", "B", FILES[0], FILES[1]],
        );
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            let (medium, position) = (index as u32 / 2 + 1, index as u32 % 2 + 1);
            track.medium_position = Some(medium);
            track.track_position = Some(position);
            track.position = Some(format!("{medium}-{position}"));
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(html.contains("<td>2-1</td>"), "{html}");
        assert!(!html.contains("2-2-"), "{html}");
        assert!(!html.contains("1-1-"), "{html}");
    }

    #[test]
    fn a_disc_without_a_named_medium_keeps_plain_positions() {
        // Two discs and an audio track that names no disc.
        let mut remote = remote_with_extra(1);
        for (index, medium) in [(0, 1), (1, 2)] {
            remote.tracklist[index].medium_position = Some(medium);
            remote.tracklist[index].track_position = Some(1);
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            !html.contains("<td>1-1</td>") && !html.contains("<td>2-2</td>"),
            "{html}"
        );
        assert!(!html.contains("<td>2-1</td>"), "{html}");
        assert!(html.contains("<td>2</td><td>Two</td>"), "{html}");
    }

    #[test]
    fn a_pair_pointing_past_the_provider_tracks_shows_the_file_as_not_mapped() {
        let mut review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        review.groups[0].candidates[0].0.mapping.pairs[0] = Some(99);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>—</td><td class=\"muted\">not mapped</td>"),
            "{html}"
        );
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
            assert_eq!(
                body(response).await,
                "forbidden: reload the review page and try again"
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
    async fn a_fetch_is_checked_before_any_provider_is_asked() {
        let (state, release) = queued_state().await;
        let fetch =
            |revision: &str, provider: &str, id: &str| -> Result<Form<ReviewForm>, FormRejection> {
                let Ok(Form(mut fields)) = form("fetch", release.id, revision, "t") else {
                    unreachable!()
                };
                fields.provider = provider.into();
                fields.id = id.into();
                Ok(Form(fields))
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

        let stale = post(
            &state,
            HeaderMap::new(),
            fetch("old", "musicbrainz", FIXTURE_MBID),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);
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

    #[test]
    fn a_fetch_error_selects_the_entered_provider_over_the_queue_filter() {
        let review = review_of(Vec::new());
        let notice = Notice {
            entered: Some((ProviderKind::Deezer, "<b>x</b>".into())),
            text: "bad <i>id</i>".into(),
        };
        let mut shown = view(&review, Some("tok"));
        shown.filter = Some(ProviderKind::Discogs);
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

    #[test]
    fn an_out_of_date_card_orders_its_notes_like_the_command_line() {
        let mut review = review_of(vec![group(
            GroupState::OutOfDate,
            vec![(exact(), vec!["blocked".into()])],
        )]);
        review.groups[0].candidates[0]
            .1
            .warnings
            .push("warned".into());
        let html = render_release(&view(&review, Some("tok")));
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {html}"))
        };
        let order = [
            at("evidence: "),
            at("blocker: blocked"),
            at("warning: warned"),
        ];
        assert!(order.is_sorted(), "{order:?} in {html}");
    }
}
