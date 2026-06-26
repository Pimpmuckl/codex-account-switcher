use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::codex;
use crate::env::AppEnv;
use crate::identity::parse_identity_from_auth_json;
use crate::model::{
    DisplayIdentity, EnvironmentKind, ExportBundle, ExportBundleAccount, ImportOutput,
    SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile,
};
use crate::repository::SnapshotRepository;
use crate::secrets::SecretStore;

pub const EXPORT_BUNDLE_SCHEMA_VERSION: u32 = 1;

pub fn export_accounts<S: SecretStore>(
    repository: &SnapshotRepository<S>,
    environment: &EnvironmentKind,
    account_ids: Option<&[Uuid]>,
) -> Result<ExportBundle> {
    let accounts = repository.list_accounts(environment)?;
    let selected = match account_ids {
        Some(ids) => accounts
            .into_iter()
            .filter(|account| ids.contains(&account.id))
            .collect(),
        None => accounts,
    };
    if selected.is_empty() {
        bail!("no saved accounts to export");
    }

    let mut exported = Vec::with_capacity(selected.len());
    for account in selected {
        let (_, snapshot) = repository.load_snapshot(environment, account.id)?;
        exported.push(ExportBundleAccount {
            id: account.id,
            email: account.email.clone(),
            label: account.label.clone(),
            subject: account.subject.clone(),
            name: account.name.clone(),
            plan_label: account.plan_label.clone(),
            snapshot,
        });
    }

    Ok(ExportBundle {
        schema_version: EXPORT_BUNDLE_SCHEMA_VERSION,
        environment: environment.clone(),
        exported_at: OffsetDateTime::now_utc(),
        accounts: exported,
    })
}

pub fn write_export_bundle(path: &Path, bundle: &ExportBundle) -> Result<()> {
    let json = serde_json::to_string_pretty(bundle).context("failed to encode export bundle")?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn read_export_bundle(path: &Path) -> Result<ExportBundle> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let bundle: ExportBundle =
        serde_json::from_slice(&bytes).context("failed to decode export bundle")?;
    if bundle.schema_version != EXPORT_BUNDLE_SCHEMA_VERSION {
        bail!(
            "unsupported export bundle schema version {}",
            bundle.schema_version
        );
    }
    Ok(bundle)
}

pub fn import_auth_file<S: SecretStore>(
    repository: &SnapshotRepository<S>,
    environment: &EnvironmentKind,
    auth_path: &Path,
    label: Option<String>,
) -> Result<ImportOutput> {
    let auth_bytes =
        fs::read(auth_path).with_context(|| format!("failed to read {}", auth_path.display()))?;
    let identity = parse_identity_from_auth_json(&auth_bytes)?;
    let mut files = vec![SnapshotFile {
        name: "auth.json".to_owned(),
        bytes_base64: STANDARD.encode(auth_bytes),
    }];
    let cap_sid_path = auth_path
        .parent()
        .map(|dir| dir.join("cap_sid"))
        .filter(|path| path.exists());
    if let Some(cap_sid_path) = cap_sid_path {
        let cap_sid_bytes = fs::read(&cap_sid_path)
            .with_context(|| format!("failed to read {}", cap_sid_path.display()))?;
        files.push(SnapshotFile {
            name: "cap_sid".to_owned(),
            bytes_base64: STANDARD.encode(cap_sid_bytes),
        });
    } else {
        files.push(SnapshotFile {
            name: "cap_sid".to_owned(),
            bytes_base64: STANDARD.encode(b""),
        });
    }
    let snapshot = SnapshotBlob {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        files,
    };
    import_snapshot(repository, environment, &identity, label, snapshot)
}

pub fn import_export_bundle<S: SecretStore>(
    repository: &SnapshotRepository<S>,
    environment: &EnvironmentKind,
    bundle: &ExportBundle,
) -> Result<Vec<ImportOutput>> {
    if bundle.environment != *environment {
        bail!(
            "export bundle environment {:?} does not match current environment {:?}",
            bundle.environment,
            environment
        );
    }
    let mut outputs = Vec::with_capacity(bundle.accounts.len());
    for account in &bundle.accounts {
        let identity = DisplayIdentity {
            email: account.email.clone(),
            subject: account.subject.clone(),
            name: account.name.clone(),
            plan_label: account.plan_label.clone(),
        };
        outputs.push(import_snapshot(
            repository,
            environment,
            &identity,
            account.label.clone(),
            account.snapshot.clone(),
        )?);
    }
    Ok(outputs)
}

fn import_snapshot<S: SecretStore>(
    repository: &SnapshotRepository<S>,
    environment: &EnvironmentKind,
    identity: &DisplayIdentity,
    label: Option<String>,
    snapshot: SnapshotBlob,
) -> Result<ImportOutput> {
    codex::validate_import_snapshot(&snapshot)?;
    let (metadata, created) = repository.save_snapshot(environment, identity, &snapshot)?;
    let metadata = if label.is_some() {
        repository.set_account_label(environment, metadata.id, label)?
    } else {
        metadata
    };
    Ok(ImportOutput {
        account_id: metadata.id,
        email: metadata.email,
        label: metadata.label,
        created,
    })
}

pub fn import_live_auth<S: SecretStore>(
    repository: &SnapshotRepository<S>,
    env: &AppEnv,
    label: Option<String>,
) -> Result<ImportOutput> {
    let live = codex::read_live_auth_bundle(env)?;
    import_snapshot(repository, &env.kind, &live.identity, label, live.snapshot)
}
