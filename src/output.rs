use crate::api::DeezerApi;
use crate::auth;
use crate::cli::{Cli, OutputFormat, SortDir, SortKey, parse_format};
use crate::download::DownloadOptions;
use crate::models::{FollowedArtist, TrackFormat};
use crate::resolve::{best_available_format, direction, env_output_dir};
use anyhow::Result;
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::path::Path;

/// Print the effective defaults for every setting. Runs before any login or
/// network access, so `deezco --show-defaults` works offline and never
/// touches disk. With `--json` the same values are emitted as JSON instead.
pub fn print_defaults(
    cli: &Cli,
    format: TrackFormat,
    min_format: Option<TrackFormat>,
    max_format: Option<TrackFormat>,
    options: DownloadOptions,
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

/// Print a value as JSON in the requested style
pub fn print_json(value: &serde_json::Value, format: OutputFormat) -> Result<()> {
    let out = match format {
        OutputFormat::Pretty => serde_json::to_string_pretty(value)?,
        OutputFormat::Compact => serde_json::to_string(value)?,
    };
    println!("{out}");
    Ok(())
}

/// Print playlist contents as JSON instead of downloading
pub async fn print_playlist_json(api: &DeezerApi, id: &str, format: OutputFormat) -> Result<()> {
    let info = api.get_playlist_info(id).await?;
    let title = info["DATA"]["TITLE"].as_str().unwrap_or("Unknown Playlist");
    let tracks = api.get_playlist_tracks(id).await?;
    print_json(
        &json!({ "type": "playlist", "id": id, "title": title, "tracks": tracks }),
        format,
    )
}

/// Print album contents as JSON instead of downloading
pub async fn print_album_json(api: &DeezerApi, id: &str, format: OutputFormat) -> Result<()> {
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
pub fn print_following_json(format: OutputFormat, artists: Vec<FollowedArtist>) -> Result<()> {
    print_json(&json!({ "type": "following", "artists": artists }), format)
}

/// Header line for a followed artist, including their release count and,
/// when known, their best available quality.
pub fn artist_header(artist: &FollowedArtist) -> String {
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
pub async fn artist_best_quality(api: &DeezerApi, art_id: &str) -> Result<Option<TrackFormat>> {
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
pub async fn enrich_followed_artists(
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

/// Print favorite tracks as JSON instead of downloading.
/// Tracks are sorted by the selected key and direction (then artist/title),
/// carry a normalized `duration` field, and are paginated with
/// `--offset`/`--limit`.
pub async fn print_favorites_json(
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

    crate::sort::sort_favorites(&mut tracks, sort, sort_dir);

    let tracks: Vec<_> = tracks
        .into_iter()
        .skip(offset as usize)
        .take(limit.map_or(usize::MAX, |l| l as usize))
        .collect();
    let tracks = crate::sort::with_duration(tracks);
    print_json(&json!({ "type": "favorites", "tracks": tracks }), format)
}
