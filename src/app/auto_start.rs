use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::codex;
use crate::env::{self, AppEnv};
use crate::model::{
    AutoStartUsageWindowAccountResult, AutoStartUsageWindowsRunOutput,
    AutoStartUsageWindowsStatusOutput, DisplayIdentity, SnapshotBlob,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{MigratingSecretStore, SecretStore};
use crate::settings::{load_settings, save_settings};

use super::App;

pub const AUTO_START_USAGE_WINDOW_POLL_SECONDS: u64 = 300;

const PING_INSTRUCTIONS: &str = "Reply only with ACK.";
const PING_PROMPT: &str = "ACK";

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn auto_start_usage_windows_status(&self) -> Result<AutoStartUsageWindowsStatusOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        Ok(auto_start_status_output(settings.auto_start_usage_windows))
    }

    pub fn set_auto_start_usage_windows(
        &self,
        enabled: bool,
    ) -> Result<AutoStartUsageWindowsStatusOutput> {
        let mut settings = load_settings(&self.env.app_data_dir)?;
        settings.auto_start_usage_windows = enabled;
        save_settings(&self.env.app_data_dir, &settings)?;
        Ok(auto_start_status_output(enabled))
    }

    pub fn auto_start_usage_windows_once(
        &self,
        require_enabled: bool,
    ) -> Result<AutoStartUsageWindowsRunOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        let enabled = settings.auto_start_usage_windows;
        let mut output = AutoStartUsageWindowsRunOutput {
            enabled,
            checked_accounts: 0,
            pinged_accounts: Vec::new(),
            skipped: Vec::new(),
        };
        if require_enabled && !enabled {
            return Ok(output);
        }

        let accounts = self.repository.list_accounts(&self.env.kind)?;
        output.checked_accounts = accounts.len();
        let now = OffsetDateTime::now_utc();
        let mut due_accounts = Vec::new();
        for account in &accounts {
            match self.usage(Some(account.id)) {
                Ok(usage) => {
                    if usage
                        .usage
                        .weekly
                        .as_ref()
                        .is_some_and(|weekly| usage_window_needs_ping(weekly.reset_at, now))
                    {
                        due_accounts.push((account.id, account.email.clone()));
                    }
                }
                Err(error) => output
                    .skipped
                    .push(format!("{}: usage unavailable: {error:#}", account.email)),
            }
        }
        if due_accounts.is_empty() {
            return Ok(output);
        }

        for (account_id, email) in due_accounts {
            let result = match self.ping_usage_window_account(account_id, &email) {
                Ok(result) => result,
                Err(error) => AutoStartUsageWindowAccountResult {
                    account_id,
                    email,
                    status: "failed".to_owned(),
                    selected_model: None,
                    detail: Some(format!("{error:#}")),
                },
            };
            output.pinged_accounts.push(result);
        }

        Ok(output)
    }

    fn ping_usage_window_account(
        &self,
        account_id: Uuid,
        email: &str,
    ) -> Result<AutoStartUsageWindowAccountResult> {
        let (_, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
        let identity = codex::identity_from_snapshot(&snapshot)?;
        let (refreshed_snapshot, selected_model) =
            run_codex_usage_ping(&self.env, &snapshot, &identity)?;
        let refreshed_identity = codex::identity_from_snapshot(&refreshed_snapshot)?;
        self.repository.replace_snapshot(
            &self.env.kind,
            account_id,
            &refreshed_identity,
            &refreshed_snapshot,
            None,
        )?;
        let usage = self.usage(Some(account_id))?;
        let now = OffsetDateTime::now_utc();
        let status = match usage.usage.weekly {
            Some(weekly) if weekly.reset_at > now => "started",
            Some(_) => "unchanged",
            None => "usage_missing",
        };
        Ok(AutoStartUsageWindowAccountResult {
            account_id,
            email: email.to_owned(),
            status: status.to_owned(),
            selected_model,
            detail: None,
        })
    }
}

pub fn spawn_auto_start_usage_windows_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = thread::Builder::new()
            .name("auto-start-usage-windows".to_owned())
            .spawn(|| {
                loop {
                    if let Err(error) = run_auto_start_usage_windows_for_detected_env() {
                        eprintln!("auto-start usage-window check failed: {error:#}");
                    }
                    thread::sleep(StdDuration::from_secs(AUTO_START_USAGE_WINDOW_POLL_SECONDS));
                }
            });
    });
}

pub(crate) fn run_auto_start_usage_windows_check_now() -> Result<()> {
    run_auto_start_usage_windows_for_detected_env()
}

fn run_auto_start_usage_windows_for_detected_env() -> Result<()> {
    let env = env::detect()?;
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        MigratingSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    let app = App::new(env, repository);
    let _ = app.auto_start_usage_windows_once(true)?;
    Ok(())
}

fn auto_start_status_output(enabled: bool) -> AutoStartUsageWindowsStatusOutput {
    AutoStartUsageWindowsStatusOutput {
        enabled,
        poll_seconds: AUTO_START_USAGE_WINDOW_POLL_SECONDS,
    }
}

fn usage_window_needs_ping(reset_at: OffsetDateTime, now: OffsetDateTime) -> bool {
    reset_at <= now
}

fn run_codex_usage_ping(
    env: &AppEnv,
    snapshot: &SnapshotBlob,
    identity: &DisplayIdentity,
) -> Result<(SnapshotBlob, Option<String>)> {
    let work_dir =
        std::env::temp_dir().join(format!("codex-account-switcher-ping-{}", Uuid::new_v4()));
    let result = run_codex_usage_ping_in_temp_home(env, snapshot, identity, &work_dir);
    let _ = fs::remove_dir_all(&work_dir);
    result
}

fn run_codex_usage_ping_in_temp_home(
    env: &AppEnv,
    snapshot: &SnapshotBlob,
    identity: &DisplayIdentity,
    work_dir: &Path,
) -> Result<(SnapshotBlob, Option<String>)> {
    fs::create_dir_all(work_dir)
        .with_context(|| format!("failed to create {}", work_dir.display()))?;
    let temp_env = AppEnv {
        kind: env.kind.clone(),
        home_dir: work_dir.to_path_buf(),
        codex_root: work_dir.join("codex-home"),
        app_data_dir: work_dir.join("app-data"),
    };
    codex::restore_snapshot(&temp_env, snapshot, identity, false)
        .context("failed to seed temporary Codex auth home")?;

    let instruction_file = work_dir.join("instructions.md");
    fs::write(&instruction_file, PING_INSTRUCTIONS)
        .with_context(|| format!("failed to write {}", instruction_file.display()))?;

    let selected_model = best_available_mini_model(&temp_env.codex_root);
    let mut command = Command::new("codex");
    command
        .env("CODEX_HOME", &temp_env.codex_root)
        .arg("exec")
        .arg("--ephemeral")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("--skip-git-repo-check")
        .arg("--cd")
        .arg(work_dir)
        .arg("-c")
        .arg("cli_auth_credentials_store=\"file\"")
        .arg("-c")
        .arg(format!(
            "model_instructions_file={}",
            toml_string_literal(&instruction_file.display().to_string())
        ))
        .arg("-c")
        .arg("model_reasoning_effort=\"low\"");
    if let Some(model) = selected_model.as_deref() {
        command.arg("-m").arg(model);
    }
    command
        .arg(PING_PROMPT)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    strip_codex_thread_env(&mut command);

    let status = run_command_with_timeout(&mut command, StdDuration::from_secs(120))
        .context("failed to run `codex exec`; make sure `codex` is on PATH")?;
    if !status.success() {
        return Err(anyhow!("`codex exec` failed with {status}"));
    }
    let refreshed = codex::read_live_auth_bundle(&temp_env)
        .context("failed to read refreshed temporary Codex auth")?;
    Ok((refreshed.snapshot, selected_model))
}

fn run_command_with_timeout(command: &mut Command, timeout: StdDuration) -> Result<ExitStatus> {
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("process timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(StdDuration::from_millis(100));
    }
}

fn strip_codex_thread_env(command: &mut Command) {
    for key in ["CODEX_THREAD_ID", "CODEX_INTERNAL_ORIGINATOR_OVERRIDE"] {
        command.env_remove(key);
    }
}

fn best_available_mini_model(codex_home: &Path) -> Option<String> {
    for bundled in [false, true] {
        let mut command = Command::new("codex");
        command.env("CODEX_HOME", codex_home);
        command.arg("debug").arg("models");
        if bundled {
            command.arg("--bundled");
        }
        strip_codex_thread_env(&mut command);
        let Ok(output) = command.output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        if let Some(model) = select_mini_model_from_catalog(&raw) {
            return Some(model);
        }
    }
    None
}

fn select_mini_model_from_catalog(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let models = if let Some(models) = value.get("models").and_then(Value::as_array) {
        models
    } else {
        value.as_array()?
    };
    let mut candidates = models
        .iter()
        .enumerate()
        .filter_map(|(position, model)| {
            let slug = model
                .get("slug")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)?;
            if !slug.ends_with("-mini")
                || model.get("supported_in_api").and_then(Value::as_bool) == Some(false)
            {
                return None;
            }
            let priority = model
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX);
            Some((priority, position, slug.to_owned()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(priority, position, _)| (*priority, *position));
    candidates.into_iter().map(|(_, _, slug)| slug).next()
}

fn toml_string_literal(value: &str) -> String {
    let mut literal = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            other => literal.push(other),
        }
    }
    literal.push('"');
    literal
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;

    #[test]
    fn usage_window_needs_ping_when_reset_is_due_or_past() {
        let now = OffsetDateTime::now_utc();

        assert!(usage_window_needs_ping(now, now));
        assert!(usage_window_needs_ping(now - Duration::minutes(1), now));
        assert!(!usage_window_needs_ping(now + Duration::minutes(1), now));
    }

    #[test]
    fn mini_model_selector_prefers_lowest_priority_available_mini() {
        let raw = r#"{
          "models": [
            {"slug": "gpt-5.5", "priority": 1, "supported_in_api": true},
            {"slug": "gpt-5.3-mini", "priority": 20, "supported_in_api": true},
            {"slug": "gpt-5.4-mini", "priority": 10, "supported_in_api": true}
          ]
        }"#;

        assert_eq!(
            select_mini_model_from_catalog(raw).as_deref(),
            Some("gpt-5.4-mini")
        );
    }

    #[test]
    fn mini_model_selector_skips_api_unsupported_models() {
        let raw = r#"[
          {"slug": "gpt-hidden-mini", "priority": 1, "supported_in_api": false},
          {"slug": "gpt-visible-mini", "priority": 2, "supported_in_api": true}
        ]"#;

        assert_eq!(
            select_mini_model_from_catalog(raw).as_deref(),
            Some("gpt-visible-mini")
        );
    }

    #[test]
    fn toml_string_literal_escapes_windows_paths() {
        assert_eq!(
            toml_string_literal(r#"C:\Temp\codex "ping"\instructions.md"#),
            r#""C:\\Temp\\codex \"ping\"\\instructions.md""#
        );
    }
}
