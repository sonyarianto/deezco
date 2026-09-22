use crate::models::GwTrack;
use serde_json::Value;
use std::path::Path;

/// Sanitize a filename by removing/replacing invalid characters
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn normalized_artist_name(name: &str) -> String {
    name.trim().to_lowercase()
}

pub fn track_belongs_to_artist(track: &GwTrack, artist_id: &str, artist_name: &str) -> bool {
    let expected_name = normalized_artist_name(artist_name);

    if track.art_id.as_ref().and_then(value_as_string).as_deref() == Some(artist_id) {
        return true;
    }

    if normalized_artist_name(&track.artist()) == expected_name {
        return true;
    }

    false
}

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_lowercase().as_str(),
                "flac" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav"
            )
        })
        .unwrap_or(false)
}

/// Audio formats the streamer supports: mp3 + flac only.
/// Everything is decoded to the PCM bus and re-encoded to session CBR
/// (or passed through natively for mp3), so the pipeline stays uniform.
pub fn is_music_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| matches!(extension.to_lowercase().as_str(), "mp3" | "flac"))
        .unwrap_or(false)
}
