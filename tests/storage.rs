use rusthouse::{ColumnSchema, DataType, InsertError, Schema, SchemaError, Table, Value, ValueRef};

fn analytics_schema() -> Schema {
    Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64),
        ColumnSchema::new("score", DataType::Float64),
        ColumnSchema::new("active", DataType::Bool),
        ColumnSchema::new("label", DataType::String),
    ])
    .unwrap()
}

fn populated_table() -> Table {
    let mut table = Table::new(analytics_schema(), 2);
    table
        .insert_row(vec![
            Value::Int64(7),
            Value::Float64(2.5),
            Value::Bool(true),
            Value::String("north".to_owned()),
        ])
        .unwrap();
    table
        .insert_row(vec![
            Value::Int64(11),
            Value::Float64(8.75),
            Value::Bool(false),
            Value::String("south".to_owned()),
        ])
        .unwrap();
    table
}

#[test]
fn inserts_and_accesses_multiple_typed_rows() {
    let table = populated_table();

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.row_limit(), 2);
    assert_eq!(table.column("id").unwrap().as_int64(), Some(&[7, 11][..]));
    assert_eq!(
        table.column("score").unwrap().as_float64(),
        Some(&[2.5, 8.75][..])
    );
    assert_eq!(
        table.column("active").unwrap().as_bool(),
        Some(&[true, false][..])
    );
    assert_eq!(
        table.column("label").unwrap().as_string().unwrap(),
        &["north".to_owned(), "south".to_owned()]
    );
    assert_eq!(table.value(1, 0), Some(ValueRef::Int64(11)));
    assert_eq!(table.value(0, 3), Some(ValueRef::String("north")));
    assert_eq!(table.value(2, 0), None);
    assert_eq!(table.value(0, 4), None);
}

#[test]
fn rejects_duplicate_column_names() {
    let error = Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64),
        ColumnSchema::new("id", DataType::String),
    ])
    .unwrap_err();

    assert_eq!(
        error,
        SchemaError::DuplicateColumn {
            name: "id".to_owned()
        }
    );
}

#[test]
fn rejects_wrong_row_shape_without_mutation() {
    let mut table = Table::new(analytics_schema(), 10);

    let error = table
        .insert_row(vec![Value::Int64(1), Value::Float64(1.0)])
        .unwrap_err();

    assert_eq!(
        error,
        InsertError::ArityMismatch {
            expected: 4,
            actual: 2
        }
    );
    assert_table_is_empty(&table);
}

#[test]
fn rejects_wrong_value_type_without_partial_mutation() {
    let mut table = Table::new(analytics_schema(), 10);

    let error = table
        .insert_row(vec![
            Value::Int64(1),
            Value::Float64(3.0),
            Value::String("not a bool".to_owned()),
            Value::String("west".to_owned()),
        ])
        .unwrap_err();

    assert_eq!(
        error,
        InsertError::TypeMismatch {
            column_index: 2,
            column_name: "active".to_owned(),
            expected: DataType::Bool,
            actual: DataType::String,
        }
    );
    assert_table_is_empty(&table);
}

#[test]
fn rejects_rows_beyond_capacity_without_mutation() {
    let mut table = populated_table();

    let error = table
        .insert_row(vec![
            Value::Int64(13),
            Value::Float64(1.25),
            Value::Bool(true),
            Value::String("east".to_owned()),
        ])
        .unwrap_err();

    assert_eq!(error, InsertError::RowLimitExceeded { limit: 2 });
    assert_eq!(table.row_count(), 2);
    assert_eq!(table.column("id").unwrap().as_int64(), Some(&[7, 11][..]));
    assert_eq!(
        table.column("label").unwrap().as_string().unwrap(),
        &["north".to_owned(), "south".to_owned()]
    );
    assert!(table.columns().iter().all(|column| column.len() == 2));
}

fn assert_table_is_empty(table: &Table) {
    assert_eq!(table.row_count(), 0);
    assert!(table.columns().iter().all(|column| column.is_empty()));
}
