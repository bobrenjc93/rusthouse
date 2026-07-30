use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::error::{Error, Result};

pub const MAX_DECIMAL128_PRECISION: u8 = 38;

/// The physical column types supported by RustHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Float64,
    Decimal128 { precision: u8, scale: u8 },
    Bool,
    String,
}

impl DataType {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "INT64" => Some(Self::Int64),
            "FLOAT64" => Some(Self::Float64),
            "BOOL" | "BOOLEAN" => Some(Self::Bool),
            "STRING" => Some(Self::String),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Decimal128 { precision, scale } => {
                return write!(f, "Decimal128({precision}, {scale})");
            }
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// An exact fixed-point value stored as an integer coefficient and a scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal128 {
    coefficient: i128,
    precision: u8,
    scale: u8,
}

impl fmt::Display for Decimal128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_decimal(*self))
    }
}

impl Decimal128 {
    pub fn new(coefficient: i128, precision: u8, scale: u8) -> Result<Self> {
        validate_decimal_type(precision, scale)?;
        if !coefficient_fits_precision(coefficient, precision) {
            return Err(Error::NumericOverflow(format!(
                "Decimal128({precision}, {scale}) value"
            )));
        }
        Ok(Self {
            coefficient,
            precision,
            scale,
        })
    }

    pub(crate) fn parse(literal: &str, precision: u8, scale: u8) -> Result<Self> {
        validate_decimal_type(precision, scale)?;
        let (negative, unsigned) = literal
            .strip_prefix('-')
            .map_or((false, literal), |value| (true, value));
        let (mantissa, exponent) = split_exponent(unsigned, literal)?;
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if whole.is_empty()
            || !whole.bytes().all(|value| value.is_ascii_digit())
            || !fraction.bytes().all(|value| value.is_ascii_digit())
        {
            return Err(invalid_decimal_literal(literal));
        }

        let combined = format!("{whole}{fraction}");
        let digits = combined.trim_start_matches('0');
        if digits.is_empty() {
            return Self::new(0, precision, scale);
        }

        let shift = exponent
            .checked_sub(i64::try_from(fraction.len()).unwrap_or(i64::MAX))
            .and_then(|value| value.checked_add(i64::from(scale)))
            .ok_or_else(|| invalid_decimal_literal(literal))?;
        let mut coefficient_digits = if shift >= 0 {
            let zeros = usize::try_from(shift).map_err(|_| invalid_decimal_literal(literal))?;
            if digits.len().saturating_add(zeros) > usize::from(precision) {
                return Err(decimal_literal_overflow(literal, precision, scale));
            }
            let mut value = String::with_capacity(digits.len() + zeros);
            value.push_str(digits);
            value.extend(std::iter::repeat_n('0', zeros));
            value
        } else {
            let dropped = shift
                .checked_neg()
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(usize::MAX);
            let kept = digits.len().saturating_sub(dropped);
            let mut value = digits[..kept].trim_start_matches('0').to_owned();
            let round_up = dropped <= digits.len()
                && digits
                    .as_bytes()
                    .get(kept)
                    .is_some_and(|digit| *digit >= b'5');
            if round_up {
                increment_decimal_digits(&mut value);
            }
            value
        };

        if coefficient_digits.is_empty() {
            coefficient_digits.push('0');
        }
        if coefficient_digits.len() > usize::from(precision) {
            return Err(decimal_literal_overflow(literal, precision, scale));
        }
        let magnitude = coefficient_digits
            .parse::<i128>()
            .map_err(|_| decimal_literal_overflow(literal, precision, scale))?;
        let coefficient = if negative && magnitude != 0 {
            -magnitude
        } else {
            magnitude
        };
        Self::new(coefficient, precision, scale)
    }

    pub(crate) fn from_validated(coefficient: i128, precision: u8, scale: u8) -> Self {
        debug_assert!(validate_decimal_type(precision, scale).is_ok());
        debug_assert!(coefficient_fits_precision(coefficient, precision));
        Self {
            coefficient,
            precision,
            scale,
        }
    }

    #[must_use]
    pub fn coefficient(self) -> i128 {
        self.coefficient
    }

    #[must_use]
    pub fn precision(self) -> u8 {
        self.precision
    }

    #[must_use]
    pub fn scale(self) -> u8 {
        self.scale
    }
}

pub(crate) fn validate_decimal_type(precision: u8, scale: u8) -> Result<()> {
    if !(1..=MAX_DECIMAL128_PRECISION).contains(&precision) {
        return Err(Error::InvalidQuery(format!(
            "Decimal128 precision must be between 1 and {MAX_DECIMAL128_PRECISION}; found {precision}"
        )));
    }
    if scale > precision {
        return Err(Error::InvalidQuery(format!(
            "Decimal128 scale {scale} exceeds precision {precision}"
        )));
    }
    Ok(())
}

pub(crate) fn coefficient_fits_precision(coefficient: i128, precision: u8) -> bool {
    coefficient.unsigned_abs() < 10_u128.pow(u32::from(precision))
}

fn split_exponent<'a>(unsigned: &'a str, literal: &str) -> Result<(&'a str, i64)> {
    let Some(index) = unsigned.find(['e', 'E']) else {
        return Ok((unsigned, 0));
    };
    let mantissa = &unsigned[..index];
    let exponent = &unsigned[index + 1..];
    if exponent.is_empty() {
        return Err(invalid_decimal_literal(literal));
    }
    let exponent = exponent
        .parse::<i64>()
        .map_err(|_| invalid_decimal_literal(literal))?;
    Ok((mantissa, exponent))
}

fn increment_decimal_digits(digits: &mut String) {
    let mut bytes = digits.as_bytes().to_vec();
    for digit in bytes.iter_mut().rev() {
        if *digit < b'9' {
            *digit += 1;
            *digits = String::from_utf8(bytes).expect("decimal digits are ASCII");
            return;
        }
        *digit = b'0';
    }
    bytes.insert(0, b'1');
    *digits = String::from_utf8(bytes).expect("decimal digits are ASCII");
}

fn invalid_decimal_literal(literal: &str) -> Error {
    Error::InvalidQuery(format!("invalid Decimal128 literal '{literal}'"))
}

fn decimal_literal_overflow(literal: &str, precision: u8, scale: u8) -> Error {
    Error::NumericOverflow(format!(
        "Decimal128({precision}, {scale}) literal '{literal}'"
    ))
}

/// A scalar value read from or written to a typed column.
#[derive(Debug, Clone)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Decimal128(Decimal128),
    Bool(bool),
    String(String),
}

/// A non-owning scalar used while scanning immutable column storage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueRef<'a> {
    Int64(i64),
    Float64(f64),
    Decimal128(Decimal128),
    Bool(bool),
    String(&'a str),
}

impl Value {
    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Decimal128(value) => DataType::Decimal128 {
                precision: value.precision,
                scale: value.scale,
            },
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => format_float(*value),
            Self::Decimal128(value) => format_decimal(*value),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }

    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Int64(value) => ValueRef::Int64(*value),
            Self::Float64(value) => ValueRef::Float64(*value),
            Self::Decimal128(value) => ValueRef::Decimal128(*value),
            Self::Bool(value) => ValueRef::Bool(*value),
            Self::String(value) => ValueRef::String(value),
        }
    }

    #[cfg(test)]
    pub(crate) fn sql_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_ref().sql_cmp(other.as_ref())
    }
}

impl ValueRef<'_> {
    pub(crate) fn to_owned(self) -> Value {
        match self {
            Self::Int64(value) => Value::Int64(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Decimal128(value) => Value::Decimal128(value),
            Self::Bool(value) => Value::Bool(value),
            Self::String(value) => Value::String(value.to_owned()),
        }
    }

    pub(crate) fn sql_cmp(self, other: Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => Some(left.cmp(&right)),
            (Self::Float64(left), Self::Float64(right)) => left.partial_cmp(&right),
            (Self::Int64(left), Self::Float64(right)) => int_float_cmp(left, right),
            (Self::Float64(left), Self::Int64(right)) => {
                int_float_cmp(right, left).map(Ordering::reverse)
            }
            (Self::Decimal128(left), Self::Decimal128(right)) => Some(decimal_cmp(left, right)),
            (Self::Decimal128(left), Self::Int64(right)) => {
                Some(decimal_cmp(left, integer_decimal(right)))
            }
            (Self::Int64(left), Self::Decimal128(right)) => {
                Some(decimal_cmp(integer_decimal(left), right))
            }
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(&right)),
            (Self::String(left), Self::String(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }

    fn variant_index(&self) -> u8 {
        match self {
            Self::Int64(_) => 0,
            Self::Float64(_) => 1,
            Self::Decimal128(_) => 2,
            Self::Bool(_) => 3,
            Self::String(_) => 4,
        }
    }
}

fn integer_decimal(value: i64) -> Decimal128 {
    Decimal128 {
        coefficient: i128::from(value),
        precision: 19,
        scale: 0,
    }
}

fn decimal_cmp(left: Decimal128, right: Decimal128) -> Ordering {
    match (left.coefficient.signum(), right.coefficient.signum()) {
        (left_sign, right_sign) if left_sign != right_sign => left_sign.cmp(&right_sign),
        (0, 0) => Ordering::Equal,
        (sign, _) => {
            let magnitude = decimal_magnitude_cmp(left, right);
            if sign < 0 {
                magnitude.reverse()
            } else {
                magnitude
            }
        }
    }
}

fn decimal_magnitude_cmp(left: Decimal128, right: Decimal128) -> Ordering {
    let left_scale_factor = 10_u128.pow(u32::from(left.scale));
    let right_scale_factor = 10_u128.pow(u32::from(right.scale));
    let left_magnitude = left.coefficient.unsigned_abs();
    let right_magnitude = right.coefficient.unsigned_abs();
    let integer_cmp =
        (left_magnitude / left_scale_factor).cmp(&(right_magnitude / right_scale_factor));
    if integer_cmp != Ordering::Equal {
        return integer_cmp;
    }

    let common_scale = left.scale.max(right.scale);
    let left_fraction =
        (left_magnitude % left_scale_factor) * 10_u128.pow(u32::from(common_scale - left.scale));
    let right_fraction =
        (right_magnitude % right_scale_factor) * 10_u128.pow(u32::from(common_scale - right.scale));
    left_fraction.cmp(&right_fraction)
}

fn format_decimal(value: Decimal128) -> String {
    let mut digits = value.coefficient.unsigned_abs().to_string();
    if value.scale > 0 {
        let minimum = usize::from(value.scale) + 1;
        if digits.len() < minimum {
            digits.insert_str(0, &"0".repeat(minimum - digits.len()));
        }
        digits.insert(digits.len() - usize::from(value.scale), '.');
    }
    if value.coefficient < 0 {
        digits.insert(0, '-');
    }
    digits
}

fn int_float_cmp(integer: i64, float: f64) -> Option<Ordering> {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

    if float.is_nan() {
        return None;
    }
    if float >= I64_UPPER_EXCLUSIVE {
        return Some(Ordering::Less);
    }
    if float < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }

    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => (truncated as f64).partial_cmp(&float),
        ordering => Some(ordering),
    }
}

fn float_cmp(left: f64, right: f64) -> Ordering {
    if left == right {
        Ordering::Equal
    } else {
        left.total_cmp(&right)
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

fn format_float(value: f64) -> String {
    let rendered = value.to_string();
    if value.is_finite() && !rendered.contains(['.', 'e', 'E']) {
        format!("{rendered}.0")
    } else {
        rendered
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_display_string())
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_ref().cmp(&other.as_ref())
    }
}

impl PartialEq for ValueRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ValueRef<'_> {}

impl PartialOrd for ValueRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ValueRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::Float64(left), Self::Float64(right)) => float_cmp(*left, *right),
            (Self::Decimal128(left), Self::Decimal128(right)) => decimal_cmp(*left, *right),
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (Self::String(left), Self::String(right)) => left.cmp(right),
            _ => self.variant_index().cmp(&other.variant_index()),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl Hash for ValueRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.variant_index().hash(state);
        match self {
            Self::Int64(value) => value.hash(state),
            Self::Float64(value) => canonical_float_bits(*value).hash(state),
            Self::Decimal128(value) => {
                let mut coefficient = value.coefficient;
                let mut scale = value.scale;
                while scale > 0 && coefficient % 10 == 0 {
                    coefficient /= 10;
                    scale -= 1;
                }
                coefficient.hash(state);
                scale.hash(state);
            }
            Self::Bool(value) => value.hash(state),
            Self::String(value) => value.hash(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_integral_floats_unambiguously() {
        assert_eq!(Value::Float64(2.0).as_display_string(), "2.0");
        assert_eq!(Value::Float64(2.5).as_display_string(), "2.5");
    }

    #[test]
    fn compares_mixed_numbers_without_losing_integer_precision() {
        let beyond_exact_f64 = Value::Int64(9_007_199_254_740_993);
        let rounded_float = Value::Float64(9_007_199_254_740_992.0);
        assert_eq!(
            beyond_exact_f64.sql_cmp(&rounded_float),
            Some(Ordering::Greater)
        );
        assert_eq!(
            rounded_float.sql_cmp(&beyond_exact_f64),
            Some(Ordering::Less)
        );

        assert_eq!(
            Value::Int64(i64::MAX).sql_cmp(&Value::Float64(9_223_372_036_854_775_808.0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Int64(i64::MAX).sql_cmp(&Value::Float64(9_223_372_036_854_774_784.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Value::Int64(i64::MIN).sql_cmp(&Value::Float64(-9_223_372_036_854_775_808.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Value::Int64(-1).sql_cmp(&Value::Float64(-1.5)),
            Some(Ordering::Greater)
        );
    }
}
