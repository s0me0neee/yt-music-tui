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

        let (cell_w, cell_h) = cell_size();
        let scaled = cover.scaled(
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
