//! An OpenSubsonic API over the library for music players: `radish subsonic`.
//!
//! Subsonic clients (Symfonium, Feishin, DSub and others) browse, search and
//! stream through `/rest/<method>`, also spelled `/rest/<method>.view`, by GET
//! or by a form POST (the `formPost` extension). Albums are local releases
//! and artists are album artists, with the effective metadata that `list` and
//! `search` show. Files are streamed as they are and never transcoded. The
//! server keeps no per-user state: scrobbles are accepted and dropped, and
//! favourites, ratings and playlists are refused.
//!
//! Every method except `getOpenSubsonicExtensions` needs the configured user's
//! credentials, as a password or as a salted MD5 token. Neither is encrypted
//! on the wire, so the server belongs on a trusted network or a VPN.

mod auth;
mod response;

pub use auth::Credentials;

use crate::outln;
use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use response::{ApiError, Format, Payload};
use serde_json::json;
use sqlx::SqlitePool;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

/// The largest form body accepted, in bytes. A POST carries the same few
/// parameters a query string would.
const MAX_FORM_BYTES: usize = 16 * 1024;

/// The parameters of one request: the query string, then a form body's.
#[derive(Default)]
pub(crate) struct Params(Vec<(String, String)>);

/// The parameters that carry a credential: `Params`' `Debug` hides their values.
const SECRET_PARAMS: [&str; 4] = ["p", "t", "s", "apiKey"];

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for (key, value) in &self.0 {
            if SECRET_PARAMS.contains(&key.as_str()) {
                list.entry(&(key, "<redacted>"));
            } else {
                list.entry(&(key, value));
            }
        }
        list.finish()
    }
}

impl Params {
    /// Reads `query`, then `body` when the request is a form POST.
    pub(crate) fn parse(query: Option<&str>, body: Option<&[u8]>) -> Self {
        let mut pairs: Vec<(String, String)> = query
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(body) = body {
            pairs.extend(form_urlencoded::parse(body).into_owned());
        }
        Self(pairs)
    }

    /// The first value of `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The first value of `name`, which the method needs.
    pub(crate) fn required(&self, name: &str) -> Result<&str, ApiError> {
        self.get(name).ok_or_else(|| ApiError::missing(name))
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        )
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes of a hex string of either case; `None` when it is not hex.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

#[derive(Clone)]
pub(crate) struct AppState {
    // Nothing reads it until Task 6's library methods; remove the allow then.
    #[allow(dead_code)]
    pool: SqlitePool,
    credentials: Arc<Credentials>,
}

/// Serves the API on `bind` until Ctrl+C.
///
/// # Errors
///
/// Returns an error when the address can't be bound or the server fails.
pub async fn serve(pool: SqlitePool, bind: SocketAddr, credentials: Credentials) -> Result<()> {
    if !bind.ip().is_loopback() {
        eprintln!(
            "warning: {bind} is not a loopback address; Subsonic clients send the password, \
             or a token made from it, unencrypted, so use it only on a trusted network or a VPN"
        );
    }
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    let bind = listener.local_addr()?;
    outln!("Serving http://{bind} (Subsonic API; Ctrl+C to stop)");
    axum::serve(
        listener,
        router(AppState {
            pool,
            credentials: Arc::new(credentials),
        }),
    )
    .with_graceful_shutdown(async {
        // A failed signal handler leaves Ctrl+C to the default action.
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .context("Subsonic server failed")
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/rest/{method}", get(rest).post(rest))
        .layer(DefaultBodyLimit::max(MAX_FORM_BYTES))
        .fallback(not_found)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Every `/rest/` request. A form POST's body adds to the query string.
async fn rest(
    State(state): State<AppState>,
    Path(method): Path<String>,
    http_method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = http_method == Method::POST && is_form(&headers);
    let params = Params::parse(query.as_deref(), form.then_some(&body[..]));
    dispatch(&state, &method, &params, &headers).await
}

/// Whether the request's `Content-Type` is a URL-encoded form. The media type
/// is compared without regard to case and without its parameters.
fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| {
            media_type
                .trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
}

/// Adds the security headers to every response. A file or picture opened in a
/// browser is never sniffed as markup, and any script in it can't run.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// What a method returns: fields for the envelope. (Task 8 adds files.)
enum Answer {
    Payload(Payload),
}

/// Answers one API call. `method` may end in `.view`.
async fn dispatch(
    state: &AppState,
    method: &str,
    params: &Params,
    headers: &HeaderMap,
) -> Response {
    let format = Format::of(params);
    let method = method.strip_suffix(".view").unwrap_or(method);
    // The extensions are public, so a client can learn them before logging in.
    if method != "getOpenSubsonicExtensions"
        && let Err(error) = state.credentials.authenticate(params)
    {
        return response::failed(format, &error);
    }
    match call(state, method, params, headers).await {
        Ok(Answer::Payload(payload)) => response::ok(format, payload),
        Err(error) => response::failed(format, &error),
    }
}

async fn call(
    _state: &AppState,
    method: &str,
    _params: &Params,
    _headers: &HeaderMap,
) -> Result<Answer, ApiError> {
    let payload = match method {
        "ping" => Payload::empty(),
        "getLicense" => Payload::new("license", json!({"valid": true})),
        "getOpenSubsonicExtensions" => Payload::new(
            "openSubsonicExtensions",
            json!([{"name": "formPost", "versions": [1]}]),
        ),
        other => return Err(ApiError::unsupported(other)),
    };
    Ok(Answer::Payload(payload))
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use serde_json::Value;

    /// A server over an empty in-memory library whose user is `u`, password `p`.
    pub(crate) async fn state() -> AppState {
        AppState {
            pool: crate::db::connect_in_memory().await.unwrap(),
            credentials: Arc::new(Credentials::new(Some("u".into()), Some("p".into())).unwrap()),
        }
    }

    /// Calls `method` as the user with `f=json` and `pairs`; returns the envelope.
    pub(crate) async fn call_json(state: &AppState, method: &str, pairs: &[(&str, &str)]) -> Value {
        call_as(
            state,
            method,
            &[&[("u", "u"), ("p", "p")][..], pairs].concat(),
        )
        .await
    }

    /// Calls `method` with `f=json` and exactly `pairs`; returns the envelope.
    pub(crate) async fn call_as(state: &AppState, method: &str, pairs: &[(&str, &str)]) -> Value {
        let pairs = [&[("f", "json")][..], pairs].concat();
        let response = send(state, method, &pairs, &HeaderMap::new()).await;
        let bytes = axum::body::to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        value["subsonic-response"].clone()
    }

    async fn send(
        state: &AppState,
        method: &str,
        pairs: &[(&str, &str)],
        headers: &HeaderMap,
    ) -> Response {
        dispatch(state, method, &Params::from_pairs(pairs), headers).await
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{call_json, state};
    use super::*;

    #[test]
    fn params_read_the_query_then_the_form_body_and_the_first_value_wins() {
        let params = Params::parse(Some("u=joe&id=1&q=a%20b"), Some(&b"id=2&p=sesame&q=x"[..]));
        assert_eq!(params.get("u"), Some("joe"));
        assert_eq!(params.get("id"), Some("1"));
        assert_eq!(params.get("p"), Some("sesame"));
        assert_eq!(params.get("q"), Some("a b"));
        assert_eq!(params.get("nope"), None);
        assert_eq!(params.required("nope"), Err(ApiError::missing("nope")));
    }

    #[test]
    fn params_debug_hides_credentials() {
        let params = Params::from_pairs(&[
            ("u", "joe"),
            ("p", "sesame"),
            ("t", "tok3n"),
            ("s", "salt42"),
            ("apiKey", "k3y"),
            ("id", "tr-1"),
        ]);
        let text = format!("{params:?}");
        for secret in ["sesame", "tok3n", "salt42", "k3y"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert_eq!(text.matches("<redacted>").count(), 4, "{text}");
        assert!(text.contains("joe") && text.contains("tr-1"), "{text}");
    }

    #[test]
    fn form_content_types_match_in_any_case_with_parameters() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert!(is_form(&with("application/x-www-form-urlencoded")));
        assert!(is_form(&with(
            "Application/X-WWW-Form-Urlencoded; charset=UTF-8"
        )));
        assert!(is_form(&with(
            " application/x-www-form-urlencoded ;charset=utf-8"
        )));
        assert!(!is_form(&with("text/plain")));
        assert!(!is_form(&with("application/x-www-form-urlencoded-x")));
        assert!(!is_form(&HeaderMap::new()));
    }

    #[test]
    fn hex_round_trips_and_rejects_anything_else() {
        assert_eq!(hex("Björk".as_bytes()), "426ac3b6726b");
        assert_eq!(decode_hex("426AC3B6726B").unwrap(), "Björk".as_bytes());
        assert_eq!(decode_hex(""), Some(Vec::new()));
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("+f"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[tokio::test]
    async fn system_methods_answer_with_and_without_the_view_suffix() {
        let state = state().await;
        assert_eq!(call_json(&state, "ping", &[]).await["status"], "ok");
        assert_eq!(call_json(&state, "ping.view", &[]).await["status"], "ok");
        assert_eq!(
            call_json(&state, "getLicense", &[]).await["license"]["valid"],
            true
        );
        let extensions = call_json(&state, "getOpenSubsonicExtensions", &[]).await;
        assert_eq!(
            extensions["openSubsonicExtensions"],
            json!([{"name": "formPost", "versions": [1]}])
        );
    }

    #[tokio::test]
    async fn unknown_methods_fail_with_code_0() {
        let reply = call_json(&state().await, "getVideos", &[]).await;
        assert_eq!(reply["status"], "failed");
        assert_eq!(reply["error"]["code"], 0);
        assert_eq!(reply["error"]["message"], "getVideos is not supported");
    }

    #[tokio::test]
    async fn every_method_but_the_extensions_needs_credentials() {
        use super::test_util::call_as;
        let state = state().await;
        assert_eq!(call_as(&state, "ping", &[]).await["error"]["code"], 10);
        // Authentication comes first, so an unknown method learns nothing.
        assert_eq!(call_as(&state, "getVideos", &[]).await["error"]["code"], 10);
        assert_eq!(
            call_as(&state, "ping", &[("u", "u"), ("p", "wrong")]).await["error"]["code"],
            40
        );
        assert_eq!(
            call_as(&state, "getLicense", &[("u", "u"), ("p", "enc:70")]).await["status"],
            "ok"
        );
        assert_eq!(
            call_as(&state, "getOpenSubsonicExtensions", &[]).await["status"],
            "ok"
        );
        assert_eq!(
            call_as(&state, "getOpenSubsonicExtensions.view", &[]).await["status"],
            "ok"
        );
    }
}
