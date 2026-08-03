use rusthouse::{
    AggregateError, AggregateFunction, Int64AggregateResult, aggregate_int64, avg_int64,
    count_int64, max_int64, min_int64, sum_int64,
};

#[test]
fn empty_and_all_null_inputs_follow_sql_null_semantics() {
    for values in [&[][..], &[None, None, None][..]] {
        assert_eq!(
            aggregate_int64(values),
            Ok(Int64AggregateResult {
                count: 0,
                sum: None,
                min: None,
                max: None,
                avg: None,
            })
        );
        assert_eq!(count_int64(values), 0);
        assert_eq!(sum_int64(values), Ok(None));
        assert_eq!(min_int64(values), None);
        assert_eq!(max_int64(values), None);
        assert_eq!(avg_int64(values), Ok(None));
    }
}

#[test]
fn mixed_sign_values_ignore_nulls() {
    let values = [None, Some(8), Some(-3), None, Some(5), Some(-10)];
    let expected = Int64AggregateResult {
        count: 4,
        sum: Some(0),
        min: Some(-10),
        max: Some(8),
        avg: Some(0.0),
    };

    assert_eq!(aggregate_int64(&values), Ok(expected));
    assert_eq!(count_int64(&values), expected.count);
    assert_eq!(sum_int64(&values), Ok(expected.sum));
    assert_eq!(min_int64(&values), expected.min);
    assert_eq!(max_int64(&values), expected.max);
    assert_eq!(avg_int64(&values), Ok(expected.avg));
}

#[test]
fn wide_accumulation_is_not_sensitive_to_transient_int64_overflow() {
    let values = [Some(i64::MAX), Some(1), Some(-1), Some(i64::MIN)];

    assert_eq!(sum_int64(&values), Ok(Some(-1)));
    assert_eq!(avg_int64(&values), Ok(Some(-0.25)));
    assert_eq!(min_int64(&values), Some(i64::MIN));
    assert_eq!(max_int64(&values), Some(i64::MAX));
}

#[test]
fn sum_reports_positive_and_negative_overflow_with_a_typed_error() {
    let expected = AggregateError::IntegerOverflow {
        function: AggregateFunction::Sum,
    };

    for values in [
        [Some(i64::MAX), None, Some(1)],
        [Some(i64::MIN), None, Some(-1)],
    ] {
        assert_eq!(sum_int64(&values), Err(expected));
        assert_eq!(aggregate_int64(&values), Err(expected));
    }

    assert_eq!(
        expected.to_string(),
        "SUM overflowed while aggregating Int64 values"
    );
}

#[test]
fn average_uses_the_wide_sum_when_int64_sum_overflows() {
    let values = [Some(i64::MAX), None, Some(i64::MAX)];

    assert!(matches!(
        sum_int64(&values),
        Err(AggregateError::IntegerOverflow {
            function: AggregateFunction::Sum
        })
    ));
    assert_eq!(avg_int64(&values), Ok(Some(i64::MAX as f64)));
}

#[test]
fn kernels_match_a_deterministic_reference_model() {
    let mut state = 0x4d59_5df4_d0f3_3173_u64;

    for length in 0..=256 {
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if state % 5 == 0 {
                values.push(None);
            } else {
                values.push(Some(((state >> 16) % 2_000_001) as i64 - 1_000_000));
            }
        }

        let expected = reference_aggregate(&values);
        assert_eq!(aggregate_int64(&values), Ok(expected));
        assert_eq!(count_int64(&values), expected.count);
        assert_eq!(sum_int64(&values), Ok(expected.sum));
        assert_eq!(min_int64(&values), expected.min);
        assert_eq!(max_int64(&values), expected.max);
        assert_eq!(avg_int64(&values), Ok(expected.avg));
    }
}

fn reference_aggregate(values: &[Option<i64>]) -> Int64AggregateResult {
    let mut count = 0;
    let mut sum = 0_i128;
    let mut min = None;
    let mut max = None;

    for value in values.iter().copied().flatten() {
        count += 1;
        sum += i128::from(value);
        min = Some(min.map_or(value, |current: i64| current.min(value)));
        max = Some(max.map_or(value, |current: i64| current.max(value)));
    }

    Int64AggregateResult {
        count,
        sum: (count != 0).then(|| i64::try_from(sum).unwrap()),
        min,
        max,
        avg: (count != 0).then(|| sum as f64 / count as f64),
    }
}
