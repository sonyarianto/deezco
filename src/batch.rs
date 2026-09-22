use anyhow::Result;
use std::path::PathBuf;

use futures_util::{StreamExt, stream};
use indicatif::MultiProgress;

use crate::api::DeezerApi;
use crate::models::GwTrack;
use crate::track::{
    DownloadOptions, ProgressMode, available_format, download_track, dry_run_track_label,
};

/// Download a list of tracks with bounded concurrency.
/// Each job pairs an output directory with a track.
/// Every track gets its own bar inside one shared [`MultiProgress`], so
/// parallel bars stay coordinated instead of garbling the terminal.
/// Returns per-track outcomes in completion order.
pub(crate) async fn download_tracks_concurrently(
    api: &DeezerApi,
    jobs: &[(PathBuf, GwTrack)],
    concurrency: usize,
    options: DownloadOptions,
) -> Vec<(String, Result<PathBuf>)> {
    let mp = MultiProgress::new();
    stream::iter(jobs.iter().cloned().map(|(dir, track)| {
        let api = api.clone();
        // `MultiProgress` is cheap to clone (Arc inside); each task owns one
        // handle to the same console instead of borrowing across the stream.
        let mp = mp.clone();
        let display = track.display_name();
        async move {
            let progress = ProgressMode::Shared(&mp);
            let outcome = download_track(&api, &track, &dir, &progress, options).await;
            (display, outcome)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await
}

/// Count outcomes, listing only failures. Successes already showed a live
/// bar each, so re-listing them would bury the errors on big batches.
/// Returns `(downloaded, failed)`.
pub(crate) fn summarize_downloads(results: Vec<(String, Result<PathBuf>)>) -> (usize, usize) {
    let mut downloaded = 0;
    let mut failed = 0;
    for (display, outcome) in results.into_iter() {
        match outcome {
            Ok(_) => downloaded += 1,
            Err(e) => {
                failed += 1;
                eprintln!("[err] {display}: {e}");
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
