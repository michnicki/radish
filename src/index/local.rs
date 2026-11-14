//! Provider adapters that answer `search` and `fetch_release` from an index.
//!
//! The trait is the only seam: discovery, scoring, acceptance, review, and
//! overlays never learn whether a candidate came from HTTP or a dump. Search
//! only decides which candidates reach detail lookups, and
//! `matcher::RankIdentity::rank` re-scores every hit anyway.

use crate::index::writer::decode_document;
use crate::index::{IndexKind, IndexPaths, open_read_only};
use crate::matcher::{
    RemoteRelease, SearchSummary, normalize_barcode, normalize_identifier, normalize_text,
};
use crate::providers::{
    BoundedCache, Mbid, MetadataProvider, ProviderError, ProviderKind, ReleaseId, SearchPage,
    SearchRequest, SearchStrategy, lock,
};
use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use std::sync::Mutex;

/// Longest identifier or text term worth putting in a query.
const MAX_TERMS: usize = 12;

/// How many text searches each open index remembers.
///
/// Releases in one series, whose titles an ID3v1 tag cut off at the same
/// place, and copies of one album in two folders repeat a search, usually a
/// few releases apart. On a 6,819-release library, 256 entries saved 56 of the
/// 65 seconds those repeats cost. Each entry holds at most one page of
/// summaries (51), so the cache stays around 13 MB at most. An open index never
/// changes, so a remembered page is always what the query would return.
const SEARCH_CACHE_ENTRIES: usize = 256;

/// One text search as sent to SQLite, the key of the search cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextQuery {
    expression: String,
    /// Present only when the query filters by it.
    release_group: Option<String>,
    offset: i64,
    limit: i64,
}

pub struct LocalIndex {
    kind: IndexKind,
    pool: SqlitePool,
    searches: Mutex<BoundedCache<TextQuery, Vec<Vec<u8>>>>,
}

impl LocalIndex {
    /// Opens a published index for reading.
    pub async fn open(paths: &IndexPaths, kind: IndexKind) -> Result<Self> {
        let (pool, _meta) = open_read_only(paths, kind).await?;
        Ok(Self {
            kind,
            pool,
            searches: Mutex::new(BoundedCache::new(SEARCH_CACHE_ENTRIES)),
        })
    }

    fn provider(&self) -> ProviderKind {
        self.kind.provider()
    }

    fn failure(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::Malformed {
            provider: self.provider(),
            message: message.into(),
        }
    }

    fn empty_term(&self) -> ProviderError {
        ProviderError::Permanent {
            provider: self.provider(),
            status: None,
            message: "refusing an unusable empty search term".to_string(),
        }
    }

    async fn search_local(&self, request: SearchRequest) -> Result<SearchPage, ProviderError> {
        let limit = if request.limit == 0 {
            50
        } else {
            i64::from(request.limit)
        };
        let offset = i64::from(request.offset);

        match &request.strategy {
            SearchStrategy::Barcode(barcode) => {
                let value = normalize_barcode(barcode);
                if value.is_empty() {
                    return Err(self.empty_term());
                }
                self.search_identifier("barcode", &value, offset, limit)
                    .await
            }
            SearchStrategy::CatalogNumber(catalog) => {
                let value = normalize_identifier(catalog);
                if value.is_empty() {
                    return Err(self.empty_term());
                }
                self.search_identifier("catalog", &value, offset, limit)
                    .await
            }
            SearchStrategy::TitleArtist {
                title,
                artist,
                release_group,
            } => {
                if title.trim().is_empty() {
                    return Err(self.empty_term());
                }
                self.search_text(
                    title,
                    artist.as_deref(),
                    release_group.as_ref(),
                    offset,
                    limit,
                )
                .await
            }
        }
    }

    /// Exact identifier lookup. Identifiers are never loosened.
    async fn search_identifier(
        &self,
        kind: &str,
        value: &str,
        offset: i64,
        limit: i64,
    ) -> Result<SearchPage, ProviderError> {
        let summaries: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT r.summary
             FROM identifiers i
             JOIN releases r ON r.id = i.release_id
             WHERE i.kind = ? AND i.value = ?
             ORDER BY r.id
             LIMIT ? OFFSET ?",
        )
        .bind(kind)
        .bind(value)
        .bind(limit + 1)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| self.failure(format!("cannot search identifiers: {error}")))?;

        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM identifiers WHERE kind = ? AND value = ?")
                .bind(kind)
                .bind(value)
                .fetch_one(&self.pool)
                .await
                .map_err(|error| self.failure(format!("cannot count identifiers: {error}")))?;

        self.page(summaries, offset, limit, Some(total.max(0) as u64))
    }

    /// Text search, with the network adapter's exact-then-loosened fallback:
    /// every term must match first, then any term, so a local "Remastered"
    /// suffix still finds a release whose remote title lacks it.
    async fn search_text(
        &self,
        title: &str,
        artist: Option<&str>,
        release_group: Option<&Mbid>,
        offset: i64,
        limit: i64,
    ) -> Result<SearchPage, ProviderError> {
        // A title with no searchable characters (`-`, `???`) is not blank, so
        // the network adapters send it and the service finds nothing. Match
        // that: a clean miss, not a failure that would abort the release.
        let Some(exact) = match_expression(title, artist, false) else {
            return self.page(Vec::new(), offset, limit, None);
        };
        let mut summaries = self.text_page(&exact, release_group, offset, limit).await?;

        if summaries.is_empty()
            && offset == 0
            && let Some(loose) = match_expression(title, artist, true)
        {
            summaries = self.text_page(&loose, release_group, offset, limit).await?;
        }

        self.page(summaries, offset, limit, None)
    }

    async fn text_page(
        &self,
        expression: &str,
        release_group: Option<&Mbid>,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Vec<u8>>, ProviderError> {
        let filtered_by_group = self.kind == IndexKind::MusicBrainz && release_group.is_some();
        let key = TextQuery {
            expression: expression.to_string(),
            release_group: release_group
                .filter(|_| filtered_by_group)
                .map(ToString::to_string),
            offset,
            limit,
        };
        if let Some(summaries) = lock(&self.searches).get(&key) {
            return Ok(summaries);
        }

        // Audited: `text_search_sql` returns one of two fixed shapes and every
        // value is bound.
        let sql = text_search_sql(filtered_by_group);
        let mut query = sqlx::query_scalar::<_, Vec<u8>>(sqlx::AssertSqlSafe(sql));
        query = query.bind(expression);
        if let Some(group) = &key.release_group {
            query = query.bind(group.as_str());
        }
        let summaries = query
            .bind(limit + 1)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| self.failure(format!("cannot search titles: {error}")))?;
        lock(&self.searches).insert(key, summaries.clone());
        Ok(summaries)
    }

    fn page(
        &self,
        mut summaries: Vec<Vec<u8>>,
        offset: i64,
        limit: i64,
        total: Option<u64>,
    ) -> Result<SearchPage, ProviderError> {
        let has_more = summaries.len() as i64 > limit;
        summaries.truncate(limit.max(0) as usize);

        // Stored as BLOBs so a corrupt row is reported instead of being read as
        // lossily decoded text.
        let hits = summaries
            .iter()
            .map(|raw| {
                serde_json::from_slice::<SearchSummary>(raw).map_err(|error| {
                    self.failure(format!("a stored search summary is unusable: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(SearchPage {
            hits,
            total,
            next_offset: if has_more {
                u16::try_from(offset + limit).ok()
            } else {
                None
            },
        })
    }

    async fn search_summary_document(
        &self,
        id: &str,
    ) -> Result<Option<RemoteRelease>, ProviderError> {
        let document: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT document FROM releases WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| self.failure(format!("cannot read release {id}: {error}")))?;

        match document {
            None => Ok(None),
            Some(bytes) => decode_document(&bytes).map(Some).map_err(|error| {
                self.failure(format!("a stored release document is unusable: {error:#}"))
            }),
        }
    }

    async fn redirected(&self, gid: &str) -> Result<Option<String>, ProviderError> {
        if self.kind != IndexKind::MusicBrainz {
            return Ok(None);
        }
        sqlx::query_scalar("SELECT release_id FROM redirects WHERE gid = ?")
            .bind(gid)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| self.failure(format!("cannot resolve a merged ID: {error}")))
    }

    async fn fetch_local(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
        if id.provider() != self.provider() {
            return Err(ProviderError::Permanent {
                provider: self.provider(),
                status: None,
                message: format!(
                    "{} cannot fetch a {} release",
                    self.provider(),
                    id.provider()
                ),
            });
        }
        let requested = id.storage_id();

        if let Some(mut release) = self.search_summary_document(&requested).await? {
            if release.id != id {
                release.lookup_aliases.push(id);
            }
            return Ok(release);
        }

        if let Some(target) = self.redirected(&requested).await?
            && let Some(mut release) = self.search_summary_document(&target).await?
        {
            release.lookup_aliases.push(id);
            return Ok(release);
        }

        Err(ProviderError::NotFound {
            provider: self.provider(),
            id: requested,
        })
    }
}

impl MetadataProvider for LocalIndex {
    fn kind(&self) -> ProviderKind {
        self.provider()
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchPage, ProviderError> {
        self.search_local(request).await
    }

    async fn fetch_release(&self, id: ReleaseId) -> Result<RemoteRelease, ProviderError> {
        self.fetch_local(id).await
    }
}

/// Builds the FTS5 expression for one text search.
///
/// `any` switches every group from all-terms to any-term, which is the
/// loosened retry. Every word is quoted, so tag text can never be read as FTS
/// syntax.
/// The text-search statement: one of two fixed shapes, every value bound.
///
/// Rank rowids inside the FTS index and join `releases` only for the page that
/// is returned. Ordering a join by `r.id` instead forces one random row read
/// per match: for a common-word query that is over a million reads, which took
/// close to a minute on a cold Discogs index against under a second for this
/// shape. The group filter needs the join, but it only narrows the set. The
/// outer ORDER BY re-sorts the page, since a join does not promise to keep its
/// input order.
fn text_search_sql(filtered_by_group: bool) -> String {
    let ranked = if filtered_by_group {
        "SELECT f.rowid AS rid, bm25(releases_fts, 5.0, 1.0) AS score
         FROM releases_fts f
         JOIN releases g ON g.rowid = f.rowid
         WHERE releases_fts MATCH ? AND g.release_group_id = ?
         ORDER BY score, f.rowid LIMIT ? OFFSET ?"
    } else {
        "SELECT rowid AS rid, bm25(releases_fts, 5.0, 1.0) AS score
         FROM releases_fts
         WHERE releases_fts MATCH ?
         ORDER BY score, rowid LIMIT ? OFFSET ?"
    };
    format!(
        "SELECT r.summary FROM ({ranked}) t JOIN releases r ON r.rowid = t.rid
         ORDER BY t.score, t.rid"
    )
}

/// Builds the FTS5 expression for one search.
///
/// The strict form requires every term. The loose form (`any`) requires only
/// one and is the fallback when the strict form finds nothing, so it drops
/// single-character tokens: `s`, `1`, or a stray initial matches millions of
/// releases and carries no evidence. The strict form keeps them because the
/// index tokenizes titles the same way (`b-52's` is `b`, `52`, `s` on both
/// sides).
fn match_expression(title: &str, artist: Option<&str>, any: bool) -> Option<String> {
    let title_terms = terms(title, any);
    if title_terms.is_empty() {
        return None;
    }

    let joiner = if any { " OR " } else { " " };
    let mut expression = format!("title_norm : ({})", title_terms.join(joiner));

    if let Some(artist) = artist.filter(|value| !value.trim().is_empty()) {
        let artist_terms = terms(artist, any);
        if !artist_terms.is_empty() {
            expression.push_str(&format!(
                " AND artist_norm : ({})",
                artist_terms.join(joiner)
            ));
        }
    }
    Some(expression)
}

fn terms(value: &str, skip_single_characters: bool) -> Vec<String> {
    normalize_text(value)
        .split_whitespace()
        .filter(|term| !skip_single_characters || term.chars().nth(1).is_some())
        .take(MAX_TERMS)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::writer::{IndexRow, IndexWriter, summary_for};
    use crate::matcher::TrackEntryType;
    use crate::providers::{GroupId, SearchPage as ProviderSearchPage};
    use crate::test_support::ScratchDir;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    fn mbid(value: &str) -> Mbid {
        value.parse().unwrap()
    }

    fn release(
        id: &str,
        title: &str,
        artist: &str,
        group: Option<&str>,
        barcode: Option<&str>,
    ) -> RemoteRelease {
        RemoteRelease {
            id: ReleaseId::MusicBrainz(mbid(id)),
            group_id: group.map(|group| GroupId::MusicBrainzReleaseGroup(mbid(group))),
            title: title.to_string(),
            artists: vec![artist.to_string()],
            year: Some(2001),
            country: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            formats: vec!["CD".to_string()],
            barcodes: barcode.into_iter().map(str::to_string).collect(),
            tracklist: vec![crate::matcher::RemoteTrack {
                position: Some("1".to_string()),
                title: "One".to_string(),
                artists: Vec::new(),
                duration_secs: Some(180.0),
                entry_type: TrackEntryType::Track,
                medium_position: Some(1),
                track_position: Some(1),
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            }],
            url: format!("https://musicbrainz.org/release/{id}"),
            lookup_aliases: Vec::new(),
            artist_credit: None,
            structure_issues: Vec::new(),
            edition_notes: Vec::new(),
            provider_metadata: serde_json::Value::Null,
        }
    }

    async fn index_with(releases: &[RemoteRelease], kind: IndexKind) -> (ScratchDir, IndexPaths) {
        let dir = ScratchDir::new("local-index");
        let paths = IndexPaths::from_dir(dir.path().join("index"));
        let writer = IndexWriter::create(&paths, kind).await.unwrap();
        for release in releases {
            writer
                .insert(&IndexRow::new(release, &summary_for(release)).unwrap())
                .await
                .unwrap();
        }
        writer.finish().await.unwrap();
        (dir, paths)
    }

    fn title_request(title: &str, artist: Option<&str>, group: Option<Mbid>) -> SearchRequest {
        SearchRequest {
            strategy: SearchStrategy::TitleArtist {
                title: title.to_string(),
                artist: artist.map(str::to_string),
                release_group: group,
            },
            offset: 0,
            limit: 50,
        }
    }

    #[tokio::test]
    async fn identifier_hits_page_through_the_whole_result_set() {
        let shared = "0123456789012";
        let releases: Vec<RemoteRelease> = (0..3)
            .map(|index| {
                release(
                    &format!("0000000{index}-1111-1111-1111-111111111111"),
                    "Blue",
                    "Test Artist",
                    None,
                    Some(shared),
                )
            })
            .collect();
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let first = index
            .search(SearchRequest {
                strategy: SearchStrategy::Barcode(shared.to_string()),
                offset: 0,
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(first.hits.len(), 2);
        assert_eq!(first.total, Some(3));
        assert_eq!(first.next_offset, Some(2));

        let second = index
            .search(SearchRequest {
                strategy: SearchStrategy::Barcode(shared.to_string()),
                offset: 2,
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.total, Some(3));
        assert_eq!(second.next_offset, None);
    }

    #[tokio::test]
    async fn a_title_search_falls_back_from_every_term_to_any_term() {
        let releases = vec![release(
            "11111111-1111-1111-1111-111111111111",
            "Blue Moon Remastered",
            "Test Artist",
            None,
            None,
        )];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let page = index
            .search(title_request("Blue Moon Deluxe", Some("Test Artist"), None))
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1, "the loosened query finds the release");
        assert_eq!(page.hits[0].title, "Blue Moon Remastered");
    }

    #[test]
    fn the_loose_form_drops_single_characters_but_the_strict_form_keeps_them() {
        assert_eq!(
            match_expression("Mistah's Wu-Pack Vol. 1", None, false).unwrap(),
            "title_norm : (\"mistah\" \"s\" \"wu\" \"pack\" \"vol\" \"1\")"
        );
        assert_eq!(
            match_expression("Mistah's Wu-Pack Vol. 1", Some("A B Cee"), true).unwrap(),
            "title_norm : (\"mistah\" OR \"wu\" OR \"pack\" OR \"vol\") \
             AND artist_norm : (\"cee\")"
        );
        // Nothing but single characters leaves no loose search at all, and an
        // artist made only of them is left out instead of emptying the clause.
        assert_eq!(match_expression("1", None, true), None);
        assert_eq!(
            match_expression("Blue", Some("X Y"), true).unwrap(),
            "title_norm : (\"blue\")"
        );
    }

    #[tokio::test]
    async fn the_loose_search_ignores_single_character_matches() {
        // Both releases share only a one-character token with the query; the
        // strict form finds nothing and the loose form must not return them.
        let releases = vec![
            release(
                "11111111-1111-1111-1111-111111111111",
                "Disc 1",
                "Someone",
                None,
                None,
            ),
            release(
                "22222222-2222-2222-2222-222222222222",
                "Greatest Hits 1",
                "Someone Else",
                None,
                None,
            ),
        ];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let page = index
            .search(title_request("Zebra 1", None, None))
            .await
            .unwrap();
        assert!(page.hits.is_empty(), "{:?}", page.hits);
    }

    #[tokio::test]
    async fn a_title_without_searchable_characters_is_a_miss_not_a_failure() {
        let releases = vec![release(
            "11111111-1111-1111-1111-111111111111",
            "Blue",
            "Test Artist",
            None,
            None,
        )];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        for title in ["-", "???", "..."] {
            let page = index
                .search(title_request(title, Some("Test Artist"), None))
                .await
                .unwrap_or_else(|error| panic!("{title:?} failed: {error}"));
            assert!(page.hits.is_empty(), "{title:?}");
            assert_eq!(page.next_offset, None, "{title:?}");
        }
    }

    #[tokio::test]
    async fn better_matches_rank_first_and_ties_keep_insertion_order() {
        let releases = vec![
            release(
                "11111111-1111-1111-1111-111111111111",
                "Blue",
                "Test Artist",
                None,
                None,
            ),
            release(
                "22222222-2222-2222-2222-222222222222",
                "Blue Moon Blue Moon",
                "Test Artist",
                None,
                None,
            ),
            release(
                "33333333-3333-3333-3333-333333333333",
                "Blue",
                "Test Artist",
                None,
                None,
            ),
        ];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let page = index
            .search(title_request("Blue", Some("Test Artist"), None))
            .await
            .unwrap();
        let ids: Vec<String> = page.hits.iter().map(|hit| hit.id.to_string()).collect();
        // The two exact "Blue" titles outrank the longer one, and the tie
        // between them falls back to insertion order.
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids[0].contains("11111111"), "{ids:?}");
        assert!(ids[1].contains("33333333"), "{ids:?}");
        assert!(ids[2].contains("22222222"), "{ids:?}");
    }

    #[tokio::test]
    async fn diacritics_and_punctuation_match_their_base_letters() {
        let releases = vec![
            release(
                "11111111-1111-1111-1111-111111111111",
                "Ágætis byrjun",
                "Sigur Rós",
                None,
                None,
            ),
            release(
                "22222222-2222-2222-2222-222222222222",
                "Unrelated",
                "Nobody",
                None,
                None,
            ),
        ];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let page = index
            .search(title_request("agætis BYRJUN", Some("sigur ros"), None))
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(
            page.hits[0].id,
            ReleaseId::MusicBrainz(mbid("11111111-1111-1111-1111-111111111111"))
        );
    }

    #[tokio::test]
    async fn a_release_group_narrows_a_musicbrainz_search_but_not_a_discogs_one() {
        let first_group = "4642ee19-7790-3c8d-ab5e-d133de942db6";
        let second_group = "5642ee19-7790-3c8d-ab5e-d133de942db6";
        let releases = vec![
            release(
                "11111111-1111-1111-1111-111111111111",
                "Blue",
                "Test Artist",
                Some(first_group),
                None,
            ),
            release(
                "22222222-2222-2222-2222-222222222222",
                "Blue",
                "Test Artist",
                Some(second_group),
                None,
            ),
        ];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let narrowed = index
            .search(title_request("Blue", None, Some(mbid(first_group))))
            .await
            .unwrap();
        assert_eq!(narrowed.hits.len(), 1);
        assert_eq!(
            narrowed.hits[0].id,
            ReleaseId::MusicBrainz(mbid("11111111-1111-1111-1111-111111111111"))
        );

        let released = index
            .search(title_request("Blue", None, None))
            .await
            .unwrap();
        assert_eq!(released.hits.len(), 2);
    }

    #[tokio::test]
    async fn a_repeated_search_is_answered_from_memory() {
        let id = "11111111-1111-1111-1111-111111111111";
        let releases = vec![release(id, "Blue", "Test Artist", None, None)];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        let first = index
            .search(title_request("Blue", Some("Test Artist"), None))
            .await
            .unwrap();
        assert_eq!(first.hits[0].title, "Blue");

        // Change the stored summary behind the open index. Only a search that
        // reads the file again can see the new title.
        let renamed = release(id, "Renamed", "Test Artist", None, None);
        let options = SqliteConnectOptions::new().filename(paths.file(IndexKind::MusicBrainz));
        let writable = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("UPDATE releases SET summary = ? WHERE id = ?")
            .bind(serde_json::to_vec(&summary_for(&renamed)).unwrap())
            .bind(id)
            .execute(&writable)
            .await
            .unwrap();
        writable.close().await;

        let repeated = index
            .search(title_request("Blue", Some("Test Artist"), None))
            .await
            .unwrap();
        assert_eq!(
            repeated.hits[0].title, "Blue",
            "the same search is not run again"
        );

        let mut other_page = title_request("Blue", Some("Test Artist"), None);
        other_page.limit = 10;
        let different = index.search(other_page).await.unwrap();
        assert_eq!(
            different.hits[0].title, "Renamed",
            "another search reads the index"
        );
    }

    #[tokio::test]
    async fn a_merged_id_resolves_through_a_redirect_and_keeps_an_alias() {
        let canonical = "11111111-1111-1111-1111-111111111111";
        let merged = "22222222-2222-2222-2222-222222222222";
        let releases = vec![release(canonical, "Blue", "Test Artist", None, None)];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;

        // A redirect is written during the build; add one to this finished file.
        let options = SqliteConnectOptions::new().filename(paths.file(IndexKind::MusicBrainz));
        let writable = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("INSERT INTO redirects(gid, release_id) VALUES (?, ?)")
            .bind(merged)
            .bind(canonical)
            .execute(&writable)
            .await
            .unwrap();
        writable.close().await;

        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        let release = index
            .fetch_release(ReleaseId::MusicBrainz(mbid(merged)))
            .await
            .unwrap();
        assert_eq!(release.id, ReleaseId::MusicBrainz(mbid(canonical)));
        assert_eq!(
            release.lookup_aliases,
            vec![ReleaseId::MusicBrainz(mbid(merged))]
        );
    }

    #[tokio::test]
    async fn an_unknown_id_is_not_found_and_a_foreign_id_is_refused() {
        let releases = vec![release(
            "11111111-1111-1111-1111-111111111111",
            "Blue",
            "Test Artist",
            None,
            None,
        )];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let missing = index
            .fetch_release(ReleaseId::MusicBrainz(mbid(
                "22222222-2222-2222-2222-222222222222",
            )))
            .await
            .unwrap_err();
        assert!(missing.is_not_found(), "{missing}");

        let foreign = index
            .fetch_release(ReleaseId::Discogs(42))
            .await
            .unwrap_err();
        assert!(matches!(foreign, ProviderError::Permanent { .. }));
    }

    #[tokio::test]
    async fn a_corrupted_document_is_reported_as_malformed() {
        let id = "11111111-1111-1111-1111-111111111111";
        let releases = vec![release(id, "Blue", "Test Artist", None, None)];
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;

        let options = SqliteConnectOptions::new().filename(paths.file(IndexKind::MusicBrainz));
        let writable = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("UPDATE releases SET document = X'00' WHERE id = ?")
            .bind(id)
            .execute(&writable)
            .await
            .unwrap();
        writable.close().await;

        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        let error = index
            .fetch_release(ReleaseId::MusicBrainz(mbid(id)))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Malformed { .. }), "{error}");
    }

    #[tokio::test]
    async fn searches_use_the_identifier_index_and_the_fts_index() {
        // Enough rows for the planner's choices to be meaningful rather than a
        // scan of a one-row table.
        let releases: Vec<RemoteRelease> = (0..120)
            .map(|index| {
                let barcode = if index == 0 {
                    "0123456789012".to_string()
                } else {
                    format!("999999999{index:04}")
                };
                release(
                    &format!("{index:08x}-1111-1111-1111-111111111111"),
                    &format!("Title {index}"),
                    "Test Artist",
                    None,
                    Some(&barcode),
                )
            })
            .collect();
        let (_dir, paths) = index_with(&releases, IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();

        let plan = |sql: &'static str| {
            let pool = index.pool.clone();
            async move {
                sqlx::query(sql)
                    .fetch_all(&pool)
                    .await
                    .unwrap()
                    .iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect::<Vec<String>>()
            }
        };

        let identifier_plan = plan(
            "EXPLAIN QUERY PLAN
             SELECT r.summary FROM identifiers i JOIN releases r ON r.id = i.release_id
             WHERE i.kind = 'barcode' AND i.value = '0123456789012' ORDER BY r.id",
        )
        .await;
        assert!(
            identifier_plan
                .iter()
                .any(|detail| detail.contains("idx_identifiers_lookup")),
            "{identifier_plan:?}"
        );

        for grouped in [false, true] {
            let sql: &'static str = Box::leak(
                format!("EXPLAIN QUERY PLAN {}", text_search_sql(grouped)).into_boxed_str(),
            );
            let text_plan = plan(sql).await;
            assert!(
                text_plan
                    .iter()
                    .any(|detail| detail.contains("VIRTUAL TABLE INDEX")),
                "grouped={grouped}: {text_plan:?}"
            );
            // The page is joined by primary key; a scan of `releases` is the
            // per-match random read this shape exists to avoid.
            // `SCAN releases_fts VIRTUAL TABLE INDEX` is the FTS access itself;
            // only the base table (aliased `r` or `g`) must not be scanned.
            assert!(
                text_plan.iter().all(|detail| {
                    detail != "SCAN r"
                        && detail != "SCAN g"
                        && !detail.starts_with("SCAN releases ")
                }),
                "grouped={grouped}: {text_plan:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_term_is_refused_before_touching_the_index() {
        let (_dir, paths) = index_with(&[], IndexKind::MusicBrainz).await;
        let index = LocalIndex::open(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        for strategy in [
            SearchStrategy::Barcode("   ".to_string()),
            SearchStrategy::CatalogNumber(String::new()),
            SearchStrategy::TitleArtist {
                title: String::new(),
                artist: None,
                release_group: None,
            },
        ] {
            let error = index
                .search(SearchRequest {
                    strategy,
                    offset: 0,
                    limit: 50,
                })
                .await
                .unwrap_err();
            assert!(matches!(error, ProviderError::Permanent { .. }), "{error}");
        }

        // A successful empty page is still distinct from a failure.
        let empty: ProviderSearchPage = index
            .search(title_request("Nothing Like This", None, None))
            .await
            .unwrap();
        assert!(empty.hits.is_empty());
        assert_eq!(empty.next_offset, None);
    }
}
