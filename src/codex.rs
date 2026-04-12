use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

use crate::env::AppEnv;
use crate::identity::parse_identity_from_auth_json;
use crate::model::{
    AUTH_FILES, DisplayIdentity, SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile,
};

#[derive(Clone, Debug)]
pub struct LiveAuthBundle {
    pub identity: DisplayIdentity,
    pub snapshot: SnapshotBlob,
}

pub fn try_read_live_auth_bundle(env: &AppEnv) -> Result<Option<LiveAuthBundle>> {
    let auth_json_path = env.codex_root.join("auth.json");
    if !auth_json_path.exists() {
        return Ok(None);
    }
    read_live_auth_bundle(env).map(Some)
}

pub fn read_live_auth_bundle(env: &AppEnv) -> Result<LiveAuthBundle> {
    let mut files = Vec::with_capacity(AUTH_FILES.len());
    let mut auth_json_bytes = None;
    for file_name in AUTH_FILES {
        let path = env.codex_root.join(file_name);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        if file_name == "auth.json" {
            auth_json_bytes = Some(bytes.clone());
        }
        files.push(SnapshotFile {
            name: file_name.to_owned(),
            bytes_base64: STANDARD.encode(bytes),
        });
    }
    let auth_json_bytes = auth_json_bytes.context("auth.json missing from live auth bundle")?;
    let identity = parse_identity_from_auth_json(&auth_json_bytes)?;
    Ok(LiveAuthBundle {
        identity,
        snapshot: SnapshotBlob {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            files,
        },
    })
}

pub fn identity_from_snapshot(snapshot: &SnapshotBlob) -> Result<DisplayIdentity> {
    let auth_file = snapshot
        .files
        .iter()
        .find(|file| file.name == "auth.json")
        .context("snapshot missing auth.json")?;
    let auth_json_bytes = STANDARD
        .decode(&auth_file.bytes_base64)
        .context("failed to decode snapshot auth.json")?;
    parse_identity_from_auth_json(&auth_json_bytes)
}

pub fn restore_snapshot(
    env: &AppEnv,
    snapshot: &SnapshotBlob,
    expected_identity: &DisplayIdentity,
    verify_stable: bool,
) -> Result<()> {
    ensure_snapshot_complete(snapshot)?;
    fs::create_dir_all(&env.codex_root)
        .with_context(|| format!("failed to create {}", env.codex_root.display()))?;
    let backup_dir = env
        .codex_root
        .join(format!(".cas-backup-{}", Uuid::new_v4()));
    let temp_dir = env
        .codex_root
        .join(format!(".cas-restore-{}", Uuid::new_v4()));
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("failed to create {}", backup_dir.display()))?;
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;

    if let Err(error) = stage_and_restore(&env.codex_root, &backup_dir, &temp_dir, snapshot) {
        let _ = restore_from_backup(&env.codex_root, &backup_dir);
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    if let Err(error) = verify_live_snapshot_once(env, snapshot, expected_identity) {
        let _ = restore_from_backup(&env.codex_root, &backup_dir);
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    if verify_stable
        && let Err(error) = verify_live_snapshot_stable_with_retry(
            env,
            snapshot,
            expected_identity,
            4,
            Duration::from_millis(250),
        )
    {
        let _ = restore_from_backup(&env.codex_root, &backup_dir);
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    let _ = fs::remove_dir_all(&temp_dir);
    let _ = fs::remove_dir_all(&backup_dir);
    Ok(())
}

pub fn live_bundle_matches_snapshot(env: &AppEnv, snapshot: &SnapshotBlob) -> Result<bool> {
    let Some(live) = try_read_live_auth_bundle(env)? else {
        return Ok(false);
    };
    Ok(snapshot_matches(&live.snapshot, snapshot))
}

pub fn verify_live_snapshot_stable(
    env: &AppEnv,
    expected_snapshot: &SnapshotBlob,
    expected_identity: &DisplayIdentity,
) -> Result<()> {
    verify_live_snapshot_stable_with_retry(
        env,
        expected_snapshot,
        expected_identity,
        4,
        Duration::from_millis(250),
    )
}

fn ensure_snapshot_complete(snapshot: &SnapshotBlob) -> Result<()> {
    for file_name in AUTH_FILES {
        if !snapshot.files.iter().any(|file| file.name == file_name) {
            return Err(anyhow!("snapshot missing managed auth file {file_name}"));
        }
    }
    Ok(())
}

fn stage_and_restore(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    snapshot: &SnapshotBlob,
) -> Result<()> {
    for file in &snapshot.files {
        let decoded = STANDARD
            .decode(&file.bytes_base64)
            .with_context(|| format!("failed to decode snapshot file {}", file.name))?;
        let temp_path = temp_dir.join(&file.name);
        fs::write(&temp_path, decoded)
            .with_context(|| format!("failed to stage {}", temp_path.display()))?;
    }

    for file_name in AUTH_FILES {
        let live_path = codex_root.join(file_name);
        if live_path.exists() {
            let backup_path = backup_dir.join(file_name);
            fs::copy(&live_path, &backup_path).with_context(|| {
                format!(
                    "failed to back up {} to {}",
                    live_path.display(),
                    backup_path.display()
                )
            })?;
            fs::remove_file(&live_path)
                .with_context(|| format!("failed to remove {}", live_path.display()))?;
        }
        let staged_path = temp_dir.join(file_name);
        fs::copy(&staged_path, &live_path).with_context(|| {
            format!(
                "failed to restore {} from {}",
                live_path.display(),
                staged_path.display()
            )
        })?;
    }
    Ok(())
}

fn restore_from_backup(codex_root: &Path, backup_dir: &Path) -> Result<()> {
    for file_name in AUTH_FILES {
        let backup_path = backup_dir.join(file_name);
        let live_path = codex_root.join(file_name);
        if backup_path.exists() {
            if live_path.exists() {
                fs::remove_file(&live_path)
                    .with_context(|| format!("failed to remove {}", live_path.display()))?;
            }
            fs::copy(&backup_path, &live_path).with_context(|| {
                format!(
                    "failed to restore backup {} to {}",
                    backup_path.display(),
                    live_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn verify_live_snapshot_stable_with_retry(
    env: &AppEnv,
    expected_snapshot: &SnapshotBlob,
    expected_identity: &DisplayIdentity,
    polls: usize,
    delay: Duration,
) -> Result<()> {
    verify_live_snapshot_once(env, expected_snapshot, expected_identity)?;
    for _ in 0..polls {
        std::thread::sleep(delay);
        verify_live_snapshot_once(env, expected_snapshot, expected_identity)
            .context("restored auth bundle changed again after activation")?;
    }
    Ok(())
}

fn verify_live_snapshot_once(
    env: &AppEnv,
    expected_snapshot: &SnapshotBlob,
    expected_identity: &DisplayIdentity,
) -> Result<()> {
    let live = read_live_auth_bundle(env).context("failed to verify restored auth bundle")?;
    if !snapshot_matches(&live.snapshot, expected_snapshot) {
        bail!(
            "restore verification failed: managed auth files no longer match the restored snapshot"
        );
    }
    if !live.identity.matches(expected_identity) {
        bail!(
            "restore verification failed: expected {:?}, got {:?}",
            expected_identity,
            live.identity
        );
    }
    Ok(())
}

fn snapshot_matches(left: &SnapshotBlob, right: &SnapshotBlob) -> bool {
    left.schema_version == right.schema_version
        && AUTH_FILES.iter().all(|file_name| {
            let left_files = snapshot_files(left, file_name);
            let right_files = snapshot_files(right, file_name);
            left_files.len() == 1 && right_files.len() == 1 && left_files[0] == right_files[0]
        })
}

fn snapshot_files<'a>(snapshot: &'a SnapshotBlob, file_name: &str) -> Vec<&'a str> {
    snapshot
        .files
        .iter()
        .filter(|file| file.name == file_name)
        .map(|file| file.bytes_base64.as_str())
        .collect()
}

#[cfg(test)]
pub fn auth_json_fixture(email: &str, subject: &str, plan: Option<&str>) -> String {
    let payload = serde_json::json!({
        "email": email,
        "sub": subject,
        "name": "Tester",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": plan
        }
    });
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
    serde_json::json!({
        "tokens": {
            "id_token": format!("{header}.{payload}."),
            "access_token": "access",
            "refresh_token": "refresh",
            "account_id": "acct"
        },
        "auth_mode": "chatgpt"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;

    use super::*;
    use crate::model::EnvironmentKind;

    #[test]
    fn reads_bundle_and_restores_it() -> Result<()> {
        let temp = tempdir()?;
        let codex_root = temp.path().join(".codex");
        fs::create_dir_all(&codex_root)?;
        fs::write(
            codex_root.join("auth.json"),
            auth_json_fixture("person@example.com", "sub-1", Some("pro")),
        )?;
        fs::write(codex_root.join("cap_sid"), "sid-1")?;
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: codex_root.clone(),
            app_data_dir: temp.path().join("data"),
        };
        let bundle = read_live_auth_bundle(&env)?;
        fs::write(
            codex_root.join("auth.json"),
            auth_json_fixture("other@example.com", "sub-2", Some("plus")),
        )?;
        fs::write(codex_root.join("cap_sid"), "sid-2")?;
        restore_snapshot(&env, &bundle.snapshot, &bundle.identity, false)?;
        let restored = read_live_auth_bundle(&env)?;
        assert_eq!(restored.identity.email, "person@example.com");
        Ok(())
    }

    #[test]
    fn restore_verification_uses_case_insensitive_email_when_subject_missing() -> Result<()> {
        let temp = tempdir()?;
        let codex_root = temp.path().join(".codex");
        fs::create_dir_all(&codex_root)?;
        fs::write(
            codex_root.join("auth.json"),
            auth_json_fixture("Person@Example.com", "sub-1", Some("pro")),
        )?;
        fs::write(codex_root.join("cap_sid"), "sid-1")?;
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: codex_root.clone(),
            app_data_dir: temp.path().join("data"),
        };
        let bundle = read_live_auth_bundle(&env)?;
        let expected = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: None,
            name: bundle.identity.name.clone(),
            plan_label: bundle.identity.plan_label.clone(),
        };
        restore_snapshot(&env, &bundle.snapshot, &expected, false)?;
        Ok(())
    }

    #[test]
    fn stable_verification_fails_when_auth_reverts() -> Result<()> {
        let temp = tempdir()?;
        let codex_root = temp.path().join(".codex");
        fs::create_dir_all(&codex_root)?;
        fs::write(
            codex_root.join("auth.json"),
            auth_json_fixture("after@example.com", "sub-2", Some("plus")),
        )?;
        fs::write(codex_root.join("cap_sid"), "sid-2")?;
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: codex_root.clone(),
            app_data_dir: temp.path().join("data"),
        };
        let expected = DisplayIdentity {
            email: "after@example.com".to_owned(),
            subject: Some("sub-2".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Plus".to_owned()),
        };
        let expected_snapshot = read_live_auth_bundle(&env)?.snapshot;
        let auth_path = codex_root.join("auth.json");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            fs::write(
                auth_path,
                auth_json_fixture("before@example.com", "sub-1", Some("pro")),
            )
            .expect("rewrite auth");
        });

        let error = verify_live_snapshot_stable_with_retry(
            &env,
            &expected_snapshot,
            &expected,
            10,
            Duration::from_millis(10),
        )
        .expect_err("verification should fail after revert");
        assert!(format!("{error:#}").contains("changed again after activation"));
        Ok(())
    }

    #[test]
    fn stable_verification_fails_when_cap_sid_reverts() -> Result<()> {
        let temp = tempdir()?;
        let codex_root = temp.path().join(".codex");
        fs::create_dir_all(&codex_root)?;
        fs::write(
            codex_root.join("auth.json"),
            auth_json_fixture("after@example.com", "sub-2", Some("plus")),
        )?;
        fs::write(codex_root.join("cap_sid"), "sid-2")?;
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: codex_root.clone(),
            app_data_dir: temp.path().join("data"),
        };
        let expected = DisplayIdentity {
            email: "after@example.com".to_owned(),
            subject: Some("sub-2".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Plus".to_owned()),
        };
        let expected_snapshot = read_live_auth_bundle(&env)?.snapshot;
        let cap_sid_path = codex_root.join("cap_sid");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            fs::write(cap_sid_path, "sid-1").expect("rewrite cap sid");
        });

        let error = verify_live_snapshot_stable_with_retry(
            &env,
            &expected_snapshot,
            &expected,
            10,
            Duration::from_millis(10),
        )
        .expect_err("verification should fail after cap_sid drift");
        assert!(format!("{error:#}").contains("changed again after activation"));
        Ok(())
    }

    #[test]
    fn snapshot_match_rejects_duplicate_managed_files() {
        let left = SnapshotBlob {
            schema_version: 1,
            files: vec![
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-a".to_owned(),
                },
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "sid-a".to_owned(),
                },
            ],
        };
        let right = SnapshotBlob {
            schema_version: 1,
            files: vec![
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-a".to_owned(),
                },
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-b".to_owned(),
                },
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "sid-a".to_owned(),
                },
            ],
        };

        assert!(!snapshot_matches(&left, &right));
        assert!(!snapshot_matches(&right, &left));
    }
}
