use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::api::DeezerApi;
use crate::batch::{download_tracks_concurrently, print_dry_run_tracks, summarize_downloads};
use crate::dedupe::{
    build_audio_hash_index, clean_artist_directory, collect_audio_files, dedupe_audio_file,
};
use crate::files::{sanitize_filename, track_belongs_to_artist};
use crate::models::*;
use crate::track::{available_format, download_track, dry_run_track_label};
use futures_util::StreamExt;

// Re-export items that other modules reach via `crate::download::` so the
// public surface stays stable after the split.
pub(crate) use crate::track::format_annotation;
pub use crate::track::{DownloadOptions, ProgressMode, fetch_track_audio};

/// Download a playlist by ID
pub async fn download_playlist(
    api: &DeezerApi,
    playlist_id: &str,
    options: DownloadOptions,
    output_dir: &Path,
    concurrency: usize,
    dry_run: bool,
) -> Result<()> {
    // Get playlist info
    let info = api.get_playlist_info(playlist_id).await?;
    let playlist_name = info["DATA"]["TITLE"].as_str().unwrap_or("Unknown Playlist");
    let playlist_dir = output_dir.join(sanitize_filename(playlist_name));

    println!("Downloading playlist: {}\n", playlist_name);

    // Get tracks
    let tracks = api.get_playlist_tracks(playlist_id).await?;
    let total = tracks.len();

    if dry_run {
        print_dry_run_tracks(&format!("Playlist: {}", playlist_name), &tracks, options);
        return Ok(());
    }

    println!("Found {} tracks\n", total);

    let jobs: Vec<(PathBuf, GwTrack)> = tracks
        .into_iter()
        .map(|track| (playlist_dir.clone(), track))
        .collect();
    let results = download_tracks_concurrently(api, &jobs, concurrency, options).await;
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
    options: DownloadOptions,
    output_dir: &Path,
    concurrency: usize,
    dry_run: bool,
) -> Result<()> {
    println!("Fetching favorite tracks...\n");

    let ids = api.get_favorite_track_ids().await?;
    if ids.is_empty() {
        println!("No favorite tracks found.");
        return Ok(());
    }

    if dry_run {
        let kind = if options.preview_and_full {
            "preview + full"
        } else if options.preview {
            "preview"
        } else {
            options.format.api_name()
        };
        println!("[dry-run] Favorites ({} track(s), {})\n", ids.len(), kind);
        let mut index = 0;
        let mut rejected = 0;
        for batch in ids.chunks(50) {
            let tracks = api.get_tracks_by_ids(batch).await?;
            for track in &tracks {
                index += 1;
                if options.preview {
                    println!(
                        "  [{}/{}] {}",
                        index,
                        ids.len(),
                        dry_run_track_label(track, options)
                    );
                    continue;
                }
                let actual = available_format(track, options.format);
                match options.min_format {
                    Some(min) if actual.rank() < min.rank() => {
                        rejected += 1;
                        println!(
                            "  [{}/{}] {} [err] below --min-quality {} (only {} available)",
                            index,
                            ids.len(),
                            track.display_name(),
                            min,
                            actual
                        );
                    }
                    _ => {
                        println!(
                            "  [{}/{}] {}",
                            index,
                            ids.len(),
                            dry_run_track_label(track, options)
                        );
                    }
                }
            }
        }
        if rejected > 0 {
            println!(
                "\nDry run: {} track(s) would be rejected by --min-quality",
                rejected
            );
        }
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
        let results = download_tracks_concurrently(api, &jobs, concurrency, options).await;
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

/// Data needed to decide whether an artist's releases are already on disk.
pub(crate) trait ArtistCatalog {
    /// The artist's display name
    async fn artist_name(&self, art_id: &str) -> Result<String>;
    /// The artist's full discography
    async fn discography(&self, art_id: &str) -> Result<Vec<AlbumInfo>>;
}

impl ArtistCatalog for DeezerApi {
    async fn artist_name(&self, art_id: &str) -> Result<String> {
        let info = self.get_artist_info(art_id).await?;
        Ok(info["ART_NAME"]
            .as_str()
            .unwrap_or("Unknown Artist")
            .to_string())
    }

    async fn discography(&self, art_id: &str) -> Result<Vec<AlbumInfo>> {
        self.get_artist_discography(art_id).await
    }
}

/// Whether an artist's full discography already exists on disk.
/// True when every release in the discography has a non-empty folder
/// under the output directory.
pub async fn is_artist_downloaded(
    api: &impl ArtistCatalog,
    art_id: &str,
    output_dir: &Path,
) -> Result<bool> {
    let artist_name = api.artist_name(art_id).await?;
    let artist_dir = output_dir.join(sanitize_filename(&artist_name));
    if !artist_dir.exists() {
        return Ok(false);
    }

    let albums = api.discography(art_id).await?;
    if albums.is_empty() {
        return Ok(false);
    }

    for album in &albums {
        let album_title = album.alb_title.as_deref().unwrap_or("Unknown Album");
        let album_dir = artist_dir.join(sanitize_filename(album_title));
        if !album_dir.exists() || !contains_audio(&album_dir).await? {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Whether a directory contains at least one audio file
async fn contains_audio(dir: &Path) -> Result<bool> {
    Ok(!collect_audio_files(dir).await?.is_empty())
}

/// Download all tracks from an artist
pub async fn download_artist(
    api: &DeezerApi,
    art_id: &str,
    options: DownloadOptions,
    output_dir: &Path,
    concurrency: usize,
    dry_run: bool,
) -> Result<()> {
    let artist_info = api.get_artist_info(art_id).await?;
    let artist_name = artist_info["ART_NAME"].as_str().unwrap_or("Unknown Artist");

    println!("Fetching discography for: {}\n", artist_name);

    let albums = api.get_artist_discography(art_id).await?;
    if albums.is_empty() {
        println!("No albums found for this artist.");
        return Ok(());
    }

    if dry_run {
        println!("Found {} albums/releases\n", albums.len());
        let mut total = 0;
        let mut rejected = 0;
        // Fetch album tracks concurrently — was sequential (N RTTs serial).
        let results = futures_util::stream::iter(albums.iter())
            .map(|album| {
                let api = api.clone();
                let alb_id = album.id_str();
                let album_title = album
                    .alb_title
                    .clone()
                    .unwrap_or_else(|| "Unknown Album".to_string());
                async move {
                    let res = api.get_album_tracks(&alb_id).await;
                    (album_title, res)
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        for (album_title, tracks_res) in results {
            println!("--- Album: {} ---", album_title);
            let tracks = match tracks_res {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("  [err] Failed to get album tracks: {}", e);
                    continue;
                }
            };
            for track in tracks {
                if !track_belongs_to_artist(&track, art_id, artist_name) {
                    println!("    [skip] Not by {}", artist_name);
                    continue;
                }
                if options.preview {
                    total += 1;
                    println!("    [dry-run] {}", dry_run_track_label(&track, options));
                    continue;
                }
                let actual = available_format(&track, options.format);
                match options.min_format {
                    Some(min) if actual.rank() < min.rank() => {
                        rejected += 1;
                        println!(
                            "    [err] below --min-quality {} (only {} available) — {}",
                            min,
                            actual,
                            track.display_name()
                        );
                    }
                    _ => {
                        total += 1;
                        println!("    [dry-run] {}", dry_run_track_label(&track, options));
                    }
                }
            }
        }
        println!(
            "\nDry run complete: would download {} track(s), {} rejected by --min-quality",
            total, rejected
        );
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

    // Gather every album's tracks concurrently (was sequential before).
    let mut jobs: Vec<(PathBuf, GwTrack)> = Vec::new();
    let fetch_results =
        futures_util::stream::iter(albums.iter())
            .map(|album| {
                let api = api.clone();
                let alb_id = album.id_str();
                let album_title = album
                    .alb_title
                    .clone()
                    .unwrap_or_else(|| "Unknown Album".to_string());
                let album_dir = artist_dir.join(sanitize_filename(&album_title));
                let art_id = art_id.to_string();
                let artist_name = artist_name.to_string();
                async move {
                    match api.get_album_tracks(&alb_id).await {
                        Ok(tracks) => {
                            let mut filtered = Vec::new();
                            let mut skipped = 0usize;
                            for track in tracks {
                                if !track_belongs_to_artist(&track, &art_id, &artist_name) {
                                    skipped += 1;
                                } else {
                                    filtered.push((album_dir.clone(), track));
                                }
                            }
                            Ok::<(String, Vec<(PathBuf, GwTrack)>, usize), (String, anyhow::Error)>(
                                (album_title, filtered, skipped),
                            )
                        }
                        Err(e) => Err((album_title, e)),
                    }
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
    for res in fetch_results {
        match res {
            Ok((album_title, filtered, skipped)) => {
                println!("--- Album: {} ---", album_title);
                if skipped > 0 {
                    println!("    [skip] {} track(s) not by {}", skipped, artist_name);
                }
                total_skipped += skipped;
                jobs.extend(filtered);
            }
            Err((album_title, e)) => {
                eprintln!(
                    "  [err] Failed to get album tracks for {}: {}",
                    album_title, e
                );
                total_failed += 1;
            }
        }
    }

    // Download all tracks in parallel (each with its own live bar), then
    // dedupe in completion order. Successes already showed a bar each, so
    // only links and failures are printed to keep big discographies readable.
    let results = download_tracks_concurrently(api, &jobs, concurrency, options).await;
    for (display, outcome) in results.into_iter() {
        match outcome {
            Ok(path) => {
                if dedupe_audio_file(&path, &mut audio_hash_index).await? {
                    total_linked += 1;
                    println!("    [link] {display}: duplicate audio linked to existing file");
                }

                total_downloaded += 1;
            }
            Err(e) => {
                total_failed += 1;
                eprintln!("    [err] {display}: {e}");
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
    options: DownloadOptions,
    output_dir: &Path,
    concurrency: usize,
    dry_run: bool,
) -> Result<()> {
    let info = api.get_album_info(alb_id).await?;
    let album_title = info["ALB_TITLE"].as_str().unwrap_or("Unknown Album");
    let artist_name = info["ART_NAME"].as_str().unwrap_or("Unknown Artist");

    println!("Downloading album: {} - {}\n", artist_name, album_title);

    let tracks = api.get_album_tracks(alb_id).await?;
    let total = tracks.len();

    if dry_run {
        print_dry_run_tracks(
            &format!("Album: {} - {}", artist_name, album_title),
            &tracks,
            options,
        );
        return Ok(());
    }

    println!("Found {} tracks\n", total);

    let album_dir = output_dir
        .join(sanitize_filename(artist_name))
        .join(sanitize_filename(album_title));

    let jobs: Vec<(PathBuf, GwTrack)> = tracks
        .into_iter()
        .map(|track| (album_dir.clone(), track))
        .collect();
    let results = download_tracks_concurrently(api, &jobs, concurrency, options).await;
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
    options: DownloadOptions,
    output_dir: &Path,
    dry_run: bool,
) -> Result<()> {
    println!("Fetching track info...\n");

    let track = api.get_track(track_id).await?;
    let display = track.display_name();

    if dry_run {
        let artist = sanitize_filename(&track.artist());
        let title = sanitize_filename(&track.title());
        let filepath = output_dir.join(&artist).join(if options.preview {
            format!("{} - {} (preview).mp3", artist, title)
        } else {
            let actual_format = available_format(&track, options.format);
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
            format!("{} - {}{}", artist, title, actual_format.extension())
        });
        println!(
            "[dry-run] Would download: {}",
            dry_run_track_label(&track, options)
        );
        println!("[dry-run] Target: {}", filepath.display());
        return Ok(());
    }

    println!("Downloading: {}\n", display);

    // Single tracks land in <output>/<Artist>/
    let track_dir = output_dir.join(sanitize_filename(&track.artist()));
    let path = download_track(api, &track, &track_dir, &ProgressMode::Single, options)
        .await
        .context("Failed to download track")?;
    println!("\nSaved to: {}", path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::files::{is_audio_file, sanitize_filename};
    use crate::models::{AlbumInfo, GwTrack, TrackFormat};
    use crate::track::{DownloadOptions, available_format, dry_run_track_label, format_annotation};

    use super::*;

    /// A fake artist catalog with a fixed name and discography
    struct FakeCatalog {
        name: String,
        albums: Vec<AlbumInfo>,
    }

    impl ArtistCatalog for FakeCatalog {
        async fn artist_name(&self, _art_id: &str) -> Result<String> {
            Ok(self.name.clone())
        }

        async fn discography(&self, _art_id: &str) -> Result<Vec<AlbumInfo>> {
            Ok(self.albums.clone())
        }
    }

    fn album(title: &str, id: u64) -> AlbumInfo {
        AlbumInfo {
            alb_id: Some(serde_json::json!(id)),
            alb_title: Some(title.to_string()),
        }
    }

    /// A temporary directory that removes itself on drop
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("deezco-download-test-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Create an audio file at `relative`, creating parent dirs
        fn write_audio(&self, relative: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fake audio").unwrap();
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn catalog(name: &str, albums: Vec<AlbumInfo>) -> FakeCatalog {
        FakeCatalog {
            name: name.to_string(),
            albums,
        }
    }

    /// DownloadOptions for tests: requested format, no min, optional previews
    fn test_options(format: TrackFormat, preview: bool, preview_and_full: bool) -> DownloadOptions {
        DownloadOptions {
            format,
            min_format: None,
            preview,
            preview_and_full,
        }
    }

    #[tokio::test]
    async fn is_artist_downloaded_true_when_all_releases_on_disk() {
        let dir = TestDir::new("artist-complete");
        let cat = catalog(
            "AC/DC",
            vec![album("Back in Black", 1), album("Highway to Hell", 2)],
        );
        // Artist folder name is the sanitized artist name
        dir.write_audio("AC_DC/Back in Black/01.mp3");
        dir.write_audio("AC_DC/Highway to Hell/02.flac");

        assert!(is_artist_downloaded(&cat, "123", dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn is_artist_downloaded_false_when_artist_dir_missing() {
        let dir = TestDir::new("artist-dir-missing");
        let cat = catalog("AC/DC", vec![album("Back in Black", 1)]);

        assert!(!is_artist_downloaded(&cat, "123", dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn is_artist_downloaded_false_when_album_folder_missing() {
        let dir = TestDir::new("artist-album-missing");
        let cat = catalog(
            "AC/DC",
            vec![album("Back in Black", 1), album("Highway to Hell", 2)],
        );
        dir.write_audio("AC_DC/Back in Black/01.mp3");

        assert!(!is_artist_downloaded(&cat, "123", dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn is_artist_downloaded_false_when_album_folder_empty() {
        let dir = TestDir::new("artist-album-empty");
        let cat = catalog("AC/DC", vec![album("Back in Black", 1)]);
        // Folder exists but contains no audio files
        std::fs::create_dir_all(dir.path().join("AC_DC/Back in Black")).unwrap();
        dir.write_audio("AC_DC/Back in Black/cover.jpg");

        assert!(!is_artist_downloaded(&cat, "123", dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn is_artist_downloaded_false_when_discography_empty() {
        let dir = TestDir::new("artist-no-albums");
        let cat = catalog("AC/DC", Vec::new());
        dir.write_audio("AC_DC/Some Release/01.mp3");

        assert!(!is_artist_downloaded(&cat, "123", dir.path()).await.unwrap());
    }

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
    fn available_format_prefers_requested_when_available() {
        let track = track_with_filesizes(100, 100, 100);
        assert_eq!(
            available_format(&track, TrackFormat::Flac),
            TrackFormat::Flac
        );
        assert_eq!(
            available_format(&track, TrackFormat::Mp3_320),
            TrackFormat::Mp3_320
        );
    }

    #[test]
    fn available_format_falls_back_when_requested_missing() {
        // FLAC unavailable, MP3 320 available
        let track = track_with_filesizes(0, 100, 100);
        assert_eq!(
            available_format(&track, TrackFormat::Flac),
            TrackFormat::Mp3_320
        );
    }

    #[test]
    fn available_format_walks_the_whole_chain() {
        // Only MP3 128 available
        let track = track_with_filesizes(0, 0, 100);
        assert_eq!(
            available_format(&track, TrackFormat::Flac),
            TrackFormat::Mp3_128
        );
    }

    #[test]
    fn available_format_keeps_requested_when_none_available() {
        let track = track_with_filesizes(0, 0, 0);
        assert_eq!(
            available_format(&track, TrackFormat::Flac),
            TrackFormat::Flac
        );
    }

    #[test]
    fn format_annotation_shows_requested_or_fallback() {
        let available = track_with_filesizes(100, 100, 100);
        assert_eq!(format_annotation(&available, TrackFormat::Flac), "[FLAC]");

        let flac_missing = track_with_filesizes(0, 100, 100);
        assert_eq!(
            format_annotation(&flac_missing, TrackFormat::Flac),
            "[MP3_320 — FLAC unavailable]"
        );
    }

    #[test]
    fn dry_run_label_shows_requested_format_when_available() {
        let track = track_with_filesizes(100, 100, 100);
        assert_eq!(
            dry_run_track_label(&track, test_options(TrackFormat::Flac, false, false)),
            "Artist - Test [FLAC]"
        );
    }

    #[test]
    fn dry_run_label_notes_when_format_falls_back() {
        let track = track_with_filesizes(0, 100, 100);
        assert_eq!(
            dry_run_track_label(&track, test_options(TrackFormat::Flac, false, false)),
            "Artist - Test [MP3_320 — FLAC unavailable]"
        );
    }

    #[test]
    fn dry_run_label_shows_preview_in_preview_mode() {
        let track = track_with_filesizes(100, 100, 100);
        assert_eq!(
            dry_run_track_label(&track, test_options(TrackFormat::Flac, true, false)),
            "Artist - Test [preview]"
        );
    }

    #[test]
    fn dry_run_label_shows_preview_and_full() {
        let track = track_with_filesizes(100, 100, 100);
        assert_eq!(
            dry_run_track_label(&track, test_options(TrackFormat::Flac, true, true)),
            "Artist - Test [preview + full]"
        );
    }

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
