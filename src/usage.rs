use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::codex::validate_managed_snapshot;
use crate::identity::parse_identity_from_auth_json;
use crate::model::{
    AccountUsageView, CreditsView, DisplayIdentity, EnvironmentKind, SnapshotBlob, UsageOutput,
    UsageSource, UsageWindowView,
};

static CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
static USAGE_ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
static REFRESH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
static TOKEN_REFRESH_INTERVAL: LazyLock<Duration> = LazyLock::new(|| Duration::days(8));

#[derive(Clone, Debug)]
pub struct UsageTarget {
    pub environment: EnvironmentKind,
    pub identity: DisplayIdentity,
    pub snapshot: SnapshotBlob,
    pub source: UsageSource,
    pub allow_refresh: bool,
}

pub fn fetch_usage(target: UsageTarget) -> Result<(UsageOutput, SnapshotBlob)> {
    let mut auth = snapshot_auth(&target.snapshot)?;
    let mut source = target.source;

    let response = match fetch_usage_response(&auth.access_token, auth.account_id.as_deref()) {
        Ok(response) => response,
        Err(error) if target.allow_refresh && should_refresh_after_error(&error, &auth) => {
            auth = refresh_auth(&auth)?;
            source = refresh_source(source);
            fetch_usage_response(&auth.access_token, auth.account_id.as_deref())?
        }
        Err(error) => return Err(error),
    };

    let snapshot = if auth.changed {
        update_snapshot_auth(&target.snapshot, &auth)?
    } else {
        target.snapshot
    };

    let fetched_identity = merge_identity(&target.identity, response.identity()?);
    Ok((
        UsageOutput {
            environment: target.environment,
            account: fetched_identity,
            usage: response.into_view(source)?,
        },
        snapshot,
    ))
}

pub fn usage_error_message(error: &anyhow::Error) -> String {
    let rendered = format!("{error:#}");
    if usage_error_requires_login(&rendered) {
        "Login required: Codex auth expired or was logged out. Log in with this account again, then refresh/save it.".to_owned()
    } else {
        let detail = rendered.lines().next().unwrap_or("unknown error");
        format!("Usage unavailable: {detail}")
    }
}

pub fn usage_error_label(error: &str) -> &'static str {
    if usage_error_requires_login(error) {
        "Login required"
    } else {
        "Usage unavailable"
    }
}

pub fn usage_error_requires_login(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("login required")
        || error.contains("usage authorization failed")
        || error.contains("snapshot refresh token missing")
        || (error.contains("token refresh failed")
            && (error.contains("invalid_grant")
                || error.contains("refresh token")
                || error.contains("log out")
                || error.contains("sign in")))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredAuth {
    tokens: StoredTokens,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredTokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Clone, Debug)]
struct SnapshotAuth {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    account_id: Option<String>,
    last_refresh: Option<OffsetDateTime>,
    changed: bool,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    email: Option<String>,
    plan_type: Option<String>,
    rate_limit: Option<UsageRateLimit>,
    credits: Option<UsageCredits>,
}

#[derive(Debug, Deserialize)]
struct UsageRateLimit {
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    used_percent: u8,
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct UsageCredits {
    has_credits: bool,
    unlimited: bool,
    balance: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

impl UsageResponse {
    fn identity(&self) -> Result<Option<DisplayIdentity>> {
        let Some(email) = &self.email else {
            return Ok(None);
        };
        Ok(Some(DisplayIdentity {
            email: email.clone(),
            subject: None,
            name: None,
            plan_label: normalize_plan_label(self.plan_type.as_deref()),
        }))
    }

    fn into_view(self, source: UsageSource) -> Result<AccountUsageView> {
        let now = OffsetDateTime::now_utc();
        Ok(AccountUsageView {
            source,
            fetched_at: now,
            five_hour: self
                .rate_limit
                .as_ref()
                .and_then(|limits| limits.primary_window.as_ref())
                .map(window_view)
                .transpose()?,
            weekly: self
                .rate_limit
                .as_ref()
                .and_then(|limits| limits.secondary_window.as_ref())
                .map(window_view)
                .transpose()?,
            credits: self.credits.map(credits_view),
        })
    }
}

fn snapshot_auth(snapshot: &SnapshotBlob) -> Result<SnapshotAuth> {
    let auth_json = snapshot_auth_json(snapshot)?;
    let stored: StoredAuth =
        serde_json::from_slice(&auth_json).context("failed to parse snapshot auth.json")?;
    Ok(SnapshotAuth {
        access_token: stored.tokens.access_token,
        refresh_token: stored.tokens.refresh_token,
        id_token: stored.tokens.id_token,
        account_id: stored.tokens.account_id,
        last_refresh: parse_last_refresh(stored.last_refresh.as_deref())?,
        changed: false,
    })
}

fn snapshot_auth_json(snapshot: &SnapshotBlob) -> Result<Vec<u8>> {
    validate_managed_snapshot(snapshot)?;
    let auth_file = snapshot
        .files
        .iter()
        .find(|file| file.name == "auth.json")
        .context("snapshot missing auth.json")?;
    STANDARD
        .decode(&auth_file.bytes_base64)
        .context("failed to decode snapshot auth.json")
}

fn update_snapshot_auth(snapshot: &SnapshotBlob, auth: &SnapshotAuth) -> Result<SnapshotBlob> {
    let mut updated = snapshot.clone();
    let auth_index = updated
        .files
        .iter()
        .position(|file| file.name == "auth.json")
        .context("snapshot missing auth.json")?;
    let auth_json = STANDARD
        .decode(&updated.files[auth_index].bytes_base64)
        .context("failed to decode snapshot auth.json")?;
    let mut stored: StoredAuth =
        serde_json::from_slice(&auth_json).context("failed to parse snapshot auth.json")?;
    stored.tokens.access_token = auth.access_token.clone();
    stored.tokens.refresh_token = auth.refresh_token.clone();
    stored.tokens.id_token = auth.id_token.clone();
    stored.tokens.account_id = auth.account_id.clone();
    stored.last_refresh = auth.last_refresh.map(format_last_refresh);
    updated.files[auth_index].bytes_base64 = STANDARD.encode(
        serde_json::to_vec_pretty(&stored).context("failed to encode refreshed auth.json")?,
    );
    Ok(updated)
}

fn fetch_usage_response(access_token: &str, account_id: Option<&str>) -> Result<UsageResponse> {
    let mut request = ureq::get(USAGE_ENDPOINT)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("User-Agent", "codex-account-switcher")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build();
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let mut response = request.call().context("failed to query Codex usage")?;
    let status = response.status();
    if status == 401 || status == 403 {
        bail!("usage authorization failed");
    }
    if status.as_u16() >= 400 {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        bail!("usage request failed with {status}: {body}");
    }
    response
        .body_mut()
        .read_json::<UsageResponse>()
        .context("failed to decode Codex usage response")
}

fn should_refresh_after_error(error: &anyhow::Error, auth: &SnapshotAuth) -> bool {
    if auth.refresh_token.as_deref().is_none_or(str::is_empty) {
        return false;
    }
    if auth.last_refresh.is_some_and(|last_refresh| {
        OffsetDateTime::now_utc() - last_refresh < *TOKEN_REFRESH_INTERVAL
    }) {
        return format!("{error:#}").contains("authorization failed");
    }
    true
}

fn refresh_auth(auth: &SnapshotAuth) -> Result<SnapshotAuth> {
    let refresh_token = auth
        .refresh_token
        .as_deref()
        .filter(|value| !value.is_empty())
        .context("snapshot refresh token missing")?;
    let payload = serde_json::json!({
        "client_id": CHATGPT_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email"
    });
    let payload_json =
        serde_json::to_string(&payload).context("failed to encode refresh payload")?;
    let mut response = ureq::post(REFRESH_ENDPOINT)
        .header("Content-Type", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build()
        .send(&payload_json)
        .context("failed to refresh Codex auth tokens")?;
    let status = response.status();
    if status.as_u16() >= 400 {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        bail!("token refresh failed with {status}: {body}");
    }
    let refreshed = response
        .body_mut()
        .read_json::<RefreshResponse>()
        .context("failed to decode refreshed Codex tokens")?;
    Ok(SnapshotAuth {
        access_token: refreshed.access_token,
        refresh_token: refreshed
            .refresh_token
            .or_else(|| auth.refresh_token.clone()),
        id_token: refreshed.id_token.or_else(|| auth.id_token.clone()),
        account_id: auth.account_id.clone(),
        last_refresh: Some(OffsetDateTime::now_utc()),
        changed: true,
    })
}

fn refresh_source(source: UsageSource) -> UsageSource {
    match source {
        UsageSource::LiveAccessToken | UsageSource::LiveRefreshToken => {
            UsageSource::LiveRefreshToken
        }
        UsageSource::SavedAccessToken | UsageSource::SavedRefreshToken => {
            UsageSource::SavedRefreshToken
        }
    }
}

fn window_view(window: &UsageWindow) -> Result<UsageWindowView> {
    let reset_at = OffsetDateTime::from_unix_timestamp(window.reset_at)
        .map_err(|error| anyhow!("invalid reset timestamp {}: {error}", window.reset_at))?;
    Ok(UsageWindowView {
        used_percent: window.used_percent,
        remaining_percent: 100u8.saturating_sub(window.used_percent),
        reset_at,
    })
}

fn credits_view(credits: UsageCredits) -> CreditsView {
    CreditsView {
        has_credits: credits.has_credits,
        unlimited: credits.unlimited,
        balance: match credits.balance {
            serde_json::Value::String(value) => value,
            other => other.to_string(),
        },
    }
}

fn normalize_plan_label(raw: Option<&str>) -> Option<String> {
    match raw?.trim() {
        "" => None,
        "go" => Some("Go".to_owned()),
        "plus" => Some("Plus".to_owned()),
        "pro" => Some("Pro".to_owned()),
        "free" => Some("Free".to_owned()),
        other => Some(other.to_owned()),
    }
}

fn merge_identity(base: &DisplayIdentity, fetched: Option<DisplayIdentity>) -> DisplayIdentity {
    let Some(fetched) = fetched else {
        return base.clone();
    };
    DisplayIdentity {
        email: fetched.email,
        subject: base.subject.clone(),
        name: base.name.clone(),
        plan_label: fetched.plan_label.or_else(|| base.plan_label.clone()),
    }
}

fn parse_last_refresh(value: Option<&str>) -> Result<Option<OffsetDateTime>> {
    value
        .map(|value| {
            OffsetDateTime::parse(value, &Rfc3339)
                .map_err(|error| anyhow!("failed to parse last_refresh {value:?}: {error}"))
        })
        .transpose()
}

fn format_last_refresh(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

pub fn usage_target_from_snapshot(
    environment: EnvironmentKind,
    snapshot: SnapshotBlob,
    source: UsageSource,
    allow_refresh: bool,
) -> Result<UsageTarget> {
    let auth_json = snapshot_auth_json(&snapshot)?;
    let identity = parse_identity_from_auth_json(&auth_json)?;
    Ok(UsageTarget {
        environment,
        identity,
        snapshot,
        source,
        allow_refresh,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use base64::Engine;

    use super::*;
    use crate::model::{SNAPSHOT_SCHEMA_VERSION, SnapshotFile};

    fn snapshot_file(name: &str, content: impl AsRef<[u8]>) -> SnapshotFile {
        SnapshotFile {
            name: name.to_owned(),
            bytes_base64: STANDARD.encode(content),
        }
    }

    #[test]
    fn reused_refresh_token_error_is_login_required() {
        let error = anyhow!(
            "token refresh failed with 400 Bad Request: {{\"error\":\"invalid_grant\",\"error_description\":\"Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.\"}}"
        );
        let message = usage_error_message(&error);

        assert_eq!(usage_error_label(&message), "Login required");
        assert!(message.contains("Log in with this account again"));
    }

    #[test]
    fn non_auth_usage_error_stays_usage_unavailable() {
        let message = usage_error_message(&anyhow!("failed to query Codex usage"));

        assert_eq!(usage_error_label(&message), "Usage unavailable");
        assert_eq!(message, "Usage unavailable: failed to query Codex usage");
    }

    #[test]
    fn snapshot_auth_rejects_duplicate_auth_json_before_selecting_token() {
        let stored_auth = serde_json::json!({
            "tokens": {
                "access_token": "first-token",
                "refresh_token": null,
                "id_token": null,
                "account_id": null
            }
        })
        .to_string();
        let snapshot = SnapshotBlob {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            files: vec![
                snapshot_file("auth.json", stored_auth),
                snapshot_file("auth.json", "not-json"),
                snapshot_file("cap_sid", "sid"),
            ],
        };

        let error = snapshot_auth(&snapshot).expect_err("duplicate auth.json should fail");

        assert!(
            format!("{error:#}")
                .contains("snapshot contains duplicate managed auth file auth.json")
        );
    }
}
