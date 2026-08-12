use std::cmp::Ordering;

use super::error::{Error, Result};
use super::value::{DataType, Value, ValueRef};

pub(super) fn checked_abs(value: ValueRef<'_>) -> Result<Value> {
    match value {
        ValueRef::Null(DataType::Int64) => Ok(Value::Null(DataType::Int64)),
        ValueRef::Int64(value) => value
            .checked_abs()
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("ABS(Int64)".to_owned())),
        ValueRef::Null(_) | ValueRef::Float64(_) | ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("ABS(Int64) input type is resolved")
        }
    }
}

pub(super) fn abs_cmp(left: ValueRef<'_>, right: ValueRef<'_>) -> Ordering {
    match (left, right) {
        (ValueRef::Null(DataType::Int64), ValueRef::Null(DataType::Int64)) => Ordering::Equal,
        (ValueRef::Null(DataType::Int64), ValueRef::Int64(_)) => Ordering::Less,
        (ValueRef::Int64(_), ValueRef::Null(DataType::Int64)) => Ordering::Greater,
        (ValueRef::Int64(left), ValueRef::Int64(right)) => {
            left.unsigned_abs().cmp(&right.unsigned_abs())
        }
        _ => unreachable!("ABS(Int64) input type is resolved"),
    }
}

pub(super) fn checked_subtract(value: ValueRef<'_>, literal: i64) -> Result<Value> {
    match value {
        ValueRef::Null(DataType::Int64) => Ok(Value::Null(DataType::Int64)),
        ValueRef::Int64(value) => value
            .checked_sub(literal)
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("Int64 subtraction".to_owned())),
        ValueRef::Null(_) | ValueRef::Float64(_) | ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("Int64 subtraction input type is resolved")
        }
    }
}

pub(super) fn if_null(value: ValueRef<'_>, fallback: i64) -> i64 {
    match value {
        ValueRef::Null(DataType::Int64) => fallback,
        ValueRef::Int64(value) => value,
        ValueRef::Null(_) | ValueRef::Float64(_) | ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("ifNull first argument is resolved as Nullable(Int64)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_propagates_typed_null_and_reports_only_minimum_overflow() {
        assert_eq!(
            checked_abs(ValueRef::Null(DataType::Int64)),
            Ok(Value::Null(DataType::Int64))
        );
        assert_eq!(checked_abs(ValueRef::Int64(-7)), Ok(Value::Int64(7)));
        assert_eq!(
            checked_abs(ValueRef::Int64(i64::MAX)),
            Ok(Value::Int64(i64::MAX))
        );
        assert_eq!(
            checked_abs(ValueRef::Int64(i64::MIN)),
            Err(Error::NumericOverflow("ABS(Int64)".to_owned()))
        );
    }

    #[test]
    fn abs_ordering_is_null_first_and_compares_unsigned_magnitudes() {
        let cases = [
            ValueRef::Null(DataType::Int64),
            ValueRef::Int64(0),
            ValueRef::Int64(-1),
            ValueRef::Int64(1),
            ValueRef::Int64(-7),
            ValueRef::Int64(7),
            ValueRef::Int64(i64::MAX),
            ValueRef::Int64(i64::MIN),
        ];

        for left in cases {
            for right in cases {
                let expected = match (left, right) {
                    (ValueRef::Null(DataType::Int64), ValueRef::Null(DataType::Int64)) => {
                        Ordering::Equal
                    }
                    (ValueRef::Null(DataType::Int64), ValueRef::Int64(_)) => Ordering::Less,
                    (ValueRef::Int64(_), ValueRef::Null(DataType::Int64)) => Ordering::Greater,
                    (ValueRef::Int64(left), ValueRef::Int64(right)) => {
                        i128::from(left).abs().cmp(&i128::from(right).abs())
                    }
                    _ => unreachable!("test cases are nullable Int64 values"),
                };
                assert_eq!(abs_cmp(left, right), expected, "{left:?} and {right:?}");
            }
        }
    }

    #[test]
    fn literal_subtraction_propagates_typed_null_and_checks_both_bounds() {
        assert_eq!(
            checked_subtract(ValueRef::Null(DataType::Int64), i64::MIN),
            Ok(Value::Null(DataType::Int64))
        );
        assert_eq!(
            checked_subtract(ValueRef::Int64(i64::MIN), i64::MIN),
            Ok(Value::Int64(0))
        );
        assert_eq!(
            checked_subtract(ValueRef::Int64(i64::MAX), i64::MAX),
            Ok(Value::Int64(0))
        );
        for (value, literal) in [(i64::MIN, 1), (i64::MAX, -1)] {
            assert_eq!(
                checked_subtract(ValueRef::Int64(value), literal),
                Err(Error::NumericOverflow("Int64 subtraction".to_owned()))
            );
        }
    }

    #[test]
    fn if_null_replaces_only_typed_nulls() {
        assert_eq!(if_null(ValueRef::Null(DataType::Int64), i64::MIN), i64::MIN);
        assert_eq!(if_null(ValueRef::Int64(i64::MAX), i64::MIN), i64::MAX);
    }
}
