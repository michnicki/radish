mod db;
mod scanner;
mod store;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "radish", version, about = "A blazing-fast music library manager")]
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
    Import { path: PathBuf },
    /// Watch a music directory and keep the library in sync in real time
    Watch { path: PathBuf },
    /// List every track in the library
    List,
    /// Search the library by title, artist, or album
    Search { query: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let pool = db::connect(&cli.db).await?;

    match cli.command {
        Command::Import { path } => run_import(&pool, &path).await?,
        Command::Watch { path } => watcher::watch_directory(pool, &path).await?,
        Command::List => print_tracks(&store::list_tracks(&pool).await?),
        Command::Search { query } => print_tracks(&store::search_tracks(&pool, &query).await?),
    }

    Ok(())
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
        match (&row.album, row.track_number) {
            (Some(album), Some(n)) => println!("{artist} - {title}  [{album} #{n}]  {duration}"),
            (Some(album), None) => println!("{artist} - {title}  [{album}]  {duration}"),
            (None, _) => println!("{artist} - {title}  {duration}"),
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

async fn run_import(pool: &sqlx::SqlitePool, path: &std::path::Path) -> Result<()> {
    let path = path.canonicalize()?;
    println!("Scanning {}...", path.display());

    let directory_id = store::upsert_directory(pool, &path).await?;
    let tracks = scanner::scan_directory(&path).await?;
    let count = tracks.len();

    for track in &tracks {
        store::upsert_track(pool, track, directory_id).await?;
    }

    println!("Imported {count} tracks.");
    Ok(())
}
