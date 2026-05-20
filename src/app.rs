use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, KeyCode},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Block, LineGauge, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;

use crate::audio::{AudioEngine, Cmd as AudioCmd};

// ── helpers ──────────────────────────────────────────────────────────────────

fn text_rows(text: &str, width: u16) -> u16 {
    if width == 0 { return 1; }
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
    if width == 0 || max_lines == 0 { return vec![text.to_string()]; }
    let mut result: Vec<String> = Vec::new();
    'outer: for raw in text.lines() {
        let chars: Vec<char> = raw.chars().collect();
        if chars.is_empty() {
            result.push(String::new());
            if result.len() >= max_lines { break; }
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let end = (start + width).min(chars.len());
            if result.len() + 1 >= max_lines && end < chars.len() {
                let mut s: String = chars[start..start + width.saturating_sub(1)].iter().collect();
                s.push('…');
                result.push(s);
                break 'outer;
            }
            result.push(chars[start..end].iter().collect());
            start = end;
            if result.len() >= max_lines { break 'outer; }
        }
    }
    if result.is_empty() { result.push(String::new()); }
    result
}

fn truncate_line(text: &str, max_chars: usize) -> String {
    if max_chars == 0 { return String::new(); }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars { return text.to_string(); }
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
enum PlayMode { Cycle, Single, Shuffle }

impl PlayMode {
    fn next(self) -> Self {
        match self {
            Self::Cycle   => Self::Single,
            Self::Single  => Self::Shuffle,
            Self::Shuffle => Self::Cycle,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Cycle   => "Cycle",
            Self::Single  => "Single",
            Self::Shuffle => "Shuffle",
        }
    }
}

// ── panel ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Panel { Playlists, Songs }

// ── app ───────────────────────────────────────────────────────────────────────

pub struct App {
    playlists:    Vec<Value>,
    list_state:   ListState,
    all_songs:    Vec<Vec<Value>>,
    songs_state:  ListState,
    active_panel: Panel,
    // audio
    audio:        AudioEngine,
    playing_pl:   Option<usize>,
    playing_song: Option<usize>,
    volume:       u8,
    mode:         PlayMode,
}

impl App {
    pub fn new(playlists: Vec<Value>, all_songs: Vec<Vec<Value>>) -> Self {
        let selected = (!playlists.is_empty()).then_some(0);
        let audio    = AudioEngine::new();
        // Set initial volume
        audio.send(AudioCmd::Volume(80));
        Self {
            playlists,
            list_state:   ListState::default().with_selected(selected),
            all_songs,
            songs_state:  ListState::default(),
            active_panel: Panel::Playlists,
            audio,
            playing_pl:   None,
            playing_song: None,
            volume:       80,
            mode:         PlayMode::Cycle,
        }
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        ratatui::run(|term| self.event_loop(term))?;
        Ok(())
    }

    // ── event loop ────────────────────────────────────────────────────────────

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            term.draw(|frame| self.render(frame))?;
            if let Some(key) = event::read()?.as_key_press_event() {
                log::debug!("key={:?}", key.code);
                match key.code {
                    // ── navigation ────────────────────────────────────────────
                    KeyCode::Char('j') => match self.active_panel {
                        Panel::Playlists => {
                            self.list_state.select_next();
                            self.songs_state = ListState::default();
                        }
                        Panel::Songs => self.songs_state.select_next(),
                    },
                    KeyCode::Char('k') => match self.active_panel {
                        Panel::Playlists => {
                            self.list_state.select_previous();
                            self.songs_state = ListState::default();
                        }
                        Panel::Songs => self.songs_state.select_previous(),
                    },
                    KeyCode::Char('h') => self.active_panel = Panel::Playlists,
                    KeyCode::Char('l') => {
                        self.active_panel = Panel::Songs;
                        if self.songs_state.selected().is_none() {
                            self.songs_state.select(Some(0));
                        }
                    }
                    KeyCode::Enter => match self.active_panel {
                        Panel::Playlists => {
                            self.active_panel = Panel::Songs;
                            if self.songs_state.selected().is_none() {
                                self.songs_state.select(Some(0));
                            }
                        }
                        Panel::Songs => {
                            if let (Some(pl), Some(song)) = (
                                self.list_state.selected(),
                                self.songs_state.selected(),
                            ) {
                                self.play_song(pl, song);
                            }
                        }
                    },
                    // ── playback ──────────────────────────────────────────────
                    KeyCode::Char(' ') => {
                        if self.playing_song.is_some() {
                            let paused = self.audio.state.lock().unwrap().paused;
                            self.audio.send(if paused { AudioCmd::Resume } else { AudioCmd::Pause });
                        }
                    }
                    KeyCode::Char('p') => self.play_prev(),
                    KeyCode::Char('n') => self.play_next(),
                    KeyCode::Char('m') => self.mode = self.mode.next(),
                    // ── seek ──────────────────────────────────────────────────
                    KeyCode::Left  => self.audio.send(AudioCmd::Seek(-5)),
                    KeyCode::Right => self.audio.send(AudioCmd::Seek(5)),
                    // ── volume ────────────────────────────────────────────────
                    KeyCode::Up => {
                        self.volume = self.volume.saturating_add(5).min(100);
                        self.audio.send(AudioCmd::Volume(self.volume));
                    }
                    KeyCode::Down => {
                        self.volume = self.volume.saturating_sub(5);
                        self.audio.send(AudioCmd::Volume(self.volume));
                    }
                    // ── quit ──────────────────────────────────────────────────
                    KeyCode::Esc => match self.active_panel {
                        Panel::Songs     => self.active_panel = Panel::Playlists,
                        Panel::Playlists => break Ok(()),
                    },
                    KeyCode::Char('q') => break Ok(()),
                    _ => {}
                }
            }
        }
    }

    // ── playback helpers ──────────────────────────────────────────────────────

    fn play_song(&mut self, pl_idx: usize, song_idx: usize) {
        if let Some(track) = self.all_songs.get(pl_idx).and_then(|s| s.get(song_idx)) {
            let video_id = track["videoId"].as_str().unwrap_or("").to_string();
            if !video_id.is_empty() {
                self.audio.send(AudioCmd::Play(video_id));
                self.audio.send(AudioCmd::Volume(self.volume));
            }
            self.playing_pl   = Some(pl_idx);
            self.playing_song = Some(song_idx);
            self.list_state.select(Some(pl_idx));
            self.songs_state.select(Some(song_idx));
        }
    }

    fn play_next(&mut self) {
        let (Some(pl), Some(song)) = (self.playing_pl, self.playing_song) else { return };
        let n = self.all_songs.get(pl).map(Vec::len).unwrap_or(0);
        if n == 0 { return; }
        self.play_song(pl, (song + 1) % n);
    }

    fn play_prev(&mut self) {
        let (Some(pl), Some(song)) = (self.playing_pl, self.playing_song) else { return };
        let n = self.all_songs.get(pl).map(Vec::len).unwrap_or(0);
        if n == 0 { return; }
        self.play_song(pl, if song == 0 { n - 1 } else { song - 1 });
    }

    // ── layout ────────────────────────────────────────────────────────────────

    fn render(&mut self, frame: &mut Frame) {
        let vertical   = Layout::vertical([Constraint::Fill(1), Constraint::Length(4)]).spacing(1);
        let horizontal = Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)]).spacing(1);

        let [main, pg_bar]   = frame.area().layout(&vertical);
        let [playlists, right] = main.layout(&horizontal);

        self.render_playlists(frame, playlists);
        self.render_right_panel(frame, right);
        self.render_player(frame, pg_bar);
    }

    // ── playlists panel ───────────────────────────────────────────────────────

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<String> = self
            .playlists
            .iter()
            .map(|pl| pl["title"].as_str().unwrap_or("Untitled").to_string())
            .collect();

        let border_style = if self.active_panel == Panel::Playlists {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(Block::bordered().title("Playlists").border_style(border_style))
            .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    // ── right panel ───────────────────────────────────────────────────────────

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        let info_height = self.info_height(area.width);
        let [info_area, songs_area] =
            area.layout(&Layout::vertical([Constraint::Length(info_height), Constraint::Fill(1)]));

        if self.songs_state.selected().is_some() {
            let [pl_area, song_area] = info_area.layout(
                &Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]),
            );
            self.render_playlist_info(frame, pl_area);
            self.render_song_info(frame, song_area);
        } else {
            self.render_playlist_info(frame, info_area);
        }

        self.render_songs(frame, songs_area);
    }

    fn info_height(&self, area_width: u16) -> u16 {
        let rows = if self.songs_state.selected().is_some() {
            let half = (area_width / 2).saturating_sub(2);
            self.playlist_info_rows(half).max(self.song_info_rows(half))
        } else {
            self.playlist_info_rows(area_width.saturating_sub(2))
        };
        rows.min(4) + 2
    }

    fn playlist_info_rows(&self, width: u16) -> u16 {
        let Some(i) = self.list_state.selected() else { return 0 };
        let pl    = &self.playlists[i];
        let songs = self.all_songs.get(i).map(Vec::as_slice).unwrap_or(&[]);

        let mut rows = text_rows(pl["title"].as_str().unwrap_or("Untitled"), width);

        if let Some(author) = pl["author"].as_array().and_then(|arr| {
            let s = arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", ");
            if s.is_empty() { None } else { Some(s) }
        }) {
            rows += text_rows(&format!("by {author}"), width);
        }

        rows += text_rows(&format!("{} songs", songs.len()), width);

        let total_secs: u64 = songs.iter().filter_map(|t| t["duration_seconds"].as_u64()).sum();
        if total_secs > 0 {
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            rows += text_rows(&if h > 0 { format!("{h}h {m}min") } else { format!("{m}min") }, width);
        }
        rows
    }

    fn song_info_rows(&self, width: u16) -> u16 {
        let Some(track) = self.selected_song() else { return 0 };
        let title_rows = text_rows(track["title"].as_str().unwrap_or("Unknown"), width).min(2);
        let has_artists = track["artists"].as_array()
            .map(|arr| arr.iter().any(|a| a["name"].as_str().is_some()))
            .unwrap_or(false);
        let has_duration = track["duration"].as_str().is_some()
            || track["duration_seconds"].as_u64().is_some();
        title_rows + has_artists as u16 + has_duration as u16
    }

    fn selected_song(&self) -> Option<&Value> {
        let pl  = self.list_state.selected()?;
        let idx = self.songs_state.selected()?;
        self.all_songs.get(pl)?.get(idx)
    }

    fn render_playlist_info(&self, frame: &mut Frame, area: Rect) {
        let Some(i) = self.list_state.selected() else { return };
        let pl    = &self.playlists[i];
        let songs = self.all_songs.get(i).map(Vec::as_slice).unwrap_or(&[]);

        let title  = pl["title"].as_str().unwrap_or("Untitled");
        let author = pl["author"].as_array().and_then(|arr| {
            let s = arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", ");
            if s.is_empty() { None } else { Some(s) }
        });

        let total_secs: u64 = songs.iter().filter_map(|t| t["duration_seconds"].as_u64()).sum();
        let duration = (total_secs > 0).then(|| {
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            if h > 0 { format!("{h}h {m}min") } else { format!("{m}min") }
        });

        let mut lines = vec![Line::from(Span::from(title.to_string()).bold())];
        if let Some(a) = author       { lines.push(Line::from(format!("by {a}"))); }
        lines.push(Line::from(format!("{} songs", songs.len())));
        if let Some(d) = duration     { lines.push(Line::from(d)); }

        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title("Info"))
                .wrap(Wrap { trim: false })
                .alignment(Alignment::Center),
            area,
        );
    }

    fn render_song_info(&self, frame: &mut Frame, area: Rect) {
        let Some(track) = self.selected_song() else { return };
        let w = area.width.saturating_sub(2) as usize;

        let title       = track["title"].as_str().unwrap_or("Unknown");
        let title_lines = wrap_n_lines(title, w, 2);

        let artists = track["artists"].as_array().and_then(|arr| {
            let s = arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", ");
            if s.is_empty() { None } else { Some(s) }
        });
        let duration = track["duration"].as_str().map(str::to_string).or_else(|| {
            track["duration_seconds"].as_u64().map(|s| format!("{}:{:02}", s / 60, s % 60))
        });

        let mut lines: Vec<Line> = title_lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| if i == 0 { Line::from(Span::from(l).bold()) } else { Line::from(l) })
            .collect();
        if let Some(a) = artists  { lines.push(Line::from(truncate_line(&a, w))); }
        if let Some(d) = duration { lines.push(Line::from(truncate_line(&d, w))); }

        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title("Track"))
                .alignment(Alignment::Center),
            area,
        );
    }

    // ── songs list ────────────────────────────────────────────────────────────

    fn render_songs(&mut self, frame: &mut Frame, area: Rect) {
        let current_pl   = self.list_state.selected();
        let playing_pl   = self.playing_pl;
        let playing_song = self.playing_song;

        let songs = current_pl
            .and_then(|i| self.all_songs.get(i))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let num_w = songs.len().to_string().len();

        let items: Vec<ListItem> = songs
            .iter()
            .enumerate()
            .map(|(i, track)| {
                let is_playing = current_pl == playing_pl && Some(i) == playing_song;
                let title   = track["title"].as_str().unwrap_or("Unknown");
                let artists = track["artists"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();

                let indicator = if is_playing { "♫ " } else { "  " };
                let num       = format!("{:>num_w$}. ", i + 1);

                let mut spans = vec![
                    Span::styled(indicator, Style::default().fg(Color::Green)),
                    Span::styled(num, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        title,
                        if is_playing {
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
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
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().title("Songs").border_style(border_style))
                .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
                .highlight_symbol("> "),
            area,
            &mut self.songs_state,
        );
    }

    // ── player bar ────────────────────────────────────────────────────────────

    fn render_player(&self, frame: &mut Frame, area: Rect) {
        let ast = self.audio.state.lock().unwrap().clone();

        let title = if ast.loading {
            "⋯ Loading"
        } else if ast.total > 0.0 && !ast.paused {
            "▶ Player"
        } else if ast.total > 0.0 {
            "⏸ Player"
        } else {
            "Player"
        };

        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [gauge_area, info_area] =
            inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(1)]));

        // ── LineGauge (thin, unicode progress line) ────────────────────────
        let ratio = if ast.total > 0.0 {
            (ast.elapsed / ast.total).clamp(0.0, 1.0)
        } else {
            0.0
        };

        frame.render_widget(
            LineGauge::default()
                .ratio(ratio)
                .filled_symbol(symbols::line::THICK.horizontal)
                .unfilled_symbol(symbols::line::NORMAL.horizontal)
                .filled_style(Style::default().fg(Color::Cyan))
                .unfilled_style(Style::default().fg(Color::DarkGray)),
            gauge_area,
        );

        // ── Info line ──────────────────────────────────────────────────────
        let (left, elapsed_str, total_str) = self.player_info_text(&ast);

        let right = format!(
            "  {} / {}   ♪ {}%   [{}]",
            elapsed_str, total_str, self.volume, self.mode.label(),
        );
        let content_w = info_area.width as usize;
        let right_w   = right.chars().count();
        let left_w    = content_w.saturating_sub(right_w);
        let left_str  = truncate_line(&left, left_w);
        let pad       = left_w.saturating_sub(left_str.chars().count());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::from(left_str).bold(),
                Span::from(" ".repeat(pad)),
                Span::from(right),
            ])),
            info_area,
        );
    }

    fn player_info_text(&self, ast: &crate::audio::AudioState) -> (String, String, String) {
        let no_track = ("—  No track playing".to_string(), "0:00".to_string(), "0:00".to_string());

        let (Some(pl_idx), Some(song_idx)) = (self.playing_pl, self.playing_song) else {
            return no_track;
        };
        let Some(track) = self.all_songs.get(pl_idx).and_then(|s| s.get(song_idx)) else {
            return no_track;
        };

        let icon   = if ast.loading { "⋯" } else if ast.paused { "⏸" } else { "▶" };
        let title  = track["title"].as_str().unwrap_or("Unknown");
        let artist = track["artists"].as_array().and_then(|arr| {
            let s = arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", ");
            if s.is_empty() { None } else { Some(s) }
        });
        let left = match artist {
            Some(a) => format!("{icon}  {title}  —  {a}"),
            None    => format!("{icon}  {title}"),
        };
        (left, fmt_secs(ast.elapsed), fmt_secs(ast.total))
    }
}
