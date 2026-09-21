# Downloading

## Commands

| Command | Description |
|---|---|
| `track` | Download track by URL/ID (names print search results) |
| `playlist` | Download playlist by URL/ID |
| `favorites` | Download your liked songs |
| `artist` | Download all songs from an artist (names print results) |
| `album` | Download album by URL/ID |
| `following` | Download all releases from every followed artist |
| `serve` | Serve playlists as HTTP audio |
| `stream` | Stream a playlist to Icecast |
| `login` / `logout` | Manage ARL |

## Global options

See [CLI Reference](/guide/cli-reference) for the full table. Useful ones:

- `-o/--output <DIR>` — output directory
- `-q/--quality flac|320|128` — preferred quality (default `320`)
- `--min-quality / --max-quality / --exact` — bounds
- `--preview` / `--preview-and-full` — 30s samples
- `-c/--concurrency N` — parallel downloads (default 4)
- `--sort quality|relevance|duration` (+ `--sort-dir asc|desc`)
- `--json` + `--output-format pretty|compact` — machine-readable
- `--dry-run` — list what would be downloaded

## Searching

Name queries print a sorted list instead of downloading:

```bash
deezco track "Get Lucky"                 # sorted by quality (default)
deezco --sort relevance track "Get Lucky"
deezco --sort duration -l 5 track "Get Lucky"
```

Each line shows the format that would be used, e.g. `[FLAC]` or `[MP3_128 — FLAC unavailable]` on a free account, plus duration.

To auto-download the Nth result:

```bash
deezco --pick 1 track "Get Lucky"
deezco -l 20 --pick 15 track "Get Lucky"
```

## JSON

```bash
deezco --json track "Get Lucky" | jq -r '.data[].id'
deezco --json --output-format compact -l 5 track "Get Lucky" | jq -r '.data[].title'
deezco --json playlist 908622995 | jq '.tracks | length'
deezco --json following | jq -r '.artists[] | "\(.name): \(.best_quality)"'
```

Favorites pagination:

```bash
deezco --json favorites -l 100
deezco --json favorites --offset 100 -l 100
```

## Dry run

```bash
deezco --dry-run -q flac album 302127
deezco --dry-run playlist 908622995 --min-quality flac
```
