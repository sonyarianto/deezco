use crate::cli::SortDir;
use crate::models::{GwTrack, TrackFormat};
use std::path::PathBuf;

/// Default output directory: the user's OS Downloads folder.
pub fn default_output_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| PathBuf::from("./downloads"))
}

/// Output directory from the `DEEZCO_OUTPUT_DIR` environment variable.
pub fn env_output_dir() -> Option<PathBuf> {
    std::env::var_os("DEEZCO_OUTPUT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Resolve the output directory: CLI flag, then env var, then the default.
pub fn resolve_output_dir(cli_output: Option<PathBuf>, env_output: Option<PathBuf>) -> PathBuf {
    cli_output.or(env_output).unwrap_or_else(default_output_dir)
}

/// Resolve the quality bounds: `--exact` sets both min and max (overriding
/// individual flags), otherwise the individual flags pass through as given.
pub fn resolve_quality_bounds(
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
pub fn best_available_format(track: &GwTrack) -> Option<TrackFormat> {
    [
        TrackFormat::Flac,
        TrackFormat::Mp3_320,
        TrackFormat::Mp3_128,
    ]
    .into_iter()
    .find(|&fmt| track.filesize_for_format(fmt) > 0)
}

/// Rank of the highest quality format available for a track (3=FLAC … 0=none)
pub fn quality_rank(track: &GwTrack) -> u8 {
    best_available_format(track).map_or(0, TrackFormat::rank)
}

/// Effective sort direction: an explicit `--sort-dir` wins, otherwise the
/// key's natural direction (quality best-first, duration shortest-first).
pub fn direction(sort_dir: Option<SortDir>, natural_desc: bool) -> bool {
    match sort_dir {
        Some(SortDir::Desc) => true,
        Some(SortDir::Asc) => false,
        None => natural_desc,
    }
}
