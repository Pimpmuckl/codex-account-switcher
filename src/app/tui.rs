use anyhow::{Context, Error, Result};
use console::{Key, Term, style};
use dialoguer::{Select, theme::ColorfulTheme};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::model::{AccountView, ListOutput, RunningCodexProcess, SaveAction, StatusOutput};
use crate::process::format_process_table;
use crate::secrets::SecretStore;

use super::{App, InteractiveMode, account_view_matches_identity};

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn interactive(&self, mode: InteractiveMode, force_running: bool) -> Result<()> {
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

            let menu = build_menu(mode, &status, &list, current_saved);
            let selection = match mode {
                InteractiveMode::Persistent => {
                    select_persistent_entry(&menu, default_selection, &feedback)?
                }
                InteractiveMode::ActivateOnce | InteractiveMode::DeleteOnce => {
                    let labels = menu.labels();
                    Select::with_theme(&ColorfulTheme::default())
                        .with_prompt(menu.prompt)
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
                    let showed_preflight = !warnings.is_empty();
                    if showed_preflight && !confirm_activation(&warnings)? {
                        continue;
                    }
                    let output = match self
                        .activate_with_running_policy(account_id, force_running || showed_preflight)
                    {
                        Ok(output) => output,
                        Err(error) => {
                            if matches!(mode, InteractiveMode::Persistent) {
                                let rendered_error = format!("{error:#}");
                                feedback = error_feedback_rendered(
                                    "Account activation failed.",
                                    &rendered_error,
                                );
                                if showed_preflight
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
                    if showed_preflight {
                        feedback.push(
                            "Codex was still running during activation. If the account does not change in Codex, close those processes fully and retry."
                                .to_owned(),
                        );
                    }
                    if !showed_preflight && !output.warnings.is_empty() {
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
                InteractiveAction::ShowStatus => {
                    feedback = interactive_status_lines(&status);
                }
                InteractiveAction::Quit => break,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum InteractiveAction {
    SaveCurrent,
    Activate(Uuid),
    Delete(Uuid),
    DeletePrompt,
    ShowStatus,
    Quit,
}

pub(crate) struct InteractiveItem {
    pub(crate) label: String,
    pub(crate) action: InteractiveAction,
}

pub(crate) struct InteractiveMenu {
    pub(crate) prompt: &'static str,
    pub(crate) current_status_label: Option<String>,
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
    saved: usize,
    last_used: usize,
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

    fn first_action_index(&self) -> Option<usize> {
        (!self.actions.is_empty()).then_some(self.accounts.len())
    }
}

fn render_account_label(account: &AccountView, widths: AccountLabelWidths) -> String {
    let email = format!("{:<width$}", account.email, width = widths.email);
    let plan = format!(
        "{:<width$}",
        account
            .plan_label
            .as_ref()
            .map(|plan| format!("Plan: {plan}"))
            .unwrap_or_default(),
        width = widths.plan
    );
    let saved = format!("Saved: {}", account.updated_at.date());
    let saved = format!("{:<width$}", saved, width = widths.saved);
    let last_used = if account.is_active {
        "Last Used: Active".to_owned()
    } else if let Some(ts) = account.last_activated_at {
        format!("Last Used: {}", ts.date())
    } else {
        String::new()
    };
    let last_used = format!("{:<width$}", last_used, width = widths.last_used);

    let (remaining, reset) = if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            ("Weekly Remaining: passed".to_owned(), String::new())
        } else {
            (
                format!("Weekly Remaining: {}%", weekly.remaining_percent),
                format!("Reset: {}", weekly.reset_at.date()),
            )
        }
    } else if account.usage_error.is_some() {
        ("Usage unavailable".to_owned(), String::new())
    } else {
        (String::new(), String::new())
    };
    let remaining = format!("{:<width$}", remaining, width = widths.remaining);
    let reset = format!("{:<width$}", reset, width = widths.reset);

    [email, plan, saved, last_used, remaining, reset]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

fn account_label_widths(accounts: &[&AccountView]) -> AccountLabelWidths {
    let mut widths = AccountLabelWidths::default();
    for account in accounts {
        widths.email = widths.email.max(account.email.len());
        widths.plan = widths.plan.max(
            account
                .plan_label
                .as_ref()
                .map(|plan| format!("Plan: {plan}").len())
                .unwrap_or(0),
        );
        widths.saved = widths
            .saved
            .max(format!("Saved: {}", account.updated_at.date()).len());
        widths.last_used = widths.last_used.max(if account.is_active {
            "Last Used: Active".len()
        } else if let Some(ts) = account.last_activated_at {
            format!("Last Used: {}", ts.date()).len()
        } else {
            0
        });

        let (remaining, reset) = if let Some(usage) = &account.usage
            && let Some(weekly) = &usage.weekly
        {
            if weekly.reset_at <= OffsetDateTime::now_utc() {
                ("Weekly Remaining: passed".len(), 0)
            } else {
                (
                    format!("Weekly Remaining: {}%", weekly.remaining_percent).len(),
                    format!("Reset: {}", weekly.reset_at.date()).len(),
                )
            }
        } else if account.usage_error.is_some() {
            ("Usage unavailable".len(), 0)
        } else {
            (0, 0)
        };
        widths.remaining = widths.remaining.max(remaining);
        widths.reset = widths.reset.max(reset);
    }
    widths
}

pub(crate) fn build_menu(
    mode: InteractiveMode,
    status: &StatusOutput,
    list: &ListOutput,
    current_saved: Option<Uuid>,
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

    let mut accounts = Vec::new();
    for account in list.accounts.iter().filter(|account| {
        !matches!(
            mode,
            InteractiveMode::Persistent | InteractiveMode::ActivateOnce
        ) || !account.is_active
    }) {
        accounts.push(InteractiveItem {
            label: render_account_label(account, widths),
            action: match mode {
                InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
                    InteractiveAction::Activate(account.id)
                }
                InteractiveMode::DeleteOnce => InteractiveAction::Delete(account.id),
            },
        });
    }

    let mut actions = Vec::new();
    let current_status_label = active_account
        .map(|account| render_account_label(account, widths))
        .or_else(|| {
            status.current_account.as_ref().map(|current| {
                format!(
                    "{}{}",
                    current.email,
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
                    format!("Refresh saved snapshot for {}", current.email)
                } else {
                    format!("Add current account {} to switcher", current.email)
                },
                action: InteractiveAction::SaveCurrent,
            });
        }
        if !list.accounts.is_empty() {
            actions.push(InteractiveItem {
                label: "Delete saved account".to_owned(),
                action: InteractiveAction::DeletePrompt,
            });
        }
        actions.push(InteractiveItem {
            label: "Show status".to_owned(),
            action: InteractiveAction::ShowStatus,
        });
    }
    actions.push(InteractiveItem {
        label: "Quit".to_owned(),
        action: InteractiveAction::Quit,
    });

    let prompt = match mode {
        InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
            "Which account do you want to activate?"
        }
        InteractiveMode::DeleteOnce => "Which saved account do you want to delete?",
    };

    InteractiveMenu {
        prompt,
        current_status_label,
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
    term.write_line(&style(menu.prompt).bold().to_string())?;
    lines += 1;
    term.write_line(&render_section_heading("Active Account"))?;
    lines += 1;
    if let Some(current_status_label) = &menu.current_status_label {
        term.write_line(
            &style(format!("  {current_status_label}"))
                .white()
                .to_string(),
        )?;
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
        for index in 0..menu.accounts.len() {
            row_lines.push(lines);
            term.write_line(&render_menu_row(menu, selection, index))?;
            lines += 1;
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
    if selected {
        style(format!("> {label}")).cyan().bold().to_string()
    } else {
        format!("  {label}")
    }
}

fn prompt_for_account_delete(accounts: &[AccountView]) -> Result<Uuid> {
    let widths = account_label_widths(&accounts.iter().collect::<Vec<_>>());
    let labels = accounts
        .iter()
        .map(|account| render_account_label(account, widths))
        .collect::<Vec<_>>();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which saved account do you want to delete?")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(accounts[selection].id)
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

fn confirm_activation(warnings: &[RunningCodexProcess]) -> Result<bool> {
    let body = vec![
        "Codex appears to be running.".to_owned(),
        "Close every listed process first for a reliable swap, or force activation anyway."
            .to_owned(),
    ];
    let details = process_summary_lines("Codex processes", warnings);
    confirm_with_menu(
        "Continue with account activation?",
        &body,
        &details,
        "Yes, force activation",
        "No, cancel",
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
    use crate::model::{DisplayIdentity, EnvironmentKind, SnapshotBlob};
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
                environment: EnvironmentKind::Windows,
                is_active,
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                last_activated_at: is_active.then_some(OffsetDateTime::UNIX_EPOCH),
                usage: None,
                usage_error: None,
            }],
        }
    }

    #[test]
    fn activate_once_menu_hides_active_account() {
        let id = Uuid::new_v4();
        let menu = build_menu(
            InteractiveMode::ActivateOnce,
            &sample_status(Some(id)),
            &sample_list(id, true),
            Some(id),
        );
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
        );
        assert_eq!(menu.accounts.len(), 1);
        assert!(matches!(menu.action(1), InteractiveAction::SaveCurrent));
        assert_eq!(
            menu.actions[0].label,
            "Refresh saved snapshot for person@example.com"
        );
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
        );
        assert_eq!(menu.accounts.len(), 0);
        let current = menu
            .current_status_label
            .as_deref()
            .expect("current status label");
        assert!(current.contains("person@example.com"));
        assert!(current.contains("Plan: Pro"));
        assert!(current.contains("Saved: 1970-01-01"));
        assert!(current.contains("Last Used: Active"));
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
        );
        assert_eq!(
            menu.current_status_label.as_deref(),
            Some("other@example.com [not saved]")
        );
        assert_eq!(menu.accounts.len(), 1);
        assert!(menu.accounts[0].label.contains("person@example.com"));
        assert!(menu.accounts[0].label.contains("Saved: 1970-01-01"));
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
        let menu = build_menu(InteractiveMode::Persistent, &status, &list, None);
        assert_eq!(menu.accounts.len(), 1);
        assert!(menu.actions.iter().any(|item| item.label == "Show status"));
    }
}
