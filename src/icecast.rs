use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream;
use reqwest::header;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::api::DeezerApi;
use crate::audio::{
    BUS_RATE, CrossfadeConfig, FrameDecoder, FrameEncoder, LameEncoder, Mp3Decoder, PcmBuffer,
    render_track_overlap,
};
use crate::download::{FetchedTrack, fetch_track_audio};
use crate::dsp::{GainProcessor, ProcessorChain, StereoToolProcessor};
use crate::models::{GwTrack, TrackFormat};
use crate::queue::{NextTrackError, TrackQueue};
use crate::track::{available_format, debug_enabled};

/// How many bytes of audio between in-band ICY metadata blocks. The source
/// picks the interval and tells Icecast about it via the `icy-metaint`
/// request header; 16000 is the widely used default.
const META_INTERVAL: usize = 16000;
/// Size of the audio chunks pushed to Icecast between pacing sleeps.
const CHUNK_SIZE: usize = 32768;
/// How long to wait before retrying after a transient failure.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// How long to wait before reconnecting after Icecast drops the source.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Cap for the exponential backoff between connection attempts, so a host
/// that keeps resetting connections (edge proxies, rate limiters) gets time
/// to clear instead of being hammered every few seconds.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);

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
}

/// PCM-bus pipeline configuration.
///
/// When inactive (default) the audio path is native MP3 passthrough with
/// zero extra dependencies. When active — crossfade, DSP gain, or an
/// explicit `--bitrate` — every track runs decode → (crossfade) → DSP →
/// CBR encode, so listeners hear one constant format for the whole session.
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    /// Overlap between consecutive tracks (0 = hard cut).
    pub crossfade: CrossfadeConfig,
    /// Static DSP gain in dB, applied post-crossfade pre-encode.
    pub gain_db: Option<f32>,
    /// Session encode bitrate in kbps. `None` keeps the fetched format's
    /// native rate (320, or 128 on fallback); `Some` forces a transcode and
    /// activates the pipeline on its own.
    pub bitrate: Option<u32>,
    /// Thimeo Stereo Tool processing. Activates the pipeline on its own;
    /// runs after the gain stage, before the encoder.
    pub stereo_tool: Option<crate::dsp::StereoToolConfig>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            crossfade: CrossfadeConfig::disabled(),
            gain_db: None,
            bitrate: None,
            stereo_tool: None,
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
            || self.gain_db.is_some_and(|db| db != 0.0)
            || self.bitrate.is_some()
            || self.stereo_tool.is_some()
    }

    /// The one CBR bitrate the whole session encodes at when the pipeline
    /// is active: the explicit override, else the fetched format's native
    /// rate (320 for lossless sources fetched as MP3).
    pub fn encode_bitrate(&self, fetch_format: TrackFormat) -> u32 {
        self.bitrate
            .unwrap_or_else(|| native_bitrate(fetch_format).unwrap_or(320))
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
        Self {
            url_base: format!("{}/admin/metadata", config.server.trim_end_matches('/')),
            mount: config.mount.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
            client: reqwest::Client::new(),
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
        eprintln!(
            "[deezco-debug] preparing track {} for stream",
            track.display_name()
        );
    }
    let fetched = fetch_track_audio(api, track, fetch_format, false).await?;
    Ok(fetched)
}

/// Ordered handoff between consecutive prefetch tasks: the held PCM tail of
/// the previous track plus a sequence number. Tasks decode and encode fully
/// in parallel; only the microsecond mix-and-publish step is ordered, so a
/// slow LAME encode never serializes the pipeline.
#[derive(Default)]
struct XfadeShared {
    /// How many tracks have published their tail. Task `seq` may render once
    /// `version == seq`; the first track (seq 0) proceeds immediately.
    version: u64,
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

/// A failed task must still let the sequence move past it, or every later
/// task would wait forever. The held tail is left untouched: it still
/// belongs to the last good track, which is exactly the streamed boundary.
async fn advance_past(shared: &Arc<Mutex<XfadeShared>>, seq: u64) {
    let mut guard = shared.lock().await;
    guard.version = guard.version.max(seq + 1);
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
}

/// Fetch the next track from the queue and prepare its audio: native MP3
/// passthrough by default, or the full decode → crossfade → DSP → CBR
/// encode path when the pipeline is active. Slow CPU work (decode, LAME)
/// runs on blocking threads inside the background task, so track changes
/// stay instant on the streaming path.
async fn fetch_next_track(ctx: TaskCtx) -> Result<(GwTrack, FetchedTrack)> {
    let track = ctx
        .queue
        .next_track(&ctx.api, &ctx.playlist)
        .await
        .map_err(|err| match err {
            NextTrackError::Empty => anyhow::anyhow!("playlist has no playable tracks"),
            NextTrackError::Fetch(message) => anyhow::anyhow!("{message}"),
        })?;
    let fetched = match prepare_track_audio(&ctx.api, &track, ctx.fetch_format).await {
        Ok(fetched) => fetched,
        Err(err) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(err);
        }
    };
    if !ctx.runtime.pipeline.is_active() {
        return Ok((track, fetched));
    }
    let pcm = match tokio::task::spawn_blocking(move || Mp3Decoder::new().decode(&fetched.data))
        .await
        .context("decoder task panicked")?
    {
        Ok(pcm) => pcm,
        Err(err) => {
            advance_past(&ctx.runtime.xfade, ctx.seq).await;
            return Err(err);
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
    let pipeline = ctx.runtime.pipeline;
    let encode_bps = ctx.runtime.encode_bps;
    let data = tokio::task::spawn_blocking(move || {
        let mut chain = pipeline.build_chain();
        let mut out = out_pcm;
        chain.process(&mut out)?;
        LameEncoder::new(encode_bps)?.encode(&PcmBuffer::from_interleaved(out))
    })
    .await
    .context("encode task panicked")??;
    Ok((track, FetchedTrack { data }))
}

/// The audio track currently being pushed, with pacing state.
struct CurrentTrack {
    title: String,
    data: Vec<u8>,
    pos: usize,
    bytes_per_sec: u64,
}

/// Background task that fetches and prepares the next track.
type PrefetchHandle = JoinHandle<Result<(GwTrack, FetchedTrack)>>;

/// Produces the endless source body for Icecast: paced audio chunks with ICY
/// metadata blocks interleaved every `META_INTERVAL` bytes. The next track is
/// prefetched on a background task while the current one streams, so track
/// changes are seamless.
///
/// Two audio paths: native MP3 passthrough (default), or — when the pipeline
/// is active — prefetch tasks deliver CBR MP3 rendered as decode →
/// crossfade → DSP → encode, with the crossfade tails handed from one task
/// to the next in activation order (`XfadeShared`). Either way `next_chunk`
/// below only ever sees final MP3 bytes, so pacing and ICY framing are
/// identical on both paths (and exact on CBR output).
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
    sleep_for: Option<Duration>,
}

impl Producer {
    fn new(
        api: DeezerApi,
        queue: TrackQueue,
        fetch_format: TrackFormat,
        metadata: bool,
        updater: Option<TitleUpdater>,
        playlist: String,
        runtime: PipelineRuntime,
    ) -> Self {
        // CBR pipeline output paces exactly at the encode rate; native
        // passthrough falls back to the fetched format's nominal rate when
        // a track reports no duration.
        let nominal_bps = if runtime.pipeline.is_active() {
            u64::from(runtime.encode_bps) * 1000 / 8
        } else {
            nominal_bytes_per_sec(fetch_format)
        };
        let mut prefetch = VecDeque::new();
        prefetch.push_back(tokio::spawn(fetch_next_track(TaskCtx {
            api: api.clone(),
            queue: queue.clone(),
            fetch_format,
            playlist: playlist.clone(),
            seq: 0,
            runtime: runtime.clone(),
        })));
        Self {
            api,
            queue,
            fetch_format,
            metadata,
            updater,
            nominal_bps,
            playlist,
            runtime,
            next_seq: 1,
            actual_kbps: None,
            current: None,
            meta_remaining: META_INTERVAL,
            prefetch,
            sleep_for: None,
        }
    }

    /// Ensure the first track is loaded and ready. Used before opening the
    /// Icecast connection so audio flows immediately upon registration:
    /// hosts like caster.fm drop silent sources within seconds.
    async fn warm_up(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        self.load_next_track().await
    }

    /// Spawn a background prefetch for the next track and push it to
    /// the prefetch queue. Sequence numbers follow activation order so the
    /// crossfade handoff stays aligned even though tasks run concurrently.
    fn spawn_prefetch(&mut self) {
        let seq = self.next_seq;
        self.next_seq += 1;
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

    /// Activate a successfully fetched track: print "now playing", update
    /// the Icecast title, top up the prefetch queue, and set `self.current`.
    /// On the pipeline path the audio already carries its rendered overlap
    /// (mixed in the background task), so activation stays instant here.
    fn activate_track(&mut self, track: GwTrack, fetched: FetchedTrack) {
        if self.runtime.pipeline.crossfade.is_enabled() {
            println!(
                "deezco: now playing: {} (crossfade {:.1}s)",
                track.display_name(),
                self.runtime.pipeline.crossfade.duration_secs
            );
        } else {
            println!("deezco: now playing: {}", track.display_name());
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
                    eprintln!("deezco: title update failed: {err}");
                }
            });
        }
        // Top up the prefetch queue so there are always PREFETCH_AHEAD
        // tracks ready (or being fetched) ahead of the current one.
        while self.prefetch.len() < Self::PREFETCH_AHEAD {
            eprintln!("deezco: prefetching next track in background");
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
        self.current = Some(CurrentTrack {
            title: track.display_name(),
            bytes_per_sec: bytes_per_sec(
                fetched.data.len(),
                track.duration_secs(),
                self.nominal_bps,
            ),
            data: fetched.data,
            pos: 0,
        });
        // Note: `meta_remaining` intentionally lives on the Producer and is
        // carried across track changes — the ICY interval must stay
        // continuous for as long as the source connection is open.
    }

    /// Load the next track: pop from the prefetch queue (or fetch directly
    /// when the queue drained) and top up the queue. On failure, the track
    /// is skipped and the next prefetched track is tried.
    async fn load_next_track(&mut self) -> Result<()> {
        loop {
            let handle = if let Some(h) = self.prefetch.pop_front() {
                h
            } else {
                let seq = self.next_seq;
                self.next_seq += 1;
                tokio::spawn(fetch_next_track(TaskCtx {
                    api: self.api.clone(),
                    queue: self.queue.clone(),
                    fetch_format: self.fetch_format,
                    playlist: self.playlist.clone(),
                    seq,
                    runtime: self.runtime.clone(),
                }))
            };
            match handle.await.context("prefetch task panicked") {
                Ok(Ok((track, fetched))) => {
                    self.activate_track(track, fetched);
                    return Ok(());
                }
                Ok(Err(err)) => {
                    eprintln!("deezco: track fetch failed: {err}; skipping");
                    continue;
                }
                Err(err) => {
                    return Err(err);
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
    }

    /// Next body chunk: audio (paced) or a metadata block. `None` never
    /// happens — errors are logged and retried so the stream never ends.
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, std::io::Error>> {
        if let Some(dur) = self.sleep_for.take() {
            sleep(dur).await;
        }
        loop {
            if self.current.is_none() {
                let started = std::time::Instant::now();
                if let Err(err) = self.load_next_track().await {
                    eprintln!(
                        "deezco: track fetch failed: {err}; retrying in {:?}",
                        RETRY_DELAY
                    );
                    sleep(RETRY_DELAY).await;
                    continue;
                }
                // A slow prep makes the stream go silent: silent-source hosts
                // drop the connection, so surface the stall.
                if started.elapsed() > Duration::from_secs(2) {
                    eprintln!(
                        "deezco: next track took {:?} to prepare (stream was silent)",
                        started.elapsed()
                    );
                }
            }
            let Some(current) = self.current.as_mut() else {
                continue;
            };
            if self.metadata && self.meta_remaining == 0 {
                self.meta_remaining = META_INTERVAL;
                return Some(Ok(icy_metadata_block(&current.title)));
            }
            // `current.data` is final MP3 on both paths — native bytes, or
            // session CBR rendered upstream by the prefetch task — so pacing
            // and ICY framing stay identical. CBR output paces exactly.
            let take = if self.metadata {
                self.meta_remaining
                    .min(CHUNK_SIZE)
                    .min(current.data.len() - current.pos)
            } else {
                CHUNK_SIZE.min(current.data.len() - current.pos)
            };
            let chunk = current.data[current.pos..current.pos + take].to_vec();
            let finished = current.pos + take >= current.data.len();
            current.pos += take;
            self.meta_remaining -= take;
            let bytes_per_sec = current.bytes_per_sec;
            if finished {
                self.current = None;
            }
            self.sleep_for = Some(Duration::from_secs_f64(take as f64 / bytes_per_sec as f64));
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
            println!("deezco: preparing the first track before connecting");
            prepared = true;
        }
        match p.warm_up().await {
            Ok(()) => {
                advertised_kbps = p.advertised_kbps();
                println!(
                    "deezco: streaming at {advertised_kbps} kbps (actual, after quality fallback)"
                );
                client = p.api.client().clone();
                break;
            }
            Err(err) => {
                eprintln!("deezco: first track not ready: {err}; retrying");
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

    let response = request.send().await.map_err(|err| {
        StreamError::Transient(anyhow::Error::new(err).context("Icecast connection failed"))
    })?;
    if !response.status().is_success() {
        return Err(StreamError::Fatal(anyhow::anyhow!(
            "Icecast rejected the source connection ({}); check the mount point and source password",
            response.status()
        )));
    }
    println!("deezco: connected to {url} (listeners can tune in at {url})");
    if let Some(updater) = updater {
        let title = producer.lock().await.current_title();
        if let Some(title) = title {
            let updater = updater.clone();
            tokio::spawn(async move {
                if let Err(err) = updater.update(&title).await
                    && !err.to_string().contains("404")
                {
                    eprintln!("deezco: title update failed: {err}");
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

/// Fail fast when the pipeline's external binaries are unusable: the
/// `lame` binary for any active pipeline, plus the Stereo Tool binary and
/// bus-rate match when processing is requested. Called before login/network
/// (good UX) and again at the top of `stream()` (backstop).
pub fn check_prerequisites(pipeline: &PipelineConfig) -> Result<()> {
    // Stereo Tool misconfiguration is reported first: it is the more
    // specific user error, and these checks need no external binary.
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
    if pipeline.is_active() && !LameEncoder::is_available() {
        bail!(
            "the stream pipeline (--crossfade, --gain-db, --bitrate, or --stereo-tool) needs the lame binary; install it (e.g. `apt-get install lame`)"
        );
    }
    Ok(())
}

/// Stream a playlist to an Icecast mount forever, reconnecting whenever the
/// source connection drops. With an active pipeline every track is rendered
/// through decode → crossfade → DSP → session-CBR encode in background
/// prefetch tasks; otherwise Deezer's native MP3 streams untouched.
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
    println!(
        "deezco: streaming \"{}\" ({}) to {}{} (refresh every {}s)",
        playlist_name,
        config.playlist,
        config.server.trim_end_matches('/'),
        config.mount,
        refresh_secs
    );
    if pipeline.is_active() {
        println!(
            "deezco: pipeline active: session CBR {encode_bps} kbps (decode → crossfade → DSP → encode per track)"
        );
        if pipeline.crossfade.is_enabled() {
            println!(
                "deezco: crossfade {:.1}s ({:?}); first-track encode can take ~20s before connect",
                pipeline.crossfade.duration_secs, pipeline.crossfade.curve
            );
        }
        let chain = pipeline.build_chain();
        if !chain.names().is_empty() {
            println!("deezco: DSP chain: {}", chain.names().join(" -> "));
        }
        if let Some(stereo) = &pipeline.stereo_tool {
            println!(
                "deezco: stereo-tool via {} (per-track spawn; processor state resets each track)",
                stereo.binary.display()
            );
        }
    }

    // The producer outlives individual connections: after a reconnect it
    // resumes the buffered track and its background prefetch, so dropped
    // connections cost a few seconds instead of a full track preparation.
    let updater = TitleUpdater::new(&config);
    let runtime = PipelineRuntime {
        pipeline,
        encode_bps,
        xfade: Arc::new(Mutex::new(XfadeShared::default())),
    };
    let producer = Arc::new(Mutex::new(Producer::new(
        api.clone(),
        queue.clone(),
        fetch_format,
        config.metadata,
        Some(updater.clone()),
        config.playlist.clone(),
        runtime,
    )));

    let mut reconnect_delay = RECONNECT_DELAY;
    loop {
        let connected = match run_connection(&producer, &config, Some(&updater)).await {
            Ok(()) => {
                eprintln!("deezco: source connection closed by Icecast; reconnecting");
                true
            }
            Err(StreamError::Transient(err)) => {
                eprintln!("deezco: stream error: {err}; reconnecting");
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
        eprintln!("deezco: next connection attempt in {:?}", reconnect_delay);
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
            gain_db: Some(6.0),
            bitrate: None,
            stereo_tool: None,
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
            gain_db: None,
            bitrate: None,
            stereo_tool: None,
        };
        assert!(pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_zero_gain_stays_passthrough() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            gain_db: Some(0.0),
            bitrate: None,
            stereo_tool: None,
        };
        assert!(!pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_bitrate_activates_transcode_alone() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            gain_db: None,
            bitrate: Some(96),
            stereo_tool: None,
        };
        assert!(pipeline.is_active());
        assert!(pipeline.build_chain().is_empty());
    }

    #[test]
    fn pipeline_stereo_tool_docks_after_gain() {
        let pipeline = PipelineConfig {
            crossfade: CrossfadeConfig::disabled(),
            gain_db: Some(3.0),
            bitrate: None,
            stereo_tool: Some(crate::dsp::StereoToolConfig {
                binary: std::path::PathBuf::from("/opt/stereo_tool_cmd_64"),
                settings: None,
                key: None,
                rate: crate::audio::BUS_RATE,
            }),
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
