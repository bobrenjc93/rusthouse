use rusthouse::storage::{DataType, Field, InsertError, Schema, SchemaError, Table, Value};

fn all_type_fields() -> Vec<Field> {
    vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("string", DataType::String),
    ]
}

fn all_type_row() -> Vec<Value> {
    vec![
        Value::Int64(i64::MIN),
        Value::Float64(f64::NEG_INFINITY),
        Value::Bool(true),
        Value::String(String::new()),
    ]
}

#[test]
fn stores_every_supported_type_in_homogeneous_columns() {
    let mut table = Table::new(all_type_fields()).unwrap();

    table.insert(all_type_row()).unwrap();
    table
        .insert(vec![
            Value::Int64(i64::MAX),
            Value::Float64(f64::INFINITY),
            Value::Bool(false),
            Value::String("rusthouse".to_owned()),
        ])
        .unwrap();

    assert_eq!(table.row_count(), 2);
    assert_eq!(
        table.column_by_name("integer").unwrap().as_int64(),
        Some([i64::MIN, i64::MAX].as_slice())
    );
    assert_eq!(
        table.column_by_name("float").unwrap().as_float64(),
        Some([f64::NEG_INFINITY, f64::INFINITY].as_slice())
    );
    assert_eq!(
        table.column_by_name("boolean").unwrap().as_bool(),
        Some([true, false].as_slice())
    );
    assert_eq!(
        table.column_by_name("string").unwrap().as_string(),
        Some([String::new(), "rusthouse".to_owned()].as_slice())
    );
    assert!(table.columns().iter().all(|column| column.len() == 2));
}

#[test]
fn rejects_schemas_without_fields() {
    assert_eq!(Table::new(vec![]), Err(SchemaError::EmptySchema));
    assert_eq!(Schema::new(vec![]), Err(SchemaError::EmptySchema));
}

#[test]
fn rejects_empty_field_names() {
    let fields = vec![
        Field::new("valid", DataType::Int64),
        Field::new("", DataType::Bool),
    ];

    assert_eq!(
        Table::new(fields),
        Err(SchemaError::EmptyFieldName { index: 1 })
    );
}

#[test]
fn rejects_duplicate_field_names_even_when_types_differ() {
    let fields = vec![
        Field::new("duplicate", DataType::Int64),
        Field::new("middle", DataType::Bool),
        Field::new("duplicate", DataType::String),
    ];

    assert_eq!(
        Table::new(fields),
        Err(SchemaError::DuplicateFieldName {
            name: "duplicate".to_owned(),
            first_index: 0,
            duplicate_index: 2,
        })
    );
}

#[test]
fn arity_errors_do_not_mutate_the_table() {
    let mut table = Table::new(all_type_fields()).unwrap();
    table.insert(all_type_row()).unwrap();
    let original = table.clone();

    assert_eq!(
        table.insert(vec![Value::Int64(1)]),
        Err(InsertError::ArityMismatch {
            expected: 4,
            actual: 1,
        })
    );
    assert_eq!(table, original);

    let mut too_many = all_type_row();
    too_many.push(Value::Bool(false));
    assert_eq!(
        table.insert(too_many),
        Err(InsertError::ArityMismatch {
            expected: 4,
            actual: 5,
        })
    );
    assert_eq!(table, original);
}

#[test]
fn type_errors_for_every_column_do_not_mutate_the_table() {
    let mut table = Table::new(all_type_fields()).unwrap();
    table.insert(all_type_row()).unwrap();
    let original = table.clone();
    let invalid_values = [
        (0, Value::Float64(1.0), DataType::Int64, DataType::Float64),
        (1, Value::Bool(false), DataType::Float64, DataType::Bool),
        (
            2,
            Value::String("false".to_owned()),
            DataType::Bool,
            DataType::String,
        ),
        (3, Value::Int64(1), DataType::String, DataType::Int64),
    ];

    for (column, invalid_value, expected, actual) in invalid_values {
        let mut row = all_type_row();
        row[column] = invalid_value;

        assert_eq!(
            table.insert(row),
            Err(InsertError::TypeMismatch {
                column,
                field: all_type_fields()[column].name().to_owned(),
                expected,
                actual,
            })
        );
        assert_eq!(table, original);
    }
}
