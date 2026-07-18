use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::Sender;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AUTH_FILES, AutoStartUsageWindowAccountResult, AutoStartUsageWindowsRunOutput,
    AutoStartUsageWindowsStatusOutput, AutoSwitchOnLimitStatusOutput, DisplayIdentity,
    LaunchAtStartupStatusOutput, ShowQuotaInMenuBarStatusOutput, SnapshotBlob,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{MigratingSecretStore, SecretStore};
use crate::settings::{DEFAULT_USAGE_WINDOW_POLL_SECONDS, load_settings, save_settings};

use super::App;

pub const NEAR_LIMIT_POLL_SECONDS: u64 = 30;
pub const URGENT_POLL_SECONDS: u64 = 15;
#[cfg(not(test))]
const AUTO_SWITCH_CODEX_WAIT_SECONDS: u64 = 120;

#[allow(dead_code)]
fn auto_switch_codex_wait_timeout() -> StdDuration {
    #[cfg(test)]
    {
        StdDuration::ZERO
    }
    #[cfg(not(test))]
    {
        StdDuration::from_secs(AUTO_SWITCH_CODEX_WAIT_SECONDS)
    }
}

const PING_INSTRUCTIONS: &str = "Reply only with ACK.";
const PING_PROMPT: &str = "ACK";

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn auto_start_usage_windows_status(&self) -> Result<AutoStartUsageWindowsStatusOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        Ok(auto_start_status_output(
            settings.auto_start_usage_windows,
            compute_poll_interval(&self.env),
        ))
    }

    pub fn set_auto_start_usage_windows(
        &self,
        enabled: bool,
    ) -> Result<AutoStartUsageWindowsStatusOutput> {
        let mut settings = load_settings(&self.env.app_data_dir)?;
        settings.auto_start_usage_windows = enabled;
        save_settings(&self.env.app_data_dir, &settings)?;
        Ok(auto_start_status_output(
            enabled,
            compute_poll_interval(&self.env),
        ))
    }

    pub fn auto_switch_on_limit_status(&self) -> Result<AutoSwitchOnLimitStatusOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        Ok(AutoSwitchOnLimitStatusOutput {
            enabled: settings.auto_switch_on_limit,
        })
    }

    pub fn set_auto_switch_on_limit(&self, enabled: bool) -> Result<AutoSwitchOnLimitStatusOutput> {
        let mut settings = load_settings(&self.env.app_data_dir)?;
        settings.auto_switch_on_limit = enabled;
        save_settings(&self.env.app_data_dir, &settings)?;
        Ok(AutoSwitchOnLimitStatusOutput { enabled })
    }

    pub fn launch_at_startup_status(&self) -> Result<LaunchAtStartupStatusOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        Ok(LaunchAtStartupStatusOutput {
            enabled: settings.launch_at_startup,
        })
    }

    pub fn show_quota_in_menu_bar_status(&self) -> Result<ShowQuotaInMenuBarStatusOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        Ok(ShowQuotaInMenuBarStatusOutput {
            enabled: settings.show_quota_in_menu_bar,
        })
    }

    pub fn set_show_quota_in_menu_bar(
        &self,
        enabled: bool,
    ) -> Result<ShowQuotaInMenuBarStatusOutput> {
        let mut settings = load_settings(&self.env.app_data_dir)?;
        settings.show_quota_in_menu_bar = enabled;
        save_settings(&self.env.app_data_dir, &settings)?;
        Ok(ShowQuotaInMenuBarStatusOutput { enabled })
    }

    pub fn set_launch_at_startup(&self, enabled: bool) -> Result<LaunchAtStartupStatusOutput> {
        let mut settings = load_settings(&self.env.app_data_dir)?;
        settings.launch_at_startup = enabled;
        save_settings(&self.env.app_data_dir, &settings)?;

        #[cfg(target_os = "macos")]
        {
            if enabled {
                if let Ok(exe) = std::env::current_exe() {
                    install_macos_launch_at_startup(&self.env.home_dir, &exe)?;
                }
            } else {
                uninstall_macos_launch_at_startup(&self.env.home_dir)?;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if enabled {
                if let Ok(exe) = std::env::current_exe() {
                    let cmd = format!(
                        "Set-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name 'CodexAccountSwitcher' -Value '\"{}\"'",
                        exe.display()
                    );
                    let _ = std::process::Command::new("powershell")
                        .args(["-Command", &cmd])
                        .output();
                }
            } else {
                let cmd = "Remove-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name 'CodexAccountSwitcher' -ErrorAction SilentlyContinue";
                let _ = std::process::Command::new("powershell")
                    .args(["-Command", cmd])
                    .output();
            }
        }

        Ok(LaunchAtStartupStatusOutput { enabled })
    }

    pub fn update_launch_at_startup_path_if_enabled(&self) -> Result<()> {
        let settings = load_settings(&self.env.app_data_dir)?;
        if !settings.launch_at_startup {
            return Ok(());
        }

        #[cfg(target_os = "macos")]
        if let Ok(exe) = std::env::current_exe() {
            update_macos_launch_at_startup_if_needed(&self.env.home_dir, &exe)?;
        }
        Ok(())
    }

    pub fn auto_switch_on_limit_once(&self) -> Result<Option<Uuid>> {
        let settings = load_settings(&self.env.app_data_dir)?;
        if !settings.auto_switch_on_limit {
            return Ok(None);
        }
        let threshold = settings.near_limit_threshold_percent;
        self.refresh_saved_usage_cache()?;
        let accounts = self.repository.list_accounts(&self.env.kind)?;
        let status = self.status()?;
        let Some(current_id) = status.current_account_saved_id else {
            return Ok(None);
        };
        let Some(current_account) = accounts.iter().find(|a| a.id == current_id) else {
            return Ok(None);
        };
        let now = OffsetDateTime::now_utc();
        let switch_reason = current_switch_reason(current_account, now);
        if switch_reason.is_none() {
            return Ok(None);
        }
        let switch_reason = switch_reason.expect("checked above");
        let scores = accounts
            .iter()
            .map(|account| {
                crate::quota_scoring::score_saved_account_for_auto_switch(
                    account.id,
                    &account.email,
                    account.label.as_deref(),
                    account.cached_usage.as_ref(),
                    account.cached_usage_error.as_deref(),
                    now,
                )
            })
            .collect::<Vec<_>>();
        let Some(target_id) = crate::quota_scoring::pick_switch_target(&scores, current_id) else {
            return Ok(None);
        };
        let target_account = accounts
            .iter()
            .find(|account| account.id == target_id)
            .context("switch target account missing after scoring")?;
        if !switch_target_is_improvement(
            switch_reason,
            target_account.cached_usage.as_ref(),
            now,
            threshold,
        ) {
            return Ok(None);
        }
        if !self.activation_preflight_warnings().is_empty() {
            if !auto_switch_should_quit_running_codex(switch_reason) {
                return Ok(None);
            }
            #[cfg(not(test))]
            {
                crate::process::quit_running_codex_app();
                crate::process::force_quit_switch_blocking_codex_processes();
            }
            if !crate::process::wait_for_codex_processes_to_exit_timeout(
                auto_switch_codex_wait_timeout(),
            ) {
                return Ok(None);
            }
        }

        self.activate_with_running_policy(target_id, false)?;
        crate::process::launch_codex_app();

        let email = target_account.email.clone();
        let plan = target_account.plan_label.as_deref().unwrap_or("Free");
        let msg = format!("Auto-switched to {email} ({plan}): {switch_reason}.");
        notify_auto_switch(&msg);

        Ok(Some(target_id))
    }

    pub fn auto_start_usage_windows_once(
        &self,
        require_enabled: bool,
    ) -> Result<AutoStartUsageWindowsRunOutput> {
        let settings = load_settings(&self.env.app_data_dir)?;
        let enabled = settings.auto_start_usage_windows || settings.auto_switch_on_limit;
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

        if !due_accounts.is_empty() {
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
        }

        if let Err(error) = self.auto_switch_on_limit_once() {
            eprintln!("Auto-switch on limit check failed: {error:#}");
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
        let ping_result = run_codex_usage_ping(&self.env, &snapshot, &identity)?;
        let (current_metadata, current_snapshot) =
            self.repository.load_snapshot(&self.env.kind, account_id)?;
        if current_snapshot != snapshot {
            return Err(anyhow!(
                "saved snapshot changed while ping was running; skipped write-back"
            ));
        }
        let refreshed_identity = codex::identity_from_snapshot(&ping_result.snapshot)?;
        self.repository.replace_snapshot(
            &self.env.kind,
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

#[cfg(any(target_os = "windows", target_os = "macos"))]
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
                    let poll_seconds = compute_poll_interval(&env);
                    thread::sleep(StdDuration::from_secs(poll_seconds));
                }
            });
    });
}

fn switch_target_is_improvement(
    reason: &str,
    target: Option<&crate::model::AccountUsageView>,
    now: OffsetDateTime,
    near_limit_threshold: u8,
) -> bool {
    if reason == "login expired" {
        return true;
    }
    let Some(target) = target else {
        return false;
    };
    if target.is_fully_exhausted(now) || target.has_stale_quota_cache(now) {
        return false;
    }
    match reason {
        "rate limit detected" | "quota exhausted" => {
            !target.should_switch_account(now, near_limit_threshold)
        }
        _ => false,
    }
}

fn current_switch_reason(
    account: &crate::model::SavedAccountMetadata,
    now: OffsetDateTime,
) -> Option<&'static str> {
    if let Some(err) = account.cached_usage_error.as_deref() {
        if crate::usage::usage_error_indicates_rate_limit(err) {
            return Some("rate limit detected");
        }
        if crate::usage::usage_error_requires_login(err) {
            return Some("login expired");
        }
    }
    let usage = account.cached_usage.as_ref()?;
    if usage.is_out_of_quota(now) {
        return Some("quota exhausted");
    }
    None
}

fn auto_switch_should_quit_running_codex(reason: &str) -> bool {
    matches!(
        reason,
        "quota exhausted" | "rate limit detected" | "login expired"
    )
}

fn compute_poll_interval(env: &AppEnv) -> u64 {
    let settings = load_settings(&env.app_data_dir).unwrap_or_default();
    let enabled = settings.auto_start_usage_windows || settings.auto_switch_on_limit;
    if !enabled {
        return DEFAULT_USAGE_WINDOW_POLL_SECONDS;
    }

    let interval = if settings.auto_switch_on_limit {
        settings.auto_switch_poll_seconds.max(15)
    } else {
        DEFAULT_USAGE_WINDOW_POLL_SECONDS
    };

    if !settings.auto_switch_on_limit {
        return interval;
    }

    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        MigratingSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    let Ok(accounts) = repository.list_accounts(&env.kind) else {
        return interval;
    };
    let now = OffsetDateTime::now_utc();
    let threshold = settings.near_limit_threshold_percent;
    let mut urgent = false;
    let mut near_limit = false;

    for account in &accounts {
        if let Some(err) = account.cached_usage_error.as_deref()
            && crate::usage::usage_error_indicates_rate_limit(err)
        {
            urgent = true;
            break;
        }
        if let Some(usage) = account.cached_usage.as_ref() {
            if usage.is_out_of_quota(now) {
                urgent = true;
                break;
            }
            if usage.is_near_limit(now, threshold) {
                near_limit = true;
            }
        }
    }

    if urgent {
        URGENT_POLL_SECONDS
    } else if near_limit {
        interval.min(NEAR_LIMIT_POLL_SECONDS)
    } else {
        interval
    }
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

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn run_auto_start_usage_windows_check_now(env: AppEnv) -> Result<()> {
    run_auto_start_usage_windows_for_env(env)
}

fn run_auto_start_usage_windows_for_env(env: AppEnv) -> Result<()> {
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        MigratingSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    let app = App::new(env, repository);
    let _ = app.auto_start_usage_windows_once(true)?;
    Ok(())
}

fn auto_start_status_output(enabled: bool, poll_seconds: u64) -> AutoStartUsageWindowsStatusOutput {
    AutoStartUsageWindowsStatusOutput {
        enabled,
        poll_seconds,
    }
}

#[cfg(target_os = "macos")]
const MACOS_LAUNCH_AGENT_LABEL: &str = "com.anlvdt.codex-account-switcher";

#[cfg(target_os = "macos")]
const MACOS_LAUNCH_AGENT_WAIT_SECONDS: u32 = 60;

#[cfg(target_os = "macos")]
fn macos_launch_agent_plist_path(home_dir: &Path) -> PathBuf {
    home_dir
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{MACOS_LAUNCH_AGENT_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn macos_launcher_script_path(home_dir: &Path) -> PathBuf {
    home_dir
        .join(".local")
        .join("bin")
        .join("codex-account-switcher-launcher.sh")
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_domain() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to resolve user id for launchctl")?;
    if !output.status.success() {
        bail!("id -u failed");
    }
    let uid = String::from_utf8(output.stdout)
        .context("id -u returned invalid utf-8")?
        .trim()
        .to_owned();
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn macos_installed_menubar_binary_path(home_dir: &Path) -> PathBuf {
    home_dir
        .join(".local")
        .join("bin")
        .join("codex-account-switcher-menubar")
}

#[cfg(target_os = "macos")]
fn install_macos_menubar_binary(home_dir: &Path, exe: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let installed = macos_installed_menubar_binary_path(home_dir);
    if let Some(parent) = installed.parent() {
        fs::create_dir_all(parent)?;
    }
    let needs_copy = fs::read(exe)
        .ok()
        .zip(fs::read(&installed).ok())
        .is_none_or(|(source, existing)| source != existing);
    if needs_copy {
        fs::copy(exe, &installed).with_context(|| {
            format!(
                "failed to install menubar binary to {}",
                installed.display()
            )
        })?;
        let mut permissions = fs::metadata(&installed)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&installed, permissions)?;
    }
    Ok(installed)
}

#[cfg(target_os = "macos")]
fn macos_launcher_script_content(installed_exe: &Path, fallback_exe: &Path) -> String {
    format!(
        r#"#!/bin/bash
# Prefer the installed copy on the boot volume; fall back to the dev build path.
TARGET="{}"
FALLBACK="{}"
if [ -x "$TARGET" ]; then
    exec "$TARGET"
fi
for i in $(seq 1 {MACOS_LAUNCH_AGENT_WAIT_SECONDS}); do
    if [ -f "$FALLBACK" ]; then
        exec "$FALLBACK"
    fi
    sleep 1
done
exit 1
"#,
        installed_exe.display().to_string().replace('"', "\\\""),
        fallback_exe.display().to_string().replace('"', "\\\"")
    )
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_plist_content(program_path: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{MACOS_LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>"#,
        program_path.display()
    )
}

#[cfg(target_os = "macos")]
fn write_macos_launcher_script(home_dir: &Path, exe: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let installed_exe = install_macos_menubar_binary(home_dir, exe)?;
    let launcher_path = macos_launcher_script_path(home_dir);
    if let Some(parent) = launcher_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = macos_launcher_script_content(&installed_exe, exe);
    fs::write(&launcher_path, content)?;
    let mut permissions = fs::metadata(&launcher_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&launcher_path, permissions)?;
    Ok(launcher_path)
}

#[cfg(target_os = "macos")]
fn write_macos_launch_agent_plist(home_dir: &Path, program_path: &Path) -> Result<PathBuf> {
    let plist_path = macos_launch_agent_plist_path(home_dir);
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&plist_path, macos_launch_agent_plist_content(program_path))?;
    Ok(plist_path)
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_is_loaded() -> Result<bool> {
    let domain = macos_launch_agent_domain()?;
    let service = format!("{domain}/{MACOS_LAUNCH_AGENT_LABEL}");
    let output = Command::new("launchctl")
        .args(["print", &service])
        .output()
        .context("failed to run launchctl print")?;
    Ok(output.status.success())
}

#[cfg(target_os = "macos")]
fn ensure_macos_launch_agent_loaded(plist_path: &Path) -> Result<()> {
    if macos_launch_agent_is_loaded()? {
        return Ok(());
    }
    if !plist_path.exists() {
        return Ok(());
    }
    reload_macos_launch_agent(plist_path)
}

#[cfg(target_os = "macos")]
fn reload_macos_launch_agent(plist_path: &Path) -> Result<()> {
    let domain = macos_launch_agent_domain()?;
    let plist = plist_path
        .to_str()
        .context("launch agent plist path is not valid utf-8")?;
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, plist])
        .output();
    let output = Command::new("launchctl")
        .args(["bootstrap", &domain, plist])
        .output()
        .context("failed to run launchctl bootstrap")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("launchctl bootstrap failed: {stderr}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn unload_macos_launch_agent(plist_path: &Path) -> Result<()> {
    let domain = macos_launch_agent_domain()?;
    let plist = plist_path
        .to_str()
        .context("launch agent plist path is not valid utf-8")?;
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, plist])
        .output();
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos_launch_at_startup(home_dir: &Path, exe: &Path) -> Result<()> {
    let installed_exe = install_macos_menubar_binary(home_dir, exe)?;
    write_macos_launcher_script(home_dir, exe)?;
    let plist_path = write_macos_launch_agent_plist(home_dir, &installed_exe)?;
    reload_macos_launch_agent(&plist_path)
}

#[cfg(target_os = "macos")]
fn uninstall_macos_launch_at_startup(home_dir: &Path) -> Result<()> {
    let plist_path = macos_launch_agent_plist_path(home_dir);
    if plist_path.exists() {
        unload_macos_launch_agent(&plist_path)?;
        let _ = fs::remove_file(plist_path);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn update_macos_launch_at_startup_if_needed(home_dir: &Path, exe: &Path) -> Result<()> {
    let installed_exe = install_macos_menubar_binary(home_dir, exe)?;
    write_macos_launcher_script(home_dir, exe)?;
    let plist_path = macos_launch_agent_plist_path(home_dir);
    let plist_content = macos_launch_agent_plist_content(&installed_exe);
    let plist_changed = fs::read_to_string(&plist_path)
        .map(|content| content != plist_content)
        .unwrap_or(true);

    if plist_changed {
        write_macos_launch_agent_plist(home_dir, &installed_exe)?;
        reload_macos_launch_agent(&plist_path)?;
    } else {
        ensure_macos_launch_agent_loaded(&plist_path)?;
    }
    Ok(())
}

fn usage_window_needs_ping(reset_at: OffsetDateTime, now: OffsetDateTime) -> bool {
    reset_at <= now
}

fn notify_auto_switch(body: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display notification \"{}\" with title \"Codex Switcher\"",
                body.replace('"', "\\\"")
            ))
            .spawn();
    }
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

    #[test]
    fn switch_target_is_improvement_rejects_exhausted_target() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let current = crate::model::AccountUsageView {
            source: crate::model::UsageSource::SavedAccessToken,
            fetched_at: now,
            five_hour: Some(crate::model::UsageWindowView {
                used_percent: 100,
                remaining_percent: 0,
                reset_at: reset,
            }),
            weekly: None,
            credits: None,
        };
        let target = current.clone();
        assert!(!switch_target_is_improvement(
            "quota exhausted",
            Some(&target),
            now,
            5,
        ));
    }

    #[test]
    fn current_switch_reason_ignores_near_limit_quota() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let account = crate::model::SavedAccountMetadata {
            id: uuid::Uuid::new_v4(),
            environment: crate::model::EnvironmentKind::Macos,
            email: "active@example.com".to_owned(),
            subject: Some("sub-active".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            secret_key: "snapshot:test".to_owned(),
            created_at: now,
            updated_at: now,
            last_activated_at: None,
            cached_usage: Some(crate::model::AccountUsageView {
                source: crate::model::UsageSource::SavedAccessToken,
                fetched_at: now,
                five_hour: Some(crate::model::UsageWindowView {
                    used_percent: 97,
                    remaining_percent: 3,
                    reset_at: reset,
                }),
                weekly: None,
                credits: None,
            }),
            cached_usage_error: None,
            label: None,
            is_archived: false,
        };

        assert_eq!(current_switch_reason(&account, now), None);
    }

    #[test]
    fn auto_switch_quits_codex_for_urgent_reasons_only() {
        assert!(auto_switch_should_quit_running_codex("quota exhausted"));
        assert!(auto_switch_should_quit_running_codex("rate limit detected"));
        assert!(auto_switch_should_quit_running_codex("login expired"));
        assert!(!auto_switch_should_quit_running_codex("near limit"));
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
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

    #[test]
    fn auto_switch_on_limit_once_switches_when_out_of_quota() -> Result<()> {
        use crate::model::{DisplayIdentity, EnvironmentKind, SnapshotBlob};
        use base64::Engine;

        let temp = tempfile::tempdir()?;
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };
        std::fs::create_dir_all(&env.codex_root)?;
        std::fs::write(
            env.codex_root.join("auth.json"),
            crate::codex::auth_json_fixture("active@example.com", "sub-active", Some("pro")),
        )?;
        std::fs::write(env.codex_root.join("cap_sid"), "sid-active")?;

        let active_snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                        crate::codex::auth_json_fixture(
                            "active@example.com",
                            "sub-active",
                            Some("pro"),
                        ),
                    ),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-active"),
                },
            ],
        };

        let candidate_snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                        crate::codex::auth_json_fixture(
                            "candidate@example.com",
                            "sub-candidate",
                            Some("pro"),
                        ),
                    ),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-candidate"),
                },
            ],
        };

        let repo = SnapshotRepository::new(
            &env.app_data_dir,
            crate::secrets::test_support::MemorySecretStore::default(),
        );
        let saved_active = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "active@example.com".to_owned(),
                    subject: Some("sub-active".to_owned()),
                    name: None,
                    plan_label: Some("Pro".to_owned()),
                    workspace_id: None,
                    workspace_name: None,
                },
                &active_snapshot,
            )?
            .0;

        let saved_candidate = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "candidate@example.com".to_owned(),
                    subject: Some("sub-candidate".to_owned()),
                    name: None,
                    plan_label: Some("Pro".to_owned()),
                    workspace_id: None,
                    workspace_name: None,
                },
                &candidate_snapshot,
            )?
            .0;

        let now = OffsetDateTime::now_utc();
        repo.replace_snapshot(
            &env.kind,
            saved_active.id,
            &DisplayIdentity {
                email: "active@example.com".to_owned(),
                subject: Some("sub-active".to_owned()),
                name: None,
                plan_label: Some("Pro".to_owned()),
                workspace_id: None,
                workspace_name: None,
            },
            &active_snapshot,
            Some(crate::model::AccountUsageView {
                source: crate::model::UsageSource::SavedAccessToken,
                fetched_at: now,
                five_hour: Some(crate::model::UsageWindowView {
                    used_percent: 100,
                    remaining_percent: 0,
                    reset_at: now + Duration::hours(1),
                }),
                weekly: None,
                credits: None,
            }),
        )?;

        repo.replace_snapshot(
            &env.kind,
            saved_candidate.id,
            &DisplayIdentity {
                email: "candidate@example.com".to_owned(),
                subject: Some("sub-candidate".to_owned()),
                name: None,
                plan_label: Some("Pro".to_owned()),
                workspace_id: None,
                workspace_name: None,
            },
            &candidate_snapshot,
            Some(crate::model::AccountUsageView {
                source: crate::model::UsageSource::SavedAccessToken,
                fetched_at: now,
                five_hour: Some(crate::model::UsageWindowView {
                    used_percent: 0,
                    remaining_percent: 100,
                    reset_at: now + Duration::hours(1),
                }),
                weekly: None,
                credits: None,
            }),
        )?;

        let app = App::new(env.clone(), repo);

        let switched = app.auto_switch_on_limit_once()?;
        assert!(switched.is_none());
        let current = crate::codex::read_live_auth_bundle(&env)?;
        assert_eq!(current.identity.email, "active@example.com");

        app.set_auto_switch_on_limit(true)?;

        let codex_running = !crate::process::detect_running_codex_processes().is_empty();
        let switched = app.auto_switch_on_limit_once()?;
        if codex_running {
            assert!(
                switched.is_none(),
                "auto-switch should defer while Codex processes are running"
            );
        } else {
            assert_eq!(switched, Some(saved_candidate.id));
            let current = crate::codex::read_live_auth_bundle(&env)?;
            assert_eq!(current.identity.email, "candidate@example.com");
        }

        Ok(())
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn launch_at_startup_files_are_written_without_loading_launch_agent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home_dir = temp.path();
        let exe = std::env::current_exe()?;

        let installed = install_macos_menubar_binary(home_dir, &exe)?;
        let launcher_path = write_macos_launcher_script(home_dir, &exe)?;
        let plist_path = write_macos_launch_agent_plist(home_dir, &installed)?;

        assert_eq!(
            launcher_path,
            home_dir
                .join(".local")
                .join("bin")
                .join("codex-account-switcher-launcher.sh")
        );
        assert_eq!(
            installed,
            home_dir
                .join(".local")
                .join("bin")
                .join("codex-account-switcher-menubar")
        );
        assert_eq!(
            plist_path,
            home_dir
                .join("Library")
                .join("LaunchAgents")
                .join("com.anlvdt.codex-account-switcher.plist")
        );
        let launcher = fs::read_to_string(&launcher_path)?;
        assert!(launcher.contains("codex-account-switcher-menubar"));
        let plist = fs::read_to_string(&plist_path)?;
        assert!(plist.contains("codex-account-switcher-menubar"));
        assert!(!plist.contains("codex-account-switcher-launcher.sh"));
        assert!(
            home_dir
                .join(".local")
                .join("bin")
                .join("codex-account-switcher-menubar")
                .exists()
        );

        Ok(())
    }
}
