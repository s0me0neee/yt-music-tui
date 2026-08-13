//! Does the search parser label a row the same way YouTube Music does?
//!
//! Two things are being checked, and they are different questions.
//!
//! **Is the parse unambiguous?** A result row carries several navigation
//! endpoints — the row itself, and every entry in its overflow menu ("play
//! next", "add to queue", "go to album"). Reading `musicVideoType` by walking
//! the row and taking the first hit is only correct if every hit within a row
//! agrees. This counts the distinct values per row and reports any row where
//! they don't, which would mean the parser can silently read a neighbour's
//! label instead of the row's own.
//!
//! **Is the filter's promise real?** The claim "the songs filter returns only
//! art tracks" was made from three queries. This runs it across a spread —
//! Western pop with famous music videos, Japanese and Vocaloid releases,
//! covers, karaoke, live recordings — and reports the type distribution per
//! query, so the claim either holds or is visibly wrong.
//!
//! **Does the label match the one YouTube Music prints?** An unfiltered search
//! shows both — the row's second column opens with the word the UI displays
//! ("Song", "Video", "Episode"), and the row's endpoint carries the type — so
//! the two can be cross-tabulated. Measured: `Song`/`Single`/`EP` was ATV every
//! time, `Video` was always OMV or UGC, never ATV. The type is the better
//! source of the two even so, because the printed word is *localized* and is
//! omitted entirely on top-result cards, where the artist is printed instead.
//!
//! ```text
//! cargo run -p ytm-core --example search_verify
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use ytm_core::Session;

const SONGS_FILTER: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D";
const VIDEOS_FILTER: &str = "EgWKAQIQAWoKEAkQChAFEAMQBA%3D%3D";

/// Queries chosen to be hostile to the claim: each has an official music
/// video, a live version, and covers competing for the same title.
const QUERIES: &[&str] = &[
    "ariiol - typing",
    "bohemian rhapsody",
    "taylor swift blank space",
    "billie eilish bad guy",
    "yorushika 花に亡霊",
    "初音ミク メルト",
    "zutomayo 秒針を噛む",
    "never gonna give you up",
    "smells like teen spirit",
    "adele hello",
];

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

/// The word YouTube Music prints at the start of a row's second column, or
/// `None` where it prints something else (a top-result card prints the artist).
fn displayed_category(item: &Value) -> Option<String> {
    // `flexColumns` is an *array* of columns; iterate its elements, not it.
    let mut arrays = Vec::new();
    find_all(item, "flexColumns", &mut arrays);
    let columns: Vec<&Value> = arrays
        .iter()
        .filter_map(|a| a.as_array())
        .flatten()
        .collect();
    let texts: Vec<String> = columns
        .iter()
        .filter_map(|c| {
            let mut runs = Vec::new();
            find_all(c, "runs", &mut runs);
            runs.first().and_then(|r| r.as_array()).map(|items| {
                items
                    .iter()
                    .filter_map(|r| r.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
        })
        .collect();
    Some(texts.get(1)?.split('•').next()?.trim().to_string())
}

/// Every distinct string under `key` within one row.
fn all_strs(item: &Value, key: &str) -> BTreeSet<String> {
    let mut hits = Vec::new();
    find_all(item, key, &mut hits);
    hits.iter()
        .filter_map(|h| h.as_str())
        .map(str::to_string)
        .collect()
}

fn title_of(item: &Value) -> String {
    let mut columns = Vec::new();
    find_all(item, "flexColumns", &mut columns);
    for column in columns {
        let mut runs = Vec::new();
        find_all(column, "runs", &mut runs);
        if let Some(items) = runs.first().and_then(|r| r.as_array()) {
            let text: String = items
                .iter()
                .filter_map(|r| r.get("text").and_then(Value::as_str))
                .collect();
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    "(untitled)".to_string()
}

struct Report {
    rows: usize,
    typed: usize,
    ambiguous_type: Vec<String>,
    ambiguous_id: Vec<String>,
    types: BTreeMap<String, usize>,
}

async fn scan(
    yt: &ytmusicapi::YTMusicClient,
    query: &str,
    filter: Option<&str>,
) -> Result<Report, Box<dyn std::error::Error>> {
    let body = match filter {
        Some(f) => json!({ "query": query, "params": f }),
        None => json!({ "query": query }),
    };
    let response = yt.send_request("search", body).await?;

    let mut items = Vec::new();
    find_all(&response, "musicResponsiveListItemRenderer", &mut items);

    let mut report = Report {
        rows: 0,
        typed: 0,
        ambiguous_type: Vec::new(),
        ambiguous_id: Vec::new(),
        types: BTreeMap::new(),
    };

    for item in items {
        let ids = all_strs(item, "videoId");
        if ids.is_empty() {
            continue; // artist / album / playlist row
        }
        report.rows += 1;
        let types = all_strs(item, "musicVideoType");
        if types.len() > 1 {
            report
                .ambiguous_type
                .push(format!("{} → {types:?}", title_of(item)));
        }
        if ids.len() > 1 {
            report
                .ambiguous_id
                .push(format!("{} → {ids:?}", title_of(item)));
        }
        if let Some(t) = types.iter().next() {
            report.typed += 1;
            *report
                .types
                .entry(t.trim_start_matches("MUSIC_VIDEO_TYPE_").to_string())
                .or_default() += 1;
        }
    }
    Ok(report)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let yt = Session::new()?.build_client()?;

    let mut ambiguous = 0;
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    let mut untyped = 0;
    let mut rows = 0;

    for (label, filter) in [
        ("songs", Some(SONGS_FILTER)),
        ("videos", Some(VIDEOS_FILTER)),
    ] {
        println!("\n═══ {label} filter ═══");
        for query in QUERIES {
            let r = scan(&yt, query, filter).await?;
            let spread: Vec<String> = r.types.iter().map(|(k, n)| format!("{k}×{n}")).collect();
            println!(
                "  {:<28} {:>2} rows  {}",
                query,
                r.rows,
                if spread.is_empty() {
                    "(none typed)".to_string()
                } else {
                    spread.join("  ")
                }
            );
            for bad in &r.ambiguous_type {
                println!("      ⚠ two types in one row: {bad}");
                ambiguous += 1;
            }
            for bad in &r.ambiguous_id {
                println!("      ⚠ two video ids in one row: {bad}");
                ambiguous += 1;
            }
            if label == "songs" {
                rows += r.rows;
                untyped += r.rows - r.typed;
                for (k, n) in r.types {
                    *totals.entry(k).or_default() += n;
                }
            }
        }
    }

    // ── does the parsed type agree with the word the UI prints? ──────────
    println!("\n═══ displayed category vs parsed type (unfiltered) ═══");
    let mut crosstab: BTreeMap<(String, String), usize> = BTreeMap::new();
    for query in QUERIES {
        let response = yt.send_request("search", json!({ "query": query })).await?;
        let mut items = Vec::new();
        find_all(&response, "musicResponsiveListItemRenderer", &mut items);
        for item in items {
            if all_strs(item, "videoId").is_empty() {
                continue;
            }
            let shown = displayed_category(item).unwrap_or_else(|| "(artist, no category)".into());
            let typed = all_strs(item, "musicVideoType")
                .into_iter()
                .next()
                .unwrap_or_else(|| "(none)".into())
                .trim_start_matches("MUSIC_VIDEO_TYPE_")
                .to_string();
            // Only the rows where the UI printed an actual category word can
            // disagree; the rest have nothing to compare against.
            let shown = if shown.chars().next().is_some_and(char::is_uppercase)
                && shown.split_whitespace().count() == 1
            {
                shown
            } else {
                "(artist, no category)".to_string()
            };
            *crosstab.entry((shown, typed)).or_default() += 1;
        }
    }
    for ((shown, typed), n) in &crosstab {
        println!("  {shown:<24} {typed:<22} {n}");
    }

    println!("\n═══ verdict ═══");
    println!(
        "  songs filter: {rows} playable rows across {} queries",
        QUERIES.len()
    );
    for (kind, n) in &totals {
        println!("    {kind:<24} {n}");
    }
    println!("  rows with no type at all:      {untyped}");
    println!("  rows whose label is ambiguous: {ambiguous}");
    Ok(())
}
