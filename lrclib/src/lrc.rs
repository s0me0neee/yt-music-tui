//! Parsing for the LRC synced-lyrics format returned in [`Lyrics::synced_lyrics`].
//!
//! A record looks like:
//!
//! ```text
//! [ar:Crusher-P]
//! [00:12.34]the quiet part before
//! [00:15.02]everything gets loud
//! [00:18.00]
//! [00:21.55][01:40.00]and I was still standing
//! ```
//!
//! Metadata tags are discarded, a line carrying several timestamps expands to
//! one entry per timestamp, and blank lines are kept — an empty [`LyricLine`]
//! marks an instrumental gap, which a renderer wants to show rather than
//! freezing on the previous lyric.
//!
//! [`Lyrics::synced_lyrics`]: crate::Lyrics::synced_lyrics

/// One timestamped lyric line.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    /// Offset from the start of the track, in seconds.
    pub at: f64,
    /// Line text, trimmed. Empty for interludes / instrumental gaps.
    pub text: String,
}

/// Parses LRC text into timestamped lines, sorted ascending by [`LyricLine::at`].
///
/// Metadata tags (`[ar:]`, `[ti:]`, `[al:]`, `[by:]`, `[length:]`, …) are
/// skipped, and `[offset:±ms]` shifts every timestamp. Lines carrying no
/// timestamp at all are dropped. Malformed input yields fewer lines rather than
/// an error — this never panics.
pub fn parse_lrc(src: &str) -> Vec<LyricLine> {
    let mut out: Vec<LyricLine> = Vec::new();
    let mut offset_secs = 0.0f64;

    for raw in src.lines() {
        let line = raw.trim_end_matches('\r');
        let mut rest = line.trim_start();
        let mut stamps: Vec<f64> = Vec::new();

        // Peel leading `[...]` groups. Each is either a timestamp or a tag.
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(close) = inner.find(']') else {
                break; // Unclosed bracket — treat the remainder as text.
            };
            let content = &inner[..close];

            if let Some(secs) = parse_timestamp(content) {
                stamps.push(secs);
            } else if let Some(v) = content.strip_prefix("offset:") {
                // Positive offset shifts lyrics earlier, per the de-facto convention.
                if let Ok(ms) = v.trim().trim_start_matches('+').parse::<f64>() {
                    offset_secs = ms / 1000.0;
                }
            }
            // Any other tag is metadata — discarded.

            rest = &inner[close + 1..];
        }

        // No timestamp means metadata-only or garbage; drop the whole line.
        if stamps.is_empty() {
            continue;
        }

        let text = rest.trim();
        for at in stamps {
            out.push(LyricLine {
                at: at - offset_secs,
                text: text.to_string(),
            });
        }
    }

    // Stable, so multi-timestamp lines sharing an `at` keep their source order.
    out.sort_by(|a, b| a.at.total_cmp(&b.at));
    out
}

/// Parses `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `hh:mm:ss.xx` into seconds.
///
/// Every component must be ASCII digits, which is what makes tags like
/// `ar:Crusher-P` and `length:03:57` reject rather than parse as a time.
fn parse_timestamp(content: &str) -> Option<f64> {
    let parts: Vec<&str> = content.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }

    // The last component carries the optional fractional part.
    let (secs_str, frac) = match parts[parts.len() - 1].split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (parts[parts.len() - 1], None),
    };

    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    if !is_digits(secs_str) || !parts[..parts.len() - 1].iter().copied().all(is_digits) {
        return None;
    }

    // "0.{frac}" handles .x / .xx / .xxx uniformly.
    let frac_secs = match frac {
        Some(f) if is_digits(f) => format!("0.{f}").parse::<f64>().ok()?,
        Some(_) => return None,
        None => 0.0,
    };

    let secs: f64 = secs_str.parse().ok()?;
    let mins: f64 = parts[parts.len() - 2].parse().ok()?;
    let hours: f64 = if parts.len() == 3 {
        parts[0].parse().ok()?
    } else {
        0.0
    };

    Some(hours * 3600.0 + mins * 60.0 + secs + frac_secs)
}

/// Index of the line active at `t` seconds, or `None` before the first
/// timestamp (i.e. during an intro). After the last timestamp the final line
/// stays active.
///
/// `lines` must be sorted as [`parse_lrc`] returns it.
pub fn active_index(lines: &[LyricLine], t: f64) -> Option<usize> {
    if lines.is_empty() || t < lines[0].at {
        return None;
    }
    // Sorted input makes the predicate true-then-false, so this is a valid
    // O(log n) binary search. `i >= 1` because `lines[0].at <= t` here.
    let i = lines.partition_point(|l| l.at <= t);
    Some(i - 1)
}

/// Seconds until the next line boundary strictly after `t`, or `None` once past
/// the last line. Used to schedule a redraw exactly when the highlight moves.
pub fn next_boundary(lines: &[LyricLine], t: f64) -> Option<f64> {
    let i = lines.partition_point(|l| l.at <= t);
    lines.get(i).map(|l| l.at - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lines: &[LyricLine], i: usize) -> (f64, &str) {
        (lines[i].at, lines[i].text.as_str())
    }

    #[test]
    fn parses_two_digit_fraction() {
        let l = parse_lrc("[00:12.34]hello");
        assert_eq!(l.len(), 1);
        assert!((l[0].at - 12.34).abs() < 1e-9);
        assert_eq!(l[0].text, "hello");
    }

    #[test]
    fn parses_three_digit_fraction() {
        let l = parse_lrc("[01:02.500]x");
        assert!((l[0].at - 62.5).abs() < 1e-9);
    }

    #[test]
    fn parses_bare_mm_ss() {
        let l = parse_lrc("[02:03]x");
        assert!((l[0].at - 123.0).abs() < 1e-9);
    }

    #[test]
    fn parses_hh_mm_ss() {
        let l = parse_lrc("[01:00:05.50]x");
        assert!((l[0].at - 3605.5).abs() < 1e-9);
    }

    #[test]
    fn multiple_timestamps_expand_to_multiple_lines() {
        let l = parse_lrc("[00:12.00][01:40.00]chorus");
        assert_eq!(l.len(), 2);
        assert_eq!(at(&l, 0), (12.0, "chorus"));
        assert_eq!(at(&l, 1), (100.0, "chorus"));
    }

    #[test]
    fn metadata_tags_are_skipped() {
        let src =
            "[ar:Crusher-P]\n[ti:Echo]\n[al:Album]\n[by:someone]\n[length:03:57]\n[00:10.00]real";
        let l = parse_lrc(src);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].text, "real");
    }

    #[test]
    fn length_tag_does_not_parse_as_timestamp() {
        // `length` is not digits, so the tag must not become a 3:57 line.
        assert!(parse_timestamp("length:03:57").is_none());
        assert!(parse_timestamp("ar:Foo").is_none());
    }

    #[test]
    fn offset_shifts_timestamps_earlier() {
        let l = parse_lrc("[offset:+500]\n[00:10.00]x");
        assert!((l[0].at - 9.5).abs() < 1e-9);
    }

    #[test]
    fn negative_offset_shifts_later() {
        let l = parse_lrc("[offset:-500]\n[00:10.00]x");
        assert!((l[0].at - 10.5).abs() < 1e-9);
    }

    #[test]
    fn out_of_order_input_is_sorted() {
        let l = parse_lrc("[00:30.00]c\n[00:10.00]a\n[00:20.00]b");
        assert_eq!(
            l.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn blank_interlude_is_preserved() {
        let l = parse_lrc("[00:10.00]a\n[00:20.00]\n[00:30.00]b");
        assert_eq!(l.len(), 3);
        assert_eq!(l[1].text, "");
    }

    #[test]
    fn handles_crlf() {
        let l = parse_lrc("[00:10.00]a\r\n[00:20.00]b\r\n");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].text, "a");
        assert_eq!(l[1].text, "b");
    }

    #[test]
    fn line_without_brackets_is_ignored() {
        let l = parse_lrc("just some prose\n[00:10.00]a");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].text, "a");
    }

    #[test]
    fn unclosed_bracket_does_not_panic() {
        assert!(parse_lrc("[00:10.00").is_empty());
        assert!(parse_lrc("[").is_empty());
        let l = parse_lrc("[00:10.00]ok\n[unclosed");
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("\n\n\n").is_empty());
    }

    #[test]
    fn active_index_covers_all_positions() {
        let l = parse_lrc("[00:10.00]a\n[00:20.00]b\n[00:30.00]c");

        assert_eq!(active_index(&l, 0.0), None, "before the first stamp");
        assert_eq!(active_index(&l, 9.99), None);
        assert_eq!(active_index(&l, 10.0), Some(0), "exactly on a boundary");
        assert_eq!(active_index(&l, 15.0), Some(0), "between boundaries");
        assert_eq!(active_index(&l, 20.0), Some(1));
        assert_eq!(
            active_index(&l, 999.0),
            Some(2),
            "past the last stays on it"
        );
    }

    #[test]
    fn active_index_on_empty_is_none() {
        assert_eq!(active_index(&[], 5.0), None);
    }

    #[test]
    fn next_boundary_counts_down_then_ends() {
        let l = parse_lrc("[00:10.00]a\n[00:20.00]b");
        assert!((next_boundary(&l, 0.0).unwrap() - 10.0).abs() < 1e-9);
        assert!((next_boundary(&l, 15.0).unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(next_boundary(&l, 20.0), None, "on the last line");
        assert_eq!(next_boundary(&l, 99.0), None);
        assert_eq!(next_boundary(&[], 1.0), None);
    }
}
