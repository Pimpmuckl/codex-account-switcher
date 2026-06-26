use std::path::Path;

use anyhow::{Context, Result};

pub fn restrict_store_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }

    #[cfg(windows)]
    {
        restrict_windows_path(path, true)?;
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
    }

    Ok(())
}

pub fn restrict_store_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }

    #[cfg(windows)]
    {
        restrict_windows_path(path, false)?;
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
    }

    Ok(())
}

#[cfg(windows)]
fn restrict_windows_path(path: &Path, is_dir: bool) -> Result<()> {
    let username = std::env::var("USERNAME").context("USERNAME is not set")?;
    let grant = if is_dir {
        format!("{username}:(OI)(CI)F")
    } else {
        format!("{username}:F")
    };
    let output = std::process::Command::new("icacls")
        .arg(path.as_os_str())
        .args(["/inheritance:r", "/grant:r", &grant])
        .output()
        .with_context(|| format!("failed to run icacls for {}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "icacls failed for {}: {}",
            path.display(),
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn restrict_store_dir_and_file_round_trip_on_current_platform() -> Result<()> {
        let temp = tempdir()?;
        let store_dir = temp.path().join("snapshots");
        std::fs::create_dir_all(&store_dir)?;
        restrict_store_dir(&store_dir)?;

        let file = store_dir.join("secret.snapshot");
        std::fs::write(&file, b"secret")?;
        restrict_store_file(&file)?;

        let metadata = std::fs::metadata(&file)?;
        assert!(metadata.is_file());
        Ok(())
    }
}
