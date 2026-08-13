//! YouTube Music search — policy over the raw InnerTube endpoint.
//!
//! `ytmusicapi 0.4.2` has no search of its own, but [`YTMusicClient::send_request`]
//! is public, so this is built on the client already in the graph: same cookies,
//! same context, no second HTTP stack. The parsing is deliberately
//! shape-tolerant — YouTube renames renderers without notice, and the first
//! version of this was written against `musicShelfRenderer` a week before the
//! unfiltered response started using `musicCardShelfRenderer` instead. Rather
//! than walk a fixed path, [`parse`] finds every result row wherever it is
//! nested and reads each row's own fields.
//!
//! ## Songs and videos are both fetched, on purpose
//!
//! YouTube Music types every result through `musicVideoType`:
//!
//! - `MUSIC_VIDEO_TYPE_ATV` — an *art track*: the label's catalogue audio,
//!   which YouTube wraps in the album cover. This is what the UI calls a Song.
//! - `MUSIC_VIDEO_TYPE_OMV` — an official music video.
//! - `MUSIC_VIDEO_TYPE_UGC` — a user upload.
//! - `MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC` — an official non-art-track source.
//!
//! The songs filter returns only ATV — measured at 198 of 198 rows across ten
//! queries, see `examples/search_verify.rs`. That is the cleaner result in
//! every way: it carries the album, the release duration, and metadata good
//! enough for the lyrics matcher to settle on the first request.
//!
//! But plenty of music exists on YouTube *only* as a video — a self-released
//! track that was never distributed, a doujin upload, anything where the
//! "video" is a still image and the audio is the whole point. Searching songs
//! alone silently cannot find those, so [`search`] asks for both and labels
//! each row with its [`ResultKind`]. The UI marks them differently; the user
//! decides. Playback is unaffected either way — mpv is given `bestaudio` and
//! never fetches a video stream.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use serde_json::{Value, json};
use ytmusicapi::YTMusicClient;

use crate::error::Result;

/// The opaque `params` blobs YouTube Music's own UI sends to filter a search.
/// Verified against the live API; see the module docs.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D";
const VIDEOS_FILTER: &str = "EgWKAQIQAWoKEAkQChAFEAMQBA%3D%3D";

/// How many results to keep per filter. YouTube returns 20 a page and the list
/// is a picker, not a catalogue — past this it is quicker to refine the query.
const PER_FILTER: usize = 20;

/// What a result *is*, reduced from `musicVideoType` to the distinction that
/// matters when choosing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    /// An art track: catalogue audio with an album and a release duration.
    Song,
    /// A video — official or user-uploaded. Often still the only copy of a
    /// track that was never distributed as a song.
    Video,
}

impl ResultKind {
    /// The classification, or `None` for a row that is neither — a podcast
    /// episode, or a type YouTube has added since.
    #[must_use]
    pub fn from_video_type(video_type: &str) -> Option<Self> {
        match video_type {
            "MUSIC_VIDEO_TYPE_ATV" => Some(Self::Song),
            "MUSIC_VIDEO_TYPE_OMV"
            | "MUSIC_VIDEO_TYPE_UGC"
            | "MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC" => Some(Self::Video),
            _ => None,
        }
    }

    /// The word the UI shows against a row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Video => "video",
        }
    }
}

/// One search hit.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub video_id: String,
    pub title: String,
    /// Credited artist, or the uploading channel for a user upload.
    pub artist: String,
    /// Album, where the row carries one. Videos rarely do.
    pub album: String,
    /// As displayed — `"3:12"`. Empty when the row carries no duration.
    pub duration: String,
    /// The same, in seconds, for the lyrics matcher.
    pub duration_seconds: Option<u32>,
    pub kind: ResultKind,
    /// The raw `musicVideoType`, kept so a surprising classification can be
    /// explained without another request.
    pub video_type: String,
    /// Cover art URL, largest first. Empty when the row carries none.
    pub thumbnail: Option<String>,
}

impl SearchResult {
    /// A [`crate::library::Track`], so a search hit can be queued and played
    /// through exactly the same path as a library one.
    #[must_use]
    pub fn to_track(&self) -> crate::library::Track {
        crate::library::Track {
            video_id: Some(self.video_id.clone()),
            title: Some(self.title.clone()),
            artists: self
                .artist
                .split(',')
                .map(|name| ytmusicapi::Artist {
                    name: name.trim().to_string(),
                    id: None,
                })
                .filter(|a| !a.name.is_empty())
                .collect(),
            album: (!self.album.is_empty()).then(|| ytmusicapi::Album {
                name: self.album.clone(),
                id: None,
            }),
            duration: (!self.duration.is_empty()).then(|| self.duration.clone()),
            duration_seconds: self.duration_seconds,
            thumbnail: self.thumbnail.clone(),
        }
    }
}

// ── JSON walking ─────────────────────────────────────────────────────────────

/// Every value under `key`, however deep.
///
/// Walking rather than pathing is what makes this survive YouTube's renderer
/// renames. Within a single result row it is also *unambiguous*: measured over
/// ~340 rows, no row ever carried two different `videoId`s or two different
/// `musicVideoType`s, so "the first hit inside this row" is always the row's
/// own. `examples/search_verify.rs` is the check.
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

fn first_str(v: &Value, key: &str) -> Option<String> {
    let mut hits = Vec::new();
    find_all(v, key, &mut hits);
    hits.first().and_then(|h| h.as_str()).map(str::to_string)
}

/// The text of each display column of a row, in order.
fn columns(item: &Value) -> Vec<String> {
    let mut arrays = Vec::new();
    find_all(item, "flexColumns", &mut arrays);
    arrays
        .iter()
        .filter_map(|a| a.as_array())
        .flatten()
        .filter_map(|column| {
            let mut runs = Vec::new();
            find_all(column, "runs", &mut runs);
            let text: String = runs
                .first()?
                .as_array()?
                .iter()
                .filter_map(|r| r.get("text").and_then(Value::as_str))
                .collect();
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

/// `"3:12"` → 192. Also handles `"1:02:03"`.
#[must_use]
pub fn parse_duration(text: &str) -> Option<u32> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut secs: u32 = 0;
    for part in &parts {
        let n: u32 = part.trim().parse().ok()?;
        secs = secs.checked_mul(60)?.checked_add(n)?;
    }
    Some(secs)
}

/// Whether a column segment looks like a duration rather than a view count or
/// an album name.
fn is_duration(text: &str) -> bool {
    parse_duration(text).is_some()
}

/// The largest cover URL on a row.
///
/// YouTube lists thumbnails smallest-first and every size is the same image, so
/// the last is the one worth fetching — a 60px crop looks like porridge once a
/// terminal scales it into a cell block.
fn thumbnail(item: &Value) -> Option<String> {
    let mut lists = Vec::new();
    find_all(item, "thumbnails", &mut lists);
    lists
        .iter()
        .filter_map(|l| l.as_array())
        .flatten()
        .filter_map(|t| {
            let url = t.get("url")?.as_str()?.to_string();
            let width = t.get("width").and_then(Value::as_u64).unwrap_or(0);
            Some((width, url))
        })
        .max_by_key(|(w, _)| *w)
        .map(|(_, url)| url)
}

/// Every playable row in a response, in the order YouTube returned them.
fn parse(response: &Value, limit: usize) -> Vec<SearchResult> {
    let mut items = Vec::new();
    find_all(response, "musicResponsiveListItemRenderer", &mut items);

    let mut out = Vec::new();
    for item in items {
        // No video id ⇒ an artist, album or playlist row: nothing to play.
        let Some(video_id) = first_str(item, "videoId") else {
            continue;
        };
        let video_type = first_str(item, "musicVideoType").unwrap_or_default();
        // A podcast episode is neither a song nor a video, and a type we don't
        // know is not one to guess at.
        let Some(kind) = ResultKind::from_video_type(&video_type) else {
            continue;
        };

        let cols = columns(item);
        let Some(title) = cols.first().cloned() else {
            continue;
        };

        // The second column is `artist • album • duration` for a song and
        // `channel • N views • duration` for a video — same shape, different
        // middle, and the duration is only sometimes present.
        let detail: Vec<String> = cols
            .get(1)
            .map(|s| s.split('•').map(|p| p.trim().to_string()).collect())
            .unwrap_or_default();
        let duration = detail
            .iter()
            .rev()
            .find(|p| is_duration(p))
            .cloned()
            .unwrap_or_default();
        // Whatever sits between the artist and the duration, when it isn't a
        // view count. Videos say "1.2M views"; songs name the album.
        let album = detail
            .get(1)
            .filter(|p| !is_duration(p) && !p.ends_with("views") && !p.ends_with("plays"))
            .cloned()
            .unwrap_or_default();

        out.push(SearchResult {
            video_id,
            title,
            artist: detail.first().cloned().unwrap_or_default(),
            album,
            duration_seconds: parse_duration(&duration),
            duration,
            kind,
            video_type,
            thumbnail: thumbnail(item),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ── the request ──────────────────────────────────────────────────────────────

async fn run_filter(yt: &YTMusicClient, query: &str, params: &str) -> Result<Vec<SearchResult>> {
    let body = json!({ "query": query, "params": params });
    let response = yt.send_request("search", body).await?;
    Ok(parse(&response, PER_FILTER))
}

/// Songs first, then any video that isn't already listed.
///
/// Two requests rather than one unfiltered search: the filtered responses are
/// uniform and carry the album and duration, while an unfiltered one mixes in
/// artists, playlists, podcasts and profiles — 15 of 32 rows on a measured
/// query had no video id at all.
pub async fn search(yt: &YTMusicClient, query: &str) -> Result<Vec<SearchResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let songs = run_filter(yt, query, SONGS_FILTER).await?;
    // A failed video search should not cost the songs that already arrived.
    let videos = match run_filter(yt, query, VIDEOS_FILTER).await {
        Ok(found) => found,
        Err(e) => {
            log::warn!("search: the video results failed ({e}) — songs only");
            Vec::new()
        }
    };

    let mut seen: std::collections::HashSet<String> =
        songs.iter().map(|r| r.video_id.clone()).collect();
    let mut out = songs;
    for video in videos {
        if seen.insert(video.video_id.clone()) {
            out.push(video);
        }
    }
    log::info!("search: {query:?} → {} results", out.len());
    Ok(out)
}

/// Adds a track to one of the user's playlists.
pub async fn add_to_playlist(yt: &YTMusicClient, playlist_id: &str, video_id: &str) -> Result<()> {
    yt.add_playlist_items(playlist_id, &[video_id.to_string()], false)
        .await?;
    Ok(())
}

/// Likes a track, which is what "add to Liked Music" means to the API.
pub async fn like(yt: &YTMusicClient, video_id: &str) -> Result<()> {
    yt.like_song(video_id).await?;
    Ok(())
}

// ── background work ──────────────────────────────────────────────────────────

/// A finished search, or a finished add.
pub enum SearchMsg {
    Results {
        query: String,
        result: std::result::Result<Vec<SearchResult>, String>,
    },
    /// An `a` that has landed. `where_to` is the playlist's display name.
    Added {
        title: String,
        where_to: String,
        result: std::result::Result<(), String>,
    },
}

pub fn spawn_search(
    handle: &tokio::runtime::Handle,
    yt: Arc<YTMusicClient>,
    query: String,
    tx: Sender<SearchMsg>,
) {
    handle.spawn(async move {
        let result = search(&yt, &query).await.map_err(|e| e.to_string());
        let _ = tx.send(SearchMsg::Results { query, result });
    });
}

/// `playlist_id` empty means Liked Music, which is its own endpoint rather
/// than a playlist you can add items to.
pub fn spawn_add(
    handle: &tokio::runtime::Handle,
    yt: Arc<YTMusicClient>,
    playlist_id: String,
    video_id: String,
    title: String,
    where_to: String,
    tx: Sender<SearchMsg>,
) {
    handle.spawn(async move {
        let result = if playlist_id.is_empty() {
            like(&yt, &video_id).await
        } else {
            add_to_playlist(&yt, &playlist_id, &video_id).await
        };
        let _ = tx.send(SearchMsg::Added {
            title,
            where_to,
            result: result.map_err(|e| e.to_string()),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_becomes_the_distinction_that_matters() {
        assert_eq!(
            ResultKind::from_video_type("MUSIC_VIDEO_TYPE_ATV"),
            Some(ResultKind::Song)
        );
        for video in [
            "MUSIC_VIDEO_TYPE_OMV",
            "MUSIC_VIDEO_TYPE_UGC",
            "MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC",
        ] {
            assert_eq!(ResultKind::from_video_type(video), Some(ResultKind::Video));
        }
        // Neither, and not to be guessed at.
        assert_eq!(
            ResultKind::from_video_type("MUSIC_VIDEO_TYPE_PODCAST_EPISODE"),
            None
        );
        assert_eq!(ResultKind::from_video_type(""), None);
        assert_eq!(
            ResultKind::from_video_type("MUSIC_VIDEO_TYPE_SOMETHING_NEW"),
            None
        );
    }

    #[test]
    fn durations_parse_and_view_counts_do_not() {
        assert_eq!(parse_duration("3:12"), Some(192));
        assert_eq!(parse_duration("0:45"), Some(45));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration(" 5:55 "), Some(355));
        // The things that share the column with it.
        assert_eq!(parse_duration("815K plays"), None);
        assert_eq!(parse_duration("2B views"), None);
        assert_eq!(parse_duration("AREEL"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("3:12:00:00"), None);
    }

    /// One row, shaped like the real thing: the fields this parser needs, at
    /// the depths YouTube puts them.
    fn row(video_type: &str, title: &str, detail: &str) -> Value {
        json!({
            "musicResponsiveListItemRenderer": {
                "playlistItemData": { "videoId": "abc12345678" },
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": {
                        "text": { "runs": [{ "text": title }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": {
                        "text": { "runs": [{ "text": detail }] } } }
                ],
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://small", "width": 60, "height": 60 },
                    { "url": "https://large", "width": 544, "height": 544 }
                ] } } },
                "menu": { "menuRenderer": { "items": [ { "menuNavigationItemRenderer": {
                    "navigationEndpoint": { "watchEndpoint": {
                        "watchEndpointMusicSupportedConfigs": {
                            "watchEndpointMusicConfig": { "musicVideoType": video_type } } } } } } ] } }
            }
        })
    }

    #[test]
    fn a_song_row_yields_album_and_duration() {
        let found = parse(
            &row("MUSIC_VIDEO_TYPE_ATV", "typing", "ariiol • AREEL • 3:12"),
            20,
        );
        assert_eq!(found.len(), 1);
        let hit = &found[0];
        assert_eq!(hit.kind, ResultKind::Song);
        assert_eq!(hit.title, "typing");
        assert_eq!(hit.artist, "ariiol");
        assert_eq!(hit.album, "AREEL");
        assert_eq!(hit.duration, "3:12");
        assert_eq!(hit.duration_seconds, Some(192));
        // Largest, not first — a 60px crop is porridge once scaled up.
        assert_eq!(hit.thumbnail.as_deref(), Some("https://large"));
    }

    #[test]
    fn a_video_rows_view_count_is_not_mistaken_for_an_album() {
        let found = parse(
            &row(
                "MUSIC_VIDEO_TYPE_UGC",
                "typing",
                "some channel • 1.2M views • 3:14",
            ),
            20,
        );
        let hit = &found[0];
        assert_eq!(hit.kind, ResultKind::Video);
        assert_eq!(hit.artist, "some channel");
        assert_eq!(hit.album, "", "a view count is not an album");
        assert_eq!(hit.duration, "3:14");
    }

    #[test]
    fn a_row_with_no_duration_is_still_a_result() {
        let found = parse(&row("MUSIC_VIDEO_TYPE_OMV", "live set", "a channel"), 20);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].duration, "");
        assert_eq!(found[0].duration_seconds, None);
    }

    #[test]
    fn rows_that_are_not_music_are_dropped() {
        // A podcast episode, and an artist row with no video id at all.
        let episode = row("MUSIC_VIDEO_TYPE_PODCAST_EPISODE", "an episode", "a show");
        assert!(parse(&episode, 20).is_empty());

        let artist = json!({ "musicResponsiveListItemRenderer": {
            "flexColumns": [ { "musicResponsiveListItemFlexColumnRenderer": {
                "text": { "runs": [{ "text": "Queen" }] } } } ] } });
        assert!(parse(&artist, 20).is_empty());
    }

    #[test]
    fn the_result_count_is_bounded() {
        let many = json!(vec![
            row("MUSIC_VIDEO_TYPE_ATV", "x", "y • z • 1:00");
            50
        ]);
        assert_eq!(parse(&many, 20).len(), 20);
    }

    /// Hits the real API with the user's session.
    /// `cargo test -p ytm-core search -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network + a set-up session"]
    async fn live_a_search_returns_both_kinds() {
        let yt = crate::Session::new()
            .expect("session")
            .build_client()
            .expect("client");
        let found = search(&yt, "ariiol typing").await.expect("searched");
        for hit in &found {
            eprintln!(
                "{:<6} {:<12} {}  ·  {}  ·  {}  ·  {}",
                hit.kind.label(),
                hit.video_id,
                hit.title,
                hit.artist,
                hit.album,
                hit.duration
            );
        }
        assert!(!found.is_empty());
        // Both kinds, since a track that only exists as a video is the reason
        // the video filter is asked for at all.
        assert!(found.iter().any(|h| h.kind == ResultKind::Song));
        assert!(found.iter().any(|h| h.kind == ResultKind::Video));
        // Every row is playable and captioned.
        for hit in &found {
            assert!(!hit.video_id.is_empty(), "{hit:?}");
            assert!(!hit.title.is_empty(), "{hit:?}");
        }
        // Songs carry a duration; that is what the lyrics matcher needs.
        let songs: Vec<_> = found
            .iter()
            .filter(|h| h.kind == ResultKind::Song)
            .collect();
        assert!(
            songs.iter().all(|h| h.duration_seconds.is_some()),
            "a song came back with no duration"
        );
        // And a cover to show.
        assert!(songs.iter().all(|h| h.thumbnail.is_some()));
    }

    #[test]
    fn a_result_converts_to_a_playable_track() {
        let found = parse(
            &row(
                "MUSIC_VIDEO_TYPE_ATV",
                "typing",
                "ariiol, Kaai Yuki • AREEL • 3:12",
            ),
            20,
        );
        let track = found[0].to_track();
        assert_eq!(track.video_id.as_deref(), Some("abc12345678"));
        assert_eq!(track.title.as_deref(), Some("typing"));
        assert_eq!(track.artist_names(), "ariiol, Kaai Yuki");
        assert_eq!(track.duration_seconds, Some(192));
        assert_eq!(track.album.map(|a| a.name).as_deref(), Some("AREEL"));
    }
}
