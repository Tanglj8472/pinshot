use crate::update_run::UpdateOutcome;
use pinshot_core::config::Config;
use pinshot_core::ocr::OcrPage;
use pinshot_core::Command;
use winit::window::WindowId;

#[derive(Clone, Debug)]
pub enum AppEvent {
    Hotkey(Command),
    Tray(TrayCmd),
    PrefsApplied(Config),
    RaisePrefs,
    Ipc(String),
    AutomationView {
        action: String,
        session_id: u64,
        image: Option<std::sync::Arc<pinshot_core::raster::Raster>>,
        shapes: Vec<pinshot_core::annotation::Shape>,
        response: std::sync::mpsc::Sender<Result<serde_json::Value, String>>,
    },
    AutomationEdit {
        session_id: u64,
        command: serde_json::Value,
        response: std::sync::mpsc::Sender<Result<serde_json::Value, String>>,
    },
    OcrDone {
        pin: WindowId,
        result: Result<OcrPage, String>,
        translate: bool,
    },
    OcrProgress {
        pin: WindowId,
        message: String,
    },
    TranslateDone {
        pin: WindowId,
        result: Result<String, String>,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    SelectionText(Result<String, String>),
    SelectionTranslation { source: String, result: Result<String, String> },
    OpenPrefs,
    UpdateCheckDone(Result<UpdateOutcome, String>),
    X11Key {
        keysym: u32,
        mods: u16,
        pressed: bool,
    },
    PinMenu(PinMenuAction),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PinMenuAction {
    Copy,
    Save,
    ToggleToolbar,
    ToggleClickThrough,
    ToggleShadow,
    Zoom(i32),
    Rotate(i32),
    FlipH,
    FlipV,
    Paste,
    ReplaceFile,
    MoveGroup(String),
    NewGroup,
    NewGroupNamed(String),
    RenameGroup,
    RenameGroupNamed(String),
    ViewInFolder,
    Close,
    Destroy,
    CopySize,
    ResetZoom,
    ToggleSmooth,
    ToggleGrayscale,
    ToggleInvert,
    Ocr,
    Translate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayCmd {
    Snip,
    Pin,
    HideAll,
    Settings,
    Quit,
    Translate,
    Restore,
    ToggleGroup(String),
}
