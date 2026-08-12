//! Whole-song lyric translation via the Anthropic Messages API.
//!
//! The free path reads its input line by line and so can never see a sentence
//! spanning three lyric lines. A model given the whole song reads the enjambment
//! itself, which is why nothing here is pre-grouped.
//!
//! Alignment is enforced, not hoped for, because a shifted reply is silent: it
//! puts every later translation under the wrong lyric with nothing to say so.
//! [`place`] checks the reply twice — the index set (nothing dropped, repeated
//! or invented) and the echo (each entry copies out the line it translates, and
//! that copy is compared against what was sent). The echo catches what indices
//! cannot: a reply of the right length and numbering whose text slipped a line.
//! Either failing rejects the whole reply, and [`super::translate_lines`] falls
//! through to the free path.
//!
//! Cost: repeated lines are sent once, so a chorus is translated once however
//! often it comes round. Measured over three runs of a 40-line Japanese song
//! into Chinese, Haiku 4.5 came to 0.67¢ — see the `usage` line each request
//! logs. `app.rs` keeps the result for the session, so a replay is free.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Output cap per distinct line, with a floor and a ceiling. Measured at ~27
/// tokens a line (the echo, the translation and the JSON around them); the rest
/// is margin for longer scripts and for a model that thinks first. Headroom is
/// free — this is a cap, not a reservation.
/// The floor is what it is because `lyrics.ai-model` is whatever the user
/// wrote: on a model that thinks by default, thinking comes out of the same
/// budget, and a short song hitting the cap fails the whole request.
const TOKENS_PER_LINE: usize = 200;
const MIN_TOKENS: usize = 8192;
const MAX_TOKENS: usize = 32000;

/// One request for a whole song, answered twice over.
const TIMEOUT: Duration = Duration::from_secs(180);

/// The standing instructions; everything song-specific is in the user turn.
///
/// The connectives paragraph was added after a hand-translated record caught
/// two lines where a contrastive particle was dropped while distributing a
/// sentence across lines.
const SYSTEM: &str = "\
You translate song lyrics into {target}. Every `text` you write is in
{target}, whatever language the lyrics themselves are in.

You are given the whole song at once, so use it: read every line before
translating any of them.

Alignment. Both are checked on the way back in, and a reply that fails either
is discarded whole:
- Return exactly one entry per line you are given — every index, each once, in
  ascending order. None omitted, none invented.
- Copy the line into `source` character for character before translating it,
  then put the translation in `text`. The copy is compared against the line
  that was sent, so a `text` belonging to a different line is caught.

Never merge two lines into one entry, and never split one line across two.
Every line that carries words gets a translation; only a marker — `[Chorus]`,
`(instrumental)`, a bare `♪` — gets an empty `text`.

Lyric lines break on singing breath, not on grammar: one sentence often runs
across two or three lines. Translate the sentence as a whole, then distribute
its meaning across the same lines it occupied — each line carrying the part
that belongs to it, so the lines read in order as the sentence does and each
line's own words stay on that line.

Keep connectives and contrast markers. If a line ends in one — \"but\", \"though\",
\"because\", \"and yet\" — that line's translation carries it too. Distributing a
sentence across lines must not drop the word that joins them.

Translate the imagery, not the idiom. Keep the register of the original: if it
is plain, stay plain. Do not add interpretation the source does not carry, and
do not explain the metaphor.";

/// The song. The count sits next to the lines because that is where it has to
/// hold: a model that can see how many it was handed drops far fewer of them.
const USER: &str = "\
{count} lines, numbered 0 to {last}. Return exactly {count} entries.

<lines>
{numbered}
</lines>";

/// Model and credential for the AI path. Built from `config.toml`; absent when
/// `lyrics.ai-model` is empty, which is the default.
#[derive(Debug, Clone)]
pub struct Ai {
    pub model: String,
    pub api_key: String,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// One `{index, source, text}` per line, indices contiguous from zero.
///
/// Property order is load-bearing: constrained decoding emits them as declared,
/// so the line is copied out *before* it is translated — which is what makes
/// the echo an anchor rather than an afterthought.
///
/// The array is left unconstrained in length on purpose: `minItems`/`maxItems`
/// are not among the keywords structured outputs enforces, so stating a count
/// here would read as a guarantee that isn't one. [`USER`] asks for it and
/// [`place`] checks it.
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "lines": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer" },
                        "source": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["index", "source", "text"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["lines"],
        "additionalProperties": false
    })
}

/// The distinct lines worth translating, in order of first appearance, plus the
/// index each original line maps to.
///
/// Blank lines cost tokens and say nothing; repeats say nothing new. Sending a
/// chorus once also makes its repeats agree by construction rather than by
/// asking the model nicely.
fn distinct(lines: &[String]) -> (Vec<&str>, HashMap<&str, usize>) {
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
    (order, seen)
}

/// Translates every line into `to`, one entry per input line.
pub async fn translate(lines: &[String], to: &str, ai: &Ai) -> Result<Vec<String>, String> {
    let (order, seen) = distinct(lines);
    if order.is_empty() {
        return Ok(vec![String::new(); lines.len()]);
    }

    let numbered = order
        .iter()
        .enumerate()
        .map(|(n, line)| format!("{n}\t{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    // The name, not the code: asked for `zh`, Haiku answers in English.
    let target = super::language_name(to).unwrap_or(to);
    let system = SYSTEM.replace("{target}", target);
    let user = USER
        .replace("{count}", &order.len().to_string())
        .replace("{last}", &(order.len() - 1).to_string())
        .replace("{numbered}", &numbered);

    let max_tokens = (order.len() * TOKENS_PER_LINE).clamp(MIN_TOKENS, MAX_TOKENS);

    let response = client()?
        .post(ENDPOINT)
        .header("x-api-key", &ai.api_key)
        .header("anthropic-version", API_VERSION)
        .json(&json!({
            "model": ai.model,
            "max_tokens": max_tokens,
            "system": system,
            "output_config": { "format": { "type": "json_schema", "schema": schema() } },
            "messages": [{ "role": "user", "content": user }],
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    // The body carries the API's own error message, which is far more useful
    // than the status alone — a bad model id and a revoked key are both 400-ish
    // and read identically without it.
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}: {}", api_error(&body)));
    }

    let reply: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    log_usage(&reply, ai, lines.len(), order.len());
    match reply.get("stop_reason").and_then(Value::as_str) {
        Some("refusal") => return Err("the model declined this request".to_string()),
        Some("max_tokens") => return Err("reply was truncated".to_string()),
        _ => {}
    }

    let text = reply
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        })
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
        .ok_or("no text block in the reply")?;

    let done = place(text, &order)?;
    Ok(lines
        .iter()
        .map(|line| {
            seen.get(line.trim())
                .map(|i| done[*i].clone())
                .unwrap_or_default()
        })
        .collect())
}

/// What the request cost, so `app.log` can answer it without guesswork.
fn log_usage(reply: &Value, ai: &Ai, lines: usize, distinct: usize) {
    let count = |field: &str| {
        reply
            .get("usage")
            .and_then(|u| u.get(field))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    log::debug!(
        "translate: {} used {} in / {} out for {distinct} of {lines} lines",
        ai.model,
        count("input_tokens"),
        count("output_tokens"),
    );
}

/// Digs the human-readable message out of an API error body, falling back to
/// the raw body when it isn't the shape we expect.
fn api_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// Whether the model's echo is the line it was handed.
///
/// Whitespace is ignored: a model copying a lyric will occasionally collapse a
/// run of spaces, and that is not a misalignment. Any other difference is.
fn echoes(got: &str, want: &str) -> bool {
    let bare = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    bare(got) == bare(want)
}

/// The first few characters of `text`, so an error fits on a log line.
fn snippet(text: &str) -> String {
    let head: String = text.chars().take(40).collect();
    if head.chars().count() < text.chars().count() {
        format!("{head}…")
    } else {
        head
    }
}

/// One translation per line in `order`, or an error if the reply cannot be
/// placed exactly: short, repeated, out of range, or echoing the wrong line.
fn place(text: &str, order: &[&str]) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        index: usize,
        source: String,
        text: String,
    }
    #[derive(serde::Deserialize)]
    struct Reply {
        lines: Vec<Entry>,
    }

    let reply: Reply = serde_json::from_str(text).map_err(|e| format!("bad reply shape: {e}"))?;

    let mut out = vec![String::new(); order.len()];
    let mut filled = vec![false; order.len()];
    for entry in reply.lines {
        let Some(line) = order.get(entry.index) else {
            return Err(format!(
                "reply indexed line {} of {}",
                entry.index,
                order.len()
            ));
        };
        if std::mem::replace(&mut filled[entry.index], true) {
            return Err(format!("reply repeated line {}", entry.index));
        }
        if !echoes(&entry.source, line) {
            return Err(format!(
                "reply put {:?} under line {} ({:?})",
                snippet(&entry.source),
                entry.index,
                snippet(line)
            ));
        }
        out[entry.index] = entry.text.trim().to_string();
    }

    let missing = filled.iter().filter(|f| !**f).count();
    if missing > 0 {
        return Err(format!("reply dropped {missing} of {} lines", order.len()));
    }

    // Asked for, not a failure: a marker line gets no translated row. Logged
    // because a song where most lines do this is a model ignoring the rule.
    let bare = out.iter().filter(|t| t.is_empty()).count();
    if bare > 0 {
        log::debug!(
            "translate: {bare} of {} lines came back with no translation",
            order.len()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_and_repeats_are_never_sent() {
        let lines: Vec<String> = ["one", "", "two", "  one  ", "two"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (order, seen) = distinct(&lines);
        assert_eq!(order, ["one", "two"]);
        assert_eq!(seen["one"], 0);
        assert_eq!(seen["two"], 1);
    }

    #[test]
    fn a_complete_reply_lands_on_the_original_positions() {
        let reply = r#"{"lines":[
            {"index":0,"source":"one","text":"一"},
            {"index":1,"source":"two","text":"二"}]}"#;
        let out = place(reply, &["one", "two"]).expect("placed");
        assert_eq!(out, ["一", "二"]);
    }

    #[test]
    fn a_short_reply_is_rejected_rather_than_shifting_every_later_line() {
        // The failure that makes this check worth having: two lines came back
        // for three sent. Accepting it would put each remaining translation
        // under the wrong lyric, silently, for the rest of the song.
        let reply = r#"{"lines":[
            {"index":0,"source":"one","text":"一"},
            {"index":1,"source":"two","text":"二"}]}"#;
        let err = place(reply, &["one", "two", "three"]).unwrap_err();
        assert!(err.contains("dropped 1"), "{err}");
    }

    #[test]
    fn a_reply_that_slipped_a_line_is_caught_by_the_echo() {
        // What the index set cannot see: three entries, numbered 0, 1, 2,
        // nothing missing or repeated — but every translation is of the line
        // after the one it is filed under.
        let reply = r#"{"lines":[
            {"index":0,"source":"two","text":"二"},
            {"index":1,"source":"three","text":"三"},
            {"index":2,"source":"three","text":"三"}]}"#;
        let err = place(reply, &["one", "two", "three"]).unwrap_err();
        assert!(err.contains("under line 0"), "{err}");
    }

    #[test]
    fn an_echo_that_differs_only_in_spacing_is_still_that_line() {
        let reply = r#"{"lines":[{"index":0,"source":"hold on tight","text":"抓紧"}]}"#;
        let out = place(reply, &["hold  on   tight"]).expect("placed");
        assert_eq!(out, ["抓紧"]);
    }

    #[test]
    fn a_repeated_index_is_rejected() {
        let reply = r#"{"lines":[
            {"index":0,"source":"one","text":"一"},
            {"index":0,"source":"one","text":"壹"}]}"#;
        let err = place(reply, &["one", "two"]).unwrap_err();
        assert!(err.contains("repeated"), "{err}");
    }

    #[test]
    fn an_out_of_range_index_is_rejected() {
        let reply = r#"{"lines":[{"index":7,"source":"seven","text":"七"}]}"#;
        let err = place(reply, &["one"]).unwrap_err();
        assert!(err.contains("indexed line 7"), "{err}");
    }

    #[test]
    fn a_reply_that_is_not_the_expected_shape_is_rejected() {
        assert!(place("not json", &["one"]).is_err());
        assert!(place("{}", &["one"]).is_err());
        // The echo is required, not optional: a reply without it can't be
        // checked, so it isn't one this file knows how to trust.
        assert!(place(r#"{"lines":[{"index":0,"text":"一"}]}"#, &["one"]).is_err());
    }

    #[test]
    fn a_marker_line_may_come_back_untranslated() {
        let reply = r#"{"lines":[
            {"index":0,"source":"[Chorus]","text":""},
            {"index":1,"source":"one","text":"一"}]}"#;
        let out = place(reply, &["[Chorus]", "one"]).expect("placed");
        assert_eq!(out, ["", "一"]);
    }

    /// Hits the real API. Needs a key:
    /// `ANTHROPIC_API_KEY=… cargo test -p ytm-core llm -- --ignored`
    #[tokio::test]
    #[ignore = "network + api key"]
    async fn live_a_split_sentence_is_read_as_one() {
        let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
            eprintln!("no ANTHROPIC_API_KEY — skipping");
            return;
        };
        let ai = Ai {
            model: "claude-haiku-4-5".to_string(),
            api_key,
        };
        // Lines 0+1 are one sentence; line 3 stands alone. The free endpoint
        // reads line 0 as a bare noun phrase — this path must not. The repeat
        // at the end is translated once and reused.
        let lines: Vec<String> = [
            "君の名前を",
            "小さく呼んでいた",
            "",
            "風が冷たい",
            "夜が明けるまで",
            "君の名前を",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let out = translate(&lines, "zh", &ai).await.expect("translated");
        eprintln!("{out:#?}");

        assert_eq!(out.len(), lines.len(), "one entry per input line");
        assert!(
            out[2].is_empty(),
            "a blank line is never sent and stays blank"
        );
        for i in [0, 1, 3, 4, 5] {
            assert!(!out[i].is_empty(), "line {i} came back empty");
        }
        assert_eq!(out[0], out[5], "the repeat differs from the original");
    }

    #[test]
    fn an_api_error_body_yields_its_message() {
        let body =
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: bogus"}}"#;
        assert_eq!(api_error(body), "model: bogus");
        // Not the expected shape — fall back to the raw body rather than "".
        assert_eq!(api_error("upstream exploded"), "upstream exploded");
    }
}
