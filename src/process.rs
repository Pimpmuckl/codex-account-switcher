use sysinfo::{Pid, ProcessesToUpdate, System};

pub fn detect_running_codex_processes() -> Vec<String> {
    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let current_pid = std::process::id();
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            if pid.as_u32() == current_pid {
                return None;
            }
            let name = process.name().to_string_lossy().to_ascii_lowercase();
            let command = process
                .cmd()
                .iter()
                .map(|item| item.to_string_lossy().to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            if matches_codex_process(&name, &command) {
                Some(format_process(*pid, &name, &command))
            } else {
                None
            }
        })
        .collect()
}

fn matches_codex_process(name: &str, command: &str) -> bool {
    if matches!(name, "codex" | "codex.exe") {
        return true;
    }
    command
        .split_whitespace()
        .filter_map(|token| std::path::Path::new(token).file_name())
        .map(|token| token.to_string_lossy().to_ascii_lowercase())
        .any(|token| matches!(token.as_str(), "codex" | "codex.exe" | "codex.js"))
}

fn format_process(pid: Pid, name: &str, command: &str) -> String {
    if command.is_empty() {
        format!("{name} (pid {})", pid.as_u32())
    } else {
        format!("{name} (pid {}): {command}", pid.as_u32())
    }
}
