//! PCM bus types for the Icecast source pipeline (Phase 1: design).
//!
//! The bus is the shared language every stage speaks:
//!
//! ```text
//! Decoder -> PcmBuffer (f32 stereo @ BUS_RATE) -> Crossfader
//!   -> dsp::ProcessorChain -> Encoder -> Icecast
//! ```
//!
//! Phase 1 ships the types plus the pure crossfade math (fully unit-tested,
//! no audio dependencies). Phase 2 plugs in the real `FrameDecoder` (MP3 ->
//! PCM, e.g. `minimp3`/`symphonia`) and `FrameEncoder` (PCM -> CBR MP3) behind
//! the traits below — the `Producer` wiring does not change again.

// Phase 1 scaffolding: this module is public bus API for Phase 2. Items not
// yet read by production code are covered here instead of per-item
// attributes; remove this allow as each stage gets wired (the compiler will
// point at what's left via the unit tests).
#![allow(dead_code)]

use anyhow::Result;

/// Sample rate (Hz) every stage resamples to before mixing. 44100 matches
/// Deezer's MP3 sources, so Phase 2 starts without a resampler; Stereo Tool
/// also accepts it natively.
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

/// Phase 2 tap: MP3 bytes -> [`PcmBuffer`]. Implementations decode, upmix to
/// stereo, and resample to [`BUS_RATE`]. Kept as a trait so the bus design
/// does not dictate the backend.
pub trait FrameDecoder: Send + Sync {
    /// Decoder name for logs (e.g. `"minimp3"`).
    fn name(&self) -> &str;
    /// Decode one whole track into bus PCM.
    fn decode(&self, mp3: &[u8]) -> Result<PcmBuffer>;
}

/// Whole-track MP3 -> bus PCM decoder backed by `minimp3` (C sources are
/// compiled in, so the binary keeps zero runtime dependencies).
///
/// Each frame is upmixed to stereo on the fly; the assembled track is
/// resampled to [`BUS_RATE`] only when the source rate differs (Deezer MP3s
/// are 44100 Hz, so the resampler is a rarely-hit fallback — linear
/// interpolation, upgradeable to `rubato` if a non-44.1k source ever shows
/// up in practice).
pub struct Mp3Decoder;

impl Mp3Decoder {
    /// Build a decoder (stateless; safe to share across prefetch tasks).
    pub fn new() -> Self {
        Self
    }

    /// Decode all frames of `mp3` into stereo `i16` at the source rate.
    /// Returns `(samples, source_rate)`.
    fn decode_frames(mp3: &[u8]) -> Result<(Vec<i16>, u32)> {
        if mp3.is_empty() {
            anyhow::bail!("cannot decode empty MP3 input");
        }
        let mut decoder = minimp3::Decoder::new(mp3);
        let mut stereo: Vec<i16> = Vec::new();
        let mut src_rate: Option<u32> = None;
        loop {
            match decoder.next_frame() {
                Ok(frame) => {
                    let rate = u32::try_from(frame.sample_rate).unwrap_or(BUS_RATE);
                    src_rate.get_or_insert(rate);
                    match frame.channels {
                        1 => upmix_mono_to_stereo(&frame.data, &mut stereo),
                        2 => stereo.extend_from_slice(&frame.data),
                        // Surround MP3 is vanishingly rare: keep L/R, drop
                        // the rest rather than breaking the stereo invariant.
                        n => {
                            let n = n.max(1);
                            for chunk in frame.data.chunks(n) {
                                if chunk.len() >= 2 {
                                    stereo.push(chunk[0]);
                                    stereo.push(chunk[1]);
                                }
                            }
                        }
                    }
                }
                Err(minimp3::Error::Eof) => break,
                Err(err) => {
                    anyhow::bail!("MP3 decode failed: {err}");
                }
            }
        }
        let Some(src_rate) = src_rate else {
            anyhow::bail!("MP3 contained no audio frames");
        };
        Ok((stereo, src_rate))
    }
}

impl Default for Mp3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder for Mp3Decoder {
    fn name(&self) -> &str {
        "minimp3"
    }

    fn decode(&self, mp3: &[u8]) -> Result<PcmBuffer> {
        let (stereo_i16, src_rate) = Self::decode_frames(mp3)?;
        let at_bus_rate = resample_linear_stereo(&stereo_i16, src_rate, BUS_RATE);
        Ok(PcmBuffer::from_interleaved(i16_to_f32_stereo(&at_bus_rate)))
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
fn i16_to_f32_stereo(input: &[i16]) -> Vec<f32> {
    input.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Phase 2 tap: [`PcmBuffer`] -> CBR MP3 bytes at one fixed bitrate for the
/// whole Icecast session (constant encoder settings — required so listeners
/// never hear a format switch mid-stream).
pub trait FrameEncoder: Send + Sync {
    /// Encoder name for logs (e.g. `"lame-128"`).
    fn name(&self) -> &str;
    /// Output bitrate in kbps.
    fn bitrate_kbps(&self) -> u32;
    /// Encode bus PCM to MP3.
    fn encode(&self, pcm: &PcmBuffer) -> Result<Vec<u8>>;
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
        assert_eq!(Mp3Decoder::new().name(), "minimp3");
    }

    #[test]
    fn decoder_rejects_empty_input() {
        assert!(Mp3Decoder::new().decode(&[]).is_err());
    }

    #[test]
    fn decoder_rejects_garbage_without_audio_frames() {
        // Plausible non-audio payload: must error, never panic or return
        // silence disguised as a track.
        let garbage = vec![0xAA; 4096];
        assert!(Mp3Decoder::new().decode(&garbage).is_err());
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

        let decoded = Mp3Decoder::new()
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
}
