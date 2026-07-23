use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::codex::LiveAuthBundle;
use crate::env::AppEnv;
use crate::model::{
    DisplayIdentity, EnvironmentKind, SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile,
};

const IDENTITY_KEYS: &[&str] = &[
    "cursorAuth/cachedEmail",
    "cursorAuth/cachedScopedProfile",
    "cursorAuth/stripeMembershipType",
    "cursorAuth/accessToken",
    "cursorAuth/refreshToken",
];

const AUTH_KEY_SQL: &str = "SELECT key, hex(value) FROM ItemTable WHERE key LIKE 'cursorAuth/%' OR key LIKE 'secret://cursorAuth/%'";

const IDENTITY_KEY_SQL: &str = "SELECT key, hex(value) FROM ItemTable WHERE key IN (\
    'cursorAuth/cachedEmail',\
    'cursorAuth/cachedScopedProfile',\
    'cursorAuth/stripeMembershipType',\
    'cursorAuth/accessToken',\
    'cursorAuth/refreshToken'\
);";

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
    match read_live_cursor_auth(env) {
        Ok(bundle) => Ok(Some(bundle)),
        Err(_) => Ok(None),
    }
}

/// Lightweight identity-only discovery for status/UI polls.
pub fn try_read_live_cursor_identity(env: &AppEnv) -> Result<Option<DisplayIdentity>> {
    let db_path = cursor_db_path(env)?;
    if !db_path.exists() {
        return Ok(None);
    }
    match read_cursor_kv_via_sqlite3(&db_path, IDENTITY_KEY_SQL) {
        Ok(map) if map.contains_key("cursorAuth/cachedEmail") => {
            return Ok(Some(identity_from_auth_map(&map)?));
        }
        Ok(_) => {}
        Err(_) => {}
    }
    match read_cursor_kv_rusqlite(&db_path, IDENTITY_KEYS) {
        Ok(map) if map.contains_key("cursorAuth/cachedEmail") => {
            Ok(Some(identity_from_auth_map(&map)?))
        }
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

fn resolve_sqlite3() -> PathBuf {
    for candidate in [
        "/usr/bin/sqlite3",
        "/opt/homebrew/bin/sqlite3",
        "/usr/local/bin/sqlite3",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("sqlite3")
}

fn decode_hex_bytes(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        bail!("invalid hex length {}", hex.len());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => bail!("invalid hex digit {}", b as char),
    }
}

fn read_cursor_kv_via_sqlite3(path: &Path, sql: &str) -> Result<HashMap<String, Vec<u8>>> {
    // Avoid mapping Cursor's multi-GB state DB into this process. System sqlite3
    // can fetch auth keys cheaply even while Cursor holds the file.
    let sqlite3 = resolve_sqlite3();
    let output = std::process::Command::new(&sqlite3)
        .arg("-readonly")
        .arg("-noheader")
        .arg("-separator")
        .arg("\t")
        .arg(path)
        .arg(sql)
        .output()
        .with_context(|| format!("failed to spawn {} for Cursor auth", sqlite3.display()))?;
    if !output.status.success() {
        bail!(
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let Some((key, hex_value)) = line.split_once('\t') else {
            continue;
        };
        if key.is_empty() || hex_value.is_empty() {
            continue;
        }
        let value = decode_hex_bytes(hex_value)
            .with_context(|| format!("failed to decode hex value for key {key}"))?;
        map.insert(key.to_owned(), value);
    }
    Ok(map)
}

pub fn read_live_cursor_auth(env: &AppEnv) -> Result<LiveAuthBundle> {
    let db_path = cursor_db_path(env)?;
    if !db_path.exists() {
        bail!("Cursor state database not found at {}", db_path.display());
    }

    let map = read_cursor_auth_map(&db_path)?;
    if map.is_empty() {
        bail!(
            "No Cursor authentication tokens found in the database. Please log in to Cursor first."
        );
    }
    if !map.contains_key("cursorAuth/cachedEmail") {
        bail!(
            "Cursor session is incomplete (cached email missing). Please log in to Cursor first."
        );
    }
    if !map.contains_key("cursorAuth/accessToken") && !map.contains_key("cursorAuth/refreshToken") {
        bail!("Cursor authentication tokens missing. Please log in to Cursor first.");
    }

    let identity = identity_from_auth_map(&map)?;

    let mut serialized_map = HashMap::new();
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

fn open_cursor_db_readonly(path: &Path) -> Result<Connection> {
    // Cursor's state.vscdb can be >1GB. Open via immutable read-only URI with a
    // tiny cache so status polls stay cheap and avoid OOM on LaunchAgents.
    let encoded = path.to_string_lossy().replace(' ', "%20");
    let uri = format!("file:{encoded}?mode=ro&immutable=1");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(&uri, flags)
        .or_else(|_| {
            // Fallback without immutable (DB may have a hot WAL while Cursor runs).
            let uri = format!("file:{encoded}?mode=ro");
            Connection::open_with_flags(uri, flags)
        })
        .with_context(|| format!("failed to open Cursor database at {}", path.display()))?;
    let _ = conn.busy_timeout(Duration::from_millis(400));
    let _ = conn.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA mmap_size = 0;
         PRAGMA cache_size = -2000;
         PRAGMA temp_store = MEMORY;",
    );
    Ok(conn)
}

fn row_value_bytes(value: ValueRef<'_>) -> Result<Vec<u8>> {
    // Cursor/VS Code ItemTable stores auth values as TEXT (not BLOB).
    match value {
        ValueRef::Blob(bytes) => Ok(bytes.to_vec()),
        ValueRef::Text(bytes) => Ok(bytes.to_vec()),
        ValueRef::Null => Ok(Vec::new()),
        ValueRef::Integer(v) => Ok(v.to_string().into_bytes()),
        ValueRef::Real(v) => Ok(v.to_string().into_bytes()),
    }
}

fn read_cursor_kv_rusqlite(path: &Path, keys: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
    let conn = open_cursor_db_readonly(path)?;
    let placeholders = keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT key, value FROM ItemTable WHERE key IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(keys.iter()), |row| {
        let key: String = row.get(0)?;
        let value = row_value_bytes(row.get_ref(1)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, e.into())
        })?;
        Ok((key, value))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        map.insert(key, value);
    }
    Ok(map)
}

fn read_cursor_auth_map_rusqlite(path: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let conn = open_cursor_db_readonly(path)?;
    let mut stmt = conn.prepare(
        "SELECT key, value FROM ItemTable WHERE key LIKE 'cursorAuth/%' OR key LIKE 'secret://cursorAuth/%'",
    )?;
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value = row_value_bytes(row.get_ref(1)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, e.into())
        })?;
        Ok((key, value))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        map.insert(key, value);
    }
    Ok(map)
}

fn read_cursor_auth_map(path: &Path) -> Result<HashMap<String, Vec<u8>>> {
    match read_cursor_kv_via_sqlite3(path, AUTH_KEY_SQL) {
        Ok(map) if !map.is_empty() => Ok(map),
        Ok(_) => read_cursor_auth_map_rusqlite(path)
            .context("sqlite3 returned no Cursor auth keys; rusqlite fallback also failed"),
        Err(sqlite3_err) => read_cursor_auth_map_rusqlite(path).with_context(|| {
            format!(
                "sqlite3 Cursor auth read failed ({sqlite3_err:#}); rusqlite fallback also failed"
            )
        }),
    }
}

fn identity_from_auth_map(map: &HashMap<String, Vec<u8>>) -> Result<DisplayIdentity> {
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

    let subject = token.and_then(|t| {
        let mut parts = t.split('.');
        if parts.next().is_some()
            && let Some(payload) = parts.next()
        {
            let payload_bytes = URL_SAFE_NO_PAD
                .decode(payload)
                .or_else(|_| {
                    let padding = "=".repeat((4 - payload.len() % 4) % 4);
                    URL_SAFE_NO_PAD.decode(format!("{payload}{padding}"))
                })
                .ok()?;
            let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
            claims
                .get("sub")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        } else {
            None
        }
    });

    Ok(DisplayIdentity {
        email,
        subject,
        name,
        plan_label,
        workspace_id: None,
        workspace_name: None,
    })
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

    let serialized_map: HashMap<String, String> =
        serde_json::from_slice(&json_bytes).context("failed to parse cursor_auth.json")?;

    let mut conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open Cursor database at {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;

    let tx = conn.transaction()?;

    tx.execute(
        "DELETE FROM ItemTable WHERE key LIKE 'cursorAuth/%' OR key LIKE 'secret://cursorAuth/%'",
        [],
    )?;

    let mut stmt = tx.prepare("INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2);")?;

    for (key, val_b64) in serialized_map {
        let value = STANDARD
            .decode(&val_b64)
            .with_context(|| format!("failed to decode value base64 for key {}", key))?;
        // Cursor stores ItemTable values as TEXT; keep UTF-8 strings as TEXT.
        match String::from_utf8(value) {
            Ok(text) => {
                stmt.execute(rusqlite::params![key, text])?;
            }
            Err(err) => {
                stmt.execute(rusqlite::params![key, err.into_bytes()])?;
            }
        }
    }

    stmt.finalize()?;
    tx.commit()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_text_auth_db(path: &Path) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO ItemTable (key, value) VALUES
               ('cursorAuth/cachedEmail', 'user@example.com'),
               ('cursorAuth/cachedScopedProfile', '{\"displayName\":\"Ada\"}'),
               ('cursorAuth/stripeMembershipType', 'pro'),
               ('cursorAuth/accessToken', 'aaa.bbb.ccc'),
               ('cursorAuth/refreshToken', 'refresh-token'),
               ('secret://cursorAuth/openAIKey', 'sk-test');",
        )?;
        Ok(())
    }

    #[test]
    fn reads_text_auth_values_via_sqlite3_hex() -> Result<()> {
        let dir = tempdir()?;
        let db = dir.path().join("state.vscdb");
        create_text_auth_db(&db)?;

        let map = read_cursor_kv_via_sqlite3(&db, AUTH_KEY_SQL)?;
        assert_eq!(
            String::from_utf8(map["cursorAuth/cachedEmail"].clone())?,
            "user@example.com"
        );
        assert_eq!(
            String::from_utf8(map["cursorAuth/accessToken"].clone())?,
            "aaa.bbb.ccc"
        );
        assert!(map.contains_key("secret://cursorAuth/openAIKey"));
        Ok(())
    }

    #[test]
    fn reads_text_auth_values_via_rusqlite_fallback() -> Result<()> {
        let dir = tempdir()?;
        let db = dir.path().join("state.vscdb");
        create_text_auth_db(&db)?;

        let map = read_cursor_auth_map_rusqlite(&db)?;
        assert_eq!(
            String::from_utf8(map["cursorAuth/cachedEmail"].clone())?,
            "user@example.com"
        );
        assert_eq!(
            String::from_utf8(map["cursorAuth/stripeMembershipType"].clone())?,
            "pro"
        );
        Ok(())
    }

    #[test]
    fn identity_from_text_auth_map() -> Result<()> {
        let mut map = HashMap::new();
        map.insert(
            "cursorAuth/cachedEmail".to_owned(),
            b"user@example.com".to_vec(),
        );
        map.insert(
            "cursorAuth/cachedScopedProfile".to_owned(),
            br#"{"displayName":"Ada"}"#.to_vec(),
        );
        map.insert(
            "cursorAuth/stripeMembershipType".to_owned(),
            b"pro".to_vec(),
        );
        let identity = identity_from_auth_map(&map)?;
        assert_eq!(identity.email, "user@example.com");
        assert_eq!(identity.name.as_deref(), Some("Ada"));
        assert_eq!(identity.plan_label.as_deref(), Some("Pro"));
        Ok(())
    }

    #[test]
    fn decode_hex_roundtrip() -> Result<()> {
        assert_eq!(decode_hex_bytes("68656c6c6f")?, b"hello");
        assert_eq!(decode_hex_bytes("00FF")?, [0x00, 0xff]);
        Ok(())
    }
}
