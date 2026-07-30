const MILLIS_PER_SECOND: i64 = 1_000;
const MILLIS_PER_MINUTE: i64 = 60 * MILLIS_PER_SECOND;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;
const MILLIS_PER_DAY: i64 = 24 * MILLIS_PER_HOUR;

pub(crate) fn parse_date(input: &str) -> Result<u16, String> {
    let days = parse_civil_date(input)?;
    u16::try_from(days).map_err(|_| {
        format!("Date '{input}' is outside the supported range 1970-01-01 through 2149-06-06")
    })
}

pub(crate) fn format_date(days: u16) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    format!("{year:04}-{month:02}-{day:02}")
}

pub(crate) fn parse_datetime64(input: &str) -> Result<i64, String> {
    let bytes = input.as_bytes();
    if bytes.len() < 19 || !matches!(bytes[10], b'T' | b't' | b' ') {
        return Err(format!(
            "invalid DateTime64(3) '{input}'; expected YYYY-MM-DDTHH:MM:SS[.sss][Z|+HH:MM]"
        ));
    }

    let days = parse_civil_date(&input[..10])?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return Err(format!(
            "invalid DateTime64(3) '{input}'; expected ':' in the time"
        ));
    }
    let hour = parse_digits(&bytes[11..13], "hour", input)?;
    let minute = parse_digits(&bytes[14..16], "minute", input)?;
    let second = parse_digits(&bytes[17..19], "second", input)?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("invalid time in DateTime64(3) '{input}'"));
    }

    let mut position = 19;
    let mut milliseconds = 0;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let fraction_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        let digits = position - fraction_start;
        if !(1..=3).contains(&digits) {
            return Err(format!(
                "DateTime64(3) '{input}' must have between one and three fractional digits"
            ));
        }
        let fraction = parse_digits(&bytes[fraction_start..position], "fraction", input)?;
        milliseconds = fraction * 10_u32.pow(3 - digits as u32);
    }

    let offset_minutes = match bytes.get(position) {
        None => 0_i64,
        Some(b'Z' | b'z') if position + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if position + 6 == bytes.len() => {
            if bytes[position + 3] != b':' {
                return Err(format!(
                    "invalid UTC offset in DateTime64(3) '{input}'; expected +HH:MM or -HH:MM"
                ));
            }
            let offset_hour =
                parse_digits(&bytes[position + 1..position + 3], "offset hour", input)?;
            let offset_minute =
                parse_digits(&bytes[position + 4..position + 6], "offset minute", input)?;
            if offset_hour > 14 || offset_minute > 59 || (offset_hour == 14 && offset_minute != 0) {
                return Err(format!("invalid UTC offset in DateTime64(3) '{input}'"));
            }
            let offset = i64::from(offset_hour * 60 + offset_minute);
            if *sign == b'-' { -offset } else { offset }
        }
        _ => {
            return Err(format!(
                "invalid DateTime64(3) '{input}'; expected YYYY-MM-DDTHH:MM:SS[.sss][Z|+HH:MM]"
            ));
        }
    };

    let local_millis = days * MILLIS_PER_DAY
        + i64::from(hour) * MILLIS_PER_HOUR
        + i64::from(minute) * MILLIS_PER_MINUTE
        + i64::from(second) * MILLIS_PER_SECOND
        + i64::from(milliseconds);
    let utc_millis = local_millis - offset_minutes * MILLIS_PER_MINUTE;
    if !datetime64_in_range(utc_millis) {
        return Err(format!(
            "DateTime64(3) '{input}' is outside the supported UTC range 1900-01-01T00:00:00.000Z through 2299-12-31T23:59:59.999Z"
        ));
    }
    Ok(utc_millis)
}

pub(crate) fn format_datetime64(milliseconds: i64) -> String {
    let days = milliseconds.div_euclid(MILLIS_PER_DAY);
    let time = milliseconds.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = time / MILLIS_PER_HOUR;
    let minute = time % MILLIS_PER_HOUR / MILLIS_PER_MINUTE;
    let second = time % MILLIS_PER_MINUTE / MILLIS_PER_SECOND;
    let fraction = time % MILLIS_PER_SECOND;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:03}Z")
}

pub(crate) fn datetime64_in_range(milliseconds: i64) -> bool {
    let minimum = days_from_civil(1900, 1, 1) * MILLIS_PER_DAY;
    let maximum = days_from_civil(2300, 1, 1) * MILLIS_PER_DAY - 1;
    (minimum..=maximum).contains(&milliseconds)
}

fn parse_civil_date(input: &str) -> Result<i64, String> {
    let bytes = input.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(format!(
            "invalid ISO-8601 date '{input}'; expected YYYY-MM-DD"
        ));
    }
    let year = parse_digits(&bytes[..4], "year", input)? as i32;
    let month = parse_digits(&bytes[5..7], "month", input)?;
    let day = parse_digits(&bytes[8..], "day", input)?;
    let maximum_day = days_in_month(year, month)
        .ok_or_else(|| format!("invalid month in ISO-8601 date '{input}'"))?;
    if day == 0 || day > maximum_day {
        return Err(format!("invalid day in ISO-8601 date '{input}'"));
    }
    Ok(days_from_civil(year, month, day))
}

fn parse_digits(bytes: &[u8], component: &str, input: &str) -> Result<u32, String> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(format!("invalid {component} in ISO-8601 value '{input}'"));
    }
    Ok(bytes
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(*digit - b'0')))
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if is_leap_year(year) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day as i32 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_epoch_leap_years_and_bounds_round_trip() {
        for (text, days) in [
            ("1970-01-01", 0),
            ("2000-02-29", 11_016),
            ("2149-06-06", u16::MAX),
        ] {
            assert_eq!(parse_date(text), Ok(days));
            assert_eq!(format_date(days), text);
        }

        for invalid in ["1900-02-29", "2023-02-29", "1969-12-31", "2149-06-07"] {
            assert!(parse_date(invalid).is_err(), "{invalid} should be invalid");
        }
    }

    #[test]
    fn datetime_normalizes_offsets_and_preserves_milliseconds() {
        let epoch = parse_datetime64("1970-01-01T00:00:00Z").expect("epoch");
        assert_eq!(epoch, 0);
        assert_eq!(
            parse_datetime64("1970-01-01T01:30:00.12+01:30"),
            Ok(epoch + 120)
        );
        assert_eq!(format_datetime64(epoch + 120), "1970-01-01T00:00:00.120Z");
        assert!(parse_datetime64("2000-02-29T23:59:59.999Z").is_ok());
        assert!(parse_datetime64("1900-02-29T00:00:00Z").is_err());
        assert!(parse_datetime64("1970-01-01T00:00:00.0001Z").is_err());
    }

    #[test]
    fn datetime_bounds_are_checked_after_offset_conversion() {
        let minimum = parse_datetime64("1900-01-01T00:00:00.000Z").expect("minimum");
        let maximum = parse_datetime64("2299-12-31T23:59:59.999Z").expect("maximum");
        assert_eq!(format_datetime64(minimum), "1900-01-01T00:00:00.000Z");
        assert_eq!(format_datetime64(maximum), "2299-12-31T23:59:59.999Z");
        assert!(parse_datetime64("1899-12-31T23:59:59.999Z").is_err());
        assert!(parse_datetime64("2300-01-01T00:00:00Z").is_err());
        assert!(parse_datetime64("1900-01-01T00:00:00+00:01").is_err());
    }
}
