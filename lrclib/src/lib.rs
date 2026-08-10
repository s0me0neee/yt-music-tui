//! A client for the [lrclib.net](https://lrclib.net) lyrics API, plus a parser
//! for the LRC synced-lyrics format the API returns.
//!
//! [`api`] is the transport — it mirrors the HTTP endpoints and returns records
//! verbatim. [`lrc`] turns a record's `synced_lyrics` string into timestamped
//! [`LyricLine`]s and answers "which line is playing at time *t*".

pub mod api;
pub mod lrc;

pub use api::{LrcError, LrcLib, Lyrics};
pub use lrc::{LyricLine, active_index, next_boundary, parse_lrc};
