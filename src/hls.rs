//! File-only HLS output for `stream --hls-dir`.
//!
//! AAC-in-TS via a single continuous ffmpeg session:
//! decode → loudnorm → crossfade → DSP (Rust) → s16le pipe →
//! `ffmpeg -c:a aac -f mpegts` → realtime-paced sliding-window live
//! playlist (`live.m3u8` + `seg_*.ts`) for an external web server.
//! No port is opened; no login, no Deezer API.
//!
//! Segments are emitted on wall-clock schedule (one segment's audio per
//! segment duration): without pacing the encoder runs at CPU speed, the
//! window races minutes ahead within seconds, and players joining "live"
//! start mid-library then skip as their buffered segments fall out of the
//! window. One continuous ffmpeg session keeps PTS stable — track
//! boundaries never need `DISCONTINUITY`. Segments are cut on 188-byte
//! MPEG-TS packet boundaries; their `EXTINF` duration comes from the PCM
//! frames fed (fed samples / 44100), so titles stay aligned within the
//! ffmpeg internal lag (<1s).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::audio::{BUS_RATE, FrameDecoder, SymphoniaDecoder, f32_to_s16_stereo};
use crate::files::is_music_file;
use crate::icecast::PipelineConfig;
use crate::queue::{LocalFileQueue, NextTrackError};

/// Playlist file served by the external web server.
const PLAYLIST_NAME: &str = "live.m3u8";
/// Temp name for atomic playlist updates (write + rename).
const PLAYLIST_TMP: &str = "live.m3u8.tmp";
/// MPEG-TS packet size; segments are cut on multiples of this.
const TS_PACKET_LEN: usize = 188;
/// MPEG-TS sync byte at the start of every packet.
const TS_SYNC: u8 = 0x47;
/// How many stereo frames per ffmpeg stdin write (~93ms at 44100Hz).
const FEED_FRAMES: usize = 4096;
/// Silence filler length when the library has no ready track.
const FILLER_SILENCE_SECS: f32 = 2.0;
/// How long to wait before retrying an empty library (avoids busy-spin).
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// How long to wait for ffmpeg TS output before cutting a segment.
const SEGMENT_WAIT: Duration = Duration::from_secs(15);
/// Shorter wait for the end-of-track flush (best-effort, never stalls).
const FLUSH_WAIT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for ffmpeg stdout.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Don't repeat the same jingle immediately.
const JINGLE_RECENT_WINDOW: usize = 3;

/// File-only HLS output configuration (from CLI).
pub struct HlsConfig {
    /// Directory receiving `live.m3u8` + `seg_*.ts` (served externally).
    pub hls_dir: PathBuf,
    /// Target segment duration in seconds (cut on TS packet boundaries).
    pub segment_secs: f32,
    /// How many recent segments stay listed (older files are deleted).
    pub window: usize,
    /// Directory of filler jingles; silence when unset or empty.
    pub jingle_dir: Option<PathBuf>,
    /// ffmpeg binary for AAC+TS encoding (`ffmpeg` in PATH by default).
    pub ffmpeg: PathBuf,
}

/// Validate HLS output options before streaming starts.
pub fn check_hls_options(segment_secs: f32, window: usize) -> Result<()> {
    if !segment_secs.is_finite() || !(2.0..=30.0).contains(&segment_secs) {
        anyhow::bail!("--segment-secs must be between 2 and 30 seconds");
    }
    if !(3..=50).contains(&window) {
        anyhow::bail!("--hls-window must be between 3 and 50 segments");
    }
    Ok(())
}

/// Fail fast when ffmpeg is missing or cannot run.
pub async fn check_ffmpeg(ffmpeg: &Path) -> Result<()> {
    let out = tokio::process::Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-version")
        .output()
        .await
        .with_context(|| format!("failed to run {} -version; is ffmpeg installed?", ffmpeg.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg check failed ({} -version exited {}); check --ffmpeg",
            ffmpeg.display(),
            out.status
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.to_lowercase().contains("ffmpeg") {
        anyhow::bail!(
            "{} -version did not look like ffmpeg; check --ffmpeg",
            ffmpeg.display()
        );
    }
    Ok(())
}

/// CLI args for the continuous AAC-TS encoder session.
fn ffmpeg_args(bitrate_kbps: u32) -> Vec<String> {
    vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-ar".to_string(),
        BUS_RATE.to_string(),
        "-ac".to_string(),
        "2".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        format!("{bitrate_kbps}k"),
        "-ar".to_string(),
        BUS_RATE.to_string(),
        "-ac".to_string(),
        "2".to_string(),
        "-f".to_string(),
        "mpegts".to_string(),
        "pipe:1".to_string(),
    ]
}

/// Split `buf` into complete MPEG-TS packets: resync to the first `0x47`,
/// then take whole 188-byte packets, leaving the trailing partial packet
/// in the buffer for the next push.
fn take_complete_ts_packets(buf: &mut Vec<u8>) -> Vec<u8> {
    let Some(start) = buf.iter().position(|&b| b == TS_SYNC) else {
        buf.clear();
        return Vec::new();
    };
    if start > 0 {
        buf.drain(..start);
    }
    let packets = buf.len() / TS_PACKET_LEN;
    if packets == 0 {
        return Vec::new();
    }
    let end = packets * TS_PACKET_LEN;
    buf.drain(..end).collect()
}

/// One continuous ffmpeg AAC-TS encoder: s16le in, MPEG-TS out.
struct FfmpegTsSession {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    buf: Arc<Mutex<Vec<u8>>>,
    stderr_tail: Arc<Mutex<String>>,
    _reader: tokio::task::JoinHandle<()>,
    _stderr: tokio::task::JoinHandle<()>,
}

impl FfmpegTsSession {
    async fn spawn(ffmpeg: &Path, bitrate_kbps: u32) -> Result<Self> {
        let mut child = tokio::process::Command::new(ffmpeg)
            .args(ffmpeg_args(bitrate_kbps))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {}", ffmpeg.display()))?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().context("ffmpeg stdout not piped")?;
        let stderr = child.stderr.take().context("ffmpeg stderr not piped")?;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail = Arc::new(Mutex::new(String::new()));

        let reader_buf = buf.clone();
        let reader = tokio::spawn(async move {
            let mut chunk = vec![0u8; 32_768];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        reader_buf.lock().await.extend_from_slice(&chunk[..n]);
                    }
                    Err(_) => break,
                }
            }
        });
        let tail = stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                let n = {
                    use tokio::io::AsyncBufReadExt;
                    async { reader.read_line(&mut line).await }
                }
                .await
                .unwrap_or(0);
                if n == 0 {
                    break;
                }
                crate::warn!("deezco: ffmpeg: {}", line.trim_end());
                let mut guard = tail.lock().await;
                guard.push_str(&line);
                if guard.len() > 8192 {
                    let skip = guard.len() - 8192;
                    guard.drain(..skip);
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            buf,
            stderr_tail,
            _reader: reader,
            _stderr: stderr_task,
        })
    }

    /// True while the ffmpeg child is still running.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn stderr_tail_text(&self) -> String {
        self.stderr_tail.lock().await.clone()
    }

    /// Feed interleaved stereo s16 samples to ffmpeg stdin.
    async fn feed_s16(&mut self, samples: &[i16]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        if !self.is_alive() {
            anyhow::bail!(
                "ffmpeg exited while feeding audio: {}",
                self.stderr_tail_text().await.trim()
            );
        }
        // Manual LE encoding avoids an extra crate; 16KB per chunk is cheap.
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let stdin = self.stdin.as_mut().context("ffmpeg stdin closed")?;
        stdin.write_all(&bytes).await.context("ffmpeg stdin write failed")?;
        Ok(())
    }

    /// Wait until at least `min_bytes` of TS output are buffered.
    /// Caller decides how long to wait; see `wait_for_encoder_output`.
    async fn wait_for_bytes(&self, min_bytes: usize, wait: Duration) -> Result<bool> {
        let res = timeout(wait, async {
            loop {
                if self.buf.lock().await.len() >= min_bytes {
                    return;
                }
                sleep(POLL_INTERVAL).await;
            }
        })
        .await;
        Ok(res.is_ok())
    }

    /// Take all complete TS packets currently buffered.
    async fn take_complete_packets(&self) -> Vec<u8> {
        let mut guard = self.buf.lock().await;
        take_complete_ts_packets(&mut guard)
    }

    /// Wait (patiently) for the encoder to emit at least `min_bytes`.
    /// Feeding more input while ffmpeg is behind only piles pipe backlog,
    /// so this blocks instead of returning early; the only exits are
    /// output arriving or ffmpeg dying (clean error, no silent stall).
    async fn wait_for_encoder_output(&mut self, min_bytes: usize) -> Result<()> {
        loop {
            if self.wait_for_bytes(min_bytes, SEGMENT_WAIT).await? {
                return Ok(());
            }
            if !self.is_alive() {
                anyhow::bail!(
                    "ffmpeg exited while waiting for output: {}",
                    self.stderr_tail_text().await.trim()
                );
            }
            crate::warn!("deezco: HLS-TS waiting for ffmpeg output (encoder is behind)");
        }
    }

    /// Kill the child (best-effort, for error paths).
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

/// One listed segment in the live playlist.
struct SegmentEntry {
    seq: u64,
    filename: String,
    duration: f32,
    title: String,
}

/// Sliding-window live playlist (`live.m3u8`, no `ENDLIST`).
struct Playlist {
    entries: VecDeque<SegmentEntry>,
    seq_next: u64,
    window: usize,
    target_duration: u32,
}

impl Playlist {
    fn new(segment_secs: f32, window: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            seq_next: 0,
            window,
            target_duration: segment_secs.ceil().max(1.0) as u32,
        }
    }

    fn segment_filename(seq: u64) -> String {
        format!("seg_{seq:05}.ts")
    }

    fn push(&mut self, duration: f32, title: String) -> (u64, String) {
        let seq = self.seq_next;
        self.seq_next += 1;
        let filename = Self::segment_filename(seq);
        self.entries.push_back(SegmentEntry {
            seq,
            filename: filename.clone(),
            duration,
            title,
        });
        while self.entries.len() > self.window {
            self.entries.pop_front();
        }
        (seq, filename)
    }

    /// Sequence numbers evicted from the window (callers delete the files).
    fn media_sequence(&self) -> u64 {
        self.entries.front().map_or(self.seq_next, |e| e.seq)
    }

    fn content(&self) -> String {
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        out.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            self.target_duration,
            self.media_sequence()
        ));
        for e in &self.entries {
            out.push_str(&format!("#EXTINF:{:.3},{}\n{}\n", e.duration, e.title, e.filename));
        }
        out
    }
}

/// Wall-clock pacer for segment emission. The encoder runs at CPU speed, so
/// without pacing the window races minutes ahead within seconds of launch:
/// players joining "live" start mid-library, then skip whenever their
/// buffered segments fall out of the window. Deadline-based (not
/// `sleep(duration)` per segment): the deadline advances by exactly the
/// emitted audio duration, so encode time is subtracted from the next sleep
/// instead of accumulating as drift. `sleep_until` with a past deadline
/// returns immediately, so a slow CPU degrades to best-effort instead of
/// stalling.
struct Pacer {
    deadline: Option<Instant>,
}

/// Encode debt beyond this is dropped (slow CPU / stall) instead of being
/// caught up forever without sleep.
const MAX_PACING_DEBT: Duration = Duration::from_secs(5);

impl Pacer {
    fn new() -> Self {
        Self { deadline: None }
    }

    /// Publish now, then sleep so the *next* segment lands on schedule.
    /// The first emission publishes immediately and starts the clock.
    async fn pace(&mut self, duration_secs: f32) {
        let step = Duration::from_secs_f32(duration_secs.max(0.0));
        let now = Instant::now();
        let deadline = match self.deadline {
            None => now + step,
            Some(d) if now > d + MAX_PACING_DEBT => now + step,
            Some(d) => {
                sleep_until(d).await;
                d + step
            }
        };
        self.deadline = Some(deadline);
    }
}

/// Write the playlist atomically (temp file + rename) so readers never see
/// a half-written `live.m3u8`.
async fn write_playlist(dir: &Path, playlist: &Playlist) -> Result<()> {
    let tmp = dir.join(PLAYLIST_TMP);
    let final_path = dir.join(PLAYLIST_NAME);
    tokio::fs::write(&tmp, playlist.content())
        .await
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &final_path)
        .await
        .with_context(|| format!("failed to publish {}", final_path.display()))?;
    Ok(())
}

/// Scan a directory for filler jingles (mp3/flac, non-recursive, sorted).
fn collect_jingle_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_music_file(&path) {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Remove a previous run's artifacts so a restart begins at seg_00000 with
/// a fresh playlist instead of leaving stale segments next to the new ones.
/// Only our own filenames are touched; anything else in the dir is kept.
async fn clear_stale_hls_artifacts(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = name == PLAYLIST_NAME
            || name == PLAYLIST_TMP
            || (name.starts_with("seg_") && name.ends_with(".ts"))
            || (name.starts_with("seg_") && name.ends_with(".mp3"));
        if !ours {
            continue;
        }
        if let Ok(ft) = entry.file_type().await
            && ft.is_file()
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Stream local files to `--hls-dir` forever as a live AAC-TS playlist,
/// paced to wall-clock (one segment's audio per segment duration) so the
/// window advances at 1x and players hear radio, not a fast-forward dump.
/// One continuous ffmpeg session keeps PTS stable — track boundaries never
/// need `DISCONTINUITY`.
pub async fn stream_hls(
    music_dir: PathBuf,
    refresh_secs: u64,
    pipeline: PipelineConfig,
    config: HlsConfig,
) -> Result<()> {
    check_hls_options(config.segment_secs, config.window)?;
    crate::icecast::check_prerequisites(&pipeline)?;
    check_ffmpeg(&config.ffmpeg).await?;
    tokio::fs::create_dir_all(&config.hls_dir)
        .await
        .with_context(|| format!("failed to create {}", config.hls_dir.display()))?;
    tokio::fs::create_dir_all(&music_dir).await?;
    clear_stale_hls_artifacts(&config.hls_dir).await;

    // Session AAC bitrate: explicit --bitrate wins, otherwise 128k.
    let encode_kbps = config_bitrate(&pipeline);
    let mut session = FfmpegTsSession::spawn(&config.ffmpeg, encode_kbps).await?;
    let queue = LocalFileQueue::new(music_dir.clone(), Duration::from_secs(refresh_secs));
    let stereo_lib = match &pipeline.stereo_lib {
        Some(cfg) => {
            let handle = crate::stereo_lib::StereoLibHandle::open(cfg)?;
            Some(Arc::new(std::sync::Mutex::new(handle)))
        }
        None => None,
    };

    let jingle_files = config
        .jingle_dir
        .as_deref()
        .map(collect_jingle_files)
        .unwrap_or_default();
    let mut recent_jingles: VecDeque<PathBuf> = VecDeque::new();
    let mut playlist = Playlist::new(config.segment_secs, config.window);
    let mut pacer = Pacer::new();
    let mut tail: Vec<f32> = Vec::new();
    let segment_frames = (config.segment_secs * BUS_RATE as f32).round().max(1.0) as u64;
    let mut pending_frames: u64 = 0;

    crate::info!(
        "deezco: writing HLS-TS to {} ({}s segments, window {}, {}k AAC via {})",
        config.hls_dir.display(),
        config.segment_secs,
        config.window,
        encode_kbps,
        config.ffmpeg.display()
    );
    write_playlist(&config.hls_dir, &playlist).await?;

    loop {
        let (title, pcm, is_filler) = match queue.next_file().await {
            Ok(track) => match load_file_pcm(&track.path).await {
                Ok(pcm) => (track.title.clone(), pcm, false),
                Err(err) => {
                    crate::warn!("deezco: skipping {}: {err}", track.path.display());
                    continue;
                }
            },
            Err(NextTrackError::Empty | NextTrackError::Fetch(_)) => {
                // Empty library: bridge with a jingle or silence so the
                // playlist keeps advancing instead of stalling.
                match next_filler_pcm(&jingle_files, &mut recent_jingles).await {
                    Some((title, pcm)) => (title, pcm, true),
                    None => {
                        sleep(RETRY_DELAY).await;
                        continue;
                    }
                }
            }
        };
        let processed = process_pcm(pcm, &title, &pipeline, &stereo_lib, &mut tail).await?;
        if processed.is_empty() {
            continue;
        }
        if !session.is_alive() {
            anyhow::bail!(
                "ffmpeg exited during stream: {}",
                session.stderr_tail_text().await.trim()
            );
        }
        let s16 = f32_to_s16_stereo(&processed);
        let frames_total = (s16.len() / 2) as u64;
        let mut fed = 0u64;
        while fed < frames_total {
            let end = ((fed + FEED_FRAMES as u64).min(frames_total) * 2) as usize;
            let start = (fed * 2) as usize;
            if let Err(err) = session.feed_s16(&s16[start..end]).await {
                let tail = session.stderr_tail_text().await;
                session.shutdown().await;
                anyhow::bail!("ffmpeg feed failed: {err}; ffmpeg: {}", tail.trim());
            }
            fed += ((end - start) / 2) as u64;
            pending_frames += ((end - start) / 2) as u64;
            if pending_frames >= segment_frames {
                // Duration comes from fed PCM; TS bytes only need to have
                // arrived (ffmpeg lag is <1s, well within one segment).
                // Block here (don't feed ahead) until the encoder emits.
                session.wait_for_encoder_output(TS_PACKET_LEN).await?;
                let bytes = session.take_complete_packets().await;
                if bytes.is_empty() {
                    continue;
                }
                let duration = pending_frames as f32 / BUS_RATE as f32;
                pending_frames = 0;
                emit_segment(&config.hls_dir, &mut playlist, &mut pacer, bytes, duration, &title)
                    .await?;
            }
        }
        // A real track shorter than the segment target must not strand its
        // audio behind the next track: flush whole packets now as a short
        // segment. Filler is synthetic and endless, so it accumulates toward
        // full segments instead.
        if is_filler {
            continue;
        }
        if pending_frames > 0
            && session
                .wait_for_bytes(TS_PACKET_LEN, FLUSH_WAIT)
                .await
                .unwrap_or(false)
        {
            let bytes = session.take_complete_packets().await;
            if !bytes.is_empty() {
                let duration = pending_frames as f32 / BUS_RATE as f32;
                pending_frames = 0;
                emit_segment(&config.hls_dir, &mut playlist, &mut pacer, bytes, duration, &title)
                    .await?;
            }
        }
        crate::info!("deezco: HLS-TS encoded \"{title}\"");
    }
}

/// Explicit --bitrate wins; HLS file output otherwise defaults to 128k so
/// the playlist format is stable without any pipeline flags.
fn config_bitrate(pipeline: &PipelineConfig) -> u32 {
    pipeline.bitrate.unwrap_or(128)
}

/// Decode one audio file to bus PCM (blocking hop, like the Icecast path).
async fn load_file_pcm(path: &Path) -> Result<Vec<f32>> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("stream read failed for {}", path.display()))?;
    if bytes.is_empty() {
        anyhow::bail!("file {} is empty", path.display());
    }
    tokio::task::spawn_blocking(move || {
        SymphoniaDecoder::new()
            .decode(&bytes)
            .map(|buf| buf.into_samples())
    })
    .await
    .context("decoder task panicked")?
    .with_context(|| format!("decode failed for {}", path.display()))
}

/// Next filler PCM: a random jingle (non-repeating) or 2s silence.
async fn next_filler_pcm(
    jingles: &[PathBuf],
    recent: &mut VecDeque<PathBuf>,
) -> Option<(String, Vec<f32>)> {
    let candidates: Vec<&PathBuf> = jingles
        .iter()
        .filter(|p| !recent.contains(p))
        .collect();
    let pool: Vec<&PathBuf> = if candidates.is_empty() {
        jingles.iter().collect()
    } else {
        candidates
    };
    if let Some(path) = pool.first().map(|p| (*p).clone()) {
        recent.push_back(path.clone());
        if recent.len() > JINGLE_RECENT_WINDOW {
            recent.pop_front();
        }
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Jingle")
            .to_string();
        let display = format!("Jingle - {title}");
        match load_file_pcm(&path).await {
            Ok(pcm) if !pcm.is_empty() => return Some((display, pcm)),
            Ok(_) => crate::warn!("deezco: jingle {} decoded empty", path.display()),
            Err(err) => crate::warn!("deezco: jingle {} failed: {err}", path.display()),
        }
    }
    let frames = (BUS_RATE as f32 * FILLER_SILENCE_SECS) as usize;
    Some(("Silence".to_string(), vec![0.0; frames * 2]))
}

/// Loudness → crossfade → DSP for one track's PCM (blocking DSP hop).
async fn process_pcm(
    mut pcm: Vec<f32>,
    title: &str,
    pipeline: &PipelineConfig,
    stereo_lib: &Option<Arc<std::sync::Mutex<crate::stereo_lib::StereoLibHandle>>>,
    tail: &mut Vec<f32>,
) -> Result<Vec<f32>> {
    if pcm.is_empty() {
        return Ok(pcm);
    }
    if let Some(target) = pipeline.loudness
        && let Some(correction) = crate::loudness::analyze(&pcm, BUS_RATE, target)
    {
        crate::loudness::apply_correction(&mut pcm, correction.gain_db);
    }
    let mut pcm = if pipeline.crossfade.is_enabled() {
        let (out, new_tail) = crate::audio::render_track_overlap(
            tail,
            pcm,
            pipeline.crossfade.overlap_frames(),
            pipeline.crossfade.curve,
        );
        *tail = new_tail;
        out
    } else {
        // Without crossfade the whole track is the body; still hold the
        // configured tail so enabling crossfade later stays aligned. When
        // disabled there is no tail to hold.
        pcm
    };
    let pipeline_clone = pipeline.clone();
    let stereo_clone = stereo_lib.clone();
    let title_owned = title.to_string();
    tokio::task::spawn_blocking(move || {
        let mut chain = pipeline_clone.build_chain();
        if let Some(shared) = &stereo_clone {
            let reset = pipeline_clone
                .stereo_lib
                .as_ref()
                .is_some_and(|c| c.reset_per_track);
            chain.push(crate::stereo_lib::StereoLibProcessor::shared(
                shared.clone(),
                reset,
            ));
        }
        if chain.is_empty() {
            return Ok(pcm);
        }
        crate::stereo_lib::set_process_label(title_owned.clone());
        let res = chain.process(&mut pcm);
        crate::stereo_lib::clear_process_label();
        res?;
        Ok(pcm)
    })
    .await
    .context("DSP task panicked")?
}

/// Write one segment file, append it to the playlist, publish the playlist,
/// then hold the wall-clock schedule before the next emission. Segment
/// files that fell out of the window are deleted (best-effort).
async fn emit_segment(
    dir: &Path,
    playlist: &mut Playlist,
    pacer: &mut Pacer,
    bytes: Vec<u8>,
    duration: f32,
    title: &str,
) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let (seq, filename) = playlist.push(duration, title.to_string());
    tokio::fs::write(dir.join(&filename), &bytes)
        .await
        .with_context(|| format!("failed to write {filename}"))?;
    write_playlist(dir, playlist).await?;
    // Evict segment files outside the window (best-effort).
    let oldest_listed = playlist.media_sequence();
    // Filenames are monotonic; anything older than the window is garbage.
    // Only the just-evicted tail can exist, so a bounded scan suffices.
    for old in seq.saturating_sub(playlist.window as u64 + 4)..oldest_listed {
        let stale = dir.join(Playlist::segment_filename(old));
        if stale.is_file() {
            let _ = tokio::fs::remove_file(&stale).await;
        }
    }
    crate::info!(
        "deezco: HLS-TS seg {seq:05} ({duration:.2}s, {} bytes, \"{title}\")",
        bytes.len()
    );
    // Hold the wall-clock schedule: the next segment must not land before
    // this one's audio has (almost) played out.
    pacer.pace(duration).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_packets_cut_on_188_boundaries() {
        // Two full packets + a partial tail: only the full packets are taken.
        let mut buf = vec![0u8; 188 * 2 + 100];
        for (i, chunk) in buf.chunks_mut(188).enumerate() {
            if !chunk.is_empty() {
                chunk[0] = TS_SYNC;
            }
            let _ = i;
        }
        buf[188 * 2] = 0x00; // partial tail is not a packet start
        let taken = take_complete_ts_packets(&mut buf);
        assert_eq!(taken.len(), 188 * 2);
        assert_eq!(buf.len(), 100);
    }

    #[test]
    fn ts_packets_resync_to_sync_byte() {
        // Leading garbage is discarded up to the first sync byte.
        let mut buf = vec![0x00, 0xFF, TS_SYNC];
        buf.extend(vec![0x11; 187]);
        buf.extend(vec![0x22; 50]);
        let taken = take_complete_ts_packets(&mut buf);
        assert_eq!(taken.len(), 188);
        assert_eq!(taken[0], TS_SYNC);
        assert_eq!(buf.len(), 50);
    }

    #[test]
    fn ts_packets_empty_without_sync() {
        let mut buf = vec![0x00; 500];
        let taken = take_complete_ts_packets(&mut buf);
        assert!(taken.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn ffmpeg_args_request_aac_in_mpegts() {
        let args = ffmpeg_args(128);
        let joined = args.join(" ");
        assert!(joined.contains("-f s16le"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-b:a 128k"), "{joined}");
        assert!(joined.contains("-f mpegts"), "{joined}");
        assert!(joined.contains("pipe:0"), "{joined}");
        assert!(joined.contains("pipe:1"), "{joined}");
        // Bus rate is the single sample rate on both ends.
        assert_eq!(args.iter().filter(|a| *a == "44100").count(), 2);
    }

    #[test]
    fn playlist_slides_with_media_sequence() {
        let mut pl = Playlist::new(6.0, 3);
        assert!(pl.content().contains("#EXT-X-MEDIA-SEQUENCE:0"));
        for i in 0..5 {
            pl.push(6.0, format!("t{i}"));
        }
        // Window of 3 keeps seq 2,3,4; media sequence advanced to 2.
        assert_eq!(pl.entries.len(), 3);
        assert_eq!(pl.media_sequence(), 2);
        let content = pl.content();
        assert!(content.contains("#EXT-X-TARGETDURATION:6"));
        assert!(content.contains("#EXT-X-MEDIA-SEQUENCE:2"));
        assert!(content.contains("seg_00004.ts"));
        assert!(!content.contains("seg_00001.ts"));
        assert!(!content.contains("EXT-X-ENDLIST"));
    }

    #[test]
    fn hls_options_reject_out_of_range() {
        assert!(check_hls_options(6.0, 6).is_ok());
        assert!(check_hls_options(0.5, 6).is_err());
        assert!(check_hls_options(60.0, 6).is_err());
        assert!(check_hls_options(f32::NAN, 6).is_err());
        assert!(check_hls_options(6.0, 2).is_err());
        assert!(check_hls_options(6.0, 100).is_err());
    }

    #[test]
    fn hls_defaults_to_128k_without_override() {
        assert_eq!(config_bitrate(&PipelineConfig::default()), 128);
        let forced = PipelineConfig {
            bitrate: Some(64),
            ..PipelineConfig::default()
        };
        assert_eq!(config_bitrate(&forced), 64);
    }

    #[tokio::test]
    async fn pacer_holds_wall_clock_schedule() {
        let mut pacer = Pacer::new();
        // First emission publishes immediately and starts the clock.
        let start = Instant::now();
        pacer.pace(0.05).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        // The next 50ms of audio costs ~50ms of wall-clock.
        pacer.pace(0.05).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(40), "elapsed: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "elapsed: {elapsed:?}");
    }
}
