//! Claude Code OAuth usage (5-hour + 7-day windows).

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::model::{AccountUsageView, SnapshotBlob, UsageSource, UsageWindowView};

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeWindow>,
}

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    /// Percent used (0–100).
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

/// Pull Claude OAuth access token from a switcher snapshot.
pub fn access_token_from_snapshot(snapshot: &SnapshotBlob) -> Result<String> {
    for name in ["claude_credentials.json", "claude_keychain.txt"] {
        if let Some(file) = snapshot.files.iter().find(|f| f.name == name) {
            let bytes = STANDARD
                .decode(&file.bytes_base64)
                .with_context(|| format!("failed to decode {name}"))?;
            let text = String::from_utf8(bytes).with_context(|| format!("{name} is not UTF-8"))?;
            if let Some(token) = extract_access_token(&text) {
                return Ok(token);
            }
        }
    }
    bail!("Claude access token not found in snapshot")
}

fn extract_access_token(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    oauth
        .get("accessToken")
        .or_else(|| oauth.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

pub fn fetch_claude_usage(access_token: &str) -> Result<AccountUsageView> {
    let mut response = ureq::get(OAUTH_USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2.0.32")
        .header("Accept", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .call()
        .context("Claude usage request failed")?;

    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .context("failed to read Claude usage body")?;
    if !(200..300).contains(&status) {
        if status == 401 || status == 403 {
            bail!("usage authorization failed: Claude token rejected ({status})");
        }
        bail!("Claude usage endpoint returned HTTP {status}: {body}");
    }

    let parsed: ClaudeUsageResponse =
        serde_json::from_str(&body).context("failed to parse Claude usage JSON")?;

    Ok(AccountUsageView {
        source: UsageSource::SavedAccessToken,
        fetched_at: OffsetDateTime::now_utc(),
        five_hour: parsed.five_hour.as_ref().and_then(window_from_claude),
        weekly: parsed
            .seven_day
            .as_ref()
            .or(parsed.seven_day_opus.as_ref())
            .and_then(window_from_claude),
        credits: None,
    })
}

fn window_from_claude(window: &ClaudeWindow) -> Option<UsageWindowView> {
    let used = window.utilization.unwrap_or(0.0).clamp(0.0, 100.0).round() as u8;
    let reset_at = window
        .resets_at
        .as_deref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or_else(|| OffsetDateTime::now_utc() + time::Duration::hours(5));
    Some(UsageWindowView {
        used_percent: used,
        remaining_percent: 100u8.saturating_sub(used),
        reset_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_oauth_json() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","refreshToken":"r"}}"#;
        assert_eq!(
            extract_access_token(raw).as_deref(),
            Some("sk-ant-oat01-test")
        );
    }
}
