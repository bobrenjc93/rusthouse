use rusthouse::{Column, ColumnDef, DataType, StorageError, Table, Value};

fn full_schema() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("id", DataType::Int64),
        ColumnDef::new("score", DataType::Float64),
        ColumnDef::new("active", DataType::Bool),
        ColumnDef::new("label", DataType::String),
    ]
}

fn sample_row(id: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Float64(1.5),
        Value::Bool(true),
        Value::String("alpha".to_owned()),
    ]
}

#[test]
fn stores_each_type_in_its_own_physical_vector() {
    let mut table = Table::new("events", full_schema()).expect("valid schema");
    table.insert_row(sample_row(7)).expect("valid row");

    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[7]));
    assert!(matches!(&table.columns()[1], Column::Float64(values) if values == &[1.5]));
    assert!(matches!(&table.columns()[2], Column::Bool(values) if values == &[true]));
    assert!(matches!(&table.columns()[3], Column::String(values) if values == &["alpha"]));
    assert_eq!(table.row_count(), 1);
}

#[test]
fn validates_schema_before_constructing_columns() {
    assert_eq!(Table::new("events", vec![]), Err(StorageError::EmptySchema));
    assert_eq!(
        Table::new("events", vec![ColumnDef::new(" ", DataType::Int64)]),
        Err(StorageError::EmptyColumnName { index: 0 })
    );
    assert_eq!(
        Table::new(
            "events",
            vec![
                ColumnDef::new("UserId", DataType::Int64),
                ColumnDef::new("userid", DataType::String),
            ],
        ),
        Err(StorageError::DuplicateColumn {
            name: "userid".to_owned()
        })
    );
}

#[test]
fn enforces_configured_row_limit_without_mutation() {
    let mut table = Table::with_row_limit("events", full_schema(), 1).expect("valid schema");
    table.insert_row(sample_row(1)).expect("row below limit");
    let before = table.clone();

    let error = table.insert_row(sample_row(2)).expect_err("row over limit");

    assert_eq!(
        error,
        StorageError::RowLimitExceeded {
            table: "events".to_owned(),
            limit: 1,
        }
    );
    assert_eq!(table, before);
}

#[test]
fn every_failed_insert_is_atomic() {
    let mut table = Table::with_row_limit("events", full_schema(), 10).expect("valid schema");
    table.insert_row(sample_row(1)).expect("valid row");

    let invalid_rows = [
        (
            vec![Value::Int64(2)],
            StorageError::RowLength {
                table: "events".to_owned(),
                expected: 4,
                actual: 1,
            },
        ),
        (
            vec![
                Value::Int64(2),
                Value::Float64(2.0),
                Value::String("not a bool".to_owned()),
                Value::String("beta".to_owned()),
            ],
            StorageError::TypeMismatch {
                table: "events".to_owned(),
                column: "active".to_owned(),
                expected: DataType::Bool,
                actual: DataType::String,
            },
        ),
        (
            vec![
                Value::Int64(2),
                Value::Float64(f64::NAN),
                Value::Bool(false),
                Value::String("beta".to_owned()),
            ],
            StorageError::NonFiniteFloat {
                table: "events".to_owned(),
                column: "score".to_owned(),
            },
        ),
        (
            vec![
                Value::Int64(2),
                Value::Float64(f64::INFINITY),
                Value::Bool(false),
                Value::String("beta".to_owned()),
            ],
            StorageError::NonFiniteFloat {
                table: "events".to_owned(),
                column: "score".to_owned(),
            },
        ),
    ];

    for (row, expected_error) in invalid_rows {
        let before = table.clone();
        let error = table.insert_row(row).expect_err("invalid row");
        assert_eq!(error, expected_error);
        assert_eq!(table, before);
        assert!(
            table
                .columns()
                .iter()
                .all(|column| column.len() == table.row_count())
        );
    }
}

#[test]
fn type_errors_preserve_typed_expected_and_actual_values() {
    let table = Table::new("events", full_schema()).expect("valid schema");
    let error = table
        .validate_row(&[
            Value::String("wrong".to_owned()),
            Value::Float64(1.0),
            Value::Bool(true),
            Value::String("label".to_owned()),
        ])
        .expect_err("wrong type");

    assert_eq!(
        error,
        StorageError::TypeMismatch {
            table: "events".to_owned(),
            column: "id".to_owned(),
            expected: DataType::Int64,
            actual: DataType::String,
        }
    );
}
