//! Discogs access behind a Radish-owned adapter.
//!
//! `discogger` handles request building and authentication. This module adds
//! the pieces Radish needs on top: credential handling, a per-attempt timeout,
//! serialized request pacing, bounded retries for transient and rate-limit
//! failures, in-memory request deduplication, credential redaction, and
//! conversion to the internal DTOs the matcher understands.

use crate::matcher::{RemoteRelease, RemoteTrack, SearchSummary, TrackEntryType, parse_duration};
use discogger::{DiscogsClient, DiscogsError, PaginationParams, SearchParams, SearchType};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Personal access token environment variable.
pub const TOKEN_ENV: &str = "RADISH_DISCOGS_TOKEN";
/// Test-server override. Never required for normal use, and never set by Radish.
pub const BASE_URL_ENV: &str = "RADISH_DISCOGS_BASE_URL";
/// Discogs search page size used for every strategy.
pub const PER_PAGE: u32 = 50;

#[derive(Debug)]
pub enum AdapterError {
    MissingToken,
    Auth(String),
    NotFound(u64),
    Network(String),
    RateLimited(String),
    Malformed(String),
}

impl AdapterError {
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth(_) | Self::MissingToken)
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingToken => write!(f, "Discogs token is not set ({TOKEN_ENV})"),
            Self::Auth(message) => write!(f, "Discogs authentication failed: {message}"),
            Self::NotFound(id) => write!(f, "Discogs release {id} was not found"),
            Self::Network(message) => write!(f, "Discogs request failed: {message}"),
            Self::RateLimited(message) => write!(f, "Discogs rate limit exceeded: {message}"),
            Self::Malformed(message) => {
                write!(f, "Discogs returned an unusable response: {message}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// Timing and retry bounds for one matching run.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub request_timeout: Duration,
    pub min_interval: Duration,
    pub transient_delays: Vec<Duration>,
    pub rate_limit_delay: Duration,
    pub rate_limit_retries: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            min_interval: Duration::from_secs(1),
            transient_delays: vec![Duration::from_secs(2), Duration::from_secs(4)],
            rate_limit_delay: Duration::from_secs(60),
            rate_limit_retries: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchPage {
    pub hits: Vec<SearchSummary>,
    pub page: u32,
    pub pages: u32,
}

impl SearchPage {
    /// True when more pages exist than the caller fetched.
    pub fn truncated(&self) -> bool {
        self.pages > self.page
    }
}

pub struct DiscogsAdapter {
    client: DiscogsClient,
    policy: RetryPolicy,
    token: String,
    last_request: Mutex<Option<Instant>>,
    release_cache: Mutex<HashMap<u64, RemoteRelease>>,
    search_cache: Mutex<HashMap<String, SearchPage>>,
}

impl DiscogsAdapter {
    /// Builds an adapter from the environment. The token is only read here, so
    /// commands that never touch the network never need it.
    pub fn from_env() -> Result<Self, AdapterError> {
        let token = std::env::var(TOKEN_ENV).unwrap_or_default();
        if token.trim().is_empty() {
            return Err(AdapterError::MissingToken);
        }

        let base_url = std::env::var(BASE_URL_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty());

        Self::build(token.trim().to_string(), RetryPolicy::default(), base_url)
    }

    fn build(
        token: String,
        policy: RetryPolicy,
        base_url: Option<String>,
    ) -> Result<Self, AdapterError> {
        let mut builder = DiscogsClient::builder()
            .user_agent(format!("Radish/{}", env!("CARGO_PKG_VERSION")))
            .personal_token(token.clone());
        if let Some(base_url) = base_url {
            builder = builder.base_url(base_url);
        }
        let client = builder
            .build()
            .map_err(|error| AdapterError::Malformed(error.to_string()))?;

        Ok(Self {
            client,
            policy,
            token,
            last_request: Mutex::new(None),
            release_cache: Mutex::new(HashMap::new()),
            search_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Removes the token from any text that might be printed or stored.
    fn redact(&self, text: String) -> String {
        if text.contains(&self.token) {
            text.replace(&self.token, "[redacted]")
        } else {
            text
        }
    }

    /// Spaces outgoing attempts by at least `min_interval`.
    async fn throttle(&self) {
        let wait = {
            let mut last = lock(&self.last_request);
            let now = Instant::now();
            let wait = match *last {
                Some(previous) => self
                    .policy
                    .min_interval
                    .checked_sub(now.saturating_duration_since(previous))
                    .unwrap_or_default(),
                None => Duration::ZERO,
            };
            *last = Some(now + wait);
            wait
        };

        if !wait.is_zero() {
            sleep(wait).await;
        }
    }

    /// Runs one request with a timeout and the bounded retry policy.
    async fn execute<T, F, Fut>(&self, mut op: F) -> Result<T, AdapterError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, DiscogsError>>,
    {
        let mut transient_failures = 0usize;
        let mut rate_limit_failures = 0usize;

        loop {
            self.throttle().await;

            let outcome = tokio::time::timeout(self.policy.request_timeout, op()).await;
            let timed_out = outcome.is_err();

            let failure = match outcome {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => {
                    let failure = Failure::classify(&error);
                    if let Failure::Auth = failure {
                        return Err(AdapterError::Auth(self.redact(error.to_string())));
                    }
                    if let Failure::Permanent = failure {
                        return Err(AdapterError::Malformed(self.redact(error.to_string())));
                    }
                    if let Failure::NotFound = failure {
                        return Err(AdapterError::NotFound(0));
                    }
                    failure
                }
                Err(_elapsed) => Failure::Transient,
            };

            match failure {
                Failure::RateLimited => {
                    if rate_limit_failures < self.policy.rate_limit_retries {
                        let delay = self.policy.rate_limit_delay;
                        rate_limit_failures += 1;
                        sleep(delay).await;
                    } else {
                        return Err(AdapterError::RateLimited(
                            "gave up after retrying a rate-limited request".to_string(),
                        ));
                    }
                }
                _ => {
                    if transient_failures < self.policy.transient_delays.len() {
                        let delay = self.policy.transient_delays[transient_failures];
                        transient_failures += 1;
                        sleep(delay).await;
                    } else if timed_out {
                        return Err(AdapterError::Network(
                            self.redact("request timed out".to_string()),
                        ));
                    } else {
                        return Err(AdapterError::Network(
                            self.redact("request failed after retries".to_string()),
                        ));
                    }
                }
            }
        }
    }

    /// Fetches a concrete release, deduplicating repeated requests in a run.
    pub async fn fetch_release(&self, id: u64) -> Result<RemoteRelease, AdapterError> {
        if let Some(cached) = lock(&self.release_cache).get(&id) {
            return Ok(cached.clone());
        }

        let release = self
            .execute(|| async { self.client.release(id).await })
            .await
            .map_err(|error| match error {
                AdapterError::NotFound(_) => AdapterError::NotFound(id),
                other => other,
            })?;

        let remote = RemoteRelease::from_discogs(release);
        lock(&self.release_cache).insert(id, remote.clone());

        Ok(remote)
    }

    /// Searches concrete releases. Masters are never returned: accepted
    /// identities always reference concrete releases.
    pub async fn search(
        &self,
        params: SearchParams,
        page: u32,
    ) -> Result<SearchPage, AdapterError> {
        let cache_key = format!("{params:?}|{page}");
        if let Some(cached) = lock(&self.search_cache).get(&cache_key) {
            return Ok(cached.clone());
        }

        let pagination = PaginationParams::new(page, PER_PAGE);
        let result = self
            .execute(|| async { self.client.search(&params, &pagination).await })
            .await?;

        let hits: Vec<SearchSummary> = result
            .items
            .into_iter()
            .filter(|hit| {
                hit.result_type
                    .as_deref()
                    .is_none_or(|kind| kind.eq_ignore_ascii_case("release"))
            })
            .map(SearchSummary::from_discogs)
            .collect();

        let page_info = SearchPage {
            hits,
            page: result.pagination.page,
            pages: result.pagination.pages,
        };

        lock(&self.search_cache).insert(cache_key, page_info.clone());

        Ok(page_info)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Auth,
    NotFound,
    RateLimited,
    Transient,
    Permanent,
}

impl Failure {
    fn classify(error: &DiscogsError) -> Self {
        match error {
            DiscogsError::AuthRequired => Self::Auth,
            DiscogsError::RateLimited => Self::RateLimited,
            DiscogsError::Api { status, .. } => match *status {
                401 | 403 => Self::Auth,
                404 => Self::NotFound,
                429 => Self::RateLimited,
                500..=599 => Self::Transient,
                _ => Self::Permanent,
            },
            DiscogsError::Http(_) => Self::Transient,
            DiscogsError::Configuration(_) => Self::Permanent,
            DiscogsError::Deserialization(_) => Self::Permanent,
        }
    }
}

impl RemoteRelease {
    fn from_discogs(release: discogger::Release) -> Self {
        let artists: Vec<String> = release
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect();

        let mut labels = Vec::new();
        let mut catalog_numbers = Vec::new();
        for label in &release.labels {
            if let Some(name) = label.name.clone() {
                labels.push(name);
            }
            if let Some(catalog) = label.catno.clone().filter(|value| !value.trim().is_empty()) {
                catalog_numbers.push(catalog);
            }
        }
        catalog_numbers.sort();
        catalog_numbers.dedup();

        let formats: Vec<String> = release
            .formats
            .iter()
            .map(|format| {
                let mut text = format.name.clone().unwrap_or_default();
                if !format.descriptions.is_empty() {
                    text = format!("{text} ({})", format.descriptions.join(", "));
                }
                text.trim().to_string()
            })
            .filter(|text| !text.is_empty())
            .collect();

        let barcodes: Vec<String> = release
            .identifiers
            .iter()
            .filter(|identifier| {
                identifier
                    .identifier_type
                    .as_deref()
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("barcode"))
            })
            .filter_map(|identifier| identifier.value.clone())
            .collect();

        let tracklist = release
            .tracklist
            .iter()
            .map(|track| RemoteTrack {
                position: track.position.clone(),
                title: track.title.clone().unwrap_or_default(),
                artists: track
                    .artists
                    .iter()
                    .map(|artist| artist.name.clone())
                    .collect(),
                duration_secs: track.duration.as_deref().and_then(parse_duration),
                entry_type: TrackEntryType::from_discogs(track.track_type.as_deref()),
            })
            .collect();

        Self {
            id: release.id,
            master_id: release.master_id,
            title: release.title.unwrap_or_default(),
            artists,
            year: release.year.map(i64::from),
            country: release.country,
            labels,
            catalog_numbers,
            formats,
            barcodes,
            tracklist,
            url: format!("https://www.discogs.com/release/{}", release.id),
        }
    }
}

impl SearchSummary {
    fn from_discogs(hit: discogger::SearchResult) -> Self {
        Self {
            id: hit.id,
            title: hit.title.unwrap_or_default(),
            year: hit.year.and_then(|value| value.parse::<i64>().ok()),
            country: hit.country,
            labels: hit.label,
            catalog_numbers: hit.catno.into_iter().collect(),
            formats: hit.format,
            barcodes: hit.barcode,
            result_type: hit.result_type.unwrap_or_else(|| "release".to_string()),
        }
    }
}

/// Convenience constructor used by the matching runner.
pub fn release_search_params(title: &str, artist: Option<&str>) -> SearchParams {
    let mut params = SearchParams::new()
        .search_type(SearchType::Release)
        .title(title.to_string());
    if let Some(artist) = artist {
        params = params.artist(artist.to_string());
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    struct MockServer {
        url: String,
        shutdown: Arc<AtomicBool>,
        requests: Arc<AtomicUsize>,
    }

    impl MockServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&str) -> (u16, String) + Send + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
            let address = listener.local_addr().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(AtomicUsize::new(0));
            let flag = shutdown.clone();
            let counter = requests.clone();

            thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _ = respond(stream, &handler);
                }
            });

            Self {
                url: format!("http://{address}"),
                shutdown,
                requests,
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    fn respond<F>(mut stream: TcpStream, handler: &F) -> std::io::Result<()>
    where
        F: Fn(&str) -> (u16, String),
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }

        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let (status, body) = handler(&format!("{path} {headers}"));
        let payload = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(payload.as_bytes())?;
        stream.flush()
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            request_timeout: Duration::from_secs(5),
            min_interval: Duration::ZERO,
            transient_delays: vec![Duration::from_millis(1), Duration::from_millis(1)],
            rate_limit_delay: Duration::from_millis(1),
            rate_limit_retries: 2,
        }
    }

    fn adapter(url: &str, policy: RetryPolicy) -> DiscogsAdapter {
        DiscogsAdapter::build("test-token".to_string(), policy, Some(url.to_string())).unwrap()
    }

    fn release_json() -> String {
        r#"{
            "id": 42,
            "title": "Blue",
            "year": 2001,
            "country": "US",
            "artists": [{"id": 1, "name": "Test Artist"}],
            "labels": [{"id": 2, "name": "Label", "catno": "CAT-1"}],
            "formats": [{"name": "CD", "descriptions": ["Album"]}],
            "identifiers": [{"type": "Barcode", "value": "0123456789012"}],
            "tracklist": [
                {"position": "1", "title": "One", "duration": "3:00", "type_": "track"},
                {"position": "2", "title": "Two", "duration": "2:30"},
                {"position": "", "title": "Notes", "type_": "heading"},
                {"position": "", "title": "Suite", "type_": "index"}
            ]
        }"#
        .to_string()
    }

    #[tokio::test]
    async fn converts_releases_and_keeps_structure_types() {
        let server = MockServer::start(|path| {
            assert!(path.starts_with("/releases/42"));
            (200, release_json())
        });
        let adapter = adapter(&server.url, fast_policy());

        let release = adapter.fetch_release(42).await.unwrap();
        assert_eq!(release.title, "Blue");
        assert_eq!(release.year, Some(2001));
        assert_eq!(release.catalog_numbers, vec!["CAT-1".to_string()]);
        assert_eq!(release.barcodes, vec!["0123456789012".to_string()]);
        assert_eq!(release.tracklist.len(), 4);
        assert_eq!(release.playable_tracks().len(), 2);
        assert_eq!(release.tracklist[0].duration_secs, Some(180.0));
        assert!(release.has_unsupported_entries());
        assert_eq!(release.url, "https://www.discogs.com/release/42");
    }

    #[tokio::test]
    async fn deduplicates_requests_within_a_run() {
        let server = MockServer::start(|_| (200, release_json()));
        let adapter = adapter(&server.url, fast_policy());

        adapter.fetch_release(42).await.unwrap();
        adapter.fetch_release(42).await.unwrap();

        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn sends_the_token_and_redacts_it_from_errors() {
        let server = MockServer::start(|path| {
            assert!(path.contains("token=test-token"), "request was: {path}");
            (401, "{\"message\":\"unauthorized\"}".to_string())
        });
        let adapter = adapter(&server.url, fast_policy());

        let error = adapter.fetch_release(42).await.unwrap_err();
        assert!(error.is_auth(), "{error}");
        assert!(!error.to_string().contains("test-token"));
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn retries_transient_failures_then_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let server = MockServer::start(move |_| {
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                (500, "{\"message\":\"boom\"}".to_string())
            } else {
                (200, release_json())
            }
        });
        let adapter = adapter(&server.url, fast_policy());

        assert!(adapter.fetch_release(42).await.is_ok());
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn reports_retry_exhaustion() {
        let server = MockServer::start(|_| (503, "{}".to_string()));
        let adapter = adapter(&server.url, fast_policy());

        let error = adapter.fetch_release(42).await.unwrap_err();
        assert!(matches!(error, AdapterError::Network(_)), "{error}");
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn waits_and_retries_rate_limited_requests() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let server = MockServer::start(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (429, "{}".to_string())
            } else {
                (200, release_json())
            }
        });
        let adapter = adapter(&server.url, fast_policy());

        assert!(adapter.fetch_release(42).await.is_ok());
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn reports_malformed_responses() {
        let server = MockServer::start(|_| (200, "not json".to_string()));
        let adapter = adapter(&server.url, fast_policy());

        let error = adapter.fetch_release(42).await.unwrap_err();
        assert!(matches!(error, AdapterError::Malformed(_)), "{error}");
    }

    #[tokio::test]
    async fn times_out_slow_requests() {
        let server = MockServer::start(|_| {
            thread::sleep(Duration::from_millis(300));
            (200, release_json())
        });
        let policy = RetryPolicy {
            request_timeout: Duration::from_millis(30),
            transient_delays: vec![Duration::from_millis(1)],
            ..fast_policy()
        };
        let adapter = adapter(&server.url, policy);

        let error = adapter.fetch_release(42).await.unwrap_err();
        assert!(matches!(error, AdapterError::Network(_)), "{error}");
    }

    #[tokio::test]
    async fn spaces_outgoing_requests() {
        let server = MockServer::start(|_| (200, release_json()));
        let policy = RetryPolicy {
            min_interval: Duration::from_millis(60),
            ..fast_policy()
        };
        let adapter = adapter(&server.url, policy);

        let start = Instant::now();
        adapter.fetch_release(1).await.unwrap();
        adapter.fetch_release(2).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(60));
    }

    #[tokio::test]
    async fn reports_pagination_so_callers_can_detect_truncation() {
        let server = MockServer::start(|path| {
            let page = if path.contains("page=2") { 2 } else { 1 };
            (
                200,
                format!(
                    r#"{{"pagination":{{"page":{page},"pages":3,"per_page":50,"items":150}},
                        "results":[{{"id":7,"type":"release","title":"Test Artist - Blue","year":"2001"}}]}}"#
                ),
            )
        });
        let adapter = adapter(&server.url, fast_policy());
        let params = release_search_params("Blue", Some("Test Artist"));

        let first = adapter.search(params.clone(), 1).await.unwrap();
        assert_eq!(first.hits.len(), 1);
        assert!(first.truncated());

        let second = adapter.search(params, 2).await.unwrap();
        assert_eq!(second.page, 2);
        assert!(second.truncated());
    }

    /// Live smoke test. Never part of CI: requires a personal token and an
    /// explicitly supplied release ID.
    ///
    /// ```text
    /// RADISH_DISCOGS_TOKEN=... RADISH_DISCOGS_RELEASE_ID=249504 \
    ///   cargo test --locked live_smoke_fetches_an_explicit_release -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires RADISH_DISCOGS_TOKEN and RADISH_DISCOGS_RELEASE_ID"]
    async fn live_smoke_fetches_an_explicit_release() {
        let id: u64 = std::env::var("RADISH_DISCOGS_RELEASE_ID")
            .expect("set RADISH_DISCOGS_RELEASE_ID to a real release ID")
            .parse()
            .expect("RADISH_DISCOGS_RELEASE_ID must be numeric");
        let adapter = DiscogsAdapter::from_env().expect("set RADISH_DISCOGS_TOKEN");
        let release = adapter.fetch_release(id).await.expect("fetch release");
        assert_eq!(release.id, id);
        assert!(!release.tracklist.is_empty());
    }

    #[test]
    fn reads_the_token_from_the_environment() {
        // SAFETY: this test owns the environment variable for its duration.
        unsafe {
            std::env::remove_var(TOKEN_ENV);
        }
        assert!(matches!(
            DiscogsAdapter::from_env(),
            Err(AdapterError::MissingToken)
        ));
    }
}
