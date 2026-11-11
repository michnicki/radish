//! Provider identities and the small shared interface every metadata service
//! implements.
//!
//! Nothing in this module knows about Discogs or MusicBrainz payloads: the
//! adapters convert their own responses into the Radish-owned models in
//! [`crate::matcher`]. Identities stay typed here and become
//! `(provider, TEXT id)` pairs in SQLite.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Most release documents an adapter keeps for one run. A library import
/// touches far more releases than it revisits, so an unbounded map would only
/// grow; this covers the repeats that occur close together.
pub const RELEASE_CACHE_CAPACITY: usize = 256;
/// Most search pages an adapter keeps for one run (same reasoning).
pub const SEARCH_CACHE_CAPACITY: usize = 256;

/// Locks a mutex, recovering the data if a panicking holder poisoned it.
///
/// Every guarded value here is plain bookkeeping that stays consistent after a
/// panic, so continuing is safe.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A small least-recently-used cache: a map plus a recency queue.
///
/// Lookups and inserts are O(capacity) because the queue is scanned to refresh
/// an entry. That is deliberate; the capacities here are a few hundred and each
/// hit saves a rate-limited network request.
#[derive(Debug)]
pub struct BoundedCache<K, V> {
    capacity: usize,
    /// Least recently used first.
    order: VecDeque<K>,
    entries: HashMap<K, V>,
}

impl<K: Eq + Hash + Clone, V: Clone> BoundedCache<K, V> {
    /// A cache holding at most `capacity` entries (at least one).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns a copy of the entry and marks it most recently used.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let value = self.entries.get(key)?.clone();
        self.touch(key);
        Some(value)
    }

    /// Stores the entry as most recently used, evicting the least recently
    /// used one when the cache is full.
    pub fn insert(&mut self, key: K, value: V) {
        if self.entries.insert(key.clone(), value).is_some() {
            self.touch(&key);
            return;
        }
        self.order.push_back(key);
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn touch(&mut self, key: &K) {
        if let Some(position) = self.order.iter().position(|existing| existing == key)
            && let Some(key) = self.order.remove(position)
        {
            self.order.push_back(key);
        }
    }
}

/// A metadata service Radish can match against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Discogs,
    #[serde(rename = "musicbrainz")]
    MusicBrainz,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discogs => "discogs",
            Self::MusicBrainz => "musicbrainz",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "discogs" => Some(Self::Discogs),
            "musicbrainz" => Some(Self::MusicBrainz),
            _ => None,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl clap::ValueEnum for ProviderKind {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Discogs, Self::MusicBrainz]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

/// Which providers a matching run may contact, and in what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderPolicy {
    /// MusicBrainz first, then Discogs only when the primary stage cannot
    /// settle the release and a token is available.
    #[default]
    MusicBrainzThenDiscogs,
    /// Exactly one service; fallback and the other factory stay unused.
    Only(ProviderKind),
}

impl ProviderPolicy {
    /// Providers in the order they may be attempted.
    pub fn order(self) -> Vec<ProviderKind> {
        match self {
            Self::MusicBrainzThenDiscogs => {
                vec![ProviderKind::MusicBrainz, ProviderKind::Discogs]
            }
            Self::Only(provider) => vec![provider],
        }
    }

    pub fn is_explicit(self) -> bool {
        matches!(self, Self::Only(_))
    }

    pub fn allows(self, provider: ProviderKind) -> bool {
        match self {
            Self::MusicBrainzThenDiscogs => true,
            Self::Only(selected) => selected == provider,
        }
    }
}

/// A MusicBrainz identifier: a hyphenated, non-nil UUID.
///
/// Parsing is strict about shape (the compact 32-character form is rejected)
/// but accepts uppercase; the canonical form is lowercase. Nil identifiers are
/// never valid evidence and are rejected everywhere, including snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mbid(uuid::Uuid);

impl Mbid {
    pub fn new(uuid: uuid::Uuid) -> anyhow::Result<Self> {
        if uuid.is_nil() {
            anyhow::bail!("a nil UUID is not a valid MusicBrainz identifier");
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for Mbid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn has_hyphenated_uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.as_bytes().iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

impl FromStr for Mbid {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if !has_hyphenated_uuid_shape(trimmed) {
            anyhow::bail!("'{trimmed}' is not a hyphenated MusicBrainz UUID");
        }
        let uuid = uuid::Uuid::parse_str(trimmed)
            .map_err(|error| anyhow::anyhow!("'{trimmed}' is not a valid UUID: {error}"))?;
        Self::new(uuid)
    }
}

impl TryFrom<String> for Mbid {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Mbid> for String {
    fn from(value: Mbid) -> Self {
        value.to_string()
    }
}

impl Serialize for Mbid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Mbid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// A concrete release on a specific service. Only a release identifies an
/// edition; a group/master does not.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "provider", content = "id", rename_all = "snake_case")]
pub enum ReleaseId {
    Discogs(u64),
    #[serde(rename = "musicbrainz")]
    MusicBrainz(Mbid),
}

impl ReleaseId {
    pub fn provider(&self) -> ProviderKind {
        match self {
            Self::Discogs(_) => ProviderKind::Discogs,
            Self::MusicBrainz(_) => ProviderKind::MusicBrainz,
        }
    }

    /// The provider-local identifier as stored in SQLite.
    pub fn storage_id(&self) -> String {
        match self {
            Self::Discogs(id) => id.to_string(),
            Self::MusicBrainz(id) => id.to_string(),
        }
    }

    pub fn discogs_id(&self) -> Option<u64> {
        match self {
            Self::Discogs(id) => Some(*id),
            Self::MusicBrainz(_) => None,
        }
    }

    pub fn mbid(&self) -> Option<Mbid> {
        match self {
            Self::Discogs(_) => None,
            Self::MusicBrainz(id) => Some(*id),
        }
    }
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discogs(id) => write!(f, "discogs {id}"),
            Self::MusicBrainz(id) => write!(f, "musicbrainz {id}"),
        }
    }
}

/// A release group (MusicBrainz) or master (Discogs).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", content = "id", rename_all = "snake_case")]
pub enum GroupId {
    DiscogsMaster(u64),
    #[serde(rename = "musicbrainz_release_group")]
    MusicBrainzReleaseGroup(Mbid),
}

impl GroupId {
    /// The provider-local identifier as stored in SQLite.
    pub fn storage_id(&self) -> String {
        match self {
            Self::DiscogsMaster(id) => id.to_string(),
            Self::MusicBrainzReleaseGroup(id) => id.to_string(),
        }
    }
}

/// One bounded search request. Providers translate the strategy themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStrategy {
    Barcode(String),
    CatalogNumber(String),
    TitleArtist {
        title: String,
        artist: Option<String>,
        release_group: Option<Mbid>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub strategy: SearchStrategy,
    pub offset: u16,
    pub limit: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<crate::matcher::SearchSummary>,
    /// Total result count when the service reports one.
    pub total: Option<u64>,
    /// Offset of the next page, when another one exists.
    pub next_offset: Option<u16>,
}

/// Provider-neutral failure classification with the service and status kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Configuration {
        provider: ProviderKind,
        message: String,
    },
    Authentication {
        provider: ProviderKind,
        message: String,
    },
    NotFound {
        provider: ProviderKind,
        id: String,
    },
    RateLimited {
        provider: ProviderKind,
        message: String,
        retry_after: Option<Duration>,
    },
    Network {
        provider: ProviderKind,
        message: String,
    },
    /// The adapter is pausing requests after repeated transient failures.
    Unavailable {
        provider: ProviderKind,
        retry_after: Duration,
    },
    Malformed {
        provider: ProviderKind,
        message: String,
    },
    Permanent {
        provider: ProviderKind,
        status: Option<u16>,
        message: String,
    },
}

impl ProviderError {
    pub fn provider(&self) -> ProviderKind {
        match self {
            Self::Configuration { provider, .. }
            | Self::Authentication { provider, .. }
            | Self::NotFound { provider, .. }
            | Self::RateLimited { provider, .. }
            | Self::Network { provider, .. }
            | Self::Unavailable { provider, .. }
            | Self::Malformed { provider, .. }
            | Self::Permanent { provider, .. } => *provider,
        }
    }

    /// Credential and configuration failures: the affected stage cannot
    /// continue, though another provider may still recover the release.
    pub fn is_auth(&self) -> bool {
        matches!(
            self,
            Self::Authentication { .. } | Self::Configuration { .. }
        )
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    /// True for failures that say nothing about whether a match exists.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::Network { .. }
                | Self::Unavailable { .. }
                | Self::Malformed { .. }
        )
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration { provider, message } => {
                write!(f, "{provider} is not configured: {message}")
            }
            Self::Authentication { provider, message } => {
                write!(f, "{provider} authentication failed: {message}")
            }
            Self::NotFound { provider, id } => {
                write!(f, "{provider} release {id} was not found")
            }
            Self::RateLimited {
                provider,
                message,
                retry_after,
            } => match retry_after {
                Some(delay) => write!(
                    f,
                    "{provider} rate limit exceeded ({message}); wait {delay:?}"
                ),
                None => write!(f, "{provider} rate limit exceeded: {message}"),
            },
            Self::Network { provider, message } => {
                write!(f, "{provider} request failed: {message}")
            }
            Self::Unavailable {
                provider,
                retry_after,
            } => write!(
                f,
                "{provider} is temporarily unavailable after transient failures; retry in {}s",
                retry_after.as_secs()
            ),
            Self::Malformed { provider, message } => {
                write!(f, "{provider} returned an unusable response: {message}")
            }
            Self::Permanent {
                provider,
                status,
                message,
            } => match status {
                Some(status) => write!(f, "{provider} returned HTTP {status}: {message}"),
                None => write!(f, "{provider} failed permanently: {message}"),
            },
        }
    }
}

impl std::error::Error for ProviderError {}

/// The service boundary. Stays small on purpose: search summaries, then fetch
/// one concrete release. Native return-position futures avoid a trait-object or
/// `async-trait` dependency.
pub trait MetadataProvider: Sync {
    fn kind(&self) -> ProviderKind;

    fn search(
        &self,
        request: SearchRequest,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send;

    fn fetch_release(
        &self,
        id: ReleaseId,
    ) -> impl std::future::Future<Output = Result<crate::matcher::RemoteRelease, ProviderError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_provider_is_a_transient_failure_that_names_its_service() {
        let error = ProviderError::Unavailable {
            provider: ProviderKind::MusicBrainz,
            retry_after: Duration::from_secs(7),
        };
        assert!(error.is_transient());
        assert!(!error.is_auth());
        assert_eq!(error.provider(), ProviderKind::MusicBrainz);
        assert!(error.to_string().contains("retry in 7s"), "{error}");
    }

    #[test]
    fn canonicalizes_mbids_without_accepting_other_identifier_shapes() {
        let id: Mbid = "76DF3287-6CDA-33EB-8E9A-044B5E15FFDD".parse().unwrap();
        assert_eq!(id.to_string(), "76df3287-6cda-33eb-8e9a-044b5e15ffdd");
        assert!("42".parse::<Mbid>().is_err());
        assert!("76df32876cda33eb8e9a044b5e15ffdd".parse::<Mbid>().is_err());
        assert!(
            "00000000-0000-0000-0000-000000000000"
                .parse::<Mbid>()
                .is_err()
        );
    }

    #[test]
    fn serde_uses_the_same_mbid_validation() {
        let id: Mbid = "\"76df3287-6cda-33eb-8e9a-044b5e15ffdd\""
            .parse::<serde_json::Value>()
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap();
        assert_eq!(id.to_string(), "76df3287-6cda-33eb-8e9a-044b5e15ffdd");

        assert!(serde_json::from_str::<Mbid>("\"76df32876cda33eb8e9a044b5e15ffdd\"").is_err());
        assert!(serde_json::from_str::<Mbid>("\"00000000-0000-0000-0000-000000000000\"").is_err());
    }

    #[test]
    fn release_identities_round_trip_with_provider_names() {
        let discogs = ReleaseId::Discogs(42);
        assert_eq!(discogs.provider(), ProviderKind::Discogs);
        assert_eq!(discogs.storage_id(), "42");

        let mbid: Mbid = "76df3287-6cda-33eb-8e9a-044b5e15ffdd".parse().unwrap();
        let musicbrainz = ReleaseId::MusicBrainz(mbid);
        let json = serde_json::to_string(&musicbrainz).unwrap();
        assert_eq!(
            json,
            r#"{"provider":"musicbrainz","id":"76df3287-6cda-33eb-8e9a-044b5e15ffdd"}"#
        );
        assert_eq!(
            serde_json::from_str::<ReleaseId>(&json).unwrap(),
            musicbrainz
        );
        assert_eq!(musicbrainz.provider().as_str(), "musicbrainz");
    }

    #[test]
    fn policy_orders_providers_and_explicit_selection_disables_fallback() {
        assert_eq!(
            ProviderPolicy::default().order(),
            vec![ProviderKind::MusicBrainz, ProviderKind::Discogs]
        );
        assert!(!ProviderPolicy::Only(ProviderKind::Discogs).allows(ProviderKind::MusicBrainz));
        assert!(ProviderPolicy::default().allows(ProviderKind::MusicBrainz));
    }

    #[test]
    fn group_ids_keep_provider_qualified_names() {
        let mbid: Mbid = "4642ee19-7790-3c8d-ab5e-d133de942db6".parse().unwrap();
        let json = serde_json::to_string(&GroupId::MusicBrainzReleaseGroup(mbid)).unwrap();
        assert!(json.contains("musicbrainz_release_group"), "{json}");
    }

    #[test]
    fn bounded_cache_evicts_the_oldest_entry_at_capacity() {
        let mut cache = BoundedCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(2));
        assert_eq!(cache.get(&"c"), Some(3));
    }

    #[test]
    fn bounded_cache_reads_and_overwrites_refresh_recency() {
        let mut cache = BoundedCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        // Reading "a" makes "b" the oldest entry.
        assert_eq!(cache.get(&"a"), Some(1));
        cache.insert("c", 3);
        assert_eq!(cache.get(&"b"), None);
        // Overwriting "a" keeps one entry and refreshes it.
        cache.insert("a", 10);
        cache.insert("d", 4);
        assert_eq!(cache.get(&"c"), None);
        assert_eq!(cache.get(&"a"), Some(10));
        assert_eq!(cache.len(), 2);
    }
}
