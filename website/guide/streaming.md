# Streaming to Icecast

`stream` has two source modes (`--mode`, default `local`).

- **`local`** — plays pure local files from `--music-dir` (MP3 native passthrough — FLAC needs the PCM pipeline). The station admin downloads first (e.g. `deezco playlist 908622995 -o ./library`), then `stream` shuffles + loops the library with periodic rescan for new files. No Deezer API, no login.
- **`deezer`** — streams a Deezer playlist directly: each track is fetched into memory, DSP-processed, then streamed. Zero disk usage (2 tracks prefetched ahead), but requires login (ARL); `--quality flac` needs an active pipeline flag.

Both modes send ICY metadata, pace in real time, prefetch the next track, and reconnect on drop.

## Basic

```bash
# 1. admin downloads the library once
deezco playlist 908622995 -o ./library

# 2. stream it 24/7 (no login needed)
deezco stream \
  --music-dir ./library \
  --server http://localhost:8000 \
  --mount /radio \
  --password hackme \
  --name "My Radio" \
  --genre "Electronic"

# ...or stream the playlist straight from memory (login required, zero disk)
deezco stream --mode deezer --playlist 908622995 \
  --server http://localhost:8000 \
  --mount /radio \
  --password hackme \
  --name "My Radio"

# password via env
export DEEZCO_ICECAST_PASSWORD=hackme
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio --public
```

Listeners tune in at the mount URL.

## Rescan & reconnect

```bash
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio --password hackme --refresh-secs 60
```

New files the admin downloads into `--music-dir` are picked up on rescan (`local` mode); playlist edits on the Deezer web page show up within one refresh interval (`deezer` mode). Drops reconnect with exponential backoff (5s → 300s).

## Metadata

In-stream ICY blocks by default (`icy-metaint: 16000`). Some proxies (caster.fm) reset the source on title changes:

```bash
deezco stream ... --no-metadata
# titles still update via Icecast admin endpoint (/admin/metadata)
```

## Filler (jingles / silence)

When no track is ready (empty source, corrupt file, slow fetch, prefetch pending) the source stays alive with filler instead of stalling:

```bash
deezco stream --music-dir ./library --server http://localhost:8000 --mount /radio --password hackme --jingle-dir ./jingles
```

- Random file from `--jingle-dir` (mp3/flac), recent-window 3
- Falls back to 2s generated silence
- When the PCM pipeline is active, filler follows the same path as tracks (`loudness -> crossfade -> DSP -> CBR`) for sonic consistency

Without `--jingle-dir`, silence keeps the TCP stream alive so Icecast doesn't drop the source.

## Pipeline

When inactive (default) audio is **native MP3 passthrough** (zero extra deps). Activate with any of `--crossfade`, `--target-lufs`, `--gain-db`, `--bitrate`, `--stereo-tool*`:

```bash
# crossfade + gain
deezco stream ... --crossfade 6 --gain-db 3

# force CBR
deezco stream ... --crossfade 6 --bitrate 128

# loudness normalization (R128 / BS.1770)
deezco stream ... --crossfade 6 --target-lufs -14  # -9 hot, -14 streaming, -23 broadcast

# Stereo Tool via CLI (per-track spawn, state resets)
deezco stream ... --crossfade 6 \
  --stereo-tool /opt/stereo_tool_cmd_64 \
  --stereo-tool-sts /etc/stereo/audio.sts \
  --stereo-tool-key "$STEREO_KEY"

# Stereo Tool via libStereoTool (persistent, state continuous)
deezco stream ... --crossfade 6 \
  --stereo-tool-lib /opt/libStereoTool_intel64.so \
  --stereo-tool-sts /etc/stereo/audio.sts \
  --stereo-tool-key "$STEREO_KEY" \
  # optional: --stereo-tool-reset-track
```

Pipeline details:

- Bus: `f32` stereo @ 44.1 kHz
- Order: `file (mp3/flac) -> decode -> loudnorm -> crossfade -> ProcessorChain (gain -> Stereo Tool) -> CBR encode`
- Session encoder is persistent — no splice clicks; carry-over frames bridge track boundaries
- `SessionEncoder` bitrate snapped to discrete MPEG rates (`nearest_bitrate`)
- Failures in Stereo Tool bypass the track instead of killing the stream
