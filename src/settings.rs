use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::file_store::replace_file_with_recovery;

pub const DEFAULT_NEAR_LIMIT_THRESHOLD_PERCENT: u8 = 5;
pub const DEFAULT_AUTO_SWITCH_POLL_SECONDS: u64 = 60;
pub const DEFAULT_USAGE_WINDOW_POLL_SECONDS: u64 = 300;

fn default_near_limit_threshold_percent() -> u8 {
    DEFAULT_NEAR_LIMIT_THRESHOLD_PERCENT
}

fn default_auto_switch_poll_seconds() -> u64 {
    DEFAULT_AUTO_SWITCH_POLL_SECONDS
}

fn default_show_quota_in_menu_bar() -> bool {
    true
}

fn default_ui_language() -> String {
    "auto".to_owned()
}

/// Preference stored in settings: `"auto"`, `"en"`, or `"vi"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedUiLanguage {
    En,
    Vi,
}

impl ResolvedUiLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Vi => "vi",
        }
    }
}

pub fn normalize_ui_language(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("auto"),
        "en" | "en-us" | "en_gb" | "english" => Some("en"),
        "vi" | "vi-vn" | "vi_vn" | "vietnamese" => Some("vi"),
        _ => None,
    }
}

pub fn resolve_ui_language(preference: &str) -> ResolvedUiLanguage {
    match normalize_ui_language(preference).unwrap_or("auto") {
        "en" => ResolvedUiLanguage::En,
        "vi" => ResolvedUiLanguage::Vi,
        _ => {
            if system_prefers_vietnamese() {
                ResolvedUiLanguage::Vi
            } else {
                ResolvedUiLanguage::En
            }
        }
    }
}

pub fn system_prefers_vietnamese() -> bool {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            let lower = value.to_ascii_lowercase();
            if lower.starts_with("vi") || lower.contains(".vi") {
                return true;
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
            && output.status.success()
        {
            let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
            if text.contains("\"vi") || text.contains("vi-") || text.contains("vi_") {
                return true;
            }
        }
    }

    false
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default)]
    pub auto_start_usage_windows: bool,
    #[serde(default)]
    pub auto_switch_on_limit: bool,
    #[serde(default)]
    pub launch_at_startup: bool,
    /// Switch proactively when any quota window drops to this remaining % or below.
    #[serde(default = "default_near_limit_threshold_percent")]
    pub near_limit_threshold_percent: u8,
    /// How often to poll usage when auto-switch is enabled.
    #[serde(default = "default_auto_switch_poll_seconds")]
    pub auto_switch_poll_seconds: u64,
    /// Show quota percentage/status directly in the menu bar / tray icon.
    #[serde(default = "default_show_quota_in_menu_bar")]
    pub show_quota_in_menu_bar: bool,
    #[serde(default)]
    pub disable_blocker_warnings: bool,
    /// UI language preference: `auto` (system), `en`, or `vi`.
    #[serde(default = "default_ui_language")]
    pub ui_language: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_start_usage_windows: false,
            auto_switch_on_limit: false,
            launch_at_startup: false,
            near_limit_threshold_percent: DEFAULT_NEAR_LIMIT_THRESHOLD_PERCENT,
            auto_switch_poll_seconds: DEFAULT_AUTO_SWITCH_POLL_SECONDS,
            show_quota_in_menu_bar: true,
            disable_blocker_warnings: false,
            ui_language: default_ui_language(),
        }
    }
}

pub fn load_settings(app_data_dir: &Path) -> Result<AppSettings> {
    let path = settings_path(app_data_dir);
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_settings(app_data_dir: &Path, settings: &AppSettings) -> Result<()> {
    fs::create_dir_all(app_data_dir)
        .with_context(|| format!("failed to create {}", app_data_dir.display()))?;
    let path = settings_path(app_data_dir);
    let bytes = serde_json::to_vec_pretty(settings).context("failed to encode settings")?;
    replace_file_with_recovery(&path, Some(&bytes), |temp_path| {
        fs::write(temp_path, &bytes)
            .with_context(|| format!("failed to write {}", temp_path.display()))
    })?;
    Ok(())
}

fn settings_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn missing_settings_default_to_disabled() {
        let temp = tempdir().expect("tempdir");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(!settings.auto_start_usage_windows);
        assert!(!settings.auto_switch_on_limit);
        assert!(!settings.launch_at_startup);
        assert_eq!(
            settings.near_limit_threshold_percent,
            DEFAULT_NEAR_LIMIT_THRESHOLD_PERCENT
        );
        assert_eq!(
            settings.auto_switch_poll_seconds,
            DEFAULT_AUTO_SWITCH_POLL_SECONDS
        );
        assert!(settings.show_quota_in_menu_bar);
        assert_eq!(settings.ui_language, "auto");
    }

    #[test]
    fn saved_settings_round_trip() {
        let temp = tempdir().expect("tempdir");
        save_settings(
            temp.path(),
            &AppSettings {
                auto_start_usage_windows: true,
                auto_switch_on_limit: true,
                launch_at_startup: true,
                show_quota_in_menu_bar: false,
                ui_language: "vi".to_owned(),
                ..AppSettings::default()
            },
        )
        .expect("save settings");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
        assert!(settings.auto_switch_on_limit);
        assert!(settings.launch_at_startup);
        assert!(!settings.show_quota_in_menu_bar);
        assert_eq!(settings.ui_language, "vi");
    }

    #[test]
    fn resolve_ui_language_honors_explicit_preference() {
        assert_eq!(resolve_ui_language("en"), ResolvedUiLanguage::En);
        assert_eq!(resolve_ui_language("vi"), ResolvedUiLanguage::Vi);
        assert_eq!(normalize_ui_language("VI-vn"), Some("vi"));
    }

    #[test]
    fn malformed_settings_default_to_disabled() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path()).expect("settings dir");
        std::fs::write(settings_path(temp.path()), "{").expect("settings file");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(!settings.auto_start_usage_windows);
        assert!(!settings.auto_switch_on_limit);
        assert!(!settings.launch_at_startup);
        assert_eq!(
            settings.near_limit_threshold_percent,
            DEFAULT_NEAR_LIMIT_THRESHOLD_PERCENT
        );
        assert_eq!(
            settings.auto_switch_poll_seconds,
            DEFAULT_AUTO_SWITCH_POLL_SECONDS
        );
        assert!(settings.show_quota_in_menu_bar);
    }
}
