//! Settings window (egui). Spawned as `--settings-window` so it has its own
//! event loop: eframe on a second thread after `gtk::init` fails on Linux, and
//! winit only allows one loop per process on Windows.
//!
//! Tab names and page layout follow 参考工具 Preferences (public UI):
//! General / Interface / Snip / Paste / Output / Control / About.
//! PinShot-only keys stay; no 参考工具-only options are invented.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use eframe::egui::{self, Color32, RichText, Ui, Visuals};
use pinshot_core::config::{self, Config};
use pinshot_core::groups;
use pinshot_core::hotkey::{bindings_from_config, find_conflicts, normalize_seq};
use pinshot_core::i18n::{self, tr, Lang};
use pinshot_core::VERSION;
use winit::event_loop::EventLoopProxy;

use crate::event::AppEvent;

static OPEN: AtomicBool = AtomicBool::new(false);
static CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// The preferences window keeps a stable width, while its height follows the
/// selected tab's content.
const PREFS_W: f32 = 540.0;
const PREFS_MIN_W: f32 = 420.0;
const PREFS_MIN_H: f32 = 220.0;
const PREFS_MAX_H: f32 = 760.0;
const FOOTER_H: f32 = 48.0;
const FORM_LABEL_W: f32 = 152.0;
const ROW_H: f32 = 32.0;
const FORM_GAP: f32 = 12.0;
const CONTENT_MARGIN: i8 = 16;
const CONTROL_W: f32 = PREFS_W - 2.0 * CONTENT_MARGIN as f32 - FORM_LABEL_W - FORM_GAP;
const ACTION_W: f32 = 80.0;

fn prefs_inner_size() -> egui::Vec2 {
    egui::vec2(PREFS_W, PREFS_MIN_H)
}

const TABS: &[&str] = &[
    "General",
    "Interface",
    "Snip",
    "Paste",
    "Output",
    "Control",
    "Translate",
    "Update",
    "About",
];

pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

#[cfg(windows)]
pub fn settings_has_focus() -> bool {
    let child = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    let Some(child) = child.as_ref() else { return false; };
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == child.id()
    }
}

pub fn mark_closed() {
    OPEN.store(false, Ordering::SeqCst);
}

pub fn open(cfg: Config, proxy: EventLoopProxy<AppEvent>) {
    crate::logutil::log("prefs: open requested");
    let _ = (cfg, &proxy);
    let mut child = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(proc) = child.as_mut() {
        if matches!(proc.try_wait(), Ok(None)) {
            raise_settings_window(proc.id());
            return;
        }
    }
    match std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .arg("--settings-window")
            .spawn()
    }) {
        Ok(proc) => {
            *child = Some(proc);
            OPEN.store(true, Ordering::SeqCst);
        }
        Err(e) => {
            crate::logutil::error(&format!("settings launch: {e}"));
            let _ = proxy.send_event(AppEvent::OpenPrefs);
        }
    }
}

/// Tray / hotkey quit: the settings child is a separate process (own HWND /
/// default exe icon). Kill it so it cannot outlive the tray.
pub fn close_settings() {
    let mut child = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut proc) = child.take() {
        let _ = proc.kill();
        let _ = proc.wait();
    }
    OPEN.store(false, Ordering::SeqCst);
}

fn raise_settings_window(pid: u32) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;
        unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == data.0 as u32 && IsWindowVisible(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
        unsafe {
            let _ = EnumWindows(Some(visit), LPARAM(pid as isize));
        }
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("xdotool")
            .args(["search", "--pid", &pid.to_string(), "windowactivate"])
            .status();
    }
}

pub fn run_preferences() {
    crate::logutil::init(&[]);
    // Do not gtk::init here: eframe/winit on the same process after GTK
    // init is what made the old Linux settings window fail silently.
    #[cfg(windows)]
    crate::native::init_process();
    let cfg = config::load().unwrap_or_default();
    i18n::set_lang(cfg.lang());
    let size = prefs_inner_size();
    let (iw, ih, rgba) = pinshot_core::icons::app_icon_rgba(32);
    let icon = egui::IconData {
        rgba,
        width: iw,
        height: ih,
    };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(tr("PinShot Preferences"))
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([PREFS_MIN_W, PREFS_MIN_H])
            .with_max_inner_size([PREFS_W, PREFS_MAX_H])
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_icon(icon),
        persist_window: false,
        centered: true,
        ..Default::default()
    };
    #[cfg(windows)]
    std::thread::spawn(|| {
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(40));
            crate::win32::restyle_prefs_windows();
        }
    });
    if let Err(e) = eframe::run_native(
        "PinShot Preferences",
        opts,
        Box::new(move |cc| {
            crate::ui_font::install_cjk(&cc.egui_ctx);
            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 10.0);
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            style.spacing.interact_size.y = ROW_H;
            style.spacing.combo_height = 240.0;
            cc.egui_ctx.set_style(style);
            apply_theme(&cc.egui_ctx, &cfg.theme);
            let tab = std::env::var("PINSHOT_PREFS_TAB")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0)
                .min(TABS.len() - 1);
            Ok(Box::new(PrefsApp {
                draft: cfg.clone(),
                saved: cfg,
                tab,
                group_sel: 0,
                group_edit: String::new(),
                status: String::new(),
                test_rx: None,
                ocr_dl_rx: None,
                ocr_dl_frac: 0.0,
                update_check_rx: None,
                models: Vec::new(),
                models_provider: String::new(),
                models_rx: None,
            }))
        }),
    ) {
        crate::logutil::error(&format!("settings window: {e}"));
    }
    OPEN.store(false, Ordering::SeqCst);
}

fn apply_theme(ctx: &egui::Context, theme: &str) {
    let dark = match theme {
        "dark" => true,
        "light" => false,
        _ => system_dark(),
    };
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let palette = crate::draw::UiTheme::resolve(if dark { "dark" } else { "light" });
    let color = |c: pinshot_core::color::Rgba| Color32::from_rgb(c.r, c.g, c.b);
    visuals.panel_fill = color(palette.bg);
    visuals.window_fill = color(palette.bg);
    visuals.faint_bg_color = color(palette.panel);
    visuals.override_text_color = Some(color(palette.text));
    visuals.widgets.inactive.bg_fill = color(palette.panel);
    visuals.widgets.inactive.weak_bg_fill = color(palette.panel);
    visuals.widgets.hovered.bg_fill = color(palette.hover);
    visuals.widgets.hovered.weak_bg_fill = color(palette.hover);
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, color(palette.border));
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, color(palette.accent));
    visuals.widgets.active.bg_fill = color(palette.hover);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, color(palette.accent));
    visuals.selection.bg_fill = if dark { Color32::from_rgb(38, 69, 104) } else { Color32::from_rgb(222, 237, 252) };
    visuals.selection.stroke = egui::Stroke::new(1.0, color(palette.accent));
    for widget in [&mut visuals.widgets.inactive, &mut visuals.widgets.hovered, &mut visuals.widgets.active] {
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    ctx.set_visuals(visuals);
}

fn system_dark() -> bool {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::System::Registry::{
            RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
        };
        unsafe {
            let mut key = Default::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                0,
                KEY_READ,
                &mut key,
            )
            .is_err()
            {
                return false;
            }
            let mut ty = REG_DWORD;
            let mut val: u32 = 1;
            let mut n = std::mem::size_of::<u32>() as u32;
            let _ = RegQueryValueExW(
                key,
                w!("AppsUseLightTheme"),
                None,
                Some(&mut ty),
                Some((&mut val as *mut u32).cast()),
                Some(&mut n),
            );
            let _ = windows::Win32::System::Registry::RegCloseKey(key);
            return val == 0;
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var("GTK_THEME")
            .or_else(|_| std::env::var("GTK_THEME_VARIANT"))
            .map(|s| s.to_ascii_lowercase().contains("dark"))
            .unwrap_or(false)
    }
}

fn reveal_dir(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

enum OcrDlUpdate {
    Progress(f32),
    Done(Result<(), String>),
}

enum UpdateCheckMsg {
    Done(Result<crate::update_run::UpdateOutcome, String>),
}

struct PrefsApp {
    draft: Config,
    saved: Config,
    tab: usize,
    group_sel: usize,
    group_edit: String,
    status: String,
    test_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    ocr_dl_rx: Option<std::sync::mpsc::Receiver<OcrDlUpdate>>,
    ocr_dl_frac: f32,
    update_check_rx: Option<std::sync::mpsc::Receiver<UpdateCheckMsg>>,
    models: Vec<String>,
    models_provider: String,
    models_rx: Option<std::sync::mpsc::Receiver<Result<Vec<String>, String>>>,
}

impl PrefsApp {
    fn apply(&mut self) -> bool {
        self.draft.hotkey_snip = normalize_seq(&self.draft.hotkey_snip);
        self.draft.hotkey_pin = normalize_seq(&self.draft.hotkey_pin);
        self.draft.hotkey_hide_all = normalize_seq(&self.draft.hotkey_hide_all);
        self.draft.hotkey_restore = normalize_seq(&self.draft.hotkey_restore);
        self.draft.hotkey_clickthrough = normalize_seq(&self.draft.hotkey_clickthrough);
        self.draft.hotkey_selection_translate =
            normalize_seq(&self.draft.hotkey_selection_translate);
        self.draft.hotkey_capture_translate = normalize_seq(&self.draft.hotkey_capture_translate);
        let conflicts = find_conflicts(&bindings_from_config(&self.draft));
        if !conflicts.is_empty() {
            self.status = format!(
                "{}: {} / {}",
                tr("Conflicting hotkeys"),
                conflicts[0].0,
                conflicts[0].1
            );
            return false;
        }
        self.draft = self.draft.clone().validate();
        if let Err(e) = config::save(&self.draft) {
            crate::logutil::error(&format!("save settings: {e}"));
            self.status = tr("Could not save settings");
            return false;
        }
        crate::logutil::set_level(&self.draft.log_level);
        self.saved = self.draft.clone();
        i18n::set_lang(self.draft.lang());
        match crate::ipc::send("reload-settings") {
            Ok(()) => self.status.clear(),
            Err(e) => {
                crate::logutil::error(&format!("apply settings: {e}"));
                self.status = tr("Saved. Restart PinShot to apply.");
            }
        }
        true
    }

    fn restore_tab(&mut self) {
        let d = Config::default();
        match self.tab {
            0 => {
                self.draft.language = d.language;
                self.draft.run_at_login = d.run_at_login;
                self.draft.start_minimized = d.start_minimized;
                self.draft.log_level = d.log_level;
                self.draft.ocr_model = d.ocr_model;
            }
            1 => {
                self.draft.theme = d.theme;
            }
            2 => {
                self.draft.snip_delay = d.snip_delay;
                self.draft.skip_abort_confirm = d.skip_abort_confirm;
                self.draft.history_max = d.history_max;
                self.draft.dim_alpha = d.dim_alpha;
            }
            3 => {
                self.draft.dblclick_pin_action = d.dblclick_pin_action;
                self.draft.closed_pins_max = d.closed_pins_max;
                self.draft.groups = d.groups;
                self.group_sel = 0;
                self.group_edit.clear();
            }
            4 => {
                self.draft.quick_save_folder = d.quick_save_folder;
                self.draft.image_quality = d.image_quality;
            }
            5 => {
                self.draft.hotkey_snip = d.hotkey_snip;
                self.draft.hotkey_pin = d.hotkey_pin;
                self.draft.hotkey_hide_all = d.hotkey_hide_all;
                self.draft.hotkey_restore = d.hotkey_restore;
                self.draft.hotkey_selection_translate = d.hotkey_selection_translate;
                self.draft.hotkey_capture_translate = d.hotkey_capture_translate;
                self.draft.hotkey_clickthrough = d.hotkey_clickthrough;
            }
            6 => {
                self.draft.translate = d.translate;
            }
            7 => {
                self.draft.update = d.update;
            }
            _ => {}
        }
        i18n::set_lang(self.draft.lang());
        self.status = tr("Restored this page to defaults.");
    }
}

impl eframe::App for PrefsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.test_rx {
            match rx.try_recv() {
                Ok(Ok(text)) => {
                    let preview: String = text.chars().take(80).collect();
                    self.status = format!("{} {preview}", tr("Translation OK:"));
                    self.test_rx = None;
                }
                Ok(Err(e)) => {
                    self.status = format!("{} {e}", tr("Translation failed:"));
                    self.test_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.status = tr("Translation failed:");
                    self.test_rx = None;
                }
            }
        }
        let mut ocr_finished = None;
        if let Some(rx) = self.models_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(models)) => { self.models = models; self.models_rx = None; self.status = tr("Models refreshed."); }
                Ok(Err(e)) => { self.models_rx = None; self.status = format!("{} {e}", tr("Model refresh failed:")); }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(80)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.models_rx = None,
            }
        }
        if let Some(rx) = self.ocr_dl_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(OcrDlUpdate::Progress(f)) => self.ocr_dl_frac = f.clamp(0.0, 1.0),
                    Ok(OcrDlUpdate::Done(r)) => {
                        ocr_finished = Some(r);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        ocr_finished = Some(Err("download stopped".into()));
                        break;
                    }
                }
            }
        }
        if let Some(r) = ocr_finished {
            self.ocr_dl_rx = None;
            match r {
                Ok(()) => {
                    self.ocr_dl_frac = 1.0;
                    self.status = tr("OCR models downloaded.");
                }
                Err(e) => {
                    self.ocr_dl_frac = 0.0;
                    self.status = format!("{} {e}", tr("OCR download failed:"));
                }
            }
        }
        if self.ocr_dl_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if let Some(rx) = self.update_check_rx.as_ref() {
            match rx.try_recv() {
                Ok(UpdateCheckMsg::Done(r)) => {
                    self.update_check_rx = None;
                    self.draft.update.last_check_unix = pinshot_core::update::now_unix();
                    if let Err(e) = config::record_update_check(self.draft.update.last_check_unix) {
                        crate::logutil::error(&format!("save update timestamp: {e}"));
                    }
                    let install = match &r {
                        Ok(crate::update_run::UpdateOutcome::ReadyToInstall { setup_path, .. }) => {
                            Some(setup_path.clone())
                        }
                        _ => None,
                    };
                    self.status = match r {
                        Ok(crate::update_run::UpdateOutcome::UpToDate) => {
                            tr("You are on the latest version.")
                        }
                        Ok(crate::update_run::UpdateOutcome::UpdateAvailable { version }) => {
                            format!("{} {version}", tr("Update available:"))
                        }
                        Ok(crate::update_run::UpdateOutcome::ReadyToInstall { version, .. }) => {
                            format!("{} {version}", tr("Installing update…"))
                        }
                        Err(e) => format!("{} {e}", tr("Update check failed:")),
                    };
                    if let Some(path) = install {
                        if let Err(e) = crate::update_run::defer_install_after_quit(path) {
                            self.status = format!("{} {e}", tr("Update check failed:"));
                            return;
                        }
                        std::process::exit(0);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_check_rx = None;
                }
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(tr("PinShot Preferences")));
        apply_theme(ctx, &self.draft.theme);
        egui::TopBottomPanel::bottom("btns")
            .exact_height(FOOTER_H)
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(CONTENT_MARGIN, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if TABS.get(self.tab) != Some(&"About")
                            && ui.button(tr("Restore Defaults")).clicked()
                        {
                            self.restore_tab();
                        }
                        if !self.status.is_empty() {
                            ui.add(egui::Label::new(&self.status).truncate())
                                .on_hover_text(&self.status);
                        }
                    });
                });
            });
        if self.draft != self.saved {
            self.apply();
        }
        let mut content_height = 0.0_f32;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(CONTENT_MARGIN, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(3.0, 0.0);
                    ui.spacing_mut().button_padding = egui::vec2(3.0, 4.0);
                    let tab_width = (ui.available_width() - (TABS.len() - 1) as f32 * 3.0)
                        / TABS.len() as f32;
                    for (i, name) in TABS.iter().enumerate() {
                        let selected = self.tab == i;
                        if ui
                            .add_sized(
                                [tab_width, ROW_H + 4.0],
                                egui::SelectableLabel::new(
                                    selected,
                                    RichText::new(tr(name)).size(11.5),
                                ),
                            )
                            .clicked()
                        {
                            self.tab = i;
                        }
                    }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                match self.tab {
                    0 => self.ui_general(ui),
                    1 => self.ui_interface(ui),
                    2 => self.ui_snip(ui),
                    3 => self.ui_paste(ui),
                    4 => self.ui_output(ui),
                    5 => self.ui_control(ui),
                    6 => self.ui_translate(ui),
                    7 => self.ui_update(ui),
                    _ => self.ui_about(ui),
                };
                // `min_rect().max.y` can include the whole available panel and
                // therefore reports the old window height. The cursor is the
                // actual end of the controls rendered by the selected tab.
                content_height = ui.cursor().min.y;
            });
        let desired_height = (content_height + FOOTER_H + 10.0)
            .clamp(PREFS_MIN_H, PREFS_MAX_H);
        let current_height = ctx.input(|i| {
            i.viewport()
                .inner_rect
                .map(|r| r.height())
                .unwrap_or(desired_height)
        });
        if (current_height - desired_height).abs() > 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                PREFS_W,
                desired_height,
            )));
        }
    }
}

impl PrefsApp {
    fn ui_general(&mut self, ui: &mut Ui) {
        form_grid(ui, "prefs_general", |ui| {
            form_combo(ui, tr("Language:"), "lang", self.draft.lang().display_name(), |ui| {
                    ui.selectable_value(
                        &mut self.draft.language,
                        "zh_CN".into(),
                        Lang::ZhCn.display_name(),
                    );
                    ui.selectable_value(
                        &mut self.draft.language,
                        "en".into(),
                        Lang::En.display_name(),
                    );
                });
            ui.label("");
            ui.checkbox(&mut self.draft.run_at_login, tr("Run at login"))
                .on_hover_text(tr("Start PinShot at login (on by default)"));
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut self.draft.start_minimized, tr("Start minimized"));
            ui.end_row();
            form_combo(ui, tr("System log:"), "log_level", match self.draft.log_level.as_str() {
                    "off" => tr("Off"),
                    "trace" | "detailed" => tr("Trace"),
                    "debug" => tr("Debug"),
                    _ => tr("Normal"),
                }, |ui| {
                    ui.selectable_value(&mut self.draft.log_level, "off".into(), tr("Off"));
                    ui.selectable_value(&mut self.draft.log_level, "normal".into(), tr("Normal"));
                    ui.selectable_value(&mut self.draft.log_level, "trace".into(), tr("Trace"));
                    ui.selectable_value(&mut self.draft.log_level, "debug".into(), tr("Debug"));
                });
            form_label(ui, tr("OCR mode:"));
            ui.horizontal(|ui| {
            let ocr = match self.draft.ocr_model.as_str() {
                "server" => tr("High accuracy (slower)"),
                _ => tr("Fast (recommended)"),
            };
            let downloading = self.ocr_dl_rx.is_some();
            ui.add_enabled_ui(!downloading, |ui| {
                egui::ComboBox::from_id_salt("ocr_model")
                    .width(CONTROL_W - OCR_DL_BTN_W - 8.0)
                    .icon(combo_chevron)
                    .selected_text(ocr)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.draft.ocr_model,
                            "mobile".into(),
                            tr("Fast (recommended)"),
                        );
                        ui.selectable_value(
                            &mut self.draft.ocr_model,
                            "server".into(),
                            tr("High accuracy (slower)"),
                        );
                    });
            });
            let ready = crate::ocr_run::models_ready_for_tier(&self.draft.ocr_model);
            if ocr_model_download_button(
                ui,
                ready,
                downloading,
                self.ocr_dl_frac,
            ) {
                self.start_ocr_model_download();
            }
            });
            ui.end_row();
        });
        ui.add_space(12.0);
        ui.label(RichText::new(tr("Configuration")).strong());
        ui.add(egui::Label::new(config::config_dir().display().to_string()).wrap());
        ui.horizontal(|ui| {
            if ui.button(tr("Open containing folder")).clicked() {
                reveal_dir(&config::config_dir());
            }
            if ui.button(tr("Open log folder")).clicked() {
                reveal_dir(&config::data_dir());
            }
        });
    }

    fn ui_interface(&mut self, ui: &mut Ui) {
        form_grid(ui, "prefs_iface", |ui| {
            let theme_txt = match self.draft.theme.as_str() {
                "light" => tr("Light"),
                "dark" => tr("Dark"),
                _ => tr("System"),
            };
            form_combo(ui, tr("Theme:"), "theme", theme_txt, |ui| {
                ui.selectable_value(&mut self.draft.theme, "system".into(), tr("System"));
                ui.selectable_value(&mut self.draft.theme, "light".into(), tr("Light"));
                ui.selectable_value(&mut self.draft.theme, "dark".into(), tr("Dark"));
            });
        });
    }

    fn ui_snip(&mut self, ui: &mut Ui) {
        form_grid(ui, "prefs_snip", |ui| {
            let mut secs = self.draft.snip_delay;
            form_combo(
                ui,
                tr("Snip delay:"),
                "delay",
                if secs == 0 {
                    tr("None (0 s)")
                } else {
                    tr(&format!("{secs} s"))
                },
                |ui| {
                    for s in [0, 1, 2, 3, 5] {
                        ui.selectable_value(
                            &mut secs,
                            s,
                            if s == 0 {
                                tr("None (0 s)")
                            } else {
                                tr(&format!("{s} s"))
                            },
                        );
                    }
                },
            );
            self.draft.snip_delay = secs;
            form_slider(ui, tr("Mask opacity:"), &mut self.draft.dim_alpha, 0..=200);
            form_slider(ui, tr("Snip history:"), &mut self.draft.history_max, 1..=50);
        });
        ui.add_space(12.0);
        let mut confirm = !self.draft.skip_abort_confirm;
        ui.horizontal(|ui| {
            ui.add_space(FORM_LABEL_W + FORM_GAP);
            ui.checkbox(&mut confirm, tr("Confirm before abort (Esc)"));
        });
        self.draft.skip_abort_confirm = !confirm;
    }

    fn ui_paste(&mut self, ui: &mut Ui) {
        form_grid(ui, "prefs_paste", |ui| {
            form_combo(
                ui,
                tr("Double-click pin:"),
                "dbl",
                match self.draft.dblclick_pin_action.as_str() {
                    "hide" => tr("Hide pin"),
                    "none" => tr("Do nothing"),
                    _ => tr("Close pin"),
                },
                |ui| {
                    ui.selectable_value(
                        &mut self.draft.dblclick_pin_action,
                        "close".into(),
                        tr("Close pin"),
                    );
                    ui.selectable_value(
                        &mut self.draft.dblclick_pin_action,
                        "hide".into(),
                        tr("Hide pin"),
                    );
                    ui.selectable_value(
                        &mut self.draft.dblclick_pin_action,
                        "none".into(),
                        tr("Do nothing"),
                    );
                },
            );
            form_slider(
                ui,
                tr("Closed pins:"),
                &mut self.draft.closed_pins_max,
                0..=50,
            );
        });
        ui.add_space(12.0);
        ui.label(RichText::new(tr("Pin groups:")).strong());
        let groups = self.draft.groups.clone();
        egui::ScrollArea::vertical().id_salt("pin_groups").max_height(144.0).show(ui, |ui| {
        for (i, g) in groups.iter().enumerate() {
            if ui
                .add_sized([ui.available_width(), ROW_H], egui::Button::new(groups::label(g)).selected(self.group_sel == i).frame(false).truncate())
                .on_hover_text(groups::label(g))
                .clicked()
            {
                self.group_sel = i;
                self.group_edit = g.clone();
            }
        }
        });
        ui.horizontal(|ui| {
            let edit_w = ui.available_width() - 3.0 * (ACTION_W + 8.0);
            ui.add_sized([edit_w, ROW_H], egui::TextEdit::singleline(&mut self.group_edit));
            if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Add"))).clicked() {
                let _ = groups::ensure(&mut self.draft.groups, &self.group_edit);
            }
            if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Rename"))).clicked() {
                if let Some(old) = self.draft.groups.get(self.group_sel).cloned() {
                    match groups::rename(&mut self.draft.groups, &old, &self.group_edit) {
                        Ok(_) => {}
                        Err(e) => self.status = tr(&e),
                    }
                }
            }
            if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Remove"))).clicked() {
                if let Some(name) = self.draft.groups.get(self.group_sel).cloned() {
                    if let Err(e) = groups::remove(&mut self.draft.groups, &name) {
                        self.status = tr(&e);
                    }
                    self.group_sel = 0;
                }
            }
        });
    }

    fn ui_output(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(tr("File")).strong());
        form_grid(ui, "prefs_output", |ui| {
            form_label(ui, tr("Save folder:"));
            ui.horizontal(|ui| {
                let w = CONTROL_W - ACTION_W * 2.0 - 16.0;
                ui.add_sized([w, ROW_H], egui::TextEdit::singleline(&mut self.draft.quick_save_folder));
                if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Browse…"))).clicked() {
                    if let Some(p) = crate::dialog::open_folder() {
                        self.draft.quick_save_folder = p;
                    }
                }
                if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Open"))).clicked() && !self.draft.quick_save_folder.is_empty() {
                    reveal_dir(Path::new(&self.draft.quick_save_folder));
                }
            });
            ui.end_row();
            form_line_hint(
                ui,
                tr("File name format:"),
                &mut self.draft.save_filename_format,
                "pinshot-{date}-{time}.png",
            );
            ui.label("");
            ui.add(egui::Label::new(RichText::new(tr("Use {date}, {time}, or {timestamp}.")).weak()).wrap());
            ui.end_row();
            form_slider(ui, tr("Quality:"), &mut self.draft.image_quality, 1..=100);
        });
    }

    fn ui_control(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(tr("Global hotkeys")).strong());
        form_grid(ui, "prefs_hotkeys", |ui| {
            form_hotkey(ui, tr("Snip:"), &mut self.draft.hotkey_snip);
            form_hotkey(ui, tr("Pin clipboard:"), &mut self.draft.hotkey_pin);
            form_hotkey(ui, tr("Hide/show pins:"), &mut self.draft.hotkey_hide_all);
            form_hotkey(ui, tr("Restore last pin:"), &mut self.draft.hotkey_restore);
            form_hotkey(
                ui,
                tr("Select-text translate:"),
                &mut self.draft.hotkey_selection_translate,
            );
            form_hotkey(
                ui,
                tr("Snip and translate:"),
                &mut self.draft.hotkey_capture_translate,
            );
            form_hotkey(
                ui,
                tr("Click-through:"),
                &mut self.draft.hotkey_clickthrough,
            );
        });
        let conflicts = find_conflicts(&bindings_from_config(&self.draft));
        if !conflicts.is_empty() {
            ui.add_space(4.0);
            ui.colored_label(
                Color32::from_rgb(200, 60, 40),
                RichText::new(format!(
                    "{}: {} + {}",
                    tr("Conflicting hotkeys"),
                    conflicts[0].0,
                    conflicts[0].1
                )),
            );
        }
    }

    fn ui_translate(&mut self, ui: &mut Ui) {
        let prev = self.draft.translate.provider.clone();
        form_grid(ui, "prefs_translate", |ui| {
            let prov = match self.draft.translate.normalized_provider() {
                "deepseek" => tr("DeepSeek"),
                "volcengine" => tr("Volcengine Ark"),
                "baidu" => tr("Baidu"),
                "deepl" => tr("DeepL"),
                "edge" => tr("Microsoft Edge (free)"),
                _ => tr("OpenAI"),
            };
            form_combo(ui, tr("Provider:"), "tr_prov", prov, |ui| {
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "openai".into(),
                    tr("OpenAI"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "deepseek".into(),
                    tr("DeepSeek"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "volcengine".into(),
                    tr("Volcengine Ark"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "baidu".into(),
                    tr("Baidu"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "deepl".into(),
                    tr("DeepL"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.provider,
                    "edge".into(),
                    tr("Microsoft Edge (free)"),
                );
            });
            let tgt = match self.draft.translate.target.as_str() {
                "" => tr("Follow UI language"),
                "zh-Hans" => tr("Simplified Chinese"),
                "zh-Hant" => tr("Traditional Chinese"),
                "ja" => tr("Japanese"),
                "en" => tr("English"),
                other => other.to_string(),
            };
            form_combo(ui, tr("Target language:"), "tr_tgt", tgt, |ui| {
                ui.selectable_value(
                    &mut self.draft.translate.target,
                    String::new(),
                    tr("Follow UI language"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.target,
                    "zh-Hans".into(),
                    tr("Simplified Chinese"),
                );
                ui.selectable_value(&mut self.draft.translate.target, "en".into(), tr("English"));
                ui.selectable_value(
                    &mut self.draft.translate.target,
                    "ja".into(),
                    tr("Japanese"),
                );
                ui.selectable_value(
                    &mut self.draft.translate.target,
                    "zh-Hant".into(),
                    tr("Traditional Chinese"),
                );
            });
            let baidu = self.draft.translate.normalized_provider() == "baidu";
            let openai = self.draft.translate.uses_openai_compat() && !baidu;
            let deepl = self.draft.translate.normalized_provider() == "deepl";
            if openai || deepl {
                form_secret(ui, tr("API key:"), &mut self.draft.translate.api_key);
            }
            if baidu {
                form_line(ui, tr("Baidu App ID:"), &mut self.draft.translate.app_id);
                form_secret(ui, tr("Baidu Secret Key:"), &mut self.draft.translate.api_key);
            }
            form_line(ui, tr("Provider proxy (optional):"), &mut self.draft.translate.proxy);
            if openai {
                let model_hint = pinshot_core::translate::service_preset(
                    self.draft.translate.normalized_provider(),
                )
                .map(|p| p.default_model)
                .unwrap_or(pinshot_core::translate::OPENAI_MODEL_DEFAULT);
                if matches!(self.draft.translate.normalized_provider(), "deepseek" | "volcengine") {
                    let current = self.draft.translate.model.clone();
                    form_combo(ui, tr("Available models:"), "tr_models", current, |ui| {
                        for model in &self.models {
                            ui.selectable_value(&mut self.draft.translate.model, model.clone(), model);
                        }
                    });
                    ui.label("");
                    if ui.add_enabled(self.models_rx.is_none(), egui::Button::new(tr("Refresh models"))).clicked() {
                        let settings = self.draft.translate.clone();
                        self.models_provider = settings.normalized_provider().to_string();
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.models_rx = Some(rx);
                        std::thread::spawn(move || {
                            let _ = tx.send(crate::translate_run::list_models(&settings));
                        });
                    }
                    ui.end_row();
                } else {
                    form_line_hint(
                        ui,
                        tr("Model:"),
                        &mut self.draft.translate.model,
                        model_hint,
                    );
                }
            }
        });
        if self.draft.translate.provider != prev {
            self.models.clear();
            self.models_provider.clear();
            let old = self.draft.translate.clone();
            self.draft.translate_profiles.insert(prev, old);
            let next = self.draft.translate.normalized_provider().to_string();
            if let Some(saved) = self.draft.translate_profiles.get(&next).cloned() {
                self.draft.translate = saved;
                self.draft.translate.provider = next.clone();
            } else {
                pinshot_core::translate::apply_service_defaults(&mut self.draft.translate);
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let testing = self.test_rx.is_some();
            if ui
                .add_enabled(!testing, egui::Button::new(tr("Test")))
                .clicked()
            {
                // Persist the selected provider and its fields before testing.
                // Otherwise the test can use the draft while a restart still
                // loads the previous provider from disk.
                if self.apply() {
                    self.start_translate_test();
                }
            }
            if testing {
                ui.label(tr("Testing…"));
            }
        });
        ui.add_space(2.0);
        ui.weak(tr(
            "OpenAI requires an API URL, key, and model. Edge is the free fallback if the API is unset or fails.",
        ));
    }

    fn start_ocr_model_download(&mut self) {
        if self.ocr_dl_rx.is_some() {
            return;
        }
        if crate::ocr_run::models_ready_for_tier(&self.draft.ocr_model) {
            return;
        }
        let tier = self.draft.ocr_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.ocr_dl_rx = Some(rx);
        self.ocr_dl_frac = 0.0;
        std::thread::spawn(move || {
            let result = crate::ocr_run::download_models_for_tier(&tier, |f| {
                let _ = tx.send(OcrDlUpdate::Progress(f));
            });
            let _ = tx.send(OcrDlUpdate::Done(result));
        });
    }

    fn start_translate_test(&mut self) {
        let settings = self.draft.translate.clone();
        let ui_lang = self.draft.language.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.test_rx = Some(rx);
        self.status = tr("Testing…");
        std::thread::spawn(move || {
            let r = crate::translate_run::translate_with_opts(&settings, &ui_lang, "hello", false);
            let _ = tx.send(r);
        });
    }

    fn ui_update(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(format!("PinShot {VERSION}")).strong());
        ui.add_space(4.0);
        ui.checkbox(
            &mut self.draft.update.check_on_startup,
            tr("Check for updates on startup"),
        );
        ui.weak(tr(
            "When enabled, PinShot checks daily after the time below while it is running. On Windows, a newer release installs silently and restarts.",
        ));
        form_grid(ui, "prefs_update", |ui| {
            form_label(ui, tr("Check time:"));
            let (h, m) =
                pinshot_core::update::parse_check_time(&self.draft.update.check_time).unwrap_or((9, 0));
            let mut hour = h as i32;
            let mut minute = m as i32;
            ui.horizontal(|ui| {
                ui.add_sized([ACTION_W, ROW_H], egui::DragValue::new(&mut hour).range(0..=23));
                ui.label(":");
                ui.add_sized([ACTION_W, ROW_H], egui::DragValue::new(&mut minute).range(0..=59));
            });
            self.draft.update.check_time =
                pinshot_core::update::normalize_check_time(&format!("{hour}:{minute:02}"));
            ui.end_row();
            form_label(ui, tr("Last check:"));
            let last = pinshot_core::update::format_last_check_unix(self.draft.update.last_check_unix)
                .unwrap_or_else(|| tr("Never"));
            ui.label(last);
            ui.end_row();
        });
        #[cfg(not(windows))]
        ui.weak(tr("Auto-install is available on Windows only."));
        ui.add_space(4.0);
        let checking = self.update_check_rx.is_some();
        ui.add_enabled_ui(!checking, |ui| {
            if ui.button(tr("Check now")).clicked() {
                self.start_update_check();
            }
        });
        if checking {
            ui.label(tr("Checking for updates…"));
        }
    }

    fn start_update_check(&mut self) {
        if self.update_check_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_check_rx = Some(rx);
        self.status = tr("Checking for updates…");
        std::thread::spawn(move || {
            let r = crate::update_run::check_and_apply(false);
            let _ = tx.send(UpdateCheckMsg::Done(r));
        });
    }

    fn ui_about(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(format!("PinShot {VERSION}")).strong());
    }
}

/// Always a down chevron when closed. egui's default uses AboveOrBelow and
/// paints ▲ on short pages (Interface) because the popup opens upward.
const OCR_DL_BTN_W: f32 = 92.0;

/// Progress fills the button left → right while downloading; solid green when ready.
fn ocr_model_download_button(
    ui: &mut Ui,
    ready: bool,
    downloading: bool,
    fraction: f32,
) -> bool {
    let label = if ready && !downloading {
        tr("Downloaded")
    } else {
        tr("Download")
    };
    let size = egui::vec2(OCR_DL_BTN_W, ROW_H);
    let sense = if ready || downloading {
        egui::Sense::hover()
    } else {
        egui::Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let visuals = ui.style().visuals.widgets.inactive;
    let rounding = egui::CornerRadius::same(5);
    let stroke = visuals.bg_stroke;
    ui.painter().rect_filled(rect, rounding, visuals.bg_fill);
    let accent = ui.style().visuals.selection.bg_fill;
    let done_fill = Color32::from_rgb(40, 120, 66);
    if ready && !downloading {
        ui.painter().rect_filled(rect, rounding, done_fill);
    } else if downloading {
        let w = rect.width() * fraction.clamp(0.0, 1.0);
        if w > 0.5 {
            let fill = egui::Rect::from_min_max(rect.min, egui::pos2(rect.min.x + w, rect.max.y));
            ui.painter().rect_filled(fill, rounding, accent);
        }
    }
    ui.painter().rect_stroke(rect, rounding, stroke, egui::StrokeKind::Inside);
    let text_color = if ready && !downloading {
        Color32::WHITE
    } else {
        visuals.fg_stroke.color
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        text_color,
    );
    response.clicked()
}

fn combo_chevron(
    ui: &Ui,
    rect: egui::Rect,
    visuals: &egui::style::WidgetVisuals,
    _is_popup_open: bool,
    _above_or_below: egui::AboveOrBelow,
) {
    let rect = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(rect.width() * 0.7, rect.height() * 0.45),
    );
    ui.painter().add(egui::Shape::convex_polygon(
        vec![rect.left_top(), rect.right_top(), rect.center_bottom()],
        visuals.fg_stroke.color,
        egui::Stroke::NONE,
    ));
}

fn form_grid(ui: &mut Ui, id: &'static str, add: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([FORM_GAP, 8.0])
        .min_col_width(FORM_LABEL_W)
        .min_row_height(ROW_H)
        .show(ui, add);
}

fn form_label(ui: &mut Ui, label: String) {
    ui.allocate_ui_with_layout(
        egui::vec2(FORM_LABEL_W, ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| { ui.add(egui::Label::new(label).wrap()); },
    );
}

fn form_combo(
    ui: &mut Ui,
    label: String,
    id: &'static str,
    selected: impl Into<egui::WidgetText>,
    add_contents: impl FnOnce(&mut Ui),
) {
    form_label(ui, label);
    egui::ComboBox::from_id_salt(id)
        .width(CONTROL_W)
        .wrap_mode(egui::TextWrapMode::Truncate)
        .icon(combo_chevron)
        .selected_text(selected)
        .show_ui(ui, add_contents);
    ui.end_row();
}

fn form_line(ui: &mut Ui, label: String, val: &mut String) {
    form_line_hint(ui, label, val, "");
}

#[derive(Clone)]
struct HotkeyCapture {
    #[cfg(windows)]
    down: [bool; 256],
}

fn form_hotkey(ui: &mut Ui, label: String, val: &mut String) {
    form_label(ui, label.clone());
    let id = ui.make_persistent_id(("hotkey_capture", &label));
    let mut capture = ui.ctx().data_mut(|data| data.get_temp::<HotkeyCapture>(id));
    ui.horizontal(|ui| {
        let text = if capture.is_some() { tr("Press a shortcut…") }
            else if val.is_empty() { tr("Click to set") } else { val.clone() };
        let response = ui.add_sized([CONTROL_W - ACTION_W - 8.0, ROW_H], egui::Button::new(text));
        if response.clicked() {
            response.request_focus();
            capture = Some(HotkeyCapture {
                #[cfg(windows)]
                down: keyboard_snapshot(),
            });
        } else if !response.has_focus() || !ui.input(|i| i.focused) {
            capture = None;
        }
        if let Some(state) = capture.as_mut() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
            #[cfg(windows)]
            let pressed = {
                let down = keyboard_snapshot();
                let key = (8..256).find(|&vk| down[vk] && !state.down[vk]
                    && !matches!(vk, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5));
                state.down = down;
                key.and_then(|vk| recorded_vk(vk as u32, &down))
            };
            #[cfg(not(windows))]
            let pressed = {
                let _ = state;
                ui.input(|i| i.events.iter().find_map(|event| {
                    if let egui::Event::Key { key, physical_key, pressed: true, repeat: false, modifiers, .. } = event {
                        recorded_egui_key(physical_key.unwrap_or(*key), *modifiers)
                    } else { None }
                }))
            };
            if let Some(seq) = pressed {
                if seq != "Escape" { *val = seq; }
                capture = None;
                response.surrender_focus();
            }
            // Do not let recorded keys activate other controls or insert text.
            ui.input_mut(|i| i.events.retain(|event| !matches!(event,
                egui::Event::Key { .. } | egui::Event::Text(_))));
        }
        if ui.add_sized([ACTION_W, ROW_H], egui::Button::new(tr("Clear"))).clicked() {
            val.clear();
            capture = None;
        }
    });
    ui.ctx().data_mut(|data| {
        if let Some(capture) = capture { data.insert_temp(id, capture); }
        else { data.remove::<HotkeyCapture>(id); }
    });
    ui.end_row();
}

#[cfg(windows)]
fn keyboard_snapshot() -> [bool; 256] {
    std::array::from_fn(|vk| unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0
    })
}

#[cfg(windows)]
fn recorded_vk(vk: u32, down: &[bool; 256]) -> Option<String> {
    let key = match vk {
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(vk)?.to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x08 => "Backspace".into(), 0x09 => "Tab".into(), 0x0d => "Enter".into(),
        0x1b => "Escape".into(), 0x20 => "Space".into(), 0x13 => "Pause".into(),
        0x21 => "PageUp".into(), 0x22 => "PageDown".into(), 0x23 => "End".into(),
        0x24 => "Home".into(), 0x25 => "Left".into(), 0x26 => "Up".into(),
        0x27 => "Right".into(), 0x28 => "Down".into(), 0x2c => "PrintScreen".into(),
        0x2d => "Insert".into(), 0x2e => "Delete".into(),
        0xc0 => "`".into(), 0xbd => "-".into(), 0xbb => "=".into(),
        0xdb => "[".into(), 0xdd => "]".into(), 0xba => ";".into(),
        0xde => "'".into(), 0xbc => ",".into(), 0xbe => ".".into(),
        0xbf => "/".into(), 0xdc => "\\".into(), _ => return None,
    };
    shortcut_sequence(&key, down[0x11], down[0x10], down[0x12], down[0x5b] || down[0x5c])
}

#[cfg(not(windows))]
fn recorded_egui_key(key: egui::Key, mods: egui::Modifiers) -> Option<String> {
    let name = match key {
        egui::Key::ArrowLeft => "Left", egui::Key::ArrowRight => "Right",
        egui::Key::ArrowUp => "Up", egui::Key::ArrowDown => "Down",
        _ => key.name(),
    };
    shortcut_sequence(name, mods.ctrl, mods.shift, mods.alt, mods.mac_cmd)
}

fn shortcut_sequence(key: &str, ctrl: bool, shift: bool, alt: bool, win: bool) -> Option<String> {
    let mut parts = Vec::new();
    if ctrl { parts.push("Ctrl"); }
    if shift { parts.push("Shift"); }
    if alt { parts.push("Alt"); }
    if win { parts.push("Win"); }
    parts.push(key);
    let seq = parts.join("+");
    pinshot_core::hotkey::parse_hotkey(&seq).map(|_| seq)
}

#[cfg(test)]
mod shortcut_capture_tests {
    use super::*;

    #[test]
    fn records_modifiers_and_keys_not_typed_text() {
        assert_eq!(shortcut_sequence("F1", false, false, false, false).as_deref(), Some("F1"));
        assert_eq!(shortcut_sequence("A", true, true, false, false).as_deref(), Some("Ctrl+Shift+A"));
        assert_eq!(shortcut_sequence("F12", false, false, true, true).as_deref(), Some("Alt+Win+F12"));
        assert!(shortcut_sequence("Ctrl", true, false, false, false).is_none());
        assert!(shortcut_sequence("输入字符", false, false, false, false).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_recording_uses_virtual_keys_with_shift_and_win() {
        let mut down = [false; 256];
        down[0x11] = true;
        down[0x10] = true;
        assert_eq!(recorded_vk(0x31, &down).as_deref(), Some("Ctrl+Shift+1"));
        down[0x5c] = true;
        assert_eq!(recorded_vk(0x7b, &down).as_deref(), Some("Ctrl+Shift+Win+F12"));
        assert!(recorded_vk(0x10, &down).is_none());
    }
}

fn form_line_hint(ui: &mut Ui, label: String, val: &mut String, hint: &str) {
    form_label(ui, label);
    ui.add_sized([CONTROL_W, ROW_H],
        egui::TextEdit::singleline(val)
            .hint_text(hint)
            .desired_width(CONTROL_W),
    );
    ui.end_row();
}

fn form_secret(ui: &mut Ui, label: String, val: &mut String) {
    form_label(ui, label);
    ui.add_sized([CONTROL_W, ROW_H],
        egui::TextEdit::singleline(val)
            .password(true)
            .desired_width(CONTROL_W),
    );
    ui.end_row();
}

fn form_slider<Num: egui::emath::Numeric>(
    ui: &mut Ui,
    label: String,
    val: &mut Num,
    range: std::ops::RangeInclusive<Num>,
) {
    form_label(ui, label);
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = CONTROL_W - ACTION_W - ui.spacing().item_spacing.x;
        ui.spacing_mut().interact_size.x = ACTION_W;
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(90);
        visuals.widgets.inactive.fg_stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(235, 235, 235));
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(55, 145, 235);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(35, 120, 215);
        visuals.widgets.active.fg_stroke = egui::Stroke::new(2.5, egui::Color32::WHITE);
        ui.add_sized([CONTROL_W, ROW_H], egui::Slider::new(val, range));
    });
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_pages_fit_content_column_in_both_languages() {
        let original = config::load().unwrap_or_default().lang();
        for lang in [Lang::ZhCn, Lang::En] {
            i18n::set_lang(lang);
            for tab in 0..TABS.len() {
                let ctx = egui::Context::default();
                crate::ui_font::install_cjk(&ctx);
                ctx.style_mut(|style| {
                    style.spacing.item_spacing = egui::vec2(8.0, 10.0);
                    style.spacing.button_padding = egui::vec2(10.0, 6.0);
                    style.spacing.interact_size.y = ROW_H;
                });
                let mut cfg = Config::default();
                cfg.groups = (0..24).map(|i| format!("Long group name {i} with more content")).collect();
                cfg.quick_save_folder = "C:/a/very/long/directory/".repeat(8);
                let mut app = PrefsApp {
                    draft: cfg.clone(), saved: cfg, tab, group_sel: 0,
                    group_edit: String::new(), status: String::new(),
                    test_rx: None, ocr_dl_rx: None, ocr_dl_frac: 0.0,
                    update_check_rx: None, models: Vec::new(),
                    models_provider: String::new(), models_rx: None,
                };
                // egui grids settle column widths across frames.
                for frame in 0..3 {
                    let _ = ctx.run(egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(PREFS_W, PREFS_MAX_H))),
                        ..Default::default()
                    }, |ctx| {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(CONTENT_MARGIN)))
                            .show(ctx, |ui| {
                                match tab {
                                    0 => app.ui_general(ui), 1 => app.ui_interface(ui),
                                    2 => app.ui_snip(ui), 3 => app.ui_paste(ui),
                                    4 => app.ui_output(ui), 5 => app.ui_control(ui),
                                    6 => app.ui_translate(ui), 7 => app.ui_update(ui),
                                    _ => app.ui_about(ui),
                                }
                                if frame == 2 {
                                    assert!(ui.min_rect().right() <= PREFS_W - CONTENT_MARGIN as f32 + 1.0,
                                        "{lang:?} {} overflows: {:?}", TABS[tab], ui.min_rect());
                                    assert!(ui.cursor().min.y < PREFS_MAX_H - FOOTER_H - 100.0,
                                        "{} exceeds available height", TABS[tab]);
                                }
                            });
                    });
                }
            }
        }
        i18n::set_lang(original);
    }

    #[test]
    fn prefs_window_size_bounds() {
        let size = prefs_inner_size();
        assert_eq!(size.x, PREFS_W);
        assert_eq!(size.y, PREFS_MIN_H);
        assert!(PREFS_MIN_H >= 200.0);
        assert!(PREFS_W <= 560.0);
        assert_eq!(TABS.len(), 9);
        assert_eq!(TABS[6], "Translate");
        assert_eq!(TABS[7], "Update");
        assert!(PREFS_MIN_H < PREFS_MAX_H);
    }

    #[test]
    fn combo_chevron_closed_points_down() {
        // Closed (openness=0) must be ▼ so Interface matches Control/Output.
        let _ = combo_chevron
            as fn(&Ui, egui::Rect, &egui::style::WidgetVisuals, bool, egui::AboveOrBelow);
    }

    #[test]
    fn form_label_column_is_aligned() {
        assert!(FORM_LABEL_W >= 100.0);
        assert!(ROW_H >= 20.0);
        assert!(CONTROL_W >= 180.0);
    }

    #[test]
    fn restore_general_resets_language_and_login() {
        let mut app = PrefsApp {
            draft: Config {
                language: "en".into(),
                run_at_login: true,
                start_minimized: false,
                ..Config::default()
            },
            saved: Config::default(),
            tab: 0,
            group_sel: 0,
            group_edit: String::new(),
            status: String::new(),
            test_rx: None,
            ocr_dl_rx: None,
            ocr_dl_frac: 0.0,
            update_check_rx: None,
                models: Vec::new(),
                models_provider: String::new(),
                models_rx: None,
        };
        app.restore_tab();
        let d = Config::default();
        assert_eq!(app.draft.language, d.language);
        assert_eq!(app.draft.run_at_login, d.run_at_login);
        assert_eq!(app.draft.start_minimized, d.start_minimized);
    }

    #[test]
    fn restore_translate_resets_to_openai_compat() {
        let mut app = PrefsApp {
            draft: Config {
                translate: pinshot_core::config::TranslateSettings {
                    provider: "edge".into(),
                    api_url: "https://example.invalid".into(),
                    api_key: "sk-x".into(),
                    app_id: String::new(),
                    model: "x".into(),
                    target: "ja".into(),
            proxy: String::new(),
                },
                ..Config::default()
            },
            saved: Config::default(),
            tab: 6,
            group_sel: 0,
            group_edit: String::new(),
            status: String::new(),
            test_rx: None,
            ocr_dl_rx: None,
            ocr_dl_frac: 0.0,
            update_check_rx: None,
            models: Vec::new(),
            models_provider: String::new(),
            models_rx: None,
        };
        app.restore_tab();
        assert_eq!(app.draft.translate.provider, "openai");
        assert!(app.draft.translate.uses_openai_compat());
        assert!(app.draft.translate.api_key.is_empty());
    }
}
