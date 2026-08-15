//! Cover art: fetching a thumbnail and decoding it to raw pixels.
//!
//! Split this way on the usual line — this crate knows HTTP and image bytes,
//! the frontend knows how to put pixels on a terminal. What comes back is a
//! plain RGB buffer, which every terminal graphics protocol can take.
//!
//! Only JPEG is decoded, because that is the only thing YouTube's image CDN
//! serves here: its URLs end in `=w120-h120-l90-rj`, and the `rj` means "return
//! JPEG". A general image crate would add a dozen formats none of which arrive.

use std::sync::mpsc::Sender;
use std::time::Duration;

/// Cover art is decoration; it must never hold up the thing it decorates.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The range a cover is fetched in, whatever the caller asks to draw at.
///
/// Search rows advertise a 120px thumbnail, which is mush once a terminal
/// scales it into a block of cells. The size is a *URL parameter* rather than
/// part of the stored path, so a larger one can simply be asked for: measured
/// against the CDN, every size up to 1400 comes back at exactly that size and
/// anything beyond is served as 1400. The ceiling here is well under that —
/// past twice what any terminal can show, the extra pixels are decode time and
/// memory spent on detail that the downscale immediately averages away.
const MIN_PX: u32 = 480;
const MAX_PX: u32 = 1080;

/// What to ask the CDN for, to end up drawing at `draw_px`.
///
/// Twice the drawn size, because [`Cover::scaled`] averages boxes of source
/// pixels: 2×2 per output pixel is what makes an edge land smoothly rather than
/// being point-sampled. The floor keeps the common case — a 240px card on an
/// ordinary display — asking for the 480 it always did.
fn fetch_px(draw_px: u32) -> u32 {
    draw_px.saturating_mul(2).clamp(MIN_PX, MAX_PX)
}

/// A decoded cover: `width * height` pixels, three bytes each.
#[derive(Debug, Clone)]
pub struct Cover {
    pub width: u32,
    pub height: u32,
    /// RGB, row-major, no padding.
    pub rgb: Vec<u8>,
}

/// Rewrites a Google image URL to ask for a bigger copy.
///
/// The parameters after `=` are the CDN's own resize instructions —
/// `w120-h120-l90-rj` is "120 wide, 120 high, quality 90, as JPEG". Replacing
/// the two dimensions is all it takes; anything unrecognised is left alone, so
/// a URL in some other shape is fetched as-is rather than mangled.
#[must_use]
pub fn at_size(url: &str, px: u32) -> String {
    let Some((base, params)) = url.rsplit_once('=') else {
        return url.to_string();
    };
    if !params.contains("-h") && !params.starts_with('w') {
        return url.to_string();
    }
    let rewritten: Vec<String> = params
        .split('-')
        .map(|part| match part.chars().next() {
            Some('w') if part[1..].chars().all(|c| c.is_ascii_digit()) => format!("w{px}"),
            Some('h') if part[1..].chars().all(|c| c.is_ascii_digit()) => format!("h{px}"),
            _ => part.to_string(),
        })
        .collect();
    format!("{base}={}", rewritten.join("-"))
}

/// The full-resolution frame behind a YouTube *video* thumbnail, where one
/// exists.
///
/// [`at_size`] can do nothing for these: a video's thumbnail is served from
/// `i.ytimg.com` as `hqdefault.jpg?sqp=…`, a signed crop with no size to
/// rewrite, and it arrives 400×225 — enough on an ordinary terminal, soft on a
/// HiDPI one where the same box is 640 pixels across. `maxresdefault.jpg` is
/// the same frame at 1280×720.
///
/// It is an *attempt*, though, and the caller falls back: the file only exists
/// for videos uploaded in HD. Measured across five, three had it and two
/// answered 404. That is why this returns a candidate rather than a
/// replacement — a missed guess costs one small request, and getting it costs
/// three times the detail on every screen that can show it.
fn hd_variant(url: &str) -> Option<String> {
    let base = url.split_once('?').map_or(url, |(base, _)| base);
    let (prefix, name) = base.rsplit_once('/')?;
    if !prefix.contains("i.ytimg.com/vi") {
        return None;
    }
    // The named sizes YouTube serves, smallest up. Anything else — including
    // `maxresdefault` itself — is left alone rather than guessed at.
    const SMALLER: &[&str] = &[
        "default.jpg",
        "mqdefault.jpg",
        "hqdefault.jpg",
        "sddefault.jpg",
        "hq720.jpg",
    ];
    SMALLER
        .contains(&name)
        .then(|| format!("{prefix}/maxresdefault.jpg"))
}

/// The most a cover response may weigh, and the largest image it may claim to
/// be.
///
/// Nothing about a decorative thumbnail justifies either number being reached:
/// the largest copy this asks for is [`MAX_PX`], which arrives around 300 KB.
/// The limits are there because the size of what comes back is decided at the
/// far end — the reply is read into memory whole, and the decoder allocates
/// `width × height × 3` on the strength of a header. Both are checked before
/// the allocation rather than after it.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DECODE_PX: u32 = 2048;

/// Decodes JPEG bytes to RGB.
///
/// Greyscale is expanded here rather than left to the caller: a terminal
/// protocol wants one pixel layout, and a mono cover is rare enough that
/// nobody should have to special-case it downstream.
pub fn decode(bytes: &[u8]) -> Result<Cover, String> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    // The header on its own, so the dimensions can be refused before any
    // pixels are allocated for them.
    decoder.read_info().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("no image header")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if width == 0 || height == 0 {
        return Err(format!("{width}x{height} is not an image"));
    }
    if width.max(height) > MAX_DECODE_PX {
        return Err(format!(
            "{width}x{height} is past the {MAX_DECODE_PX}px a cover may be"
        ));
    }

    let pixels = decoder.decode().map_err(|e| e.to_string())?;

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => pixels.iter().flat_map(|&v| [v, v, v]).collect(),
        other => return Err(format!("unsupported pixel format {other:?}")),
    };

    let want = (width as usize) * (height as usize) * 3;
    if rgb.len() < want {
        return Err(format!("truncated image: {} of {want} bytes", rgb.len()));
    }
    Ok(Cover { width, height, rgb })
}

impl Cover {
    /// A copy no larger than `max_w` × `max_h`, keeping the aspect ratio.
    ///
    /// Averaged over the source box rather than sampled at a point: going from
    /// 480px to the ~160px a terminal actually shows is a 3× reduction, where
    /// nearest-neighbour drops eight of every nine pixels and turns fine album
    /// art into aliased noise. Averaging costs one pass and looks like the
    /// picture.
    ///
    /// Called twice on the way to the screen, which is not a duplication: once
    /// on arrival, down to the largest square any panel could use, so nothing
    /// bigger is ever held; then again at draw time, down to the rectangle the
    /// panel actually got. Only the second can be decided in advance — a panel's
    /// size changes with the terminal, while what was fetched does not.
    #[must_use]
    pub fn scaled(&self, max_w: u32, max_h: u32) -> Cover {
        if max_w == 0 || max_h == 0 || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        if self.width <= max_w && self.height <= max_h {
            return self.clone();
        }
        // One ratio for both axes, so the cover is never stretched.
        let ratio = f64::from(max_w) / f64::from(self.width);
        let ratio = ratio.min(f64::from(max_h) / f64::from(self.height));
        let width = ((f64::from(self.width) * ratio).round() as u32).max(1);
        let height = ((f64::from(self.height) * ratio).round() as u32).max(1);
        self.resampled(width, height)
    }

    /// A copy with the *shape* of `box_w × box_h`, as large as the source
    /// allows.
    ///
    /// This is the last step before the terminal, and it exists because of
    /// what the terminal does: it scales whatever it is handed to fill exactly
    /// the cells it was told to fill, so an image of any other shape arrives
    /// stretched by the difference. Where the box was built from the picture's
    /// own proportions — which is how the panels build one — this changes
    /// nothing except the last few pixels of rounding.
    ///
    /// Only the shape is guaranteed, not the size: nothing is ever *enlarged*
    /// here, since sending a terminal more pixels than the source had is
    /// bandwidth spent inventing detail it can invent itself. A small
    /// thumbnail is sent small and scaled up at the far end, which — the shape
    /// being right — is a clean enlargement rather than a stretch.
    #[must_use]
    pub fn filling(&self, box_w: u32, box_h: u32) -> Cover {
        if box_w == 0 || box_h == 0 || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        // The box's shape at the largest size neither axis has to be invented
        // for: shrink the wider-in-proportion axis to the source, not past it.
        let scale = f64::min(
            f64::from(self.width) / f64::from(box_w),
            f64::from(self.height) / f64::from(box_h),
        )
        .min(1.0);
        let width = ((f64::from(box_w) * scale).round() as u32).max(1);
        let height = ((f64::from(box_h) * scale).round() as u32).max(1);
        self.resampled(width, height)
    }

    /// This cover at exactly `width` × `height`, each axis scaled on its own.
    ///
    /// Box-averaged: every destination pixel is the mean of the source pixels
    /// it covers, which is what keeps an edge smooth where point sampling
    /// would drop most of them. Enlarging works but repeats pixels — callers
    /// avoid asking.
    #[must_use]
    fn resampled(&self, width: u32, height: u32) -> Cover {
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            // The source rows this destination row averages over.
            let y0 = (y * self.height / height) as usize;
            let y1 = (((y + 1) * self.height).div_ceil(height) as usize).min(self.height as usize);
            for x in 0..width {
                let x0 = (x * self.width / width) as usize;
                let x1 = (((x + 1) * self.width).div_ceil(width) as usize).min(self.width as usize);
                let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                for sy in y0..y1.max(y0 + 1) {
                    for sx in x0..x1.max(x0 + 1) {
                        let i = (sy * self.width as usize + sx) * 3;
                        if i + 2 < self.rgb.len() {
                            r += u32::from(self.rgb[i]);
                            g += u32::from(self.rgb[i + 1]);
                            b += u32::from(self.rgb[i + 2]);
                            n += 1;
                        }
                    }
                }
                let n = n.max(1);
                rgb.push((r / n) as u8);
                rgb.push((g / n) as u8);
                rgb.push((b / n) as u8);
            }
        }
        Cover { width, height, rgb }
    }
}

/// A finished cover fetch, keyed by the video it belongs to.
pub struct CoverMsg {
    pub video_id: String,
    pub result: Result<Cover, String>,
}

/// Fetches and decodes one cover in the background, sized to be drawn at
/// `draw_px` — the largest square, in pixels, the caller will ever put it in.
///
/// It comes back already scaled to that, rather than at the size fetched: the
/// downscale is the same box-average the drawing would do anyway, and doing it
/// here does it once, off the UI thread, and leaves the caller holding the
/// pixels it can show instead of the four times as many it asked the CDN for.
///
/// The picture keeps its own shape here — `draw_px` bounds it, it does not
/// describe it. A video's thumbnail is 16:9 and comes back 16:9; what it is
/// drawn in is the panel's business, and the panel builds a box to match.
///
/// Failures are reported rather than retried: a cover that doesn't arrive
/// costs a blank square, and the panel it decorates is still fully usable.
pub fn spawn_fetch(
    handle: &tokio::runtime::Handle,
    video_id: String,
    url: String,
    draw_px: u32,
    tx: Sender<CoverMsg>,
) {
    let url = at_size(&url, fetch_px(draw_px));
    handle.spawn(async move {
        let mut got = match hd_variant(&url) {
            Some(hd) => match fetch(&hd).await {
                Ok(cover) => Ok(cover),
                // No HD frame for this one, which is ordinary — the advertised
                // thumbnail is what the row promised in the first place.
                Err(e) => {
                    log::debug!("cover: {video_id} has no maxres frame ({e})");
                    fetch(&url).await
                }
            },
            None => fetch(&url).await,
        };
        got = got.map(|c| c.scaled(draw_px, draw_px));
        if let Err(e) = &got {
            log::debug!("cover: {video_id} failed ({e})");
        }
        let _ = tx.send(CoverMsg {
            video_id,
            result: got,
        });
    });
}

/// The one client every cover fetch shares.
///
/// Built per request before, which meant a fresh connection pool and a fresh
/// TLS handshake for each one — and covers arrive in runs, every row the
/// selection passes over, all to the same host. Held for the process, since
/// that is exactly how long the CDN connection is worth keeping.
fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .inspect_err(|e| log::warn!("cover: no HTTP client ({e}) — covers are off"))
                .ok()
        })
        .as_ref()
}

async fn fetch(url: &str) -> Result<Cover, String> {
    decode(&fetch_bytes(url).await?)
}

/// The bytes behind a cover URL, read against [`MAX_BYTES`].
///
/// Split out from [`fetch`] because macOS's Now Playing centre wants an
/// `NSImage` rather than a URL, so [`crate::media`] has to do the fetching
/// itself — and should do it through the same client and the same ceiling as
/// everything else here rather than growing its own.
pub(crate) async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = client().ok_or("no HTTP client")?;
    let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("{}", response.status()));
    }
    // Read in chunks against a ceiling rather than with `bytes()`, which takes
    // whatever the far end sends. A header claiming more than the cap is
    // refused without reading it at all.
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err(format!("cover is larger than {MAX_BYTES} bytes"));
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(format!("cover is larger than {MAX_BYTES} bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bigger_copy_is_asked_for_by_rewriting_the_size() {
        assert_eq!(
            at_size(
                "https://yt3.googleusercontent.com/abc=w120-h120-l90-rj",
                480
            ),
            "https://yt3.googleusercontent.com/abc=w480-h480-l90-rj"
        );
        // The quality and format instructions are not ours to change.
        assert!(at_size("https://x/abc=w60-h60-l90-rj", 480).ends_with("-l90-rj"));
    }

    #[test]
    fn a_url_in_another_shape_is_left_exactly_as_it_is() {
        for url in [
            "https://i.ytimg.com/vi/abc/maxresdefault.jpg",
            "https://example.com/cover.jpg",
            "https://example.com/a=b",
        ] {
            assert_eq!(at_size(url, 480), url, "{url}");
        }
    }

    #[test]
    fn a_videos_thumbnail_is_asked_for_at_full_resolution() {
        // What a search row advertises: a signed crop with no size to rewrite,
        // which arrives 400×225.
        assert_eq!(
            hd_variant("https://i.ytimg.com/vi/3UJZ8CndI8Y/hqdefault.jpg?sqp=-oaymwEW&rs=AMzJ")
                .as_deref(),
            Some("https://i.ytimg.com/vi/3UJZ8CndI8Y/maxresdefault.jpg")
        );
        assert!(hd_variant("https://i.ytimg.com/vi/abc/sddefault.jpg").is_some());
    }

    #[test]
    fn nothing_else_is_guessed_at() {
        // Album art, which `at_size` already handles and which has no
        // `maxresdefault` to ask for.
        assert_eq!(
            hd_variant("https://yt3.googleusercontent.com/abc=w480-h480-l90-rj"),
            None
        );
        // Already the biggest.
        assert_eq!(
            hd_variant("https://i.ytimg.com/vi/abc/maxresdefault.jpg"),
            None
        );
        // A name we don't know is not one to replace.
        assert_eq!(
            hd_variant("https://i.ytimg.com/vi/abc/oardefault.jpg"),
            None
        );
        assert_eq!(hd_variant("https://example.com/hqdefault.jpg"), None);
        assert_eq!(hd_variant("nonsense"), None);
    }

    #[test]
    fn a_size_that_is_not_a_number_is_not_rewritten() {
        // `w` here is part of a word, not a width.
        let url = "https://x/abc=wide-hd-rj";
        assert_eq!(at_size(url, 480), url);
    }

    /// A `w`×`h` image whose pixels encode their own position, so a resize can
    /// be checked for having actually averaged rather than just returned.
    fn ramp(w: u32, h: u32) -> Cover {
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.push((x % 256) as u8);
                rgb.push((y % 256) as u8);
                rgb.push(0);
            }
        }
        Cover {
            width: w,
            height: h,
            rgb,
        }
    }

    #[test]
    fn scaling_down_keeps_the_shape_and_the_pixel_count() {
        let small = ramp(480, 480).scaled(160, 160);
        assert_eq!((small.width, small.height), (160, 160));
        assert_eq!(small.rgb.len(), 160 * 160 * 3);
    }

    #[test]
    fn a_wide_cover_is_not_stretched_to_fit() {
        // 16:9 into a square box comes out 16:9, bounded by the width.
        let small = ramp(1600, 900).scaled(160, 160);
        assert_eq!(small.width, 160);
        assert_eq!(small.height, 90);
    }

    #[test]
    fn an_image_smaller_than_the_box_is_left_alone() {
        let source = ramp(64, 64);
        let same = source.scaled(160, 160);
        assert_eq!((same.width, same.height), (64, 64));
        assert_eq!(same.rgb, source.rgb);
    }

    #[test]
    fn scaling_averages_rather_than_dropping_pixels() {
        // Two source columns, 0 and 1, averaging to 0 (integer) — and a wider
        // ramp where the average of 0..4 is 1, which point-sampling could not
        // produce from the first pixel alone.
        let small = ramp(8, 1).scaled(2, 1);
        assert_eq!(small.width, 2);
        // First destination pixel spans source x 0..4 → red = (0+1+2+3)/4 = 1.
        assert_eq!(small.rgb[0], 1);
        // Second spans 4..8 → (4+5+6+7)/4 = 5.
        assert_eq!(small.rgb[3], 5);
    }

    // ── filling a box, which is what a cover is actually drawn into ────────

    #[test]
    fn a_cover_keeps_its_shape_through_the_box_built_for_it() {
        // What the panels actually do: a 16:9 thumbnail into a 16:9 box, and
        // album art into a square one. Neither is distorted, which is the
        // whole arrangement — the box is built from the picture.
        let out = ramp(480, 270).filling(320, 180);
        assert_eq!((out.width, out.height), (320, 180));
        assert_eq!(out.rgb.len(), 320 * 180 * 3);

        let out = ramp(480, 480).filling(240, 240);
        assert_eq!((out.width, out.height), (240, 240));
    }

    #[test]
    fn the_shape_is_the_boxs_whatever_the_source_was() {
        // The property the terminal cares about: it scales what it is given to
        // fill the cells it was told to fill, so anything other than the box's
        // own shape arrives stretched by the difference.
        for (w, h) in [(480, 270), (270, 480), (480, 480), (100, 400)] {
            let out = ramp(w, h).filling(240, 120);
            assert_eq!(
                out.width * 120,
                out.height * 240,
                "{w}x{h} came out {}x{}",
                out.width,
                out.height
            );
        }
    }

    #[test]
    fn a_source_smaller_than_the_box_is_not_enlarged_to_fit_it() {
        // Sending more pixels than arrived is bandwidth spent on detail that
        // was never there; the terminal can scale up by itself.
        let out = ramp(120, 120).filling(240, 240);
        assert_eq!((out.width, out.height), (120, 120));
        // Still the box's shape, just smaller.
        let out = ramp(120, 120).filling(240, 120);
        assert_eq!((out.width, out.height), (120, 60));
    }

    #[test]
    fn a_degenerate_box_does_not_divide_by_zero() {
        let source = ramp(8, 8);
        assert_eq!(source.scaled(0, 10).width, 8, "returns the original");
        assert_eq!(source.scaled(10, 0).width, 8);
        // And a box of one cell still produces one pixel rather than none.
        let tiny = source.scaled(1, 1);
        assert_eq!((tiny.width, tiny.height), (1, 1));
        assert_eq!(tiny.rgb.len(), 3);
    }

    /// A JPEG header and nothing behind it, claiming to be `w`×`h`.
    fn header(w: u16, h: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        // Three components, each with its sampling factors and table ids.
        out.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn an_image_too_large_to_be_a_cover_is_refused_before_it_is_decoded() {
        // The header is all that is read to decide this — the point is that
        // `width × height × 3` is never allocated on the strength of a number
        // that came from the network.
        let err = decode(&header(5000, 5000)).expect_err("should be refused");
        assert!(err.contains("2048px"), "{err}");
        // The size actually asked for is well inside it, so nothing real is
        // caught by this: what fails here is the missing image data, later.
        let err = decode(&header(1080, 1080)).expect_err("no pixels behind it");
        assert!(!err.contains("2048px"), "{err}");
    }

    #[test]
    fn a_zero_sized_image_is_not_an_image() {
        // The decoder gets there first — a zero height is JPEG's "defined
        // later", which it refuses outright — so this only pins that nothing
        // downstream is ever handed a zero to divide by.
        assert!(decode(&header(0, 0)).is_err());
    }

    #[test]
    fn nonsense_bytes_are_an_error_rather_than_a_panic() {
        assert!(decode(b"not a jpeg at all").is_err());
        assert!(decode(&[]).is_err());
        // A valid header with nothing behind it.
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_err());
    }

    #[test]
    fn what_is_fetched_is_twice_what_is_drawn_within_bounds() {
        // Twice the drawn size, so the box-average has 2×2 to work with.
        assert_eq!(fetch_px(300), 600);
        assert_eq!(fetch_px(540), MAX_PX);
        // An ordinary terminal draws a 240px card, and asks for the 480 it
        // always did rather than dropping to a thumbnail's worth of pixels.
        assert_eq!(fetch_px(240), MIN_PX);
        assert_eq!(fetch_px(0), MIN_PX);
        // A wildly big request is capped rather than fetching a poster.
        assert_eq!(fetch_px(u32::MAX), MAX_PX);
    }

    /// Hits Google's image CDN. `cargo test -p ytm-core cover -- --ignored`
    #[tokio::test]
    #[ignore = "network"]
    async fn live_a_cover_comes_back_at_the_size_asked_for() {
        let url = "https://yt3.googleusercontent.com/WS2ZqBCuEsGugI4SFV43J_vtlgl0VHhXImpnOf_63h58UeU3H4HRhVDPuv96zuXE5Io8P3FnfbDmLcJuSQ=w120-h120-l90-rj";
        // The row advertised 120px. Both ends of the range come back at exactly
        // what was asked for, which is what `fetch_px` counts on — a CDN that
        // quietly served the stored 120 would make every ceiling here fiction.
        for px in [MIN_PX, MAX_PX] {
            let cover = fetch(&at_size(url, px)).await.expect("fetched");
            eprintln!(
                "{px} → {}x{}, {} bytes",
                cover.width,
                cover.height,
                cover.rgb.len()
            );
            assert_eq!(cover.width, px, "asked for {px}px");
            assert_eq!(
                cover.rgb.len(),
                (cover.width * cover.height * 3) as usize,
                "not three bytes a pixel"
            );
        }
    }
}
