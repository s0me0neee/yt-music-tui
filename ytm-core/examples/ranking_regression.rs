//! Checks a ranking change against the picks the previous run actually made.
//!
//! `app.log` records what the automatic matcher chose for every track played,
//! and the real audio length mpv reported for it. That makes the log a
//! before-picture: re-run the matcher now, compare, and every track falls into
//! one of a handful of buckets — unchanged, changed but carrying the same
//! lyrics, changed to the record that had to be corrected by hand (a fix), or
//! changed on a track nobody complained about (a risk, and the one to read).
//!
//! ```text
//! cargo run -p ytm-core --example ranking_regression
//! ```
//!
//! The log is truncated on every start, so this compares against the most
//! recent session only. Run it before changing the ranker, not after.

use std::collections::HashMap;

use ytm_core::{LyricsKind, LyricsQuery, LyricsService, Session, TrackLyrics, library};

/// What the previous run did with one track.
struct Before {
    title: String,
    picked: Option<u64>,
    duration: Option<f64>,
}

/// Reads the before-picture, either from `app.log` directly or from a JSON
/// snapshot of one taken earlier — `{"title":{}, "auto":{}, "dur":{}}` keyed by
/// video id. The log is truncated on every app start, so a snapshot is often
/// the only surviving copy.
fn parse_baseline(arg: Option<String>) -> HashMap<String, Before> {
    let mut out: HashMap<String, Before> = HashMap::new();

    if let Some(path) = arg {
        let raw = std::fs::read_to_string(&path).expect("baseline file");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("baseline json");
        for (id, title) in v["title"].as_object().into_iter().flatten() {
            out.insert(
                id.clone(),
                Before {
                    title: title.as_str().unwrap_or_default().to_string(),
                    picked: v["auto"][id].as_u64(),
                    duration: v["dur"][id].as_f64(),
                },
            );
        }
        return out;
    }

    let path = ytm_core::session::app_config_dir().join("app.log");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut playing: Option<String> = None;

    for line in raw.lines() {
        if let Some(rest) = line.split_once("lyrics: fetching for ").map(|(_, r)| r) {
            let (id, title) = rest.split_once(' ').unwrap_or((rest, ""));
            out.entry(id.to_string()).or_insert(Before {
                title: title.trim_matches(['(', ')']).to_string(),
                picked: None,
                duration: None,
            });
        }
        if let Some(rest) = line.split_once("lyrics: got #").map(|(_, r)| r)
            && let Some((num, id)) = rest.split_once(" for ")
            && let Ok(rec) = num.parse::<u64>()
            && let Some(before) = out.get_mut(id.trim())
        {
            // First win only: a later one would be a manual choice being
            // re-read, not what the matcher decided on its own.
            before.picked.get_or_insert(rec);
        }
        if let Some(rest) = line.split_once("videoId=Some(\"").map(|(_, r)| r)
            && let Some((id, _)) = rest.split_once('"')
        {
            playing = Some(id.to_string());
        }
        if let Some(rest) = line.split_once("[audio] duration: ").map(|(_, r)| r)
            && let Ok(secs) = rest.trim_end_matches('s').parse::<f64>()
            && let Some(id) = &playing
            && let Some(before) = out.get_mut(id)
        {
            before.duration = Some(secs);
        }
    }
    out
}

fn content(c: &TrackLyrics) -> (usize, String) {
    match &c.kind {
        LyricsKind::Synced(l) => (
            l.len(),
            l.iter()
                .map(|x| format!("{:.1}|{}", x.at, x.text))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        LyricsKind::Plain(l) => (l.len(), l.join("\n")),
        LyricsKind::Instrumental => (0, "instrumental".into()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    // `--dump <playlist>` writes this build's picks for every track in a named
    // playlist, with no before-picture. Run it on two commits and diff, for the
    // tracks the log never covered.
    let dump = args
        .iter()
        .position(|a| a == "--dump")
        .map(|i| args[i + 1].clone());
    let before = parse_baseline(args.get(1).filter(|a| *a != "--dump").cloned());
    eprintln!("{} tracks in the previous run's log", before.len());

    let overrides: HashMap<String, u64> = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(ytm_core::session::lyrics_path()).unwrap_or_default(),
    )
    .ok()
    .and_then(|v| v["choices"].as_object().cloned())
    .map(|m| {
        m.iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_u64()?)))
            .collect()
    })
    .unwrap_or_default();
    eprintln!("{} manual overrides", overrides.len());

    let session = Session::new()?;
    if let Err(e) = session.refresh_cookies() {
        eprintln!("cookie refresh failed ({e}) — trying the cached session");
    }
    let yt = session.build_client()?;
    let playlists = library::get_playlists(&yt).await?;
    if playlists.is_empty() {
        eprintln!("no playlists — the session is expired; run the app once");
        return Ok(());
    }
    let mut tracks: HashMap<String, ytm_core::Track> = HashMap::new();
    for pl in &playlists {
        if let Some(want) = &dump
            && &pl.title != want
        {
            continue;
        }
        for t in library::get_songs(&yt, &pl.playlist_id).await {
            if let Some(id) = t.video_id.clone() {
                tracks.entry(id).or_insert(t);
            }
        }
    }
    eprintln!("indexed {} library tracks\n", tracks.len());

    let svc = LyricsService::new();
    let mut rows = Vec::new();

    // In dump mode every track in the playlist is examined, whether or not the
    // log ever saw it; otherwise only what there is a before-picture for.
    let subjects: Vec<String> = match &dump {
        Some(_) => tracks.keys().cloned().collect(),
        None => before.keys().cloned().collect(),
    };
    let blank = Before {
        title: String::new(),
        picked: None,
        duration: None,
    };

    for video_id in &subjects {
        let b = before.get(video_id).unwrap_or(&blank);
        let Some(track) = tracks.get(video_id) else {
            continue;
        };
        let Some(mut q) = LyricsQuery::from_track(track) else {
            continue;
        };
        // The real length mpv measured, which is what the app now ranks with.
        if b.duration.is_some() {
            q.duration = b.duration;
        }

        let now = match svc.best_for(&q, None).await {
            Ok(found) => found,
            Err(e) => {
                eprintln!("  {} errored: {e}", b.title);
                continue;
            }
        };
        eprint!(".");

        let old_rec = match b.picked {
            Some(id) => svc.by_id(id).await.ok().flatten(),
            None => None,
        };

        rows.push(serde_json::json!({
            "video_id": video_id,
            "title": if b.title.is_empty() { track.title.clone().unwrap_or_default() } else { b.title.clone() },
            "duration": b.duration,
            "overridden": overrides.get(video_id),
            "before_id": b.picked,
            "before_content": old_rec.as_ref().map(|c| content(c).1),
            "before_lines": old_rec.as_ref().map(|c| content(c).0),
            "after_id": now.as_ref().map(|c| c.id),
            "after_content": now.as_ref().map(|c| content(c).1),
            "after_lines": now.as_ref().map(|c| content(c).0),
            "after_artist": now.as_ref().map(|c| c.artist_name.clone()),
            "after_dur": now.as_ref().and_then(|c| c.duration),
            "before_artist": old_rec.as_ref().map(|c| c.artist_name.clone()),
            "before_dur": old_rec.as_ref().and_then(|c| c.duration),
        }));
    }
    eprintln!();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
