//! Win32 helpers: DPI, window styles, click-through, global hotkeys.

use pinshot_core::hotkey::{parse_hotkey, Command, Hotkey, MOD_NOREPEAT};
use pinshot_core::Config;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAK, DWMWA_CLOAKED,
    DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, VK_CONTROL,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, SetProcessWorkingSetSize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GetClassNameW, GetCursorPos,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    PostMessageW, RegisterClassW, SetCursorPos, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
    HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_REMOTESESSION, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WINDOW_DISPLAY_AFFINITY, WM_DESTROY, WM_HOTKEY, WNDCLASSW, WS_EX_APPWINDOW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP, WS_VISIBLE,
};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

use crate::event::AppEvent;

pub fn hwnd_of(window: &Window) -> HWND {
    match window.window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => HWND(w.hwnd.get() as *mut _),
            _ => HWND::default(),
        },
        Err(_) => HWND::default(),
    }
}

pub fn init_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    ignore_ctrl_c();
}

/// F5 划词翻译 SendInput Ctrl+C. A debug console (`--debug` / cargo) would
/// otherwise get CTRL_C_EVENT and die with STATUS_CONTROL_C_EXIT (0xC000013A).
pub fn ignore_ctrl_c() {
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(None, true);
    }
}

/// `TerminateProcess` from another thread. Do not call it (or `ExitProcess`)
/// on the GUI thread: tray-icon `DestroyWindow` deadlocks and the notify
/// icon stays clickable but dead.
pub fn schedule_kill_process(code: u32) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(
                windows::Win32::System::Threading::GetCurrentProcess(),
                code,
            );
        }
    });
}

pub fn park_until_killed() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub fn kill_process(code: u32) -> ! {
    schedule_kill_process(code);
    park_until_killed();
}

pub fn hide_sync(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        // Do not UpdateWindow here: a synchronous WM_PAINT on a 4160×1400
        // overlay from about_to_wait / DestroyWindow deadlocks the pump.
    }
}

/// DWM still composes a cloaked HWND, but the user does not see it. Warmup
/// must cloak *before* ShowWindow: an off-screen fullscreen window is clamped
/// onto a monitor and flashes.
pub fn cloak_window(hwnd: HWND, cloak: bool) {
    if hwnd.is_invalid() {
        return;
    }
    let v: i32 = i32::from(cloak);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            std::ptr::from_ref(&v).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}

pub fn show_noactivate(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn focus_overlay_window(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    if hwnd.is_invalid() { return; }
    unsafe {
        if GetForegroundWindow() != hwnd && !SetForegroundWindow(hwnd).as_bool() {
            crate::logutil::log_at("trace", "overlay foreground request declined");
        }
    }
}

pub fn apply_overlay_style(hwnd: HWND) {
    unsafe {
        // Overlapped / caption windows are inset to the work area, so the
        // live taskbar stays on top of the freeze. WS_POPUP covers it.
        // Force the overlay to be a borderless popup.  Removing only
        // WS_OVERLAPPEDWINDOW is not sufficient on some Windows builds:
        // winit may leave individual caption/frame bits behind and DWM can
        // briefly paint an otherwise blank non-client title bar while the
        // window is being shown.
        let visible = GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_VISIBLE.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_POPUP.0 as isize | visible);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // winit sets WS_EX_APPWINDOW by default; that forces a taskbar button
        // even after OR-ing TOOLWINDOW. 参考工具 is TOOLWINDOW from create.
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (ex | WS_EX_TOPMOST.0 as isize | WS_EX_TOOLWINDOW.0 as isize)
                & !(WS_EX_NOACTIVATE.0 as isize | WS_EX_APPWINDOW.0 as isize),
        );
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        // Do not WDA_EXCLUDEFROMCAPTURE the overlay. RDP / ToDesk / RustDesk
        // stream DXGI Desktop Duplication, so an excluded HWND is invisible
        // to the remote viewer while still eating mouse hits in the session.
        include_in_capture(hwnd);
    }
}

/// Cover the virtual desktop including the taskbar, above other TOPMOST shells.
pub fn cover_shell(hwnd: HWND, x: i32, y: i32, w: u32, h: u32) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        // HWND_TOPMOST on an *already* topmost HWND is a no-op, so the tray
        // (also TOPMOST) stays above on the second F1. Drop TOPMOST, size to
        // the virtual desktop, then re-apply TOPMOST to restack on top.
        let _ = SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            x,
            y,
            w as i32,
            h as i32,
            SWP_NOACTIVATE | SWP_NOREDRAW,
        );
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w as i32, h as i32, SWP_NOACTIVATE);
    }
}

fn is_shell_tray_class(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland"
    )
}

fn hwnd_class_name(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

thread_local! {
    static CLOAKED_TRAYS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn enum_cloak_trays(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let cloak = lparam.0 != 0;
    if is_shell_tray_class(&hwnd_class_name(hwnd)) {
        cloak_window(hwnd, cloak);
        if cloak {
            CLOAKED_TRAYS.with(|slot| slot.borrow_mut().push(hwnd.0 as isize));
        }
    }
    BOOL(1)
}

/// Ask Windows to give unused pages back. The CRT heap stays reserved after
/// Drop; Task Manager working-set is what the user sees.
pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Hide the live taskbar while the freeze overlay is up. Explorer's tray is
/// also TOPMOST; after focus it jumps above the overlay, eats clicks, and
/// the hover ring stays on the maximized app underneath.
pub fn cloak_shell_trays(cloak: bool) {
    if cloak {
        CLOAKED_TRAYS.with(|slot| slot.borrow_mut().clear());
        unsafe {
            let _ = EnumWindows(Some(enum_cloak_trays), LPARAM(1));
        }
        crate::logutil::log(&format!(
            "cloak trays n={}",
            CLOAKED_TRAYS.with(|slot| slot.borrow().len())
        ));
    } else {
        CLOAKED_TRAYS.with(|slot| {
            for raw in slot.borrow_mut().drain(..) {
                let hwnd = HWND(raw as *mut _);
                if unsafe { IsWindow(hwnd).as_bool() } {
                    cloak_window(hwnd, false);
                }
            }
        });
        unsafe {
            let _ = EnumWindows(Some(enum_cloak_trays), LPARAM(0));
        }
    }
}

/// DXGI / Windows Graphics Capture skip this HWND (Win10 2004+). Pins stay
/// capturable. Use only on short-lived chrome (countdown), not the snip overlay:
/// remote-desktop tools duplicate the desktop and would hide an excluded overlay.
pub fn exclude_from_capture(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    // Win10 2004+ WDA_EXCLUDEFROMCAPTURE (0x11). windows 0.58 may not export the alias.
    const EXCLUDE: WINDOW_DISPLAY_AFFINITY = WINDOW_DISPLAY_AFFINITY(0x0000_0011);
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, EXCLUDE);
    }
}

pub fn include_in_capture(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    const NONE: WINDOW_DISPLAY_AFFINITY = WINDOW_DISPLAY_AFFINITY(0);
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, NONE);
    }
}

pub fn is_remote_session() -> bool {
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            (0, 0, 1920, 1080)
        } else {
            (vx, vy, vw, vh)
        }
    }
}

/// 参考工具-style: Preferences is a tool window, not a taskbar app.
pub fn restyle_prefs_windows() {
    unsafe {
        let pid = GetCurrentProcessId();
        let _ = EnumWindows(Some(restyle_prefs_visit), LPARAM(pid as isize));
    }
}

unsafe extern "system" fn restyle_prefs_visit(hwnd: HWND, data: LPARAM) -> windows::Win32::Foundation::BOOL {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != data.0 as u32 {
            return windows::Win32::Foundation::BOOL(1);
        }
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return windows::Win32::Foundation::BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if !title.contains("Preferences") && !title.contains("首选项") && !title.contains("设置") {
            return windows::Win32::Foundation::BOOL(1);
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize),
        );
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        windows::Win32::Foundation::BOOL(1)
    }
}

pub fn apply_pin_style(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (ex | WS_EX_TOPMOST.0 as isize
                | WS_EX_TOOLWINDOW.0 as isize
                | WS_EX_LAYERED.0 as isize)
                & !(WS_EX_APPWINDOW.0 as isize),
        );
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let _ = SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            255,
            LWA_ALPHA,
        );
    }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    unsafe {
        let mut p = windows::Win32::Foundation::POINT::default();
        GetCursorPos(&mut p).ok()?;
        Some((p.x, p.y))
    }
}

/// One-pixel SetCursorPos round-trip so a still mouse still delivers WM_MOUSEMOVE.
pub fn nudge_cursor() {
    unsafe {
        let mut p = windows::Win32::Foundation::POINT::default();
        if GetCursorPos(&mut p).is_err() {
            return;
        }
        let _ = SetCursorPos(p.x + 1, p.y);
        let _ = SetCursorPos(p.x, p.y);
    }
}

/// Move a layered pin without a CPU redraw. DWM keeps the last frame.
pub fn move_window(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
}

/// Place a hidden overlay without mapping it. Reused warmup HWNDs sit
/// off-screen until the first GDI blit, then ShowWindow.
pub fn place_window(hwnd: HWND, x: i32, y: i32, w: u32, h: u32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            w as i32,
            h as i32,
            SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
}

pub fn pointer_ctrl_down() -> bool {
    unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) < 0 }
}

pub fn set_opacity(hwnd: HWND, opacity: f32) {
    let alpha = (opacity.clamp(0.1, 1.0) * 255.0).round() as u8;
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_LAYERED.0 as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        }
        let _ = SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            alpha,
            LWA_ALPHA,
        );
    }
}

/// Toggle `WS_EX_TRANSPARENT` without changing alpha, so UIA/MSAA
/// `ElementFromPoint` / `AccessibleObjectFromPoint` can see through the overlay.
pub fn with_hit_through<R>(hwnd: HWND, f: impl FnOnce() -> R) -> R {
    if hwnd.is_invalid() {
        return f();
    }
    struct Restore(HWND, isize);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                SetWindowLongPtrW(self.0, GWL_EXSTYLE, self.1);
            }
        }
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as isize);
        let _restore = Restore(hwnd, ex);
        f()
    }
}

pub fn set_click_through(hwnd: HWND, on: bool, opacity: f32) {
    unsafe {
        let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if on {
            ex |= WS_EX_TRANSPARENT.0 as isize | WS_EX_LAYERED.0 as isize;
        } else {
            ex &= !(WS_EX_TRANSPARENT.0 as isize);
            ex |= WS_EX_LAYERED.0 as isize;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let a = if on {
            (opacity.min(0.55) * 255.0).round() as u8
        } else {
            (opacity.clamp(0.1, 1.0) * 255.0).round() as u8
        };
        let _ =
            SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), a, LWA_ALPHA);
    }
}

#[derive(Clone, Copy)]
struct HotkeyBind {
    id: i32,
    cmd: Command,
}

static PROXY: Mutex<Option<EventLoopProxy<AppEvent>>> = Mutex::new(None);
static BINDS: Mutex<Vec<HotkeyBind>> = Mutex::new(Vec::new());
const HOTKEY_NOTICE_MESSAGE: u32 = 0x8000 + 42;
const HOTKEY_NOTICE_ID: u32 = 42;

unsafe extern "system" fn hotkey_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == HOTKEY_NOTICE_MESSAGE {
        use windows::Win32::UI::Shell::{NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK};
        let event = lparam.0 as u32;
        if event == NIN_BALLOONUSERCLICK {
            if let Ok(proxy) = PROXY.lock() {
                if let Some(proxy) = proxy.as_ref() {
                    let _ = proxy.send_event(AppEvent::Hotkey(Command::OpenSettings));
                }
            }
        }
        if matches!(event, NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT | NIN_BALLOONUSERCLICK) {
            remove_hotkey_notice(hwnd);
        }
        return LRESULT(0);
    }
    if msg == WM_HOTKEY {
        // Settings records actual key states, including our already-bound keys.
        // Do not launch a snip/translation while the user is editing shortcuts.
        if crate::prefs::settings_has_focus() {
            return LRESULT(0);
        }
        let id = wparam.0 as i32;
        if let Ok(binds) = BINDS.lock() {
            if let Some(b) = binds.iter().find(|b| b.id == id) {
                if let Ok(g) = PROXY.lock() {
                    if let Some(p) = g.as_ref() {
                        let _ = p.send_event(AppEvent::Hotkey(b.cmd));
                    }
                }
            }
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub struct HotkeyHost {
    hwnd: HWND,
    ids: Vec<i32>,
}

impl HotkeyHost {
    pub fn install(proxy: EventLoopProxy<AppEvent>, cfg: &Config) -> windows::core::Result<Self> {
        *PROXY.lock().expect("proxy") = Some(proxy);
        let class = w!("PinShotHotkeyHost");
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(hotkey_wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class,
                w!("PinShotHotkeys"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                hinst,
                None,
            )?;
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = UpdateWindow(hwnd);

            let specs = [
                (&cfg.hotkey_snip, Command::Snip, "Snip"),
                (&cfg.hotkey_pin, Command::PinClipboard, "Pin clipboard"),
                (&cfg.hotkey_hide_all, Command::HideAllPins, "Hide/Show all pins"),
                (&cfg.hotkey_restore, Command::RestoreClosed, "Restore last closed pin"),
                (&cfg.hotkey_selection_translate, Command::SelectionTranslate, "Selection translation"),
                (&cfg.hotkey_capture_translate, Command::CaptureTranslate, "Snip and translate"),
                (&cfg.hotkey_clickthrough, Command::ClickThrough, "Click-through"),
            ];
            let mut ids = Vec::new();
            let mut binds = Vec::new();
            let mut failures = Vec::new();
            let mut next = 0xA000i32;
            for (seq, cmd, label) in specs {
                if !seq.trim().is_empty() {
                    match register_hotkey(hwnd, next, seq) {
                        Ok(()) => {
                            binds.push(HotkeyBind { id: next, cmd });
                            ids.push(next);
                        }
                        Err(reason) => failures.push(format!(
                            "{}: {} — {reason}", pinshot_core::i18n::tr(label), seq.trim()
                        )),
                    }
                }
                next += 1;
            }
            *BINDS.lock().expect("binds") = binds;
            notify_hotkey_failures(hwnd, failures);
            Ok(Self { hwnd, ids })
        }
    }
}

fn register_hotkey(hwnd: HWND, id: i32, seq: &str) -> Result<(), String> {
    use pinshot_core::i18n::tr;
    let Hotkey { mods, vk } = parse_hotkey(seq)
        .ok_or_else(|| tr("Invalid shortcut").to_string())?;
    unsafe { RegisterHotKey(hwnd, id, HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT), vk) }
        .map_err(|error| {
            // ERROR_HOTKEY_ALREADY_REGISTERED also covers duplicate settings.
            if error.code() == windows::core::HRESULT::from_win32(1409) {
                tr("Already in use or assigned to another PinShot action").to_string()
            } else {
                format!("{}: {error}", tr("Could not register shortcut"))
            }
        })
}

#[cfg(test)]
mod hotkey_registration_tests {
    use super::*;

    #[test]
    fn invalid_shortcut_is_reported() {
        assert!(register_hotkey(HWND::default(), 0x7ef0, "not a shortcut").is_err());
    }

    #[test]
    fn notification_text_is_terminated_without_splitting_surrogates() {
        let mut buffer = [9; 4];
        copy_notice_text(&mut buffer, "中😀文");
        assert_eq!(String::from_utf16(&buffer[..3]).unwrap(), "中😀");
        assert_eq!(buffer[3], 0);
        copy_notice_text(&mut buffer, "ab😀");
        assert_eq!(buffer, [b'a' as u16, b'b' as u16, 0, 0]);
    }

    #[test]
    fn occupied_shortcut_is_reported_and_can_be_registered_after_release() {
        // Hold a rarely used shortcut on another thread, without interfering
        // with the user's configured shortcuts or showing a notification.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let seq = (13..=24).map(|n| format!("Ctrl+Alt+Shift+F{n}"))
                .find(|seq| register_hotkey(HWND::default(), 0x7ef0, seq).is_ok());
            ready_tx.send(seq).unwrap();
            let _ = release_rx.recv();
            unsafe { let _ = UnregisterHotKey(None, 0x7ef0); }
        });
        let seq = ready_rx.recv().unwrap();
        let conflict = seq.as_ref().map(|seq| register_hotkey(HWND::default(), 0x7ef1, seq));
        let _ = release_tx.send(());
        owner.join().unwrap();
        let seq = seq.expect("no spare test shortcut available");
        assert!(conflict.unwrap().is_err(), "occupied shortcut must fail");
        let registered = register_hotkey(HWND::default(), 0x7ef1, &seq);
        unsafe { let _ = UnregisterHotKey(None, 0x7ef1); }
        assert!(registered.is_ok(), "released shortcut must be available");
    }
}

fn notify_hotkey_failures(hwnd: HWND, failures: Vec<String>) {
    if failures.is_empty() {
        return;
    }
    use pinshot_core::i18n::tr;
    let message = format!("{}\n\n{}\n\n{}",
        tr("Some shortcuts could not be enabled:"), failures.join("\n"),
        tr("Change these shortcuts in Settings, or close the application using them. Other shortcuts remain available."));
    crate::logutil::log(&message);
    // Keep the system's bounded notification body concise; full failure
    // reasons remain in the log. Put the action before the potentially long list.
    let summary = format!("{}\n{}", tr("Shortcuts unavailable. Click to change them in Settings."),
        failures.iter().map(|failure| failure.split(" — ").next().unwrap_or(failure))
            .collect::<Vec<_>>().join("\n"));
    // Native Windows notification: no dialog, focus change or startup blocking.
    // This temporary notification icon is removed on dismissal or host teardown.
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
        NIIF_WARNING, NIM_ADD, NIM_MODIFY,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::LoadIconW;
    use windows::core::PCWSTR;
    let app_icon = unsafe {
        GetModuleHandleW(None)
            .ok()
            .and_then(|module| LoadIconW(module, PCWSTR(1 as *const u16)).ok())
            .unwrap_or_default()
    };
    let mut notice = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: HOTKEY_NOTICE_ID,
        uFlags: NIF_ICON | NIF_MESSAGE,
        uCallbackMessage: HOTKEY_NOTICE_MESSAGE,
        hIcon: app_icon,
        ..Default::default()
    };
    unsafe {
        if !Shell_NotifyIconW(NIM_ADD, &notice).as_bool() {
            crate::logutil::log("hotkey notification: could not add notification icon");
            return;
        }
        notice.uFlags = NIF_INFO;
        notice.dwInfoFlags = NIIF_WARNING;
        copy_notice_text(&mut notice.szInfoTitle, &tr("PinShot shortcut warning"));
        copy_notice_text(&mut notice.szInfo, &summary);
        if !Shell_NotifyIconW(NIM_MODIFY, &notice).as_bool() {
            crate::logutil::log("hotkey notification: Windows rejected notification");
            remove_hotkey_notice(hwnd);
        }
    }
}

fn copy_notice_text<const N: usize>(buffer: &mut [u16; N], text: &str) {
    buffer.fill(0);
    let mut used = 0;
    for ch in text.chars() {
        let mut units = [0; 2];
        let encoded = ch.encode_utf16(&mut units);
        if used + encoded.len() >= N {
            break;
        }
        buffer[used..used + encoded.len()].copy_from_slice(encoded);
        used += encoded.len();
    }
}

fn remove_hotkey_notice(hwnd: HWND) {
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NOTIFYICONDATAW, NIM_DELETE};
    let notice = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: HOTKEY_NOTICE_ID,
        ..Default::default()
    };
    unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &notice); }
}

impl Drop for HotkeyHost {
    fn drop(&mut self) {
        remove_hotkey_notice(self.hwnd);
        unsafe {
            for id in &self.ids {
                let _ = UnregisterHotKey(self.hwnd, *id);
            }
            if IsWindow(self.hwnd).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        let _ = BINDS.lock().map(|mut b| b.clear());
    }
}

pub fn window_frame(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let mut r = windows::Win32::Foundation::RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            std::mem::size_of::<windows::Win32::Foundation::RECT>() as u32,
        )
        .is_ok()
        {
            return Some((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r).is_ok() {
            return Some((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        None
    }
}

pub fn is_cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut v: u32 = 0;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut v as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && v != 0
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcw(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

pub fn post_quit_like(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(hwnd, WM_DESTROY, WPARAM(0), LPARAM(0));
    }
}

/// F5: send Ctrl+C, read clipboard text, restore later via caller.
pub fn send_ctrl_c() -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    ignore_ctrl_c();
    unsafe {
        for vk in [0x12u16, 0x10, 0x5B, 0x5C] {
            if GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 {
                let up = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(vk),
                            dwFlags: KEYEVENTF_KEYUP,
                            ..Default::default()
                        },
                    },
                };
                let _ = SendInput(&[up], std::mem::size_of::<INPUT>() as i32);
            }
        }
        let n = SendInput(
            &[
                key(0x11, false),
                key(0x43, false),
                key(0x43, true),
                key(0x11, true),
            ],
            std::mem::size_of::<INPUT>() as i32,
        );
        if n != 4 {
            return Err("SendInput Ctrl+C failed".into());
        }
    }
    Ok(())
}

fn key(vk: u16, up: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                ..Default::default()
            },
        },
    }
}
