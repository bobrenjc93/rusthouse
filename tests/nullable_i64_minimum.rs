use rusthouse::{AggregateError, AggregateLimits, RowSelection, min_nullable_i64};

#[test]
fn empty_and_all_null_inputs_return_sql_null() {
    assert_eq!(
        min_nullable_i64(&[], RowSelection::All, AggregateLimits::new(0, 0)),
        Ok(None)
    );

    let values = [None, None, None];
    assert_eq!(
        min_nullable_i64(
            &values,
            RowSelection::All,
            AggregateLimits::new(values.len(), values.len()),
        ),
        Ok(None)
    );
}

#[test]
fn ignores_nulls_and_preserves_duplicate_int64_extremes() {
    let values = [Some(i64::MAX), None, Some(i64::MIN), Some(i64::MIN), None];

    assert_eq!(
        min_nullable_i64(
            &values,
            RowSelection::All,
            AggregateLimits::new(values.len(), values.len()),
        ),
        Ok(Some(i64::MIN))
    );
}

#[test]
fn computes_only_over_the_explicit_selection() {
    let values = [Some(i64::MIN), Some(7), None, Some(3), Some(i64::MAX)];
    let rows = [1, 2, 3, 4];

    assert_eq!(
        min_nullable_i64(
            &values,
            RowSelection::Indices(&rows),
            AggregateLimits::new(values.len(), rows.len()),
        ),
        Ok(Some(3))
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_input_limits() {
    let values = [Some(2), None, Some(1)];

    assert_eq!(
        min_nullable_i64(&values, RowSelection::All, AggregateLimits::new(3, 3)),
        Ok(Some(1))
    );
    assert_eq!(
        min_nullable_i64(&values, RowSelection::All, AggregateLimits::new(2, 3)),
        Err(AggregateError::InputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        min_nullable_i64(&values, RowSelection::All, AggregateLimits::new(3, 2)),
        Err(AggregateError::SelectionLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
}

#[test]
fn preserves_explicit_selection_validation() {
    let values = [Some(1), Some(2), Some(3)];

    assert!(matches!(
        min_nullable_i64(
            &values,
            RowSelection::Indices(&[1, 1]),
            AggregateLimits::new(3, 2),
        ),
        Err(AggregateError::SelectionNotStrictlyIncreasing { .. })
    ));
    assert_eq!(
        min_nullable_i64(
            &values,
            RowSelection::Indices(&[0, 3]),
            AggregateLimits::new(3, 2),
        ),
        Err(AggregateError::SelectionIndexOutOfBounds {
            selection_position: 1,
            row_index: 3,
            input_rows: 3,
        })
    );
}
