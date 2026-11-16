//! Deezer access behind a Radish-owned HTTP adapter.
//!
//! Deezer's public API needs no key. Search is free text only
//! (`/search/album?q=`), a barcode is looked up directly
//! (`/album/upc:{code}`), and a release is `/album/{id}` plus its paged
//! `/album/{id}/tracks`. Deezer reports every failure as HTTP 200 with an
//! `error` object: code 4 is the request quota (50 requests per 5 seconds) and
//! code 800 is "no data".
//!
//! Every attempt, retries included, passes through one pacing slot that all
//! releases of a run share, so `--jobs` never multiplies the request rate. No
//! credential exists or is read here.

use crate::matcher::{
    RemoteRelease, RemoteTrack, SearchSummary, TrackEntryType, normalize_barcode, normalize_text,
};
use crate::musicbrainz::USER_AGENT_VALUE;
use crate::providers::{
    BoundedCache, MetadataProvider, ProviderError, ProviderKind, RELEASE_CACHE_CAPACITY, ReleaseId,
    SEARCH_CACHE_CAPACITY, SearchPage, SearchRequest, SearchStrategy, lock, loopback_override,
};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Test-server override. Never required for normal use, and never set by Radish.
pub const BASE_URL_ENV: &str = "RADISH_DEEZER_BASE_URL";

/// Canonical public album links, independent of any test-server override.
pub const ALBUM_URL_PREFIX: &str = "https://www.deezer.com/album/";

/// Deezer's own artist entry for compilations. Its name is localized by the
/// requester's address (`Verschillende artiesten` from the Netherlands), so it
/// is recognized by id and always read as [`VARIOUS_ARTISTS`].
pub const VARIOUS_ARTISTS_ID: u64 = 5080;

/// The name a [`VARIOUS_ARTISTS_ID`] credit is read as.
const VARIOUS_ARTISTS: &str = "Various Artists";

/// Deezer error code for an exhausted request quota (50 requests per 5 s).
const QUOTA_EXCEEDED: i64 = 4;

/// Deezer error code for a lookup or barcode that matches nothing.
const NO_DATA: i64 = 800;

/// Tracks asked for per `/album/{id}/tracks` page.
const TRACKS_PER_PAGE: usize = 100;

/// Most track pages read for one album: 1,000 tracks. A longer album is cut
/// off there, and the structure issue for missing tracks keeps it in review.
const MAX_TRACK_PAGES: usize = 10;

/// Timing and retry bounds for one run.
#[derive(Debug, Clone)]
pub struct DeezerConfig {
    pub base_url: Url,
    /// Least time between the starts of two requests. 110 ms keeps a run at
    /// about 9 requests a second, under the quota of 50 per 5 s.
    pub min_interval: Duration,
    pub request_timeout: Duration,
    /// Attempts per request, the first included.
    pub max_attempts: u8,
    /// Wait after a failed attempt, by retry; the last entry repeats.
    pub backoff: Vec<Duration>,
    /// Wait after a quota error before the next attempt.
    pub quota_wait: Duration,
}

impl Default for DeezerConfig {
    fn default() -> Self {
        Self {
            base_url: Url::parse("https://api.deezer.com").expect("the Deezer URL is valid"),
            min_interval: Duration::from_millis(110),
            request_timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoff: vec![Duration::from_secs(2), Duration::from_secs(4)],
            quota_wait: Duration::from_secs(5),
        }
    }
}

/// The Deezer adapter: pacing, retries, caches and conversion.
#[derive(Debug)]
pub struct DeezerAdapter {
    client: reqwest::Client,
    config: DeezerConfig,
    /// When the next request may start; shared by every attempt.
    next_slot: Mutex<Option<Instant>>,
    /// When each attempt was released by `schedule`, so tests can measure the
    /// spacing without depending on when the server accepts.
    #[cfg(test)]
    released_at: Mutex<Vec<Instant>>,
    release_cache: Mutex<BoundedCache<u64, RemoteRelease>>,
    search_cache: Mutex<BoundedCache<String, SearchPage>>,
}

/// Why one attempt failed.
enum Failure {
    /// Deezer's quota error (code 4): wait, then try again.
    Quota,
    /// A transport failure, a timeout or a server error: back off, then try again.
    Transient(ProviderError),
    /// Trying again would fail the same way.
    Final(ProviderError),
}

impl DeezerAdapter {
    /// Builds the production adapter. No request is made until a search or lookup.
    ///
    /// # Errors
    ///
    /// When [`BASE_URL_ENV`] names anything but a loopback URL, or the HTTP
    /// client cannot be built.
    pub fn new() -> anyhow::Result<Self> {
        let mut config = DeezerConfig::default();
        if let Some(base_url) = loopback_override(BASE_URL_ENV)? {
            config.base_url = base_url;
        }
        Self::with_config(config)
    }

    /// Builds an adapter with explicit bounds, as tests do.
    ///
    /// # Errors
    ///
    /// When the HTTP client cannot be built.
    pub fn with_config(config: DeezerConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()?;
        Ok(Self {
            client,
            config,
            next_slot: Mutex::new(None),
            #[cfg(test)]
            released_at: Mutex::new(Vec::new()),
            release_cache: Mutex::new(BoundedCache::new(RELEASE_CACHE_CAPACITY)),
            search_cache: Mutex::new(BoundedCache::new(SEARCH_CACHE_CAPACITY)),
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.config
            .base_url
            .join(path)
            .map_err(|error| ProviderError::Configuration {
                provider: ProviderKind::Deezer,
                message: format!("cannot build {path}: {error}"),
            })
    }

    /// Waits for this attempt's slot. Setting the next slot from `now` means no
    /// burst even when a caller arrives late.
    async fn schedule(&self) {
        let wait = {
            let mut next = lock(&self.next_slot);
            let now = Instant::now();
            let wait = match *next {
                Some(previous) => {
                    (previous + self.config.min_interval).saturating_duration_since(now)
                }
                None => Duration::ZERO,
            };
            *next = Some(now + wait);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        #[cfg(test)]
        lock(&self.released_at).push(Instant::now());
    }

    /// The wait before retry number `retry` (0 for the first retry).
    fn backoff(&self, retry: usize) -> Duration {
        self.config
            .backoff
            .get(retry)
            .or(self.config.backoff.last())
            .copied()
            .unwrap_or_default()
    }

    /// One GET, paced and retried, with Deezer's error object read.
    ///
    /// `Ok(None)` is "no data": Deezer's code 800, or HTTP 404.
    async fn get_json(&self, url: Url) -> Result<Option<Value>, ProviderError> {
        let mut attempt: u8 = 0;
        loop {
            attempt += 1;
            self.schedule().await;
            let failure = match self.attempt(url.clone()).await {
                Ok(outcome) => return Ok(outcome),
                Err(failure) => failure,
            };
            let last = attempt >= self.config.max_attempts;
            let wait = match failure {
                Failure::Final(error) => return Err(error),
                Failure::Quota if last => {
                    return Err(ProviderError::RateLimited {
                        provider: ProviderKind::Deezer,
                        message: format!(
                            "the request quota stayed exhausted after {attempt} attempts"
                        ),
                        retry_after: Some(self.config.quota_wait),
                    });
                }
                Failure::Transient(error) if last => return Err(error),
                Failure::Quota => self.config.quota_wait,
                Failure::Transient(_) => self.backoff(usize::from(attempt) - 1),
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One attempt; `Ok(None)` is "no data".
    async fn attempt(&self, url: Url) -> Result<Option<Value>, Failure> {
        let response = self
            .client
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/json")
            .header(ACCEPT_LANGUAGE, "en")
            .send()
            .await
            .map_err(|error| Failure::Transient(transport_error(&error)))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Err(Failure::Transient(ProviderError::Network {
                provider: ProviderKind::Deezer,
                message: format!("HTTP {status}"),
            }));
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Failure::Final(ProviderError::Permanent {
                provider: ProviderKind::Deezer,
                status: Some(status.as_u16()),
                message: "the request failed permanently".to_string(),
            }));
        }
        // A connection that drops mid-body is as transient as one that fails
        // before the headers.
        let body = response
            .text()
            .await
            .map_err(|error| Failure::Transient(transport_error(&error)))?;
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            Failure::Final(ProviderError::Malformed {
                provider: ProviderKind::Deezer,
                message: format!("response body is not JSON: {error}"),
            })
        })?;
        match deezer_error(&value) {
            None => Ok(Some(value)),
            Some((NO_DATA, _)) => Ok(None),
            Some((QUOTA_EXCEEDED, _)) => Err(Failure::Quota),
            Some((code, message)) => Err(Failure::Final(ProviderError::Permanent {
                provider: ProviderKind::Deezer,
                status: None,
                message: format!("Deezer error {code}: {message}"),
            })),
        }
    }

    /// `/album/upc:{digits}`, as a page of zero or one hit.
    async fn search_barcode(&self, barcode: &str) -> Result<SearchPage, ProviderError> {
        let digits = normalize_barcode(barcode);
        if digits.is_empty() {
            return Err(empty_search());
        }
        let key = format!("upc:{digits}");
        if let Some(cached) = lock(&self.search_cache).get(&key) {
            return Ok(cached);
        }
        let mut album = self
            .get_json(self.endpoint(&format!("/album/upc:{digits}"))?)
            .await?;
        // An EAN-13 tag of a 12-digit UPC carries a leading zero Deezer does
        // not store.
        if album.is_none()
            && digits.len() == 13
            && let Some(upc) = digits.strip_prefix('0')
        {
            album = self
                .get_json(self.endpoint(&format!("/album/upc:{upc}"))?)
                .await?;
        }
        let hits: Vec<SearchSummary> = album
            .as_ref()
            .and_then(decode_summary)
            .into_iter()
            .collect();
        let page = SearchPage {
            total: Some(hits.len() as u64),
            hits,
            next_offset: None,
        };
        lock(&self.search_cache).insert(key, page.clone());
        Ok(page)
    }

    /// `/search/album?q=`, one page.
    async fn search_text(
        &self,
        query: &str,
        request: &SearchRequest,
    ) -> Result<SearchPage, ProviderError> {
        let key = format!("{query}|{}|{}", request.offset, request.limit);
        if let Some(cached) = lock(&self.search_cache).get(&key) {
            return Ok(cached);
        }
        let mut url = self.endpoint("/search/album")?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("limit", &request.limit.to_string())
            .append_pair("index", &request.offset.to_string());
        let page = match self.get_json(url).await? {
            Some(value) => decode_search_page(&value, request.offset),
            None => SearchPage {
                hits: Vec::new(),
                total: Some(0),
                next_offset: None,
            },
        };
        lock(&self.search_cache).insert(key, page.clone());
        Ok(page)
    }

    /// `/album/{id}` and every page of its tracks, cached by album id.
    async fn fetch_album(&self, id: u64) -> Result<RemoteRelease, ProviderError> {
        if let Some(cached) = lock(&self.release_cache).get(&id) {
            return Ok(cached);
        }
        let not_found = || ProviderError::NotFound {
            provider: ProviderKind::Deezer,
            id: id.to_string(),
        };
        let album = self
            .get_json(self.endpoint(&format!("/album/{id}"))?)
            .await?
            .ok_or_else(not_found)?;

        let mut tracks: Vec<Value> = Vec::new();
        for _ in 0..MAX_TRACK_PAGES {
            let mut url = self.endpoint(&format!("/album/{id}/tracks"))?;
            url.query_pairs_mut()
                .append_pair("limit", &TRACKS_PER_PAGE.to_string())
                .append_pair("index", &tracks.len().to_string());
            let Some(page) = self.get_json(url).await? else {
                break;
            };
            let data = page
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let more = !data.is_empty() && page.get("next").and_then(Value::as_str).is_some();
            tracks.extend(data);
            if !more {
                break;
            }
        }

        let release = decode_album(&album, &tracks)?;
        lock(&self.release_cache).insert(id, release.clone());
        Ok(release)
    }
}

impl MetadataProvider for DeezerAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Deezer
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchPage, ProviderError> {
        match &request.strategy {
            SearchStrategy::Barcode(barcode) => self.search_barcode(barcode).await,
            SearchStrategy::TitleArtist { title, artist, .. } if !title.trim().is_empty() => {
                let query = query_text(title, artist.as_deref());
                self.search_text(&query, &request).await
            }
            SearchStrategy::CatalogNumber(_) => Err(ProviderError::Permanent {
                provider: ProviderKind::Deezer,
                status: None,
                message: "Deezer has no catalog number search".to_string(),
            }),
            SearchStrategy::TitleArtist { .. } => Err(empty_search()),
        }
    }

    async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
        let ReleaseId::Deezer(album) = id else {
            return Err(ProviderError::Permanent {
                provider: ProviderKind::Deezer,
                status: None,
                message: format!("Deezer cannot fetch a {} release", id.provider()),
            });
        };
        self.fetch_album(album).await
    }
}

fn empty_search() -> ProviderError {
    ProviderError::Permanent {
        provider: ProviderKind::Deezer,
        status: None,
        message: "refusing an unusable empty search term".to_string(),
    }
}

/// Classifies a failed send or body read as a transient network failure.
fn transport_error(error: &reqwest::Error) -> ProviderError {
    ProviderError::Network {
        provider: ProviderKind::Deezer,
        message: if error.is_timeout() {
            "request timed out".to_string()
        } else {
            error.to_string()
        },
    }
}

/// The code and message of a Deezer `{"error": {...}}` body.
fn deezer_error(value: &Value) -> Option<(i64, String)> {
    let error = value.get("error")?;
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let message = text(error, "message").unwrap_or_default();
    Some((code, message))
}

/// The free text of a title search: the artist, then the title.
///
/// A missing or Various Artists artist is left out: Deezer credits
/// compilations to its own entry, whose name it translates.
fn query_text(title: &str, artist: Option<&str>) -> String {
    let title = title.trim();
    match artist
        .map(str::trim)
        .filter(|artist| !artist.is_empty() && !is_various(artist))
    {
        Some(artist) => format!("{artist} {title}"),
        None => title.to_string(),
    }
}

fn is_various(name: &str) -> bool {
    matches!(
        normalize_text(name).as_str(),
        "va" | "various" | "various artists"
    )
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// An artist object's name, with Deezer's Various Artists entry read by id.
fn artist_name(artist: &Value) -> Option<String> {
    if artist.get("id").and_then(Value::as_u64) == Some(VARIOUS_ARTISTS_ID) {
        return Some(VARIOUS_ARTISTS.to_string());
    }
    text(artist, "name")
}

/// `Digital Media` plus the release type, as Radish's short-release rules read it.
fn formats(record_type: Option<&str>) -> Vec<String> {
    let mut formats = vec!["Digital Media".to_string()];
    let kind = match record_type {
        Some("album") => Some("Album"),
        Some("ep") => Some("EP"),
        Some("single") => Some("Single"),
        Some("compile") => Some("Compilation"),
        _ => None,
    };
    formats.extend(kind.map(str::to_string));
    formats
}

/// The UPC, plus its 13-digit EAN form when it has 12 digits. The matcher
/// compares barcodes as digit strings, so an EAN-13 tag would otherwise
/// contradict the same code.
fn barcodes(upc: Option<&str>) -> Vec<String> {
    let digits = upc.map(normalize_barcode).unwrap_or_default();
    match digits.len() {
        0 => Vec::new(),
        12 => vec![digits.clone(), format!("0{digits}")],
        _ => vec![digits],
    }
}

/// The year of a `YYYY-MM-DD` date; Deezer's `0000-00-00` is no year.
fn year(date: Option<&str>) -> Option<i64> {
    date?.get(..4)?.parse::<i64>().ok().filter(|year| *year > 0)
}

/// A search hit or barcode lookup as a summary.
fn decode_summary(album: &Value) -> Option<SearchSummary> {
    let id = album.get("id").and_then(Value::as_u64)?;
    let record_type = text(album, "record_type");
    Some(SearchSummary {
        id: ReleaseId::Deezer(id),
        title: text(album, "title").unwrap_or_default(),
        artists: album
            .get("artist")
            .and_then(artist_name)
            .into_iter()
            .collect(),
        artist_credit: None,
        year: year(text(album, "release_date").as_deref()),
        country: None,
        labels: text(album, "label").into_iter().collect(),
        catalog_numbers: Vec::new(),
        formats: formats(record_type.as_deref()),
        barcodes: barcodes(text(album, "upc").as_deref()),
        result_type: "album".to_string(),
    })
}

/// One `/search/album` page. A next page exists while Deezer names one.
fn decode_search_page(value: &Value, offset: u16) -> SearchPage {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let next_offset = value
        .get("next")
        .and_then(Value::as_str)
        .filter(|_| !data.is_empty())
        .map(|_| (usize::from(offset) + data.len()).min(usize::from(u16::MAX)) as u16);
    SearchPage {
        hits: data.iter().filter_map(decode_summary).collect(),
        total: value.get("total").and_then(Value::as_u64),
        next_offset,
    }
}

/// Converts `/album/{id}` and its tracks into the shared model.
///
/// # Errors
///
/// [`ProviderError::Malformed`] when the album has no id or title.
pub fn decode_album(album: &Value, tracks: &[Value]) -> Result<RemoteRelease, ProviderError> {
    let malformed = |message: &str| ProviderError::Malformed {
        provider: ProviderKind::Deezer,
        message: message.to_string(),
    };
    let id = album
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed("album has no id"))?;
    let title = text(album, "title").ok_or_else(|| malformed("album has no title"))?;

    // The main contributors, else the album artist.
    let mut artists: Vec<String> = Vec::new();
    for contributor in album
        .get("contributors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|contributor| text(contributor, "role").as_deref() == Some("Main"))
    {
        if let Some(name) = artist_name(contributor)
            && !artists.contains(&name)
        {
            artists.push(name);
        }
    }
    if artists.is_empty() {
        artists.extend(album.get("artist").and_then(artist_name));
    }

    let record_type = text(album, "record_type");
    let upc = text(album, "upc");
    let mut ordered: Vec<(u32, u32, usize, &Value)> = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let disk = track
                .get("disk_number")
                .and_then(Value::as_u64)
                .and_then(|disk| u32::try_from(disk).ok())
                .unwrap_or(1);
            let position = track
                .get("track_position")
                .and_then(Value::as_u64)
                .and_then(|position| u32::try_from(position).ok())
                .unwrap_or(0);
            (disk, position, index, track)
        })
        .collect();
    ordered.sort_by_key(|(disk, position, index, _)| (*disk, *position, *index));

    let mut structure_issues = Vec::new();
    let mut tracklist = Vec::with_capacity(ordered.len());
    for (disk, position, index, track) in ordered {
        let track_position = (position > 0).then_some(position);
        if track_position.is_none() {
            structure_issues.push(format!("track {} has no position", index + 1));
        }
        tracklist.push(RemoteTrack {
            position: track_position.map(|position| format!("{disk}-{position}")),
            title: text(track, "title").unwrap_or_default(),
            artists: track
                .get("artist")
                .and_then(artist_name)
                .into_iter()
                .collect(),
            duration_secs: track
                .get("duration")
                .and_then(Value::as_u64)
                .filter(|seconds| *seconds > 0)
                .map(|seconds| seconds as f64),
            entry_type: TrackEntryType::Track,
            medium_position: Some(disk),
            track_position,
            release_track_id: None,
            recording_id: None,
            artist_credit: None,
        });
    }
    if let Some(declared) = album.get("nb_tracks").and_then(Value::as_u64)
        && (tracklist.len() as u64) < declared
    {
        structure_issues.push(format!(
            "the album lists {declared} tracks but Deezer returned {}",
            tracklist.len()
        ));
    }

    Ok(RemoteRelease {
        id: ReleaseId::Deezer(id),
        group_id: None,
        title,
        artists,
        year: year(text(album, "release_date").as_deref()),
        country: None,
        labels: text(album, "label").into_iter().collect(),
        catalog_numbers: Vec::new(),
        formats: formats(record_type.as_deref()),
        barcodes: barcodes(upc.as_deref()),
        tracklist,
        url: format!("{ALBUM_URL_PREFIX}{id}"),
        lookup_aliases: Vec::new(),
        artist_credit: None,
        structure_issues,
        edition_notes: Vec::new(),
        provider_metadata: json!({
            "record_type": record_type,
            "upc": upc,
            "explicit_lyrics": album.get("explicit_lyrics").and_then(Value::as_bool),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    /// A loopback server that answers each request with `handler(target)` and
    /// records each target and its lowercased header block.
    struct MockServer {
        url: String,
        shutdown: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
            let address = listener.local_addr().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let flag = shutdown.clone();
            let recorded = requests.clone();

            thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let mut headers = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        headers.push_str(&line.to_ascii_lowercase());
                    }
                    let target = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    recorded.lock().unwrap().push((target.clone(), headers));
                    let (status, body) = handler(&target);
                    let payload = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(payload.as_bytes());
                }
            });

            Self {
                url: format!("http://{address}"),
                shutdown,
                requests,
            }
        }

        fn targets(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(target, _)| target.clone())
                .collect()
        }

        fn headers(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, headers)| headers.clone())
                .collect()
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    fn config(server: &MockServer) -> DeezerConfig {
        DeezerConfig {
            base_url: Url::parse(&server.url).unwrap(),
            min_interval: Duration::ZERO,
            request_timeout: Duration::from_secs(5),
            max_attempts: 3,
            backoff: vec![Duration::ZERO],
            quota_wait: Duration::ZERO,
        }
    }

    fn adapter(server: &MockServer) -> DeezerAdapter {
        DeezerAdapter::with_config(config(server)).unwrap()
    }

    /// `/album/302127` as Deezer sends it, embedded tracks left out.
    fn album_json() -> Value {
        json!({
            "id": 302127,
            "title": "Discovery",
            "upc": "724384960650",
            "link": "https://www.deezer.com/album/302127",
            "label": "Parlophone (France)",
            "nb_tracks": 3,
            "release_date": "2001-03-07",
            "record_type": "album",
            "explicit_lyrics": false,
            "contributors": [
                {"id": 27, "name": "Daft Punk", "role": "Main"},
                {"id": 99, "name": "Romanthony", "role": "Featured"}
            ],
            "artist": {"id": 27, "name": "Daft Punk"}
        })
    }

    fn track_json(id: u64, title: &str, position: u64, disk: u64, duration: u64) -> Value {
        json!({
            "id": id,
            "title": title,
            "title_short": title,
            "title_version": "",
            "duration": duration,
            "track_position": position,
            "disk_number": disk,
            "isrc": "GBDUW0000053",
            "artist": {"id": 27, "name": "Daft Punk"}
        })
    }

    fn tracks_json() -> Vec<Value> {
        vec![
            track_json(3135553, "One More Time", 1, 1, 320),
            track_json(3135554, "Aerodynamic", 2, 1, 212),
            track_json(3135555, "Digital Love (Edit)", 3, 1, 301),
        ]
    }

    fn page(data: Vec<Value>, next: Option<&str>) -> String {
        let mut page = json!({"data": data, "total": 3});
        if let Some(next) = next {
            page["next"] = Value::from(next);
        }
        page.to_string()
    }

    fn error_json(code: i64, message: &str) -> String {
        json!({"error": {"type": "Exception", "message": message, "code": code}}).to_string()
    }

    /// Serves [`album_json`] and its tracks in one page.
    fn album_handler(target: &str) -> (u16, String) {
        if target.starts_with("/album/302127/tracks") {
            (200, page(tracks_json(), None))
        } else if target.starts_with("/album/302127") {
            (200, album_json().to_string())
        } else {
            (200, error_json(NO_DATA, "no data"))
        }
    }

    #[test]
    fn decodes_an_album_with_its_tracks() {
        let release = decode_album(&album_json(), &tracks_json()).unwrap();
        assert_eq!(release.id, ReleaseId::Deezer(302127));
        assert_eq!(release.group_id, None);
        assert_eq!(release.title, "Discovery");
        assert_eq!(release.artists, vec!["Daft Punk".to_string()]);
        assert_eq!(release.year, Some(2001));
        assert_eq!(release.labels, vec!["Parlophone (France)".to_string()]);
        assert!(release.catalog_numbers.is_empty());
        assert_eq!(release.formats, vec!["Digital Media", "Album"]);
        assert_eq!(release.barcodes, vec!["724384960650", "0724384960650"]);
        assert_eq!(release.url, "https://www.deezer.com/album/302127");
        assert!(release.structure_issues.is_empty());
        assert!(release.edition_notes.is_empty());
        assert_eq!(
            release.provider_metadata,
            json!({"record_type": "album", "upc": "724384960650", "explicit_lyrics": false})
        );

        let third = &release.tracklist[2];
        assert_eq!(third.title, "Digital Love (Edit)", "version words are kept");
        assert_eq!(third.position.as_deref(), Some("1-3"));
        assert_eq!(
            (third.medium_position, third.track_position),
            (Some(1), Some(3))
        );
        assert_eq!(third.duration_secs, Some(301.0));
        assert_eq!(third.artists, vec!["Daft Punk".to_string()]);
        assert_eq!(third.entry_type, TrackEntryType::Track);
    }

    #[test]
    fn reads_various_artists_by_id_whatever_its_name() {
        let mut album = album_json();
        album["contributors"] =
            json!([{"id": 5080, "name": "Verschillende artiesten", "role": "Main"}]);
        album["artist"] = json!({"id": 5080, "name": "Verschillende artiesten"});
        album["record_type"] = Value::from("compile");
        let release = decode_album(&album, &tracks_json()).unwrap();
        assert_eq!(release.artists, vec!["Various Artists".to_string()]);
        assert_eq!(release.formats, vec!["Digital Media", "Compilation"]);

        // Without main contributors, the album artist is the credit.
        album["contributors"] = json!([]);
        let release = decode_album(&album, &tracks_json()).unwrap();
        assert_eq!(release.artists, vec!["Various Artists".to_string()]);

        let hit = json!({"id": 1, "title": "Hits", "nb_tracks": 20, "record_type": "compile",
                         "artist": {"id": 5080, "name": "Verschillende artiesten"}});
        let summary = decode_summary(&hit).unwrap();
        assert_eq!(summary.id, ReleaseId::Deezer(1));
        assert_eq!(summary.artists, vec!["Various Artists".to_string()]);
        assert_eq!(summary.formats, vec!["Digital Media", "Compilation"]);
    }

    #[test]
    fn orders_tracks_by_disc_and_position_and_reports_missing_ones() {
        let tracks = vec![
            track_json(3, "Disc Two Opener", 1, 2, 200),
            track_json(1, "First", 1, 1, 180),
            track_json(2, "Second", 2, 1, 190),
        ];
        let mut album = album_json();
        album["nb_tracks"] = Value::from(4);
        let release = decode_album(&album, &tracks).unwrap();
        let positions: Vec<_> = release
            .tracklist
            .iter()
            .map(|track| track.position.clone().unwrap())
            .collect();
        assert_eq!(positions, ["1-1", "1-2", "2-1"]);
        assert_eq!(release.tracklist[2].medium_position, Some(2));
        assert_eq!(
            release.structure_issues,
            ["the album lists 4 tracks but Deezer returned 3"]
        );
        assert!(release.has_unsupported_entries());
    }

    #[test]
    fn keeps_a_thirteen_digit_upc_and_reads_no_year_from_zeros() {
        let mut album = album_json();
        album["upc"] = Value::from("5099902987620");
        album["release_date"] = Value::from("0000-00-00");
        album["record_type"] = Value::from("ep");
        let release = decode_album(&album, &tracks_json()).unwrap();
        assert_eq!(release.barcodes, vec!["5099902987620"]);
        assert_eq!(release.year, None);
        assert_eq!(release.formats, vec!["Digital Media", "EP"]);
    }

    #[test]
    fn an_album_without_id_or_title_is_malformed() {
        let mut album = album_json();
        album.as_object_mut().unwrap().remove("title");
        assert!(matches!(
            decode_album(&album, &[]),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn searches_free_text_without_a_various_artists_credit() {
        assert_eq!(
            query_text(" Discovery ", Some("Daft Punk")),
            "Daft Punk Discovery"
        );
        assert_eq!(query_text("Hits", Some("Various Artists")), "Hits");
        assert_eq!(query_text("Hits", Some("VA")), "Hits");
        assert_eq!(query_text("Hits", Some("  ")), "Hits");
        assert_eq!(query_text("Hits", None), "Hits");
    }

    #[tokio::test]
    async fn fetches_an_album_and_caches_it() {
        let server = MockServer::start(album_handler);
        let adapter = adapter(&server);

        let release = adapter
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap();
        assert_eq!(release.tracklist.len(), 3);
        let again = adapter
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap();
        assert_eq!(again.title, release.title);
        assert_eq!(
            server.targets(),
            ["/album/302127", "/album/302127/tracks?limit=100&index=0"]
        );
        for headers in server.headers() {
            assert!(headers.contains("accept-language: en"), "{headers}");
            assert!(headers.contains("user-agent: radish/"), "{headers}");
        }
    }

    #[tokio::test]
    async fn follows_the_track_pages_until_no_next_page() {
        let server = MockServer::start(|target| {
            if target.contains("/tracks") && target.contains("index=0") {
                let first = tracks_json()[..2].to_vec();
                let next = "https://api.deezer.com/album/302127/tracks?index=2";
                (200, page(first, Some(next)))
            } else if target.contains("/tracks") && target.contains("index=2") {
                (200, page(tracks_json()[2..].to_vec(), None))
            } else {
                album_handler(target)
            }
        });
        let release = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap();
        assert_eq!(release.tracklist.len(), 3);
        assert!(release.structure_issues.is_empty());
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn no_data_is_a_missing_album_and_an_empty_barcode_page() {
        let server = MockServer::start(|_| (200, error_json(NO_DATA, "no data")));
        let adapter = adapter(&server);
        assert!(matches!(
            adapter.fetch_release(ReleaseId::Deezer(1)).await,
            Err(ProviderError::NotFound { .. })
        ));
        let page = adapter
            .search(SearchRequest {
                strategy: SearchStrategy::Barcode("724384960650".to_string()),
                offset: 0,
                limit: 25,
            })
            .await
            .unwrap();
        assert!(page.hits.is_empty());
        assert_eq!(page.next_offset, None);
    }

    #[tokio::test]
    async fn a_thirteen_digit_barcode_is_tried_again_without_its_leading_zero() {
        let server = MockServer::start(|target| {
            if target == "/album/upc:724384960650" {
                (200, album_json().to_string())
            } else {
                (200, error_json(NO_DATA, "no data"))
            }
        });
        let page = adapter(&server)
            .search(SearchRequest {
                strategy: SearchStrategy::Barcode("0724384960650".to_string()),
                offset: 0,
                limit: 25,
            })
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].id, ReleaseId::Deezer(302127));
        assert_eq!(
            server.targets(),
            ["/album/upc:0724384960650", "/album/upc:724384960650"]
        );
    }

    #[tokio::test]
    async fn a_text_search_reads_one_page_and_offers_the_next() {
        let server = MockServer::start(|_| {
            let hit = json!({"id": 302127, "title": "Discovery", "nb_tracks": 14,
                             "record_type": "album", "artist": {"id": 27, "name": "Daft Punk"}});
            let next = "https://api.deezer.com/search/album?q=daft&index=25";
            (
                200,
                json!({"data": [hit], "total": 40, "next": next}).to_string(),
            )
        });
        let page = adapter(&server)
            .search(SearchRequest {
                strategy: SearchStrategy::TitleArtist {
                    title: "Discovery".to_string(),
                    artist: Some("Daft Punk".to_string()),
                    release_group: None,
                },
                offset: 0,
                limit: 25,
            })
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].artists, vec!["Daft Punk".to_string()]);
        assert_eq!(page.total, Some(40));
        assert_eq!(page.next_offset, Some(1));
        let target = server.targets().remove(0);
        let url = Url::parse("http://test.local")
            .unwrap()
            .join(&target)
            .unwrap();
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(url.path(), "/search/album");
        assert_eq!(
            pairs,
            [
                ("q".to_string(), "Daft Punk Discovery".to_string()),
                ("limit".to_string(), "25".to_string()),
                ("index".to_string(), "0".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_catalog_number_search_is_refused_without_a_request() {
        let server = MockServer::start(album_handler);
        let error = adapter(&server)
            .search(SearchRequest {
                strategy: SearchStrategy::CatalogNumber("CAT-1".to_string()),
                offset: 0,
                limit: 25,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Permanent { .. }), "{error}");
        assert_eq!(server.request_count(), 0);
    }

    #[tokio::test]
    async fn the_quota_error_is_retried_then_reported_as_a_rate_limit() {
        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let server = MockServer::start(move |target| {
            if target == "/album/302127" {
                let mut calls = counter.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    return (200, error_json(QUOTA_EXCEEDED, "Quota limit exceeded"));
                }
            }
            album_handler(target)
        });
        let release = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap();
        assert_eq!(release.title, "Discovery");
        assert_eq!(*calls.lock().unwrap(), 2);

        let server =
            MockServer::start(|_| (200, error_json(QUOTA_EXCEEDED, "Quota limit exceeded")));
        let error = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderError::RateLimited { .. }),
            "{error}"
        );
        assert!(error.is_transient());
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn server_errors_are_retried_and_end_as_network_failures() {
        let server = MockServer::start(|_| (503, "{}".to_string()));
        let error = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Network { .. }), "{error}");
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn other_errors_and_bodies_that_are_not_json_are_not_retried() {
        let server = MockServer::start(|_| (200, error_json(500, "Wrong parameter")));
        let error = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Permanent { .. }), "{error}");
        assert!(error.to_string().contains("Wrong parameter"), "{error}");
        assert_eq!(server.request_count(), 1);

        let server = MockServer::start(|_| (200, "<html>".to_string()));
        let error = adapter(&server)
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Malformed { .. }), "{error}");
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn requests_are_spaced_across_the_whole_adapter() {
        let server = MockServer::start(album_handler);
        let adapter = DeezerAdapter::with_config(DeezerConfig {
            min_interval: Duration::from_millis(80),
            ..config(&server)
        })
        .unwrap();
        adapter
            .fetch_release(ReleaseId::Deezer(302127))
            .await
            .unwrap();
        // Measured where `schedule` releases each attempt, not when the server
        // happens to accept it.
        let released = lock(&adapter.released_at).clone();
        assert_eq!(released.len(), 2);
        assert!(
            released[1].duration_since(released[0]) >= Duration::from_millis(80),
            "the tracks request was not spaced from the album request"
        );
    }

    #[test]
    fn the_production_pace_stays_under_the_quota() {
        let config = DeezerConfig::default();
        // 50 requests per 5 s is one every 100 ms.
        assert!(config.min_interval > Duration::from_millis(100));
        assert_eq!(config.max_attempts, 3);
        assert_eq!(
            config.backoff,
            [Duration::from_secs(2), Duration::from_secs(4)]
        );
        assert_eq!(config.quota_wait, Duration::from_secs(5));
    }
}
