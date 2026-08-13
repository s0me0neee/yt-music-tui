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
# the AI one takes ANTHROPIC_API_KEY or DEEPSEEK_API_KEY, whichever is set
cargo test -p ytm-core mpris -- --ignored       # needs a session D-Bus
```

While the app is running, `playerctl -p ytm status|metadata|play-pause` talks to it, and
`dbus-monitor "type='signal',path='/org/mpris/MediaPlayer2'"` shows what it emits.

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
false` to always be asked. `Session::reauth` returns with a working session either way —
silently from the browser on record, or through the prompts — so every caller carries
straight on and nothing ever ends with "run the app again"; `Reauth` says which happened,
for the log and the wording. `main.rs` is a loop around one `start()`: an expired session
reaches the TUI as an account with no playlists, so `App::run` returns `Exit::Reauth`
*before* `ratatui::init` — the renewal happens, everything is built again in the same
process, and the TUI appears once, populated, with nothing pressed. `session::can_auto_reauth`
(a browser on record and the setting on) is what makes that self-starting: without it the
fallback is a set of prompts, which only a keypress should open. Only the first start may
renew on its own, so a library that really is empty is reported once, with `r` to ask again.
First-run setup goes the same way: the prompts finish and the TUI opens, rather than ending
with an instruction to start the app again. It counts as that one renewal, since cookies
seconds old are not a session worth fetching afresh.
Config lives in `~/.config/yt-music-tui/` (`browser.json`, `queue.json`, `settings.json`,
`lyrics.json`, `translations.json`, `config.toml`, `app.log`). Everything but `config.toml`
is written by the app; `config.toml` is the hand-edited one, read once at startup by
`config.rs`. The directory is `0700` and the files in it `0600`, re-applied by
`ensure_config_dir` on every startup rather than only at creation — `browser.json` holds the
cookies that *are* the signed-in session, and the default umask would leave them readable by
every account on the machine. Everything the app writes goes through `session::write_private`,
which writes a private temporary and renames it over the target, so a `Ctrl+C` or a crash
mid-write leaves the previous contents rather than half a file.

## Architecture

A Cargo workspace with three members. `ytm-core` is UI-agnostic so the engine can be driven
by something other than the ratatui frontend.

```
lrclib/     lyrics.net API client + LRC format parser (no app knowledge)
ytm-core/   session/auth, library, search, playback, queue, lyrics policy, persistence
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
- **`library.rs`** — `Library`, `Track`, `Playlist`. `LibraryFetcher` streams each
  playlist's tracks back over an `mpsc` channel as they arrive, and is kept afterwards so
  `fetch` can ask again for one that failed. `get_songs` returns `Option`: a fetch that gave
  up after three attempts is `None`, which leaves the playlist *unloaded* and flagged rather
  than loaded-and-empty. The distinction is load-bearing in two places — the panel says
  "couldn't load this playlist" and offers `r` instead of showing an empty list, and
  `try_restore` keeps waiting instead of deciding the saved queue's tracks are gone.
- **`playback.rs`** — `AudioEngine` owns an in-process **libmpv** instance (`libmpv2`) on its
  own thread, plus an `Arc<Mutex<AudioState>>` snapshot. `AudioState::elapsed` is the live
  playback position, fed by an mpv `time-pos` property observer. `audio-client-name` is set
  to `ytm` so PipeWire/PulseAudio lists the app under its own name in the system mixer
  rather than as "mpv". `pending_resolve` is the song a yt-dlp resolve is running for, and
  it is cleared only where that resolve is answered — never on an mpv event, which belongs
  to whatever mpv currently has open rather than to what the user just pressed. Clearing it
  on the `duration` property change meant a track started while the previous one was still
  settling resolved, cached its URL and was never loaded: audio carried on with the old song
  under the new song's title.
  `AudioState::track` names the video the rest of the snapshot describes, and
  `begin_track` — called by `do_play` on the *caller's* thread, before the `Play` command is
  even queued — is what makes it true immediately. `Cmd::Play` is a message to another
  thread that wakes every 20 ms, while the event loop reads the state back within
  microseconds, so for that window every figure in it belonged to the track playing a moment
  ago. `total` is the one that did damage: a plausible length for the wrong song, and lyrics
  are ranked mostly on which record's length is closest. See `measured_duration` in `app.rs`.
- **`player.rs`** — `Player`: queue, play modes, volume/mute, song-end advance. Leaving
  Shuffle restores the order the queue had before it (`unshuffled` + `reorder_to`), rather
  than sorting: a queue built by hand with `a` has an order the user chose, and across
  playlists the `(playlist, song)` pairs say nothing about it. Entries added while shuffled
  keep their place at the end, removed ones stay removed. `remap_refs` is the other side of
  a `TrackRef` being a position: when a playlist is fetched again and comes back in a
  different order, the caller says where each track went and this applies it to the queue,
  to `unshuffled` and to what is playing — `remap_queue` holding the position on the same
  entry as things are dropped around it.
- **`lyrics.rs`** — policy over `lrclib`. `LyricsService::best_for` layers `/get` (exact, has
  duration, returns one) over `/search` (returns many, ignores duration), preferring synced
  over plain; `rank` does duration-proximity scoring client-side. `spawn_best`/`spawn_choices`
  do the fetching in background tokio tasks.
- **`translate.rs`** — two backends behind one `translate_lines`, chosen by `Backend` and,
  through it, by which key was pressed. `i` is always the **free path**, described below and
  unchanged from before the AI backend existed; `I` — offered only where
  `lyrics.ai-translation` set it up — routes to **`translate/llm.rs`**,
  which sends the whole song to the Messages API in one request with no pre-processing: a
  model that sees every line reads a sentence spanning several of them by itself. Alignment
  is enforced rather than hoped for, and `place` checks it twice: the reply is constrained
  to one `{index, source, text}` per line sent, so a dropped, repeated or out-of-range line
  is an error rather than a silent shift; and each entry's `source` — the model's verbatim
  copy of the line it is translating — is compared against what actually went out, which
  catches the case the index set can't, a reply of the right length and numbering whose text
  has slipped a line. Writing the line out before translating it also keeps the model on
  *that* line, so the drift the echo detects happens less to begin with. What neither check
  can see is *meaning* moving between correctly-numbered lines, which is what a model does
  when it reads two stanzas as one passage — so `numbered` keeps the song's blank lines as
  bare separators, and the prompt says a sentence never crosses one. The prompt also asks
  for each line's own words on its own line even where the target language would order them
  differently: told instead to make the lines read as natural prose, the model collapses an
  enjambed sentence onto one line and leaves its neighbour empty or padded with invention
  (3 runs in 6, measured; 0 in 8 after). The prompt names
  the target language rather than coding it: asked for `zh`, Haiku answers in *English*
  three times out of three, hence `language_name`. Blank and repeated lines are never sent,
  so a chorus is translated once and its repeats agree by construction. Measured over three
  runs of a 40-line song, Haiku 4.5 costs 0.67¢ (the `usage` line in `app.log` says what
  each one actually cost); against a hand-translated record it beat the free path on 53% of
  lines and lost on 2%. Any failure — rate limit, spent balance, bad model id, either
  alignment check — logs and falls through to the free path, so the feature never
  disappears, only its quality changes.

  The free path is policy over the `rust-translate` crate, which wraps Google's public
  `translate_a/single`. Two of the crate's flaws are handled here and both are silent if
  they aren't: it interpolates text straight into the URL (a lyric containing `&`, `#` or
  `%` comes back mangled — hence `percent_encode`), and it returns only the *first segment*
  of the reply, dropping everything past the first full stop. So a reply is used only when
  it can be *proved* complete — one line back per line sent. `translate_distinct` probes
  with the first batch: whole ⇒ the rest of the song goes the same way (Japanese comes back
  in one segment, so a song is a couple of requests); short ⇒ re-fetched a sentence at a
  time via `sentence_pieces`, which the endpoint cannot segment further. Blank and repeated
  lines are never sent, so a chorus costs one request. Returns one entry per input line,
  empty where nothing could be translated.
- **`mpris.rs`** — MPRIS2 over the session D-Bus, via `mpris-server`. This is what makes the
  keyboard's media keys work *and* what lists the app as a player in GNOME/KDE: both grab
  `XF86Audio*` globally and forward to whoever owns an `org.mpris.MediaPlayer2.*` bus name,
  so the TUI grabs no keys itself — it could not under Wayland anyway. Same shape as
  `playback.rs`: an `Arc<Mutex<NowPlaying>>` the D-Bus tasks read, plus an `mpsc` of
  `MediaCmd` the event loop drains, since `Player` is not shared. `MediaControls::update` is
  called every tick but only emits `PropertiesChanged` on an actual change; `Position` is
  never among them, deliberately — the spec keeps it out and has clients poll it, with
  `Seeked` (emitted on a position jump larger than a tick can explain) as the only push.
  MPRIS splits looping and shuffle where `PlayMode` fuses them, so `LoopStatus::None` is
  answered as `Playlist`: this player always wraps. No session bus (ssh, headless) ⇒ `new`
  returns `None` with a log line and nothing else changes. Linux-only; other targets get a
  same-shaped stub so `app.rs` needs no `cfg`.
- **`search.rs`** — YouTube Music search, built on `YTMusicClient::send_request` since
  `ytmusicapi 0.4.2` has no search of its own: same cookies, same context, no second HTTP
  stack. Parsing walks the response for result rows rather than pathing into it — YouTube
  renames renderers without notice, and the shelf became `musicCardShelfRenderer` between one
  probe and the next. Within a row the walk is unambiguous: over ~340 rows measured, none
  carried two different `videoId`s or `musicVideoType`s, so "the first hit inside this row" is
  always the row's own (`examples/search_verify.rs` is the check).
  Songs *and* videos are fetched, deliberately, as two filtered requests rather than one
  unfiltered one — an unfiltered search mixes in artists, playlists, podcasts and profiles,
  15 of 32 rows on a measured query having no video id at all. `musicVideoType` reduces to
  `ResultKind`: `ATV` is an *art track*, the label's catalogue audio wrapped in the album
  cover, which is what the UI calls a Song and what carries a real album and release
  duration; `OMV`/`UGC`/`OFFICIAL_SOURCE_MUSIC` are videos. Both are offered because plenty
  of music exists on YouTube *only* as a video, and playback is unaffected either way — mpv
  is given `bestaudio` and never fetches a video stream. `place_search_result` files a hit
  into a synthetic `__search__` playlist so the queue, player, lyrics and prefetch can all
  address it as the `(playlist, song)` pair they already expect. Nothing ever took one out
  again, so `App::prune_search_history` empties it once it passes `MAX_SEARCH_TRACKS` — but
  only while no queue entry and nothing playing points there. Dropping any *one* of them is
  not possible: a `TrackRef` is a position, so removing a track renumbers the rest and the
  queue quietly changes meaning. `clear_search_playlist` empties the tracks and keeps the
  playlist, for the same reason.
- **`cover.rs`** — fetches a thumbnail and decodes it to RGB. `at_size` rewrites the CDN's
  own resize parameters (`=w120-h120-l90-rj`) to ask for a bigger copy than the 120px a
  search row advertises, and the size asked for is the *terminal's*: `spawn_fetch` takes the
  largest square any panel could draw the cover in, requests twice that (`fetch_px`, floored
  at 480 and capped at 1080 — measured, the CDN serves any size up to 1400 exactly and
  anything beyond as 1400), and returns it scaled to fit that square. So what is held in
  memory is what can be shown, and the 2× is there because the resampling box-averages: 2×2
  source pixels per output pixel is what makes an edge land smoothly rather than being point
  sampled.
  A cover keeps its **own shape** the whole way — `draw_px` bounds it, it does not describe
  it. Album art is served square (measured, 480×480) and a video's thumbnail is 16:9, and
  what each is drawn in is the panel's business: `kitty::fit_cells` builds a box to match.
  Getting a video's *resolution* right needs one more step, since `at_size` can do nothing
  for it — the row advertises `i.ytimg.com/…/hqdefault.jpg?sqp=…`, a signed crop with no
  size to rewrite, which arrives 400×225. `hd_variant` asks for `maxresdefault.jpg` instead,
  the same frame at 1280×720, and falls back to the advertised URL when it 404s: measured
  over five videos, three had one and two didn't. `Cover::filling` is the last step before
  the terminal, giving the image the box's exact shape so the terminal's own fill has
  nothing left to stretch; it never enlarges, since the far end can do that itself and a
  clean enlargement of the right shape is all that is wanted.
  How big what comes back may be is not the CDN's to decide: the body is read in chunks
  against `MAX_BYTES` rather than with `bytes()`, and `decode` reads the JPEG *header* first
  so a claimed size past `MAX_DECODE_PX` is refused before `width × height × 3` is allocated
  for it. One `reqwest::Client` is shared by every fetch — covers arrive in runs, all to the
  same host, and a client apiece was a TLS handshake apiece.
- **`persistence.rs`** — all through `write_private` (above), since these are written on the
  way out, when an interrupted write is most likely. `queue.json`, `settings.json` (volume), `lyrics.json` (manual lyric
  choices, keyed by video ID), `translations.json` (**AI** translations, keyed by lrclib
  record id, so one is paid for once). A queue's saved *position* is an index into the
  queue, so both halves of the round trip move it with the entries rather than carrying the
  number across: `build_queue_state` drops an entry with no video id and `try_restore` drops
  one whose playlist is gone — the synthetic search playlist, every time — and
  `follow_position` applies the same rule `Player::remap_refs` does, or the queue comes back
  playing a song it wasn't on.

  `translations.json` holds **one translation per lrclib record**, and the rules around it
  are all consequences of that. *Only the AI ones are in it*: the free endpoint costs
  nothing but a wait, so `i` asks again each session and its translation can improve, while
  `I` reuses what it bought — and nothing is written when the answering model is *empty*, an
  `I` request the free endpoint ended up serving being not what `I` bought. *One per record,
  whichever model made it*: the model is recorded for the log, never keyed on, so swapping
  `ai-model` or providers costs nothing and cannot accumulate a copy per model. The language
  is keyed on, since last week's `translate-to` is no use. *A redo replaces rather than
  clears*: `App::retranslate` forces a fresh request past the cache and lets the answer
  overwrite the entry when it lands, so a redo that hits a rate limit leaves the paid
  translation where it was instead of throwing it away and putting nothing in its place.
  Under the free translator `r` does the same thing minus the disk, since there is nothing
  of `i`'s down there to replace. Capped at `MAX_SAVED_TRANSLATIONS`, oldest written evicted
  first.
- **`config.rs`** — `config.toml`, the hand-edited settings, read once at startup. Every
  value has a working default and a missing or malformed file falls back to those with a
  log warning, so a typo can never stop playback. Because it *is* hand-edited, the reading
  is deliberately forgiving in three ways. Only a **syntax** error costs the whole file: the
  document is parsed to a `toml::Table` and each setting read out of it by `field`, so
  `offset = "0.5"` costs `offset` and leaves `cookie-browser` alone — losing that would ask
  the user to set up again at the next expiry, over a stray quote. Anything the app doesn't
  read is **named in the log** by `unknown`, because `translate_to` for `translate-to`
  parses perfectly and does nothing, which is the worst way for a setting to fail. And
  values are trimmed and range-checked in `validated`: `nan`, `inf` and 300s offsets become
  0 or ±`MAX_LYRICS_OFFSET`. What can't be recovered — a duplicate key, an unclosed string,
  `1e400` — falls back whole, with the line and column in the warning.
  `lyrics.offset` shifts every lyric line
  against the record's timings (negative = early, positive = late); it is applied by
  shifting the clock handed to `active_index`/`next_boundary`, never the cached records.
  `lyrics.ai-translation` does not *switch* translators, it *offers* one: `i` is the free
  endpoint whatever the flag says, and `true` adds `I` alongside it. So nothing is billed or
  sent to an API without a deliberate keypress, and the default install behaves exactly as it
  did before that backend existed. `lyrics.ai-model` and `lyrics.ai-key-env` default to
  something usable (`"claude-haiku-4-5"`, `"ANTHROPIC_API_KEY"`), so flipping the flag is the
  only edit needed. The key variable is *named* rather than the key being stored, so
  `config.toml` stays safe to copy around. That name is also the whole provider switch:
  `Provider::for_key_env` reads `DEEPSEEK_API_KEY` as DeepSeek and anything else as
  Anthropic, since a key belongs to one API and there is nothing else to configure. A model
  from the *other* family — `claude-haiku-4-5` still sitting there after the key line was
  swapped — is replaced by that provider's default rather than sent somewhere it would 404;
  a name neither family claims (a snapshot id, a gateway's own naming) is left alone.
  `ai_available` is the runtime test — flag on,
  model named, key found — and gates whether `I` does anything or explains itself; `validated`
  turns the flag back off when the model is blank or no key is found.
  `lyrics.translate-to` is the language `i` translates into (`"zh"`, `"fr"`, …); it is
  checked against the endpoint's own list at startup and cleared if unknown, because an
  unknown code is answered with the input unchanged rather than an error. Empty (the
  default) means nothing is ever sent to a translation service.
  `ui.covers` turns cover art off on a terminal that could draw it; it is never *attempted*
  on one the frontend doesn't recognise, so the default is safe everywhere.
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
  - **The cursor** moves through `select_next_bounded` / `select_prev_bounded` rather than
    ratatui's own, which count rows without knowing how many there are: holding `j` at the
    bottom of a list used to walk off the end and take as many `k` presses to come back.
    They also pull a selection left past the end — by a refetch that shortened the list —
    back into it, and select nothing at all in an empty one.
  - **Prefetch**: j/k in Songs warms the CDN URL for the selected + next song. The resolved
    URLs are held by `playback.rs`'s `UrlCache`, bounded at 64 entries and an hour: YouTube's
    own expiry is a few hours, and handing mpv a stale URL costs a failed load and a
    re-resolve — the work the cache exists to save.
  - **Filtering**: `filtered_songs` / `filtered_queue_positions` are memoised, keyed by
    (playlist, query, length) and (queue revision, query). Both are asked for once per frame
    *and* once per keystroke, and recomputing means lowercasing every title and artist in the
    list — at lyric-mode frame rates that is thousands of allocations a second for an answer
    that hasn't changed. `Player::queue_revision` is what makes the queue's key exact, since
    shuffling reorders it without changing its length.
  - **Lyrics**: `y` replaces the right column with a synced-lyrics panel that auto-centres the
    active line; `c` opens a modal to pick a different lrclib record; `r` retries a failed
    fetch. Results are cached per video ID and never re-fetched, so toggling is free —
    `MAX_LYRICS` of them, oldest first, since nothing else ever took one out and a session
    left running all day held every track it had played.
    A lookup is ranked against the length **mpv measured for that track** — `ensure_lyrics`
    waits up to `DURATION_WAIT` for it and falls back to YouTube's rounded figure after
    that. `measured_duration` is what makes "for that track" true: the snapshot's `total`
    holds the *previous* song's length until mpv reports the new one, and using it picked a
    different record, or the same one demoted to plain because the gap looked like a timing
    mismatch. Since the cache entry is terminal, one wrong lookup was the answer for the
    rest of the session.
    The picker collapses records carrying identical lyrics — two thirds of what lrclib
    returns for a popular track — and its left column marks `IN USE` (what the panel is
    showing) and `AUTO` (the record automatic matching resolved to). The record on screen
    is guaranteed a row, since it is often the `/get` hit the search ladder never sees.
  - **Translation**: `i` weaves a translation in under each lyric, in the language
    `lyrics.translate-to` names, from the free endpoint. `I` does the same through the AI model,
    and appears in the hint bar only where `ai_available()` — so the paid path is never a
    keypress away by accident. They are one `TranslateMode` between them: each key turns its
    own translator off again and switching is one press, with the badge marking `ai` when
    that is what is showing. Translated rows are Magenta and italic and carry the same
    `lyric` index as their original (`lyric_rows`), so a pair highlights, wraps and centres
    as the one line it is; the active line's own highlight deliberately stays on the words
    alone. A line whose translation is blank, or identical to the original, gets no row at
    all. Cached per **lrclib record id and translator**, not per video — a translation
    belongs to the words, so `c` gets one of its own and two tracks on the same record share
    one. `MAX_TRANSLATIONS` of them for the session, so replaying a song costs nothing
    either way; only the AI ones outlive it, in `translations.json`. Pressing the same key
    twice is the retry after a failure; `r` fetches another, whichever translator made the
    one on screen, and the answer replaces what was saved when it arrives.
  - **Search**: `s` opens it — a query line, then results. Songs are listed before videos and
    each row is marked `♪ song` or `▶ video` with its length, because the two are genuinely
    different things and the choice should be deliberate. `↵` plays (through
    `place_search_result`, so it queues and gets lyrics like any other track), `a` opens a
    modal listing the user's own playlists to add it to, `/` returns to the query line.
    Liked Music is special-cased: its id is literally `LM` and it is the like button rather
    than a playlist items can be added to.
    An add that lands **refetches that playlist** (`refresh_playlist`), so the new track is
    playable in the session that added it rather than at the next start. The refetch is what
    makes `TrackRef`'s "position, not identity" nature dangerous — a like lands at the *top*
    of Liked Music, moving every song down one — so `moved_indices` matches the old tracks to
    the new by video id and `Player::remap_refs` carries the queue, the order Shuffle is
    holding and the playing track across. `None` for every track means nothing moved, which
    is what appending to an ordinary playlist does, and then nothing is touched at all. A
    track that has left the playlist entirely (an edit made in a browser) drops out of the
    queue; if it is the one *playing*, it is filed under the search playlist instead, since
    it is still audibly a track and its lyrics and title have to keep resolving.
    The highlighted result's details are the one place the panel wraps rather than truncates
    (`wrap_words`): the list beside it already shows a cut-down title, so a column whose only
    job is to say more about that row has to actually say more. It breaks between words, and
    falls back to `wrap_n_lines`'s cell-exact split for a run without any — which is also the
    CJK path. Each field is capped (3 lines, then 2, then 2) so a long title can't push the
    kind and length off a short panel.
    `search_has_focus` is the one predicate deciding whether the panel owns the keyboard, and
    the key dispatch, the hint bar and the header colour all read it, so they cannot disagree.
    Typing a query and the add modal take every key regardless of focus — `h` mid-word must
    type an `h` — but once there are results to move through, `h`/`l` are the ordinary panel
    keys and focus returns to the playlists, exactly as in lyrics mode.
  - **Now playing card**: in lyrics mode the *playlists* column is given over to the playing
    track — cover centred, then title (Green, since green is what this app means by playing),
    artist, album, a short rule and the length. You are reading rather than browsing at that
    point, and it is the one column wide enough for a square picture without taking room from
    the words. `render_now_playing_card` reserves the cover's square whether or not the image
    has arrived, so nothing moves under the text when it does, and shows a `♪` in the meantime.
    Where no image can be drawn at all the block is centred vertically instead of clinging to
    the top of an empty column.
  - **Cover art** (`kitty.rs`) draws the highlighted result's cover with the kitty graphics
    protocol, and the now-playing card's. `cover_target` — a `(video id, rect)` claimed
    during the frame and acted on after it — is how a panel asks for one; `render` clears it
    first, so a cover can never outlive the panel that wanted it. Track thumbnails ride along
    with the playlist fetch (`Track::thumbnail`), so showing one costs no request of its own.
    It works *around* ratatui rather than through it: the frame is drawn with that
    rectangle left empty, then the image is placed over it afterwards, because the terminal
    composites images above the cell grid. That persistence is what the module is mostly
    about — an image left behind hangs over whatever comes next, so `Canvas` tracks exactly
    what is on screen and where, redraws only when one of those changes (resending a megabyte
    per frame is how a TUI starts to flicker), and deletes on every exit path including
    `Drop`. Support is detected from the environment rather than by querying the terminal:
    crossterm owns stdin in raw mode, and a query a terminal ignores would hang startup — so
    kitty, Ghostty and WezTerm get covers and everything else silently gets none, which is
    the right direction to be wrong in when the failure mode is base64 sprayed across the
    screen. `q=2` on every escape suppresses the terminal's reply, which crossterm would
    otherwise read as keypresses. How many pixels to send is the other half of how a cover
    looks: the terminal is told its target in *cells* and scales the image to fit, so an
    image sent smaller than the rectangle physically holds is scaled **up**, which is what a
    soft cover on a HiDPI display is. `cell_size` therefore asks what a cell really measures
    — `TIOCGWINSZ` via crossterm, an ioctl rather than a query written to the terminal, so
    unlike the support handshake above it cannot hang — and falls back to 10×20 on a zero, an
    implausible size, or no tty. `App::cover_draw_px` turns that into the pixels the
    now-playing card's `MAX_COVER_COLS`-wide box comes to, which is what gets fetched.
    The *shape* of that box is the same measurement used the other way round, and
    `fit_cells` is where both panels get theirs. Two things it settles at once. A cell is
    not square and is not reliably twice as tall as it is wide either, so `n` columns by
    `n / 2` rows — which is what both panels reserved — is a square only by luck: on a 9×20
    cell it is 216 across by 240 down, and since the terminal scales the image to fill
    exactly the cells it was given, the cover comes out 11% too tall. And a cover is not
    always square to begin with, so the box is built from the *picture's* proportions rather
    than an assumed one — `App::cover_aspect` reads them off the cover once it has arrived,
    and answers square until then, which is what album art is and what the reserved block
    should look like while it loads. Whole cells rarely divide out exactly, so the largest
    box within `ASPECT_TOLERANCE` (3%, about nine pixels on a 300-pixel cover) wins rather
    than the exact one — area is visible where the last few pixels of shape are not.
  - **Fitting** (`fit_meta`) is the one rule for a title with metadata after it: the title
    has first claim on the width, each following field takes what is left, and anything cut
    is marked `…`. Used by the songs list, the queue, the search results and the player bar,
    because a `Table` otherwise clips at the column edge — mid-word, and mid-*character* on a
    CJK title, with nothing to show that anything was lost. The width each list can offer is
    computed from its own columns (`track_text_width` and friends) rather than guessed, so
    the ellipsis lands exactly where the clip used to.
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
| `PgUp` / `PgDn` | (in lyrics mode) Scroll lyrics five lines |
| `h` / `l` | Switch panels (Playlists ↔ Songs) |
| `Enter` | Open playlist / play song / play from queue |
| `/` | Enter filter mode (songs or queue panel) |
| `Esc` | Re-centre lyrics → close lyrics → clear filter → back to playlists → quit |
| `Space` | Pause / resume |
| `p` / `n` | Previous / next in queue — `p` past 3 s into a track restarts it, at the start it goes back one. Since the restart leaves playback at zero, a run of presses walks back a track at a time, untimed (`Player::restart_or_previous`, shared with the MPRIS `Previous` key) |
| `←` / `→` | Seek −5s / +5s |
| `↑` / `↓` | Volume +5 / −5 |
| `m` | Mute / unmute |
| `t` | Cycle play mode (Cycle → Single → Shuffle) |
| `a` | Append selected song to queue |
| `d` | Remove selected queue entry |
| `o` | Toggle queue / songs view |
| `s` | Search YouTube Music (`↵` play, `a` add to a playlist, `/` edit query) |
| `y` | Toggle lyrics panel |
| `c` | (in lyrics mode) Choose a different lrclib record |
| `i` | (in lyrics mode) Toggle the translation under each line, from the free endpoint |
| `I` | (in lyrics mode) The same, translated by the AI model instead — only where `lyrics.ai-translation` set it up |
| `r` | (in lyrics mode) Redo the translation on screen, or retry a failed lyrics fetch when there are no words yet. On a playlist whose fetch failed, fetches it again |
| `?` | Full keymap overlay (any key closes it) |
| `q` / `Ctrl+C` | Quit |

Each context's hint list names *every* key that works in it, and `fit_hints` drops
whole hints from the end until what is left fits the terminal — so order is
priority order, and a wider terminal is simply a longer bar. `?` sits sixth in
every list rather than last, because it is the way to the bindings below it and
must survive 80 columns; `the_way_to_the_full_keymap_survives_a_narrow_terminal`
is the test that says so. `App::KEYMAP` is the `?` overlay, and
`the_keymap_and_the_hint_bar_agree` fails if it grows a binding no hint bar names.
`Self::TAIL` is the set that works everywhere (seek, volume, mute, mode, panel,
quit), appended to every context except the modal picker.

Note: raw mode clears `ISIG`, so `Ctrl+C` never becomes a signal — `app.rs` matches it as a
key event. The `ctrlc` handler in `main.rs` only covers SIGTERM/SIGHUP.

### Data flow

```
main.rs
  └─ Session::build_client()
  └─ library::get_playlists()  ──► App::new(library, saved_queue, songs_rx, rt_handle)
  └─ library::LibraryFetcher::new()   └─ App::run()
       (tokio tasks → mpsc)                └─ event_loop
                                                ├─ drain_song_channel / drain_lyrics
                                                │  / drain_translations / drain_media
                                                ├─ render
                                                ├─ update_media → MPRIS → D-Bus
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

**`mpris-server 0.10`** is a Linux-only target dependency, so Windows and macOS builds never
see zbus at all. Its `tokio` feature matters more than it looks: zbus picks a reactor by
asking `Handle::try_current()` *while the connection is being built*, so `Server::new` has to
be called from inside the runtime (`MediaControls::new` uses `Handle::block_on` for exactly
this) or it silently starts a second, async-io driver thread. `async-io` is compiled either
way — mpris-server depends on zbus with default features, and features unify — which costs
build time only. zbus 5 shares nothing with the reqwest 0.12 stack; `cargo tree -i aws-lc-sys`
still reports *not found*.

`rust-translate 0.1.3` is the free path's transport, and the source of `supported_languages`
that `normalise_language` checks `lyrics.translate-to` against. It brings no HTTP stack of its
own — its `reqwest 0.12.4` resolves to the 0.12 already in the graph. It does ask for tokio's
`full` feature set, which unifies across the workspace; that costs build time, nothing at
runtime. See `ytm-core/src/translate.rs` for what it gets wrong and how that is handled.

The AI path in `translate/llm.rs` calls the Anthropic Messages API over the workspace's own
`reqwest` — no SDK, since there is no official Rust one, and no new HTTP stack. `cargo tree -i
aws-lc-sys` still reports *not found*. DeepSeek serves the same request shape at its own host,
so `Provider` carries the four things that differ: the URL, `Authorization: Bearer` against
Anthropic's `x-api-key`, the `max_tokens` ceiling (8192 there, 32000 here), and whether
`output_config` can constrain the reply. It can't on DeepSeek, so the schema goes in the
prompt and `json_object` digs the JSON out of any fence or preamble around it — the two
alignment checks are what make that safe, since a reply that arrives mangled is rejected and
the free path answers instead. Measured at 0.04¢ a song on `deepseek-chat`.

Every request sends `thinking: {"type": "disabled"}`, which both providers accept. It is not
an optimisation: `deepseek-v4-flash` thinks by default, thinking is charged against
`max_tokens`, and it spent all 8192 of them reasoning — no text block at all, `stop_reason:
max_tokens`, 80 seconds to fail, then a silent fall through to the free path. Disabled, the
same request is 3.7s and 541 tokens. Nothing on this path needs deliberation; alignment is a
rule, not a judgement.

`jpeg-decoder 0.3.2` decodes cover art, with `default-features = false` to drop rayon — a
120px thumbnail decodes in well under a millisecond and does not want a thread pool, still
less a second one beside tokio's. It is the only image dependency because it is the only
format that arrives: YouTube's image CDN serves `…-rj`, where `rj` *is* "return JPEG". Base64
for the kitty protocol is hand-written in `tui/src/kitty.rs` for the same reason
`translate::percent_encode` is — a dozen lines of table lookup against a dependency to audit
and pin.

Key deps: `ratatui 0.30`, `ytmusicapi 0.4.2`, `libmpv2 6`, `reqwest 0.12`, `thiserror 1`,
`mpris-server 0.10` (Linux only),
`rust-translate 0.1.3`, `ctrlc 3` (termination feature), `simplelog 0.12`, `rand 0.8`,
`throbber-widgets-tui 0.11`.
