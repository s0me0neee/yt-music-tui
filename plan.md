---
  Tier 1 — Core usability (gaps that hurt daily use)
  
  1. (actually tier 1.5) Search — s opens a search bar, results show in the songs panel. Without this you're stuck browsing playlists only. ytmusicapi.search() is already
  wired.
  2. Like / unlike current song — L toggles. rate_song() is already in the API. Most common action while listening.

  ---
  Tier 2 — Library navigation
  
  1. Library views — get_liked_songs(), get_history(), get_library_albums() already exist. A tab bar (or 1/2/3 keys) to switch between Playlists / Liked
   / History / Albums.
  2. Album drill-down — press Enter on an album to see its tracks. get_album() returns them.
  3. Radio / "Up next" — r starts a radio from the current song using get_watch_playlist(video_id=...). Fills the queue automatically.

  ---
  Tier 3 — Polish

  1. Persistent queue — write queue to a JSON file on exit, restore on startup. Survives crashes.
  2. Lyrics panel — y fetches lyrics via get_lyrics() and shows them in the right panel, scrollable. Very popular in terminal players.
  3. Config file — ~/.config/yt-tui/config.toml: default volume, keybindings, auth path. Avoids hardcoded values.

  ---
  Tier 4 — Playback power features

  1. Download / offline cache — D on a song runs yt-dlp -x --audio-format opus -o
     ~/.cache/yt-tui/%(id)s.%(ext)s. On Cmd::Play, check cache first before resolving URL —
     instant start, works offline. Show ↓ indicator next to cached songs in the list.
     Progress shown in notification bar during download.

  2. Crossfade — buffer next song early via mpv audio-device switching or a second mpv
     instance. C key cycles 0 / 2s / 5s crossfade. Requires coordinating two mpv instances
     or using mpv's --blend-subtitles / lavfi crossfade filter. Store in config.

  3. Speed control — [ / ] adjusts playback rate 0.5×–2× via mpv set_property speed.
     Display current speed in the player bar extra row (replaces or alongside volume).

  4. Local history — every do_play() appends {video_id, title, artist, timestamp} to
     ~/.local/share/yt-tui/history.json (capped at 1000 entries, deduplicated by recency).
     Shown as a tab (key 4) alongside Playlists/Liked/Albums. No API call needed.

  5. Mouse support — ratatui has MouseEvent support. Enable with
     crossterm::event::EnableMouseCapture. Click playlist to select, click song to play,
     scroll wheel for j/k, click player bar to seek to position. Low cost since all state
     is already index-driven.

  ---
  Tier 5 — Auth improvements

  Context: ytmusicapi OAuth is broken upstream. Cookie/header auth (browser.json) works but expires ~1yr
  and requires a manual DevTools cURL copy-paste. Goal: eliminate that friction.

  1. Chrome CDP auto-auth — spawn the user's existing Chrome with --remote-debugging-port, user logs in
     normally (real browser, no bot detection), we intercept the first music.youtube.com/youtubei/ request
     via CDP WebSocket to extract cookies + headers, write browser.json, kill Chrome. Zero manual steps.
     New deps: ureq = "2" (sync HTTP for /json/version poll), tungstenite = "0.24" (CDP WebSocket).
     Falls back to existing cURL flow if Chrome not found.
     Flow in src/setup.rs:
       find_chrome() → spawn Chrome → poll /json/version → connect WebSocket →
       Network.enable → navigate music.youtube.com → await requestWillBeSent on youtubei/ →
       Network.getAllCookies → merge headers + cookie string → write browser.json → kill Chrome
     Edge cases: port 9222 in use (use port=0, parse actual port), 60s timeout, missing x-goog-authuser
     (default "0"), Chrome crash (wait() + fall back to manual).

  2. Session expiry warning — on startup, parse cookie expiry dates from browser.json and warn
     "session expires in N days" in the help bar before it goes stale.

  3. Upstream OAuth — if/when ytmusicapi fixes OAuth, switch to yup_oauth2 (already scaffolded in
     src/auth.rs with tokencache.json). Gives automatic silent token refresh with no user action.
