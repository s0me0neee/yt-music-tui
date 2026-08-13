//! Cover art on the terminal, via the kitty graphics protocol.
//!
//! ratatui draws cells; this draws pixels, so it has to work *around* ratatui
//! rather than through it. The sequence is: let ratatui paint the frame, leave
//! the cover's rectangle empty in that frame, then position the cursor over
//! that rectangle and hand the terminal the image. Kitty composites the image
//! above the cell grid and keeps it there until it is deleted, so ratatui
//! repainting spaces underneath does not erase it.
//!
//! That persistence is the thing to design against. An image left behind is
//! worse than no image at all — it hangs over whatever the panel shows next —
//! so [`Canvas`] tracks exactly what is on screen and where, redraws only when
//! one of those changes, and deletes on every exit path.
//!
//! ## Detection
//!
//! Escape sequences a terminal doesn't understand get *printed*, which would
//! spray the display with base64. Querying the terminal and waiting for a reply
//! is the thorough way and is not available here: crossterm owns stdin in raw
//! mode, and a query that goes unanswered by a terminal that ignores it would
//! block the UI at startup. So [`supported`] reads the environment instead,
//! which is conservative in the right direction — an unrecognised terminal
//! silently gets no covers rather than a mess.

use std::io::Write;

use ratatui::layout::Rect;
use ytm_core::Cover;

/// Payload bytes per escape sequence. The protocol's own limit is 4096 base64
/// characters per chunk.
const CHUNK: usize = 4096;

/// Pixel size of one terminal cell where the terminal won't say.
const FALLBACK_CELL: (u32, u32) = (10, 20);

/// Pixel size of one terminal cell.
///
/// This decides how many pixels an image is sent at, since the terminal is told
/// its target in *cells* and does the final fit itself: send fewer than the
/// rectangle physically holds and the terminal scales up, which is what a soft
/// cover on a HiDPI display actually is.
///
/// It comes from `TIOCGWINSZ`'s pixel fields, via crossterm — an ioctl, not a
/// query written to the terminal, so unlike the graphics-protocol handshake in
/// the module header it cannot hang waiting for a reply that never comes. The
/// three terminals that get this far all fill those fields in; a zero, a size
/// no font could have, or no tty at all falls back to a common 10×20.
#[must_use]
pub fn cell_size() -> (u32, u32) {
    let Ok(ws) = ratatui::crossterm::terminal::window_size() else {
        return FALLBACK_CELL;
    };
    if ws.columns == 0 || ws.rows == 0 {
        return FALLBACK_CELL;
    }
    let w = u32::from(ws.width) / u32::from(ws.columns);
    let h = u32::from(ws.height) / u32::from(ws.rows);
    if (4..=64).contains(&w) && (8..=128).contains(&h) {
        (w, h)
    } else {
        FALLBACK_CELL
    }
}

/// How far a box's shape may sit from the picture's before it is worth giving
/// up space to correct it.
///
/// Whole cells are all that can be reserved, so most shapes are not exactly
/// reachable and something has to give. Three percent of a 300-pixel cover is
/// nine pixels along one edge — not a thing anyone can see — while dropping
/// columns until the arithmetic comes out exact costs area that is plainly
/// visible. So the largest box inside this tolerance wins, and only when
/// nothing is inside it does the closest one win instead.
const ASPECT_TOLERANCE: f64 = 0.03;

/// The largest cell box within `max_cols` × `max_rows` whose **pixels** carry
/// the shape of an `aspect` (width, height) picture.
///
/// Both halves of that are load-bearing. A terminal cell is not square and is
/// not reliably twice as tall as it is wide either, so `n` columns by `n / 2`
/// rows is a square only by luck — on a 9×20 cell it is 216 pixels across by
/// 240 down, and since the terminal scales the image to fill exactly the cells
/// it was given, everything in it comes out 11% too tall. And a cover is not
/// always square to begin with: album art is, a video's thumbnail is 16:9, and
/// giving the second one the first one's box is what squashes it.
///
/// So the box is built from the picture's own shape and the cell size the
/// terminal reports, and the image is then sent shaped to the box — which,
/// because the box was built from it, changes nothing about how it looks.
#[must_use]
pub fn fit_cells(max_cols: u16, max_rows: u16, aspect: (u32, u32)) -> (u16, u16) {
    fit_cells_for(max_cols, max_rows, aspect, cell_size())
}

fn fit_cells_for(
    max_cols: u16,
    max_rows: u16,
    (aspect_w, aspect_h): (u32, u32),
    (cell_w, cell_h): (u32, u32),
) -> (u16, u16) {
    if aspect_w == 0 || aspect_h == 0 {
        return (0, 0);
    }
    let want = f64::from(aspect_w) / f64::from(aspect_h);
    let mut best = (0, 0);
    let mut best_err = f64::INFINITY;
    // Downwards, so the first box inside the tolerance is the biggest one.
    for cols in (1..=max_cols).rev() {
        let across = f64::from(u32::from(cols) * cell_w);
        let rows = (across / want / f64::from(cell_h)).round().max(1.0);
        if rows > f64::from(max_rows) {
            continue;
        }
        let down = rows * f64::from(cell_h);
        let err = (across / down - want).abs() / want;
        if err <= ASPECT_TOLERANCE {
            return (cols, rows as u16);
        }
        if err < best_err {
            best = (cols, rows as u16);
            best_err = err;
        }
    }
    best
}

/// Whether this terminal speaks the kitty graphics protocol.
///
/// Kitty itself, and the two other terminals that implement it. Ghostty sets
/// `TERM=xterm-ghostty`; WezTerm identifies through `TERM_PROGRAM`. Anything
/// else is assumed not to, because the failure mode of guessing wrong is
/// base64 all over the user's screen.
#[must_use]
pub fn supported() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || term.contains("kitty")
        || term.contains("ghostty")
        || program.eq_ignore_ascii_case("ghostty")
        || program.eq_ignore_ascii_case("WezTerm")
}

/// Standard base64, as the protocol's payload encoding wants it.
///
/// Hand-rolled for the same reason `translate::percent_encode` is: it is a
/// dozen lines of table lookup, and a dependency for it would be a dependency
/// to audit, pin and keep.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What is currently on the screen, so it is neither redrawn every frame nor
/// left behind when it should be gone.
#[derive(Default)]
pub struct Canvas {
    shown: Option<(String, Rect)>,
}

impl Canvas {
    /// Draws `cover` for `id` into `area`, if it isn't already there.
    ///
    /// Call after `terminal.draw`, with `area` left blank in that frame.
    pub fn show(&mut self, id: &str, cover: &Cover, area: Rect) {
        if area.width == 0 || area.height == 0 {
            self.clear();
            return;
        }
        // Same picture, same place: the terminal already has it, and resending
        // a megabyte thirty times a second is how a TUI starts to flicker.
        if self
            .shown
            .as_ref()
            .is_some_and(|(s, r)| s == id && *r == area)
        {
            return;
        }
        // A different picture, or the same one somewhere else — either way the
        // old one has to go first, or both are on screen at once.
        self.delete();

        // Shaped to the box, not fitted inside it. The terminal scales what it
        // is given to fill `area` exactly, so an image of any other shape
        // arrives stretched by the difference — and a 16:9 video thumbnail is
        // squashed into the square rather than sitting in a band across the
        // middle of it.
        let (cell_w, cell_h) = cell_size();
        let scaled = cover.filling(
            u32::from(area.width) * cell_w,
            u32::from(area.height) * cell_h,
        );
        if let Err(e) = self.transmit(&scaled, area) {
            log::debug!("[kitty] cover failed to draw: {e}");
            self.shown = None;
            return;
        }
        self.shown = Some((id.to_string(), area));
    }

    /// Removes whatever is on screen. Idempotent, and safe on a terminal that
    /// never had an image.
    pub fn clear(&mut self) {
        if self.shown.take().is_some() {
            self.delete();
        }
    }

    /// `a=d,d=A` — every image this program placed.
    fn delete(&self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b_Ga=d,d=A\x1b\\");
        let _ = out.flush();
    }

    fn transmit(&self, cover: &Cover, area: Rect) -> std::io::Result<()> {
        let payload = base64(&cover.rgb);
        let mut out = std::io::stdout().lock();

        // Cursor to the top-left of the target, 1-based. The image is placed
        // where the cursor is, so this is the whole of "positioning".
        write!(out, "\x1b[{};{}H", area.y + 1, area.x + 1)?;

        let chunks: Vec<&str> = payload
            .as_bytes()
            .chunks(CHUNK)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();

        for (i, chunk) in chunks.iter().enumerate() {
            let more = u8::from(i + 1 < chunks.len());
            if i == 0 {
                // f=24  — three bytes a pixel, which is what `Cover` holds
                // s,v   — the image's own size in pixels
                // c,r   — the cell box to fit it into; the terminal scales
                // C=1   — leave the cursor alone, so ratatui's idea of where
                //         it is stays true
                // q=2   — suppress the terminal's reply. Without this it
                //         answers on stdin and crossterm reads the answer as
                //         a keypress.
                write!(
                    out,
                    "\x1b_Ga=T,f=24,s={},v={},c={},r={},C=1,q=2,m={more};{chunk}\x1b\\",
                    cover.width, cover.height, area.width, area.height
                )?;
            } else {
                write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?;
            }
        }
        out.flush()
    }
}

impl Drop for Canvas {
    /// The terminal outlives the program; an image does not get to.
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_covers_the_whole_byte_range() {
        // The two characters that distinguish the standard alphabet from the
        // URL-safe one are `+` and `/`; a wrong table shows up here.
        assert_eq!(base64(&[0xFB, 0xFF]), "+/8=");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64(&[0xFF, 0xFF, 0xFF]), "////");
        // Every length remainder, at size.
        for n in 1..=9 {
            let data: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let encoded = base64(&data);
            assert_eq!(encoded.len(), data.len().div_ceil(3) * 4, "n={n}");
            assert!(encoded.is_ascii());
        }
    }

    #[test]
    fn an_image_is_not_redrawn_where_it_already_is() {
        // No terminal here, so this only checks the bookkeeping — which is the
        // part that decides whether the screen flickers.
        let mut canvas = Canvas::default();
        let area = Rect::new(1, 1, 10, 5);
        canvas.shown = Some(("abc".to_string(), area));
        assert!(
            canvas
                .shown
                .as_ref()
                .is_some_and(|(s, r)| s == "abc" && *r == area)
        );

        // A move counts as a change, not just a different id.
        let moved = Rect::new(2, 1, 10, 5);
        assert!(!canvas.shown.as_ref().is_some_and(|(_, r)| *r == moved));
    }

    /// The pixel dimensions of a cell box, for asserting on its shape.
    fn px(cols: u16, rows: u16, (cell_w, cell_h): (u32, u32)) -> (u32, u32) {
        (u32::from(cols) * cell_w, u32::from(rows) * cell_h)
    }

    /// How far a box's pixels sit from the shape asked for.
    fn shape_error(cols: u16, rows: u16, (aw, ah): (u32, u32), cell: (u32, u32)) -> f64 {
        let (w, h) = px(cols, rows, cell);
        let want = f64::from(aw) / f64::from(ah);
        (f64::from(w) / f64::from(h) - want).abs() / want
    }

    #[test]
    fn album_art_gets_a_box_that_is_square_in_pixels() {
        // The ordinary case, where a cell really is twice as tall as it is
        // wide and the old `cols / 2` was right.
        assert_eq!(fit_cells_for(32, 16, (1, 1), (10, 20)), (32, 16));

        // The case that came out stretched: 24 columns by 12 rows of a 9x20
        // cell is 216 across and 240 down, an 11% difference nobody asked for.
        for cell in [(9, 20), (20, 44), (13, 30), (6, 13)] {
            let (cols, rows) = fit_cells_for(32, 16, (1, 1), cell);
            let err = shape_error(cols, rows, (1, 1), cell);
            let (w, h) = px(cols, rows, cell);
            assert!(
                err <= ASPECT_TOLERANCE,
                "{cell:?}: {w}x{h} px, {err:.3} out"
            );
        }
    }

    #[test]
    fn a_video_thumbnail_gets_a_box_of_its_own_shape() {
        // 16:9, which is what a video result carries where a song carries
        // album art. Given the square one it was squashed into it.
        for cell in [(10, 20), (9, 20), (20, 44), (13, 30)] {
            let (cols, rows) = fit_cells_for(32, 16, (16, 9), cell);
            let err = shape_error(cols, rows, (16, 9), cell);
            let (w, h) = px(cols, rows, cell);
            assert!(
                err <= ASPECT_TOLERANCE,
                "{cell:?}: {w}x{h} px, {err:.3} out"
            );
            // And it is wider than it is tall, which a square box would not be.
            assert!(w > h, "{cell:?}: {w}x{h} px");
        }
    }

    #[test]
    fn the_box_is_the_biggest_shape_allows_rather_than_the_exact_one() {
        // Whole cells rarely divide exactly, and dropping columns until they
        // do costs area that is visible where the last few pixels of shape are
        // not. 24 columns of a 9x20 cell is 1.8% off square and kept.
        let (cols, rows) = fit_cells_for(24, 12, (1, 1), (9, 20));
        assert_eq!((cols, rows), (24, 11));
        assert!(shape_error(cols, rows, (1, 1), (9, 20)) <= ASPECT_TOLERANCE);
    }

    #[test]
    fn a_shape_no_box_can_carry_falls_back_to_the_closest() {
        // A panoramic cover in a box only six rows tall: nothing inside the
        // tolerance fits, so the least wrong is taken rather than nothing.
        let (cols, rows) = fit_cells_for(32, 2, (16, 9), (10, 20));
        assert!(cols > 0 && rows > 0, "{cols}x{rows}");
        assert!(rows <= 2);
    }

    #[test]
    fn a_box_never_exceeds_what_it_was_offered() {
        for cell in [(10, 20), (9, 20), (20, 44), (13, 30), (6, 13)] {
            for aspect in [(1, 1), (16, 9), (9, 16), (4, 3)] {
                for (max_cols, max_rows) in [(32, 16), (24, 12), (20, 10), (8, 3), (40, 40)] {
                    let (cols, rows) = fit_cells_for(max_cols, max_rows, aspect, cell);
                    assert!(
                        cols <= max_cols && rows <= max_rows,
                        "{cols}x{rows} {cell:?} {aspect:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_room_means_no_cover() {
        // Nothing fits, and the caller draws the words alone rather than a
        // one-row smear of album art.
        assert_eq!(fit_cells_for(24, 0, (1, 1), (10, 20)), (0, 0));
        assert_eq!(fit_cells_for(0, 12, (1, 1), (10, 20)), (0, 0));
        // A picture with no size to speak of, which is not one to lay out.
        assert_eq!(fit_cells_for(24, 12, (0, 0), (10, 20)), (0, 0));
    }

    #[test]
    fn a_cell_is_always_a_plausible_size() {
        // Under `cargo test` there is no tty to ask, so this is the fallback
        // path — which is the one worth pinning: a zero here would ask the CDN
        // for a zero-pixel cover and send the terminal an empty image.
        let (w, h) = cell_size();
        assert!((4..=64).contains(&w), "{w}px wide");
        assert!((8..=128).contains(&h), "{h}px tall");
    }

    #[test]
    fn a_zero_sized_area_shows_nothing() {
        let mut canvas = Canvas::default();
        canvas.shown = Some(("abc".to_string(), Rect::new(0, 0, 4, 4)));
        let cover = Cover {
            width: 2,
            height: 2,
            rgb: vec![0; 12],
        };
        // A panel collapsed to nothing must drop the image, not draw into it.
        canvas.show("abc", &cover, Rect::new(0, 0, 0, 0));
        assert!(canvas.shown.is_none());
    }
}
