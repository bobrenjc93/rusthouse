use std::cmp::Ordering;

pub(super) fn abs(value: f64) -> f64 {
    debug_assert!(value.is_finite(), "Float64 scalar input is finite");
    value.abs()
}

pub(super) fn abs_cmp(left: f64, right: f64) -> Ordering {
    finite_cmp(abs(left), abs(right))
}

pub(super) fn round(value: f64) -> f64 {
    debug_assert!(value.is_finite(), "Float64 scalar input is finite");
    value.round()
}

pub(super) fn round_cmp(left: f64, right: f64) -> Ordering {
    finite_cmp(round(left), round(right))
}

pub(super) fn floor(value: f64) -> f64 {
    debug_assert!(value.is_finite(), "Float64 scalar input is finite");
    value.floor()
}

pub(super) fn floor_cmp(left: f64, right: f64) -> Ordering {
    finite_cmp(floor(left), floor(right))
}

pub(super) fn ceil(value: f64) -> f64 {
    debug_assert!(value.is_finite(), "Float64 scalar input is finite");
    value.ceil()
}

pub(super) fn ceil_cmp(left: f64, right: f64) -> Ordering {
    finite_cmp(ceil(left), ceil(right))
}

fn finite_cmp(left: f64, right: f64) -> Ordering {
    debug_assert!(left.is_finite(), "Float64 scalar result is finite");
    debug_assert!(right.is_finite(), "Float64 scalar result is finite");
    if left == right {
        Ordering::Equal
    } else {
        left.total_cmp(&right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Transform = fn(f64) -> f64;
    type Compare = fn(f64, f64) -> Ordering;

    #[test]
    fn abs_preserves_finite_extrema_and_canonicalizes_signed_zero() {
        let smallest = f64::from_bits(1);

        assert_eq!(abs(-f64::MAX), f64::MAX);
        assert_eq!(abs(f64::MAX), f64::MAX);
        assert_eq!(abs(-smallest), smallest);
        assert_eq!(abs(-0.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(abs(0.0).to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn round_covers_halfway_subnormal_and_signed_zero_boundaries() {
        let smallest = f64::from_bits(1);

        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-smallest).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(round(smallest).to_bits(), 0.0_f64.to_bits());
        assert_eq!(round(-0.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(round(f64::MAX), f64::MAX);
    }

    #[test]
    fn floor_and_ceil_cover_subnormal_extrema_and_signed_zero() {
        let smallest = f64::from_bits(1);

        assert_eq!(floor(-smallest), -1.0);
        assert_eq!(floor(smallest).to_bits(), 0.0_f64.to_bits());
        assert_eq!(floor(-0.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(floor(f64::MAX), f64::MAX);

        assert_eq!(ceil(-smallest).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(ceil(smallest), 1.0);
        assert_eq!(ceil(0.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(ceil(-f64::MAX), -f64::MAX);
    }

    #[test]
    fn comparisons_match_values_and_leave_transform_ties_equal() {
        let cases = [
            -f64::MAX,
            -2.5,
            -1.1,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            1.1,
            2.5,
            f64::MAX,
        ];
        let transforms: [(Transform, Compare); 4] = [
            (abs, abs_cmp),
            (round, round_cmp),
            (floor, floor_cmp),
            (ceil, ceil_cmp),
        ];

        for (transform, compare) in transforms {
            for left in cases {
                for right in cases {
                    let expected = transform(left)
                        .partial_cmp(&transform(right))
                        .expect("finite transformed values are ordered");
                    assert_eq!(compare(left, right), expected, "{left} and {right}");
                }
            }
        }

        assert_eq!(abs_cmp(-7.0, 7.0), Ordering::Equal);
        assert_eq!(round_cmp(-0.49, 0.49), Ordering::Equal);
        assert_eq!(floor_cmp(1.1, 1.9), Ordering::Equal);
        assert_eq!(ceil_cmp(-1.9, -1.1), Ordering::Equal);
    }
}
