//! The System Media Transport Controls — what Windows calls the thing MPRIS is
//! on Linux.
//!
//! One registration answers the same two questions as MPRIS: the keyboard's
//! media keys reach us with the terminal unfocused, and the panel behind the
//! volume flyout (and the lock screen, and every Bluetooth headset's play
//! button) shows the track with working buttons and a seek bar.
//!
//! ## Getting controls without a window
//!
//! The documented Win32 route is
//! `ISystemMediaTransportControlsInterop::GetForWindow`, which wants an HWND
//! and a thread pumping messages for it — this is why `souvlaki` demands an
//! `hwnd` in its config, and why it is no use to a console app. The route taken
//! here is the other one Microsoft documents, for apps that drive the SMTC by
//! hand: create a [`MediaPlayer`], take the controls it owns, and turn off its
//! `CommandManager` so it stops trying to be the thing those controls drive.
//!
//! The player is never given a source and never opens an audio device — mpv
//! still owns every sample. It exists only as the handle the controls hang off,
//! and is kept alive for exactly that reason. `examples/smtc_probe.rs` is the
//! standalone proof that this works from a process with no window at all.
//!
//! ## Shape
//!
//! Mirrors [`crate::playback`]: a dedicated thread owns the WinRT objects and
//! the UI thread never makes a COM call. Every one of these is a call into
//! another process, and [`MediaControls::update`] is invoked on every tick of
//! the event loop — so the diff that decides whether anything is worth saying
//! stays here, in plain Rust, and only the differences cross the channel.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::Playback::MediaPlayer;
use windows::Media::{
    AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
    MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    ShuffleEnabledChangeRequestedEventArgs, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

use super::{MediaCmd, NowPlaying, PlayState, TrackInfo, is_seek};
use crate::player::PlayMode;

/// How long to wait for the worker thread to say whether it has controls.
/// This is local COM against a system service, so it answers in milliseconds;
/// the timeout is only here so a wedged service cannot hold up the TUI.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the timeline is resent while nothing else changes. Microsoft's
/// guidance asks for roughly this, and it is the figure that keeps a panel that
/// nobody is looking at down to one cross-process call every few seconds. A
/// seek does not wait for it — see [`is_seek`].
const TIMELINE_INTERVAL: Duration = Duration::from_secs(5);

/// WinRT measures durations in 100 ns units.
fn ticks(secs: f64) -> TimeSpan {
    TimeSpan {
        Duration: (secs.max(0.0) * 1e7) as i64,
    }
}

fn status(state: PlayState) -> MediaPlaybackStatus {
    match state {
        PlayState::Playing => MediaPlaybackStatus::Playing,
        PlayState::Paused => MediaPlaybackStatus::Paused,
        PlayState::Stopped => MediaPlaybackStatus::Stopped,
    }
}

/// SMTC splits what [`PlayMode`] fuses, the same way MPRIS does: repeat is a
/// tri-state and shuffle is a flag beside it. `None` has no equivalent here
/// either — this player always wraps — so the two in-order modes both report
/// looping the list.
fn repeat_mode(mode: PlayMode) -> MediaPlaybackAutoRepeatMode {
    match mode {
        PlayMode::Single => MediaPlaybackAutoRepeatMode::Track,
        PlayMode::Cycle | PlayMode::Shuffle => MediaPlaybackAutoRepeatMode::List,
    }
}

/// And back, for what the panel's own repeat button asks for. `None` is the
/// third state the button offers and the one this player cannot be in, so it
/// is read as the in-order mode rather than refused.
fn mode_from(repeat: MediaPlaybackAutoRepeatMode, shuffling: bool) -> PlayMode {
    match repeat {
        MediaPlaybackAutoRepeatMode::Track => PlayMode::Single,
        // Leave shuffle alone: it is a separate control with its own event.
        _ if shuffling => PlayMode::Shuffle,
        _ => PlayMode::Cycle,
    }
}

// ── the diff ─────────────────────────────────────────────────────────────────

/// One thing worth telling the panel. Computed on the UI thread by comparing
/// two snapshots, applied on the worker thread.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Status(PlayState),
    /// Which buttons are live: play/pause, next, previous.
    Buttons {
        play: bool,
        next: bool,
        previous: bool,
    },
    Metadata(Option<TrackInfo>),
    /// Position and length, in seconds.
    Timeline {
        position: f64,
        length: f64,
    },
    Mode(PlayMode),
}

/// The properties that differ between two snapshots. The timeline is not among
/// them — it moves every tick by definition, so [`MediaControls::update`] adds
/// it on its own schedule.
fn changed(now: &NowPlaying, prev: &NowPlaying) -> Vec<Change> {
    let mut out = Vec::new();
    if now.state != prev.state {
        out.push(Change::Status(now.state));
    }
    if now.track != prev.track {
        out.push(Change::Metadata(now.track.clone()));
    }
    if (now.can_play, now.can_go_next, now.can_go_previous)
        != (prev.can_play, prev.can_go_next, prev.can_go_previous)
    {
        out.push(Change::Buttons {
            play: now.can_play,
            next: now.can_go_next,
            previous: now.can_go_previous,
        });
    }
    if now.mode != prev.mode {
        out.push(Change::Mode(now.mode));
    }
    out
}

// ── the worker ───────────────────────────────────────────────────────────────

/// Everything the worker thread owns. Held together so the teardown on the way
/// out can reach all of it.
struct Controls {
    player: MediaPlayer,
    smtc: SystemMediaTransportControls,
}

impl Controls {
    /// Builds the controls and wires the panel's buttons to `tx`. The thread
    /// calling this must already be in an apartment.
    fn new(tx: Sender<MediaCmd>) -> windows::core::Result<Self> {
        let player = MediaPlayer::new()?;
        // Without this the player answers the buttons itself, and it has
        // nothing to play — mpv is where the audio actually is.
        player.CommandManager()?.SetIsEnabled(false)?;
        let smtc = player.SystemMediaTransportControls()?;

        // Handlers run on WinRT threadpool threads, which is why the whole
        // interface is a channel: nothing here touches `Player`.
        let button_tx = tx.clone();
        smtc.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let cmd = match args.ok()?.Button()? {
                SystemMediaTransportControlsButton::Play => MediaCmd::Play,
                SystemMediaTransportControlsButton::Pause => MediaCmd::Pause,
                SystemMediaTransportControlsButton::Stop => MediaCmd::Stop,
                SystemMediaTransportControlsButton::Next => MediaCmd::Next,
                SystemMediaTransportControlsButton::Previous => MediaCmd::Previous,
                // FastForward, Rewind, Record, ChannelUp/Down — none of them
                // enabled, so none of them should arrive.
                other => {
                    log::debug!("[smtc] ignoring button {other:?}");
                    return Ok(());
                }
            };
            let _ = button_tx.send(cmd);
            Ok(())
        }))?;

        // Raised by a drag on the flyout's seek bar — but only once the
        // timeline carries a seekable range, which is why `MinSeekTime` and
        // `MaxSeekTime` are always set beside the position.
        let seek_tx = tx.clone();
        smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<
            SystemMediaTransportControls,
            PlaybackPositionChangeRequestedEventArgs,
        >::new(move |_, args| {
            let secs = args.ok()?.RequestedPlaybackPosition()?.Duration as f64 / 1e7;
            let _ = seek_tx.send(MediaCmd::SeekTo(secs));
            Ok(())
        }))?;

        // The panel holds shuffle and repeat as two independent controls, so
        // each event has to be read against the other's current value to name
        // the one mode the player actually has.
        let shuffle_tx = tx.clone();
        let shuffle_smtc = smtc.clone();
        smtc.ShuffleEnabledChangeRequested(&TypedEventHandler::<
            SystemMediaTransportControls,
            ShuffleEnabledChangeRequestedEventArgs,
        >::new(move |_, args| {
            let on = args.ok()?.RequestedShuffleEnabled()?;
            let mode = mode_from(shuffle_smtc.AutoRepeatMode()?, on);
            let _ = shuffle_tx.send(MediaCmd::Mode(mode));
            Ok(())
        }))?;

        let repeat_tx = tx;
        let repeat_smtc = smtc.clone();
        smtc.AutoRepeatModeChangeRequested(&TypedEventHandler::<
            SystemMediaTransportControls,
            AutoRepeatModeChangeRequestedEventArgs,
        >::new(move |_, args| {
            let repeat = args.ok()?.RequestedAutoRepeatMode()?;
            let mode = mode_from(repeat, repeat_smtc.ShuffleEnabled()?);
            let _ = repeat_tx.send(MediaCmd::Mode(mode));
            Ok(())
        }))?;

        // Stop is always available; the rest follow the queue and are set by
        // the first `Buttons` change. Shuffle and repeat have to be given a
        // value *once* before their change-requested events are ever raised.
        smtc.SetIsStopEnabled(true)?;
        smtc.SetShuffleEnabled(false)?;
        smtc.SetAutoRepeatMode(MediaPlaybackAutoRepeatMode::List)?;
        smtc.SetPlaybackStatus(MediaPlaybackStatus::Stopped)?;
        // Not enabled yet. An enabled session with nothing in it is a blank
        // card in the flyout, and the app has a library to browse before it
        // has anything to play — so the first `Metadata` turns it on, and a
        // `Metadata(None)` turns it off again.
        smtc.SetIsEnabled(false)?;

        Ok(Self { player, smtc })
    }

    fn apply(&self, change: &Change) -> windows::core::Result<()> {
        match change {
            Change::Status(state) => self.smtc.SetPlaybackStatus(status(*state))?,
            Change::Buttons {
                play,
                next,
                previous,
            } => {
                self.smtc.SetIsPlayEnabled(*play)?;
                self.smtc.SetIsPauseEnabled(*play)?;
                self.smtc.SetIsNextEnabled(*next)?;
                self.smtc.SetIsPreviousEnabled(*previous)?;
            }
            Change::Metadata(track) => {
                let updater = self.smtc.DisplayUpdater()?;
                let Some(t) = track else {
                    updater.ClearAll()?;
                    updater.Update()?;
                    self.smtc.SetIsEnabled(false)?;
                    return Ok(());
                };
                // Set even when nothing else is: it is how the system knows to
                // keep the screen saver off a playing track.
                updater.SetType(MediaPlaybackType::Music)?;
                let music = updater.MusicProperties()?;
                music.SetTitle(&HSTRING::from(t.title.as_str()))?;
                music.SetArtist(&HSTRING::from(t.artists.join(", ").as_str()))?;
                music.SetAlbumTitle(&HSTRING::from(t.album.as_str()))?;
                if t.art_url.is_empty() {
                    updater.SetThumbnail(None)?;
                } else if let Ok(uri) = Uri::CreateUri(&HSTRING::from(t.art_url.as_str())) {
                    // A remote URL, fetched by the shell — no image bytes pass
                    // through this process, and nothing here waits on the CDN.
                    updater.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&uri)?)?;
                }
                updater.Update()?;
                // There is now something to show. Idempotent, so no need to
                // remember whether it was already on.
                self.smtc.SetIsEnabled(true)?;
            }
            Change::Timeline { position, length } => {
                let props = SystemMediaTransportControlsTimelineProperties::new()?;
                props.SetStartTime(ticks(0.0))?;
                props.SetEndTime(ticks(*length))?;
                props.SetPosition(ticks(*position))?;
                // Without a seekable range there is no bar to drag, and
                // `PlaybackPositionChangeRequested` is never raised at all.
                props.SetMinSeekTime(ticks(0.0))?;
                props.SetMaxSeekTime(ticks(*length))?;
                self.smtc.UpdateTimelineProperties(&props)?;
            }
            Change::Mode(mode) => {
                self.smtc.SetShuffleEnabled(*mode == PlayMode::Shuffle)?;
                self.smtc.SetAutoRepeatMode(repeat_mode(*mode))?;
            }
        }
        Ok(())
    }

    /// Takes the session off the panel. Without this it lingers, paused and
    /// unresponsive, until the shell notices the process is gone.
    fn teardown(&self) {
        let _ = self.smtc.SetIsEnabled(false);
        if let Ok(updater) = self.smtc.DisplayUpdater() {
            let _ = updater.ClearAll();
            let _ = updater.Update();
        }
        let _ = self.player.Close();
    }
}

/// Builds the controls, then applies changes until the sender is dropped.
fn worker(ready: Sender<bool>, cmd_tx: Sender<MediaCmd>, changes: Receiver<Vec<Change>>) {
    // WinRT wants an apartment. MTA, because the events arrive on threadpool
    // threads and nothing here owns a window to pump messages for.
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() } {
        log::warn!("[smtc] no COM apartment, media keys will not work: {e}");
        let _ = ready.send(false);
        return;
    }

    let controls = match Controls::new(cmd_tx) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[smtc] not available, media keys will not work: {e}");
            let _ = ready.send(false);
            return;
        }
    };
    log::info!("[smtc] serving the system media transport controls");
    let _ = ready.send(true);

    while let Ok(batch) = changes.recv() {
        for change in &batch {
            if let Err(e) = controls.apply(change) {
                log::warn!("[smtc] {change:?} failed: {e}");
            }
        }
    }
    controls.teardown();
}

// ── the handle the app holds ─────────────────────────────────────────────────

/// Owns the worker thread. Dropping it takes the session off the panel, so the
/// player disappears from the flyout on quit.
#[derive(Debug)]
pub struct MediaControls {
    changes: Option<Sender<Vec<Change>>>,
    rx: Receiver<MediaCmd>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The last snapshot whose changes were sent.
    announced: NowPlaying,
    /// Position and track at the previous update, for spotting a seek.
    last_position: f64,
    last_track: Option<String>,
    last_timeline: Instant,
}

impl MediaControls {
    /// Registers with the system controls. `None` — logged, never fatal — when
    /// they are unavailable, which is what a Windows build stripped of the
    /// media stack (Server core, an N edition without the Media Feature Pack)
    /// looks like. Everything else about the app is unaffected.
    pub fn new(_rt: &tokio::runtime::Handle) -> Option<Self> {
        let (cmd_tx, rx) = std::sync::mpsc::channel();
        let (changes, changes_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let worker = std::thread::Builder::new()
            .name("smtc".into())
            .spawn(move || worker(ready_tx, cmd_tx, changes_rx))
            .inspect_err(|e| log::warn!("[smtc] could not start its thread: {e}"))
            .ok()?;

        // The thread reports whether it got controls before anything is sent
        // to it — a handle that answers `Some` and then does nothing would be
        // worse than no handle at all.
        match ready_rx.recv_timeout(SETUP_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                log::warn!("[smtc] setup did not finish in {SETUP_TIMEOUT:?}: {e}");
                return None;
            }
        }

        Some(Self {
            changes: Some(changes),
            rx,
            worker: Some(worker),
            announced: NowPlaying::default(),
            last_position: 0.0,
            last_track: None,
            // Far enough back that the first update carries a timeline.
            last_timeline: Instant::now() - TIMELINE_INTERVAL,
        })
    }

    /// Publishes the current state. Cheap to call every tick: the diff is
    /// plain Rust and nothing crosses the channel on a snapshot that has not
    /// moved.
    pub fn update(&mut self, now: &NowPlaying) {
        let mut changes = changed(now, &self.announced);
        if !changes.is_empty() {
            self.announced = now.clone();
        }

        // The timeline is the one thing that changes every tick, so it is sent
        // on its own schedule instead: when the track changes, when the
        // position jumps further than the clock can explain, and otherwise
        // every few seconds so a watching panel does not drift.
        let new_track = now.track.as_ref().map(|t| t.id.as_str()) != self.last_track.as_deref();
        let seeked = is_seek(now, self.last_track.as_deref(), self.last_position);
        if new_track || seeked || self.last_timeline.elapsed() >= TIMELINE_INTERVAL {
            changes.push(Change::Timeline {
                position: now.position,
                length: now.track.as_ref().map_or(0.0, |t| t.length),
            });
            self.last_timeline = Instant::now();
        }
        self.last_position = now.position;
        self.last_track = now.track.as_ref().map(|t| t.id.clone());

        if changes.is_empty() {
            return;
        }
        if let Some(tx) = &self.changes
            && tx.send(changes).is_err()
        {
            // The worker has gone; stop pretending there is a panel.
            self.changes = None;
        }
    }

    /// Next queued command from the panel, if any.
    pub fn try_recv(&self) -> Option<MediaCmd> {
        self.rx.try_recv().ok()
    }
}

impl Drop for MediaControls {
    fn drop(&mut self) {
        // Dropping the sender is what tells the worker to tear down. Joining
        // it is what makes the session gone *before* the process is.
        self.changes = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, len: f64) -> TrackInfo {
        TrackInfo {
            id: id.into(),
            title: "Song".into(),
            artists: vec!["A".into()],
            album: "Album".into(),
            length: len,
            art_url: String::new(),
        }
    }

    fn playing(id: &str, position: f64) -> NowPlaying {
        NowPlaying {
            state: PlayState::Playing,
            track: Some(track(id, 213.0)),
            position,
            ..NowPlaying::default()
        }
    }

    #[test]
    fn position_alone_is_never_a_property_change() {
        // What the timeline is for — and why `changed` must not answer it.
        let a = playing("abc", 1.0);
        let b = playing("abc", 90.0);
        assert!(changed(&b, &a).is_empty());
    }

    #[test]
    fn a_duration_arriving_late_refreshes_metadata() {
        // mpv reports `duration` a beat after the track starts, so the first
        // snapshot of every song has length 0.
        let a = NowPlaying {
            track: Some(track("abc", 0.0)),
            ..NowPlaying::default()
        };
        let b = NowPlaying {
            track: Some(track("abc", 213.0)),
            ..a.clone()
        };
        assert!(matches!(changed(&b, &a)[..], [Change::Metadata(_)]));
    }

    #[test]
    fn a_cover_arriving_late_refreshes_metadata() {
        let a = NowPlaying {
            track: Some(track("abc", 213.0)),
            ..NowPlaying::default()
        };
        let b = NowPlaying {
            track: Some(TrackInfo {
                art_url: "https://example.invalid/cover.jpg".into(),
                ..track("abc", 213.0)
            }),
            ..a.clone()
        };
        assert!(matches!(changed(&b, &a)[..], [Change::Metadata(_)]));
    }

    #[test]
    fn the_three_button_flags_travel_together() {
        // One call site sets all four SMTC properties, so one change carries
        // them: a queue arriving lights up next and previous at once.
        let a = NowPlaying::default();
        let b = NowPlaying {
            can_play: true,
            can_go_next: true,
            can_go_previous: true,
            ..a.clone()
        };
        assert_eq!(
            changed(&b, &a),
            vec![Change::Buttons {
                play: true,
                next: true,
                previous: true
            }]
        );
    }

    #[test]
    fn shuffle_and_repeat_are_one_change() {
        let a = NowPlaying::default();
        let b = NowPlaying {
            mode: PlayMode::Single,
            ..a.clone()
        };
        assert_eq!(changed(&b, &a), vec![Change::Mode(PlayMode::Single)]);
    }

    #[test]
    fn the_panels_two_controls_collapse_back_into_one_mode() {
        // Turning repeat to Track while shuffling: repeat wins, because Single
        // is the mode that has one.
        assert_eq!(
            mode_from(MediaPlaybackAutoRepeatMode::Track, true),
            PlayMode::Single
        );
        assert_eq!(
            mode_from(MediaPlaybackAutoRepeatMode::List, true),
            PlayMode::Shuffle
        );
        assert_eq!(
            mode_from(MediaPlaybackAutoRepeatMode::List, false),
            PlayMode::Cycle
        );
        // `None` is the third state the button offers and the one this player
        // cannot be in — read as in-order rather than refused.
        assert_eq!(
            mode_from(MediaPlaybackAutoRepeatMode::None, false),
            PlayMode::Cycle
        );
    }

    #[test]
    fn a_mode_round_trips_through_the_panels_two_controls() {
        for mode in [PlayMode::Cycle, PlayMode::Single, PlayMode::Shuffle] {
            assert_eq!(
                mode_from(repeat_mode(mode), mode == PlayMode::Shuffle),
                mode
            );
        }
    }

    /// The only test that touches the system controls: publishes a real
    /// session, reads it back the way any client on the machine would, and
    /// then drives it from that side. The counterpart of the Linux backend's
    /// `round_trips_over_the_session_bus`, and ignored for the same reason —
    /// it needs the machine's media stack, registers a session other apps can
    /// see for as long as it runs, and would collide with a second copy.
    ///
    /// `cargo test -p ytm-core smtc -- --ignored`
    #[test]
    #[ignore = "publishes a real SMTC session"]
    fn round_trips_through_the_system_controls() {
        use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;

        const TITLE: &str = "Never Gonna Give You Up";

        // The worker has its own apartment; this thread needs one too.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut controls = MediaControls::new(rt.handle()).expect("system media controls");

        controls.update(&NowPlaying {
            state: PlayState::Playing,
            track: Some(TrackInfo {
                id: "dQw4w9WgXcQ".into(),
                title: TITLE.into(),
                artists: vec!["Rick Astley".into()],
                album: "Whenever You Need Somebody".into(),
                length: 213.0,
                art_url: "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg".into(),
            }),
            mode: PlayMode::Shuffle,
            can_go_next: true,
            can_go_previous: true,
            can_play: true,
            can_seek: true,
            position: 42.0,
            ..NowPlaying::default()
        });

        // The changes cross a channel and are applied by another thread, so
        // give it a moment to reach the shell.
        std::thread::sleep(Duration::from_millis(500));

        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .unwrap()
            .join()
            .unwrap();
        let session = manager
            .GetSessions()
            .unwrap()
            .into_iter()
            .find(|s| {
                s.TryGetMediaPropertiesAsync()
                    .and_then(|op| op.join())
                    .and_then(|p| p.Title())
                    .is_ok_and(|t| t == TITLE)
            })
            .expect("our own session, among whatever else is playing");

        let props = session
            .TryGetMediaPropertiesAsync()
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(props.Artist().unwrap(), "Rick Astley");
        assert_eq!(props.AlbumTitle().unwrap(), "Whenever You Need Somebody");
        // A thumbnail the shell fetched off the URL we handed it.
        assert!(props.Thumbnail().is_ok());

        let info = session.GetPlaybackInfo().unwrap();
        assert_eq!(
            info.PlaybackStatus().unwrap(),
            windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing
        );
        assert!(info.IsShuffleActive().unwrap().Value().unwrap());

        let timeline = session.GetTimelineProperties().unwrap();
        assert_eq!(timeline.Position().unwrap().Duration, 420_000_000);
        assert_eq!(timeline.EndTime().unwrap().Duration, 2_130_000_000);
        // Without a seekable range the flyout draws no bar to drag.
        assert_eq!(timeline.MaxSeekTime().unwrap().Duration, 2_130_000_000);

        // And the other direction: what a media key ends up doing.
        assert!(session.TryPauseAsync().unwrap().join().unwrap());
        let cmd = (0..50)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                controls.try_recv()
            })
            .expect("the button press reaches the event loop");
        assert_eq!(cmd, MediaCmd::Pause);

        // 90 s, in 100 ns ticks — a drag on the flyout's seek bar.
        assert!(
            session
                .TryChangePlaybackPositionAsync(900_000_000)
                .unwrap()
                .join()
                .unwrap()
        );
        let cmd = (0..50)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                controls.try_recv()
            })
            .expect("the seek reaches the event loop");
        assert_eq!(cmd, MediaCmd::SeekTo(90.0));
    }

    #[test]
    fn a_length_becomes_whole_ticks() {
        // 100 ns units, and never negative — mpv's position can come back a
        // hair below zero on a fresh load.
        assert_eq!(ticks(213.0).Duration, 2_130_000_000);
        assert_eq!(ticks(-0.001).Duration, 0);
    }
}
