use std::time::{Duration, Instant};

use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{
        self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, HighlightSpacing, LineGauge, Padding, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState,
    },
};
use throbber_widgets_tui::{Throbber, ThrobberState};

use ytm_core::library::SongBatch;
use ytm_core::lyrics::{self, LyricsMsg, LyricsQuery, LyricsService, TrackLyrics};
use ytm_core::persistence::{self, LyricsOverrides, QueueState, RestoreOutcome};
use ytm_core::{AppendOutcome, AudioState, Library, Player, RemoveOutcome, Track};

// ── theme ────────────────────────────────────────────────────────────────────

/// Semantic styles for the whole UI.
///
/// Every value is an **ANSI named colour**, never `Rgb` or `Indexed`, so the
/// user's own terminal palette keeps driving how the app looks. Each role below
/// owns its colour: previously Cyan alone meant focus, key-caps, progress,
/// modal chrome and "synced lyrics" all at once, which made none of them
/// readable as a signal.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    /// Focused section header. The only accent-coloured header on screen.
    pub const HEADER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    /// Unfocused section header.
    pub const HEADER_BLUR: Style = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    /// The rule under a section header.
    pub const RULE: Style = Style::new().fg(Color::DarkGray);

    /// Selected row in the focused panel.
    pub const SELECTED: Style = Style::new()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    /// Selected row in an unfocused panel — kept visible so you don't lose your
    /// place, but clearly not where keys will land.
    pub const SELECTED_BLUR: Style = Style::new().add_modifier(Modifier::BOLD);

    /// The track currently playing.
    pub const PLAYING: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    /// Primary content text (track titles, playlist names).
    pub const PRIMARY: Style = Style::new().add_modifier(Modifier::BOLD);
    /// Secondary content (artists, albums).
    pub const META: Style = Style::new().fg(Color::Gray);
    /// Chrome: separators, counts, durations, hints.
    pub const DIM: Style = Style::new().fg(Color::DarkGray);

    /// A key the user can press.
    pub const KEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    /// Progress fill and other "live" accents.
    pub const ACCENT: Style = Style::new().fg(Color::Cyan);

    /// Something needs attention but still works (mute, filter, no synced lyrics).
    pub const WARN: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    /// Something failed. Red is used for nothing else, and error *bodies* are
    /// no longer DarkGray — the dimmest style in the palette was carrying the
    /// most important text.
    pub const ERROR: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    /// Body text under an ERROR or WARN headline.
    pub const ERROR_BODY: Style = Style::new().fg(Color::Red);
    /// A completed action, in the notification bar.
    pub const SUCCESS: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
}

/// Separator between inline items, e.g. `a  ·  b`.
const SEP: &str = "  ·  ";

/// How long a notification toast stays in the status bar.
const NOTIFICATION_TTL: Duration = Duration::from_secs(2);

// ── helpers ──────────────────────────────────────────────────────────────────

/// Display width in terminal cells. CJK titles and emoji are two cells wide, so
/// `chars().count()` would under-measure them and over-run the column.
fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Draws a section header and its underline, returning the rect left for
/// content.
///
/// This replaces bordered panels: a bold label, an optional right-aligned
/// status, and a rule spanning the full width. Focus is carried by the label's
/// colour, since there is no border left to carry it.
fn section(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    status: Option<Line<'static>>,
    focused: bool,
) -> Rect {
    if area.height == 0 || area.width == 0 {
        return area;
    }

    let head_style = if focused {
        theme::HEADER
    } else {
        theme::HEADER_BLUR
    };
    let avail = area.width as usize;
    let label = truncate_line(&label.to_uppercase(), avail);
    let label_w = width_of(&label);
    let mut spans = vec![Span::styled(label, head_style)];

    // The status sits to the right of the label, dropped entirely rather than
    // wrapped if the terminal is too narrow for both.
    if let Some(status) = status {
        let status_w: usize = status.spans.iter().map(|s| width_of(&s.content)).sum();
        if label_w + SEP.len() + status_w <= avail {
            spans.push(Span::styled(SEP, theme::DIM));
            spans.extend(status.spans);
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );

    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                symbols::line::NORMAL.horizontal.repeat(area.width as usize),
                theme::RULE,
            )),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
    }

    Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    }
}

/// Shrinks a list's rect to leave room for a scrollbar, but only when the list
/// actually overflows. Without a border to hang it on, the bar needs a column
/// of its own or it would paint over the rightmost content.
fn list_body(area: Rect, total: usize) -> Rect {
    if total > area.height as usize {
        Rect {
            width: area.width.saturating_sub(2),
            ..area
        }
    } else {
        area
    }
}

/// Draws a scrollbar in the last column — only when the content overflows.
/// Previously these appeared for any list with more than one item, so a
/// 3-song playlist in a 30-row panel still showed a full-height bar.
fn render_scrollbar(frame: &mut Frame, area: Rect, total: usize, selected: Option<usize>) {
    if total <= area.height as usize || area.width == 0 {
        return;
    }
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(theme::META)
            .track_style(theme::RULE),
        area,
        &mut ScrollbarState::new(total).position(selected.unwrap_or(0)),
    );
}

/// Renders a vertically-and-horizontally centred message — the shared shape for
/// every empty, loading and error state.
fn centered_message(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let pad = (area.height as usize).saturating_sub(lines.len()) / 2;
    let mut out: Vec<Line> = vec![Line::from(""); pad];
    out.extend(lines);
    frame.render_widget(Paragraph::new(out).alignment(Alignment::Center), area);
}

/// A `key` + `description` hint pair, as shown in the help bar.
fn hint(key: &str, desc: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_string(), theme::KEY),
        Span::styled(format!(" {desc}"), theme::DIM),
    ]
}

/// Lays out as many hints as fit in `width`, dropping whole hints from the end
/// rather than letting the line be clipped mid-word.
///
/// The full hint list needs ~143 columns; this is what keeps an 80-column
/// terminal readable. `?` opens the complete keymap.
fn fit_hints(items: &[(&str, &str)], width: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;

    for (i, (key, desc)) in items.iter().enumerate() {
        let sep = if i > 0 { SEP.len() } else { 0 };
        let cost = sep + width_of(key) + 1 + width_of(desc);
        if used + cost > width {
            break;
        }
        if i > 0 {
            spans.push(Span::styled(SEP, theme::DIM));
        }
        spans.extend(hint(key, desc));
        used += cost;
    }
    spans
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

/// Truncates to `max` display cells, appending `…`. Measured in cells rather
/// than chars so wide (CJK, emoji) titles don't over-run their column.
fn truncate_line(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width_of(text) <= max {
        return text.to_string();
    }
    // Leave one cell for the ellipsis.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// `m:ss`, or `h:mm:ss` past an hour — the old form printed a 70-minute track
/// as `70:11` and clipped anything over 100 minutes.
fn fmt_secs(secs: f64) -> String {
    fmt_duration(secs.max(0.0) as u64)
}

fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn track_duration(track: &Track) -> Option<String> {
    track
        .duration
        .clone()
        .or_else(|| track.duration_seconds.map(|s| fmt_duration(u64::from(s))))
}

// ── panel ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Playlists,
    Songs,
}

// ── lyrics ────────────────────────────────────────────────────────────────────

/// The currently-playing lyric line. Green matches what the songs list already
/// uses for "now playing", and it is the only row in the panel with either a
/// hue or a background — the rest of the panel stays achromatic so this lands
/// immediately.
const ACTIVE_LYRIC: Style = Style::new()
    .bg(Color::Green)
    .fg(Color::Black)
    .add_modifier(Modifier::BOLD);

/// Builds exactly `height` display lines with the active lyric on the centre
/// row, `scroll` rows away from it.
///
/// Rows outside the lyric range become blanks rather than the view clamping to
/// the ends — that is what keeps the active line dead-centre for the whole
/// song, first and last lines included.
fn synced_view(
    rows: &[LyricRow],
    active: Option<usize>,
    height: u16,
    scroll: i32,
) -> Vec<Line<'static>> {
    // First display row of the active lyric — a long lyric wraps across several
    // rows and highlights as a unit.
    let focus = active
        .and_then(|a| rows.iter().position(|r| r.lyric == a))
        .unwrap_or(0) as i32;
    let height = i32::from(height);
    let top = focus - (height - 1) / 2 + scroll;

    (0..height)
        .map(|i| {
            let idx = top + i;
            if idx < 0 || idx as usize >= rows.len() {
                return Line::from("");
            }
            let row = &rows[idx as usize];
            let Some(active) = active else {
                // Intro: nothing is playing yet, so nothing is emphasised.
                return Line::styled(row.text.clone(), theme::DIM).centered();
            };

            if row.lyric == active {
                let text = if row.text.is_empty() {
                    "♪ ♪ ♪".to_string()
                } else {
                    row.text.clone()
                };
                // Padded by a space either side so the highlight reads as a
                // marker pen over the words rather than clinging to the glyphs.
                return Line::styled(format!(" {text} "), ACTIVE_LYRIC).centered();
            }

            let style = if row.lyric.abs_diff(active) == 1 {
                theme::META
            } else {
                theme::DIM
            };
            Line::styled(row.text.clone(), style).centered()
        })
        .collect()
}

/// Per-track lyrics state. Every variant except `Loading` is terminal: a cached
/// entry is never re-fetched, so toggling lyrics mode or skipping away and back
/// costs nothing. `Failed` is deliberately sticky too — a dead network must not
/// be retried once per tick. `r` evicts the entry to retry explicitly.
enum LyricsEntry {
    Loading,
    Ready(Box<TrackLyrics>),
    /// lrclib has no record for this track.
    Missing,
    Failed(String),
}

/// One display row: wrapped text plus the lyric line it came from, so a long
/// lyric spanning several rows highlights as a unit.
struct LyricRow {
    lyric: usize,
    text: String,
}

/// The `c` variant picker.
struct LyricsPicker {
    /// The track these candidates were fetched for; results for anything else
    /// are stale and dropped.
    video_id: String,
    items: Vec<TrackLyrics>,
    /// The record the panel was showing when the picker opened. It is
    /// guaranteed a row, so the list can mark what is already in use.
    on_screen: Option<u64>,
    /// Whether that record came from a manual choice rather than the automatic
    /// match — which of the two the "Automatic" row is ticked against.
    overridden: bool,
    state: TableState,
    loading: bool,
    error: Option<String>,
}

/// The picker's rows: the pinned "Automatic" entry, then one per candidate.
///
/// Which row is in use gets its own column rather than a badge at the end of
/// the line. The name/artist/album line is free to overflow and be clipped,
/// which is exactly where a trailing marker would end up — and the point of
/// the marker is to stop you re-picking what is already playing, so it has to
/// be readable without reaching the end of the row.
fn picker_rows(
    items: &[TrackLyrics],
    current_id: Option<u64>,
    overridden: bool,
    track_secs: Option<f64>,
    name_w: usize,
) -> Vec<Row<'static>> {
    let badge = |text: &'static str, style: Style| Cell::from(Line::styled(text, style));

    // Row 0 is pinned: the only way back to automatic matching after a
    // choice has been made, and what's in use until one is.
    let mut rows = vec![Row::new(vec![
        badge(if overridden { "" } else { "IN USE" }, theme::PLAYING),
        Cell::from(Line::styled("Automatic (best match)", theme::KEY)),
        Cell::from(""),
    ])];

    rows.extend(items.iter().map(|c| {
        let (marker, marker_style) = match c.kind {
            ytm_core::LyricsKind::Synced(_) => ("♪ ", theme::ACCENT),
            ytm_core::LyricsKind::Plain(_) => ("¶ ", theme::WARN),
            ytm_core::LyricsKind::Instrumental => ("· ", theme::DIM),
        };

        let mut spans = vec![
            Span::styled(marker, marker_style),
            Span::styled(truncate_line(&c.track_name, name_w), theme::PRIMARY),
        ];
        if !c.artist_name.is_empty() {
            spans.push(Span::styled(SEP, theme::DIM));
            spans.push(Span::styled(c.artist_name.clone(), theme::META));
        }
        if !c.album_name.is_empty() {
            spans.push(Span::styled(SEP, theme::DIM));
            spans.push(Span::styled(c.album_name.clone(), theme::DIM));
        }
        // Green when the length matches the track — a one-glance cue that
        // this is the right edit. Yellow when the gap is why the record
        // lost its timings, so the trade-off is visible before choosing.
        let close = c.duration_delta(track_secs).is_some_and(|d| d <= 2.0);
        let dur_style = if close {
            theme::PLAYING
        } else if c.timing_mismatch {
            theme::WARN
        } else {
            theme::DIM
        };

        // Two different facts, so two different words. On a manual choice this
        // row *is* the choice; on automatic it is what the matcher resolved to
        // — worth showing, since otherwise there is no way to tell which
        // record "Automatic" means, but not the same as having picked it.
        let (label, style) = match (Some(c.id) == current_id, overridden) {
            (false, _) => ("", theme::DIM),
            (true, true) => ("IN USE", theme::PLAYING),
            (true, false) => ("AUTO", theme::ACCENT),
        };

        Row::new(vec![
            badge(label, style),
            Cell::from(Line::from(spans)),
            Cell::from(
                Line::styled(
                    c.duration.map_or_else(|| "—".to_string(), fmt_secs),
                    dur_style,
                )
                .right_aligned(),
            ),
        ])
    }));

    rows
}

/// Row to start the picker on: whatever is already in use, so re-picking it
/// takes a deliberate keypress. Row 0 is the pinned "Automatic" entry, so the
/// candidates are offset by one.
fn initial_picker_row(items: &[TrackLyrics], on_screen: Option<u64>, overridden: bool) -> usize {
    if !overridden {
        return 0;
    }
    on_screen
        .and_then(|id| items.iter().position(|c| c.id == id))
        .map_or(0, |i| i + 1)
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
    show_keymap: bool,
    queue_view_state: TableState,
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
    // lyrics
    lyrics_mode: bool,
    lyrics_handle: tokio::runtime::Handle,
    lyrics_svc: std::sync::Arc<LyricsService>,
    lyrics_tx: std::sync::mpsc::Sender<LyricsMsg>,
    lyrics_rx: std::sync::mpsc::Receiver<LyricsMsg>,
    lyrics_cache: std::collections::HashMap<String, LyricsEntry>,
    /// Wrapped rows cached per `(video_id, width)` so we re-wrap only when the
    /// track or the panel width actually changes.
    lyrics_rows: Option<(String, u16, Vec<LyricRow>)>,
    /// Manual offset from the auto-centred position, in display rows.
    lyrics_scroll: i32,
    /// Cleared once the user scrolls away; `Esc` re-centres. Always false for
    /// plain lyrics, which have no position to follow.
    lyrics_following: bool,
    lyrics_picker: Option<LyricsPicker>,
    lyrics_overrides: LyricsOverrides,
    lyrics_dirty: bool,
    /// When we started waiting for mpv to report the playing track's real
    /// duration, so the wait can't become a permanent block if it never does.
    lyrics_duration_wait: Option<(String, Instant)>,
    /// User settings from `config.toml`, read once at startup.
    config: ytm_core::Config,
}

impl App {
    pub fn new(
        library: Library,
        saved_queue: Option<QueueState>,
        songs_rx: std::sync::mpsc::Receiver<SongBatch>,
        rt: tokio::runtime::Handle,
        config: ytm_core::Config,
    ) -> Self {
        let n = library.len();
        let selected = (n > 0).then_some(0);

        // Restore the volume saved on the previous exit.
        let mut player = Player::new();
        player.set_volume(persistence::load_settings().volume);

        let (lyrics_tx, lyrics_rx) = std::sync::mpsc::channel();

        Self {
            library,
            list_state: {
                let mut s = TableState::default();
                s.select(selected);
                s
            },
            songs_state: TableState::default(),
            active_panel: Panel::Playlists,
            player,
            throbber_state: ThrobberState::default(),
            show_queue: false,
            show_keymap: false,
            queue_view_state: TableState::default(),
            notification: None,
            reauth_requested: false,
            songs_rx,
            pending_queue_restore: saved_queue,
            filter: String::new(),
            filter_mode: false,
            playlists_area: Rect::default(),
            songs_area: Rect::default(),
            lyrics_mode: false,
            lyrics_handle: rt,
            lyrics_svc: std::sync::Arc::new(LyricsService::new()),
            lyrics_tx,
            lyrics_rx,
            lyrics_cache: std::collections::HashMap::new(),
            lyrics_rows: None,
            lyrics_scroll: 0,
            lyrics_following: true,
            lyrics_picker: None,
            lyrics_overrides: persistence::load_lyrics_overrides(),
            lyrics_duration_wait: None,
            config,
            lyrics_dirty: false,
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

    // ── lyrics ────────────────────────────────────────────────────────────────

    /// The video ID of the track currently playing, if any.
    fn current_video_id(&self) -> Option<String> {
        let (pl, song) = self.player.playing()?;
        self.library.track(pl, song)?.video_id.clone()
    }

    /// How long to wait for mpv's duration before falling back to YouTube's.
    /// Long enough to cover a yt-dlp resolve, short enough that a track which
    /// never plays still gets its lyrics looked up.
    const DURATION_WAIT: Duration = Duration::from_secs(4);

    /// Starts a lyrics fetch for `video_id` unless one is already cached or in
    /// flight. The single `Occupied` arm is what makes repeated `y` toggles and
    /// skip-away-and-back free.
    fn ensure_lyrics(&mut self, video_id: &str) {
        use std::collections::hash_map::Entry;

        if matches!(
            self.lyrics_cache.entry(video_id.to_string()),
            Entry::Occupied(_)
        ) {
            return;
        }
        let Some((pl, song)) = self.player.playing() else {
            return;
        };
        let Some(mut query) = self
            .library
            .track(pl, song)
            .and_then(LyricsQuery::from_track)
        else {
            return;
        };

        // Rank against the real audio length rather than YouTube's, which
        // rounds *up* — measured across this library it runs 0 to 1.0s long,
        // 0.54s on average. Matching lrclib records against the inflated
        // figure favours the ones whose own duration is inflated too, and
        // rejects the accurate ones: of the pairs the user corrected by hand,
        // theirs was the closer record 7 times to 1 against the true length,
        // and only 2 to 6 against YouTube's.
        //
        // mpv reports it a moment after the file loads, so this waits a tick
        // or two — invisible next to the second the lookup itself takes, and
        // bounded so a track that never starts still gets its lyrics.
        let total = self.player.audio_state().total;
        if total <= 0.0 {
            let waited = match &self.lyrics_duration_wait {
                Some((id, since)) if id == video_id => since.elapsed(),
                _ => {
                    self.lyrics_duration_wait = Some((video_id.to_string(), Instant::now()));
                    Duration::ZERO
                }
            };
            if waited < Self::DURATION_WAIT {
                return; // not cached, not in flight — we retry next tick
            }
            log::debug!("lyrics: no duration from mpv after {waited:?} — using YouTube's");
        } else {
            query.duration = Some(total);
        }

        self.lyrics_cache
            .insert(video_id.to_string(), LyricsEntry::Loading);
        log::info!("lyrics: fetching for {video_id} ({})", query.title);
        lyrics::spawn_best(
            &self.lyrics_handle,
            std::sync::Arc::clone(&self.lyrics_svc),
            video_id.to_string(),
            query,
            self.lyrics_overrides.get(video_id),
            self.lyrics_tx.clone(),
        );
    }

    /// Drain completed lyrics fetches. Results are keyed by video ID, so they
    /// are always safe to store; only the on-screen track resets view state.
    fn drain_lyrics(&mut self) {
        while let Ok(msg) = self.lyrics_rx.try_recv() {
            match msg {
                LyricsMsg::Best { video_id, result } => {
                    let entry = match result {
                        Ok(Some(found)) => {
                            log::info!("lyrics: got #{} for {video_id}", found.id);
                            LyricsEntry::Ready(Box::new(found))
                        }
                        Ok(None) => {
                            log::info!("lyrics: none found for {video_id}");
                            LyricsEntry::Missing
                        }
                        Err(e) => {
                            log::warn!("lyrics: fetch failed for {video_id}: {e}");
                            LyricsEntry::Failed(e)
                        }
                    };
                    self.lyrics_cache.insert(video_id.clone(), entry);
                    if self.current_video_id().as_deref() == Some(video_id.as_str()) {
                        self.reset_lyrics_view();
                    }
                }
                LyricsMsg::Choices { video_id, result } => {
                    // A picker opened for a different track has moved on.
                    if let Some(picker) = self.lyrics_picker.as_mut()
                        && picker.video_id == video_id
                    {
                        picker.loading = false;
                        match result {
                            Ok(items) => {
                                let start =
                                    initial_picker_row(&items, picker.on_screen, picker.overridden);
                                picker.state.select(Some(start));
                                picker.items = items;
                            }
                            Err(e) => picker.error = Some(e),
                        }
                    }
                }
            }
        }
    }

    /// How long to block waiting for input.
    ///
    /// Normally 200 ms, but while synced lyrics are following playback we wake
    /// just after the next line boundary instead, so the highlight flips on
    /// time. Costs nothing when lyrics mode is off.
    fn poll_timeout(&self) -> Duration {
        const IDLE: Duration = Duration::from_millis(200);

        if !self.lyrics_mode || !self.lyrics_following {
            return IDLE;
        }
        let state = self.player.audio_state();
        if state.paused || state.loading {
            return IDLE;
        }
        let Some(lines) = self.current_lyrics().and_then(TrackLyrics::synced_lines) else {
            return IDLE;
        };
        // Against the shifted clock, so the wake-up lands on the boundary the
        // highlight will actually flip at rather than the record's raw one.
        match lyrics::next_boundary(lines, self.config.lyrics.lyric_time(state.elapsed)) {
            // The +20 ms absorbs `elapsed` staleness (mpv's time-pos observer),
            // so we don't wake early and busy-spin; the 33 ms floor bounds a
            // densely-timed record to ~30 redraws/sec worst case.
            Some(dt) => Duration::from_secs_f64(dt + 0.020).clamp(Duration::from_millis(33), IDLE),
            None => IDLE,
        }
    }

    fn reset_lyrics_view(&mut self) {
        self.lyrics_rows = None;
        self.lyrics_scroll = 0;
        self.lyrics_following = true;
    }

    fn toggle_lyrics_mode(&mut self) {
        self.lyrics_mode = !self.lyrics_mode;
        self.lyrics_picker = None;
        self.reset_lyrics_view();
    }

    fn scroll_lyrics(&mut self, delta: i32) {
        self.lyrics_scroll += delta;
        self.lyrics_following = false;
    }

    /// Lyrics for the on-screen track, if they've arrived.
    fn current_lyrics(&self) -> Option<&TrackLyrics> {
        let id = self.current_video_id()?;
        match self.lyrics_cache.get(&id)? {
            LyricsEntry::Ready(found) => Some(found),
            _ => None,
        }
    }

    /// Drops the cached entry so the next tick re-fetches — the escape hatch
    /// from a sticky `Failed`/`Missing`.
    fn retry_lyrics(&mut self) {
        if let Some(id) = self.current_video_id() {
            self.lyrics_cache.remove(&id);
            self.reset_lyrics_view();
            self.notify("Retrying lyrics…");
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

        if let Err(e) = persistence::save_settings(&persistence::Settings {
            volume: self.player.effective_volume(),
        }) {
            log::warn!("failed to save settings: {e}");
        }

        if self.lyrics_dirty
            && let Err(e) = persistence::save_lyrics_overrides(&self.lyrics_overrides)
        {
            log::warn!("failed to save lyrics overrides: {e}");
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
            self.drain_lyrics();
            // Kicked off from here rather than on each key: this one lookup
            // covers entering lyrics mode, p/n, auto-advance and Enter alike.
            if self.lyrics_mode
                && let Some(id) = self.current_video_id()
            {
                self.ensure_lyrics(&id);
            }
            self.throbber_state.calc_next();
            // Expire the toast here rather than inside the render pass, which
            // had `render_help` mutating state while drawing.
            if self
                .notification
                .as_ref()
                .is_some_and(|(_, t)| t.elapsed() >= NOTIFICATION_TTL)
            {
                self.notification = None;
            }
            term.draw(|frame| self.render(frame))?;
            if self.player.handle_song_end(&self.library) {
                self.sync_queue_view();
            }
            if event::poll(self.poll_timeout())? {
                match event::read()? {
                    Event::Mouse(me) => self.handle_mouse(me),
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        log::debug!("key={:?}", key.code);
                        match key.code {
                            // Raw mode clears ISIG, so Ctrl+C never becomes a
                            // signal — it arrives here as a plain key. Without
                            // this guard the `c` binding below would swallow it.
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break Ok(());
                            }

                            // ── the keymap overlay swallows the next key ──────────────
                            _ if self.show_keymap => self.show_keymap = false,

                            // ── the picker owns all input while it is open ────────────
                            _ if self.lyrics_picker.is_some() => {
                                if self.handle_picker_key(key.code) {
                                    break Ok(());
                                }
                            }

                            // ── filter mode intercepts all input ──────────────────────
                            _ if self.filter_mode => self.handle_filter_key(key.code),

                            // ── navigation ────────────────────────────────────────────
                            KeyCode::Char('j') => match self.active_panel {
                                Panel::Playlists => {
                                    self.list_state.select_next();
                                    self.songs_state = TableState::default();
                                    self.clear_filter();
                                }
                                // The songs list is hidden behind the lyrics, so
                                // scroll those rather than a cursor nobody sees.
                                Panel::Songs if self.lyrics_mode => self.scroll_lyrics(1),
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
                                Panel::Songs if self.lyrics_mode => self.scroll_lyrics(-1),
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
                            KeyCode::Char('?') => self.show_keymap = true,
                            // ── lyrics ────────────────────────────────────────────────
                            KeyCode::Char('y') => self.toggle_lyrics_mode(),
                            KeyCode::Char('c') if self.lyrics_mode => self.open_lyrics_picker(),
                            KeyCode::Char('r') if self.lyrics_mode => self.retry_lyrics(),
                            KeyCode::PageDown if self.lyrics_mode => self.scroll_lyrics(5),
                            KeyCode::PageUp if self.lyrics_mode => self.scroll_lyrics(-5),
                            // ── quit ──────────────────────────────────────────────────
                            // In lyrics mode Esc first re-centres, then closes the
                            // panel, before falling back to the usual behaviour.
                            KeyCode::Esc if self.lyrics_mode && !self.lyrics_following => {
                                self.reset_lyrics_view();
                            }
                            KeyCode::Esc if self.lyrics_mode => self.lyrics_mode = false,
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
                    if self.lyrics_mode {
                        self.scroll_lyrics(1);
                    } else if self.show_queue {
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
                    if self.lyrics_mode {
                        self.scroll_lyrics(-1);
                    } else if self.show_queue {
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

    // ── lyrics picker ─────────────────────────────────────────────────────────

    /// Opens the variant picker for the playing track, fetching candidates in
    /// the background.
    fn open_lyrics_picker(&mut self) {
        let Some(video_id) = self.current_video_id() else {
            self.notify("No track playing");
            return;
        };
        let Some((pl, song)) = self.player.playing() else {
            return;
        };
        let Some(query) = self
            .library
            .track(pl, song)
            .and_then(LyricsQuery::from_track)
        else {
            self.notify("Track has no title to search on");
            return;
        };

        let on_screen = self.current_lyrics().map(|l| l.id);
        let overridden = self.lyrics_overrides.get(&video_id).is_some();

        self.lyrics_picker = Some(LyricsPicker {
            video_id: video_id.clone(),
            items: Vec::new(),
            on_screen,
            overridden,
            state: TableState::default(),
            loading: true,
            error: None,
        });
        lyrics::spawn_choices(
            &self.lyrics_handle,
            std::sync::Arc::clone(&self.lyrics_svc),
            video_id,
            query,
            on_screen,
            self.lyrics_tx.clone(),
        );
    }

    /// Handles a key while the picker is open. Returns `true` if the app should
    /// quit. Playback keys are forwarded so the music stays controllable.
    fn handle_picker_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc | KeyCode::Char('c') => self.lyrics_picker = None,
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(p) = self.lyrics_picker.as_mut() {
                    p.state.select_next();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(p) = self.lyrics_picker.as_mut() {
                    p.state.select_previous();
                }
            }
            KeyCode::Enter => self.commit_lyrics_choice(),
            // Keep playback usable without closing the modal.
            KeyCode::Char(' ') => {
                if self.player.playing().is_some() && self.player.playback_started() {
                    self.player.toggle_pause();
                }
            }
            KeyCode::Left => self.player.seek(-5),
            KeyCode::Right => self.player.seek(5),
            _ => {}
        }
        false
    }

    /// Applies the highlighted candidate. Row 0 is the pinned "Automatic" entry,
    /// which clears any override; everything below is offset by one.
    ///
    /// Committing needs no network — search already returned full records.
    fn commit_lyrics_choice(&mut self) {
        let Some(picker) = self.lyrics_picker.as_ref() else {
            return;
        };
        let Some(row) = picker.state.selected() else {
            return;
        };
        let video_id = picker.video_id.clone();

        if row == 0 {
            self.lyrics_overrides.clear(&video_id);
            self.lyrics_dirty = true;
            self.lyrics_cache.remove(&video_id);
            self.notify("Lyrics: automatic match");
        } else {
            let Some(chosen) = picker.items.get(row - 1).cloned() else {
                return;
            };
            self.lyrics_overrides.set(&video_id, chosen.id);
            self.lyrics_dirty = true;
            self.lyrics_cache
                .insert(video_id, LyricsEntry::Ready(Box::new(chosen)));
            self.notify("Lyrics source updated");
        }

        self.lyrics_picker = None;
        self.reset_lyrics_view();
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
                let artists = track
                    .map(|t| t.artist_names().to_lowercase())
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

    // ── help / notification bar ───────────────────────────────────────────────

    /// The hints for the current context, most useful first — `fit_hints` drops
    /// from the end when the terminal is too narrow, so the order is the
    /// priority order.
    fn hints(&self) -> &'static [(&'static str, &'static str)] {
        if self.lyrics_picker.is_some() {
            &[
                ("j/k", "select"),
                ("↵", "use"),
                ("Esc", "cancel"),
                ("spc", "pause"),
            ]
        } else if self.lyrics_mode {
            &[
                ("y", "close"),
                ("c", "source"),
                ("spc", "pause"),
                ("p/n", "skip"),
                ("j/k", "scroll"),
                ("?", "keys"),
            ]
        } else {
            match (self.active_panel, self.show_queue) {
                (Panel::Playlists, _) => &[
                    ("j/k", "nav"),
                    ("l/↵", "open"),
                    ("spc", "pause"),
                    ("?", "keys"),
                    ("q", "quit"),
                ],
                (Panel::Songs, false) => &[
                    ("↵", "play"),
                    ("spc", "pause"),
                    ("/", "filter"),
                    ("a", "+queue"),
                    ("o", "queue"),
                    ("y", "lyrics"),
                    ("p/n", "skip"),
                    ("?", "keys"),
                ],
                (Panel::Songs, true) => &[
                    ("↵", "play"),
                    ("spc", "pause"),
                    ("d", "remove"),
                    ("o", "songs"),
                    ("y", "lyrics"),
                    ("p/n", "skip"),
                    ("?", "keys"),
                ],
            }
        }
    }

    fn render_help(&mut self, frame: &mut Frame, area: Rect) {
        let line = if self.filter_mode {
            let mut spans = vec![
                Span::styled(format!("/{}", self.filter), theme::WARN),
                Span::styled("█", theme::WARN),
            ];
            spans.extend(fit_hints(
                &[("↵", "confirm"), ("Esc", "cancel")],
                area.width as usize - width_of(&self.filter) - 2,
            ));
            Line::from(spans)
        } else if let Some((msg, _)) = &self.notification {
            Line::from(vec![
                Span::styled("✓ ", theme::SUCCESS),
                Span::styled(msg.clone(), theme::SUCCESS),
            ])
        } else {
            Line::from(fit_hints(self.hints(), area.width as usize))
        };

        frame.render_widget(Paragraph::new(line), area);
    }

    /// Full keymap overlay, opened with `?`. The one-line help bar can only
    /// carry a handful of hints; everything lives here.
    fn render_keymap(&self, frame: &mut Frame, screen: Rect) {
        const KEYS: &[(&str, &str)] = &[
            ("j / k", "Move down / up"),
            ("h / l", "Switch panel"),
            ("↵", "Open playlist · play song"),
            ("/", "Filter by title or artist"),
            ("Esc", "Clear filter · back · close"),
            ("", ""),
            ("space", "Pause / resume"),
            ("p / n", "Previous / next in queue"),
            ("← / →", "Seek ∓5s"),
            ("↑ / ↓", "Volume ±5"),
            ("m", "Mute / unmute"),
            ("t", "Cycle play mode"),
            ("", ""),
            ("a", "Add selected song to queue"),
            ("d", "Remove selected queue entry"),
            ("o", "Toggle queue / songs"),
            ("", ""),
            ("y", "Toggle lyrics"),
            ("c", "Choose lyrics source (in lyrics)"),
            ("r", "Retry lyrics (in lyrics)"),
            ("", ""),
            ("?", "Close this help"),
            ("q  ·  Ctrl+C", "Quit"),
        ];

        let width = 46u16.min(screen.width.saturating_sub(4));
        let height = (KEYS.len() as u16 + 4).min(screen.height.saturating_sub(2));
        let area = screen.centered(Constraint::Length(width), Constraint::Length(height));

        frame.render_widget(Clear, area);
        // Overlays keep a border: they float above other content, so they need
        // an edge to separate them from it. Panels in the main layout don't.
        let block = Block::bordered()
            .title(Line::styled(" Keys ", theme::HEADER))
            .border_style(theme::RULE)
            .padding(Padding::symmetric(2, 1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let key_w = KEYS.iter().map(|(k, _)| width_of(k)).max().unwrap_or(0);
        let lines: Vec<Line> = KEYS
            .iter()
            .map(|(key, desc)| {
                if key.is_empty() {
                    return Line::from("");
                }
                Line::from(vec![
                    Span::styled(format!("{key:>key_w$}"), theme::KEY),
                    Span::styled("   ", theme::DIM),
                    Span::styled((*desc).to_string(), theme::DIM),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // ── layout ────────────────────────────────────────────────────────────────

    #[hotpath::measure]
    fn render(&mut self, frame: &mut Frame) {
        // One column of margin all round, so content never touches the terminal
        // edge now that there are no borders holding it off.
        let screen = frame.area();
        let body = Rect {
            x: screen.x + 1,
            width: screen.width.saturating_sub(2),
            ..screen
        };

        // Bottom block: a blank spacer, then the two player rows, then hints.
        // The spacer is what separates the player from the lists — previously
        // that job was done by two stacked borders drawing a double rule.
        let [main, bottom] = body.layout(&Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(4),
        ]));
        let [_gap, now_playing, progress, help_bar] = bottom.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]));

        // Wider gutter than the old 1 column: with no borders between them, the
        // columns need real space to read as separate.
        let [playlists, right] = main.layout(
            &Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)]).spacing(3),
        );

        self.playlists_area = playlists;
        self.songs_area = right;

        self.render_playlists(frame, playlists);
        self.render_right_panel(frame, right);
        self.render_player(frame, now_playing, progress);
        self.render_help(frame, help_bar);

        // Overlays last so they sit above everything.
        if self.show_keymap {
            self.render_keymap(frame, screen);
        }
    }

    // ── playlists panel ───────────────────────────────────────────────────────

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.active_panel == Panel::Playlists;
        let count = self.library.len();
        let status = (count > 0).then(|| Line::styled(count.to_string(), theme::DIM));
        let body = section(frame, area, "Playlists", status, focused);

        if self.library.is_empty() {
            centered_message(
                frame,
                body,
                vec![
                    Line::styled("No playlists found", theme::WARN),
                    Line::from(""),
                    Line::styled("Your session may have expired.", theme::DIM),
                    Line::from(""),
                    Line::from(
                        [hint("r", "re-authenticate"), hint("q", "quit")]
                            .join(&Span::styled(SEP, theme::DIM)),
                    ),
                ],
            );
            return;
        }

        let rows: Vec<Row> = self
            .library
            .entries()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let playing = self.player.playing().is_some_and(|(pl, _)| pl == i);
                Row::new([
                    Cell::from(Span::styled(
                        entry.playlist.title.clone(),
                        if playing {
                            theme::PLAYING
                        } else {
                            theme::PRIMARY
                        },
                    )),
                    Cell::from(
                        Line::styled(self.library.songs(i).len().to_string(), theme::DIM)
                            .alignment(Alignment::Right),
                    ),
                ])
            })
            .collect();

        let n = rows.len();
        frame.render_stateful_widget(
            Table::new(rows, [Constraint::Fill(1), Constraint::Length(4)])
                .row_highlight_style(if focused {
                    theme::SELECTED
                } else {
                    theme::SELECTED_BLUR
                })
                .highlight_symbol("▸ ")
                // Always reserve the cursor gutter, so rows don't jump sideways
                // the first time a selection appears.
                .highlight_spacing(HighlightSpacing::Always)
                .column_spacing(1),
            list_body(body, n),
            &mut self.list_state,
        );

        render_scrollbar(frame, body, n, self.list_state.selected());
    }

    // ── right panel ───────────────────────────────────────────────────────────

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        // Lyrics take over the whole right column — Info, Track and the song
        // list all give way to it.
        if self.lyrics_mode {
            self.render_lyrics(frame, area);
            if self.lyrics_picker.is_some() {
                self.render_lyrics_picker(frame, area);
            }
            return;
        }

        if self.show_queue {
            self.render_queue(frame, area);
        } else {
            self.render_songs(frame, area);
        }
    }

    /// The right-hand status shown beside a section header: the live filter
    /// query if one is set, otherwise the position within the list.
    fn list_status(&self, shown: usize, total: usize) -> Option<Line<'static>> {
        if !self.filter.is_empty() {
            let mut spans = vec![
                Span::styled("/", theme::WARN),
                Span::styled(self.filter.clone(), theme::WARN),
            ];
            if self.filter_mode {
                spans.push(Span::styled("█", theme::WARN));
            }
            spans.push(Span::styled(format!("  {shown}/{total}"), theme::DIM));
            return Some(Line::from(spans));
        }
        (total > 0).then(|| Line::styled(format!("{total}"), theme::DIM))
    }

    /// One row of a track list — shared by Songs and Queue so the two views are
    /// visually identical and don't shift when `o` toggles between them.
    fn track_row(
        &self,
        track: Option<&Track>,
        number: usize,
        num_w: usize,
        playing: bool,
    ) -> Row<'static> {
        let title = track
            .and_then(|t| t.title.as_deref())
            .unwrap_or("Unknown")
            .to_string();
        let artists = track.map(Track::artist_names).unwrap_or_default();

        let mut spans = vec![
            Span::styled(if playing { "♫ " } else { "  " }, theme::PLAYING),
            Span::styled(format!("{number:>num_w$}  "), theme::DIM),
            Span::styled(
                title,
                if playing {
                    theme::PLAYING
                } else {
                    theme::PRIMARY
                },
            ),
        ];
        if !artists.is_empty() {
            spans.push(Span::styled(SEP, theme::DIM));
            spans.push(Span::styled(artists, theme::META));
        }

        Row::new([
            Cell::from(Line::from(spans)),
            Cell::from(
                Line::styled(
                    track.and_then(track_duration).unwrap_or_default(),
                    theme::DIM,
                )
                .alignment(Alignment::Right),
            ),
        ])
    }

    /// Column widths shared by both track lists. `8` fits `1:02:03`.
    const TRACK_COLS: [Constraint; 2] = [Constraint::Fill(1), Constraint::Length(8)];

    fn track_table(rows: Vec<Row<'static>>, focused: bool) -> Table<'static> {
        Table::new(rows, Self::TRACK_COLS)
            .row_highlight_style(if focused {
                theme::SELECTED
            } else {
                theme::SELECTED_BLUR
            })
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always)
            .column_spacing(1)
    }

    // ── songs list ────────────────────────────────────────────────────────────

    // ── lyrics panel ──────────────────────────────────────────────────────────

    fn render_lyrics(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.active_panel == Panel::Songs;

        let Some(video_id) = self.current_video_id() else {
            let body = section(frame, area, "Lyrics", None, focused);
            centered_message(
                frame,
                body,
                vec![Line::styled("Nothing playing", theme::DIM)],
            );
            return;
        };

        match self.lyrics_cache.get(&video_id) {
            None | Some(LyricsEntry::Loading) => {
                let body = section(frame, area, "Lyrics", None, focused);
                frame.render_stateful_widget(
                    Throbber::default()
                        .label(" Searching lrclib…")
                        .throbber_style(theme::ACCENT),
                    body,
                    &mut self.throbber_state,
                );
            }
            Some(LyricsEntry::Missing) => {
                let body = section(frame, area, "Lyrics", None, focused);
                centered_message(
                    frame,
                    body,
                    vec![
                        Line::styled("No lyrics found", theme::WARN),
                        Line::from(""),
                        Line::from(
                            [hint("c", "search lrclib"), hint("r", "retry")]
                                .join(&Span::styled(SEP, theme::DIM)),
                        ),
                    ],
                );
            }
            Some(LyricsEntry::Failed(err)) => {
                let msg = truncate_line(err, area.width.saturating_sub(2) as usize);
                let body = section(frame, area, "Lyrics", None, focused);
                centered_message(
                    frame,
                    body,
                    vec![
                        Line::styled("Lyrics unavailable", theme::ERROR),
                        Line::from(""),
                        Line::styled(msg, theme::ERROR_BODY),
                        Line::from(""),
                        Line::from(hint("r", "retry")),
                    ],
                );
            }
            Some(LyricsEntry::Ready(found)) => match &found.kind {
                ytm_core::LyricsKind::Instrumental => {
                    let status = Self::lyrics_status(found, None, None);
                    let body = section(frame, area, "Lyrics", Some(status), focused);
                    centered_message(
                        frame,
                        body,
                        vec![
                            Line::styled("♪", theme::ACCENT),
                            Line::from(""),
                            Line::styled("instrumental", theme::DIM),
                        ],
                    );
                }
                ytm_core::LyricsKind::Synced(_) => self.render_synced(frame, area, &video_id),
                ytm_core::LyricsKind::Plain(_) => self.render_plain(frame, area, &video_id),
            },
        }
    }

    /// Right-hand status for the lyrics header: which lrclib record is in use
    /// and what it matched — the cue to press `c` when the match is wrong.
    /// `offset` is shown only when it is non-zero, so a shift that is silently
    /// in effect can't be mistaken for a badly-timed record.
    fn lyrics_status(
        found: &TrackLyrics,
        badge: Option<Span<'static>>,
        offset: Option<String>,
    ) -> Line<'static> {
        let mut spans = Vec::new();
        if let Some(badge) = badge {
            spans.push(badge);
            spans.push(Span::styled(SEP, theme::DIM));
        }
        if let Some(offset) = offset {
            spans.push(Span::styled(format!("offset {offset}"), theme::WARN));
            spans.push(Span::styled(SEP, theme::DIM));
        }
        spans.push(Span::styled(
            format!("{} — {}", found.track_name, found.artist_name),
            theme::DIM,
        ));
        spans.push(Span::styled(
            format!("{SEP}lrclib #{}", found.id),
            theme::DIM,
        ));
        Line::from(spans)
    }

    /// Re-wraps the lyric text if the track or panel width changed. Returns the
    /// total row count.
    fn ensure_lyric_rows(&mut self, video_id: &str, width: u16) -> usize {
        let stale = match &self.lyrics_rows {
            Some((id, w, _)) => id != video_id || *w != width,
            None => true,
        };
        if stale {
            let texts: Vec<String> = match self.lyrics_cache.get(video_id) {
                Some(LyricsEntry::Ready(found)) => match &found.kind {
                    ytm_core::LyricsKind::Synced(lines) => {
                        lines.iter().map(|l| l.text.clone()).collect()
                    }
                    ytm_core::LyricsKind::Plain(lines) => lines.clone(),
                    ytm_core::LyricsKind::Instrumental => Vec::new(),
                },
                _ => Vec::new(),
            };

            let mut rows = Vec::new();
            for (i, text) in texts.iter().enumerate() {
                if text.trim().is_empty() {
                    // Keep interludes as a row of their own so synced playback
                    // has something to sit on during instrumental gaps.
                    rows.push(LyricRow {
                        lyric: i,
                        text: String::new(),
                    });
                    continue;
                }
                for piece in wrap_n_lines(text, width as usize, usize::MAX) {
                    rows.push(LyricRow {
                        lyric: i,
                        text: piece,
                    });
                }
            }
            self.lyrics_rows = Some((video_id.to_string(), width, rows));
        }
        self.lyrics_rows.as_ref().map_or(0, |(_, _, r)| r.len())
    }

    fn render_synced(&mut self, frame: &mut Frame, area: Rect, video_id: &str) {
        // Take the header as an owned value so the cache borrow ends before the
        // mutable re-wrap below — that lets the lyric lines stay borrowed
        // rather than cloned on every frame.
        let status = {
            let Some(LyricsEntry::Ready(found)) = self.lyrics_cache.get(video_id) else {
                return;
            };
            Self::lyrics_status(
                found,
                Some(Span::styled("♪ synced", theme::ACCENT)),
                self.config.lyrics.offset_label(),
            )
        };

        let focused = self.active_panel == Panel::Songs;
        let body = section(frame, area, "Lyrics", Some(status), focused);
        // Breathing room either side, since there is no border holding the
        // centred text off the neighbouring column.
        let inner = Rect {
            x: body.x + 2,
            width: body.width.saturating_sub(4),
            ..body
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let elapsed = self
            .config
            .lyrics
            .lyric_time(self.player.audio_state().elapsed);
        // Two columns short of the full width: the active line is padded by one
        // space either side for its highlight, and wrapping to the same width
        // for every row keeps that padding from clipping the longest lines.
        self.ensure_lyric_rows(video_id, inner.width.saturating_sub(2).max(1));

        let Some(LyricsEntry::Ready(found)) = self.lyrics_cache.get(video_id) else {
            return;
        };
        let active = lyrics::active_index(found.synced_lines().unwrap_or(&[]), elapsed);
        let Some((_, _, rows)) = self.lyrics_rows.as_ref() else {
            return;
        };

        let out = synced_view(rows, active, inner.height, self.lyrics_scroll);

        frame.render_widget(Paragraph::new(out), inner);
    }

    fn render_plain(&mut self, frame: &mut Frame, area: Rect, video_id: &str) {
        let Some(LyricsEntry::Ready(found)) = self.lyrics_cache.get(video_id) else {
            return;
        };
        // "no timing available" belongs in the header, not consuming a content
        // row and scrolling away with the text as it used to.
        // Distinguish "lrclib has no timed version" from "the timed version is
        // for a different-length recording" — the second is worth a nudge to
        // press `c`, the first isn't.
        let badge = if found.timing_mismatch {
            Span::styled("¶ timing differs", theme::WARN)
        } else {
            Span::styled("¶ unsynced", theme::WARN)
        };
        let status = Self::lyrics_status(found, Some(badge), None);

        let focused = self.active_panel == Panel::Songs;
        let body = section(frame, area, "Lyrics", Some(status), focused);
        let inner = Rect {
            x: body.x + 2,
            width: body.width.saturating_sub(4),
            ..body
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let total = self.ensure_lyric_rows(video_id, inner.width);
        let Some((_, _, rows)) = self.lyrics_rows.as_ref() else {
            return;
        };

        // No timing to follow, so this scrolls from the top under manual control.
        let height = inner.height as usize;
        let top = (self.lyrics_scroll.max(0) as usize).min(total.saturating_sub(height));

        let out: Vec<Line> = rows
            .iter()
            .skip(top)
            .take(height)
            // Left-aligned: unsynced lyrics are prose-shaped, and centred prose
            // reads badly.
            .map(|r| Line::styled(r.text.clone(), theme::META))
            .collect();
        frame.render_widget(Paragraph::new(out), inner);

        render_scrollbar(frame, body, total, Some(top));
    }

    /// The `c` variant picker: a modal centred over the lyrics panel, leaving
    /// the playlists column and player bar visible.
    fn render_lyrics_picker(&mut self, frame: &mut Frame, area: Rect) {
        // Which record is in use, so the active one can be ticked.
        let current_id = self.current_lyrics().map(|l| l.id);
        let track_secs = self
            .player
            .playing()
            .and_then(|(pl, s)| self.library.track(pl, s))
            .and_then(|t| t.duration_seconds)
            .map(f64::from);

        let Some(picker) = self.lyrics_picker.as_mut() else {
            return;
        };

        let modal = area.centered(
            Constraint::Length(area.width.saturating_sub(4).clamp(40, 78)),
            Constraint::Length(area.height.saturating_sub(2).clamp(7, 20)),
        );

        // Overlays keep a border — they float above other content and need an
        // edge to sit against. The main layout's panels don't.
        let block = Block::bordered()
            .title(Line::styled(" Choose lyrics ", theme::HEADER))
            .title_bottom(Line::from(fit_hints(
                &[("j/k", "select"), ("↵", "use"), ("Esc", "cancel")],
                modal.width.saturating_sub(4) as usize,
            )))
            .border_style(theme::RULE)
            .padding(Padding::horizontal(1));

        // Required: ratatui composites into one buffer, so without this the
        // lyrics underneath bleed through the modal.
        frame.render_widget(Clear, modal);

        if picker.loading {
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            frame.render_stateful_widget(
                Throbber::default()
                    .label(" Searching lrclib…")
                    .throbber_style(theme::ACCENT),
                inner,
                &mut self.throbber_state,
            );
            return;
        }

        if let Some(err) = &picker.error {
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("Search failed", theme::ERROR),
                    Line::from(""),
                    Line::styled(truncate_line(err, inner.width as usize), theme::ERROR_BODY),
                ])
                .alignment(Alignment::Center),
                inner,
            );
            return;
        }

        let rows = picker_rows(
            &picker.items,
            current_id,
            picker.overridden,
            track_secs,
            modal.width.saturating_sub(20) as usize,
        );

        let count = rows.len();
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(6),
                    Constraint::Fill(1),
                    Constraint::Length(8),
                ],
            )
            .block(block)
            .row_highlight_style(theme::SELECTED)
            .highlight_symbol("▶ ")
            .column_spacing(1),
            modal,
            &mut picker.state,
        );

        if count > 1 {
            let pos = picker.state.selected().unwrap_or(0);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                modal,
                &mut ScrollbarState::new(count).position(pos),
            );
        }
    }

    fn render_songs(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.active_panel == Panel::Songs;
        let current_pl = self.list_state.selected();

        // The playlist's own name and stats become this section's header —
        // the old bordered "Info" and "Track" boxes duplicated the list below
        // and cost five rows to say it.
        let entry = current_pl.and_then(|i| self.library.entry(i));
        let label = entry
            .map_or("Songs", |e| e.playlist.title.as_str())
            .to_string();

        let all_songs = current_pl.map_or(&[][..], |i| self.library.songs(i));
        let filtered = current_pl
            .map(|pl| self.filtered_songs(pl))
            .unwrap_or_default();

        let status = if self.filter.is_empty() {
            entry.map(|e| {
                let secs = e.total_duration_secs;
                let mut s = format!("{} songs", e.songs.len());
                if secs > 0 {
                    let (h, m) = (secs / 3600, (secs % 3600) / 60);
                    s.push_str(&if h > 0 {
                        format!("{SEP}{h}h {m}min")
                    } else {
                        format!("{SEP}{m}min")
                    });
                }
                Line::styled(s, theme::DIM)
            })
        } else {
            self.list_status(filtered.len(), all_songs.len())
        };

        let body = section(frame, area, &label, status, focused);

        if current_pl.is_some_and(|i| !self.library.is_loaded(i)) {
            frame.render_stateful_widget(
                Throbber::default()
                    .label(" Loading…")
                    .throbber_style(theme::ACCENT),
                body,
                &mut self.throbber_state,
            );
            return;
        }

        if filtered.is_empty() {
            // Previously both of these rendered an empty box with no explanation.
            let msg = if all_songs.is_empty() {
                vec![Line::styled("This playlist is empty", theme::DIM)]
            } else {
                vec![
                    Line::styled(format!("Nothing matches /{}", self.filter), theme::WARN),
                    Line::from(""),
                    Line::from(hint("Esc", "clear filter")),
                ]
            };
            centered_message(frame, body, msg);
            return;
        }

        let playing = self.player.playing();
        let num_w = all_songs.len().to_string().len();
        let rows: Vec<Row> = filtered
            .iter()
            .map(|&i| {
                self.track_row(
                    Some(&all_songs[i]),
                    i + 1,
                    num_w,
                    current_pl.map(|pl| (pl, i)) == playing,
                )
            })
            .collect();

        let n = rows.len();
        frame.render_stateful_widget(
            Self::track_table(rows, focused),
            list_body(body, n),
            &mut self.songs_state,
        );
        render_scrollbar(frame, body, n, self.songs_state.selected());
    }

    // ── queue view ────────────────────────────────────────────────────────────

    fn render_queue(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.active_panel == Panel::Songs;
        let queue_pos = self.player.queue_position();
        let queue = self.player.queue().to_vec();
        let filtered = self.filtered_queue_positions();

        let status = if self.filter.is_empty() {
            let pos = queue_pos.map_or(0, |p| p + 1);
            Some(Line::from(vec![
                Span::styled(format!("{pos}/{}", queue.len()), theme::DIM),
                Span::styled(SEP, theme::DIM),
                // Via PlayMode::label() so the queue header and the player row
                // can never disagree about the mode.
                Span::styled(self.player.mode().label().to_string(), theme::DIM),
            ]))
        } else {
            self.list_status(filtered.len(), queue.len())
        };

        let body = section(frame, area, "Queue", status, focused);

        if filtered.is_empty() {
            let msg = if queue.is_empty() {
                vec![
                    Line::styled("The queue is empty", theme::DIM),
                    Line::from(""),
                    Line::from(hint("a", "add the selected song")),
                ]
            } else {
                vec![
                    Line::styled(format!("Nothing matches /{}", self.filter), theme::WARN),
                    Line::from(""),
                    Line::from(hint("Esc", "clear filter")),
                ]
            };
            centered_message(frame, body, msg);
            return;
        }

        let num_w = queue.len().to_string().len();
        let rows: Vec<Row> = filtered
            .iter()
            .map(|&q_pos| {
                let (pl, song_idx) = queue[q_pos];
                self.track_row(
                    self.library.track(pl, song_idx),
                    q_pos + 1,
                    num_w,
                    Some(q_pos) == queue_pos,
                )
            })
            .collect();

        let n = rows.len();
        frame.render_stateful_widget(
            Self::track_table(rows, focused),
            list_body(body, n),
            &mut self.queue_view_state,
        );
        render_scrollbar(frame, body, n, self.queue_view_state.selected());
    }

    // ── player bar ────────────────────────────────────────────────────────────

    /// The player occupies two borderless rows: what is playing, then how far
    /// through it we are. Title first because that is what you look for.
    fn render_player(&mut self, frame: &mut Frame, now_playing: Rect, progress: Rect) {
        let ast = self.player.audio_state();
        let (title_text, artist_text, elapsed_str, total_str) = self.player_track_info(&ast);

        // ── row 1: [state] title · artist ............ mode · volume ────────
        let status = {
            let volume = self.player.volume();
            let muted = self.player.is_muted();
            vec![
                Span::styled(self.player.mode().label().to_string(), theme::DIM),
                Span::styled(SEP, theme::DIM),
                if muted {
                    Span::styled("muted", theme::WARN)
                } else {
                    Span::styled(format!("{volume}%"), theme::DIM)
                },
            ]
        };
        let status_w: usize = status.iter().map(|s| width_of(&s.content)).sum();

        // Right-aligned by splitting the rect, not by padding with spaces —
        // the old `" ".repeat(pad)` collapsed to no gap on narrow terminals.
        let [left, right] = now_playing.layout(&Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(status_w as u16),
        ]));

        let left_line = if let Some(err) = &ast.error {
            // Previously this only swapped the block title to "Error" and the
            // message itself was never shown anywhere.
            Line::from(vec![
                Span::styled("✕ ", theme::ERROR),
                Span::styled(
                    truncate_line(err, left.width.saturating_sub(2) as usize),
                    theme::ERROR_BODY,
                ),
            ])
        } else {
            let icon = if ast.loading {
                "⋯ "
            } else if ast.paused {
                "⏸ "
            } else if ast.total > 0.0 {
                "♫ "
            } else {
                "  "
            };
            let mut spans = vec![Span::styled(
                icon,
                if ast.paused {
                    theme::DIM
                } else {
                    theme::PLAYING
                },
            )];
            let budget = left.width.saturating_sub(2) as usize;
            spans.push(Span::styled(
                truncate_line(&title_text, budget),
                if ast.paused {
                    theme::DIM
                } else {
                    theme::PRIMARY
                },
            ));
            if let Some(artist) = &artist_text {
                let used = width_of(&title_text).min(budget);
                let rest = budget.saturating_sub(used + SEP.len());
                if rest > 3 {
                    spans.push(Span::styled(SEP, theme::DIM));
                    spans.push(Span::styled(truncate_line(artist, rest), theme::META));
                }
            }
            Line::from(spans)
        };

        frame.render_widget(Paragraph::new(left_line), left);
        frame.render_widget(Paragraph::new(Line::from(status)), right);

        // ── row 2: elapsed ──────────── bar ──────────── total ──────────────
        let time_w = elapsed_str.len().max(total_str.len()).max(4) as u16;
        let [elapsed_area, bar_area, total_area] = progress.layout(&Layout::horizontal([
            Constraint::Length(time_w),
            Constraint::Fill(1),
            Constraint::Length(time_w),
        ]));

        frame.render_widget(
            Paragraph::new(Span::styled(elapsed_str, theme::DIM)),
            elapsed_area,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(total_str, theme::DIM)).alignment(Alignment::Right),
            total_area,
        );

        let ratio = if ast.total > 0.0 {
            (ast.elapsed / ast.total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        frame.render_widget(
            LineGauge::default()
                .ratio(ratio)
                // Without an explicit empty label, LineGauge prints its default
                // "{:3.0}%" into the start of the bar area in the default style.
                .label("")
                .filled_symbol(symbols::line::THICK.horizontal)
                .unfilled_symbol(symbols::line::NORMAL.horizontal)
                .filled_style(if ast.paused {
                    theme::DIM
                } else {
                    theme::ACCENT
                })
                .unfilled_style(theme::RULE),
            // Inset by one so the bar doesn't butt against the timestamps.
            Rect {
                x: bar_area.x + 1,
                width: bar_area.width.saturating_sub(2),
                ..bar_area
            },
        );
    }

    fn player_track_info(&self, ast: &AudioState) -> (String, Option<String>, String, String) {
        let nothing = || {
            (
                "Nothing playing".to_string(),
                None,
                "0:00".to_string(),
                "0:00".to_string(),
            )
        };
        let Some((pl_idx, song_idx)) = self.player.playing() else {
            return nothing();
        };
        let Some(track) = self.library.track(pl_idx, song_idx) else {
            return nothing();
        };
        let title = track.title.clone().unwrap_or_else(|| "Unknown".to_string());
        let artist = {
            let s = track.artist_names();
            (!s.is_empty()).then_some(s)
        };
        (title, artist, fmt_secs(ast.elapsed), fmt_secs(ast.total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<LyricRow> {
        (0..n)
            .map(|i| LyricRow {
                lyric: i,
                text: format!("line{i}"),
            })
            .collect()
    }

    /// The plain text of each rendered line, with the highlight padding stripped.
    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect()
    }

    /// `Line::styled` stores the style on the line, not on its spans.
    fn styles(lines: &[Line<'static>]) -> Vec<Style> {
        lines.iter().map(|l| l.style).collect()
    }

    // ── layout & typography primitives ────────────────────────────────────

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders `f` into a `w`x`h` terminal and returns the rows as plain text.
    fn draw(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|frame| f(frame)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Lyric lines at the given timestamps.
    fn timed_lines(at: &[f64]) -> Vec<ytm_core::lyrics::LyricLine> {
        at.iter()
            .map(|at| ytm_core::lyrics::LyricLine {
                at: *at,
                text: "line".into(),
            })
            .collect()
    }

    // ── lyrics offset ─────────────────────────────────────────────────────

    #[test]
    fn the_offset_moves_which_line_is_active() {
        let lines = timed_lines(&[10.0, 20.0, 30.0]);
        let at = |offset: f64, elapsed: f64| {
            let cfg = ytm_core::config::Lyrics { offset };
            lyrics::active_index(&lines, cfg.lyric_time(elapsed))
        };

        // Unshifted: line b starts exactly at 20s.
        assert_eq!(at(0.0, 19.5), Some(0));
        assert_eq!(at(0.0, 20.0), Some(1));

        // Early (negative): b already shows half a second before it is sung,
        // and a full second before it with -1.0.
        assert_eq!(at(-0.5, 19.5), Some(1));
        assert_eq!(at(-1.0, 19.0), Some(1));
        assert_eq!(at(-1.0, 18.9), Some(0), "but not before its shifted time");

        // Late (positive): b is held back past 20s.
        assert_eq!(at(0.5, 20.0), Some(0));
        assert_eq!(at(0.5, 20.5), Some(1));

        // Shifted into the intro, nothing is active — as before the first line.
        assert_eq!(at(5.0, 12.0), None);
    }

    #[test]
    fn the_offset_moves_the_redraw_boundary_with_it() {
        // The wake-up has to land on the boundary the highlight flips at, not
        // the record's raw one, or every line changes late by the offset.
        let lines = timed_lines(&[10.0, 20.0]);
        let wait = |offset: f64, elapsed: f64| {
            let cfg = ytm_core::config::Lyrics { offset };
            lyrics::next_boundary(&lines, cfg.lyric_time(elapsed))
        };

        assert_eq!(wait(0.0, 15.0), Some(5.0));
        // Showing lines a second early means waking a second sooner.
        assert_eq!(wait(-1.0, 15.0), Some(4.0));
        assert_eq!(wait(1.0, 15.0), Some(6.0));
    }

    // ── lyrics picker ─────────────────────────────────────────────────────

    fn candidate(id: u64, track: &str, album: &str) -> TrackLyrics {
        TrackLyrics {
            id,
            track_name: track.into(),
            artist_name: "Lia".into(),
            album_name: album.into(),
            duration: Some(245.0),
            timing_mismatch: false,
            relevance: id as usize,
            kind: ytm_core::LyricsKind::Plain(vec!["x".into()]),
        }
    }

    /// Renders the picker's rows the way `render_lyrics_picker` does.
    fn draw_picker(w: u16, items: &[TrackLyrics], current: Option<u64>, over: bool) -> Vec<String> {
        let rows = picker_rows(items, current, over, Some(245.0), 30);
        draw(w, (rows.len() + 2) as u16, |frame| {
            frame.render_widget(
                Table::new(
                    rows,
                    [
                        Constraint::Length(6),
                        Constraint::Fill(1),
                        Constraint::Length(8),
                    ],
                )
                .column_spacing(1),
                frame.area(),
            );
        })
    }

    #[test]
    fn the_picker_marks_the_record_in_use() {
        let items = [
            candidate(1, "Song", "Album One"),
            candidate(2, "Song", "Album Two"),
        ];

        // A manual choice: the badge sits on that record, not on "Automatic".
        let out = draw_picker(60, &items, Some(2), true);
        assert!(
            !out[0].contains("IN USE"),
            "automatic is not in use: {out:?}"
        );
        assert!(!out[1].contains("IN USE"));
        assert!(out[2].contains("IN USE"), "row for #2 unmarked: {out:?}");

        // No override: "Automatic" is what's in use, and the record it
        // resolved to is marked so you can see which one that is.
        let out = draw_picker(60, &items, Some(1), false);
        assert!(out[0].contains("IN USE"), "{out:?}");
        assert_eq!(
            out.iter().filter(|r| r.contains("IN USE")).count(),
            1,
            "only one row can be in use: {out:?}"
        );
        assert!(
            out[1].contains("AUTO"),
            "automatic's record unmarked: {out:?}"
        );
        assert!(!out[2].contains("AUTO"));
    }

    #[test]
    #[ignore = "visual smoke check — prints the picker, asserts nothing"]
    fn render_picker() {
        let items = [
            candidate(1, "\u{9ce5}\u{306e}\u{8a69}", "AIR ORIGINAL SOUNDTRACK"),
            candidate(2, "\u{9ce5}\u{306e}\u{8a69}", "Key BEST SELECTION"),
            candidate(
                3,
                "\u{9ce5}\u{306e}\u{8a69} (TV size)",
                "KeyBOX -for two decades-",
            ),
        ];
        for (label, current, over) in [
            ("automatic", Some(2), false),
            ("manual choice", Some(3), true),
        ] {
            println!("\n--- {label} ---");
            for row in draw_picker(64, &items, current, over) {
                println!("|{row}");
            }
        }
    }

    #[test]
    fn the_in_use_badge_survives_a_narrow_modal() {
        // The badge led the row precisely so a long name can't push it out of
        // view. 40 columns is the narrowest the modal goes.
        let items = [candidate(
            1,
            "A Very Long Track Name That Runs Past The Edge",
            "And A Long Album Name Too",
        )];
        let out = draw_picker(40, &items, Some(1), true);
        assert!(out[1].starts_with("IN USE"), "{out:?}");
    }

    #[test]
    fn the_picker_opens_on_whatever_is_in_use() {
        let items = [candidate(1, "Song", "One"), candidate(2, "Song", "Two")];

        // Row 0 is "Automatic", so candidates are offset by one.
        assert_eq!(initial_picker_row(&items, Some(2), true), 2);
        assert_eq!(initial_picker_row(&items, Some(1), true), 1);
        // No override means automatic is in use, whatever is on screen.
        assert_eq!(initial_picker_row(&items, Some(2), false), 0);
        // An override the list doesn't contain falls back to the pinned row.
        assert_eq!(initial_picker_row(&items, Some(99), true), 0);
        assert_eq!(initial_picker_row(&[], Some(1), true), 0);
    }

    /// Renders a representative screen so the layout can be eyeballed:
    /// `cargo test -p yt-music-tui -- --ignored --nocapture render_screen`
    #[test]
    #[ignore = "visual smoke check — prints a screen, asserts nothing"]
    fn render_screen() {
        for (w, h) in [(120u16, 24u16), (80, 20), (46, 12)] {
            let out = draw(w, h, |frame| {
                let screen = frame.area();
                let body = Rect {
                    x: screen.x + 1,
                    width: screen.width.saturating_sub(2),
                    ..screen
                };
                let [main, bottom] = body.layout(&Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(4),
                ]));
                let [_gap, np, prog, help] = bottom.layout(&Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]));
                let [left, right] = main.layout(
                    &Layout::horizontal([Constraint::Percentage(25), Constraint::Fill(1)])
                        .spacing(3),
                );

                let pl_body = section(
                    frame,
                    left,
                    "Playlists",
                    Some(Line::styled("3", theme::DIM)),
                    false,
                );
                let mut st = TableState::default();
                st.select(Some(0));
                frame.render_stateful_widget(
                    Table::new(
                        [("My Mix", 42), ("Chill", 17), ("Focus", 88)].map(|(n, c)| {
                            Row::new([
                                Cell::from(Span::styled(n, theme::PRIMARY)),
                                Cell::from(
                                    Line::styled(c.to_string(), theme::DIM)
                                        .alignment(Alignment::Right),
                                ),
                            ])
                        }),
                        [Constraint::Fill(1), Constraint::Length(4)],
                    )
                    .row_highlight_style(theme::SELECTED_BLUR)
                    .highlight_symbol("▸ ")
                    .highlight_spacing(HighlightSpacing::Always),
                    pl_body,
                    &mut st,
                );

                let sb = section(
                    frame,
                    right,
                    "Nothing Gold",
                    Some(Line::styled("12 songs  ·  47min", theme::DIM)),
                    true,
                );
                let mut st2 = TableState::default();
                st2.select(Some(1));
                frame.render_stateful_widget(
                    Table::new(
                        [
                            (1, "Ribs", "Lorde", "3:41", true),
                            (2, "Vienna", "Billy Joel", "3:34", false),
                            (3, "Team", "Lorde", "3:13", false),
                        ]
                        .map(|(i, t, a, d, playing)| {
                            Row::new([
                                Cell::from(Line::from(vec![
                                    Span::styled(if playing { "♫ " } else { "  " }, theme::PLAYING),
                                    Span::styled(format!("{i}  "), theme::DIM),
                                    Span::styled(
                                        t,
                                        if playing {
                                            theme::PLAYING
                                        } else {
                                            theme::PRIMARY
                                        },
                                    ),
                                    Span::styled(SEP, theme::DIM),
                                    Span::styled(a, theme::META),
                                ])),
                                Cell::from(Line::styled(d, theme::DIM).alignment(Alignment::Right)),
                            ])
                        }),
                        App::TRACK_COLS,
                    )
                    .row_highlight_style(theme::SELECTED)
                    .highlight_symbol("▸ ")
                    .highlight_spacing(HighlightSpacing::Always),
                    sb,
                    &mut st2,
                );

                let status = vec![
                    Span::styled("↺ Cycle", theme::DIM),
                    Span::styled(SEP, theme::DIM),
                    Span::styled("80%", theme::DIM),
                ];
                let sw: usize = status.iter().map(|x| width_of(&x.content)).sum();
                let [l, r] = np.layout(&Layout::horizontal([
                    Constraint::Fill(1),
                    Constraint::Length(sw as u16),
                ]));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("♫ ", theme::PLAYING),
                        Span::styled("Ribs", theme::PRIMARY),
                        Span::styled(SEP, theme::DIM),
                        Span::styled("Lorde", theme::META),
                    ])),
                    l,
                );
                frame.render_widget(Paragraph::new(Line::from(status)), r);

                let [ea, ba, ta] = prog.layout(&Layout::horizontal([
                    Constraint::Length(4),
                    Constraint::Fill(1),
                    Constraint::Length(4),
                ]));
                frame.render_widget(Paragraph::new(Span::styled("1:12", theme::DIM)), ea);
                frame.render_widget(
                    Paragraph::new(Span::styled("3:41", theme::DIM)).alignment(Alignment::Right),
                    ta,
                );
                frame.render_widget(
                    LineGauge::default()
                        .ratio(0.33)
                        .label("")
                        .filled_symbol(symbols::line::THICK.horizontal)
                        .unfilled_symbol(symbols::line::NORMAL.horizontal)
                        .filled_style(theme::ACCENT)
                        .unfilled_style(theme::RULE),
                    Rect {
                        x: ba.x + 1,
                        width: ba.width.saturating_sub(2),
                        ..ba
                    },
                );

                frame.render_widget(
                    Paragraph::new(Line::from(fit_hints(
                        &[
                            ("↵", "play"),
                            ("spc", "pause"),
                            ("/", "filter"),
                            ("a", "+queue"),
                            ("o", "queue"),
                            ("y", "lyrics"),
                            ("p/n", "skip"),
                            ("?", "keys"),
                        ],
                        help.width as usize,
                    ))),
                    help,
                );
            });
            println!("\n──── {w}x{h} ────");
            for line in out {
                println!("|{line}");
            }
        }
    }

    #[test]
    fn section_draws_an_uppercase_label_over_a_rule() {
        let out = draw(24, 4, |frame| {
            let body = section(frame, frame.area(), "Playlists", None, true);
            // The rect handed back must start below the rule.
            assert_eq!(body.y, 2);
            assert_eq!(body.height, 2);
        });
        assert_eq!(out[0], "PLAYLISTS");
        assert_eq!(out[1], "─".repeat(24));
    }

    #[test]
    fn section_shows_a_status_when_it_fits() {
        let status = || Some(Line::styled("12 songs", theme::DIM));
        let wide = draw(30, 2, |frame| {
            section(frame, frame.area(), "Songs", status(), true);
        });
        assert_eq!(wide[0], "SONGS  ·  12 songs");

        // Too narrow for both: the status is dropped rather than wrapped or
        // clipped mid-word.
        let narrow = draw(10, 2, |frame| {
            section(frame, frame.area(), "Songs", status(), true);
        });
        assert_eq!(narrow[0], "SONGS");
    }

    #[test]
    fn section_survives_a_degenerate_rect() {
        // One row: header only, no rule, and a zero-height body.
        let out = draw(12, 1, |frame| {
            let body = section(frame, frame.area(), "Songs", None, false);
            assert_eq!(body.height, 0);
        });
        assert_eq!(out[0], "SONGS");
        draw(1, 1, |frame| {
            section(frame, frame.area(), "Songs", None, false);
        });
    }

    #[test]
    fn fit_hints_drops_whole_hints_rather_than_clipping() {
        let items = [("j/k", "nav"), ("↵", "play"), ("q", "quit")];
        let width =
            |spans: &[Span<'static>]| -> usize { spans.iter().map(|s| width_of(&s.content)).sum() };

        // Everything fits.
        assert_eq!(width(&fit_hints(&items, 80)), 7 + 5 + 6 + 5 + 6);

        // Tight: keeps a prefix, never exceeds the budget, never half a hint.
        for w in 0..40usize {
            let spans = fit_hints(&items, w);
            assert!(width(&spans) <= w, "overflowed at width {w}");
            // n complete hints == 2n spans + (n-1) separators == 3n-1.
            // Anything else would mean a half-rendered hint.
            assert!(
                spans.is_empty() || (spans.len() + 1).is_multiple_of(3),
                "partial hint at width {w}: {} spans",
                spans.len()
            );
        }

        assert!(fit_hints(&items, 0).is_empty());
    }

    #[test]
    fn truncate_line_measures_display_cells_not_chars() {
        assert_eq!(truncate_line("hello", 10), "hello");
        assert_eq!(truncate_line("hello world", 8), "hello w…");
        assert_eq!(truncate_line("", 5), "");
        assert_eq!(truncate_line("abc", 0), "");

        // Wide (CJK) characters are two cells each: 5 chars = 10 cells, so a
        // char-based truncation would have over-run the column by 5.
        let wide = "日本語の歌";
        assert_eq!(width_of(wide), 10);
        let cut = truncate_line(wide, 6);
        assert!(width_of(&cut) <= 6, "{cut:?} was {} cells", width_of(&cut));
    }

    #[test]
    fn fmt_secs_switches_to_hours() {
        assert_eq!(fmt_secs(0.0), "0:00");
        assert_eq!(fmt_secs(61.0), "1:01");
        assert_eq!(fmt_secs(221.0), "3:41");
        // Used to print "70:11" and clip a 5-wide column.
        assert_eq!(fmt_secs(4211.0), "1:10:11");
        assert_eq!(fmt_secs(-5.0), "0:00", "negatives must not wrap");
    }

    #[test]
    fn scrollbar_only_reserves_space_when_the_list_overflows() {
        let area = Rect::new(0, 0, 20, 10);
        // Fits: full width, no bar.
        assert_eq!(list_body(area, 10).width, 20);
        // Overflows: a column is reserved so the bar can't paint over content.
        assert_eq!(list_body(area, 11).width, 18);
    }

    #[test]
    fn scrollbar_is_not_drawn_for_a_list_that_fits() {
        let blank = draw(20, 6, |frame| {
            render_scrollbar(frame, frame.area(), 6, Some(0));
        });
        assert!(
            blank.iter().all(String::is_empty),
            "a fitting list must draw no scrollbar, got {blank:?}"
        );

        let drawn = draw(20, 6, |frame| {
            render_scrollbar(frame, frame.area(), 60, Some(0));
        });
        assert!(drawn.iter().any(|r| !r.is_empty()));
    }

    #[test]
    fn active_line_sits_on_the_centre_row() {
        let out = synced_view(&rows(20), Some(10), 7, 0);
        assert_eq!(out.len(), 7);
        // Centre of a 7-row view is index 3.
        assert_eq!(texts(&out)[3], "line10");
        assert_eq!(styles(&out)[3], ACTIVE_LYRIC);
    }

    #[test]
    fn active_line_stays_centred_at_the_very_start() {
        // The view pads with blanks above rather than clamping to row 0.
        let out = synced_view(&rows(20), Some(0), 7, 0);
        let t = texts(&out);
        assert_eq!(t[3], "line0", "first lyric must still be centred");
        assert!(
            t[..3].iter().all(String::is_empty),
            "expected blank padding above"
        );
        assert_eq!(styles(&out)[3], ACTIVE_LYRIC);
    }

    #[test]
    fn active_line_stays_centred_at_the_very_end() {
        let out = synced_view(&rows(20), Some(19), 7, 0);
        let t = texts(&out);
        assert_eq!(t[3], "line19");
        assert!(
            t[4..].iter().all(String::is_empty),
            "expected blank padding below"
        );
    }

    #[test]
    fn only_the_active_line_is_coloured() {
        // The whole point of the contrast change: exactly one row carries a
        // background, and no other row carries a hue.
        let out = synced_view(&rows(20), Some(10), 7, 0);
        let s = styles(&out);
        assert_eq!(s.iter().filter(|st| st.bg.is_some()).count(), 1);
        assert!(
            s.iter()
                .enumerate()
                .filter(|(i, _)| *i != 3)
                .all(|(_, st)| matches!(st.fg, None | Some(Color::Gray) | Some(Color::DarkGray))),
            "context rows must stay achromatic"
        );
    }

    #[test]
    fn neighbours_are_brighter_than_distant_lines() {
        let out = synced_view(&rows(20), Some(10), 7, 0);
        let s = styles(&out);
        assert_eq!(s[2].fg, Some(Color::Gray), "line above");
        assert_eq!(s[4].fg, Some(Color::Gray), "line below");
        assert_eq!(s[1].fg, Some(Color::DarkGray), "two above");
        assert_eq!(s[5].fg, Some(Color::DarkGray), "two below");
    }

    #[test]
    fn a_wrapped_lyric_highlights_as_one_unit() {
        // Two display rows belonging to lyric 1 must both be highlighted.
        let wrapped = vec![
            LyricRow {
                lyric: 0,
                text: "a".into(),
            },
            LyricRow {
                lyric: 1,
                text: "long part one".into(),
            },
            LyricRow {
                lyric: 1,
                text: "long part two".into(),
            },
            LyricRow {
                lyric: 2,
                text: "b".into(),
            },
        ];
        let out = synced_view(&wrapped, Some(1), 4, 0);
        let highlighted = styles(&out).iter().filter(|s| **s == ACTIVE_LYRIC).count();
        assert_eq!(highlighted, 2);
    }

    #[test]
    fn interlude_shows_a_marker_instead_of_empty_text() {
        let gap = vec![
            LyricRow {
                lyric: 0,
                text: "a".into(),
            },
            LyricRow {
                lyric: 1,
                text: String::new(),
            },
            LyricRow {
                lyric: 2,
                text: "b".into(),
            },
        ];
        let out = synced_view(&gap, Some(1), 3, 0);
        assert_eq!(texts(&out)[1], "♪ ♪ ♪");
        assert_eq!(styles(&out)[1], ACTIVE_LYRIC);
    }

    #[test]
    fn intro_dims_everything() {
        let out = synced_view(&rows(20), None, 5, 0);
        assert!(styles(&out).iter().all(|s| s.bg.is_none()));
    }

    #[test]
    fn scroll_offsets_the_view_without_panicking() {
        // Far out of range in both directions must yield blanks, not a panic.
        assert_eq!(synced_view(&rows(20), Some(10), 5, -9999).len(), 5);
        assert_eq!(synced_view(&rows(20), Some(10), 5, 9999).len(), 5);
        assert!(
            texts(&synced_view(&rows(20), Some(10), 5, 9999))
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(
            synced_view(&[], Some(0), 5, 0)
                .iter()
                .all(|l| l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty()))
        );
        assert_eq!(synced_view(&rows(5), Some(0), 0, 0).len(), 0);
    }
}
