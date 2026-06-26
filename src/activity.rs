use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize)]
struct ActivityEntry {
    timestamp: OffsetDateTime,
    action: &'static str,
    account_id: Uuid,
    email: String,
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

pub fn log_account_activation(
    app_data_dir: &Path,
    account_id: Uuid,
    email: &str,
    label: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    append_entry(
        app_data_dir,
        ActivityEntry {
            timestamp: OffsetDateTime::now_utc(),
            action: "activate",
            account_id,
            email: email.to_owned(),
            label: label.map(str::to_owned),
            detail: detail.map(str::to_owned),
        },
    )
}

pub fn log_pick_best(
    app_data_dir: &Path,
    account_id: Uuid,
    email: &str,
    label: Option<&str>,
    score: f64,
) -> Result<()> {
    append_entry(
        app_data_dir,
        ActivityEntry {
            timestamp: OffsetDateTime::now_utc(),
            action: "pick_best",
            account_id,
            email: email.to_owned(),
            label: label.map(str::to_owned),
            detail: Some(format!("score={score:.1}")),
        },
    )
}

fn append_entry(app_data_dir: &Path, entry: ActivityEntry) -> Result<()> {
    std::fs::create_dir_all(app_data_dir)
        .with_context(|| format!("failed to create {}", app_data_dir.display()))?;
    let path = app_data_dir.join("activity.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(&entry).context("failed to encode activity entry")?;
    writeln!(file, "{line}").with_context(|| format!("failed to append {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn appends_activity_lines() -> Result<()> {
        let temp = tempdir()?;
        let id = Uuid::new_v4();
        log_account_activation(temp.path(), id, "person@example.com", Some("work"), None)?;
        let contents = std::fs::read_to_string(temp.path().join("activity.jsonl"))?;
        assert!(contents.contains("person@example.com"));
        assert!(contents.contains("work"));
        Ok(())
    }
}
