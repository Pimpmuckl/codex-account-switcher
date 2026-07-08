use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const METADATA_SCHEMA_VERSION: u32 = 1;
pub const AUTH_FILES: [&str; 2] = ["auth.json", "cap_sid"];
/// User-facing label for `settings.auto_start_usage_windows`.
pub const AUTO_REFRESH_QUOTA_ON_RESET_LABEL: &str = "Auto-refresh quota on reset";
/// Weekly quota window reset time has passed (new window may be available).
pub const QUOTA_PAST_RESET_LABEL: &str = "Past reset";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    Windows,
    Wsl,
    Linux,
    Macos,
}

impl std::fmt::Display for EnvironmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Windows => "windows",
            Self::Wsl => "wsl",
            Self::Linux => "linux",
            Self::Macos => "macos",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotBlob {
    pub schema_version: u32,
    pub files: Vec<SnapshotFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotFile {
    pub name: String,
    pub bytes_base64: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayIdentity {
    pub email: String,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub plan_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
}

impl DisplayIdentity {
    pub fn matches(&self, other: &Self) -> bool {
        let same_user = match (&self.subject, &other.subject) {
            (Some(left), Some(right)) => left == right,
            _ => self.email.eq_ignore_ascii_case(&other.email),
        };
        same_user && self.workspace_matches(other)
    }

    pub fn workspace_matches(&self, other: &Self) -> bool {
        match (&self.workspace_id, &other.workspace_id) {
            (Some(left), Some(right)) => left == right,
            (None, None) => match (&self.workspace_name, &other.workspace_name) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => true,
            },
            _ => true,
        }
    }

    pub fn workspace_label(&self) -> Option<&str> {
        self.workspace_name
            .as_deref()
            .or_else(|| {
                self.workspace_id
                    .as_deref()
                    .filter(|id| Uuid::parse_str(id).is_err())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::DisplayIdentity;

    fn identity(email: &str, subject: Option<&str>) -> DisplayIdentity {
        DisplayIdentity {
            email: email.to_owned(),
            subject: subject.map(str::to_owned),
            name: None,
            plan_label: None,
            workspace_id: None,
            workspace_name: None,
        }
    }

    #[test]
    fn matches_falls_back_to_email_when_either_subject_is_missing() {
        assert!(
            identity("person@example.com", Some("sub-1"))
                .matches(&identity("PERSON@example.com", None))
        );
        assert!(
            identity("person@example.com", None)
                .matches(&identity("PERSON@example.com", Some("sub-1")))
        );
        assert!(
            identity("person@example.com", None).matches(&identity("PERSON@example.com", None))
        );
        assert!(
            !identity("person@example.com", Some("sub-1"))
                .matches(&identity("other@example.com", None))
        );
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedAccountMetadata {
    pub id: Uuid,
    pub environment: EnvironmentKind,
    pub email: String,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub plan_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    pub secret_key: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_activated_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub cached_usage: Option<AccountUsageView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_usage_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetadataIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub write_generation: u64,
    pub accounts: Vec<SavedAccountMetadata>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountView {
    pub id: Uuid,
    pub email: String,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub plan_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    pub environment: EnvironmentKind,
    pub is_active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_activated_at: Option<OffsetDateTime>,
    pub usage: Option<AccountUsageView>,
    pub usage_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl AccountView {
    pub fn workspace_label(&self) -> Option<&str> {
        self.workspace_name
            .as_deref()
            .or_else(|| {
                self.workspace_id
                    .as_deref()
                    .filter(|id| Uuid::parse_str(id).is_err())
            })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusOutput {
    pub environment: EnvironmentKind,
    pub codex_root: String,
    pub current_account: Option<DisplayIdentity>,
    pub current_account_saved_id: Option<Uuid>,
    pub saved_accounts: usize,
    pub process_warnings: Vec<RunningCodexProcess>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ListOutput {
    pub environment: EnvironmentKind,
    pub accounts: Vec<AccountView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageOutput {
    pub environment: EnvironmentKind,
    pub account: DisplayIdentity,
    pub usage: AccountUsageView,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutoStartUsageWindowsStatusOutput {
    pub enabled: bool,
    pub poll_seconds: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutoSwitchOnLimitStatusOutput {
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchAtStartupStatusOutput {
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShowQuotaInMenuBarStatusOutput {
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutoStartUsageWindowsRunOutput {
    pub enabled: bool,
    pub checked_accounts: usize,
    pub pinged_accounts: Vec<AutoStartUsageWindowAccountResult>,
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutoStartUsageWindowAccountResult {
    pub account_id: Uuid,
    pub email: String,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchRefreshOutput {
    pub total: usize,
    pub refreshed: Vec<Uuid>,
    pub failed: Vec<BatchRefreshFailure>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchRefreshFailure {
    pub account_id: Uuid,
    pub email: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PickBestOutput {
    pub switched: bool,
    pub account: AccountView,
    pub scores: Vec<PickBestScoreView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PickBestScoreView {
    pub account_id: Uuid,
    pub email: String,
    pub label: Option<String>,
    pub score: Option<f64>,
    pub eligible: bool,
    pub weekly_used_percent: Option<u8>,
    pub five_hour_used_percent: Option<u8>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportBundle {
    pub schema_version: u32,
    pub environment: EnvironmentKind,
    pub exported_at: OffsetDateTime,
    pub accounts: Vec<ExportBundleAccount>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportBundleAccount {
    pub id: Uuid,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub plan_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    pub snapshot: SnapshotBlob,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportOutput {
    pub account_id: Uuid,
    pub email: String,
    pub label: Option<String>,
    pub created: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RenameOutput {
    pub account: AccountView,
}

#[derive(Clone, Debug, Serialize)]
pub struct SaveOutput {
    pub account: AccountView,
    pub action: SaveAction,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveAction {
    Created,
    Refreshed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActivateOutput {
    pub account: AccountView,
    pub warnings: Vec<RunningCodexProcess>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunningCodexProcess {
    pub pid: u32,
    pub executable: String,
    pub role: String,
    pub summary: Option<String>,
}

/// User choice when switching accounts while Codex processes are running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchWhenRunning {
    Cancel,
    WaitAndSwitch,
    SwitchNow,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeleteOutput {
    pub deleted_account_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountUsageView {
    pub source: UsageSource,
    pub fetched_at: OffsetDateTime,
    pub five_hour: Option<UsageWindowView>,
    pub weekly: Option<UsageWindowView>,
    pub credits: Option<CreditsView>,
}

impl AccountUsageView {
    pub fn is_out_of_quota(&self, now: OffsetDateTime) -> bool {
        if let Some(five_hour) = &self.five_hour
            && five_hour.remaining_percent == 0
            && five_hour.reset_at > now
        {
            return true;
        }
        if let Some(weekly) = &self.weekly
            && weekly.remaining_percent == 0
            && weekly.reset_at > now
        {
            return true;
        }
        if self.five_hour.is_none()
            && self.weekly.is_none()
            && let Some(credits) = &self.credits
            && !credits.unlimited
            && !credits.has_credits
        {
            return true;
        }
        false
    }

    /// Lowest remaining quota across active windows, or `None` when no window is active.
    pub fn min_remaining_percent(&self, now: OffsetDateTime) -> Option<u8> {
        [self.five_hour.as_ref(), self.weekly.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| window.reset_at > now)
            .map(|window| window.remaining_percent)
            .min()
    }

    /// True when any active window is nearly exhausted but not yet at 0%.
    pub fn is_near_limit(&self, now: OffsetDateTime, threshold_percent: u8) -> bool {
        if threshold_percent == 0 {
            return false;
        }
        [self.five_hour.as_ref(), self.weekly.as_ref()]
            .into_iter()
            .flatten()
            .any(|window| {
                window.reset_at > now
                    && window.remaining_percent > 0
                    && window.remaining_percent <= threshold_percent
            })
    }

    /// True when every active quota window is depleted. An account with an exhausted
    /// 5-hour window can still be switchable when weekly quota remains.
    pub fn is_fully_exhausted(&self, now: OffsetDateTime) -> bool {
        let active_windows: Vec<_> = [self.five_hour.as_ref(), self.weekly.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| window.reset_at > now)
            .collect();
        if !active_windows.is_empty() {
            return active_windows
                .iter()
                .all(|window| window.remaining_percent == 0);
        }
        self.is_out_of_quota(now)
    }

    /// Whether auto-switch should leave this account (exhausted or proactively near limit).
    pub fn should_switch_account(&self, now: OffsetDateTime, near_limit_threshold: u8) -> bool {
        self.is_out_of_quota(now) || self.is_near_limit(now, near_limit_threshold)
    }

    /// True when any window's reset time has passed and cached usage may be stale.
    pub fn has_stale_quota_cache(&self, now: OffsetDateTime) -> bool {
        [self.five_hour.as_ref(), self.weekly.as_ref()]
            .into_iter()
            .flatten()
            .any(|window| window.reset_at <= now)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageWindowView {
    pub used_percent: u8,
    pub remaining_percent: u8,
    pub reset_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreditsView {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    LiveAccessToken,
    LiveRefreshToken,
    SavedAccessToken,
    SavedRefreshToken,
}

#[cfg(test)]
mod usage_tests {
    use super::{AccountUsageView, CreditsView, UsageSource, UsageWindowView};
    use time::OffsetDateTime;

    fn usage_view(
        five_hour: Option<UsageWindowView>,
        weekly: Option<UsageWindowView>,
        credits: Option<CreditsView>,
    ) -> AccountUsageView {
        AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour,
            weekly,
            credits,
        }
    }

    fn window(remaining_percent: u8, reset_at: OffsetDateTime) -> UsageWindowView {
        UsageWindowView {
            used_percent: 100 - remaining_percent,
            remaining_percent,
            reset_at,
        }
    }

    #[test]
    fn is_out_of_quota_when_five_hour_window_is_exhausted() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let usage = usage_view(Some(window(0, reset)), None, None);
        assert!(usage.is_out_of_quota(now));
    }

    #[test]
    fn is_not_fully_exhausted_when_weekly_quota_remains() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let five_hour_reset = now + time::Duration::hours(1);
        let weekly_reset = now + time::Duration::days(7);
        let usage = usage_view(
            Some(window(0, five_hour_reset)),
            Some(window(84, weekly_reset)),
            None,
        );
        assert!(usage.is_out_of_quota(now));
        assert!(!usage.is_fully_exhausted(now));
    }

    #[test]
    fn is_fully_exhausted_when_all_active_windows_are_depleted() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let five_hour_reset = now + time::Duration::hours(1);
        let weekly_reset = now + time::Duration::days(7);
        let usage = usage_view(
            Some(window(0, five_hour_reset)),
            Some(window(0, weekly_reset)),
            None,
        );
        assert!(usage.is_fully_exhausted(now));
    }

    #[test]
    fn is_out_of_quota_when_weekly_window_is_exhausted() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::days(1);
        let usage = usage_view(None, Some(window(0, reset)), None);
        assert!(usage.is_out_of_quota(now));
    }

    #[test]
    fn is_not_out_of_quota_after_reset_time_passes() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2);
        let reset = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);
        let usage = usage_view(Some(window(0, reset)), None, None);
        assert!(!usage.is_out_of_quota(now));
    }

    #[test]
    fn is_out_of_quota_when_only_credits_remain_and_depleted() {
        let usage = usage_view(
            None,
            None,
            Some(CreditsView {
                has_credits: false,
                unlimited: false,
                balance: "0".to_owned(),
            }),
        );
        assert!(usage.is_out_of_quota(OffsetDateTime::UNIX_EPOCH));
    }

    #[test]
    fn unlimited_credits_do_not_count_as_out_of_quota() {
        let usage = usage_view(
            None,
            None,
            Some(CreditsView {
                has_credits: false,
                unlimited: true,
                balance: "0".to_owned(),
            }),
        );
        assert!(!usage.is_out_of_quota(OffsetDateTime::UNIX_EPOCH));
    }

    #[test]
    fn is_near_limit_when_remaining_is_at_threshold() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let usage = usage_view(Some(window(5, reset)), None, None);
        assert!(usage.is_near_limit(now, 5));
        assert!(!usage.is_near_limit(now, 4));
    }

    #[test]
    fn should_switch_account_when_near_limit_or_exhausted() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let near = usage_view(Some(window(3, reset)), None, None);
        let exhausted = usage_view(Some(window(0, reset)), None, None);
        let healthy = usage_view(Some(window(50, reset)), None, None);
        assert!(near.should_switch_account(now, 5));
        assert!(exhausted.should_switch_account(now, 5));
        assert!(!healthy.should_switch_account(now, 5));
    }

    #[test]
    fn min_remaining_percent_picks_lowest_active_window() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let reset = now + time::Duration::hours(1);
        let usage = usage_view(
            Some(window(20, reset)),
            Some(window(60, reset + time::Duration::days(1))),
            None,
        );
        assert_eq!(usage.min_remaining_percent(now), Some(20));
    }

    #[test]
    fn has_stale_quota_cache_when_reset_time_passed() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2);
        let reset = now - time::Duration::hours(1);
        let usage = usage_view(Some(window(0, reset)), None, None);
        assert!(usage.has_stale_quota_cache(now));
    }
}
