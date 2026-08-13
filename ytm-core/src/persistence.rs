//! Queue persistence: serialising the live queue to `queue.json` on exit and
//! resolving it back to `(playlist_idx, song_idx)` pairs on the next launch.
//! Also holds user settings persisted to `settings.json`.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::Library;
use crate::player::TrackRef;
use crate::session::{
    lyrics_path, queue_path, settings_path, translations_path, write_private,
};

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

/// Every one of these goes through [`write_private`]: they are written on the
/// way out, when a `Ctrl+C` landing mid-write is at its most likely, and a
/// half-written file reads back as no file at all — the queue, the volume or a
/// paid-for translation silently gone. A rename is atomic, so the worst case
/// becomes the previous contents rather than none.
pub fn save_queue(state: &QueueState) -> Result<()> {
    write_private(&queue_path(), &serde_json::to_string_pretty(state)?)
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
    write_private(&settings_path(), &serde_json::to_string_pretty(settings)?)
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
    write_private(&lyrics_path(), &serde_json::to_string_pretty(overrides)?)
}

pub fn load_lyrics_overrides() -> LyricsOverrides {
    std::fs::read_to_string(lyrics_path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

// ── translations ─────────────────────────────────────────────────────────────

/// AI translations kept between sessions, so a song is paid for once.
///
/// Only the AI ones: the free endpoint costs nothing but a wait, and a
/// translation kept for ever is a translation that can never improve. `i` asks
/// for a fresh one each session; `I` reuses what it already bought.
///
/// Keyed by lrclib record id — a translation belongs to the words, so two
/// tracks on the same record share one and `c` gets its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Translations {
    /// Record id as a string, since JSON object keys are strings.
    #[serde(default)]
    entries: std::collections::HashMap<String, CachedTranslation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTranslation {
    /// What it was translated into; `lyrics.translate-to` can change between
    /// sessions, and last week's language is no use.
    pub language: String,
    /// The model that produced it, empty for the free endpoint.
    #[serde(default)]
    pub model: String,
    /// Unix seconds, for evicting the oldest once the file is at its cap.
    #[serde(default)]
    pub saved_at: u64,
    /// One entry per lyric line of the record.
    pub lines: Vec<String>,
}

/// Records kept on disk. A few kilobytes each; past this the oldest goes.
const MAX_SAVED_TRANSLATIONS: usize = 1000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Translations {
    /// What was bought for `record_id`, if it is in the language now
    /// configured.
    #[must_use]
    pub fn get(&self, record_id: u64, language: &str) -> Option<&[String]> {
        let entry = self.entries.get(&record_id.to_string())?;
        (entry.language == language).then_some(entry.lines.as_slice())
    }

    pub fn set(&mut self, record_id: u64, language: &str, model: &str, lines: Vec<String>) {
        self.entries.insert(
            record_id.to_string(),
            CachedTranslation {
                language: language.to_string(),
                model: model.to_string(),
                saved_at: now_secs(),
                lines,
            },
        );
        self.evict();
    }

    /// Forgets one, so the next `I` buys another — what `r` does with a
    /// translation you don't like. `true` if there was one to forget.
    pub fn remove(&mut self, record_id: u64) -> bool {
        self.entries.remove(&record_id.to_string()).is_some()
    }

    /// Drops the oldest until the file is back under the cap.
    fn evict(&mut self) {
        while self.entries.len() > MAX_SAVED_TRANSLATIONS {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(id, e)| (e.saved_at, (*id).clone()))
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            self.entries.remove(&oldest);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn save_translations(translations: &Translations) -> Result<()> {
    write_private(
        &translations_path(),
        &serde_json::to_string_pretty(translations)?,
    )
}

pub fn load_translations() -> Translations {
    std::fs::read_to_string(translations_path())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Playlist, Track};

    // ── restoring a saved queue ──────────────────────────────────────────────

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

    fn saved() -> QueueState {
        QueueState {
            entries: vec![QueueEntry {
                playlist_id: Some("PL1".to_string()),
                video_id: "aaa".to_string(),
            }],
            position: Some(0),
        }
    }

    #[test]
    fn a_queue_waits_for_the_playlist_it_names() {
        let lib = library();
        assert!(matches!(
            try_restore(&lib, &saved()),
            RestoreOutcome::Pending
        ));
    }

    #[test]
    fn a_playlist_that_failed_to_load_does_not_discard_the_queue() {
        // The failure this pairs with: marking a failed fetch "loaded" made
        // the saved video look permanently absent, so a queue the user had
        // built over weeks was dropped because one request timed out. Pending
        // keeps it, and `r` re-fetching is what finally resolves it.
        let mut lib = library();
        lib.apply_song_batch(0, None);
        assert!(matches!(
            try_restore(&lib, &saved()),
            RestoreOutcome::Pending
        ));

        lib.apply_song_batch(0, Some(vec![track("aaa")]));
        let RestoreOutcome::Ready { queue, position } = try_restore(&lib, &saved()) else {
            panic!("the retry should have restored it");
        };
        assert_eq!(queue, [(0, 0)]);
        assert_eq!(position, Some(0));
    }

    #[test]
    fn a_playlist_that_is_loaded_and_really_empty_abandons_the_queue() {
        // No amount of waiting brings the track back — the user deleted it.
        let mut lib = library();
        lib.apply_song_batch(0, Some(Vec::new()));
        assert!(matches!(
            try_restore(&lib, &saved()),
            RestoreOutcome::Abandoned
        ));
    }

    #[test]
    fn a_queue_naming_a_playlist_that_is_gone_is_abandoned() {
        let lib = Library::new(Vec::new());
        assert!(matches!(
            try_restore(&lib, &saved()),
            RestoreOutcome::Abandoned
        ));
    }

    // ── translations ─────────────────────────────────────────────────────────

    fn lines() -> Vec<String> {
        vec!["\u{4e00}".to_string(), "\u{4e8c}".to_string()]
    }

    #[test]
    fn what_was_bought_comes_back() {
        let mut saved = Translations::default();
        saved.set(7, "zh", "claude-haiku-4-5", lines());
        assert_eq!(saved.get(7, "zh").unwrap(), lines());
        assert!(saved.get(8, "zh").is_none());
    }

    #[test]
    fn a_translation_comes_back_in_the_language_it_was_stored_under() {
        let mut saved = Translations::default();
        saved.set(7, "zh", "claude-haiku-4-5", lines());
        // `lyrics.translate-to` changed between sessions.
        assert!(saved.get(7, "fr").is_none());
    }

    #[test]
    fn r_forgets_one_and_leaves_the_rest() {
        let mut saved = Translations::default();
        saved.set(7, "zh", "claude-haiku-4-5", lines());
        saved.set(8, "zh", "claude-haiku-4-5", lines());
        assert!(saved.remove(7));
        assert!(!saved.remove(7));
        assert!(saved.get(7, "zh").is_none());
        assert!(saved.get(8, "zh").is_some());
    }

    #[test]
    fn the_file_stops_growing_at_the_cap() {
        let mut saved = Translations::default();
        for id in 0..MAX_SAVED_TRANSLATIONS as u64 + 10 {
            // Set directly rather than through `set`, which stamps the clock.
            saved.entries.insert(
                id.to_string(),
                CachedTranslation {
                    language: "zh".to_string(),
                    model: "claude-haiku-4-5".to_string(),
                    saved_at: id,
                    lines: lines(),
                },
            );
            saved.evict();
        }
        assert_eq!(saved.len(), MAX_SAVED_TRANSLATIONS);
        assert!(saved.get(0, "zh").is_none());
        assert!(saved.get(MAX_SAVED_TRANSLATIONS as u64 + 9, "zh").is_some());
    }

    #[test]
    fn a_translations_file_survives_a_round_trip() {
        let mut saved = Translations::default();
        saved.set(7, "zh", "claude-haiku-4-5", lines());
        let json = serde_json::to_string_pretty(&saved).expect("serialised");
        let back: Translations = serde_json::from_str(&json).expect("parsed");
        assert_eq!(back.get(7, "zh").unwrap(), lines());
    }
}
