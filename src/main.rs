use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use radish::output::{Clean, clean_path};
use radish::progress::ImportProgress;
use radish::providers::{ProviderKind, ReleaseId};
use radish::{db, importer, outln, runner, store, watcher};
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
        /// Match the imported directory after importing (MusicBrainz first,
        /// then Discogs when a token is available)
        #[arg(long = "match")]
        match_releases: bool,
        /// Restrict matching to one service and disable fallback
        #[arg(long, value_name = "PROVIDER", requires = "match_releases")]
        provider: Option<ProviderKind>,
        /// Automatically resolve strongly supported album matches with Clef
        #[arg(
            long = "ai-match",
            visible_alias = "ai-review",
            requires = "match_releases"
        )]
        ai_review: bool,
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
    /// Match local releases against MusicBrainz, with optional Discogs fallback
    Match {
        /// Restrict matching to this directory tree
        path: Option<PathBuf>,
        /// Only process this local release
        #[arg(long = "local-release", value_name = "ID", conflicts_with = "path")]
        local_release: Option<i64>,
        /// Restrict matching to one service and disable fallback
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<ProviderKind>,
        /// Fetch this MusicBrainz release directly, bypassing search
        #[arg(
            long = "musicbrainz-release",
            value_name = "MBID",
            requires = "local_release",
            conflicts_with = "discogs_release"
        )]
        musicbrainz_release: Option<String>,
        /// Fetch this Discogs release directly, bypassing search
        #[arg(
            long = "discogs-release",
            value_name = "ID",
            requires = "local_release",
            conflicts_with = "musicbrainz_release"
        )]
        discogs_release: Option<u64>,
        /// Review candidates interactively instead of accepting automatically
        #[arg(long, conflicts_with = "clear")]
        review: bool,
        /// Search and show decisions without storing matches
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Automatically resolve strongly supported album matches with Clef
        #[arg(long = "ai-match", visible_alias = "ai-review")]
        ai_review: bool,
        /// Remove the accepted match for a local release
        #[arg(
            long,
            value_name = "ID",
            conflicts_with_all = [
                "path",
                "local_release",
                "provider",
                "musicbrainz_release",
                "discogs_release",
                "review",
                "dry_run",
                "ai_review"
            ]
        )]
        clear: Option<i64>,
    },
}

fn parse_direct(musicbrainz: Option<&str>, discogs: Option<u64>) -> Result<Option<ReleaseId>> {
    match (musicbrainz, discogs) {
        (Some(raw), None) => {
            let id = raw
                .parse()
                .with_context(|| format!("'{raw}' is not a hyphenated MusicBrainz MBID"))?;
            Ok(Some(ReleaseId::MusicBrainz(id)))
        }
        (None, Some(id)) => Ok(Some(ReleaseId::Discogs(id))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => bail!("choose either a MusicBrainz or a Discogs release selector"),
    }
}

/// The only keys read from `.env`. Endpoint overrides are deliberately excluded
/// so a stray `.env` can never redirect real credentials to another host.
const DOTENV_KEYS: [&str; 4] = [
    "RADISH_CLOUDFLARE_ACCOUNT_ID",
    "RADISH_CLOUDFLARE_API_TOKEN",
    "RADISH_CLOUDFLARE_MODEL",
    "RADISH_DISCOGS_TOKEN",
];

/// Loads allowed keys from `./.env` in the current directory only, never from a
/// parent. Existing environment variables win; a missing or malformed file is
/// ignored quietly.
fn load_dotenv() {
    if let Ok(entries) = dotenvy::from_path_iter(".env") {
        for (key, value) in entries.flatten() {
            if DOTENV_KEYS.contains(&key.as_str()) && std::env::var_os(&key).is_none() {
                // SAFETY: still single-threaded; Tokio has not started.
                unsafe { std::env::set_var(key, value) }
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    // Set process configuration before Tokio starts any worker threads.
    load_dotenv();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run()));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error);
            std::process::ExitCode::FAILURE
        }
    }
}

/// Prints an error and its causes like `anyhow` does, but with every line
/// escaped: error contexts can carry file names, which are untrusted.
fn report_error(error: &anyhow::Error) {
    eprintln!("Error: {}", Clean(&error.to_string()));
    let causes: Vec<String> = error.chain().skip(1).map(ToString::to_string).collect();
    if !causes.is_empty() {
        eprintln!("\nCaused by:");
        for (index, cause) in causes.iter().enumerate() {
            if causes.len() == 1 {
                eprintln!("    {}", Clean(cause));
            } else {
                eprintln!("    {index}: {}", Clean(cause));
            }
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // CLI validation happens before the database is opened or any client is
    // constructed, so invalid combinations never touch storage or the network.
    let selection: Option<(Option<ProviderKind>, Option<ReleaseId>)> = match &cli.command {
        Command::Match {
            provider,
            musicbrainz_release,
            discogs_release,
            ..
        } => Some((
            *provider,
            parse_direct(musicbrainz_release.as_deref(), *discogs_release)?,
        )),
        Command::Import { provider, .. } => Some((*provider, None)),
        _ => None,
    };
    let policy = match &selection {
        Some((provider, direct)) => Some(runner::resolve_policy(*provider, direct.as_ref())?),
        None => None,
    };
    let direct = selection.and_then(|(_, direct)| direct);

    let pool = db::connect(&cli.db).await?;

    match cli.command {
        Command::Import {
            path,
            match_releases,
            provider: _,
            ai_review,
        } => {
            let canonical = run_import(&pool, &path).await?;
            if match_releases {
                let summary = runner::run_match(
                    &pool,
                    runner::MatchOptions {
                        scope: Some(canonical),
                        policy: policy.unwrap_or_default(),
                        ai_review,
                        ..runner::MatchOptions::default()
                    },
                )
                .await?;
                summary.print_report();
                if summary.has_failures() {
                    bail!(
                        "matching finished with {} failure(s); the import was kept",
                        summary.failed
                    );
                }
            }
        }
        Command::Watch { path } => watcher::watch_directory(pool, &path, &cli.db).await?,
        Command::List => print_tracks(&store::list_tracks(&pool).await?),
        Command::Search { query } => print_tracks(&store::search_tracks(&pool, &query).await?),
        Command::Releases { path } => {
            let scope = canonical_scope(path.as_deref())?;
            print_releases(&store::list_local_releases(&pool, scope.as_deref()).await?);
        }
        Command::Match {
            path,
            local_release,
            provider: _,
            musicbrainz_release: _,
            discogs_release: _,
            review,
            dry_run,
            ai_review,
            clear,
        } => {
            if let Some(id) = clear {
                if store::clear_match(&pool, id).await? {
                    outln!("Cleared the accepted match for local release {id}.");
                } else {
                    outln!("Local release {id} has no accepted match.");
                }
            } else {
                let scope = canonical_scope(path.as_deref())?;
                let summary = runner::run_match(
                    &pool,
                    runner::MatchOptions {
                        scope,
                        local_release,
                        direct_release: direct,
                        review,
                        dry_run,
                        ai_review,
                        policy: policy.unwrap_or_default(),
                    },
                )
                .await?;
                summary.print_report();
                if summary.has_failures() {
                    bail!(
                        "matching finished with {} failure(s); completed matches were kept",
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
                format!("cannot resolve {}", clean_path(path))
            })?))
        }
        None => Ok(None),
    }
}

fn print_releases(rows: &[store::LocalReleaseStatus]) {
    if rows.is_empty() {
        outln!("No local releases found.");
        return;
    }

    for row in rows {
        let artist = row.album_artist.as_deref().unwrap_or("(unknown artist)");
        let title = row.album_title.as_deref().unwrap_or("(no album tag)");
        let mut status = row.status.clone();
        if row.match_scope.as_deref() == Some("album") {
            status.push_str(", album match; edition unconfirmed");
        }
        if row.stale && row.status != store::STATUS_STALE {
            status.push_str(", stale");
        }
        if row.queued() && row.status != store::STATUS_MATCHED {
            let providers: Vec<&str> = row
                .queued_providers
                .iter()
                .map(|provider| provider.as_str())
                .collect();
            status.push_str(&format!(", queued ({})", providers.join(", ")));
        }
        let remote = match (&row.provider, &row.remote_release_id) {
            (Some(provider), Some(id)) => format!(
                " -> {provider} {}{}",
                Clean(id),
                row.remote_title
                    .as_deref()
                    .map(|title| format!(" ({})", Clean(title)))
                    .unwrap_or_default()
            ),
            _ => String::new(),
        };
        outln!(
            "#{} {} — {}  [{} track(s)]  {status}{remote}\n     {}",
            row.id,
            Clean(artist),
            Clean(title),
            row.track_count,
            Clean(&row.directory)
        );
    }
    outln!("\n{} local release(s).", rows.len());
}

fn print_tracks(rows: &[store::TrackRow]) {
    if rows.is_empty() {
        outln!("No tracks found.");
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
        let location = match (&row.album, row.disc_number, row.track_number) {
            (Some(album), Some(disc), Some(number)) if disc > 1 => {
                format!("  [{} disc {disc} #{number}]", Clean(album))
            }
            (Some(album), _, Some(number)) => format!("  [{} #{number}]", Clean(album)),
            (Some(album), _, None) => format!("  [{}]", Clean(album)),
            _ => String::new(),
        };
        outln!(
            "{} - {}{location}  {duration}{stale}",
            Clean(artist),
            Clean(title)
        );
    }
    outln!("\n{} track(s).", rows.len());
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
    outln!("Scanning {}...", clean_path(&path));

    let progress = ImportProgress::new();
    let summary = importer::sync_directory(pool, &path, &progress).await?;
    drop(progress);

    let kept = match summary.kept {
        0 => String::new(),
        kept => format!(", {kept} kept (unreadable)"),
    };
    outln!(
        "Imported {} new or changed tracks ({} unchanged, {} removed{kept}) into {} local release(s).",
        summary.saved,
        summary.unchanged,
        summary.removed,
        summary.rebuild.groups
    );
    importer::report_rebuild(&summary.rebuild);
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
    fn import_provider_requires_match() {
        assert!(parse(&["import", "/music", "--provider", "discogs"]).is_err());
        assert!(parse(&["import", "/music", "--match", "--provider", "discogs"]).is_ok());
    }

    #[test]
    fn ai_review_is_explicit_and_requires_matching() {
        assert!(parse(&["match", "--ai-match"]).is_ok());
        assert!(parse(&["match", "--ai-review"]).is_ok());
        assert!(parse(&["match", "--clear", "3", "--ai-match"]).is_err());
        assert!(parse(&["match", "--clear", "3", "--ai-review"]).is_err());
        assert!(parse(&["import", "/music", "--ai-review"]).is_err());
        assert!(parse(&["import", "/music", "--match", "--ai-review"]).is_ok());
        assert!(parse(&["import", "/music", "--match", "--ai-match"]).is_ok());
    }

    #[test]
    fn discogs_release_requires_local_release() {
        assert!(parse(&["match", "--discogs-release", "42"]).is_err());
        assert!(parse(&["match", "--local-release", "3", "--discogs-release", "42"]).is_ok());
    }

    #[test]
    fn musicbrainz_release_requires_local_release_and_conflicts_with_discogs() {
        let mbid = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";
        assert!(parse(&["match", "--musicbrainz-release", mbid]).is_err());
        assert!(
            parse(&[
                "match",
                "--local-release",
                "3",
                "--musicbrainz-release",
                mbid
            ])
            .is_ok()
        );
        assert!(
            parse(&[
                "match",
                "--local-release",
                "3",
                "--musicbrainz-release",
                mbid,
                "--discogs-release",
                "42"
            ])
            .is_err()
        );
    }

    #[test]
    fn provider_parsing_rejects_unknown_services() {
        assert!(parse(&["match", "--provider", "spotify"]).is_err());
        assert!(parse(&["match", "--provider", "musicbrainz"]).is_ok());
        assert!(parse(&["match", "--provider", "discogs"]).is_ok());
    }

    #[test]
    fn clear_conflicts_with_matching_options() {
        assert!(parse(&["match", "--clear", "3", "--review"]).is_err());
        assert!(parse(&["match", "--clear", "3", "/music"]).is_err());
        assert!(parse(&["match", "--clear", "3", "--provider", "discogs"]).is_err());
        assert!(parse(&["match", "--clear", "3"]).is_ok());
    }

    #[test]
    fn local_release_conflicts_with_a_path() {
        assert!(parse(&["match", "--local-release", "3", "/music"]).is_err());
    }

    #[test]
    fn review_can_revisit_an_explicit_local_release() {
        assert!(parse(&["match", "--review", "--local-release", "3"]).is_ok());
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

    #[test]
    fn direct_id_validation_happens_before_the_database_opens() {
        let cli = parse(&[
            "match",
            "--local-release",
            "3",
            "--musicbrainz-release",
            "not-an-mbid",
        ])
        .unwrap();
        let selection = match cli.command {
            Command::Match {
                musicbrainz_release,
                discogs_release,
                ..
            } => parse_direct(musicbrainz_release.as_deref(), discogs_release),
            _ => panic!("expected match"),
        };
        assert!(selection.is_err());
    }

    #[test]
    fn contradictory_provider_and_direct_id_is_rejected() {
        let id: ReleaseId =
            ReleaseId::MusicBrainz("76df3287-6cda-33eb-8e9a-044b5e15ffdd".parse().unwrap());
        assert!(runner::resolve_policy(Some(ProviderKind::Discogs), Some(&id)).is_err());
    }
}
