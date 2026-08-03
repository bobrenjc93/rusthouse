use rusthouse::{
    AggregateError, AggregateLimits, ComparisonOperator, RowSelection, ScanLimits,
    aggregate_nullable_i64, scan_nullable_i64,
};

fn limits(input_rows: usize, selected_rows: usize) -> AggregateLimits {
    AggregateLimits::new(input_rows, selected_rows)
}

#[test]
fn empty_input_has_sql_count_and_sum_semantics() {
    let result = aggregate_nullable_i64(&[], RowSelection::All, limits(0, 0)).unwrap();

    assert_eq!(result.count_star(), 0);
    assert_eq!(result.count_column(), 0);
    assert_eq!(result.sum(), None);
}

#[test]
fn all_null_input_counts_rows_but_has_a_null_sum() {
    let values = [None, None, None];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::All,
        limits(values.len(), values.len()),
    )
    .unwrap();

    assert_eq!(result.count_star(), 3);
    assert_eq!(result.count_column(), 0);
    assert_eq!(result.sum(), None);
}

#[test]
fn aggregates_all_rows_with_sql_null_semantics() {
    let values = [Some(7), None, Some(-2), Some(0)];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::All,
        limits(values.len(), values.len()),
    )
    .unwrap();

    assert_eq!(result.count_star(), 4);
    assert_eq!(result.count_column(), 3);
    assert_eq!(result.sum(), Some(5));
}

#[test]
fn consumes_ordered_row_indices_returned_by_a_scan() {
    let values = [Some(4), None, Some(9), Some(4), Some(-3)];
    let rows = scan_nullable_i64(
        &values,
        ComparisonOperator::Le,
        4,
        ScanLimits::new(values.len(), values.len()),
    )
    .unwrap();

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::Indices(&rows),
        limits(values.len(), rows.len()),
    )
    .unwrap();

    assert_eq!(rows, vec![0, 3, 4]);
    assert_eq!(result.count_star(), 3);
    assert_eq!(result.count_column(), 3);
    assert_eq!(result.sum(), Some(5));
}

#[test]
fn a_selection_can_contain_only_null_rows() {
    let values = [Some(1), None, Some(3), None];
    let rows = [1, 3];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::Indices(&rows),
        limits(values.len(), rows.len()),
    )
    .unwrap();

    assert_eq!(result.count_star(), 2);
    assert_eq!(result.count_column(), 0);
    assert_eq!(result.sum(), None);
}

#[test]
fn int64_boundaries_sum_exactly() {
    let values = [Some(i64::MIN), Some(i64::MAX), Some(1)];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::All,
        limits(values.len(), values.len()),
    )
    .unwrap();

    assert_eq!(result.count_column(), 3);
    assert_eq!(result.sum(), Some(0));
}

#[test]
fn accepts_input_and_selection_exactly_at_the_limits() {
    let values = [Some(1), None, Some(2)];
    let rows = [0, 2];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::Indices(&rows),
        AggregateLimits::new(3, 2),
    )
    .unwrap();

    assert_eq!(result.count_star(), 2);
    assert_eq!(result.sum(), Some(3));
}

#[test]
fn rejects_input_and_effective_selection_above_their_limits() {
    let values = [Some(1), None, Some(2)];

    assert_eq!(
        aggregate_nullable_i64(&values, RowSelection::All, AggregateLimits::new(2, 3)),
        Err(AggregateError::InputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        aggregate_nullable_i64(&values, RowSelection::All, AggregateLimits::new(3, 2)),
        Err(AggregateError::SelectionLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );

    let rows = [0, 2];
    assert_eq!(
        aggregate_nullable_i64(
            &values,
            RowSelection::Indices(&rows),
            AggregateLimits::new(3, 1),
        ),
        Err(AggregateError::SelectionLimitExceeded {
            rows: 2,
            max_rows: 1,
        })
    );
}

#[test]
fn rejects_out_of_bounds_selection_indices() {
    let values = [Some(1), Some(2)];
    let rows = [0, 2];

    assert_eq!(
        aggregate_nullable_i64(
            &values,
            RowSelection::Indices(&rows),
            limits(values.len(), rows.len()),
        ),
        Err(AggregateError::SelectionIndexOutOfBounds {
            selection_position: 1,
            row_index: 2,
            input_rows: 2,
        })
    );
}

#[test]
fn rejects_unsorted_or_duplicate_selection_indices() {
    let values = [Some(1), Some(2), Some(3)];

    for rows in [[0, 0], [2, 1]] {
        assert!(matches!(
            aggregate_nullable_i64(
                &values,
                RowSelection::Indices(&rows),
                limits(values.len(), rows.len()),
            ),
            Err(AggregateError::SelectionNotStrictlyIncreasing { .. })
        ));
    }
}

#[test]
fn reports_typed_positive_and_negative_sum_overflow() {
    for (values, expected_sum) in [
        ([Some(i64::MAX), Some(1)], i128::from(i64::MAX) + 1),
        ([Some(i64::MIN), Some(-1)], i128::from(i64::MIN) - 1),
    ] {
        assert_eq!(
            aggregate_nullable_i64(&values, RowSelection::All, limits(2, 2)),
            Err(AggregateError::SumOverflow { sum: expected_sum })
        );
    }
}

#[test]
fn overflow_is_based_on_the_final_exact_sum() {
    let values = [Some(i64::MAX), Some(1), Some(-1)];

    let result = aggregate_nullable_i64(
        &values,
        RowSelection::All,
        limits(values.len(), values.len()),
    )
    .unwrap();

    assert_eq!(result.sum(), Some(i64::MAX));
}
