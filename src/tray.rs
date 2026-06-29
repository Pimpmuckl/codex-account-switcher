use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::app::App;
use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AUTO_REFRESH_QUOTA_ON_RESET_LABEL, AccountView, DisplayIdentity, QUOTA_PAST_RESET_LABEL,
    SwitchWhenRunning,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{MigratingSecretStore, SecretStore};
use crate::usage::usage_error_requires_login;

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    AutoStartUsageWindowsChecked,
    BackgroundTaskDone {
        notification: Option<(String, String)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TrayCommand {
    Activate(Uuid),
    Login(Uuid),
    SaveCurrent,
    StartAddAccount,
    FinishAddAccount,
    CancelAddAccount,
    PickBestQuota,
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
    #[cfg(target_os = "macos")]
    detach_from_controlling_terminal();

    let _instance_lock = match TrayInstanceLock::acquire(&tray_lock_path(app))? {
        Some(lock) => lock,
        None => {
            log_tray_message(
                app,
                &format!(
                    "tray already running (pid={}), exiting duplicate instance",
                    read_tray_lock_pid(&tray_lock_path(app)).unwrap_or(0)
                ),
            );
            return Ok(TrayExit::Quit);
        }
    };

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_activate_ignoring_other_apps(false)
            .with_default_menu(false);
    }
    let event_loop = builder
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
    let run_result = event_loop.run_app(&mut state);
    log_tray_message(
        app,
        &format!(
            "tray event loop finished (exit={:?}, pid={})",
            state.exit,
            std::process::id()
        ),
    );
    run_result.context("tray event loop failed")?;
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
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        // tray-icon must be created once the event loop is running (tauri-apps/tray-icon#90).
        if cause == StartCause::Init {
            self.ensure_tray_icon();
        }
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        self.ensure_tray_icon();
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::AutoStartUsageWindowsChecked | UserEvent::BackgroundTaskDone { .. } => {
                if let UserEvent::BackgroundTaskDone {
                    notification: Some((title, body)),
                } = &event
                {
                    tray_notify(title, body);
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            UserEvent::Menu(event) => self.handle_menu_event(event_loop, event),
        }
    }
}

impl<S> TrayState<'_, S>
where
    S: SecretStore,
{
    fn ensure_tray_icon(&mut self) {
        if self.tray_icon.is_some() {
            return;
        }
        match self
            .app
            .status()
            .and_then(|s| self.app.list().map(|l| (s, l)))
        {
            Ok((status, list)) => {
                let tooltip = self.get_tooltip_text(&status, &list);
                match self.rebuild_menu_with_status_and_list(&status, &list) {
                    Ok(menu) => {
                        let (icon, template) = load_tray_icon();
                        let mut builder = TrayIconBuilder::new()
                            .with_tooltip(tooltip)
                            .with_icon(icon)
                            .with_menu(Box::new(menu));
                        #[cfg(target_os = "macos")]
                        {
                            builder = builder.with_icon_as_template(template);
                        }
                        match builder.build() {
                            Ok(tray_icon) => {
                                self.tray_icon = Some(tray_icon);
                                self.spawn_startup_usage_refresh();
                                #[cfg(target_os = "macos")]
                                wake_main_run_loop();
                                log_tray_message(
                                    self.app,
                                    &format!(
                                        "tray icon created (template={template}, pid={})",
                                        std::process::id()
                                    ),
                                );
                            }
                            Err(error) => log_tray_error(
                                self.app,
                                &format!("failed to create tray icon: {error:#}"),
                            ),
                        }
                    }
                    Err(error) => {
                        log_tray_error(self.app, &format!("failed to build tray menu: {error:#}"))
                    }
                }
            }
            Err(error) => log_tray_error(
                self.app,
                &format!("failed to query app status and list: {error:#}"),
            ),
        }
    }

    fn handle_menu_event(&mut self, event_loop: &ActiveEventLoop, event: MenuEvent) {
        let command = self.commands.get(event.id.as_ref()).cloned();
        match command {
            Some(TrayCommand::Activate(account_id)) => {
                let was_running = !self.app.activation_preflight_warnings().is_empty();
                let policy = if was_running {
                    crate::process::prompt_switch_when_running()
                } else {
                    SwitchWhenRunning::WaitAndSwitch
                };
                if was_running && policy == SwitchWhenRunning::Cancel {
                    return;
                }
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    if was_running && policy == SwitchWhenRunning::SwitchNow {
                        crate::process::quit_running_codex_app();
                    } else if was_running {
                        crate::process::wait_for_codex_processes_to_exit();
                    }
                    let force = was_running && policy == SwitchWhenRunning::SwitchNow;
                    let output = app.activate_with_running_policy(account_id, force)?;
                    crate::process::launch_codex_app();
                    let email = output.account.email;
                    let plan = format_plan_label_simple(output.account.plan_label.as_deref());
                    let detail = if was_running && policy == SwitchWhenRunning::WaitAndSwitch {
                        " Codex restarted after tasks finished."
                    } else {
                        " Codex restarted."
                    };
                    Ok(Some((
                        "Codex Switcher".to_owned(),
                        format!("Switched to account {email} ({plan}).{detail}"),
                    )))
                });
            }
            Some(TrayCommand::Login(account_id)) => {
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    crate::process::quit_running_codex_app();
                    app.activate_with_running_policy(account_id, false)?;
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
                                .args([
                                    "/c",
                                    "start",
                                    "cmd",
                                    "/k",
                                    &format!("codex login && \"{}\" save", exe.display()),
                                ])
                                .spawn();
                        }
                    }
                    Ok(None)
                });
            }
            Some(TrayCommand::SaveCurrent) => {
                let msg = match self.app.save_current() {
                    Ok(output) => {
                        format!("Saved account {} successfully.", output.account.email)
                    }
                    Err(error) => {
                        format!("Failed to save account: {error:#}")
                    }
                };
                tray_notify("Codex Switcher", &msg);
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::StartAddAccount) => {
                match self.app.begin_add_account_session() {
                    Ok(()) => {
                        let proxy = self.event_proxy.clone();
                        spawn_tray_background(proxy, move || {
                            crate::process::quit_running_codex_app();
                            crate::process::launch_codex_app();
                            Ok(Some((
                                "Codex Switcher".to_owned(),
                                "Login in Codex with the new account, then choose Finish Adding Account."
                                    .to_owned(),
                            )))
                        });
                    }
                    Err(error) => {
                        tray_notify(
                            "Codex Switcher",
                            &format!("Failed to start add account: {error:#}"),
                        );
                    }
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
                }
            }
            Some(TrayCommand::FinishAddAccount) => {
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    let msg = match app.save_during_add_account_session() {
                        Ok(output) => {
                            crate::process::quit_running_codex_app();
                            if let Err(error) = codex::restore_add_account_backup(app.env()) {
                                format!(
                                    "Saved {} but failed to restore original login: {error:#}",
                                    output.account.email
                                )
                            } else {
                                crate::process::launch_codex_app();
                                format!("Added account {} successfully.", output.account.email)
                            }
                        }
                        Err(error) => format!("Failed to finish adding account: {error:#}"),
                    };
                    Ok(Some(("Codex Switcher".to_owned(), msg)))
                });
            }
            Some(TrayCommand::CancelAddAccount) => {
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    crate::process::quit_running_codex_app();
                    let msg = match app.cancel_add_account_session() {
                        Ok(()) => {
                            crate::process::launch_codex_app();
                            "Cancelled. Original account restored.".to_owned()
                        }
                        Err(error) => format!("Failed to cancel add account: {error:#}"),
                    };
                    Ok(Some(("Codex Switcher".to_owned(), msg)))
                });
            }
            Some(TrayCommand::PickBestQuota) => {
                let was_running = !self.app.activation_preflight_warnings().is_empty();
                let policy = if was_running {
                    crate::process::prompt_switch_when_running()
                } else {
                    SwitchWhenRunning::WaitAndSwitch
                };
                if was_running && policy == SwitchWhenRunning::Cancel {
                    return;
                }
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    if was_running && policy == SwitchWhenRunning::SwitchNow {
                        crate::process::quit_running_codex_app();
                    } else if was_running {
                        crate::process::wait_for_codex_processes_to_exit();
                    }
                    let msg = match app.pick_best_account(true, true) {
                        Ok(output) => {
                            if output.switched {
                                if was_running && policy == SwitchWhenRunning::WaitAndSwitch {
                                    format!(
                                        "Switched to best quota account {} after Codex finished.",
                                        output.account.email
                                    )
                                } else {
                                    format!(
                                        "Switched to best quota account {}.",
                                        output.account.email
                                    )
                                }
                            } else {
                                format!("Already on best quota account {}.", output.account.email)
                            }
                        }
                        Err(error) => format!("Pick best quota failed: {error:#}"),
                    };
                    if was_running && msg.starts_with("Switched to best") {
                        crate::process::launch_codex_app();
                    }
                    Ok(Some(("Codex Switcher".to_owned(), msg)))
                });
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
                } else if enabled {
                    let proxy = self.event_proxy.clone();
                    let env = self.app.env().clone();
                    let _ = thread::Builder::new()
                        .name("tray-auto-switch-on-limit".to_owned())
                        .spawn(move || {
                            if let Err(error) =
                                crate::app::run_auto_start_usage_windows_check_now(env)
                            {
                                eprintln!("failed to run auto-switch check from tray: {error:#}");
                            }
                            let _ = proxy.send_event(UserEvent::AutoStartUsageWindowsChecked);
                        });
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
                                    .args([
                                        "/c",
                                        "start",
                                        "cmd",
                                        "/k",
                                        &format!("\"{}\"", exe.display()),
                                    ])
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
                let usage_info = active_account
                    .map(|act| {
                        let (remaining, _) = account_usage_labels_simple(act);
                        if remaining.is_empty() {
                            String::new()
                        } else {
                            format!(" - {remaining}")
                        }
                    })
                    .unwrap_or_default();
                format!(
                    "Codex Account Switcher — {}{} ({})",
                    account.email, usage_info, plan
                )
            }
            None => "Codex Account Switcher — Not logged in".to_owned(),
        }
    }

    fn rebuild_menu_with_status_and_list(
        &mut self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
    ) -> Result<Menu> {
        if codex::add_account_session_active(self.app.env()) {
            return self.rebuild_add_account_pending_menu();
        }

        let menu = Menu::new();
        self.commands.clear();

        let active_account = find_active_tray_account(
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &list.accounts,
        );
        let active_account_id = active_account.map(|account| account.id);
        let saved_accounts = tray_saved_accounts(&list.accounts, active_account_id);

        menu.append(&MenuItem::new("Active Account", false, None))?;
        if let Some(current) = &status.current_account {
            // Line 1: Full Email
            menu.append(&MenuItem::new(format!("  {}", current.email), false, None))?;

            // Line 2: Details
            let plan = format_plan_label_simple(current.plan_label.as_deref());
            let (remaining, reset) = active_account
                .map(account_usage_labels_simple)
                .unwrap_or_default();

            let needs_login = active_account
                .and_then(|a| a.usage_error.as_deref())
                .is_some_and(usage_error_requires_login);
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
                    self.commands
                        .insert(login_id, TrayCommand::Login(act_acc.id));
                } else {
                    let details = format_details_line(
                        plan,
                        remaining,
                        reset,
                        active_account.is_none().then_some("[not saved]"),
                    );
                    menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
                }
            } else {
                let details = format_details_line(
                    plan,
                    remaining,
                    reset,
                    active_account.is_none().then_some("[not saved]"),
                );
                menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
            }

            self.append_command(
                &menu,
                "save-current",
                "  Save Current Account",
                TrayCommand::SaveCurrent,
            )?;
            self.append_command(
                &menu,
                "add-account",
                "  Add New Account…",
                TrayCommand::StartAddAccount,
            )?;
            self.append_command(
                &menu,
                "pick-best-quota",
                "  Switch to Best Quota",
                TrayCommand::PickBestQuota,
            )?;
        } else {
            menu.append(&MenuItem::new("  not logged in", false, None))?;
        }
        menu.append(&PredefinedMenuItem::separator())?;

        menu.append(&MenuItem::new("Saved Accounts", false, None))?;
        if saved_accounts.is_empty() {
            menu.append(&MenuItem::new("  No saved accounts", false, None))?;
        } else {
            for account in &saved_accounts {
                let id = format!("activate:{}", account.id);
                // Line 1: Full Email (clickable)
                let item =
                    MenuItem::with_id(MenuId::new(&id), format!("  {}", account.email), true, None);
                menu.append(&item)?;
                self.commands.insert(id, TrayCommand::Activate(account.id));

                // Line 2: Details (non-clickable unless needs login)
                let plan = format_plan_label_simple(account.plan_label.as_deref());
                let (remaining, reset) = account_usage_labels_simple(account);

                let needs_login = account
                    .usage_error
                    .as_deref()
                    .is_some_and(usage_error_requires_login);
                if needs_login {
                    let login_id = format!("login:{}", account.id);
                    let item = MenuItem::with_id(
                        MenuId::new(&login_id),
                        format!("    {plan}  •  Click to Login"),
                        true,
                        None,
                    );
                    menu.append(&item)?;
                    self.commands
                        .insert(login_id, TrayCommand::Login(account.id));
                } else {
                    let details = format_details_line(plan, remaining, reset, None);
                    menu.append(&MenuItem::new(format!("    {details}"), false, None))?;
                }
            }

            menu.append(&PredefinedMenuItem::separator())?;
            let delete_submenu = Submenu::new("  Delete Account", true);
            for account in &saved_accounts {
                let delete_id = format!("delete:{}", account.id);
                delete_submenu.append(&MenuItem::with_id(
                    MenuId::new(&delete_id),
                    &account.email,
                    true,
                    None,
                ))?;
                self.commands.insert(
                    delete_id,
                    TrayCommand::Delete(account.id, account.email.clone()),
                );
            }
            menu.append(&delete_submenu)?;
        }

        menu.append(&PredefinedMenuItem::separator())?;
        let auto_start_enabled = self.app.auto_start_usage_windows_status()?.enabled;
        self.append_check_command(
            &menu,
            "toggle-auto-start-usage-windows",
            AUTO_REFRESH_QUOTA_ON_RESET_LABEL,
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

    fn rebuild_add_account_pending_menu(&mut self) -> Result<Menu> {
        let menu = Menu::new();
        self.commands.clear();

        menu.append(&MenuItem::new("Adding New Account", false, None))?;
        menu.append(&MenuItem::new("  Waiting for Codex login…", false, None))?;
        menu.append(&PredefinedMenuItem::separator())?;
        self.append_command(
            &menu,
            "finish-add-account",
            "  Finish Adding Account",
            TrayCommand::FinishAddAccount,
        )?;
        self.append_command(
            &menu,
            "cancel-add-account",
            "  Cancel",
            TrayCommand::CancelAddAccount,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;
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

    fn spawn_startup_usage_refresh(&self) {
        let proxy = self.event_proxy.clone();
        let env = self.app.env().clone();
        let _ = thread::Builder::new()
            .name("tray-startup-usage-refresh".to_owned())
            .spawn(move || {
                let repository = SnapshotRepository::new(
                    &env.app_data_dir,
                    crate::secrets::MigratingSecretStore::new(&env.app_data_dir.join("snapshots")),
                );
                let app = App::new(env, repository);
                if let Err(error) = app.refresh_saved_usage_cache() {
                    eprintln!("failed to refresh usage cache on tray startup: {error:#}");
                }
                let _ = proxy.send_event(UserEvent::AutoStartUsageWindowsChecked);
            });
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
    plan.map(|p| match p.to_ascii_lowercase().as_str() {
        "free" => "Free".to_owned(),
        "plus" => "Plus".to_owned(),
        "k12" => "K12".to_owned(),
        other => other.to_owned(),
    })
    .unwrap_or_else(|| "Free".to_owned())
}

fn account_usage_labels_simple(account: &AccountView) -> (String, String) {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        ("Login required".to_owned(), String::new())
    } else if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            (QUOTA_PAST_RESET_LABEL.to_owned(), String::new())
        } else {
            (
                format!(
                    "{}% remaining",
                    format_remaining_percent(weekly.remaining_percent).trim()
                ),
                format!(
                    "Reset: {}",
                    crate::time_display::format_short_local_reset_at(weekly.reset_at)
                ),
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

fn format_details_line(
    plan: String,
    remaining: String,
    reset: String,
    marker: Option<&str>,
) -> String {
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

fn format_remaining_percent(percent: u8) -> String {
    format!("{percent:>3}").replace(' ', "\u{2007}")
}

fn tray_app_for_env(env: &AppEnv) -> App<impl SecretStore> {
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        MigratingSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    App::new(env.clone(), repository)
}

fn spawn_tray_background(
    proxy: EventLoopProxy<UserEvent>,
    job: impl FnOnce() -> Result<Option<(String, String)>> + Send + 'static,
) {
    thread::spawn(move || {
        let notification = match job() {
            Ok(notification) => notification,
            Err(error) => {
                eprintln!("background tray task failed: {error:#}");
                Some((
                    "Codex Switcher".to_owned(),
                    format!("Operation failed: {error:#}"),
                ))
            }
        };
        let _ = proxy.send_event(UserEvent::BackgroundTaskDone { notification });
    });
}

fn tray_notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display notification \"{}\" with title \"{}\"",
                body.replace('"', "\\\""),
                title.replace('"', "\\\"")
            ))
            .spawn();
    }
}

fn load_tray_icon() -> (Icon, bool) {
    #[cfg(target_os = "macos")]
    {
        for bytes in macos_menubar_icon_candidates() {
            if let Ok(icon) = decode_menubar_icon_bytes(bytes) {
                return (icon, true);
            }
        }
    }
    (load_codex_icon(), false)
}

fn load_codex_icon() -> Icon {
    for bytes in embedded_icon_candidates() {
        if let Ok(icon) = decode_icon_bytes(bytes) {
            return icon;
        }
    }
    candidate_icon_paths()
        .into_iter()
        .find_map(|path| decode_icon(&path).ok())
        .unwrap_or_else(fallback_icon)
}

fn embedded_icon_candidates() -> &'static [&'static [u8]] {
    &[
        include_bytes!("../assets/codex-account-switcher.ico"),
        include_bytes!("../assets/codex-account-switcher-dock.png"),
        include_bytes!("../assets/codex-account-switcher-transparent.png"),
    ]
}

#[cfg(target_os = "macos")]
fn macos_menubar_icon_candidates() -> &'static [&'static [u8]] {
    &[
        include_bytes!("../assets/codex-account-switcher-transparent.png"),
        include_bytes!("../assets/codex-account-switcher-dock.png"),
    ]
}

#[cfg(target_os = "macos")]
const MENUBAR_ICON_SIZE: u32 = 22;

#[cfg(target_os = "macos")]
fn decode_menubar_icon_bytes(bytes: &[u8]) -> Result<Icon> {
    let image = image::load_from_memory(bytes)
        .context("failed to decode menubar icon bytes")?
        .into_rgba8();
    let resized = image::imageops::resize(
        &image,
        MENUBAR_ICON_SIZE,
        MENUBAR_ICON_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    let (width, height) = resized.dimensions();
    Icon::from_rgba(resized.into_raw(), width, height).context("failed to create menubar tray icon")
}

pub(crate) fn hide_console_window() {
    #[cfg(target_os = "windows")]
    release_console();
}

#[cfg(target_os = "macos")]
fn wake_main_run_loop() {
    // tray-icon may not paint until the main run loop is nudged (winit #3835).
    unsafe {
        use core_foundation::runloop::{CFRunLoopGetMain, CFRunLoopWakeUp};
        CFRunLoopWakeUp(CFRunLoopGetMain());
    }
}

#[cfg(target_os = "macos")]
fn detach_from_controlling_terminal() {
    // Detach from the launching terminal session so SIGHUP cannot stop the agent.
    // SAFETY: setsid and SIG_IGN are async-signal-safe POSIX calls.
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

struct TrayInstanceLock {
    #[cfg(unix)]
    file: fs::File,
}

impl TrayInstanceLock {
    fn acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| format!("failed to open tray lock {}", path.display()))?;
            let fd = file.as_raw_fd();
            let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
            if !locked {
                return Ok(None);
            }
            file.set_len(0)?;
            writeln!(file, "{}", std::process::id())?;
            Ok(Some(Self { file }))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Some(Self {}))
        }
    }
}

fn tray_lock_path<S: SecretStore>(app: &App<S>) -> PathBuf {
    app.env().app_data_dir.join("tray.lock")
}

fn read_tray_lock_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse().ok())
}

#[cfg(target_os = "macos")]
pub(crate) fn tray_instance_pid() -> Option<u32> {
    directories::ProjectDirs::from("com", "nextide", "codex-account-switcher")
        .map(|dirs| dirs.data_local_dir().join("tray.lock"))
        .and_then(|path| read_tray_lock_pid(&path))
        .filter(|pid| unsafe { libc::kill(*pid as i32, 0) == 0 })
}

fn log_tray_error<S: SecretStore>(app: &App<S>, message: &str) {
    eprintln!("{message}");
    log_tray_message(app, message);
}

fn log_tray_message<S: SecretStore>(app: &App<S>, message: &str) {
    let log_path = tray_log_path(app);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown-time".to_owned());
        let _ = writeln!(file, "[{timestamp}] {message}");
    }
}

fn tray_log_path<S: SecretStore>(app: &App<S>) -> PathBuf {
    app.env().app_data_dir.join("tray.log")
}

#[cfg(target_os = "macos")]
fn default_tray_log_path() -> PathBuf {
    directories::ProjectDirs::from("com", "nextide", "codex-account-switcher")
        .map(|dirs| dirs.data_local_dir().join("tray.log"))
        .unwrap_or_else(|| PathBuf::from("tray.log"))
}

/// Start a detached tray instance and return `true` when the current process should exit.
#[cfg(target_os = "macos")]
pub(crate) fn spawn_detached_tray_instance() -> Result<bool> {
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    if tray_instance_pid().is_some() {
        return Ok(false);
    }
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let log_path = default_tray_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("failed to open tray log for detached tray instance")?;
    Command::new("nohup")
        .arg(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("failed to spawn detached tray instance")?;
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn spawn_detached_tray_instance() -> Result<bool> {
    Ok(false)
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
            format_details_line(
                "Plus".to_owned(),
                "17% remaining".to_owned(),
                "Reset: 12/05 00:52".to_owned(),
                None
            ),
            "Plus  •  17% remaining  •  Reset: 12/05 00:52"
        );
        assert_eq!(
            format_details_line(
                "Free".to_owned(),
                String::new(),
                String::new(),
                Some("[not saved]")
            ),
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
            label: None,
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
            label: None,
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
