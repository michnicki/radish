//! A small web interface over the library database.
//!
//! `radish serve` shows server-rendered HTML pages: **Status** (what the
//! library holds), **Tracks** (the `list` and `search` data), **Releases**
//! (the `releases` data), **Review** (the queue of releases waiting for a
//! decision, with a page per release) and **Folders** (the `folder list` data,
//! with a page per folder). There are two writes: the folder form, which
//! merges a folder's releases, keeps them apart or leaves them to the
//! automatic rule, and the review form, which accepts a saved candidate or
//! fetches a release by its ID. No JavaScript is served.
//!
//! The server has no authentication, so safety rests on its defaults:
//!
//! - It binds to loopback unless told otherwise, and only a loopback server
//!   takes writes.
//! - A request whose `Host` header is not a loopback name or the bind address
//!   gets a plain-text `403`, which stops DNS-rebinding pages from reading the
//!   library.
//! - A `POST` needs this process's random form token and must not come from
//!   another site (`Sec-Fetch-Site`).
//! - Tags, paths and provider titles are untrusted. Each value goes through
//!   [`Clean`](crate::output::Clean) and then HTML escaping before it is
//!   written into a page.
//! - Every response carries a strict `Content-Security-Policy`.
//!
//! An error is a page with the usual header and a link back, so a reader who
//! hits one keeps the navigation. Only the `403` for a foreign `Host` and
//! requests the pages never send (a repeated query parameter, an unsupported
//! method) get axum's bare answer instead. A failure's detail goes to stderr,
//! not to the page.

// Rust guideline compliant 2026-08-14

use crate::grouping::FolderAction;
use crate::layout::{format_duration, plural, track_number};
use crate::output::Clean;
use crate::{outln, store};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::rejection::FormRejection;
use axum::extract::{DefaultBodyLimit, Form, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;

mod review;

/// Rows on one page of tracks or releases.
///
/// Large enough to scan a whole album or a short search result in one go, and
/// small enough that a page of very long titles stays well under a megabyte
/// of HTML. Changing it only changes paging, not what is shown.
const PAGE_SIZE: usize = 100;

/// The longest search text used, in characters; the rest is ignored.
///
/// The search is a `LIKE` scan over every track, so an unbounded query from a
/// URL would let one request do arbitrary work.
const MAX_QUERY_CHARS: usize = 200;

/// The highest page number honored.
///
/// Keeps `page * PAGE_SIZE` far from overflow, whatever the URL says. A real
/// library never has a million pages.
const MAX_PAGE: u64 = 1_000_000;

/// The message of the `403` a server that takes no writes gives to a form.
const NOT_WRITABLE: &str = "This server listens on a non-loopback address, so it changes nothing.";

/// The largest form body accepted, in bytes. A folder form holds a path, an
/// action and a token; a review form a few short fields and an entered ID of
/// at most 200 characters.
const MAX_FORM_BYTES: usize = 16 * 1024;

/// The column headings of a release table.
const RELEASE_HEADER: &str = "<tr><th class=\"num\">ID</th><th>Artist</th><th>Album</th>\
     <th class=\"num\">Tracks</th><th>Status</th><th>Match</th><th>Folder</th></tr>";

/// What the browser may load: nothing, except this page's own inline style.
///
/// `frame-ancestors` does not fall back to `default-src`, so it is named: with
/// it, no other site can frame a folder page and trick a click on its button
/// (`X-Frame-Options` says the same to browsers that predate it).
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'";

/// The page stylesheet, embedded at compile time and inlined into every page,
/// so the server never serves a second file.
const STYLE: &str = include_str!("web/style.css");

/// Shared state for the handlers and the request guard.
#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    /// The address actually bound, which decides which `Host` values are fine.
    bind: SocketAddr,
    /// This server process's form token: every form carries it, and a `POST`
    /// without it is refused.
    token: Arc<str>,
    /// Whether forms may change anything: only on a loopback bind.
    writable: bool,
    /// Fetches releases entered on the review page; builds nothing until then.
    fetcher: Arc<crate::review::Fetcher>,
}

/// Serves the web interface on `bind` until Ctrl+C.
///
/// Prints `Serving http://<address>` once listening, with the real port when
/// `bind` asked for port 0. A non-loopback `bind` is allowed but warns on
/// stderr, because the pages show library paths to anyone who can connect.
///
/// `fetcher` serves the review page's fetch form.
///
/// # Errors
///
/// Returns an error when the address cannot be bound or the server fails.
pub async fn serve(
    pool: SqlitePool,
    bind: SocketAddr,
    fetcher: crate::review::Fetcher,
) -> Result<()> {
    if !bind.ip().is_loopback() {
        eprintln!(
            "warning: {bind} is not a loopback address; the web interface has no \
             authentication and shows library paths to anyone who can connect"
        );
    }
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    let bind = listener.local_addr()?;
    let token = new_token()?;

    outln!("Serving http://{bind} (Ctrl+C to stop)");
    axum::serve(listener, router(pool, bind, token, fetcher))
        .with_graceful_shutdown(async {
            // A failed signal handler leaves Ctrl+C to the default action.
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("web server failed")
}

fn router(
    pool: SqlitePool,
    bind: SocketAddr,
    token: String,
    fetcher: crate::review::Fetcher,
) -> Router {
    let state = AppState {
        pool,
        bind,
        token: token.into(),
        writable: writes_allowed(bind),
        fetcher: Arc::new(fetcher),
    };
    Router::new()
        .route("/", get(status_page))
        .route("/tracks", get(tracks_page))
        .route("/releases", get(releases_page))
        .route("/reviews", get(review::queue_page))
        .route(
            "/review",
            get(review::release_page)
                .post(review::action)
                .layer(DefaultBodyLimit::max(MAX_FORM_BYTES)),
        )
        .route("/folders", get(folders_page))
        .route(
            "/folder",
            get(folder_page)
                .post(folder_action)
                .layer(DefaultBodyLimit::max(MAX_FORM_BYTES)),
        )
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Whether a server bound to `bind` may change anything. Anyone who can load a
/// page can read its token, so only a loopback bind takes writes.
fn writes_allowed(bind: SocketAddr) -> bool {
    bind.ip().is_loopback()
}

/// A fresh random form token: 128 bits as 32 hex digits.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("cannot create a form token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Compares tokens in time that does not depend on where they differ.
fn same_token(expected: &str, given: &str) -> bool {
    let (expected, given) = (expected.as_bytes(), given.as_bytes());
    expected.len() == given.len()
        && expected
            .iter()
            .zip(given)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Whether the browser says the request came from elsewhere. Browsers send
/// `Sec-Fetch-Site` with every request; a client that sends none still needs
/// the token. `Origin` is no help here: with `Referrer-Policy: no-referrer`,
/// browsers send `Origin: null` on a `POST`.
fn cross_site(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .is_some_and(|value| value.as_bytes() != b"same-origin")
}

/// Rejects foreign `Host` headers and adds the security headers to every
/// response, including errors and `404`s.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(|host| host_allowed(state.bind, host));
    let mut response = if allowed {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden host").into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Whether a `Host` header value names this server.
///
/// Loopback names and addresses always pass, and so does the bind address. A
/// wildcard bind (`0.0.0.0`) cannot know its own names, so it accepts any host.
fn host_allowed(bind: SocketAddr, host: &str) -> bool {
    if bind.ip().is_unspecified() {
        return true;
    }
    let name = match host.strip_prefix('[') {
        // "[::1]:8787" -> "::1"
        Some(rest) => rest.split_once(']').map_or(rest, |(ip, _)| ip),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip == bind.ip())
}

/// A handler failure: logged on stderr, reported to the browser as an error
/// page whose message says nothing about the cause.
struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("error: {}", Clean(&format!("{:#}", self.0)));
        error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "Internal error; the server log says why.",
            ("/", "Go to the status page"),
        )
    }
}

/// An error as a page with the usual header, a message and one link back.
///
/// So a reader who hits an error keeps the navigation. `back` is
/// `(href, text)`; `message` is plain text.
fn error_page(
    status: StatusCode,
    active: Option<Nav>,
    message: &str,
    back: (&str, &str),
) -> Response {
    let title = status.canonical_reason().unwrap_or("Error");
    let body = format!(
        "<h1>{}</h1><p class=\"notice error\" role=\"alert\">{}</p><p><a href=\"{}\">{}</a></p>",
        escape_html(title),
        escape_html(message),
        escape_html(back.0),
        escape_html(back.1),
    );
    (status, Html(page(title, active, &body))).into_response()
}

/// Any path the server does not serve.
async fn not_found() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        None,
        "No page here.",
        ("/", "Go to the status page"),
    )
}

/// An error page that links back to the folder list.
fn folders_error(status: StatusCode, message: &str) -> Response {
    error_page(
        status,
        Some(Nav::Folders),
        message,
        ("/folders", "Back to the folders"),
    )
}

/// Query parameters shared by the paged pages.
///
/// `page` stays text so a malformed value falls back to page 1 instead of
/// failing the request.
#[derive(Debug, Default, Deserialize)]
struct ListParams {
    q: Option<String>,
    page: Option<String>,
}

impl ListParams {
    /// The 1-based page, defaulting to 1 for anything unusable.
    fn page(&self) -> u64 {
        page_param(self.page.as_deref())
    }

    /// The trimmed search text, or `None` when empty.
    fn query(&self) -> Option<String> {
        let query = self.q.as_deref()?.trim();
        let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
        (!query.is_empty()).then_some(query)
    }
}

/// The 1-based page in `value`, defaulting to 1 for anything unusable.
fn page_param(value: Option<&str>) -> u64 {
    value
        .and_then(|page| page.trim().parse::<u64>().ok())
        .map_or(1, |page| page.clamp(1, MAX_PAGE))
}

async fn status_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let status = store::library_status(&state.pool).await?;
    Ok(Html(page(
        "Status",
        Some(Nav::Status),
        &render_status(&status),
    )))
}

async fn tracks_page(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Html<String>, AppError> {
    let page_number = params.page();
    let query = params.query();
    let offset = (page_number - 1) * PAGE_SIZE as u64;
    // One row past the page tells us whether a next page exists.
    let mut rows = store::track_page(
        &state.pool,
        query.as_deref(),
        PAGE_SIZE as i64 + 1,
        offset as i64,
    )
    .await?;
    let has_next = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let body = render_tracks(&rows, query.as_deref(), page_number, has_next);
    Ok(Html(page("Tracks", Some(Nav::Tracks), &body)))
}

async fn releases_page(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Html<String>, AppError> {
    let page_number = params.page();
    let all = store::list_local_releases(&state.pool, None).await?;
    let start = usize::try_from((page_number - 1) * PAGE_SIZE as u64).unwrap_or(usize::MAX);
    let end = start.saturating_add(PAGE_SIZE);
    let rows = all
        .get(start.min(all.len())..end.min(all.len()))
        .unwrap_or(&[]);
    let body = render_releases(rows, page_number, all.len() > end);
    Ok(Html(page("Releases", Some(Nav::Releases), &body)))
}

async fn folders_page(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Html<String>, AppError> {
    let page_number = params.page();
    let all = store::list_folder_units(&state.pool, None).await?;
    let start = usize::try_from((page_number - 1) * PAGE_SIZE as u64).unwrap_or(usize::MAX);
    let end = start.saturating_add(PAGE_SIZE);
    let units = all
        .get(start.min(all.len())..end.min(all.len()))
        .unwrap_or(&[]);
    let body = render_folders(units, page_number, all.len() > end);
    Ok(Html(page("Folders", Some(Nav::Folders), &body)))
}

/// The `/folder` query: which unit.
#[derive(Debug, Default, Deserialize)]
struct FolderParams {
    path: Option<String>,
}

async fn folder_page(
    State(state): State<AppState>,
    Query(params): Query<FolderParams>,
) -> Result<Response, AppError> {
    let Some(path) = params
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(folders_error(StatusCode::NOT_FOUND, "No such folder."));
    };
    let Some(view) = store::folder_view(&state.pool, path).await? else {
        return Ok(folders_error(StatusCode::NOT_FOUND, "No such folder."));
    };
    let token = state.writable.then_some(&*state.token);
    Ok(Html(page(
        "Folder",
        Some(Nav::Folders),
        &render_folder(&view, token),
    ))
    .into_response())
}

/// The `403` for a form that is from another site or carries the wrong token.
///
/// A tab left open across a server restart holds the old process's token, so
/// the body says what to do.
fn stale_form() -> Response {
    folders_error(
        StatusCode::FORBIDDEN,
        "Reload the folder page and try again.",
    )
}

/// The folder form's fields.
///
/// A missing field is empty, so a body without the token reaches the token
/// check and gets its `403` rather than a rejection of the body.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FolderForm {
    path: String,
    action: String,
    token: String,
}

/// Records a folder decision from the form, then shows the folder again.
///
/// Refused with `403` off loopback, from another site, or without this
/// process's token; `400` for an unknown action; `404` for a path that names
/// no unit. The checks that need no body come first, so a server that takes no
/// writes answers `403` whatever the body holds, and a bad body gets its own
/// rejection (`413` or `415`) only on a server that could use it. A body serde
/// cannot decode, such as a repeated field (`token=a&token=b`), gets `422`.
async fn folder_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<FolderForm>, FormRejection>,
) -> Result<Response, AppError> {
    if !state.writable {
        return Ok(folders_error(StatusCode::FORBIDDEN, NOT_WRITABLE));
    }
    if cross_site(&headers) {
        return Ok(stale_form());
    }
    let Form(form) = match form {
        Ok(form) => form,
        Err(rejection) => {
            return Ok(folders_error(rejection.status(), &rejection.body_text()));
        }
    };
    if !same_token(&state.token, &form.token) {
        return Ok(stale_form());
    }
    let Some(action) = FolderAction::parse(&form.action) else {
        return Ok(folders_error(StatusCode::BAD_REQUEST, "Unknown action."));
    };
    let path = form.path.trim().trim_end_matches(['/', '\\']);
    let unit = store::resolve_folder(&state.pool, &store::FolderTarget::Path(path.to_string()))
        .await?
        .filter(|unit| unit.eq_ignore_ascii_case(path));
    let Some(unit) = unit else {
        return Ok(folders_error(StatusCode::NOT_FOUND, "No such folder."));
    };
    store::set_folder_mode(&state.pool, &unit, action.mode()).await?;
    Ok(Redirect::to(&folder_href(&unit)).into_response())
}

/// The page a link in the header points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Status,
    Tracks,
    Releases,
    Review,
    Folders,
}

/// Wraps `body` in the document shell with the header navigation.
///
/// `title` is plain text; `body` must already be escaped HTML. `active` marks
/// the current section, if any.
fn page(title: &str, active: Option<Nav>, body: &str) -> String {
    let link = |nav: Nav, href: &str, label: &str| {
        let current = if Some(nav) == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!("<a href=\"{href}\"{current}>{label}</a>")
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} · Radish</title><style>{STYLE}</style></head><body>\
         <header><div class=\"bar\"><b>Radish</b><nav>{}{}{}{}{}</nav></div></header>\
         <main>{body}</main></body></html>\n",
        escape_html(title),
        link(Nav::Status, "/", "Status"),
        link(Nav::Tracks, "/tracks", "Tracks"),
        link(Nav::Releases, "/releases", "Releases"),
        link(Nav::Review, "/reviews", "Review"),
        link(Nav::Folders, "/folders", "Folders"),
    )
}

/// Escapes untrusted text for HTML.
///
/// Control and bidirectional-override characters are escaped first, exactly as
/// on the terminal, then `& < > " '` become entities.
fn escape_html(text: &str) -> String {
    let cleaned = Clean(text).to_string();
    let mut escaped = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Percent-encodes `text` for use as a URL query value.
fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Formats seconds since the epoch as `YYYY-MM-DD HH:MM UTC`.
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let of_day = secs.rem_euclid(86_400);
    // Civil date from a day count (Howard Hinnant's `civil_from_days`): the
    // constants shift the epoch to 0000-03-01 so leap days fall at year end,
    // and 146097 is the number of days in a 400-year cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        of_day / 3_600,
        of_day % 3_600 / 60
    )
}

/// One tile on the status page.
fn stat(label: &str, value: i64) -> String {
    format!("<div><b>{value}</b><span>{label}</span></div>")
}

/// One tile on the status page whose number links to `href`.
fn stat_link(label: &str, value: i64, href: &str) -> String {
    format!(
        "<div><b><a href=\"{}\">{value}</a></b><span>{label}</span></div>",
        escape_html(href)
    )
}

fn render_status(status: &store::LibraryStatus) -> String {
    let tiles = [
        stat("Directories", status.directories.len() as i64),
        stat("Tracks", status.tracks),
        stat("Local releases", status.releases),
        stat("Matched", status.matched),
        stat("Unmatched", status.unmatched),
        stat("Stale", status.stale),
        stat_link("Queued for review", status.queued, "/reviews"),
    ]
    .concat();

    let folders = if status.folders > 0 {
        format!(
            "<p>Folders with several releases, a merged release or a grouping decision: \
             <a href=\"/folders\">{}</a></p>",
            status.folders
        )
    } else {
        String::new()
    };

    let newest = match status.newest_modified {
        Some(secs) => format!(
            "<p class=\"muted\">Newest file change seen: {}</p>",
            format_utc(secs)
        ),
        None => String::new(),
    };

    let directories = if status.directories.is_empty() {
        "<p>Nothing imported yet. Run <code>radish import PATH</code>.</p>".to_string()
    } else {
        let rows: String = status
            .directories
            .iter()
            .map(|dir| {
                format!(
                    "<tr><td class=\"path\">{}</td><td class=\"num\">{}</td></tr>",
                    escape_html(&dir.path),
                    dir.tracks
                )
            })
            .collect();
        format!(
            "<div class=\"table\"><table><thead><tr><th>Directory</th>\
             <th class=\"num\">Tracks</th></tr></thead><tbody>{rows}</tbody></table></div>"
        )
    };

    format!(
        "<h1>Library status</h1><div class=\"stats\">{tiles}</div>{folders}{newest}\
         <h1>Imported directories</h1>{directories}"
    )
}

fn render_tracks(
    rows: &[store::TrackRow],
    query: Option<&str>,
    page_number: u64,
    has_next: bool,
) -> String {
    let form = format!(
        "<form method=\"get\" action=\"/tracks\"><input type=\"search\" name=\"q\" \
         value=\"{}\" maxlength=\"{MAX_QUERY_CHARS}\" \
         placeholder=\"Search title, artist or album\" aria-label=\"Search\">\
         <button>Search</button></form>",
        escape_html(query.unwrap_or_default())
    );

    let table = if rows.is_empty() {
        "<p>No tracks found.</p>".to_string()
    } else {
        let body: String = rows
            .iter()
            .map(|row| {
                format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td>\
                     <td class=\"num\">{}</td><td class=\"muted\">{}</td></tr>",
                    escape_html(row.artist.as_deref().unwrap_or("(unknown artist)")),
                    escape_html(&row.display_title()),
                    escape_html(row.album.as_deref().unwrap_or("-")),
                    track_number(row.disc_number, row.track_number),
                    format_duration(row.duration_secs),
                    if row.stale { "stale match" } else { "" },
                )
            })
            .collect();
        format!(
            "<div class=\"table\"><table><thead><tr><th>Artist</th><th>Title</th>\
             <th>Album</th><th class=\"num\">#</th><th class=\"num\">Time</th><th></th>\
             </tr></thead><tbody>{body}</tbody></table></div>"
        )
    };

    format!(
        "<h1>Tracks</h1>{form}{table}{}",
        pager("/tracks", query, page_number, has_next)
    )
}

fn render_releases(rows: &[store::LocalReleaseStatus], page_number: u64, has_next: bool) -> String {
    if rows.is_empty() {
        return "<h1>Releases</h1><p>No local releases found.</p>".to_string();
    }
    let body: String = rows.iter().map(release_row).collect();
    format!(
        "<h1>Releases</h1><p class=\"muted\">{} on this page.</p>\
         <div class=\"table\"><table><thead>{RELEASE_HEADER}</thead><tbody>{body}</tbody>\
         </table></div>{}",
        plural(rows.len() as u64, "release", "releases"),
        pager("/releases", None, page_number, has_next)
    )
}

/// One release as a table row; its folder links to its unit's page.
fn release_row(row: &store::LocalReleaseStatus) -> String {
    let merged = row
        .merged
        .as_ref()
        .map(|merged| {
            format!(
                "<br><span class=\"muted\">merged: {}</span>",
                escape_html(&merged.label())
            )
        })
        .unwrap_or_default();
    format!(
        "<tr><td class=\"num\"><a href=\"/review?release={}\">{}</a></td><td>{}</td>\
         <td>{}{merged}</td><td class=\"num\">{}</td>\
         <td>{}</td><td>{}</td><td class=\"path\"><a href=\"{}\">{}</a></td></tr>",
        row.id,
        row.id,
        escape_html(row.album_artist.as_deref().unwrap_or("(unknown artist)")),
        escape_html(row.album_title.as_deref().unwrap_or("(no album tag)")),
        row.track_count,
        escape_html(&row.status_label()),
        render_match(row),
        escape_html(&folder_href(&row.unit)),
        escape_html(&row.directory),
    )
}

/// The page of the unit at `unit`.
fn folder_href(unit: &str) -> String {
    format!("/folder?path={}", percent_encode(unit))
}

/// `text` quoted for a POSIX shell, so a shown command can be pasted.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `path` relative to the folder `unit`, or all of it when it is elsewhere.
fn relative_path<'a>(unit: &str, path: &'a str) -> &'a str {
    path.get(..unit.len())
        .filter(|head| head.eq_ignore_ascii_case(unit))
        .and_then(|_| path[unit.len()..].strip_prefix(['/', '\\']))
        .unwrap_or(path)
}

fn render_folders(units: &[store::FolderUnit], page_number: u64, has_next: bool) -> String {
    if units.is_empty() {
        return "<h1>Folders</h1><p>No folder holds several releases, a merged release or a \
                grouping decision.</p>"
            .to_string();
    }
    let body: String = units
        .iter()
        .map(|unit| {
            let tracks: i64 = unit
                .releases
                .iter()
                .map(|release| release.track_count)
                .sum();
            let matched = unit
                .releases
                .iter()
                .filter(|release| release.status == store::STATUS_MATCHED)
                .count();
            format!(
                "<tr><td class=\"path\"><a href=\"{}\">{}</a></td><td>{}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{tracks}</td>\
                 <td class=\"num\">{matched}</td></tr>",
                escape_html(&folder_href(&unit.path)),
                escape_html(&unit.path),
                unit.state.label(),
                unit.releases.len(),
            )
        })
        .collect();
    format!(
        "<h1>Folders</h1><p class=\"muted\">{} on this page.</p>\
         <div class=\"table\"><table><thead><tr><th>Folder</th><th>Grouping</th>\
         <th class=\"num\">Releases</th><th class=\"num\">Tracks</th>\
         <th class=\"num\">Matched</th></tr></thead><tbody>{body}</tbody></table></div>{}",
        plural(units.len() as u64, "folder", "folders"),
        pager("/folders", None, page_number, has_next)
    )
}

/// One unit: its state, the choices, its releases and its files. `token` is
/// `None` on a server that takes no writes, which shows commands instead.
fn render_folder(view: &store::FolderView, token: Option<&str>) -> String {
    let unit = &view.unit;
    let releases: String = unit.releases.iter().map(release_row).collect();
    let files: String = view
        .files
        .iter()
        .map(|file| {
            format!(
                "<tr><td class=\"path\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td>\
                 <td class=\"num\">{}</td></tr>",
                escape_html(relative_path(&unit.path, &file.path)),
                track_number(file.disc, file.track_number),
                escape_html(file.album.as_deref().unwrap_or("(no album tag)")),
                escape_html(file.title.as_deref().unwrap_or("")),
                format_duration(file.duration_secs),
            )
        })
        .collect();
    format!(
        "<h1>Folder</h1><p class=\"path\">{}</p><p>Grouping: {}</p>{}\
         <h1>Releases</h1><div class=\"table\"><table><thead>{RELEASE_HEADER}</thead>\
         <tbody>{releases}</tbody></table></div>\
         <h1>Files</h1><div class=\"table\"><table><thead><tr><th>File</th>\
         <th class=\"num\">#</th><th>Album tag</th><th>Title</th><th class=\"num\">Time</th>\
         </tr></thead><tbody>{files}</tbody></table></div>",
        escape_html(&unit.path),
        unit.state.label(),
        render_choices(view, token),
    )
}

/// The decision buttons, or the equivalent commands, with their effects.
fn render_choices(view: &store::FolderView, token: Option<&str>) -> String {
    if view.choices.is_empty() {
        return String::new();
    }
    let notes: String = view
        .choices
        .iter()
        .filter_map(|(action, effects)| {
            effects
                .describe()
                .map(|text| format!("<li>{}: {text}.</li>", action.label()))
        })
        .collect();
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!("<ul class=\"muted\">{notes}</ul>")
    };
    match token {
        Some(token) => {
            let buttons: String = view
                .choices
                .iter()
                .map(|(action, _)| {
                    format!(
                        "<button name=\"action\" value=\"{}\">{}</button>",
                        action.as_str(),
                        action.label()
                    )
                })
                .collect();
            format!(
                "<form method=\"post\" action=\"/folder\">\
                 <input type=\"hidden\" name=\"path\" value=\"{}\">\
                 <input type=\"hidden\" name=\"token\" value=\"{}\">{buttons}</form>{notes}",
                escape_html(&view.unit.path),
                escape_html(token)
            )
        }
        None => {
            let commands: String = view
                .choices
                .iter()
                .map(|(action, _)| {
                    format!(
                        "<li><code>radish folder {} {}</code></li>",
                        action.cli_verb(),
                        escape_html(&shell_quote(&view.unit.path))
                    )
                })
                .collect();
            format!(
                "<p class=\"muted\">This server listens on a non-loopback address, so it \
                 changes nothing. Run one of these with the same <code>--db</code> as this \
                 server:</p><ul>{commands}</ul>{notes}"
            )
        }
    }
}

/// The accepted match as provider, release ID and title, linked when the
/// provider URL is plain `http` or `https`.
fn render_match(row: &store::LocalReleaseStatus) -> String {
    let (Some(provider), Some(id)) = (&row.provider, &row.remote_release_id) else {
        return String::new();
    };
    let id = escape_html(id);
    let id = match row.remote_url.as_deref().filter(|url| is_web_url(url)) {
        Some(url) => format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">{id}</a>",
            escape_html(url)
        ),
        None => id,
    };
    // The title, then the disc when the files are one disc of the release.
    let mut detail: Vec<String> = row
        .remote_title
        .as_deref()
        .map(escape_html)
        .into_iter()
        .collect();
    detail.extend(row.medium.map(|medium| format!("disc {medium}")));
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({})", detail.join(", "))
    };
    format!("{provider} {id}{detail}")
}

/// Whether `url` starts with an `http` or `https` scheme.
///
/// Anything else, such as `javascript:`, is never turned into a link.
fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// Previous and next links for a paged page, or nothing when there is one page.
///
/// The search text is carried through both links.
fn pager(path: &str, query: Option<&str>, page_number: u64, has_next: bool) -> String {
    let params: Vec<(&str, &str)> = query.map(|query| ("q", query)).into_iter().collect();
    pager_with(path, &params, page_number, has_next)
}

/// Previous and next links that carry the given query parameters.
fn pager_with(path: &str, params: &[(&str, &str)], page_number: u64, has_next: bool) -> String {
    if page_number == 1 && !has_next {
        return String::new();
    }
    let href = |target: u64| {
        let mut parts: Vec<String> = params
            .iter()
            .map(|(key, value)| format!("{key}={}", percent_encode(value)))
            .collect();
        if target > 1 {
            parts.push(format!("page={target}"));
        }
        let href = if parts.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", parts.join("&"))
        };
        escape_html(&href)
    };
    let previous = if page_number > 1 {
        format!("<a href=\"{}\">Previous</a>", href(page_number - 1))
    } else {
        "<span>Previous</span>".to_string()
    };
    let next = if has_next {
        format!("<a href=\"{}\">Next</a>", href(page_number + 1))
    } else {
        "<span>Next</span>".to_string()
    };
    format!("<div class=\"pager\">{previous}<span>Page {page_number}</span>{next}</div>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn escape_html_neutralizes_markup_and_terminal_tricks() {
        assert_eq!(
            escape_html("<script>alert('x' & \"y\")</script>"),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
        let shown = escape_html("a\x1b[31mb\u{202e}c");
        assert!(!shown.chars().any(|c| c.is_control() || c == '\u{202e}'));
        assert!(shown.contains("\\u{1b}"), "{shown}");
        assert_eq!(escape_html("Sigur Rós"), "Sigur Rós");
    }

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("blue-moon_1.~"), "blue-moon_1.~");
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("Rós"), "R%C3%B3s");
    }

    #[test]
    fn host_allowed_accepts_only_this_server() {
        let loopback = addr("127.0.0.1:8787");
        for host in [
            "localhost",
            "LOCALHOST:8787",
            "127.0.0.1:8787",
            "127.0.0.2",
            "[::1]:8787",
        ] {
            assert!(host_allowed(loopback, host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8787",
            "127.0.0.1.evil.example",
            "192.168.1.5:8787",
            "",
        ] {
            assert!(!host_allowed(loopback, host), "{host}");
        }

        // A specific LAN bind also accepts its own address, but no other.
        let lan = addr("192.168.1.5:8787");
        assert!(host_allowed(lan, "192.168.1.5:8787"));
        assert!(!host_allowed(lan, "192.168.1.6"));
        // A wildcard bind cannot know its names.
        assert!(host_allowed(addr("0.0.0.0:8787"), "nas.local"));
    }

    #[test]
    fn list_params_fall_back_instead_of_failing() {
        let params = |q: Option<&str>, page: Option<&str>| ListParams {
            q: q.map(str::to_string),
            page: page.map(str::to_string),
        };
        assert_eq!(params(None, None).page(), 1);
        assert_eq!(params(None, Some("3")).page(), 3);
        assert_eq!(params(None, Some("0")).page(), 1);
        assert_eq!(params(None, Some("abc")).page(), 1);
        assert_eq!(params(None, Some("-4")).page(), 1);
        assert_eq!(params(None, Some("99999999999999999999")).page(), 1);
        assert_eq!(params(None, Some("5000000")).page(), MAX_PAGE);

        assert_eq!(
            params(Some("  blue "), None).query().as_deref(),
            Some("blue")
        );
        assert_eq!(params(Some("   "), None).query(), None);
        let long = "x".repeat(MAX_QUERY_CHARS + 50);
        assert_eq!(
            params(Some(&long), None).query().unwrap().chars().count(),
            MAX_QUERY_CHARS
        );
    }

    #[test]
    fn format_utc_handles_the_epoch_and_leap_days() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_utc(-1), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn pager_links_keep_the_search_and_stop_at_the_ends() {
        assert_eq!(pager("/tracks", None, 1, false), "");

        let first = pager("/tracks", Some("a&b \"c\""), 1, true);
        assert!(first.contains("<span>Previous</span>"), "{first}");
        assert!(
            first.contains("href=\"/tracks?q=a%26b%20%22c%22&amp;page=2\""),
            "{first}"
        );

        let middle = pager("/releases", None, 2, true);
        assert!(middle.contains("href=\"/releases\">Previous"), "{middle}");
        assert!(
            middle.contains("href=\"/releases?page=3\">Next"),
            "{middle}"
        );

        let last = pager("/tracks", Some("blue"), 3, false);
        assert!(
            last.contains("href=\"/tracks?q=blue&amp;page=2\">Previous"),
            "{last}"
        );
        assert!(last.contains("<span>Next</span>"), "{last}");
    }

    #[test]
    fn only_http_urls_become_links() {
        assert!(is_web_url("https://musicbrainz.org/release/1"));
        assert!(is_web_url("HTTP://example.org"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("data:text/html,x"));
        assert!(!is_web_url(""));
    }

    fn release(id: i64) -> store::LocalReleaseStatus {
        store::LocalReleaseStatus {
            id,
            directory: "/music/<b>".into(),
            unit: "/music/<b>".into(),
            album_title: Some("Blue".into()),
            album_artist: Some("Test Artist".into()),
            track_count: 2,
            status: store::STATUS_MATCHED.into(),
            provider: Some(crate::providers::ProviderKind::MusicBrainz),
            remote_release_id: Some("76df3287-6cda-33eb-8e9a-044b5e15ffdd".into()),
            remote_url: Some("javascript:alert(1)".into()),
            remote_title: Some("<i>Blue</i>".into()),
            match_scope: None,
            medium: None,
            stale: false,
            queued_providers: Vec::new(),
            merged: None,
        }
    }

    #[test]
    fn release_rows_escape_values_and_skip_unsafe_links() {
        let html = render_releases(&[release(12)], 1, false);
        assert!(html.contains("/music/&lt;b&gt;"), "{html}");
        assert!(html.contains("(&lt;i&gt;Blue&lt;/i&gt;)"), "{html}");
        assert!(!html.contains("<i>"), "{html}");
        assert!(!html.contains("href=\"javascript"), "{html}");
        assert!(html.contains("1 release on this page."), "{html}");

        let mut linked = release(13);
        linked.remote_url = Some("https://musicbrainz.org/release/x?a=1&b=2".into());
        let html = render_releases(&[linked], 1, false);
        assert!(
            html.contains("href=\"https://musicbrainz.org/release/x?a=1&amp;b=2\""),
            "{html}"
        );

        assert!(render_releases(&[], 1, false).contains("No local releases found."));
    }

    #[test]
    fn a_disc_match_shows_its_disc() {
        let mut row = release(12);
        row.medium = Some(2);
        let html = render_match(&row);
        assert!(
            html.ends_with("(&lt;i&gt;Blue&lt;/i&gt;, disc 2)"),
            "{html}"
        );
        row.remote_title = None;
        assert!(render_match(&row).ends_with("(disc 2)"));
        row.medium = None;
        assert!(!render_match(&row).contains("disc"));
    }

    #[test]
    fn track_rows_use_cli_fallbacks_and_escape_values() {
        let row = store::TrackRow {
            path: "/m/<x>.wav".into(),
            title: None,
            artist: None,
            album: None,
            track_number: Some(1),
            disc_number: Some(2),
            duration_secs: Some(65.0),
            stale: true,
        };
        let html = render_tracks(&[row], Some("<q>"), 1, false);
        assert!(html.contains("(unknown artist)"), "{html}");
        assert!(html.contains("&lt;x&gt;.wav"), "{html}");
        assert!(html.contains("2-1"), "{html}");
        assert!(html.contains("1:05"), "{html}");
        assert!(html.contains("stale match"), "{html}");
        assert!(html.contains("value=\"&lt;q&gt;\""), "{html}");
        assert!(!html.contains("<x>") && !html.contains("<q>"), "{html}");

        assert!(render_tracks(&[], None, 1, false).contains("No tracks found."));
    }

    #[test]
    fn the_header_bar_sits_inside_a_full_width_header() {
        let html = page("Status", Some(Nav::Status), "");
        assert!(
            html.contains("<header><div class=\"bar\"><b>Radish</b><nav>"),
            "{html}"
        );
    }

    #[test]
    fn the_title_is_escaped() {
        let html = page("<b>x</b>", None, "");
        assert!(
            html.contains("<title>&lt;b&gt;x&lt;/b&gt; · Radish</title>"),
            "{html}"
        );
        assert!(!html.contains("aria-current=\"page\""), "{html}");
    }

    #[test]
    fn the_stylesheet_wraps_the_nav_has_focus_rings_and_no_break_all() {
        assert!(STYLE.contains("nav{display:flex;flex-wrap:wrap"), "{STYLE}");
        assert!(!STYLE.contains("break-all"), "{STYLE}");
        assert!(STYLE.contains(":focus-visible"), "{STYLE}");
        assert!(STYLE.contains("scrollbar-gutter:stable"), "{STYLE}");
    }

    #[test]
    fn every_page_links_the_review_queue() {
        let html = page("Status", Some(Nav::Status), "");
        assert!(
            html.contains("<a href=\"/releases\">Releases</a><a href=\"/reviews\">Review</a>"),
            "{html}"
        );
    }

    #[test]
    fn release_ids_link_to_their_review_page() {
        let html = render_releases(&[release(12)], 1, false);
        assert!(
            html.contains("<a href=\"/review?release=12\">12</a>"),
            "{html}"
        );
    }

    #[test]
    fn the_pager_carries_its_parameters() {
        let html = pager_with("/reviews", &[("provider", "deezer")], 2, true);
        assert!(html.contains("href=\"/reviews?provider=deezer\""), "{html}");
        assert!(
            html.contains("href=\"/reviews?provider=deezer&amp;page=3\""),
            "{html}"
        );
        assert_eq!(
            pager("/tracks", Some("a b"), 1, true),
            pager_with("/tracks", &[("q", "a b")], 1, true)
        );
        let html = pager_with(
            "/reviews",
            &[("sort", "score"), ("show", "acceptable")],
            2,
            true,
        );
        assert!(
            html.contains("href=\"/reviews?sort=score&amp;show=acceptable\""),
            "{html}"
        );
        assert!(
            html.contains("href=\"/reviews?sort=score&amp;show=acceptable&amp;page=3\""),
            "{html}"
        );
        assert!(pager_with("/reviews", &[], 2, true).contains("href=\"/reviews\""));
    }

    #[test]
    fn status_page_shows_counts_and_an_empty_hint() {
        let empty = render_status(&store::LibraryStatus::default());
        assert!(empty.contains("Nothing imported yet"), "{empty}");

        let status = store::LibraryStatus {
            directories: vec![store::DirectorySummary {
                path: "/music/<a>".into(),
                tracks: 7,
            }],
            tracks: 7,
            releases: 2,
            matched: 1,
            unmatched: 1,
            stale: 0,
            folders: 3,
            queued: 1,
            newest_modified: Some(1_700_000_000),
        };
        let html = render_status(&status);
        assert!(html.contains("/music/&lt;a&gt;"), "{html}");
        assert!(html.contains("<b>7</b><span>Tracks</span>"), "{html}");
        assert!(
            html.contains("<b><a href=\"/reviews\">1</a></b><span>Queued for review</span>"),
            "{html}"
        );
        assert!(html.contains("2023-11-14 22:13 UTC"), "{html}");
        assert!(html.contains("<a href=\"/folders\">3</a>"), "{html}");
    }

    #[test]
    fn tokens_are_random_hex_and_compare_exactly() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(same_token(&a, &a.clone()));
        assert!(!same_token(&a, &b));
        assert!(!same_token(&a, ""));
    }

    #[test]
    fn only_same_origin_or_silent_browsers_may_post() {
        let mut headers = HeaderMap::new();
        assert!(!cross_site(&headers));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!cross_site(&headers));
        for value in ["cross-site", "same-site", "none"] {
            headers.insert("sec-fetch-site", HeaderValue::from_static(value));
            assert!(cross_site(&headers), "{value}");
        }
    }

    #[test]
    fn only_a_loopback_server_takes_writes() {
        assert!(writes_allowed(addr("127.0.0.1:8787")));
        assert!(writes_allowed(addr("[::1]:8787")));
        assert!(!writes_allowed(addr("0.0.0.0:8787")));
        assert!(!writes_allowed(addr("192.168.1.5:8787")));
    }

    #[tokio::test]
    async fn a_server_off_loopback_refuses_every_post() {
        let state = AppState {
            pool: crate::db::connect_in_memory().await.unwrap(),
            bind: addr("0.0.0.0:8787"),
            token: "t".into(),
            writable: false,
            fetcher: Arc::new(crate::review::Fetcher::new(crate::review::Source::Network)),
        };
        let form = FolderForm {
            path: "/m/x".into(),
            action: "merge".into(),
            token: "t".into(),
        };
        let Ok(response) = folder_action(State(state), HeaderMap::new(), Ok(Form(form))).await
        else {
            panic!("the handler failed");
        };
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_internal_error_page_keeps_the_header_and_hides_the_cause() {
        let response = AppError(anyhow::anyhow!("/secret/db <path>")).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("<nav>"), "{html}");
        assert!(
            html.contains("Internal error; the server log says why."),
            "{html}"
        );
        assert!(!html.contains("secret"), "{html}");
    }

    #[tokio::test]
    async fn an_error_page_keeps_the_header_and_escapes_its_message() {
        let response = error_page(
            StatusCode::NOT_FOUND,
            None,
            "no <b>such</b> folder",
            ("/folders", "Back to the folders"),
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("<nav>"), "{html}");
        assert!(html.contains("<h1>Not Found</h1>"), "{html}");
        assert!(html.contains("no &lt;b&gt;such&lt;/b&gt; folder"), "{html}");
        assert!(
            html.contains("<a href=\"/folders\">Back to the folders</a>"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn a_stale_token_or_another_site_is_told_to_reload() {
        let state = AppState {
            pool: crate::db::connect_in_memory().await.unwrap(),
            bind: addr("127.0.0.1:8787"),
            token: "t".into(),
            writable: true,
            fetcher: Arc::new(crate::review::Fetcher::new(crate::review::Source::Network)),
        };
        let form = |token: &str| {
            Ok(Form(FolderForm {
                path: "/m/x".into(),
                action: "merge".into(),
                token: token.into(),
            }))
        };
        let mut foreign = HeaderMap::new();
        foreign.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        for (headers, token) in [(foreign, "t"), (HeaderMap::new(), "old")] {
            let Ok(response) = folder_action(State(state.clone()), headers, form(token)).await
            else {
                panic!("the handler failed");
            };
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                html.contains("Reload the folder page and try again"),
                "{html}"
            );
        }
    }

    fn folder_view() -> store::FolderView {
        let mut first = release(12);
        first.unit = "/music/<b>".into();
        store::FolderView {
            unit: store::FolderUnit {
                path: "/music/<b>".into(),
                state: store::FolderState::Automatic,
                releases: vec![first],
            },
            files: vec![store::FolderFile {
                path: "/music/<b>/cd2/01.wav".into(),
                disc: Some(2),
                track_number: Some(1),
                album: Some("<i>Blue</i>".into()),
                title: None,
                duration_secs: Some(61.0),
            }],
            choices: vec![
                (
                    FolderAction::Merge,
                    store::MatchEffects {
                        stale: 1,
                        dropped: 0,
                    },
                ),
                (FolderAction::Separate, store::MatchEffects::default()),
            ],
        }
    }

    #[test]
    fn a_loopback_folder_page_has_a_form_with_the_token() {
        let html = render_folder(&folder_view(), Some("abc123"));
        assert!(
            html.contains("<form method=\"post\" action=\"/folder\">"),
            "{html}"
        );
        assert!(html.contains("name=\"token\" value=\"abc123\""), "{html}");
        assert!(
            html.contains("name=\"path\" value=\"/music/&lt;b&gt;\""),
            "{html}"
        );
        assert!(
            html.contains("value=\"merge\">Merge into one release</button>"),
            "{html}"
        );
        assert!(
            html.contains("value=\"separate\">Keep separate</button>"),
            "{html}"
        );
        assert!(
            html.contains("Merge into one release: 1 accepted match becomes stale."),
            "{html}"
        );
        assert!(html.contains("cd2/01.wav"), "{html}");
        assert!(html.contains("&lt;i&gt;Blue&lt;/i&gt;"), "{html}");
        assert!(!html.contains("<i>"), "{html}");
    }

    #[test]
    fn a_folder_page_off_loopback_shows_commands_instead() {
        let html = render_folder(&folder_view(), None);
        assert!(!html.contains("<form"), "{html}");
        assert!(
            html.contains("radish folder merge &#39;/music/&lt;b&gt;&#39;"),
            "{html}"
        );
        assert!(html.contains("radish folder split"), "{html}");
    }

    #[test]
    fn release_rows_link_their_folder() {
        let html = render_releases(&[release(12)], 1, false);
        assert!(
            html.contains("href=\"/folder?path=%2Fmusic%2F%3Cb%3E\""),
            "{html}"
        );
    }

    #[test]
    fn the_folders_page_lists_units_and_their_state() {
        let mut first = release(12);
        first.unit = "/music/<b>".into();
        let unit = store::FolderUnit {
            path: "/music/<b>".into(),
            state: store::FolderState::MergedByUser,
            releases: vec![first],
        };
        let html = render_folders(&[unit], 1, false);
        assert!(html.contains("merged by you"), "{html}");
        assert!(html.contains("/music/&lt;b&gt;</a>"), "{html}");
        assert!(render_folders(&[], 1, false).contains("No folder holds"));
    }
}
