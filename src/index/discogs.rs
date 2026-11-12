//! Building the Discogs index from the monthly releases dump.
//!
//! `data.discogs.com` publishes one XML document per release inside a large
//! gzip stream (sometimes wrapped in a tar). Releases are pulled out in a
//! streaming pass and converted with the same rules the HTTP adapter uses, so
//! a locally found candidate is the same document the network would return.
//! Only releases are needed: credits, labels, catalog numbers, formats, and
//! barcodes are all inline, and a release carries its master ID.

use crate::index::writer::{IndexRow, IndexWriter};
use crate::index::{BuildProgress, BuildReport, IndexKind, IndexPaths};
use crate::matcher::{RemoteRelease, RemoteTrack, SearchSummary, TrackEntryType, parse_duration};
use crate::output::clean_path;
use crate::providers::{GroupId, ReleaseId};
use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const PROGRESS_EVERY: u64 = 100_000;

/// Builds (or rebuilds) the Discogs index from one dump file.
///
/// The input may be the published `.xml.gz`, a tar archive holding it, or a
/// plain `.xml` file; the choice is made from the file's first bytes.
pub async fn build(
    input: &Path,
    paths: &IndexPaths,
    progress: &dyn BuildProgress,
) -> Result<BuildReport> {
    let started = Instant::now();
    let writer = IndexWriter::create(paths, IndexKind::Discogs).await?;
    writer.set_meta("source_file", &source_name(input)).await?;

    let mut counters = Counters::default();
    match sniff(input)? {
        Input::Xml(file) => {
            let mut reader = ReleaseReader::new(BufReader::new(file));
            consume(&mut reader, &writer, &mut counters, progress).await?;
        }
        Input::Gzip(file) => {
            let mut reader = ReleaseReader::new(BufReader::new(flate2::read::GzDecoder::new(file)));
            consume(&mut reader, &writer, &mut counters, progress).await?;
        }
        Input::Tar(file) => {
            let mut archive = tar::Archive::new(BufReader::new(file));
            let mut found = false;
            for entry in archive
                .entries()
                .with_context(|| format!("{} is a truncated tar", clean_path(input)))?
            {
                let entry =
                    entry.with_context(|| format!("{} is a truncated tar", clean_path(input)))?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let name = entry
                    .path()
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().to_string())
                    })
                    .unwrap_or_default();
                if name.ends_with(".xml.gz") {
                    let mut reader =
                        ReleaseReader::new(BufReader::new(flate2::read::GzDecoder::new(entry)));
                    consume(&mut reader, &writer, &mut counters, progress).await?;
                    found = true;
                    break;
                }
                if name.ends_with(".xml") {
                    let mut reader = ReleaseReader::new(BufReader::new(entry));
                    consume(&mut reader, &writer, &mut counters, progress).await?;
                    found = true;
                    break;
                }
            }
            if !found {
                bail!("{} holds no Discogs releases XML", clean_path(input));
            }
        }
    }

    if counters.indexed == 0 {
        bail!(
            "{} contains no usable releases; check that it is the Discogs releases dump",
            clean_path(input)
        );
    }

    writer
        .set_meta("indexed_releases", &counters.indexed.to_string())
        .await?;
    writer
        .set_meta("skipped_rows", &counters.skipped.to_string())
        .await?;
    writer.set_meta("imported_at", &imported_at()).await?;
    writer.finish().await?;

    Ok(BuildReport {
        indexed: counters.indexed,
        skipped: counters.skipped,
        elapsed: started.elapsed(),
    })
}

#[derive(Default)]
struct Counters {
    indexed: u64,
    skipped: u64,
}

async fn consume<R: BufRead>(
    reader: &mut ReleaseReader<R>,
    writer: &IndexWriter,
    counters: &mut Counters,
    progress: &dyn BuildProgress,
) -> Result<()> {
    while let Some(node) = reader.next_release()? {
        let release = match convert(&node) {
            Ok(release) => release,
            Err(_) => {
                counters.skipped += 1;
                continue;
            }
        };
        let row = IndexRow::new(&release, &summary_for_dump(&release))?;
        writer.insert(&row).await?;
        counters.indexed += 1;
        if counters.indexed.is_multiple_of(PROGRESS_EVERY) {
            progress.indexing(counters.indexed);
        }
    }
    Ok(())
}

fn source_name(input: &Path) -> String {
    input
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn imported_at() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// Input detection
// ---------------------------------------------------------------------------

enum Input {
    Xml(File),
    Gzip(File),
    Tar(File),
}

fn sniff(input: &Path) -> Result<Input> {
    let mut file =
        File::open(input).with_context(|| format!("cannot read {}", clean_path(input)))?;
    let mut head = Vec::with_capacity(512);
    (&mut file)
        .take(512)
        .read_to_end(&mut head)
        .with_context(|| format!("cannot read {}", clean_path(input)))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("cannot rewind {}", clean_path(input)))?;

    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(Input::Gzip(file));
    }
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Ok(Input::Tar(file));
    }
    if head.starts_with(b"BZh") {
        bail!(
            "{} is a bzip2 archive; extract it or pass the releases .xml.gz",
            clean_path(input)
        );
    }
    Ok(Input::Xml(file))
}

// ---------------------------------------------------------------------------
// Streaming XML
// ---------------------------------------------------------------------------

/// One element, kept only for the release being read.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn new(event: &BytesStart<'_>) -> Self {
        Self {
            name: event.name().as_ref().to_string(),
            attributes: event
                .attributes()
                .flatten()
                .map(|attribute| (attribute.key.as_ref().to_string(), decode(&attribute.value)))
                .collect(),
            text: String::new(),
            children: Vec::new(),
        }
    }

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    fn elements(&self, name: &str) -> impl Iterator<Item = &Node> {
        self.children.iter().filter(move |child| child.name == name)
    }

    fn trimmed(&self) -> &str {
        self.text.trim()
    }
}

fn decode(raw: &str) -> String {
    match quick_xml::escape::unescape(raw) {
        Ok(value) => value.into_owned(),
        // A malformed entity is kept verbatim rather than dropping the release;
        // the value only reaches search text and the matcher's own display.
        Err(_) => raw.to_string(),
    }
}

/// Pulls one `<release>` element at a time out of the dump stream.
struct ReleaseReader<R: BufRead> {
    reader: Reader<R>,
    buffer: Vec<u8>,
    stack: Vec<Node>,
    finished: bool,
}

impl<R: BufRead> ReleaseReader<R> {
    fn new(source: R) -> Self {
        // Text is trimmed and unescaped once, when its element closes. Trimming
        // each event would trim around entity boundaries and turn
        // `Blue &amp; Green` into `BlueGreen`.
        let reader = Reader::from_reader(source);
        Self {
            reader,
            buffer: Vec::new(),
            stack: Vec::new(),
            finished: false,
        }
    }

    fn next_release(&mut self) -> Result<Option<Node>> {
        loop {
            if self.finished {
                return Ok(None);
            }

            let mut completed: Option<Node> = None;
            let mut closed_release = false;
            {
                let event = self
                    .reader
                    .read_event_into(&mut self.buffer)
                    .context("the Discogs XML is malformed")?;
                match &event {
                    Event::Start(start) => self.stack.push(Node::new(start)),
                    Event::Empty(empty) => completed = Some(Node::new(empty)),
                    Event::Text(text) => {
                        if let Some(top) = self.stack.last_mut() {
                            top.text.push_str(text.as_ref());
                        }
                    }
                    Event::CData(text) => {
                        if let Some(top) = self.stack.last_mut() {
                            top.text.push_str(text.as_ref());
                        }
                    }
                    // Entity references arrive as their own events, so the
                    // reference is written back verbatim and resolved together
                    // with the rest of the element's text when it closes.
                    Event::GeneralRef(reference) => {
                        if let Some(top) = self.stack.last_mut() {
                            top.text.push('&');
                            top.text.push_str(reference.as_ref());
                            top.text.push(';');
                        }
                    }
                    Event::End(_) => {
                        let mut node = self
                            .stack
                            .pop()
                            .context("the Discogs XML closes an element that never opened")?;
                        node.text = decode(node.text.trim());
                        if node.name == "release" {
                            completed = Some(node);
                            closed_release = true;
                        } else {
                            completed = Some(node);
                        }
                    }
                    Event::Eof => {
                        self.finished = true;
                        if !self.stack.is_empty() {
                            bail!("the Discogs XML ends inside an element");
                        }
                        return Ok(None);
                    }
                    _ => {}
                }
            }

            if let Some(node) = completed {
                if closed_release {
                    self.buffer.clear();
                    return Ok(Some(node));
                }
                // The `<releases>` container would otherwise retain every
                // release it contains.
                if let Some(parent) = self.stack.last_mut()
                    && parent.name != "releases"
                {
                    parent.children.push(node);
                }
            }
            self.buffer.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Converts one release element into the shared model.
///
/// Fields the HTTP adapter does not read stay absent here too: no edition
/// notes, no structure findings, no provider extension, and no artist credit
/// beyond the plain names.
fn convert(node: &Node) -> Result<RemoteRelease> {
    let id = node
        .attribute("id")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .context("a Discogs release has no usable id")?;
    let title = node.child("title").map(Node::trimmed).unwrap_or_default();
    if title.is_empty() {
        bail!("a Discogs release has no title");
    }

    let artists: Vec<String> = node
        .child("artists")
        .map(|artists| {
            artists
                .elements("artist")
                .filter_map(|artist| artist.child("name"))
                .map(|name| name.trimmed().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut labels = Vec::new();
    let mut catalog_numbers = Vec::new();
    if let Some(container) = node.child("labels") {
        for label in container.elements("label") {
            if let Some(name) = label.attribute("name").map(str::trim)
                && !name.is_empty()
            {
                labels.push(name.to_string());
            }
            if let Some(catalog) = label.attribute("catno").map(str::trim)
                && !catalog.is_empty()
            {
                catalog_numbers.push(catalog.to_string());
            }
        }
    }
    catalog_numbers.sort();
    catalog_numbers.dedup();

    let mut formats = Vec::new();
    if let Some(container) = node.child("formats") {
        for format in container.elements("format") {
            let name = format.attribute("name").map(str::trim).unwrap_or_default();
            let descriptions: Vec<String> = format
                .child("descriptions")
                .map(|descriptions| {
                    descriptions
                        .elements("description")
                        .map(Node::trimmed)
                        .filter(|description| !description.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let mut text = name.to_string();
            if !descriptions.is_empty() {
                text = format!("{text} ({})", descriptions.join(", "));
            }
            let text = text.trim().to_string();
            if !text.is_empty() {
                formats.push(text);
            }
        }
    }

    let barcodes: Vec<String> = node
        .child("identifiers")
        .map(|identifiers| {
            identifiers
                .elements("identifier")
                .filter(|identifier| {
                    identifier
                        .attribute("type")
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("barcode"))
                })
                .filter_map(|identifier| identifier.attribute("value"))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let tracklist = node
        .child("tracklist")
        .map(|tracklist| tracklist.elements("track").map(convert_track).collect())
        .unwrap_or_default();

    Ok(RemoteRelease {
        id: ReleaseId::Discogs(id),
        group_id: node
            .child("master_id")
            .and_then(|master| master.trimmed().parse::<u64>().ok())
            .map(GroupId::DiscogsMaster),
        title: title.to_string(),
        artists,
        year: node
            .child("released")
            .and_then(|released| parse_year(released.trimmed())),
        country: node
            .child("country")
            .map(Node::trimmed)
            .filter(|country| !country.is_empty())
            .map(str::to_string),
        labels,
        catalog_numbers,
        formats,
        barcodes,
        tracklist,
        url: format!("https://www.discogs.com/release/{id}"),
        lookup_aliases: Vec::new(),
        artist_credit: None,
        structure_issues: Vec::new(),
        edition_notes: Vec::new(),
        provider_metadata: serde_json::Value::Null,
    })
}

fn convert_track(node: &Node) -> RemoteTrack {
    let position = node
        .child("position")
        .map(Node::trimmed)
        .filter(|position| !position.is_empty())
        .map(str::to_string);
    let artists = node
        .child("artists")
        .map(|artists| {
            artists
                .elements("artist")
                .filter_map(|artist| artist.child("name"))
                .map(Node::trimmed)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    RemoteTrack {
        // The dump carries no entry type. A missing position is the same signal
        // the web service uses for a structural (heading) entry.
        entry_type: if position.is_some() {
            TrackEntryType::Track
        } else {
            TrackEntryType::Heading
        },
        position,
        title: node
            .child("title")
            .map(Node::trimmed)
            .unwrap_or_default()
            .to_string(),
        artists,
        duration_secs: node
            .child("duration")
            .map(Node::trimmed)
            .and_then(parse_duration),
        medium_position: None,
        track_position: None,
        release_track_id: None,
        recording_id: None,
        artist_credit: None,
    }
}

fn parse_year(raw: &str) -> Option<i64> {
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    digits.parse::<i64>().ok().filter(|year| *year > 0)
}

/// The summary a Discogs search returns: the artist is part of the title and
/// the artist list is empty.
///
/// `discovery`'s detail-lookup floor and its Discogs reasoning were tuned
/// against exactly this shape, so a local summary reproduces it instead of
/// "improving" the ranking behavior offline.
fn summary_for_dump(release: &RemoteRelease) -> SearchSummary {
    SearchSummary {
        id: release.id.clone(),
        title: if release.artists.is_empty() {
            release.title.clone()
        } else {
            format!("{} - {}", release.artists.join(", "), release.title)
        },
        artists: Vec::new(),
        artist_credit: None,
        year: release.year,
        country: release.country.clone(),
        labels: release.labels.clone(),
        catalog_numbers: release.catalog_numbers.clone(),
        formats: release.formats.clone(),
        barcodes: release.barcodes.clone(),
        result_type: "release".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NoProgress;
    use crate::index::open_read_only;
    use crate::test_support::ScratchDir;
    use std::io::Write as _;

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<releases>
  <release id="42" status="Accepted">
    <title>Blue &amp; Green</title>
    <artists>
      <artist id="1"><name>Test Artist</name><anv/><join/></artist>
    </artists>
    <labels>
      <label name="Label" catno="CAT-1"/>
      <label name="" catno="CAT-2"/>
    </labels>
    <formats>
      <format name="CD" qty="1"><descriptions><description>Album</description><description>Reissue</description></descriptions></format>
    </formats>
    <country>US</country>
    <released>2001-00-00</released>
    <master_id is_main_release="true">7</master_id>
    <identifiers>
      <identifier type="Barcode" description="" value="0123456789012"/>
      <identifier type="Matrix" description="" value="IGNORED"/>
    </identifiers>
    <tracklist>
      <track><position>1</position><title>One</title><duration>3:00</duration>
        <artists><artist><name>Featured</name></artist></artists>
      </track>
      <track><position></position><title>Interlude</title></track>
      <track><position>2</position><title>Two</title><duration></duration></track>
    </tracklist>
  </release>
  <release id="43">
    <title>Second</title>
    <released>1999</released>
    <tracklist><track><position>A1</position><title>Vinyl</title><duration>1:30</duration></track></tracklist>
  </release>
  <release id="not-a-number"><title>Broken</title></release>
</releases>
"#;

    fn parse(xml: &str) -> Vec<Result<RemoteRelease>> {
        let mut reader = ReleaseReader::new(BufReader::new(xml.as_bytes()));
        let mut out = Vec::new();
        while let Some(node) = reader.next_release().unwrap() {
            out.push(convert(&node));
        }
        out
    }

    #[test]
    fn converts_a_dump_release_the_way_the_http_adapter_does() {
        let releases = parse(FIXTURE);
        assert_eq!(releases.len(), 3);
        let release = releases[0].as_ref().unwrap();

        assert_eq!(release.id, ReleaseId::Discogs(42));
        assert_eq!(release.group_id, Some(GroupId::DiscogsMaster(7)));
        assert_eq!(release.title, "Blue & Green");
        assert_eq!(release.artists, vec!["Test Artist".to_string()]);
        assert_eq!(release.year, Some(2001));
        assert_eq!(release.country.as_deref(), Some("US"));
        assert_eq!(release.labels, vec!["Label".to_string()]);
        assert_eq!(
            release.catalog_numbers,
            vec!["CAT-1".to_string(), "CAT-2".to_string()]
        );
        assert_eq!(release.formats, vec!["CD (Album, Reissue)".to_string()]);
        assert_eq!(release.barcodes, vec!["0123456789012".to_string()]);
        assert_eq!(release.url, "https://www.discogs.com/release/42");
        assert!(release.structure_issues.is_empty());
        assert!(release.edition_notes.is_empty());
        assert_eq!(release.provider_metadata, serde_json::Value::Null);

        assert_eq!(release.tracklist.len(), 3);
        assert_eq!(release.tracklist[0].title, "One");
        assert_eq!(release.tracklist[0].duration_secs, Some(180.0));
        assert_eq!(release.tracklist[0].entry_type, TrackEntryType::Track);
        assert_eq!(release.tracklist[0].artists, vec!["Featured".to_string()]);
        assert_eq!(release.tracklist[1].entry_type, TrackEntryType::Heading);
        assert_eq!(release.tracklist[1].position, None);
        assert_eq!(release.playable_tracks().len(), 2);
        assert_eq!(release.tracklist[2].duration_secs, None);
    }

    #[test]
    fn the_search_summary_keeps_the_network_title_shape() {
        let release = parse(FIXTURE)[0].as_ref().unwrap().clone();
        let summary = summary_for_dump(&release);
        assert_eq!(summary.title, "Test Artist - Blue & Green");
        assert!(summary.artists.is_empty());
        assert_eq!(summary.result_type, "release");
        assert_eq!(summary.barcodes, release.barcodes);
    }

    #[test]
    fn a_release_without_a_numeric_id_is_skipped_by_the_caller() {
        let releases = parse(FIXTURE);
        assert!(releases[2].is_err());
    }

    fn write_gz(path: &Path, contents: &str) {
        let file = File::create(path).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        encoder.write_all(contents.as_bytes()).unwrap();
        encoder.finish().unwrap();
    }

    async fn indexed_titles(paths: &IndexPaths) -> Vec<String> {
        let (pool, meta) = open_read_only(paths, IndexKind::Discogs).await.unwrap();
        assert_eq!(meta.provider, "discogs");
        let titles = sqlx::query_scalar::<_, String>("SELECT title FROM releases ORDER BY title")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;
        titles
    }

    #[tokio::test]
    async fn builds_from_plain_compressed_and_tar_wrapped_inputs() {
        let dir = ScratchDir::new("discogs-build");

        let plain = dir.path().join("releases.xml");
        std::fs::write(&plain, FIXTURE).unwrap();
        let paths = IndexPaths::from_dir(dir.path().join("plain-index"));
        let report = build(&plain, &paths, &NoProgress).await.unwrap();
        assert_eq!(report.indexed, 2);
        assert_eq!(
            indexed_titles(&paths).await,
            vec!["Blue & Green".to_string(), "Second".to_string()]
        );

        let gzip = dir.path().join("releases.xml.gz");
        write_gz(&gzip, FIXTURE);
        let paths = IndexPaths::from_dir(dir.path().join("gzip-index"));
        assert_eq!(build(&gzip, &paths, &NoProgress).await.unwrap().indexed, 2);

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut gz = Vec::new();
            {
                let mut encoder =
                    flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
                encoder.write_all(FIXTURE.as_bytes()).unwrap();
                encoder.finish().unwrap();
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(gz.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "discogs_20260101_releases.xml.gz",
                    gz.as_slice(),
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let tarred = dir.path().join("discogs_20260101_releases.tar");
        std::fs::write(&tarred, tar_bytes).unwrap();
        let paths = IndexPaths::from_dir(dir.path().join("tar-index"));
        assert_eq!(
            build(&tarred, &paths, &NoProgress).await.unwrap().indexed,
            2
        );
        assert_eq!(
            paths.file(IndexKind::Discogs).file_name().unwrap(),
            "discogs-index.db"
        );
    }

    #[tokio::test]
    async fn input_without_releases_fails_instead_of_publishing_an_empty_index() {
        let dir = ScratchDir::new("discogs-empty");
        let empty = dir.path().join("releases.xml");
        std::fs::write(&empty, "<releases></releases>").unwrap();
        let paths = IndexPaths::from_dir(dir.path().join("index"));
        let error = build(&empty, &paths, &NoProgress)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no usable releases"), "{error}");
        assert!(!paths.file(IndexKind::Discogs).exists());
    }
}
