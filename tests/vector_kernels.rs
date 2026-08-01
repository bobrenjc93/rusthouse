use std::cmp::Ordering;
use std::mem::size_of;

use rusthouse::batch::{
    BatchConfig, BooleanArray, Column, DataType, DictionaryArray, Field, Float64Array, Int64Array,
    RecordBatch, Schema, SelectionMask,
};
use rusthouse::kernels::{
    AggregateExpr, AggregateKind, AggregateResult, ComparisonOp, GroupByConfig, GroupKey,
    ScalarValue, SumValue, aggregate, avg, compare_bool, compare_columns, compare_f64, compare_i64,
    compare_string, count, hash_group, is_not_null, is_null, max, min, sum,
};
use rusthouse::{Error, Result};

const LEN: usize = 137;
const CAPACITY: usize = 160;

#[derive(Clone)]
struct Input {
    ints: Vec<Option<i64>>,
    other_ints: Vec<Option<i64>>,
    floats: Vec<Option<f64>>,
    bools: Vec<Option<bool>>,
    strings: Vec<Option<&'static str>>,
}

fn input() -> Input {
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    let mut ints = Vec::with_capacity(LEN);
    let mut other_ints = Vec::with_capacity(LEN);
    let mut floats = Vec::with_capacity(LEN);
    let mut bools = Vec::with_capacity(LEN);
    let mut strings = Vec::with_capacity(LEN);
    let names = ["delta", "alpha", "delta", "", "omega", "beta"];
    for row in 0..LEN {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let random = state as i64;
        let integer = match row {
            3 => i64::MIN,
            7 => i64::MAX,
            _ => random % 10_003,
        };
        ints.push((row % 11 != 0).then_some(integer));
        other_ints.push((row % 17 != 0).then_some(integer.wrapping_add((row % 3) as i64 - 1)));
        let float = match row % 19 {
            0 => f64::NAN,
            1 => f64::INFINITY,
            2 => f64::NEG_INFINITY,
            3 => -0.0,
            4 => 0.0,
            5 => f64::MAX,
            6 => f64::MIN,
            _ => (random % 100_000) as f64 / 17.0,
        };
        floats.push((row % 13 != 0).then_some(float));
        bools.push((row % 9 != 0).then_some(row % 3 == 0));
        strings.push((row % 10 != 0).then_some(names[row % names.len()]));
    }
    Input {
        ints,
        other_ints,
        floats,
        bools,
        strings,
    }
}

fn schema() -> Schema {
    Schema::new(vec![
        Field::new("i", DataType::Int64, true),
        Field::new("j", DataType::Int64, true),
        Field::new("f", DataType::Float64, true),
        Field::new("b", DataType::Boolean, true),
        Field::new("s", DataType::String, true),
    ])
}

fn columns(input: &Input) -> Result<Vec<Column>> {
    Ok(vec![
        Column::Int64(Int64Array::from_options(
            CAPACITY,
            input.ints.iter().copied(),
        )?),
        Column::Int64(Int64Array::from_options(
            CAPACITY,
            input.other_ints.iter().copied(),
        )?),
        Column::Float64(Float64Array::from_options(
            CAPACITY,
            input.floats.iter().copied(),
        )?),
        Column::Boolean(BooleanArray::from_options(
            CAPACITY,
            input.bools.iter().copied(),
        )?),
        Column::String(DictionaryArray::from_options(
            CAPACITY,
            input.strings.iter().copied(),
        )?),
    ])
}

fn batch(input: &Input) -> RecordBatch {
    let mut batch = RecordBatch::try_new(
        schema(),
        columns(input).unwrap(),
        BatchConfig::unlimited(CAPACITY),
    )
    .unwrap();
    let mut selection = SelectionMask::all(LEN, CAPACITY).unwrap();
    for row in 0..LEN {
        if row % 7 == 2 || row % 23 == 4 {
            selection.set(row, false);
        }
    }
    batch.replace_selection(selection).unwrap();
    batch
}

fn scalar_compare<T: PartialOrd>(left: T, op: ComparisonOp, right: T) -> bool {
    match op {
        ComparisonOp::Eq => left == right,
        ComparisonOp::NotEq => left != right,
        ComparisonOp::Less => left < right,
        ComparisonOp::LessEq => left <= right,
        ComparisonOp::Greater => left > right,
        ComparisonOp::GreaterEq => left >= right,
    }
}

fn assert_mask(mask: &SelectionMask, expected: impl Fn(usize) -> bool) {
    for row in 0..LEN {
        assert_eq!(mask.is_selected(row), expected(row), "row {row}");
    }
}

#[test]
fn predicates_match_scalar_references_across_nulls_and_edges() {
    let input = input();
    let batch = batch(&input);
    let operations = [
        ComparisonOp::Eq,
        ComparisonOp::NotEq,
        ComparisonOp::Less,
        ComparisonOp::LessEq,
        ComparisonOp::Greater,
        ComparisonOp::GreaterEq,
    ];

    for operation in operations {
        let mask = compare_i64(&batch, 0, operation, 0).unwrap();
        assert_mask(&mask, |row| {
            batch.selection().is_selected(row)
                && input.ints[row].is_some_and(|value| scalar_compare(value, operation, 0))
        });

        for target in [0.0, -0.0, f64::NAN, f64::INFINITY] {
            let mask = compare_f64(&batch, 2, operation, target).unwrap();
            assert_mask(&mask, |row| {
                batch.selection().is_selected(row)
                    && input.floats[row]
                        .is_some_and(|value| scalar_compare(value, operation, target))
            });
        }

        let mask = compare_bool(&batch, 3, operation, true).unwrap();
        assert_mask(&mask, |row| {
            batch.selection().is_selected(row)
                && input.bools[row].is_some_and(|value| scalar_compare(value, operation, true))
        });

        let mask = compare_string(&batch, 4, operation, "delta").unwrap();
        assert_mask(&mask, |row| {
            batch.selection().is_selected(row)
                && input.strings[row].is_some_and(|value| scalar_compare(value, operation, "delta"))
        });

        let mask = compare_columns(&batch, 0, operation, 1).unwrap();
        assert_mask(&mask, |row| {
            batch.selection().is_selected(row)
                && input.ints[row]
                    .zip(input.other_ints[row])
                    .is_some_and(|(left, right)| scalar_compare(left, operation, right))
        });

        for (column, expected) in [
            (
                2,
                input
                    .floats
                    .iter()
                    .map(|value| value.map(|value| scalar_compare(value, operation, value)))
                    .collect::<Vec<_>>(),
            ),
            (
                3,
                input
                    .bools
                    .iter()
                    .map(|value| value.map(|value| scalar_compare(value, operation, value)))
                    .collect::<Vec<_>>(),
            ),
            (
                4,
                input
                    .strings
                    .iter()
                    .map(|value| value.map(|value| scalar_compare(value, operation, value)))
                    .collect::<Vec<_>>(),
            ),
        ] {
            let mask = compare_columns(&batch, column, operation, column).unwrap();
            assert_mask(&mask, |row| {
                batch.selection().is_selected(row) && expected[row] == Some(true)
            });
        }
    }

    let nulls = is_null(&batch, 0).unwrap();
    assert_mask(&nulls, |row| {
        batch.selection().is_selected(row) && input.ints[row].is_none()
    });
    let valid = is_not_null(&batch, 0).unwrap();
    assert_mask(&valid, |row| {
        batch.selection().is_selected(row) && input.ints[row].is_some()
    });
}

fn selected_values<'a, T: Copy>(
    batch: &'a RecordBatch,
    values: &'a [Option<T>],
) -> impl Iterator<Item = T> + 'a {
    values.iter().enumerate().filter_map(|(row, value)| {
        batch
            .selection()
            .is_selected(row)
            .then_some(*value)
            .flatten()
    })
}

fn scalar_float_min_max(values: impl Iterator<Item = f64>, is_min: bool) -> Option<f64> {
    values.reduce(|current, candidate| {
        let ordering = candidate.total_cmp(&current);
        if (is_min && ordering == Ordering::Less) || (!is_min && ordering == Ordering::Greater) {
            candidate
        } else {
            current
        }
    })
}

fn assert_float(actual: f64, expected: f64) {
    if expected.is_nan() {
        assert!(actual.is_nan(), "expected NaN, got {actual}");
    } else {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }
}

#[test]
fn every_aggregate_matches_scalar_references() {
    let input = input();
    let batch = batch(&input);
    assert_eq!(
        count(&batch, None).unwrap(),
        batch.selection().selected_count() as u64
    );
    assert_eq!(
        count(&batch, Some(0)).unwrap(),
        selected_values(&batch, &input.ints).count() as u64
    );

    let int_values: Vec<_> = selected_values(&batch, &input.ints).collect();
    let int_sum: i128 = int_values.iter().copied().map(i128::from).sum();
    assert_eq!(sum(&batch, 0).unwrap(), Some(SumValue::Int128(int_sum)));
    assert_eq!(
        min(&batch, 0).unwrap(),
        int_values.iter().min().copied().map(ScalarValue::Int64)
    );
    assert_eq!(
        max(&batch, 0).unwrap(),
        int_values.iter().max().copied().map(ScalarValue::Int64)
    );
    assert_eq!(
        avg(&batch, 0).unwrap(),
        Some(int_sum as f64 / int_values.len() as f64)
    );

    let float_values: Vec<_> = selected_values(&batch, &input.floats).collect();
    let expected_sum = float_values.iter().copied().sum::<f64>();
    let Some(SumValue::Float64(actual_sum)) = sum(&batch, 2).unwrap() else {
        panic!("expected a floating sum")
    };
    assert_float(actual_sum, expected_sum);
    let Some(ScalarValue::Float64(actual_min)) = min(&batch, 2).unwrap() else {
        panic!("expected a floating minimum")
    };
    assert_float(
        actual_min,
        scalar_float_min_max(float_values.iter().copied(), true).unwrap(),
    );
    let Some(ScalarValue::Float64(actual_max)) = max(&batch, 2).unwrap() else {
        panic!("expected a floating maximum")
    };
    assert_float(
        actual_max,
        scalar_float_min_max(float_values.iter().copied(), false).unwrap(),
    );
    assert_float(
        avg(&batch, 2).unwrap().unwrap(),
        expected_sum / float_values.len() as f64,
    );

    let bool_values: Vec<_> = selected_values(&batch, &input.bools).collect();
    assert_eq!(
        min(&batch, 3).unwrap(),
        bool_values.iter().min().copied().map(ScalarValue::Boolean)
    );
    assert_eq!(
        max(&batch, 3).unwrap(),
        bool_values.iter().max().copied().map(ScalarValue::Boolean)
    );
    let string_values: Vec<_> = selected_values(&batch, &input.strings).collect();
    assert_eq!(
        min(&batch, 4).unwrap(),
        string_values
            .iter()
            .min()
            .map(|value| ScalarValue::String((*value).into()))
    );
    assert_eq!(
        max(&batch, 4).unwrap(),
        string_values
            .iter()
            .max()
            .map(|value| ScalarValue::String((*value).into()))
    );

    assert_eq!(
        aggregate(&batch, AggregateExpr::count_all()).unwrap(),
        AggregateResult::Count(batch.selection().selected_count() as u64)
    );
    assert_eq!(
        aggregate(&batch, AggregateExpr::new(AggregateKind::Sum, 0)).unwrap(),
        AggregateResult::Sum(Some(SumValue::Int128(int_sum)))
    );
}

#[test]
fn aggregates_return_sql_null_results_for_an_empty_selection() {
    let input = input();
    let mut batch = batch(&input);
    batch
        .replace_selection(SelectionMask::none(LEN, CAPACITY).unwrap())
        .unwrap();
    assert_eq!(count(&batch, None).unwrap(), 0);
    assert_eq!(sum(&batch, 0).unwrap(), None);
    assert_eq!(min(&batch, 0).unwrap(), None);
    assert_eq!(max(&batch, 0).unwrap(), None);
    assert_eq!(avg(&batch, 0).unwrap(), None);
}

fn group_expressions() -> Vec<AggregateExpr> {
    vec![
        AggregateExpr::count_all(),
        AggregateExpr::new(AggregateKind::Count, 0),
        AggregateExpr::new(AggregateKind::Sum, 0),
        AggregateExpr::new(AggregateKind::Min, 0),
        AggregateExpr::new(AggregateKind::Max, 0),
        AggregateExpr::new(AggregateKind::Avg, 0),
        AggregateExpr::new(AggregateKind::Min, 3),
        AggregateExpr::new(AggregateKind::Max, 3),
        AggregateExpr::new(AggregateKind::Min, 4),
        AggregateExpr::new(AggregateKind::Max, 4),
        AggregateExpr::new(AggregateKind::Count, 2),
        AggregateExpr::new(AggregateKind::Sum, 2),
        AggregateExpr::new(AggregateKind::Min, 2),
        AggregateExpr::new(AggregateKind::Max, 2),
        AggregateExpr::new(AggregateKind::Avg, 2),
    ]
}

fn assert_aggregate_result(actual: AggregateResult, expected: AggregateResult, context: &str) {
    match (actual, expected) {
        (
            AggregateResult::Sum(Some(SumValue::Float64(actual))),
            AggregateResult::Sum(Some(SumValue::Float64(expected))),
        )
        | (
            AggregateResult::Min(Some(ScalarValue::Float64(actual))),
            AggregateResult::Min(Some(ScalarValue::Float64(expected))),
        )
        | (
            AggregateResult::Max(Some(ScalarValue::Float64(actual))),
            AggregateResult::Max(Some(ScalarValue::Float64(expected))),
        )
        | (AggregateResult::Avg(Some(actual)), AggregateResult::Avg(Some(expected))) => {
            if expected.is_nan() {
                assert!(actual.is_nan(), "{context}: expected NaN, got {actual}");
            } else {
                assert_eq!(actual.to_bits(), expected.to_bits(), "{context}");
            }
        }
        (actual, expected) => assert_eq!(actual, expected, "{context}"),
    }
}

#[test]
fn hash_grouping_matches_per_group_scalar_aggregates() {
    let input = input();
    let batch = batch(&input);
    let expressions = group_expressions();
    let grouped = hash_group(
        &batch,
        &[4],
        &expressions,
        GroupByConfig::unlimited(CAPACITY),
    )
    .unwrap();

    let mut expected_keys: Vec<Option<&str>> = Vec::new();
    for row in 0..LEN {
        if batch.selection().is_selected(row) && !expected_keys.contains(&input.strings[row]) {
            expected_keys.push(input.strings[row]);
        }
    }
    assert_eq!(grouped.len(), expected_keys.len());

    for (group, expected_key) in grouped.iter().zip(expected_keys) {
        match (&group.keys()[0], expected_key) {
            (GroupKey::Null, None) => {}
            (GroupKey::String(actual), Some(expected)) => assert_eq!(actual.as_ref(), expected),
            pair => panic!("unexpected key pair {pair:?}"),
        }
        let mut mask = SelectionMask::none(LEN, CAPACITY).unwrap();
        for row in 0..LEN {
            mask.set(
                row,
                batch.selection().is_selected(row) && input.strings[row] == expected_key,
            );
        }
        let mut reference_batch = batch.clone();
        reference_batch.replace_selection(mask).unwrap();
        for (index, expression) in expressions.iter().enumerate() {
            assert_aggregate_result(
                group.aggregate(index).unwrap(),
                aggregate(&reference_batch, *expression).unwrap(),
                &format!("aggregate {index} for {expected_key:?}"),
            );
        }
    }
}

#[test]
fn float_group_keys_coalesce_nan_and_signed_zero() {
    let input = input();
    let batch = batch(&input);
    let grouped = hash_group(
        &batch,
        &[2],
        &[AggregateExpr::count_all()],
        GroupByConfig::unlimited(CAPACITY),
    )
    .unwrap();
    let nan_groups = grouped
        .iter()
        .filter(|group| group.keys()[0].as_f64().is_some_and(f64::is_nan))
        .count();
    let zero_groups = grouped
        .iter()
        .filter(|group| group.keys()[0].as_f64() == Some(0.0))
        .count();
    assert_eq!(nan_groups, 1);
    assert_eq!(zero_groups, 1);

    for group in grouped.iter() {
        let expected = match &group.keys()[0] {
            GroupKey::Null => (0..LEN)
                .filter(|&row| batch.selection().is_selected(row) && input.floats[row].is_none())
                .count(),
            GroupKey::Float64(_) => {
                let key = group.keys()[0].as_f64().unwrap();
                (0..LEN)
                    .filter(|&row| {
                        batch.selection().is_selected(row)
                            && input.floats[row].is_some_and(|value| {
                                (value.is_nan() && key.is_nan()) || value == key
                            })
                    })
                    .count()
            }
            _ => unreachable!(),
        };
        assert_eq!(
            group.aggregate(0),
            Some(AggregateResult::Count(expected as u64))
        );
    }
}

#[test]
fn retained_byte_ceilings_are_exact_boundaries() {
    let input = input();
    let columns = columns(&input).unwrap();
    let unlimited =
        RecordBatch::try_new(schema(), columns.clone(), BatchConfig::unlimited(CAPACITY)).unwrap();
    let required = unlimited.retained_bytes();
    let exact = RecordBatch::try_new(
        schema(),
        columns.clone(),
        BatchConfig::new(CAPACITY, required),
    )
    .unwrap();
    assert_eq!(exact.retained_bytes(), required);
    assert!(matches!(
        RecordBatch::try_new(
            schema(),
            columns,
            BatchConfig::new(CAPACITY, required - 1)
        ),
        Err(Error::MemoryLimitExceeded {
            operator: "record batch",
            required: actual,
            limit
        }) if actual == required && limit == required - 1
    ));

    let batch = batch(&input);
    let expressions = group_expressions();
    let unlimited = hash_group(
        &batch,
        &[4],
        &expressions,
        GroupByConfig::unlimited(CAPACITY),
    )
    .unwrap();
    let peak = unlimited.peak_retained_bytes();
    let exact = hash_group(
        &batch,
        &[4],
        &expressions,
        GroupByConfig::new(CAPACITY, peak),
    )
    .unwrap();
    assert_eq!(exact.peak_retained_bytes(), peak);
    assert!(matches!(
        hash_group(
            &batch,
            &[4],
            &expressions,
            GroupByConfig::new(CAPACITY, peak - 1)
        ),
        Err(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            ..
        })
    ));
}

#[test]
fn fixed_capacity_and_group_limits_fail_cleanly() {
    let mut array = Int64Array::with_capacity(1);
    array.push(Some(1)).unwrap();
    assert_eq!(
        array.push(Some(2)),
        Err(Error::CapacityExceeded { capacity: 1 })
    );

    let input = input();
    let batch = batch(&input);
    assert_eq!(
        hash_group(
            &batch,
            &[4],
            &[AggregateExpr::count_all()],
            GroupByConfig::unlimited(1)
        )
        .unwrap_err(),
        Error::GroupLimitExceeded { max_groups: 1 }
    );
}

#[test]
fn compressed_array_layout_accounting_matches_owned_allocations() {
    let capacity = 130;
    let strings = DictionaryArray::from_options(
        capacity,
        [Some("repeat"), None, Some("repeat"), Some("repeat")],
    )
    .unwrap();
    assert_eq!(strings.dictionary().collect::<Vec<_>>(), ["repeat"]);
    assert_eq!(strings.value(0), Some("repeat"));
    assert_eq!(strings.value(1), None);
    let bitmap_bytes = capacity.div_ceil(u64::BITS as usize) * size_of::<u64>();
    assert_eq!(
        strings.retained_bytes(),
        capacity * size_of::<u32>()
            + capacity * size_of::<Option<Box<str>>>()
            + "repeat".len()
            + bitmap_bytes
    );

    let booleans = BooleanArray::from_options(capacity, [Some(true), None, Some(false)]).unwrap();
    assert_eq!(booleans.retained_bytes(), bitmap_bytes * 2);
    let selection = SelectionMask::all(3, capacity).unwrap();
    assert_eq!(selection.retained_bytes(), bitmap_bytes);
}
