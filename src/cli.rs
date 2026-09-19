use crate::models::TrackFormat;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "deezco", version, about = "Deezer music downloader.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Output directory for downloads
    #[arg(short, long, global = true)]
    pub output: Option<PathBuf>,

    /// Audio quality: flac, 320, 128
    #[arg(short, long, global = true, default_value = "320")]
    pub quality: String,

    /// Minimum acceptable quality: fail instead of falling back below it
    #[arg(long, global = true)]
    pub min_quality: Option<String>,

    /// Maximum quality to download: caps the bitrate, never exceeding it
    #[arg(long, global = true)]
    pub max_quality: Option<String>,

    /// Shorthand for --min-quality + --max-quality: download exactly this quality
    #[arg(long, global = true)]
    pub exact: Option<String>,

    /// Deezer ARL cookie (overrides any stored login)
    #[arg(long, global = true)]
    pub arl: Option<String>,

    /// Number of search results to print (default: 10); also caps favorites JSON output
    #[arg(short, long, global = true)]
    pub limit: Option<u32>,

    /// Skip the first N tracks in favorites JSON output
    #[arg(long, global = true, default_value_t = 0)]
    pub offset: u32,

    /// Number of parallel downloads (minimum 1)
    #[arg(
        short,
        long,
        global = true,
        default_value_t = 4,
        value_parser = clap::builder::RangedI64ValueParser::<usize>::new().range(1..)
    )]
    pub concurrency: usize,

    /// 1-based index of the search result to download instead of printing the list
    #[arg(long, global = true)]
    pub pick: Option<usize>,

    /// Print search results as JSON instead of the human-readable list
    #[arg(long, global = true)]
    pub json: bool,

    /// List what would be downloaded without touching disk
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// JSON output style used with --json
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Pretty)]
    pub output_format: OutputFormat,

    /// Sort key for search and listing results
    #[arg(long, global = true, value_enum, default_value_t = SortKey::Quality)]
    pub sort: SortKey,

    /// Sort direction; defaults to each key's natural order
    /// (quality: best first, duration: shortest first)
    #[arg(long, global = true, value_enum)]
    pub sort_dir: Option<SortDir>,

    /// Download 30-second previews instead of full tracks
    #[arg(long, global = true)]
    pub preview: bool,

    /// Download both the 30-second preview and the full track (implies --preview)
    #[arg(long, global = true)]
    pub preview_and_full: bool,

    /// Print the effective defaults for every setting, then exit without
    /// logging in or touching the network
    #[arg(long, global = true)]
    pub show_defaults: bool,

    /// Show stored ARL and exit without logging in (masked by default)
    #[arg(long, global = true)]
    pub show_arl: bool,

    /// With --show-arl, reveal the full ARL instead of masked (careful: ARL is a password)
    #[arg(long, global = true)]
    pub reveal: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-friendly multi-line JSON
    Pretty,
    /// Single-line JSON, ideal for piping
    Compact,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortKey {
    /// Highest available quality first
    Quality,
    /// Original API order
    Relevance,
    /// Shortest duration first
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortDir {
    /// Lowest quality / shortest duration first
    Asc,
    /// Highest quality / longest duration first
    Desc,
}

/// The Stream variant carries many small CLI fields, making it larger than
/// the other variants.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum Commands {
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
    /// Save ARL and verify login (prompts if no --arl / DEEZCO_ARL)
    Login,
    /// Serve playlists as HTTP audio to an external service consumer
    Serve {
        /// Address to bind the HTTP server to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind the HTTP server to
        #[arg(long, default_value_t = 9001)]
        port: u16,
        /// How often to refetch playlists so web edits are picked up
        #[arg(long, default_value_t = 780)]
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
        /// Disable in-stream ICY track titles; titles still reach listeners via
        /// Icecast's admin endpoint. Use for source proxies that reset
        /// connections on metadata updates (e.g. caster.fm)
        #[arg(long)]
        no_metadata: bool,
        /// Crossfade duration in seconds (0 = hard cut). Rendered as an
        /// equal-power overlap in the PCM bus feeding the session encoder.
        #[arg(long, default_value_t = 0.0)]
        crossfade: f32,
        /// Static DSP gain in dB (-24..+24), applied post-crossfade
        /// pre-encode through the processor chain.
        #[arg(long)]
        gain_db: Option<f32>,
        /// R128 loudness target in LUFS (-23..-6). Each track is measured
        /// and corrected toward the target before the mix; unmeasurable
        /// tracks pass through. Example: `--target-lufs -14`.
        #[arg(long)]
        target_lufs: Option<f32>,
        /// Session MP3 bitrate in kbps (8..=320) when the pipeline is active,
        /// snapped to the nearest discrete MPEG rate (e.g. 100 -> 96);
        /// also activates the pipeline alone as a transcode. Without pipeline
        /// flags, 128/320 keep streaming natively.
        #[arg(long)]
        bitrate: Option<u32>,
        /// Path to the Thimeo Stereo Tool CLI binary (stereo_tool_cmd_64).
        /// Runs every track through it (decode -> process -> session encode)
        /// and activates the pipeline. State resets per track.
        /// Conflicts with `--stereo-tool-lib` (pick one backend).
        #[arg(long, conflicts_with = "stereo_tool_lib")]
        stereo_tool: Option<PathBuf>,
        /// Path to the Thimeo `libStereoTool` shared library
        /// (e.g. `libStereoTool_intel64.so` from `Stereo_Tool_Generic_plugin.zip`).
        /// In-process alternative to `--stereo-tool`: no per-track spawn, no
        /// pipe roundtrip, key stays out of `ps aux`, and processor state can
        /// persist across tracks (see `--stereo-tool-reset-track`). Shares
        /// `--stereo-tool-sts` / `--stereo-tool-key`.
        #[arg(long, conflicts_with = "stereo_tool")]
        stereo_tool_lib: Option<PathBuf>,
        /// Reset the `libStereoTool` processor state on each track boundary,
        /// mimicking the CLI per-track spawn. Default (unset) keeps AGC and
        /// loudness history continuous across tracks. Only meaningful with
        /// `--stereo-tool-lib`.
        #[arg(long)]
        stereo_tool_reset_track: bool,
        /// Stereo Tool processor settings file (.sts); the tool's own
        /// defaults apply when unset.
        #[arg(long)]
        stereo_tool_sts: Option<PathBuf>,
        /// Stereo Tool license key (visible in `ps aux` while running, like
        /// the official CLI expects).
        #[arg(long)]
        stereo_tool_key: Option<String>,
        /// Sample rate (Hz) of the Stereo Tool bus; must be 44100 to match
        /// the PCM bus.
        #[arg(long, default_value_t = 44100)]
        stereo_rate: u32,
        /// How often to refetch the playlist so web edits are picked up
        #[arg(long, default_value_t = 780)]
        refresh_secs: u64,
    },
}

pub fn parse_format(quality: &str) -> TrackFormat {
    match quality.to_lowercase().as_str() {
        "flac" | "lossless" | "9" => TrackFormat::Flac,
        "320" | "mp3_320" | "3" => TrackFormat::Mp3_320,
        "128" | "mp3_128" | "1" => TrackFormat::Mp3_128,
        _ => TrackFormat::Mp3_320,
    }
}

/// Extract ID from a Deezer URL or return the input as-is if it's already an ID
pub fn extract_id(input: &str, _entity: &str) -> String {
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
