use anyhow::{Context, Result};
use base64::Engine;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

use std::sync::{Mutex, OnceLock};

use crate::codex;
use crate::env::AppEnv;
use crate::model::{
    ActivateOutput, BatchRefreshFailure, BatchRefreshOutput, DeleteOutput, DisplayIdentity,
    ExportBundle, ImportOutput, ListOutput, PickBestOutput, PickBestScoreView, RenameOutput,
    RunningCodexProcess, SaveAction, SaveOutput, SnapshotBlob, StatusOutput, UsageOutput,
    UsageSource,
};
use crate::repository::SnapshotRepository;
use crate::secrets::SecretStore;
use crate::usage::{
    fetch_usage, usage_error_message, usage_error_requires_login, usage_target_from_snapshot,
};

use super::{
    App, account_view, match_saved_account, match_saved_account_with_app, saved_identity,
    should_verify_activation_stability, subject_bound_identity_matches,
};

static ACCOUNT_SWITCH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

impl<S> App<S>
where
    S: SecretStore,
{
    pub fn new(env: AppEnv, repository: SnapshotRepository<S>) -> Self {
        Self { env, repository }
    }

    pub(crate) fn env(&self) -> &AppEnv {
        &self.env
    }

    pub fn status(&self) -> Result<StatusOutput> {
        self.status_with_processes(true)
    }

    /// Like [`Self::status`], but process scanning is optional (UI polls skip it when unused).
    pub fn status_with_processes(&self, include_processes: bool) -> Result<StatusOutput> {
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
            process_warnings: if include_processes {
                crate::process::detect_running_codex_processes()
            } else {
                Vec::new()
            },
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
                .map(|account| account_view(account, active_id, None, None))
                .collect(),
        })
    }

    pub fn begin_add_account_session(&self) -> Result<()> {
        codex::begin_add_account_session(&self.env)
    }

    pub fn save_during_add_account_session(&self) -> Result<SaveOutput> {
        if !codex::add_account_session_active(&self.env) {
            anyhow::bail!("no add account session in progress");
        }
        if !self.env.codex_root.join("auth.json").exists() {
            anyhow::bail!("not logged in yet — complete Codex login first");
        }
        codex::ensure_cap_sid_exists(&self.env)?;
        // Marker stays until restore_add_account_backup / cancel clears the session.
        self.save_current()
    }

    pub fn cancel_add_account_session(&self) -> Result<()> {
        codex::cancel_add_account_session(&self.env)
    }

    /// Call after a successful interactive re-login + Save when not in add-account flow.
    pub fn clear_interactive_login_if_saved(&self) {
        if self.env.codex_root.join("auth.json").exists() {
            codex::clear_interactive_login_session(&self.env);
        }
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
        // OAuth finished for a normal save. Keep the guard during add-account until
        // backup restore completes (see save_during_add_account_session / cancel).
        if !codex::add_account_session_active(&self.env) {
            codex::clear_interactive_login_session(&self.env);
        }
        Ok(SaveOutput {
            account: account_view(metadata.clone(), Some(metadata.id), None, None),
            action: if created {
                SaveAction::Created
            } else {
                SaveAction::Refreshed
            },
        })
    }

    pub fn save_cursor_current(&self) -> Result<SaveOutput> {
        let live = crate::cursor::read_live_cursor_auth(&self.env)
            .context("failed to read live Cursor session — open Cursor, sign in, then try again")?;
        let (metadata, created) = self.repository.save_snapshot_with_app(
            &self.env.kind,
            &live.identity,
            &live.snapshot,
            Some("cursor".to_owned()),
        )?;
        Ok(SaveOutput {
            account: account_view(metadata.clone(), Some(metadata.id), None, None),
            action: if created {
                SaveAction::Created
            } else {
                SaveAction::Refreshed
            },
        })
    }

    pub fn save_claude_current(&self) -> Result<SaveOutput> {
        let live = crate::claude::read_live_claude_auth(&self.env)
            .context("no live Claude auth bundle found")?;
        let (metadata, created) = self.repository.save_snapshot_with_app(
            &self.env.kind,
            &live.identity,
            &live.snapshot,
            Some("claude".to_owned()),
        )?;
        Ok(SaveOutput {
            account: account_view(metadata.clone(), Some(metadata.id), None, None),
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

    pub fn start_login_for_saved_account(&self, account_id: Uuid) -> Result<ActivateOutput> {
        let metadata = self
            .repository
            .get_account(&self.env.kind, account_id)?
            .ok_or_else(|| anyhow::anyhow!("saved account not found"))?;
        // Do not restore then clear — that races Codex Desktop OAuth and can leave
        // the browser stuck on auth.openai.com with nothing listening on :1455.
        codex::begin_relogin_session(&self.env)
            .context("failed to prepare live Codex auth for re-login")?;
        Ok(ActivateOutput {
            account: account_view(metadata, Some(account_id), None, None),
            warnings: Vec::new(),
        })
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
        let _switch_guard = ACCOUNT_SWITCH_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| anyhow::anyhow!("account switch lock poisoned"))?;

        let account_metadata = self
            .repository
            .get_account(&self.env.kind, account_id)?
            .ok_or_else(|| anyhow::anyhow!("saved account not found"))?;
        let is_cursor = account_metadata.target_app.as_deref() == Some("cursor");
        let is_claude = account_metadata.target_app.as_deref() == Some("claude");

        let warnings = if is_cursor {
            crate::process::detect_switch_blocking_cursor_processes()
        } else if is_claude {
            crate::process::detect_switch_blocking_claude_processes()
        } else {
            crate::process::detect_switch_blocking_codex_processes()
        };

        if !is_cursor && !is_claude {
            self.refresh_current_saved_account_before_activation();
        }

        let (snapshot, snapshot_identity, restore_identity) =
            self.load_activation_target(account_id)?;
        let verify_stable = should_verify_activation_stability(force_running, &warnings);

        if is_cursor {
            crate::cursor::restore_cursor_snapshot(&self.env, &snapshot)
                .context("failed to restore the selected Cursor account snapshot")?;
        } else if is_claude {
            crate::claude::restore_claude_snapshot(&self.env, &snapshot)
                .context("failed to restore the selected Claude account snapshot")?;
        } else {
            codex::restore_snapshot(&self.env, &snapshot, &restore_identity, verify_stable)
                .context("failed to restore the selected account snapshot")?;
        }

        let metadata = self
            .repository
            .sync_activated_account(&self.env.kind, account_id, &snapshot_identity)
            .context("activated live auth but failed to update local metadata")?;
        let _ = crate::activity::log_account_activation(
            &self.env.app_data_dir,
            account_id,
            &metadata.email,
            metadata.label.as_deref(),
            Some(if is_cursor {
                "cursor"
            } else if is_claude {
                "claude"
            } else {
                "codex"
            }),
        );
        Ok(ActivateOutput {
            account: account_view(metadata, Some(account_id), None, None),
            warnings,
        })
    }

    fn refresh_current_saved_account_before_activation(&self) {
        let Ok(saved_accounts) = self.repository.list_accounts(&self.env.kind) else {
            return;
        };
        let Ok(Some(live)) = codex::try_read_live_auth_bundle(&self.env) else {
            return;
        };
        let Some(_current_saved) = match_saved_account(&saved_accounts, &live.identity) else {
            return;
        };
        let Ok(saved) = self.save_current() else {
            return;
        };
        let _ = self.usage(Some(saved.account.id));
    }

    fn load_activation_target(
        &self,
        account_id: Uuid,
    ) -> Result<(SnapshotBlob, DisplayIdentity, DisplayIdentity)> {
        let (metadata, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
        let expected_identity = saved_identity(&metadata);

        let snapshot_identity = if metadata.target_app.as_deref() == Some("cursor") {
            let file = snapshot
                .files
                .iter()
                .find(|f| f.name == "cursor_auth.json")
                .context("snapshot missing cursor_auth.json")?;
            let json_bytes =
                base64::engine::general_purpose::STANDARD.decode(&file.bytes_base64)?;
            let serialized_map: std::collections::HashMap<String, String> =
                serde_json::from_slice(&json_bytes)?;

            let email = serialized_map
                .get("cursorAuth/cachedEmail")
                .and_then(|val| base64::engine::general_purpose::STANDARD.decode(val).ok())
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .context("Cursor cached email not found in database")?;

            let cached_profile = serialized_map
                .get("cursorAuth/cachedScopedProfile")
                .and_then(|val| base64::engine::general_purpose::STANDARD.decode(val).ok())
                .and_then(|bytes| String::from_utf8(bytes).ok());
            let name = cached_profile
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|val| {
                    val.get("displayName")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                });

            let plan_label = serialized_map
                .get("cursorAuth/stripeMembershipType")
                .and_then(|val| base64::engine::general_purpose::STANDARD.decode(val).ok())
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map(|p| {
                    if p.eq_ignore_ascii_case("pro") {
                        "Pro".to_owned()
                    } else {
                        p
                    }
                });

            DisplayIdentity {
                email,
                subject: None,
                name,
                plan_label,
                workspace_id: None,
                workspace_name: None,
            }
        } else if metadata.target_app.as_deref() == Some("claude") {
            DisplayIdentity {
                email: metadata.email.clone(),
                subject: metadata.subject.clone(),
                name: metadata.name.clone(),
                plan_label: metadata.plan_label.clone(),
                workspace_id: metadata.workspace_id.clone(),
                workspace_name: metadata.workspace_name.clone(),
            }
        } else {
            codex::identity_from_snapshot(&snapshot)?
        };

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

    pub fn activation_preflight_warnings(&self) -> Vec<RunningCodexProcess> {
        crate::process::detect_switch_blocking_codex_processes()
    }

    pub fn activation_preflight_warnings_for_account(
        &self,
        account_id: Uuid,
    ) -> Vec<RunningCodexProcess> {
        let target_app = self
            .repository
            .get_account(&self.env.kind, account_id)
            .ok()
            .flatten()
            .and_then(|meta| meta.target_app);
        if target_app.as_deref() == Some("cursor") {
            return crate::process::detect_switch_blocking_cursor_processes();
        } else if target_app.as_deref() == Some("claude") {
            return crate::process::detect_switch_blocking_claude_processes();
        }
        crate::process::detect_switch_blocking_codex_processes()
    }

    pub fn refresh_saved_usage_cache(&self) -> Result<()> {
        let _ = self.refresh_all_usage()?;
        Ok(())
    }

    pub fn refresh_all_usage(&self) -> Result<BatchRefreshOutput> {
        let accounts = self.repository.list_accounts(&self.env.kind)?;
        let mut refreshed = Vec::new();
        let mut failed = Vec::new();
        for account in accounts {
            match self.usage(Some(account.id)) {
                Ok(_) => refreshed.push(account.id),
                Err(error) => failed.push(BatchRefreshFailure {
                    account_id: account.id,
                    email: account.email.clone(),
                    error: format!("{error:#}"),
                }),
            }
        }
        Ok(BatchRefreshOutput {
            total: refreshed.len() + failed.len(),
            refreshed,
            failed,
        })
    }

    pub fn usage(&self, account_id: Option<Uuid>) -> Result<UsageOutput> {
        match account_id {
            Some(account_id) => {
                let metadata = self
                    .repository
                    .get_account(&self.env.kind, account_id)?
                    .ok_or_else(|| anyhow::anyhow!("saved account not found"))?;
                let app = metadata.target_app.as_deref().unwrap_or("codex");
                match app {
                    "cursor" => self.usage_cursor_saved(account_id, &metadata),
                    "claude" => self.usage_claude_saved(account_id, &metadata),
                    _ => self.usage_codex_saved(account_id),
                }
            }
            None => self.usage_live(true),
        }
    }

    fn usage_codex_saved(&self, account_id: Uuid) -> Result<UsageOutput> {
        let (snapshot, _, _) = self.load_activation_target(account_id)?;
        let original_snapshot = snapshot.clone();
        let target = usage_target_from_snapshot(
            self.env.kind.clone(),
            snapshot,
            UsageSource::SavedAccessToken,
            true,
        )?;
        let (output, refreshed_snapshot) = match fetch_usage(target) {
            Ok(result) => result,
            Err(error) => {
                let _ = self.repository.record_usage_error(
                    &self.env.kind,
                    account_id,
                    usage_error_message(&error),
                );
                return Err(error);
            }
        };
        if refreshed_snapshot != original_snapshot {
            self.restore_refreshed_live_auth_if_still_current(
                &original_snapshot,
                &refreshed_snapshot,
                &output.account,
            )
            .context("refreshed saved auth but failed to update matching live auth files")?;
        }
        self.repository.replace_snapshot(
            &self.env.kind,
            account_id,
            &output.account,
            &refreshed_snapshot,
            Some(output.usage.clone()),
        )?;
        Ok(output)
    }

    fn usage_cursor_saved(
        &self,
        account_id: Uuid,
        metadata: &crate::model::SavedAccountMetadata,
    ) -> Result<UsageOutput> {
        let (_, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
        let token = crate::cursor_usage::access_token_from_snapshot(&snapshot).map_err(|e| {
            let _ = self.repository.record_usage_error(
                &self.env.kind,
                account_id,
                usage_error_message(&e),
            );
            e
        })?;
        let usage = crate::cursor_usage::fetch_cursor_usage(&token).map_err(|e| {
            let _ = self.repository.record_usage_error(
                &self.env.kind,
                account_id,
                usage_error_message(&e),
            );
            e
        })?;
        let identity = crate::model::DisplayIdentity {
            email: metadata.email.clone(),
            subject: metadata.subject.clone(),
            name: metadata.name.clone(),
            plan_label: metadata.plan_label.clone(),
            workspace_id: metadata.workspace_id.clone(),
            workspace_name: metadata.workspace_name.clone(),
        };
        self.repository.replace_snapshot(
            &self.env.kind,
            account_id,
            &identity,
            &snapshot,
            Some(usage.clone()),
        )?;
        Ok(UsageOutput {
            environment: self.env.kind.clone(),
            account: identity,
            usage,
        })
    }

    fn usage_claude_saved(
        &self,
        account_id: Uuid,
        metadata: &crate::model::SavedAccountMetadata,
    ) -> Result<UsageOutput> {
        let (_, snapshot) = self.repository.load_snapshot(&self.env.kind, account_id)?;
        let token = crate::claude_usage::access_token_from_snapshot(&snapshot).map_err(|e| {
            let _ = self.repository.record_usage_error(
                &self.env.kind,
                account_id,
                usage_error_message(&e),
            );
            e
        })?;
        let usage = crate::claude_usage::fetch_claude_usage(&token).map_err(|e| {
            let _ = self.repository.record_usage_error(
                &self.env.kind,
                account_id,
                usage_error_message(&e),
            );
            e
        })?;
        let identity = crate::model::DisplayIdentity {
            email: metadata.email.clone(),
            subject: metadata.subject.clone(),
            name: metadata.name.clone(),
            plan_label: metadata.plan_label.clone(),
            workspace_id: metadata.workspace_id.clone(),
            workspace_name: metadata.workspace_name.clone(),
        };
        self.repository.replace_snapshot(
            &self.env.kind,
            account_id,
            &identity,
            &snapshot,
            Some(usage.clone()),
        )?;
        Ok(UsageOutput {
            environment: self.env.kind.clone(),
            account: identity,
            usage,
        })
    }

    fn usage_live(&self, retry_if_live_changed: bool) -> Result<UsageOutput> {
        let live = codex::read_live_auth_bundle(&self.env).with_context(|| {
            format!(
                "no live Codex auth bundle found at {}",
                self.env.codex_root.display()
            )
        })?;
        let live_identity = live.identity.clone();
        let live_snapshot = live.snapshot.clone();
        let target = usage_target_from_snapshot(
            self.env.kind.clone(),
            live.snapshot,
            UsageSource::LiveAccessToken,
            true,
        )?;
        let (output, refreshed_snapshot) = match fetch_usage(target) {
            Ok(result) => result,
            Err(error) => {
                if retry_if_live_changed
                    && usage_error_requires_login(&format!("{error:#}"))
                    && self.live_snapshot_changed_since(&live_snapshot)
                {
                    return self.usage_live(false);
                }
                self.record_usage_error_for_identity(&live_identity, &error);
                return Err(error);
            }
        };
        if refreshed_snapshot != live_snapshot {
            self.restore_refreshed_live_auth_if_still_current(
                &live_snapshot,
                &refreshed_snapshot,
                &output.account,
            )
            .context("refreshed live auth but failed to update local auth files")?;
        }
        if let Some(account_id) = self.saved_account_id_for_identity(&live_identity) {
            self.repository.replace_snapshot(
                &self.env.kind,
                account_id,
                &output.account,
                &refreshed_snapshot,
                Some(output.usage.clone()),
            )?;
        }
        Ok(output)
    }

    fn restore_refreshed_live_auth_if_still_current(
        &self,
        previous_snapshot: &SnapshotBlob,
        refreshed_snapshot: &SnapshotBlob,
        identity: &DisplayIdentity,
    ) -> Result<()> {
        let lock = codex::acquire_auth_write_lock(&self.env)?;
        if codex::live_bundle_matches_snapshot(&self.env, previous_snapshot)? {
            codex::restore_snapshot_with_lock(
                &lock,
                &self.env,
                refreshed_snapshot,
                identity,
                false,
            )?;
        }
        Ok(())
    }

    fn live_snapshot_changed_since(&self, snapshot: &SnapshotBlob) -> bool {
        !live_bundle_still_matches_snapshot(&self.env, snapshot)
    }

    fn saved_account_id_for_identity(&self, identity: &DisplayIdentity) -> Option<Uuid> {
        self.repository
            .list_accounts(&self.env.kind)
            .ok()
            .and_then(|accounts| match_saved_account(&accounts, identity).map(|account| account.id))
    }

    fn record_usage_error_for_identity(&self, identity: &DisplayIdentity, error: &anyhow::Error) {
        let Ok(accounts) = self.repository.list_accounts(&self.env.kind) else {
            return;
        };
        let Some(account) = match_saved_account(&accounts, identity) else {
            return;
        };
        let _ = self.repository.record_usage_error(
            &self.env.kind,
            account.id,
            usage_error_message(error),
        );
    }

    pub fn delete(&self, account_id: Uuid) -> Result<DeleteOutput> {
        self.repository
            .delete_snapshot(&self.env.kind, account_id)?;
        Ok(DeleteOutput {
            deleted_account_id: account_id,
        })
    }

    pub fn find_account_by_id_or_email(
        &self,
        query: &str,
    ) -> Result<crate::model::SavedAccountMetadata> {
        let accounts = self.repository.list_accounts(&self.env.kind)?;
        if let Ok(id) = Uuid::parse_str(query)
            && let Some(account) = accounts.iter().find(|a| a.id == id)
        {
            return Ok(account.clone());
        }
        let query_lower = query.to_ascii_lowercase();
        let matched = accounts.into_iter().find(|account| {
            account.email.to_ascii_lowercase() == query_lower
                || account
                    .label
                    .as_ref()
                    .is_some_and(|label| label.to_ascii_lowercase() == query_lower)
        });
        matched.ok_or_else(|| anyhow::anyhow!("no saved account found matching '{}'", query))
    }

    pub fn pick_best_account(&self, refresh: bool, activate: bool) -> Result<PickBestOutput> {
        if refresh {
            self.refresh_saved_usage_cache()?;
        }
        let accounts = self.repository.list_accounts(&self.env.kind)?;
        let status = self.status()?;
        let active_id = status.current_account_saved_id;
        let now = OffsetDateTime::now_utc();
        let scores = accounts
            .iter()
            .map(|account| {
                crate::quota_scoring::score_saved_account(
                    account.id,
                    &account.email,
                    account.label.as_deref(),
                    account.cached_usage.as_ref(),
                    account.cached_usage_error.as_deref(),
                    now,
                )
            })
            .collect::<Vec<_>>();
        let score_views = scores
            .iter()
            .map(|entry| PickBestScoreView {
                account_id: entry.account_id,
                email: entry.email.clone(),
                label: entry.label.clone(),
                score: entry.eligible.then_some(entry.score),
                eligible: entry.eligible,
                weekly_used_percent: entry.weekly_used_percent,
                five_hour_used_percent: entry.five_hour_used_percent,
                detail: entry.detail.clone(),
            })
            .collect::<Vec<_>>();
        let Some(best_id) = crate::quota_scoring::pick_best_account_id(&scores) else {
            anyhow::bail!("no eligible saved account with usable quota");
        };
        if active_id == Some(best_id) || !activate {
            let metadata = self
                .repository
                .get_account(&self.env.kind, best_id)?
                .context("best account metadata missing")?;
            return Ok(PickBestOutput {
                switched: false,
                account: account_view(metadata, active_id, None, None),
                scores: score_views,
            });
        }
        let output = self.activate_with_running_policy(best_id, true)?;
        let best_score = scores
            .iter()
            .find(|entry| entry.account_id == best_id)
            .map(|entry| entry.score)
            .unwrap_or(0.0);
        let _ = crate::activity::log_pick_best(
            &self.env.app_data_dir,
            best_id,
            &output.account.email,
            output.account.label.as_deref(),
            best_score,
        );
        Ok(PickBestOutput {
            switched: true,
            account: output.account,
            scores: score_views,
        })
    }

    pub fn login_and_save(&self) -> Result<SaveOutput> {
        let status = std::process::Command::new(crate::process::codex_cli_path())
            .arg("login")
            .status()
            .context("failed to run `codex login`; make sure Codex is installed")?;
        if !status.success() {
            anyhow::bail!("`codex login` failed with {status}");
        }
        self.save_current()
    }

    pub fn export_accounts(&self, account_ids: Option<Vec<Uuid>>) -> Result<ExportBundle> {
        crate::import_export::export_accounts(
            &self.repository,
            &self.env.kind,
            account_ids.as_deref(),
        )
    }

    pub fn import_auth_path(
        &self,
        auth_path: &std::path::Path,
        label: Option<String>,
    ) -> Result<ImportOutput> {
        crate::import_export::import_auth_file(&self.repository, &self.env.kind, auth_path, label)
    }

    pub fn import_bundle(&self, bundle: &ExportBundle) -> Result<Vec<ImportOutput>> {
        crate::import_export::import_export_bundle(&self.repository, &self.env.kind, bundle)
    }

    pub fn rename_account(&self, account_id: Uuid, label: Option<String>) -> Result<RenameOutput> {
        let metadata = self
            .repository
            .set_account_label(&self.env.kind, account_id, label)?;
        let active_id = self
            .status()?
            .current_account_saved_id
            .filter(|id| *id == account_id);
        Ok(RenameOutput {
            account: account_view(metadata, active_id, None, None),
        })
    }

    pub fn set_account_archived(&self, account_id: Uuid, archived: bool) -> Result<()> {
        let _ = self
            .repository
            .set_account_archived(&self.env.kind, account_id, archived)?;
        Ok(())
    }

    pub fn exec_with_temporary_account(
        &self,
        account_id: Uuid,
        command: &[String],
    ) -> Result<std::process::ExitStatus> {
        if command.is_empty() {
            anyhow::bail!("no command specified to execute");
        }
        let auth_lock = codex::acquire_auth_write_lock(&self.env)?;
        let original_bundle = codex::try_read_live_auth_bundle(&self.env)?;
        let _guard = ActiveSnapshotGuard {
            env: &self.env,
            original_bundle,
            active_now: true,
            auth_lock,
        };
        let (snapshot, _snapshot_identity, restore_identity) =
            self.load_activation_target(account_id)?;
        codex::restore_snapshot_with_lock(
            &_guard.auth_lock,
            &self.env,
            &snapshot,
            &restore_identity,
            false,
        )
        .context("failed to temporarily restore target account snapshot")?;

        let mut child = std::process::Command::new(&command[0])
            .args(&command[1..])
            .spawn()
            .with_context(|| format!("failed to start command '{}'", command[0]))?;
        let status = child.wait().context("failed to wait for child process")?;
        Ok(status)
    }

    pub fn cursor_status(&self) -> Result<StatusOutput> {
        self.cursor_status_with_processes(true)
    }

    pub fn cursor_status_with_processes(&self, include_processes: bool) -> Result<StatusOutput> {
        let saved_accounts = self
            .repository
            .list_accounts(&self.env.kind)?
            .into_iter()
            .filter(|acc| acc.target_app.as_deref() == Some("cursor"))
            .collect::<Vec<_>>();
        // Identity-only read keeps status/popover polls cheap and avoids RW locks
        // on Cursor's large state DB while Cursor itself is running.
        let live = crate::cursor::try_read_live_cursor_identity(&self.env).unwrap_or(None);
        let current_saved_id = live.as_ref().and_then(|identity| {
            match_saved_account_with_app(&saved_accounts, identity, Some("cursor"))
                .map(|account| account.id)
        });
        Ok(StatusOutput {
            environment: self.env.kind.clone(),
            codex_root: crate::cursor::cursor_db_path(&self.env)?
                .display()
                .to_string(),
            current_account: live,
            current_account_saved_id: current_saved_id,
            saved_accounts: saved_accounts.len(),
            process_warnings: if include_processes {
                crate::process::detect_running_cursor_processes()
            } else {
                Vec::new()
            },
        })
    }

    pub fn claude_status(&self) -> Result<StatusOutput> {
        self.claude_status_with_processes(true)
    }

    pub fn claude_status_with_processes(&self, include_processes: bool) -> Result<StatusOutput> {
        let saved_accounts = self
            .repository
            .list_accounts(&self.env.kind)?
            .into_iter()
            .filter(|acc| acc.target_app.as_deref() == Some("claude"))
            .collect::<Vec<_>>();
        let live = crate::claude::try_read_live_claude_identity(&self.env).unwrap_or(None);
        let current_saved_id = live.as_ref().and_then(|identity| {
            match_saved_account_with_app(&saved_accounts, identity, Some("claude"))
                .map(|account| account.id)
        });
        Ok(StatusOutput {
            environment: self.env.kind.clone(),
            codex_root: crate::claude::claude_dir(&self.env).display().to_string(),
            current_account: live,
            current_account_saved_id: current_saved_id,
            saved_accounts: saved_accounts.len(),
            process_warnings: if include_processes {
                crate::process::detect_running_claude_processes()
            } else {
                Vec::new()
            },
        })
    }

    pub fn import_cookies_json(
        &self,
        provider: &str,
        json_text: &str,
        label: Option<String>,
    ) -> Result<ImportOutput> {
        let mut provider = provider.trim().to_ascii_lowercase();
        if provider.is_empty() || provider == "auto" {
            provider = crate::cookie_import::detect_provider_from_json(json_text).to_owned();
        }
        let (imported, target_app) = match provider.as_str() {
            "codex" | "chatgpt" | "openai" => {
                let imported = crate::cookie_import::import_codex_from_cookies_json(json_text)?;
                crate::codex::validate_import_snapshot(&imported.snapshot)?;
                (imported, None)
            }
            "cursor" => {
                let imported = crate::cookie_import::import_cursor_from_cookies_json(json_text)?;
                (imported, Some("cursor".to_owned()))
            }
            other => anyhow::bail!(crate::cookie_import::unsupported_provider_message(other)),
        };

        let (metadata, created) = if let Some(app) = target_app {
            self.repository.save_snapshot_with_app(
                &self.env.kind,
                &imported.identity,
                &imported.snapshot,
                Some(app),
            )?
        } else {
            self.repository.save_snapshot(
                &self.env.kind,
                &imported.identity,
                &imported.snapshot,
            )?
        };
        let metadata = if label.is_some() {
            self.repository
                .set_account_label(&self.env.kind, metadata.id, label)?
        } else {
            metadata
        };
        Ok(ImportOutput {
            account_id: metadata.id,
            email: metadata.email,
            label: metadata.label,
            created,
            warnings: imported.warnings,
        })
    }

    pub fn is_cursor_account(&self, account_id: Uuid) -> bool {
        self.repository
            .get_account(&self.env.kind, account_id)
            .ok()
            .flatten()
            .and_then(|acc| acc.target_app)
            .as_deref()
            == Some("cursor")
    }

    pub fn is_claude_account(&self, account_id: Uuid) -> bool {
        self.repository
            .get_account(&self.env.kind, account_id)
            .ok()
            .flatten()
            .and_then(|acc| acc.target_app)
            .as_deref()
            == Some("claude")
    }
}

struct ActiveSnapshotGuard<'a> {
    env: &'a AppEnv,
    original_bundle: Option<crate::codex::LiveAuthBundle>,
    active_now: bool,
    auth_lock: crate::codex::AuthWriteLock,
}

impl<'a> Drop for ActiveSnapshotGuard<'a> {
    fn drop(&mut self) {
        if self.active_now {
            if let Some(original) = &self.original_bundle {
                let _ = crate::codex::restore_snapshot_with_lock(
                    &self.auth_lock,
                    self.env,
                    &original.snapshot,
                    &original.identity,
                    false,
                );
            } else {
                for file_name in crate::model::AUTH_FILES {
                    let _ = std::fs::remove_file(self.env.codex_root.join(file_name));
                }
            }
        }
    }
}

fn live_bundle_still_matches_snapshot(env: &AppEnv, snapshot: &SnapshotBlob) -> bool {
    for attempt in 0..3 {
        if codex::live_bundle_matches_snapshot(env, snapshot).unwrap_or(false) {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use tempfile::tempdir;

    use super::*;
    use crate::codex::auth_json_fixture;
    use time::OffsetDateTime;

    use crate::model::SnapshotFile;
    use crate::model::{
        AccountUsageView, DisplayIdentity, EnvironmentKind, UsageSource, UsageWindowView,
    };
    use crate::secrets::test_support::MemorySecretStore;

    fn fixture_id_token(email: &str, subject: &str, plan: Option<&str>) -> String {
        let auth: serde_json::Value =
            serde_json::from_str(&auth_json_fixture(email, subject, plan)).expect("auth fixture");
        auth["tokens"]["id_token"]
            .as_str()
            .expect("id token")
            .to_owned()
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
        let output = app.list().expect("list");
        assert_eq!(output.accounts.len(), 1);
        assert_eq!(output.accounts[0].email, "saved@example.com");
        assert!(!output.accounts[0].is_active);
    }

    #[test]
    fn list_surfaces_cached_usage_error() {
        let temp = tempdir().expect("tempdir");
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "expired@example.com".to_owned(),
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
            .expect("save")
            .0;
        repo.replace_snapshot(
            &env.kind,
            saved.id,
            &DisplayIdentity {
                email: "expired@example.com".to_owned(),
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
            Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: Some(UsageWindowView {
                    used_percent: 0,
                    remaining_percent: 100,
                    reset_at: OffsetDateTime::UNIX_EPOCH,
                }),
                credits: None,
            }),
        )
        .expect("replace");
        repo.record_usage_error(
            &env.kind,
            saved.id,
            "Login required: Codex auth expired or was logged out.".to_owned(),
        )
        .expect("record usage error");

        let app = App::new(env, repo);
        let output = app.list().expect("list");

        assert_eq!(
            output.accounts[0].usage_error.as_deref(),
            Some("Login required: Codex auth expired or was logged out.")
        );
        assert!(output.accounts[0].usage.is_none());
    }

    #[test]
    fn list_keeps_cached_usage_for_transient_usage_error() {
        let temp = tempdir().expect("tempdir");
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let identity = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: None,
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let snapshot = SnapshotBlob {
            schema_version: 1,
            files: vec![],
        };
        let saved = repo
            .save_snapshot(&env.kind, &identity, &snapshot)
            .expect("save")
            .0;
        repo.replace_snapshot(
            &env.kind,
            saved.id,
            &identity,
            &snapshot,
            Some(AccountUsageView {
                source: UsageSource::SavedAccessToken,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
                five_hour: None,
                weekly: Some(UsageWindowView {
                    used_percent: 10,
                    remaining_percent: 90,
                    reset_at: OffsetDateTime::UNIX_EPOCH,
                }),
                credits: None,
            }),
        )
        .expect("replace");
        repo.record_usage_error(
            &env.kind,
            saved.id,
            "Usage unavailable: failed to query Codex usage".to_owned(),
        )
        .expect("record usage error");

        let app = App::new(env, repo);
        let output = app.list().expect("list");

        assert!(output.accounts[0].usage.is_some());
        assert_eq!(
            output.accounts[0].usage_error.as_deref(),
            Some("Usage unavailable: failed to query Codex usage")
        );
    }

    #[test]
    fn saved_usage_refresh_updates_matching_live_auth() {
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
        std::fs::write(env.codex_root.join("cap_sid"), "sid-active").expect("cap");
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let app = App::new(env.clone(), repo);
        let saved = app.save_current().expect("save current");

        let (_, original_snapshot) = app
            .repository
            .load_snapshot(&env.kind, saved.account.id)
            .expect("original snapshot");
        let refreshed_snapshot = refreshed_auth_snapshot(
            &original_snapshot,
            "access-new",
            "refresh-new",
            "active@example.com",
            "sub-1",
            Some("pro"),
        );
        let refreshed_identity = DisplayIdentity {
            email: "active@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: Some("acct".to_owned()),
            workspace_name: None,
        };

        app.restore_refreshed_live_auth_if_still_current(
            &original_snapshot,
            &refreshed_snapshot,
            &refreshed_identity,
        )
        .expect("restore refreshed live auth");
        app.repository
            .replace_snapshot(
                &env.kind,
                saved.account.id,
                &refreshed_identity,
                &refreshed_snapshot,
                None,
            )
            .expect("replace saved snapshot");

        let live_auth =
            std::fs::read_to_string(env.codex_root.join("auth.json")).expect("live auth");
        assert!(live_auth.contains("access-new"));
        assert!(live_auth.contains("refresh-new"));

        let (_, saved_snapshot) = app
            .repository
            .load_snapshot(&env.kind, saved.account.id)
            .expect("saved snapshot");
        let saved_auth = saved_snapshot
            .files
            .iter()
            .find(|file| file.name == "auth.json")
            .expect("saved auth");
        let saved_auth_json = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(&saved_auth.bytes_base64)
                .expect("decode saved auth"),
        )
        .expect("saved auth utf8");
        assert!(saved_auth_json.contains("access-new"));
        assert!(saved_auth_json.contains("refresh-new"));
    }

    fn refreshed_auth_snapshot(
        snapshot: &SnapshotBlob,
        access_token: &str,
        refresh_token: &str,
        email: &str,
        subject: &str,
        plan: Option<&str>,
    ) -> SnapshotBlob {
        let mut refreshed = snapshot.clone();
        let auth_index = refreshed
            .files
            .iter()
            .position(|file| file.name == "auth.json")
            .expect("auth file");
        let auth_json = base64::engine::general_purpose::STANDARD
            .decode(&refreshed.files[auth_index].bytes_base64)
            .expect("decode auth");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&auth_json).expect("parse auth json");
        auth["tokens"]["access_token"] = serde_json::Value::String(access_token.to_owned());
        auth["tokens"]["refresh_token"] = serde_json::Value::String(refresh_token.to_owned());
        auth["tokens"]["id_token"] =
            serde_json::Value::String(fixture_id_token(email, subject, plan));
        refreshed.files[auth_index].bytes_base64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&auth).expect("encode auth json"));
        refreshed
    }

    #[test]
    fn subject_bound_identity_requires_matching_subject() {
        let expected = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let missing_subject = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: None,
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let wrong_subject = DisplayIdentity {
            email: "person@example.com".to_owned(),
            subject: Some("sub-2".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
        };
        let matching_subject = DisplayIdentity {
            email: "other@example.com".to_owned(),
            subject: Some("sub-1".to_owned()),
            name: Some("Tester".to_owned()),
            plan_label: Some("Pro".to_owned()),
            workspace_id: None,
            workspace_name: None,
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
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("after@example.com", "sub-1", Some("plus")),
                            ),
                        },
                        SnapshotFile {
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
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("wrong@example.com", "sub-wrong", Some("plus")),
                            ),
                        },
                        SnapshotFile {
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
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("new@example.com", "sub-new", Some("plus")),
                            ),
                        },
                        SnapshotFile {
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

    #[test]
    fn find_account_by_id_or_email_works() {
        let temp = tempdir().expect("tempdir");
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };
        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "test@example.com".to_owned(),
                    subject: Some("sub-test".to_owned()),
                    name: Some("Test".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env, repo);

        // Find by exact email
        let found = app
            .find_account_by_id_or_email("test@example.com")
            .expect("found");
        assert_eq!(found.id, saved.id);

        // Find by case-insensitive email
        let found = app
            .find_account_by_id_or_email("TEST@EXAMPLE.COM")
            .expect("found");
        assert_eq!(found.id, saved.id);

        // Find by UUID
        let found = app
            .find_account_by_id_or_email(&saved.id.to_string())
            .expect("found");
        assert_eq!(found.id, saved.id);

        // Not found
        assert!(
            app.find_account_by_id_or_email("other@example.com")
                .is_err()
        );
    }

    #[test]
    fn exec_with_temporary_account_restores_original_bundle() {
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
            auth_json_fixture("original@example.com", "sub-orig", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid-orig").expect("cap");

        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "temp@example.com".to_owned(),
                    subject: Some("sub-temp".to_owned()),
                    name: Some("Temp".to_owned()),
                    plan_label: Some("Plus".to_owned()),
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture("temp@example.com", "sub-temp", Some("plus")),
                            ),
                        },
                        SnapshotFile {
                            name: "cap_sid".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD
                                .encode("sid-temp"),
                        },
                    ],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env.clone(), repo);

        // Run a simple command (e.g. echo "hello")
        #[cfg(windows)]
        let command = vec!["cmd".to_owned(), "/c".to_owned(), "echo hello".to_owned()];
        #[cfg(not(windows))]
        let command = vec!["echo".to_owned(), "hello".to_owned()];

        let status = app
            .exec_with_temporary_account(saved.id, &command)
            .expect("exec");
        assert!(status.success());

        // Verify the original account is restored
        let live = crate::codex::read_live_auth_bundle(&env).expect("live bundle");
        assert_eq!(live.identity.email, "original@example.com");
    }

    #[test]
    fn start_login_for_saved_account_clears_live_auth_and_marks_login_session() {
        let temp = tempdir().expect("tempdir");
        let env = AppEnv {
            kind: EnvironmentKind::Linux,
            home_dir: temp.path().to_path_buf(),
            codex_root: temp.path().join(".codex"),
            app_data_dir: temp.path().join("app"),
        };

        // Seed live auth so clear has something to remove.
        std::fs::create_dir_all(&env.codex_root).expect("codex root");
        std::fs::write(
            env.codex_root.join("auth.json"),
            auth_json_fixture("expired@example.com", "sub-expired", Some("pro")),
        )
        .expect("auth");
        std::fs::write(env.codex_root.join("cap_sid"), "sid-expired").expect("sid");

        let repo = SnapshotRepository::new(&env.app_data_dir, MemorySecretStore::default());
        let saved = repo
            .save_snapshot(
                &env.kind,
                &DisplayIdentity {
                    email: "expired@example.com".to_owned(),
                    subject: Some("sub-expired".to_owned()),
                    name: Some("Expired".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                    workspace_id: None,
                    workspace_name: None,
                },
                &SnapshotBlob {
                    schema_version: 1,
                    files: vec![
                        SnapshotFile {
                            name: "auth.json".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                                auth_json_fixture(
                                    "expired@example.com",
                                    "sub-expired",
                                    Some("pro"),
                                ),
                            ),
                        },
                        SnapshotFile {
                            name: "cap_sid".to_owned(),
                            bytes_base64: base64::engine::general_purpose::STANDARD
                                .encode("sid-expired"),
                        },
                    ],
                },
            )
            .expect("save")
            .0;

        let app = App::new(env.clone(), repo);

        let output = app
            .start_login_for_saved_account(saved.id)
            .expect("start login");

        assert_eq!(output.account.email, "expired@example.com");
        assert!(!env.codex_root.join("auth.json").exists());
        assert!(!env.codex_root.join("cap_sid").exists());
        assert!(crate::codex::interactive_login_in_progress(&env));
    }
}
