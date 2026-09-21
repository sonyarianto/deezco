# Streaming to Icecast

`stream` pushes a playlist to an Icecast server as a live radio source (MP3 128/320 — FLAC needs the PCM pipeline). It sends ICY metadata, paces in real time, prefetches the next track, and reconnects on drop.

## Basic

```bash
deezco stream 908622995 \
  --server http://localhost:8000 \
  --mount /radio \
  --password hackme \
  --name "My Radio" \
  --genre "Electronic"

# password via env
export DEEZCO_ICECAST_PASSWORD=hackme
deezco stream https://www.deezer.com/en/playlist/908622995 --server http://localhost:8000 --mount /radio --public
```

Listeners tune in at the mount URL.

## Refresh & reconnect

```bash
deezco stream 908622995 --server http://localhost:8000 --mount /radio --password hackme --refresh-secs 60
```

Playlist edits are picked up periodically. Drops reconnect with exponential backoff (5s → 300s).

## Metadata

In-stream ICY blocks by default (`icy-metaint: 16000`). Some proxies (caster.fm) reset the source on title changes:

```bash
deezco stream ... --no-metadata
# titles still update via Icecast admin endpoint (/admin/metadata)
```

## Filler (jingles / silence)

When no track is ready (failed download, empty playlist, prefetch pending) the source stays alive with filler instead of stalling:

```bash
deezco stream 908622995 --server http://localhost:8000 --mount /radio --password hackme --jingle-dir ./jingles
```

- Random file from `--jingle-dir` (mp3/flac/ogg/wav), recent-window 3
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
- Order: `MP3 -> decode -> loudnorm -> crossfade -> ProcessorChain (gain -> Stereo Tool) -> CBR encode`
- Session encoder is persistent — no splice clicks; carry-over frames bridge track boundaries
- `SessionEncoder` bitrate snapped to discrete MPEG rates (`nearest_bitrate`)
- Failures in Stereo Tool bypass the track instead of killing the stream
