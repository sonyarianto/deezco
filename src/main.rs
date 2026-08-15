mod api;
mod auth;
mod crypto;
mod download;
mod models;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::path::{Path, PathBuf};

use std::collections::HashMap;

use crate::api::DeezerApi;
use crate::models::{FollowedArtist, GwTrack, TrackFormat};

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

    /// Minimum acceptable quality: fail instead of falling back below it
    #[arg(long, global = true)]
    min_quality: Option<String>,

    /// Maximum quality to download: caps the bitrate, never exceeding it
    #[arg(long, global = true)]
    max_quality: Option<String>,

    /// Shorthand for --min-quality + --max-quality: download exactly this quality
    #[arg(long, global = true)]
    exact: Option<String>,

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

    /// List what would be downloaded without touching disk
    #[arg(long, global = true)]
    dry_run: bool,

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

/// Resolve the quality bounds: `--exact` sets both min and max (overriding
/// individual flags), otherwise the individual flags pass through as given.
fn resolve_quality_bounds(
    exact: Option<TrackFormat>,
    min: Option<TrackFormat>,
    max: Option<TrackFormat>,
) -> (Option<TrackFormat>, Option<TrackFormat>) {
    match exact {
        Some(e) => (Some(e), Some(e)),
        None => (min, max),
    }
}

/// The best format actually available for a track (None when no filesize data)
fn best_available_format(track: &GwTrack) -> Option<TrackFormat> {
    [TrackFormat::Flac, TrackFormat::Mp3_320, TrackFormat::Mp3_128]
        .into_iter()
        .find(|&fmt| track.filesize_for_format(fmt) > 0)
}

/// Rank of the highest quality format available for a track (3=FLAC … 0=none)
fn quality_rank(track: &GwTrack) -> u8 {
    best_available_format(track).map_or(0, TrackFormat::rank)
}

/// Result indices sorted by highest available quality. The sort is stable, so
/// relevance order is kept within equal quality; results without a track ID
/// (or missing from `by_id`) sort last.
fn sort_by_quality(data: &[serde_json::Value], by_id: &HashMap<String, GwTrack>) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..data.len()).collect();
    indices.sort_by_key(|&i| {
        let rank = data[i]["id"]
            .as_u64()
            .and_then(|id| by_id.get(&id.to_string()))
            .map(quality_rank)
            .unwrap_or(0);
        std::cmp::Reverse(rank)
    });
    indices
}

/// Reorder `results["data"]` by highest available quality (stable).
fn sort_results_by_quality(results: &mut serde_json::Value, by_id: &HashMap<String, GwTrack>) {
    if let Some(data) = results["data"].as_array_mut() {
        let indices = sort_by_quality(data, by_id);
        let sorted: Vec<serde_json::Value> =
            indices.into_iter().map(|i| data[i].clone()).collect();
        *data = sorted;
    }
}

/// Fetch full track data (with filesizes) for the search results in one
/// batched call, then return the quality-sorted indices and the track map.
async fn quality_sorted_indices(
    api: &DeezerApi,
    data: &[serde_json::Value],
) -> Result<(Vec<usize>, HashMap<String, GwTrack>)> {
    let ids: Vec<String> = data
        .iter()
        .filter_map(|track| track["id"].as_u64())
        .map(|id| id.to_string())
        .collect();

    let mut by_id: HashMap<String, GwTrack> = HashMap::new();
    if !ids.is_empty() {
        for track in api.get_tracks_by_ids(&ids).await? {
            by_id.insert(track.id_str(), track);
        }
    }

    Ok((sort_by_quality(data, &by_id), by_id))
}

/// Download a track from a URL or ID, or print search results for a name.
/// Returns `false` when nothing was downloaded (search results shown).
#[allow(clippy::too_many_arguments)]
async fn download_track_query(
    api: &DeezerApi,
    query: &str,
    format: TrackFormat,
    output: &Path,
    limit: u32,
    pick: Option<usize>,
    output_format: Option<OutputFormat>,
    dry_run: bool,
    min_format: Option<TrackFormat>,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "track");
        download::download_single_track(api, &id, format, output, dry_run, min_format).await?;
        return Ok(true);
    }

    // Search for the track
    let mut results = api.search_track(query, limit).await?;

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
        let (indices, _) = quality_sorted_indices(api, data).await?;
        let track = &data[indices[pick - 1]];
        let id = track["id"].as_u64().unwrap_or(0).to_string();
        if id == "0" {
            println!("Result #{pick} has no track ID.");
            return Ok(false);
        }
        download::download_single_track(api, &id, format, output, dry_run, min_format).await?;
        return Ok(true);
    }

    // Print the raw API response as JSON for scripting, with the data array
    // reordered by highest available quality to match the human-readable list
    if let Some(format) = output_format {
        let data = results["data"].as_array().cloned().unwrap_or_default();
        let (_, by_id) = quality_sorted_indices(api, &data).await?;
        sort_results_by_quality(&mut results, &by_id);
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
    let limited = &data[..data.len().min(limit as usize)];

    // Fetch full track data (with filesizes) in one batched call, sort by
    // highest available quality (stable, so relevance order holds within
    // equal quality), and annotate each result with its format.
    let (indices, by_id) = quality_sorted_indices(api, limited).await?;

    println!("Tracks matching '{query}':");
    for (position, &i) in indices.iter().enumerate() {
        let track = &limited[i];
        let title = track["title"].as_str().unwrap_or("Unknown");
        let artist = track["artist"]["name"].as_str().unwrap_or("Unknown");
        let id = track["id"].as_u64().unwrap_or(0);
        let format = by_id
            .get(&id.to_string())
            .map(|full| download::format_annotation(full, format))
            .unwrap_or_default();
        println!(
            "  {:>2}. {} - {} {} (ID: {})",
            position + 1, artist, title, format, id
        );
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
    dry_run: bool,
    min_format: Option<TrackFormat>,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "artist");
        download::download_artist(api, &id, format, output, concurrency, dry_run, min_format)
            .await?;
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
        download::download_artist(api, &id, format, output, concurrency, dry_run, min_format)
            .await?;
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
fn print_following_json(format: OutputFormat, artists: Vec<FollowedArtist>) -> Result<()> {
    print_json(&json!({ "type": "following", "artists": artists }), format)
}

/// Header line for a followed artist, including their release count
fn artist_header(artist: &FollowedArtist) -> String {
    let noun = if artist.nb_album == 1 { "album" } else { "albums" };
    format!("--- {} ({} {}) ---", artist.name, artist.nb_album, noun)
}

/// Best quality available across an artist's discography, derived from the
/// filesizes of every album's tracks (None when unknown).
async fn artist_best_quality(api: &DeezerApi, art_id: &str) -> Result<Option<TrackFormat>> {
    let albums = api.get_artist_discography(art_id).await?;
    let mut best: Option<TrackFormat> = None;
    for album in &albums {
        let tracks = api.get_album_tracks(&album.id_str()).await?;
        for track in &tracks {
            if let Some(fmt) = best_available_format(track) {
                let better = match best {
                    Some(current) => fmt.rank() > current.rank(),
                    None => true,
                };
                if better {
                    best = Some(fmt);
                }
            }
        }
    }
    Ok(best)
}

/// Fetch the best available quality for every followed artist, concurrently.
/// Artists whose quality can't be determined keep `best_quality` unset.
async fn enrich_followed_artists(
    api: &DeezerApi,
    artists: Vec<FollowedArtist>,
) -> Result<Vec<FollowedArtist>> {
    let futures = artists.into_iter().map(|mut artist| {
        let api = api.clone();
        async move {
            let best = artist_best_quality(&api, &artist.id.to_string())
                .await
                .ok()
                .flatten();
            artist.best_quality = best.map(|fmt| fmt.api_name().to_string());
            artist
        }
    });
    Ok(stream::iter(futures).buffer_unordered(4).collect().await)
}

/// Sort followed artists by best available quality (stable; the followed-list
/// order is kept within equal quality, unknown quality sorts last).
fn sort_followed_by_quality(artists: &mut [FollowedArtist]) {
    artists.sort_by_key(|artist| {
        let rank = artist
            .best_quality
            .as_deref()
            .map_or(0, |quality| parse_format(quality).rank());
        std::cmp::Reverse(rank)
    });
}

/// Sort tracks for favorites JSON: highest available quality first, then
/// artist/title/id. The tiebreakers are deterministic so pagination pages
/// stay stable across calls.
fn sort_favorites(tracks: &mut [GwTrack]) {
    tracks.sort_by(|a, b| {
        quality_rank(b)
            .cmp(&quality_rank(a))
            .then_with(|| a.artist().to_lowercase().cmp(&b.artist().to_lowercase()))
            .then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase()))
            .then_with(|| a.id_str().cmp(&b.id_str()))
    });
}

/// Print favorite tracks as JSON instead of downloading.
/// Tracks are sorted by quality (then artist/title) and paginated with
/// `--offset`/`--limit`.
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

    sort_favorites(&mut tracks);

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
    let mut format = parse_format(&cli.quality);
    let (min_format, max_format) = resolve_quality_bounds(
        cli.exact.as_deref().map(parse_format),
        cli.min_quality.as_deref().map(parse_format),
        cli.max_quality.as_deref().map(parse_format),
    );

    // An unsatisfiable floor: every download would fail
    if let (Some(min), Some(max)) = (min_format, max_format)
        && min.rank() > max.rank()
    {
        anyhow::bail!(
            "--min-quality {} is higher than --max-quality {}; no quality can satisfy both",
            min,
            max
        );
    }

    // Cap the effective quality once; the fallback chain only goes downward,
    // so the cap propagates through downloads, dry-run, and annotations.
    if let Some(max) = max_format {
        format = format.capped_by(max);
    }

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
                cli.dry_run,
                min_format,
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
                    download::download_playlist(
                        &api,
                        &id,
                        format,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                        min_format,
                    )
                    .await?;
                }
            }
        }
        Commands::Favorites => match json_output {
            Some(fmt) => print_favorites_json(&api, fmt, cli.limit, cli.offset).await?,
            None => {
                download::download_favorites(
                    &api,
                    format,
                    &output,
                    cli.concurrency,
                    cli.dry_run,
                    min_format,
                )
                .await?;
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
                cli.dry_run,
                min_format,
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
            let artists = api.get_followed_artists(user_id).await?;

            // --pick and --json share the quality-sorted order; plain
            // downloads keep the followed-list order with no extra fetches
            if cli.pick.is_some() || json_output.is_some() {
                let mut enriched = enrich_followed_artists(&api, artists).await?;
                sort_followed_by_quality(&mut enriched);

                // --pick: download a single followed artist's releases
                if let Some(pick) = cli.pick {
                    if !(1..=enriched.len()).contains(&pick) {
                        println!(
                            "No result #{pick} — found {} followed artist(s).",
                            enriched.len()
                        );
                        return Ok(());
                    }
                    let artist = &enriched[pick - 1];
                    let art_id = artist.id.to_string();
                    println!("{}", artist_header(artist));
                    if download::is_artist_downloaded(&api, &art_id, &output).await? {
                        println!("  [skip] Already on disk");
                        return Ok(());
                    }
                    download::download_artist(
                        &api,
                        &art_id,
                        format,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                        min_format,
                    )
                    .await?;
                    return Ok(());
                }

                if let Some(fmt) = json_output {
                    print_following_json(fmt, enriched)?;
                    return Ok(());
                }
            } else {
                println!(
                    "Downloading releases from {} followed artist(s)\n",
                    artists.len()
                );
                for artist in &artists {
                    println!("{}", artist_header(artist));
                    let art_id = artist.id.to_string();
                    if download::is_artist_downloaded(&api, &art_id, &output).await? {
                        println!("  [skip] Already on disk");
                        continue;
                    }
                    download::download_artist(
                        &api,
                        &art_id,
                        format,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                        min_format,
                    )
                    .await?;
                }
            }
        }
        Commands::Album { url } => {
            let id = extract_id(&url, "album");
            match json_output {
                Some(fmt) => print_album_json(&api, &id, fmt).await?,
                None => {
                    download::download_album(
                        &api,
                        &id,
                        format,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                        min_format,
                    )
                    .await?;
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

    /// A track with the given reported filesizes (0 = unavailable)
    fn track_with_filesizes(flac: u64, mp3_320: u64, mp3_128: u64) -> GwTrack {
        GwTrack {
            sng_id: serde_json::json!(1),
            sng_title: Some("Test".to_string()),
            md5_origin: Some("md5".to_string()),
            media_version: Some(serde_json::json!(1)),
            art_name: Some("Artist".to_string()),
            art_id: Some(serde_json::json!(1)),
            track_token: None,
            filesize_mp3_128: Some(serde_json::json!(mp3_128)),
            filesize_mp3_320: Some(serde_json::json!(mp3_320)),
            filesize_flac: Some(serde_json::json!(flac)),
        }
    }

    #[test]
    fn quality_rank_orders_formats() {
        let flac = track_with_filesizes(100, 100, 100);
        let mp3_320 = track_with_filesizes(0, 100, 100);
        let mp3_128 = track_with_filesizes(0, 0, 100);
        let none = track_with_filesizes(0, 0, 0);

        assert_eq!(quality_rank(&flac), 3);
        assert_eq!(quality_rank(&mp3_320), 2);
        assert_eq!(quality_rank(&mp3_128), 1);
        assert_eq!(quality_rank(&none), 0);
    }

    #[test]
    fn sort_by_quality_orders_by_rank_stably() {
        let by_id = HashMap::from([
            ("1".to_string(), track_with_filesizes(0, 0, 100)),
            ("2".to_string(), track_with_filesizes(100, 100, 100)),
            ("3".to_string(), track_with_filesizes(0, 100, 100)),
            ("4".to_string(), track_with_filesizes(100, 100, 100)),
        ]);
        let data: Vec<serde_json::Value> = (1..=4)
            .map(|i| serde_json::json!({ "id": i, "title": format!("t{i}") }))
            .collect();

        // 2 and 4 both rank 3 (FLAC): their original order (2 before 4) is kept,
        // then rank 2 (id 3), then rank 1 (id 1)
        assert_eq!(sort_by_quality(&data, &by_id), vec![1, 3, 2, 0]);
    }

    #[test]
    fn resolve_quality_bounds_exact_sets_both_and_overrides() {
        // --exact sets both bounds
        assert_eq!(
            resolve_quality_bounds(Some(TrackFormat::Flac), None, None),
            (Some(TrackFormat::Flac), Some(TrackFormat::Flac))
        );
        // Individual flags pass through when no exact is given
        assert_eq!(
            resolve_quality_bounds(
                None,
                Some(TrackFormat::Mp3_320),
                Some(TrackFormat::Mp3_128)
            ),
            (Some(TrackFormat::Mp3_320), Some(TrackFormat::Mp3_128))
        );
        // Exact overrides conflicting individual flags
        assert_eq!(
            resolve_quality_bounds(
                Some(TrackFormat::Flac),
                Some(TrackFormat::Mp3_320),
                Some(TrackFormat::Mp3_128)
            ),
            (Some(TrackFormat::Flac), Some(TrackFormat::Flac))
        );
    }

    #[test]
    fn sort_followed_by_quality_orders_best_first_stably() {
        let mut artists = vec![
            FollowedArtist { id: 1, name: "Low".to_string(), nb_album: 2, best_quality: Some("MP3_128".to_string()) },
            FollowedArtist { id: 2, name: "High".to_string(), nb_album: 2, best_quality: Some("FLAC".to_string()) },
            FollowedArtist { id: 3, name: "Unknown".to_string(), nb_album: 2, best_quality: None },
            FollowedArtist { id: 4, name: "Mid".to_string(), nb_album: 2, best_quality: Some("MP3_320".to_string()) },
            FollowedArtist { id: 5, name: "High2".to_string(), nb_album: 2, best_quality: Some("FLAC".to_string()) },
        ];

        sort_followed_by_quality(&mut artists);

        let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
        // FLAC pair keeps original order (2 before 5), then 320, 128, unknown
        assert_eq!(names, vec!["High", "High2", "Mid", "Low", "Unknown"]);
    }

    #[test]
    fn sort_favorites_orders_by_quality_then_artist_title() {
        let high_b = GwTrack {
            art_name: Some("B Artist".to_string()),
            sng_title: Some("b-title".to_string()),
            sng_id: serde_json::json!(2),
            ..track_with_filesizes(100, 100, 100)
        };
        let high_a = GwTrack {
            art_name: Some("A Artist".to_string()),
            sng_title: Some("a-title".to_string()),
            sng_id: serde_json::json!(1),
            ..track_with_filesizes(100, 100, 100)
        };
        let low_a = GwTrack {
            art_name: Some("A Artist".to_string()),
            sng_title: Some("a-title".to_string()),
            sng_id: serde_json::json!(3),
            ..track_with_filesizes(0, 0, 100)
        };
        let mut tracks = vec![low_a, high_b, high_a];

        sort_favorites(&mut tracks);

        // FLAC-available tracks first (by artist/title), then MP3_128-only
        let ids: Vec<String> = tracks.iter().map(|t| t.id_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"]);
    }

    #[test]
    fn sort_results_by_quality_reorders_the_data_array() {
        let by_id = HashMap::from([
            ("1".to_string(), track_with_filesizes(0, 0, 100)),
            ("2".to_string(), track_with_filesizes(100, 100, 100)),
            ("3".to_string(), track_with_filesizes(0, 100, 100)),
        ]);
        let mut results = serde_json::json!({
            "data": [
                { "id": 1, "title": "low" },
                { "id": 2, "title": "high" },
                { "id": 3, "title": "mid" },
            ],
            "total": 3,
            "next": "https://example.com/next",
        });

        sort_results_by_quality(&mut results, &by_id);

        let titles: Vec<&str> = results["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, vec!["high", "mid", "low"]);
        // Envelope fields are untouched
        assert_eq!(results["total"], serde_json::json!(3));
        assert_eq!(
            results["next"],
            serde_json::json!("https://example.com/next")
        );
    }

    #[test]
    fn sort_by_quality_puts_missing_ids_last() {
        let by_id = HashMap::from([("2".to_string(), track_with_filesizes(0, 100, 100))]);
        let data: Vec<serde_json::Value> = (1..=3)
            .map(|i| serde_json::json!({ "id": i, "title": format!("t{i}") }))
            .collect();

        // Only id 2 has data (rank 2); ids 1 and 3 rank 0 and keep their order
        assert_eq!(sort_by_quality(&data, &by_id), vec![1, 0, 2]);
    }

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
