# Deezco

A fast, lightweight Deezer music downloader written in Rust. Single binary, no runtime dependencies.

## Features

- **Track download** — by URL or Deezer ID (name searches print results)
- **Playlist download** — by URL or ID
- **Favorites download** — all your liked/loved tracks
- **Artist discography** — download every album from an artist (name searches print results)
- **Followed artists** — download all releases from every artist you follow
- **Parallel downloads** — configurable concurrency for playlists, albums, favorites, and artists
- **Quality selection** — FLAC, MP3 320kbps, MP3 128kbps with automatic fallback
- **Blowfish CBC decryption** — handles Deezer's encrypted streams natively
- **Skip existing** — won't re-download files already on disk
- **Progress bars** — per-track download progress
- **Persistent login** — ARL cookie stored in `~/.config/deezco/.arl`

## Installation

### From source

```bash
git clone https://github.com/sonyarianto/deezco.git
cd deezco
cargo build --release
```

The binary will be at `target/release/deezco` (approx. 2.5 MB).

### Requirements

- Rust 1.88+ (edition 2024)
- A valid Deezer ARL cookie (see [Authentication](#authentication))

## Usage

```
deezco [OPTIONS] [COMMAND]
```

### Commands

| Command | Description |
|-------------|----------------------------------------------|
| `track` | Download a track by URL or ID (names print search results) |
| `playlist` | Download a playlist by URL or ID |
| `favorites` | Download your liked/favorite songs |
| `artist` | Download all songs from an artist (names print search results) |
| `album` | Download an album by URL or ID |
| `following` | Download all releases from every artist you follow |
| `logout` | Remove stored login credentials |

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `-o, --output <DIR>` | Output directory | OS Downloads folder (e.g. `~/Downloads`); override with `DEEZCO_OUTPUT_DIR` |
| `-q, --quality <QUALITY>` | Audio quality: `flac`, `320`, `128` | `320` |
| `--min-quality <QUALITY>` | Minimum acceptable quality: fail instead of silently falling back below it | |
| `--arl <COOKIE>` | Deezer ARL cookie (overrides any stored login) | |
| `-l, --limit <N>` | Number of search results to print (default `10`); also caps favorites JSON tracks (all when unset) | |
| `--offset <N>` | Skip the first N tracks in favorites JSON output | `0` |
| `-c, --concurrency <N>` | Number of parallel downloads (playlists, albums, favorites, artists) | `4` |
| `--pick <N>` | Download the Nth search result (1-based) instead of printing the list; also picks a followed artist for `following` | |
| `--json` | Print results as JSON instead of downloading: track/artist name searches print the raw API response; `playlist`, `album`, `favorites`, and `following` print their contents | |
| `--output-format <FMT>` | JSON style used with `--json`: `pretty` or `compact` | `pretty` |
| `--dry-run` | List what would be downloaded without writing to disk, showing the format each track would use (takes precedence over `--json` for download commands) | |
| `-h, --help` | Print help | |
| `-V, --version` | Print version | |

### Environment variables

| Variable | Description |
|----------|-------------|
| `DEEZCO_OUTPUT_DIR` | Overrides the default output directory. Takes precedence over the default, but `-o, --output` still wins. |
| `DEEZCO_ARL` | Deezer ARL cookie used for login when no ARL is stored. Takes precedence over the stored cookie, but `--arl` still wins. |

```bash
# Set it once for your session
DEEZCO_OUTPUT_DIR=~/Music deezco track 3135556

# Or export it
export DEEZCO_OUTPUT_DIR=~/Music
deezco -q flac favorites
```

### Examples

```bash
# No command prints help
deezco

# Download a single track
deezco track https://www.deezer.com/en/track/3135556
deezco track 3135556

# Search by name — prints results with IDs, the format each would use
# (e.g. [FLAC], or [MP3_128 — FLAC unavailable] on a free account), sorted by
# highest available quality (relevance order kept within equal quality)
deezco track "Get Lucky"
deezco track 3135556

# ...or auto-download a result by its list position (1-based, in the
# quality-sorted order shown above)
deezco --pick 1 track "Get Lucky"
deezco -l 20 --pick 15 track "Get Lucky"

# Machine-readable results for scripting
# (use jq to extract IDs: jq -r '.data[].id')
# The data array is sorted by highest available quality, like the list output
deezco --json track "Get Lucky"
deezco --json --output-format compact -l 5 track "Get Lucky" | jq -r '.data[].title'

# Inspect a playlist, album, or favorites without downloading
deezco --json playlist 908622995 | jq '.tracks | length'
deezco --json album 302127
deezco --json favorites

# Browse a large favorites library in sorted, paginated chunks
# (tracks are sorted by artist/title; offset+limit give stable pages)
deezco --json favorites -l 100
deezco --json favorites --offset 100 -l 100

# Download a playlist
deezco playlist https://www.deezer.com/en/playlist/908622995

# Download all your liked songs in FLAC
deezco -q flac favorites

# Download an artist's full discography (name prints results with IDs)
deezco artist "Daft Punk"
deezco artist 27

# Download a single album
deezco album https://www.deezer.com/en/album/302127
deezco album 302127

# Custom output directory
deezco -o ~/Music -q flac artist "Radiohead"

# Download all releases from every artist you follow
# (artists whose releases are already on disk are skipped)
deezco following
# ...or list them as JSON
deezco --json following
# ...or download just one followed artist (by 1-based list position)
deezco --json following | jq -r '.artists[].name'
deezco --pick 2 following

# Fail instead of silently falling back below a quality floor
deezco --min-quality flac track 3135556
# Batch commands reject below-floor tracks but continue with the rest
deezco -q flac --min-quality 320 playlist 908622995
# Works with dry-run too: exit code 1 when the single track would fall below
deezco --dry-run --min-quality flac --pick 1 track "Get Lucky"

# Control parallelism (default 4)
deezco -c 8 playlist 908622995
# Artist discographies download in parallel too
deezco -c 2 artist 27

# Preview what a command would download, without touching disk.
# Each line shows the format the track would actually use (e.g. [FLAC]),
# including fallback when the requested format isn't available
# (e.g. [MP3_128 — FLAC unavailable] on a free account).
deezco --dry-run -q flac album 302127
deezco --dry-run playlist 908622995
deezco --dry-run artist 27
deezco --dry-run favorites
deezco --dry-run following
deezco --dry-run --pick 1 track "Get Lucky"
```

### Output layout

Within the output directory, downloads are organized by type:

- **Track** → `<output>/<Artist>/<Artist> - <Title>.<ext>`
- **Playlist** → `<output>/<Playlist Name>/`
- **Favorites** → `<output>/Favorites/`
- **Artist** → `<output>/<Artist>/<Album>/`

## Authentication

deezco uses Deezer's ARL cookie for authentication. To obtain it:

1. Log in to [deezer.com](https://www.deezer.com) in your browser
2. Open Developer Tools (`F12`)
3. Go to **Application** > **Cookies** > `https://www.deezer.com`
4. Copy the value of the `arl` cookie

On first launch, the CLI will prompt you to enter your ARL. For scripting, pass it with `--arl <COOKIE>` or the `DEEZCO_ARL` environment variable instead. It is then stored locally at `~/.config/deezco/.arl` for subsequent sessions.

To clear your credentials:

```bash
deezco logout
```

## Architecture

```
src/
  main.rs      CLI entry point, argument parsing
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
