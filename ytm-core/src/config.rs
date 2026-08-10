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

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub lyrics: Lyrics,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Lyrics {
    /// Seconds to shift every lyric line by. Negative is early, positive late.
    #[serde(deserialize_with = "seconds")]
    pub offset: f64,
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
        self
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
        let at = |offset| Lyrics { offset }.lyric_time(10.0);
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

    #[test]
    fn unknown_keys_do_not_discard_the_rest() {
        // Forward compatibility: a setting from a newer version, or a stray
        // key, must not cost the user the settings that are valid.
        let c = parse("[lyrics]\noffset = -0.4\nsomething_else = true\n");
        assert_eq!(c.lyrics.offset, -0.4);
    }

    #[test]
    fn the_label_only_appears_when_there_is_a_shift() {
        assert_eq!(Lyrics { offset: 0.0 }.offset_label(), None);
        assert_eq!(
            Lyrics { offset: -0.3 }.offset_label().as_deref(),
            Some("-0.3s")
        );
        assert_eq!(
            Lyrics { offset: 1.25 }.offset_label().as_deref(),
            Some("+1.2s")
        );
    }
}
