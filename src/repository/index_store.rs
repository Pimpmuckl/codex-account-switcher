use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::file_store::{RecoveryFileKind, list_recovery_files, replace_file_with_recovery};
use crate::model::{METADATA_SCHEMA_VERSION, MetadataIndex};
use anyhow::{Context, Result, anyhow};

pub(super) struct MetadataIndexStore {
    metadata_path: PathBuf,
}

impl MetadataIndexStore {
    pub(super) fn new(data_dir: &Path) -> Self {
        Self {
            metadata_path: data_dir.join("metadata.json"),
        }
    }

    pub(super) fn load_index(&self) -> Result<MetadataIndex> {
        match self.best_available_index()? {
            Some(index) => Ok(index),
            None => Ok(MetadataIndex {
                schema_version: METADATA_SCHEMA_VERSION,
                write_generation: 0,
                accounts: Vec::new(),
            }),
        }
    }

    pub(super) fn save_index(&self, index: &MetadataIndex) -> Result<()> {
        let mut persisted = index.clone();
        persisted.write_generation = index.write_generation.saturating_add(1);
        let json =
            serde_json::to_string_pretty(&persisted).context("failed to serialize metadata")?;
        replace_file_with_recovery(&self.metadata_path, Some(json.as_bytes()), |temp_path| {
            fs::write(temp_path, &json)
                .with_context(|| format!("failed to write {}", temp_path.display()))
        })
    }

    pub(super) fn best_available_index(&self) -> Result<Option<MetadataIndex>> {
        let Some(parent) = self.metadata_path.parent() else {
            return Ok(None);
        };
        if !parent.exists() {
            return Ok(None);
        }
        let pending_path = self.metadata_path.with_extension("json.pending");
        let entries = list_recovery_files(&self.metadata_path, Some(&pending_path))?;
        let mut invalid_kinds = Vec::new();
        let mut candidates = entries
            .into_iter()
            .filter_map(|entry| {
                let raw = match fs::read_to_string(&entry.path) {
                    Ok(raw) => raw,
                    Err(_) => {
                        invalid_kinds.push(entry.kind);
                        return None;
                    }
                };
                let mut index: MetadataIndex = match serde_json::from_str(&raw) {
                    Ok(index) => index,
                    Err(_) => {
                        invalid_kinds.push(entry.kind);
                        return None;
                    }
                };
                if index.schema_version == 0 {
                    index.schema_version = METADATA_SCHEMA_VERSION;
                }
                if index.schema_version != METADATA_SCHEMA_VERSION {
                    invalid_kinds.push(entry.kind);
                    return None;
                }
                Some(RecoveryCandidate {
                    kind: entry.kind,
                    write_generation: index.write_generation,
                    modified: entry.modified,
                    index,
                })
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() && !invalid_kinds.is_empty() {
            if invalid_kinds
                .iter()
                .all(|kind| matches!(kind, RecoveryFileKind::Pending))
            {
                return Ok(None);
            }
            return Err(anyhow!(
                "failed to parse metadata recovery state under {}",
                parent.display()
            ));
        }
        if let Some(canonical) = candidates
            .iter()
            .find(|entry| matches!(entry.kind, RecoveryFileKind::Canonical))
        {
            if let Some(pending) = candidates.iter().find(|entry| {
                matches!(entry.kind, RecoveryFileKind::Pending)
                    && entry.write_generation > canonical.write_generation
            }) {
                return Ok(Some(pending.index.clone()));
            }
            return Ok(Some(canonical.index.clone()));
        }
        candidates.sort_by(|left, right| {
            right
                .write_generation
                .cmp(&left.write_generation)
                .then_with(|| right.modified.cmp(&left.modified))
                .then_with(|| recovery_priority(right.kind).cmp(&recovery_priority(left.kind)))
        });
        Ok(candidates.first().map(|entry| entry.index.clone()))
    }
}

struct RecoveryCandidate {
    kind: RecoveryFileKind,
    write_generation: u64,
    modified: SystemTime,
    index: MetadataIndex,
}

fn recovery_priority(kind: RecoveryFileKind) -> u8 {
    match kind {
        RecoveryFileKind::Canonical => 3,
        RecoveryFileKind::Pending => 2,
        RecoveryFileKind::Temp => 1,
        RecoveryFileKind::Backup => 0,
    }
}
