use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

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
        // Disconnecting the sender causes run() to detect Disconnected and kill mpv.
        drop(self.cmd_tx.take());
        if let Some(h) = self.audio_thread.take() {
            let _ = h.join();
        }
    }
}

// ── URL resolution ────────────────────────────────────────────────────────────

fn resolve_url(video_id: &str) -> Option<String> {
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

fn mpv_write(w: &mut impl Write, msg: Value) {
    let mut s = msg.to_string();
    s.push('\n');
    if let Err(e) = w.write_all(s.as_bytes()) {
        log::error!("[audio] mpv write failed: {e}");
    }
}

// ── audio thread ──────────────────────────────────────────────────────────────

fn run(rx: std::sync::mpsc::Receiver<Cmd>, state: Arc<Mutex<AudioState>>) {
    for bin in ["mpv", "yt-dlp"] {
        match Command::new("which").arg(bin).output() {
            Ok(o) if o.status.success() => {
                log::info!("[audio] {bin} found at {}", String::from_utf8_lossy(&o.stdout).trim());
            }
            _ => {
                let msg = format!("{bin} not found — install with: brew install {bin}");
                log::error!("[audio] {msg}");
                state.lock().unwrap().error = Some(msg);
            }
        }
    }

    let socket = format!("/tmp/yt-tui-{}.sock", std::process::id());

    // ── spawn mpv ────────────────────────────────────────────────────────────
    let mut child = match Command::new("mpv")
        .args([
            "--no-video",
            "--no-terminal",
            "--ytdl",
            "--ytdl-format=bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
            "--script-opts=ytdl_hook-ytdl_path=yt-dlp",
            "--gapless-audio=yes",
            "--audio-display=no",
            "--prefetch-playlist=yes",
            "--cache=yes",
            "--demuxer-readahead-secs=30",
            &format!("--input-ipc-server={socket}"),
            "--idle=yes",
            "--keep-open=yes",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => { log::info!("[audio] mpv spawned (pid {})", c.id()); c }
        Err(e) => {
            let msg = format!("mpv spawn failed: {e} — brew install mpv");
            log::error!("[audio] {msg}");
            state.lock().unwrap().error = Some(msg);
            return;
        }
    };

    // ── wait for IPC socket (up to 2.5 s) ───────────────────────────────────
    let stream = {
        let mut conn = None;
        for attempt in 0..50 {
            thread::sleep(Duration::from_millis(50));
            match UnixStream::connect(&socket) {
                Ok(s) => {
                    log::info!("[audio] IPC connected after {}ms", (attempt + 1) * 50);
                    conn = Some(s);
                    break;
                }
                Err(e) if attempt == 49 => {
                    log::error!("[audio] IPC socket never appeared: {e}");
                }
                _ => {}
            }
        }
        match conn {
            Some(s) => s,
            None => {
                state.lock().unwrap().error = Some("mpv IPC unavailable".into());
                let _ = child.kill();
                let _ = child.wait();
                std::fs::remove_file(&socket).ok();
                return;
            }
        }
    };

    let mut writer = stream.try_clone().expect("clone IPC socket");
    let reader     = BufReader::new(stream);

    for (id, prop) in [(1u32, "time-pos"), (2, "pause"), (3, "duration")] {
        mpv_write(&mut writer, json!({"command": ["observe_property", id, prop]}));
    }
    log::info!("[audio] mpv IPC ready, entering event loop");

    // ── mpv reader thread ─────────────────────────────────────────────────────
    // Exits naturally when mpv's socket closes (mpv died). ev_tx being dropped
    // signals the audio loop that mpv is gone.
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Value>();
    thread::Builder::new()
        .name("mpv-reader".into())
        .spawn(move || {
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&l) {
                            if ev_tx.send(v).is_err() { break; }
                        }
                    }
                    Err(e) => { log::error!("[audio] reader: {e}"); break; }
                }
            }
            log::info!("[audio] reader thread exited");
        })
        .expect("spawn mpv-reader");

    // ── background URL resolution ─────────────────────────────────────────────
    // Single channel for both Prefetch and Play-time resolution.
    // pending_resolve tracks whether the in-flight resolve is for the currently
    // loading song — if it arrives while mpv is still loading we upgrade the URL.
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<(String, Option<String>)>();
    let mut url_cache:       HashMap<String, String> = HashMap::new();
    let mut fetching:        HashSet<String>          = HashSet::new();
    let mut pending_resolve: Option<String>           = None;

    const MAX_PREFETCH: usize = 2;

    // ── main loop ─────────────────────────────────────────────────────────────
    loop {
        // ── SIGTERM / SIGHUP shutdown ────────────────────────────────────────
        if crate::QUIT.load(Ordering::Relaxed) {
            log::info!("[audio] QUIT signal — killing mpv");
            let _ = child.kill();
            let _ = child.wait();
            std::fs::remove_file(&socket).ok();
            return;
        }

        // ── drain background resolution results ──────────────────────────────
        while let Ok((id, maybe_url)) = fetch_rx.try_recv() {
            fetching.remove(&id);
            match maybe_url {
                Some(url) => {
                    // If this resolve is for the currently loading song, switch
                    // mpv to the direct CDN URL now — skips mpv's own yt-dlp run.
                    if pending_resolve.as_deref() == Some(id.as_str()) {
                        let still_loading = state.lock().unwrap().loading;
                        if still_loading {
                            log::info!("[audio] upgrading in-flight play to direct URL for {id}");
                            mpv_write(&mut writer, json!({"command": ["loadfile", url, "replace"]}));
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
        loop {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("[audio] channel disconnected — killing mpv");
                    let _ = child.kill();
                    let _ = child.wait();
                    std::fs::remove_file(&socket).ok();
                    return;
                }
                Ok(cmd) => match cmd {
                    Cmd::Play(id) => {
                        // Cancel any stale pending resolution from a previous song.
                        pending_resolve = None;

                        let url = if let Some(cached) = url_cache.get(&id) {
                            log::info!("[audio] Play {id}: cache HIT — instant CDN URL");
                            cached.clone()
                        } else {
                            // No cache — send YouTube URL immediately so mpv starts
                            // buffering right away, and simultaneously resolve the
                            // direct CDN URL in the background. If resolution wins
                            // the race (mpv still loading), we upgrade with loadfile.
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

                        mpv_write(&mut writer, json!({"command": ["loadfile", url, "replace"]}));
                        // --keep-open=yes leaves mpv paused at EOF; loadfile inherits that
                        // paused state, so the new song would silently sit at 0:00 without
                        // this explicit unpause.
                        mpv_write(&mut writer, json!({"command": ["set_property", "pause", false]}));
                        let mut s = state.lock().unwrap();
                        s.loading = true;
                        s.paused  = false;
                        s.elapsed = 0.0;
                        s.total   = 0.0;
                        s.error   = None;
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

                    Cmd::Pause   => { log::debug!("[audio] Pause");       mpv_write(&mut writer, json!({"command": ["set_property", "pause", true]})); }
                    Cmd::Resume  => { log::debug!("[audio] Resume");      mpv_write(&mut writer, json!({"command": ["set_property", "pause", false]})); }
                    Cmd::Seek(d) => { log::debug!("[audio] Seek {d:+}s"); mpv_write(&mut writer, json!({"command": ["seek", d, "relative"]})); }
                    Cmd::Volume(v)=>{ log::debug!("[audio] Volume {v}");  mpv_write(&mut writer, json!({"command": ["set_property", "volume", v]})); }
                    Cmd::Stop    => { log::debug!("[audio] Stop");        mpv_write(&mut writer, json!({"command": ["stop"]})); }
                }
            }
        }

        // ── events from mpv ──────────────────────────────────────────────────
        let mut mpv_dead = false;
        loop {
            match ev_rx.try_recv() {
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => { mpv_dead = true; break; }
                Ok(ev) => match ev["event"].as_str().unwrap_or("") {
                    "property-change" => {
                        let mut s = state.lock().unwrap();
                        match ev["name"].as_str().unwrap_or("") {
                            "time-pos" => { if let Some(v) = ev["data"].as_f64() { s.elapsed = v; } }
                            "pause"    => { if let Some(v) = ev["data"].as_bool() { s.paused  = v; } }
                            "duration" => {
                                if let Some(v) = ev["data"].as_f64() {
                                    log::info!("[audio] duration: {v:.1}s");
                                    s.total   = v;
                                    s.loading = false;
                                    pending_resolve = None; // mpv buffered; upgrade no longer useful
                                }
                            }
                            _ => {}
                        }
                    }
                    "start-file"  => {
                        log::info!("[audio] start-file");
                        let mut s = state.lock().unwrap();
                        s.loading = true;
                        s.elapsed = 0.0;
                    }
                    "end-file" => {
                        let reason = ev["reason"].as_str().unwrap_or("?");
                        log::info!("[audio] end-file: reason={reason}");
                        let mut s = state.lock().unwrap();
                        s.loading = false;
                        if reason == "eof" { s.song_ended = true; }
                    }
                    "file-loaded" => { log::info!("[audio] file-loaded"); }
                    _ => {}
                }
            }
        }

        if mpv_dead {
            log::warn!("[audio] mpv reader exited — mpv crashed or closed");
            let _ = child.wait();
            std::fs::remove_file(&socket).ok();
            return;
        }

        thread::sleep(Duration::from_millis(50));
    }
}
