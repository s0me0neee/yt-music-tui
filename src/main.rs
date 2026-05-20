mod api;
mod app;
mod audio;
mod setup;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use ytmusicapi::YTMusic;

/// Set to true by SIGINT / SIGTERM / SIGHUP handlers. The event loop polls
/// this and breaks cleanly, which runs all Drop impls and kills mpv.
pub static QUIT: AtomicBool = AtomicBool::new(false);

fn reauth() -> anyhow::Result<()> {
    eprintln!();
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("  Session expired — re-authentication required");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!();
    eprintln!("  1. Open music.youtube.com in your browser (stay logged in)");
    eprintln!("  2. Open DevTools → Network tab  (F12 or Cmd+Opt+I)");
    eprintln!("  3. Reload the page, then click any request in the list");
    eprintln!("  4. Right-click → \"Copy as cURL (bash)\"");
    eprintln!("  5. Paste it here, then press Ctrl+D\n");
    std::fs::remove_file("browser.json").ok();
    setup::run_setup("browser.json")?;
    eprintln!("\nRe-authentication complete. Run `cargo run` again to start.\n");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    WriteLogger::init(LevelFilter::Debug, Config::default(), File::create("app.log")?)?;

    ctrlc::set_handler(|| {
        QUIT.store(true, Ordering::Relaxed);
    })?;

    if !Path::new("browser.json").exists() {
        setup::run_setup("browser.json")?;
        eprintln!("\nSetup complete. Run `cargo run` to start the TUI.");
        return Ok(());
    }

    let yt = match YTMusic::authenticated("browser.json") {
        Ok(yt) => yt,
        Err(e) => {
            log::error!("YTMusic::authenticated failed: {e:#}");
            eprintln!("\nFailed to load session: {e}");
            reauth()?;
            return Ok(());
        }
    };
    let yt = &yt; // &YTMusic is Send+Sync — all spawn closures share the borrow

    let playlists = match api::get_playlists(yt) {
        Ok(p) => p,
        Err(e) if e.downcast_ref::<api::ApiError>().map_or(false, |ae| matches!(ae, api::ApiError::SessionExpired)) => {
            reauth()?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

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
