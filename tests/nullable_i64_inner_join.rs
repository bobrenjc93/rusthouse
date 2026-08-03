use rusthouse::{JoinError, JoinLimits, JoinRowPair, inner_equi_join_nullable_i64};

fn pairs(matches: Vec<JoinRowPair>) -> Vec<(usize, usize)> {
    matches.into_iter().map(JoinRowPair::into_pair).collect()
}

fn limits(input_rows: usize, output_pairs: usize) -> JoinLimits {
    JoinLimits::new(input_rows, output_pairs)
}

#[test]
fn empty_inputs_produce_no_matches() {
    let one = [Some(1)];

    for (left, right, input_bound) in [
        (&[][..], &[][..], 0),
        (&[][..], &one[..], 1),
        (&one[..], &[][..], 1),
    ] {
        assert_eq!(
            inner_equi_join_nullable_i64(left, right, limits(input_bound, 0)),
            Ok(vec![])
        );
    }
}

#[test]
fn nulls_never_match() {
    let left = [None, Some(5), None];
    let right = [None, Some(5), None];

    assert_eq!(
        pairs(inner_equi_join_nullable_i64(&left, &right, limits(3, 1)).unwrap()),
        vec![(1, 1)]
    );
}

#[test]
fn duplicate_values_produce_the_full_cross_product() {
    let left = [Some(7), Some(7), Some(8), Some(7)];
    let right = [Some(7), Some(9), Some(7)];

    assert_eq!(
        pairs(inner_equi_join_nullable_i64(&left, &right, limits(4, 6)).unwrap()),
        vec![(0, 0), (0, 2), (1, 0), (1, 2), (3, 0), (3, 2)]
    );
}

#[test]
fn matches_integer_extremes_without_overflow() {
    let left = [Some(i64::MIN), Some(0), Some(i64::MAX), Some(-1)];
    let right = [Some(i64::MAX), Some(i64::MIN), Some(-1), Some(1)];

    assert_eq!(
        pairs(inner_equi_join_nullable_i64(&left, &right, limits(4, 3)).unwrap()),
        vec![(0, 1), (2, 0), (3, 2)]
    );
}

#[test]
fn output_order_is_left_major_then_right_source_order() {
    let left = [Some(2), Some(1), Some(2), Some(1)];
    let right = [Some(1), Some(2), Some(1), Some(2)];

    assert_eq!(
        pairs(inner_equi_join_nullable_i64(&left, &right, limits(4, 8)).unwrap()),
        vec![
            (0, 1),
            (0, 3),
            (1, 0),
            (1, 2),
            (2, 1),
            (2, 3),
            (3, 0),
            (3, 2),
        ]
    );
}

#[test]
fn accepts_input_rows_and_output_pairs_exactly_at_their_bounds() {
    let left = [Some(1), Some(1), None];
    let right = [Some(1), None, Some(1)];

    assert_eq!(
        pairs(inner_equi_join_nullable_i64(&left, &right, JoinLimits::new(3, 4)).unwrap()),
        vec![(0, 0), (0, 2), (1, 0), (1, 2)]
    );
}

#[test]
fn rejects_each_input_above_the_row_bound_before_joining() {
    let three = [Some(1), Some(1), Some(1)];
    let two = [Some(1), Some(1)];

    assert_eq!(
        inner_equi_join_nullable_i64(&three, &two, JoinLimits::new(2, usize::MAX)),
        Err(JoinError::LeftInputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        inner_equi_join_nullable_i64(&two, &three, JoinLimits::new(2, usize::MAX)),
        Err(JoinError::RightInputLimitExceeded {
            rows: 3,
            max_rows: 2,
        })
    );
}

#[test]
fn rejects_one_pair_above_the_output_bound_without_a_partial_result() {
    let left = [Some(4), Some(4)];
    let right = [Some(4), Some(4)];

    assert_eq!(
        inner_equi_join_nullable_i64(&left, &right, JoinLimits::new(2, 3)),
        Err(JoinError::OutputLimitExceeded {
            pairs: 4,
            max_pairs: 3,
        })
    );
}

#[test]
fn zero_output_bound_allows_no_matches_but_rejects_the_first_match() {
    assert_eq!(
        inner_equi_join_nullable_i64(&[None], &[None], JoinLimits::new(1, 0)),
        Ok(vec![])
    );
    assert_eq!(
        inner_equi_join_nullable_i64(&[Some(0)], &[Some(0)], JoinLimits::new(1, 0)),
        Err(JoinError::OutputLimitExceeded {
            pairs: 1,
            max_pairs: 0,
        })
    );
}
