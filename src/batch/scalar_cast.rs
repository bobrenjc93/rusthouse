use std::cmp::Ordering;

use super::error::{Error, Result};
use super::value::DataType;

pub(super) fn checked_string_to_int64(value: &str) -> Result<i64> {
    validate_string_to_int64_syntax(value)?;
    value
        .parse::<i64>()
        .map_err(|_| Error::NumericOverflow("CAST(String AS Int64)".to_owned()))
}

pub(super) fn validate_string_to_int64_syntax(value: &str) -> Result<()> {
    decimal_text(value)
        .map(|_| ())
        .ok_or_else(invalid_string_to_int64_cast)
}

fn invalid_string_to_int64_cast() -> Error {
    Error::InvalidCast {
        source_type: DataType::String,
        target_type: DataType::Int64,
    }
}

pub(super) fn checked_string_to_float64(value: &str) -> Result<f64> {
    validate_string_to_float64_syntax(value)?;
    let value = value
        .parse::<f64>()
        .map_err(|_| Error::NumericOverflow("CAST(String AS Float64)".to_owned()))?;
    if !value.is_finite() {
        return Err(Error::NumericOverflow("CAST(String AS Float64)".to_owned()));
    }
    Ok(value)
}

pub(super) fn validate_string_to_float64_syntax(value: &str) -> Result<()> {
    float64_text(value)
        .then_some(())
        .ok_or_else(invalid_string_to_float64_cast)
}

fn invalid_string_to_float64_cast() -> Error {
    Error::InvalidCast {
        source_type: DataType::String,
        target_type: DataType::Float64,
    }
}

pub(super) fn checked_string_to_bool(value: &str) -> Result<bool> {
    bool_text(value).ok_or_else(invalid_string_to_bool_cast)
}

fn invalid_string_to_bool_cast() -> Error {
    Error::InvalidCast {
        source_type: DataType::String,
        target_type: DataType::Bool,
    }
}

fn bool_text(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn float64_text(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let integer_digits = position - integer_start;

    let mut fractional_digits = 0;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let fractional_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        fractional_digits = position - fractional_start;
    }
    if integer_digits == 0 && fractional_digits == 0 {
        return false;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if position == exponent_start {
            return false;
        }
    }
    position == bytes.len()
}

pub(super) fn ordering_string_to_float64(value: &str) -> f64 {
    debug_assert!(float64_text(value));
    value.parse::<f64>().unwrap_or_else(|_| {
        if value.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }
    })
}

#[derive(Clone, Copy)]
struct DecimalText<'a> {
    negative: bool,
    magnitude: &'a [u8],
}

fn decimal_text(value: &str) -> Option<DecimalText<'_>> {
    let bytes = value.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        Some(_) => (false, bytes),
        None => return None,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }

    let first_nonzero = digits.iter().position(|digit| *digit != b'0');
    let magnitude = first_nonzero.map_or_else(
        || &digits[digits.len() - 1..],
        |first_nonzero| &digits[first_nonzero..],
    );
    Some(DecimalText {
        negative: negative && magnitude != b"0",
        magnitude,
    })
}

pub(super) fn decimal_text_cmp(left: &str, right: &str) -> Ordering {
    let left = decimal_text(left).expect("String-to-Int64 ordering values were validated");
    let right = decimal_text(right).expect("String-to-Int64 ordering values were validated");
    match (left.negative, right.negative) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (left_negative, _) => {
            let magnitude_order = left
                .magnitude
                .len()
                .cmp(&right.magnitude.len())
                .then_with(|| left.magnitude.cmp(right.magnitude));
            if left_negative {
                magnitude_order.reverse()
            } else {
                magnitude_order
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int64_cast_separates_malformed_text_from_overflow() {
        for malformed in ["", "+", "-", " 1", "1 ", "1.0", "1e0", "１２"] {
            assert_eq!(
                checked_string_to_int64(malformed),
                Err(Error::InvalidCast {
                    source_type: DataType::String,
                    target_type: DataType::Int64,
                }),
                "{malformed:?}"
            );
        }

        for overflow in ["9223372036854775808", "-9223372036854775809"] {
            assert_eq!(
                checked_string_to_int64(overflow),
                Err(Error::NumericOverflow("CAST(String AS Int64)".to_owned())),
                "{overflow:?}"
            );
        }

        assert_eq!(checked_string_to_int64("+00042"), Ok(42));
        assert_eq!(
            checked_string_to_int64("-9223372036854775808"),
            Ok(i64::MIN)
        );
        assert_eq!(checked_string_to_int64("9223372036854775807"), Ok(i64::MAX));
    }

    #[test]
    fn decimal_ordering_is_numeric_for_arbitrary_valid_magnitudes() {
        assert_eq!(decimal_text_cmp("-000", "+0"), Ordering::Equal);
        assert_eq!(decimal_text_cmp("+0009", "10"), Ordering::Less);
        assert_eq!(decimal_text_cmp("-10", "-0009"), Ordering::Less);
        assert_eq!(
            decimal_text_cmp("999999999999999999999", "1000000000000000000000"),
            Ordering::Less
        );
        assert_eq!(
            decimal_text_cmp("-999999999999999999999", "-1000000000000000000000"),
            Ordering::Greater
        );
    }

    #[test]
    fn float64_cast_validates_decimal_grammar_and_finite_range() {
        for malformed in ["", "+", ".", "1e", " 1", "1 ", "NaN", "inf", "0x1"] {
            assert_eq!(
                checked_string_to_float64(malformed),
                Err(Error::InvalidCast {
                    source_type: DataType::String,
                    target_type: DataType::Float64,
                }),
                "{malformed:?}"
            );
        }

        assert_eq!(
            checked_string_to_float64("1e9999"),
            Err(Error::NumericOverflow("CAST(String AS Float64)".to_owned()))
        );
        assert_eq!(checked_string_to_float64("+1.25e2"), Ok(125.0));

        let negative_zero = checked_string_to_float64("-0.0").expect("valid signed zero");
        assert_eq!(negative_zero.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn float64_order_keys_preserve_signed_zero_and_order_overflow() {
        validate_string_to_float64_syntax("-1e9999").expect("valid decimal syntax");
        validate_string_to_float64_syntax("1e9999").expect("valid decimal syntax");
        assert_eq!(ordering_string_to_float64("-1e9999"), f64::NEG_INFINITY);
        assert_eq!(ordering_string_to_float64("1e9999"), f64::INFINITY);
        assert_eq!(
            ordering_string_to_float64("-0").to_bits(),
            (-0.0_f64).to_bits()
        );
    }

    #[test]
    fn bool_cast_is_ascii_case_insensitive_but_trim_free() {
        assert_eq!(checked_string_to_bool("TRUE"), Ok(true));
        assert_eq!(checked_string_to_bool("fAlSe"), Ok(false));
        for malformed in ["", " true", "false ", "1", "yes", "trüe"] {
            assert_eq!(
                checked_string_to_bool(malformed),
                Err(Error::InvalidCast {
                    source_type: DataType::String,
                    target_type: DataType::Bool,
                }),
                "{malformed:?}"
            );
        }
    }
}
