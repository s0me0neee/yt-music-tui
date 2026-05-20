use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

#[allow(dead_code)]
pub enum Cmd {
    Play(String),     // video_id
    Prefetch(String), // video_id — resolve URL in background for faster future Play
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
    pub song_ended: bool, // set on natural eof; app must reset after reading
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
        // Disconnect the channel — the audio thread detects this and kills mpv.
        drop(self.cmd_tx.take());
        // Wait for the audio thread to finish so mpv is dead before we return.
        if let Some(handle) = self.audio_thread.take() {
            let _ = handle.join();
        }
    }
}

// ── URL pre-resolution ────────────────────────────────────────────────────────

/// Calls yt-dlp to get a direct CDN audio URL for the given video ID.
/// Returns None on failure (audio thread falls back to YouTube URL so mpv handles it).
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
        .output()
        .ok()?;

    if !out.status.success() {
        log::warn!("[audio] yt-dlp resolve failed for {video_id}: {}",
            String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let url = stdout.trim().lines().next()?.to_string();
    if url.starts_with("http") { Some(url) } else { None }
}

fn mpv_write(w: &mut impl Write, msg: Value) {
    let mut s = msg.to_string();
    s.push('\n');
    if let Err(e) = w.write_all(s.as_bytes()) {
        log::error!("[audio] write to mpv socket failed: {e}");
    }
}

// ── audio thread ──────────────────────────────────────────────────────────────

fn run(rx: std::sync::mpsc::Receiver<Cmd>, state: Arc<Mutex<AudioState>>) {
    // ── dependency checks ────────────────────────────────────────────────────
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
    log::info!("[audio] IPC socket path: {socket}");

    // ── spawn mpv ────────────────────────────────────────────────────────────
    log::info!("[audio] spawning mpv…");
    let child = Command::new("mpv")
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
        .spawn();

    let mut _child = match child {
        Ok(c) => { log::info!("[audio] mpv spawned (pid {})", c.id()); c }
        Err(e) => {
            let msg = format!("mpv spawn failed: {e} — brew install mpv");
            log::error!("[audio] {msg}");
            state.lock().unwrap().error = Some(msg);
            return;
        }
    };

    // ── wait for IPC socket ───────────────────────────────────────────────────
    log::info!("[audio] waiting for IPC socket…");
    let stream = {
        let mut conn = None;
        for attempt in 0..40 {
            thread::sleep(Duration::from_millis(50));
            match UnixStream::connect(&socket) {
                Ok(s) => {
                    log::info!("[audio] IPC connected after {}ms", (attempt + 1) * 50);
                    conn = Some(s);
                    break;
                }
                Err(e) if attempt == 39 => log::error!("[audio] IPC socket never appeared: {e}"),
                _ => {}
            }
        }
        match conn {
            Some(s) => s,
            None => {
                state.lock().unwrap().error = Some("mpv IPC unavailable".into());
                let _ = _child.kill();
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

    // ── reader thread ─────────────────────────────────────────────────────────
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Value>();
    thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&l) { let _ = ev_tx.send(v); }
                }
                Err(e) => { log::error!("[audio] mpv reader: {e}"); break; }
            }
        }
        log::warn!("[audio] reader thread exited");
    });

    // URL cache: video_id → direct CDN URL (populated by Prefetch)
    let mut url_cache: HashMap<String, String> = HashMap::new();
    let mut fetching:  HashSet<String>          = HashSet::new();
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<(String, Option<String>)>();

    // ── main loop ─────────────────────────────────────────────────────────────
    loop {
        // Drain completed prefetch results into cache
        while let Ok((id, maybe_url)) = fetch_rx.try_recv() {
            fetching.remove(&id);
            match maybe_url {
                Some(url) => {
                    log::info!("[audio] prefetch done: cached URL for {id}");
                    url_cache.insert(id, url);
                }
                None => log::debug!("[audio] prefetch failed for {id} — will resolve at play time"),
            }
        }

        // Commands from UI — also watch for channel disconnect (AudioEngine dropped)
        loop {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("[audio] channel disconnected — killing mpv and exiting");
                    let _ = _child.kill();
                    let _ = _child.wait();
                    return;
                }
                Ok(cmd) => match cmd {
                Cmd::Play(id) => {
                    // Use cached direct URL if available (instant start), else fall back
                    // to the YouTube URL and let mpv invoke yt-dlp itself
                    let url = if let Some(cached) = url_cache.get(&id) {
                        log::info!("[audio] Play {id}: cache HIT — direct CDN URL");
                        cached.clone()
                    } else {
                        log::info!("[audio] Play {id}: cache miss — YouTube URL (mpv resolves)");
                        format!("https://music.youtube.com/watch?v={id}")
                    };
                    mpv_write(&mut writer, json!({"command": ["loadfile", url, "replace"]}));
                    let mut s = state.lock().unwrap();
                    s.loading = true;
                    s.elapsed = 0.0;
                    s.total   = 0.0;
                    s.error   = None;
                }
                Cmd::Prefetch(id) => {
                    if url_cache.contains_key(&id) || fetching.contains(&id) {
                        continue; // already cached or in-flight
                    }
                    fetching.insert(id.clone());
                    let tx = fetch_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send((id.clone(), resolve_url(&id)));
                    });
                }
                Cmd::Pause   => { log::debug!("[audio] Pause");       mpv_write(&mut writer, json!({"command": ["set_property", "pause", true]})); }
                Cmd::Resume  => { log::debug!("[audio] Resume");      mpv_write(&mut writer, json!({"command": ["set_property", "pause", false]})); }
                Cmd::Seek(d) => { log::debug!("[audio] Seek {d:+}s"); mpv_write(&mut writer, json!({"command": ["seek", d, "relative"]})); }
                Cmd::Volume(v)=>{ log::debug!("[audio] Volume {v}");  mpv_write(&mut writer, json!({"command": ["set_property", "volume", v]})); }
                Cmd::Stop    => { log::debug!("[audio] Stop");        mpv_write(&mut writer, json!({"command": ["stop"]})); }
                }  // end match cmd
            }  // end match rx.try_recv()
        }  // end loop

        // Events from mpv; Disconnected means reader thread exited (mpv crashed/closed)
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
                        "pause"    => { if let Some(v) = ev["data"].as_bool() { s.paused = v; } }
                        "duration" => {
                            if let Some(v) = ev["data"].as_f64() {
                                log::info!("[audio] duration: {v:.1}s");
                                s.total   = v;
                                s.loading = false;
                            }
                        }
                        _ => {}
                    }
                }
                "start-file"  => { log::info!("[audio] start-file");  let mut s = state.lock().unwrap(); s.loading = true;  s.elapsed = 0.0; }
                "end-file"    => {
                    let reason = ev["reason"].as_str().unwrap_or("?");
                    log::info!("[audio] end-file: reason={reason}");
                    let mut s = state.lock().unwrap();
                    s.loading = false;
                    if reason == "eof" { s.song_ended = true; }
                }
                "file-loaded" => { log::info!("[audio] file-loaded"); }
                _ => {}
                }  // end match ev["event"]
            }  // end match ev_rx.try_recv()
        }  // end loop
        if mpv_dead {
            log::warn!("[audio] mpv reader exited — mpv crashed or was closed externally");
            let _ = _child.wait();
            return;
        }

        thread::sleep(Duration::from_millis(50));
    }
}
