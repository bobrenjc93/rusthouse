use rusthouse::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};

fn limits(input_rows: usize, limit: usize) -> OrderLimits {
    OrderLimits::new(input_rows, limit)
}

#[test]
fn supports_every_direction_and_explicit_null_placement() {
    let values = [
        Some(2),
        None,
        Some(i64::MIN),
        Some(2),
        Some(i64::MAX),
        None,
        Some(i64::MIN),
    ];
    let cases = [
        (
            OrderDirection::Asc,
            NullOrder::First,
            vec![1, 5, 2, 6, 0, 3, 4],
        ),
        (
            OrderDirection::Asc,
            NullOrder::Last,
            vec![2, 6, 0, 3, 4, 1, 5],
        ),
        (
            OrderDirection::Desc,
            NullOrder::First,
            vec![1, 5, 4, 0, 3, 2, 6],
        ),
        (
            OrderDirection::Desc,
            NullOrder::Last,
            vec![4, 0, 3, 2, 6, 1, 5],
        ),
    ];

    for (direction, null_order, expected) in cases {
        assert_eq!(
            order_nullable_i64(
                &values,
                direction,
                null_order,
                values.len(),
                limits(values.len(), values.len()),
            ),
            Ok(expected),
            "direction {direction:?}, null order {null_order:?}"
        );
    }
}

#[test]
fn preserves_source_order_for_duplicate_values_and_nulls() {
    let values = [Some(7), None, Some(7), None, Some(7)];

    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Asc,
            NullOrder::Last,
            values.len(),
            limits(values.len(), values.len()),
        ),
        Ok(vec![0, 2, 4, 1, 3])
    );
}

#[test]
fn truncates_the_ordered_rows_to_the_requested_limit() {
    let values = [Some(4), None, Some(-1), Some(9), Some(4)];

    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Desc,
            NullOrder::Last,
            3,
            limits(values.len(), 3),
        ),
        Ok(vec![3, 0, 4])
    );
}

#[test]
fn zero_limit_returns_no_rows_for_empty_or_nonempty_inputs() {
    for values in [&[][..], &[Some(1), None][..]] {
        assert_eq!(
            order_nullable_i64(
                values,
                OrderDirection::Asc,
                NullOrder::First,
                0,
                limits(values.len(), 0),
            ),
            Ok(vec![])
        );
    }
}

#[test]
fn a_limit_larger_than_the_input_returns_every_ordered_row() {
    let values = [None, Some(i64::MAX), Some(i64::MIN)];

    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Asc,
            NullOrder::Last,
            5,
            limits(values.len(), 5),
        ),
        Ok(vec![2, 1, 0])
    );
}

#[test]
fn accepts_input_and_requested_limit_exactly_at_their_bounds() {
    let values = [Some(3), None, Some(1)];

    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Asc,
            NullOrder::Last,
            2,
            OrderLimits::new(3, 2),
        ),
        Ok(vec![2, 0])
    );
}

#[test]
fn rejects_input_and_requested_limit_above_their_bounds() {
    let values = [Some(3), None, Some(1)];

    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Asc,
            NullOrder::Last,
            0,
            OrderLimits::new(2, 0),
        ),
        Err(OrderError::InputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        order_nullable_i64(
            &values,
            OrderDirection::Asc,
            NullOrder::Last,
            3,
            OrderLimits::new(3, 2),
        ),
        Err(OrderError::LimitExceeded {
            limit: 3,
            max_limit: 2,
        })
    );
}
