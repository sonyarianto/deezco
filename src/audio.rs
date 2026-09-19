//! PCM bus types for the Icecast source pipeline.
//!
//! The bus is the shared language every stage speaks:
//!
//! ```text
//! Decoder -> PcmBuffer (f32 stereo @ BUS_RATE) -> Crossfader
//!   -> dsp::ProcessorChain -> Encoder -> Icecast
//! ```
//!
//! Ships the bus types, the pure crossfade math, and both converters:
//! `SymphoniaDecoder` (pure Rust, no runtime deps) and `LameEncoder`
//! (external `lame` binary, opt-in — only spawned when the pipeline is
//! active). What remains is driving them from `Producer` (decode->
//! crossfade->process->encode per track) instead of native passthrough.

// Scaffolding allow: covers bus API surface that unit tests exercise but
// production constructs only on some paths (e.g. alternate curve variants,
// future taps); remove it as coverage converges.
#![allow(dead_code)]

use std::io::Write;

use anyhow::{Context, Result};

/// Sample rate (Hz) every stage resamples to before mixing. 44100 matches
/// Deezer's MP3 sources, so the linear resampler below is a rarely-hit
/// fallback; Stereo Tool also accepts this rate natively.
pub const BUS_RATE: u32 = 44100;
/// Channel count on the bus. Stereo Tool is a stereo processor, so mono
/// sources are upmixed on decode and the encoder always emits stereo.
pub const BUS_CHANNELS: u8 = 2;
/// Longest crossfade the CLI accepts (seconds). Broadcast practice is 3–12 s;
/// 30 s is a generous ceiling for ambient/dj sets.
pub const MAX_CROSSFADE_SECS: f32 = 30.0;

/// Interleaved stereo `f32` PCM in `[-1.0, 1.0]`, always at [`BUS_RATE`].
/// `samples.len()` is even: `[L0, R0, L1, R1, ...]`.
#[derive(Clone, Debug, Default)]
pub struct PcmBuffer {
    samples: Vec<f32>,
}

impl PcmBuffer {
    /// Empty buffer.
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    /// Wrap already-interleaved stereo samples (caller guarantees the range).
    pub fn from_interleaved(samples: Vec<f32>) -> Self {
        debug_assert!(
            samples.len().is_multiple_of(2),
            "stereo frames must be even"
        );
        Self { samples }
    }

    /// Silence of `frames` stereo frames.
    pub fn silence(frames: usize) -> Self {
        Self {
            samples: vec![0.0; frames * 2],
        }
    }

    /// Raw interleaved samples.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Mutable access for DSP stages.
    pub fn samples_mut(&mut self) -> &mut [f32] {
        &mut self.samples
    }

    /// Consume the buffer into its raw interleaved samples.
    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }

    /// Number of stereo frames.
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Duration in seconds at the bus rate.
    pub fn duration_secs(&self) -> f32 {
        self.frames() as f32 / BUS_RATE as f32
    }

    /// True when there is no audio.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Fade curve used for the overlap region. Equal-power is the broadcast
/// default for music (constant perceived loudness); linear is offered for
/// speech/talk segments where power compensation sounds unnatural.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrossfadeCurve {
    /// `out = cos(t·π/2)`, `inn = sin(t·π/2)` — constant power.
    #[default]
    EqualPower,
    /// `out = 1-t`, `inn = t` — simple, dips ~3 dB in the middle.
    Linear,
}

/// Crossfade configuration: how long the tail of the outgoing track overlaps
/// the head of the incoming one.
#[derive(Clone, Copy, Debug)]
pub struct CrossfadeConfig {
    /// Overlap length in seconds; `0.0` disables crossfading (hard cut,
    /// current behaviour).
    pub duration_secs: f32,
    /// Fade curve for the overlap.
    pub curve: CrossfadeCurve,
}

impl Default for CrossfadeConfig {
    fn default() -> Self {
        Self {
            duration_secs: 0.0,
            curve: CrossfadeCurve::EqualPower,
        }
    }
}

impl CrossfadeConfig {
    /// Build a config, clamping to `0..=MAX_CROSSFADE_SECS`. `NaN`/negative
    /// collapse to disabled.
    pub fn new(duration_secs: f32, curve: CrossfadeCurve) -> Self {
        let duration_secs = if duration_secs.is_nan() {
            0.0
        } else {
            duration_secs.clamp(0.0, MAX_CROSSFADE_SECS)
        };
        Self {
            duration_secs,
            curve,
        }
    }

    /// Convenience for the current hard-cut behaviour.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// True when an overlap should be rendered.
    pub fn is_enabled(&self) -> bool {
        self.duration_secs > 0.0
    }

    /// Overlap length in stereo frames at the bus rate.
    pub fn overlap_frames(&self) -> usize {
        (self.duration_secs * BUS_RATE as f32) as usize
    }
}

/// Per-sample gains for one position `t` in `0.0..=1.0` of the overlap:
/// `(outgoing_gain, incoming_gain)`.
pub fn crossfade_weights(t: f32, curve: CrossfadeCurve) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    match curve {
        CrossfadeCurve::Linear => (1.0 - t, t),
        CrossfadeCurve::EqualPower => {
            let angle = t * std::f32::consts::FRAC_PI_2;
            (angle.cos(), angle.sin())
        }
    }
}

/// Render the overlap of two tracks: the last `tail.len()` frames of the
/// outgoing track mixed with the first `head.len()` frames of the incoming
/// one. Both slices are interleaved stereo and must have equal length.
/// Returns the mixed overlap that replaces both regions in the output stream.
pub fn apply_crossfade(tail: &[f32], head: &[f32], curve: CrossfadeCurve) -> Vec<f32> {
    debug_assert_eq!(tail.len(), head.len(), "overlap regions must match");
    debug_assert!(tail.len().is_multiple_of(2), "stereo frames must be even");
    let frames = tail.len() / 2;
    if frames == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(tail.len());
    for i in 0..frames {
        // t spans 0 (all outgoing) to 1 (all incoming) across the overlap.
        let t = if frames == 1 {
            1.0
        } else {
            i as f32 / (frames - 1) as f32
        };
        let (og, ig) = crossfade_weights(t, curve);
        out.push((tail[i * 2] * og + head[i * 2] * ig).clamp(-1.0, 1.0));
        out.push((tail[i * 2 + 1] * og + head[i * 2 + 1] * ig).clamp(-1.0, 1.0));
    }
    out
}

/// MP3 bytes -> [`PcmBuffer`]: decode, upmix to stereo, resample to
/// [`BUS_RATE`]. Implemented by [`SymphoniaDecoder`]; kept as a trait so the
/// bus design does not dictate the backend.
pub trait FrameDecoder: Send + Sync {
    /// Decoder name for logs (e.g. `"symphonia"`).
    fn name(&self) -> &str;
    /// Decode one whole track into bus PCM.
    fn decode(&self, mp3: &[u8]) -> Result<PcmBuffer>;
}

/// Whole-track MP3 -> bus PCM decoder backed by Symphonia (pure Rust, no C
/// sources, so untrusted network input never passes through a C parser).
/// Output: stereo `i16` at the source rate, resampled to [`BUS_RATE`] only
/// when it differs (Deezer MP3s are 44100 Hz, so the resampler is a
/// rarely-hit fallback — linear interpolation, upgradeable to `rubato` if a
/// non-44.1k source ever shows up in practice).
pub struct SymphoniaDecoder;

impl SymphoniaDecoder {
    /// Build a decoder (stateless; safe to share across prefetch tasks).
    pub fn new() -> Self {
        Self
    }

    /// Decode all packets of `mp3` into stereo `i16` at the source rate.
    /// Returns `(samples, source_rate)`.
    fn decode_frames(mp3: &[u8]) -> Result<(Vec<i16>, u32)> {
        use std::io::Cursor;
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::errors::Error;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;

        if mp3.is_empty() {
            anyhow::bail!("cannot decode empty MP3 input");
        }
        // Symphonia owns its source (`'static`), so the track is copied
        // once here; decoding itself stays streaming packet-by-packet.
        let mss = MediaSourceStream::new(Box::new(Cursor::new(mp3.to_vec())), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("mp3");
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|err| anyhow::anyhow!("MP3 probe failed: {err}"))?;
        let mut reader = probed.format;
        let track = reader
            .default_track()
            .context("MP3 has no default audio track")?;
        let track_id = track.id;
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .context("unsupported MP3 codec parameters")?;
        let mut stereo: Vec<i16> = Vec::new();
        let mut src_rate: Option<u32> = None;
        // Reused across packets; recreated when the stream spec changes
        // (mid-file rate/channel switches are pathological but handled).
        let mut sample_buf = None;
        loop {
            let packet = match reader.next_packet() {
                Ok(packet) => packet,
                // The packet reader reports any I/O exhaustion this way;
                // mirrors the upstream decode example (end of input).
                Err(Error::IoError(_)) => break,
                Err(Error::ResetRequired) => {
                    anyhow::bail!("MP3 track list changed mid-stream");
                }
                Err(err) => {
                    anyhow::bail!("MP3 packet read failed: {err}");
                }
            };
            if packet.track_id() != track_id {
                continue;
            }
            let decoded = match decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // Corrupt packet: skip it like the decode loop tolerates
                // trailing garbage, rather than failing the whole track.
                Err(Error::DecodeError(_)) => continue,
                Err(err) => {
                    anyhow::bail!("MP3 decode failed: {err}");
                }
            };
            let spec = *decoded.spec();
            let rate = spec.rate;
            let channels = spec.channels.count();
            src_rate.get_or_insert(rate);
            let buf: &mut SampleBuffer<i16> = match &mut sample_buf {
                Some((old_spec, buf)) if *old_spec == spec => buf,
                _ => {
                    sample_buf = Some((spec, SampleBuffer::new(decoded.capacity() as u64, spec)));
                    &mut sample_buf.as_mut().expect("just inserted").1
                }
            };
            buf.copy_interleaved_ref(decoded);
            if rate == src_rate.expect("just set") {
                push_stereo_frame(buf.samples(), channels, &mut stereo);
            } else {
                // Rate switched mid-file: resample this chunk back to the
                // track rate so the single output stream stays coherent.
                let mut chunk = Vec::new();
                push_stereo_frame(buf.samples(), channels, &mut chunk);
                stereo.extend(resample_linear_stereo(
                    &chunk,
                    rate,
                    src_rate.expect("just set"),
                ));
            }
        }
        let Some(src_rate) = src_rate else {
            anyhow::bail!("MP3 contained no audio frames");
        };
        Ok((stereo, src_rate))
    }
}

impl Default for SymphoniaDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder for SymphoniaDecoder {
    fn name(&self) -> &str {
        "symphonia"
    }

    fn decode(&self, mp3: &[u8]) -> Result<PcmBuffer> {
        let (stereo_i16, src_rate) = Self::decode_frames(mp3)?;
        let at_bus_rate = resample_linear_stereo(&stereo_i16, src_rate, BUS_RATE);
        Ok(PcmBuffer::from_interleaved(i16_to_f32_stereo(&at_bus_rate)))
    }
}

/// Append one interleaved multi-channel frame buffer as stereo `i16`:
/// mono is duplicated, stereo passes through, surround keeps L/R.
fn push_stereo_frame(interleaved: &[i16], channels: usize, out: &mut Vec<i16>) {
    let channels = channels.max(1);
    match channels {
        1 => upmix_mono_to_stereo(interleaved, out),
        2 => out.extend_from_slice(interleaved),
        n => {
            for chunk in interleaved.chunks(n) {
                if chunk.len() >= 2 {
                    out.push(chunk[0]);
                    out.push(chunk[1]);
                }
            }
        }
    }
}

/// Duplicate every mono sample to L+R.
fn upmix_mono_to_stereo(mono: &[i16], out: &mut Vec<i16>) {
    out.reserve(mono.len() * 2);
    for &s in mono {
        out.push(s);
        out.push(s);
    }
}

/// Linear-interpolating resampler for interleaved stereo `i16`.
/// Returns the input unchanged when `src_rate == dst_rate`.
fn resample_linear_stereo(input: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if src_rate == dst_rate || input.is_empty() {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    let out_frames = (in_frames as u64 * dst_rate as u64 / src_rate as u64) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 * src_rate as f64 / dst_rate as f64;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = idx.min(in_frames - 1) * 2;
        let b = (idx + 1).min(in_frames - 1) * 2;
        for ch in 0..2 {
            let s = input[a + ch] as f32 * (1.0 - frac) + input[b + ch] as f32 * frac;
            out.push(s.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
    }
    out
}

/// Convert interleaved stereo `i16` to `f32` in `[-1.0, 1.0]`.
pub(crate) fn i16_to_f32_stereo(input: &[i16]) -> Vec<f32> {
    input.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Render one track's share of the crossfade: mix the previous track's
/// held tail with the head of `pcm`, and hold this track's own tail for
/// the next one.
///
/// Returns `(output_pcm, new_tail)` — both interleaved stereo. `output_pcm`
/// is what gets DSP-processed and encoded now; `new_tail` is published to
/// the shared handoff for the following track. Degenerate inputs (empty
/// previous tail on the first track, tracks shorter than the overlap) fall
/// back gracefully instead of panicking.
pub fn render_track_overlap(
    prev_tail: &[f32],
    pcm: Vec<f32>,
    overlap_frames: usize,
    curve: CrossfadeCurve,
) -> (Vec<f32>, Vec<f32>) {
    let own_frames = pcm.len() / 2;
    if own_frames == 0 {
        return (Vec::new(), Vec::new());
    }
    let prev_frames = prev_tail.len() / 2;
    // Never consume the whole track into the overlap: keep at least one
    // frame of body so the handoff tail always moves forward, and cap the
    // held tail so overlap + hold never exceed the track (short tracks
    // shrink both gracefully instead of panicking on slice bounds).
    let overlap = overlap_frames
        .min(prev_frames)
        .min(own_frames.saturating_sub(1));
    let hold = overlap_frames.min(own_frames.saturating_sub(overlap));
    let new_tail = pcm[pcm.len() - hold * 2..].to_vec();
    if overlap == 0 {
        let body = pcm[..pcm.len() - hold * 2].to_vec();
        return (body, new_tail);
    }
    let tail_part = &prev_tail[prev_tail.len() - overlap * 2..];
    let head_part = &pcm[..overlap * 2];
    let mut out = apply_crossfade(tail_part, head_part, curve);
    out.extend_from_slice(&pcm[overlap * 2..pcm.len() - hold * 2]);
    (out, new_tail)
}

/// [`PcmBuffer`] -> CBR MP3 bytes at one fixed bitrate for the whole
/// Icecast session (constant encoder settings — required so listeners
/// never hear a format switch mid-stream).
pub trait FrameEncoder: Send + Sync {
    /// Encoder name for logs (e.g. `"lame"`).
    fn name(&self) -> &str;
    /// Output bitrate in kbps.
    fn bitrate_kbps(&self) -> u32;
    /// Encode bus PCM to MP3.
    fn encode(&self, pcm: &PcmBuffer) -> Result<Vec<u8>>;
}

/// [`FrameEncoder`] backed by the external `lame` binary (opt-in runtime
/// dependency: required only when the PCM pipeline is active — native MP3
/// passthrough never spawns it).
///
/// Bus PCM (`f32`) is converted to 16-bit stereo, wrapped in a minimal WAV
/// container so LAME reads rate/channels from the header instead of flags,
/// and piped through `lame --silent -b <bitrate> - -`.
///
/// Blocking by design (plain `std::process`): async callers must run it
/// under `spawn_blocking` so the stream pacer never stalls.
pub struct LameEncoder {
    bitrate: u32,
}

impl LameEncoder {
    /// Lowest / highest CBR bitrate LAME accepts for MP3.
    pub const MIN_BITRATE: u32 = 8;
    /// Lowest / highest CBR bitrate LAME accepts for MP3.
    pub const MAX_BITRATE: u32 = 320;

    /// Build an encoder for `bitrate` kbps; rejects anything outside
    /// 8..=320 before LAME ever runs.
    pub fn new(bitrate: u32) -> Result<Self> {
        if !(Self::MIN_BITRATE..=Self::MAX_BITRATE).contains(&bitrate) {
            anyhow::bail!(
                "LAME bitrate must be between {} and {} kbps, got {bitrate}",
                Self::MIN_BITRATE,
                Self::MAX_BITRATE
            );
        }
        Ok(Self { bitrate })
    }

    /// True when the `lame` binary runs on this machine. Check at startup
    /// (before opening the Icecast connection) so a missing binary fails
    /// fast instead of mid-stream.
    pub fn is_available() -> bool {
        std::process::Command::new("lame")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

impl FrameEncoder for LameEncoder {
    fn name(&self) -> &str {
        "lame"
    }

    fn bitrate_kbps(&self) -> u32 {
        self.bitrate
    }

    fn encode(&self, pcm: &PcmBuffer) -> Result<Vec<u8>> {
        if pcm.is_empty() {
            anyhow::bail!("cannot encode empty PCM buffer");
        }
        let s16 = f32_to_s16_stereo(pcm.samples());
        let mut wav = wav_header(s16.len() * 2, BUS_RATE);
        wav.extend(s16.iter().flat_map(|s| s.to_le_bytes()));

        let mut child = std::process::Command::new("lame")
            .args(["--silent", "-b", &self.bitrate.to_string(), "-", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| anyhow::anyhow!("failed to start lame: {err}"))?;
        // Writer thread + wait_with_output drain concurrently: a full track
        // (~35 MB WAV) never fits in a 64 KB pipe, so sequential
        // write-then-wait deadlocks (parent blocks on stdin while lame
        // blocks on stdout).
        let mut stdin = child.stdin.take().expect("piped stdin");
        let writer = std::thread::spawn(move || stdin.write_all(&wav));
        let output = child
            .wait_with_output()
            .map_err(|err| anyhow::anyhow!("failed to read lame output: {err}"))?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("lame stdin writer panicked"))?
            .map_err(|err| anyhow::anyhow!("failed to feed PCM to lame: {err}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "lame failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if output.stdout.is_empty() {
            anyhow::bail!("lame produced no MP3 output");
        }
        Ok(output.stdout)
    }
}

/// Convert interleaved stereo `f32` in `[-1.0, 1.0]` to `i16` (clamped).
pub(crate) fn f32_to_s16_stereo(input: &[f32]) -> Vec<i16> {
    input
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Minimal 44-byte WAV header for 16-bit stereo PCM at `sample_rate`.
/// `data_bytes` is the payload length that follows the header.
pub(crate) fn wav_header(data_bytes: usize, sample_rate: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_bytes as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&(sample_rate * 2 * 2).to_le_bytes());
    header.extend_from_slice(&4u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_constants_match_decoder_assumptions() {
        assert_eq!(BUS_RATE, 44100);
        assert_eq!(BUS_CHANNELS, 2);
    }

    #[test]
    fn crossfade_disabled_by_default() {
        let config = CrossfadeConfig::default();
        assert!(!config.is_enabled());
        assert_eq!(config.overlap_frames(), 0);
    }

    #[test]
    fn crossfade_duration_clamps_to_range() {
        assert_eq!(
            CrossfadeConfig::new(-5.0, CrossfadeCurve::EqualPower).duration_secs,
            0.0
        );
        assert_eq!(
            CrossfadeConfig::new(999.0, CrossfadeCurve::EqualPower).duration_secs,
            MAX_CROSSFADE_SECS
        );
        assert!(!CrossfadeConfig::new(f32::NAN, CrossfadeCurve::EqualPower).is_enabled());
        let six = CrossfadeConfig::new(6.0, CrossfadeCurve::Linear);
        assert!(six.is_enabled());
        assert_eq!(six.overlap_frames(), 6 * BUS_RATE as usize);
    }

    #[test]
    fn weights_endpoints_are_full_out_then_full_in() {
        for curve in [CrossfadeCurve::Linear, CrossfadeCurve::EqualPower] {
            let (og, ig) = crossfade_weights(0.0, curve);
            assert!((og - 1.0).abs() < 1e-6, "{curve:?} start");
            assert!(ig.abs() < 1e-6, "{curve:?} start");
            let (og, ig) = crossfade_weights(1.0, curve);
            assert!(og.abs() < 1e-6, "{curve:?} end");
            assert!((ig - 1.0).abs() < 1e-6, "{curve:?} end");
        }
    }

    #[test]
    fn linear_midpoint_dips_while_equal_power_holds() {
        let (og, ig) = crossfade_weights(0.5, CrossfadeCurve::Linear);
        assert!((og - 0.5).abs() < 1e-6);
        assert!((ig - 0.5).abs() < 1e-6);
        // Equal power: cos²+sin² = 1 at every position (constant loudness).
        let (og, ig) = crossfade_weights(0.5, CrossfadeCurve::EqualPower);
        assert!((og * og + ig * ig - 1.0).abs() < 1e-6);
        let (og, ig) = crossfade_weights(0.25, CrossfadeCurve::EqualPower);
        assert!((og * og + ig * ig - 1.0).abs() < 1e-6);
    }

    #[test]
    fn apply_crossfade_blends_tail_into_head() {
        // Outgoing = full-scale left/right, incoming = silence: the overlap
        // must start loud and end silent (and vice versa for the reverse).
        let tail = vec![1.0, 1.0, 1.0, 1.0];
        let head = vec![0.0, 0.0, 0.0, 0.0];
        let mixed = apply_crossfade(&tail, &head, CrossfadeCurve::Linear);
        assert_eq!(mixed.len(), 4);
        assert!((mixed[0] - 1.0).abs() < 1e-6);
        assert!(mixed[3].abs() < 1e-6);
        assert!(mixed[0] > mixed[2], "outgoing fades out across the overlap");
    }

    #[test]
    fn apply_crossfade_empty_overlap_is_empty() {
        let mixed = apply_crossfade(&[], &[], CrossfadeCurve::EqualPower);
        assert!(mixed.is_empty());
    }

    #[test]
    fn pcm_buffer_tracks_frames_and_duration() {
        let buf = PcmBuffer::silence(BUS_RATE as usize);
        assert_eq!(buf.frames(), BUS_RATE as usize);
        assert!((buf.duration_secs() - 1.0).abs() < 1e-6);
        assert!(!buf.is_empty());
        assert!(PcmBuffer::new().is_empty());
    }

    /// 10 stereo frames of silence-valued `v` (distinct per test).
    fn ten_frames(v: f32) -> Vec<f32> {
        vec![v; 20]
    }

    #[test]
    fn render_overlap_mixes_head_and_holds_tail() {
        // 10-frame track, 4-frame overlap: out = 4 mixed + 2 body, new tail 4.
        let (out, tail) =
            render_track_overlap(&ten_frames(1.0), ten_frames(0.0), 4, CrossfadeCurve::Linear);
        assert_eq!(out.len(), 12, "4 mixed + 2 body frames");
        assert_eq!(tail.len(), 8, "held tail");
        assert!((out[0] - 1.0).abs() < 1e-6, "overlap starts at outgoing");
        assert!(out[10].abs() < 1e-6, "body is the incoming track");
    }

    #[test]
    fn render_overlap_first_track_holds_tail_without_prefix() {
        // No previous tail: straight body, still holds its own tail.
        let (out, tail) = render_track_overlap(&[], ten_frames(0.5), 4, CrossfadeCurve::EqualPower);
        assert_eq!(out.len(), 12, "10 - 4 held frames");
        assert_eq!(tail.len(), 8);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn render_overlap_short_track_shrinks_gracefully() {
        // 3-frame track against a 4-frame request: overlap clamps to 2,
        // output is just the mixed region plus an empty body — no panic.
        let (out, tail) =
            render_track_overlap(&ten_frames(1.0), vec![0.0; 6], 4, CrossfadeCurve::Linear);
        assert_eq!(out.len(), 4);
        assert_eq!(tail.len(), 2, "hold capped so overlap + hold fit");
    }

    #[test]
    fn render_overlap_empty_track_is_empty() {
        let (out, tail) =
            render_track_overlap(&ten_frames(1.0), Vec::new(), 4, CrossfadeCurve::Linear);
        assert!(out.is_empty());
        assert!(tail.is_empty());
    }

    #[test]
    fn upmix_duplicates_mono_to_both_channels() {
        let mut out = Vec::new();
        upmix_mono_to_stereo(&[1000, -1000, 0], &mut out);
        assert_eq!(out, vec![1000, 1000, -1000, -1000, 0, 0]);
    }

    #[test]
    fn resample_same_rate_is_identity() {
        let input = vec![1, 2, 3, 4, 5, 6];
        assert_eq!(resample_linear_stereo(&input, 44100, 44100), input);
        assert!(resample_linear_stereo(&[], 22050, 44100).is_empty());
    }

    #[test]
    fn resample_upsample_doubles_frame_count() {
        // 4 frames at 22050 Hz -> 8 frames at 44100 Hz; endpoints preserved.
        let input = vec![0, 0, 1000, 1000, 2000, 2000, 3000, 3000];
        let out = resample_linear_stereo(&input, 22050, 44100);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[..2], &[0, 0]);
        assert_eq!(&out[14..], &[3000, 3000]);
        // Midpoint between frame 0 and 1 lands exactly between samples.
        assert_eq!(&out[2..4], &[500, 500]);
    }

    #[test]
    fn resample_downsample_halves_frame_count() {
        let input = vec![0, 0, 1000, 1000, 2000, 2000, 3000, 3000];
        let out = resample_linear_stereo(&input, 44100, 22050);
        assert_eq!(out.len(), 4);
        assert_eq!(&out[..2], &[0, 0]);
    }

    #[test]
    fn i16_to_f32_scales_to_unit_range() {
        let out = i16_to_f32_stereo(&[32767, -32768, 0]);
        assert!((out[0] - 32767.0 / 32768.0).abs() < 1e-6);
        assert_eq!(out[1], -1.0);
        assert_eq!(out[2], 0.0);
    }

    #[test]
    fn decoder_reports_its_backend_name() {
        assert_eq!(SymphoniaDecoder::new().name(), "symphonia");
    }

    #[test]
    fn decoder_rejects_empty_input() {
        assert!(SymphoniaDecoder::new().decode(&[]).is_err());
    }

    #[test]
    fn decoder_rejects_garbage_without_audio_frames() {
        // Plausible non-audio payload: must error, never panic or return
        // silence disguised as a track.
        let garbage = vec![0xAA; 4096];
        assert!(SymphoniaDecoder::new().decode(&garbage).is_err());
    }

    #[test]
    fn push_stereo_frame_maps_channel_layouts() {
        let mut out = Vec::new();
        push_stereo_frame(&[7, 8, 9], 1, &mut out);
        assert_eq!(out, vec![7, 7, 8, 8, 9, 9]);
        let mut out = Vec::new();
        push_stereo_frame(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, vec![1, 2, 3, 4]);
        // Surround keeps L/R of each frame, drops the rest.
        let mut out = Vec::new();
        push_stereo_frame(&[1, 2, 3, 4, 5, 6], 3, &mut out);
        assert_eq!(out, vec![1, 2, 4, 5]);
    }

    /// 1 second of stereo 16-bit 44.1 kHz 440 Hz sine, the raw PCM that the
    /// roundtrip test below feeds to LAME.
    fn sine_pcm_i16() -> Vec<i16> {
        let mut pcm = Vec::with_capacity(44100 * 2);
        for i in 0..44100 {
            let t = i as f32 / 44100.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 8000.0;
            pcm.push(s as i16);
            pcm.push(s as i16);
        }
        pcm
    }

    /// End-to-end decode of a real MP3: needs LAME on PATH. Run with
    /// `cargo test -- --ignored decode_real_mp3`.
    #[test]
    #[ignore = "requires lame on PATH"]
    fn decode_real_mp3_produces_bus_pcm() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let raw: Vec<u8> = sine_pcm_i16()
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut child = Command::new("lame")
            .args(["-r", "-s", "44.1", "-m", "s", "-b", "128", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("lame must be on PATH for this test");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(&raw)
            .expect("write PCM to lame");
        let output = child.wait_with_output().expect("read lame output");
        assert!(output.status.success(), "lame failed to encode");
        assert!(!output.stdout.is_empty(), "lame produced no MP3");

        let decoded = SymphoniaDecoder::new()
            .decode(&output.stdout)
            .expect("decode MP3");
        // 1s at 44100 Hz, plus the MP3 encoder delay/padding tail (~2112
        // samples) that survives decoding.
        assert!(
            (44100..=(44100 + 5000)).contains(&decoded.frames()),
            "unexpected frame count: {}",
            decoded.frames()
        );
        let samples = PcmBuffer::from_interleaved(decoded.samples().to_vec());
        let energy: f32 = samples.samples().iter().map(|s| s * s).sum();
        let rms = (energy / samples.samples().len() as f32).sqrt();
        assert!(rms > 0.05, "decoded sine must not be silent (rms={rms})");
        assert!(
            samples.samples().iter().all(|s| *s >= -1.0 && *s <= 1.0),
            "samples must stay in unit range"
        );
    }

    /// Decode of a 10-second real MP3 from disk (`DEEZCO_BENCH_MP3`):
    /// proves the backend handles full-size tracks, not just 1 s synthetic
    /// fixtures. Run with e.g.
    /// `DEEZCO_BENCH_MP3=/tmp/test10.mp3 cargo test -- --ignored decode_full_track_mp3`.
    #[test]
    #[ignore = "requires a real MP3 file on disk"]
    fn decode_full_track_mp3_produces_audible_pcm() {
        let path = std::env::var("DEEZCO_BENCH_MP3").expect("DEEZCO_BENCH_MP3");
        let mp3 = std::fs::read(&path).expect("read bench MP3");
        let decoded = SymphoniaDecoder::new()
            .decode(&mp3)
            .expect("decode full-track MP3");
        assert!(decoded.frames() > 44100, "must decode minutes of audio");
        let energy: f32 = decoded.samples().iter().map(|s| s * s).sum();
        let rms = (energy / decoded.samples().len() as f32).sqrt();
        assert!(rms > 0.01, "decoded track must not be silent (rms={rms})");
    }

    #[test]
    fn f32_to_s16_scales_and_clamps() {
        assert_eq!(f32_to_s16_stereo(&[1.0, -1.0, 0.0]), vec![32767, -32767, 0]);
        // Out-of-range input hard-clips instead of wrapping.
        assert_eq!(f32_to_s16_stereo(&[2.0, -2.0]), vec![32767, -32767]);
    }

    #[test]
    fn wav_header_describes_stereo_16bit_pcm() {
        let header = wav_header(176400, 44100);
        assert_eq!(header.len(), 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(&header[12..16], b"fmt ");
        assert_eq!(
            u16::from_le_bytes([header[20], header[21]]),
            1,
            "PCM format"
        );
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 2, "channels");
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            44100,
            "sample rate"
        );
        assert_eq!(&header[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([header[40], header[41], header[42], header[43]]),
            176400,
            "data length"
        );
    }

    #[test]
    fn lame_encoder_validates_bitrate_before_spawning() {
        assert!(LameEncoder::new(128).is_ok());
        assert!(LameEncoder::new(8).is_ok());
        assert!(LameEncoder::new(320).is_ok());
        assert!(LameEncoder::new(0).is_err());
        assert!(LameEncoder::new(7).is_err());
        assert!(LameEncoder::new(321).is_err());
        assert_eq!(LameEncoder::new(96).unwrap().bitrate_kbps(), 96);
        assert_eq!(LameEncoder::new(96).unwrap().name(), "lame");
    }

    #[test]
    fn lame_encoder_rejects_empty_pcm_without_spawning() {
        let pcm = PcmBuffer::new();
        assert!(LameEncoder::new(128).unwrap().encode(&pcm).is_err());
    }

    /// PCM -> MP3 smoke test: needs LAME on PATH. Run with
    /// `cargo test -- --ignored encode_pcm_to_mp3`.
    #[test]
    #[ignore = "requires lame on PATH"]
    fn encode_pcm_to_mp3_produces_mp3_frames() {
        assert!(LameEncoder::is_available(), "lame must be on PATH");
        let pcm = PcmBuffer::from_interleaved(i16_to_f32_stereo(&sine_pcm_i16()));
        let mp3 = LameEncoder::new(128).unwrap().encode(&pcm).unwrap();
        assert!(!mp3.is_empty(), "lame produced no output");
        // MP3 frame sync (11 set bits), no ID3 tag requested.
        assert_eq!(mp3[0], 0xFF);
        assert_eq!(mp3[1] & 0xE0, 0xE0, "second byte must carry frame sync");
    }

    /// Full bus loop PCM -> MP3 -> PCM: needs LAME on PATH. Run with
    /// `cargo test -- --ignored bus_roundtrip`.
    #[test]
    #[ignore = "requires lame on PATH"]
    fn bus_roundtrip_preserves_audible_sine() {
        assert!(LameEncoder::is_available(), "lame must be on PATH");
        let pcm = PcmBuffer::from_interleaved(i16_to_f32_stereo(&sine_pcm_i16()));
        let mp3 = LameEncoder::new(128).unwrap().encode(&pcm).unwrap();
        let back = SymphoniaDecoder::new()
            .decode(&mp3)
            .expect("decode the encoded MP3");
        assert!(
            (44100..=(44100 + 5000)).contains(&back.frames()),
            "unexpected frame count: {}",
            back.frames()
        );
        let energy: f32 = back.samples().iter().map(|s| s * s).sum();
        let rms = (energy / back.samples().len() as f32).sqrt();
        assert!(
            rms > 0.05,
            "roundtripped sine must stay audible (rms={rms})"
        );
    }
}
