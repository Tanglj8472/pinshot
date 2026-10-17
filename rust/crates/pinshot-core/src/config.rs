//! PinShot settings. Stored as TOML under `%APPDATA%\PinShot` (Windows).

use crate::i18n::Lang;
use serde::{Deserialize, Serialize};
use std::fs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub language: String,
    pub start_minimized: bool,
    pub skip_abort_confirm: bool,
    pub dblclick_pin_action: String,
    pub dblclick_tip_shown: bool,
    pub hotkey_snip: String,
    pub hotkey_pin: String,
    pub hotkey_hide_all: String,
    pub hotkey_clickthrough: String,
    pub hotkey_restore: String,
    pub hotkey_selection_translate: String,
    pub hotkey_capture_translate: String,
    pub run_at_login: bool,
    pub groups: Vec<String>,
    #[serde(default = "default_pictures_dir")]
    pub quick_save_folder: String,
    /// File name template for saved images. Supports `{date}`, `{time}`, and
    /// `{timestamp}`; the value must end in `.png` or `.jpg`.
    #[serde(default = "default_save_filename_format")]
    pub save_filename_format: String,
    pub image_quality: u8,
    pub snip_delay: u32,
    pub history_max: u32,
    pub closed_pins_max: u32,
    pub theme: String,
    pub dim_alpha: u8,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// `server` (default, ~170 MB, more accurate) or `mobile` (faster on CPU).
    #[serde(default)]
    pub ocr_model: String,
    /// F5 translator (pot / STranslate / openai-translator style).
    #[serde(default)]
    pub translate: TranslateSettings,
    /// Saved credentials and model settings per provider.
    #[serde(default)]
    pub translate_profiles: BTreeMap<String, TranslateSettings>,
    #[serde(default)]
    pub update: crate::update::UpdateSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language: "zh_CN".into(),
            start_minimized: true,
            skip_abort_confirm: false,
            dblclick_pin_action: "close".into(),
            dblclick_tip_shown: false,
            hotkey_snip: "F1".into(),
            hotkey_pin: "F3".into(),
            hotkey_hide_all: "Shift+F3".into(),
            hotkey_clickthrough: "F4".into(),
            hotkey_restore: "Ctrl+Shift+Z".into(),
            hotkey_selection_translate: "F5".into(),
            hotkey_capture_translate: "F6".into(),
            run_at_login: true,
            groups: vec![
                "Default".into(),
                "Group 1".into(),
                "Group 2".into(),
                "Group 3".into(),
            ],
            quick_save_folder: default_pictures_dir(),
            save_filename_format: default_save_filename_format(),
            image_quality: 92,
            snip_delay: 0,
            history_max: 10,
            closed_pins_max: 10,
            theme: "system".into(),
            dim_alpha: 160,
            log_level: default_log_level(),
            ocr_model: "mobile".into(),
            translate: TranslateSettings::default(),
            translate_profiles: BTreeMap::new(),
            update: crate::update::UpdateSettings::default(),
        }
    }
}

fn default_log_level() -> String { "normal".into() }

/// Provider + endpoint + key + model + target. `api_key` is never shown in Debug.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateSettings {
    /// OpenAI preset (`openai` / `deepseek` /
    /// `baidu`), `deepl`, or `edge` (free fallback).
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    #[serde(default)]
    pub app_id: String,
    pub model: String,
    /// Edge/DeepL/OpenAI `to=` code (`en`, `zh-Hans`, …). Empty = follow UI language.
    pub target: String,
    #[serde(default)]
    pub proxy: String,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            api_url: crate::translate::OPENAI_URL_DEFAULT.into(),
            api_key: String::new(),
            app_id: String::new(),
            model: String::new(),
            target: String::new(),
            proxy: String::new(),
        }
    }
}

impl std::fmt::Debug for TranslateSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslateSettings")
            .field("provider", &self.provider)
            .field("api_url", &self.api_url)
            .field("api_key", &if self.api_key.is_empty() { "" } else { "***" })
            .field("app_id", &if self.app_id.is_empty() { "" } else { "***" })
            .field("model", &self.model)
            .field("target", &self.target)
            .field("proxy", &if self.proxy.is_empty() { "" } else { "***" })
            .finish()
    }
}

impl TranslateSettings {
    pub fn normalized_provider(&self) -> &str {
        match self.provider.trim() {
            "openai" | "openai-compatible" | "openai_compatible" => "openai",
            "volcengine" | "volc" | "ark" | "doubao" => "volcengine",
            "deepseek" => "deepseek",
            "qwen" | "tongyi" | "dashscope" => "openai",
            "ollama" => "openai",
            "baidu" | "qianfan" | "ernie" | "wenxin" => "baidu",
            "deepl" => "deepl",
            "edge" | "microsoft" | "free" => "edge",
            _ => "openai",
        }
    }

    pub fn uses_openai_compat(&self) -> bool {
        matches!(
            self.normalized_provider(),
            "openai" | "deepseek" | "volcengine" | "baidu"
        )
    }
}

impl Config {
    pub fn lang(&self) -> Lang {
        Lang::from_code(&self.language)
    }

    pub fn validate(mut self) -> Self {
        self.log_level = match self.log_level.trim().to_ascii_lowercase().as_str() {
            "off" => "off",
            "trace" | "detailed" => "trace",
            "debug" => "debug",
            _ => "normal",
        }.into();
        if Lang::from_code(&self.language) == Lang::ZhCn
            && self.language != "zh_CN"
            && self.language != "en"
        {
            self.language = "zh_CN".into();
        }
        if !matches!(self.language.as_str(), "en" | "zh_CN") {
            // from_code maps unknown → zh, keep a canonical code
            self.language = self.lang().code().into();
        }
        if !matches!(self.dblclick_pin_action.as_str(), "close" | "hide" | "none") {
            self.dblclick_pin_action = "close".into();
        }
        self.image_quality = self.image_quality.clamp(1, 100);
        self.history_max = self.history_max.max(1);
        self.closed_pins_max = self.closed_pins_max.max(1);
        self.dim_alpha = self.dim_alpha.min(200);
        // v1 overlay hardcoded 110, which is too light vs 参考工具 and makes
        // the magnifier look like it paints an extra shadow on the freeze.
        if self.schema_version < 2 && self.dim_alpha == 110 {
            self.dim_alpha = 160;
        }
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".into();
        }
        self.ocr_model = match self.ocr_model.trim() {
            "server" => "server".into(),
            _ => "mobile".into(),
        };
        self.groups = crate::groups::dedupe(&self.groups);
        if self.quick_save_folder.trim().is_empty() {
            self.quick_save_folder = default_pictures_dir();
        }
        if self.hotkey_selection_translate.trim().is_empty() {
            self.hotkey_selection_translate = "F5".into();
        }
        if self.hotkey_capture_translate.trim().is_empty() {
            self.hotkey_capture_translate = "F6".into();
        }
        if self.hotkey_clickthrough.trim().is_empty() {
            self.hotkey_clickthrough = "F4".into();
        }
        if self.schema_version < SCHEMA_VERSION {
            self.schema_version = SCHEMA_VERSION;
        }
        self.translate.provider = self.translate.normalized_provider().to_string();
        self.translate.api_url = canonical_translate_url(&self.translate.provider).into();
        self.translate.api_key = self.translate.api_key.trim().to_string();
        self.translate.model = self.translate.model.trim().to_string();
        self.translate.target = self.translate.target.trim().to_string();
        for settings in self.translate_profiles.values_mut() {
            settings.provider = settings.normalized_provider().to_string();
            settings.api_url = canonical_translate_url(&settings.provider).into();
            settings.api_key = settings.api_key.trim().to_string();
            settings.model = settings.model.trim().to_string();
            settings.target = settings.target.trim().to_string();
        }
        self.update = self.update.clone().normalized();
        self
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))
    }

    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Ok(cfg.validate())
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        Self::from_toml(&text)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| ConfigError::Io(e.to_string()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, self.to_toml()?).map_err(|e| ConfigError::Io(e.to_string()))?;
        fs::rename(&tmp, path).map_err(|e| ConfigError::Io(e.to_string()))?;
        Ok(())
    }
}

fn default_pictures_dir() -> String {
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return PathBuf::from(profile).join("Pictures").to_string_lossy().into_owned();
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join("Pictures").to_string_lossy().into_owned();
        }
    }
    "Pictures".into()
}

fn canonical_translate_url(provider: &str) -> &'static str {
    match provider {
        "deepseek" => crate::translate::DEEPSEEK_URL_DEFAULT,
        "volcengine" => crate::translate::VOLCENGINE_URL_DEFAULT,
        "baidu" => crate::translate::BAIDU_URL_DEFAULT,
        "deepl" => crate::translate::DEEPL_URL_DEFAULT,
        "edge" => crate::translate::EDGE_URL,
        _ => crate::translate::OPENAI_URL_DEFAULT,
    }
}

fn default_save_filename_format() -> String {
    "pinshot-{date}-{time}.png".into()
}

/// Expand the user-facing save filename template and keep it a filename rather
/// than allowing separators in the configured value.
pub fn expand_save_filename(template: &str) -> String {
    let now = chrono::Local::now();
    let mut name = template.trim().to_string();
    if name.is_empty() {
        name = default_save_filename_format();
    }
    name = name
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H-%M-%S").to_string())
        .replace("{timestamp}", &now.timestamp().to_string());
    name = name
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    if !name.to_ascii_lowercase().ends_with(".png") && !name.to_ascii_lowercase().ends_with(".jpg") {
        name.push_str(".png");
    }
    name
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("config io: {0}")]
    Io(String),
    #[error("config parse: {0}")]
    Parse(String),
    #[error("config serialize: {0}")]
    Serialize(String),
}

/// `%APPDATA%\PinShot` on Windows; `$XDG_CONFIG_HOME/PinShot` elsewhere (tests).
pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PINSHOT_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("PinShot");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("PinShot");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config").join("PinShot");
    }
    PathBuf::from("PinShot")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// `%APPDATA%\PinShot` on Windows; `$XDG_DATA_HOME/PinShot` or `~/.local/share/PinShot` elsewhere.
/// Session PNGs, OCR models, and the debug log live here. Config stays in [`config_dir`].
pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PINSHOT_DATA_DIR") {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        return config_dir();
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("PinShot");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("PinShot");
        }
        PathBuf::from("PinShot")
    }
}

/// `~/.config/autostart` on Linux; unused on Windows (HKCU Run).
pub fn xdg_autostart_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("autostart");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config").join("autostart");
    }
    PathBuf::from("autostart")
}

pub fn load() -> Result<Config, ConfigError> {
    Config::load_from(&config_path())
}

pub fn save(cfg: &Config) -> Result<(), ConfigError> {
    cfg.save_to(&config_path())
}

/// Record an update check without persisting a stale UI draft or app snapshot.
pub fn record_update_check(unix: i64) -> Result<(), ConfigError> {
    record_update_check_at(&config_path(), unix)
}

fn record_update_check_at(path: &Path, unix: i64) -> Result<(), ConfigError> {
    let mut cfg = Config::load_from(path)?;
    cfg.update.last_check_unix = unix;
    cfg.save_to(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_load_old_configs_and_roundtrip() {
        assert_eq!(Config::from_toml("").unwrap().log_level, "normal");
        for (input, expected) in [("off", "off"), ("normal", "normal"),
            ("trace", "trace"), ("debug", "debug"), ("detailed", "trace"),
            (" DEBUG ", "debug"), ("invalid", "normal")] {
            let cfg = Config::from_toml(&format!("log_level = '{input}'")).unwrap();
            assert_eq!(cfg.log_level, expected);
            assert_eq!(Config::from_toml(&cfg.to_toml().unwrap()).unwrap().log_level, expected);
        }
    }

    #[test]
    fn older_config_preserves_user_values_when_new_settings_are_added() {
        let old = "schema_version = 2\nlanguage = 'en'\nrun_at_login = false\n\
            hotkey_snip = 'Ctrl+Alt+P'\nocr_model = 'mobile'\n\
            [translate]\nprovider = 'edge'\napi_key = 'test-key-do-not-use'\n";
        let config = Config::from_toml(old).unwrap();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        assert_eq!(config.language, "en");
        assert_eq!(config.hotkey_snip, "Ctrl+Alt+P");
        assert!(!config.run_at_login);
        assert_eq!(config.ocr_model, "mobile");
        assert_eq!(config.translate.api_key, "test-key-do-not-use");
        assert_eq!(Config::from_toml(&config.to_toml().unwrap()).unwrap(), config);
    }

    #[test]
    fn update_check_only_changes_timestamp_and_preserves_unreadable_config() {
        let path = temp_path();
        let mut cfg = Config::default();
        cfg.hotkey_snip = "Ctrl+Alt+P".into();
        cfg.translate.api_key = "test-key-do-not-use".into();
        cfg.save_to(&path).unwrap();
        record_update_check_at(&path, 12345).unwrap();
        cfg.update.last_check_unix = 12345;
        assert_eq!(Config::load_from(&path).unwrap(), cfg);
        fs::write(&path, "not valid TOML [[").unwrap();
        assert!(record_update_check_at(&path, 23456).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not valid TOML [[");
        fs::remove_file(&path).unwrap();
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("pinshot-core-cfg-{n}.toml"))
    }

    #[test]
    fn defaults_are_zh() {
        let c = Config::default();
        assert_eq!(c.language, "zh_CN");
        assert_eq!(c.hotkey_snip, "F1");
        assert_eq!(c.hotkey_pin, "F3");
        assert_eq!(c.hotkey_clickthrough, "F4");
        assert_eq!(c.schema_version, 3);
        assert_eq!(c.dim_alpha, 160);
        assert!(c.run_at_login);
        assert_eq!(c.ocr_model, "mobile");
        assert!(c.update.check_on_startup);
    }

    #[test]
    fn toml_roundtrip() {
        let mut c = Config::default();
        c.skip_abort_confirm = true;
        c.image_quality = 80;
        c.quick_save_folder = r"D:\Pics".into();
        let text = c.to_toml().unwrap();
        assert!(text.contains("skip_abort_confirm"));
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn validate_clamps() {
        let mut c = Config::default();
        c.image_quality = 0;
        c.history_max = 0;
        c.dblclick_pin_action = "explode".into();
        c = c.validate();
        assert_eq!(c.image_quality, 1);
        assert_eq!(c.history_max, 1);
        assert_eq!(c.dblclick_pin_action, "close");
    }

    #[test]
    fn file_roundtrip() {
        let path = temp_path();
        let mut c = Config::default();
        c.language = "en".into();
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.language, "en");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn missing_file_is_default() {
        let path = std::env::temp_dir().join("pinshot-core-missing-nope.toml");
        let _ = fs::remove_file(&path);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn bad_toml_errors() {
        assert!(Config::from_toml("language = [").is_err());
    }

    #[test]
    fn data_dir_honors_env() {
        let p = std::env::temp_dir().join("pinshot-core-data-test");
        std::env::set_var("PINSHOT_DATA_DIR", &p);
        assert_eq!(data_dir(), p);
        std::env::remove_var("PINSHOT_DATA_DIR");
    }

    #[test]
    fn v1_light_mask_becomes_reference_tool_dim() {
        let c = Config::from_toml("schema_version = 1\ndim_alpha = 110\n").unwrap();
        assert_eq!(c.dim_alpha, 160);
        assert_eq!(c.schema_version, 3);
    }

    #[test]
    fn old_toml_fills_new_fields() {
        let c = Config::from_toml("language = \"en\"\nhotkey_snip = \"F2\"\n").unwrap();
        assert_eq!(c.language, "en");
        assert_eq!(c.hotkey_snip, "F2");
        assert_eq!(c.hotkey_selection_translate, "F5");
        assert_eq!(c.hotkey_clickthrough, "F4");
        assert_eq!(c.ocr_model, "mobile");
        assert!(c.groups.iter().any(|g| g == "Default"));
    }

    #[test]
    fn empty_clickthrough_becomes_f4() {
        let c = Config::from_toml("hotkey_clickthrough = \"\"\n").unwrap();
        assert_eq!(c.hotkey_clickthrough, "F4");
    }

    #[test]
    fn translate_settings_roundtrip_redacts_key() {
        let mut c = Config::default();
        c.translate.provider = "openai".into();
        c.translate.api_url = "https://api.deepseek.com/v1/chat/completions".into();
        c.translate.api_key = "sk-secret-do-not-log".into();
        c.translate.model = "deepseek-chat".into();
        c.translate.target = "en".into();
        let text = c.to_toml().unwrap();
        assert!(text.contains("[translate]"));
        assert!(text.contains("sk-secret-do-not-log"), "key is stored");
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back.translate.api_key, "sk-secret-do-not-log");
        assert_eq!(back.translate.provider, "openai");
        let dbg = format!("{:?}", c.translate);
        assert!(!dbg.contains("sk-secret"));
        assert!(dbg.contains("***"));
    }

    #[test]
    fn old_toml_defaults_translate_to_openai() {
        let c = Config::from_toml("language = \"en\"\n").unwrap();
        assert_eq!(c.translate.provider, "openai");
        assert!(c.translate.api_key.is_empty());
    }

    #[test]
    fn validate_keeps_compat_presets() {
        for id in ["deepseek", "baidu", "qianfan"] {
            let c = Config::from_toml(&format!("[translate]\nprovider = \"{id}\"\n")).unwrap();
            assert!(
                c.translate.uses_openai_compat(),
                "{id} should stay supported, got {}",
                c.translate.provider
            );
        }
        let edge = Config::from_toml("[translate]\nprovider = \"edge\"\n").unwrap();
        assert_eq!(edge.translate.provider, "edge");
    }
}
