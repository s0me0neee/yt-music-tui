//! Queue persistence: serialising the live queue to `queue.json` on exit and
//! resolving it back to `(playlist_idx, song_idx)` pairs on the next launch.
//! Also holds user settings persisted to `settings.json`.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::Library;
use crate::player::TrackRef;
use crate::session::{lyrics_path, queue_path, settings_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub playlist_id: Option<String>,
    pub video_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueState {
    /// Ordered queue entries — each carries its own playlist ID so the queue
    /// can span multiple playlists.
    pub entries: Vec<QueueEntry>,
    /// Current position within `entries`.
    pub position: Option<usize>,
}

pub fn save_queue(state: &QueueState) -> Result<()> {
    std::fs::write(queue_path(), serde_json::to_string_pretty(state)?)?;
    Ok(())
}

pub fn load_queue() -> Option<QueueState> {
    let json = std::fs::read_to_string(queue_path()).ok()?;
    serde_json::from_str(&json).ok()
}

// ── settings ─────────────────────────────────────────────────────────────────

fn default_volume() -> u8 {
    80
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Playback volume (0-100), restored on the next launch.
    #[serde(default = "default_volume")]
    pub volume: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
        }
    }
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    std::fs::write(settings_path(), serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

pub fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

// ── lyrics overrides ─────────────────────────────────────────────────────────

/// Manual lyric choices: `video_id` → lrclib record id.
///
/// Wrapped in a struct rather than serialised as a bare map so later fields
/// don't break the on-disk format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LyricsOverrides {
    #[serde(default)]
    pub choices: std::collections::HashMap<String, u64>,
}

impl LyricsOverrides {
    pub fn get(&self, video_id: &str) -> Option<u64> {
        self.choices.get(video_id).copied()
    }

    pub fn set(&mut self, video_id: &str, id: u64) {
        self.choices.insert(video_id.to_string(), id);
    }

    /// Reverts a track to automatic matching.
    pub fn clear(&mut self, video_id: &str) {
        self.choices.remove(video_id);
    }
}

pub fn save_lyrics_overrides(overrides: &LyricsOverrides) -> Result<()> {
    std::fs::write(lyrics_path(), serde_json::to_string_pretty(overrides)?)?;
    Ok(())
}

pub fn load_lyrics_overrides() -> LyricsOverrides {
    std::fs::read_to_string(lyrics_path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Serialises a live queue into a [`QueueState`] ready for [`save_queue`].
/// Returns `None` if the queue is empty or none of its entries resolve to a
/// (non-empty) video ID.
pub fn build_queue_state(
    library: &Library,
    queue: &[TrackRef],
    position: Option<usize>,
) -> Option<QueueState> {
    if queue.is_empty() {
        return None;
    }
    let entries: Vec<QueueEntry> = queue
        .iter()
        .filter_map(|&(pl_idx, song_idx)| {
            let video_id = library.track(pl_idx, song_idx)?.video_id.clone()?;
            if video_id.is_empty() {
                return None;
            }
            let playlist_id = library.playlist(pl_idx).map(|p| p.playlist_id.clone());
            Some(QueueEntry {
                playlist_id,
                video_id,
            })
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    Some(QueueState { entries, position })
}

/// Outcome of attempting to resolve a saved [`QueueState`] against the
/// (progressively-loading) library.
pub enum RestoreOutcome {
    /// One or more referenced playlists haven't finished loading yet — call
    /// again once more songs have arrived.
    Pending,
    /// The saved queue couldn't be restored — a referenced playlist no
    /// longer exists, or none of its entries matched current library
    /// contents. Stop retrying.
    Abandoned,
    /// Resolved successfully.
    Ready {
        queue: Vec<TrackRef>,
        position: Option<usize>,
    },
}

/// Attempts to resolve `saved` against `library`. Call this again (with the
/// same `saved`) each time a new song batch is applied to `library`, until
/// it stops returning `Pending`.
pub fn try_restore(library: &Library, saved: &QueueState) -> RestoreOutcome {
    for entry in &saved.entries {
        let Some(pl_id) = entry.playlist_id.as_deref() else {
            continue;
        };
        let Some(pl_idx) = library.find_playlist_index(pl_id) else {
            return RestoreOutcome::Abandoned;
        };
        if !library.is_loaded(pl_idx) {
            return RestoreOutcome::Pending;
        }
    }

    let queue: Vec<TrackRef> = saved
        .entries
        .iter()
        .filter_map(|entry| {
            let pl_id = entry.playlist_id.as_deref()?;
            let pl_idx = library.find_playlist_index(pl_id)?;
            let song_idx = library.find_song_index(pl_idx, &entry.video_id)?;
            Some((pl_idx, song_idx))
        })
        .collect();

    if queue.is_empty() {
        return RestoreOutcome::Abandoned;
    }

    let position = saved.position.filter(|&p| p < queue.len()).or(Some(0));
    RestoreOutcome::Ready { queue, position }
}
