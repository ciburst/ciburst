//! UTC calendar arithmetic for the timestamps of the cloud APIs (RFC 3339) and SigV4 dates.
//! Howard Hinnant's civil-date algorithms, valid for the whole proleptic Gregorian calendar.

/// A UTC date and time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateTime {
    pub fn from_unix(unix: u64) -> DateTime {
        let days = (unix / 86_400) as i64;
        let secs = (unix % 86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        DateTime {
            year,
            month,
            day,
            hour: secs / 3600,
            minute: (secs / 60) % 60,
            second: secs % 60,
        }
    }

    /// `YYYY-MM-DD`.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `HH:MM:SS`.
    pub fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(unix: u64) -> String {
    let at = DateTime::from_unix(unix);
    format!("{}T{}Z", at.date(), at.time())
}

/// Days since 1970-01-01 of a civil date.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (month, day) = (i64::from(month), i64::from(day));
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01, the inverse of [`days_from_civil`].
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Unix seconds of an RFC 3339 timestamp such as `2026-10-24T12:00:00.123456+00:00` or
/// `2026-10-24T12:00:00Z`; `None` for anything else and for times before 1970.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let (year, month, day) = (
        number(text, 0..4)?,
        number(text, 5..7)?,
        number(text, 8..10)?,
    );
    let (hour, minute, second) = (
        number(text, 11..13)?,
        number(text, 14..16)?,
        number(text, 17..19)?,
    );
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            sign * (number(rest, 1..3)? * 3600 + number(rest, 4..6)? * 60)
        }
    };
    let seconds = days_from_civil(year, month as u32, day as u32) * 86_400
        + hour * 3600
        + minute * 60
        + second
        - offset;
    u64::try_from(seconds).ok()
}

/// Unix seconds of a timestamp of a GitLab hook, `2026-10-26 06:45:46 UTC`, or of an RFC 3339
/// one; `None` for anything else.
pub fn parse_gitlab_time(text: &str) -> Option<u64> {
    match text.strip_suffix(" UTC") {
        Some(utc) => parse_rfc3339(&format!("{utc}Z")),
        None => parse_rfc3339(text),
    }
}

/// The decimal number at `range` of `text`, digits only.
fn number(text: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let part = text.get(range)?;
    part.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| part.parse().ok())?
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_792_800_000;

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        for days in [-719_468, -1, 0, 59, 60, 11_016, 20_750, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "{days}");
        }
    }

    #[test]
    fn date_times_split_unix_seconds() {
        let at = DateTime::from_unix(1_709_210_096);
        assert_eq!(
            at,
            DateTime {
                year: 2024,
                month: 2,
                day: 29,
                hour: 12,
                minute: 34,
                second: 56,
            }
        );
        assert_eq!(
            (at.date(), at.time()),
            ("2024-02-29".into(), "12:34:56".into())
        );
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(T0), "2026-10-24T00:00:00Z");
    }

    #[test]
    fn parses_rfc3339_with_fractions_and_offsets() {
        assert_eq!(parse_rfc3339("2026-10-24T00:00:00Z"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-24t00:00:00z"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-24 00:00:00Z"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-24T02:00:00.123456+02:00"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-23T22:30:00-01:30"), Some(T0));
        assert_eq!(parse_rfc3339("2026-10-24T00:00:00.000Z"), Some(T0));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        for unix in [0, 951_782_400, 4_102_444_800, T0 + 86_399] {
            assert_eq!(parse_rfc3339(&rfc3339(unix)), Some(unix));
        }
        for bad in [
            "",
            "2026-10-24",
            "2026-13-01T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-10-24T24:00:00Z",
            "2026-10-24T00:00:00",
            "2026-10-24T00:00:00.Z",
            "2026-10-24T00:00:00+0200",
            "2026-10-24T00:00:00+0a:00",
            "2026/10/24T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "+026-10-24T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_the_timestamps_of_gitlab_hooks() {
        assert_eq!(parse_gitlab_time("2026-10-24 00:00:00 UTC"), Some(T0));
        assert_eq!(parse_gitlab_time("2026-10-24 00:00:07 UTC"), Some(T0 + 7));
        assert_eq!(parse_gitlab_time("2026-10-24T00:00:00Z"), Some(T0));
        for bad in [
            "",
            "UTC",
            "2026-10-24 00:00:00",
            "2026-10-24 00:00:00 CET",
            "2026-10-24 00:00:00 UTCZ",
        ] {
            assert_eq!(parse_gitlab_time(bad), None, "{bad}");
        }
    }
}
