//! Pure transforms over the finite `Float64` values admitted by batch storage.
//!
//! This module owns value and ordering semantics only. The execution engine
//! remains responsible for type resolution, physical row access, stable tie
//! breaking, pagination, deferred projection, and result allocation.

use std::cmp::Ordering;

use super::value::ValueRef;

#[inline]
pub(super) fn abs(value: f64) -> f64 {
    transform(value, f64::abs)
}

#[inline]
pub(super) fn abs_cmp(left: f64, right: f64) -> Ordering {
    transformed_cmp(left, right, abs)
}

#[inline]
pub(super) fn round(value: f64) -> f64 {
    transform(value, f64::round)
}

#[inline]
pub(super) fn round_cmp(left: f64, right: f64) -> Ordering {
    transformed_cmp(left, right, round)
}

#[inline]
pub(super) fn floor(value: f64) -> f64 {
    transform(value, f64::floor)
}

#[inline]
pub(super) fn floor_cmp(left: f64, right: f64) -> Ordering {
    transformed_cmp(left, right, floor)
}

#[inline]
pub(super) fn ceil(value: f64) -> f64 {
    transform(value, f64::ceil)
}

#[inline]
pub(super) fn ceil_cmp(left: f64, right: f64) -> Ordering {
    transformed_cmp(left, right, ceil)
}

#[inline]
fn transform(value: f64, operation: impl FnOnce(f64) -> f64) -> f64 {
    debug_assert!(value.is_finite(), "stored Float64 values are finite");
    let transformed = operation(value);
    debug_assert!(
        transformed.is_finite(),
        "finite Float64 scalar transforms remain finite"
    );
    transformed
}

#[inline]
fn transformed_cmp(left: f64, right: f64, operation: impl Fn(f64) -> f64) -> Ordering {
    ValueRef::Float64(operation(left)).cmp(&ValueRef::Float64(operation(right)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_preserve_finite_extrema_and_integral_precision_boundary() {
        let cases = [
            -f64::MAX,
            -4_503_599_627_370_496.0,
            -f64::MIN_POSITIVE,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            4_503_599_627_370_496.0,
            f64::MAX,
        ];

        for value in cases {
            for operation in [abs, round, floor, ceil] {
                let transformed = operation(value);
                assert!(transformed.is_finite(), "{value:?}");
                assert_eq!(transformed, operation(transformed), "{value:?}");
            }
        }

        assert_eq!(abs(-f64::MAX), f64::MAX);
        for operation in [round, floor, ceil] {
            assert_eq!(operation(f64::MAX), f64::MAX);
            assert_eq!(operation(-f64::MAX), -f64::MAX);
            assert_eq!(operation(4_503_599_627_370_496.0), 4_503_599_627_370_496.0);
        }
    }

    #[test]
    fn halfway_and_subnormal_boundaries_have_the_expected_direction() {
        let negative_subnormal = -f64::from_bits(1);
        let positive_subnormal = f64::from_bits(1);

        assert_eq!(round(-0.5), -1.0);
        assert_eq!(round(f64::from_bits((-0.5_f64).to_bits() - 1)), -0.0);
        assert_eq!(round(f64::from_bits(0.5_f64.to_bits() - 1)), 0.0);
        assert_eq!(round(0.5), 1.0);

        assert_eq!(floor(negative_subnormal), -1.0);
        assert_eq!(floor(positive_subnormal), 0.0);
        assert_eq!(ceil(negative_subnormal), -0.0);
        assert_eq!(ceil(positive_subnormal), 1.0);
    }

    #[test]
    fn transforms_preserve_their_signed_zero_contracts() {
        assert_eq!(abs(-0.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(abs(0.0).to_bits(), 0.0_f64.to_bits());

        for operation in [round, floor, ceil] {
            assert_eq!(operation(-0.0).to_bits(), (-0.0_f64).to_bits());
            assert_eq!(operation(0.0).to_bits(), 0.0_f64.to_bits());
        }
        assert_eq!(round(-f64::from_bits(1)).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(ceil(-f64::from_bits(1)).to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn comparisons_match_transformed_values_and_leave_equal_results_tied() {
        let cases = [
            -f64::MAX,
            -2.5,
            -1.5,
            -0.5,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            0.5,
            1.5,
            2.5,
            f64::MAX,
        ];
        let operations = [
            (abs as fn(f64) -> f64, abs_cmp as fn(f64, f64) -> Ordering),
            (round, round_cmp),
            (floor, floor_cmp),
            (ceil, ceil_cmp),
        ];

        for (operation, comparison) in operations {
            for left in cases {
                for right in cases {
                    let expected = ValueRef::Float64(operation(left))
                        .cmp(&ValueRef::Float64(operation(right)));
                    assert_eq!(comparison(left, right), expected, "{left:?}, {right:?}");
                }
            }
        }

        assert_eq!(abs_cmp(-2.5, 2.5), Ordering::Equal);
        assert_eq!(round_cmp(-0.0, 0.0), Ordering::Equal);
        assert_eq!(floor_cmp(-0.0, 0.0), Ordering::Equal);
        assert_eq!(ceil_cmp(-f64::from_bits(1), -0.0), Ordering::Equal);
    }
}
