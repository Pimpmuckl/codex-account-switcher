//! Even-consumption pace tracking inspired by CodexBar's `UsagePace`.
//!
//! Compares actual used% against the expected burn rate for the remaining window.
//! Hidden when less than 3% of the window has elapsed (same gate as CodexBar).

use time::OffsetDateTime;

pub const FIVE_HOUR_WINDOW_SECS: f64 = 5.0 * 3600.0;
pub const WEEKLY_WINDOW_SECS: f64 = 168.0 * 3600.0;
const MIN_ELAPSED_FRACTION: f64 = 0.03;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaceStage {
    OnTrack,
    SlightlyAhead,
    Ahead,
    FarAhead,
    SlightlyBehind,
    Behind,
    FarBehind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsagePace {
    pub stage: PaceStage,
    /// Actual used% minus expected used%. Positive = burning faster (deficit).
    pub delta_percent: f64,
    pub expected_used_percent: f64,
    pub actual_used_percent: f64,
    pub eta_secs: Option<f64>,
    pub will_last_to_reset: bool,
}

impl UsagePace {
    /// Compact CodexBar-style left label: "On pace" / "N% in deficit" / "N% in reserve".
    pub fn summary_label(&self) -> String {
        self.summary_label_localized(false)
    }

    pub fn summary_label_localized(&self, vietnamese: bool) -> String {
        let delta = self.delta_percent.abs().round() as i64;
        if delta == 0 || matches!(self.stage, PaceStage::OnTrack) {
            return if vietnamese {
                "Đúng nhịp".to_owned()
            } else {
                "On pace".to_owned()
            };
        }
        match self.stage {
            PaceStage::SlightlyAhead | PaceStage::Ahead | PaceStage::FarAhead => {
                if vietnamese {
                    format!("Thiếu {delta}%")
                } else {
                    format!("{delta}% in deficit")
                }
            }
            PaceStage::SlightlyBehind | PaceStage::Behind | PaceStage::FarBehind => {
                if vietnamese {
                    format!("Dư {delta}%")
                } else {
                    format!("{delta}% in reserve")
                }
            }
            PaceStage::OnTrack => {
                if vietnamese {
                    "Đúng nhịp".to_owned()
                } else {
                    "On pace".to_owned()
                }
            }
        }
    }

    /// Optional right-hand ETA: "Lasts until reset" / "Runs out in 2h".
    pub fn eta_label(&self, now: OffsetDateTime) -> Option<String> {
        self.eta_label_localized(now, false)
    }

    pub fn eta_label_localized(&self, now: OffsetDateTime, vietnamese: bool) -> Option<String> {
        if self.will_last_to_reset {
            return Some(if vietnamese {
                "Đủ đến lúc làm mới".to_owned()
            } else {
                "Lasts until reset".to_owned()
            });
        }
        let eta_secs = self.eta_secs?;
        if eta_secs <= 0.0 {
            return Some(if vietnamese {
                "Hết ngay".to_owned()
            } else {
                "Runs out now".to_owned()
            });
        }
        let eta_at = now + time::Duration::seconds(eta_secs.round() as i64);
        let countdown = crate::time_display::format_countdown(eta_at, now);
        Some(if vietnamese {
            format!("Hết sau {countdown}")
        } else {
            format!("Runs out in {countdown}")
        })
    }

    /// Signed delta for menu-bar display: `+14%` / `-5%` / `0%`.
    pub fn delta_text(&self) -> String {
        let delta = self.delta_percent.round() as i64;
        if delta == 0 {
            "0%".to_owned()
        } else if delta > 0 {
            format!("+{delta}%")
        } else {
            format!("{delta}%")
        }
    }
}

/// Compute pace for a quota window. Returns `None` when data is insufficient.
pub fn compute_pace(
    used_percent: u8,
    reset_at: OffsetDateTime,
    now: OffsetDateTime,
    window_secs: f64,
) -> Option<UsagePace> {
    if window_secs <= 0.0 || reset_at <= now {
        return None;
    }

    let time_until_reset = (reset_at - now).whole_seconds().max(0) as f64;
    if time_until_reset > window_secs {
        return None;
    }

    let elapsed = (window_secs - time_until_reset).clamp(0.0, window_secs);
    if elapsed < window_secs * MIN_ELAPSED_FRACTION {
        return None;
    }

    let actual = f64::from(used_percent).clamp(0.0, 100.0);
    if elapsed == 0.0 && actual > 0.0 {
        return None;
    }

    let expected = ((elapsed / window_secs) * 100.0).clamp(0.0, 100.0);
    let delta = actual - expected;
    let stage = stage_for(delta);

    let mut eta_secs = None;
    let mut will_last_to_reset = false;

    if actual >= 100.0 {
        eta_secs = Some(0.0);
    } else if elapsed > 0.0 && actual > 0.0 {
        let rate = actual / elapsed;
        if rate > 0.0 {
            let remaining = 100.0 - actual;
            let candidate = remaining / rate;
            if candidate >= time_until_reset {
                will_last_to_reset = true;
            } else {
                eta_secs = Some(candidate);
            }
        }
    } else if elapsed > 0.0 && actual == 0.0 {
        will_last_to_reset = true;
    }

    Some(UsagePace {
        stage,
        delta_percent: delta,
        expected_used_percent: expected,
        actual_used_percent: actual,
        eta_secs,
        will_last_to_reset,
    })
}

pub fn pace_for_five_hour(
    used_percent: u8,
    reset_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<UsagePace> {
    compute_pace(used_percent, reset_at, now, FIVE_HOUR_WINDOW_SECS)
}

pub fn pace_for_weekly(
    used_percent: u8,
    reset_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<UsagePace> {
    compute_pace(used_percent, reset_at, now, WEEKLY_WINDOW_SECS)
}

fn stage_for(delta: f64) -> PaceStage {
    let abs = delta.abs();
    if abs <= 2.0 {
        PaceStage::OnTrack
    } else if abs <= 6.0 {
        if delta >= 0.0 {
            PaceStage::SlightlyAhead
        } else {
            PaceStage::SlightlyBehind
        }
    } else if abs <= 12.0 {
        if delta >= 0.0 {
            PaceStage::Ahead
        } else {
            PaceStage::Behind
        }
    } else if delta >= 0.0 {
        PaceStage::FarAhead
    } else {
        PaceStage::FarBehind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn on_pace_when_usage_matches_elapsed() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(100);
        // Half the weekly window elapsed → expect ~50% used
        let reset = now + Duration::days(3) + Duration::hours(12);
        let pace = pace_for_weekly(50, reset, now).expect("pace");
        assert!(pace.delta_percent.abs() < 1.0);
        assert_eq!(pace.summary_label(), "On pace");
        assert!(pace.will_last_to_reset);
    }

    #[test]
    fn deficit_when_burning_faster() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(100);
        let reset = now + Duration::days(3) + Duration::hours(12);
        let pace = pace_for_weekly(80, reset, now).expect("pace");
        assert!(pace.delta_percent > 20.0);
        assert!(pace.summary_label().contains("deficit"));
        assert!(!pace.will_last_to_reset);
        assert!(pace.eta_secs.is_some());
    }

    #[test]
    fn reserve_when_burning_slower() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(100);
        let reset = now + Duration::days(3) + Duration::hours(12);
        let pace = pace_for_weekly(20, reset, now).expect("pace");
        assert!(pace.delta_percent < -20.0);
        assert!(pace.summary_label().contains("reserve"));
        assert!(pace.will_last_to_reset);
    }

    #[test]
    fn hidden_near_window_start() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(100);
        // Only ~1% of weekly elapsed
        let reset = now + Duration::days(7) - Duration::hours(1);
        assert!(pace_for_weekly(5, reset, now).is_none());
    }

    #[test]
    fn delta_text_signs() {
        let deficit = UsagePace {
            stage: PaceStage::Ahead,
            delta_percent: 14.2,
            expected_used_percent: 50.0,
            actual_used_percent: 64.2,
            eta_secs: Some(3600.0),
            will_last_to_reset: false,
        };
        assert_eq!(deficit.delta_text(), "+14%");

        let reserve = UsagePace {
            stage: PaceStage::Behind,
            delta_percent: -5.4,
            expected_used_percent: 50.0,
            actual_used_percent: 44.6,
            eta_secs: None,
            will_last_to_reset: true,
        };
        assert_eq!(reserve.delta_text(), "-5%");
    }
}
