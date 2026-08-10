//! Unified error type for the crate's public API.

use crate::session::Browser;

/// How to install yt-dlp, phrased for the platform actually running.
#[cfg(target_os = "windows")]
const YTDLP_INSTALL: &str = "winget install yt-dlp  or  pip install yt-dlp";
#[cfg(target_os = "macos")]
const YTDLP_INSTALL: &str = "brew install yt-dlp  or  pip install yt-dlp";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const YTDLP_INSTALL: &str = "pip install yt-dlp";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The YouTube Music session has expired and re-authentication is required.
    #[error("YouTube Music session expired — re-authenticate")]
    SessionExpired,

    #[error(transparent)]
    Ytmusicapi(#[from] ytmusicapi::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Prompt(#[from] inquire::InquireError),

    /// `yt-dlp` isn't installed or failed to spawn.
    #[error("yt-dlp not found — install it with: {}", YTDLP_INSTALL)]
    YtDlpNotInstalled,

    /// `yt-dlp` ran but wrote no cookie file. Usually the user isn't signed in to
    /// YouTube Music in the given browser — but it is also what a cookie store
    /// that couldn't be read at all looks like from out here, so `diagnosis`
    /// carries whatever yt-dlp's own stderr said about which it was (empty when
    /// it said nothing recognisable). See `session::diagnose_ytdlp_stderr`.
    #[error(
        "yt-dlp wrote no cookie file — make sure you're signed in to YouTube Music in \
         {browser}{diagnosis}"
    )]
    BrowserNotSignedIn { browser: Browser, diagnosis: String },

    /// A cookie file was written, but none of its cookies were for `*.youtube.com`.
    #[error("no youtube.com cookies found in {browser} — are you signed in to YouTube Music?")]
    NoCookiesFound { browser: Browser },

    /// The pasted cURL command had no `-H`/`-b` flags at all.
    #[error("no headers found — make sure the input is a 'Copy as cURL (bash)' export")]
    CurlEmpty,

    /// The pasted cURL command was missing one or more required headers.
    #[error(
        "required headers missing: {0:?} — copy a request from music.youtube.com while logged in"
    )]
    CurlMissingHeaders(Vec<&'static str>),

    #[error("libmpv init failed: {0}")]
    Mpv(String),

    #[error("lyrics lookup failed: {0}")]
    Lyrics(#[from] lrclib::LrcError),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
