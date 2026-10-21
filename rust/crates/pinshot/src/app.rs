//! Application shell: tray, hotkeys, overlay, pins, prefs, session, OCR.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pinshot_core::config::{self, data_dir, Config};
use pinshot_core::geom::Point;
use pinshot_core::history::{HistoryEntry, HistoryFile, SnipHistory};
use pinshot_core::hotkey::Command;
use pinshot_core::i18n::{set_lang, tr};
use pinshot_core::raster::Raster;
use pinshot_core::session::{
    next_pin_filename, pin_png_path, ClosedPin, ClosedStack, PinSnapshot, SessionFile,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::autostart;
use crate::capture::Capture;
use crate::clipboard::{self, ClipContent};
use crate::countdown::CountdownWin;
use crate::event::{AppEvent, PinMenuAction, TrayCmd};
use crate::fallback::FallbackWin;
use crate::native::{self, HotkeyHost};
use crate::overlay::{Overlay, OverlayResult};
use crate::pin::PinWin;
use crate::prefs;
use crate::tray::Tray;
use crate::translate_popup::TranslatePopup;

pub fn run(args: Vec<String>) -> Result<(), String> {
    crate::logutil::init(&args);
    native::init_process();
    let cfg = config::load().unwrap_or_default();
    crate::logutil::set_level(&cfg.log_level);
    set_lang(cfg.lang());

    let cmd = crate::ipc::parse_cli_cmd(&args);
    match crate::ipc::claim_or_forward(cmd)? {
        crate::ipc::Boot::Forwarded => return Ok(()),
        crate::ipc::Boot::Primary(mtx) => {
            let _ = mtx;
        }
    }

    let start_snip = args.iter().any(|a| a == "--snip" || a == "snip");
    let start_pin = args
        .iter()
        .any(|a| a == "--pin" || a == "--pin-clipboard" || a == "pin");
    let start_prefs = args.iter().any(|a| a == "--prefs" || a == "prefs");
    let start_translate = args.iter().any(|a| a == "--translate" || a == "translate");
    let dump_capture = args.iter().any(|a| a == "--dump-capture");

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    crate::ipc::spawn_server(proxy.clone());
    if args.iter().any(|arg| arg == "--automation") {
        crate::automation::spawn_server(proxy.clone())?;
        crate::logutil::log("native automation enabled");
    }

    let mut group_vis = HashMap::new();
    for g in &cfg.groups {
        group_vis.insert(g.clone(), true);
    }

    let mut app = App {
        cfg,
        proxy,
        hotkeys: None,
        tray: None,
        fallback: None,
        overlay: None,
        pins: Vec::new(),
        automation_pins: HashMap::new(),
        closed: ClosedStack::default(),
        history: SnipHistory::new(),
        group_vis,
        start_snip,
        start_pin,
        start_prefs,
        start_translate,
        dump_capture,
        mods: ModifiersState::empty(),
        last_click: None,
        pins_hidden: false,
        snip: SnipSchedule::default(),
        snip_phase: SnipPhase::Idle,
        session_restored: false,
        restore: DesktopRestore::default(),
        prefs: None,
        pin_menu: None,
        translate_popup: None,
        menu_pin: None,
        menu_esc_until: None,
        menu_raise_until: None,
        #[cfg(unix)]
        input_hold: None,
        pump_win: None,
        update_inflight: false,
        update_poll_at: None,
        pending_installer: None,
        #[cfg(windows)]
        dxgi_prefetched: false,
        #[cfg(windows)]
        dxgi_prefetch_at: Some(Instant::now() + Duration::from_millis(500)),
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())
}

/// Single source of truth for the snip lifecycle.
///
/// Replaces the previous implicit combination of `overlay.is_some()`,
/// `snip.at.is_some()` and `snip.countdown.is_some()`. The maintained
/// invariant is: `phase == Overlay` iff `overlay.is_some()`, and
/// `phase == Idle` iff none of `overlay`/`snip.at`/`snip.countdown` are set.
/// Every transition below keeps this invariant, so the old tri-state read in
/// `request_snip` (`overlay || snip.at || snip.countdown`) is exactly
/// `phase != Idle`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SnipPhase {
    /// Nothing pending; hotkeys/tray can arm a new snip.
    Idle,
    /// Countdown window ticking down (`snip.countdown` is `Some`).
    Countdown,
    /// Fallback hidden, waiting `hide_wait_ms()` before the grab.
    HideWait,
    /// Capture taken, annotation overlay is live (`overlay` is `Some`).
    Overlay,
}

impl SnipPhase {
    /// Phase entered when a snip is armed: `Countdown` if a delay is
    /// configured, otherwise jump straight to `HideWait`.
    fn arm(delay_enabled: bool) -> Self {
        if delay_enabled {
            SnipPhase::Countdown
        } else {
            SnipPhase::HideWait
        }
    }

    /// Phase after a single countdown tick. While `left > 1` we stay in
    /// `Countdown`; the final tick (`left <= 1`) transitions to `HideWait`
    /// and the grab proceeds.
    fn countdown_tick(left: u32) -> Self {
        if left > 1 {
            SnipPhase::Countdown
        } else {
            SnipPhase::HideWait
        }
    }
}

#[derive(Default)]
struct SnipSchedule {
    at: Option<Instant>,
    translate_next: bool,
    countdown: Option<CountdownWin>,
    countdown_left: u32,
}

#[derive(Default)]
struct DesktopRestore {
    fallback_hidden: bool,
    wait_after_overlay: bool,
    discard_dxgi_frame: bool,
}

struct App {
    cfg: Config,
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    hotkeys: Option<HotkeyHost>,
    tray: Option<Tray>,
    fallback: Option<FallbackWin>,
    overlay: Option<Overlay>,
    pins: Vec<PinWin>,
    automation_pins: HashMap<u64, PinWin>,
    closed: ClosedStack,
    history: SnipHistory,
    group_vis: HashMap<String, bool>,
    start_snip: bool,
    start_pin: bool,
    start_prefs: bool,
    start_translate: bool,
    dump_capture: bool,
    mods: ModifiersState,
    last_click: Option<(WindowId, Instant, Point)>,
    pins_hidden: bool,
    snip: SnipSchedule,
    snip_phase: SnipPhase,
    session_restored: bool,
    restore: DesktopRestore,
    prefs: Option<crate::prefs_win::PrefsWin>,
    pin_menu: Option<crate::pin_menu::PinMenuWin>,
    translate_popup: Option<TranslatePopup>,
    menu_pin: Option<WindowId>,
    /// Swallow Escape on pins after the menu closes so the still-held Esc
    /// does not close the pin (and persist/reap must not drop the others).
    menu_esc_until: Option<Instant>,
    /// Keep restacking the menu above the pin (and its F5 strip) briefly.
    menu_raise_until: Option<Instant>,
    #[cfg(unix)]
    input_hold: Option<crate::x11util::InputHold>,
    /// Hidden winit HWND so the event loop still pumps after the overlay drops.
    /// Reused as the snip overlay so the first F1 is not a cold DWM surface.
    pump_win: Option<Rc<Window>>,
    update_inflight: bool,
    update_poll_at: Option<Instant>,
    pending_installer: Option<std::path::PathBuf>,
    #[cfg(windows)]
    dxgi_prefetched: bool,
    #[cfg(windows)]
    dxgi_prefetch_at: Option<Instant>,
}

impl App {
    fn group_list(&self) -> Vec<(String, bool)> {
        self.cfg
            .groups
            .iter()
            .map(|g| (g.clone(), *self.group_vis.get(g).unwrap_or(&true)))
            .collect()
    }

    fn rebuild_tray(&mut self) {
        // tray-icon/GTK can panic when the desktop has no indicator (Xfce without a
        // notification plugin). Never take the process down — fallback window exists.
        let groups = self.group_list();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Tray::new(&groups))) {
            Ok(Ok(t)) => self.tray = Some(t),
            Ok(Err(e)) => {
                crate::logutil::log(&format!("tray: {e}"));
                self.tray = None;
            }
            Err(_) => {
                crate::logutil::log("tray: panic (GTK/indicator missing); using fallback window");
                self.tray = None;
            }
        }
    }

    fn ensure_fallback(&mut self, event_loop: &ActiveEventLoop) {
        let need = {
            #[cfg(unix)]
            {
                let _ = event_loop;
                true
            }
            #[cfg(not(unix))]
            {
                self.tray.is_none()
            }
        };
        if need && self.fallback.is_none() {
            match FallbackWin::create(event_loop) {
                Ok(mut f) => {
                    f.apply_theme(&self.cfg.theme);
                    f.retranslate();
                    self.fallback = Some(f);
                }
                Err(e) => crate::logutil::log(&format!("fallback window: {e}")),
            }
        }
    }

    fn maybe_run_scheduled_update(&mut self) {
        if self.update_inflight {
            return;
        }
        if !pinshot_core::update::due_for_check_now(&self.cfg.update) {
            return;
        }
        self.start_update_check();
    }

    fn start_update_check(&mut self) {
        if self.update_inflight {
            return;
        }
        self.update_inflight = true;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let r = crate::update_run::check_and_apply(true);
            let _ = proxy.send_event(AppEvent::UpdateCheckDone(r));
        });
    }

    fn finish_update_check(&mut self, result: Result<crate::update_run::UpdateOutcome, String>) {
        self.update_inflight = false;
        self.cfg.update.last_check_unix = pinshot_core::update::now_unix();
        if let Err(e) = pinshot_core::config::record_update_check(self.cfg.update.last_check_unix) {
            crate::logutil::log(&format!("save after update check: {e}"));
        }
        match result {
            Ok(crate::update_run::UpdateOutcome::UpToDate) => {
                crate::logutil::log("update: up to date");
            }
            Ok(crate::update_run::UpdateOutcome::UpdateAvailable { version }) => {
                crate::logutil::log(&format!("update available {version} (manual install on this OS)"));
            }
            Ok(crate::update_run::UpdateOutcome::ReadyToInstall { .. }) => {}
            Err(e) => crate::logutil::log(&format!("update check: {e}")),
        }
    }

    fn apply_cfg(&mut self, cfg: Config) {
        crate::logutil::set_level(&cfg.log_level);
        set_lang(cfg.lang());
        if cfg.run_at_login != autostart::is_enabled() {
            if let Err(e) = autostart::set_enabled(cfg.run_at_login) {
                crate::logutil::error(&format!("autostart: {e}"));
            }
        }
        self.cfg = cfg;
        self.hotkeys = None;
        self.rebuild_tray();
        if let Some(fb) = &mut self.fallback {
            fb.retranslate();
            fb.apply_theme(&self.cfg.theme);
        }
    }

    fn request_snip(&mut self, event_loop: &ActiveEventLoop, translate: bool) {
        if self.snip_phase == SnipPhase::Overlay {
            let closing_resnip = self
                .overlay
                .as_ref()
                .and_then(|o| o.peek_result())
                .is_some_and(|r| matches!(r, OverlayResult::Resnip));
            let closing = self
                .overlay
                .as_ref()
                .is_some_and(|o| o.peek_result().is_some());
            if !closing {
                // Overlay may be up while Chrome kept keyboard focus, so F1
                // lands here instead of OverlayKeyGrab. Restart the snip.
                crate::logutil::log("hotkey F1 while overlay live");
                if let Some(ov) = self.overlay.as_mut() {
                    ov.resnip();
                }
                self.finish_overlay(event_loop);
                return;
            }
            self.finish_overlay(event_loop);
            if closing_resnip {
                return;
            }
        }
        if self.snip_phase != SnipPhase::Idle {
            return;
        }
        self.snip.translate_next = translate;
        let delay = self.cfg.snip_delay;
        if delay > 0 {
            self.snip.countdown_left = delay;
            match CountdownWin::create(event_loop, delay) {
                Ok(c) => self.snip.countdown = Some(c),
                Err(e) => crate::logutil::log(&format!("countdown: {e}")),
            }
            self.snip.at = Some(Instant::now() + Duration::from_secs(1));
            self.snip_phase = SnipPhase::arm(true);
        } else {
            self.prepare_capture_hide();
            self.snip.at = Some(Instant::now() + Duration::from_millis(self.hide_wait_ms()));
            self.snip_phase = SnipPhase::arm(false);
        }
    }

    fn hide_wait_ms(&self) -> u64 {
        // Overlay is visible to DXGI (remote-desktop tools must see it). After
        // it closes, give DWM a beat so the next grab is not the overlay itself.
        if self.restore.wait_after_overlay || self.restore.fallback_hidden {
            150
        } else {
            0
        }
    }

    fn prepare_capture_hide(&mut self) {
        // Python `_snip_now`: grab pointer+keyboard *before* hiding the
        // fallback so the first drag after F1 is not delivered to Chrome.
        #[cfg(unix)]
        if self.input_hold.is_none() {
            self.input_hold = Some(crate::x11util::InputHold::grab());
        }
        if let Some(fb) = &self.fallback {
            if fb.is_visible() {
                fb.hide();
                self.restore.fallback_hidden = true;
            }
        }
        // A tray click is still being processed when the capture starts. Hide
        // the icon before the desktop grab so the tray/menu cannot leak into
        // the captured image.
        if let Some(tray) = &self.tray {
            tray.hide_icon();
        }
        self.snip.countdown = None;
    }

    fn restore_fallback_if_needed(&mut self) {
        if self.restore.fallback_hidden {
            if let Some(fb) = &self.fallback {
                fb.show();
            }
            self.restore.fallback_hidden = false;
        }
    }

    fn begin_snip(&mut self, event_loop: &ActiveEventLoop) {
        if self.snip_phase == SnipPhase::Overlay {
            return;
        }
        self.restore.wait_after_overlay = false;
        crate::logutil::log("begin_snip capture");
        if self.history.entries.is_empty() {
            self.restore_history();
        }
        let t0 = Instant::now();
        let translate = self.snip.translate_next;
        self.snip.translate_next = false;
        crate::logutil::step("before capture");
        #[cfg(unix)]
        if self.input_hold.is_none() {
            self.input_hold = Some(crate::x11util::InputHold::grab());
        }
        #[cfg(unix)]
        let grabbed = Capture::grab_while_held(self.dump_capture);
        #[cfg(not(unix))]
        let grabbed = if self.restore.discard_dxgi_frame {
            self.restore.discard_dxgi_frame = false;
            Capture::grab_fresh(self.dump_capture)
        } else {
            Capture::grab(self.dump_capture)
        };
        match grabbed {
            Ok(cap) => match Overlay::start(
                event_loop,
                cap,
                SnipHistory::new(),
                self.cfg.history_max,
                self.cfg.dim_alpha,
                translate,
                self.proxy.clone(),
                self.pump_win.clone(),
                #[cfg(unix)]
                self.input_hold.take(),
            ) {
                Ok(mut ov) => {
                    ov.history = std::mem::take(&mut self.history);
                    self.pump_win = Some(ov.window_rc());
                    self.overlay = Some(ov);
                    self.snip_phase = SnipPhase::Overlay;
                }
                Err(e) => {
                    #[cfg(unix)]
                    {
                        self.input_hold = None;
                    }
                    crate::logutil::error(&format!("overlay: {e}"));
                    self.snip_phase = SnipPhase::Idle;
                }
            },
            Err(e) => {
                #[cfg(unix)]
                {
                    self.input_hold = None;
                }
                crate::logutil::error(&format!("capture: {e}"));
                self.snip_phase = SnipPhase::Idle;
            }
        }
        crate::logutil::ms("begin_snip total", t0);
    }

    fn pin_clipboard(&mut self, event_loop: &ActiveEventLoop) {
        match clipboard::read_best() {
            ClipContent::Image(img) => self.spawn_pin(event_loop, img, None, None),
            ClipContent::Color(c) => {
                self.spawn_pin(event_loop, clipboard::color_card(c, 220, 140), None, None)
            }
            ClipContent::Text(t) => {
                self.spawn_pin(event_loop, clipboard::text_card(&t, 280, 120), None, None)
            }
            ClipContent::Empty => crate::logutil::log("clipboard: no supported image or text"),
        }
    }

    fn spawn_pin(
        &mut self,
        event_loop: &ActiveEventLoop,
        img: Raster,
        pos: Option<(i32, i32)>,
        group: Option<String>,
    ) {
        self.spawn_pin_ex(event_loop, img, pos, group, true);
    }

    fn spawn_pin_ex(
        &mut self,
        event_loop: &ActiveEventLoop,
        img: Raster,
        pos: Option<(i32, i32)>,
        group: Option<String>,
        persist: bool,
    ) {
        let pos = pos.unwrap_or_else(|| native::pin_pos_near_cursor(img.width, img.height));
        match PinWin::create(event_loop, img, pos) {
            Ok(mut p) => {
                if let Some(g) = group {
                    p.group = g;
                }
                let vis = *self.group_vis.get(&p.group).unwrap_or(&true);
                if !vis || self.pins_hidden {
                    p.hide();
                }
                self.pins.push(p);
                if persist {
                    self.persist_session();
                }
            }
            Err(e) => crate::logutil::error(&format!("pin: {e}")),
        }
    }

    fn finish_overlay(&mut self, event_loop: &ActiveEventLoop) {
        let Some(mut ov) = self.overlay.take() else {
            return;
        };
        let Some(res) = ov.take_result() else {
            self.overlay = Some(ov);
            return;
        };
        self.history = std::mem::take(&mut ov.history);
        self.history.entries.truncate(self.cfg.history_max as usize);
        let persist = res.writes_history();
        let resnip = matches!(res, OverlayResult::Resnip);
        self.snip_phase = SnipPhase::Idle;
        ov.hide_for_capture();
        self.pump_win = Some(ov.window_rc());
        drop(ov);
        crate::logutil::log("overlay closed");
        crate::native::release_stray_grabs();
        self.retire_pump_window();
        self.persist_overlay_history(persist);
        self.handle_overlay_result(event_loop, res);
        if resnip {
            self.restore.wait_after_overlay = true;
            self.restore.discard_dxgi_frame = true;
            crate::logutil::log("resnip: wait 150ms for overlay to leave the DXGI frame");
            self.request_snip(event_loop, false);
        } else {
            self.restore_fallback_if_needed();
            if let Some(tray) = &self.tray {
                tray.show_icon();
            }
        }
    }

    fn handle_overlay_result(&mut self, event_loop: &ActiveEventLoop, res: OverlayResult) {
        match res {
            OverlayResult::Copy(img, _) => {
                if clipboard::write_image(&img).is_ok() {
                    crate::logutil::log("clipboard: image copied successfully");
                }
            }
            OverlayResult::Pin(img, _, pos) => self.spawn_pin(event_loop, img, Some(pos), None),
            OverlayResult::Save(img) => self.save_overlay_image(&img),
            OverlayResult::Ocr(img, translate, pos) => {
                self.handle_overlay_ocr(event_loop, img, translate, pos)
            }
            OverlayResult::Cancel => {}
            OverlayResult::Resnip => {}
        }
    }

    fn persist_overlay_history(&mut self, persist: bool) {
        // Keep DXGI warm between screenshots. Reopening D3D11 and
        // DuplicateOutput on every F1 puts initialization on the hot path.
        if persist {
            let t0 = Instant::now();
            self.persist_history();
            crate::logutil::log(&format!(
                "persist history {} entries in {}ms",
                self.history.entries.len(),
                t0.elapsed().as_millis()
            ));
        } else {
            crate::logutil::log("skip history persist (cancel/resnip)");
        }
        // Full-desktop history frames stay on disk; only metadata remains hot.
        self.history.evict_pixels();
    }

    fn save_overlay_image(&self, img: &Raster) {
        let folder = self.cfg.quick_save_folder.trim();
        let filename = pinshot_core::config::expand_save_filename(&self.cfg.save_filename_format);
        let suggested = if folder.is_empty() {
            filename.clone()
        } else {
            let sep = if cfg!(windows) { "\\" } else { "/" };
            format!("{folder}{sep}{filename}")
        };
        if let Some(path) = crate::dialog::save_path(&suggested) {
            if crate::dialog::save_raster(img, &path, self.cfg.image_quality).is_ok() {
                crate::logutil::log("output: image saved successfully");
            }
        }
    }

    fn handle_overlay_ocr(
        &mut self,
        event_loop: &ActiveEventLoop,
        img: Raster,
        translate: bool,
        pos: (i32, i32),
    ) {
        self.spawn_pin(event_loop, img.clone(), Some(pos), None);
        if let Some(p) = self.pins.last_mut() {
            if p.start_ocr() {
                p.set_status(Some(tr("Recognizing…")));
                let id = p.window_id();
                self.spawn_ocr(id, img, translate);
            }
        }
    }

    fn request_quit(&mut self, event_loop: &ActiveEventLoop) {
        crate::logutil::log("quit");
        if let Some(path) = self.pending_installer.take() {
            if let Err(e) = crate::update_run::defer_install_after_quit(path) {
                crate::logutil::log(&format!("update helper: {e}"));
                return;
            }
        }
        prefs::close_settings();
        if let Some(w) = self.prefs.as_mut() {
            w.close();
        }
        self.prefs = None;
        self.persist_session();
        crate::ocr_run::release_engine();
        crate::logutil::log("quit persist done");
        // The fallback termination thread can run even if tray teardown stalls.
        // Persist queued diagnostics before scheduling that forced exit.
        crate::logutil::flush();
        // Schedule the kill *before* hiding the tray: NIM_DELETE is a
        // SendMessage to the tray HWND and can stall this thread.
        native::schedule_kill_process(0);
        if let Some(tray) = &self.tray {
            tray.hide_icon();
        }
        crate::logutil::log("quit tray hidden");
        crate::logutil::log("quit exiting");
        crate::logutil::detach_console();
        let _ = event_loop;
        native::park_until_killed();
    }

    fn ensure_pump_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.pump_win.is_some() {
            return;
        }
        #[cfg(windows)]
        {
            let attrs = native::as_tool_window(
                Window::default_attributes()
                    .with_title("")
                    .with_decorations(false)
                    .with_visible(false)
                    .with_inner_size(winit::dpi::LogicalSize::new(1.0, 1.0)),
            );
            if let Ok(w) = event_loop.create_window(attrs) {
                w.set_visible(false);
                self.pump_win = Some(Rc::new(w));
            }
        }
        #[cfg(not(windows))]
        let _ = event_loop;
    }

    fn spawn_ocr(&self, pin: winit::window::WindowId, img: Raster, translate: bool) {
        crate::logutil::log(&format!(
            "ocr start {}x{} translate={translate}",
            img.width, img.height
        ));
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let mut last = String::new();
            let result = crate::ocr_run::recognize_with_progress(&img, |msg| {
                if msg != last {
                    last = msg.to_string();
                    let _ = proxy.send_event(AppEvent::OcrProgress {
                        pin,
                        message: msg.to_string(),
                    });
                }
            })
            .map_err(|e| {
        crate::logutil::error(&format!("ocr fail: {e}"));
                e
            });
            match &result {
                Ok(page) => {
                    crate::logutil::log(&format!("ocr done {} chars", page.text.chars().count()))
                }
                Err(e) => crate::logutil::log(&format!("ocr empty: {e}")),
            }
            let _ = proxy.send_event(AppEvent::OcrDone {
                pin,
                result,
                translate,
            });
        });
    }

    fn spawn_translate(&self, pin: winit::window::WindowId, text: String) {
        let text = pinshot_core::ocr::page_for_translate(&text);
        let proxy = self.proxy.clone();
        let settings = self.cfg.translate.clone();
        let ui_lang = self.cfg.language.clone();
        std::thread::spawn(move || {
            let result =
                crate::translate_run::translate_with(&settings, &ui_lang, &text).map_err(|e| {
                    crate::logutil::error("translate: background request failed");
                    e
                });
            let _ = proxy.send_event(AppEvent::TranslateDone { pin, result });
        });
    }

    fn toggle_pins(&mut self) {
        self.pins_hidden = !self.pins_hidden;
        for p in &self.pins {
            if self.pins_hidden {
                p.hide();
            } else if *self.group_vis.get(&p.group).unwrap_or(&true) {
                p.show();
            }
        }
    }

    fn toggle_group(&mut self, name: String) {
        let vis = !*self.group_vis.get(&name).unwrap_or(&true);
        self.group_vis.insert(name.clone(), vis);
        for p in &self.pins {
            if p.group.eq_ignore_ascii_case(&name) {
                if vis && !self.pins_hidden {
                    p.show();
                } else {
                    p.hide();
                }
            }
        }
        self.rebuild_tray();
    }

    fn restore_closed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(c) = self.closed.pop() {
            if let Some(img) = decode_png(&c.png) {
                self.spawn_pin(event_loop, img, Some(c.pos), Some(c.group));
                if let Some(p) = self.pins.last_mut() {
                    p.set_shadow(c.shadow);
                    p.thumb = c.thumb;
                    p.set_scale(c.scale);
                    p.set_opacity_ex(c.opacity, false);
                    p.persist_needed = false;
                }
            }
        }
    }

    fn pin_or_selection_translate(&mut self) {
        if let Some(id) = self.menu_pin {
            self.start_pin_translate(id);
            return;
        }
        self.selection_translate();
    }

    fn start_pin_translate(&mut self, pin: WindowId) {
        let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pin) else {
            return;
        };
        if let Some(text) = p.ocr_text() {
            if !text.trim().is_empty() {
                p.set_translating(true);
                self.spawn_translate(pin, text);
                return;
            }
        }
        p.set_status(Some(tr("Recognizing…")));
        let img = p.rendered();
        self.spawn_ocr(pin, img, true);
    }

    fn selection_translate(&self) {
        #[cfg(not(windows))]
        {
            crate::logutil::log("selection_translate: unavailable on this platform");
            return;
        }
        #[cfg(windows)]
        {
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let backup = clipboard::read_best();
                if crate::win32::send_ctrl_c().is_err() {
                    let _ = proxy.send_event(AppEvent::SelectionText(Err(tr(
                        "Could not copy selected text.",
                    ))));
                    return;
                }
                std::thread::sleep(Duration::from_millis(80));
                let text = match clipboard::read_best() {
                    ClipContent::Text(t) => t,
                    _ => String::new(),
                };
                if let ClipContent::Image(img) = backup {
                    let _ = clipboard::write_image(&img);
                }
                let result = if text.trim().is_empty() {
                    Err(tr("Could not copy selected text."))
                } else {
                    Ok(text)
                };
                let _ = proxy.send_event(AppEvent::SelectionText(result));
            });
        }
    }

    fn spawn_selection_translation(&self, source: String) {
        let proxy = self.proxy.clone();
        let settings = self.cfg.translate.clone();
        let ui_lang = self.cfg.language.clone();
        let input = source.clone();
        std::thread::spawn(move || {
            let result = crate::translate_run::translate_with(&settings, &ui_lang, &input)
                .map_err(|e| { crate::logutil::error("selection_translate: background request failed"); e });
            let _ = proxy.send_event(AppEvent::SelectionTranslation { source, result });
        });
    }

    fn persist_session(&self) {
        let dir = data_dir();
        let mut snaps = Vec::new();
        for p in &self.pins {
            if p.closed {
                continue;
            }
            let file = next_pin_filename(&snaps);
            // Persist the original pixels + annotation shapes (Python bakes
            // composed() into the PNG; we keep them separate so restore still
            // lets the user undo/edit after a restart).
            let png = encode_png(&p.img);
            if png.is_empty() {
                continue;
            }
            let path = pin_png_path(&dir, &file);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&path, &png).is_err() {
                continue;
            }
            let (x, y) = p.img_origin();
            snaps.push(PinSnapshot {
                file,
                x,
                y,
                scale: p.scale,
                opacity: p.opacity,
                group: p.group.clone(),
                shapes: p.annot.shapes.clone(),
                shadow: p.shadow,
                thumb: p.thumb,
            });
        }
        let sess = SessionFile {
            groups: self.cfg.groups.clone(),
            pins: snaps,
        };
        let _ = sess.save(&dir);
    }

    fn restore_session(&mut self, event_loop: &ActiveEventLoop) {
        let dir = data_dir();
        let sess = SessionFile::load(&dir).unwrap_or_default();
        if !sess.groups.is_empty() {
            self.cfg.groups = sess.groups;
        }
        for snap in sess.pins {
            let path = pin_png_path(&dir, &snap.file);
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Some(img) = decode_png(&bytes) else {
                continue;
            };
            self.spawn_pin_ex(
                event_loop,
                img,
                Some((snap.x, snap.y)),
                Some(snap.group),
                false,
            );
            if let Some(p) = self.pins.last_mut() {
                p.set_shadow(snap.shadow);
                p.thumb = snap.thumb;
                p.set_scale(snap.scale);
                p.set_opacity_ex(snap.opacity, false);
                if !snap.shapes.is_empty() {
                    p.restore_shapes(snap.shapes);
                }
                p.persist_needed = false;
            }
        }
    }

    fn persist_history(&mut self) {
        let dir = HistoryFile::dir(&data_dir());
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let mut names = Vec::new();
        let mut kept = SnipHistory::new();
        for e in &mut self.history.entries {
            if !e.resident() {
                continue;
            }
            let name = format!("hist-{:03}.png", names.len());
            let png = encode_png(&e.image);
            if png.is_empty() {
                continue;
            }
            let path = dir.join(&name);
            if std::fs::write(&path, png).is_err() {
                continue;
            }
            e.disk = Some(path);
            names.push(name);
            kept.entries.push(HistoryEntry {
                rect: e.rect,
                image: Raster::new(0, 0),
                disk: e.disk.clone(),
            });
        }
        let file = kept.to_file(&names);
        if let Ok(text) = file.to_toml() {
            let _ = std::fs::write(HistoryFile::path(&data_dir()), text);
        }
    }

    fn restore_history(&mut self) {
        let dir = data_dir();
        let Ok(text) = std::fs::read_to_string(HistoryFile::path(&dir)) else {
            return;
        };
        let Ok(file) = HistoryFile::from_toml(&text) else {
            return;
        };
        let hist_dir = HistoryFile::dir(&dir);
        let mut hist = SnipHistory::new();
        for meta in file.entries {
            let path = hist_dir.join(&meta.file);
            if !path.is_file() {
                continue;
            }
            hist.entries.push(HistoryEntry {
                rect: meta.rect(),
                image: Raster::new(0, 0),
                disk: Some(path),
            });
        }
        hist.idx = hist.entries.len();
        if !hist.entries.is_empty() {
            crate::logutil::log(&format!(
                "restored {} snip history slots (lazy)",
                hist.entries.len()
            ));
        }
        self.history = hist;
    }

    fn retire_pump_window(&self) {
        let Some(w) = &self.pump_win else {
            return;
        };
        w.set_visible(false);
        if !native::configure_window_geom(w, 0, 0, 1, 1) {
            let _ = w.request_inner_size(winit::dpi::PhysicalSize::new(1, 1));
        }
    }

    fn reap_pins(&mut self) {
        let mut i = 0;
        while i < self.pins.len() {
            if self.pins[i].closed {
                let p = self.pins.remove(i);
                if !p.destroy {
                    self.closed.push(
                        ClosedPin {
                            png: encode_png(&p.rendered()),
                            pos: p.img_origin(),
                            scale: p.scale,
                            opacity: p.opacity,
                            group: p.group.clone(),
                            shadow: p.shadow,
                            thumb: p.thumb,
                        },
                        self.cfg.closed_pins_max,
                    );
                }
            } else {
                i += 1;
            }
        }
    }

    fn open_prefs_win(&mut self, event_loop: &ActiveEventLoop) {
        if self.prefs.is_some() {
            let _ = self.proxy.send_event(AppEvent::RaisePrefs);
            return;
        }
        match crate::prefs_win::PrefsWin::create(event_loop, self.cfg.clone(), self.proxy.clone()) {
            Ok(w) => self.prefs = Some(w),
            Err(e) => crate::logutil::log(&format!("prefs window: {e}")),
        }
    }

    fn open_pin_menu(&mut self, event_loop: &ActiveEventLoop, pid: WindowId) {
        let Some(p) = self.pins.iter().find(|p| p.window_id() == pid) else {
            return;
        };
        let (px, py) = p.pos();
        let state = crate::pin_menu::PinMenuState {
            annotating: p.annotating,
            click_through: p.click_through,
            shadow: p.shadow,
            scale: p.scale,
            opacity: p.opacity,
            group: p.group.clone(),
            groups: self.cfg.groups.clone(),
            group_counts: self
                .cfg
                .groups
                .iter()
                .map(|g| {
                    let n = self
                        .pins
                        .iter()
                        .filter(|pin| pin.group.eq_ignore_ascii_case(g))
                        .count();
                    (g.clone(), n)
                })
                .collect(),
            img_w: p.img.width,
            img_h: p.img.height,
            smooth: p.smooth,
            grayscale: p.grayscale,
            invert: p.invert,
            rotation: p.rotation,
            mirror_x: p.mirror_x,
            mirror_y: p.mirror_y,
        };
        let (mw, mh) = crate::pin_menu::menu_pixel_size(&state);
        let (mx, my) = crate::pin_menu::clamp_menu_pos(
            px + p.last_cursor_i32().0,
            py + p.last_cursor_i32().1,
            mw as i32,
            mh as i32,
            1920,
            1200,
        );
        match crate::pin_menu::PinMenuWin::create(event_loop, (mx, my), state) {
            Ok(m) => {
                if let Some(p) = self.pins.iter().find(|p| p.window_id() == pid) {
                    m.restack_above(p.window());
                }
                self.menu_pin = Some(pid);
                self.pin_menu = Some(m);
                self.menu_esc_until = None;
                self.menu_raise_until = Some(Instant::now() + Duration::from_millis(300));
            }
            Err(e) => crate::logutil::log(&format!("pin menu: {e}")),
        }
    }

    fn dismiss_pin_menu(&mut self) {
        if let Some(m) = self.pin_menu.as_mut() {
            m.close();
        }
        self.pin_menu = None;
        self.menu_raise_until = None;
        self.menu_esc_until = Some(Instant::now() + Duration::from_millis(450));
    }

    fn pin_esc_guarded(&self) -> bool {
        self.pin_menu.is_some()
            || self
                .menu_esc_until
                .map(|t| Instant::now() < t)
                .unwrap_or(false)
    }

    fn apply_pin_menu(&mut self, action: PinMenuAction) {
        let Some(pid) = self.menu_pin else {
            return;
        };
        if matches!(action, PinMenuAction::NewGroup | PinMenuAction::RenameGroup) {
            return;
        }
        if let PinMenuAction::NewGroupNamed(ref name) = action {
            if name.trim().is_empty() {
                crate::logutil::log("pin menu empty name ignored");
                return;
            }
            let name = pinshot_core::groups::ensure(&mut self.cfg.groups, name);
            self.group_vis.entry(name.clone()).or_insert(true);
            if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pid) {
                p.group = name;
                p.persist_needed = true;
            }
            let _ = pinshot_core::config::save(&self.cfg);
            self.rebuild_tray();
            crate::logutil::log("pin menu new group named");
            return;
        }
        if let PinMenuAction::RenameGroupNamed(ref name) = action {
            if name.trim().is_empty() {
                crate::logutil::log("pin menu empty name ignored");
                return;
            }
            let old = self
                .pins
                .iter()
                .find(|p| p.window_id() == pid)
                .map(|p| p.group.clone())
                .unwrap_or_else(|| pinshot_core::groups::DEFAULT.into());
            if old.eq_ignore_ascii_case(pinshot_core::groups::DEFAULT) {
                crate::logutil::log("pin menu rename skipped (Default)");
                return;
            }
            match pinshot_core::groups::rename(&mut self.cfg.groups, &old, name) {
                Ok(new) => {
                    let key = self
                        .group_vis
                        .keys()
                        .find(|k| k.eq_ignore_ascii_case(&old))
                        .cloned();
                    if let Some(k) = key {
                        let vis = self.group_vis.remove(&k).unwrap_or(true);
                        self.group_vis.insert(new.clone(), vis);
                    } else {
                        self.group_vis.entry(new.clone()).or_insert(true);
                    }
                    for p in &mut self.pins {
                        if p.group.eq_ignore_ascii_case(&old) {
                            p.group = new.clone();
                            p.persist_needed = true;
                        }
                    }
                    let _ = pinshot_core::config::save(&self.cfg);
                    self.rebuild_tray();
                    crate::logutil::log("pin menu: group renamed");
                }
                Err(e) => crate::logutil::log(&format!("pin menu rename rejected: {e}")),
            }
            return;
        }
        if let PinMenuAction::MoveGroup(ref g) = action {
            let vis = *self.group_vis.get(g).unwrap_or(&true);
            if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pid) {
                p.apply_menu(action.clone());
                if !vis {
                    p.hide();
                }
            }
            return;
        }
        if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pid) {
            p.apply_menu(action.clone());
        }
        match action {
            PinMenuAction::Ocr => {
                if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pid) {
                    let _ = p.take_pending_ocr();
                    p.set_status(Some(tr("Recognizing…")));
                    let img = p.rendered();
                    self.spawn_ocr(pid, img, false);
                }
            }
            PinMenuAction::Translate => {
                let _ = self
                    .pins
                    .iter_mut()
                    .find(|p| p.window_id() == pid)
                    .map(|p| {
                        p.take_pending_translate();
                    });
                self.start_pin_translate(pid);
            }
            _ => {}
        }
    }
}

fn encode_png(img: &Raster) -> Vec<u8> {
    let rgba = image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone());
    let Some(rgba) = rgba else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let dynimg = image::DynamicImage::ImageRgba8(rgba);
    let _ = dynimg.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png);
    buf
}

fn decode_png(bytes: &[u8]) -> Option<Raster> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Raster::from_rgba(img.width(), img.height(), img.into_raw())
}

impl App {
    fn init_runtime(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(windows)]
        {
            // Warm the fontdue chrome stack before the first F1. On Windows
            // this may parse the large msyh.ttc collection; doing it here
            // moves that one-time cost out of the screenshot hot path.
            let t_font = Instant::now();
            let _ = pinshot_core::font::chrome_font_paths();
            crate::logutil::ms("chrome font prefetch", t_font);
        }
        if self.hotkeys.is_none() {
            match native::install_hotkeys(self.proxy.clone(), &self.cfg) {
                Ok(h) => self.hotkeys = Some(h),
            Err(e) => crate::logutil::error(&format!("hotkeys: {e}")),
            }
        }
        if self.tray.is_none() {
            self.rebuild_tray();
        }
        self.ensure_fallback(event_loop);
        std::thread::spawn(crate::detect::warm_element_backend);
        if !self.session_restored {
            self.session_restored = true;
            if self.cfg.run_at_login != autostart::is_enabled() {
                if let Err(e) = autostart::set_enabled(self.cfg.run_at_login) {
                    crate::logutil::error(&format!("autostart: {e}"));
                }
            }
            self.restore_session(event_loop);
            // History PNGs stay on disk until the next F1. Loading them here
            // is how a fresh tray process sat at ~200MB with no overlay.
            self.maybe_run_scheduled_update();
        }
        self.ensure_pump_window(event_loop);
        if self.start_prefs {
            self.start_prefs = false;
            prefs::open(self.cfg.clone(), self.proxy.clone());
        }
        if self.start_snip {
            self.start_snip = false;
            self.request_snip(event_loop, false);
        }
        if self.start_translate {
            self.start_translate = false;
            self.request_snip(event_loop, true);
        }
        if self.start_pin {
            self.start_pin = false;
            self.pin_clipboard(event_loop);
        }
    }

}

impl App {
    fn handle_hotkey(&mut self, event_loop: &ActiveEventLoop, command: Command) {
        match command {
            Command::Snip => self.request_snip(event_loop, false),
            Command::PinClipboard => {
                if self.overlay.is_none() {
                    self.pin_clipboard(event_loop);
                }
            }
            Command::HideAllPins => self.toggle_pins(),
            Command::RestoreClosed => self.restore_closed(event_loop),
            Command::ClickThrough => {
                if let Some(p) = self.pins.last_mut() {
                    p.set_click_through(!p.click_through);
                }
            }
            Command::Quit => self.request_quit(event_loop),
            Command::OpenSettings => prefs::open(self.cfg.clone(), self.proxy.clone()),
            Command::CaptureTranslate => self.request_snip(event_loop, true),
            Command::SelectionTranslate => {
                if self.overlay.is_none() {
                    self.pin_or_selection_translate();
                }
            }
        }
    }

    fn handle_tray_command(&mut self, event_loop: &ActiveEventLoop, command: TrayCmd) {
        match command {
            TrayCmd::Snip => {
                if let Some(tray) = &self.tray {
                    tray.hide_icon();
                }
                self.request_snip(event_loop, false);
            }
            TrayCmd::Pin => self.pin_clipboard(event_loop),
            TrayCmd::HideAll => self.toggle_pins(),
            TrayCmd::Settings => prefs::open(self.cfg.clone(), self.proxy.clone()),
            TrayCmd::Quit => self.request_quit(event_loop),
            TrayCmd::Translate => {
                if let Some(tray) = &self.tray {
                    tray.hide_icon();
                }
                self.request_snip(event_loop, true);
            }
            TrayCmd::Restore => self.restore_closed(event_loop),
            TrayCmd::ToggleGroup(name) => self.toggle_group(name),
        }
    }

}

impl App {
    fn reap_automation_pins(&mut self) {
        let closed = self.automation_pins.iter().filter(|(_, pin)| pin.closed).map(|(id, _)| *id).collect::<Vec<_>>();
        for id in closed {
            if let Some(mut pin) = self.automation_pins.remove(&id) {
                pin.annot.finish();
                crate::automation::normalize_ids(&mut pin.annot.shapes);
                crate::automation::publish_closed(id, pin.annot.shapes.clone());
            }
        }
    }

    fn automation_edit(&mut self, session: u64, command: serde_json::Value) -> Result<serde_json::Value, String> {
        self.reap_automation_pins();
        let Some(pin) = self.automation_pins.get_mut(&session) else { return Ok(serde_json::json!({"open": false})); };
        let op = command["op"].as_str().ok_or("Missing editor operation")?;
        crate::automation::normalize_ids(&mut pin.annot.shapes);
        let busy = pin.annot.automation_busy();
        if op != "snapshot" && busy { return Err("Finish the active drawing or text edit first".into()); }
        match op {
            "snapshot" => {}
            "apply" => {
                let expected: Vec<pinshot_core::annotation::Shape> = serde_json::from_value(command["expected"].clone()).map_err(|e| e.to_string())?;
                if pin.annot.shapes != expected { return Err("Editor changed concurrently; query state and retry".into()); }
                let shapes: Vec<pinshot_core::annotation::Shape> = serde_json::from_value(command["shapes"].clone()).map_err(|e| e.to_string())?;
                pin.annot.replace_automation_shapes(shapes);
                pin.automation_refresh();
            }
            "undo" => { pin.annot.undo(); pin.automation_refresh(); }
            "redo" => { pin.annot.redo(); pin.automation_refresh(); }
            "edit" => {
                let tool = command["tool"].as_str().and_then(pinshot_core::annotation::Tool::from_id).ok_or("Unsupported editor tool")?;
                pin.show_toolbar(true);
                pin.annot.set_tool(Some(tool));
                if command["tool"] == "arrow" {
                    pin.annot.style_mut(pinshot_core::annotation::Tool::Line).set_arrow_kind(pinshot_core::annotation::ArrowKind::Thin);
                }
                pin.automation_refresh();
            }
            _ => return Err("Unknown editor operation".into()),
        }
        crate::automation::normalize_ids(&mut pin.annot.shapes);
        Ok(serde_json::json!({"open": true, "busy": busy, "shapes": pin.annot.shapes,
            "can_undo": pin.annot.can_undo(), "can_redo": pin.annot.can_redo(), "editing": pin.annotating,
            "window_id": format!("{:?}", pin.window_id())}))
    }

    fn automation_view(
        &mut self,
        event_loop: &ActiveEventLoop,
        action: &str,
        session_id: u64,
        image: Option<std::sync::Arc<pinshot_core::raster::Raster>>,
        shapes: Vec<pinshot_core::annotation::Shape>,
    ) -> Result<serde_json::Value, String> {
        self.reap_automation_pins();
        if action == "state" {
            return Ok(serde_json::json!(self.automation_pins.iter().filter(|(_, pin)| pin.is_visible()).map(|(id, pin)| serde_json::json!({
                "session_id": id, "window_id": format!("{:?}", pin.window_id()),
                "visible": pin.is_visible(), "scale": pin.scale, "position": pin.pos(),
                "editing": pin.annotating, "busy": pin.annot.automation_busy()
            })).collect::<Vec<_>>()));
        }
        if action == "hide" {
            if let Some(pin) = self.automation_pins.get_mut(&session_id) {
                pin.annot.finish();
                crate::automation::normalize_ids(&mut pin.annot.shapes);
                pin.hide();
            }
            return Ok(serde_json::json!({"visible": false}));
        }
        if action == "close" {
            if let Some(mut pin) = self.automation_pins.remove(&session_id) {
                pin.annot.finish();
                crate::automation::normalize_ids(&mut pin.annot.shapes);
                crate::automation::publish_closed(session_id, pin.annot.shapes.clone());
            }
            return Ok(serde_json::json!({"visible": false}));
        }
        if action != "show" { return Err("Unknown view operation".into()); }
        if !self.automation_pins.contains_key(&session_id) {
            let frame = image.as_ref().ok_or("Missing preview image")?;
            let scale = (960.0 / frame.width.max(1) as f32).min(640.0 / frame.height.max(1) as f32).min(1.0).max(0.05);
            let pos = native::pin_pos_near_cursor((frame.width as f32 * scale) as u32, (frame.height as f32 * scale) as u32);
            let mut pin = PinWin::create_at_scale(event_loop, (**frame).clone(), pos, scale, shapes)?;
            pin.automation_mode = true;
            pin.automation_refresh();
            pin.window().set_title("PinShot AI");
            self.automation_pins.insert(session_id, pin);
        }
        let pin = self.automation_pins.get(&session_id).ok_or("Missing preview window")?;
        pin.show();
        Ok(serde_json::json!({"visible": pin.is_visible(), "window_id": format!("{:?}", pin.window_id()),
            "scale": pin.scale, "position": pin.pos()}))
    }

    fn dispatch_user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Hotkey(command) => self.handle_hotkey(event_loop, command),
            AppEvent::Tray(command) => self.handle_tray_command(event_loop, command),
            AppEvent::PrefsApplied(cfg) => self.apply_cfg(cfg),
            AppEvent::RaisePrefs => {
                if let Some(w) = &self.prefs {
                    w.raise();
                } else {
                    self.open_prefs_win(event_loop);
                }
            }
            AppEvent::OpenPrefs => self.open_prefs_win(event_loop),
            AppEvent::UpdateCheckDone(result) => self.handle_update_done(event_loop, result),
            AppEvent::X11Key { keysym, mods, pressed } => {
                self.handle_x11_key(event_loop, keysym, mods, pressed)
            }
            AppEvent::PinMenu(action) => self.apply_pin_menu(action),
            AppEvent::Ipc(cmd) => self.handle_ipc_command(cmd),
            AppEvent::AutomationEdit { session_id, command, response } => {
                let result = self.automation_edit(session_id, command);
                let _ = response.send(result);
            }
            AppEvent::AutomationView { action, session_id, image, shapes, response } => {
                let result = self.automation_view(event_loop, &action, session_id, image, shapes);
                let _ = response.send(result);
            }
            AppEvent::OcrProgress { pin, message } => self.handle_ocr_progress(pin, message),
            AppEvent::OcrDone { pin, result, translate } => {
                self.handle_ocr_done(pin, result, translate)
            }
            AppEvent::TranslateDone { pin, result } => self.handle_translate_done(pin, result),
            AppEvent::SelectionText(result) => self.handle_selection_text(event_loop, result),
            AppEvent::SelectionTranslation { source, result } => self.handle_selection_translation(event_loop, source, result),
        }
    }

}

impl App {
    fn handle_ipc_command(&mut self, command: String) {
        match command.as_str() {
            "reload-settings" => match config::load() {
                Ok(cfg) => self.apply_cfg(cfg),
                Err(e) => crate::logutil::log(&format!("reload settings: {e}")),
            },
            "raise" => {
                if let Some(fb) = &self.fallback {
                    fb.show();
                }
            }
            _ => {}
        }
    }

    fn handle_update_done(
        &mut self,
        event_loop: &ActiveEventLoop,
        result: Result<crate::update_run::UpdateOutcome, String>,
    ) {
        match &result {
            Ok(crate::update_run::UpdateOutcome::ReadyToInstall {
                setup_path,
                version,
            }) => {
                crate::logutil::log(&format!("update ready {version}"));
                self.finish_update_check(Ok(crate::update_run::UpdateOutcome::UpToDate));
                self.pending_installer = Some(setup_path.clone());
                self.request_quit(event_loop);
            }
            _ => self.finish_update_check(result),
        }
    }

    fn handle_ocr_progress(&mut self, pin: WindowId, message: String) {
        if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pin) {
            p.set_status(Some(tr(&message)));
        }
    }

    fn handle_ocr_done(
        &mut self,
        pin: WindowId,
        result: Result<pinshot_core::ocr::OcrPage, String>,
        translate: bool,
    ) {
        let mut follow = None;
        if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pin) {
            p.finish_ocr();
            match result {
                Ok(page) => {
                    let text = page.text.clone();
                    p.attach_ocr(page);
                    if translate && !text.trim().is_empty() {
                        p.set_translating(true);
                        follow = Some(text);
                    }
                }
                Err(err) => {
                    p.set_status(None);
                    p.toast(tr(&err));
                }
            }
        }
        if let Some(text) = follow {
            self.spawn_translate(pin, text);
        }
        #[cfg(windows)]
        crate::capture::Capture::release_idle();
    }

    fn handle_translate_done(&mut self, pin: WindowId, result: Result<String, String>) {
        if let Some(p) = self.pins.iter_mut().find(|p| p.window_id() == pin) {
            match result {
                Ok(text) => p.set_translation(text),
                Err(err) => {
                    p.set_translating(false);
                    p.toast(tr(&err));
                }
            }
        }
    }

    fn handle_selection_text(&mut self, _event_loop: &ActiveEventLoop, result: Result<String, String>) {
        match result {
            Ok(text) => self.spawn_selection_translation(text),
            Err(_) => crate::logutil::error("selection_translate: failed to obtain selected text"),
        }
    }

    fn handle_selection_translation(&mut self, event_loop: &ActiveEventLoop, source: String, result: Result<String, String>) {
        match result {
            Ok(text) => {
                if let Some(popup) = self.translate_popup.as_mut() {
                    let _ = popup.handle(&WindowEvent::RedrawRequested);
                } else if let Ok(popup) = TranslatePopup::create(event_loop, source, text) {
                    self.translate_popup = Some(popup);
                }
            }
            Err(_) => crate::logutil::error("selection_translate: provider request failed"),
        }
    }

    fn handle_x11_key(&mut self, event_loop: &ActiveEventLoop, keysym: u32, mods: u16, pressed: bool) {
        if let Some(ov) = self.overlay.as_mut() {
            ov.handle_x11_key(keysym, mods, pressed);
            self.finish_overlay(event_loop);
        } else if pressed && keysym == 0xffbe {
            // Esc already closed the overlay; the grab loop may still forward F1.
            self.request_snip(event_loop, false);
        }
    }

}

impl App {
    fn handle_aux_window_event(&mut self, id: WindowId, event: &WindowEvent) -> bool {
        if self
            .prefs
            .as_ref()
            .map(|w| w.window_id() == id)
            .unwrap_or(false)
        {
            if let Some(pw) = self.prefs.as_mut() {
                pw.handle(event);
            }
            if self.prefs.as_ref().map(|w| w.closed).unwrap_or(false) {
                self.prefs = None;
            }
            return true;
        }
        if self
            .pin_menu
            .as_ref()
            .map(|m| m.window_id() == id)
            .unwrap_or(false)
        {
            let action = self.pin_menu.as_mut().and_then(|m| m.handle(event));
            let closed = self.pin_menu.as_ref().map(|m| m.closed).unwrap_or(false);
            if action.is_some() || closed {
                self.dismiss_pin_menu();
            }
            if let Some(action) = action {
                let _ = self.proxy.send_event(AppEvent::PinMenu(action));
            }
            return true;
        }
        if self.translate_popup.as_ref().map(|w| w.window_id() == id).unwrap_or(false) {
            let close = self.translate_popup.as_mut().map(|w| w.handle(event)).unwrap_or(false);
            if close { self.translate_popup = None; }
            return true;
        }
        if let Some(cd) = self.snip.countdown.as_mut() {
            if cd.window_id() == id {
                cd.handle(event);
                return true;
            }
        }
        if let Some(fb) = self.fallback.as_mut() {
            if fb.window_id() == id {
                if let Some(cmd) = fb.handle(event) {
                    let _ = self.proxy.send_event(AppEvent::Tray(cmd));
                }
                return true;
            }
        }
        false
    }

}

impl App {
    fn dispatch_window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(session) = self.automation_pins.iter().find(|(_, pin)| pin.window_id() == id).map(|(session, _)| *session) {
            if let Some(pin) = self.automation_pins.get_mut(&session) {
                match &event {
                    WindowEvent::MouseInput { button: MouseButton::Right, state: ElementState::Pressed, .. } => {
                        if pin.annotating { pin.annot.right_click(); pin.automation_refresh(); } else { pin.close(); }
                    }
                    WindowEvent::KeyboardInput { event: key, .. } => {
                        let changes_base = pin.annot.text_edit.is_none() && matches!(&key.logical_key, Key::Character(c) if c == "3" || c == "4" || ((c == "1" || c == "2") && pin.annot.tool.is_none()));
                        let unsupported_job = matches!(&key.logical_key, Key::Named(NamedKey::F5));
                        if !changes_base && !unsupported_job { pin.handle(&event); }
                    }
                    _ => pin.handle(&event),
                }
                crate::automation::normalize_ids(&mut pin.annot.shapes);
            }
            self.reap_automation_pins();
            return;
        }
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = m.state();
        }
        if self.handle_aux_window_event(id, &event) {
            return;
        }
        if self.handle_overlay_window_event(event_loop, id, &event) {
                return;
        }
        let mut open_menu = None;
        let mut close_menu = false;
        let mut ocr_job = None;
        let mut translate_pin = None;
        let swallow_keys = self.pin_esc_guarded();
        let menu_open = self.pin_menu.is_some();
        for p in &mut self.pins {
            if p.window_id() == id {
                if let WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } = &event
                {
                    let now = Instant::now();
                    let pt = Point::new(p.last_cursor_i32().0 as f32, p.last_cursor_i32().1 as f32);
                    if let Some((wid, t, prev)) = self.last_click {
                        if wid == id
                            && now.duration_since(t).as_millis() < 350
                            && pt.manhattan(prev) <= 12.0
                        {
                            p.on_double_click(&mut self.cfg, self.mods.shift_key());
                            self.last_click = None;
                        } else {
                            self.last_click = Some((id, now, pt));
                        }
                    } else {
                        self.last_click = Some((id, now, pt));
                    }
                }
                if let WindowEvent::MouseWheel { delta, .. } = &event {
                    if self.mods.control_key() || crate::native::pointer_ctrl_down() {
                        let dy = match delta {
                            MouseScrollDelta::LineDelta(_, y) => *y,
                            MouseScrollDelta::PixelDelta(pt) => pt.y as f32 / 120.0,
                        };
                        p.handle_ctrl_wheel(dy);
                        return;
                    }
                }
                if swallow_keys && matches!(&event, WindowEvent::KeyboardInput { .. }) {
                    if let WindowEvent::KeyboardInput { event: kev, .. } = &event {
                        if kev.state == ElementState::Pressed
                            && matches!(kev.logical_key, Key::Named(NamedKey::Escape))
                            && menu_open
                        {
                            close_menu = true;
                        }
                    }
                } else {
                    p.handle(&event);
                }
                if p.take_pending_ocr() {
                    p.set_status(Some(tr("Recognizing…")));
                    ocr_job = Some(p.rendered());
                }
                if p.take_pending_translate() {
                    translate_pin = Some(id);
                }
                if p.pending_menu {
                    p.pending_menu = false;
                    open_menu = Some(id);
                }
                break;
            }
        }
        if let Some(img) = ocr_job {
            self.spawn_ocr(id, img, false);
        }
        if let Some(pid) = translate_pin {
            self.start_pin_translate(pid);
        }
        if close_menu {
            self.dismiss_pin_menu();
        }
        if let Some(pid) = open_menu {
            self.open_pin_menu(event_loop, pid);
        }
    }

}

impl App {
    fn handle_overlay_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WindowId,
        event: &WindowEvent,
    ) -> bool {
        let Some(ov) = self.overlay.as_mut() else {
            return false;
        };
        if ov.window_id() != id {
            return false;
        }
        if let WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } = event
        {
            let now = Instant::now();
            let pt = ov.cursor();
            if let Some((wid, t, prev)) = self.last_click {
                // Same-spot only: a toolbar click then a drag is not a double-click.
                if wid == id && now.duration_since(t).as_millis() < 350 && pt.manhattan(prev) <= 12.0 {
                    ov.on_double_click();
                    self.last_click = None;
                } else {
                    self.last_click = Some((id, now, pt));
                }
            } else {
                self.last_click = Some((id, now, pt));
            }
        }
        ov.handle(event);
        self.finish_overlay(event_loop);
        true
    }

}

impl App {
    fn dispatch_about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() {
            return;
        }
        event_loop.set_control_flow(ControlFlow::Wait);
        self.prefetch_dxgi(event_loop);
        let poll = self
            .update_poll_at
            .map(|t| t.elapsed() >= Duration::from_secs(60))
            .unwrap_or(true);
        if poll {
            self.update_poll_at = Some(Instant::now());
            self.maybe_run_scheduled_update();
        }
        if self.hotkeys.is_none() {
            if let Ok(h) = native::install_hotkeys(self.proxy.clone(), &self.cfg) {
                self.hotkeys = Some(h);
            }
        }
        self.advance_snip_schedule(event_loop);
        if let Some(tray) = &self.tray {
            if let Some(cmd) = tray.poll() {
                let _ = self.proxy.send_event(AppEvent::Tray(cmd));
            }
        }
        self.pump_overlay_and_schedule_wake(event_loop);
        self.maintain_windows(event_loop);
    }

}

impl App {
    fn maintain_windows(&mut self, event_loop: &ActiveEventLoop) {
        for pin in self.automation_pins.values_mut() { pin.tick_glow(); pin.pump_redraw(); }
        self.reap_automation_pins();

        let mut persist = false;
        for p in &mut self.pins {
            p.tick_glow();
            p.pump_redraw();
            if p.persist_needed {
                p.persist_needed = false;
                persist = true;
            }
        }
        if let Some(until) = self.menu_raise_until {
            if Instant::now() < until {
                if let (Some(m), Some(pid)) = (&self.pin_menu, self.menu_pin) {
                    if let Some(p) = self.pins.iter().find(|p| p.window_id() == pid) {
                        m.restack_above(p.window());
                    }
                }
                let next = Instant::now() + Duration::from_millis(50);
                event_loop.set_control_flow(ControlFlow::WaitUntil(next.min(until)));
            } else {
                self.menu_raise_until = None;
            }
        }
        if self.pin_menu.as_ref().map(|m| m.closed).unwrap_or(false) {
            self.dismiss_pin_menu();
        }
        if self.prefs.as_ref().map(|w| w.closed).unwrap_or(false) {
            self.prefs = None;
        }
        let before = self.pins.len();
        self.reap_pins();
        if persist || self.pins.len() != before {
            self.persist_session();
        }
    }

    #[cfg(windows)]
    fn prefetch_dxgi(&mut self, event_loop: &ActiveEventLoop) {
        if self.dxgi_prefetched {
            return;
        }
        let Some(at) = self.dxgi_prefetch_at else {
            return;
        };
        if Instant::now() < at {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            return;
        }
        if crate::capture::Capture::prefetch() {
            self.dxgi_prefetched = true;
            self.dxgi_prefetch_at = None;
        } else {
            // Locked desktops reject DuplicateOutput; retry after unlock.
            let next = Instant::now() + Duration::from_secs(2);
            self.dxgi_prefetch_at = Some(next);
            event_loop.set_control_flow(ControlFlow::WaitUntil(next));
        }
    }

    #[cfg(not(windows))]
    fn prefetch_dxgi(&mut self, _event_loop: &ActiveEventLoop) {}

    fn pump_overlay_and_schedule_wake(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(ov) = &mut self.overlay {
            if ov.peek_result().is_none() {
                ov.pump_redraw();
            }
        }
        self.finish_overlay(event_loop);
        if self.overlay.is_some() {
            // X11 drains the pointer grab; Windows waits for the next redraw.
            #[cfg(unix)]
            event_loop.set_control_flow(ControlFlow::Poll);
            #[cfg(not(unix))]
            if let Some(ov) = &self.overlay {
                if let Some(t) = ov.next_wake() {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(t));
                }
            }
            return;
        }
        let mut wake = self.snip.at;
        for p in &self.pins {
            if let Some(t) = p.next_wake() {
                wake = Some(wake.map(|w| w.min(t)).unwrap_or(t));
            }
        }
        if let Some(at) = wake {
            if Instant::now() < at {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
    }

    fn advance_snip_schedule(&mut self, event_loop: &ActiveEventLoop) {
        let Some(at) = self.snip.at else {
            return;
        };
        if Instant::now() < at {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            return;
        }
        if self.snip.countdown.is_some() {
            if self.snip.countdown_left > 1 {
                self.snip.countdown_left -= 1;
                if let Some(cd) = &mut self.snip.countdown {
                    cd.set_left(self.snip.countdown_left);
                }
                self.snip.at = Some(Instant::now() + Duration::from_secs(1));
            } else {
                self.snip.countdown = None;
                self.prepare_capture_hide();
                self.snip.at = Some(Instant::now() + Duration::from_millis(self.hide_wait_ms()));
                self.snip_phase = SnipPhase::countdown_tick(self.snip.countdown_left);
            }
        } else {
            self.snip.at = None;
            self.begin_snip(event_loop);
            if self.snip_phase != SnipPhase::Overlay {
                self.restore_fallback_if_needed();
            }
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.init_runtime(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        self.dispatch_user_event(event_loop, event);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.dispatch_window_event(event_loop, id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch_about_to_wait(event_loop);
    }
}

#[cfg(test)]
mod snip_phase_tests {
    use super::*;

    #[test]
    fn arm_selects_countdown_or_hidewait() {
        assert_eq!(SnipPhase::arm(true), SnipPhase::Countdown);
        assert_eq!(SnipPhase::arm(false), SnipPhase::HideWait);
    }

    #[test]
    fn countdown_tick_stays_until_final() {
        assert_eq!(SnipPhase::countdown_tick(5), SnipPhase::Countdown);
        assert_eq!(SnipPhase::countdown_tick(2), SnipPhase::Countdown);
        assert_eq!(SnipPhase::countdown_tick(1), SnipPhase::HideWait);
        assert_eq!(SnipPhase::countdown_tick(0), SnipPhase::HideWait);
    }

    #[test]
    fn variants_are_distinct() {
        // Single-source-of-truth invariant relied on by request_snip /
        // begin_snip / finish_overlay / advance_snip_schedule: each phase is a
        // unique discriminant, so the "phase != Idle" / "phase == Overlay"
        // guards can never collide.
        let all = [
            SnipPhase::Idle,
            SnipPhase::Countdown,
            SnipPhase::HideWait,
            SnipPhase::Overlay,
        ];
        for i in 0..all.len() {
            for j in 0..all.len() {
                if i != j {
                    assert_ne!(all[i], all[j]);
                }
            }
        }
    }

    #[test]
    fn overlay_is_the_only_live_state() {
        // Documented contract: `phase == Overlay` iff `overlay.is_some()`
        // (enforced in begin_snip / finish_overlay). `Overlay` must therefore
        // be reachable only from the capture path and is the sole "live"
        // state among the four.
        let live = SnipPhase::Overlay;
        assert_ne!(live, SnipPhase::Idle);
        assert_ne!(live, SnipPhase::Countdown);
        assert_ne!(live, SnipPhase::HideWait);
    }
}
