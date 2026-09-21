# Quality & Fallback

Deezco supports three Deezer qualities:

- `flac` — lossless
- `320` — MP3 320 kbps
- `128` — MP3 128 kbps

## Fallback

If the account cannot fetch the requested quality, Deezco falls back down the chain (`flac → 320 → 128`). This is shown as:

```
[MP3_128 — FLAC unavailable] Get Lucky - Daft Punk (4:09)
```

## Bounding

- `--min-quality flac` — fail instead of falling below FLAC (exit 1)
- `--max-quality 128` — cap, never exceed (capped value becomes the request)
- `--exact flac` — `min==max` shorthand, no fallback, no cap surprise

Batch commands (playlist/album/favorites/artist) skip below-floor tracks but continue with the rest. For single-track commands the exit code is 1 on floor violation (also with `--dry-run`).

## Sorting by quality

Search results and `favorites`/`following` JSON can be sorted by `quality` (default, highest first), `relevance` (API order), or `duration`.

```bash
deezco --sort quality track "Get Lucky"
deezco --sort quality --sort-dir asc track "Get Lucky"
deezco --sort duration --json favorites
```
