//! The operating system's own media controls — the media keys on the keyboard,
//! and the panel the desktop shows for whatever is playing.
//!
//! Every desktop has this feature and no two of them spell it the same way, so
//! the vocabulary lives here and the wire protocol lives in a backend:
//!
//! | Target | Backend | What it talks to |
//! |---|---|---|
//! | Linux | `mpris` | MPRIS2 over the session D-Bus — GNOME, KDE, `playerctl` |
//! | Windows | `smtc` | `SystemMediaTransportControls` — the volume flyout, the lock screen |
//! | macOS | `nowplaying` | `MPNowPlayingInfoCenter` / `MPRemoteCommandCenter` — Control Centre |
//! | anything else | `stub` | nothing, and says so once in the log |
//!
//! One handle with three methods is all any of them expose — `new`, `update`,
//! `try_recv` — so `tui/src/app.rs` carries no `cfg` of its own and gains no
//! branch per platform. The caller builds a [`NowPlaying`] once a tick and
//! drains [`MediaCmd`]s from the same place it drains everything else; what a
//! backend cannot express (there is no volume on Windows, no quit anywhere but
//! MPRIS) simply never arrives, which costs the caller nothing.

use crate::player::PlayMode;

// The backend is chosen by target rather than by `cfg` inside one file, so each
// one reads as an ordinary module and none of them can see the others' names.
// They share the module name so nothing downstream has to know which one it
// got — including the test filter, which is why `cargo test media -- --ignored`
// runs whichever protocol's live round-trip this machine can actually do.
#[cfg_attr(target_os = "linux", path = "mpris.rs")]
#[cfg_attr(target_os = "windows", path = "smtc.rs")]
#[cfg_attr(target_os = "macos", path = "nowplaying.rs")]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "windows", target_os = "macos")),
    path = "stub.rs"
)]
mod backend;

pub use backend::MediaControls;

// ── what the desktop asks of us ──────────────────────────────────────────────

/// A command from the desktop — a media key, a click in GNOME's player widget,
/// a `playerctl` invocation, a button in the Windows flyout, a squeeze of an
/// AirPods stem. Every variant maps onto a method the UI already calls from its
/// own key handlers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaCmd {
    /// Resume, or start a queue restored from disk.
    Play,
    Pause,
    PlayPause,
    /// Stop, keeping the queue — see [`crate::player::Player::stop`].
    Stop,
    Next,
    Previous,
    /// Relative seek, in seconds.
    Seek(f64),
    /// Absolute seek, in seconds.
    SeekTo(f64),
    /// 0-100, converted from MPRIS's 0.0-1.0. MPRIS only: neither SMTC nor the
    /// Now Playing centre has a volume, because both operating systems put it
    /// in the system mixer instead, against the audio session mpv opens.
    Volume(u8),
    /// Loop status and shuffle collapsed back into the one tri-state the
    /// player actually has.
    Mode(PlayMode),
    /// MPRIS only — the spec gives the desktop a `Quit` method, and neither of
    /// the others has anything like it.
    Quit,
}

// ── what we tell the desktop ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// The playing track, as the panels want it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackInfo {
    /// YouTube video id — becomes `mpris:trackid` and `xesam:url`.
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    /// Seconds; 0 while unknown, which is most of a track's first second.
    pub length: f64,
    /// Cover art, as a URL — the same thumbnail the playlist fetch already
    /// carried, asked for at a size worth showing (see [`crate::cover::at_size`]).
    /// Empty when there is none. Every backend wants it in a different form:
    /// MPRIS and SMTC hand the URL straight to the desktop and let *it* fetch,
    /// while macOS needs an `NSImage` and so has to fetch the bytes itself.
    pub art_url: String,
}

/// Everything the interface reports, built by the caller once per tick.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlaying {
    pub state: PlayState,
    pub track: Option<TrackInfo>,
    pub mode: PlayMode,
    /// 0-100, as [`crate::player::Player`] holds it.
    pub volume: u8,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_play: bool,
    pub can_seek: bool,
    /// Live playback position, in seconds. Deliberately left out of the MPRIS
    /// property diff: the spec keeps `Position` out of `PropertiesChanged`
    /// (clients poll it, and a jump is announced with `Seeked` instead), and
    /// `mpris_server::Property` has no variant for it. Signalling it every
    /// tick is the classic way to make a shell busy-loop. The same restraint
    /// is what keeps the other two backends off the wire between seeks.
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

/// A position change larger than this, on the same track, is a seek rather
/// than the clock advancing: the event loop ticks at most every 200 ms, so
/// ordinary playback can never move this far between two updates.
///
/// Shared, because every backend needs the same answer to the same question —
/// MPRIS to decide whether to emit `Seeked`, the other two to decide whether a
/// timeline that is otherwise only refreshed every few seconds has gone stale.
pub(crate) const SEEK_JUMP_SECS: f64 = 1.0;

/// Whether `now` has moved somewhere the clock alone cannot explain since
/// `previous_position` on `previous_track`. False across a track change, which
/// is a new timeline rather than a jump within one.
pub(crate) fn is_seek(
    now: &NowPlaying,
    previous_track: Option<&str>,
    previous_position: f64,
) -> bool {
    now.track.as_ref().map(|t| t.id.as_str()) == previous_track
        && now.state != PlayState::Stopped
        && (now.position - previous_position).abs() > SEEK_JUMP_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(id: &str, position: f64) -> NowPlaying {
        NowPlaying {
            state: PlayState::Playing,
            track: Some(TrackInfo {
                id: id.into(),
                ..TrackInfo::default()
            }),
            position,
            ..NowPlaying::default()
        }
    }

    #[test]
    fn the_clock_advancing_is_not_a_seek() {
        // A tick is 200 ms at the slowest, so this is what one looks like.
        assert!(!is_seek(&playing("a", 10.2), Some("a"), 10.0));
    }

    #[test]
    fn an_arrow_key_is_a_seek() {
        assert!(is_seek(&playing("a", 15.0), Some("a"), 10.0));
        // Backwards too.
        assert!(is_seek(&playing("a", 5.0), Some("a"), 10.0));
    }

    #[test]
    fn a_new_track_is_not_a_seek() {
        // Position falls to zero on every track change, which is a jump by any
        // arithmetic and a new timeline by every protocol.
        assert!(!is_seek(&playing("b", 0.0), Some("a"), 180.0));
    }

    #[test]
    fn a_stopped_player_never_seeks() {
        let stopped = NowPlaying {
            state: PlayState::Stopped,
            ..playing("a", 90.0)
        };
        assert!(!is_seek(&stopped, Some("a"), 0.0));
    }
}
