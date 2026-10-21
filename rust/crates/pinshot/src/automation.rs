//! Opt-in native automation: persistent documents, native capture and rendering.
use std::collections::BTreeMap;
use std::path::Path;
use std::io::Write;

use pinshot_core::annotation::{ArrowKind, DrawMode, Effect, Shape, Style, Tool};
use pinshot_core::color::Rgba;
use pinshot_core::geom::{Point, Rect};
use pinshot_core::raster::Raster;
use serde::Deserialize;
use serde_json::{json, Value};


static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static CLOSED_EDITORS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<u64, Vec<Shape>>>> = std::sync::OnceLock::new();

fn fresh_id() -> u64 { NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }

pub(crate) fn normalize_ids(shapes: &mut [Shape]) {
    let mut seen = std::collections::HashSet::new();
    for shape in shapes {
        let id = match shape.automation_id {
            Some(id) if id > 0 && seen.insert(id) => id,
            _ => { let mut id = fresh_id(); while !seen.insert(id) { id = fresh_id(); } id }
        };
        shape.automation_id = Some(id);
    }
}

pub(crate) fn publish_closed(session: u64, shapes: Vec<Shape>) {
    if let Ok(mut closed) = CLOSED_EDITORS.get_or_init(Default::default).lock() {
        closed.insert(session, shapes);
    }
}

fn numbered(shapes: Vec<Shape>) -> Vec<(u64, Shape)> {
    shapes.into_iter().map(|shape| (shape.automation_id.unwrap_or_else(fresh_id), shape)).collect()
}

fn titled_windows() -> Vec<(Rect, String)> {
    crate::detect::window_rects().into_iter().filter(|(_, title)| !title.trim().is_empty()).collect()
}

struct WindowTarget {
    title: String,
    rect: Rect,
    activated: bool,
}

fn prepare_window(keyword: &str, activate: bool) -> Result<WindowTarget, String> {
    let needle = keyword.trim().to_lowercase();
    if needle.is_empty() { return Err("Window title keyword is empty".into()); }
    let (_, title) = titled_windows().into_iter().find(|(_, title)| title.to_lowercase().contains(&needle))
        .ok_or_else(|| format!("No visible window title contains \"{}\"; call list_windows first", keyword.trim()))?;
    let activated = activate && activate_window(&title);
    if activated { std::thread::sleep(std::time::Duration::from_millis(300)); }
    let (rect, title) = titled_windows().into_iter().find(|(_, current)| *current == title)
        .ok_or("Window closed before capture")?;
    Ok(WindowTarget { title, rect, activated })
}

#[cfg(windows)]
fn activate_window(title: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{BringWindowToTop, FindWindowW, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow};
    let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) else { return false; };
        if GetForegroundWindow() == hwnd { return true; }
        // Windows ignores SetForegroundWindow from background processes unless input queues are attached.
        let current = GetCurrentThreadId();
        let foreground = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = foreground != 0 && foreground != current && AttachThreadInput(current, foreground, BOOL::from(true)).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached { let _ = AttachThreadInput(current, foreground, BOOL::from(false)); }
        GetForegroundWindow() == hwnd
    }
}

#[cfg(not(windows))]
fn activate_window(_title: &str) -> bool { false }

struct Document {
    image: Raster,
    origin: (i32, i32),
    backend: String,
    shapes: Vec<(u64, Shape)>,
    shown: bool,
    busy: bool,
}

#[derive(Default)]
struct Automation {
    documents: BTreeMap<u64, Document>,
    proxy: Option<winit::event_loop::EventLoopProxy<crate::event::AppEvent>>,
}

#[derive(Deserialize)]
struct Annotation {
    kind: String,
    points: Vec<Point>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    color: Option<Rgba>,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    strength: Option<i32>,
}

impl Annotation {
    fn shape(self, image: &Raster) -> Result<Shape, String> {
        let tool = match self.kind.as_str() {
            "rect" => Tool::Rect,
            "ellipse" => Tool::Ellipse,
            "line" | "arrow" => Tool::Line,
            "text" => Tool::Text,
            "blur" | "mosaic" => Tool::Mosaic,
            _ => return Err("Unsupported annotation kind".into()),
        };
        let count = if tool == Tool::Text { 1 } else { 2 };
        if self.points.len() != count || self.points.iter().any(|p| {
            !p.x.is_finite() || !p.y.is_finite() || p.x < 0.0 || p.y < 0.0
                || p.x >= image.width as f32 || p.y >= image.height as f32
        }) {
            return Err("Points must be inside the image; text needs one point, other shapes two".into());
        }
        if tool == Tool::Text && (self.text.is_empty() || self.text.len() > 16384) {
            return Err("Text must contain 1 to 16384 UTF-8 bytes".into());
        }
        let mut style = Style::for_tool(tool);
        if let Some(color) = self.color { style.color = color; }
        if let Some(width) = self.width {
            if !(1..=200).contains(&width) { return Err("Width must be 1..200".into()); }
            style.width = width;
            style.font_size = width;
        }
        if let Some(strength) = self.strength {
            if !(1..=30).contains(&strength) { return Err("Strength must be 1..30".into()); }
            style.size = strength;
        }
        if self.kind == "arrow" { style.set_arrow_kind(ArrowKind::Thin); }
        if tool == Tool::Mosaic {
            style.mode = DrawMode::Rect;
            style.effect = if self.kind == "blur" { Effect::Blur } else { Effect::Mosaic };
        }
        let mut shape = Shape::new(tool, self.points, style);
        shape.text = self.text;
        Ok(shape)
    }
}

impl Automation {
    fn id(&mut self) -> u64 { fresh_id() }

    fn collect_closed(&mut self) {
        let updates = CLOSED_EDITORS.get_or_init(Default::default).lock().ok().map(|mut closed| {
            self.documents.keys().filter_map(|id| closed.remove(id).map(|shapes| (*id, shapes))).collect::<Vec<_>>()
        }).unwrap_or_default();
        for (session, shapes) in updates {
            if let Some(document) = self.documents.get_mut(&session) {
                document.shapes = numbered(shapes);
                document.shown = false;
                document.busy = false;
            }
        }
    }

    fn editor_request(&self, session_id: u64, command: Value) -> Result<Value, String> {
        let proxy = self.proxy.as_ref().ok_or("No GUI event loop is connected")?;
        let (response, result) = std::sync::mpsc::channel();
        proxy.send_event(crate::event::AppEvent::AutomationEdit { session_id, command, response }).map_err(|e| e.to_string())?;
        result.recv_timeout(std::time::Duration::from_secs(10)).map_err(|_| "Editor did not respond within 10 seconds".to_string())?
    }

    fn accept_snapshot(&mut self, session: u64, snapshot: &Value) -> Result<(), String> {
        if snapshot["open"] == true {
            let shapes: Vec<Shape> = serde_json::from_value(snapshot["shapes"].clone()).map_err(|e| e.to_string())?;
            let document = self.documents.get_mut(&session).ok_or("Unknown session_id")?;
            document.shapes = numbered(shapes);
            document.shown = true;
            document.busy = snapshot["busy"].as_bool().unwrap_or(false);
        } else {
            self.collect_closed();
            if let Some(document) = self.documents.get_mut(&session) { document.shown = false; document.busy = false; }
        }
        Ok(())
    }

    fn sync_document(&mut self, session: u64) -> Result<(), String> {
        if self.proxy.is_some() {
            let snapshot = self.editor_request(session, json!({"op":"snapshot"}))?;
            self.accept_snapshot(session, &snapshot)?;
        }
        Ok(())
    }

    fn commit_shapes(&mut self, session: u64, expected: Vec<Shape>, shapes: Vec<Shape>) -> Result<(), String> {
        if self.proxy.is_some() {
            let response = self.editor_request(session, json!({"op":"apply", "expected":expected, "shapes":shapes}))?;
            if response["open"] == true { return self.accept_snapshot(session, &response); }
            self.collect_closed();
            let actual = self.documents[&session].shapes.iter().map(|(_, shape)| shape.clone()).collect::<Vec<_>>();
            if actual != expected { return Err("Editor changed while closing; query state and retry".into()); }
        }
        self.documents.get_mut(&session).ok_or("Unknown session_id")?.shapes = numbered(shapes);
        Ok(())
    }

    fn view_request(&self, action: &str, session_id: u64, image: Option<std::sync::Arc<Raster>>, shapes: Vec<Shape>) -> Result<Value, String> {
        let proxy = self.proxy.as_ref().ok_or("No GUI event loop is connected")?;
        let (response, result) = std::sync::mpsc::channel();
        proxy.send_event(crate::event::AppEvent::AutomationView {
            action: action.into(), session_id, image, shapes, response,
        }).map_err(|e| e.to_string())?;
        result.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| "Preview window did not respond within 10 seconds".to_string())?
    }

    fn refresh_view(&mut self, session: u64, _show: bool) -> Result<Value, String> {
        let document = self.documents.get(&session).ok_or("Unknown session_id")?;
        let shapes = document.shapes.iter().map(|(_, shape)| shape.clone()).collect::<Vec<_>>();
        let image = std::sync::Arc::new(document.image.clone());
        let result = self.view_request("show", session, Some(image), shapes)?;
        if let Some(document) = self.documents.get_mut(&session) {
            document.shown = result["visible"].as_bool().unwrap_or(false);
        }
        Ok(result)
    }

    fn execute(&mut self, request: &Value) -> Result<Value, String> {
        self.collect_closed();
        let op = request["op"].as_str().ok_or("Missing op")?;
        if op == "status" {
            let sessions = self.documents.keys().copied().collect::<Vec<_>>();
            for session in sessions { self.sync_document(session)?; }
            let documents: Vec<_> = self.documents.iter().map(|(id, d)| json!({
                "session_id": id, "width": d.image.width, "height": d.image.height,
                "origin": d.origin, "backend": d.backend, "busy": d.busy,
                "annotations": d.shapes.iter().map(|(id, shape)| json!({"annotation_id": id, "shape": shape})).collect::<Vec<_>>()
            })).collect();
            let views = if self.proxy.is_some() { self.view_request("state", 0, None, Vec::new())? } else { json!([]) };
            return Ok(json!({"protocol": 1, "pid": std::process::id(), "sessions": documents, "views": views}));
        }
        if op == "windows" {
            let windows: Vec<_> = titled_windows().into_iter().map(|(rect, title)| {
                let (x, y, width, height) = rect.to_i32();
                json!({"title": title, "x": x, "y": y, "width": width, "height": height})
            }).collect();
            return Ok(json!({"windows": windows}));
        }
        if op == "capture" {
            if self.documents.len() >= 4 { return Err("Close a session first (maximum 4)".into()); }
            let target = match request["window"].as_str() {
                Some(keyword) => Some(prepare_window(keyword, request["activate"].as_bool().unwrap_or(true))?),
                None => None,
            };
            let capture = if target.is_some() { crate::capture::Capture::grab_fresh(false)? } else { crate::capture::Capture::grab(false)? };
            if u64::from(capture.composed.width) * u64::from(capture.composed.height) > 40_000_000 {
                return Err("Desktop exceeds 40 million pixels".into());
            }
            let (image, origin, window) = match target {
                Some(target) => {
                    let local = target.rect.translated(-capture.virtual_origin.0 as f32, -capture.virtual_origin.1 as f32)
                        .intersect(capture.composed.bounds());
                    if local.is_empty() { return Err("Window is outside the captured desktop".into()); }
                    let (x, y, _, _) = local.to_i32();
                    let origin = (capture.virtual_origin.0 + x, capture.virtual_origin.1 + y);
                    (capture.composed.crop(local), origin, json!({"title": target.title, "activated": target.activated}))
                }
                None => (capture.composed, capture.virtual_origin, Value::Null),
            };
            let id = self.id();
            let mut result = json!({"session_id": id, "width": image.width,
                "height": image.height, "origin": origin, "backend": capture.backend});
            if !window.is_null() { result["window"] = window; }
            self.documents.insert(id, Document { image,
                origin, backend: capture.backend, shapes: Vec::new(), shown: false, busy: false });
            return Ok(result);
        }
        let session = request["session_id"].as_u64().ok_or("Missing session_id")?;
        if !self.documents.contains_key(&session) { return Err("Unknown session_id".into()); }
        self.sync_document(session)?;
        if self.documents[&session].busy && !matches!(op, "hide" | "close") {
            return Err("Finish the active drawing or text edit first".into());
        }
        if op == "show" { return self.refresh_view(session, true); }
        if op == "hide" {
            let result = self.view_request("hide", session, None, Vec::new())?;
            self.collect_closed();
            self.documents.get_mut(&session).ok_or("Unknown session_id")?.shown = false;
            return Ok(result);
        }
        if op == "close" {
            if self.proxy.is_some() { self.view_request("close", session, None, Vec::new())?; }
            self.collect_closed();
            self.documents.remove(&session);
            return Ok(json!({"closed": session}));
        }
        if matches!(op, "edit" | "undo" | "redo") {
            if op == "edit" { self.refresh_view(session, true)?; }
            if !self.documents[&session].shown { return Err("Show the session before undo/redo".into()); }
            let result = self.editor_request(session, json!({"op":op, "tool":request["tool"]}))?;
            self.accept_snapshot(session, &result)?;
            return Ok(result);
        }
        if op == "annotate" || op == "remove" {
            let document = &self.documents[&session];
            let expected = document.shapes.iter().map(|(_, shape)| shape.clone()).collect::<Vec<_>>();
            let mut shapes = expected.clone();
            let id;
            if op == "annotate" {
                if shapes.len() >= 1000 { return Err("Annotation limit reached".into()); }
                let spec: Annotation = serde_json::from_value(request["annotation"].clone()).map_err(|e| e.to_string())?;
                let mut shape = spec.shape(&document.image)?;
                id = fresh_id();
                shape.automation_id = Some(id);
                shapes.push(shape);
            } else {
                id = request["annotation_id"].as_u64().ok_or("Missing annotation_id")?;
                let index = shapes.iter().position(|shape| shape.automation_id == Some(id)).ok_or("Unknown annotation_id")?;
                shapes.remove(index);
            }
            self.commit_shapes(session, expected, shapes)?;
            return Ok(json!({"session_id":session, "annotation_id":id}));
        }
        let document = self.documents.get_mut(&session).ok_or("Unknown session_id")?;
        match op {
            "preview" | "save" => {
                let path = Path::new(request["path"].as_str().ok_or("Missing path")?);
                if !path.is_absolute() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
                    return Err("An absolute PNG path is required".into());
                }
                if path.exists() { return Err("Output already exists; choose another path".into()); }
                let max_side = if op == "preview" { request["max_side"].as_u64() } else { None };
                if max_side.is_some_and(|side| !(256..=16384).contains(&side)) { return Err("max_side must be 256..16384".into()); }
                let shapes: Vec<_> = document.shapes.iter().map(|(_, shape)| shape.clone()).collect();
                let image = pinshot_core::render::compose(&document.image, &shapes);
                let (source_width, source_height) = (image.width, image.height);
                // Reserve the output so existing files are never silently overwritten.
                let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| e.to_string())?;
                let mut rgba = image::RgbaImage::from_raw(image.width, image.height, image.pixels).ok_or("Invalid raster")?;
                if let Some(side) = max_side {
                    let longest = u64::from(source_width.max(source_height));
                    if longest > side {
                        let fit = |n: u32| ((u64::from(n) * side / longest) as u32).max(1);
                        rgba = image::imageops::resize(&rgba, fit(source_width), fit(source_height), image::imageops::FilterType::Triangle);
                    }
                }
                let (width, height) = rgba.dimensions();
                let mut writer = std::io::BufWriter::new(file);
                let encoded = image::DynamicImage::ImageRgba8(rgba).write_to(&mut writer, image::ImageFormat::Png)
                    .map_err(|e| e.to_string()).and_then(|_| writer.flush().map_err(|e| e.to_string()));
                drop(writer);
                if let Err(error) = encoded {
                    let _ = std::fs::remove_file(path);
                    return Err(error);
                }
                Ok(json!({"session_id": session, "path": path, "width": width, "height": height,
                    "source_width": source_width, "source_height": source_height}))
            }
            _ => Err("Unknown operation".into()),
        }
    }

    fn reply(&mut self, request: Value) -> Value {
        match self.execute(&request) {
            Ok(mut result) => {
                if request["op"] == "capture" && request["show"] == true {
                    if let Some(session) = result["session_id"].as_u64() {
                        match self.refresh_view(session, true) {
                            Ok(view) => result["view"] = view,
                            Err(error) => { result["view_error"] = json!(error); result["mutation_applied"] = json!(true); }
                        }
                    }
                }
                json!({"id": request["id"], "result": result})
            }
            Err(error) => json!({"id": request["id"], "error": error}),
        }
    }
}

#[cfg(windows)]
type Job = (Value, std::sync::mpsc::Sender<Value>);

#[cfg(windows)]
pub fn spawn_server(proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>) -> Result<(), String> {
    let (jobs, queue) = std::sync::mpsc::channel::<Job>();
    // Clients are accepted concurrently, but every request is executed in order on this one thread.
    std::thread::Builder::new().name("pinshot-automation".into()).spawn(move || {
        let mut state = Automation { proxy: Some(proxy), ..Automation::default() };
        for (request, reply) in queue { let _ = reply.send(state.reply(request)); }
    }).map_err(|e| e.to_string())?;
    std::thread::Builder::new().name("pinshot-automation-pipe".into()).spawn(move || {
        let mut listening = None;
        loop {
            let pipe = match listening.take().map_or_else(create_pipe, Ok) {
                Ok(pipe) => pipe,
                Err(error) => {
                    crate::logutil::error(&format!("automation: {error}"));
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
            };
            if let Err(error) = connect_client(&pipe) {
                crate::logutil::error(&format!("automation: {error}"));
                continue;
            }
            // Clients treat a missing pipe name as fatal, so a successor must exist before this instance can close.
            listening = create_pipe().map_err(|error| crate::logutil::error(&format!("automation: {error}"))).ok();
            let jobs = jobs.clone();
            let spawned = std::thread::Builder::new().name("pinshot-automation-client".into()).spawn(move || {
                if let Err(error) = serve_client(pipe, &jobs) { crate::logutil::error(&format!("automation: {error}")); }
            });
            if let Err(error) = spawned { crate::logutil::error(&format!("automation: {error}")); }
        }
    }).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(windows))]
pub fn spawn_server(_proxy: winit::event_loop::EventLoopProxy<crate::event::AppEvent>) -> Result<(), String> {
    Err("Automation transport currently supports Windows only".into())
}

#[cfg(windows)]
struct Pipe(windows::Win32::Foundation::HANDLE);

// The handle is owned by exactly one client thread and closed on drop.
#[cfg(windows)]
unsafe impl Send for Pipe {}

#[cfg(windows)]
impl Drop for Pipe {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Pipes::DisconnectNamedPipe;
        unsafe { let _ = DisconnectNamedPipe(self.0); let _ = CloseHandle(self.0); }
    }
}

#[cfg(windows)]
fn create_pipe() -> Result<Pipe, String> {
    use windows::core::w;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PIPE_REJECT_REMOTE_CLIENTS};
    unsafe {
        let handle = CreateNamedPipeW(w!(r"\\.\pipe\PinShotAutomation-v1"), PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, None);
        if handle == INVALID_HANDLE_VALUE { return Err("Cannot create automation pipe".into()); }
        Ok(Pipe(handle))
    }
}

#[cfg(windows)]
fn connect_client(pipe: &Pipe) -> Result<(), String> {
    use windows::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED};
    use windows::Win32::System::Pipes::ConnectNamedPipe;
    unsafe {
        if ConnectNamedPipe(pipe.0, None).is_err() && GetLastError() != ERROR_PIPE_CONNECTED { return Err("Automation connect failed".into()); }
    }
    Ok(())
}

#[cfg(windows)]
fn serve_client(pipe: Pipe, jobs: &std::sync::mpsc::Sender<Job>) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
    unsafe {
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0u8; 4096];
            let mut count = 0;
            ReadFile(pipe.0, Some(&mut buffer), Some(&mut count), None).map_err(|e| e.to_string())?;
            if count == 0 { return Err("Automation client disconnected".into()); }
            bytes.extend_from_slice(&buffer[..count as usize]);
            if bytes.len() > 65536 { return Err("Automation request too large".into()); }
            if bytes.contains(&b'\n') { break; }
        }
        let reply = match serde_json::from_slice::<Value>(&bytes) {
            Ok(request) => {
                let id = request["id"].clone();
                let (sender, receiver) = std::sync::mpsc::channel();
                jobs.send((request, sender)).map_err(|_| "Automation worker stopped".to_string())?;
                receiver.recv().unwrap_or_else(|_| json!({"id": id, "error": "Automation worker stopped"}))
            }
            Err(_) => json!({"id": null, "error": "Invalid JSON request"}),
        };
        let payload = format!("{reply}\n");
        let mut remaining = payload.as_bytes();
        while !remaining.is_empty() {
            let mut written = 0;
            WriteFile(pipe.0, Some(remaining), Some(&mut written), None).map_err(|e| e.to_string())?;
            if written == 0 { return Err("Automation write failed".into()); }
            remaining = &remaining[written as usize..];
        }
        let _ = FlushFileBuffers(pipe.0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_editor_changes_are_retained_with_stable_ids() {
        let session = fresh_id();
        let mut state = Automation::default();
        state.documents.insert(session, Document { image: Raster::new(100, 100), origin: (0, 0),
            backend: "test".into(), shapes: Vec::new(), shown: true, busy: false });
        let mut shapes = vec![Shape::new(Tool::Rect, vec![Point::new(2.0, 2.0), Point::new(20.0, 20.0)], Style::default())];
        normalize_ids(&mut shapes);
        let id = shapes[0].automation_id.unwrap();
        let mut duplicate = shapes[0].clone();
        duplicate.pts[0].x = 4.0;
        shapes.push(duplicate);
        normalize_ids(&mut shapes);
        assert_eq!(shapes[0].automation_id, Some(id));
        assert_ne!(shapes[1].automation_id, Some(id));
        publish_closed(session, shapes);
        let status = state.execute(&json!({"op":"status"})).unwrap();
        assert_eq!(status["sessions"][0]["annotations"].as_array().unwrap().len(), 2);
        assert_eq!(state.documents[&session].shapes[0].0, id);
        assert!(!state.documents[&session].shown);
    }

    #[test]
    fn native_export_renders_and_refuses_overwrite() {
        let directory = std::env::temp_dir().join(format!("pinshot-automation-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&directory).unwrap();
        let output = directory.join("result.png");
        let mut state = Automation::default();
        let mut image = Raster::new(64, 64);
        image.force_opaque();
        state.documents.insert(1, Document { image, origin: (0, 0), backend: "test".into(), shapes: Vec::new(), shown: false, busy: false });
        state.execute(&json!({"op":"annotate","session_id":1,"annotation":{"kind":"rect","points":[{"x":5,"y":5},{"x":35,"y":35}],"width":3}})).unwrap();
        state.execute(&json!({"op":"save","session_id":1,"path":output})).unwrap();
        let saved = std::fs::read(&output).unwrap();
        let decoded = image::load_from_memory(&saved).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (64, 64));
        assert!(decoded.pixels().any(|p| p.0[0] > 100 && p.0[1] < 50));
        assert!(state.execute(&json!({"op":"save","session_id":1,"path":output})).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), saved);
        std::fs::remove_file(output).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn preview_downscales_but_save_keeps_full_resolution() {
        let directory = std::env::temp_dir().join(format!("pinshot-automation-scale-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&directory).unwrap();
        let mut state = Automation::default();
        let mut image = Raster::new(1000, 500);
        image.force_opaque();
        state.documents.insert(1, Document { image, origin: (0, 0), backend: "test".into(), shapes: Vec::new(), shown: false, busy: false });
        let preview = state.execute(&json!({"op":"preview","session_id":1,"path":directory.join("p.png"),"max_side":400})).unwrap();
        assert_eq!((preview["width"].as_u64(), preview["height"].as_u64()), (Some(400), Some(200)));
        assert_eq!((preview["source_width"].as_u64(), preview["source_height"].as_u64()), (Some(1000), Some(500)));
        assert_eq!(image::image_dimensions(directory.join("p.png")).unwrap(), (400, 200));
        let saved = state.execute(&json!({"op":"save","session_id":1,"path":directory.join("s.png"),"max_side":400})).unwrap();
        assert_eq!(saved["width"].as_u64(), Some(1000));
        assert!(state.execute(&json!({"op":"preview","session_id":1,"path":directory.join("x.png"),"max_side":10})).is_err());
        assert!(!directory.join("x.png").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn document_lifecycle_and_annotation_validation() {
        let mut state = Automation::default();
        state.documents.insert(1, Document { image: Raster::new(100, 100), origin: (-100, 0), backend: "test".into(), shapes: Vec::new(), shown: false, busy: false });
        assert!(state.execute(&json!({"op":"annotate","session_id":1,"annotation":{"kind":"rect","points":[{"x":1,"y":1},{"x":50,"y":50}]}})).is_ok());
        assert_eq!(state.documents[&1].shapes.len(), 1);
        assert!(state.execute(&json!({"op":"annotate","session_id":1,"annotation":{"kind":"text","text":"hello","points":[{"x":200,"y":1}]}})).is_err());
        assert_eq!(state.documents[&1].shapes.len(), 1);
        assert!(state.execute(&json!({"op":"remove","session_id":1,"annotation_id":state.documents[&1].shapes[0].0})).is_ok());
        assert!(state.execute(&json!({"op":"close","session_id":1})).is_ok());
        assert!(state.documents.is_empty());
        assert!(state.execute(&json!({"op":"preview","session_id":1,"path":"x.png"})).is_err());
    }
}
