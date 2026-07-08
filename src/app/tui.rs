use anyhow::{Context, Error, Result};
use console::{Key, Term, style};
use dialoguer::{Input, Select, theme::ColorfulTheme};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::model::{
    AUTO_REFRESH_QUOTA_ON_RESET_LABEL, AccountView, AutoStartUsageWindowsRunOutput,
    BatchRefreshOutput, ListOutput, PickBestOutput, QUOTA_PAST_RESET_LABEL, RunningCodexProcess,
    SaveAction, StatusOutput, SwitchWhenRunning,
};
use crate::process::format_process_table;
use crate::secrets::SecretStore;
use crate::time_display::format_local_reset_at;
use crate::usage::{usage_error_label, usage_error_requires_login};

use super::{App, InteractiveExit, InteractiveMode, account_view_matches_identity};

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn interactive(
        &self,
        mode: InteractiveMode,
        force_running: bool,
    ) -> Result<InteractiveExit> {
        let mut default_selection = 0usize;
        if matches!(mode, InteractiveMode::Persistent) {
            self.refresh_saved_usage_cache()?;
        }
        let mut feedback = Vec::new();
        loop {
            let status = self.status()?;
            let list = self.list()?;
            let current_saved = status.current_account.as_ref().and_then(|identity| {
                list.accounts
                    .iter()
                    .find(|account| account_view_matches_identity(account, identity))
                    .map(|account| account.id)
            });
            let auto_start_usage_windows_enabled = matches!(mode, InteractiveMode::Persistent)
                && self.auto_start_usage_windows_status()?.enabled;
            let auto_switch_on_limit_enabled = matches!(mode, InteractiveMode::Persistent)
                && self.auto_switch_on_limit_status()?.enabled;
            let launch_at_startup_enabled = matches!(mode, InteractiveMode::Persistent)
                && self.launch_at_startup_status()?.enabled;
            let show_quota_in_menu_bar_enabled = matches!(mode, InteractiveMode::Persistent)
                && self.show_quota_in_menu_bar_status()?.enabled;

            let menu = build_menu(
                mode,
                &status,
                &list,
                current_saved,
                auto_start_usage_windows_enabled,
                auto_switch_on_limit_enabled,
                launch_at_startup_enabled,
                show_quota_in_menu_bar_enabled,
            );
            let selection = match mode {
                InteractiveMode::Persistent => {
                    select_persistent_entry(&menu, default_selection, &feedback)?
                }
                InteractiveMode::ActivateOnce | InteractiveMode::DeleteOnce => {
                    let labels = menu.labels();
                    let theme = ColorfulTheme::default();
                    let mut select = Select::with_theme(&theme);
                    if !menu.prompt.is_empty() {
                        select = select.with_prompt(menu.prompt);
                    }
                    select
                        .items(&labels)
                        .default(default_selection.min(menu.len().saturating_sub(1)))
                        .interact()?
                }
            };
            default_selection = selection;
            feedback.clear();

            match menu.action(selection) {
                InteractiveAction::SaveCurrent => {
                    let output = match self.save_current() {
                        Ok(output) => output,
                        Err(error) => {
                            if matches!(mode, InteractiveMode::Persistent) {
                                feedback =
                                    error_feedback("Saving the current account failed.", error);
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    feedback.push(format!(
                        "{} {} ({})",
                        match output.action {
                            SaveAction::Created => "Saved",
                            SaveAction::Refreshed => "Refreshed",
                        },
                        output.account.email,
                        output.account.id
                    ));
                }
                InteractiveAction::Activate(account_id) => {
                    let warnings = self.activation_preflight_warnings();
                    let was_running = !warnings.is_empty();
                    let force = if was_running {
                        match prompt_switch_policy(
                            "Switch account while Codex is running?",
                            &warnings,
                        )? {
                            SwitchWhenRunning::Cancel => continue,
                            SwitchWhenRunning::SwitchNow => true,
                            SwitchWhenRunning::WaitAndSwitch => {
                                crate::process::wait_for_codex_processes_to_exit();
                                false
                            }
                        }
                    } else {
                        force_running
                    };
                    let output = match self.activate_with_running_policy(account_id, force) {
                        Ok(output) => output,
                        Err(error) => {
                            if matches!(mode, InteractiveMode::Persistent) {
                                let rendered_error = format!("{error:#}");
                                feedback = error_feedback_rendered(
                                    "Account activation failed.",
                                    &rendered_error,
                                );
                                if was_running
                                    && force
                                    && error_indicates_running_process_instability(&rendered_error)
                                {
                                    feedback.push(
                                        "Codex was still running during activation. Close those processes fully and retry."
                                            .to_owned(),
                                    );
                                }
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    feedback.push(format!(
                        "Activated {} ({})",
                        output.account.email, output.account.id
                    ));
                    if was_running && force {
                        feedback.push(
                            "Codex was still running during activation. If the account does not change in Codex, close those processes fully and retry."
                                .to_owned(),
                        );
                    }
                    if !was_running && !output.warnings.is_empty() {
                        feedback.extend(process_summary_lines("Codex processes", &output.warnings));
                    }
                    if matches!(mode, InteractiveMode::ActivateOnce) {
                        break;
                    }
                }
                InteractiveAction::Delete(account_id) => {
                    let account = list
                        .accounts
                        .iter()
                        .find(|account| account.id == account_id)
                        .context("selected account no longer exists")?;
                    if !confirm_delete(account)? {
                        continue;
                    }
                    let output = match self.delete(account_id) {
                        Ok(output) => output,
                        Err(error) => {
                            if matches!(mode, InteractiveMode::Persistent) {
                                feedback =
                                    error_feedback("Deleting the saved account failed.", error);
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    feedback.push(format!(
                        "Deleted saved snapshot {}",
                        output.deleted_account_id
                    ));
                    if matches!(mode, InteractiveMode::DeleteOnce) {
                        break;
                    }
                }
                InteractiveAction::DeletePrompt => {
                    let account_id = prompt_for_account_delete(&list.accounts)?;
                    let account = list
                        .accounts
                        .iter()
                        .find(|account| account.id == account_id)
                        .context("selected account no longer exists")?;
                    if !confirm_delete(account)? {
                        continue;
                    }
                    let output = match self.delete(account_id) {
                        Ok(output) => output,
                        Err(error) => {
                            if matches!(mode, InteractiveMode::Persistent) {
                                feedback =
                                    error_feedback("Deleting the saved account failed.", error);
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    feedback.push(format!(
                        "Deleted saved snapshot {}",
                        output.deleted_account_id
                    ));
                }
                InteractiveAction::RefreshAllUsage => match self.refresh_all_usage() {
                    Ok(output) => feedback.extend(batch_refresh_feedback(&output)),
                    Err(error) => {
                        feedback = error_feedback("Refreshing saved account usage failed.", error);
                        continue;
                    }
                },
                InteractiveAction::PickBestQuota => {
                    let warnings = self.activation_preflight_warnings();
                    let was_running = !warnings.is_empty();
                    let forced_switch = if was_running {
                        match prompt_switch_policy(
                            "Switch to best quota while Codex is running?",
                            &warnings,
                        )? {
                            SwitchWhenRunning::Cancel => continue,
                            SwitchWhenRunning::SwitchNow => true,
                            SwitchWhenRunning::WaitAndSwitch => {
                                crate::process::wait_for_codex_processes_to_exit();
                                false
                            }
                        }
                    } else {
                        false
                    };
                    match self.pick_best_account(true, true) {
                        Ok(output) => {
                            feedback.extend(pick_best_feedback(&output));
                            if forced_switch && output.switched {
                                feedback.push(
                                    "Codex was still running during the switch. Close those processes fully if the account does not change in Codex."
                                        .to_owned(),
                                );
                            }
                        }
                        Err(error) => {
                            feedback =
                                error_feedback("Switching to best quota account failed.", error);
                            continue;
                        }
                    }
                }
                InteractiveAction::RenamePrompt => {
                    let account_id = prompt_for_account_rename(&list.accounts)?;
                    let account = list
                        .accounts
                        .iter()
                        .find(|account| account.id == account_id)
                        .context("selected account no longer exists")?;
                    let label = prompt_for_account_label(account)?;
                    match self.rename_account(account_id, label) {
                        Ok(output) => match &output.account.label {
                            Some(name) => feedback.push(format!(
                                "Renamed {} to label \"{name}\"",
                                output.account.email
                            )),
                            None => {
                                feedback.push(format!("Cleared label for {}", output.account.email))
                            }
                        },
                        Err(error) => {
                            feedback = error_feedback("Renaming the saved account failed.", error);
                            continue;
                        }
                    }
                }
                InteractiveAction::ShowStatus => {
                    feedback = interactive_status_lines(&status);
                }
                InteractiveAction::SetAutoStartUsageWindows(enabled) => {
                    match self.set_auto_start_usage_windows(enabled) {
                        Ok(output) => feedback.push(format!(
                            "{} {}.",
                            AUTO_REFRESH_QUOTA_ON_RESET_LABEL,
                            if output.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        )),
                        Err(error) => {
                            feedback = error_feedback(
                                &format!("Updating {AUTO_REFRESH_QUOTA_ON_RESET_LABEL} failed."),
                                error,
                            );
                            continue;
                        }
                    }
                    if enabled {
                        match self.auto_start_usage_windows_once(true) {
                            Ok(output) => {
                                feedback.extend(auto_start_usage_window_feedback(&output))
                            }
                            Err(error) => {
                                feedback =
                                    error_feedback("Weekly quota refresh check failed.", error);
                            }
                        }
                    }
                }
                InteractiveAction::SetAutoSwitchOnLimit(enabled) => {
                    match self.set_auto_switch_on_limit(enabled) {
                        Ok(output) => feedback.push(format!(
                            "Auto-switch on limit {}.",
                            if output.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        )),
                        Err(error) => {
                            feedback =
                                error_feedback("Updating auto-switch on limit failed.", error);
                            continue;
                        }
                    }
                    if enabled {
                        match crate::app::run_auto_start_usage_windows_check_now(self.env().clone())
                        {
                            Ok(()) => feedback.push("Quota check completed.".to_owned()),
                            Err(error) => {
                                feedback =
                                    error_feedback("Initial auto-switch check failed.", error);
                            }
                        }
                    }
                }
                InteractiveAction::SetLaunchAtStartup(enabled) => {
                    match self.set_launch_at_startup(enabled) {
                        Ok(output) => feedback.push(format!(
                            "Launch at startup {}.",
                            if output.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        )),
                        Err(error) => {
                            feedback = error_feedback("Updating launch at startup failed.", error);
                            continue;
                        }
                    }
                }
                InteractiveAction::SetShowQuotaInMenuBar(enabled) => {
                    match self.set_show_quota_in_menu_bar(enabled) {
                        Ok(output) => feedback.push(format!(
                            "Show quota in menu bar {}.",
                            if output.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        )),
                        Err(error) => {
                            feedback =
                                error_feedback("Updating show quota in menu bar failed.", error);
                            continue;
                        }
                    }
                }
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                InteractiveAction::SendToTray => return Ok(InteractiveExit::SendToTray),
                InteractiveAction::Quit => break,
            }
        }
        Ok(InteractiveExit::Quit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteractiveAction {
    SaveCurrent,
    Activate(Uuid),
    Delete(Uuid),
    DeletePrompt,
    RefreshAllUsage,
    PickBestQuota,
    RenamePrompt,
    ShowStatus,
    SetAutoStartUsageWindows(bool),
    SetAutoSwitchOnLimit(bool),
    SetLaunchAtStartup(bool),
    SetShowQuotaInMenuBar(bool),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    SendToTray,
    Quit,
}

pub(crate) struct InteractiveItem {
    pub(crate) label: String,
    pub(crate) action: InteractiveAction,
    pub(crate) tone: MenuLabelTone,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MenuLabelTone {
    #[default]
    Normal,
    Active,
    Warning,
    Critical,
    LoginRequired,
}

pub(crate) struct InteractiveMenu {
    pub(crate) prompt: &'static str,
    pub(crate) environment: String,
    pub(crate) saved_account_count: usize,
    pub(crate) process_warning_count: usize,
    pub(crate) current_status_label: Option<String>,
    pub(crate) current_status_tone: MenuLabelTone,
    pub(crate) active_count: usize,
    pub(crate) depleted_count: usize,
    pub(crate) login_count: usize,
    pub(crate) accounts: Vec<InteractiveItem>,
    pub(crate) actions: Vec<InteractiveItem>,
}

struct PersistentRenderState {
    total_lines: usize,
    row_lines: Vec<usize>,
}

#[derive(Clone, Copy, Default)]
struct AccountLabelWidths {
    email: usize,
    plan: usize,
    status: usize,
    remaining: usize,
    reset: usize,
}

impl InteractiveMenu {
    pub(crate) fn len(&self) -> usize {
        self.accounts.len() + self.actions.len()
    }

    fn labels(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .chain(self.actions.iter())
            .map(|item| item.label.as_str())
            .collect()
    }

    pub(crate) fn action(&self, index: usize) -> InteractiveAction {
        if index < self.accounts.len() {
            self.accounts[index].action
        } else {
            self.actions[index - self.accounts.len()].action
        }
    }

    fn label(&self, index: usize) -> &str {
        if index < self.accounts.len() {
            &self.accounts[index].label
        } else {
            &self.actions[index - self.accounts.len()].label
        }
    }

    fn tone(&self, index: usize) -> MenuLabelTone {
        if index < self.accounts.len() {
            self.accounts[index].tone
        } else {
            self.actions[index - self.accounts.len()].tone
        }
    }

    fn first_action_index(&self) -> Option<usize> {
        (!self.actions.is_empty()).then_some(self.accounts.len())
    }
}

fn account_menu_tone(account: &AccountView) -> MenuLabelTone {
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        return MenuLabelTone::LoginRequired;
    }
    if let Some(usage) = &account.usage {
        let now = OffsetDateTime::now_utc();
        if usage.is_out_of_quota(now) {
            return MenuLabelTone::Critical;
        }
        let lowest = [usage.five_hour.as_ref(), usage.weekly.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| window.reset_at > now)
            .map(|window| window.remaining_percent)
            .min();
        if lowest.is_some_and(|percent| percent <= 10) {
            return MenuLabelTone::Critical;
        }
        if lowest.is_some_and(|percent| percent <= 25) {
            return MenuLabelTone::Warning;
        }
    }
    if account.is_active {
        MenuLabelTone::Active
    } else {
        MenuLabelTone::Normal
    }
}

fn render_account_label(account: &AccountView, widths: AccountLabelWidths) -> String {
    let display_email = account_display_name(account);
    let email = format!("{:<width$}", display_email, width = widths.email);
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
        account_status_label(account),
        width = widths.status
    );

    let (remaining, reset) = if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        (
            usage_error_label(account.usage_error.as_deref().unwrap_or_default()).to_owned(),
            String::new(),
        )
    } else if let Some(usage) = &account.usage {
        let now = OffsetDateTime::now_utc();
        let mut parts = Vec::new();
        let mut reset_value = String::new();
        if let Some(five_hour) = &usage.five_hour
            && five_hour.reset_at > now
        {
            parts.push(format!("5h Remaining: {}%", five_hour.remaining_percent));
        }
        if let Some(weekly) = &usage.weekly {
            if weekly.reset_at <= now {
                parts.push(format!("Weekly: {QUOTA_PAST_RESET_LABEL}"));
            } else {
                parts.push(format!("Weekly Remaining: {}%", weekly.remaining_percent));
                reset_value = format!("Reset: {}", format_local_reset_at(weekly.reset_at));
            }
        }
        (parts.join("  "), reset_value)
    } else if let Some(error) = &account.usage_error {
        (usage_error_label(error).to_owned(), String::new())
    } else {
        (String::new(), String::new())
    };
    let remaining = format!("{:<width$}", remaining, width = widths.remaining);
    let reset = format!("{:<width$}", reset, width = widths.reset);

    [email, plan, status, remaining, reset]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

fn account_status_label(account: &AccountView) -> String {
    let mut parts = Vec::new();
    if account.is_active {
        parts.push("Active");
    }
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        parts.push("Login required");
    } else if let Some(usage) = &account.usage {
        let now = OffsetDateTime::now_utc();
        if usage.is_out_of_quota(now) {
            parts.push("Quota depleted");
        } else {
            let lowest = [usage.five_hour.as_ref(), usage.weekly.as_ref()]
                .into_iter()
                .flatten()
                .filter(|window| window.reset_at > now)
                .map(|window| window.remaining_percent)
                .min();
            if lowest.is_some_and(|percent| percent <= 10) {
                parts.push("Low quota");
            } else if lowest.is_some_and(|percent| percent <= 25) {
                parts.push("Watch quota");
            } else {
                parts.push("Ready");
            }
        }
    } else if account.usage_error.is_some() {
        parts.push("Usage stale");
    } else {
        parts.push("No usage");
    }
    parts.join(" / ")
}

fn account_label_widths(accounts: &[&AccountView]) -> AccountLabelWidths {
    let mut widths = AccountLabelWidths::default();
    for account in accounts {
        let display_email = account_display_name(account);
        widths.email = widths.email.max(display_email.len());
        widths.plan = widths.plan.max(
            account
                .plan_label
                .as_ref()
                .map(|plan| format!("Plan: {plan}").len())
                .unwrap_or(0),
        );
        widths.status = widths.status.max(account_status_label(account).len());
        let (remaining, reset) = if account
            .usage_error
            .as_deref()
            .is_some_and(usage_error_requires_login)
        {
            (
                usage_error_label(account.usage_error.as_deref().unwrap_or_default()).len(),
                0,
            )
        } else if let Some(usage) = &account.usage {
            let now = OffsetDateTime::now_utc();
            let mut remaining = 0;
            let mut reset = 0;
            if let Some(five_hour) = &usage.five_hour
                && five_hour.reset_at > now
            {
                remaining =
                    remaining.max(format!("5h Remaining: {}%", five_hour.remaining_percent).len());
            }
            if let Some(weekly) = &usage.weekly {
                if weekly.reset_at <= now {
                    remaining = remaining.max(format!("Weekly: {QUOTA_PAST_RESET_LABEL}").len());
                } else {
                    remaining = remaining
                        .max(format!("Weekly Remaining: {}%", weekly.remaining_percent).len());
                    reset = format!("Reset: {}", format_local_reset_at(weekly.reset_at)).len();
                }
            }
            (remaining, reset)
        } else if let Some(error) = &account.usage_error {
            (usage_error_label(error).len(), 0)
        } else {
            (0, 0)
        };
        widths.remaining = widths.remaining.max(remaining);
        widths.reset = widths.reset.max(reset);
    }
    widths
}

fn account_display_name(account: &AccountView) -> String {
    let label = account
        .label
        .as_deref()
        .map(|label| format!(" [{label}]"))
        .unwrap_or_default();
    let workspace = account
        .workspace_label()
        .map(|workspace| format!(" ({workspace})"))
        .unwrap_or_default();
    format!("{}{}{}", account.email, label, workspace)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_menu(
    mode: InteractiveMode,
    status: &StatusOutput,
    list: &ListOutput,
    current_saved: Option<Uuid>,
    auto_start_usage_windows_enabled: bool,
    auto_switch_on_limit_enabled: bool,
    launch_at_startup_enabled: bool,
    show_quota_in_menu_bar_enabled: bool,
) -> InteractiveMenu {
    let active_account = current_saved
        .and_then(|saved_id| list.accounts.iter().find(|account| account.id == saved_id));
    let mut label_accounts = list.accounts.iter().collect::<Vec<_>>();
    if let Some(active_account) = active_account
        && !label_accounts
            .iter()
            .any(|account| account.id == active_account.id)
    {
        label_accounts.push(active_account);
    }
    let widths = account_label_widths(&label_accounts);

    let mut active_accounts = Vec::new();
    let mut depleted_accounts = Vec::new();
    let mut login_accounts = Vec::new();

    for account in list.accounts.iter().filter(|account| {
        !matches!(
            mode,
            InteractiveMode::Persistent | InteractiveMode::ActivateOnce
        ) || !account.is_active
    }) {
        let item = InteractiveItem {
            label: render_account_label(account, widths),
            action: match mode {
                InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
                    InteractiveAction::Activate(account.id)
                }
                InteractiveMode::DeleteOnce => InteractiveAction::Delete(account.id),
            },
            tone: account_menu_tone(account),
        };

        let needs_login = account
            .usage_error
            .as_deref()
            .is_some_and(usage_error_requires_login);
        if needs_login {
            login_accounts.push(item);
        } else if let Some(usage) = &account.usage
            && usage.is_out_of_quota(OffsetDateTime::now_utc())
        {
            depleted_accounts.push(item);
        } else {
            active_accounts.push(item);
        }
    }

    let active_count = active_accounts.len();
    let depleted_count = depleted_accounts.len();
    let login_count = login_accounts.len();

    let mut accounts = active_accounts;
    accounts.extend(depleted_accounts);
    accounts.extend(login_accounts);

    let mut actions = Vec::new();
    let current_status_tone = active_account
        .map(account_menu_tone)
        .unwrap_or(MenuLabelTone::Normal);
    let current_status_label = active_account
        .map(|account| render_account_label(account, widths))
        .or_else(|| {
            status.current_account.as_ref().map(|current| {
                format!(
                    "{}{}{}",
                    current.email,
                    current
                        .workspace_label()
                        .map(|workspace| format!(" ({workspace})"))
                        .unwrap_or_default(),
                    if current_saved.is_some() {
                        " [saved]"
                    } else {
                        " [not saved]"
                    }
                )
            })
        });

    if matches!(mode, InteractiveMode::Persistent) {
        if let Some(current) = status.current_account.as_ref() {
            actions.push(InteractiveItem {
                label: if current_saved.is_some() {
                    format!(
                        "Refresh saved snapshot for {}{}",
                        current.email,
                        current
                            .workspace_label()
                            .map(|workspace| format!(" ({workspace})"))
                            .unwrap_or_default()
                    )
                } else {
                    format!(
                        "Add current account {}{} to switcher",
                        current.email,
                        current
                            .workspace_label()
                            .map(|workspace| format!(" ({workspace})"))
                            .unwrap_or_default()
                    )
                },
                action: InteractiveAction::SaveCurrent,
                tone: MenuLabelTone::Normal,
            });
        }
        if !list.accounts.is_empty() {
            actions.push(InteractiveItem {
                label: "Refresh usage for all saved accounts".to_owned(),
                action: InteractiveAction::RefreshAllUsage,
                tone: MenuLabelTone::Normal,
            });
            actions.push(InteractiveItem {
                label: "Switch to best quota account".to_owned(),
                action: InteractiveAction::PickBestQuota,
                tone: MenuLabelTone::Normal,
            });
            actions.push(InteractiveItem {
                label: "Rename saved account label".to_owned(),
                action: InteractiveAction::RenamePrompt,
                tone: MenuLabelTone::Normal,
            });
            actions.push(InteractiveItem {
                label: "Delete saved account".to_owned(),
                action: InteractiveAction::DeletePrompt,
                tone: MenuLabelTone::Normal,
            });
        }
        actions.push(InteractiveItem {
            label: if auto_start_usage_windows_enabled {
                format!("Disable {AUTO_REFRESH_QUOTA_ON_RESET_LABEL}")
            } else {
                format!("Enable {AUTO_REFRESH_QUOTA_ON_RESET_LABEL}")
            },
            action: InteractiveAction::SetAutoStartUsageWindows(!auto_start_usage_windows_enabled),
            tone: MenuLabelTone::Normal,
        });
        actions.push(InteractiveItem {
            label: if auto_switch_on_limit_enabled {
                "Disable auto-switch on limit".to_owned()
            } else {
                "Enable auto-switch on limit".to_owned()
            },
            action: InteractiveAction::SetAutoSwitchOnLimit(!auto_switch_on_limit_enabled),
            tone: MenuLabelTone::Normal,
        });
        actions.push(InteractiveItem {
            label: if launch_at_startup_enabled {
                "Disable launch at startup".to_owned()
            } else {
                "Enable launch at startup".to_owned()
            },
            action: InteractiveAction::SetLaunchAtStartup(!launch_at_startup_enabled),
            tone: MenuLabelTone::Normal,
        });
        actions.push(InteractiveItem {
            label: if show_quota_in_menu_bar_enabled {
                "Disable show quota in menu bar".to_owned()
            } else {
                "Enable show quota in menu bar".to_owned()
            },
            action: InteractiveAction::SetShowQuotaInMenuBar(!show_quota_in_menu_bar_enabled),
            tone: MenuLabelTone::Normal,
        });
        actions.push(InteractiveItem {
            label: "Show status".to_owned(),
            action: InteractiveAction::ShowStatus,
            tone: MenuLabelTone::Normal,
        });
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        actions.push(InteractiveItem {
            label: if cfg!(target_os = "macos") {
                "Send to Menu Bar".to_owned()
            } else {
                "Send to Tray".to_owned()
            },
            action: InteractiveAction::SendToTray,
            tone: MenuLabelTone::Normal,
        });
    }
    actions.push(InteractiveItem {
        label: "Quit".to_owned(),
        action: InteractiveAction::Quit,
        tone: MenuLabelTone::Normal,
    });

    let prompt = match mode {
        InteractiveMode::Persistent | InteractiveMode::ActivateOnce => "",
        InteractiveMode::DeleteOnce => "Which saved account do you want to delete?",
    };

    InteractiveMenu {
        prompt,
        environment: status.environment.to_string(),
        saved_account_count: list.accounts.len(),
        process_warning_count: status.process_warnings.len(),
        current_status_label,
        current_status_tone,
        active_count,
        depleted_count,
        login_count,
        accounts,
        actions,
    }
}

fn select_persistent_entry(
    menu: &InteractiveMenu,
    default_selection: usize,
    feedback: &[String],
) -> Result<usize> {
    let term = Term::stderr();
    let mut selection = default_selection.min(menu.len().saturating_sub(1));
    term.hide_cursor()?;
    let render_state = render_persistent_menu(&term, menu, selection, feedback)?;
    loop {
        match term.read_key()? {
            Key::ArrowUp | Key::Char('k') => {
                let next = if selection == 0 {
                    menu.len().saturating_sub(1)
                } else {
                    selection - 1
                };
                update_persistent_selection(&term, menu, &render_state, selection, next)?;
                selection = next;
            }
            Key::ArrowDown | Key::Char('j') => {
                let next = (selection + 1) % menu.len().max(1);
                update_persistent_selection(&term, menu, &render_state, selection, next)?;
                selection = next;
            }
            Key::ArrowLeft | Key::ArrowRight | Key::Tab => {
                let next = jump_section(menu, selection);
                update_persistent_selection(&term, menu, &render_state, selection, next)?;
                selection = next;
            }
            Key::Enter => {
                if render_state.total_lines > 0 {
                    term.clear_last_lines(render_state.total_lines)?;
                }
                term.show_cursor()?;
                return Ok(selection);
            }
            Key::Escape | Key::Char('q') => {
                if render_state.total_lines > 0 {
                    term.clear_last_lines(render_state.total_lines)?;
                }
                term.show_cursor()?;
                return Ok(menu.len().saturating_sub(1));
            }
            _ => {}
        }
    }
}

pub(crate) fn jump_section(menu: &InteractiveMenu, selection: usize) -> usize {
    if selection < menu.accounts.len() {
        menu.first_action_index().unwrap_or(selection)
    } else if !menu.accounts.is_empty() {
        0
    } else {
        selection
    }
}

fn render_persistent_menu(
    term: &Term,
    menu: &InteractiveMenu,
    selection: usize,
    feedback: &[String],
) -> Result<PersistentRenderState> {
    let mut lines = 0usize;
    let mut row_lines = Vec::with_capacity(menu.len());
    for line in feedback {
        term.write_line(line)?;
        lines += 1;
    }
    if !feedback.is_empty() {
        term.write_line("")?;
        lines += 1;
    }
    if !menu.prompt.is_empty() {
        term.write_line(&style(menu.prompt).bold().to_string())?;
        lines += 1;
    }
    term.write_line(&render_app_header(
        &menu.environment,
        menu.saved_account_count,
    ))?;
    lines += 1;
    if menu.process_warning_count > 0 {
        term.write_line(
            &style(format!(
                "  {} Codex process(es) running — close them before switching for best results",
                menu.process_warning_count
            ))
            .yellow()
            .to_string(),
        )?;
        lines += 1;
    }
    term.write_line(&render_section_heading("Active Account"))?;
    lines += 1;
    if let Some(current_status_label) = &menu.current_status_label {
        term.write_line(&style_menu_label(
            current_status_label,
            menu.current_status_tone,
            false,
        ))?;
    } else {
        term.write_line(&style("  (not logged in)").white().to_string())?;
    }
    lines += 1;
    term.write_line(&render_section_heading("Saved Accounts"))?;
    lines += 1;
    if menu.accounts.is_empty() {
        term.write_line(&style("  (no saved accounts)").dim().to_string())?;
        lines += 1;
    } else {
        let mut index = 0;
        if menu.active_count > 0 {
            term.write_line(&style("  Active (Còn token)").magenta().bold().to_string())?;
            lines += 1;
            for _ in 0..menu.active_count {
                row_lines.push(lines);
                term.write_line(&render_menu_row(menu, selection, index))?;
                lines += 1;
                index += 1;
            }
        }
        if menu.depleted_count > 0 {
            term.write_line(&style("  Depleted (Hết token)").magenta().bold().to_string())?;
            lines += 1;
            for _ in 0..menu.depleted_count {
                row_lines.push(lines);
                term.write_line(&render_menu_row(menu, selection, index))?;
                lines += 1;
                index += 1;
            }
        }
        if menu.login_count > 0 {
            term.write_line(
                &style("  Login Required (Cần login lại)")
                    .magenta()
                    .bold()
                    .to_string(),
            )?;
            lines += 1;
            for _ in 0..menu.login_count {
                row_lines.push(lines);
                term.write_line(&render_menu_row(menu, selection, index))?;
                lines += 1;
                index += 1;
            }
        }
    }
    term.write_line(&render_section_heading("Actions"))?;
    lines += 1;
    for index in menu.accounts.len()..menu.len() {
        row_lines.push(lines);
        term.write_line(&render_menu_row(menu, selection, index))?;
        lines += 1;
    }
    term.write_line(&render_divider())?;
    lines += 1;
    term.write_line(
        &style("Arrows or j/k move. Tab or left/right jumps sections. Enter selects. q exits.")
            .dim()
            .to_string(),
    )?;
    lines += 1;
    Ok(PersistentRenderState {
        total_lines: lines,
        row_lines,
    })
}

fn render_menu_row(menu: &InteractiveMenu, selection: usize, index: usize) -> String {
    render_menu_row_explicit(menu, index, selection == index)
}

fn render_app_header(environment: &str, saved_account_count: usize) -> String {
    style(format!(
        "Codex Account Switcher v{}  ·  {}  ·  {} saved",
        env!("CARGO_PKG_VERSION"),
        environment,
        saved_account_count
    ))
    .magenta()
    .bold()
    .to_string()
}

fn style_menu_label(label: &str, tone: MenuLabelTone, selected: bool) -> String {
    if selected {
        return style(format!("> {label}")).cyan().bold().to_string();
    }
    let body = match tone {
        MenuLabelTone::LoginRequired => style(label).red().bold().to_string(),
        MenuLabelTone::Critical => style(label).red().to_string(),
        MenuLabelTone::Warning => style(label).yellow().to_string(),
        MenuLabelTone::Active => style(label).green().bold().to_string(),
        MenuLabelTone::Normal => label.to_owned(),
    };
    format!("  {body}")
}

fn render_section_heading(title: &str) -> String {
    style(title).blue().bold().to_string()
}

fn render_divider() -> String {
    style("--------------------------------------------------")
        .dim()
        .to_string()
}

fn update_persistent_selection(
    term: &Term,
    menu: &InteractiveMenu,
    render_state: &PersistentRenderState,
    previous: usize,
    next: usize,
) -> Result<()> {
    if previous == next {
        return Ok(());
    }
    rewrite_menu_row(term, menu, render_state, previous, false)?;
    rewrite_menu_row(term, menu, render_state, next, true)?;
    Ok(())
}

fn rewrite_menu_row(
    term: &Term,
    menu: &InteractiveMenu,
    render_state: &PersistentRenderState,
    index: usize,
    selected: bool,
) -> Result<()> {
    let line_index = render_state.row_lines[index];
    let lines_up = render_state.total_lines.saturating_sub(line_index);
    term.move_cursor_up(lines_up)?;
    term.clear_line()?;
    term.write_line(&render_menu_row_explicit(menu, index, selected))?;
    term.move_cursor_down(lines_up.saturating_sub(1))?;
    Ok(())
}

fn render_menu_row_explicit(menu: &InteractiveMenu, index: usize, selected: bool) -> String {
    let label = menu.label(index);
    style_menu_label(label, menu.tone(index), selected)
}

fn prompt_for_account_delete(accounts: &[AccountView]) -> Result<Uuid> {
    prompt_for_account_selection(accounts, "Which saved account do you want to delete?")
}

fn prompt_for_account_rename(accounts: &[AccountView]) -> Result<Uuid> {
    prompt_for_account_selection(accounts, "Which saved account do you want to rename?")
}

fn prompt_for_account_selection(accounts: &[AccountView], prompt: &str) -> Result<Uuid> {
    let widths = account_label_widths(&accounts.iter().collect::<Vec<_>>());
    let labels = accounts
        .iter()
        .map(|account| render_account_label(account, widths))
        .collect::<Vec<_>>();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(accounts[selection].id)
}

fn prompt_for_account_label(account: &AccountView) -> Result<Option<String>> {
    let default = account.label.clone().unwrap_or_default();
    let input = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Label for {} (leave blank to clear)",
            account.email
        ))
        .default(default)
        .allow_empty(true)
        .interact_text()?;
    let trimmed = input.trim().to_owned();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    })
}

fn prompt_switch_policy(
    prompt: &str,
    warnings: &[RunningCodexProcess],
) -> Result<SwitchWhenRunning> {
    let body = vec![
        "Codex appears to be running.".to_owned(),
        "Wait and switch defers until Codex finishes.".to_owned(),
        "Switch now forces the swap immediately.".to_owned(),
    ];
    let details = process_summary_lines("Codex processes", warnings);
    let selection = prompt_choice_menu(
        prompt,
        &body,
        &details,
        &["Cancel", "Wait and switch", "Switch now"],
        1,
    )?;
    Ok(match selection {
        0 => SwitchWhenRunning::Cancel,
        1 => SwitchWhenRunning::WaitAndSwitch,
        2 => SwitchWhenRunning::SwitchNow,
        _ => SwitchWhenRunning::Cancel,
    })
}

fn prompt_choice_menu(
    prompt: &str,
    body_lines: &[String],
    trailing_lines: &[String],
    options: &[&str],
    default_index: usize,
) -> Result<usize> {
    let term = Term::stderr();
    let mut selection = default_index.min(options.len().saturating_sub(1));
    let mut rendered_lines = 0usize;
    term.hide_cursor()?;
    loop {
        if rendered_lines > 0 {
            term.clear_last_lines(rendered_lines)?;
        }
        rendered_lines = 0;
        term.write_line(&style(prompt).bold().to_string())?;
        rendered_lines += 1;
        for line in body_lines {
            term.write_line(line)?;
            rendered_lines += 1;
        }
        if !body_lines.is_empty() || !trailing_lines.is_empty() {
            term.write_line("")?;
            rendered_lines += 1;
        }
        for (index, option) in options.iter().enumerate() {
            term.write_line(&render_confirm_option(option, selection == index))?;
            rendered_lines += 1;
        }
        if !trailing_lines.is_empty() {
            term.write_line("")?;
            rendered_lines += 1;
            for line in trailing_lines {
                term.write_line(line)?;
                rendered_lines += 1;
            }
        }
        match term.read_key()? {
            Key::ArrowUp | Key::Char('k') => {
                selection = selection.checked_sub(1).unwrap_or(options.len() - 1);
            }
            Key::ArrowDown | Key::Char('j') => {
                selection = (selection + 1) % options.len();
            }
            Key::Enter => {
                term.clear_last_lines(rendered_lines)?;
                term.show_cursor()?;
                return Ok(selection);
            }
            Key::Escape | Key::Char('q') => {
                term.clear_last_lines(rendered_lines)?;
                term.show_cursor()?;
                return Ok(0);
            }
            _ => {}
        }
    }
}

fn confirm_delete(account: &AccountView) -> Result<bool> {
    let widths = account_label_widths(&[account]);
    confirm_with_menu(
        "Delete saved snapshot?",
        &[
            render_account_label(account, widths),
            format!("Snapshot id: {}", account.id),
        ],
        &[],
        "Yes, delete",
        "No, keep it",
        false,
    )
}

fn interactive_status_lines(status: &StatusOutput) -> Vec<String> {
    let mut lines = vec![
        "Status".to_owned(),
        format!("Environment: {}", status.environment),
        format!("Codex root: {}", status.codex_root),
    ];
    match &status.current_account {
        Some(account) => {
            lines.push(format!("Current account: {}", account.email));
            if let Some(plan) = &account.plan_label {
                lines.push(format!("Plan: {plan}"));
            }
        }
        None => lines.push("Current account: not logged in".to_owned()),
    }
    lines.push(format!("Saved accounts: {}", status.saved_accounts));
    if !status.process_warnings.is_empty() {
        lines.extend(process_summary_lines(
            "Codex processes",
            &status.process_warnings,
        ));
    }
    lines
}

fn process_summary_lines(title: &str, processes: &[RunningCodexProcess]) -> Vec<String> {
    let mut lines = vec![format!("{title}:")];
    lines.extend(format_process_table(processes));
    lines
}

fn auto_start_usage_window_feedback(output: &AutoStartUsageWindowsRunOutput) -> Vec<String> {
    let mut lines = vec![format!(
        "Checked {} saved accounts for due weekly windows.",
        output.checked_accounts
    )];
    if output.pinged_accounts.is_empty() && output.skipped.is_empty() {
        lines.push("No due weekly windows found.".to_owned());
    }
    for account in &output.pinged_accounts {
        lines.push(match &account.detail {
            Some(detail) => format!("{}: {} ({detail})", account.email, account.status),
            None => format!("{}: {}", account.email, account.status),
        });
    }
    for skipped in &output.skipped {
        lines.push(format!("Skipped: {skipped}"));
    }
    lines
}

fn batch_refresh_feedback(output: &BatchRefreshOutput) -> Vec<String> {
    let mut lines = vec![format!(
        "Refreshed usage for {}/{} saved accounts.",
        output.refreshed.len(),
        output.total
    )];
    for failure in &output.failed {
        lines.push(format!(
            "Failed {} ({}): {}",
            failure.email, failure.account_id, failure.error
        ));
    }
    lines
}

fn pick_best_feedback(output: &PickBestOutput) -> Vec<String> {
    let mut lines = vec![if output.switched {
        format!(
            "Switched to best quota account {} ({}).",
            output.account.email, output.account.id
        )
    } else {
        format!(
            "Already on best quota account {} ({}).",
            output.account.email, output.account.id
        )
    }];
    for entry in &output.scores {
        let score = entry
            .score
            .map(|value| format!("{value:+.1}"))
            .unwrap_or_else(|| "n/a".to_owned());
        let marker = if entry.account_id == output.account.id {
            " <-"
        } else {
            ""
        };
        let label = entry
            .label
            .as_deref()
            .map(|name| format!(" [{name}]"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}{label}: score {score}{marker}{}",
            entry.email,
            if entry.eligible { "" } else { " (ineligible)" },
        ));
    }
    lines
}

fn error_feedback(prefix: &str, error: Error) -> Vec<String> {
    error_feedback_rendered(prefix, &format!("{error:#}"))
}

fn error_feedback_rendered(prefix: &str, rendered_error: &str) -> Vec<String> {
    let mut lines = vec![prefix.to_owned()];
    for (index, line) in rendered_error.lines().enumerate() {
        lines.push(if index == 0 {
            format!("Error: {line}")
        } else {
            format!("  {line}")
        });
    }
    lines
}

fn error_indicates_running_process_instability(rendered_error: &str) -> bool {
    rendered_error.contains("managed auth files no longer match")
        || rendered_error.contains("changed again after activation")
}

fn confirm_with_menu(
    prompt: &str,
    body_lines: &[String],
    trailing_lines: &[String],
    yes_label: &str,
    no_label: &str,
    default_yes: bool,
) -> Result<bool> {
    let term = Term::stderr();
    let mut selection = usize::from(default_yes);
    let options = [no_label, yes_label];
    let mut rendered_lines = 0usize;
    term.hide_cursor()?;
    loop {
        if rendered_lines > 0 {
            term.clear_last_lines(rendered_lines)?;
        }
        rendered_lines = 0;
        term.write_line(&style(prompt).bold().to_string())?;
        rendered_lines += 1;
        for line in body_lines {
            term.write_line(line)?;
            rendered_lines += 1;
        }
        if !body_lines.is_empty() || !trailing_lines.is_empty() {
            term.write_line("")?;
            rendered_lines += 1;
        }
        for (index, option) in options.iter().enumerate() {
            term.write_line(&render_confirm_option(option, selection == index))?;
            rendered_lines += 1;
        }
        if !trailing_lines.is_empty() {
            term.write_line("")?;
            rendered_lines += 1;
            for line in trailing_lines {
                term.write_line(line)?;
                rendered_lines += 1;
            }
        }
        match term.read_key()? {
            Key::ArrowUp
            | Key::ArrowDown
            | Key::ArrowLeft
            | Key::ArrowRight
            | Key::Tab
            | Key::Char('j')
            | Key::Char('k') => selection = 1 - selection,
            Key::Enter => {
                term.clear_last_lines(rendered_lines)?;
                term.show_cursor()?;
                return Ok(selection == 1);
            }
            Key::Escape | Key::Char('q') => {
                term.clear_last_lines(rendered_lines)?;
                term.show_cursor()?;
                return Ok(false);
            }
            _ => {}
        }
    }
}

fn render_confirm_option(label: &str, selected: bool) -> String {
    if selected {
        style(format!("> {label}")).cyan().bold().to_string()
    } else {
        format!("  {label}")
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use super::*;
    use crate::app::should_verify_activation_stability;
    use crate::codex::auth_json_fixture;
    use crate::env::AppEnv;
    use crate::model::{
        AccountUsageView, DisplayIdentity, EnvironmentKind, SnapshotBlob, UsageSource,
        UsageWindowView,
    };
    use crate::repository::SnapshotRepository;
    use crate::secrets::test_support::MemorySecretStore;

    fn sample_status_with_email(email: &str, current_saved_id: Option<Uuid>) -> StatusOutput {
        StatusOutput {
            environment: EnvironmentKind::Windows,
            codex_root: "C:\\Users\\tester\\.codex".to_owned(),
            current_account: current_saved_id.map(|_| DisplayIdentity {
                email: email.to_owned(),
                subject: Some("sub-1".to_owned()),
                name: Some("Tester".to_owned()),
                plan_label: Some("Pro".to_owned()),
                workspace_id: None,
                workspace_name: None,
            }),
            current_account_saved_id: current_saved_id,
            saved_accounts: usize::from(current_saved_id.is_some()),
            process_warnings: Vec::new(),
        }
    }

    fn sample_status(current_saved_id: Option<Uuid>) -> StatusOutput {
        sample_status_with_email("person@example.com", current_saved_id)
    }

    fn sample_list(id: Uuid, is_active: bool) -> ListOutput {
        ListOutput {
            environment: EnvironmentKind::Windows,
            accounts: vec![AccountView {
                id,
                email: "person@example.com".to_owned(),
                subject: Some("sub-1".to_owned()),
                name: Some("Tester".to_owned()),
                plan_label: Some("Pro".to_owned()),
                workspace_id: None,
                workspace_name: None,
                environment: EnvironmentKind::Windows,
                is_active,
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                last_activated_at: is_active.then_some(OffsetDateTime::UNIX_EPOCH),
                usage: None,
                usage_error: None,
                label: None,
                is_archived: false,
            }],
        }
    }

    #[test]
    fn account_label_includes_reset_time() {
        let id = Uuid::new_v4();
        let mut list = sample_list(id, false);
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(time::Date::from_calendar_date(2099, time::Month::May, 12).unwrap())
            .replace_time(time::Time::from_hms(13, 56, 0).unwrap());
        list.accounts[0].usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 77,
                remaining_percent: 23,
                reset_at,
            }),
            credits: None,
        });

        let label = render_account_label(
            &list.accounts[0],
            account_label_widths(&[&list.accounts[0]]),
        );

        assert!(label.contains(&format!("Reset: {}", format_local_reset_at(reset_at))));
    }

    #[test]
    fn account_label_marks_login_required_usage_error() {
        let id = Uuid::new_v4();
        let mut list = sample_list(id, false);
        list.accounts[0].usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 0,
                remaining_percent: 100,
                reset_at: OffsetDateTime::UNIX_EPOCH
                    .replace_date(
                        time::Date::from_calendar_date(2099, time::Month::May, 12).unwrap(),
                    )
                    .replace_time(time::Time::from_hms(13, 56, 0).unwrap()),
            }),
            credits: None,
        });
        list.accounts[0].usage_error = Some("Login required: Codex auth expired.".to_owned());

        let label = render_account_label(
            &list.accounts[0],
            account_label_widths(&[&list.accounts[0]]),
        );

        assert!(label.contains("Login required"));
        assert!(!label.contains("Usage unavailable"));
        assert!(!label.contains("Weekly Remaining"));
    }

    #[test]
    fn account_label_keeps_cached_usage_for_transient_usage_error() {
        let id = Uuid::new_v4();
        let mut list = sample_list(id, false);
        list.accounts[0].usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 10,
                remaining_percent: 90,
                reset_at: OffsetDateTime::UNIX_EPOCH
                    .replace_date(
                        time::Date::from_calendar_date(2099, time::Month::May, 12).unwrap(),
                    )
                    .replace_time(time::Time::from_hms(13, 56, 0).unwrap()),
            }),
            credits: None,
        });
        list.accounts[0].usage_error =
            Some("Usage unavailable: failed to query Codex usage".to_owned());

        let label = render_account_label(
            &list.accounts[0],
            account_label_widths(&[&list.accounts[0]]),
        );

        assert!(label.contains("Weekly Remaining: 90%"));
        assert!(label.contains("Ready"));
        assert!(!label.contains("Usage unavailable"));
    }

    #[test]
    fn account_label_surfaces_status_for_fast_scanning() {
        let id = Uuid::new_v4();
        let mut list = sample_list(id, true);
        list.accounts[0].usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: Some(UsageWindowView {
                used_percent: 93,
                remaining_percent: 7,
                reset_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
            }),
            weekly: None,
            credits: None,
        });

        let label = render_account_label(
            &list.accounts[0],
            account_label_widths(&[&list.accounts[0]]),
        );

        assert!(label.contains("Active / Low quota"));
        assert!(label.contains("5h Remaining: 7%"));
    }

    #[test]
    fn activate_once_menu_hides_active_account() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::ActivateOnce,
            &sample_status(Some(id)),
            &sample_list(id, true),
            Some(id),
            false,
            false,
            false,
            false,
        );
        assert_eq!(menu.prompt, "");
        assert_eq!(menu.accounts.len(), 0);
        assert_eq!(menu.len(), 1);
        assert!(matches!(menu.action(0), InteractiveAction::Quit));
    }

    #[test]
    fn delete_once_menu_only_lists_deletes_and_quit() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::DeleteOnce,
            &sample_status(Some(id)),
            &sample_list(id, true),
            Some(id),
            false,
            false,
            false,
            false,
        );
        assert_eq!(menu.prompt, "Which saved account do you want to delete?");
        assert_eq!(menu.len(), 2);
        assert_eq!(menu.accounts.len(), 1);
        assert!(matches!(menu.action(0), InteractiveAction::Delete(actual) if actual == id));
        assert!(matches!(menu.action(1), InteractiveAction::Quit));
    }

    #[test]
    fn persistent_menu_keeps_refresh_in_actions() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id, false),
            Some(id),
            false,
            false,
            false,
            false,
        );
        assert_eq!(menu.accounts.len(), 1);
        assert!(matches!(menu.action(1), InteractiveAction::SaveCurrent));
        assert_eq!(
            menu.actions[0].label,
            "Refresh saved snapshot for person@example.com"
        );
    }

    #[test]
    fn persistent_menu_shows_auto_start_usage_window_toggle() {
        let id = Uuid::new_v4();
        let disabled_menu = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id, false),
            Some(id),
            false,
            false,
            false,
            false,
        );
        let disabled_toggle = disabled_menu
            .actions
            .iter()
            .find(|item| item.label == format!("Enable {AUTO_REFRESH_QUOTA_ON_RESET_LABEL}"))
            .expect("enable toggle");
        assert!(matches!(
            disabled_toggle.action,
            InteractiveAction::SetAutoStartUsageWindows(true)
        ));

        let enabled_menu = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id, false),
            Some(id),
            true,
            false,
            false,
            false,
        );
        let enabled_toggle = enabled_menu
            .actions
            .iter()
            .find(|item| item.label == format!("Disable {AUTO_REFRESH_QUOTA_ON_RESET_LABEL}"))
            .expect("disable toggle");
        assert!(matches!(
            enabled_toggle.action,
            InteractiveAction::SetAutoStartUsageWindows(false)
        ));
    }

    #[test]
    fn force_running_always_enables_stability_verification() {
        let warnings = Vec::new();
        assert!(should_verify_activation_stability(true, &warnings));
        assert!(!should_verify_activation_stability(false, &warnings));

        let warnings = vec![RunningCodexProcess {
            pid: 1,
            executable: "codex.exe".to_owned(),
            role: "process".to_owned(),
            summary: None,
        }];
        assert!(should_verify_activation_stability(false, &warnings));
    }

    #[test]
    fn jump_section_switches_between_accounts_and_actions() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id, false),
            Some(id),
            false,
            false,
            false,
            false,
        );
        assert_eq!(jump_section(&menu, 0), 1);
        assert_eq!(jump_section(&menu, 1), 0);
    }

    #[test]
    fn persistent_menu_hides_active_account_from_switch_targets() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id, true),
            Some(id),
            false,
            false,
            false,
            false,
        );
        assert_eq!(menu.accounts.len(), 0);
        let current = menu
            .current_status_label
            .as_deref()
            .expect("current status label");
        assert!(current.contains("person@example.com"));
        assert!(current.contains("Plan: Pro"));
        assert!(!current.contains("Saved:"));
        assert!(!current.contains("Last Used:"));
        assert_eq!(
            menu.actions[0].label,
            "Refresh saved snapshot for person@example.com"
        );
    }

    #[test]
    fn persistent_menu_keeps_unsaved_current_account_out_of_saved_accounts() {
        let id = Uuid::new_v4();
        let status = StatusOutput {
            environment: EnvironmentKind::Windows,
            codex_root: "C:\\Users\\tester\\.codex".to_owned(),
            current_account: Some(DisplayIdentity {
                email: "other@example.com".to_owned(),
                subject: Some("sub-2".to_owned()),
                name: Some("Other".to_owned()),
                plan_label: Some("Plus".to_owned()),
                workspace_id: None,
                workspace_name: None,
            }),
            current_account_saved_id: None,
            saved_accounts: 1,
            process_warnings: Vec::new(),
        };
        let menu = build_menu(
            InteractiveMode::Persistent,
            &status,
            &sample_list(id, false),
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(
            menu.current_status_label.as_deref(),
            Some("other@example.com [not saved]")
        );
        assert_eq!(menu.accounts.len(), 1);
        assert!(menu.accounts[0].label.contains("person@example.com"));
        assert!(!menu.accounts[0].label.contains("Saved:"));
        assert!(!menu.accounts[0].label.contains("Last Used:"));
        assert_eq!(
            menu.actions[0].label,
            "Add current account other@example.com to switcher"
        );
    }

    #[test]
    fn persistent_menu_reads_live_status_and_accounts() {
        let temp = tempdir().expect("tempdir");
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };
        std::fs::create_dir_all(&env.codex_root).expect("codex root");
        std::fs::write(
            env.codex_root.join("auth.json"),
            auth_json_fixture("current@example.com", "sub-current", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid-current").expect("cap");
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        repo.save_snapshot(
            &env.kind,
            &DisplayIdentity {
                email: "saved@example.com".to_owned(),
                subject: Some("sub-1".to_owned()),
                name: None,
                plan_label: Some("Pro".to_owned()),
                workspace_id: None,
                workspace_name: None,
            },
            &SnapshotBlob {
                schema_version: 1,
                files: vec![],
            },
        )
        .expect("save");
        let app = App::new(env, repo);
        let status = app.status().expect("status");
        let list = app.list().expect("list");
        let menu = build_menu(
            InteractiveMode::Persistent,
            &status,
            &list,
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(menu.accounts.len(), 1);
        assert!(menu.actions.iter().any(|item| item.label == "Show status"));
    }

    #[test]
    fn build_menu_groups_accounts_correctly() {
        let status = sample_status(None);
        let id_active = Uuid::new_v4();
        let id_depleted = Uuid::new_v4();
        let id_login = Uuid::new_v4();

        // 1. Active account: normal usage, no error
        let acc_active = AccountView {
            id: id_active,
            email: "active@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Active".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
            environment: EnvironmentKind::Windows,
            is_active: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_activated_at: None,
            usage: Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: Some(UsageWindowView {
                    used_percent: 50,
                    remaining_percent: 50,
                    reset_at: OffsetDateTime::now_utc() + time::Duration::days(1),
                }),
                credits: None,
            }),
            usage_error: None,
            label: None,
            is_archived: false,
        };

        // 2. Depleted account: 0% quota remaining
        let mut acc_depleted = acc_active.clone();
        acc_depleted.id = id_depleted;
        acc_depleted.email = "depleted@example.com".to_owned();
        acc_depleted.usage = Some(AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour: None,
            weekly: Some(UsageWindowView {
                used_percent: 100,
                remaining_percent: 0,
                reset_at: OffsetDateTime::now_utc() + time::Duration::days(1),
            }),
            credits: None,
        });

        // 3. Login required account
        let mut acc_login = acc_active.clone();
        acc_login.id = id_login;
        acc_login.email = "login@example.com".to_owned();
        acc_login.usage_error = Some("Login required: Codex auth expired.".to_owned());

        let list = ListOutput {
            environment: EnvironmentKind::Windows,
            accounts: vec![acc_active, acc_depleted, acc_login],
        };

        let menu = build_menu(
            InteractiveMode::Persistent,
            &status,
            &list,
            None,
            false,
            false,
            false,
            false,
        );

        assert_eq!(menu.active_count, 1);
        assert_eq!(menu.depleted_count, 1);
        assert_eq!(menu.login_count, 1);

        // Verify order in menu.accounts: Active first, then Depleted, then Login Required
        assert_eq!(
            menu.accounts[0].action,
            InteractiveAction::Activate(id_active)
        );
        assert_eq!(
            menu.accounts[1].action,
            InteractiveAction::Activate(id_depleted)
        );
        assert_eq!(
            menu.accounts[2].action,
            InteractiveAction::Activate(id_login)
        );
    }
}
