use rusthouse::{
    GroupedCountError, GroupedCountLimits, NullableI64GroupedCount, grouped_count_nullable_i64,
};

fn pairs(groups: Vec<NullableI64GroupedCount>) -> Vec<(Option<i64>, u64)> {
    groups.into_iter().map(|group| group.into_pair()).collect()
}

#[test]
fn empty_input_produces_no_groups() {
    assert_eq!(
        grouped_count_nullable_i64(&[], GroupedCountLimits::new(0, 0)),
        Ok(vec![])
    );
}

#[test]
fn repeated_values_and_nulls_form_exact_groups() {
    let values = [Some(7), None, Some(-2), Some(7), None, Some(-2), Some(7)];

    let groups =
        grouped_count_nullable_i64(&values, GroupedCountLimits::new(values.len(), 3)).unwrap();

    assert_eq!(pairs(groups), [(None, 2), (Some(-2), 2), (Some(7), 3)]);
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
    let expected = vec![
        (None, 1),
        (Some(i64::MIN), 1),
        (Some(-1), 1),
        (Some(0), 1),
        (Some(i64::MAX), 2),
    ];

    for input in [&values[..], &reordered[..]] {
        let groups =
            grouped_count_nullable_i64(input, GroupedCountLimits::new(input.len(), expected.len()))
                .unwrap();

        assert_eq!(pairs(groups), expected);
    }
}

#[test]
fn accepts_input_rows_and_distinct_groups_exactly_at_their_caps() {
    let values = [Some(2), None, Some(1), Some(2)];

    let groups = grouped_count_nullable_i64(&values, GroupedCountLimits::new(4, 3)).unwrap();

    assert_eq!(pairs(groups), [(None, 1), (Some(1), 1), (Some(2), 2)]);
}

#[test]
fn rejects_input_rows_and_distinct_groups_above_their_caps() {
    let values = [Some(2), None, Some(1), Some(2)];

    assert_eq!(
        grouped_count_nullable_i64(&values, GroupedCountLimits::new(3, 3)),
        Err(GroupedCountError::InputLimitExceeded {
            rows: 4,
            max_rows: 3,
        })
    );
    assert_eq!(
        grouped_count_nullable_i64(&values, GroupedCountLimits::new(4, 2)),
        Err(GroupedCountError::DistinctGroupLimitExceeded {
            groups: 3,
            max_groups: 2,
        })
    );
}

#[test]
fn zero_group_cap_accepts_empty_input_and_rejects_the_first_key() {
    assert_eq!(
        grouped_count_nullable_i64(&[], GroupedCountLimits::new(0, 0)),
        Ok(vec![])
    );
    assert_eq!(
        grouped_count_nullable_i64(&[None, None], GroupedCountLimits::new(2, 0)),
        Err(GroupedCountError::DistinctGroupLimitExceeded {
            groups: 1,
            max_groups: 0,
        })
    );
}

#[test]
fn validates_the_input_cap_before_the_group_cap() {
    assert_eq!(
        grouped_count_nullable_i64(&[Some(1)], GroupedCountLimits::new(0, 0)),
        Err(GroupedCountError::InputLimitExceeded {
            rows: 1,
            max_rows: 0,
        })
    );
}
