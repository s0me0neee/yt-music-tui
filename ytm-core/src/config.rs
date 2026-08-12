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
# Translate with Claude instead of the free endpoint. The whole song goes in
# one request, so a sentence spanning several lyric lines is read as a sentence
# — the free endpoint splits on newlines and cannot. Costs roughly a cent per
# song, charged to your own API key. false (the default) is the free translator,
# which costs nothing and sends nothing to any API.
#ai-translation = false
# Which model, when the above is on.
#ai-model = \"claude-haiku-4-5\"
# Environment variable holding the Anthropic API key. The key itself is never
# read from this file, so config.toml stays safe to copy around.
#ai-key-env = \"ANTHROPIC_API_KEY\"

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
    enum Seconds {
        Float(f64),
        Int(i64),
    }
    Ok(match Seconds::deserialize(de)? {
        Seconds::Float(f) => f,
        Seconds::Int(i) => i as f64,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub lyrics: Lyrics,
    pub auth: Auth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Lyrics {
    /// Seconds to shift every lyric line by. Negative is early, positive late.
    #[serde(deserialize_with = "seconds")]
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
    /// Claude model to translate with. Only consulted when
    /// [`Self::ai_translation`] is on.
    pub ai_model: String,
    /// Environment variable holding the Anthropic API key. Named here rather
    /// than holding the key itself, so `config.toml` stays safe to share and
    /// the secret lives wherever the user already keeps secrets.
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

    /// The translator this configuration selects.
    ///
    /// AI only when a model is named *and* its key was found — the two are
    /// checked together so a partial setup silently uses the free endpoint
    /// instead of failing on every song.
    #[must_use]
    pub fn backend(&self) -> crate::translate::Backend {
        crate::translate::Backend {
            to: self.translate_to.clone(),
            ai: self
                .ai_key()
                .filter(|_| self.ai_translation && !self.ai_model.is_empty())
                .map(|api_key| crate::translate::Ai {
                    model: self.ai_model.clone(),
                    api_key,
                }),
        }
    }
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

        match toml::from_str::<Self>(&raw) {
            Ok(config) => config.validated(),
            Err(e) => {
                log::warn!(
                    "config: {} is not valid TOML ({e}) — using defaults",
                    path.display()
                );
                Self::default()
            }
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
        if self.lyrics.ai_translation {
            if self.lyrics.ai_model.is_empty() {
                log::warn!("config: lyrics.ai-translation is on but ai-model is empty — off");
                self.lyrics.ai_translation = false;
            } else if self.lyrics.ai_key().is_some() {
                log::info!(
                    "config: lyrics.ai-translation on, {:?} (key from ${})",
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

    match std::fs::write(&path, doc.to_string()) {
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

    fn parse(src: &str) -> Config {
        toml::from_str::<Config>(src)
            .expect("valid toml")
            .validated()
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
        assert!(lyrics.backend().ai.is_none());
    }

    #[test]
    fn an_empty_model_turns_the_flag_back_off() {
        let lyrics = parse("[lyrics]\nai-translation = true\nai-model = \"\"\n").lyrics;
        assert!(!lyrics.ai_translation);
        assert!(lyrics.backend().ai.is_none());
    }

    #[test]
    fn a_key_variable_that_names_nothing_yields_no_key() {
        let lyrics =
            parse("[lyrics]\nai-translation = true\nai-key-env = \"YTM_NO_SUCH_VAR_XYZ\"\n").lyrics;
        assert!(lyrics.ai_key().is_none());
        assert!(lyrics.backend().ai.is_none());
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
}
