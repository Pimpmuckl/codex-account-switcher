use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use uuid::Uuid;

#[derive(Debug, Serialize)]
struct ActivityEntry {
    #[serde(with = "crate::time_serde::offset_datetime")]
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

pub fn read_activity_log(app_data_dir: &Path) -> Result<Vec<ActivityEntryView>> {
    let path = app_data_dir.join("activity.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut entries = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            let timestamp = parse_activity_timestamp(entry.get("timestamp"));
            let action = entry
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            let account_id = entry
                .get("account_id")
                .and_then(|id| id.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(Uuid::nil);
            let email = entry
                .get("email")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string();
            let label = entry
                .get("label")
                .and_then(|l| l.as_str())
                .map(String::from);
            let detail = entry
                .get("detail")
                .and_then(|d| d.as_str())
                .map(String::from);

            let (message, subtext) = format_activity_copy(&action, &email, label.as_deref(), detail.as_deref());
            let timestamp_iso = timestamp
                .map(|t| t.format(&Rfc3339).unwrap_or_default())
                .unwrap_or_default();
            let timestamp_unix = timestamp.map(|t| t.unix_timestamp()).unwrap_or(0);

            entries.push(ActivityEntryView {
                timestamp: timestamp_iso,
                timestamp_unix,
                action,
                account_id,
                email,
                label,
                detail,
                message,
                subtext,
            });
        }
    }
    entries.reverse();
    entries.truncate(50);
    Ok(entries)
}

fn format_activity_copy(
    action: &str,
    email: &str,
    label: Option<&str>,
    detail: Option<&str>,
) -> (String, String) {
    let who = match label {
        Some(l) if !l.is_empty() => format!("{email} ({l})"),
        _ => email.to_owned(),
    };
    let message = match action {
        "activate" => {
            if who.is_empty() {
                "Switched account".to_owned()
            } else {
                format!("Switched to {who}")
            }
        }
        "pick_best" => {
            if who.is_empty() {
                "Picked best quota account".to_owned()
            } else {
                format!("Best quota → {who}")
            }
        }
        other if !other.is_empty() => {
            if who.is_empty() {
                other.to_owned()
            } else {
                format!("{other}: {who}")
            }
        }
        _ => {
            if who.is_empty() {
                "Activity".to_owned()
            } else {
                who
            }
        }
    };
    let subtext = detail.unwrap_or("").to_owned();
    (message, subtext)
}

/// Accept RFC3339 string or legacy `time` serde tuple arrays from older logs.
fn parse_activity_timestamp(value: Option<&serde_json::Value>) -> Option<OffsetDateTime> {
    let value = value?;
    if let Some(s) = value.as_str() {
        return OffsetDateTime::parse(s, &Rfc3339).ok();
    }
    if let Some(items) = value.as_array()
        && items.len() >= 6
    {
        let year = items[0].as_i64()? as i32;
        let ordinal = items[1].as_u64()? as u16;
        let hour = items[2].as_u64().unwrap_or(0) as u8;
        let minute = items[3].as_u64().unwrap_or(0) as u8;
        let second = items[4].as_u64().unwrap_or(0) as u8;
        let nanosecond = items[5].as_u64().unwrap_or(0) as u32;
        let off_h = items.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
        let off_m = items.get(7).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
        let off_s = items.get(8).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
        let date = Date::from_ordinal_date(year, ordinal).ok()?;
        let time = Time::from_hms_nano(hour, minute, second, nanosecond).ok()?;
        let offset = UtcOffset::from_hms(off_h, off_m, off_s).ok()?;
        return Some(PrimitiveDateTime::new(date, time).assume_offset(offset));
    }
    if let Some(secs) = value.as_i64() {
        return OffsetDateTime::from_unix_timestamp(secs).ok();
    }
    None
}

#[derive(Debug, Serialize)]
pub struct ActivityEntryView {
    /// RFC3339 string for display parsers.
    pub timestamp: String,
    /// Unix seconds for simple JS `new Date(ts * 1000)`.
    pub timestamp_unix: i64,
    pub action: String,
    pub account_id: Uuid,
    pub email: String,
    pub label: Option<String>,
    pub detail: Option<String>,
    /// Preformatted primary line for the Overview UI.
    pub message: String,
    /// Secondary line (provider / score / etc.).
    pub subtext: String,
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
        // New writes use RFC3339 timestamps, not tuple arrays.
        assert!(contents.contains("T"));

        let read = read_activity_log(temp.path())?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].email, "person@example.com");
        assert!(read[0].message.contains("person@example.com"));
        assert!(read[0].message.contains("Switched"));
        assert!(read[0].timestamp_unix > 0);
        assert!(!read[0].timestamp.is_empty());
        Ok(())
    }

    #[test]
    fn reads_legacy_tuple_timestamp() {
        let raw = r#"{"timestamp":[2026,186,3,52,38,0,0,0,0],"action":"activate","account_id":"00000000-0000-0000-0000-000000000001","email":"a@b.com","label":null,"detail":"codex"}"#;
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let ts = parse_activity_timestamp(value.get("timestamp")).expect("legacy ts");
        assert_eq!(ts.year(), 2026);
        let (msg, sub) = format_activity_copy("activate", "a@b.com", None, Some("codex"));
        assert_eq!(msg, "Switched to a@b.com");
        assert_eq!(sub, "codex");
    }
}
