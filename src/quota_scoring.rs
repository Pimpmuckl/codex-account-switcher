use time::OffsetDateTime;
use uuid::Uuid;

use crate::model::{AccountUsageView, UsageWindowView};
use crate::usage::usage_error_requires_login;

const WEEKLY_WINDOW_SECONDS: f64 = 168.0 * 3600.0;

#[derive(Clone, Debug, PartialEq)]
pub struct AccountQuotaScore {
    pub account_id: Uuid,
    pub email: String,
    pub label: Option<String>,
    pub score: f64,
    pub weekly_used_percent: Option<u8>,
    pub five_hour_used_percent: Option<u8>,
    pub eligible: bool,
    pub detail: Option<String>,
}

pub fn score_usage(usage: &AccountUsageView, now: OffsetDateTime) -> f64 {
    let weekly_used = usage
        .weekly
        .as_ref()
        .map(|window| effective_used_percent(window, now))
        .unwrap_or(0);
    let five_hour_used = usage
        .five_hour
        .as_ref()
        .map(|window| effective_used_percent(window, now));

    if weekly_used >= 100 {
        return 500.0 + f64::from(weekly_used);
    }

    let weekly_reset = usage
        .weekly
        .as_ref()
        .map(|window| window.reset_at)
        .unwrap_or(now);
    let weekly_remaining_secs = (weekly_reset - now).whole_seconds().max(0) as f64;
    let weekly_elapsed_secs = (WEEKLY_WINDOW_SECONDS - weekly_remaining_secs).max(0.0);
    let weekly_budget = (weekly_elapsed_secs / WEEKLY_WINDOW_SECONDS) * 100.0;
    let weekly_score = f64::from(weekly_used) - weekly_budget;

    let five_hour_penalty = match five_hour_used {
        Some(used) if used >= 100 => 200.0,
        Some(used) if used >= 90 => 50.0,
        Some(used) if used >= 75 => 10.0,
        _ => 0.0,
    };

    weekly_score + five_hour_penalty
}

pub fn score_saved_account(
    account_id: Uuid,
    email: &str,
    label: Option<&str>,
    cached_usage: Option<&AccountUsageView>,
    cached_usage_error: Option<&str>,
    now: OffsetDateTime,
) -> AccountQuotaScore {
    if cached_usage_error.is_some_and(usage_error_requires_login) {
        return AccountQuotaScore {
            account_id,
            email: email.to_owned(),
            label: label.map(str::to_owned),
            score: f64::INFINITY,
            weekly_used_percent: None,
            five_hour_used_percent: None,
            eligible: false,
            detail: Some("login required".to_owned()),
        };
    }

    let Some(usage) = cached_usage else {
        return AccountQuotaScore {
            account_id,
            email: email.to_owned(),
            label: label.map(str::to_owned),
            score: 25.0,
            weekly_used_percent: None,
            five_hour_used_percent: None,
            eligible: true,
            detail: Some("no cached usage".to_owned()),
        };
    };

    if usage.is_out_of_quota(now) {
        return AccountQuotaScore {
            account_id,
            email: email.to_owned(),
            label: label.map(str::to_owned),
            score: f64::INFINITY,
            weekly_used_percent: usage
                .weekly
                .as_ref()
                .map(|w| effective_used_percent(w, now)),
            five_hour_used_percent: usage
                .five_hour
                .as_ref()
                .map(|w| effective_used_percent(w, now)),
            eligible: false,
            detail: Some("out of quota".to_owned()),
        };
    }

    AccountQuotaScore {
        account_id,
        email: email.to_owned(),
        label: label.map(str::to_owned),
        score: score_usage(usage, now),
        weekly_used_percent: usage
            .weekly
            .as_ref()
            .map(|w| effective_used_percent(w, now)),
        five_hour_used_percent: usage
            .five_hour
            .as_ref()
            .map(|w| effective_used_percent(w, now)),
        eligible: true,
        detail: None,
    }
}

pub fn pick_best_account_id(scores: &[AccountQuotaScore]) -> Option<Uuid> {
    scores
        .iter()
        .filter(|entry| entry.eligible)
        .min_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|entry| entry.account_id)
}

fn effective_used_percent(window: &UsageWindowView, now: OffsetDateTime) -> u8 {
    if window.reset_at <= now {
        0
    } else {
        window.used_percent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountUsageView, UsageSource, UsageWindowView};

    fn window(used_percent: u8, reset_at: OffsetDateTime) -> UsageWindowView {
        UsageWindowView {
            used_percent,
            remaining_percent: 100 - used_percent,
            reset_at,
        }
    }

    fn usage(
        weekly: Option<UsageWindowView>,
        five_hour: Option<UsageWindowView>,
    ) -> AccountUsageView {
        AccountUsageView {
            source: UsageSource::SavedAccessToken,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
            five_hour,
            weekly,
            credits: None,
        }
    }

    #[test]
    fn lower_score_wins_when_under_weekly_budget() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(84);
        let reset = now + time::Duration::hours(84);
        let ahead = score_usage(&usage(Some(window(80, reset)), None), now);
        let behind = score_usage(&usage(Some(window(40, reset)), None), now);
        assert!(behind < ahead);
    }

    #[test]
    fn five_hour_penalty_prefers_clear_short_window() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(24);
        let weekly_reset = now + time::Duration::hours(120);
        let five_hour_reset = now + time::Duration::hours(2);
        let warm = score_usage(
            &usage(
                Some(window(30, weekly_reset)),
                Some(window(80, five_hour_reset)),
            ),
            now,
        );
        let clear = score_usage(
            &usage(
                Some(window(30, weekly_reset)),
                Some(window(20, five_hour_reset)),
            ),
            now,
        );
        assert!(clear < warm);
    }

    #[test]
    fn pick_best_skips_ineligible_accounts() {
        let eligible = AccountQuotaScore {
            account_id: Uuid::new_v4(),
            email: "good@example.com".to_owned(),
            label: None,
            score: 5.0,
            weekly_used_percent: Some(20),
            five_hour_used_percent: Some(10),
            eligible: true,
            detail: None,
        };
        let blocked = AccountQuotaScore {
            account_id: Uuid::new_v4(),
            email: "bad@example.com".to_owned(),
            label: None,
            score: 0.0,
            weekly_used_percent: Some(100),
            five_hour_used_percent: Some(100),
            eligible: false,
            detail: Some("out of quota".to_owned()),
        };
        assert_eq!(
            pick_best_account_id(&[blocked, eligible.clone()]),
            Some(eligible.account_id)
        );
    }
}
