---
# yt-music-tui — feature plan
Last updated: 2026-08-20

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

~~**Chrome CDP auto-auth**~~ — superseded by the Tauri webview plan below (same
idea — drive a real Google login and lift cookies out of it — but a bundled
webview needs no external Chrome, no debug port, no WebSocket protocol client.

~~**Upstream OAuth**~~ — dead end, confirmed: YouTube Music rejects Bearer
tokens from user-created OAuth clients
([sigma67/ytmusicapi#813](https://github.com/sigma67/ytmusicapi/issues/813)),
and the `ytmusicapi-rs` sibling project's README documents hitting this
directly. Not worth wiring up `yup_oauth2` against — there is no token this
flow can produce that the API will accept. Cookie auth stays the only real
path; the plan below changes *how* the cookie is obtained, not what it is.

❌ **Tauri frontend + webview login** (replaces yt-dlp cookie extraction, for
GUI users)
- Motivation: today's setup (`ytm-core/src/session.rs`) gets cookies by
  reaching into an *existing* browser's cookie store via
  `yt-dlp --cookies-from-browser` — which is where the current pain lives
  (Chrome 127+'s App-Bound Encryption defeats yt-dlp outright, the browser
  has to be closed first, and the manual cURL-paste fallback exists only
  because the automatic path fails often enough to need one). A Tauri app
  bundles its own webview (wry — WebView2 / WKWebView / WebKitGTK), so it can
  render an actual Google login page in-process and read the session back out
  of *that* — no external browser, no encrypted store to crack open, no "is
  Chrome closed" prompt.
- Architecture: `ytm-core` is already UI-agnostic (the engine `tui/` drives
  today), so this is a fourth workspace member — a `gui/` (or `tauri-app/`)
  crate depending on `ytm-core` the same way `tui/` does, not a rewrite of
  anything under `ytm-core/`.
- Auth flow:
  1. Open a Tauri `WebviewWindow` at `music.youtube.com` (redirects to Google
     login if signed out). The user logs in exactly as in a normal browser —
     passkeys, 2FA, "select account" all just work, since it's a real engine.
  2. Detect completion via a navigation hook (`on_navigation` /
     `on_page_load`) firing for a `music.youtube.com/*` URL post-login, not a
     fixed timer.
  3. Read the session back out via the webview's cookie API (Tauri
     `webview.cookies()`, wrapping the `cookie` crate) and filter to the
     `youtube.com`/`google.com` cookies innertube needs — the same
     `is_youtube_domain` filter `session.rs` already applies to yt-dlp's
     Netscape-format output.
  4. Add `Session::setup_with_webview_cookies(cookie_header: String)` to
     `ytm-core` — a new *adapter* alongside `setup_with_browser` /
     `setup_with_curl` that reuses `build_default_headers` and
     `write_private` verbatim. `browser.json`'s shape doesn't change, so a
     session started in the Tauri app is a session the TUI can already read,
     and vice versa — no migration, no new file format.
  5. Silent refresh: replace `refresh_cookies()`'s yt-dlp call, for GUI users
     only, with the same webview pulled again (hidden/offscreen) before the
     existing 6h `REFRESH_AFTER` window closes. TUI-only users keep the
     yt-dlp path, since a terminal has no webview to reuse.
- Suggested milestones:
  1. Throwaway spike: bare Tauri window, log in, dump `webview.cookies()` to
     stdout — confirms the cookie-read API actually returns `SAPISID` et al.
     on this machine (WKWebView/macOS first) before anything is built on it.
  2. `Session::setup_with_webview_cookies` in `ytm-core` (pure adapter, no
     Tauri dependency inside `ytm-core` itself — the crate stays UI-agnostic).
  3. Minimal `gui/` crate: a "Sign in" button driving the flow above, calling
     into `ytm-core` on success. No player UI yet.
  4. Background silent-refresh task using the same hidden-webview trick.
  5. Decide the GUI's actual scope (full player replacing the TUI, runs
     alongside it, or an auth-only helper that just produces `browser.json`
     for the TUI to consume) — worth settling before building past the spike.
- Open risks:
  - The cookie-read API's maturity differs by platform backend (WebView2 /
    WKWebView / WebKitGTK) — same caveat flagged for raw `wry` earlier, now
    one layer down inside Tauri. Milestone 1 exists specifically to find this
    out early.
  - An embedded webview may draw extra Google anti-automation friction
    (captcha, "this browser may not be secure") that a real Chrome/Firefox
    profile doesn't — untested, could rule the approach out on one platform.
  - "Login finished" is a heuristic (navigation reaching `music.youtube.com`),
    and Google's login flow is multi-page (email → password → 2FA → "stay
    signed in" interstitial) — needs to be robust against that sequence
    rather than firing on the first navigation past the email screen.

---

## Backlog / nice-to-have

- Playlist management from the TUI (create playlist, add/remove songs)
- macOS/Linux native notifications on track change (`notify-rust` crate)
- Visualizer bar in the player area (requires PCM data from mpv's audio output)
- Vim-style `gg`/`G` jump-to-top/bottom in any list
- `?` opens a keybinding help overlay
