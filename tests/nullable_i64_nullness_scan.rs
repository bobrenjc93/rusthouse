use rusthouse::{NullPredicate, ScanError, ScanLimits, scan_nullable_i64_nullness};

fn scan(
    values: &[Option<i64>],
    predicate: NullPredicate,
    max_result_rows: usize,
) -> Result<Vec<usize>, ScanError> {
    scan_nullable_i64_nullness(
        values,
        predicate,
        ScanLimits::new(values.len(), max_result_rows),
    )
}

#[test]
fn empty_input_has_no_null_or_present_rows() {
    for predicate in [NullPredicate::IsNull, NullPredicate::IsNotNull] {
        assert_eq!(scan(&[], predicate, 0), Ok(vec![]));
    }
}

#[test]
fn mixed_input_returns_stable_source_indices_for_both_predicates() {
    let values = [None, Some(8), None, Some(-3), Some(8), None];

    assert_eq!(scan(&values, NullPredicate::IsNull, 3), Ok(vec![0, 2, 5]));
    assert_eq!(
        scan(&values, NullPredicate::IsNotNull, 3),
        Ok(vec![1, 3, 4])
    );
}

#[test]
fn all_null_input_partitions_correctly() {
    let values = [None, None, None];

    assert_eq!(scan(&values, NullPredicate::IsNull, 3), Ok(vec![0, 1, 2]));
    assert_eq!(scan(&values, NullPredicate::IsNotNull, 0), Ok(vec![]));
}

#[test]
fn all_present_input_partitions_correctly() {
    let values = [Some(i64::MIN), Some(0), Some(i64::MAX)];

    assert_eq!(scan(&values, NullPredicate::IsNull, 0), Ok(vec![]));
    assert_eq!(
        scan(&values, NullPredicate::IsNotNull, 3),
        Ok(vec![0, 1, 2])
    );
}

#[test]
fn zero_limits_accept_empty_work_and_reject_input_rows_or_matches() {
    for predicate in [NullPredicate::IsNull, NullPredicate::IsNotNull] {
        assert_eq!(
            scan_nullable_i64_nullness(&[None], predicate, ScanLimits::new(0, 0)),
            Err(ScanError::InputLimitExceeded {
                rows: 1,
                max_rows: 0,
            })
        );
    }

    assert_eq!(scan(&[Some(1)], NullPredicate::IsNull, 0), Ok(vec![]));
    assert_eq!(scan(&[None], NullPredicate::IsNotNull, 0), Ok(vec![]));

    for (values, predicate) in [
        (&[None][..], NullPredicate::IsNull),
        (&[Some(1)][..], NullPredicate::IsNotNull),
    ] {
        assert_eq!(
            scan(values, predicate, 0),
            Err(ScanError::ResultLimitExceeded {
                rows: 1,
                max_rows: 0,
            })
        );
    }
}

#[test]
fn exact_input_and_result_limits_are_accepted_for_both_predicates() {
    let values = [None, Some(1), None, Some(2)];

    assert_eq!(scan(&values, NullPredicate::IsNull, 2), Ok(vec![0, 2]));
    assert_eq!(scan(&values, NullPredicate::IsNotNull, 2), Ok(vec![1, 3]));
}

#[test]
fn exceeded_limits_return_typed_errors_instead_of_partial_results() {
    let values = [None, Some(1), None, Some(2)];

    for predicate in [NullPredicate::IsNull, NullPredicate::IsNotNull] {
        assert_eq!(
            scan_nullable_i64_nullness(&values, predicate, ScanLimits::new(3, 4)),
            Err(ScanError::InputLimitExceeded {
                rows: 4,
                max_rows: 3,
            })
        );
        assert_eq!(
            scan_nullable_i64_nullness(&values, predicate, ScanLimits::new(4, 1)),
            Err(ScanError::ResultLimitExceeded {
                rows: 2,
                max_rows: 1,
            })
        );
    }
}
