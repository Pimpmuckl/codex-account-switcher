//! Cursor plan usage (monthly Auto + API pools).

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::model::{AccountUsageView, SnapshotBlob, UsageSource, UsageWindowView};

const PERIOD_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";

#[derive(Debug, Deserialize)]
struct PeriodUsageResponse {
    #[serde(default, rename = "billingCycleEnd")]
    billing_cycle_end: Option<serde_json::Value>,
    #[serde(default, rename = "planUsage")]
    plan_usage: Option<PlanUsage>,
}

#[derive(Debug, Deserialize)]
struct PlanUsage {
    #[serde(default, rename = "autoPercentUsed")]
    auto_percent_used: Option<f64>,
    #[serde(default, rename = "apiPercentUsed")]
    api_percent_used: Option<f64>,
    #[serde(default, rename = "totalPercentUsed")]
    total_percent_used: Option<f64>,
}

/// Extract Cursor access token from a switcher snapshot (`cursor_auth.json`).
pub fn access_token_from_snapshot(snapshot: &SnapshotBlob) -> Result<String> {
    let file = snapshot
        .files
        .iter()
        .find(|f| f.name == "cursor_auth.json")
        .context("snapshot missing cursor_auth.json")?;
    let json_bytes = STANDARD
        .decode(&file.bytes_base64)
        .context("failed to decode cursor_auth.json")?;
    let map: std::collections::HashMap<String, String> =
        serde_json::from_slice(&json_bytes).context("failed to parse cursor_auth.json")?;
    let encoded = map
        .get("cursorAuth/accessToken")
        .context("cursorAuth/accessToken missing from snapshot")?;
    let bytes = STANDARD
        .decode(encoded)
        .context("failed to decode cursor access token")?;
    String::from_utf8(bytes).context("cursor access token is not UTF-8")
}

pub fn fetch_cursor_usage(access_token: &str) -> Result<AccountUsageView> {
    let mut response = ureq::post(PERIOD_USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "codex-account-switcher")
        .header("Accept", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .send(&b"{}"[..])
        .context("Cursor usage request failed")?;

    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .context("failed to read Cursor usage body")?;
    if !(200..300).contains(&status) {
        if status == 401 || status == 403 {
            bail!("usage authorization failed: Cursor token rejected ({status})");
        }
        bail!("Cursor usage endpoint returned HTTP {status}: {body}");
    }

    let parsed: PeriodUsageResponse =
        serde_json::from_str(&body).context("failed to parse Cursor usage JSON")?;
    let plan = parsed
        .plan_usage
        .context("Cursor usage response missing planUsage")?;
    let reset_at = parse_billing_end(parsed.billing_cycle_end.as_ref())
        .unwrap_or_else(|| OffsetDateTime::now_utc() + time::Duration::days(30));

    // Cursor has monthly pools (not Codex-style 5h/weekly).
    // Map Auto/Composer pool → five_hour (session-style meter)
    // Map total (or API) pool → weekly meter for the billing cycle.
    let auto_used = plan.auto_percent_used.unwrap_or(0.0);
    let total_used = plan
        .total_percent_used
        .or(plan.api_percent_used)
        .unwrap_or(auto_used);

    Ok(AccountUsageView {
        source: UsageSource::SavedAccessToken,
        fetched_at: OffsetDateTime::now_utc(),
        five_hour: Some(percent_window(auto_used, reset_at)),
        weekly: Some(percent_window(total_used, reset_at)),
        credits: None,
    })
}

fn percent_window(used_percent: f64, reset_at: OffsetDateTime) -> UsageWindowView {
    let used = used_percent.clamp(0.0, 100.0).round() as u8;
    UsageWindowView {
        used_percent: used,
        remaining_percent: 100u8.saturating_sub(used),
        reset_at,
    }
}

fn parse_billing_end(value: Option<&serde_json::Value>) -> Option<OffsetDateTime> {
    let value = value?;
    // API returns ms epoch as string: "1787112722000"
    if let Some(s) = value.as_str() {
        if let Ok(ms) = s.parse::<i64>() {
            return OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok();
        }
        if let Ok(dt) = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
            return Some(dt);
        }
    }
    if let Some(ms) = value.as_i64() {
        return OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok();
    }
    if let Some(ms) = value.as_f64() {
        return OffsetDateTime::from_unix_timestamp_nanos((ms as i128) * 1_000_000).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percent_window_clamps() {
        let w = percent_window(93.7, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(w.used_percent, 94);
        assert_eq!(w.remaining_percent, 6);
    }

    #[test]
    fn parse_billing_end_ms_string() {
        let v = json!("1787112722000");
        let dt = parse_billing_end(Some(&v)).expect("parse");
        assert!(dt.year() >= 2026);
    }
}
