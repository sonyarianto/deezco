use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::api::DeezerApi;
use crate::crypto;
use crate::files::sanitize_filename;
use crate::models::*;

/// Whether verbose debug logging is enabled. Set `DEEZCO_DEBUG=1` (or `true`)
/// to print download-stage messages (start/finish, byte counts) that help
/// diagnose stalls and throttling without cluttering normal output.
pub(crate) fn debug_enabled() -> bool {
    std::env::var("DEEZCO_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Quality and format settings shared by every download command.
#[derive(Clone, Copy)]
pub struct DownloadOptions {
    /// Requested audio quality
    pub format: TrackFormat,
    /// Minimum acceptable quality (fail instead of falling back below it)
    pub min_format: Option<TrackFormat>,
    /// Download the 30-second preview instead of the full track
    pub preview: bool,
    /// Download both the preview and the full track (implies `preview`)
    pub preview_and_full: bool,
}

/// The format a track would actually download as, walking the fallback chain.
/// Picks the first format (starting from the requested one) with a reported
/// filesize, falling back to the requested format when none have one.
pub(crate) fn available_format(track: &GwTrack, format: TrackFormat) -> TrackFormat {
    let mut current = Some(format);
    while let Some(fmt) = current {
        if track.filesize_for_format(fmt) > 0 {
            return fmt;
        }
        current = fmt.fallback();
    }
    format
}

/// Get a download URL for a track at the preferred format, with fallback
async fn get_download_url(
    api: &DeezerApi,
    track: &GwTrack,
    format: TrackFormat,
) -> Result<(String, TrackFormat)> {
    let current_format = format;

    // Try the new media API first
    if let Some(token) = &track.track_token
        && !token.is_empty()
    {
        if let Ok(Some(url)) = api.get_track_url(token, current_format.api_name()).await {
            return Ok((url, current_format));
        }
        // Fallback formats with new API
        let mut fallback = current_format.fallback();
        while let Some(fb) = fallback {
            if let Ok(Some(url)) = api.get_track_url(token, fb.api_name()).await {
                return Ok((url, fb));
            }
            fallback = fb.fallback();
        }
    }

    // Fallback to legacy URL generation
    let md5 = track.md5();
    let media_version = track.media_ver();
    let sng_id = track.id_str();

    if md5.is_empty() {
        bail!("Track has no MD5, cannot generate download URL");
    }

    let actual_format = available_format(track, current_format);
    let url =
        crypto::generate_crypted_stream_url(&sng_id, &md5, &media_version, actual_format.code());
    Ok((url, actual_format))
}

/// Download a plain (non-encrypted) URL to a file, used for previews.
async fn download_to_file(api: &DeezerApi, url: &str, filepath: &Path) -> Result<()> {
    let response = api
        .client()
        .get(url)
        .send()
        .await
        .context("Failed to download preview")?;

    if !response.status().is_success() {
        bail!("Preview download failed with status: {}", response.status());
    }

    let mut file = tokio::fs::File::create(filepath).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk.context("Error reading preview stream")?)
            .await?;
    }
    file.flush().await?;
    Ok(())
}

/// Download and decrypt a single track into memory.
pub struct FetchedTrack {
    pub data: Vec<u8>,
}

/// Resolve a track's download URL and fetch its decrypted audio.
pub async fn fetch_track_audio(
    api: &DeezerApi,
    track: &GwTrack,
    format: TrackFormat,
    show_progress: bool,
) -> Result<FetchedTrack> {
    let (url, _) = get_download_url(api, track, format).await?;
    fetch_track_audio_from_url(api, &url, &track.id_str(), show_progress).await
}

/// Fetch and decrypt audio from an already-resolved stream URL.
pub async fn fetch_track_audio_from_url(
    api: &DeezerApi,
    url: &str,
    sng_id: &str,
    show_progress: bool,
) -> Result<FetchedTrack> {
    // Download using the shared API client
    if debug_enabled() {
        crate::warn!("[deezco-debug] downloading track {sng_id} from {url}");
    }
    let response = api
        .client()
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download track {sng_id} from {url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Download failed with status {status} for track {sng_id}: {body}");
    }

    let total_size = response.content_length().unwrap_or(0);

    let pb = if show_progress && total_size > 0 {
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("##-"),
        );
        Some(pb)
    } else {
        None
    };

    // Download to memory (needed for decryption)
    let mut data = Vec::with_capacity(total_size as usize);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading download stream")?;
        if let Some(ref pb) = pb {
            pb.inc(chunk.len() as u64);
        }
        data.extend_from_slice(&chunk);
    }

    if let Some(pb) = pb {
        pb.finish_and_clear();
    }

    if data.is_empty() {
        bail!("Downloaded file is empty");
    }

    // Decrypt the stream
    let blowfish_key = crypto::generate_blowfish_key(sng_id);
    let final_data = crypto::decrypt_stream(&data, &blowfish_key);

    // Remove leading null bytes (depadding) - but not for ftyp (MP4)
    let output_data = if !final_data.is_empty() && final_data[0] == 0 {
        if final_data.len() > 8 && &final_data[4..8] == b"ftyp" {
            final_data
        } else {
            let start = final_data.iter().position(|&b| b != 0).unwrap_or(0);
            final_data[start..].to_vec()
        }
    } else {
        final_data
    };

    if debug_enabled() {
        crate::warn!(
            "[deezco-debug] downloaded track {sng_id}: {} bytes",
            output_data.len()
        );
    }

    Ok(FetchedTrack { data: output_data })
}

pub async fn download_track(
    api: &DeezerApi,
    track: &GwTrack,
    output_dir: &Path,
    show_progress: bool,
    options: DownloadOptions,
) -> Result<PathBuf> {
    let artist = sanitize_filename(&track.artist());
    let title = sanitize_filename(&track.title());
    let sng_id = track.id_str();

    if sng_id == "0" || title.is_empty() {
        bail!("Invalid track data");
    }

    // Ensure the destination directory exists. Callers pass the final
    // destination (e.g. <output>/Artist for single tracks, <output>/Artist/
    // <Album> for albums), so no artist folder is added here.
    fs::create_dir_all(output_dir).await?;

    // 30-second preview: a plain MP3, no format negotiation or decryption.
    // In preview-and-full mode it's saved alongside the full track.
    if options.preview {
        let filename = format!("{} - {} (preview).mp3", artist, title);
        let filepath = output_dir.join(&filename);
        if !filepath.exists() {
            let url = match api.get_track_preview(&sng_id).await? {
                Some(url) => url,
                None => bail!("Track has no preview available"),
            };
            download_to_file(api, &url, &filepath).await?;
        } else if show_progress {
            println!("  [skip] {} (already exists)", filename);
        }
        if !options.preview_and_full {
            return Ok(filepath);
        }
    }

    // Get download URL
    let (url, actual_format) = get_download_url(api, track, options.format).await?;

    // Refuse to silently fall back below the minimum quality
    if let Some(min) = options.min_format
        && actual_format.rank() < min.rank()
    {
        bail!(
            "Requested {} but only {} is available (below --min-quality {})",
            options.format,
            actual_format,
            min
        );
    }

    let extension = actual_format.extension();
    let filename = format!("{} - {}{}", artist, title, extension);
    let filepath = output_dir.join(&filename);

    // Skip if already exists
    if filepath.exists() {
        if show_progress {
            println!("  [skip] {} (already exists)", filename);
        }
        return Ok(filepath);
    }

    let fetched = fetch_track_audio_from_url(api, &url, &sng_id, show_progress).await?;

    // Write to file
    let mut file = tokio::fs::File::create(&filepath).await?;
    file.write_all(&fetched.data).await?;
    file.flush().await?;

    Ok(filepath)
}

/// Format annotation like `[FLAC]`, or `[MP3_320 — FLAC unavailable]` when
/// the requested format falls back to a lower one.
pub(crate) fn format_annotation(track: &GwTrack, format: TrackFormat) -> String {
    let actual = available_format(track, format);
    if actual == format {
        format!("[{}]", actual.api_name())
    } else {
        format!(
            "[{} — {} unavailable]",
            actual.api_name(),
            format.api_name()
        )
    }
}

/// Track label for dry-run output: the format it would use, `[preview]` for
/// sample-only downloads, or `[preview + full]` for both.
pub(crate) fn dry_run_track_label(track: &GwTrack, options: DownloadOptions) -> String {
    if options.preview_and_full {
        format!("{} [preview + full]", track.display_name())
    } else if options.preview {
        format!("{} [preview]", track.display_name())
    } else {
        format!(
            "{} {}",
            track.display_name(),
            format_annotation(track, options.format)
        )
    }
}
