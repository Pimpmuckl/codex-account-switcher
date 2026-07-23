use time::{OffsetDateTime, UtcOffset};

pub(crate) fn format_local_reset_at(reset_at: OffsetDateTime) -> String {
    let reset_at = to_local(reset_at);
    format_datetime_vn(reset_at)
}

fn to_local(reset_at: OffsetDateTime) -> OffsetDateTime {
    UtcOffset::local_offset_at(reset_at)
        .map(|offset| reset_at.to_offset(offset))
        .unwrap_or(reset_at)
}

fn format_datetime_vn(reset_at: OffsetDateTime) -> String {
    format!(
        "{:02}/{:02}/{} {:02}:{:02}",
        reset_at.day(),
        reset_at.month() as u8,
        reset_at.year(),
        reset_at.hour(),
        reset_at.minute()
    )
}

pub(crate) fn format_countdown(reset_at: OffsetDateTime, now: OffsetDateTime) -> String {
    if reset_at <= now {
        return "now".to_owned();
    }
    let diff = reset_at - now;
    let days = diff.whole_days();
    let hours = diff.whole_hours() % 24;
    let minutes = diff.whole_minutes() % 60;

    if days > 0 {
        if hours > 0 {
            format!("{}d {}h", days, hours)
        } else {
            format!("{}d", days)
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}h", hours)
        }
    } else {
        let mins = minutes.max(1);
        format!("{}m", mins)
    }
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

    use super::{format_countdown, format_datetime_vn};

    #[test]
    fn format_datetime_vn_uses_day_month_year_order() {
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
            .replace_time(Time::from_hms(23, 30, 0).unwrap())
            .to_offset(UtcOffset::from_hms(2, 0, 0).unwrap());

        assert_eq!(format_datetime_vn(reset_at), "13/05/2099 01:30");
    }

    #[test]
    fn test_format_countdown() {
        let now = OffsetDateTime::UNIX_EPOCH;

        // negative or zero diff
        assert_eq!(format_countdown(now, now), "now");
        assert_eq!(
            format_countdown(now - time::Duration::minutes(5), now),
            "now"
        );

        // minutes
        assert_eq!(
            format_countdown(now + time::Duration::seconds(30), now),
            "1m"
        );
        assert_eq!(
            format_countdown(now + time::Duration::minutes(5), now),
            "5m"
        );
        assert_eq!(
            format_countdown(now + time::Duration::minutes(59), now),
            "59m"
        );

        // hours
        assert_eq!(format_countdown(now + time::Duration::hours(2), now), "2h");
        assert_eq!(
            format_countdown(
                now + time::Duration::hours(2) + time::Duration::minutes(15),
                now
            ),
            "2h 15m"
        );

        // days
        assert_eq!(format_countdown(now + time::Duration::days(3), now), "3d");
        assert_eq!(
            format_countdown(
                now + time::Duration::days(3) + time::Duration::hours(5),
                now
            ),
            "3d 5h"
        );
        assert_eq!(
            format_countdown(
                now + time::Duration::days(3)
                    + time::Duration::hours(5)
                    + time::Duration::minutes(12),
                now
            ),
            "3d 5h"
        );
    }
}
