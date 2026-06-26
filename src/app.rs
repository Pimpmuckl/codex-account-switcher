mod auto_start;
mod service;
mod tui;

use uuid::Uuid;

pub use auto_start::spawn_auto_start_usage_windows_worker;
#[cfg(windows)]
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
    #[cfg(windows)]
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
        account_key: account.account_key,
        email: account.email,
        name: account.name,
        plan_label: account.plan_label,
        is_active: active_id.is_some_and(|id| id == account.id),
        created_at: account.created_at,
        updated_at: account.updated_at,
        last_activated_at: account.last_activated_at,
        usage,
        usage_error,
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
        account_key: account.account_key.clone(),
        email: account.email.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
    }
    .matches(identity)
}

fn saved_identity(account: &SavedAccountMetadata) -> DisplayIdentity {
    DisplayIdentity {
        account_key: account.account_key.clone(),
        email: account.email.clone(),
        name: account.name.clone(),
        plan_label: account.plan_label.clone(),
    }
}

fn should_verify_activation_stability(
    force_running: bool,
    warnings: &[RunningCodexProcess],
) -> bool {
    force_running || !warnings.is_empty()
}
