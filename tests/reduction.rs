use std::collections::HashSet;

use rusthouse::{ComparisonOperator, DataType, Field, ReductionError, RowSelection, Table, Value};

#[test]
fn count_and_sum_return_typed_zeroes_for_empty_inputs() {
    let table = all_types_table();
    let selection = RowSelection::try_empty(0).unwrap();

    assert_eq!(table.count(None), Ok(0));
    assert_eq!(table.count(Some(&selection)), Ok(0));
    assert_eq!(table.sum("integer", None), Ok(Value::Int64(0)));
    assert_eq!(
        table.sum("float", Some(&selection)),
        Ok(Value::Float64(0.0))
    );
    let Value::Float64(float_sum) = table.sum("float", None).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert_eq!(float_sum.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn count_and_sum_cover_full_and_selected_numeric_inputs() {
    let mut table = all_types_table();
    table
        .insert_batch([
            row(-7, 1.25, false, "skip"),
            row(12, -3.5, true, "keep"),
            row(4, 0.75, true, "keep"),
        ])
        .unwrap();
    let selection = table
        .scan("boolean", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(table.count(None), Ok(3));
    assert_eq!(table.sum("integer", None), Ok(Value::Int64(9)));
    assert_eq!(table.sum("float", None), Ok(Value::Float64(-1.5)));
    assert_eq!(table.count(Some(&selection)), Ok(2));
    assert_eq!(table.sum("integer", Some(&selection)), Ok(Value::Int64(16)));
    assert_eq!(
        table.sum("float", Some(&selection)),
        Ok(Value::Float64(-2.75))
    );
}

#[test]
fn count_distinct_covers_duplicates_and_selections_for_every_physical_type() {
    let mut table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
        Field::new("include", DataType::Bool),
    ])
    .unwrap();
    table
        .insert_batch([
            filtered_row(1, 1.5, true, "one", true),
            filtered_row(1, 1.5, true, "one", true),
            filtered_row(2, 2.5, false, "two", true),
            filtered_row(3, 3.5, false, "three", false),
        ])
        .unwrap();
    let selection = table
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    for (field, all, selected) in [
        ("integer", 3, 2),
        ("float", 3, 2),
        ("boolean", 2, 2),
        ("text", 3, 2),
    ] {
        assert_eq!(table.count_distinct(field, None), Ok(all));
        assert_eq!(table.count_distinct(field, Some(&selection)), Ok(selected));
    }
}

#[test]
fn count_distinct_returns_zero_for_empty_inputs_and_validates_before_allocating() {
    let empty = all_types_table();
    let empty_selection = RowSelection::try_empty(0).unwrap();
    for field in ["integer", "float", "boolean", "text"] {
        assert_eq!(empty.count_distinct(field, None), Ok(0));
        assert_eq!(empty.count_distinct(field, Some(&empty_selection)), Ok(0));
    }

    let mut populated = all_types_table();
    populated.insert_batch([row(1, 1.0, true, "one")]).unwrap();
    let no_rows = RowSelection::try_empty(1).unwrap();
    assert_eq!(populated.count_distinct("text", Some(&no_rows)), Ok(0));
    assert_eq!(
        populated.count_distinct("missing", None),
        Err(ReductionError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
    assert_eq!(
        populated.count_distinct("integer", Some(&RowSelection::try_empty(2).unwrap())),
        Err(ReductionError::SelectionLengthMismatch {
            table_rows: 1,
            selection_rows: 2,
        })
    );
}

#[test]
fn count_distinct_uses_total_float_identity_for_nans_and_signed_zeroes() {
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let other_nan = f64::from_bits(0x7ff8_0000_0000_0002);
    let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch([
            vec![Value::Float64(nan)],
            vec![Value::Float64(nan)],
            vec![Value::Float64(other_nan)],
            vec![Value::Float64(negative_nan)],
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(0.0)],
        ])
        .unwrap();

    assert_eq!(table.count_distinct("value", None), Ok(5));
}

#[test]
fn reductions_differentially_match_straightforward_references() {
    let mut state = 0xa409_3822_299f_31d0_u64;

    for case in 0..128 {
        state = next_state(state);
        let row_count = state as usize % 97;
        let mut integers = Vec::with_capacity(row_count);
        let mut floats = Vec::with_capacity(row_count);
        let mut booleans = Vec::with_capacity(row_count);
        let mut strings = Vec::with_capacity(row_count);
        let mut included = Vec::with_capacity(row_count);

        for row in 0..row_count {
            state = next_state(state);
            integers.push((state % 2_001) as i64 - 1_000);
            state = next_state(state);
            floats.push((state % 2_001) as f64 / 4.0 - 250.0);
            booleans.push(state & 1 == 0);
            strings.push(format!("key-{}", state % 13));
            included.push(!(state ^ row as u64).is_multiple_of(3));
        }

        let mut table = Table::new(vec![
            Field::new("integer", DataType::Int64),
            Field::new("float", DataType::Float64),
            Field::new("boolean", DataType::Bool),
            Field::new("text", DataType::String),
            Field::new("include", DataType::Bool),
        ])
        .unwrap();
        table
            .insert_batch((0..row_count).map(|row| {
                vec![
                    Value::Int64(integers[row]),
                    Value::Float64(floats[row]),
                    Value::Bool(booleans[row]),
                    Value::String(strings[row].clone()),
                    Value::Bool(included[row]),
                ]
            }))
            .unwrap();
        let selection = table
            .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
            .unwrap();

        let all_rows = (0..row_count).collect::<Vec<_>>();
        let selected_rows = included
            .iter()
            .enumerate()
            .filter_map(|(row, include)| include.then_some(row))
            .collect::<Vec<_>>();
        check_reduction_references(
            &table, None, &all_rows, &integers, &floats, &booleans, &strings, case,
        );
        check_reduction_references(
            &table,
            Some(&selection),
            &selected_rows,
            &integers,
            &floats,
            &booleans,
            &strings,
            case,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_reduction_references(
    table: &Table,
    selection: Option<&RowSelection>,
    rows: &[usize],
    integers: &[i64],
    floats: &[f64],
    booleans: &[bool],
    strings: &[String],
    case: usize,
) {
    let integer_values = rows.iter().map(|row| integers[*row]).collect::<Vec<_>>();
    let float_values = rows.iter().map(|row| floats[*row]).collect::<Vec<_>>();
    let boolean_values = rows.iter().map(|row| booleans[*row]).collect::<Vec<_>>();
    let string_values = rows
        .iter()
        .map(|row| strings[*row].as_str())
        .collect::<Vec<_>>();

    assert_eq!(table.count(selection), Ok(rows.len()), "case {case}");
    assert_eq!(
        table.sum("integer", selection),
        Ok(Value::Int64(integer_values.iter().sum())),
        "case {case}",
    );
    let expected_float_sum = float_values.iter().fold(0.0, |sum, value| sum + value);
    let Value::Float64(actual_float_sum) = table.sum("float", selection).unwrap() else {
        panic!("Float64 sum returned a different type in case {case}");
    };
    assert_eq!(
        actual_float_sum.to_bits(),
        expected_float_sum.to_bits(),
        "case {case}"
    );

    for (field, expected) in [
        (
            "integer",
            integer_values.iter().copied().collect::<HashSet<_>>().len(),
        ),
        (
            "float",
            float_values
                .iter()
                .map(|value| value.to_bits())
                .collect::<HashSet<_>>()
                .len(),
        ),
        (
            "boolean",
            boolean_values.iter().copied().collect::<HashSet<_>>().len(),
        ),
        (
            "text",
            string_values.iter().copied().collect::<HashSet<_>>().len(),
        ),
    ] {
        assert_eq!(
            table.count_distinct(field, selection),
            Ok(expected),
            "case {case}"
        );
    }

    let expected_integer_average = (!rows.is_empty())
        .then(|| Value::Float64(integer_values.iter().sum::<i64>() as f64 / rows.len() as f64));
    let expected_float_average =
        (!rows.is_empty()).then(|| Value::Float64(expected_float_sum / rows.len() as f64));
    assert_eq!(
        table.avg("integer", selection),
        Ok(expected_integer_average),
        "case {case}"
    );
    assert_eq!(
        table.avg("float", selection),
        Ok(expected_float_average),
        "case {case}"
    );

    assert_eq!(
        table.min("integer", selection),
        Ok(integer_values.iter().min().copied().map(Value::Int64)),
        "case {case}",
    );
    assert_eq!(
        table.max("integer", selection),
        Ok(integer_values.iter().max().copied().map(Value::Int64)),
        "case {case}",
    );
    assert_eq!(
        table.min("boolean", selection),
        Ok(boolean_values.iter().min().copied().map(Value::Bool)),
        "case {case}",
    );
    assert_eq!(
        table.max("text", selection),
        Ok(string_values.iter().max().map(|value| Value::from(*value))),
        "case {case}",
    );
}

const fn next_state(state: u64) -> u64 {
    state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

#[test]
fn int64_sum_checks_both_overflow_directions_and_honors_selection() {
    let mut positive = Table::new(vec![
        Field::new("value", DataType::Int64),
        Field::new("include", DataType::Bool),
    ])
    .unwrap();
    positive
        .insert_batch([
            vec![Value::Int64(i64::MAX), Value::Bool(true)],
            vec![Value::Int64(1), Value::Bool(false)],
        ])
        .unwrap();
    let selection = positive
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(
        positive.sum("value", Some(&selection)),
        Ok(Value::Int64(i64::MAX))
    );
    assert_eq!(
        positive.sum("value", None),
        Err(ReductionError::Int64Overflow {
            field: "value".to_owned(),
            row: 1,
        })
    );

    let mut negative = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    negative
        .insert_batch([vec![Value::Int64(i64::MIN)], vec![Value::Int64(-1)]])
        .unwrap();
    assert_eq!(
        negative.sum("value", None),
        Err(ReductionError::Int64Overflow {
            field: "value".to_owned(),
            row: 1,
        })
    );

    let mut boundaries = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    boundaries
        .insert_batch([vec![Value::Int64(i64::MAX)], vec![Value::Int64(i64::MIN)]])
        .unwrap();
    assert_eq!(boundaries.sum("value", None), Ok(Value::Int64(-1)));
}

#[test]
fn float64_sum_uses_ordered_ieee_754_addition() {
    let mut overflow = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    overflow
        .insert_batch([
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(f64::MAX)],
        ])
        .unwrap();
    assert_eq!(
        overflow.sum("value", None),
        Ok(Value::Float64(f64::INFINITY))
    );

    let mut special = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    special
        .insert_batch([
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(f64::INFINITY)],
            vec![Value::Float64(f64::NEG_INFINITY)],
        ])
        .unwrap();
    let Value::Float64(total) = special.sum("value", None).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert!(total.is_nan());

    let negative_zero = special
        .scan("value", ComparisonOperator::Equal, &Value::Float64(-0.0))
        .unwrap();
    let Value::Float64(zero) = special.sum("value", Some(&negative_zero)).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn count_and_sum_reject_selections_for_a_different_row_count() {
    let mut table = all_types_table();
    table.insert_batch([row(1, 1.0, true, "one")]).unwrap();
    let selection = RowSelection::try_empty(2).unwrap();
    let expected = ReductionError::SelectionLengthMismatch {
        table_rows: 1,
        selection_rows: 2,
    };

    assert_eq!(table.count(Some(&selection)), Err(expected.clone()));
    assert_eq!(table.sum("integer", Some(&selection)), Err(expected));
}

#[test]
fn sum_reports_missing_and_nonnumeric_columns() {
    let table = all_types_table();

    assert_eq!(
        table.sum("missing", None),
        Err(ReductionError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
    for (field, data_type) in [("boolean", DataType::Bool), ("text", DataType::String)] {
        assert_eq!(
            table.sum(field, None),
            Err(ReductionError::NonNumericColumn {
                field: field.to_owned(),
                data_type,
            })
        );
    }
}

#[test]
fn avg_covers_full_and_selected_numeric_inputs() {
    let mut table = all_types_table();
    table
        .insert_batch([
            row(-7, 1.25, false, "skip"),
            row(12, -3.5, true, "keep"),
            row(4, 0.75, true, "keep"),
        ])
        .unwrap();
    let selection = table
        .scan("boolean", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(table.avg("integer", None), Ok(Some(Value::Float64(3.0))));
    assert_eq!(table.avg("float", None), Ok(Some(Value::Float64(-0.5))));
    assert_eq!(
        table.avg("integer", Some(&selection)),
        Ok(Some(Value::Float64(8.0)))
    );
    assert_eq!(
        table.avg("float", Some(&selection)),
        Ok(Some(Value::Float64(-1.375)))
    );
}

#[test]
fn avg_returns_none_for_empty_tables_and_selections() {
    let empty = all_types_table();
    let empty_table_selection = RowSelection::try_empty(0).unwrap();

    for field in ["integer", "float"] {
        assert_eq!(empty.avg(field, None), Ok(None));
        assert_eq!(empty.avg(field, Some(&empty_table_selection)), Ok(None));
    }

    let mut populated = all_types_table();
    populated
        .insert_batch([row(1, 1.0, true, "present")])
        .unwrap();
    let empty_selection = RowSelection::try_empty(1).unwrap();
    for field in ["integer", "float"] {
        assert_eq!(populated.avg(field, Some(&empty_selection)), Ok(None));
    }
}

#[test]
fn int64_avg_uses_a_wide_exact_accumulator() {
    let mut repeated_maximum = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    repeated_maximum
        .insert_batch([vec![Value::Int64(i64::MAX)], vec![Value::Int64(i64::MAX)]])
        .unwrap();
    assert_eq!(
        repeated_maximum.avg("value", None),
        Ok(Some(Value::Float64(i64::MAX as f64)))
    );

    let mut cancellation = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    cancellation
        .insert_batch([vec![Value::Int64(i64::MAX)], vec![Value::Int64(i64::MIN)]])
        .unwrap();
    assert_eq!(
        cancellation.avg("value", None),
        Ok(Some(Value::Float64(-0.5)))
    );

    let mut exact_quotient = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    exact_quotient
        .insert_batch([
            vec![Value::Int64(9_007_199_254_740_991)],
            vec![Value::Int64(1)],
            vec![Value::Int64(1)],
        ])
        .unwrap();
    assert_eq!(
        exact_quotient.avg("value", None),
        Ok(Some(Value::Float64(3_002_399_751_580_331.0)))
    );
}

#[test]
fn int64_avg_rounds_halfway_results_to_even() {
    const BASE: i64 = 9_007_199_254_740_992;
    let mut table = Table::new(vec![
        Field::new("even_lower", DataType::Int64),
        Field::new("even_upper", DataType::Int64),
    ])
    .unwrap();
    table
        .insert_batch([
            vec![Value::Int64(BASE), Value::Int64(BASE + 2)],
            vec![Value::Int64(BASE + 2), Value::Int64(BASE + 4)],
        ])
        .unwrap();

    assert_eq!(
        table.avg("even_lower", None),
        Ok(Some(Value::Float64(BASE as f64)))
    );
    assert_eq!(
        table.avg("even_upper", None),
        Ok(Some(Value::Float64((BASE + 4) as f64)))
    );
}

#[test]
fn float64_avg_resists_finite_overflow_and_uses_ieee_754_operations() {
    let mut overflow = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    overflow
        .insert_batch([
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(f64::MAX)],
        ])
        .unwrap();
    assert_eq!(
        overflow.avg("value", None),
        Ok(Some(Value::Float64(f64::MAX)))
    );

    let mut cancellation = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    cancellation
        .insert_batch([
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(-f64::MAX)],
        ])
        .unwrap();
    assert_eq!(
        cancellation.avg("value", None),
        Ok(Some(Value::Float64(f64::MAX / 3.0)))
    );

    let mut infinities = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    infinities
        .insert_batch([
            vec![Value::Float64(f64::INFINITY)],
            vec![Value::Float64(f64::NEG_INFINITY)],
        ])
        .unwrap();
    let positive = infinities
        .scan(
            "value",
            ComparisonOperator::Equal,
            &Value::Float64(f64::INFINITY),
        )
        .unwrap();
    let negative = infinities
        .scan(
            "value",
            ComparisonOperator::Equal,
            &Value::Float64(f64::NEG_INFINITY),
        )
        .unwrap();
    assert_eq!(
        infinities.avg("value", Some(&positive)),
        Ok(Some(Value::Float64(f64::INFINITY)))
    );
    assert_eq!(
        infinities.avg("value", Some(&negative)),
        Ok(Some(Value::Float64(f64::NEG_INFINITY)))
    );
    assert_avg_is_nan(&infinities);

    let mut nan = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    nan.insert_batch([vec![Value::Float64(1.0)], vec![Value::Float64(f64::NAN)]])
        .unwrap();
    assert_avg_is_nan(&nan);
}

#[test]
fn float64_avg_of_signed_zeroes_is_positive_zero() {
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch([vec![Value::Float64(-0.0)], vec![Value::Float64(-0.0)]])
        .unwrap();

    let Some(Value::Float64(average)) = table.avg("value", None).unwrap() else {
        panic!("nonempty Float64 averages must produce a Float64 value");
    };
    assert_eq!(average.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn avg_reports_validation_errors() {
    let mut table = all_types_table();
    table.insert_batch([row(1, 1.0, true, "one")]).unwrap();

    assert_eq!(
        table.avg("missing", None),
        Err(ReductionError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
    for (field, data_type) in [("boolean", DataType::Bool), ("text", DataType::String)] {
        assert_eq!(
            table.avg(field, None),
            Err(ReductionError::NonNumericColumn {
                field: field.to_owned(),
                data_type,
            })
        );
    }

    let selection = RowSelection::try_empty(2).unwrap();
    assert_eq!(
        table.avg("integer", Some(&selection)),
        Err(ReductionError::SelectionLengthMismatch {
            table_rows: 1,
            selection_rows: 2,
        })
    );
}

#[test]
fn min_and_max_preserve_every_physical_type_for_full_inputs() {
    let mut table = all_types_table();
    table
        .insert_batch([
            row(7, 3.5, true, "pear"),
            row(-4, -8.25, false, "apple"),
            row(12, 1.0, true, "orange"),
        ])
        .unwrap();

    assert_extrema(&table, "integer", None, Value::Int64(-4), Value::Int64(12));
    assert_extrema(
        &table,
        "float",
        None,
        Value::Float64(-8.25),
        Value::Float64(3.5),
    );
    assert_extrema(
        &table,
        "boolean",
        None,
        Value::Bool(false),
        Value::Bool(true),
    );
    assert_extrema(
        &table,
        "text",
        None,
        Value::String("apple".to_owned()),
        Value::String("pear".to_owned()),
    );
}

#[test]
fn min_and_max_only_consider_selected_rows_for_every_type() {
    let mut table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
        Field::new("include", DataType::Bool),
    ])
    .unwrap();
    table
        .insert_batch([
            filtered_row(-100, -100.0, false, "aardvark", false),
            filtered_row(8, 4.0, false, "lime", true),
            filtered_row(2, 9.0, true, "plum", true),
            filtered_row(100, 100.0, true, "zebra", false),
        ])
        .unwrap();
    let selection = table
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(selection.selected_rows().collect::<Vec<_>>(), [1, 2]);
    assert_extrema(
        &table,
        "integer",
        Some(&selection),
        Value::Int64(2),
        Value::Int64(8),
    );
    assert_extrema(
        &table,
        "float",
        Some(&selection),
        Value::Float64(4.0),
        Value::Float64(9.0),
    );
    assert_extrema(
        &table,
        "boolean",
        Some(&selection),
        Value::Bool(false),
        Value::Bool(true),
    );
    assert_extrema(
        &table,
        "text",
        Some(&selection),
        Value::String("lime".to_owned()),
        Value::String("plum".to_owned()),
    );
}

#[test]
fn min_and_max_return_none_for_empty_tables_and_empty_selections() {
    let empty = all_types_table();
    let empty_table_selection = RowSelection::try_empty(0).unwrap();

    for field in ["integer", "float", "boolean", "text"] {
        assert_eq!(empty.min(field, None), Ok(None));
        assert_eq!(empty.max(field, Some(&empty_table_selection)), Ok(None));
    }

    let mut populated = all_types_table();
    populated
        .insert_batch([row(1, 1.0, true, "present")])
        .unwrap();
    let empty_selection = RowSelection::try_empty(1).unwrap();
    for field in ["integer", "float", "boolean", "text"] {
        assert_eq!(populated.min(field, Some(&empty_selection)), Ok(None));
        assert_eq!(populated.max(field, Some(&empty_selection)), Ok(None));
    }
}

#[test]
fn float_extrema_use_total_nan_and_signed_zero_ordering() {
    let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
    let positive_nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch([
            vec![Value::Float64(0.0)],
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(negative_nan)],
            vec![Value::Float64(positive_nan)],
        ])
        .unwrap();

    assert_float_bits(table.min("value", None).unwrap(), negative_nan.to_bits());
    assert_float_bits(table.max("value", None).unwrap(), positive_nan.to_bits());

    let zeros = table
        .scan("value", ComparisonOperator::Equal, &Value::Float64(0.0))
        .unwrap();
    assert_float_bits(
        table.min("value", Some(&zeros)).unwrap(),
        (-0.0_f64).to_bits(),
    );
    assert_float_bits(table.max("value", Some(&zeros)).unwrap(), 0.0_f64.to_bits());
}

#[test]
fn min_and_max_reject_mismatched_selections_before_reducing() {
    let mut table = all_types_table();
    table.insert_batch([row(1, 1.0, true, "one")]).unwrap();
    let selection = RowSelection::try_empty(2).unwrap();
    let expected = ReductionError::SelectionLengthMismatch {
        table_rows: 1,
        selection_rows: 2,
    };

    assert_eq!(
        table.min("integer", Some(&selection)),
        Err(expected.clone())
    );
    assert_eq!(table.max("text", Some(&selection)), Err(expected));
}

#[test]
fn min_and_max_report_missing_fields() {
    let table = all_types_table();
    let expected = ReductionError::FieldNotFound {
        name: "missing".to_owned(),
    };

    assert_eq!(table.min("missing", None), Err(expected.clone()));
    assert_eq!(table.max("missing", None), Err(expected));
}

fn all_types_table() -> Table {
    Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ])
    .unwrap()
}

fn row(integer: i64, float: f64, boolean: bool, text: &str) -> Vec<Value> {
    vec![
        Value::Int64(integer),
        Value::Float64(float),
        Value::Bool(boolean),
        Value::String(text.to_owned()),
    ]
}

fn filtered_row(integer: i64, float: f64, boolean: bool, text: &str, include: bool) -> Vec<Value> {
    let mut row = row(integer, float, boolean, text);
    row.push(Value::Bool(include));
    row
}

fn assert_extrema(
    table: &Table,
    field: &str,
    selection: Option<&RowSelection>,
    minimum: Value,
    maximum: Value,
) {
    assert_eq!(table.min(field, selection), Ok(Some(minimum)));
    assert_eq!(table.max(field, selection), Ok(Some(maximum)));
}

fn assert_float_bits(value: Option<Value>, expected: u64) {
    let Some(Value::Float64(value)) = value else {
        panic!("Float64 extrema must produce a Float64 value");
    };
    assert_eq!(value.to_bits(), expected);
}

fn assert_avg_is_nan(table: &Table) {
    let Some(Value::Float64(average)) = table.avg("value", None).unwrap() else {
        panic!("nonempty Float64 averages must produce a Float64 value");
    };
    assert!(average.is_nan());
}
