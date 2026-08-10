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
use throbber_widgets_tui::{Throbber, ThrobberState};

use ytm_core::library::SongBatch;
use ytm_core::persistence::{self, QueueState, RestoreOutcome};
use ytm_core::{AppendOutcome, AudioState, Library, PlayMode, Player, RemoveOutcome, Track};

// ── helpers ──────────────────────────────────────────────────────────────────

fn text_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    text.lines()
        .map(|line| {
            let len = line.chars().count();
            if len == 0 { 1 } else { len.div_ceil(w) }
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

fn track_duration(track: &Track) -> Option<String> {
    track
        .duration
        .clone()
        .or_else(|| track.duration_seconds.map(|s| format!("{}:{:02}", s / 60, s % 60)))
}

// ── panel ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Playlists,
    Songs,
}

// ── app ───────────────────────────────────────────────────────────────────────

pub struct App {
    library: Library,
    list_state: TableState,
    songs_state: TableState,
    active_panel: Panel,
    player: Player,
    throbber_state: ThrobberState,
    // queue
    show_queue: bool,
    queue_view_state: ListState,
    notification: Option<(String, Instant)>,
    reauth_requested: bool,
    // background song loading
    songs_rx: std::sync::mpsc::Receiver<SongBatch>,
    pending_queue_restore: Option<QueueState>,
    // filter
    filter: String,
    filter_mode: bool,
    // hit-test areas for mouse events (updated each frame)
    playlists_area: Rect,
    songs_area: Rect,
}

impl App {
    pub fn new(
        library: Library,
        saved_queue: Option<QueueState>,
        songs_rx: std::sync::mpsc::Receiver<SongBatch>,
    ) -> Self {
        let n = library.len();
        let selected = (n > 0).then_some(0);

        Self {
            library,
            list_state: {
                let mut s = TableState::default();
                s.select(selected);
                s
            },
            songs_state: TableState::default(),
            active_panel: Panel::Playlists,
            player: Player::new(),
            throbber_state: ThrobberState::default(),
            show_queue: false,
            queue_view_state: ListState::default(),
            notification: None,
            reauth_requested: false,
            songs_rx,
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
            self.library.apply_song_batch(idx, songs);
        }
        if self.pending_queue_restore.is_some() {
            self.try_restore_queue();
        }
    }

    /// Attempt to restore a saved queue. Called after every song-batch arrival;
    /// waits until ALL playlists referenced in the saved queue have loaded.
    fn try_restore_queue(&mut self) {
        let Some(qs) = self.pending_queue_restore.clone() else {
            return;
        };

        match persistence::try_restore(&self.library, &qs) {
            RestoreOutcome::Pending => {}
            RestoreOutcome::Abandoned => {
                self.pending_queue_restore = None;
            }
            RestoreOutcome::Ready { queue, position } => {
                self.pending_queue_restore = None;
                self.player.restore(&self.library, queue, position);
                self.queue_view_state.select(position);
                self.list_state
                    .select(self.player.playing().map(|(pl, _)| pl));
                log::info!(
                    "try_restore_queue: len={} pos={:?}",
                    self.player.queue().len(),
                    position
                );
            }
        }
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        let mut terminal = ratatui::init();
        ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        let result = self.event_loop(&mut terminal);
        ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture).ok();
        ratatui::restore();

        // Persist queue before anything else so a crash during reauth doesn't lose it.
        if let Some(state) = persistence::build_queue_state(
            &self.library,
            self.player.queue(),
            self.player.queue_position(),
        ) && let Err(e) = persistence::save_queue(&state)
        {
            log::warn!("failed to save queue: {e}");
        }

        result?;
        if self.reauth_requested {
            std::fs::remove_file(ytm_core::session::browser_json_path()).ok();
            let session = ytm_core::Session::new()?;
            session.run_setup()?;
            eprintln!("\nSetup complete. Run the app again to start.");
        }
        Ok(())
    }

    // ── event loop ────────────────────────────────────────────────────────────

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            // Check for SIGTERM / SIGHUP — breaks cleanly so Drop runs and mpv is killed.
            if ytm_core::shutdown::is_shutdown_requested() {
                break Ok(());
            }

            self.drain_song_channel();
            self.throbber_state.calc_next();
            term.draw(|frame| self.render(frame))?;
            if self.player.handle_song_end(&self.library) {
                self.sync_queue_view();
            }
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
                                            self.player.jump_to(&self.library, q_pos);
                                        }
                                    }
                                }
                                Panel::Songs => {
                                    if let (Some(pl), Some(display_idx)) =
                                        (self.list_state.selected(), self.songs_state.selected())
                                    {
                                        let filtered = self.filtered_songs(pl);
                                        if let Some(&song) = filtered.get(display_idx) {
                                            self.player.play(&self.library, pl, song);
                                            self.sync_queue_view();
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
                                if self.player.playing().is_some() {
                                    if self.player.playback_started() {
                                        self.player.toggle_pause();
                                    } else {
                                        // Restored from saved state — start playback now.
                                        self.player.start_current(&self.library);
                                    }
                                }
                            }
                            KeyCode::Char('p') => {
                                self.player.prev(&self.library);
                                self.sync_queue_view();
                            }
                            KeyCode::Char('n') => {
                                self.player.next(&self.library);
                                self.sync_queue_view();
                            }
                            KeyCode::Char('t') => {
                                self.player.cycle_mode();
                                self.sync_queue_view();
                            }
                            KeyCode::Char('m') => self.player.toggle_mute(),
                            // ── queue edit ────────────────────────────────────────────
                            KeyCode::Char('a')
                                if self.active_panel == Panel::Songs && !self.show_queue =>
                            {
                                if let (Some(pl), Some(display_idx)) =
                                    (self.list_state.selected(), self.songs_state.selected())
                                {
                                    let filtered = self.filtered_songs(pl);
                                    if let Some(&song) = filtered.get(display_idx) {
                                        self.do_append_to_queue(pl, song);
                                    }
                                }
                            }
                            KeyCode::Char('d')
                                if self.active_panel == Panel::Songs && self.show_queue =>
                            {
                                if let Some(display_idx) = self.queue_view_state.selected() {
                                    let filtered = self.filtered_queue_positions();
                                    if let Some(&q_pos) = filtered.get(display_idx) {
                                        self.do_remove_from_queue(q_pos);
                                    }
                                }
                            }
                            KeyCode::Char('o') => {
                                self.show_queue = !self.show_queue;
                                self.filter.clear();
                                self.filter_mode = false;
                                if self.show_queue {
                                    self.queue_view_state.select(self.player.queue_position());
                                }
                            }
                            // ── seek ──────────────────────────────────────────────────
                            KeyCode::Left => self.player.seek(-5),
                            KeyCode::Right => self.player.seek(5),
                            // ── volume ────────────────────────────────────────────────
                            KeyCode::Up => self.player.adjust_volume(5),
                            KeyCode::Down => self.player.adjust_volume(-5),
                            // ── quit ──────────────────────────────────────────────────
                            KeyCode::Esc => match self.active_panel {
                                Panel::Songs if !self.filter.is_empty() => self.clear_filter(),
                                Panel::Songs => self.active_panel = Panel::Playlists,
                                Panel::Playlists => break Ok(()),
                            },
                            KeyCode::Char('r') if self.library.is_empty() => {
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

    /// Keeps the queue panel's visual cursor pinned to whatever is currently
    /// playing. No-op while the queue panel isn't visible.
    fn sync_queue_view(&mut self) {
        if self.show_queue {
            self.queue_view_state.select(self.player.queue_position());
        }
    }

    fn do_append_to_queue(&mut self, pl_idx: usize, song_idx: usize) {
        let title = self
            .library
            .track(pl_idx, song_idx)
            .and_then(|t| t.title.clone())
            .unwrap_or_else(|| "song".to_string());
        match self.player.append_to_queue(&self.library, pl_idx, song_idx) {
            AppendOutcome::StartedPlaying { .. } => self.notify(format!("Playing: {title}")),
            AppendOutcome::Queued { queue_len } => {
                self.notify(format!("+ queue #{queue_len}: {title}"));
            }
        }
    }

    fn do_remove_from_queue(&mut self, q_pos: usize) {
        let outcome = self.player.remove_from_queue(&self.library, q_pos);

        // Keep the visual cursor near where the user just deleted from —
        // independent of `queue_pos`, which tracks what's playing.
        let queue_len = self.player.queue().len();
        self.queue_view_state.select(if queue_len == 0 {
            None
        } else {
            Some(q_pos.min(queue_len - 1))
        });

        if let RemoveOutcome::Switched { track } = outcome {
            let title = self
                .library
                .track(track.0, track.1)
                .and_then(|t| t.title.as_deref())
                .unwrap_or("next song")
                .to_string();
            self.notify(format!("▶  {title}"));
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

    /// Original song indices in the selected playlist that match the current
    /// filter. Returns all indices when the filter is empty.
    #[hotpath::measure]
    fn filtered_songs(&self, pl: usize) -> Vec<usize> {
        let songs = self.library.songs(pl);
        if self.filter.is_empty() {
            return (0..songs.len()).collect();
        }
        let q = self.filter.to_lowercase();
        songs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let title = t.title.as_deref().unwrap_or("").to_lowercase();
                let artists = t.artist_names().to_lowercase();
                title.contains(&q) || artists.contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Queue positions whose songs match the current filter.
    /// Returns all positions when the filter is empty.
    fn filtered_queue_positions(&self) -> Vec<usize> {
        let queue = self.player.queue();
        if self.filter.is_empty() {
            return (0..queue.len()).collect();
        }
        let q = self.filter.to_lowercase();
        queue
            .iter()
            .enumerate()
            .filter(|&(_, &(pl, song_idx))| {
                let track = self.library.track(pl, song_idx);
                let title = track
                    .and_then(|t| t.title.as_deref())
                    .unwrap_or("")
                    .to_lowercase();
                let artists = track.map(|t| t.artist_names().to_lowercase()).unwrap_or_default();
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
        let songs = self.library.songs(pl);
        let filtered = self.filtered_songs(pl);
        let base = self.songs_state.selected().unwrap_or(0);
        for display_idx in [base, base + 1] {
            if let Some(&real_idx) = filtered.get(display_idx)
                && let Some(id) = songs.get(real_idx).and_then(|t| t.video_id.as_deref())
            {
                self.player.prefetch(id);
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notification = Some((msg.into(), Instant::now()));
    }

    fn selected_song(&self) -> Option<&Track> {
        let pl = self.list_state.selected()?;
        let display_idx = self.songs_state.selected()?;
        let real_idx = *self.filtered_songs(pl).get(display_idx)?;
        self.library.track(pl, real_idx)
    }

    // ── help / notification bar ───────────────────────────────────────────────

    fn render_help(&mut self, frame: &mut Frame, area: Rect) {
        // Show notification for 2 s, then fall back to context-sensitive hints
        let show_notif = self
            .notification
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() < Duration::from_secs(2));

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

    #[hotpath::measure]
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

        if self.library.is_empty() {
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
            .library
            .entries()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let name = entry.playlist.title.as_str();
                let count = self.library.songs(i).len();
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
        let total_secs = self.library.total_duration_secs(i);
        2 + u16::from(total_secs > 0)
    }

    fn song_info_rows(&self, width: u16) -> u16 {
        let Some(track) = self.selected_song() else {
            return 0;
        };
        let title_rows = text_rows(track.title.as_deref().unwrap_or("Unknown"), width).min(2);
        let has_artists = !track.artists.is_empty();
        let has_duration = track_duration(track).is_some();
        title_rows + u16::from(has_artists) + u16::from(has_duration)
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
        let Some(entry) = self.library.entry(i) else {
            return;
        };

        let total_secs = entry.total_duration_secs;
        let duration = (total_secs > 0).then(|| {
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            if h > 0 {
                format!("{h}h {m}min")
            } else {
                format!("{m}min")
            }
        });

        let mut lines = vec![
            Line::from(Span::from(entry.playlist.title.clone()).bold()),
            Line::from(format!("{} songs", entry.songs.len())),
        ];
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

        let title = track.title.as_deref().unwrap_or("Unknown");
        let title_lines = wrap_n_lines(title, w, 2);

        let artists = {
            let s = track.artist_names();
            (!s.is_empty()).then_some(s)
        };
        let duration = track_duration(track);

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
        let is_loading = current_pl.is_some_and(|i| !self.library.is_loaded(i));
        if is_loading {
            let border_style = if self.active_panel == Panel::Songs {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
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

        let playing = self.player.playing();

        let all_songs = current_pl.map_or(&[][..], |i| self.library.songs(i));

        let filtered = current_pl
            .map(|pl| self.filtered_songs(pl))
            .unwrap_or_default();
        let num_w = all_songs.len().to_string().len();

        let rows: Vec<Row> = filtered
            .iter()
            .map(|&i| {
                let track = &all_songs[i];
                let is_playing = current_pl.map(|pl| (pl, i)) == playing;
                let title = track.title.as_deref().unwrap_or("Unknown");
                let artists = track.artist_names();

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

                let dur = track_duration(track).unwrap_or_default();

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
        let queue_pos = self.player.queue_position();
        let queue = self.player.queue();
        let filtered = self.filtered_queue_positions();

        let items: Vec<ListItem> = filtered
            .iter()
            .map(|&q_pos| {
                let (pl, song_idx) = queue[q_pos];
                let is_current = Some(q_pos) == queue_pos;
                let track = self.library.track(pl, song_idx);

                let title = track.and_then(|t| t.title.as_deref()).unwrap_or("Unknown");
                let artists = track.map(Track::artist_names).unwrap_or_default();

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
            let n = queue.len();
            let pos_str = queue_pos.map_or_else(|| format!("0/{n}"), |p| format!("{}/{n}", p + 1));
            match self.player.mode() {
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
        let ast = self.player.audio_state();

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
                "▶"
            } else if ast.total > 0.0 {
                "⏸"
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
        let volume = self.player.volume();
        let muted = self.player.is_muted();
        let filled = ((volume as usize * 8 + 50) / 100).min(8);
        let vol_bar = if muted {
            "MUTE".to_string()
        } else {
            format!("{}{} {}%", "█".repeat(filled), "░".repeat(8 - filled), volume)
        };
        let mode_str = self.player.mode().label().to_string();
        let w = extra_area.width as usize;
        let pad = w.saturating_sub(vol_bar.chars().count() + mode_str.chars().count());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    vol_bar,
                    if muted {
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

    fn player_track_info(&self, ast: &AudioState) -> (String, Option<String>, String, String) {
        let Some((pl_idx, song_idx)) = self.player.playing() else {
            return (
                "—  No track playing".into(),
                None,
                "0:00".into(),
                "0:00".into(),
            );
        };
        let Some(track) = self.library.track(pl_idx, song_idx) else {
            return (
                "—  No track playing".into(),
                None,
                "0:00".into(),
                "0:00".into(),
            );
        };
        let title = track.title.clone().unwrap_or_else(|| "Unknown".to_string());
        let artist = {
            let s = track.artist_names();
            (!s.is_empty()).then_some(s)
        };
        (title, artist, fmt_secs(ast.elapsed), fmt_secs(ast.total))
    }
}
