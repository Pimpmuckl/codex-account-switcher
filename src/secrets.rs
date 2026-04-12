use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::file_store::{RecoveryFileKind, list_recovery_files, replace_file_with_recovery};
use crate::model::SnapshotBlob;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flate2::read::GzDecoder;

const SNAPSHOT_ENCODING_V1_MAGIC: &[u8] = b"cas-snapshot-v1\n";

pub trait SecretStore {
    fn save(&self, key: &str, value: &[u8]) -> Result<()>;
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn delete(&self, key: &str) -> Result<()>;
}

trait LegacySnapshotStore {
    fn load_legacy(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn delete_legacy(&self, key: &str) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct LocalSecretStore {
    root_dir: PathBuf,
}

impl LocalSecretStore {
    pub fn new(root_dir: &Path) -> Self {
        Self {
            root_dir: root_dir.to_path_buf(),
        }
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.root_dir.join(format!(
            "{}.snapshot",
            URL_SAFE_NO_PAD.encode(key.as_bytes())
        ))
    }

    fn recovery_paths(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut candidates = list_recovery_files(path, None)?
            .into_iter()
            .filter(|entry| {
                matches!(
                    entry.kind,
                    RecoveryFileKind::Temp | RecoveryFileKind::Backup
                )
            })
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| {
            fs::metadata(candidate)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });
        candidates.reverse();
        Ok(candidates)
    }

    fn save_local(&self, key: &str, value: &[u8]) -> Result<()> {
        ensure_store_dir(&self.root_dir)?;
        let path = self.path_for_key(key);
        replace_file_with_recovery(&path, None, |temp_path| {
            write_private_file(temp_path, value)
        })
    }

    fn load_local(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path_for_key(key);
        let mut saw_invalid = false;
        let mut canonical_error = None;
        let canonical_value = match fs::read(&path) {
            Ok(value) if snapshot_payload_is_valid(&value) => Some(value),
            Ok(_) => {
                saw_invalid = true;
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                canonical_error = Some(
                    Err(error)
                        .with_context(|| format!("failed to read snapshot {}", path.display())),
                );
                None
            }
        };
        let canonical_mtime = canonical_value.as_ref().and_then(|_| file_mtime(&path));

        let candidates = self.recovery_paths(&path)?;
        for recovery_path in candidates {
            let Some(recovery_mtime) = file_mtime(&recovery_path) else {
                continue;
            };
            if let Some(canonical_mtime) = canonical_mtime
                && recovery_mtime <= canonical_mtime
            {
                continue;
            }
            let value = match fs::read(&recovery_path) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if !snapshot_payload_is_valid(&value) {
                saw_invalid = true;
                continue;
            }
            let _ = self.save_local(key, &value);
            return Ok(Some(value));
        }

        if let Some(value) = canonical_value {
            return Ok(Some(value));
        }
        if saw_invalid {
            return Err(anyhow!(
                "failed to recover saved snapshot data from interrupted writes"
            ));
        }
        canonical_error.unwrap_or(Ok(None))
    }

    fn delete_local(&self, key: &str) -> Result<()> {
        let path = self.path_for_key(key);
        let mut targets = vec![path];
        targets.extend(self.recovery_paths(&targets[0])?);
        let mut first_error = None;
        for target in targets {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(Err(error).with_context(|| {
                            format!("failed to delete snapshot {}", target.display())
                        }));
                    }
                }
            }
        }
        first_error.unwrap_or(Ok(()))
    }
}

impl SecretStore for LocalSecretStore {
    fn save(&self, key: &str, value: &[u8]) -> Result<()> {
        self.save_local(key, value)
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.load_local(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.delete_local(key)
    }
}

#[derive(Clone, Debug)]
pub struct MigratingSecretStore<L> {
    local: LocalSecretStore,
    legacy: L,
}

impl<L> MigratingSecretStore<L> {
    #[cfg(test)]
    pub fn with_legacy(root_dir: &Path, legacy: L) -> Self {
        Self {
            local: LocalSecretStore::new(root_dir),
            legacy,
        }
    }
}

impl MigratingSecretStore<DefaultLegacyStore> {
    pub fn new(root_dir: &Path) -> Self {
        Self {
            local: LocalSecretStore::new(root_dir),
            legacy: DefaultLegacyStore::default(),
        }
    }
}

impl<L> SecretStore for MigratingSecretStore<L>
where
    L: LegacySnapshotStore,
{
    fn save(&self, key: &str, value: &[u8]) -> Result<()> {
        self.local.save(key, value)?;
        let _ = self.legacy.delete_legacy(key);
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let local_result = self.local.load(key);
        match local_result {
            Ok(Some(value)) => return Ok(Some(value)),
            Ok(None) => {}
            Err(local_error) => {
                if let Some(legacy_value) = self.legacy.load_legacy(key)? {
                    if self.local.save(key, &legacy_value).is_ok() {
                        let _ = self.legacy.delete_legacy(key);
                    }
                    return Ok(Some(legacy_value));
                }
                return Err(local_error);
            }
        }

        let Some(legacy_value) = self.legacy.load_legacy(key)? else {
            return Ok(None);
        };
        if self.local.save(key, &legacy_value).is_ok() {
            let _ = self.legacy.delete_legacy(key);
        }
        Ok(Some(legacy_value))
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.legacy.delete_legacy(key)?;
        self.local.delete(key)
    }
}

fn ensure_store_dir(root_dir: &Path) -> Result<()> {
    if root_dir.exists() {
        set_private_dir_permissions(root_dir)?;
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::fs::DirBuilder;
        use std::os::unix::fs::DirBuilderExt;

        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root_dir)
            .with_context(|| format!("failed to create {}", root_dir.display()))?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(root_dir)
            .with_context(|| format!("failed to create {}", root_dir.display()))?;
    }

    set_private_dir_permissions(root_dir)?;
    Ok(())
}

fn write_private_file(path: &Path, value: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(value)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::write(path, value).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

fn snapshot_payload_is_valid(value: &[u8]) -> bool {
    if let Some(compressed) = value.strip_prefix(SNAPSHOT_ENCODING_V1_MAGIC) {
        let mut decoder = GzDecoder::new(compressed);
        let mut decoded = Vec::new();
        if std::io::Read::read_to_end(&mut decoder, &mut decoded).is_err() {
            return false;
        }
        return serde_json::from_slice::<SnapshotBlob>(&decoded).is_ok();
    }

    serde_json::from_slice::<SnapshotBlob>(value).is_ok()
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
type DefaultLegacyStore = KeyringLegacyStore;
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
type DefaultLegacyStore = NoopLegacyStore;

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
#[derive(Clone, Debug)]
pub struct KeyringLegacyStore {
    service_name: &'static str,
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
impl Default for KeyringLegacyStore {
    fn default() -> Self {
        Self {
            service_name: "codex-account-switcher",
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
impl LegacySnapshotStore for KeyringLegacyStore {
    fn load_legacy(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let entry = keyring::Entry::new(self.service_name, key)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value.into_bytes())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to load legacy snapshot from keychain"),
        }
    }

    fn delete_legacy(&self, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(self.service_name, key)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("failed to delete legacy snapshot from keychain"),
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[derive(Clone, Debug, Default)]
pub struct NoopLegacyStore;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
impl LegacySnapshotStore for NoopLegacyStore {
    fn load_legacy(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn delete_legacy(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub mod test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::SecretStore;

    #[derive(Clone, Default)]
    pub struct MemorySecretStore {
        inner: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl SecretStore for MemorySecretStore {
        fn save(&self, key: &str, value: &[u8]) -> Result<()> {
            self.inner
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value.to_vec());
            Ok(())
        }

        fn load(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.inner.lock().expect("lock").get(key).cloned())
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.lock().expect("lock").remove(key);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{LegacySnapshotStore, LocalSecretStore, MigratingSecretStore, SecretStore};
    use crate::model::{SnapshotBlob, SnapshotFile};

    #[derive(Clone, Default)]
    struct FakeLegacyStore {
        inner: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl FakeLegacyStore {
        fn insert(&self, key: &str, value: &[u8]) {
            self.inner
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value.to_vec());
        }
    }

    impl LegacySnapshotStore for FakeLegacyStore {
        fn load_legacy(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.inner.lock().expect("lock").get(key).cloned())
        }

        fn delete_legacy(&self, key: &str) -> Result<()> {
            self.inner.lock().expect("lock").remove(key);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FailingDeleteLegacyStore {
        inner: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl FailingDeleteLegacyStore {
        fn insert(&self, key: &str, value: &[u8]) {
            self.inner
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value.to_vec());
        }
    }

    impl LegacySnapshotStore for FailingDeleteLegacyStore {
        fn load_legacy(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.inner.lock().expect("lock").get(key).cloned())
        }

        fn delete_legacy(&self, _key: &str) -> Result<()> {
            Err(anyhow::anyhow!("legacy delete failed"))
        }
    }

    fn valid_snapshot_bytes() -> Vec<u8> {
        serde_json::to_vec(&SnapshotBlob {
            schema_version: 1,
            files: vec![
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-payload".to_owned(),
                },
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "cap-payload".to_owned(),
                },
            ],
        })
        .expect("snapshot")
    }

    fn updated_snapshot_bytes() -> Vec<u8> {
        serde_json::to_vec(&SnapshotBlob {
            schema_version: 1,
            files: vec![
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-updated".to_owned(),
                },
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "cap-updated".to_owned(),
                },
            ],
        })
        .expect("snapshot")
    }

    #[test]
    fn local_store_round_trips_across_instances() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
    }

    #[test]
    fn local_store_recovers_from_backup_candidate() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        fs::rename(&canonical, &backup).expect("move backup");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
        assert!(canonical.exists());
    }

    #[test]
    fn local_store_recovers_from_temp_candidate() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let pending = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.tmp-new");
        fs::rename(&canonical, &pending).expect("move temp");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
        assert!(canonical.exists());
    }

    #[test]
    fn local_store_prefers_newer_temp_over_stale_canonical() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        let updated = updated_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.tmp-new"),
            &updated,
        )
        .expect("write temp");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, updated);
    }

    #[test]
    fn local_store_skips_invalid_newer_temp_and_uses_valid_backup() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        fs::rename(&canonical, &backup).expect("move backup");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.tmp-new"),
            b"{not-json",
        )
        .expect("write corrupt temp");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
        assert!(canonical.exists());
    }

    #[test]
    fn local_store_skips_unreadable_newer_temp_and_uses_valid_backup() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        fs::rename(&canonical, &backup).expect("move backup");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::create_dir(temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.tmp-new"))
            .expect("create unreadable temp directory");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
        assert!(canonical.exists());
    }

    #[test]
    fn local_store_skips_invalid_canonical_and_uses_valid_backup() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        fs::copy(&canonical, &backup).expect("copy backup");
        fs::write(&canonical, b"{not-json").expect("corrupt canonical");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
    }

    #[test]
    fn local_store_recovers_backup_when_canonical_is_unreadable() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let canonical = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        fs::copy(&canonical, &backup).expect("copy backup");
        fs::remove_file(&canonical).expect("remove canonical");
        fs::create_dir(&canonical).expect("create unreadable canonical directory");

        let reopened = LocalSecretStore::new(temp.path());
        let loaded = reopened
            .load("snapshot:test")
            .expect("load")
            .expect("payload");
        assert_eq!(loaded, payload);
    }

    #[test]
    fn local_store_delete_removes_recovery_artifacts() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let backup = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-old");
        let pending = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.tmp-new");
        fs::write(&backup, &payload).expect("write backup");
        fs::write(&pending, &payload).expect("write temp");

        store.delete("snapshot:test").expect("delete");
        assert!(!temp.path().join("c25hcHNob3Q6dGVzdA.snapshot").exists());
        assert!(!backup.exists());
        assert!(!pending.exists());
    }

    #[test]
    fn local_store_delete_surfaces_auxiliary_cleanup_failures() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        fs::create_dir(temp.path().join("c25hcHNob3Q6dGVzdA.snapshot.bak-stale"))
            .expect("create stale backup directory");
        let error = store
            .delete("snapshot:test")
            .expect_err("delete should fail");
        assert!(format!("{error:#}").contains("failed to delete snapshot"));

        assert!(!temp.path().join("c25hcHNob3Q6dGVzdA.snapshot").exists());
    }

    #[test]
    fn migrating_store_moves_legacy_payload_into_local_storage() {
        let temp = tempdir().expect("tempdir");
        let legacy = FakeLegacyStore::default();
        let payload = valid_snapshot_bytes();
        legacy.insert("snapshot:test", &payload);
        let store = MigratingSecretStore::with_legacy(temp.path(), legacy.clone());

        let loaded = store.load("snapshot:test").expect("load").expect("payload");
        assert_eq!(loaded, payload);
        assert!(
            temp.path().join("c25hcHNob3Q6dGVzdA.snapshot").exists(),
            "payload should be migrated into local storage"
        );
        assert!(
            legacy
                .load_legacy("snapshot:test")
                .expect("legacy load")
                .is_none(),
            "legacy payload should be cleaned up after migration"
        );
    }

    #[test]
    fn migrating_store_save_cleans_up_legacy_copy() {
        let temp = tempdir().expect("tempdir");
        let legacy = FakeLegacyStore::default();
        let payload = valid_snapshot_bytes();
        legacy.insert("snapshot:test", &payload);
        let store = MigratingSecretStore::with_legacy(temp.path(), legacy.clone());

        store.save("snapshot:test", &payload).expect("save");
        assert!(
            legacy
                .load_legacy("snapshot:test")
                .expect("legacy load")
                .is_none(),
            "save should clear the stale legacy copy"
        );
    }

    #[test]
    fn migrating_store_falls_back_to_legacy_when_local_load_errors() {
        let temp = tempdir().expect("tempdir");
        fs::create_dir(temp.path().join("c25hcHNob3Q6dGVzdA.snapshot"))
            .expect("create unreadable canonical directory");
        let legacy = FakeLegacyStore::default();
        let payload = valid_snapshot_bytes();
        legacy.insert("snapshot:test", &payload);
        let store = MigratingSecretStore::with_legacy(temp.path(), legacy);

        let loaded = store.load("snapshot:test").expect("load").expect("payload");
        assert_eq!(loaded, payload);
    }

    #[test]
    fn migrating_store_delete_surfaces_legacy_cleanup_failures() {
        let temp = tempdir().expect("tempdir");
        let legacy = FailingDeleteLegacyStore::default();
        let payload = valid_snapshot_bytes();
        legacy.insert("snapshot:test", &payload);
        let store = MigratingSecretStore::with_legacy(temp.path(), legacy);
        store.save("snapshot:test", &payload).expect("save");

        let error = store
            .delete("snapshot:test")
            .expect_err("delete should fail");
        assert!(format!("{error:#}").contains("legacy delete failed"));
        assert!(temp.path().join("c25hcHNob3Q6dGVzdA.snapshot").exists());
    }

    #[cfg(unix)]
    #[test]
    fn local_store_uses_private_unix_permissions() {
        let temp = tempdir().expect("tempdir");
        let store = LocalSecretStore::new(temp.path());
        let payload = valid_snapshot_bytes();
        store.save("snapshot:test", &payload).expect("save");

        let dir_mode = fs::metadata(temp.path())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        let file_path = temp.path().join("c25hcHNob3Q6dGVzdA.snapshot");
        let file_mode = fs::metadata(file_path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
