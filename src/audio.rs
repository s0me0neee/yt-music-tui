use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

pub enum Cmd {
    Play(String), // video_id
    Pause,
    Resume,
    Seek(i64),    // relative seconds
    Volume(u8),   // 0-100
    Stop,
}

#[derive(Clone, Default)]
pub struct AudioState {
    pub elapsed: f64,
    pub total:   f64,
    pub paused:  bool,
    pub loading: bool,
    pub error:   Option<String>,
}

pub struct AudioEngine {
    cmd_tx:     std::sync::mpsc::Sender<Cmd>,
    pub state:  Arc<Mutex<AudioState>>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state    = Arc::new(Mutex::new(AudioState::default()));
        let state2   = Arc::clone(&state);
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(rx, state2))
            .expect("spawn audio thread");
        Self { cmd_tx: tx, state }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

// ── mpv helpers ─────────────────────────────────────────────────────────────

fn mpv_write(w: &mut impl Write, msg: Value) {
    let mut s = msg.to_string();
    s.push('\n');
    let _ = w.write_all(s.as_bytes());
}

// ── audio thread ─────────────────────────────────────────────────────────────

fn run(rx: std::sync::mpsc::Receiver<Cmd>, state: Arc<Mutex<AudioState>>) {
    let socket = format!("/tmp/yt-tui-{}.sock", std::process::id());

    // Spawn mpv in idle mode with IPC socket.
    // --ytdl-format selects Opus (webm) > AAC (m4a) for best quality.
    // --gapless-audio enables gapless playback between tracks.
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
            &format!("--input-ipc-server={socket}"),
            "--idle=yes",
            "--keep-open=yes",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let mut _child = match child {
        Ok(c) => c,
        Err(e) => {
            state.lock().unwrap().error = Some(format!("mpv: {e}"));
            return;
        }
    };

    // Wait for the IPC socket to appear (up to 2 s)
    let stream = {
        let mut conn = None;
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(50));
            if let Ok(s) = UnixStream::connect(&socket) {
                conn = Some(s);
                break;
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

    // Observe the three properties we care about
    for (id, prop) in [(1u32, "time-pos"), (2, "pause"), (3, "duration")] {
        mpv_write(&mut writer, json!({"command": ["observe_property", id, prop]}));
    }

    // Spawn a reader thread that forwards mpv JSON lines over a channel
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Value>();
    thread::spawn(move || {
        for line in reader.lines() {
            if let Ok(l) = line {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    let _ = ev_tx.send(v);
                }
            }
        }
    });

    loop {
        // ── Commands from UI ────────────────────────────────────────────────
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Play(id) => {
                    let url = format!("https://music.youtube.com/watch?v={id}");
                    mpv_write(&mut writer, json!({"command": ["loadfile", url, "replace"]}));
                    let mut s  = state.lock().unwrap();
                    s.loading  = true;
                    s.elapsed  = 0.0;
                    s.total    = 0.0;
                    s.error    = None;
                }
                Cmd::Pause    => mpv_write(&mut writer, json!({"command": ["set_property", "pause", true]})),
                Cmd::Resume   => mpv_write(&mut writer, json!({"command": ["set_property", "pause", false]})),
                Cmd::Seek(d)  => mpv_write(&mut writer, json!({"command": ["seek", d, "relative"]})),
                Cmd::Volume(v)=> mpv_write(&mut writer, json!({"command": ["set_property", "volume", v]})),
                Cmd::Stop     => mpv_write(&mut writer, json!({"command": ["stop"]})),
            }
        }

        // ── Events from mpv ─────────────────────────────────────────────────
        while let Ok(ev) = ev_rx.try_recv() {
            match ev["event"].as_str().unwrap_or("") {
                "property-change" => {
                    let mut s = state.lock().unwrap();
                    match ev["name"].as_str().unwrap_or("") {
                        "time-pos"  => { if let Some(v) = ev["data"].as_f64() { s.elapsed = v; } }
                        "pause"     => { if let Some(v) = ev["data"].as_bool() { s.paused = v; } }
                        "duration"  => {
                            if let Some(v) = ev["data"].as_f64() {
                                s.total   = v;
                                s.loading = false;
                            }
                        }
                        _ => {}
                    }
                }
                "start-file" => { let mut s = state.lock().unwrap(); s.loading = true;  s.elapsed = 0.0; }
                "end-file"   => { state.lock().unwrap().loading = false; }
                _ => {}
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}
