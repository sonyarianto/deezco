use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use tokio::sync::Mutex;

use crate::api::DeezerApi;
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
    names: Arc<Mutex<HashMap<String, String>>>,
}

pub enum NextTrackError {
    /// The playlist has no playable tracks
    Empty,
    /// Fetching the playlist from Deezer failed and no cache exists
    Fetch(String),
}

impl TrackQueue {
    pub fn new(refresh: Duration) -> Self {
        Self {
            refresh,
            queues: Arc::new(Mutex::new(HashMap::new())),
            names: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Cache a playlist name for display in log messages.
    pub async fn set_name(&self, playlist_id: &str, name: &str) {
        self.names
            .lock()
            .await
            .insert(playlist_id.to_string(), name.to_string());
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
        if queue.tracks.is_empty() || stale {
            let display = self
                .names
                .lock()
                .await
                .get(playlist_id)
                .cloned()
                .unwrap_or_else(|| playlist_id.to_string());
            eprintln!("deezco: refreshing playlist \"{display}\" ({playlist_id})");
            match api.get_playlist_tracks(playlist_id).await {
                Ok(tracks) if !tracks.is_empty() => {
                    let mut tracks = tracks;
                    tracks.shuffle(&mut rand::thread_rng());
                    avoid_recent_repeat(&mut tracks, &queue.recent);
                    queue.tracks = tracks.into();
                    queue.last_fetch = Some(Instant::now());
                }
                Ok(_) => {} // Deezer returned no tracks: keep the cached queue.
                Err(err) if !queue.tracks.is_empty() => {
                    eprintln!("deezco: playlist fetch failed, keeping cache: {err}");
                }
                Err(err) => {
                    return Err(NextTrackError::Fetch(err.to_string()));
                }
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
