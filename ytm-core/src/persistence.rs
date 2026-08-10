//! Queue persistence: serialising the live queue to `queue.json` on exit and
//! resolving it back to `(playlist_idx, song_idx)` pairs on the next launch.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::Library;
use crate::player::TrackRef;
use crate::session::queue_path;

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
