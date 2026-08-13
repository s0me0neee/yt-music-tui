//! `config.toml` — the settings a user edits by hand.
//!
//! Read once at startup. Everything here has a working default, and a file
//! that is missing, unreadable or malformed falls back to those defaults with a
//! warning in the log: a typo in a config file must never stop the music.

use serde::{Deserialize, Deserializer};

use crate::session::config_toml_path;

/// The furthest [`Lyrics::offset`] may be pushed, in seconds.
///
/// Sync corrections are fractions of a second; anything past half a minute is a
/// typo (a millisecond value, most likely) rather than an intention, and would
/// leave the panel showing lyrics from a different part of the song entirely.
const MAX_LYRICS_OFFSET: f64 = 30.0;

/// Written on a fresh install, and in place of the bare header that older
/// versions left behind. Every setting is commented out at its default, so the
/// file doubles as the documentation for what can be set.
pub const TEMPLATE: &str = "\
# yt-music-tui configuration

[lyrics]
# Shift every lyric line, in seconds, against the timings the lrclib record
# carries. Negative switches lines *early*, positive switches them *late*.
# Applies to every song. Fractions are the useful range — try -0.3 if lines
# consistently arrive a moment after they are sung.
#offset = 0.0
# Show a translation under each lyric, in this language: \"zh\", \"fr\", \"en\".
# Press `i` in the lyrics panel to turn it on and off. Empty means the key
# does nothing, which is the default — translation is never fetched unasked.
#translate-to = \"\"

# Offer an AI model as a second translator, on `I` (shift-i). `i` stays the free
# one whatever this is set to. The whole song goes in one request, so a sentence
# spanning several lyric lines is read as a sentence — the free endpoint splits
# on newlines and cannot. Costs under a cent per song, charged to your own API
# key; nothing is sent to any API until you press `I`.
#ai-translation = false
# Which model, when the above is on.
#ai-model = \"claude-haiku-4-5\"
# Environment variable holding the API key. The key itself is never read from
# this file, so config.toml stays safe to copy around.
#
# Its name also picks the provider: name a DeepSeek variable and requests go to
# DeepSeek's Anthropic-compatible endpoint instead. Two lines is the whole
# switch —
#   ai-model = \"deepseek-chat\"
#   ai-key-env = \"DEEPSEEK_API_KEY\"
# and changing only the key line is enough: a Claude model left behind a
# DeepSeek key becomes that provider's default.
#ai-key-env = \"ANTHROPIC_API_KEY\"

[ui]
# Show cover art in the search panel, using the kitty graphics protocol. Only
# ever attempted on a terminal known to speak it — kitty, Ghostty, WezTerm —
# so the default is safe everywhere; set false to turn it off on one that does.
#covers = true

[auth]
# Renew an expired session by re-running yt-dlp against the browser below,
# instead of asking which method to use. Set false to always be asked.
#auto-reauth = true
# The browser yt-dlp reads cookies from. Filled in for you the first time you
# set up with one; edit it if you switch browsers.
#cookie-browser = \"firefox\"
";

/// The one-line file older versions wrote. Recognised so it can be replaced
/// with [`TEMPLATE`]; anything else is the user's and is never touched.
pub(crate) const LEGACY_STUB: &str = "# yt-music-tui configuration\n";

/// Accepts `-1` as well as `-1.0`.
///
/// TOML types those differently and serde would reject the integer, dropping
/// the user's whole config back to defaults over a missing `.0`.
fn seconds<'de, D: Deserializer<'de>>(de: D) -> std::result::Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Float(f64),
        Int(i64),
    }
    Ok(match Either::deserialize(de)? {
        Either::Float(f) => f,
        Either::Int(i) => i as f64,
    })
}

/// `offset`, as a type [`field`] can read.
#[derive(Deserialize)]
struct Seconds(#[serde(deserialize_with = "seconds")] pub f64);

/// Every section and key this file understands, so a misspelling can be
/// pointed out instead of silently doing nothing.
const KNOWN: &[(&str, &[&str])] = &[
    (
        "lyrics",
        &[
            "offset",
            "translate-to",
            "ai-translation",
            "ai-model",
            "ai-key-env",
        ],
    ),
    ("ui", &["covers"]),
    ("auth", &["auto-reauth", "cookie-browser"]),
];

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub lyrics: Lyrics,
    pub ui: Ui,
    pub auth: Auth,
}

#[derive(Debug, Clone)]
pub struct Ui {
    /// Show cover art in the search panel where the terminal can draw it.
    ///
    /// On by default because it is *asked for* rather than assumed: the
    /// frontend only tries where it recognises the terminal, so a terminal that
    /// would print the escape sequences as text never gets them. This setting
    /// is for turning it off on one that could.
    pub covers: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self { covers: true }
    }
}

#[derive(Debug, Clone)]
pub struct Auth {
    /// Renew an expired session with yt-dlp instead of asking. Only ever does
    /// anything once [`Auth::cookie_browser`] is known, which is why it can
    /// default on: until a browser has been chosen there is nothing to run.
    pub auto_reauth: bool,
    /// The browser yt-dlp reads cookies from, lowercase — `"firefox"`. Written
    /// here the first time setup completes with one, so the next expiry needs
    /// no conversation. Empty when setup was done by pasting a cURL command,
    /// which yt-dlp can't repeat.
    pub cookie_browser: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            auto_reauth: true,
            cookie_browser: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lyrics {
    /// Seconds to shift every lyric line by. Negative is early, positive late.
    pub offset: f64,
    /// Language code to translate lyrics into — `"zh"`, `"fr"`, `"en"`. Empty
    /// disables translation entirely, which is the default: nothing is ever
    /// sent to a translation service unless a language is named here.
    ///
    /// Normalised by [`Config::validated`] to the spelling the endpoint uses,
    /// and cleared if it isn't a language the endpoint knows.
    pub translate_to: String,
    /// Translate with Claude instead of the free endpoint. `false` — the
    /// default — is the original behaviour in every respect: the `rust-translate`
    /// path, no API key, nothing billed, nothing sent to any API.
    ///
    /// Turned off again by [`Config::validated`] when no API key can be found,
    /// so a half-finished setup falls back rather than failing on every song.
    pub ai_translation: bool,
    /// Model to translate with. Only consulted when [`Self::ai_translation`]
    /// is on.
    pub ai_model: String,
    /// Environment variable holding the API key. Named here rather than
    /// holding the key itself, so `config.toml` stays safe to share and the
    /// secret lives wherever the user already keeps secrets.
    ///
    /// Its *name* also picks the provider — `DEEPSEEK_API_KEY` goes to
    /// DeepSeek's Anthropic-compatible endpoint. A key belongs to one API, so
    /// there is nothing else to set.
    pub ai_key_env: String,
}

impl Default for Lyrics {
    fn default() -> Self {
        Self {
            offset: 0.0,
            translate_to: String::new(),
            // Off, so the default install behaves exactly as it did before this
            // backend existed. The model and key-variable names default to
            // something usable, so turning the flag on is the only edit needed.
            ai_translation: false,
            ai_model: "claude-haiku-4-5".to_string(),
            ai_key_env: "ANTHROPIC_API_KEY".to_string(),
        }
    }
}

impl Lyrics {
    /// The API key, read from the environment variable [`Self::ai_key_env`]
    /// names. `None` when unset or blank.
    #[must_use]
    pub fn ai_key(&self) -> Option<String> {
        let name = self.ai_key_env.trim();
        if name.is_empty() {
            return None;
        }
        std::env::var(name).ok().filter(|k| !k.trim().is_empty())
    }

    /// Whether the AI backend is usable: asked for, a model named, and its key
    /// found. `I` is offered only when this is true.
    #[must_use]
    pub fn ai_available(&self) -> bool {
        self.ai_translation && !self.ai_model.is_empty() && self.ai_key().is_some()
    }

    /// Whose API the key is for, read off the variable's name.
    #[must_use]
    pub fn ai_provider(&self) -> crate::translate::Provider {
        crate::translate::Provider::for_key_env(&self.ai_key_env)
    }

    /// The translator to use. `use_ai` is the user's choice — `i` against `I` —
    /// and falls back to the free endpoint when the AI side isn't configured.
    #[must_use]
    pub fn backend(&self, use_ai: bool) -> crate::translate::Backend {
        crate::translate::Backend {
            to: self.translate_to.clone(),
            ai: self
                .ai_key()
                .filter(|_| use_ai && self.ai_available())
                .map(|api_key| crate::translate::Ai {
                    model: self.ai_model.clone(),
                    api_key,
                    provider: self.ai_provider(),
                }),
        }
    }
}

/// One setting, or its default with a word in the log about why.
///
/// `config.toml` is hand-edited, so `offset = "0.5"` and `ai-translation =
/// "true"` are the kind of thing that turns up in it.
fn field<T: serde::de::DeserializeOwned>(
    section: Option<&toml::Table>,
    key: &str,
    default: T,
) -> T {
    let Some(value) = section.and_then(|s| s.get(key)) else {
        return default;
    };
    T::deserialize(value.clone()).unwrap_or_else(|e| {
        log::warn!("config: {key} = {value:?} won't do ({e}) — using the default");
        default
    })
}

/// Anything in the file this app doesn't read, named as the user wrote it.
///
/// Silence here would be worse than it sounds: `translate_to` for
/// `translate-to` parses perfectly well and simply does nothing, leaving the
/// user looking at a setting that appears not to work.
fn unknown(table: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    for (name, value) in table {
        let Some((_, keys)) = KNOWN.iter().find(|(known, _)| known == name) else {
            out.push(name.clone());
            continue;
        };
        let Some(section) = value.as_table() else {
            out.push(name.clone());
            continue;
        };
        out.extend(
            section
                .keys()
                .filter(|key| !keys.contains(&key.as_str()))
                .map(|key| format!("{name}.{key}")),
        );
    }
    out
}

impl Config {
    /// Reads `config.toml`, falling back to defaults on any problem.
    pub fn load() -> Self {
        let path = config_toml_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!(
                    "config: {} is unreadable ({e}) — using defaults",
                    path.display()
                );
                return Self::default();
            }
        };

        // Only a syntax error costs the whole file; a value that is the wrong
        // *type* costs its own key and nothing else, so a stray quote around a
        // number can't take the browser setting down with it.
        match raw.parse::<toml::Table>() {
            Ok(table) => {
                for name in unknown(&table) {
                    log::warn!("config: {name} is not a setting this app reads — ignoring it");
                }
                Self::from_table(&table).validated()
            }
            Err(e) => {
                log::warn!(
                    "config: {} is not valid TOML ({e}) — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Reads each setting on its own, so one unusable value doesn't discard the
    /// rest of the file.
    fn from_table(table: &toml::Table) -> Self {
        let lyrics = table.get("lyrics").and_then(toml::Value::as_table);
        let ui = table.get("ui").and_then(toml::Value::as_table);
        let auth = table.get("auth").and_then(toml::Value::as_table);
        let d = Self::default();
        Self {
            lyrics: Lyrics {
                offset: field(lyrics, "offset", Seconds(d.lyrics.offset)).0,
                translate_to: field(lyrics, "translate-to", d.lyrics.translate_to),
                ai_translation: field(lyrics, "ai-translation", d.lyrics.ai_translation),
                ai_model: field(lyrics, "ai-model", d.lyrics.ai_model),
                ai_key_env: field(lyrics, "ai-key-env", d.lyrics.ai_key_env),
            },
            ui: Ui {
                covers: field(ui, "covers", d.ui.covers),
            },
            auth: Auth {
                auto_reauth: field(auth, "auto-reauth", d.auth.auto_reauth),
                cookie_browser: field(auth, "cookie-browser", d.auth.cookie_browser),
            },
        }
    }

    /// Replaces out-of-range values with usable ones, saying so in the log.
    fn validated(mut self) -> Self {
        let offset = self.lyrics.offset;
        if !offset.is_finite() {
            log::warn!("config: lyrics.offset is not a number — ignoring it");
            self.lyrics.offset = 0.0;
        } else if offset.abs() > MAX_LYRICS_OFFSET {
            let clamped = offset.clamp(-MAX_LYRICS_OFFSET, MAX_LYRICS_OFFSET);
            log::warn!("config: lyrics.offset {offset}s is out of range — clamping to {clamped}s");
            self.lyrics.offset = clamped;
        }

        if self.lyrics.offset != 0.0 {
            log::info!("config: lyrics.offset {}s", self.lyrics.offset);
        }

        // Checked here rather than at the point of use, because an unknown
        // code is not an error the endpoint reports: it answers `tl=zzz` with
        // the text unchanged, which looks like a translation that silently
        // never works. Better to say so once, at startup, and stay off.
        let want = self.lyrics.translate_to.trim().to_string();
        self.lyrics.translate_to = match crate::translate::normalise_language(&want) {
            Some(code) => {
                log::info!("config: lyrics.translate-to {code:?}");
                code.to_string()
            }
            None => {
                if !want.is_empty() {
                    log::warn!(
                        "config: lyrics.translate-to {want:?} is not a language code — \
                         translation is off"
                    );
                }
                String::new()
            }
        };

        // A model with no key behind it would fail on the first lyric and keep
        // failing, so it is turned off here instead — the free endpoint still
        // works, and the log says why the better one isn't being used.
        self.lyrics.ai_model = self.lyrics.ai_model.trim().to_string();
        self.lyrics.ai_key_env = self.lyrics.ai_key_env.trim().to_string();

        // The key's variable names the provider, so a model from the *other*
        // provider can only be left over from before the key was changed — and
        // sending it would 404 on every song. Substituting the provider's own
        // default is what the user meant by swapping the key; a name neither
        // family claims is left alone, since it may well be a snapshot id.
        let provider = self.lyrics.ai_provider();
        if let Some(named) = crate::translate::Provider::of_model(&self.lyrics.ai_model)
            && named != provider
        {
            log::warn!(
                "config: lyrics.ai-model {:?} is not a {} model, and ${} is — using {:?}",
                self.lyrics.ai_model,
                provider.label(),
                self.lyrics.ai_key_env,
                provider.default_model()
            );
            self.lyrics.ai_model = provider.default_model().to_string();
        }

        if self.lyrics.ai_translation {
            if self.lyrics.ai_model.is_empty() {
                log::warn!("config: lyrics.ai-translation is on but ai-model is empty — off");
                self.lyrics.ai_translation = false;
            } else if self.lyrics.ai_key().is_some() {
                log::info!(
                    "config: lyrics.ai-translation on, {} {:?} (key from ${})",
                    provider.label(),
                    self.lyrics.ai_model,
                    self.lyrics.ai_key_env
                );
            } else {
                log::warn!(
                    "config: lyrics.ai-translation is on but ${} is empty or unset — \
                     falling back to the free translator",
                    self.lyrics.ai_key_env
                );
                self.lyrics.ai_translation = false;
            }
        }

        self
    }
}

/// Records the browser yt-dlp just extracted cookies from, so the next expiry
/// can be handled without asking.
///
/// Rewrites `config.toml` in place, preserving its comments, key order and
/// formatting — it is a file the user edits, and setup succeeding is no reason
/// to reformat it. A no-op when the value is already right, and a logged
/// warning rather than an error when the file can't be parsed: failing to
/// record a preference must not fail the authentication that just worked.
pub fn remember_cookie_browser(browser: &str) {
    let path = config_toml_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => {
            log::warn!(
                "config: can't read {} to record the browser ({e})",
                path.display()
            );
            return;
        }
    };

    let mut doc = match raw.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(e) => {
            log::warn!(
                "config: {} is not valid TOML ({e}) — leaving it alone",
                path.display()
            );
            return;
        }
    };

    if doc
        .get("auth")
        .and_then(|a| a.get("cookie-browser"))
        .and_then(|b| b.as_str())
        == Some(browser)
    {
        return;
    }

    if !doc.get("auth").is_some_and(|a| a.is_table()) {
        doc["auth"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["auth"]["cookie-browser"] = toml_edit::value(browser);

    // Atomically, like every other file the app writes: this one is the
    // *user's*, comments and all, and a truncated config.toml would cost them
    // both their settings and the browser this call exists to record.
    match crate::session::write_private(&path, &doc.to_string()) {
        Ok(()) => log::info!("config: remembered cookie-browser = {browser:?}"),
        Err(e) => log::warn!("config: can't write {} ({e})", path.display()),
    }
}

impl Lyrics {
    /// The playback position the lyric timings should be looked up against.
    ///
    /// A record's timestamps describe when each line is sung; the offset says
    /// how far from that the *display* should sit. Shifting the clock we hand
    /// the lookup, rather than the timestamps themselves, keeps cached records
    /// untouched and costs nothing per line.
    ///
    /// A negative offset runs the lookup ahead of playback, so lines arrive
    /// early; a positive one holds it back.
    pub fn lyric_time(&self, elapsed: f64) -> f64 {
        elapsed - self.offset
    }

    /// How the offset reads in the UI, or `None` when there is nothing to say.
    pub fn offset_label(&self) -> Option<String> {
        (self.offset != 0.0).then(|| format!("{:+.1}s", self.offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipping path, minus the file read: parse, warn, read each key,
    /// validate.
    fn parse(src: &str) -> Config {
        let table = src.parse::<toml::Table>().expect("valid toml");
        Config::from_table(&table).validated()
    }

    // ── ai translation ────────────────────────────────────────────────────

    #[test]
    fn ai_translation_is_off_unless_asked_for() {
        // The whole point of the flag: an install nobody has configured behaves
        // exactly as it did before the backend existed, and bills nothing.
        assert!(!parse("").lyrics.ai_translation);
        assert!(!parse("[lyrics]\n").lyrics.ai_translation);
        assert!(!Config::default().lyrics.ai_translation);
        assert!(!parse(TEMPLATE).lyrics.ai_translation);
    }

    #[test]
    fn the_flag_alone_is_enough_to_turn_it_on() {
        // Model and key-variable names default to something usable, so nobody
        // has to name a model to get started.
        let lyrics = parse("[lyrics]\nai-translation = true\n").lyrics;
        assert_eq!(lyrics.ai_model, "claude-haiku-4-5");
        assert_eq!(lyrics.ai_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn the_flag_off_means_the_free_backend_whatever_else_is_set() {
        // Independent of the environment: with the flag off, no key is even
        // looked for, so a key lying around in the shell can't switch backends.
        let lyrics =
            parse("[lyrics]\nai-translation = false\nai-model = \"claude-opus-5\"\n").lyrics;
        assert!(lyrics.backend(true).ai.is_none());
    }

    #[test]
    fn an_empty_model_turns_the_flag_back_off() {
        let lyrics = parse("[lyrics]\nai-translation = true\nai-model = \"\"\n").lyrics;
        assert!(!lyrics.ai_translation);
        assert!(lyrics.backend(true).ai.is_none());
    }

    #[test]
    fn a_key_variable_that_names_nothing_yields_no_key() {
        let lyrics =
            parse("[lyrics]\nai-translation = true\nai-key-env = \"YTM_NO_SUCH_VAR_XYZ\"\n").lyrics;
        assert!(lyrics.ai_key().is_none());
        assert!(lyrics.backend(true).ai.is_none());
    }

    // ── providers ─────────────────────────────────────────────────────────

    #[test]
    fn the_key_variable_is_the_whole_provider_switch() {
        let lyrics = parse(
            "[lyrics]\nai-translation = true\nai-model = \"deepseek-chat\"\n\
             ai-key-env = \"DEEPSEEK_API_KEY\"\n",
        )
        .lyrics;
        assert_eq!(lyrics.ai_provider(), crate::translate::Provider::DeepSeek);
        assert_eq!(lyrics.ai_model, "deepseek-chat");
        // Untouched, so the default install still goes to Anthropic.
        assert_eq!(
            parse("").lyrics.ai_provider(),
            crate::translate::Provider::Anthropic
        );
    }

    #[test]
    fn changing_only_the_key_leaves_no_model_from_the_wrong_provider() {
        // What the user actually does: swap the variable name and leave the
        // model line alone. Sending `claude-haiku-4-5` to DeepSeek would fail
        // on every song.
        let lyrics = parse("[lyrics]\nai-key-env = \"DEEPSEEK_API_KEY\"\n").lyrics;
        assert_eq!(lyrics.ai_model, "deepseek-chat");

        // And back the other way.
        let lyrics = parse("[lyrics]\nai-model = \"deepseek-chat\"\n").lyrics;
        assert_eq!(lyrics.ai_model, "claude-haiku-4-5");
    }

    #[test]
    fn a_model_neither_family_claims_is_left_as_written() {
        // A snapshot id or a gateway's own naming is the user's business.
        let lyrics = parse(
            "[lyrics]\nai-model = \"my-proxy/whatever\"\nai-key-env = \"DEEPSEEK_API_KEY\"\n",
        )
        .lyrics;
        assert_eq!(lyrics.ai_model, "my-proxy/whatever");
    }

    #[test]
    fn an_empty_config_means_no_shift() {
        assert_eq!(parse("").lyrics.offset, 0.0);
        assert_eq!(parse("[lyrics]\n").lyrics.offset, 0.0);
        assert_eq!(Config::default().lyrics.offset, 0.0);
    }

    #[test]
    fn the_template_parses_and_is_all_defaults() {
        // Every setting in it is commented out, so it must read as untouched.
        let from_template = parse(TEMPLATE);
        assert_eq!(from_template.lyrics.offset, Config::default().lyrics.offset);
    }

    #[test]
    fn a_whole_number_of_seconds_is_accepted() {
        // TOML types `-1` as an integer; without help serde rejects it and the
        // whole file silently reverts to defaults.
        assert_eq!(parse("[lyrics]\noffset = -1\n").lyrics.offset, -1.0);
        assert_eq!(parse("[lyrics]\noffset = 2\n").lyrics.offset, 2.0);
        assert_eq!(parse("[lyrics]\noffset = -0.3\n").lyrics.offset, -0.3);
    }

    #[test]
    fn negative_is_early_and_positive_is_late() {
        let at = |offset| {
            Lyrics {
                offset,
                ..Default::default()
            }
            .lyric_time(10.0)
        };
        // Early: the lookup runs ahead of the clock, so the line due at 10.5s
        // is already active at 10s.
        assert_eq!(at(-0.5), 10.5);
        // Late: the lookup lags, so the line due at 10s is still to come.
        assert_eq!(at(0.5), 9.5);
        assert_eq!(at(0.0), 10.0);
    }

    #[test]
    fn absurd_offsets_are_clamped_rather_than_obeyed() {
        // A user typing milliseconds would otherwise land in another verse.
        assert_eq!(parse("[lyrics]\noffset = -300\n").lyrics.offset, -30.0);
        assert_eq!(parse("[lyrics]\noffset = 1000.0\n").lyrics.offset, 30.0);
        assert_eq!(parse("[lyrics]\noffset = nan\n").lyrics.offset, 0.0);
        assert_eq!(parse("[lyrics]\noffset = inf\n").lyrics.offset, 0.0);
    }

    // ── translation ───────────────────────────────────────────────────────

    #[test]
    fn translation_is_off_until_a_language_is_named() {
        assert_eq!(parse("").lyrics.translate_to, "");
        assert_eq!(parse("[lyrics]\noffset = 0.0\n").lyrics.translate_to, "");
        assert_eq!(Config::default().lyrics.translate_to, "");
        // The template names none, so a fresh install fetches nothing.
        assert_eq!(parse(TEMPLATE).lyrics.translate_to, "");
    }

    #[test]
    fn a_language_is_read_in_kebab_case_and_normalised() {
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \"zh\"\n")
                .lyrics
                .translate_to,
            "zh"
        );
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \" FR \"\n")
                .lyrics
                .translate_to,
            "fr"
        );
        // The one code with capitals in it.
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \"zh-tw\"\n")
                .lyrics
                .translate_to,
            "zh-TW"
        );
    }

    #[test]
    fn a_language_nobody_translates_into_turns_the_feature_off() {
        // Left set, this would look like a translation that never arrives:
        // the endpoint answers an unknown code with the input unchanged.
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \"chinese\"\n")
                .lyrics
                .translate_to,
            ""
        );
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \"\"\n").lyrics.translate_to,
            ""
        );
        assert_eq!(
            parse("[lyrics]\ntranslate-to = \"  \"\n")
                .lyrics
                .translate_to,
            ""
        );
    }

    #[test]
    fn a_bad_language_does_not_cost_the_offset() {
        let c = parse("[lyrics]\noffset = -0.4\ntranslate-to = \"nope\"\n");
        assert_eq!(c.lyrics.offset, -0.4);
        assert_eq!(c.lyrics.translate_to, "");
    }

    #[test]
    fn unknown_keys_do_not_discard_the_rest() {
        // Forward compatibility: a setting from a newer version, or a stray
        // key, must not cost the user the settings that are valid.
        let c = parse("[lyrics]\noffset = -0.4\nsomething_else = true\n");
        assert_eq!(c.lyrics.offset, -0.4);
    }

    // ── ui ────────────────────────────────────────────────────────────────

    #[test]
    fn covers_are_on_unless_turned_off() {
        assert!(Config::default().ui.covers);
        assert!(parse("").ui.covers);
        assert!(parse(TEMPLATE).ui.covers, "the template is all defaults");
        assert!(!parse("[ui]\ncovers = false\n").ui.covers);
        // A value of the wrong type costs the setting, not the file.
        let c = parse("[ui]\ncovers = \"no\"\n[lyrics]\noffset = -0.4\n");
        assert!(c.ui.covers);
        assert_eq!(c.lyrics.offset, -0.4);
    }

    // ── auth ──────────────────────────────────────────────────────────────

    #[test]
    fn auth_defaults_to_asking_nothing_it_cannot_answer() {
        let c = Config::default();
        // On by default, but inert until a browser is on record — there is
        // nothing to run yt-dlp against until then.
        assert!(c.auth.auto_reauth);
        assert!(c.auth.cookie_browser.is_empty());
        assert_eq!(parse("").auth.cookie_browser, "");
        assert!(parse("").auth.auto_reauth);
    }

    #[test]
    fn auth_settings_are_read_in_kebab_case() {
        // The names as they appear in the file, which is what the user types.
        let c = parse("[auth]\nauto-reauth = false\ncookie-browser = \"firefox\"\n");
        assert!(!c.auth.auto_reauth);
        assert_eq!(c.auth.cookie_browser, "firefox");
    }

    #[test]
    fn a_broken_auth_section_does_not_cost_the_lyrics_settings() {
        // Whole-file fallback would silently undo an offset the user tuned.
        let c = parse("[lyrics]\noffset = -0.4\n\n[auth]\nsomething-new = 1\n");
        assert_eq!(c.lyrics.offset, -0.4);
        assert!(c.auth.auto_reauth);
    }

    /// `remember_cookie_browser` against an arbitrary document, so the
    /// format-preserving behaviour can be checked without touching the real
    /// config file.
    fn record_browser(src: &str, browser: &str) -> String {
        let mut doc = src.parse::<toml_edit::DocumentMut>().expect("valid toml");
        if !doc.get("auth").is_some_and(|a| a.is_table()) {
            doc["auth"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        doc["auth"]["cookie-browser"] = toml_edit::value(browser);
        doc.to_string()
    }

    #[test]
    fn recording_the_browser_keeps_the_file_as_the_user_left_it() {
        let src = "\
# my notes
[lyrics]
# tuned by ear
offset = -0.35

[auth]
auto-reauth = true
";
        let out = record_browser(src, "firefox");
        assert!(out.contains("# my notes"), "comments survive: {out}");
        assert!(out.contains("# tuned by ear"));
        assert!(out.contains("offset = -0.35"), "values survive: {out}");
        assert!(out.contains("auto-reauth = true"));
        assert!(out.contains("cookie-browser = \"firefox\""));
        // And it still reads back as the same settings.
        let c = parse(&out);
        assert_eq!(c.lyrics.offset, -0.35);
        assert_eq!(c.auth.cookie_browser, "firefox");
    }

    #[test]
    fn recording_the_browser_creates_the_section_when_absent() {
        let out = record_browser("[lyrics]\noffset = 0.5\n", "chrome");
        let c = parse(&out);
        assert_eq!(c.auth.cookie_browser, "chrome");
        assert_eq!(c.lyrics.offset, 0.5);

        // Including into the shipped template, which has it commented out.
        let c = parse(&record_browser(TEMPLATE, "brave"));
        assert_eq!(c.auth.cookie_browser, "brave");
        assert!(c.auth.auto_reauth);
    }

    #[test]
    fn recording_the_browser_replaces_an_earlier_one() {
        let out = record_browser("[auth]\ncookie-browser = \"chrome\"\n", "firefox");
        assert_eq!(parse(&out).auth.cookie_browser, "firefox");
        assert_eq!(
            out.matches("cookie-browser").count(),
            1,
            "not duplicated: {out}"
        );
    }

    #[test]
    fn the_label_only_appears_when_there_is_a_shift() {
        assert_eq!(
            Lyrics {
                offset: 0.0,
                ..Default::default()
            }
            .offset_label(),
            None
        );
        assert_eq!(
            Lyrics {
                offset: -0.3,
                ..Default::default()
            }
            .offset_label()
            .as_deref(),
            Some("-0.3s")
        );
        assert_eq!(
            Lyrics {
                offset: 1.25,
                ..Default::default()
            }
            .offset_label()
            .as_deref(),
            Some("+1.2s")
        );
    }

    // ── a hand-edited file, edited by hand ────────────────────────────────

    #[test]
    fn a_value_of_the_wrong_type_costs_its_own_key_and_nothing_else() {
        // The realistic slip is a stray quote round a number. Losing the whole
        // file over it would take `cookie-browser` down too, and the next
        // expiry would ask to set up again for no reason.
        let c = parse(
            "[lyrics]\noffset = \"0.5\"\ntranslate-to = \"zh\"\n\
             [auth]\ncookie-browser = \"firefox\"\n",
        );
        assert_eq!(c.lyrics.offset, 0.0);
        assert_eq!(c.lyrics.translate_to, "zh");
        assert_eq!(c.auth.cookie_browser, "firefox");
    }

    #[test]
    fn every_setting_falls_back_on_its_own() {
        for src in [
            "[lyrics]\noffset = true\n",
            "[lyrics]\noffset = \"x\"\n",
            "[lyrics]\ntranslate-to = 5\n",
            "[lyrics]\ntranslate-to = [\"zh\"]\n",
            "[lyrics]\nai-translation = \"true\"\n",
            "[lyrics]\nai-model = 42\n",
            "[lyrics]\nai-key-env = false\n",
            "[auth]\nauto-reauth = \"no\"\n",
            "[auth]\ncookie-browser = 5\n",
        ] {
            let c = parse(src);
            let d = Config::default();
            assert_eq!(c.lyrics.offset, d.lyrics.offset, "{src:?}");
            assert_eq!(c.lyrics.translate_to, d.lyrics.translate_to, "{src:?}");
            assert_eq!(c.lyrics.ai_translation, d.lyrics.ai_translation, "{src:?}");
            assert_eq!(c.lyrics.ai_model, d.lyrics.ai_model, "{src:?}");
            assert_eq!(c.lyrics.ai_key_env, d.lyrics.ai_key_env, "{src:?}");
            assert_eq!(c.auth.auto_reauth, d.auth.auto_reauth, "{src:?}");
            assert_eq!(c.auth.cookie_browser, d.auth.cookie_browser, "{src:?}");
        }
    }

    #[test]
    fn a_misspelled_setting_is_named_rather_than_ignored_in_silence() {
        // `translate_to` parses fine and does nothing whatsoever, which is the
        // worst way for a setting to fail.
        let table = "[lyrics]\ntranslate_to = \"zh\"\nofset = 1.0\n"
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(unknown(&table), ["lyrics.ofset", "lyrics.translate_to"]);

        let table = "[lyric]\ntranslate-to = \"zh\"\n"
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(unknown(&table), ["lyric"]);

        // A setting written outside any section is a section as far as TOML is
        // concerned, and is just as invisible.
        let table = "offset = 1.0\n".parse::<toml::Table>().unwrap();
        assert_eq!(unknown(&table), ["offset"]);
    }

    #[test]
    fn the_template_and_an_empty_file_raise_nothing() {
        for src in [TEMPLATE, "", "   \n\t\n", "[lyrics]\n[auth]\n"] {
            let table = src.parse::<toml::Table>().expect("valid toml");
            assert!(unknown(&table).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn a_file_that_is_not_toml_at_all_is_left_to_the_caller() {
        // Nothing per-key can be recovered from these — `load` falls back to
        // defaults whole and says so with the line number.
        for src in [
            "this is not toml",
            "[lyrics]\noffset = 1.0\noffset = 2.0\n",
            "[lyrics]\n[lyrics]\n",
            "[lyrics]\noffset = 1e400\n",
        ] {
            assert!(src.parse::<toml::Table>().is_err(), "{src:?}");
        }
    }

    #[test]
    fn a_bom_or_windows_line_endings_are_still_a_config_file() {
        assert_eq!(parse("\u{feff}[lyrics]\noffset = 1.0\n").lyrics.offset, 1.0);
        assert_eq!(parse("[lyrics]\r\noffset = 1.0\r\n").lyrics.offset, 1.0);
    }

    #[test]
    fn whitespace_round_a_setting_is_not_the_setting() {
        unsafe { std::env::set_var("YTM_SPACED_KEY", "sk-test") };
        let c = parse(
            "[lyrics]\nai-translation = true\nai-model = \"  claude-haiku-4-5  \"\n\
             ai-key-env = \"  YTM_SPACED_KEY  \"\ntranslate-to = \"  ZH  \"\n",
        );
        assert_eq!(c.lyrics.ai_model, "claude-haiku-4-5");
        assert_eq!(c.lyrics.ai_key_env, "YTM_SPACED_KEY");
        assert_eq!(c.lyrics.translate_to, "zh");
        assert!(c.lyrics.ai_available());
    }
}
