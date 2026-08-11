//! Allocation-free String CAST validation and numeric ordering semantics.

use std::cmp::Ordering;

use super::error::{Error, Result};
use super::value::DataType;

pub(super) fn checked_string_to_int64(value: &str) -> Result<i64> {
    validate_string_to_int64(value)?;
    value
        .parse::<i64>()
        .map_err(|_| Error::NumericOverflow("CAST(String AS Int64)".to_owned()))
}

pub(super) fn checked_string_to_float64(value: &str) -> Result<f64> {
    validate_string_to_float64(value)?;
    let value = value
        .parse::<f64>()
        .map_err(|_| Error::NumericOverflow("CAST(String AS Float64)".to_owned()))?;
    if !value.is_finite() {
        return Err(Error::NumericOverflow("CAST(String AS Float64)".to_owned()));
    }
    Ok(value)
}

pub(super) fn checked_string_to_bool(value: &str) -> Result<bool> {
    bool_text(value).ok_or_else(invalid_string_to_bool_cast)
}

/// Validates String-to-`Int64` syntax without applying the target range.
///
/// Ordering uses this distinction so out-of-range decimal text can be sorted
/// before projection decides whether a selected value overflows.
pub(super) fn validate_string_to_int64(value: &str) -> Result<()> {
    decimal_text(value)
        .map(|_| ())
        .ok_or_else(invalid_string_to_int64_cast)
}

/// Validates String-to-`Float64` syntax without applying the finite range.
pub(super) fn validate_string_to_float64(value: &str) -> Result<()> {
    float64_text(value)
        .then_some(())
        .ok_or_else(invalid_string_to_float64_cast)
}

pub(super) fn validate_string_to_bool(value: &str) -> Result<()> {
    bool_text(value)
        .map(|_| ())
        .ok_or_else(invalid_string_to_bool_cast)
}

/// Parses a previously validated Bool ordering key.
pub(super) fn ordering_string_to_bool(value: &str) -> bool {
    bool_text(value).expect("String-to-Bool ordering syntax is validated")
}

/// Parses a previously validated Float64 ordering key.
///
/// Syntactically valid values outside the finite `f64` range sort as the
/// corresponding infinity. Projection retains the checked overflow error.
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

/// Numerically compares two previously validated signed decimal integers.
///
/// This avoids parsing into a bounded integer, allocating normalized strings,
/// or losing the equivalence between signed and padded zero spellings.
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

fn invalid_string_to_int64_cast() -> Error {
    Error::InvalidCast {
        source_type: DataType::String,
        target_type: DataType::Int64,
    }
}

fn invalid_string_to_float64_cast() -> Error {
    Error::InvalidCast {
        source_type: DataType::String,
        target_type: DataType::Float64,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_conversions_preserve_types_boundaries_and_signed_zero() {
        assert_eq!(
            checked_string_to_int64("-9223372036854775808"),
            Ok(i64::MIN)
        );
        assert_eq!(checked_string_to_int64("+000"), Ok(0));
        assert_eq!(checked_string_to_bool("TrUe"), Ok(true));
        assert_eq!(checked_string_to_bool("FALSE"), Ok(false));

        let negative_zero = checked_string_to_float64("-2e-324").expect("finite underflow");
        assert_eq!(negative_zero.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(
            checked_string_to_float64("1.7976931348623157e308"),
            Ok(f64::MAX)
        );
    }

    #[test]
    fn malformed_and_overflowing_text_have_distinct_typed_errors() {
        assert_eq!(
            checked_string_to_int64("1.0"),
            Err(Error::InvalidCast {
                source_type: DataType::String,
                target_type: DataType::Int64,
            })
        );
        assert_eq!(
            checked_string_to_int64("9223372036854775808"),
            Err(Error::NumericOverflow("CAST(String AS Int64)".to_owned()))
        );
        assert_eq!(
            checked_string_to_float64("NaN"),
            Err(Error::InvalidCast {
                source_type: DataType::String,
                target_type: DataType::Float64,
            })
        );
        assert_eq!(
            checked_string_to_float64("1e999"),
            Err(Error::NumericOverflow("CAST(String AS Float64)".to_owned()))
        );
        assert_eq!(
            checked_string_to_bool(" true"),
            Err(Error::InvalidCast {
                source_type: DataType::String,
                target_type: DataType::Bool,
            })
        );
    }

    #[test]
    fn syntax_only_validation_allows_ordering_before_range_checks() {
        assert_eq!(validate_string_to_int64("-9223372036854775809"), Ok(()));
        assert_eq!(validate_string_to_float64("+1e999"), Ok(()));
        assert_eq!(ordering_string_to_float64("+1e999"), f64::INFINITY);
        assert_eq!(ordering_string_to_float64("-1e999"), f64::NEG_INFINITY);
        assert!(ordering_string_to_bool("TRUE"));
    }

    #[test]
    fn decimal_comparison_is_numeric_for_unbounded_and_zero_text() {
        assert_eq!(decimal_text_cmp("-0", "+000"), Ordering::Equal);
        assert_eq!(decimal_text_cmp("0002", "+10"), Ordering::Less);
        assert_eq!(
            decimal_text_cmp("9223372036854775808", "999999999999999999999"),
            Ordering::Less
        );
        assert_eq!(
            decimal_text_cmp("-999999999999999999999", "-9223372036854775809"),
            Ordering::Less
        );
    }
}
