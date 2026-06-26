use time::{OffsetDateTime, UtcOffset};

pub(crate) fn format_local_reset_at(reset_at: OffsetDateTime) -> String {
    let reset_at = to_local(reset_at);
    format_datetime_vn(reset_at)
}

pub(crate) fn format_short_local_reset_at(reset_at: OffsetDateTime) -> String {
    let reset_at = to_local(reset_at);
    format_short_datetime_vn(reset_at)
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

fn format_short_datetime_vn(reset_at: OffsetDateTime) -> String {
    format!(
        "{:02}/{:02} {:02}:{:02}",
        reset_at.day(),
        reset_at.month() as u8,
        reset_at.hour(),
        reset_at.minute()
    )
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

    use super::format_datetime_vn;

    #[test]
    fn format_datetime_vn_uses_day_month_year_order() {
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
            .replace_time(Time::from_hms(23, 30, 0).unwrap())
            .to_offset(UtcOffset::from_hms(2, 0, 0).unwrap());

        assert_eq!(format_datetime_vn(reset_at), "13/05/2099 01:30");
    }
}
