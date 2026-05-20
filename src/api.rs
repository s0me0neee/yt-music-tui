use anyhow::Result;
use serde_json::Value;
use thiserror::Error;
use ytmusicapi::YTMusic;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("YouTube Music session expired or invalid — re-run setup to re-authenticate")]
    SessionExpired,
}

pub fn get_playlists(yt: &YTMusic) -> Result<Vec<Value>> {
    log::debug!("get_playlists: calling get_library_playlists");
    let val = yt.get_library_playlists(None)?;
    log::debug!("get_playlists: raw result type={} value={:.500}", val_type(&val), val);

    let list = val.as_array().cloned().ok_or_else(|| {
        let code   = val["error"]["code"].as_i64().unwrap_or(0);
        let status = val["error"]["status"].as_str().unwrap_or("");
        log::warn!("get_playlists: non-array response (code={code} status={status}): {:.300}", val);
        anyhow::Error::new(ApiError::SessionExpired)
    })?;

    log::info!("get_playlists: got {} playlists", list.len());
    if let Some(first) = list.first() {
        log::debug!("get_playlists: first item keys={:?}",
            first.as_object().map(|m| m.keys().collect::<Vec<_>>()));
    }
    Ok(list)
}

fn val_type(v: &Value) -> &'static str {
    match v {
        Value::Null   => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_)  => "array",
        Value::Object(_) => "object",
    }
}


pub fn get_songs(yt: &YTMusic, playlist_id: &str) -> Result<Vec<Value>> {
    log::debug!("get_songs: fetching playlist_id={playlist_id}");
    let val = yt.get_playlist(playlist_id, Some(5000), None, None)?;
    let tracks = val["tracks"].as_array().cloned().unwrap_or_default();
    log::debug!("get_songs: done, total={}", tracks.len());
    Ok(tracks)
}
