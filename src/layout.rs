//! Plain-text layout helpers for listings: pluralization, track fields and aligned tables.
//!
//! Everything here is pure and returns strings, so callers decide how to write
//! them (always through [`outln!`](crate::outln)). [`Table`] escapes every cell
//! with [`Clean`] before measuring it, so untrusted tag values can neither
//! inject terminal control sequences nor break column alignment. Widths are
//! terminal display columns, not `char` counts, so CJK and accented text line
//! up.

// Rust guideline compliant 2026-08-14

use crate::output::Clean;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Text between two columns.
const COLUMN_GAP: &str = "  ";

/// Marks the point where truncated text was cut.
const ELLIPSIS: char = '…';

/// Display columns taken by [`ELLIPSIS`]; terminals draw it one column wide.
const ELLIPSIS_WIDTH: usize = 1;

/// Formats `count` with the matching noun form, such as `1 track` or `2 tracks`.
///
/// # Examples
///
/// ```
/// use radish::layout::plural;
///
/// assert_eq!(plural(1, "track", "tracks"), "1 track");
/// assert_eq!(plural(0, "track", "tracks"), "0 tracks");
/// ```
pub fn plural(count: u64, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

/// Formats a duration as `minutes:seconds`, or `?:??` when it is unknown.
///
/// # Examples
///
/// ```
/// use radish::layout::format_duration;
///
/// assert_eq!(format_duration(Some(225.0)), "3:45");
/// assert_eq!(format_duration(None), "?:??");
/// ```
pub fn format_duration(secs: Option<f64>) -> String {
    match secs {
        Some(secs) => {
            let total = secs.round() as u64;
            format!("{}:{:02}", total / 60, total % 60)
        }
        None => "?:??".to_string(),
    }
}

/// Formats a track position as `track`, or `disc-track` above disc 1.
///
/// A missing track number shows as `-`.
///
/// # Examples
///
/// ```
/// use radish::layout::track_number;
///
/// assert_eq!(track_number(Some(1), Some(3)), "3");
/// assert_eq!(track_number(Some(2), Some(1)), "2-1");
/// assert_eq!(track_number(None, None), "-");
/// ```
pub fn track_number(disc: Option<i64>, track: Option<i64>) -> String {
    match (disc, track) {
        (Some(disc), Some(track)) if disc > 1 => format!("{disc}-{track}"),
        (_, Some(track)) => track.to_string(),
        _ => "-".to_string(),
    }
}

/// Returns how many terminal columns `text` occupies.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Shortens `text` to at most `max` display columns, ending in `…` when cut.
///
/// Text that already fits is returned unchanged. A `max` of zero yields an
/// empty string.
pub fn truncate_display(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    let Some(budget) = max.checked_sub(ELLIPSIS_WIDTH) else {
        return String::new();
    };
    let mut shortened = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + width > budget {
            break;
        }
        shortened.push(c);
        used += width;
    }
    shortened.push(ELLIPSIS);
    shortened
}

/// Horizontal alignment of a column's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Pad on the right.
    Left,
    /// Pad on the left, which lines up numbers.
    Right,
}

/// Header, alignment and optional width cap for one table column.
#[derive(Debug, Clone)]
pub struct Column {
    header: &'static str,
    align: Align,
    max_width: Option<usize>,
}

impl Column {
    /// Creates a left-aligned column with no width cap.
    pub fn new(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Left,
            max_width: None,
        }
    }

    /// Right-aligns the column.
    #[must_use]
    pub fn right(mut self) -> Self {
        self.align = Align::Right;
        self
    }

    /// Caps the column at `width` display columns when the table is truncated.
    #[must_use]
    pub fn capped(mut self, width: usize) -> Self {
        self.max_width = Some(width);
        self
    }
}

/// Rows of text rendered as aligned columns under a header line.
///
/// Each column is as wide as its widest cell, and the last column is never
/// padded, so lines carry no trailing whitespace. An empty header on the last
/// column makes it a free-form note.
///
/// # Examples
///
/// ```
/// use radish::layout::{Column, Table};
///
/// let mut table = Table::new(vec![Column::new("NAME"), Column::new("N").right()]);
/// table.push(["Blue", "3"]);
/// table.push(["Geogaddi", "12"]);
/// assert_eq!(
///     table.render(false),
///     ["NAME       N", "Blue       3", "Geogaddi  12"]
/// );
/// ```
#[derive(Debug, Clone)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// Creates an empty table with the given columns.
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Appends a row, escaping each cell with [`Clean`].
    ///
    /// # Panics
    ///
    /// Panics when the row does not have one cell per column.
    pub fn push<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let row: Vec<String> = cells
            .into_iter()
            .map(|cell| Clean(cell.as_ref()).to_string())
            .collect();
        assert_eq!(
            row.len(),
            self.columns.len(),
            "a table row needs one cell per column"
        );
        self.rows.push(row);
    }

    /// Renders the header line followed by one line per row.
    ///
    /// With `truncate` set, cells in capped columns are shortened with
    /// [`truncate_display`]. Pass `false` for piped output, which must stay
    /// complete.
    pub fn render(&self, truncate: bool) -> Vec<String> {
        let shown: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&self.columns)
                    .map(|(cell, column)| match column.max_width {
                        Some(max) if truncate => truncate_display(cell, max),
                        _ => cell.clone(),
                    })
                    .collect()
            })
            .collect();

        let widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                shown
                    .iter()
                    .map(|row| display_width(&row[index]))
                    .fold(display_width(column.header), usize::max)
            })
            .collect();

        let header: Vec<&str> = self.columns.iter().map(|column| column.header).collect();
        let mut lines = vec![self.line(&header, &widths)];
        lines.extend(shown.iter().map(|row| self.line(row, &widths)));
        lines
    }

    fn line<S: AsRef<str>>(&self, cells: &[S], widths: &[usize]) -> String {
        let mut line = String::new();
        for (index, cell) in cells.iter().enumerate() {
            let cell = cell.as_ref();
            if index > 0 {
                line.push_str(COLUMN_GAP);
            }
            let padding = " ".repeat(widths[index].saturating_sub(display_width(cell)));
            match self.columns[index].align {
                Align::Left => {
                    line.push_str(cell);
                    line.push_str(&padding);
                }
                Align::Right => {
                    line.push_str(&padding);
                    line.push_str(cell);
                }
            }
        }
        line.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        let mut table = Table::new(vec![
            Column::new("ARTIST").capped(8),
            Column::new("#").right(),
            Column::new(""),
        ]);
        table.push(["Test", "3", ""]);
        table.push(["Some Band Name", "2-1", "stale match"]);
        table
    }

    #[test]
    fn plural_picks_the_noun_form() {
        assert_eq!(plural(0, "track", "tracks"), "0 tracks");
        assert_eq!(plural(1, "track", "tracks"), "1 track");
        assert_eq!(plural(2, "track", "tracks"), "2 tracks");
        assert_eq!(
            plural(1, "local release", "local releases"),
            "1 local release"
        );
    }

    #[test]
    fn columns_align_and_numbers_right_align() {
        let lines = sample().render(false);
        assert_eq!(
            lines,
            [
                "ARTIST            #",
                "Test              3",
                "Some Band Name  2-1  stale match",
            ]
        );
    }

    #[test]
    fn lines_have_no_trailing_whitespace() {
        for line in sample().render(false) {
            assert_eq!(line, line.trim_end(), "{line:?}");
        }
    }

    #[test]
    fn truncation_only_applies_when_asked_and_ends_in_an_ellipsis() {
        let full = sample().render(false);
        assert!(full[2].starts_with("Some Band Name"), "{full:?}");

        let cut = sample().render(true);
        assert!(cut[2].starts_with("Some Ba…"), "{cut:?}");
        // The cap applies to the cell, and the column shrinks to match.
        assert_eq!(cut[1], "Test        3");
    }

    #[test]
    fn wide_and_accented_text_aligns_by_display_width() {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")]);
        table.push(["日本語", "x"]);
        table.push(["Sigur Rós", "y"]);
        let lines = table.render(false);
        let columns: Vec<usize> = lines
            .iter()
            .map(|line| display_width(line.rsplit_once("  ").unwrap().0))
            .collect();
        assert_eq!(columns, [display_width("Sigur Rós"); 3], "{lines:?}");
    }

    #[test]
    fn control_characters_are_escaped_before_measuring() {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")]);
        table.push(["a\x1b[31mb", "x"]);
        let lines = table.render(false);
        assert!(
            !lines.iter().any(|l| l.chars().any(char::is_control)),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("a\\u{1b}[31mb"), "{lines:?}");
    }

    #[test]
    fn truncate_display_respects_width_and_degenerate_caps() {
        assert_eq!(truncate_display("short", 10), "short");
        assert_eq!(truncate_display("abcdef", 4), "abc…");
        assert_eq!(truncate_display("日本語", 4), "日…");
        assert_eq!(truncate_display("abcdef", 1), "…");
        assert_eq!(truncate_display("abcdef", 0), "");
    }

    #[test]
    #[should_panic(expected = "one cell per column")]
    fn a_short_row_is_a_programming_error() {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")]);
        table.push(["only one"]);
    }
}
