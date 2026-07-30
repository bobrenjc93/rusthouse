use std::fmt;
use std::str::FromStr;

const MILLIS_PER_DAY: i64 = 86_400_000;
const MIN_YEAR: i32 = 1;
const MAX_YEAR: i32 = 9_999;

/// A calendar date in the proleptic Gregorian calendar.
///
/// Values are stored as an `i32` day offset from 1970-01-01 and are limited to
/// the four-digit ISO range 0001-01-01 through 9999-12-31.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Date(i32);

impl Date {
    /// Construct a date from its day offset from 1970-01-01.
    #[must_use]
    pub fn from_days_since_unix_epoch(days: i32) -> Option<Self> {
        (days >= days_from_civil(MIN_YEAR, 1, 1) as i32
            && days <= days_from_civil(MAX_YEAR, 12, 31) as i32)
            .then_some(Self(days))
    }

    /// Return the number of days since 1970-01-01.
    #[must_use]
    pub const fn days_since_unix_epoch(self) -> i32 {
        self.0
    }

    fn from_components(year: i32, month: u32, day: u32) -> Result<Self, TemporalParseError> {
        validate_date(year, month, day)?;
        let days = days_from_civil(year, month, day);
        Ok(Self(
            i32::try_from(days).expect("four-digit years fit in i32 days"),
        ))
    }

    fn components(self) -> (i32, u32, u32) {
        civil_from_days(i64::from(self.0))
    }
}

impl FromStr for Date {
    type Err = TemporalParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (year, month, day) = parse_date_components(value)?;
        Self::from_components(year, month, day)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.components();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

/// A UTC timestamp with millisecond resolution.
///
/// Values are stored as an `i64` millisecond offset from the Unix epoch and
/// are limited to 0001-01-01T00:00:00.000Z through
/// 9999-12-31T23:59:59.999Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DateTime64(i64);

impl DateTime64 {
    /// Construct a timestamp from its millisecond offset from the Unix epoch.
    #[must_use]
    pub fn from_millis_since_unix_epoch(milliseconds: i64) -> Option<Self> {
        let minimum = days_from_civil(MIN_YEAR, 1, 1) * MILLIS_PER_DAY;
        let maximum = (days_from_civil(MAX_YEAR, 12, 31) + 1) * MILLIS_PER_DAY - 1;
        (milliseconds >= minimum && milliseconds <= maximum).then_some(Self(milliseconds))
    }

    /// Return the number of UTC milliseconds since the Unix epoch.
    #[must_use]
    pub const fn millis_since_unix_epoch(self) -> i64 {
        self.0
    }
}

impl FromStr for DateTime64 {
    type Err = TemporalParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != 24
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
            || bytes[19] != b'.'
            || bytes[23] != b'Z'
        {
            return Err(TemporalParseError::new("expected YYYY-MM-DDTHH:MM:SS.sssZ"));
        }

        let (year, month, day) = parse_date_components(&value[..10])?;
        let date = Date::from_components(year, month, day)?;
        let hour = parse_digits(bytes, 11, 13, "hour")?;
        let minute = parse_digits(bytes, 14, 16, "minute")?;
        let second = parse_digits(bytes, 17, 19, "second")?;
        let millisecond = parse_digits(bytes, 20, 23, "millisecond")?;
        if hour > 23 {
            return Err(TemporalParseError::new("hour must be between 00 and 23"));
        }
        if minute > 59 {
            return Err(TemporalParseError::new("minute must be between 00 and 59"));
        }
        if second > 59 {
            return Err(TemporalParseError::new("second must be between 00 and 59"));
        }

        let time = i64::from(hour) * 3_600_000
            + i64::from(minute) * 60_000
            + i64::from(second) * 1_000
            + i64::from(millisecond);
        Ok(Self(
            i64::from(date.days_since_unix_epoch()) * MILLIS_PER_DAY + time,
        ))
    }
}

impl fmt::Display for DateTime64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(MILLIS_PER_DAY);
        let time = self.0.rem_euclid(MILLIS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let hour = time / 3_600_000;
        let minute = time % 3_600_000 / 60_000;
        let second = time % 60_000 / 1_000;
        let millisecond = time % 1_000;
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
        )
    }
}

/// Describes why an ISO temporal literal could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalParseError {
    message: String,
}

impl TemporalParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TemporalParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TemporalParseError {}

fn parse_date_components(value: &str) -> Result<(i32, u32, u32), TemporalParseError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(TemporalParseError::new("expected YYYY-MM-DD"));
    }
    let year = parse_digits(bytes, 0, 4, "year")? as i32;
    let month = parse_digits(bytes, 5, 7, "month")?;
    let day = parse_digits(bytes, 8, 10, "day")?;
    Ok((year, month, day))
}

fn parse_digits(
    bytes: &[u8],
    start: usize,
    end: usize,
    component: &str,
) -> Result<u32, TemporalParseError> {
    let digits = &bytes[start..end];
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err(TemporalParseError::new(format!(
            "{component} must contain only digits"
        )));
    }
    Ok(digits
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(digit - b'0')))
}

fn validate_date(year: i32, month: u32, day: u32) -> Result<(), TemporalParseError> {
    if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
        return Err(TemporalParseError::new(
            "year must be between 0001 and 9999",
        ));
    }
    if !(1..=12).contains(&month) {
        return Err(TemporalParseError::new("month must be between 01 and 12"));
    }
    let maximum = days_in_month(year, month);
    if !(1..=maximum).contains(&day) {
        return Err(TemporalParseError::new(format!(
            "day must be between 01 and {maximum:02} for {year:04}-{month:02}"
        )));
    }
    Ok(())
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

// Howard Hinnant's civil calendar algorithms, with 1970-01-01 as day zero.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let adjusted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * adjusted_month + 2) / 5 + 1;
    let month = adjusted_month + if adjusted_month < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        i32::try_from(year).expect("supported dates have i32 years"),
        u32::try_from(month).expect("calendar month is positive"),
        u32::try_from(day).expect("calendar day is positive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_validate_leap_years_and_round_trip_epoch_offsets() {
        let epoch = "1970-01-01".parse::<Date>().expect("epoch date");
        assert_eq!(epoch.days_since_unix_epoch(), 0);
        assert_eq!(epoch.to_string(), "1970-01-01");

        assert!("2000-02-29".parse::<Date>().is_ok());
        assert!("2024-02-29".parse::<Date>().is_ok());
        assert!("1900-02-29".parse::<Date>().is_err());
        assert!("2023-02-29".parse::<Date>().is_err());
    }

    #[test]
    fn temporal_range_boundaries_round_trip() {
        let minimum_date = "0001-01-01".parse::<Date>().expect("minimum Date");
        let maximum_date = "9999-12-31".parse::<Date>().expect("maximum Date");
        for (value, date) in [("0001-01-01", minimum_date), ("9999-12-31", maximum_date)] {
            assert_eq!(date.to_string(), value);
            assert_eq!(
                Date::from_days_since_unix_epoch(date.days_since_unix_epoch()),
                Some(date)
            );
        }
        assert_eq!(
            Date::from_days_since_unix_epoch(minimum_date.days_since_unix_epoch() - 1),
            None
        );
        assert_eq!(
            Date::from_days_since_unix_epoch(maximum_date.days_since_unix_epoch() + 1),
            None
        );

        let minimum_timestamp = "0001-01-01T00:00:00.000Z"
            .parse::<DateTime64>()
            .expect("minimum DateTime64");
        let maximum_timestamp = "9999-12-31T23:59:59.999Z"
            .parse::<DateTime64>()
            .expect("maximum DateTime64");
        for (value, timestamp) in [
            ("0001-01-01T00:00:00.000Z", minimum_timestamp),
            (
                "1969-12-31T23:59:59.999Z",
                "1969-12-31T23:59:59.999Z"
                    .parse::<DateTime64>()
                    .expect("pre-epoch timestamp"),
            ),
            ("9999-12-31T23:59:59.999Z", maximum_timestamp),
        ] {
            assert_eq!(timestamp.to_string(), value);
            assert_eq!(
                DateTime64::from_millis_since_unix_epoch(timestamp.millis_since_unix_epoch()),
                Some(timestamp)
            );
        }
        assert_eq!(
            DateTime64::from_millis_since_unix_epoch(
                minimum_timestamp.millis_since_unix_epoch() - 1
            ),
            None
        );
        assert_eq!(
            DateTime64::from_millis_since_unix_epoch(
                maximum_timestamp.millis_since_unix_epoch() + 1
            ),
            None
        );
    }

    #[test]
    fn strict_iso_parsing_rejects_noncanonical_and_invalid_inputs() {
        for value in [
            "2024-1-01",
            "2024/01/01",
            "0000-01-01",
            "2024-13-01",
            "2024-04-31",
            "2024-01-01 ",
        ] {
            assert!(value.parse::<Date>().is_err(), "accepted Date {value:?}");
        }
        for value in [
            "2024-01-01",
            "2024-01-01 00:00:00.000Z",
            "2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00.000",
            "2024-01-01T00:00:00.000+00:00",
            "2024-01-01t00:00:00.000z",
            "2024-01-01T24:00:00.000Z",
            "2024-01-01T00:60:00.000Z",
            "2024-01-01T00:00:60.000Z",
        ] {
            assert!(
                value.parse::<DateTime64>().is_err(),
                "accepted DateTime64 {value:?}"
            );
        }
    }
}
