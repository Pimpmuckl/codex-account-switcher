use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default)]
    pub auto_start_usage_windows: bool,
}

pub fn load_settings(app_data_dir: &Path) -> Result<AppSettings> {
    let path = settings_path(app_data_dir);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_settings(app_data_dir: &Path, settings: &AppSettings) -> Result<()> {
    fs::create_dir_all(app_data_dir)
        .with_context(|| format!("failed to create {}", app_data_dir.display()))?;
    let path = settings_path(app_data_dir);
    let bytes = serde_json::to_vec_pretty(settings).context("failed to encode settings")?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
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
    }

    #[test]
    fn saved_settings_round_trip() {
        let temp = tempdir().expect("tempdir");
        save_settings(
            temp.path(),
            &AppSettings {
                auto_start_usage_windows: true,
            },
        )
        .expect("save settings");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
    }
}
