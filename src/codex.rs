use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
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

const RESTORE_MANIFEST_FILE: &str = ".cas-restore-manifest.json";
const COMMITTED_RESTORE_MANIFEST_FILE: &str = ".cas-restore-manifest.committed.json";
const PREPARE_OWNER_FILE: &str = ".cas-restore-owner.json";
const RESTORE_MANIFEST_SCHEMA_VERSION: u32 = 1;
const BACKUP_DIR_PREFIX: &str = ".cas-backup-";
const CLEANUP_DIR_PREFIX: &str = ".cas-cleanup-";
const PREPARE_DIR_PREFIX: &str = ".cas-prepare-";
const RESTORE_DIR_PREFIX: &str = ".cas-restore-";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RestoreManifest {
    schema_version: u32,
    state: RestoreManifestState,
    owner_pid: u32,
    owner_start_time: u64,
    backup_dir_name: String,
    temp_dir_name: String,
    #[serde(default)]
    snapshot_verified: bool,
    #[serde(default)]
    stable_verification_required: bool,
    #[serde(default)]
    stable_verified: bool,
    managed_files: Vec<RestoreManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RestoreManifestState {
    InProgress,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RestoreManifestFile {
    name: String,
    existed_before: bool,
    original_bytes_base64: Option<String>,
    target_bytes_base64: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RestoreOwner {
    owner_pid: u32,
    owner_start_time: u64,
}

pub fn try_read_live_auth_bundle(env: &AppEnv) -> Result<Option<LiveAuthBundle>> {
    recover_interrupted_restores(&env.codex_root, false)?;
    try_read_live_auth_bundle_unchecked(env)
}

fn try_read_live_auth_bundle_unchecked(env: &AppEnv) -> Result<Option<LiveAuthBundle>> {
    let auth_json_path = env.codex_root.join("auth.json");
    if !auth_json_path.exists() {
        return Ok(None);
    }
    read_live_auth_bundle_unchecked(env).map(Some)
}

pub fn read_live_auth_bundle(env: &AppEnv) -> Result<LiveAuthBundle> {
    recover_interrupted_restores(&env.codex_root, false)?;
    read_live_auth_bundle_unchecked(env)
}

fn read_live_auth_bundle_unchecked(env: &AppEnv) -> Result<LiveAuthBundle> {
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
    validate_managed_snapshot(snapshot)?;
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
    validate_managed_snapshot(snapshot)?;
    validate_snapshot_file_bytes(snapshot)?;
    fs::create_dir_all(&env.codex_root)
        .with_context(|| format!("failed to create {}", env.codex_root.display()))?;
    recover_interrupted_restores(&env.codex_root, true)?;

    let backup_dir_name = format!("{BACKUP_DIR_PREFIX}{}", Uuid::new_v4());
    let backup_prepare_dir = env
        .codex_root
        .join(format!("{PREPARE_DIR_PREFIX}{}", Uuid::new_v4()));
    let backup_dir = env.codex_root.join(&backup_dir_name);
    let temp_dir = env
        .codex_root
        .join(format!("{RESTORE_DIR_PREFIX}{}", Uuid::new_v4()));
    fs::create_dir_all(&backup_prepare_dir)
        .with_context(|| format!("failed to create {}", backup_prepare_dir.display()))?;

    let mut manifest = restore_manifest(&env.codex_root, &backup_dir, &temp_dir, snapshot)?;
    manifest.stable_verification_required = verify_stable;
    write_prepare_owner(&backup_prepare_dir, &manifest)?;
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;
    if let Err(error) = write_restore_manifest(&backup_prepare_dir, &manifest) {
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&backup_prepare_dir);
        return Err(error);
    }
    if let Err(error) = fs::rename(&backup_prepare_dir, &backup_dir).with_context(|| {
        format!(
            "failed to prepare {} from {}",
            backup_dir.display(),
            backup_prepare_dir.display()
        )
    }) {
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&backup_prepare_dir);
        return Err(error);
    }

    if let Err(error) = stage_and_restore(&env.codex_root, &backup_dir, &temp_dir, snapshot) {
        let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
        let _ = retire_dir_if_exists(&temp_dir);
        let _ = retire_dir_if_exists(&backup_dir);
        return Err(error);
    }

    if let Err(error) = verify_live_snapshot_once(env, snapshot, expected_identity) {
        let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
        let _ = retire_dir_if_exists(&temp_dir);
        let _ = retire_dir_if_exists(&backup_dir);
        return Err(error);
    }
    manifest.snapshot_verified = true;
    if let Err(error) = replace_restore_manifest(&backup_dir, &manifest) {
        let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
        let _ = retire_dir_if_exists(&temp_dir);
        let _ = retire_dir_if_exists(&backup_dir);
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
        let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
        let _ = retire_dir_if_exists(&temp_dir);
        let _ = retire_dir_if_exists(&backup_dir);
        return Err(error);
    }
    if verify_stable {
        manifest.stable_verified = true;
        if let Err(error) = replace_restore_manifest(&backup_dir, &manifest) {
            let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
            let _ = retire_dir_if_exists(&temp_dir);
            let _ = retire_dir_if_exists(&backup_dir);
            return Err(error);
        }
    }

    manifest.state = RestoreManifestState::Committed;
    if let Err(error) = write_restore_manifest(&backup_dir, &manifest) {
        let _ = restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest);
        let _ = retire_dir_if_exists(&temp_dir);
        let _ = retire_dir_if_exists(&backup_dir);
        return Err(error);
    }

    let _ = retire_dir_if_exists(&temp_dir);
    let _ = retire_dir_if_exists(&backup_dir);
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
    recover_interrupted_restores(&env.codex_root, false)?;
    verify_live_snapshot_stable_with_retry(
        env,
        expected_snapshot,
        expected_identity,
        4,
        Duration::from_millis(250),
    )
}

pub(crate) fn validate_managed_snapshot(snapshot: &SnapshotBlob) -> Result<()> {
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        bail!(
            "unsupported snapshot schema version {} (expected {SNAPSHOT_SCHEMA_VERSION})",
            snapshot.schema_version
        );
    }

    let mut seen = Vec::with_capacity(AUTH_FILES.len());
    for file in &snapshot.files {
        validate_snapshot_file_name(&file.name)?;
        if !AUTH_FILES.contains(&file.name.as_str()) {
            bail!(
                "snapshot contains unexpected managed auth file {}",
                file.name
            );
        }
        if seen.contains(&file.name.as_str()) {
            bail!(
                "snapshot contains duplicate managed auth file {}",
                file.name
            );
        }
        seen.push(file.name.as_str());
    }

    for file_name in AUTH_FILES {
        if !seen.contains(&file_name) {
            return Err(anyhow!("snapshot missing managed auth file {file_name}"));
        }
    }
    Ok(())
}

fn validate_snapshot_file_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || Path::new(name).is_absolute()
    {
        bail!("snapshot contains invalid managed auth file name {name:?}");
    }
    Ok(())
}

fn validate_snapshot_file_bytes(snapshot: &SnapshotBlob) -> Result<()> {
    for file in &snapshot.files {
        STANDARD
            .decode(&file.bytes_base64)
            .with_context(|| format!("failed to decode snapshot file {}", file.name))?;
    }
    Ok(())
}

fn recover_interrupted_restores(codex_root: &Path, fail_on_live_in_progress: bool) -> Result<()> {
    if !codex_root.exists() {
        return Ok(());
    }

    remove_retired_cleanup_dirs(codex_root)?;
    remove_abandoned_prepare_dirs(codex_root)?;

    let mut backup_dirs = Vec::new();
    for entry in fs::read_dir(codex_root)
        .with_context(|| format!("failed to read {}", codex_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with(BACKUP_DIR_PREFIX)
        {
            backup_dirs.push(path);
        }
    }
    backup_dirs.sort();

    for backup_dir in backup_dirs {
        if !backup_dir.join(RESTORE_MANIFEST_FILE).exists()
            && !backup_dir.join(COMMITTED_RESTORE_MANIFEST_FILE).exists()
        {
            bail!(
                "interrupted restore backup {} has no restore manifest; inspect it before retrying",
                backup_dir.display()
            );
        }
        let manifest = read_restore_manifest(&backup_dir)?;
        validate_restore_manifest(codex_root, &backup_dir, &manifest)?;
        recover_restore_manifest(codex_root, &backup_dir, &manifest, fail_on_live_in_progress)?;
    }

    Ok(())
}

fn remove_retired_cleanup_dirs(codex_root: &Path) -> Result<()> {
    for entry in fs::read_dir(codex_root)
        .with_context(|| format!("failed to read {}", codex_root.display()))?
    {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with(CLEANUP_DIR_PREFIX)
        {
            remove_dir_if_exists(&path)?;
        }
    }
    Ok(())
}

fn remove_abandoned_prepare_dirs(codex_root: &Path) -> Result<()> {
    for entry in fs::read_dir(codex_root)
        .with_context(|| format!("failed to read {}", codex_root.display()))?
    {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with(PREPARE_DIR_PREFIX)
        {
            let manifest_path = path.join(RESTORE_MANIFEST_FILE);
            if !manifest_path.exists() {
                let Some(owner) = read_prepare_owner(&path).ok() else {
                    continue;
                };
                if restore_owner_is_live(owner.owner_pid, owner.owner_start_time) {
                    continue;
                }
                remove_dir_if_exists(&path)?;
                continue;
            }
            let manifest = match read_restore_manifest(&path) {
                Ok(manifest) => manifest,
                Err(_) => {
                    remove_dir_if_exists(&path)?;
                    continue;
                }
            };
            if !restore_owner_is_live(manifest.owner_pid, manifest.owner_start_time) {
                validate_restore_manifest(
                    codex_root,
                    &codex_root.join(&manifest.backup_dir_name),
                    &manifest,
                )?;
                retire_dir_if_exists(&codex_root.join(&manifest.temp_dir_name))?;
                remove_dir_if_exists(&path)?;
            }
        }
    }
    Ok(())
}

fn read_prepare_owner(prepare_dir: &Path) -> Result<RestoreOwner> {
    let path = prepare_dir.join(PREPARE_OWNER_FILE);
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_optional_file_base64(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(STANDARD.encode(bytes))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn recover_restore_manifest(
    codex_root: &Path,
    backup_dir: &Path,
    manifest: &RestoreManifest,
    fail_on_live_in_progress: bool,
) -> Result<()> {
    let temp_dir = codex_root.join(&manifest.temp_dir_name);
    match manifest.state {
        RestoreManifestState::InProgress => {
            if restore_owner_is_live(manifest.owner_pid, manifest.owner_start_time) {
                if !fail_on_live_in_progress {
                    return Ok(());
                }
                bail!(
                    "restore transaction {} is still in progress by process {}",
                    backup_dir.display(),
                    manifest.owner_pid
                );
            }
            recover_in_progress_manifest(codex_root, backup_dir, &temp_dir, manifest)
                .with_context(|| format!("failed to recover {}", backup_dir.display()))?;
        }
        RestoreManifestState::Committed => {
            if committed_manifest_should_restore_backup(
                codex_root, backup_dir, &temp_dir, manifest,
            )? {
                restore_from_manifest_backup(codex_root, backup_dir, manifest)
                    .with_context(|| format!("failed to recover {}", backup_dir.display()))?;
            }
        }
    }
    retire_dir_if_exists(&temp_dir)?;
    retire_dir_if_exists(backup_dir)?;
    Ok(())
}

fn restore_manifest(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    snapshot: &SnapshotBlob,
) -> Result<RestoreManifest> {
    let backup_dir_name = dir_name(backup_dir)?;
    let temp_dir_name = dir_name(temp_dir)?;
    let managed_files = AUTH_FILES
        .iter()
        .map(|file_name| {
            let snapshot_file = snapshot
                .files
                .iter()
                .find(|file| file.name == *file_name)
                .with_context(|| format!("snapshot missing managed auth file {file_name}"))?;
            Ok(RestoreManifestFile {
                name: (*file_name).to_owned(),
                existed_before: codex_root.join(file_name).exists(),
                original_bytes_base64: read_optional_file_base64(&codex_root.join(file_name))?,
                target_bytes_base64: snapshot_file.bytes_base64.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RestoreManifest {
        schema_version: RESTORE_MANIFEST_SCHEMA_VERSION,
        state: RestoreManifestState::InProgress,
        owner_pid: std::process::id(),
        owner_start_time: current_process_start_time()
            .context("failed to read current process start time")?,
        backup_dir_name,
        temp_dir_name,
        snapshot_verified: false,
        stable_verification_required: false,
        stable_verified: false,
        managed_files,
    })
}

fn read_restore_manifest(backup_dir: &Path) -> Result<RestoreManifest> {
    let path = restore_manifest_path(backup_dir);
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_restore_manifest(backup_dir: &Path, manifest: &RestoreManifest) -> Result<()> {
    validate_restore_dir_name(&manifest.backup_dir_name, BACKUP_DIR_PREFIX)?;
    validate_restore_dir_name(&manifest.temp_dir_name, RESTORE_DIR_PREFIX)?;
    let path = if manifest.state == RestoreManifestState::Committed
        && backup_dir.join(RESTORE_MANIFEST_FILE).exists()
    {
        backup_dir.join(COMMITTED_RESTORE_MANIFEST_FILE)
    } else {
        backup_dir.join(RESTORE_MANIFEST_FILE)
    };
    let bytes =
        serde_json::to_vec_pretty(manifest).context("failed to serialize restore manifest")?;
    if path.exists() {
        let existing =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        if existing == bytes {
            return Ok(());
        }
        bail!("restore manifest {} already exists", path.display());
    }

    write_manifest_bytes(&path, &bytes)
}

fn replace_restore_manifest(backup_dir: &Path, manifest: &RestoreManifest) -> Result<()> {
    validate_restore_dir_name(&manifest.backup_dir_name, BACKUP_DIR_PREFIX)?;
    validate_restore_dir_name(&manifest.temp_dir_name, RESTORE_DIR_PREFIX)?;
    let path = if manifest.state == RestoreManifestState::Committed
        && backup_dir.join(RESTORE_MANIFEST_FILE).exists()
    {
        backup_dir.join(COMMITTED_RESTORE_MANIFEST_FILE)
    } else {
        backup_dir.join(RESTORE_MANIFEST_FILE)
    };
    let bytes =
        serde_json::to_vec_pretty(manifest).context("failed to serialize restore manifest")?;
    write_manifest_bytes(&path, &bytes)
}

fn write_manifest_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let pending_path = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    fs::write(&pending_path, bytes)
        .with_context(|| format!("failed to write {}", pending_path.display()))?;
    fs::rename(&pending_path, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            pending_path.display()
        )
    })?;
    Ok(())
}

fn write_prepare_owner(prepare_dir: &Path, manifest: &RestoreManifest) -> Result<()> {
    let owner = RestoreOwner {
        owner_pid: manifest.owner_pid,
        owner_start_time: manifest.owner_start_time,
    };
    let path = prepare_dir.join(PREPARE_OWNER_FILE);
    let bytes = serde_json::to_vec_pretty(&owner).context("failed to serialize restore owner")?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn validate_restore_manifest(
    codex_root: &Path,
    backup_dir: &Path,
    manifest: &RestoreManifest,
) -> Result<()> {
    if manifest.schema_version != RESTORE_MANIFEST_SCHEMA_VERSION {
        bail!(
            "unsupported restore manifest schema version {} in {}",
            manifest.schema_version,
            backup_dir.display()
        );
    }
    if manifest.owner_pid == 0 {
        bail!(
            "restore manifest missing owner process id in {}",
            backup_dir.display()
        );
    }
    if manifest.owner_start_time == 0 {
        bail!(
            "restore manifest missing owner process start time in {}",
            backup_dir.display()
        );
    }
    let backup_dir_name = dir_name(backup_dir)?;
    if manifest.backup_dir_name != backup_dir_name {
        bail!(
            "restore manifest backup dir mismatch in {}",
            backup_dir.display()
        );
    }
    validate_restore_dir_name(&manifest.backup_dir_name, BACKUP_DIR_PREFIX)?;
    validate_restore_dir_name(&manifest.temp_dir_name, RESTORE_DIR_PREFIX)?;
    if codex_root.join(&manifest.temp_dir_name).parent() != Some(codex_root) {
        bail!("restore manifest temp dir escapes {}", codex_root.display());
    }

    let mut seen = Vec::with_capacity(AUTH_FILES.len());
    for file in &manifest.managed_files {
        validate_snapshot_file_name(&file.name)?;
        if !AUTH_FILES.contains(&file.name.as_str()) {
            bail!(
                "restore manifest contains unexpected auth file {}",
                file.name
            );
        }
        if seen.contains(&file.name.as_str()) {
            bail!(
                "restore manifest contains duplicate auth file {}",
                file.name
            );
        }
        STANDARD
            .decode(&file.target_bytes_base64)
            .with_context(|| {
                format!(
                    "restore manifest contains invalid target bytes for {}",
                    file.name
                )
            })?;
        if file.existed_before && file.original_bytes_base64.is_none() {
            bail!(
                "restore manifest missing original bytes for existing auth file {}",
                file.name
            );
        }
        if let Some(original_bytes_base64) = &file.original_bytes_base64 {
            STANDARD.decode(original_bytes_base64).with_context(|| {
                format!(
                    "restore manifest contains invalid original bytes for {}",
                    file.name
                )
            })?;
        }
        seen.push(file.name.as_str());
    }
    for file_name in AUTH_FILES {
        if !seen.contains(&file_name) {
            bail!("restore manifest missing managed auth file {file_name}");
        }
    }
    Ok(())
}

fn restore_owner_is_live(owner_pid: u32, owner_start_time: u64) -> bool {
    process_start_time(owner_pid) == Some(owner_start_time)
}

fn current_process_start_time() -> Option<u64> {
    process_start_time(std::process::id())
}

fn process_start_time(pid: u32) -> Option<u64> {
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(|process| process.start_time())
}

fn validate_restore_dir_name(name: &str, prefix: &str) -> Result<()> {
    if !name.starts_with(prefix)
        || name.len() == prefix.len()
        || name.contains('/')
        || name.contains('\\')
        || Path::new(name).is_absolute()
    {
        bail!("invalid restore artifact directory name {name:?}");
    }
    Ok(())
}

fn live_matches_manifest_target(
    codex_root: &Path,
    temp_dir: &Path,
    manifest: &RestoreManifest,
) -> Result<bool> {
    for file in &manifest.managed_files {
        let live_path = codex_root.join(&file.name);
        let live_bytes = match fs::read(&live_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", live_path.display()));
            }
        };
        let target_bytes = manifest_target_bytes(temp_dir, file)?;
        if live_bytes != target_bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

fn committed_manifest_should_restore_backup(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    manifest: &RestoreManifest,
) -> Result<bool> {
    if live_matches_manifest_target(codex_root, temp_dir, manifest)? {
        return Ok(false);
    }

    let mut saw_target_file = false;
    let mut saw_newer_file = false;
    let mut saw_transaction_file = false;
    for file in &manifest.managed_files {
        match committed_file_state(codex_root, backup_dir, temp_dir, file)? {
            CommittedFileState::Target => {
                saw_target_file = true;
                saw_transaction_file = true;
            }
            CommittedFileState::Backup => saw_transaction_file = true,
            CommittedFileState::Newer => saw_newer_file = true,
        }
    }
    if saw_transaction_file && saw_newer_file {
        bail!(
            "committed restore {} found newer live auth mixed with transaction auth; preserved restore artifacts",
            backup_dir.display()
        );
    }
    Ok(saw_target_file)
}

enum CommittedFileState {
    Target,
    Backup,
    Newer,
}

fn committed_file_state(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    file: &RestoreManifestFile,
) -> Result<CommittedFileState> {
    let live_path = codex_root.join(&file.name);
    let live_bytes = match fs::read(&live_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", live_path.display()));
        }
    };

    let target_bytes = manifest_target_bytes(temp_dir, file)?;
    if live_bytes
        .as_ref()
        .is_some_and(|bytes| bytes.as_slice() == target_bytes.as_slice())
    {
        return Ok(CommittedFileState::Target);
    }

    if !file.existed_before {
        return if live_bytes.is_none() {
            Ok(CommittedFileState::Backup)
        } else {
            Ok(CommittedFileState::Newer)
        };
    }

    let backup_path = backup_dir.join(&file.name);
    let backup_bytes = match fs::read(&backup_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", backup_path.display()));
        }
    };
    match (live_bytes, backup_bytes) {
        (Some(live_bytes), Some(backup_bytes)) if live_bytes == backup_bytes => {
            Ok(CommittedFileState::Backup)
        }
        (None, Some(_)) => Ok(CommittedFileState::Newer),
        _ => Ok(CommittedFileState::Newer),
    }
}

fn recover_in_progress_manifest(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    manifest: &RestoreManifest,
) -> Result<()> {
    let states = manifest
        .managed_files
        .iter()
        .map(|file| in_progress_file_state(codex_root, backup_dir, temp_dir, file))
        .collect::<Result<Vec<_>>>()?;

    let has_newer_live = states
        .iter()
        .any(|state| matches!(state, InProgressFileState::NewerLive));
    let has_unrecoverable_target = states
        .iter()
        .any(|state| matches!(state, InProgressFileState::TargetMissingBackup));
    if states
        .iter()
        .all(|state| matches!(state, InProgressFileState::Target))
        && manifest.snapshot_verified
        && (!manifest.stable_verification_required || manifest.stable_verified)
    {
        return Ok(());
    }

    if has_unrecoverable_target {
        bail!(
            "interrupted restore {} has target auth without a completed backup; preserved restore artifacts",
            backup_dir.display()
        );
    }
    if has_newer_live
        && states
            .iter()
            .any(|state| !matches!(state, InProgressFileState::NewerLive))
    {
        bail!(
            "interrupted restore {} found newer live auth mixed with transaction auth; preserved restore artifacts",
            backup_dir.display()
        );
    }

    for (file, state) in manifest.managed_files.iter().zip(&states) {
        match state {
            InProgressFileState::Target | InProgressFileState::MissingAfterBackup => {
                restore_manifest_file_from_backup(codex_root, backup_dir, file)?;
            }
            InProgressFileState::Backup
            | InProgressFileState::Original
            | InProgressFileState::OriginallyAbsent
            | InProgressFileState::NewerLive
            | InProgressFileState::TargetMissingBackup => {}
        }
    }
    Ok(())
}

enum InProgressFileState {
    Target,
    TargetMissingBackup,
    Backup,
    Original,
    MissingAfterBackup,
    OriginallyAbsent,
    NewerLive,
}

fn in_progress_file_state(
    codex_root: &Path,
    backup_dir: &Path,
    temp_dir: &Path,
    file: &RestoreManifestFile,
) -> Result<InProgressFileState> {
    let backup_path = backup_dir.join(&file.name);
    let backup_bytes = if backup_path.exists() {
        Some(
            fs::read(&backup_path)
                .with_context(|| format!("failed to read {}", backup_path.display()))?,
        )
    } else {
        None
    };
    let live_path = codex_root.join(&file.name);
    let live_bytes = match fs::read(&live_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", live_path.display()));
        }
    };

    let Some(live_bytes) = live_bytes else {
        if file.existed_before && backup_bytes.is_some() {
            return Ok(InProgressFileState::MissingAfterBackup);
        }
        return Ok(InProgressFileState::OriginallyAbsent);
    };

    let target_bytes = manifest_target_bytes(temp_dir, file)?;
    if live_bytes == target_bytes {
        if file.existed_before && backup_bytes.is_none() {
            if original_file_bytes(file)?.is_some_and(|original_bytes| live_bytes == original_bytes)
            {
                return Ok(InProgressFileState::Target);
            }
            return Ok(InProgressFileState::TargetMissingBackup);
        }
        return Ok(InProgressFileState::Target);
    }
    if backup_bytes
        .as_ref()
        .is_some_and(|backup_bytes| live_bytes == *backup_bytes)
    {
        return Ok(InProgressFileState::Backup);
    }
    if original_file_bytes(file)?.is_some_and(|original_bytes| live_bytes == original_bytes) {
        return Ok(InProgressFileState::Original);
    }
    if !file.existed_before {
        return Ok(InProgressFileState::NewerLive);
    }
    Ok(InProgressFileState::NewerLive)
}

fn restore_manifest_file_from_backup(
    codex_root: &Path,
    backup_dir: &Path,
    file: &RestoreManifestFile,
) -> Result<()> {
    let live_path = codex_root.join(&file.name);
    if !file.existed_before {
        if live_path.exists() {
            fs::remove_file(&live_path)
                .with_context(|| format!("failed to remove {}", live_path.display()))?;
        }
        return Ok(());
    }

    let backup_path = backup_dir.join(&file.name);
    match fs::copy(&backup_path, &live_path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let original_bytes = original_file_bytes(file)?.with_context(|| {
                format!(
                    "restore manifest missing original bytes for existing auth file {}",
                    file.name
                )
            })?;
            fs::write(&live_path, original_bytes)
                .with_context(|| format!("failed to restore {}", live_path.display()))
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to restore backup {} to {}",
                backup_path.display(),
                live_path.display()
            )
        }),
    }
}

fn restore_manifest_path(backup_dir: &Path) -> PathBuf {
    let committed_path = backup_dir.join(COMMITTED_RESTORE_MANIFEST_FILE);
    if committed_path.exists() {
        committed_path
    } else {
        backup_dir.join(RESTORE_MANIFEST_FILE)
    }
}

fn manifest_target_bytes(temp_dir: &Path, file: &RestoreManifestFile) -> Result<Vec<u8>> {
    let staged_path = temp_dir.join(&file.name);
    match fs::read(&staged_path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => STANDARD
            .decode(&file.target_bytes_base64)
            .with_context(|| format!("failed to decode restore target {}", file.name)),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read {}", staged_path.display()))
        }
    }
}

fn original_file_bytes(file: &RestoreManifestFile) -> Result<Option<Vec<u8>>> {
    file.original_bytes_base64
        .as_deref()
        .map(|bytes_base64| {
            STANDARD
                .decode(bytes_base64)
                .with_context(|| format!("failed to decode restore original {}", file.name))
        })
        .transpose()
}

fn restore_from_manifest_backup(
    codex_root: &Path,
    backup_dir: &Path,
    manifest: &RestoreManifest,
) -> Result<()> {
    for file in &manifest.managed_files {
        restore_manifest_file_from_backup(codex_root, backup_dir, file)?;
    }
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn retire_dir_if_exists(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return remove_dir_if_exists(path);
    };
    let cleanup_path = parent.join(format!("{CLEANUP_DIR_PREFIX}{}", Uuid::new_v4()));
    match fs::rename(path, &cleanup_path) {
        Ok(()) => remove_dir_if_exists(&cleanup_path),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to retire {} to {}",
                path.display(),
                cleanup_path.display()
            )
        }),
    }
}

fn dir_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .with_context(|| format!("path has no valid directory name: {}", path.display()))
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
            let pending_backup_path =
                backup_dir.join(format!("{file_name}.tmp-{}", Uuid::new_v4()));
            fs::copy(&live_path, &pending_backup_path).with_context(|| {
                format!(
                    "failed to back up {} to {}",
                    live_path.display(),
                    pending_backup_path.display()
                )
            })?;
            fs::rename(&pending_backup_path, &backup_path).with_context(|| {
                format!(
                    "failed to publish backup {} to {}",
                    pending_backup_path.display(),
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
    let live =
        read_live_auth_bundle_unchecked(env).context("failed to verify restored auth bundle")?;
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

    fn test_env(temp: &tempfile::TempDir) -> Result<AppEnv> {
        let codex_root = temp.path().join(".codex");
        fs::create_dir_all(&codex_root)?;
        Ok(AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root,
            app_data_dir: temp.path().join("data"),
        })
    }

    fn snapshot_file(name: &str, content: impl AsRef<[u8]>) -> SnapshotFile {
        SnapshotFile {
            name: name.to_owned(),
            bytes_base64: STANDARD.encode(content),
        }
    }

    fn snapshot(files: Vec<SnapshotFile>) -> SnapshotBlob {
        SnapshotBlob {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            files,
        }
    }

    fn identity(email: &str, subject: &str) -> DisplayIdentity {
        DisplayIdentity {
            email: email.to_owned(),
            subject: Some(subject.to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        }
    }

    fn live_env() -> Result<(tempfile::TempDir, AppEnv, String)> {
        let temp = tempdir()?;
        let env = test_env(&temp)?;
        let live_auth = auth_json_fixture("live@example.com", "live-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &live_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "live-sid")?;
        Ok((temp, env, live_auth))
    }

    fn assert_live_auth_unchanged(env: &AppEnv, live_auth: &str) -> Result<()> {
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "live-sid"
        );
        Ok(())
    }

    fn restore_manifest_for_test(
        env: &AppEnv,
        backup_dir: &Path,
        temp_dir: &Path,
        snapshot: &SnapshotBlob,
        state: RestoreManifestState,
    ) -> Result<RestoreManifest> {
        let mut manifest = restore_manifest(&env.codex_root, backup_dir, temp_dir, snapshot)?;
        manifest.state = state;
        manifest.owner_pid = u32::MAX;
        manifest.owner_start_time = 1;
        write_restore_manifest(backup_dir, &manifest)?;
        Ok(manifest)
    }

    fn write_snapshot_files(dir: &Path, snapshot: &SnapshotBlob) -> Result<()> {
        fs::create_dir_all(dir)?;
        for file in &snapshot.files {
            fs::write(dir.join(&file.name), STANDARD.decode(&file.bytes_base64)?)?;
        }
        Ok(())
    }

    fn codex_root_contains_prefixed_dir(env: &AppEnv, prefix: &str) -> Result<bool> {
        for entry in fs::read_dir(&env.codex_root)? {
            let path = entry?.path();
            if path.is_dir()
                && let Some(name) = path.file_name().and_then(|name| name.to_str())
                && name.starts_with(prefix)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

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
    fn restore_rejects_missing_cap_sid_before_touching_live_auth() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let snapshot = snapshot(vec![snapshot_file(
            "auth.json",
            auth_json_fixture("saved@example.com", "saved-sub", Some("pro")),
        )]);

        let error = restore_snapshot(
            &env,
            &snapshot,
            &identity("saved@example.com", "saved-sub"),
            false,
        )
        .expect_err("missing cap_sid should fail");

        assert!(format!("{error:#}").contains("snapshot missing managed auth file cap_sid"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        Ok(())
    }

    #[test]
    fn restore_rejects_duplicate_auth_json() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let snapshot = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("first@example.com", "first-sub", Some("pro")),
            ),
            snapshot_file(
                "auth.json",
                auth_json_fixture("second@example.com", "second-sub", Some("pro")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);

        let error = restore_snapshot(
            &env,
            &snapshot,
            &identity("first@example.com", "first-sub"),
            false,
        )
        .expect_err("duplicate auth.json should fail");

        assert!(
            format!("{error:#}")
                .contains("snapshot contains duplicate managed auth file auth.json")
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        Ok(())
    }

    #[test]
    fn restore_rejects_unexpected_snapshot_file() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let snapshot = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("pro")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
            snapshot_file("notes.txt", "extra"),
        ]);

        let error = restore_snapshot(
            &env,
            &snapshot,
            &identity("saved@example.com", "saved-sub"),
            false,
        )
        .expect_err("unexpected file should fail");

        assert!(
            format!("{error:#}")
                .contains("snapshot contains unexpected managed auth file notes.txt")
        );
        assert_live_auth_unchanged(&env, &live_auth)?;
        Ok(())
    }

    #[test]
    fn restore_rejects_invalid_managed_file_bytes_before_manifest() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let snapshot = SnapshotBlob {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            files: vec![
                snapshot_file(
                    "auth.json",
                    auth_json_fixture("saved@example.com", "saved-sub", Some("pro")),
                ),
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "not valid base64".to_owned(),
                },
            ],
        };

        let error = restore_snapshot(
            &env,
            &snapshot,
            &identity("saved@example.com", "saved-sub"),
            false,
        )
        .expect_err("invalid cap_sid bytes should fail");

        assert!(format!("{error:#}").contains("failed to decode snapshot file cap_sid"));
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!codex_root_contains_prefixed_dir(&env, BACKUP_DIR_PREFIX)?);
        assert!(!codex_root_contains_prefixed_dir(&env, PREPARE_DIR_PREFIX)?);
        assert!(!codex_root_contains_prefixed_dir(&env, RESTORE_DIR_PREFIX)?);
        Ok(())
    }

    #[test]
    fn restore_rejects_parent_path_before_staging_files() -> Result<()> {
        let (temp, env, live_auth) = live_env()?;
        let snapshot = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("pro")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
            snapshot_file("../outside.txt", "outside"),
        ]);

        let error = restore_snapshot(
            &env,
            &snapshot,
            &identity("saved@example.com", "saved-sub"),
            false,
        )
        .expect_err("parent path should fail");

        assert!(format!("{error:#}").contains("invalid managed auth file name"));
        assert!(!env.codex_root.join("outside.txt").exists());
        assert!(!temp.path().join("outside.txt").exists());
        assert_live_auth_unchanged(&env, &live_auth)?;
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
    fn interrupted_in_progress_restore_restores_backup_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-interrupted");
        let temp_dir = env.codex_root.join(".cas-restore-interrupted");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        fs::remove_file(env.codex_root.join("auth.json"))?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "live-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_in_progress_restore_preserves_newer_live_auth() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-newer-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-newer-in-progress");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        let newer_auth = auth_json_fixture("newer@example.com", "newer-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &newer_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "newer@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            newer_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_in_progress_restore_with_complete_target_cleans_artifacts_only() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-complete-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-complete-in-progress");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        let mut manifest = restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        manifest.snapshot_verified = true;
        replace_restore_manifest(&backup_dir, &manifest)?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "saved@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "saved-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_stable_restore_without_stable_verification_rolls_back_target() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-stable-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-stable-in-progress");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        let mut manifest = restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        manifest.snapshot_verified = true;
        manifest.stable_verification_required = true;
        replace_restore_manifest(&backup_dir, &manifest)?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn live_in_progress_restore_does_not_block_live_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-live-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-live-in-progress");
        fs::create_dir_all(&backup_dir)?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        let manifest = restore_manifest(&env.codex_root, &backup_dir, &temp_dir, &target)?;
        write_restore_manifest(&backup_dir, &manifest)?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(backup_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_in_progress_restore_preserves_artifacts_when_newer_live_is_mixed() -> Result<()>
    {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-mixed-newer-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-mixed-newer-in-progress");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let error =
            read_live_auth_bundle(&env).expect_err("mixed newer live auth should block cleanup");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(backup_dir.exists());
        assert!(temp_dir.exists());

        let error =
            read_live_auth_bundle(&env).expect_err("mixed newer live auth should remain blocked");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert!(backup_dir.exists());
        assert!(temp_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_in_progress_restore_rolls_back_only_switched_files() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-partial-in-progress");
        let temp_dir = env.codex_root.join(".cas-restore-partial-in-progress");
        fs::create_dir_all(&backup_dir)?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "live-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn interrupted_in_progress_restore_accepts_unchanged_target_without_backup() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env
            .codex_root
            .join(".cas-backup-unchanged-target-in-progress");
        let temp_dir = env
            .codex_root
            .join(".cas-restore-unchanged-target-in-progress");
        fs::create_dir_all(&backup_dir)?;

        let target = snapshot(vec![
            snapshot_file("auth.json", &live_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "live-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn restore_manifest_backup_falls_back_to_original_bytes() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-original-fallback");
        let temp_dir = env.codex_root.join(".cas-restore-original-fallback");

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        let manifest = restore_manifest(&env.codex_root, &backup_dir, &temp_dir, &target)?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        restore_from_manifest_backup(&env.codex_root, &backup_dir, &manifest)?;

        assert_live_auth_unchanged(&env, &live_auth)?;
        Ok(())
    }

    #[test]
    fn public_stable_verification_recovers_interrupted_restore_first() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let expected = read_live_auth_bundle(&env)?;
        let backup_dir = env.codex_root.join(".cas-backup-before-verify");
        let temp_dir = env.codex_root.join(".cas-restore-before-verify");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::InProgress,
        )?;
        fs::remove_file(env.codex_root.join("auth.json"))?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        verify_live_snapshot_stable(&env, &expected.snapshot, &expected.identity)?;

        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn abandoned_prepare_dir_is_removed_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let prepare_dir = env.codex_root.join(".cas-prepare-abandoned");
        let backup_dir = env.codex_root.join(".cas-backup-abandoned");
        let temp_dir = env.codex_root.join(".cas-restore-abandoned");
        fs::create_dir_all(&prepare_dir)?;
        fs::create_dir_all(&temp_dir)?;
        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        let mut manifest = restore_manifest(&env.codex_root, &backup_dir, &temp_dir, &target)?;
        manifest.owner_pid = u32::MAX;
        manifest.owner_start_time = 1;
        write_restore_manifest(&prepare_dir, &manifest)?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!prepare_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn active_prepare_dir_is_preserved_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let prepare_dir = env.codex_root.join(".cas-prepare-active");
        let backup_dir = env.codex_root.join(".cas-backup-active");
        let temp_dir = env.codex_root.join(".cas-restore-active");
        fs::create_dir_all(&prepare_dir)?;
        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        let manifest = restore_manifest(&env.codex_root, &backup_dir, &temp_dir, &target)?;
        write_restore_manifest(&prepare_dir, &manifest)?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(prepare_dir.exists());
        Ok(())
    }

    #[test]
    fn manifestless_abandoned_prepare_dir_is_removed_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let prepare_dir = env.codex_root.join(".cas-prepare-manifestless-abandoned");
        fs::create_dir_all(&prepare_dir)?;
        let owner = RestoreOwner {
            owner_pid: u32::MAX,
            owner_start_time: 1,
        };
        fs::write(
            prepare_dir.join(PREPARE_OWNER_FILE),
            serde_json::to_vec(&owner)?,
        )?;
        fs::write(
            prepare_dir.join(format!("{RESTORE_MANIFEST_FILE}.pending")),
            "pending target auth bytes",
        )?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!prepare_dir.exists());
        Ok(())
    }

    #[test]
    fn retired_cleanup_dir_is_removed_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let cleanup_dir = env.codex_root.join(".cas-cleanup-abandoned");
        fs::create_dir_all(&cleanup_dir)?;
        fs::write(cleanup_dir.join(RESTORE_MANIFEST_FILE), "stale cleanup")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!cleanup_dir.exists());
        Ok(())
    }

    #[test]
    fn manifestless_active_prepare_dir_is_preserved_on_next_read() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let prepare_dir = env.codex_root.join(".cas-prepare-manifestless-active");
        fs::create_dir_all(&prepare_dir)?;
        let owner = RestoreOwner {
            owner_pid: std::process::id(),
            owner_start_time: current_process_start_time()
                .context("failed to read current process start time")?,
        };
        fs::write(
            prepare_dir.join(PREPARE_OWNER_FILE),
            serde_json::to_vec(&owner)?,
        )?;
        fs::write(
            prepare_dir.join(format!("{RESTORE_MANIFEST_FILE}.pending")),
            "pending target auth bytes",
        )?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(prepare_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_with_matching_live_files_cleans_artifacts_only() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-committed");
        let temp_dir = env.codex_root.join(".cas-restore-committed");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "saved-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "saved@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "saved-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_with_mixed_live_files_restores_backup() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-mixed");
        let temp_dir = env.codex_root.join(".cas-restore-mixed");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "live-sid")?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "live@example.com");
        assert_live_auth_unchanged(&env, &live_auth)?;
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_newer_live_files_when_temp_dir_remains() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-newer-live");
        let temp_dir = env.codex_root.join(".cas-restore-newer-live");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        let newer_auth = auth_json_fixture("newer@example.com", "newer-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &newer_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "newer@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            newer_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_artifacts_when_newer_live_is_mixed() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-committed-mixed-newer");
        let temp_dir = env.codex_root.join(".cas-restore-committed-mixed-newer");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let error =
            read_live_auth_bundle(&env).expect_err("mixed committed auth should block cleanup");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(backup_dir.exists());
        assert!(temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_artifacts_when_newer_live_is_mixed_with_backup() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env
            .codex_root
            .join(".cas-backup-committed-mixed-newer-backup");
        let temp_dir = env
            .codex_root
            .join(".cas-restore-committed-mixed-newer-backup");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        let newer_auth = auth_json_fixture("newer@example.com", "newer-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &newer_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "live-sid")?;

        let error =
            read_live_auth_bundle(&env).expect_err("backup plus newer auth should block cleanup");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            newer_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "live-sid"
        );
        assert!(backup_dir.exists());
        assert!(temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_artifacts_when_missing_temp_has_mixed_live() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env
            .codex_root
            .join(".cas-backup-committed-mixed-missing-temp");
        let temp_dir = env
            .codex_root
            .join(".cas-restore-committed-mixed-missing-temp");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let error = read_live_auth_bundle(&env)
            .expect_err("mixed committed auth should block cleanup without temp files");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_artifacts_when_live_file_is_missing() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-committed-missing-file");
        let temp_dir = env.codex_root.join(".cas-restore-committed-missing-file");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target_auth = auth_json_fixture("saved@example.com", "saved-sub", Some("plus"));
        let target = snapshot(vec![
            snapshot_file("auth.json", &target_auth),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        fs::write(env.codex_root.join("auth.json"), &target_auth)?;
        fs::remove_file(env.codex_root.join("cap_sid"))?;

        let error =
            read_live_auth_bundle(&env).expect_err("missing committed auth should block cleanup");

        assert!(format!("{error:#}").contains("newer live auth mixed with transaction auth"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            target_auth
        );
        assert!(!env.codex_root.join("cap_sid").exists());
        assert!(backup_dir.exists());
        assert!(temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_preserves_missing_live_auth_when_temp_dir_remains() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-missing-live");
        let temp_dir = env.codex_root.join(".cas-restore-missing-live");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        write_snapshot_files(&temp_dir, &target)?;
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        fs::remove_file(env.codex_root.join("auth.json"))?;
        fs::remove_file(env.codex_root.join("cap_sid"))?;

        let recovered = try_read_live_auth_bundle(&env)?;

        assert!(recovered.is_none());
        assert!(!env.codex_root.join("auth.json").exists());
        assert!(!env.codex_root.join("cap_sid").exists());
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn committed_restore_with_missing_temp_dir_cleans_stale_backup_only() -> Result<()> {
        let (_temp, env, live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-stale-cleanup");
        let temp_dir = env.codex_root.join(".cas-restore-stale-cleanup");
        fs::create_dir_all(&backup_dir)?;
        fs::write(backup_dir.join("auth.json"), &live_auth)?;
        fs::write(backup_dir.join("cap_sid"), "live-sid")?;

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        restore_manifest_for_test(
            &env,
            &backup_dir,
            &temp_dir,
            &target,
            RestoreManifestState::Committed,
        )?;
        let newer_auth = auth_json_fixture("newer@example.com", "newer-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &newer_auth)?;
        fs::write(env.codex_root.join("cap_sid"), "newer-sid")?;

        let recovered = read_live_auth_bundle(&env)?;

        assert_eq!(recovered.identity.email, "newer@example.com");
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            newer_auth
        );
        assert_eq!(
            fs::read_to_string(env.codex_root.join("cap_sid"))?,
            "newer-sid"
        );
        assert!(!backup_dir.exists());
        assert!(!temp_dir.exists());
        Ok(())
    }

    #[test]
    fn backup_dir_without_manifest_returns_actionable_error() -> Result<()> {
        let (_temp, env, _live_auth) = live_env()?;
        let backup_dir = env.codex_root.join(".cas-backup-legacy");
        fs::create_dir_all(&backup_dir)?;

        let error = try_read_live_auth_bundle(&env)
            .expect_err("manifestless backup should block live auth access");

        let message = format!("{error:#}");
        assert!(message.contains("has no restore manifest"));
        assert!(message.contains(".cas-backup-legacy"));
        assert!(backup_dir.exists());
        Ok(())
    }

    #[test]
    fn restore_rollback_removes_files_absent_before_activation() -> Result<()> {
        let temp = tempdir()?;
        let env = test_env(&temp)?;
        let live_auth = auth_json_fixture("live@example.com", "live-sub", Some("pro"));
        fs::write(env.codex_root.join("auth.json"), &live_auth)?;
        assert!(!env.codex_root.join("cap_sid").exists());

        let target = snapshot(vec![
            snapshot_file(
                "auth.json",
                auth_json_fixture("saved@example.com", "saved-sub", Some("plus")),
            ),
            snapshot_file("cap_sid", "saved-sid"),
        ]);
        let error = restore_snapshot(
            &env,
            &target,
            &identity("different@example.com", "different-sub"),
            false,
        )
        .expect_err("identity mismatch should roll back");

        assert!(format!("{error:#}").contains("restore verification failed"));
        assert_eq!(
            fs::read_to_string(env.codex_root.join("auth.json"))?,
            live_auth
        );
        assert!(!env.codex_root.join("cap_sid").exists());
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
