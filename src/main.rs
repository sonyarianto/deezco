mod api;
mod audio;
mod auth;
mod batch;
mod cli;
mod crypto;
mod dedupe;
mod download;
mod dsp;
mod files;
mod icecast;
mod log;
mod models;
mod output;
mod queue;
mod resolve;
mod serve;
mod sort;
mod stereo_lib;
mod track;
mod web_assets;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::collections::HashMap;
use std::path::Path;

use crate::api::DeezerApi;
use crate::cli::{Cli, Commands, OutputFormat, SortDir, SortKey, extract_id, parse_format};
use crate::models::GwTrack;
use crate::output::{
    artist_header, enrich_followed_artists, print_album_json, print_defaults, print_favorites_json,
    print_following_json, print_json, print_playlist_json,
};
use crate::resolve::{direction, env_output_dir, resolve_output_dir, resolve_quality_bounds};
use crate::sort::{
    duration_sorted_indices, format_duration, quality_sorted_indices, sort_by_quality,
    sort_followed_by_quality,
};

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
                crate::sort::reorder_results_data(&mut results, indices);
            }
            SortKey::Quality => {
                let data = results["data"].as_array().cloned().unwrap_or_default();
                let (indices, _) = quality_sorted_indices(api, &data, quality_desc).await?;
                crate::sort::reorder_results_data(&mut results, indices);
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

    // --show-arl: print stored ARL (masked by default) and exit before login
    if cli.show_arl {
        match auth::stored_arl().await {
            Some(arl) => {
                if cli.reveal {
                    println!("{arl}");
                    eprintln!("warning: ARL is a password — do not share it");
                } else {
                    println!("{}", auth::mask_arl(&arl));
                    eprintln!(
                        "stored at {} (use --show-arl --reveal to show full)",
                        auth::config_dir().join(".arl").display()
                    );
                }
            }
            None => {
                println!(
                    "No stored ARL found at {}",
                    auth::config_dir().join(".arl").display()
                );
                if cli.reveal {
                    eprintln!("hint: log in once (or use --arl / DEEZCO_ARL) to store it");
                }
            }
        }
        return Ok(());
    }
    if cli.reveal && !cli.show_arl {
        anyhow::bail!("--reveal requires --show-arl");
    }

    // ARL sources: --arl is persisted on success, DEEZCO_ARL is transient
    let flag_arl: Option<String> = cli.arl.clone();
    let env_arl: Option<String> = std::env::var("DEEZCO_ARL")
        .ok()
        .filter(|value| !value.is_empty());
    let api = DeezerApi::new()?;

    // Keep stdout clean when emitting JSON for scripts
    let json_output = cli.json.then_some(cli.output_format);
    let search_limit = cli.limit.unwrap_or(10);

    // Explicit login command: validate and persist only --arl (env stays transient, prompt saves)
    if matches!(cli.command, Some(Commands::Login)) {
        if !auth::login(&api, flag_arl.as_deref(), env_arl.as_deref()).await? {
            return Ok(());
        }
        let user = api.current_user.lock().await;
        if let Some(u) = user.as_ref() {
            println!("Logged in as: {}", u.name);
        }
        drop(user);
        println!("ARL saved to {}", auth::config_dir().join(".arl").display());
        return Ok(());
    }

    // No command: bare --arl auto-saves, otherwise print help
    let Some(command) = cli.command else {
        if flag_arl.is_some() {
            if !auth::login(&api, flag_arl.as_deref(), env_arl.as_deref()).await? {
                return Ok(());
            }
            let user = api.current_user.lock().await;
            if let Some(u) = user.as_ref() {
                println!("Logged in as: {}", u.name);
            }
            drop(user);
            println!("ARL saved to {}", auth::config_dir().join(".arl").display());
            return Ok(());
        }
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    // Login and prepare the output dir for every command except logout
    if !matches!(command, Commands::Logout) {
        if !auth::login(&api, flag_arl.as_deref(), env_arl.as_deref()).await? {
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
        Commands::Login => unreachable!("login handled before match"),
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
            no_metadata,
            crossfade,
            gain_db,
            bitrate,
            stereo_tool,
            stereo_tool_lib,
            stereo_tool_reset_track,
            stereo_tool_sts,
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
            if !(0.0..=crate::audio::MAX_CROSSFADE_SECS).contains(&crossfade) || crossfade.is_nan()
            {
                anyhow::bail!(
                    "--crossfade must be between 0 and {} seconds",
                    crate::audio::MAX_CROSSFADE_SECS
                );
            }
            if let Some(db) = gain_db
                && !(-24.0..=24.0).contains(&db)
            {
                anyhow::bail!("--gain-db must be between -24 and +24 dB");
            }
            if let Some(br) = bitrate
                && !(8..=320).contains(&br)
            {
                anyhow::bail!("--bitrate must be between 8 and 320 kbps");
            }
            if stereo_tool_reset_track && stereo_tool_lib.is_none() {
                anyhow::bail!("--stereo-tool-reset-track requires --stereo-tool-lib");
            }
            let stereo_tool = stereo_tool.map(|binary| crate::dsp::StereoToolConfig {
                binary,
                settings: stereo_tool_sts.clone(),
                key: stereo_tool_key.clone(),
                rate: stereo_rate,
            });
            let stereo_lib = stereo_tool_lib.map(|lib| crate::stereo_lib::StereoLibConfig {
                lib,
                settings: stereo_tool_sts,
                key: stereo_tool_key,
                reset_per_track: stereo_tool_reset_track,
            });
            let config = icecast::IcecastConfig {
                server,
                mount,
                username,
                password,
                metadata: !no_metadata,
                name,
                genre,
                url,
                public,
                playlist: extract_id(&playlist, "playlist"),
            };
            let pipeline = icecast::PipelineConfig {
                crossfade: crate::audio::CrossfadeConfig::new(
                    crossfade,
                    crate::audio::CrossfadeCurve::EqualPower,
                ),
                gain_db,
                bitrate,
                stereo_tool,
                stereo_lib,
            };
            // Fail fast on missing pipeline binaries before login/network.
            icecast::check_prerequisites(&pipeline)?;
            icecast::stream(api, format, config, refresh_secs, pipeline).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{SortDir, SortKey, extract_id, parse_format};
    use crate::models::{FollowedArtist, GwTrack, TrackFormat};
    use crate::output::artist_header;
    use crate::resolve::{
        default_output_dir, direction, quality_rank, resolve_output_dir, resolve_quality_bounds,
    };
    use crate::sort::{
        duration_sorted_indices, format_duration, reorder_results_data, sort_by_quality,
        sort_favorites, sort_followed_by_quality, with_duration,
    };
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::PathBuf;

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
        let data: Vec<Value> = (1..=4)
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
        let data: Vec<Value> = (1..=3)
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
        let data: Vec<Value> = vec![
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
        let data: Vec<Value> = (1..=3)
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
