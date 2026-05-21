use std::time::{Duration, Instant};

use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEvent, MouseEventKind},
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{
        Block, Cell, LineGauge, List, ListItem, ListState, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
    },
};
use serde_json::Value;
use throbber_widgets_tui::{Throbber, ThrobberState};

use crate::audio::{AudioEngine, Cmd as AudioCmd};

// ── helpers ──────────────────────────────────────────────────────────────────

fn text_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    text.lines()
        .map(|line| {
            let len = line.chars().count();
            if len == 0 { 1 } else { (len + w - 1) / w }
        })
        .sum::<usize>()
        .max(1) as u16
}

fn wrap_n_lines(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return vec![text.to_string()];
    }
    let mut result: Vec<String> = Vec::new();
    'outer: for raw in text.lines() {
        let chars: Vec<char> = raw.chars().collect();
        if chars.is_empty() {
            result.push(String::new());
            if result.len() >= max_lines {
                break;
            }
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let end = (start + width).min(chars.len());
            if result.len() + 1 >= max_lines && end < chars.len() {
                let mut s: String = chars[start..start + width.saturating_sub(1)]
                    .iter()
                    .collect();
                s.push('…');
                result.push(s);
                break 'outer;
            }
            result.push(chars[start..end].iter().collect());
            start = end;
            if result.len() >= max_lines {
                break 'outer;
            }
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn truncate_line(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let mut s: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    s.push('…');
    s
}

fn fmt_secs(secs: f64) -> String {
    let s = secs as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

// ── play mode ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum PlayMode {
    Cycle,
    Single,
    Shuffle,
}

impl PlayMode {
    fn next(self) -> Self {
        match self {
            Self::Cycle => Self::Single,
            Self::Single => Self::Shuffle,
            Self::Shuffle => Self::Cycle,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Cycle => "↺ Cycle",
            Self::Single => "⊙ Single",
            Self::Shuffle => "⇌ Shuffle",
        }
    }
}

// ── panel ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Playlists,
    Songs,
}

// ── app ───────────────────────────────────────────────────────────────────────

pub struct App {
    playlists: Vec<Value>,
    list_state: TableState,
    all_songs: Vec<Vec<Value>>,
    songs_state: TableState,
    active_panel: Panel,
    // audio
    audio: AudioEngine,
    playing_pl: Option<usize>,
    playing_song: Option<usize>,
    volume: u8,
    muted: bool,
    pre_mute_vol: u8,
    mode: PlayMode,
    throbber_state: ThrobberState,
    // cached totals — computed once, not every frame
    playlist_total_secs: Vec<u64>,
    // queue
    queue: Vec<usize>,        // song indices within queue_pl, in playback order
    queue_pos: Option<usize>, // current position in queue
    queue_pl: Option<usize>,  // which playlist the queue belongs to
    show_queue: bool,
    queue_view_state: ListState,
    notification: Option<(String, Instant)>,
    reauth_requested: bool,
    // true once do_play fires; false on restore so Space starts rather than pauses
    playback_started: bool,
    // background song loading
    songs_rx:              std::sync::mpsc::Receiver<(usize, Vec<Value>)>,
    songs_loaded:          Vec<bool>,
    pending_queue_restore: Option<crate::config::QueueState>,
    // filter
    filter: String,
    filter_mode: bool,
    // hit-test areas for mouse events (updated each frame)
    playlists_area: Rect,
    songs_area: Rect,
}

impl App {
    pub fn new(
        playlists: Vec<Value>,
        all_songs: Vec<Vec<Value>>,
        saved_queue: Option<crate::config::QueueState>,
        songs_rx: std::sync::mpsc::Receiver<(usize, Vec<Value>)>,
    ) -> Self {
        let n = playlists.len();
        let selected = (n > 0).then_some(0);
        let audio = AudioEngine::new();
        audio.send(AudioCmd::Volume(80));
        let playlist_total_secs = vec![0u64; n];

        Self {
            playlists,
            list_state: {
                let mut s = TableState::default();
                s.select(selected);
                s
            },
            all_songs,
            songs_state: TableState::default(),
            active_panel: Panel::Playlists,
            audio,
            playing_pl: None,
            playing_song: None,
            volume: 80,
            muted: false,
            pre_mute_vol: 80,
            mode: PlayMode::Cycle,
            throbber_state: ThrobberState::default(),
            playlist_total_secs,
            queue: Vec::new(),
            queue_pos: None,
            queue_pl: None,
            show_queue: false,
            queue_view_state: ListState::default(),
            notification: None,
            reauth_requested: false,
            playback_started: false,
            songs_rx,
            songs_loaded: vec![false; n],
            pending_queue_restore: saved_queue,
            filter: String::new(),
            filter_mode: false,
            playlists_area: Rect::default(),
            songs_area: Rect::default(),
        }
    }

    /// Drain all pending song-batch messages from the background loader.
    /// Called each event-loop tick so the UI stays up-to-date without blocking.
    fn drain_song_channel(&mut self) {
        while let Ok((idx, songs)) = self.songs_rx.try_recv() {
            if let Some(slot) = self.all_songs.get_mut(idx) {
                *slot = songs;
            }
            if let Some(loaded) = self.songs_loaded.get_mut(idx) {
                *loaded = true;
            }
            // Recompute total duration for this playlist now that songs are known.
            if let (Some(songs_ref), Some(total_slot)) =
                (self.all_songs.get(idx), self.playlist_total_secs.get_mut(idx))
            {
                *total_slot = songs_ref
                    .iter()
                    .filter_map(|t| t["duration_seconds"].as_u64())
                    .sum();
            }
            if self.pending_queue_restore.is_some() {
                self.try_restore_queue(idx);
            }
        }
    }

    /// Attempt to restore a saved queue once the relevant playlist's songs have loaded.
    fn try_restore_queue(&mut self, loaded_pl_idx: usize) {
        let qs = match &self.pending_queue_restore {
            Some(q) => q,
            None => return,
        };
        let pl_id = match qs.playlist_id.as_deref() {
            Some(id) => id.to_string(),
            None => {
                self.pending_queue_restore = None;
                return;
            }
        };
        let pl_idx = match self
            .playlists
            .iter()
            .position(|pl| pl["playlistId"].as_str() == Some(pl_id.as_str()))
        {
            Some(i) => i,
            None => {
                self.pending_queue_restore = None;
                return;
            }
        };
        if pl_idx != loaded_pl_idx {
            return; // wait for the right playlist to load
        }
        let songs = match self.all_songs.get(pl_idx) {
            Some(s) => s,
            None => return,
        };
        let video_ids = qs.video_ids.clone();
        let saved_pos = qs.position;
        let queue: Vec<usize> = video_ids
            .iter()
            .filter_map(|vid| {
                songs
                    .iter()
                    .position(|s| s["videoId"].as_str() == Some(vid.as_str()))
            })
            .collect();
        self.pending_queue_restore = None;
        if queue.is_empty() {
            return;
        }
        let pos = saved_pos.filter(|&p| p < queue.len());
        self.queue = queue;
        self.queue_pos = pos;
        self.queue_pl = Some(pl_idx);
        self.queue_view_state.select(pos);

        // Show the track in the player bar and navigate to its playlist,
        // but don't start audio — playback_started stays false.
        if let Some(q_pos) = pos {
            if let Some(&song_idx) = self.queue.get(q_pos) {
                self.playing_pl = Some(pl_idx);
                self.playing_song = Some(song_idx);
                self.playback_started = false;

                // Navigate the playlists panel to this playlist.
                self.list_state.select(Some(pl_idx));

                // Warm up the CDN URL so the first play is instant.
                if let Some(vid) = self
                    .all_songs
                    .get(pl_idx)
                    .and_then(|s| s.get(song_idx))
                    .and_then(|t| t["videoId"].as_str())
                {
                    if !vid.is_empty() {
                        self.audio.send(AudioCmd::Prefetch(vid.to_string()));
                    }
                }
            }
        }

        log::info!(
            "try_restore_queue: pl={pl_idx} len={} pos={:?}",
            self.queue.len(),
            self.queue_pos
        );
    }

    /// Serialise the current queue into a `QueueState` for persistence.
    fn queue_state(&self) -> Option<crate::config::QueueState> {
        let pl_idx = self.queue_pl?;
        let songs = self.all_songs.get(pl_idx)?;
        let playlist_id = self
            .playlists
            .get(pl_idx)
            .and_then(|pl| pl["playlistId"].as_str())
            .map(str::to_string);

        let video_ids: Vec<String> = self
            .queue
            .iter()
            .filter_map(|&si| songs.get(si)?["videoId"].as_str().map(str::to_string))
            .collect();

        if video_ids.is_empty() {
            return None;
        }

        Some(crate::config::QueueState {
            playlist_id,
            video_ids,
            position: self.queue_pos,
        })
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        let mut terminal = ratatui::init();
        ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        let result = self.event_loop(&mut terminal);
        ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture).ok();
        ratatui::restore();

        // Persist queue before anything else so a crash during reauth doesn't lose it.
        if let Some(state) = self.queue_state() {
            if let Err(e) = crate::config::save_queue(&state) {
                log::warn!("failed to save queue: {e}");
            }
        }

        result?;
        if self.reauth_requested {
            let path = crate::config::browser_json_path();
            std::fs::remove_file(&path).ok();
            crate::setup::run_setup(&path.to_string_lossy())?;
            eprintln!("\nSetup complete. Run the app again to start.");
        }
        Ok(())
    }

    // ── event loop ────────────────────────────────────────────────────────────

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        loop {
            // Check for SIGTERM / SIGHUP — breaks cleanly so Drop runs and mpv is killed.
            if crate::QUIT.load(Ordering::Relaxed) {
                break Ok(());
            }

            self.drain_song_channel();
            self.throbber_state.calc_next();
            term.draw(|frame| self.render(frame))?;
            self.handle_song_end();
            if event::poll(Duration::from_millis(200))? {
                match event::read()? {
                    Event::Mouse(me) => self.handle_mouse(me),
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        log::debug!("key={:?}", key.code);
                        match key.code {
                            // ── filter mode intercepts all input ──────────────────────
                            _ if self.filter_mode => self.handle_filter_key(key.code),

                            // ── navigation ────────────────────────────────────────────
                            KeyCode::Char('j') => match self.active_panel {
                                Panel::Playlists => {
                                    self.list_state.select_next();
                                    self.songs_state = TableState::default();
                                    self.clear_filter();
                                }
                                Panel::Songs if self.show_queue => {
                                    self.queue_view_state.select_next();
                                }
                                Panel::Songs => {
                                    self.songs_state.select_next();
                                    self.prefetch_selected();
                                }
                            },
                            KeyCode::Char('k') => match self.active_panel {
                                Panel::Playlists => {
                                    self.list_state.select_previous();
                                    self.songs_state = TableState::default();
                                    self.clear_filter();
                                }
                                Panel::Songs if self.show_queue => {
                                    self.queue_view_state.select_previous();
                                }
                                Panel::Songs => {
                                    self.songs_state.select_previous();
                                    self.prefetch_selected();
                                }
                            },
                            KeyCode::Char('h') => {
                                self.active_panel = Panel::Playlists;
                                self.clear_filter();
                            }
                            KeyCode::Char('l') => {
                                self.active_panel = Panel::Songs;
                                if self.songs_state.selected().is_none() {
                                    self.songs_state.select(Some(0));
                                }
                                self.prefetch_selected();
                            }
                            KeyCode::Enter => match self.active_panel {
                                Panel::Playlists => {
                                    self.active_panel = Panel::Songs;
                                    if self.songs_state.selected().is_none() {
                                        self.songs_state.select(Some(0));
                                    }
                                    self.clear_filter();
                                    self.prefetch_selected();
                                }
                                Panel::Songs if self.show_queue => {
                                    if let Some(display_idx) = self.queue_view_state.selected() {
                                        let filtered = self.filtered_queue_positions();
                                        if let Some(&q_pos) = filtered.get(display_idx) {
                                            self.queue_pos = Some(q_pos);
                                            if let (Some(pl), Some(&song)) =
                                                (self.queue_pl, self.queue.get(q_pos))
                                            {
                                                self.do_play(pl, song);
                                            }
                                        }
                                    }
                                }
                                Panel::Songs => {
                                    if let (Some(pl), Some(display_idx)) =
                                        (self.list_state.selected(), self.songs_state.selected())
                                    {
                                        let filtered = self.filtered_songs(pl);
                                        if let Some(&song) = filtered.get(display_idx) {
                                            self.play_song(pl, song);
                                        }
                                    }
                                }
                            },
                            // ── filter ────────────────────────────────────────────────
                            KeyCode::Char('/') if self.active_panel == Panel::Songs => {
                                self.filter_mode = true;
                            }
                            // ── playback ──────────────────────────────────────────────
                            KeyCode::Char(' ') => {
                                if let (Some(pl), Some(song)) =
                                    (self.playing_pl, self.playing_song)
                                {
                                    if !self.playback_started {
                                        // Restored from saved state — start playback now.
                                        self.do_play(pl, song);
                                    } else {
                                        let ast = self
                                            .audio
                                            .state
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        if !ast.loading {
                                            let paused = ast.paused;
                                            drop(ast);
                                            self.audio.send(if paused {
                                                AudioCmd::Resume
                                            } else {
                                                AudioCmd::Pause
                                            });
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('p') => self.play_prev(),
                            KeyCode::Char('n') => self.play_next(),
                            KeyCode::Char('t') => {
                                let old = self.mode;
                                self.mode = self.mode.next();
                                self.sync_queue_to_mode(old);
                            }
                            KeyCode::Char('m') => {
                                if self.muted {
                                    self.muted = false;
                                    self.volume = self.pre_mute_vol;
                                } else {
                                    self.pre_mute_vol = self.volume;
                                    self.muted = true;
                                    self.volume = 0;
                                }
                                self.audio.send(AudioCmd::Volume(self.volume));
                            }
                            // ── queue edit ────────────────────────────────────────────
                            KeyCode::Char('a')
                                if self.active_panel == Panel::Songs && !self.show_queue =>
                            {
                                if let (Some(pl), Some(display_idx)) =
                                    (self.list_state.selected(), self.songs_state.selected())
                                {
                                    let filtered = self.filtered_songs(pl);
                                    if let Some(&song) = filtered.get(display_idx) {
                                        self.append_to_queue(pl, song);
                                    }
                                }
                            }
                            KeyCode::Char('d')
                                if self.active_panel == Panel::Songs && self.show_queue =>
                            {
                                if let Some(display_idx) = self.queue_view_state.selected() {
                                    let filtered = self.filtered_queue_positions();
                                    if let Some(&q_pos) = filtered.get(display_idx) {
                                        self.remove_from_queue(q_pos);
                                    }
                                }
                            }
                            KeyCode::Char('o') => {
                                self.show_queue = !self.show_queue;
                                self.filter.clear();
                                self.filter_mode = false;
                                if self.show_queue {
                                    self.queue_view_state.select(self.queue_pos);
                                }
                            }
                            // ── seek ──────────────────────────────────────────────────
                            KeyCode::Left => self.audio.send(AudioCmd::Seek(-5)),
                            KeyCode::Right => self.audio.send(AudioCmd::Seek(5)),
                            // ── volume ────────────────────────────────────────────────
                            KeyCode::Up => {
                                self.muted = false;
                                self.volume = self.volume.saturating_add(5).min(100);
                                self.audio.send(AudioCmd::Volume(self.volume));
                            }
                            KeyCode::Down => {
                                self.muted = false;
                                self.volume = self.volume.saturating_sub(5);
                                self.audio.send(AudioCmd::Volume(self.volume));
                            }
                            // ── quit ──────────────────────────────────────────────────
                            KeyCode::Esc => match self.active_panel {
                                Panel::Songs if !self.filter.is_empty() => self.clear_filter(),
                                Panel::Songs => self.active_panel = Panel::Playlists,
                                Panel::Playlists => break Ok(()),
                            },
                            KeyCode::Char('r') if self.playlists.is_empty() => {
                                self.reauth_requested = true;
                                break Ok(());
                            }
                            KeyCode::Char('q') => break Ok(()),
                            _ => {}
                        }
                    }
                    _ => {}
                } // match event::read()
            }
        }
    }

    // ── mouse handling ────────────────────────────────────────────────────────

    fn handle_mouse(&mut self, me: MouseEvent) {
        let pos = Position::new(me.column, me.row);
        match me.kind {
            MouseEventKind::ScrollDown => {
                if self.playlists_area.contains(pos) {
                    self.list_state.select_next();
                    self.songs_state = TableState::default();
                    self.filter.clear();
                    self.filter_mode = false;
                } else if self.songs_area.contains(pos) {
                    if self.show_queue {
                        self.queue_view_state.select_next();
                    } else {
                        self.songs_state.select_next();
                        self.prefetch_selected();
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if self.playlists_area.contains(pos) {
                    self.list_state.select_previous();
                    self.songs_state = TableState::default();
                    self.filter.clear();
                    self.filter_mode = false;
                } else if self.songs_area.contains(pos) {
                    if self.show_queue {
                        self.queue_view_state.select_previous();
                    } else {
                        self.songs_state.select_previous();
                        self.prefetch_selected();
                    }
                }
            }
            _ => {}
        }
    }

    // ── playback helpers ──────────────────────────────────────────────────────

    /// Called when the user explicitly selects a song (Enter).
    /// Always rebuilds the queue for the playlist in the current mode.
    fn play_song(&mut self, pl_idx: usize, song_idx: usize) {
        self.build_queue(pl_idx, song_idx);
        self.do_play(pl_idx, song_idx);
    }

    /// Build (or rebuild) the playback queue for `pl_idx`.
    /// In Shuffle mode the order is randomised; `start_song` marks the
    /// current position regardless of order.
    fn build_queue(&mut self, pl_idx: usize, start_song: usize) {
        use rand::seq::SliceRandom;
        let n = self.all_songs.get(pl_idx).map(Vec::len).unwrap_or(0);
        self.queue_pl = Some(pl_idx);
        self.queue = (0..n).collect();
        if matches!(self.mode, PlayMode::Shuffle) {
            self.queue.shuffle(&mut rand::thread_rng());
        }
        self.queue_pos = self.queue.iter().position(|&i| i == start_song);
        if self.show_queue {
            self.queue_view_state.select(self.queue_pos);
        }
        log::info!(
            "build_queue: pl={pl_idx} n={n} mode={} pos={:?}",
            self.mode.label(),
            self.queue_pos
        );
    }

    /// Send the audio command and update playing state — does not touch the queue.
    fn do_play(&mut self, pl_idx: usize, song_idx: usize) {
        let Some(track) = self.all_songs.get(pl_idx).and_then(|s| s.get(song_idx)) else {
            log::warn!("do_play: no track at pl={pl_idx} song={song_idx}");
            return;
        };
        let video_id = track["videoId"].as_str().unwrap_or("").to_string();
        log::info!("do_play: pl={pl_idx} song={song_idx} videoId={video_id:?}");
        if !video_id.is_empty() {
            self.audio.send(AudioCmd::Play(video_id));
            self.audio.send(AudioCmd::Volume(self.volume));
        } else {
            log::warn!("do_play: videoId missing — no Play sent");
        }
        self.playing_pl = Some(pl_idx);
        self.playing_song = Some(song_idx);
        self.playback_started = true;
        self.prefetch_next_in_queue();
    }

    /// Step through the queue by `delta` positions and play.
    fn advance_queue(&mut self, delta: i64) {
        let Some(pl) = self.queue_pl else { return };
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        let pos = match self.queue_pos {
            Some(p) => ((p as i64 + delta).rem_euclid(n as i64)) as usize,
            None => 0,
        };
        self.queue_pos = Some(pos);
        if self.show_queue {
            self.queue_view_state.select(Some(pos));
        }
        let song = self.queue[pos];
        log::info!("advance_queue: delta={delta} pos={pos} song={song}");
        self.do_play(pl, song);
    }

    fn play_next(&mut self) {
        self.advance_queue(1);
    }
    fn play_prev(&mut self) {
        self.advance_queue(-1);
    }

    /// Called after `self.mode` is updated. Reorders the live queue so that
    /// Shuffle gives a random order and Cycle gives the original index order.
    /// `queue_pos` is re-pinned to the currently playing song so playback
    /// state stays consistent.
    fn sync_queue_to_mode(&mut self, old_mode: PlayMode) {
        use rand::seq::SliceRandom;
        match (old_mode, self.mode) {
            (_, PlayMode::Shuffle) => {
                self.queue.shuffle(&mut rand::thread_rng());
            }
            (PlayMode::Shuffle, _) => {
                // Restore original playlist order (queue entries are song indices).
                self.queue.sort_unstable();
            }
            _ => {} // Single↔Cycle switch doesn't need a reorder
        }
        // Re-find where the current song ended up in the new order.
        if let Some(song) = self.playing_song {
            self.queue_pos = self.queue.iter().position(|&i| i == song);
            if self.show_queue {
                self.queue_view_state.select(self.queue_pos);
            }
        }
    }

    fn handle_song_end(&mut self) {
        let ended = {
            let mut ast = self.audio.state.lock().unwrap_or_else(|p| p.into_inner());
            if ast.song_ended {
                ast.song_ended = false;
                true
            } else {
                false
            }
        };
        if !ended {
            return;
        }
        match self.mode {
            PlayMode::Single => {
                if let (Some(pl), Some(song)) = (self.playing_pl, self.playing_song) {
                    self.do_play(pl, song);
                }
            }
            PlayMode::Cycle | PlayMode::Shuffle => self.advance_queue(1),
        }
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_mode = false;
        self.songs_state.select(Some(0));
        self.queue_view_state.select(Some(0));
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.clear_filter(),
            KeyCode::Enter => {
                self.filter_mode = false;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.songs_state.select(Some(0));
                self.queue_view_state.select(Some(0));
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.songs_state.select(Some(0));
                self.queue_view_state.select(Some(0));
            }
            _ => {}
        }
    }

    /// Original song indices in `all_songs[pl]` that match the current filter.
    /// Returns all indices when the filter is empty.
    fn filtered_songs(&self, pl: usize) -> Vec<usize> {
        let songs = self.all_songs.get(pl).map(Vec::as_slice).unwrap_or(&[]);
        if self.filter.is_empty() {
            return (0..songs.len()).collect();
        }
        let q = self.filter.to_lowercase();
        songs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let title = t["title"].as_str().unwrap_or("").to_lowercase();
                let artists = t["artists"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_lowercase()
                    })
                    .unwrap_or_default();
                title.contains(&q) || artists.contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Queue positions whose songs match the current filter.
    /// Returns all positions when the filter is empty.
    fn filtered_queue_positions(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.queue.len()).collect();
        }
        let q = self.filter.to_lowercase();
        let pl = match self.queue_pl {
            Some(p) => p,
            None => return vec![],
        };
        self.queue
            .iter()
            .enumerate()
            .filter(|&(_, &song_idx)| {
                let track = self.all_songs.get(pl).and_then(|s| s.get(song_idx));
                let title = track
                    .and_then(|t| t["title"].as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let artists = track
                    .and_then(|t| t["artists"].as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_lowercase()
                    })
                    .unwrap_or_default();
                title.contains(&q) || artists.contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Prefetch whichever song is currently highlighted in the Songs panel (plus the
    /// one after it). Called on every j/k movement so the CDN URL is warm by the
    /// time the user presses Enter.
    fn prefetch_selected(&self) {
        let Some(pl) = self.list_state.selected() else {
            return;
        };
        let Some(songs) = self.all_songs.get(pl) else {
            return;
        };
        let filtered = self.filtered_songs(pl);
        let base = self.songs_state.selected().unwrap_or(0);
        for display_idx in [base, base + 1] {
            if let Some(&real_idx) = filtered.get(display_idx) {
                if let Some(id) = songs.get(real_idx).and_then(|t| t["videoId"].as_str()) {
                    if !id.is_empty() {
                        self.audio.send(AudioCmd::Prefetch(id.to_string()));
                    }
                }
            }
        }
    }

    /// Prefetch the next song in the queue so it starts instantly when needed.
    fn prefetch_next_in_queue(&self) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        let next_pos = match self.queue_pos {
            Some(p) => (p + 1) % n,
            None => return,
        };
        let Some(&next_song) = self.queue.get(next_pos) else {
            return;
        };
        let Some(pl) = self.queue_pl else { return };
        if let Some(track) = self.all_songs.get(pl).and_then(|s| s.get(next_song)) {
            let id = track["videoId"].as_str().unwrap_or("").to_string();
            if !id.is_empty() {
                self.audio.send(AudioCmd::Prefetch(id));
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notification = Some((msg.into(), Instant::now()));
    }

    /// Append the song at `song_idx` in `pl_idx` to the end of the current queue.
    /// If no queue exists yet for this playlist, start one.
    fn append_to_queue(&mut self, pl_idx: usize, song_idx: usize) {
        if self.queue_pl != Some(pl_idx) {
            // No queue for this playlist yet — initialise an empty one
            self.queue_pl = Some(pl_idx);
            self.queue = Vec::new();
            self.queue_pos = None;
        }
        self.queue.push(song_idx);
        let q_len = self.queue.len();
        let title = self
            .all_songs
            .get(pl_idx)
            .and_then(|s| s.get(song_idx))
            .and_then(|t| t["title"].as_str())
            .unwrap_or("song")
            .to_string();
        log::info!("append_to_queue: pl={pl_idx} song={song_idx} queue_len={q_len}");

        if self.playing_song.is_none() {
            // Nothing playing — start this song immediately
            self.queue_pos = Some(q_len - 1);
            self.do_play(pl_idx, song_idx);
            self.notify(format!("Playing: {title}"));
        } else {
            self.notify(format!("+ queue #{q_len}: {title}"));
        }
    }

    /// Remove the entry at `q_pos` from the queue and fix up `queue_pos`.
    /// If the removed entry was currently playing, immediately switch to whatever
    /// queue_pos now points at (or stop if the queue became empty).
    fn remove_from_queue(&mut self, q_pos: usize) {
        if q_pos >= self.queue.len() {
            return;
        }

        let was_playing = self.queue_pos == Some(q_pos);

        self.queue.remove(q_pos);
        log::info!(
            "remove_from_queue: removed q_pos={q_pos} remaining={}",
            self.queue.len()
        );

        // Adjust queue_pos so it still refers to the same logical position.
        self.queue_pos = match self.queue_pos {
            None => None,
            Some(p) if p == q_pos && self.queue.is_empty() => None,
            Some(p) if p >= self.queue.len() => Some(self.queue.len() - 1),
            Some(p) if p > q_pos => Some(p - 1),
            Some(p) => Some(p),
        };

        // Keep queue_view_state in bounds.
        let new_sel = q_pos.min(self.queue.len().saturating_sub(1));
        self.queue_view_state.select(if self.queue.is_empty() {
            None
        } else {
            Some(new_sel)
        });

        // If we removed the actively playing entry, audio must actually switch.
        if was_playing {
            match (self.queue_pos, self.queue_pl) {
                (None, _) | (_, None) => {
                    self.audio.send(AudioCmd::Stop);
                    self.playing_pl = None;
                    self.playing_song = None;
                    log::info!("remove_from_queue: queue empty — stopped playback");
                }
                (Some(pos), Some(pl)) => {
                    let song = self.queue[pos];
                    let title = self
                        .all_songs
                        .get(pl)
                        .and_then(|s| s.get(song))
                        .and_then(|t| t["title"].as_str())
                        .unwrap_or("next song")
                        .to_string();
                    log::info!("remove_from_queue: switching to pl={pl} song={song}");
                    self.do_play(pl, song);
                    self.notify(format!("▶  {title}"));
                }
            }
        }
    }

    // ── help / notification bar ───────────────────────────────────────────────

    fn render_help(&mut self, frame: &mut Frame, area: Rect) {
        // Show notification for 2 s, then fall back to context-sensitive hints
        let show_notif = self
            .notification
            .as_ref()
            .map(|(_, t)| t.elapsed() < Duration::from_secs(2))
            .unwrap_or(false);

        if !show_notif {
            self.notification = None;
        }

        let line = if self.filter_mode {
            Line::from(vec![
                Span::styled(
                    "/",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.filter.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("█", Style::default().fg(Color::Yellow)),
                Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "↵",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" confirm  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ])
        } else if let Some((msg, _)) = &self.notification {
            Line::from(vec![
                Span::styled("▶ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    msg.clone(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            let items: &[(&str, &str)] = match (self.active_panel, self.show_queue) {
                (Panel::Playlists, _) => &[("j/k", "nav"), ("l/↵", "open"), ("q", "quit")],
                (Panel::Songs, false) => &[
                    ("j/k", "nav"),
                    ("↵", "play"),
                    ("/", "filter"),
                    ("a", "+queue"),
                    ("o", "queue"),
                    ("spc", "pause"),
                    ("p/n", "skip"),
                    ("←/→", "seek"),
                    ("↑/↓", "vol"),
                    ("m", "mute"),
                    ("t", "mode"),
                ],
                (Panel::Songs, true) => &[
                    ("j/k", "nav"),
                    ("↵", "play"),
                    ("/", "filter"),
                    ("d", "remove"),
                    ("o", "songs"),
                    ("spc", "pause"),
                    ("p/n", "skip"),
                    ("←/→", "seek"),
                    ("↑/↓", "vol"),
                    ("m", "mute"),
                    ("t", "mode"),
                ],
            };
            let mut spans: Vec<Span> = Vec::new();
            for (i, (key, desc)) in items.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
                }
                spans.push(Span::styled(
                    key.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" {desc}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        };

        frame.render_widget(Paragraph::new(line), area);
    }

    // ── layout ────────────────────────────────────────────────────────────────

    fn render(&mut self, frame: &mut Frame) {
        // No vertical spacing — player sits flush below the main panels
        let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(6)]);
        let horizontal =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)]).spacing(1);

        let [main, bottom] = frame.area().layout(&vertical);
        let [pg_bar, help_bar] = bottom.layout(&Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(1),
        ]));
        let [playlists, right] = main.layout(&horizontal);

        self.playlists_area = playlists;
        self.songs_area = right;

        self.render_playlists(frame, playlists);
        self.render_right_panel(frame, right);
        self.render_player(frame, pg_bar);
        self.render_help(frame, help_bar);
    }

    // ── playlists panel ───────────────────────────────────────────────────────

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        let border_style = if self.active_panel == Panel::Playlists {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        if self.playlists.is_empty() {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "No playlists found.",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Your session may have expired.",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(vec![
                        Span::styled(
                            "r",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            " re-authenticate  ·  ",
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            "q",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
                    ]),
                ])
                .block(
                    Block::bordered()
                        .title("Playlists")
                        .border_style(border_style),
                )
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }

        let rows: Vec<Row> = self
            .playlists
            .iter()
            .enumerate()
            .map(|(i, pl)| {
                let name = pl["title"].as_str().unwrap_or("Untitled");
                let count = self.all_songs.get(i).map(Vec::len).unwrap_or(0);
                Row::new([
                    Cell::from(Span::styled(
                        name.to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(
                        Line::from(Span::styled(
                            count.to_string(),
                            Style::default().fg(Color::DarkGray),
                        ))
                        .alignment(Alignment::Right),
                    ),
                ])
            })
            .collect();

        let n = rows.len();
        frame.render_stateful_widget(
            Table::new(rows, [Constraint::Fill(1), Constraint::Length(4)])
                .block(
                    Block::bordered()
                        .title("Playlists")
                        .border_style(border_style),
                )
                .row_highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ")
                .column_spacing(1),
            area,
            &mut self.list_state,
        );

        if n > 1 {
            let pos = self.list_state.selected().unwrap_or(0);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut ScrollbarState::new(n).position(pos),
            );
        }
    }

    // ── right panel ───────────────────────────────────────────────────────────

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        let info_height = self.info_height(area.width);
        let [info_area, songs_area] = area.layout(&Layout::vertical([
            Constraint::Length(info_height),
            Constraint::Fill(1),
        ]));

        if self.songs_state.selected().is_some() {
            let [pl_area, song_area] = info_area.layout(&Layout::horizontal([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ]));
            self.render_playlist_info(frame, pl_area);
            self.render_song_info(frame, song_area);
        } else {
            self.render_playlist_info(frame, info_area);
        }

        if self.show_queue {
            self.render_queue(frame, songs_area);
        } else {
            self.render_songs(frame, songs_area);
        }
    }

    fn info_height(&self, area_width: u16) -> u16 {
        let rows = if self.songs_state.selected().is_some() {
            let half = (area_width / 2).saturating_sub(2);
            self.playlist_info_rows(half).max(self.song_info_rows(half))
        } else {
            self.playlist_info_rows(area_width.saturating_sub(2))
        };
        rows.min(3) + 2
    }

    fn playlist_info_rows(&self, _width: u16) -> u16 {
        let Some(i) = self.list_state.selected() else {
            return 0;
        };
        let total_secs = self.playlist_total_secs.get(i).copied().unwrap_or(0);
        2 + if total_secs > 0 { 1 } else { 0 }
    }

    fn song_info_rows(&self, width: u16) -> u16 {
        let Some(track) = self.selected_song() else {
            return 0;
        };
        let title_rows = text_rows(track["title"].as_str().unwrap_or("Unknown"), width).min(2);
        let has_artists = track["artists"]
            .as_array()
            .map(|arr| arr.iter().any(|a| a["name"].as_str().is_some()))
            .unwrap_or(false);
        let has_duration =
            track["duration"].as_str().is_some() || track["duration_seconds"].as_u64().is_some();
        title_rows + has_artists as u16 + has_duration as u16
    }

    fn selected_song(&self) -> Option<&Value> {
        let pl = self.list_state.selected()?;
        let display_idx = self.songs_state.selected()?;
        let real_idx = *self.filtered_songs(pl).get(display_idx)?;
        self.all_songs.get(pl)?.get(real_idx)
    }

    /// Build a panel title that shows the active filter query.
    fn filter_title(&self, base: &str) -> String {
        if self.filter.is_empty() {
            return base.to_string();
        }
        if self.filter_mode {
            format!("{base}  /{}█", self.filter)
        } else {
            format!("{base}  /{}", self.filter)
        }
    }

    fn render_playlist_info(&self, frame: &mut Frame, area: Rect) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let pl = &self.playlists[i];
        let songs = self.all_songs.get(i).map(Vec::as_slice).unwrap_or(&[]);

        let title = pl["title"].as_str().unwrap_or("Untitled");
        let author = pl["author"].as_array().and_then(|arr| {
            let s = arr
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if s.is_empty() { None } else { Some(s) }
        });

        let total_secs = self.playlist_total_secs.get(i).copied().unwrap_or(0);
        let duration = (total_secs > 0).then(|| {
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            if h > 0 {
                format!("{h}h {m}min")
            } else {
                format!("{m}min")
            }
        });

        let title_line = if let Some(a) = author {
            Line::from(vec![
                Span::from(title.to_string()).bold(),
                Span::from(format!("  by {a}")).dim(),
            ])
        } else {
            Line::from(Span::from(title.to_string()).bold())
        };

        let mut lines = vec![title_line, Line::from(format!("{} songs", songs.len()))];
        if let Some(d) = duration {
            lines.push(Line::from(d));
        }

        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title("Info"))
                .wrap(Wrap { trim: false })
                .alignment(Alignment::Center),
            area,
        );
    }

    fn render_song_info(&self, frame: &mut Frame, area: Rect) {
        let Some(track) = self.selected_song() else {
            return;
        };
        let w = area.width.saturating_sub(2) as usize;

        let title = track["title"].as_str().unwrap_or("Unknown");
        let title_lines = wrap_n_lines(title, w, 2);

        let artists = track["artists"].as_array().and_then(|arr| {
            let s = arr
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if s.is_empty() { None } else { Some(s) }
        });
        let duration = track["duration"].as_str().map(str::to_string).or_else(|| {
            track["duration_seconds"]
                .as_u64()
                .map(|s| format!("{}:{:02}", s / 60, s % 60))
        });

        let mut lines: Vec<Line> = title_lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    Line::from(Span::from(l).bold())
                } else {
                    Line::from(l)
                }
            })
            .collect();
        if let Some(a) = artists {
            lines.push(Line::from(truncate_line(&a, w)));
        }
        if let Some(d) = duration {
            lines.push(Line::from(truncate_line(&d, w)));
        }

        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title("Track"))
                .alignment(Alignment::Center),
            area,
        );
    }

    // ── songs list ────────────────────────────────────────────────────────────

    fn render_songs(&mut self, frame: &mut Frame, area: Rect) {
        let current_pl = self.list_state.selected();

        // Show loading spinner while background fetch is in progress.
        let is_loading = current_pl
            .and_then(|i| self.songs_loaded.get(i))
            .map(|&loaded| !loaded)
            .unwrap_or(false);
        if is_loading {
            let border_style = if self.active_panel == Panel::Songs {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let loading_block = Block::bordered().title("Songs").border_style(border_style);
            let inner = loading_block.inner(area);
            frame.render_widget(loading_block, area);
            frame.render_stateful_widget(
                throbber_widgets_tui::Throbber::default()
                    .label(" Loading…")
                    .throbber_style(Style::default().fg(Color::Cyan)),
                inner,
                &mut self.throbber_state,
            );
            return;
        }

        let playing_pl = self.playing_pl;
        let playing_song = self.playing_song;

        let all_songs = current_pl
            .and_then(|i| self.all_songs.get(i))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let filtered = current_pl
            .map(|pl| self.filtered_songs(pl))
            .unwrap_or_default();
        let num_w = all_songs.len().to_string().len();

        let rows: Vec<Row> = filtered
            .iter()
            .map(|&i| {
                let track = &all_songs[i];
                let is_playing = current_pl == playing_pl && Some(i) == playing_song;
                let title = track["title"].as_str().unwrap_or("Unknown");
                let artists = track["artists"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| a["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();

                let indicator = if is_playing { "♫ " } else { "  " };
                let num = format!("{:>num_w$}. ", i + 1);

                let mut spans = vec![
                    Span::styled(indicator, Style::default().fg(Color::Green)),
                    Span::styled(num, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        title,
                        if is_playing {
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().add_modifier(Modifier::BOLD)
                        },
                    ),
                ];
                if !artists.is_empty() {
                    spans.push(Span::styled("  —  ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled(artists, Style::default().fg(Color::Gray)));
                }

                let dur = track["duration"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| {
                        track["duration_seconds"]
                            .as_u64()
                            .map(|s| format!("{}:{:02}", s / 60, s % 60))
                    })
                    .unwrap_or_default();

                Row::new([
                    Cell::from(Line::from(spans)),
                    Cell::from(
                        Line::from(Span::styled(dur, Style::default().fg(Color::DarkGray)))
                            .alignment(Alignment::Right),
                    ),
                ])
            })
            .collect();

        let border_style = if self.active_panel == Panel::Songs {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let item_count = rows.len();
        let panel_title = self.filter_title("Songs");
        frame.render_stateful_widget(
            Table::new(rows, [Constraint::Fill(1), Constraint::Length(5)])
                .block(
                    Block::bordered()
                        .title(panel_title)
                        .border_style(border_style),
                )
                .row_highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ")
                .column_spacing(1),
            area,
            &mut self.songs_state,
        );

        if item_count > 1 {
            let pos = self.songs_state.selected().unwrap_or(0);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut ScrollbarState::new(item_count).position(pos),
            );
        }
    }

    // ── queue view ────────────────────────────────────────────────────────────

    fn render_queue(&mut self, frame: &mut Frame, area: Rect) {
        let pl = self.queue_pl;
        let queue_pos = self.queue_pos;
        let filtered = self.filtered_queue_positions();

        let items: Vec<ListItem> = filtered
            .iter()
            .map(|&q_pos| {
                let song_idx = self.queue[q_pos];
                let is_current = Some(q_pos) == queue_pos;
                let track = pl
                    .and_then(|p| self.all_songs.get(p))
                    .and_then(|songs| songs.get(song_idx));

                let title = track.and_then(|t| t["title"].as_str()).unwrap_or("Unknown");
                let artists = track
                    .and_then(|t| t["artists"].as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| a["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();

                let indicator = if is_current { "♫ " } else { "  " };
                let num = format!("{:>3}. ", q_pos + 1);

                let mut spans = vec![
                    Span::styled(indicator, Style::default().fg(Color::Green)),
                    Span::styled(num, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        title,
                        if is_current {
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().add_modifier(Modifier::BOLD)
                        },
                    ),
                ];
                if !artists.is_empty() {
                    spans.push(Span::styled("  —  ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled(artists, Style::default().fg(Color::Gray)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let border_style = if self.active_panel == Panel::Songs {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let base_title = {
            let n = self.queue.len();
            let pos_str = self
                .queue_pos
                .map(|p| format!("{}/{n}", p + 1))
                .unwrap_or_else(|| format!("0/{n}"));
            match self.mode {
                PlayMode::Shuffle => format!("Queue  [{pos_str}]  ⇌ Shuffle"),
                PlayMode::Single => format!("Queue  [{pos_str}]  ⊙ Single"),
                PlayMode::Cycle => format!("Queue  [{pos_str}]"),
            }
        };
        let item_count = items.len();
        let panel_title = self.filter_title(&base_title);
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::bordered()
                        .title(panel_title)
                        .border_style(border_style),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ "),
            area,
            &mut self.queue_view_state,
        );

        if item_count > 1 {
            let pos = self.queue_view_state.selected().unwrap_or(0);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut ScrollbarState::new(item_count).position(pos),
            );
        }
    }

    // ── player bar ────────────────────────────────────────────────────────────

    fn render_player(&mut self, frame: &mut Frame, area: Rect) {
        let ast = self
            .audio
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();

        let title = if ast.error.is_some() {
            "Error"
        } else {
            "Player"
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [gauge_area, title_area, extra_area] = inner.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]));

        let (title_text, artist_text, elapsed_str, total_str) = self.player_track_info(&ast);

        // ── row 1: [icon] [elapsed] [progress bar] [total] ─────────────────
        if ast.loading {
            frame.render_stateful_widget(
                Throbber::default().throbber_style(Style::default().fg(Color::Cyan)),
                gauge_area,
                &mut self.throbber_state,
            );
        } else {
            let [icon_area, elapsed_area, bar_area, total_area] =
                gauge_area.layout(&Layout::horizontal([
                    Constraint::Length(2),
                    Constraint::Length(5),
                    Constraint::Fill(1),
                    Constraint::Length(5),
                ]));
            let ratio = if ast.total > 0.0 {
                (ast.elapsed / ast.total).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let state_icon = if ast.paused {
                "⏸"
            } else if ast.total > 0.0 {
                "▶"
            } else {
                ""
            };
            frame.render_widget(Paragraph::new(state_icon), icon_area);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    elapsed_str.clone(),
                    Style::default().fg(Color::DarkGray),
                )),
                elapsed_area,
            );
            frame.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .filled_symbol(symbols::line::THICK.horizontal)
                    .unfilled_symbol(symbols::line::NORMAL.horizontal)
                    .filled_style(Style::default().fg(Color::Cyan))
                    .unfilled_style(Style::default().fg(Color::DarkGray)),
                bar_area,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    total_str.clone(),
                    Style::default().fg(Color::DarkGray),
                ))
                .alignment(Alignment::Right),
                total_area,
            );
        }

        // ── row 2: song title — artist (grey+italic when paused) ───────────
        let track_line = match artist_text {
            Some(ref a) => format!("{title_text}  —  {a}"),
            None => title_text.clone(),
        };
        let track_style = if ast.paused {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let track_str = truncate_line(&track_line, title_area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(track_str, track_style)),
            title_area,
        );

        // ── row 3: volume bar (left) · mode (right) ─────────────────────────
        let filled = ((self.volume as usize * 8 + 50) / 100).min(8);
        let vol_bar = if self.muted {
            "MUTE".to_string()
        } else {
            format!(
                "{}{} {}%",
                "█".repeat(filled),
                "░".repeat(8 - filled),
                self.volume
            )
        };
        let mode_str = self.mode.label().to_string();
        let w = extra_area.width as usize;
        let pad = w.saturating_sub(vol_bar.chars().count() + mode_str.chars().count());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    vol_bar,
                    if self.muted {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::from(" ".repeat(pad)),
                Span::styled(mode_str, Style::default().fg(Color::DarkGray)),
            ])),
            extra_area,
        );
    }

    fn player_track_info(
        &self,
        ast: &crate::audio::AudioState,
    ) -> (String, Option<String>, String, String) {
        let (Some(pl_idx), Some(song_idx)) = (self.playing_pl, self.playing_song) else {
            return (
                "—  No track playing".into(),
                None,
                "0:00".into(),
                "0:00".into(),
            );
        };
        let Some(track) = self.all_songs.get(pl_idx).and_then(|s| s.get(song_idx)) else {
            return (
                "—  No track playing".into(),
                None,
                "0:00".into(),
                "0:00".into(),
            );
        };
        let title = track["title"].as_str().unwrap_or("Unknown").to_string();
        let artist = track["artists"].as_array().and_then(|arr| {
            let s = arr
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if s.is_empty() { None } else { Some(s) }
        });
        (title, artist, fmt_secs(ast.elapsed), fmt_secs(ast.total))
    }
}
