---
# yt-music-tui — feature plan
Last updated: 2026-07-01

Legend: ✅ done  🔄 in progress  ❌ not started

---

## Completed

- ✅ libmpv2 embedding — no longer spawning the mpv binary, lower latency, cleaner cleanup
- ✅ Persistent queue — queue.json written on exit, restored on startup (without auto-play)
- ✅ Prefetch / hot CDN URL — j/k navigation fires `Cmd::Prefetch`; concurrent resolution capped at 2
- ✅ Cookie-refresh gating — yt-dlp cookie extraction skipped when cookies are fresh enough
- ✅ Mouse support — scroll wheel maps to j/k, click selects panels
- ✅ Config dir — `~/.config/yt-music-tui/` with `config.toml` stub, `queue.json`, `browser.json`
- ✅ Cross-playlist queue — queue entries track (playlist_idx, song_idx) pairs
- ✅ Hotpath profiling — `#[hotpath::measure]` on `resolve_url`, gated behind `--features hotpath`

---

## In progress

🔄 **rustypipe URL resolution** (current branch: `rustypipe`)
- Goal: replace the `yt-dlp --get-url` subprocess in `resolve_url()` with the rustypipe crate
- Why: removes the yt-dlp runtime dependency for stream resolution; faster, no subprocess overhead
- Files: `src/audio.rs:75` (`resolve_url`), `rustypipe_cache.json`, `rustypipe_reports/`
- Blocking issue to resolve: rustypipe crate is not yet in `Cargo.toml`; verify API stability before pinning

---

## Tier 1 — Core usability

❌ **Search** (`s` key)
- Open a search bar at the bottom; results populate the songs panel
- `ytmusicapi::search()` is already available on the `YtMusic` client
- Need a new `View::Search` variant in `App`; run search in a background thread to avoid blocking the TUI

❌ **Like / unlike current song** (`L` key)
- `rate_song(video_id, "LIKE" | "INDIFFERENT")` is in the ytmusicapi crate
- Show a heart indicator in the player bar next to the title
- Need to track `liked: bool` on the currently playing song

---

## Tier 2 — Library navigation

❌ **Library views** (keys `1`/`2`/`3`/`4` or a tab bar)
- Tab 1: Playlists (current default)
- Tab 2: Liked Songs — `get_liked_songs()`
- Tab 3: History — `get_history()`
- Tab 4: Albums — `get_library_albums()`
- Add a `LibraryTab` enum; render a tab header row at the top of the playlist panel

❌ **Album drill-down**
- Press `Enter` on an album entry to load tracks via `get_album(browse_id)`
- Push a new songs view; `Backspace` pops back to the album list

❌ **Radio / "Up next"** (`r` key)
- `get_watch_playlist(video_id=current)` returns a "Up next" list
- Append results to the queue automatically
- Show "Radio seeded from <title>" in the notification bar

---

## Tier 3 — Polish

✅ **Lyrics panel** (`y` key)
- Sourced from **lrclib.net**, not ytmusicapi: `get_lyrics(browse_id)` does not exist in
  ytmusicapi 0.4.2. The `lrclib` crate was vendored into the workspace as a member.
- `y` replaces the right column with synced lyrics that auto-centre the active line, driven by
  `AudioState::elapsed`; falls back to unsynced when no synced record exists
- `c` opens a modal to pick a different lrclib record; the choice persists in `lyrics.json`
- Scrollable with `j`/`k`; cached per video_id so toggling never re-fetches

❌ **Config file** (`~/.config/yt-music-tui/config.toml`)
- The file is created but not read yet
- Values to support: `default_volume`, `keybindings` (map), `browser` (chrome/firefox/brave), `auth_path`
- Parse with `toml` crate on startup; merge over hardcoded defaults

❌ **Session expiry warning**
- On startup, parse the `expires` fields in `browser.json` cookies
- If any cookie expires within 7 days, show a warning in the help bar: "session expires in N days"

---

## Tier 4 — Playback power features

❌ **Speed control** (`[` / `]` keys)
- Adjust `speed` property on the libmpv2 handle: 0.5× → 0.75× → 1.0× → 1.25× → 1.5× → 2.0×
- Display current speed in the player bar (only when ≠ 1.0× to avoid clutter)
- Persist speed setting across sessions in `config.toml`

❌ **Local playback history** (automatic, no key)
- On every `do_play()`, append `{video_id, title, artist, timestamp}` to
  `~/.local/share/yt-music-tui/history.json`
- Cap at 1000 entries; deduplicate by recency (most-recent occurrence wins)
- Show as Library tab 3 or 4; no API call needed, instant load

❌ **Download / offline cache** (`D` key)
- Run `yt-dlp -x --audio-format opus -o ~/.cache/yt-music-tui/<id>.opus`
- Check cache in `resolve_url()` before calling yt-dlp or rustypipe
- Show ↓ indicator next to cached songs in the list
- Progress shown in the notification bar during download (background thread)

❌ **Crossfade** (`C` key cycles: off → 2s → 5s)
- Pre-load next song in a second libmpv2 instance; fade volume of the first out
- Store crossfade duration in `config.toml`
- Complex: requires careful state management for two mpv handles; implement after speed control

---

## Tier 5 — Auth improvements

❌ **Chrome CDP auto-auth** (fallback in `src/setup.rs`)
- Spawn Chrome with `--remote-debugging-port=9222`, navigate to `music.youtube.com`
- Connect to CDP WebSocket, enable `Network`, intercept first `youtubei/` request
- Extract cookies + headers, write `browser.json`, kill Chrome
- Zero manual steps; fall back to existing yt-dlp / cURL flow if Chrome not found
- New deps: `ureq = "2"` (HTTP poll of `/json/version`), `tungstenite = "0.24"` (CDP WebSocket)
- Edge cases: port 9222 in use, 60s timeout, Chrome crash

❌ **Upstream OAuth**
- `src/auth.rs` already scaffolds `yup_oauth2` with `tokencache.json`
- Wire up once `ytmusicapi` upstream fixes their OAuth flow
- Gives automatic silent token refresh with no user action

---

## Backlog / nice-to-have

- Playlist management from the TUI (create playlist, add/remove songs)
- macOS/Linux native notifications on track change (`notify-rust` crate)
- Visualizer bar in the player area (requires PCM data from mpv's audio output)
- Vim-style `gg`/`G` jump-to-top/bottom in any list
- `?` opens a keybinding help overlay
