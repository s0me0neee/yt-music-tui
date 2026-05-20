# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build          # compile
cargo run            # run the TUI
cargo check          # fast type-check without linking
cargo test           # run tests
cargo test <name>    # run a single test by name
```

## Credential Setup

Auth uses cookie-based auth via `browser.json` (ytmusicapi style — no OAuth).

On first run (`browser.json` absent), the app runs an interactive setup:
- Open `music.youtube.com` in Chrome, open DevTools → Network
- Click any request → right-click → "Copy as cURL (bash)"
- Paste into the terminal and press Enter (no Ctrl+D needed — the app auto-detects the end of the curl command)
- `browser.json` is written and the app continues

`src/auth.rs` contains a scaffolded OAuth 2.0 / yup_oauth2 flow that is not currently wired up (ytmusicapi OAuth is broken upstream).

## Architecture

A full Rust TUI (ratatui + crossterm) that streams YouTube Music via mpv.

### Source files

**`src/main.rs`**
- Initialises logging to `app.log`
- Installs a `ctrlc` handler (`SIGINT/SIGTERM/SIGHUP`) that sets `QUIT: AtomicBool`
- Checks for `browser.json`; if missing or expired, runs `setup::run_setup()`
- Fetches all playlists + their tracks in parallel (scoped threads) then hands off to `App::run()`

**`src/api.rs`**
- `get_playlists(yt)` — returns `Err(ApiError::SessionExpired)` when the YTM API returns a non-array (expired cookie); callers in `main.rs` catch this and re-run setup
- `get_songs(yt, playlist_id)` — returns all tracks for a playlist

**`src/setup.rs`**
- `run_setup(path)` — reads cURL input line-by-line, stops automatically when the trailing-backslash continuation chain ends (no Ctrl+D required), writes `browser.json`

**`src/audio.rs`**
- `AudioEngine` — owns the audio thread and an `Arc<Mutex<AudioState>>`
- `Drop` kills mpv and joins the audio thread
- Audio thread spawns mpv with `--input-ipc-server=/tmp/yt-tui-{pid}.sock`
- Background URL resolution: `Cmd::Prefetch(id)` pre-resolves CDN URLs via yt-dlp (capped at 2 concurrent threads); `Cmd::Play(id)` uses the cache if warm, otherwise sends the YouTube URL to mpv immediately and races a background resolve — if it arrives while mpv is still loading, it upgrades to the direct CDN URL (`pending_resolve`)
- `pub static QUIT: AtomicBool` — checked in the audio loop; killed cleanly on signal
- IPC socket file cleaned up on every exit path

**`src/app.rs`**
- `App` — full TUI state: playlists, songs, queue, playback, filter
- Event loop polls `QUIT` flag and `event::poll(200ms)`
- **Filter**: `/` opens inline filter mode; typing narrows songs or queue by title/artist in real time; `Enter` confirms (keeps filter), `Esc` clears; filter is shown in the panel title as `/query█` while typing
- **Queue**: `a` appends, `d` removes (switching audio to next if current entry removed), `o` toggles queue/songs view, `p`/`n` skip prev/next
- **Prefetch**: j/k navigation in Songs fires `Cmd::Prefetch` for the selected + next song so the CDN URL is warm before Enter is pressed

**`src/auth.rs`**
- Unused scaffolding for yup_oauth2 OAuth flow (for when ytmusicapi upstream fixes OAuth)

**`src/ytm.rs`**
- Legacy pyo3 bridge (unused in current flow — ytmusicapi Rust crate is used instead)

### Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate down / up |
| `h` / `l` | Switch panels (Playlists ↔ Songs) |
| `Enter` | Open playlist / play song / play from queue |
| `/` | Enter filter mode (songs or queue panel) |
| `Esc` | Clear filter → back to playlists → quit |
| `Space` | Pause / resume |
| `p` / `n` | Previous / next in queue |
| `←` / `→` | Seek −5s / +5s |
| `↑` / `↓` | Volume +5 / −5 |
| `m` | Cycle play mode (Cycle → Single → Shuffle) |
| `a` | Append selected song to queue |
| `d` | Remove selected queue entry |
| `o` | Toggle queue / songs view |
| `q` | Quit |

### Data flow

```
main.rs
  └─ YTMusic::authenticated("browser.json")
  └─ api::get_playlists()  ──► App::new(playlists, all_songs)
  └─ api::get_songs() ×N          └─ App::run()
       (parallel threads)               └─ event_loop → AudioEngine::send(Cmd)
                                                             └─ audio thread → mpv IPC
```

## Dependency notes

Cargo resolves versions more loosely than expected in edition 2024. When adding or updating dependencies, pin exact versions in `Cargo.toml` and run `cargo tree` to confirm the resolved version before writing code against a specific API.

Key deps: `ratatui 0.30`, `ytmusicapi` (path dep `../ytmusicapi`), `thiserror 1`, `ctrlc 3` (termination feature), `simplelog 0.12`, `rand 0.8`, `throbber-widgets-tui 0.11`.
