//! Lyric translation, on top of the [`rust_translate`] crate.
//!
//! [`rust_translate`] is the transport; this module is the policy, the same
//! split as [`lrclib`](lrclib) and [`crate::lyrics`]. The crate is a thin
//! wrapper over Google's public `translate_a/single` endpoint and has two
//! sharp edges that everything here exists to work around:
//!
//! 1. **It interpolates the text straight into the URL.** A lyric containing
//!    `&`, `#` or `%` silently truncates or corrupts the request — `"Rock &
//!    roll #1"` comes back as `"岩石"`, just the first word. Fixed by
//!    [`percent_encode`], which `Url::parse` then leaves alone.
//! 2. **It returns only the first *segment* of the reply.** The endpoint splits
//!    its answer at sentence boundaries and the crate reads `[0][0][0]`, so
//!    anything past the first full stop is dropped without a word. Whether that
//!    bites depends on the *source* language: Japanese comes back as one
//!    segment even for a whole song, English arrives one sentence per segment.
//!
//! So a request is only trusted when the reply can be *proved* complete — one
//! line back for every line sent. [`translate_lines`] sends the first batch as
//! a probe: if it comes back whole, the rest of the song goes the same way (a
//! couple of requests, and every line translated with its neighbours for
//! context); if it doesn't, the song is re-fetched a sentence at a time, which
//! is slower but cannot silently lose half a line.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

/// Sentence terminators, used to cut a line into pieces small enough that the
/// endpoint cannot split one — the fallback when batching came back short.
///
/// The CJK forms are here because lyrics mix scripts freely; the endpoint
/// happens not to segment on them today, and splitting anyway costs only
/// context.
const SENTENCE_END: &[char] = &['.', '!', '?', '。', '！', '？', '…', '‥'];

/// Lines per batched request.
///
/// Two limits, both about the URL: the text goes in a query parameter, and
/// percent-encoded CJK runs 9 bytes per character. 40 lines of Japanese lands
/// around 3.5 KB, comfortably inside what the endpoint accepts.
const MAX_BATCH_LINES: usize = 40;
const MAX_BATCH_BYTES: usize = 4000;

/// Requests in flight at once. `rust_translate` builds a fresh client per call,
/// so there is no connection pool to share and each request pays its own TLS
/// handshake — concurrency is what makes a per-sentence fetch bearable.
/// Measured at 30 requests against the live endpoint: no rate limiting.
const CONCURRENCY: usize = 6;

/// The canonical spelling of a language code, or `None` if the endpoint has no
/// such language.
///
/// Worth checking up front because an unknown code is not an error out there:
/// `tl=zzz` returns the input unchanged, which would look like a translation
/// that simply never works.
pub fn normalise_language(code: &str) -> Option<&'static str> {
    let want = code.trim();
    if want.is_empty() {
        return None;
    }
    rust_translate::supported_languages::get_languages()
        .into_iter()
        .find(|known| known.eq_ignore_ascii_case(want))
}

/// Percent-encodes `text` for use as a query value.
///
/// Escapes everything outside the unreserved set, which is always safe and
/// leaves nothing for the URL parser to reinterpret. See the module docs for
/// why this can't be left to the crate.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// What [`percent_encode`] would cost, without building the string.
fn encoded_len(text: &str) -> usize {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => 1,
            _ => 3,
        })
        .sum()
}

/// Splits a line after each sentence terminator that has more text behind it.
///
/// The pieces concatenate back to the original exactly. Erring towards *more*
/// pieces is deliberate: cutting `Mr. Brightside` in two costs a little
/// translation quality, whereas cutting too few loses the words outright.
/// A `.` between two digits is the one exception — `3.5` is never a sentence.
fn sentence_pieces(line: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();

    while let Some((at, c)) = chars.next() {
        if !SENTENCE_END.contains(&c) {
            continue;
        }
        // Take the whole run, so `?!` and `...` cut once rather than three times.
        let mut end = at + c.len_utf8();
        while let Some(&(next_at, next)) = chars.peek() {
            if !SENTENCE_END.contains(&next) {
                break;
            }
            end = next_at + next.len_utf8();
            chars.next();
        }
        // Nothing behind it — this is the end of the line, not a boundary in it.
        if line[end..].trim().is_empty() {
            continue;
        }
        let decimal = c == '.'
            && end == at + 1
            && line[..at].ends_with(|p: char| p.is_ascii_digit())
            && line[end..].starts_with(|n: char| n.is_ascii_digit());
        if decimal {
            continue;
        }
        pieces.push(&line[start..end]);
        start = end;
    }

    if start < line.len() {
        pieces.push(&line[start..]);
    }
    if pieces.is_empty() {
        pieces.push(line);
    }
    pieces
}

/// Rejoins the translations of one line's sentences.
///
/// Whether a space belongs between them is a property of the *target*
/// language, not the source: Chinese and Japanese run sentences together,
/// Latin scripts don't. The last character of the piece before says which of
/// the two we are in.
fn join_pieces(pieces: &[String]) -> String {
    let mut out = String::new();
    for piece in pieces {
        if !out.is_empty() && out.chars().next_back().is_some_and(|c| c.is_ascii()) {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Groups lines into requests, bounded by both line count and encoded size.
fn chunk_lines(lines: &[&str]) -> Vec<Vec<String>> {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut bytes = 0;

    for line in lines {
        // `+ 3` for the encoded newline joining it to the line before.
        let cost = encoded_len(line) + 3;
        if !current.is_empty()
            && (current.len() >= MAX_BATCH_LINES || bytes + cost > MAX_BATCH_BYTES)
        {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push((*line).to_string());
        bytes += cost;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// One request: `lines` joined by newlines, answered line for line.
///
/// `Ok(None)` means the reply could not be proved complete — the endpoint
/// segmented it and the crate handed back only the first segment. The caller
/// re-fetches rather than displaying what came back, because there is no way
/// to tell a short answer from a correct one by reading it.
async fn translate_group(
    lines: &[String],
    to: &str,
) -> std::result::Result<Option<Vec<String>>, String> {
    let joined = lines.join("\n");
    let raw = rust_translate::translate(&percent_encode(&joined), "auto", to)
        .await
        // The crate's error is a `Box<dyn Error>`, which is not `Send` and so
        // cannot cross a task boundary. Stringify it here, as `LyricsMsg` does.
        .map_err(|e| e.to_string())?;

    let parts: Vec<String> = raw.split('\n').map(|p| p.trim().to_string()).collect();
    if parts.len() != lines.len() || parts.iter().any(String::is_empty) {
        return Ok(None);
    }
    Ok(Some(parts))
}

/// Runs one request per job, at most [`CONCURRENCY`] in flight, results in
/// order.
///
/// Each job is spawned rather than simply polled together so that a panic in
/// the crate — it `unwrap`s its way through the JSON — costs one line instead
/// of the whole song.
async fn run_jobs(
    jobs: Vec<Vec<String>>,
    to: &str,
) -> Vec<std::result::Result<Option<Vec<String>>, String>> {
    let mut out = Vec::with_capacity(jobs.len());
    for round in jobs.chunks(CONCURRENCY) {
        let handles: Vec<_> = round
            .iter()
            .map(|lines| {
                let (lines, to) = (lines.clone(), to.to_string());
                tokio::spawn(async move { translate_group(&lines, &to).await })
            })
            .collect();
        for handle in handles {
            out.push(match handle.await {
                Ok(result) => result,
                Err(e) => Err(format!("translation task died: {e}")),
            });
        }
    }
    out
}

/// Translates every distinct line, `None` where it could not be done.
///
/// The second half of the pair is the first error seen, kept so a total
/// failure can say *why* rather than reporting an empty translation.
async fn translate_distinct(lines: &[&str], to: &str) -> (Vec<Option<String>>, Option<String>) {
    let mut out: Vec<Option<String>> = vec![None; lines.len()];
    let mut error: Option<String> = None;
    let mut note = |e: String| {
        if error.is_none() {
            error = Some(e);
        }
    };

    let chunks = chunk_lines(lines);
    let Some(first) = chunks.first() else {
        return (out, None);
    };

    // The first chunk doubles as a probe: whether a multi-line request comes
    // back in one piece depends on the source language, so one request decides
    // how the rest of this song is fetched.
    let batched = match translate_group(first, to).await {
        Ok(Some(done)) => {
            for (slot, text) in out.iter_mut().zip(done) {
                *slot = Some(text);
            }
            true
        }
        Ok(None) => {
            log::debug!("translate: the endpoint segments this source — falling back to sentences");
            false
        }
        Err(e) => {
            note(e);
            false
        }
    };

    if batched {
        let rest: Vec<Vec<String>> = chunks[1..].to_vec();
        let mut at = first.len();
        for (job, result) in chunks[1..].iter().zip(run_jobs(rest, to).await) {
            match result {
                Ok(Some(done)) => {
                    for (slot, text) in out[at..at + job.len()].iter_mut().zip(done) {
                        *slot = Some(text);
                    }
                }
                Ok(None) => {
                    log::warn!("translate: a batch came back short — those lines stay bare")
                }
                Err(e) => note(e),
            }
            at += job.len();
        }
        return (out, error);
    }

    // One request per sentence. Every piece is free of internal terminators,
    // so the endpoint has nothing left to segment on and the reply is whole.
    let mut jobs: Vec<Vec<String>> = Vec::new();
    let mut spans: Vec<std::ops::Range<usize>> = Vec::with_capacity(lines.len());
    for line in lines {
        let from = jobs.len();
        jobs.extend(
            sentence_pieces(line)
                .into_iter()
                .map(|p| vec![p.trim().to_string()]),
        );
        spans.push(from..jobs.len());
    }

    let results = run_jobs(jobs, to).await;
    for (i, span) in spans.into_iter().enumerate() {
        let mut pieces = Vec::new();
        let mut whole = true;
        for result in &results[span] {
            match result {
                Ok(Some(done)) => pieces.push(done.join(" ")),
                Ok(None) => whole = false,
                Err(e) => {
                    whole = false;
                    note(e.clone());
                }
            }
        }
        // Half a translated line is worse than none: it reads as the whole
        // thought when it is only the start of one.
        if whole && !pieces.is_empty() {
            out[i] = Some(join_pieces(&pieces));
        }
    }

    (out, error)
}

/// Translates `lines` into `to`, one translation per input line.
///
/// The result is always the same length as the input; a line that is blank or
/// could not be translated gets an empty string, so callers can index the two
/// together. Blank lines and repeats are never sent — a chorus is one request,
/// however often it comes round.
///
/// Fails only when *nothing* came back, which is what a dead network or a
/// blocked endpoint looks like. A partial result is returned rather than
/// discarded: most of a translated song is worth having.
pub async fn translate_lines(
    lines: &[String],
    to: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut order: Vec<&str> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for line in lines {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let next = order.len();
        seen.entry(text).or_insert_with(|| {
            order.push(text);
            next
        });
    }
    if order.is_empty() {
        return Ok(vec![String::new(); lines.len()]);
    }

    let (done, error) = translate_distinct(&order, to).await;

    let got = done.iter().filter(|t| t.is_some()).count();
    if got == 0 {
        return Err(error.unwrap_or_else(|| "nothing came back".to_string()));
    }
    if got < order.len() {
        log::warn!(
            "translate: {} of {} lines came back untranslated{}",
            order.len() - got,
            order.len(),
            error.map_or(String::new(), |e| format!(" ({e})"))
        );
    }

    Ok(lines
        .iter()
        .map(|line| {
            seen.get(line.trim())
                .and_then(|i| done[*i].clone())
                .unwrap_or_default()
        })
        .collect())
}

/// A completed background translation.
///
/// Keyed by the lrclib record rather than the video: a translation belongs to
/// the *words*, so two tracks that resolve to the same record share one, and
/// changing record with `c` correctly gets a translation of its own.
pub enum TranslateMsg {
    Done {
        record_id: u64,
        result: std::result::Result<Vec<String>, String>,
    },
}

/// Translates one record's lines in the background.
pub fn spawn_translate(
    handle: &tokio::runtime::Handle,
    record_id: u64,
    lines: Vec<String>,
    to: String,
    tx: Sender<TranslateMsg>,
) {
    handle.spawn(async move {
        let result = translate_lines(&lines, &to).await;
        let _ = tx.send(TranslateMsg::Done { record_id, result });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── language codes ────────────────────────────────────────────────────

    #[test]
    fn a_language_is_recognised_however_it_is_typed() {
        assert_eq!(normalise_language("zh"), Some("zh"));
        assert_eq!(normalise_language("  fr  "), Some("fr"));
        assert_eq!(normalise_language("EN"), Some("en"));
        // The one code with capitals in it, so the canonical spelling has to
        // come back from the list rather than from what the user typed.
        assert_eq!(normalise_language("zh-tw"), Some("zh-TW"));
    }

    #[test]
    fn an_unknown_language_is_rejected_rather_than_passed_on() {
        // The endpoint answers `tl=zzz` with the input unchanged, which would
        // read as a translation that mysteriously never works.
        assert_eq!(normalise_language("zzz"), None);
        assert_eq!(normalise_language("chinese"), None);
        assert_eq!(normalise_language(""), None);
        assert_eq!(normalise_language("   "), None);
    }

    // ── encoding ──────────────────────────────────────────────────────────

    #[test]
    fn everything_the_url_could_reinterpret_is_escaped() {
        // The three that actually corrupt a request: `&` starts a new
        // parameter, `#` starts the fragment, `%` opens an escape.
        assert_eq!(percent_encode("a&b"), "a%26b");
        assert_eq!(percent_encode("a#b"), "a%23b");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b"), "a%2Bb");
    }

    #[test]
    fn the_unreserved_set_is_left_alone() {
        assert_eq!(percent_encode("Hello-world_1.0~"), "Hello-world_1.0~");
    }

    #[test]
    fn non_ascii_is_encoded_by_utf8_byte() {
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("君"), "%E5%90%9B");
    }

    #[test]
    fn the_length_estimate_matches_what_encoding_produces() {
        for s in [
            "",
            "abc",
            "a&b",
            "君の名前を呼ぶよ",
            "Rock & roll #1 + you?",
        ] {
            assert_eq!(encoded_len(s), percent_encode(s).len(), "{s:?}");
        }
    }

    // ── sentence pieces ───────────────────────────────────────────────────

    #[test]
    fn a_line_without_an_internal_stop_stays_whole() {
        assert_eq!(
            sentence_pieces("I walked alone tonight"),
            ["I walked alone tonight"]
        );
        // Terminators at the very end are not boundaries — nothing follows.
        assert_eq!(sentence_pieces("Hold on!"), ["Hold on!"]);
        assert_eq!(
            sentence_pieces("君の名前を呼ぶよ。"),
            ["君の名前を呼ぶよ。"]
        );
        assert_eq!(sentence_pieces(""), [""]);
        assert_eq!(sentence_pieces("..."), ["..."]);
    }

    #[test]
    fn an_internal_stop_splits_and_the_pieces_rebuild_the_line() {
        // This is the line the crate truncates to "我说了再见。" — half of it.
        let line = "I said goodbye. But I lied.";
        assert_eq!(sentence_pieces(line), ["I said goodbye.", " But I lied."]);
        assert_eq!(sentence_pieces(line).concat(), line);
    }

    #[test]
    fn a_run_of_terminators_cuts_once() {
        assert_eq!(sentence_pieces("What?! Really"), ["What?!", " Really"]);
        assert_eq!(
            sentence_pieces("Wait... and then"),
            ["Wait...", " and then"]
        );
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence() {
        assert_eq!(sentence_pieces("3.5 seconds left"), ["3.5 seconds left"]);
        // But a full stop that happens to sit next to a digit still is one.
        assert_eq!(sentence_pieces("Take 5. Go now"), ["Take 5.", " Go now"]);
    }

    #[test]
    fn every_split_line_rebuilds_exactly() {
        for line in [
            "a.b",
            "Oh! Oh! Oh!",
            "夜が明ける。君を呼ぶ。もう一度",
            "Mr. Brightside",
            "...what?",
            "  spaced .  out  ",
        ] {
            assert_eq!(sentence_pieces(line).concat(), line, "{line:?}");
        }
    }

    // ── rejoining ─────────────────────────────────────────────────────────

    #[test]
    fn latin_pieces_get_a_space_and_cjk_pieces_do_not() {
        assert_eq!(
            join_pieces(&["J'ai dit au revoir.".into(), "Mais j'ai menti.".into()]),
            "J'ai dit au revoir. Mais j'ai menti."
        );
        assert_eq!(
            join_pieces(&["我说了再见。".into(), "但我撒谎了。".into()]),
            "我说了再见。但我撒谎了。"
        );
        assert_eq!(join_pieces(&["alone".into()]), "alone");
        assert_eq!(join_pieces(&[]), "");
    }

    // ── chunking ──────────────────────────────────────────────────────────

    #[test]
    fn chunks_respect_both_the_line_and_the_byte_limit() {
        let many: Vec<&str> = vec!["a"; MAX_BATCH_LINES * 2 + 1];
        let chunks = chunk_lines(&many);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), MAX_BATCH_LINES);
        assert_eq!(chunks[2].len(), 1);

        // Encoded CJK is 9 bytes a character, so the byte limit bites first.
        let long = "君の名前を呼ぶよ夜が明けるまで".repeat(4);
        let heavy: Vec<&str> = vec![long.as_str(); 20];
        let chunks = chunk_lines(&heavy);
        assert!(chunks.len() > 1, "byte budget ignored");
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|l| encoded_len(l) + 3).sum();
            assert!(
                bytes <= MAX_BATCH_BYTES || chunk.len() == 1,
                "{bytes} over budget"
            );
        }
    }

    #[test]
    fn a_single_over_budget_line_still_gets_its_own_chunk() {
        // Never dropped: one oversized request is better than no translation.
        let huge = "x".repeat(MAX_BATCH_BYTES * 2);
        let chunks = chunk_lines(&[huge.as_str()]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
    }

    #[test]
    fn nothing_to_translate_makes_no_chunks() {
        assert!(chunk_lines(&[]).is_empty());
    }

    // ── assembling the result ─────────────────────────────────────────────

    #[tokio::test]
    async fn a_song_with_no_words_needs_no_requests() {
        // No network touched: `translate_lines` returns before spawning
        // anything when every line is blank.
        let lines = vec![String::new(), "   ".to_string()];
        assert_eq!(translate_lines(&lines, "zh").await.unwrap(), ["", ""]);
    }

    /// The mapping step on its own, with the fetch stubbed out — the part that
    /// has to line the translations back up with the original rows.
    fn reassemble(lines: &[String], done: &[(&str, Option<&str>)]) -> Vec<String> {
        let table: HashMap<&str, Option<&str>> = done.iter().copied().collect();
        lines
            .iter()
            .map(|l| {
                table
                    .get(l.trim())
                    .copied()
                    .flatten()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_repeated_chorus_reuses_one_translation() {
        let lines: Vec<String> = ["hold on", "", "hold on", "let go"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = reassemble(
            &lines,
            &[("hold on", Some("坚持")), ("let go", Some("放手"))],
        );
        assert_eq!(out, ["坚持", "", "坚持", "放手"]);
    }

    #[test]
    fn a_line_that_failed_comes_back_blank_rather_than_shifting_the_rest() {
        let lines: Vec<String> = ["one", "two", "three"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = reassemble(
            &lines,
            &[("one", Some("一")), ("two", None), ("three", Some("三"))],
        );
        assert_eq!(out, ["一", "", "三"]);
    }

    // ── live ──────────────────────────────────────────────────────────────

    /// Hits the real endpoint. `cargo test -p ytm-core -- --ignored`
    #[tokio::test]
    #[ignore = "network"]
    async fn live_translation_survives_the_crates_sharp_edges() {
        let lines: Vec<String> = [
            "Rock & roll #1 + you?",
            "I said goodbye. But I lied.",
            "君の名前を呼ぶよ",
            "",
            "君の名前を呼ぶよ",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let out = translate_lines(&lines, "zh").await.expect("translated");
        assert_eq!(out.len(), lines.len());
        // The `&`/`#` line survives whole rather than collapsing to one word.
        assert!(out[0].contains('1'), "query was corrupted: {:?}", out[0]);
        // Both halves of the two-sentence line come back.
        assert!(
            out[1].chars().count() > 6,
            "line was truncated: {:?}",
            out[1]
        );
        assert_eq!(out[3], "", "a blank line gained text");
        assert_eq!(out[2], out[4], "the repeat differs from the original");
    }

    /// A song's worth of lines, both ways round: Japanese takes the batched
    /// path (a couple of requests), English falls back to a sentence each.
    /// Both have to come back complete.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_a_whole_song_comes_back_line_for_line() {
        let verse = [
            "夜が明けるまで踊ろう",
            "君の声が聞こえる",
            "遠い街の灯り",
            "僕は歩き出した",
            "涙は見せないで",
            "風が吹いている",
            "誰も知らない場所へ",
            "君と二人だけ",
            "時計の針が止まる",
            "もう戻れないんだ",
        ];
        let english = [
            "I walked alone tonight.",
            "The city lights are fading fast!",
            "Do you remember me?",
            "We danced until the morning came.",
            "Nothing lasts forever, love.",
        ];

        for source in [verse.to_vec(), english.to_vec()] {
            // Four verses, so the chorus-dedupe and the chunking both engage.
            let lines: Vec<String> = std::iter::repeat_n(source.iter(), 4)
                .flatten()
                .map(|s| s.to_string())
                .collect();

            let started = std::time::Instant::now();
            let out = translate_lines(&lines, "zh").await.expect("translated");
            eprintln!(
                "{} lines ({} distinct) in {:?}",
                lines.len(),
                source.len(),
                started.elapsed()
            );

            assert_eq!(out.len(), lines.len());
            for (src, got) in lines.iter().zip(&out) {
                assert!(!got.is_empty(), "{src:?} came back untranslated");
            }
            // The repeats must agree — they are one translation reused.
            assert_eq!(out[..source.len()], out[source.len()..source.len() * 2]);
        }
    }
}
