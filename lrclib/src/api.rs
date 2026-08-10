use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://lrclib.net/api";

/// lrclib.net asks callers to identify themselves.
const USER_AGENT: &str = concat!(
    "yt-music-tui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/s0me0neee/yt-music-tui)"
);

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum LrcError {
    /// The lrclib API returned a non-2xx status with a structured error body.
    #[error("[{status_code}] {name}: {message}")]
    Api {
        message: String,
        name: String,
        status_code: u16,
    },
    /// A network or (de)serialisation error from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

// ── Response types ────────────────────────────────────────────────────────────

/// A lyrics record returned by GET /api/get or GET /api/get/:id.
/// `plain_lyrics` and `synced_lyrics` are `None` for instrumental tracks
/// or entries where that data is missing.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Lyrics {
    pub id: u64,
    /// Same as `track_name`.
    #[serde(default)]
    pub name: String,
    #[serde(rename = "trackName")]
    pub track_name: String,
    #[serde(rename = "artistName")]
    pub artist_name: String,
    #[serde(rename = "albumName", default)]
    pub album_name: String,
    /// Track length in seconds. `None` for the occasional record where lrclib
    /// has no duration — a non-optional field here made a single such record
    /// fail the whole response.
    pub duration: Option<f64>,
    pub instrumental: bool,
    /// Unsynced plain-text lyrics.
    #[serde(rename = "plainLyrics")]
    pub plain_lyrics: Option<String>,
    /// LRC-format synced lyrics (`[mm:ss.xx] line`).
    #[serde(rename = "syncedLyrics")]
    pub synced_lyrics: Option<String>,
}

// Used only to deserialise API error bodies — not part of the public surface.
#[derive(Deserialize)]
struct ApiErrorBody {
    message: String,
    name: String,
    #[serde(rename = "statusCode")]
    status_code: u16,
}

impl From<ApiErrorBody> for LrcError {
    fn from(b: ApiErrorBody) -> Self {
        LrcError::Api {
            message: b.message,
            name: b.name,
            status_code: b.status_code,
        }
    }
}

/// Deserialises a search response, skipping records that don't parse.
///
/// lrclib is a community database and the occasional record is malformed; a
/// strict `Vec<Lyrics>` would throw away every good result alongside it.
fn parse_results(raw: Vec<serde_json::Value>) -> Vec<Lyrics> {
    raw.into_iter()
        .filter_map(|v| match serde_json::from_value::<Lyrics>(v) {
            Ok(l) => Some(l),
            Err(e) => {
                log::debug!("lrclib: skipping unparseable record: {e}");
                None
            }
        })
        .collect()
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Appends `(key, value)` only when `value` is non-blank.
fn push_if_set<'a>(params: &mut Vec<(&'static str, &'a str)>, key: &'static str, value: &'a str) {
    if !value.trim().is_empty() {
        params.push((key, value));
    }
}

pub struct LrcLib {
    client: Client,
}

impl Default for LrcLib {
    fn default() -> Self {
        Self::new()
    }
}

impl LrcLib {
    /// # Panics
    /// If the TLS backend fails to initialise. Construct this once, eagerly,
    /// before taking over the terminal.
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                // Bounded so a hung lrclib.net can never stall a caller's UI.
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    // ── GET /api/get ─────────────────────────────────────────────────────────

    /// Look up lyrics by track metadata. `duration` is in seconds.
    ///
    /// Returns `LrcError::Api` with `status_code = 404` when no match is found.
    pub async fn get(
        &self,
        track_name: &str,
        artist_name: &str,
        album_name: &str,
        duration: f64,
    ) -> Result<Lyrics, LrcError> {
        let duration = duration.to_string();
        let mut params = vec![("track_name", track_name), ("duration", duration.as_str())];
        // An empty album/artist is a filter that matches nothing — omit rather
        // than send it. Tracks frequently have no album metadata.
        push_if_set(&mut params, "artist_name", artist_name);
        push_if_set(&mut params, "album_name", album_name);

        let resp = self
            .client
            .get(format!("{BASE_URL}/get"))
            .query(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(resp.json::<ApiErrorBody>().await?.into());
        }
        Ok(resp.json::<Lyrics>().await?)
    }

    // ── GET /api/get/:id ─────────────────────────────────────────────────────

    /// Look up lyrics by lrclib numeric ID.
    pub async fn get_by_id(&self, id: u64) -> Result<Lyrics, LrcError> {
        let resp = self
            .client
            .get(format!("{BASE_URL}/get/{id}"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(resp.json::<ApiErrorBody>().await?.into());
        }
        Ok(resp.json::<Lyrics>().await?)
    }

    // ── GET /api/search ──────────────────────────────────────────────────────

    /// Full-text search across track name, artist, and album.
    pub async fn search(&self, query: &str) -> Result<Vec<Lyrics>, LrcError> {
        let resp = self
            .client
            .get(format!("{BASE_URL}/search"))
            .query(&[("q", query)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(resp.json::<ApiErrorBody>().await?.into());
        }
        Ok(parse_results(resp.json::<Vec<serde_json::Value>>().await?))
    }

    /// Metadata search — filter by individual fields instead of a free-text query.
    pub async fn search_by_meta(
        &self,
        track_name: &str,
        artist_name: &str,
        album_name: &str,
    ) -> Result<Vec<Lyrics>, LrcError> {
        let mut params = vec![("track_name", track_name)];
        push_if_set(&mut params, "artist_name", artist_name);
        push_if_set(&mut params, "album_name", album_name);

        let resp = self
            .client
            .get(format!("{BASE_URL}/search"))
            .query(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(resp.json::<ApiErrorBody>().await?.into());
        }
        Ok(parse_results(resp.json::<Vec<serde_json::Value>>().await?))
    }
}
// ── Tests ─────────────────────────────────────────────────────────────────────

// These hit the live API, so they are excluded from a plain `cargo test`.
// Run them deliberately with `cargo test -p lrclib -- --ignored`.
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn get_by_metadata() {
        let client = LrcLib::new();
        let lyrics = client
            .get("Echo", "Crusher-P", "", 245.0)
            .await
            .expect("get failed");
        assert!(lyrics.track_name.to_lowercase().contains("echo"));
    }

    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn get_by_id() {
        let client = LrcLib::new();
        let lyrics = client.get_by_id(1).await.expect("get_by_id failed");
        assert_eq!(lyrics.id, 1);
    }

    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn search_query() {
        let client = LrcLib::new();
        let results = client
            .search("Bohemian Rhapsody")
            .await
            .expect("search failed");
        assert!(!results.is_empty());
    }

    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn get_not_found_is_api_error() {
        let client = LrcLib::new();
        let err = client
            .get("zzz_no_such_track_xyzzy", "zzz_nobody", "", 1.0)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            LrcError::Api {
                status_code: 404,
                ..
            }
        ));
    }
}
