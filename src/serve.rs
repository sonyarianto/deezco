use std::time::Duration;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::api::DeezerApi;
use crate::download;
use crate::models::{GwTrack, TrackFormat};
use crate::queue::{NextTrackError, TrackQueue};

#[derive(Clone)]
struct ServeState {
    api: DeezerApi,
    format: TrackFormat,
    host: String,
    port: u16,
    queue: TrackQueue,
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

async fn web_index() -> Html<&'static str> {
    Html(crate::web_assets::INDEX_HTML)
}

async fn api_health(State(state): State<ServeState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "deezco-serve",
        "host": state.host,
        "port": state.port,
        "format": state.format.api_name(),
        "crabsoup_compatible": true,
        "endpoints": [
            "GET /",
            "GET /api/health",
            "GET /playlists/{id}",
            "GET /playlists/{id}/next",
            "POST /playlists/{id}/next",
            "GET /tracks/{id}"
        ]
    }))
}

/// Pop the next track for a playlist, mapping queue errors to HTTP errors.
async fn playlist_next(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let track = state
        .queue
        .next_track(&state.api, &id)
        .await
        .map_err(|err| match err {
            NextTrackError::Empty => {
                ApiError::new(StatusCode::NOT_FOUND, "playlist has no playable tracks")
            }
            NextTrackError::Fetch(message) => ApiError::new(StatusCode::BAD_GATEWAY, message),
        })?;
    let url = format!(
        "{}/tracks/{}",
        request_base_url(&headers, &state),
        track.id_str()
    );
    Ok(Json(track_json(&track, &url)))
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
        .map(|track| track_json(track, &format!("{}/tracks/{}", base, track.id_str())))
        .collect();
    Ok(Json(
        json!({ "id": id, "count": items.len(), "tracks": items }),
    ))
}

async fn track_audio(State(state): State<ServeState>, Path(id): Path<String>) -> Response {
    let track = match state.api.get_track(&id).await {
        Ok(track) => track,
        Err(err) => {
            return ApiError::new(StatusCode::NOT_FOUND, err.to_string()).into_response();
        }
    };
    match download::fetch_track_audio(&state.api, &track, state.format, false, false).await {
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
        queue: TrackQueue::new(Duration::from_secs(refresh_secs)),
    };

    let app = Router::new()
        // --- crabsoup contract: MUST NOT BREAK ---
        .route(
            "/playlists/{id}/next",
            get(playlist_next).post(playlist_next),
        )
        .route("/playlists/{id}", get(playlist_list))
        .route("/tracks/{id}", get(track_audio))
        // --- web player + control-plane (additive, no conflict) ---
        .route("/", get(web_index))
        .route("/api/health", get(api_health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    println!(
        "deezco serving playlists on http://{}:{} (refresh every {}s)",
        host, port, refresh_secs
    );
    println!(
        "web player: http://{}:{}/  |  health: http://{}:{}/api/health  |  crabsoup: /playlists/{{id}}/next, /playlists/{{id}}, /tracks/{{id}}",
        host, port, host, port
    );
    axum::serve(listener, app).await?;
    Ok(())
}
