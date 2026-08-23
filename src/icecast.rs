use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream;
use reqwest::header;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::api::DeezerApi;
use crate::download::{FetchedTrack, fetch_track_audio};
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
/// Initial delay before retrying a failed track fetch inside the stream loop.
/// Kept short to minimise silence gaps that can cause Icecast to drop the
/// source connection.
const INITIAL_FETCH_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Cap for the exponential back-off between consecutive track-fetch retries
/// so a persistently failing CDN URL does not hammer the API.
const MAX_FETCH_RETRY_DELAY: Duration = Duration::from_secs(30);
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

/// Thimeo Stereo Tool processing configuration.
#[derive(Clone)]
pub struct StereoConfig {
    /// Path to the licensed `stereo_tool_cmd_64` binary
    pub binary: PathBuf,
    /// Processor settings file (.sts); the tool falls back to its defaults
    /// when unset
    pub settings: Option<PathBuf>,
    /// License key; passed on the command line like the official CLI expects
    /// (visible in `ps aux`)
    pub key: Option<String>,
    /// Sample rate (Hz) the processing bus runs at
    pub rate: u32,
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

/// Whether a bitrate can be streamed natively.
fn is_valid_bitrate(bitrate: u32) -> bool {
    (8..=320).contains(&bitrate)
}

/// Which format to fetch from Deezer and whether to transcode it. Bitrates
/// that exist natively (128/320) stream directly; anything else is fetched
/// as MP3 320 (falling back automatically) and re-encoded via LAME.
fn fetch_plan(format: TrackFormat, bitrate: Option<u32>) -> (TrackFormat, Option<u32>) {
    match bitrate {
        Some(128) => (TrackFormat::Mp3_128, None),
        Some(320) => (TrackFormat::Mp3_320, None),
        Some(br) => (TrackFormat::Mp3_320, Some(br)),
        None => (format, None),
    }
}

/// Run a filter process with piped stdin/stdout: write `input` to its stdin
/// while reading its stdout, collecting everything. Writes and reads run
/// concurrently so a full pipe buffer cannot deadlock the pipeline.
async fn run_filter(
    mut command: tokio::process::Command,
    input: Vec<u8>,
    name: &str,
) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {name}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let write_stdin = tokio::spawn(async move {
        let result = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
        result
    });

    let mut stderr = child.stderr.take().expect("piped stderr");
    let read_stderr = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut output = Vec::new();
    let mut buf = vec![0u8; 16384];
    loop {
        let n = stdout
            .read(&mut buf)
            .await
            .with_context(|| format!("failed to read {name} output"))?;
        if n == 0 {
            break;
        }
        output.extend_from_slice(&buf[..n]);
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for {name}"))?;
    let stderr_text = read_stderr.await.unwrap_or_default();
    let _ = write_stdin.await;
    if !status.success() {
        bail!("{name} failed ({}): {}", status, stderr_text.trim());
    }
    if output.is_empty() {
        bail!("{name} produced no output");
    }
    Ok(output)
}

/// Re-encode MP3 audio to a target bitrate via LAME (decode to PCM, then
/// re-encode). Kept as a thin wrapper so the transcode and Stereo Tool paths
/// share the same decode/encode helpers.
async fn transcode_mp3(data: Vec<u8>, bitrate: u32) -> Result<Vec<u8>> {
    let pcm = decode_to_pcm(data).await?;
    encode_mp3(pcm, 44100, bitrate).await
}

/// Run LAME with `input` spilled to a temp file and output captured from
/// stdout. LAME's input backends seek, so they cannot read from a pipe; the
/// file path and `-` (stdout) are appended to `args` automatically. The temp
/// file is removed whether or not LAME succeeds.
async fn lame_temp_input(input: &[u8], ext: &str, args: &[&str], name: &str) -> Result<Vec<u8>> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("deezco_{}_{}.{}", std::process::id(), seq, ext));
    {
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(input)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    let mut command = tokio::process::Command::new("lame");
    let mut full = args.to_vec();
    full.push(path.to_str().unwrap_or("-"));
    full.push("-");
    command.args(full);
    let result = run_filter(command, Vec::new(), name).await;
    let _ = std::fs::remove_file(&path);
    result
}

/// Decode MP3 to raw 16-bit little-endian stereo PCM via LAME. LAME decodes at
/// the source sample rate (Deezer MP3 is 44.1 kHz). `lame --decode` emits a
/// WAV container (even on stdout), so we strip the header to recover the raw
/// PCM that Stereo Tool and the encoder expect.
async fn decode_to_pcm(data: Vec<u8>) -> Result<Vec<u8>> {
    let wav = lame_temp_input(&data, "mp3", &["--decode"], "lame-decode").await?;
    Ok(strip_wav_header(wav))
}

/// Drop a WAV container header, returning just the raw PCM samples. LAME's
/// decoder wraps PCM in a WAV file; Stereo Tool and LAME's raw encoder need
/// the bare samples. We locate the `data` chunk rather than assuming a fixed
/// 44-byte header, so extra chunks don't confuse the offset.
fn strip_wav_header(data: Vec<u8>) -> Vec<u8> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return data;
    }
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if id == b"data" {
            let start = pos + 8;
            let end = (start + size).min(data.len());
            return data[start..end].to_vec();
        }
        // Chunks are word-aligned (even size).
        pos += 8 + size + (size & 1);
    }
    data
}

/// Encode raw 16-bit little-endian stereo PCM back to MP3 via LAME.
async fn encode_mp3(pcm: Vec<u8>, rate: u32, bitrate: u32) -> Result<Vec<u8>> {
    let rate_khz = format!("{}", rate as f32 / 1000.0);
    let bitrate_arg = format!("{bitrate}");
    lame_temp_input(
        &pcm,
        "raw",
        &[
            "-r",
            "-s",
            rate_khz.as_str(),
            "-m",
            "s",
            "-b",
            bitrate_arg.as_str(),
        ],
        "lame-encode",
    )
    .await
}

/// Monotonic counter so concurrent temp files get distinct names.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Process raw PCM through the Thimeo Stereo Tool (raw PCM in, raw PCM out).
async fn process_stereo(pcm: Vec<u8>, config: &StereoConfig) -> Result<Vec<u8>> {
    let rate_arg = config.rate.to_string();
    let mut command = tokio::process::Command::new(&config.binary);
    command
        .arg("-q")
        .arg("-b")
        .arg("16")
        .arg("-r")
        .arg(&rate_arg);
    if let Some(settings) = &config.settings {
        command.arg("-s").arg(settings);
    }
    if let Some(key) = &config.key {
        command.arg("-k").arg(key);
    }
    command.arg("-").arg("-");
    run_filter(command, pcm, "Stereo Tool").await
}

/// Whether the LAME binary is available on PATH.
async fn lame_available() -> bool {
    tokio::process::Command::new("lame")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// The bitrate the final encode step targets when transcoding or processing
/// is involved: the requested one, or the native rate of the fetched format.
fn target_bitrate(fetch_format: TrackFormat, transcode: Option<u32>) -> u32 {
    transcode.unwrap_or_else(|| native_bitrate(fetch_format).unwrap_or(320))
}

/// Maximum number of times to retry fetching the same track with a fresh
/// CDN URL before giving up and moving on to the next track in the queue.
const MAX_TRACK_RETRIES: u8 = 3;

/// Download, decrypt, and optionally process (Stereo Tool / transcode) audio
/// for a track that is already known (e.g. a retry after a CDN failure).
async fn prepare_track_audio(
    api: &DeezerApi,
    track: &GwTrack,
    fetch_format: TrackFormat,
    transcode: Option<u32>,
    stereo: &Option<StereoConfig>,
) -> Result<FetchedTrack> {
    if debug_enabled() {
        eprintln!(
            "[deezco-debug] preparing track {} for stream",
            track.display_name()
        );
    }
    let fetched = fetch_track_audio(api, track, fetch_format, false).await?;
    let data = match stereo {
        Some(config) => {
            let rate = config.rate;
            let pcm = decode_to_pcm(fetched.data).await?;
            let pcm = match process_stereo(pcm.clone(), config).await {
                Ok(processed) => processed,
                Err(err) => {
                    eprintln!("deezco: Stereo Tool failed, bypassing: {err}");
                    pcm
                }
            };
            encode_mp3(pcm, rate, target_bitrate(fetch_format, transcode)).await?
        }
        None => match transcode {
            Some(bitrate) => transcode_mp3(fetched.data, bitrate).await?,
            None => fetched.data,
        },
    };
    Ok(FetchedTrack { data })
}

/// Fetch the next track from the queue and prepare its audio. Returns the
/// track and the audio result separately so callers can retry the same
/// track when the CDN URL fails (the track is always available even if
/// the audio fetch errors).
async fn fetch_next_track(
    api: DeezerApi,
    queue: TrackQueue,
    fetch_format: TrackFormat,
    transcode: Option<u32>,
    stereo: Option<StereoConfig>,
    playlist: String,
) -> Result<(GwTrack, Result<FetchedTrack>)> {
    let track = queue
        .next_track(&api, &playlist)
        .await
        .map_err(|err| match err {
            NextTrackError::Empty => anyhow::anyhow!("playlist has no playable tracks"),
            NextTrackError::Fetch(message) => anyhow::anyhow!("{message}"),
        })?;
    let result = prepare_track_audio(&api, &track, fetch_format, transcode, &stereo).await;
    Ok((track, result))
}

/// The audio track currently being pushed, with pacing state.
struct CurrentTrack {
    title: String,
    data: Vec<u8>,
    pos: usize,
    bytes_per_sec: u64,
}

/// Produces the endless source body for Icecast: paced audio chunks with ICY
/// metadata blocks interleaved every `META_INTERVAL` bytes. The next track is
/// prefetched on a background task while the current one streams, so track
/// changes are seamless.
struct Producer {
    api: DeezerApi,
    queue: TrackQueue,
    fetch_format: TrackFormat,
    transcode: Option<u32>,
    stereo: Option<StereoConfig>,
    metadata: bool,
    updater: Option<TitleUpdater>,
    nominal_bps: u64,
    playlist: String,
    /// Actual output bitrate in kbps, resolved per track after Deezer's
    /// quality fallback. Advertised to Icecast so the server's reported
    /// bitrate matches what listeners actually receive (e.g. 128 on a free
    /// account even when 320 was requested).
    actual_kbps: Option<u32>,
    current: Option<CurrentTrack>,
    /// Audio bytes remaining until the next ICY metadata block. Counted
    /// continuously across track changes (Icecast measures the interval from
    /// the start of the connection, not per track) and re-aligned to
    /// `META_INTERVAL` at the start of every new source connection via
    /// `on_connected`, because a reconnect restarts Icecast's counter at 0.
    meta_remaining: usize,
    prefetch: Option<JoinHandle<Result<(GwTrack, Result<FetchedTrack>)>>>,
    sleep_for: Option<Duration>,
    /// Exponential back-off for track-fetch retries in `next_chunk`.
    fetch_backoff: Duration,
    /// A track whose audio fetch failed (CDN error). Retried with a fresh
    /// URL before moving on to the next track in the queue.
    failed_track: Option<GwTrack>,
    /// How many consecutive retries have been attempted for `failed_track`.
    failed_retries: u8,
}

impl Producer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        api: DeezerApi,
        queue: TrackQueue,
        fetch_format: TrackFormat,
        transcode: Option<u32>,
        stereo: Option<StereoConfig>,
        metadata: bool,
        updater: Option<TitleUpdater>,
        playlist: String,
    ) -> Self {
        let nominal_bps = if stereo.is_some() {
            target_bitrate(fetch_format, transcode) as u64 * 1000 / 8
        } else {
            match transcode {
                Some(bitrate) => bitrate as u64 * 1000 / 8,
                None => nominal_bytes_per_sec(fetch_format),
            }
        };
        let prefetch = tokio::spawn(fetch_next_track(
            api.clone(),
            queue.clone(),
            fetch_format,
            transcode,
            stereo.clone(),
            playlist.clone(),
        ));
        Self {
            api,
            queue,
            fetch_format,
            transcode,
            stereo,
            metadata,
            updater,
            nominal_bps,
            playlist,
            actual_kbps: None,
            current: None,
            meta_remaining: META_INTERVAL,
            prefetch: Some(prefetch),
            sleep_for: None,
            fetch_backoff: INITIAL_FETCH_RETRY_DELAY,
            failed_track: None,
            failed_retries: 0,
        }
    }

    /// Ensure the first track is loaded and ready. Used before opening the
    /// Icecast connection so audio flows immediately upon registration:
    /// hosts like caster.fm drop silent sources within seconds, and the
    /// first track can take a while to prepare (download + Stereo Tool
    /// processing + encoding).
    async fn warm_up(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        self.load_next_track().await
    }

    /// Activate a successfully fetched track: print "now playing", update
    /// the Icecast title, kick off the next prefetch, and set `self.current`.
    fn activate_track(&mut self, track: GwTrack, fetched: FetchedTrack) {
        println!("deezco: now playing: {}", track.display_name());
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
        self.prefetch = Some(tokio::spawn(fetch_next_track(
            self.api.clone(),
            self.queue.clone(),
            self.fetch_format,
            self.transcode,
            self.stereo.clone(),
            self.playlist.clone(),
        )));
        // Resolve the real output bitrate so the advertised rate matches what
        // listeners receive: the per-track Deezer fallback for native streams,
        // or the target bitrate for transcoded/Stereo Tool output.
        self.actual_kbps = Some(if self.transcode.is_some() || self.stereo.is_some() {
            target_bitrate(self.fetch_format, self.transcode)
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

    /// Load the next track: retry a previously failed track first, then fall
    /// back to the prefetch (or a direct fetch). The track is always returned
    /// by the fetch helper even when the CDN URL fails, so we can retry with
    /// a fresh URL instead of skipping to the next track.
    async fn load_next_track(&mut self) -> Result<()> {
        // 1. Retry a previously failed track with a fresh CDN URL.
        if let Some(track) = self.failed_track.clone() {
            self.failed_retries += 1;
            if self.failed_retries > MAX_TRACK_RETRIES {
                eprintln!(
                    "deezco: skipping {} after {} retries",
                    track.display_name(),
                    MAX_TRACK_RETRIES
                );
                self.failed_track = None;
                self.failed_retries = 0;
            } else {
                match prepare_track_audio(
                    &self.api,
                    &track,
                    self.fetch_format,
                    self.transcode,
                    &self.stereo,
                )
                .await
                {
                    Ok(fetched) => {
                        self.failed_track = None;
                        self.failed_retries = 0;
                        self.activate_track(track, fetched);
                        return Ok(());
                    }
                    Err(err) => {
                        bail!(
                            "retry {}/{} for {}: {err}",
                            self.failed_retries,
                            MAX_TRACK_RETRIES,
                            track.display_name()
                        );
                    }
                }
            }
        }

        // 2. Await the prefetch (or fetch directly on the first run).
        let (track, fetch_result) = match self.prefetch.take() {
            Some(handle) => handle.await.context("prefetch task panicked")??,
            None => {
                fetch_next_track(
                    self.api.clone(),
                    self.queue.clone(),
                    self.fetch_format,
                    self.transcode,
                    self.stereo.clone(),
                    self.playlist.clone(),
                )
                .await?
            }
        };
        let fetched = match fetch_result {
            Ok(fetched) => fetched,
            Err(err) => {
                // Store the track so the next call retries it with a fresh URL.
                self.failed_track = Some(track);
                self.failed_retries = 0;
                return Err(err);
            }
        };
        self.activate_track(track, fetched);
        Ok(())
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
                        self.fetch_backoff
                    );
                    sleep(self.fetch_backoff).await;
                    self.fetch_backoff = (self.fetch_backoff * 2).min(MAX_FETCH_RETRY_DELAY);
                    continue;
                }
                // Backoff succeeded: reset for the next failure cycle.
                self.fetch_backoff = INITIAL_FETCH_RETRY_DELAY;
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
        if self.stereo.is_some() {
            target_bitrate(self.fetch_format, self.transcode)
        } else {
            self.transcode
                .unwrap_or_else(|| (nominal_bytes_per_sec(self.fetch_format) * 8 / 1000) as u32)
        }
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
            let note = if p.stereo.is_some() {
                " (Stereo Tool processing can take a minute)"
            } else {
                ""
            };
            println!("deezco: preparing the first track before connecting{note}");
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

/// Stream a playlist to an Icecast mount forever, reconnecting whenever the
/// source connection drops. `bitrate` (kbps) transcodes via LAME when set;
/// 128 and 320 stream natively without transcoding. `stereo` runs every
/// track through the Thimeo Stereo Tool (decode -> process -> encode).
pub async fn stream(
    api: DeezerApi,
    format: TrackFormat,
    config: IcecastConfig,
    refresh_secs: u64,
    bitrate: Option<u32>,
    stereo: Option<StereoConfig>,
) -> Result<()> {
    if bitrate.is_some_and(|br| !is_valid_bitrate(br)) {
        bail!("--bitrate must be between 8 and 320 kbps");
    }
    if format == TrackFormat::Flac && bitrate.is_none() && stereo.is_none() {
        bail!("streaming FLAC to Icecast is not supported; use --quality 320 or 128");
    }
    let (fetch_format, transcode) = fetch_plan(format, bitrate);
    if (transcode.is_some() || stereo.is_some()) && !lame_available().await {
        bail!(
            "--bitrate (transcoding) and --stereo-tool (decode/process/encode) \
             require lame; install it (e.g. `apt-get install lame`)"
        );
    }
    let queue = TrackQueue::new(Duration::from_secs(refresh_secs));
    println!(
        "deezco: streaming playlist {} to {}{} (refresh every {}s)",
        config.playlist,
        config.server.trim_end_matches('/'),
        config.mount,
        refresh_secs
    );

    // The producer outlives individual connections: after a reconnect it
    // resumes the buffered track and its background prefetch, so dropped
    // connections cost a few seconds instead of a full track preparation.
    let updater = TitleUpdater::new(&config);
    let producer = Arc::new(Mutex::new(Producer::new(
        api.clone(),
        queue.clone(),
        fetch_format,
        transcode,
        stereo,
        config.metadata,
        Some(updater.clone()),
        config.playlist.clone(),
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
    fn fetch_plan_uses_native_formats_for_128_and_320() {
        // Native bitrates stream without transcoding
        assert_eq!(
            fetch_plan(TrackFormat::Mp3_320, Some(128)),
            (TrackFormat::Mp3_128, None)
        );
        assert_eq!(
            fetch_plan(TrackFormat::Mp3_320, Some(320)),
            (TrackFormat::Mp3_320, None)
        );
        // No bitrate: the requested format is used as-is
        assert_eq!(
            fetch_plan(TrackFormat::Mp3_320, None),
            (TrackFormat::Mp3_320, None)
        );
    }

    #[test]
    fn fetch_plan_transcodes_other_bitrates_from_mp3_320() {
        assert_eq!(
            fetch_plan(TrackFormat::Mp3_320, Some(96)),
            (TrackFormat::Mp3_320, Some(96))
        );
        assert_eq!(
            fetch_plan(TrackFormat::Flac, Some(96)),
            (TrackFormat::Mp3_320, Some(96))
        );
    }

    #[test]
    fn bitrate_validation_accepts_mp3_range() {
        assert!(is_valid_bitrate(8));
        assert!(is_valid_bitrate(96));
        assert!(is_valid_bitrate(320));
        assert!(!is_valid_bitrate(0));
        assert!(!is_valid_bitrate(7));
        assert!(!is_valid_bitrate(321));
    }

    #[test]
    fn native_bitrate_maps_formats() {
        assert_eq!(native_bitrate(TrackFormat::Mp3_320), Some(320));
        assert_eq!(native_bitrate(TrackFormat::Mp3_128), Some(128));
        assert_eq!(native_bitrate(TrackFormat::Flac), None);
    }

    #[test]
    fn target_bitrate_uses_requested_or_native_rate() {
        assert_eq!(target_bitrate(TrackFormat::Mp3_320, Some(96)), 96);
        assert_eq!(target_bitrate(TrackFormat::Mp3_320, None), 320);
        assert_eq!(target_bitrate(TrackFormat::Mp3_128, None), 128);
        // Lossless has no native rate; the encode falls back to 320
        assert_eq!(target_bitrate(TrackFormat::Flac, None), 320);
    }

    /// Build a raw 16-bit little-endian stereo PCM sine (44100 Hz) for tests.
    fn sine_pcm(seconds: f32) -> Vec<u8> {
        let rate = 44100u32;
        let total = (rate as f32 * seconds) as usize;
        let mut pcm = Vec::with_capacity(total * 4);
        for i in 0..total {
            let t = i as f32 / rate as f32;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 8000.0;
            let v = sample as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        pcm
    }

    /// Encode the test sine to an MP3 via LAME.
    async fn sine_mp3() -> Vec<u8> {
        let mut command = tokio::process::Command::new("lame");
        command.args(["-r", "-s", "44.1", "-m", "s", "-b", "128", "-", "-"]);
        run_filter(command, sine_pcm(2.0), "lame").await.unwrap()
    }

    /// End-to-end transcode check: needs LAME on PATH. Run with
    /// `cargo test -- --ignored transcode_pipeline`.
    #[tokio::test]
    #[ignore = "requires lame on PATH"]
    async fn transcode_pipeline_produces_smaller_mp3() {
        let source = sine_mp3().await;
        assert!(!source.is_empty(), "failed to generate source MP3");

        let encoded = transcode_mp3(source.clone(), 96).await.unwrap();
        assert!(!encoded.is_empty(), "transcode produced no audio");
        assert!(
            encoded.len() < source.len(),
            "96 kbps output should be smaller than the 128 kbps source"
        );
    }

    /// End-to-end PCM decode/encode check (the Stereo Tool stages): needs
    /// LAME on PATH. Run with `cargo test -- --ignored decode_encode_pipeline`.
    #[tokio::test]
    #[ignore = "requires lame on PATH"]
    async fn decode_encode_pipeline_roundtrips_pcm() {
        let source = sine_mp3().await;
        assert!(!source.is_empty(), "failed to generate source MP3");

        let pcm = decode_to_pcm(source.clone()).await.unwrap();
        assert!(!pcm.is_empty(), "decode produced no PCM");
        // 2 seconds of stereo 16-bit 44.1 kHz PCM is 352,800 bytes; the MP3
        // encoder adds a short padding tail that survives decoding
        assert!(
            (pcm.len() as i64 - 352_800).abs() < 10_000,
            "unexpected PCM length: {}",
            pcm.len()
        );

        let encoded = encode_mp3(pcm, 44100, 128).await.unwrap();
        assert!(!encoded.is_empty(), "encode produced no MP3");
    }
}
