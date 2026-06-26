use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use time::OffsetDateTime;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

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
const EXPORT_ZIP_ENTRY: &str = "bundle.json";

pub fn write_export_file(path: &Path, bundle: &ExportBundle) -> Result<()> {
    if is_zip_path(path) {
        write_export_zip(path, bundle)
    } else {
        write_export_bundle(path, bundle)
    }
}

pub fn read_export_file(path: &Path) -> Result<ExportBundle> {
    if is_zip_path(path) {
        read_export_zip(path)
    } else {
        read_export_bundle(path)
    }
}

fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

pub fn write_export_zip(path: &Path, bundle: &ExportBundle) -> Result<()> {
    let file =
        fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file(EXPORT_ZIP_ENTRY, options)
        .context("failed to start zip entry")?;
    let json = serde_json::to_vec_pretty(bundle).context("failed to encode export bundle")?;
    zip.write_all(&json)
        .context("failed to write export bundle into zip")?;
    zip.finish().context("failed to finalize export zip")?;
    Ok(())
}

pub fn read_export_zip(path: &Path) -> Result<ExportBundle> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("failed to read zip {}", path.display()))?;
    for name in [
        EXPORT_ZIP_ENTRY,
        "export.json",
        "codex-account-switcher-export.json",
    ] {
        if let Ok(mut entry) = archive.by_name(name) {
            return decode_bundle_reader(&mut entry)
                .with_context(|| format!("failed to decode {name} from {}", path.display()));
        }
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read zip entry {index}"))?;
        if entry.name().ends_with(".json") {
            return decode_bundle_reader(&mut entry).with_context(|| {
                format!("failed to decode {} from {}", entry.name(), path.display())
            });
        }
    }
    bail!(
        "zip archive {} does not contain an export bundle JSON file",
        path.display()
    )
}

fn decode_bundle_reader(reader: &mut dyn Read) -> Result<ExportBundle> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .context("failed to read export bundle bytes")?;
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::codex::auth_json_fixture;
    use crate::model::SnapshotFile;

    #[test]
    fn zip_export_round_trips_bundle() -> Result<()> {
        let temp = tempdir()?;
        let snapshot = SnapshotBlob {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            files: vec![
                SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: STANDARD.encode(auth_json_fixture(
                        "person@example.com",
                        "sub-1",
                        Some("plus"),
                    )),
                },
                SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: STANDARD.encode("sid"),
                },
            ],
        };
        let bundle = ExportBundle {
            schema_version: EXPORT_BUNDLE_SCHEMA_VERSION,
            environment: EnvironmentKind::Linux,
            exported_at: OffsetDateTime::now_utc(),
            accounts: vec![ExportBundleAccount {
                id: Uuid::new_v4(),
                email: "person@example.com".to_owned(),
                label: Some("work".to_owned()),
                subject: Some("sub-1".to_owned()),
                name: None,
                plan_label: Some("Plus".to_owned()),
                snapshot,
            }],
        };
        let zip_path = temp.path().join("accounts.zip");
        write_export_zip(&zip_path, &bundle)?;
        let restored = read_export_zip(&zip_path)?;
        assert_eq!(restored.accounts.len(), 1);
        assert_eq!(restored.accounts[0].email, "person@example.com");
        assert_eq!(restored.accounts[0].label.as_deref(), Some("work"));
        Ok(())
    }
}
