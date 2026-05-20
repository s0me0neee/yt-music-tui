use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::BufRead;

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
