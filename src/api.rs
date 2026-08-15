use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::models::*;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/79.0.3945.130 Safari/537.36";
const GW_API_URL: &str = "http://www.deezer.com/ajax/gw-light.php";
const MEDIA_URL: &str = "https://media.deezer.com/v1/get_url";
const PUBLIC_API_URL: &str = "https://api.deezer.com";

#[derive(Clone)]
pub struct DeezerApi {
    client: Client,
    api_token: Arc<Mutex<Option<String>>>,
    pub current_user: Arc<Mutex<Option<CurrentUser>>>,
}

impl DeezerApi {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .cookie_store(true)
            .user_agent(USER_AGENT)
            .danger_accept_invalid_certs(true)
            .build()?;

        Ok(Self {
            client,
            api_token: Arc::new(Mutex::new(None)),
            current_user: Arc::new(Mutex::new(None)),
        })
    }

    /// HTTP client used for API calls and stream downloads
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Login using ARL cookie
    pub async fn login_via_arl(&self, arl: &str) -> Result<bool> {
        // Set the ARL cookie by making a request with it
        let cookie_val = format!("arl={}", arl.trim());
        let response = self
            .client
            .get("https://www.deezer.com/")
            .header("Cookie", &cookie_val)
            .send()
            .await?;
        drop(response);

        // Get user data to validate login
        let user_data = self
            .gw_call_with_arl("deezer.getUserData", json!({}), arl)
            .await?;

        let user_id = &user_data["USER"]["USER_ID"];
        let is_zero = match user_id {
            Value::Number(n) => n.as_u64() == Some(0),
            Value::String(s) => s == "0",
            _ => true,
        };

        if is_zero {
            return Ok(false);
        }

        // Store the api token
        if let Some(check_form) = user_data["checkForm"].as_str() {
            let mut token = self.api_token.lock().await;
            *token = Some(check_form.to_string());
        } else if let Some(check_form) = user_data["checkForm"].as_u64() {
            let mut token = self.api_token.lock().await;
            *token = Some(check_form.to_string());
        }

        // Extract user info
        let license_token = user_data["USER"]["OPTIONS"]["license_token"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let uid = match user_id {
            Value::Number(n) => n.as_u64().unwrap_or(0),
            Value::String(s) => s.parse().unwrap_or(0),
            _ => 0,
        };
        let name = user_data["USER"]["BLOG_NAME"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string();

        let mut cu = self.current_user.lock().await;
        *cu = Some(CurrentUser {
            id: uid,
            name,
            license_token,
        });

        Ok(true)
    }

    /// Internal GW API call with ARL in cookie header
    async fn gw_call_with_arl(&self, method: &str, args: Value, arl: &str) -> Result<Value> {
        let api_token = if method == "deezer.getUserData" {
            "null".to_string()
        } else {
            let token = self.api_token.lock().await;
            token.clone().unwrap_or_else(|| "null".to_string())
        };

        let response = self
            .client
            .post(GW_API_URL)
            .header("Cookie", format!("arl={}", arl.trim()))
            .query(&[
                ("api_version", "1.0"),
                ("api_token", &api_token),
                ("input", "3"),
                ("method", method),
            ])
            .json(&args)
            .send()
            .await
            .context("GW API request failed")?;

        let body: Value = response
            .json()
            .await
            .context("Failed to parse GW response")?;

        if let Some(results) = body.get("results") {
            // Store checkForm token if this is getUserData
            if method == "deezer.getUserData"
                && let Some(check_form) = results.get("checkForm")
            {
                let mut token = self.api_token.lock().await;
                *token = Some(match check_form {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return Ok(results.clone()),
                });
            }
            Ok(results.clone())
        } else {
            bail!("No results in GW response for {}: {:?}", method, body)
        }
    }

    /// GW API call using cookie jar (after login)
    pub async fn gw_call(&self, method: &str, args: Value) -> Result<Value> {
        let mut retried = false;

        loop {
            let api_token = if method == "deezer.getUserData" {
                "null".to_string()
            } else {
                let token = self.api_token.lock().await;
                match token.as_ref() {
                    Some(t) => t.clone(),
                    None => {
                        drop(token);
                        self.refresh_token().await?;
                        let token = self.api_token.lock().await;
                        token.clone().unwrap_or_else(|| "null".to_string())
                    }
                }
            };

            let response = self
                .client
                .post(GW_API_URL)
                .query(&[
                    ("api_version", "1.0"),
                    ("api_token", &api_token),
                    ("input", "3"),
                    ("method", method),
                ])
                .json(&args)
                .send()
                .await
                .context(format!("GW API call failed: {}", method))?;

            let body: GwResponse = response
                .json()
                .await
                .context(format!("Failed to parse GW response for {}", method))?;

            // Check for token errors - retry once
            let err_str = body.error.to_string();
            if !retried
                && (err_str.contains("invalid api token") || err_str.contains("Invalid CSRF token"))
            {
                self.refresh_token().await?;
                retried = true;
                continue;
            }

            if body.error.is_object() && !body.error.as_object().unwrap().is_empty() {
                bail!("GW API error for {}: {}", method, body.error);
            }

            return Ok(body.results);
        }
    }

    async fn refresh_token(&self) -> Result<()> {
        let response = self
            .client
            .post(GW_API_URL)
            .query(&[
                ("api_version", "1.0"),
                ("api_token", "null"),
                ("input", "3"),
                ("method", "deezer.getUserData"),
            ])
            .json(&json!({}))
            .send()
            .await?;

        let body: GwResponse = response.json().await?;
        if let Some(check_form) = body.results.get("checkForm") {
            let mut token = self.api_token.lock().await;
            *token = Some(match check_form {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => bail!("Unexpected checkForm type"),
            });
        }
        Ok(())
    }

    // ========== Track operations ==========

    pub async fn get_track(&self, sng_id: &str) -> Result<GwTrack> {
        let result = self
            .gw_call("song.getData", json!({ "SNG_ID": sng_id }))
            .await?;
        let track: GwTrack = serde_json::from_value(result)?;
        Ok(track)
    }

    // ========== Playlist operations ==========

    pub async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<GwTrack>> {
        let result = self
            .gw_call(
                "playlist.getSongs",
                json!({ "PLAYLIST_ID": playlist_id, "nb": -1 }),
            )
            .await?;

        let data = result["data"]
            .as_array()
            .context("No data array in playlist response")?;

        let mut tracks = Vec::new();
        for item in data {
            if let Ok(track) = serde_json::from_value::<GwTrack>(item.clone()) {
                tracks.push(track);
            }
        }
        Ok(tracks)
    }

    pub async fn get_playlist_info(&self, playlist_id: &str) -> Result<Value> {
        self.gw_call(
            "deezer.pagePlaylist",
            json!({
                "PLAYLIST_ID": playlist_id,
                "lang": "en",
                "header": true,
                "tab": 0,
            }),
        )
        .await
    }

    // ========== User playlists ==========

    pub async fn get_user_playlists(&self, user_id: u64) -> Result<Vec<PlaylistInfo>> {
        let result = self
            .gw_call(
                "deezer.pageProfile",
                json!({
                    "USER_ID": user_id,
                    "tab": "playlists",
                    "nb": 100,
                }),
            )
            .await?;

        let data = &result["TAB"]["playlists"]["data"];
        let playlists: Vec<PlaylistInfo> = if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|p| serde_json::from_value(p.clone()).ok())
                .collect()
        } else {
            Vec::new()
        };
        Ok(playlists)
    }

    // ========== Favorites ==========

    pub async fn get_favorite_track_ids(&self) -> Result<Vec<String>> {
        let result = self
            .gw_call("song.getFavoriteIds", json!({ "nb": 100000, "start": 0 }))
            .await?;

        let data = result["data"]
            .as_array()
            .context("No data in favorites response")?;

        let ids: Vec<String> = data
            .iter()
            .filter_map(|item| {
                let sng_id = &item["SNG_ID"];
                match sng_id {
                    Value::Number(n) => Some(n.to_string()),
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                }
            })
            .collect();

        Ok(ids)
    }

    pub async fn get_tracks_by_ids(&self, ids: &[String]) -> Result<Vec<GwTrack>> {
        let sng_ids: Vec<Value> = ids
            .iter()
            .map(|id| {
                if let Ok(n) = id.parse::<i64>() {
                    Value::Number(n.into())
                } else {
                    Value::String(id.clone())
                }
            })
            .collect();

        let result = self
            .gw_call("song.getListData", json!({ "SNG_IDS": sng_ids }))
            .await?;

        let data = result["data"]
            .as_array()
            .context("No data in getListData response")?;

        let tracks: Vec<GwTrack> = data
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect();

        Ok(tracks)
    }

    // ========== Artist operations ==========

    pub async fn get_artist_discography(&self, art_id: &str) -> Result<Vec<AlbumInfo>> {
        let mut all_albums = Vec::new();
        let mut start = 0u64;
        let limit = 100u64;

        loop {
            let result = self
                .gw_call(
                    "album.getDiscography",
                    json!({
                        "ART_ID": art_id,
                        "discography_mode": "all",
                        "nb": limit,
                        "nb_songs": 0,
                        "start": start,
                    }),
                )
                .await?;

            let data = result["data"]
                .as_array()
                .context("No data in discography response")?;

            let albums: Vec<AlbumInfo> = data
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();

            let count = albums.len() as u64;
            all_albums.extend(albums);

            let total = result["total"].as_u64().unwrap_or(0);
            start += limit;
            if start >= total || count == 0 {
                break;
            }
        }

        Ok(all_albums)
    }

    pub async fn get_album_tracks(&self, alb_id: &str) -> Result<Vec<GwTrack>> {
        let result = self
            .gw_call("song.getListByAlbum", json!({ "ALB_ID": alb_id, "nb": -1 }))
            .await?;

        let data = result["data"]
            .as_array()
            .context("No data in album tracks response")?;

        let tracks: Vec<GwTrack> = data
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect();

        Ok(tracks)
    }

    pub async fn search_artist(&self, query: &str) -> Result<Value> {
        let result = self
            .client
            .get(format!("{}/search/artist", PUBLIC_API_URL))
            .query(&[("q", query), ("limit", "20")])
            .send()
            .await?
            .json()
            .await?;
        Ok(result)
    }

    pub async fn get_artist_info(&self, art_id: &str) -> Result<Value> {
        self.gw_call("artist.getData", json!({ "ART_ID": art_id }))
            .await
    }

    // ========== Track URL ==========

    pub async fn get_track_url(&self, track_token: &str, format: &str) -> Result<Option<String>> {
        let user = self.current_user.lock().await;
        let user = user.as_ref().context("Not logged in")?;

        let response = self
            .client
            .post(MEDIA_URL)
            .json(&json!({
                "license_token": user.license_token,
                "media": [{
                    "type": "FULL",
                    "formats": [{ "cipher": "BF_CBC_STRIPE", "format": format }]
                }],
                "track_tokens": [track_token],
            }))
            .send()
            .await?;

        let body: Value = response.json().await?;

        if let Some(data) = body["data"].as_array() {
            for item in data {
                if item.get("errors").is_some() {
                    continue;
                }
                if let Some(media) = item["media"].as_array()
                    && let Some(first) = media.first()
                    && let Some(sources) = first["sources"].as_array()
                    && let Some(source) = sources.first()
                    && let Some(url) = source["url"].as_str()
                {
                    return Ok(Some(url.to_string()));
                }
            }
        }

        Ok(None)
    }
}
