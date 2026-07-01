mod auto_start;
mod service;
mod tui;

use uuid::Uuid;

pub use auto_start::spawn_auto_start_usage_windows_worker;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) use auto_start::{
    run_auto_start_usage_windows_check_now, subscribe_auto_start_usage_windows_checks,
};

use crate::env::AppEnv;
use crate::model::{
    AccountUsageView, AccountView, DisplayIdentity, RunningCodexProcess, SavedAccountMetadata,
};
use crate::repository::SnapshotRepository;
use crate::usage::usage_error_requires_login;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractiveExit {
    Quit,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    SendToTray,
}

fn account_view(
    account: SavedAccountMetadata,
    active_id: Option<Uuid>,
    usage: Option<AccountUsageView>,
    usage_error: Option<String>,
) -> AccountView {
    let usage_error = usage_error.or(account.cached_usage_error);
    let usage = if usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        None
    } else {
        usage.or(account.cached_usage)
    };
    AccountView {
        id: account.id,
        email: account.email,
        subject: account.subject,
        name: account.name,
        plan_label: account.plan_label,
        workspace_id: account.workspace_id,
        workspace_name: account.workspace_name,
        environment: account.environment,
        is_active: active_id.is_some_and(|id| id == account.id),
        created_at: account.created_at,
        updated_at: account.updated_at,
        last_activated_at: account.last_activated_at,
        usage,
        usage_error,
        label: account.label,
    }
}

fn match_saved_account<'a>(
    accounts: &'a [SavedAccountMetadata],
    identity: &DisplayIdentity,
) -> Option<&'a SavedAccountMetadata> {
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
        workspace_id: account.workspace_id.clone(),
        workspace_name: account.workspace_name.clone(),
    }
    .matches(identity)
}

fn saved_identity(account: &SavedAccountMetadata) -> DisplayIdentity {
    DisplayIdentity {
        email: account.email.clone(),
        subject: account.subject.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
        workspace_id: account.workspace_id.clone(),
        workspace_name: account.workspace_name.clone(),
    }
}

fn subject_bound_identity_matches(expected: &DisplayIdentity, snapshot: &DisplayIdentity) -> bool {
    match (&expected.subject, &snapshot.subject) {
        (Some(left), Some(right)) => left == right && expected.workspace_matches(snapshot),
        _ => false,
    }
}

fn should_verify_activation_stability(
    force_running: bool,
    warnings: &[RunningCodexProcess],
) -> bool {
    force_running || !warnings.is_empty()
}
