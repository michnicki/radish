//! `radish index build` and `radish index status`.
//!
//! Building is minutes to an hour of disk work, so progress is printed in the
//! same plain lines as the rest of the CLI. Every path that reaches the user is
//! escaped, and no dump-derived text is printed at all.

use crate::index::{
    self, BuildProgress, BuildReport, INDEX_SCHEMA_VERSION, IndexKind, IndexPaths, IndexState,
};
use crate::outln;
use crate::output::{Clean, clean_path};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Builds whichever sides were given. Each side is published independently, so
/// one failure never damages the other index.
pub async fn build(
    musicbrainz: Option<PathBuf>,
    discogs: Option<PathBuf>,
    index_dir: &Path,
) -> Result<()> {
    if musicbrainz.is_none() && discogs.is_none() {
        bail!(
            "give at least one dump: `radish index build --musicbrainz PATH`, `--discogs PATH`, or both"
        );
    }

    let paths = IndexPaths::from_dir(index_dir);
    let progress = ConsoleProgress::new();

    if let Some(input) = musicbrainz {
        outln!("Building the MusicBrainz index from {}", clean_path(&input));
        let report = index::musicbrainz::build(&input, &paths, &progress).await?;
        report_side(IndexKind::MusicBrainz, &paths, report).await?;
    }
    if let Some(input) = discogs {
        outln!("Building the Discogs index from {}", clean_path(&input));
        let report = index::discogs::build(&input, &paths, &progress).await?;
        report_side(IndexKind::Discogs, &paths, report).await?;
    }

    Ok(())
}

pub async fn status(index_dir: &Path) -> Result<()> {
    let paths = IndexPaths::from_dir(index_dir);
    outln!("Index directory: {}", clean_path(paths.dir()));

    for kind in IndexKind::ALL {
        let state = index::inspect(&paths, kind)
            .await
            .with_context(|| format!("cannot read the {kind} index"))?;
        match state {
            IndexState::Missing => {
                outln!("{kind}: not built ({})", clean_path(&paths.file(kind)));
            }
            IndexState::Present(meta) => {
                let mut notes = Vec::new();
                if meta.schema_version != INDEX_SCHEMA_VERSION {
                    notes.push(format!(
                        "rebuild needed: schema {} (this Radish reads {})",
                        meta.schema_version, INDEX_SCHEMA_VERSION
                    ));
                }
                if let Some(date) = &meta.dump_date {
                    notes.push(format!("dump date {date}"));
                }
                if let Some(checksum) = &meta.checksum {
                    notes.push(format!("checksum {checksum}"));
                }
                if meta.skipped_rows > 0 {
                    notes.push(format!("{} row(s) skipped", meta.skipped_rows));
                }
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", notes.join(", "))
                };
                outln!(
                    "{kind}: {} release(s), {}{notes}",
                    meta.indexed_releases,
                    human_size(file_size(&paths.file(kind)))
                );
            }
        }
    }

    Ok(())
}

async fn report_side(kind: IndexKind, paths: &IndexPaths, report: BuildReport) -> Result<()> {
    let meta = index::inspect(paths, kind)
        .await?
        .meta()
        .cloned()
        .unwrap_or_default();

    outln!(
        "  {} release(s) indexed, {} skipped in {:.1}s",
        report.indexed,
        report.skipped,
        report.elapsed.as_secs_f64()
    );
    outln!(
        "  {} ({})",
        clean_path(&paths.file(kind)),
        human_size(file_size(&paths.file(kind)))
    );
    if let Some(date) = &meta.dump_date {
        outln!("  dump date: {date}");
    }
    outln!(
        "  checksum: {}",
        meta.checksum.as_deref().unwrap_or("not recorded")
    );
    if report.skipped > 0 {
        outln!("  some rows could not be converted and were left out");
    }

    Ok(())
}

/// Prints each progress milestone with elapsed time, so an hour-long build
/// shows that it is still moving and how far it has come.
struct ConsoleProgress {
    started: Instant,
}

impl ConsoleProgress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    fn elapsed(&self) -> String {
        let seconds = self.started.elapsed().as_secs();
        if seconds < 60 {
            format!("{seconds}s")
        } else {
            format!("{}m{:02}s", seconds / 60, seconds % 60)
        }
    }
}

impl BuildProgress for ConsoleProgress {
    fn staging(&self, table: &str, rows: u64) {
        outln!(
            "  staged {rows} {} row(s) — {}",
            Clean(table),
            self.elapsed()
        );
    }

    fn indexing(&self, releases: u64) {
        outln!("  indexed {releases} release(s) — {}", self.elapsed());
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
