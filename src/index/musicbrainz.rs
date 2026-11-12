//! Building the MusicBrainz index from the core dump.
//!
//! `mbdump.tar.bz2` is PostgreSQL COPY data, not JSON, and matching needs
//! `matcher::RemoteRelease` shapes. Rather than duplicate the conversion rules,
//! this builder synthesizes the `/ws/2/release` document the web service would
//! return for the same row and hands it to
//! [`crate::musicbrainz::decode_release_document`]. Network and dump candidates
//! are therefore identical by construction: same tracks, credit precedence,
//! structure findings, edition notes, and provider metadata.
//!
//! Parsing streams the archive once into a throwaway SQLite staging file (the
//! dump is sorted by primary key, not by release, so joins need a store).
//! Assembly then reads one release at a time, so memory stays bounded.

use crate::index::copy::{self, DumpSchema};
use crate::index::writer::{IndexRow, IndexWriter, summary_for};
use crate::index::{BuildProgress, BuildReport, IndexKind, IndexPaths};
use crate::musicbrainz::decode_release_document;
use crate::output::clean_path;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sqlx::Row;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
    SqliteSynchronous,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Tables staged from the dump. Optional ones only enrich a document.
const STAGED: &[&str] = &[
    "release",
    "release_group",
    "release_group_meta",
    "release_group_secondary_type_join",
    "release_group_secondary_type",
    "medium",
    "medium_format",
    "track",
    "recording",
    "artist",
    "artist_credit_name",
    "release_label",
    "label",
    "release_country",
    "area",
    "release_status",
    "release_gid_redirect",
];

/// Tables assembly cannot work without.
const REQUIRED: &[&str] = &[
    "release",
    "release_group",
    "medium",
    "medium_format",
    "track",
    "recording",
    "artist",
    "artist_credit_name",
    "release_label",
    "label",
    "release_country",
    "area",
    "release_status",
];

/// Columns each required table must carry, checked before the long parse.
const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "release",
        &[
            "id",
            "gid",
            "name",
            "artist_credit",
            "release_group",
            "status",
            "barcode",
            "comment",
        ],
    ),
    ("release_group", &["id", "gid"]),
    (
        "medium",
        &["id", "release", "position", "format", "track_count"],
    ),
    ("medium_format", &["id", "name"]),
    (
        "track",
        &[
            "id",
            "gid",
            "recording",
            "medium",
            "position",
            "number",
            "name",
            "artist_credit",
            "length",
            "is_data_track",
        ],
    ),
    ("recording", &["id", "gid", "name", "length", "video"]),
    ("artist", &["id", "gid"]),
    (
        "artist_credit_name",
        &["artist_credit", "position", "name", "join_phrase", "artist"],
    ),
    (
        "release_label",
        &["id", "release", "label", "catalog_number"],
    ),
    ("label", &["id", "name"]),
    (
        "release_country",
        &["release", "country", "date_year", "date_month", "date_day"],
    ),
    ("area", &["id", "name"]),
    ("release_status", &["id", "name"]),
];

const OPTIONAL_COLUMNS: &[(&str, &[&str])] = &[
    (
        "release_group_meta",
        &[
            "id",
            "first_release_date_year",
            "first_release_date_month",
            "first_release_date_day",
        ],
    ),
    (
        "release_group_secondary_type_join",
        &["release_group", "secondary_type"],
    ),
    ("release_group_secondary_type", &["id", "name"]),
    ("release_gid_redirect", &["gid", "new_id"]),
];

/// The dump layout this build can align COPY fields against, keyed by the
/// archive's `SCHEMA_SEQUENCE`.
///
/// Published dumps are PostgreSQL COPY exports with no DDL: they name a layout
/// version and expect the server's own `CreateTables.sql`. Columns are matched
/// positionally, so the order below is part of the dump format.
const EMBEDDED_SCHEMA_SEQUENCE: i64 = 31;

/// Column order of every table [`STAGED`] reads, for schema 31.
const EMBEDDED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "area",
        &[
            "id",
            "gid",
            "name",
            "type",
            "edits_pending",
            "last_updated",
            "begin_date_year",
            "begin_date_month",
            "begin_date_day",
            "end_date_year",
            "end_date_month",
            "end_date_day",
            "ended",
            "comment",
        ],
    ),
    (
        "release",
        &[
            "id",
            "gid",
            "name",
            "artist_credit",
            "release_group",
            "status",
            "packaging",
            "language",
            "script",
            "barcode",
            "comment",
            "edits_pending",
            "quality",
            "last_updated",
        ],
    ),
    (
        "release_group",
        &[
            "id",
            "gid",
            "name",
            "artist_credit",
            "type",
            "comment",
            "edits_pending",
            "last_updated",
        ],
    ),
    (
        "release_group_meta",
        &[
            "id",
            "release_count",
            "first_release_date_year",
            "first_release_date_month",
            "first_release_date_day",
            "rating",
            "rating_count",
        ],
    ),
    (
        "release_group_secondary_type_join",
        &["release_group", "secondary_type", "created"],
    ),
    (
        "release_group_secondary_type",
        &["id", "name", "parent", "child_order", "description", "gid"],
    ),
    (
        "medium",
        &[
            "id",
            "release",
            "position",
            "format",
            "name",
            "edits_pending",
            "last_updated",
            "track_count",
            "gid",
        ],
    ),
    (
        "medium_format",
        &[
            "id",
            "name",
            "parent",
            "child_order",
            "year",
            "has_discids",
            "description",
            "gid",
        ],
    ),
    (
        "track",
        &[
            "id",
            "gid",
            "recording",
            "medium",
            "position",
            "number",
            "name",
            "artist_credit",
            "length",
            "edits_pending",
            "last_updated",
            "is_data_track",
        ],
    ),
    (
        "recording",
        &[
            "id",
            "gid",
            "name",
            "artist_credit",
            "length",
            "comment",
            "edits_pending",
            "last_updated",
            "video",
        ],
    ),
    (
        "artist",
        &[
            "id",
            "gid",
            "name",
            "sort_name",
            "begin_date_year",
            "begin_date_month",
            "begin_date_day",
            "end_date_year",
            "end_date_month",
            "end_date_day",
            "type",
            "area",
            "gender",
            "comment",
            "edits_pending",
            "last_updated",
            "ended",
            "begin_area",
            "end_area",
        ],
    ),
    (
        "artist_credit_name",
        &["artist_credit", "position", "artist", "name", "join_phrase"],
    ),
    (
        "release_label",
        &["id", "release", "label", "catalog_number", "last_updated"],
    ),
    (
        "label",
        &[
            "id",
            "gid",
            "name",
            "begin_date_year",
            "begin_date_month",
            "begin_date_day",
            "end_date_year",
            "end_date_month",
            "end_date_day",
            "label_code",
            "type",
            "area",
            "comment",
            "edits_pending",
            "last_updated",
            "ended",
        ],
    ),
    (
        "release_country",
        &["release", "country", "date_year", "date_month", "date_day"],
    ),
    (
        "release_status",
        &["id", "name", "parent", "child_order", "description", "gid"],
    ),
    ("release_gid_redirect", &["gid", "new_id", "created"]),
];

const BATCH_ROWS: usize = 200;
const ASSEMBLY_PAGE: i64 = 500;
const PROGRESS_EVERY: u64 = 100_000;
const STAGING_PROGRESS_EVERY: u64 = 1_000_000;

/// Builds (or rebuilds) the MusicBrainz index from `mbdump.tar.bz2`.
pub async fn build(
    input: &Path,
    paths: &IndexPaths,
    progress: &dyn BuildProgress,
) -> Result<BuildReport> {
    let started = Instant::now();
    let scan = copy::read_schema(input)?;
    let schema = match scan.schema {
        Some(schema) => schema,
        None => embedded_schema(scan.sequence)
            .with_context(|| format!("cannot read the schema of {}", clean_path(input)))?,
    };
    let layout = Layout::resolve(&schema)?;

    // Removing the file is the cleanup: the guard deletes it again on any path.
    let staging_path = paths.staging(IndexKind::MusicBrainz);
    std::fs::create_dir_all(paths.dir()).with_context(|| {
        format!(
            "cannot create the index directory {}",
            clean_path(paths.dir())
        )
    })?;
    match std::fs::remove_file(&staging_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot replace {}", clean_path(&staging_path)));
        }
    }
    let _cleanup = StagingFile(staging_path.clone());
    let staging = open_staging(&staging_path).await?;
    for table in layout.tables() {
        create_staging_table(&staging, table, &layout.columns[table]).await?;
    }

    let digest = load(&staging, &layout, input, progress).await?;
    create_staging_indexes(&staging, &layout).await?;

    let writer = IndexWriter::create(paths, IndexKind::MusicBrainz).await?;
    writer.set_meta("source_file", &source_name(input)).await?;
    writer.set_meta("source_checksum", &digest).await?;
    writer
        .set_meta("checksum", verify_checksum(input, &digest)?)
        .await?;
    writer.set_meta("imported_at", &imported_at()).await?;
    if let Some(date) = &scan.dump_date {
        writer.set_meta("dump_date", date).await?;
    }

    let counters = assemble(&staging, &layout, &writer, progress).await?;
    if counters.indexed == 0 {
        bail!(
            "{} contains no usable releases; check that it is the MusicBrainz core dump",
            clean_path(input)
        );
    }
    let redirects = store_redirects(&staging, &layout, &writer).await?;
    writer
        .set_meta("indexed_releases", &counters.indexed.to_string())
        .await?;
    writer
        .set_meta("skipped_rows", &counters.skipped.to_string())
        .await?;
    writer.set_meta("redirects", &redirects.to_string()).await?;
    writer.finish().await?;
    staging.close().await;

    Ok(BuildReport {
        indexed: counters.indexed,
        skipped: counters.skipped,
        elapsed: started.elapsed(),
    })
}

/// The staged-table layout for an archive that ships no `CreateTables.sql`.
///
/// Guessing a newer layout would misalign every COPY row, so an unknown
/// sequence stops the build instead.
fn embedded_schema(sequence: Option<i64>) -> Result<DumpSchema> {
    let Some(sequence) = sequence else {
        bail!(
            "the dump has neither CreateTables.sql nor a SCHEMA_SEQUENCE; check that it is the MusicBrainz core dump"
        );
    };
    if sequence != EMBEDDED_SCHEMA_SEQUENCE {
        bail!(
            "the dump uses MusicBrainz schema {sequence}, but this Radish reads schema {EMBEDDED_SCHEMA_SEQUENCE}; use a matching dump"
        );
    }
    Ok(DumpSchema::from_columns(EMBEDDED_COLUMNS))
}

struct StagingFile(PathBuf);

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Layout {
    columns: HashMap<String, Vec<String>>,
    has_group_meta: bool,
    has_secondary_types: bool,
    has_redirects: bool,
}

impl Layout {
    fn resolve(schema: &DumpSchema) -> Result<Self> {
        let mut columns = HashMap::new();
        for table in STAGED {
            if schema.has_table(table) {
                columns.insert((*table).to_string(), schema.columns(table)?.to_vec());
            }
        }
        for table in REQUIRED {
            if !columns.contains_key(*table) {
                bail!("the MusicBrainz dump has no {table} table; check that it is mbdump.tar.bz2");
            }
        }
        for (table, required) in REQUIRED_COLUMNS {
            for column in *required {
                schema.index(table, column)?;
            }
        }
        for (table, required) in OPTIONAL_COLUMNS {
            if columns.contains_key(*table) {
                for column in *required {
                    schema.index(table, column)?;
                }
            }
        }

        Ok(Self {
            has_group_meta: columns.contains_key("release_group_meta"),
            has_secondary_types: columns.contains_key("release_group_secondary_type_join")
                && columns.contains_key("release_group_secondary_type"),
            has_redirects: columns.contains_key("release_gid_redirect"),
            columns,
        })
    }

    fn tables(&self) -> impl Iterator<Item = &String> {
        self.columns.keys()
    }
}

async fn open_staging(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Off)
        .synchronous(SqliteSynchronous::Off)
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-262144");
    SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(options)
        .await
        .with_context(|| format!("cannot create the staging database {}", clean_path(path)))
}

fn staging_name(table: &str) -> String {
    format!("mb_{table}")
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn create_staging_table(pool: &SqlitePool, table: &str, columns: &[String]) -> Result<()> {
    let columns: Vec<String> = columns.iter().map(|column| quote(column)).collect();
    let sql = format!(
        "CREATE TABLE {} ({})",
        quote(&staging_name(table)),
        columns.join(", ")
    );
    // Audited: every identifier comes from the archive's own schema and is
    // quoted; no value is interpolated here.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .with_context(|| format!("cannot stage the {table} table"))?;
    Ok(())
}

struct StageBatch {
    table: String,
    rows: Vec<Vec<Option<String>>>,
}

async fn load(
    staging: &SqlitePool,
    layout: &Layout,
    input: &Path,
    progress: &dyn BuildProgress,
) -> Result<String> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<StageBatch>(8);
    let archive = input.to_path_buf();
    let needed: HashSet<String> = layout.tables().cloned().collect();

    let parser = tokio::task::spawn_blocking(move || -> Result<String> {
        copy::for_each_table(&archive, |name, reader| {
            if !needed.contains(name) {
                return Ok(());
            }
            let mut rows: Vec<Vec<Option<String>>> = Vec::with_capacity(BATCH_ROWS);
            copy::for_each_row(reader, |fields| {
                rows.push(fields.values().to_vec());
                if rows.len() >= BATCH_ROWS {
                    let batch = StageBatch {
                        table: name.to_string(),
                        rows: std::mem::take(&mut rows),
                    };
                    if sender.blocking_send(batch).is_err() {
                        bail!("the index writer stopped reading dump rows");
                    }
                }
                Ok(())
            })?;
            if !rows.is_empty() {
                let batch = StageBatch {
                    table: name.to_string(),
                    rows,
                };
                if sender.blocking_send(batch).is_err() {
                    bail!("the index writer stopped reading dump rows");
                }
            }
            Ok(())
        })
    });

    let mut staged: HashMap<String, u64> = HashMap::new();
    while let Some(batch) = receiver.recv().await {
        let column_count = layout
            .columns
            .get(&batch.table)
            .with_context(|| format!("unexpected dump table {}", batch.table))?
            .len();
        insert_rows(staging, &batch.table, column_count, &batch.rows).await?;

        let rows = staged.entry(batch.table.clone()).or_default();
        let before = *rows;
        *rows += batch.rows.len() as u64;
        if *rows / STAGING_PROGRESS_EVERY != before / STAGING_PROGRESS_EVERY {
            progress.staging(&batch.table, *rows);
        }
    }

    parser.await.context("the dump reader stopped")?
}

async fn insert_rows(
    pool: &SqlitePool,
    table: &str,
    column_count: usize,
    rows: &[Vec<Option<String>>],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if rows.iter().any(|row| row.len() != column_count) {
        bail!("a {table} row does not match the dump schema");
    }

    let placeholders = vec!["?"; column_count].join(", ");
    let tuples = vec![format!("({placeholders})"); rows.len()].join(", ");
    let sql = format!(
        "INSERT INTO {} VALUES {tuples}",
        quote(&staging_name(table))
    );

    // Audited: the table name and placeholder count come from the dump's own
    // schema; every value is bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for row in rows {
        for value in row {
            query = query.bind(value.as_deref());
        }
    }
    query
        .execute(pool)
        .await
        .with_context(|| format!("cannot stage {table} rows"))?;
    Ok(())
}

async fn create_staging_indexes(pool: &SqlitePool, layout: &Layout) -> Result<()> {
    let mut statements = vec![
        "CREATE INDEX mb_release_id ON mb_release(id)",
        "CREATE INDEX mb_release_group_id ON mb_release_group(id)",
        "CREATE INDEX mb_medium_id ON mb_medium(id)",
        "CREATE INDEX mb_medium_release ON mb_medium(release)",
        "CREATE INDEX mb_medium_format_id ON mb_medium_format(id)",
        "CREATE INDEX mb_track_medium ON mb_track(medium)",
        "CREATE INDEX mb_recording_id ON mb_recording(id)",
        "CREATE INDEX mb_artist_id ON mb_artist(id)",
        "CREATE INDEX mb_artist_credit_name_credit ON mb_artist_credit_name(artist_credit)",
        "CREATE INDEX mb_release_label_release ON mb_release_label(release)",
        "CREATE INDEX mb_label_id ON mb_label(id)",
        "CREATE INDEX mb_release_country_release ON mb_release_country(release)",
        "CREATE INDEX mb_area_id ON mb_area(id)",
        "CREATE INDEX mb_release_status_id ON mb_release_status(id)",
    ];
    if layout.has_group_meta {
        statements.push("CREATE INDEX mb_release_group_meta_id ON mb_release_group_meta(id)");
    }
    if layout.has_secondary_types {
        statements.push(
            "CREATE INDEX mb_secondary_join_group ON mb_release_group_secondary_type_join(release_group)",
        );
    }

    for statement in statements {
        sqlx::raw_sql(statement)
            .execute(pool)
            .await
            .context("cannot index the staged tables")?;
    }
    Ok(())
}

#[derive(Default)]
struct Counters {
    indexed: u64,
    skipped: u64,
}

async fn assemble(
    staging: &SqlitePool,
    layout: &Layout,
    writer: &IndexWriter,
    progress: &dyn BuildProgress,
) -> Result<Counters> {
    let mut counters = Counters::default();
    let mut cursor: i64 = 0;

    loop {
        let rows = sqlx::query(
            "SELECT r.rowid AS rid, r.id AS id, r.gid AS gid, r.name AS name,
                    r.artist_credit AS artist_credit, r.release_group AS release_group,
                    r.barcode AS barcode, r.comment AS comment,
                    s.name AS status_name, g.gid AS group_gid
             FROM mb_release r
             LEFT JOIN mb_release_status s ON s.id = r.status
             LEFT JOIN mb_release_group g ON g.id = r.release_group
             WHERE r.rowid > ?
             ORDER BY r.rowid
             LIMIT ?",
        )
        .bind(cursor)
        .bind(ASSEMBLY_PAGE)
        .fetch_all(staging)
        .await
        .context("cannot read staged releases")?;

        if rows.is_empty() {
            return Ok(counters);
        }

        for row in &rows {
            cursor = row.get::<i64, _>("rid");
            let Some(document) = document(staging, layout, row).await? else {
                counters.skipped += 1;
                continue;
            };
            let release = match decode_release_document(document) {
                Ok(release) => release,
                // One unusual release must not end an hour-long build; the
                // skipped count reports it.
                Err(_) => {
                    counters.skipped += 1;
                    continue;
                }
            };
            let index_row = IndexRow::new(&release, &summary_for(&release))?;
            writer.insert(&index_row).await?;
            counters.indexed += 1;
            if counters.indexed.is_multiple_of(PROGRESS_EVERY) {
                progress.indexing(counters.indexed);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Credit {
    name: String,
    join_phrase: Option<String>,
    artist_gid: Option<String>,
}

async fn document(staging: &SqlitePool, layout: &Layout, row: &SqliteRow) -> Result<Option<Value>> {
    let Some(gid) = text(row, "gid") else {
        return Ok(None);
    };
    let title = text(row, "name").unwrap_or_default();
    if title.trim().is_empty() {
        return Ok(None);
    }
    let Some(release_id) = text(row, "id") else {
        return Ok(None);
    };

    let mut object = Map::new();
    object.insert("id".to_string(), json!(gid));
    object.insert("title".to_string(), json!(title));

    let release_credits = credits_for(staging, text(row, "artist_credit").as_deref()).await?;
    object.insert("artist-credit".to_string(), credits_json(&release_credits));

    if let Some(barcode) = text(row, "barcode").filter(|value| !value.trim().is_empty()) {
        object.insert("barcode".to_string(), json!(barcode));
    }
    if let Some(status) = text(row, "status_name").filter(|value| !value.trim().is_empty()) {
        object.insert("status".to_string(), json!(status));
    }
    if let Some(comment) = text(row, "comment").filter(|value| !value.trim().is_empty()) {
        object.insert("disambiguation".to_string(), json!(comment));
    }

    let events = sqlx::query(
        "SELECT c.name AS area, rc.date_year AS year, rc.date_month AS month,
                rc.date_day AS day
         FROM mb_release_country rc
         LEFT JOIN mb_area c ON c.id = rc.country
         WHERE rc.release = ?
         ORDER BY CAST(rc.date_year AS INTEGER), CAST(rc.date_month AS INTEGER),
                  CAST(rc.date_day AS INTEGER), CAST(rc.country AS INTEGER)",
    )
    .bind(&release_id)
    .fetch_all(staging)
    .await
    .context("cannot read staged release events")?;

    let mut release_events = Vec::new();
    let mut date = None;
    for event in &events {
        let event_date = date_string(
            number(event, "year"),
            number(event, "month"),
            number(event, "day"),
        );
        if date.is_none() {
            date.clone_from(&event_date);
        }
        release_events.push(json!({
            "date": event_date,
            "area": match text(event, "area").filter(|name| !name.trim().is_empty()) {
                Some(name) => json!({ "name": name }),
                None => Value::Null,
            },
        }));
    }
    object.insert("release-events".to_string(), Value::Array(release_events));
    if let Some(date) = date {
        object.insert("date".to_string(), json!(date));
    }

    let labels = sqlx::query(
        "SELECT l.name AS name, rl.catalog_number AS catalog_number
         FROM mb_release_label rl
         LEFT JOIN mb_label l ON l.id = rl.label
         WHERE rl.release = ?
         ORDER BY CAST(rl.id AS INTEGER)",
    )
    .bind(&release_id)
    .fetch_all(staging)
    .await
    .context("cannot read staged labels")?;

    let mut label_info = Vec::new();
    for label in &labels {
        let name = text(label, "name").filter(|value| !value.trim().is_empty());
        let catalog = text(label, "catalog_number").filter(|value| !value.trim().is_empty());
        if name.is_none() && catalog.is_none() {
            continue;
        }
        label_info.push(json!({
            "label": match name {
                Some(name) => json!({ "name": name }),
                None => Value::Null,
            },
            "catalog-number": catalog,
        }));
    }
    object.insert("label-info".to_string(), Value::Array(label_info));

    if let Some(group_id) = text(row, "release_group")
        && let Some(group_gid) = text(row, "group_gid")
    {
        let mut secondary_types = Vec::new();
        if layout.has_secondary_types {
            secondary_types = sqlx::query_scalar::<_, String>(
                "SELECT t.name
                 FROM mb_release_group_secondary_type_join j
                 JOIN mb_release_group_secondary_type t ON t.id = j.secondary_type
                 WHERE j.release_group = ?
                 ORDER BY CAST(t.id AS INTEGER)",
            )
            .bind(&group_id)
            .fetch_all(staging)
            .await
            .context("cannot read staged release group types")?;
        }

        let mut first_release_date = None;
        if layout.has_group_meta {
            let rows = sqlx::query(
                "SELECT first_release_date_year AS year, first_release_date_month AS month,
                        first_release_date_day AS day
                 FROM mb_release_group_meta
                 WHERE id = ?",
            )
            .bind(&group_id)
            .fetch_all(staging)
            .await
            .context("cannot read staged release group metadata")?;
            if let Some(meta) = rows.first() {
                first_release_date = date_string(
                    number(meta, "year"),
                    number(meta, "month"),
                    number(meta, "day"),
                );
            }
        }

        let mut group = Map::new();
        group.insert("id".to_string(), json!(group_gid));
        group.insert("secondary-types".to_string(), json!(secondary_types));
        if let Some(date) = first_release_date {
            group.insert("first-release-date".to_string(), json!(date));
        }
        object.insert("release-group".to_string(), Value::Object(group));
    }

    object.insert("media".to_string(), media(staging, &release_id).await?);

    Ok(Some(Value::Object(object)))
}

async fn media(staging: &SqlitePool, release_id: &str) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT m.id AS id, m.position AS position, m.track_count AS track_count,
                f.name AS format,
                (SELECT COUNT(*) FROM mb_track t
                 WHERE t.medium = m.id AND CAST(t.position AS INTEGER) = 0) AS pregap_count
         FROM mb_medium m
         LEFT JOIN mb_medium_format f ON f.id = m.format
         WHERE m.release = ?
         ORDER BY CAST(m.position AS INTEGER), CAST(m.id AS INTEGER)",
    )
    .bind(release_id)
    .fetch_all(staging)
    .await
    .context("cannot read staged media")?;

    let mut media = Vec::new();
    for medium in &rows {
        let Some(medium_id) = text(medium, "id") else {
            continue;
        };
        let (tracks, data_tracks) = tracks_for(staging, &medium_id).await?;

        let mut medium_json = Map::new();
        if let Some(position) = number(medium, "position") {
            medium_json.insert("position".to_string(), json!(position));
        }
        if let Some(format) = text(medium, "format").filter(|value| !value.trim().is_empty()) {
            medium_json.insert("format".to_string(), json!(format));
        }
        if let Some(count) = number(medium, "track_count") {
            medium_json.insert("track-count".to_string(), json!(count));
        }
        if number(medium, "pregap_count").unwrap_or_default() > 0 {
            medium_json.insert("pregap".to_string(), json!({ "position": 0 }));
        }
        if !data_tracks.is_empty() {
            medium_json.insert("data-tracks".to_string(), Value::Array(data_tracks));
        }
        medium_json.insert("tracks".to_string(), Value::Array(tracks));
        media.push(Value::Object(medium_json));
    }

    Ok(Value::Array(media))
}

/// Returns the playable track documents and the structural data tracks.
///
/// The web service lists data tracks inside `tracks` as well, so the declared
/// medium count still matches the parsed count; only pregap tracks (position
/// zero) sit outside the list.
async fn tracks_for(staging: &SqlitePool, medium_id: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    let rows = sqlx::query(
        "SELECT t.id AS id, t.gid AS gid, t.position AS position, t.number AS number,
                t.name AS name, t.artist_credit AS artist_credit, t.length AS length,
                t.is_data_track AS is_data_track,
                r.gid AS recording_gid, r.name AS recording_name,
                r.length AS recording_length, r.video AS video
         FROM mb_track t
         LEFT JOIN mb_recording r ON r.id = t.recording
         WHERE t.medium = ? AND CAST(t.position AS INTEGER) > 0
         ORDER BY CAST(t.position AS INTEGER), CAST(t.id AS INTEGER)",
    )
    .bind(medium_id)
    .fetch_all(staging)
    .await
    .context("cannot read staged tracks")?;

    let credit_ids: Vec<String> = rows
        .iter()
        .filter_map(|track| text(track, "artist_credit"))
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();
    let track_credits = credits_map(staging, &credit_ids).await?;

    let mut data_tracks = Vec::new();
    let mut tracks = Vec::new();
    for track in &rows {
        if flag(track, "is_data_track") {
            data_tracks.push(json!({ "position": 0 }));
        }

        let credits = text(track, "artist_credit")
            .and_then(|credit_id| track_credits.get(&credit_id).cloned())
            .unwrap_or_default();

        let mut entry = Map::new();
        if let Some(gid) = text(track, "gid") {
            entry.insert("id".to_string(), json!(gid));
        }
        entry.insert(
            "title".to_string(),
            json!(text(track, "name").unwrap_or_default()),
        );
        if let Some(number) = text(track, "number") {
            entry.insert("number".to_string(), json!(number));
        }
        if let Some(position) = number(track, "position") {
            entry.insert("position".to_string(), json!(position));
        }
        if let Some(length) = number(track, "length") {
            entry.insert("length".to_string(), json!(length));
        }
        if !credits.is_empty() {
            entry.insert("artist-credit".to_string(), credits_json(&credits));
        }

        let mut recording = Map::new();
        if let Some(gid) = text(track, "recording_gid") {
            recording.insert("id".to_string(), json!(gid));
        }
        if let Some(name) = text(track, "recording_name") {
            recording.insert("title".to_string(), json!(name));
        }
        if let Some(length) = number(track, "recording_length") {
            recording.insert("length".to_string(), json!(length));
        }
        recording.insert("video".to_string(), json!(flag(track, "video")));
        entry.insert("recording".to_string(), Value::Object(recording));

        tracks.push(Value::Object(entry));
    }

    Ok((tracks, data_tracks))
}

async fn credits_for(staging: &SqlitePool, credit_id: Option<&str>) -> Result<Vec<Credit>> {
    let Some(credit_id) = credit_id else {
        return Ok(Vec::new());
    };
    let mut map = credits_map(staging, &[credit_id.to_string()]).await?;
    Ok(map.remove(credit_id).unwrap_or_default())
}

async fn credits_map(
    staging: &SqlitePool,
    credit_ids: &[String],
) -> Result<HashMap<String, Vec<Credit>>> {
    if credit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; credit_ids.len()].join(", ");
    let sql = format!(
        "SELECT n.artist_credit AS artist_credit, n.name AS name,
                n.join_phrase AS join_phrase, a.gid AS artist_gid
         FROM mb_artist_credit_name n
         LEFT JOIN mb_artist a ON a.id = n.artist
         WHERE n.artist_credit IN ({placeholders})
         ORDER BY n.artist_credit, CAST(n.position AS INTEGER)"
    );
    // Audited: only the placeholder count is interpolated; every id is bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for credit_id in credit_ids {
        query = query.bind(credit_id);
    }

    let mut credits: HashMap<String, Vec<Credit>> = HashMap::new();
    for row in query
        .fetch_all(staging)
        .await
        .context("cannot read staged credits")?
    {
        let Some(credit_id) = text(&row, "artist_credit") else {
            continue;
        };
        credits.entry(credit_id).or_default().push(Credit {
            name: text(&row, "name").unwrap_or_default(),
            join_phrase: text(&row, "join_phrase").filter(|join| !join.is_empty()),
            artist_gid: text(&row, "artist_gid"),
        });
    }
    Ok(credits)
}

fn credits_json(credits: &[Credit]) -> Value {
    Value::Array(
        credits
            .iter()
            .map(|credit| {
                let mut credit_json = Map::new();
                credit_json.insert("name".to_string(), json!(credit.name));
                if let Some(join) = &credit.join_phrase {
                    credit_json.insert("joinphrase".to_string(), json!(join));
                }
                if let Some(artist) = &credit.artist_gid {
                    credit_json.insert("artist".to_string(), json!({ "id": artist }));
                }
                Value::Object(credit_json)
            })
            .collect(),
    )
}

async fn store_redirects(
    staging: &SqlitePool,
    layout: &Layout,
    writer: &IndexWriter,
) -> Result<u64> {
    if !layout.has_redirects {
        return Ok(0);
    }
    let rows = sqlx::query(
        "SELECT r.gid AS gid, t.gid AS target_gid
         FROM mb_release_gid_redirect r
         JOIN mb_release t ON t.id = r.new_id",
    )
    .fetch_all(staging)
    .await
    .context("cannot read staged release redirects")?;

    let mut stored = 0;
    for row in &rows {
        if let (Some(gid), Some(target)) = (text(row, "gid"), text(row, "target_gid")) {
            writer.insert_redirect(&gid, &target).await?;
            stored += 1;
        }
    }
    Ok(stored)
}

/// Reads a staged text column, treating SQL null as absent.
fn text(row: &SqliteRow, column: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(column).ok().flatten()
}

fn number(row: &SqliteRow, column: &str) -> Option<i64> {
    // Staged columns are text, but computed columns such as a `COUNT(*)`
    // subquery come back as integers, so both shapes are accepted.
    if let Ok(value) = row.try_get::<Option<i64>, _>(column) {
        return value;
    }
    text(row, column).and_then(|value| value.trim().parse::<i64>().ok())
}

fn flag(row: &SqliteRow, column: &str) -> bool {
    text(row, column)
        .map(|value| value.trim() == "t")
        .unwrap_or(false)
}

/// Formats a partial MusicBrainz date the way the web service does.
fn date_string(year: Option<i64>, month: Option<i64>, day: Option<i64>) -> Option<String> {
    let year = year.filter(|year| *year > 0)?;
    match month.filter(|month| (1..=12).contains(month)) {
        Some(month) => match day.filter(|day| (1..=31).contains(day)) {
            Some(day) => Some(format!("{year:04}-{month:02}-{day:02}")),
            None => Some(format!("{year:04}-{month:02}")),
        },
        None => Some(format!("{year:04}")),
    }
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

/// Compares the streamed digest with a sibling `MD5SUMS` entry when present.
fn verify_checksum(input: &Path, digest: &str) -> Result<&'static str> {
    let name = input
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let sums = input
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("MD5SUMS");
    let Ok(contents) = std::fs::read_to_string(&sums) else {
        return Ok("not published");
    };

    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let (Some(expected), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        if file.trim_start_matches('*') == name {
            if expected.eq_ignore_ascii_case(digest) {
                return Ok("verified");
            }
            bail!(
                "{} does not match the published MD5SUMS entry; download it again",
                clean_path(input)
            );
        }
    }
    Ok("not published")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NoProgress;
    use crate::index::open_read_only;
    use crate::test_support::{ScratchDir, write_mb_archive};

    const RELEASE_GID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
    const GROUP_GID: &str = "4642ee19-7790-3c8d-ab5e-d133de942db6";
    const ARTIST_GID: &str = "0f2b0c7d-2222-4c1d-9a51-1d2f2b0a5a11";
    const PREGAP_GID: &str = "11111111-1111-1111-1111-111111111111";
    const TRACK_ONE_GID: &str = "22222222-2222-2222-2222-222222222222";
    const TRACK_TWO_GID: &str = "33333333-3333-3333-3333-333333333333";
    const RECORDING_ONE_GID: &str = "aaaaaaaa-1111-1111-1111-111111111111";
    const RECORDING_TWO_GID: &str = "bbbbbbbb-2222-2222-2222-222222222222";
    const REDIRECT_GID: &str = "99999999-9999-9999-9999-999999999999";

    fn schema() -> &'static str {
        "CREATE TABLE release (\n id SERIAL,\n gid UUID NOT NULL,\n name VARCHAR NOT NULL,\n artist_credit INTEGER NOT NULL,\n release_group INTEGER NOT NULL,\n status INTEGER,\n barcode VARCHAR,\n comment VARCHAR(255) NOT NULL DEFAULT ''\n);\n\
         CREATE TABLE release_group (id SERIAL, gid UUID NOT NULL, name VARCHAR NOT NULL);\n\
         CREATE TABLE release_group_meta (id INTEGER PRIMARY KEY, first_release_date_year SMALLINT, first_release_date_month SMALLINT, first_release_date_day SMALLINT);\n\
         CREATE TABLE release_group_secondary_type_join (release_group INTEGER NOT NULL, secondary_type INTEGER NOT NULL);\n\
         CREATE TABLE release_group_secondary_type (id SERIAL, name VARCHAR NOT NULL);\n\
         CREATE TABLE medium (id SERIAL, release INTEGER NOT NULL, position INTEGER NOT NULL, format INTEGER, track_count INTEGER NOT NULL DEFAULT 0);\n\
         CREATE TABLE medium_format (id SERIAL, name VARCHAR NOT NULL);\n\
         CREATE TABLE track (\n id SERIAL,\n gid UUID NOT NULL,\n recording INTEGER NOT NULL,\n medium INTEGER NOT NULL,\n position INTEGER NOT NULL,\n number TEXT NOT NULL,\n name VARCHAR NOT NULL,\n artist_credit INTEGER NOT NULL,\n length INTEGER,\n is_data_track BOOLEAN NOT NULL DEFAULT FALSE\n);\n\
         CREATE TABLE recording (id SERIAL, gid UUID NOT NULL, name VARCHAR NOT NULL, length INTEGER, video BOOLEAN NOT NULL DEFAULT FALSE);\n\
         CREATE TABLE artist (id SERIAL, gid UUID NOT NULL);\n\
         CREATE TABLE artist_credit_name (artist_credit INTEGER NOT NULL, position INTEGER NOT NULL, artist INTEGER NOT NULL, name VARCHAR NOT NULL, join_phrase TEXT NOT NULL DEFAULT '');\n\
         CREATE TABLE release_label (id SERIAL, release INTEGER NOT NULL, label INTEGER, catalog_number VARCHAR);\n\
         CREATE TABLE label (id SERIAL, name VARCHAR NOT NULL);\n\
         CREATE TABLE release_country (release INTEGER NOT NULL, country INTEGER NOT NULL, date_year SMALLINT, date_month SMALLINT, date_day SMALLINT);\n\
         CREATE TABLE area (id SERIAL, name VARCHAR NOT NULL);\n\
         CREATE TABLE release_status (id SERIAL, name VARCHAR NOT NULL);\n\
         CREATE TABLE release_gid_redirect (gid UUID NOT NULL, new_id INTEGER NOT NULL);\n"
    }

    fn archive(dir: &Path) -> PathBuf {
        let rows = vec![
            ("TIMESTAMP", "2026-11-11 00:23:36.560115+00\n".to_string()),
            ("CreateTables.sql", schema().to_string()),
            (
                "release",
                format!("1\t{RELEASE_GID}\tBlue\t1\t1\t1\t0123456789012\t"),
            ),
            ("release_group", format!("1\t{GROUP_GID}\tBlue")),
            ("release_group_meta", "1\t2001\t3\t4".to_string()),
            ("release_group_secondary_type_join", "1\t1".to_string()),
            ("release_group_secondary_type", "1\tAlbum".to_string()),
            ("medium", "1\t1\t1\t1\t2".to_string()),
            ("medium_format", "1\tCD".to_string()),
            (
                "track",
                format!(
                    "1\t{PREGAP_GID}\t1\t1\t0\t0\tPregap\t1\t120000\tf\n\
                     2\t{TRACK_ONE_GID}\t1\t1\t1\t1\tOne\t1\t180000\tf\n\
                     3\t{TRACK_TWO_GID}\t2\t1\t2\t2\tTwo\t2\t150000\tf"
                ),
            ),
            (
                "recording",
                format!(
                    "1\t{RECORDING_ONE_GID}\tOne\t180000\tf\n\
                     2\t{RECORDING_TWO_GID}\tTwo\t150000\tf"
                ),
            ),
            ("artist", format!("1\t{ARTIST_GID}")),
            (
                "artist_credit_name",
                "1\t0\t1\tTest Artist\t\n\
                 2\t0\t1\tTest Artist\t & \n\
                 2\t1\t1\tGuest\t"
                    .to_string(),
            ),
            ("release_label", "1\t1\t1\tCAT-1".to_string()),
            ("label", "1\tLabel".to_string()),
            ("release_country", "1\t1\t2001\t1\t2".to_string()),
            ("area", "1\tUS".to_string()),
            ("release_status", "1\tOfficial".to_string()),
            ("release_gid_redirect", format!("{REDIRECT_GID}\t1")),
        ];
        let files: Vec<(&str, &str)> = rows
            .iter()
            .map(|(name, body)| (*name, body.as_str()))
            .collect();
        write_mb_archive(dir, &files)
    }

    fn expected_document() -> Value {
        json!({
            "id": RELEASE_GID,
            "title": "Blue",
            "artist-credit": [{
                "name": "Test Artist",
                "artist": { "id": ARTIST_GID }
            }],
            "release-group": {
                "id": GROUP_GID,
                "secondary-types": ["Album"],
                "first-release-date": "2001-03-04"
            },
            "date": "2001-01-02",
            "status": "Official",
            "barcode": "0123456789012",
            "label-info": [{ "label": { "name": "Label" }, "catalog-number": "CAT-1" }],
            "release-events": [{ "date": "2001-01-02", "area": { "name": "US" } }],
            "media": [{
                "position": 1,
                "format": "CD",
                "track-count": 2,
                "pregap": { "position": 0 },
                "tracks": [
                    {
                        "id": TRACK_ONE_GID,
                        "title": "One",
                        "number": "1",
                        "position": 1,
                        "length": 180000,
                        "artist-credit": [{
                            "name": "Test Artist",
                            "artist": { "id": ARTIST_GID }
                        }],
                        "recording": {
                            "id": RECORDING_ONE_GID,
                            "title": "One",
                            "length": 180000,
                            "video": false
                        }
                    },
                    {
                        "id": TRACK_TWO_GID,
                        "title": "Two",
                        "number": "2",
                        "position": 2,
                        "length": 150000,
                        "artist-credit": [
                            { "name": "Test Artist", "joinphrase": " & ", "artist": { "id": ARTIST_GID } },
                            { "name": "Guest", "artist": { "id": ARTIST_GID } }
                        ],
                        "recording": {
                            "id": RECORDING_TWO_GID,
                            "title": "Two",
                            "length": 150000,
                            "video": false
                        }
                    }
                ]
            }]
        })
    }

    #[tokio::test]
    async fn the_index_holds_exactly_what_the_network_decoder_returns() {
        let dir = ScratchDir::new("mb-build");
        let archive = archive(dir.path());
        let paths = IndexPaths::from_dir(dir.path().join("index"));
        let report = build(&archive, &paths, &NoProgress).await.unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(report.skipped, 0);

        let (pool, meta) = open_read_only(&paths, IndexKind::MusicBrainz)
            .await
            .unwrap();
        let document: Vec<u8> = sqlx::query_scalar("SELECT document FROM releases WHERE id = ?")
            .bind(RELEASE_GID)
            .fetch_one(&pool)
            .await
            .unwrap();
        let stored = crate::index::writer::decode_document(&document).unwrap();
        let expected = decode_release_document(expected_document()).unwrap();
        assert_eq!(stored, expected);
        assert_eq!(meta.indexed_releases, 1);
        assert_eq!(meta.checksum.as_deref(), Some("not published"));
        assert_eq!(meta.dump_date.as_deref(), Some("2026-11-11"));

        let redirects: i64 = sqlx::query_scalar("SELECT count(*) FROM redirects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(redirects, 1);
        let barcodes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM identifiers WHERE kind = 'barcode'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(barcodes, 1);
        let fts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM releases_fts WHERE releases_fts MATCH 'title_norm : (blue)'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts, 1);
        pool.close().await;
    }

    #[test]
    fn the_document_reports_the_same_structure_findings_as_the_network() {
        let expected = decode_release_document(expected_document()).unwrap();
        assert!(
            expected
                .structure_issues
                .iter()
                .any(|issue| issue.contains("pregap")),
            "{:?}",
            expected.structure_issues
        );
        assert_eq!(expected.edition_notes, vec!["Album".to_string()]);
        assert_eq!(expected.playable_tracks().len(), 2);
        assert_eq!(
            expected.provider_metadata["first_release_date"],
            "2001-03-04"
        );
    }

    #[test]
    fn dates_format_like_the_web_service() {
        assert_eq!(date_string(Some(1969), None, None).as_deref(), Some("1969"));
        assert_eq!(
            date_string(Some(1969), Some(7), None).as_deref(),
            Some("1969-07")
        );
        assert_eq!(
            date_string(Some(1969), Some(7), Some(20)).as_deref(),
            Some("1969-07-20")
        );
        assert_eq!(date_string(Some(0), Some(7), Some(20)), None);
        assert_eq!(
            date_string(Some(1969), Some(13), Some(0)).as_deref(),
            Some("1969")
        );
    }

    #[tokio::test]
    async fn an_archive_without_a_schema_is_refused_before_any_parsing() {
        let dir = ScratchDir::new("mb-no-schema");
        let archive = write_mb_archive(dir.path(), &[("release", "1\tx\n")]);
        let paths = IndexPaths::from_dir(dir.path().join("index"));
        let error = build(&archive, &paths, &NoProgress).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("CreateTables.sql"), "{text}");
        assert!(!paths.file(IndexKind::MusicBrainz).exists());
    }

    #[test]
    fn the_embedded_schema_matches_every_staged_table() {
        let schema = embedded_schema(Some(EMBEDDED_SCHEMA_SEQUENCE)).unwrap();
        let layout = Layout::resolve(&schema).unwrap();
        assert_eq!(layout.tables().count(), STAGED.len());
    }

    #[test]
    fn an_unknown_schema_sequence_is_refused_instead_of_guessed() {
        let error = embedded_schema(Some(EMBEDDED_SCHEMA_SEQUENCE + 1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema"), "{error}");
        assert!(embedded_schema(None).is_err());
    }

    #[tokio::test]
    async fn a_dump_missing_a_required_table_is_refused() {
        let dir = ScratchDir::new("mb-missing-table");
        let archive = write_mb_archive(
            dir.path(),
            &[
                (
                    "CreateTables.sql",
                    "CREATE TABLE release (\n id SERIAL,\n gid UUID\n);",
                ),
                ("release", "1\tx\n"),
            ],
        );
        let paths = IndexPaths::from_dir(dir.path().join("index"));
        let error = build(&archive, &paths, &NoProgress)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no release_group table"), "{error}");
    }
}
