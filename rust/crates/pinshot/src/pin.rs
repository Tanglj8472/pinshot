//! Frameless always-on-top pin windows.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::draw::{
    fill_rect, paint_color_picker, paint_toolbar, present_argb, stroke_rect, PIN_SHADOW_M,
};
use crate::native;
use crate::toolbar::{TbAction, Toolbar};
use pinshot_core::annotation::{Annotator, ArrowKind, Style, Tool};
use pinshot_core::color::Rgba;
use pinshot_core::colorpicker::{color_picker_layout, ColorPicker, PickerAction};
use pinshot_core::cursor::cursor_for;
use pinshot_core::geom::{Point, Rect};
use pinshot_core::groups;
use pinshot_core::i18n::tr;
use pinshot_core::ocr::{hit_token, nearest_token, selected_text, tokens_in_rect, OcrPage};
use pinshot_core::raster::Raster;
use pinshot_core::render::compose;
use pinshot_core::wheel::WheelGate;
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

static LAST_TOOL_STYLES: std::sync::OnceLock<std::sync::Mutex<Option<[Style; 9]>>> =
    std::sync::OnceLock::new();

fn remembered_styles() -> Option<[Style; 9]> {
    LAST_TOOL_STYLES
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .ok()
        .and_then(|g| g.clone())
}

fn remember_styles(styles: &[Style; 9]) {
    if let Ok(mut g) = LAST_TOOL_STYLES
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
    {
        *g = Some(styles.clone());
    }
}

#[cfg(windows)]
use crate::win32::{hwnd_of, wide};
#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenu, HMENU, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
};

const ID_COPY: usize = 1;
const ID_SAVE: usize = 2;
const ID_CLOSE: usize = 3;
const ID_CT: usize = 4;
const ID_Z25: usize = 10;
const ID_Z50: usize = 11;
const ID_Z75: usize = 12;
const ID_Z100: usize = 13;
const ID_Z150: usize = 14;
const ID_Z200: usize = 15;
const ID_Z400: usize = 16;
const ID_TOOLBAR: usize = 5;
const OCR_STRIP_MIN_H: u32 = 40;
const OCR_STRIP_LINE: f32 = 18.0;
const OCR_STRIP_PAD: f32 = 8.0;
const OCR_STRIP_MAX_H: u32 = 160;

struct OcrOverlay {
    page: OcrPage,
    sel: Option<(usize, usize)>,
    translation: Option<String>,
    translating: bool,
}

/// Drag-select on the pin image. Empty-space drags still move the pin.
#[derive(Clone, Copy)]
struct OcrGesture {
    img0: Point,
    win0: Point,
    tl0: (i32, i32),
    selecting: bool,
}

pub struct PinWin {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    pub img: Raster,
    pub annot: Annotator,
    pub scale: f32,
    pub opacity: f32,
    pub click_through: bool,
    glow: f32,
    glow_at: Option<Instant>,
    /// Grab offset from the window origin to the screen cursor while dragging.
    drag: Option<(i32, i32)>,
    last_cursor: Point,
    /// Last applied outer origin. winit `outer_position` stays stale on xfwm4
    /// after `request_inner_size`, so consecutive wheel notches must not reread it.
    geom_pos: Option<(i32, i32)>,
    pub closed: bool,
    dirty: bool,
    pub group: String,
    pub annotating: bool,
    pub automation_mode: bool,
    toolbar: Option<Toolbar>,
    arrow_kind: ArrowKind,
    ellipse_on: bool,
    line_menu_open: bool,
    font_menu_open: bool,
    font_menu_offset: usize,
    toast: Option<String>,
    toast_until: Option<Instant>,
    pub persist_needed: bool,
    pub destroy: bool,
    pub pending_menu: bool,
    pub saved_path: Option<String>,
    wheel: WheelGate,
    pub shadow: bool,
    pub can_shadow: bool,
    pub thumb: bool,
    color_picker: Option<ColorPicker>,
    mods: ModifiersState,
    pub smooth: bool,
    pub grayscale: bool,
    pub invert: bool,
    pub rotation: i32,
    pub mirror_x: bool,
    pub mirror_y: bool,
    ocr: Option<OcrOverlay>,
    ocr_gesture: Option<OcrGesture>,
    pending_ocr: bool,
    ocr_running: bool,
    pending_translate: bool,
    /// Sticky banner (download / recognize). Unlike toast it does not expire.
    status: Option<String>,
    ocr_flash_until: Option<Instant>,
}

impl PinWin {
    pub fn create(
        event_loop: &ActiveEventLoop,
        img: Raster,
        pos: (i32, i32),
    ) -> Result<Self, String> {
        Self::create_at_scale(event_loop, img, pos, 1.0, Vec::new())
    }

    pub fn create_at_scale(
        event_loop: &ActiveEventLoop,
        img: Raster,
        pos: (i32, i32),
        scale: f32,
        shapes: Vec<pinshot_core::annotation::Shape>,
    ) -> Result<Self, String> {
        let scale = scale.clamp(0.05, 1.0);
        let w = ((img.width as f32 * scale).round() as u32).max(8);
        let h = ((img.height as f32 * scale).round() as u32).max(8);
        let can_shadow = native::compositor_active();
        // Python sizes the widget to image + 2M and places the photo at (M,M).
        // Creating at bare image size cropped the shadow pixmap to its top-left
        // — a dark band along the top and left edges.
        let m = if can_shadow { PIN_SHADOW_M as i32 } else { 0 };
        let attrs = native::as_tool_window(
            Window::default_attributes()
                .with_title("PinShot Pin")
                .with_decorations(false)
                .with_resizable(false)
                .with_visible(false)
                .with_transparent(can_shadow)
                .with_position(PhysicalPosition::new(pos.0 - m, pos.1 - m))
                .with_inner_size(PhysicalSize::new(
                    (w as i32 + 2 * m) as u32,
                    (h as i32 + 2 * m) as u32,
                ))
                .with_window_level(winit::window::WindowLevel::AlwaysOnTop),
        );
        let window = Rc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        native::apply_pin_style(&window);
        // Paint while cloaked so DWM never shows an empty layered HWND.
        #[cfg(windows)]
        {
            native::cloak_window(&window, true);
            native::show_noactivate(&window);
        }
        window.set_ime_allowed(true);
        let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let mut annot = Annotator::new();
        annot.shapes = shapes;
        if let Some(styles) = remembered_styles() {
            annot.styles = styles;
        }
        let mut pin = Self {
            window,
            surface,
            img,
            annot,
            scale,
            opacity: 1.0,
            click_through: false,
            glow: 1.0,
            glow_at: Some(Instant::now()),
            drag: None,
            last_cursor: Point::ZERO,
            geom_pos: Some((pos.0 - m, pos.1 - m)),
            closed: false,
            dirty: true,
            group: groups::DEFAULT.into(),
            annotating: false,
            automation_mode: false,
            toolbar: None,
            arrow_kind: ArrowKind::Thin,
            ellipse_on: false,
            line_menu_open: false,
            font_menu_open: false,
            font_menu_offset: 0,
            toast: None,
            toast_until: None,
            persist_needed: false,
            destroy: false,
            pending_menu: false,
            saved_path: None,
            wheel: WheelGate::new(),
            can_shadow,
            shadow: can_shadow,
            thumb: false,
            color_picker: None,
            mods: ModifiersState::empty(),
            smooth: true,
            grayscale: false,
            invert: false,
            rotation: 0,
            mirror_x: false,
            mirror_y: false,
            ocr: None,
            ocr_gesture: None,
            pending_ocr: false,
            ocr_running: false,
            pending_translate: false,
            status: None,
            ocr_flash_until: None,
        };
        let t_paint = Instant::now();
        pin.redraw();
        crate::logutil::ms("pin first paint", t_paint);
        #[cfg(windows)]
        native::cloak_window(&pin.window, false);
        pin.show();
        crate::logutil::log(&format!(
            "pin create {}x{} at {},{} margin={m} transparent={can_shadow}",
            w as i32 + 2 * m,
            h as i32 + 2 * m,
            pos.0 - m,
            pos.1 - m
        ));
        Ok(pin)
    }

    /// Replace an AI session's rendered preview without modifying its geometry.
    pub fn update_automation_image(&mut self, image: Raster) {
        let resized = self.img.width != image.width || self.img.height != image.height;
        self.img = image;
        self.annot.clear();
        self.clear_ocr();
        self.show_toolbar(false);
        if resized { self.apply_size(); }
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn automation_refresh(&mut self) {
        if self.annotating { self.relayout_tb(); }
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    #[cfg(windows)]
    pub fn hwnd(&self) -> HWND {
        hwnd_of(&self.window)
    }

    pub fn last_cursor_i32(&self) -> (i32, i32) {
        (
            self.last_cursor.x.round() as i32,
            self.last_cursor.y.round() as i32,
        )
    }

    pub fn pos(&self) -> (i32, i32) {
        self.current_tl()
    }

    /// Image top-left in screen pixels (Python `PinWindow` constructor `pos`).
    /// The window itself sits at this origin minus the shadow margin.
    pub fn img_origin(&self) -> (i32, i32) {
        let m = self.margin();
        let tl = self.current_tl();
        (tl.0 + m, tl.1 + m)
    }

    fn current_tl(&self) -> (i32, i32) {
        self.geom_pos
            .or_else(|| self.window.outer_position().ok().map(|p| (p.x, p.y)))
            .unwrap_or((40, 40))
    }

    fn screen_pointer(&self) -> (i32, i32) {
        if let Some(p) = native::cursor_screen() {
            return p;
        }
        let tl = self.current_tl();
        (
            tl.0 + self.last_cursor.x.round() as i32,
            tl.1 + self.last_cursor.y.round() as i32,
        )
    }

    fn begin_drag(&mut self) {
        self.dirty = false;
        let tl = self.current_tl();
        let (sx, sy) = self.screen_pointer();
        self.drag = Some(pin_grab_offset(tl, (sx, sy)));
    }

    fn drag_to_cursor(&mut self) {
        let Some(grab) = self.drag else {
            return;
        };
        let (nx, ny) = pin_origin_from_cursor(self.screen_pointer(), grab);
        if self.geom_pos == Some((nx, ny)) {
            return;
        }
        native::move_window(&self.window, nx, ny);
        self.geom_pos = Some((nx, ny));
    }

    fn apply_geom(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.geom_pos = Some((x, y));
        // Size through winit, then move on the window's own Xlib connection.
        // x11rb ConfigureRequest from a second client is ignored by xfwm4, so
        // `configure_window_geom` alone left the pin at its old origin.
        let _ = self.window.request_inner_size(PhysicalSize::new(w, h));
        self.window.set_outer_position(PhysicalPosition::new(x, y));
        let _ = native::configure_window_geom(&self.window, x, y, w, h);
        native::set_opacity(&self.window, self.opacity);
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn show(&self) {
        self.window.set_visible(true);
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible().unwrap_or(true)
    }

    pub fn set_scale(&mut self, s: f32) {
        self.set_scale_at(s, None);
    }

    /// Zoom around `anchor` (window coords) so the pointer stays on the pin.
    pub fn set_scale_at(&mut self, s: f32, anchor: Option<Point>) {
        let old = self.scale.max(0.01);
        let new = pinshot_core::geom::clamp_f32(s, 0.05, 8.0);
        if (new - old).abs() < f32::EPSILON {
            return;
        }
        let tl = self.current_tl();
        self.scale = new;
        let (w, h) = self.win_px();
        let m = self.margin() as f32;
        let (nx, ny) = match anchor {
            Some(a) => zoom_pos(tl, a, old, new, m),
            None => tl,
        };
        self.apply_geom(nx, ny, w, h);
        crate::logutil::log_at("trace", &format!(
            "pin zoom {old:.3}->{new:.3} {}x{} @ {},{} (was {},{}) anchor {:.0},{:.0}",
            w,
            h,
            nx,
            ny,
            tl.0,
            tl.1,
            anchor.map(|a| a.x).unwrap_or(0.0),
            anchor.map(|a| a.y).unwrap_or(0.0)
        ));
        self.persist_needed = true;
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn set_opacity(&mut self, o: f32) {
        self.set_opacity_ex(o, true);
    }

    pub fn set_opacity_ex(&mut self, o: f32, toast: bool) {
        self.opacity = pinshot_core::geom::clamp_f32(o, 0.1, 1.0);
        native::set_opacity(&self.window, self.opacity);
        if toast {
            self.show_toast(tr(&format!(
                "Opacity: {}%",
                (self.opacity * 100.0).round() as i32
            )));
        }
        self.persist_needed = true;
        self.dirty = true;
    }

    fn show_toast(&mut self, text: String) {
        self.toast = Some(text);
        self.toast_until = Some(Instant::now() + Duration::from_millis(1200));
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn toast(&mut self, text: String) {
        self.show_toast(text);
    }

    pub fn attach_ocr(&mut self, page: OcrPage) {
        self.ocr = Some(OcrOverlay {
            page,
            sel: None,
            translation: None,
            translating: false,
        });
        self.ocr_gesture = None;
        self.status = None;
        self.ocr_flash_until = Some(Instant::now() + Duration::from_millis(900));
        self.prefer_select_for_ocr();
        self.show_toast(tr("OCR ready — drag to select text"));
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn set_status(&mut self, text: Option<String>) {
        self.status = text;
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn ocr_text(&self) -> Option<String> {
        self.ocr.as_ref().map(|o| o.page.text.clone())
    }

    pub fn request_ocr(&mut self) {
        if self.start_ocr() {
            self.pending_ocr = true;
        }
    }

    pub fn start_ocr(&mut self) -> bool {
        if self.ocr_running {
            return false;
        }
        self.ocr_running = true;
        true
    }

    pub fn ocr_running(&self) -> bool {
        self.ocr_running
    }

    pub fn finish_ocr(&mut self) {
        self.ocr_running = false;
    }

    pub fn request_translate(&mut self) {
        self.pending_translate = true;
    }

    pub fn set_translating(&mut self, on: bool) {
        if let Some(ocr) = self.ocr.as_mut() {
            ocr.translating = on;
            if on {
                ocr.translation = None;
            }
        }
        self.apply_size();
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn set_translation(&mut self, text: String) {
        if let Some(ocr) = self.ocr.as_mut() {
            ocr.translating = false;
            ocr.translation = Some(text);
        }
        self.apply_size();
        self.dirty = true;
        self.window.request_redraw();
    }

    pub fn take_pending_ocr(&mut self) -> bool {
        let v = self.pending_ocr;
        self.pending_ocr = false;
        v
    }

    pub fn take_pending_translate(&mut self) -> bool {
        let v = self.pending_translate;
        self.pending_translate = false;
        v
    }

    fn clear_ocr(&mut self) {
        self.ocr = None;
        self.ocr_gesture = None;
    }

    fn ocr_select_to(&mut self, img_pt: Point) {
        let Some(g) = self.ocr_gesture.as_mut() else {
            return;
        };
        let Some(ocr) = self.ocr.as_mut() else {
            return;
        };
        // Reading-order caret range (RapidOCR / 参考工具 style), not a
        // geometric band: a tall drag used to copy every token between the
        // first and last box that touched the rectangle.
        let start =
            hit_token(&ocr.page.tokens, g.img0).or_else(|| nearest_token(&ocr.page.tokens, g.img0));
        let end =
            hit_token(&ocr.page.tokens, img_pt).or_else(|| nearest_token(&ocr.page.tokens, img_pt));
        if let (Some(start), Some(end)) = (start, end) {
            ocr.sel = Some((start, end));
            g.selecting = true;
            self.dirty = true;
        }
    }

    fn copy_hotkey(&mut self) {
        if self.ocr_over_strip(self.last_cursor) {
            if let Some(t) = self.ocr.as_ref().and_then(|o| o.translation.clone()) {
                let _ = crate::clipboard::write_text(&t);
                self.show_toast(tr("Copied to clipboard"));
                return;
            }
        }
        if let Some(ocr) = &self.ocr {
            if let Some((a, b)) = ocr.sel {
                let s = selected_text(&ocr.page.tokens, a, b);
                if !s.is_empty() {
                    let _ = crate::clipboard::write_text(&s);
                    self.show_toast(tr("Copied to clipboard"));
                    return;
                }
            }
        }
        let _ = crate::clipboard::write_image(&self.rendered());
    }

    fn expire_toast(&mut self) {
        if let Some(t) = self.toast_until {
            if Instant::now() >= t {
                self.toast = None;
                self.toast_until = None;
                self.dirty = true;
            }
        }
    }

    pub fn set_click_through(&mut self, on: bool) {
        self.click_through = on;
        native::set_click_through(&self.window, on, self.opacity);
        self.dirty = true;
    }

    pub fn restore_shapes(&mut self, shapes: Vec<pinshot_core::annotation::Shape>) {
        self.annot.shapes = shapes;
        self.annot.renumber();
        self.dirty = true;
    }

    pub fn rendered(&self) -> Raster {
        compose(&self.img, &self.annot.shapes)
    }

    pub fn tick_glow(&mut self) {
        if self.glow <= 0.0 {
            return;
        }
        let now = Instant::now();
        if self.glow_at.is_some_and(|t| now < t) {
            return;
        }
        self.glow_at = Some(now + Duration::from_millis(40));
        self.glow = (self.glow - 0.05).max(0.0);
        if self.drag.is_none() {
            self.dirty = true;
        }
    }

    pub fn show_toolbar(&mut self, on: bool) {
        self.annotating = on;
        if on {
            if self.thumb {
                self.toggle_thumb();
            }
            // Showing the toolbar must not put the pin into annotation-select
            // mode. Keep the neutral tool state so the image remains movable;
            // an annotation tool is entered only after the user chooses one.
            self.annot.set_tool(None);
            self.relayout_tb();
        } else {
            self.annot.set_tool(None);
            self.toolbar = None;
        }
        self.apply_size();
        self.dirty = true;
        self.sync_cursor();
    }

    fn prefer_select_for_ocr(&mut self) {
        if self.annotating {
            self.annot.set_tool(Some(Tool::Select));
            self.relayout_tb();
        }
    }

    fn ocr_text_mode(&self) -> bool {
        pin_ocr_text_mode(
            self.ocr.is_some(),
            self.thumb,
            self.annotating,
            self.annot.tool,
        )
    }

    fn relayout_tb(&mut self) {
        let m = self.margin() as f32;
        let (iw, ih) = self.img_px();
        let (ww, hh) = self.win_px();
        let sel = Rect::new(m, m, iw as f32, ih as f32);
        let over = Rect::new(0.0, 0.0, ww as f32, hh as f32);
        let mut tb = Toolbar::layout_pin(
            sel,
            over,
            self.annot.tool,
            self.arrow_kind,
            self.annot.option_style(),
        );
        tb.set_width_value(self.annot.option_width());
        if self.font_menu_open && self.annot.tool == Some(Tool::Text) {
            if let Some(st) = self.annot.option_style() {
                self.font_menu_offset = tb.attach_font_menu(over, st, self.font_menu_offset);
            }
        } else {
            self.font_menu_open = false;
        }
        if self.line_menu_open && self.annot.tool == Some(Tool::Line) {
            if let Some(st) = self.annot.option_style() {
                tb.attach_line_menu(over, st);
            }
        } else {
            self.line_menu_open = false;
        }
        self.toolbar = Some(tb);
    }

    fn margin(&self) -> i32 {
        if self.shadow {
            crate::draw::PIN_SHADOW_M as i32
        } else {
            0
        }
    }

    fn img_px(&self) -> (u32, u32) {
        if self.thumb {
            return (crate::draw::PIN_THUMB, crate::draw::PIN_THUMB);
        }
        let w = ((self.img.width as f32) * self.scale).round().max(8.0) as u32;
        let h = ((self.img.height as f32) * self.scale).round().max(8.0) as u32;
        (w, h)
    }

    fn strip_lines(&self) -> Vec<String> {
        let Some(ocr) = &self.ocr else {
            return Vec::new();
        };
        let msg = if ocr.translating {
            tr("Translating…")
        } else {
            ocr.translation.clone().unwrap_or_default()
        };
        if msg.is_empty() && !ocr.translating {
            return Vec::new();
        }
        let (iw, _) = self.img_px();
        let max_w = (iw as f32 - OCR_STRIP_PAD * 2.0).max(24.0);
        pinshot_core::font::wrap_chrome(&msg, 14.0, max_w)
    }

    fn strip_h(&self) -> u32 {
        match &self.ocr {
            Some(o) if o.translation.is_some() || o.translating => {
                let n = self.strip_lines().len().max(1) as f32;
                let h = (OCR_STRIP_PAD * 2.0 + n * OCR_STRIP_LINE).ceil() as u32;
                h.clamp(OCR_STRIP_MIN_H, OCR_STRIP_MAX_H)
            }
            _ => 0,
        }
    }

    fn win_px(&self) -> (u32, u32) {
        let (w, h) = self.img_px();
        let m = (self.margin() * 2) as u32;
        let (ww, hh) = (w + m, h + m + self.strip_h());
        if self.annotating {
            (ww.max(420), hh + 90)
        } else {
            (ww, hh)
        }
    }

    fn token_win_rect(&self, r: Rect) -> Rect {
        let m = self.margin() as f32;
        Rect::new(
            m + r.x * self.scale,
            m + r.y * self.scale,
            r.w * self.scale,
            r.h * self.scale,
        )
    }

    fn strip_rect(&self) -> Option<Rect> {
        let h = self.strip_h();
        if h == 0 {
            return None;
        }
        let m = self.margin() as f32;
        let (iw, ih) = self.img_px();
        Some(Rect::new(m, m + ih as f32, iw as f32, h as f32))
    }

    fn ocr_over_strip(&self, p: Point) -> bool {
        self.strip_rect().is_some_and(|r| r.contains(p))
    }

    fn begin_ocr_select(&mut self, p: Point) {
        if self.ocr_over_strip(p) {
            return;
        }
        let img = self.img_pt(p);
        let hit = self.ocr_hit(img);
        self.ocr_gesture = Some(OcrGesture {
            img0: img,
            win0: p,
            tl0: self.current_tl(),
            selecting: hit.is_some(),
        });
        if let Some(i) = hit {
            if let Some(ocr) = self.ocr.as_mut() {
                ocr.sel = Some((i, i));
            }
            self.dirty = true;
        }
    }

    fn ocr_hit(&self, img_pt: Point) -> Option<usize> {
        self.ocr
            .as_ref()
            .and_then(|o| hit_token(&o.page.tokens, img_pt))
    }

    fn apply_size(&mut self) {
        let (ww, hh) = self.win_px();
        let (x, y) = self.current_tl();
        self.apply_geom(x, y, ww, hh);
    }

    fn img_pt(&self, p: Point) -> Point {
        let m = self.margin() as f32;
        if self.thumb {
            let s = self.img.width.min(self.img.height) as f32;
            let sx = (self.img.width as f32 - s) / 2.0;
            let sy = (self.img.height as f32 - s) / 2.0;
            let t = crate::draw::PIN_THUMB as f32;
            Point::new(sx + (p.x - m) / t * s, sy + (p.y - m) / t * s)
        } else {
            Point::new(
                (p.x - m) / self.scale.max(0.01),
                (p.y - m) / self.scale.max(0.01),
            )
        }
    }

    pub fn set_shadow(&mut self, on: bool) {
        let m = self.margin();
        let tl = self.current_tl();
        let img_tl = (tl.0 + m, tl.1 + m);
        self.shadow = on && self.can_shadow;
        let nm = self.margin();
        let (ww, hh) = self.win_px();
        self.apply_geom(img_tl.0 - nm, img_tl.1 - nm, ww, hh);
        self.persist_needed = true;
        self.dirty = true;
        crate::logutil::log(&format!("pin shadow {}", self.shadow));
    }

    pub fn toggle_thumb(&mut self) {
        let m = self.margin();
        let tl = self.current_tl();
        let img_tl = (tl.0 + m, tl.1 + m);
        self.thumb = !self.thumb;
        let (ww, hh) = self.win_px();
        self.apply_geom(img_tl.0 - m, img_tl.1 - m, ww, hh);
        self.persist_needed = true;
        self.dirty = true;
        crate::logutil::log(&format!("pin thumb {}", self.thumb));
    }

    fn sync_cursor(&mut self) {
        if self.click_through {
            return;
        }
        let over_chrome = self.color_picker.is_some()
            || self
                .toolbar
                .as_ref()
                .is_some_and(|t| t.contains(self.last_cursor));
        if !self.annotating || self.ocr_text_mode() {
            let m = self.margin() as f32;
            let (iw, ih) = self.img_px();
            let p = self.last_cursor;
            let over_img = p.x >= m && p.y >= m && p.x <= m + iw as f32 && p.y <= m + ih as f32;
            let over_text = (self.ocr_text_mode() && over_img) || self.ocr_over_strip(p);
            if over_text && !over_chrome {
                crate::cursor::apply(
                    &self.window,
                    cursor_for(pinshot_core::CursorHint {
                        editing_text: true,
                        over_chrome,
                        ..Default::default()
                    }),
                );
                return;
            }
            if !self.annotating {
                crate::cursor::apply(
                    &self.window,
                    cursor_for(pinshot_core::CursorHint {
                        pin_idle: !over_chrome,
                        over_chrome,
                        ..Default::default()
                    }),
                );
                return;
            }
        }
        let mut hint = self.annot.cursor_hint(self.img_pt(self.last_cursor));
        hint.over_chrome = over_chrome;
        crate::cursor::apply_annot(&self.window, cursor_for(hint), &self.annot);
    }

    fn apply_tb(&mut self, a: TbAction) {
        if self.automation_mode && matches!(a, TbAction::Ocr) {
            self.toast("OCR automation is not connected yet".into());
            return;
        }
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
                let _ = self.annot.undo();
            }
            TbAction::Redo => {
                let _ = self.annot.redo();
            }
            TbAction::Close => self.close(),
            TbAction::Save => self.save(),
            TbAction::Copy => {
                self.copy_hotkey();
            }
            TbAction::Check => self.show_toolbar(false),
            TbAction::Width(d) => {
                let _ = self.annot.adjust_width(d);
            }
            TbAction::WidthSet(v) => {
                let _ = self.annot.set_option_width(v);
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
            TbAction::CustomColor => {
                if let Some(tool) = self.annot.option_tool().filter(|t| *t != Tool::Select) {
                    let cur = self
                        .annot
                        .option_style()
                        .map(|s| s.color)
                        .unwrap_or_else(|| self.annot.style(tool).color);
                    self.color_picker = Some(ColorPicker::new(cur));
                    crate::logutil::log(&format!("color picker open {}", cur.hex()));
                }
            }
            TbAction::Fill(on) => {
                if let Some(tool) = self.annot.option_tool().filter(|t| *t != Tool::Select) {
                    self.annot.set_style_value(tool, |s| s.filled = on);
                }
            }
            TbAction::LineStyle(ls) => {
                if let Some(tool) = self.annot.option_tool() {
                    self.annot.set_style_value(tool, |s| s.line_style = ls);
                }
            }
            TbAction::SetMode(mode) => {
                if let Some(tool) = self.annot.option_tool() {
                    self.annot.set_draw_mode(tool, mode);
                }
            }
            TbAction::SetEffect(effect) => {
                if let Some(tool) = self.annot.option_tool() {
                    self.annot.set_style_value(tool, |s| s.effect = effect);
                }
            }
            TbAction::Bold(on) => {
                self.annot.set_style_value(Tool::Text, |s| s.bold = on);
            }
            TbAction::Italic(on) => {
                self.annot.set_style_value(Tool::Text, |s| s.italic = on);
            }
            TbAction::Outline(on) => {
                self.annot.set_style_value(Tool::Text, |s| s.outline = on);
            }
            TbAction::Family(fam) => {
                self.font_menu_open = false;
                self.annot
                    .set_style_value(Tool::Text, |s| s.family = fam.to_string());
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
            TbAction::Ocr => {
                let running = self.ocr_running;
                self.request_ocr();
                if !running {
                    self.prefer_select_for_ocr();
                }
            }
            TbAction::Pin => {}
        }
        if self.annotating {
            self.relayout_tb();
        }
        self.persist_needed = true;
        self.dirty = true;
    }

    fn picker_layout(&self) -> pinshot_core::ColorPickerLayout {
        let (ww, hh) = self.win_px();
        color_picker_layout(ww as f32, hh as f32)
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
            PickerAction::Continue => self.dirty = true,
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
        if self.annotating {
            self.relayout_tb();
        }
        self.persist_needed = true;
        self.dirty = true;
    }

    fn close_picker(&mut self) {
        if self.color_picker.take().is_some() {
            crate::logutil::log("color picker cancel");
            self.dirty = true;
        }
    }

    pub fn handle(&mut self, event: &WindowEvent) {
        if self.click_through {
            return;
        }
        if self.color_picker.is_some() {
            match event {
                WindowEvent::CursorMoved { position, .. } => {
                    let p = Point::new(position.x as f32, position.y as f32);
                    self.last_cursor = p;
                    let lay = self.picker_layout();
                    if let Some(picker) = self.color_picker.as_mut() {
                        if picker.drag.is_some() {
                            picker.drag_to(p, &lay);
                            self.dirty = true;
                        }
                    }
                }
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Left,
                    ..
                } => {
                    if *state == ElementState::Pressed {
                        self.picker_press(self.last_cursor);
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
                WindowEvent::CloseRequested => self.close(),
                _ => {}
            }
            self.sync_cursor();
            return;
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let p = Point::new(position.x as f32, position.y as f32);
                let was_over_tb = self
                    .toolbar
                    .as_ref()
                    .map(|t| t.contains(self.last_cursor))
                    .unwrap_or(false);
                self.last_cursor = p;
                if self.annotating
                { self.dirty = true; }
                if self.annotating
                    && (self.annot.is_brush_live()
                        || self.annot.is_selecting()
                        || self.annot.is_dragging_text()
                        || self.annot.live.is_some())
                {
                    self.annot.move_to(self.img_pt(p), self.mods.shift_key());
                    self.dirty = true;
                } else if self.annotating
                    && (was_over_tb || self.toolbar.as_ref().is_some_and(|t| t.contains(p)))
                {
                    self.dirty = true;
                } else if self.ocr_gesture.is_some_and(|g| g.selecting) {
                    self.ocr_select_to(self.img_pt(p));
                } else if let Some(g) = self.ocr_gesture {
                    let img = self.img_pt(p);
                    let band = Rect::from_points(g.img0, img);
                    let hit = self
                        .ocr
                        .as_ref()
                        .and_then(|o| tokens_in_rect(&o.page.tokens, band));
                    if hit.is_some() {
                        self.ocr_select_to(img);
                    } else if (p.x - g.win0.x).hypot(p.y - g.win0.y) > 8.0 {
                        self.ocr_gesture = None;
                        if let Some(ocr) = self.ocr.as_mut() {
                            ocr.sel = None;
                        }
                        self.begin_drag();
                        self.drag_to_cursor();
                    }
                } else if self.drag.is_some() {
                    self.drag_to_cursor();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if *state == ElementState::Pressed && *button != MouseButton::Right {
                    native::focus_window(&self.window);
                    native::raise_on_top(&self.window);
                }
                if *button == MouseButton::Right && *state == ElementState::Pressed {
                    if self.annotating && self.annot.live.as_ref().is_some_and(|s| s.kind == Tool::Line) {
                        self.annot.move_to(self.img_pt(self.last_cursor), self.mods.shift_key());
                        self.persist_needed = true;
                    }
                    if self.annotating && self.annot.right_click() {
                        self.relayout_tb();
                        self.dirty = true;
                        return;
                    }
                    self.popup_menu();
                    return;
                }
                if *button == MouseButton::Middle && *state == ElementState::Pressed {
                    if self.annot.text_edit.is_some() {
                        if let Ok(s) = crate::clipboard::read_text() {
                            self.annot.insert_str(&s);
                            self.dirty = true;
                        }
                        return;
                    }
                }
                if *button != MouseButton::Left {
                    return;
                }
                let p = self.last_cursor;
                if *state == ElementState::Pressed {
                    if self.annotating {
                        if let Some(tb) = &self.toolbar {
                            if let Some(a) = tb.hit(p) {
                                self.apply_tb(a);
                                return;
                            }
                        }
                        if self.line_menu_open || self.font_menu_open {
                            self.line_menu_open = false;
                            self.font_menu_open = false;
                            self.relayout_tb();
                            self.dirty = true;
                            return;
                        }
                        let m = self.margin() as f32;
                        let (iw, ih) = self.img_px();
                        let over_img =
                            p.x >= m && p.y >= m && p.x <= m + iw as f32 && p.y <= m + ih as f32;
                        if over_img && self.ocr_text_mode() {
                            let img = self.img_pt(p);
                            let steal_shape = self.annot.hit_handle_of_selected(img).is_some()
                                || self.annot.hit_test(img).is_some();
                            if steal_shape {
                                if self.annot.press(img, self.mods.shift_key()) {
                                    self.dirty = true;
                                }
                                return;
                            }
                            self.begin_ocr_select(p);
                            return;
                        }
                        if over_img {
                            if self.annot.press(self.img_pt(p), self.mods.shift_key()) {
                                self.dirty = true;
                                return;
                            }
                            // The toolbar's default Select tool only owns the
                            // pointer when an annotation is actually hit. If
                            // the image itself was hit, keep normal pin drag
                            // behavior without requiring another tool click.
                            self.begin_drag();
                            return;
                        }
                    } else if let Some(tb) = &self.toolbar {
                        if let Some(a) = tb.hit(p) {
                            self.apply_tb(a);
                            return;
                        }
                    }
                    if self.ocr_text_mode() {
                        self.begin_ocr_select(p);
                        return;
                    }
                    self.begin_drag();
                } else {
                    if self.annotating
                        && (self.annot.live.is_some()
                            || self.annot.is_selecting()
                            || self.annot.is_dragging_text()
                            || self.annot.is_swallowing_text_click()
                            || self.annot.is_editing_text())
                    {
                        self.annot.release(self.img_pt(p));
                        self.persist_needed = true;
                        self.dirty = true;
                    }
                    if let Some(g) = self.ocr_gesture.take() {
                        if !g.selecting {
                            if let Some(ocr) = self.ocr.as_mut() {
                                ocr.sel = None;
                            }
                            self.dirty = true;
                        }
                    }
                    self.drag = None;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                native::focus_window(&self.window);
                native::raise_on_top(&self.window);
                if let Some((x, y, _)) = native::pointer_in_client(&self.window) {
                    self.last_cursor = Point::new(x as f32, y as f32);
                }
                let raw = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (*y * 120.0) as i32,
                    MouseScrollDelta::PixelDelta(p) => p.y as i32,
                };
                let Some(angle) = self.wheel.take(raw) else {
                    return;
                };
                if self.font_menu_open {
                    self.font_menu_offset = self.font_menu_offset.saturating_add_signed(if angle > 0 { -3 } else { 3 });
                    self.relayout_tb();
                    self.dirty = true;
                    return;
                }
                if self.annotating
                    && (self.annot.tool.is_some()
                        || self.annot.selected.is_some()
                        || self.annot.option_target().is_some()
                        || self.annot.hit_test(self.img_pt(self.last_cursor)).is_some())
                {
                    if self.annot.wheel_at(self.img_pt(self.last_cursor), angle) {
                        crate::logutil::log_at("trace", &format!(
                            "pin wheel {angle:+} width={}",
                            self.annot.option_width()
                        ));
                        self.relayout_tb();
                        self.dirty = true;
                    }
                } else if angle > 0 {
                    self.set_scale_at(self.scale * 1.1, Some(self.last_cursor));
                } else if angle < 0 {
                    self.set_scale_at(self.scale / 1.1, Some(self.last_cursor));
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::Ime(winit::event::Ime::Commit(s)) => {
                crate::logutil::log_at("trace", &format!("pin: IME committed {} characters", s.chars().count()));
                self.annot.insert_str(s);
                self.dirty = true;
            }
            WindowEvent::Focused(true) => {
                native::focus_window(&self.window);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if self.font_menu_open && event.logical_key == Key::Named(NamedKey::Escape) {
                    self.font_menu_open = false;
                    self.relayout_tb();
                    self.dirty = true;
                    return;
                }
                if self.annot.text_edit.is_some() {
                    let ctrl = self.mods.control_key();
                    let shift = self.mods.shift_key();
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => self.annot.commit_text(true),
                        Key::Named(NamedKey::Enter) if shift => self.annot.type_text('\n'),
                        Key::Named(NamedKey::Enter) => {
                            if let Some(ed) = &self.annot.text_edit {
                                crate::logutil::log_at("trace", &format!("pin: text committed {} characters", ed.text.chars().count()));
                            }
                            self.annot.commit_text(false);
                        }
                        Key::Named(NamedKey::Backspace) => self.annot.type_text('\u{8}'),
                        Key::Named(NamedKey::Delete) => self.annot.type_text('\u{7f}'),
                        Key::Named(NamedKey::ArrowLeft) => self.annot.move_caret(-1),
                        Key::Named(NamedKey::ArrowRight) => self.annot.move_caret(1),
                        Key::Named(NamedKey::Home) => self.annot.move_caret(-2),
                        Key::Named(NamedKey::End) => self.annot.move_caret(2),
                        Key::Character(c) if c.as_str() == "v" && ctrl => {
                            if let Ok(s) = crate::clipboard::read_text() {
                                self.annot.insert_str(&s);
                            }
                        }
                        Key::Character(c) if (c.as_str() == "1" || c.as_str() == "[") && !ctrl => {
                            self.annot.adjust_width(-1);
                            self.relayout_tb();
                        }
                        Key::Character(c) if (c.as_str() == "2" || c.as_str() == "]") && !ctrl => {
                            self.annot.adjust_width(1);
                            self.relayout_tb();
                        }
                        _ => {
                            if !ctrl {
                                if let Some(t) = event.text.as_deref() {
                                    self.annot.insert_str(t);
                                }
                            }
                        }
                    }
                    self.dirty = true;
                    return;
                }
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if self.annotating {
                            self.show_toolbar(false);
                        } else {
                            self.close();
                        }
                    }
                    Key::Named(NamedKey::Space) => self.show_toolbar(!self.annotating),
                    Key::Named(NamedKey::Tab) if self.annotating => {
                        if crate::toolbar::toggle_grouped_variant(
                            &mut self.annot,
                            &mut self.arrow_kind,
                            &mut self.ellipse_on,
                        ) {
                            self.relayout_tb();
                            self.dirty = true;
                        }
                    }
                    Key::Named(NamedKey::F5) => {
                        self.pending_translate = true;
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                        self.copy_hotkey();
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("s") => self.save(),
                    Key::Character(c) if c.eq_ignore_ascii_case("w") => self.close(),
                    Key::Character(c) if c.eq_ignore_ascii_case("z") => {
                        self.annot.undo();
                        self.dirty = true;
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("y") => {
                        self.annot.redo();
                        self.dirty = true;
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("r") => {
                        self.show_toolbar(true);
                        crate::toolbar::set_shape_kind(
                            &mut self.annot,
                            &mut self.ellipse_on,
                            false,
                        );
                        self.relayout_tb();
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("e") => {
                        self.show_toolbar(true);
                        crate::toolbar::set_shape_kind(&mut self.annot, &mut self.ellipse_on, true);
                        self.relayout_tb();
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("p") => {
                        self.show_toolbar(true);
                        self.annot.set_tool(Some(Tool::Pen));
                        self.relayout_tb();
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("t") => {
                        self.show_toolbar(true);
                        self.annot.set_tool(Some(Tool::Text));
                        self.relayout_tb();
                    }
                    Key::Character(c) if c == "n" || c == "N" => {
                        self.show_toolbar(true);
                        self.annot.set_tool(Some(Tool::Number));
                        self.relayout_tb();
                    }
                    Key::Character(c) if c == "1" || c == "[" => {
                        if self.annotating && self.annot.tool.is_some() {
                            self.annot.adjust_width(-1);
                            self.relayout_tb();
                        } else if c == "1" {
                            self.rotate(-1);
                        }
                        self.dirty = true;
                    }
                    Key::Character(c) if c == "2" || c == "]" => {
                        if self.annotating && self.annot.tool.is_some() {
                            self.annot.adjust_width(1);
                            self.relayout_tb();
                        } else if c == "2" {
                            self.rotate(1);
                        }
                        self.dirty = true;
                    }
                    Key::Character(c) if c == "3" => {
                        self.flip(true);
                        self.dirty = true;
                    }
                    Key::Character(c) if c == "4" => {
                        self.flip(false);
                        self.dirty = true;
                    }
                    Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace)
                        if self.annot.tool == Some(Tool::Select) =>
                    {
                        self.annot.delete_selected();
                        self.dirty = true;
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::CloseRequested => self.close(),
            _ => {}
        }
        self.sync_cursor();
    }

    pub fn handle_ctrl_wheel(&mut self, dy: f32) {
        let raw = if dy > 0.0 {
            120
        } else if dy < 0.0 {
            -120
        } else {
            0
        };
        if self.wheel.take(raw).is_none() {
            return;
        }
        self.set_opacity(self.opacity + if dy > 0.0 { 0.1 } else { -0.1 });
    }

    pub fn rotate(&mut self, turns: i32) {
        self.img = self.rendered().rotate90(turns);
        self.rotation = ((self.rotation + turns * 90) % 360 + 360) % 360;
        self.annot.clear();
        self.clear_ocr();
        self.apply_size();
        self.persist_needed = true;
        self.dirty = true;
    }

    pub fn flip(&mut self, horizontal: bool) {
        self.img = self.rendered().flip(horizontal);
        if horizontal {
            self.mirror_x = !self.mirror_x;
        } else {
            self.mirror_y = !self.mirror_y;
        }
        self.annot.clear();
        self.clear_ocr();
        self.apply_size();
        self.persist_needed = true;
        self.dirty = true;
    }

    pub fn paste(&mut self) {
        if let crate::clipboard::ClipContent::Image(img) = crate::clipboard::read_best() {
            self.img = img;
            self.annot.clear();
            self.clear_ocr();
            self.apply_size();
            self.persist_needed = true;
            self.dirty = true;
        }
    }

    fn popup_menu(&mut self) {
        #[cfg(windows)]
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            append(menu, ID_COPY, &tr("Copy image"), false);
            append(menu, ID_SAVE, &tr("Save image as…"), false);
            append(menu, ID_TOOLBAR, &tr("Show toolbar"), self.annotating);
            append(menu, 23, &tr("Shadow"), self.shadow);
            append(menu, 0, "", true);
            append(menu, ID_CT, &tr("Click-through"), self.click_through);
            append(menu, 0, "", true);
            append(menu, ID_Z25, "25%", (self.scale - 0.25).abs() < 0.02);
            append(menu, ID_Z50, "50%", (self.scale - 0.5).abs() < 0.02);
            append(menu, ID_Z100, "100%", (self.scale - 1.0).abs() < 0.02);
            append(menu, ID_Z200, "200%", (self.scale - 2.0).abs() < 0.02);
            append(menu, ID_Z400, "400%", (self.scale - 4.0).abs() < 0.02);
            append(menu, 0, "", true);
            append(menu, ID_CLOSE, &tr("Close"), false);
            append(menu, 20, &tr("Destroy"), false);
            append(menu, 21, &tr("Paste"), false);
            append(menu, 22, &tr("Reset zoom"), false);
            let pos = self.window.outer_position().unwrap_or_default();
            let cx = pos.x + self.last_cursor.x as i32;
            let cy = pos.y + self.last_cursor.y as i32;
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_RETURNCMD,
                cx,
                cy,
                0,
                self.hwnd(),
                None,
            );
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                ID_COPY => {
                    self.copy_hotkey();
                }
                ID_SAVE => self.save(),
                ID_CLOSE => self.close(),
                ID_TOOLBAR => self.show_toolbar(!self.annotating),
                23 => self.set_shadow(!self.shadow),
                ID_CT => self.set_click_through(!self.click_through),
                ID_Z25 => self.set_scale(0.25),
                ID_Z50 => self.set_scale(0.5),
                ID_Z75 => self.set_scale(0.75),
                ID_Z100 => self.set_scale(1.0),
                ID_Z150 => self.set_scale(1.5),
                ID_Z200 => self.set_scale(2.0),
                ID_Z400 => self.set_scale(4.0),
                20 => self.destroy_pin(),
                21 => self.paste(),
                22 => self.set_scale(1.0),
                _ => {}
            }
        }
        #[cfg(not(windows))]
        {
            self.pending_menu = true;
        }
    }

    pub fn apply_menu(&mut self, a: crate::event::PinMenuAction) {
        use crate::event::PinMenuAction;
        match a {
            PinMenuAction::Copy => {
                self.copy_hotkey();
            }
            PinMenuAction::Save => self.save(),
            PinMenuAction::ToggleToolbar => self.show_toolbar(!self.annotating),
            PinMenuAction::ToggleClickThrough => self.set_click_through(!self.click_through),
            PinMenuAction::ToggleShadow => self.set_shadow(!self.shadow),
            PinMenuAction::Zoom(pct) => {
                let s = if pct == 33 {
                    1.0 / 3.0
                } else {
                    pct as f32 / 100.0
                };
                self.set_scale(s);
            }
            PinMenuAction::Rotate(t) => self.rotate(t),
            PinMenuAction::FlipH => self.flip(true),
            PinMenuAction::FlipV => self.flip(false),
            PinMenuAction::Paste => self.paste(),
            PinMenuAction::ReplaceFile => self.replace_from_file(),
            PinMenuAction::MoveGroup(g) => {
                self.group = g;
                self.persist_needed = true;
            }
            PinMenuAction::NewGroup
            | PinMenuAction::RenameGroup
            | PinMenuAction::NewGroupNamed(_)
            | PinMenuAction::RenameGroupNamed(_) => {}
            PinMenuAction::ViewInFolder => self.view_in_folder(),
            PinMenuAction::Close => self.close(),
            PinMenuAction::Destroy => self.destroy_pin(),
            PinMenuAction::CopySize => {
                let _ = crate::clipboard::write_text(&format!(
                    "{}x{}",
                    self.img.width, self.img.height
                ));
            }
            PinMenuAction::ResetZoom => self.set_scale(1.0),
            PinMenuAction::ToggleSmooth => {
                self.smooth = !self.smooth;
                self.dirty = true;
            }
            PinMenuAction::ToggleGrayscale => {
                self.grayscale = !self.grayscale;
                self.dirty = true;
            }
            PinMenuAction::ToggleInvert => {
                self.invert = !self.invert;
                self.dirty = true;
            }
            PinMenuAction::Ocr => {
                let running = self.ocr_running;
                self.request_ocr();
                if !running {
                    self.prefer_select_for_ocr();
                }
            }
            PinMenuAction::Translate => self.pending_translate = true,
        }
    }

    pub fn destroy_pin(&mut self) {
        self.destroy = true;
        self.close();
    }

    pub fn replace_from_file(&mut self) {
        let Some(path) = crate::dialog::open_image_path() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let Ok(img) = image::load_from_memory(&bytes) else {
            return;
        };
        let rgba = img.to_rgba8();
        if let Some(r) = Raster::from_rgba(rgba.width(), rgba.height(), rgba.into_raw()) {
            self.img = r;
            self.annot.clear();
            self.clear_ocr();
            self.apply_size();
            self.persist_needed = true;
            self.dirty = true;
        }
    }

    pub fn view_in_folder(&self) {
        let Some(path) = &self.saved_path else {
            return;
        };
        let dir = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(path));
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("explorer").arg(dir).spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }

    fn save(&mut self) {
        let format = pinshot_core::config::load()
            .map(|c| c.save_filename_format)
            .unwrap_or_else(|_| "pinshot-{date}-{time}.png".into());
        let suggested = pinshot_core::config::expand_save_filename(&format);
        if let Some(path) = crate::dialog::save_path(&suggested) {
            if crate::dialog::save_raster(&self.rendered(), &path, 92).is_ok() {
                self.saved_path = Some(path);
            }
        }
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.window.set_visible(false);
    }

    /// Python `mouseDoubleClickEvent`: first hit shows a tip, then close/hide/none.
    pub fn on_double_click(&mut self, cfg: &mut pinshot_core::Config, shift: bool) {
        if self.click_through {
            return;
        }
        if shift && self.annot.tool.is_none() {
            self.toggle_thumb();
            return;
        }
        if self.annot.tool == Some(Tool::Select)
            || (self.annot.tool.is_none() && self.annot.selected.is_some())
        {
            if let Some(i) = self.annot.hit_test(self.img_pt(self.last_cursor)) {
                if self.annot.shapes.get(i).map(|s| s.kind) == Some(Tool::Text) {
                    self.show_toolbar(true);
                    self.annot.set_tool(Some(Tool::Select));
                    self.annot.begin_edit_text(i);
                    self.dirty = true;
                    return;
                }
            }
        }
        if self.annot.tool.is_some() {
            return;
        }
        match cfg.dblclick_pin_action.as_str() {
            "none" => return,
            _ => {}
        }
        if !cfg.dblclick_tip_shown {
            cfg.dblclick_tip_shown = true;
            let _ = pinshot_core::config::save(cfg);
            let tip = if cfg.dblclick_pin_action == "hide" {
                tr("Tip: double-click hides a pin.")
            } else {
                tr("Tip: double-click closes a pin.")
            };
            self.show_toast(format!("{} {}", tip, tr("Double-click again to confirm.")));
            self.dirty = true;
            return;
        }
        if cfg.dblclick_pin_action == "hide" {
            self.hide();
        } else {
            self.close();
        }
    }

    pub fn redraw(&mut self) {
        if self.drag.is_some() {
            return;
        }
        self.expire_toast();
        remember_styles(&self.annot.styles);
        let (ww, hh) = self.win_px();
        if self
            .surface
            .resize(NonZeroU32::new(ww).unwrap(), NonZeroU32::new(hh).unwrap())
            .is_err()
        {
            return;
        }
        let img_pm = crate::draw::paint_pin_ex(
            &self.img,
            &self.annot,
            self.scale,
            self.click_through,
            self.glow,
            self.shadow,
            self.thumb,
            self.smooth,
            self.grayscale,
            self.invert,
        );
        let extra = self.ocr.is_some()
            || self.annotating
            || self.annot.selected.is_some()
            || self.color_picker.is_some()
            || self.status.is_some()
            || self.toast.is_some();
        let mut pm = if extra || img_pm.width() != ww || img_pm.height() != hh {
            let mut pm = tiny_skia::Pixmap::new(ww, hh)
                .unwrap_or_else(|| tiny_skia::Pixmap::new(8, 8).unwrap());
            // Keep the shadow margin premultiplied-clear. Filling 0xF7 made the
            // drop shadow an opaque grey slab (and a dark top/left crop band).
            let img_w = img_pm.width().min(ww);
            let img_h = img_pm.height().min(hh);
            {
                let src = img_pm.data();
                let dst = pm.data_mut();
                let sw = img_pm.width() as usize;
                for y in 0..img_h as usize {
                    let so = y * sw * 4;
                    let doff = y * ww as usize * 4;
                    let n = (img_w as usize) * 4;
                    if so + n <= src.len() && doff + n <= dst.len() {
                        dst[doff..doff + n].copy_from_slice(&src[so..so + n]);
                    }
                }
            }
            pm
        } else {
            img_pm
        };
        if let Some(ocr) = &self.ocr {
            let flashing = self.ocr_flash_until.is_some_and(|t| Instant::now() < t);
            if let Some((a, b)) = ocr.sel {
                let lo = a.min(b);
                let hi = a.max(b);
                for (i, tok) in ocr.page.tokens.iter().enumerate() {
                    if i < lo || i > hi {
                        continue;
                    }
                    fill_rect(
                        &mut pm,
                        self.token_win_rect(tok.rect),
                        Rgba::rgba(61, 143, 209, 90),
                    );
                }
            } else if flashing {
                for tok in &ocr.page.tokens {
                    fill_rect(
                        &mut pm,
                        self.token_win_rect(tok.rect),
                        Rgba::rgba(255, 196, 72, 70),
                    );
                }
            }
            if let Some(sr) = self.strip_rect() {
                fill_rect(&mut pm, sr, Rgba::rgba(252, 252, 253, 255));
                stroke_rect(&mut pm, sr, Rgba::rgba(206, 208, 214, 255), 1.0);
                let lines = self.strip_lines();
                for (i, line) in lines.iter().enumerate() {
                    if line.is_empty() {
                        continue;
                    }
                    let y = sr.y + OCR_STRIP_PAD + i as f32 * OCR_STRIP_LINE;
                    if y + OCR_STRIP_LINE > sr.bottom() + 1.0 {
                        break;
                    }
                    pinshot_core::font::blit_pixmap(
                        &mut pm,
                        line,
                        sr.x + OCR_STRIP_PAD,
                        y,
                        14.0,
                        Rgba::rgb(36, 36, 40),
                        false,
                    );
                }
            }
        }
        if self.annotating {
            if self.toolbar.is_none() {
                self.relayout_tb();
            }
            if let Some(tb) = &self.toolbar {
                paint_toolbar(&mut pm, tb, Some(self.last_cursor));
            }
            if let Some(ed) = &self.annot.text_edit {
                if let Some(r) = self.annot.text_edit_rect() {
                    let m = self.margin() as f32;
                    let rr = Rect::new(
                        m + r.x * self.scale,
                        m + r.y * self.scale,
                        r.w * self.scale,
                        r.h * self.scale,
                    );
                    crate::draw::fill_rect(
                        &mut pm,
                        rr,
                        pinshot_core::color::Rgba::rgba(255, 255, 255, 90),
                    );
                    let st = self.annot.text_edit_style();
                    crate::draw::stroke_rect(&mut pm, rr, st.color, 1.0);
                    pinshot_core::font::blit_pixmap_ex(
                        &mut pm,
                        &ed.text,
                        rr.x + 2.0,
                        rr.y + 2.0,
                        (st.font_size as f32 * self.scale).max(8.0),
                        st.color,
                        st.outline,
                        &st.family,
                        st.bold,
                        st.italic,
                    );
                    if self.annot.caret_visible() {
                        let caret_x = rr.x + 2.0 + self.annot.caret_x() * self.scale;
                        let ch = (rr.h - 4.0).max(8.0);
                        crate::draw::fill_rect(
                            &mut pm,
                            Rect::new(caret_x - 1.0, rr.y + 1.0, 4.0, ch + 2.0),
                            pinshot_core::color::Rgba::rgba(255, 255, 255, 200),
                        );
                        crate::draw::fill_rect(
                            &mut pm,
                            Rect::new(caret_x, rr.y + 2.0, 2.0, ch),
                            st.color,
                        );
                    }
                }
            }
        }
        if let Some(i) = self.annot.selected {
            if let Some(sh) = self.annot.shapes.get(i) {
                let m = self.margin() as f32;
                if !sh.is_line_stroke() {
                    let hb = sh.bounds().adjusted(-2.0, -2.0, 2.0, 2.0);
                    let sr = Rect::new(
                        m + hb.x * self.scale,
                        m + hb.y * self.scale,
                        hb.w * self.scale,
                        hb.h * self.scale,
                    );
                    stroke_rect(&mut pm, sr, Rgba::BLUE, 1.0);
                }
                for (_, c) in self.annot.selected_handle_points() {
                    crate::draw::paint_handle_dot(
                        &mut pm,
                        Point::new(m + c.x * self.scale, m + c.y * self.scale),
                        Rgba::SNIP_RING,
                    );
                }
                if let Some(r) = self.annot.selected_text_delete_rect() {
                    crate::draw::paint_text_delete(&mut pm, Rect::new(
                        m + r.x * self.scale, m + r.y * self.scale,
                        r.w * self.scale, r.h * self.scale,
                    ));
                }
            }
        }
        if let Some(picker) = &self.color_picker {
            paint_color_picker(&mut pm, ww, hh, picker);
        }
        if let Some(t) = self.status.as_ref().or(self.toast.as_ref()) {
            crate::draw::paint_toast(&mut pm, t, ww, hh);
        }
        if self.annotating && self.color_picker.is_none() {
            let mut hint = self.annot.cursor_hint(self.img_pt(self.last_cursor));
            hint.over_chrome = self.toolbar.as_ref().is_some_and(|t| t.contains(self.last_cursor));
            if let Some(width) = crate::cursor::brush_width(&self.annot, cursor_for(hint)) {
                crate::cursor::paint_brush(&mut pm, self.last_cursor, width * self.scale);
            }
        }
        if let Ok(mut buf) = self.surface.buffer_mut() {
            present_argb(&mut buf, &pm);
            let _ = buf.present();
        }
        self.dirty = false;
    }

    pub fn pump_redraw(&mut self) {
        self.expire_toast();
        if let Some(t) = self.ocr_flash_until {
            if Instant::now() >= t {
                self.ocr_flash_until = None;
                self.dirty = true;
            } else {
                self.dirty = true;
            }
        }
        if self.status.is_some() {
            self.dirty = true;
        }
        if self.annot.text_edit.is_some() {
            self.dirty = true;
        }
        if self.dirty && self.drag.is_none() {
            self.window.request_redraw();
        }
    }

    pub fn next_wake(&self) -> Option<Instant> {
        if self.glow > 0.0 {
            return Some(self.glow_at.unwrap_or_else(Instant::now));
        }
        if self.annot.text_edit.is_some() || self.status.is_some() {
            return Some(Instant::now() + Duration::from_millis(80));
        }
        match (self.toast_until, self.ocr_flash_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

/// OCR drag-select is allowed when the pin is idle, or the toolbar is open
/// on Select / no drawing tool. An explicit Rect/Pen/… still draws.
pub fn pin_ocr_text_mode(has_ocr: bool, thumb: bool, annotating: bool, tool: Option<Tool>) -> bool {
    has_ocr && !thumb && (!annotating || matches!(tool, None | Some(Tool::Select)))
}

pub fn pin_grab_offset(tl: (i32, i32), cursor: (i32, i32)) -> (i32, i32) {
    (cursor.0 - tl.0, cursor.1 - tl.1)
}

pub fn pin_origin_from_cursor(cursor: (i32, i32), grab: (i32, i32)) -> (i32, i32) {
    (cursor.0 - grab.0, cursor.1 - grab.1)
}

/// Keep the image pixel under `anchor` fixed while the scale changes.
pub fn zoom_pos(tl: (i32, i32), anchor: Point, old: f32, new: f32, margin: f32) -> (i32, i32) {
    let old = old.max(0.01);
    let ax = anchor.x - margin;
    let ay = anchor.y - margin;
    let nx = tl.0 + (ax * (1.0 - new / old)).round() as i32;
    let ny = tl.1 + (ay * (1.0 - new / old)).round() as i32;
    (nx, ny)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_uses_screen_cursor_minus_grab() {
        let grab = pin_grab_offset((100, 80), (140, 110));
        assert_eq!(grab, (40, 30));
        assert_eq!(pin_origin_from_cursor((200, 160), grab), (160, 130));
    }

    #[test]
    fn zoom_out_moves_origin_so_pointer_stays() {
        // Pin at (100,100), pointer 50px inside. Zoom 1.0 → 0.5 should move
        // the window by +25 so the same image pixel stays under the pointer.
        let (x, y) = zoom_pos((100, 100), Point::new(50.0, 40.0), 1.0, 0.5, 0.0);
        assert_eq!((x, y), (125, 120));
    }

    #[test]
    fn toolbar_default_select_lets_ocr_pick_text() {
        assert!(pin_ocr_text_mode(true, false, false, None));
        assert!(pin_ocr_text_mode(true, false, true, Some(Tool::Select)));
        assert!(pin_ocr_text_mode(true, false, true, None));
        assert!(
            !pin_ocr_text_mode(true, false, true, Some(Tool::Rect)),
            "drawing tools must still draw"
        );
        assert!(!pin_ocr_text_mode(true, true, false, None));
        assert!(!pin_ocr_text_mode(false, false, true, Some(Tool::Select)));
    }

    #[test]
    fn consecutive_zoom_out_accumulates_from_last_pos() {
        let mut tl = (148, 100);
        let anchor = Point::new(662.0, 137.0);
        let mut s = 1.0_f32;
        for _ in 0..3 {
            let n = s / 1.1;
            tl = zoom_pos(tl, anchor, s, n, 0.0);
            s = n;
        }
        assert!(
            tl.0 > 200 && tl.1 > 110,
            "tracked origin must walk, got {tl:?}"
        );
    }
}

#[cfg(windows)]
unsafe fn append(menu: HMENU, id: usize, text: &str, checked: bool) {
    if id == 0 {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
        return;
    }
    let w = wide(text);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    let _ = AppendMenuW(menu, flags, id, windows::core::PCWSTR(w.as_ptr()));
}
