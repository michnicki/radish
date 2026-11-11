use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use radish::{db, runner, scanner, store, watcher};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "radish",
    version,
    about = "A blazing-fast music library manager"
)]
struct Cli {
    /// Path to the SQLite library database
    #[arg(long, global = true, default_value = "radish.db")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Multi-threaded scan and import of a music directory
    Import {
        path: PathBuf,
        /// Match the imported directory against Discogs after importing
        #[arg(long = "match")]
        match_releases: bool,
    },
    /// Watch a music directory and keep the library in sync in real time
    Watch { path: PathBuf },
    /// List every track in the library
    List,
    /// Search the library by title, artist, or album
    Search { query: String },
    /// List local releases and their match status
    Releases {
        /// Restrict output to this directory tree
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Match local releases against Discogs releases
    Match {
        /// Restrict matching to this directory tree
        path: Option<PathBuf>,
        /// Only process this local release
        #[arg(long = "local-release", value_name = "ID", conflicts_with = "path")]
        local_release: Option<i64>,
        /// Fetch this Discogs release directly, bypassing search
        #[arg(
            long = "discogs-release",
            value_name = "ID",
            requires = "local_release"
        )]
        discogs_release: Option<u64>,
        /// Review candidates interactively instead of accepting automatically
        #[arg(
            long,
            conflicts_with_all = ["clear", "local_release", "discogs_release"]
        )]
        review: bool,
        /// Search and show decisions without storing matches
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Remove the accepted match for a local release
        #[arg(
            long,
            value_name = "ID",
            conflicts_with_all = ["path", "local_release", "discogs_release", "review", "dry_run"]
        )]
        clear: Option<i64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let pool = db::connect(&cli.db).await?;

    match cli.command {
        Command::Import {
            path,
            match_releases,
        } => {
            let canonical = run_import(&pool, &path).await?;
            if match_releases {
                let summary = runner::run_match(
                    &pool,
                    runner::MatchOptions {
                        scope: Some(canonical),
                        ..runner::MatchOptions::default()
                    },
                )
                .await?;
                summary.print_report();
                if summary.has_failures() {
                    bail!(
                        "Discogs matching finished with {} failure(s); the import was kept",
                        summary.failed
                    );
                }
            }
        }
        Command::Watch { path } => watcher::watch_directory(pool, &path).await?,
        Command::List => print_tracks(&store::list_tracks(&pool).await?),
        Command::Search { query } => print_tracks(&store::search_tracks(&pool, &query).await?),
        Command::Releases { path } => {
            let scope = canonical_scope(path.as_deref())?;
            print_releases(&store::list_local_releases(&pool, scope.as_deref()).await?);
        }
        Command::Match {
            path,
            local_release,
            discogs_release,
            review,
            dry_run,
            clear,
        } => {
            if let Some(id) = clear {
                if store::clear_match(&pool, id).await? {
                    println!("Cleared the accepted match for local release {id}.");
                } else {
                    println!("Local release {id} has no accepted match.");
                }
            } else {
                let scope = canonical_scope(path.as_deref())?;
                let summary = runner::run_match(
                    &pool,
                    runner::MatchOptions {
                        scope,
                        local_release,
                        discogs_release,
                        review,
                        dry_run,
                    },
                )
                .await?;
                summary.print_report();
                if summary.has_failures() {
                    bail!(
                        "Discogs matching finished with {} failure(s); completed matches were kept",
                        summary.failed
                    );
                }
            }
        }
    }

    Ok(())
}

fn canonical_scope(path: Option<&Path>) -> Result<Option<PathBuf>> {
    match path {
        Some(path) => {
            Ok(Some(path.canonicalize().with_context(|| {
                format!("cannot resolve {}", path.display())
            })?))
        }
        None => Ok(None),
    }
}

fn print_releases(rows: &[store::LocalReleaseStatus]) {
    if rows.is_empty() {
        println!("No local releases found.");
        return;
    }

    for row in rows {
        let artist = row.album_artist.as_deref().unwrap_or("(unknown artist)");
        let title = row.album_title.as_deref().unwrap_or("(no album tag)");
        let mut status = row.status.clone();
        if row.queued && row.status != store::STATUS_MATCHED {
            status.push_str(", queued");
        }
        let discogs = match row.discogs_release_id {
            Some(id) => format!(
                " -> discogs {id}{}",
                row.discogs_title
                    .as_deref()
                    .map(|title| format!(" ({title})"))
                    .unwrap_or_default()
            ),
            None => String::new(),
        };
        println!(
            "#{} {} — {}  [{} track(s)]  {status}{discogs}\n     {}",
            row.id, artist, title, row.track_count, row.directory
        );
    }
    println!("\n{} local release(s).", rows.len());
}

fn print_tracks(rows: &[store::TrackRow]) {
    if rows.is_empty() {
        println!("No tracks found.");
        return;
    }

    for row in rows {
        let filename = std::path::Path::new(&row.path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| row.path.clone());
        let title = row.title.as_deref().unwrap_or(&filename);
        let artist = row.artist.as_deref().unwrap_or("(unknown artist)");
        let duration = format_duration(row.duration_secs);
        let stale = if row.stale { "  [stale match]" } else { "" };
        match (&row.album, row.track_number) {
            (Some(album), Some(n)) => {
                println!("{artist} - {title}  [{album} #{n}]  {duration}{stale}")
            }
            (Some(album), None) => println!("{artist} - {title}  [{album}]  {duration}{stale}"),
            (None, _) => println!("{artist} - {title}  {duration}{stale}"),
        }
    }
    println!("\n{} track(s).", rows.len());
}

fn format_duration(secs: Option<f64>) -> String {
    match secs {
        Some(secs) => {
            let total = secs.round() as u64;
            format!("{}:{:02}", total / 60, total % 60)
        }
        None => "?:??".to_string(),
    }
}

async fn run_import(pool: &sqlx::SqlitePool, path: &std::path::Path) -> Result<PathBuf> {
    let path = path.canonicalize()?;
    println!("Scanning {}...", path.display());

    let directory_id = store::upsert_directory(pool, &path).await?;
    let tracks = scanner::scan_directory(&path).await?;
    let count = tracks.len();

    for track in &tracks {
        store::upsert_track(pool, track, directory_id).await?;
    }

    let groups = store::rebuild_local_releases(pool, Some(&path)).await?;

    println!("Imported {count} tracks into {groups} local release(s).");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("radish").chain(args.iter().copied()))
    }

    #[test]
    fn import_match_flag_parses() {
        let cli = parse(&["import", "/music", "--match"]).unwrap();
        match cli.command {
            Command::Import { match_releases, .. } => assert!(match_releases),
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn discogs_release_requires_local_release() {
        assert!(parse(&["match", "--discogs-release", "42"]).is_err());
        assert!(parse(&["match", "--local-release", "3", "--discogs-release", "42"]).is_ok());
    }

    #[test]
    fn clear_conflicts_with_matching_options() {
        assert!(parse(&["match", "--clear", "3", "--review"]).is_err());
        assert!(parse(&["match", "--clear", "3", "/music"]).is_err());
        assert!(parse(&["match", "--clear", "3"]).is_ok());
    }

    #[test]
    fn local_release_conflicts_with_a_path() {
        assert!(parse(&["match", "--local-release", "3", "/music"]).is_err());
    }

    #[test]
    fn releases_accepts_an_optional_path() {
        assert!(parse(&["releases"]).is_ok());
        assert!(parse(&["releases", "--path", "/music"]).is_ok());
    }

    #[test]
    fn matching_options_parse_together() {
        let cli = parse(&["match", "/music", "--dry-run", "--review"]).unwrap();
        match cli.command {
            Command::Match {
                dry_run, review, ..
            } => {
                assert!(dry_run);
                assert!(review);
            }
            _ => panic!("expected match"),
        }
    }
}
