//! Does a console process get System Media Transport Controls at all?
//!
//! The whole Windows backend rests on one question: can a plain Win32 console
//! app — no window, no message pump — publish an SMTC session? The documented
//! Win32 route is `ISystemMediaTransportControlsInterop::GetForWindow`, which
//! wants an HWND and a thread pumping messages for it. The route tried here is
//! the other one Microsoft documents for manual SMTC control: create a
//! `MediaPlayer`, take the controls it owns, and disable the automatic
//! integration that would otherwise have it drive playback we never route
//! through it.
//!
//! Run it, then press a volume key to bring up the media flyout:
//!
//! ```text
//! cargo run -p ytm-core --example smtc_probe
//! ```
//!
//! What it proves, if it works: the flyout names the track, the buttons are
//! live, the seek bar tracks a position we never actually play, and the media
//! keys reach us with the terminal unfocused. It prints every button press.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("smtc_probe is Windows-only.");
}

#[cfg(target_os = "windows")]
fn main() -> windows::core::Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
    use windows::Media::Playback::MediaPlayer;
    use windows::Media::{
        AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
        MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
        ShuffleEnabledChangeRequestedEventArgs, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    };
    use windows::Storage::Streams::RandomAccessStreamReference;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::HSTRING;

    const LENGTH: f64 = 213.0;

    fn ticks(secs: f64) -> TimeSpan {
        // WinRT durations are in 100 ns units.
        TimeSpan {
            Duration: (secs * 1e7) as i64,
        }
    }

    // WinRT needs an apartment. MTA, because the events arrive on threadpool
    // threads and nothing here owns a window.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    let player = MediaPlayer::new()?;
    // Without this the player tries to be the thing the buttons control, and
    // it has nothing to play.
    player.CommandManager()?.SetIsEnabled(false)?;
    let smtc: SystemMediaTransportControls = player.SystemMediaTransportControls()?;

    let (tx, rx) = mpsc::channel();
    let button_tx = tx.clone();
    smtc.ButtonPressed(&TypedEventHandler::<
        SystemMediaTransportControls,
        SystemMediaTransportControlsButtonPressedEventArgs,
    >::new(move |_, args| {
        let name = match args.ok()?.Button()? {
            SystemMediaTransportControlsButton::Play => "Play",
            SystemMediaTransportControlsButton::Pause => "Pause",
            SystemMediaTransportControlsButton::Stop => "Stop",
            SystemMediaTransportControlsButton::Next => "Next",
            SystemMediaTransportControlsButton::Previous => "Previous",
            _ => "other",
        };
        let _ = button_tx.send(format!("button {name}"));
        Ok(())
    }))?;

    // Only raised once the timeline carries a seekable range — that is the
    // thing worth proving, since it is what a drag on the flyout's bar sends.
    let seek_tx = tx.clone();
    smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<
        SystemMediaTransportControls,
        PlaybackPositionChangeRequestedEventArgs,
    >::new(move |_, args| {
        let pos = args.ok()?.RequestedPlaybackPosition()?.Duration as f64 / 1e7;
        let _ = seek_tx.send(format!("seek to {pos:.1}s"));
        Ok(())
    }))?;

    let shuffle_tx = tx.clone();
    smtc.ShuffleEnabledChangeRequested(&TypedEventHandler::<
        SystemMediaTransportControls,
        ShuffleEnabledChangeRequestedEventArgs,
    >::new(move |_, args| {
        let on = args.ok()?.RequestedShuffleEnabled()?;
        let _ = shuffle_tx.send(format!("shuffle {on}"));
        Ok(())
    }))?;

    let repeat_tx = tx;
    smtc.AutoRepeatModeChangeRequested(&TypedEventHandler::<
        SystemMediaTransportControls,
        AutoRepeatModeChangeRequestedEventArgs,
    >::new(move |_, args| {
        let mode = args.ok()?.RequestedAutoRepeatMode()?;
        let _ = repeat_tx.send(format!("repeat {mode:?}"));
        Ok(())
    }))?;

    smtc.SetIsPlayEnabled(true)?;
    smtc.SetIsPauseEnabled(true)?;
    smtc.SetIsStopEnabled(true)?;
    smtc.SetIsNextEnabled(true)?;
    smtc.SetIsPreviousEnabled(true)?;
    // Each of these has to be set once before its change-requested event is
    // ever raised.
    smtc.SetShuffleEnabled(false)?;
    smtc.SetAutoRepeatMode(MediaPlaybackAutoRepeatMode::List)?;
    smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;

    let updater = smtc.DisplayUpdater()?;
    updater.SetType(MediaPlaybackType::Music)?;
    let music = updater.MusicProperties()?;
    music.SetTitle(&HSTRING::from("Never Gonna Give You Up"))?;
    music.SetArtist(&HSTRING::from("Rick Astley"))?;
    music.SetAlbumTitle(&HSTRING::from("Whenever You Need Somebody"))?;
    // A remote https URI: the shell fetches it, so no image bytes cross this
    // process. Whether that works is the other thing worth proving.
    updater.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(
        &Uri::CreateUri(&HSTRING::from(
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg",
        ))?,
    )?)?;
    updater.Update()?;

    smtc.SetIsEnabled(true)?;

    println!("SMTC published. Press a volume key for the media flyout.");
    println!("Media keys and flyout buttons print here. 60 s, then it tears down.\n");

    let start = Instant::now();
    let mut sent_timeline = Instant::now() - Duration::from_secs(10);
    while start.elapsed() < Duration::from_secs(60) {
        while let Ok(msg) = rx.try_recv() {
            println!("  ← {msg}");
        }
        // Only every 5 s, which is what the guidance asks for — and enough for
        // the bar to visibly move.
        if sent_timeline.elapsed() >= Duration::from_secs(5) {
            let position = start.elapsed().as_secs_f64() % LENGTH;
            let props = SystemMediaTransportControlsTimelineProperties::new()?;
            props.SetStartTime(ticks(0.0))?;
            props.SetMinSeekTime(ticks(0.0))?;
            props.SetPosition(ticks(position))?;
            props.SetMaxSeekTime(ticks(LENGTH))?;
            props.SetEndTime(ticks(LENGTH))?;
            smtc.UpdateTimelineProperties(&props)?;
            sent_timeline = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // What the real backend does on the way out: the session has to leave the
    // flyout before the process does.
    smtc.SetIsEnabled(false)?;
    updater.ClearAll()?;
    updater.Update()?;
    player.Close()?;
    println!("\nTorn down.");
    Ok(())
}
