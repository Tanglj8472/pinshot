//! Thin platform facade. Windows keeps the existing Win32 helpers; Linux uses X11.

use winit::event_loop::EventLoopProxy;
use winit::window::{Window, WindowAttributes};

use crate::event::AppEvent;
use pinshot_core::Config;

#[cfg(windows)]
pub use crate::win32::HotkeyHost;

#[cfg(unix)]
pub use crate::x11util::HotkeyHost;

#[cfg(windows)]
pub type NativeWin = windows::Win32::Foundation::HWND;

#[cfg(not(windows))]
pub type NativeWin = ();

#[cfg(windows)]
pub fn hwnd_of(window: &Window) -> NativeWin {
    crate::win32::hwnd_of(window)
}

#[cfg(not(windows))]
pub fn hwnd_of(_window: &Window) -> NativeWin {}

pub fn init_process() {
    #[cfg(windows)]
    crate::win32::init_dpi();
    #[cfg(unix)]
    {
        if let Err(e) = gtk::init() {
            crate::logutil::log(&format!("gtk::init: {e}"));
        }
    }
}

/// Pins / overlay / countdown: hide from the Windows taskbar (参考工具-style).
/// winit otherwise sets `WS_EX_APPWINDOW` and explorer draws a caption icon.
pub fn as_tool_window(attrs: WindowAttributes) -> WindowAttributes {
    #[cfg(windows)]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        attrs.with_skip_taskbar(true)
    }
    #[cfg(not(windows))]
    {
        attrs
    }
}

pub fn apply_overlay_style(window: &Window) {
    #[cfg(windows)]
    {
        crate::win32::apply_overlay_style(hwnd_of(window));
        use winit::platform::windows::WindowExtWindows;
        window.set_skip_taskbar(true);
    }
    #[cfg(unix)]
    crate::x11util::apply_overlay_style(window);
}

/// Size and restack the overlay over the taskbar (Windows).
pub fn cover_shell(window: &Window, x: i32, y: i32, w: u32, h: u32) {
    #[cfg(windows)]
    crate::win32::cover_shell(hwnd_of(window), x, y, w, h);
    #[cfg(not(windows))]
    let _ = (window, x, y, w, h);
}

pub fn cloak_window(window: &Window, cloak: bool) {
    #[cfg(windows)]
    crate::win32::cloak_window(hwnd_of(window), cloak);
    #[cfg(not(windows))]
    let _ = (window, cloak);
}

/// Cloak explorer's live taskbar so it cannot sit above the freeze overlay.
pub fn cloak_shell_trays(cloak: bool) {
    #[cfg(windows)]
    crate::win32::cloak_shell_trays(cloak);
    #[cfg(not(windows))]
    let _ = cloak;
}

pub fn show_noactivate(window: &Window) {
    #[cfg(windows)]
    crate::win32::show_noactivate(hwnd_of(window));
    #[cfg(not(windows))]
    let _ = window;
}

pub fn exclude_from_capture(window: &Window) {
    #[cfg(windows)]
    crate::win32::exclude_from_capture(hwnd_of(window));
    #[cfg(not(windows))]
    let _ = window;
}

pub fn apply_pin_style(window: &Window) {
    #[cfg(windows)]
    {
        crate::win32::apply_pin_style(hwnd_of(window));
        use winit::platform::windows::WindowExtWindows;
        window.set_skip_taskbar(true);
    }
    #[cfg(unix)]
    crate::x11util::apply_pin_style(window);
}

pub fn cursor_screen() -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        crate::win32::cursor_pos()
    }
    #[cfg(unix)]
    {
        crate::x11util::root_cursor()
    }
}

pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    #[cfg(windows)]
    {
        crate::win32::virtual_desktop()
    }
    #[cfg(unix)]
    {
        crate::x11util::root_desktop()
    }
}

/// Place a new pin with its center under the pointer. This makes pasted images
/// appear where the user clicked instead of using a fixed screen position.
pub fn pin_pos_near_cursor(w: u32, h: u32) -> (i32, i32) {
    let cursor = cursor_screen().unwrap_or((40, 40));
    let (dx, dy, dw, dh) = virtual_desktop();
    let x = cursor.0 - w as i32 / 2;
    let y = cursor.1 - h as i32 / 2;
    let max_x = (dx + dw - w as i32).max(dx);
    let max_y = (dy + dh - h as i32).max(dy);
    (x.clamp(dx, max_x), y.clamp(dy, max_y))
}

/// Fake a 1px mouse move and restore so overlay hover runs without the user moving.
pub fn nudge_cursor() {
    #[cfg(windows)]
    crate::win32::nudge_cursor();
}

pub fn move_window(window: &Window, x: i32, y: i32) {
    #[cfg(windows)]
    crate::win32::move_window(hwnd_of(window), x, y);
    #[cfg(not(windows))]
    window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
}

pub fn set_opacity(window: &Window, opacity: f32) {
    #[cfg(windows)]
    crate::win32::set_opacity(hwnd_of(window), opacity);
    #[cfg(unix)]
    crate::x11util::set_opacity(window, opacity);
}

pub fn set_click_through(window: &Window, on: bool, opacity: f32) {
    #[cfg(windows)]
    crate::win32::set_click_through(hwnd_of(window), on, opacity);
    #[cfg(unix)]
    crate::x11util::set_click_through(window, on, opacity);
}

pub fn raise_on_top(window: &Window) {
    #[cfg(unix)]
    crate::x11util::raise_on_top(window);
    let _ = window;
}

/// Raise `window` above `lower` (pin menu above the F5 strip on the pin).
pub fn raise_above(window: &Window, lower: Option<&Window>) {
    #[cfg(unix)]
    {
        if let Some(xid) = crate::x11util::xid_of(window) {
            let low = lower.and_then(crate::x11util::xid_of);
            match low {
                Some(l) => crate::x11util::raise_xid_above(xid, &[l]),
                None => crate::x11util::raise_xid_above(xid, &[]),
            }
            return;
        }
    }
    let _ = (window, lower);
}

pub fn focus_window(window: &Window) {
    #[cfg(unix)]
    crate::x11util::focus_window(window);
    #[cfg(not(unix))]
    window.focus_window();
}

/// Screenshot focus must not inject Alt while the capture hotkey is held.
pub fn focus_overlay_window(window: &Window) {
    #[cfg(windows)]
    crate::win32::focus_overlay_window(hwnd_of(window));
    #[cfg(not(windows))]
    focus_window(window);
}

#[cfg(unix)]
pub fn release_stray_grabs() {
    crate::x11util::release_stray_grabs();
}

#[cfg(not(unix))]
pub fn release_stray_grabs() {}

#[cfg(unix)]
pub fn grab_overlay_keys(
    window: &Window,
    proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>,
) -> Option<crate::x11util::OverlayKeyGrab> {
    crate::x11util::xid_of(window).map(|xid| crate::x11util::OverlayKeyGrab::start(xid, proxy))
}

#[cfg(unix)]
pub fn pointer_in_window(window: &Window) -> Option<(i32, i32, bool)> {
    crate::x11util::xid_of(window).and_then(crate::x11util::pointer_in_window)
}

#[cfg(unix)]
pub fn pointer_in_client(window: &Window) -> Option<(i32, i32, bool)> {
    crate::x11util::xid_of(window).and_then(crate::x11util::pointer_in_client)
}

pub fn pointer_ctrl_down() -> bool {
    #[cfg(unix)]
    {
        crate::x11util::pointer_ctrl_down()
    }
    #[cfg(windows)]
    {
        crate::win32::pointer_ctrl_down()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(unix)]
pub fn configure_window_geom(window: &Window, x: i32, y: i32, w: u32, h: u32) -> bool {
    match crate::x11util::xid_of(window) {
        Some(xid) => {
            crate::x11util::configure_geom(xid, x, y, w, h);
            true
        }
        None => false,
    }
}

#[cfg(not(unix))]
pub fn pointer_in_window(_window: &Window) -> Option<(i32, i32, bool)> {
    None
}

#[cfg(not(unix))]
pub fn pointer_in_client(_window: &Window) -> Option<(i32, i32, bool)> {
    None
}

#[cfg(windows)]
pub fn configure_window_geom(window: &Window, x: i32, y: i32, w: u32, h: u32) -> bool {
    crate::win32::place_window(hwnd_of(window), x, y, w, h);
    true
}

#[cfg(not(any(unix, windows)))]
pub fn configure_window_geom(_window: &Window, _x: i32, _y: i32, _w: u32, _h: u32) -> bool {
    false
}

#[cfg(not(unix))]
pub fn grab_overlay_keys(
    _window: &Window,
    _proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>,
) -> Option<()> {
    None
}

pub fn unmap_sync(window: &Window) {
    #[cfg(unix)]
    crate::x11util::unmap_sync(window);
    #[cfg(windows)]
    crate::win32::hide_sync(hwnd_of(window));
    let _ = window;
}

pub fn install_hotkeys(
    proxy: EventLoopProxy<AppEvent>,
    cfg: &Config,
) -> Result<HotkeyHost, String> {
    #[cfg(windows)]
    {
        crate::win32::HotkeyHost::install(proxy, cfg).map_err(|e| e.to_string())
    }
    #[cfg(unix)]
    {
        crate::x11util::HotkeyHost::install(proxy, cfg)
    }
}

pub fn compositor_active() -> bool {
    #[cfg(unix)]
    {
        crate::x11util::compositor_active()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn schedule_kill_process(code: u32) {
    #[cfg(windows)]
    crate::win32::schedule_kill_process(code);
    #[cfg(not(windows))]
    let _ = code;
}

pub fn park_until_killed() -> ! {
    #[cfg(windows)]
    crate::win32::park_until_killed();
    #[cfg(not(windows))]
    std::process::exit(0)
}

pub fn kill_process(code: u32) -> ! {
    #[cfg(windows)]
    crate::win32::kill_process(code);
    #[cfg(not(windows))]
    std::process::exit(code as i32)
}

pub fn platform_name() -> &'static str {
    #[cfg(windows)]
    {
        "windows-rust"
    }
    #[cfg(unix)]
    {
        "linux-rust"
    }
    #[cfg(not(any(windows, unix)))]
    {
        "rust"
    }
}
