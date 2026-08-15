//! No OS media controls here.
//!
//! Reached on the targets none of the three protocols covers — the BSDs, and
//! anything else that builds. A player without media keys is a smaller thing,
//! not a broken one, so this says so once in the log and then costs nothing.

use super::{MediaCmd, NowPlaying};

#[derive(Debug)]
pub struct MediaControls(());

impl MediaControls {
    pub fn new(_rt: &tokio::runtime::Handle) -> Option<Self> {
        log::info!("[media] no OS media controls on this platform");
        None
    }

    pub fn update(&mut self, _now: &NowPlaying) {}

    pub fn try_recv(&self) -> Option<MediaCmd> {
        None
    }
}
