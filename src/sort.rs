use crate::api::DeezerApi;
use crate::cli::{parse_format, SortDir, SortKey};
use crate::models::{FollowedArtist, GwTrack};
use crate::resolve::{direction, quality_rank};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

/// Result indices sorted by available quality (stable, so relevance order is
/// kept within equal quality; results without a track ID or missing from
/// `by_id` sort last). Descending = best quality first.
pub fn sort_by_quality(
    data: &[Value],
    by_id: &HashMap<String, GwTrack>,
    desc: bool,
) -> Vec<usize> {
    let rank = |i: usize| {
        data[i]["id"]
            .as_u64()
            .and_then(|id| by_id.get(&id.to_string()))
            .map(quality_rank)
            .unwrap_or(0)
    };
    let mut indices: Vec<usize> = (0..data.len()).collect();
    indices.sort_by(|&a, &b| {
        let (ra, rb) = (rank(a), rank(b));
        if desc { rb.cmp(&ra) } else { ra.cmp(&rb) }
    });
    indices
}

/// Reorder `results["data"]` by the given indices (stable, so the source
/// order is kept for equal sort keys).
pub fn reorder_results_data(results: &mut Value, indices: Vec<usize>) {
    if let Some(data) = results["data"].as_array_mut() {
        let sorted: Vec<Value> = indices.into_iter().map(|i| data[i].clone()).collect();
        *data = sorted;
    }
}

/// Order two durations (None = unknown, which sorts last in either direction).
fn duration_ordering(da: Option<u64>, db: Option<u64>, desc: bool) -> std::cmp::Ordering {
    match (da, db) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => {
            if desc {
                y.cmp(&x)
            } else {
                x.cmp(&y)
            }
        }
    }
}

/// Indices sorted by the search response's `duration` field. Tracks without a
/// duration sort last regardless of direction.
pub fn duration_sorted_indices(data: &[Value], desc: bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..data.len()).collect();
    indices.sort_by(|&a, &b| {
        duration_ordering(
            data[a]["duration"].as_u64(),
            data[b]["duration"].as_u64(),
            desc,
        )
    });
    indices
}

/// Format seconds as `m:ss` (or `h:mm:ss` for an hour or more)
pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Fetch full track data (with filesizes) for the search results in one
/// batched call, then return the quality-sorted indices and the track map.
pub async fn quality_sorted_indices(
    api: &DeezerApi,
    data: &[Value],
    desc: bool,
) -> Result<(Vec<usize>, HashMap<String, GwTrack>)> {
    let ids: Vec<String> = data
        .iter()
        .filter_map(|track| track["id"].as_u64())
        .map(|id| id.to_string())
        .collect();

    let mut by_id: HashMap<String, GwTrack> = HashMap::new();
    if !ids.is_empty() {
        for track in api.get_tracks_by_ids(&ids).await? {
            by_id.insert(track.id_str(), track);
        }
    }

    Ok((sort_by_quality(data, &by_id, desc), by_id))
}

/// Sort followed artists by best available quality (stable; the followed-list
/// order is kept within equal quality, unknown quality sorts last).
/// Descending = best quality first.
pub fn sort_followed_by_quality(artists: &mut [FollowedArtist], desc: bool) {
    let rank = |artist: &FollowedArtist| {
        artist
            .best_quality
            .as_deref()
            .map_or(0, |quality| parse_format(quality).rank())
    };
    artists.sort_by(|a, b| {
        let (ra, rb) = (rank(a), rank(b));
        if desc { rb.cmp(&ra) } else { ra.cmp(&rb) }
    });
}

/// Sort tracks for favorites JSON by the selected key and direction. The
/// artist/title/id tiebreakers are deterministic so pagination pages stay
/// stable across calls.
pub fn sort_favorites(tracks: &mut [GwTrack], key: SortKey, sort_dir: Option<SortDir>) {
    let quality_desc = direction(sort_dir, true);
    let duration_desc = direction(sort_dir, false);
    let tiebreakers = |a: &GwTrack, b: &GwTrack| {
        a.artist()
            .to_lowercase()
            .cmp(&b.artist().to_lowercase())
            .then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase()))
            .then_with(|| a.id_str().cmp(&b.id_str()))
    };
    match key {
        SortKey::Relevance => {} // raw liked order
        SortKey::Duration => {
            let known = |t: &GwTrack| (t.duration_secs() != 0).then_some(t.duration_secs());
            tracks.sort_by(|a, b| {
                duration_ordering(known(a), known(b), duration_desc).then_with(|| tiebreakers(a, b))
            });
        }
        SortKey::Quality => {
            tracks.sort_by(|a, b| {
                let (ra, rb) = (quality_rank(a), quality_rank(b));
                let ord = if quality_desc {
                    rb.cmp(&ra)
                } else {
                    ra.cmp(&rb)
                };
                ord.then_with(|| tiebreakers(a, b))
            });
        }
    }
}

/// Serialize tracks with a normalized numeric `duration` (seconds) added,
/// matching the search JSON's field name, alongside the raw GW fields.
pub fn with_duration(tracks: Vec<GwTrack>) -> Vec<Value> {
    tracks
        .into_iter()
        .map(|track| {
            let duration = track.duration_secs();
            let mut value = serde_json::to_value(&track).unwrap_or_default();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("duration".to_string(), serde_json::json!(duration));
            }
            value
        })
        .collect()
}
