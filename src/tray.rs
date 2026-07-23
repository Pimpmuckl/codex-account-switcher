use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{WindowId, WindowLevel};

use crate::app::App;
use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AUTO_REFRESH_QUOTA_ON_RESET_LABEL, AccountView, DisplayIdentity, QUOTA_PAST_RESET_LABEL,
    SwitchWhenRunning,
};
use crate::repository::SnapshotRepository;
use crate::secrets::{MigratingSecretStore, SecretStore};
use crate::settings::{ResolvedUiLanguage, load_settings, resolve_ui_language};
use crate::usage::usage_error_requires_login;

fn tray_ui_lang<S: SecretStore>(app: &App<S>) -> ResolvedUiLanguage {
    load_settings(&app.env().app_data_dir)
        .map(|settings| resolve_ui_language(&settings.ui_language))
        .unwrap_or(ResolvedUiLanguage::En)
}

fn tt(lang: ResolvedUiLanguage, en: &'static str, vi: &'static str) -> &'static str {
    match lang {
        ResolvedUiLanguage::En => en,
        ResolvedUiLanguage::Vi => vi,
    }
}

trait AppendableMenu {
    fn append_item(
        &self,
        item: &dyn tray_icon::menu::IsMenuItem,
    ) -> Result<(), tray_icon::menu::Error>;
}

impl AppendableMenu for Menu {
    fn append_item(
        &self,
        item: &dyn tray_icon::menu::IsMenuItem,
    ) -> Result<(), tray_icon::menu::Error> {
        self.append(item)
    }
}

impl AppendableMenu for Submenu {
    fn append_item(
        &self,
        item: &dyn tray_icon::menu::IsMenuItem,
    ) -> Result<(), tray_icon::menu::Error> {
        self.append(item)
    }
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    TrayIcon(TrayIconEvent),
    AutoStartUsageWindowsChecked,
    BackgroundTaskDone {
        notification: Option<(String, String)>,
    },
    UpdateMenu,
    PopoverAction(PopoverAction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PopoverAction {
    OpenOverview,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum TrayCommand {
    Activate(Uuid),
    Login(Uuid),
    SaveCurrent,
    SaveCursorCurrent,
    SaveClaudeCurrent,
    OpenDashboard,
    StartAddAccount,
    FinishAddAccount,
    CancelAddAccount,
    PickBestQuota,
    Delete(Uuid, String),
    SetArchived(Uuid, bool),
    SetAutoStartUsageWindows(bool),
    SetAutoSwitchOnLimit(bool),
    SetLaunchAtStartup(bool),
    SetShowQuotaInMenuBar(bool),
    OpenLogsDir,
    Refresh,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayExit {
    Quit,
}

struct TrayState<S> {
    app: Arc<App<S>>,
    tray_icon: Option<TrayIcon>,
    commands: HashMap<String, TrayCommand>,
    event_proxy: EventLoopProxy<UserEvent>,
    exit: TrayExit,
    dashboard_port: Option<u16>,
    window: Option<winit::window::Window>,
    webview: Option<wry::WebView>,
    popover: Option<winit::window::Window>,
    popover_webview: Option<wry::WebView>,
    last_tray_rect: Option<(PhysicalPosition<f64>, PhysicalSize<u32>)>,
    open_dashboard_on_start: bool,
    /// Coalesce rapid UpdateMenu storms (auto-start + background tasks).
    menu_update_pending: bool,
    last_menu_rebuild_at: Option<std::time::Instant>,
}

pub(crate) fn run<S>(app: Arc<App<S>>, open_dashboard: bool) -> Result<TrayExit>
where
    S: SecretStore + Send + Sync + 'static,
{
    #[cfg(target_os = "macos")]
    detach_from_controlling_terminal();

    let _instance_lock = match TrayInstanceLock::acquire(&tray_lock_path(&app))? {
        Some(lock) => lock,
        None => {
            log_tray_message(
                &app,
                &format!(
                    "tray already running (pid={}), exiting duplicate instance",
                    read_tray_lock_pid(&tray_lock_path(&app)).unwrap_or(0)
                ),
            );
            return Ok(TrayExit::Quit);
        }
    };

    let bound_port = match crate::server::start_dashboard_server(app.clone()) {
        Ok(port) => {
            log_tray_message(
                &app,
                &format!("Dashboard server started on http://127.0.0.1:{}", port),
            );
            Some(port)
        }
        Err(e) => {
            log_tray_message(&app, &format!("Failed to start dashboard server: {e:#}"));
            None
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
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(UserEvent::TrayIcon(event));
    }));
    spawn_auto_start_usage_windows_menu_refresh(proxy.clone());

    spawn_menu_bar_title_refresh(proxy.clone());

    let mut state = TrayState {
        app: app.clone(),
        tray_icon: None,
        commands: HashMap::new(),
        event_proxy: proxy,
        exit: TrayExit::Quit,
        dashboard_port: bound_port,
        window: None,
        webview: None,
        popover: None,
        popover_webview: None,
        last_tray_rect: None,
        open_dashboard_on_start: open_dashboard,
        menu_update_pending: false,
        last_menu_rebuild_at: None,
    };
    let run_result = event_loop.run_app(&mut state);
    log_tray_message(
        &app,
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

/// Keep menu-bar countdown / quota text fresh without waiting for a user action.
fn spawn_menu_bar_title_refresh(proxy: EventLoopProxy<UserEvent>) {
    let _ = thread::Builder::new()
        .name("tray-menu-bar-title-refresh".to_owned())
        .spawn(move || {
            loop {
                thread::sleep(std::time::Duration::from_secs(45));
                if proxy.send_event(UserEvent::UpdateMenu).is_err() {
                    break;
                }
            }
        });
}

impl<S> ApplicationHandler<UserEvent> for TrayState<S>
where
    S: SecretStore + Send + Sync + 'static,
{
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        // tray-icon must be created once the event loop is running (tauri-apps/tray-icon#90).
        if cause == StartCause::Init {
            self.ensure_tray_icon();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_tray_icon();
        if self.open_dashboard_on_start {
            self.open_dashboard_on_start = false;
            self.open_dashboard_window(event_loop);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let is_overview = self.window.as_ref().is_some_and(|w| w.id() == window_id);
        let is_popover = self.popover.as_ref().is_some_and(|w| w.id() == window_id);

        if is_overview {
            match event {
                WindowEvent::CloseRequested => {
                    self.webview = None;
                    self.window = None;
                }
                WindowEvent::Resized(size) => {
                    if let Some(webview) = &self.webview {
                        let _ = webview.set_bounds(wry_bounds_from_physical(size));
                    }
                }
                _ => {}
            }
            return;
        }

        if is_popover {
            match event {
                WindowEvent::CloseRequested => {
                    // Hide + reuse — destroying/recreating WKWebView on every click is the lag source.
                    self.hide_popover();
                }
                // Click outside / app switch: auto-dismiss the menu panel.
                WindowEvent::Focused(false) => {
                    self.hide_popover();
                }
                WindowEvent::Destroyed => {
                    self.popover_webview = None;
                    self.popover = None;
                }
                _ => {}
            }
            return;
        }

        let _ = event_loop;
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::AutoStartUsageWindowsChecked
            | UserEvent::UpdateMenu
            | UserEvent::BackgroundTaskDone { .. } => {
                if let UserEvent::BackgroundTaskDone {
                    notification: Some((title, body)),
                } = &event
                {
                    tray_notify(title, body);
                }
                self.request_menu_update();
            }
            UserEvent::Menu(event) => self.handle_menu_event(event_loop, event),
            UserEvent::TrayIcon(event) => self.handle_tray_icon_event(event_loop, event),
            UserEvent::PopoverAction(action) => match action {
                PopoverAction::OpenOverview => {
                    self.hide_popover();
                    self.open_dashboard_window(event_loop);
                }
                PopoverAction::Quit => {
                    self.hide_popover();
                    self.exit = TrayExit::Quit;
                    event_loop.exit();
                }
            },
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.menu_update_pending {
            self.menu_update_pending = false;
            if let Err(error) = self.update_tray_menu() {
                eprintln!("failed to refresh tray menu: {error:#}");
            }
        }
    }
}

impl<S> TrayState<S>
where
    S: SecretStore + Send + Sync + 'static,
{
    fn open_dashboard_window(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(port) = self.dashboard_port {
            if self.window.is_none() {
                // Native titled window — tray stays primary UX; overview is secondary.
                // Keep a normal macOS title bar ("ChatGPT Codex") so this does not feel
                // like a browser tab; localhost is never shown in UI chrome/copy.
                let window_attrs = winit::window::Window::default_attributes()
                    .with_title("ChatGPT Codex")
                    .with_inner_size(LogicalSize::new(1120.0, 760.0))
                    .with_resizable(true);
                match event_loop.create_window(window_attrs) {
                    Ok(window) => {
                        let url = format!("http://127.0.0.1:{port}/");
                        // new_as_child keeps winit's NSView as contentView. WebViewBuilder::new
                        // replaces it and later crashes in windowDidResignKey (objc weak / SIGSEGV).
                        match wry::WebViewBuilder::new_as_child(&window)
                            .with_bounds(wry_bounds_from_physical(window.inner_size()))
                            .with_url(url)
                            .build()
                        {
                            Ok(webview) => {
                                self.webview = Some(webview);
                                self.window = Some(window);
                            }
                            Err(e) => {
                                eprintln!("failed to create WebView: {e}");
                                tray_notify("ChatGPT Codex", "Failed to open overview window.");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("failed to create window: {e}");
                        tray_notify("ChatGPT Codex", "Failed to create overview window.");
                    }
                }
            } else if let Some(window) = &self.window {
                window.focus_window();
            }
        } else {
            tray_notify("ChatGPT Codex", "Overview is unavailable right now.");
        }
    }

    fn hide_popover(&mut self) {
        if let Some(window) = &self.popover {
            window.set_visible(false);
        }
        if let Some(webview) = &self.popover_webview {
            // Cached WKWebView stays alive — stop JS polls while hidden.
            let _ = webview
                .evaluate_script("try{window.__popoverHidden&&window.__popoverHidden()}catch(e){}");
        }
    }

    fn destroy_popover(&mut self) {
        self.popover_webview = None;
        self.popover = None;
    }

    fn handle_tray_icon_event(&mut self, event_loop: &ActiveEventLoop, event: TrayIconEvent) {
        #[cfg(target_os = "macos")]
        {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                self.last_tray_rect = Some((rect.position, rect.size));
                self.toggle_popover(event_loop);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (event_loop, event);
        }
    }

    #[cfg(target_os = "macos")]
    fn popover_anchor(&self) -> (f64, f64) {
        const WIDTH: f64 = 392.0;
        if let Some((pos, size)) = self.last_tray_rect {
            let x = pos.x + (f64::from(size.width) / 2.0) - (WIDTH / 2.0);
            let y = pos.y + f64::from(size.height) + 6.0;
            return (x, y);
        }
        if let Some(tray) = &self.tray_icon
            && let Some(rect) = tray.rect()
        {
            let x = rect.position.x + (f64::from(rect.size.width) / 2.0) - (WIDTH / 2.0);
            let y = rect.position.y + f64::from(rect.size.height) + 6.0;
            return (x, y);
        }
        (40.0, 40.0)
    }

    #[cfg(target_os = "macos")]
    fn toggle_popover(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.popover
            && window.is_visible() == Some(true)
        {
            self.hide_popover();
            return;
        }
        self.open_popover(event_loop);
    }

    #[cfg(target_os = "macos")]
    fn show_existing_popover(&mut self) {
        let (x, y) = self.popover_anchor();
        if let Some(window) = &self.popover {
            window.set_outer_position(PhysicalPosition::new(x, y));
            window.set_visible(true);
            window.focus_window();
        }
        if let Some(webview) = &self.popover_webview {
            // Refresh data without reloading the document / recreating WKWebView.
            let _ = webview
                .evaluate_script("try{window.__popoverShown&&window.__popoverShown()}catch(e){}");
        }
    }

    #[cfg(target_os = "macos")]
    fn open_popover(&mut self, event_loop: &ActiveEventLoop) {
        let Some(port) = self.dashboard_port else {
            tray_notify("ChatGPT Codex", "Menu panel is unavailable right now.");
            return;
        };

        if self.popover.is_some() && self.popover_webview.is_some() {
            self.show_existing_popover();
            return;
        }
        // Partial / failed prior create — drop and rebuild once.
        self.destroy_popover();

        const WIDTH: f64 = 392.0;
        const HEIGHT: f64 = 580.0;
        let (x, y) = self.popover_anchor();

        use winit::platform::macos::WindowAttributesExtMacOS;
        let window_attrs = winit::window::Window::default_attributes()
            .with_title("ChatGPT Codex")
            .with_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_max_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_min_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_visible(false)
            // Native square shadow bleeds past HTML border-radius; CSS shadow is enough.
            .with_has_shadow(false)
            .with_position(PhysicalPosition::new(x, y));

        match event_loop.create_window(window_attrs) {
            Ok(window) => {
                // Reinforce transparency: NSWindow must be non-opaque + clearColor or
                // rounded HTML leaves white corners over the default window fill.
                window.set_transparent(true);
                let proxy = self.event_proxy.clone();
                let url = format!("http://127.0.0.1:{port}/menu");
                // Child webview: must not replace winit's contentView (see open_dashboard_window).
                match wry::WebViewBuilder::new_as_child(&window)
                    .with_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
                        size: wry::dpi::LogicalSize::new(WIDTH, HEIGHT).into(),
                    })
                    .with_transparent(true)
                    .with_url(url)
                    .with_ipc_handler(move |request| {
                        let body = request.body().as_str();
                        let action = match body {
                            "open-overview" => Some(PopoverAction::OpenOverview),
                            "quit" => Some(PopoverAction::Quit),
                            _ => None,
                        };
                        if let Some(action) = action {
                            let _ = proxy.send_event(UserEvent::PopoverAction(action));
                        }
                    })
                    .build()
                {
                    Ok(webview) => {
                        let _ = window.set_cursor_hittest(true);
                        window.set_visible(true);
                        window.focus_window();
                        self.popover_webview = Some(webview);
                        self.popover = Some(window);
                    }
                    Err(e) => {
                        eprintln!("failed to create popover WebView: {e}");
                        tray_notify("ChatGPT Codex", "Failed to open menu panel.");
                    }
                }
            }
            Err(e) => {
                eprintln!("failed to create popover window: {e}");
                tray_notify("ChatGPT Codex", "Failed to create menu panel.");
            }
        }
    }

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
                let cursor_status = self.app.cursor_status().ok();
                let claude_status = self.app.claude_status().ok();
                let tooltip = self.get_tooltip_text(
                    &status,
                    &list,
                    cursor_status.as_ref(),
                    claude_status.as_ref(),
                );
                let title = self.get_menu_bar_title(
                    &status,
                    &list,
                    cursor_status.as_ref(),
                    claude_status.as_ref(),
                );
                match self.rebuild_menu_with_status_and_list(
                    &status,
                    &list,
                    cursor_status.as_ref(),
                    claude_status.as_ref(),
                ) {
                    Ok(menu) => {
                        let (icon, template) = load_tray_icon();
                        let mut builder = TrayIconBuilder::new()
                            .with_tooltip(tooltip)
                            .with_icon(icon)
                            .with_title(title)
                            .with_menu(Box::new(menu));
                        #[cfg(target_os = "macos")]
                        {
                            builder = builder
                                .with_icon_as_template(template)
                                .with_menu_on_left_click(false)
                                .with_menu_on_right_click(true);
                        }
                        match builder.build() {
                            Ok(tray_icon) => {
                                self.tray_icon = Some(tray_icon);
                                self.last_menu_rebuild_at = Some(std::time::Instant::now());
                                self.spawn_startup_usage_refresh();
                                #[cfg(target_os = "macos")]
                                wake_main_run_loop();
                                log_tray_message(
                                    &self.app,
                                    &format!(
                                        "tray icon created (template={template}, pid={})",
                                        std::process::id()
                                    ),
                                );
                            }
                            Err(error) => log_tray_error(
                                &self.app,
                                &format!("failed to create tray icon: {error:#}"),
                            ),
                        }
                    }
                    Err(error) => {
                        log_tray_error(&self.app, &format!("failed to build tray menu: {error:#}"))
                    }
                }
            }
            Err(error) => log_tray_error(
                &self.app,
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
                    let account_name = account_display_name(&output.account);
                    let plan = format_plan_label_simple(output.account.plan_label.as_deref());
                    let detail = if was_running && policy == SwitchWhenRunning::WaitAndSwitch {
                        " Codex restarted after tasks finished."
                    } else {
                        " Codex restarted."
                    };
                    Ok(Some((
                        "ChatGPT Codex".to_owned(),
                        format!("Switched to {account_name} ({plan}).{detail}"),
                    )))
                });
            }
            Some(TrayCommand::Login(account_id)) => {
                let proxy = self.event_proxy.clone();
                let env = self.app.env().clone();
                spawn_tray_background(proxy, move || {
                    let app = tray_app_for_env(&env);
                    let output = app.start_login_for_saved_account(account_id)?;
                    let launch =
                        crate::process::relaunch_codex_for_interactive_login(&env.codex_root);
                    let account_name = account_display_name(&output.account);
                    let mut msg = format!(
                        "Sign in for {account_name}: {}. After the browser finishes, choose Save signed-in session.",
                        launch.detail
                    );
                    if !launch.oauth_port_ready {
                        msg.push_str(" (OAuth port not ready yet.)");
                    }
                    Ok(Some(("ChatGPT Codex".to_owned(), msg)))
                });
            }
            Some(TrayCommand::SaveCurrent) => {
                let msg = match self.app.save_current() {
                    Ok(output) => {
                        format!(
                            "Saved workspace {} successfully.",
                            account_display_name(&output.account)
                        )
                    }
                    Err(error) => {
                        format!("Failed to save workspace: {error:#}")
                    }
                };
                tray_notify("ChatGPT Codex", &msg);
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::SaveCursorCurrent) => {
                let msg = match self.app.save_cursor_current() {
                    Ok(output) => {
                        format!(
                            "Saved Cursor workspace {} successfully.",
                            account_display_name(&output.account)
                        )
                    }
                    Err(error) => {
                        format!("Failed to save Cursor workspace: {error:#}")
                    }
                };
                tray_notify("ChatGPT Codex", &msg);
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::SaveClaudeCurrent) => {
                let msg = match self.app.save_claude_current() {
                    Ok(output) => {
                        format!(
                            "Saved Claude workspace {} successfully.",
                            account_display_name(&output.account)
                        )
                    }
                    Err(error) => {
                        format!("Failed to save Claude workspace: {error:#}")
                    }
                };
                tray_notify("ChatGPT Codex", &msg);
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::OpenDashboard) => {
                self.open_dashboard_window(event_loop);
            }
            Some(TrayCommand::StartAddAccount) => {
                let env = self.app.env().clone();
                let started = if codex::add_account_session_active(self.app.env()) {
                    // Stuck / mid-session: re-run CLI login without failing "already in progress".
                    Ok(())
                } else {
                    self.app.begin_add_account_session()
                };
                match started {
                    Ok(()) => {
                        let proxy = self.event_proxy.clone();
                        spawn_tray_background(proxy, move || {
                            let launch =
                                crate::process::relaunch_codex_for_interactive_login(&env.codex_root);
                            let mut msg = format!(
                                "Step 1: {}. Step 2: return here → Finish Adding Workspace.",
                                launch.detail
                            );
                            if !launch.oauth_port_ready {
                                msg.push_str(" (OAuth port not ready yet.)");
                            }
                            Ok(Some(("ChatGPT Codex".to_owned(), msg)))
                        });
                    }
                    Err(error) => {
                        tray_notify(
                            "ChatGPT Codex",
                            &format!("Failed to start add account: {error:#}"),
                        );
                    }
                }
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
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
                                    account_display_name(&output.account)
                                )
                            } else {
                                crate::process::launch_codex_app();
                                format!(
                                    "Added workspace {} successfully.",
                                    account_display_name(&output.account)
                                )
                            }
                        }
                        Err(error) => format!("Failed to finish adding account: {error:#}"),
                    };
                    Ok(Some(("ChatGPT Codex".to_owned(), msg)))
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
                    Ok(Some(("ChatGPT Codex".to_owned(), msg)))
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
                            let account_name = account_display_name(&output.account);
                            if output.switched {
                                if was_running && policy == SwitchWhenRunning::WaitAndSwitch {
                                    format!(
                                        "Switched to best available quota: {account_name} after Codex finished."
                                    )
                                } else {
                                    format!("Switched to best available quota: {account_name}.")
                                }
                            } else {
                                format!("Already on best available quota: {account_name}.")
                            }
                        }
                        Err(error) => format!("Pick best quota failed: {error:#}"),
                    };
                    if was_running && msg.starts_with("Switched to best") {
                        crate::process::launch_codex_app();
                    }
                    Ok(Some(("ChatGPT Codex".to_owned(), msg)))
                });
            }
            Some(TrayCommand::Delete(account_id, account_name)) => {
                let confirmed = {
                    #[cfg(target_os = "macos")]
                    {
                        let script = format!(
                            "tell application \"System Events\" to display dialog \"Are you sure you want to delete {}?\" buttons {{\"Cancel\", \"Delete\"}} default button \"Cancel\" with icon caution",
                            account_name.replace('"', "\\\"")
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
                            account_name
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
                            let msg = format!("Deleted workspace {} successfully.", account_name);
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("osascript")
                                    .arg("-e")
                                    .arg(format!(
                                        "display notification \"{}\" with title \"ChatGPT Codex\"",
                                        msg.replace('"', "\\\"")
                                    ))
                                    .spawn();
                            }
                        }
                        Err(error) => {
                            eprintln!("failed to delete account: {error:#}");
                        }
                    }
                    let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
                }
            }
            Some(TrayCommand::SetArchived(account_id, archived)) => {
                match self.app.set_account_archived(account_id, archived) {
                    Ok(_) => {
                        let verb = if archived { "Archived" } else { "Unarchived" };
                        tray_notify("ChatGPT Codex", &format!("{verb} account successfully."));
                    }
                    Err(error) => {
                        tray_notify(
                            "ChatGPT Codex",
                            &format!("Failed to archive/unarchive: {error:#}"),
                        );
                    }
                }
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
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
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
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
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::SetLaunchAtStartup(enabled)) => {
                if let Err(error) = self.app.set_launch_at_startup(enabled) {
                    eprintln!("failed to update launch at startup from tray: {error:#}");
                }
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::SetShowQuotaInMenuBar(enabled)) => {
                if let Err(error) = self.app.set_show_quota_in_menu_bar(enabled) {
                    eprintln!("failed to update show quota in menu bar from tray: {error:#}");
                }
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::OpenLogsDir) => {
                let log_dir = self.app.env().app_data_dir.join("logs");
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(&log_dir).spawn();
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("explorer").arg(&log_dir).spawn();
            }
            Some(TrayCommand::Refresh) => {
                let _ = self.event_proxy.send_event(UserEvent::UpdateMenu);
            }
            Some(TrayCommand::Quit) => {
                self.exit = TrayExit::Quit;
                event_loop.exit();
            }
            None => {}
        }
    }

    fn request_menu_update(&mut self) {
        // Coalesce bursts so one event-loop turn rebuilds once.
        self.menu_update_pending = true;
        // If the last rebuild was very recent, still mark pending — about_to_wait
        // will run it once the event loop settles.
        if self
            .last_menu_rebuild_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(80))
        {
            return;
        }
        self.menu_update_pending = false;
        if let Err(error) = self.update_tray_menu() {
            eprintln!("failed to refresh tray menu: {error:#}");
        }
    }

    fn update_tray_menu(&mut self) -> Result<()> {
        let status = self.app.status()?;
        let list = self.app.list()?;
        // Single status pass — title/tooltip/menu previously each re-fetched Cursor/Claude.
        let cursor_status = self.app.cursor_status().ok();
        let claude_status = self.app.claude_status().ok();
        let tooltip = self.get_tooltip_text(
            &status,
            &list,
            cursor_status.as_ref(),
            claude_status.as_ref(),
        );
        let title = self.get_menu_bar_title(
            &status,
            &list,
            cursor_status.as_ref(),
            claude_status.as_ref(),
        );
        let menu = self.rebuild_menu_with_status_and_list(
            &status,
            &list,
            cursor_status.as_ref(),
            claude_status.as_ref(),
        )?;
        if let Some(tray_icon) = &self.tray_icon {
            let _ = tray_icon.set_tooltip(Some(&tooltip));
            tray_icon.set_title(Some(title));
            tray_icon.set_menu(Some(Box::new(menu)));
        }
        self.last_menu_rebuild_at = Some(std::time::Instant::now());
        Ok(())
    }

    fn get_menu_bar_title(
        &self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
        cursor_status: Option<&crate::model::StatusOutput>,
        claude_status: Option<&crate::model::StatusOutput>,
    ) -> String {
        let Ok(settings) = self.app.show_quota_in_menu_bar_status() else {
            return String::new();
        };
        if !settings.enabled {
            return String::new();
        }

        let codex_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref().unwrap_or("codex") == "codex")
            .cloned()
            .collect();
        let active_codex = find_active_tray_account(
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &codex_accounts,
        );

        let cursor_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("cursor"))
            .cloned()
            .collect();
        let active_cursor = cursor_status.and_then(|cur_status| {
            find_active_tray_account(
                cur_status.current_account.as_ref(),
                cur_status.current_account_saved_id,
                &cursor_accounts,
            )
        });

        let claude_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("claude"))
            .cloned()
            .collect();
        // Claude live identity often lacks email (or keychain times out on poll).
        // Still resolve a saved Claude account so the menu bar can show CL %.
        let active_claude = claude_status
            .and_then(|c_status| {
                find_active_tray_account(
                    c_status.current_account.as_ref(),
                    c_status.current_account_saved_id,
                    &claude_accounts,
                )
            })
            .or_else(|| {
                fallback_unresolved_claude_tray_account(
                    &claude_accounts,
                    &DisplayIdentity {
                        email: crate::claude::CLAUDE_UNKNOWN_EMAIL.to_owned(),
                        subject: None,
                        name: None,
                        plan_label: claude_status
                            .and_then(|s| s.current_account.as_ref())
                            .and_then(|id| id.plan_label.clone()),
                        workspace_id: None,
                        workspace_name: None,
                    },
                )
            });
        // Prefer live identity; otherwise synthesize from the resolved saved account
        // so format_menu_bar_part does not skip CL when keychain identity is empty.
        let claude_identity_owned: Option<DisplayIdentity> = claude_status
            .and_then(|s| s.current_account.clone())
            .or_else(|| {
                active_claude.map(|acc| DisplayIdentity {
                    email: acc.email.clone(),
                    subject: acc.subject.clone(),
                    name: acc.name.clone(),
                    plan_label: acc.plan_label.clone(),
                    workspace_id: acc.workspace_id.clone(),
                    workspace_name: acc.workspace_name.clone(),
                })
            });

        let mut parts = Vec::new();

        if let Some(part) =
            self.format_menu_bar_part("CX", status.current_account.as_ref(), active_codex, true)
        {
            parts.push(part);
        }
        if let Some(part) = self.format_menu_bar_part(
            "CR",
            cursor_status.and_then(|s| s.current_account.as_ref()),
            active_cursor,
            false,
        ) {
            parts.push(part);
        }
        if let Some(part) = self.format_menu_bar_part(
            "CL",
            claude_identity_owned.as_ref(),
            active_claude,
            false,
        ) {
            parts.push(part);
        }

        if parts.is_empty() {
            String::new()
        } else {
            // Compact spacing keeps the macOS menu bar readable with 3 providers.
            format!(" {}", parts.join(" "))
        }
    }

    fn format_menu_bar_part(
        &self,
        label: &str,
        current_identity: Option<&DisplayIdentity>,
        active_account: Option<&AccountView>,
        weekly_only: bool,
    ) -> Option<String> {
        current_identity?;

        let Some(account) = active_account else {
            // Live session detected but not matched to a saved snapshot with usage.
            return Some(format!("{label} ·"));
        };

        if account
            .usage_error
            .as_deref()
            .is_some_and(usage_error_requires_login)
        {
            return Some(format!("{label} auth"));
        }

        if let Some(usage) = &account.usage {
            let now = OffsetDateTime::now_utc();
            // Prefer provider-specific windows, then fall back to any available
            // window so Codex still shows 5h when weekly is null (common API shape).
            let mut windows: Vec<&crate::model::UsageWindowView> = if weekly_only {
                usage.weekly.as_ref().into_iter().collect()
            } else {
                [usage.five_hour.as_ref(), usage.weekly.as_ref()]
                    .into_iter()
                    .flatten()
                    .collect()
            };
            if windows.is_empty() {
                windows = [usage.five_hour.as_ref(), usage.weekly.as_ref()]
                    .into_iter()
                    .flatten()
                    .collect();
            }

            // When any active window is at 0%, show the soonest reset countdown.
            let exhausted_resets: Vec<_> = windows
                .iter()
                .filter(|w| w.remaining_percent == 0 && w.reset_at > now)
                .map(|w| w.reset_at)
                .collect();
            if !exhausted_resets.is_empty() {
                // Prefer showing remaining % of a non-zero window when available
                // (e.g. Cursor Auto 0% but monthly still has quota).
                let live = windows
                    .iter()
                    .filter(|w| w.remaining_percent > 0 && w.reset_at > now)
                    .min_by_key(|w| w.remaining_percent);
                if let Some(window) = live {
                    return Some(format!("{label} {}%", window.remaining_percent));
                }
                if let Some(reset_at) = exhausted_resets.into_iter().min() {
                    return Some(format!(
                        "{label} {}",
                        crate::time_display::format_countdown(reset_at, now)
                    ));
                }
            }

            let bottleneck = windows
                .iter()
                .filter(|w| w.reset_at > now)
                .min_by_key(|w| w.remaining_percent);
            if let Some(window) = bottleneck {
                return Some(format!("{label} {}%", window.remaining_percent));
            }

            // Past-reset cache: still show last known remaining so the bar is not blank.
            if let Some(window) = windows.iter().min_by_key(|w| w.remaining_percent) {
                return Some(format!("{label} {}%", window.remaining_percent));
            }
            if usage.has_stale_quota_cache(now) {
                return Some(format!("{label} stale"));
            }
        }

        // No cached usage yet (refresh in progress or never fetched).
        Some(format!("{label} …"))
    }

    fn get_tooltip_text(
        &self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
        cursor_status: Option<&crate::model::StatusOutput>,
        claude_status: Option<&crate::model::StatusOutput>,
    ) -> String {
        let codex_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref().unwrap_or("codex") == "codex")
            .cloned()
            .collect();
        let active_codex = find_active_tray_account(
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &codex_accounts,
        );

        let cursor_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("cursor"))
            .cloned()
            .collect();
        let active_cursor = cursor_status.and_then(|cur_status| {
            find_active_tray_account(
                cur_status.current_account.as_ref(),
                cur_status.current_account_saved_id,
                &cursor_accounts,
            )
        });

        let claude_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("claude"))
            .cloned()
            .collect();
        let active_claude = claude_status.and_then(|c_status| {
            find_active_tray_account(
                c_status.current_account.as_ref(),
                c_status.current_account_saved_id,
                &claude_accounts,
            )
        });

        fn format_app_tooltip_line(
            app_name: &str,
            current: Option<&DisplayIdentity>,
            active_account: Option<&AccountView>,
        ) -> Option<String> {
            current.map(|identity| {
                let plan = format_plan_label_simple(identity.plan_label.as_deref());
                let usage_info =
                    active_account
                        .map(|act| {
                            let (remaining, _) = account_usage_labels_simple(act);
                            let status = account_status_label_simple(act);
                            let pace = act.usage.as_ref().and_then(|u| u.weekly.as_ref()).and_then(
                                |weekly| {
                                    let now = OffsetDateTime::now_utc();
                                    if weekly.reset_at > now {
                                        crate::usage_pace::pace_for_weekly(
                                            weekly.used_percent,
                                            weekly.reset_at,
                                            now,
                                        )
                                        .map(|p| p.delta_text().to_owned())
                                    } else {
                                        None
                                    }
                                },
                            );
                            let mut parts = Vec::new();
                            if !status.is_empty() {
                                parts.push(status);
                            }
                            if !remaining.is_empty() {
                                parts.push(remaining);
                            }
                            if let Some(pace) = pace {
                                parts.push(pace);
                            }
                            if parts.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", parts.join(", "))
                            }
                        })
                        .unwrap_or_default();
                format!(
                    "{app_name}: {} ({plan}){usage_info}",
                    identity_display_name(identity)
                )
            })
        }

        let codex_line = format_app_tooltip_line(
            "ChatGPT Codex",
            status.current_account.as_ref(),
            active_codex,
        );
        let cursor_line = cursor_status.and_then(|cur_status| {
            format_app_tooltip_line("Cursor", cur_status.current_account.as_ref(), active_cursor)
        });
        let claude_line = claude_status.and_then(|c_status| {
            format_app_tooltip_line("Claude", c_status.current_account.as_ref(), active_claude)
        });

        let mut lines = vec!["ChatGPT Codex".to_owned()];
        let lang = tray_ui_lang(self.app.as_ref());
        let not_logged = tt(lang, "Not logged in", "Chưa đăng nhập");
        if let Some(line) = codex_line {
            lines.push(line);
        } else {
            lines.push(format!("ChatGPT Codex: {not_logged}"));
        }
        if let Some(line) = cursor_line {
            lines.push(line);
        } else {
            lines.push(format!("Cursor: {not_logged}"));
        }
        if let Some(line) = claude_line {
            lines.push(line);
        } else {
            lines.push(format!("Claude: {not_logged}"));
        }

        lines.join("\n")
    }

    fn rebuild_menu_with_status_and_list(
        &mut self,
        status: &crate::model::StatusOutput,
        list: &crate::model::ListOutput,
        cursor_status: Option<&crate::model::StatusOutput>,
        claude_status: Option<&crate::model::StatusOutput>,
    ) -> Result<Menu> {
        if codex::add_account_session_active(self.app.env()) {
            return self.rebuild_add_account_pending_menu();
        }

        let menu = Menu::new();
        self.commands.clear();

        let codex_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref().unwrap_or("codex") == "codex")
            .cloned()
            .collect();
        let cursor_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("cursor"))
            .cloned()
            .collect();
        let claude_accounts: Vec<AccountView> = list
            .accounts
            .iter()
            .filter(|acc| acc.target_app.as_deref() == Some("claude"))
            .cloned()
            .collect();
        let lang = tray_ui_lang(self.app.as_ref());

        // ── Codex Section ───────────────────────────────────────
        self.rebuild_app_section(
            &menu,
            "ChatGPT Codex",
            status.current_account.as_ref(),
            status.current_account_saved_id,
            &codex_accounts,
            lang,
        )?;

        menu.append(&PredefinedMenuItem::separator())?;

        // ── Cursor Section (always shown) ───────────────────────
        self.rebuild_app_section(
            &menu,
            "Cursor",
            cursor_status.and_then(|s| s.current_account.as_ref()),
            cursor_status.and_then(|s| s.current_account_saved_id),
            &cursor_accounts,
            lang,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;

        // ── Claude Section (always shown) ───────────────────────
        self.rebuild_app_section(
            &menu,
            "Claude",
            claude_status.and_then(|s| s.current_account.as_ref()),
            claude_status.and_then(|s| s.current_account_saved_id),
            &claude_accounts,
            lang,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;

        // ── Actions (tray-first; overview is secondary) ──────────
        self.append_command(
            &menu,
            "pick-best-quota",
            tt(lang, "Switch to Best Quota", "Chuyển hạn mức tốt nhất"),
            TrayCommand::PickBestQuota,
        )?;
        self.append_command(
            &menu,
            "refresh",
            tt(lang, "Refresh", "Làm mới"),
            TrayCommand::Refresh,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;

        let automation_submenu = Submenu::new(tt(lang, "Automation", "Tự động hóa"), true);
        let auto_start_enabled = self.app.auto_start_usage_windows_status()?.enabled;
        let auto_refresh_label = tt(
            lang,
            AUTO_REFRESH_QUOTA_ON_RESET_LABEL,
            "Tự làm mới hạn mức khi reset",
        );
        self.append_check_submenu_command(
            &automation_submenu,
            "toggle-auto-start-usage-windows",
            auto_refresh_label,
            auto_start_enabled,
            TrayCommand::SetAutoStartUsageWindows(!auto_start_enabled),
        )?;
        let auto_switch_enabled = self.app.auto_switch_on_limit_status()?.enabled;
        self.append_check_submenu_command(
            &automation_submenu,
            "toggle-auto-switch-on-limit",
            tt(
                lang,
                "Auto-switch when exhausted",
                "Tự chuyển khi hết hạn mức",
            ),
            auto_switch_enabled,
            TrayCommand::SetAutoSwitchOnLimit(!auto_switch_enabled),
        )?;
        let launch_at_startup_enabled = self.app.launch_at_startup_status()?.enabled;
        self.append_check_submenu_command(
            &automation_submenu,
            "toggle-launch-at-startup",
            tt(lang, "Launch at Login", "Mở khi đăng nhập"),
            launch_at_startup_enabled,
            TrayCommand::SetLaunchAtStartup(!launch_at_startup_enabled),
        )?;
        let show_quota_enabled = self.app.show_quota_in_menu_bar_status()?.enabled;
        self.append_check_submenu_command(
            &automation_submenu,
            "toggle-show-quota-in-menu-bar",
            tt(lang, "Show Quota in Menu Bar", "Hiện hạn mức trên menu bar"),
            show_quota_enabled,
            TrayCommand::SetShowQuotaInMenuBar(!show_quota_enabled),
        )?;
        menu.append(&automation_submenu)?;
        self.append_command(
            &menu,
            "open-dashboard",
            tt(lang, "Open Overview…", "Mở Tổng quan…"),
            TrayCommand::OpenDashboard,
        )?;
        self.append_command(
            &menu,
            "start-add-account",
            tt(lang, "Add Codex Account…", "Thêm tài khoản Codex…"),
            TrayCommand::StartAddAccount,
        )?;
        self.append_command(
            &menu,
            "open-logs",
            tt(lang, "Open Logs Folder", "Mở thư mục nhật ký"),
            TrayCommand::OpenLogsDir,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::new(
            tt(lang, "Inspired by CodexBar", "Tham khảo CodexBar"),
            false,
            None,
        ))?;
        self.append_command(
            &menu,
            "quit",
            tt(lang, "Quit ChatGPT Codex", "Thoát ChatGPT Codex"),
            TrayCommand::Quit,
        )?;
        Ok(menu)
    }

    fn rebuild_app_section(
        &mut self,
        menu: &dyn AppendableMenu,
        app_label: &str,
        current_account: Option<&crate::model::DisplayIdentity>,
        current_saved_id: Option<Uuid>,
        accounts: &[crate::model::AccountView],
        lang: ResolvedUiLanguage,
    ) -> Result<()> {
        // CodexBar-style section header (disabled text item, not "--- dashes ---").
        menu.append_item(&MenuItem::new(app_label, false, None))?;

        let active_account = find_active_tray_account(current_account, current_saved_id, accounts);
        let active_account_id = active_account.map(|account| account.id);
        let weekly_only = app_label == "ChatGPT Codex";

        if let Some(current) = current_account {
            let not_saved = active_account.is_none();
            let name = identity_display_name(current);
            let plan = format_plan_label_simple(current.plan_label.as_deref());
            let unsaved = tt(lang, "unsaved", "chưa lưu");
            let title = if not_saved {
                format!("\u{2713} {name} · {plan} ({unsaved})")
            } else {
                format!("\u{2713} {name} · {plan}")
            };
            menu.append_item(&MenuItem::new(title, false, None))?;

            let needs_login = active_account
                .and_then(|a| a.usage_error.as_deref())
                .is_some_and(usage_error_requires_login);
            if needs_login {
                if let Some(act_acc) = active_account {
                    let login_id = format!("login_active_{}", act_acc.id);
                    let item = MenuItem::with_id(
                        MenuId::new(&login_id),
                        tt(lang, "Sign in again…", "Đăng nhập lại…"),
                        true,
                        None,
                    );
                    menu.append_item(&item)?;
                    self.commands
                        .insert(login_id, TrayCommand::Login(act_acc.id));
                } else {
                    menu.append_item(&MenuItem::new(
                        tt(lang, "Login required", "Cần đăng nhập"),
                        false,
                        None,
                    ))?;
                }
            } else {
                for line in format_usage_menu_lines(
                    active_account.and_then(|a| a.usage.as_ref()),
                    weekly_only,
                    lang,
                ) {
                    menu.append_item(&MenuItem::new(line, false, None))?;
                }

                if not_saved {
                    let save_id =
                        format!("save_active_{}", app_label.to_lowercase().replace(' ', "_"));
                    let item = MenuItem::with_id(
                        MenuId::new(&save_id),
                        tt(lang, "Save Active Account", "Lưu tài khoản đang dùng"),
                        true,
                        None,
                    );
                    menu.append_item(&item)?;
                    self.commands.insert(
                        save_id,
                        match app_label {
                            "ChatGPT Codex" => TrayCommand::SaveCurrent,
                            "Cursor" => TrayCommand::SaveCursorCurrent,
                            "Claude" => TrayCommand::SaveClaudeCurrent,
                            _ => TrayCommand::Refresh,
                        },
                    );
                }
            }
        } else {
            menu.append_item(&MenuItem::new(
                tt(lang, "Not logged in", "Chưa đăng nhập"),
                false,
                None,
            ))?;
        }

        // Flat Switch Account submenu (no nested "Hidden Accounts").
        let switch_submenu = Submenu::new(tt(lang, "Switch Account", "Đổi tài khoản"), true);
        if accounts.is_empty() {
            switch_submenu.append_item(&MenuItem::new(
                tt(lang, "No saved accounts", "Chưa có tài khoản đã lưu"),
                false,
                None,
            ))?;
        } else {
            let mut ready_group = Vec::new();
            let mut depleted_group = Vec::new();
            let mut login_group = Vec::new();
            let mut archived_group = Vec::new();

            for account in accounts {
                if account.is_archived {
                    archived_group.push(account);
                } else {
                    let needs_login = account
                        .usage_error
                        .as_deref()
                        .is_some_and(usage_error_requires_login);
                    if needs_login {
                        login_group.push(account);
                    } else if let Some(usage) = &account.usage
                        && usage.is_out_of_quota(OffsetDateTime::now_utc())
                    {
                        depleted_group.push(account);
                    } else {
                        ready_group.push(account);
                    }
                }
            }

            depleted_group.sort_by(|a, b| {
                let ta = get_nearest_reset_time(a);
                let tb = get_nearest_reset_time(b);
                match (ta, tb) {
                    (Some(t1), Some(t2)) => t1.cmp(&t2),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            });

            let mut first_group = true;
            append_switch_account_group(
                &switch_submenu,
                tt(lang, "Ready", "Sẵn sàng"),
                &ready_group,
                active_account_id,
                &mut self.commands,
                &mut first_group,
            )?;
            append_switch_account_group(
                &switch_submenu,
                tt(lang, "Depleted", "Đã hết"),
                &depleted_group,
                active_account_id,
                &mut self.commands,
                &mut first_group,
            )?;
            append_switch_account_group(
                &switch_submenu,
                tt(lang, "Login required", "Cần đăng nhập"),
                &login_group,
                active_account_id,
                &mut self.commands,
                &mut first_group,
            )?;
            append_switch_account_group(
                &switch_submenu,
                tt(lang, "Archived", "Đã lưu trữ"),
                &archived_group,
                active_account_id,
                &mut self.commands,
                &mut first_group,
            )?;
        }
        menu.append_item(&switch_submenu)?;

        Ok(())
    }

    fn rebuild_add_account_pending_menu(&mut self) -> Result<Menu> {
        let lang = tray_ui_lang(self.app.as_ref());
        let menu = Menu::new();
        self.commands.clear();

        menu.append(&MenuItem::new(
            tt(
                lang,
                "Adding Account / Workspace",
                "Đang thêm tài khoản / workspace",
            ),
            false,
            None,
        ))?;
        menu.append(&MenuItem::new(
            tt(lang, "  1. Log in with Codex", "  1. Đăng nhập Codex"),
            false,
            None,
        ))?;
        menu.append(&MenuItem::new(
            tt(lang, "  2. Return to this menu", "  2. Quay lại menu này"),
            false,
            None,
        ))?;
        menu.append(&MenuItem::new(
            tt(
                lang,
                "  3. Finish adding workspace",
                "  3. Hoàn tất thêm workspace",
            ),
            false,
            None,
        ))?;
        menu.append(&PredefinedMenuItem::separator())?;
        self.append_command(
            &menu,
            "finish-add-account",
            tt(
                lang,
                "  Finish Adding Workspace",
                "  Hoàn tất thêm workspace",
            ),
            TrayCommand::FinishAddAccount,
        )?;
        self.append_command(
            &menu,
            "cancel-add-account",
            tt(lang, "  Cancel", "  Hủy"),
            TrayCommand::CancelAddAccount,
        )?;
        menu.append(&PredefinedMenuItem::separator())?;
        self.append_command(
            &menu,
            "refresh",
            tt(lang, "Refresh", "Làm mới"),
            TrayCommand::Refresh,
        )?;
        self.append_command(&menu, "quit", tt(lang, "Quit", "Thoát"), TrayCommand::Quit)?;
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

    fn append_check_submenu_command(
        &mut self,
        menu: &Submenu,
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

fn get_nearest_reset_time(account: &AccountView) -> Option<OffsetDateTime> {
    let usage = account.usage.as_ref()?;
    let now = OffsetDateTime::now_utc();
    let mut nearest: Option<OffsetDateTime> = None;
    if let Some(five_hour) = &usage.five_hour
        && five_hour.remaining_percent == 0
        && five_hour.reset_at > now
    {
        nearest = Some(five_hour.reset_at);
    }
    if let Some(weekly) = &usage.weekly
        && weekly.remaining_percent == 0
        && weekly.reset_at > now
    {
        if let Some(n) = nearest {
            if weekly.reset_at < n {
                nearest = Some(weekly.reset_at);
            }
        } else {
            nearest = Some(weekly.reset_at);
        }
    }
    nearest
}

#[allow(dead_code)]
fn tray_saved_accounts(accounts: &[AccountView]) -> Vec<&AccountView> {
    accounts.iter().collect()
}

fn append_switch_account_group(
    menu: &dyn AppendableMenu,
    heading: &str,
    accounts: &[&AccountView],
    active_account_id: Option<Uuid>,
    commands: &mut HashMap<String, TrayCommand>,
    first_group: &mut bool,
) -> Result<()> {
    if accounts.is_empty() {
        return Ok(());
    }
    if !*first_group {
        menu.append_item(&PredefinedMenuItem::separator())?;
    }
    *first_group = false;
    menu.append_item(&MenuItem::new(heading, false, None))?;
    for account in accounts {
        append_tray_account_item(menu, account, active_account_id, commands)?;
    }
    Ok(())
}

fn append_tray_account_item(
    menu: &dyn AppendableMenu,
    account: &AccountView,
    active_account_id: Option<Uuid>,
    commands: &mut HashMap<String, TrayCommand>,
) -> Result<()> {
    let id = format!("activate:{}", account.id);
    let is_active = Some(account.id) == active_account_id || account.is_active;
    let weekly_only = account
        .target_app
        .as_deref()
        .map(|app| app == "codex")
        .unwrap_or(true);
    let label = format_switch_account_label(account, is_active, weekly_only);

    let needs_login = account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login);

    if is_active {
        menu.append_item(&MenuItem::new(label, false, None))?;
    } else if needs_login {
        let login_id = format!("login:{}", account.id);
        let item = MenuItem::with_id(MenuId::new(&login_id), label, true, None);
        menu.append_item(&item)?;
        commands.insert(login_id, TrayCommand::Login(account.id));
    } else {
        let item = MenuItem::with_id(MenuId::new(&id), label, true, None);
        menu.append_item(&item)?;
        commands.insert(id, TrayCommand::Activate(account.id));
    }
    Ok(())
}

fn format_switch_account_label(
    account: &AccountView,
    is_active: bool,
    weekly_only: bool,
) -> String {
    let name = account_display_name(account);
    let prefix = if is_active { "\u{2713} " } else { "" };
    let summary = format_account_quota_summary(account, weekly_only);
    if summary.is_empty() {
        format!("{prefix}{name}")
    } else {
        format!("{prefix}{name} · {summary}")
    }
}

fn format_account_quota_summary(account: &AccountView, weekly_only: bool) -> String {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        return "Login required".to_owned();
    }
    let Some(usage) = account.usage.as_ref() else {
        return String::new();
    };
    let now = OffsetDateTime::now_utc();

    if !weekly_only
        && let Some(five_hour) = &usage.five_hour
        && five_hour.reset_at > now
    {
        let at = crate::time_display::format_local_reset_at(five_hour.reset_at);
        if five_hour.remaining_percent == 0 {
            return format!(
                "Session 0% · reset {at} ({})",
                crate::time_display::format_countdown(five_hour.reset_at, now)
            );
        }
        return format!("Session {}% · reset {at}", five_hour.remaining_percent);
    }

    if let Some(weekly) = &usage.weekly {
        let at = crate::time_display::format_local_reset_at(weekly.reset_at);
        if weekly.reset_at <= now {
            return format!("Weekly {QUOTA_PAST_RESET_LABEL} · {at}");
        }
        if weekly.remaining_percent == 0 {
            return format!(
                "Weekly 0% · reset {at} ({})",
                crate::time_display::format_countdown(weekly.reset_at, now)
            );
        }
        return format!("Weekly {}% · reset {at}", weekly.remaining_percent);
    }

    if usage.has_stale_quota_cache(now) {
        return QUOTA_PAST_RESET_LABEL.to_owned();
    }
    String::new()
}

fn find_active_tray_account<'a>(
    current_account: Option<&DisplayIdentity>,
    current_saved_id: Option<Uuid>,
    accounts: &'a [AccountView],
) -> Option<&'a AccountView> {
    if let Some(account) =
        current_saved_id.and_then(|id| accounts.iter().find(|account| account.id == id))
    {
        return Some(account);
    }
    if let Some(current) = current_account {
        if let Some(account) = accounts
            .iter()
            .find(|account| account.is_active && account_matches_identity(account, current))
        {
            return Some(account);
        }
        // Claude live identity is often the placeholder email — still show quota
        // from the sole / most-recent Claude saved account with cached usage.
        if current
            .email
            .eq_ignore_ascii_case(crate::claude::CLAUDE_UNKNOWN_EMAIL)
        {
            return fallback_unresolved_claude_tray_account(accounts, current);
        }
    }
    None
}

fn fallback_unresolved_claude_tray_account<'a>(
    accounts: &'a [AccountView],
    identity: &DisplayIdentity,
) -> Option<&'a AccountView> {
    let mut candidates: Vec<&'a AccountView> = accounts
        .iter()
        .filter(|account| {
            account.target_app.as_deref() == Some("claude") && !account.is_archived
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if let Some(plan) = identity.plan_label.as_deref() {
        let plan_matches: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|account| {
                account
                    .plan_label
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(plan))
            })
            .collect();
        if !plan_matches.is_empty() {
            candidates = plan_matches;
        }
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }
    candidates.into_iter().max_by_key(|account| {
        (
            account
                .last_activated_at
                .map(|t| t.unix_timestamp_nanos())
                .unwrap_or(0),
            account.updated_at.unix_timestamp_nanos(),
        )
    })
}

fn account_matches_identity(account: &AccountView, identity: &DisplayIdentity) -> bool {
    DisplayIdentity {
        email: account.email.clone(),
        subject: account.subject.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
        workspace_id: account.workspace_id.clone(),
        workspace_name: account.workspace_name.clone(),
    }
    .matches(identity)
}

fn account_display_name(account: &AccountView) -> String {
    display_name_with_workspace(&account.email, account.workspace_label())
}

fn identity_display_name(identity: &DisplayIdentity) -> String {
    display_name_with_workspace(&identity.email, identity.workspace_label())
}

/// Shorten email for compact display: strip common suffixes.
fn compact_email(email: &str) -> &str {
    email
        .strip_suffix("@gmail.com")
        .or_else(|| email.strip_suffix("@googlemail.com"))
        .unwrap_or(email)
}

fn display_name_with_workspace(email: &str, workspace: Option<&str>) -> String {
    let short = compact_email(email);
    workspace
        .map(|workspace| format!("{short} ({workspace})"))
        .unwrap_or_else(|| short.to_owned())
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

/// CodexBar-style compact 3-glyph usage bar: `▮▮▯ 83%`
#[allow(dead_code)]
fn format_quota_bar(percent: u8) -> String {
    const BAR_WIDTH: usize = 3;
    let filled = ((f64::from(percent) / 100.0) * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let empty = BAR_WIDTH.saturating_sub(filled);
    format!(
        "{}{} {percent}%",
        "\u{25ae}".repeat(filled),
        "\u{25af}".repeat(empty),
    )
}

/// CodexBar-style multi-line usage card for the active account.
/// Codex (`weekly_only`): Weekly / Resets / Pace only — no session/5h.
fn format_usage_menu_lines(
    usage: Option<&crate::model::AccountUsageView>,
    weekly_only: bool,
    lang: ResolvedUiLanguage,
) -> Vec<String> {
    let Some(usage) = usage else {
        return Vec::new();
    };
    let now = OffsetDateTime::now_utc();
    let mut lines = Vec::new();
    let vi = matches!(lang, ResolvedUiLanguage::Vi);

    if !weekly_only
        && let Some(five_hour) = &usage.five_hour
        && five_hour.reset_at > now
    {
        let at = crate::time_display::format_local_reset_at(five_hour.reset_at);
        let in_cd = crate::time_display::format_countdown(five_hour.reset_at, now);
        if five_hour.remaining_percent == 0 {
            lines.push(if vi {
                format!("Phiên 0% · làm mới {at} ({in_cd})")
            } else {
                format!("Session 0% · reset {at} ({in_cd})")
            });
        } else {
            lines.push(if vi {
                format!("Phiên còn {}%", five_hour.remaining_percent)
            } else {
                format!("Session {}% left", five_hour.remaining_percent)
            });
            lines.push(if vi {
                format!("Làm mới {at} · còn {in_cd}")
            } else {
                format!("Reset {at} · in {in_cd}")
            });
        }
    }

    if let Some(weekly) = &usage.weekly {
        let at = crate::time_display::format_local_reset_at(weekly.reset_at);
        if weekly.reset_at <= now {
            lines.push(if vi {
                format!("Tuần: đã qua mốc làm mới · {at}")
            } else {
                format!("Weekly {QUOTA_PAST_RESET_LABEL} · {at}")
            });
        } else if weekly.remaining_percent == 0 {
            let in_cd = crate::time_display::format_countdown(weekly.reset_at, now);
            lines.push(if vi {
                format!("Tuần 0% · làm mới {at} ({in_cd})")
            } else {
                format!("Weekly 0% · reset {at} ({in_cd})")
            });
        } else {
            let in_cd = crate::time_display::format_countdown(weekly.reset_at, now);
            lines.push(if vi {
                format!("Tuần còn {}%", weekly.remaining_percent)
            } else {
                format!("Weekly {}% left", weekly.remaining_percent)
            });
            lines.push(if vi {
                format!("Làm mới {at} · còn {in_cd}")
            } else {
                format!("Reset {at} · in {in_cd}")
            });
            if let Some(pace) =
                crate::usage_pace::pace_for_weekly(weekly.used_percent, weekly.reset_at, now)
            {
                lines.push(pace.summary_label_localized(vi));
                if let Some(eta) = pace.eta_label_localized(now, vi) {
                    lines.push(eta);
                }
            }
        }
    } else if usage.has_stale_quota_cache(now) && (weekly_only || usage.five_hour.is_none()) {
        lines.push(if vi {
            "Đã qua mốc làm mới".to_owned()
        } else {
            QUOTA_PAST_RESET_LABEL.to_owned()
        });
    }

    lines
}

/// Compact one-line summary kept for tooltips / legacy helpers.
#[allow(dead_code)]
fn format_account_details_line(
    plan: &str,
    usage: Option<&crate::model::AccountUsageView>,
    weekly_only: bool,
    lang: ResolvedUiLanguage,
) -> String {
    let mut parts = vec![plan.to_owned()];
    parts.extend(format_usage_menu_lines(usage, weekly_only, lang));
    parts.join(" · ")
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
        let now = OffsetDateTime::now_utc();
        if weekly.reset_at <= now {
            (QUOTA_PAST_RESET_LABEL.to_owned(), String::new())
        } else {
            (
                format!(
                    "{}% remaining",
                    format_remaining_percent(weekly.remaining_percent).trim()
                ),
                format!(
                    "Reset in: {}",
                    crate::time_display::format_countdown(weekly.reset_at, now)
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

/// Compact status tag for saved account list: Ready / Low / Depleted / Login / Stale
#[allow(dead_code)]
fn account_status_tag(account: &AccountView) -> String {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        return "Login".to_owned();
    }
    if let Some(usage) = &account.usage {
        let now = OffsetDateTime::now_utc();
        if usage.is_out_of_quota(now) {
            return "Depleted".to_owned();
        }
        if usage.is_near_limit(now, 10) {
            return "Low".to_owned();
        }
        return "Ready".to_owned();
    }
    if account.usage_error.is_some() {
        "Stale".to_owned()
    } else {
        "—".to_owned()
    }
}

/// Verbose status label (used in tooltip and legacy paths).
fn account_status_label_simple(account: &AccountView) -> String {
    let prefix = if account.is_active { "Active / " } else { "" };
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        return format!("{prefix}Login required");
    }
    if let Some(usage) = &account.usage {
        let now = OffsetDateTime::now_utc();
        if usage.is_out_of_quota(now) {
            return format!("{prefix}Quota depleted");
        }
        if usage.is_near_limit(now, 10) {
            return format!("{prefix}Low quota");
        }
        return format!("{prefix}Ready");
    }
    if account.usage_error.is_some() {
        format!("{prefix}Usage stale")
    } else {
        format!("{prefix}No usage")
    }
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
                    "ChatGPT Codex".to_owned(),
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
    _file: fs::File,
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
                .truncate(false)
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
            Ok(Some(Self { _file: file }))
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

fn wry_bounds_from_physical(size: PhysicalSize<u32>) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
        size: wry::dpi::PhysicalSize::new(size.width, size.height).into(),
    }
}

fn read_tray_lock_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse().ok())
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

#[cfg(target_os = "windows")]
fn release_console() {
    use windows_sys::Win32::System::Console::FreeConsole;

    unsafe {
        FreeConsole();
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
    use crate::model::{AccountUsageView, EnvironmentKind, UsageSource, UsageWindowView};
    use time::OffsetDateTime;

    #[test]
    fn test_format_plan_label_simple() {
        assert_eq!(format_plan_label_simple(Some("pro")), "pro");
        assert_eq!(format_plan_label_simple(Some("Free")), "Free");
        assert_eq!(format_plan_label_simple(None), "Free");
    }

    #[test]
    fn remaining_percent_uses_fixed_width_visual_slot() {
        assert_eq!(format_remaining_percent(2), "\u{2007}\u{2007}2");
        assert_eq!(format_remaining_percent(89), "\u{2007}89");
        assert_eq!(format_remaining_percent(100), "100");
    }

    #[test]
    fn tray_status_label_surfaces_account_health() {
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "low@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            environment: EnvironmentKind::Windows,
            is_active: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: Some(UsageWindowView {
                    used_percent: 96,
                    remaining_percent: 4,
                    reset_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                }),
                weekly: None,
                credits: None,
            }),
            usage_error: None,
            label: None,
            is_archived: false,
        };

        assert_eq!(account_status_label_simple(&account), "Low quota");
    }

    #[test]
    fn tray_saved_accounts_keeps_active_flag_without_rendered_active_id() {
        let active = AccountView {
            id: Uuid::new_v4(),
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            environment: EnvironmentKind::Windows,
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
            label: None,
            is_archived: false,
        };
        let inactive = AccountView {
            id: Uuid::new_v4(),
            email: "inactive@example.com".to_owned(),
            is_active: false,
            ..active.clone()
        };
        let accounts = vec![active, inactive];

        let saved_accounts = tray_saved_accounts(&accounts);

        assert_eq!(saved_accounts.len(), 2);
        assert_eq!(saved_accounts[0].email, "active@example.com");
        assert_eq!(saved_accounts[1].email, "inactive@example.com");
    }

    #[test]
    fn tray_status_label_marks_active_account() {
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            environment: EnvironmentKind::Windows,
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: None,
                credits: None,
            }),
            usage_error: None,
            label: None,
            is_archived: false,
        };

        assert_eq!(account_status_label_simple(&account), "Active / Ready");
    }

    #[test]
    fn active_account_fallback_requires_live_identity_match() {
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            environment: EnvironmentKind::Windows,
            is_active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
            label: None,
            is_archived: false,
        };
        let matching_identity = DisplayIdentity {
            email: "active@example.com".to_owned(),
            subject: Some("sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let mismatched_identity = DisplayIdentity {
            email: "other@example.com".to_owned(),
            subject: Some("other-sub".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let accounts = vec![account];

        assert!(find_active_tray_account(Some(&matching_identity), None, &accounts).is_some());
        assert!(find_active_tray_account(Some(&mismatched_identity), None, &accounts).is_none());
    }

    #[test]
    fn quota_bar_renders_correct_fill() {
        assert_eq!(format_quota_bar(100), "▮▮▮ 100%");
        assert_eq!(format_quota_bar(0), "▯▯▯ 0%");
        assert_eq!(format_quota_bar(50), "▮▮▯ 50%");
        assert_eq!(format_quota_bar(83), "▮▮▯ 83%");
        assert_eq!(format_quota_bar(17), "▮▯▯ 17%");
    }

    #[test]
    fn usage_menu_lines_are_codexbar_style_weekly_only_for_codex() {
        let now = OffsetDateTime::now_utc();
        let usage = AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: now,
            five_hour: Some(UsageWindowView {
                used_percent: 40,
                remaining_percent: 60,
                reset_at: now + time::Duration::hours(2),
            }),
            weekly: Some(UsageWindowView {
                used_percent: 16,
                remaining_percent: 84,
                reset_at: now + time::Duration::days(3),
            }),
            credits: None,
        };

        let lines = format_usage_menu_lines(Some(&usage), true, ResolvedUiLanguage::En);
        assert_eq!(lines[0], "Weekly 84% left");
        assert!(lines[1].starts_with("Reset "));
        assert!(lines[1].contains(" · in "));
        assert!(!lines.iter().any(|line| line.contains("Session")));
    }

    #[test]
    fn usage_menu_lines_include_session_when_not_weekly_only() {
        let now = OffsetDateTime::now_utc();
        let usage = AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: now,
            five_hour: Some(UsageWindowView {
                used_percent: 40,
                remaining_percent: 60,
                reset_at: now + time::Duration::hours(2),
            }),
            weekly: Some(UsageWindowView {
                used_percent: 16,
                remaining_percent: 84,
                reset_at: now + time::Duration::days(3),
            }),
            credits: None,
        };

        let lines = format_usage_menu_lines(Some(&usage), false, ResolvedUiLanguage::En);
        assert_eq!(lines[0], "Session 60% left");
        assert!(lines.iter().any(|line| line == "Weekly 84% left"));
    }

    #[test]
    fn switch_account_label_is_single_compact_line() {
        let now = OffsetDateTime::now_utc();
        let account = AccountView {
            id: Uuid::new_v4(),
            email: "person@gmail.com".to_owned(),
            subject: None,
            name: None,
            plan_label: Some("Plus".to_owned()),
            workspace_id: None,
            workspace_name: None,
            target_app: Some("codex".to_owned()),
            environment: EnvironmentKind::Macos,
            is_active: false,
            created_at: now,
            updated_at: now,
            last_activated_at: None,
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: now,
                five_hour: Some(UsageWindowView {
                    used_percent: 90,
                    remaining_percent: 10,
                    reset_at: now + time::Duration::hours(1),
                }),
                weekly: Some(UsageWindowView {
                    used_percent: 20,
                    remaining_percent: 80,
                    reset_at: now + time::Duration::days(2),
                }),
                credits: None,
            }),
            usage_error: None,
            label: None,
            is_archived: false,
        };

        let label = format_switch_account_label(&account, false, true);
        assert!(
            label.starts_with("person · Weekly 80% · reset "),
            "unexpected switch label: {label}"
        );
    }

    #[test]
    fn status_tag_is_compact() {
        let base = AccountView {
            id: Uuid::new_v4(),
            email: "a@b.com".to_owned(),
            subject: None,
            name: None,
            plan_label: None,
            workspace_id: None,
            workspace_name: None,
            target_app: None,
            environment: EnvironmentKind::Windows,
            is_active: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: None,
            usage_error: None,
            label: None,
            is_archived: false,
        };

        // No usage → dash
        assert_eq!(account_status_tag(&base), "\u{2014}");

        // With healthy usage → Ready
        let ready = AccountView {
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: Some(UsageWindowView {
                    used_percent: 20,
                    remaining_percent: 80,
                    reset_at: OffsetDateTime::now_utc() + time::Duration::days(3),
                }),
                credits: None,
            }),
            ..base.clone()
        };
        assert_eq!(account_status_tag(&ready), "Ready");

        // Low quota → Low
        let low = AccountView {
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: Some(UsageWindowView {
                    used_percent: 95,
                    remaining_percent: 5,
                    reset_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                }),
                weekly: None,
                credits: None,
            }),
            ..base.clone()
        };
        assert_eq!(account_status_tag(&low), "Low");
    }
}
