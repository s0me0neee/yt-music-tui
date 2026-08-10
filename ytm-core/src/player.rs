//! Queue and playback orchestration — the UI-agnostic controller extracted
//! from the TUI's event handlers. Owns an [`AudioEngine`] and the playback
//! queue; takes a [`Library`] reference wherever it needs to resolve a
//! `(playlist_idx, song_idx)` pair to an actual track.

use rand::seq::SliceRandom;

use crate::library::Library;
use crate::playback::{AudioEngine, AudioState, Cmd};

/// A track's position within a [`Library`]: `(playlist_idx, song_idx)`.
pub type TrackRef = (usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayMode {
    Cycle,
    Single,
    Shuffle,
}

impl PlayMode {
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Cycle => Self::Single,
            Self::Single => Self::Shuffle,
            Self::Shuffle => Self::Cycle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cycle => "↺ Cycle",
            Self::Single => "⊙ Single",
            Self::Shuffle => "⇌ Shuffle",
        }
    }
}

/// What happened as a result of [`Player::append_to_queue`].
pub enum AppendOutcome {
    /// Nothing was playing, so the appended track started immediately.
    StartedPlaying { track: TrackRef, queue_len: usize },
    /// Something was already playing; the track was appended at `queue_len`
    /// (its 1-based position, since it was pushed onto the end).
    Queued { queue_len: usize },
}

/// What happened as a result of [`Player::remove_from_queue`].
pub enum RemoveOutcome {
    /// The removed entry wasn't the one playing — no playback change.
    Unaffected,
    /// The removed entry was playing and the queue is now empty — stopped.
    Stopped,
    /// The removed entry was playing; playback switched to this track.
    Switched { track: TrackRef },
}

pub struct Player {
    audio: AudioEngine,
    queue: Vec<TrackRef>,
    queue_pos: Option<usize>,
    mode: PlayMode,
    playing: Option<TrackRef>,
    /// True once playback has actually been started (vs. a queue restored
    /// from disk, where `playing` is set but audio hasn't been asked to play
    /// yet — see [`Player::restore`]).
    playback_started: bool,
    volume: u8,
    muted: bool,
    pre_mute_vol: u8,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub fn new() -> Self {
        let audio = AudioEngine::new();
        audio.send(Cmd::Volume(80));
        Self {
            audio,
            queue: Vec::new(),
            queue_pos: None,
            mode: PlayMode::Cycle,
            playing: None,
            playback_started: false,
            volume: 80,
            muted: false,
            pre_mute_vol: 80,
        }
    }

    // ── accessors ────────────────────────────────────────────────────────────

    pub fn audio_state(&self) -> AudioState {
        self.audio.state()
    }

    pub fn queue(&self) -> &[TrackRef] {
        &self.queue
    }

    pub fn queue_position(&self) -> Option<usize> {
        self.queue_pos
    }

    pub fn playing(&self) -> Option<TrackRef> {
        self.playing
    }

    pub fn playback_started(&self) -> bool {
        self.playback_started
    }

    pub fn mode(&self) -> PlayMode {
        self.mode
    }

    pub fn volume(&self) -> u8 {
        self.volume
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// The volume to persist across runs: while muted this is the pre-mute
    /// level, so quitting muted doesn't save a level of 0.
    pub fn effective_volume(&self) -> u8 {
        if self.muted { self.pre_mute_vol } else { self.volume }
    }

    // ── playback ─────────────────────────────────────────────────────────────

    /// Warm the CDN URL cache for `video_id` ahead of an expected `play`/`resume`.
    pub fn prefetch(&self, video_id: &str) {
        if !video_id.is_empty() {
            self.audio.send(Cmd::Prefetch(video_id.to_string()));
        }
    }

    /// Called when the user explicitly selects a song. Always rebuilds the
    /// queue for `pl_idx` in the current mode, then plays `song_idx`.
    pub fn play(&mut self, library: &Library, pl_idx: usize, song_idx: usize) {
        self.build_queue(library, pl_idx, song_idx);
        self.do_play(library, pl_idx, song_idx);
    }

    /// Toggles pause/resume, unless audio is still loading. Returns `true` if
    /// a command was actually sent.
    pub fn toggle_pause(&self) -> bool {
        let ast = self.audio.state();
        if ast.loading {
            return false;
        }
        self.audio.send(if ast.paused { Cmd::Resume } else { Cmd::Pause });
        true
    }

    pub fn seek(&self, delta_secs: i64) {
        self.audio.send(Cmd::Seek(delta_secs));
    }

    /// Sets the volume (0-100) and clears mute.
    pub fn set_volume(&mut self, volume: u8) {
        self.muted = false;
        self.volume = volume.min(100);
        self.audio.send(Cmd::Volume(self.volume));
    }

    /// Adjusts the volume by `delta` (clamped to 0-100) and clears mute.
    pub fn adjust_volume(&mut self, delta: i8) {
        let next = if delta >= 0 {
            self.volume.saturating_add(delta.unsigned_abs())
        } else {
            self.volume.saturating_sub(delta.unsigned_abs())
        };
        self.set_volume(next.min(100));
    }

    pub fn toggle_mute(&mut self) {
        if self.muted {
            self.muted = false;
            self.volume = self.pre_mute_vol;
        } else {
            self.pre_mute_vol = self.volume;
            self.muted = true;
            self.volume = 0;
        }
        self.audio.send(Cmd::Volume(self.volume));
    }

    pub fn cycle_mode(&mut self) {
        let old = self.mode;
        self.mode = self.mode.next();
        self.sync_queue_to_mode(old);
    }

    /// Step through the queue by `delta` positions and play.
    pub fn advance(&mut self, library: &Library, delta: i64) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        let pos = match self.queue_pos {
            Some(p) => ((p as i64 + delta).rem_euclid(n as i64)) as usize,
            None => 0,
        };
        self.queue_pos = Some(pos);
        let (pl, song) = self.queue[pos];
        log::info!("advance_queue: delta={delta} pos={pos} pl={pl} song={song}");
        self.do_play(library, pl, song);
    }

    /// Jump directly to an absolute queue position and play it (e.g. Enter on
    /// an already-highlighted row in the queue panel — no wraparound math).
    pub fn jump_to(&mut self, library: &Library, q_pos: usize) {
        let Some(&(pl, song)) = self.queue.get(q_pos) else {
            return;
        };
        self.queue_pos = Some(q_pos);
        self.do_play(library, pl, song);
    }

    pub fn next(&mut self, library: &Library) {
        self.advance(library, 1);
    }

    pub fn prev(&mut self, library: &Library) {
        self.advance(library, -1);
    }

    /// Call once per tick. Advances (or replays, in `Single` mode) when the
    /// current track finished naturally. Returns `true` if playback changed.
    pub fn handle_song_end(&mut self, library: &Library) -> bool {
        if !self.audio.take_song_ended() {
            return false;
        }
        match self.mode {
            PlayMode::Single => {
                if let Some((pl, song)) = self.playing {
                    self.do_play(library, pl, song);
                }
            }
            PlayMode::Cycle | PlayMode::Shuffle => self.advance(library, 1),
        }
        true
    }

    // ── queue editing ────────────────────────────────────────────────────────

    /// Appends `(pl_idx, song_idx)` to the end of the queue. Works across
    /// playlists — only [`Player::play`] rebuilds/replaces the queue.
    pub fn append_to_queue(
        &mut self,
        library: &Library,
        pl_idx: usize,
        song_idx: usize,
    ) -> AppendOutcome {
        self.queue.push((pl_idx, song_idx));
        let queue_len = self.queue.len();
        log::info!("append_to_queue: pl={pl_idx} song={song_idx} queue_len={queue_len}");

        if self.playing.is_none() {
            self.queue_pos = Some(queue_len - 1);
            self.do_play(library, pl_idx, song_idx);
            AppendOutcome::StartedPlaying {
                track: (pl_idx, song_idx),
                queue_len,
            }
        } else {
            AppendOutcome::Queued { queue_len }
        }
    }

    /// Removes the entry at `q_pos` and fixes up `queue_pos`. If the removed
    /// entry was currently playing, immediately switches to whatever
    /// `queue_pos` now points at (or stops if the queue became empty).
    pub fn remove_from_queue(&mut self, library: &Library, q_pos: usize) -> RemoveOutcome {
        if q_pos >= self.queue.len() {
            return RemoveOutcome::Unaffected;
        }

        let was_playing = self.queue_pos == Some(q_pos);

        self.queue.remove(q_pos);
        log::info!(
            "remove_from_queue: removed q_pos={q_pos} remaining={}",
            self.queue.len()
        );

        self.queue_pos = match self.queue_pos {
            None => None,
            Some(p) if p == q_pos && self.queue.is_empty() => None,
            Some(p) if p >= self.queue.len() => Some(self.queue.len() - 1),
            Some(p) if p > q_pos => Some(p - 1),
            Some(p) => Some(p),
        };

        if !was_playing {
            return RemoveOutcome::Unaffected;
        }

        match self.queue_pos {
            None => {
                self.audio.send(Cmd::Stop);
                self.playing = None;
                log::info!("remove_from_queue: queue empty — stopped playback");
                RemoveOutcome::Stopped
            }
            Some(pos) => {
                let (pl, song) = self.queue[pos];
                log::info!("remove_from_queue: switching to pl={pl} song={song}");
                self.do_play(library, pl, song);
                RemoveOutcome::Switched { track: (pl, song) }
            }
        }
    }

    // ── persistence glue ─────────────────────────────────────────────────────

    /// Restores a previously-saved queue without starting audio: sets the
    /// queue and current position, and warms the CDN cache for the current
    /// track. Call [`Player::start_current`] (e.g. on the user's first
    /// play/pause keypress) to actually begin playback.
    pub fn restore(&mut self, library: &Library, queue: Vec<TrackRef>, position: Option<usize>) {
        self.queue = queue;
        self.queue_pos = position;
        self.playback_started = false;

        let Some(pos) = position else { return };
        let Some(&track) = self.queue.get(pos) else {
            return;
        };
        self.playing = Some(track);
        if let Some(video_id) = library.track(track.0, track.1).and_then(|t| t.video_id.as_deref())
        {
            self.prefetch(video_id);
        }
        log::info!("restore: len={} pos={:?}", self.queue.len(), self.queue_pos);
    }

    /// Starts playback of the currently-selected track. Used after
    /// [`Player::restore`], when [`Player::playback_started`] is still false.
    pub fn start_current(&mut self, library: &Library) {
        if let Some((pl, song)) = self.playing {
            self.do_play(library, pl, song);
        }
    }

    // ── internal ─────────────────────────────────────────────────────────────

    /// Build (or rebuild) the playback queue for `pl_idx`. In Shuffle mode
    /// the order is randomised; `start_song` marks the current position
    /// regardless of order.
    fn build_queue(&mut self, library: &Library, pl_idx: usize, start_song: usize) {
        let n = library.songs(pl_idx).len();
        self.queue = (0..n).map(|i| (pl_idx, i)).collect();
        if matches!(self.mode, PlayMode::Shuffle) {
            self.queue.shuffle(&mut rand::thread_rng());
        }
        self.queue_pos = self
            .queue
            .iter()
            .position(|&(p, s)| p == pl_idx && s == start_song);
        log::info!(
            "build_queue: pl={pl_idx} n={n} mode={} pos={:?}",
            self.mode.label(),
            self.queue_pos
        );
    }

    /// Send the audio command and update playing state — does not touch the queue.
    fn do_play(&mut self, library: &Library, pl_idx: usize, song_idx: usize) {
        let Some(track) = library.track(pl_idx, song_idx) else {
            log::warn!("do_play: no track at pl={pl_idx} song={song_idx}");
            return;
        };
        let video_id = track.video_id.clone();
        log::info!("do_play: pl={pl_idx} song={song_idx} videoId={video_id:?}");
        match video_id {
            Some(id) if !id.is_empty() => {
                self.audio.send(Cmd::Play(id));
                self.audio.send(Cmd::Volume(self.volume));
            }
            _ => log::warn!("do_play: videoId missing — no Play sent"),
        }
        self.playing = Some((pl_idx, song_idx));
        self.playback_started = true;
        self.prefetch_upcoming(library);
    }

    /// Prefetch the next song in the queue so it starts instantly when needed.
    fn prefetch_upcoming(&self, library: &Library) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        let Some(p) = self.queue_pos else { return };
        let next_pos = (p + 1) % n;
        let Some(&(pl, next_song)) = self.queue.get(next_pos) else {
            return;
        };
        if let Some(id) = library.track(pl, next_song).and_then(|t| t.video_id.as_deref()) {
            self.prefetch(id);
        }
    }

    /// Called after `self.mode` is updated. Reorders the live queue so that
    /// Shuffle gives a random order and Cycle/Single give the original index
    /// order. `queue_pos` is re-pinned to the currently playing song so
    /// playback state stays consistent.
    fn sync_queue_to_mode(&mut self, old_mode: PlayMode) {
        match (old_mode, self.mode) {
            (_, PlayMode::Shuffle) => self.queue.shuffle(&mut rand::thread_rng()),
            (PlayMode::Shuffle, _) => self.queue.sort_unstable(),
            _ => {} // Single <-> Cycle switch doesn't need a reorder
        }
        if let Some((song_pl, song)) = self.playing {
            self.queue_pos = self
                .queue
                .iter()
                .position(|&(p, s)| p == song_pl && s == song);
        }
    }
}
