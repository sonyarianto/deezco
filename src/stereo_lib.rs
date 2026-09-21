//! Persistent in-process Thimeo Stereo Tool via the `libStereoTool` shared
//! library (`Stereo_Tool_Generic_plugin.zip`, e.g. `libStereoTool_intel64.so`).
//!
//! Alternative to the CLI subprocess path (`dsp::StereoToolProcessor`), which
//! spawns `stereo_tool_cmd_64` per track:
//!
//! ```text
//! CLI path:  PCM f32 -> s16 WAV -> spawn -> pipe -> s16 -> f32 (state resets)
//! Lib path:  PCM f32 -> stereoTool_Process (in place, state optionally persists)
//! ```
//!
//! Wins of the lib path: no per-track spawn (`Creating processing objects...`
//! disappears), no pipe/WAV roundtrip, no f32→s16→f32 quantization, the
//! license key stays in process memory (never visible in `ps aux`), and
//! processor state (AGC, loudness history) can stay continuous across track
//! boundaries — the radio-like behavior. Throughput is still dominated by the
//! DSP math itself, so expect roughly the same ~2x-realtime speed, minus the
//! spawn/IPC overhead (~10 s saved on a 4-minute track).
//!
//! Threading: one `StereoLibHandle` is opened once in `stream()` and shared
//! by every prefetch task behind a blocking `std::sync::Mutex`. DSP runs
//! inside `spawn_blocking`, so the mutex never blocks the async pacer; DSP
//! calls serialize while decode/encode stay parallel.
//!
//! Latency: the library delays audio by a constant number of frames (query
//! with [`StereoLibHandle::latency_frames`], typically 50–100 ms). The delay
//! is left to flow into the stream — a constant offset, inaudible for radio —
//! instead of being trimmed per track, which would cause drift.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use libloading::{Library, Symbol};
use std::cell::RefCell;

thread_local! {
    static PROCESS_LABEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn set_process_label(label: String) {
    PROCESS_LABEL.with(|c| *c.borrow_mut() = Some(label));
}

pub fn clear_process_label() {
    PROCESS_LABEL.with(|c| *c.borrow_mut() = None);
}

fn current_label() -> Option<String> {
    PROCESS_LABEL.with(|c| c.borrow().clone())
}

/// `loadsave_type` for [`StereoLibHandle::load_preset`]: all settings except
/// configuration settings. Matches the header example
/// (`ID_SAVE_ALLSETTINGS`, verified against `ParameterEnum.h` with gcc).
const ID_SAVE_ALLSETTINGS: c_int = 20386;

/// Frames per `stereoTool_Process` call. The library accepts any block size,
/// but very large single calls (a whole 4-minute track = ~10 M frames) risk
/// internal buffer assumptions; 8192 keeps every call well inside the
/// documented streaming regime while adding negligible call overhead.
const PROCESS_BLOCK_FRAMES: usize = 8192;

/// Opaque C++ instance (`class gStereoTool`); never dereferenced, only passed
/// back to library functions.
enum Opaque {}

/// Redirect fd 2 (stderr) to `/dev/null` until the guard drops. The library
/// probes ALSA/JACK soundcards during load/init; on a headless server that
/// spams dozens of `cannot find card '0'` lines that drown the actual log.
/// Only the noisy C init runs under the guard — our own Rust log calls stay
/// outside it. Panic-safe via `Drop`. No-op on non-Unix platforms.
#[cfg(unix)]
struct StderrGuard {
    saved: std::os::raw::c_int,
}

#[cfg(unix)]
impl StderrGuard {
    fn redirect() -> Self {
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        // SAFETY: plain dup/dup2/close bookkeeping on fd 2; every return
        // value is checked and the original fd is restored in `Drop`.
        let saved = unsafe { libc::dup(2) };
        if saved >= 0 {
            let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
            if null >= 0 {
                unsafe {
                    libc::dup2(null, 2);
                    libc::close(null);
                }
            }
        }
        Self { saved }
    }
}

#[cfg(unix)]
impl Drop for StderrGuard {
    fn drop(&mut self) {
        if self.saved >= 0 {
            // SAFETY: `saved` is a valid dup of the original stderr.
            unsafe {
                libc::dup2(self.saved, 2);
                libc::close(self.saved);
            }
        }
    }
}

type Create2Fn = unsafe extern "C" fn(has_gui: bool, key: *const c_char) -> *mut Opaque;
type DeleteFn = unsafe extern "C" fn(st: *mut Opaque);
type ProcessFn = unsafe extern "C" fn(
    st: *mut Opaque,
    samples: *mut f32,
    numsamples: i32,
    channels: i32,
    samplerate: i32,
);
type LoadPresetFn =
    unsafe extern "C" fn(st: *mut Opaque, filename: *const c_char, loadsave_type: c_int) -> bool;
type ResetFn = unsafe extern "C" fn(st: *mut Opaque, loadsave_type: c_int);
type GetLatency2Fn =
    unsafe extern "C" fn(st: *mut Opaque, samplerate: i32, feed_silence: bool) -> c_int;
type CheckLicenseFn = unsafe extern "C" fn(st: *mut Opaque) -> bool;
type EnableSoundCardFn = unsafe extern "C" fn(enabled: bool);
type GetVersionFn = unsafe extern "C" fn() -> c_int;
type UnlicensedFeaturesFn =
    unsafe extern "C" fn(st: *mut Opaque, text: *mut c_char, text_maxlen: c_int) -> bool;

/// Connection settings for the shared-library path. The `.sts` settings file
/// and license key are shared with the CLI flags; only the backend differs.
#[derive(Clone, Debug)]
pub struct StereoLibConfig {
    /// Path to `libStereoTool_*.so` (e.g. `libStereoTool_intel64.so`).
    pub lib: PathBuf,
    /// Processor settings file (.sts); tool defaults apply when unset.
    pub settings: Option<PathBuf>,
    /// License key, with or without the surrounding `<...>` (both work).
    pub key: Option<String>,
    /// Reset processor state on each track boundary, mimicking the CLI
    /// per-track spawn. Default `false` = continuous state across tracks.
    pub reset_per_track: bool,
}

impl StereoLibConfig {
    /// The library file must exist to be usable; checked at startup.
    pub fn lib_exists(&self) -> bool {
        self.lib.is_file()
    }
}

/// One persistent library instance. Opened once via [`StereoLibHandle::open`];
/// `Send` so it can live behind a mutex shared by prefetch tasks. All method
/// calls must happen under that mutex (the library instance is not
/// thread-safe on its own) and on blocking threads (processing is CPU-bound).
pub struct StereoLibHandle {
    /// Kept to prevent `dlclose` while function pointers are in use.
    _lib: Library,
    instance: *mut Opaque,
    delete: DeleteFn,
    process: ProcessFn,
    reset: ResetFn,
    get_latency2: GetLatency2Fn,
    check_license: CheckLicenseFn,
    /// Processing delay in frames per channel, measured once after preset
    /// load. Constant for the session; left to flow into the stream.
    latency_frames: usize,
    /// Software version reported by the library (e.g. `1105`).
    pub software_version: c_int,
    /// API version reported by the library.
    pub api_version: c_int,
}

// The instance is only ever touched under an external mutex on blocking
// threads; the raw pointer itself is never shared across threads unsafely.
unsafe impl Send for StereoLibHandle {}

impl StereoLibHandle {
    /// `dlopen` the library, create one instance (no GUI), load the `.sts`
    /// preset when given, and measure latency. Fails fast with a clear error
    /// so `stream()` never starts a 24/7 session on a broken backend.
    pub fn open(config: &StereoLibConfig) -> Result<Self> {
        // The library probes soundcards at load/init; on a headless server
        // that spams stderr with dozens of ALSA/JACK `cannot find card`
        // lines. Silence fd 2 for the noisy C init only — errors surface as
        // `Err` values (printed later by the caller), and our own license
        // log lines below stay outside the guard.
        #[cfg(unix)]
        let _quiet = StderrGuard::redirect();
        // SAFETY: `Library::new` is marked unsafe because a malicious `.so`
        // could run arbitrary constructors; the path comes from our own CLI
        // flag, same trust level as the `--stereo-tool` binary path.
        let lib = unsafe { Library::new(&config.lib) }
            .with_context(|| format!("cannot open stereo-tool lib {}", config.lib.display()))?;
        // Copy every function pointer out so the handle owns no borrows into
        // `lib` (self-referential `Symbol` guards would not compile here).
        let create2: Symbol<Create2Fn> = unsafe { lib.get(b"stereoTool_Create2") }
            .context("stereo-tool lib is missing stereoTool_Create2")?;
        let delete: Symbol<DeleteFn> = unsafe { lib.get(b"stereoTool_Delete") }
            .context("stereo-tool lib is missing stereoTool_Delete")?;
        let process: Symbol<ProcessFn> = unsafe { lib.get(b"stereoTool_Process") }
            .context("stereo-tool lib is missing stereoTool_Process")?;
        let load_preset: Symbol<LoadPresetFn> = unsafe { lib.get(b"stereoTool_LoadPreset") }
            .context("stereo-tool lib is missing stereoTool_LoadPreset")?;
        let reset: Symbol<ResetFn> = unsafe { lib.get(b"stereoTool_Reset") }
            .context("stereo-tool lib is missing stereoTool_Reset")?;
        let get_latency2: Symbol<GetLatency2Fn> = unsafe { lib.get(b"stereoTool_GetLatency2") }
            .context("stereo-tool lib is missing stereoTool_GetLatency2")?;
        let check_license: Symbol<CheckLicenseFn> =
            unsafe { lib.get(b"stereoTool_CheckLicenseValid") }
                .context("stereo-tool lib is missing stereoTool_CheckLicenseValid")?;
        let get_software: Symbol<GetVersionFn> =
            unsafe { lib.get(b"stereoTool_GetSoftwareVersion") }
                .context("stereo-tool lib is missing stereoTool_GetSoftwareVersion")?;
        let get_api: Symbol<GetVersionFn> = unsafe { lib.get(b"stereoTool_GetApiVersion") }
            .context("stereo-tool lib is missing stereoTool_GetApiVersion")?;
        let unlicensed: Symbol<UnlicensedFeaturesFn> =
            unsafe { lib.get(b"stereoTool_GetUnlicensedUsedFeatures") }
                .context("stereo-tool lib is missing stereoTool_GetUnlicensedUsedFeatures")?;
        // Optional: disable the internal soundcard so the library never
        // probes ALSA/JACK (a headless server has no soundcard; the probe
        // only spams stderr with "cannot find card" noise). Must run before
        // any instance is created, per the header docs. Missing symbol on
        // older builds is non-fatal — the probe noise returns, nothing else.
        let enable_soundcard: Option<Symbol<EnableSoundCardFn>> =
            unsafe { lib.get(b"stereoTool_EnableInternalSoundCard").ok() };
        if let Some(enable) = &enable_soundcard {
            // SAFETY: valid library symbol, plain bool argument.
            unsafe { enable(false) };
        }
        let (delete_fn, process, reset, get_latency2, check_license) =
            (*delete, *process, *reset, *get_latency2, *check_license);
        let (load_preset_fn, get_software, get_api, unlicensed) =
            (*load_preset, *get_software, *get_api, *unlicensed);

        // The key may arrive with or without `<...>`; the library prints its
        // own license warnings, and `check_license` below reports validity.
        let key_cstr;
        let key_ptr = match &config.key {
            Some(key) => {
                key_cstr =
                    CString::new(key.as_str()).context("stereo-tool key is not valid UTF-8")?;
                key_cstr.as_ptr()
            }
            None => std::ptr::null(),
        };
        // SAFETY: `create2` is a valid library symbol; `key_ptr` is either
        // null or points to a live `CString` for the duration of the call.
        let instance = unsafe { create2(false, key_ptr) };
        if instance.is_null() {
            bail!("stereo-tool lib returned a null instance (out of memory?)");
        }
        // From here every early return must delete the instance. A closure
        // cannot easily own it, so failures use explicit cleanup.
        let mut handle = Self {
            _lib: lib,
            instance,
            delete: delete_fn,
            process,
            reset,
            get_latency2,
            check_license,
            latency_frames: 0,
            software_version: unsafe { get_software() },
            api_version: unsafe { get_api() },
        };
        if let Some(sts) = &config.settings {
            let path = CString::new(sts.to_string_lossy().as_bytes())
                .context("stereo-tool settings path is not valid UTF-8")?;
            // SAFETY: instance is valid; `path` outlives the call.
            let ok = unsafe { load_preset_fn(instance, path.as_ptr(), ID_SAVE_ALLSETTINGS) };
            if !ok {
                // Disarm Drop (which would double-delete) before returning.
                // SAFETY: instance was created above and is deleted here.
                unsafe { delete_fn(instance) };
                handle.instance = std::ptr::null_mut();
                bail!("stereo-tool lib refused preset {}", sts.display());
            }
        }
        // SAFETY: instance is valid and the preset (if any) is loaded.
        handle.latency_frames =
            unsafe { (handle.get_latency2)(instance, crate::audio::BUS_RATE as i32, true) }.max(0)
                as usize;
        // Noisy C init is over: restore stderr before our own log lines.
        #[cfg(unix)]
        drop(_quiet);
        // License problems do not fail the open: unlicensed processing still
        // runs (with voice-overs/beeps), exactly like the CLI path, but the
        // operator must see it in the log.
        // SAFETY: instance is valid; some audio may be needed before the
        // check is reliable, so a negative here is advisory only.
        if unsafe { (handle.check_license)(instance) } {
            crate::info!("deezco: stereo-tool-lib license OK");
        } else {
            let mut text = vec![0 as c_char; 1024];
            // SAFETY: `text` is a valid 1024-byte buffer for the call.
            let clean = unsafe { unlicensed(instance, text.as_mut_ptr(), 1024) };
            let message = unsafe { CStr::from_ptr(text.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            if clean {
                crate::warn!(
                    "deezco: stereo-tool-lib license not (yet) verified; unlicensed output adds voice-overs/beeps"
                );
            } else {
                crate::warn!("deezco: stereo-tool-lib license issue: {message}");
            }
        }
        Ok(handle)
    }

    /// Processing delay in frames per channel for the loaded preset.
    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    /// Process interleaved stereo `f32` in place, in bounded blocks so a
    /// whole multi-minute track never becomes a single gigantic FFI call.
    /// Must be called with the external mutex held.
    pub fn process_buffer(&mut self, buf: &mut [f32]) {
        let label = current_label();
        self.process_buffer_with_label(buf, label.as_deref());
    }

    /// Same as `process_buffer` but logs progress with a label (track/jingle
    /// title) so the `progress` line is unambiguous.
    pub fn process_buffer_with_label(&mut self, buf: &mut [f32], label: Option<&str>) {
        if buf.is_empty() {
            return;
        }
        debug_assert!(
            buf.len().is_multiple_of(2),
            "stereo-tool-lib input must be stereo frames"
        );
        let block = PROCESS_BLOCK_FRAMES * 2;
        let total_frames = buf.len() / 2;
        let total_blocks = total_frames.div_ceil(PROCESS_BLOCK_FRAMES);
        let mut last_log = std::time::Instant::now();
        let label_suffix = label.map(|l| format!(" \"{l}\"")).unwrap_or_default();
        for (idx, chunk) in buf.chunks_mut(block).enumerate() {
            // The library documents in-place processing with identical
            // in/out size; frames = samples / channels.
            let frames = (chunk.len() / 2) as i32;
            // SAFETY: instance is valid (opened, not deleted until Drop);
            // `chunk` is a live valid `f32` slice for the call; calls are
            // serialized by the caller's mutex.
            unsafe {
                (self.process)(
                    self.instance,
                    chunk.as_mut_ptr(),
                    frames,
                    2,
                    crate::audio::BUS_RATE as i32,
                );
            }
            // Progress every ~5s or at the end — cheap throttle, not per-block spam.
            // For a 3:35 track (9493260 frames, ~1159 blocks) this is ~20 lines in 107s.
            let is_last = idx + 1 == total_blocks;
            if is_last || last_log.elapsed().as_secs() >= 5 {
                let percent = (idx + 1) * 100 / total_blocks;
                if label_suffix.is_empty() {
                    crate::info!(
                        "deezco: stereo-tool-lib progress {percent}% ({}/{} blocks, {} frames)",
                        idx + 1,
                        total_blocks,
                        total_frames
                    );
                } else {
                    crate::info!(
                        "deezco: stereo-tool-lib progress {percent}%{} ({}/{} blocks, {} frames)",
                        label_suffix,
                        idx + 1,
                        total_blocks,
                        total_frames
                    );
                }
                last_log = std::time::Instant::now();
            }
        }
    }

    /// Reset stateful processing (AGC, loudness history) to power-on state.
    /// Called on track boundaries only when `--stereo-tool-reset-track` is
    /// set; otherwise state flows continuously across tracks. Must be called
    /// with the external mutex held.
    pub fn reset_state(&mut self) {
        // SAFETY: instance is valid; serialized by the caller's mutex.
        unsafe { (self.reset)(self.instance, ID_SAVE_ALLSETTINGS) };
    }
}

impl Drop for StereoLibHandle {
    fn drop(&mut self) {
        if !self.instance.is_null() {
            // SAFETY: instance was created by `open` and deleted at most once
            // (nulled here and on the preset-failure path above).
            unsafe { (self.delete)(self.instance) };
            self.instance = std::ptr::null_mut();
        }
    }
}

/// [`crate::dsp::AudioProcessor`] over a shared [`StereoLibHandle`]. One
/// instance is opened in `stream()`; every prefetch task's chain holds an
/// `Arc` clone, so all tracks flow through the same persistent processor.
pub struct StereoLibProcessor {
    shared: Arc<Mutex<StereoLibHandle>>,
    reset_per_track: bool,
}

impl StereoLibProcessor {
    /// Share one persistent instance across chains/tasks.
    pub fn shared(shared: Arc<Mutex<StereoLibHandle>>, reset_per_track: bool) -> Self {
        Self {
            shared,
            reset_per_track,
        }
    }
}

impl crate::dsp::AudioProcessor for StereoLibProcessor {
    fn name(&self) -> &str {
        "stereo-tool-lib"
    }

    fn process(&mut self, buf: &mut [f32]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut handle = self
            .shared
            .lock()
            .map_err(|_| anyhow::anyhow!("stereo-tool-lib mutex poisoned"))?;
        if self.reset_per_track {
            handle.reset_state();
        }
        handle.process_buffer(buf);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_config_reports_missing_file() {
        let config = StereoLibConfig {
            lib: PathBuf::from("/nonexistent/libStereoTool.so"),
            settings: None,
            key: None,
            reset_per_track: false,
        };
        assert!(!config.lib_exists());
        assert!(StereoLibHandle::open(&config).is_err());
    }

    #[test]
    fn lib_config_accepts_existing_file() {
        let exe = std::env::current_exe().expect("test binary path");
        let config = StereoLibConfig {
            lib: exe,
            settings: None,
            key: None,
            reset_per_track: true,
        };
        // The test binary exists, so the existence gate passes; the open
        // itself must fail (not a Stereo Tool library) without panicking.
        assert!(config.lib_exists());
        assert!(StereoLibHandle::open(&config).is_err());
    }

    /// End-to-end through the real `.so`: needs the library plus the
    /// licensed key. Run with
    /// `DEEZCO_STEREO_LIB=/path/to/libStereoTool_intel64.so cargo test -- --ignored stereo_lib_real`.
    /// Without the env var (or without the file) the test passes trivially
    /// so normal `cargo test` stays hermetic.
    #[test]
    #[ignore = "requires libStereoTool .so and license key"]
    fn stereo_lib_real_so_processes_audio() {
        let path = std::env::var("DEEZCO_STEREO_LIB").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
            format!("{home}/tools/stereo_tool/libStereoTool_intel64.so")
        });
        if !PathBuf::from(&path).is_file() {
            eprintln!("stereo-tool-lib not found at {path}, skipping");
            return;
        }
        let config = StereoLibConfig {
            lib: PathBuf::from(&path),
            settings: Some(PathBuf::from(
                std::env::var("DEEZCO_STEREO_STS").unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                    format!("{home}/tools/stereo_tool/audio.sts")
                }),
            )),
            key: std::env::var("DEEZCO_STEREO_KEY").ok(),
            reset_per_track: false,
        };
        let mut handle = StereoLibHandle::open(&config).expect("open lib");
        eprintln!(
            "stereo-tool-lib v{} api {} latency {} frames",
            handle.software_version,
            handle.api_version,
            handle.latency_frames()
        );
        // 1 s of 440 Hz sine: processing must preserve length and audibly
        // change samples (proving the library ran, not a bypass).
        let mut buf = Vec::with_capacity(44100 * 2);
        for i in 0..44100 {
            let t = i as f32 / 44100.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            buf.push(s);
            buf.push(s);
        }
        let before = buf.clone();
        handle.process_buffer(&mut buf);
        assert_eq!(buf.len(), before.len());
        let energy: f32 = buf.iter().map(|s| s * s).sum();
        assert!(energy > 1.0, "processed sine must not be silent");
        assert_ne!(buf, before, "library must have changed the audio");
    }
}
