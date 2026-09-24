use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use tokio::sync::Mutex;

use crate::api::DeezerApi;
use crate::files::is_music_file;
use crate::models::GwTrack;

/// The no-repeat guard: a reshuffled queue never opens with a track that
/// was served recently (the last few pops, up to `RECENT_WINDOW`). Swaps the
/// front with the first id outside the window; a playlist smaller than the
/// window has nothing left to swap with and plays as shuffled.
const RECENT_WINDOW: usize = 3;

struct PlaylistQueue {
    tracks: VecDeque<GwTrack>,
    last_fetch: Option<Instant>,
    recent: VecDeque<String>,
}

#[derive(Clone)]
pub struct TrackQueue {
    refresh: Duration,
    queues: Arc<Mutex<HashMap<String, PlaylistQueue>>>,
}

pub enum NextTrackError {
    /// No playable tracks (empty playlist / empty music-dir)
    Empty,
    /// Fetching failed and no cache exists
    Fetch(String),
}

impl TrackQueue {
    pub fn new(refresh: Duration) -> Self {
        Self {
            refresh,
            queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Pop the next track for a playlist. Each playlist is fetched once,
    /// shuffled, and walked in order; it is refetched and reshuffled when the
    /// queue runs out or when the last fetch is older than `refresh` — so
    /// edits made on the Deezer web page show up within one refresh interval.
    /// A failed fetch keeps the cached queue rather than interrupting.
    pub async fn next_track(
        &self,
        api: &DeezerApi,
        playlist_id: &str,
    ) -> Result<GwTrack, NextTrackError> {
        // Fast path: queue already has fresh tracks — pop without any I/O
        // and without holding the lock across an await.
        {
            let mut queues = self.queues.lock().await;
            if let Some(queue) = queues.get_mut(playlist_id) {
                let stale = queue
                    .last_fetch
                    .is_none_or(|fetched| fetched.elapsed() >= self.refresh);
                if !queue.tracks.is_empty() && !stale {
                    let track = queue.tracks.pop_front().ok_or(NextTrackError::Empty)?;
                    queue.recent.push_back(track.id_str());
                    if queue.recent.len() > RECENT_WINDOW {
                        queue.recent.pop_front();
                    }
                    return Ok(track);
                }
            }
        }

        // Need a refresh (queue missing, empty, or stale). Fetch outside
        // the queue lock so concurrent callers aren't blocked on the network
        // (MutexGuard must not be held across .await).
        if crate::track::debug_enabled() {
            crate::warn!("deezco: refreshing playlist ({playlist_id})");
        }
        let fetched = api.get_playlist_tracks(playlist_id).await;

        // Re-acquire the queue lock to publish the fetched tracks. Another
        // concurrent caller may have already refreshed while we were in the
        // network call — re-check staleness and avoid clobbering its fresh
        // queue if we lost the race.
        let mut queues = self.queues.lock().await;
        let queue = queues
            .entry(playlist_id.to_string())
            .or_insert_with(|| PlaylistQueue {
                tracks: VecDeque::new(),
                last_fetch: None,
                recent: VecDeque::new(),
            });
        let stale = queue
            .last_fetch
            .is_none_or(|fetched| fetched.elapsed() >= self.refresh);
        let needs_update = queue.tracks.is_empty() || stale;
        match fetched {
            Ok(tracks) if !tracks.is_empty() => {
                if needs_update {
                    let mut tracks = tracks;
                    tracks.shuffle(&mut rand::rng());
                    avoid_recent_repeat(&mut tracks, &queue.recent);
                    queue.tracks = tracks.into();
                    queue.last_fetch = Some(Instant::now());
                } else if crate::track::debug_enabled() {
                    // Another task already refreshed; keep its result and drop
                    // the redundant fetch (still shuffled, still avoids recent).
                    crate::warn!("deezco: refresh race won by concurrent task, using cached queue");
                }
            }
            Ok(_) => {} // Deezer returned no tracks: keep the cached queue.
            Err(err) if !queue.tracks.is_empty() => {
                crate::warn!("deezco: playlist fetch failed, keeping cache: {err}");
            }
            Err(err) => {
                return Err(NextTrackError::Fetch(err.to_string()));
            }
        }
        let track = queue.tracks.pop_front().ok_or(NextTrackError::Empty)?;
        queue.recent.push_back(track.id_str());
        if queue.recent.len() > RECENT_WINDOW {
            queue.recent.pop_front();
        }
        Ok(track)
    }
}

fn avoid_recent_repeat(tracks: &mut [GwTrack], recent: &VecDeque<String>) {
    let Some(front) = tracks.first().map(|t| t.id_str()) else {
        return;
    };
    if !recent.contains(&front) {
        return;
    }
    if let Some(swap_idx) = tracks.iter().position(|t| !recent.contains(&t.id_str())) {
        tracks.swap(0, swap_idx);
    }
}

/// A local audio file ready for streaming: path + human title.
/// `duration_secs` / `actual_kbps` are filled after decode (0/None = unknown,
/// pacing falls back to the nominal rate).
#[derive(Clone, Debug)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub title: String,
    pub duration_secs: u64,
    pub actual_kbps: Option<u32>,
}

impl LocalTrack {
    pub fn display_name(&self) -> String {
        self.title.clone()
    }

    pub fn duration_secs(&self) -> u64 {
        self.duration_secs
    }
}

/// Human title for a file: stem without extension, e.g. `Artist - Title`.
pub fn local_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Track")
        .to_string()
}

struct LocalQueueState {
    files: VecDeque<PathBuf>,
    last_scan: Option<Instant>,
    recent: VecDeque<PathBuf>,
}

/// Pure-local station library: recursive mp3/flac scan of `--music-dir`,
/// shuffled + looped with a no-repeat guard. Rescanned when empty or when
/// the last scan is older than `refresh`, so files the admin downloads
/// appear without restarting the stream.
#[derive(Clone)]
pub struct LocalFileQueue {
    refresh: Duration,
    music_dir: PathBuf,
    state: Arc<Mutex<LocalQueueState>>,
}

impl LocalFileQueue {
    pub fn new(music_dir: PathBuf, refresh: Duration) -> Self {
        Self {
            refresh,
            music_dir,
            state: Arc::new(Mutex::new(LocalQueueState {
                files: VecDeque::new(),
                last_scan: None,
                recent: VecDeque::new(),
            })),
        }
    }

    pub fn music_dir(&self) -> &Path {
        &self.music_dir
    }

    /// Pop the next file for playback.
    pub async fn next_file(&self) -> Result<LocalTrack, NextTrackError> {
        // Fast path: fresh queue with files — pop without I/O.
        {
            let mut state = self.state.lock().await;
            let stale = state.last_scan.is_none_or(|t| t.elapsed() >= self.refresh);
            if !state.files.is_empty() && !stale {
                let path = state.files.pop_front().ok_or(NextTrackError::Empty)?;
                state.recent.push_back(path.clone());
                if state.recent.len() > RECENT_WINDOW {
                    state.recent.pop_front();
                }
                return Ok(LocalTrack {
                    title: local_title(&path),
                    path,
                    duration_secs: 0,
                    actual_kbps: None,
                });
            }
        }

        // Need a rescan (empty or stale). Scan outside the lock.
        let scanned = collect_music_files(&self.music_dir).await;

        let mut state = self.state.lock().await;
        let stale = state.last_scan.is_none_or(|t| t.elapsed() >= self.refresh);
        let needs_update = state.files.is_empty() || stale;
        match scanned {
            Ok(mut files) if !files.is_empty() => {
                if needs_update {
                    files.sort();
                    files.shuffle(&mut rand::rng());
                    let mut files = files;
                    avoid_recent_repeat_paths(&mut files, &state.recent);
                    state.files = files.into();
                    state.last_scan = Some(Instant::now());
                }
            }
            Ok(_) => {
                // No files on disk: keep cached queue if any.
                if state.files.is_empty() {
                    return Err(NextTrackError::Empty);
                }
            }
            Err(err) if !state.files.is_empty() => {
                crate::warn!("deezco: music-dir scan failed, keeping cache: {err}");
            }
            Err(err) => {
                return Err(NextTrackError::Fetch(err.to_string()));
            }
        }
        let path = state.files.pop_front().ok_or(NextTrackError::Empty)?;
        state.recent.push_back(path.clone());
        if state.recent.len() > RECENT_WINDOW {
            state.recent.pop_front();
        }
        Ok(LocalTrack {
            title: local_title(&path),
            path,
            duration_secs: 0,
            actual_kbps: None,
        })
    }
}

fn avoid_recent_repeat_paths(files: &mut [PathBuf], recent: &VecDeque<PathBuf>) {
    let Some(front) = files.first() else {
        return;
    };
    if !recent.contains(front) {
        return;
    }
    if let Some(idx) = files.iter().position(|p| !recent.contains(p)) {
        files.swap(0, idx);
    }
}

/// Recursive mp3/flac scan (sorted by caller before shuffle).
async fn collect_music_files(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && is_music_file(&path) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> GwTrack {
        GwTrack {
            sng_id: serde_json::json!(id),
            sng_title: None,
            md5_origin: None,
            media_version: None,
            art_name: None,
            art_id: None,
            track_token: None,
            filesize_mp3_128: None,
            filesize_mp3_320: None,
            filesize_flac: None,
            duration: None,
        }
    }

    fn recent(ids: &[&str]) -> VecDeque<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_repeat_guard_swaps_a_recent_front_track_out() {
        let mut tracks = vec![track("1"), track("2"), track("3")];
        avoid_recent_repeat(&mut tracks, &recent(&["1", "9"]));
        assert_ne!(tracks[0].id_str(), "1", "front must not be recently served");
        assert!(tracks.iter().any(|t| t.id_str() == "1"));
    }

    #[test]
    fn no_repeat_guard_leaves_a_fresh_front_untouched() {
        let mut tracks = vec![track("7"), track("1"), track("2")];
        avoid_recent_repeat(&mut tracks, &recent(&["1", "2"]));
        assert_eq!(tracks[0].id_str(), "7");
    }

    #[test]
    fn no_repeat_guard_handles_a_playlist_smaller_than_the_window() {
        // Everything is recent: nothing to swap with, play as shuffled.
        let mut tracks = vec![track("1")];
        avoid_recent_repeat(&mut tracks, &recent(&["1"]));
        assert_eq!(tracks[0].id_str(), "1");
    }
}
