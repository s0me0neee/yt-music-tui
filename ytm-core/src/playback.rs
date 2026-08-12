//! Audio playback: an mpv instance embedded in-process via `libmpv2`, driven
//! from a dedicated thread through a command mailbox.
//!
//! Split into its own thread (rather than driven from the caller's event
//! loop) so a blocking mpv/yt-dlp call never stalls anything else — worst
//! case this thread hangs, not the whole process.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};

use crate::shutdown::is_shutdown_requested;

#[allow(dead_code)]
pub enum Cmd {
    Play(String),     // video_id
    Prefetch(String), // pre-resolve CDN URL before the user presses play
    Pause,
    Resume,
    Seek(f64),    // relative seconds
    SeekAbs(f64), // absolute position, seconds
    Volume(u8),   // 0-100
    Stop,
}

#[derive(Clone, Default)]
pub struct AudioState {
    pub elapsed: f64,
    pub total: f64,
    pub paused: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub song_ended: bool, // set on natural eof; caller must reset after reading
}

pub struct AudioEngine {
    cmd_tx: Option<std::sync::mpsc::Sender<Cmd>>,
    state: Arc<Mutex<AudioState>>,
    audio_thread: Option<thread::JoinHandle<()>>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = Arc::new(Mutex::new(AudioState::default()));
        let state2 = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(rx, state2))
            .expect("spawn audio thread");
        Self {
            cmd_tx: Some(tx),
            state,
            audio_thread: Some(handle),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    /// Snapshot of the current playback state.
    pub fn state(&self) -> AudioState {
        self.lock_state().clone()
    }

    /// Atomically reads and clears `song_ended`. `true` only once per natural
    /// end-of-track.
    pub fn take_song_ended(&self) -> bool {
        let mut s = self.lock_state();
        std::mem::take(&mut s.song_ended)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AudioState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Dropping the sender ends the audio thread, which drops the embedded mpv.
        drop(self.cmd_tx.take());
        if let Some(h) = self.audio_thread.take() {
            let _ = h.join();
        }
    }
}

// ── URL resolution ──────────────────────────────────────────────────────────────

#[hotpath::measure]
fn resolve_url(video_id: &str) -> Option<String> {
    // Don't start new yt-dlp work if the app is shutting down.
    if is_shutdown_requested() {
        return None;
    }

    let yt_url = format!("https://music.youtube.com/watch?v={video_id}");
    log::debug!("[audio] yt-dlp resolving {video_id}");
    let out = Command::new("yt-dlp")
        .args([
            "-f",
            "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
            "--get-url",
            "--no-playlist",
            &yt_url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !out.status.success() {
        log::warn!("[audio] yt-dlp failed for {video_id}");
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let url = stdout.trim().lines().next()?.to_string();
    if url.starts_with("http") {
        Some(url)
    } else {
        None
    }
}

// ── audio thread ──────────────────────────────────────────────────────────────

fn lock_state(state: &Mutex<AudioState>) -> std::sync::MutexGuard<'_, AudioState> {
    // Recover from a poisoned lock rather than panicking the audio thread.
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set an mpv property, logging (but not propagating) any error.
fn set_prop<V: libmpv2::SetData>(mpv: &Mpv, name: &str, value: V) {
    if let Err(e) = mpv.set_property(name, value) {
        log::warn!("[audio] set_property {name} failed: {e}");
    }
}

/// Load `url` into mpv (replacing whatever is playing) and unpause.
fn load_url(mpv: &Mpv, url: &str) {
    if let Err(e) = mpv.command("loadfile", &[url, "replace"]) {
        log::warn!("[audio] loadfile failed: {e}");
    }
    set_prop(mpv, "pause", false);
}

/// mpv errors that mean "that URL did not play", as opposed to something being
/// wrong with the player itself.
///
/// `LOADING_FAILED` is the everyday one — a CDN URL that has gone stale. The
/// other two are how a *fallback* fails once mpv has opened something that
/// isn't media at all: an expiry page parses as an unknown format, or as a
/// container with no playable stream. Matching only `LOADING_FAILED` meant the
/// fallback's own failure fell through to the catch-all warn arm, so the retry
/// could neither escalate nor give up and the player sat "loading" forever.
///
/// `UNSUPPORTED` (-18) is deliberately absent: it means the system can't play
/// this kind of stream, which a different URL for the same song won't change.
const LOAD_FAILED: &[libmpv2::MpvError] = &[
    -13, // MPV_ERROR_LOADING_FAILED
    -16, // MPV_ERROR_NOTHING_TO_PLAY
    -17, // MPV_ERROR_UNKNOWN_FORMAT
];

/// Starts a yt-dlp resolve for `id` on its own thread, unless one is already
/// running. `what` only names the thread, for debuggers and panic messages.
fn spawn_resolve(
    id: &str,
    what: &str,
    fetching: &mut HashSet<String>,
    tx: &std::sync::mpsc::Sender<(String, Option<String>)>,
) {
    if !fetching.insert(id.to_string()) {
        return; // already in flight; its result will be delivered to us anyway
    }
    let tx = tx.clone();
    let id = id.to_string();
    thread::Builder::new()
        .name(format!("{what}-{id}"))
        .spawn(move || {
            let _ = tx.send((id.clone(), resolve_url(&id)));
        })
        .ok();
}

fn run(rx: Receiver<Cmd>, state: Arc<Mutex<AudioState>>) {
    #[cfg(windows)]
    let which_cmd = "where";
    #[cfg(not(windows))]
    let which_cmd = "which";
    match Command::new(which_cmd).arg("yt-dlp").output() {
        Ok(o) if o.status.success() => {
            log::info!(
                "[audio] yt-dlp found at {}",
                String::from_utf8_lossy(&o.stdout).trim()
            );
        }
        _ => {
            let msg = "yt-dlp not found — install with: brew install yt-dlp".to_string();
            log::error!("[audio] {msg}");
            lock_state(&state).error = Some(msg);
        }
    }

    // ── create embedded mpv (options must be set before init) ──────────────────
    let mpv = match Mpv::with_initializer(|init| {
        init.set_property("vid", "no")?;
        init.set_property("vo", "null")?;
        init.set_property("ytdl", "yes")?;
        init.set_property(
            "ytdl-format",
            "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
        )?;
        init.set_property("script-opts", "ytdl_hook-ytdl_path=yt-dlp")?;
        init.set_property("gapless-audio", "yes")?;
        init.set_property("audio-display", "no")?;
        // What PipeWire/PulseAudio calls this stream, so the system mixer
        // lists the app under its own name and slider rather than "mpv".
        init.set_property("audio-client-name", "ytm")?;
        init.set_property("cache", "yes")?;
        init.set_property("demuxer-readahead-secs", 30i64)?;
        init.set_property("cache-pause-initial", "no")?; // start ASAP, don't pre-fill
        // Probe only a little before starting — YouTube audio is detected from the
        // first packets, so this avoids ffmpeg reading megabytes up front. The
        // LOADING_FAILED retry re-resolves if a stream ever needs more.
        init.set_property("demuxer-lavf-probesize", 65536i64)?;
        init.set_property("demuxer-lavf-analyzeduration", 0.1f64)?;
        init.set_property("idle", "yes")?;
        init.set_property("keep-open", "yes")?;
        Ok(())
    }) {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("libmpv init failed: {e}");
            log::error!("[audio] {msg}");
            lock_state(&state).error = Some(msg);
            return;
        }
    };

    for prop in ["time-pos", "duration"] {
        if let Err(e) = mpv.observe_property(prop, Format::Double, 0) {
            log::error!("[audio] observe {prop} failed: {e}");
        }
    }
    for prop in ["pause", "eof-reached"] {
        if let Err(e) = mpv.observe_property(prop, Format::Flag, 0) {
            log::error!("[audio] observe {prop} failed: {e}");
        }
    }
    log::info!("[audio] embedded mpv ready, entering event loop");

    // ── background URL resolution ─────────────────────────────────────────────
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<(String, Option<String>)>();
    let mut url_cache: HashMap<String, String> = HashMap::new();
    let mut fetching: HashSet<String> = HashSet::new();
    let mut pending_resolve: Option<String> = None;
    // Track of the song mpv is loading, so a load failure can be retried once.
    let mut current_id: Option<String> = None;
    let mut load_retried = false;

    // Max concurrent yt-dlp resolves (play-resolve + prefetches share `fetching`).
    const MAX_PREFETCH: usize = 3;

    // ── main loop ─────────────────────────────────────────────────────────────
    loop {
        if is_shutdown_requested() {
            log::info!("[audio] QUIT signal — dropping mpv");
            return;
        }

        // ── drain background resolution results ──────────────────────────────
        while let Ok((id, maybe_url)) = fetch_rx.try_recv() {
            fetching.remove(&id);
            match maybe_url {
                Some(url) => {
                    // If the user is waiting on this song, load it now.
                    if pending_resolve.as_deref() == Some(id.as_str()) {
                        log::info!("[audio] loading resolved CDN URL for {id}");
                        load_url(&mpv, &url);
                        pending_resolve = None;
                    }
                    log::info!("[audio] cached CDN URL for {id}");
                    url_cache.insert(id, url);
                }
                None => {
                    if pending_resolve.as_deref() == Some(id.as_str()) {
                        // Fall back to mpv's own ytdl_hook so playback still works.
                        log::warn!("[audio] resolve failed for {id} — falling back to ytdl_hook");
                        load_url(&mpv, &format!("https://music.youtube.com/watch?v={id}"));
                        pending_resolve = None;
                    } else {
                        log::warn!("[audio] prefetch resolve failed for {id}");
                    }
                }
            }
        }

        // ── commands from UI ─────────────────────────────────────────────────
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("[audio] channel disconnected — dropping mpv");
                    disconnected = true;
                    break;
                }
                Ok(cmd) => match cmd {
                    Cmd::Play(id) => {
                        pending_resolve = None;
                        current_id = Some(id.clone());
                        load_retried = false;
                        {
                            let mut s = lock_state(&state);
                            s.loading = true;
                            s.paused = false;
                            s.elapsed = 0.0;
                            s.total = 0.0;
                            s.error = None;
                            s.song_ended = false;
                        }

                        if let Some(cached) = url_cache.get(&id).cloned() {
                            log::info!("[audio] Play {id}: cache HIT — instant CDN URL");
                            load_url(&mpv, &cached);
                        } else {
                            // Cache miss: resolve ourselves and load when it lands.
                            // Don't hand mpv the watch URL — that spawns a second,
                            // competing yt-dlp via its ytdl_hook.
                            log::info!("[audio] Play {id}: cache miss — resolving (single yt-dlp)");
                            pending_resolve = Some(id.clone());
                            spawn_resolve(&id, "resolve", &mut fetching, &fetch_tx);
                        }
                    }

                    Cmd::Prefetch(id) => {
                        if url_cache.contains_key(&id) || fetching.contains(&id) {
                            continue;
                        }
                        if fetching.len() >= MAX_PREFETCH {
                            continue;
                        }
                        spawn_resolve(&id, "prefetch", &mut fetching, &fetch_tx);
                    }

                    Cmd::Pause => {
                        log::debug!("[audio] Pause");
                        set_prop(&mpv, "pause", true);
                    }
                    Cmd::Resume => {
                        log::debug!("[audio] Resume");
                        set_prop(&mpv, "pause", false);
                    }
                    Cmd::Seek(d) => {
                        log::debug!("[audio] Seek {d:+}s");
                        let ds = d.to_string();
                        if let Err(e) = mpv.command("seek", &[ds.as_str(), "relative"]) {
                            log::warn!("[audio] seek failed: {e}");
                        }
                    }
                    Cmd::SeekAbs(t) => {
                        log::debug!("[audio] SeekAbs {t:.1}s");
                        let ts = t.to_string();
                        if let Err(e) = mpv.command("seek", &[ts.as_str(), "absolute"]) {
                            log::warn!("[audio] absolute seek failed: {e}");
                        }
                    }
                    Cmd::Volume(v) => {
                        log::debug!("[audio] Volume {v}");
                        set_prop(&mpv, "volume", i64::from(v));
                    }
                    Cmd::Stop => {
                        log::debug!("[audio] Stop");
                        pending_resolve = None; // don't auto-play a late resolve
                        current_id = None;
                        if let Err(e) = mpv.command("stop", &[]) {
                            log::warn!("[audio] stop failed: {e}");
                        }
                    }
                },
            }
        }
        if disconnected {
            return;
        }

        // ── events from mpv ──────────────────────────────────────────────────
        while let Some(ev) = mpv.wait_event(0.0) {
            match ev {
                Ok(Event::PropertyChange { name, change, .. }) => {
                    let mut s = lock_state(&state);
                    match (name, change) {
                        ("time-pos", PropertyData::Double(v)) => s.elapsed = v,
                        ("pause", PropertyData::Flag(b)) => s.paused = b,
                        ("duration", PropertyData::Double(v)) => {
                            log::info!("[audio] duration: {v:.1}s");
                            s.total = v;
                            s.loading = false;
                            pending_resolve = None;
                        }
                        ("eof-reached", PropertyData::Flag(true)) => {
                            log::info!("[audio] eof-reached → song_ended");
                            s.song_ended = true;
                        }
                        _ => {}
                    }
                }
                Ok(Event::StartFile) => {
                    log::info!("[audio] start-file");
                    let mut s = lock_state(&state);
                    s.loading = true;
                    s.elapsed = 0.0;
                }
                Ok(Event::EndFile(reason)) => {
                    log::info!("[audio] end-file: reason={reason}");
                    lock_state(&state).loading = false;
                }
                Ok(Event::FileLoaded) => log::info!("[audio] file-loaded"),
                Ok(_) => {}
                // The URL didn't play. Drop it and resolve the song again, once.
                Err(libmpv2::Error::Raw(code)) if LOAD_FAILED.contains(&code) => {
                    match current_id.clone() {
                        Some(id) if !load_retried => {
                            log::warn!("[audio] load failed for {id} (mpv {code}) — re-resolving");
                            load_retried = true;
                            url_cache.remove(&id);
                            // Resolve it ourselves rather than handing mpv the
                            // watch URL. Its ytdl_hook is the path that failed
                            // in the field, on a video a fresh single yt-dlp
                            // resolve then played without complaint. If this
                            // resolve comes back empty we still fall back to
                            // ytdl_hook, where the results are drained.
                            pending_resolve = Some(id.clone());
                            spawn_resolve(&id, "re-resolve", &mut fetching, &fetch_tx);
                        }
                        Some(id) => {
                            log::error!(
                                "[audio] load failed for {id} (mpv {code}) after retry — giving up"
                            );
                            let mut s = lock_state(&state);
                            s.loading = false;
                            s.error = Some("playback failed".into());
                        }
                        None => log::warn!("[audio] load failed (mpv {code}), no current track"),
                    }
                }
                Err(e) => log::warn!("[audio] mpv event error: {e}"),
            }
        }

        thread::sleep(Duration::from_millis(20));
    }
}
