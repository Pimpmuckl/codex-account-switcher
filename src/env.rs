use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use directories::{BaseDirs, ProjectDirs};

use crate::model::EnvironmentKind;

#[derive(Clone, Debug)]
pub struct AppEnv {
    pub kind: EnvironmentKind,
    pub home_dir: PathBuf,
    pub codex_root: PathBuf,
    pub app_data_dir: PathBuf,
}

pub fn detect() -> Result<AppEnv> {
    let base_dirs = BaseDirs::new().context("could not resolve home directory")?;
    let project_dirs = ProjectDirs::from("com", "nextide", "codex-account-switcher")
        .context("could not resolve app data directory")?;
    let home_dir = base_dirs.home_dir().to_path_buf();
    let kind = detect_environment_kind()?;
    Ok(AppEnv {
        kind,
        codex_root: home_dir.join(".codex"),
        home_dir,
        app_data_dir: project_dirs.data_local_dir().to_path_buf(),
    })
}

fn detect_environment_kind() -> Result<EnvironmentKind> {
    if cfg!(target_os = "windows") {
        return Ok(EnvironmentKind::Windows);
    }
    if cfg!(target_os = "macos") {
        return Ok(EnvironmentKind::Macos);
    }
    if cfg!(target_os = "linux") {
        if std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
        {
            return Ok(EnvironmentKind::Wsl);
        }
        if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            && contents.to_ascii_lowercase().contains("microsoft")
        {
            return Ok(EnvironmentKind::Wsl);
        }
        return Ok(EnvironmentKind::Linux);
    }
    bail!("unsupported operating system")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_runtime_environment() {
        let env = detect().expect("env");
        match env.kind {
            EnvironmentKind::Windows
            | EnvironmentKind::Wsl
            | EnvironmentKind::Linux
            | EnvironmentKind::Macos => {}
        }
        assert!(env.codex_root.ends_with(".codex"));
    }
}
