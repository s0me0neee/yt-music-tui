use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::BufRead;
use std::process::Stdio;

/// Refresh the `cookie` field in browser.json by extracting live cookies
/// from Chrome via yt-dlp. Called at every startup so the session never
/// expires as long as the user is signed in to YouTube Music in Chrome.
pub fn refresh_cookies(browser_json_path: &str) -> Result<()> {
    let tmp = format!("/tmp/yt-tui-cookies-{}.txt", std::process::id());

    // yt-dlp exits non-zero for the unsupported URL but still writes the
    // cookie file — so we ignore the exit code and check for file content.
    std::process::Command::new("yt-dlp")
        .args([
            "--cookies-from-browser", "chrome",
            "--cookies", &tmp,
            "--skip-download",
            "https://music.youtube.com/",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok(); // ignore spawn / exit errors; we check the output file below

    let cookie_content = match std::fs::read_to_string(&tmp) {
        Ok(c) => { std::fs::remove_file(&tmp).ok(); c }
        Err(e) => bail!("yt-dlp did not write cookie file: {e}"),
    };

    // Netscape format: domain \t subdomain_flag \t path \t secure \t expiry \t name \t value
    let cookie_header: String = cookie_content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(7, '\t');
            let domain = parts.next()?;
            let _subdom = parts.next()?;
            let _path   = parts.next()?;
            let _secure = parts.next()?;
            let _expiry = parts.next()?;
            let name    = parts.next()?;
            let value   = parts.next()?;
            // keep all youtube.com cookies (covers music.youtube.com too)
            if domain.ends_with("youtube.com") {
                Some(format!("{name}={value}"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("; ");

    if cookie_header.is_empty() {
        bail!("no youtube.com cookies found — is Chrome logged in to YouTube Music?");
    }

    let json_str = std::fs::read_to_string(browser_json_path)?;
    let mut json: serde_json::Value = serde_json::from_str(&json_str)?;
    json["cookie"] = serde_json::Value::String(cookie_header);
    std::fs::write(browser_json_path, serde_json::to_string_pretty(&json)?)?;
    log::info!("[setup] refreshed cookies from Chrome");
    Ok(())
}

pub fn run_setup(browser_json_path: &str) -> Result<()> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if is_tty {
        eprintln!("No browser.json found.");
        eprintln!("Open music.youtube.com, open DevTools (F12) → Network,");
        eprintln!("click any request → right-click → Copy as cURL (bash),");
        eprintln!("paste below and press Enter:\n");
    }

    // Read until a line with no trailing backslash (end of the curl command).
    // Blank lines before content are skipped; a blank line after content also stops.
    // This means the user just pastes and presses Enter — no Ctrl+D required.
    let mut lines: Vec<String> = Vec::new();
    for raw in std::io::stdin().lock().lines() {
        let line = raw?;
        let cont = line.trim_end().ends_with('\\');
        let empty = line.trim().is_empty();
        if empty {
            if !lines.is_empty() { break; } // blank line after content = done
            continue;                        // skip leading blank lines
        }
        lines.push(line);
        if !cont { break; } // non-continuation line = end of curl command
    }
    let text = lines.join("\n");

    let headers = parse_curl(&text)?;
    let json = serde_json::to_string_pretty(&headers)?;
    std::fs::write(browser_json_path, &json)?;
    Ok(())
}

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
            "required headers missing: {}\nCopy a request from music.youtube.com while logged in.",
            missing.join(", ")
        );
    }

    Ok(headers)
}

/// Finds all `flag 'value'` occurrences and returns the quoted values.
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
