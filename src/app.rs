use anyhow::{Context, Error, Result};
use console::{Key, Term, style};
use dialoguer::{Select, theme::ColorfulTheme};
use uuid::Uuid;

use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AccountView, ActivateOutput, DeleteOutput, DisplayIdentity, ListOutput, SaveAction, SaveOutput,
    StatusOutput,
};
use crate::process::{detect_running_codex_processes, format_process_table};
use crate::repository::SnapshotRepository;
use crate::secrets::SecretStore;

pub struct App<S> {
    env: AppEnv,
    repository: SnapshotRepository<S>,
}

#[derive(Clone, Copy)]
pub enum InteractiveMode {
    Persistent,
    ActivateOnce,
    DeleteOnce,
}

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn new(env: AppEnv, repository: SnapshotRepository<S>) -> Self {
        Self { env, repository }
    }

    pub fn status(&self) -> Result<StatusOutput> {
        let saved_accounts = self.repository.list_accounts(&self.env.kind)?;
        let live = codex::try_read_live_auth_bundle(&self.env)?;
        let current_saved_id = live
            .as_ref()
            .and_then(|bundle| match_saved_account(&saved_accounts, &bundle.identity))
            .map(|account| account.id);
        Ok(StatusOutput {
            environment: self.env.kind.clone(),
            codex_root: self.env.codex_root.display().to_string(),
            current_account: live.map(|bundle| bundle.identity),
            current_account_saved_id: current_saved_id,
            saved_accounts: saved_accounts.len(),
            process_warnings: detect_running_codex_processes(),
        })
    }

    pub fn list(&self) -> Result<ListOutput> {
        let accounts = self.repository.list_accounts(&self.env.kind)?;
        let live = codex::try_read_live_auth_bundle(&self.env)?;
        let active_id = live
            .as_ref()
            .and_then(|bundle| match_saved_account(&accounts, &bundle.identity))
            .map(|account| account.id);
        Ok(ListOutput {
            environment: self.env.kind.clone(),
            accounts: accounts
                .into_iter()
                .map(|account| account_view(account, active_id))
                .collect(),
        })
    }

    pub fn save_current(&self) -> Result<SaveOutput> {
        let live = codex::read_live_auth_bundle(&self.env).with_context(|| {
            format!(
                "no live Codex auth bundle found at {}",
                self.env.codex_root.display()
            )
        })?;
        let (metadata, created) =
            self.repository
                .save_snapshot(&self.env.kind, &live.identity, &live.snapshot)?;
        let active_id = Some(metadata.id);
        Ok(SaveOutput {
            account: account_view(metadata, active_id),
            action: if created {
                SaveAction::Created
            } else {
                SaveAction::Refreshed
            },
        })
    }

    pub fn activate(&self, account_id: Uuid) -> Result<ActivateOutput> {
        self.activate_with_running_policy(account_id, false)
    }

    pub fn validate_activation_target(&self, account_id: Uuid) -> Result<()> {
        let _ = self.load_activation_target(account_id)?;
        Ok(())
    }

    pub fn activate_with_running_policy(
        &self,
        account_id: Uuid,
        force_running: bool,
    ) -> Result<ActivateOutput> {
        let warnings = detect_running_codex_processes();
        let (snapshot, snapshot_identity, restore_identity) =
            self.load_activation_target(account_id)?;
        let verify_stable = should_verify_activation_stability(force_running, &warnings);
        codex::restore_snapshot(&self.env, &snapshot, &restore_identity, verify_stable)
            .context("failed to restore the selected account snapshot")?;
        let metadata = self
            .repository
            .sync_activated_account(&self.env.kind, account_id, &snapshot_identity)
            .context("activated live auth but failed to update local metadata")?;
        Ok(ActivateOutput {
            account: account_view(metadata, Some(account_id)),
            warnings,
        })
    }

    fn load_activation_target(
        &self,
        account_id: Uuid,
    ) -> Result<(crate::model::SnapshotBlob, DisplayIdentity, DisplayIdentity)> {
        let (metadata, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
        let expected_identity = saved_identity(&metadata);
        let snapshot_identity = codex::identity_from_snapshot(&snapshot)?;
        let restore_identity = if expected_identity.subject.is_some() {
            if !subject_bound_identity_matches(&expected_identity, &snapshot_identity) {
                anyhow::bail!(
                    "saved snapshot identity does not match the selected account: expected {:?}, got {:?}",
                    expected_identity,
                    snapshot_identity
                );
            }
            expected_identity.clone()
        } else {
            snapshot_identity.clone()
        };
        Ok((snapshot, snapshot_identity, restore_identity))
    }

    pub fn activation_preflight_warnings(&self) -> Vec<crate::model::RunningCodexProcess> {
        detect_running_codex_processes()
    }

    pub fn delete(&self, account_id: Uuid) -> Result<DeleteOutput> {
        self.repository
            .delete_snapshot(&self.env.kind, account_id)?;
        Ok(DeleteOutput {
            deleted_account_id: account_id,
        })
    }

    pub fn interactive(&self, mode: InteractiveMode, force_running: bool) -> Result<()> {
        let mut default_selection = 0usize;
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
enum InteractiveAction {
    SaveCurrent,
    Activate(Uuid),
    Delete(Uuid),
    DeletePrompt,
    ShowStatus,
    Quit,
}

struct InteractiveItem {
    label: String,
    action: InteractiveAction,
}

struct InteractiveMenu {
    prompt: &'static str,
    current_status_label: Option<String>,
    accounts: Vec<InteractiveItem>,
    actions: Vec<InteractiveItem>,
}

struct PersistentRenderState {
    total_lines: usize,
    row_lines: Vec<usize>,
}

impl InteractiveMenu {
    fn len(&self) -> usize {
        self.accounts.len() + self.actions.len()
    }

    fn labels(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .chain(self.actions.iter())
            .map(|item| item.label.as_str())
            .collect()
    }

    fn action(&self, index: usize) -> InteractiveAction {
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

fn account_view(
    account: crate::model::SavedAccountMetadata,
    active_id: Option<Uuid>,
) -> AccountView {
    AccountView {
        id: account.id,
        email: account.email,
        subject: account.subject,
        name: account.name,
        plan_label: account.plan_label,
        environment: account.environment,
        is_active: active_id.is_some_and(|id| id == account.id),
        created_at: account.created_at,
        updated_at: account.updated_at,
        last_activated_at: account.last_activated_at,
    }
}

fn match_saved_account<'a>(
    accounts: &'a [crate::model::SavedAccountMetadata],
    identity: &DisplayIdentity,
) -> Option<&'a crate::model::SavedAccountMetadata> {
    accounts
        .iter()
        .find(|account| saved_identity(account).matches(identity))
}

fn account_view_matches_identity(account: &AccountView, identity: &DisplayIdentity) -> bool {
    DisplayIdentity {
        email: account.email.clone(),
        subject: account.subject.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
    }
    .matches(identity)
}

fn saved_identity(account: &crate::model::SavedAccountMetadata) -> DisplayIdentity {
    DisplayIdentity {
        email: account.email.clone(),
        subject: account.subject.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
    }
}

fn subject_bound_identity_matches(expected: &DisplayIdentity, snapshot: &DisplayIdentity) -> bool {
    match (&expected.subject, &snapshot.subject) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn should_verify_activation_stability(
    force_running: bool,
    warnings: &[crate::model::RunningCodexProcess],
) -> bool {
    force_running || !warnings.is_empty()
}

fn render_account_label(account: &AccountView) -> String {
    let mut parts = vec![account.email.clone()];
    if let Some(plan) = &account.plan_label {
        parts.push(format!("[{plan}]"));
    }
    if account.is_active {
        parts.push("- Active".to_owned());
    } else if let Some(ts) = account.last_activated_at {
        parts.push(format!("- Last activated {}", ts.date()));
    } else {
        parts.push(format!("- Saved on {}", account.updated_at.date()));
    }
    parts.join(" ")
}

fn build_menu(
    mode: InteractiveMode,
    status: &StatusOutput,
    list: &ListOutput,
    current_saved: Option<Uuid>,
) -> InteractiveMenu {
    let mut accounts = Vec::new();
    for account in list.accounts.iter().filter(|account| {
        !matches!(
            mode,
            InteractiveMode::Persistent | InteractiveMode::ActivateOnce
        ) || !account.is_active
    }) {
        accounts.push(InteractiveItem {
            label: render_account_label(account),
            action: match mode {
                InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
                    InteractiveAction::Activate(account.id)
                }
                InteractiveMode::DeleteOnce => InteractiveAction::Delete(account.id),
            },
        });
    }

    let mut actions = Vec::new();
    let current_status_label = status.current_account.as_ref().map(|current| {
        format!(
            "{}{}",
            current.email,
            if current_saved.is_some() {
                " [saved]"
            } else {
                " [not saved]"
            }
        )
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

fn jump_section(menu: &InteractiveMenu, selection: usize) -> usize {
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
    let labels = accounts
        .iter()
        .map(render_account_label)
        .collect::<Vec<_>>();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which saved account do you want to delete?")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(accounts[selection].id)
}

fn confirm_delete(account: &AccountView) -> Result<bool> {
    confirm_with_menu(
        "Delete saved snapshot?",
        &[
            render_account_label(account),
            format!("Snapshot id: {}", account.id),
        ],
        &[],
        "Yes, delete",
        "No, keep it",
        false,
    )
}

fn confirm_activation(warnings: &[crate::model::RunningCodexProcess]) -> Result<bool> {
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

fn process_summary_lines(
    title: &str,
    processes: &[crate::model::RunningCodexProcess],
) -> Vec<String> {
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
            | Key::Char('k') => {
                selection = 1 - selection;
            }
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
    use base64::Engine;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use super::*;
    use crate::codex::auth_json_fixture;
    use crate::env::AppEnv;
    use crate::model::{EnvironmentKind, SnapshotBlob};
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

        let warnings = vec![crate::model::RunningCodexProcess {
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
        assert_eq!(
            menu.current_status_label.as_deref(),
            Some("person@example.com [saved]")
        );
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
        assert!(menu.accounts[0].label.contains("Saved on 1970-01-01"));
        assert_eq!(
            menu.actions[0].label,
            "Add current account other@example.com to switcher"
        );
    }

    #[test]
    fn list_marks_active_account() {
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
            auth_json_fixture("active@example.com", "sub-1", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid").expect("cap");
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        repo.save_snapshot(
            &env.kind,
            &DisplayIdentity {
                email: "active@example.com".to_owned(),
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
        let output = app.list().expect("list");
        assert_eq!(output.accounts.len(), 1);
        assert!(output.accounts[0].is_active);
    }

    #[test]
    fn list_keeps_saved_account_when_live_account_is_unsaved() {
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
            auth_json_fixture("current@example.com", "sub-2", Some("plus")),
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
        let output = app.list().expect("list");
        assert_eq!(output.accounts.len(), 1);
        assert_eq!(output.accounts[0].email, "saved@example.com");
        assert!(!output.accounts[0].is_active);
    }

    #[test]
    fn subject_bound_identity_requires_matching_subject() {
        let expected = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        };
        let missing_subject = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: None,
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        };
        let wrong_subject = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: Some("sub-2".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        };
        let matching_subject = DisplayIdentity {
            email: "other@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
        };
        assert!(!subject_bound_identity_matches(&expected, &missing_subject));
        assert!(!subject_bound_identity_matches(&expected, &wrong_subject));
        assert!(subject_bound_identity_matches(&expected, &matching_subject));
    }

    #[test]
    fn activate_returns_refreshed_identity_after_subject_stable_restore() {
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
        std::fs::write(env.codex_root.join("cap_sid"), "sid-a").expect("cap");

        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "before@example.com".to_owned(),
                    subject: Some("sub-1".to_owned()),
                    name: Some("Before".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        crate::model::SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("after@example.com", "sub-1", Some("plus")),
                            ),
                        },
                        crate::model::SnapshotFile {
                            name: "cap_sid".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-b"),
                        },
                    ],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env.clone(), repo);
        let output = app.activate(saved.id).expect("activate");
        assert_eq!(output.account.email, "after@example.com");
        assert_eq!(output.account.plan_label.as_deref(), Some("Plus"));

        let list = app.list().expect("list");
        assert_eq!(list.accounts[0].email, "after@example.com");
    }

    #[test]
    fn activate_rejects_snapshot_that_does_not_match_selected_account() {
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
            auth_json_fixture("active@example.com", "sub-1", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid-a").expect("cap");

        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "expected@example.com".to_owned(),
                    subject: Some("sub-expected".to_owned()),
                    name: Some("Expected".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        crate::model::SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("wrong@example.com", "sub-wrong", Some("plus")),
                            ),
                        },
                        crate::model::SnapshotFile {
                            name: "cap_sid".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-b"),
                        },
                    ],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env.clone(), repo);
        let error = app.activate(saved.id).expect_err("activate should fail");
        assert!(format!("{error:#}").contains("does not match the selected account"));
        let live = crate::codex::read_live_auth_bundle(&env).expect("live bundle");
        assert_eq!(live.identity.email, "active@example.com");
    }

    #[test]
    fn activate_allows_legacy_metadata_without_subject_to_refresh_email() {
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
            auth_json_fixture("active@example.com", "sub-1", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid-a").expect("cap");

        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "old@example.com".to_owned(),
                    subject: None,
                    name: Some("Old".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        crate::model::SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("new@example.com", "sub-new", Some("plus")),
                            ),
                        },
                        crate::model::SnapshotFile {
                            name: "cap_sid".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-b"),
                        },
                    ],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env.clone(), repo);
        let output = app.activate(saved.id).expect("activate");
        assert_eq!(output.account.email, "new@example.com");
        assert_eq!(output.account.subject.as_deref(), Some("sub-new"));
    }
}
