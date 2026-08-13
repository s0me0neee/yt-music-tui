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

/// What to ask the CDN for.
///
/// Search rows advertise a 120px thumbnail, which is mush once a terminal
/// scales it into a block of cells. The size is a *URL parameter* rather than
/// part of the stored path, so a larger one can simply be asked for.
const WANT_PX: u32 = 480;

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

/// Decodes JPEG bytes to RGB.
///
/// Greyscale is expanded here rather than left to the caller: a terminal
/// protocol wants one pixel layout, and a mono cover is rare enough that
/// nobody should have to special-case it downstream.
pub fn decode(bytes: &[u8]) -> Result<Cover, String> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("no image header")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));

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
    /// Cheap enough to do at draw time, and worth doing there rather than at
    /// fetch time: the target size is a property of the panel, which changes
    /// when the terminal is resized, while the fetched image does not.
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

/// Fetches and decodes one cover in the background.
///
/// Failures are reported rather than retried: a cover that doesn't arrive
/// costs a blank square, and the panel it decorates is still fully usable.
pub fn spawn_fetch(
    handle: &tokio::runtime::Handle,
    video_id: String,
    url: String,
    tx: Sender<CoverMsg>,
) {
    let url = at_size(&url, WANT_PX);
    handle.spawn(async move {
        let result = fetch(&url).await;
        if let Err(e) = &result {
            log::debug!("cover: {video_id} failed ({e})");
        }
        let _ = tx.send(CoverMsg { video_id, result });
    });
}

async fn fetch(url: &str) -> Result<Cover, String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("{}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bigger_copy_is_asked_for_by_rewriting_the_size() {
        assert_eq!(
            at_size("https://yt3.googleusercontent.com/abc=w120-h120-l90-rj", 480),
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

    #[test]
    fn nonsense_bytes_are_an_error_rather_than_a_panic() {
        assert!(decode(b"not a jpeg at all").is_err());
        assert!(decode(&[]).is_err());
        // A valid header with nothing behind it.
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_err());
    }

    /// Hits Google's image CDN. `cargo test -p ytm-core cover -- --ignored`
    #[tokio::test]
    #[ignore = "network"]
    async fn live_a_cover_comes_back_bigger_than_advertised() {
        let url = "https://yt3.googleusercontent.com/WS2ZqBCuEsGugI4SFV43J_vtlgl0VHhXImpnOf_63h58UeU3H4HRhVDPuv96zuXE5Io8P3FnfbDmLcJuSQ=w120-h120-l90-rj";
        let cover = fetch(&at_size(url, WANT_PX)).await.expect("fetched");
        eprintln!("{}x{}, {} bytes", cover.width, cover.height, cover.rgb.len());
        // The row advertised 120px; asking for more is the whole point.
        assert!(cover.width > 120, "got {}px", cover.width);
        assert_eq!(
            cover.rgb.len(),
            (cover.width * cover.height * 3) as usize,
            "not three bytes a pixel"
        );
    }
}
