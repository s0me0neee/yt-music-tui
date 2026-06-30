use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use thiserror::Error;
use ytmusicapi::{Error as YtmError, YTMusicClient};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("YouTube Music session expired — re-authenticate")]
    SessionExpired,
}

#[hotpath::measure]
pub async fn get_playlists(yt: &YTMusicClient) -> Result<Vec<Value>> {
    match yt.get_library_playlists(None).await {
        Ok(list) => Ok(list
            .into_iter()
            .map(|pl| {
                json!({
                    "playlistId": pl.playlist_id,
                    "title":      pl.title,
                    "count":      pl.count,
                })
            })
            .collect()),
        Err(YtmError::AuthRequired) => Err(anyhow::Error::new(ApiError::SessionExpired)),
        Err(YtmError::Server { status: 401, .. }) => {
            Err(anyhow::Error::new(ApiError::SessionExpired))
        }
        Err(e) => Err(anyhow::anyhow!("get_library_playlists: {e}")),
    }
}

#[hotpath::measure]
pub async fn get_songs(yt: &YTMusicClient, playlist_id: &str) -> Vec<Value> {
    log::debug!("get_songs: fetching {playlist_id}");
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        match yt.get_playlist(playlist_id, Some(5000)).await {
            Ok(pl) => {
                return pl
                    .tracks
                    .into_iter()
                    .map(|t| {
                        json!({
                            "videoId":          t.video_id,
                            "title":            t.title,
                            "artists":          t.artists.iter().map(|a| json!({"name": a.name, "id": a.id})).collect::<Vec<_>>(),
                            "album":            t.album.as_ref().map(|a| json!({"name": a.name, "id": a.id})),
                            "duration":         t.duration,
                            "duration_seconds": t.duration_seconds,
                        })
                    })
                    .collect();
            }
            Err(e) => {
                log::warn!("get_songs({playlist_id}) attempt {attempt}/{ATTEMPTS}: {e:#}");
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
    log::error!("get_songs({playlist_id}): giving up after {ATTEMPTS} attempts");
    Vec::new()
}
