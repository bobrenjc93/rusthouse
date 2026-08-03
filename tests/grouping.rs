use rusthouse::{
    ComparisonOperator, DataType, Field, GroupedCount, GroupedCountError, RowSelection, Table,
    Value,
};

#[test]
fn groups_every_physical_type_in_deterministic_key_order() {
    let mut table = all_types_table();
    table
        .insert_batch([
            row(4, 2.5, true, "pear"),
            row(-2, -1.0, false, "apple"),
            row(4, 2.5, false, "pear"),
            row(0, 9.0, true, "banana"),
            row(-2, -1.0, false, "apple"),
        ])
        .unwrap();

    assert_groups(
        table.grouped_count("integer", None, 3).unwrap(),
        &[
            (Value::Int64(-2), 2),
            (Value::Int64(0), 1),
            (Value::Int64(4), 2),
        ],
    );
    assert_groups(
        table.grouped_count("float", None, 3).unwrap(),
        &[
            (Value::Float64(-1.0), 2),
            (Value::Float64(2.5), 2),
            (Value::Float64(9.0), 1),
        ],
    );
    assert_groups(
        table.grouped_count("boolean", None, 2).unwrap(),
        &[(Value::Bool(false), 3), (Value::Bool(true), 2)],
    );
    assert_groups(
        table.grouped_count("text", None, 3).unwrap(),
        &[
            (Value::String("apple".to_owned()), 2),
            (Value::String("banana".to_owned()), 1),
            (Value::String("pear".to_owned()), 2),
        ],
    );
}

#[test]
fn grouping_is_selection_aware_for_every_physical_type() {
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
            filtered_row(-10, -10.0, false, "excluded", false),
            filtered_row(2, 3.0, true, "lime", true),
            filtered_row(2, 3.0, false, "plum", true),
            filtered_row(8, 1.0, true, "lime", true),
            filtered_row(99, 99.0, false, "excluded", false),
        ])
        .unwrap();
    let selection = table
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_groups(
        table.grouped_count("integer", Some(&selection), 2).unwrap(),
        &[(Value::Int64(2), 2), (Value::Int64(8), 1)],
    );
    assert_groups(
        table.grouped_count("float", Some(&selection), 2).unwrap(),
        &[(Value::Float64(1.0), 1), (Value::Float64(3.0), 2)],
    );
    assert_groups(
        table.grouped_count("boolean", Some(&selection), 2).unwrap(),
        &[(Value::Bool(false), 1), (Value::Bool(true), 2)],
    );
    assert_groups(
        table.grouped_count("text", Some(&selection), 2).unwrap(),
        &[
            (Value::String("lime".to_owned()), 2),
            (Value::String("plum".to_owned()), 1),
        ],
    );
}

#[test]
fn empty_tables_and_empty_selections_return_no_groups() {
    let empty = all_types_table();
    let empty_selection = RowSelection::try_empty(0).unwrap();
    for field in ["integer", "float", "boolean", "text"] {
        assert!(empty.grouped_count(field, None, 0).unwrap().is_empty());
        assert!(
            empty
                .grouped_count(field, Some(&empty_selection), 0)
                .unwrap()
                .is_empty()
        );
    }

    let mut populated = all_types_table();
    populated
        .insert_batch([row(1, 1.0, true, "present")])
        .unwrap();
    let no_rows = RowSelection::try_empty(1).unwrap();
    for field in ["integer", "float", "boolean", "text"] {
        assert!(
            populated
                .grouped_count(field, Some(&no_rows), 0)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn duplicate_heavy_input_keeps_checked_exact_counts() {
    let mut table = Table::new(vec![Field::new("key", DataType::Int64)]).unwrap();
    table
        .insert_batch((0..25_000).map(|row| vec![Value::Int64(row % 5)]))
        .unwrap();

    assert_groups(
        table.grouped_count("key", None, 5).unwrap(),
        &[
            (Value::Int64(0), 5_000),
            (Value::Int64(1), 5_000),
            (Value::Int64(2), 5_000),
            (Value::Int64(3), 5_000),
            (Value::Int64(4), 5_000),
        ],
    );
}

#[test]
fn reverse_ordered_high_cardinality_input_sorts_after_accumulation() {
    const ROWS: i64 = 100_000;

    let mut table = Table::new(vec![Field::new("key", DataType::Int64)]).unwrap();
    table
        .insert_batch((0..ROWS).rev().map(|key| vec![Value::Int64(key)]))
        .unwrap();

    let groups = table.grouped_count("key", None, ROWS as usize).unwrap();
    assert_eq!(groups.len(), ROWS as usize);
    for (expected, group) in groups.iter().enumerate() {
        assert_eq!(group.value(), &Value::Int64(expected as i64));
        assert_eq!(group.count(), 1);
    }
}

#[test]
fn permissive_limit_does_not_leave_low_cardinality_result_row_sized() {
    const ROWS: usize = 100_000;

    let mut table = Table::new(vec![Field::new("key", DataType::Bool)]).unwrap();
    table
        .insert_batch((0..ROWS).map(|row| vec![Value::Bool(row % 2 == 0)]))
        .unwrap();

    let groups = table.grouped_count("key", None, usize::MAX).unwrap();
    assert!(groups.capacity() < ROWS);
    assert_groups(
        groups,
        &[
            (Value::Bool(false), ROWS / 2),
            (Value::Bool(true), ROWS / 2),
        ],
    );
}

#[test]
fn float_groups_use_total_order_for_nans_and_signed_zero() {
    let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
    let positive_signaling_nan = f64::from_bits(0x7ff0_0000_0000_0001);
    let positive_nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let other_positive_nan = f64::from_bits(0x7ff8_0000_0000_0002);
    let values = [
        positive_nan,
        0.0,
        -0.0,
        negative_nan,
        f64::INFINITY,
        f64::NEG_INFINITY,
        positive_nan,
        -0.0,
        0.0,
        other_positive_nan,
        positive_signaling_nan,
    ];
    let mut table = Table::new(vec![Field::new("key", DataType::Float64)]).unwrap();
    table
        .insert_batch(values.map(|value| vec![Value::Float64(value)]))
        .unwrap();

    let groups = table.grouped_count("key", None, 8).unwrap();
    let actual = groups
        .iter()
        .map(|group| {
            let Value::Float64(value) = group.value() else {
                panic!("Float64 grouping must return Float64 values");
            };
            (value.to_bits(), group.count())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            (negative_nan.to_bits(), 1),
            (f64::NEG_INFINITY.to_bits(), 1),
            ((-0.0_f64).to_bits(), 2),
            (0.0_f64.to_bits(), 2),
            (f64::INFINITY.to_bits(), 1),
            (positive_signaling_nan.to_bits(), 1),
            (positive_nan.to_bits(), 2),
            (other_positive_nan.to_bits(), 1),
        ]
    );

    assert_eq!(groups[0], groups[0]);
}

#[test]
fn group_limit_is_exact_and_never_truncates_results() {
    let mut table = Table::new(vec![Field::new("key", DataType::String)]).unwrap();
    table
        .insert_batch([
            vec![Value::from("beta")],
            vec![Value::from("alpha")],
            vec![Value::from("gamma")],
            vec![Value::from("alpha")],
        ])
        .unwrap();

    assert_eq!(table.grouped_count("key", None, 3).unwrap().len(), 3);
    assert_eq!(
        table.grouped_count("key", None, 2),
        Err(GroupedCountError::GroupLimitExceeded {
            field: "key".to_owned(),
            limit: 2,
        })
    );
    assert_eq!(
        table.grouped_count("key", None, 0),
        Err(GroupedCountError::GroupLimitExceeded {
            field: "key".to_owned(),
            limit: 0,
        })
    );
}

#[test]
fn grouped_string_byte_limit_is_total_exact_and_counts_distinct_keys_once() {
    let mut table = Table::new(vec![Field::new("key", DataType::String)]).unwrap();
    table
        .insert_batch([
            vec![Value::from("alpha")],
            vec![Value::from("beta")],
            vec![Value::from("alpha")],
        ])
        .unwrap();

    let exact = table
        .grouped_count_with_string_limit("key", None, 2, 9)
        .unwrap();
    assert_groups(
        exact,
        &[(Value::from("alpha"), 2), (Value::from("beta"), 1)],
    );
    assert_eq!(
        table.grouped_count_with_string_limit("key", None, 2, 8),
        Err(GroupedCountError::StringResultTooLarge {
            field: "key".to_owned(),
            limit: 8,
            required: 9,
        })
    );

    let no_rows = RowSelection::try_empty(table.len()).unwrap();
    assert!(
        table
            .grouped_count_with_string_limit("key", Some(&no_rows), 0, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn grouping_reports_validation_errors_before_work() {
    let mut table = Table::new(vec![Field::new("key", DataType::Int64)]).unwrap();
    table.insert_batch([vec![Value::Int64(1)]]).unwrap();
    let wrong_length = RowSelection::try_empty(2).unwrap();

    assert_eq!(
        table.grouped_count("key", Some(&wrong_length), 1),
        Err(GroupedCountError::SelectionLengthMismatch {
            table_rows: 1,
            selection_rows: 2,
        })
    );
    assert_eq!(
        table.grouped_count("missing", None, 1),
        Err(GroupedCountError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
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
    let mut values = row(integer, float, boolean, text);
    values.push(Value::Bool(include));
    values
}

fn assert_groups(actual: Vec<GroupedCount>, expected: &[(Value, usize)]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, (expected_value, expected_count)) in actual.iter().zip(expected) {
        assert_eq!(actual.value(), expected_value);
        assert_eq!(actual.count(), *expected_count);
    }
}
