use rusthouse::{ComparisonOperator, DataType, Field, RowSelection, ScanError, Table, Value};

const ALL_OPERATORS: [ComparisonOperator; 6] = [
    ComparisonOperator::Equal,
    ComparisonOperator::NotEqual,
    ComparisonOperator::LessThan,
    ComparisonOperator::LessThanOrEqual,
    ComparisonOperator::GreaterThan,
    ComparisonOperator::GreaterThanOrEqual,
];

#[test]
fn scans_every_operator_across_all_physical_types() {
    let mut table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ])
    .unwrap();
    table
        .insert_batch(vec![
            vec![
                Value::Int64(-2),
                Value::Float64(-2.0),
                Value::Bool(false),
                Value::String("ant".to_owned()),
            ],
            vec![
                Value::Int64(0),
                Value::Float64(0.0),
                Value::Bool(true),
                Value::String("bee".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::String("cat".to_owned()),
            ],
        ])
        .unwrap();

    let ordered_expectations: [&[usize]; 6] = [&[1], &[0, 2], &[0], &[0, 1], &[2], &[1, 2]];
    let bool_expectations: [&[usize]; 6] = [&[1], &[0, 2], &[0, 2], &[0, 1, 2], &[], &[1]];

    for (operator, expected) in ALL_OPERATORS.into_iter().zip(ordered_expectations) {
        assert_scan(&table, "integer", operator, &Value::Int64(0), expected);
        assert_scan(&table, "float", operator, &Value::Float64(0.0), expected);
        assert_scan(
            &table,
            "text",
            operator,
            &Value::String("bee".to_owned()),
            expected,
        );
    }
    for (operator, expected) in ALL_OPERATORS.into_iter().zip(bool_expectations) {
        assert_scan(&table, "boolean", operator, &Value::Bool(true), expected);
    }
}

#[test]
fn float_scans_follow_ieee_nan_and_signed_zero_semantics() {
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch(vec![
            vec![Value::Float64(f64::NAN)],
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(1.0)],
        ])
        .unwrap();

    let zero_expectations: [&[usize]; 6] = [&[1, 2], &[0, 3], &[], &[1, 2], &[3], &[1, 2, 3]];
    for (operator, expected) in ALL_OPERATORS.into_iter().zip(zero_expectations) {
        assert_scan(&table, "value", operator, &Value::Float64(0.0), expected);
    }

    for operator in [
        ComparisonOperator::Equal,
        ComparisonOperator::LessThan,
        ComparisonOperator::LessThanOrEqual,
        ComparisonOperator::GreaterThan,
        ComparisonOperator::GreaterThanOrEqual,
    ] {
        assert_scan(&table, "value", operator, &Value::Float64(f64::NAN), &[]);
    }
    assert_scan(
        &table,
        "value",
        ComparisonOperator::NotEqual,
        &Value::Float64(f64::NAN),
        &[0, 1, 2, 3],
    );
}

#[test]
fn scans_empty_columns_without_allocating_bitmap_storage() {
    let table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ])
    .unwrap();

    for (field, literal) in [
        ("integer", Value::Int64(0)),
        ("float", Value::Float64(0.0)),
        ("boolean", Value::Bool(false)),
        ("text", Value::String(String::new())),
    ] {
        let selection = table
            .scan(field, ComparisonOperator::Equal, &literal)
            .unwrap();
        assert!(selection.is_empty());
        assert!(selection.as_bytes().is_empty());
        assert_eq!(selection.selected_count(), 0);
        assert_eq!(selection.iter().next(), None);
    }
}

#[test]
fn reports_missing_fields_and_literal_type_mismatches_explicitly() {
    let table = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();

    assert_eq!(
        table.scan("missing", ComparisonOperator::Equal, &Value::Int64(1)),
        Err(ScanError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
    assert_eq!(
        table.scan(
            "id",
            ComparisonOperator::Equal,
            &Value::String("1".to_owned()),
        ),
        Err(ScanError::TypeMismatch {
            field: "id".to_owned(),
            column_type: DataType::Int64,
            literal_type: DataType::String,
        })
    );
}

#[test]
fn row_selection_is_packed_at_byte_boundaries_and_bounds_checked() {
    for (row_count, required_bytes) in [(0, 0), (1, 1), (8, 1), (9, 2), (16, 2), (17, 3)] {
        let selection = RowSelection::try_empty(row_count).unwrap();
        assert_eq!(selection.len(), row_count);
        assert_eq!(selection.as_bytes(), vec![0; required_bytes]);
        assert_eq!(selection.iter().len(), row_count);
        assert_eq!(selection.get(row_count), None);
    }

    let error = RowSelection::try_empty(usize::MAX).unwrap_err();
    assert_eq!(error.row_count(), usize::MAX);
    assert_eq!(error.required_bytes(), usize::MAX / u8::BITS as usize + 1);
}

#[test]
fn scan_sets_only_in_range_bits_in_the_final_byte() {
    let mut table = Table::new(vec![Field::new("keep", DataType::Bool)]).unwrap();
    table
        .insert_batch((0..9).map(|_| vec![Value::Bool(true)]))
        .unwrap();

    let selection = table
        .scan("keep", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(selection.as_bytes(), &[u8::MAX, 1]);
    assert_eq!(selection.selected_count(), 9);
    assert_eq!(selection.iter().collect::<Vec<_>>(), vec![true; 9]);
    assert_eq!(
        selection.selected_rows().rev().collect::<Vec<_>>(),
        (0..9).rev().collect::<Vec<_>>()
    );
}

fn assert_scan(
    table: &Table,
    field: &str,
    operator: ComparisonOperator,
    literal: &Value,
    expected: &[usize],
) {
    let selection = table.scan(field, operator, literal).unwrap();
    assert_eq!(selection.len(), table.len());
    assert_eq!(selection.selected_count(), expected.len());
    assert_eq!(selection.selected_rows().collect::<Vec<_>>(), expected);
    for row in 0..table.len() {
        assert_eq!(selection.get(row), Some(expected.contains(&row)));
    }
}
