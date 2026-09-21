# CLI Reference

```
deezco [OPTIONS] [COMMAND]
```

## Commands

| Command | Description |
|---|---|
| `track` | Download track by URL/ID (names print search results) |
| `playlist` | Download playlist by URL/ID |
| `album` | Download album by URL/ID |
| `artist` | Download all albums from an artist (name prints results) |
| `favorites` | Download your liked songs |
| `following` | Download all releases from followed artists |
| `serve` | Serve playlists as HTTP audio (+ web player) |
| `stream` | Stream a playlist to Icecast |
| `login` | Save ARL and verify login |
| `logout` | Remove stored credentials |

## Options

| Flag | Description | Default |
|---|---|---|
| `-o, --output <DIR>` | Output directory | OS Downloads / `DEEZCO_OUTPUT_DIR` |
| `-q, --quality <QUALITY>` | `flac`, `320`, `128` | `320` |
| `--min-quality <QUALITY>` | Fail instead of fallback below |  |
| `--max-quality <QUALITY>` | Cap bitrate, never exceed |  |
| `--exact <QUALITY>` | `min==max` shorthand |  |
| `--preview` | 30s previews |  |
| `--preview-and-full` | Both preview + full |  |
| `--show-defaults` | Print effective defaults, exit |  |
| `--arl <COOKIE>` | Override stored login |  |
| `-l, --limit <N>` | Search results / JSON cap | `10` |
| `--offset <N>` | Skip first N in favorites JSON | `0` |
| `-c, --concurrency <N>` | Parallel downloads | `4` |
| `--pick <N>` | Download Nth search result (1-based) |  |
| `--json` | Print JSON instead of downloading |  |
| `--output-format <FMT>` | `pretty` or `compact` | `pretty` |
| `--sort <KEY>` | `quality`, `relevance`, `duration` | `quality` |
| `--sort-dir <DIR>` | `asc` or `desc` |  |
| `--dry-run` | List without writing |  |
| `-h, --help` | Print help |  |
| `-V, --version` | Print version |  |

## Serve options

```
deezco serve [--host 127.0.0.1] [--port 9001] [--refresh-secs 780]
```

## Stream options

```
deezco stream <PLAYLIST> --server <URL> --mount <MOUNT> --password <PW>
  [--username source] [--name TEXT] [--genre TEXT] [--url TEXT] [--public]
  [--no-metadata] [--refresh-secs 780] [--jingle-dir DIR]
  [--crossfade 0..12] [--gain-db -24..24] [--bitrate 8..320]
  [--target-lufs -40..0] [--stereo-tool PATH] [--stereo-tool-lib PATH]
  [--stereo-tool-sts PATH] [--stereo-tool-key KEY] [--stereo-tool-reset-track]
```

## Env vars

| Variable | Description |
|---|---|
| `DEEZCO_OUTPUT_DIR` | Override output dir ( `-o` wins ) |
| `DEEZCO_ARL` | Transient ARL ( `--arl` wins ) |
| `DEEZCO_ICECAST_PASSWORD` | Icecast password fallback |

## Examples

See [Quick Start](/guide/quickstart) and [Streaming](/guide/streaming).
