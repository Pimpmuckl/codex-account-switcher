use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
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
    AUTH_FILES, AccountUsageView, AutoStartUsageWindowAccountResult,
    AutoStartUsageWindowsRunOutput, AutoStartUsageWindowsStatusOutput, DisplayIdentity,
    SavedAccountMetadata, SnapshotBlob, UsageOutput, UsageSource, UsageWindowView,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{LocalSecretStore, SecretStore};
use crate::settings::{load_settings, save_settings};
use crate::usage::{
    fetch_usage, usage_error_message, usage_error_requires_login, usage_target_from_snapshot,
};

use super::App;

pub const AUTO_START_USAGE_WINDOW_POLL_SECONDS: u64 = 300;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

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
            if !auto_start_check_needs_usage_refresh(account) {
                continue;
            }
            let cached_weekly = cached_weekly_window(account);
            let cached_due = cached_weekly.is_some_and(|weekly| weekly.reset_at <= now);
            let previous_reset_at = cached_weekly.map(|weekly| weekly.reset_at);
            match self.fetch_usage_for_auto_start_check(account.id) {
                Ok((usage, refreshed_snapshot)) => {
                    let needs_ping = auto_start_check_needs_ping(
                        cached_due,
                        usage.usage.weekly.as_ref(),
                        previous_reset_at,
                        now,
                    );
                    if let Err(error) = self.repository.replace_snapshot(
                        account.id,
                        &usage.account,
                        &refreshed_snapshot,
                        auto_start_check_usage_cache(account, &usage.usage, needs_ping),
                    ) {
                        output
                            .skipped
                            .push(format!("{}: usage unavailable: {error:#}", account.email));
                        continue;
                    }
                    if needs_ping {
                        due_accounts.push((account.id, account.email.clone()));
                    }
                }
                Err(error) => {
                    let _ = self
                        .repository
                        .record_usage_error(account.id, usage_error_message(&error));
                    output
                        .skipped
                        .push(format!("{}: usage unavailable: {error:#}", account.email));
                }
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
            Some(weekly) if weekly.reset_at > now => "pinged",
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

    fn fetch_usage_for_auto_start_check(
        &self,
        account_id: Uuid,
    ) -> Result<(UsageOutput, SnapshotBlob)> {
        let (snapshot, _, _) = self.load_activation_target(account_id)?;
        let target = usage_target_from_snapshot(
            self.env.kind.clone(),
            snapshot,
            UsageSource::SavedAccessToken,
            true,
        )?;
        fetch_usage(target)
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

fn cached_weekly_window(account: &SavedAccountMetadata) -> Option<&UsageWindowView> {
    if account
        .cached_usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        return None;
    }
    account.cached_usage.as_ref()?.weekly.as_ref()
}

fn auto_start_check_needs_usage_refresh(account: &SavedAccountMetadata) -> bool {
    !account
        .cached_usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
}

fn auto_start_check_usage_cache(
    account: &SavedAccountMetadata,
    usage: &AccountUsageView,
    needs_ping: bool,
) -> Option<AccountUsageView> {
    if needs_ping {
        account.cached_usage.clone()
    } else {
        Some(usage.clone())
    }
}

fn auto_start_check_needs_ping(
    cached_due: bool,
    weekly: Option<&UsageWindowView>,
    previous_reset_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> bool {
    let refreshed_active =
        weekly.is_some_and(|weekly| weekly.reset_at > now && weekly.remaining_percent < 100);
    !refreshed_active
        && (cached_due
            || weekly.is_some_and(|weekly| usage_window_needs_ping(weekly, previous_reset_at, now)))
}

fn usage_window_needs_ping(
    weekly: &UsageWindowView,
    previous_reset_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> bool {
    weekly.reset_at <= now
        || weekly.remaining_percent == 100
            && previous_reset_at.is_some_and(|previous| previous < weekly.reset_at)
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

    let mut command = codex_command();
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

fn codex_command() -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new(
            windows_codex_exe()
                .unwrap_or_else(|| PathBuf::from("codex.cmd"))
                .as_os_str(),
        );
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("codex")
    }
}

#[cfg(windows)]
fn windows_codex_exe() -> Option<PathBuf> {
    windows_codex_exe_in_paths(std::env::split_paths(&std::env::var_os("PATH")?))
}

#[cfg(windows)]
fn windows_codex_exe_in_paths(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let package = if cfg!(target_arch = "aarch64") {
        "codex-win32-arm64"
    } else {
        "codex-win32-x64"
    };
    let target = if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    paths.into_iter().find_map(|dir| {
        let direct = dir.join("codex.exe");
        if direct.is_file() {
            Some(direct)
        } else {
            let npm = dir
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("node_modules")
                .join("@openai")
                .join(package)
                .join("vendor")
                .join(target)
                .join("bin")
                .join("codex.exe");
            npm.is_file().then_some(npm)
        }
    })
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

    fn weekly_window(reset_at: OffsetDateTime, remaining_percent: u8) -> UsageWindowView {
        UsageWindowView {
            used_percent: 100u8.saturating_sub(remaining_percent),
            remaining_percent,
            reset_at,
        }
    }

    fn weekly_usage(reset_at: OffsetDateTime) -> AccountUsageView {
        weekly_usage_with_remaining(reset_at, 100)
    }

    fn weekly_usage_with_remaining(
        reset_at: OffsetDateTime,
        remaining_percent: u8,
    ) -> AccountUsageView {
        AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(weekly_window(reset_at, remaining_percent)),
            credits: None,
        }
    }

    fn saved_account(
        cached_usage: Option<AccountUsageView>,
        cached_usage_error: Option<String>,
    ) -> SavedAccountMetadata {
        SavedAccountMetadata {
            id: Uuid::new_v4(),
            account_key: "sub-1".to_owned(),
            legacy_subject: None,
            email: "person@example.com".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
            secret_key: "secret".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            cached_usage,
            cached_usage_error,
        }
    }

    #[test]
    fn usage_window_needs_ping_when_reset_is_due_or_past() {
        let now = OffsetDateTime::now_utc();

        assert!(usage_window_needs_ping(&weekly_window(now, 50), None, now));
        assert!(usage_window_needs_ping(
            &weekly_window(now - Duration::minutes(1), 50),
            None,
            now
        ));
        assert!(!usage_window_needs_ping(
            &weekly_window(now + Duration::minutes(1), 50),
            None,
            now
        ));
    }

    #[test]
    fn usage_window_needs_ping_when_full_window_moves_forward() {
        let now = OffsetDateTime::now_utc();
        let previous_reset_at = now + Duration::days(7);
        let moved_reset_at = previous_reset_at + Duration::minutes(5);
        let full_moved_weekly = weekly_window(moved_reset_at, 100);
        let used_moved_weekly = weekly_window(moved_reset_at, 99);

        assert!(usage_window_needs_ping(
            &full_moved_weekly,
            Some(previous_reset_at),
            now
        ));
        assert!(!usage_window_needs_ping(
            &full_moved_weekly,
            Some(moved_reset_at),
            now
        ));
        assert!(!usage_window_needs_ping(&full_moved_weekly, None, now));
        assert!(!usage_window_needs_ping(
            &used_moved_weekly,
            Some(previous_reset_at),
            now
        ));
        assert!(auto_start_check_needs_ping(
            false,
            Some(&full_moved_weekly),
            Some(previous_reset_at),
            now
        ));
        assert!(!auto_start_check_needs_ping(
            false,
            Some(&full_moved_weekly),
            Some(moved_reset_at),
            now
        ));
    }

    #[test]
    fn cached_weekly_window_ignores_login_required_errors() {
        let account = saved_account(
            Some(weekly_usage(OffsetDateTime::UNIX_EPOCH)),
            Some("Login required: Codex auth expired.".to_owned()),
        );

        assert!(cached_weekly_window(&account).is_none());
    }

    #[test]
    fn auto_start_usage_refresh_polls_every_account_with_usable_auth() {
        let now = OffsetDateTime::now_utc();

        assert!(auto_start_check_needs_usage_refresh(&saved_account(
            Some(weekly_usage_with_remaining(now + Duration::days(1), 99)),
            None
        )));
        assert!(!auto_start_check_needs_usage_refresh(&saved_account(
            Some(weekly_usage(now - Duration::minutes(1))),
            Some("Login required: Codex auth expired.".to_owned())
        )));
    }

    #[test]
    fn auto_start_check_preserves_cached_usage_until_ping_succeeds() {
        let old_usage = weekly_usage(OffsetDateTime::UNIX_EPOCH);
        let new_usage = weekly_usage(OffsetDateTime::UNIX_EPOCH + Duration::days(7));
        let account = saved_account(Some(old_usage.clone()), None);

        assert_eq!(
            auto_start_check_usage_cache(&account, &new_usage, true)
                .expect("cached usage should remain")
                .weekly
                .expect("weekly usage")
                .reset_at,
            old_usage.weekly.expect("old weekly usage").reset_at
        );
        assert_eq!(
            auto_start_check_usage_cache(&account, &new_usage, false)
                .expect("fresh usage should save")
                .weekly
                .expect("weekly usage")
                .reset_at,
            new_usage.weekly.expect("new weekly usage").reset_at
        );
    }

    #[test]
    fn auto_start_check_skips_cached_due_when_refreshed_window_is_active() {
        let now = OffsetDateTime::now_utc();
        let active_weekly = weekly_window(now + Duration::days(7), 99);
        let full_weekly = weekly_window(now + Duration::days(7), 100);

        assert!(!auto_start_check_needs_ping(
            true,
            Some(&active_weekly),
            Some(now - Duration::minutes(1)),
            now
        ));
        assert!(auto_start_check_needs_ping(
            true,
            Some(&full_weekly),
            Some(now - Duration::minutes(1)),
            now
        ));
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
    fn codex_command_uses_platform_launcher() {
        let command = codex_command();
        let program = command.get_program().to_string_lossy();
        if cfg!(windows) {
            assert!(program.ends_with("codex.exe") || program == "codex.cmd");
        } else {
            assert_eq!(program, "codex");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_codex_exe_finds_direct_path_exe() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let exe = temp.path().join("codex.exe");
        fs::write(&exe, "")?;

        assert_eq!(
            windows_codex_exe_in_paths([temp.path().to_owned()]),
            Some(exe)
        );
        Ok(())
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
