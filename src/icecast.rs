use std::path::PathBuf;
use std::sync::Arc;
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
/// as MP3 320 (falling back automatically) and re-encoded via ffmpeg.
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

/// Re-encode MP3 audio to a target bitrate via ffmpeg.
async fn transcode_mp3(data: Vec<u8>, bitrate: u32) -> Result<Vec<u8>> {
    let bitrate_arg = format!("{bitrate}k");
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-i",
        "pipe:0",
        "-c:a",
        "libmp3lame",
        "-b:a",
        bitrate_arg.as_str(),
        "-f",
        "mp3",
        "pipe:1",
    ]);
    run_filter(command, data, "ffmpeg").await
}

/// Decode MP3 to raw 16-bit little-endian stereo PCM via ffmpeg.
async fn decode_to_pcm(data: Vec<u8>, rate: u32) -> Result<Vec<u8>> {
    let rate_arg = rate.to_string();
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-i",
        "pipe:0",
        "-f",
        "s16le",
        "-ar",
        rate_arg.as_str(),
        "-ac",
        "2",
        "pipe:1",
    ]);
    run_filter(command, data, "ffmpeg").await
}

/// Encode raw 16-bit little-endian stereo PCM back to MP3 via ffmpeg.
async fn encode_mp3(pcm: Vec<u8>, rate: u32, bitrate: u32) -> Result<Vec<u8>> {
    let rate_arg = rate.to_string();
    let bitrate_arg = format!("{bitrate}k");
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "s16le",
        "-ar",
        rate_arg.as_str(),
        "-ac",
        "2",
        "-i",
        "pipe:0",
        "-c:a",
        "libmp3lame",
        "-b:a",
        bitrate_arg.as_str(),
        "-f",
        "mp3",
        "pipe:1",
    ]);
    run_filter(command, pcm, "ffmpeg").await
}

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

/// Whether the ffmpeg binary is available on PATH.
async fn ffmpeg_available() -> bool {
    tokio::process::Command::new("ffmpeg")
        .arg("-version")
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

/// Fetch the next track from the queue, download its decrypted audio, and
/// optionally process it (Stereo Tool) or transcode it to the target
/// bitrate. Runs on a background task so the current track can finish
/// streaming without a gap.
async fn fetch_next_track(
    api: DeezerApi,
    queue: TrackQueue,
    fetch_format: TrackFormat,
    transcode: Option<u32>,
    stereo: Option<StereoConfig>,
    playlist: String,
) -> Result<(GwTrack, FetchedTrack)> {
    let track = queue
        .next_track(&api, &playlist)
        .await
        .map_err(|err| match err {
            NextTrackError::Empty => anyhow::anyhow!("playlist has no playable tracks"),
            NextTrackError::Fetch(message) => anyhow::anyhow!("{message}"),
        })?;
    let fetched = fetch_track_audio(&api, &track, fetch_format, false).await?;
    let data = match stereo {
        Some(config) => {
            let rate = config.rate;
            let pcm = decode_to_pcm(fetched.data, rate).await?;
            let pcm = match process_stereo(pcm.clone(), &config).await {
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
    Ok((track, FetchedTrack { data }))
}

/// The audio track currently being pushed, with pacing state.
struct CurrentTrack {
    title: String,
    data: Vec<u8>,
    pos: usize,
    meta_remaining: usize,
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
    nominal_bps: u64,
    playlist: String,
    current: Option<CurrentTrack>,
    prefetch: Option<JoinHandle<Result<(GwTrack, FetchedTrack)>>>,
    sleep_for: Option<Duration>,
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
            nominal_bps,
            playlist,
            current: None,
            prefetch: Some(prefetch),
            sleep_for: None,
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

    /// Load the next track: await the prefetch (or fetch it directly on the
    /// first run) and kick off the prefetch for the following one.
    async fn load_next_track(&mut self) -> Result<()> {
        let (track, fetched) = match self.prefetch.take() {
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
        println!("deezco: now playing: {}", track.display_name());
        self.prefetch = Some(tokio::spawn(fetch_next_track(
            self.api.clone(),
            self.queue.clone(),
            self.fetch_format,
            self.transcode,
            self.stereo.clone(),
            self.playlist.clone(),
        )));
        self.current = Some(CurrentTrack {
            title: track.display_name(),
            bytes_per_sec: bytes_per_sec(
                fetched.data.len(),
                track.duration_secs(),
                self.nominal_bps,
            ),
            data: fetched.data,
            pos: 0,
            meta_remaining: META_INTERVAL,
        });
        Ok(())
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
            if self.metadata && current.meta_remaining == 0 {
                current.meta_remaining = META_INTERVAL;
                return Some(Ok(icy_metadata_block(&current.title)));
            }
            let take = if self.metadata {
                current
                    .meta_remaining
                    .min(CHUNK_SIZE)
                    .min(current.data.len() - current.pos)
            } else {
                CHUNK_SIZE.min(current.data.len() - current.pos)
            };
            let chunk = current.data[current.pos..current.pos + take].to_vec();
            let finished = current.pos + take >= current.data.len();
            current.pos += take;
            current.meta_remaining -= take;
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
/// source connection drops. `bitrate` (kbps) transcodes via ffmpeg when set;
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
    if (transcode.is_some() || stereo.is_some()) && !ffmpeg_available().await {
        bail!(
            "--bitrate (transcoding) and --stereo-tool (decode/process/encode) \
             require ffmpeg; install it"
        );
    }
    let queue = TrackQueue::new(Duration::from_secs(refresh_secs));
    println!(
        "deezco: streaming playlist {} to {}{} at {} kbps (refresh every {}s)",
        config.playlist,
        config.server.trim_end_matches('/'),
        config.mount,
        bitrate.unwrap_or_else(|| native_bitrate(format).unwrap_or(320)),
        refresh_secs
    );

    // The producer outlives individual connections: after a reconnect it
    // resumes the buffered track and its background prefetch, so dropped
    // connections cost a few seconds instead of a full track preparation.
    let producer = Arc::new(Mutex::new(Producer::new(
        api.clone(),
        queue.clone(),
        fetch_format,
        transcode,
        stereo,
        config.metadata,
        config.playlist.clone(),
    )));

    let mut reconnect_delay = RECONNECT_DELAY;
    loop {
        let connected = match run_connection(&producer, &config).await {
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

    /// End-to-end transcode check: needs ffmpeg on PATH. Run with
    /// `cargo test -- --ignored transcode_pipeline`.
    #[tokio::test]
    #[ignore = "requires ffmpeg on PATH"]
    async fn transcode_pipeline_produces_smaller_mp3() {
        let source = tokio::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-f",
                "mp3",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = source.wait_with_output().await.unwrap();
        assert!(!output.stdout.is_empty(), "failed to generate source MP3");

        let encoded = transcode_mp3(output.stdout.clone(), 96).await.unwrap();
        assert!(!encoded.is_empty(), "transcode produced no audio");
        assert!(
            encoded.len() < output.stdout.len(),
            "96 kbps output should be smaller than the 128 kbps source"
        );
    }

    /// End-to-end PCM decode/encode check (the Stereo Tool stages): needs
    /// ffmpeg on PATH. Run with `cargo test -- --ignored decode_encode_pipeline`.
    #[tokio::test]
    #[ignore = "requires ffmpeg on PATH"]
    async fn decode_encode_pipeline_roundtrips_pcm() {
        let source = tokio::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-f",
                "mp3",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = source.wait_with_output().await.unwrap();
        assert!(!output.stdout.is_empty(), "failed to generate source MP3");

        let pcm = decode_to_pcm(output.stdout.clone(), 44100).await.unwrap();
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
