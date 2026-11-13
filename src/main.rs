use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use radish::layout::{Column, Table, format_duration, plural, track_number};
use radish::output::{Clean, clean_path};
use radish::progress::ImportProgress;
use radish::providers::{ProviderKind, ReleaseId};
use radish::{db, importer, index, outln, runner, store, watcher, web};
use std::io::{self, IsTerminal};
use std::net::SocketAddr;
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
        /// Read every file again, even when its modification time is unchanged
        #[arg(long)]
        rescan: bool,
        /// Match against locally built dump indexes instead of the network
        #[arg(long, requires = "match_releases")]
        offline: bool,
        /// Offline releases matched at once (default: one per core, up to 8)
        #[arg(
            long,
            value_name = "N",
            requires = "offline",
            value_parser = clap::value_parser!(u16).range(1..=i64::from(runner::MAX_JOBS))
        )]
        jobs: Option<u16>,
    },
    /// Build and inspect the local provider indexes used by --offline
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Watch a music directory and keep the library in sync in real time
    Watch { path: PathBuf },
    /// Serve a read-only web interface: status, tracks (with search) and releases
    Serve {
        /// Address to listen on. Anything but a loopback address exposes the
        /// library paths to the network without authentication.
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
    },
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
        /// Match against locally built dump indexes instead of the network
        #[arg(long, conflicts_with = "clear")]
        offline: bool,
        /// Offline releases matched at once (default: one per core, up to 8)
        #[arg(
            long,
            value_name = "N",
            requires = "offline",
            conflicts_with = "review",
            value_parser = clap::value_parser!(u16).range(1..=i64::from(runner::MAX_JOBS))
        )]
        jobs: Option<u16>,
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
                "dry_run"
            ]
        )]
        clear: Option<i64>,
    },
}

#[derive(Subcommand)]
enum IndexAction {
    /// Build an index from an official data dump (at least one flag)
    Build {
        /// MusicBrainz core dump (mbdump.tar.bz2)
        #[arg(long, value_name = "PATH")]
        musicbrainz: Option<PathBuf>,
        /// Discogs releases dump (releases.xml.gz, or the tar holding it)
        #[arg(long, value_name = "PATH")]
        discogs: Option<PathBuf>,
    },
    /// Show each index's dump date, size, and release count
    Status,
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

/// The most releases `--offline` matches at once when `--jobs` is not given.
///
/// Offline matching spends nearly all of its time in SQLite queries, which run
/// on their own threads, so it speeds up with the cores it gets. On an 8-core,
/// 16-thread laptop a 6,819-release library took 112 s with 8 jobs and 92 s
/// with 16: this default keeps most of the gain and leaves half the threads to
/// the rest of the machine. Machines with fewer cores get one job per core.
const DEFAULT_OFFLINE_JOBS: usize = 8;

/// How many releases a match run handles at once.
///
/// Network matching is paced by the services' rate limits, so it always takes
/// one release at a time.
fn match_jobs(offline: bool, jobs: Option<u16>) -> usize {
    if !offline {
        return 1;
    }
    jobs.map_or_else(
        || {
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get)
                .min(DEFAULT_OFFLINE_JOBS)
        },
        usize::from,
    )
}

/// The only keys read from `.env`. Endpoint overrides are deliberately excluded
/// so a stray `.env` can never redirect real credentials to another host.
const DOTENV_KEYS: [&str; 2] = ["RADISH_DISCOGS_TOKEN", "RADISH_INDEX_DIR"];

/// Loads allowed keys from `./.env` in the current directory only, never from a
/// parent. Existing environment variables win. Anything but a readable regular
/// file (absent, a directory such as a virtualenv, a FIFO) is ignored quietly,
/// and so are malformed lines.
fn load_dotenv() {
    // Opening a directory succeeds but every read fails, which would make the
    // line iterator yield errors forever.
    if !std::fs::metadata(".env").is_ok_and(|metadata| metadata.is_file()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    // Some editors and PowerShell 5 write a UTF-8 byte order mark, which
    // would otherwise become part of the first key.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    for entry in dotenvy::from_read_iter(text.as_bytes()) {
        match entry {
            Ok((key, value)) => {
                if DOTENV_KEYS.contains(&key.as_str()) && std::env::var_os(&key).is_none() {
                    // SAFETY: still single-threaded; Tokio has not started.
                    unsafe { std::env::set_var(key, value) }
                }
            }
            // A malformed line is skipped; a read error would repeat forever.
            Err(dotenvy::Error::Io(_)) => break,
            Err(_) => {}
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

    // `index build` and `--offline` resolve the directory the same way.
    let index_dir_value = std::env::var(index::INDEX_DIR_ENV).ok();
    let index_dir = index::index_dir_value(index_dir_value.as_deref());

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
            rescan,
            offline,
            jobs,
        } => {
            let canonical = run_import(&pool, &path, rescan).await?;
            if match_releases {
                let options = runner::MatchOptions {
                    scope: Some(canonical),
                    policy: policy.unwrap_or_default(),
                    jobs: match_jobs(offline, jobs),
                    ..runner::MatchOptions::default()
                };
                let summary = if offline {
                    runner::run_match_offline(&pool, options, &index_dir).await?
                } else {
                    runner::run_match(&pool, options).await?
                };
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
        Command::Serve { bind } => web::serve(pool, bind).await?,
        Command::Index { action } => match action {
            IndexAction::Build {
                musicbrainz,
                discogs,
            } => index::cli::build(musicbrainz, discogs, &index_dir).await?,
            IndexAction::Status => index::cli::status(&index_dir).await?,
        },
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
            offline,
            jobs,
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
                let options = runner::MatchOptions {
                    scope,
                    local_release,
                    direct_release: direct,
                    review,
                    dry_run,
                    policy: policy.unwrap_or_default(),
                    jobs: match_jobs(offline, jobs),
                };
                let summary = if offline {
                    runner::run_match_offline(&pool, options, &index_dir).await?
                } else {
                    runner::run_match(&pool, options).await?
                };
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

/// Display columns kept for artist, title and album on a terminal.
///
/// Wide enough for most titles on an 80-column terminal alongside the
/// number and duration columns; piped output is never cut.
const TEXT_COLUMN_CAP: usize = 40;

fn print_releases(rows: &[store::LocalReleaseStatus]) {
    if rows.is_empty() {
        outln!("No local releases found.");
        return;
    }
    for line in render_releases(rows) {
        outln!("{line}");
    }
}

/// Lays out one block per local release, then the total.
fn render_releases(rows: &[store::LocalReleaseStatus]) -> Vec<String> {
    // Continuation lines line up under the artist, past the widest `#id`.
    let id_width = rows
        .iter()
        .map(|row| format!("#{}", row.id).len())
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(id_width + 2);

    let mut lines = Vec::new();
    for row in rows {
        let artist = row.album_artist.as_deref().unwrap_or("(unknown artist)");
        let title = row.album_title.as_deref().unwrap_or("(no album tag)");
        let status = row.status_label();
        let tracks = plural(row.track_count.max(0) as u64, "track", "tracks");
        lines.push(format!(
            "{:<id_width$}  {} — {}",
            format!("#{}", row.id),
            Clean(artist),
            Clean(title)
        ));
        lines.push(format!("{indent}{tracks} · {status}"));
        if let (Some(provider), Some(id)) = (&row.provider, &row.remote_release_id) {
            let remote_title = row
                .remote_title
                .as_deref()
                .map(|title| format!(" ({})", Clean(title)))
                .unwrap_or_default();
            lines.push(format!(
                "{indent}match:  {provider} {}{remote_title}",
                Clean(id)
            ));
        }
        lines.push(format!("{indent}path:   {}", Clean(&row.directory)));
        lines.push(String::new());
    }
    lines.push(format!(
        "{}.",
        plural(rows.len() as u64, "local release", "local releases")
    ));
    lines
}

fn print_tracks(rows: &[store::TrackRow]) {
    if rows.is_empty() {
        outln!("No tracks found.");
        return;
    }
    for line in render_tracks(rows, io::stdout().is_terminal()) {
        outln!("{line}");
    }
}

/// Lays out the tracks as aligned columns, then the total.
///
/// `truncate` shortens long text cells; leave it off for piped output.
fn render_tracks(rows: &[store::TrackRow], truncate: bool) -> Vec<String> {
    let mut table = Table::new(vec![
        Column::new("ARTIST").capped(TEXT_COLUMN_CAP),
        Column::new("TITLE").capped(TEXT_COLUMN_CAP),
        Column::new("ALBUM").capped(TEXT_COLUMN_CAP),
        Column::new("#").right(),
        Column::new("TIME").right(),
        // Header-less and last, so the stale marker stays unpadded.
        Column::new(""),
    ]);
    for row in rows {
        table.push([
            row.artist.as_deref().unwrap_or("(unknown artist)"),
            &row.display_title(),
            row.album.as_deref().unwrap_or("-"),
            &track_number(row.disc_number, row.track_number),
            &format_duration(row.duration_secs),
            if row.stale { "stale match" } else { "" },
        ]);
    }

    let mut lines = table.render(truncate);
    lines.push(String::new());
    lines.push(format!("{}.", plural(rows.len() as u64, "track", "tracks")));
    lines
}

async fn run_import(
    pool: &sqlx::SqlitePool,
    path: &std::path::Path,
    rescan: bool,
) -> Result<PathBuf> {
    let path = path.canonicalize()?;
    outln!("Scanning {}...", clean_path(&path));

    let progress = ImportProgress::new();
    let summary = importer::sync_directory(pool, &path, &progress, rescan).await?;
    drop(progress);

    let mut kept = String::new();
    if summary.kept > 0 {
        kept.push_str(&format!(", {} kept (unreadable)", summary.kept));
    }
    if summary.kept_no_audio > 0 {
        kept.push_str(&format!(
            ", {} kept (no audio files found)",
            summary.kept_no_audio
        ));
    }
    outln!(
        "Imported {} into {}: {} unchanged, {} removed{kept}.",
        plural(
            summary.saved,
            "new or changed track",
            "new or changed tracks"
        ),
        plural(
            summary.rebuild.groups as u64,
            "local release",
            "local releases"
        ),
        summary.unchanged,
        summary.removed
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
    fn offline_matching_selects_the_provider_policy_before_any_work() {
        let cli = parse(&["match", "--offline", "--provider", "musicbrainz"]).unwrap();
        match cli.command {
            Command::Match {
                provider, offline, ..
            } => {
                assert!(offline);
                assert_eq!(provider, Some(ProviderKind::MusicBrainz));
            }
            _ => panic!("expected match"),
        }
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
    fn jobs_need_offline_matching_and_stay_in_range() {
        match parse(&["match", "--offline", "--jobs", "4"])
            .unwrap()
            .command
        {
            Command::Match { jobs, .. } => assert_eq!(jobs, Some(4)),
            _ => panic!("expected match"),
        }
        assert!(parse(&["import", "/music", "--match", "--offline", "--jobs", "4"]).is_ok());
        // Network matching is paced by the services' rate limits.
        assert!(parse(&["match", "--jobs", "4"]).is_err());
        assert!(parse(&["import", "/music", "--match", "--jobs", "4"]).is_err());
        // Interactive review asks about one release at a time.
        assert!(parse(&["match", "--offline", "--review", "--jobs", "2"]).is_err());
        assert!(parse(&["match", "--offline", "--jobs", "0"]).is_err());
        assert!(parse(&["match", "--offline", "--jobs", "17"]).is_err());
    }

    #[test]
    fn offline_matching_runs_several_releases_unless_told_otherwise() {
        assert_eq!(match_jobs(false, None), 1);
        assert_eq!(match_jobs(true, Some(3)), 3);
        let default = match_jobs(true, None);
        assert!((1..=DEFAULT_OFFLINE_JOBS).contains(&default), "{default}");
    }

    #[test]
    fn offline_requires_matching_and_conflicts_with_clearing() {
        assert!(parse(&["match", "--offline"]).is_ok());
        assert!(parse(&["import", "/music", "--offline"]).is_err());
        assert!(parse(&["import", "/music", "--match", "--offline"]).is_ok());
        assert!(parse(&["match", "--clear", "3", "--offline"]).is_err());
    }

    #[test]
    fn index_commands_parse() {
        let cli = parse(&[
            "index",
            "build",
            "--musicbrainz",
            "/dumps/mbdump.tar.bz2",
            "--discogs",
            "/dumps/releases.xml.gz",
        ])
        .unwrap();
        match cli.command {
            Command::Index {
                action:
                    IndexAction::Build {
                        musicbrainz,
                        discogs,
                    },
            } => {
                assert_eq!(musicbrainz, Some(PathBuf::from("/dumps/mbdump.tar.bz2")));
                assert_eq!(discogs, Some(PathBuf::from("/dumps/releases.xml.gz")));
            }
            _ => panic!("expected index build"),
        }
        assert!(parse(&["index", "status"]).is_ok());
        assert!(parse(&["index", "build"]).is_ok());
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

    fn release(id: i64) -> store::LocalReleaseStatus {
        store::LocalReleaseStatus {
            id,
            directory: "/music/Test Artist/Blue".into(),
            album_title: Some("Blue".into()),
            album_artist: Some("Test Artist".into()),
            track_count: 2,
            status: "unmatched".into(),
            provider: None,
            remote_release_id: None,
            remote_url: None,
            remote_title: None,
            match_scope: None,
            stale: false,
            queued_providers: Vec::new(),
        }
    }

    #[test]
    fn releases_render_as_blocks_with_a_match_line_only_when_matched() {
        let mut matched = release(12);
        matched.status = store::STATUS_MATCHED.into();
        matched.provider = Some(ProviderKind::MusicBrainz);
        matched.remote_release_id = Some("76df3287-6cda-33eb-8e9a-044b5e15ffdd".into());
        matched.remote_title = Some("Blue".into());
        let mut bare = release(130);
        bare.album_title = None;
        bare.album_artist = None;
        bare.track_count = 1;
        bare.directory = "/music/misc".into();

        assert_eq!(
            render_releases(&[matched, bare]),
            [
                "#12   Test Artist — Blue",
                "      2 tracks · matched",
                "      match:  musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd (Blue)",
                "      path:   /music/Test Artist/Blue",
                "",
                "#130  (unknown artist) — (no album tag)",
                "      1 track · unmatched",
                "      path:   /music/misc",
                "",
                "2 local releases.",
            ]
        );
    }

    #[test]
    fn release_status_keeps_its_album_stale_and_queue_notes() {
        let mut row = release(1);
        row.status = store::STATUS_MATCHED.into();
        row.match_scope = Some("album".into());
        row.stale = true;
        let lines = render_releases(&[row]);
        assert_eq!(
            lines[1],
            "    2 tracks · matched, album match; edition unconfirmed, stale"
        );

        let mut queued = release(1);
        queued.queued_providers = vec![ProviderKind::MusicBrainz];
        assert_eq!(
            render_releases(&[queued])[1],
            "    2 tracks · unmatched, queued (musicbrainz)"
        );
        assert_eq!(
            render_releases(&[release(1)]).last().unwrap(),
            "1 local release."
        );
    }

    fn track(path: &str) -> store::TrackRow {
        store::TrackRow {
            path: path.into(),
            title: Some("Blue Moon".into()),
            artist: Some("Test Artist".into()),
            album: Some("Blue".into()),
            track_number: Some(3),
            disc_number: Some(1),
            duration_secs: Some(225.0),
            stale: false,
        }
    }

    #[test]
    fn tracks_render_as_aligned_columns() {
        let mut second = track("/m/2.flac");
        second.title = Some("A Much Longer Title".into());
        second.album = Some("Live in Tokyo".into());
        second.disc_number = Some(2);
        second.track_number = Some(1);
        second.duration_secs = Some(730.0);
        second.stale = true;
        let mut bare = track("/m/untitled.wav");
        bare.title = None;
        bare.artist = None;
        bare.album = None;
        bare.track_number = None;
        bare.duration_secs = None;

        assert_eq!(
            render_tracks(&[track("/m/1.flac"), second, bare], false),
            [
                "ARTIST            TITLE                ALBUM            #   TIME",
                "Test Artist       Blue Moon            Blue             3   3:45",
                "Test Artist       A Much Longer Title  Live in Tokyo  2-1  12:10  stale match",
                "(unknown artist)  untitled.wav         -                -   ?:??",
                "",
                "3 tracks.",
            ]
        );
    }

    #[test]
    fn long_track_text_is_cut_only_for_terminals() {
        let long = || {
            let mut row = track("/m/1.flac");
            row.title = Some("x".repeat(60));
            row
        };
        assert!(render_tracks(&[long()], false)[1].contains(&"x".repeat(60)));
        let cut = render_tracks(&[long()], true);
        assert!(cut[1].contains(&format!("{}…", "x".repeat(TEXT_COLUMN_CAP - 1))));
    }

    #[test]
    fn contradictory_provider_and_direct_id_is_rejected() {
        let id: ReleaseId =
            ReleaseId::MusicBrainz("76df3287-6cda-33eb-8e9a-044b5e15ffdd".parse().unwrap());
        assert!(runner::resolve_policy(Some(ProviderKind::Discogs), Some(&id)).is_err());
    }
}
