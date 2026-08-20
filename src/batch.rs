use anyhow::Result;
use std::path::PathBuf;

use futures_util::{StreamExt, stream};

use crate::api::DeezerApi;
use crate::models::GwTrack;
use crate::track::{DownloadOptions, available_format, download_track, dry_run_track_label};

/// Download a list of tracks with bounded concurrency.
/// Each job pairs an output directory with a track.
/// Returns per-track outcomes in completion order.
pub(crate) async fn download_tracks_concurrently(
    api: &DeezerApi,
    jobs: &[(PathBuf, GwTrack)],
    concurrency: usize,
    options: DownloadOptions,
) -> Vec<(String, Result<PathBuf>)> {
    stream::iter(jobs.iter().cloned().map(|(dir, track)| {
        let api = api.clone();
        let display = track.display_name();
        async move {
            let outcome = download_track(&api, &track, &dir, false, options).await;
            (display, outcome)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await
}

/// Print per-track results and return `(downloaded, failed)`.
pub(crate) fn summarize_downloads(results: Vec<(String, Result<PathBuf>)>) -> (usize, usize) {
    let total = results.len();
    let mut downloaded = 0;
    let mut failed = 0;
    for (i, (display, outcome)) in results.into_iter().enumerate() {
        println!("[{}/{}] {}", i + 1, total, display);
        match outcome {
            Ok(_) => {
                downloaded += 1;
                println!("  [ok] Downloaded successfully");
            }
            Err(e) => {
                failed += 1;
                eprintln!("  [err] Failed: {}", e);
            }
        }
    }
    (downloaded, failed)
}

/// Print the tracks a download would fetch, without downloading.
/// Tracks below `--min-quality` are flagged instead of listed as downloadable.
/// In preview modes, format checks don't apply and lines are marked
/// `[preview]` or `[preview + full]`.
pub(crate) fn print_dry_run_tracks(header: &str, tracks: &[GwTrack], options: DownloadOptions) {
    if options.preview {
        let kind = if options.preview_and_full {
            "preview + full"
        } else {
            "preview"
        };
        println!("[dry-run] {header} ({} track(s), {})\n", tracks.len(), kind);
        for (i, track) in tracks.iter().enumerate() {
            println!(
                "  [{}/{}] {}",
                i + 1,
                tracks.len(),
                dry_run_track_label(track, options)
            );
        }
        return;
    }

    println!(
        "[dry-run] {header} ({} track(s), {})\n",
        tracks.len(),
        options.format
    );
    let mut rejected = 0;
    for (i, track) in tracks.iter().enumerate() {
        let actual = available_format(track, options.format);
        match options.min_format {
            Some(min) if actual.rank() < min.rank() => {
                rejected += 1;
                println!(
                    "  [{}/{}] {} [err] below --min-quality {} (only {} available)",
                    i + 1,
                    tracks.len(),
                    track.display_name(),
                    min,
                    actual
                );
            }
            _ => {
                println!(
                    "  [{}/{}] {}",
                    i + 1,
                    tracks.len(),
                    dry_run_track_label(track, options)
                );
            }
        }
    }
    if rejected > 0 {
        println!(
            "\nDry run: {} track(s) would be rejected by --min-quality",
            rejected
        );
    }
}
