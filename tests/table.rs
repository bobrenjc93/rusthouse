use rusthouse::{Column, ColumnSchema, DataType, Schema, Table, TableError, TableLimits, Value};

fn schema(columns: &[(&str, DataType)]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|(name, data_type)| ColumnSchema::new(*name, *data_type))
            .collect(),
    )
    .unwrap()
}

fn limits(max_columns: usize, max_rows: usize, max_string_bytes: usize) -> TableLimits {
    TableLimits {
        max_columns,
        max_rows,
        max_string_bytes,
    }
}

#[test]
fn stores_every_type_in_name_addressable_columns() {
    let schema = schema(&[
        ("id", DataType::Int64),
        ("score", DataType::Float64),
        ("active", DataType::Bool),
        ("label", DataType::String),
    ]);
    let mut table = Table::new(schema, limits(4, 2, 6)).unwrap();

    table
        .insert_batch(vec![
            vec![
                Value::Int64(-4),
                Value::Float64(1.25),
                Value::Bool(true),
                Value::String("red".into()),
            ],
            vec![
                Value::Int64(9),
                Value::Float64(-0.5),
                Value::Bool(false),
                Value::String("sky".into()),
            ],
        ])
        .unwrap();

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.string_bytes(), 6);
    assert_eq!(
        table.schema().column("score").unwrap().data_type(),
        DataType::Float64
    );
    assert_eq!(table.column("id").unwrap().as_int64(), Some(&[-4, 9][..]));
    assert_eq!(
        table.column("score").unwrap().as_float64(),
        Some(&[1.25, -0.5][..])
    );
    assert_eq!(
        table.column("active").unwrap().as_bool(),
        Some(&[true, false][..])
    );
    assert_eq!(
        table.column("label").unwrap(),
        &Column::String(vec!["red".into(), "sky".into()])
    );
    assert!(table.column("missing").is_none());
}

#[test]
fn rejects_duplicate_column_names() {
    let error = Schema::new(vec![
        ColumnSchema::new("same", DataType::Int64),
        ColumnSchema::new("same", DataType::String),
    ])
    .unwrap_err();

    assert_eq!(
        error,
        TableError::DuplicateColumnName {
            name: "same".into()
        }
    );
}

#[test]
fn enforces_column_limit_at_the_boundary() {
    Table::new(
        schema(&[("left", DataType::Int64), ("right", DataType::Bool)]),
        limits(2, 0, 0),
    )
    .unwrap();

    let error = Table::new(
        schema(&[("left", DataType::Int64), ("right", DataType::Bool)]),
        limits(1, 0, 0),
    )
    .unwrap_err();
    assert_eq!(
        error,
        TableError::ColumnLimitExceeded {
            limit: 1,
            attempted: 2
        }
    );
}

#[test]
fn enforces_row_and_utf8_byte_limits_at_the_boundary() {
    let mut table = Table::new(schema(&[("text", DataType::String)]), limits(1, 2, 4)).unwrap();
    table
        .insert_batch(vec![
            vec![Value::String("ab".into())],
            vec![Value::String("é".into())],
        ])
        .unwrap();
    assert_eq!(table.string_bytes(), 4);

    let snapshot = table.clone();
    assert_eq!(
        table.insert_row(vec![Value::String(String::new())]),
        Err(TableError::RowLimitExceeded {
            limit: 2,
            attempted: 3
        })
    );
    assert_eq!(table, snapshot);

    let mut byte_limited =
        Table::new(schema(&[("text", DataType::String)]), limits(1, 2, 3)).unwrap();
    let error = byte_limited
        .insert_batch(vec![
            vec![Value::String("ab".into())],
            vec![Value::String("é".into())],
        ])
        .unwrap_err();
    assert_eq!(
        error,
        TableError::StringByteLimitExceeded {
            limit: 3,
            attempted: 4
        }
    );
    assert!(byte_limited.is_empty());
}

#[test]
fn rejects_row_shape_without_mutating_the_table() {
    let mut table = Table::new(
        schema(&[("id", DataType::Int64), ("ok", DataType::Bool)]),
        limits(2, 3, 0),
    )
    .unwrap();

    let error = table.insert_row(vec![Value::Int64(1)]).unwrap_err();
    assert_eq!(
        error,
        TableError::RowShapeMismatch {
            row: 0,
            expected: 2,
            actual: 1
        }
    );
    assert!(table.is_empty());
    assert!(table.columns().iter().all(Column::is_empty));
}

#[test]
fn rejects_a_late_type_error_without_partially_appending_the_batch() {
    let mut table = Table::new(
        schema(&[("id", DataType::Int64), ("name", DataType::String)]),
        limits(2, 4, 20),
    )
    .unwrap();
    table
        .insert_row(vec![Value::Int64(1), Value::String("kept".into())])
        .unwrap();
    let snapshot = table.clone();

    let error = table
        .insert_batch(vec![
            vec![Value::Int64(2), Value::String("valid".into())],
            vec![Value::Bool(false), Value::String("invalid".into())],
        ])
        .unwrap_err();

    assert_eq!(
        error,
        TableError::TypeMismatch {
            row: 1,
            column: 0,
            column_name: "id".into(),
            expected: DataType::Int64,
            actual: DataType::Bool,
        }
    );
    assert_eq!(table, snapshot);
}
