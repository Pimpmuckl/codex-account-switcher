use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const METADATA_SCHEMA_VERSION: u32 = 1;
pub const AUTH_FILES: [&str; 2] = ["auth.json", "cap_sid"];

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotBlob {
    pub schema_version: u32,
    pub files: Vec<SnapshotFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

impl DisplayIdentity {
    pub fn matches(&self, other: &Self) -> bool {
        match (&self.subject, &other.subject) {
            (Some(left), Some(right)) => left == right,
            _ => self.email.eq_ignore_ascii_case(&other.email),
        }
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
    pub secret_key: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_activated_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetadataIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub write_generation: u64,
    pub accounts: Vec<SavedAccountMetadata>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    pub id: Uuid,
    pub email: String,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub plan_label: Option<String>,
    pub environment: EnvironmentKind,
    pub is_active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_activated_at: Option<OffsetDateTime>,
    pub usage: Option<AccountUsageView>,
    pub usage_error: Option<String>,
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

#[derive(Clone, Debug, Serialize)]
pub struct DeleteOutput {
    pub deleted_account_id: Uuid,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsageView {
    pub source: UsageSource,
    pub fetched_at: OffsetDateTime,
    pub five_hour: Option<UsageWindowView>,
    pub weekly: Option<UsageWindowView>,
    pub credits: Option<CreditsView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageWindowView {
    pub used_percent: u8,
    pub remaining_percent: u8,
    pub reset_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreditsView {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    LiveAccessToken,
    LiveRefreshToken,
    SavedAccessToken,
    SavedRefreshToken,
}
