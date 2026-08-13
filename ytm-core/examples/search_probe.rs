//! What YouTube Music search actually returns, and what type each result is.
//!
//! `ytmusicapi 0.4.2` has no search of its own, so this goes through the raw
//! `send_request` escape hatch and walks the response for every
//! `musicResponsiveListItemRenderer` — the same node the crate's playlist
//! parser reads — reporting the `musicVideoType` beside each hit.
//!
//! That field is the whole question: YouTube Music labels an *art track*
//! (`MUSIC_VIDEO_TYPE_ATV`, the catalogue audio, whose entire visual content is
//! the album cover) and an *official music video* (`OMV`, a real video with
//! intros and outros) both as "Song", while their durations — and so their
//! lyrics matching — behave completely differently.
//!
//! ```text
//! cargo run -p ytm-core --example search_probe -- "bohemian rhapsody"
//! ```

use serde_json::{Value, json};
use ytm_core::Session;

/// Every value under `key`, however deep.
fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == key {
                    out.push(val);
                }
                find_all(val, key, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| find_all(i, key, out)),
        _ => {}
    }
}

/// The first string under `key`, however deep — for the leaf fields whose exact
/// path is not worth hard-coding in a probe.
fn first_str(v: &Value, key: &str) -> Option<String> {
    let mut hits = Vec::new();
    find_all(v, key, &mut hits);
    hits.first()
        .and_then(|h| h.as_str())
        .map(std::string::ToString::to_string)
}

/// The visible text of a result row: title, then artist / album / duration.
fn row_text(item: &Value) -> Vec<String> {
    let mut columns = Vec::new();
    find_all(item, "flexColumns", &mut columns);
    let mut out = Vec::new();
    for column in columns {
        let mut runs = Vec::new();
        find_all(column, "runs", &mut runs);
        for run in runs {
            if let Some(items) = run.as_array() {
                let text: String = items
                    .iter()
                    .filter_map(|r| r.get("text").and_then(Value::as_str))
                    .collect();
                let text = text.trim().to_string();
                if !text.is_empty() && text != " • " && !out.contains(&text) {
                    out.push(text);
                }
            }
        }
    }
    out
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let query = std::env::args().nth(1).unwrap_or_else(|| "echo".to_string());
    let session = Session::new()?;
    let yt = session.build_client()?;

    // `params` is the opaque filter blob YouTube Music's own UI sends. Passed
    // as the second argument so the probe can compare filtered against not.
    let filter = std::env::args().nth(2);
    let body = match &filter {
        Some(p) => json!({ "query": query, "params": p }),
        None => json!({ "query": query }),
    };
    println!("filter: {filter:?}");
    let response = yt.send_request("search", body).await?;

    println!("query: {query:?}\n");

    // Which container renderers this response actually uses — YouTube renames
    // them, so a probe should ask rather than assume.
    let mut kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    fn keys(v: &Value, out: &mut std::collections::BTreeMap<String, usize>) {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    if k.ends_with("ShelfRenderer") || k.ends_with("ItemRenderer") {
                        *out.entry(k.clone()).or_default() += 1;
                    }
                    keys(val, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| keys(i, out)),
            _ => {}
        }
    }
    keys(&response, &mut kinds);
    println!("renderers in this response:");
    for (k, n) in &kinds {
        println!("   {k:<38} {n}");
    }
    println!();

    // Every result row in the response, wherever it is nested. Grouping by
    // shelf is a UI concern; what matters here is the type of each hit.
    let shelves: Vec<&Value> = vec![&response];
    println!("all result rows\n");

    let mut totals: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for shelf in shelves {
        let title = first_str(shelf, "title").unwrap_or_else(|| "(untitled)".to_string());
        let mut items = Vec::new();
        find_all(shelf, "musicResponsiveListItemRenderer", &mut items);
        println!("── {title}  ({} items)", items.len());

        for item in items {
            let video_type = first_str(item, "musicVideoType").unwrap_or_else(|| "—".to_string());
            let video_id = first_str(item, "videoId").unwrap_or_else(|| "—".to_string());
            let short = video_type.trim_start_matches("MUSIC_VIDEO_TYPE_").to_string();
            *totals.entry(short.clone()).or_default() += 1;
            let text = row_text(item);
            println!("   [{short:<21}] {video_id:<12} {}", text.join("  ·  "));
        }
        println!();
    }

    println!("── totals by type");
    let mut counts: Vec<_> = totals.into_iter().collect();
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (kind, n) in counts {
        println!("   {kind:<22} {n}");
    }
    Ok(())
}
