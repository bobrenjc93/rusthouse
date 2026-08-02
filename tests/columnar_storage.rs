use rusthouse::{
    BatchInsertError, Column, ColumnSchema, DataType, InsertError, Schema, SchemaError, Table,
    Value,
};

fn all_types_schema() -> Schema {
    Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64),
        ColumnSchema::new("score", DataType::Float64),
        ColumnSchema::new("active", DataType::Bool),
        ColumnSchema::new("name", DataType::String),
    ])
    .expect("test schema is valid")
}

#[test]
fn rejects_empty_and_duplicate_schemas() {
    assert_eq!(Schema::new(vec![]), Err(SchemaError::Empty));
    assert_eq!(
        Schema::new(vec![ColumnSchema::new("", DataType::Int64)]),
        Err(SchemaError::EmptyColumnName { index: 0 })
    );
    assert_eq!(
        Schema::new(vec![
            ColumnSchema::new("id", DataType::Int64),
            ColumnSchema::new("id", DataType::String),
        ]),
        Err(SchemaError::DuplicateColumn {
            name: "id".to_owned(),
        })
    );
}

#[test]
fn stores_each_type_in_its_own_physical_vector() {
    let mut table = Table::new(all_types_schema());
    table
        .insert_row(vec![
            Value::Int64(7),
            Value::Float64(3.5),
            Value::Bool(true),
            Value::String("Ada".to_owned()),
        ])
        .expect("row is valid");
    table
        .insert_row(vec![
            Value::Int64(9),
            Value::Float64(-2.25),
            Value::Bool(false),
            Value::String("Lin".to_owned()),
        ])
        .expect("row is valid");

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.columns().len(), 4);
    assert_eq!(table.columns()[0], Column::Int64(vec![7, 9]));
    assert_eq!(table.columns()[1], Column::Float64(vec![3.5, -2.25]));
    assert_eq!(table.columns()[2], Column::Bool(vec![true, false]));
    assert_eq!(
        table.columns()[3],
        Column::String(vec!["Ada".to_owned(), "Lin".to_owned()])
    );
}

#[test]
fn inserts_an_all_type_batch_into_physical_columns() {
    let mut table = Table::new(all_types_schema());

    table
        .insert_batch(vec![
            vec![1_i64.into(), 1.25_f64.into(), true.into(), "Ada".into()],
            vec![2_i64.into(), (-3.5_f64).into(), false.into(), "Lin".into()],
            vec![3_i64.into(), 0.0_f64.into(), true.into(), "Grace".into()],
        ])
        .expect("batch is valid");

    assert_eq!(table.row_count(), 3);
    assert_eq!(table.columns()[0], Column::Int64(vec![1, 2, 3]));
    assert_eq!(table.columns()[1], Column::Float64(vec![1.25, -3.5, 0.0]));
    assert_eq!(table.columns()[2], Column::Bool(vec![true, false, true]));
    assert_eq!(
        table.columns()[3],
        Column::String(vec!["Ada".to_owned(), "Lin".to_owned(), "Grace".to_owned()])
    );
}

#[test]
fn invalid_final_batch_row_reports_its_index_and_changes_nothing() {
    let mut table = Table::new(all_types_schema());
    table
        .insert_row(vec![
            10_i64.into(),
            9.5_f64.into(),
            true.into(),
            "existing".into(),
        ])
        .expect("baseline row is valid");
    let baseline = table.clone();

    let error = table.insert_batch(vec![
        vec![11_i64.into(), 1.0_f64.into(), false.into(), "valid".into()],
        vec![12_i64.into(), 2.0_f64.into(), true.into(), "valid".into()],
        vec![
            13_i64.into(),
            3.0_f64.into(),
            false.into(),
            Value::Bool(true),
        ],
    ]);

    assert_eq!(
        error,
        Err(BatchInsertError {
            batch_index: 2,
            source: InsertError::TypeMismatch {
                column_index: 3,
                column_name: "name".to_owned(),
                expected: DataType::String,
                actual: DataType::Bool,
            },
        })
    );
    assert_eq!(table, baseline);
}

#[test]
fn empty_batch_is_a_no_op() {
    let mut table = Table::new(all_types_schema());
    let baseline = table.clone();

    table
        .insert_batch(Vec::new())
        .expect("empty batch is valid");

    assert_eq!(table, baseline);
}

#[test]
fn rejected_rows_leave_every_column_unchanged() {
    let mut table = Table::new(all_types_schema());
    table
        .insert_row(vec![
            1_i64.into(),
            1.5_f64.into(),
            true.into(),
            "kept".into(),
        ])
        .expect("baseline row is valid");
    let baseline = table.clone();

    let invalid_rows = [
        vec![1_i64.into()],
        vec![
            1_i64.into(),
            1.0_f64.into(),
            true.into(),
            "name".into(),
            "too wide".into(),
        ],
        vec![
            2_i64.into(),
            2.5_f64.into(),
            false.into(),
            Value::Bool(true),
        ],
        vec![
            3_i64.into(),
            Value::Float64(f64::NAN),
            false.into(),
            "nan".into(),
        ],
        vec![
            4_i64.into(),
            Value::Float64(f64::INFINITY),
            false.into(),
            "infinity".into(),
        ],
    ];

    for row in invalid_rows {
        assert!(table.insert_row(row).is_err());
        assert_eq!(table, baseline);
    }
}

#[test]
fn reports_specific_validation_errors() {
    let mut table = Table::new(all_types_schema());

    assert_eq!(
        table.insert_row(vec![1_i64.into()]),
        Err(InsertError::RowWidth {
            expected: 4,
            actual: 1,
        })
    );
    assert_eq!(
        table.insert_row(vec![
            1_i64.into(),
            Value::Int64(2),
            true.into(),
            "name".into(),
        ]),
        Err(InsertError::TypeMismatch {
            column_index: 1,
            column_name: "score".to_owned(),
            expected: DataType::Float64,
            actual: DataType::Int64,
        })
    );

    let error = table.insert_row(vec![
        1_i64.into(),
        Value::Float64(f64::NEG_INFINITY),
        true.into(),
        "name".into(),
    ]);
    assert!(matches!(
        error,
        Err(InsertError::NonFiniteFloat {
            column_index: 1,
            ref column_name,
            value,
        }) if column_name == "score" && value == f64::NEG_INFINITY
    ));
}
