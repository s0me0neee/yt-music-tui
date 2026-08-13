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

/// One playlist's tracks, or `None` when every attempt failed.
///
/// The distinction is the caller's to make and it matters: an empty `Vec` and a
/// failed fetch used to arrive identically, so a network blip was displayed as
/// "this playlist is empty" for the rest of the session — and, because the
/// playlist counted as loaded, it also silently discarded a queue saved from
/// the previous run.
#[hotpath::measure]
pub async fn get_songs(yt: &YTMusicClient, playlist_id: &str) -> Option<Vec<Track>> {
    log::debug!("get_songs: fetching {playlist_id}");
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        match yt.get_playlist(playlist_id, Some(5000)).await {
            Ok(pl) => {
                return Some(
                    pl.tracks
                        .into_iter()
                        .map(|t| Track {
                            video_id: t.video_id,
                            title: t.title,
                            artists: t.artists,
                            album: t.album,
                            duration: t.duration,
                            duration_seconds: t.duration_seconds,
                        })
                        .collect(),
                );
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
    None
}

/// One playlist's freshly-fetched tracks, tagged with its index in the
/// `Vec<Playlist>` that was passed to [`LibraryFetcher::new`]. `None` where the
/// fetch failed — see [`get_songs`].
pub type SongBatch = (usize, Option<Vec<Track>>);

/// Fetches playlists' tracks in the background, streaming each result back over
/// a channel as it completes rather than blocking on all of them.
///
/// Kept as a handle rather than a one-shot call so a playlist whose fetch
/// failed can be asked for again later, on the user's `r`, without the UI
/// needing to know what a `YTMusicClient` is.
pub struct LibraryFetcher {
    yt: Arc<YTMusicClient>,
    handle: tokio::runtime::Handle,
    tx: std::sync::mpsc::Sender<SongBatch>,
}

impl LibraryFetcher {
    /// Starts a fetch for every playlist and returns the fetcher alongside the
    /// channel its results arrive on.
    pub fn new(
        handle: &tokio::runtime::Handle,
        yt: Arc<YTMusicClient>,
        playlists: &[Playlist],
    ) -> (Self, Receiver<SongBatch>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let fetcher = Self {
            yt,
            handle: handle.clone(),
            tx,
        };
        for (idx, pl) in playlists.iter().enumerate() {
            fetcher.fetch(idx, &pl.playlist_id);
        }
        (fetcher, rx)
    }

    /// Re-runs one playlist's fetch. The result arrives on the same channel as
    /// the first attempt's, so nothing else has to know it was a retry.
    pub fn fetch(&self, idx: usize, playlist_id: &str) {
        let yt = Arc::clone(&self.yt);
        let tx = self.tx.clone();
        let id = playlist_id.to_string();
        self.handle.spawn(async move {
            let songs = get_songs(&yt, &id).await;
            let _ = tx.send((idx, songs));
        });
    }
}

// ── in-memory library ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub playlist: Playlist,
    pub songs: Vec<Track>,
    /// Whether this playlist's tracks have finished loading. Stays false when
    /// the fetch failed, so nothing downstream reads a failure as "loaded, and
    /// it has no songs".
    pub loaded: bool,
    /// The last fetch for this playlist failed and there is nothing to show.
    /// Cleared when a retry is started, so the UI goes back to loading.
    pub failed: bool,
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
                failed: false,
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

    /// Whether this playlist's last fetch failed, so the UI can say so instead
    /// of showing an empty list, and offer to try again.
    pub fn has_failed(&self, idx: usize) -> bool {
        self.entries.get(idx).is_some_and(|e| e.failed)
    }

    pub fn total_duration_secs(&self, idx: usize) -> u64 {
        self.entries.get(idx).map_or(0, |e| e.total_duration_secs)
    }

    /// Applies one background-fetched song batch: stores the tracks, marks
    /// the playlist loaded, and recomputes its total duration.
    ///
    /// `None` is a failed fetch. It leaves the playlist *unloaded* and flags
    /// it, so the UI can offer a retry and a saved queue that references it
    /// keeps waiting rather than being abandoned.
    pub fn apply_song_batch(&mut self, idx: usize, songs: Option<Vec<Track>>) {
        let Some(entry) = self.entries.get_mut(idx) else {
            return;
        };
        let Some(songs) = songs else {
            log::warn!(
                "library: {:?} could not be fetched",
                entry.playlist.title.as_str()
            );
            entry.failed = true;
            return;
        };
        entry.total_duration_secs = songs
            .iter()
            .filter_map(|t| t.duration_seconds)
            .map(u64::from)
            .sum();
        entry.songs = songs;
        entry.loaded = true;
        entry.failed = false;
    }

    /// Marks a playlist as being fetched again, so it reads as loading rather
    /// than failed until the answer arrives.
    pub fn mark_retrying(&mut self, idx: usize) {
        if let Some(entry) = self.entries.get_mut(idx) {
            entry.failed = false;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> Library {
        Library::new(vec![Playlist {
            playlist_id: "PL1".to_string(),
            title: "Mine".to_string(),
            count: Some(1),
        }])
    }

    fn track(video_id: &str) -> Track {
        Track {
            video_id: Some(video_id.to_string()),
            title: Some("Song".to_string()),
            artists: Vec::new(),
            album: None,
            duration: None,
            duration_seconds: Some(100),
        }
    }

    #[test]
    fn a_playlist_that_arrives_is_loaded() {
        let mut lib = library();
        lib.apply_song_batch(0, Some(vec![track("aaa")]));
        assert!(lib.is_loaded(0));
        assert!(!lib.has_failed(0));
        assert_eq!(lib.total_duration_secs(0), 100);
    }

    #[test]
    fn a_playlist_that_failed_is_not_an_empty_one() {
        // The whole point: "loaded, and it has no songs" is what this used to
        // say, which reads to the user as an empty playlist and to
        // `try_restore` as grounds for throwing the saved queue away.
        let mut lib = library();
        lib.apply_song_batch(0, None);
        assert!(!lib.is_loaded(0));
        assert!(lib.has_failed(0));
        assert!(lib.songs(0).is_empty());
    }

    #[test]
    fn an_empty_playlist_is_still_loaded() {
        let mut lib = library();
        lib.apply_song_batch(0, Some(Vec::new()));
        assert!(lib.is_loaded(0));
        assert!(!lib.has_failed(0));
    }

    #[test]
    fn a_retry_reads_as_loading_and_then_clears_the_failure() {
        let mut lib = library();
        lib.apply_song_batch(0, None);
        lib.mark_retrying(0);
        assert!(!lib.has_failed(0), "shows the throbber, not the error");
        assert!(!lib.is_loaded(0));

        lib.apply_song_batch(0, Some(vec![track("aaa")]));
        assert!(lib.is_loaded(0));
        assert!(!lib.has_failed(0));
    }

    #[test]
    fn a_batch_for_a_playlist_that_does_not_exist_is_dropped() {
        let mut lib = library();
        lib.apply_song_batch(9, Some(vec![track("aaa")]));
        lib.apply_song_batch(9, None);
        assert_eq!(lib.len(), 1);
    }
}
