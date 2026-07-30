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
    let date = std::str::from_utf8(&bytes[..10]).map_err(|_| {
        format!("invalid DateTime64(3) '{input}'; expected an ASCII ISO-8601 timestamp")
    })?;
    let days = parse_civil_date(date)?;
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
        let start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        let digits = position - start;
        if !(1..=3).contains(&digits) {
            return Err(format!(
                "DateTime64(3) '{input}' must have between one and three fractional digits"
            ));
        }
        milliseconds = parse_digits(&bytes[start..position], "fraction", input)?
            * 10_u32.pow(3 - digits as u32);
    }

    let offset_minutes = match bytes.get(position) {
        None => 0_i64,
        Some(b'Z' | b'z') if position + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if position + 6 == bytes.len() => {
            if bytes[position + 3] != b':' {
                return Err(format!("invalid UTC offset in DateTime64(3) '{input}'"));
            }
            let hours = parse_digits(&bytes[position + 1..position + 3], "offset hour", input)?;
            let minutes = parse_digits(&bytes[position + 4..position + 6], "offset minute", input)?;
            if hours > 14 || minutes > 59 || (hours == 14 && minutes != 0) {
                return Err(format!("invalid UTC offset in DateTime64(3) '{input}'"));
            }
            let offset = i64::from(hours * 60 + minutes);
            if *sign == b'-' { -offset } else { offset }
        }
        _ => {
            return Err(format!(
                "invalid DateTime64(3) '{input}'; expected YYYY-MM-DDTHH:MM:SS[.sss][Z|+HH:MM]"
            ));
        }
    };

    let local = days * MILLIS_PER_DAY
        + i64::from(hour) * MILLIS_PER_HOUR
        + i64::from(minute) * MILLIS_PER_MINUTE
        + i64::from(second) * MILLIS_PER_SECOND
        + i64::from(milliseconds);
    let utc = local - offset_minutes * MILLIS_PER_MINUTE;
    if !datetime64_in_range(utc) {
        return Err(format!(
            "DateTime64(3) '{input}' is outside the supported UTC range 1900 through 2299"
        ));
    }
    Ok(utc)
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
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => Some(29),
        2 => Some(28),
        _ => None,
    }
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
    fn temporal_values_round_trip_and_reject_invalid_boundaries() {
        for date in ["1970-01-01", "2000-02-29", "2149-06-06"] {
            assert_eq!(format_date(parse_date(date).expect("valid date")), date);
        }
        assert!(parse_date("2023-02-29").is_err());
        assert!(parse_date("2149-06-07").is_err());

        let timestamp = "2026-07-30T12:34:56.789Z";
        assert_eq!(
            format_datetime64(parse_datetime64(timestamp).expect("valid timestamp")),
            timestamp
        );
        assert!(parse_datetime64("\u{1f4a5}\u{1f4a5}\u{1f4a5}T12:34:56.000Z").is_err());
    }
}
