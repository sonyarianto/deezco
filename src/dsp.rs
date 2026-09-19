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
//! docks here in the follow-up (its config and bypass stub already define
//! the slot). The default path stays native MP3 passthrough with zero extra
//! dependencies; the chain only runs when the pipeline is active.

// Scaffolding allow: covers chain API surface that unit tests exercise
// but production constructs only on the pipeline path (e.g. the Stereo
// Tool stub); remove it as coverage converges.
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
/// Carries the config + a bypass stub so the CLI shape and the chain slot
/// are stable; the follow-up replaces `process` with the real streaming
/// subprocess (raw PCM over stdin/stdout, kept open across tracks so the
/// processor state — AGC, loudness history — survives track boundaries).
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
    /// The binary must exist to be usable; checked at startup on integration.
    pub fn binary_exists(&self) -> bool {
        self.binary.is_file()
    }
}

/// Bypass stub: advertises the chain slot, passes audio through untouched,
/// and warns once so a misconfigured `--stereo-tool` can never silently
/// degrade the stream. The follow-up fills in the subprocess here —
/// callers (`Producer`) do not change.
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
}

impl AudioProcessor for StereoToolProcessor {
    fn name(&self) -> &str {
        "stereo-tool"
    }

    fn process(&mut self, buf: &mut [f32]) -> Result<()> {
        // Phase 1: transparent bypass. The single warning makes the stub
        // visible in logs without spamming per-chunk.
        if !self.warned {
            self.warned = true;
            eprintln!(
                "deezco: stereo-tool stub active ({}): bypassing, PCM passes through unchanged",
                self.config.binary.display()
            );
        }
        let _ = buf;
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
    fn stereo_tool_stub_is_transparent_bypass() {
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
}
