use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, KeyCode, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};
use serde_json::Value;

/// Returns the number of terminal rows `text` occupies when wrapped to `width` columns.
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

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Playlists,
    Songs,
}

pub struct App {
    playlists: Vec<Value>,
    list_state: ListState,
    all_songs: Vec<Vec<Value>>,
    songs_state: ListState,
    active_panel: Panel,
}

impl App {
    pub fn new(playlists: Vec<Value>, all_songs: Vec<Vec<Value>>) -> Self {
        let selected = (!playlists.is_empty()).then_some(0);
        Self {
            playlists,
            list_state: ListState::default().with_selected(selected),
            all_songs,
            songs_state: ListState::default(),
            active_panel: Panel::Playlists,
        }
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        ratatui::run(|term| self.event_loop(term))?;
        Ok(())
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            term.draw(|frame| self.render(frame))?;
            if let Some(key) = event::read()?.as_key_press_event() {
                log::debug!("key={:?} modifiers={:?}", key.code, key.modifiers);
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => match self.active_panel {
                        Panel::Playlists => {
                            self.list_state.select_next();
                            self.songs_state = ListState::default();
                        }
                        Panel::Songs => self.songs_state.select_next(),
                    },
                    KeyCode::Char('k') | KeyCode::Up => match self.active_panel {
                        Panel::Playlists => {
                            self.list_state.select_previous();
                            self.songs_state = ListState::default();
                        }
                        Panel::Songs => self.songs_state.select_previous(),
                    },
                    KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.active_panel = Panel::Playlists;
                    }
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.active_panel = Panel::Songs;
                    }
                    KeyCode::Esc => match self.active_panel {
                        Panel::Songs => self.active_panel = Panel::Playlists,
                        Panel::Playlists => break Ok(()),
                    },
                    KeyCode::Char('q') => break Ok(()),
                    _ => {}
                }
            }
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let vertical =
            Layout::vertical([Constraint::Percentage(90), Constraint::Fill(1)]).spacing(1);
        let horizontal =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)]).spacing(1);

        let [main, pg_bar] = frame.area().layout(&vertical);
        let [playlists, right] = main.layout(&horizontal);

        self.render_playlists(frame, playlists);
        self.render_right_panel(frame, right);
    }

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<String> = self
            .playlists
            .iter()
            .map(|pl| pl["title"].as_str().unwrap_or("Untitled").to_string())
            .collect();

        let border_style = if self.active_panel == Panel::Playlists {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        let list = List::new(items)
            .block(
                Block::bordered()
                    .title("Playlists")
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        let info_height = self.info_height(area.width);
        let vertical = Layout::vertical([Constraint::Length(info_height), Constraint::Fill(1)]);
        let [info_area, songs_area] = area.layout(&vertical);

        let info_split = Layout::horizontal([Constraint::Percentage(75), Constraint::Percentage(25)]);
        let [pl_info_area, song_info_area] = info_area.layout(&info_split);

        self.render_playlist_info(frame, pl_info_area);
        self.render_song_info(frame, song_info_area);
        self.render_songs(frame, songs_area);
    }

    fn info_height(&self, area_width: u16) -> u16 {
        // Approximate inner widths: panel% of total, minus 2 for the border columns
        let left_w  = (area_width * 75 / 100).saturating_sub(2);
        let right_w = (area_width * 25 / 100).saturating_sub(2);
        self.playlist_info_rows(left_w)
            .max(self.song_info_rows(right_w))
            + 2 // top + bottom border
    }

    fn playlist_info_rows(&self, width: u16) -> u16 {
        let Some(i) = self.list_state.selected() else { return 0 };
        let pl = &self.playlists[i];
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
            let d = if h > 0 { format!("{h}h {m}min") } else { format!("{m}min") };
            rows += text_rows(&d, width);
        }

        rows
    }

    fn song_info_rows(&self, width: u16) -> u16 {
        let Some(track) = self.selected_song() else { return 0 };

        let mut rows = text_rows(track["title"].as_str().unwrap_or("Unknown"), width);

        if let Some(artists) = track["artists"].as_array().and_then(|arr| {
            let s = arr.iter().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join(", ");
            if s.is_empty() { None } else { Some(s) }
        }) {
            rows += text_rows(&artists, width);
        }
        if let Some(album) = track["album"]["name"].as_str() {
            rows += text_rows(album, width);
        }
        let dur = track["duration"].as_str().map(str::to_string).or_else(|| {
            track["duration_seconds"]
                .as_u64()
                .map(|s| format!("{}:{:02}", s / 60, s % 60))
        });
        if let Some(d) = dur {
            rows += text_rows(&d, width);
        }
        let year = track["year"].as_str().map(str::to_string)
            .or_else(|| track["year"].as_u64().map(|y| y.to_string()));
        if let Some(y) = year {
            rows += text_rows(&y, width);
        }

        rows
    }

    fn selected_song(&self) -> Option<&Value> {
        let pl  = self.list_state.selected()?;
        let idx = self.songs_state.selected()?;
        self.all_songs.get(pl)?.get(idx)
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

        let total_secs: u64 = songs
            .iter()
            .filter_map(|t| t["duration_seconds"].as_u64())
            .sum();
        let duration = (total_secs > 0).then(|| {
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            if h > 0 { format!("{h}h {m}min") } else { format!("{m}min") }
        });

        let mut lines = vec![Line::from(Span::from(title.to_string()).bold())];
        if let Some(a) = author {
            lines.push(Line::from(format!("by {a}")));
        }
        lines.push(Line::from(format!("{} songs", songs.len())));
        if let Some(d) = duration {
            lines.push(Line::from(d));
        }

        let info = Paragraph::new(lines)
            .block(Block::bordered().title("Info"))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center);

        frame.render_widget(info, area);
    }

    fn render_song_info(&self, frame: &mut Frame, area: Rect) {
        let Some(track) = self.selected_song() else { return };

        let title = track["title"].as_str().unwrap_or("Unknown");

        let artists = track["artists"].as_array().and_then(|arr| {
            let s = arr
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if s.is_empty() { None } else { Some(s) }
        });

        let album = track["album"]["name"].as_str().map(str::to_string);

        let duration = track["duration"].as_str().map(str::to_string).or_else(|| {
            track["duration_seconds"].as_u64().map(|s| format!("{}:{:02}", s / 60, s % 60))
        });

        let year = track["year"].as_str().map(str::to_string)
            .or_else(|| track["year"].as_u64().map(|y| y.to_string()));

        let mut lines = vec![Line::from(Span::from(title.to_string()).bold())];
        if let Some(a) = artists  { lines.push(Line::from(a)); }
        if let Some(al) = album   { lines.push(Line::from(al)); }
        if let Some(d) = duration { lines.push(Line::from(d)); }
        if let Some(y) = year     { lines.push(Line::from(y)); }

        let info = Paragraph::new(lines)
            .block(Block::bordered().title("Track"))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center);

        frame.render_widget(info, area);
    }

    fn render_songs(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<String> = {
            let songs = self
                .list_state
                .selected()
                .and_then(|i| self.all_songs.get(i))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let width = songs.len().to_string().len();
            songs
                .iter()
                .enumerate()
                .map(|(i, track)| {
                    let title = track["title"].as_str().unwrap_or("Unknown");
                    let artists: String = track["artists"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|a| a["name"].as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    if artists.is_empty() {
                        format!("{:>width$}. {title}", i + 1)
                    } else {
                        format!("{:>width$}. {title}  —  {artists}", i + 1)
                    }
                })
                .collect()
        };

        let border_style = if self.active_panel == Panel::Songs {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        let list = List::new(items)
            .block(Block::bordered().title("Songs").border_style(border_style))
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.songs_state);
    }
}
