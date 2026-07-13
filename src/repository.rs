use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod codec;
mod index_store;

use anyhow::{Context, Result, anyhow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::codex;
use crate::model::{AccountUsageView, DisplayIdentity, SavedAccountMetadata, SnapshotBlob};
use crate::secrets::SecretStore;
use crate::usage::usage_error_requires_login;
use codec::{decode_snapshot, encode_snapshot};
use index_store::MetadataIndexStore;

static REPOSITORY_WRITE_LOCK: Mutex<()> = Mutex::new(());

pub struct SnapshotRepository<S> {
    index_store: MetadataIndexStore,
    secret_store: S,
}

impl<S> SnapshotRepository<S>
where
    S: SecretStore,
{
    pub fn new(data_dir: &Path, secret_store: S) -> Self {
        Self {
            index_store: MetadataIndexStore::new(data_dir),
            secret_store,
        }
    }

    pub fn list_accounts(&self) -> Result<Vec<SavedAccountMetadata>> {
        let _guard = repository_write_lock()?;
        self.list_accounts_unlocked()
    }

    fn list_accounts_unlocked(&self) -> Result<Vec<SavedAccountMetadata>> {
        let mut accounts = self.load_index()?.accounts;
        accounts.sort_by_key(|account| std::cmp::Reverse(account.updated_at));
        Ok(accounts)
    }

    pub fn get_account(&self, account_id: Uuid) -> Result<Option<SavedAccountMetadata>> {
        let _guard = repository_write_lock()?;
        self.get_account_unlocked(account_id)
    }

    fn get_account_unlocked(&self, account_id: Uuid) -> Result<Option<SavedAccountMetadata>> {
        Ok(self
            .list_accounts_unlocked()?
            .into_iter()
            .find(|account| account.id == account_id))
    }

    pub fn save_snapshot(
        &self,
        identity: &DisplayIdentity,
        snapshot: &SnapshotBlob,
    ) -> Result<(SavedAccountMetadata, bool)> {
        let _guard = repository_write_lock()?;
        let mut index = self.load_index()?;
        let now = OffsetDateTime::now_utc();
        let encoded_snapshot = encode_snapshot(snapshot)?;
        let existing_index = index.accounts.iter().position(|account| {
            DisplayIdentity {
                account_key: account.account_key.clone(),
                email: account.email.clone(),
                name: account.name.clone(),
                plan_label: account.plan_label.clone(),
            }
            .matches(identity)
        });

        let (metadata, created) = if let Some(position) = existing_index {
            let account = &mut index.accounts[position];
            account.account_key = identity.account_key.clone();
            account.email = identity.email.clone();
            account.name = identity.name.clone();
            account.plan_label = identity.plan_label.clone();
            account.cached_usage_error = None;
            account.updated_at = now;
            (account.clone(), false)
        } else {
            let id = Uuid::new_v4();
            let metadata = SavedAccountMetadata {
                id,
                account_key: identity.account_key.clone(),
                legacy_subject: None,
                email: identity.email.clone(),
                name: identity.name.clone(),
                plan_label: identity.plan_label.clone(),
                secret_key: format!("snapshot:{id}"),
                created_at: now,
                updated_at: now,
                last_activated_at: None,
                cached_usage: None,
                cached_usage_error: None,
            };
            index.accounts.push(metadata.clone());
            (metadata, true)
        };

        self.secret_store
            .save(&metadata.secret_key, &encoded_snapshot)?;
        self.index_store.save_index(&index)?;
        Ok((metadata, created))
    }

    pub fn load_snapshot(&self, account_id: Uuid) -> Result<(SavedAccountMetadata, SnapshotBlob)> {
        let _guard = repository_write_lock()?;
        self.load_snapshot_unlocked(account_id)
    }

    fn load_snapshot_unlocked(
        &self,
        account_id: Uuid,
    ) -> Result<(SavedAccountMetadata, SnapshotBlob)> {
        let metadata = self
            .get_account_unlocked(account_id)?
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let encoded_snapshot = self
            .secret_store
            .load(&metadata.secret_key)?
            .ok_or_else(|| {
                anyhow!(
                    "saved snapshot data missing for {}. Re-save that account while logged into it.",
                    metadata.email
                )
            })?;
        let snapshot = decode_snapshot(&encoded_snapshot)?;
        Ok((metadata, snapshot))
    }

    pub fn replace_snapshot(
        &self,
        account_id: Uuid,
        identity: &DisplayIdentity,
        snapshot: &SnapshotBlob,
        usage: Option<AccountUsageView>,
    ) -> Result<SavedAccountMetadata> {
        let _guard = repository_write_lock()?;
        self.replace_snapshot_unlocked(account_id, identity, snapshot, usage)
    }

    pub fn replace_snapshot_if_current(
        &self,
        account_id: Uuid,
        expected_snapshot: &SnapshotBlob,
        identity: &DisplayIdentity,
        snapshot: &SnapshotBlob,
        usage: Option<AccountUsageView>,
    ) -> Result<bool> {
        let _guard = repository_write_lock()?;
        let (_, current_snapshot) = self.load_snapshot_unlocked(account_id)?;
        if current_snapshot != *expected_snapshot {
            return Ok(false);
        }
        self.replace_snapshot_unlocked(account_id, identity, snapshot, usage)?;
        Ok(true)
    }

    fn replace_snapshot_unlocked(
        &self,
        account_id: Uuid,
        identity: &DisplayIdentity,
        snapshot: &SnapshotBlob,
        usage: Option<AccountUsageView>,
    ) -> Result<SavedAccountMetadata> {
        let mut index = self.load_index()?;
        let position = index
            .accounts
            .iter()
            .position(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let encoded_snapshot = encode_snapshot(snapshot)?;
        let account = &mut index.accounts[position];
        account.account_key = identity.account_key.clone();
        account.email = identity.email.clone();
        account.name = identity.name.clone();
        account.plan_label = identity.plan_label.clone();
        account.cached_usage = usage;
        account.cached_usage_error = None;
        let metadata = account.clone();
        self.secret_store
            .save(&metadata.secret_key, &encoded_snapshot)?;
        self.index_store.save_index(&index)?;
        Ok(metadata)
    }

    pub fn record_usage_error(
        &self,
        account_id: Uuid,
        usage_error: String,
    ) -> Result<SavedAccountMetadata> {
        let _guard = repository_write_lock()?;
        self.record_usage_error_unlocked(account_id, usage_error)
    }

    pub fn record_usage_error_if_current(
        &self,
        account_id: Uuid,
        expected_snapshot: &SnapshotBlob,
        usage_error: String,
    ) -> Result<bool> {
        let _guard = repository_write_lock()?;
        let (_, current_snapshot) = self.load_snapshot_unlocked(account_id)?;
        if current_snapshot != *expected_snapshot {
            return Ok(false);
        }
        self.record_usage_error_unlocked(account_id, usage_error)?;
        Ok(true)
    }

    fn record_usage_error_unlocked(
        &self,
        account_id: Uuid,
        usage_error: String,
    ) -> Result<SavedAccountMetadata> {
        let mut index = self.load_index()?;
        let position = index
            .accounts
            .iter()
            .position(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let account = &mut index.accounts[position];
        if account
            .cached_usage_error
            .as_deref()
            .is_some_and(usage_error_requires_login)
            && !usage_error_requires_login(&usage_error)
        {
            return Ok(account.clone());
        }
        account.cached_usage_error = Some(usage_error);
        let metadata = account.clone();
        self.index_store.save_index(&index)?;
        Ok(metadata)
    }

    pub fn delete_snapshot(&self, account_id: Uuid) -> Result<()> {
        let _guard = repository_write_lock()?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .accounts
            .iter()
            .position(|account| account.id == account_id)
        else {
            return Err(anyhow!("saved account {account_id} not found"));
        };
        let metadata = index.accounts.remove(position);
        let deleted_secret = self.secret_store.load(&metadata.secret_key).ok().flatten();
        if let Err(error) = self.secret_store.delete(&metadata.secret_key) {
            return Err(error).context("failed to delete saved snapshot data");
        }
        if let Err(error) = self.index_store.save_index(&index) {
            if let Some(serialized_snapshot) = deleted_secret.as_deref()
                && let Err(restore_error) = self
                    .secret_store
                    .save(&metadata.secret_key, serialized_snapshot)
            {
                return Err(anyhow!(
                    "failed to persist deleted metadata and failed to restore saved snapshot data: {error:#}; restore error: {restore_error:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn sync_activated_account(
        &self,
        account_id: Uuid,
        identity: &DisplayIdentity,
    ) -> Result<SavedAccountMetadata> {
        let _guard = repository_write_lock()?;
        let mut index = self.load_index()?;
        let now = OffsetDateTime::now_utc();
        let Some(account_position) = index
            .accounts
            .iter()
            .position(|account| account.id == account_id)
        else {
            return Err(anyhow!("saved account {account_id} not found"));
        };
        let duplicate_positions = index
            .accounts
            .iter()
            .enumerate()
            .filter(|(position, account)| {
                *position != account_position
                    && DisplayIdentity {
                        account_key: account.account_key.clone(),
                        email: account.email.clone(),
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
            .position(|account| account.id == account_id)
            .ok_or_else(|| anyhow!("saved account {account_id} not found"))?;
        let account = &mut index.accounts[adjusted_position];
        account.account_key = identity.account_key.clone();
        account.email = identity.email.clone();
        account.name = identity.name.clone();
        account.plan_label = identity.plan_label.clone();
        account.last_activated_at = Some(now);
        account.updated_at = now;
        let updated = account.clone();
        self.index_store.save_index(&index)?;
        for duplicate in duplicates {
            let _ = self.secret_store.delete(&duplicate.secret_key);
        }
        Ok(updated)
    }

    fn load_index(&self) -> Result<crate::model::MetadataIndex> {
        let mut index = self.index_store.load_index()?;
        let mut changed = false;
        for account in &mut index.accounts {
            if account.account_key.trim().is_empty()
                || account.legacy_subject.as_deref().is_some_and(|subject| {
                    !subject.trim().is_empty() && subject != account.account_key
                })
            {
                if let Some(identity) = self.identity_for_account(account) {
                    account.account_key = identity.account_key;
                } else if let Some(subject) = account.legacy_subject.as_deref() {
                    account.account_key = subject.to_owned();
                } else if account.account_key.trim().is_empty() {
                    // ponytail: old subjectless rows stay listable/deletable; snapshot refresh supplies the real key.
                    account.account_key = format!("legacy:{}", account.id);
                }
                account.legacy_subject = None;
                changed = true;
            }
        }
        if changed {
            self.index_store.save_index(&index)?;
        }
        Ok(index)
    }

    fn identity_for_account(&self, account: &SavedAccountMetadata) -> Option<DisplayIdentity> {
        let encoded_snapshot = self.secret_store.load(&account.secret_key).ok().flatten()?;
        let snapshot = decode_snapshot(&encoded_snapshot).ok()?;
        codex::identity_from_snapshot(&snapshot).ok()
    }
}

fn repository_write_lock() -> Result<MutexGuard<'static, ()>> {
    REPOSITORY_WRITE_LOCK
        .lock()
        .map_err(|_| anyhow!("snapshot repository write lock poisoned"))
}

#[cfg(test)]
impl<S> SnapshotRepository<S>
where
    S: SecretStore,
{
    fn best_available_index(&self) -> Result<Option<crate::model::MetadataIndex>> {
        self.index_store.best_available_index()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::{Result, anyhow};
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use tempfile::tempdir;
    use time::Duration;

    use super::*;
    use crate::model::{METADATA_SCHEMA_VERSION, MetadataIndex};
    use crate::repository::codec::SNAPSHOT_ENCODING_V1_MAGIC;
    use crate::secrets::{SecretStore, test_support::MemorySecretStore};

    fn identity(email: &str, account_key: &str) -> DisplayIdentity {
        DisplayIdentity {
            email: email.to_owned(),
            account_key: account_key.to_owned(),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        }
    }

    fn snapshot_with_auth(email: &str, subject: &str, account_key: &str) -> SnapshotBlob {
        let payload = serde_json::json!({
            "email": email,
            "sub": subject,
            "name": "Tester",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_key,
                "chatgpt_plan_type": "pro"
            }
        });
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
        let auth_json = serde_json::json!({
            "tokens": {
                "id_token": format!("{header}.{payload}."),
                "access_token": "access",
                "refresh_token": "refresh",
                "account_id": "acct"
            },
            "auth_mode": "chatgpt"
        });
        SnapshotBlob {
            schema_version: 1,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: STANDARD.encode(auth_json.to_string()),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: STANDARD.encode("sid"),
                },
            ],
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
    fn refreshes_existing_account_by_account_key() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (first, created) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");
        assert!(created);
        let (second, created) = repo
            .save_snapshot(&identity("person2@example.com", "sub-1"), &snapshot)
            .expect("save");
        assert!(!created);
        assert_eq!(first.id, second.id);
        assert_eq!(second.email, "person2@example.com");
    }

    #[test]
    fn loads_legacy_subject_metadata_without_account_key() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (subject_row, _) = repo
            .save_snapshot(
                &identity("subject@example.com", "account-1"),
                &snapshot_with_auth("subject@example.com", "sub-1", "account-1"),
            )
            .expect("save subject row");
        let (subjectless_row, _) = repo
            .save_snapshot(&identity("subjectless@example.com", "sub-2"), &snapshot)
            .expect("save subjectless row");
        let (corrupt_row, _) = repo
            .save_snapshot(&identity("corrupt@example.com", "sub-3"), &snapshot)
            .expect("save corrupt row");
        let metadata_path = temp.path().join("metadata.json");
        let mut raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&metadata_path).expect("read metadata"))
                .expect("parse metadata");
        for account in raw["accounts"].as_array_mut().expect("accounts") {
            let account = account.as_object_mut().expect("account");
            let account_key = account.remove("account_key").expect("account key");
            account.insert("environment".to_owned(), serde_json::json!("windows"));
            account.insert(
                "subject".to_owned(),
                if account["id"] == subject_row.id.to_string() {
                    account_key
                } else if account["id"] == corrupt_row.id.to_string() {
                    serde_json::json!("sub-3")
                } else {
                    serde_json::Value::Null
                },
            );
        }
        repo.secret_store
            .save(&corrupt_row.secret_key, b"not-a-snapshot")
            .expect("corrupt saved snapshot");
        fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&raw).expect("serialize legacy metadata"),
        )
        .expect("write legacy metadata");

        let loaded = repo.list_accounts().expect("list legacy metadata");
        let subject_account = loaded
            .iter()
            .find(|account| account.id == subject_row.id)
            .expect("subject account");
        let subjectless_account = loaded
            .iter()
            .find(|account| account.id == subjectless_row.id)
            .expect("subjectless account");
        let corrupt_account = loaded
            .iter()
            .find(|account| account.id == corrupt_row.id)
            .expect("corrupt account");

        assert_eq!(subject_account.account_key, "account-1");
        assert_eq!(
            subjectless_account.account_key,
            format!("legacy:{}", subjectless_row.id)
        );
        assert_eq!(corrupt_account.account_key, "sub-3");
    }

    #[derive(Clone, Default)]
    struct FailingDeleteSecretStore {
        inner: MemorySecretStore,
    }

    impl SecretStore for FailingDeleteSecretStore {
        fn save(&self, key: &str, value: &[u8]) -> Result<()> {
            self.inner.save(key, value)
        }

        fn load(&self, key: &str) -> Result<Option<Vec<u8>>> {
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
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let error = repo
            .delete_snapshot(saved.id)
            .expect_err("delete should fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("failed to delete saved snapshot data"));
        assert!(rendered.contains("delete failed"));
        let restored = repo.get_account(saved.id).expect("get account");
        assert!(restored.is_some());
    }

    #[test]
    fn recovers_metadata_from_backup_when_primary_is_missing() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert!(recovered.is_some());
        assert!(!metadata_path.exists());
        assert!(backup_path.exists());
    }

    #[test]
    fn recovers_newer_temp_before_older_backup() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");

        let temp_path = temp.path().join("metadata.json.tmp-test");
        fs::copy(&backup_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "second@example.com");
        assert!(!metadata_path.exists());
        assert!(temp_path.exists());
    }

    #[test]
    fn falls_back_to_backup_when_temp_is_invalid() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::rename(&metadata_path, &backup_path).expect("move backup");
        fs::write(temp.path().join("metadata.json.tmp-test"), "{not-json").expect("write temp");

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn ignores_recovery_candidates_with_unsupported_schema() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
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

        let recovered = repo.get_account(saved.id).expect("recover account");
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
    fn ignores_invalid_pending_metadata_when_no_other_index_exists() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());

        fs::write(temp.path().join("metadata.json.pending"), "{not-json").expect("write pending");

        let index = repo.best_available_index().expect("pending-only recovery");
        assert!(index.is_none());
    }

    #[test]
    fn errors_when_canonical_metadata_is_unreadable() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());

        fs::write(temp.path().join("metadata.json"), "{not-json").expect("write metadata");

        let error = repo.list_accounts().expect_err("list should fail");
        assert!(format!("{error:#}").contains("failed to parse"));
    }

    #[test]
    fn recovers_newest_valid_candidate_across_temp_and_backup() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
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

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn prefers_canonical_when_metadata_file_is_valid() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let temp_path = temp.path().join("metadata.json.tmp-new");
        fs::copy(&metadata_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "first@example.com");
    }

    #[test]
    fn prefers_pending_temp_when_it_is_newer_than_canonical() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("first@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let now = OffsetDateTime::now_utc();
        rewrite_index(&metadata_path, "first@example.com", now, 1);
        let temp_path = temp.path().join("metadata.json.tmp-new");
        fs::copy(&metadata_path, &temp_path).expect("copy temp");
        rewrite_index(&temp_path, "second@example.com", now + Duration::days(1), 2);
        fs::copy(&temp_path, temp.path().join("metadata.json.pending")).expect("write pending");

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert_eq!(recovered.expect("account").email, "second@example.com");
    }

    #[test]
    fn successful_save_cleans_up_its_recovery_artifacts() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        repo.save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
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
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");

        let metadata_path = temp.path().join("metadata.json");
        let backup_path = temp.path().join("metadata.json.bak-test");
        fs::copy(&metadata_path, &backup_path).expect("copy backup");
        fs::write(&metadata_path, "{not-json").expect("corrupt canonical");

        let recovered = repo.get_account(saved.id).expect("recover account");
        assert!(recovered.is_some());
    }

    #[test]
    fn activation_sync_removes_duplicate_identity_rows() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        let first = repo
            .save_snapshot(&identity("stale@example.com", "stale-key"), &snapshot)
            .expect("save first")
            .0;
        let duplicate = repo
            .save_snapshot(&identity("current@example.com", "sub-1"), &snapshot)
            .expect("save duplicate")
            .0;

        let updated = repo
            .sync_activated_account(first.id, &identity("current@example.com", "sub-1"))
            .expect("sync");
        assert_eq!(updated.email, "current@example.com");

        let accounts = repo.list_accounts().expect("list");
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
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };

        let first = repo
            .save_snapshot(&identity("stale@example.com", "stale-key"), &snapshot)
            .expect("save first")
            .0;
        let duplicate = repo
            .save_snapshot(&identity("current@example.com", "sub-1"), &snapshot)
            .expect("save duplicate")
            .0;

        let updated = repo
            .sync_activated_account(first.id, &identity("current@example.com", "sub-1"))
            .expect("sync");
        assert_eq!(updated.email, "current@example.com");

        let accounts = repo.list_accounts().expect("list");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, first.id);
        assert!(
            repo.secret_store
                .load(&duplicate.secret_key)
                .expect("load duplicate secret")
                .is_some()
        );
    }

    #[test]
    fn save_snapshot_stores_compressed_payload_and_loads_it_back() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "auth-payload".to_owned(),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "cap-payload".to_owned(),
                },
            ],
        };

        let (saved, _) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");
        let raw = repo
            .secret_store
            .load(&saved.secret_key)
            .expect("load stored payload")
            .expect("stored payload");
        assert!(raw.starts_with(SNAPSHOT_ENCODING_V1_MAGIC));

        let loaded = repo.load_snapshot(saved.id).expect("load snapshot").1;
        assert_eq!(loaded.schema_version, snapshot.schema_version);
        assert_eq!(loaded.files.len(), snapshot.files.len());
        assert_eq!(loaded.files[0].bytes_base64, "auth-payload");
        assert_eq!(loaded.files[1].bytes_base64, "cap-payload");
    }

    #[test]
    fn usage_error_is_persisted_and_cleared_by_snapshot_refresh() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let saved = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save")
            .0;

        repo.record_usage_error(saved.id, "Login required".to_owned())
            .expect("record error");
        assert_eq!(
            repo.get_account(saved.id)
                .expect("get")
                .expect("account")
                .cached_usage_error
                .as_deref(),
            Some("Login required")
        );

        repo.replace_snapshot(
            saved.id,
            &identity("person@example.com", "sub-1"),
            &snapshot,
            None,
        )
        .expect("replace");

        assert!(
            repo.get_account(saved.id)
                .expect("get")
                .expect("account")
                .cached_usage_error
                .is_none()
        );
    }

    #[test]
    fn conditional_snapshot_writes_skip_when_snapshot_changed() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let first_identity = identity("first@example.com", "sub-1");
        let first_snapshot = snapshot_with_auth("first@example.com", "subject-1", "sub-1");
        let second_identity = identity("second@example.com", "sub-2");
        let second_snapshot = snapshot_with_auth("second@example.com", "subject-2", "sub-2");
        let saved = repo
            .save_snapshot(&first_identity, &first_snapshot)
            .expect("save")
            .0;

        assert!(
            repo.replace_snapshot_if_current(
                saved.id,
                &first_snapshot,
                &second_identity,
                &second_snapshot,
                None,
            )
            .expect("conditional replace")
        );
        assert!(
            !repo
                .replace_snapshot_if_current(
                    saved.id,
                    &first_snapshot,
                    &first_identity,
                    &first_snapshot,
                    None,
                )
                .expect("stale conditional replace")
        );
        assert!(
            !repo
                .record_usage_error_if_current(
                    saved.id,
                    &first_snapshot,
                    "Usage unavailable".to_owned(),
                )
                .expect("stale conditional error")
        );

        let (loaded, loaded_snapshot) = repo.load_snapshot(saved.id).expect("load");
        assert_eq!(loaded.email, "second@example.com");
        assert_eq!(loaded_snapshot, second_snapshot);
        assert!(loaded.cached_usage_error.is_none());
    }

    #[test]
    fn transient_usage_error_does_not_replace_login_required_marker() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let saved = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save")
            .0;

        repo.record_usage_error(saved.id, "Login required".to_owned())
            .expect("record login error");
        repo.record_usage_error(
            saved.id,
            "Usage unavailable: failed to query Codex usage".to_owned(),
        )
        .expect("record transient error");

        assert_eq!(
            repo.get_account(saved.id)
                .expect("get")
                .expect("account")
                .cached_usage_error
                .as_deref(),
            Some("Login required")
        );
    }

    #[test]
    fn load_snapshot_rejects_plain_json_payloads() {
        let temp = tempdir().expect("tempdir");
        let repo = SnapshotRepository::new(temp.path(), MemorySecretStore::default());
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: "plain-json-auth".to_owned(),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: "plain-json-cap".to_owned(),
                },
            ],
        };
        let (saved, _) = repo
            .save_snapshot(&identity("person@example.com", "sub-1"), &snapshot)
            .expect("save");
        let plain_json = serde_json::to_vec(&snapshot).expect("serialize plain JSON");
        repo.secret_store
            .save(&saved.secret_key, &plain_json)
            .expect("overwrite with plain JSON payload");

        let error = repo
            .load_snapshot(saved.id)
            .expect_err("plain JSON payload should be rejected");
        assert!(format!("{error:#}").contains("unsupported stored snapshot encoding"));
    }
}
