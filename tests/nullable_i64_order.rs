use rusthouse::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};
use std::cmp::Ordering;

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

#[test]
fn matches_full_sort_across_deterministic_corpora_and_boundary_limits() {
    let directions = [OrderDirection::Asc, OrderDirection::Desc];
    let null_orders = [NullOrder::First, NullOrder::Last];

    for values in deterministic_corpora() {
        let mut boundary_limits = vec![
            0,
            1,
            values.len() / 2,
            values.len().saturating_sub(1),
            values.len(),
            values.len().saturating_add(1),
        ];
        boundary_limits.sort_unstable();
        boundary_limits.dedup();

        for direction in directions {
            for null_order in null_orders {
                for &limit in &boundary_limits {
                    let expected = full_sort_reference(&values, direction, null_order, limit);
                    let actual = order_nullable_i64(
                        &values,
                        direction,
                        null_order,
                        limit,
                        limits(values.len(), limit),
                    )
                    .unwrap();

                    assert_eq!(
                        actual, expected,
                        "values {values:?}, direction {direction:?}, null order {null_order:?}, limit {limit}"
                    );
                }
            }
        }
    }
}

fn deterministic_corpora() -> Vec<Vec<Option<i64>>> {
    let mut generated = Vec::with_capacity(257);
    let choices = [
        None,
        Some(i64::MIN),
        Some(-7),
        Some(0),
        Some(7),
        Some(i64::MAX),
    ];
    let mut state = 0x4d59_5df4_d0f3_3173_u64;

    for _ in 0..257 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        generated.push(choices[(state as usize) % choices.len()]);
    }

    vec![
        vec![],
        vec![None],
        vec![Some(0)],
        vec![None, None, None, None],
        vec![Some(5), Some(5), Some(5), Some(5)],
        vec![
            Some(i64::MAX),
            None,
            Some(i64::MIN),
            Some(0),
            None,
            Some(i64::MAX),
            Some(i64::MIN),
        ],
        vec![
            Some(2),
            Some(1),
            Some(2),
            None,
            Some(1),
            None,
            Some(2),
            Some(1),
        ],
        generated,
    ]
}

fn full_sort_reference(
    values: &[Option<i64>],
    direction: OrderDirection,
    null_order: NullOrder,
    limit: usize,
) -> Vec<usize> {
    let mut rows: Vec<_> = (0..values.len()).collect();
    rows.sort_unstable_by(|&left, &right| {
        compare_reference(values[left], values[right], direction, null_order)
            .then_with(|| left.cmp(&right))
    });
    rows.truncate(limit);
    rows
}

fn compare_reference(
    left: Option<i64>,
    right: Option<i64>,
    direction: OrderDirection,
    null_order: NullOrder,
) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => match null_order {
            NullOrder::First => Ordering::Less,
            NullOrder::Last => Ordering::Greater,
        },
        (Some(_), None) => match null_order {
            NullOrder::First => Ordering::Greater,
            NullOrder::Last => Ordering::Less,
        },
        (Some(left), Some(right)) => match direction {
            OrderDirection::Asc => left.cmp(&right),
            OrderDirection::Desc => right.cmp(&left),
        },
    }
}
