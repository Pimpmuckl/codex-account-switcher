use anyhow::{Context, Result};
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use uuid::Uuid;

use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    AccountView, ActivateOutput, DeleteOutput, DisplayIdentity, ListOutput, SaveAction, SaveOutput,
    StatusOutput,
};
use crate::process::detect_running_codex_processes;
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
        let warnings = detect_running_codex_processes();
        let (mut metadata, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
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
        codex::restore_snapshot(&self.env, &snapshot, &restore_identity)?;
        metadata = self
            .repository
            .sync_activated_account(&self.env.kind, account_id, &snapshot_identity)
            .context("activated live auth but failed to update local metadata")?;
        Ok(ActivateOutput {
            account: account_view(metadata, Some(account_id)),
            warnings,
        })
    }

    pub fn activation_preflight_warnings(&self) -> Vec<String> {
        detect_running_codex_processes()
    }

    pub fn delete(&self, account_id: Uuid) -> Result<DeleteOutput> {
        self.repository
            .delete_snapshot(&self.env.kind, account_id)?;
        Ok(DeleteOutput {
            deleted_account_id: account_id,
        })
    }

    pub fn interactive(&self, mode: InteractiveMode) -> Result<()> {
        let mut default_selection = 0usize;
        loop {
            let status = self.status()?;
            let list = self.list()?;
            let current_saved = status.current_account.as_ref().and_then(|identity| {
                list.accounts
                    .iter()
                    .find(|account| account_view_matches_identity(account, identity))
                    .map(|account| account.id)
            });

            let (prompt, labels, actions) = build_menu(mode, &status, &list, current_saved);
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt(prompt)
                .items(&labels)
                .default(next_selectable_index(
                    &actions,
                    default_selection.min(labels.len().saturating_sub(1)),
                ))
                .interact()?;
            default_selection = next_selectable_index(&actions, selection);

            match actions[selection] {
                InteractiveAction::Separator => {}
                InteractiveAction::SaveCurrent => {
                    let output = self.save_current()?;
                    println!(
                        "{} {} ({})",
                        match output.action {
                            SaveAction::Created => "Saved",
                            SaveAction::Refreshed => "Refreshed",
                        },
                        output.account.email,
                        output.account.id
                    );
                }
                InteractiveAction::Activate(account_id) => {
                    let warnings = self.activation_preflight_warnings();
                    let showed_preflight = !warnings.is_empty();
                    if showed_preflight && !confirm_activation(&warnings)? {
                        continue;
                    }
                    let output = self.activate(account_id)?;
                    println!("Activated {} ({})", output.account.email, output.account.id);
                    if !showed_preflight && !output.warnings.is_empty() {
                        println!("Warnings:");
                        for warning in output.warnings {
                            println!("- {warning}");
                        }
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
                    let output = self.delete(account_id)?;
                    println!("Deleted saved snapshot {}", output.deleted_account_id);
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
                    let output = self.delete(account_id)?;
                    println!("Deleted saved snapshot {}", output.deleted_account_id);
                }
                InteractiveAction::ShowStatus => {
                    print_interactive_status(&status);
                }
                InteractiveAction::Quit => break,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum InteractiveAction {
    Separator,
    SaveCurrent,
    Activate(Uuid),
    Delete(Uuid),
    DeletePrompt,
    ShowStatus,
    Quit,
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
        parts.push("- Saved".to_owned());
    }
    parts.join(" ")
}

fn build_menu(
    mode: InteractiveMode,
    status: &StatusOutput,
    list: &ListOutput,
    current_saved: Option<Uuid>,
) -> (&'static str, Vec<String>, Vec<InteractiveAction>) {
    let mut labels = Vec::new();
    let mut actions = Vec::new();

    labels.push("-------------------- Accounts --------------------".to_owned());
    actions.push(InteractiveAction::Separator);

    for account in &list.accounts {
        labels.push(render_account_label(account));
        actions.push(match mode {
            InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
                InteractiveAction::Activate(account.id)
            }
            InteractiveMode::DeleteOnce => InteractiveAction::Delete(account.id),
        });
    }

    if matches!(mode, InteractiveMode::Persistent) {
        labels.push("-------------------- Actions ---------------------".to_owned());
        actions.push(InteractiveAction::Separator);

        if let Some(current) = status.current_account.as_ref() {
            labels.push(if current_saved.is_some() {
                format!("Refresh saved snapshot for {}", current.email)
            } else {
                format!("Save current account {}", current.email)
            });
            actions.push(InteractiveAction::SaveCurrent);
        }
        if !list.accounts.is_empty() {
            labels.push("Delete saved account".to_owned());
            actions.push(InteractiveAction::DeletePrompt);
        }
        labels.push("Show status".to_owned());
        actions.push(InteractiveAction::ShowStatus);
    }
    labels.push("Quit".to_owned());
    actions.push(InteractiveAction::Quit);

    let prompt = match mode {
        InteractiveMode::Persistent | InteractiveMode::ActivateOnce => {
            "Which account do you want to activate?"
        }
        InteractiveMode::DeleteOnce => "Which saved account do you want to delete?",
    };

    (prompt, labels, actions)
}

fn next_selectable_index(actions: &[InteractiveAction], preferred: usize) -> usize {
    if !matches!(actions.get(preferred), Some(InteractiveAction::Separator)) {
        return preferred;
    }
    actions
        .iter()
        .enumerate()
        .skip(preferred + 1)
        .find(|(_, action)| !matches!(action, InteractiveAction::Separator))
        .map(|(index, _)| index)
        .or_else(|| {
            actions
                .iter()
                .enumerate()
                .find(|(_, action)| !matches!(action, InteractiveAction::Separator))
                .map(|(index, _)| index)
        })
        .unwrap_or(0)
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
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Delete saved snapshot for {} ({})?",
            render_account_label(account),
            account.id
        ))
        .default(false)
        .interact()
        .map_err(Into::into)
}

fn confirm_activation(warnings: &[String]) -> Result<bool> {
    println!("Warnings:");
    for warning in warnings {
        println!("- {warning}");
    }
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Codex appears to be running. Continue with account activation?")
        .default(false)
        .interact()
        .map_err(Into::into)
}

fn print_interactive_status(status: &StatusOutput) {
    println!("Status");
    println!("Environment: {}", status.environment);
    println!("Codex root: {}", status.codex_root);
    match &status.current_account {
        Some(account) => {
            println!("Current account: {}", account.email);
            if let Some(plan) = &account.plan_label {
                println!("Plan: {plan}");
            }
        }
        None => println!("Current account: not logged in"),
    }
    println!("Saved accounts: {}", status.saved_accounts);
    if !status.process_warnings.is_empty() {
        println!("Warnings:");
        for warning in &status.process_warnings {
            println!("- {warning}");
        }
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

    fn sample_status(current_saved_id: Option<Uuid>) -> StatusOutput {
        StatusOutput {
            environment: EnvironmentKind::Windows,
            codex_root: "C:\\Users\\tester\\.codex".to_owned(),
            current_account: current_saved_id.map(|_| DisplayIdentity {
                email: "person@example.com".to_owned(),
                subject: Some("sub-1".to_owned()),
                name: Some("Tester".to_owned()),
                plan_label: Some("Pro".to_owned()),
            }),
            current_account_saved_id: current_saved_id,
            saved_accounts: usize::from(current_saved_id.is_some()),
            process_warnings: Vec::new(),
        }
    }

    fn sample_list(id: Uuid) -> ListOutput {
        ListOutput {
            environment: EnvironmentKind::Windows,
            accounts: vec![AccountView {
                id,
                email: "person@example.com".to_owned(),
                subject: Some("sub-1".to_owned()),
                name: Some("Tester".to_owned()),
                plan_label: Some("Pro".to_owned()),
                environment: EnvironmentKind::Windows,
                is_active: true,
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                last_activated_at: Some(OffsetDateTime::UNIX_EPOCH),
            }],
        }
    }

    #[test]
    fn activate_once_menu_only_lists_accounts_and_quit() {
        let id = Uuid::new_v4();
        let (_, labels, actions) = build_menu(
            InteractiveMode::ActivateOnce,
            &sample_status(Some(id)),
            &sample_list(id),
            Some(id),
        );
        assert_eq!(labels.len(), 3);
        assert!(matches!(actions[0], InteractiveAction::Separator));
        assert!(matches!(actions[1], InteractiveAction::Activate(actual) if actual == id));
        assert!(matches!(actions[2], InteractiveAction::Quit));
    }

    #[test]
    fn delete_once_menu_only_lists_deletes_and_quit() {
        let id = Uuid::new_v4();
        let (prompt, labels, actions) = build_menu(
            InteractiveMode::DeleteOnce,
            &sample_status(Some(id)),
            &sample_list(id),
            Some(id),
        );
        assert_eq!(prompt, "Which saved account do you want to delete?");
        assert_eq!(labels.len(), 3);
        assert!(matches!(actions[0], InteractiveAction::Separator));
        assert!(matches!(actions[1], InteractiveAction::Delete(actual) if actual == id));
        assert!(matches!(actions[2], InteractiveAction::Quit));
    }

    #[test]
    fn persistent_menu_keeps_refresh_in_actions() {
        let id = Uuid::new_v4();
        let (_, labels, actions) = build_menu(
            InteractiveMode::Persistent,
            &sample_status(Some(id)),
            &sample_list(id),
            Some(id),
        );
        assert_eq!(
            labels[0],
            "-------------------- Accounts --------------------"
        );
        assert_eq!(
            labels[2],
            "-------------------- Actions ---------------------"
        );
        assert!(matches!(actions[3], InteractiveAction::SaveCurrent));
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
            auth_json_fixture("before@example.com", "sub-1", Some("pro")),
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
