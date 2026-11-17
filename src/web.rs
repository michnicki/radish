//! A small read-only web interface over the library database.
//!
//! `radish serve` shows three server-rendered HTML pages: **Status** (what the
//! library holds), **Tracks** (the `list` and `search` data) and **Releases**
//! (the `releases` data). Every request is a `GET`, nothing is written, and no
//! JavaScript is served.
//!
//! The server has no authentication, so safety rests on its defaults:
//!
//! - It binds to loopback unless told otherwise.
//! - A request whose `Host` header is not a loopback name or the bind address
//!   gets `403`, which stops DNS-rebinding pages from reading the library.
//! - Tags, paths and provider titles are untrusted. Each value goes through
//!   [`Clean`](crate::output::Clean) and then HTML escaping before it is
//!   written into a page.
//! - Every response carries a strict `Content-Security-Policy`.

// Rust guideline compliant 2026-08-14

use crate::layout::{format_duration, plural, track_number};
use crate::output::Clean;
use crate::{outln, store};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::net::{IpAddr, SocketAddr};
use tokio::net::TcpListener;

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

/// What the browser may load: nothing, except this page's own inline style.
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'";

/// The page stylesheet, inlined so the server never serves a second file.
const STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fff;--fg:#1c1c1e;--muted:#6b6b70;--line:#e2e2e6;--accent:#2f6f3e;--card:#f6f6f8}\
@media(prefers-color-scheme:dark){:root{--bg:#161618;--fg:#e8e8ea;--muted:#9a9aa0;--line:#2e2e33;--accent:#7bc98d;--card:#1f1f23}}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}\
header,main{max-width:72rem;margin:0 auto;padding:0 1rem}\
header{display:flex;gap:1.5rem;align-items:baseline;border-bottom:1px solid var(--line);padding-top:.75rem;padding-bottom:.5rem}\
header b{color:var(--accent)}\
nav a{color:var(--muted);text-decoration:none;margin-right:1rem}\
nav a[aria-current]{color:var(--fg);font-weight:600}\
h1{font-size:1.25rem;margin:1.25rem 0 .75rem}\
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(9rem,1fr));gap:.75rem;margin:0 0 1.5rem}\
.stats div{background:var(--card);border-radius:.5rem;padding:.6rem .8rem}\
.stats b{display:block;font-size:1.4rem}\
.stats span,.muted{color:var(--muted);font-size:.85rem}\
.table{overflow-x:auto}\
table{border-collapse:collapse;width:100%}\
th,td{text-align:left;padding:.35rem .6rem;border-bottom:1px solid var(--line);vertical-align:top}\
th{color:var(--muted);font-size:.8rem;text-transform:uppercase}\
.num{text-align:right;font-variant-numeric:tabular-nums}\
.path{color:var(--muted);word-break:break-all}\
form{display:flex;gap:.5rem;margin-bottom:1rem}\
input[type=search]{flex:1;padding:.4rem .6rem;font:inherit;color:inherit;background:var(--card);border:1px solid var(--line);border-radius:.4rem}\
button{font:inherit;padding:.4rem .9rem;border:0;border-radius:.4rem;background:var(--accent);color:var(--bg)}\
.pager{display:flex;gap:1rem;margin:1rem 0;color:var(--muted)}\
a{color:var(--accent)}";

/// Shared state for the handlers and the request guard.
#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    /// The address actually bound, which decides which `Host` values are fine.
    bind: SocketAddr,
}

/// Serves the web interface on `bind` until Ctrl+C.
///
/// Prints `Serving http://<address>` once listening, with the real port when
/// `bind` asked for port 0. A non-loopback `bind` is allowed but warns on
/// stderr, because the pages show library paths to anyone who can connect.
///
/// # Errors
///
/// Returns an error when the address cannot be bound or the server fails.
pub async fn serve(pool: SqlitePool, bind: SocketAddr) -> Result<()> {
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

    outln!("Serving http://{bind} (Ctrl+C to stop)");
    axum::serve(listener, router(pool, bind))
        .with_graceful_shutdown(async {
            // A failed signal handler leaves Ctrl+C to the default action.
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("web server failed")
}

fn router(pool: SqlitePool, bind: SocketAddr) -> Router {
    let state = AppState { pool, bind };
    Router::new()
        .route("/", get(status_page))
        .route("/tracks", get(tracks_page))
        .route("/releases", get(releases_page))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
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

/// A handler failure: logged on stderr, reported to the browser without detail.
struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("error: {}", Clean(&format!("{:#}", self.0)));
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
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
        self.page
            .as_deref()
            .and_then(|page| page.trim().parse::<u64>().ok())
            .map_or(1, |page| page.clamp(1, MAX_PAGE))
    }

    /// The trimmed search text, or `None` when empty.
    fn query(&self) -> Option<String> {
        let query = self.q.as_deref()?.trim();
        let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
        (!query.is_empty()).then_some(query)
    }
}

async fn status_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let status = store::library_status(&state.pool).await?;
    Ok(Html(page("Status", Nav::Status, &render_status(&status))))
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
    Ok(Html(page("Tracks", Nav::Tracks, &body)))
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
    Ok(Html(page("Releases", Nav::Releases, &body)))
}

/// The page a link in the header points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Status,
    Tracks,
    Releases,
}

/// Wraps `body` in the document shell with the header navigation.
///
/// `body` must already be escaped HTML.
fn page(title: &str, active: Nav, body: &str) -> String {
    let link = |nav: Nav, href: &str, label: &str| {
        let current = if nav == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!("<a href=\"{href}\"{current}>{label}</a>")
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title} · Radish</title><style>{STYLE}</style></head><body>\
         <header><b>Radish</b><nav>{}{}{}</nav></header><main>{body}</main></body></html>\n",
        link(Nav::Status, "/", "Status"),
        link(Nav::Tracks, "/tracks", "Tracks"),
        link(Nav::Releases, "/releases", "Releases"),
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

fn render_status(status: &store::LibraryStatus) -> String {
    let tiles = [
        stat("Directories", status.directories.len() as i64),
        stat("Tracks", status.tracks),
        stat("Local releases", status.releases),
        stat("Matched", status.matched),
        stat("Unmatched", status.unmatched),
        stat("Stale", status.stale),
        stat("Queued for review", status.queued),
    ]
    .concat();

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
        "<h1>Library status</h1><div class=\"stats\">{tiles}</div>{newest}\
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
    let body: String = rows
        .iter()
        .map(|row| {
            format!(
                "<tr><td class=\"num\">{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td>\
                 <td>{}</td><td>{}</td><td class=\"path\">{}</td></tr>",
                row.id,
                escape_html(row.album_artist.as_deref().unwrap_or("(unknown artist)")),
                escape_html(row.album_title.as_deref().unwrap_or("(no album tag)")),
                row.track_count,
                escape_html(&row.status_label()),
                render_match(row),
                escape_html(&row.directory),
            )
        })
        .collect();
    format!(
        "<h1>Releases</h1><p class=\"muted\">{} on this page.</p>\
         <div class=\"table\"><table><thead><tr><th class=\"num\">ID</th><th>Artist</th>\
         <th>Album</th><th class=\"num\">Tracks</th><th>Status</th><th>Match</th>\
         <th>Folder</th></tr></thead><tbody>{body}</tbody></table></div>{}",
        plural(rows.len() as u64, "release", "releases"),
        pager("/releases", None, page_number, has_next)
    )
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
    if page_number == 1 && !has_next {
        return String::new();
    }
    let href = |target: u64| {
        let mut href = path.to_string();
        let mut separator = '?';
        if let Some(query) = query {
            href.push_str(&format!("?q={}", percent_encode(query)));
            separator = '&';
        }
        if target > 1 {
            href.push_str(&format!("{separator}page={target}"));
        }
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
            folders: 0,
            queued: 1,
            newest_modified: Some(1_700_000_000),
        };
        let html = render_status(&status);
        assert!(html.contains("/music/&lt;a&gt;"), "{html}");
        assert!(html.contains("<b>7</b><span>Tracks</span>"), "{html}");
        assert!(
            html.contains("<b>1</b><span>Queued for review</span>"),
            "{html}"
        );
        assert!(html.contains("2023-11-14 22:13 UTC"), "{html}");
    }
}
