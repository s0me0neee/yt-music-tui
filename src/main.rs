mod api;
mod app;
mod setup;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::path::Path;
use ytmusicapi::YTMusic;

fn main() -> anyhow::Result<()> {
    WriteLogger::init(LevelFilter::Debug, Config::default(), File::create("app.log")?)?;

    if !Path::new("browser.json").exists() {
        setup::run_setup("browser.json")?;
        eprintln!("\nSetup complete. Run `cargo run` to start the TUI.");
        return Ok(());
    }

    let yt = YTMusic::authenticated("browser.json")?;
    let yt = &yt; // &YTMusic is Copy — all spawn closures share the borrow

    let playlists = api::get_playlists(yt)?;

    let pl_ids: Vec<String> = playlists
        .iter()
        .map(|pl| pl["playlistId"].as_str().unwrap_or("").to_string())
        .collect();

    // Fetch every playlist's tracks in parallel.
    // YTMusic is Send+Sync; requests releases the GIL during socket I/O so
    // threads genuinely overlap their network waits.
    let all_songs: Vec<Vec<_>> = std::thread::scope(|s| {
        let handles: Vec<_> = pl_ids
            .iter()
            .map(|id| {
                s.spawn(move || {
                    api::get_songs(yt, id).unwrap_or_else(|e| {
                        log::error!("failed to fetch songs for {id}: {e:#}");
                        Vec::new()
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("song-fetch thread panicked"))
            .collect()
    });

    app::App::new(playlists, all_songs).run()
}
