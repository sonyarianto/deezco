//! Loudness normalization (ITU-R BS.1770-4 / EBU R128) for the stream.
//!
//! Signal path: K-weighting (high shelf + high-pass, RBJ biquads at the bus
//! rate) → 400 ms blocks with 75% overlap → BS.1770 gating (absolute
//! −70 LUFS, relative −10 LU) → integrated LUFS → correction gain toward
//! the configured target, clamped to ±[`MAX_GAIN_DB`].
//!
//! Ported from CrabBoss (`crabboss/crates/core/src/audio/loudness.rs`,
//! including the `Biquad` core and the K-weighting coefficients), with two
//! deliberate differences: the analyzer takes bus PCM instead of decoding
//! a file (deezco already holds decoded audio in the prefetch task), and
//! the target travels per invocation instead of a hardcoded default.
//! Recommended targets (sibling convention): −9 hot, −14 streaming,
//! −23 broadcast floor.

/// Adjustable target range: broadcast floor … hot ceiling.
pub const TARGET_MIN_LUFS: f32 = -23.0;
/// Adjustable target range: broadcast floor … hot ceiling.
pub const TARGET_MAX_LUFS: f32 = -6.0;

/// Safety clamp on applied correction (±24 dB covers practically anything).
pub const MAX_GAIN_DB: f32 = 24.0;

/// RBJ "Cookbook" biquad, direct form 1. Only what K-weighting needs.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Build from raw (unnormalized) b0..a2 coefficients; a0-normalized.
    fn from_raw(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        let g = |v: f32| v / a0;
        Self {
            b0: g(b0),
            b1: g(b1),
            b2: g(b2),
            a1: g(a1),
            a2: g(a2),
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Process one sample with direct-form-1 state.
    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// BS.1770 K-weighting pre-filter (per channel state) at an arbitrary rate.
// Constants are the published BS.1770 coefficients (f64 precision kept
// verbatim on purpose; f32 truncation is irrelevant at audio rates).
#[allow(clippy::excessive_precision)]
fn k_weighting(rate: f32) -> [Biquad; 2] {
    // Stage 1: high shelf (+4 dB above ≈1.68 kHz) — tangent form as used by
    // libebur128. Verifiable at the band edges: H(0) = 1, H(π) = Vh.
    let vh = 10f32.powf(3.99984385397 / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let k = (std::f32::consts::PI * 1681.9744509555319 / rate).tan();
    let kq = k / 0.7071752369554196;
    let k2 = k * k;
    let shelf = Biquad::from_raw(
        vh + vb * kq + k2,
        2.0 * (k2 - vh),
        vh - vb * kq + k2,
        1.0 + kq + k2,
        2.0 * (k2 - 1.0),
        1.0 - kq + k2,
    );

    // Stage 2: high-pass, fc ≈ 38.13 Hz, Q ≈ 0.5003.
    let w0 = std::f32::consts::TAU * 38.13547087602444 / rate;
    let cw = w0.cos();
    let alpha = w0.sin() / (2.0 * 0.5003270373238773);
    let hp = Biquad::from_raw(
        (1.0 + cw) / 2.0,
        -(1.0 + cw),
        (1.0 + cw) / 2.0,
        1.0 + alpha,
        -2.0 * cw,
        1.0 - alpha,
    );

    [shelf, hp]
}

/// Running BS.1770 loudness meter. Feed every frame with [`push`](Self::push),
/// then read [`integrated_lufs`](Self::integrated_lufs).
pub struct LoudnessMeter {
    /// 100 ms hop length in frames.
    hop_frames: usize,
    /// Power sums (K-weighted) for the last 4 hops = one 400 ms block.
    recent: Vec<(f64, f64)>,
    /// Power sums of the hop currently being accumulated.
    cur: (f64, f64),
    frames_in_hop: usize,
    /// Channel power of every completed block (for gating).
    block_powers: Vec<f64>,
    k_l: [Biquad; 2],
    k_r: [Biquad; 2],
}

impl LoudnessMeter {
    /// Meter for `rate` Hz audio.
    pub fn new(rate: u32) -> Self {
        let hop_frames = ((rate as f32) * 0.1).round().max(1.0) as usize;
        Self {
            hop_frames,
            recent: Vec::new(),
            cur: (0.0, 0.0),
            frames_in_hop: 0,
            block_powers: Vec::new(),
            k_l: k_weighting(rate as f32),
            k_r: k_weighting(rate as f32),
        }
    }

    /// K-weight one channel sample through the 2-biquad chain.
    fn k_filter(chain: &mut [Biquad; 2], x: f32) -> f32 {
        let hp = chain[1].tick(x);
        chain[0].tick(hp)
    }

    /// Push one stereo frame.
    pub fn push(&mut self, l: f32, r: f32) {
        let kl = Self::k_filter(&mut self.k_l, l);
        let kr = Self::k_filter(&mut self.k_r, r);
        self.cur.0 += (kl as f64) * (kl as f64);
        self.cur.1 += (kr as f64) * (kr as f64);
        self.frames_in_hop += 1;
        if self.frames_in_hop >= self.hop_frames {
            self.recent.push(self.cur);
            self.cur = (0.0, 0.0);
            self.frames_in_hop = 0;
            if self.recent.len() > 4 {
                self.recent.remove(0);
            }
            if self.recent.len() == 4 {
                let (pl, pr): (f64, f64) = self
                    .recent
                    .iter()
                    .fold((0.0, 0.0), |(a, b), (l, r)| (a + l, b + r));
                self.block_powers
                    .push((pl + pr) / (4.0 * self.hop_frames as f64));
            }
        }
    }

    /// Loudness of one block power: −0.691 + 10·log₁₀(power).
    fn block_lufs(power: f64) -> f32 {
        (-0.691 + 10.0 * power.max(f64::EPSILON).log10()) as f32
    }

    /// Integrated (gated) loudness, or `None` for clips under 400 ms.
    pub fn integrated_lufs(&self) -> Option<f32> {
        if self.block_powers.is_empty() {
            return None;
        }
        // Absolute gate: discard blocks below −70 LUFS.
        let abs_gate: Vec<f64> = self
            .block_powers
            .iter()
            .copied()
            .filter(|p| Self::block_lufs(*p) > -70.0)
            .collect();
        if abs_gate.is_empty() {
            return None;
        }
        // Relative gate: mean of surviving blocks, minus 10 LU.
        let mean = |ps: &[f64]| ps.iter().sum::<f64>() / ps.len() as f64;
        let rel_threshold = Self::block_lufs(mean(&abs_gate)) - 10.0;
        let gated: Vec<f64> = abs_gate
            .into_iter()
            .filter(|p| Self::block_lufs(*p) > rel_threshold)
            .collect();
        if gated.is_empty() {
            return None;
        }
        Some(Self::block_lufs(mean(&gated)))
    }
}

/// Result of analyzing bus PCM toward a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessCorrection {
    /// Integrated loudness (gated) in LUFS.
    pub integrated_lufs: f32,
    /// Correction in dB that brings the audio to the target, clamped to
    /// ±[`MAX_GAIN_DB`].
    pub gain_db: f32,
}

/// Analyze interleaved stereo `f32` PCM at `rate` Hz toward `target` LUFS.
/// Returns `None` when nothing measurable survives gating (too short,
/// digital silence) — callers must skip correction then, never guess.
pub fn analyze(samples: &[f32], rate: u32, target: f32) -> Option<LoudnessCorrection> {
    let mut meter = LoudnessMeter::new(rate);
    // Bus PCM is always even-length stereo; a stray trailing sample would
    // unbalance the meter, so it is ignored rather than guessed.
    let (pairs, _) = samples.as_chunks::<2>();
    for pair in pairs {
        meter.push(pair[0], pair[1]);
    }
    let integrated = meter.integrated_lufs()?;
    if !integrated.is_finite() {
        return None;
    }
    Some(LoudnessCorrection {
        integrated_lufs: integrated,
        gain_db: (target - integrated).clamp(-MAX_GAIN_DB, MAX_GAIN_DB),
    })
}

/// Apply a correction in place (linear gain with hard-clip guard).
pub fn apply_correction(samples: &mut [f32], gain_db: f32) {
    let gain = 10f32.powf(gain_db / 20.0);
    if (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for s in samples.iter_mut() {
        *s = (*s * gain).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sine generator: 1 kHz at `amp`, `secs` long, stereo-identical.
    fn sine(amp: f32, secs: f32, rate: u32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let s = amp * (std::f32::consts::TAU * 1_000.0 * i as f32 / rate as f32).sin();
                [s, s]
            })
            .collect()
    }

    fn measure(samples: &[f32], rate: u32) -> Option<f32> {
        let mut m = LoudnessMeter::new(rate);
        let (pairs, _) = samples.as_chunks::<2>();
        for pair in pairs {
            m.push(pair[0], pair[1]);
        }
        m.integrated_lufs()
    }

    #[test]
    fn stereo_full_scale_sine_reads_zero_lufs() {
        // Stereo 1 kHz FS sine: K(1 kHz) ≈ +0.62 dB doubles per the two
        // channels → −0.691 + 10·log10(2·0.5·G) ≈ −0.07 LUFS.
        let lufs = measure(&sine(1.0, 3.0, 44_100), 44_100).unwrap();
        assert!((-0.6..=0.2).contains(&lufs), "got {lufs}");
    }

    #[test]
    fn mono_full_scale_sine_is_minus_3_lufs() {
        // ITU anchor: a mono 997 Hz FS sine measures −3.01 LUFS — this is
        // exactly what calibrates the −0.691 offset (K(997) ≈ +0.69 dB).
        let rate = 44_100u32;
        let n = rate as usize * 3;
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (std::f32::consts::TAU * 997.0 * i as f32 / rate as f32).sin();
            samples.push(s);
            samples.push(0.0); // silent right channel → mono-equivalent
        }
        let lufs = measure(&samples, rate).unwrap();
        assert!((-3.4..=-2.6).contains(&lufs), "got {lufs}");
    }

    #[test]
    fn quieter_sine_reads_lower() {
        let loud = measure(&sine(1.0, 3.0, 44_100), 44_100).unwrap();
        let quiet = measure(&sine(0.1, 3.0, 44_100), 44_100).unwrap();
        assert!((loud - quiet - 20.0).abs() < 0.5, "{loud} vs {quiet}");
    }

    #[test]
    fn gating_ignores_silence() {
        let rate = 44_100;
        let mut samples = sine(0.1, 4.0, rate);
        samples.extend(vec![0.0; rate as usize * 4]); // 4 s digital silence
        let gated = measure(&samples, rate).unwrap();
        let clean = measure(&sine(0.1, 4.0, rate), rate).unwrap();
        assert!(
            (gated - clean).abs() < 0.7,
            "silence must be gated: {gated} vs {clean}"
        );
    }

    #[test]
    fn analyze_points_at_target_with_rails() {
        // A −40 LUFS whisper toward −9 wants +31 dB → railed at +24.
        let quiet = analyze(&sine(0.01, 3.0, 44_100), 44_100, -9.0).unwrap();
        assert!((quiet.gain_db - 24.0).abs() < 1e-5, "got {}", quiet.gain_db);
        // Over-loud target gets negative correction, inside the rails.
        let hot = analyze(&sine(1.0, 3.0, 44_100), 44_100, TARGET_MIN_LUFS).unwrap();
        assert!(
            hot.gain_db <= 0.0,
            "hot target must attenuate: {}",
            hot.gain_db
        );
        assert!(hot.gain_db >= -MAX_GAIN_DB);
    }

    #[test]
    fn analyze_rejects_silence_and_short_clips() {
        assert!(analyze(&vec![0.0; 44100 * 2], 44_100, -9.0).is_none());
        assert!(analyze(&[], 44_100, -9.0).is_none());
        assert!(analyze(&[0.5, 0.5], 44_100, -9.0).is_none());
    }

    #[test]
    fn correction_roundtrip_lands_on_target() {
        // A −20 dBFS sine corrected toward −9 must measure ≈ −9 afterwards.
        let mut samples = sine(0.1, 3.0, 44_100);
        let first = analyze(&samples, 44_100, -9.0).unwrap();
        apply_correction(&mut samples, first.gain_db);
        let second = analyze(&samples, 44_100, -9.0).unwrap();
        assert!(
            (second.integrated_lufs - (-9.0)).abs() < 0.6,
            "got {}",
            second.integrated_lufs
        );
    }

    #[test]
    fn apply_correction_clamps_and_skips_unity() {
        let mut buf = [0.9, -0.9];
        apply_correction(&mut buf, 24.0);
        assert!(buf[0] <= 1.0 && buf[1] >= -1.0);
        let mut unity = [0.25, -0.25];
        apply_correction(&mut unity, 0.0);
        assert_eq!(unity, [0.25, -0.25]);
    }
}
