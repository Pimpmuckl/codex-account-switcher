use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::identity::parse_identity_from_auth_json;
use crate::model::{
    AccountUsageView, CreditsView, DisplayIdentity, EnvironmentKind, SnapshotBlob, UsageOutput,
    UsageSource, UsageWindowView,
};

static CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
static USAGE_ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
static REFRESH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
static TOKEN_REFRESH_INTERVAL: LazyLock<Duration> = LazyLock::new(|| Duration::days(8));

thread_local! {
    static TEST_USAGE_ENDPOINT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static TEST_REFRESH_ENDPOINT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn usage_endpoint() -> String {
    TEST_USAGE_ENDPOINT
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| USAGE_ENDPOINT.to_owned())
}

fn refresh_endpoint() -> String {
    TEST_REFRESH_ENDPOINT
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| REFRESH_ENDPOINT.to_owned())
}

#[cfg(test)]
pub(crate) fn with_test_http_endpoints<R>(
    usage_endpoint: &str,
    refresh_endpoint: &str,
    run: impl FnOnce() -> R,
) -> R {
    TEST_USAGE_ENDPOINT.with(|slot| *slot.borrow_mut() = Some(usage_endpoint.to_owned()));
    TEST_REFRESH_ENDPOINT.with(|slot| *slot.borrow_mut() = Some(refresh_endpoint.to_owned()));
    let result = run();
    TEST_USAGE_ENDPOINT.with(|slot| *slot.borrow_mut() = None);
    TEST_REFRESH_ENDPOINT.with(|slot| *slot.borrow_mut() = None);
    result
}

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
                || error.contains("refresh_token")
                || error.contains("log out")
                || error.contains("log in")
                || error.contains("session has ended")
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
    let auth_file = snapshot
        .files
        .iter()
        .find(|file| file.name == "auth.json")
        .context("snapshot missing auth.json")?;
    let auth_json = STANDARD
        .decode(&auth_file.bytes_base64)
        .context("failed to decode snapshot auth.json")?;
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

#[cfg(test)]
fn parse_usage_response_json(value: &str) -> Result<UsageResponse> {
    serde_json::from_str(value).context("failed to parse usage response json")
}

fn fetch_usage_response(access_token: &str, account_id: Option<&str>) -> Result<UsageResponse> {
    let mut request = ureq::get(&usage_endpoint())
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
    let mut response = ureq::post(&refresh_endpoint())
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
    let auth_file = snapshot
        .files
        .iter()
        .find(|file| file.name == "auth.json")
        .context("snapshot missing auth.json")?;
    let auth_json = STANDARD
        .decode(&auth_file.bytes_base64)
        .context("failed to decode snapshot auth.json")?;
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

    use crate::usage_http::MockHttpServer;
    use crate::{codex::auth_json_fixture, model::EnvironmentKind};

    use super::*;

    static HTTP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_http_test_lock<R>(run: impl FnOnce() -> R) -> R {
        let _guard = HTTP_TEST_LOCK.lock().expect("http integration test lock");
        run()
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
    fn usage_response_parses_windows_and_identity() {
        let response = parse_usage_response_json(
            r#"{
                "email": "user@example.com",
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": { "used_percent": 80, "reset_at": 1700000000 },
                    "secondary_window": { "used_percent": 35, "reset_at": 1700100000 }
                },
                "credits": { "has_credits": true, "unlimited": false, "balance": "12.50" }
            }"#,
        )
        .expect("parse usage response");

        let identity = response
            .identity()
            .expect("identity")
            .expect("usage identity");
        assert_eq!(identity.email, "user@example.com");
        assert_eq!(identity.plan_label.as_deref(), Some("Plus"));
        let view = response
            .into_view(UsageSource::SavedAccessToken)
            .expect("usage view");
        assert_eq!(view.five_hour.as_ref().unwrap().remaining_percent, 20);
        assert_eq!(view.weekly.as_ref().unwrap().remaining_percent, 65);
        assert_eq!(view.credits.as_ref().unwrap().balance, "12.50");
    }

    #[test]
    fn usage_response_rejects_invalid_reset_timestamp() {
        let response = parse_usage_response_json(
            r#"{
                "email": "user@example.com",
                "rate_limit": {
                    "primary_window": { "used_percent": 10, "reset_at": 9999999999999 }
                }
            }"#,
        )
        .expect("parse usage response");

        let error = response
            .into_view(UsageSource::LiveAccessToken)
            .expect_err("invalid reset timestamp");
        assert!(format!("{error:#}").contains("invalid reset timestamp"));
    }

    #[test]
    fn usage_error_requires_login_detects_authorization_failures() {
        assert!(usage_error_requires_login(
            "usage authorization failed for saved account"
        ));
        assert!(usage_error_requires_login("snapshot refresh token missing"));
        assert!(!usage_error_requires_login("failed to query Codex usage"));
    }

    #[test]
    fn merge_identity_keeps_subject_from_saved_snapshot() {
        let base = DisplayIdentity {
            email: "saved@example.com".to_owned(),
            subject: Some("sub-123".to_owned()),
            name: Some("Saved User".to_owned()),
            plan_label: Some("Pro".to_owned()),
        };
        let fetched = DisplayIdentity {
            email: "live@example.com".to_owned(),
            subject: None,
            name: None,
            plan_label: Some("Plus".to_owned()),
        };
        let merged = merge_identity(&base, Some(fetched));
        assert_eq!(merged.email, "live@example.com");
        assert_eq!(merged.subject.as_deref(), Some("sub-123"));
        assert_eq!(merged.plan_label.as_deref(), Some("Plus"));
    }

    fn usage_snapshot_fixture() -> SnapshotBlob {
        SnapshotBlob {
            schema_version: crate::model::SNAPSHOT_SCHEMA_VERSION,
            files: vec![
                crate::model::SnapshotFile {
                    name: "auth.json".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(
                        auth_json_fixture("user@example.com", "sub-user", Some("plus")),
                    ),
                },
                crate::model::SnapshotFile {
                    name: "cap_sid".to_owned(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode("sid-user"),
                },
            ],
        }
    }

    #[test]
    fn fetch_usage_refreshes_after_authorization_failure() {
        with_http_test_lock(|| {
            let server = MockHttpServer::bind();
            server.enqueue(401, r#"{"error":"expired"}"#);
            server.enqueue(
                200,
                r#"{
                "access_token": "access-new",
                "refresh_token": "refresh-new",
                "id_token": "id-new"
            }"#,
            );
            server.enqueue(
                200,
                r#"{
                "email": "user@example.com",
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": { "used_percent": 25, "reset_at": 1700000000 },
                    "secondary_window": { "used_percent": 10, "reset_at": 1700100000 }
                }
            }"#,
            );

            let snapshot = usage_snapshot_fixture();
            let target = UsageTarget {
                environment: EnvironmentKind::Macos,
                identity: DisplayIdentity {
                    email: "user@example.com".to_owned(),
                    subject: Some("sub-user".to_owned()),
                    name: None,
                    plan_label: Some("Plus".to_owned()),
                },
                snapshot,
                source: UsageSource::SavedAccessToken,
                allow_refresh: true,
            };

            let (output, updated_snapshot) =
                with_test_http_endpoints(&server.usage_url(), &server.refresh_url(), || {
                    fetch_usage(target).expect("fetch usage with refresh")
                });

            assert_eq!(output.account.email, "user@example.com");
            assert_eq!(output.usage.source, UsageSource::SavedRefreshToken);
            assert_eq!(output.usage.weekly.as_ref().unwrap().remaining_percent, 90);
            let updated_auth = snapshot_auth(&updated_snapshot).expect("updated auth");
            assert_eq!(updated_auth.access_token, "access-new");
            assert_eq!(updated_auth.refresh_token.as_deref(), Some("refresh-new"));
        });
    }

    #[test]
    fn fetch_usage_does_not_refresh_when_disabled() {
        with_http_test_lock(|| {
            let server = MockHttpServer::bind();
            server.enqueue(401, r#"{"error":"expired"}"#);

            let target = UsageTarget {
                environment: EnvironmentKind::Linux,
                identity: DisplayIdentity {
                    email: "user@example.com".to_owned(),
                    subject: Some("sub-user".to_owned()),
                    name: None,
                    plan_label: None,
                },
                snapshot: usage_snapshot_fixture(),
                source: UsageSource::SavedAccessToken,
                allow_refresh: false,
            };

            let error =
                with_test_http_endpoints(&server.usage_url(), &server.refresh_url(), || {
                    fetch_usage(target).expect_err("usage should fail without refresh")
                });
            assert!(format!("{error:#}").contains("usage authorization failed"));
        });
    }

    #[test]
    fn fetch_usage_surfaces_refresh_invalid_grant_as_login_required() {
        with_http_test_lock(|| {
            let server = MockHttpServer::bind();
            server.enqueue(401, r#"{"error":"expired"}"#);
            server.enqueue(
            400,
            r#"{"error":"invalid_grant","error_description":"Please log out and sign in again."}"#,
        );

            let target = UsageTarget {
                environment: EnvironmentKind::Windows,
                identity: DisplayIdentity {
                    email: "user@example.com".to_owned(),
                    subject: Some("sub-user".to_owned()),
                    name: None,
                    plan_label: None,
                },
                snapshot: usage_snapshot_fixture(),
                source: UsageSource::SavedAccessToken,
                allow_refresh: true,
            };

            let error =
                with_test_http_endpoints(&server.usage_url(), &server.refresh_url(), || {
                    fetch_usage(target).expect_err("refresh should fail")
                });
            let message = usage_error_message(&error);
            assert_eq!(usage_error_label(&message), "Login required");
        });
    }
}
