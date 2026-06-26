use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::file_store::{RecoveryFileKind, list_recovery_files, replace_file_with_recovery};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default)]
    pub auto_start_usage_windows: bool,
    #[serde(default)]
    pub auto_switch_on_limit: bool,
    #[serde(default)]
    pub launch_at_startup: bool,
}

pub fn load_settings(app_data_dir: &Path) -> Result<AppSettings> {
    let path = settings_path(app_data_dir);
    let mut saw_invalid = false;
    let mut canonical_error = None;
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(settings) => return Ok(settings),
            Err(_) => saw_invalid = true,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            canonical_error =
                Some(Err(error).with_context(|| format!("failed to read {}", path.display())));
        }
    }

    let pending_path = pending_settings_path(&path);
    let entries = list_recovery_files(&path, Some(&pending_path))?;
    if entries.is_empty() && canonical_error.is_none() && !saw_invalid {
        return Ok(AppSettings::default());
    }

    let mut recovery_candidates = Vec::new();
    for entry in entries {
        if entry.kind == RecoveryFileKind::Canonical {
            continue;
        }
        let bytes = match fs::read(&entry.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        let settings = match serde_json::from_slice::<AppSettings>(&bytes) {
            Ok(settings) => settings,
            Err(_) => {
                saw_invalid = true;
                continue;
            }
        };
        recovery_candidates.push(SettingsRecoveryCandidate {
            modified: entry.modified,
            settings,
        });
    }

    if let Some(candidate) = recovery_candidates
        .into_iter()
        .max_by_key(|candidate| candidate.modified)
    {
        save_settings(app_data_dir, &candidate.settings)?;
        return Ok(candidate.settings);
    }

    if saw_invalid {
        return Err(settings_recovery_error(&path));
    }

    if let Some(error) = canonical_error {
        return error;
    }

    Ok(AppSettings::default())
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

fn pending_settings_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}.pending",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
    ))
}

fn settings_recovery_error(path: &Path) -> anyhow::Error {
    anyhow!(
        "settings recovery failed for {}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings.json")
    )
}

struct SettingsRecoveryCandidate {
    modified: SystemTime,
    settings: AppSettings,
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
            },
        )
        .expect("save settings");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
        assert!(settings.auto_switch_on_limit);
        assert!(settings.launch_at_startup);
    }

    #[test]
    fn valid_settings_ignore_stale_unreadable_recovery_artifact() {
        let temp = tempdir().expect("tempdir");
        save_settings(
            temp.path(),
            &AppSettings {
                auto_start_usage_windows: true,
            },
        )
        .expect("save settings");
        std::fs::create_dir(temp.path().join("settings.json.bak-stale"))
            .expect("stale backup directory");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
    }

    #[test]
    fn missing_settings_ignore_stale_unreadable_recovery_artifact() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join("settings.json.bak-stale"))
            .expect("stale backup directory");

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(!settings.auto_start_usage_windows);
        assert!(!settings.auto_switch_on_limit);
        assert!(!settings.launch_at_startup);
    }

    #[test]
    fn malformed_settings_with_valid_backup_recovers_enabled() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path()).expect("settings dir");
        std::fs::write(settings_path(temp.path()), "{").expect("settings file");
        write_settings(
            &temp.path().join("settings.json.bak-test"),
            &AppSettings {
                auto_start_usage_windows: true,
            },
        );

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
        assert_eq!(
            serde_json::from_slice::<AppSettings>(
                &std::fs::read(settings_path(temp.path())).expect("canonical settings")
            )
            .expect("parse canonical settings"),
            settings
        );
    }

    #[test]
    fn unreadable_canonical_with_valid_backup_recovers_enabled() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join("settings.json")).expect("settings directory");
        write_settings(
            &temp.path().join("settings.json.bak-test"),
            &AppSettings {
                auto_start_usage_windows: true,
            },
        );

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
    }

    #[test]
    fn malformed_settings_with_valid_pending_recovers_enabled() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path()).expect("settings dir");
        let path = settings_path(temp.path());
        std::fs::write(&path, "{").expect("settings file");
        write_settings(
            &pending_settings_path(&path),
            &AppSettings {
                auto_start_usage_windows: true,
            },
        );

        let settings = load_settings(temp.path()).expect("load settings");

        assert!(settings.auto_start_usage_windows);
    }

    #[test]
    fn malformed_settings_returns_error_without_recovery_candidate() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path()).expect("settings dir");
        std::fs::write(settings_path(temp.path()), "{").expect("settings file");

        let error = load_settings(temp.path()).expect_err("invalid settings should fail");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("settings.json"));
        assert!(rendered.contains("settings recovery failed"));
    }

    fn write_settings(path: &Path, settings: &AppSettings) {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(settings).expect("encode settings"),
        )
        .expect("write settings");
    }
}
