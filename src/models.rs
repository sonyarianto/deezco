use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: u64,
    pub name: String,
    pub license_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowedArtist {
    pub id: u64,
    pub name: String,
    /// Number of releases, as reported by the public API
    #[serde(default)]
    pub nb_album: u64,
    /// Best quality available across the artist's releases, when known
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_quality: Option<String>,
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
    #[serde(rename = "MD5_ORIGIN")]
    pub md5_origin: Option<String>,
    #[serde(rename = "MEDIA_VERSION")]
    pub media_version: Option<serde_json::Value>,
    #[serde(rename = "ART_NAME")]
    pub art_name: Option<String>,
    #[serde(rename = "ART_ID")]
    pub art_id: Option<serde_json::Value>,
    #[serde(rename = "TRACK_TOKEN")]
    pub track_token: Option<String>,
    #[serde(rename = "FILESIZE_MP3_128")]
    pub filesize_mp3_128: Option<serde_json::Value>,
    #[serde(rename = "FILESIZE_MP3_320")]
    pub filesize_mp3_320: Option<serde_json::Value>,
    #[serde(rename = "FILESIZE_FLAC")]
    pub filesize_flac: Option<serde_json::Value>,
    #[serde(rename = "DURATION")]
    pub duration: Option<serde_json::Value>,
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

    /// Track length in seconds (0 when unknown)
    pub fn duration_secs(&self) -> u64 {
        match &self.duration {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        }
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
pub struct AlbumInfo {
    #[serde(rename = "ALB_ID")]
    pub alb_id: Option<serde_json::Value>,
    #[serde(rename = "ALB_TITLE")]
    pub alb_title: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackFormat {
    Flac,
    Mp3_320,
    Mp3_128,
}

impl TrackFormat {
    /// Quality rank used for comparisons: FLAC=3, MP3_320=2, MP3_128=1
    pub fn rank(self) -> u8 {
        match self {
            TrackFormat::Flac => 3,
            TrackFormat::Mp3_320 => 2,
            TrackFormat::Mp3_128 => 1,
        }
    }

    /// Cap this format to at most `cap` quality (e.g. FLAC capped by 320 → MP3_320)
    pub fn capped_by(self, cap: TrackFormat) -> TrackFormat {
        if self.rank() > cap.rank() {
            cap
        } else {
            self
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn followed_artist_parses_full_payload() {
        let artist: FollowedArtist = serde_json::from_value(serde_json::json!({
            "id": 2529,
            "name": "Daft Punk",
            "nb_album": 4,
        }))
        .unwrap();

        assert_eq!(artist.id, 2529);
        assert_eq!(artist.name, "Daft Punk");
        assert_eq!(artist.nb_album, 4);
    }

    #[test]
    fn followed_artist_defaults_missing_album_count() {
        let artist: FollowedArtist = serde_json::from_value(serde_json::json!({
            "id": 42,
            "name": "Some Artist",
        }))
        .unwrap();

        assert_eq!(artist.id, 42);
        assert_eq!(artist.name, "Some Artist");
        assert_eq!(artist.nb_album, 0);
    }

    #[test]
    fn followed_artist_ignores_unknown_fields() {
        let artist: FollowedArtist = serde_json::from_value(serde_json::json!({
            "id": 7,
            "name": "Artist",
            "nb_album": 2,
            "picture": "https://e-cdn-images.dzcdn.net/images/artist/x/500x500.jpg",
            "link": "https://www.deezer.com/artist/7",
        }))
        .unwrap();

        assert_eq!(artist.id, 7);
        assert_eq!(artist.name, "Artist");
        assert_eq!(artist.nb_album, 2);
    }

    #[test]
    fn track_format_capped_by_never_exceeds_the_cap() {
        assert_eq!(TrackFormat::Flac.capped_by(TrackFormat::Mp3_320), TrackFormat::Mp3_320);
        assert_eq!(TrackFormat::Flac.capped_by(TrackFormat::Mp3_128), TrackFormat::Mp3_128);
        assert_eq!(TrackFormat::Mp3_320.capped_by(TrackFormat::Flac), TrackFormat::Mp3_320);
        assert_eq!(TrackFormat::Mp3_320.capped_by(TrackFormat::Mp3_320), TrackFormat::Mp3_320);
        assert_eq!(TrackFormat::Mp3_128.capped_by(TrackFormat::Flac), TrackFormat::Mp3_128);
    }

    #[test]
    fn track_format_rank_orders_qualities() {
        assert!(TrackFormat::Flac.rank() > TrackFormat::Mp3_320.rank());
        assert!(TrackFormat::Mp3_320.rank() > TrackFormat::Mp3_128.rank());
        assert_eq!(TrackFormat::Flac.rank(), 3);
        assert_eq!(TrackFormat::Mp3_320.rank(), 2);
        assert_eq!(TrackFormat::Mp3_128.rank(), 1);
    }

    #[test]
    fn followed_artist_serializes_with_album_count() {
        let artist = FollowedArtist {
            id: 27,
            name: "Test Artist".to_string(),
            nb_album: 3,
            best_quality: None,
        };
        let value = serde_json::to_value(&artist).unwrap();

        assert_eq!(value["id"], serde_json::json!(27));
        assert_eq!(value["name"], serde_json::json!("Test Artist"));
        assert_eq!(value["nb_album"], serde_json::json!(3));
        // best_quality is omitted when unknown
        assert!(value.get("best_quality").is_none());
    }

    #[test]
    fn followed_artist_serializes_best_quality_when_known() {
        let artist = FollowedArtist {
            id: 27,
            name: "Test Artist".to_string(),
            nb_album: 3,
            best_quality: Some("FLAC".to_string()),
        };
        let value = serde_json::to_value(&artist).unwrap();

        assert_eq!(value["best_quality"], serde_json::json!("FLAC"));
    }
}
