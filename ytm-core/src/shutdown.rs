//! Process-wide shutdown signal.
//!
//! Set by the host application's SIGINT/SIGTERM/SIGHUP handler and polled by
//! the event loop and the audio thread so both break cleanly — running Drop
//! impls and killing mpv — instead of the audio thread hanging on in-flight
//! `yt-dlp` work.

use std::sync::atomic::{AtomicBool, Ordering};

static QUIT: AtomicBool = AtomicBool::new(false);

/// Request that all `ytm-core`-owned background work stop as soon as possible.
pub fn request_shutdown() {
    QUIT.store(true, Ordering::Relaxed);
}

/// Whether [`request_shutdown`] has been called.
pub fn is_shutdown_requested() -> bool {
    QUIT.load(Ordering::Relaxed)
}
