# Deezco

A fast, lightweight Deezer music downloader written in Rust. Single binary, no runtime dependencies.

## Features

- **Track download** — by URL or Deezer ID
- **Playlist download** — by URL, ID, or interactive selection from your account
- **Favorites download** — all your liked/loved tracks
- **Artist discography** — download every album from an artist, with name search
- **Interactive mode** — menu-driven TUI when no command is specified
- **Quality selection** — FLAC, MP3 320kbps, MP3 128kbps with automatic fallback
- **Blowfish CBC decryption** — handles Deezer's encrypted streams natively
- **Skip existing** — won't re-download files already on disk
- **Progress bars** — per-track download progress
- **Persistent login** — ARL cookie stored in `~/.config/deezco/.arl`

## Installation

### From source

```bash
git clone https://github.com/youruser/deezco.git
cd deezco
cargo build --release
```

The binary will be at `target/release/deezco` (approx. 2.5 MB).

### Requirements

- Rust 1.82+ (edition 2024)
- A valid Deezer ARL cookie (see [Authentication](#authentication))

## Usage

```
deezco [OPTIONS] [COMMAND]
```

### Commands

| Command | Description |
|-------------|----------------------------------------------|
| `track` | Download a track by URL or ID |
| `playlist` | Download a playlist by URL or ID |
| `favorites` | Download your liked/favorite songs |
| `artist` | Download all songs from an artist |
| `interactive`| Interactive mode (default when no command) |
| `logout` | Remove stored login credentials |

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `-o, --output <DIR>` | Output directory | OS Downloads folder (e.g. `~/Downloads`); override with `DEEZCO_OUTPUT_DIR` |
| `-q, --quality <QUALITY>` | Audio quality: `flac`, `320`, `128` | `320` |
| `-h, --help` | Print help | |
| `-V, --version` | Print version | |

### Environment variables

| Variable | Description |
|----------|-------------|
| `DEEZCO_OUTPUT_DIR` | Overrides the default output directory. Takes precedence over the default, but `-o, --output` still wins. |

```bash
# Set it once for your session
DEEZCO_OUTPUT_DIR=~/Music deezco track 3135556

# Or export it
export DEEZCO_OUTPUT_DIR=~/Music
deezco -q flac favorites
```

### Examples

```bash
# Interactive mode (launches menu)
deezco

# Download a single track
deezco track https://www.deezer.com/en/track/3135556
deezco track 3135556

# Download a playlist
deezco playlist https://www.deezer.com/en/playlist/908622995

# Download all your liked songs in FLAC
deezco -q flac favorites

# Download an artist's full discography
deezco artist "Daft Punk"
deezco artist 27

# Custom output directory
deezco -o ~/Music -q flac artist "Radiohead"
```

## Authentication

deezco uses Deezer's ARL cookie for authentication. To obtain it:

1. Log in to [deezer.com](https://www.deezer.com) in your browser
2. Open Developer Tools (`F12`)
3. Go to **Application** > **Cookies** > `https://www.deezer.com`
4. Copy the value of the `arl` cookie

On first launch, the CLI will prompt you to enter your ARL. It is then stored locally at `~/.config/deezco/.arl` for subsequent sessions.

To clear your credentials:

```bash
deezco logout
```

## Architecture

```
src/
  main.rs      CLI entry point, argument parsing, interactive mode
  api.rs       Deezer GW (internal) API + public API + media URL client
  auth.rs      ARL-based login, persistent credential storage
  crypto.rs    Blowfish CBC decryption, AES-128-ECB stream path, key generation
  download.rs  Track/playlist/favorites/artist download orchestration
  models.rs    Data structures (tracks, playlists, albums, formats)
```

### Technical Details

- **GW API**: `http://www.deezer.com/ajax/gw-light.php` — internal API for track metadata, playlists, user data
- **Public API**: `https://api.deezer.com` — artist search, track info
- **Media API**: `https://media.deezer.com/v1/get_url` — authenticated track stream URLs
- **Decryption**: Blowfish CBC with per-track key derived from `MD5(track_id) XOR secret`, IV `[0,1,2,3,4,5,6,7]`
- **Stream format**: every 6144 bytes (2048 * 3), the first 2048 bytes are Blowfish-encrypted

## License

MIT
