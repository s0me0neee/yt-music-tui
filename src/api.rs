use anyhow::Result;
use serde_json::Value;
use ytmusicapi::YTMusic;

pub fn get_playlists(yt: &YTMusic) -> Result<Vec<Value>> {
    let val = yt.get_library_playlists(None)?;
    Ok(val.as_array().cloned().unwrap_or_default())
}

pub fn get_liked_songs(yt: &YTMusic) -> Result<Vec<Value>> {
    let val = yt.get_liked_songs(Some(5000))?;
    Ok(val["tracks"].as_array().cloned().unwrap_or_default())
}

pub fn get_songs(yt: &YTMusic, playlist_id: &str) -> Result<Vec<Value>> {
    log::debug!("get_songs: fetching playlist_id={playlist_id}");
    let val = yt.get_playlist(playlist_id, Some(5000), None, None)?;
    let tracks = val["tracks"].as_array().cloned().unwrap_or_default();
    log::debug!("get_songs: done, total={}", tracks.len());
    Ok(tracks)
}
