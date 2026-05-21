mod api;
mod app;
mod audio;
mod setup;
mod ytm;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::path::Path;
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
    std::fs::remove_file(setup::BROWSER_FILE).ok();
    setup::run_setup(browser_json)?;
    eprintln!("\nSetup complete. Restart the app to continue.\n");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    WriteLogger::init(
        LevelFilter::Debug,
        Config::default(),
        File::create("app.log")?,
    )?;
    log::info!("Start up");

    ctrlc::set_handler(|| {
        QUIT.store(true, Ordering::Relaxed);
    })?;

    let browser_json = "browser.json";

    if !Path::new(browser_json).exists() {
        setup::run_setup(browser_json)?;
        eprintln!("\nSetup complete. Run again to start the TUI.");
        return Ok(());
    }

    // Build the API client immediately with cached cookies, then kick off a
    // background refresh so yt-dlp's 2-5 s run doesn't block startup.
    // The refresh writes fresh cookies for the *next* session; if today's
    // cached cookies are still valid (they usually are) we proceed right away.
    let yt = match build_client(browser_json) {
        Ok(c) => c,
        Err(e) => {
            log::error!("build_client failed: {e:#}");
            eprintln!("\nFailed to load session: {e}");
            reauth(browser_json)?;
            return Ok(());
        }
    };

    let cookie_refresh = {
        let path = browser_json.to_owned();
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
            reauth(browser_json)?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let pl_ids: Vec<String> = playlists
        .iter()
        .map(|pl| pl["playlistId"].as_str().unwrap_or("").to_string())
        .collect();

    let yt = Arc::new(yt);

    let all_songs: Vec<Vec<_>> = rt.block_on(async {
        let handles: Vec<_> = pl_ids
            .iter()
            .map(|id| {
                let yt = Arc::clone(&yt);
                let id = id.clone();
                tokio::spawn(async move { api::get_songs(&yt, &id).await })
            })
            .collect();

        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.unwrap_or_default());
        }
        out
    });

    let result = app::App::new(playlists, all_songs).run();

    // Wait for the cookie refresh to finish writing before the process exits,
    // so browser.json is never left in a partial state.
    let _ = cookie_refresh.join();

    result
}
