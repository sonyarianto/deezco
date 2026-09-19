//! DSP chain for the Icecast source pipeline.
//!
//! Pipeline order (industry standard):
//!
//! ```text
//! MP3 bytes -> Decoder -> PCM f32 stereo @ bus rate -> Crossfader
//!   -> ProcessorChain (Gain -> StereoTool -> ...) -> Encoder -> Icecast
//! ```
//!
//! This module owns the `ProcessorChain` tap: gain is live, and Stereo Tool
//! runs here as a per-track subprocess with bypass fallback. The default
//! path stays native MP3 passthrough with zero extra dependencies; the
//! chain only runs when the pipeline is active.

// Scaffolding allow: covers chain API surface that unit tests exercise
// but production constructs only on the pipeline path (e.g. bypass-only
// configs); remove it as coverage converges.
#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::Result;

/// Audio processors operate on interleaved stereo `f32` PCM in the range
/// `[-1.0, 1.0]`, at the bus sample rate (see [`crate::audio::BUS_RATE`]).
/// Implementations must be `Send` because the producer drives the chain from
/// a background streaming task.
pub trait AudioProcessor: Send {
    /// Short human-readable name used in log lines.
    fn name(&self) -> &str;
    /// Process `buf` in place. `buf.len()` is always even (stereo frames).
    fn process(&mut self, buf: &mut [f32]) -> Result<()>;
    /// Reset stateful processors on track boundaries / reconnects.
    fn reset(&mut self) {}
}

/// Ordered DSP chain. Runs post-crossfade, pre-encode — the exact slot a
/// broadcast processor like Stereo Tool expects.
#[derive(Default)]
pub struct ProcessorChain {
    processors: Vec<Box<dyn AudioProcessor>>,
}

impl ProcessorChain {
    /// Empty chain: passthrough, zero cost.
    pub fn new() -> Self {
        Self {
            processors: Vec::new(),
        }
    }

    /// Append a processor to the end of the chain.
    pub fn push(&mut self, processor: impl AudioProcessor + 'static) {
        self.processors.push(Box::new(processor));
    }

    /// True when no processor is registered (native passthrough fast path).
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }

    /// Number of registered processors.
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// Names of the registered processors, in order (for startup logs).
    pub fn names(&self) -> Vec<&str> {
        self.processors.iter().map(|p| p.name()).collect()
    }

    /// Run the whole chain in order over `buf`.
    pub fn process(&mut self, buf: &mut [f32]) -> Result<()> {
        for processor in &mut self.processors {
            processor.process(buf)?;
        }
        Ok(())
    }

    /// Reset every stateful processor (e.g. after a reconnect).
    pub fn reset(&mut self) {
        for processor in &mut self.processors {
            processor.reset();
        }
    }
}

/// Simple static gain in decibels. Useful both as a real feature (trim level
/// into the encoder / Stereo Tool) and as the reference `AudioProcessor`
/// implementation proving the tap works end to end.
pub struct GainProcessor {
    gain: f32,
}

impl GainProcessor {
    /// `db` is clamped to a sane broadcast-trim range (-24..+24 dB).
    pub fn new(db: f32) -> Self {
        let db = db.clamp(-24.0, 24.0);
        Self {
            gain: 10_f32.powf(db / 20.0),
        }
    }

    /// Linear gain factor (1.0 = unity).
    pub fn factor(&self) -> f32 {
        self.gain
    }
}

impl AudioProcessor for GainProcessor {
    fn name(&self) -> &str {
        "gain"
    }

    fn process(&mut self, buf: &mut [f32]) -> Result<()> {
        // Unity gain is a no-op so the default path stays bit-transparent.
        if (self.gain - 1.0).abs() < f32::EPSILON {
            return Ok(());
        }
        for sample in buf.iter_mut() {
            *sample = (*sample * self.gain).clamp(-1.0, 1.0);
        }
        Ok(())
    }
}

/// Connection settings for the Thimeo Stereo Tool CLI (`stereo_tool_cmd_64`).
/// Failures bypass the track (never kill the stream); see `process`.
#[derive(Clone, Debug)]
pub struct StereoToolConfig {
    /// Path to the licensed `stereo_tool_cmd_64` binary.
    pub binary: PathBuf,
    /// Processor settings file (.sts); tool defaults apply when unset.
    pub settings: Option<PathBuf>,
    /// License key (visible in `ps aux` while running, like the official CLI).
    pub key: Option<String>,
    /// Sample rate (Hz) of the processing bus; must match the PCM bus rate.
    pub rate: u32,
}

impl StereoToolConfig {
    /// The binary must exist to be usable; checked at startup (fail-fast).
    pub fn binary_exists(&self) -> bool {
        self.binary.is_file()
    }

    /// CLI arguments for `stereo_tool_cmd_64`: quiet 16-bit stereo WAV at
    /// the bus rate, optional settings/license, WAV on stdin and stdout.
    /// A WAV container (not raw PCM) is required: with raw input the tool
    /// silently drops its internal latency buffer on flush (~8192 samples
    /// short, every track), which then trips the length check and bypasses.
    /// With WAV in -> WAV out the length matches exactly.
    /// Extracted so the exact invocation is unit-tested without the
    /// licensed binary.
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "-q".to_string(),
            "-b".to_string(),
            "16".to_string(),
            "-r".to_string(),
            self.rate.to_string(),
        ];
        if let Some(settings) = &self.settings {
            args.push("-s".to_string());
            args.push(settings.display().to_string());
        }
        if let Some(key) = &self.key {
            args.push("-k".to_string());
            args.push(key.clone());
        }
        args.push("-".to_string());
        args.push("-".to_string());
        args
    }
}

/// Wrap interleaved stereo `i16` in a 44-byte WAV container for the Stereo
/// Tool CLI. The tool documents `<infile>` as "WAV or PCM"; in practice raw
/// PCM output comes back ~8192 samples short (unflushed latency buffer),
/// while WAV in -> WAV out preserves the exact length.
fn encode_wav(samples: &[i16]) -> Vec<u8> {
    let mut wav = crate::audio::wav_header(samples.len() * 2, crate::audio::BUS_RATE);
    wav.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    wav
}

/// Parse the Stereo Tool's WAV output back to interleaved stereo `i16`.
/// Returns `None` when the output is not a 44-byte-header stereo 16-bit WAV
/// or the payload has an odd byte count.
fn decode_wav(data: &[u8]) -> Option<Vec<i16>> {
    if data.len() < 44 {
        return None;
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" || &data[36..40] != b"data" {
        return None;
    }
    let payload = &data[44..];
    let (chunks, remainder) = payload.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    Some(chunks.iter().map(|c| i16::from_le_bytes(*c)).collect())
}

/// Thimeo Stereo Tool processor: runs bus PCM through the licensed
/// `stereo_tool_cmd_64` CLI (16-bit stereo WAV in and out) once per track.
///
/// Two deliberate trade-offs, both logged:
/// - Per-track spawn (not one persistent process): fits the parallel
///   prefetch architecture, but processor state (AGC, loudness history)
///   resets at every track boundary.
/// - Any tool failure bypasses the track untouched instead of killing the
///   24/7 stream; a missing binary or wrong rate bypasses with a one-time
///   warning (startup already rejects both — this is defense in depth).
pub struct StereoToolProcessor {
    config: StereoToolConfig,
    warned: bool,
}

impl StereoToolProcessor {
    pub fn new(config: StereoToolConfig) -> Self {
        Self {
            config,
            warned: false,
        }
    }

    /// One-time warning helper; keeps bypasses visible without per-track spam.
    fn warn_once(&mut self, message: String) {
        if !self.warned {
            self.warned = true;
            crate::warn!("deezco: stereo-tool: {message}");
        }
    }

    /// Run 16-bit stereo PCM through the tool; returns the processed
    /// samples, or `None` when the tool failed (caller bypasses).
    /// Stdin writes run on a writer thread while stdout drains: a full
    /// track never fits in a pipe buffer, so sequential write-then-read
    /// would deadlock. Input is wrapped in a 44-byte WAV container and the
    /// output is parsed back as WAV, so the tool flushes its latency buffer
    /// and the sample count matches exactly (raw PCM loses ~8192 samples).
    fn run_tool(&self, input: &[i16]) -> Option<Vec<i16>> {
        use std::io::Write;
        use std::process::Stdio;

        let wav = encode_wav(input);
        let mut child = std::process::Command::new(&self.config.binary)
            .args(self.config.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let writer = std::thread::spawn(move || stdin.write_all(&wav));
        let output = child.wait_with_output().ok()?;
        // A dead writer (panic or broken pipe) means the tool is gone;
        // the status check below is the backstop for the rest.
        if !matches!(writer.join(), Ok(Ok(()))) {
            return None;
        }
        if !output.status.success() {
            crate::warn!(
                "deezco: stereo-tool failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        let Some(processed) = decode_wav(&output.stdout) else {
            crate::warn!("deezco: stereo-tool returned non-WAV output, bypassing");
            return None;
        };
        if processed.len() != input.len() {
            crate::warn!(
                "deezco: stereo-tool returned {} samples for {} in, bypassing",
                processed.len(),
                input.len()
            );
            return None;
        }
        Some(processed)
    }
}

impl AudioProcessor for StereoToolProcessor {
    fn name(&self) -> &str {
        "stereo-tool"
    }

    fn process(&mut self, buf: &mut [f32]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.config.binary_exists() {
            self.warn_once(format!(
                "binary not found ({}): bypassing, PCM passes through unchanged",
                self.config.binary.display()
            ));
            return Ok(());
        }
        if self.config.rate != crate::audio::BUS_RATE {
            self.warn_once(format!(
                "rate {} Hz != bus {} Hz: bypassing (resample the bus instead)",
                self.config.rate,
                crate::audio::BUS_RATE
            ));
            return Ok(());
        }
        let Some(processed) = self.run_tool(&crate::audio::f32_to_s16_stereo(buf)) else {
            // `run_tool` already logged the cause; keep the input untouched.
            return Ok(());
        };
        buf.copy_from_slice(&crate::audio::i16_to_f32_stereo(&processed));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        calls: usize,
        add: f32,
    }

    impl AudioProcessor for Probe {
        fn name(&self) -> &str {
            "probe"
        }

        fn process(&mut self, buf: &mut [f32]) -> Result<()> {
            self.calls += 1;
            for s in buf.iter_mut() {
                *s += self.add;
            }
            Ok(())
        }
    }

    #[test]
    fn empty_chain_is_passthrough() {
        let mut chain = ProcessorChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        let mut buf = [0.5, -0.5];
        chain.process(&mut buf).unwrap();
        assert_eq!(buf, [0.5, -0.5]);
    }

    #[test]
    fn chain_runs_processors_in_registration_order() {
        let mut chain = ProcessorChain::new();
        chain.push(Probe { calls: 0, add: 1.0 });
        chain.push(Probe {
            calls: 0,
            add: 10.0,
        });
        assert_eq!(chain.names(), vec!["probe", "probe"]);
        let mut buf = [0.0, 0.0];
        chain.process(&mut buf).unwrap();
        assert_eq!(buf, [11.0, 11.0]);
    }

    #[test]
    fn gain_unity_leaves_samples_untouched() {
        let mut gain = GainProcessor::new(0.0);
        assert!((gain.factor() - 1.0).abs() < 1e-6);
        let mut buf = [0.25, -0.75];
        gain.process(&mut buf).unwrap();
        assert_eq!(buf, [0.25, -0.75]);
    }

    #[test]
    fn gain_plus_6db_doubles_amplitude() {
        let mut gain = GainProcessor::new(6.0);
        let mut buf = [0.25, -0.25];
        gain.process(&mut buf).unwrap();
        assert!((buf[0] - 0.5).abs() < 0.01);
        assert!((buf[1] + 0.5).abs() < 0.01);
    }

    #[test]
    fn gain_clamps_to_prevent_hard_clip() {
        let mut gain = GainProcessor::new(24.0);
        let mut buf = [0.9, -0.9];
        gain.process(&mut buf).unwrap();
        assert!(buf[0] <= 1.0);
        assert!(buf[1] >= -1.0);
    }

    #[test]
    fn stereo_tool_missing_binary_bypasses_transparently() {
        let config = StereoToolConfig {
            binary: PathBuf::from("/opt/stereo_tool_cmd_64"),
            settings: None,
            key: None,
            rate: 44100,
        };
        assert!(!config.binary_exists());
        let mut tool = StereoToolProcessor::new(config);
        assert_eq!(tool.name(), "stereo-tool");
        let mut buf = [0.1, -0.2, 0.3, -0.4];
        tool.process(&mut buf).unwrap();
        assert_eq!(buf, [0.1, -0.2, 0.3, -0.4]);
    }

    #[test]
    fn stereo_tool_builds_the_documented_cli_invocation() {
        let config = StereoToolConfig {
            binary: PathBuf::from("/opt/stereo_tool_cmd_64"),
            settings: Some(PathBuf::from("/etc/stereo/audio.sts")),
            key: Some("KEY-123".to_string()),
            rate: 44100,
        };
        assert_eq!(
            config.args(),
            vec![
                "-q",
                "-b",
                "16",
                "-r",
                "44100",
                "-s",
                "/etc/stereo/audio.sts",
                "-k",
                "KEY-123",
                "-",
                "-"
            ]
        );
        let bare = StereoToolConfig {
            binary: PathBuf::from("stereo_tool_cmd_64"),
            settings: None,
            key: None,
            rate: 48000,
        };
        assert_eq!(bare.args(), vec!["-q", "-b", "16", "-r", "48000", "-", "-"]);
    }

    #[test]
    #[cfg(unix)]
    fn stereo_tool_wrong_rate_bypasses_without_spawning() {
        // The binary exists, so the unchanged audio proves the rate gate
        // fired before any subprocess.
        let config = StereoToolConfig {
            binary: coreutil("cat"),
            settings: None,
            key: None,
            rate: 48000,
        };
        assert!(config.binary_exists());
        let mut tool = StereoToolProcessor::new(config);
        let mut buf = [0.25, -0.25];
        tool.process(&mut buf).unwrap();
        assert_eq!(buf, [0.25, -0.25]);
    }

    /// Resolve a coreutils binary to an absolute path: `Command` searches
    /// `PATH` at spawn, but `binary_exists` (correctly) does not, so tests
    /// that must reach the subprocess need the real path.
    #[cfg(unix)]
    fn coreutil(name: &str) -> PathBuf {
        [format!("/bin/{name}"), format!("/usr/bin/{name}")]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{name} must exist for this test"))
    }
    /// A fake "processor" binary: ignores argv (so the real Stereo Tool
    /// flags pass through harmlessly) and echoes stdin to stdout like the
    /// tool's raw-PCM mode. Hermetic: only needs `/bin/sh` + `cat`.
    ///
    /// Written next to the test binary — not `temp_dir()`: CI runners have
    /// been observed refusing to exec files under `/tmp`, which turned this
    /// test red there while green locally. `target/debug/deps` is proven
    /// executable because the test itself runs from it.
    #[cfg(unix)]
    fn fake_processor() -> PathBuf {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::current_exe()
            .expect("test binary path")
            .parent()
            .expect("test binary dir")
            .to_path_buf();
        let path = dir.join(format!("deezco-fake-stereo-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create fake processor");
        file.write_all(b"#!/bin/sh\nexec cat\n")
            .expect("write fake processor");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake processor");
        // Pre-flight: prove the script actually executes here. The script
        // ignores argv, so this runs `cat --version` → exit 0. A failure
        // panics with the OS error instead of a mysterious bypass later.
        let status = std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("fake processor must execute");
        assert!(
            status.success(),
            "fake processor pre-flight failed: {status}"
        );
        path
    }

    /// Pipe-plumbing proof without the licensed binary: the fake processor
    /// echoes stdin to stdout. Values are chosen so f32→s16→f32 quantization
    /// visibly differs from the input — the `assert_ne` proves the
    /// subprocess ran (a bypass would leave the input bit-identical).
    #[test]
    #[cfg(unix)]
    fn stereo_tool_pipe_plumbing_roundtrips() {
        let binary = fake_processor();
        let config = StereoToolConfig {
            binary: binary.clone(),
            settings: None,
            key: None,
            rate: crate::audio::BUS_RATE,
        };
        assert!(config.binary_exists());
        let mut tool = StereoToolProcessor::new(config);
        let original = vec![0.1, -0.2, 0.3, -0.4];
        let mut buf = original.clone();
        tool.process(&mut buf).unwrap();
        let expected = crate::audio::i16_to_f32_stereo(&crate::audio::f32_to_s16_stereo(&original));
        assert_ne!(buf, original, "subprocess must have run");
        assert_eq!(buf, expected);
        let _ = std::fs::remove_file(&binary);
    }

    /// A tool that exits nonzero must leave the track untouched (bypass).
    /// The binary exists, so this exercises the failure path, not the
    /// missing-binary shortcut.
    #[test]
    #[cfg(unix)]
    fn stereo_tool_failing_binary_bypasses() {
        let config = StereoToolConfig {
            binary: coreutil("false"),
            settings: None,
            key: None,
            rate: crate::audio::BUS_RATE,
        };
        assert!(config.binary_exists());
        let mut tool = StereoToolProcessor::new(config);
        let mut buf = [0.3, -0.3];
        tool.process(&mut buf).unwrap();
        assert_eq!(buf, [0.3, -0.3]);
    }

    #[test]
    fn wav_wrapper_roundtrips_exact_sample_count() {
        // Regression test: raw PCM through the tool came back ~8192 samples
        // short (unflushed latency), tripping the length check into bypass.
        // WAV in -> WAV out must preserve the exact count.
        let samples: Vec<i16> = (0..4410).map(|i| (i % 32767) as i16).collect();
        let wav = encode_wav(&samples);
        assert_eq!(wav.len(), 44 + samples.len() * 2);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let back = decode_wav(&wav).expect("valid WAV must decode");
        assert_eq!(back, samples);
    }

    #[test]
    fn wav_decoder_rejects_non_wav_and_short_input() {
        assert!(decode_wav(&[]).is_none());
        assert!(decode_wav(&[0u8; 43]).is_none());
        assert!(decode_wav(&[0xAAu8; 100]).is_none());
        // Odd payload after a valid header is rejected.
        let mut wav = encode_wav(&[1, 2, 3, 4]);
        wav.push(0xFF);
        assert!(decode_wav(&wav).is_none());
    }
}
