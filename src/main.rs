mod api;
mod auth;
mod crypto;
mod download;
mod icecast;
mod models;
mod queue;
mod serve;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::path::{Path, PathBuf};

use std::collections::HashMap;

use crate::api::DeezerApi;
use crate::models::{FollowedArtist, GwTrack, TrackFormat};

#[derive(Parser)]
#[command(name = "deezco", version, about = "Deezer music downloader.")]
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

    /// Sort key for search and listing results
    #[arg(long, global = true, value_enum, default_value_t = SortKey::Quality)]
    sort: SortKey,

    /// Sort direction; defaults to each key's natural order
    /// (quality: best first, duration: shortest first)
    #[arg(long, global = true, value_enum)]
    sort_dir: Option<SortDir>,

    /// Download 30-second previews instead of full tracks
    #[arg(long, global = true)]
    preview: bool,

    /// Download both the 30-second preview and the full track (implies --preview)
    #[arg(long, global = true)]
    preview_and_full: bool,

    /// Print the effective defaults for every setting, then exit without
    /// logging in or touching the network
    #[arg(long, global = true)]
    show_defaults: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Human-friendly multi-line JSON
    Pretty,
    /// Single-line JSON, ideal for piping
    Compact,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SortKey {
    /// Highest available quality first
    Quality,
    /// Original API order
    Relevance,
    /// Shortest duration first
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SortDir {
    /// Lowest quality / shortest duration first
    Asc,
    /// Highest quality / longest duration first
    Desc,
}

/// The Stream variant carries many small CLI fields, making it larger than
/// the other variants.
#[allow(clippy::large_enum_variant)]
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
    /// Serve playlists as HTTP audio to an external service consumer
    Serve {
        /// Address to bind the HTTP server to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind the HTTP server to
        #[arg(long, default_value_t = 9001)]
        port: u16,
        /// How often to refetch playlists so web edits are picked up
        #[arg(long, default_value_t = 300)]
        refresh_secs: u64,
    },
    /// Stream a playlist to an Icecast server as a live radio source
    Stream {
        /// Deezer playlist URL or playlist ID
        playlist: String,
        /// Icecast server base URL, e.g. http://localhost:8000
        #[arg(long)]
        server: String,
        /// Icecast mount point, e.g. /radio
        #[arg(long)]
        mount: String,
        /// Icecast source username
        #[arg(long, default_value = "source")]
        username: String,
        /// Icecast source password (or set DEEZCO_ICECAST_PASSWORD)
        #[arg(long)]
        password: Option<String>,
        /// Stream name shown to listeners
        #[arg(long)]
        name: Option<String>,
        /// Stream genre shown to listeners
        #[arg(long)]
        genre: Option<String>,
        /// Station website URL
        #[arg(long)]
        url: Option<String>,
        /// Advertise the stream in public directories
        #[arg(long)]
        public: bool,
        /// Target stream bitrate in kbps (e.g. 96). Transcodes via ffmpeg
        /// when set; 128 and 320 stream natively without transcoding
        #[arg(long)]
        bitrate: Option<u32>,
        /// Path to the Thimeo Stereo Tool CLI binary (stereo_tool_cmd_64).
        /// When set, every track runs through it (decode -> process -> encode)
        #[arg(long)]
        stereo_tool: Option<PathBuf>,
        /// Stereo Tool processor settings file (.sts); defaults to audio.sts
        /// next to the binary when present
        #[arg(long)]
        sts: Option<PathBuf>,
        /// Stereo Tool license key (visible in `ps aux` while running)
        #[arg(long)]
        stereo_tool_key: Option<String>,
        /// Sample rate (Hz) of the Stereo Tool processing bus
        #[arg(long, default_value_t = 44100)]
        stereo_rate: u32,
        /// How often to refetch the playlist so web edits are picked up
        #[arg(long, default_value_t = 300)]
        refresh_secs: u64,
    },
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
    [
        TrackFormat::Flac,
        TrackFormat::Mp3_320,
        TrackFormat::Mp3_128,
    ]
    .into_iter()
    .find(|&fmt| track.filesize_for_format(fmt) > 0)
}

/// Rank of the highest quality format available for a track (3=FLAC … 0=none)
fn quality_rank(track: &GwTrack) -> u8 {
    best_available_format(track).map_or(0, TrackFormat::rank)
}

/// Effective sort direction: an explicit `--sort-dir` wins, otherwise the
/// key's natural direction (quality best-first, duration shortest-first).
fn direction(sort_dir: Option<SortDir>, natural_desc: bool) -> bool {
    match sort_dir {
        Some(SortDir::Desc) => true,
        Some(SortDir::Asc) => false,
        None => natural_desc,
    }
}

/// Print the effective defaults for every setting. Runs before any login or
/// network access, so `deezco --show-defaults` works offline and never
/// touches disk. With `--json` the same values are emitted as JSON instead.
fn print_defaults(
    cli: &Cli,
    format: TrackFormat,
    min_format: Option<TrackFormat>,
    max_format: Option<TrackFormat>,
    options: download::DownloadOptions,
    output: &Path,
    sort: SortKey,
) -> Result<()> {
    let fmt_opt = |opt: Option<TrackFormat>| match opt {
        Some(fmt) => fmt.to_string(),
        None => "none".to_string(),
    };
    let preview = if options.preview_and_full {
        "preview_and_full"
    } else if options.preview {
        "preview"
    } else {
        "full"
    };
    let preview_label = match preview {
        "preview_and_full" => "preview + full (--preview-and-full)",
        "preview" => "preview only (--preview)",
        _ => "full track only",
    };
    let sort_key = match sort {
        SortKey::Quality => "quality",
        SortKey::Relevance => "relevance",
        SortKey::Duration => "duration",
    };
    let (sort_direction, sort_dir_label) = match sort {
        SortKey::Quality => {
            let desc = direction(cli.sort_dir, true);
            (
                Some(if desc { "desc" } else { "asc" }),
                if desc {
                    "best first (desc)"
                } else {
                    "lowest first (asc)"
                },
            )
        }
        SortKey::Duration => {
            let desc = direction(cli.sort_dir, false);
            (
                Some(if desc { "desc" } else { "asc" }),
                if desc {
                    "longest first (desc)"
                } else {
                    "shortest first (asc)"
                },
            )
        }
        SortKey::Relevance => (None, "API order (--sort-dir has no effect)"),
    };
    let output_source = if cli.output.is_some() {
        "flag"
    } else if env_output_dir().is_some() {
        "env"
    } else {
        "default"
    };
    let output_source_label = match output_source {
        "flag" => "(--output)",
        "env" => "(DEEZCO_OUTPUT_DIR)",
        _ => "(default)",
    };
    let arl_source = if cli.arl.is_some() {
        "flag"
    } else if std::env::var("DEEZCO_ARL")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some()
    {
        "env"
    } else if auth::config_dir().join(".arl").exists() {
        "stored"
    } else {
        "none"
    };
    let arl_source_label = match arl_source {
        "flag" => "--arl flag",
        "env" => "DEEZCO_ARL environment variable",
        "stored" => "stored login",
        _ => "none (will prompt interactively)",
    };
    let json_output = if cli.json {
        match cli.output_format {
            OutputFormat::Pretty => "pretty",
            OutputFormat::Compact => "compact",
        }
    } else {
        "off"
    };
    let json_label = match json_output {
        "pretty" => "pretty JSON (--json)",
        "compact" => "compact JSON (--json --output-format compact)",
        _ => "off (human-readable output)",
    };

    let value = json!({
        "requested_quality": cli.quality,
        "min_quality": min_format.map(|fmt| fmt.to_string()),
        "max_quality": max_format.map(|fmt| fmt.to_string()),
        "exact_quality": cli.exact.as_deref().map(parse_format).map(|fmt| fmt.to_string()),
        "effective_quality": format.to_string(),
        "preview": preview,
        "sort_key": sort_key,
        "sort_direction": sort_direction,
        "output": output.display().to_string(),
        "output_source": output_source,
        "concurrency": cli.concurrency,
        "dry_run": cli.dry_run,
        "search_limit": cli.limit.unwrap_or(10),
        "json_output": json_output,
        "arl_source": arl_source,
    });

    if cli.json {
        print_json(&value, cli.output_format)?;
        return Ok(());
    }

    println!("Effective defaults (no login or network needed):");
    println!(
        "  Requested quality:  {} (-q {})",
        parse_format(&cli.quality),
        cli.quality
    );
    println!("  Min quality:        {}", fmt_opt(min_format));
    println!("  Max quality:        {}", fmt_opt(max_format));
    if let Some(exact) = cli.exact.as_deref().map(parse_format) {
        println!("  Exact quality:      {} (--exact)", exact);
    }
    println!("  Effective quality:  {}", format);
    println!("  Preview:            {}", preview_label);
    println!("  Sort key:           {}", sort_key);
    println!("  Sort direction:     {}", sort_dir_label);
    println!(
        "  Output dir:         {} {}",
        output.display(),
        output_source_label
    );
    println!("  Concurrency:        {}", cli.concurrency);
    println!(
        "  Dry run:            {}",
        if cli.dry_run { "on" } else { "off" }
    );
    println!(
        "  Search limit:       {} (--limit)",
        cli.limit.unwrap_or(10)
    );
    println!("  JSON output:        {}", json_label);
    println!("  ARL:                {}", arl_source_label);
    Ok(())
}

/// Result indices sorted by available quality (stable, so relevance order is
/// kept within equal quality; results without a track ID or missing from
/// `by_id` sort last). Descending = best quality first.
fn sort_by_quality(
    data: &[serde_json::Value],
    by_id: &HashMap<String, GwTrack>,
    desc: bool,
) -> Vec<usize> {
    let rank = |i: usize| {
        data[i]["id"]
            .as_u64()
            .and_then(|id| by_id.get(&id.to_string()))
            .map(quality_rank)
            .unwrap_or(0)
    };
    let mut indices: Vec<usize> = (0..data.len()).collect();
    indices.sort_by(|&a, &b| {
        let (ra, rb) = (rank(a), rank(b));
        if desc { rb.cmp(&ra) } else { ra.cmp(&rb) }
    });
    indices
}

/// Reorder `results["data"]` by the given indices (stable, so the source
/// order is kept for equal sort keys).
fn reorder_results_data(results: &mut serde_json::Value, indices: Vec<usize>) {
    if let Some(data) = results["data"].as_array_mut() {
        let sorted: Vec<serde_json::Value> = indices.into_iter().map(|i| data[i].clone()).collect();
        *data = sorted;
    }
}

/// Order two durations (None = unknown, which sorts last in either direction).
fn duration_ordering(da: Option<u64>, db: Option<u64>, desc: bool) -> std::cmp::Ordering {
    match (da, db) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => {
            if desc {
                y.cmp(&x)
            } else {
                x.cmp(&y)
            }
        }
    }
}

/// Indices sorted by the search response's `duration` field. Tracks without a
/// duration sort last regardless of direction.
fn duration_sorted_indices(data: &[serde_json::Value], desc: bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..data.len()).collect();
    indices.sort_by(|&a, &b| {
        duration_ordering(
            data[a]["duration"].as_u64(),
            data[b]["duration"].as_u64(),
            desc,
        )
    });
    indices
}

/// Format seconds as `m:ss` (or `h:mm:ss` for an hour or more)
fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Fetch full track data (with filesizes) for the search results in one
/// batched call, then return the quality-sorted indices and the track map.
async fn quality_sorted_indices(
    api: &DeezerApi,
    data: &[serde_json::Value],
    desc: bool,
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

    Ok((sort_by_quality(data, &by_id, desc), by_id))
}

/// Download a track from a URL or ID, or print search results for a name.
/// Returns `false` when nothing was downloaded (search results shown).
#[allow(clippy::too_many_arguments)]
async fn download_track_query(
    api: &DeezerApi,
    query: &str,
    options: download::DownloadOptions,
    output: &Path,
    limit: u32,
    pick: Option<usize>,
    output_format: Option<OutputFormat>,
    dry_run: bool,
    sort: SortKey,
    sort_dir: Option<SortDir>,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "track");
        download::download_single_track(api, &id, options, output, dry_run).await?;
        return Ok(true);
    }

    // Search for the track
    let mut results = api.search_track(query, limit).await?;
    let quality_desc = direction(sort_dir, true);
    let duration_desc = direction(sort_dir, false);

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
        let index = match sort {
            SortKey::Relevance => pick - 1,
            SortKey::Duration => duration_sorted_indices(data, duration_desc)[pick - 1],
            SortKey::Quality => quality_sorted_indices(api, data, quality_desc).await?.0[pick - 1],
        };
        let track = &data[index];
        let id = track["id"].as_u64().unwrap_or(0).to_string();
        if id == "0" {
            println!("Result #{pick} has no track ID.");
            return Ok(false);
        }
        download::download_single_track(api, &id, options, output, dry_run).await?;
        return Ok(true);
    }

    // Print the raw API response as JSON for scripting, reordered by the
    // selected sort key to match the human-readable list
    if let Some(format) = output_format {
        match sort {
            SortKey::Relevance => {} // raw order; no extra fetch
            SortKey::Duration => {
                let data = results["data"].as_array().cloned().unwrap_or_default();
                let indices = duration_sorted_indices(&data, duration_desc);
                reorder_results_data(&mut results, indices);
            }
            SortKey::Quality => {
                let data = results["data"].as_array().cloned().unwrap_or_default();
                let (indices, _) = quality_sorted_indices(api, &data, quality_desc).await?;
                reorder_results_data(&mut results, indices);
            }
        }
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

    // In preview mode there's no format to annotate and no filesizes needed.
    let ids: Vec<String> = if options.preview {
        Vec::new()
    } else {
        limited
            .iter()
            .filter_map(|track| track["id"].as_u64())
            .map(|id| id.to_string())
            .collect()
    };
    let mut by_id: HashMap<String, GwTrack> = HashMap::new();
    if !ids.is_empty() {
        for track in api.get_tracks_by_ids(&ids).await? {
            by_id.insert(track.id_str(), track);
        }
    }
    let indices: Vec<usize> = match sort {
        SortKey::Relevance => (0..limited.len()).collect(),
        SortKey::Duration => duration_sorted_indices(limited, duration_desc),
        SortKey::Quality => sort_by_quality(limited, &by_id, quality_desc),
    };

    println!("Tracks matching '{query}':");
    for (position, &i) in indices.iter().enumerate() {
        let track = &limited[i];
        let title = track["title"].as_str().unwrap_or("Unknown");
        let artist = track["artist"]["name"].as_str().unwrap_or("Unknown");
        let id = track["id"].as_u64().unwrap_or(0);
        let format = if options.preview {
            if options.preview_and_full {
                "[preview + full]".to_string()
            } else {
                "[preview]".to_string()
            }
        } else {
            by_id
                .get(&id.to_string())
                .map(|full| download::format_annotation(full, options.format))
                .unwrap_or_default()
        };
        let duration = track["duration"].as_u64().map(format_duration);
        let suffix = match duration {
            Some(d) => format!("(ID: {}, {})", id, d),
            None => format!("(ID: {})", id),
        };
        println!(
            "  {:>2}. {} - {} {} {}",
            position + 1,
            artist,
            title,
            format,
            suffix
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
    options: download::DownloadOptions,
    output: &Path,
    limit: u32,
    pick: Option<usize>,
    output_format: Option<OutputFormat>,
    concurrency: usize,
    dry_run: bool,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "artist");
        download::download_artist(api, &id, options, output, concurrency, dry_run).await?;
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
        download::download_artist(api, &id, options, output, concurrency, dry_run).await?;
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

/// Header line for a followed artist, including their release count and,
/// when known, their best available quality.
fn artist_header(artist: &FollowedArtist) -> String {
    let noun = if artist.nb_album == 1 {
        "album"
    } else {
        "albums"
    };
    match &artist.best_quality {
        Some(quality) => format!(
            "--- {} ({} {}, best {}) ---",
            artist.name, artist.nb_album, noun, quality
        ),
        None => format!("--- {} ({} {}) ---", artist.name, artist.nb_album, noun),
    }
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
/// Descending = best quality first.
fn sort_followed_by_quality(artists: &mut [FollowedArtist], desc: bool) {
    let rank = |artist: &FollowedArtist| {
        artist
            .best_quality
            .as_deref()
            .map_or(0, |quality| parse_format(quality).rank())
    };
    artists.sort_by(|a, b| {
        let (ra, rb) = (rank(a), rank(b));
        if desc { rb.cmp(&ra) } else { ra.cmp(&rb) }
    });
}

/// Sort tracks for favorites JSON by the selected key and direction. The
/// artist/title/id tiebreakers are deterministic so pagination pages stay
/// stable across calls.
fn sort_favorites(tracks: &mut [GwTrack], key: SortKey, sort_dir: Option<SortDir>) {
    let quality_desc = direction(sort_dir, true);
    let duration_desc = direction(sort_dir, false);
    let tiebreakers = |a: &GwTrack, b: &GwTrack| {
        a.artist()
            .to_lowercase()
            .cmp(&b.artist().to_lowercase())
            .then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase()))
            .then_with(|| a.id_str().cmp(&b.id_str()))
    };
    match key {
        SortKey::Relevance => {} // raw liked order
        SortKey::Duration => {
            let known = |t: &GwTrack| (t.duration_secs() != 0).then_some(t.duration_secs());
            tracks.sort_by(|a, b| {
                duration_ordering(known(a), known(b), duration_desc).then_with(|| tiebreakers(a, b))
            });
        }
        SortKey::Quality => {
            tracks.sort_by(|a, b| {
                let (ra, rb) = (quality_rank(a), quality_rank(b));
                let ord = if quality_desc {
                    rb.cmp(&ra)
                } else {
                    ra.cmp(&rb)
                };
                ord.then_with(|| tiebreakers(a, b))
            });
        }
    }
}

/// Serialize tracks with a normalized numeric `duration` (seconds) added,
/// matching the search JSON's field name, alongside the raw GW fields.
fn with_duration(tracks: Vec<GwTrack>) -> Vec<serde_json::Value> {
    tracks
        .into_iter()
        .map(|track| {
            let duration = track.duration_secs();
            let mut value = serde_json::to_value(&track).unwrap_or_default();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("duration".to_string(), json!(duration));
            }
            value
        })
        .collect()
}

/// Print favorite tracks as JSON instead of downloading.
/// Tracks are sorted by the selected key and direction (then artist/title),
/// carry a normalized `duration` field, and are paginated with
/// `--offset`/`--limit`.
async fn print_favorites_json(
    api: &DeezerApi,
    format: OutputFormat,
    limit: Option<u32>,
    offset: u32,
    sort: SortKey,
    sort_dir: Option<SortDir>,
) -> Result<()> {
    let ids = api.get_favorite_track_ids().await?;
    let mut tracks = Vec::new();
    for batch in ids.chunks(50) {
        tracks.extend(api.get_tracks_by_ids(batch).await?);
    }

    sort_favorites(&mut tracks, sort, sort_dir);

    let tracks: Vec<_> = tracks
        .into_iter()
        .skip(offset as usize)
        .take(limit.map_or(usize::MAX, |l| l as usize))
        .collect();
    let tracks = with_duration(tracks);
    print_json(&json!({ "type": "favorites", "tracks": tracks }), format)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut format = parse_format(&cli.quality);
    let sort = cli.sort;
    let preview = cli.preview || cli.preview_and_full;
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

    // Shared quality/format/preview settings for every download command
    let options = download::DownloadOptions {
        format,
        min_format,
        preview,
        preview_and_full: cli.preview_and_full,
    };

    let output = resolve_output_dir(cli.output.clone(), env_output_dir());

    // --show-defaults: print the effective settings and exit before any
    // login, network access, or disk writes
    if cli.show_defaults {
        print_defaults(&cli, format, min_format, max_format, options, &output, sort)?;
        return Ok(());
    }

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
        drop(user);

        if !matches!(command, Commands::Serve { .. } | Commands::Stream { .. }) {
            tokio::fs::create_dir_all(&output).await?;
        }
    }

    match command {
        Commands::Track { query } => {
            if !download_track_query(
                &api,
                &query,
                options,
                &output,
                search_limit,
                cli.pick,
                json_output,
                cli.dry_run,
                cli.sort,
                cli.sort_dir,
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
                        options,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                    )
                    .await?;
                }
            }
        }
        Commands::Favorites => match json_output {
            Some(fmt) => {
                print_favorites_json(&api, fmt, cli.limit, cli.offset, cli.sort, cli.sort_dir)
                    .await?;
            }
            None => {
                download::download_favorites(&api, options, &output, cli.concurrency, cli.dry_run)
                    .await?;
            }
        },
        Commands::Artist { query } => {
            if !download_artist_query(
                &api,
                &query,
                options,
                &output,
                search_limit,
                cli.pick,
                json_output,
                cli.concurrency,
                cli.dry_run,
            )
            .await?
            {
                return Ok(());
            }
        }
        Commands::Following => {
            let user_id = api.current_user.lock().await.as_ref().map_or(0, |u| u.id);
            let artists = api.get_followed_artists(user_id).await?;

            if sort == SortKey::Duration {
                anyhow::bail!("--sort duration does not apply to followed artists");
            }

            // Enrich every artist with its best available quality so --json
            // rows, --pick, and the download-all headers all agree; only the
            // quality sort key changes the order (relevance keeps the
            // followed-list order).
            let mut enriched = enrich_followed_artists(&api, artists).await?;
            if sort == SortKey::Quality {
                sort_followed_by_quality(&mut enriched, direction(cli.sort_dir, true));
            }

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
                    options,
                    &output,
                    cli.concurrency,
                    cli.dry_run,
                )
                .await?;
                return Ok(());
            }

            if let Some(fmt) = json_output {
                print_following_json(fmt, enriched)?;
                return Ok(());
            }

            println!(
                "Downloading releases from {} followed artist(s)\n",
                enriched.len()
            );
            for artist in &enriched {
                println!("{}", artist_header(artist));
                let art_id = artist.id.to_string();
                if download::is_artist_downloaded(&api, &art_id, &output).await? {
                    println!("  [skip] Already on disk");
                    continue;
                }
                download::download_artist(
                    &api,
                    &art_id,
                    options,
                    &output,
                    cli.concurrency,
                    cli.dry_run,
                )
                .await?;
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
                        options,
                        &output,
                        cli.concurrency,
                        cli.dry_run,
                    )
                    .await?;
                }
            }
        }
        Commands::Logout => {
            auth::remove_arl().await?;
            println!("Logged out. Stored ARL removed.");
        }
        Commands::Serve {
            host,
            port,
            refresh_secs,
        } => serve::serve(api, format, &host, port, refresh_secs).await?,
        Commands::Stream {
            playlist,
            server,
            mount,
            username,
            password,
            name,
            genre,
            url,
            public,
            bitrate,
            stereo_tool,
            sts,
            stereo_tool_key,
            stereo_rate,
            refresh_secs,
        } => {
            let password = password.or_else(|| {
                std::env::var("DEEZCO_ICECAST_PASSWORD")
                    .ok()
                    .filter(|value| !value.is_empty())
            });
            let Some(password) = password else {
                anyhow::bail!(
                    "an Icecast source password is required (--password or DEEZCO_ICECAST_PASSWORD)"
                );
            };
            let stereo = match stereo_tool {
                Some(binary) => {
                    if !binary.exists() {
                        anyhow::bail!("--stereo-tool binary not found: {}", binary.display());
                    }
                    let settings = match sts {
                        Some(path) => Some(path),
                        None => binary
                            .parent()
                            .map(|dir| dir.join("audio.sts"))
                            .filter(|path| path.exists()),
                    };
                    Some(icecast::StereoConfig {
                        binary,
                        settings,
                        key: stereo_tool_key,
                        rate: stereo_rate,
                    })
                }
                None => None,
            };
            let config = icecast::IcecastConfig {
                server,
                mount,
                username,
                password,
                name,
                genre,
                url,
                public,
                playlist: extract_id(&playlist, "playlist"),
            };
            icecast::stream(api, format, config, refresh_secs, bitrate, stereo).await?;
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
            duration: None,
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
        assert_eq!(sort_by_quality(&data, &by_id, true), vec![1, 3, 2, 0]);
    }

    #[test]
    fn sort_by_quality_ascending_puts_lowest_first() {
        let by_id = HashMap::from([
            ("1".to_string(), track_with_filesizes(0, 0, 100)),
            ("2".to_string(), track_with_filesizes(100, 100, 100)),
            ("3".to_string(), track_with_filesizes(0, 100, 100)),
        ]);
        let data: Vec<serde_json::Value> = (1..=3)
            .map(|i| serde_json::json!({ "id": i, "title": format!("t{i}") }))
            .collect();

        // Ascending: rank 1 (id 1), then rank 2 (id 3), then rank 3 (id 2)
        assert_eq!(sort_by_quality(&data, &by_id, false), vec![0, 2, 1]);
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
            resolve_quality_bounds(None, Some(TrackFormat::Mp3_320), Some(TrackFormat::Mp3_128)),
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
    fn artist_header_shows_quality_and_pluralization() {
        let base = |name: &str, albums: u64| FollowedArtist {
            id: 1,
            name: name.to_string(),
            nb_album: albums,
            best_quality: None,
        };

        assert_eq!(
            artist_header(&base("Artist", 12)),
            "--- Artist (12 albums) ---"
        );
        assert_eq!(
            artist_header(&base("Artist", 1)),
            "--- Artist (1 album) ---"
        );

        let mut flac = base("Artist", 12);
        flac.best_quality = Some("FLAC".to_string());
        assert_eq!(
            artist_header(&flac),
            "--- Artist (12 albums, best FLAC) ---"
        );
    }

    #[test]
    fn sort_followed_by_quality_orders_best_first_stably() {
        let mut artists = vec![
            FollowedArtist {
                id: 1,
                name: "Low".to_string(),
                nb_album: 2,
                best_quality: Some("MP3_128".to_string()),
            },
            FollowedArtist {
                id: 2,
                name: "High".to_string(),
                nb_album: 2,
                best_quality: Some("FLAC".to_string()),
            },
            FollowedArtist {
                id: 3,
                name: "Unknown".to_string(),
                nb_album: 2,
                best_quality: None,
            },
            FollowedArtist {
                id: 4,
                name: "Mid".to_string(),
                nb_album: 2,
                best_quality: Some("MP3_320".to_string()),
            },
            FollowedArtist {
                id: 5,
                name: "High2".to_string(),
                nb_album: 2,
                best_quality: Some("FLAC".to_string()),
            },
        ];

        sort_followed_by_quality(&mut artists, true);

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

        sort_favorites(&mut tracks, SortKey::Quality, None);

        // FLAC-available tracks first (by artist/title), then MP3_128-only
        let ids: Vec<String> = tracks.iter().map(|t| t.id_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"]);
    }

    #[test]
    fn sort_favorites_duration_sorts_shortest_first() {
        let short = GwTrack {
            duration: Some(serde_json::json!(60)),
            sng_id: serde_json::json!(1),
            ..track_with_filesizes(100, 100, 100)
        };
        let long = GwTrack {
            duration: Some(serde_json::json!(300)),
            sng_id: serde_json::json!(2),
            ..track_with_filesizes(100, 100, 100)
        };
        let unknown = GwTrack {
            duration: None,
            sng_id: serde_json::json!(3),
            ..track_with_filesizes(100, 100, 100)
        };
        let mut tracks = vec![long, unknown, short];

        sort_favorites(&mut tracks, SortKey::Duration, None);

        let ids: Vec<String> = tracks.iter().map(|t| t.id_str()).collect();
        // Shortest first; unknown duration (0s) is treated as missing and
        // sorts last
        assert_eq!(ids, vec!["1", "2", "3"]);
    }

    #[test]
    fn sort_favorites_duration_descending_longest_first() {
        let short = GwTrack {
            duration: Some(serde_json::json!(60)),
            sng_id: serde_json::json!(1),
            ..track_with_filesizes(100, 100, 100)
        };
        let long = GwTrack {
            duration: Some(serde_json::json!(300)),
            sng_id: serde_json::json!(2),
            ..track_with_filesizes(100, 100, 100)
        };
        let mut tracks = vec![short, long];

        sort_favorites(&mut tracks, SortKey::Duration, Some(SortDir::Desc));

        let ids: Vec<String> = tracks.iter().map(|t| t.id_str()).collect();
        assert_eq!(ids, vec!["2", "1"]);
    }

    #[test]
    fn sort_favorites_relevance_keeps_order() {
        // Low quality first, high quality second: relevance must not reorder
        let low = GwTrack {
            sng_id: serde_json::json!(10),
            ..track_with_filesizes(0, 0, 100)
        };
        let high = GwTrack {
            sng_id: serde_json::json!(20),
            ..track_with_filesizes(100, 100, 100)
        };
        let mut tracks = vec![low, high];

        sort_favorites(&mut tracks, SortKey::Relevance, None);

        let ids: Vec<String> = tracks.iter().map(|t| t.id_str()).collect();
        assert_eq!(ids, vec!["10", "20"]);
    }

    #[test]
    fn reorder_results_data_reorders_the_data_array() {
        let mut results = serde_json::json!({
            "data": [
                { "id": 1, "title": "first" },
                { "id": 2, "title": "second" },
                { "id": 3, "title": "third" },
            ],
            "total": 3,
            "next": "https://example.com/next",
        });

        reorder_results_data(&mut results, vec![2, 0, 1]);

        let titles: Vec<&str> = results["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, vec!["third", "first", "second"]);
        // Envelope fields are untouched
        assert_eq!(results["total"], serde_json::json!(3));
        assert_eq!(
            results["next"],
            serde_json::json!("https://example.com/next")
        );
    }

    #[test]
    fn with_duration_adds_numeric_seconds() {
        let string_duration = GwTrack {
            duration: Some(serde_json::json!("320")),
            sng_id: serde_json::json!(1),
            ..track_with_filesizes(100, 100, 100)
        };
        let missing = GwTrack {
            duration: None,
            sng_id: serde_json::json!(2),
            ..track_with_filesizes(100, 100, 100)
        };

        let out = with_duration(vec![string_duration, missing]);
        // Normalized numeric duration, plus the raw GW field
        assert_eq!(out[0]["duration"], serde_json::json!(320));
        assert_eq!(out[0]["DURATION"], serde_json::json!("320"));
        // Missing duration becomes 0
        assert_eq!(out[1]["duration"], serde_json::json!(0));
    }

    #[test]
    fn format_duration_renders_minutes_and_hours() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(60), "1:00");
        assert_eq!(format_duration(248), "4:08");
        assert_eq!(format_duration(3661), "1:01:01");
    }

    #[test]
    fn duration_sorted_indices_orders_ascending_with_missing_last() {
        let data: Vec<serde_json::Value> = vec![
            serde_json::json!({ "duration": 300 }),
            serde_json::json!({}),
            serde_json::json!({ "duration": 100 }),
        ];

        assert_eq!(duration_sorted_indices(&data, false), vec![2, 0, 1]);
        // Descending: longest first, missing still last
        assert_eq!(duration_sorted_indices(&data, true), vec![0, 2, 1]);
    }

    #[test]
    fn direction_defaults_to_natural_per_key() {
        // Quality: best first by default
        assert!(direction(None, true));
        // Duration: shortest first by default
        assert!(!direction(None, false));
        // Explicit flag wins
        assert!(!direction(Some(SortDir::Asc), true));
        assert!(direction(Some(SortDir::Desc), false));
    }

    #[test]
    fn sort_by_quality_puts_missing_ids_last() {
        let by_id = HashMap::from([("2".to_string(), track_with_filesizes(0, 100, 100))]);
        let data: Vec<serde_json::Value> = (1..=3)
            .map(|i| serde_json::json!({ "id": i, "title": format!("t{i}") }))
            .collect();

        // Only id 2 has data (rank 2); ids 1 and 3 rank 0 and keep their order
        assert_eq!(sort_by_quality(&data, &by_id, true), vec![1, 0, 2]);
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
