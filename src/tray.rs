use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use time::{OffsetDateTime, UtcOffset};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::app::App;
use crate::model::{AccountView, DisplayIdentity};
use crate::secrets::SecretStore;
use crate::usage::usage_error_requires_login;

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    AutoStartUsageWindowsChecked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TrayCommand {
    Activate(Uuid),
    Login(Uuid),
    SaveCurrent,
    Delete(Uuid, String),
    SetAutoStartUsageWindows(bool),
    SetAutoSwitchOnLimit(bool),
    SetLaunchAtStartup(bool),
    ShowTui,
    Refresh,
    Quit,
}



#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayExit {
    ShowTui,
    Quit,
}

struct TrayState<'a, S> {
    app: &'a App<S>,
    tray_icon: Option<TrayIcon>,
    commands: HashMap<String, TrayCommand>,
    event_proxy: EventLoopProxy<UserEvent>,
    exit: TrayExit,
}

pub(crate) fn run<S>(app: &App<S>) -> Result<TrayExit>
where
    S: SecretStore,
{
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create tray event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    spawn_auto_start_usage_windows_menu_refresh(proxy.clone());

    let mut state = TrayState {
        app,
        tray_icon: None,
        commands: HashMap::new(),
        event_proxy: proxy,
        exit: TrayExit::Quit,
    };
    event_loop
        .run_app(&mut state)
        .context("tray event loop failed")?;
    Ok(state.exit)
}

fn spawn_auto_start_usage_windows_menu_refresh(proxy: EventLoopProxy<UserEvent>) {
    let receiver = crate::app::subscribe_auto_start_usage_windows_checks();
    let _ = thread::Builder::new()
        .name("tray-auto-start-usage-windows-menu-refresh".to_owned())
        .spawn(move || {
            while receiver.recv().is_ok() {
                if proxy
                    .send_event(UserEvent::AutoStartUsageWindowsChecked)
                    .is_err()
                {
                    break;
                }
            }
        });
}

impl<S> ApplicationHandler<UserEvent> for TrayState<'_, S>
where
    S: SecretStore,
{
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray_icon.is_some() {
            return;
        }
        match self.app.status().and_then(|s| self.app.list().map(|l| (s, l))) {
            Ok((status, list)) => {
                let tooltip = self.get_tooltip_text(&status, &list);
                match self.rebuild_menu_with_status_and_list(&status, &list) {
                    Ok(menu) => {
                        let mut builder = TrayIconBuilder::new()
                            .with_tooltip(tooltip)
                            .with_icon(load_codex_icon())
                            .with_menu(Box::new(menu));
                        #[cfg(target_os = "macos")]
                        {
                            builder = builder.with_icon_as_template(true);
                        }
                        match builder.build() {
                            Ok(tray_icon) => self.tray_icon = Some(tray_icon),
                            Err(error) => eprintln!("failed to create tray icon: {error:#}"),
                        }
                    }
                    Err(error) => eprintln!("failed to build tray menu: {error:#}"),
                }
            }
            Err(error) => eprintln!("failed to query app status and list: {error:#}"),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::Menu(event) = event else {
            if let Err(error) = self.update_tray_menu() {
                eprintln!("failed to refresh tray menu: {error:#}");
            }
            return;
        };
        let command = self.commands.get(event.id.as_ref()).cloned();
        match command {
            Some(TrayCommand::Activate(account_id)) => {
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript").args(["-e", "quit app \"Codex\""]).output();
                    let _ = std::process::Command::new("pkill").args(["-x", "Codex"]).output();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("taskkill").args(["/f", "/im", "Codex.exe"]).output();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                let mut activated_info = None;
                match self.app.activate_with_running_policy(account_id, false) {
                    Ok(output) => {
                        let email = output.account.email.clone();
                        let plan = format_plan_label_simple(output.account.plan_label.as_deref());
                        activated_info = Some((email, plan));
                    }
                    Err(error) => {
                        eprintln!("failed to activate account from tray: {error:#}");
                    }
                }

                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("open").args(["-a", "Codex"]).spawn();
                    if let Some((email, plan)) = activated_info {
                        let msg = format!("Switched to account {email} ({plan}). Codex restarted.");
                        let _ = std::process::Command::new("osascript")
                            .arg("-e")
                            .arg(format!(
                                "display notification \"{}\" with title \"Codex Switcher\"",
                                msg.replace('"', "\\\"")
                            ))
                            .spawn();
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("cmd").args(["/c", "start", "", "Codex"]).spawn();
                }

                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::Login(account_id)) => {
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript").args(["-e", "quit app \"Codex\""]).output();
                    let _ = std::process::Command::new("pkill").args(["-x", "Codex"]).output();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("taskkill").args(["/f", "/im", "Codex.exe"]).output();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                if let Err(error) = self.app.activate_with_running_policy(account_id, false) {
                    eprintln!("failed to activate account from tray: {error:#}");
                }

                #[cfg(target_os = "macos")]
                {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new("osascript")
                            .arg("-e")
                            .arg(format!(
                                "tell application \"Terminal\" to do script \"codex login && '{}' save\"",
                                exe.display()
                            ))
                            .spawn();
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new("cmd")
                            .args(["/c", "start", "cmd", "/k", &format!("codex login && \"{}\" save", exe.display())])
                            .spawn();
                    }
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::SaveCurrent) => {
                let msg = match self.app.save_current() {
                    Ok(output) => {
                        format!(
                            "Saved account {} successfully.",
                            output.account.email
                        )
                    }
                    Err(error) => {
                        format!("Failed to save account: {error:#}")
                    }
                };
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!(
                            "display notification \"{}\" with title \"Codex Switcher\"",
                            msg.replace('"', "\\\"")
                        ))
                        .spawn();
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::Delete(account_id, email)) => {
                let confirmed = {
                    #[cfg(target_os = "macos")]
                    {
                        let script = format!(
                            "tell application \"System Events\" to display dialog \"Are you sure you want to delete {}?\" buttons {{\"Cancel\", \"Delete\"}} default button \"Cancel\" with icon caution",
                            email.replace('"', "\\\"")
                        );
                        let output = std::process::Command::new("osascript")
                            .args(["-e", &script])
                            .output();
                        if let Ok(out) = output {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            stdout.contains("button returned:Delete")
                        } else {
                            false
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        let script = format!(
                            "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show('Are you sure you want to delete {}?', 'Confirm Delete', 'YesNo') -eq 'Yes'",
                            email
                        );
                        let output = std::process::Command::new("powershell")
                            .args(["-Command", &script])
                            .output();
                        if let Ok(out) = output {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            stdout.trim().eq_ignore_ascii_case("True")
                        } else {
                            false
                        }
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    {
                        true
                    }
                };

                if confirmed {
                    match self.app.delete(account_id) {
                        Ok(_) => {
                            let msg = format!("Deleted account {} successfully.", email);
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("osascript")
                                    .arg("-e")
                                    .arg(format!(
                                        "display notification \"{}\" with title \"Codex Switcher\"",
                                        msg.replace('"', "\\\"")
                                    ))
                                    .spawn();
                            }
                        }
                        Err(error) => {
                            eprintln!("failed to delete account: {error:#}");
                        }
                    }
                    if let Err(error) = self.update_tray_menu() {
                        eprintln!("failed to refresh tray menu: {error:#}");
                    }
                }
            }
            Some(TrayCommand::SetAutoStartUsageWindows(enabled)) => {
                if let Err(error) = self.app.set_auto_start_usage_windows(enabled) {
                    eprintln!("failed to update auto-start usage windows from tray: {error:#}");
                } else if enabled {
                    let proxy = self.event_proxy.clone();
                    let env = self.app.env().clone();
                    let _ = thread::Builder::new()
                        .name("tray-auto-start-usage-windows".to_owned())
                        .spawn(move || {
                            if let Err(error) =
                                crate::app::run_auto_start_usage_windows_check_now(env)
                            {
                                  eprintln!(
                                      "failed to run auto-start usage window check from tray: {error:#}"
                                  );
                            }
                            let _ = proxy.send_event(UserEvent::AutoStartUsageWindowsChecked);
                        });
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::SetAutoSwitchOnLimit(enabled)) => {
                if let Err(error) = self.app.set_auto_switch_on_limit(enabled) {
                    eprintln!("failed to update auto-switch on limit from tray: {error:#}");
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::SetLaunchAtStartup(enabled)) => {
                if let Err(error) = self.app.set_launch_at_startup(enabled) {
                    eprintln!("failed to update launch at startup from tray: {error:#}");
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::ShowTui) => {
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    use std::io::IsTerminal;
                    if !std::io::stdin().is_terminal() {
                        #[cfg(target_os = "macos")]
                        {
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new("osascript")
                                    .arg("-e")
                                    .arg(format!(
                                        "tell application \"Terminal\" to do script \"'{}'\"",
                                        exe.display()
                                    ))
                                    .spawn();
                            }
                        }
                        #[cfg(target_os = "windows")]
                        {
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new("cmd")
                                    .args(["/c", "start", "cmd", "/k", &format!("\"{}\"", exe.display())])
                                    .spawn();
                            }
                        }
                        return;
                    }
                }
                self.exit = TrayExit::ShowTui;
                event_loop.exit();
            }
            Some(TrayCommand::Refresh) => {
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::Quit) => {
                self.exit = TrayExit::Quit;
                event_loop.exit();
            }
            None => {}
        }
    }
}

impl<S> TrayState<'_, S>
where
    S: SecretStore,
{
    fn update_tray_menu(&mut self) -> Result<()> {
        let status = self.app.status()?;
        let list = self.app.list()?;
        let tooltip = self.get_tooltip_text(&status, &list);
        let menu = self.rebuild_menu_with_status_and_list(&status, &list)?;
        if let Some(tray_icon) = &self.tray_icon {
            let _ = tray_icon.set_tooltip(Some(&tooltip));
            tray_icon.set_menu(Some(Box::new(menu)));
        }
        Ok(())
    }

    fn get_tooltip_text(
        &self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
    ) -> String {
        match &status.current_account {
            Some(account) => {
                let plan = format_plan_label_simple(account.plan_label.as_deref());
                let active_account = find_active_tray_account(
                    Some(account),
                    status.current_account_saved_id,
                    &list.accounts,
                );
                let usage_info = active_account.map(|act| {
                    let (remaining, _) = account_usage_labels_simple(act);
                    if remaining.is_empty() {
                        String::new()
                    } else {
                        format!(" - {remaining}")
                    }
                }).unwrap_or_default();
                format!("Codex: {}{} ({})", account.email, usage_info, plan)
            }
            None => "Codex: Not logged in".to_owned(),
        }
    }

    fn rebuild_menu_with_status_and_list(
        &mut self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
    ) -> Result<Menu> {
        let menu = Menu::new();
        self.commands.clear();

        let active_account = find_active_tray_account(
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &list.accounts,
        );
        let active_account_id = active_account.map(|account| account.id);
        let saved_accounts = tray_saved_accounts(&list.accounts, active_account_id);

        menu.append(&MenuItem::new("ACTIVE ACCOUNT", false, None))?;
        if let Some(current) = &status.current_account {
            // Line 1: Full Email
            menu.append(&MenuItem::new(format!("  {}", current.email), false, None))?;
            
            // Line 2: Details
            let plan = format_plan_label_simple(current.plan_label.as_deref());
            let (remaining, reset) = active_account.map(account_usage_labels_simple).unwrap_or_default();
            
            let needs_login = active_account.and_then(|a| a.usage_error.as_deref()).is_some_and(usage_error_requires_login);
            if needs_login {
                if let Some(act_acc) = active_account {
                    let login_id = "login_active".to_owned();
                    let item = MenuItem::with_id(
                        MenuId::new(&login_id),
                        format!("    {plan}  •  Click to Login"),
                        true,
                        None,
                    );
                    menu.append(&item)?;
                    self.commands.insert(login_id, TrayCommand::Login(act_acc.id));
                } else {
                    let details = format_details_line(plan, remaining, reset, active_account.is_none().then_some("[not saved]"));
                    menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
                }
            } else {
                let details = format_details_line(plan, remaining, reset, active_account.is_none().then_some("[not saved]"));
                menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
            }

            self.append_command(&menu, "save-current", "  Save Current Account", TrayCommand::SaveCurrent)?;
        } else {
            menu.append(&MenuItem::new("  not logged in", false, None))?;
        }
        menu.append(&PredefinedMenuItem::separator())?;

        menu.append(&MenuItem::new("SAVED ACCOUNTS", false, None))?;
        if saved_accounts.is_empty() {
            menu.append(&MenuItem::new("  No saved accounts", false, None))?;
        } else {
            for account in &saved_accounts {
                let id = format!("activate:{}", account.id);
                // Line 1: Full Email (clickable)
                let item = MenuItem::with_id(
                    MenuId::new(&id),
                    format!("  {}", account.email),
                    true,
                    None,
                );
                menu.append(&item)?;
                self.commands.insert(id, TrayCommand::Activate(account.id));
                
                // Line 2: Details (non-clickable unless needs login)
                let plan = format_plan_label_simple(account.plan_label.as_deref());
                let (remaining, reset) = account_usage_labels_simple(account);
                
                let needs_login = account.usage_error.as_deref().is_some_and(usage_error_requires_login);
                if needs_login {
                    let login_id = format!("login:{}", account.id);
                    let item = MenuItem::with_id(
                        MenuId::new(&login_id),
                        format!("    {plan}  •  Click to Login"),
                        true,
                        None,
                    );
                    menu.append(&item)?;
                    self.commands.insert(login_id, TrayCommand::Login(account.id));
                } else {
                    let details = format_details_line(plan, remaining, reset, None);
                    menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
                }
            }
            
            menu.append(&PredefinedMenuItem::separator())?;
            let delete_submenu = Submenu::new("  Delete Account", true);
            for account in &saved_accounts {
                let delete_id = format!("delete:{}", account.id);
                delete_submenu.append(&MenuItem::with_id(MenuId::new(&delete_id), &account.email, true, None))?;
                self.commands.insert(delete_id, TrayCommand::Delete(account.id, account.email.clone()));
            }
            menu.append(&delete_submenu)?;
        }

        menu.append(&PredefinedMenuItem::separator())?;
        let auto_start_enabled = self.app.auto_start_usage_windows_status()?.enabled;
        self.append_check_command(
            &menu,
            "toggle-auto-start-usage-windows",
            "Auto-start usage windows",
            auto_start_enabled,
            TrayCommand::SetAutoStartUsageWindows(!auto_start_enabled),
        )?;
        let auto_switch_enabled = self.app.auto_switch_on_limit_status()?.enabled;
        self.append_check_command(
            &menu,
            "toggle-auto-switch-on-limit",
            "Auto-switch on limit",
            auto_switch_enabled,
            TrayCommand::SetAutoSwitchOnLimit(!auto_switch_enabled),
        )?;
        let launch_at_startup_enabled = self.app.launch_at_startup_status()?.enabled;
        self.append_check_command(
            &menu,
            "toggle-launch-at-startup",
            "Launch at Login",
            launch_at_startup_enabled,
            TrayCommand::SetLaunchAtStartup(!launch_at_startup_enabled),
        )?;
        self.append_command(&menu, "show-tui", "Show TUI", TrayCommand::ShowTui)?;
        self.append_command(&menu, "refresh", "Refresh", TrayCommand::Refresh)?;
        self.append_command(&menu, "quit", "Quit", TrayCommand::Quit)?;
        Ok(menu)
    }

    fn append_command(
        &mut self,
        menu: &Menu,
        id: &str,
        label: &str,
        command: TrayCommand,
    ) -> Result<()> {
        menu.append(&MenuItem::with_id(MenuId::new(id), label, true, None))?;
        self.commands.insert(id.to_owned(), command);
        Ok(())
    }

    fn append_check_command(
        &mut self,
        menu: &Menu,
        id: &str,
        label: &str,
        checked: bool,
        command: TrayCommand,
    ) -> Result<()> {
        menu.append(&CheckMenuItem::with_id(
            MenuId::new(id),
            label,
            true,
            checked,
            None,
        ))?;
        self.commands.insert(id.to_owned(), command);
        Ok(())
    }
}

fn tray_saved_accounts(
    accounts: &[AccountView],
    active_account_id: Option<Uuid>,
) -> Vec<&AccountView> {
    accounts
        .iter()
        .filter(|account| Some(account.id) != active_account_id)
        .collect()
}

fn find_active_tray_account<'a>(
    current_account: Option<&DisplayIdentity>,
    current_saved_id: Option<Uuid>,
    accounts: &'a [AccountView],
) -> Option<&'a AccountView> {
    current_saved_id
        .and_then(|id| accounts.iter().find(|account| account.id == id))
        .or_else(|| {
            current_account.and_then(|current| {
                accounts
                    .iter()
                    .find(|account| account.is_active && account_matches_identity(account, current))
            })
        })
}

fn account_matches_identity(account: &AccountView, identity: &DisplayIdentity) -> bool {
    match (&account.subject, &identity.subject) {
        (Some(left), Some(right)) => left == right,
        _ => account.email.eq_ignore_ascii_case(&identity.email),
    }
}

fn format_plan_label_simple(plan: Option<&str>) -> String {
    plan.map(|p| {
        match p.to_ascii_lowercase().as_str() {
            "free" => "Free".to_owned(),
            "plus" => "Plus".to_owned(),
            "k12" => "K12".to_owned(),
            other => other.to_owned(),
        }
    })
    .unwrap_or_else(|| "Free".to_owned())
}

fn account_usage_labels_simple(account: &AccountView) -> (String, String) {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        (
            "Login required".to_owned(),
            String::new(),
        )
    } else if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            ("Quota passed".to_owned(), String::new())
        } else {
            (
                format!(
                    "{}% remaining",
                    format_remaining_percent(weekly.remaining_percent).trim()
                ),
                format!("Reset: {}", format_short_reset_at(weekly.reset_at)),
            )
        }
    } else if let Some(error) = &account.usage_error {
        if error.to_lowercase().contains("login required") {
            ("Login required".to_owned(), String::new())
        } else {
            ("Error".to_owned(), String::new())
        }
    } else {
        (String::new(), String::new())
    }
}

fn format_details_line(plan: String, remaining: String, reset: String, marker: Option<&str>) -> String {
    let mut parts = vec![plan];
    if !remaining.is_empty() {
        parts.push(remaining);
    }
    if !reset.is_empty() {
        parts.push(reset);
    }
    if let Some(m) = marker {
        parts.push(m.to_owned());
    }
    parts.join("  •  ")
}

fn format_short_reset_at(reset_at: OffsetDateTime) -> String {
    let local = UtcOffset::local_offset_at(reset_at)
        .map(|offset| reset_at.to_offset(offset))
        .unwrap_or(reset_at);
    format!("{:02}/{:02} {:02}:{:02}", local.month() as u8, local.day(), local.hour(), local.minute())
}

fn format_remaining_percent(percent: u8) -> String {
    format!("{percent:>3}").replace(' ', "\u{2007}")
}

fn load_codex_icon() -> Icon {
    if let Ok(icon) = decode_icon_bytes(include_bytes!("../assets/codex-account-switcher.ico")) {
        return icon;
    }
    candidate_icon_paths()
        .into_iter()
        .find_map(|path| decode_icon(&path).ok())
        .unwrap_or_else(fallback_icon)
}

pub(crate) fn hide_console_window() {
    #[cfg(target_os = "windows")]
    release_console();
}

pub(crate) fn show_console_window() {
    #[cfg(target_os = "windows")]
    allocate_console();
}

#[cfg(target_os = "windows")]
fn release_console() {
    use windows_sys::Win32::System::Console::FreeConsole;

    unsafe {
        FreeConsole();
    }
}

#[cfg(target_os = "windows")]
fn allocate_console() {
    use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindow};

    unsafe {
        AllocConsole();
    }
    let window = unsafe { GetConsoleWindow() };
    if !window.is_null() {
        unsafe {
            ShowWindow(window, SW_RESTORE);
        }
    }
}

fn candidate_icon_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("CODEX_ACCOUNT_SWITCHER_ICON") {
        paths.push(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        paths.push(dir.join("icon.ico"));
        paths.push(dir.join("icon.png"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            let windows_apps = PathBuf::from(program_files).join("WindowsApps");
            if let Ok(entries) = std::fs::read_dir(windows_apps) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    if file_name.starts_with("OpenAI.Codex_") {
                        paths.push(entry.path().join("app").join("resources").join("icon.ico"));
                        paths.push(entry.path().join("app").join("assets").join("icon.png"));
                    }
                }
            }
        }
    }
    paths
}

fn decode_icon(path: &Path) -> Result<Icon> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to open icon {}", path.display()))?;
    decode_icon_bytes(&bytes).with_context(|| format!("failed to decode icon {}", path.display()))
}

fn decode_icon_bytes(bytes: &[u8]) -> Result<Icon> {
    let image = image::load_from_memory(bytes)
        .context("failed to decode icon bytes")?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).context("failed to create tray icon")
}

fn fallback_icon() -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            let distance = dx * dx + dy * dy;
            let (r, g, b, a) = if distance < 196 && distance > 64 {
                (245, 245, 245, 255)
            } else if distance <= 64 {
                (18, 18, 18, 255)
            } else {
                (0, 0, 0, 0)
            };
            rgba.extend([r, g, b, a]);
        }
    }
    Icon::from_rgba(rgba, size, size).expect("fallback icon dimensions are valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EnvironmentKind;
    use time::OffsetDateTime;

    #[test]
    fn test_format_plan_label_simple() {
        assert_eq!(format_plan_label_simple(Some("pro")), "pro");
        assert_eq!(format_plan_label_simple(Some("Free")), "Free");
        assert_eq!(format_plan_label_simple(None), "Free");
    }

    #[test]
    fn test_format_details_line() {
        assert_eq!(
            format_details_line("Plus".to_owned(), "17% remaining".to_owned(), "Reset: 05/12 00:52".to_owned(), None),
            "Plus  •  17% remaining  •  Reset: 05/12 00:52"
        );
        assert_eq!(
            format_details_line("Free".to_owned(), String::new(), String::new(), Some("[not saved]")),
            "Free  •  [not saved]"
        );
    }

    #[test]
    fn remaining_percent_uses_fixed_width_visual_slot() {
        assert_eq!(format_remaining_percent(2), "\u{2007}\u{2007}2");
        assert_eq!(format_remaining_percent(89), "\u{2007}89");
        assert_eq!(format_remaining_percent(100), "100");
    }

    #[test]
    fn tray_saved_accounts_keeps_active_flag_without_rendered_active_id() {
        let active = AccountView {
            id: Uuid::new_v4(),
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            environment: EnvironmentKind::Windows,
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        let inactive = AccountView {
            id: Uuid::new_v4(),
            email: "inactive@example.com".to_owned(),
            is_active: false,
            ..active.clone()
        };
        let accounts = vec![active, inactive];

        let saved_accounts = tray_saved_accounts(&accounts, None);

        assert_eq!(saved_accounts.len(), 2);
        assert_eq!(saved_accounts[0].email, "active@example.com");
        assert_eq!(saved_accounts[1].email, "inactive@example.com");
    }

    #[test]
    fn active_account_fallback_requires_live_identity_match() {
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            environment: EnvironmentKind::Windows,
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        let matching_identity = DisplayIdentity {
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
        };
        let mismatched_identity = DisplayIdentity {
            email: "other@example.com".to_owned(),
            subject: Some("other-sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
        };
        let accounts = vec![account];

        assert!(find_active_tray_account(Some(&matching_identity), None, &accounts).is_some());
        assert!(find_active_tray_account(Some(&mismatched_identity), None, &accounts).is_none());
    }
}
