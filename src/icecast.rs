use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream;
use reqwest::header;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use crate::api::DeezerApi;
use crate::audio::{
    BUS_RATE, CrossfadeConfig, FrameDecoder, PcmBuffer, SessionEncoder, SymphoniaDecoder,
    nearest_bitrate, render_track_overlap,
};
use crate::download::{FetchedTrack, fetch_track_audio};
use crate::dsp::{GainProcessor, ProcessorChain, StereoToolProcessor};
use crate::files::is_audio_file;
use crate::models::{GwTrack, TrackFormat};
use crate::queue::{NextTrackError, TrackQueue};
use crate::track::{available_format, debug_enabled};
use rand::seq::IndexedRandom;

/// How many bytes of audio between in-band ICY metadata blocks. The source
/// picks the interval and tells Icecast about it via the `icy-metaint`
/// request header; 16000 is the widely used default.
const META_INTERVAL: usize = 16000;
/// Size of the audio chunks pushed to Icecast between pacing sleeps.
const CHUNK_SIZE: usize = 32768;
/// Pipeline path batches several encoder feeds into one TCP write (~0.5s at
/// 128 kbps). One feed is ~93ms / ~1.5KB — one packet per feed hits
/// Nagle/delayed-ACK worst case (up to ~40ms extra per chunk) and drifts the
/// stream slow until Icecast's queue drains (~1 min symptom). Batching 5-6x
/// restores large-write behaviour like native passthrough while keeping
/// latency under a second.
const PIPELINE_TARGET_BYTES: usize = 8192;
/// How long to wait before retrying after a transient failure.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// How long to wait before reconnecting after Icecast drops the source.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Cap for the exponential backoff between connection attempts, so a host
/// that keeps resetting connections (edge proxies, rate limiters) gets time
/// to clear instead of being hammered every few seconds.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);
/// How long silence filler lasts when used as a temporary track.
const FILLER_SILENCE_SECS: f32 = 2.0;
/// Avoid repeating the same jingle immediately.
const JINGLE_RECENT_WINDOW: usize = 3;

/// Scan a directory for audio files to use as filler jingles.
fn collect_jingle_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_audio_file(&path) {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Errors from a single source connection. Transient errors (drops,
/// unreachable server) are worth reconnecting; fatal ones (rejected by
/// Icecast) mean the config is wrong and the process should exit.
enum StreamError {
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

/// Icecast mount configuration, filled from CLI arguments.
pub struct IcecastConfig {
    pub server: String,
    pub mount: String,
    /// Source username for basic auth (usually `source`)
    pub username: String,
    pub password: String,
    /// Send track titles to listeners as ICY metadata blocks; some source
    /// proxies (e.g. caster.fm) reset the connection on metadata updates
    pub metadata: bool,
    pub name: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
    pub public: bool,
    pub playlist: String,
    /// Directory of filler jingles. When the playlist has no ready track
    /// a random file from here is played to keep the source alive.
    pub jingle_dir: Option<PathBuf>,
}

/// PCM-bus pipeline configuration.
///
/// When inactive (default) the audio path is native MP3 passthrough with
/// zero extra dependencies. When active — crossfade, loudness target, DSP
/// gain, or an explicit `--bitrate` — every track runs decode → loudnorm →
/// (crossfade) → DSP → CBR encode, so listeners hear one constant format
/// for the whole session.
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    /// Overlap between consecutive tracks (0 = hard cut).
    pub crossfade: CrossfadeConfig,
    /// R128 loudness target in LUFS. `None` disables normalization; `Some`
    /// measures each track and corrects toward the target before the mix.
    pub loudness: Option<f32>,
    /// Static DSP gain in dB, applied post-crossfade pre-encode.
    pub gain_db: Option<f32>,
    /// Session encode bitrate in kbps. `None` keeps the fetched format's
    /// native rate (320, or 128 on fallback); `Some` forces a transcode and
    /// activates the pipeline on its own.
    pub bitrate: Option<u32>,
    /// Thimeo Stereo Tool processing. Activates the pipeline on its own;
    /// runs after the gain stage, before the encoder.
    pub stereo_tool: Option<crate::dsp::StereoToolConfig>,
    /// In-process `libStereoTool` backend. Mutually exclusive with
    /// `stereo_tool` (enforced by the CLI); same chain slot.
    pub stereo_lib: Option<crate::stereo_lib::StereoLibConfig>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            crossfade: CrossfadeConfig::disabled(),
            loudness: None,
            gain_db: None,
            bitrate: None,
            stereo_tool: None,
            stereo_lib: None,
        }
    }
}

impl PipelineConfig {
    /// Build the post-crossfade processor chain for this config.
    /// Order is broadcast order: trim gain first, then Stereo Tool,
    /// then the encoder.
    pub fn build_chain(&self) -> ProcessorChain {
        let mut chain = ProcessorChain::new();
        if let Some(db) = self.gain_db
            && db != 0.0
        {
            chain.push(GainProcessor::new(db));
        }
        if let Some(config) = &self.stereo_tool {
            chain.push(StereoToolProcessor::new(config.clone()));
        }
        chain
    }

    /// True when any stage beyond native passthrough was requested.
    pub fn is_active(&self) -> bool {
        self.crossfade.is_enabled()
            || self.loudness.is_some()
            || self.gain_db.is_some_and(|db| db != 0.0)
            || self.bitrate.is_some()
            || self.stereo_tool.is_some()
            || self.stereo_lib.is_some()
    }

    /// The one CBR bitrate the whole session encodes at when the pipeline
    /// is active: the explicit override snapped to a discrete MPEG rate,
    /// else the fetched format's native rate (320 for lossless sources
    /// fetched as MP3). This is the exact advertised rate.
    pub fn encode_bitrate(&self, fetch_format: TrackFormat) -> u32 {
        nearest_bitrate(
            self.bitrate
                .unwrap_or_else(|| native_bitrate(fetch_format).unwrap_or(320)),
        ) as u32
    }
}

/// Out-of-band "now playing" title updates via Icecast's admin metadata
/// endpoint (`/admin/metadata?mode=updinfo`), used instead of in-stream ICY
/// blocks on hosts that reset the source connection on metadata updates
/// (e.g. caster.fm). Each update is a fresh short request on its own
/// connection, so the source stream is never disturbed.
#[derive(Clone)]
struct TitleUpdater {
    /// `http://host:port/admin/metadata`
    url_base: String,
    mount: String,
    username: String,
    password: String,
    client: reqwest::Client,
}

impl TitleUpdater {
    fn new(config: &IcecastConfig) -> Self {
        // Short leash: these fire-and-forget updates run on detached tasks,
        // so a hanging admin endpoint must fail instead of leaking tasks.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            url_base: format!("{}/admin/metadata", config.server.trim_end_matches('/')),
            mount: config.mount.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
            client,
        }
    }

    async fn update(&self, title: &str) -> Result<()> {
        let params = format!(
            "mode=updinfo&charset=UTF-8&mount={}&song={}",
            percent_encode(self.mount.as_bytes()),
            percent_encode(title.as_bytes())
        );
        let response = self
            .client
            .get(format!("{}?{params}", self.url_base))
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .context("Icecast title update request failed")?;
        if !response.status().is_success() {
            bail!("Icecast title update rejected: HTTP {}", response.status());
        }
        // Icecast answers HTTP 200 even when refusing the update; only the
        // body reveals the rejection.
        let text = response.text().await.unwrap_or_default();
        if text.contains("will not accept") {
            bail!("Icecast refused the title update: {}", text.trim());
        }
        Ok(())
    }
}

/// RFC 3986 percent-encoding (unreserved chars pass through, everything else
/// becomes uppercase `%XX`).
fn percent_encode(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// A full ICY metadata block: one length byte (16-byte units) followed by
/// `StreamTitle='...';` padded with null bytes.
fn icy_metadata_block(title: &str) -> Vec<u8> {
    let title = title.replace('\'', "");
    let meta = format!("StreamTitle='{title}';StreamUrl='';");
    let meta_len = meta.len().min(4080);
    let units = meta_len.div_ceil(16);
    let mut block = vec![units as u8];
    block.extend_from_slice(&meta.as_bytes()[..meta_len]);
    block.resize(1 + units * 16, 0);
    block
}

/// Send rate in bytes per second: paced to the track's real duration when
/// known, otherwise the nominal rate.
fn bytes_per_sec(data_len: usize, duration_secs: u64, nominal_bps: u64) -> u64 {
    if duration_secs > 0 {
        (data_len / duration_secs as usize).max(1) as u64
    } else {
        nominal_bps
    }
}

/// Nominal send rate in bytes per second for a format.
fn nominal_bytes_per_sec(format: TrackFormat) -> u64 {
    match format {
        TrackFormat::Flac => 1_000_000 / 8,
        TrackFormat::Mp3_320 => 320_000 / 8,
        TrackFormat::Mp3_128 => 128_000 / 8,
    }
}

/// MP3 bitrates streamable without transcoding.
fn native_bitrate(format: TrackFormat) -> Option<u32> {
    match format {
        TrackFormat::Mp3_320 => Some(320),
        TrackFormat::Mp3_128 => Some(128),
        TrackFormat::Flac => None,
    }
}

/// Download and decrypt audio for a track that is already known
/// (e.g. a retry after a CDN failure).
async fn prepare_track_audio(
    api: &DeezerApi,
    track: &GwTrack,
    fetch_format: TrackFormat,
) -> Result<FetchedTrack> {
    if debug_enabled() {
        crate::warn!(
            "[deezco-debug] preparing track \"{}\" for stream",
            track.display_name()
        );
    }
    let fetched = fetch_track_audio(api, track, fetch_format, false).await?;
    Ok(fetched)
}

/// Ordered handoff between consecutive prefetch tasks: the held PCM tail of
/// the previous track plus two sequence numbers. Tasks decode and DSP fully
/// in parallel; only the microsecond mix-and-publish step and the shared
/// libStereoTool invocations are ordered, so slow work never serializes
/// more than correctness requires.
#[derive(Default)]
struct XfadeShared {
    /// How many tracks have published their tail. Task `seq` may render once
    /// `version == seq`; the first track (seq 0) proceeds immediately.
    version: u64,
    /// How many tracks have finished the shared libStereoTool call. The lib
    /// instance is persistent (broadcast-continuous state), so invocations
    /// must follow stream order even though tasks run concurrently. Unused
    /// (stays 0) unless ordered DSP is armed.
    dsp_version: u64,
    /// Held tail of the last published track (interleaved stereo f32).
    tail: Vec<f32>,
}

/// Wait until every earlier track has published its tail. Polling (not
/// `Notify`) on purpose: a missed wakeup here would hang the stream, while
/// a 20 ms granularity is nothing next to a multi-minute track.
async fn claim_turn(shared: &Arc<Mutex<XfadeShared>>, seq: u64) {
    loop {
        if shared.lock().await.version == seq {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until every earlier track has finished its shared libStereoTool
/// call, keeping the persistent instance's state in stream order.
/// Same polling rationale as `claim_turn`.
async fn claim_dsp_turn(shared: &Arc<Mutex<XfadeShared>>, seq: u64) {
    loop {
        if shared.lock().await.dsp_version == seq {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// A failed task must still let both sequences move past it, or every later
/// task would wait forever. The held tail is left untouched: it still
/// belongs to the last good track, which is exactly the streamed boundary.
async fn advance_past(shared: &Arc<Mutex<XfadeShared>>, seq: u64) {
    let mut guard = shared.lock().await;
    guard.version = guard.version.max(seq + 1);
    guard.dsp_version = guard.dsp_version.max(seq + 1);
}

/// Everything one prefetch task needs. Bundled so `fetch_next_track` stays
/// a single argument and later stages can grow without reshuffling callers.
#[derive(Clone)]
struct TaskCtx {
    api: DeezerApi,
    queue: TrackQueue,
    fetch_format: TrackFormat,
    playlist: String,
    /// Position in activation order; gates the crossfade handoff.
    seq: u64,
    /// Session pipeline: config, resolved encode bitrate, shared handoff.
    runtime: PipelineRuntime,
}

/// Pipeline state shared by the `Producer` and every prefetch task.
#[derive(Clone)]
struct PipelineRuntime {
    pipeline: PipelineConfig,
    /// Session CBR resolved in `stream()` (override or native rate).
    encode_bps: u32,
    xfade: Arc<Mutex<XfadeShared>>,
    /// Persistent `libStereoTool` instance, opened once in `stream()`.
    /// `None` unless `--stereo-tool-lib` was given.
    stereo_lib: Option<Arc<std::sync::Mutex<crate::stereo_lib::StereoLibHandle>>>,
}

/// What one prefetch task delivers: native MP3 bytes straight through, or
/// bus PCM (decoded, leveled, mixed, DSP-processed) waiting on the session
/// encoder in the streaming path. Encoding itself is progressive and lives
/// with the `Producer`, never in a task, so no track boundary ever exists
/// at the MP3 level.
enum PreparedAudio {
    Native(Vec<u8>),
    Pcm(Vec<f32>),
}

/// Fetch the next track from the queue and prepare its audio: native MP3
/// passthrough by default, or decode → loudnorm → crossfade → DSP when the
/// pipeline is active (the session encoder downstream turns PCM into
/// continuous CBR). Slow CPU work runs on blocking threads inside the
/// background task, so track changes stay instant on the streaming path.
///
/// Every slow stage logs start/finish with timings: without them a
/// multi-minute Stereo Tool process looks like a hang (the exact symptom
/// this fixes — previously only the download had a debug line).
async fn fetch_next_track(ctx: TaskCtx) -> Result<(GwTrack, PreparedAudio)> {
    let track = match ctx.queue.next_track(&ctx.api, &ctx.playlist).await {
        Ok(track) => track,
        Err(err) => {
            // Queue failures must also advance the crossfade/DSP handoff,
            // otherwise every later task would wait forever on claim_turn.
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(match err {
                NextTrackError::Empty => anyhow::anyhow!("playlist has no playable tracks"),
                NextTrackError::Fetch(message) => anyhow::anyhow!("{message}"),
            });
        }
    };
    let fetched = match prepare_track_audio(&ctx.api, &track, ctx.fetch_format).await {
        Ok(fetched) => fetched,
        Err(err) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(err);
        }
    };
    if !ctx.runtime.pipeline.is_active() {
        return Ok((track, PreparedAudio::Native(fetched.data)));
    }
    let title = track.display_name();
    let mp3_len = fetched.data.len();
    // Source vs session: on a free account the fetch falls back (e.g. 320
    // -> 128) while the session CBR stays fixed, so an upscale (128 -> 320)
    // would waste bandwidth without adding quality. Surface it per track.
    let actual = available_format(&track, ctx.fetch_format);
    if actual != ctx.fetch_format {
        crate::info!(
            "deezco: source \"{title}\" is {actual} (fallback from {}) → session {} kbps CBR",
            ctx.fetch_format,
            ctx.runtime.encode_bps,
        );
    }
    if let Some(native) = native_bitrate(actual)
        && ctx.runtime.encode_bps > native
    {
        crate::warn!(
            "deezco: upscaling \"{title}\" ({actual} → {} kbps); consider --bitrate {native}",
            ctx.runtime.encode_bps,
        );
    }
    crate::info!("deezco: decoding \"{title}\" ({} bytes MP3)...", mp3_len);
    let decode_started = std::time::Instant::now();
    // Decode + loudness share one blocking hop: both are full-track CPU
    // (Symphonia decode, then BS.1770's ~16M biquad ticks for a 3-minute
    // track) that must never sit on an async worker.
    // A panicked/cancelled task must still advance the crossfade sequence,
    // or every later task would wait forever on `claim_turn`.
    let target = ctx.runtime.pipeline.loudness;
    let task_title = title.clone();
    let decode_join = tokio::task::spawn_blocking(move || -> Result<PcmBuffer> {
        let mut pcm = SymphoniaDecoder::new().decode(&fetched.data)?;
        // Loudness normalization (opt-in): measure this track's own
        // integrated LUFS and correct toward the target BEFORE the mix, so
        // the overlap blends already-leveled audio. Unmeasurable tracks
        // pass through.
        if let Some(target) = target {
            match crate::loudness::analyze(pcm.samples(), crate::audio::BUS_RATE, target) {
                Some(correction) => {
                    crate::info!(
                        "deezco: loudness \"{task_title}\": {:.1} LUFS -> {:+.1} dB (target {target:.0})",
                        correction.integrated_lufs,
                        correction.gain_db,
                    );
                    crate::loudness::apply_correction(pcm.samples_mut(), correction.gain_db);
                }
                None => {
                    crate::warn!("deezco: loudness \"{task_title}\": unmeasurable, passing through");
                }
            }
        }
        Ok(pcm)
    })
    .await;
    let pcm = match decode_join {
        Ok(Ok(pcm)) => {
            crate::info!(
                "deezco: decoded \"{title}\" in {:.1}s ({} frames)",
                decode_started.elapsed().as_secs_f32(),
                pcm.frames(),
            );
            pcm
        }
        Ok(Err(err)) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(err);
        }
        Err(join_err) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(anyhow::anyhow!("decoder task panicked: {join_err}"));
        }
    };
    // Ordered micro-step: mix the held tail, publish this track's own tail.
    // Encodes stay parallel — only this handoff is serialized.
    let out_pcm = if ctx.runtime.pipeline.crossfade.is_enabled() {
        claim_turn(&ctx.runtime.xfade, ctx.seq).await;
        let mut shared = ctx.runtime.xfade.lock().await;
        let (out, new_tail) = render_track_overlap(
            &shared.tail,
            pcm.into_samples(),
            ctx.runtime.pipeline.crossfade.overlap_frames(),
            ctx.runtime.pipeline.crossfade.curve,
        );
        shared.tail = new_tail;
        shared.version = ctx.seq + 1;
        out
    } else {
        pcm.into_samples()
    };
    let pipeline = ctx.runtime.pipeline.clone();
    let stereo_lib = ctx.runtime.stereo_lib.clone();
    let mut label_chain = pipeline.build_chain();
    if let Some(shared) = &stereo_lib {
        label_chain.push(crate::stereo_lib::StereoLibProcessor::shared(
            shared.clone(),
            pipeline
                .stereo_lib
                .as_ref()
                .is_some_and(|config| config.reset_per_track),
        ));
    }
    let chain_names = label_chain.names().join(" -> ");
    let dsp_label = if chain_names.is_empty() {
        "ready".to_string()
    } else {
        format!("DSP ({chain_names})")
    };
    let frames = out_pcm.len() / 2;
    crate::info!("deezco: {dsp_label} \"{title}\" ({frames} frames)...",);
    let process_started = std::time::Instant::now();
    // Ordered DSP for the shared libStereoTool instance: continuity
    // requires stream order even though prefetch tasks run concurrently.
    // The tail version above already moved, so only the DSP turn waits.
    let ordered_dsp = ctx.runtime.stereo_lib.is_some()
        && !ctx
            .runtime
            .pipeline
            .stereo_lib
            .as_ref()
            .is_some_and(|config| config.reset_per_track);
    if ordered_dsp {
        claim_dsp_turn(&ctx.runtime.xfade, ctx.seq).await;
    }
    // The version was already published above, so `advance_past` here is a
    // harmless no-op that only guards the non-crossfade path ordering.
    let dsp_join = tokio::task::spawn_blocking(move || {
        let mut chain = pipeline.build_chain();
        if let Some(shared) = &stereo_lib {
            chain.push(crate::stereo_lib::StereoLibProcessor::shared(
                shared.clone(),
                pipeline
                    .stereo_lib
                    .as_ref()
                    .is_some_and(|config| config.reset_per_track),
            ));
        }
        let mut out = out_pcm;
        chain.process(&mut out)?;
        Ok(out)
    })
    .await;
    // Advance the DSP turn on every path past the wait — success, DSP
    // failure, or a panicked worker — or the next task waits forever.
    // Encoding itself moved to the streaming path (session encoder), so
    // this task's slow work ends here and activation stays instant.
    if ordered_dsp {
        let mut shared = ctx.runtime.xfade.lock().await;
        shared.dsp_version = shared.dsp_version.max(ctx.seq + 1);
    }
    let data = match dsp_join {
        Ok(Ok(data)) => {
            crate::info!(
                "deezco: ready \"{title}\" in {:.1}s ({} frames PCM)",
                process_started.elapsed().as_secs_f32(),
                data.len() / 2,
            );
            data
        }
        Ok(Err(err)) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(err);
        }
        Err(join_err) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(anyhow::anyhow!("DSP task panicked: {join_err}"));
        }
    };
    Ok((track, PreparedAudio::Pcm(data)))
}

/// The audio track currently being pushed: native MP3 bytes straight
/// through, or bus PCM waiting on the session encoder.
enum CurrentAudio {
    Native { data: Vec<u8>, pos: usize },
    Pcm { samples: Vec<f32>, pos: usize },
}

/// A track on the wire, with pacing state. On the pipeline path the PCM
/// here is already mixed and DSP-processed; the session encoder below turns
/// it into continuous CBR progressively.
struct CurrentTrack {
    title: String,
    audio: CurrentAudio,
    bytes_per_sec: u64,
}

/// Background task that fetches and prepares the next track.
type PrefetchHandle = JoinHandle<Result<(GwTrack, PreparedAudio)>>;

/// How many stereo frames each progressive encode feed covers (~93 ms —
/// several MP3 frames per call, small enough to stay responsive).
const FEED_FRAMES: usize = 4096;

/// Outcome of one progressive encode feed. A free function (not a method)
/// so the encoder, the output buffer, and the track audio borrow as three
/// disjoint `&mut`s — a method on `Producer` could not split them.
enum FeedOutcome {
    /// Feed encoded; output appended (possibly empty on partial frames).
    Fed,
    /// Track PCM exhausted; the caller drops the track.
    TrackDone,
    /// Non-PCM audio reached the encoder; the caller drops the track loudly.
    Mismatch,
    /// Encoder failure; the caller drops the track loudly.
    EncodeFailed(String),
}

/// Encode one feed of the current track into the shared output buffer.
fn feed_encoder(
    encoder: &mut SessionEncoder,
    out: &mut Vec<u8>,
    audio: &mut CurrentAudio,
) -> FeedOutcome {
    let CurrentAudio::Pcm { samples, pos } = audio else {
        return FeedOutcome::Mismatch;
    };
    if *pos >= samples.len() {
        return FeedOutcome::TrackDone;
    }
    let end = (*pos + FEED_FRAMES * 2).min(samples.len());
    let bytes = match encoder.encode_chunk(&samples[*pos..end]) {
        Ok(bytes) => bytes.to_vec(),
        Err(err) => return FeedOutcome::EncodeFailed(err.to_string()),
    };
    *pos = end;
    out.extend_from_slice(&bytes);
    FeedOutcome::Fed
}

/// Produces the endless source body for Icecast: paced audio chunks with ICY
/// metadata blocks interleaved every `META_INTERVAL` bytes. The next track is
/// prefetched on a background task while the current one streams, so track
/// changes are seamless.
///
/// Two audio paths: native MP3 passthrough (default), or — when the pipeline
/// is active — prefetch tasks deliver processed PCM (decode → loudnorm →
/// crossfade → DSP) and the session encoder below turns the endless PCM
/// stream into continuous CBR. The encoder never resets between tracks, so
/// no boundary exists at the MP3 level: no fresh delay, no padding, no
/// reservoir restart — the splice click is gone by construction. ICY
/// framing counts output bytes on both paths, so metadata stays aligned.
struct Producer {
    api: DeezerApi,
    queue: TrackQueue,
    fetch_format: TrackFormat,
    metadata: bool,
    updater: Option<TitleUpdater>,
    nominal_bps: u64,
    playlist: String,
    /// Session pipeline config plus the shared crossfade handoff.
    runtime: PipelineRuntime,
    /// Session CBR encoder (`None` on native passthrough). Lives for the
    /// whole `Producer`: reconnects resume it, tracks never reset it.
    encoder: Option<SessionEncoder>,
    /// Encoder output bytes not yet pushed. Partial MP3 frames carry across
    /// track boundaries here — that carry-over IS the seamless splice.
    out: Vec<u8>,
    out_pos: usize,
    /// Sequence number for the next spawned prefetch task.
    next_seq: u64,
    /// Actual output bitrate in kbps. Native path: resolved per track after
    /// Deezer's quality fallback. Pipeline path: the session CBR, constant
    /// for every track. Advertised to Icecast so the server's reported
    /// bitrate matches what listeners actually receive.
    actual_kbps: Option<u32>,
    current: Option<CurrentTrack>,
    /// Audio bytes remaining until the next ICY metadata block. Counted
    /// continuously across track changes (Icecast measures the interval from
    /// the start of the connection, not per track) and re-aligned to
    /// `META_INTERVAL` at the start of every new source connection via
    /// `on_connected`, because a reconnect restarts Icecast's counter at 0.
    meta_remaining: usize,
    prefetch: VecDeque<PrefetchHandle>,
    /// Next wake time for paced audio. Deadline-based (not `sleep(duration)`
    /// per chunk): the deadline advances by exactly `take/rate` per chunk, so
    /// encode + TCP time is subtracted from the next sleep instead of
    /// accumulating as drift. `sleep_until` with a past deadline returns
    /// immediately (catch-up); a debt over 5s (stall/reconnect) resets.
    deadline: Option<Instant>,
    /// Jingle filler files from --jingle-dir, played when no real track is ready.
    jingle_files: Vec<PathBuf>,
    recent_jingles: VecDeque<PathBuf>,
    /// Encoder for native filler (silence/jingles) when pipeline is inactive.
    filler_encoder: Option<SessionEncoder>,
}

impl Producer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        api: DeezerApi,
        queue: TrackQueue,
        fetch_format: TrackFormat,
        metadata: bool,
        updater: Option<TitleUpdater>,
        playlist: String,
        runtime: PipelineRuntime,
        jingle_dir: Option<PathBuf>,
    ) -> Result<Self> {
        // CBR pipeline output paces exactly at the encode rate; native
        // passthrough falls back to the fetched format's nominal rate when
        // a track reports no duration.
        let nominal_bps = if runtime.pipeline.is_active() {
            u64::from(runtime.encode_bps) * 1000 / 8
        } else {
            nominal_bytes_per_sec(fetch_format)
        };
        // One encoder for the session, built before the first prefetch so a
        // bad bitrate fails here instead of mid-stream.
        let encoder = runtime
            .pipeline
            .is_active()
            .then(|| SessionEncoder::new(runtime.encode_bps))
            .transpose()?;
        // Native filler needs its own encoder (same rate) to generate valid
        // MP3 silence/jingles when there is no session encoder.
        let filler_encoder = if encoder.is_none() {
            let kbps = native_bitrate(fetch_format).unwrap_or(128);
            Some(SessionEncoder::new(kbps)?)
        } else {
            None
        };
        let jingle_files = jingle_dir
            .as_deref()
            .map(collect_jingle_files)
            .unwrap_or_default();
        if let Some(dir) = &jingle_dir {
            if jingle_files.is_empty() {
                crate::warn!(
                    "deezco: jingle-dir {} has no audio files (mp3/flac/ogg/wav); falling back to silence",
                    dir.display()
                );
            } else {
                crate::info!(
                    "deezco: jingle-dir {} with {} file(s)",
                    dir.display(),
                    jingle_files.len()
                );
            }
        }
        let mut prefetch = VecDeque::new();
        prefetch.push_back(tokio::spawn(fetch_next_track(TaskCtx {
            api: api.clone(),
            queue: queue.clone(),
            fetch_format,
            playlist: playlist.clone(),
            seq: 0,
            runtime: runtime.clone(),
        })));
        Ok(Self {
            api,
            queue,
            fetch_format,
            metadata,
            updater,
            nominal_bps,
            playlist,
            runtime,
            encoder,
            out: Vec::new(),
            out_pos: 0,
            next_seq: 1,
            actual_kbps: None,
            current: None,
            meta_remaining: META_INTERVAL,
            prefetch,
            deadline: None,
            jingle_files,
            recent_jingles: VecDeque::new(),
            filler_encoder,
        })
    }

    /// Advance the pacing deadline by `take` bytes at `bytes_per_sec`.
    /// Returns the new deadline; the next `next_chunk` sleeps until it.
    fn push_deadline(&mut self, prev: Option<Instant>, take: usize, bytes_per_sec: u64) {
        if take == 0 || bytes_per_sec == 0 {
            self.deadline = prev;
            return;
        }
        let duration = Duration::from_secs_f64(take as f64 / bytes_per_sec as f64);
        let now = Instant::now();
        let base = match prev {
            // Stall/reconnect debt over 5s: drop it and resume paced from now
            // instead of spinning without sleep trying to catch up forever.
            Some(t) if t + Duration::from_secs(5) < now => now,
            Some(t) => t,
            None => now,
        };
        self.deadline = Some(base + duration);
    }

    /// Ensure the first track is loaded and ready. Used before opening the
    /// Icecast connection so audio flows immediately upon registration:
    /// hosts like caster.fm drop silent sources within seconds.
    async fn warm_up(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        // Try to promote any ready prefetch quickly; if none ready within
        // 5s, fall back to filler (jingle/silence) so the Icecast source
        // can connect immediately instead of hanging until Deezer recovers.
        let mut attempts = 0;
        while attempts < 4 {
            let ready = self.prefetch.front().is_some_and(|h| h.is_finished());
            if !ready {
                break;
            }
            let handle = self.prefetch.pop_front().expect("ready front");
            while self.prefetch.len() < Self::PREFETCH_AHEAD {
                self.spawn_prefetch();
            }
            match handle.await {
                Ok(Ok((track, audio))) => {
                    self.activate_track(track, audio);
                    return Ok(());
                }
                Ok(Err(err)) => {
                    crate::warn!("deezco: warm_up track fetch failed: {err}; skipping");
                    attempts += 1;
                    continue;
                }
                Err(err) => {
                    crate::warn!("deezco: warm_up prefetch panicked: {err}; skipping");
                    attempts += 1;
                    self.spawn_prefetch();
                    continue;
                }
            }
        }
        let start = Instant::now();
        let timeout = Duration::from_secs(5);
        while start.elapsed() < timeout {
            while self.prefetch.len() < Self::PREFETCH_AHEAD {
                self.spawn_prefetch();
            }
            let ready = self.prefetch.front().is_some_and(|h| h.is_finished());
            if ready {
                let handle = self.prefetch.pop_front().expect("ready front");
                while self.prefetch.len() < Self::PREFETCH_AHEAD {
                    self.spawn_prefetch();
                }
                match handle.await {
                    Ok(Ok((track, audio))) => {
                        self.activate_track(track, audio);
                        return Ok(());
                    }
                    Ok(Err(err)) => {
                        crate::warn!("deezco: warm_up track fetch failed: {err}; skipping");
                        continue;
                    }
                    Err(err) => {
                        crate::warn!("deezco: warm_up prefetch panicked: {err}; skipping");
                        self.spawn_prefetch();
                        continue;
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        crate::warn!("deezco: warm_up no real track ready after 5s, using filler");
        self.load_filler().await;
        Ok(())
    }

    /// Spawn a background prefetch for the next track and push it to
    /// the prefetch queue. Sequence numbers follow activation order so the
    /// crossfade handoff stays aligned even though tasks run concurrently;
    /// the number is logged so double top-ups (e.g. the two spawns that
    /// fill an empty queue at startup) read as distinct tracks, not dupes.
    fn spawn_prefetch(&mut self) {
        let seq = self.next_seq;
        self.next_seq += 1;
        crate::warn!("deezco: prefetching track #{seq} in background");
        self.prefetch
            .push_back(tokio::spawn(fetch_next_track(TaskCtx {
                api: self.api.clone(),
                queue: self.queue.clone(),
                fetch_format: self.fetch_format,
                playlist: self.playlist.clone(),
                seq,
                runtime: self.runtime.clone(),
            })));
    }

    /// Number of tracks prefetched ahead of the current one.
    const PREFETCH_AHEAD: usize = 2;

    /// Activate prepared audio: print "now playing", update the Icecast
    /// title, top up the prefetch queue, and set `self.current`. Stays
    /// instant: heavy work (decode/DSP) already happened in the task, and
    /// encoding is progressive downstream.
    fn activate_track(&mut self, track: GwTrack, audio: PreparedAudio) {
        if self.runtime.pipeline.crossfade.is_enabled() {
            crate::info!(
                "deezco: now playing: \"{}\" (crossfade {:.1}s)",
                track.display_name(),
                self.runtime.pipeline.crossfade.duration_secs
            );
        } else {
            crate::info!("deezco: now playing: \"{}\"", track.display_name());
        }
        if let Some(updater) = &self.updater {
            let updater = updater.clone();
            let title = track.display_name();
            tokio::spawn(async move {
                // 404 is common on hosted Icecast proxies (e.g. caster.fm)
                // that don't expose /admin/metadata — best-effort, don't spam.
                if let Err(err) = updater.update(&title).await
                    && !err.to_string().contains("404")
                {
                    crate::warn!("deezco: title update failed: {err}");
                }
            });
        }
        // Top up the prefetch queue so there are always PREFETCH_AHEAD
        // tracks ready (or being fetched) ahead of the current one.
        // (Each spawn logs its own seq number; see `spawn_prefetch`.)
        while self.prefetch.len() < Self::PREFETCH_AHEAD {
            self.spawn_prefetch();
        }
        // Resolve the real output bitrate so the advertised rate matches what
        // listeners receive: the session CBR on the pipeline path, or the
        // per-track Deezer quality fallback on native passthrough.
        self.actual_kbps = Some(if self.runtime.pipeline.is_active() {
            self.runtime.encode_bps
        } else {
            native_bitrate(available_format(&track, self.fetch_format)).unwrap_or(128)
        });
        let (audio, bytes_per_sec) = match audio {
            PreparedAudio::Native(data) => {
                let rate = bytes_per_sec(data.len(), track.duration_secs(), self.nominal_bps);
                (CurrentAudio::Native { data, pos: 0 }, rate)
            }
            // CBR output paces exactly at the session rate; duration math
            // would only re-derive the same number through rounding.
            PreparedAudio::Pcm(samples) => (
                CurrentAudio::Pcm { samples, pos: 0 },
                u64::from(self.runtime.encode_bps) * 1000 / 8,
            ),
        };
        self.current = Some(CurrentTrack {
            title: track.display_name(),
            audio,
            bytes_per_sec,
        });
        // Note: `meta_remaining` intentionally lives on the Producer and is
        // carried across track changes — the ICY interval must stay
        // continuous for as long as the source connection is open.
    }

    /// Pick a random jingle that wasn't played recently.
    fn pick_jingle(&mut self) -> Option<PathBuf> {
        if self.jingle_files.is_empty() {
            return None;
        }
        let candidates: Vec<&PathBuf> = self
            .jingle_files
            .iter()
            .filter(|p| !self.recent_jingles.contains(p))
            .collect();
        let pool = if candidates.is_empty() {
            self.jingle_files.iter().collect::<Vec<_>>()
        } else {
            candidates
        };
        let choice = (*pool.choose(&mut rand::rng())?).clone();
        self.recent_jingles.push_back(choice.clone());
        if self.recent_jingles.len() > JINGLE_RECENT_WINDOW {
            self.recent_jingles.pop_front();
        }
        Some(choice)
    }

    /// Try to load a jingle file as the current track. Returns true on success.
    async fn load_jingle_file(&mut self, path: PathBuf) -> bool {
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => {
                crate::warn!("deezco: jingle {} is empty, skipping", path.display());
                return false;
            }
            Err(err) => {
                crate::warn!("deezco: failed to read jingle {}: {err}", path.display());
                return false;
            }
        };
        // Decode in blocking thread (Symphonia is CPU-bound).
        let decode = tokio::task::spawn_blocking(move || {
            SymphoniaDecoder::new()
                .decode(&bytes)
                .map(|buf| buf.into_samples())
        })
        .await;
        let pcm = match decode {
            Ok(Ok(samples)) if !samples.is_empty() => samples,
            Ok(Ok(_)) => {
                crate::warn!("deezco: jingle {} decoded to empty PCM", path.display());
                return false;
            }
            Ok(Err(err)) => {
                crate::warn!("deezco: failed to decode jingle {}: {err}", path.display());
                return false;
            }
            Err(err) => {
                crate::warn!(
                    "deezco: jingle decode task panicked for {}: {err}",
                    path.display()
                );
                return false;
            }
        };
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Jingle")
            .to_string();
        let display = format!("Jingle - {title}");
        crate::info!(
            "deezco: playing jingle \"{display}\" ({} frames)",
            pcm.len() / 2
        );
        if let Some(updater) = &self.updater {
            let updater = updater.clone();
            let t = display.clone();
            tokio::spawn(async move {
                if let Err(err) = updater.update(&t).await
                    && !err.to_string().contains("404")
                {
                    crate::warn!("deezco: jingle title update failed: {err}");
                }
            });
        }
        if self.encoder.is_some() {
            // Pipeline CBR: PCM goes through the session encoder.
            let bps = u64::from(self.runtime.encode_bps) * 1000 / 8;
            self.current = Some(CurrentTrack {
                title: display,
                audio: CurrentAudio::Pcm {
                    samples: pcm,
                    pos: 0,
                },
                bytes_per_sec: bps,
            });
            if self.actual_kbps.is_none() {
                self.actual_kbps = Some(self.runtime.encode_bps);
            }
        } else {
            // Native: encode PCM to MP3 via filler encoder so we get valid MP3 bytes.
            let Some(encoder) = self.ensure_filler_encoder() else {
                crate::warn!("deezco: filler encoder missing for jingle");
                return false;
            };
            let mut mp3 = Vec::new();
            let mut pos = 0;
            while pos < pcm.len() {
                let end = (pos + FEED_FRAMES * 2).min(pcm.len());
                match encoder.encode_chunk(&pcm[pos..end]) {
                    Ok(out) => mp3.extend_from_slice(out),
                    Err(err) => {
                        crate::warn!("deezco: jingle encode failed: {err}");
                        return false;
                    }
                }
                pos = end;
            }
            if mp3.is_empty() {
                crate::warn!("deezco: jingle {} produced no MP3 bytes", path.display());
                return false;
            }
            self.current = Some(CurrentTrack {
                title: display,
                audio: CurrentAudio::Native { data: mp3, pos: 0 },
                bytes_per_sec: self.nominal_bps,
            });
            if self.actual_kbps.is_none() {
                self.actual_kbps = Some((self.nominal_bps * 8 / 1000) as u32);
            }
        }
        true
    }

    /// Ensure native filler encoder exists and matches the current advertised rate.
    fn ensure_filler_encoder(&mut self) -> Option<&mut SessionEncoder> {
        let target_kbps = self
            .actual_kbps
            .unwrap_or((self.nominal_bps * 8 / 1000) as u32);
        let needs = match &self.filler_encoder {
            Some(enc) => enc.bitrate_kbps() != target_kbps,
            None => true,
        };
        if needs {
            match SessionEncoder::new(target_kbps) {
                Ok(enc) => self.filler_encoder = Some(enc),
                Err(err) => {
                    crate::warn!(
                        "deezco: failed to create filler encoder at {target_kbps} kbps: {err}"
                    );
                    return None;
                }
            }
        }
        self.filler_encoder.as_mut()
    }

    /// Load a short silence filler as the current track. Always succeeds.
    fn load_silence_filler(&mut self) {
        let frames = (BUS_RATE as f32 * FILLER_SILENCE_SECS) as usize;
        let pcm = vec![0.0; frames * 2];
        crate::warn!("deezco: no track ready, playing silence filler ({FILLER_SILENCE_SECS:.1}s)");
        if self.encoder.is_some() {
            let bps = u64::from(self.runtime.encode_bps) * 1000 / 8;
            self.current = Some(CurrentTrack {
                title: "Silence".to_string(),
                audio: CurrentAudio::Pcm {
                    samples: pcm,
                    pos: 0,
                },
                bytes_per_sec: bps,
            });
            if self.actual_kbps.is_none() {
                self.actual_kbps = Some(self.runtime.encode_bps);
            }
        } else {
            let Some(encoder) = self.ensure_filler_encoder() else {
                crate::warn!("deezco: filler encoder missing for silence");
                return;
            };
            let mut mp3 = Vec::new();
            let mut pos = 0;
            while pos < pcm.len() {
                let end = (pos + FEED_FRAMES * 2).min(pcm.len());
                match encoder.encode_chunk(&pcm[pos..end]) {
                    Ok(out) => mp3.extend_from_slice(out),
                    Err(err) => {
                        crate::warn!("deezco: silence encode failed: {err}");
                        return;
                    }
                }
                pos = end;
            }
            self.current = Some(CurrentTrack {
                title: "Silence".to_string(),
                audio: CurrentAudio::Native { data: mp3, pos: 0 },
                bytes_per_sec: self.nominal_bps,
            });
            if self.actual_kbps.is_none() {
                self.actual_kbps = Some((self.nominal_bps * 8 / 1000) as u32);
            }
        }
    }

    /// Try jingle, fall back to silence. Always leaves `self.current` ready.
    async fn load_filler(&mut self) {
        if let Some(path) = self.pick_jingle()
            && self.load_jingle_file(path).await
        {
            return;
        }
        self.load_silence_filler();
    }

    /// Load the next track: pop from the prefetch queue (or fetch directly
    /// when the queue drained) and top up the queue. On failure, the track
    /// is skipped and the next prefetched track is tried.
    #[allow(dead_code)]
    async fn load_next_track(&mut self) -> Result<()> {
        loop {
            let handle = if let Some(h) = self.prefetch.pop_front() {
                h
            } else {
                // Queue drained (e.g. after skips): spawn one through the
                // shared path so seq numbering and logging stay uniform.
                self.spawn_prefetch();
                self.prefetch.pop_front().expect("just spawned")
            };
            match handle.await.context("prefetch task panicked") {
                Ok(Ok((track, fetched))) => {
                    self.activate_track(track, fetched);
                    return Ok(());
                }
                Ok(Err(err)) => {
                    crate::warn!("deezco: track fetch failed: {err}; skipping");
                    // Keep the pipeline full so the next attempt doesn't block
                    // on a cold spawn (that would create a silent gap).
                    self.spawn_prefetch();
                    continue;
                }
                Err(err) => {
                    crate::warn!("deezco: prefetch task panicked: {err}; skipping");
                    self.spawn_prefetch();
                    continue;
                }
            }
        }
    }

    /// Current track title, for out-of-band updates after a reconnect (the
    /// server loses the title when the source drops).
    fn current_title(&self) -> Option<String> {
        self.current.as_ref().map(|current| current.title.clone())
    }

    /// Re-align the ICY metadata interval for a new source connection.
    /// Icecast counts audio bytes from the start of each connection, so the
    /// first metadata block must land exactly `META_INTERVAL` bytes into the
    /// new connection — even when a track resumes mid-interval after a
    /// reconnect. Without this, Icecast reads audio bytes as metadata and
    /// kills the source with "Incorrect metadata format, ending stream"
    /// within about a second of connecting.
    fn on_connected(&mut self) {
        self.meta_remaining = META_INTERVAL;
        // Fresh connection restarts Icecast's byte counter (see above) and
        // its playout buffer: drop any pacing debt from the reconnect sleep
        // so the first chunk sends immediately.
        self.deadline = None;
    }

    /// Next body chunk: audio (paced) or a metadata block. `None` never
    /// happens — errors are logged and retried so the stream never ends.
    /// Borrows are kept to single statements throughout: the loop mutates
    /// `self.current` (activate/skip), the encoder, and the output buffer
    /// in turns, never overlapping.
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, std::io::Error>> {
        let prev_deadline = self.deadline.take();
        if let Some(deadline) = prev_deadline {
            sleep_until(deadline).await;
        }
        loop {
            if self.current.is_none() {
                // Non-blocking promotion: if a prefetched track is ready, use it.
                // Otherwise play filler (jingle or silence) to keep the Icecast
                // source alive. The previous code blocked on `handle.await`
                // here with no bytes returned, so Icecast's silent-source
                // timeout killed the connection and listeners disconnected.
                let mut promoted = false;
                let mut attempts = 0;
                while attempts < 4 {
                    let ready = self.prefetch.front().is_some_and(|h| h.is_finished());
                    if !ready {
                        break;
                    }
                    let handle = self.prefetch.pop_front().expect("ready front just checked");
                    while self.prefetch.len() < Self::PREFETCH_AHEAD {
                        self.spawn_prefetch();
                    }
                    match handle.await {
                        Ok(Ok((track, audio))) => {
                            self.activate_track(track, audio);
                            promoted = true;
                            break;
                        }
                        Ok(Err(err)) => {
                            crate::warn!("deezco: track fetch failed: {err}; skipping");
                            attempts += 1;
                            continue;
                        }
                        Err(err) => {
                            crate::warn!("deezco: prefetch panicked: {err}; skipping");
                            self.spawn_prefetch();
                            attempts += 1;
                            continue;
                        }
                    }
                }
                if promoted {
                    continue;
                }
                // No ready real track — ensure queue stays full for next check
                // and emit filler so the TCP stream never stalls.
                while self.prefetch.len() < Self::PREFETCH_AHEAD {
                    self.spawn_prefetch();
                }
                // If everything is still pending (cold start, CDN stall, empty
                // playlist), `load_next_track` would have blocked for seconds
                // with no bytes out. Filler keeps bytes flowing.
                self.load_filler().await;
                continue;
            }
            if self.metadata && self.meta_remaining == 0 {
                self.meta_remaining = META_INTERVAL;
                let title = self
                    .current
                    .as_ref()
                    .map(|current| current.title.clone())
                    .unwrap_or_default();
                // Metadata is overhead, not audio: keep the audio deadline for
                // the next chunk instead of consuming it (it was already
                // waited above).
                self.deadline = prev_deadline;
                return Some(Ok(icy_metadata_block(&title)));
            }
            let bytes_per_sec = self
                .current
                .as_ref()
                .map(|current| current.bytes_per_sec)
                .unwrap_or(self.nominal_bps);
            // Pipeline path: keep the encoder output buffer filled from the
            // track's PCM. A partially-filled MP3 frame carries across track
            // boundaries in `self.out` — that carry-over is exactly what
            // makes the splice seamless. Native path serves bytes directly.
            if self.encoder.is_some() {
                let Some(encoder) = self.encoder.as_mut() else {
                    // Unreachable: checked above. Break loudly, never spin.
                    crate::warn!("deezco: pipeline encoder missing; skipping track");
                    self.current = None;
                    continue;
                };
                // Batch several ~93ms feeds into one ~0.5s TCP write (see
                // PIPELINE_TARGET_BYTES): one packet per feed hits
                // Nagle/delayed-ACK and drifts slow. Bounded (8 feeds max) so
                // a degenerate encoder can't stall the stream filling it.
                let mut feeds = 0;
                while self.out.len() - self.out_pos < PIPELINE_TARGET_BYTES && feeds < 8 {
                    let Some(current) = self.current.as_mut() else {
                        break;
                    };
                    match feed_encoder(encoder, &mut self.out, &mut current.audio) {
                        FeedOutcome::Fed => {
                            feeds += 1;
                        }
                        FeedOutcome::TrackDone => {
                            self.current = None;
                            break;
                        }
                        FeedOutcome::Mismatch => {
                            crate::warn!("deezco: pipeline state mismatch; skipping track");
                            self.current = None;
                            break;
                        }
                        FeedOutcome::EncodeFailed(err) => {
                            crate::warn!("deezco: encode failed: {err}; skipping track");
                            self.current = None;
                            break;
                        }
                    }
                }
                if self.out_pos >= self.out.len() {
                    // Track(s) exhausted with the encoder still buffering a
                    // partial frame: pull the next track, its samples will
                    // complete the frame with zero gap.
                    continue;
                }
                let take = if self.metadata {
                    self.meta_remaining
                        .min(CHUNK_SIZE)
                        .min(self.out.len() - self.out_pos)
                } else {
                    CHUNK_SIZE.min(self.out.len() - self.out_pos)
                };
                let chunk = self.out[self.out_pos..self.out_pos + take].to_vec();
                self.out_pos += take;
                self.meta_remaining -= take;
                // Compact the consumed prefix so the buffer stays bounded
                // across a whole session (carry-over is at most one feed).
                if self.out_pos >= self.out.len() {
                    self.out.clear();
                    self.out_pos = 0;
                } else if self.out_pos > 65_536 {
                    self.out.drain(..self.out_pos);
                    self.out_pos = 0;
                }
                self.push_deadline(prev_deadline, take, bytes_per_sec);
                return Some(Ok(chunk));
            }
            let Some(current) = self.current.as_mut() else {
                continue;
            };
            let CurrentAudio::Native { data, pos } = &mut current.audio else {
                // Unreachable: the encoder exists exactly when prefetch
                // tasks produce PCM. Drop loudly rather than spin.
                crate::warn!("deezco: pipeline state mismatch; skipping track");
                self.current = None;
                continue;
            };
            let take = if self.metadata {
                self.meta_remaining.min(CHUNK_SIZE).min(data.len() - *pos)
            } else {
                CHUNK_SIZE.min(data.len() - *pos)
            };
            let chunk = data[*pos..*pos + take].to_vec();
            let finished = *pos + take >= data.len();
            *pos += take;
            self.meta_remaining -= take;
            let bytes_per_sec = current.bytes_per_sec;
            if finished {
                self.current = None;
            }
            self.push_deadline(prev_deadline, take, bytes_per_sec);
            return Some(Ok(chunk));
        }
    }

    /// The bitrate advertised to Icecast, matching the actual output stream.
    fn advertised_kbps(&self) -> u32 {
        if let Some(kbps) = self.actual_kbps {
            return kbps;
        }
        (nominal_bytes_per_sec(self.fetch_format) * 8 / 1000) as u32
    }
}

/// Run one source connection: PUT the endless body to Icecast, then wait
/// until Icecast drops the connection. Returns when the connection ends.
/// The producer survives reconnects, so a dropped connection resumes the
/// buffered track instead of starting over from scratch.
async fn run_connection(
    producer: &Arc<Mutex<Producer>>,
    config: &IcecastConfig,
    updater: Option<&TitleUpdater>,
) -> Result<(), StreamError> {
    let url = format!("{}{}", config.server.trim_end_matches('/'), config.mount);
    // Wait until a track is fully ready (fetched, processed, encoded)
    // before registering the source: once connected, audio must flow
    // immediately or silent-source hosts drop the connection. When the
    // producer already holds a ready track (e.g. after a reconnect), this
    // returns instantly.
    let mut prepared = false;
    let advertised_kbps;
    let client;
    loop {
        let mut p = producer.lock().await;
        if p.current.is_none() && !prepared {
            crate::info!("deezco: preparing the first track before connecting");
            prepared = true;
        }
        match p.warm_up().await {
            Ok(()) => {
                advertised_kbps = p.advertised_kbps();
                crate::info!(
                    "deezco: streaming at {advertised_kbps} kbps (actual, after quality fallback)"
                );
                // The source body never ends: this client must not carry
                // the Deezer total request timeout (see `source_client`).
                client = p.api.source_client().clone();
                break;
            }
            Err(err) => {
                crate::warn!("deezco: first track not ready: {err}; retrying");
                drop(p);
                sleep(RETRY_DELAY).await;
            }
        }
    }
    // A fresh Icecast connection restarts the metadata-interval byte counter
    // at 0; re-align our counter so the first ICY metadata block lands at
    // exactly META_INTERVAL bytes into this connection (the body stream is
    // lazy, so this happens before any audio is pushed).
    producer.lock().await.on_connected();
    let body = stream::unfold(producer.clone(), |producer| async move {
        let chunk = {
            let mut p = producer.lock().await;
            p.next_chunk().await
        };
        chunk.map(|chunk| (chunk, producer))
    });
    let mut request = client
        .put(&url)
        .basic_auth(&config.username, Some(&config.password))
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header("ice-public", if config.public { "1" } else { "0" })
        .header("ice-bitrate", advertised_kbps.to_string());
    if config.metadata {
        request = request
            .header("icy-metaint", META_INTERVAL.to_string())
            .header("ice-metadata", "1");
    }
    if let Some(name) = &config.name {
        request = request.header("ice-name", name);
    }
    if let Some(genre) = &config.genre {
        request = request.header("ice-genre", genre);
    }
    if let Some(url) = &config.url {
        request = request.header("ice-url", url);
    }
    let request = request.body(reqwest::Body::wrap_stream(body));

    // Visible while the PUT handshake is in flight: without this, a server
    // that accepts TCP but never answers looks identical to a missing
    // "connected" log with no error.
    crate::info!("deezco: connecting to {url} as {} ...", config.username);
    let response = request.send().await.map_err(|err| {
        StreamError::Transient(anyhow::Error::new(err).context("Icecast connection failed"))
    })?;
    if !response.status().is_success() {
        return Err(StreamError::Fatal(anyhow::anyhow!(
            "Icecast rejected the source connection ({}); check the mount point and source password",
            response.status()
        )));
    }
    crate::info!("deezco: connected to {url} (listeners can tune in at {url})");
    if let Some(updater) = updater {
        let title = producer.lock().await.current_title();
        if let Some(title) = title {
            let updater = updater.clone();
            tokio::spawn(async move {
                if let Err(err) = updater.update(&title).await
                    && !err.to_string().contains("404")
                {
                    crate::warn!("deezco: title update failed: {err}");
                }
            });
        }
    }

    // Icecast keeps the response open for the whole stream; the body ends
    // only when the source connection is dropped.
    let mut response_body = response.bytes_stream();
    while let Some(chunk) = response_body.next().await {
        if let Err(err) = chunk {
            return Err(StreamError::Transient(
                anyhow::Error::new(err).context("Icecast source connection error"),
            ));
        }
    }
    Ok(())
}

/// Fail fast on unusable Stereo Tool backends: missing CLI binary, rate
/// mismatch, or missing `.so`. Called before login/network (good UX) and
/// again at the top of `stream()` (backstop). Encoding itself is statically
/// linked and needs no external binary.
pub fn check_prerequisites(pipeline: &PipelineConfig) -> Result<()> {
    // Stereo Tool misconfiguration is reported first: it is the more
    // specific user error.
    if let Some(stereo) = &pipeline.stereo_tool {
        if !stereo.binary_exists() {
            bail!(
                "stereo-tool binary not found ({}); check --stereo-tool",
                stereo.binary.display()
            );
        }
        if stereo.rate != BUS_RATE {
            bail!(
                "stereo-tool rate must be {} Hz (the bus rate); got {} Hz",
                BUS_RATE,
                stereo.rate
            );
        }
    }
    if let Some(lib) = &pipeline.stereo_lib
        && !lib.lib_exists()
    {
        bail!(
            "stereo-tool lib not found ({}); check --stereo-tool-lib",
            lib.lib.display()
        );
    }
    Ok(())
}

/// Stream a playlist to an Icecast mount forever, reconnecting whenever the
/// source connection drops. With an active pipeline, prefetch tasks deliver
/// processed PCM (decode → loudnorm → crossfade → DSP) and the session
/// encoder below turns the endless PCM stream into continuous CBR — no
/// boundary exists at the MP3 level, so splices cannot click. Otherwise
/// Deezer's native MP3 streams untouched.
pub async fn stream(
    api: DeezerApi,
    format: TrackFormat,
    config: IcecastConfig,
    refresh_secs: u64,
    pipeline: PipelineConfig,
) -> Result<()> {
    // Lossless has no native MP3 to pass through: an active pipeline fetches
    // MP3 320 and encodes from there; without one there is nothing to send.
    let fetch_format = if pipeline.is_active() && format == TrackFormat::Flac {
        TrackFormat::Mp3_320
    } else {
        format
    };
    if format == TrackFormat::Flac && !pipeline.is_active() {
        bail!("streaming FLAC to Icecast is not supported; use --quality 320 or 128");
    }
    let encode_bps = pipeline.encode_bitrate(fetch_format);
    check_prerequisites(&pipeline)?;
    let queue = TrackQueue::new(Duration::from_secs(refresh_secs));
    let playlist_name = match api.get_playlist_info(&config.playlist).await {
        Ok(info) => info["DATA"]["TITLE"]
            .as_str()
            .unwrap_or("Unknown Playlist")
            .to_string(),
        Err(_) => config.playlist.clone(),
    };
    queue.set_name(&config.playlist, &playlist_name).await;
    crate::info!(
        "deezco: streaming \"{}\" ({}) to {}{} (refresh every {}s)",
        playlist_name,
        config.playlist,
        config.server.trim_end_matches('/'),
        config.mount,
        refresh_secs
    );
    if pipeline.is_active() {
        crate::info!(
            "deezco: pipeline active: session CBR {encode_bps} kbps (decode → DSP per track in prefetch, continuous encode downstream)"
        );
        if pipeline.crossfade.is_enabled() {
            crate::info!(
                "deezco: crossfade {:.1}s ({:?})",
                pipeline.crossfade.duration_secs,
                pipeline.crossfade.curve
            );
        } else if pipeline.stereo_tool.is_some() || pipeline.stereo_lib.is_some() {
            crate::info!(
                "deezco: first-track render can take a while before connect (stereo-tool runs ~2x realtime; watch for decoding/DSP/ready lines)"
            );
        }
        let chain = pipeline.build_chain();
        if !chain.names().is_empty() {
            crate::info!("deezco: DSP chain: {}", chain.names().join(" -> "));
        }
        if let Some(stereo) = &pipeline.stereo_tool {
            crate::info!(
                "deezco: stereo-tool via {} (per-track spawn; processor state resets each track)",
                stereo.binary.display()
            );
        }
        if let Some(lib) = &pipeline.stereo_lib {
            crate::info!(
                "deezco: stereo-tool-lib via {} (persistent instance; state {})",
                lib.lib.display(),
                if lib.reset_per_track {
                    "resets each track"
                } else {
                    "continuous across tracks"
                },
            );
        }
    }

    // The producer outlives individual connections: after a reconnect it
    // resumes the buffered track and its background prefetch, so dropped
    // connections cost a few seconds instead of a full track preparation.
    //
    // The libStereoTool instance is opened once here (fail fast with a clear
    // error) and shared by every prefetch task; the CLI-subprocess backend
    // needs no such handle.
    let stereo_lib = match &pipeline.stereo_lib {
        Some(config) => {
            let handle = crate::stereo_lib::StereoLibHandle::open(config)?;
            crate::info!(
                "deezco: stereo-tool-lib v{} (api {}), latency {} frames (~{:.0} ms)",
                handle.software_version,
                handle.api_version,
                handle.latency_frames(),
                handle.latency_frames() as f32 / crate::audio::BUS_RATE as f32 * 1000.0,
            );
            Some(Arc::new(std::sync::Mutex::new(handle)))
        }
        None => None,
    };
    let updater = TitleUpdater::new(&config);
    let runtime = PipelineRuntime {
        pipeline,
        encode_bps,
        xfade: Arc::new(Mutex::new(XfadeShared::default())),
        stereo_lib,
    };
    let jingle_dir = config.jingle_dir.clone();
    let producer = Arc::new(Mutex::new(Producer::new(
        api.clone(),
        queue.clone(),
        fetch_format,
        config.metadata,
        Some(updater.clone()),
        config.playlist.clone(),
        runtime,
        jingle_dir,
    )?));

    let mut reconnect_delay = RECONNECT_DELAY;
    loop {
        let connected = match run_connection(&producer, &config, Some(&updater)).await {
            Ok(()) => {
                crate::warn!("deezco: source connection closed by Icecast; reconnecting");
                true
            }
            Err(StreamError::Transient(err)) => {
                crate::warn!("deezco: stream error: {err}; reconnecting");
                false
            }
            Err(StreamError::Fatal(err)) => bail!("{err}"),
        };
        // Back off after failed attempts (hosts like caster.fm reset rapid
        // reconnects, and hammering every 5s keeps the block alive); reset
        // after a connection that actually lasted.
        if connected {
            reconnect_delay = RECONNECT_DELAY;
        }
        crate::warn!("deezco: next connection attempt in {:?}", reconnect_delay);
        sleep(reconnect_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_block_packs_title_with_length_byte_and_padding() {
        let block = icy_metadata_block("Artist - Song");
        // First byte: number of 16-byte units
        let meta = "StreamTitle='Artist - Song';StreamUrl='';";
        let units = meta.len().div_ceil(16);
        assert_eq!(block[0], units as u8);
        assert_eq!(block.len(), 1 + units * 16);
        assert!(block[1..].starts_with(meta.as_bytes()));
        // Padding is null bytes
        assert!(block[1 + meta.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn metadata_block_strips_single_quotes_from_title() {
        let block = icy_metadata_block("It's a \"Song\"");
        let text = String::from_utf8_lossy(&block[1..]);
        assert!(text.contains("StreamTitle='Its a \"Song\"';"));
    }

    #[test]
    fn metadata_block_truncates_very_long_titles() {
        let long = "x".repeat(5000);
        let block = icy_metadata_block(&long);
        // 4080 chars of title + StreamTitle='...'; wrapper fills the largest
        // single block the length byte can express (255 units of 16 bytes)
        assert_eq!(block.len(), 1 + 255 * 16);
        assert_eq!(block[0], 255);
    }

    #[test]
    fn bytes_per_sec_uses_duration_when_known() {
        // 10 MB over 200 seconds = 50 KB/s
        assert_eq!(bytes_per_sec(10_000_000, 200, 40_000), 50_000);
        // Unknown duration falls back to the nominal rate
        assert_eq!(bytes_per_sec(10_000_000, 0, 40_000), 40_000);
        assert_eq!(bytes_per_sec(10_000_000, 0, 16_000), 16_000);
    }

    #[test]
    fn bytes_per_sec_never_returns_zero() {
        assert_eq!(bytes_per_sec(100, 10_000, 16_000), 1);
    }

    #[test]
    fn native_bitrate_maps_formats() {
        assert_eq!(native_bitrate(TrackFormat::Mp3_320), Some(320));
        assert_eq!(native_bitrate(TrackFormat::Mp3_128), Some(128));
        assert_eq!(native_bitrate(TrackFormat::Flac), None);
    }

    #[test]
    fn pipeline_default_is_native_passthrough() {
        let pipeline = PipelineConfig::default();
        assert!(!pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_gain_builds_a_named_chain() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            loudness: None,
            gain_db: Some(6.0),
            bitrate: None,
            stereo_tool: None,
            stereo_lib: None,
        };
        assert!(pipeline.is_active());
        let chain = pipeline.build_chain();
        assert_eq!(chain.names(), vec!["gain"]);
    }

    #[test]
    fn pipeline_crossfade_marks_active_without_chain() {
        // Crossfade renders in the mixer stage, not the DSP chain: active
        // pipeline, empty chain.
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::new(6.0, crate::audio::CrossfadeCurve::EqualPower),
            loudness: None,
            gain_db: None,
            bitrate: None,
            stereo_tool: None,
            stereo_lib: None,
        };
        assert!(pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_zero_gain_stays_passthrough() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            loudness: None,
            gain_db: Some(0.0),
            bitrate: None,
            stereo_tool: None,
            stereo_lib: None,
        };
        assert!(!pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_loudness_target_activates_alone() {
        let pipeline = PipelineConfig {
            loudness: Some(-14.0),
            ..PipelineConfig::default()
        };
        assert!(pipeline.is_active());
        // Loudness corrects pre-mix, not in the DSP chain.
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_bitrate_activates_transcode_alone() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            loudness: None,
            gain_db: None,
            bitrate: Some(96),
            stereo_tool: None,
            stereo_lib: None,
        };
        assert!(pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_stereo_tool_docks_after_gain() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            loudness: None,
            gain_db: Some(3.0),
            bitrate: None,
            stereo_tool: Some(crate::dsp::StereoToolConfig {
                binary: std::path::PathBuf::from("/opt/stereo_tool_cmd_64"),
                settings: None,
                key: None,
                rate: crate::audio::BUS_RATE,
            }),
            stereo_lib: None,
        };
        assert!(pipeline.is_active());
        // Broadcast order: trim gain first, then the broadcast processor.
        assert_eq!(pipeline.build_chain().names(), vec!["gain", "stereo-tool"]);
    }

    #[test]
    fn prerequisites_pass_for_inactive_pipeline() {
        // No external binaries involved on the passthrough path.
        check_prerequisites(&PipelineConfig::default()).unwrap();
    }

    #[test]
    fn prerequisites_reject_missing_stereo_binary() {
        let pipeline = PipelineConfig {
            stereo_tool: Some(crate::dsp::StereoToolConfig {
                binary: std::path::PathBuf::from("/nonexistent/stereo_tool_cmd_64"),
                settings: None,
                key: None,
                rate: crate::audio::BUS_RATE,
            }),
            ..PipelineConfig::default()
        };
        let err = check_prerequisites(&pipeline).unwrap_err();
        assert!(err.to_string().contains("--stereo-tool"), "{err}");
    }

    #[test]
    fn prerequisites_reject_wrong_stereo_rate() {
        // The test binary itself stands in as an existing executable, so
        // this exercises the rate gate with no external dependency.
        let pipeline = PipelineConfig {
            stereo_tool: Some(crate::dsp::StereoToolConfig {
                binary: std::env::current_exe().expect("test binary path"),
                settings: None,
                key: None,
                rate: 48000,
            }),
            ..PipelineConfig::default()
        };
        let err = check_prerequisites(&pipeline).unwrap_err();
        assert!(err.to_string().contains("44100"), "{err}");
    }

    #[test]
    fn pipeline_stereo_lib_activates_alone() {
        let pipeline = PipelineConfig {
            stereo_lib: Some(crate::stereo_lib::StereoLibConfig {
                lib: std::path::PathBuf::from("/opt/libStereoTool.so"),
                settings: None,
                key: None,
                reset_per_track: false,
            }),
            ..PipelineConfig::default()
        };
        assert!(pipeline.is_active());
        // The lib backend docks in the encode stage, not the CLI chain:
        // `build_chain` stays empty until the runtime handle is attached.
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn prerequisites_reject_missing_stereo_lib() {
        let pipeline = PipelineConfig {
            stereo_lib: Some(crate::stereo_lib::StereoLibConfig {
                lib: std::path::PathBuf::from("/nonexistent/libStereoTool.so"),
                settings: None,
                key: None,
                reset_per_track: false,
            }),
            ..PipelineConfig::default()
        };
        let err = check_prerequisites(&pipeline).unwrap_err();
        assert!(err.to_string().contains("--stereo-tool-lib"), "{err}");
    }

    #[test]
    fn encode_bitrate_prefers_override_then_native() {
        let native = PipelineConfig::default();
        assert_eq!(native.encode_bitrate(TrackFormat::Mp3_320), 320);
        assert_eq!(native.encode_bitrate(TrackFormat::Mp3_128), 128);
        // Lossless has no native MP3 rate (fetched as 320 when active).
        assert_eq!(native.encode_bitrate(TrackFormat::Flac), 320);
        let forced = PipelineConfig {
            bitrate: Some(96),
            ..PipelineConfig::default()
        };
        assert_eq!(forced.encode_bitrate(TrackFormat::Mp3_320), 96);
        // Overrides snap to discrete MPEG rates: this is the exact
        // advertised session rate.
        let snapped = PipelineConfig {
            bitrate: Some(100),
            ..PipelineConfig::default()
        };
        assert_eq!(snapped.encode_bitrate(TrackFormat::Mp3_320), 96);
    }

    /// The handoff is the deadlock-critical piece: task 1 must wait for task
    /// 0's publish, and a failed task must still let the sequence advance.
    #[tokio::test]
    async fn crossfade_handoff_orders_tasks_and_survives_failures() {
        use tokio::time::{Duration, timeout};
        let shared = Arc::new(Mutex::new(XfadeShared::default()));

        // Task 1 claims before task 0 publishes: must stay pending.
        let mut waiter = tokio::spawn({
            let shared = shared.clone();
            async move {
                claim_turn(&shared, 1).await;
            }
        });
        assert!(
            timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "seq 1 must wait for seq 0"
        );

        // Task 0 proceeds immediately and publishes.
        timeout(Duration::from_secs(1), claim_turn(&shared, 0))
            .await
            .expect("seq 0 must proceed at once");
        {
            let mut guard = shared.lock().await;
            guard.tail = vec![0.5, 0.5];
            guard.version = 1;
        }
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("seq 1 must proceed after publish")
            .expect("waiter panicked");

        // A failure at seq 2 advances the version without touching the tail.
        advance_past(&shared, 2).await;
        let guard = shared.lock().await;
        assert_eq!(guard.version, 3);
        assert_eq!(guard.tail, vec![0.5, 0.5]);
    }
}
