use rusthouse::{DistinctError, DistinctLimits, distinct_nullable_i64};

#[test]
fn empty_input_produces_no_values() {
    assert_eq!(
        distinct_nullable_i64(&[], DistinctLimits::new(0, 0)),
        Ok(vec![])
    );
}

#[test]
fn returns_duplicate_values_and_null_once() {
    let values = [Some(7), None, Some(-2), Some(7), None, Some(-2), Some(7)];

    let distinct = distinct_nullable_i64(&values, DistinctLimits::new(values.len(), 3)).unwrap();

    assert_eq!(distinct, [None, Some(-2), Some(7)]);
}

#[test]
fn orders_null_and_integer_extremes_deterministically() {
    let values = [
        Some(i64::MAX),
        Some(0),
        None,
        Some(i64::MIN),
        Some(i64::MAX),
        Some(-1),
    ];
    let reordered = [
        Some(-1),
        Some(i64::MAX),
        Some(i64::MIN),
        None,
        Some(0),
        Some(i64::MAX),
    ];
    let expected = vec![None, Some(i64::MIN), Some(-1), Some(0), Some(i64::MAX)];

    for input in [&values[..], &reordered[..]] {
        let distinct =
            distinct_nullable_i64(input, DistinctLimits::new(input.len(), expected.len())).unwrap();

        assert_eq!(distinct, expected);
    }
}

#[test]
fn accepts_input_rows_and_distinct_values_exactly_at_their_caps() {
    let values = [Some(2), None, Some(1), Some(2)];

    assert_eq!(
        distinct_nullable_i64(&values, DistinctLimits::new(4, 3)),
        Ok(vec![None, Some(1), Some(2)])
    );
}

#[test]
fn rejects_input_rows_and_distinct_values_above_their_caps() {
    let values = [Some(2), None, Some(1), Some(2)];

    assert_eq!(
        distinct_nullable_i64(&values, DistinctLimits::new(3, 3)),
        Err(DistinctError::InputLimitExceeded {
            rows: 4,
            max_rows: 3,
        })
    );
    assert_eq!(
        distinct_nullable_i64(&values, DistinctLimits::new(4, 2)),
        Err(DistinctError::DistinctValueLimitExceeded {
            values: 3,
            max_values: 2,
        })
    );
}

#[test]
fn zero_value_cap_accepts_empty_input_and_rejects_the_first_value() {
    assert_eq!(
        distinct_nullable_i64(&[], DistinctLimits::new(0, 0)),
        Ok(vec![])
    );
    assert_eq!(
        distinct_nullable_i64(&[None, None], DistinctLimits::new(2, 0)),
        Err(DistinctError::DistinctValueLimitExceeded {
            values: 1,
            max_values: 0,
        })
    );
}

#[test]
fn validates_the_input_cap_before_the_distinct_value_cap() {
    assert_eq!(
        distinct_nullable_i64(&[Some(1)], DistinctLimits::new(0, 0)),
        Err(DistinctError::InputLimitExceeded {
            rows: 1,
            max_rows: 0,
        })
    );
}
