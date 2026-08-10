mod app;

use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::sync::Arc;

use ytm_core::{Session, library, persistence, session, shutdown};

#[hotpath::main]
fn main() -> anyhow::Result<()> {
    // Ensure config dir exists before anything else touches it.
    let config_dir = session::ensure_config_dir()?;

    WriteLogger::init(
        LevelFilter::Debug,
        Config::default(),
        File::create(config_dir.join("app.log"))?,
    )?;
    log::info!("Start up — config dir: {}", config_dir.display());

    ctrlc::set_handler(shutdown::request_shutdown)?;

    let session = Session::new()?;

    if !session.is_set_up() {
        session.run_setup()?;
        eprintln!("\nSetup complete. Run again to start the TUI.");
        return Ok(());
    }

    // Build the API client immediately with cached cookies, then kick off a
    // background refresh so yt-dlp's 2-5 s run doesn't block startup.
    let yt = match session.build_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("build_client failed: {e:#}");
            eprintln!("\nFailed to load session: {e}");
            session.reauth()?;
            return Ok(());
        }
    };

    let cookie_refresh = {
        let session = session.clone();
        std::thread::spawn(move || {
            if let Err(e) = session.refresh_cookies() {
                log::warn!("cookie refresh failed (using cached): {e}");
            }
        })
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let playlists = match rt.block_on(library::get_playlists(&yt)) {
        Ok(p) => p,
        Err(ytm_core::Error::SessionExpired) => {
            eprintln!("\nSession expired — re-authenticating.");
            session.reauth()?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let yt = Arc::new(yt);

    // Spawn per-playlist song fetches in the background so the TUI starts
    // immediately rather than waiting for all network calls to complete.
    let songs_rx = library::spawn_library_fetch(rt.handle(), Arc::clone(&yt), &playlists);

    let saved_queue = persistence::load_queue();
    let lib = library::Library::new(playlists);

    let result = app::App::new(lib, saved_queue, songs_rx).run();

    // Wait for cookie refresh before exiting so browser.json is never partial.
    let _ = cookie_refresh.join();

    result
}
