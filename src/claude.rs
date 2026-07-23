use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::codex::LiveAuthBundle;
use crate::env::AppEnv;
use crate::model::{DisplayIdentity, SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile};

/// Placeholder email when Claude is signed in but CLI identity lookup failed.
pub const CLAUDE_UNKNOWN_EMAIL: &str = "claude-user@unknown.com";

pub fn claude_dir(env: &AppEnv) -> PathBuf {
    env.home_dir.join(".claude")
}

pub fn get_local_username() -> Result<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .context("failed to determine local username from environment (USER/USERNAME)")
}

pub fn try_read_live_claude_auth(env: &AppEnv) -> Result<Option<LiveAuthBundle>> {
    if !claude_auth_present(env)? {
        return Ok(None);
    }
    read_live_claude_auth(env).map(Some)
}

/// Lightweight identity-only discovery for status/UI polls.
/// Prefer the credentials file; never block on Keychain prompts or `claude` CLI.
pub fn try_read_live_claude_identity(env: &AppEnv) -> Result<Option<DisplayIdentity>> {
    let credentials_path = claude_dir(env).join(".credentials.json");
    if credentials_path.exists() {
        let mut identity = base_claude_identity();
        if let Ok(text) = fs::read_to_string(&credentials_path) {
            enrich_identity_from_keychain_json(&mut identity, &text);
        }
        return Ok(Some(identity));
    }
    // Keychain only as fallback, with a hard timeout so UI polls never freeze.
    // 800ms matches full-auth path — 350ms was too aggressive and often returned
    // no live Claude identity, blanking CL in the menu bar.
    if let Some(password) = read_keychain_password_timeout(Duration::from_millis(800)) {
        let mut identity = base_claude_identity();
        enrich_identity_from_keychain_json(&mut identity, &password);
        return Ok(Some(identity));
    }
    Ok(None)
}

fn claude_auth_present(env: &AppEnv) -> Result<bool> {
    let credentials_path = claude_dir(env).join(".credentials.json");
    if credentials_path.exists() {
        return Ok(true);
    }
    Ok(read_keychain_password_timeout(Duration::from_millis(800)).is_some())
}

fn base_claude_identity() -> DisplayIdentity {
    DisplayIdentity {
        email: CLAUDE_UNKNOWN_EMAIL.to_string(),
        subject: None,
        name: None,
        plan_label: None,
        workspace_id: None,
        workspace_name: None,
    }
}

pub fn read_live_claude_auth(env: &AppEnv) -> Result<LiveAuthBundle> {
    let identity = discover_claude_identity(env);
    let mut files = Vec::new();

    let credentials_path = claude_dir(env).join(".credentials.json");
    if credentials_path.exists() {
        let content = fs::read(&credentials_path).with_context(|| {
            format!(
                "failed to read Claude credentials file at {}",
                credentials_path.display()
            )
        })?;
        files.push(SnapshotFile {
            name: "claude_credentials.json".to_owned(),
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(content),
        });
    }

    if let Some(password) = read_keychain_password_timeout(Duration::from_millis(800)) {
        files.push(SnapshotFile {
            name: "claude_keychain.txt".to_owned(),
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(password.as_bytes()),
        });
    }

    if files.is_empty() {
        bail!(
            "No Claude Code authentication state found (credentials file and Keychain are both empty)."
        );
    }

    let snapshot = SnapshotBlob {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        files,
    };

    Ok(LiveAuthBundle { identity, snapshot })
}

fn discover_claude_identity(env: &AppEnv) -> DisplayIdentity {
    let mut identity = base_claude_identity();

    // File first (fast, no UI prompts).
    let credentials_path = claude_dir(env).join(".credentials.json");
    if let Ok(text) = fs::read_to_string(&credentials_path) {
        enrich_identity_from_keychain_json(&mut identity, &text);
    }

    if let Some(password) = read_keychain_password_timeout(Duration::from_millis(800)) {
        enrich_identity_from_keychain_json(&mut identity, &password);
    }

    // CLI is optional enrichment only — never block save/status for long.
    enrich_identity_from_claude_cli_timeout(&mut identity, Duration::from_millis(900));
    identity
}

fn enrich_identity_from_keychain_json(identity: &mut DisplayIdentity, raw: &str) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    if identity.plan_label.is_none()
        && let Some(sub_type) = oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .or_else(|| value.get("subscriptionType").and_then(Value::as_str))
    {
        identity.plan_label = Some(normalize_plan(sub_type));
    }
    if identity.email == CLAUDE_UNKNOWN_EMAIL
        && let Some(email) = oauth
            .get("email")
            .and_then(Value::as_str)
            .or_else(|| value.get("email").and_then(Value::as_str))
    {
        identity.email = email.to_owned();
    }
}

fn enrich_identity_from_claude_cli_timeout(identity: &mut DisplayIdentity, timeout: Duration) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(run_claude_auth_status());
    });
    if let Ok(Some(status_json)) = rx.recv_timeout(timeout) {
        if let Some(email) = status_json.get("email").and_then(Value::as_str) {
            identity.email = email.to_string();
        }
        if let Some(sub_type) = status_json.get("subscriptionType").and_then(Value::as_str) {
            identity.plan_label = Some(normalize_plan(sub_type));
        }
        if let Some(org_name) = status_json.get("orgName").and_then(Value::as_str) {
            identity.workspace_name = Some(org_name.to_string());
        }
        if let Some(org_id) = status_json.get("orgId").and_then(Value::as_str) {
            identity.workspace_id = Some(org_id.to_string());
        }
    }
}

fn run_claude_auth_status() -> Option<Value> {
    let claude_bin = resolve_claude_cli()?;
    let output = Command::new(claude_bin).arg("auth").arg("status").output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn normalize_plan(sub_type: &str) -> String {
    match sub_type.to_ascii_lowercase().as_str() {
        "pro" => "Pro".to_string(),
        "free" => "Free".to_string(),
        other => other.to_string(),
    }
}

fn read_keychain_password_timeout(timeout: Duration) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(read_keychain_password_blocking());
    });
    rx.recv_timeout(timeout).ok().flatten()
}

fn read_keychain_password_blocking() -> Option<String> {
    let username = get_local_username().ok()?;
    keyring::Entry::new("Claude Code-credentials", &username)
        .ok()?
        .get_password()
        .ok()
}

/// Resolve `claude` even when LaunchAgents provide a minimal PATH.
fn resolve_claude_cli() -> Option<PathBuf> {
    if let Ok(path) = which_in_path("claude") {
        return Some(path);
    }
    for candidate in [
        "/opt/homebrew/bin/claude",
        "/usr/local/bin/claude",
        &format!(
            "{}/.local/bin/claude",
            std::env::var("HOME").unwrap_or_default()
        ),
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn which_in_path(bin: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH unset")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("not found in PATH");
}

pub fn restore_claude_snapshot(env: &AppEnv, snapshot: &SnapshotBlob) -> Result<()> {
    if let Some(file) = snapshot
        .files
        .iter()
        .find(|f| f.name == "claude_credentials.json")
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.bytes_base64)
            .context("failed to decode claude_credentials.json base64")?;
        let dir = claude_dir(env);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create Claude directory at {}", dir.display()))?;
        let path = dir.join(".credentials.json");
        fs::write(&path, bytes).with_context(|| {
            format!(
                "failed to write Claude credentials file to {}",
                path.display()
            )
        })?;
    }

    if let Some(file) = snapshot
        .files
        .iter()
        .find(|f| f.name == "claude_keychain.txt")
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.bytes_base64)
            .context("failed to decode claude_keychain.txt base64")?;
        let password = String::from_utf8(bytes)
            .context("failed to parse claude_keychain.txt as UTF-8 string")?;
        let username = get_local_username()?;
        let entry = keyring::Entry::new("Claude Code-credentials", &username)?;
        entry
            .set_password(&password)
            .context("failed to restore Claude credentials into Keychain")?;
    }

    Ok(())
}
