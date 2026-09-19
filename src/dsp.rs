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

    /// CLI arguments for `stereo_tool_cmd_64`: quiet 16-bit raw PCM at the
    /// bus rate, optional settings/license, raw PCM on stdin and stdout.
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

/// Thimeo Stereo Tool processor: runs bus PCM through the licensed
/// `stereo_tool_cmd_64` CLI (16-bit raw stereo in and out) once per track.
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
            eprintln!("deezco: stereo-tool: {message}");
        }
    }

    /// Run 16-bit stereo PCM through the tool; returns the processed
    /// samples, or `None` when the tool failed (caller bypasses).
    /// Stdin writes run on a writer thread while stdout drains: a full
    /// track never fits in a pipe buffer, so sequential write-then-read
    /// would deadlock.
    fn run_tool(&self, input: &[i16]) -> Option<Vec<i16>> {
        use std::io::Write;
        use std::process::Stdio;

        let raw: Vec<u8> = input.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut child = std::process::Command::new(&self.config.binary)
            .args(self.config.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let writer = std::thread::spawn(move || stdin.write_all(&raw));
        let output = child.wait_with_output().ok()?;
        // A dead writer (panic or broken pipe) means the tool is gone;
        // the status check below is the backstop for the rest.
        if !matches!(writer.join(), Ok(Ok(()))) {
            return None;
        }
        if !output.status.success() {
            eprintln!(
                "deezco: stereo-tool failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        let (chunks, remainder) = output.stdout.as_chunks::<2>();
        if !remainder.is_empty() {
            eprintln!("deezco: stereo-tool returned an odd byte count, bypassing");
            return None;
        }
        let processed: Vec<i16> = chunks.iter().map(|c| i16::from_le_bytes(*c)).collect();
        if processed.len() != input.len() {
            eprintln!(
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
    #[cfg(unix)]
    fn fake_processor() -> PathBuf {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("deezco-fake-stereo-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create fake processor");
        file.write_all(b"#!/bin/sh\nexec cat\n")
            .expect("write fake processor");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake processor");
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
}
