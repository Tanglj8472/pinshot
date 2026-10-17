use std::num::NonZeroU32;
use std::rc::Rc;

use pinshot_core::geom::Point;
use pinshot_core::icons::app_icon_rgba;
use softbuffer::{Context, Surface};
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::draw::{fill_rect, present, UiTheme};

pub struct TranslatePopup {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    source: String,
    translated: String,
    cursor: Point,
    theme: UiTheme,
}

impl TranslatePopup {
    pub fn create(event_loop: &ActiveEventLoop, source: String, translated: String) -> Result<Self, String> {
        let max_chars = source.chars().count().max(translated.chars().count());
        let lines = (max_chars as f64 / 58.0).ceil().clamp(1.0, 12.0);
        let height = (112.0 + lines * 28.0).clamp(180.0, 520.0);
        let width = (420.0 + (max_chars.min(80) as f64 * 2.0)).clamp(420.0, 760.0);
        let (iw, ih, rgba) = app_icon_rgba(32);
        let icon = winit::window::Icon::from_rgba(rgba, iw, ih).ok();
        let cursor = crate::native::cursor_screen().unwrap_or((0, 0));
        let desk = crate::native::virtual_desktop();
        let (x, y) = pinshot_core::geom::place_near_cursor(cursor, (width as u32, height as u32), desk);
        let window = Rc::new(event_loop.create_window(
            Window::default_attributes()
                .with_title("PinShot - Translation")
                .with_window_icon(icon)
                .with_inner_size(LogicalSize::new(width, height))
                .with_position(PhysicalPosition::new(x, y))
                .with_resizable(true),
        ).map_err(|e| e.to_string())?);
        crate::native::exclude_from_capture(&window);
        let context = Context::new(window.clone()).map_err(|e| format!("{e:?}"))?;
        let surface = Surface::new(&context, window.clone()).map_err(|e| format!("{e:?}"))?;
        let mut popup = Self { window, surface, source, translated, cursor: Point::ZERO, theme: UiTheme::resolve("system") };
        popup.redraw();
        Ok(popup)
    }

    pub fn window_id(&self) -> WindowId { self.window.id() }

    pub fn handle(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => self.cursor = Point::new(position.x as f32, position.y as f32),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if matches!(event.logical_key, Key::Named(NamedKey::Escape)) { return true; }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => return true,
            WindowEvent::CloseRequested => return true,
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
        self.window.request_redraw();
        false
    }

    fn redraw(&mut self) {
        let size = self.window.inner_size();
        let (w, h) = (size.width.max(8), size.height.max(8));
        if self.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).is_err() { return; }
        let Some(mut pm) = tiny_skia::Pixmap::new(w, h) else { return; };
        fill_rect(&mut pm, pinshot_core::geom::Rect::new(0.0, 0.0, w as f32, h as f32), self.theme.bg);
        crate::text::blit_into_pm(&mut pm, "Selected text", 20.0, 20.0, 14, self.theme.muted, true);
        crate::text::blit_into_pm(&mut pm, &self.source, 20.0, 46.0, 16, self.theme.text, true);
        crate::text::blit_into_pm(&mut pm, "Translation", 20.0, 112.0, 14, self.theme.muted, true);
        crate::text::blit_into_pm(&mut pm, &self.translated, 20.0, 138.0, 18, self.theme.text, true);
        if let Ok(mut buf) = self.surface.buffer_mut() { present(&mut buf, &pm); let _ = buf.present(); }
    }
}
