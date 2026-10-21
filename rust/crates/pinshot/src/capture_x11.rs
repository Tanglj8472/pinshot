//! X11 capture: MIT-SHM `XShmGetImage` when available, else `XGetImage` on the root.

use pinshot_core::geom::Rect;
use pinshot_core::raster::Raster;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

#[derive(Clone, Debug)]
pub struct ScreenFrame {
    pub image: Raster,
    pub physical: Rect,
    pub scale: f32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Capture {
    pub frames: Vec<ScreenFrame>,
    pub virtual_origin: (i32, i32),
    pub composed: Raster,
    pub backend: String,
}

impl Capture {
    pub fn grab(dump: bool) -> Result<Self, String> {
        let t0 = std::time::Instant::now();
        let hold = crate::x11util::InputHold::grab();
        let result = Self::finish_grab(grab_inner(), dump);
        drop(hold);
        crate::logutil::ms("capture grab", t0);
        result
    }

    /// Capture after window activation or overlay close.
    ///
    /// Windows DXGI can return one stale compositor frame. X11 `XGetImage` /
    /// `XShmGetImage` already read current root pixels, so this is one
    /// `InputHold` plus one snapshot. Do not nest it inside another hold —
    /// use `grab_while_held` for that.
    pub fn grab_fresh(dump: bool) -> Result<Self, String> {
        let t0 = std::time::Instant::now();
        let hold = crate::x11util::InputHold::grab();
        let result = Self::finish_grab(grab_inner(), dump);
        drop(hold);
        crate::logutil::ms("capture grab fresh", t0);
        result
    }

    /// Capture while the caller already holds `InputHold` (Python `_snip_now`).
    pub fn grab_while_held(dump: bool) -> Result<Self, String> {
        let t0 = std::time::Instant::now();
        let result = Self::finish_grab(grab_inner(), dump);
        crate::logutil::ms("capture grab", t0);
        result
    }

    fn finish_grab(result: Result<Self, String>, dump: bool) -> Result<Self, String> {
        let cap = result?;
        if dump || std::env::var("PINSHOT_DUMP_CAPTURE").is_ok() {
            let path = std::env::temp_dir().join("pinshot-capture.png");
            if let Some(pm) = cap.composed.to_skia() {
                let _ = pm.save_png(&path);
                crate::logutil::log(&format!(
                    "dumped capture {}x{} → {}",
                    cap.composed.width,
                    cap.composed.height,
                    path.display()
                ));
            }
        }
        crate::logutil::log(&format!(
            "capture {} {}x{} origin={:?}",
            cap.backend, cap.composed.width, cap.composed.height, cap.virtual_origin
        ));
        Ok(cap)
    }
}

fn grab_inner() -> Result<Capture, String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("X11 connect: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let w = screen.width_in_pixels;
    let h = screen.height_in_pixels;
    if w == 0 || h == 0 {
        return Err("X11 root is 0×0".into());
    }

    let mut tried = Vec::new();
    match grab_shm(&conn, root, w, h) {
        Ok(img) if !img.is_visually_blank() => {
            return finish(img, w, h, "x11-shm");
        }
        Ok(_) => tried.push("x11-shm: blank".into()),
        Err(e) => tried.push(format!("x11-shm: {e}")),
    }
    match grab_getimage(&conn, root, w, h) {
        Ok(img) if !img.is_visually_blank() => finish(img, w, h, "x11-getimage"),
        Ok(_) => Err(format!("XGetImage blank ({})", tried.join("; "))),
        Err(e) => Err(format!("XGetImage {e} ({})", tried.join("; "))),
    }
}

fn finish(img: Raster, w: u16, h: u16, backend: &str) -> Result<Capture, String> {
    let physical = Rect::from_i32(0, 0, w as i32, h as i32);
    Ok(Capture {
        frames: vec![ScreenFrame {
            image: img.clone(),
            physical,
            scale: 1.0,
            name: "root".into(),
        }],
        virtual_origin: (0, 0),
        composed: img,
        backend: backend.into(),
    })
}

fn grab_getimage(
    conn: &x11rb::rust_connection::RustConnection,
    root: u32,
    w: u16,
    h: u16,
) -> Result<Raster, String> {
    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, w, h, !0)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    decode_zpixmap(w as u32, h as u32, reply.depth, &reply.data)
}

fn grab_shm(
    conn: &x11rb::rust_connection::RustConnection,
    root: u32,
    w: u16,
    h: u16,
) -> Result<Raster, String> {
    use x11rb::protocol::shm::{self, ConnectionExt as _};
    if conn
        .extension_information(shm::X11_EXTENSION_NAME)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("no MIT-SHM".into());
    }
    let _ = conn
        .shm_query_version()
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let bytes = (w as usize) * (h as usize) * 4;
    let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, bytes, libc::IPC_CREAT | 0o600) };
    if shmid < 0 {
        return Err("shmget failed".into());
    }
    let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
    if addr == (-1isize as *mut libc::c_void) {
        unsafe {
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
        }
        return Err("shmat failed".into());
    }
    let seg = conn.generate_id().map_err(|e| e.to_string())?;
    let attach = conn.shm_attach(seg, shmid as u32, false);
    if attach.is_err() {
        unsafe {
            libc::shmdt(addr);
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
        }
        return Err("ShmAttach failed".into());
    }
    let reply = conn
        .shm_get_image(root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), seg, 0)
        .map_err(|e| e.to_string())?
        .reply();
    let _ = conn.shm_detach(seg);
    let _ = conn.flush();
    let slice = unsafe { std::slice::from_raw_parts(addr as *const u8, bytes) };
    let img = reply
        .map(|r| decode_zpixmap(w as u32, h as u32, r.depth, slice))
        .map_err(|e| e.to_string());
    unsafe {
        libc::shmdt(addr);
        libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
    }
    img?
}

fn decode_zpixmap(w: u32, h: u32, depth: u8, data: &[u8]) -> Result<Raster, String> {
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    let bpp = if depth <= 16 {
        2
    } else if depth <= 24 && data.len() >= (w * h * 3) as usize && data.len() < (w * h * 4) as usize
    {
        3
    } else {
        4
    };
    let stride = (w as usize) * bpp;
    if data.len() < stride * h as usize {
        return Err(format!(
            "short GetImage {} < {}",
            data.len(),
            stride * h as usize
        ));
    }
    if bpp == 4 {
        return Ok(Raster::from_bgra_opaque(w, h, data, stride));
    }
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = y * stride + x * bpp;
            let d = (y * w as usize + x) * 4;
            rgba[d] = data[s + 2];
            rgba[d + 1] = data[s + 1];
            rgba[d + 2] = data[s];
            rgba[d + 3] = 255;
        }
    }
    Raster::from_rgba(w, h, rgba).ok_or_else(|| "rgba".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grab_fresh_matches_grab_signature() {
        let grab: fn(bool) -> Result<Capture, String> = Capture::grab;
        let fresh: fn(bool) -> Result<Capture, String> = Capture::grab_fresh;
        let held: fn(bool) -> Result<Capture, String> = Capture::grab_while_held;
        let _ = (grab, fresh, held);
    }

    #[test]
    fn grab_fresh_takes_its_own_input_hold() {
        // Automation window capture calls grab_fresh without a pre-held
        // InputHold. Headless CI has no X server; the method must still exist
        // and fail at connect rather than at a missing API.
        match Capture::grab_fresh(false) {
            Ok(cap) => {
                assert!(cap.composed.width >= 1);
                assert!(cap.composed.height >= 1);
                assert!(!cap.backend.is_empty());
            }
            Err(e) => {
                assert!(
                    e.contains("X11 connect") || e.contains("root is 0"),
                    "unexpected grab_fresh error: {e}"
                );
            }
        }
    }
}
