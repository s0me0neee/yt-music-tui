//! Playlists and tracks: fetching from YouTube Music and the in-memory
//! library that accumulates results as they stream in.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ytmusicapi::YTMusicClient;
pub use ytmusicapi::{Album, Artist};

use crate::error::{Error, Result};

// ── types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub video_id: Option<String>,
    pub title: Option<String>,
    pub artists: Vec<Artist>,
    pub album: Option<Album>,
    pub duration: Option<String>,
    pub duration_seconds: Option<u32>,
}

impl Track {
    /// Artist names joined with `", "`, or empty if there are none.
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub playlist_id: String,
    pub title: String,
    pub count: Option<u32>,
}

// ── fetching ─────────────────────────────────────────────────────────────────

#[hotpath::measure]
pub async fn get_playlists(yt: &YTMusicClient) -> Result<Vec<Playlist>> {
    match yt.get_library_playlists(None).await {
        Ok(list) => Ok(list
            .into_iter()
            .map(|pl| Playlist {
                playlist_id: pl.playlist_id,
                title: pl.title,
                count: pl.count,
            })
            .collect()),
        Err(ytmusicapi::Error::AuthRequired) => Err(Error::SessionExpired),
        Err(ytmusicapi::Error::Server { status: 401, .. }) => Err(Error::SessionExpired),
        Err(e) => Err(Error::Ytmusicapi(e)),
    }
}

#[hotpath::measure]
pub async fn get_songs(yt: &YTMusicClient, playlist_id: &str) -> Vec<Track> {
    log::debug!("get_songs: fetching {playlist_id}");
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        match yt.get_playlist(playlist_id, Some(5000)).await {
            Ok(pl) => {
                return pl
                    .tracks
                    .into_iter()
                    .map(|t| Track {
                        video_id: t.video_id,
                        title: t.title,
                        artists: t.artists,
                        album: t.album,
                        duration: t.duration,
                        duration_seconds: t.duration_seconds,
                    })
                    .collect();
            }
            Err(e) => {
                log::warn!("get_songs({playlist_id}) attempt {attempt}/{ATTEMPTS}: {e:#}");
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
    log::error!("get_songs({playlist_id}): giving up after {ATTEMPTS} attempts");
    Vec::new()
}

/// One playlist's freshly-fetched tracks, tagged with its index in the
/// `Vec<Playlist>` that was passed to [`spawn_library_fetch`].
pub type SongBatch = (usize, Vec<Track>);

/// Fetches every playlist's tracks in the background — one task per
/// playlist — streaming each result back over the returned channel as soon
/// as it completes, rather than blocking the caller on all of them.
pub fn spawn_library_fetch(
    handle: &tokio::runtime::Handle,
    yt: Arc<YTMusicClient>,
    playlists: &[Playlist],
) -> Receiver<SongBatch> {
    let (tx, rx) = std::sync::mpsc::channel();
    for (idx, pl) in playlists.iter().enumerate() {
        let yt = Arc::clone(&yt);
        let tx = tx.clone();
        let id = pl.playlist_id.clone();
        handle.spawn(async move {
            let songs = get_songs(&yt, &id).await;
            let _ = tx.send((idx, songs));
        });
    }
    rx
}

// ── in-memory library ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub playlist: Playlist,
    pub songs: Vec<Track>,
    /// Whether this playlist's tracks have finished loading.
    pub loaded: bool,
    /// Sum of `songs[*].duration_seconds`, recomputed each time songs are set.
    pub total_duration_secs: u64,
}

/// Playlists and their tracks, filled in progressively as background
/// fetches (see [`spawn_library_fetch`]) complete.
#[derive(Debug, Clone, Default)]
pub struct Library {
    entries: Vec<PlaylistEntry>,
}

impl Library {
    pub fn new(playlists: Vec<Playlist>) -> Self {
        let entries = playlists
            .into_iter()
            .map(|playlist| PlaylistEntry {
                playlist,
                songs: Vec::new(),
                loaded: false,
                total_duration_secs: 0,
            })
            .collect();
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[PlaylistEntry] {
        &self.entries
    }

    pub fn entry(&self, idx: usize) -> Option<&PlaylistEntry> {
        self.entries.get(idx)
    }

    pub fn playlist(&self, idx: usize) -> Option<&Playlist> {
        self.entries.get(idx).map(|e| &e.playlist)
    }

    pub fn songs(&self, idx: usize) -> &[Track] {
        self.entries.get(idx).map_or(&[], |e| e.songs.as_slice())
    }

    pub fn track(&self, pl_idx: usize, song_idx: usize) -> Option<&Track> {
        self.entries.get(pl_idx)?.songs.get(song_idx)
    }

    pub fn is_loaded(&self, idx: usize) -> bool {
        self.entries.get(idx).is_some_and(|e| e.loaded)
    }

    pub fn total_duration_secs(&self, idx: usize) -> u64 {
        self.entries.get(idx).map_or(0, |e| e.total_duration_secs)
    }

    /// Applies one background-fetched song batch: stores the tracks, marks
    /// the playlist loaded, and recomputes its total duration.
    pub fn apply_song_batch(&mut self, idx: usize, songs: Vec<Track>) {
        let Some(entry) = self.entries.get_mut(idx) else {
            return;
        };
        entry.total_duration_secs = songs
            .iter()
            .filter_map(|t| t.duration_seconds)
            .map(u64::from)
            .sum();
        entry.songs = songs;
        entry.loaded = true;
    }

    pub fn find_playlist_index(&self, playlist_id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.playlist.playlist_id == playlist_id)
    }

    pub fn find_song_index(&self, pl_idx: usize, video_id: &str) -> Option<usize> {
        self.entries
            .get(pl_idx)?
            .songs
            .iter()
            .position(|t| t.video_id.as_deref() == Some(video_id))
    }
}
