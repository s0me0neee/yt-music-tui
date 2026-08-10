//! Core engine for yt-music-tui: session/auth, library fetching, playback,
//! queue orchestration, and persistence — extracted from the ratatui TUI so
//! it can be driven by something else too (e.g. a headless daemon).

pub mod config;
pub mod error;
pub mod library;
pub mod lyrics;
pub mod persistence;
pub mod playback;
pub mod player;
pub mod session;
pub mod shutdown;

pub use config::Config;
pub use error::{Error, Result};
pub use library::{Album, Artist, Library, Playlist, PlaylistEntry, Track};
pub use lyrics::{LyricsKind, LyricsMsg, LyricsQuery, LyricsService, TrackLyrics};
pub use playback::AudioState;
pub use player::{AppendOutcome, PlayMode, Player, RemoveOutcome, TrackRef};
pub use session::{Browser, Reauth, Session};

/// Re-exported so consumers don't need `ytmusicapi` as a direct dependency.
pub use ytmusicapi::YTMusicClient;
