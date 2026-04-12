use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryFileKind {
    Canonical,
    Pending,
    Temp,
    Backup,
}

#[derive(Clone, Debug)]
pub struct RecoveryFile {
    pub path: PathBuf,
    pub kind: RecoveryFileKind,
    pub modified: SystemTime,
}

pub fn list_recovery_files(
    canonical_path: &Path,
    pending_path: Option<&Path>,
) -> Result<Vec<RecoveryFile>> {
    let Some(parent) = canonical_path.parent() else {
        return Ok(Vec::new());
    };
    if !parent.exists() {
        return Ok(Vec::new());
    }
    let Some(file_name) = canonical_path.file_name().and_then(|value| value.to_str()) else {
        return Ok(Vec::new());
    };
    let canonical_name = file_name.to_owned();
    let backup_prefix = format!("{file_name}.bak-");
    let temp_prefix = format!("{file_name}.tmp-");

    let mut files = fs::read_dir(parent)
        .with_context(|| format!("failed to read {}", parent.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let kind = if name == canonical_name {
                RecoveryFileKind::Canonical
            } else if name.starts_with(&backup_prefix) {
                RecoveryFileKind::Backup
            } else if name.starts_with(&temp_prefix) {
                RecoveryFileKind::Temp
            } else {
                return None;
            };
            Some(RecoveryFile {
                modified: entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                path: entry.path(),
                kind,
            })
        })
        .collect::<Vec<_>>();

    if let Some(pending_path) = pending_path
        && pending_path.exists()
    {
        files.push(RecoveryFile {
            modified: fs::metadata(pending_path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH),
            path: pending_path.to_path_buf(),
            kind: RecoveryFileKind::Pending,
        });
    }

    Ok(files)
}

pub fn replace_file_with_recovery<F>(
    canonical_path: &Path,
    pending_write: Option<&[u8]>,
    mut write_temp: F,
) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    replace_file_with_recovery_impl(
        canonical_path,
        pending_write,
        &mut write_temp,
        cleanup_paths,
    )
}

fn replace_file_with_recovery_impl<F, C>(
    canonical_path: &Path,
    pending_write: Option<&[u8]>,
    write_temp: &mut F,
    cleanup: C,
) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
    C: Fn(&[&Path]) -> Result<()>,
{
    if let Some(parent) = canonical_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let pending_path = canonical_path.with_extension(format!(
        "{}.pending",
        canonical_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
    ));
    if let Some(contents) = pending_write {
        fs::write(&pending_path, contents)
            .with_context(|| format!("failed to write {}", pending_path.display()))?;
    }

    let file_stem = canonical_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let parent = canonical_path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = parent.join(format!("{file_stem}.tmp-{}", Uuid::new_v4().simple()));
    let backup_path = parent.join(format!("{file_stem}.bak-{}", Uuid::new_v4().simple()));

    if let Err(error) = write_temp(&temp_path) {
        cleanup_selected(
            &cleanup,
            [
                Some(temp_path.as_path()),
                Some(pending_path.as_path()),
                None,
            ],
        )?;
        return Err(error);
    }

    if !canonical_path.exists() {
        if let Err(error) = fs::rename(&temp_path, canonical_path) {
            cleanup_selected(
                &cleanup,
                [
                    Some(temp_path.as_path()),
                    Some(pending_path.as_path()),
                    None,
                ],
            )?;
            return Err(error)
                .with_context(|| format!("failed to persist {}", canonical_path.display()));
        }
        cleanup_best_effort(
            &cleanup,
            [
                Some(temp_path.as_path()),
                Some(pending_path.as_path()),
                None,
            ],
        );
        return Ok(());
    }

    if let Err(error) = fs::rename(canonical_path, &backup_path) {
        cleanup_selected(
            &cleanup,
            [
                Some(temp_path.as_path()),
                Some(pending_path.as_path()),
                None,
            ],
        )?;
        return Err(error).with_context(|| {
            format!(
                "failed to prepare existing file {} for replacement",
                canonical_path.display()
            )
        });
    }

    match fs::rename(&temp_path, canonical_path) {
        Ok(()) => {
            cleanup_best_effort(
                &cleanup,
                [
                    Some(temp_path.as_path()),
                    Some(pending_path.as_path()),
                    Some(backup_path.as_path()),
                ],
            );
            Ok(())
        }
        Err(error) => {
            let rollback_succeeded = fs::rename(&backup_path, canonical_path).is_ok();
            if rollback_succeeded {
                cleanup_best_effort(
                    &cleanup,
                    [
                        Some(temp_path.as_path()),
                        Some(pending_path.as_path()),
                        Some(backup_path.as_path()),
                    ],
                );
            }
            Err(error).with_context(|| format!("failed to persist {}", canonical_path.display()))
        }
    }
}

fn cleanup_selected<'a, C>(
    cleanup: &C,
    paths: impl IntoIterator<Item = Option<&'a Path>>,
) -> Result<()>
where
    C: Fn(&[&Path]) -> Result<()>,
{
    let paths = paths.into_iter().flatten().collect::<Vec<_>>();
    cleanup(&paths)
}

fn cleanup_best_effort<'a, C>(cleanup: &C, paths: impl IntoIterator<Item = Option<&'a Path>>)
where
    C: Fn(&[&Path]) -> Result<()>,
{
    let _ = cleanup_selected(cleanup, paths);
}

pub fn cleanup_paths(paths: &[&Path]) -> Result<()> {
    let mut first_error = None;
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(
                        Err(error).with_context(|| format!("failed to delete {}", path.display())),
                    );
                }
            }
        }
    }
    first_error.unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::anyhow;
    use tempfile::tempdir;

    use super::{cleanup_paths, replace_file_with_recovery_impl};

    #[test]
    fn save_without_existing_file_ignores_cleanup_failure_after_persist() {
        let temp_dir = tempdir().expect("temp dir");
        let canonical_path = temp_dir.path().join("snapshot.bin");

        let result = replace_file_with_recovery_impl(
            &canonical_path,
            Some(b"pending"),
            &mut |temp_path| fs::write(temp_path, b"snapshot").map_err(Into::into),
            |_| Err(anyhow!("cleanup failed")),
        );

        assert!(result.is_ok());
        assert_eq!(
            fs::read(&canonical_path).expect("canonical file"),
            b"snapshot"
        );
    }

    #[test]
    fn save_with_existing_file_ignores_cleanup_failure_after_persist() {
        let temp_dir = tempdir().expect("temp dir");
        let canonical_path = temp_dir.path().join("snapshot.bin");
        fs::write(&canonical_path, b"before").expect("seed canonical file");

        let result = replace_file_with_recovery_impl(
            &canonical_path,
            Some(b"pending"),
            &mut |temp_path| fs::write(temp_path, b"after").map_err(Into::into),
            |_| Err(anyhow!("cleanup failed")),
        );

        assert!(result.is_ok());
        assert_eq!(fs::read(&canonical_path).expect("canonical file"), b"after");
    }

    #[test]
    fn cleanup_paths_attempts_later_deletes_after_first_error() {
        let temp_dir = tempdir().expect("temp dir");
        let blocked_dir = temp_dir.path().join("blocked");
        let trailing_file = temp_dir.path().join("trailing.tmp");
        fs::create_dir(&blocked_dir).expect("blocked dir");
        fs::write(&trailing_file, b"cleanup").expect("trailing file");

        let error = cleanup_paths(&[blocked_dir.as_path(), trailing_file.as_path()])
            .expect_err("directory delete should fail");

        assert!(format!("{error:#}").contains("failed to delete"));
        assert!(!trailing_file.exists());
    }
}
