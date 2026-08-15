mod api;
mod auth;
mod crypto;
mod download;
mod models;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dialoguer::{Input, Select};
use std::path::{Path, PathBuf};

use crate::api::DeezerApi;
use crate::models::TrackFormat;

#[derive(Parser)]
#[command(name = "deezco", version, about = "Deezer music downloader CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Output directory for downloads
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Audio quality: flac, 320, 128
    #[arg(short, long, default_value = "320")]
    quality: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Download a track by URL or ID
    Track {
        /// Deezer track URL or track ID
        url: String,
    },
    /// Download a playlist by URL or ID
    Playlist {
        /// Deezer playlist URL or playlist ID
        url: String,
    },
    /// Download your liked/favorite songs
    Favorites,
    /// Download all songs from an artist
    Artist {
        /// Deezer artist URL, ID, or search name
        query: String,
    },
    /// Download an album by URL or ID
    Album {
        /// Deezer album URL or album ID
        url: String,
    },
    /// Interactive mode - choose what to download
    Interactive,
    /// Remove stored login credentials
    Logout,
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

/// Print a startup banner with the tool name and version.
fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");
    let title = format!("deezco v{version} — Deezer music downloader");
    let width = title.chars().count() + 4;
    println!("╔{}╗", "═".repeat(width));
    println!("║  {title}  ║");
    println!("╚{}╝\n", "═".repeat(width));
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

async fn interactive_mode(api: &DeezerApi, format: TrackFormat, output: &Path) -> Result<()> {
    println!("Output directory: {}\n", output.display());

    loop {
        println!();
        let choices = &[
            "Download a track (URL or search)",
            "Download a playlist",
            "Download favorites (liked songs)",
            "Download all songs from an artist",
            "Download an album",
            "Quit",
        ];

        let selection = Select::new()
            .with_prompt("What would you like to do?")
            .items(choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                let input: String = Input::new()
                    .with_prompt("Enter track URL or ID")
                    .interact_text()?;
                let id = extract_id(&input, "track");
                download::download_single_track(api, &id, format, output).await?;
            }
            1 => {
                // Show user playlists or enter URL
                let playlist_choices = &["Enter playlist URL or ID", "Choose from my playlists"];
                let pl_sel = Select::new()
                    .with_prompt("How to find the playlist?")
                    .items(playlist_choices)
                    .default(0)
                    .interact()?;

                match pl_sel {
                    0 => {
                        let input: String = Input::new()
                            .with_prompt("Enter playlist URL or ID")
                            .interact_text()?;
                        let id = extract_id(&input, "playlist");
                        download::download_playlist(api, &id, format, output).await?;
                    }
                    1 => {
                        let user = api.current_user.lock().await;
                        let user_id = user.as_ref().map(|u| u.id).unwrap_or(0);
                        drop(user);

                        let playlists = api.get_user_playlists(user_id).await?;
                        if playlists.is_empty() {
                            println!("No playlists found.");
                            continue;
                        }

                        let names: Vec<String> =
                            playlists.iter().map(|p| p.display_name()).collect();

                        let sel = Select::new()
                            .with_prompt("Select a playlist")
                            .items(&names)
                            .default(0)
                            .interact()?;

                        let playlist_id = playlists[sel].id_str();
                        download::download_playlist(api, &playlist_id, format, output).await?;
                    }
                    _ => {}
                }
            }
            2 => {
                download::download_favorites(api, format, output).await?;
            }
            3 => {
                let input: String = Input::new()
                    .with_prompt("Enter artist URL, ID, or name to search")
                    .interact_text()?;

                if !download_artist_query(api, &input, format, output).await? {
                    continue;
                }
            }
            4 => {
                let input: String = Input::new()
                    .with_prompt("Enter album URL or ID")
                    .interact_text()?;
                let id = extract_id(&input, "album");
                download::download_album(api, &id, format, output).await?;
            }
            5 => {
                println!("Bye!");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Download an artist from a URL, ID, or search query.
/// Returns `false` when the query is a name with no matching results.
async fn download_artist_query(
    api: &DeezerApi,
    query: &str,
    format: TrackFormat,
    output: &Path,
) -> Result<bool> {
    // Already a URL or ID
    if query.contains("deezer.com") || query.chars().all(|c| c.is_ascii_digit()) {
        let id = extract_id(query, "artist");
        download::download_artist(api, &id, format, output).await?;
        return Ok(true);
    }

    // Search for the artist and let the user pick one
    let results = api.search_artist(query).await?;
    let data = match results["data"].as_array() {
        Some(data) if !data.is_empty() => data,
        _ => {
            println!("No artists found for '{}'.", query);
            return Ok(false);
        }
    };

    let names: Vec<String> = data
        .iter()
        .map(|a| {
            let name = a["name"].as_str().unwrap_or("Unknown");
            let fans = a["nb_fan"].as_u64().unwrap_or(0);
            format!("{} ({} fans)", name, fans)
        })
        .collect();

    let sel = Select::new()
        .with_prompt("Select an artist")
        .items(&names)
        .default(0)
        .interact()?;

    let art_id = data[sel]["id"].as_u64().unwrap_or(0).to_string();
    download::download_artist(api, &art_id, format, output).await?;
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = parse_format(&cli.quality);
    let output = resolve_output_dir(cli.output.clone(), env_output_dir());

    if !matches!(&cli.command, Some(Commands::Logout)) {
        print_banner();
    }

    let api = DeezerApi::new()?;

    // Login and prepare the output dir for every command except logout
    if !matches!(&cli.command, Some(Commands::Logout)) {
        if !auth::login(&api).await? {
            return Ok(());
        }

        let user = api.current_user.lock().await;
        if let Some(u) = user.as_ref() {
            println!("Logged in as: {}\n", u.name);
        }

        tokio::fs::create_dir_all(&output).await?;
    }

    match cli.command {
        Some(Commands::Track { url }) => {
            let id = extract_id(&url, "track");
            download::download_single_track(&api, &id, format, &output).await?;
        }
        Some(Commands::Playlist { url }) => {
            let id = extract_id(&url, "playlist");
            download::download_playlist(&api, &id, format, &output).await?;
        }
        Some(Commands::Favorites) => {
            download::download_favorites(&api, format, &output).await?;
        }
        Some(Commands::Artist { query }) => {
            if !download_artist_query(&api, &query, format, &output).await? {
                return Ok(());
            }
        }
        Some(Commands::Album { url }) => {
            let id = extract_id(&url, "album");
            download::download_album(&api, &id, format, &output).await?;
        }
        Some(Commands::Interactive) | None => {
            interactive_mode(&api, format, &output).await?;
        }
        Some(Commands::Logout) => {
            auth::remove_arl().await?;
            println!("Logged out. Stored ARL removed.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
