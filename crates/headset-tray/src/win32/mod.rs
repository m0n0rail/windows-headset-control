//! The only `unsafe` in this crate.
//!
//! Holds the shell notification icon, the popup menu, the message loop, and the
//! Core Audio calls for microphone mute. This module exists instead of a
//! tray-icon dependency: the phase's footprint rule is zero new crates, and a
//! second confined `unsafe` module was judged the better trade. Every other
//! module in `headset-tray` is safe Rust.
//!
//! Mute lives here rather than in the device layer because it is a USB Audio
//! Class control, not a vendor command — see `docs/device-research.md`. There is
//! no vendor set-mute to call.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, FALSE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HBITMAP};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eMultimedia, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage, HICON, ICONINFO, IDC_ARROW,
    MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, WINDOW_EX_STYLE, WM_ACTIVATE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DPICHANGED, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

// Not part of any public API: these are the tray's own window plumbing, and
// several are `unsafe` in ways only this module's call sites can uphold.
pub(crate) mod audio;
pub(crate) mod dpi;
pub(crate) mod panel;
pub(crate) mod place;

use crate::state::HeadsetState;
use crate::ui::{self, HitTarget, SliderParam, View};
use crate::worker::Command;
use headset_protocol::v3pro::noise::{NoiseControl, NoiseMode};

/// Tray icon callback.
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the worker thread when state changed.
pub const WM_STATE: u32 = WM_APP + 2;

/// Posted by a second instance to ask the first to show itself.
pub const WM_SHOW_PANEL: u32 = WM_APP + 3;

/// Sent when the icon is activated by keyboard, under notification version 4.
///
/// `NIN_SELECT` is in the `windows` crate; `NIN_KEYSELECT` is not, so it is
/// spelled out here. The shell defines it as `NIN_SELECT | 1` and sends it for
/// Enter or Space on a focused icon — the whole reason for moving to version 4.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

/// The shell's "I have restarted, re-add your icon" broadcast.
///
/// Registered at runtime rather than being a constant: `RegisterWindowMessageW`
/// allocates the value, and every process that registers the same string gets
/// the same number. Zero means registration failed, which must never match an
/// incoming message.
static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const ID_EXIT: usize = 1;
const ID_MUTE: usize = 2;
const ID_REFRESH: usize = 3;
const ID_STARTUP: usize = 4;
const ID_SYNAPSE_WARNING: usize = 5;
const ID_SIDETONE_BASE: usize = 100;
const ID_GAMECHAT_BASE: usize = 200;

const SIDETONE_MAX: u8 = 15;
const GAMECHAT_MAX: u8 = 20;

/// Timer that debounces the output switch.
const ID_OUTPUT_TIMER: usize = 1;

/// How long the link has to hold its new state before the sound is moved.
///
/// The link parameter carries no reason, so a deliberate power-off, an
/// auto-sleep, and a walk out of range are the same event to us. Two seconds is
/// what stops a momentary dropout from bouncing the whole machine's audio to
/// the speakers and straight back — which is far more disruptive than reacting
/// two seconds late to a real power-off.
const OUTPUT_SWITCH_DEBOUNCE_MS: u32 = 2000;

struct Ctx {
    icon: HICON,
    commands: Sender<Command>,
    state: Arc<Mutex<HeadsetState>>,
    nid: NOTIFYICONDATAW,
    /// The popup. Created once and shown/hidden, so its Direct2D resources and
    /// window handle survive across openings.
    panel_hwnd: HWND,
    renderer: Option<crate::ui::render::Renderer>,
    view: crate::ui::View,
    param: crate::ui::SliderParam,
    /// Hit regions from the last render, so a click can be resolved against
    /// what is actually on screen rather than a freshly computed guess.
    hits: Vec<(crate::ui::layout::Rect, crate::ui::HitTarget)>,
    track: Option<crate::ui::layout::TrackGeometry>,
    /// ANC level track from the last render. `None` in every mode but ANC.
    level_track: Option<crate::ui::layout::LevelTrack>,
    /// Render scale for the display the panel is on. Mouse coordinates arrive
    /// in physical pixels and `ui::layout` works in logical ones, so this is
    /// also the divisor for hit-testing.
    scale: f32,
    /// Value being dragged. Shown instead of the device's value until release,
    /// which is what makes one write per adjustment rather than twenty.
    drag: Option<u8>,
    /// Value released but not yet confirmed by the device, tagged with the
    /// parameter it belongs to. Held until the read-back arrives: dropping it at
    /// release made the knob snap back to the old value for the length of the
    /// write, because the panel then had nothing to draw but stale state.
    pending_slider: Option<(SliderParam, u8)>,
    /// Noise state asked for but not yet confirmed. Shown in place of the
    /// device's own until the read-back arrives, exactly as `drag` is for the
    /// slider.
    pending_noise: Option<NoiseControl>,
    panel_visible: bool,
    /// The link state the output switch has already reacted to. `None` until
    /// the first read lands, which is why a fresh start does not count as a
    /// transition and does not move anybody's sound.
    link: Option<bool>,
    /// How many times the endpoint owed a move back has been looked for since
    /// the last link change. Held in memory, unlike the record itself: the
    /// debt has to survive a restart, but a count of how long we have been
    /// waiting on this particular power-on does not.
    restore_attempts: u8,
    /// Endpoint ids in the order the picker last drew them, so a click resolves
    /// against that paint rather than a re-enumeration that may have shifted.
    output_ids: Vec<String>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Runs `f` with the tray context, or does nothing if we are already inside a
/// handler that holds it.
///
/// This guard is not optional. `ShowWindow`, `SetForegroundWindow` and
/// `UpdateLayeredWindow` dispatch messages **synchronously**, so a handler that
/// calls them re-enters this window procedure before returning. Holding a
/// `RefCell` borrow across that is a double-borrow panic, and with
/// `panic = "abort"` the process dies on the spot — which looks from outside
/// like the panel opening and then freezing, because a layered window's pixels
/// outlive the process that drew them.
///
/// Skipping the nested call is the correct behaviour, not a workaround: the
/// outer call is already mutating the same state, and the message that provoked
/// the re-entry was caused by us rather than by the user.
fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> Option<R> {
    CTX.with(|c| match c.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            tracing::debug!("ignoring a re-entrant window message");
            None
        }
    })
}

/// Holds the single-instance mutex for the life of the process.
///
/// Dropping this releases the claim, so it must be bound to a named local for
/// the whole run — `let _ = claim_single_instance()` would drop it immediately
/// and let a second instance straight in.
pub struct OwnedMutex(windows::Win32::Foundation::HANDLE);

impl Drop for OwnedMutex {
    fn drop(&mut self) {
        // May be null: the "could not create a mutex" branch below still hands
        // back an OwnedMutex so the caller has one code path, and closing a
        // null handle is an error rather than a no-op.
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub enum SingleInstance {
    Claimed(OwnedMutex),
    AlreadyRunning,
}

/// Claims the right to be the one running tray for this user session.
///
/// `Local\` rather than `Global\`: the tray is per-user and per-session by
/// design — it installs to `%LOCALAPPDATA%` and writes only to
/// `HKEY_CURRENT_USER` — and `Global\` would additionally block a second user
/// on the same machine from running their own.
pub fn claim_single_instance() -> SingleInstance {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        match CreateMutexW(None, TRUE, w!("Local\\HeadsetTray.SingleInstance")) {
            // The handle is returned even when the mutex already existed, so
            // the error code is what distinguishes the two, not the result.
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    SingleInstance::AlreadyRunning
                } else {
                    SingleInstance::Claimed(OwnedMutex(h))
                }
            }
            // Without a mutex there is no way to tell. Starting is the less
            // annoying failure: a duplicate icon beats refusing to run.
            Err(e) => {
                tracing::warn!("single-instance mutex unavailable: {e}");
                SingleInstance::Claimed(OwnedMutex(windows::Win32::Foundation::HANDLE::default()))
            }
        }
    }
}

/// Asks an already-running tray to show its panel, so a second launch does
/// something useful instead of nothing.
pub fn signal_existing_instance() {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        if let Ok(h) = FindWindowW(w!("HeadsetTrayWindow"), PCWSTR::null()) {
            let _ = PostMessageW(h, WM_SHOW_PANEL, WPARAM(0), LPARAM(0));
        }
    }
}

/// Runs the tray UI on the calling thread until the user exits.
///
/// Blocks. `state` is shared with the worker thread, which posts [`WM_STATE`]
/// after updating it. `on_window` is called once the window exists, so the
/// caller can hand the handle to that worker.
pub fn run_ui_with<F: FnOnce(isize)>(
    commands: Sender<Command>,
    state: Arc<Mutex<HeadsetState>>,
    on_window: F,
) -> windows::core::Result<()> {
    unsafe {
        // Before any window exists: the awareness context is fixed at first use.
        dpi::make_process_per_monitor_aware();
        apply_appearance();

        // Apartment-threaded because this thread also pumps messages, which is
        // what COM's STA contract expects.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let instance = GetModuleHandleW(None)?;
        let class = w!("HeadsetTrayWindow");
        // A class with a null hCursor never sets the pointer shape, so the
        // cursor keeps whatever it was when it entered -- which is the
        // app-starting hourglass for a freshly launched process. That is the
        // "loading cursor that never goes away" over the panel.
        let arrow = LoadCursorW(None, IDC_ARROW)?;

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: arrow,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let panel_class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: panel::CLASS_NAME,
            hCursor: arrow,
            ..Default::default()
        };
        if RegisterClassW(&panel_class) == 0 {
            return Err(windows::core::Error::from_win32());
        }

        // A real (never-shown) window rather than a message-only one:
        // TrackPopupMenu needs a foreground-capable owner to dismiss cleanly.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Headset Tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;

        // Registered before the icon is added: if the shell restarts between the
        // two, the broadcast still has somewhere to land.
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            std::sync::atomic::Ordering::Relaxed,
        );

        let icon = build_icon()?;
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        write_tip(&mut nid, "Headset - starting");
        if !add_icon(&mut nid) {
            return Err(windows::core::Error::from_win32());
        }
        set_icon_version(&nid);

        let panel_hwnd = panel::create(instance)?;
        panel::set_tag(panel_hwnd, panel::TAG_PANEL);

        // A failed renderer must not take the tray down with it: the icon,
        // tooltip and right-click menu still work without the panel.
        let renderer = match ui::render::Renderer::new() {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!("panel renderer unavailable: {e}");
                None
            }
        };

        CTX.with(|c| {
            *c.borrow_mut() = Some(Ctx {
                icon,
                commands,
                state,
                nid,
                panel_hwnd,
                renderer,
                view: View::Main,
                param: SliderParam::GameChat,
                hits: Vec::new(),
                track: None,
                level_track: None,
                scale: 1.0,
                drag: None,
                pending_slider: None,
                pending_noise: None,
                panel_visible: false,
                link: None,
                restore_attempts: 0,
                output_ids: Vec::new(),
            })
        });

        on_window(hwnd.0 as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

/// Copies text into the fixed 128-wchar tooltip buffer.
fn write_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    write_fixed(&mut nid.szTip, text);
}

/// Resolves the palette from the user's choice, Windows' preference, and high
/// contrast, and stores it for the next paint.
///
/// Called at startup and on every `WM_SETTINGCHANGE`, so a user on `AUTO` who
/// switches Windows to light sees the panel follow without reopening it.
fn apply_appearance() {
    crate::ui::theme::set_palette(crate::ui::theme::resolve(
        crate::settings::appearance(),
        crate::settings::windows_prefers_light(),
        high_contrast_enabled(),
    ));
}

/// Whether Windows is in high-contrast mode.
fn high_contrast_enabled() -> bool {
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    unsafe {
        let mut hc = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        let ok = SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            std::mem::size_of::<HIGHCONTRASTW>() as u32,
            Some(&mut hc as *mut HIGHCONTRASTW as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        ok.is_ok() && (hc.dwFlags & HCF_HIGHCONTRASTON).0 != 0
    }
}

/// Identifies this tray icon to the shell across restarts and reinstalls.
///
/// Arbitrary but permanent: the value itself means nothing, and changing it
/// discards every user preference attached to the icon. Do not regenerate it.
const ICON_GUID: GUID = GUID::from_u128(0x7f3a_2c14_9b6d_4e58_a1c2_5d90_e3b4_7f61);

/// Adds the notification icon, preferring a stable GUID identity.
///
/// Windows binds a notification GUID to the registering executable's path, so
/// the first run from a new location — which `--install` creates by design —
/// is rejected. Falling back to hWnd+uID identity keeps the icon working; the
/// user loses their pin-to-taskbar choice that once, rather than losing the
/// icon entirely.
///
/// Mutates `nid` so the caller keeps whichever identity actually succeeded:
/// every later `NIM_MODIFY` and the final `NIM_DELETE` must use the same one.
fn add_icon(nid: &mut NOTIFYICONDATAW) -> bool {
    nid.uFlags |= NIF_GUID;
    nid.guidItem = ICON_GUID;
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, nid).as_bool() {
            return true;
        }
        tracing::warn!(
            "the shell refused this icon's GUID, most likely because the executable \
             moved; falling back to window-handle identity"
        );
        nid.uFlags &= !NIF_GUID;
        nid.guidItem = GUID::zeroed();
        Shell_NotifyIconW(NIM_ADD, nid).as_bool()
    }
}

/// Opts the icon into notification version 4.
///
/// Must follow every `NIM_ADD`, including the one in `readd_icon`: the shell
/// forgets the version along with the icon when it restarts, and an icon that
/// silently reverts to the legacy version stops answering the keyboard.
fn set_icon_version(nid: &NOTIFYICONDATAW) {
    let mut versioned = *nid;
    // uVersion shares a union with uTimeout; this field is only read by
    // NIM_SETVERSION, so setting it here affects nothing else.
    versioned.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe {
        if !Shell_NotifyIconW(NIM_SETVERSION, &versioned).as_bool() {
            tracing::warn!(
                "could not set notification version 4; icon will not answer the keyboard"
            );
        }
    }
}

fn refresh_tray(ctx: &mut Ctx) {
    let tip = ctx
        .state
        .lock()
        .map(|s| s.tooltip())
        .unwrap_or_else(|_| "Headset".into());
    write_tip(&mut ctx.nid, &tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &ctx.nid);
    }
}

/// Re-registers the notification icon after the shell restarted.
///
/// The previous icon was destroyed with the old taskbar, so this is `NIM_ADD`
/// and not `NIM_MODIFY`; modifying an icon the shell no longer knows about
/// fails silently and leaves the tray empty.
fn readd_icon(ctx: &mut Ctx) {
    unsafe {
        // Delete first, ignoring failure. If the shell somehow does still hold
        // the icon, adding a second one would leave a duplicate that no message
        // ever reaches.
        let _ = Shell_NotifyIconW(NIM_DELETE, &ctx.nid);
    }
    if add_icon(&mut ctx.nid) {
        set_icon_version(&ctx.nid);
        tracing::info!("re-added the tray icon after a shell restart");
    } else {
        tracing::error!("could not re-add the tray icon after a shell restart");
    }
    refresh_tray(ctx);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // Version 4 packing: notification in the low word of lParam, icon
            // id in the high word. Masking is required -- comparing the whole
            // lParam against WM_LBUTTONUP silently stops matching.
            let notification = (lp.0 as u32) & 0xFFFF;
            match notification {
                // NIN_SELECT is a click; NIN_KEYSELECT is Enter or Space on a
                // focused icon. Both mean "activate", and handling only the
                // first is what makes an icon mouse-only.
                NIN_SELECT | NIN_KEYSELECT => {
                    with_ctx(toggle_panel);
                }
                // Replaces WM_RBUTTONUP under version 4, and is also what the
                // Menu key and Shift+F10 produce on a focused icon. The classic
                // menu is where Exit lives: the panel design has no room for it.
                WM_CONTEXTMENU => {
                    with_ctx(|ctx| show_menu(hwnd, ctx));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_STATE => {
            CTX.with(|c| {
                if let Some(ctx) = c.borrow_mut().as_mut() {
                    // The device has spoken; stop showing what was asked for.
                    ctx.pending_noise = None;
                    ctx.pending_slider = None;
                    note_link_change(hwnd, ctx);
                    refresh_tray(ctx);
                    if ctx.panel_visible {
                        redraw_panel(ctx);
                    }
                }
            });
            LRESULT(0)
        }
        // The debounce elapsed and the link has held its new state. Only the
        // owner window arms this timer; the panel shares the procedure.
        WM_TIMER if wp.0 == ID_OUTPUT_TIMER && panel::tag(hwnd) != panel::TAG_PANEL => {
            let _ = KillTimer(hwnd, ID_OUTPUT_TIMER);
            with_ctx(reconcile_output);
            LRESULT(0)
        }
        WM_LBUTTONDOWN if panel::tag(hwnd) == panel::TAG_PANEL => {
            let (x, y) = panel::point_from_lparam(lp);
            with_ctx(|ctx| on_panel_press(ctx, x, y));
            LRESULT(0)
        }
        WM_MOUSEMOVE if panel::tag(hwnd) == panel::TAG_PANEL => {
            if panel::left_button_down(wp) {
                let (x, _) = panel::point_from_lparam(lp);
                with_ctx(|ctx| on_panel_drag(ctx, x));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if panel::tag(hwnd) == panel::TAG_PANEL => {
            with_ctx(on_panel_release);
            LRESULT(0)
        }
        // Clicking away closes the panel, which is what a tray popup should do.
        WM_ACTIVATE if panel::tag(hwnd) == panel::TAG_PANEL && wp.0 == 0 => {
            with_ctx(hide_panel);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wp.0 & 0xFFFF;
            with_ctx(|ctx| on_command(id, ctx));
            LRESULT(0)
        }
        // Only the owner window's destruction ends the application. Both windows
        // share this procedure, and tearing down on the panel's WM_DESTROY would
        // take the context away while leaving the panel on screen — a live
        // process with an orphaned window that ignores every click.
        WM_DESTROY if panel::tag(hwnd) != panel::TAG_PANEL => {
            CTX.with(|c| {
                if let Some(ctx) = c.borrow_mut().take() {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &ctx.nid);
                    let _ = DestroyIcon(ctx.icon);
                    let _ = ctx.commands.send(Command::Shutdown);
                    // Take the panel with us; otherwise its pixels outlive the
                    // message loop and look like a frozen window.
                    if !ctx.panel_hwnd.is_invalid() {
                        let _ = DestroyWindow(ctx.panel_hwnd);
                    }
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        // The user turned high contrast on or off while we were running.
        WM_SETTINGCHANGE => {
            apply_appearance();
            with_ctx(|ctx| {
                if ctx.panel_visible {
                    redraw_panel(ctx);
                }
            });
            LRESULT(0)
        }
        // A second instance asked us to show ourselves rather than starting a
        // duplicate tray.
        WM_SHOW_PANEL => {
            with_ctx(|ctx| {
                if !ctx.panel_visible {
                    toggle_panel(ctx);
                }
            });
            LRESULT(0)
        }
        // Dragged onto a display with a different scale, or the user changed the
        // scaling while the panel was open. Re-render at the new density.
        WM_DPICHANGED if panel::tag(hwnd) == panel::TAG_PANEL => {
            with_ctx(redraw_panel);
            LRESULT(0)
        }
        // The shell restarted and took every tray icon with it. Re-register, and
        // drop the panel: it was anchored to an icon that no longer exists.
        m if m != 0 && m == TASKBAR_CREATED.load(std::sync::atomic::Ordering::Relaxed) => {
            with_ctx(|ctx| {
                if ctx.panel_visible {
                    hide_panel(ctx);
                }
                readd_icon(ctx);
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Notices the headset coming or going, and starts the debounce.
///
/// Restarting the timer on every edge is the debounce: a link that flaps twice
/// inside the window arms it twice and fires once, against whatever state it
/// finally settled on.
fn note_link_change(hwnd: HWND, ctx: &mut Ctx) {
    let now = ctx.state.lock().ok().and_then(|s| s.connected);
    if now == ctx.link {
        return;
    }
    ctx.link = now;
    // Each transition gets its own budget of looks for the endpoint owed. A
    // previous power-on that gave up must not leave the next one with none.
    ctx.restore_attempts = 0;
    // A vanished dongle reads as `None`, not as a powered-down headset, and is
    // deliberately not acted on: its endpoints go with it and Windows moves the
    // sound by itself.
    if now.is_some() {
        let _ = unsafe { SetTimer(hwnd, ID_OUTPUT_TIMER, OUTPUT_SWITCH_DEBOUNCE_MS, None) };
    }
}

/// Moves the sound, or moves it back, according to where the link settled.
///
/// The rule itself is in `crate::output`, which is handed facts and answers
/// with an action; this function gathers those facts and carries the action
/// out. Everything here is best-effort by design: setting the default output
/// uses an interface Windows does not document (see `win32::audio`), so a
/// failure is a thing that can simply happen — and when it does, the user is
/// left exactly where they were rather than half-moved, and told.
fn reconcile_output(ctx: &mut Ctx) {
    use crate::output::{Action, Problem};

    match plan_output(ctx.link, ctx.restore_attempts) {
        // Deliberately not a retraction. "Nothing to do" is the answer for a
        // powered-on headset with nothing owed, which is the state the machine
        // is in moments after a power-off that could not find a device to move
        // to — clearing here would wipe that complaint off the settings row and
        // then raise it as news all over again on the next power-off. A
        // complaint is retracted by a switch that works, or by the user
        // changing the setting it is about.
        Action::Nothing => {}
        Action::MoveAway { to, owing } => {
            if audio::set_default_output(&to) {
                // Recorded only after the move actually took. A debt for a move
                // that never happened would drag the sound somewhere it never
                // left.
                let _ = crate::settings::set_restore_output(Some(&owing));
                tracing::info!("headset powered down; sound moved to {to}");
                report_output_issue(ctx, None);
            } else {
                report_output_issue(ctx, Some(Problem::SwitchFailed));
            }
        }
        Action::MoveBack { to } => {
            if audio::set_default_output(&to) {
                let _ = crate::settings::set_restore_output(None);
                tracing::info!("headset back; sound restored");
                report_output_issue(ctx, None);
            } else {
                report_output_issue(ctx, Some(Problem::SwitchFailed));
            }
        }
        Action::Split { game, chat } => {
            if audio::set_split_output(&game, &chat) {
                // Whatever was owed is settled: the sound has been put on the
                // headset deliberately, and leaving the record standing would
                // drag it back off at the next opportunity.
                let _ = crate::settings::set_restore_output(None);
                tracing::info!("headset back; game on {game}, calls on {chat}");
                report_output_issue(ctx, None);
            } else {
                report_output_issue(ctx, Some(Problem::SwitchFailed));
            }
        }
        Action::Retry => {
            // The endpoint may be moments from appearing — a wireless link
            // comes up before its audio endpoints do. Look again rather than
            // concluding anything.
            ctx.restore_attempts = ctx.restore_attempts.saturating_add(1);
            tracing::debug!(
                "the endpoint to restore is not present yet (look {})",
                ctx.restore_attempts
            );
            let _ =
                unsafe { SetTimer(ctx.nid.hWnd, ID_OUTPUT_TIMER, crate::output::RETRY_MS, None) };
        }
        Action::GiveUp => {
            // The endpoint is gone for good — a reinstalled driver or a
            // re-enumerated dongle is enough to retire an endpoint id. Letting
            // the record go is what stops it blocking every later switch.
            let _ = crate::settings::set_restore_output(None);
            tracing::warn!("gave up restoring the previous output; the endpoint is gone");
            report_output_issue(ctx, Some(Problem::RestoreGone));
        }
        Action::Blocked(p) => report_output_issue(ctx, Some(p)),
    }
}

/// Gathers what the machine currently says and asks `output::decide` about it.
///
/// Reads only — every write is `reconcile_output`'s. Separating the two is what
/// lets the decision be inspected on a real machine (`--explain-output`)
/// without moving anybody's sound to find out what it would have done.
pub fn plan_output(link: Option<bool>, attempts: u8) -> crate::output::Action {
    use crate::output::{decide, Facts, Slot};

    let fallback = crate::settings::output_choice(Slot::Fallback);
    let game = crate::settings::output_choice(Slot::Game);
    let chat = crate::settings::output_choice(Slot::Chat);
    let fallback_id = fallback.as_ref().map(|(id, _)| id.as_str());
    let game_id = game.as_ref().map(|(id, _)| id.as_str());
    let chat_id = chat.as_ref().map(|(id, _)| id.as_str());
    let debt = crate::settings::restore_output();
    let current = audio::default_output_id();

    // Enumerated once and asked repeatedly: `audio::is_present` is a full Core
    // Audio enumeration each time, and every question is about one snapshot.
    let present = audio::render_outputs();
    let has = |id: Option<&str>| id.is_some_and(|i| present.iter().any(|d| d.id == i));

    decide(&Facts {
        enabled: crate::settings::switch_output_when_off(),
        split: crate::settings::split_game_and_chat(),
        link,
        fallback: fallback_id,
        fallback_present: has(fallback_id),
        game: game_id,
        game_present: has(game_id),
        chat: chat_id,
        chat_present: has(chat_id),
        current: current.as_deref(),
        debt: debt.as_deref(),
        debt_present: has(debt.as_deref()),
        attempts,
    })
}

/// Whether the user is visibly halfway through setting the split up.
///
/// The settings handlers act on the current state the moment something changes,
/// so that a switch turned on with the headset already powered does something
/// rather than appearing broken. That is the wrong thing to do between the two
/// halves of one choice: picking the game channel and immediately being told
/// off for not having picked the chat channel is a complaint about the step the
/// user is in the middle of taking. Only the powered-on case is held back —
/// that is the only one the split has any say over.
fn mid_split_setup(link: Option<bool>) -> bool {
    use crate::output::Slot;

    link == Some(true)
        && crate::settings::split_game_and_chat()
        && (crate::settings::output_choice(Slot::Game).is_none()
            || crate::settings::output_choice(Slot::Chat).is_none())
}

/// Records a problem (or its absence) and tells the user the first time.
///
/// Persisting it is what lets the settings panel explain itself hours later,
/// when nobody was watching the balloon. `set_output_problem` reports whether the
/// stored value actually changed, so a condition that recurs on every power-off
/// interrupts once rather than every time.
fn report_output_issue(ctx: &Ctx, problem: Option<crate::output::Problem>) {
    let changed = crate::settings::set_output_problem(problem);
    if let Some(p) = problem {
        tracing::warn!("output switch: {}", p.detail());
        if changed {
            notify_balloon(ctx, "Headset audio switching", p.detail());
        }
    }
}

/// Shows a balloon from the tray icon.
///
/// The panel is the wrong place for news: it is only on screen when the user
/// opened it, and the whole failure being reported is one they notice by
/// *not* seeing anything happen. Windows may suppress this — focus assist,
/// or notifications turned off for the app — which is why the same text is
/// also persisted for the settings row.
fn notify_balloon(ctx: &Ctx, title: &str, body: &str) {
    // A copy carrying only NIF_INFO: the identity fields (hWnd/uID, or the
    // GUID when the icon was added with one) come along in the copy, and
    // re-sending NIF_ICON | NIF_TIP here would rewrite them for no reason.
    let mut nid = ctx.nid;
    nid.uFlags = (ctx.nid.uFlags & NIF_GUID) | NIF_INFO;
    write_fixed(&mut nid.szInfoTitle, title);
    write_fixed(&mut nid.szInfo, body);
    nid.dwInfoFlags = NIIF_WARNING;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Copies text into one of the notification struct's fixed wide buffers,
/// truncating rather than overflowing and always leaving the final NUL.
///
/// Windows silently drops an unterminated string, so a body that exactly
/// filled the buffer would produce no balloon at all rather than a clipped one.
fn write_fixed(buf: &mut [u16], text: &str) {
    let wide: Vec<u16> = text.encode_utf16().take(buf.len() - 1).collect();
    buf.fill(0);
    buf[..wide.len()].copy_from_slice(&wide);
}

/// Renders the current state and pushes it into the layered window.
fn redraw_panel(ctx: &mut Ctx) {
    let Some(renderer) = ctx.renderer.as_ref() else {
        return;
    };
    let state = ctx.state.lock().map(|s| s.clone()).unwrap_or_default();
    let state = state.with_pending_noise(ctx.pending_noise);
    // The slider a release commits is `ctx.param`, so it must agree with the one
    // the panel draws. A device without a game/chat balance has only sidetone.
    if !state.features.game_chat {
        ctx.param = SliderParam::Sidetone;
    }
    let preview = crate::ui::layout::slider_preview(ctx.param, ctx.drag, ctx.pending_slider);
    let panel = ui::build(&state, ctx.view, ctx.param, preview);
    ctx.scale = unsafe { dpi::window_scale(ctx.panel_hwnd) };
    let img = match renderer.render(&panel, ctx.scale) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("panel render failed: {e}");
            return;
        }
    };
    // Keep the regions that match what is now on screen, so a click resolves
    // against the drawn layout rather than a recomputed one.
    ctx.hits = panel.hits.clone();
    ctx.track = panel.track;
    ctx.level_track = panel.level_track;
    ctx.output_ids = panel.output_ids.clone();

    let first_show = !ctx.panel_visible;
    unsafe {
        let (x, y) = if first_show {
            panel::anchor(
                ctx.nid.hWnd,
                ctx.nid.uID,
                img.width as i32,
                img.height as i32,
            )
        } else {
            // Keep the panel where it is across repaints; re-anchoring to the
            // cursor would make it walk around the screen as values update.
            // Held by the bottom edge, because that is the edge sitting just
            // above the tray icon and a repaint can change the height.
            panel::reanchor_bottom(ctx.panel_hwnd, img.height as i32)
        };
        if let Err(e) = panel::show(ctx.panel_hwnd, x, y, &img, first_show) {
            tracing::error!("showing the panel failed: {e}");
            return;
        }
    }
    ctx.panel_visible = true;
}

fn hide_panel(ctx: &mut Ctx) {
    unsafe { panel::hide(ctx.panel_hwnd) };
    ctx.panel_visible = false;
    ctx.drag = None;
    ctx.pending_slider = None;
    ctx.pending_noise = None;
    // Always reopen on the main view; landing back in Settings is disorienting.
    ctx.view = View::Main;
}

fn toggle_panel(ctx: &mut Ctx) {
    if ctx.panel_visible {
        hide_panel(ctx);
    } else {
        // Ask for fresh values on open rather than showing whatever was last
        // pushed: the device is the source of truth.
        let _ = ctx.commands.send(Command::Refresh);
        // Re-evaluate the warning here rather than trusting a cached answer:
        // Synapse may have started or stopped since the panel was last open.
        if let Ok(mut s) = ctx.state.lock() {
            s.warn_vendor_software = crate::warn_vendor_software();
        }
        redraw_panel(ctx);
    }
}

fn on_panel_press(ctx: &mut Ctx, x: f32, y: f32) {
    // Physical pixels in, logical units out: the window is sized in physical
    // pixels but every hit region came from `ui::layout`, which works in the
    // same logical units as the theme's metrics. The window also includes the
    // shadow margin, which is in those logical units.
    let s = if ctx.scale > 0.0 { ctx.scale } else { 1.0 };
    let (lx, ly) = (
        x / s - crate::ui::theme::SHADOW,
        y / s - crate::ui::theme::SHADOW,
    );
    let Some(target) = ctx
        .hits
        .iter()
        .find(|(r, _)| r.contains(lx, ly))
        .map(|(_, t)| *t)
    else {
        return;
    };

    match target {
        HitTarget::Gear => {
            ctx.view = View::Settings;
            redraw_panel(ctx);
        }
        HitTarget::Back => {
            // One step back, not all the way out: the picker is reached from
            // Settings, so leaving it should land where it was opened from.
            ctx.view = match ctx.view {
                View::Output(_) => View::Settings,
                _ => View::Main,
            };
            redraw_panel(ctx);
        }
        HitTarget::Refresh => {
            let _ = ctx.commands.send(Command::Refresh);
            // Refresh means "re-read everything", including whether Synapse is
            // still running. Only the device half comes back from the worker.
            if let Ok(mut s) = ctx.state.lock() {
                s.warn_vendor_software = crate::warn_vendor_software();
            }
            redraw_panel(ctx);
        }
        HitTarget::Switcher => {
            ctx.param = ctx.param.other();
            redraw_panel(ctx);
        }
        HitTarget::MutePill => {
            let current = get_mic_mute().unwrap_or(false);
            if set_mic_mute(!current).is_ok() {
                if let Ok(mut s) = ctx.state.lock() {
                    s.mic_mute_os = Some(!current);
                }
                redraw_panel(ctx);
            }
        }
        HitTarget::SliderTrack => {
            if let Some(g) = ctx.track {
                ctx.drag = Some(g.value_at(lx));
                redraw_panel(ctx);
            }
        }
        HitTarget::NoiseOff => set_noise_mode(ctx, NoiseMode::Off),
        HitTarget::NoiseAnc => set_noise_mode(ctx, NoiseMode::Anc),
        HitTarget::NoiseAmbient => set_noise_mode(ctx, NoiseMode::Ambient),
        HitTarget::NoiseLevel => {
            if let Some(t) = ctx.level_track {
                send_noise(ctx, |n| NoiseControl {
                    anc_level: t.level_at(lx),
                    ..n
                });
            }
        }
        HitTarget::AppearanceSystem => set_appearance(ctx, crate::ui::theme::Appearance::System),
        HitTarget::AppearanceDark => set_appearance(ctx, crate::ui::theme::Appearance::Dark),
        HitTarget::AppearanceLight => set_appearance(ctx, crate::ui::theme::Appearance::Light),
        HitTarget::ToggleStartup => {
            let target_exe = crate::install::installed_exe()
                .filter(|p| p.exists())
                .or_else(|| std::env::current_exe().ok());
            if let Some(exe) = target_exe {
                let now = crate::settings::run_on_startup();
                let _ = crate::settings::set_run_on_startup(!now, &exe);
                redraw_panel(ctx);
            }
        }
        HitTarget::ToggleWarning => {
            let now = crate::settings::show_synapse_warning();
            if crate::settings::set_show_synapse_warning(!now) {
                if let Ok(mut s) = ctx.state.lock() {
                    s.warn_vendor_software = crate::warn_vendor_software();
                }
                redraw_panel(ctx);
            }
        }
        HitTarget::ToggleOutputSwitch => {
            let now = crate::settings::switch_output_when_off();
            if crate::settings::set_switch_output_when_off(!now) {
                if now {
                    // Turning it off cancels any move still owed. The sound is
                    // left where it is — the user may well be listening to it —
                    // but a switched-off feature must not act on a later
                    // power-on. The complaint goes with it: a feature that is
                    // off has nothing to report.
                    let _ = crate::settings::set_restore_output(None);
                    let _ = crate::settings::set_output_problem(None);
                } else {
                    // Turning it on acts on the state the headset is in now.
                    // Waiting for the next transition would mean switching it
                    // on with the headset already off and watching nothing
                    // happen, which reads as a setting that does not work.
                    //
                    // The old complaint goes first: it was about a setting the
                    // user has just changed, so it has to be re-earned rather
                    // than left standing over a fresh attempt.
                    let _ = crate::settings::set_output_problem(None);
                    reconcile_output(ctx);
                }
                redraw_panel(ctx);
            }
        }
        HitTarget::ToggleSplit => {
            let now = crate::settings::split_game_and_chat();
            if crate::settings::set_split_game_and_chat(!now) {
                // The complaint goes either way: turning the split off retires
                // a message about a feature that is no longer running, and
                // turning it on means any standing one was about the settings
                // as they were, so it has to be re-earned.
                let _ = crate::settings::set_output_problem(None);
                if !now && !mid_split_setup(ctx.link) {
                    // Turning it on acts on the state the headset is in now,
                    // for the same reason the switch above it does: waiting for
                    // the next power cycle reads as a setting that does nothing.
                    reconcile_output(ctx);
                }
                redraw_panel(ctx);
            }
        }
        HitTarget::OpenOutputPicker(slot) => {
            ctx.view = View::Output(slot);
            redraw_panel(ctx);
        }
        HitTarget::OutputChoice(i) => {
            // Which of the three choices is being made is carried by the view,
            // because the picker draws the same list for all three.
            let View::Output(slot) = ctx.view else {
                return;
            };
            // Resolved against the list this paint drew, then re-read from the
            // system for its current name: the id is the durable half, and the
            // stored name is only ever a label for it.
            if let Some(id) = ctx.output_ids.get(i).cloned() {
                let name = audio::render_outputs()
                    .into_iter()
                    .find(|d| d.id == id)
                    .map(|d| d.name)
                    .unwrap_or_else(|| id.clone());
                let _ = crate::settings::set_output_choice(slot, &id, &name);
                // Same reasoning as turning the switch on: choosing the device
                // is the step that made the setting actionable, so act on where
                // the headset is now rather than waiting for it to move. Any
                // standing complaint was about the device that was chosen
                // before, so it is dropped and has to be re-earned.
                let _ = crate::settings::set_output_problem(None);
                if !mid_split_setup(ctx.link) {
                    reconcile_output(ctx);
                }
                ctx.view = View::Settings;
                redraw_panel(ctx);
            }
        }
    }
}

/// Stores the appearance choice, re-resolves the palette, and repaints.
///
/// Re-resolves rather than assuming the choice maps straight to a palette:
/// `System` depends on Windows, and high contrast overrides all three.
fn set_appearance(ctx: &mut Ctx, a: crate::ui::theme::Appearance) {
    if crate::settings::set_appearance(a) {
        apply_appearance();
        redraw_panel(ctx);
    }
}

fn set_noise_mode(ctx: &mut Ctx, mode: NoiseMode) {
    send_noise(ctx, |n| NoiseControl { mode, ..n });
}

/// Read-modify-write, on the UI side of the wire.
///
/// The device holds mode and level in one two-byte parameter, so a change to
/// either has to carry the other. `f` is handed the state the panel is
/// currently showing and returns the whole thing.
///
/// Nothing is sent when the current state is unknown: composing a write would
/// mean inventing the byte we did not read, and the panel does not hit-test the
/// noise row while the headset is unreachable anyway.
fn send_noise(ctx: &mut Ctx, f: impl FnOnce(NoiseControl) -> NoiseControl) {
    let current = ctx.state.lock().ok().and_then(|s| s.noise);
    let Some(current) = current else { return };
    let want = f(current);
    let _ = ctx.commands.send(Command::SetNoise(want));
    // Show it immediately. The worker's read-back replaces it with whatever
    // the device actually holds, including when the device refuses.
    ctx.pending_noise = Some(want);
    redraw_panel(ctx);
}

fn on_panel_drag(ctx: &mut Ctx, x: f32) {
    if ctx.drag.is_none() {
        return;
    }
    let Some(g) = ctx.track else { return };
    let s = if ctx.scale > 0.0 { ctx.scale } else { 1.0 };
    let v = g.value_at(x / s - crate::ui::theme::SHADOW);
    if ctx.drag != Some(v) {
        ctx.drag = Some(v);
        redraw_panel(ctx);
    }
}

/// Commits the dragged value.
///
/// One write per adjustment: requests are paced at 250 ms and a sidetone write
/// costs two exchanges, so writing every step of a drag would put the device
/// seconds behind the knob.
fn on_panel_release(ctx: &mut Ctx) {
    let Some(v) = ctx.drag.take() else { return };
    // Commit to the slider that was drawn, which is the one that was dragged.
    if let Some(g) = ctx.track {
        ctx.param = g.param;
    }
    let cmd = match ctx.param {
        SliderParam::Sidetone => Command::SetSidetone(v),
        SliderParam::GameChat => Command::SetGameChat(v),
    };
    let _ = ctx.commands.send(cmd);
    // The drag is over, but the device has not answered yet. Keep drawing the
    // released value until it does; the read-back that follows every write
    // replaces it with whatever the device actually holds.
    ctx.pending_slider = Some((ctx.param, v));
    redraw_panel(ctx);
}

fn on_command(id: usize, ctx: &Ctx) {
    match id {
        ID_EXIT => unsafe {
            // WM_CLOSE, not a synthetic WM_DESTROY: posting WM_DESTROY runs the
            // teardown handler without the window actually being destroyed,
            // which leaves a live process holding windows nothing can reach.
            let _ = PostMessageW(ctx.nid.hWnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        ID_REFRESH => {
            let _ = ctx.commands.send(Command::Refresh);
        }
        ID_STARTUP => {
            // Point the Run entry at the installed copy when there is one, so
            // enabling startup from a freshly built binary still survives the
            // build directory being cleaned.
            let target = crate::install::installed_exe()
                .filter(|p| p.exists())
                .or_else(|| std::env::current_exe().ok());
            if let Some(exe) = target {
                let now = crate::settings::run_on_startup();
                let _ = crate::settings::set_run_on_startup(!now, &exe);
            }
        }
        ID_SYNAPSE_WARNING => {
            let now = crate::settings::show_synapse_warning();
            if crate::settings::set_show_synapse_warning(!now) {
                if let Ok(mut s) = ctx.state.lock() {
                    // Recompute rather than just clearing: turning the warning
                    // back on must restore it only if Synapse is actually there.
                    s.warn_vendor_software = crate::warn_vendor_software();
                }
                unsafe {
                    let _ = PostMessageW(ctx.nid.hWnd, WM_STATE, WPARAM(0), LPARAM(0));
                }
            }
        }
        ID_MUTE => {
            // Only the OS endpoint can be toggled. The headset's hardware switch
            // is not software-writable and no vendor set-mute command exists.
            let current = get_mic_mute().unwrap_or(false);
            if set_mic_mute(!current).is_ok() {
                if let Ok(mut s) = ctx.state.lock() {
                    s.mic_mute_os = Some(!current);
                }
                unsafe {
                    let _ = PostMessageW(ctx.nid.hWnd, WM_STATE, WPARAM(0), LPARAM(0));
                }
            }
        }
        i if (ID_SIDETONE_BASE..=ID_SIDETONE_BASE + SIDETONE_MAX as usize).contains(&i) => {
            let _ = ctx
                .commands
                .send(Command::SetSidetone((i - ID_SIDETONE_BASE) as u8));
        }
        i if (ID_GAMECHAT_BASE..=ID_GAMECHAT_BASE + GAMECHAT_MAX as usize).contains(&i) => {
            let _ = ctx
                .commands
                .send(Command::SetGameChat((i - ID_GAMECHAT_BASE) as u8));
        }
        _ => {}
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The right-click menu.
///
/// Deliberately minimal: everything about the headset lives in the panel now.
/// This exists because the panel design has nowhere sensible to put Exit, and a
/// tray icon with no way to quit is hostile.
unsafe fn show_menu(hwnd: HWND, ctx: &Ctx) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        ID_REFRESH,
        PCWSTR(wide("Refresh").as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, PCWSTR(wide("Exit").as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when focus moves elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    let _ = ctx;
}

unsafe fn build_icon() -> windows::core::Result<HICON> {
    const N: usize = 32;
    let pixels = crate::ui::icon::icon_pixels(N);

    let color: HBITMAP = CreateBitmap(
        N as i32,
        N as i32,
        1,
        32,
        Some(pixels.as_ptr() as *const std::ffi::c_void),
    );
    // An all-zero mask means "use the colour bitmap's alpha everywhere".
    let mask: HBITMAP = CreateBitmap(N as i32, N as i32, 1, 1, None);
    let info = ICONINFO {
        fIcon: TRUE,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&info)?;
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    Ok(icon)
}

fn endpoint_volume() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eCapture, eMultimedia)?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

/// Reads the default capture endpoint's mute.
///
/// This is the Windows-side mute, which is independent of the headset's
/// hardware switch: toggling one was observed to produce no traffic for the
/// other.
pub fn get_mic_mute() -> windows::core::Result<bool> {
    unsafe { Ok(endpoint_volume()?.GetMute()?.as_bool()) }
}

pub fn set_mic_mute(muted: bool) -> windows::core::Result<()> {
    unsafe {
        endpoint_volume()?.SetMute(BOOL::from(muted), std::ptr::null::<GUID>())?;
        Ok(())
    }
}

/// Posts a state-changed message to the UI thread. Safe to call from the worker.
pub fn post_state(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            HWND(hwnd as *mut std::ffi::c_void),
            WM_STATE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Whether a process with this executable name is running.
///
/// Used to warn that the vendor engine will contend for settings. Advisory
/// only: a false negative costs a missing tooltip line, nothing more.
pub fn process_running(exe_name: &str) -> bool {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if name.eq_ignore_ascii_case(exe_name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// `FALSE` exists to keep the unused-import lint quiet in builds where the
/// Core Audio path is compiled but never called.
#[allow(dead_code)]
const _UNUSED: BOOL = FALSE;
