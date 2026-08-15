//! A stand-in for `ytm-core`, holding only what `media/nowplaying.rs` reaches
//! for — so that file can be type-checked against the real `objc2` bindings
//! from a machine with no Apple SDK. See this crate's `Cargo.toml` for why.
//!
//! Nothing here is shipped or run. If the backend starts using something else
//! out of `ytm-core`, the shape of it has to be added below, and keeping the
//! two in step is the price of being able to check the file at all.

pub mod player {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum PlayMode {
        #[default]
        Cycle,
        Single,
        Shuffle,
    }
}

pub mod cover {
    pub async fn fetch_bytes(_url: &str) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

/// The same vocabulary `ytm-core/src/media/mod.rs` defines, copied rather than
/// included: that file selects a backend by target, and this crate has to name
/// one of them itself.
pub mod media {
    use crate::player::PlayMode;

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum MediaCmd {
        Play,
        Pause,
        PlayPause,
        Stop,
        Next,
        Previous,
        Seek(f64),
        SeekTo(f64),
        Volume(u8),
        Mode(PlayMode),
        Quit,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum PlayState {
        #[default]
        Stopped,
        Playing,
        Paused,
    }

    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct TrackInfo {
        pub id: String,
        pub title: String,
        pub artists: Vec<String>,
        pub album: String,
        pub length: f64,
        pub art_url: String,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct NowPlaying {
        pub state: PlayState,
        pub track: Option<TrackInfo>,
        pub mode: PlayMode,
        pub volume: u8,
        pub can_go_next: bool,
        pub can_go_previous: bool,
        pub can_play: bool,
        pub can_seek: bool,
        pub position: f64,
    }

    impl Default for NowPlaying {
        fn default() -> Self {
            Self {
                state: PlayState::default(),
                track: None,
                mode: PlayMode::Cycle,
                volume: 0,
                can_go_next: false,
                can_go_previous: false,
                can_play: false,
                can_seek: false,
                position: 0.0,
            }
        }
    }

    pub(crate) const SEEK_JUMP_SECS: f64 = 1.0;

    pub(crate) fn is_seek(
        now: &NowPlaying,
        previous_track: Option<&str>,
        previous_position: f64,
    ) -> bool {
        now.track.as_ref().map(|t| t.id.as_str()) == previous_track
            && now.state != PlayState::Stopped
            && (now.position - previous_position).abs() > SEEK_JUMP_SECS
    }

    #[cfg(target_os = "macos")]
    // Relative to `src/media/`, since that is where a `mod media` in `lib.rs`
    // would look for its children.
    #[path = "../../../../ytm-core/src/media/nowplaying.rs"]
    mod backend;

    #[cfg(target_os = "macos")]
    pub use backend::MediaControls;
}
