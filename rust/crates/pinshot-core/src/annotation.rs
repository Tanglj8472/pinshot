//! Annotation document, tools, hit-testing, and undo/redo (snapshot stack).
//!
//! Mirrors `pinshot/annotate.py`: `option_target` is live | selected | edit_target |
//! width_target; `wheel()` accumulates Qt-style `angleDelta/120` and merges undo
//! for 0.5s on the same committed shape.

use crate::color::{palette_colors, Rgba};
use crate::geom::{
    constrain_square, dist_to_polyline, handle_points, hit_handle, Handle, Point, Rect, HANDLE_SIZE,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Select,
    Rect,
    Ellipse,
    Line,
    Pen,
    Marker,
    Mosaic,
    Text,
    Number,
    Eraser,
}

impl Tool {
    pub const DRAW: [Tool; 9] = [
        Tool::Rect,
        Tool::Ellipse,
        Tool::Line,
        Tool::Pen,
        Tool::Marker,
        Tool::Mosaic,
        Tool::Text,
        Tool::Number,
        Tool::Eraser,
    ];

    /// Phase-1 toolbar (no select / eraser / OCR).
    pub const PHASE1: [Tool; 8] = [
        Tool::Rect,
        Tool::Ellipse,
        Tool::Line,
        Tool::Pen,
        Tool::Marker,
        Tool::Mosaic,
        Tool::Text,
        Tool::Number,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Rect => "rect",
            Tool::Ellipse => "ellipse",
            Tool::Line => "line",
            Tool::Pen => "pen",
            Tool::Marker => "marker",
            Tool::Mosaic => "mosaic",
            Tool::Text => "text",
            Tool::Number => "number",
            Tool::Eraser => "eraser",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            "select" => Tool::Select,
            "rect" => Tool::Rect,
            "ellipse" => Tool::Ellipse,
            "line" => Tool::Line,
            "arrow" => Tool::Line,
            "pen" | "pencil" => Tool::Pen,
            "marker" => Tool::Marker,
            "mosaic" => Tool::Mosaic,
            "text" => Tool::Text,
            "number" => Tool::Number,
            "eraser" => Tool::Eraser,
            _ => return None,
        })
    }

    pub fn tip(self) -> &'static str {
        match self {
            Tool::Select => "Select / move / resize (V)",
            Tool::Rect => "Rectangle",
            Tool::Ellipse => "Ellipse",
            Tool::Line => "Line / arrow (Shift+drag continues polyline)",
            Tool::Pen => "Brush",
            Tool::Marker => "Marker",
            Tool::Mosaic => "Mosaic / blur",
            Tool::Text => "Text",
            Tool::Number => "Numbered marker (N)",
            Tool::Eraser => "Eraser",
        }
    }

    pub fn is_brush(self, style: &Style) -> bool {
        matches!(self, Tool::Pen)
            || (matches!(self, Tool::Marker | Tool::Mosaic | Tool::Eraser) && style.mode == DrawMode::Brush)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineStyleKind {
    Solid,
    Dash,
    Dot,
}

/// Line-tool head (参考工具: none / thin / thick / hollow).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrowKind {
    #[default]
    None,
    Thin,
    Thick,
    Hollow,
}

impl ArrowKind {
    pub fn is_arrow(self) -> bool {
        !matches!(self, ArrowKind::None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawMode {
    Brush,
    Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Mosaic,
    Blur,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub color: Rgba,
    pub width: i32,
    pub filled: bool,
    /// Legacy flag kept so `session.toml` still loads; prefer [`Style::arrow_kind`].
    #[serde(default)]
    pub arrow: bool,
    #[serde(default)]
    pub arrow_kind: ArrowKind,
    pub line_style: LineStyleKind,
    pub mode: DrawMode,
    pub effect: Effect,
    /// Mosaic cell size / blur strength, independent of brush width.
    pub size: i32,
    pub bold: bool,
    pub italic: bool,
    pub outline: bool,
    pub family: String,
    pub font_size: i32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: Rgba::RED,
            width: 3,
            filled: false,
            arrow: false,
            arrow_kind: ArrowKind::None,
            line_style: LineStyleKind::Solid,
            mode: DrawMode::Rect,
            effect: Effect::Mosaic,
            size: 6,
            bold: false,
            italic: false,
            outline: true,
            family: String::new(),
            font_size: 20,
        }
    }
}

impl Style {
    pub fn for_tool(tool: Tool) -> Self {
        let mut s = Self::default();
        match tool {
            Tool::Marker => {
                s.color = Rgba::MARKER_YELLOW;
                s.width = 14;
                s.mode = DrawMode::Brush;
            }
            Tool::Pen => s.width = 3,
            Tool::Line => {
                s.width = 3;
                s.set_arrow_kind(ArrowKind::None);
            }
            Tool::Eraser => {
                s.mode = DrawMode::Brush;
                s.width = 16;
            }
            Tool::Mosaic => {
                s.mode = DrawMode::Brush;
                s.width = 36;
            }
            Tool::Number => {
                s.width = 22;
                s.filled = true;
                s.font_size = 14;
            }
            // 参考工具: default text annotation is not outlined; dark interface font.
            Tool::Text => {
                s.outline = false;
                s.font_size = 22;
                s.color = Rgba::rgb(30, 30, 30);
            }
            _ => {}
        }
        s
    }

    pub fn arrow_kind(&self) -> ArrowKind {
        match self.arrow_kind {
            ArrowKind::None if self.arrow => ArrowKind::Thin,
            k => k,
        }
    }

    pub fn has_arrow(&self) -> bool {
        self.arrow_kind().is_arrow()
    }

    pub fn set_arrow_kind(&mut self, kind: ArrowKind) {
        self.arrow_kind = kind;
        self.arrow = kind.is_arrow();
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub kind: Tool,
    pub pts: Vec<Point>,
    pub style: Style,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub number: i32,
    /// Stable identity for the experimental automation editor; absent in ordinary documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<u64>,
}

impl Shape {
    pub fn is_line_stroke(&self) -> bool {
        self.kind == Tool::Line
    }
    pub fn new(kind: Tool, pts: Vec<Point>, style: Style) -> Self {
        Self {
            kind,
            pts,
            style,
            text: String::new(),
            number: 0,
            automation_id: None,
        }
    }

    pub fn bounds(&self) -> Rect {
        if self.kind == Tool::Number {
            if let Some(c) = self.pts.first() {
                let r = (self.style.width.max(8) as f32) / 2.0;
                return Rect::new(c.x - r, c.y - r, r * 2.0, r * 2.0);
            }
        }
        if self.kind == Tool::Text && !self.text.is_empty() {
            if let Some(p) = self.pts.first() {
                let (w, h) = crate::font::measure_ex(
                    &self.text,
                    self.style.font_size.max(8) as f32,
                    &self.style.family,
                    self.style.bold,
                    self.style.italic,
                );
                return Rect::new(p.x, p.y, w + 4.0, h + 2.0);
            }
        }
        if matches!(self.kind, Tool::Pen | Tool::Marker | Tool::Line | Tool::Mosaic | Tool::Eraser) && self.pts.len() > 2 {
            let xs = self.pts.iter().map(|p| p.x);
            let ys = self.pts.iter().map(|p| p.y);
            let x0 = xs.clone().fold(f32::MAX, f32::min);
            let x1 = xs.fold(f32::MIN, f32::max);
            let y0 = ys.clone().fold(f32::MAX, f32::min);
            let y1 = ys.fold(f32::MIN, f32::max);
            return Rect::from_xyxy(x0, y0, x1, y1);
        }
        if self.pts.len() < 2 {
            let p = self.pts.first().copied().unwrap_or(Point::ZERO);
            return Rect::new(p.x - 2.0, p.y - 2.0, 4.0, 4.0);
        }
        Rect::from_points(self.pts[0], *self.pts.last().unwrap()).normalized()
    }
}

/// Start / end nubs for a 2-point line (not the 8-handle AABB of `bounds()`).
fn line_endpoint_handles(sh: &Shape) -> Option<[(Handle, Point); 2]> {
    if !sh.is_line_stroke()
        || sh.pts.len() != 2
    {
        return None;
    }
    Some([(Handle::Tl, sh.pts[0]), (Handle::Br, sh.pts[1])])
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UndoStack {
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
}

impl UndoStack {
    pub const LIMIT: usize = 50;

    pub fn push(&mut self, before: Vec<Shape>) {
        self.undo.push(before);
        if self.undo.len() > Self::LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, current: Vec<Shape>) -> Option<Vec<Shape>> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        Some(prev)
    }

    pub fn redo(&mut self, current: Vec<Shape>) -> Option<Vec<Shape>> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        Some(next)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    pub fn translate_all(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let d = Point::new(dx, dy);
        for doc in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            for sh in doc {
                for p in &mut sh.pts {
                    *p = p.add(d);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelMode {
    Move,
    Handle(Handle),
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    pub img_pt: Point,
    pub text: String,
    pub existing: Option<usize>,
    /// UTF-8 byte offset of the caret.
    pub caret: usize,
    started: Instant,
    blink: Instant,
}

#[derive(Clone, Debug)]
pub struct Annotator {
    pub tool: Option<Tool>,
    pub styles: [Style; 9],
    pub shapes: Vec<Shape>,
    pub live: Option<Shape>,
    pub selected: Option<usize>,
    /// Shapes that have been explicitly deselected cannot be selected again.
    /// This matches the one-edit session used by the annotation UI.
    selection_locked: Vec<bool>,
    pub next_number: i32,
    pub text_edit: Option<TextEdit>,
    /// Last committed shape the user clicked (Python `edit_target`).
    pub edit_target: Option<usize>,
    /// Last added shape (Python `width_target`); wheel/style hit this when nothing is selected.
    pub width_target: Option<usize>,
    undo: UndoStack,
    sel_mode: Option<SelMode>,
    sel_origin: Point,
    sel_pts0: Vec<Point>,
    sel_text_resize: Option<(Rect, i32)>,
    sel_dirty: bool,
    /// Text-tool press on committed text: click re-edits, drag moves.
    pending_text_click: Option<Point>,
    /// After click-outside commit, ignore the rest of this click so we do not
    /// immediately open a new empty box (参考工具: next click places).
    swallow_text_place: bool,
    line_press: Option<Point>,
    line_click_mode: bool,
    wheel_target: Option<usize>,
    wheel_time: Instant,
    wheel_delta: i32,
}

impl Default for Annotator {
    fn default() -> Self {
        Self {
            tool: None,
            styles: Tool::DRAW.map(Style::for_tool),
            shapes: Vec::new(),
            live: None,
            selected: None,
            selection_locked: Vec::new(),
            next_number: 1,
            text_edit: None,
            edit_target: None,
            width_target: None,
            undo: UndoStack::default(),
            sel_mode: None,
            sel_origin: Point::ZERO,
            sel_pts0: Vec::new(),
            sel_text_resize: None,
            sel_dirty: false,
            pending_text_click: None,
            swallow_text_place: false,
            line_press: None,
            line_click_mode: false,
            wheel_target: None,
            wheel_time: Instant::now(),
            wheel_delta: 0,
        }
    }
}

impl Annotator {
    pub fn set_draw_mode(&mut self, tool: Tool, mode: DrawMode) {
        if matches!(tool, Tool::Marker | Tool::Mosaic | Tool::Eraser) {
            self.set_tool(Some(tool));
            self.style_mut(tool).mode = mode;
        } else {
            self.set_style_value(tool, |s| s.mode = mode);
        }
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub fn style_index(tool: Tool) -> Option<usize> {
        Tool::DRAW.iter().position(|&t| t == tool)
    }

    pub fn style(&self, tool: Tool) -> &Style {
        let i = Self::style_index(tool).unwrap_or(0);
        &self.styles[i]
    }

    pub fn style_mut(&mut self, tool: Tool) -> &mut Style {
        let i = Self::style_index(tool).unwrap_or(0);
        &mut self.styles[i]
    }

    pub fn set_tool(&mut self, tool: Option<Tool>) {
        self.finish();
        self.width_target = None;
        self.edit_target = None;
        self.wheel_target = None;
        self.wheel_delta = 0;
        if tool != Some(Tool::Select) {
            self.lock_selected();
            self.selected = None;
        }
        self.tool = tool;
        self.sel_mode = None;
    }

    fn lock_selected(&mut self) {
        if let Some(i) = self.selected {
            if i < self.selection_locked.len() {
                self.selection_locked[i] = true;
            }
        }
    }

    pub fn all_shapes(&self) -> impl Iterator<Item = &Shape> {
        self.shapes.iter().chain(self.live.as_ref())
    }

    /// Shapes to paint: hide the committed copy while its editor is open
    /// so glyphs are not drawn twice (参考工具).
    pub fn visible_shapes(&self) -> Vec<Shape> {
        let skip = self.text_edit.as_ref().and_then(|e| e.existing);
        let mut out: Vec<Shape> = self
            .shapes
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .map(|(_, s)| s.clone())
            .collect();
        if let Some(live) = &self.live {
            out.push(live.clone());
        }
        out
    }

    fn push_edit(&mut self) {
        self.wheel_target = None;
        self.undo.push(self.shapes.clone());
    }

    pub fn add(&mut self, shape: Shape) {
        let selectable = !matches!(shape.kind, Tool::Pen | Tool::Eraser)
            && !(matches!(shape.kind, Tool::Marker | Tool::Mosaic) && shape.style.mode == DrawMode::Brush);
        self.push_edit();
        self.shapes.push(shape);
        let i = self.shapes.len() - 1;
        self.selection_locked.push(false);
        // Python `add` sets only `width_target`. `option_target` is
        // live | selected | edit_target | width_target, so Solid/color after a
        // draw still edit that shape. Python paints handles only for `selected`
        // (None after add) — keep the edit, and select the shape so the 8
        // handles make the target visible and hittable.
        self.width_target = selectable.then_some(i);
        self.selected = selectable.then_some(i);
        self.edit_target = selectable.then_some(i);
        self.renumber();
    }

    pub fn undo(&mut self) -> bool {
        self.finish();
        self.selected = None;
        self.selection_locked.clear();
        self.edit_target = None;
        self.wheel_target = None;
        self.width_target = None;
        if let Some(prev) = self.undo.undo(self.shapes.clone()) {
            self.shapes = prev;
            self.renumber();
            return true;
        }
        false
    }

    pub fn redo(&mut self) -> bool {
        self.selected = None;
        self.selection_locked.clear();
        self.edit_target = None;
        self.wheel_target = None;
        self.width_target = None;
        if let Some(next) = self.undo.redo(self.shapes.clone()) {
            self.shapes = next;
            self.renumber();
            return true;
        }
        false
    }

    pub fn automation_busy(&self) -> bool {
        self.live.is_some() || self.text_edit.is_some() || self.sel_mode.is_some()
            || self.pending_text_click.is_some() || self.line_click_mode
    }

    pub fn replace_automation_shapes(&mut self, shapes: Vec<Shape>) {
        self.push_edit();
        self.shapes = shapes;
        self.selected = None;
        self.selection_locked.clear();
        self.edit_target = None;
        self.width_target = None;
        self.wheel_target = None;
        self.renumber();
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.undo_len()
    }

    /// Shift every shape (and undo history) in freeze / image space.
    /// The overlay hole is a dim viewport and does not call this on move.
    pub fn translate_all(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let d = Point::new(dx, dy);
        fn shift(pts: &mut [Point], d: Point) {
            for p in pts {
                *p = p.add(d);
            }
        }
        for sh in &mut self.shapes {
            shift(&mut sh.pts, d);
        }
        if let Some(sh) = self.live.as_mut() {
            shift(&mut sh.pts, d);
        }
        if let Some(ed) = self.text_edit.as_mut() {
            ed.img_pt = ed.img_pt.add(d);
        }
        shift(&mut self.sel_pts0, d);
        self.sel_origin = self.sel_origin.add(d);
        if let Some((bounds, _)) = self.sel_text_resize.as_mut() {
            *bounds = bounds.translated(dx, dy);
        }
        self.undo.translate_all(dx, dy);
    }

    pub fn clear(&mut self) {
        self.shapes.clear();
        self.line_press = None;
        self.line_click_mode = false;
        self.live = None;
        self.selected = None;
        self.next_number = 1;
        self.undo.clear();
        self.text_edit = None;
        self.sel_mode = None;
        self.edit_target = None;
        self.width_target = None;
        self.wheel_target = None;
        self.wheel_delta = 0;
    }

    pub fn renumber(&mut self) {
        let mut n = 1;
        for sh in &mut self.shapes {
            if sh.kind == Tool::Number {
                sh.number = n;
                n += 1;
            }
        }
        self.next_number = n;
    }

    pub fn finish(&mut self) {
        self.line_press = None;
        self.line_click_mode = false;
        if let Some(ed) = self.text_edit.take() {
            if !ed.text.trim().is_empty() {
                if let Some(idx) = ed.existing {
                    if idx < self.shapes.len() {
                        self.push_edit();
                        self.shapes[idx].text = ed.text;
                        self.shapes[idx].pts = vec![ed.img_pt];
                    }
                } else {
                    let mut sh =
                        Shape::new(Tool::Text, vec![ed.img_pt], self.style(Tool::Text).clone());
                    sh.text = ed.text;
                    self.add(sh);
                }
            }
            // 参考工具: committing text does not leave a selection box / 8 handles.
            // `add()` would otherwise keep width_target, and chrome paints that.
            self.selected = None;
            self.edit_target = None;
            self.width_target = None;
        }
        if let Some(mut sh) = self.live.take() {
            if sh.kind == Tool::Line {
                while sh.pts.len() > 2 && sh.pts[sh.pts.len() - 2] == *sh.pts.last().unwrap() {
                    sh.pts.pop();
                }
            }
            let marker_dot = matches!(sh.kind, Tool::Marker | Tool::Mosaic | Tool::Eraser)
                && sh.style.mode == DrawMode::Brush
                && !sh.pts.is_empty();
            let ok = marker_dot || sh.pts.len() > 1
                && (sh.pts[0].manhattan(*sh.pts.last().unwrap()) > 2.0 || sh.pts.len() > 2);
            if ok {
                self.add(sh);
            }
        }
        self.sel_mode = None;
    }

    pub fn set_style_value(&mut self, kind: Tool, setter: impl Fn(&mut Style)) {
        if self.text_edit.is_some() {
            if let Some(idx) = self.text_edit.as_ref().and_then(|e| e.existing) {
                if idx < self.shapes.len() && self.shapes[idx].kind == kind {
                    self.push_edit();
                    setter(&mut self.shapes[idx].style);
                }
            }
            setter(self.style_mut(kind));
            return;
        }
        if let Some(live) = self.live.as_mut() {
            if live.kind == kind {
                setter(&mut live.style);
                setter(self.style_mut(kind));
                return;
            }
        }
        if let Some(idx) = self.option_target() {
            if self.shapes[idx].kind == kind {
                self.push_edit();
                setter(&mut self.shapes[idx].style);
            }
        }
        setter(self.style_mut(kind));
    }

    /// Change the default style for *new* drawings only (picking Arrow/Line in
    /// the toolbar must not rewrite the last committed shape).
    pub fn set_style_default(&mut self, kind: Tool, setter: impl Fn(&mut Style)) {
        setter(self.style_mut(kind));
    }

    /// 参考工具: rectangle and ellipse share one tool; switching the chip (or Tab)
    /// retargets the live / selected shape instead of starting a new one.
    pub fn convert_rect_ellipse(&mut self, to: Tool) {
        if !matches!(to, Tool::Rect | Tool::Ellipse) {
            return;
        }
        let from = if to == Tool::Ellipse {
            Tool::Rect
        } else {
            Tool::Ellipse
        };
        if let Some(live) = self.live.as_mut() {
            if matches!(live.kind, Tool::Rect | Tool::Ellipse) {
                live.kind = to;
                self.tool = Some(to);
                *self.style_mut(to) = live.style.clone();
                return;
            }
        }
        if let Some(idx) = self.option_target() {
            if matches!(self.shapes[idx].kind, Tool::Rect | Tool::Ellipse) {
                if self.shapes[idx].kind != to {
                    self.push_edit();
                    self.shapes[idx].kind = to;
                }
                self.tool = Some(to);
                self.selected = Some(idx);
                self.edit_target = Some(idx);
                self.width_target = Some(idx);
                *self.style_mut(to) = self.shapes[idx].style.clone();
                return;
            }
        }
        self.copy_draw_style(from, to);
        self.set_tool(Some(to));
    }

    /// Line and arrow share one tool; switching the chip (or Tab) retargets the
    /// live / selected line instead of starting a new one.
    pub fn convert_line_arrow(&mut self, kind: ArrowKind) {
        if let Some(live) = self.live.as_mut() {
            if live.kind == Tool::Line {
                live.style.set_arrow_kind(kind);
                self.tool = Some(Tool::Line);
                *self.style_mut(Tool::Line) = live.style.clone();
                return;
            }
        }
        if let Some(idx) = self.option_target() {
            if self.shapes[idx].kind == Tool::Line {
                if self.shapes[idx].style.arrow_kind() != kind {
                    self.push_edit();
                    self.shapes[idx].style.set_arrow_kind(kind);
                }
                self.tool = Some(Tool::Line);
                self.selected = Some(idx);
                self.edit_target = Some(idx);
                self.width_target = Some(idx);
                *self.style_mut(Tool::Line) = self.shapes[idx].style.clone();
                return;
            }
        }
        self.set_tool(Some(Tool::Line));
        self.style_mut(Tool::Line).set_arrow_kind(kind);
    }

    pub fn current_line_arrow(&self) -> ArrowKind {
        if let Some(live) = self.live.as_ref() {
            if live.kind == Tool::Line {
                return live.style.arrow_kind();
            }
        }
        if let Some(idx) = self.option_target() {
            if self.shapes[idx].kind == Tool::Line {
                return self.shapes[idx].style.arrow_kind();
            }
        }
        self.style(Tool::Line).arrow_kind()
    }

    fn copy_draw_style(&mut self, from: Tool, to: Tool) {
        if from == to {
            return;
        }
        let src = self.style(from).clone();
        let dst = self.style_mut(to);
        dst.color = src.color;
        dst.width = src.width;
        dst.filled = src.filled;
        dst.line_style = src.line_style;
    }

    /// Index of the committed shape that style/width edits apply to.
    /// Python: `live or selected or edit_target or width_target` — live is handled
    /// separately because it is not in `shapes`.
    pub fn option_target(&self) -> Option<usize> {
        if self.live.is_some() || self.text_edit.is_some() {
            return None;
        }
        for cand in [self.selected, self.edit_target, self.width_target] {
            if let Some(i) = cand {
                if i < self.shapes.len() {
                    return Some(i);
                }
            }
        }
        None
    }

    pub fn option_tool(&self) -> Option<Tool> {
        if matches!(self.tool, Some(Tool::Eraser | Tool::Mosaic)) {
            return self.tool;
        }
        if let Some(live) = &self.live {
            return Some(live.kind);
        }
        if let Some(i) = self.option_target() {
            return Some(self.shapes[i].kind);
        }
        self.tool
    }

    /// Style shown in the toolbar panel: live / selected shape, else the tool default.
    pub fn option_style(&self) -> Option<&Style> {
        if let Some(ed) = &self.text_edit {
            if let Some(i) = ed.existing {
                if let Some(sh) = self.shapes.get(i) {
                    return Some(&sh.style);
                }
            }
            return Some(self.style(Tool::Text));
        }
        if let Some(live) = &self.live {
            return Some(&live.style);
        }
        if let Some(i) = self.option_target() {
            return Some(&self.shapes[i].style);
        }
        self.tool
            .filter(|t| *t != Tool::Select)
            .map(|t| self.style(t))
    }

    /// Width shown in the toolbar panel: live / selected shape, else the tool default.
    /// Text uses `font_size`; other tools use brush/stroke width.
    pub fn option_width(&self) -> i32 {
        let (kind, st) = if self.text_edit.is_some() {
            (
                Tool::Text,
                match self.text_edit.as_ref().and_then(|e| e.existing) {
                    Some(i) => self
                        .shapes
                        .get(i)
                        .map(|s| &s.style)
                        .unwrap_or_else(|| self.style(Tool::Text)),
                    None => self.style(Tool::Text),
                },
            )
        } else if let Some(live) = &self.live {
            (live.kind, &live.style)
        } else if let Some(i) = self.option_target() {
            (self.shapes[i].kind, &self.shapes[i].style)
        } else if let Some(t) = self.tool.filter(|t| *t != Tool::Select) {
            (t, self.style(t))
        } else {
            return 0;
        };
        match kind {
            Tool::Text => st.font_size,
            _ => st.width,
        }
    }

    pub fn adjust_width(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        if self.text_edit.is_some() {
            let existing = self.text_edit.as_ref().and_then(|e| e.existing);
            if let Some(i) = existing {
                let now = Instant::now();
                if self.wheel_target != Some(i)
                    || now.duration_since(self.wheel_time) > Duration::from_millis(500)
                {
                    self.push_edit();
                }
                self.wheel_target = Some(i);
                self.wheel_time = now;
                Self::nudge_style(&mut self.shapes[i].style, Tool::Text, delta);
                let st = self.shapes[i].style.clone();
                *self.style_mut(Tool::Text) = st;
            } else {
                Self::nudge_style(self.style_mut(Tool::Text), Tool::Text, delta);
            }
            return true;
        }
        if let Some(live) = self.live.as_mut() {
            let kind = live.kind;
            Self::nudge_style(&mut live.style, kind, delta);
            let st = live.style.clone();
            *self.style_mut(kind) = st;
            return true;
        }
        let target = self.option_target();
        let kind = target
            .map(|i| self.shapes[i].kind)
            .or(self.tool.filter(|t| *t != Tool::Select));
        let Some(kind) = kind else {
            return false;
        };
        if let Some(i) = target {
            let now = Instant::now();
            if self.wheel_target != Some(i)
                || now.duration_since(self.wheel_time) > Duration::from_millis(500)
            {
                self.push_edit();
            }
            self.wheel_target = Some(i);
            self.wheel_time = now;
            Self::nudge_style(&mut self.shapes[i].style, kind, delta);
            let st = self.shapes[i].style.clone();
            *self.style_mut(kind) = st;
        } else {
            Self::nudge_style(self.style_mut(kind), kind, delta);
        }
        true
    }

    pub fn set_option_width(&mut self, value: i32) -> bool {
        let cur = self.option_width();
        if cur == 0 {
            return false;
        }
        let kind = if self.text_edit.is_some() {
            Tool::Text
        } else if let Some(live) = &self.live {
            live.kind
        } else if let Some(i) = self.option_target() {
            self.shapes[i].kind
        } else if let Some(t) = self.tool.filter(|t| *t != Tool::Select) {
            t
        } else {
            return false;
        };
        let clamped = match kind {
            Tool::Text => value.clamp(8, 120),
            Tool::Number => value.clamp(12, 80),
            _ => value.clamp(1, 60),
        };
        if clamped == cur {
            return false;
        }
        let assign = |st: &mut Style| Self::assign_style_width(st, kind, clamped);
        if self.text_edit.is_some() {
            let existing = self.text_edit.as_ref().and_then(|e| e.existing);
            if let Some(i) = existing {
                self.push_edit();
                assign(&mut self.shapes[i].style);
                let st = self.shapes[i].style.clone();
                *self.style_mut(Tool::Text) = st;
            } else {
                assign(self.style_mut(Tool::Text));
            }
            return true;
        }
        if let Some(live) = self.live.as_mut() {
            assign(&mut live.style);
            let st = live.style.clone();
            *self.style_mut(kind) = st;
            return true;
        }
        if let Some(i) = self.option_target() {
            self.push_edit();
            assign(&mut self.shapes[i].style);
            let st = self.shapes[i].style.clone();
            *self.style_mut(kind) = st;
        } else {
            assign(self.style_mut(kind));
        }
        true
    }

    fn assign_style_width(st: &mut Style, kind: Tool, value: i32) {
        match kind {
            Tool::Text => st.font_size = value,
            Tool::Number => {
                st.width = value;
                st.font_size = (st.width as f32 * 0.65) as i32;
                st.font_size = st.font_size.max(8);
            }
            _ => st.width = value,
        }
    }

    /// Qt `QWheelEvent.angleDelta().y()` — 120 units per notch. Truncates toward 0.
    pub fn wheel(&mut self, angle_delta: i32) -> bool {
        self.wheel_delta += angle_delta;
        let steps = self.wheel_delta / 120;
        self.wheel_delta -= steps * 120;
        if steps != 0 {
            self.adjust_width(steps)
        } else {
            false
        }
    }

    fn nudge_style(st: &mut Style, kind: Tool, delta: i32) {
        match kind {
            Tool::Text => st.font_size = (st.font_size + 2 * delta).clamp(8, 120),
            Tool::Number => {
                st.width = (st.width + 2 * delta).clamp(12, 80);
                st.font_size = (st.width as f32 * 0.65) as i32;
                st.font_size = st.font_size.max(8);
            }
            _ => st.width = (st.width + delta).clamp(1, 60),
        }
    }

    pub fn hit_test(&self, img_pt: Point) -> Option<usize> {
        for (i, sh) in self.shapes.iter().enumerate().rev() {
            if self.selection_locked.get(i).copied().unwrap_or(false) {
                continue;
            }
            if sh.kind == Tool::Pen || (matches!(sh.kind, Tool::Marker | Tool::Mosaic) && sh.style.mode == DrawMode::Brush) {
                continue;
            }
            if sh.kind == Tool::Eraser {
                if self.hits_shape(sh, img_pt) {
                    return None;
                }
                continue;
            }
            if self.hits_shape(sh, img_pt) {
                return Some(i);
            }
        }
        None
    }

    fn hits_shape(&self, sh: &Shape, p: Point) -> bool {
        let b = sh.bounds();
        let mut margin = (sh.style.width as f32 / 2.0)
            .max(sh.style.font_size as f32 / 7.0)
            .max(sh.style.size as f32 * 3.0)
            + 2.0;
        if sh.kind == Tool::Line && sh.style.has_arrow() {
            margin = margin.max(12.0 + sh.style.width as f32 * 2.0);
        }
        let expanded = b.adjusted(-margin, -margin, margin, margin);
        if !expanded.contains(p) {
            return false;
        }
        match sh.kind {
            Tool::Mosaic => {
                if sh.style.mode == DrawMode::Brush {
                    dist_to_polyline(p, &sh.pts) <= (sh.style.width as f32 / 2.0).max(4.0)
                } else {
                    b.contains(p)
                }
            }
            Tool::Eraser => {
                if sh.style.mode == DrawMode::Brush {
                    dist_to_polyline(p, &sh.pts) <= (sh.style.width as f32 / 2.0).max(4.0)
                } else {
                    b.contains(p)
                }
            }
            Tool::Select => false,
            Tool::Text => {
                // Glyph ink plus a 参考工具-style border grab so drag-to-move
                // still hits when the pointer is on the box edge, not the ink.
                let pad = (sh.style.font_size.max(12) as f32) * 0.4 + 10.0;
                b.adjusted(-pad, -pad, pad, pad).contains(p)
                    || crate::render::shape_paints_at(sh, p)
            }
            _ => crate::render::shape_paints_at(sh, p),
        }
    }

    pub fn hit_handle_of_selected(&self, img_pt: Point) -> Option<Handle> {
        let i = self.selected?;
        let sh = self.shapes.get(i)?;
        if matches!(sh.kind, Tool::Pen | Tool::Eraser) || (matches!(sh.kind, Tool::Marker | Tool::Mosaic) && sh.style.mode == DrawMode::Brush) {
            return None;
        }
        let slop = HANDLE_SIZE / 2.0 + 2.0;
        if sh.kind == Tool::Text {
            return self.selected_handle_points().into_iter().find_map(|(h, c)| {
                ((img_pt.x - c.x).abs() <= slop && (img_pt.y - c.y).abs() <= slop).then_some(h)
            });
        }
        if let Some(pts) = line_endpoint_handles(sh) {
            for (h, c) in pts {
                if (img_pt.x - c.x).abs() <= slop && (img_pt.y - c.y).abs() <= slop {
                    return Some(h);
                }
            }
            return None;
        }
        let b = sh.bounds().adjusted(-2.0, -2.0, 2.0, 2.0);
        hit_handle(b, img_pt, slop)
    }

    pub fn selected_text_delete_rect(&self) -> Option<Rect> {
        let sh = self.shapes.get(self.selected?)?;
        if sh.kind != Tool::Text { return None; }
        let c = handle_points(sh.bounds().inflated(2.0)).into_iter()
            .find(|(h, _)| *h == Handle::Tr)?.1;
        Some(Rect::new(c.x - 7.0, c.y - 7.0, 14.0, 14.0))
    }

    pub fn delete_selected(&mut self) -> bool {
        if let Some(i) = self.selected {
            if i < self.shapes.len() {
                self.push_edit();
                self.shapes.remove(i);
                self.selected = None;
                self.edit_target = None;
                self.width_target = None;
                self.sel_mode = None;
                self.sel_text_resize = None;
                self.sel_pts0.clear();
                self.pending_text_click = None;
                if self.text_edit.as_ref().is_some_and(|ed| ed.existing == Some(i)) {
                    self.text_edit = None;
                } else if let Some(ed) = self.text_edit.as_mut() {
                    if let Some(index) = ed.existing.as_mut() {
                        if *index > i { *index -= 1; }
                    }
                }
                self.renumber();
                return true;
            }
        }
        false
    }

    pub fn is_selecting(&self) -> bool {
        self.sel_mode.is_some()
    }

    pub fn selecting_handle(&self) -> Option<Handle> {
        match self.sel_mode {
            Some(SelMode::Handle(h)) => Some(h),
            _ => None,
        }
    }

    pub fn is_editing_text(&self) -> bool {
        self.text_edit.is_some()
    }

    /// Text-tool press on committed text: the pointer is down, drag will move.
    pub fn is_dragging_text(&self) -> bool {
        self.pending_text_click.is_some()
    }

    pub fn is_swallowing_text_click(&self) -> bool {
        self.swallow_text_place
    }

    /// Shape / handle under the pointer for cursor mapping.
    pub fn hover_for_cursor(&self, img_pt: Point) -> (bool, Option<Handle>) {
        let handle = self.hit_handle_of_selected(img_pt);
        let shape = self.hit_test(img_pt).is_some();
        (shape, handle)
    }

    pub fn cursor_hint(&self, img_pt: Point) -> crate::cursor::CursorHint {
        let (hover_shape, hover_shape_handle) = self.hover_for_cursor(img_pt);
        crate::cursor::CursorHint {
            tool: self.tool,
            selecting: self.is_selecting(),
            selecting_handle: self.selecting_handle(),
            hover_shape,
            hover_shape_handle,
            hover_shape_delete: self.selected_text_delete_rect().is_some_and(|r| r.contains(img_pt)),
            editing_text: self.is_editing_text(),
            drawing: self.live.is_some(),
            ..Default::default()
        }
    }

    pub fn is_brush_live(&self) -> bool {
        self.live
            .as_ref()
            .map(|s| s.kind.is_brush(&s.style))
            .unwrap_or(false)
    }

    /// Left-button press in image coordinates. Returns true if the annotator consumed the event.
    pub fn press(&mut self, img_pt: Point, shift: bool) -> bool {
        if self.selected_text_delete_rect().is_some_and(|r| r.contains(img_pt)) {
            return self.delete_selected();
        }
        let tool = self.tool;
        if self.line_click_mode && self.live.as_ref().is_some_and(|s| s.kind == Tool::Line) {
            self.move_to(img_pt, shift);
            if let Some(sh) = self.live.as_mut() {
                sh.pts.push(img_pt);
            }
            return true;
        }

        // Select, or any drawing tool except eraser/mosaic/text, steals a hit
        // on an existing shape so the 8 handles appear. Text follows 参考工具:
        // click existing text to re-edit; click elsewhere to place a new box.
        let steal = tool == Some(Tool::Select)
            || (self.selected.and_then(|i| self.shapes.get(i)).is_some_and(|sh| sh.kind == Tool::Text)
                && self.hit_handle_of_selected(img_pt).is_some())
            || (tool != Some(Tool::Eraser)
                && tool != Some(Tool::Mosaic)
                && tool != Some(Tool::Text)
                && (self.hit_test(img_pt).is_some()
                    || self.hit_handle_of_selected(img_pt).is_some()));
        if steal {
            if let Some(h) = self.hit_handle_of_selected(img_pt) {
                if self.selected.is_some() {
                    self.sel_mode = Some(SelMode::Handle(h));
                    self.sel_origin = img_pt;
                    self.sel_dirty = false;
                    self.sel_pts0 = self.shapes[self.selected.unwrap()].pts.clone();
                    let sh = &self.shapes[self.selected.unwrap()];
                    self.sel_text_resize = (sh.kind == Tool::Text).then(|| (sh.bounds(), sh.style.font_size));
                    return true;
                }
            }
            let hit = self.hit_test(img_pt);
            self.selected = hit;
            self.edit_target = hit;
            if let Some(i) = hit {
                self.sel_mode = Some(SelMode::Move);
                self.sel_origin = img_pt;
                self.sel_dirty = false;
                self.sel_pts0 = self.shapes[i].pts.clone();
            }
            return true;
        }

        if self.selected.is_some() || self.edit_target.is_some() {
            self.selected = None;
            self.edit_target = None;
        }
        let Some(tool) = tool else {
            return false;
        };
        self.width_target = None;
        self.edit_target = None;

        let st = self.style(tool).clone();
        match tool {
            Tool::Text => {
                if self.swallow_text_place {
                    return true;
                }
                if let Some(r) = self.text_edit_rect() {
                    if r.contains(img_pt) {
                        self.set_caret_at_x(img_pt.x);
                        // Drag the box (参考工具: grab text/border to move) even
                        // while the caret is already in this editor.
                        if let Some(i) = self.text_edit.as_ref().and_then(|e| e.existing) {
                            if i < self.shapes.len() {
                                self.selected = Some(i);
                                self.sel_mode = Some(SelMode::Move);
                                self.sel_origin = img_pt;
                                self.sel_dirty = false;
                                self.sel_pts0 = self.shapes[i].pts.clone();
                                self.pending_text_click = Some(img_pt);
                            }
                        } else {
                            self.pending_text_click = Some(img_pt);
                            self.sel_origin = img_pt;
                            self.sel_dirty = false;
                        }
                        return true;
                    }
                }
                if let Some(i) = self.hit_test(img_pt) {
                    if self.shapes.get(i).map(|s| s.kind) == Some(Tool::Text) {
                        if self.text_edit.is_some() {
                            self.commit_text(false);
                        }
                        self.selected = Some(i);
                        self.edit_target = Some(i);
                        self.sel_mode = Some(SelMode::Move);
                        self.sel_origin = img_pt;
                        self.sel_dirty = false;
                        self.sel_pts0 = self.shapes[i].pts.clone();
                        self.pending_text_click = Some(img_pt);
                        return true;
                    }
                }
                if self.text_edit.is_some() {
                    self.commit_text(false);
                    self.selected = None;
                    self.swallow_text_place = true;
                    return true;
                }
                self.start_text(img_pt, None);
                true
            }
            Tool::Number => {
                let mut sh = Shape::new(Tool::Number, vec![img_pt], st);
                sh.number = self.next_number;
                self.next_number += 1;
                self.add(sh);
                true
            }
            Tool::Line if shift && self.shapes.last().map(|s| s.kind) == Some(Tool::Line) => {
                // Continue the last polyline. `add()` already snapshotted the pre-line
                // document, so popping here returns to that snapshot without a second undo push.
                if let Some(mut sh) = self.shapes.pop() {
                    sh.pts.push(img_pt);
                    self.live = Some(sh);
                }
                true
            }
            _ if tool.is_brush(&st) => {
                self.live = Some(Shape::new(tool, vec![img_pt], st));
                true
            }
            _ => {
                self.live = Some(Shape::new(tool, vec![img_pt, img_pt], st));
                if tool == Tool::Line {
                    self.line_press = Some(img_pt);
                }
                true
            }
        }
    }

    pub fn start_text(&mut self, pt: Point, existing: Option<usize>) {
        let (text, existing) = if let Some(idx) = existing {
            match self.shapes.get(idx) {
                Some(sh) if sh.kind == Tool::Text => (sh.text.clone(), Some(idx)),
                _ => (String::new(), None),
            }
        } else {
            (String::new(), None)
        };
        let caret = text.len();
        self.pending_text_click = None;
        self.selected = None;
        self.text_edit = Some(TextEdit {
            img_pt: pt,
            text,
            existing,
            caret,
            started: Instant::now(),
            blink: Instant::now(),
        });
    }

    /// Style used by the live editor (existing shape, else the text tool default).
    pub fn text_edit_style(&self) -> Style {
        let ed = match &self.text_edit {
            Some(ed) => ed,
            None => return self.style(Tool::Text).clone(),
        };
        if let Some(i) = ed.existing {
            if let Some(sh) = self.shapes.get(i) {
                return sh.style.clone();
            }
        }
        self.style(Tool::Text).clone()
    }

    pub fn caret_visible(&self) -> bool {
        let Some(ed) = &self.text_edit else {
            return false;
        };
        if ed.started.elapsed() < Duration::from_millis(400)
            || ed.blink.elapsed() < Duration::from_millis(400)
        {
            return true;
        }
        (ed.blink.elapsed().as_millis() / 530) % 2 == 0
    }

    pub fn caret_x(&self) -> f32 {
        let Some(ed) = &self.text_edit else {
            return 0.0;
        };
        let st = self.text_edit_style();
        let prefix = ed.text.get(..ed.caret.min(ed.text.len())).unwrap_or("");
        crate::font::measure_ex(
            prefix,
            st.font_size.max(8) as f32,
            &st.family,
            st.bold,
            st.italic,
        )
        .0
    }

    /// Wheel over a shape selects it (so width/font follow the hit) then adjusts.
    /// While typing, the wheel changes the live text size (参考工具).
    pub fn wheel_at(&mut self, img_pt: Point, angle_delta: i32) -> bool {
        if self.text_edit.is_none() && self.live.is_none() && self.selected.is_none() {
            if let Some(i) = self.hit_test(img_pt) {
                self.selected = Some(i);
                self.edit_target = Some(i);
                self.width_target = Some(i);
            }
        }
        self.wheel(angle_delta)
    }

    pub fn begin_edit_text(&mut self, idx: usize) {
        let Some(sh) = self.shapes.get(idx) else {
            return;
        };
        if sh.kind != Tool::Text {
            return;
        }
        let pt = sh.pts.first().copied().unwrap_or(Point::ZERO);
        // 参考工具 FAQ / changelog: re-edit while the text icon is selected.
        // Double-click from Select still stays on Select so the 8 handles remain.
        if self.tool != Some(Tool::Text) {
            self.set_tool(Some(Tool::Select));
        }
        self.selected = None;
        self.edit_target = Some(idx);
        self.start_text(pt, Some(idx));
    }

    /// Place the caret at image-space `x` (click-to-caret, 参考工具).
    pub fn set_caret_at_x(&mut self, x: f32) {
        let Some(ed) = self.text_edit.as_ref() else {
            return;
        };
        let origin = ed.img_pt.x;
        let text = ed.text.clone();
        let st = self.text_edit_style();
        let fs = st.font_size.max(8) as f32;
        let mut offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        offsets.push(text.len());
        let mut best = 0usize;
        let mut best_d = f32::MAX;
        for off in offsets {
            let prefix = &text[..off];
            let w = crate::font::measure_ex(prefix, fs, &st.family, st.bold, st.italic).0;
            let d = (origin + 2.0 + w - x).abs();
            if d < best_d {
                best_d = d;
                best = off;
            }
        }
        if let Some(ed) = self.text_edit.as_mut() {
            ed.caret = best;
            ed.blink = Instant::now();
        }
    }

    /// Editor box hugs the glyphs. An empty box stays wide enough that the
    /// caret and outline are visible on the first click (参考工具).
    pub fn text_edit_rect(&self) -> Option<Rect> {
        let ed = self.text_edit.as_ref()?;
        let st = self.text_edit_style();
        let fs = st.font_size.max(8) as f32;
        let (w, h) = crate::font::measure_ex(&ed.text, fs, &st.family, st.bold, st.italic);
        Some(Rect::new(
            ed.img_pt.x,
            ed.img_pt.y,
            (w + 8.0).max(fs * 0.7).max(16.0),
            h.max(fs) + 4.0,
        ))
    }

    fn clamp_caret(ed: &mut TextEdit) {
        if ed.caret > ed.text.len() {
            ed.caret = ed.text.len();
        }
        while ed.caret > 0 && !ed.text.is_char_boundary(ed.caret) {
            ed.caret -= 1;
        }
    }

    pub fn type_text(&mut self, ch: char) {
        self.insert_str(&ch.to_string());
    }

    /// IME commit / clipboard paste. Newlines stay (Shift+Enter); other control
    /// chars and the overlay dash artefacts are dropped.
    pub fn insert_str(&mut self, s: &str) {
        let Some(ed) = self.text_edit.as_mut() else {
            return;
        };
        Self::clamp_caret(ed);
        for ch in s.chars() {
            if ch == '\n' {
                ed.text.insert(ed.caret, ch);
                ed.caret += 1;
                continue;
            }
            if ch == '\u{8}' {
                if ed.caret == 0 {
                    continue;
                }
                let prev = ed.text[..ed.caret]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                let start = ed.caret - prev;
                ed.text.replace_range(start..ed.caret, "");
                ed.caret = start;
                continue;
            }
            if ch == '\u{7f}' {
                if ed.caret >= ed.text.len() {
                    continue;
                }
                let n = ed.text[ed.caret..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                ed.text.replace_range(ed.caret..ed.caret + n, "");
                continue;
            }
            if ch.is_control() {
                continue;
            }
            ed.text.insert(ed.caret, ch);
            ed.caret += ch.len_utf8();
        }
        ed.blink = Instant::now();
    }

    pub fn move_caret(&mut self, dir: i32) {
        let Some(ed) = self.text_edit.as_mut() else {
            return;
        };
        Self::clamp_caret(ed);
        match dir {
            -2 => ed.caret = 0,
            2 => ed.caret = ed.text.len(),
            -1 if ed.caret > 0 => {
                let prev = ed.text[..ed.caret]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                ed.caret -= prev;
            }
            1 if ed.caret < ed.text.len() => {
                let n = ed.text[ed.caret..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                ed.caret += n;
            }
            _ => {}
        }
        ed.blink = Instant::now();
    }

    /// 参考工具: right-click ends the current annotation (including text).
    pub fn right_click(&mut self) -> bool {
        if self.live.is_none() && self.text_edit.is_none()
            && (self.selected.is_some() || self.edit_target.is_some() || self.width_target.is_some())
        {
            self.lock_selected();
            self.selected = None;
            self.edit_target = None;
            self.width_target = None;
            self.wheel_target = None;
            self.wheel_delta = 0;
            self.sel_mode = None;
            self.sel_pts0.clear();
            return true;
        }
        if self.tool.is_none() && self.text_edit.is_none() && self.live.is_none() {
            return false;
        }
        self.finish();
        true
    }

    pub fn commit_text(&mut self, cancel: bool) {
        if cancel {
            self.text_edit = None;
            return;
        }
        self.finish();
    }

    pub fn move_to(&mut self, img_pt: Point, shift: bool) -> bool {
        if let (Some(mode), Some(idx)) = (self.sel_mode, self.selected) {
            if img_pt == self.sel_origin && !self.sel_dirty {
                return true;
            }
            if !self.sel_dirty {
                self.push_edit();
                self.sel_dirty = true;
            }
            match mode {
                SelMode::Move => {
                    let d = img_pt.sub(self.sel_origin);
                    self.shapes[idx].pts = self.sel_pts0.iter().map(|p| p.add(d)).collect();
                    if let Some(ed) = self.text_edit.as_mut() {
                        if ed.existing == Some(idx) {
                            if let Some(p) = self.shapes[idx].pts.first() {
                                ed.img_pt = *p;
                            }
                        }
                    }
                }
                SelMode::Handle(h) => self.resize_selected(idx, h, img_pt),
            }
            return true;
        }
        if self.pending_text_click.is_some() && self.text_edit.is_some() && self.selected.is_none()
        {
            if img_pt.manhattan(self.sel_origin) <= 0.5 {
                return true;
            }
            let d = img_pt.sub(self.sel_origin);
            if let Some(ed) = self.text_edit.as_mut() {
                ed.img_pt = ed.img_pt.add(d);
            }
            self.sel_origin = img_pt;
            self.sel_dirty = true;
            return true;
        }
        let Some(sh) = self.live.as_mut() else {
            return false;
        };
        if sh.kind.is_brush(&sh.style) {
            if sh
                .pts
                .last()
                .map(|p| p.manhattan(img_pt) >= 0.5)
                .unwrap_or(true)
            {
                sh.pts.push(img_pt);
            }
            return true;
        }
        let mut pt = img_pt;
        if shift && matches!(sh.kind, Tool::Rect | Tool::Ellipse) {
            pt = constrain_square(sh.pts[0], img_pt);
        }
        if sh.is_line_stroke()
            && sh.pts.len() >= 2
        {
            // Segment from the press origin to the pointer. Never fold into
            // Rect::normalized() corners (that is the selection-box diagonal).
            let n = sh.pts.len();
            sh.pts[n - 1] = pt;
            return true;
        }
        if let Some(last) = sh.pts.last_mut() {
            *last = pt;
        }
        true
    }

    fn resize_selected(&mut self, idx: usize, h: Handle, pt: Point) {
        let sh = &mut self.shapes[idx];
        if sh.kind == Tool::Text {
            let Some((bounds, font_size)) = self.sel_text_resize else { return; };
            // Use the press-time bounds and pointer offset so intermediate
            // motion events cannot compound the scale or cause a first-frame jump.
            let delta = pt.sub(self.sel_origin);
            let vx = if h.has_left() { -bounds.w } else if h.has_right() { bounds.w } else { 0.0 };
            let vy = if h.has_top() { -bounds.h } else if h.has_bottom() { bounds.h } else { 0.0 };
            let length_sq = vx * vx + vy * vy;
            let scale = if length_sq > 0.0 { 1.0 + (delta.x * vx + delta.y * vy) / length_sq } else { 1.0 };
            sh.style.font_size = (font_size as f32 * scale).round().clamp(8.0, 120.0) as i32;
            let resized = sh.bounds();
            // Keep the opposite corner/edge midpoint anchored using the actual
            // measured glyph bounds, including rounding and font-size limits.
            let x = if h.has_left() { bounds.right() - resized.w }
                else if h.has_right() { bounds.x }
                else { bounds.center().x - resized.w * 0.5 };
            let y = if h.has_top() { bounds.bottom() - resized.h }
                else if h.has_bottom() { bounds.y }
                else { bounds.center().y - resized.h * 0.5 };
            sh.pts = vec![Point::new(x, y)];
            if let Some(ed) = self.text_edit.as_mut() {
                if ed.existing == Some(idx) { ed.img_pt = sh.pts[0]; }
            }
            return;
        }
        if sh.kind == Tool::Number && self.sel_pts0.len() == 1 {
            let c = self.sel_pts0[0];
            let r = pt.manhattan(c).max(8.0);
            sh.style.width = (r * 2.0) as i32;
            sh.style.font_size = (r * 1.2) as i32;
            return;
        }
        if matches!(sh.kind, Tool::Pen | Tool::Marker | Tool::Line) && self.sel_pts0.len() > 2 {
            let xs = self.sel_pts0.iter().map(|p| p.x);
            let ys = self.sel_pts0.iter().map(|p| p.y);
            let x0 = xs.clone().fold(f32::MAX, f32::min);
            let x1 = xs.fold(f32::MIN, f32::max);
            let y0 = ys.clone().fold(f32::MAX, f32::min);
            let y1 = ys.fold(f32::MIN, f32::max);
            let b0 = Rect::from_xyxy(x0, y0, x1, y1);
            let c = b0.center();
            let dx0 = (b0.w.abs() / 2.0).max(1.0);
            let dy0 = (b0.h.abs() / 2.0).max(1.0);
            let sx = (pt.x - c.x).abs() / dx0;
            let sy = (pt.y - c.y).abs() / dy0;
            let s = ((sx + sy) / 2.0).max(0.05);
            sh.pts = self
                .sel_pts0
                .iter()
                .map(|p| c.add(p.sub(c).scale(s)))
                .collect();
            return;
        }
        // Two-point line/arrow: drag moves that endpoint. Do not rewrite as
        // the AABB diagonal (top-left → bottom-right of a selection box).
        if sh.is_line_stroke()
            && self.sel_pts0.len() >= 2
        {
            let a0 = self.sel_pts0[0];
            let b0 = *self.sel_pts0.last().unwrap();
            if h == Handle::Tl {
                sh.pts[0] = pt;
                let n = sh.pts.len();
                sh.pts[n - 1] = b0;
            } else {
                sh.pts[0] = a0;
                let n = sh.pts.len();
                sh.pts[n - 1] = pt;
            }
            return;
        }
        let mut b = if self.sel_pts0.len() >= 2 {
            Rect::from_points(self.sel_pts0[0], *self.sel_pts0.last().unwrap()).normalized()
        } else {
            sh.bounds()
        };
        if h.has_left() {
            b.w += b.x - pt.x;
            b.x = pt.x;
        }
        if h.has_right() {
            b.w = pt.x - b.x;
        }
        if h.has_top() {
            b.h += b.y - pt.y;
            b.y = pt.y;
        }
        if h.has_bottom() {
            b.h = pt.y - b.y;
        }
        let b = b.normalized();
        sh.pts = vec![b.top_left(), b.bottom_right()];
    }

    pub fn release(&mut self, img_pt: Point) -> bool {
        if self.swallow_text_place {
            self.swallow_text_place = false;
            self.sel_mode = None;
            return true;
        }
        if self.pending_text_click.take().is_some() {
            // Fast drags may skip CursorMoved; still treat a moved pointer as move.
            let dragged = self.sel_dirty || img_pt.manhattan(self.sel_origin) > 5.0;
            let idx = self.selected;
            let editing = self.text_edit.is_some();
            if dragged {
                if !self.sel_dirty {
                    let d = img_pt.sub(self.sel_origin);
                    if let Some(i) = idx {
                        if i < self.shapes.len() && !self.sel_pts0.is_empty() {
                            self.push_edit();
                            self.shapes[i].pts = self.sel_pts0.iter().map(|p| p.add(d)).collect();
                            if let Some(ed) = self.text_edit.as_mut() {
                                if ed.existing == Some(i) {
                                    if let Some(p) = self.shapes[i].pts.first() {
                                        ed.img_pt = *p;
                                    }
                                }
                            }
                        }
                    } else if let Some(ed) = self.text_edit.as_mut() {
                        ed.img_pt = ed.img_pt.add(d);
                    }
                }
                self.sel_mode = None;
                self.sel_pts0.clear();
                self.selected = None;
                return true;
            }
            self.sel_mode = None;
            self.sel_pts0.clear();
            self.selected = None;
            if editing {
                // Caret was already placed on press.
                return true;
            }
            if let Some(i) = idx {
                if self.shapes.get(i).map(|s| s.kind) == Some(Tool::Text) {
                    self.begin_edit_text(i);
                    self.set_caret_at_x(img_pt.x);
                }
            }
            return true;
        }
        if self.sel_mode.is_some() || self.tool == Some(Tool::Select) {
            if matches!(self.sel_mode, Some(SelMode::Handle(_))) && self.sel_text_resize.is_some() {
                self.move_to(img_pt, false);
            }
            self.sel_mode = None;
            self.sel_text_resize = None;
            self.sel_pts0.clear();
            return true;
        }
        if self.live.is_some() {
            if self.live.as_ref().is_some_and(|s| matches!(s.kind, Tool::Marker | Tool::Mosaic | Tool::Eraser)) {
                self.move_to(img_pt, false);
            }
            if self.live.as_ref().is_some_and(|s| s.kind == Tool::Line) {
                if self.line_click_mode {
                    return true;
                }
                if let Some(origin) = self.line_press.take() {
                    if img_pt.manhattan(origin) <= 5.0 {
                        self.line_click_mode = true;
                        if let Some(sh) = self.live.as_mut() {
                            sh.pts = vec![origin, origin];
                        }
                        return true;
                    }
                    self.move_to(img_pt, false);
                }
            }
            self.finish();
            return true;
        }
        false
    }

    pub fn selected_handle_points(&self) -> Vec<(Handle, Point)> {
        if let Some(i) = self.selected {
            if let Some(sh) = self.shapes.get(i) {
                if let Some(pts) = line_endpoint_handles(sh) {
                    return pts.to_vec();
                }
                return handle_points(sh.bounds().adjusted(-2.0, -2.0, 2.0, 2.0)).into_iter()
                    .filter(|(h, _)| sh.kind != Tool::Text || matches!(h, Handle::Tl | Handle::Bl | Handle::Br))
                    .collect();
            }
        }
        Vec::new()
    }

    pub fn palette() -> [Rgba; 20] {
        palette_colors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automation_identity_survives_editor_history() {
        let mut editor = Annotator::new();
        let mut shape = Shape::new(Tool::Rect, vec![Point::new(5.0, 5.0), Point::new(30.0, 30.0)], Style::default());
        shape.automation_id = Some(42);
        editor.replace_automation_shapes(vec![shape.clone()]);
        assert_eq!(editor.shapes[0].automation_id, Some(42));
        assert!(editor.undo());
        assert!(editor.shapes.is_empty());
        assert!(editor.redo());
        assert_eq!(editor.shapes[0].automation_id, Some(42));
        assert!(!editor.automation_busy());
        editor.live = Some(shape);
        assert!(editor.automation_busy());
    }


    fn rect_shape() -> Shape {
        Shape::new(
            Tool::Rect,
            vec![Point::new(40.0, 40.0), Point::new(200.0, 160.0)],
            Style::default(),
        )
    }

    #[test]
    fn hollow_hit() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        assert!(a.hit_test(Point::new(100.0, 100.0)).is_none());
        assert_eq!(a.hit_test(Point::new(40.0, 80.0)), Some(0));
        a.shapes[0].style.filled = true;
        assert_eq!(a.hit_test(Point::new(100.0, 100.0)), Some(0));
    }

    #[test]
    fn deselected_shape_cannot_be_selected_again() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        a.shapes[0].style.filled = true;
        assert_eq!(a.selected, Some(0));
        assert!(a.right_click());
        assert!(a.selected.is_none());
        assert!(a.hit_test(Point::new(100.0, 100.0)).is_none());

        let mut b = Annotator::new();
        b.add(rect_shape());
        b.set_tool(Some(Tool::Rect));
        assert!(b.hit_test(Point::new(40.0, 80.0)).is_none());
    }

    #[test]
    fn drag_existing_with_drawing_tool_selects_and_moves() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        assert!(a.press(Point::new(40.0, 80.0), false));
        assert_eq!(a.selected, Some(0));
        a.move_to(Point::new(70.0, 100.0), false);
        a.release(Point::new(70.0, 100.0));
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].pts[0], Point::new(70.0, 60.0));
    }

    #[test]
    fn drag_empty_with_drawing_tool_starts_new_shape() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        assert!(a.press(Point::new(10.0, 10.0), false));
        a.move_to(Point::new(25.0, 25.0), false);
        a.release(Point::new(25.0, 25.0));
        assert_eq!(a.shapes.len(), 2);
        assert_eq!(a.shapes[1].kind, Tool::Rect);
    }

    #[test]
    fn width_after_release_and_undo() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        a.selected = Some(0);
        a.adjust_width(2);
        assert_eq!(a.shapes[0].style.width, 5);
        assert_eq!(a.style(Tool::Rect).width, 5);
        a.undo();
        assert_eq!(a.shapes[0].style.width, 3);
        a.undo();
        assert!(a.shapes.is_empty());
    }

    #[test]
    fn drag_during_initial_selection() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        a.set_tool(Some(Tool::Select));
        assert!(a.press(Point::new(40.0, 80.0), false));
        assert_eq!(a.selected, Some(0));
        a.move_to(Point::new(60.0, 80.0), false);
        a.release(Point::new(60.0, 80.0));
        assert_eq!(a.shapes[0].pts[0], Point::new(60.0, 40.0));
    }

    #[test]
    fn mosaic_does_not_drag_existing() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        a.set_tool(Some(Tool::Mosaic));
        a.press(Point::new(40.0, 80.0), false);
        assert!(a.live.as_ref().unwrap().kind == Tool::Mosaic);
        assert!(a.sel_mode.is_none());
        a.finish();
    }

    #[test]
    fn style_edit_undo() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        a.selected = Some(0);
        a.set_style_value(Tool::Rect, |s| s.filled = true);
        assert!(a.shapes[0].style.filled);
        a.undo();
        assert!(!a.shapes[0].style.filled);
    }

    #[test]
    fn eraser_hides_hit() {
        let mut a = Annotator::new();
        let mut r = rect_shape();
        r.style.filled = true;
        a.add(r);
        a.add(Shape::new(
            Tool::Eraser,
            vec![Point::new(40.0, 80.0), Point::new(200.0, 80.0)],
            Style {
                width: 16,
                mode: DrawMode::Brush,
                ..Style::default()
            },
        ));
        assert!(a.hit_test(Point::new(100.0, 80.0)).is_none());
    }

    #[test]
    fn numbered_markers_renumber_on_undo() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Number));
        a.press(Point::new(10.0, 10.0), false);
        // Place outside the first marker's hit region; clicking it selects it.
        a.press(Point::new(100.0, 10.0), false);
        assert_eq!(a.shapes.len(), 2);
        assert_eq!(a.shapes[0].number, 1);
        assert_eq!(a.shapes[1].number, 2);
        a.undo();
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].number, 1);
        assert_eq!(a.next_number, 2);
    }

    #[test]
    fn draw_rect_shift_square() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(0.0, 0.0), false);
        a.move_to(Point::new(40.0, 10.0), true);
        a.release(Point::new(40.0, 10.0));
        let b = a.shapes[0].bounds();
        assert!((b.w - b.h).abs() < 0.01);
    }

    #[test]
    fn undo_stack_limit() {
        let mut u = UndoStack::default();
        for i in 0..60 {
            u.push(vec![Shape::new(
                Tool::Rect,
                vec![Point::new(i as f32, 0.0)],
                Style::default(),
            )]);
        }
        assert_eq!(u.undo_len(), UndoStack::LIMIT);
    }

    #[test]
    fn text_commit() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.type_text('H');
        a.type_text('i');
        a.commit_text(false);
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].text, "Hi");
        assert!(a.text_edit.is_none());
        assert!(a.selected.is_none(), "committed text has no handles");
        assert!(
            a.option_target().is_none(),
            "no leftover width_target chrome"
        );
        assert!(a.selected_handle_points().is_empty());
    }

    #[test]
    fn option_target_prefers_selected_then_edit_then_width() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        a.add(Shape::new(
            Tool::Ellipse,
            vec![Point::new(10.0, 10.0), Point::new(30.0, 30.0)],
            Style::default(),
        ));
        a.selected = None;
        a.edit_target = None;
        assert_eq!(a.option_target(), Some(1)); // last add → width_target
        a.edit_target = Some(0);
        assert_eq!(a.option_target(), Some(0));
        a.selected = Some(1);
        assert_eq!(a.option_target(), Some(1));
        a.live = Some(Shape::new(
            Tool::Pen,
            vec![Point::new(1.0, 1.0)],
            Style::default(),
        ));
        assert!(a.option_target().is_none());
        a.adjust_width(2);
        assert_eq!(a.live.as_ref().unwrap().style.width, 5);
        assert_eq!(a.option_width(), 5);
    }

    #[test]
    fn option_width_text_uses_font_size_mosaic_uses_brush_width() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        assert_eq!(a.option_width(), a.style(Tool::Text).font_size);
        a.set_tool(Some(Tool::Mosaic));
        assert_eq!(a.option_width(), a.style(Tool::Mosaic).width);
        a.set_tool(Some(Tool::Rect));
        assert_eq!(a.option_width(), a.style(Tool::Rect).width);
    }

    #[test]
    fn effect_strength_and_brush_width_are_independent() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Mosaic));
        let width = a.option_width();
        a.set_style_value(Tool::Mosaic, |s| s.size = 20);
        assert_eq!(a.option_width(), width);
        a.set_option_width(24);
        assert_eq!(a.style(Tool::Mosaic).width, 24);
        assert_eq!(a.style(Tool::Mosaic).size, 20);
        a.wheel(120);
        assert_eq!(a.style(Tool::Mosaic).width, 25);
        assert_eq!(a.style(Tool::Mosaic).size, 20);
        a.set_draw_mode(Tool::Mosaic, DrawMode::Rect);
        a.press(Point::new(10.0, 10.0), false);
        a.release(Point::new(50.0, 40.0));
        let pts = a.shapes[0].pts.clone();
        a.set_style_value(Tool::Mosaic, |s| s.size = 8);
        assert_eq!(a.shapes[0].style.size, 8);
        assert_eq!(a.shapes[0].style.width, 25);
        assert_eq!(a.shapes[0].pts, pts);
        a.undo();
        assert_eq!(a.shapes[0].style.size, 20);
    }

    #[test]
    fn option_width_follows_selected_not_tool_default() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        a.selected = Some(0);
        a.shapes[0].style.width = 9;
        assert_eq!(a.style(Tool::Rect).width, 3);
        assert_eq!(a.option_width(), 9);
        a.adjust_width(1);
        assert_eq!(a.option_width(), 10);
    }

    #[test]
    fn wheel_accumulates_120_and_merges_undo() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        a.selected = Some(0);
        assert!(!a.wheel(60));
        assert_eq!(a.shapes[0].style.width, 3);
        assert!(a.wheel(60));
        assert_eq!(a.shapes[0].style.width, 4);
        assert!(a.wheel(120));
        assert_eq!(a.shapes[0].style.width, 5);
        // same target within 0.5s → one undo snapshot
        assert_eq!(a.undo_len(), 2); // add + first merged wheel
        a.undo();
        assert_eq!(a.shapes[0].style.width, 3);
    }

    #[test]
    fn wheel_without_selection_uses_width_target() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.add(rect_shape());
        a.selected = None;
        a.edit_target = None;
        assert_eq!(a.width_target, Some(0));
        assert!(a.wheel(120));
        assert_eq!(a.shapes[0].style.width, 4);
    }

    #[test]
    fn text_editor_rect_and_cancel() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(20.0, 30.0), false);
        let empty = a.text_edit_rect().expect("empty editor");
        assert!(
            empty.w >= 16.0 && empty.h >= 20.0,
            "first click must show a visible box, {empty:?}"
        );
        a.type_text('A');
        let r = a.text_edit_rect().expect("editor");
        assert!(r.w < 48.0, "editor must hug glyphs, w={}", r.w);
        assert!(r.w > 4.0);
        a.commit_text(true);
        assert!(a.shapes.is_empty());
        assert!(a.text_edit.is_none());
    }

    #[test]
    fn picking_arrow_does_not_convert_last_line() {
        let mut a = Annotator::new();
        a.set_style_default(Tool::Line, |s| s.arrow = false);
        a.set_tool(Some(Tool::Line));
        a.press(Point::new(0.0, 0.0), false);
        a.move_to(Point::new(40.0, 0.0), false);
        a.release(Point::new(40.0, 0.0));
        assert_eq!(a.shapes.len(), 1);
        assert!(!a.shapes[0].style.arrow);
        a.set_tool(Some(Tool::Line));
        a.set_style_default(Tool::Line, |s| s.arrow = true);
        assert!(!a.shapes[0].style.arrow);
        assert!(a.style(Tool::Line).arrow);
    }

    #[test]
    fn switching_draw_tool_locks_previous_shape() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        a.set_style_default(Tool::Line, |s| s.arrow = false);
        a.press(Point::new(0.0, 0.0), false);
        a.move_to(Point::new(40.0, 0.0), false);
        a.release(Point::new(40.0, 0.0));
        a.set_tool(Some(Tool::Rect));
        assert!(a.press(Point::new(20.0, 0.0), false));
        assert_eq!(a.selected, None);
        assert!(a.text_edit.is_none());
        assert_eq!(a.live.as_ref().map(|shape| shape.kind), Some(Tool::Rect));
    }

    #[test]
    fn text_tool_click_existing_reedits() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("Hi");
        a.commit_text(false);
        assert_eq!(a.shapes[0].text, "Hi");
        a.set_tool(Some(Tool::Text));
        assert!(a.press(Point::new(10.0, 10.0), false));
        a.release(Point::new(10.0, 10.0));
        let ed = a.text_edit.as_ref().expect("re-edit");
        assert_eq!(ed.text, "Hi");
        assert_eq!(ed.existing, Some(0));
        assert_eq!(a.tool, Some(Tool::Text));
    }

    #[test]
    fn text_default_style_matches_reference_tool() {
        let a = Annotator::new();
        let st = a.style(Tool::Text);
        assert!(!st.outline, "参考工具 default text is not outlined");
        assert_eq!(st.font_size, 22);
        assert_eq!(st.color, Rgba::rgb(30, 30, 30));
    }

    #[test]
    fn cjk_insert_does_not_dismiss_editor() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("你好PinShot");
        let ed = a.text_edit.as_ref().expect("editor stays");
        assert_eq!(ed.text, "你好PinShot");
        assert!(a.caret_visible());
    }

    #[test]
    fn text_tool_right_click_commits() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("你好");
        assert!(a.right_click());
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].text, "你好");
        assert!(a.text_edit.is_none());
    }

    #[test]
    fn text_caret_and_backspace_at_middle() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("abc");
        a.move_caret(-1);
        a.type_text('\u{8}');
        a.commit_text(false);
        assert_eq!(a.shapes[0].text, "ac");
    }

    #[test]
    fn text_wheel_while_editing_changes_font_size() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        let before = a.style(Tool::Text).font_size;
        assert!(a.wheel_at(Point::new(10.0, 10.0), 120));
        assert_eq!(a.style(Tool::Text).font_size, before + 2);
        a.insert_str("A");
        a.commit_text(false);
        assert_eq!(a.shapes[0].style.font_size, before + 2);
    }

    #[test]
    fn new_tool_on_empty_starts_draw() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        a.press(Point::new(0.0, 0.0), false);
        a.move_to(Point::new(30.0, 0.0), false);
        a.release(Point::new(30.0, 0.0));
        a.set_tool(Some(Tool::Pen));
        assert!(a.press(Point::new(15.0, 20.0), false));
        assert!(a.live.as_ref().map(|s| s.kind) == Some(Tool::Pen));
        assert_eq!(a.shapes.len(), 1);
    }

    #[test]
    fn text_click_outside_commits_without_new_box() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        assert!(a.press(Point::new(20.0, 30.0), false));
        a.insert_str("Hi");
        assert!(a.press(Point::new(200.0, 180.0), false));
        assert!(a.text_edit.is_none());
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].text, "Hi");
        assert!(a.selected.is_none());
        assert!(a.option_target().is_none());
        a.release(Point::new(200.0, 180.0));
        assert!(a.press(Point::new(200.0, 180.0), false));
        assert!(a.text_edit.is_some());
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(
            a.text_edit.as_ref().unwrap().img_pt,
            Point::new(200.0, 180.0)
        );
    }

    #[test]
    fn text_empty_click_outside_discards_and_next_click_places() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        assert!(a.press(Point::new(20.0, 30.0), false));
        assert!(a.text_edit.is_some());
        assert!(a.press(Point::new(40.0, 50.0), false));
        assert!(a.text_edit.is_none());
        assert!(a.shapes.is_empty());
        a.release(Point::new(40.0, 50.0));
        assert!(a.press(Point::new(40.0, 50.0), false));
        let ed = a.text_edit.as_ref().expect("new box");
        assert!(ed.text.is_empty());
        assert_eq!(ed.img_pt, Point::new(40.0, 50.0));
    }

    #[test]
    fn text_reedit_hides_committed_and_caret_at_click() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("Hello");
        a.commit_text(false);
        assert_eq!(a.visible_shapes().len(), 1);
        a.press(Point::new(9.0, 10.0), false);
        a.release(Point::new(9.0, 10.0));
        let ed = a.text_edit.as_ref().expect("re-edit");
        assert_eq!(ed.text, "Hello");
        assert_eq!(ed.existing, Some(0));
        assert_eq!(ed.caret, 0, "click at origin places caret at start");
        assert!(a.visible_shapes().is_empty());
        assert!(a.selected.is_none());
    }

    #[test]
    fn text_drag_moves_without_new_box() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("Hi");
        a.commit_text(false);
        assert!(a.press(Point::new(10.0, 10.0), false));
        a.move_to(Point::new(40.0, 28.0), false);
        a.release(Point::new(40.0, 28.0));
        assert!(a.text_edit.is_none());
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].text, "Hi");
        assert!((a.shapes[0].pts[0].x - 38.0).abs() < 0.5);
    }

    #[test]
    fn text_drag_without_move_events_still_moves() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("Hi");
        a.commit_text(false);
        assert!(a.press(Point::new(10.0, 10.0), false));
        a.release(Point::new(40.0, 28.0));
        assert!(a.text_edit.is_none());
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].text, "Hi");
        assert!((a.shapes[0].pts[0].x - 38.0).abs() < 0.5);
    }

    #[test]
    fn text_drag_while_editing_moves_editor() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        a.insert_str("Hi");
        a.commit_text(false);
        a.press(Point::new(10.0, 10.0), false);
        a.release(Point::new(10.0, 10.0));
        assert!(a.text_edit.is_some());
        let before = a.text_edit.as_ref().unwrap().img_pt;
        a.press(Point::new(12.0, 12.0), false);
        a.move_to(Point::new(42.0, 32.0), false);
        a.release(Point::new(42.0, 32.0));
        assert!(a.text_edit.is_some(), "stay in edit after drag");
        assert_eq!(a.shapes.len(), 1);
        let ed = a.text_edit.as_ref().unwrap();
        assert!((ed.img_pt.x - (before.x + 30.0)).abs() < 0.5);
        assert_eq!(a.shapes[0].pts[0], ed.img_pt);
        assert!(a.selected.is_none());
    }

    #[test]
    fn caret_shows_immediately() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        a.press(Point::new(8.0, 8.0), false);
        assert!(a.caret_visible());
    }

    #[test]
    fn wheel_does_not_reselect_locked_shape() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(10.0, 10.0), false);
        a.move_to(Point::new(40.0, 40.0), false);
        a.release(Point::new(40.0, 40.0));
        let width = a.shapes[0].style.width;
        assert!(a.right_click());
        assert!(a.wheel_at(Point::new(10.0, 10.0), 120));
        assert_eq!(a.selected, None);
        assert_eq!(a.shapes[0].style.width, width);
    }

    /// Overlay `handle` MouseInput / CursorMoved / MouseInput — not QueryPointer recover.
    #[test]
    fn event_path_draws_rect_ellipse_arrow() {
        for (tool, arrow) in [
            (Tool::Rect, false),
            (Tool::Ellipse, false),
            (Tool::Line, true),
        ] {
            let mut a = Annotator::new();
            a.set_tool(Some(tool));
            if arrow {
                a.set_style_default(Tool::Line, |s| s.arrow = true);
            }
            assert!(a.press(Point::new(20.0, 20.0), false), "{tool:?} press");
            assert!(a.live.is_some(), "{tool:?} live after press");
            assert!(a.move_to(Point::new(90.0, 70.0), false), "{tool:?} move");
            assert!(a.release(Point::new(90.0, 70.0)), "{tool:?} release");
            assert_eq!(a.shapes.len(), 1, "{tool:?}");
            assert_eq!(a.shapes[0].kind, tool);
            if arrow {
                assert!(a.shapes[0].style.arrow, "arrow flag");
            }
            let b = a.shapes[0].bounds();
            assert!(b.w >= 60.0 && b.h >= 40.0, "{tool:?} bounds {b:?}");
            assert_eq!(a.selected, Some(0), "{tool:?} just-drawn is selected");
            assert_eq!(a.option_target(), Some(0));
        }
    }

    #[test]
    fn line_drag_is_press_to_cursor_not_rect_diagonal() {
        for arrow in [false, true] {
            let mut a = Annotator::new();
            a.set_tool(Some(Tool::Line));
            if arrow {
                a.set_style_default(Tool::Line, |s| s.arrow = true);
            }
            let start = Point::new(80.0, 70.0);
            let end = Point::new(20.0, 10.0);
            assert!(a.press(start, false));
            assert_eq!(a.live.as_ref().unwrap().pts[0], start);
            assert!(a.move_to(end, false));
            let live = a.live.as_ref().unwrap();
            assert_eq!(live.pts[0], start, "press stays the start point");
            assert_eq!(live.pts[1], end, "cursor is the end, not AABB BR");
            assert_ne!(
                live.pts,
                vec![Point::new(20.0, 10.0), Point::new(80.0, 70.0)],
                "must not be the selection-box diagonal"
            );
            assert!(a.release(end));
            assert_eq!(a.shapes[0].pts, vec![start, end]);
            a.undo();
            assert!(a.shapes.is_empty());
        }
    }

    #[test]
    fn line_handle_moves_endpoint_not_box_corner() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        a.press(Point::new(80.0, 70.0), false);
        a.move_to(Point::new(20.0, 10.0), false);
        a.release(Point::new(20.0, 10.0));
        assert_eq!(a.selected, Some(0));
        let end = Point::new(20.0, 10.0);
        assert_eq!(a.hit_handle_of_selected(end), Some(Handle::Br));
        assert!(a.press(end, false));
        a.move_to(Point::new(30.0, 15.0), false);
        a.release(Point::new(30.0, 15.0));
        assert_eq!(a.shapes[0].pts[0], Point::new(80.0, 70.0));
        assert_eq!(a.shapes[0].pts[1], Point::new(30.0, 15.0));
        let hs = a.selected_handle_points();
        assert_eq!(hs.len(), 2);
    }

    #[test]
    fn style_after_draw_edits_just_drawn_and_selects_it() {
        let mut a = Annotator::new();
        a.set_style_default(Tool::Rect, |s| s.line_style = LineStyleKind::Dot);
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(10.0, 10.0), false);
        a.move_to(Point::new(80.0, 60.0), false);
        a.release(Point::new(80.0, 60.0));
        assert_eq!(a.selected, Some(0));
        assert_eq!(a.edit_target, Some(0));
        assert_eq!(a.width_target, Some(0));
        assert_eq!(a.option_target(), Some(0));
        assert!(!a.selected_handle_points().is_empty());
        assert_eq!(a.shapes[0].style.line_style, LineStyleKind::Dot);
        a.set_style_value(Tool::Rect, |s| s.line_style = LineStyleKind::Solid);
        assert_eq!(a.shapes[0].style.line_style, LineStyleKind::Solid);
        a.set_style_value(Tool::Rect, |s| s.color = Rgba::BLUE);
        assert_eq!(a.shapes[0].style.color, Rgba::BLUE);
        assert_eq!(a.style(Tool::Rect).line_style, LineStyleKind::Solid);
        assert_eq!(a.style(Tool::Rect).color, Rgba::BLUE);
    }

    #[test]
    fn translate_all_keeps_undo_in_selection_space() {
        let mut a = Annotator::new();
        a.add(rect_shape());
        a.add(Shape::new(
            Tool::Ellipse,
            vec![Point::new(10.0, 12.0), Point::new(30.0, 40.0)],
            Style::default(),
        ));
        a.translate_all(8.0, -4.0);
        assert_eq!(a.shapes[0].pts[0], Point::new(48.0, 36.0));
        assert_eq!(a.shapes[1].pts[0], Point::new(18.0, 8.0));
        a.undo();
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].pts[0], Point::new(48.0, 36.0));
        a.redo();
        assert_eq!(a.shapes[1].pts[0], Point::new(18.0, 8.0));
    }

    #[test]
    fn empty_recover_release_does_not_block_event_path_stroke() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(10.0, 10.0), false);
        a.release(Point::new(10.0, 10.0));
        assert!(a.shapes.is_empty());
        assert!(a.live.is_none());
        a.press(Point::new(10.0, 10.0), false);
        a.move_to(Point::new(50.0, 40.0), false);
        a.release(Point::new(50.0, 40.0));
        assert_eq!(a.shapes.len(), 1);
        let b = a.shapes[0].bounds();
        assert!(b.w >= 30.0 && b.h >= 20.0);
    }

    #[test]
    fn selected_rect_converts_to_ellipse_and_undo_restores() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(10.0, 10.0), false);
        a.move_to(Point::new(80.0, 50.0), false);
        a.release(Point::new(80.0, 50.0));
        assert_eq!(a.shapes[0].kind, Tool::Rect);
        assert_eq!(a.selected, Some(0));
        a.convert_rect_ellipse(Tool::Ellipse);
        assert_eq!(a.shapes[0].kind, Tool::Ellipse);
        assert_eq!(a.tool, Some(Tool::Ellipse));
        assert_eq!(a.selected, Some(0));
        a.convert_rect_ellipse(Tool::Rect);
        assert_eq!(a.shapes[0].kind, Tool::Rect);
        a.undo();
        assert_eq!(a.shapes[0].kind, Tool::Ellipse);
    }

    #[test]
    fn live_rect_converts_to_ellipse_before_commit() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Rect));
        a.press(Point::new(4.0, 4.0), false);
        a.move_to(Point::new(40.0, 30.0), false);
        assert_eq!(a.live.as_ref().map(|s| s.kind), Some(Tool::Rect));
        a.convert_rect_ellipse(Tool::Ellipse);
        assert_eq!(a.live.as_ref().map(|s| s.kind), Some(Tool::Ellipse));
        assert!(a.shapes.is_empty());
        a.release(Point::new(40.0, 30.0));
        assert_eq!(a.shapes[0].kind, Tool::Ellipse);
    }

    #[test]
    fn selected_line_converts_to_arrow_and_undo_restores() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        a.press(Point::new(10.0, 10.0), false);
        a.move_to(Point::new(80.0, 10.0), false);
        a.release(Point::new(80.0, 10.0));
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::None);
        assert_eq!(a.selected, Some(0));
        a.convert_line_arrow(ArrowKind::Thin);
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::Thin);
        assert_eq!(a.tool, Some(Tool::Line));
        assert_eq!(a.selected, Some(0));
        a.convert_line_arrow(ArrowKind::Hollow);
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::Hollow);
        a.convert_line_arrow(ArrowKind::None);
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::None);
        a.undo();
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::Hollow);
    }

    #[test]
    fn live_line_converts_to_arrow_before_commit() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        a.press(Point::new(4.0, 4.0), false);
        a.move_to(Point::new(40.0, 4.0), false);
        assert_eq!(
            a.live.as_ref().map(|s| s.style.arrow_kind()),
            Some(ArrowKind::None)
        );
        a.convert_line_arrow(ArrowKind::Thick);
        assert_eq!(
            a.live.as_ref().map(|s| s.style.arrow_kind()),
            Some(ArrowKind::Thick)
        );
        assert!(a.shapes.is_empty());
        a.release(Point::new(40.0, 4.0));
        assert_eq!(a.shapes[0].style.arrow_kind(), ArrowKind::Thick);
    }

    #[test]
    fn click_polyline_commits_only_on_right_click_and_undoes_as_one_shape() {
        for kind in [ArrowKind::None, ArrowKind::Thin, ArrowKind::Thick, ArrowKind::Hollow] {
            let mut a = Annotator::new();
            a.set_tool(Some(Tool::Line));
            a.style_mut(Tool::Line).set_arrow_kind(kind);
            let start = Point::new(10.0, 10.0);
            let bend = Point::new(80.0, 10.0);
            let end = Point::new(80.0, 70.0);
            a.press(start, false);
            a.release(start);
            a.move_to(bend, false);
            a.press(bend, false);
            a.release(bend);
            assert!(a.shapes.is_empty());
            a.move_to(end, false);
            assert!(a.right_click());
            assert_eq!(a.shapes.len(), 1);
            assert_eq!(a.shapes[0].pts, vec![start, bend, end]);
            assert_eq!(a.shapes[0].style.arrow_kind(), kind);
            assert!(a.live.is_none());
            assert!(a.undo());
            assert!(a.shapes.is_empty());
        }
    }

    #[test]
    fn dragged_line_uses_release_endpoint_without_motion_event() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Line));
        let start = Point::new(10.0, 10.0);
        let end = Point::new(90.0, 70.0);
        a.press(start, false);
        a.release(end);
        assert_eq!(a.shapes[0].pts, vec![start, end]);
        assert!(a.live.is_none());
    }

    #[test]
    fn marker_brush_follows_path_while_rect_keeps_endpoints() {
        for mode in [DrawMode::Brush, DrawMode::Rect] {
            let mut a = Annotator::new();
            a.set_tool(Some(Tool::Marker));
            a.style_mut(Tool::Marker).mode = mode;
            let start = Point::new(10.0, 10.0);
            let end = Point::new(90.0, 70.0);
            a.press(start, false);
            let bend = Point::new(50.0, 5.0);
            a.move_to(bend, false);
            a.release(end);
            let expected = if mode == DrawMode::Brush { vec![start, bend, end] } else { vec![start, end] };
            assert_eq!(a.shapes[0].pts, expected);
            assert_eq!(a.shapes[0].style.mode, mode);
            if mode == DrawMode::Brush {
                assert_eq!(a.selected, None);
                assert_eq!(a.edit_target, None);
                assert_eq!(a.width_target, None);
                assert_eq!(a.hit_test(bend), None);
                a.set_tool(Some(Tool::Select));
                a.press(bend, false);
                a.move_to(Point::new(60.0, 15.0), false);
                a.release(Point::new(60.0, 15.0));
                assert_eq!(a.selected, None);
                assert_eq!(a.shapes[0].pts, expected);
            } else {
                assert_eq!(a.selected, Some(0));
                assert_eq!(a.hit_test(Point::new(50.0, 40.0)), Some(0));
            }
        }
    }

    #[test]
    fn eraser_modes_are_unselectable_and_preserve_freehand_or_rect_geometry() {
        for mode in [DrawMode::Brush, DrawMode::Rect] {
            let mut a = Annotator::new();
            a.set_draw_mode(Tool::Eraser, mode);
            let start = Point::new(10.0, 10.0);
            let bend = Point::new(70.0, 80.0);
            let end = Point::new(30.0, 20.0);
            a.press(start, false);
            a.move_to(bend, false);
            a.release(end);
            let expected = if mode == DrawMode::Brush { vec![start, bend, end] } else { vec![start, end] };
            assert_eq!(a.shapes[0].pts, expected);
            assert_eq!(a.selected, None);
            assert_eq!(a.edit_target, None);
            assert_eq!(a.width_target, None);
            assert_eq!(a.hit_test(end), None);
            a.set_tool(Some(Tool::Select));
            a.press(end, false);
            a.move_to(bend, false);
            a.release(bend);
            assert_eq!(a.shapes[0].pts, expected);
            assert_eq!(a.selected, None);
            a.undo();
            assert!(a.shapes.is_empty());
        }
    }

    #[test]
    fn eraser_brush_click_commits_a_dot() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Eraser));
        let point = Point::new(25.0, 25.0);
        a.press(point, false);
        a.release(point);
        assert_eq!(a.shapes.len(), 1);
        assert_eq!(a.shapes[0].pts, vec![point]);
        assert_eq!(a.selected, None);
    }

    #[test]
    fn effect_brush_follows_freehand_path_and_remains_unselectable() {
        for effect in [Effect::Mosaic, Effect::Blur] {
            let mut a = Annotator::new();
            a.set_tool(Some(Tool::Mosaic));
            a.style_mut(Tool::Mosaic).effect = effect;
            a.style_mut(Tool::Mosaic).mode = DrawMode::Brush;
            let start = Point::new(10.0, 10.0);
            let bend = Point::new(70.0, 80.0);
            let end = Point::new(30.0, 20.0);
            a.press(start, false);
            assert!(a.is_brush_live());
            a.move_to(bend, false);
            a.release(end);
            let sh = &a.shapes[0];
            assert_eq!(sh.pts, vec![start, bend, end]);
            assert_eq!(sh.bounds(), Rect::new(10.0, 10.0, 60.0, 70.0));
            assert!(!sh.is_line_stroke());
            assert_eq!(a.selected, None);
            assert_eq!(a.hit_test(bend), None);
            a.undo();
            assert!(a.shapes.is_empty());
            a.press(start, false);
            a.release(start);
            assert_eq!(a.shapes[0].pts, vec![start]);
        }
    }

    #[test]
    fn effect_rect_keeps_only_drag_endpoints() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Mosaic));
        a.style_mut(Tool::Mosaic).mode = DrawMode::Rect;
        let start = Point::new(10.0, 10.0);
        let end = Point::new(90.0, 70.0);
        a.press(start, false);
        assert!(!a.is_brush_live());
        a.move_to(Point::new(50.0, 5.0), false);
        a.release(end);
        assert_eq!(a.shapes[0].pts, vec![start, end]);
    }

    #[test]
    fn text_resize_is_uniform_anchored_and_independent_of_motion_events() {
        for handle in [Handle::Tl, Handle::Bl, Handle::Br] {
            let mut a = Annotator::new();
            a.set_tool(Some(Tool::Select));
            let mut text = Shape::new(Tool::Text, vec![Point::new(200.0, 200.0)], Style::for_tool(Tool::Text));
            text.text = "Resize text\nSecond line".into();
            text.style.font_size = 24;
            a.add(text.clone());
            let bounds = text.bounds();
            let start = crate::geom::handle_points(bounds.inflated(2.0)).into_iter()
                .find(|(h, _)| *h == handle).unwrap().1;
            assert_eq!(a.hit_handle_of_selected(start), Some(handle));
            a.press(start, false);
            let mut direct = a.clone();
            let delta = Point::new(
                if handle.has_left() { -bounds.w } else if handle.has_right() { bounds.w } else { 0.0 },
                if handle.has_top() { -bounds.h } else if handle.has_bottom() { bounds.h } else { 0.0 },
            );
            a.move_to(start.add(delta.scale(0.5)), false);
            a.move_to(start, false);
            assert_eq!(a.shapes[0], text, "returning to the press position restores the text");
            a.move_to(start.add(delta.scale(0.25)), false);
            a.move_to(start.add(delta), false);
            a.release(start.add(delta));
            direct.release(start.add(delta));
            assert_eq!(a.shapes, direct.shapes, "{handle:?}: release-only and stepped drag must match");
            assert_eq!(a.shapes[0].style.font_size, 48);
            let resized = a.shapes[0].bounds();
            assert!(resized.w > bounds.w && resized.h > bounds.h);
            let anchor = |b: Rect| Point::new(
                if handle.has_left() { b.right() } else if handle.has_right() { b.x } else { b.center().x },
                if handle.has_top() { b.bottom() } else if handle.has_bottom() { b.y } else { b.center().y },
            );
            assert!(anchor(bounds).distance(anchor(resized)) < 0.01);
            a.undo();
            assert_eq!(a.shapes, vec![text]);
        }
    }

    #[test]
    fn text_has_three_resize_corners_and_undoable_delete_button() {
        let mut a = Annotator::new();
        a.set_tool(Some(Tool::Text));
        let mut text = Shape::new(Tool::Text, vec![Point::new(100.0, 100.0)], Style::for_tool(Tool::Text));
        text.text = "Selected text annotation".into();
        a.add(text.clone());
        assert_eq!(a.selected_handle_points().iter().map(|(h, _)| *h).collect::<Vec<_>>(),
            vec![Handle::Tl, Handle::Bl, Handle::Br]);
        for (handle, pt) in handle_points(text.bounds().inflated(2.0)) {
            if !matches!(handle, Handle::Tl | Handle::Bl | Handle::Br) {
                assert_eq!(a.hit_handle_of_selected(pt), None);
            }
        }
        let close = a.selected_text_delete_rect().unwrap().center();
        assert_eq!(crate::cursor::cursor_for(a.cursor_hint(close)), crate::cursor::CursorKind::Arrow);
        a.press(close, false);
        a.release(close);
        assert!(a.shapes.is_empty());
        assert!(a.selected.is_none() && a.edit_target.is_none() && a.width_target.is_none());
        assert!(a.text_edit.is_none());
        a.undo();
        assert_eq!(a.shapes, vec![text]);
    }
}
