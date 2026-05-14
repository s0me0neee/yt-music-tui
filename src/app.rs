use google_youtube3::api::Playlist;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, KeyCode},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};

pub struct App {
    playlists: Vec<Playlist>,
    list_state: ListState,
}

impl App {
    pub fn new(playlists: Vec<Playlist>) -> Self {
        let selected = (!playlists.is_empty()).then_some(0);
        Self {
            playlists,
            list_state: ListState::default().with_selected(selected),
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
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => self.list_state.select_next(),
                    KeyCode::Char('k') | KeyCode::Up => self.list_state.select_previous(),
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
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
        let [playlists, info] = main.layout(&horizontal);

        self.render_playlists(frame, playlists);
        self.render_playlist_info(frame, info);
    }

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<String> = self
            .playlists
            .iter()
            .filter_map(|p| {
                let s = p.snippet.as_ref()?;
                let title = s.title.as_deref().unwrap_or("Untitled");
                let date = s
                    .published_at
                    .map(|d| d.format("%m/%d/%Y").to_string())
                    .unwrap_or_default();
                Some(format!("{title} - {date}"))
            })
            .collect();

        let list = List::new(items)
            .block(Block::bordered().title("Playlists"))
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_playlist_info(&mut self, frame: &mut Frame, area: Rect) {
        let info_layout = Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]);
        let [pl_info, songs] = area.layout(&info_layout);
        if let Some(i) = self.list_state.selected() {
            let selected_pl = self.playlists[i].clone();
            let snp = selected_pl.snippet.unwrap();
            let pl_title = snp.title.unwrap();
            let pl_desc = snp.description.unwrap();
            let pl_create_date = format!(
                "Created at: {}",
                snp.published_at.unwrap().format("%m/%d/%Y %H:%M")
            );
            let pl_id = selected_pl.id.unwrap();
            let info = Paragraph::new(vec![
                Line::from(Span::from(pl_title).bold()),
                Line::from(Span::from(pl_create_date)),
                Line::from(
                    Span::from(pl_desc).style(Style::default().add_modifier(Modifier::ITALIC)),
                ),
            ])
            .block(Block::bordered().title("Info"))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center);
            frame.render_widget(info, pl_info);
        }
    }
}
