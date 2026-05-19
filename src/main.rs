mod api;
mod app;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::path::Path;
use ytmusicapi::YTMusic;

fn main() -> anyhow::Result<()> {
    WriteLogger::init(LevelFilter::Debug, Config::default(), File::create("app.log")?)?;

    let yt = if Path::new("browser.json").exists() {
        YTMusic::authenticated("browser.json")?
    } else {
        log::info!("browser.json not found, running setup");
        YTMusic::setup(Some("browser.json"))?;
        YTMusic::authenticated("browser.json")?
    };

    let mut playlists = api::get_playlists(&yt)?;

    let liked_songs = api::get_liked_songs(&yt).unwrap_or_else(|e| {
        log::warn!("failed to fetch liked songs: {e:#}");
        Vec::new()
    });
    playlists.insert(0, serde_json::json!({"title": "Liked Songs", "playlistId": "LM"}));

    let mut all_songs = vec![liked_songs];
    for pl in &playlists[1..] {
        let id = pl["playlistId"].as_str().unwrap_or("");
        let songs = api::get_songs(&yt, id).unwrap_or_else(|e| {
            log::error!("failed to fetch songs for {id}: {e:#}");
            Vec::new()
        });
        all_songs.push(songs);
    }

    app::App::new(playlists, all_songs).run()
}
