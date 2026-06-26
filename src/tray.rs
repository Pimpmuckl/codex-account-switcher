use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::app::App;
use crate::model::{AccountView, DisplayIdentity};
use crate::secrets::SecretStore;
use crate::time_display::format_local_reset_at;
use crate::usage::{usage_error_label, usage_error_requires_login};

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    AutoStartUsageWindowsChecked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayCommand {
    Activate(Uuid),
    SetAutoStartUsageWindows(bool),
    ShowTui,
    Quit,
}

#[derive(Clone, Copy, Default)]
struct TrayLabelWidths {
    plan: usize,
    remaining: usize,
    reset: usize,
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
        match self.rebuild_menu() {
            Ok(menu) => match TrayIconBuilder::new()
                .with_tooltip("Codex account switcher")
                .with_icon(load_codex_icon())
                .with_menu(Box::new(menu))
                .build()
            {
                Ok(tray_icon) => self.tray_icon = Some(tray_icon),
                Err(error) => eprintln!("failed to create tray icon: {error:#}"),
            },
            Err(error) => eprintln!("failed to build tray menu: {error:#}"),
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
        let command = self.commands.get(event.id.as_ref()).copied();
        match command {
            Some(TrayCommand::Activate(account_id)) => {
                if let Err(error) = self.app.activate_with_running_policy(account_id, false) {
                    eprintln!("failed to activate account from tray: {error:#}");
                }
                if let Err(error) = self.update_tray_menu() {
                    eprintln!("failed to refresh tray menu: {error:#}");
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
            Some(TrayCommand::ShowTui) => {
                self.exit = TrayExit::ShowTui;
                event_loop.exit();
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
        let menu = self.rebuild_menu()?;
        if let Some(tray_icon) = &self.tray_icon {
            tray_icon.set_menu(Some(Box::new(menu)));
        }
        Ok(())
    }

    fn rebuild_menu(&mut self) -> Result<Menu> {
        let status = self.app.status()?;
        let list = self.app.list()?;
        let menu = Menu::new();
        self.commands.clear();

        let active_account = find_active_tray_account(
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &list.accounts,
        );
        let active_account_id = active_account.map(|account| account.id);
        let saved_accounts = tray_saved_accounts(&list.accounts, active_account_id);
        let label_accounts = active_account
            .into_iter()
            .chain(saved_accounts.iter().copied())
            .collect::<Vec<_>>();
        let mut widths = tray_label_widths(&label_accounts);
        if let Some(current) = &status.current_account {
            widths.include_plan(current.plan_label.as_deref());
        }

        menu.append(&MenuItem::new("Active Account", false, None))?;
        let active_label = status
            .current_account
            .as_ref()
            .map(|account| active_account_label(account, active_account, widths))
            .unwrap_or_else(|| "not logged in".to_owned());
        menu.append(&MenuItem::new(format!("  {active_label}"), false, None))?;
        menu.append(&PredefinedMenuItem::separator())?;

        menu.append(&MenuItem::new("Saved Accounts", false, None))?;
        if saved_accounts.is_empty() {
            menu.append(&MenuItem::new("No saved accounts", false, None))?;
        } else {
            for account in saved_accounts {
                let id = format!("activate:{}", account.id);
                let item = MenuItem::with_id(
                    MenuId::new(&id),
                    format!("  {}", tray_account_label(account, widths)),
                    true,
                    None,
                );
                menu.append(&item)?;
                self.commands.insert(id, TrayCommand::Activate(account.id));
            }
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
        self.append_command(&menu, "show-tui", "Show TUI", TrayCommand::ShowTui)?;
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

fn active_account_label(
    account: &DisplayIdentity,
    saved_account: Option<&AccountView>,
    widths: TrayLabelWidths,
) -> String {
    let plan = format_plan_label(account.plan_label.as_deref(), widths);
    let (remaining, reset) = saved_account.map(account_usage_labels).unwrap_or_default();
    let remaining = format!("{:<width$}", remaining, width = widths.remaining);
    let reset = format!("{:<width$}", reset, width = widths.reset);
    let saved_marker = saved_account.is_none().then_some("[not saved]");
    tray_row_label(&account.email, [plan, remaining, reset], saved_marker)
}

fn tray_account_label(account: &AccountView, widths: TrayLabelWidths) -> String {
    let plan = format_plan_label(account.plan_label.as_deref(), widths);
    let (remaining, reset) = account_usage_labels(account);
    let remaining = format!("{:<width$}", remaining, width = widths.remaining);
    let reset = format!("{:<width$}", reset, width = widths.reset);

    tray_row_label(&account.email, [plan, remaining, reset], None)
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
    account.account_key == identity.account_key
}

fn tray_row_label<const N: usize>(
    email: &str,
    details: [String; N],
    marker: Option<&str>,
) -> String {
    let details = details
        .into_iter()
        .chain(marker.map(str::to_owned))
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ");
    if details.is_empty() {
        email.to_owned()
    } else {
        format!(
            "{}{separator}{details}",
            email,
            separator = tray_detail_separator()
        )
    }
}

fn tray_detail_separator() -> &'static str {
    if cfg!(windows) { "\t" } else { "  " }
}

fn format_plan_label(plan: Option<&str>, widths: TrayLabelWidths) -> String {
    format!(
        "{:<width$}",
        plan.map(|plan| format!("Plan: {plan}")).unwrap_or_default(),
        width = widths.plan
    )
}

fn tray_label_widths(accounts: &[&AccountView]) -> TrayLabelWidths {
    let mut widths = TrayLabelWidths::default();
    for account in accounts {
        widths.include_plan(account.plan_label.as_deref());
        let (remaining, reset) = account_usage_labels(account);
        widths.remaining = widths.remaining.max(visible_width(&remaining));
        widths.reset = widths.reset.max(visible_width(&reset));
    }
    widths
}

impl TrayLabelWidths {
    fn include_plan(&mut self, plan: Option<&str>) {
        self.plan = self.plan.max(
            plan.map(|plan| visible_width(&format!("Plan: {plan}")))
                .unwrap_or_default(),
        );
    }
}

fn account_usage_labels(account: &AccountView) -> (String, String) {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        (
            usage_error_label(account.usage_error.as_deref().unwrap_or_default()).to_owned(),
            String::new(),
        )
    } else if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            ("Weekly Remaining: passed".to_owned(), String::new())
        } else {
            (
                format!(
                    "Weekly Remaining: {}%",
                    format_remaining_percent(weekly.remaining_percent)
                ),
                format!("Reset: {}", format_local_reset_at(weekly.reset_at)),
            )
        }
    } else if let Some(error) = &account.usage_error {
        (usage_error_label(error).to_owned(), String::new())
    } else {
        (String::new(), String::new())
    }
}

fn format_remaining_percent(percent: u8) -> String {
    format!("{percent:>3}").replace(' ', "\u{2007}")
}

fn visible_width(text: &str) -> usize {
    text.chars().count()
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
    release_console();
}

pub(crate) fn show_console_window() {
    allocate_console();
}

fn release_console() {
    use windows_sys::Win32::System::Console::FreeConsole;

    unsafe {
        FreeConsole();
    }
}

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
    use crate::model::{AccountUsageView, UsageSource, UsageWindowView};
    use time::{Date, Month, OffsetDateTime, Time};

    #[test]
    fn tray_account_label_includes_usage_table_columns() {
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
            .replace_time(Time::from_hms(0, 52, 0).unwrap());
        let mut account = AccountView {
            id: Uuid::new_v4(),
            email: "person@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        account.usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 83,
                remaining_percent: 17,
                reset_at,
            }),
            credits: None,
        });

        assert_eq!(
            tray_account_label(&account, tray_label_widths(&[&account])),
            format!(
                "person@example.com{}Plan: Pro  Weekly Remaining: \u{2007}17%  Reset: {}",
                tray_detail_separator(),
                format_local_reset_at(reset_at)
            )
        );
    }

    #[test]
    fn tray_account_label_marks_login_required_usage_error() {
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "person@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
            is_active: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: Some(UsageWindowView {
                    used_percent: 0,
                    remaining_percent: 100,
                    reset_at: OffsetDateTime::UNIX_EPOCH
                        .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
                        .replace_time(Time::from_hms(13, 56, 0).unwrap()),
                }),
                credits: None,
            }),
            usage_error: Some("Login required: Codex auth expired.".to_owned()),
        };

        let label = tray_account_label(&account, tray_label_widths(&[&account]));

        assert!(label.contains("Login required"));
        assert!(!label.contains("Usage unavailable"));
        assert!(!label.contains("Weekly Remaining"));
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
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
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
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        let matching_identity = DisplayIdentity {
            email: "active@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
        };
        let mismatched_identity = DisplayIdentity {
            email: "other@example.com".to_owned(),
            account_key: "other-sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
        };
        let accounts = vec![account];

        assert!(find_active_tray_account(Some(&matching_identity), None, &accounts).is_some());
        assert!(find_active_tray_account(Some(&mismatched_identity), None, &accounts).is_none());
    }

    #[test]
    fn active_account_label_keeps_live_identity_and_saved_usage() {
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
            .replace_time(Time::from_hms(0, 52, 0).unwrap());
        let mut saved_account = AccountView {
            id: Uuid::new_v4(),
            email: "stale@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Pro".to_owned()),
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        saved_account.usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 83,
                remaining_percent: 17,
                reset_at,
            }),
            credits: None,
        });
        let account = DisplayIdentity {
            email: "person@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("ProLite".to_owned()),
        };
        let mut widths = tray_label_widths(&[&saved_account]);
        widths.include_plan(account.plan_label.as_deref());

        assert_eq!(
            active_account_label(&account, Some(&saved_account), widths),
            format!(
                "person@example.com{}Plan: ProLite  Weekly Remaining: \u{2007}17%  Reset: {}",
                tray_detail_separator(),
                format_local_reset_at(reset_at)
            )
        );
    }

    #[test]
    fn active_account_label_marks_unsaved_current_account() {
        let account = DisplayIdentity {
            email: "person@example.com".to_owned(),
            account_key: "sub".to_owned(),
            name: None,
            plan_label: Some("Plus".to_owned()),
        };

        assert_eq!(
            active_account_label(&account, None, TrayLabelWidths::default()),
            format!(
                "person@example.com{}Plan: Plus  [not saved]",
                tray_detail_separator()
            )
        );
    }
}
