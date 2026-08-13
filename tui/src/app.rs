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

use ytm_core::library::{LibraryFetcher, SongBatch};
use ytm_core::lyrics::{self, LyricsMsg, LyricsQuery, LyricsService, TrackLyrics};
use ytm_core::persistence::{self, LyricsOverrides, QueueState, RestoreOutcome};
use ytm_core::search::{self, ResultKind, SearchMsg, SearchResult};
use ytm_core::{
    AppendOutcome, AudioState, Cover, CoverMsg, Library, MediaCmd, MediaControls, NowPlaying,
    PlayState, Player, RemoveOutcome, Track, TrackInfo, TranslateMsg,
};

use crate::kitty;

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

    /// Translated text. Magenta plays no other role in the palette, so a line
    /// the app wrote can never be read as a line the song did — which is the
    /// whole point of showing the two together.
    pub const TRANSLATION: Style = Style::new().fg(Color::Magenta);
    /// A translation that isn't the line playing: italic and faint, so it sits
    /// under its original rather than competing with it. The italic is a second
    /// cue for terminals whose magenta is loud, and the colour is a second cue
    /// for those that don't render italics at all.
    pub const TRANSLATION_DIM: Style = Style::new()
        .fg(Color::Magenta)
        .add_modifier(Modifier::ITALIC)
        .add_modifier(Modifier::DIM);
    /// The translation of the line currently playing.
    pub const TRANSLATION_ACTIVE: Style = Style::new()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::ITALIC);

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

/// Where each of `before`'s tracks ended up in `now`, matched by video id, or
/// `None` when every one of them is exactly where it was.
///
/// The `None` is the common answer and the reason this is worth a function: a
/// track added to an ordinary playlist is *appended*, so a refetch moves
/// nothing and there is no reason to rebuild a queue. Adding to Liked Music
/// puts it at the top, and then everything has moved. A track that has left the
/// playlist gets `Some(None)` — its own entry, saying it is gone.
fn moved_indices(before: &[Option<String>], now: &[Track]) -> Option<Vec<Option<usize>>> {
    let places: std::collections::HashMap<&str, usize> = now
        .iter()
        .enumerate()
        .filter_map(|(i, t)| Some((t.video_id.as_deref()?, i)))
        .collect();
    let moved: Vec<Option<usize>> = before
        .iter()
        .enumerate()
        .map(|(i, id)| match id.as_deref() {
            Some(id) => places.get(id).copied(),
            // A track with no video id is unplayable and unmatchable, so it is
            // left exactly where it was rather than counted as having moved —
            // otherwise one such track means every refetch of that playlist
            // reads as a reorder and drops it out of the queue.
            None => Some(i),
        })
        .collect();
    moved
        .iter()
        .enumerate()
        .any(|(i, m)| *m != Some(i))
        .then_some(moved)
}

/// Word-wraps `text` to `width` **display cells**, at most `max_lines` of them,
/// marking a truncation with `…` as [`truncate_line`] does.
///
/// [`wrap_n_lines`] breaks at the exact cell the column runs out at, which is
/// right for a lyric: the line is the unit, the panel is narrow, and a break
/// inside a word reads as the continuation it is. A metadata card is read as
/// prose instead — a title broken as `Everybody Wants To R / ule The World`
/// reads as a bug — so this breaks between words where there are any. A run
/// with none falls back to the cell-exact split, which is also the CJK path:
/// there the whole title is one "word" and cells are the only unit there is.
fn wrap_words(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        for piece in if width_of(word) > width {
            wrap_n_lines(word, width, usize::MAX)
        } else {
            vec![word.to_string()]
        } {
            let space = usize::from(!line.is_empty());
            if !line.is_empty() && width_of(&line) + space + width_of(&piece) > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&piece);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            while width_of(last) + 1 > width && last.pop().is_some() {}
            last.push('…');
        }
    }
    lines
}

/// Hard-wraps `text` to `width` **display cells**.
///
/// Cells rather than `char`s, for the same reason [`truncate_line`] measures
/// them: a CJK line is two cells per character, so counting characters lets it
/// run to twice the panel's width and be clipped. Lyrics are where this shows
/// — and where the missing half is the point of the panel.
fn wrap_n_lines(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return vec![text.to_string()];
    }
    let mut result: Vec<String> = Vec::new();
    'outer: for raw in text.lines() {
        if raw.is_empty() {
            result.push(String::new());
            if result.len() >= max_lines {
                break;
            }
            continue;
        }
        let mut chars = raw.chars().peekable();
        while chars.peek().is_some() {
            let mut piece = String::new();
            let mut used = 0;
            while let Some(&c) = chars.peek() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                // `piece.is_empty()` keeps a character wider than the whole
                // column from stalling the loop; it overflows by a cell
                // instead, which is the lesser of the two failures.
                if used + w > width && !piece.is_empty() {
                    break;
                }
                piece.push(c);
                used += w;
                chars.next();
            }
            if result.len() + 1 >= max_lines && chars.peek().is_some() {
                while width_of(&piece) + 1 > width && piece.pop().is_some() {}
                piece.push('…');
                result.push(piece);
                break 'outer;
            }
            result.push(piece);
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

/// A title with metadata after it, fitted to `budget` display cells.
///
/// The title has first claim; each field after it takes what is left, in
/// order, and anything cut is marked with `…`. Without this a `Table` clips
/// the row at the column edge instead — which cuts mid-word, and with a CJK
/// title mid-character, so a row ends in a half-drawn glyph and no sign that
/// anything is missing.
fn fit_meta(
    title: &str,
    title_style: Style,
    rest: &[(String, Style)],
    budget: usize,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(truncate_line(title, budget), title_style)];
    let mut used = width_of(title).min(budget);
    for (text, style) in rest {
        if text.is_empty() {
            continue;
        }
        let left = budget.saturating_sub(used + width_of(SEP));
        // Below this there is room for the separator and an ellipsis and
        // nothing else, which says less than stopping does.
        if left <= 3 {
            break;
        }
        spans.push(Span::styled(SEP, theme::DIM));
        spans.push(Span::styled(truncate_line(text, left), *style));
        used += width_of(SEP) + width_of(text).min(left);
    }
    spans
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

/// The same, rounded rather than truncated.
///
/// For a *total* that is what the rest of the UI already shows: YouTube gives
/// whole seconds, mpv gives the real length, and truncating 191.6s printed
/// `3:11` under a list that said `3:12`. Elapsed still truncates — a clock
/// should not reach `0:01` before the first second is out.
fn fmt_secs_rounded(secs: f64) -> String {
    fmt_duration(secs.max(0.0).round() as u64)
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
                let style = if row.translated {
                    theme::TRANSLATION_DIM
                } else {
                    theme::DIM
                };
                return Line::styled(row.text.clone(), style).centered();
            };

            if row.lyric == active {
                // The translation of the active line is *not* given the
                // highlight: two adjacent rows in the same marker pen would
                // read as one four-line lyric.
                if row.translated {
                    return Line::styled(row.text.clone(), theme::TRANSLATION_ACTIVE).centered();
                }
                let text = if row.text.is_empty() {
                    "♪ ♪ ♪".to_string()
                } else {
                    row.text.clone()
                };
                // Padded by a space either side so the highlight reads as a
                // marker pen over the words rather than clinging to the glyphs.
                return Line::styled(format!(" {text} "), ACTIVE_LYRIC).centered();
            }

            let style = if row.translated {
                theme::TRANSLATION_DIM
            } else if row.lyric.abs_diff(active) == 1 {
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
    /// This row is the translation of `lyric` rather than the words themselves.
    /// It follows its original's rows, so the highlight still lands on the
    /// first row of the line being sung.
    translated: bool,
}

/// Translations kept before the oldest is dropped. A few kilobytes each, and
/// the AI backend charges for every one that has to be fetched again.
const MAX_TRANSLATIONS: usize = 64;

/// Which translator the lyrics panel is showing. `i` picks [`Self::Free`] and
/// `I` picks [`Self::Ai`]; each key turns its own off again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranslateMode {
    Off,
    Free,
    Ai,
}

/// Per-record translation state, keyed by lrclib id in [`App::translations`].
/// Terminal like [`LyricsEntry`]: toggling `i` off and on again re-reads the
/// cache rather than the network.
enum TranslationEntry {
    Loading,
    /// One entry per lyric line, empty where there was nothing to translate.
    Ready(Vec<String>),
    /// The reason is reported as a toast when it happens and written to the
    /// log; what has to survive here is only that this record was tried, so a
    /// dead network isn't re-dialled once a tick. Pressing `i` twice retries.
    Failed,
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

/// Wraps a record's lines to `width`, weaving each line's translation in under
/// it.
///
/// Both halves of a line carry the same `lyric` index, so a translated pair
/// highlights, scrolls and centres as the single line it is. `translation` may
/// be shorter than `texts`, or empty when nothing is being translated.
fn lyric_rows(texts: &[String], translation: &[String], width: u16) -> Vec<LyricRow> {
    let mut rows = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        if text.trim().is_empty() {
            // Keep interludes as a row of their own so synced playback has
            // something to sit on during instrumental gaps.
            rows.push(LyricRow {
                lyric: i,
                text: String::new(),
                translated: false,
            });
            continue;
        }
        for piece in wrap_n_lines(text, width as usize, usize::MAX) {
            rows.push(LyricRow {
                lyric: i,
                text: piece,
                translated: false,
            });
        }
        // Nothing at all when the line couldn't be translated, rather than a
        // blank row that would read as a lyric the record is missing. Nothing
        // either when the translation came back identical — a kanji-only line
        // often does, and printing it twice is noise, not information.
        let Some(line) = translation
            .get(i)
            .filter(|t| !t.trim().is_empty() && t.trim() != text.trim())
        else {
            continue;
        };
        for piece in wrap_n_lines(line, width as usize, usize::MAX) {
            rows.push(LyricRow {
                lyric: i,
                text: piece,
                translated: true,
            });
        }
    }
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

// ── search ────────────────────────────────────────────────────────────────────

/// Covers held in memory before the oldest is dropped. Each is a few hundred
/// kilobytes of decoded pixels, and only one is ever on screen.
const MAX_COVERS: usize = 32;

/// Widest a cover is ever drawn, in cells — the now-playing card's ceiling, and
/// more than the search panel's 20. It is what a cover is fetched and kept at,
/// so nothing is held at a size no panel can put on screen.
const MAX_COVER_COLS: u16 = 24;

/// The search panel: a query line, a result list, and optionally the "add to"
/// popup over the top of it.
struct SearchState {
    /// What the user has typed.
    query: String,
    /// Still editing the query, rather than moving through results. `Enter`
    /// crosses from one to the other and `/`-style editing never resumes by
    /// accident, so `j` means "down a result" and not "type a j".
    typing: bool,
    /// The query the results below actually belong to, so a stale response for
    /// a query since retyped can be recognised and dropped.
    ran: String,
    results: Vec<SearchResult>,
    state: TableState,
    loading: bool,
    error: Option<String>,
    /// The `a` popup: which library to add the selected result to.
    add: Option<TableState>,
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            typing: true,
            ran: String::new(),
            results: Vec::new(),
            state: TableState::default(),
            loading: false,
            error: None,
            add: None,
        }
    }

    fn selected(&self) -> Option<&SearchResult> {
        self.results.get(self.state.selected()?)
    }
}

/// The marker and colour for a result's kind.
///
/// A song is an art track — the label's own audio, with an album and the
/// release duration — and is what you want when it exists. A video may be the
/// only copy of a track that was never distributed as one, so it is offered
/// rather than hidden, but marked clearly enough that the choice is deliberate.
fn kind_marker(kind: ResultKind) -> (&'static str, Style) {
    match kind {
        ResultKind::Song => ("♪ song ", theme::ACCENT),
        ResultKind::Video => ("▶ video", theme::WARN),
    }
}

// ── app ───────────────────────────────────────────────────────────────────────

/// Why [`App::run`] returned, for the caller to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The user quit. Nothing left to do.
    Quit,
    /// The session needs renewing and the app wants starting again after it.
    Reauth,
}

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
    /// Whether an empty library may renew the session by itself: a browser on
    /// record, the setting left on, and no renewal tried yet this run. Off, an
    /// empty library is reported and `r` is the way out, since the fallback is
    /// a set of prompts and those are not something to start unasked.
    auto_reauth: bool,
    // background song loading
    songs_rx: std::sync::mpsc::Receiver<SongBatch>,
    /// Kept so `r` can ask again for a playlist whose fetch failed.
    fetcher: LibraryFetcher,
    pending_queue_restore: Option<QueueState>,
    // filter
    filter: String,
    filter_mode: bool,
    /// The last answer [`App::filtered_songs`] gave, against the playlist,
    /// query and song count it was computed for. Rebuilt only when one of
    /// those changes — it is asked for on every frame, and lowercasing a few
    /// thousand titles thirty times a second is not free.
    songs_filter: Option<(usize, String, usize, Vec<usize>)>,
    /// The same for the queue, keyed by [`Player::queue_revision`] since a
    /// queue can be reordered without changing length.
    queue_filter: Option<(u64, String, Vec<usize>)>,
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
    // translation
    /// Set by `i` / `I`. A mode rather than a per-song setting, like lyrics
    /// themselves: turn it on once and it follows you through the queue.
    translate_mode: TranslateMode,
    translate_tx: std::sync::mpsc::Sender<TranslateMsg>,
    translate_rx: std::sync::mpsc::Receiver<TranslateMsg>,
    /// Translations by lrclib record id — by *record* rather than by video, so
    /// picking a different one with `c` gets a translation of its own and two
    /// tracks on the same record share one.
    translations: std::collections::HashMap<(u64, bool), TranslationEntry>,
    /// Insertion order for [`Self::translations`], oldest first, so the map can
    /// be bounded without holding a whole session's songs.
    translation_order: Vec<(u64, bool)>,
    /// The same thing on disk, so a song is translated once rather than once a
    /// session. `r` drops a record from it to get a fresh translation.
    saved_translations: persistence::Translations,
    /// User settings from `config.toml`, read once at startup.
    config: ytm_core::Config,
    /// MPRIS: the media keys and the desktop's player list. `None` when there
    /// is no session bus to serve on.
    media: Option<MediaControls>,
    // search
    /// The authenticated client, for the calls that aren't library fetches.
    yt: std::sync::Arc<ytm_core::YTMusicClient>,
    /// `None` until `s` opens the search panel.
    search: Option<SearchState>,
    search_tx: std::sync::mpsc::Sender<SearchMsg>,
    search_rx: std::sync::mpsc::Receiver<SearchMsg>,
    // cover art
    /// Decoded covers by video id, bounded — each is a few hundred kilobytes
    /// of pixels and a long search session would otherwise keep every one.
    covers: std::collections::HashMap<String, Cover>,
    /// Insertion order for [`Self::covers`], oldest first.
    cover_order: Vec<String>,
    /// Fetches in flight, so a cover is asked for once however many times the
    /// selection passes over its row.
    cover_pending: std::collections::HashSet<String>,
    /// Fetches that failed. Without this the next tick asks again — and the
    /// tick after that — so a thumbnail URL that 404s becomes a request every
    /// 200 ms for as long as its row stays highlighted.
    cover_failed: std::collections::HashSet<String>,
    cover_tx: std::sync::mpsc::Sender<CoverMsg>,
    cover_rx: std::sync::mpsc::Receiver<CoverMsg>,
    /// What is on the terminal, and where. Only touched after a frame is
    /// drawn — see [`Self::draw_cover`].
    canvas: kitty::Canvas,
    /// Whether this terminal can show a cover at all.
    covers_enabled: bool,
    /// What the last frame left room for: which video's cover, and where.
    /// Set while rendering, acted on afterwards — see [`Self::draw_cover`].
    /// Search and lyrics never both own a panel, so one slot is enough.
    cover_target: Option<(String, Rect)>,
}

impl App {
    pub fn new(
        library: Library,
        saved_queue: Option<QueueState>,
        songs_rx: std::sync::mpsc::Receiver<SongBatch>,
        fetcher: LibraryFetcher,
        rt: tokio::runtime::Handle,
        config: ytm_core::Config,
        auto_reauth: bool,
    ) -> Self {
        let yt = fetcher.client();
        let n = library.len();
        let selected = (n > 0).then_some(0);

        // Restore the volume saved on the previous exit.
        let mut player = Player::new();
        player.set_volume(persistence::load_settings().volume);

        let (lyrics_tx, lyrics_rx) = std::sync::mpsc::channel();
        let (translate_tx, translate_rx) = std::sync::mpsc::channel();
        let (search_tx, search_rx) = std::sync::mpsc::channel();
        let (cover_tx, cover_rx) = std::sync::mpsc::channel();

        // Decided once: it depends on the terminal the app was launched in,
        // which cannot change under it.
        let covers_enabled = config.ui.covers && kitty::supported();
        log::info!("covers: {}", if covers_enabled { "on" } else { "off" });

        let media = MediaControls::new(&rt);

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
            auto_reauth,
            songs_rx,
            fetcher,
            pending_queue_restore: saved_queue,
            filter: String::new(),
            filter_mode: false,
            songs_filter: None,
            queue_filter: None,
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
            translate_mode: TranslateMode::Off,
            translate_tx,
            translate_rx,
            translations: std::collections::HashMap::new(),
            translation_order: Vec::new(),
            saved_translations: persistence::load_translations(),
            config,
            lyrics_dirty: false,
            media,
            yt,
            search: None,
            search_tx,
            search_rx,
            covers: std::collections::HashMap::new(),
            cover_order: Vec::new(),
            cover_pending: std::collections::HashSet::new(),
            cover_failed: std::collections::HashSet::new(),
            cover_tx,
            cover_rx,
            canvas: kitty::Canvas::default(),
            covers_enabled,
            cover_target: None,
        }
    }

    // ── search ────────────────────────────────────────────────────────────────

    /// Whether the search panel currently owns the keyboard.
    ///
    /// Typing a query and the add popup take every key regardless of which
    /// panel has focus — otherwise `h` would leave mid-word. Past that it is
    /// the ordinary `h`/`l` business, so moving focus to the playlists gives
    /// the normal bindings back, exactly as it does in lyrics mode.
    fn search_has_focus(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|s| s.typing || s.add.is_some() || self.active_panel == Panel::Songs)
    }

    /// `s` — opens the search panel, or closes it if it is already open.
    fn toggle_search(&mut self) {
        if self.search.take().is_some() {
            self.canvas.clear();
            self.cover_target = None;
            return;
        }
        // Lyrics take the same column, and two panels cannot both have it.
        self.lyrics_mode = false;
        self.lyrics_picker = None;
        self.search = Some(SearchState::new());
    }

    /// Runs whatever is in the query line.
    fn submit_search(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let query = search.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        search.typing = false;
        search.loading = true;
        search.error = None;
        search.ran = query.clone();
        search::spawn_search(
            &self.lyrics_handle,
            std::sync::Arc::clone(&self.yt),
            query,
            self.search_tx.clone(),
        );
    }

    /// Plays the highlighted result.
    ///
    /// A search hit has no place in the library, and everything downstream
    /// addresses a track by its position in one — so it is given a place first.
    fn play_search_result(&mut self) {
        let Some(hit) = self
            .search
            .as_ref()
            .and_then(SearchState::selected)
            .cloned()
        else {
            return;
        };
        let (pl, song) = self.library.place_search_result(hit.to_track());
        self.player.play(&self.library, pl, song);
        self.sync_queue_view();
        self.notify(format!("Playing: {}", hit.title));
    }

    /// `a` — opens the "add to" popup for the highlighted result.
    fn open_add_picker(&mut self) {
        if self.library.is_empty() {
            self.notify("No playlists to add to");
            return;
        }
        if let Some(search) = self.search.as_mut()
            && search.selected().is_some()
        {
            let mut state = TableState::default();
            state.select(Some(0));
            search.add = Some(state);
        }
    }

    /// The playlists offered by the `a` popup — the user's own, never the
    /// synthetic one search results are filed under.
    fn add_targets(&self) -> Vec<(usize, &str)> {
        self.library
            .entries()
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.library.is_search_playlist(*i))
            .map(|(i, e)| (i, e.playlist.title.as_str()))
            .collect()
    }

    /// Sends the highlighted result to the highlighted playlist.
    fn commit_add(&mut self) {
        let targets: Vec<(usize, String, String)> = self
            .add_targets()
            .into_iter()
            .filter_map(|(i, title)| {
                Some((
                    i,
                    self.library.playlist(i)?.playlist_id.clone(),
                    title.to_string(),
                ))
            })
            .collect();

        let Some(search) = self.search.as_mut() else {
            return;
        };
        let Some(row) = search.add.as_ref().and_then(TableState::selected) else {
            return;
        };
        let Some((playlist, playlist_id, title)) = targets.get(row).cloned() else {
            return;
        };
        let Some(hit) = search.selected().cloned() else {
            return;
        };
        search.add = None;

        // Liked Music is not a playlist you can add items to — it is the
        // like button, under a different name.
        let playlist_id = if playlist_id.eq_ignore_ascii_case("LM") {
            String::new()
        } else {
            playlist_id
        };
        self.notify(format!("Adding to {title}…"));
        search::spawn_add(
            &self.lyrics_handle,
            std::sync::Arc::clone(&self.yt),
            search::AddRequest {
                playlist_id,
                playlist,
                video_id: hit.video_id,
                title: hit.title,
                where_to: title,
            },
            self.search_tx.clone(),
        );
    }

    /// Keys while the search panel has focus. `true` means quit.
    fn handle_search_key(&mut self, code: KeyCode) -> bool {
        // The popup is modal within the panel.
        if self.search.as_ref().is_some_and(|s| s.add.is_some()) {
            let n = self.add_targets().len();
            match code {
                KeyCode::Esc | KeyCode::Char('a') => {
                    if let Some(s) = self.search.as_mut() {
                        s.add = None;
                    }
                }
                KeyCode::Enter => self.commit_add(),
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(state) = self.search.as_mut().and_then(|s| s.add.as_mut()) {
                        let next = state.selected().map_or(0, |i| (i + 1) % n.max(1));
                        state.select(Some(next));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(state) = self.search.as_mut().and_then(|s| s.add.as_mut()) {
                        let prev = state
                            .selected()
                            .map_or(0, |i| if i == 0 { n.saturating_sub(1) } else { i - 1 });
                        state.select(Some(prev));
                    }
                }
                _ => {}
            }
            return false;
        }

        let typing = self.search.as_ref().is_some_and(|s| s.typing);
        if typing {
            match code {
                KeyCode::Esc => self.toggle_search(),
                KeyCode::Enter => self.submit_search(),
                KeyCode::Backspace => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.push(c);
                    }
                }
                _ => {}
            }
            return false;
        }

        match code {
            KeyCode::Char('q') => return true,
            // Back to the query line, so a refinement doesn't need the panel
            // closed and reopened. A second Esc closes it.
            KeyCode::Esc => {
                if let Some(s) = self.search.as_mut() {
                    s.typing = true;
                } else {
                    self.toggle_search();
                }
            }
            KeyCode::Char('/') => {
                if let Some(s) = self.search.as_mut() {
                    s.typing = true;
                }
            }
            KeyCode::Char('s') => self.toggle_search(),
            // Panel movement, as everywhere else: the search panel keeps its
            // results, focus goes to the playlists, `l` comes back.
            KeyCode::Char('h') => self.active_panel = Panel::Playlists,
            KeyCode::Char('l') => self.active_panel = Panel::Songs,
            KeyCode::Enter => self.play_search_result(),
            KeyCode::Char('a') => self.open_add_picker(),
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(s) = self.search.as_mut() {
                    s.state.select_next();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(s) = self.search.as_mut() {
                    s.state.select_previous();
                }
            }
            KeyCode::Char(' ') => self.player.play_pause(&self.library),
            KeyCode::Char('n') => {
                self.player.next(&self.library);
                self.sync_queue_view();
            }
            KeyCode::Char('p') => {
                if self.player.restart_or_previous(&self.library) {
                    self.sync_queue_view();
                }
            }
            KeyCode::Left => self.player.seek(-5.0),
            KeyCode::Right => self.player.seek(5.0),
            KeyCode::Char('m') => self.player.toggle_mute(),
            KeyCode::Char('?') => self.show_keymap = true,
            _ => {}
        }
        false
    }

    /// Drain finished searches and adds.
    fn drain_search(&mut self) {
        while let Ok(msg) = self.search_rx.try_recv() {
            match msg {
                SearchMsg::Results { query, result } => {
                    let Some(search) = self.search.as_mut() else {
                        continue;
                    };
                    // The user has typed something else since; this answer is
                    // to a question no longer being asked.
                    if search.ran != query {
                        continue;
                    }
                    search.loading = false;
                    match result {
                        Ok(results) => {
                            search.state.select((!results.is_empty()).then_some(0));
                            search.results = results;
                            search.error = None;
                        }
                        Err(e) => {
                            log::warn!("search: {query:?} failed: {e}");
                            search.results.clear();
                            search.error = Some(e);
                        }
                    }
                }
                SearchMsg::Added {
                    title,
                    where_to,
                    playlist,
                    result,
                } => match result {
                    Ok(()) => {
                        log::info!("search: added {title:?} to {where_to:?}");
                        self.notify(format!("Added {title} to {where_to}"));
                        // The playlist on the server now has a track this copy
                        // of it does not. Fetching it again is the only way to
                        // find out where the track landed — appended for a
                        // playlist, at the *top* for Liked Music — and it is
                        // what makes the new song playable without a restart.
                        self.refresh_playlist(playlist);
                    }
                    Err(e) => {
                        log::warn!("search: adding {title:?} to {where_to:?} failed: {e}");
                        self.notify(format!("Couldn't add to {where_to}: {e}"));
                    }
                },
            }
        }
    }

    // ── cover art ─────────────────────────────────────────────────────────────

    /// The cover that wants showing: the highlighted search result, or — in
    /// lyrics mode, where the left column is given over to it — the track
    /// that is playing.
    fn wanted_cover(&self) -> Option<(String, String)> {
        if let Some(search) = self.search.as_ref() {
            let hit = search.selected()?;
            return Some((hit.video_id.clone(), hit.thumbnail.clone()?));
        }
        if self.lyrics_mode {
            let (pl, song) = self.player.playing()?;
            let track = self.library.track(pl, song)?;
            return Some((track.video_id.clone()?, track.thumbnail.clone()?));
        }
        None
    }

    /// Starts a fetch for that cover, if it isn't held or already coming.
    fn ensure_cover(&mut self) {
        if !self.covers_enabled {
            return;
        }
        let Some((id, url)) = self.wanted_cover() else {
            return;
        };
        if self.covers.contains_key(&id)
            || self.cover_failed.contains(&id)
            || !self.cover_pending.insert(id.clone())
        {
            return;
        }
        ytm_core::cover::spawn_fetch(
            &self.lyrics_handle,
            id,
            url,
            Self::cover_draw_px(),
            self.cover_tx.clone(),
        );
    }

    /// The largest square, in pixels, a cover can be drawn in on this terminal:
    /// the card's cell ceiling times what a cell actually measures. Asked of the
    /// terminal each time rather than cached, since a font size can change under
    /// a running app and this is one ioctl per fetch.
    fn cover_draw_px() -> u32 {
        Self::cover_draw_px_for(kitty::cell_size())
    }

    /// The square above, for a given cell size. The cover is square and the
    /// card's box is `MAX_COVER_COLS` wide by half that tall, so whichever axis
    /// works out to more pixels is what the image has to satisfy.
    fn cover_draw_px_for((cell_w, cell_h): (u32, u32)) -> u32 {
        u32::max(
            u32::from(MAX_COVER_COLS) * cell_w,
            u32::from(MAX_COVER_COLS / 2) * cell_h,
        )
    }

    fn drain_covers(&mut self) {
        while let Ok(CoverMsg { video_id, result }) = self.cover_rx.try_recv() {
            self.cover_pending.remove(&video_id);
            let cover = match result {
                Ok(cover) => cover,
                Err(e) => {
                    // Remembered, not retried: the URL came from the track and
                    // will not change, so asking again every tick would only
                    // repeat the same failure at the frame rate.
                    log::debug!("cover: {video_id} failed ({e}) — not asking again");
                    self.cover_failed.insert(video_id);
                    continue;
                }
            };
            self.cover_order.retain(|id| id != &video_id);
            while self.cover_order.len() >= MAX_COVERS {
                let oldest = self.cover_order.remove(0);
                self.covers.remove(&oldest);
            }
            self.cover_order.push(video_id.clone());
            self.covers.insert(video_id, cover);
        }
    }

    /// Puts the highlighted result's cover on screen.
    ///
    /// Called *after* the frame is drawn, because the image is composited over
    /// the cell grid rather than into it — drawing it first would put ratatui's
    /// own repaint of that rectangle on top of nothing, and the image under a
    /// frame that has already been flushed.
    fn draw_cover(&mut self) {
        if !self.covers_enabled {
            return;
        }
        let Some((id, area)) = self.cover_target.clone() else {
            self.canvas.clear();
            return;
        };
        match self.covers.get(&id) {
            Some(cover) => self.canvas.show(&id, cover, area),
            // Not arrived yet: leave the space blank rather than the previous
            // track's art sitting under the wrong title.
            None => self.canvas.clear(),
        }
    }

    /// `r` on a playlist whose fetch failed. The result comes back on the same
    /// channel as the first attempt's, so nothing else needs to change.
    fn retry_playlist(&mut self) {
        let Some(pl) = self.list_state.selected() else {
            return;
        };
        if !self.library.has_failed(pl) {
            return;
        }
        let Some(id) = self.library.playlist(pl).map(|p| p.playlist_id.clone()) else {
            return;
        };
        self.library.mark_retrying(pl);
        self.fetcher.fetch(pl, &id);
        self.notify("Retrying…");
    }

    /// Fetches a playlist again, because what the server holds has changed.
    ///
    /// The tracks come back on the same channel as the first load's, so the
    /// only thing this has to get right is not asking for the impossible: the
    /// search playlist is synthetic and has nothing to fetch.
    fn refresh_playlist(&mut self, pl: usize) {
        if self.library.is_search_playlist(pl) {
            return;
        }
        let Some(id) = self.library.playlist(pl).map(|p| p.playlist_id.clone()) else {
            return;
        };
        log::info!("library: refetching {id:?} after a change");
        self.fetcher.fetch(pl, &id);
    }

    /// Keeps the queue and the playing track meaning the same songs across a
    /// refetch of `pl` that replaced its tracks.
    ///
    /// `before` is the video ids the playlist held, in the order the queue's
    /// indices were built against — an id is a track's identity, its index is
    /// only where it sat at the time. `playing` is that track itself, kept
    /// from before the replace for the one case that needs more than a number.
    fn follow_tracks(&mut self, pl: usize, before: &[Option<String>], playing: Option<Track>) {
        if before.is_empty() {
            return;
        }
        let Some(moved) = moved_indices(before, self.library.songs(pl)) else {
            return;
        };
        log::info!("library: playlist {pl} came back reordered — following the queue across");

        let playing_ref = self.player.playing().filter(|(p, _)| *p == pl);
        let library = &mut self.library;
        self.player.remap_refs(|(p, song)| {
            if p != pl {
                return Some((p, song));
            }
            if let Some(new) = moved.get(song).copied().flatten() {
                return Some((pl, new));
            }
            // Gone from the playlist altogether. A queue entry goes with it,
            // but the track that is *playing* is still audibly playing, so it
            // is filed where tracks played from search live rather than lost.
            // Filing is idempotent, so the queue entry for that same track
            // lands on the pair the playing one did.
            if Some((p, song)) == playing_ref {
                playing.clone().map(|t| library.place_search_result(t))
            } else {
                None
            }
        });
    }

    /// Drain all pending song-batch messages from the background loader.
    /// Called each event-loop tick so the UI stays up-to-date without blocking.
    fn drain_song_channel(&mut self) {
        let mut arrived = false;
        while let Ok((idx, songs)) = self.songs_rx.try_recv() {
            // What the playlist held before this batch replaced it. Empty on
            // the first load, which is every batch at startup — only a refetch
            // of a playlist already on screen can move anything, and only then
            // is there anything to pay for.
            let before: Vec<Option<String>> = self
                .library
                .songs(idx)
                .iter()
                .map(|t| t.video_id.clone())
                .collect();
            let playing = self
                .player
                .playing()
                .filter(|(pl, _)| *pl == idx)
                .and_then(|(pl, song)| self.library.track(pl, song).cloned());

            self.library.apply_song_batch(idx, songs);
            self.follow_tracks(idx, &before, playing);
            arrived = true;
        }
        // The queue filter matches on track titles, which a batch can turn
        // from unknown into known. Its own key — the queue's revision — can't
        // see that, since the queue itself didn't change. The songs filter is
        // dropped for the same reason from the other end: its key includes the
        // playlist's length, which a refetch can leave alone while changing
        // every track under it.
        if arrived {
            self.queue_filter = None;
            self.songs_filter = None;
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
        if self.search.take().is_some() {
            self.canvas.clear();
            self.cover_target = None;
        }
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
    /// `r` in lyrics mode. Retries whichever half is on screen: with a
    /// translation showing, that is the translation — it is the part that cost
    /// something and the part a user re-reads and dislikes. With no words at
    /// all, it is the lyrics fetch that failed.
    fn retry_lyrics(&mut self) {
        if self.translate_mode != TranslateMode::Off && self.current_lyrics().is_some() {
            self.retranslate();
            return;
        }
        if let Some(id) = self.current_video_id() {
            self.lyrics_cache.remove(&id);
            // The words may come back from a different record, and last
            // record's translation is not this one's.
            self.reset_lyrics_view();
            self.notify("Retrying lyrics…");
        }
    }

    // ── translation ───────────────────────────────────────────────────────────

    /// `i` — the free translator, which is what translation means unless the
    /// user asks otherwise.
    fn toggle_translation(&mut self) {
        self.select_translator(TranslateMode::Free);
    }

    /// `I` — the AI model instead, offered only when `config.toml` set it up. There
    /// is nothing to enable at runtime, so say where the switch is.
    fn toggle_ai_translation(&mut self) {
        if !self.config.lyrics.ai_available() {
            self.notify("Set lyrics.ai-translation = true in config.toml for this");
            return;
        }
        self.select_translator(TranslateMode::Ai);
    }

    /// Turns `want` on, or off again when it is already what is showing.
    fn select_translator(&mut self, want: TranslateMode) {
        if self.config.lyrics.translate_to.is_empty() {
            self.notify("Set lyrics.translate-to in config.toml (e.g. \"zh\")");
            return;
        }
        self.translate_mode = if self.translate_mode == want {
            TranslateMode::Off
        } else {
            want
        };
        // Only the wrapped rows change: keep the scroll position and whether
        // the panel is following, so the view doesn't jump under the user.
        self.lyrics_rows = None;
        if self.translate_mode == TranslateMode::Off {
            self.notify("Translation off");
            return;
        }
        // Pressing the key again is also the retry, since a failure is
        // otherwise sticky for the rest of the session.
        if let Some(key) = self.current_translation_key()
            && matches!(self.translations.get(&key), Some(TranslationEntry::Failed))
        {
            self.translations.remove(&key);
        }
        self.notify(format!(
            "Translating to {} with {}",
            self.config.lyrics.translate_to,
            self.translator_name()
        ));
        self.ensure_translation();
    }

    /// What the badge and the toasts call the translator in use.
    fn translator_name(&self) -> String {
        if self.translate_mode == TranslateMode::Ai {
            self.config.lyrics.ai_model.clone()
        } else {
            "the free endpoint".to_string()
        }
    }

    /// Cache key for the record on screen under the current translator.
    fn current_translation_key(&self) -> Option<(u64, bool)> {
        let id = self.current_lyrics().map(|l| l.id)?;
        Some((id, self.translate_mode == TranslateMode::Ai))
    }

    /// Starts a translation for whatever record is on screen, unless one is
    /// cached or already in flight — the same one-shot shape as
    /// [`Self::ensure_lyrics`], and what makes `i` free to press twice.
    fn ensure_translation(&mut self) {
        if self.translate_mode == TranslateMode::Off || self.config.lyrics.translate_to.is_empty() {
            return;
        }
        let Some(found) = self.current_lyrics() else {
            return;
        };
        let record_id = found.id;
        let lines: Vec<String> = match &found.kind {
            ytm_core::LyricsKind::Synced(lines) => lines.iter().map(|l| l.text.clone()).collect(),
            ytm_core::LyricsKind::Plain(lines) => lines.clone(),
            // Nothing to translate, and no entry held either — an instrumental
            // that later resolves to a real record should still get one.
            ytm_core::LyricsKind::Instrumental => return,
        };
        if lines.iter().all(|l| l.trim().is_empty()) {
            return;
        }

        let ai = self.translate_mode == TranslateMode::Ai;
        let key = (record_id, ai);
        if self.translations.contains_key(&key) {
            return;
        }
        // Held for the session, oldest evicted first, so skipping back and
        // forth costs one translation a song rather than one a play.
        self.translation_order.retain(|&k| k != key);
        while self.translation_order.len() >= MAX_TRANSLATIONS {
            let oldest = self.translation_order.remove(0);
            self.translations.remove(&oldest);
        }
        self.translation_order.push(key);

        let backend = self.config.lyrics.backend(ai);
        // Only `I` has anything on disk to find: the free endpoint is re-asked
        // each session, so its translation can improve rather than being kept
        // for ever.
        if ai && let Some(done) = self.saved_translations.get(record_id, &backend.to) {
            log::debug!("translate: lrclib #{record_id} came from translations.json");
            self.translations
                .insert(key, TranslationEntry::Ready(done.to_vec()));
            self.lyrics_rows = None;
            return;
        }

        log::info!(
            "translate: {} lines of lrclib #{record_id} into {} via {}",
            lines.len(),
            backend.to,
            backend
                .ai
                .as_ref()
                .map_or("the free endpoint", |a| &a.model)
        );
        self.translations.insert(key, TranslationEntry::Loading);
        ytm_core::translate::spawn_translate(
            &self.lyrics_handle,
            record_id,
            lines,
            backend,
            self.translate_tx.clone(),
        );
    }

    /// `r` in lyrics mode with a translation on screen: forget it, on disk as
    /// well as in memory, and fetch another.
    fn retranslate(&mut self) {
        let Some(key) = self.current_translation_key() else {
            return;
        };
        // One in flight is enough: dropping it here would leave the first
        // request running and pay for a second answer to overwrite it.
        if matches!(self.translations.get(&key), Some(TranslationEntry::Loading)) {
            self.notify("Already translating…");
            return;
        }
        let (record_id, _) = key;
        self.translations.remove(&key);
        self.translation_order.retain(|&k| k != key);
        // Nothing on disk for the free translator, so this is a no-op there.
        if self.saved_translations.remove(record_id) {
            self.save_translations();
        }
        self.lyrics_rows = None;
        self.notify(format!("Re-translating with {}", self.translator_name()));
        self.ensure_translation();
    }

    /// Writes `translations.json`. A failure costs the next session a
    /// re-translation, which is not worth interrupting playback over.
    fn save_translations(&self) {
        if let Err(e) = persistence::save_translations(&self.saved_translations) {
            log::warn!("translate: couldn't write translations.json: {e}");
        }
    }

    /// Drain completed translations. Keyed by record, so a result that arrives
    /// after the user has skipped on is still worth keeping.
    fn drain_translations(&mut self) {
        while let Ok(TranslateMsg::Done {
            record_id,
            ai,
            result,
        }) = self.translate_rx.try_recv()
        {
            let on_screen = self.current_translation_key() == Some((record_id, ai));
            let entry = match result {
                Ok(done) => {
                    log::info!("translate: lrclib #{record_id} done");
                    // Kept only when the model answered. An AI request that
                    // fell back to the free endpoint is not what `I` bought, so
                    // it isn't stored and gets another go at the model.
                    if !done.model.is_empty() {
                        self.saved_translations.set(
                            record_id,
                            &self.config.lyrics.translate_to,
                            &done.model,
                            done.lines.clone(),
                        );
                        self.save_translations();
                    }
                    TranslationEntry::Ready(done.lines)
                }
                Err(e) => {
                    log::warn!("translate: lrclib #{record_id} failed: {e}");
                    // Said once, here, since the header has room for the fact
                    // but not the reason. Only for the record on screen — a
                    // result for a track the user has skipped past is noise.
                    if on_screen {
                        self.notify(format!("Translation failed: {e}"));
                    }
                    TranslationEntry::Failed
                }
            };
            self.translations.insert((record_id, ai), entry);
            // Rebuild the wrapped rows with the translation woven in — but
            // leave the scroll alone, since the user may have moved it while
            // waiting.
            if on_screen {
                self.lyrics_rows = None;
            }
        }
    }

    /// The translation to show under the on-screen record's lines, if it has
    /// arrived and a translator is selected.
    fn shown_translation(&self, record_id: u64) -> Option<&[String]> {
        let ai = match self.translate_mode {
            TranslateMode::Off => return None,
            TranslateMode::Ai => true,
            TranslateMode::Free => false,
        };
        match self.translations.get(&(record_id, ai)) {
            Some(TranslationEntry::Ready(lines)) => Some(lines),
            _ => None,
        }
    }

    /// The lyrics header's badge: the language, an `ai` mark when `I` chose the
    /// model, and how the fetch is getting on.
    fn translation_badge(&self, record_id: u64) -> Option<Span<'static>> {
        let ai = match self.translate_mode {
            TranslateMode::Off => return None,
            TranslateMode::Ai => true,
            TranslateMode::Free => false,
        };
        let lang = format!(
            "{}{}",
            self.config.lyrics.translate_to,
            if ai { " ai" } else { "" }
        );
        Some(match self.translations.get(&(record_id, ai)) {
            Some(TranslationEntry::Ready(_)) => {
                Span::styled(format!("⇄ {lang}"), theme::TRANSLATION)
            }
            Some(TranslationEntry::Failed) => {
                Span::styled(format!("⇄ {lang} failed"), theme::ERROR)
            }
            _ => Span::styled(format!("⇄ {lang}…"), theme::DIM),
        })
    }

    // ── MPRIS ─────────────────────────────────────────────────────────────────

    /// Acts on anything the desktop asked for — a media key, GNOME's player
    /// widget, `playerctl`. Commands are collected first because acting on one
    /// needs `&mut self` while the channel is borrowed from `self.media`.
    fn drain_media(&mut self) {
        let Some(media) = self.media.as_ref() else {
            return;
        };
        let mut cmds = Vec::new();
        while let Some(cmd) = media.try_recv() {
            cmds.push(cmd);
        }

        for cmd in cmds {
            log::debug!("[mpris] {cmd:?}");
            match cmd {
                MediaCmd::Play => self.player.resume(&self.library),
                MediaCmd::Pause => {
                    self.player.set_paused(true);
                }
                MediaCmd::PlayPause => self.player.play_pause(&self.library),
                MediaCmd::Stop => self.player.stop(),
                MediaCmd::Next => {
                    self.player.next(&self.library);
                    self.sync_queue_view();
                }
                // Same double-press gesture as `p`: the media key on a
                // keyboard is the same button, and this is what one does
                // everywhere else.
                MediaCmd::Previous => {
                    if self.player.restart_or_previous(&self.library) {
                        self.sync_queue_view();
                    }
                }
                MediaCmd::Seek(secs) => self.player.seek(secs),
                MediaCmd::SeekTo(secs) => self.player.seek_to(secs),
                MediaCmd::Volume(v) => self.player.set_volume(v),
                MediaCmd::Mode(mode) => self.player.set_mode(mode),
                // The same door SIGTERM uses: the loop checks this at the top
                // of every tick, so the usual save-on-exit path still runs.
                MediaCmd::Quit => ytm_core::shutdown::request_shutdown(),
            }
        }
    }

    /// Publishes the current state to the desktop. Called every tick; the
    /// diffing that decides whether anything reaches D-Bus lives in
    /// `MediaControls::update`.
    fn update_media(&mut self) {
        if self.media.is_none() {
            return;
        }
        let now = self.now_playing();
        if let Some(media) = self.media.as_mut() {
            media.update(&now);
        }
    }

    fn now_playing(&self) -> NowPlaying {
        let ast = self.player.audio_state();
        let playing = self.player.playing();

        let track = playing
            .and_then(|(pl, song)| self.library.track(pl, song))
            .map(|t| TrackInfo {
                id: t.video_id.clone().unwrap_or_default(),
                title: t.title.clone().unwrap_or_default(),
                artists: t.artists.iter().map(|a| a.name.clone()).collect(),
                album: t.album.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
                // mpv's duration is the accurate one but arrives a beat late,
                // so YouTube's stands in until it does.
                length: if ast.total > 0.0 {
                    ast.total
                } else {
                    t.duration_seconds.unwrap_or(0).into()
                },
            });

        // A queue restored from disk has a track but has never been handed to
        // mpv, so Stopped — not Paused — is the honest answer: `Play` starts
        // it from the beginning, which is exactly what `start_current` does.
        let state = if playing.is_none() || !self.player.playback_started() {
            PlayState::Stopped
        } else if ast.paused {
            PlayState::Paused
        } else {
            PlayState::Playing
        };

        let queued = !self.player.queue().is_empty();
        NowPlaying {
            state,
            track,
            mode: self.player.mode(),
            volume: self.player.volume(),
            // The queue wraps, so there is always a next once it is non-empty.
            can_go_next: queued,
            can_go_previous: queued,
            can_play: queued || playing.is_some(),
            can_seek: !ast.loading && ast.total > 0.0,
            position: ast.elapsed,
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

    pub fn run(mut self) -> anyhow::Result<Exit> {
        use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};

        // An expired session looks, from here, like an account with no
        // playlists. Where the renewal is silent the caller does it and starts
        // us again, so there is nothing worth drawing first — the TUI never
        // appears, rather than appearing empty for the time yt-dlp takes.
        // Returning before `init` also leaves the saved queue alone: it has
        // not been restored yet, and saving it back now would be saving
        // nothing over it.
        if self.auto_reauth && self.library.is_empty() {
            log::info!("no playlists and a browser on record — renewing without asking");
            return Ok(Exit::Reauth);
        }

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
        // The renewal itself belongs to `main.rs`: it is what builds a session
        // from it and starts everything again, so the app comes back rather
        // than ending with something for the user to run.
        Ok(if self.reauth_requested {
            Exit::Reauth
        } else {
            Exit::Quit
        })
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
            self.drain_translations();
            self.drain_media();
            self.drain_search();
            self.drain_covers();
            self.ensure_cover();
            // Kicked off from here rather than on each key: this one lookup
            // covers entering lyrics mode, p/n, auto-advance and Enter alike.
            if self.lyrics_mode
                && let Some(id) = self.current_video_id()
            {
                self.ensure_lyrics(&id);
                // Only once the record has arrived — which is why this can't
                // hang off the `i` keypress alone.
                self.ensure_translation();
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
            // After the frame: the terminal composites the image over the cell
            // grid, so it has to be placed onto a grid that is already there.
            self.draw_cover();
            if self.player.handle_song_end(&self.library) {
                self.sync_queue_view();
            }
            // After the auto-advance above, so a track change reaches the
            // desktop on the same tick the UI shows it.
            self.update_media();
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

                            // ── so does the search panel, while it has focus ──────────
                            // Not unconditionally: with focus moved to the
                            // playlists the ordinary bindings apply, so `h`
                            // leaves and `j`/`k` walk the playlists — the same
                            // way lyrics mode behaves. Typing a query and the
                            // add popup still take every key, or `h` would
                            // leave mid-word.
                            _ if self.search_has_focus() => {
                                if self.handle_search_key(key.code) {
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
                                        // Copied out, so the cached filter is
                                        // no longer borrowed when the player
                                        // takes `&mut self`.
                                        let q_pos = self
                                            .filtered_queue_positions()
                                            .get(display_idx)
                                            .copied();
                                        if let Some(q_pos) = q_pos {
                                            self.player.jump_to(&self.library, q_pos);
                                        }
                                    }
                                }
                                Panel::Songs => {
                                    if let (Some(pl), Some(display_idx)) =
                                        (self.list_state.selected(), self.songs_state.selected())
                                    {
                                        let song =
                                            self.filtered_songs(pl).get(display_idx).copied();
                                        if let Some(song) = song {
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
                            // Resumes, pauses, or — for a queue restored from
                            // saved state — starts playback.
                            KeyCode::Char(' ') => self.player.play_pause(&self.library),
                            // First press restarts the track; a second within
                            // half a second goes back one instead.
                            KeyCode::Char('p') => {
                                if self.player.restart_or_previous(&self.library) {
                                    self.sync_queue_view();
                                }
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
                                    let song = self.filtered_songs(pl).get(display_idx).copied();
                                    if let Some(song) = song {
                                        self.do_append_to_queue(pl, song);
                                    }
                                }
                            }
                            KeyCode::Char('d')
                                if self.active_panel == Panel::Songs && self.show_queue =>
                            {
                                if let Some(display_idx) = self.queue_view_state.selected() {
                                    let q_pos =
                                        self.filtered_queue_positions().get(display_idx).copied();
                                    if let Some(q_pos) = q_pos {
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
                            KeyCode::Left => self.player.seek(-5.0),
                            KeyCode::Right => self.player.seek(5.0),
                            // ── volume ────────────────────────────────────────────────
                            KeyCode::Up => self.player.adjust_volume(5),
                            KeyCode::Down => self.player.adjust_volume(-5),
                            KeyCode::Char('s') => self.toggle_search(),
                            KeyCode::Char('?') => self.show_keymap = true,
                            // ── lyrics ────────────────────────────────────────────────
                            KeyCode::Char('y') => self.toggle_lyrics_mode(),
                            KeyCode::Char('c') if self.lyrics_mode => self.open_lyrics_picker(),
                            KeyCode::Char('r') if self.lyrics_mode => self.retry_lyrics(),
                            KeyCode::Char('i') if self.lyrics_mode => self.toggle_translation(),
                            KeyCode::Char('I') if self.lyrics_mode => {
                                self.toggle_ai_translation();
                            }
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
                            // Ahead of the re-auth binding below: a single
                            // playlist that failed is not an expired session,
                            // and quitting to re-authenticate would be a
                            // remarkable answer to one that can just be asked
                            // for again.
                            KeyCode::Char('r')
                                if self
                                    .list_state
                                    .selected()
                                    .is_some_and(|pl| self.library.has_failed(pl)) =>
                            {
                                self.retry_playlist();
                            }
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
            KeyCode::Left => self.player.seek(-5.0),
            KeyCode::Right => self.player.seek(5.0),
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

    /// Whether `track` matches the already-lowercased query `q`.
    fn matches_filter(track: Option<&Track>, q: &str) -> bool {
        let Some(track) = track else {
            return false;
        };
        track
            .title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(q))
            || track.artist_names().to_lowercase().contains(q)
    }

    /// Original song indices in the selected playlist that match the current
    /// filter. All of them when the filter is empty.
    ///
    /// Memoised because it is called once per frame *and* once per keystroke,
    /// while its answer can only change when the playlist, the query or the
    /// playlist's length does. Recomputing it lowercases every title and
    /// artist in the playlist, which for a few thousand tracks at lyric-mode
    /// frame rates is thousands of allocations a second for an answer that
    /// never changed.
    #[hotpath::measure]
    fn filtered_songs(&mut self, pl: usize) -> &[usize] {
        let len = self.library.songs(pl).len();
        let fresh = self
            .songs_filter
            .as_ref()
            .is_some_and(|(p, q, n, _)| *p == pl && *n == len && q == &self.filter);

        if !fresh {
            let list = if self.filter.is_empty() {
                (0..len).collect()
            } else {
                let q = self.filter.to_lowercase();
                self.library
                    .songs(pl)
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| Self::matches_filter(Some(t), &q))
                    .map(|(i, _)| i)
                    .collect()
            };
            self.songs_filter = Some((pl, self.filter.clone(), len, list));
        }
        self.songs_filter.as_ref().map_or(&[], |(_, _, _, list)| {
            let slice: &[usize] = list;
            slice
        })
    }

    /// Queue positions whose songs match the current filter. All of them when
    /// the filter is empty. Memoised like [`App::filtered_songs`], against the
    /// queue's revision rather than its length — shuffling reorders it without
    /// changing how long it is.
    fn filtered_queue_positions(&mut self) -> &[usize] {
        let revision = self.player.queue_revision();
        let fresh = self
            .queue_filter
            .as_ref()
            .is_some_and(|(r, q, _)| *r == revision && q == &self.filter);

        if !fresh {
            let list = if self.filter.is_empty() {
                (0..self.player.queue().len()).collect()
            } else {
                let q = self.filter.to_lowercase();
                self.player
                    .queue()
                    .iter()
                    .enumerate()
                    .filter(|&(_, &(pl, song_idx))| {
                        Self::matches_filter(self.library.track(pl, song_idx), &q)
                    })
                    .map(|(i, _)| i)
                    .collect()
            };
            self.queue_filter = Some((revision, self.filter.clone(), list));
        }
        self.queue_filter.as_ref().map_or(&[], |(_, _, list)| {
            let slice: &[usize] = list;
            slice
        })
    }

    /// Prefetch whichever song is currently highlighted in the Songs panel (plus the
    /// one after it). Called on every j/k movement so the CDN URL is warm by the
    /// time the user presses Enter.
    fn prefetch_selected(&mut self) {
        let Some(pl) = self.list_state.selected() else {
            return;
        };
        let base = self.songs_state.selected().unwrap_or(0);
        // Resolved to real indices first: the borrow of the cached filter has
        // to end before `self.library` and `self.player` are read below.
        let filtered = self.filtered_songs(pl);
        let wanted: Vec<usize> = [base, base + 1]
            .iter()
            .filter_map(|&i| filtered.get(i).copied())
            .collect();

        let songs = self.library.songs(pl);
        for real_idx in wanted {
            if let Some(id) = songs.get(real_idx).and_then(|t| t.video_id.as_deref()) {
                self.player.prefetch(id);
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notification = Some((msg.into(), Instant::now()));
    }

    // ── help / notification bar ───────────────────────────────────────────────

    /// The keys that work everywhere the picker isn't, in the order they are
    /// worth dropping. Appended to each context's own so the bar can name every
    /// binding that context has; `fit_hints` decides how much of it fits.
    const TAIL: &'static [(&'static str, &'static str)] = &[
        ("←/→", "seek"),
        ("↑/↓", "volume"),
        ("m", "mute"),
        ("t", "mode"),
        ("h/l", "panel"),
        ("q", "quit"),
    ];

    /// The picker is modal — only these keys reach it, so it gets no tail.
    fn picker_hints() -> Vec<(&'static str, &'static str)> {
        vec![
            ("j/k", "select"),
            ("↵", "use"),
            ("Esc", "cancel"),
            ("c", "close"),
            ("spc", "pause"),
            ("←/→", "seek"),
            ("q", "quit"),
        ]
    }

    /// `ai` is [`Lyrics::ai_available`]: `I` is named only where it does
    /// something, since the paid path shouldn't look like a key you can press.
    fn lyrics_hints(ai: bool) -> Vec<(&'static str, &'static str)> {
        let mut keys = vec![("y", "close"), ("c", "source"), ("i", "translate")];
        if ai {
            keys.push(("I", "ai"));
        }
        keys.extend_from_slice(&[
            ("r", "redo"),
            // Early enough to survive 80 columns: it is the way to everything
            // below it. `the_way_to_the_full_keymap_survives_a_narrow_terminal`
            // is what says how early that is.
            ("?", "keys"),
            ("j/k", "scroll"),
            ("PgUp/PgDn", "page"),
            ("Esc", "re-centre"),
            ("spc", "pause"),
            ("p/n", "skip"),
            ("o", "queue"),
        ]);
        keys.extend_from_slice(Self::TAIL);
        keys
    }

    /// The search panel. Two contexts really — typing a query, and moving
    /// through what came back — and the keys differ enough that showing both
    /// at once would be a lie about half of them.
    fn search_hints(typing: bool) -> Vec<(&'static str, &'static str)> {
        if typing {
            return vec![
                ("↵", "search"),
                ("Esc", "close"),
                ("?", "keys"),
                ("spc", "pause"),
            ];
        }
        let mut keys = vec![
            ("↵", "play"),
            ("a", "add to…"),
            ("/", "edit query"),
            ("s", "close"),
            ("Esc", "back"),
            ("?", "keys"),
            ("j/k", "select"),
            ("h/l", "panel"),
            ("spc", "pause"),
            ("p/n", "skip"),
            ("←/→", "seek"),
            ("m", "mute"),
            ("q", "quit"),
        ];
        keys.dedup();
        keys
    }

    fn browse_hints(panel: Panel, queue: bool) -> Vec<(&'static str, &'static str)> {
        let mut keys = match (panel, queue) {
            (Panel::Playlists, _) => vec![
                ("j/k", "nav"),
                ("l/↵", "open"),
                ("spc", "pause"),
                ("p/n", "skip"),
                ("o", "queue"),
                ("?", "keys"),
                ("y", "lyrics"),
                ("s", "search"),
            ],
            (Panel::Songs, false) => vec![
                ("↵", "play"),
                ("spc", "pause"),
                ("/", "filter"),
                ("a", "+queue"),
                ("o", "queue"),
                ("?", "keys"),
                ("y", "lyrics"),
                ("s", "search"),
                ("p/n", "skip"),
                ("j/k", "nav"),
                ("Esc", "back"),
            ],
            (Panel::Songs, true) => vec![
                ("↵", "play"),
                ("spc", "pause"),
                ("d", "remove"),
                ("o", "songs"),
                ("y", "lyrics"),
                ("?", "keys"),
                ("s", "search"),
                ("p/n", "skip"),
                ("j/k", "nav"),
                ("/", "filter"),
                ("Esc", "back"),
            ],
        };
        keys.extend_from_slice(Self::TAIL);
        keys
    }

    /// Every hint for the current context, most useful first — `fit_hints` drops
    /// from the end when the terminal is too narrow, so the order is the
    /// priority order. `?` sits ahead of the tail because it is the way to the
    /// rest of them on a terminal too narrow to show any.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.lyrics_picker.is_some() {
            Self::picker_hints()
        } else if self.search_has_focus() {
            Self::search_hints(self.search.as_ref().is_some_and(|s| s.typing))
        } else if self.lyrics_mode {
            Self::lyrics_hints(self.config.lyrics.ai_available())
        } else {
            Self::browse_hints(self.active_panel, self.show_queue)
        }
    }

    fn render_help(&mut self, frame: &mut Frame, area: Rect) {
        let line = if self.filter_mode {
            let mut spans = vec![
                Span::styled(format!("/{}", self.filter), theme::WARN),
                Span::styled("█", theme::WARN),
            ];
            // Saturating: a filter query can be longer than the terminal is
            // wide, and the room left for hints is then none rather than a
            // subtraction that underflows.
            spans.extend(fit_hints(
                &[("↵", "confirm"), ("Esc", "cancel")],
                (area.width as usize).saturating_sub(width_of(&self.filter) + 2),
            ));
            Line::from(spans)
        } else if let Some((msg, _)) = &self.notification {
            Line::from(vec![
                Span::styled("✓ ", theme::SUCCESS),
                Span::styled(msg.clone(), theme::SUCCESS),
            ])
        } else {
            Line::from(fit_hints(&self.hints(), area.width as usize))
        };

        frame.render_widget(Paragraph::new(line), area);
    }

    /// Full keymap overlay, opened with `?`. Every one of these is in some
    /// context's hint bar too — `the_keymap_and_the_hint_bar_agree` checks it —
    /// but only here are they all visible at once, and described rather than
    /// abbreviated. A blank pair is a spacer.
    const KEYMAP: &'static [(&'static str, &'static str)] = &[
        ("j / k", "Move down / up · scroll lyrics"),
        ("PgUp / PgDn", "Scroll lyrics by five"),
        ("h / l", "Switch panel"),
        ("↵", "Open playlist · play song"),
        ("/", "Filter by title or artist"),
        ("Esc", "Clear filter · back · close"),
        ("", ""),
        ("space", "Pause / resume"),
        ("p", "Restart track · again for previous"),
        ("n", "Next in queue"),
        ("← / →", "Seek ∓5s"),
        ("↑ / ↓", "Volume ±5"),
        ("m", "Mute / unmute"),
        ("t", "Cycle play mode"),
        ("", ""),
        ("a", "Add selected song to queue"),
        ("d", "Remove selected queue entry"),
        ("o", "Toggle queue / songs"),
        ("", ""),
        ("s", "Search YouTube Music"),
        ("a", "In search: add the result to a playlist"),
        ("", ""),
        ("y", "Toggle lyrics"),
        ("c", "Choose lyrics source (in lyrics)"),
        ("i", "Toggle translation (in lyrics)"),
        ("I", "Translate with the AI model instead"),
        ("r", "Redo (translation or lyrics)"),
        ("", ""),
        ("?", "Close this help"),
        ("q  ·  Ctrl+C", "Quit"),
    ];

    fn render_keymap(&self, frame: &mut Frame, screen: Rect) {
        const KEYS: &[(&str, &str)] = App::KEYMAP;

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

        // Cleared here and claimed by whichever panel wants it below, so a
        // cover can never outlive the panel that asked for it.
        self.cover_target = None;

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

    /// The left column while the lyrics panel is open: the cover, then the
    /// track under it.
    ///
    /// The playlist list is not much use with lyrics on screen — you are
    /// reading, not browsing — and the column is the one piece of the layout
    /// wide enough to hold a square picture without taking room from the words.
    ///
    /// Follows the same chrome as everything else: an uppercase header, a rule,
    /// and no border. The art is the only thing on screen with a shape of its
    /// own, so the text under it is centred to sit with it, and graded down —
    /// title, then artist, then album — so the eye lands on the title first.
    fn render_now_playing_card(&mut self, frame: &mut Frame, area: Rect) {
        let status = self
            .player
            .playing()
            .and_then(|(pl, _)| self.library.playlist(pl))
            .filter(|p| p.playlist_id != Library::SEARCH_PLAYLIST_ID)
            .map(|p| Line::styled(truncate_line(&p.title, 18), theme::DIM));
        let body = section(frame, area, "Now playing", status, false);
        self.cover_target = None;
        if body.height == 0 || body.width == 0 {
            return;
        }

        let Some(track) = self
            .player
            .playing()
            .and_then(|(pl, song)| self.library.track(pl, song))
        else {
            centered_message(
                frame,
                body,
                vec![Line::styled("Nothing playing", theme::DIM)],
            );
            return;
        };

        // A square of cells: they are about twice as tall as wide, so a cover
        // `n` columns across needs `n / 2` rows. Bounded by both, and by a
        // ceiling — a cover taller than the column leaves nothing for the text.
        let side = u16::min(body.width.saturating_sub(2), (body.height / 2).max(1) * 2);
        let side = side.min(MAX_COVER_COLS) / 2 * 2; // even, so the halves land on cells
        let (cover_w, cover_h) = (side, side / 2);
        let can_draw = self.covers_enabled && cover_w >= 8 && body.height > cover_h + 4;

        // The words, built before anything is placed: the card is centred as a
        // whole, so where the cover goes depends on how many lines follow it.
        // Each gets exactly one line — a long title truncates rather than
        // pushing the album off the bottom.
        let width = body.width as usize;
        // Green, because green is what this app means by "playing" — the same
        // colour the songs list marks the current row with.
        let mut lines = vec![
            Line::styled(
                truncate_line(track.title.as_deref().unwrap_or("Unknown"), width),
                theme::PLAYING,
            )
            .centered(),
        ];
        let artist = track.artist_names();
        if !artist.is_empty() {
            lines.push(Line::styled(truncate_line(&artist, width), theme::META).centered());
        }
        if let Some(album) = track.album.as_ref().map(|a| a.name.as_str())
            && !album.is_empty()
        {
            lines.push(Line::styled(truncate_line(album, width), theme::DIM).centered());
        }
        // A rule as wide as the art, then the length — the one number worth
        // having here, and it keeps the block from ending on a ragged edge.
        if let Some(duration) = track_duration(track) {
            lines.push(Line::from(""));
            lines.push(
                Line::styled(
                    symbols::line::NORMAL
                        .horizontal
                        .repeat(cover_w.max(8) as usize / 2),
                    theme::RULE,
                )
                .centered(),
            );
            lines.push(Line::styled(duration, theme::DIM).centered());
        }

        // Cover, a blank row, then the words — placed as one block and centred
        // in the column rather than hung from its top. Pinned up there it reads
        // as a rendering accident, art or no art, and the column is otherwise
        // empty: in lyrics mode this panel is something to glance at, not a
        // list to run down.
        let card_h = if can_draw { cover_h + 1 } else { 0 } + lines.len() as u16;
        let mut y = body.y + body.height.saturating_sub(card_h) / 2;

        if can_draw {
            let rect = Rect {
                x: body.x + (body.width.saturating_sub(cover_w)) / 2,
                y,
                width: cover_w,
                height: cover_h,
            };
            // Claimed whether or not the picture has arrived: the space is
            // reserved by the layout either way, and a cover appearing without
            // moving the text under it is the point.
            if let Some(id) = track.video_id.clone() {
                self.cover_target = Some((id, rect));
            }
            // A placeholder in the same square while it loads, so the column
            // isn't briefly empty on every track change.
            if self
                .cover_target
                .as_ref()
                .is_none_or(|(id, _)| !self.covers.contains_key(id))
            {
                centered_message(frame, rect, vec![Line::styled("♪", theme::DIM)]);
            }
            y += cover_h + 1;
        }

        let text_area = Rect {
            y,
            height: body.height.saturating_sub(y - body.y),
            ..body
        };
        if text_area.height == 0 {
            return;
        }
        frame.render_widget(Paragraph::new(lines), text_area);
    }

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        // Lyrics mode gives this column to the cover instead — see
        // `render_now_playing_card`.
        if self.lyrics_mode {
            self.render_now_playing_card(frame, area);
            return;
        }
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

        // Cursor gutter, column spacing and the count column, as in the track
        // lists — a playlist name is no more readable clipped than a title is.
        let name_w = (list_body(body, count).width as usize).saturating_sub(2 + 1 + 4);
        let rows: Vec<Row> = self
            .library
            .entries()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let playing = self.player.playing().is_some_and(|(pl, _)| pl == i);
                Row::new([
                    Cell::from(Span::styled(
                        truncate_line(&entry.playlist.title, name_w),
                        if playing {
                            theme::PLAYING
                        } else {
                            theme::PRIMARY
                        },
                    )),
                    // A playlist that failed to load would otherwise sit here
                    // reading "0", which is a lie about the playlist rather
                    // than a fact about the network.
                    Cell::from(if self.library.has_failed(i) {
                        Line::styled("!", theme::ERROR).alignment(Alignment::Right)
                    } else {
                        Line::styled(self.library.songs(i).len().to_string(), theme::DIM)
                            .alignment(Alignment::Right)
                    }),
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

    // ── search panel ──────────────────────────────────────────────────────────

    /// The search panel: query line, results, and a cover for the highlighted
    /// row where the terminal can draw one.
    fn render_search(&mut self, frame: &mut Frame, area: Rect) {
        // While a query is being typed the panel owns the keyboard whatever
        // `active_panel` says, so it is focused by definition; once there are
        // results to move through, focus is the ordinary `h`/`l` business.
        let focused = self.search_has_focus();
        let Some(search) = self.search.as_ref() else {
            return;
        };

        // The header carries the query, so the results keep every row.
        let mut query = vec![
            Span::styled("/", theme::WARN),
            Span::styled(search.query.clone(), theme::WARN),
        ];
        if search.typing {
            query.push(Span::styled("█", theme::WARN));
        }
        if !search.results.is_empty() {
            // `  ·  ` like every other section status, rather than a bare gap.
            query.push(Span::styled(SEP, theme::DIM));
            query.push(Span::styled(
                format!("{} results", search.results.len()),
                theme::DIM,
            ));
        }
        let body = section(frame, area, "Search", Some(Line::from(query)), focused);
        if body.height == 0 || body.width == 0 {
            self.cover_target = None;
            return;
        }

        if search.loading {
            self.cover_target = None;
            frame.render_stateful_widget(
                Throbber::default()
                    .label(" Searching YouTube Music…")
                    .throbber_style(theme::ACCENT),
                body,
                &mut self.throbber_state,
            );
            return;
        }

        if let Some(err) = &search.error {
            self.cover_target = None;
            centered_message(
                frame,
                body,
                vec![
                    Line::styled("Search failed", theme::ERROR),
                    Line::from(""),
                    Line::styled(truncate_line(err, body.width as usize), theme::ERROR_BODY),
                    Line::from(""),
                    Line::from(hint("↵", "try again")),
                ],
            );
            return;
        }

        if search.results.is_empty() {
            self.cover_target = None;
            let msg = if search.ran.is_empty() {
                vec![
                    Line::styled("Type a query, then press ↵", theme::DIM),
                    Line::from(""),
                    Line::styled(
                        "Songs and videos both — some tracks only exist as a video.",
                        theme::DIM,
                    ),
                ]
            } else {
                vec![Line::styled(
                    format!("Nothing found for /{}", search.ran),
                    theme::WARN,
                )]
            };
            centered_message(frame, body, msg);
            return;
        }

        // A cover column only where there is a terminal to draw into and room
        // to do it — below this the list is worth more than the picture.
        const COVER_COLS: u16 = 20;
        let show_cover = self.covers_enabled && body.width > COVER_COLS + 30 && body.height >= 12;
        let (list_area, cover_area) = if show_cover {
            let [list, gap, cover] = body.layout(&Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(2),
                Constraint::Length(COVER_COLS),
            ]));
            let _ = gap;
            // Square-ish: cells are about twice as tall as they are wide.
            let side = Rect {
                height: (COVER_COLS / 2).min(cover.height),
                ..cover
            };
            (list, Some(side))
        } else {
            (body, None)
        };
        self.cover_target =
            cover_area.and_then(|rect| Some((search.selected()?.video_id.clone(), rect)));

        // The details under the cover, which the image must not overlap.
        if let (Some(cover), Some(hit)) = (cover_area, search.selected()) {
            let below = Rect {
                y: cover.y + cover.height + 1,
                height: body.height.saturating_sub(cover.height + 1),
                ..cover
            };
            if below.height > 0 {
                // Centred under the cover, as the now-playing card is: the art
                // is the only thing here with a shape, so the words sit with it.
                //
                // Wrapped rather than cut, because this column *is* the detail
                // view — a search list already shows a truncated title, and a
                // panel whose whole job is to say more about the highlighted
                // row saying the same amount less is no use. The caps keep the
                // kind and length below from being pushed off a short panel by
                // a long title; each is what the field is ever plausibly worth.
                let width = below.width as usize;
                let mut lines: Vec<Line> = Vec::new();
                for (text, style, max) in [
                    (hit.title.as_str(), theme::PRIMARY, 3),
                    (hit.artist.as_str(), theme::META, 2),
                    (hit.album.as_str(), theme::DIM, 2),
                ] {
                    if text.is_empty() {
                        continue;
                    }
                    lines.extend(
                        wrap_words(text, width, max)
                            .into_iter()
                            .map(|piece| Line::styled(piece, style).centered()),
                    );
                }
                let (marker, style) = kind_marker(hit.kind);
                lines.push(Line::from(""));
                lines.push(
                    Line::from(vec![
                        Span::styled(marker.trim().to_string(), style),
                        Span::styled(
                            if hit.duration.is_empty() {
                                String::new()
                            } else {
                                format!("{SEP}{}", hit.duration)
                            },
                            theme::DIM,
                        ),
                    ])
                    .centered(),
                );
                // Only when it isn't obvious from the marker: an art track is
                // the catalogue audio, which is not what "video" promises.
                if hit.kind == ResultKind::Video {
                    lines.push(Line::styled("audio only", theme::DIM).centered());
                }
                frame.render_widget(Paragraph::new(lines), below);
            }
        }

        // Cursor gutter, two column gaps, the kind marker and the duration.
        let text_w = (list_body(list_area, search.results.len()).width as usize)
            .saturating_sub(2 + 7 + 1 + 7 + 1);
        let rows: Vec<Row> = search
            .results
            .iter()
            .map(|hit| {
                let (marker, marker_style) = kind_marker(hit.kind);
                let spans = fit_meta(
                    &hit.title,
                    theme::PRIMARY,
                    &[
                        (hit.artist.clone(), theme::META),
                        (hit.album.clone(), theme::DIM),
                    ],
                    text_w,
                );
                Row::new(vec![
                    Cell::from(Line::styled(marker, marker_style)),
                    Cell::from(Line::from(spans)),
                    Cell::from(Line::styled(hit.duration.clone(), theme::DIM).right_aligned()),
                ])
            })
            .collect();

        let n = rows.len();
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(7),
                    Constraint::Fill(1),
                    // Seven, not six: the video results routinely include an
                    // hour-long upload, and `1:03:04` does not fit in six.
                    Constraint::Length(7),
                ],
            )
            .row_highlight_style(if focused {
                theme::SELECTED
            } else {
                theme::SELECTED_BLUR
            })
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always)
            .column_spacing(1),
            list_body(list_area, n),
            &mut self.search.as_mut().expect("checked above").state,
        );
        render_scrollbar(
            frame,
            list_area,
            n,
            self.search.as_ref().and_then(|s| s.state.selected()),
        );
    }

    /// The `a` popup: which of the user's libraries to add the highlighted
    /// result to.
    fn render_add_picker(&mut self, frame: &mut Frame, area: Rect) {
        let targets: Vec<String> = self
            .add_targets()
            .into_iter()
            .map(|(_, title)| title.to_string())
            .collect();
        let counts: Vec<usize> = self
            .add_targets()
            .into_iter()
            .map(|(i, _)| self.library.songs(i).len())
            .collect();
        let title = self
            .search
            .as_ref()
            .and_then(SearchState::selected)
            .map(|h| h.title.clone())
            .unwrap_or_default();

        let Some(state) = self.search.as_mut().and_then(|s| s.add.as_mut()) else {
            return;
        };

        let modal = area.centered(
            Constraint::Length(area.width.saturating_sub(4).clamp(30, 54)),
            Constraint::Length((targets.len() as u16 + 4).clamp(6, area.height.saturating_sub(2))),
        );
        let block = Block::bordered()
            .title(Line::from(vec![
                Span::styled(" Add ", theme::HEADER),
                Span::styled(truncate_line(&title, 28), theme::PRIMARY),
                Span::styled(" to ", theme::HEADER),
            ]))
            .title_bottom(Line::from(fit_hints(
                &[("j/k", "select"), ("↵", "add"), ("Esc", "cancel")],
                modal.width.saturating_sub(4) as usize,
            )))
            .border_style(theme::RULE)
            .padding(Padding::horizontal(1));

        // Without this the search results bleed through the modal.
        frame.render_widget(Clear, modal);

        let rows: Vec<Row> = targets
            .iter()
            .zip(&counts)
            .map(|(name, count)| {
                Row::new(vec![
                    Cell::from(Line::styled(name.clone(), theme::PRIMARY)),
                    Cell::from(Line::styled(count.to_string(), theme::DIM).right_aligned()),
                ])
            })
            .collect();

        frame.render_stateful_widget(
            Table::new(rows, [Constraint::Fill(1), Constraint::Length(5)])
                .block(block)
                .row_highlight_style(theme::SELECTED)
                .highlight_symbol("▶ ")
                .column_spacing(1),
            modal,
            state,
        );
    }

    fn render_right_panel(&mut self, frame: &mut Frame, area: Rect) {
        // Search takes the whole right column, like lyrics do. The cover is
        // drawn onto it afterwards, outside ratatui — see `draw_cover`.
        if self.search.is_some() {
            self.render_search(frame, area);
            if self.search.as_ref().is_some_and(|s| s.add.is_some()) {
                self.render_add_picker(frame, area);
            }
            return;
        }

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
            spans.push(Span::styled(SEP, theme::DIM));
            spans.push(Span::styled(format!("{shown}/{total}"), theme::DIM));
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
        width: usize,
    ) -> Row<'static> {
        let title = track
            .and_then(|t| t.title.as_deref())
            .unwrap_or("Unknown")
            .to_string();
        let artists = track.map(Track::artist_names).unwrap_or_default();

        let mut spans = vec![
            Span::styled(if playing { "♫ " } else { "  " }, theme::PLAYING),
            Span::styled(format!("{number:>num_w$}  "), theme::DIM),
        ];
        // What the marker and the number have already taken.
        let budget = width.saturating_sub(num_w + 4);
        spans.extend(fit_meta(
            &title,
            if playing {
                theme::PLAYING
            } else {
                theme::PRIMARY
            },
            &[(artists, theme::META)],
            budget,
        ));

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

    /// Cells left for a track row's text, once the cursor gutter, the column
    /// spacing and the duration have taken theirs. Computed rather than
    /// guessed, so the ellipsis lands exactly where the clip used to.
    fn track_text_width(area: Rect) -> usize {
        (area.width as usize).saturating_sub(2 + 1 + 8)
    }

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
                    // No badge: there are no words, so nothing is being
                    // translated however the mode is set.
                    let status = Self::lyrics_status(found, None, None, None);
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
        translation: Option<Span<'static>>,
        offset: Option<String>,
    ) -> Line<'static> {
        let mut spans = Vec::new();
        if let Some(badge) = badge {
            spans.push(badge);
            spans.push(Span::styled(SEP, theme::DIM));
        }
        if let Some(translation) = translation {
            spans.push(translation);
            spans.push(Span::styled(SEP, theme::DIM));
        }
        if let Some(offset) = offset {
            spans.push(Span::styled(format!("offset {offset}"), theme::WARN));
            spans.push(Span::styled(SEP, theme::DIM));
        }
        // An em-dash, not the `·` separator: the record's title and artist are
        // one field between them, and `·` is what divides fields from each
        // other. Flattening the two reads as four peers instead of three.
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
            // Both cloned up front so the borrows of `self` end before the
            // rows are stored back into it.
            let (texts, translation): (Vec<String>, Vec<String>) =
                match self.lyrics_cache.get(video_id) {
                    Some(LyricsEntry::Ready(found)) => {
                        let texts = match &found.kind {
                            ytm_core::LyricsKind::Synced(lines) => {
                                lines.iter().map(|l| l.text.clone()).collect()
                            }
                            ytm_core::LyricsKind::Plain(lines) => lines.clone(),
                            ytm_core::LyricsKind::Instrumental => Vec::new(),
                        };
                        let translation = self.shown_translation(found.id).unwrap_or(&[]).to_vec();
                        (texts, translation)
                    }
                    _ => (Vec::new(), Vec::new()),
                };

            let rows = lyric_rows(&texts, &translation, width);
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
                self.translation_badge(found.id),
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
        let status =
            Self::lyrics_status(found, Some(badge), self.translation_badge(found.id), None);

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
            .map(|r| {
                let style = if r.translated {
                    theme::TRANSLATION_DIM
                } else {
                    theme::META
                };
                Line::styled(r.text.clone(), style)
            })
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

        // Refreshed first, on its own, so the `&mut self` it needs is over
        // before the panel starts reading `self` to draw itself. Everything
        // below reads the cache back through the field.
        if let Some(pl) = current_pl {
            self.filtered_songs(pl);
        }

        // The playlist's own name and stats become this section's header —
        // the old bordered "Info" and "Track" boxes duplicated the list below
        // and cost five rows to say it.
        let entry = current_pl.and_then(|i| self.library.entry(i));
        let label = entry
            .map_or("Songs", |e| e.playlist.title.as_str())
            .to_string();

        let all_songs = current_pl.map_or(&[][..], |i| self.library.songs(i));
        let filtered: &[usize] = match (current_pl, &self.songs_filter) {
            (Some(_), Some((_, _, _, list))) => list,
            _ => &[],
        };

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

        // Before the loading throbber: a failed playlist is also "not loaded",
        // and spinning at the user for ever is the one thing it must not do.
        if current_pl.is_some_and(|i| self.library.has_failed(i)) {
            centered_message(
                frame,
                body,
                vec![
                    Line::styled("Couldn't load this playlist", theme::ERROR),
                    Line::from(""),
                    Line::styled(
                        "YouTube didn't answer after three tries.",
                        theme::ERROR_BODY,
                    ),
                    Line::from(""),
                    Line::from(hint("r", "try again")),
                ],
            );
            return;
        }

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
        // The scrollbar takes two columns when the list overflows, so the
        // width a row may use depends on how many rows there are.
        let width = Self::track_text_width(list_body(body, filtered.len()));
        let rows: Vec<Row> = filtered
            .iter()
            .map(|&i| {
                self.track_row(
                    Some(&all_songs[i]),
                    i + 1,
                    num_w,
                    current_pl.map(|pl| (pl, i)) == playing,
                    width,
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
        // As in `render_songs`: refresh under `&mut self`, then read the cache
        // back through the field so the rest of the draw can borrow `self`.
        self.filtered_queue_positions();
        let filtered: &[usize] = self
            .queue_filter
            .as_ref()
            .map_or(&[], |(_, _, list)| list.as_slice());

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
        let width = Self::track_text_width(list_body(body, filtered.len()));
        let rows: Vec<Row> = filtered
            .iter()
            .map(|&q_pos| {
                let (pl, song_idx) = queue[q_pos];
                self.track_row(
                    self.library.track(pl, song_idx),
                    q_pos + 1,
                    num_w,
                    Some(q_pos) == queue_pos,
                    width,
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
            spans.extend(fit_meta(
                &title_text,
                if ast.paused {
                    theme::DIM
                } else {
                    theme::PRIMARY
                },
                &[(artist_text.unwrap_or_default(), theme::META)],
                budget,
            ));
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
        (
            title,
            artist,
            fmt_secs(ast.elapsed),
            fmt_secs_rounded(ast.total),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a cover is asked for and kept at, which is the whole of how sharp
    /// one looks: too small and the terminal scales it up to fill the cells.
    mod cover_size {
        use super::*;

        #[test]
        fn the_drawn_size_follows_the_terminals_own_cells() {
            // An ordinary display: a 24×12 card of 10×20 cells is 240px square.
            assert_eq!(App::cover_draw_px_for((10, 20)), 240);
            // A HiDPI one, where the same card is physically the same size but
            // more than twice the pixels — the case a fixed 10×20 got wrong.
            assert_eq!(App::cover_draw_px_for((20, 44)), 528);
            // Cells are not always twice as tall as wide, and the axis that
            // needs the most pixels is the one that decides.
            assert_eq!(App::cover_draw_px_for((16, 24)), 384);
        }
    }

    /// Following a queue across a playlist that was fetched again — what makes
    /// `a` safe to act on the library rather than only the copy of it here.
    mod refetch {
        use super::*;

        fn songs(ids: &[&str]) -> Vec<Track> {
            ids.iter()
                .map(|id| Track {
                    video_id: Some((*id).to_string()),
                    title: Some((*id).to_string()),
                    artists: Vec::new(),
                    album: None,
                    duration: None,
                    duration_seconds: None,
                    thumbnail: None,
                })
                .collect()
        }

        fn ids(ids: &[&str]) -> Vec<Option<String>> {
            ids.iter().map(|id| Some((*id).to_string())).collect()
        }

        #[test]
        fn a_track_appended_to_a_playlist_moves_nothing() {
            // Which is what adding to an ordinary playlist does, so the queue
            // is left alone entirely.
            let out = moved_indices(&ids(&["a", "b"]), &songs(&["a", "b", "new"]));
            assert_eq!(out, None);
        }

        #[test]
        fn a_like_lands_at_the_top_and_moves_everything_down() {
            let out = moved_indices(&ids(&["a", "b"]), &songs(&["new", "a", "b"]));
            assert_eq!(out, Some(vec![Some(1), Some(2)]));
        }

        #[test]
        fn a_track_no_longer_in_the_playlist_is_marked_gone() {
            // An edit made somewhere else. `b` has no index to move to, and
            // saying so is what lets the queue drop it rather than inherit
            // whatever took its place.
            let out = moved_indices(&ids(&["a", "b", "c"]), &songs(&["c", "a"]));
            assert_eq!(out, Some(vec![Some(1), None, Some(0)]));
        }

        #[test]
        fn a_track_with_no_video_id_is_not_a_reorder() {
            // Unplayable and unmatchable, so it counts as staying put. Read as
            // "gone" it would make every refetch of a playlist holding one look
            // like a reorder, and drop it from the queue each time.
            let before = vec![Some("a".to_string()), None, Some("b".to_string())];
            let now = songs(&["a", "x", "b"]);
            assert_eq!(moved_indices(&before, &now), None);
        }

        #[test]
        fn a_first_load_has_nothing_to_follow() {
            assert_eq!(moved_indices(&[], &songs(&["a"])), None);
        }
    }

    /// The filter's matching rule, which memoising moved but must not change.
    /// `App` itself can't be built in a test — it boots libmpv.
    mod filtering {
        use super::*;

        fn track(title: Option<&str>, artists: &[&str]) -> Track {
            Track {
                video_id: Some("aaa".to_string()),
                title: title.map(str::to_string),
                artists: artists
                    .iter()
                    .map(|name| ytm_core::library::Artist {
                        name: (*name).to_string(),
                        id: None,
                    })
                    .collect(),
                album: None,
                duration: None,
                duration_seconds: None,
                thumbnail: None,
            }
        }

        #[test]
        fn a_query_matches_the_title() {
            let t = track(Some("Bohemian Rhapsody"), &["Queen"]);
            assert!(App::matches_filter(Some(&t), "rhapsody"));
            assert!(App::matches_filter(Some(&t), "bohemian"));
            assert!(!App::matches_filter(Some(&t), "waltz"));
        }

        #[test]
        fn a_query_matches_any_credited_artist() {
            let t = track(Some("Sway"), &["Anna Yvette", "Nevve"]);
            assert!(App::matches_filter(Some(&t), "nevve"));
            assert!(App::matches_filter(Some(&t), "yvette"));
        }

        #[test]
        fn a_track_with_no_title_is_matched_on_its_artist_alone() {
            let t = track(None, &["Feint"]);
            assert!(App::matches_filter(Some(&t), "feint"));
            assert!(!App::matches_filter(Some(&t), "sway"));
        }

        #[test]
        fn a_queue_entry_whose_playlist_has_not_loaded_matches_nothing() {
            // `library.track()` is `None` until the batch arrives, and a
            // filter must not claim a hit it can't see.
            assert!(!App::matches_filter(None, "anything"));
        }
    }

    fn rows(n: usize) -> Vec<LyricRow> {
        (0..n)
            .map(|i| LyricRow {
                lyric: i,
                text: format!("line{i}"),
                translated: false,
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
            let cfg = ytm_core::config::Lyrics {
                offset,
                ..Default::default()
            };
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
            let cfg = ytm_core::config::Lyrics {
                offset,
                ..Default::default()
            };
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

    /// Every hint list there is, including the one `I` only appears in.
    fn all_hints() -> Vec<(&'static str, &'static str)> {
        let mut all = App::picker_hints();
        all.extend(App::search_hints(true));
        all.extend(App::search_hints(false));
        all.extend(App::lyrics_hints(true));
        all.extend(App::lyrics_hints(false));
        for queue in [false, true] {
            all.extend(App::browse_hints(Panel::Songs, queue));
            all.extend(App::browse_hints(Panel::Playlists, queue));
        }
        all
    }

    /// `"j / k"` and `"q · Ctrl+C"` name two keys each; `"l/↵"` names two ways
    /// into one action.
    fn keys_named(label: &str) -> Vec<String> {
        // The filter key is a slash, so it is a label and not a separator.
        if label.trim() == "/" {
            return vec!["/".to_string()];
        }
        label
            .split(['/', '·'])
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect()
    }

    #[test]
    fn the_keymap_and_the_hint_bar_agree() {
        // A binding in the overlay but in no hint bar is one nobody finds
        // without opening the overlay — which is the thing the bar is for.
        let named: std::collections::HashSet<String> = all_hints()
            .iter()
            .flat_map(|(k, _)| keys_named(k))
            .collect();
        for (label, desc) in App::KEYMAP {
            if label.is_empty() {
                continue; // spacer
            }
            let keys = keys_named(label);
            assert!(
                keys.iter()
                    // The bar abbreviates the space bar, and Ctrl+C is the
                    // signal-free spelling of `q` rather than a key of its own.
                    .any(|k| named.contains(k)
                        || (k == "space" && named.contains("spc"))
                        || k == "Ctrl+C"),
                "{label:?} ({desc}) is in the keymap but in no hint bar"
            );
        }
    }

    #[test]
    fn no_hint_bar_names_a_key_twice() {
        for context in [
            App::picker_hints(),
            App::search_hints(false),
            App::lyrics_hints(true),
            App::browse_hints(Panel::Songs, false),
            App::browse_hints(Panel::Songs, true),
            App::browse_hints(Panel::Playlists, false),
        ] {
            let mut seen = std::collections::HashSet::new();
            for (key, _) in &context {
                assert!(seen.insert(*key), "{key:?} twice in {context:?}");
            }
        }
    }

    #[test]
    fn the_way_to_the_full_keymap_survives_a_narrow_terminal() {
        // The lists are long now, and `fit_hints` drops from the end. `?` has
        // to land inside 80 columns or a narrow terminal shows no way to the
        // rest of the bindings.
        for context in [
            App::search_hints(false),
            App::lyrics_hints(true),
            App::browse_hints(Panel::Songs, false),
            App::browse_hints(Panel::Songs, true),
            App::browse_hints(Panel::Playlists, false),
        ] {
            let shown = fit_hints(&context, 80);
            let text: String = shown.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains('?'), "no `?` in 80 columns: {text:?}");
        }
    }

    #[test]
    fn a_row_that_fits_is_left_whole() {
        let spans = fit_meta(
            "Echo",
            theme::PRIMARY,
            &[("Crusher-P".into(), theme::META)],
            40,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Echo  ·  Crusher-P");
    }

    #[test]
    fn a_row_that_does_not_fit_says_so_rather_than_stopping_mid_word() {
        // What a `Table` does instead is clip at the column edge, which with a
        // CJK title cuts a character in half.
        let spans = fit_meta(
            "Once Upon A Time (Melodic House & Techno Extended Mix)",
            theme::PRIMARY,
            &[("Max Oazo".into(), theme::META)],
            20,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(width_of(&text), 20);
        assert!(text.ends_with('…'), "{text:?}");
    }

    #[test]
    fn the_title_has_first_claim_and_the_rest_take_what_is_left() {
        let spans = fit_meta(
            "Echo",
            theme::PRIMARY,
            &[
                ("Crusher-P".into(), theme::META),
                ("An Album".into(), theme::DIM),
            ],
            24,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // Title and artist whole; the album is what gets cut.
        assert!(text.starts_with("Echo  ·  Crusher-P"), "{text:?}");
        assert!(width_of(&text) <= 24, "{text:?} is {}", width_of(&text));
    }

    #[test]
    fn a_field_with_no_room_left_is_dropped_whole() {
        // Four cells of ellipsis and separator would say less than nothing.
        let spans = fit_meta(
            "Echo",
            theme::PRIMARY,
            &[("Crusher-P".into(), theme::META)],
            8,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Echo");
    }

    #[test]
    fn an_empty_field_costs_no_separator() {
        let spans = fit_meta("Echo", theme::PRIMARY, &[(String::new(), theme::META)], 40);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Echo", "a track with no artist gets no dangling `·`");
    }

    #[test]
    fn a_wide_title_is_measured_in_cells() {
        // Eight CJK characters are sixteen cells, so half of them fit in ten.
        let spans = fit_meta("君の名前を呼ぶよ", theme::PRIMARY, &[], 10);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(width_of(&text) <= 10, "{text:?} is {}", width_of(&text));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn a_total_is_rounded_and_an_elapsed_is_not() {
        // mpv reports the real length; the track list shows YouTube's whole
        // seconds. Truncating printed 3:11 under a list that said 3:12.
        assert_eq!(fmt_secs_rounded(191.6), "3:12");
        assert_eq!(fmt_secs(191.6), "3:11");
        // A clock must not reach 0:01 before the first second is out.
        assert_eq!(fmt_secs(0.9), "0:00");
        assert_eq!(fmt_secs_rounded(0.0), "0:00");
        assert_eq!(fmt_secs_rounded(-5.0), "0:00");
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
                translated: false,
            },
            LyricRow {
                lyric: 1,
                text: "long part one".into(),
                translated: false,
            },
            LyricRow {
                lyric: 1,
                text: "long part two".into(),
                translated: false,
            },
            LyricRow {
                lyric: 2,
                text: "b".into(),
                translated: false,
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
                translated: false,
            },
            LyricRow {
                lyric: 1,
                text: String::new(),
                translated: false,
            },
            LyricRow {
                lyric: 2,
                text: "b".into(),
                translated: false,
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

    // ── translation ───────────────────────────────────────────────────────

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrapping_counts_cells_so_cjk_stays_in_its_column() {
        // Counting characters let a 26-character Japanese line occupy 52 cells
        // of a 26-cell panel, and the second half was simply clipped away.
        let long = "君の名前を呼ぶよ夜が明けるまでずっと歌っていたいんだ";
        let out = wrap_n_lines(long, 26, usize::MAX);
        assert!(out.len() > 1, "did not wrap at all: {out:?}");
        assert!(out.iter().all(|p| width_of(p) <= 26), "{out:?}");
        assert_eq!(out.concat(), long, "characters were lost");

        // ASCII is unchanged: one cell a character either way.
        assert_eq!(wrap_n_lines("abcdef", 3, usize::MAX), ["abc", "def"]);
    }

    #[test]
    fn a_truncating_wrap_leaves_room_for_the_ellipsis() {
        let out = wrap_n_lines("君の名前を呼ぶよ", 6, 1);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with('…'));
        assert!(
            width_of(&out[0]) <= 6,
            "{:?} is {} cells",
            out[0],
            width_of(&out[0])
        );
    }

    #[test]
    fn a_character_wider_than_the_column_still_makes_progress() {
        let out = wrap_n_lines("君の名", 1, usize::MAX);
        assert_eq!(out, ["君", "の", "名"]);
    }

    // ── the search panel's detail card, which wraps rather than cuts ───────

    #[test]
    fn words_break_between_words_where_there_are_any() {
        // The card's own width. Cell-exact wrapping gave "Everybody Wants To R
        // / ule The World", which reads as a rendering fault rather than a
        // long title.
        let out = wrap_words("Everybody Wants To Rule The World", 20, usize::MAX);
        assert_eq!(out, ["Everybody Wants To", "Rule The World"]);
        assert!(out.iter().all(|l| width_of(l) <= 20), "{out:?}");
    }

    #[test]
    fn a_word_longer_than_the_column_is_broken_by_cells() {
        // Nothing to break on, so the fallback is the lyric wrap — the
        // alternative is a line running past the column.
        let out = wrap_words("Supercalifragilistic", 8, usize::MAX);
        assert!(out.iter().all(|l| width_of(l) <= 8), "{out:?}");
        assert_eq!(out.concat(), "Supercalifragilistic");
    }

    #[test]
    fn a_cjk_title_wraps_by_cells_since_it_has_no_spaces() {
        let out = wrap_words("君の名前を呼ぶよ夜が明けるまで", 8, usize::MAX);
        assert!(out.len() > 1, "did not wrap: {out:?}");
        assert!(out.iter().all(|l| width_of(l) <= 8), "{out:?}");
        assert_eq!(out.concat(), "君の名前を呼ぶよ夜が明けるまで");
    }

    #[test]
    fn a_wrap_that_runs_out_of_lines_says_so() {
        let out = wrap_words("Everybody Wants To Rule The World", 20, 1);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with('…'), "{out:?}");
        assert!(width_of(&out[0]) <= 20, "{out:?}");
    }

    #[test]
    fn text_that_fits_is_left_exactly_alone() {
        assert_eq!(wrap_words("Kaai Yuki", 20, 2), ["Kaai Yuki"]);
        assert_eq!(wrap_words("", 20, 2), [""]);
    }

    #[test]
    fn without_a_translation_the_rows_are_the_lyrics_alone() {
        let out = lyric_rows(&strings(&["one", "two"]), &[], 40);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| !r.translated));
    }

    #[test]
    fn each_translation_follows_the_line_it_translates() {
        let out = lyric_rows(&strings(&["one", "two"]), &strings(&["一", "二"]), 40);
        let seen: Vec<(usize, bool, &str)> = out
            .iter()
            .map(|r| (r.lyric, r.translated, r.text.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (0, false, "one"),
                (0, true, "一"),
                (1, false, "two"),
                (1, true, "二"),
            ]
        );
    }

    #[test]
    fn an_untranslated_line_gets_no_row_of_its_own() {
        // A blank would read as a lyric the record is missing, and would push
        // the pairing out of step for everything below it.
        let out = lyric_rows(
            &strings(&["one", "two", "three"]),
            &strings(&["一", "", "三"]),
            40,
        );
        let seen: Vec<(usize, bool)> = out.iter().map(|r| (r.lyric, r.translated)).collect();
        assert_eq!(
            seen,
            [(0, false), (0, true), (1, false), (2, false), (2, true)]
        );

        // Same when the translation simply ran short.
        let short = lyric_rows(&strings(&["one", "two"]), &strings(&["一"]), 40);
        assert_eq!(short.len(), 3);
        assert!(!short[2].translated);
    }

    #[test]
    fn a_translation_identical_to_the_line_is_not_shown_twice() {
        // Kanji-only lines routinely come back unchanged, and a source already
        // in the target language comes back unchanged throughout.
        let out = lyric_rows(
            &strings(&["永遠", "走った"]),
            &strings(&["永遠", "跑了"]),
            40,
        );
        let seen: Vec<(usize, bool)> = out.iter().map(|r| (r.lyric, r.translated)).collect();
        assert_eq!(seen, [(0, false), (1, false), (1, true)]);
    }

    #[test]
    fn an_interlude_is_never_given_a_translation() {
        let out = lyric_rows(
            &strings(&["a", "  ", "b"]),
            &strings(&["甲", "x", "乙"]),
            40,
        );
        let gap: Vec<_> = out.iter().filter(|r| r.lyric == 1).collect();
        assert_eq!(gap.len(), 1, "the gap row stands alone");
        assert!(!gap[0].translated);
    }

    #[test]
    fn a_wrapped_translation_stays_bound_to_its_line() {
        // Both halves of the pair carry the same lyric index, so the highlight
        // covers the line and its translation together.
        let out = lyric_rows(&strings(&["aaaaaa"]), &strings(&["bbbbbb"]), 3);
        assert!(out.len() > 2, "nothing wrapped: {}", out.len());
        assert!(out.iter().all(|r| r.lyric == 0));
        assert_eq!(out.iter().filter(|r| r.translated).count(), 2);
        // Originals first, so the centring lands on the words being sung.
        assert!(!out[0].translated && !out[1].translated);
    }

    /// A lyric and its translation, ready for `synced_view`.
    fn paired(n: usize) -> Vec<LyricRow> {
        let texts: Vec<String> = (0..n).map(|i| format!("line{i}")).collect();
        let trans: Vec<String> = (0..n).map(|i| format!("译{i}")).collect();
        lyric_rows(&texts, &trans, 40)
    }

    #[test]
    fn a_translation_never_looks_like_the_words_themselves() {
        // The requirement in one assertion: no row is styled both ways, and
        // the translated rows own a colour nothing else in the panel uses.
        let out = synced_view(&paired(20), Some(10), 9, 0);
        let (styles, texts) = (styles(&out), texts(&out));

        for (style, text) in styles.iter().zip(&texts) {
            if text.is_empty() {
                continue;
            }
            if text.starts_with('译') {
                assert_eq!(style.fg, Some(Color::Magenta), "{text}: not marked");
            } else {
                assert_ne!(style.fg, Some(Color::Magenta), "{text}: wrongly marked");
            }
        }
    }

    #[test]
    fn the_highlight_stays_on_the_words_and_off_the_translation() {
        let out = synced_view(&paired(20), Some(10), 9, 0);
        let styles = styles(&out);
        // Still exactly one background on screen — two adjacent highlighted
        // rows would read as a single four-line lyric.
        assert_eq!(styles.iter().filter(|s| s.bg.is_some()).count(), 1);

        let active = styles.iter().position(|s| s.bg.is_some()).unwrap();
        assert_eq!(texts(&out)[active], "line10");
        assert_eq!(texts(&out)[active + 1], "译10", "translation sits under it");
        assert_eq!(styles[active + 1].fg, Some(Color::Magenta));
        assert!(styles[active + 1].bg.is_none());
    }

    #[test]
    fn the_active_line_is_still_centred_with_translations_on() {
        // Twice as many rows, so a centring bug shows up as an off-by-several.
        let height = 9;
        let out = synced_view(&paired(20), Some(10), height, 0);
        let active = styles(&out).iter().position(|s| s.bg.is_some()).unwrap();
        assert_eq!(active, (height as usize - 1) / 2);
    }

    #[test]
    fn translations_are_dimmed_during_the_intro() {
        // Nothing is playing yet, so nothing is emphasised — including the
        // translations, which still have to be tellable apart.
        let out = synced_view(&paired(20), None, 6, 0);
        assert!(styles(&out).iter().all(|s| s.bg.is_none()));
        for (style, text) in styles(&out).iter().zip(texts(&out)) {
            if text.starts_with('译') {
                assert_eq!(style.fg, Some(Color::Magenta));
            }
        }
    }

    /// Prints the translated panel so the pairing can be eyeballed:
    /// `cargo test -p yt-music-tui -- --ignored --nocapture render_translated`
    #[test]
    #[ignore = "visual smoke check — prints the panel, asserts nothing"]
    fn render_translated() {
        let words = strings(&[
            "夜が明けるまで踊ろう",
            "君の声が聞こえる",
            "",
            "誰も知らない場所へ",
            "もう戻れないんだ",
        ]);
        let trans = strings(&[
            "让我们跳舞到黎明",
            "我听到你的声音",
            "",
            "去一个无人知晓的地方",
            "我们再也回不去了",
        ]);
        let rows = lyric_rows(&words, &trans, 40);
        let out = draw(46, 11, |frame| {
            let body = section(
                frame,
                frame.area(),
                "Lyrics",
                Some(Line::from(vec![
                    Span::styled("♪ synced", theme::ACCENT),
                    Span::styled(SEP, theme::DIM),
                    Span::styled("⇄ zh", theme::TRANSLATION),
                ])),
                true,
            );
            frame.render_widget(
                Paragraph::new(synced_view(&rows, Some(3), body.height, 0)),
                body,
            );
        });
        println!("{}", out.join("\n"));
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
