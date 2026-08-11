# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build          # compile
cargo run            # run the TUI
cargo check          # fast type-check without linking
cargo test           # run tests (offline — network tests are #[ignore]d)
cargo test <name>    # run a single test by name

cargo test -p lrclib -- --ignored   # the live lrclib.net API tests
cargo test -p ytm-core translate -- --ignored   # the live translation tests
```

### Linking libmpv

libmpv is linked, not spawned, so the build needs the *library* — the mpv player on its own
is not enough. `ytm-core/build.rs` finds it in one of two ways:

- **`LIBMPV_DIR`**, if set: the directory holding the import library. Checked first, and on
  Windows it is the only option — there is no pkg-config, and the mpv player builds ship no
  import library at all. Unpack the **mpv-dev** package (`libmpv-2.dll`, `libmpv.dll.a`,
  `include/mpv/`) from [shinchiro/mpv-winbuild-cmake][mpv-win] and point `LIBMPV_DIR` at it;
  the same directory has to be on `PATH` at runtime so `libmpv-2.dll` can be loaded.
  A missing DLL shows up as a bare `STATUS_DLL_NOT_FOUND` exit, with no message.
- **pkg-config** otherwise, which is what picks up Homebrew's libmpv on macOS
  (`brew install mpv`) and the distro package on Linux (`libmpv-dev`).

[mpv-win]: https://github.com/shinchiro/mpv-winbuild-cmake/releases

## Credential Setup

Auth uses cookie-based auth via `browser.json` (ytmusicapi style — no OAuth).

On first run (`browser.json` absent), the app runs an interactive setup that shells out to
`yt-dlp --cookies-from-browser` to extract YouTube cookies from a local browser profile.
The browser that worked is written to `config.toml` as `auth.cookie-browser`, so a later
expiry renews itself by re-running yt-dlp rather than prompting — set `auth.auto-reauth =
false` to always be asked. `Session::reauth` reports which happened via `Reauth`, and an
automatic renewal lets `main.rs` carry on instead of asking for a restart.
Config lives in `~/.config/yt-music-tui/` (`browser.json`, `queue.json`, `settings.json`,
`lyrics.json`, `config.toml`, `app.log`). Everything but `config.toml` is written by the
app; `config.toml` is the hand-edited one, read once at startup by `config.rs`.

## Architecture

A Cargo workspace with three members. `ytm-core` is UI-agnostic so the engine can be driven
by something other than the ratatui frontend.

```
lrclib/     lyrics.net API client + LRC format parser (no app knowledge)
ytm-core/   session/auth, library, playback, queue, lyrics policy, persistence
tui/        the ratatui frontend — single `ytm` binary
```

### `lrclib/`

- **`api.rs`** — `LrcLib` client over `reqwest` (async). `get`, `get_by_id`, `search`,
  `search_by_meta`. Timeouts are set on the client so a hung server can't stall the UI.
- **`lrc.rs`** — `parse_lrc` turns raw `[mm:ss.xx]line` text into sorted `LyricLine`s;
  `active_index` answers "which line is playing at time *t*" via binary search;
  `next_boundary` drives the redraw schedule. Handles multi-timestamp lines, metadata tags,
  and `[offset:]`.

### `ytm-core/`

- **`session.rs`** — `Session`: cookie extraction via yt-dlp, `browser.json` I/O, config paths.
- **`library.rs`** — `Library`, `Track`, `Playlist`. `spawn_library_fetch` streams each
  playlist's tracks back over an `mpsc` channel as they arrive.
- **`playback.rs`** — `AudioEngine` owns an in-process **libmpv** instance (`libmpv2`) on its
  own thread, plus an `Arc<Mutex<AudioState>>` snapshot. `AudioState::elapsed` is the live
  playback position, fed by an mpv `time-pos` property observer.
- **`player.rs`** — `Player`: queue, play modes, volume/mute, song-end advance.
- **`lyrics.rs`** — policy over `lrclib`. `LyricsService::best_for` layers `/get` (exact, has
  duration, returns one) over `/search` (returns many, ignores duration), preferring synced
  over plain; `rank` does duration-proximity scoring client-side. `spawn_best`/`spawn_choices`
  do the fetching in background tokio tasks.
- **`translate.rs`** — policy over the `rust-translate` crate, which wraps Google's public
  `translate_a/single` endpoint. Two of the crate's flaws are handled here, and both are
  silent if they aren't: it interpolates the text straight into the URL (a lyric with `&`,
  `#` or `%` in it comes back mangled — hence `percent_encode`), and it returns only the
  *first segment* of the reply, dropping everything past the first full stop. So a reply is
  only used when it can be proved complete — one line back per line sent. `translate_lines`
  probes with the first batch: whole ⇒ the rest of the song goes the same way (Japanese
  sources come back in one piece, so a song is a couple of requests); short ⇒ re-fetched a
  sentence at a time via `sentence_pieces`, which the endpoint cannot segment further.
  Blank and repeated lines are never sent, so a chorus costs one request. Returns one entry
  per input line, empty where nothing could be translated.
- **`persistence.rs`** — `queue.json`, `settings.json` (volume), `lyrics.json` (manual lyric
  choices, keyed by video ID).
- **`config.rs`** — `config.toml`, the hand-edited settings, read once at startup. Every
  value has a working default and a missing or malformed file falls back to those with a
  log warning, so a typo can never stop playback. `lyrics.offset` shifts every lyric line
  against the record's timings (negative = early, positive = late); it is applied by
  shifting the clock handed to `active_index`/`next_boundary`, never the cached records.
  `lyrics.translate-to` is the language `i` translates into (`"zh"`, `"fr"`, …); it is
  checked against the endpoint's own list at startup and cleared if unknown, because an
  unknown code is answered with the input unchanged rather than an error. Empty (the
  default) means nothing is ever sent to a translation service.
  `auth.auto-reauth` / `auth.cookie-browser` drive silent re-authentication.
  `remember_cookie_browser` writes back through `toml_edit`, so the user's comments and
  formatting survive — this is the one file the app both reads and writes.

### `tui/`

- **`main.rs`** — logging, ctrlc handler, session setup, builds the tokio runtime, kicks off
  the background library fetch, then hands off to `App::run()`.
- **`app.rs`** — all TUI state and rendering.
  - **Chrome**: no bordered panels. Each panel is a `section()` — an uppercase
    header, an optional right-aligned status, and a rule — separated by
    whitespace. Only *overlays* (the `?` keymap, the lyrics picker) keep a
    border, because they float above other content.
  - **Colour**: every style is a named constant in the `theme` module, and every
    value is an **ANSI named colour** (never `Rgb`/`Indexed`) so the user's own
    terminal palette drives the look. One role per colour — Cyan is focus and
    keys, Green is playing, Yellow warns, Red errors, Magenta is translated
    text, Gray/DarkGray are content metadata and chrome respectively.
  - **Focus** is carried by the section header's colour and by the selection
    style (`SELECTED` vs `SELECTED_BLUR`), since there is no border to tint.
  - **Filter**: `/` opens inline filter mode; `Enter` confirms (keeps filter), `Esc` clears.
  - **Queue**: `a` appends, `d` removes, `o` toggles queue/songs view, `p`/`n` skip.
  - **Prefetch**: j/k in Songs warms the CDN URL for the selected + next song.
  - **Lyrics**: `y` replaces the right column with a synced-lyrics panel that auto-centres the
    active line; `c` opens a modal to pick a different lrclib record; `r` retries a failed
    fetch. Results are cached per video ID and never re-fetched, so toggling is free.
    The picker collapses records carrying identical lyrics — two thirds of what lrclib
    returns for a popular track — and its left column marks `IN USE` (what the panel is
    showing) and `AUTO` (the record automatic matching resolved to). The record on screen
    is guaranteed a row, since it is often the `/get` hit the search ladder never sees.
  - **Translation**: `i` weaves a translation in under each lyric, in the language
    `lyrics.translate-to` names. Translated rows are Magenta and italic and carry the same
    `lyric` index as their original (`lyric_rows`), so a pair highlights, wraps and centres
    as the one line it is; the active line's own highlight deliberately stays on the words
    alone. A line whose translation is blank, or identical to the original, gets no row at
    all. Cached per **lrclib record id**, not per video — a translation belongs to the
    words, so `c` gets one of its own and two tracks on the same record share one. `i`
    twice is the retry after a failure.
  - **Wrapping** (`wrap_n_lines`) measures display *cells*, not `char`s: a CJK lyric is two
    cells per character and would otherwise run to twice the panel width and be clipped.

### Event loop cadence

`event::poll` normally blocks 200 ms, but `App::poll_timeout` shortens it to wake just after
the next lyric boundary while synced lyrics are following playback (clamped to 33–200 ms).
Lyrics mode off ⇒ unchanged 200 ms, so there is no idle cost.

### Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate down / up (scroll lyrics in lyrics mode) |
| `h` / `l` | Switch panels (Playlists ↔ Songs) |
| `Enter` | Open playlist / play song / play from queue |
| `/` | Enter filter mode (songs or queue panel) |
| `Esc` | Re-centre lyrics → close lyrics → clear filter → back to playlists → quit |
| `Space` | Pause / resume |
| `p` / `n` | Previous / next in queue |
| `←` / `→` | Seek −5s / +5s |
| `↑` / `↓` | Volume +5 / −5 |
| `m` | Mute / unmute |
| `t` | Cycle play mode (Cycle → Single → Shuffle) |
| `a` | Append selected song to queue |
| `d` | Remove selected queue entry |
| `o` | Toggle queue / songs view |
| `y` | Toggle lyrics panel |
| `c` | (in lyrics mode) Choose a different lrclib record |
| `i` | (in lyrics mode) Toggle the translation under each line |
| `r` | (in lyrics mode) Retry a failed lyrics fetch |
| `?` | Full keymap overlay (any key closes it) |
| `q` / `Ctrl+C` | Quit |

The one-row help bar shows only as many hints as fit, dropping whole hints from
the end (`fit_hints`) — the full list needs ~143 columns, so `?` is where the
complete keymap lives.

Note: raw mode clears `ISIG`, so `Ctrl+C` never becomes a signal — `app.rs` matches it as a
key event. The `ctrlc` handler in `main.rs` only covers SIGTERM/SIGHUP.

### Data flow

```
main.rs
  └─ Session::build_client()
  └─ library::get_playlists()  ──► App::new(library, saved_queue, songs_rx, rt_handle)
  └─ library::spawn_library_fetch()   └─ App::run()
       (tokio tasks → mpsc)                └─ event_loop
                                                ├─ drain_song_channel / drain_lyrics
                                                │  / drain_translations
                                                ├─ render
                                                └─ Player → AudioEngine → libmpv
```

## Dependency notes

Cargo resolves versions more loosely than expected in edition 2024. When adding or updating
dependencies, pin exact versions in `Cargo.toml` and run `cargo tree` to confirm the resolved
version before writing code against a specific API.

**reqwest is pinned to 0.12** across the workspace to match what `ytmusicapi` already pulls in.
Do not bump `lrclib` to 0.13 casually — it would add a second HTTP stack plus an `aws-lc-sys`
C build (needs cc + cmake). Verify with `cargo tree -i aws-lc-sys` (should report *not found*).
Note 0.12 has no `query` feature; `.query()` is unconditional there.

`rust-translate 0.1.3` brings no HTTP stack of its own — its `reqwest 0.12.4` resolves to the
0.12 already in the graph. It does ask for tokio's `full` feature set, which unifies across
the workspace; that costs build time, nothing at runtime. Its API is three free async
functions and a language list; see `ytm-core/src/translate.rs` for what it gets wrong.

Key deps: `ratatui 0.30`, `ytmusicapi 0.4.2`, `libmpv2 6`, `reqwest 0.12`, `thiserror 1`,
`rust-translate 0.1.3`, `ctrlc 3` (termination feature), `simplelog 0.12`, `rand 0.8`,
`throbber-widgets-tui 0.11`.
