use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rand::seq::SliceRandom;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::api::DeezerApi;
use crate::download;
use crate::models::{GwTrack, TrackFormat};

#[derive(Clone)]
struct ServeState {
    api: DeezerApi,
    format: TrackFormat,
    host: String,
    port: u16,
    refresh: Duration,
    queues: Arc<Mutex<HashMap<String, PlaylistQueue>>>,
}

struct PlaylistQueue {
    tracks: VecDeque<GwTrack>,
    last_fetch: Option<Instant>,
    recent: VecDeque<String>,
}

/// The no-repeat guard: a reshuffled queue never opens with a track that
/// was served recently (the last few pops, up to `RECENT_WINDOW`). Swaps the
/// front with the first id outside the window; a playlist smaller than the
/// window has nothing left to swap with and plays as shuffled.
const RECENT_WINDOW: usize = 3;

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

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn track_json(track: &GwTrack, url: &str) -> Value {
    json!({
        "id": track.id_str(),
        "title": track.title(),
        "artist": track.artist(),
        "duration": track.duration_secs(),
        "url": url,
    })
}

/// Pop the next track for a playlist. Each playlist is fetched once, shuffled,
/// and walked in order; it is refetched and reshuffled when the queue runs
/// out or when the last fetch is older than `refresh` — so edits made on the
/// Deezer web page show up within one refresh interval. A failed fetch keeps
/// the cached queue rather than interrupting playback.
async fn next_track(state: &ServeState, playlist_id: &str) -> Result<GwTrack, ApiError> {
    let mut queues = state.queues.lock().await;
    let queue = queues.entry(playlist_id.to_string()).or_insert_with(|| PlaylistQueue {
        tracks: VecDeque::new(),
        last_fetch: None,
        recent: VecDeque::new(),
    });
    let stale = queue
        .last_fetch
        .is_none_or(|fetched| fetched.elapsed() >= state.refresh);
    if queue.tracks.is_empty() || stale {
        match state.api.get_playlist_tracks(playlist_id).await {
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
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    err.to_string(),
                ));
            }
        }
    }
    let track = queue
        .tracks
        .pop_front()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "playlist has no playable tracks"));
    if let Ok(t) = &track {
        queue.recent.push_back(t.id_str());
        if queue.recent.len() > RECENT_WINDOW {
            queue.recent.pop_front();
        }
    }
    track
}

fn request_base_url(headers: &HeaderMap, state: &ServeState) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(&state.host);
    // A bare host (no port) means the client connected on the port it
    // knows about — for crabsoup that is the bound port, not 80.
    if host.contains(':') {
        format!("http://{host}")
    } else {
        format!("http://{}:{}", host, state.port)
    }
}

async fn playlist_next(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let track = next_track(&state, &id).await?;
    let url = format!("{}/tracks/{}", request_base_url(&headers, &state), track.id_str());
    Ok(Json(track_json(&track, &url)))
}

async fn playlist_list(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let tracks = state
        .api
        .get_playlist_tracks(&id)
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
    let base = request_base_url(&headers, &state);
    let items: Vec<Value> = tracks
        .iter()
        .map(|track| {
            track_json(track, &format!("{}/tracks/{}", base, track.id_str()))
        })
        .collect();
    Ok(Json(json!({ "id": id, "count": items.len(), "tracks": items })))
}

async fn track_audio(
    State(state): State<ServeState>,
    Path(id): Path<String>,
) -> Response {
    let track = match state.api.get_track(&id).await {
        Ok(track) => track,
        Err(err) => {
            return ApiError::new(StatusCode::NOT_FOUND, err.to_string()).into_response();
        }
    };
    match download::fetch_track_audio(&state.api, &track, state.format, false).await {
        Ok(fetched) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "audio/mpeg".to_string()),
                (header::CONTENT_LENGTH, fetched.data.len().to_string()),
            ],
            fetched.data,
        )
            .into_response(),
        Err(err) => ApiError::new(StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    }
}

/// Run the HTTP server until the process is stopped.
pub async fn serve(
    api: DeezerApi,
    format: TrackFormat,
    host: &str,
    port: u16,
    refresh_secs: u64,
) -> Result<()> {
    let state = ServeState {
        api,
        format,
        host: host.to_string(),
        port,
        refresh: Duration::from_secs(refresh_secs),
        queues: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/playlists/{id}/next", get(playlist_next))
        .route("/playlists/{id}", get(playlist_list))
        .route("/tracks/{id}", get(track_audio))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    println!(
        "deezco serving playlists on http://{}:{} (refresh every {}s)",
        host, port, refresh_secs
    );
    axum::serve(listener, app).await?;
    Ok(())
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