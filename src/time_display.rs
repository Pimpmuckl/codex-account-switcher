use time::{OffsetDateTime, UtcOffset};

pub(crate) fn format_local_reset_at(reset_at: OffsetDateTime) -> String {
    let reset_at = UtcOffset::local_offset_at(reset_at)
        .map(|offset| reset_at.to_offset(offset))
        .unwrap_or(reset_at);

    format_reset_at(reset_at)
}

fn format_reset_at(reset_at: OffsetDateTime) -> String {
    format!(
        "{} {:02}:{:02}",
        reset_at.date(),
        reset_at.hour(),
        reset_at.minute()
    )
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

    use super::format_reset_at;

    #[test]
    fn format_reset_at_uses_supplied_offset_time() {
        let reset_at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2099, Month::May, 12).unwrap())
            .replace_time(Time::from_hms(23, 30, 0).unwrap())
            .to_offset(UtcOffset::from_hms(2, 0, 0).unwrap());

        assert_eq!(format_reset_at(reset_at), "2099-05-13 01:30");
    }
}
