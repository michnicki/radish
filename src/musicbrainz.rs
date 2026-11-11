//! MusicBrainz access behind a Radish-owned HTTP adapter.
//!
//! Search and lookup use the public JSON API. Radish owns the transport so it
//! can inspect response headers and the raw document: the conversion below
//! reads the JSON directly and retains pregap/data-track findings and every
//! label/catalog pair.
//!
//! All attempts, including redirect hops and retries, pass through one
//! scheduler. No Discogs credential is read here.

use crate::matcher::{RemoteRelease, RemoteTrack, SearchSummary, TrackEntryType};
use crate::providers::{
    GroupId, Mbid, MetadataProvider, ProviderError, ProviderKind, ReleaseId, SearchPage,
    SearchRequest, SearchStrategy,
};
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::{HeaderMap, LOCATION, RETRY_AFTER, USER_AGENT};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Test-server override. Never required for normal use, and never set by Radish.
pub const BASE_URL_ENV: &str = "RADISH_MUSICBRAINZ_BASE_URL";

pub const USER_AGENT_VALUE: &str = concat!(
    "Radish/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/reveler-hub/radish)"
);

/// Canonical public release links, independent of any test-server override.
pub const RELEASE_URL_PREFIX: &str = "https://musicbrainz.org/release/";

/// A suggested wait longer than this is reported instead of slept.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(120);
/// Stop restarting retry budgets for other releases while the service is down.
const OUTAGE_COOLDOWN: Duration = Duration::from_secs(60);
const BACKOFF_START_SECS: u64 = 2;
const INCLUDES: &str = "recordings+artist-credits+release-groups+labels";

#[derive(Debug, Clone)]
pub struct MusicBrainzConfig {
    pub base_url: Url,
    pub min_interval: Duration,
    pub request_timeout: Duration,
    pub max_attempts: u8,
}

impl Default for MusicBrainzConfig {
    fn default() -> Self {
        Self {
            base_url: Url::parse("https://musicbrainz.org").expect("the MusicBrainz URL is valid"),
            min_interval: Duration::from_millis(1100),
            request_timeout: Duration::from_secs(30),
            max_attempts: 3,
        }
    }
}

pub struct MusicBrainzAdapter {
    client: reqwest::Client,
    config: MusicBrainzConfig,
    /// Monotonic next-slot bookkeeping shared by every actual attempt.
    scheduler: Mutex<Option<Instant>>,
    /// Shared by searches and lookups; successful cache reads bypass this gate.
    cooldown_until: Mutex<Option<tokio::time::Instant>>,
    release_cache: Mutex<HashMap<Mbid, RemoteRelease>>,
    search_cache: Mutex<HashMap<String, SearchPage>>,
}

impl MusicBrainzAdapter {
    /// Production adapter. Only environment *configuration* is read here; no
    /// request is made until a search or lookup runs.
    pub fn new() -> anyhow::Result<Self> {
        let mut config = MusicBrainzConfig::default();
        if let Ok(base) = std::env::var(BASE_URL_ENV)
            && !base.trim().is_empty()
        {
            config.base_url = Url::parse(base.trim())
                .map_err(|error| anyhow::anyhow!("invalid {BASE_URL_ENV}: {error}"))?;
        }
        Self::with_config(config)
    }

    pub fn with_config(config: MusicBrainzConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            // Every hop must be scheduled explicitly, so automatic redirects
            // are disabled.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()?;

        Ok(Self {
            client,
            config,
            scheduler: Mutex::new(None),
            cooldown_until: Mutex::new(None),
            release_cache: Mutex::new(HashMap::new()),
            search_cache: Mutex::new(HashMap::new()),
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.config
            .base_url
            .join(path)
            .map_err(|error| ProviderError::Configuration {
                provider: ProviderKind::MusicBrainz,
                message: format!("cannot build {path}: {error}"),
            })
    }

    /// Waits until this attempt's slot. Setting the next slot from `now` means
    /// no bursts even when a caller arrives late.
    async fn schedule(&self) {
        let wait = {
            let mut last = lock(&self.scheduler);
            let now = Instant::now();
            let wait = match *last {
                Some(previous) => {
                    (previous + self.config.min_interval).saturating_duration_since(now)
                }
                None => Duration::ZERO,
            };
            *last = Some(now + wait);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn start_cooldown(&self, retry_after: Option<Duration>) {
        let delay = retry_after
            .unwrap_or_default()
            .max(OUTAGE_COOLDOWN)
            .min(MAX_RETRY_WAIT);
        let deadline = tokio::time::Instant::now() + delay;
        let mut until = lock(&self.cooldown_until);
        *until = Some(until.map_or(deadline, |previous| previous.max(deadline)));
    }

    fn check_cooldown(&self) -> Result<(), ProviderError> {
        let mut until = lock(&self.cooldown_until);
        let Some(deadline) = *until else {
            return Ok(());
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            *until = None;
            return Ok(());
        }
        let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
        Err(ProviderError::Network {
            provider: ProviderKind::MusicBrainz,
            message: format!(
                "temporarily unavailable after transient failures; retry in {seconds}s"
            ),
        })
    }

    fn is_allowed_redirect(&self, url: &Url) -> bool {
        url.scheme() == self.config.base_url.scheme()
            && url.host_str() == self.config.base_url.host_str()
            && url.port_or_known_default() == self.config.base_url.port_or_known_default()
            && url.path().starts_with("/ws/2/release/")
    }

    async fn request_json(&self, mut url: Url, not_found_id: &str) -> Result<Value, ProviderError> {
        let mut attempt: u8 = 0;
        let mut backoff_index = 0u32;

        loop {
            self.check_cooldown()?;
            attempt += 1;
            self.schedule().await;
            // Another request can exhaust its retries while this one waits.
            self.check_cooldown()?;

            let response = self
                .client
                .get(url.clone())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if attempt >= self.config.max_attempts {
                        if !error.is_builder() {
                            self.start_cooldown(None);
                        }
                        return Err(ProviderError::Network {
                            provider: ProviderKind::MusicBrainz,
                            message: if error.is_timeout() {
                                "request timed out".to_string()
                            } else {
                                error.to_string()
                            },
                        });
                    }
                    tokio::time::sleep(backoff(backoff_index)).await;
                    backoff_index += 1;
                    continue;
                }
            };

            let status = response.status();

            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let Some(location) = location else {
                    return Err(ProviderError::Malformed {
                        provider: ProviderKind::MusicBrainz,
                        message: "redirect response without a Location header".to_string(),
                    });
                };
                let target = url
                    .join(&location)
                    .map_err(|error| ProviderError::Malformed {
                        provider: ProviderKind::MusicBrainz,
                        message: format!("invalid redirect target: {error}"),
                    })?;
                if !self.is_allowed_redirect(&target) {
                    return Err(ProviderError::Permanent {
                        provider: ProviderKind::MusicBrainz,
                        status: Some(status.as_u16()),
                        message: format!(
                            "refusing a redirect outside MusicBrainz releases: {target}"
                        ),
                    });
                }
                if attempt >= self.config.max_attempts {
                    return Err(ProviderError::Network {
                        provider: ProviderKind::MusicBrainz,
                        message: "redirects exhausted the attempt budget".to_string(),
                    });
                }
                url = target;
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE
                || status.is_server_error()
            {
                let retry_after = parse_retry_after(response.headers());
                let delay = retry_after.unwrap_or_default().max(backoff(backoff_index));
                if delay > MAX_RETRY_WAIT {
                    self.start_cooldown(retry_after);
                    return Err(ProviderError::RateLimited {
                        provider: ProviderKind::MusicBrainz,
                        message: "the service asked for a longer wait than Radish allows"
                            .to_string(),
                        retry_after: Some(delay),
                    });
                }
                if attempt >= self.config.max_attempts {
                    self.start_cooldown(retry_after);
                    return Err(ProviderError::Network {
                        provider: ProviderKind::MusicBrainz,
                        message: format!(
                            "HTTP {status} after {} attempts",
                            self.config.max_attempts
                        ),
                    });
                }
                tokio::time::sleep(delay).await;
                backoff_index += 1;
                continue;
            }

            if status == StatusCode::NOT_FOUND {
                return Err(ProviderError::NotFound {
                    provider: ProviderKind::MusicBrainz,
                    id: not_found_id.to_string(),
                });
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(ProviderError::Authentication {
                    provider: ProviderKind::MusicBrainz,
                    message: format!("HTTP {status}"),
                });
            }
            if !status.is_success() {
                return Err(ProviderError::Permanent {
                    provider: ProviderKind::MusicBrainz,
                    status: Some(status.as_u16()),
                    message: "the request failed permanently".to_string(),
                });
            }

            // A connection that drops mid-body is as transient as one that
            // fails before the headers, so it gets the same retry budget.
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => {
                    if attempt >= self.config.max_attempts {
                        self.start_cooldown(None);
                        return Err(ProviderError::Network {
                            provider: ProviderKind::MusicBrainz,
                            message: if error.is_timeout() {
                                "request timed out".to_string()
                            } else {
                                error.to_string()
                            },
                        });
                    }
                    tokio::time::sleep(backoff(backoff_index)).await;
                    backoff_index += 1;
                    continue;
                }
            };

            return serde_json::from_str(&body).map_err(|error| ProviderError::Malformed {
                provider: ProviderKind::MusicBrainz,
                message: format!("response body is not JSON: {error}"),
            });
        }
    }

    fn query_for(&self, strategy: &SearchStrategy) -> Result<String, ProviderError> {
        match strategy {
            SearchStrategy::Barcode(barcode) if !barcode.trim().is_empty() => {
                Ok(literal_field("barcode", barcode))
            }
            SearchStrategy::CatalogNumber(catalog) if !catalog.trim().is_empty() => {
                Ok(literal_field("catno", catalog))
            }
            SearchStrategy::TitleArtist {
                title,
                artist,
                release_group,
            } if !title.trim().is_empty() => {
                // A release group ID already pins the search; ANDing the title
                // phrase on top would only exclude editions titled differently.
                // The UUID has no Lucene metacharacters beyond hyphens, which
                // are harmless inside the quotes.
                if let Some(group) = release_group {
                    return Ok(format!("rgid:\"{group}\""));
                }
                let mut parts = vec![literal_field("release", title)];
                if let Some(artist) = artist.as_deref().filter(|value| !value.trim().is_empty()) {
                    parts.push(literal_field("artist", artist));
                }
                Ok(parts.join(" AND "))
            }
            _ => Err(ProviderError::Permanent {
                provider: ProviderKind::MusicBrainz,
                status: None,
                message: "refusing an unusable empty search term".to_string(),
            }),
        }
    }

    /// The all-terms fallback for a title/artist search whose exact phrase
    /// query found nothing. Other strategies are identifier lookups and are
    /// never loosened.
    fn loosened_query_for(&self, strategy: &SearchStrategy) -> Option<String> {
        let SearchStrategy::TitleArtist { title, artist, .. } = strategy else {
            return None;
        };
        if title.trim().is_empty() {
            return None;
        }
        let mut parts = vec![terms_field("release", title)];
        if let Some(artist) = artist.as_deref().filter(|value| !value.trim().is_empty()) {
            parts.push(terms_field("artist", artist));
        }
        Some(parts.join(" AND "))
    }

    /// One `/ws/2/release` search for an already-built Lucene query.
    async fn search_query(
        &self,
        query: &str,
        request: &SearchRequest,
    ) -> Result<SearchPage, ProviderError> {
        let mut url = self.endpoint("/ws/2/release")?;
        {
            let mut pairs = url.query_pairs_mut();
            // `reqwest` encodes the query parameter exactly once. The Lucene
            // escaping above is separate from this URL encoding.
            pairs.append_pair("query", query);
            pairs.append_pair("fmt", "json");
            pairs.append_pair("limit", &request.limit.to_string());
            pairs.append_pair("offset", &request.offset.to_string());
        }

        let value = self.request_json(url, "search").await?;
        decode_search_page(&value, request.offset)
    }

    async fn fetch_document(&self, id: Mbid) -> Result<RemoteRelease, ProviderError> {
        if let Some(cached) = lock(&self.release_cache).get(&id) {
            return Ok(cached.clone());
        }

        let mut url = self.endpoint(&format!("/ws/2/release/{id}"))?;
        url.query_pairs_mut()
            .append_pair("fmt", "json")
            .append_pair("inc", INCLUDES);

        let value = self.request_json(url, &id.to_string()).await?;
        let mut release = decode_release_document(value)?;

        if release.id != ReleaseId::MusicBrainz(id) {
            release.lookup_aliases.push(ReleaseId::MusicBrainz(id));
        }

        if let Some(canonical) = release.id.mbid() {
            lock(&self.release_cache).insert(canonical, release.clone());
        }

        Ok(release)
    }
}

impl MetadataProvider for MusicBrainzAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::MusicBrainz
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchPage, ProviderError> {
        let key = format!(
            "{:?}|{}|{}",
            request.strategy, request.offset, request.limit
        );
        if let Some(cached) = lock(&self.search_cache).get(&key) {
            return Ok(cached.clone());
        }

        let query = self.query_for(&request.strategy)?;
        let mut page = self.search_query(&query, &request).await?;

        // An exact phrase misses titles with extra words, such as a
        // "(Remastered)" suffix. Try the all-terms form once; its next page
        // cannot be requested through the exact strategy, so none is offered.
        if page.hits.is_empty()
            && request.offset == 0
            && let Some(loosened) = self.loosened_query_for(&request.strategy)
        {
            page = self.search_query(&loosened, &request).await?;
            page.next_offset = None;
        }

        lock(&self.search_cache).insert(key, page.clone());

        Ok(page)
    }

    async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
        let ReleaseId::MusicBrainz(mbid) = id else {
            return Err(ProviderError::Permanent {
                provider: ProviderKind::MusicBrainz,
                status: None,
                message: "MusicBrainz cannot fetch a Discogs release".to_string(),
            });
        };

        self.fetch_document(mbid).await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn backoff(index: u32) -> Duration {
    let exponent = index.min(1);
    Duration::from_secs(BACKOFF_START_SECS << exponent)
}

/// Parses both `Retry-After` forms: delay-seconds and an HTTP date.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim().to_string();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(&raw).ok()?;
    date.duration_since(std::time::SystemTime::now()).ok()
}

/// Escapes Lucene metacharacters inside a quoted literal. Field names are
/// internal constants, never user input.
fn escape_lucene(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '"'
                | '+'
                | '-'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '~'
                | '*'
                | '?'
                | ':'
                | '/'
                | '&'
                | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Builds a literal field query such as `release:"Blue"`.
pub fn literal_field(field: &str, value: &str) -> String {
    format!("{field}:\"{}\"", escape_lucene(value))
}

/// Builds an unquoted all-terms field query such as `release:(Blue AND Moon)`.
///
/// Each whitespace-separated word is escaped on its own. MusicBrainz searches
/// default to OR between bare terms, so the words are joined with an explicit
/// `AND`. A literal upper-case `AND`/`OR`/`NOT` word is lowered so Lucene does
/// not read it as an operator.
fn terms_field(field: &str, value: &str) -> String {
    let terms: Vec<String> = value
        .split_whitespace()
        .map(|word| match word {
            "AND" | "OR" | "NOT" => word.to_ascii_lowercase(),
            _ => escape_lucene(word),
        })
        .collect();
    format!("{field}:({})", terms.join(" AND "))
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn credit_array(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn credits_text(credits: &[Value]) -> Option<String> {
    let mut rendered = String::new();
    for credit in credits {
        if let Some(name) = text(credit, "name") {
            rendered.push_str(&name);
        }
        if let Some(join) = credit.get("joinphrase").and_then(Value::as_str) {
            rendered.push_str(join);
        }
    }
    let trimmed = rendered.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn credit_names(credits: &[Value]) -> Vec<String> {
    credits
        .iter()
        .filter_map(|credit| text(credit, "name"))
        .collect()
}

fn credits_for_metadata(credits: &[Value]) -> Value {
    Value::Array(
        credits
            .iter()
            .map(|credit| {
                json!({
                    "name": text(credit, "name"),
                    "joinphrase": credit.get("joinphrase").and_then(Value::as_str),
                    "artist_id": credit
                        .get("artist")
                        .and_then(|artist| text(artist, "id")),
                })
            })
            .collect(),
    )
}

fn parse_year(date: Option<&str>) -> Option<i64> {
    let date = date?;
    let digits: String = date.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() >= 4 {
        digits[..4].parse().ok()
    } else {
        None
    }
}

fn leading_number(value: &str) -> Option<u32> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn is_unsupported_format(format: &str) -> bool {
    let format = format.to_ascii_lowercase();
    ["dvd", "video", "blu-ray", "bluray", "laserdisc", "vhs"]
        .iter()
        .any(|needle| format.contains(needle))
}

/// Converts one `/ws/2/release` document into the shared model.
///
/// Raw structural findings are preserved as `structure_issues`; nothing is
/// silently flattened. Every field Radish reads is validated explicitly.
pub fn decode_release_document(value: Value) -> Result<RemoteRelease, ProviderError> {
    let malformed = |message: String| ProviderError::Malformed {
        provider: ProviderKind::MusicBrainz,
        message,
    };

    let id_text =
        text(&value, "id").ok_or_else(|| malformed("release document has no id".to_string()))?;
    let id: Mbid = id_text
        .parse()
        .map_err(|error| malformed(format!("release id {id_text} is invalid: {error}")))?;
    let title = text(&value, "title")
        .ok_or_else(|| malformed("release document has no title".to_string()))?;

    let release_credits = credit_array(value.get("artist-credit"));
    let artists = credit_names(release_credits);
    let artist_credit = credits_text(release_credits);

    let release_group = value.get("release-group");
    let group_id = release_group
        .and_then(|group| text(group, "id"))
        .and_then(|raw| raw.parse::<Mbid>().ok())
        .map(GroupId::MusicBrainzReleaseGroup);
    let secondary_types: Vec<String> = release_group
        .and_then(|group| group.get("secondary-types"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let first_release_date = release_group.and_then(|group| text(group, "first-release-date"));

    let date = text(&value, "date");
    let year = parse_year(date.as_deref());

    let mut labels = Vec::new();
    let mut catalog_numbers = Vec::new();
    let mut label_info = Vec::new();
    for info in credit_array(value.get("label-info")) {
        let label = info.get("label").and_then(|label| text(label, "name"));
        let catalog = text(info, "catalog-number");
        if let Some(label) = &label {
            labels.push(label.clone());
        }
        if let Some(catalog) = &catalog {
            catalog_numbers.push(catalog.clone());
        }
        label_info.push(json!({ "label": label, "catalog_number": catalog }));
    }
    labels.dedup();
    catalog_numbers.sort();
    catalog_numbers.dedup();

    let mut release_events = Vec::new();
    let mut country = None;
    for event in credit_array(value.get("release-events")) {
        let event_date = text(event, "date");
        let area = event.get("area").and_then(|area| text(area, "name"));
        if country.is_none() {
            country = area.clone();
        }
        release_events.push(json!({ "date": event_date, "area": area }));
    }

    let barcodes: Vec<String> = text(&value, "barcode").into_iter().collect();
    let status = text(&value, "status");
    let disambiguation = text(&value, "disambiguation");

    let mut structure_issues = Vec::new();
    let mut formats: Vec<String> = Vec::new();
    let mut tracklist: Vec<RemoteTrack> = Vec::new();
    let mut track_credits = Map::new();
    let mut seen_track_ids: HashSet<String> = HashSet::new();

    let media: Vec<&Value> = value
        .get("media")
        .and_then(Value::as_array)
        .map(|media| media.iter().collect())
        .unwrap_or_default();

    if media.is_empty() {
        structure_issues.push("release has no media".to_string());
    }

    let mut ordered_media: Vec<(u32, usize, &Value)> = media
        .iter()
        .enumerate()
        .map(|(index, medium)| {
            let position = medium
                .get("position")
                .and_then(Value::as_u64)
                .map(|value| value as u32)
                .unwrap_or(index as u32 + 1);
            (position, index, *medium)
        })
        .collect();
    ordered_media.sort_by_key(|(position, index, _)| (*position, *index));

    for (medium_position, _, medium) in ordered_media {
        if let Some(format) = text(medium, "format") {
            if is_unsupported_format(&format) {
                structure_issues.push(format!(
                    "medium {medium_position} uses unsupported format '{format}'"
                ));
            }
            if !formats.iter().any(|existing| existing == &format) {
                formats.push(format);
            }
        }

        if medium.get("pregap").is_some_and(|pregap| !pregap.is_null()) {
            structure_issues.push(format!("medium {medium_position} has a pregap track"));
        }

        if medium
            .get("data-tracks")
            .and_then(Value::as_array)
            .is_some_and(|tracks| !tracks.is_empty())
        {
            structure_issues.push(format!("medium {medium_position} has data tracks"));
        }

        let tracks: Vec<&Value> = medium
            .get("tracks")
            .and_then(Value::as_array)
            .map(|tracks| tracks.iter().collect())
            .unwrap_or_default();

        if let Some(declared) = medium.get("track-count").and_then(Value::as_u64)
            && declared as usize != tracks.len()
        {
            structure_issues.push(format!(
                "medium {medium_position} declares {declared} tracks but contains {}",
                tracks.len()
            ));
        }

        for (track_index, track) in tracks.iter().enumerate() {
            let recording = track.get("recording");
            let recording_credits = recording
                .map(|recording| credit_array(recording.get("artist-credit")))
                .unwrap_or(&[]);

            let track_title = text(track, "title")
                .or_else(|| recording.and_then(|recording| text(recording, "title")))
                .unwrap_or_default();

            let number = text(track, "number");
            if number.is_none() {
                structure_issues.push(format!(
                    "medium {medium_position} track {} has no position",
                    track_index + 1
                ));
            }

            let raw_track_id = text(track, "id");
            let release_track_id = raw_track_id
                .as_deref()
                .and_then(|raw| raw.parse::<Mbid>().ok());
            if let Some(raw) = raw_track_id.as_deref()
                && release_track_id.is_none()
            {
                structure_issues.push(format!(
                    "medium {medium_position} track {} has an invalid release-track ID '{raw}'",
                    track_index + 1
                ));
            }
            if let Some(raw) = raw_track_id.as_deref()
                && !seen_track_ids.insert(raw.to_string())
            {
                structure_issues.push(format!(
                    "release-track ID {raw} appears on more than one medium"
                ));
            }

            let recording_id = recording
                .and_then(|recording| text(recording, "id"))
                .and_then(|raw| raw.parse::<Mbid>().ok());

            let (duration_secs, duration_source) =
                if let Some(millis) = track.get("length").and_then(Value::as_u64) {
                    (Some(f64::from(millis as u32) / 1000.0), "track")
                } else if let Some(millis) = recording
                    .and_then(|recording| recording.get("length"))
                    .and_then(Value::as_u64)
                {
                    (Some(f64::from(millis as u32) / 1000.0), "recording")
                } else {
                    (None, "none")
                };

            // Track credit wins over recording credit, then release credit.
            let credits = if !credit_array(track.get("artist-credit")).is_empty() {
                credit_array(track.get("artist-credit"))
            } else if !recording_credits.is_empty() {
                recording_credits
            } else {
                release_credits
            };

            let video = recording
                .and_then(|recording| recording.get("video"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if video {
                structure_issues.push(format!(
                    "medium {medium_position} track {} is a video",
                    track_index + 1
                ));
            }

            if let Some(track_id) = release_track_id {
                track_credits.insert(
                    track_id.to_string(),
                    json!({
                        "artist_credits": credits_for_metadata(credits),
                        "duration_source": duration_source,
                    }),
                );
            }

            tracklist.push(RemoteTrack {
                position: number.clone(),
                title: track_title,
                artists: credit_names(credits),
                duration_secs,
                entry_type: if video {
                    TrackEntryType::Other
                } else {
                    TrackEntryType::Track
                },
                medium_position: Some(medium_position),
                // `position` is the numeric order MusicBrainz always sends; the
                // free-text `number` ("A1") only has a leading number on CD.
                track_position: track
                    .get("position")
                    .and_then(Value::as_u64)
                    .and_then(|position| u32::try_from(position).ok())
                    .or_else(|| number.as_deref().and_then(leading_number)),
                release_track_id,
                recording_id,
                artist_credit: credits_text(credits),
            });
        }
    }

    let mut edition_notes = Vec::new();
    if let Some(disambiguation) = &disambiguation {
        edition_notes.push(disambiguation.clone());
    }
    edition_notes.extend(secondary_types.iter().cloned());

    let url = format!("{RELEASE_URL_PREFIX}{id}");

    let provider_metadata = json!({
        "schema_version": 1,
        "release_date": date,
        "first_release_date": first_release_date,
        "status": status,
        "disambiguation": disambiguation,
        "artist_credits": credits_for_metadata(release_credits),
        "label_info": label_info,
        "release_events": release_events,
        "track_credits": Value::Object(track_credits),
    });

    Ok(RemoteRelease {
        id: ReleaseId::MusicBrainz(id),
        group_id,
        title,
        artists,
        year,
        country,
        labels,
        catalog_numbers,
        formats,
        barcodes,
        tracklist,
        url,
        lookup_aliases: Vec::new(),
        artist_credit,
        structure_issues,
        edition_notes,
        provider_metadata,
    })
}

fn decode_search_page(value: &Value, offset: u16) -> Result<SearchPage, ProviderError> {
    let total = value.get("count").and_then(Value::as_u64);
    let releases = value
        .get("releases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();

    let mut hits = Vec::new();
    for release in releases {
        let Some(raw_id) = text(release, "id") else {
            continue;
        };
        let Ok(id) = raw_id.parse::<Mbid>() else {
            continue;
        };
        let title = text(release, "title").unwrap_or_default();
        let credits = credit_array(release.get("artist-credit"));
        let mut labels = Vec::new();
        let mut catalog_numbers = Vec::new();
        for info in credit_array(release.get("label-info")) {
            if let Some(label) = info.get("label").and_then(|label| text(label, "name")) {
                labels.push(label);
            }
            if let Some(catalog) = text(info, "catalog-number") {
                catalog_numbers.push(catalog);
            }
        }
        catalog_numbers.sort();
        catalog_numbers.dedup();

        let media = credit_array(release.get("media"));
        let mut formats: Vec<String> = Vec::new();
        for format in media.iter().filter_map(|medium| text(medium, "format")) {
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        let media_tracks: Vec<u64> = media
            .iter()
            .filter_map(|medium| medium.get("track-count").and_then(Value::as_u64))
            .collect();
        let track_count = release
            .get("track-count")
            .and_then(Value::as_u64)
            .or_else(|| (!media_tracks.is_empty()).then(|| media_tracks.iter().sum()))
            .and_then(|count| u32::try_from(count).ok());

        hits.push(SearchSummary {
            id: ReleaseId::MusicBrainz(id),
            title,
            artists: credit_names(credits),
            artist_credit: credits_text(credits),
            year: parse_year(text(release, "date").as_deref()),
            country: text(release, "country"),
            labels,
            catalog_numbers,
            formats,
            barcodes: text(release, "barcode").into_iter().collect(),
            result_type: "release".to_string(),
            track_count,
        });
    }

    let next_offset = match total {
        Some(total) if !hits.is_empty() => {
            let next = u64::from(offset) + hits.len() as u64;
            if next < total {
                Some(next.min(u64::from(u16::MAX)) as u16)
            } else {
                None
            }
        }
        _ => None,
    };

    Ok(SearchPage {
        hits,
        total,
        next_offset,
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

    struct MockServer {
        url: String,
        shutdown: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<(String, Instant)>>>,
    }

    impl MockServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&str) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
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
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }

                    let target = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    recorded
                        .lock()
                        .unwrap()
                        .push((target.clone(), Instant::now()));

                    let (status, headers, body) = handler(&target);
                    let mut payload = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        payload.push_str(&format!("{name}: {value}\r\n"));
                    }
                    payload.push_str("\r\n");
                    payload.push_str(&body);
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

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn timestamps(&self) -> Vec<Instant> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, at)| *at)
                .collect()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    fn config(server: &MockServer) -> MusicBrainzConfig {
        MusicBrainzConfig {
            base_url: Url::parse(&server.url).unwrap(),
            min_interval: Duration::ZERO,
            request_timeout: Duration::from_secs(5),
            max_attempts: 3,
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/musicbrainz/release-single.json"
        ))
        .unwrap()
    }

    fn multidisc() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/musicbrainz/release-multidisc.json"
        ))
        .unwrap()
    }

    fn release_id() -> ReleaseId {
        ReleaseId::MusicBrainz("76df3287-6cda-33eb-8e9a-044b5e15ffdd".parse().unwrap())
    }

    fn search_request(title: &str) -> SearchRequest {
        SearchRequest {
            strategy: SearchStrategy::TitleArtist {
                title: title.to_string(),
                artist: None,
                release_group: None,
            },
            offset: 0,
            limit: 50,
        }
    }

    async fn expect_cooldown<T: std::fmt::Debug>(
        request: impl std::future::Future<Output = Result<T, ProviderError>>,
    ) {
        let before = tokio::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_millis(1), request)
            .await
            .expect("cooldown must return immediately")
            .unwrap_err();
        assert!(matches!(error, ProviderError::Network { .. }), "{error}");
        assert!(
            error.to_string().contains("temporarily unavailable"),
            "{error}"
        );
        assert_eq!(tokio::time::Instant::now(), before);
    }

    #[test]
    fn literal_queries_do_not_treat_titles_as_query_operators() {
        assert_eq!(literal_field("release", "Blue"), "release:\"Blue\"");
        assert_eq!(literal_field("release", "a\"b"), "release:\"a\\\"b\"");
        assert_eq!(literal_field("release", "A+B"), "release:\"A\\+B\"");
        assert_eq!(
            literal_field("release", "(Live) & More: Part/2"),
            "release:\"\\(Live\\) \\& More\\: Part\\/2\""
        );
    }

    #[test]
    fn converts_release_fields_media_and_credits() {
        let release = decode_release_document(fixture()).unwrap();
        assert_eq!(release.id, release_id());
        assert_eq!(release.title, "Blue");
        assert_eq!(release.year, Some(2001));
        assert_eq!(release.country.as_deref(), Some("United States"));
        assert_eq!(release.labels, vec!["Test Label".to_string()]);
        assert_eq!(release.catalog_numbers, vec!["CAT-1".to_string()]);
        assert_eq!(release.barcodes, vec!["0123456789012".to_string()]);
        assert_eq!(release.artist_display(), "Test Artist");
        assert_eq!(release.structure_issues, Vec::<String>::new());
        assert_eq!(release.tracklist.len(), 2);
        assert_eq!(release.tracklist[0].medium_position, Some(1));
        assert_eq!(release.tracklist[0].track_position, Some(1));
        assert_eq!(
            release.tracklist[0].release_track_id.unwrap().to_string(),
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(
            release.tracklist[1].recording_id.unwrap().to_string(),
            "ddddddd2-dddd-4ddd-8ddd-ddddddddddd2"
        );
        // Track duration wins over the recording duration.
        assert_eq!(release.tracklist[1].duration_secs, Some(181.0));
        assert_eq!(
            release.group_id,
            Some(GroupId::MusicBrainzReleaseGroup(
                "4642ee19-7790-3c8d-ab5e-d133de942db6".parse().unwrap()
            ))
        );
        assert_eq!(release.provider_metadata["schema_version"], Value::from(1));
        assert_eq!(
            release.provider_metadata["release_date"],
            Value::from("2001-05-01")
        );
        assert_eq!(
            release.provider_metadata["first_release_date"],
            Value::from("2001")
        );
        assert!(
            release.provider_metadata["track_credits"]
                .get("11111111-1111-1111-1111-111111111111")
                .is_some()
        );
        assert_eq!(
            release.url,
            format!("{RELEASE_URL_PREFIX}{}", release_id().storage_id())
        );
        assert!(!release.has_unsupported_entries());
    }

    #[test]
    fn keeps_numeric_positions_for_vinyl_and_cassette_side_numbers() {
        let mut value = fixture();
        value["media"][0]["format"] = Value::from("12\" Vinyl");
        value["media"][0]["tracks"][0]["number"] = Value::from("A1");
        value["media"][0]["tracks"][0]["position"] = Value::from(1);
        value["media"][0]["tracks"][1]["number"] = Value::from("B1");
        value["media"][0]["tracks"][1]["position"] = Value::from(2);
        let release = decode_release_document(value).unwrap();
        assert_eq!(release.tracklist[0].position.as_deref(), Some("A1"));
        assert_eq!(release.tracklist[0].track_position, Some(1));
        assert_eq!(release.tracklist[1].track_position, Some(2));
    }

    #[test]
    fn falls_back_to_the_leading_number_without_a_position_field() {
        let mut value = fixture();
        value["media"][0]["tracks"][1]
            .as_object_mut()
            .unwrap()
            .remove("position");
        let release = decode_release_document(value).unwrap();
        assert_eq!(release.tracklist[1].track_position, Some(2));
    }

    #[test]
    fn keeps_partial_dates_and_repeated_recordings_across_media() {
        let release = decode_release_document(multidisc()).unwrap();
        assert_eq!(release.year, Some(2001));
        assert_eq!(release.tracklist.len(), 3);
        assert_eq!(release.tracklist[0].medium_position, Some(1));
        assert_eq!(release.tracklist[1].medium_position, Some(2));
        assert_eq!(
            release.tracklist[0].recording_id,
            release.tracklist[1].recording_id
        );
        assert_ne!(
            release.tracklist[0].release_track_id,
            release.tracklist[1].release_track_id
        );
        assert!(
            release
                .edition_notes
                .iter()
                .any(|note| note == "Compilation")
        );
        // The group's first-release year is not substituted for the edition.
        assert_eq!(
            release.provider_metadata["first_release_date"],
            Value::from("1999")
        );
        assert_eq!(release.year, Some(2001));
    }

    #[test]
    fn preserves_blocked_structures_instead_of_flattening_them() {
        let mut value = fixture();
        value["media"][0]["pregap"] = json!({ "number": "0", "title": "Pregap" });
        value["media"][0]["data-tracks"] = json!([{ "number": "3", "title": "Data" }]);
        value["media"][0]["track-count"] = Value::from(4);
        value["media"][0]["tracks"][1]["id"] = Value::from("11111111-1111-1111-1111-111111111111");
        let release = decode_release_document(value).unwrap();
        assert!(release.has_unsupported_entries());
        assert!(
            release
                .structure_issues
                .iter()
                .any(|issue| issue.contains("pregap"))
        );
        assert!(
            release
                .structure_issues
                .iter()
                .any(|issue| issue.contains("data tracks"))
        );
        assert!(
            release
                .structure_issues
                .iter()
                .any(|issue| issue.contains("declares 4 tracks"))
        );
        assert!(
            release
                .structure_issues
                .iter()
                .any(|issue| issue.contains("more than one medium"))
        );
    }

    #[test]
    fn rejects_documents_without_a_valid_release_identity() {
        let mut value = fixture();
        value["id"] = Value::from("42");
        assert!(matches!(
            decode_release_document(value),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[tokio::test]
    async fn fetches_releases_and_caches_successful_documents() {
        let body = include_str!("../tests/fixtures/musicbrainz/release-single.json").to_string();
        let server = MockServer::start(move |_| (200, Vec::new(), body.clone()));
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let first = adapter.fetch_release(release_id()).await.unwrap();
        let second = adapter.fetch_release(release_id()).await.unwrap();
        assert_eq!(first.title, second.title);
        assert_eq!(server.request_count(), 1);

        let target = server.targets().remove(0);
        assert!(target.starts_with("/ws/2/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd"));
        assert!(target.contains("fmt=json"));
        assert!(target.contains("inc=recordings"));
        assert!(target.contains("artist-credits"));
        assert!(target.contains("release-groups"));
    }

    #[tokio::test]
    async fn sends_literal_queries_through_one_encoding() {
        let server = MockServer::start(|_| {
            (
                200,
                Vec::new(),
                r#"{"count":1,"offset":0,"releases":[{"id":"76df3287-6cda-33eb-8e9a-044b5e15ffdd","title":"Blue","date":"2001-05-01","artist-credit":[{"name":"Test Artist","joinphrase":""}]}]}"#
                    .to_string(),
            )
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let request = SearchRequest {
            strategy: SearchStrategy::TitleArtist {
                title: "A+B & (C)".to_string(),
                artist: Some("Test: Artist".to_string()),
                release_group: None,
            },
            offset: 0,
            limit: 50,
        };
        let page = adapter.search(request).await.unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].id, release_id());
        assert_eq!(page.total, Some(1));
        assert!(page.next_offset.is_none());

        let target = server.targets().remove(0);
        let url = Url::parse("http://test.local")
            .unwrap()
            .join(&target)
            .unwrap();
        let query = url
            .query_pairs()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| value.to_string())
            .unwrap();
        assert_eq!(
            query,
            "release:\"A\\+B \\& \\(C\\)\" AND artist:\"Test\\: Artist\""
        );
    }

    #[tokio::test]
    async fn classifies_permanent_and_missing_releases() {
        let server = MockServer::start(|target| {
            if target.contains("99999999-9999-4999-8999-999999999999") {
                (404, Vec::new(), "{}".to_string())
            } else {
                (400, Vec::new(), "{}".to_string())
            }
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let missing: ReleaseId =
            ReleaseId::MusicBrainz("99999999-9999-4999-8999-999999999999".parse().unwrap());
        assert!(matches!(
            adapter.fetch_release(missing).await,
            Err(ProviderError::NotFound { .. })
        ));

        let other: ReleaseId =
            ReleaseId::MusicBrainz("88888888-8888-4888-8888-888888888888".parse().unwrap());
        assert!(matches!(
            adapter.fetch_release(other).await,
            Err(ProviderError::Permanent {
                status: Some(400),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn retries_throttled_requests_with_retry_after() {
        let attempts = Arc::new(Mutex::new(0usize));
        let counter = attempts.clone();
        let body = include_str!("../tests/fixtures/musicbrainz/release-single.json").to_string();
        let server = MockServer::start(move |_| {
            let mut attempts = counter.lock().unwrap();
            *attempts += 1;
            if *attempts == 1 {
                (
                    503,
                    vec![("Retry-After".to_string(), "0".to_string())],
                    "{}".to_string(),
                )
            } else {
                (200, Vec::new(), body.clone())
            }
        });
        let config = MusicBrainzConfig {
            min_interval: Duration::ZERO,
            ..config(&server)
        };
        let adapter = MusicBrainzAdapter::with_config(config).unwrap();

        adapter.fetch_release(release_id()).await.unwrap();
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn exhausted_outage_skips_uncached_requests_until_cooldown_expires() {
        let attempts = Arc::new(Mutex::new(0usize));
        let counter = attempts.clone();
        let body = fixture().to_string();
        let server = MockServer::start(move |_| {
            let mut attempts = counter.lock().unwrap();
            *attempts += 1;
            if *attempts <= 3 {
                (503, Vec::new(), "{}".to_string())
            } else {
                (200, Vec::new(), body.clone())
            }
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let error = adapter.fetch_release(release_id()).await.unwrap_err();
        assert!(error.to_string().contains("after 3 attempts"), "{error}");
        assert_eq!(server.request_count(), 3);

        tokio::time::pause();
        expect_cooldown(adapter.search(search_request("Another album"))).await;
        let other = ReleaseId::MusicBrainz("99999999-9999-4999-8999-999999999999".parse().unwrap());
        expect_cooldown(adapter.fetch_release(other)).await;
        tokio::time::advance(Duration::from_secs(59)).await;
        expect_cooldown(adapter.fetch_release(release_id())).await;
        assert_eq!(server.request_count(), 3);

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        assert_eq!(
            adapter.fetch_release(release_id()).await.unwrap().title,
            "Blue"
        );
        assert_eq!(server.request_count(), 4);
    }

    #[tokio::test]
    async fn exhausted_rate_limits_and_server_errors_honor_retry_after_during_cooldown() {
        for status in [429, 500] {
            let failed = Arc::new(AtomicBool::new(false));
            let flag = failed.clone();
            let body = fixture().to_string();
            let server = MockServer::start(move |_| {
                if !flag.swap(true, Ordering::SeqCst) {
                    (
                        status,
                        vec![("Retry-After".to_string(), "90".to_string())],
                        "{}".to_string(),
                    )
                } else {
                    (200, Vec::new(), body.clone())
                }
            });
            let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig {
                max_attempts: 1,
                ..config(&server)
            })
            .unwrap();

            let error = adapter.fetch_release(release_id()).await.unwrap_err();
            assert!(matches!(error, ProviderError::Network { .. }), "{error}");
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(89)).await;
            expect_cooldown(adapter.fetch_release(release_id())).await;
            assert_eq!(server.request_count(), 1);

            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::time::resume();
            adapter.fetch_release(release_id()).await.unwrap();
            assert_eq!(server.request_count(), 2);
        }
    }

    #[tokio::test]
    async fn oversized_retry_after_starts_capped_cooldown_without_sleeping() {
        let future_date =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(600));
        for (status, retry_after, minimum_delay) in [
            (429, "121".to_string(), Duration::from_secs(121)),
            (503, "600".to_string(), Duration::from_secs(600)),
            (503, future_date, Duration::from_secs(598)),
        ] {
            let failed = Arc::new(AtomicBool::new(false));
            let flag = failed.clone();
            let body = fixture().to_string();
            let server = MockServer::start(move |_| {
                if !flag.swap(true, Ordering::SeqCst) {
                    (
                        status,
                        vec![("Retry-After".to_string(), retry_after.clone())],
                        "{}".to_string(),
                    )
                } else {
                    (200, Vec::new(), body.clone())
                }
            });
            let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

            let error =
                tokio::time::timeout(Duration::from_secs(1), adapter.fetch_release(release_id()))
                    .await
                    .expect("oversized Retry-After must be reported without sleeping")
                    .unwrap_err();
            match error {
                ProviderError::RateLimited {
                    retry_after: Some(delay),
                    ..
                } => assert!(delay >= minimum_delay),
                other => panic!("expected the original rate-limit error, got {other}"),
            }
            assert_eq!(server.request_count(), 1);

            tokio::time::pause();
            expect_cooldown(adapter.search(search_request("Another album"))).await;
            tokio::time::advance(Duration::from_secs(119)).await;
            expect_cooldown(adapter.fetch_release(release_id())).await;
            assert_eq!(server.request_count(), 1);

            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::time::resume();
            adapter.fetch_release(release_id()).await.unwrap();
            assert_eq!(server.request_count(), 2);
        }
    }

    #[tokio::test]
    async fn successful_searches_and_documents_remain_cached_during_outages() {
        let unavailable = Arc::new(AtomicBool::new(false));
        let flag = unavailable.clone();
        let body = fixture().to_string();
        let server = MockServer::start(move |target| {
            if flag.load(Ordering::SeqCst) {
                (503, Vec::new(), "{}".to_string())
            } else if target.starts_with("/ws/2/release?") {
                (
                    200,
                    Vec::new(),
                    json!({"count": 1, "releases": [fixture()]}).to_string(),
                )
            } else {
                (200, Vec::new(), body.clone())
            }
        });
        let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig {
            max_attempts: 1,
            ..config(&server)
        })
        .unwrap();
        adapter.fetch_release(release_id()).await.unwrap();
        adapter.search(search_request("Blue")).await.unwrap();
        unavailable.store(true, Ordering::SeqCst);
        assert!(adapter.search(search_request("Uncached")).await.is_err());
        assert_eq!(server.request_count(), 3);

        tokio::time::pause();
        expect_cooldown(adapter.search(search_request("Uncached"))).await;
        assert_eq!(
            adapter.fetch_release(release_id()).await.unwrap().title,
            "Blue"
        );
        let page = adapter.search(search_request("Blue")).await.unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].id, release_id());
        assert_eq!(server.request_count(), 3);

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        unavailable.store(false, Ordering::SeqCst);
        assert_eq!(
            adapter
                .search(search_request("Uncached"))
                .await
                .unwrap()
                .hits
                .len(),
            1
        );
        assert_eq!(server.request_count(), 4);
    }

    #[tokio::test]
    async fn missing_permanent_malformed_and_redirect_errors_do_not_start_outage_cooldown() {
        for (status, body, headers) in [
            (404, "{}", Vec::new()),
            (400, "{}", Vec::new()),
            (401, "{}", Vec::new()),
            (403, "{}", Vec::new()),
            (200, "not json", Vec::new()),
            (200, "{}", Vec::new()),
            (
                302,
                "",
                vec![(
                    "Location".to_string(),
                    format!("/ws/2/release/{}", release_id().storage_id()),
                )],
            ),
        ] {
            let failed = Arc::new(AtomicBool::new(false));
            let flag = failed.clone();
            let success = fixture().to_string();
            let server = MockServer::start(move |_| {
                if !flag.swap(true, Ordering::SeqCst) {
                    (status, headers.clone(), body.to_string())
                } else {
                    (200, Vec::new(), success.clone())
                }
            });
            let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig {
                max_attempts: 1,
                ..config(&server)
            })
            .unwrap();

            assert!(adapter.fetch_release(release_id()).await.is_err());
            assert_eq!(
                adapter.fetch_release(release_id()).await.unwrap().title,
                "Blue"
            );
            assert_eq!(server.request_count(), 2);
        }
    }

    #[tokio::test]
    async fn follows_same_service_redirects_and_retains_aliases() {
        let requested = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
        let canonical = "12121212-1212-4212-8212-121212121212";
        let body = include_str!("../tests/fixtures/musicbrainz/release-single.json")
            .replace(requested, canonical);
        let server = MockServer::start(move |target| {
            if target.contains(requested) {
                (
                    302,
                    vec![(
                        "Location".to_string(),
                        format!("/ws/2/release/{canonical}?fmt=json"),
                    )],
                    String::new(),
                )
            } else {
                (200, Vec::new(), body.clone())
            }
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let release = adapter.fetch_release(release_id()).await.unwrap();
        assert_eq!(release.id.storage_id(), canonical);
        assert_eq!(release.lookup_aliases, vec![release_id()]);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn spaces_requests_including_redirect_hops() {
        let requested = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
        let canonical = "12121212-1212-4212-8212-121212121212";
        let body = include_str!("../tests/fixtures/musicbrainz/release-single.json")
            .replace(requested, canonical);
        let server = MockServer::start(move |target| {
            if target.contains(requested) {
                (
                    302,
                    vec![(
                        "Location".to_string(),
                        format!("/ws/2/release/{canonical}?fmt=json"),
                    )],
                    String::new(),
                )
            } else {
                (200, Vec::new(), body.clone())
            }
        });
        let config = MusicBrainzConfig {
            min_interval: Duration::from_millis(80),
            ..config(&server)
        };
        let adapter = MusicBrainzAdapter::with_config(config).unwrap();

        adapter.fetch_release(release_id()).await.unwrap();
        let timestamps = server.timestamps();
        assert_eq!(timestamps.len(), 2);
        assert!(
            timestamps[1].duration_since(timestamps[0]) >= Duration::from_millis(80),
            "redirect bypassed request spacing"
        );
    }

    #[tokio::test]
    async fn reports_malformed_success_bodies_without_retrying() {
        let server = MockServer::start(|_| (200, Vec::new(), "not json".to_string()));
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        let error = adapter.fetch_release(release_id()).await.unwrap_err();
        assert!(matches!(error, ProviderError::Malformed { .. }), "{error}");
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn times_out_without_unbounded_attempts() {
        let server = MockServer::start(|_| {
            thread::sleep(Duration::from_millis(200));
            (200, Vec::new(), "{}".to_string())
        });
        let config = MusicBrainzConfig {
            request_timeout: Duration::from_millis(40),
            max_attempts: 1,
            ..config(&server)
        };
        let adapter = MusicBrainzAdapter::with_config(config).unwrap();

        let error = adapter.fetch_release(release_id()).await.unwrap_err();
        assert!(matches!(error, ProviderError::Network { .. }), "{error}");
        assert_eq!(server.request_count(), 1);
        tokio::time::pause();
        expect_cooldown(adapter.search(search_request("Another album"))).await;
        assert_eq!(server.request_count(), 1);
    }

    /// Serves one truncated body (headers promise more than is sent, then the
    /// socket closes) followed by complete responses. Returns the URL and the
    /// attempt counter.
    fn start_truncating_server(complete_body: String) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind truncating server");
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(0usize));
        let counter = attempts.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let attempt = {
                    let mut attempts = counter.lock().unwrap();
                    *attempts += 1;
                    *attempts
                };
                let payload = if attempt == 1 {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{\"id\":"
                        .to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{complete_body}",
                        complete_body.len()
                    )
                };
                let _ = stream.write_all(payload.as_bytes());
                // Dropping the stream closes it mid-body on the first attempt.
            }
        });
        (format!("http://{address}"), attempts)
    }

    #[tokio::test]
    async fn retries_failures_while_reading_the_response_body() {
        let body = include_str!("../tests/fixtures/musicbrainz/release-single.json").to_string();
        let (url, attempts) = start_truncating_server(body);
        let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig {
            base_url: Url::parse(&url).unwrap(),
            min_interval: Duration::ZERO,
            request_timeout: Duration::from_secs(5),
            max_attempts: 3,
        })
        .unwrap();

        let release = adapter.fetch_release(release_id()).await.unwrap();
        assert_eq!(release.title, "Blue");
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    fn query_param(target: &str) -> String {
        Url::parse("http://test.local")
            .unwrap()
            .join(target)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| value.to_string())
            .unwrap()
    }

    const EMPTY_SEARCH: &str = r#"{"count":0,"offset":0,"releases":[]}"#;
    const ONE_HIT_SEARCH: &str = r#"{"count":1,"offset":0,"releases":[{"id":"76df3287-6cda-33eb-8e9a-044b5e15ffdd","title":"Abbey Road","artist-credit":[{"name":"The Beatles","joinphrase":""}]}]}"#;

    #[test]
    fn search_summaries_carry_formats_and_track_counts() {
        let page = decode_search_page(
            &json!({
                "count": 2,
                "releases": [
                    {
                        "id": "76df3287-6cda-33eb-8e9a-044b5e15ffdd",
                        "title": "Blue",
                        "track-count": 12,
                        "media": [
                            { "format": "CD", "track-count": 7 },
                            { "format": "CD", "track-count": 5 },
                            { "format": "DVD-Video", "track-count": 0 }
                        ]
                    },
                    {
                        "id": "12121212-1212-4212-8212-121212121212",
                        "title": "Blue",
                        "media": [{ "track-count": 9 }]
                    }
                ]
            }),
            0,
        )
        .unwrap();
        assert_eq!(page.hits[0].formats, vec!["CD", "DVD-Video"]);
        assert_eq!(page.hits[0].track_count, Some(12));
        // Without a release-level count, the media counts are summed.
        assert!(page.hits[1].formats.is_empty());
        assert_eq!(page.hits[1].track_count, Some(9));
    }

    #[test]
    fn known_release_group_is_searched_on_its_own() {
        let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig::default()).unwrap();
        let query = adapter
            .query_for(&SearchStrategy::TitleArtist {
                title: "Blue".to_string(),
                artist: Some("Test Artist".to_string()),
                release_group: Some("4642ee19-7790-3c8d-ab5e-d133de942db6".parse().unwrap()),
            })
            .unwrap();
        assert_eq!(query, "rgid:\"4642ee19-7790-3c8d-ab5e-d133de942db6\"");
    }

    #[test]
    fn term_queries_require_every_escaped_word() {
        assert_eq!(
            terms_field("release", "  Abbey Road (Remastered) "),
            "release:(Abbey AND Road AND \\(Remastered\\))"
        );
        // Lucene only treats upper-case words as operators.
        assert_eq!(terms_field("artist", "This AND That"), "artist:(This AND and AND That)");
    }

    #[tokio::test]
    async fn loosens_title_artist_search_once_when_the_exact_phrase_finds_nothing() {
        let server = MockServer::start(|target| {
            let body = if query_param(target).contains("release:\"") {
                EMPTY_SEARCH
            } else {
                ONE_HIT_SEARCH
            };
            (200, Vec::new(), body.to_string())
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();
        let request = SearchRequest {
            strategy: SearchStrategy::TitleArtist {
                title: "Abbey Road Remastered".to_string(),
                artist: Some("The Beatles".to_string()),
                release_group: None,
            },
            offset: 0,
            limit: 50,
        };

        let page = adapter.search(request.clone()).await.unwrap();
        assert_eq!(page.hits.len(), 1);

        let queries: Vec<String> = server.targets().iter().map(|t| query_param(t)).collect();
        assert_eq!(
            queries,
            vec![
                "release:\"Abbey Road Remastered\" AND artist:\"The Beatles\"".to_string(),
                "release:(Abbey AND Road AND Remastered) AND artist:(The AND Beatles)".to_string(),
            ]
        );

        // The loosened result is cached under the original request.
        adapter.search(request).await.unwrap();
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn does_not_loosen_searches_that_have_hits_or_are_identifier_lookups() {
        let server = MockServer::start(|target| {
            let body = if query_param(target).starts_with("barcode:") {
                EMPTY_SEARCH
            } else {
                ONE_HIT_SEARCH
            };
            (200, Vec::new(), body.to_string())
        });
        let adapter = MusicBrainzAdapter::with_config(config(&server)).unwrap();

        adapter.search(search_request("Abbey Road")).await.unwrap();
        assert_eq!(server.request_count(), 1);

        let barcode = SearchRequest {
            strategy: SearchStrategy::Barcode("0123456789012".to_string()),
            offset: 0,
            limit: 50,
        };
        let page = adapter.search(barcode).await.unwrap();
        assert!(page.hits.is_empty());
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn refuses_empty_search_terms_locally() {
        let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig::default()).unwrap();
        let error = adapter
            .query_for(&SearchStrategy::TitleArtist {
                title: "   ".to_string(),
                artist: None,
                release_group: None,
            })
            .unwrap_err();
        assert!(matches!(error, ProviderError::Permanent { .. }), "{error}");
    }

    #[test]
    fn validates_redirect_authority() {
        let adapter = MusicBrainzAdapter::with_config(MusicBrainzConfig::default()).unwrap();
        assert!(
            adapter.is_allowed_redirect(
                &Url::parse("https://musicbrainz.org/ws/2/release/abc").unwrap()
            )
        );
        assert!(
            !adapter
                .is_allowed_redirect(&Url::parse("https://evil.example/ws/2/release/abc").unwrap())
        );
        assert!(
            !adapter.is_allowed_redirect(
                &Url::parse("https://musicbrainz.org/ws/2/artist/abc").unwrap()
            )
        );
    }
}
