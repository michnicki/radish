use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use radish::grouping::{self, FolderAction};
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
        /// then Discogs when a token is available, then Deezer with --deezer)
        #[arg(long = "match")]
        match_releases: bool,
        /// Restrict matching to one service and disable fallback
        #[arg(long, value_name = "PROVIDER", requires = "match_releases")]
        provider: Option<ProviderKind>,
        /// Try Deezer after Discogs, over the network (with --offline too)
        #[arg(long, requires = "match_releases", conflicts_with = "provider")]
        deezer: bool,
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
    /// Merge a folder's releases into one, keep them apart, or let Radish decide
    Folder {
        #[command(subcommand)]
        action: FolderCommand,
    },
    /// Match local releases against MusicBrainz, with optional Discogs and Deezer fallbacks
    Match {
        /// Restrict matching to this directory tree
        path: Option<PathBuf>,
        /// Only process this local release
        #[arg(long = "local-release", value_name = "ID", conflicts_with = "path")]
        local_release: Option<i64>,
        /// Restrict matching to one service and disable fallback
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<ProviderKind>,
        /// Try Deezer after Discogs, over the network (with --offline too)
        #[arg(
            long,
            conflicts_with_all = [
                "provider",
                "musicbrainz_release",
                "discogs_release",
                "deezer_release",
                "clear"
            ]
        )]
        deezer: bool,
        /// Fetch this MusicBrainz release directly, bypassing search
        #[arg(
            long = "musicbrainz-release",
            value_name = "MBID",
            requires = "local_release",
            conflicts_with_all = ["discogs_release", "deezer_release"]
        )]
        musicbrainz_release: Option<String>,
        /// Fetch this Discogs release directly, bypassing search
        #[arg(
            long = "discogs-release",
            value_name = "ID",
            requires = "local_release",
            conflicts_with_all = ["musicbrainz_release", "deezer_release"]
        )]
        discogs_release: Option<u64>,
        /// Fetch this Deezer album directly, bypassing search
        #[arg(
            long = "deezer-release",
            value_name = "ID",
            requires = "local_release",
            conflicts_with_all = ["musicbrainz_release", "discogs_release"]
        )]
        deezer_release: Option<u64>,
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
                "deezer_release",
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

#[derive(Subcommand)]
enum FolderCommand {
    /// Merge each target's folder into one local release
    Merge {
        /// Local release IDs or folder paths (a folder named only by digits needs a path
        /// form such as ./2001)
        #[arg(required = true, value_name = "TARGET")]
        targets: Vec<String>,
    },
    /// Keep each target's folder split by album title
    Split {
        /// Local release IDs or folder paths (a folder named only by digits needs a path
        /// form such as ./2001)
        #[arg(required = true, value_name = "TARGET")]
        targets: Vec<String>,
    },
    /// Drop the decision for each target's folder; the automatic rule decides again
    Auto {
        /// Local release IDs or folder paths (a folder named only by digits needs a path
        /// form such as ./2001)
        #[arg(required = true, value_name = "TARGET")]
        targets: Vec<String>,
    },
    /// List folders with several releases, a merged release, or a decision
    List {
        /// Restrict output to this directory tree
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

/// One folder target as typed: what to look up, in order, and how to name it
/// in an error.
#[derive(Debug, PartialEq, Eq)]
struct FolderRequest {
    candidates: Vec<store::FolderTarget>,
    given: String,
}

/// A path as this machine sees it, else as written: the library may be
/// imported on another machine, so a path that does not exist here is still
/// a valid name.
fn existing_or_as_written(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Reads a folder target: all ASCII digits name a local release, anything
/// else a folder. A folder is looked up by its canonical path first, then by
/// the path as written, since a symlinked mount here may differ from the form
/// stored on the machine that imported it.
fn parse_folder_target(raw: &str) -> Result<FolderRequest> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("an empty folder target names nothing");
    }
    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        let id = trimmed
            .parse::<i64>()
            .with_context(|| format!("'{}' is not a local release ID", Clean(trimmed)))?;
        return Ok(FolderRequest {
            candidates: vec![store::FolderTarget::Release(id)],
            given: trimmed.to_string(),
        });
    }
    let canonical = existing_or_as_written(Path::new(raw))
        .to_string_lossy()
        .into_owned();
    let mut candidates = vec![store::FolderTarget::Path(canonical.clone())];
    if canonical != raw {
        candidates.push(store::FolderTarget::Path(raw.to_string()));
    }
    Ok(FolderRequest {
        candidates,
        given: raw.to_string(),
    })
}

/// Resolves every target first, then records `action` for each distinct unit.
async fn apply_folder_action(
    pool: &sqlx::SqlitePool,
    requests: &[FolderRequest],
    action: FolderAction,
) -> Result<()> {
    let mut units: Vec<String> = Vec::new();
    for request in requests {
        let mut found = None;
        for candidate in &request.candidates {
            found = store::resolve_folder(pool, candidate).await?;
            if found.is_some() {
                break;
            }
        }
        let Some(unit) = found else {
            match request.candidates.first() {
                Some(store::FolderTarget::Release(id)) => {
                    bail!("local release {id} was not found")
                }
                _ => bail!(
                    "'{}' is not a folder with local releases",
                    Clean(&request.given)
                ),
            }
        };
        if !units.iter().any(|known| known.eq_ignore_ascii_case(&unit)) {
            units.push(unit);
        }
    }
    for unit in units {
        let outcome = store::set_folder_mode(pool, &unit, action.mode()).await?;
        for line in render_folder_outcome(action, &outcome) {
            outln!("{line}");
        }
    }
    Ok(())
}

/// What one folder decision did, for the terminal.
fn render_folder_outcome(action: FolderAction, outcome: &store::FolderOutcome) -> Vec<String> {
    let verb = match action {
        FolderAction::Merge => "Merged",
        FolderAction::Separate => "Kept separate",
        FolderAction::Auto => "Left to the automatic rule",
    };
    let mut lines = vec![format!("{verb}: {}", Clean(&outcome.unit))];
    let releases: Vec<String> = outcome
        .releases
        .iter()
        .map(|(id, tracks)| {
            format!(
                "#{id} ({})",
                plural((*tracks).max(0) as u64, "track", "tracks")
            )
        })
        .collect();
    lines.push(match releases.as_slice() {
        [] => "  no releases".to_string(),
        [one] => format!("  release {one}"),
        many => format!("  releases {}", many.join(", ")),
    });
    let summary = &outcome.summary;
    if summary.newly_stale > 0 {
        lines.push(format!(
            "  {} marked stale",
            plural(
                summary.newly_stale as u64,
                "accepted match",
                "accepted matches"
            )
        ));
    }
    if summary.dropped_matches > 0 {
        lines.push(format!(
            "  {} dropped",
            plural(
                summary.dropped_matches as u64,
                "accepted match",
                "accepted matches"
            )
        ));
    }
    if summary.changed()
        && let [(id, _)] = outcome.releases.as_slice()
    {
        lines.push(format!(
            "  run `radish match --local-release {id}` to match it"
        ));
    }
    lines
}

/// Prints the folder blocks, or a note when no folder qualifies.
fn print_folder_units(units: &[store::FolderUnit]) {
    if units.is_empty() {
        outln!("No folder holds several releases, a merged release or a grouping decision.");
        return;
    }
    for line in render_folder_units(units) {
        outln!("{line}");
    }
}

/// One block per unit: path, state, releases; then the total.
fn render_folder_units(units: &[store::FolderUnit]) -> Vec<String> {
    let mut lines = Vec::new();
    for unit in units {
        lines.push(Clean(&unit.path).to_string());
        lines.push(format!("  {}", unit.state.label()));
        for row in &unit.releases {
            lines.push(format!(
                "  #{}  {} — {} · {} · {}",
                row.id,
                Clean(row.album_artist.as_deref().unwrap_or("(unknown artist)")),
                Clean(row.album_title.as_deref().unwrap_or("(no album tag)")),
                plural(row.track_count.max(0) as u64, "track", "tracks"),
                row.status_label()
            ));
        }
        lines.push(String::new());
    }
    lines.push(format!(
        "{}.",
        plural(units.len() as u64, "folder", "folders")
    ));
    lines
}

fn parse_direct(
    musicbrainz: Option<&str>,
    discogs: Option<u64>,
    deezer: Option<u64>,
) -> Result<Option<ReleaseId>> {
    match (musicbrainz, discogs, deezer) {
        (Some(raw), None, None) => {
            let id = raw
                .parse()
                .with_context(|| format!("'{raw}' is not a hyphenated MusicBrainz MBID"))?;
            Ok(Some(ReleaseId::MusicBrainz(id)))
        }
        (None, Some(id), None) => Ok(Some(ReleaseId::Discogs(id))),
        (None, None, Some(id)) => Ok(Some(ReleaseId::Deezer(id))),
        (None, None, None) => Ok(None),
        _ => bail!("choose one release selector: MusicBrainz, Discogs or Deezer"),
    }
}

/// The provider choices of `match` and `import --match`, read before the
/// database is opened.
struct Selection {
    provider: Option<ProviderKind>,
    direct: Option<ReleaseId>,
    deezer: bool,
    offline: bool,
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
    let selection = match &cli.command {
        Command::Match {
            provider,
            musicbrainz_release,
            discogs_release,
            deezer_release,
            deezer,
            offline,
            ..
        } => Some(Selection {
            provider: *provider,
            direct: parse_direct(
                musicbrainz_release.as_deref(),
                *discogs_release,
                *deezer_release,
            )?,
            deezer: *deezer,
            offline: *offline,
        }),
        Command::Import {
            provider,
            deezer,
            offline,
            ..
        } => Some(Selection {
            provider: *provider,
            direct: None,
            deezer: *deezer,
            offline: *offline,
        }),
        _ => None,
    };
    let policy = match &selection {
        Some(selection) => {
            let policy = runner::resolve_policy(
                selection.provider,
                selection.direct.as_ref(),
                selection.deezer,
            )?;
            if selection.offline {
                runner::check_offline(policy)?;
            }
            Some(policy)
        }
        None => None,
    };
    let direct = selection.and_then(|selection| selection.direct);

    // Folder targets are parsed before the database is opened, and resolved
    // against it before anything is written.
    let folder_targets = match &cli.command {
        Command::Folder {
            action:
                FolderCommand::Merge { targets }
                | FolderCommand::Split { targets }
                | FolderCommand::Auto { targets },
        } => targets
            .iter()
            .map(|target| parse_folder_target(target))
            .collect::<Result<Vec<_>>>()?,
        _ => Vec::new(),
    };

    let pool = db::connect(&cli.db).await?;

    match cli.command {
        Command::Import {
            path,
            match_releases,
            provider: _,
            deezer: _,
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
        Command::Folder { action } => match action {
            FolderCommand::List { path } => {
                // Resolved like a target path, then widened to whole units.
                let scope = path
                    .as_deref()
                    .map(|path| grouping::unit_scope(&existing_or_as_written(path)));
                print_folder_units(&store::list_folder_units(&pool, scope.as_deref()).await?);
            }
            FolderCommand::Merge { .. } => {
                apply_folder_action(&pool, &folder_targets, FolderAction::Merge).await?;
            }
            FolderCommand::Split { .. } => {
                apply_folder_action(&pool, &folder_targets, FolderAction::Separate).await?;
            }
            FolderCommand::Auto { .. } => {
                apply_folder_action(&pool, &folder_targets, FolderAction::Auto).await?;
            }
        },
        Command::Match {
            path,
            local_release,
            provider: _,
            deezer: _,
            musicbrainz_release: _,
            discogs_release: _,
            deezer_release: _,
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
        if let Some(merged) = &row.merged {
            lines.push(format!("{indent}merged: {}", merged.label()));
        }
        if let (Some(provider), Some(id)) = (&row.provider, &row.remote_release_id) {
            // The title, then the disc when the files are one disc of the release.
            let mut detail: Vec<String> = row
                .remote_title
                .as_deref()
                .map(|title| Clean(title).to_string())
                .into_iter()
                .collect();
            detail.extend(row.medium.map(|medium| format!("disc {medium}")));
            let remote_title = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", detail.join(", "))
            };
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
        assert!(parse(&["match", "--provider", "deezer"]).is_ok());
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
            } => parse_direct(musicbrainz_release.as_deref(), discogs_release, None),
            _ => panic!("expected match"),
        };
        assert!(selection.is_err());
    }

    fn release(id: i64) -> store::LocalReleaseStatus {
        store::LocalReleaseStatus {
            id,
            directory: "/music/Test Artist/Blue".into(),
            unit: "/music/Test Artist/Blue".into(),
            album_title: Some("Blue".into()),
            album_artist: Some("Test Artist".into()),
            track_count: 2,
            status: "unmatched".into(),
            provider: None,
            remote_release_id: None,
            remote_url: None,
            remote_title: None,
            match_scope: None,
            medium: None,
            stale: false,
            queued_providers: Vec::new(),
            merged: None,
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
    fn a_disc_match_names_its_disc_on_the_match_line() {
        let mut row = release(12);
        row.status = store::STATUS_MATCHED.into();
        row.provider = Some(ProviderKind::MusicBrainz);
        row.remote_release_id = Some("76df3287-6cda-33eb-8e9a-044b5e15ffdd".into());
        row.remote_title = Some("Sandinista!".into());
        row.medium = Some(2);
        assert_eq!(
            render_releases(std::slice::from_ref(&row))[2],
            "     match:  musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd (Sandinista!, disc 2)"
        );
        row.remote_title = None;
        assert_eq!(
            render_releases(&[row])[2],
            "     match:  musicbrainz 76df3287-6cda-33eb-8e9a-044b5e15ffdd (disc 2)"
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
        assert!(runner::resolve_policy(Some(ProviderKind::Discogs), Some(&id), false).is_err());
    }

    #[test]
    fn deezer_joins_match_and_import_match_but_no_explicit_selection() {
        let cli = parse(&["match", "--deezer", "--offline"]).unwrap();
        match cli.command {
            Command::Match {
                deezer, offline, ..
            } => assert!(deezer && offline),
            _ => panic!("expected match"),
        }
        assert!(parse(&["import", "/music", "--match", "--deezer"]).is_ok());
        assert!(parse(&["import", "/music", "--deezer"]).is_err());
        for conflict in [
            &["--provider", "discogs"][..],
            &["--clear", "3"],
            &["--local-release", "3", "--discogs-release", "42"],
            &["--local-release", "3", "--deezer-release", "302127"],
        ] {
            let mut args = vec!["match", "--deezer"];
            args.extend_from_slice(conflict);
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(
            parse(&[
                "import",
                "/music",
                "--match",
                "--deezer",
                "--provider",
                "musicbrainz"
            ])
            .is_err()
        );
    }

    #[test]
    fn deezer_release_is_a_direct_selector() {
        assert!(parse(&["match", "--deezer-release", "302127"]).is_err());
        assert!(
            parse(&[
                "match",
                "--local-release",
                "3",
                "--deezer-release",
                "302127"
            ])
            .is_ok()
        );
        assert!(parse(&["match", "--local-release", "3", "--deezer-release", "x"]).is_err());
        for other in [
            &["--discogs-release", "42"][..],
            &[
                "--musicbrainz-release",
                "76df3287-6cda-33eb-8e9a-044b5e15ffdd",
            ],
        ] {
            let mut args = vec!["match", "--local-release", "3", "--deezer-release", "1"];
            args.extend_from_slice(other);
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(parse(&["match", "--clear", "3", "--deezer-release", "1"]).is_err());
        assert_eq!(
            parse_direct(None, None, Some(302127)).unwrap(),
            Some(ReleaseId::Deezer(302127))
        );
        assert!(parse_direct(None, Some(42), Some(302127)).is_err());
    }

    #[test]
    fn deezer_alone_is_a_network_run() {
        assert!(parse(&["match", "--provider", "deezer"]).is_ok());
        let only = runner::resolve_policy(Some(ProviderKind::Deezer), None, false).unwrap();
        assert!(runner::check_offline(only).is_err());
        let direct = runner::resolve_policy(None, Some(&ReleaseId::Deezer(1)), false).unwrap();
        assert!(runner::check_offline(direct).is_err());
        let chain = runner::resolve_policy(None, None, true).unwrap();
        assert!(runner::check_offline(chain).is_ok());
        let musicbrainz =
            runner::resolve_policy(Some(ProviderKind::MusicBrainz), None, false).unwrap();
        assert!(runner::check_offline(musicbrainz).is_ok());
    }

    #[test]
    fn folder_commands_parse_targets_and_need_one() {
        let cli = parse(&["folder", "merge", "12", "/music/x"]).unwrap();
        match cli.command {
            Command::Folder {
                action: FolderCommand::Merge { targets },
            } => assert_eq!(targets, ["12", "/music/x"]),
            _ => panic!("expected folder merge"),
        }
        assert!(parse(&["folder", "split"]).is_err());
        assert!(parse(&["folder", "list", "--path", "/music"]).is_ok());
    }

    #[test]
    fn folder_targets_are_release_ids_or_paths() {
        assert_eq!(
            parse_folder_target(" 5586 ").unwrap(),
            FolderRequest {
                candidates: vec![store::FolderTarget::Release(5586)],
                given: "5586".into(),
            }
        );
        // A path that does not exist here is tried as written, spaces kept.
        assert_eq!(
            parse_folder_target("/nowhere/radish-test/x ").unwrap(),
            FolderRequest {
                candidates: vec![store::FolderTarget::Path("/nowhere/radish-test/x ".into())],
                given: "/nowhere/radish-test/x ".into(),
            }
        );
        assert!(parse_folder_target("99999999999999999999").is_err());
        assert!(parse_folder_target("  ").is_err());
    }

    #[test]
    fn an_existing_folder_is_tried_canonical_first_then_as_written() {
        // The current directory always exists, whatever the platform.
        let canonical = std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let request = parse_folder_target(".").unwrap();
        assert_eq!(request.given, ".");
        assert_eq!(
            request.candidates,
            vec![
                store::FolderTarget::Path(canonical),
                store::FolderTarget::Path(".".into()),
            ]
        );
    }

    #[test]
    fn folder_outcomes_name_the_releases_and_the_matches_they_touched() {
        let merged = store::FolderOutcome {
            unit: "/music/box".into(),
            summary: store::RebuildSummary {
                rekeyed: 1,
                removed: 1,
                newly_stale: 1,
                dropped_matches: 1,
                ..store::RebuildSummary::default()
            },
            releases: vec![(7, 4)],
        };
        assert_eq!(
            render_folder_outcome(FolderAction::Merge, &merged),
            vec![
                "Merged: /music/box",
                "  release #7 (4 tracks)",
                "  1 accepted match marked stale",
                "  1 accepted match dropped",
                "  run `radish match --local-release 7` to match it",
            ]
        );
        let unchanged = store::FolderOutcome {
            unit: "/music/box".into(),
            summary: store::RebuildSummary::default(),
            releases: vec![(7, 2), (8, 2)],
        };
        assert_eq!(
            render_folder_outcome(FolderAction::Separate, &unchanged),
            vec![
                "Kept separate: /music/box",
                "  releases #7 (2 tracks), #8 (2 tracks)"
            ]
        );
    }

    #[test]
    fn folder_units_render_as_blocks() {
        let mut first = release(12);
        first.unit = "/music/box".into();
        let mut second = release(13);
        second.unit = "/music/box".into();
        second.album_title = Some("Vision Thing".into());
        let unit = store::FolderUnit {
            path: "/music/box".into(),
            state: store::FolderState::Automatic,
            releases: vec![first, second],
        };
        assert_eq!(
            render_folder_units(&[unit]),
            vec![
                "/music/box",
                "  automatic: separate",
                "  #12  Test Artist — Blue · 2 tracks · unmatched",
                "  #13  Test Artist — Vision Thing · 2 tracks · unmatched",
                "",
                "1 folder.",
            ]
        );
    }

    #[test]
    fn merged_releases_show_how_they_were_merged() {
        let mut row = release(12);
        row.merged = Some(store::MergeInfo {
            titles: 2,
            folders: 2,
            manual: true,
        });
        let lines = render_releases(&[row]);
        assert!(
            lines.contains(&"     merged: 2 album titles, 2 folders (manual)".to_string()),
            "{lines:?}"
        );
    }
}
