use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use std::path::PathBuf;

use crate::codex::LiveAuthBundle;
use crate::env::AppEnv;
use crate::model::{
    DisplayIdentity, EnvironmentKind, SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile,
};

pub fn cursor_db_path(env: &AppEnv) -> Result<PathBuf> {
    let relative_path = match env.kind {
        EnvironmentKind::Macos => {
            "Library/Application Support/Cursor/User/globalStorage/state.vscdb"
        }
        EnvironmentKind::Windows => "AppData/Roaming/Cursor/User/globalStorage/state.vscdb",
        EnvironmentKind::Linux | EnvironmentKind::Wsl => {
            ".config/Cursor/User/globalStorage/state.vscdb"
        }
    };
    Ok(env.home_dir.join(relative_path))
}

pub fn try_read_live_cursor_auth(env: &AppEnv) -> Result<Option<LiveAuthBundle>> {
    let db_path = cursor_db_path(env)?;
    if !db_path.exists() {
        return Ok(None);
    }
    read_live_cursor_auth(env).map(Some)
}

pub fn read_live_cursor_auth(env: &AppEnv) -> Result<LiveAuthBundle> {
    let db_path = cursor_db_path(env)?;
    if !db_path.exists() {
        bail!("Cursor state database not found at {}", db_path.display());
    }

    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("failed to open Cursor database at {}", db_path.display()))?;

    let mut stmt =
        conn.prepare("SELECT key, value FROM ItemTable WHERE key LIKE '%cursorAuth%';")?;
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value: Vec<u8> = row.get(1)?;
        Ok((key, value))
    })?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (key, value) = row?;
        map.insert(key, value);
    }

    if map.is_empty() {
        bail!(
            "No Cursor authentication tokens found in the database. Please log in to Cursor first."
        );
    }

    // Extract DisplayIdentity fields
    let email = map
        .get("cursorAuth/cachedEmail")
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
        .context("Cursor cached email not found in database")?;

    let cached_profile = map
        .get("cursorAuth/cachedScopedProfile")
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok());

    let name = cached_profile
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|val| {
            val.get("displayName")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        });

    let plan_label = map
        .get("cursorAuth/stripeMembershipType")
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
        .map(|plan| {
            if plan.eq_ignore_ascii_case("pro") {
                "Pro".to_string()
            } else if plan.eq_ignore_ascii_case("free") {
                "Free".to_string()
            } else {
                plan
            }
        });

    let token = map
        .get("cursorAuth/accessToken")
        .or_else(|| map.get("cursorAuth/refreshToken"))
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok());

    let subject = if let Some(t) = token {
        let mut parts = t.split('.');
        if parts.next().is_some()
            && let Some(payload) = parts.next()
        {
            if let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(payload).or_else(|_| {
                let padding = "=".repeat((4 - payload.len() % 4) % 4);
                URL_SAFE_NO_PAD.decode(format!("{payload}{padding}"))
            }) {
                if let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) {
                    claims
                        .get("sub")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let identity = DisplayIdentity {
        email,
        subject,
        name,
        plan_label,
        workspace_id: None,
        workspace_name: None,
    };

    let mut serialized_map = std::collections::HashMap::new();
    for (k, v) in &map {
        serialized_map.insert(k.clone(), STANDARD.encode(v));
    }

    let json_bytes = serde_json::to_vec_pretty(&serialized_map)?;

    let files = vec![SnapshotFile {
        name: "cursor_auth.json".to_owned(),
        bytes_base64: STANDARD.encode(json_bytes),
    }];

    let snapshot = SnapshotBlob {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        files,
    };

    Ok(LiveAuthBundle { identity, snapshot })
}

pub fn restore_cursor_snapshot(env: &AppEnv, snapshot: &SnapshotBlob) -> Result<()> {
    let db_path = cursor_db_path(env)?;
    if !db_path.exists() {
        bail!("Cursor state database not found at {}", db_path.display());
    }

    let file = snapshot
        .files
        .iter()
        .find(|f| f.name == "cursor_auth.json")
        .context("snapshot missing cursor_auth.json")?;

    let json_bytes = STANDARD
        .decode(&file.bytes_base64)
        .context("failed to decode cursor_auth.json base64")?;

    let serialized_map: std::collections::HashMap<String, String> =
        serde_json::from_slice(&json_bytes).context("failed to parse cursor_auth.json")?;

    let mut conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("failed to open Cursor database at {}", db_path.display()))?;

    let tx = conn.transaction()?;

    tx.execute("DELETE FROM ItemTable WHERE key LIKE '%cursorAuth%';", [])?;

    let mut stmt = tx.prepare("INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2);")?;

    for (key, val_b64) in serialized_map {
        let value = STANDARD
            .decode(&val_b64)
            .with_context(|| format!("failed to decode value base64 for key {}", key))?;
        stmt.execute(rusqlite::params![key, value])?;
    }

    stmt.finalize()?;
    tx.commit()?;

    Ok(())
}
