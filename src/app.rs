use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, KeyCode, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};
use serde_json::Value;

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
            songs_state: ListState::default().with_selected(Some(0)),
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
                            self.songs_state = ListState::default().with_selected(Some(0));
                        }
                        Panel::Songs => self.songs_state.select_next(),
                    },
                    KeyCode::Char('k') | KeyCode::Up => match self.active_panel {
                        Panel::Playlists => {
                            self.list_state.select_previous();
                            self.songs_state = ListState::default().with_selected(Some(0));
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
            Layout::vertical([Constraint::Percentage(8), Constraint::Fill(1)]).spacing(1);
        let horizontal =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)]).spacing(1);

        let [_top, main] = frame.area().layout(&vertical);
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
            .block(Block::bordered().title("Playlists").border_style(border_style))
            .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        let layout = Layout::vertical([Constraint::Length(6), Constraint::Fill(1)]);
        let [info_area, songs_area] = area.layout(&layout);
        self.render_playlist_info(frame, info_area);
        self.render_songs(frame, songs_area);
    }

    fn render_playlist_info(&self, frame: &mut Frame, area: Rect) {
        let Some(i) = self.list_state.selected() else { return };
        let pl = &self.playlists[i];

        let title = pl["title"].as_str().unwrap_or("Untitled");
        let count = pl.get("count").map(|v| {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else if let Some(n) = v.as_u64() {
                format!("{n} songs")
            } else {
                String::new()
            }
        }).unwrap_or_default();
        let desc = pl["description"].as_str().unwrap_or("");

        let info = Paragraph::new(vec![
            Line::from(Span::from(title.to_string()).bold()),
            Line::from(Span::from(count)),
            Line::from(
                Span::from(desc.to_string()).style(Style::default().add_modifier(Modifier::ITALIC)),
            ),
        ])
        .block(Block::bordered().title("Info"))
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
                        format!("{}. {title}", i + 1)
                    } else {
                        format!("{}. {title}  —  {artists}", i + 1)
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
            .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.songs_state);
    }
}
