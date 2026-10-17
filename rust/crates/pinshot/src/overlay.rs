//! Full-screen snip overlay session (one winit window covering the virtual desktop).

use std::rc::Rc;
use std::time::{Duration, Instant};

use pinshot_core::annotation::{Annotator, ArrowKind, Tool};
use pinshot_core::color::Rgba;
use pinshot_core::colorpicker::{color_picker_layout, ColorPicker, PickerAction};
use pinshot_core::confirm::{confirm_abort_layout, ConfirmAbortHit};
use pinshot_core::cursor::cursor_for;
use pinshot_core::geom::{
    hit_sel_handle, hover_rect, overlay_from_screen, screen_from_overlay, Point, Rect,
};
use pinshot_core::history::{HistoryEntry, HistoryView, SnipHistory};
use pinshot_core::i18n::tr;
use pinshot_core::magnifier::magnifier_visible;
use pinshot_core::raster::Raster;
use pinshot_core::render::compose;
use pinshot_core::selection::{DetectMode, Nudge, PressKind, SelState, Selection};
use pinshot_core::snap::SnapGuides;
use pinshot_core::wheel::WheelGate;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use crate::capture::Capture;
#[cfg(not(windows))]
use crate::detect::translated_elements;
use crate::detect::{element_at_point, element_source, translated_windows, window_at_point};
#[cfg(not(windows))]
use crate::draw::present;
use crate::draw::OverlayFront;
use crate::native::{self, hwnd_of};
use crate::toolbar::{TbAction, Toolbar};

pub enum OverlayResult {
    Copy(Raster, Rect),
    Pin(Raster, Rect, (i32, i32)),
    Save(Raster),
    Ocr(Raster, bool, (i32, i32)),
    Cancel,
    /// Esc then F1 (or F1 while the overlay still holds the grab): start a new snip.
    Resnip,
}

impl OverlayResult {
    /// Copy / pin / save / OCR push a history frame. Cancel and F1 resnip do not.
    pub fn writes_history(&self) -> bool {
        matches!(
            self,
            Self::Copy(_, _) | Self::Pin(_, _, _) | Self::Save(_) | Self::Ocr(_, _, _)
        )
    }
}

struct ConfirmAbort {
    dont: bool,
}

pub struct Overlay {
    window: Rc<Window>,
    front: OverlayFront,
    capture: Capture,
    sel: Selection,
    cursor: Point,
    hover: Rect,
    windows: Vec<Rect>,
    elements: Vec<Rect>,
    elements_rx: Option<std::sync::mpsc::Receiver<Vec<Rect>>>,
    hit_px: Option<(i32, i32)>,
    hit_rect: Option<Rect>,
    hit_at: Option<Instant>,
    moved_at: Instant,
    hover_deep: bool,
    annot: Annotator,
    toolbar: Option<Toolbar>,
    hex: bool,
    mag_forced: bool,
    detect: DetectMode,
    toast: Option<String>,
    toast_until: Option<Instant>,
    arrow_kind: ArrowKind,
    ellipse_on: bool,
    line_menu_open: bool,
    font_menu_open: bool,
    font_menu_offset: usize,
    font_scroll_drag: bool,
    dpr: f32,
    result: Option<OverlayResult>,
    dirty: bool,
    #[cfg(windows)]
    need_stack: bool,
    pub history: SnipHistory,
    live_shot: Option<Raster>,
    translate_on_select: bool,
    show_cursor: bool,
    mods: ModifiersState,
    history_max: u32,
    held_btn1: bool,
    slider_drag: bool,
    /// Left-button gesture started on this overlay (Esc then asks 放弃截图?).
    interacted: bool,
    #[cfg(unix)]
    ptr_grab_ok: bool,
    /// Consecutive QueryPointer Button1-up samples. Used only to finish an
    /// in-progress drag if Release was dropped — never as a press origin.
    btn1_up_streak: u8,
    /// Last CursorMoved / grab motion while Button1 was up. A 2-point handle
    /// drag can skip the press pixel; this is the real corner.
    last_idle_pointer: Option<Point>,
    wheel: WheelGate,
    confirm: Option<ConfirmAbort>,
    color_picker: Option<ColorPicker>,
    sel_move0: Option<Rect>,
    #[cfg(unix)]
    proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>,
    #[cfg(unix)]
    key_grab: Option<crate::x11util::OverlayKeyGrab>,
    #[cfg(unix)]
    ptr_hold: Option<crate::x11util::InputHold>,
    #[cfg(not(windows))]
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    ime_preedit: bool,
}

/// Warmup / last-snip HWND is reusable only when it already matches the
/// virtual desktop. A 1×1 pump fallback is a cold DWM surface if resized.
fn overlay_hwnd_fits(inner_w: u32, inner_h: u32, cap_w: u32, cap_h: u32) -> bool {
    inner_w == cap_w && inner_h == cap_h && cap_w > 1 && cap_h > 1
}

fn overlay_key_is_actionable(key: &Key, synthetic: bool, repeat: bool) -> bool {
    !synthetic && !(repeat && matches!(key, Key::Named(NamedKey::F1)))
}

#[cfg(all(test, windows))]
#[test]
fn windows_focus_keys_do_not_resnip() {
    let f1 = Key::Named(NamedKey::F1);
    assert!(!overlay_key_is_actionable(&f1, true, false));
    assert!(!overlay_key_is_actionable(&f1, false, true));
    assert!(overlay_key_is_actionable(&f1, false, false));
    assert!(overlay_key_is_actionable(&Key::Named(NamedKey::Backspace), false, true));
}

fn overlay_window(
    event_loop: &ActiveEventLoop,
    recycled: Option<Rc<Window>>,
    vx: i32,
    vy: i32,
    w: u32,
    h: u32,
) -> Result<Rc<Window>, String> {
    if let Some(window) = recycled {
        let sz = window.inner_size();
        if overlay_hwnd_fits(sz.width, sz.height, w, h) {
            crate::logutil::log("overlay reuse hwnd");
            window.set_visible(false);
            // Hide can let the tray restack above this already-TOPMOST HWND.
            if !native::configure_window_geom(&window, vx, vy, w, h) {
                native::move_window(&window, vx, vy);
                let _ = window.request_inner_size(PhysicalSize::new(w, h));
            }
            native::apply_overlay_style(&window);
            // Recycled HWND was uncloaked on the last F1. Cloak before the
            // live hit-test so a still mouse still sees the window underneath.
            native::cloak_window(&window, true);
            return Ok(window);
        }
        crate::logutil::log(&format!(
            "overlay hwnd {}x{} != capture {}x{}, create",
            sz.width, sz.height, w, h
        ));
    }
    let t0 = Instant::now();
    let attrs = native::as_tool_window(
        Window::default_attributes()
            // The overlay is a borderless freeze surface. It does not need a
            // user-visible title; keeping this empty also prevents a transient
            // native frame from exposing the product name during resnip.
            .with_title("")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_active(false)
            .with_position(PhysicalPosition::new(vx, vy))
            .with_inner_size(PhysicalSize::new(w, h))
            .with_window_level(winit::window::WindowLevel::AlwaysOnTop),
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let attrs = {
        use winit::platform::x11::WindowAttributesExtX11;
        attrs.with_override_redirect(true)
    };
    let window = Rc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
    crate::logutil::ms("overlay create_window", t0);
    native::apply_overlay_style(&window);
    Ok(window)
}

impl Overlay {
    pub fn start(
        event_loop: &ActiveEventLoop,
        capture: Capture,
        history: SnipHistory,
        history_max: u32,
        dim_alpha: u8,
        translate_on_select: bool,
        proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>,
        recycled: Option<Rc<Window>>,
        #[cfg(unix)] mut pre_hold: Option<crate::x11util::InputHold>,
    ) -> Result<Self, String> {
        let t0 = Instant::now();
        let (vx, vy) = capture.virtual_origin;
        let w = capture.composed.width;
        let h = capture.composed.height;
        crate::logutil::step("before overlay window");
        let window = overlay_window(event_loop, recycled, vx, vy, w, h)?;
        window.set_cursor(CursorIcon::Crosshair);
        // IME only while a text editor is open; a standing IME grab lets Tab
        // leak to Chrome before XGrabKeyboard wins.
        window.set_ime_allowed(false);
        #[cfg(not(windows))]
        let surface = {
            let ctx = softbuffer::Context::new(window.clone()).map_err(|e| format!("{e:?}"))?;
            Some(softbuffer::Surface::new(&ctx, window.clone()).map_err(|e| format!("{e:?}"))?)
        };
        let t_front = Instant::now();
        let dim = dim_alpha.max(160);
        crate::logutil::step("before overlay front");
        let front = OverlayFront::new(&capture.composed, dim)?;
        crate::logutil::ms("overlay front", t_front);
        let t_enum = Instant::now();
        crate::logutil::step("before enumerate windows");
        let windows = translated_windows(capture.virtual_origin);
        crate::logutil::ms("overlay enum windows", t_enum);
        crate::logutil::log(&format!("detect windows={}", windows.len()));
        let elements_rx = {
            #[cfg(windows)]
            {
                None
            }
            #[cfg(not(windows))]
            {
                let (elem_tx, elem_rx) = std::sync::mpsc::channel();
                let origin = capture.virtual_origin;
                std::thread::spawn(move || {
                    let elems = translated_elements(origin);
                    let _ = elem_tx.send(elems);
                });
                Some(elem_rx)
            }
        };
        let overlay = Rect::new(0.0, 0.0, w as f32, h as f32);
        let mut s = Self {
            window,
            front,
            capture,
            sel: Selection::new(),
            cursor: Point::ZERO,
            hover: overlay,
            windows,
            elements: Vec::new(),
            elements_rx,
            hit_px: None,
            hit_rect: None,
            hit_at: None,
            moved_at: Instant::now(),
            hover_deep: false,
            annot: Annotator::new(),
            toolbar: None,
            hex: false,
            mag_forced: false,
            detect: DetectMode::Element,
            toast: None,
            toast_until: None,
            arrow_kind: ArrowKind::Thin,
            ellipse_on: false,
            line_menu_open: false,
            font_menu_open: false,
            font_menu_offset: 0,
            font_scroll_drag: false,
            dpr: 1.0,
            result: None,
            dirty: true,
            #[cfg(windows)]
            need_stack: true,
            history,
            live_shot: None,
            translate_on_select,
            show_cursor: false,
            mods: ModifiersState::empty(),
            history_max: history_max.max(1),
            held_btn1: false,
            slider_drag: false,
            interacted: false,
            #[cfg(unix)]
            ptr_grab_ok: false,
            btn1_up_streak: 0,
            last_idle_pointer: None,
            wheel: WheelGate::new(),
            confirm: None,
            color_picker: None,
            sel_move0: None,
            #[cfg(unix)]
            proxy: proxy.clone(),
            #[cfg(unix)]
            key_grab: None,
            #[cfg(unix)]
            ptr_hold: None,
            #[cfg(not(windows))]
            surface,
            ime_preedit: false,
        };
        #[cfg(windows)]
        if let Some((sx, sy)) = native::cursor_screen() {
            let (ox, oy) = s.capture.virtual_origin;
            s.cursor = Point::new((sx - ox) as f32, (sy - oy) as f32);
        }
        // Hit-test now, not on the first CursorMoved: F1 with a still mouse
        // must still draw the window / control ring. Stay cloaked so the
        // recycled overlay is not the UIA / z-order hit.
        #[cfg(windows)]
        native::cloak_window(&s.window, true);
        s.update_hover();
        crate::logutil::log(&format!(
            "overlay hover {:.0}x{:.0} at {:.0},{:.0}",
            s.hover.w, s.hover.h, s.cursor.x, s.cursor.y
        ));
        s.load_elements();
        if s.translate_on_select {
            s.show_toast(tr("Select text, then release"));
        }
        #[cfg(unix)]
        {
            // Map before the grab so the overlay is a valid pointer target.
            s.window.set_visible(true);
            native::focus_window(&s.window);
            // Grab the pointer on the overlay *before* the first frame, on the
            // same connection as InputHold (replaces the root grab; no gap for
            // Chrome). Ungrab keyboard inside grab_overlay_pointer so
            // OverlayKeyGrab can take keys.
            if pre_hold.is_none() {
                pre_hold = Some(crate::x11util::InputHold::grab());
            }
            if let Some(xid) = crate::x11util::xid_of(&s.window) {
                if let Some(hold) = pre_hold.as_mut() {
                    s.ptr_grab_ok = hold.grab_overlay_pointer(xid);
                    if !s.ptr_grab_ok {
                        s.ptr_grab_ok = hold.grab_overlay_pointer(xid);
                    }
                }
            }
            s.ptr_hold = pre_hold;
            s.key_grab = native::grab_overlay_keys(&s.window, proxy);
            crate::logutil::log(&format!(
                "overlay mapped (pointer grab ok={})",
                s.ptr_grab_ok
            ));
            s.sync_pointer();
            s.update_hover();
            s.poll_overlay_pointer();
        }
        #[cfg(not(unix))]
        {
            let _ = proxy;
        }
        let t_draw = Instant::now();
        crate::logutil::step("before first redraw");
        s.redraw();
        crate::logutil::ms("overlay first redraw", t_draw);
        #[cfg(windows)]
        crate::logutil::log(&format!(
            "overlay shown {}x{} remote={}",
            w,
            h,
            crate::win32::is_remote_session()
        ));
        #[cfg(not(any(unix, windows)))]
        native::focus_overlay_window(&s.window);
        // Capture already includes the taskbar. Cloak the live explorer
        // chrome so it cannot restack above this HWND and steal clicks.
        let t_trays = Instant::now();
        crate::logutil::step("before cloak trays");
        #[cfg(not(windows))]
        native::cloak_shell_trays(true);
        crate::logutil::ms("overlay start cloak trays", t_trays);
        // After focus the taskbar may jump above an already-TOPMOST overlay.
        let t_cover = Instant::now();
        crate::logutil::step("before cover desktop");
        #[cfg(not(windows))]
        s.cover_desktop();
        crate::logutil::ms("overlay start cover desktop", t_cover);
        // Recycled HWND often skips CursorMoved until the mouse actually
        // moves. Nudge 1px (and back) so hover paints on a still cursor.
        #[cfg(not(windows))]
        {
            let t_hover = Instant::now();
            s.synth_pointer_hover();
            crate::logutil::ms("overlay start synth hover", t_hover);
            let t_second = Instant::now();
            if s.dirty {
                s.redraw();
            }
            crate::logutil::ms("overlay start second redraw", t_second);
        }
        let t_cover2 = Instant::now();
        crate::logutil::step("before cover desktop 2");
        #[cfg(not(windows))]
        s.cover_desktop();
        crate::logutil::ms("overlay start cover desktop 2", t_cover2);
        crate::logutil::ms("overlay start", t0);
        Ok(s)
    }

    fn show_toast(&mut self, text: String) {
        crate::logutil::log_at("trace", "overlay: toast shown");
        self.toast = Some(text);
        self.toast_until = Some(Instant::now() + Duration::from_millis(1200));
        self.mark_dirty();
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.window.request_redraw();
    }

    fn sync_cursor(&mut self) {
        let over_chrome = self.confirm.is_some()
            || self.color_picker.is_some()
            || self
                .toolbar
                .as_ref()
                .is_some_and(|t| t.contains(self.cursor));
        let mut hint = self.annot.cursor_hint(self.img_pt(self.cursor));
        hint.over_chrome = over_chrome;
        hint.sel_state = Some(self.sel.state);
        hint.hover_sel_handle = self.sel.resize_handle().or_else(|| {
            if self.annot.tool.is_none() && self.sel.has_sel() {
                hit_sel_handle(self.sel.normalized(), self.cursor)
            } else {
                None
            }
        });
        hint.over_selection = self.sel.has_sel() && self.sel.normalized().contains(self.cursor);
        crate::cursor::apply_annot(&self.window, cursor_for(hint), &self.annot);
    }

    fn release_grab(&mut self) {
        #[cfg(unix)]
        if let Some(mut g) = self.key_grab.take() {
            g.release();
        }
        #[cfg(unix)]
        {
            self.ptr_hold = None;
        }
    }

    fn expire_toast(&mut self) {
        if let Some(t) = self.toast_until {
            if Instant::now() >= t {
                self.toast = None;
                self.toast_until = None;
                self.mark_dirty();
            }
        }
    }

    fn set_ime_pause(&mut self, pause: bool) {
        #[cfg(unix)]
        if let Some(g) = &self.key_grab {
            g.set_pause_keys(pause);
        }
        #[cfg(not(unix))]
        let _ = pause;
    }

    fn sync_ime(&mut self) {
        let editing = self.annot.text_edit.is_some();
        self.window.set_ime_allowed(editing);
        // Keep XGrabKeyboard while typing so xdotool / Unicode keysyms reach
        // the overlay. Pause only during IME preedit so XIM can compose.
        self.set_ime_pause(editing && self.ime_preedit);
        if let Some(r) = self.annot.text_edit_rect() {
            let dpr = self.dpr.max(0.01);
            self.window.set_ime_cursor_area(
                PhysicalPosition::new((r.x / dpr) as f64, (r.y / dpr) as f64),
                PhysicalSize::new((r.w / dpr).max(8.0) as f64, (r.h / dpr).max(8.0) as f64),
            );
        }
    }

    fn commit_annot_release(&mut self, pt: Point) {
        let before = self.annot.shapes.len();
        self.annot.release(self.img_pt(pt));
        self.sync_ime();
        if self.annot.shapes.len() > before {
            if let Some(sh) = self.annot.shapes.last() {
                let b = sh.bounds();
                crate::logutil::log(&format!(
                    "annot {:?} {:.0}x{:.0} selected={:?}",
                    sh.kind, b.w, b.h, self.annot.selected
                ));
            }
        }
        self.relayout_toolbar();
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn window_rc(&self) -> Rc<Window> {
        self.window.clone()
    }

    pub fn cursor(&self) -> Point {
        self.cursor
    }

    pub fn take_result(&mut self) -> Option<OverlayResult> {
        self.result.take()
    }

    pub fn peek_result(&self) -> Option<&OverlayResult> {
        self.result.as_ref()
    }

    fn bounds(&self) -> Rect {
        Rect::new(
            0.0,
            0.0,
            self.capture.composed.width as f32,
            self.capture.composed.height as f32,
        )
    }

    fn img_pt(&self, p: Point) -> Point {
        Point::new(p.x * self.dpr, p.y * self.dpr)
    }

    fn poll_elements(&mut self) {
        let Some(rx) = self.elements_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(elems) => {
                crate::logutil::log(&format!(
                    "detect elements={} ({})",
                    elems.len(),
                    element_source()
                ));
                self.elements = elems;
                self.elements_rx = None;
                self.update_hover();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.elements_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn snap_guides(&self) -> SnapGuides {
        let mut rects = self.windows.clone();
        rects.extend(self.elements.iter().copied());
        rects.push(self.bounds());
        SnapGuides::from_rects(rects)
    }

    fn hover_at(&mut self, pt: Point) -> Rect {
        if self.detect == DetectMode::Element {
            self.live_element_at(pt)
        } else {
            hover_rect(pt, &self.windows, &[], self.bounds())
        }
    }

    fn exclude_hwnd(&self) -> Option<isize> {
        #[cfg(windows)]
        {
            Some(hwnd_of(&self.window).0 as isize)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn live_element_at(&mut self, pt: Point) -> Rect {
        let origin = self.capture.virtual_origin;
        let key = screen_from_overlay(pt, origin);
        if self.hit_px == Some(key) {
            if let Some(r) = self.hit_rect {
                return r;
            }
        }
        if let (Some(at), Some(r)) = (self.hit_at, self.hit_rect) {
            if at.elapsed() < std::time::Duration::from_millis(8) {
                return r;
            }
        }
        self.hit_px = Some(key);
        self.hit_at = Some(Instant::now());
        let deep = self.moved_at.elapsed() >= std::time::Duration::from_millis(48);
        let hit = if deep {
            self.hover_deep = true;
            element_at_point(key.0, key.1, self.exclude_hwnd())
        } else {
            window_at_point(key.0, key.1, self.exclude_hwnd())
        };
        let hover = if let Some(hit) = hit {
            overlay_from_screen(hit.rect, origin)
        } else {
            hover_rect(pt, &self.windows, self.elements.as_slice(), self.bounds())
        };
        self.hit_rect = Some(hover);
        hover
    }

    fn update_hover(&mut self) {
        if self.sel.state != SelState::Idle || self.sel.has_sel() {
            return;
        }
        self.hover = self.hover_at(self.cursor);
    }

    /// Pretend the mouse moved one overlay pixel so live hit-test / winit
    /// CursorMoved run even when the user has not moved.
    fn synth_pointer_hover(&mut self) {
        native::nudge_cursor();
        #[cfg(windows)]
        if let Some((sx, sy)) = native::cursor_screen() {
            let (ox, oy) = self.capture.virtual_origin;
            self.cursor = Point::new((sx - ox) as f32, (sy - oy) as f32);
        }
        let origin = self.cursor;
        let b = self.bounds();
        let dx = if origin.x + 1.0 < b.w.max(1.0) {
            1.0
        } else {
            -1.0
        };
        self.hit_px = None;
        self.hit_rect = None;
        self.on_cursor_moved(Point::new(origin.x + dx, origin.y));
        self.hit_px = None;
        self.hit_rect = None;
        self.on_cursor_moved(origin);
    }

    fn relayout_toolbar(&mut self) {
        if self.sel.has_sel() && self.sel.state == SelState::Selected {
            let mut tb = Toolbar::layout(
                self.sel.normalized(),
                self.bounds(),
                self.annot.tool,
                self.arrow_kind,
                self.annot.option_style(),
            );
            tb.set_width_value(self.annot.option_width());
            if self.font_menu_open && self.annot.tool == Some(Tool::Text) {
                if let Some(st) = self.annot.option_style() {
                    self.font_menu_offset = tb.attach_font_menu(self.bounds(), st, self.font_menu_offset);
                }
            } else {
                self.font_menu_open = false;
            }
            if self.line_menu_open && self.annot.tool == Some(Tool::Line) {
                if let Some(st) = self.annot.option_style() {
                    tb.attach_line_menu(self.bounds(), st);
                }
            } else {
                self.line_menu_open = false;
            }
            self.toolbar = Some(tb);
        } else {
            self.toolbar = None;
        }
    }

    fn log_tb_chrome(&self) {
        let Some(tb) = &self.toolbar else {
            return;
        };
        crate::logutil::log_at("trace", &format!(
            "tb bar {:.0},{:.0} {:.0}x{:.0}",
            tb.bar.x, tb.bar.y, tb.bar.w, tb.bar.h
        ));
        for (name, act) in [
            ("Rect", TbAction::Shape),
            ("Line", TbAction::LineStroke),
            ("Pen", TbAction::Tool(Tool::Pen)),
            ("Mosaic", TbAction::Tool(Tool::Mosaic)),
            ("Text", TbAction::Tool(Tool::Text)),
            ("Eraser", TbAction::Tool(Tool::Eraser)),
            ("Ocr", TbAction::Ocr),
            ("Pin", TbAction::Pin),
        ] {
            if let Some(c) = tb.button_center(act) {
                crate::logutil::log_at("trace", &format!("tb {name} at {:.0},{:.0}", c.x, c.y));
            }
        }
        if let Some(pr) = tb.panel {
            crate::logutil::log_at("trace", &format!(
                "tb panel {:.0},{:.0} {:.0}x{:.0}",
                pr.x, pr.y, pr.w, pr.h
            ));
        }
    }

    fn crop(&mut self) -> Option<Raster> {
        if !self.sel.has_sel() {
            return None;
        }
        self.annot.finish();
        let mut composed = compose(&self.capture.composed, &self.annot.shapes);
        if self.show_cursor {
            if let Some(mut pm) = composed.to_skia() {
                crate::draw::paint_pointer(&mut pm, self.img_pt(self.cursor));
                if let Some(img) = crate::draw::pixmap_to_raster(&pm) {
                    composed = img;
                }
            }
        }
        Some(composed.crop(self.sel.normalized()))
    }

    fn remember(&mut self) {
        self.history.push(
            HistoryEntry::new(self.sel.normalized(), self.capture.composed.clone()),
            self.history_max,
        );
    }

    fn finish_copy(&mut self) {
        if let Some(img) = self.crop() {
            self.remember();
            crate::logutil::log(&format!("overlay copy {}x{}", img.width, img.height));
            self.release_grab();
            self.result = Some(OverlayResult::Copy(img, self.sel.normalized()));
            self.window.set_visible(false);
        }
    }

    fn finish_pin(&mut self) {
        if let Some(img) = self.crop() {
            self.remember();
            let (vx, vy) = self.capture.virtual_origin;
            let r = self.sel.normalized();
            self.release_grab();
            self.result = Some(OverlayResult::Pin(
                img,
                r,
                (vx + r.x as i32, vy + r.y as i32),
            ));
            self.window.set_visible(false);
        }
    }

    fn finish_save(&mut self) {
        if let Some(img) = self.crop() {
            self.remember();
            self.release_grab();
            self.result = Some(OverlayResult::Save(img));
            self.window.set_visible(false);
        }
    }

    fn finish_ocr(&mut self, translate: bool) {
        if !self.sel.has_sel() {
            return;
        }
        let img = self.capture.composed.crop(self.sel.normalized());
        self.remember();
        let pos = native::pin_pos_near_cursor(img.width, img.height);
        self.release_grab();
        self.result = Some(OverlayResult::Ocr(img, translate, pos));
        self.window.set_visible(false);
    }

    fn step_history(&mut self, dir: i32) {
        if self.history.entries.is_empty() {
            return;
        }
        if self.live_shot.is_none() {
            self.live_shot = Some(self.capture.composed.clone());
        }
        let replay = match self.history.step(dir) {
            HistoryView::Live => None,
            HistoryView::Replay(e) => {
                let img = if e.resident() {
                    Some(e.image.clone())
                } else {
                    e.disk.as_deref().and_then(load_hist_raster)
                };
                img.map(|image| (e.rect, image))
            }
        };
        match replay {
            None => {
                if let Some(live) = self.live_shot.clone() {
                    self.apply_replay(&live, None);
                }
                self.show_toast(tr("Snip history"));
            }
            Some((rect, img)) => {
                self.apply_replay(&img, Some(rect));
                self.show_toast(tr("Snip history"));
            }
        }
    }

    fn reuse_last(&mut self) {
        if let Some(e) = self.history.last().cloned() {
            // R reapplies the last selection rect on the *current* capture.
            // `,` / `.` still replace the shot via apply_replay.
            self.annot.clear();
            self.sel.set_rect(e.rect, self.bounds());
            self.relayout_toolbar();
            crate::logutil::log(&format!(
                "R last rect {:.0}x{:.0} on live capture {}x{}",
                e.rect.w, e.rect.h, self.capture.composed.width, self.capture.composed.height
            ));
            self.show_toast(tr("Last snip area"));
            self.mark_dirty();
        }
    }

    fn apply_replay(&mut self, img: &Raster, rect: Option<Rect>) {
        self.capture.composed = img.clone();
        let _ = self.front.replace_shot(img, hwnd_of(&self.window));
        self.annot.clear();
        if let Some(r) = rect {
            self.sel.set_rect(r, self.bounds());
            self.relayout_toolbar();
        }
        // Live (no rect): Python keeps the current selection.
        self.mark_dirty();
    }

    fn load_elements(&mut self) {
        self.hit_px = None;
        self.hit_rect = None;
        #[cfg(not(windows))]
        {
            self.poll_elements();
            if self.elements.is_empty() {
                self.elements = translated_elements(self.capture.virtual_origin);
                crate::logutil::log(&format!(
                    "detect elements={} ({})",
                    self.elements.len(),
                    element_source()
                ));
            }
        }
        #[cfg(windows)]
        {
            crate::logutil::log("detect element mode=live hit-test");
        }
    }

    fn cancel(&mut self) {
        crate::logutil::log("overlay cancel");
        self.release_grab();
        self.result = Some(OverlayResult::Cancel);
        self.hide_for_capture();
    }

    pub fn resnip(&mut self) {
        crate::logutil::log("overlay F1 resnip");
        self.release_grab();
        self.result = Some(OverlayResult::Resnip);
        self.hide_for_capture();
    }

    pub fn hide_for_capture(&self) {
        #[cfg(windows)]
        native::cloak_window(&self.window, true);
        native::cloak_shell_trays(false);
        self.window.set_visible(false);
        crate::native::unmap_sync(&self.window);
    }

    fn apply_tb(&mut self, a: TbAction) {
        match a {
            TbAction::Shape
            | TbAction::LineStroke
            | TbAction::Tool(Tool::Pen)
            | TbAction::Tool(Tool::Marker)
            | TbAction::Tool(Tool::Mosaic)
            | TbAction::Tool(Tool::Text)
            | TbAction::Tool(Tool::Number)
            | TbAction::Tool(Tool::Eraser)
            | TbAction::Tool(Tool::Select) => {
                self.line_menu_open = false;
                crate::toolbar::toggle_tool(
                    &mut self.annot,
                    &mut self.arrow_kind,
                    &mut self.ellipse_on,
                    a,
                )
            }
            TbAction::Tool(Tool::Rect) => {
                self.line_menu_open = false;
                crate::toolbar::set_shape_kind(&mut self.annot, &mut self.ellipse_on, false);
            }
            TbAction::Tool(Tool::Ellipse) => {
                self.line_menu_open = false;
                crate::toolbar::set_shape_kind(&mut self.annot, &mut self.ellipse_on, true);
            }
            TbAction::Tool(Tool::Line) => {
                self.line_menu_open = false;
                crate::toolbar::toggle_tool(
                    &mut self.annot,
                    &mut self.arrow_kind,
                    &mut self.ellipse_on,
                    TbAction::LineStroke,
                )
            }
            TbAction::ToggleLineMenu => {
                self.line_menu_open = !self.line_menu_open;
            }
            TbAction::LinePreset(kind) => {
                crate::toolbar::set_line_preset(&mut self.annot, &mut self.arrow_kind, kind);
                self.line_menu_open = false;
            }
            TbAction::ToggleLineArrow => {
                crate::toolbar::toggle_line_arrow(&mut self.annot, &mut self.arrow_kind);
            }
            TbAction::Undo => {
                self.annot.undo();
            }
            TbAction::Redo => {
                self.annot.redo();
            }
            TbAction::Close => self.request_abort(),
            TbAction::Pin => self.finish_pin(),
            TbAction::Save => self.finish_save(),
            TbAction::Copy => self.finish_copy(),
            TbAction::Ocr => self.finish_ocr(false),
            TbAction::Check => {}
            TbAction::Width(d) => {
                self.annot.adjust_width(d);
            }
            TbAction::WidthSet(v) => {
                self.annot.set_option_width(v);
            }
            TbAction::StrengthSet(v) => {
                self.annot.set_style_value(Tool::Mosaic, |s| s.size = v.clamp(1, 30));
            }
            TbAction::Color(i) => {
                if let Some(tool) = self.annot.tool.filter(|t| *t != Tool::Select) {
                    let c = pinshot_core::palette_colors()[i as usize];
                    self.annot.set_style_value(tool, |s| s.color = c);
                }
            }
            TbAction::CustomColor => self.pick_custom_color(),
            TbAction::Fill(on) => self.style_tool(|s| s.filled = on),
            TbAction::LineStyle(ls) => self.style_tool(|s| s.line_style = ls),
            TbAction::SetMode(mode) => {
                if let Some(tool) = self.annot.option_tool() {
                    self.annot.set_draw_mode(tool, mode);
                }
            }
            TbAction::SetEffect(effect) => self.style_tool(|s| s.effect = effect),
            TbAction::Bold(on) => {
                self.style_tool(|s| s.bold = on);
                self.log_font_resolve();
            }
            TbAction::Italic(on) => {
                self.style_tool(|s| s.italic = on);
                self.log_font_resolve();
            }
            TbAction::Outline(on) => self.style_tool(|s| s.outline = on),
            TbAction::Family(fam) => {
                self.font_menu_open = false;
                self.style_tool(|s| s.family = fam.to_string());
                self.log_font_resolve();
            }
            TbAction::ToggleFontMenu => {
                self.font_menu_open = !self.font_menu_open;
                if self.font_menu_open {
                    let family = self.annot.option_style().map(|s| s.family.as_str()).unwrap_or("");
                    self.font_menu_offset = pinshot_core::font::available_families().iter()
                        .position(|f| f.eq_ignore_ascii_case(family)).unwrap_or(0).saturating_sub(3);
                }
            }
            TbAction::FontMenuScroll(delta) => {
                self.font_menu_offset = self.font_menu_offset.saturating_add_signed(delta as isize);
            }
        }
        crate::logutil::log_at("trace", &format!("tb {a:?}"));
        if self.result.is_some() {
            return;
        }
        self.relayout_toolbar();
        self.held_btn1 = true;
        self.mark_dirty();
    }

    fn style_tool(&mut self, setter: impl Fn(&mut pinshot_core::annotation::Style)) {
        if let Some(tool) = self.annot.option_tool().filter(|t| *t != Tool::Select) {
            self.annot.set_style_value(tool, setter);
        }
    }

    fn log_font_resolve(&self) {
        let st = self.annot.option_style();
        let fam = st.map(|s| s.family.as_str()).unwrap_or("");
        let bold = st.map(|s| s.bold).unwrap_or(false);
        let italic = st.map(|s| s.italic).unwrap_or(false);
        if let Some(p) = pinshot_core::font::resolve_family_path(fam, bold, italic) {
            crate::logutil::log_at("trace", &format!(
                "font resolve {fam:?} bold={bold} italic={italic} -> {}",
                p.display()
            ));
        }
    }

    fn pick_custom_color(&mut self) {
        let Some(tool) = self.annot.option_tool().filter(|t| *t != Tool::Select) else {
            return;
        };
        let cur = self
            .annot
            .option_style()
            .map(|s| s.color)
            .unwrap_or_else(|| self.annot.style(tool).color);
        self.color_picker = Some(ColorPicker::new(cur));
        crate::logutil::log(&format!("color picker open {}", cur.hex()));
        self.mark_dirty();
    }

    fn picker_layout(&self) -> pinshot_core::ColorPickerLayout {
        let b = self.bounds();
        color_picker_layout(b.w, b.h)
    }

    fn picker_press(&mut self, pt: Point) {
        let lay = self.picker_layout();
        let Some(picker) = self.color_picker.as_mut() else {
            return;
        };
        let Some(hit) = lay.hit(pt) else {
            return;
        };
        match picker.press(hit, pt, &lay) {
            PickerAction::Accept => self.accept_picker(),
            PickerAction::Cancel => self.close_picker(),
            PickerAction::Continue => self.mark_dirty(),
        }
    }

    fn picker_drag(&mut self, pt: Point) {
        let lay = self.picker_layout();
        if let Some(picker) = self.color_picker.as_mut() {
            if picker.drag.is_some() {
                picker.drag_to(pt, &lay);
                self.dirty = true;
            }
        }
    }

    fn accept_picker(&mut self) {
        let Some(picker) = self.color_picker.take() else {
            return;
        };
        if let Some(tool) = self.annot.option_tool().filter(|t| *t != Tool::Select) {
            let c = picker.current;
            self.annot.set_style_value(tool, |s| s.color = c);
            crate::logutil::log(&format!("custom color {}", c.hex()));
        }
        self.relayout_toolbar();
        self.mark_dirty();
    }

    fn close_picker(&mut self) {
        if self.color_picker.take().is_some() {
            crate::logutil::log("color picker cancel");
            self.mark_dirty();
        }
    }

    fn finish_sel_drag(&mut self, pt: Point) {
        let guides = self.snap_guides();
        // Recompute at the click, not a stale hover from before the overlay
        // mapped (that is how a dock strip was adopted over Chrome).
        let hover = self.hover_at(pt);
        self.hover = hover;
        self.sel
            .release_snapped(pt, hover, self.bounds(), Some(&guides));
        if self.sel.has_sel() {
            let r = self.sel.normalized();
            crate::logutil::log(&format!(
                "sel done {:.0}x{:.0} at {:.0},{:.0}",
                r.w, r.h, r.x, r.y
            ));
        }
        self.log_sel_translate();
        self.relayout_toolbar();
        self.log_tb_chrome();
    }

    fn apply_sel_motion(&mut self, pt: Point) {
        let guides = self.snap_guides();
        self.sel.motion_snapped(pt, self.bounds(), Some(&guides));
    }

    fn log_sel_translate(&mut self) {
        if let Some(o) = self.sel_move0.take() {
            let n = self.sel.normalized();
            let dx = (n.x - o.x) * self.dpr;
            let dy = (n.y - o.y) * self.dpr;
            if dx != 0.0 || dy != 0.0 {
                crate::logutil::log_at("trace", &format!("sel move {dx:.0},{dy:.0}"));
            }
        }
    }

    fn sync_pointer(&mut self) {
        if let Some((x, y, _)) = native::pointer_in_window(&self.window) {
            let (vx, vy) = self.capture.virtual_origin;
            self.cursor = Point::new((x - vx) as f32, (y - vy) as f32);
        }
    }

    #[cfg(unix)]
    fn overlay_pt_from_root(&self, root_x: i32, root_y: i32) -> Point {
        let (vx, vy) = self.capture.virtual_origin;
        Point::new((root_x - vx) as f32, (root_y - vy) as f32)
    }

    #[cfg(unix)]
    fn ensure_overlay_grab(&mut self) {
        if self.ptr_grab_ok {
            return;
        }
        let Some(xid) = crate::x11util::xid_of(&self.window) else {
            return;
        };
        if self.ptr_hold.is_none() {
            self.ptr_hold = Some(crate::x11util::InputHold::grab());
        }
        if let Some(hold) = self.ptr_hold.as_mut() {
            self.ptr_grab_ok = hold.grab_overlay_pointer(xid);
            if self.ptr_grab_ok {
                crate::logutil::log("overlay XGrabPointer recovered");
            }
        }
    }

    #[cfg(unix)]
    fn poll_overlay_pointer(&mut self) {
        let evs = match self.ptr_hold.as_mut() {
            Some(hold) => hold.poll_pointer(),
            None => return,
        };
        for ev in evs {
            let pt = self.overlay_pt_from_root(ev.root_x, ev.root_y);
            match ev.kind {
                crate::x11util::PtrKind::Move => self.on_cursor_moved(pt),
                crate::x11util::PtrKind::Press => {
                    if let Some(btn) = xbutton(ev.detail) {
                        self.cursor = pt;
                        self.on_mouse_button(btn, true);
                    } else {
                        self.on_cursor_moved(pt);
                    }
                }
                crate::x11util::PtrKind::Release => {
                    if let Some(btn) = xbutton(ev.detail) {
                        self.cursor = pt;
                        self.on_mouse_button(btn, false);
                    } else {
                        self.on_cursor_moved(pt);
                    }
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn poll_overlay_pointer(&mut self) {}

    fn on_cursor_moved(&mut self, next: Point) {
        if self.slider_drag {
            self.cursor = next;
            if let Some(TbAction::StrengthSet(v)) = self.toolbar.as_ref().and_then(|t| t.hit(next)) {
                self.apply_tb(TbAction::StrengthSet(v));
                self.mark_dirty();
            }
            return;
        }
        if self.color_picker.is_some() {
            self.cursor = next;
            self.picker_drag(self.cursor);
            return;
        }
        if !self.held_btn1 {
            self.last_idle_pointer = Some(next);
        }
        if self.confirm.is_some() {
            self.cursor = next;
            return;
        }
        let same_px =
            next.x.round() == self.cursor.x.round() && next.y.round() == self.cursor.y.round();
        if !same_px {
            self.moved_at = Instant::now();
            self.hover_deep = false;
        }
        self.cursor = next;
        if self.font_scroll_drag {
            if let Some(tb) = &self.toolbar {
                if let Some(offset) = tb.font_menu_drag_offset(next) {
                    self.font_menu_offset = offset;
                    self.relayout_toolbar();
                    self.dirty = true;
                }
            }
            return;
        }
        if self.annot.is_brush_live()
            || self.annot.is_selecting()
            || self.annot.is_dragging_text()
            || self.annot.live.is_some()
        {
            self.annot
                .move_to(self.img_pt(self.cursor), self.mods.shift_key());
            self.dirty = true;
        } else if self
            .toolbar
            .as_ref()
            .is_some_and(|t| t.contains(self.cursor))
        {
            self.dirty = true;
        } else if matches!(
            self.sel.state,
            SelState::Creating | SelState::Moving | SelState::Resizing
        ) {
            self.apply_sel_motion(self.cursor);
            self.dirty = true;
        } else {
            let prev_hover = self.hover;
            self.update_hover();
            if !same_px || prev_hover != self.hover {
                self.dirty = true;
            }
        }
    }

    fn on_mouse_button(&mut self, button: MouseButton, pressed: bool) {
        if self.color_picker.is_some() {
            if pressed && button == MouseButton::Left {
                self.picker_press(self.cursor);
            } else if !pressed && button == MouseButton::Left {
                if let Some(p) = self.color_picker.as_mut() {
                    p.release();
                }
            }
            return;
        }
        let live_sel = matches!(
            self.sel.state,
            SelState::Creating | SelState::Moving | SelState::Resizing
        );
        let pt = self.cursor;
        if button == MouseButton::Left {
            if pressed {
                if let Some(TbAction::StrengthSet(v)) = self.toolbar.as_ref().and_then(|t| t.hit(pt)) {
                    self.slider_drag = true;
                    self.apply_tb(TbAction::StrengthSet(v));
                    self.mark_dirty();
                    return;
                }
            } else {
                self.slider_drag = false;
            }
        }
        if pressed && self.confirm.is_some() {
            if !self.held_btn1 {
                let _ = self.hit_confirm(pt);
            }
            self.held_btn1 = button == MouseButton::Left;
            return;
        }
        if pressed {
            native::focus_window(&self.window);
        }
        if pressed && button == MouseButton::Left {
            self.held_btn1 = true;
            self.interacted = true;
            if let Some(tb) = &self.toolbar {
                if self.font_menu_open && tb.font_menu_drag_offset(pt).is_some() {
                    self.font_scroll_drag = true;
                    if let Some(offset) = tb.font_menu_drag_offset(pt) {
                        self.font_menu_offset = offset;
                        self.relayout_toolbar();
                    }
                    self.mark_dirty();
                    return;
                }
                if let Some(a) = tb.hit(pt) {
                    self.apply_tb(a);
                    return;
                }
            }
            if self.line_menu_open || self.font_menu_open {
                self.line_menu_open = false;
                self.font_menu_open = false;
                self.relayout_toolbar();
                self.mark_dirty();
                return;
            }
            if live_sel {
                self.apply_sel_motion(pt);
                self.dirty = true;
                return;
            }
            if self.sel.has_sel() && self.annot.press(self.img_pt(pt), self.mods.shift_key()) {
                self.sync_ime();
                self.relayout_toolbar();
                self.mark_dirty();
                return;
            }
            let anchor = handle_press_point(
                self.last_idle_pointer,
                pt,
                self.sel.has_sel(),
                self.sel.normalized(),
            );
            let mut kind = self
                .sel
                .press(anchor, self.bounds(), self.annot.tool.is_some());
            if kind == PressKind::Ignore && self.sel.has_sel() {
                kind = self.sel.press(pt, self.bounds(), self.annot.tool.is_some());
            }
            match kind {
                PressKind::Ignore => {}
                PressKind::Move => {
                    self.sel_move0 = Some(self.sel.normalized());
                    self.toolbar = None;
                }
                _ => self.toolbar = None,
            }
            if matches!(
                self.sel.state,
                SelState::Creating | SelState::Moving | SelState::Resizing
            ) {
                self.apply_sel_motion(pt);
            }
            self.dirty = true;
        } else if pressed && button == MouseButton::Middle {
            if self.annot.text_edit.is_some() {
                if let Ok(s) = crate::clipboard::read_text() {
                    self.annot.insert_str(&s);
                    self.sync_ime();
                    self.mark_dirty();
                }
            }
        } else if pressed && button == MouseButton::Right {
            if self.annot.live.as_ref().is_some_and(|s| s.kind == Tool::Line) {
                self.annot.move_to(self.img_pt(pt), self.mods.shift_key());
            }
            if self.annot.right_click() {
                self.sync_ime();
                self.relayout_toolbar();
                self.mark_dirty();
                return;
            }
            if self.sel.has_sel() {
                self.sel.reset();
                self.annot.set_tool(None);
                self.annot.clear();
                self.toolbar = None;
                self.update_hover();
            } else {
                self.cancel();
            }
            self.dirty = true;
        } else if !pressed && button == MouseButton::Left {
            self.held_btn1 = false;
            self.font_scroll_drag = false;
            if self.annot.live.is_some()
                || self.annot.is_selecting()
                || self.annot.is_dragging_text()
                || self.annot.is_swallowing_text_click()
                || self.annot.is_editing_text()
            {
                self.commit_annot_release(pt);
            } else {
                self.finish_sel_drag(pt);
                if self.translate_on_select && self.sel.has_sel() {
                    self.finish_ocr(true);
                }
            }
            self.dirty = true;
        }
    }

    fn request_abort(&mut self) {
        let gesturing = self.sel.has_sel()
            || self.interacted
            || self.held_btn1
            || !matches!(self.sel.state, SelState::Idle);
        if !gesturing {
            self.cancel();
            return;
        }
        let skip = pinshot_core::config::load()
            .map(|c| c.skip_abort_confirm)
            .unwrap_or(false);
        if skip {
            self.cancel();
            return;
        }
        if self.confirm.is_some() {
            return;
        }
        self.annot.finish();
        self.confirm = Some(ConfirmAbort { dont: false });
        crate::logutil::log("confirm abort shown");
        self.mark_dirty();
    }

    fn confirm_yes(&mut self) {
        let dont = self.confirm.as_ref().map(|c| c.dont).unwrap_or(false);
        self.confirm = None;
        if dont {
            if let Ok(mut cfg) = pinshot_core::config::load() {
                cfg.skip_abort_confirm = true;
                let _ = pinshot_core::config::save(&cfg);
            }
            crate::logutil::log("confirm abort yes (do not show again)");
        } else {
            crate::logutil::log("confirm abort yes");
        }
        self.cancel();
    }

    fn confirm_no(&mut self) {
        self.confirm = None;
        crate::logutil::log("confirm abort no");
        self.mark_dirty();
    }

    fn hit_confirm(&mut self, pt: Point) -> bool {
        if self.confirm.is_none() {
            return false;
        }
        let b = self.bounds();
        let lay = confirm_abort_layout(b.w, b.h);
        match lay.hit(pt) {
            Some(ConfirmAbortHit::Yes) => self.confirm_yes(),
            Some(ConfirmAbortHit::No) => self.confirm_no(),
            Some(ConfirmAbortHit::Dont) => {
                if let Some(c) = self.confirm.as_mut() {
                    c.dont = !c.dont;
                    crate::logutil::log(&format!("confirm abort dont={}", c.dont));
                }
                self.mark_dirty();
            }
            None => {}
        }
        true
    }

    pub fn handle(&mut self, event: &WindowEvent) {
        self.poll_elements();
        self.poll_overlay_pointer();
        if self.color_picker.is_some() {
            match event {
                WindowEvent::CursorMoved { position, .. } => {
                    self.cursor = Point::new(position.x as f32, position.y as f32);
                    self.picker_drag(self.cursor);
                }
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Left,
                    ..
                } => {
                    if *state == ElementState::Pressed {
                        self.picker_press(self.cursor);
                    } else if let Some(p) = self.color_picker.as_mut() {
                        p.release();
                    }
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => self.close_picker(),
                        Key::Named(NamedKey::Enter) => self.accept_picker(),
                        _ => {}
                    }
                }
                WindowEvent::RedrawRequested => self.redraw(),
                WindowEvent::CloseRequested => {
                    crate::logutil::log("overlay CloseRequested ignored");
                }
                _ => {}
            }
            if self.dirty {
                self.window.request_redraw();
            }
            self.sync_cursor();
            return;
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor_moved(Point::new(position.x as f32, position.y as f32));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_button(*button, *state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.confirm.is_some() {
                    return;
                }
                self.sync_pointer();
                let raw = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (*y * 120.0) as i32,
                    MouseScrollDelta::PixelDelta(p) => p.y as i32,
                };
                let Some(angle) = self.wheel.take(raw) else {
                    return;
                };
                if self.font_menu_open {
                    self.font_menu_offset = self.font_menu_offset.saturating_add_signed(if angle > 0 { -3 } else { 3 });
                    self.relayout_toolbar();
                    self.mark_dirty();
                    return;
                }
                if self.annot.wheel_at(self.img_pt(self.cursor), angle) {
                    crate::logutil::log_at("trace", &format!(
                        "overlay wheel {angle:+} width={}",
                        self.annot.option_width()
                    ));
                    self.relayout_toolbar();
                    self.mark_dirty();
                }
            }
            WindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Commit(s) => {
                    crate::logutil::log_at("trace", &format!("text: IME committed {} characters", s.chars().count()));
                    self.annot.insert_str(s);
                    self.ime_preedit = false;
                    self.sync_ime();
                    self.mark_dirty();
                }
                winit::event::Ime::Preedit(s, _) => {
                    self.ime_preedit = !s.is_empty();
                    self.sync_ime();
                }
                winit::event::Ime::Enabled => {
                    self.sync_ime();
                }
                winit::event::Ime::Disabled => {
                    self.ime_preedit = false;
                    self.sync_ime();
                }
            },
            WindowEvent::KeyboardInput { event, is_synthetic, .. }
                if event.state == ElementState::Pressed => {
                // A held capture key synthesized on focus is not a new action.
                if !overlay_key_is_actionable(&event.logical_key, *is_synthetic, event.repeat) {
                    return;
                }
                self.key(&event.logical_key, event.text.as_deref());
            }
            WindowEvent::Focused(focused) => {
                if *focused {
                    native::focus_overlay_window(&self.window);
                    #[cfg(unix)]
                    if let Some(g) = &self.key_grab {
                        g.kick();
                    }
                    self.sync_ime();
                } else if self.result.is_none() {
                    native::focus_overlay_window(&self.window);
                    #[cfg(unix)]
                    if let Some(g) = &self.key_grab {
                        g.kick();
                    }
                    self.sync_ime();
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                self.mag_forced = self.mods.contains(ModifiersState::ALT);
                self.dirty = true;
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::CloseRequested => {
                crate::logutil::log("overlay CloseRequested ignored");
            }
            _ => {}
        }
        if self.dirty {
            self.window.request_redraw();
        }
        self.sync_cursor();
    }

    pub fn handle_x11_key(&mut self, keysym: u32, mods: u16, pressed: bool) {
        self.mods = x11_mods_to_winit(mods);
        self.mag_forced = self.mods.contains(ModifiersState::ALT);
        if !pressed {
            self.mark_dirty();
            return;
        }
        let Some((key, text)) = keysym_to_winit(keysym).or_else(|| {
            // Direct UCS keysym without the 0x01000000 prefix (some xdotool setups).
            let ch = char::from_u32(keysym)?;
            if ch.is_control() || (keysym >= 0xff00 && keysym <= 0xffff) {
                return None;
            }
            let s = ch.to_string();
            Some((Key::Character(s.clone().into()), Some(s)))
        }) else {
            return;
        };
        self.key(&key, text.as_deref());
        self.sync_ime();
        self.sync_cursor();
        self.window.request_redraw();
    }

    fn key(&mut self, key: &Key, text: Option<&str>) {
        if self.font_menu_open && *key == Key::Named(NamedKey::Escape) {
            self.font_menu_open = false;
            self.relayout_toolbar();
            self.mark_dirty();
            return;
        }
        if self.color_picker.is_some() {
            match key {
                Key::Named(NamedKey::Enter) => self.accept_picker(),
                Key::Named(NamedKey::Escape) => self.close_picker(),
                _ => {}
            }
            return;
        }
        if self.confirm.is_some() {
            match key {
                Key::Named(NamedKey::Enter) => self.confirm_yes(),
                Key::Named(NamedKey::Escape) => self.confirm_no(),
                Key::Named(NamedKey::F1) => {
                    self.confirm = None;
                    self.resnip();
                }
                Key::Character(c) if c.eq_ignore_ascii_case("y") => self.confirm_yes(),
                Key::Character(c) if c.eq_ignore_ascii_case("n") => self.confirm_no(),
                _ => {}
            }
            return;
        }
        if self.annot.text_edit.is_some() {
            let ctrl = self.mods.control_key();
            let shift = self.mods.shift_key();
            match key {
                Key::Named(NamedKey::Escape) => self.annot.commit_text(true),
                Key::Named(NamedKey::Enter) => {
                    if shift {
                        self.annot.type_text('\n');
                    } else {
                        if let Some(ed) = &self.annot.text_edit {
                            crate::logutil::log_at("trace", &format!("text: committed {} characters", ed.text.chars().count()));
                        }
                        self.annot.commit_text(false);
                    }
                }
                Key::Named(NamedKey::Backspace) => self.annot.type_text('\u{8}'),
                Key::Named(NamedKey::Delete) => self.annot.type_text('\u{7f}'),
                Key::Named(NamedKey::ArrowLeft) => self.annot.move_caret(-1),
                Key::Named(NamedKey::ArrowRight) => self.annot.move_caret(1),
                Key::Named(NamedKey::Home) => self.annot.move_caret(-2),
                Key::Named(NamedKey::End) => self.annot.move_caret(2),
                Key::Named(NamedKey::F1) => {
                    self.annot.commit_text(true);
                    self.resnip();
                    return;
                }
                Key::Character(c) if c == "v" && ctrl => {
                    if let Ok(s) = crate::clipboard::read_text() {
                        self.annot.insert_str(&s);
                    }
                }
                Key::Character(c) if (c == "1" || c == "[") && !ctrl => {
                    self.annot.adjust_width(-1);
                    self.relayout_toolbar();
                }
                Key::Character(c) if (c == "2" || c == "]") && !ctrl => {
                    self.annot.adjust_width(1);
                    self.relayout_toolbar();
                }
                _ => {
                    if ctrl {
                        // Ctrl+Enter already handled; do not insert 'c' from Ctrl+C etc.
                    } else if let Some(t) = text {
                        self.annot.insert_str(t);
                    }
                }
            }
            self.sync_ime();
            self.mark_dirty();
            return;
        }
        let ctrl = self.mods.control_key();
        let _shift = self.mods.shift_key();
        match key {
            Key::Named(NamedKey::Escape) => self.request_abort(),
            Key::Named(NamedKey::F1) => self.resnip(),
            Key::Named(NamedKey::F5) => {
                if self.sel.has_sel() {
                    self.finish_ocr(true);
                }
            }
            Key::Named(NamedKey::Enter) => {
                if self.sel.has_sel() {
                    self.finish_copy();
                } else {
                    crate::logutil::log("Enter with no selection");
                }
            }
            Key::Character(c) if c.eq_ignore_ascii_case("c") && ctrl => self.finish_copy(),
            Key::Named(NamedKey::F3) => self.finish_pin(),
            Key::Character(c) if c.eq_ignore_ascii_case("t") && ctrl => self.finish_pin(),
            Key::Character(c) if c.eq_ignore_ascii_case("t") && self.sel.has_sel() => {
                self.annot.set_tool(Some(Tool::Text));
                self.relayout_toolbar();
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("s") && ctrl => self.finish_save(),
            Key::Character(c) if c.eq_ignore_ascii_case("z") && ctrl => {
                self.annot.undo();
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("y") && ctrl => {
                self.annot.redo();
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("a") && ctrl => {
                self.sel.set_rect(self.bounds(), self.bounds());
                self.relayout_toolbar();
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("r") => self.reuse_last(),
            Key::Character(c) if c.eq_ignore_ascii_case("c") && !ctrl => self.copy_color(),
            Key::Named(NamedKey::Shift) => {
                self.hex = !self.hex;
                self.dirty = true;
            }
            Key::Named(NamedKey::Tab) => {
                if self.sel.has_sel()
                    && crate::toolbar::toggle_grouped_variant(
                        &mut self.annot,
                        &mut self.arrow_kind,
                        &mut self.ellipse_on,
                    )
                {
                    self.relayout_toolbar();
                    self.dirty = true;
                    return;
                }
                crate::logutil::log("overlay Tab swallowed");
                self.detect = self.detect.toggle();
                if self.detect == DetectMode::Element {
                    self.load_elements();
                }
                crate::logutil::log(&format!(
                    "detect mode={} ({})",
                    self.detect.id(),
                    element_source()
                ));
                self.update_hover();
                self.dirty = true;
            }
            Key::Character(c) if c == "`" || c == "~" => {
                self.show_cursor = !self.show_cursor;
                self.window.set_cursor_visible(!self.show_cursor);
                self.show_toast(tr(if self.show_cursor {
                    "Cursor shown"
                } else {
                    "Cursor hidden"
                }));
                self.dirty = true;
            }
            Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace)
                if self.annot.tool == Some(Tool::Select) =>
            {
                self.annot.delete_selected();
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("v") && self.sel.has_sel() => {
                self.annot.set_tool(Some(Tool::Select));
                self.relayout_toolbar();
                self.dirty = true;
            }
            Key::Character(c) if (c == "n" || c == "N") && self.sel.has_sel() && !ctrl => {
                self.annot.set_tool(Some(Tool::Number));
                self.relayout_toolbar();
                self.dirty = true;
            }
            Key::Character(c) if (c == "1" || c == "[") && self.annot.tool.is_some() => {
                self.annot.adjust_width(-1);
                self.dirty = true;
            }
            Key::Character(c) if (c == "2" || c == "]") && self.annot.tool.is_some() => {
                self.annot.adjust_width(1);
                self.dirty = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("a") && !ctrl => {
                self.move_cursor(-1.0, 0.0)
            }
            Key::Character(c) if c.eq_ignore_ascii_case("d") && !ctrl => self.move_cursor(1.0, 0.0),
            Key::Character(c) if c.eq_ignore_ascii_case("w") && !ctrl => {
                self.move_cursor(0.0, -1.0)
            }
            Key::Character(c) if c.eq_ignore_ascii_case("s") && !ctrl => self.move_cursor(0.0, 1.0),
            Key::Named(NamedKey::ArrowLeft) => self.nudge_key(-1.0, 0.0),
            Key::Named(NamedKey::ArrowRight) => self.nudge_key(1.0, 0.0),
            Key::Named(NamedKey::ArrowUp) => self.nudge_key(0.0, -1.0),
            Key::Named(NamedKey::ArrowDown) => self.nudge_key(0.0, 1.0),
            Key::Character(c) if c == "," => self.step_history(-1),
            Key::Character(c) if c == "." => self.step_history(1),
            _ => {}
        }
    }

    fn move_cursor(&mut self, dx: f32, dy: f32) {
        let b = self.bounds();
        self.cursor = Point::new(
            pinshot_core::geom::clamp_f32(self.cursor.x + dx, 0.0, (b.w - 1.0).max(0.0)),
            pinshot_core::geom::clamp_f32(self.cursor.y + dy, 0.0, (b.h - 1.0).max(0.0)),
        );
        let _ = self.window.set_cursor_position(PhysicalPosition::new(
            self.cursor.x as f64,
            self.cursor.y as f64,
        ));
        if self.sel.state == SelState::Creating {
            self.sel.motion(self.cursor, b);
        } else {
            self.update_hover();
        }
        self.dirty = true;
    }

    fn nudge_key(&mut self, dx: f32, dy: f32) {
        if !self.sel.has_sel() {
            self.move_cursor(dx, dy);
            return;
        }
        let mode = if self.mods.control_key() {
            Nudge::Grow
        } else if self.mods.shift_key() {
            Nudge::Shrink
        } else {
            Nudge::Move
        };
        let before = self.sel.normalized();
        self.sel.nudge(dx, dy, mode, self.bounds());
        if mode == Nudge::Move {
            let after = self.sel.normalized();
            let adx = (after.x - before.x) * self.dpr;
            let ady = (after.y - before.y) * self.dpr;
            if adx != 0.0 || ady != 0.0 {
                crate::logutil::log_at("trace", &format!("sel move {adx:.0},{ady:.0}"));
            }
        }
        self.relayout_toolbar();
        self.dirty = true;
    }

    fn copy_color(&mut self) {
        let (c, _) = pinshot_core::sample_color(&self.capture.composed, self.cursor, self.dpr);
        let txt = c.format(self.hex);
        let _ = crate::clipboard::write_text(&txt);
        self.show_toast(tr(&format!("Copied {txt}")));
        self.dirty = true;
        // Python exits shortly after colour copy.
        self.cancel();
        let _ = Rgba::BLACK;
    }

    pub fn on_double_click(&mut self) {
        if self.sel.has_sel() {
            let hit = self.annot.hit_test(self.img_pt(self.cursor));
            if let Some(i) = hit {
                if self.annot.shapes.get(i).map(|s| s.kind) == Some(Tool::Text) {
                    self.annot.begin_edit_text(i);
                    self.sync_ime();
                    self.dirty = true;
                    return;
                }
            }
        }
        if self.annot.tool.is_some() {
            return;
        }
        if self.sel.has_sel() && self.sel.normalized().contains(self.cursor) {
            self.finish_copy();
        }
    }

    pub fn redraw(&mut self) {
        self.expire_toast();
        if !self.dirty {
            // SetWindowPos / DWM / RDP may ask for a paint without a new
            // mouse sample. Replay the last DIB so the freeze is not erased
            // down to black behind the loupe.
            #[cfg(windows)]
            self.front.present_full(hwnd_of(&self.window));
            return;
        }
        let over_tb = self
            .toolbar
            .as_ref()
            .map(|t| t.contains(self.cursor))
            .unwrap_or(false);
        let mag = magnifier_visible(
            self.mag_forced,
            self.annot.tool.is_some(),
            false,
            over_tb,
            matches!(self.sel.state, SelState::Creating | SelState::Resizing),
            self.sel.state == SelState::Idle && !self.sel.has_sel(),
        );
        let t_paint = Instant::now();
        self.front.paint(
            hwnd_of(&self.window),
            &self.sel,
            self.hover,
            self.cursor,
            self.dpr,
            self.hex,
            mag,
            &self.annot,
            self.toolbar.as_ref(),
            self.toast.as_deref(),
            self.show_cursor,
            &self.capture.composed,
            self.confirm.as_ref().map(|c| c.dont),
            self.color_picker.as_ref(),
        );
        crate::logutil::ms("overlay redraw paint", t_paint);
        #[cfg(not(windows))]
        if let Some(surface) = &mut self.surface {
            let pm = self.front.pixmap();
            if let (Some(nw), Some(nh)) = (
                std::num::NonZeroU32::new(pm.width()),
                std::num::NonZeroU32::new(pm.height()),
            ) {
                if surface.resize(nw, nh).is_ok() {
                    if let Ok(mut buf) = surface.buffer_mut() {
                        present(&mut buf, pm);
                        let _ = buf.present();
                    }
                }
            }
        }
        // Uncloak before ShowWindow so DWM / RDP actually composite this frame.
        // Hit-test earlier in start() stays cloaked so UIA sees the window under
        // the cursor, not this HWND.
        let t_window = Instant::now();
        #[cfg(not(windows))]
        self.window.set_visible(true);
        // First uncloak only: restack above the (now cloaked) tray. Doing
        // this on every mouse move invalidates the client and stains the
        // freeze under the loupe.
        #[cfg(windows)]
        if self.need_stack {
            // winit may recalculate native frame styles when showing a
            // window. Keep it cloaked until that transition has finished.
            self.window.set_visible(true);
            native::apply_overlay_style(&self.window);
            native::focus_overlay_window(&self.window);
            native::cloak_shell_trays(true);
            self.cover_desktop();
            self.front.present_full(hwnd_of(&self.window));
            native::cloak_window(&self.window, false);
            self.need_stack = false;
        }
        crate::logutil::ms("overlay redraw window", t_window);
        self.dirty = false;
    }

    fn cover_desktop(&self) {
        #[cfg(windows)]
        {
            let (vx, vy) = self.capture.virtual_origin;
            native::cover_shell(
                &self.window,
                vx,
                vy,
                self.capture.composed.width,
                self.capture.composed.height,
            );
        }
    }

    pub fn pump_redraw(&mut self) {
        self.poll_elements();
        #[cfg(unix)]
        self.ensure_overlay_grab();
        self.poll_overlay_pointer();
        self.recover_pointer_drag();
        self.expire_toast();
        if self.annot.text_edit.is_some() {
            self.dirty = true;
        }
        if self
            .toolbar
            .as_ref()
            .is_some_and(|t| t.contains(self.cursor))
        {
            // Keep tooltips live even when winit skipped CursorMoved (XGrab).
            self.dirty = true;
        }
        if !self.hover_deep
            && self.sel.state == SelState::Idle
            && !self.sel.has_sel()
            && self.moved_at.elapsed() >= std::time::Duration::from_millis(48)
        {
            let prev = self.hover;
            self.hit_px = None;
            self.update_hover();
            if self.hover != prev {
                self.dirty = true;
            }
        }
        if self.dirty {
            self.window.request_redraw();
        }
    }

    /// QueryPointer is never the press origin. The overlay grab delivers
    /// ButtonPress / Motion; recover only finishes a live drag if Release
    /// was dropped (two consecutive ups).
    fn recover_pointer_drag(&mut self) {
        if self.color_picker.is_some() || self.confirm.is_some() {
            return;
        }
        let Some((_, _, down)) = native::pointer_in_window(&self.window) else {
            return;
        };
        if down {
            self.btn1_up_streak = 0;
            return;
        }
        self.btn1_up_streak = self.btn1_up_streak.saturating_add(1);
        let drawing = self.annot.live.is_some()
            || self.annot.is_selecting()
            || self.annot.is_dragging_text()
            || self.annot.is_brush_live();
        if drawing
            && recover_stroke_action(false, drawing, annot_stroke_extent(&self.annot))
                == RecoverStroke::Release
        {
            self.commit_annot_release(self.cursor);
            self.dirty = true;
        } else if recover_should_commit_sel(false, self.btn1_up_streak, self.sel.state) {
            self.finish_sel_drag(self.cursor);
            if self.sel.has_sel() {
                let r = self.sel.normalized();
                crate::logutil::log(&format!("recover release sel {:.0}x{:.0}", r.w, r.h));
            }
            self.dirty = true;
        }
    }

    pub fn next_wake(&self) -> Option<Instant> {
        let mut wake = self.toast_until;
        if !self.hover_deep && self.sel.state == SelState::Idle && !self.sel.has_sel() {
            let refine = self.moved_at + Duration::from_millis(48);
            wake = Some(wake.map(|w| w.min(refine)).unwrap_or(refine));
        }
        #[cfg(unix)]
        {
            let poll = Instant::now() + Duration::from_millis(8);
            wake = Some(wake.map(|w| w.min(poll)).unwrap_or(poll));
        }
        wake
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoverStroke {
    Idle,
    Move,
    Release,
}

/// Create uses the live ButtonPress / MouseInput cursor. A 2-point handle
/// grab still uses the last idle pixel if the press skipped the handle;
/// a press that itself hits a handle wins over a stale idle corner.
fn handle_press_point(idle: Option<Point>, live: Point, has_sel: bool, sel: Rect) -> Point {
    if has_sel {
        if hit_sel_handle(sel, live).is_some() {
            return live;
        }
        if let Some(p) = idle {
            if hit_sel_handle(sel, p).is_some() {
                return p;
            }
        }
    }
    live
}

#[cfg(unix)]
fn xbutton(detail: u8) -> Option<MouseButton> {
    match detail {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        _ => None,
    }
}

/// Two consecutive QueryPointer ups commit Creating (window-adopt if the
fn load_hist_raster(path: &std::path::Path) -> Option<Raster> {
    let img = image::open(path).ok()?.to_rgba8();
    Raster::from_rgba(img.width(), img.height(), img.into_raw())
}

/// Preselect / selection hole is a dim viewport over the freeze. Ink is
/// stored in image space and must not be translated when the hole moves.
#[cfg(test)]
fn ink_follows_snip_hole() -> bool {
    false
}

/// span is a click), Moving, and Resizing. A single false up is ignored.
fn recover_should_commit_sel(down: bool, up_streak: u8, state: SelState) -> bool {
    if down || up_streak < 2 {
        return false;
    }
    matches!(
        state,
        SelState::Creating | SelState::Moving | SelState::Resizing
    )
}

/// Recover continues a live stroke; it never starts one (see `recover_pointer_drag`).
fn recover_stroke_action(down: bool, drawing: bool, live_has_extent: bool) -> RecoverStroke {
    if !drawing {
        return RecoverStroke::Idle;
    }
    if down {
        RecoverStroke::Move
    } else if live_has_extent {
        RecoverStroke::Release
    } else {
        RecoverStroke::Idle
    }
}

fn annot_stroke_extent(annot: &Annotator) -> bool {
    if annot.is_selecting() || annot.is_dragging_text() {
        return true;
    }
    let Some(sh) = annot.live.as_ref() else {
        return false;
    };
    if sh.kind.is_brush(&sh.style) {
        return sh.pts.len() > 1;
    }
    sh.pts.len() > 1 && sh.pts[0].manhattan(*sh.pts.last().unwrap()) > 2.0
}

fn x11_mods_to_winit(mods: u16) -> ModifiersState {
    let mut m = ModifiersState::empty();
    if mods & 1 != 0 {
        m |= ModifiersState::SHIFT;
    }
    if mods & 4 != 0 {
        m |= ModifiersState::CONTROL;
    }
    if mods & 8 != 0 {
        m |= ModifiersState::ALT;
    }
    if mods & 64 != 0 {
        m |= ModifiersState::SUPER;
    }
    m
}

fn keysym_to_winit(keysym: u32) -> Option<(Key, Option<String>)> {
    let named = match keysym {
        0xff1b => Some(NamedKey::Escape),
        0xff0d | 0xff8d => Some(NamedKey::Enter),
        0xff08 => Some(NamedKey::Backspace),
        0xffff | 0xff9f => Some(NamedKey::Delete),
        0xff09 => Some(NamedKey::Tab),
        0xffe1 | 0xffe2 => Some(NamedKey::Shift),
        0xffe3 | 0xffe4 => Some(NamedKey::Control),
        0xffe9 | 0xffea => Some(NamedKey::Alt),
        0xffeb | 0xffec => Some(NamedKey::Super),
        0xff51 | 0xff96 => Some(NamedKey::ArrowLeft),
        0xff52 | 0xff97 => Some(NamedKey::ArrowUp),
        0xff53 | 0xff98 => Some(NamedKey::ArrowRight),
        0xff54 | 0xff99 => Some(NamedKey::ArrowDown),
        0xffbe => Some(NamedKey::F1),
        0xffbf => Some(NamedKey::F2),
        0xffc0 => Some(NamedKey::F3),
        0xffc1 => Some(NamedKey::F4),
        0xffc2 => Some(NamedKey::F5),
        0xffc3 => Some(NamedKey::F6),
        0x0020 => {
            return Some((Key::Character(" ".into()), Some(" ".into())));
        }
        _ => None,
    };
    if let Some(n) = named {
        return Some((Key::Named(n), None));
    }
    // X11 function-key range (0xff00–0xffff): never treat as Unicode text.
    // Control_L is 0xffe3; char::from_u32 maps that to '￣' and used to leak into text.
    if keysym >= 0xff00 && keysym <= 0xffff {
        return None;
    }
    // Unicode keysym: 0x01000000 | codepoint (xdotool CJK, X11 UTF-8).
    let cp = if keysym & 0xff000000 == 0x01000000 {
        keysym & 0x00ffffff
    } else {
        keysym
    };
    let ch = char::from_u32(cp)?;
    if ch.is_control() {
        return None;
    }
    let s = ch.to_string();
    Some((Key::Character(s.clone().into()), Some(s)))
}

impl Drop for Overlay {
    fn drop(&mut self) {
        native::cloak_shell_trays(false);
    }
}

#[cfg(test)]
mod keysym_tests {
    use super::keysym_to_winit;
    use super::{
        annot_stroke_extent, handle_press_point, overlay_hwnd_fits, recover_should_commit_sel,
        recover_stroke_action, RecoverStroke,
    };
    use pinshot_core::annotation::{Annotator, Tool};
    use pinshot_core::geom::Point;
    use pinshot_core::selection::SelState;
    use winit::keyboard::{Key, NamedKey};

    #[test]
    fn control_l_is_modifier_not_macron() {
        match keysym_to_winit(0xffe3) {
            Some((Key::Named(NamedKey::Control), None)) => {}
            other => panic!("Control_L became {other:?}"),
        }
        assert!(keysym_to_winit(0xffe5).is_none()); // Caps_Lock
    }

    #[test]
    fn latin_letters_keep_text() {
        let (k, t) = keysym_to_winit(b'h' as u32).unwrap();
        assert!(matches!(k, Key::Character(c) if c.as_str() == "h"));
        assert_eq!(t.as_deref(), Some("h"));
    }

    #[test]
    fn unicode_keysym_inserts_cjk() {
        let (k, t) = keysym_to_winit(0x01004f60).unwrap();
        assert!(matches!(k, Key::Character(c) if c.as_str() == "你"));
        assert_eq!(t.as_deref(), Some("你"));
    }

    #[test]
    fn recover_never_starts_a_press() {
        assert_eq!(
            recover_stroke_action(true, false, false),
            RecoverStroke::Idle
        );
        assert_eq!(
            recover_stroke_action(true, true, false),
            RecoverStroke::Move
        );
        assert_eq!(
            recover_stroke_action(false, true, false),
            RecoverStroke::Idle
        );
        assert_eq!(
            recover_stroke_action(false, true, true),
            RecoverStroke::Release
        );
    }

    #[test]
    fn same_point_live_has_no_extent() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(10.0, 10.0), false);
        assert!(!annot_stroke_extent(&a));
        a.move_to(Point::new(40.0, 30.0), false);
        assert!(annot_stroke_extent(&a));
    }

    #[test]
    fn create_uses_live_press_not_stale_idle() {
        let stale = Point::new(40.0, 80.0);
        let press = Point::new(400.0, 300.0);
        assert_eq!(
            handle_press_point(Some(stale), press, false, pinshot_core::geom::Rect::EMPTY),
            press
        );
    }

    #[test]
    fn two_point_handle_uses_idle_pixel_not_drop() {
        let handle = Point::new(300.0, 200.0);
        let drop = Point::new(380.0, 250.0);
        let sel = pinshot_core::geom::Rect::new(100.0, 80.0, 200.0, 120.0);
        assert_eq!(handle_press_point(Some(handle), drop, true, sel), handle);
        assert_eq!(handle_press_point(Some(drop), drop, true, sel), drop);
    }

    #[test]
    fn live_handle_beats_stale_idle_handle() {
        let br = Point::new(300.0, 200.0);
        let tl = Point::new(100.0, 80.0);
        let sel = pinshot_core::geom::Rect::new(100.0, 80.0, 200.0, 120.0);
        assert_eq!(handle_press_point(Some(br), tl, true, sel), tl);
    }

    #[test]
    fn recover_does_not_commit_click_or_single_up() {
        assert!(!recover_should_commit_sel(false, 1, SelState::Creating));
        assert!(recover_should_commit_sel(false, 2, SelState::Creating));
        assert!(recover_should_commit_sel(false, 2, SelState::Resizing));
        assert!(!recover_should_commit_sel(true, 2, SelState::Creating));
        assert!(!recover_should_commit_sel(false, 2, SelState::Selected));
    }

    #[test]
    fn overlay_hwnd_fits_full_desktop_only() {
        assert!(overlay_hwnd_fits(4160, 1400, 4160, 1400));
        assert!(!overlay_hwnd_fits(1, 1, 4160, 1400));
        assert!(!overlay_hwnd_fits(4160, 1400, 1920, 1080));
        assert!(!overlay_hwnd_fits(0, 0, 0, 0));
    }

    #[test]
    fn focus_and_held_f1_do_not_restart_capture() {
        let f1 = Key::Named(NamedKey::F1);
        assert!(!super::overlay_key_is_actionable(&f1, true, false));
        assert!(!super::overlay_key_is_actionable(&f1, false, true));
        assert!(super::overlay_key_is_actionable(&f1, false, false));
        let backspace = Key::Named(NamedKey::Backspace);
        assert!(super::overlay_key_is_actionable(&backspace, false, true));
        assert!(!super::overlay_key_is_actionable(&backspace, true, false));
    }

    #[test]
    fn snip_hole_is_viewport_ink_stays_on_freeze() {
        assert!(
            !super::ink_follows_snip_hole(),
            "preselect / sel hole must not drag annotations"
        );
    }

    #[test]
    fn cancel_and_resnip_do_not_rewrite_history() {
        use super::OverlayResult;
        use pinshot_core::geom::Rect;
        use pinshot_core::raster::Raster;
        let img = Raster::new(4, 4);
        let r = Rect::new(0.0, 0.0, 4.0, 4.0);
        assert!(!OverlayResult::Cancel.writes_history());
        assert!(!OverlayResult::Resnip.writes_history());
        assert!(OverlayResult::Copy(img.clone(), r).writes_history());
        assert!(OverlayResult::Pin(img.clone(), r, (0, 0)).writes_history());
        assert!(OverlayResult::Save(img.clone()).writes_history());
        assert!(OverlayResult::Ocr(img, false, (0, 0)).writes_history());
    }
}
