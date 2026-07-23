//! Flexible OffsetDateTime serde: emit RFC3339 for JS clients, accept RFC3339
//! or the legacy `time` crate tuple form used in older on-disk metadata.

use serde::{Deserialize, Deserializer, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

pub mod offset_datetime {
    use super::*;

    pub fn serialize<S>(dt: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = dt.format(&Rfc3339).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        parse_offset_datetime(&value).map_err(serde::de::Error::custom)
    }
}

pub mod option_offset_datetime {
    use super::*;

    pub fn serialize<S>(dt: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match dt {
            Some(dt) => offset_datetime::serialize(dt, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => parse_offset_datetime(&value)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

fn parse_offset_datetime(value: &serde_json::Value) -> Result<OffsetDateTime, String> {
    match value {
        serde_json::Value::String(s) => OffsetDateTime::parse(s, &Rfc3339)
            .or_else(|_| {
                // Accept trailing Z / space variants via a quick normalize.
                let trimmed = s.trim();
                OffsetDateTime::parse(trimmed, &Rfc3339)
            })
            .map_err(|e| format!("invalid RFC3339 datetime {s:?}: {e}")),
        serde_json::Value::Array(items) if items.len() >= 6 => {
            // Legacy `time` serde shape:
            // [year, ordinal, hour, minute, second, nanosecond, off_h, off_m, off_s]
            let year = items[0].as_i64().ok_or("year")? as i32;
            let ordinal = items[1].as_u64().ok_or("ordinal")? as u16;
            let hour = items[2].as_u64().ok_or("hour")? as u8;
            let minute = items[3].as_u64().ok_or("minute")? as u8;
            let second = items[4].as_u64().ok_or("second")? as u8;
            let nanosecond = items[5].as_u64().unwrap_or(0) as u32;
            let off_h = items.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
            let off_m = items.get(7).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
            let off_s = items.get(8).and_then(|v| v.as_i64()).unwrap_or(0) as i8;

            let date = Date::from_ordinal_date(year, ordinal)
                .map_err(|e| format!("invalid ordinal date: {e}"))?;
            let time = Time::from_hms_nano(hour, minute, second, nanosecond)
                .map_err(|e| format!("invalid time: {e}"))?;
            let offset = UtcOffset::from_hms(off_h, off_m, off_s)
                .map_err(|e| format!("invalid offset: {e}"))?;
            Ok(PrimitiveDateTime::new(date, time).assume_offset(offset))
        }
        other => Err(format!("unsupported datetime JSON shape: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use time::Month;

    #[derive(Serialize, Deserialize)]
    struct Sample {
        #[serde(with = "offset_datetime")]
        at: OffsetDateTime,
    }

    #[test]
    fn round_trips_rfc3339() {
        let at = OffsetDateTime::UNIX_EPOCH
            .replace_date(Date::from_calendar_date(2026, Month::July, 23).unwrap())
            .replace_time(Time::from_hms(14, 30, 0).unwrap());
        let json = serde_json::to_string(&Sample { at }).unwrap();
        assert!(json.contains("2026-07-23"));
        let back: Sample = serde_json::from_str(&json).unwrap();
        assert_eq!(back.at.year(), 2026);
        assert_eq!(back.at.month(), Month::July);
        assert_eq!(back.at.day(), 23);
    }

    #[test]
    fn deserializes_legacy_tuple() {
        // year=2026, ordinal=204 (~July 23), 07:51:00 UTC
        let json = r#"{"at":[2026,204,7,51,0,0,0,0,0]}"#;
        let sample: Sample = serde_json::from_str(json).unwrap();
        assert_eq!(sample.at.year(), 2026);
        assert_eq!(sample.at.hour(), 7);
        assert_eq!(sample.at.minute(), 51);
    }
}
