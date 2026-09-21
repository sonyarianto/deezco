# Pipeline & DSP

## When is the pipeline active?

Native MP3 passthrough (default) has zero extra deps. The PCM pipeline activates if **any** of these is set:

- `--crossfade <secs>` (0..12, equal-power)
- `--target-lufs <LUFS>` (R128 loudness)
- `--gain-db <dB>` (-24..+24)
- `--bitrate <kbps>` (forces CBR transcode)
- `--stereo-tool*` / `--stereo-tool-lib`

## Flow

```
MP3 bytes -> Symphonia decode -> f32 stereo @44.1k -> loudness (opt) -> crossfade
  -> ProcessorChain (gain -> Stereo Tool) -> SessionEncoder CBR -> Icecast
```

Prefetch tasks do `decode -> loudnorm -> crossfade -> DSP` in parallel on `spawn_blocking`; one session encoder downstream turns the endless PCM stream into continuous CBR — no track boundary at MP3 level, no clicks.

 filler (jingle/silence) follows the same path when the pipeline is active (loudness -> crossfade -> DSP) for consistency.

## Bitrate

`PipelineConfig::encode_bitrate` snaps `--bitrate` via `nearest_bitrate` to discrete MPEG rates (ties down). Without `--bitrate`, the fetched format's native rate wins (320 or 128). FLAC fetch as MP3 320 when pipeline active (Icecast can't stream FLAC).

## Loudness

ITU-R BS.1770 gating, per-track correction toward `target` **before** crossfade so overlap blends already-leveled audio.

```bash
--target-lufs -14   # streaming
--target-lufs -9    # hot
--target-lufs -23   # broadcast
```

Unmeasurable tracks (silence/short) pass through with a warning.

## Stereo Tool

Two backends (mutually exclusive):

- **`--stereo-tool /opt/stereo_tool_cmd_64`** — per-track subprocess (`WAV 16-bit` stdin/stdout), state resets each track, key visible in `ps aux`, failure bypasses track.
- **`--stereo-tool-lib /opt/libStereoTool_intel64.so`** — in-process `libStereoTool` (`dlopen`), no spawn/pipe, key stays in-process, state persists across tracks by default (`--stereo-tool-reset-track` for CLI-like resets). Shared via `Arc<Mutex<...>>` with ordered `claim_dsp_turn` for continuity.

Prerequisites are checked before login/network (`check_prerequisites`).

## Crossfade

Equal-power curve, overlap in stereo frames. Rendered via `render_track_overlap` — short tracks shrink gracefully, first track has no overlap. Filler uses the same handoff (tail updated, version not advanced so prefetch `claim_turn` stays ordered).
