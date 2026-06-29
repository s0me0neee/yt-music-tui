use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};

#[allow(dead_code)]
pub enum Cmd {
    Play(String),     // video_id
    Prefetch(String), // pre-resolve CDN URL before the user presses play
    Pause,
    Resume,
    Seek(i64),        // relative seconds
    Volume(u8),       // 0-100
    Stop,
}

#[derive(Clone, Default)]
pub struct AudioState {
    pub elapsed:    f64,
    pub total:      f64,
    pub paused:     bool,
    pub loading:    bool,
    pub error:      Option<String>,
    pub song_ended: bool, // set on natural eof; caller must reset after reading
}

pub struct AudioEngine {
    cmd_tx:       Option<std::sync::mpsc::Sender<Cmd>>,
    pub state:    Arc<Mutex<AudioState>>,
    audio_thread: Option<thread::JoinHandle<()>>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state    = Arc::new(Mutex::new(AudioState::default()));
        let state2   = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(rx, state2))
            .expect("spawn audio thread");
        Self { cmd_tx: Some(tx), state, audio_thread: Some(handle) }
    }

    pub fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Dropping the sender disconnects the channel; the audio thread detects
        // Disconnected and drops the embedded mpv (clean teardown, no process).
        drop(self.cmd_tx.take());
        if let Some(h) = self.audio_thread.take() {
            let _ = h.join(); // returns Err if the thread panicked — ignored intentionally
        }
    }
}

// ── URL resolution ──────────────────────────────────────────────────────────────

fn resolve_url(video_id: &str) -> Option<String> {
    // Don't start new yt-dlp work if the app is shutting down.
    if crate::QUIT.load(Ordering::Relaxed) {
        return None;
    }

    let yt_url = format!("https://music.youtube.com/watch?v={video_id}");
    log::debug!("[audio] yt-dlp resolving {video_id}");
    let out = Command::new("yt-dlp")
        .args([
            "-f", "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
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
    let url    = stdout.trim().lines().next()?.to_string();
    if url.starts_with("http") { Some(url) } else { None }
}

// ── audio thread ──────────────────────────────────────────────────────────────

fn lock_state(state: &Mutex<AudioState>) -> std::sync::MutexGuard<'_, AudioState> {
    // If another thread panicked while holding this lock, recover the inner
    // value rather than propagating the poison and panicking the audio thread.
    state.lock().unwrap_or_else(|p| p.into_inner())
}

/// Set an mpv property, logging (but not propagating) any error.
fn set_prop<V: libmpv2::SetData>(mpv: &Mpv, name: &str, value: V) {
    if let Err(e) = mpv.set_property(name, value) {
        log::warn!("[audio] set_property {name} failed: {e}");
    }
}

fn run(rx: Receiver<Cmd>, state: Arc<Mutex<AudioState>>) {
    #[cfg(windows)]
    let which_cmd = "where";
    #[cfg(not(windows))]
    let which_cmd = "which";
    match Command::new(which_cmd).arg("yt-dlp").output() {
        Ok(o) if o.status.success() => {
            log::info!("[audio] yt-dlp found at {}", String::from_utf8_lossy(&o.stdout).trim());
        }
        _ => {
            let msg = "yt-dlp not found — install with: brew install yt-dlp".to_string();
            log::error!("[audio] {msg}");
            lock_state(&state).error = Some(msg);
        }
    }

    // ── create embedded mpv ────────────────────────────────────────────────────
    // ytdl/format/vo options must be set before init, so use with_initializer.
    let mpv = match Mpv::with_initializer(|init| {
        init.set_property("vid", "no")?;
        init.set_property("vo", "null")?;
        init.set_property("ytdl", "yes")?;
        init.set_property("ytdl-format", "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio")?;
        init.set_property("script-opts", "ytdl_hook-ytdl_path=yt-dlp")?;
        init.set_property("gapless-audio", "yes")?;
        init.set_property("audio-display", "no")?;
        init.set_property("cache", "yes")?;
        init.set_property("demuxer-readahead-secs", 30i64)?;
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
    let mut url_cache:       HashMap<String, String> = HashMap::new();
    let mut fetching:        HashSet<String>          = HashSet::new();
    let mut pending_resolve: Option<String>           = None;

    const MAX_PREFETCH: usize = 2;

    // ── main loop ─────────────────────────────────────────────────────────────
    loop {
        if crate::QUIT.load(Ordering::Relaxed) {
            log::info!("[audio] QUIT signal — dropping mpv");
            return;
        }

        // ── drain background resolution results ──────────────────────────────
        while let Ok((id, maybe_url)) = fetch_rx.try_recv() {
            fetching.remove(&id);
            match maybe_url {
                Some(url) => {
                    if pending_resolve.as_deref() == Some(id.as_str()) {
                        if lock_state(&state).loading {
                            log::info!("[audio] upgrading in-flight play to direct URL for {id}");
                            if let Err(e) = mpv.command("loadfile", &[url.as_str(), "replace"]) {
                                log::warn!("[audio] loadfile upgrade failed: {e}");
                            }
                        }
                        pending_resolve = None;
                    }
                    log::info!("[audio] cached CDN URL for {id}");
                    url_cache.insert(id, url);
                }
                None => {
                    log::warn!("[audio] resolve failed for {id} — mpv will use its own yt-dlp");
                    if pending_resolve.as_deref() == Some(id.as_str()) {
                        pending_resolve = None;
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

                        let url = if let Some(cached) = url_cache.get(&id) {
                            log::info!("[audio] Play {id}: cache HIT — instant CDN URL");
                            cached.clone()
                        } else {
                            if !fetching.contains(&id) {
                                fetching.insert(id.clone());
                                pending_resolve = Some(id.clone());
                                let tx  = fetch_tx.clone();
                                let id2 = id.clone();
                                thread::Builder::new()
                                    .name(format!("resolve-{id2}"))
                                    .spawn(move || { let _ = tx.send((id2.clone(), resolve_url(&id2))); })
                                    .ok();
                            }
                            log::info!("[audio] Play {id}: cache miss — YouTube URL (resolving in background)");
                            format!("https://music.youtube.com/watch?v={id}")
                        };

                        if let Err(e) = mpv.command("loadfile", &[url.as_str(), "replace"]) {
                            log::warn!("[audio] loadfile failed: {e}");
                        }
                        set_prop(&mpv, "pause", false);
                        let mut s = lock_state(&state);
                        s.loading    = true;
                        s.paused     = false;
                        s.elapsed    = 0.0;
                        s.total      = 0.0;
                        s.error      = None;
                        s.song_ended = false;
                    }

                    Cmd::Prefetch(id) => {
                        if url_cache.contains_key(&id) || fetching.contains(&id) { continue; }
                        if fetching.len() >= MAX_PREFETCH { continue; }
                        fetching.insert(id.clone());
                        let tx  = fetch_tx.clone();
                        let id2 = id.clone();
                        thread::Builder::new()
                            .name(format!("prefetch-{id2}"))
                            .spawn(move || { let _ = tx.send((id2.clone(), resolve_url(&id2))); })
                            .ok();
                    }

                    Cmd::Pause   => { log::debug!("[audio] Pause");       set_prop(&mpv, "pause", true); }
                    Cmd::Resume  => { log::debug!("[audio] Resume");      set_prop(&mpv, "pause", false); }
                    Cmd::Seek(d) => {
                        log::debug!("[audio] Seek {d:+}s");
                        let ds = d.to_string();
                        if let Err(e) = mpv.command("seek", &[ds.as_str(), "relative"]) {
                            log::warn!("[audio] seek failed: {e}");
                        }
                    }
                    Cmd::Volume(v) => { log::debug!("[audio] Volume {v}"); set_prop(&mpv, "volume", i64::from(v)); }
                    Cmd::Stop    => {
                        log::debug!("[audio] Stop");
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
                        ("pause", PropertyData::Flag(b))       => s.paused = b,
                        ("duration", PropertyData::Double(v)) => {
                            log::info!("[audio] duration: {v:.1}s");
                            s.total   = v;
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
                Err(e) => log::warn!("[audio] mpv event error: {e}"),
            }
        }

        thread::sleep(Duration::from_millis(20));
    }
}
