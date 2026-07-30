use std::fmt::Write;

const MILLIS_PER_SECOND: i64 = 1_000;
const SECONDS_PER_DAY: i64 = 86_400;
const MILLIS_PER_DAY: i64 = SECONDS_PER_DAY * MILLIS_PER_SECOND;

pub(crate) const MIN_DATE_DAYS: i32 = -719_162;
pub(crate) const MAX_DATE_DAYS: i32 = 2_932_896;
pub(crate) const MIN_DATETIME_MILLIS: i64 = -62_135_596_800_000;
pub(crate) const MAX_DATETIME_MILLIS: i64 = 253_402_300_799_999;

pub(crate) fn parse_date(value: &str) -> Result<i32, String> {
    let (year, month, day) = parse_civil_date(value)?;
    let days = days_from_civil(year, month, day);
    i32::try_from(days).map_err(|_| {
        format!(
            "Date literal '{}' is out of range",
            escape_diagnostic_literal(value)
        )
    })
}

pub(crate) fn parse_datetime64(value: &str) -> Result<i64, String> {
    let bytes = value.as_bytes();
    let diagnostic_value = escape_diagnostic_literal(value);
    if !value.is_ascii() || bytes.len() < 19 || bytes.get(10) != Some(&b' ') {
        return Err(format!(
            "invalid TIMESTAMP literal '{diagnostic_value}'; expected YYYY-MM-DD HH:MM:SS[.fff]"
        ));
    }

    let (year, month, day) = parse_civil_date(&value[..10]).map_err(|message| {
        if message.contains("out of range") {
            format!(
                "TIMESTAMP literal '{diagnostic_value}' is out of range; supported range is 0001-01-01 through 9999-12-31"
            )
        } else {
            format!("invalid TIMESTAMP literal '{diagnostic_value}'")
        }
    })?;
    if bytes.get(13) != Some(&b':') || bytes.get(16) != Some(&b':') {
        return Err(format!("invalid TIMESTAMP literal '{diagnostic_value}'"));
    }
    let hour = parse_digits(bytes, 11, 2)
        .ok_or_else(|| format!("invalid TIMESTAMP literal '{diagnostic_value}'"))?;
    let minute = parse_digits(bytes, 14, 2)
        .ok_or_else(|| format!("invalid TIMESTAMP literal '{diagnostic_value}'"))?;
    let second = parse_digits(bytes, 17, 2)
        .ok_or_else(|| format!("invalid TIMESTAMP literal '{diagnostic_value}'"))?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("invalid TIMESTAMP literal '{diagnostic_value}'"));
    }

    let millisecond = match &bytes[19..] {
        [] => 0,
        [b'.', fraction @ ..] if (1..=3).contains(&fraction.len()) => {
            if !fraction.iter().all(u8::is_ascii_digit) {
                return Err(format!("invalid TIMESTAMP literal '{diagnostic_value}'"));
            }
            let parsed = fraction.iter().fold(0_i64, |number, digit| {
                number * 10 + i64::from(*digit - b'0')
            });
            parsed * 10_i64.pow((3 - fraction.len()) as u32)
        }
        [b'.', ..] => {
            return Err(format!(
                "TIMESTAMP literal '{diagnostic_value}' exceeds DateTime64(3) millisecond precision"
            ));
        }
        _ => {
            return Err(format!("invalid TIMESTAMP literal '{diagnostic_value}'"));
        }
    };

    days_from_civil(year, month, day)
        .checked_mul(MILLIS_PER_DAY)
        .and_then(|value| value.checked_add(i64::from(hour) * 3_600_000))
        .and_then(|value| value.checked_add(i64::from(minute) * 60_000))
        .and_then(|value| value.checked_add(i64::from(second) * MILLIS_PER_SECOND))
        .and_then(|value| value.checked_add(millisecond))
        .filter(|value| is_valid_datetime_millis(*value))
        .ok_or_else(|| format!("TIMESTAMP literal '{diagnostic_value}' is out of range"))
}

pub(crate) fn is_valid_date_days(value: i32) -> bool {
    (MIN_DATE_DAYS..=MAX_DATE_DAYS).contains(&value)
}

pub(crate) fn is_valid_datetime_millis(value: i64) -> bool {
    (MIN_DATETIME_MILLIS..=MAX_DATETIME_MILLIS).contains(&value)
}

pub(crate) fn format_date(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    format_civil_date(year, month, day)
}

pub(crate) fn format_datetime64(milliseconds: i64) -> String {
    let days = milliseconds.div_euclid(MILLIS_PER_DAY);
    let within_day = milliseconds.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = within_day / 3_600_000;
    let minute = (within_day % 3_600_000) / 60_000;
    let second = (within_day % 60_000) / MILLIS_PER_SECOND;
    let millisecond = within_day % MILLIS_PER_SECOND;
    format!(
        "{}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z",
        format_civil_date(year, month, day)
    )
}

fn parse_civil_date(value: &str) -> Result<(i64, u32, u32), String> {
    let bytes = value.as_bytes();
    let diagnostic_value = escape_diagnostic_literal(value);
    if bytes.len() != 10 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return Err(format!(
            "invalid DATE literal '{diagnostic_value}'; expected YYYY-MM-DD"
        ));
    }
    let year = parse_digits(bytes, 0, 4)
        .ok_or_else(|| format!("invalid DATE literal '{diagnostic_value}'"))?;
    let month = parse_digits(bytes, 5, 2)
        .ok_or_else(|| format!("invalid DATE literal '{diagnostic_value}'"))?;
    let day = parse_digits(bytes, 8, 2)
        .ok_or_else(|| format!("invalid DATE literal '{diagnostic_value}'"))?;
    if year == 0 {
        return Err(format!(
            "DATE literal '{diagnostic_value}' is out of range; supported range is 0001-01-01 through 9999-12-31"
        ));
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err(format!("invalid DATE literal '{diagnostic_value}'"));
    }
    Ok((i64::from(year), month, day))
}

fn escape_diagnostic_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("\\'"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if value.is_control() => {
                write!(escaped, "\\u{{{:04x}}}", value as u32)
                    .expect("writing to String cannot fail");
            }
            value => escaped.push(value),
        }
    }
    escaped
}

fn parse_digits(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    let digits = bytes.get(start..start.checked_add(length)?)?;
    digits.iter().all(u8::is_ascii_digit).then(|| {
        digits.iter().fold(0_u32, |number, digit| {
            number * 10 + u32::from(*digit - b'0')
        })
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted_days = days + 719_468;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

fn format_civil_date(year: i64, month: u32, day: u32) -> String {
    if (0..=9_999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year}-{month:02}-{day:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_pre_epoch_values_round_trip() {
        assert_eq!(parse_date("1970-01-01"), Ok(0));
        assert_eq!(parse_date("1969-12-31"), Ok(-1));
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(parse_datetime64("1970-01-01 00:00:00.000"), Ok(0));
        assert_eq!(parse_datetime64("1969-12-31 23:59:59.999"), Ok(-1));
        assert_eq!(format_datetime64(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn validates_gregorian_leap_years() {
        assert!(parse_date("2000-02-29").is_ok());
        assert!(parse_date("2024-02-29").is_ok());
        assert!(parse_date("1900-02-29").is_err());
        assert!(parse_date("2023-02-29").is_err());
    }

    #[test]
    fn checks_supported_range_and_millisecond_precision() {
        assert_eq!(parse_date("0001-01-01"), Ok(MIN_DATE_DAYS));
        assert_eq!(parse_date("9999-12-31"), Ok(MAX_DATE_DAYS));
        assert_eq!(format_date(MIN_DATE_DAYS), "0001-01-01");
        assert_eq!(format_date(MAX_DATE_DAYS), "9999-12-31");
        assert_eq!(
            parse_datetime64("0001-01-01 00:00:00"),
            Ok(MIN_DATETIME_MILLIS)
        );
        assert_eq!(
            parse_datetime64("9999-12-31 23:59:59.999"),
            Ok(MAX_DATETIME_MILLIS)
        );
        assert_eq!(
            format_datetime64(MIN_DATETIME_MILLIS),
            "0001-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_datetime64(MAX_DATETIME_MILLIS),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(parse_datetime64("1970-01-01 00:00:00.1"), Ok(100));
        assert_eq!(parse_datetime64("1970-01-01 00:00:00.12"), Ok(120));
        assert!(parse_datetime64("1970-01-01 00:00:00.0001").is_err());
    }
}
