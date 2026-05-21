use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── Directory ─────────────────────────────────────────────────────────────────

/// App config directory.
/// - macOS : `~/.config/yt-music-tui/`  (XDG-style, not ~/Library)
/// - Other : `{dirs::config_dir()}/yt-music-tui/`
pub fn app_config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config");

    #[cfg(not(target_os = "macos"))]
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));

    base.join("yt-music-tui")
}

/// Creates the config directory if it doesn't exist, then returns its path.
pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = app_config_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ── Well-known paths ──────────────────────────────────────────────────────────

pub fn browser_json_path() -> PathBuf { app_config_dir().join("browser.json") }
pub fn browser_file_path() -> PathBuf { app_config_dir().join(".yt-tui-browser") }
pub fn queue_path()        -> PathBuf { app_config_dir().join("queue.json")   }
pub fn config_toml_path()  -> PathBuf { app_config_dir().join("config.toml")  }

// ── config.toml stub ──────────────────────────────────────────────────────────

/// Creates `config.toml` with a comment header if it doesn't exist yet.
pub fn ensure_config_toml() -> Result<()> {
    let path = config_toml_path();
    if !path.exists() {
        std::fs::write(&path, "# yt-music-tui configuration\n")?;
    }
    Ok(())
}

// ── Queue persistence ─────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct QueueEntry {
    pub playlist_id: Option<String>,
    pub video_id:    String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct QueueState {
    /// Ordered queue entries — each carries its own playlist ID so the queue
    /// can span multiple playlists.
    pub entries:  Vec<QueueEntry>,
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
