//! Ground truth for the lyrics ranker.
//!
//! Every entry in `lyrics.json` is a case where the automatic match was judged
//! wrong by hand, which makes the file a labelled dataset. This dumps, for each
//! of those tracks, every candidate the search ladder reaches together with the
//! signals a ranker could use — duration, metadata agreement, ladder relevance,
//! and the shape of the lyrics themselves.
//!
//! Run it before and after a ranking change and compare: how often does the top
//! candidate carry the same lyrics as the record that was chosen by hand?
//!
//! ```text
//! cargo run -p ytm-core --example override_audit > audit.json
//! ```
//!
//! Needs a set-up session — it reads the library to recover each track's real
//! title, artist and duration — and hits lrclib once per ladder rung per track.

use std::collections::HashMap;

use ytm_core::{LyricsKind, LyricsQuery, LyricsService, Session, TrackLyrics, library};

fn kind(c: &TrackLyrics) -> &'static str {
    match c.kind {
        LyricsKind::Synced(_) => "synced",
        LyricsKind::Plain(_) => "plain",
        LyricsKind::Instrumental => "instrumental",
    }
}

fn candidate_json(c: &TrackLyrics, want: Option<f64>, chosen: u64, auto: Option<u64>) -> String {
    let (n_lines, first_at, last_at) = match &c.kind {
        LyricsKind::Synced(l) => (
            l.len(),
            l.first().map(|x| x.at).unwrap_or(-1.0),
            l.last().map(|x| x.at).unwrap_or(-1.0),
        ),
        LyricsKind::Plain(l) => (l.len(), -1.0, -1.0),
        LyricsKind::Instrumental => (0, -1.0, -1.0),
    };
    let blank = match &c.kind {
        LyricsKind::Synced(l) => l.iter().filter(|x| x.text.trim().is_empty()).count(),
        LyricsKind::Plain(l) => l.iter().filter(|x| x.trim().is_empty()).count(),
        LyricsKind::Instrumental => 0,
    };
    serde_json::json!({
        "id": c.id,
        "kind": kind(c),
        "duration": c.duration,
        "delta": c.duration_delta(want),
        "timing_mismatch": c.timing_mismatch,
        "relevance": c.relevance,
        "track_name": c.track_name,
        "artist_name": c.artist_name,
        "album_name": c.album_name,
        "n_lines": n_lines,
        "blank_lines": blank,
        "first_at": first_at,
        "last_at": last_at,
        "is_chosen": c.id == chosen,
        "is_auto": auto == Some(c.id),
    })
    .to_string()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(ytm_core::session::lyrics_path())?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    let choices: HashMap<String, u64> = json["choices"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
        .collect();

    let session = Session::new()?;
    let yt = session.build_client()?;
    let playlists = library::get_playlists(&yt).await?;
    eprintln!("fetching {} playlists...", playlists.len());

    let mut tracks: HashMap<String, ytm_core::Track> = HashMap::new();
    for pl in &playlists {
        for t in library::get_songs(&yt, &pl.playlist_id).await {
            if let Some(id) = t.video_id.clone() {
                tracks.entry(id).or_insert(t);
            }
        }
    }
    eprintln!("indexed {} tracks", tracks.len());

    let svc = LyricsService::new();
    let mut out = Vec::new();

    for (video_id, chosen_id) in &choices {
        let Some(track) = tracks.get(video_id) else {
            eprintln!("{video_id}: not in library");
            continue;
        };
        let Some(q) = LyricsQuery::from_track(track) else {
            continue;
        };
        eprintln!("auditing {video_id} ({:?})", q.title);

        let auto = match svc.best_for(&q, None).await {
            Ok(best) => best.map(|b| b.id),
            Err(e) => {
                eprintln!("  best_for errored: {e}");
                None
            }
        };
        // Deliberately no `on_screen`: protecting the chosen record from
        // de-duplication would let it displace the copy the ranker actually
        // prefers, and the ranker is what is being measured. Choices that get
        // collapsed are compared on content instead.
        let all = match svc.candidates(&q, None).await {
            Ok(all) => all,
            Err(e) => {
                eprintln!("  candidates errored: {e}");
                continue;
            }
        };

        // The chosen record may sit outside the ladder's reach; fetch it so it
        // is always present with its own signals.
        let mut extra = String::new();
        if !all.iter().any(|c| c.id == *chosen_id)
            && let Ok(Some(c)) = svc.by_id(*chosen_id).await
        {
            extra = candidate_json(&c, q.duration, *chosen_id, auto);
        }

        let cands: Vec<serde_json::Value> = all
            .iter()
            .map(|c| {
                serde_json::from_str(&candidate_json(c, q.duration, *chosen_id, auto)).unwrap()
            })
            .collect();

        out.push(serde_json::json!({
            "video_id": video_id,
            "title": q.title,
            "artist": q.artist,
            "album": q.album,
            "duration": q.duration,
            "chosen_id": chosen_id,
            "auto_id": auto,
            "in_ladder": all.iter().any(|c| c.id == *chosen_id),
            "unreachable_chosen": if extra.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&extra).unwrap()
            },
            "candidates": cands,
        }));
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
