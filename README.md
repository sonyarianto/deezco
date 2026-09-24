[![CI](https://github.com/sonyarianto/deezco/actions/workflows/ci.yml/badge.svg)](https://github.com/sonyarianto/deezco/actions/workflows/ci.yml)

# Deezco

A fast, lightweight Deezer music downloader. Zero runtime dependencies.

## Features

- **Track download** — by URL or Deezer ID (name searches print results)
- **Playlist download** — by URL or ID
- **Favorites download** — all your liked/loved tracks
- **Artist discography** — download every album from an artist (name searches print results)
- **Followed artists** — download all releases from every artist you follow
- **Parallel downloads** — configurable concurrency for playlists, albums, favorites, and artists
- **Quality selection** — FLAC, MP3 320kbps, MP3 128kbps with automatic fallback
- **Blowfish CBC decryption** — handles Deezer's encrypted streams natively
- **Previews** — download the 30-second public sample MP3 instead of the full track (`--preview`)
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

The binary will be at `target/release/deezco`.

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
| `serve` | Serve playlists as HTTP audio for external media players (crabsoup, etc.) |
| `stream` | Stream local `--music-dir` files to an Icecast server as a live radio source (no login) |
| `login` | Save ARL and verify login (uses `--arl`, `DEEZCO_ARL`, or prompt; only `--arl`/prompt is persisted) |
| `logout` | Remove stored login credentials |

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `-o, --output <DIR>` | Output directory | OS Downloads folder (e.g. `~/Downloads`); override with `DEEZCO_OUTPUT_DIR` |
| `-q, --quality <QUALITY>` | Audio quality: `flac`, `320`, `128` | `320` |
| `--min-quality <QUALITY>` | Minimum acceptable quality: fail instead of silently falling back below it | |
| `--max-quality <QUALITY>` | Maximum quality to download: caps the bitrate, never exceeding it | |
| `--exact <QUALITY>` | Shorthand for `--min-quality` + `--max-quality`: download exactly this quality, no fallback (overrides both flags) | |
| `--preview` | Download 30-second previews instead of full tracks | |
| `--preview-and-full` | Download both the 30-second preview and the full track (implies `--preview`) | |
| `--show-defaults` | Print the effective defaults for every setting (quality resolution, preview mode, sort, output dir, ARL source), then exit without logging in or touching the network; with `--json` the same values are emitted as JSON | |
| `--arl <COOKIE>` | Deezer ARL cookie (overrides any stored login) | |
| `-l, --limit <N>` | Number of search results to print (default `10`); also caps favorites JSON tracks (all when unset) | |
| `--offset <N>` | Skip the first N tracks in favorites JSON output | `0` |
| `-c, --concurrency <N>` | Number of parallel downloads (playlists, albums, favorites, artists) | `4` |
| `--pick <N>` | Download the Nth search result (1-based) instead of printing the list; also picks a followed artist for `following` | |
| `--json` | Print results as JSON instead of downloading: track name searches print the API response (its `data` reordered per `--sort`), artist name searches print the raw API response; `playlist`, `album`, `favorites`, and `following` print their contents | |
| `--output-format <FMT>` | JSON style used with `--json`: `pretty` or `compact` | `pretty` |
| `--sort <KEY>` | Sort key for track search and listing results: `quality` (highest available first), `relevance` (original API order), `duration` (shortest first). Applies to track searches, favorites, and `following` (which supports `quality`/`relevance` only — `duration` is rejected); artists have no sortable columns | `quality` |
| `--sort-dir <DIR>` | Sort direction: `asc` or `desc`. Defaults to each key's natural order (quality best-first, duration shortest-first) | |
| `--dry-run` | List what would be downloaded without writing to disk, showing the format each track would use. Applies to downloads (IDs/URLs and `--pick`); `--json` still wins for search listings and `playlist`/`album`/`favorites`/`following` | |
| `-h, --help` | Print help | |
| `-V, --version` | Print version | |

### Environment variables

| Variable | Description |
|----------|-------------|
| `DEEZCO_OUTPUT_DIR` | Overrides the default output directory. Takes precedence over the default, but `-o, --output` still wins. |
| `DEEZCO_ARL` | Deezer ARL cookie used for login when no ARL is stored. Takes precedence over the stored cookie, but `--arl` still wins. Transient — never written to `~/.config/deezco/.arl` (use `--arl` or `login` to persist). |
| `DEEZCO_ICECAST_PASSWORD` | Icecast source password used by `stream` when `--password` is not given. |

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

# ...or just the 30-second preview instead (saved as "... (preview).mp3")
deezco --preview track 3135556
deezco --preview album 302127
deezco --preview --dry-run playlist 908622995

# ...or download both the preview and the full track
deezco --preview-and-full track 3135556

# Search by name — prints results with IDs, the format each would use
# (e.g. [FLAC], or [MP3_128 — FLAC unavailable] on a free account), duration,
# sorted by highest available quality (relevance order kept within equal quality)
deezco track "Get Lucky"
deezco track 3135556

# Other sort keys: original API order, or shortest track first
deezco --sort relevance track "Get Lucky"
deezco --sort duration -l 5 track "Get Lucky"
# Reverse the direction: longest tracks first, or lowest quality first
deezco --sort duration --sort-dir desc -l 5 track "Get Lucky"
deezco --sort quality --sort-dir asc --json favorites
# Same keys apply to the JSON output and favorites listing
deezco --sort duration --json favorites

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
# (tracks are sorted by highest available quality, then artist/title;
# each track carries a numeric "duration" in seconds; offset+limit give
# stable pages)
deezco --json favorites -l 100
deezco --json favorites --offset 100 -l 100
# e.g. list the five longest favorites
deezco --json favorites --sort duration --sort-dir desc | jq -r '.tracks[].duration' | head -5

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
# (artists whose releases are already on disk are skipped; headers show each
# artist's best available quality, e.g. "--- Daft Punk (12 albums, best FLAC) ---")
deezco following
# ...or list them as JSON, sorted by each artist's best available quality
# (each entry gains a "best_quality" field, e.g. "FLAC")
deezco --json following
deezco --json following | jq -r '.artists[] | "\(.name): \(.best_quality // "unknown")"'
# ...or download just one followed artist (by 1-based list position;
# --pick uses the same quality-sorted order as the JSON output)
deezco --json following | jq -r '.artists[].name'
deezco --pick 2 following

# Fail instead of silently falling back below a quality floor
deezco --min-quality flac track 3135556
# Batch commands reject below-floor tracks but continue with the rest
deezco -q flac --min-quality 320 playlist 908622995
# Works with dry-run too: exit code 1 when the single track would fall below
deezco --dry-run --min-quality flac --pick 1 track "Get Lucky"

# Cap the bitrate for data-sensitive connections (e.g. 128kbps max)
deezco -q flac --max-quality 128 favorites
# The cap is the effective request: annotations show [MP3_128], not a fallback
deezco --dry-run -q flac --max-quality 128 album 302127
# min and max together must be satisfiable (error otherwise)
deezco -q flac --min-quality 320 --max-quality 128 playlist 908622995

# Download exactly one quality — no fallback, no cap surprises
# (shorthand for --min-quality X --max-quality X)
deezco --exact flac track 3135556
deezco --exact 128 favorites
# Same command, but verify before downloading
deezco --dry-run --exact 320 album 302127

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

### Serving playlists to media players

`serve` exposes playlists over HTTP for external consumers (e.g. the crabsoup
media player) that pull track-by-track. It also serves a built-in web player
at `http://127.0.0.1:9001/` — the 3 crabsoup endpoints stay compatible:

```bash
# Serve every playlist on http://127.0.0.1:9001 (pick a host/port if needed)
deezco serve
deezco serve --host 0.0.0.0 --port 9002

# Each playlist is fetched once, shuffled, and walked in order; the no-repeat
# guard never opens a reshuffle with a track played recently
curl http://127.0.0.1:9001/playlists/908622995/next
curl http://127.0.0.1:9001/playlists/908622995            # list tracks
curl http://127.0.0.1:9001/tracks/3135556 -o track.mp3     # fetch audio

# Web player (control plane): open in a browser, no extra install
# - GET /                → single-file player (calls the 3 endpoints above)
# - GET /api/health      → {"status":"ok","crabsoup_compatible":true}
# - POST /playlists/{id}/next also works (GET kept for crabsoup compat)
xdg-open http://127.0.0.1:9001/
curl http://127.0.0.1:9001/api/health

# Playlist edits made on deezer.com show up within the refresh interval
deezco serve --refresh-secs 60
```

### Live streaming to Icecast

`stream` plays pure local files from `--music-dir` to an Icecast server as
a live radio source (MP3 native passthrough — FLAC files need an active
pipeline flag, otherwise they are skipped). The station admin downloads
first (e.g. `deezco playlist 908622995 -o ./library`), then `stream`
shuffles + loops the library with periodic rescan for new files — no Deezer
API, no login. It sends track titles to listeners via
ICY metadata, paces playback in real time, prefetches the next track so
changes are seamless, and reconnects automatically if the connection
drops.

```bash
# 1. admin downloads the library once
deezco playlist 908622995 -o ./library

# 2. stream it to an Icecast mount as a 24/7 radio source (no login needed)
deezco stream \
  --music-dir ./library \
  --server http://localhost:8000 \
  --mount /radio \
  --password hackme \
  --name "My Radio" \
  --genre "Electronic"

# The password can also come from the environment
export DEEZCO_ICECAST_PASSWORD=hackme
deezco stream \
  --music-dir ./library \
  --server http://localhost:8000 --mount /radio --public

# Non-default source username (defaults to "source")
deezco stream --music-dir ./library --server http://sapircast.caster.fm:14508 \
  --mount /hDQFK --password hackme --username source

# Listeners tune in at the mount; new files the admin downloads into
# --music-dir are picked up on rescan
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --refresh-secs 60

# Some source proxies (notably caster.fm's custom Icecast) reset the source
# connection whenever the in-stream ICY track title changes. If the
# connection keeps dropping at track changes, disable in-stream metadata;
# track titles then still update through Icecast's admin endpoint, so
# listeners keep seeing "now playing":
deezco stream --music-dir ./library --server http://sapircast.caster.fm:14508 \
  --mount /hDQFK --password hackme --no-metadata

# Filler when no track is ready (empty library, or next
# prefetch still pending). Instead of stalling the TCP stream and letting
# Icecast drop the source, a random jingle from the directory is played;
# without --jingle-dir a 2s silence filler keeps the connection alive.
# When the PCM pipeline is active (--crossfade/--gain-db/--target-lufs/
# --bitrate/--stereo-tool*/--stereo-tool-lib) filler PCM follows the same
# path as regular tracks (loudness -> crossfade -> DSP -> session CBR) so
# jingles stay sonically consistent. With --stereo-tool-lib the single
# shared instance serializes all DSP: jingle cache is throttled to 1 while
# a real track is still being decoded/DSP'd (so the track wins the Mutex
# and avoids the 2026-09-21 infinite-jingle starvation), expanding to 5
# only when tracks are fully prefetched to bridge burst holes; filler
# streaks (5+ consecutive) are now warned with last track error and
# music-dir context (use DEEZCO_DEBUG=1):
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --jingle-dir ./jingles

# PCM pipeline options (equal-power crossfade + static DSP gain).
# Every track is rendered decode → DSP in background prefetch tasks while
# one session encoder turns the endless PCM stream into continuous CBR —
# track boundaries never exist at the MP3 level, so splices cannot click
# (native MP3 passthrough without these flags needs nothing extra):
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --crossfade 6 --gain-db 3

# Force the session bitrate (8..=320 kbps); also activates the pipeline
# alone as a transcode. Without it, each file's own estimated rate wins
# on native passthrough:
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --crossfade 6 --bitrate 128

# R128 loudness normalization: every track is measured (ITU-R BS.1770
# gating) and corrected toward the target before the mix, so consecutive
# tracks play at one consistent level. -9 hot, -14 streaming, -23 broadcast:
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --crossfade 6 --target-lufs -14

# Broadcast processing via the licensed Thimeo Stereo Tool CLI. Every track
# runs decode -> Stereo Tool -> session encode; processor state resets at
# each track boundary, and a tool failure bypasses the track instead of
# killing the stream. The key is visible in `ps aux` while running,
# like the official CLI:
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --crossfade 6 \
  --stereo-tool /opt/stereo_tool_cmd_64 \
  --stereo-tool-sts /etc/stereo/audio.sts \
  --stereo-tool-key "$STEREO_KEY"

# Same processing via the in-process libStereoTool shared library
# (from Stereo_Tool_Generic_plugin.zip): no per-track spawn, no pipe
# roundtrip, key stays out of `ps aux`, and processor state persists across
# tracks by default (--stereo-tool-reset-track restores CLI-like resets).
# Conflicts with --stereo-tool (pick one backend):
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio \
  --password hackme --crossfade 6 \
  --stereo-tool-lib /opt/libStereoTool_intel64.so \
  --stereo-tool-sts /etc/stereo/audio.sts \
  --stereo-tool-key "$STEREO_KEY"
```

### Output layout

Within the output directory, downloads are organized by type:

- **Track** → `<output>/<Artist>/<Artist> - <Title>.<ext>`
- **Playlist** → `<output>/<Playlist Name>/`
- **Favorites** → `<output>/Favorites/`
- **Artist** → `<output>/<Artist>/<Album>/`
- **Album** → `<output>/<Artist>/<Album>/`

## Authentication

deezco uses Deezer's ARL cookie for authentication. To obtain it:

1. Log in to [deezer.com](https://www.deezer.com) in your browser
2. Open Developer Tools (`F12`)
3. Go to **Application** > **Cookies** > `https://www.deezer.com`
4. Copy the value of the `arl` cookie

There are four ways to provide the ARL (priority order, highest first):

| Priority | Method | Example | Notes |
|----------|--------|---------|-------|
| 1 | `--arl` flag | `deezco --arl 123abc track 3135556` | Overrides everything; on success saved to `~/.config/deezco/.arl` (also: `deezco --arl 123abc` or `deezco --arl 123abc login` to just persist) |
| 2 | `DEEZCO_ARL` env var | `DEEZCO_ARL=123abc deezco track 3135556` | For scripting/CI; transient — never saved (use `--arl` to persist) |
| 3 | Stored file `~/.config/deezco/.arl` | (auto-created) | Reused across runs; invalid/expired ARL is auto-removed and you are prompted again |
| 4 | Interactive prompt | (stdin) | Shown when 1–3 are missing; input is trimmed and saved on success (`deezco login` to force prompt) |

On first launch, the CLI will prompt you to enter your ARL. For scripting, pass it with `--arl <COOKIE>` (persisted) or the `DEEZCO_ARL` environment variable (transient) instead. `--arl` is then stored locally at `~/.config/deezco/.arl` for subsequent sessions; `DEEZCO_ARL` is not.
Use `deezco login` to save/verify without downloading, or `deezco --arl <COOKIE>` / `deezco --arl <COOKIE> login` to persist directly. Bare `deezco --arl <COOKIE>` also persists and exits without requiring a download command.

To inspect or clear your credentials:

```bash
deezco login                      # verify/persist via prompt (or --arl / DEEZCO_ARL)
deezco --arl 123abc login         # save with flag (DEEZCO_ARL stays transient)
deezco --arl 123abc               # also persists and exits (no download)
deezco --show-arl                 # masked: 1234****cdef (safe for screenshots)
deezco --show-arl --reveal        # full ARL — careful, it's a password (also: cat ~/.config/deezco/.arl)
deezco logout                     # remove stored ARL
```
 
## Support

If deezco is useful to you, consider supporting its development:

- [GitHub Sponsors](https://github.com/sponsors/sonyarianto)
- [Buy Me a Coffee](https://buymeacoffee.com/sonyarianto)
- [Ko-fi](https://ko-fi.com/sonyarianto)

## License

MIT

## Disclaimer

deezco is provided for **personal and educational use only**. By using this software you acknowledge and agree to the following:

- **Authorized content only.** Only download music you are lawfully entitled to access — e.g. tracks available through your own Deezer account and personal library. This tool does not grant or convey any rights to the content it downloads; those rights remain with their respective owners.
- **No redistribution.** Do not share, upload, or redistribute downloaded files. If you do not have the rights to a file, delete it.
- **Anti-circumvention.** deezco decrypts Deezer's stream encryption. Depending on your jurisdiction, circumventing technical protection measures may violate applicable laws (including §1201 of the DMCA in the United States) and Deezer's Terms of Service. You are solely responsible for using this software in compliance with the laws that apply to you.
- **No affiliation.** deezco is an independent project. It is not affiliated with, endorsed by, or connected to Deezer or any of its trademarks. "Deezer" and related names are the property of their respective owners.
- **No content included.** This repository contains no copyrighted media. All audio is downloaded at runtime from Deezer's servers.
- **As-is.** This software is provided "as is", without warranty of any kind. The authors are not liable for any damages arising from its use.

This is not legal advice. If you are unsure whether your use is lawful, consult a qualified legal professional.
