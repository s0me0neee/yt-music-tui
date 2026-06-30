use anyhow::{bail, Result};
use inquire::{Select, Text};
use std::collections::HashMap;
use std::process::Stdio;

fn browser_file() -> std::path::PathBuf { crate::config::browser_file_path() }

// ── RAII helpers ──────────────────────────────────────────────────────────────

struct FileGuard(String);
impl Drop for FileGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ── public API ────────────────────────────────────────────────────────────────

pub fn run_setup(browser_json_path: &str) -> Result<()> {
    let options = vec![
        "Auto   — extract cookies from browser via yt-dlp  (recommended)",
        "Manual — paste a cURL command from browser DevTools",
    ];

    let choice = Select::new("Authentication method:", options)
        .with_help_message("yt-dlp reads cookies directly from your browser profile")
        .prompt()?;

    std::fs::remove_file(browser_file()).ok();

    match choice {
        c if c.starts_with("Auto") => setup_via_ytdlp(browser_json_path),
        _                          => setup_via_headers(browser_json_path),
    }
}

/// Refresh the `cookie` field in browser.json via yt-dlp.
/// No-op (returns Ok) when setup was done with the manual cURL method.
pub fn refresh_cookies(browser_json_path: &str) -> Result<()> {
    let browser = if let Ok(b) = std::fs::read_to_string(browser_file()) { b.trim().to_string() } else {
        log::info!("[setup] no browser file — skipping cookie refresh (manual setup)");
        return Ok(());
    };

    log::info!("[setup] refreshing cookies from {browser} via yt-dlp");
    let cookie_header = extract_cookies_via_ytdlp(&browser)?;

    let json_str = std::fs::read_to_string(browser_json_path)?;
    let mut json: serde_json::Value = serde_json::from_str(&json_str)?;
    json["cookie"] = serde_json::Value::String(cookie_header);
    std::fs::write(browser_json_path, serde_json::to_string_pretty(&json)?)?;
    log::info!("[setup] cookies refreshed");
    Ok(())
}

// ── setup methods ─────────────────────────────────────────────────────────────

fn setup_via_ytdlp(browser_json_path: &str) -> Result<()> {
    let browsers = vec![
        "Chrome", "Firefox", "Edge", "Brave",
        "Opera",  "Chromium", "Vivaldi", "Safari",
    ];

    let chosen = Select::new("Browser you are signed in to YouTube Music with:", browsers)
        .prompt()?;

    let browser = chosen.to_lowercase();

    let cookie_header = extract_cookies_via_ytdlp(&browser)?;
    let headers = build_default_headers(cookie_header);
    std::fs::write(browser_json_path, serde_json::to_string_pretty(&headers)?)?;
    std::fs::write(browser_file(), &browser)?;
    Ok(())
}

fn setup_via_headers(browser_json_path: &str) -> Result<()> {
    let curl = Text::new("Paste cURL command:")
        .with_help_message(
            "music.youtube.com → DevTools (F12) → Network → any request \
             → right-click → Copy as cURL (bash)",
        )
        .prompt()?;

    let headers = parse_curl(curl.trim())?;
    std::fs::write(browser_json_path, serde_json::to_string_pretty(&headers)?)?;
    Ok(())
}

// ── yt-dlp cookie extraction ──────────────────────────────────────────────────

fn extract_cookies_via_ytdlp(browser: &str) -> Result<String> {
    let tmp = std::env::temp_dir()
        .join(format!("yt-tui-cookies-{}.txt", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _guard = FileGuard(tmp.clone());

    let mut child = std::process::Command::new("yt-dlp")
        .args([
            "--cookies-from-browser", browser,
            "--cookies", &tmp,
            "--skip-download",
            "https://music.youtube.com/",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "yt-dlp failed to start: {e}\n\
                 Install it with:  pip install yt-dlp  or  brew install yt-dlp"
            )
        })?;

    // Always wait() — never leave a zombie.
    // yt-dlp exits non-zero for this URL but still writes the cookie file.
    let _ = child.wait();

    let content = std::fs::read_to_string(&tmp).map_err(|_| {
        anyhow::anyhow!(
            "yt-dlp did not write a cookie file — \
             make sure you are signed in to YouTube Music in {browser}"
        )
    })?;

    let header = parse_netscape_cookies(&content);
    if header.is_empty() {
        bail!(
            "no youtube.com cookies found — \
             are you signed in to YouTube Music in {browser}?"
        );
    }
    Ok(header)
}

fn parse_netscape_cookies(content: &str) -> String {
    content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(7, '\t');
            let domain  = parts.next()?;
            let _subdom = parts.next()?;
            let _path   = parts.next()?;
            let _secure = parts.next()?;
            let _expiry = parts.next()?;
            let name    = parts.next()?;
            let value   = parts.next()?;
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
            headers.insert(value[..colon].to_lowercase(), value[colon + 2..].to_string());
        }
    }

    if let Some(cookie) = extract_single_quoted(text, "-b").into_iter().next() {
        headers.insert("cookie".to_string(), cookie);
    }

    if headers.is_empty() {
        bail!("no headers found — make sure the input is a 'Copy as cURL (bash)' export");
    }

    let missing: Vec<&str> = ["cookie", "x-goog-authuser"]
        .iter()
        .copied()
        .filter(|&k| !headers.contains_key(k))
        .collect();
    if !missing.is_empty() {
        bail!(
            "required headers missing: {}\n\
             Copy a request from music.youtube.com while logged in.",
            missing.join(", ")
        );
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
