use rusthouse::{Column, DataType, Field, InsertError, ScalarValue, Schema, SchemaError, Table};

fn four_type_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Int64),
        Field::new("score", DataType::Float64),
        Field::new("active", DataType::Bool),
        Field::new("name", DataType::String),
    ])
    .expect("test schema is valid")
}

fn row(id: i64, score: f64, active: bool, name: &str) -> Vec<ScalarValue> {
    vec![
        ScalarValue::Int64(id),
        ScalarValue::Float64(score),
        ScalarValue::Bool(active),
        ScalarValue::String(name.to_owned()),
    ]
}

#[test]
fn schemas_reject_empty_and_duplicate_fields() {
    assert_eq!(Schema::new(vec![]), Err(SchemaError::EmptySchema));
    assert_eq!(
        Schema::new(vec![Field::new("  ", DataType::Int64)]),
        Err(SchemaError::EmptyFieldName { index: 0 })
    );
    assert_eq!(
        Schema::new(vec![
            Field::new("UserId", DataType::Int64),
            Field::new("userid", DataType::String),
        ]),
        Err(SchemaError::DuplicateFieldName {
            name: "userid".to_owned(),
        })
    );
}

#[test]
fn rows_are_transposed_into_typed_columns() {
    let mut table = Table::new(four_type_schema(), 2);

    table
        .insert(row(7, 2.5, true, "Ada"))
        .expect("first row fits");
    table
        .insert(row(9, -1.25, false, "Lin"))
        .expect("second row fits");

    assert_eq!(table.len(), 2);
    assert_eq!(
        table.columns(),
        [
            Column::Int64(vec![7, 9]),
            Column::Float64(vec![2.5, -1.25]),
            Column::Bool(vec![true, false]),
            Column::String(vec!["Ada".to_owned(), "Lin".to_owned()]),
        ]
    );
    assert_eq!(
        table.column_by_name("NAME").and_then(Column::as_string),
        Some(["Ada".to_owned(), "Lin".to_owned()].as_slice())
    );
}

#[test]
fn rejected_inserts_are_atomic() {
    let mut table = Table::new(four_type_schema(), 1);
    table
        .insert(row(7, 2.5, true, "Ada"))
        .expect("initial row fits");
    let original = table.clone();

    assert_eq!(
        table.insert(vec![]),
        Err(InsertError::ArityMismatch {
            expected: 4,
            actual: 0,
        })
    );
    assert_eq!(table, original);

    assert_eq!(
        table.insert(vec![
            ScalarValue::Int64(8),
            ScalarValue::Float64(3.0),
            ScalarValue::Bool(false),
            ScalarValue::Int64(99),
        ]),
        Err(InsertError::TypeMismatch {
            column_index: 3,
            column_name: "name".to_owned(),
            expected: DataType::String,
            actual: DataType::Int64,
        })
    );
    assert_eq!(table, original);

    assert_eq!(
        table.insert(row(8, 3.0, false, "Grace")),
        Err(InsertError::RowLimitExceeded { limit: 1 })
    );
    assert_eq!(table, original);
}

#[test]
fn zero_row_limit_rejects_the_first_insert_without_mutation() {
    let mut table = Table::new(four_type_schema(), 0);

    assert_eq!(
        table.insert(row(1, 1.0, true, "bounded")),
        Err(InsertError::RowLimitExceeded { limit: 0 })
    );
    assert!(table.is_empty());
    assert!(table.columns().iter().all(Column::is_empty));
}
