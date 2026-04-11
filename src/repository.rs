use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::model::{
    DisplayIdentity, EnvironmentKind, METADATA_SCHEMA_VERSION, MetadataIndex, SavedAccountMetadata,
    SnapshotBlob,
};
use crate::secrets::SecretStore;

pub struct SnapshotRepository<S> {
    metadata_path: PathBuf,
    secret_store: S,
}

impl<S> SnapshotRepository<S>
where
    S: SecretStore,
{
    pub fn new(data_dir: &Path, secret_store: S) -> Self {
        Self {
            metadata_path: data_dir.join("metadata.json"),
            secret_store,
        }
    }

    pub fn list_accounts(
        &self,
        environment: &EnvironmentKind,
    ) -> Result<Vec<SavedAccountMetadata>> {
        let mut accounts = self
            .load_index()?
            .accounts
            .into_iter()
            .filter(|account| &account.environment == environment)
            .collect::<Vec<_>>();
        accounts.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(accounts)
    }

    pub fn get_account(
        &self,
        environment: &EnvironmentKind,
        account_id: Uuid,
    ) -> Result<Option<SavedAccountMetadata>> {
        Ok(self
            .list_accounts(environment)?
            .into_iter()
            .find(|account| account.id == account_id))
    }

    pub fn save_snapshot(
        &self,
        environment: &EnvironmentKind,
        identity: &DisplayIdentity,
        snapshot: &SnapshotBlob,
    ) -> Result<(SavedAccountMetadata, bool)> {
        let mut index = self.load_index()?;
        let now = OffsetDateTime::now_utc();
        let serialized_snapshot =
            serde_json::to_string(snapshot).context("failed to serialize snapshot")?;
        let existing_index = index.accounts.iter().position(|account| {
            &account.environment == environment
                && DisplayIdentity {
                    email: account.email.clone(),
                    subject: account.subject.clone(),
                    name: account.name.clone(),
                    plan_label: account.plan_label.clone(),
                }
                .matches(identity)
        });

        let (metadata, created) = if let Some(position) = existing_index {
            let account = &mut index.accounts[position];
            account.email = identity.email.clone();
            account.subject = identity.subject.clone();
            account.name = identity.name.clone();
            account.plan_label = identity.plan_label.clone();
            account.updated_at = now;
            (account.clone(), false)
        } else {
            let id = Uuid::new_v4();
            let metadata = SavedAccountMetadata {
                id,
                environment: environment.clone(),
                email: identity.email.clone(),
                subject: identity.subject.clone(),
                name: identity.name.clone(),
                plan_label: identity.plan_label.clone(),
                secret_key: format!("snapshot:{id}"),
                created_at: now,
                updated_at: now,
                last_activated_at: None,
            };
            index.accounts.push(metadata.clone());
            (metadata, true)
        };

        self.secret_store
            .save(&metadata.secret_key, &serialized_snapshot)?;
        self.save_index(&index)?;
        Ok((metadata, created))
    }

    pub fn load_snapshot(
        &self,
        environment: &EnvironmentKind,
        account_id: Uuid,
    ) -> Result<(SavedAccountMetadata, SnapshotBlob)> {
        let metadata = self
            .get_account(environment, account_id)?
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let serialized_snapshot = self
            .secret_store
            .load(&metadata.secret_key)?
            .ok_or_else(|| anyhow!("snapshot secret missing for {}", metadata.email))?;
        let snapshot: SnapshotBlob = serde_json::from_str(&serialized_snapshot)
            .context("failed to parse stored snapshot")?;
        Ok((metadata, snapshot))
    }

    pub fn delete_snapshot(&self, environment: &EnvironmentKind, account_id: Uuid) -> Result<()> {
        let mut index = self.load_index()?;
        let Some(position) = index
            .accounts
            .iter()
            .position(|account| account.id == account_id && &account.environment == environment)
        else {
            return Err(anyhow!("saved account {account_id} not found"));
        };
        let metadata = index.accounts.remove(position);
        let deleted_secret = self.secret_store.load(&metadata.secret_key).ok().flatten();
        if let Err(error) = self.secret_store.delete(&metadata.secret_key) {
            return Err(error).context("failed to delete snapshot secret");
        }
        if let Err(error) = self.save_index(&index) {
            if let Some(serialized_snapshot) = deleted_secret.as_deref()
                && let Err(restore_error) = self
                    .secret_store
                    .save(&metadata.secret_key, serialized_snapshot)
            {
                return Err(anyhow!(
                    "failed to persist deleted metadata and failed to restore snapshot secret: {error:#}; restore error: {restore_error:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn sync_activated_account(
        &self,
        environment: &EnvironmentKind,
        account_id: Uuid,
        identity: &DisplayIdentity,
    ) -> Result<SavedAccountMetadata> {
        let mut index = self.load_index()?;
        let now = OffsetDateTime::now_utc();
        let Some(account_position) = index
            .accounts
            .iter()
            .position(|account| account.id == account_id && &account.environment == environment)
        else {
            return Err(anyhow!("saved account {account_id} not found"));
        };
        let duplicate_positions = index
            .accounts
            .iter()
            .enumerate()
            .filter(|(position, account)| {
                *position != account_position
                    && &account.environment == environment
                    && DisplayIdentity {
                        email: account.email.clone(),
                        subject: account.subject.clone(),
                        name: account.name.clone(),
                        plan_label: account.plan_label.clone(),
                    }
                    .matches(identity)
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        let duplicates = duplicate_positions
            .into_iter()
            .rev()
            .map(|position| index.accounts.remove(position))
            .collect::<Vec<_>>();
        let adjusted_position = index
            .accounts
            .iter()
            .position(|account| account.id == account_id && &account.environment == environment)
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let account = &mut index.accounts[adjusted_position];
        account.email = identity.email.clone();
        account.subject = identity.subject.clone();
        account.name = identity.name.clone();
        account.plan_label = identity.plan_label.clone();
        account.last_activated_at = Some(now);
        account.updated_at = now;
        let updated = account.clone();
        self.save_index(&index)?;
        for duplicate in duplicates {
            let _ = self.secret_store.delete(&duplicate.secret_key);
        }
        Ok(updated)
    }

    fn load_index(&self) -> Result<MetadataIndex> {
        match self.best_available_index()? {
            Some(index) => Ok(index),
            None => Ok(MetadataIndex {
                schema_version: METADATA_SCHEMA_VERSION,
                write_generation: 0,
                accounts: Vec::new(),
            }),
        }
    }

    fn save_index(&self, index: &MetadataIndex) -> Result<()> {
        if let Some(parent) = self.metadata_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let pending_path = self.metadata_path.with_extension("json.pending");
        let mut persisted = index.clone();
        persisted.write_generation = index.write_generation.saturating_add(1);
        let json =
            serde_json::to_string_pretty(&persisted).context("failed to serialize metadata")?;
        let temp_path = self
            .metadata_path
            .with_extension(format!("json.tmp-{}", Uuid::new_v4().simple()));
        let backup_path = self
            .metadata_path
            .with_extension(format!("json.bak-{}", Uuid::new_v4().simple()));
        fs::write(&pending_path, &json)
            .with_context(|| format!("failed to write {}", pending_path.display()))?;
        if let Err(error) = fs::write(&temp_path, &json) {
            let _ = self.cleanup_recovery_paths(&pending_path, &temp_path, None);
            return Err(error).with_context(|| format!("failed to write {}", temp_path.display()));
        }
        if !self.metadata_path.exists() {
            if let Err(error) = fs::rename(&temp_path, &self.metadata_path) {
                let _ = self.cleanup_recovery_paths(&pending_path, &temp_path, None);
                return Err(error).with_context(|| {
                    format!(
                        "failed to replace {} with {}",
                        self.metadata_path.display(),
                        temp_path.display()
                    )
                });
            }
            let _ = self.cleanup_recovery_paths(&pending_path, &temp_path, None);
            return Ok(());
        }

        if let Err(error) = fs::rename(&self.metadata_path, &backup_path) {
            let _ = self.cleanup_recovery_paths(&pending_path, &temp_path, None);
            return Err(error).with_context(|| {
                format!(
                    "failed to rotate {} to {}",
                    self.metadata_path.display(),
                    backup_path.display()
                )
            });
        }

        match fs::rename(&temp_path, &self.metadata_path) {
            Ok(()) => {
                let _ = self.cleanup_recovery_paths(&pending_path, &temp_path, Some(&backup_path));
                Ok(())
            }
            Err(error) => {
                let rollback_succeeded = fs::rename(&backup_path, &self.metadata_path).is_ok();
                if rollback_succeeded {
                    let _ =
                        self.cleanup_recovery_paths(&pending_path, &temp_path, Some(&backup_path));
                }
                Err(error).with_context(|| {
                    format!(
                        "failed to replace {} with {}",
                        self.metadata_path.display(),
                        temp_path.display()
                    )
                })
            }
        }
    }

    fn best_available_index(&self) -> Result<Option<MetadataIndex>> {
        let Some(parent) = self.metadata_path.parent() else {
            return Ok(None);
        };
        if !parent.exists() {
            return Ok(None);
        }
        let Some(file_name) = self
            .metadata_path
            .file_name()
            .and_then(|value| value.to_str())
        else {
            return Ok(None);
        };
        let canonical_name = file_name.to_owned();
        let temp_prefix = format!("{file_name}.tmp");
        let backup_prefix = format!("{file_name}.bak");
        let mut saw_candidate = false;
        let mut entries = fs::read_dir(parent)
            .with_context(|| format!("failed to read {}", parent.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|name| {
                    name == canonical_name
                        || name.starts_with(&backup_prefix)
                        || name.starts_with(&temp_prefix)
                })
            })
            .filter_map(|entry| {
                saw_candidate = true;
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_str()?.to_owned();
                let raw = fs::read_to_string(&path).ok()?;
                let mut index: MetadataIndex = serde_json::from_str(&raw).ok()?;
                if index.schema_version == 0 {
                    index.schema_version = METADATA_SCHEMA_VERSION;
                }
                if index.schema_version != METADATA_SCHEMA_VERSION {
                    return None;
                }
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let kind = if name == canonical_name {
                    RecoveryKind::Canonical
                } else if name.starts_with(&backup_prefix) {
                    RecoveryKind::Backup
                } else if name.starts_with(&temp_prefix) {
                    RecoveryKind::Temp
                } else {
                    return None;
                };
                Some(RecoveryCandidate {
                    kind,
                    write_generation: index.write_generation,
                    modified,
                    index,
                })
            })
            .collect::<Vec<_>>();
        if let Some(pending) =
            parse_recovery_candidate(&self.metadata_path.with_extension("json.pending"))
        {
            saw_candidate = true;
            entries.push(pending);
        }
        if saw_candidate && entries.is_empty() {
            return Err(anyhow!(
                "failed to parse metadata recovery state under {}",
                parent.display()
            ));
        }
        if let Some(canonical) = entries
            .iter()
            .find(|entry| matches!(entry.kind, RecoveryKind::Canonical))
        {
            if let Some(pending) = entries.iter().find(|entry| {
                matches!(entry.kind, RecoveryKind::Pending)
                    && entry.write_generation > canonical.write_generation
            }) {
                return Ok(Some(pending.index.clone()));
            }
            return Ok(Some(canonical.index.clone()));
        }
        entries.sort_by(|left, right| {
            right
                .write_generation
                .cmp(&left.write_generation)
                .then_with(|| right.modified.cmp(&left.modified))
                .then_with(|| recovery_priority(right.kind).cmp(&recovery_priority(left.kind)))
        });
        Ok(entries.first().map(|entry| entry.index.clone()))
    }

    fn cleanup_recovery_paths(
        &self,
        pending_path: &Path,
        temp_path: &Path,
        backup_path: Option<&Path>,
    ) -> Result<()> {
        let _ = fs::remove_file(pending_path);
        let _ = fs::remove_file(temp_path);
        if let Some(backup_path) = backup_path {
            let _ = fs::remove_file(backup_path);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum RecoveryKind {
    Canonical,
    Pending,
    Temp,
    Backup,
}

struct RecoveryCandidate {
    kind: RecoveryKind,
    write_generation: u64,
    modified: SystemTime,
    index: MetadataIndex,
}

fn recovery_priority(kind: RecoveryKind) -> u8 {
    match kind {
        RecoveryKind::Canonical => 3,
        RecoveryKind::Pending => 2,
        RecoveryKind::Temp => 1,
        RecoveryKind::Backup => 0,
    }
}

fn parse_recovery_candidate(path: &Path) -> Option<RecoveryCandidate> {
    let _ = path.file_name()?.to_str()?;
    let raw = fs::read_to_string(path).ok()?;
    let mut index: MetadataIndex = serde_json::from_str(&raw).ok()?;
    if index.schema_version == 0 {
        index.schema_version = METADATA_SCHEMA_VERSION;
    }
    if index.schema_version != METADATA_SCHEMA_VERSION {
        return None;
    }
    let modified = path
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Some(RecoveryCandidate {
        kind: RecoveryKind::Pending,
        write_generation: index.write_generation,
        modified,
        index,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use tempfile::tempdir;
    use time::Duration;

    use super::*;
    use crate::secrets::{SecretStore, test_support::MemorySecretStore};

    fn identity(email: &str, subject: &str) -> DisplayIdentity {
        DisplayIdentity {
            email: email.to_owned(),
            subject: Some(subject.to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        }
    }

    fn rewrite_index(path: &Path, email: &str, updated_at: OffsetDateTime, write_generation: u64) {
        let raw = fs::read_to_string(path).expect("read index");
        let mut index: MetadataIndex = serde_json::from_str(&raw).expect("parse index");
        let account = index.accounts.first_mut().expect("account");
        account.email = email.to_owned();
        account.updated_at = updated_at;
        index.write_generation = write_generation;
        fs::write(
            path,
            serde_json::to_string_pretty(&index).expect("serialize index"),
        )
        .expect("write index");
    }

    #[test]
    fn refreshes_existing_account_by_subject() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (first, created) = repo
            .save_snapshot(&env, &identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");
        assert!(created);
        let (second, created) = repo
            .save_snapshot(&env, &identity("person2@example.com", "sub-1"), &snapshot)
            .expect("save");
        assert!(!created);
        assert_eq!(first.id, second.id);
        assert_eq!(second.email, "person2@example.com");
    }

    #[derive(Clone, Default)]
    struct FailingDeleteSecretStore {
        inner: MemorySecretStore,
    }

    impl SecretStore for FailingDeleteSecretStore {
        fn save(&self, key: &str, value: &str) -> Result<()> {
            self.inner.save(key, value)
        }

        fn load(&self, key: &str) -> Result<Option<String>> {
            self.inner.load(key)
        }

        fn delete(&self, _key: &str) -> Result<()> {
            Err(anyhow!("delete failed"))
        }
    }

    #[test]
    fn delete_rolls_back_metadata_when_secret_delete_fails() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), FailingDeleteSecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let error = repo
            .delete_snapshot(&env, saved.id)
            .expect_err("delete should fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("failed to delete snapshot secret"));
        assert!(rendered.contains("delete failed"));
        let restored = repo.get_account(&env, saved.id).expect("get account");
        assert!(restored.is_some());
    }

    #[test]
    fn recovers_metadata_from_backup_when_primary_is_missing() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert!(recovered.is_some());
        assert!(!metadata_path.exists());
        assert!(backup_path.exists());
    }

    #[test]
    fn recovers_newer_temp_before_older_backup() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");

        let temp_path = temp.path().join("metadata.json.tmp-test");
        fs::copy(&backup_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "second@example.com");
        assert!(!metadata_path.exists());
        assert!(temp_path.exists());
    }

    #[test]
    fn falls_back_to_backup_when_temp_is_invalid() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");
        fs::write(temp.path().join("metadata.json.tmp-test"), "{not-json").expect("write temp");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn ignores_recovery_candidates_with_unsupported_schema() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");
        let raw = fs::read_to_string(&backup_path).expect("read backup");
        let mut index: MetadataIndex = serde_json::from_str(&raw).expect("parse backup");
        index.schema_version = METADATA_SCHEMA_VERSION + 1;
        fs::write(
            temp.path().join("metadata.json.tmp-test"),
            serde_json::to_string_pretty(&index).expect("serialize temp"),
        )
        .expect("write temp");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn errors_when_only_invalid_recovery_candidates_exist() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());

        fs::write(temp.path().join("metadata.json.tmp-test"), "{not-json").expect("write temp");

        let error = repo
            .best_available_index()
            .expect_err("invalid recovery state should fail");
        assert!(format!("{error:#}").contains("failed to parse metadata recovery state"));
    }

    #[test]
    fn errors_when_canonical_metadata_is_unreadable() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;

        fs::write(temp.path().join("metadata.json"), "{not-json").expect("write metadata");

        let error = repo.list_accounts(&env).expect_err("list should fail");
        assert!(format!("{error:#}").contains("failed to parse"));
    }

    #[test]
    fn recovers_newest_valid_candidate_across_temp_and_backup() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        let temp_path = temp.path().join("metadata.json.tmp-old");
        fs::copy(&metadata_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now, 1);
        let backup_path = temp.path().join("metadata.json.bak-new");
        rewrite_index(
            &metadata_path,
            "first@example.com",
            now + Duration::days(1),
            2,
        );
        fs::rename(&metadata_path, &backup_path).expect("move backup");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn prefers_canonical_when_metadata_file_is_valid() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let temp_path = temp.path().join("metadata.json.tmp-new");
        fs::copy(&metadata_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn prefers_pending_temp_when_it_is_newer_than_canonical() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let temp_path = temp.path().join("metadata.json.tmp-new");
        fs::copy(&metadata_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);
        fs::copy(&temp_path, temp.path().join("metadata.json.pending")).expect("write pending");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "second@example.com");
    }

    #[test]
    fn successful_save_cleans_up_its_recovery_artifacts() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        repo.save_snapshot(&env, &identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let entries = fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !entries
                .iter()
                .any(|name| name.starts_with("metadata.json.tmp-"))
        );
        assert!(
            !entries
                .iter()
                .any(|name| name.starts_with("metadata.json.bak-"))
        );
    }

    #[test]
    fn falls_back_to_valid_recovery_when_canonical_is_corrupt() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&env, &identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::copy(&metadata_path, &backup_path).expect("copy backup");
        fs::write(&metadata_path, "{not-json").expect("corrupt canonical");

        let recovered = repo.get_account(&env, saved.id).expect("recover account");
        assert!(recovered.is_some());
    }

    #[test]
    fn activation_sync_removes_duplicate_identity_rows() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        let first = repo
            .save_snapshot(
                &env,
                &DisplayIdentity {
                    email: "legacy@example.com".to_owned(),
                    subject: None,
                    name: Some("Tester".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                },
                &snapshot,
            )
            .expect("save first")
            .0;
        let duplicate = repo
            .save_snapshot(&env, &identity("current@example.com", "sub-1"), &snapshot)
            .expect("save duplicate")
            .0;

        let updated = repo
            .sync_activated_account(&env, first.id, &identity("current@example.com", "sub-1"))
            .expect("sync");
        assert_eq!(updated.email, "current@example.com");

        let accounts = repo.list_accounts(&env).expect("list");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, first.id);
        assert!(
            repo.secret_store
                .load(&duplicate.secret_key)
                .expect("load duplicate secret")
                .is_none()
        );
    }

    #[test]
    fn activation_sync_succeeds_when_duplicate_secret_cleanup_fails() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), FailingDeleteSecretStore::default());
        let env = EnvironmentKind::Windows;
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        let first = repo
            .save_snapshot(
                &env,
                &DisplayIdentity {
                    email: "legacy@example.com".to_owned(),
                    subject: None,
                    name: Some("Tester".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                },
                &snapshot,
            )
            .expect("save first")
            .0;
        let duplicate = repo
            .save_snapshot(&env, &identity("current@example.com", "sub-1"), &snapshot)
            .expect("save duplicate")
            .0;

        let updated = repo
            .sync_activated_account(&env, first.id, &identity("current@example.com", "sub-1"))
            .expect("sync");
        assert_eq!(updated.email, "current@example.com");

        let accounts = repo.list_accounts(&env).expect("list");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, first.id);
        assert!(
            repo.secret_store
                .load(&duplicate.secret_key)
                .expect("load duplicate secret")
                .is_some()
        );
    }
}
