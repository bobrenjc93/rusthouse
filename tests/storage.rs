use rusthouse::{
    Column, ColumnSchema, DataType, InsertError, NonFiniteFloat, Schema, SchemaError, Table,
    TableLimits, Value, ValueRef, ValueType,
};

fn all_types_schema(nullable: bool) -> Schema {
    Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64, nullable),
        ColumnSchema::new("score", DataType::Float64, nullable),
        ColumnSchema::new("active", DataType::Bool, nullable),
        ColumnSchema::new("label", DataType::String, nullable),
    ])
    .unwrap()
}

#[test]
fn validates_schema_names() {
    assert_eq!(Schema::new(vec![]), Err(SchemaError::Empty));
    assert_eq!(
        Schema::new(vec![ColumnSchema::new("", DataType::Int64, false)]),
        Err(SchemaError::EmptyColumnName { column: 0 })
    );
    assert_eq!(
        Schema::new(vec![
            ColumnSchema::new("id", DataType::Int64, false),
            ColumnSchema::new("id", DataType::String, true),
        ]),
        Err(SchemaError::DuplicateColumnName {
            name: "id".to_owned()
        })
    );
}

#[test]
fn stores_every_physical_type_and_round_trips_nulls() {
    let mut table = Table::new(all_types_schema(true));
    table
        .insert_row(&[
            Value::Int64(-42),
            Value::Float64(1.25),
            Value::Bool(true),
            Value::String("data".to_owned()),
        ])
        .unwrap();
    table
        .insert_row(&[Value::Null, Value::Null, Value::Null, Value::Null])
        .unwrap();

    assert_eq!(table.len(), 2);
    assert_eq!(table.string_bytes(), 4);
    assert_eq!(
        table.row(0),
        Some(vec![
            ValueRef::Int64(-42),
            ValueRef::Float64(1.25),
            ValueRef::Bool(true),
            ValueRef::String("data"),
        ])
    );
    assert_eq!(
        table.row(1),
        Some(vec![
            ValueRef::Null,
            ValueRef::Null,
            ValueRef::Null,
            ValueRef::Null,
        ])
    );
    assert_eq!(table.row(2), None);

    let Column::Int64(ids) = table.column_by_name("id").unwrap() else {
        panic!("id should be an Int64 column");
    };
    assert_eq!(ids.values(), &[-42, 0]);
    assert_eq!(ids.null_bitmap_words(), Some(&[0b10][..]));

    let Column::Float64(scores) = table.column(1).unwrap() else {
        panic!("score should be a Float64 column");
    };
    assert_eq!(scores.values(), &[1.25, 0.0]);

    let Column::Bool(active) = table.column(2).unwrap() else {
        panic!("active should be a Bool column");
    };
    assert_eq!(active.values(), &[true, false]);

    let Column::String(labels) = table.column(3).unwrap() else {
        panic!("label should be a String column");
    };
    assert_eq!(labels.values(), &["data".to_owned(), String::new()]);
    assert!(
        table
            .columns()
            .iter()
            .all(|column| column.null_count() == 1)
    );
}

#[test]
fn rejects_invalid_rows_without_mutation() {
    let mut table = Table::with_limits(all_types_schema(false), TableLimits::new(3, 8));
    table
        .insert_row(&[
            Value::Int64(1),
            Value::Float64(2.0),
            Value::Bool(false),
            Value::from("base"),
        ])
        .unwrap();

    let cases = [
        (
            vec![Value::Int64(2)],
            InsertError::Shape {
                expected: 4,
                actual: 1,
            },
        ),
        (
            vec![
                Value::Bool(true),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::from("ok"),
            ],
            InsertError::TypeMismatch {
                column: 0,
                column_name: "id".to_owned(),
                expected: DataType::Int64,
                actual: ValueType::Bool,
            },
        ),
        (
            vec![
                Value::Null,
                Value::Float64(2.0),
                Value::Bool(false),
                Value::from("ok"),
            ],
            InsertError::NullNotAllowed {
                column: 0,
                column_name: "id".to_owned(),
            },
        ),
        (
            vec![
                Value::Int64(2),
                Value::Float64(f64::NAN),
                Value::Bool(false),
                Value::from("ok"),
            ],
            InsertError::NonFiniteFloat {
                column: 1,
                column_name: "score".to_owned(),
                value: NonFiniteFloat::NaN,
            },
        ),
        (
            vec![
                Value::Int64(2),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::from("12345"),
            ],
            InsertError::StringLimitExceeded {
                limit: 8,
                current: 4,
                attempted: 9,
            },
        ),
    ];

    for (row, expected_error) in cases {
        let before = table.clone();
        assert_eq!(table.insert_row(&row), Err(expected_error));
        assert_eq!(table, before);
    }
}

#[test]
fn enforces_exact_row_and_total_string_boundaries() {
    let schema = Schema::new(vec![ColumnSchema::new("text", DataType::String, false)]).unwrap();
    let mut table = Table::with_limits(schema, TableLimits::new(2, 5));

    table.insert_row(&[Value::from("abc")]).unwrap();
    table.insert_row(&[Value::from("de")]).unwrap();
    assert_eq!(table.len(), 2);
    assert_eq!(table.string_bytes(), 5);

    let before = table.clone();
    assert_eq!(
        table.insert_row(&[Value::from("")]),
        Err(InsertError::RowLimitExceeded { limit: 2 })
    );
    assert_eq!(table, before);
}

#[test]
fn rejects_each_non_finite_float_classification() {
    let schema = Schema::new(vec![ColumnSchema::new("value", DataType::Float64, true)]).unwrap();
    let mut table = Table::new(schema);

    for (value, kind) in [
        (f64::NAN, NonFiniteFloat::NaN),
        (f64::INFINITY, NonFiniteFloat::PositiveInfinity),
        (f64::NEG_INFINITY, NonFiniteFloat::NegativeInfinity),
    ] {
        let before = table.clone();
        assert_eq!(
            table.insert_row(&[Value::Float64(value)]),
            Err(InsertError::NonFiniteFloat {
                column: 0,
                column_name: "value".to_owned(),
                value: kind,
            })
        );
        assert_eq!(table, before);
    }
}

#[test]
fn non_nullable_columns_omit_null_bitmaps() {
    let mut table = Table::new(all_types_schema(false));
    table
        .insert_row(&[
            Value::Int64(1),
            Value::Float64(2.5),
            Value::Bool(true),
            Value::from("x"),
        ])
        .unwrap();

    for column in table.columns() {
        assert!(!column.is_nullable());
        assert_eq!(column.null_count(), 0);
    }
    let Column::Int64(column) = table.column(0).unwrap() else {
        unreachable!()
    };
    assert_eq!(column.null_bitmap_words(), None);
}
