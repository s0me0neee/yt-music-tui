//! MPRIS2 — the desktop's media-player protocol, spoken over the session D-Bus.
//!
//! One feature answers both "the media keys on my keyboard work" and "the OS
//! lists this as a player": GNOME's media-keys plugin, KDE's mpris2 engine and
//! `playerctl` all grab the `XF86Audio*` keys globally and forward them to
//! whichever process owns an `org.mpris.MediaPlayer2.*` bus name. So the TUI
//! grabs no keys itself — it could not under Wayland anyway, where global grabs
//! are denied to unprivileged clients — and the terminal need not be focused.
//!
//! The shape mirrors [`crate::playback`]: an `Arc<Mutex<_>>` snapshot the D-Bus
//! tasks read, plus a channel of commands the caller drains from its own event
//! loop. [`crate::player::Player`] lives on the UI thread and is not shared, so
//! nothing here touches it directly.
//!
//! MPRIS is a freedesktop protocol, so this is Linux-only. Other targets get a
//! stub with the same three methods, and callers need no `cfg` of their own.

use crate::player::PlayMode;

// ── what the desktop asks of us ──────────────────────────────────────────────

/// A command from the desktop — a media key, a click in GNOME's player widget,
/// a `playerctl` invocation. Every variant maps onto a method the UI already
/// calls from its own key handlers.
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
    /// 0-100, converted from MPRIS's 0.0-1.0.
    Volume(u8),
    /// Loop status and shuffle collapsed back into the one tri-state the
    /// player actually has.
    Mode(PlayMode),
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

/// The playing track, as MPRIS metadata wants it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackInfo {
    /// YouTube video id — becomes `mpris:trackid` and `xesam:url`.
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    /// Seconds; 0 while unknown, which is most of a track's first second.
    pub length: f64,
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
    /// Live playback position, in seconds. Deliberately left out of the
    /// property diff: the spec keeps `Position` out of `PropertiesChanged`
    /// (clients poll it, and a jump is announced with `Seeked` instead), and
    /// `mpris_server::Property` has no variant for it. Signalling it every
    /// tick is the classic way to make a shell busy-loop.
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

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use mpris_server::zbus::{Result as ZResult, fdo};
    use mpris_server::{
        LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property,
        RootInterface, Server, Signal, Time, TrackId, Volume,
    };

    use super::{MediaCmd, NowPlaying, PlayState};
    use crate::player::PlayMode;

    /// A position change larger than this, on the same track, is a seek rather
    /// than the clock advancing: the event loop ticks at most every 200 ms, so
    /// ordinary playback can never move this far between two updates.
    const SEEK_JUMP_SECS: f64 = 1.0;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        // Recover from a poisoned lock rather than panicking a D-Bus task.
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    impl From<PlayState> for PlaybackStatus {
        fn from(s: PlayState) -> Self {
            match s {
                PlayState::Playing => Self::Playing,
                PlayState::Paused => Self::Paused,
                PlayState::Stopped => Self::Stopped,
            }
        }
    }

    /// MPRIS splits what [`PlayMode`] fuses: looping is a tri-state and shuffle
    /// is a separate flag. Shuffle is reported as looping the playlist, since
    /// this player always wraps.
    fn loop_status(mode: PlayMode) -> LoopStatus {
        match mode {
            PlayMode::Single => LoopStatus::Track,
            PlayMode::Cycle | PlayMode::Shuffle => LoopStatus::Playlist,
        }
    }

    /// `mpris:trackid` has to be a D-Bus object path, whose elements may only
    /// contain `[A-Za-z0-9_]`. YouTube ids are base64url, so their `-` would
    /// make the path invalid — and an invalid trackid makes some clients drop
    /// the metadata whole.
    fn track_path(video_id: &str) -> TrackId {
        let safe: String = video_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        TrackId::try_from(format!("/org/mpris/MediaPlayer2/ytm/track/{safe}"))
            // Also the empty-id case, which would leave a trailing '/'.
            .unwrap_or(TrackId::NO_TRACK)
    }

    fn metadata(now: &NowPlaying) -> Metadata {
        let mut m = Metadata::new();
        let Some(t) = &now.track else {
            return m;
        };
        m.set_trackid(Some(track_path(&t.id)));
        if !t.title.is_empty() {
            m.set_title(Some(t.title.as_str()));
        }
        if !t.artists.is_empty() {
            m.set_artist(Some(t.artists.clone()));
        }
        if !t.album.is_empty() {
            m.set_album(Some(t.album.as_str()));
        }
        if t.length > 0.0 {
            m.set_length(Some(Time::from_micros((t.length * 1e6) as i64)));
        }
        if !t.id.is_empty() {
            m.set_url(Some(format!("https://music.youtube.com/watch?v={}", t.id)));
        }
        m
    }

    /// The properties that differ between two snapshots. `Position` is never
    /// among them — see [`NowPlaying::position`].
    fn changed(now: &NowPlaying, prev: &NowPlaying) -> Vec<Property> {
        let mut out = Vec::new();
        if now.state != prev.state {
            out.push(Property::PlaybackStatus(now.state.into()));
        }
        if now.track != prev.track {
            out.push(Property::Metadata(metadata(now)));
        }
        if now.mode != prev.mode {
            out.push(Property::LoopStatus(loop_status(now.mode)));
            out.push(Property::Shuffle(now.mode == PlayMode::Shuffle));
        }
        if now.volume != prev.volume {
            out.push(Property::Volume(f64::from(now.volume) / 100.0));
        }
        if now.can_go_next != prev.can_go_next {
            out.push(Property::CanGoNext(now.can_go_next));
        }
        if now.can_go_previous != prev.can_go_previous {
            out.push(Property::CanGoPrevious(now.can_go_previous));
        }
        if now.can_play != prev.can_play {
            out.push(Property::CanPlay(now.can_play));
            out.push(Property::CanPause(now.can_play));
        }
        if now.can_seek != prev.can_seek {
            out.push(Property::CanSeek(now.can_seek));
        }
        out
    }

    // ── the interface ────────────────────────────────────────────────────────

    /// The D-Bus side: reads the shared snapshot, posts commands to the UI.
    struct Iface {
        state: Arc<Mutex<NowPlaying>>,
        tx: Sender<MediaCmd>,
    }

    impl Iface {
        fn now(&self) -> NowPlaying {
            lock(&self.state).clone()
        }

        /// Methods return as soon as the command is queued. That is what MPRIS
        /// callers expect — none of them waits for the effect, they watch
        /// `PropertiesChanged` for it.
        fn send(&self, cmd: MediaCmd) -> fdo::Result<()> {
            self.tx
                .send(cmd)
                .map_err(|_| fdo::Error::Failed("player has shut down".into()))
        }
    }

    impl RootInterface for Iface {
        async fn raise(&self) -> fdo::Result<()> {
            // A terminal app cannot raise itself; `CanRaise` says so, and the
            // spec has this do nothing in that case.
            Ok(())
        }

        async fn quit(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Quit)
        }

        async fn can_quit(&self) -> fdo::Result<bool> {
            Ok(true)
        }

        async fn fullscreen(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        async fn set_fullscreen(&self, _fullscreen: bool) -> ZResult<()> {
            Ok(())
        }

        async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        async fn can_raise(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        async fn has_track_list(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        async fn identity(&self) -> fdo::Result<String> {
            Ok("yt-music-tui".into())
        }

        /// The basename of a `.desktop` file, which is where the shell looks
        /// for the icon it shows beside the track. Harmless when absent.
        async fn desktop_entry(&self) -> fdo::Result<String> {
            Ok("ytm".into())
        }

        async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl PlayerInterface for Iface {
        async fn next(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Next)
        }

        async fn previous(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Previous)
        }

        async fn pause(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Pause)
        }

        async fn play_pause(&self) -> fdo::Result<()> {
            self.send(MediaCmd::PlayPause)
        }

        async fn stop(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Stop)
        }

        async fn play(&self) -> fdo::Result<()> {
            self.send(MediaCmd::Play)
        }

        async fn seek(&self, offset: Time) -> fdo::Result<()> {
            self.send(MediaCmd::Seek(offset.as_micros() as f64 / 1e6))
        }

        async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
            // The spec: a SetPosition naming a track other than the current one
            // is to be ignored, not refused — it is a stale click on a seek bar.
            let now = self.now();
            if now.track.as_ref().map(|t| track_path(&t.id)) != Some(track_id) {
                return Ok(());
            }
            self.send(MediaCmd::SeekTo(position.as_micros() as f64 / 1e6))
        }

        async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
            Err(fdo::Error::NotSupported(
                "this player only plays its own library".into(),
            ))
        }

        async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
            Ok(self.now().state.into())
        }

        async fn loop_status(&self) -> fdo::Result<LoopStatus> {
            Ok(loop_status(self.now().mode))
        }

        /// `LoopStatus::None` has no equivalent — this player always wraps —
        /// so it is treated as looping the playlist rather than refused.
        async fn set_loop_status(&self, loop_status: LoopStatus) -> ZResult<()> {
            let mode = self.now().mode;
            let next = match loop_status {
                LoopStatus::Track => PlayMode::Single,
                // Leave shuffle alone: it is a separate MPRIS property.
                LoopStatus::None | LoopStatus::Playlist if mode == PlayMode::Shuffle => {
                    PlayMode::Shuffle
                }
                LoopStatus::None | LoopStatus::Playlist => PlayMode::Cycle,
            };
            let _ = self.send(MediaCmd::Mode(next));
            Ok(())
        }

        async fn rate(&self) -> fdo::Result<PlaybackRate> {
            Ok(1.0)
        }

        /// Rate is fixed at 1.0 (`MinimumRate` == `MaximumRate` say so), so
        /// this is accepted and ignored rather than raising an error.
        async fn set_rate(&self, _rate: PlaybackRate) -> ZResult<()> {
            Ok(())
        }

        async fn shuffle(&self) -> fdo::Result<bool> {
            Ok(self.now().mode == PlayMode::Shuffle)
        }

        async fn set_shuffle(&self, shuffle: bool) -> ZResult<()> {
            let mode = self.now().mode;
            let next = match (shuffle, mode) {
                (true, _) => PlayMode::Shuffle,
                // Turning shuffle off has to land somewhere: back to the
                // in-order mode it displaces.
                (false, PlayMode::Shuffle) => PlayMode::Cycle,
                (false, m) => m,
            };
            let _ = self.send(MediaCmd::Mode(next));
            Ok(())
        }

        async fn metadata(&self) -> fdo::Result<Metadata> {
            Ok(metadata(&self.now()))
        }

        async fn volume(&self) -> fdo::Result<Volume> {
            Ok(f64::from(self.now().volume) / 100.0)
        }

        async fn set_volume(&self, volume: Volume) -> ZResult<()> {
            let v = (volume * 100.0).round().clamp(0.0, 100.0) as u8;
            let _ = self.send(MediaCmd::Volume(v));
            Ok(())
        }

        async fn position(&self) -> fdo::Result<Time> {
            Ok(Time::from_micros((self.now().position * 1e6) as i64))
        }

        async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
            Ok(1.0)
        }

        async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
            Ok(1.0)
        }

        async fn can_go_next(&self) -> fdo::Result<bool> {
            Ok(self.now().can_go_next)
        }

        async fn can_go_previous(&self) -> fdo::Result<bool> {
            Ok(self.now().can_go_previous)
        }

        async fn can_play(&self) -> fdo::Result<bool> {
            Ok(self.now().can_play)
        }

        async fn can_pause(&self) -> fdo::Result<bool> {
            Ok(self.now().can_play)
        }

        async fn can_seek(&self) -> fdo::Result<bool> {
            Ok(self.now().can_seek)
        }

        async fn can_control(&self) -> fdo::Result<bool> {
            Ok(true)
        }
    }

    // ── the handle the app holds ─────────────────────────────────────────────

    /// Owns the D-Bus server and the command channel. Dropping it releases the
    /// bus name, so the player disappears from the desktop on quit.
    pub struct MediaControls {
        state: Arc<Mutex<NowPlaying>>,
        rx: Receiver<MediaCmd>,
        server: Arc<Server<Iface>>,
        rt: tokio::runtime::Handle,
        /// The last snapshot whose properties were announced.
        announced: NowPlaying,
        /// Position and track at the previous update, for spotting a seek.
        last_position: f64,
        last_track: Option<String>,
    }

    impl std::fmt::Debug for MediaControls {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MediaControls")
                .field("bus_name", &self.server.bus_name().as_str())
                .finish()
        }
    }

    impl MediaControls {
        /// Claims a bus name and starts serving. `None` — logged, never fatal —
        /// when there is no session bus to talk to, which is the normal case
        /// over ssh or in a bare tmux; everything else about the app is
        /// unaffected.
        pub fn new(rt: &tokio::runtime::Handle) -> Option<Self> {
            let (tx, rx) = std::sync::mpsc::channel();
            let state = Arc::new(Mutex::new(NowPlaying::default()));
            let iface = Iface {
                state: Arc::clone(&state),
                tx,
            };

            // The spec wants an instance-unique suffix when several copies can
            // run at once; `playerctl -p ytm` still matches the part before it.
            let suffix = format!("ytm.instance{}", std::process::id());

            // `block_on` rather than `spawn` deliberately: zbus decides which
            // reactor to use by asking `Handle::try_current()` while the
            // connection is being built, so building it outside the runtime
            // would silently start a second, async-io driver thread. This is a
            // local socket, so the wait is sub-millisecond.
            let server = match rt.block_on(Server::new(&suffix, iface)) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("[mpris] not available, media keys will not work: {e}");
                    return None;
                }
            };
            log::info!("[mpris] serving {}", server.bus_name());

            Some(Self {
                state,
                rx,
                server: Arc::new(server),
                rt: rt.clone(),
                announced: NowPlaying::default(),
                last_position: 0.0,
                last_track: None,
            })
        }

        /// Publishes the current state. Cheap to call every tick: the snapshot
        /// is always refreshed, but D-Bus traffic only happens on a change.
        pub fn update(&mut self, now: &NowPlaying) {
            *lock(&self.state) = now.clone();

            // A position jump the clock alone cannot explain — the TUI's own
            // arrow keys, or a `SetPosition` just handled. Since `Position` is
            // never in `PropertiesChanged`, `Seeked` is the only way clients
            // hear about it. Skipped across a track change, which the spec
            // says needs no `Seeked`.
            let same_track =
                now.track.as_ref().map(|t| t.id.as_str()) == self.last_track.as_deref();
            if same_track
                && now.state != PlayState::Stopped
                && (now.position - self.last_position).abs() > SEEK_JUMP_SECS
            {
                self.emit_seeked(now.position);
            }
            self.last_position = now.position;
            self.last_track = now.track.as_ref().map(|t| t.id.clone());

            let props = changed(now, &self.announced);
            if props.is_empty() {
                return;
            }
            self.announced = now.clone();

            let server = Arc::clone(&self.server);
            self.rt.spawn(async move {
                if let Err(e) = server.properties_changed(props).await {
                    log::warn!("[mpris] PropertiesChanged failed: {e}");
                }
            });
        }

        /// Next queued command from the desktop, if any.
        pub fn try_recv(&self) -> Option<MediaCmd> {
            self.rx.try_recv().ok()
        }

        /// The claimed bus name, e.g. `org.mpris.MediaPlayer2.ytm.instance4213`.
        pub fn bus_name(&self) -> &str {
            self.server.bus_name().as_str()
        }

        fn emit_seeked(&self, position: f64) {
            let server = Arc::clone(&self.server);
            let position = Time::from_micros((position * 1e6) as i64);
            self.rt.spawn(async move {
                if let Err(e) = server.emit(Signal::Seeked { position }).await {
                    log::warn!("[mpris] Seeked failed: {e}");
                }
            });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::mpris::TrackInfo;

        fn track(id: &str, len: f64) -> TrackInfo {
            TrackInfo {
                id: id.into(),
                title: "Song".into(),
                artists: vec!["A".into()],
                album: "Album".into(),
                length: len,
            }
        }

        #[test]
        fn track_path_survives_a_dash() {
            // Video ids are base64url, so roughly one in eight has a '-'.
            let id = track_path("a-B_c9");
            assert_eq!(id.to_string(), "/org/mpris/MediaPlayer2/ytm/track/a_B_c9");
            assert_ne!(id, TrackId::NO_TRACK);
        }

        #[test]
        fn track_path_falls_back_when_there_is_no_track() {
            assert_eq!(track_path(""), TrackId::NO_TRACK);
        }

        #[test]
        fn position_alone_is_never_a_property_change() {
            let a = NowPlaying {
                position: 1.0,
                ..NowPlaying::default()
            };
            let b = NowPlaying {
                position: 90.0,
                ..a.clone()
            };
            assert!(changed(&b, &a).is_empty());
        }

        #[test]
        fn a_duration_arriving_late_refreshes_metadata() {
            // mpv reports `duration` a beat after the track starts, so the
            // first snapshot of every song has length 0.
            let a = NowPlaying {
                track: Some(track("abc", 0.0)),
                ..NowPlaying::default()
            };
            let b = NowPlaying {
                track: Some(track("abc", 213.0)),
                ..a.clone()
            };
            assert!(matches!(changed(&b, &a)[..], [Property::Metadata(_)]));
        }

        /// The only test that touches D-Bus: claims a real bus name, reads the
        /// properties back off the bus and calls a method on it. Ignored by
        /// default because a session bus is exactly what CI and ssh lack —
        /// which is also the case `MediaControls::new` returns `None` for.
        #[test]
        #[ignore = "needs a session D-Bus"]
        fn round_trips_over_the_session_bus() {
            use std::collections::HashMap;

            use mpris_server::zbus::Connection;
            use mpris_server::zbus::zvariant::{OwnedObjectPath, OwnedValue};

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut controls = MediaControls::new(rt.handle()).expect("a session bus");
            let name = controls.bus_name().to_string();

            controls.update(&NowPlaying {
                state: PlayState::Playing,
                track: Some(TrackInfo {
                    id: "dQw4w9WgXcQ".into(),
                    title: "Never Gonna Give You Up".into(),
                    artists: vec!["Rick Astley".into()],
                    album: "Whenever You Need Somebody".into(),
                    length: 213.0,
                }),
                volume: 80,
                can_go_next: true,
                can_seek: true,
                position: 42.0,
                ..NowPlaying::default()
            });

            let props: HashMap<String, OwnedValue> = rt.block_on(async {
                let conn = Connection::session().await.unwrap();
                let reply = conn
                    .call_method(
                        Some(name.as_str()),
                        "/org/mpris/MediaPlayer2",
                        Some("org.freedesktop.DBus.Properties"),
                        "GetAll",
                        &("org.mpris.MediaPlayer2.Player",),
                    )
                    .await
                    .unwrap();
                reply.body().deserialize().unwrap()
            });

            assert_eq!(
                String::try_from(props["PlaybackStatus"].clone()).unwrap(),
                "Playing"
            );
            assert_eq!(f64::try_from(&props["Volume"]).unwrap(), 0.8);
            assert_eq!(i64::try_from(&props["Position"]).unwrap(), 42_000_000);
            assert!(bool::try_from(&props["CanGoNext"]).unwrap());

            let meta = HashMap::<String, OwnedValue>::try_from(props["Metadata"].clone()).unwrap();
            assert_eq!(
                String::try_from(meta["xesam:title"].clone()).unwrap(),
                "Never Gonna Give You Up"
            );
            assert_eq!(i64::try_from(&meta["mpris:length"]).unwrap(), 213_000_000);
            // An object path on the wire, not a string — a client that gets
            // this wrong is one that drops the metadata whole.
            let trackid = OwnedObjectPath::try_from(meta["mpris:trackid"].clone()).unwrap();
            assert_eq!(
                trackid.as_str(),
                "/org/mpris/MediaPlayer2/ytm/track/dQw4w9WgXcQ"
            );

            // And the other direction: what a media key ends up doing.
            rt.block_on(async {
                let conn = Connection::session().await.unwrap();
                conn.call_method(
                    Some(name.as_str()),
                    "/org/mpris/MediaPlayer2",
                    Some("org.mpris.MediaPlayer2.Player"),
                    "PlayPause",
                    &(),
                )
                .await
                .unwrap();
            });
            assert_eq!(controls.try_recv(), Some(MediaCmd::PlayPause));
        }

        #[test]
        fn shuffle_and_loop_are_reported_together() {
            let a = NowPlaying::default();
            let b = NowPlaying {
                mode: PlayMode::Single,
                ..a.clone()
            };
            assert_eq!(
                changed(&b, &a),
                vec![
                    Property::LoopStatus(LoopStatus::Track),
                    Property::Shuffle(false)
                ]
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{MediaCmd, NowPlaying};

    /// Stand-in for the platforms MPRIS does not cover. macOS wants
    /// `MPNowPlayingInfoCenter` and Windows `SystemMediaTransportControls`,
    /// and both need host plumbing a terminal app has to build first — an
    /// `NSApp` run loop and a hidden window respectively.
    #[derive(Debug)]
    pub struct MediaControls(());

    impl MediaControls {
        pub fn new(_rt: &tokio::runtime::Handle) -> Option<Self> {
            None
        }

        pub fn update(&mut self, _now: &NowPlaying) {}

        pub fn try_recv(&self) -> Option<MediaCmd> {
            None
        }
    }
}

pub use imp::MediaControls;
