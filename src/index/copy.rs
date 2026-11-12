//! Reading the PostgreSQL COPY-format tables inside `mbdump.tar.bz2`.
//!
//! The archive is a tar stream compressed with bzip2, and each entry is one
//! table in PostgreSQL COPY text format: tab-separated fields, `\N` for null,
//! and backslash escapes. Nothing here needs PostgreSQL. Column names come
//! from the archive's own `CreateTables.sql` when it carries one, or from the
//! layout its `SCHEMA_SEQUENCE` names, so rows are read by name and a future
//! column insertion cannot silently misalign the parser.

use crate::output::clean_path;
use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// One COPY row, with nulls kept distinct from empty strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyFields(Vec<Option<String>>);

impl CopyFields {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The field as text, without trimming (dump values may have meaning in
    /// their spacing, and every consumer decides for itself).
    pub fn text(&self, index: usize) -> Option<&str> {
        self.0.get(index).and_then(|field| field.as_deref())
    }

    /// The raw field values, for a staged insert.
    pub fn values(&self) -> &[Option<String>] {
        &self.0
    }

    /// A field the assembler cannot work without.
    pub fn require(&self, table: &str, column: &str, index: usize) -> Result<&str> {
        self.text(index)
            .with_context(|| format!("{table} row has no {column} in a column the index needs"))
    }

    pub fn int(&self, table: &str, column: &str, index: usize) -> Result<Option<i64>> {
        let Some(raw) = self.text(index) else {
            return Ok(None);
        };
        match raw.trim().parse::<i64>() {
            Ok(value) => Ok(Some(value)),
            Err(_) => bail!("{table} row has a non-numeric {column}"),
        }
    }

    /// PostgreSQL writes booleans as `t`/`f`; a null boolean is false.
    pub fn flag(&self, table: &str, column: &str, index: usize) -> Result<bool> {
        match self.text(index).map(str::trim) {
            None | Some("f") => Ok(false),
            Some("t") => Ok(true),
            Some(_) => bail!("{table} row has an unreadable {column} flag"),
        }
    }
}

/// Splits one COPY line into fields, resolving escapes and nulls.
///
/// Errors name the field position but never the value: dump titles are
/// untrusted text and may contain terminal control sequences.
pub fn parse_copy_fields(line: &str) -> Result<CopyFields> {
    let mut fields = Vec::new();
    for (index, raw) in line.split('\t').enumerate() {
        if raw == "\\N" {
            fields.push(None);
            continue;
        }
        fields.push(Some(unescape(raw).with_context(|| {
            format!("COPY field {} is malformed", index + 1)
        })?));
    }
    Ok(CopyFields(fields))
}

fn unescape(raw: &str) -> Result<String> {
    if !raw.contains('\\') {
        return Ok(raw.to_string());
    }

    // COPY escapes octal sequences as UTF-8 *bytes*, so escapes are collected
    // as bytes and decoded once at the end; decoding each escape as a code
    // point would turn `\303\263` into two wrong characters instead of `ó`.
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buffer = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('v') => out.push(0x0b),
            Some('\\') => out.push(b'\\'),
            Some(first @ '0'..='7') => {
                let mut value = first.to_digit(8).expect("an octal digit");
                for _ in 0..2 {
                    let Some(next @ '0'..='7') = chars.next() else {
                        bail!("a COPY octal escape is not three digits");
                    };
                    value = value * 8 + next.to_digit(8).expect("an octal digit");
                }
                let byte = u8::try_from(value)
                    .map_err(|_| anyhow::anyhow!("a COPY octal escape is not a byte"))?;
                out.push(byte);
            }
            Some(_) => bail!("unknown backslash escape"),
            None => bail!("a field ends with a lone backslash"),
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("a COPY field is not valid UTF-8"))
}

/// The column layout of every table in the archive.
#[derive(Debug, Clone, Default)]
pub struct DumpSchema {
    tables: HashMap<String, Vec<String>>,
}

impl DumpSchema {
    /// Parses `mbdump/CreateTables.sql`.
    ///
    /// Statements are split at top-level semicolons and columns at top-level
    /// commas, so both the dump's multi-line layout and a compact one-line
    /// definition read the same way. Parenthesised type arguments such as
    /// `VARCHAR(255)` never split a column.
    pub fn parse(sql: &str) -> Self {
        let mut tables: HashMap<String, Vec<String>> = HashMap::new();

        for statement in sql.split(';') {
            let Some(rest) = statement.trim().strip_prefix("CREATE TABLE ") else {
                continue;
            };
            let Some((name, body)) = rest.split_once('(') else {
                continue;
            };
            let name = name
                .trim()
                .trim_matches('"')
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let columns = column_names(body);
            if !columns.is_empty() {
                tables.insert(name, columns);
            }
        }

        Self { tables }
    }

    /// Builds a layout from column lists kept in code, for archives that name
    /// a schema version instead of shipping its DDL.
    pub fn from_columns(tables: &[(&str, &[&str])]) -> Self {
        let tables = tables
            .iter()
            .map(|(table, columns)| {
                (
                    (*table).to_string(),
                    columns.iter().map(|column| (*column).to_string()).collect(),
                )
            })
            .collect();
        Self { tables }
    }

    pub fn has_table(&self, table: &str) -> bool {
        self.tables.contains_key(table)
    }

    pub fn columns(&self, table: &str) -> Result<&[String]> {
        self.tables
            .get(table)
            .map(Vec::as_slice)
            .with_context(|| format!("the dump schema has no {table} table"))
    }

    /// The position of a column, resolved by name.
    pub fn index(&self, table: &str, column: &str) -> Result<usize> {
        let columns = self.columns(table)?;
        columns
            .iter()
            .position(|candidate| candidate == column)
            .with_context(|| format!("the {table} table has no {column} column"))
    }
}

/// Column names in one `CREATE TABLE` body, split at top-level commas.
fn column_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();

    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = (depth - 1).max(0);
                current.push(ch);
            }
            ',' if depth == 0 => flush_column(&mut current, &mut names),
            _ => current.push(ch),
        }
    }
    flush_column(&mut current, &mut names);
    names
}

fn flush_column(current: &mut String, names: &mut Vec<String>) {
    let Some(first) = current.split_whitespace().next() else {
        current.clear();
        return;
    };
    let first = first.trim_matches(|ch| ch == '"' || ch == ',');
    if !first.is_empty()
        && !matches!(
            first,
            "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "EXCLUDE"
        )
    {
        names.push(first.to_string());
    }
    current.clear();
}

/// What the archive says about its own table layout.
pub struct SchemaScan {
    /// `mbdump/CreateTables.sql`, when the archive ships one before its data.
    pub schema: Option<DumpSchema>,
    /// The archive's `SCHEMA_SEQUENCE` number, when it declares one.
    pub sequence: Option<i64>,
    /// The export's own date (`YYYY-MM-DD`) from its `TIMESTAMP` entry.
    ///
    /// The file's modification time says when the dump was downloaded or
    /// copied, not when MusicBrainz exported it, so it is never a substitute.
    pub dump_date: Option<String>,
}

/// The `YYYY-MM-DD` prefix of a `TIMESTAMP` entry such as
/// `2026-11-11 00:23:36.560115+00`, or `None` when it is not a date.
fn parse_dump_date(text: &str) -> Option<String> {
    let date = text.trim().get(..10)?;
    let mut parts = date.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    let valid = parts.next().is_none()
        && year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && [year, month, day]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        && (1..=12).contains(&month.parse::<u32>().ok()?)
        && (1..=31).contains(&day.parse::<u32>().ok()?);
    valid.then(|| date.to_string())
}

/// Reads the archive's schema header without decompressing the whole dump.
///
/// Published MusicBrainz archives carry no DDL: they are PostgreSQL COPY
/// exports meant to be loaded with the server software's own `CreateTables.sql`.
/// Those archives declare their layout with `SCHEMA_SEQUENCE` instead, so the
/// scan stops at the first data table and reports that number. An archive that
/// does ship `CreateTables.sql` (hand-built or future ones) is read directly.
pub fn read_schema(archive: &Path) -> Result<SchemaScan> {
    let file =
        File::open(archive).with_context(|| format!("cannot read {}", clean_path(archive)))?;
    let decoder = bzip2::read::BzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);
    let mut sequence = None;
    let mut dump_date = None;

    for entry in tar
        .entries()
        .with_context(|| format!("{} is not a tar archive", clean_path(archive)))?
    {
        let mut entry = entry.with_context(|| format!("{} is truncated", clean_path(archive)))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .with_context(|| format!("{} has an unreadable entry", clean_path(archive)))?
            .to_path_buf();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if name == "CreateTables.sql" {
            let mut sql = String::new();
            entry
                .read_to_string(&mut sql)
                .with_context(|| format!("{} has an unreadable schema", clean_path(archive)))?;
            return Ok(SchemaScan {
                schema: Some(DumpSchema::parse(&sql)),
                sequence,
                dump_date,
            });
        }
        if name == "SCHEMA_SEQUENCE" {
            let mut text = String::new();
            entry.read_to_string(&mut text).with_context(|| {
                format!("{} has an unreadable schema sequence", clean_path(archive))
            })?;
            sequence = text.trim().parse::<i64>().ok();
            continue;
        }
        if name == "TIMESTAMP" {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .with_context(|| format!("{} has an unreadable timestamp", clean_path(archive)))?;
            dump_date = parse_dump_date(&text);
            continue;
        }
        if is_data_table(name) {
            break;
        }
    }

    Ok(SchemaScan {
        schema: None,
        sequence,
        dump_date,
    })
}

fn is_data_table(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() && !name.ends_with(".sql"))
}

/// Visits every table entry of the archive in one streaming pass.
///
/// The visitor receives the entry's file name (`release`, `medium`, …) and a
/// buffered reader over its COPY rows. Returns the MD5 of the compressed bytes
/// that were read, so a published checksum can be verified in the same pass
/// instead of re-reading seventeen gigabytes.
pub fn for_each_table(
    archive: &Path,
    mut visit: impl FnMut(&str, &mut dyn BufRead) -> Result<()>,
) -> Result<String> {
    let file =
        File::open(archive).with_context(|| format!("cannot read {}", clean_path(archive)))?;
    let hashing = HashingReader::new(BufReader::new(file));
    let decoder = bzip2::read::BzDecoder::new(hashing);
    let mut tar = tar::Archive::new(decoder);

    for entry in tar
        .entries()
        .with_context(|| format!("{} is not a tar archive", clean_path(archive)))?
    {
        let entry = entry.with_context(|| format!("{} is truncated", clean_path(archive)))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .with_context(|| format!("{} has an unreadable entry", clean_path(archive)))?
            .to_path_buf();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        let mut reader = BufReader::new(entry);
        visit(&name, &mut reader)?;
    }

    let decoder = tar.into_inner();
    Ok(decoder.into_inner().finish())
}

/// Feeds every COPY row of one table to `visit`.
pub fn for_each_row(
    reader: &mut dyn BufRead,
    mut visit: impl FnMut(&CopyFields) -> Result<()>,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .context("a dump table is not valid UTF-8")?;
        if read == 0 {
            return Ok(());
        }
        let trimmed = line.strip_suffix('\n').unwrap_or(&line);
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        if trimmed.is_empty() {
            continue;
        }
        let fields = parse_copy_fields(trimmed)?;
        visit(&fields)?;
    }
}

/// MD5s the bytes as they are read through it.
struct HashingReader<R> {
    inner: R,
    hasher: Md5,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Md5::new(),
        }
    }

    fn finish(self) -> String {
        self.hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.hasher.update(&buffer[..read]);
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScratchDir, write_mb_archive};

    #[test]
    fn nulls_empty_values_and_trailing_fields_stay_distinct() {
        let fields = parse_copy_fields("1\t\\N\t\tlast\t").unwrap();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields.text(0), Some("1"));
        assert_eq!(fields.text(1), None);
        assert_eq!(fields.text(2), Some(""));
        assert_eq!(fields.text(3), Some("last"));
        assert_eq!(fields.text(4), Some(""));
    }

    #[test]
    fn a_dump_without_ddl_reports_its_schema_sequence() {
        let dir = ScratchDir::new("copy-sequence");
        let archive = write_mb_archive(
            dir.path(),
            &[("SCHEMA_SEQUENCE", "31\n"), ("release", "1\tx\n")],
        );
        let scan = read_schema(&archive).unwrap();
        assert!(scan.schema.is_none());
        assert_eq!(scan.sequence, Some(31));
    }

    #[test]
    fn the_export_date_comes_from_the_timestamp_entry() {
        let dir = ScratchDir::new("copy-timestamp");
        let archive = write_mb_archive(
            dir.path(),
            &[
                ("TIMESTAMP", "2026-11-11 00:23:36.560115+00\n"),
                ("SCHEMA_SEQUENCE", "31\n"),
                ("release", "1\tx\n"),
            ],
        );
        let scan = read_schema(&archive).unwrap();
        assert_eq!(scan.dump_date.as_deref(), Some("2026-11-11"));
    }

    #[test]
    fn a_missing_or_unreadable_timestamp_leaves_the_date_unknown() {
        let dir = ScratchDir::new("copy-no-timestamp");
        let archive = write_mb_archive(dir.path(), &[("release", "1\tx\n")]);
        assert_eq!(read_schema(&archive).unwrap().dump_date, None);
        for text in [
            "",
            "soon",
            "2026-13-01 00:00:00+00",
            "2026-1-11",
            "20261111",
        ] {
            assert_eq!(parse_dump_date(text), None, "{text:?}");
        }
    }

    #[test]
    fn copy_escapes_decode_without_splitting_fields() {
        let fields = parse_copy_fields("a\\tb\tc\\nd\te\\\\N\tf\\101").unwrap();
        assert_eq!(fields.text(0), Some("a\tb"));
        assert_eq!(fields.text(1), Some("c\nd"));
        assert_eq!(fields.text(2), Some("e\\N"));
        assert_eq!(fields.text(3), Some("fA"));
        let fields = parse_copy_fields("Sigur R\\303\\263s").unwrap();
        assert_eq!(fields.text(0), Some("Sigur Rós"));
    }

    #[test]
    fn malformed_escapes_fail_without_echoing_the_value() {
        for raw in ["a\\qb", "a\\12", "a\\"] {
            let error = parse_copy_fields(raw).unwrap_err().to_string();
            assert!(error.contains("COPY field 1"), "{error}");
            assert!(!error.contains("a\\qb"), "{error}");
        }
    }

    #[test]
    fn schema_columns_resolve_by_name() {
        let schema = DumpSchema::parse(
            "CREATE TABLE release (\n    id SERIAL,\n    gid UUID NOT NULL,\n    name VARCHAR NOT NULL,\n    barcode VARCHAR,\n    PRIMARY KEY (id)\n);\n\
             CREATE TABLE public.track (\n    id SERIAL,\n\"gid\" UUID\n);\n",
        );
        assert_eq!(schema.index("release", "gid").unwrap(), 1);
        assert_eq!(schema.index("release", "barcode").unwrap(), 3);
        assert_eq!(schema.columns("release").unwrap().len(), 4);
        assert_eq!(schema.index("track", "gid").unwrap(), 1);
        assert!(schema.index("release", "missing").is_err());
        assert!(schema.index("nope", "id").is_err());
    }

    #[test]
    fn numeric_and_boolean_columns_are_validated() {
        let fields = parse_copy_fields("42\t\\N\t7\tt").unwrap();
        assert_eq!(fields.int("release", "id", 0).unwrap(), Some(42));
        assert_eq!(fields.int("release", "status", 1).unwrap(), None);
        assert_eq!(fields.int("release", "quality", 2).unwrap(), Some(7));
        assert!(fields.flag("track", "is_data_track", 3).unwrap());
        assert!(fields.flag("track", "is_data_track", 1).is_ok());

        let bad = parse_copy_fields("not-a-number").unwrap();
        let error = bad.int("release", "id", 0).unwrap_err().to_string();
        assert!(error.contains("non-numeric id"), "{error}");
        assert!(!error.contains("not-a-number"), "{error}");
    }

    #[test]
    fn a_streamed_archive_exposes_named_tables_and_their_digest() {
        let dir = ScratchDir::new("copy-archive");
        let archive = write_mb_archive(
            dir.path(),
            &[
                (
                    "CreateTables.sql",
                    "CREATE TABLE release (\n id SERIAL,\n gid UUID\n);",
                ),
                ("release", "1\t76df3287-6cda-33eb-8e9a-044b5e15ffdd\n"),
            ],
        );

        let scan = read_schema(&archive).unwrap();
        let schema = scan.schema.expect("a schema");
        assert_eq!(schema.index("release", "gid").unwrap(), 1);

        let mut seen = Vec::new();
        let mut release_rows = 0;
        let digest = for_each_table(&archive, |name, reader| {
            seen.push(name.to_string());
            if name == "release" {
                for_each_row(reader, |fields| {
                    assert_eq!(fields.len(), 2);
                    release_rows += 1;
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec!["CreateTables.sql", "release"]);
        assert_eq!(release_rows, 1);
        assert_eq!(digest.len(), 32);
        assert!(digest.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn a_truncated_archive_is_an_error_rather_than_a_short_table() {
        let dir = ScratchDir::new("copy-truncated");
        let archive = write_mb_archive(dir.path(), &[("release", "1\tx\n2\ty\n")]);
        let bytes = std::fs::read(&archive).unwrap();
        std::fs::write(&archive, &bytes[..bytes.len() / 2]).unwrap();

        let error = for_each_table(&archive, |_, _| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("mbdump.tar.bz2"), "{error}");
    }
}
