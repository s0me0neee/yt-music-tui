mod api;
mod app;
mod audio;
mod config;
mod setup;
mod ytm;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use ytmusicapi::{BrowserAuth, YTMusicClient};

/// Set to true by SIGINT / SIGTERM / SIGHUP handlers. The event loop polls
/// this and breaks cleanly, which runs all Drop impls and kills mpv.
pub static QUIT: AtomicBool = AtomicBool::new(false);

fn build_client(browser_json: &str) -> anyhow::Result<YTMusicClient> {
    let auth = BrowserAuth::from_file(browser_json)?;
    Ok(YTMusicClient::builder().with_browser_auth(auth).build()?)
}

fn reauth(browser_json: &str) -> anyhow::Result<()> {
    std::fs::remove_file(browser_json).ok();
    std::fs::remove_file(config::browser_file_path()).ok();
    setup::run_setup(browser_json)?;
    eprintln!("\nSetup complete. Restart the app to continue.\n");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Ensure config dir exists before anything else touches it.
    let config_dir = config::ensure_config_dir()?;

    WriteLogger::init(
        LevelFilter::Debug,
        Config::default(),
        File::create(config_dir.join("app.log"))?,
    )?;
    log::info!("Start up — config dir: {}", config_dir.display());

    config::ensure_config_toml()?;

    ctrlc::set_handler(|| {
        QUIT.store(true, Ordering::Relaxed);
    })?;

    let browser_json = config::browser_json_path();
    let browser_json_str = browser_json.to_string_lossy().into_owned();

    if !browser_json.exists() {
        setup::run_setup(&browser_json_str)?;
        eprintln!("\nSetup complete. Run again to start the TUI.");
        return Ok(());
    }

    // Build the API client immediately with cached cookies, then kick off a
    // background refresh so yt-dlp's 2-5 s run doesn't block startup.
    let yt = match build_client(&browser_json_str) {
        Ok(c) => c,
        Err(e) => {
            log::error!("build_client failed: {e:#}");
            eprintln!("\nFailed to load session: {e}");
            reauth(&browser_json_str)?;
            return Ok(());
        }
    };

    let cookie_refresh = {
        let path = browser_json_str.clone();
        std::thread::spawn(move || {
            if let Err(e) = setup::refresh_cookies(&path) {
                log::warn!("cookie refresh failed (using cached): {e}");
            }
        })
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let playlists = match rt.block_on(api::get_playlists(&yt)) {
        Ok(p) => p,
        Err(e)
            if e.downcast_ref::<api::ApiError>()
                .map_or(false, |ae| matches!(ae, api::ApiError::SessionExpired)) =>
        {
            eprintln!("\nSession expired — re-authenticating.");
            reauth(&browser_json_str)?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let pl_ids: Vec<String> = playlists
        .iter()
        .map(|pl| pl["playlistId"].as_str().unwrap_or("").to_string())
        .collect();

    let yt = Arc::new(yt);

    // Spawn per-playlist song fetches in the background so the TUI starts
    // immediately rather than waiting for all network calls to complete.
    let (songs_tx, songs_rx) = std::sync::mpsc::channel::<(usize, Vec<serde_json::Value>)>();
    for (idx, id) in pl_ids.into_iter().enumerate() {
        let yt  = Arc::clone(&yt);
        let tx  = songs_tx.clone();
        rt.spawn(async move {
            let songs = api::get_songs(&yt, &id).await;
            let _ = tx.send((idx, songs));
        });
    }
    drop(songs_tx); // close sender side so the channel ends when all tasks finish

    let saved_queue = config::load_queue();
    let all_songs   = vec![vec![]; playlists.len()];

    let result = app::App::new(playlists, all_songs, saved_queue, songs_rx).run();

    // Wait for cookie refresh before exiting so browser.json is never partial.
    let _ = cookie_refresh.join();

    result
}
