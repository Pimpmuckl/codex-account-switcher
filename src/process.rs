use std::sync::Mutex;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::model::RunningCodexProcess;

const SUMMARY_LIMIT: usize = 72;
const EXECUTABLE_WIDTH: usize = 12;
const ROLE_WIDTH: usize = 14;
/// Short TTL so tray title/tooltip/menu + dashboard polls share one OS process walk.
const PROCESS_SCAN_CACHE_TTL: Duration = Duration::from_millis(2500);

#[derive(Clone, Default)]
pub struct ProcessScanSnapshot {
    pub codex: Vec<RunningCodexProcess>,
    pub cursor: Vec<RunningCodexProcess>,
    pub claude: Vec<RunningCodexProcess>,
}

struct ProcessScanCache {
    at: Instant,
    snapshot: ProcessScanSnapshot,
}

static PROCESS_SCAN_CACHE: Mutex<Option<ProcessScanCache>> = Mutex::new(None);

/// One sysinfo walk classifying Codex / Cursor / Claude processes.
pub fn detect_all_processes() -> ProcessScanSnapshot {
    if let Ok(guard) = PROCESS_SCAN_CACHE.lock()
        && let Some(cache) = guard.as_ref()
        && cache.at.elapsed() < PROCESS_SCAN_CACHE_TTL
    {
        return cache.snapshot.clone();
    }
    let snapshot = scan_all_processes_uncached();
    if let Ok(mut guard) = PROCESS_SCAN_CACHE.lock() {
        *guard = Some(ProcessScanCache {
            at: Instant::now(),
            snapshot: snapshot.clone(),
        });
    }
    snapshot
}

fn scan_all_processes_uncached() -> ProcessScanSnapshot {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .without_tasks(),
    );
    let current_pid = std::process::id();
    let mut codex = Vec::new();
    let mut cursor = Vec::new();
    let mut claude = Vec::new();

    for (pid, process) in system.processes() {
        if pid.as_u32() == current_pid {
            continue;
        }
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        let command = process
            .cmd()
            .iter()
            .map(|item| item.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        if matches_codex_process(&name, &command) {
            codex.push(format_process(*pid, &name, &command));
            continue;
        }
        if name.contains("cursor")
            || command
                .iter()
                .any(|arg| arg.to_ascii_lowercase().contains("cursor"))
        {
            cursor.push(RunningCodexProcess {
                pid: pid.as_u32(),
                executable: name.clone(),
                role: "editor".to_owned(),
                summary: Some(command.join(" ")),
            });
            continue;
        }
        if name.contains("claude")
            || command
                .iter()
                .any(|arg| arg.to_ascii_lowercase().contains("claude"))
        {
            claude.push(RunningCodexProcess {
                pid: pid.as_u32(),
                executable: name,
                role: "cli-agent".to_owned(),
                summary: Some(command.join(" ")),
            });
        }
    }

    codex.sort_by(|left, right| {
        left.executable
            .cmp(&right.executable)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.pid.cmp(&right.pid))
    });
    cursor.sort_by_key(|p| p.pid);
    claude.sort_by_key(|p| p.pid);

    ProcessScanSnapshot {
        codex,
        cursor,
        claude,
    }
}

#[cfg(target_os = "macos")]
const MACOS_CHATGPT_CLI_PATH: &str = "/Applications/ChatGPT.app/Contents/Resources/codex";
#[cfg(target_os = "macos")]
const MACOS_CODEX_CLI_PATH: &str = "/Applications/Codex.app/Contents/Resources/codex";

/// Resolve the Codex CLI binary. GUI-launched processes often lack shell aliases on PATH.
pub fn codex_cli_path() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let chatgpt_path = std::path::PathBuf::from(MACOS_CHATGPT_CLI_PATH);
        if chatgpt_path.is_file() {
            return chatgpt_path;
        }
        let path = std::path::PathBuf::from(MACOS_CODEX_CLI_PATH);
        if path.is_file() {
            return path;
        }
    }
    std::path::PathBuf::from("codex")
}

pub fn detect_running_codex_processes() -> Vec<RunningCodexProcess> {
    detect_all_processes().codex
}

/// IDE/extension `codex app-server` processes do not hold live auth and should not
/// block account switching.
pub fn is_switch_blocking_process(process: &RunningCodexProcess) -> bool {
    process.role != "app-server"
}

pub fn detect_switch_blocking_codex_processes() -> Vec<RunningCodexProcess> {
    detect_running_codex_processes()
        .into_iter()
        .filter(is_switch_blocking_process)
        .collect()
}

pub fn detect_running_cursor_processes() -> Vec<RunningCodexProcess> {
    detect_all_processes().cursor
}

pub fn detect_switch_blocking_cursor_processes() -> Vec<RunningCodexProcess> {
    detect_running_cursor_processes()
}

pub fn detect_running_claude_processes() -> Vec<RunningCodexProcess> {
    detect_all_processes().claude
}

pub fn detect_switch_blocking_claude_processes() -> Vec<RunningCodexProcess> {
    detect_running_claude_processes()
}

pub fn format_process_table(processes: &[RunningCodexProcess]) -> Vec<String> {
    let mut lines = Vec::with_capacity(processes.len() + 1);
    lines.push(format!(
        "{:<6} {:<exe_width$} {:<role_width$} {}",
        "PID",
        "Exe",
        "Role",
        "Summary",
        exe_width = EXECUTABLE_WIDTH,
        role_width = ROLE_WIDTH
    ));
    for process in processes {
        lines.push(format!(
            "{:<6} {:<exe_width$} {:<role_width$} {}",
            process.pid,
            truncate_cell(&process.executable, EXECUTABLE_WIDTH),
            truncate_cell(&process.role, ROLE_WIDTH),
            process.summary.as_deref().unwrap_or("-"),
            exe_width = EXECUTABLE_WIDTH,
            role_width = ROLE_WIDTH
        ));
    }
    lines
}

fn matches_codex_process(name: &str, command: &[String]) -> bool {
    if matches!(name, "codex" | "codex.exe" | "chatgpt" | "chatgpt.exe") {
        return true;
    }
    command
        .iter()
        .filter_map(|token| path_file_name(token))
        .map(|token| token.to_ascii_lowercase())
        .any(|token| {
            matches!(
                token.as_str(),
                "codex" | "codex.exe" | "codex.js" | "chatgpt" | "chatgpt.exe" | "chatgpt.js"
            )
        })
}

fn format_process(pid: Pid, name: &str, command: &[String]) -> RunningCodexProcess {
    let (role, summary) = classify_process(command);
    RunningCodexProcess {
        pid: pid.as_u32(),
        executable: name.to_owned(),
        role,
        summary,
    }
}

fn classify_process(command: &[String]) -> (String, Option<String>) {
    if command.is_empty() {
        return ("process".to_owned(), None);
    }

    if let Some(wrapper) = classify_wrapped_codex_command(command) {
        return wrapper;
    }

    if let Some(role) = detect_flag_value(command, "--utility-sub-type=") {
        return ("utility".to_owned(), Some(truncate_summary(&role)));
    }
    if let Some(role) = detect_flag_value(command, "--type=") {
        return (role, summarize_args(command));
    }

    let mut positional = command
        .iter()
        .skip(1)
        .filter(|token| !token.is_empty())
        .peekable();
    if let Some(token) = positional.next()
        && !token.starts_with('-')
    {
        return (
            token.to_ascii_lowercase(),
            summarize_tokens(positional.cloned()),
        );
    }

    (
        "process".to_owned(),
        summarize_tokens(
            command
                .iter()
                .skip(1)
                .filter(|token| !token.starts_with('-'))
                .cloned(),
        ),
    )
}

fn classify_wrapped_codex_command(command: &[String]) -> Option<(String, Option<String>)> {
    let script = command.get(1)?;
    let script_name = path_file_name(script)?.to_ascii_lowercase();
    if script_name != "codex.js" {
        return None;
    }
    let mut positional = command
        .iter()
        .skip(2)
        .filter(|token| !token.is_empty())
        .peekable();
    let token = positional.next()?;
    if token.starts_with('-') {
        return Some(("process".to_owned(), summarize_tokens(positional.cloned())));
    }
    Some((
        token.to_ascii_lowercase(),
        summarize_tokens(positional.cloned()),
    ))
}

fn detect_flag_value(command: &[String], prefix: &str) -> Option<String> {
    command
        .iter()
        .find_map(|token| token.strip_prefix(prefix))
        .map(|value| value.to_ascii_lowercase())
}

fn summarize_args(command: &[String]) -> Option<String> {
    summarize_tokens(
        command
            .iter()
            .skip(1)
            .filter(|token| {
                !token.starts_with("--type=") && !token.starts_with("--utility-sub-type=")
            })
            .filter(|token| !token.starts_with("--user-data-dir="))
            .filter(|token| !token.starts_with("--field-trial-handle="))
            .filter(|token| !token.starts_with("--trace-process-track-uuid="))
            .filter(|token| !token.starts_with("--variations-seed-version"))
            .filter(|token| !token.starts_with("--mojo-platform-channel-handle="))
            .cloned(),
    )
}

fn summarize_tokens(tokens: impl Iterator<Item = String>) -> Option<String> {
    let filtered = tokens
        .filter_map(|token| clean_token(&token))
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        None
    } else {
        Some(truncate_summary(&filtered.join(" ")))
    }
}

fn clean_token(token: &str) -> Option<String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("--") {
        return flag_value_preview(trimmed);
    }
    if trimmed.starts_with("/prefetch:") {
        return None;
    }
    let value = path_file_name(trimmed).unwrap_or(trimmed).to_owned();
    if value.is_empty() { None } else { Some(value) }
}

fn path_file_name(value: &str) -> Option<&str> {
    value
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
}

fn flag_value_preview(token: &str) -> Option<String> {
    let (key, value) = token.split_once('=')?;
    match key {
        "--app-path" | "--title" | "--base" => clean_token(value),
        _ => None,
    }
}

fn truncate_summary(value: &str) -> String {
    let mut chars = value.chars();
    let preview = chars.by_ref().take(SUMMARY_LIMIT).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn truncate_cell(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let preview = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 1 {
        format!("{}...", preview.chars().take(width - 3).collect::<String>())
    } else {
        preview
    }
}

#[cfg(target_os = "macos")]
const MACOS_CHATGPT_MAIN_PROCESS_PATTERN: &str = "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT";
#[cfg(target_os = "macos")]
const MACOS_CODEX_MAIN_PROCESS_PATTERN: &str = "/Applications/Codex.app/Contents/MacOS/Codex";

/// Codex Desktop OAuth callback port (browser redirects here after auth.openai.com).
pub const CODEX_OAUTH_CALLBACK_PORT: u16 = 1455;

/// Gracefully stop the Codex desktop app before swapping auth snapshots.
pub fn quit_running_codex_app() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args(["-e", "quit app \"ChatGPT\""])
            .output();
        let _ = std::process::Command::new("osascript")
            .args(["-e", "quit app \"Codex\""])
            .output();
        let _ = std::process::Command::new("pkill")
            .args(["-f", MACOS_CHATGPT_MAIN_PROCESS_PATTERN])
            .output();
        let _ = std::process::Command::new("pkill")
            .args(["-f", MACOS_CODEX_MAIN_PROCESS_PATTERN])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/f", "/im", "ChatGPT.exe"])
            .output();
        let _ = std::process::Command::new("taskkill")
            .args(["/f", "/im", "Codex.exe"])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Quit Codex/ChatGPT and wait until main processes are gone (OAuth needs a clean restart).
pub fn quit_and_wait_for_codex_app() {
    quit_running_codex_app();
    let _ = wait_for_codex_processes_to_exit_timeout(std::time::Duration::from_secs(12));
    // Brief settle so the previous instance releases :1455 before relaunch.
    std::thread::sleep(std::time::Duration::from_millis(400));
}

/// True when something is accepting TCP connections on the Codex OAuth callback port.
pub fn codex_oauth_callback_listening() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], CODEX_OAUTH_CALLBACK_PORT)),
        std::time::Duration::from_millis(150),
    )
    .is_ok()
}

/// Wait until Codex Desktop binds `:1455` for the OAuth redirect (or timeout).
pub fn wait_for_codex_oauth_callback(timeout: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if codex_oauth_callback_listening() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    codex_oauth_callback_listening()
}

/// Wait until nothing is listening on the OAuth callback port.
pub fn wait_for_codex_oauth_port_free(timeout: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if !codex_oauth_callback_listening() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    !codex_oauth_callback_listening()
}

/// Best-effort free of `:1455` (Desktop/CLI leftovers hold this and blank OAuth pages).
pub fn free_codex_oauth_port() {
    quit_and_wait_for_codex_app();
    force_quit_switch_blocking_codex_processes();
    #[cfg(unix)]
    {
        // Kill any remaining listener on the OAuth callback port.
        if let Ok(output) = std::process::Command::new("lsof")
            .args([
                "-nP",
                &format!("-iTCP:{CODEX_OAUTH_CALLBACK_PORT}"),
                "-sTCP:LISTEN",
                "-t",
            ])
            .output()
        {
            let pids = String::from_utf8_lossy(&output.stdout);
            for pid in pids.split_whitespace() {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", pid])
                    .output();
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            for pid in pids.split_whitespace() {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", pid])
                    .output();
            }
        }
    }
    let _ = wait_for_codex_oauth_port_free(std::time::Duration::from_secs(5));
}

#[derive(Debug, Clone)]
pub struct InteractiveLoginLaunch {
    pub oauth_port_ready: bool,
    pub method: &'static str,
    pub detail: String,
}

/// Start interactive Codex login the reliable way:
/// 1) free `:1455`
/// 2) run `codex login` (binds OAuth server **then** opens the browser)
///
/// Opening ChatGPT Desktop with wiped `auth.json` uses a streamlined web flow that
/// often lands on a blank / stuck `auth.openai.com/oauth/authorize` page.
pub fn start_codex_cli_interactive_login(codex_home: &std::path::Path) -> InteractiveLoginLaunch {
    free_codex_oauth_port();

    let cli = codex_cli_path();
    let spawn = std::process::Command::new(&cli)
        .arg("login")
        .env("CODEX_HOME", codex_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match spawn {
        Ok(_child) => {
            // CLI owns the browser tab; wait until its callback server is up.
            let ready = wait_for_codex_oauth_callback(std::time::Duration::from_secs(20));
            InteractiveLoginLaunch {
                oauth_port_ready: ready,
                method: "codex-login",
                detail: if ready {
                    "Browser login started via `codex login`. Close any blank tabs and finish in the new browser window, then return here."
                        .to_owned()
                } else {
                    format!(
                        "Started `{} login`, but port {CODEX_OAUTH_CALLBACK_PORT} is not listening yet. Wait a few seconds; if the page is blank, close it and run Add account again.",
                        cli.display()
                    )
                },
            }
        }
        Err(error) => {
            // Last resort: Desktop app (historically flaky blank OAuth pages).
            launch_codex_app();
            let ready = wait_for_codex_oauth_callback(std::time::Duration::from_secs(12));
            InteractiveLoginLaunch {
                oauth_port_ready: ready,
                method: "desktop-fallback",
                detail: format!(
                    "Could not run `codex login` ({error}). Opened ChatGPT/Codex Desktop instead — if the browser page is blank, install/fix Codex CLI and retry."
                ),
            }
        }
    }
}

/// Prefers CLI login over Desktop relaunch. Pass the live Codex home (`~/.codex`).
pub fn relaunch_codex_for_interactive_login(codex_home: &std::path::Path) -> InteractiveLoginLaunch {
    start_codex_cli_interactive_login(codex_home)
}

/// Force-stop Codex processes that can hold live auth open during an urgent switch.
pub fn force_quit_switch_blocking_codex_processes() {
    let processes = detect_switch_blocking_codex_processes();
    if processes.is_empty() {
        return;
    }

    #[cfg(unix)]
    {
        for process in &processes {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &process.pid.to_string()])
                .output();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        for process in detect_switch_blocking_codex_processes() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &process.pid.to_string()])
                .output();
        }
    }

    #[cfg(windows)]
    {
        for process in &processes {
            let _ = std::process::Command::new("taskkill")
                .args(["/f", "/pid", &process.pid.to_string()])
                .output();
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(500));
}

pub fn force_quit_processes(processes: &[RunningCodexProcess]) {
    if processes.is_empty() {
        return;
    }
    #[cfg(unix)]
    {
        for process in processes {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &process.pid.to_string()])
                .output();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        for process in processes {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &process.pid.to_string()])
                .output();
        }
    }
    #[cfg(windows)]
    {
        for process in processes {
            let _ = std::process::Command::new("taskkill")
                .args(["/f", "/pid", &process.pid.to_string()])
                .output();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
}

pub fn force_quit_all_switch_blocking_processes() {
    let mut processes = detect_switch_blocking_codex_processes();
    processes.extend(detect_switch_blocking_cursor_processes());
    processes.extend(detect_switch_blocking_claude_processes());
    processes.sort_by_key(|w| w.pid);
    processes.dedup_by_key(|w| w.pid);
    force_quit_processes(&processes);
}

pub const SWITCH_WAIT_POLL_MS: u64 = 2_000;

/// Block until no switch-blocking Codex processes remain (polls every [`SWITCH_WAIT_POLL_MS`]).
pub fn wait_for_codex_processes_to_exit() {
    while !detect_switch_blocking_codex_processes().is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(SWITCH_WAIT_POLL_MS));
    }
}

/// Wait up to `timeout` for switch-blocking Codex processes to exit. Returns `true` when none remain.
pub fn wait_for_codex_processes_to_exit_timeout(timeout: std::time::Duration) -> bool {
    if timeout.is_zero() {
        return detect_switch_blocking_codex_processes().is_empty();
    }
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if detect_switch_blocking_codex_processes().is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(SWITCH_WAIT_POLL_MS));
    }
    detect_switch_blocking_codex_processes().is_empty()
}

/// Ask how to proceed when Codex is running. Defaults to waiting when the dialog fails.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn prompt_switch_when_running() -> crate::model::SwitchWhenRunning {
    use crate::model::SwitchWhenRunning;

    #[cfg(target_os = "macos")]
    {
        let script = r#"tell application "System Events" to display dialog "Codex is running with active tasks.

Switch now quits Codex and switches immediately.
Wait and switch defers until Codex finishes." buttons {"Cancel", "Wait and Switch", "Switch Now"} default button "Wait and Switch" with icon caution"#;
        let output = std::process::Command::new("osascript")
            .args(["-e", script])
            .output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains("button returned:Switch Now") {
                return SwitchWhenRunning::SwitchNow;
            }
            if stdout.contains("button returned:Wait and Switch") {
                return SwitchWhenRunning::WaitAndSwitch;
            }
            return SwitchWhenRunning::Cancel;
        }
        SwitchWhenRunning::WaitAndSwitch
    }

    #[cfg(target_os = "windows")]
    {
        let script = r#"Add-Type -AssemblyName Microsoft.VisualBasic
$result = [Microsoft.VisualBasic.Interaction]::MsgBox(
  'Codex is running with active tasks.' + [Environment]::NewLine + [Environment]::NewLine +
  'Yes = Switch now (quit Codex immediately)' + [Environment]::NewLine +
  'No = Wait and switch (defer until Codex finishes)' + [Environment]::NewLine +
  'Cancel = Cancel',
  [System.Windows.Forms.MessageBoxButtons]::YesNoCancel,
  'Codex Account Switcher'
)
switch ($result) {
  'Yes' { 'SwitchNow' }
  'No' { 'WaitAndSwitch' }
  default { 'Cancel' }
}"#;
        let output = std::process::Command::new("powershell")
            .args(["-Command", script])
            .output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            return match stdout.as_str() {
                "SwitchNow" => SwitchWhenRunning::SwitchNow,
                "WaitAndSwitch" => SwitchWhenRunning::WaitAndSwitch,
                _ => SwitchWhenRunning::Cancel,
            };
        }
        SwitchWhenRunning::WaitAndSwitch
    }
}

/// Relaunch the Codex/ChatGPT desktop app after auth has been restored.
///
/// Launches **one** preferred app only. Starting both ChatGPT and Codex races
/// two OAuth listeners on `localhost:1455` and freezes sign-in in the browser.
pub fn launch_codex_app() {
    #[cfg(target_os = "macos")]
    {
        // Prefer Codex Desktop when installed (matches originator=Codex Desktop OAuth).
        // Fall back to ChatGPT.app (bundled Codex) when Codex.app is absent.
        if std::path::Path::new("/Applications/Codex.app").exists() {
            let _ = std::process::Command::new("open")
                .args(["-a", "Codex"])
                .spawn();
        } else if std::path::Path::new("/Applications/ChatGPT.app").exists() {
            let _ = std::process::Command::new("open")
                .args(["-a", "ChatGPT"])
                .spawn();
        } else {
            let _ = std::process::Command::new("open")
                .args(["-a", "Codex"])
                .spawn();
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Single app only — starting both races two OAuth listeners on :1455.
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", "ChatGPT"])
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_process, clean_token, detect_flag_value, format_process_table,
        is_switch_blocking_process, matches_codex_process, truncate_summary,
    };
    use crate::model::RunningCodexProcess;

    #[test]
    fn app_server_processes_do_not_block_account_switch() {
        let app_server = RunningCodexProcess {
            pid: 1,
            executable: "codex".to_owned(),
            role: "app-server".to_owned(),
            summary: None,
        };
        let renderer = RunningCodexProcess {
            pid: 2,
            executable: "codex (renderer)".to_owned(),
            role: "renderer".to_owned(),
            summary: None,
        };
        assert!(!is_switch_blocking_process(&app_server));
        assert!(is_switch_blocking_process(&renderer));
    }

    #[test]
    fn matches_codex_process_detects_wrapped_cli() {
        assert!(matches_codex_process(
            "node.exe",
            &[
                "node".to_owned(),
                "C:\\Users\\tester\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js"
                    .to_owned(),
                "review".to_owned()
            ]
        ));
    }

    #[test]
    fn classify_process_prefers_known_process_type() {
        let (role, summary) = classify_process(&[
            "codex.exe".to_owned(),
            "--type=renderer".to_owned(),
            "--lang=en-gb".to_owned(),
            "--user-data-dir=C:\\Users\\tester\\AppData\\Roaming\\codex".to_owned(),
            "--app-path=C:\\Program Files\\Codex\\app.asar".to_owned(),
        ]);
        assert_eq!(role, "renderer");
        assert_eq!(summary.as_deref(), Some("app.asar"));
    }

    #[test]
    fn classify_process_uses_first_subcommand_for_cli_processes() {
        let (role, summary) = classify_process(&[
            "codex.exe".to_owned(),
            "review".to_owned(),
            "--title".to_owned(),
            "benchmark".to_owned(),
            "--base".to_owned(),
            "main".to_owned(),
        ]);
        assert_eq!(role, "review");
        assert_eq!(summary.as_deref(), Some("benchmark main"));
    }

    #[test]
    fn classify_process_uses_wrapped_codex_script_subcommand() {
        let (role, summary) = classify_process(&[
            "node.exe".to_owned(),
            "C:\\Users\\tester\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js"
                .to_owned(),
            "resume".to_owned(),
            "jjagentskills".to_owned(),
        ]);
        assert_eq!(role, "resume");
        assert_eq!(summary.as_deref(), Some("jjagentskills"));
    }

    #[test]
    fn clean_token_compacts_paths_and_skips_flags() {
        assert_eq!(
            clean_token("C:\\Program Files\\Codex\\codex.exe").as_deref(),
            Some("codex.exe")
        );
        assert_eq!(clean_token("--lang=en-gb"), None);
        assert_eq!(clean_token("/prefetch:11"), None);
    }

    #[test]
    fn detect_flag_value_reads_inline_flags() {
        assert_eq!(
            detect_flag_value(
                &["codex.exe".to_owned(), "--type=gpu-process".to_owned()],
                "--type="
            )
            .as_deref(),
            Some("gpu-process")
        );
    }

    #[test]
    fn truncate_summary_adds_ellipsis_for_long_values() {
        let long = "x".repeat(90);
        assert!(truncate_summary(&long).ends_with("..."));
    }

    #[test]
    fn format_process_table_renders_compact_rows() {
        let lines = format_process_table(&[RunningCodexProcess {
            pid: 12,
            executable: "codex.exe".to_owned(),
            role: "renderer".to_owned(),
            summary: Some("app.asar".to_owned()),
        }]);
        assert_eq!(lines[0], "PID    Exe          Role           Summary");
        assert!(lines[1].contains("12"));
        assert!(lines[1].contains("renderer"));
        assert!(lines[1].contains("app.asar"));
    }
}
