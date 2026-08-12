//! Whole-song lyric translation via the Anthropic Messages API, or DeepSeek's
//! compatible one — see [`Provider`], picked from the name of the environment
//! variable the key comes out of.
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
//! Neither check sees meaning move between lines, which is what happens when
//! the model reads two stanzas as one passage — hence [`numbered`], which keeps
//! the song's blank lines as separators.
//!
//! Cost: repeated lines are sent once, so a chorus is translated once however
//! often it comes round. Measured over three runs of a 39-line Japanese song
//! into Chinese, Haiku 4.5 came to 0.59¢; `deepseek-chat` on a 44-line one came
//! to 0.04¢ — see the `usage` line each request logs. `app.rs` keeps the
//! result, for this session and the next.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

const API_VERSION: &str = "2023-06-01";

/// Which API the key belongs to.
///
/// DeepSeek serves the Messages API shape at its own host, so the request built
/// here is the same one either way — only the URL, the auth header, the output
/// cap and whether the schema can be enforced differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    Anthropic,
    DeepSeek,
}

impl Provider {
    /// The provider a key variable names — `DEEPSEEK_API_KEY` is DeepSeek's.
    /// Anything else is Anthropic, which is what it was before this existed.
    #[must_use]
    pub fn for_key_env(name: &str) -> Self {
        if name.to_ascii_uppercase().contains("DEEPSEEK") {
            Self::DeepSeek
        } else {
            Self::Anthropic
        }
    }

    /// The provider a model id belongs to, or `None` for a name neither
    /// family claims — a snapshot id, a proxy's own naming.
    #[must_use]
    pub fn of_model(model: &str) -> Option<Self> {
        let model = model.trim().to_ascii_lowercase();
        if model.starts_with("deepseek") {
            Some(Self::DeepSeek)
        } else if model.starts_with("claude") {
            Some(Self::Anthropic)
        } else {
            None
        }
    }

    #[must_use]
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            Self::DeepSeek => "https://api.deepseek.com/anthropic/v1/messages",
        }
    }

    /// What `lyrics.ai-model` falls back to for this provider.
    #[must_use]
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-haiku-4-5",
            Self::DeepSeek => "deepseek-chat",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
        }
    }

    /// Whether the reply's shape can be *constrained* rather than asked for.
    /// Only Anthropic's endpoint takes `output_config`; DeepSeek's compatible
    /// one doesn't, so there the schema goes in the prompt and [`json_object`]
    /// digs the reply out of whatever wrapping came with it.
    fn structured_output(self) -> bool {
        self == Self::Anthropic
    }

    /// Ceiling on `max_tokens`. DeepSeek rejects a request asking for more
    /// than its own limit, so the cap is the provider's, not ours.
    fn token_ceiling(self) -> usize {
        match self {
            Self::Anthropic => 32000,
            Self::DeepSeek => 8192,
        }
    }
}

/// Output cap per distinct line, with a floor and a ceiling. Measured at ~27
/// tokens a line (the echo, the translation and the JSON around them); the rest
/// is margin for longer scripts. Headroom is free — this is a cap, not a
/// reservation — and hitting it fails the whole request, so the floor is
/// generous.
const TOKENS_PER_LINE: usize = 200;
const MIN_TOKENS: usize = 8192;

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
Only a marker — `[Chorus]`, `(instrumental)`, a bare `♪` — gets an empty
`text`. A line with words in it always comes back with words in it.

Lyric lines break on singing breath, not on grammar: one sentence often runs
across two or three lines. Read the whole sentence for its sense, then put each
line's own words on that line and nowhere else.

Where {target} would order those words differently, keep them with their lines
anyway. Two lines that read a little oddly one after the other are right; one
line carrying the whole sentence while its neighbour carries nothing is wrong,
and so is padding that neighbour with words the song never sang.

A blank line in the list is a break in the song, not a line to translate. No
sentence runs across one, and no line's meaning may move across one.

Keep connectives and contrast markers. If a line ends in one — \"but\", \"though\",
\"because\", \"and yet\" — that line's translation carries it too. Distributing a
sentence across lines must not drop the word that joins them.

Translate the imagery, not the idiom. Keep the register of the original: if it
is plain, stay plain. Do not add interpretation the source does not carry, and
do not explain the metaphor.";

/// Added to [`SYSTEM`] where the schema can't be enforced by the API, asking
/// for what `output_config` would otherwise guarantee.
const JSON_ONLY: &str = "

Reply with JSON and nothing else — no prose around it, no markdown fence:

{\"lines\":[{\"index\":0,\"source\":\"…\",\"text\":\"…\"}]}

Every entry carries all three keys and no others, `index` a number rather than
a string, `source` first so the line is copied before it is translated.";

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
    /// Whose key it is — decided by what `lyrics.ai-key-env` names.
    pub provider: Provider,
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

/// The block of lines sent to the model: `index\ttext` per distinct line, with
/// the song's blank lines kept as bare separators.
///
/// The separators are why this walks the original rather than `order`. Without
/// them the model reads two stanzas as one passage and redistributes meaning
/// across the join — "until dawn" landing on the line before the one that says
/// it — which the alignment checks cannot see, because every index and echo is
/// still correct.
fn numbered(lines: &[String], seen: &HashMap<&str, usize>) -> String {
    let mut out = String::new();
    let mut written = vec![false; seen.len()];
    let mut gap = false;
    for line in lines {
        let text = line.trim();
        if text.is_empty() {
            gap = true;
            continue;
        }
        let Some(&i) = seen.get(text) else { continue };
        if std::mem::replace(&mut written[i], true) {
            continue;
        }
        if !out.is_empty() && gap {
            out.push('\n');
        }
        out.push_str(&format!("{i}\t{text}\n"));
        gap = false;
    }
    out.truncate(out.trim_end().len());
    out
}

/// Translates every line into `to`, one entry per input line.
pub async fn translate(lines: &[String], to: &str, ai: &Ai) -> Result<Vec<String>, String> {
    let (order, seen) = distinct(lines);
    if order.is_empty() {
        return Ok(vec![String::new(); lines.len()]);
    }

    let numbered = numbered(lines, &seen);
    // The name, not the code: asked for `zh`, Haiku answers in English.
    let target = super::language_name(to).unwrap_or(to);
    let mut system = SYSTEM.replace("{target}", target);
    let user = USER
        .replace("{count}", &order.len().to_string())
        .replace("{last}", &(order.len() - 1).to_string())
        .replace("{numbered}", &numbered);

    let max_tokens = (order.len() * TOKENS_PER_LINE).clamp(MIN_TOKENS, ai.provider.token_ceiling());

    // Thinking is off because on a model that does it by default it comes out
    // of `max_tokens` — `deepseek-v4-flash` spent all 8192 of them reasoning,
    // emitted no text block at all, and took 80s to fail. Off: 3.7s, 541
    // tokens. Nothing here needs deliberation; alignment is a rule, not a
    // judgement. Both providers accept the field.
    let mut body = json!({
        "model": ai.model,
        "max_tokens": max_tokens,
        "thinking": { "type": "disabled" },
        "messages": [{ "role": "user", "content": user }],
    });
    if ai.provider.structured_output() {
        body["output_config"] = json!({ "format": { "type": "json_schema", "schema": schema() } });
    } else {
        system.push_str(JSON_ONLY);
    }
    body["system"] = json!(system);

    // DeepSeek's compatible endpoint takes the bearer form; Anthropic's takes
    // its own header and rejects a request carrying both.
    let request = match ai.provider {
        Provider::Anthropic => client()?
            .post(ai.provider.endpoint())
            .header("x-api-key", &ai.api_key),
        Provider::DeepSeek => client()?
            .post(ai.provider.endpoint())
            .bearer_auth(&ai.api_key),
    };

    let started = std::time::Instant::now();
    let response = request
        .header("anthropic-version", API_VERSION)
        .json(&body)
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
    log_usage(&reply, ai, lines.len(), order.len(), started.elapsed());
    match reply.get("stop_reason").and_then(Value::as_str) {
        Some("refusal") => return Err("the model declined this request".to_string()),
        // Naming the cap matters: the usual cause is a model that thinks out of
        // the same budget, and the number is the thing to change.
        Some("max_tokens") => {
            return Err(format!(
                "reply hit the {max_tokens}-token cap and was truncated"
            ));
        }
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

    let done = place(json_object(text), &order)?;
    Ok(lines
        .iter()
        .map(|line| {
            seen.get(line.trim())
                .map(|i| done[*i].clone())
                .unwrap_or_default()
        })
        .collect())
}

/// What the request cost and how long it took, so `app.log` can answer both
/// without guesswork.
fn log_usage(reply: &Value, ai: &Ai, lines: usize, distinct: usize, took: Duration) {
    let count = |field: &str| {
        reply
            .get("usage")
            .and_then(|u| u.get(field))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    log::debug!(
        "translate: {} used {} in / {} out for {distinct} of {lines} lines in {:.1}s",
        ai.model,
        count("input_tokens"),
        count("output_tokens"),
        took.as_secs_f64(),
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

/// The JSON object inside a reply, dropping a markdown fence or a sentence of
/// preamble around it. A no-op where the shape was constrained by the API.
fn json_object(text: &str) -> &str {
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => &text[start..=end],
        _ => text,
    }
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

    fn song(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn blanks_and_repeats_are_never_sent() {
        let lines = song(&["one", "", "two", "  one  ", "two"]);
        let (order, seen) = distinct(&lines);
        assert_eq!(order, ["one", "two"]);
        assert_eq!(seen["one"], 0);
        assert_eq!(seen["two"], 1);
    }

    #[test]
    fn a_stanza_break_survives_as_a_blank_line() {
        // The break is the whole point: without it the model reads both
        // stanzas as one passage and moves meaning across the join.
        let lines = song(&["one", "two", "", "three"]);
        let (_, seen) = distinct(&lines);
        assert_eq!(numbered(&lines, &seen), "0\tone\n1\ttwo\n\n2\tthree");
    }

    #[test]
    fn a_repeat_is_dropped_but_the_break_around_it_is_not() {
        let lines = song(&["one", "", "one", "", "two"]);
        let (_, seen) = distinct(&lines);
        assert_eq!(numbered(&lines, &seen), "0\tone\n\n1\ttwo");
    }

    #[test]
    fn leading_and_trailing_blanks_add_nothing() {
        let lines = song(&["", "  ", "one", "two", "", ""]);
        let (_, seen) = distinct(&lines);
        assert_eq!(numbered(&lines, &seen), "0\tone\n1\ttwo");
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

    /// Hits the real API. Needs a key — either provider's:
    /// `ANTHROPIC_API_KEY=… cargo test -p ytm-core llm -- --ignored`
    #[tokio::test]
    #[ignore = "network + api key"]
    async fn live_a_split_sentence_is_read_as_one() {
        let found = ["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"]
            .into_iter()
            .find_map(|name| {
                let key = std::env::var(name).ok().filter(|k| !k.trim().is_empty())?;
                Some((Provider::for_key_env(name), key))
            });
        let Some((provider, api_key)) = found else {
            eprintln!("no ANTHROPIC_API_KEY or DEEPSEEK_API_KEY — skipping");
            return;
        };
        let ai = Ai {
            model: provider.default_model().to_string(),
            api_key,
            provider,
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
    fn the_key_variable_picks_the_provider() {
        assert_eq!(
            Provider::for_key_env("DEEPSEEK_API_KEY"),
            Provider::DeepSeek
        );
        assert_eq!(Provider::for_key_env("my_deepseek_key"), Provider::DeepSeek);
        // Anything else is what it always was.
        assert_eq!(
            Provider::for_key_env("ANTHROPIC_API_KEY"),
            Provider::Anthropic
        );
        assert_eq!(Provider::for_key_env("WORK_KEY"), Provider::Anthropic);
        assert_eq!(Provider::for_key_env(""), Provider::Anthropic);
    }

    #[test]
    fn a_model_is_claimed_only_by_the_family_that_names_it() {
        assert_eq!(
            Provider::of_model("deepseek-chat"),
            Some(Provider::DeepSeek)
        );
        assert_eq!(
            Provider::of_model("claude-haiku-4-5"),
            Some(Provider::Anthropic)
        );
        // Not ours to reassign — a gateway's own naming, or a snapshot id.
        assert_eq!(Provider::of_model("gpt-4o"), None);
        assert_eq!(Provider::of_model(""), None);
    }

    #[test]
    fn a_fenced_reply_is_still_a_reply() {
        // DeepSeek's endpoint takes no schema, so the JSON arrives however the
        // model felt like wrapping it.
        let fenced = "```json\n{\"lines\":[{\"index\":0,\"source\":\"one\",\"text\":\"一\"}]}\n```";
        assert_eq!(
            place(json_object(fenced), &["one"]).expect("placed"),
            ["一"]
        );

        let chatty = "Here you go:\n{\"lines\":[{\"index\":0,\"source\":\"one\",\"text\":\"一\"}]}";
        assert_eq!(
            place(json_object(chatty), &["one"]).expect("placed"),
            ["一"]
        );

        // Nothing object-shaped in it — left alone for `place` to reject.
        assert_eq!(json_object("sorry, I can't"), "sorry, I can't");
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
