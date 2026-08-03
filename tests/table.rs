use rusthouse::{DataType, Field, Table, TableError, Value};

fn full_schema() -> Vec<Field> {
    vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ]
}

fn first_row() -> Vec<Value> {
    vec![
        Value::Int64(i64::MIN),
        Value::Float64(f64::MIN),
        Value::Bool(false),
        Value::String(String::new()),
    ]
}

#[test]
fn stores_boundary_values_in_typed_columns() {
    let mut table = Table::with_row_limit(full_schema(), 2).unwrap();

    assert_eq!(
        table
            .insert_batch(vec![
                first_row(),
                vec![
                    Value::Int64(i64::MAX),
                    Value::Float64(f64::MAX),
                    Value::Bool(true),
                    Value::String("rusthouse".to_owned()),
                ],
            ])
            .unwrap(),
        2
    );

    assert_eq!(table.len(), 2);
    assert_eq!(table.int64_column("integer").unwrap(), [i64::MIN, i64::MAX]);
    assert_eq!(table.float64_column("float").unwrap(), [f64::MIN, f64::MAX]);
    assert_eq!(
        table.bool_column("boolean").unwrap().collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(table.string_column("text").unwrap(), ["", "rusthouse"]);
}

#[test]
fn rejects_invalid_schemas_deterministically() {
    assert_eq!(Table::new(vec![]).unwrap_err(), TableError::EmptySchema);
    assert_eq!(
        Table::new(vec![Field::new("", DataType::Int64)]).unwrap_err(),
        TableError::EmptyFieldName { index: 0 }
    );
    assert_eq!(
        Table::new(vec![
            Field::new("id", DataType::Int64),
            Field::new("id", DataType::String),
        ])
        .unwrap_err(),
        TableError::DuplicateField {
            name: "id".to_owned()
        }
    );
}

#[test]
fn row_width_failure_rolls_back_the_whole_batch() {
    let mut table = Table::new(full_schema()).unwrap();
    table.insert_batch(vec![first_row()]).unwrap();

    let error = table
        .insert_batch(vec![
            vec![
                Value::Int64(1),
                Value::Float64(1.0),
                Value::Bool(true),
                Value::String("valid".to_owned()),
            ],
            vec![Value::Int64(2), Value::Float64(2.0)],
        ])
        .unwrap_err();

    assert_eq!(
        error,
        TableError::RowWidthMismatch {
            row: 1,
            expected: 4,
            actual: 2,
        }
    );
    assert_first_row_is_unchanged(&table);
}

#[test]
fn type_failure_rolls_back_the_whole_batch() {
    let mut table = Table::new(full_schema()).unwrap();
    table.insert_batch(vec![first_row()]).unwrap();

    let error = table
        .insert_batch(vec![
            vec![
                Value::Int64(1),
                Value::Float64(1.0),
                Value::Bool(true),
                Value::String("valid".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::Int64(3),
            ],
        ])
        .unwrap_err();

    assert_eq!(
        error,
        TableError::TypeMismatch {
            row: 1,
            column: 3,
            field: "text".to_owned(),
            expected: DataType::String,
            actual: DataType::Int64,
        }
    );
    assert_first_row_is_unchanged(&table);
}

#[test]
fn exact_row_limit_succeeds_and_excess_rolls_back() {
    let mut table = Table::with_row_limit(vec![Field::new("id", DataType::Int64)], 2).unwrap();

    assert_eq!(table.insert_batch(Vec::<Vec<Value>>::new()).unwrap(), 0);
    assert_eq!(
        table
            .insert_batch(vec![vec![Value::Int64(1)], vec![Value::Int64(2)]])
            .unwrap(),
        2
    );
    assert_eq!(
        table.insert_batch(vec![vec![Value::Int64(3)]]),
        Err(TableError::RowLimitExceeded {
            limit: 2,
            current: 2,
        })
    );
    assert_eq!(table.int64_column("id").unwrap(), [1, 2]);
}

#[test]
fn bounded_insert_stops_an_unbounded_producer_without_mutation() {
    let mut table = Table::with_row_limit(vec![Field::new("id", DataType::Int64)], 2).unwrap();
    let rows = (0_i64..).map(|value| vec![Value::Int64(value)]);

    assert_eq!(
        table.insert_batch(rows),
        Err(TableError::RowLimitExceeded {
            limit: 2,
            current: 0,
        })
    );
    assert!(table.is_empty());
    assert!(table.int64_column("id").unwrap().is_empty());

    let mut zero_limit = Table::with_row_limit(vec![Field::new("id", DataType::Int64)], 0).unwrap();
    assert_eq!(
        zero_limit.insert_batch(vec![vec![Value::Int64(1)]]),
        Err(TableError::RowLimitExceeded {
            limit: 0,
            current: 0,
        })
    );
}

#[test]
fn typed_accessors_report_lookup_and_type_errors() {
    let table = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();

    assert_eq!(
        table.string_column("id"),
        Err(TableError::ColumnTypeMismatch {
            field: "id".to_owned(),
            expected: DataType::String,
            actual: DataType::Int64,
        })
    );
    assert_eq!(
        table.int64_column("missing"),
        Err(TableError::FieldNotFound {
            name: "missing".to_owned()
        })
    );
}

fn assert_first_row_is_unchanged(table: &Table) {
    assert_eq!(table.len(), 1);
    assert_eq!(table.int64_column("integer").unwrap(), [i64::MIN]);
    assert_eq!(table.float64_column("float").unwrap(), [f64::MIN]);
    assert_eq!(
        table.bool_column("boolean").unwrap().collect::<Vec<_>>(),
        [false]
    );
    assert_eq!(table.string_column("text").unwrap(), [""]);
}
