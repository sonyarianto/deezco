use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::api::DeezerApi;
use crate::crypto;
use crate::models::*;

/// Sanitize a filename by removing/replacing invalid characters
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn value_as_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn normalized_artist_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn track_belongs_to_artist(track: &GwTrack, artist_id: &str, artist_name: &str) -> bool {
    let expected_name = normalized_artist_name(artist_name);

    if track.art_id.as_ref().and_then(value_as_string).as_deref() == Some(artist_id) {
        return true;
    }

    if normalized_artist_name(&track.artist()) == expected_name {
        return true;
    }

    false
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_lowercase().as_str(),
                "flac" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav"
            )
        })
        .unwrap_or(false)
}

async fn remove_empty_dirs(root: &Path) -> Result<()> {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                stack.push(path);
            }
        }
        dirs.push(dir);
    }

    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    for dir in dirs {
        if dir == root {
            continue;
        }

        if fs::read_dir(&dir).await?.next_entry().await?.is_none() {
            fs::remove_dir(&dir).await?;
        }
    }

    Ok(())
}

async fn clean_artist_directory(artist_dir: &Path, artist_name: &str) -> Result<usize> {
    if !artist_dir.exists() {
        return Ok(0);
    }

    let expected_prefix = format!("{} - ", sanitize_filename(artist_name));
    let mut removed = 0;
    let mut stack = vec![artist_dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;

            if file_type.is_dir() {
                stack.push(path);
                continue;
            }

            if !file_type.is_file() || !is_audio_file(&path) {
                continue;
            }

            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            if !filename.starts_with(&expected_prefix) {
                fs::remove_file(&path).await?;
                removed += 1;
            }
        }
    }

    remove_empty_dirs(artist_dir).await?;

    Ok(removed)
}

async fn audio_file_hash(path: &Path) -> Result<String> {
    let data = fs::read(path)
        .await
        .with_context(|| format!("Failed to read audio file for hashing: {}", path.display()))?;
    Ok(crypto::md5_hex(&data))
}

async fn collect_audio_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_audio_file(&path) {
                files.push(path);
            }
        }
    }

    Ok(files)
}

async fn build_audio_hash_index(root: &Path) -> Result<(HashMap<String, PathBuf>, usize)> {
    let mut index = HashMap::new();
    let mut linked = 0;

    for file in collect_audio_files(root).await? {
        if dedupe_audio_file(&file, &mut index).await? {
            linked += 1;
        }
    }

    Ok((index, linked))
}

async fn replace_with_hardlink(source: &Path, target: &Path) -> Result<()> {
    let filename = target
        .file_name()
        .and_then(|filename| filename.to_str())
        .unwrap_or("duplicate");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = parent.join(format!("{}.dedupe-tmp", filename));

    for i in 1.. {
        if !temp.exists() {
            break;
        }

        temp = parent.join(format!("{}.dedupe-tmp-{}", filename, i));
    }

    fs::rename(target, &temp)
        .await
        .with_context(|| format!("Failed to prepare duplicate file: {}", target.display()))?;

    match fs::hard_link(source, target).await {
        Ok(()) => {
            fs::remove_file(&temp).await.with_context(|| {
                format!("Failed to remove duplicate temp file: {}", temp.display())
            })?;
            Ok(())
        }
        Err(err) => {
            if let Err(restore_err) = fs::rename(&temp, target).await {
                bail!(
                    "Failed to create hardlink from {} to {}: {}; also failed to restore duplicate: {}",
                    source.display(),
                    target.display(),
                    err,
                    restore_err
                );
            }

            Err(err).with_context(|| {
                format!(
                    "Failed to create hardlink from {} to {}",
                    source.display(),
                    target.display()
                )
            })
        }
    }
}

async fn dedupe_audio_file(path: &Path, hash_index: &mut HashMap<String, PathBuf>) -> Result<bool> {
    let hash = audio_file_hash(path).await?;

    if let Some(existing) = hash_index.get(&hash) {
        if existing == path {
            return Ok(false);
        }

        replace_with_hardlink(existing, path).await?;
        return Ok(true);
    }

    hash_index.insert(hash, path.to_path_buf());
    Ok(false)
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

    // Try preferred format first
    let mut try_format = Some(current_format);
    while let Some(fmt) = try_format {
        if track.filesize_for_format(fmt) > 0 {
            let url =
                crypto::generate_crypted_stream_url(&sng_id, &md5, &media_version, fmt.code());
            return Ok((url, fmt));
        }
        try_format = fmt.fallback();
    }

    // Last resort: try the preferred format anyway
    let url =
        crypto::generate_crypted_stream_url(&sng_id, &md5, &media_version, current_format.code());
    Ok((url, current_format))
}

/// Download and decrypt a single track
pub async fn download_track(
    api: &DeezerApi,
    track: &GwTrack,
    format: TrackFormat,
    output_dir: &Path,
    show_progress: bool,
) -> Result<PathBuf> {
    let artist = sanitize_filename(&track.artist());
    let title = sanitize_filename(&track.title());
    let sng_id = track.id_str();

    if sng_id == "0" || title.is_empty() {
        bail!("Invalid track data");
    }

    // Get download URL
    let (url, actual_format) = get_download_url(api, track, format).await?;
    let extension = actual_format.extension();

    // Create output directory
    let track_dir = output_dir.join(sanitize_filename(&artist));
    fs::create_dir_all(&track_dir).await?;

    let filename = format!("{} - {}{}", artist, title, extension);
    let filepath = track_dir.join(&filename);

    // Skip if already exists
    if filepath.exists() {
        if show_progress {
            println!("  [skip] {} (already exists)", filename);
        }
        return Ok(filepath);
    }

    // Download using the shared API client
    let response = api
        .client()
        .get(&url)
        .send()
        .await
        .context("Failed to download track")?;

    if !response.status().is_success() {
        bail!("Download failed with status: {}", response.status());
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
    let blowfish_key = crypto::generate_blowfish_key(&sng_id);
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

    // Write to file
    let mut file = tokio::fs::File::create(&filepath).await?;
    file.write_all(&output_data).await?;
    file.flush().await?;

    Ok(filepath)
}

/// Download a list of tracks with bounded concurrency.
/// Each job pairs an output directory with a track.
/// Returns per-track outcomes in completion order.
async fn download_tracks_concurrently(
    api: &DeezerApi,
    jobs: &[(PathBuf, GwTrack)],
    format: TrackFormat,
    concurrency: usize,
) -> Vec<(String, Result<PathBuf>)> {
    stream::iter(jobs.iter().cloned().map(|(dir, track)| {
        let api = api.clone();
        let display = track.display_name();
        async move {
            let outcome = download_track(&api, &track, format, &dir, false).await;
            (display, outcome)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await
}

/// Print per-track results and return `(downloaded, failed)`.
fn summarize_downloads(results: Vec<(String, Result<PathBuf>)>) -> (usize, usize) {
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

/// Download a playlist by ID
pub async fn download_playlist(
    api: &DeezerApi,
    playlist_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
) -> Result<()> {
    // Get playlist info
    let info = api.get_playlist_info(playlist_id).await?;
    let playlist_name = info["DATA"]["TITLE"].as_str().unwrap_or("Unknown Playlist");
    let playlist_dir = output_dir.join(sanitize_filename(playlist_name));

    println!("Downloading playlist: {}\n", playlist_name);

    // Get tracks
    let tracks = api.get_playlist_tracks(playlist_id).await?;
    let total = tracks.len();

    println!("Found {} tracks\n", total);

    let jobs: Vec<(PathBuf, GwTrack)> = tracks
        .into_iter()
        .map(|track| (playlist_dir.clone(), track))
        .collect();
    let results = download_tracks_concurrently(api, &jobs, format, concurrency).await;
    let (downloaded, failed) = summarize_downloads(results);

    println!(
        "\nPlaylist complete: {} downloaded, {} failed out of {} tracks",
        downloaded, failed, total
    );
    Ok(())
}

/// Download user's favorite (liked) tracks
pub async fn download_favorites(
    api: &DeezerApi,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
) -> Result<()> {
    println!("Fetching favorite tracks...\n");

    let ids = api.get_favorite_track_ids().await?;
    if ids.is_empty() {
        println!("No favorite tracks found.");
        return Ok(());
    }

    println!("Found {} favorite tracks\n", ids.len());

    // Fetch track data and download in batches of 50
    let favorites_dir = output_dir.join("Favorites");
    let total = ids.len();
    let mut downloaded = 0;
    let mut failed = 0;

    for batch in ids.chunks(50) {
        let tracks = api.get_tracks_by_ids(batch).await?;
        let jobs: Vec<(PathBuf, GwTrack)> = tracks
            .into_iter()
            .map(|track| (favorites_dir.clone(), track))
            .collect();
        let results = download_tracks_concurrently(api, &jobs, format, concurrency).await;
        let (downloaded_in_batch, failed_in_batch) = summarize_downloads(results);
        downloaded += downloaded_in_batch;
        failed += failed_in_batch;
    }

    println!(
        "\nFavorites complete: {} downloaded, {} failed out of {} tracks",
        downloaded, failed, total
    );
    Ok(())
}

/// Download all tracks from an artist
pub async fn download_artist(
    api: &DeezerApi,
    art_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
) -> Result<()> {
    let artist_info = api.get_artist_info(art_id).await?;
    let artist_name = artist_info["ART_NAME"].as_str().unwrap_or("Unknown Artist");

    println!("Fetching discography for: {}\n", artist_name);

    let albums = api.get_artist_discography(art_id).await?;
    if albums.is_empty() {
        println!("No albums found for this artist.");
        return Ok(());
    }

    println!("Found {} albums/releases\n", albums.len());

    let artist_dir = output_dir.join(sanitize_filename(artist_name));
    let removed = clean_artist_directory(&artist_dir, artist_name).await?;
    if removed > 0 {
        println!(
            "Removed {} non-{} audio file(s) from the artist folder\n",
            removed, artist_name
        );
    }

    let mut total_downloaded = 0;
    let mut total_failed = 0;
    let mut total_skipped = 0;
    let (mut audio_hash_index, mut total_linked) = build_audio_hash_index(&artist_dir).await?;
    if total_linked > 0 {
        println!(
            "Linked {} duplicate audio file(s) already present in the artist folder\n",
            total_linked
        );
    }

    // Gather every album's tracks so they can be downloaded in parallel
    let mut jobs: Vec<(PathBuf, GwTrack)> = Vec::new();
    for album in &albums {
        let alb_id = album.id_str();
        let album_title = album.alb_title.as_deref().unwrap_or("Unknown Album");
        let album_dir = artist_dir.join(sanitize_filename(album_title));

        println!("--- Album: {} ---", album_title);

        let tracks = match api.get_album_tracks(&alb_id).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  [err] Failed to get album tracks: {}", e);
                total_failed += 1;
                continue;
            }
        };

        for track in tracks {
            if !track_belongs_to_artist(&track, art_id, artist_name) {
                total_skipped += 1;
                println!("    [skip] Not by {}", artist_name);
                continue;
            }
            jobs.push((album_dir.clone(), track));
        }
    }

    // Download all tracks in parallel, then dedupe in completion order
    let total = jobs.len();
    let results = download_tracks_concurrently(api, &jobs, format, concurrency).await;
    for (i, (display, outcome)) in results.into_iter().enumerate() {
        println!("  [{}/{}] {}", i + 1, total, display);
        match outcome {
            Ok(path) => {
                if dedupe_audio_file(&path, &mut audio_hash_index).await? {
                    total_linked += 1;
                    println!("    [link] Duplicate audio linked to existing file");
                }

                total_downloaded += 1;
                println!("    [ok] Downloaded");
            }
            Err(e) => {
                total_failed += 1;
                eprintln!("    [err] Failed: {}", e);
            }
        }
    }

    println!(
        "\nArtist download complete: {} downloaded, {} linked duplicates, {} skipped, {} failed",
        total_downloaded, total_linked, total_skipped, total_failed
    );
    Ok(())
}

/// Download all tracks from an album
pub async fn download_album(
    api: &DeezerApi,
    alb_id: &str,
    format: TrackFormat,
    output_dir: &Path,
    concurrency: usize,
) -> Result<()> {
    let info = api.get_album_info(alb_id).await?;
    let album_title = info["ALB_TITLE"].as_str().unwrap_or("Unknown Album");
    let artist_name = info["ART_NAME"].as_str().unwrap_or("Unknown Artist");

    println!("Downloading album: {} - {}\n", artist_name, album_title);

    let tracks = api.get_album_tracks(alb_id).await?;
    let total = tracks.len();
    println!("Found {} tracks\n", total);

    let album_dir = output_dir
        .join(sanitize_filename(artist_name))
        .join(sanitize_filename(album_title));

    let jobs: Vec<(PathBuf, GwTrack)> = tracks
        .into_iter()
        .map(|track| (album_dir.clone(), track))
        .collect();
    let results = download_tracks_concurrently(api, &jobs, format, concurrency).await;
    let (downloaded, failed) = summarize_downloads(results);

    println!(
        "\nAlbum complete: {} downloaded, {} failed out of {} tracks",
        downloaded, failed, total
    );
    Ok(())
}

/// Download a single track by URL or ID
pub async fn download_single_track(
    api: &DeezerApi,
    track_id: &str,
    format: TrackFormat,
    output_dir: &Path,
) -> Result<()> {
    println!("Fetching track info...\n");

    let track = api.get_track(track_id).await?;
    let display = track.display_name();
    println!("Downloading: {}\n", display);

    let path = download_track(api, &track, format, output_dir, true)
        .await
        .context("Failed to download track")?;
    println!("\nSaved to: {}", path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_replaces_invalid_characters() {
        assert_eq!(
            sanitize_filename("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
    }

    #[test]
    fn sanitize_filename_keeps_valid_characters() {
        assert_eq!(
            sanitize_filename("Hello, World! (2024) [FLAC] - 01.mp3"),
            "Hello, World! (2024) [FLAC] - 01.mp3"
        );
        assert_eq!(sanitize_filename("Café — Näive"), "Café — Näive");
    }

    #[test]
    fn sanitize_filename_trims_whitespace() {
        assert_eq!(sanitize_filename("  padded  "), "padded");
        // A name of only spaces sanitizes to an empty string
        assert_eq!(sanitize_filename("   "), "");
    }

    #[test]
    fn sanitize_filename_handles_empty_input() {
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn is_audio_file_recognizes_supported_extensions() {
        for ext in ["flac", "mp3", "m4a", "aac", "ogg", "opus", "wav"] {
            assert!(is_audio_file(Path::new(&format!("song.{ext}"))), "{ext}");
        }
        // Case-insensitive and works on paths with directories
        assert!(is_audio_file(Path::new("artist/album/Song.MP3")));
    }

    #[test]
    fn is_audio_file_rejects_other_files() {
        assert!(!is_audio_file(Path::new("notes.txt")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
        assert!(!is_audio_file(Path::new("song")));
        assert!(!is_audio_file(Path::new("")));
    }
}
