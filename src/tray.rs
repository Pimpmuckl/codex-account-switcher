use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use crate::app::App;
use crate::model::{AccountView, DisplayIdentity};
use crate::secrets::SecretStore;

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayCommand {
    Activate(Uuid),
    ShowTui,
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
    exit: TrayExit,
}

#[derive(Clone, Copy, Default)]
struct TrayLabelWidths {
    plan: usize,
    status: usize,
    usage: usize,
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
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let mut state = TrayState {
        app,
        tray_icon: None,
        commands: HashMap::new(),
        exit: TrayExit::Quit,
    };
    event_loop
        .run_app(&mut state)
        .context("tray event loop failed")?;
    Ok(state.exit)
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
        let UserEvent::Menu(event) = event;
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

        let active = status
            .current_account
            .as_ref()
            .map(active_account_label)
            .unwrap_or_else(|| "Active Account: not logged in".to_owned());
        menu.append(&MenuItem::new(active, false, None))?;
        menu.append(&PredefinedMenuItem::separator())?;

        if list.accounts.is_empty() {
            menu.append(&MenuItem::new("No saved accounts", false, None))?;
        } else {
            let widths = tray_label_widths(&list.accounts);
            for account in &list.accounts {
                let id = format!("activate:{}", account.id);
                let enabled = !account.is_active;
                let item = MenuItem::with_id(
                    MenuId::new(&id),
                    saved_account_label(account, widths),
                    enabled,
                    None,
                );
                menu.append(&item)?;
                if enabled {
                    self.commands.insert(id, TrayCommand::Activate(account.id));
                }
            }
        }

        menu.append(&PredefinedMenuItem::separator())?;
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
}

fn active_account_label(account: &DisplayIdentity) -> String {
    let mut label = format!("Active Account: {}", account.email);
    if let Some(plan) = &account.plan_label {
        label.push_str(&format!("\tPlan: {plan}"));
    }
    label
}

fn saved_account_label(account: &AccountView, widths: TrayLabelWidths) -> String {
    let plan = format!(
        "{:<width$}",
        account
            .plan_label
            .as_ref()
            .map(|plan| format!("Plan: {plan}"))
            .unwrap_or_default(),
        width = widths.plan
    );
    let status = format!(
        "{:<width$}",
        if account.is_active { "Active" } else { "" },
        width = widths.status
    );
    let usage = format!(
        "{:<width$}",
        account_usage_label(account),
        width = widths.usage
    );

    let details = [plan, status, usage]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ");
    if details.is_empty() {
        account.email.clone()
    } else {
        format!("{}\t{details}", account.email)
    }
}

fn tray_label_widths(accounts: &[AccountView]) -> TrayLabelWidths {
    let mut widths = TrayLabelWidths::default();
    for account in accounts {
        widths.plan = widths.plan.max(
            account
                .plan_label
                .as_ref()
                .map(|plan| format!("Plan: {plan}").len())
                .unwrap_or(0),
        );
        widths.status = widths
            .status
            .max(if account.is_active { "Active".len() } else { 0 });
        widths.usage = widths.usage.max(account_usage_label(account).len());
    }
    widths
}

fn account_usage_label(account: &AccountView) -> String {
    if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            "Weekly Remaining: passed".to_owned()
        } else {
            format!("Weekly Remaining: {}%", weekly.remaining_percent)
        }
    } else if account.usage_error.is_some() {
        "Usage unavailable".to_owned()
    } else {
        String::new()
    }
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
    use crate::model::EnvironmentKind;

    #[test]
    fn saved_account_label_marks_active_account() {
        let id = Uuid::new_v4();
        let account = AccountView {
            id,
            email: "person@example.com".to_owned(),
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
        let accounts = vec![account.clone()];

        assert_eq!(
            saved_account_label(&account, tray_label_widths(&accounts)),
            "person@example.com\tPlan: Pro  Active"
        );
    }

    #[test]
    fn active_account_label_includes_plan_when_present() {
        let account = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: None,
            name: None,
            plan_label: Some("Plus".to_owned()),
        };

        assert_eq!(
            active_account_label(&account),
            "Active Account: person@example.com\tPlan: Plus"
        );
    }

    #[test]
    fn saved_account_labels_pad_columns() {
        let first = AccountView {
            id: Uuid::new_v4(),
            email: "a@example.com".to_owned(),
            subject: None,
            name: None,
            plan_label: Some("Pro".to_owned()),
            environment: EnvironmentKind::Windows,
            is_active: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
        };
        let second = AccountView {
            email: "longer.person@example.com".to_owned(),
            plan_label: Some("Enterprise".to_owned()),
            is_active: true,
            ..first.clone()
        };
        let accounts = vec![first, second];
        let widths = tray_label_widths(&accounts);
        let first_label = saved_account_label(&accounts[0], widths);
        let second_label = saved_account_label(&accounts[1], widths);

        assert!(first_label.starts_with("a@example.com\t"));
        assert!(first_label.contains(&format!("{:<width$}", "Plan: Pro", width = widths.plan)));
        assert!(second_label.starts_with("longer.person@example.com\t"));
        assert!(second_label.contains("  Active"));
    }
}
