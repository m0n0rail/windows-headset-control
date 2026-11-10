//! Tray application entry point.
//!
//! `windows_subsystem = "windows"` so launching it does not flash a console.
//! `--install` and `--uninstall` still need to say something, so they attach to
//! the parent console when there is one and fall back to a message box when the
//! user double-clicked the exe.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("headset-tray targets Windows only");

#[cfg(windows)]
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "--install" => run_install(),
        "--uninstall" => run_uninstall(),
        "--render-panel" => run_render_panel(),
        "--export-icon" => run_export_icon(),
        "--explain-output" => run_explain_output(),
        "--help" | "-h" | "/?" => report(
            "Headset Tray",
            "Usage:\n  headset-tray.exe              run the tray\n  \
             headset-tray.exe --install    install and run at logon\n  \
             headset-tray.exe --uninstall  remove startup and the uninstall entry\n  \
             headset-tray.exe --explain-output   say why the output switch is or is not working\n  \
             headset-tray.exe --export-icon <path.ico>   regenerate the application icon",
        ),
        _ => run_tray(),
    }
}

#[cfg(windows)]
fn run_install() {
    use headset_tray::install;
    match install::install() {
        Ok(path) => {
            // Launch the installed copy, not this one: if --install was run from
            // a build directory, the running image is the wrong binary to leave
            // resident. Report a spawn failure rather than discarding it -- a
            // silent failure here looks exactly like a successful install that
            // mysteriously did not start.
            // Detach the child's stdio explicitly. Inheriting it makes the spawn
            // depend on console state: `report` may call FreeConsole, which
            // invalidates inherited console handles and makes CreateProcess fail
            // with a confusing error. Null stdio removes the coupling entirely
            // rather than relying on the order these two run in.
            let started = std::process::Command::new(&path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let note = match started {
                Ok(_) => "Starting it now.".to_string(),
                Err(e) => format!("Could not start it now ({e}); it will start at next sign-in."),
            };
            report(
                "Headset Tray installed",
                &format!(
                    "Installed to:\n{}\n\nIt will start automatically when you sign in.\n{note}",
                    path.display()
                ),
            );
        }
        Err(e) => report(
            "Headset Tray install failed",
            &format!("Could not install: {e}"),
        ),
    }
}

#[cfg(windows)]
fn run_uninstall() {
    use headset_tray::install;
    match install::uninstall() {
        Ok(dir) => {
            let where_ = dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the install folder".into());
            report(
                "Headset Tray uninstalled",
                &format!(
                    "Removed the startup entry, the uninstall registration, and \
                     stopped the running tray.\n\n{where_}\nis being deleted now \
                     - a moment is needed for this program to exit first.",
                ),
            );
        }
        Err(e) => report(
            "Headset Tray uninstall failed",
            &format!("Could not uninstall: {e}"),
        ),
    }
}

/// Writes the multi-size icon to a path. Run when the drawing changes; the
/// result is committed and embedded as a resource.
#[cfg(windows)]
fn run_export_icon() {
    use headset_tray::ui::icon::{encode_ico, ICON_SIZES};

    let Some(path) = std::env::args().nth(2) else {
        report(
            "Export icon",
            "Usage: headset-tray.exe --export-icon <path.ico>",
        );
        return;
    };
    match std::fs::write(&path, encode_ico(&ICON_SIZES)) {
        Ok(()) => report("Export icon", &format!("Wrote {path}")),
        Err(e) => report("Export icon", &format!("Could not write {path}: {e}")),
    }
}

/// Explains what the output switch would do, and why, without doing it.
///
/// The switch acts on a device event nobody can stage on demand — it needs a
/// power button held down — and it acts on facts the user cannot see: which
/// endpoint is default, which endpoint ids still exist, and what the tray
/// recorded as the way back. When it does nothing, every one of those is a
/// candidate, and until this existed the only way to tell them apart was to
/// read the registry by hand.
///
/// Read-only: it asks `plan_output` what it would decide for a powered-off and
/// a powered-on headset and prints both, rather than moving anything.
#[cfg(windows)]
fn run_explain_output() {
    use headset_tray::{output::Action, settings, win32, Slot};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    // Core Audio needs an initialised apartment; the tray does this in its own
    // startup, and this path does not go through it.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let devices = headset_tray::output_devices();
    let name_of = |id: &str| {
        devices
            .iter()
            .find(|(d, _)| d == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| format!("{id} (no longer on this machine)"))
    };

    let mut out = Vec::new();
    out.push(format!(
        "Switch output when off: {}",
        if settings::switch_output_when_off() {
            "on"
        } else {
            "off"
        }
    ));
    out.push(match settings::output_choice(Slot::Fallback) {
        Some((id, name)) => format!("Play through:            {name}\n  {}", name_of(&id)),
        None => "Play through:            nothing chosen".to_string(),
    });
    out.push(format!(
        "Split game and chat:     {}",
        if settings::split_game_and_chat() {
            "on"
        } else {
            "off"
        }
    ));
    for (slot, label) in [
        (Slot::Game, "Game channel:           "),
        (Slot::Chat, "Chat channel:           "),
    ] {
        out.push(match settings::output_choice(slot) {
            Some((id, name)) => format!("{label} {name}\n  {}", name_of(&id)),
            None => format!("{label} nothing chosen"),
        });
    }
    out.push(match settings::restore_output() {
        Some(id) => format!("Owed a move back to:     {}", name_of(&id)),
        None => "Owed a move back to:     nothing".to_string(),
    });
    out.push(match settings::output_problem() {
        Some(p) => format!("Last problem:            {}", p.short()),
        None => "Last problem:            none recorded".to_string(),
    });

    out.push("\nPlayback devices Windows currently has:".into());
    for (_, name) in &devices {
        out.push(format!("  {name}"));
    }

    let describe = |a: Action| match a {
        Action::Nothing => "nothing to do".to_string(),
        Action::MoveAway { to, owing } => format!(
            "move the sound to {}, remembering {}",
            name_of(&to),
            name_of(&owing)
        ),
        Action::MoveBack { to } => format!("move the sound back to {}", name_of(&to)),
        Action::Split { game, chat } => format!(
            "play through {}, and take calls on {}",
            name_of(&game),
            name_of(&chat)
        ),
        Action::Retry => "wait for the endpoint to reappear".to_string(),
        Action::GiveUp => "give up on the endpoint owed, and say so".to_string(),
        Action::Blocked(p) => format!("nothing — {}", p.detail()),
    };
    out.push("\nIf the headset powered off right now:".into());
    out.push(format!(
        "  {}",
        describe(win32::plan_output(Some(false), 0))
    ));
    out.push("If the headset powered on right now:".into());
    out.push(format!("  {}", describe(win32::plan_output(Some(true), 0))));

    report("Headset Tray output switch", &out.join("\n"));
}

/// Renders panel states to PNGs so appearance can be diffed against the
/// mockups without a window or a headset.
///
/// `--render-panel <dir> [state...]`, where each state names one of the fixtures
/// below. With no states, renders all of them.
#[cfg(windows)]
fn run_render_panel() {
    use headset_protocol::v3pro::noise::{NoiseControl, NoiseMode};
    use headset_tray::state::HeadsetState;
    use headset_tray::ui::{self, layout::SliderParam, View};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    let args: Vec<String> = std::env::args().skip(2).collect();
    let dir = args.first().cloned().unwrap_or_else(|| ".".to_string());
    let _ = std::fs::create_dir_all(&dir);

    let base = HeadsetState {
        device_name: Some("BlackShark V3 Pro PS HID".into()),
        connected: Some(true),
        battery: Some(49),
        sidetone: Some(0),
        game_chat: Some(10),
        noise: Some(NoiseControl {
            mode: NoiseMode::Anc,
            anc_level: 3,
        }),
        mic_mute_hardware: Some(false),
        mic_mute_os: Some(false),
        warn_vendor_software: true,
        dongle_silent: false,
    };

    let mut cases: Vec<(&str, HeadsetState, View, SliderParam)> = Vec::new();
    cases.push((
        "live-gamechat-balanced",
        base.clone(),
        View::Main,
        SliderParam::GameChat,
    ));

    let mut muted = base.clone();
    muted.mic_mute_os = Some(true);
    cases.push((
        "muted-gamechat-balanced",
        muted.clone(),
        View::Main,
        SliderParam::GameChat,
    ));

    let mut gc17 = muted.clone();
    gc17.game_chat = Some(17);
    cases.push(("muted-gamechat-17", gc17, View::Main, SliderParam::GameChat));

    let mut st0 = muted.clone();
    st0.sidetone = Some(0);
    cases.push(("muted-sidetone-off", st0, View::Main, SliderParam::Sidetone));

    let mut st14 = muted.clone();
    st14.sidetone = Some(14);
    cases.push(("muted-sidetone-14", st14, View::Main, SliderParam::Sidetone));

    let mut nowarn = base.clone();
    nowarn.warn_vendor_software = false;
    nowarn.sidetone = Some(14);
    cases.push((
        "live-sidetone-14-nobanner",
        nowarn,
        View::Main,
        SliderParam::Sidetone,
    ));

    // The wedged dongle: the control channel is open and answering nothing, so
    // every proxied value is unknown and the header carries the remedy.
    let silent = HeadsetState {
        device_name: base.device_name.clone(),
        connected: None,
        battery: None,
        sidetone: None,
        game_chat: None,
        noise: None,
        mic_mute_hardware: None,
        mic_mute_os: None,
        warn_vendor_software: false,
        dongle_silent: true,
    };
    cases.push((
        "dongle-not-responding",
        silent,
        View::Main,
        SliderParam::GameChat,
    ));

    cases.push((
        "settings",
        base.clone(),
        View::Settings,
        SliderParam::GameChat,
    ));

    // The output picker. Its list comes from the machine this runs on, so the
    // rendered height varies with how many playback devices are attached --
    // which is the point of rendering it at all.
    for (name, slot) in [
        ("output-picker", headset_tray::Slot::Fallback),
        ("output-picker-game", headset_tray::Slot::Game),
        ("output-picker-chat", headset_tray::Slot::Chat),
    ] {
        cases.push((
            name,
            base.clone(),
            View::Output(slot),
            SliderParam::GameChat,
        ));
    }

    // The three noise modes, so the segment row and the dimmed level track can
    // be diffed the same way every other state is.
    for (name, mode) in [
        ("live-noise-off", NoiseMode::Off),
        ("live-noise-ambient", NoiseMode::Ambient),
    ] {
        let mut s = base.clone();
        s.noise = Some(NoiseControl { mode, anc_level: 3 });
        cases.push((name, s, View::Main, SliderParam::GameChat));
    }

    let mut anc1 = base.clone();
    anc1.noise = Some(NoiseControl {
        mode: NoiseMode::Anc,
        anc_level: 1,
    });
    cases.push(("live-noise-anc-1", anc1, View::Main, SliderParam::GameChat));

    // The high-contrast palette, so it can be diffed like every other state
    // rather than being taken on trust. Rendered from the same fixture as the
    // first case, so the two files differ only by palette.
    cases.push((
        "high-contrast",
        base.clone(),
        View::Main,
        SliderParam::GameChat,
    ));

    // The light palette, on both views.
    cases.push((
        "light-main",
        base.clone(),
        View::Main,
        SliderParam::GameChat,
    ));
    cases.push((
        "light-settings",
        base.clone(),
        View::Settings,
        SliderParam::GameChat,
    ));

    let mut off = base.clone();
    off.connected = Some(false);
    off.battery = None;
    off.sidetone = None;
    off.game_chat = None;
    off.noise = None;
    off.mic_mute_hardware = None;
    cases.push(("disconnected", off, View::Main, SliderParam::GameChat));

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let renderer = match ui::render::Renderer::new() {
        Ok(r) => r,
        Err(e) => {
            report(
                "Render failed",
                &format!("Could not create the renderer: {e}"),
            );
            return;
        }
    };

    let mut lines = Vec::new();
    for (name, state, view, param) in cases {
        if args.len() > 1 && !args[1..].iter().any(|a| a == name) {
            continue;
        }
        // One fixture exercises the accessibility palette; the rest are the
        // sampled one, so the flag is set per case rather than once.
        // One fixture per palette, so each is diffable. Everything else renders
        // in the sampled dark palette, and those files must stay byte-identical.
        headset_tray::ui::theme::set_palette(if name == "high-contrast" {
            headset_tray::ui::theme::Which::HighContrast
        } else if name.starts_with("light-") {
            headset_tray::ui::theme::Which::Light
        } else {
            headset_tray::ui::theme::Which::Dark
        });
        let panel = ui::build(&state, view, param, None);
        match renderer.render(&panel, 1.0) {
            Ok(img) => {
                let path = format!("{dir}\\{name}.png");
                match renderer.save_png(&img, &path) {
                    Ok(()) => lines.push(format!("{name}: {}x{}", img.width, img.height)),
                    Err(e) => lines.push(format!("{name}: save failed: {e}")),
                }
            }
            Err(e) => lines.push(format!("{name}: render failed: {e}")),
        }
    }
    report("Rendered panels", &lines.join("\n"));
}

#[cfg(windows)]
fn run_tray() {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    use headset_tray::{install, state::HeadsetState, win32, worker};

    tracing_subscriber_init();

    // Bound to a named local: this must outlive the message loop. `let _ = ...`
    // would drop it here and defeat the whole guard. Claimed before anything
    // else starts, so a second instance never opens a ControlSession.
    let _instance = match win32::claim_single_instance() {
        win32::SingleInstance::Claimed(guard) => guard,
        win32::SingleInstance::AlreadyRunning => {
            win32::signal_existing_instance();
            return;
        }
    };

    // A previous --install may have parked the old image alongside the new one;
    // by now it is no longer running, so it can go.
    install::tidy_previous_upgrade();

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(HeadsetState::default()));

    // The worker starts before the window exists, so it needs somewhere to
    // learn the handle once there is one. Until then its notifications are
    // dropped, which is correct: there is no UI to repaint yet.
    let hwnd = Arc::new(AtomicIsize::new(0));

    let worker_state = state.clone();
    let worker_hwnd = hwnd.clone();
    let device_thread = std::thread::spawn(move || {
        let backend = headset_device::WindowsHidBackend::new();
        worker::run(&backend, rx, |s| {
            if let Ok(mut guard) = worker_state.lock() {
                // Merge, never replace: `mic_mute_os` and `warn_vendor_software`
                // belong to the UI thread and the worker has no values for them.
                guard.apply_device_snapshot(&s);
            }
            win32::post_state(worker_hwnd.load(Ordering::Relaxed));
        });
    });

    // Seed the OS-side mute before the first paint so the menu does not open
    // showing "unknown" for something we can read synchronously.
    if let Ok(m) = win32::get_mic_mute() {
        if let Ok(mut guard) = state.lock() {
            guard.mic_mute_os = Some(m);
        }
    }

    if let Err(e) = win32::run_ui_with(tx.clone(), state, |h| {
        hwnd.store(h, Ordering::Relaxed);
    }) {
        report("Headset Tray failed to start", &format!("{e}"));
    }

    let _ = tx.send(worker::Command::Shutdown);
    let _ = device_thread.join();
}

/// Prints to the parent console if there is one, otherwise shows a message box.
///
/// A `windows_subsystem = "windows"` binary has no console of its own, so
/// running `--install` from a terminal would otherwise produce silence.
///
/// `AttachConsole` alone is not enough: it gives the process a console, but the
/// standard-output handle was already resolved (to nothing) at startup, so
/// `println!` still goes nowhere. Writing to `CONOUT$` directly is what actually
/// reaches the terminal.
#[cfg(windows)]
fn report(title: &str, body: &str) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let text = format!("{title}\n\n{body}\n");

    // Case 1: stdout is already a real handle -- the caller redirected us to a
    // file or a pipe. Ordinary printing works and is what they asked for.
    let inherited = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if matches!(inherited, Ok(h) if !h.is_invalid() && h.0 as usize != 0) {
        use std::io::Write as _;
        print!("{text}");
        let _ = std::io::stdout().flush();
        return;
    }

    // Case 2: launched from a terminal that gave us no stdout of our own.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() {
        let name: Vec<u16> = "CONOUT$".encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        if let Ok(h) = handle {
            let bytes = text.as_bytes();
            let mut written = 0u32;
            unsafe {
                let _ = WriteFile(h, Some(bytes), Some(&mut written), None);
                let _ = CloseHandle(h);
            }
        }
        unsafe {
            let _ = FreeConsole();
        }
        return;
    }

    // No parent console: the user double-clicked, so a dialog is the only way
    // to say anything at all.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(body),
            &HSTRING::from(title),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(windows)]
fn tracing_subscriber_init() {
    // Logging is opt-in via HEADSET_TRAY_LOG; a tray app has no console by
    // default, so the default is to stay silent rather than write nowhere.
    if std::env::var("HEADSET_TRAY_LOG").is_ok() {
        tracing::subscriber::set_global_default(
            tracing_subscriber::FmtSubscriber::builder()
                .with_writer(std::io::stderr)
                .finish(),
        )
        .ok();
    }
}
