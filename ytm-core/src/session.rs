//! Authentication and config-directory management.
//!
//! Auth is cookie-based, ytmusicapi "browser auth" style — a `browser.json`
//! header/cookie file, no OAuth. First run (or an expired session) drives an
//! interactive setup: either `yt-dlp --cookies-from-browser` extraction, or
//! pasting a "Copy as cURL" export from browser DevTools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use inquire::{Select, Text};
use ytmusicapi::{BrowserAuth, YTMusicClient};

use crate::error::{Error, Result};

// ── well-known paths ────────────────────────────────────────────────────────

/// App config directory.
/// - macOS : `~/.config/yt-music-tui/`  (XDG-style, not ~/Library)
/// - Other : `{dirs::config_dir()}/yt-music-tui/`
pub fn app_config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config");

    #[cfg(not(target_os = "macos"))]
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));

    base.join("yt-music-tui")
}

/// Creates the config directory if it doesn't exist, then returns its path.
pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = app_config_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn browser_json_path() -> PathBuf {
    app_config_dir().join("browser.json")
}
pub fn browser_file_path() -> PathBuf {
    app_config_dir().join(".yt-tui-browser")
}
pub fn queue_path() -> PathBuf {
    app_config_dir().join("queue.json")
}
pub fn config_toml_path() -> PathBuf {
    app_config_dir().join("config.toml")
}

/// Creates `config.toml` with a comment header if it doesn't exist yet.
pub fn ensure_config_toml() -> Result<()> {
    let path = config_toml_path();
    if !path.exists() {
        std::fs::write(&path, "# yt-music-tui configuration\n")?;
    }
    Ok(())
}

// ── browser ──────────────────────────────────────────────────────────────────

/// A browser yt-dlp can extract cookies from via `--cookies-from-browser`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Firefox,
    Edge,
    Brave,
    Opera,
    Chromium,
    Vivaldi,
    Safari,
}

impl Browser {
    /// Every supported browser, in the order offered by the interactive setup prompt.
    pub const ALL: [Browser; 8] = [
        Self::Chrome,
        Self::Firefox,
        Self::Edge,
        Self::Brave,
        Self::Opera,
        Self::Chromium,
        Self::Vivaldi,
        Self::Safari,
    ];

    /// Display name, e.g. `"Chrome"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Firefox => "Firefox",
            Self::Edge => "Edge",
            Self::Brave => "Brave",
            Self::Opera => "Opera",
            Self::Chromium => "Chromium",
            Self::Vivaldi => "Vivaldi",
            Self::Safari => "Safari",
        }
    }

    /// Lowercase form used both as the `--cookies-from-browser` argument and
    /// as the on-disk format of the `.yt-tui-browser` marker file.
    fn as_ytdlp_arg(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Firefox => "firefox",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Opera => "opera",
            Self::Chromium => "chromium",
            Self::Vivaldi => "vivaldi",
            Self::Safari => "safari",
        }
    }

    /// Parses the lowercase form written by [`Browser::as_ytdlp_arg`] (case-insensitive).
    fn parse(s: &str) -> Option<Browser> {
        Self::ALL
            .into_iter()
            .find(|b| b.as_ytdlp_arg().eq_ignore_ascii_case(s))
    }
}

impl std::fmt::Display for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── session ──────────────────────────────────────────────────────────────────

/// A YouTube Music session backed by `browser.json`.
#[derive(Clone)]
pub struct Session {
    browser_json: PathBuf,
}

impl Session {
    /// Ensures the config directory and `config.toml` stub exist, then returns
    /// a handle to the (possibly not-yet-created) session.
    pub fn new() -> Result<Self> {
        ensure_config_dir()?;
        ensure_config_toml()?;
        Ok(Self {
            browser_json: browser_json_path(),
        })
    }

    pub fn browser_json_path(&self) -> &Path {
        &self.browser_json
    }

    pub fn is_set_up(&self) -> bool {
        self.browser_json.exists()
    }

    /// Builds an authenticated client from the cached `browser.json`.
    pub fn build_client(&self) -> Result<YTMusicClient> {
        let auth = BrowserAuth::from_file(&self.browser_json)?;
        Ok(YTMusicClient::builder().with_browser_auth(auth).build()?)
    }

    /// Runs the interactive (terminal) setup flow: choose Auto (yt-dlp
    /// `--cookies-from-browser`) or Manual (paste a cURL command), then writes
    /// `browser.json`. Blocks on stdin/stdout — call from a plain terminal
    /// context, not from inside a raw-mode TUI screen.
    ///
    /// For a non-interactive caller (no TTY — e.g. a daemon), use
    /// [`Session::setup_with_browser`] or [`Session::setup_with_curl`] instead.
    pub fn run_setup(&self) -> Result<()> {
        let options = vec![
            "Auto   — extract cookies from browser via yt-dlp  (recommended)",
            "Manual — paste a cURL command from browser DevTools",
        ];

        let choice = Select::new("Authentication method:", options)
            .with_help_message("yt-dlp reads cookies directly from your browser profile")
            .prompt()?;

        std::fs::remove_file(browser_file_path()).ok();

        if choice.starts_with("Auto") {
            self.setup_via_ytdlp()
        } else {
            self.setup_via_headers()
        }
    }

    /// Headless equivalent of the "Auto" setup path: extracts cookies from
    /// `browser` via yt-dlp and writes `browser.json` + the browser marker
    /// file. No prompts — safe to call without a TTY.
    pub fn setup_with_browser(&self, browser: Browser) -> Result<()> {
        let cookie_header = extract_cookies_via_ytdlp(browser)?;
        let headers = build_default_headers(cookie_header);
        std::fs::write(&self.browser_json, serde_json::to_string_pretty(&headers)?)?;
        std::fs::write(browser_file_path(), browser.as_ytdlp_arg())?;
        Ok(())
    }

    /// Headless equivalent of the "Manual" setup path: parses a pasted cURL
    /// command and writes `browser.json`. No prompts — safe to call without a TTY.
    pub fn setup_with_curl(&self, curl: &str) -> Result<()> {
        let headers = parse_curl(curl.trim())?;
        std::fs::write(&self.browser_json, serde_json::to_string_pretty(&headers)?)?;
        Ok(())
    }

    /// Drops the current session (`browser.json` + the browser marker file)
    /// without re-authenticating. Pair with [`Session::setup_with_browser`] or
    /// [`Session::setup_with_curl`] for a headless re-auth.
    pub fn clear(&self) -> Result<()> {
        std::fs::remove_file(&self.browser_json).ok();
        std::fs::remove_file(browser_file_path()).ok();
        Ok(())
    }

    /// Refreshes the `cookie` field in `browser.json` via yt-dlp.
    /// No-op when setup was done with the manual cURL method, and skipped
    /// while cookies are still fresh (checked via `browser.json`'s mtime).
    pub fn refresh_cookies(&self) -> Result<()> {
        let raw = match std::fs::read_to_string(browser_file_path()) {
            Ok(b) => b,
            Err(_) => {
                log::info!("[session] no browser file — skipping cookie refresh (manual setup)");
                return Ok(());
            }
        };
        let Some(browser) = Browser::parse(raw.trim()) else {
            log::warn!("[session] unrecognized browser {raw:?} in marker file — skipping refresh");
            return Ok(());
        };

        const REFRESH_AFTER: Duration = Duration::from_secs(6 * 3600);
        if let Ok(meta) = std::fs::metadata(&self.browser_json)
            && let Ok(modified) = meta.modified()
            && let Ok(age) = modified.elapsed()
            && age < REFRESH_AFTER
        {
            log::info!(
                "[session] cookies {}m old — skipping refresh",
                age.as_secs() / 60
            );
            return Ok(());
        }

        log::info!("[session] refreshing cookies from {browser} via yt-dlp");
        let cookie_header = extract_cookies_via_ytdlp(browser)?;

        let json_str = std::fs::read_to_string(&self.browser_json)?;
        let mut json: serde_json::Value = serde_json::from_str(&json_str)?;
        json["cookie"] = serde_json::Value::String(cookie_header);
        std::fs::write(&self.browser_json, serde_json::to_string_pretty(&json)?)?;
        log::info!("[session] cookies refreshed");
        Ok(())
    }

    /// Drops the current session (`browser.json` + the browser marker file)
    /// and re-runs the interactive setup flow.
    pub fn reauth(&self) -> Result<()> {
        self.clear()?;
        self.run_setup()?;
        eprintln!("\nSetup complete. Restart the app to continue.\n");
        Ok(())
    }

    // ── interactive setup methods ───────────────────────────────────────────

    fn setup_via_ytdlp(&self) -> Result<()> {
        let browser = Select::new(
            "Browser you are signed in to YouTube Music with:",
            Browser::ALL.to_vec(),
        )
        .prompt()?;
        self.setup_with_browser(browser)
    }

    fn setup_via_headers(&self) -> Result<()> {
        let curl = Text::new("Paste cURL command:")
            .with_help_message(
                "music.youtube.com → DevTools (F12) → Network → any request \
                 → right-click → Copy as cURL (bash)",
            )
            .prompt()?;
        self.setup_with_curl(&curl)
    }
}

// ── RAII helpers ──────────────────────────────────────────────────────────────

struct FileGuard(String);
impl Drop for FileGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ── yt-dlp cookie extraction ──────────────────────────────────────────────────

#[hotpath::measure]
fn extract_cookies_via_ytdlp(browser: Browser) -> Result<String> {
    let tmp = std::env::temp_dir()
        .join(format!("yt-tui-cookies-{}.txt", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _guard = FileGuard(tmp.clone());

    let mut child = std::process::Command::new("yt-dlp")
        .args([
            "--cookies-from-browser",
            browser.as_ytdlp_arg(),
            "--cookies",
            &tmp,
            "--skip-download",
            "https://music.youtube.com/",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::YtDlpNotInstalled)?;

    // Always wait() — never leave a zombie.
    // yt-dlp exits non-zero for this URL but still writes the cookie file.
    let _ = child.wait();

    let content =
        std::fs::read_to_string(&tmp).map_err(|_| Error::BrowserNotSignedIn { browser })?;

    let header = parse_netscape_cookies(&content);
    if header.is_empty() {
        return Err(Error::NoCookiesFound { browser });
    }
    Ok(header)
}

fn parse_netscape_cookies(content: &str) -> String {
    content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(7, '\t');
            let domain = parts.next()?;
            let _subdom = parts.next()?;
            let _path = parts.next()?;
            let _secure = parts.next()?;
            let _expiry = parts.next()?;
            let name = parts.next()?;
            let value = parts.next()?;
            if domain.ends_with("youtube.com") {
                Some(format!("{name}={value}"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn build_default_headers(cookie: String) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("cookie".into(), cookie);
    h.insert("x-goog-authuser".into(), "0".into());
    h.insert("x-origin".into(), "https://music.youtube.com".into());
    h.insert(
        "user-agent".into(),
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
            .into(),
    );
    h.insert("accept".into(), "*/*".into());
    h.insert("accept-language".into(), "en-US,en;q=0.9".into());
    h.insert("content-type".into(), "application/json".into());
    h
}

// ── cURL header parsing ───────────────────────────────────────────────────────

fn parse_curl(text: &str) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();

    for value in extract_single_quoted(text, "-H") {
        if let Some(colon) = value.find(": ") {
            headers.insert(
                value[..colon].to_lowercase(),
                value[colon + 2..].to_string(),
            );
        }
    }

    if let Some(cookie) = extract_single_quoted(text, "-b").into_iter().next() {
        headers.insert("cookie".to_string(), cookie);
    }

    if headers.is_empty() {
        return Err(Error::CurlEmpty);
    }

    let missing: Vec<&'static str> = ["cookie", "x-goog-authuser"]
        .into_iter()
        .filter(|&k| !headers.contains_key(k))
        .collect();
    if !missing.is_empty() {
        return Err(Error::CurlMissingHeaders(missing));
    }

    Ok(headers)
}

fn extract_single_quoted(text: &str, flag: &str) -> Vec<String> {
    let needle = format!("{flag} '");
    let mut results = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        rest = &rest[i + needle.len()..];
        if let Some(j) = rest.find('\'') {
            results.push(rest[..j].to_string());
            rest = &rest[j + 1..];
        } else {
            break;
        }
    }
    results
}
