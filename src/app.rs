mod auto_start;
mod service;

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
        is_archived: account.is_archived,
        target_app: account.target_app,
    }
}

fn match_saved_account<'a>(
    accounts: &'a [SavedAccountMetadata],
    identity: &DisplayIdentity,
) -> Option<&'a SavedAccountMetadata> {
    match_saved_account_with_app(accounts, identity, Some("codex"))
}

fn match_saved_account_with_app<'a>(
    accounts: &'a [SavedAccountMetadata],
    identity: &DisplayIdentity,
    target_app: Option<&str>,
) -> Option<&'a SavedAccountMetadata> {
    let app = target_app.unwrap_or("codex");
    if let Some(found) = accounts.iter().find(|account| {
        account.target_app.as_deref().unwrap_or("codex") == app
            && saved_identity(account).matches(identity)
    }) {
        return Some(found);
    }

    // Claude Code often exposes tokens without email (keychain / credentials).
    // Status uses a lightweight identity path that never blocks on CLI, so live
    // email can be the placeholder even when a saved Claude snapshot exists.
    // Bind to the sole / best-guess Claude account so tray quota still resolves.
    if app == "claude"
        && identity
            .email
            .eq_ignore_ascii_case(crate::claude::CLAUDE_UNKNOWN_EMAIL)
    {
        return fallback_unresolved_claude_account(accounts, identity);
    }
    None
}

/// When live Claude identity has no email, pick the best saved Claude snapshot.
fn fallback_unresolved_claude_account<'a>(
    accounts: &'a [SavedAccountMetadata],
    identity: &DisplayIdentity,
) -> Option<&'a SavedAccountMetadata> {
    let mut candidates: Vec<&'a SavedAccountMetadata> = accounts
        .iter()
        .filter(|account| {
            account.target_app.as_deref() == Some("claude") && !account.is_archived
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Prefer same subscription plan when multiple Claude accounts exist.
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
