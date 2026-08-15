#![allow(dead_code)]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserData {
    #[serde(rename = "USER")]
    pub user: UserInfo,
    #[serde(rename = "checkForm")]
    pub check_form: Option<String>,
    #[serde(rename = "checkFormLogin")]
    pub check_form_login: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    #[serde(rename = "USER_ID")]
    pub user_id: serde_json::Value,
    #[serde(rename = "BLOG_NAME")]
    pub blog_name: Option<String>,
    #[serde(rename = "USER_PICTURE")]
    pub user_picture: Option<String>,
    #[serde(rename = "OPTIONS")]
    pub options: Option<UserOptions>,
    #[serde(rename = "SETTING")]
    pub setting: Option<serde_json::Value>,
    #[serde(rename = "LOVEDTRACKS_ID")]
    pub loved_tracks_id: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOptions {
    pub license_token: Option<String>,
    pub web_hq: Option<bool>,
    pub mobile_hq: Option<bool>,
    pub web_lossless: Option<bool>,
    pub mobile_lossless: Option<bool>,
    pub license_country: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: u64,
    pub name: String,
    pub license_token: String,
    pub can_stream_hq: bool,
    pub can_stream_lossless: bool,
    pub country: String,
    pub loved_tracks_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GwResponse {
    pub error: serde_json::Value,
    pub results: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GwTrack {
    #[serde(rename = "SNG_ID")]
    pub sng_id: serde_json::Value,
    #[serde(rename = "SNG_TITLE")]
    pub sng_title: Option<String>,
    #[serde(rename = "DURATION")]
    pub duration: Option<serde_json::Value>,
    #[serde(rename = "MD5_ORIGIN")]
    pub md5_origin: Option<String>,
    #[serde(rename = "MEDIA_VERSION")]
    pub media_version: Option<serde_json::Value>,
    #[serde(rename = "ART_NAME")]
    pub art_name: Option<String>,
    #[serde(rename = "ART_ID")]
    pub art_id: Option<serde_json::Value>,
    #[serde(rename = "ALB_TITLE")]
    pub alb_title: Option<String>,
    #[serde(rename = "ALB_PICTURE")]
    pub alb_picture: Option<String>,
    #[serde(rename = "ALB_ID")]
    pub alb_id: Option<serde_json::Value>,
    #[serde(rename = "TRACK_NUMBER")]
    pub track_number: Option<serde_json::Value>,
    #[serde(rename = "DISK_NUMBER")]
    pub disk_number: Option<serde_json::Value>,
    #[serde(rename = "TRACK_TOKEN")]
    pub track_token: Option<String>,
    #[serde(rename = "TRACK_TOKEN_EXPIRE")]
    pub track_token_expire: Option<serde_json::Value>,
    #[serde(rename = "ISRC")]
    pub isrc: Option<String>,
    #[serde(rename = "FILESIZE_MP3_128")]
    pub filesize_mp3_128: Option<serde_json::Value>,
    #[serde(rename = "FILESIZE_MP3_320")]
    pub filesize_mp3_320: Option<serde_json::Value>,
    #[serde(rename = "FILESIZE_FLAC")]
    pub filesize_flac: Option<serde_json::Value>,
    #[serde(rename = "FILESIZE_MP3_MISC")]
    pub filesize_mp3_misc: Option<serde_json::Value>,
    #[serde(rename = "EXPLICIT_LYRICS")]
    pub explicit_lyrics: Option<serde_json::Value>,
    #[serde(rename = "GAIN")]
    pub gain: Option<serde_json::Value>,
    #[serde(rename = "ARTISTS")]
    pub artists: Option<Vec<serde_json::Value>>,
    #[serde(rename = "LYRICS")]
    pub lyrics: Option<serde_json::Value>,
    #[serde(rename = "FALLBACK")]
    pub fallback: Option<serde_json::Value>,
    #[serde(rename = "VERSION")]
    pub version: Option<String>,
    #[serde(rename = "POSITION")]
    pub position: Option<serde_json::Value>,
}

impl GwTrack {
    pub fn id_str(&self) -> String {
        match &self.sng_id {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => "0".to_string(),
        }
    }

    pub fn title(&self) -> String {
        self.sng_title.clone().unwrap_or_default()
    }

    pub fn artist(&self) -> String {
        self.art_name
            .clone()
            .unwrap_or_else(|| "Unknown".to_string())
    }

    pub fn album(&self) -> String {
        self.alb_title.clone().unwrap_or_default()
    }

    pub fn md5(&self) -> String {
        self.md5_origin.clone().unwrap_or_default()
    }

    pub fn media_ver(&self) -> String {
        match &self.media_version {
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => "1".to_string(),
        }
    }

    pub fn display_name(&self) -> String {
        format!("{} - {}", self.artist(), self.title())
    }

    pub fn filesize_for_format(&self, format: TrackFormat) -> u64 {
        let val = match format {
            TrackFormat::Flac => &self.filesize_flac,
            TrackFormat::Mp3_320 => &self.filesize_mp3_320,
            TrackFormat::Mp3_128 => &self.filesize_mp3_128,
        };
        match val {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistInfo {
    #[serde(rename = "PLAYLIST_ID")]
    pub playlist_id: Option<serde_json::Value>,
    #[serde(rename = "TITLE")]
    pub title: Option<String>,
    #[serde(rename = "NB_SONG")]
    pub nb_song: Option<serde_json::Value>,
    #[serde(rename = "PARENT_USERNAME")]
    pub parent_username: Option<String>,
    #[serde(rename = "PLAYLIST_PICTURE")]
    pub playlist_picture: Option<String>,
}

impl PlaylistInfo {
    pub fn id_str(&self) -> String {
        match &self.playlist_id {
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => "0".to_string(),
        }
    }

    pub fn display_name(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| "Unknown Playlist".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumInfo {
    #[serde(rename = "ALB_ID")]
    pub alb_id: Option<serde_json::Value>,
    #[serde(rename = "ALB_TITLE")]
    pub alb_title: Option<String>,
    #[serde(rename = "ART_NAME")]
    pub art_name: Option<String>,
    #[serde(rename = "NB_TRACKS")]
    pub nb_tracks: Option<serde_json::Value>,
    #[serde(rename = "ARTISTS_ALBUMS_IS_OFFICIAL")]
    pub is_official: Option<bool>,
    #[serde(rename = "TYPE")]
    pub album_type: Option<serde_json::Value>,
}

impl AlbumInfo {
    pub fn id_str(&self) -> String {
        match &self.alb_id {
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => "0".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaResponse {
    pub data: Vec<MediaData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaData {
    pub media: Option<Vec<MediaInfo>>,
    pub errors: Option<Vec<MediaError>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub sources: Vec<MediaSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSource {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaError {
    pub code: i64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackFormat {
    Flac,
    Mp3_320,
    Mp3_128,
}

impl TrackFormat {
    pub fn code(&self) -> u32 {
        match self {
            TrackFormat::Flac => 9,
            TrackFormat::Mp3_320 => 3,
            TrackFormat::Mp3_128 => 1,
        }
    }

    pub fn api_name(&self) -> &'static str {
        match self {
            TrackFormat::Flac => "FLAC",
            TrackFormat::Mp3_320 => "MP3_320",
            TrackFormat::Mp3_128 => "MP3_128",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            TrackFormat::Flac => ".flac",
            TrackFormat::Mp3_320 | TrackFormat::Mp3_128 => ".mp3",
        }
    }

    pub fn fallback(&self) -> Option<TrackFormat> {
        match self {
            TrackFormat::Flac => Some(TrackFormat::Mp3_320),
            TrackFormat::Mp3_320 => Some(TrackFormat::Mp3_128),
            TrackFormat::Mp3_128 => None,
        }
    }
}

impl std::fmt::Display for TrackFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.api_name())
    }
}
