use rusthouse::{JoinError, JoinLimits, LeftOuterJoinRowPair, left_outer_equi_join_nullable_i64};

fn pairs(rows: Vec<LeftOuterJoinRowPair>) -> Vec<(usize, Option<usize>)> {
    rows.into_iter()
        .map(LeftOuterJoinRowPair::into_pair)
        .collect()
}

#[test]
fn empty_left_input_has_no_rows_and_empty_right_input_preserves_every_left_row() {
    assert_eq!(
        left_outer_equi_join_nullable_i64(&[], &[], JoinLimits::new(0, 0)),
        Ok(vec![])
    );
    assert_eq!(
        left_outer_equi_join_nullable_i64(&[], &[Some(1)], JoinLimits::new(1, 0)),
        Ok(vec![])
    );
    assert_eq!(
        pairs(
            left_outer_equi_join_nullable_i64(&[Some(1), None], &[], JoinLimits::new(2, 2),)
                .unwrap()
        ),
        vec![(0, None), (1, None)]
    );
}

#[test]
fn nulls_duplicates_and_unmatched_rows_follow_left_major_source_order() {
    let left = [Some(2), None, Some(1), Some(2), Some(9)];
    let right = [Some(1), Some(2), None, Some(2)];

    assert_eq!(
        pairs(left_outer_equi_join_nullable_i64(&left, &right, JoinLimits::new(5, 7)).unwrap()),
        vec![
            (0, Some(1)),
            (0, Some(3)),
            (1, None),
            (2, Some(0)),
            (3, Some(1)),
            (3, Some(3)),
            (4, None),
        ]
    );
}

#[test]
fn accepts_exact_input_and_complete_output_bounds() {
    let left = [Some(7), Some(7), None];
    let right = [Some(7), None, Some(7)];

    assert_eq!(
        pairs(left_outer_equi_join_nullable_i64(&left, &right, JoinLimits::new(3, 5)).unwrap()),
        vec![
            (0, Some(0)),
            (0, Some(2)),
            (1, Some(0)),
            (1, Some(2)),
            (2, None),
        ]
    );
}

#[test]
fn rejects_each_input_above_the_shared_row_bound() {
    let three = [Some(1), Some(2), None];
    let two = [Some(1), Some(2)];

    assert_eq!(
        left_outer_equi_join_nullable_i64(&three, &two, JoinLimits::new(2, usize::MAX)),
        Err(JoinError::LeftInputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        left_outer_equi_join_nullable_i64(&two, &three, JoinLimits::new(2, usize::MAX)),
        Err(JoinError::RightInputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
}

#[test]
fn rejects_complete_output_above_the_bound_without_a_partial_result() {
    let left = [Some(4), Some(4), None];
    let right = [Some(4), Some(4)];

    assert_eq!(
        left_outer_equi_join_nullable_i64(&left, &right, JoinLimits::new(3, 4)),
        Err(JoinError::OutputLimitExceeded {
            pairs: 5,
            max_pairs: 4,
        })
    );
}

#[test]
fn output_bound_counts_unmatched_and_null_left_rows() {
    assert_eq!(
        left_outer_equi_join_nullable_i64(&[Some(9), None], &[Some(1)], JoinLimits::new(2, 1),),
        Err(JoinError::OutputLimitExceeded {
            pairs: 2,
            max_pairs: 1,
        })
    );
    assert_eq!(
        left_outer_equi_join_nullable_i64(&[None], &[None], JoinLimits::new(1, 0)),
        Err(JoinError::OutputLimitExceeded {
            pairs: 1,
            max_pairs: 0,
        })
    );
}
