# Introduction

Deezco is a fast, lightweight **Deezer music downloader** and **Icecast radio streamer** built in Rust. One static binary covers both offline downloads and 24/7 radio.

## What it does

- **Download** tracks, playlists, albums, artist discographies, favorites & followed artists
- **Serve** playlists over HTTP for external players (crabsoup, custom apps) + built-in web player
- **Stream** playlists to Icecast as a live CBR radio source with metadata, crossfade & broadcast processing

## Highlights

- Parallel downloads (configurable `-c/--concurrency`)
- FLAC / MP3 320 / 128 with automatic fallback + `--min-quality` / `--max-quality` / `--exact`
- 30s preview mode (`--preview`, `--preview-and-full`)
- Skip-existing + progress bars + persistent login (`~/.config/deezco/.arl`)
- JSON output (`--json`) & dry-run (`--dry-run`) for scripting
- Quality-aware sorting (`--sort quality` default, `relevance`, `duration`)

## Who is it for?

- Your own Deezer library on disk (personal use)
- Self-hosted radio from a curated Deezer playlist
- Scripts/automation that need machine-readable Deezer metadata

> Deezco is for personal/educational use only. Only download content you are entitled to access. See the repository Disclaimer.

## Next steps

- [Installation](/guide/installation)
- [Authentication](/guide/authentication)
- [Quick Start](/guide/quickstart)
