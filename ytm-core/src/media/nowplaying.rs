//! The Now Playing centre — what macOS calls the thing MPRIS is on Linux.
//!
//! `MPNowPlayingInfoCenter` is the half we publish to: what Control Centre,
//! the menu bar's Now Playing widget, the Touch Bar and a paired Watch all
//! show. `MPRemoteCommandCenter` is the half we listen on: the media keys, the
//! same widget's buttons, and the play button on a set of headphones.
//!
//! ## Being an app at all
//!
//! Neither works for a process the system does not consider an application, so
//! [`MediaControls::new`] makes this one: `NSApplication::sharedApplication`
//! with an **accessory** activation policy, which is a real app with no Dock
//! icon, no menu bar and no window. That is all AppKit is used for.
//!
//! ## The run loop, without giving up the main thread
//!
//! Remote-command handlers are delivered through the *main thread's* run loop,
//! and a TUI's main thread is busy being a TUI. The usual answer is to give
//! Cocoa the main thread and move the interface onto another one — which this
//! app does not need, because its event loop already comes back here on every
//! tick: [`MediaControls::update`] drains the main run loop with a zero
//! timeout before it returns. A media key therefore costs at most one tick of
//! latency (200 ms, and much less while lyrics are following playback), and
//! nothing about `App::run` has to move.
//!
//! Everything here runs on the main thread, so — unlike the other two backends
//! — there is no channel between threads and no lock. The `mpsc` is still how
//! commands travel, because the handler blocks are called *between* our own
//! stack frames and cannot be handed `&mut App`.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage};
use objc2_core_foundation::CGSize;
use objc2_foundation::{
    NSData, NSDate, NSDefaultRunLoopMode, NSDictionary, NSNumber, NSRunLoop, NSString,
};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPChangeRepeatModeCommand,
    MPChangeRepeatModeCommandEvent, MPChangeShuffleModeCommand, MPChangeShuffleModeCommandEvent,
    MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoCenter, MPNowPlayingInfoMediaType, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus, MPRepeatType, MPShuffleType, MPSkipIntervalCommandEvent,
};

use super::{MediaCmd, NowPlaying, PlayState, TrackInfo, is_seek};
use crate::player::PlayMode;

/// How often the elapsed time is refreshed while nothing else changes. The
/// system extrapolates position from the rate and the last elapsed time it was
/// given, so sending one every tick would buy nothing and cost a dictionary
/// each. A seek does not wait for it — see [`is_seek`].
const ELAPSED_INTERVAL: Duration = Duration::from_secs(5);

/// What `←`/`→` do in the TUI, and what the skip commands are offered as, so
/// the two agree.
const SKIP_SECS: f64 = 5.0;

/// How many turns of the main run loop one tick is allowed to drain. Handler
/// blocks arrive one per turn, and a burst — a run of media-key presses, or a
/// drag on Control Centre's scrubber — should not take a tick each.
const RUN_LOOP_TURNS: usize = 8;

fn playback_state(state: PlayState) -> MPNowPlayingPlaybackState {
    match state {
        PlayState::Playing => MPNowPlayingPlaybackState::Playing,
        PlayState::Paused => MPNowPlayingPlaybackState::Paused,
        PlayState::Stopped => MPNowPlayingPlaybackState::Stopped,
    }
}

/// The Now Playing centre splits what [`PlayMode`] fuses, the same way MPRIS
/// and SMTC do. `Off` has no equivalent — this player always wraps — so the
/// two in-order modes both report repeating everything.
fn repeat_type(mode: PlayMode) -> MPRepeatType {
    match mode {
        PlayMode::Single => MPRepeatType::One,
        PlayMode::Cycle | PlayMode::Shuffle => MPRepeatType::All,
    }
}

fn shuffle_type(mode: PlayMode) -> MPShuffleType {
    if mode == PlayMode::Shuffle {
        MPShuffleType::Items
    } else {
        MPShuffleType::Off
    }
}

/// Control Centre's two controls, collapsed back into the one mode the player
/// has. `Off` on the repeat control is the state this player cannot be in, so
/// it is read as the in-order mode rather than refused.
fn mode_from(repeat: MPRepeatType, shuffling: bool) -> PlayMode {
    match repeat {
        MPRepeatType::One => PlayMode::Single,
        // Leave shuffle alone: it is a separate control with its own command.
        _ if shuffling => PlayMode::Shuffle,
        _ => PlayMode::Cycle,
    }
}

// ── the diff ─────────────────────────────────────────────────────────────────

/// What the panel is currently showing. Held rather than the whole
/// [`NowPlaying`] because the cover arrives separately, long after the track
/// it belongs to — and because most of a [`NowPlaying`] has no equivalent
/// here: there is no volume in Control Centre, and which buttons are live is
/// the command centre's own `isEnabled` rather than anything in the dictionary.
#[derive(Debug, Clone, PartialEq)]
struct Published {
    track: Option<TrackInfo>,
    state: PlayState,
    position: f64,
    mode: PlayMode,
}

/// A command and the opaque object `addTargetWithHandler` gave back for it —
/// which is the only thing that can take the handler off again.
type Target = (Retained<MPRemoteCommand>, Retained<AnyObject>);

/// A cover, and the URL it was fetched from — which is what says whether it
/// still belongs to the track on screen.
type Art = (String, Retained<MPMediaItemArtwork>);

// ── the handle the app holds ─────────────────────────────────────────────────

/// Owns the app's registration with the Now Playing centre. Dropping it clears
/// the panel, so the player disappears from Control Centre on quit.
pub struct MediaControls {
    info: Retained<MPNowPlayingInfoCenter>,
    rx: Receiver<MediaCmd>,
    rt: tokio::runtime::Handle,
    /// The commands' handlers are kept alive by the command centre itself, but
    /// the centre is a singleton shared with anything else in the process —
    /// so the targets are remembered and removed on the way out.
    targets: Vec<Target>,
    /// Kept in their own types, because these two are read *and written*: they
    /// are where Control Centre's shuffle and repeat buttons get the state
    /// they draw, as well as where their presses come from.
    shuffle_command: Retained<MPChangeShuffleModeCommand>,
    repeat_command: Retained<MPChangeRepeatModeCommand>,
    /// The last state published, for the diff.
    published: Option<Published>,
    last_elapsed: Instant,
    /// The cover for the playing track, once its bytes have arrived. Keyed by
    /// the URL they were fetched from, so a track that reuses a cover — a
    /// whole album queued up — costs one request.
    art: Rc<RefCell<Option<Art>>>,
    art_pending: Option<String>,
    art_rx: Receiver<(String, Vec<u8>)>,
    art_tx: Sender<(String, Vec<u8>)>,
    /// Held for what it *proves* rather than what it does: a
    /// [`MainThreadMarker`] is neither `Send` nor `Sync`, so a handle carrying
    /// one cannot be moved to another thread — which is exactly the rule
    /// AppKit and the run-loop pump depend on.
    _mtm: MainThreadMarker,
}

impl std::fmt::Debug for MediaControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaControls")
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

impl MediaControls {
    /// Registers with the Now Playing centre. `None` — logged, never fatal —
    /// when this is not the main thread, which is the one thing AppKit will
    /// not forgive; everything else about the app is unaffected.
    pub fn new(rt: &tokio::runtime::Handle) -> Option<Self> {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("[nowplaying] not the main thread, media keys will not work");
            return None;
        };

        // An accessory app: real enough for the system to route media keys to,
        // with no Dock icon, no menu bar and no window. This does not start a
        // run loop — `update` turns it by hand.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let (tx, rx) = std::sync::mpsc::channel();
        let (art_tx, art_rx) = std::sync::mpsc::channel();
        let commands = register_commands(&tx);
        log::info!("[nowplaying] serving the Now Playing centre");

        Some(Self {
            info: unsafe { MPNowPlayingInfoCenter::defaultCenter() },
            rx,
            rt: rt.clone(),
            targets: commands.targets,
            shuffle_command: commands.shuffle,
            repeat_command: commands.repeat,
            published: None,
            // Far enough back that the first update carries an elapsed time.
            last_elapsed: Instant::now() - ELAPSED_INTERVAL,
            art: Rc::new(RefCell::new(None)),
            art_pending: None,
            art_rx,
            art_tx,
            _mtm: mtm,
        })
    }

    /// Publishes the current state, then turns the main run loop so anything
    /// the system asked for in the meantime is delivered. Cheap to call every
    /// tick: the diff is plain Rust and nothing is published for a snapshot
    /// that has not moved.
    pub fn update(&mut self, now: &NowPlaying) {
        self.drain_art();

        let want = Published {
            track: now.track.clone(),
            state: now.state,
            position: now.position,
            mode: now.mode,
        };
        let previous = self.published.as_ref();

        let track_changed = previous.map(|p| &p.track) != Some(&want.track);
        let state_changed = previous.map(|p| p.state) != Some(want.state);
        if previous.map(|p| p.mode) != Some(want.mode) {
            // Not part of the dictionary: the two controls hold their own
            // state, and this is what makes their buttons draw as on.
            unsafe {
                self.shuffle_command
                    .setCurrentShuffleType(shuffle_type(want.mode));
                self.repeat_command
                    .setCurrentRepeatType(repeat_type(want.mode));
            }
        }
        // The elapsed time moves every tick by definition, so it goes on its
        // own schedule: on a jump the clock cannot explain, and otherwise
        // every few seconds so a watching panel does not drift.
        let seeked = is_seek(
            now,
            previous
                .and_then(|p| p.track.as_ref())
                .map(|t| t.id.as_str()),
            previous.map_or(0.0, |p| p.position),
        );
        let stale = self.last_elapsed.elapsed() >= ELAPSED_INTERVAL;

        if track_changed {
            self.request_art(now.track.as_ref());
        }
        if track_changed || state_changed || seeked || stale {
            self.publish(&want);
            self.last_elapsed = Instant::now();
        } else {
            // Still remember where we are, or the next seek is measured from
            // a position several seconds old.
            self.published = Some(want);
        }

        self.pump();
    }

    /// Next queued command from the system, if any.
    pub fn try_recv(&self) -> Option<MediaCmd> {
        self.rx.try_recv().ok()
    }

    /// Turns the main run loop far enough to deliver whatever the system has
    /// queued, and no further: `distantPast` means every call returns at once
    /// whether or not there was anything to do.
    fn pump(&self) {
        let run_loop = NSRunLoop::mainRunLoop();
        let past = NSDate::distantPast();
        for _ in 0..RUN_LOOP_TURNS {
            // False means the mode had no input sources left to run — nothing
            // is waiting, so neither is the TUI.
            if !run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &past) {
                break;
            }
        }
    }

    fn publish(&mut self, want: &Published) {
        unsafe { self.info.setPlaybackState(playback_state(want.state)) };

        let Some(track) = &want.track else {
            unsafe { self.info.setNowPlayingInfo(None) };
            self.published = Some(want.clone());
            return;
        };

        // Owned first, borrowed second: the dictionary copies its keys and
        // retains its values, but not before this call returns.
        let title = NSString::from_str(&track.title);
        let artist = NSString::from_str(&track.artists.join(", "));
        let album = NSString::from_str(&track.album);
        let duration = NSNumber::new_f64(track.length);
        let elapsed = NSNumber::new_f64(want.position);
        let rate = NSNumber::new_f64(if want.state == PlayState::Playing {
            1.0
        } else {
            0.0
        });
        // Set even when nothing else is: it is how the system knows this is
        // music rather than video, and keeps the display asleep accordingly.
        let media_type = NSNumber::new_usize(MPNowPlayingInfoMediaType::Audio.0);

        let mut keys: Vec<&NSString> = vec![
            unsafe { MPMediaItemPropertyTitle },
            unsafe { MPMediaItemPropertyArtist },
            unsafe { MPMediaItemPropertyAlbumTitle },
            unsafe { MPMediaItemPropertyPlaybackDuration },
            unsafe { MPNowPlayingInfoPropertyElapsedPlaybackTime },
            unsafe { MPNowPlayingInfoPropertyPlaybackRate },
            unsafe { MPNowPlayingInfoPropertyMediaType },
        ];
        let mut values: Vec<&AnyObject> = vec![
            &title,
            &artist,
            &album,
            &duration,
            &elapsed,
            &rate,
            &media_type,
        ];

        // The cover is whatever has arrived for *this* track; a stale one is
        // worse than none, so it is only used when the URL still matches.
        let art = self.art.borrow();
        if let Some((url, artwork)) = art.as_ref()
            && *url == track.art_url
        {
            keys.push(unsafe { MPMediaItemPropertyArtwork });
            values.push(artwork);
        }

        let info = NSDictionary::from_slices(&keys, &values);
        unsafe { self.info.setNowPlayingInfo(Some(&info)) };
        drop(art);
        self.published = Some(want.clone());
    }

    /// Starts fetching the cover for a track, unless it is already in hand.
    /// Unlike the other two backends, this one cannot hand the system a URL —
    /// `MPMediaItemArtwork` wants an image — so the bytes have to come here.
    fn request_art(&mut self, track: Option<&TrackInfo>) {
        let Some(url) = track.map(|t| t.art_url.clone()).filter(|u| !u.is_empty()) else {
            return;
        };
        let held = self.art.borrow().as_ref().is_some_and(|(u, _)| *u == url);
        if held || self.art_pending.as_deref() == Some(url.as_str()) {
            return;
        }
        self.art_pending = Some(url.clone());

        let tx = self.art_tx.clone();
        self.rt.spawn(async move {
            match crate::cover::fetch_bytes(&url).await {
                Ok(bytes) => {
                    let _ = tx.send((url, bytes));
                }
                Err(e) => log::debug!("[nowplaying] no cover for {url}: {e}"),
            }
        });
    }

    /// Turns any arrived cover bytes into an image the panel can draw, and
    /// republishes so it appears under the track already on screen.
    fn drain_art(&mut self) {
        let mut arrived = None;
        while let Ok(next) = self.art_rx.try_recv() {
            arrived = Some(next);
        }
        let Some((url, bytes)) = arrived else {
            return;
        };
        if self.art_pending.as_deref() == Some(url.as_str()) {
            self.art_pending = None;
        }

        let Some(image) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&bytes))
        else {
            log::debug!("[nowplaying] cover from {url} did not decode");
            return;
        };
        // The handler is asked for the image at whatever size the panel wants
        // to draw it; one copy answers every size, which is what every player
        // that is not resampling on demand does.
        let size = image.size();
        let handler = RcBlock::new(move |_: CGSize| NonNull::from(&*image));
        let artwork = unsafe {
            MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                MPMediaItemArtwork::alloc(),
                size,
                &handler,
            )
        };
        *self.art.borrow_mut() = Some((url, artwork));

        // Republish, or the cover waits for whatever changes next.
        if let Some(published) = self.published.clone() {
            self.publish(&published);
        }
    }
}

impl Drop for MediaControls {
    fn drop(&mut self) {
        // The command centre is a process-wide singleton, so the handlers have
        // to be taken off it by hand — a dropped `MediaControls` that left
        // them attached would answer the next media key with a channel nobody
        // is reading.
        for (command, target) in self.targets.drain(..) {
            unsafe { command.removeTarget(Some(&target)) };
        }
        unsafe {
            self.info
                .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            self.info.setNowPlayingInfo(None);
        }
    }
}

// ── the commands ─────────────────────────────────────────────────────────────

/// Attaches a handler that always sends the same command.
fn on(command: &MPRemoteCommand, tx: &Sender<MediaCmd>, cmd: MediaCmd) -> Retained<AnyObject> {
    let tx = tx.clone();
    let handler = RcBlock::new(move |_: NonNull<MPRemoteCommandEvent>| {
        let _ = tx.send(cmd);
        MPRemoteCommandHandlerStatus::Success
    });
    unsafe {
        command.setEnabled(true);
        command.addTargetWithHandler(&handler)
    }
}

/// What [`register_commands`] hands back: everything that has to be detached
/// again on the way out, plus the two commands that are also written to.
struct Commands {
    targets: Vec<Target>,
    /// The two Control Centre also draws state from, so they are handed back
    /// in their own types rather than only as `MPRemoteCommand`s to detach.
    shuffle: Retained<MPChangeShuffleModeCommand>,
    repeat: Retained<MPChangeRepeatModeCommand>,
}

/// Every command this player answers, wired to `tx`.
fn register_commands(tx: &Sender<MediaCmd>) -> Commands {
    let centre = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let mut targets = Vec::new();

    // The plain ones: a press is the whole message.
    let simple: [(Retained<MPRemoteCommand>, MediaCmd); 6] = unsafe {
        [
            (centre.playCommand(), MediaCmd::Play),
            (centre.pauseCommand(), MediaCmd::Pause),
            (centre.togglePlayPauseCommand(), MediaCmd::PlayPause),
            (centre.stopCommand(), MediaCmd::Stop),
            (centre.nextTrackCommand(), MediaCmd::Next),
            // Same double-press gesture as `p`: a media key is the same
            // button, and this is what it does everywhere else in the app.
            (centre.previousTrackCommand(), MediaCmd::Previous),
        ]
    };
    for (command, cmd) in simple {
        let target = on(&command, tx, cmd);
        targets.push((command, target));
    }

    // A drag on Control Centre's scrubber.
    let seek_command = unsafe { centre.changePlaybackPositionCommand() };
    let seek_tx = tx.clone();
    let seek_handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        // The command is declared to deliver this subclass, and the system is
        // the only thing that calls the handler.
        let event: &MPChangePlaybackPositionCommandEvent = unsafe {
            &*event
                .as_ptr()
                .cast::<MPChangePlaybackPositionCommandEvent>()
        };
        let _ = seek_tx.send(MediaCmd::SeekTo(unsafe { event.positionTime() }));
        MPRemoteCommandHandlerStatus::Success
    });
    let seek_target = unsafe {
        seek_command.setEnabled(true);
        seek_command.addTargetWithHandler(&seek_handler)
    };
    targets.push((Retained::into_super(seek_command), seek_target));

    // Skip forward and back, offered at the same five seconds `←`/`→` move.
    for (command, sign) in unsafe {
        [
            (centre.skipForwardCommand(), 1.0),
            (centre.skipBackwardCommand(), -1.0),
        ]
    } {
        let skip_tx = tx.clone();
        let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            let event: &MPSkipIntervalCommandEvent =
                unsafe { &*event.as_ptr().cast::<MPSkipIntervalCommandEvent>() };
            // The system usually sends back one of the intervals offered, but
            // it is not obliged to, so the event is what counts.
            let interval = unsafe { event.interval() };
            let secs = if interval > 0.0 { interval } else { SKIP_SECS };
            let _ = skip_tx.send(MediaCmd::Seek(sign * secs));
            MPRemoteCommandHandlerStatus::Success
        });
        let target = unsafe {
            command.setPreferredIntervals(&objc2_foundation::NSArray::from_retained_slice(&[
                NSNumber::new_f64(SKIP_SECS),
            ]));
            command.setEnabled(true);
            command.addTargetWithHandler(&handler)
        };
        targets.push((Retained::into_super(command), target));
    }

    // Shuffle and repeat are two controls where the player has one mode, so
    // each has to be read against the other's current value.
    let shuffle_command = unsafe { centre.changeShuffleModeCommand() };
    let repeat_command = unsafe { centre.changeRepeatModeCommand() };

    let shuffle_tx = tx.clone();
    let shuffle_repeat = repeat_command.clone();
    let shuffle_handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        let event: &MPChangeShuffleModeCommandEvent =
            unsafe { &*event.as_ptr().cast::<MPChangeShuffleModeCommandEvent>() };
        let shuffling = unsafe { event.shuffleType() } != MPShuffleType::Off;
        let repeat = unsafe { shuffle_repeat.currentRepeatType() };
        let _ = shuffle_tx.send(MediaCmd::Mode(mode_from(repeat, shuffling)));
        MPRemoteCommandHandlerStatus::Success
    });
    let shuffle_target = unsafe {
        shuffle_command.setEnabled(true);
        shuffle_command.addTargetWithHandler(&shuffle_handler)
    };

    let repeat_tx = tx.clone();
    let repeat_shuffle = shuffle_command.clone();
    let repeat_handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        let event: &MPChangeRepeatModeCommandEvent =
            unsafe { &*event.as_ptr().cast::<MPChangeRepeatModeCommandEvent>() };
        let repeat = unsafe { event.repeatType() };
        let shuffling = unsafe { repeat_shuffle.currentShuffleType() } != MPShuffleType::Off;
        let _ = repeat_tx.send(MediaCmd::Mode(mode_from(repeat, shuffling)));
        MPRemoteCommandHandlerStatus::Success
    });
    let repeat_target = unsafe {
        repeat_command.setEnabled(true);
        repeat_command.addTargetWithHandler(&repeat_handler)
    };

    targets.push((
        Retained::into_super(shuffle_command.clone()),
        shuffle_target,
    ));
    targets.push((Retained::into_super(repeat_command.clone()), repeat_target));

    Commands {
        targets,
        shuffle: shuffle_command,
        repeat: repeat_command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_centres_two_controls_collapse_back_into_one_mode() {
        assert_eq!(mode_from(MPRepeatType::One, true), PlayMode::Single);
        assert_eq!(mode_from(MPRepeatType::All, true), PlayMode::Shuffle);
        assert_eq!(mode_from(MPRepeatType::All, false), PlayMode::Cycle);
        // `Off` is the state this player cannot be in — read as in-order
        // rather than refused.
        assert_eq!(mode_from(MPRepeatType::Off, false), PlayMode::Cycle);
    }

    #[test]
    fn a_mode_round_trips_through_control_centres_two_controls() {
        for mode in [PlayMode::Cycle, PlayMode::Single, PlayMode::Shuffle] {
            assert_eq!(
                mode_from(repeat_type(mode), shuffle_type(mode) != MPShuffleType::Off),
                mode
            );
        }
    }
}
