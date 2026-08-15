mod api;
mod auth;
mod crypto;
mod download;
mod models;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::api::DeezerApi;
use crate::models::TrackFormat;

#[derive(Parser)]
#[command(name = "deezco", version, about = "Deezer music downloader CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Output directory for downloads
    #[arg(short, long, global = true)]
    output: Option<PathBuf>,

    /// Audio quality: flac, 320, 128
    #[arg(short, long, global = true, default_value = "320")]
    quality: String,

    /// Deezer ARL cookie (overrides any stored login)
    #[arg(long, global = true)]
    arl: Option<String>,

    /// Number of search results to print (default: 10); also caps favorites JSON output
    #[arg(short, long, global = true)]
    limit: Option<u32>,

    /// Skip the first N tracks in favorites JSON output
    #[arg(long, global = true, default_value_t = 0)]
    offset: u32,

    /// Number of parallel downloads (minimum 1)
    #[arg(
        short,
        long,
        global = true,
        default_value_t = 4,
        value_parser = clap::builder::RangedI64ValueParser::<usize>::new().range(1..)
    )]
    concurrency: usize,

    /// 1-based index of the search result to download instead of printing the list
    #[arg(long, global = true)]
    pick: Option<usize>,

    /// Print search results as JSON instead of the human-readable list
    #[arg(long, global = true)]
    json: bool,

    /// JSON output style used with --json
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Pretty)]
    output_format: OutputFormat,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Human-friendly multi-line JSON
    Pretty,
    /// Single-line JSON, ideal for piping
    Compact,
}

#[derive(Subcommand)]
enum Commands {
    /// Download a track by URL or ID (names print search results)
    Track {
        /// Deezer track URL, track ID, or search name
        query: String,
    },
    /// Download a playlist by URL or ID
    Playlist {
        /// Deezer playlist URL or playlist ID
        url: String,
    },
    /// Download your liked/favorite songs
    Favorites,
    /// Download all songs from an artist (names print search results)
    Artist {
        /// Deezer artist URL, artist ID, or search name
        query: String,
    },
    /// Download an album by URL or ID
    Album {
        /// Deezer album URL or album ID
        url: String,
    },
    /// Download all releases from every artist you follow
    Following,
    /// Remove stored login credentials
    Logout,
}

fn parse_format(quality: &str) -> TrackFormat {
    match quality.to_lowercase().as_str() {
        "flac" | "lossless" | "9" => TrackFormat::Flac,
        "320" | "mp3_320" | "3" => TrackFormat::Mp3_320,
        "128" | "mp3_128" | "1" => TrackFormat::Mp3_128,
        _ => TrackFormat::Mp3_320,
    }
}

/// Extract ID from a Deezer URL or return the input as-is if it's already an ID
fn extract_id(input: &str, _entity: &str) -> String {
    // Handle URLs like https://www.deezer.com/en/track/12345
    let trimmed = input.trim_end_matches('/');
    if trimmed.contains("deezer.com")
        && let Some(pos) = trimmed.rfind('/')
    {
        // Drop query params and fragments
        let id = trimmed[pos + 1..].split(['?', '#']).next().unwrap_or("");
        if !id.is_empty() {
            return id.to_string();
        }
    }
    // Already an ID
    input.to_string()
}

/// Print a startup banner with the tool name and version.
fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");
    let title = format!("deezco v{version} — Deezer music downloader");
    let width = title.chars().count() + 4;
    println!("╔{}╗", "═".repeat(width));
    println!("║  {title}  ║");
    println!("╚{}╝\n", "═".repeat(width));
}

/// Default output directory: the user's OS Downloads folder.
fn default_output_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| PathBuf::from("./downloads"))
}

/// Output directory from the `DEEZCO_OUTPUT_DIR` environment variable.
fn env_output_dir() -> Option<PathBuf> {
    std::env::var_os("DEEZCO_OUTPUT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Resolve the output directory: CLI flag, then env var, then the default.
fn resolve_output_dir(cli_output: Option<PathBuf>, env_output: Option<PathBuf>) -> PathBuf {
    cli_output.or(env_output).unwrap_or_else(default_output_dir)
}

/// Download a track from a URL or ID, or print search results for a name.
/// Returns `false` when nothing was downloaded (search results shown).
async fn download_track_query(
    api: &DeezerApi,
    query: &str,
    format: TrackFormat,
    output: &Path,
    limit: u32,
    pick: Option<usize>,
    output_format: Option<OutputFormat>,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "track");
        download::download_single_track(api, &id, format, output).await?;
        return Ok(true);
    }

    // Search for the track
    let results = api.search_track(query, limit).await?;

    // Auto-download a specific result instead of printing
    if let Some(pick) = pick {
        let data = match results["data"].as_array() {
            Some(data) if !data.is_empty() => data,
            _ => {
                println!("No tracks found for '{}'.", query);
                return Ok(false);
            }
        };
        if !(1..=data.len()).contains(&pick) {
            println!("No result #{pick} — found {} track(s).", data.len());
            return Ok(false);
        }
        let track = &data[pick - 1];
        let id = track["id"].as_u64().unwrap_or(0).to_string();
        if id == "0" {
            println!("Result #{pick} has no track ID.");
            return Ok(false);
        }
        download::download_single_track(api, &id, format, output).await?;
        return Ok(true);
    }

    // Print the raw API response as JSON for scripting
    if let Some(format) = output_format {
        print_json(&results, format)?;
        return Ok(false);
    }

    // Print search results so the user can pick an ID
    let data = match results["data"].as_array() {
        Some(data) if !data.is_empty() && limit > 0 => data,
        _ => {
            println!("No tracks found for '{}'.", query);
            return Ok(false);
        }
    };
    println!("Tracks matching '{query}':");
    for (i, track) in data.iter().take(limit as usize).enumerate() {
        let title = track["title"].as_str().unwrap_or("Unknown");
        let artist = track["artist"]["name"].as_str().unwrap_or("Unknown");
        let id = track["id"].as_u64().unwrap_or(0);
        println!("  {:>2}. {} - {} (ID: {})", i + 1, artist, title, id);
    }
    println!("\nRun `deezco track <ID>` to download one.");
    Ok(false)
}

/// Download an artist from a URL or ID, or print search results for a name.
/// Returns `false` when nothing was downloaded (search results shown).
#[allow(clippy::too_many_arguments)]
async fn download_artist_query(
    api: &DeezerApi,
    query: &str,
    format: TrackFormat,
    output: &Path,
    limit: u32,
    pick: Option<usize>,
    output_format: Option<OutputFormat>,
    concurrency: usize,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "artist");
        download::download_artist(api, &id, format, output, concurrency).await?;
        return Ok(true);
    }

    // Search for the artist
    let results = api.search_artist(query, limit).await?;

    // Auto-download a specific result instead of printing
    if let Some(pick) = pick {
        let data = match results["data"].as_array() {
            Some(data) if !data.is_empty() => data,
            _ => {
                println!("No artists found for '{}'.", query);
                return Ok(false);
            }
        };
        if !(1..=data.len()).contains(&pick) {
            println!("No result #{pick} — found {} artist(s).", data.len());
            return Ok(false);
        }
        let artist = &data[pick - 1];
        let id = artist["id"].as_u64().unwrap_or(0).to_string();
        if id == "0" {
            println!("Result #{pick} has no artist ID.");
            return Ok(false);
        }
        download::download_artist(api, &id, format, output, concurrency).await?;
        return Ok(true);
    }

    // Print the raw API response as JSON for scripting
    if let Some(format) = output_format {
        print_json(&results, format)?;
        return Ok(false);
    }

    // Print search results so the user can pick an ID
    let data = match results["data"].as_array() {
        Some(data) if !data.is_empty() && limit > 0 => data,
        _ => {
            println!("No artists found for '{}'.", query);
            return Ok(false);
        }
    };
    println!("Artists matching '{query}':");
    for (i, artist) in data.iter().take(limit as usize).enumerate() {
        let name = artist["name"].as_str().unwrap_or("Unknown");
        let fans = artist["nb_fan"].as_u64().unwrap_or(0);
        let id = artist["id"].as_u64().unwrap_or(0);
        println!("  {:>2}. {} ({} fans, ID: {})", i + 1, name, fans, id);
    }
    println!("\nRun `deezco artist <ID>` to download the discography.");
    Ok(false)
}

/// Print a value as JSON in the requested style
fn print_json(value: &serde_json::Value, format: OutputFormat) -> Result<()> {
    let out = match format {
        OutputFormat::Pretty => serde_json::to_string_pretty(value)?,
        OutputFormat::Compact => serde_json::to_string(value)?,
    };
    println!("{out}");
    Ok(())
}

/// Print playlist contents as JSON instead of downloading
async fn print_playlist_json(api: &DeezerApi, id: &str, format: OutputFormat) -> Result<()> {
    let info = api.get_playlist_info(id).await?;
    let title = info["DATA"]["TITLE"].as_str().unwrap_or("Unknown Playlist");
    let tracks = api.get_playlist_tracks(id).await?;
    print_json(
        &json!({ "type": "playlist", "id": id, "title": title, "tracks": tracks }),
        format,
    )
}

/// Print album contents as JSON instead of downloading
async fn print_album_json(api: &DeezerApi, id: &str, format: OutputFormat) -> Result<()> {
    let info = api.get_album_info(id).await?;
    let title = info["ALB_TITLE"].as_str().unwrap_or("Unknown Album");
    let artist = info["ART_NAME"].as_str().unwrap_or("Unknown Artist");
    let tracks = api.get_album_tracks(id).await?;
    print_json(
        &json!({
            "type": "album",
            "id": id,
            "title": title,
            "artist": artist,
            "tracks": tracks,
        }),
        format,
    )
}

/// Print followed artists as JSON instead of downloading
async fn print_following_json(api: &DeezerApi, format: OutputFormat, user_id: u64) -> Result<()> {
    let artists = api.get_followed_artists(user_id).await?;
    print_json(&json!({ "type": "following", "artists": artists }), format)
}

/// Print favorite tracks as JSON instead of downloading.
/// Tracks are sorted by artist/title and paginated with `--offset`/`--limit`.
async fn print_favorites_json(
    api: &DeezerApi,
    format: OutputFormat,
    limit: Option<u32>,
    offset: u32,
) -> Result<()> {
    let ids = api.get_favorite_track_ids().await?;
    let mut tracks = Vec::new();
    for batch in ids.chunks(50) {
        tracks.extend(api.get_tracks_by_ids(batch).await?);
    }

    // Deterministic ordering so pagination pages are stable
    tracks.sort_by(|a, b| {
        a.artist()
            .to_lowercase()
            .cmp(&b.artist().to_lowercase())
            .then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase()))
            .then_with(|| a.id_str().cmp(&b.id_str()))
    });

    let tracks: Vec<_> = tracks
        .into_iter()
        .skip(offset as usize)
        .take(limit.map_or(usize::MAX, |l| l as usize))
        .collect();
    print_json(&json!({ "type": "favorites", "tracks": tracks }), format)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = parse_format(&cli.quality);
    let output = resolve_output_dir(cli.output.clone(), env_output_dir());

    // No command: print help and exit
    let Some(command) = cli.command else {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    // Keep stdout clean when emitting JSON for scripts
    let json_output = cli.json.then_some(cli.output_format);
    let search_limit = cli.limit.unwrap_or(10);
    if !matches!(command, Commands::Logout) && json_output.is_none() {
        print_banner();
    }

    let api = DeezerApi::new()?;

    // Login and prepare the output dir for every command except logout
    if !matches!(command, Commands::Logout) {
        let arl: Option<String> = cli.arl.clone().or_else(|| {
            std::env::var("DEEZCO_ARL")
                .ok()
                .filter(|value| !value.is_empty())
        });
        if !auth::login(&api, arl.as_deref()).await? {
            return Ok(());
        }

        let user = api.current_user.lock().await;
        if let Some(u) = user.as_ref()
            && json_output.is_none()
        {
            println!("Logged in as: {}\n", u.name);
        }

        tokio::fs::create_dir_all(&output).await?;
    }

    match command {
        Commands::Track { query } => {
            if !download_track_query(
                &api,
                &query,
                format,
                &output,
                search_limit,
                cli.pick,
                json_output,
            )
            .await?
            {
                return Ok(());
            }
        }
        Commands::Playlist { url } => {
            let id = extract_id(&url, "playlist");
            match json_output {
                Some(fmt) => print_playlist_json(&api, &id, fmt).await?,
                None => {
                    download::download_playlist(&api, &id, format, &output, cli.concurrency)
                        .await?;
                }
            }
        }
        Commands::Favorites => match json_output {
            Some(fmt) => print_favorites_json(&api, fmt, cli.limit, cli.offset).await?,
            None => {
                download::download_favorites(&api, format, &output, cli.concurrency).await?;
            }
        },
        Commands::Artist { query } => {
            if !download_artist_query(
                &api,
                &query,
                format,
                &output,
                search_limit,
                cli.pick,
                json_output,
                cli.concurrency,
            )
            .await?
            {
                return Ok(());
            }
        }
        Commands::Following => {
            let user_id = api
                .current_user
                .lock()
                .await
                .as_ref()
                .map_or(0, |u| u.id);
            match json_output {
                Some(fmt) => print_following_json(&api, fmt, user_id).await?,
                None => {
                    let artists = api.get_followed_artists(user_id).await?;
                    println!("Downloading releases from {} followed artist(s)\n", artists.len());
                    for artist in &artists {
                        println!("--- {} ---", artist.name);
                        download::download_artist(
                            &api,
                            &artist.id.to_string(),
                            format,
                            &output,
                            cli.concurrency,
                        )
                        .await?;
                    }
                }
            }
        }
        Commands::Album { url } => {
            let id = extract_id(&url, "album");
            match json_output {
                Some(fmt) => print_album_json(&api, &id, fmt).await?,
                None => {
                    download::download_album(&api, &id, format, &output, cli.concurrency).await?;
                }
            }
        }
        Commands::Logout => {
            auth::remove_arl().await?;
            println!("Logged out. Stored ARL removed.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_maps_quality_names_and_codes() {
        assert_eq!(parse_format("flac"), TrackFormat::Flac);
        assert_eq!(parse_format("FLAC"), TrackFormat::Flac);
        assert_eq!(parse_format("Lossless"), TrackFormat::Flac);
        assert_eq!(parse_format("9"), TrackFormat::Flac);

        assert_eq!(parse_format("320"), TrackFormat::Mp3_320);
        assert_eq!(parse_format("mp3_320"), TrackFormat::Mp3_320);
        assert_eq!(parse_format("MP3_320"), TrackFormat::Mp3_320);
        assert_eq!(parse_format("3"), TrackFormat::Mp3_320);

        assert_eq!(parse_format("128"), TrackFormat::Mp3_128);
        assert_eq!(parse_format("mp3_128"), TrackFormat::Mp3_128);
        assert_eq!(parse_format("1"), TrackFormat::Mp3_128);
    }

    #[test]
    fn parse_format_defaults_to_mp3_320() {
        assert_eq!(parse_format(""), TrackFormat::Mp3_320);
        assert_eq!(parse_format("wav"), TrackFormat::Mp3_320);
        // Whitespace is not trimmed
        assert_eq!(parse_format(" flac"), TrackFormat::Mp3_320);
    }

    #[test]
    fn extract_id_passes_through_plain_ids() {
        assert_eq!(extract_id("12345", "track"), "12345");
        assert_eq!(extract_id("", "track"), "");
    }

    #[test]
    fn extract_id_parses_deezer_urls() {
        assert_eq!(
            extract_id("https://www.deezer.com/en/track/12345", "track"),
            "12345"
        );
        assert_eq!(
            extract_id("https://www.deezer.com/en/playlist/67890", "playlist"),
            "67890"
        );
        assert_eq!(
            extract_id("https://www.deezer.com/artist/42", "artist"),
            "42"
        );
    }

    #[test]
    fn extract_id_strips_query_fragments_and_trailing_slash() {
        assert_eq!(
            extract_id(
                "https://www.deezer.com/en/track/12345?utm_source=share",
                "track"
            ),
            "12345"
        );
        assert_eq!(
            extract_id("https://www.deezer.com/en/track/12345/", "track"),
            "12345"
        );
        assert_eq!(
            extract_id("https://www.deezer.com/en/track/12345#fragment", "track"),
            "12345"
        );
    }

    #[test]
    fn resolve_output_dir_prefers_flag_then_env_then_default() {
        let flag = Some(PathBuf::from("/flag"));
        let env = Some(PathBuf::from("/env"));

        // CLI flag wins over everything
        assert_eq!(
            resolve_output_dir(flag.clone(), env.clone()),
            PathBuf::from("/flag")
        );
        // Env var is used when no flag is given
        assert_eq!(resolve_output_dir(None, env.clone()), PathBuf::from("/env"));
        // Defaults to the OS Downloads folder when neither is set
        assert_eq!(resolve_output_dir(None, None), default_output_dir());
    }
}
