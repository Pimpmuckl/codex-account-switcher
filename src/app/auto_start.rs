use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::Sender;
#[cfg(windows)]
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, anyhow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AUTH_FILES, AutoStartUsageWindowAccountResult, AutoStartUsageWindowsRunOutput,
    AutoStartUsageWindowsStatusOutput, DisplayIdentity, SnapshotBlob,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{LocalSecretStore, SecretStore};
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

        let _run_guard = AUTO_START_RUN_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| anyhow!("auto-start usage-window run lock poisoned"))?;
        let accounts = self.repository.list_accounts()?;
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
        let (_, snapshot) = self.repository.load_snapshot(account_id)?;
        let identity = codex::identity_from_snapshot(&snapshot)?;
        let ping_result = run_codex_usage_ping(&self.env, &snapshot, &identity)?;
        let (current_metadata, current_snapshot) = self.repository.load_snapshot(account_id)?;
        if current_snapshot != snapshot {
            return Err(anyhow!(
                "saved snapshot changed while ping was running; skipped write-back"
            ));
        }
        let refreshed_identity = codex::identity_from_snapshot(&ping_result.snapshot)?;
        self.repository.replace_snapshot(
            account_id,
            &refreshed_identity,
            &ping_result.snapshot,
            current_metadata.cached_usage,
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
            detail: ping_result.cleanup_warning,
        })
    }
}

static AUTO_START_RUN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static AUTO_START_CHECK_LISTENERS: OnceLock<Mutex<Vec<Sender<()>>>> = OnceLock::new();

#[cfg(windows)]
pub(crate) fn subscribe_auto_start_usage_windows_checks() -> Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    let mut listeners = AUTO_START_CHECK_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("auto-start usage-window listener lock poisoned");
    listeners.push(sender);
    receiver
}

pub fn spawn_auto_start_usage_windows_worker(env: AppEnv) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(move || {
        let _ = thread::Builder::new()
            .name("auto-start-usage-windows".to_owned())
            .spawn(move || {
                loop {
                    if let Err(error) = run_auto_start_usage_windows_for_env(env.clone()) {
                        eprintln!("auto-start usage-window check failed: {error:#}");
                    }
                    notify_auto_start_usage_windows_checked();
                    thread::sleep(StdDuration::from_secs(AUTO_START_USAGE_WINDOW_POLL_SECONDS));
                }
            });
    });
}

fn notify_auto_start_usage_windows_checked() {
    let Some(listeners) = AUTO_START_CHECK_LISTENERS.get() else {
        return;
    };
    let Ok(mut listeners) = listeners.lock() else {
        eprintln!("auto-start usage-window listener lock poisoned");
        return;
    };
    listeners.retain(|listener| listener.send(()).is_ok());
}

#[cfg(windows)]
pub(crate) fn run_auto_start_usage_windows_check_now(env: AppEnv) -> Result<()> {
    run_auto_start_usage_windows_for_env(env)
}

fn run_auto_start_usage_windows_for_env(env: AppEnv) -> Result<()> {
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        LocalSecretStore::new(&env.app_data_dir.join("snapshots")),
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

struct CodexUsagePingResult {
    snapshot: SnapshotBlob,
    cleanup_warning: Option<String>,
}

fn run_codex_usage_ping(
    env: &AppEnv,
    snapshot: &SnapshotBlob,
    identity: &DisplayIdentity,
) -> Result<CodexUsagePingResult> {
    let work_dir =
        std::env::temp_dir().join(format!("codex-account-switcher-ping-{}", Uuid::new_v4()));
    let result = run_codex_usage_ping_in_temp_home(env, snapshot, identity, &work_dir);
    let cleanup = remove_temp_auth_home(&work_dir);
    match (result, cleanup) {
        (Ok(snapshot), Ok(())) => Ok(CodexUsagePingResult {
            snapshot,
            cleanup_warning: None,
        }),
        (Ok(snapshot), Err(error)) => {
            scrub_temp_auth_material(&work_dir)
                .context("temporary auth cleanup failed and auth scrub failed")?;
            let cleanup_warning = match remove_temp_auth_home(&work_dir) {
                Ok(()) => {
                    format!("temporary auth cleanup initially failed, then recovered: {error:#}")
                }
                Err(final_error) => format!(
                    "temporary auth cleanup failed after auth scrub; non-auth temp files may remain: {error:#}; final cleanup failed: {final_error:#}"
                ),
            };
            Ok(CodexUsagePingResult {
                snapshot,
                cleanup_warning: Some(cleanup_warning),
            })
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            let mut combined = error.context(format!(
                "also failed to remove temporary auth home: {cleanup_error:#}"
            ));
            match scrub_temp_auth_material(&work_dir) {
                Ok(()) => {
                    if let Err(final_error) = remove_temp_auth_home(&work_dir) {
                        combined = combined.context(format!(
                            "temporary auth scrub succeeded but final cleanup failed: {final_error:#}"
                        ));
                    }
                }
                Err(scrub_error) => {
                    combined =
                        combined.context(format!("temporary auth scrub failed: {scrub_error:#}"));
                }
            }
            Err(combined)
        }
    }
}

fn run_codex_usage_ping_in_temp_home(
    env: &AppEnv,
    snapshot: &SnapshotBlob,
    identity: &DisplayIdentity,
    work_dir: &Path,
) -> Result<SnapshotBlob> {
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

    let mut command = Command::new("codex");
    command
        .env("CODEX_HOME", &temp_env.codex_root)
        .env("HOME", &temp_env.home_dir)
        .env("USERPROFILE", &temp_env.home_dir)
        .env("APPDATA", temp_env.home_dir.join("AppData").join("Roaming"))
        .env(
            "LOCALAPPDATA",
            temp_env.home_dir.join("AppData").join("Local"),
        )
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
    Ok(refreshed.snapshot)
}

fn remove_temp_auth_home(path: &Path) -> Result<()> {
    for attempt in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt < 4 => {
                thread::sleep(StdDuration::from_millis(100));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

fn scrub_temp_auth_material(work_dir: &Path) -> Result<()> {
    let codex_home = work_dir.join("codex-home");
    match fs::remove_dir_all(&codex_home) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }

    let mut errors = Vec::new();
    for file_name in AUTH_FILES {
        remove_file_if_exists(&codex_home.join(file_name), &mut errors);
    }
    match fs::read_dir(&codex_home) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if file_name.to_string_lossy().starts_with(".cas-") {
                    remove_dir_if_exists(&entry.path(), &mut errors);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(format!(
            "failed to inspect {}: {error}",
            codex_home.display()
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to scrub temporary auth material: {}",
            errors.join("; ")
        ))
    }
}

fn remove_file_if_exists(path: &Path, errors: &mut Vec<String>) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(format!("failed to remove {}: {error}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path, errors: &mut Vec<String>) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(format!("failed to remove {}: {error}", path.display())),
    }
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

    #[cfg(windows)]
    #[test]
    fn auto_start_check_notification_reaches_listener() {
        let receiver = subscribe_auto_start_usage_windows_checks();

        notify_auto_start_usage_windows_checked();

        receiver
            .recv_timeout(StdDuration::from_secs(1))
            .expect("listener should receive auto-start check notification");
    }

    #[test]
    fn toml_string_literal_escapes_windows_paths() {
        assert_eq!(
            toml_string_literal(r#"C:\Temp\codex "ping"\instructions.md"#),
            r#""C:\\Temp\\codex \"ping\"\\instructions.md""#
        );
    }

    #[test]
    fn scrub_temp_auth_material_removes_managed_auth_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let codex_home = temp.path().join("codex-home");
        let backup_dir = codex_home.join(".cas-backup-test");
        fs::create_dir_all(&backup_dir)?;
        fs::write(codex_home.join("auth.json"), "{}")?;
        fs::write(codex_home.join("cap_sid"), "sid")?;
        fs::write(backup_dir.join("auth.json"), "{}")?;
        fs::write(backup_dir.join("cap_sid"), "sid")?;

        scrub_temp_auth_material(temp.path())?;

        assert!(!codex_home.exists());
        Ok(())
    }
}
