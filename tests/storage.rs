use rusthouse::{
    AppendError, BatchAppendError, Column, DataType, Field, MAX_IDENTIFIER_BYTES,
    MAX_SCHEMA_FIELDS, MAX_STORED_STRING_BYTES, Schema, SchemaError, Table, Value, ValueType,
};
use std::cell::Cell;

fn four_type_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("score", DataType::Float64, true),
        Field::new("active", DataType::Bool, true),
        Field::new("name", DataType::String, true),
    ])
    .unwrap()
}

fn populated_table(row_limit: usize) -> Table {
    let mut table = Table::new(four_type_schema(), row_limit);
    table
        .append_row([
            ("name", Value::String("Ada".into())),
            ("active", Value::Bool(true)),
            ("score", Value::Float64(9.5)),
            ("id", Value::Int64(7)),
        ])
        .unwrap();
    table
}

#[test]
fn schema_rejects_duplicate_field_names() {
    let error = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("id", DataType::String, true),
    ])
    .unwrap_err();

    assert_eq!(error, SchemaError::DuplicateField { field: "id".into() });
}

#[test]
fn schema_field_count_accepts_the_boundary_and_rejects_one_over() {
    let fields = (0..MAX_SCHEMA_FIELDS)
        .map(|index| Field::new(format!("field_{index}"), DataType::Int64, false))
        .collect::<Vec<_>>();

    let schema = Schema::new(fields.clone()).unwrap();
    assert_eq!(schema.len(), MAX_SCHEMA_FIELDS);

    let mut oversized = fields;
    oversized.push(Field::new("one_too_many", DataType::Int64, false));
    assert_eq!(
        Schema::new(oversized),
        Err(SchemaError::TooManyFields {
            limit: MAX_SCHEMA_FIELDS,
            actual: MAX_SCHEMA_FIELDS + 1,
        })
    );
}

#[test]
fn schema_identifier_limit_counts_multibyte_utf8_bytes() {
    let exact = "é".repeat(MAX_IDENTIFIER_BYTES / "é".len());
    assert_eq!(exact.len(), MAX_IDENTIFIER_BYTES);
    assert!(exact.chars().count() < MAX_IDENTIFIER_BYTES);
    Schema::new(vec![Field::new(&exact, DataType::String, false)]).unwrap();

    let one_over = format!("{exact}a");
    assert_eq!(one_over.len(), MAX_IDENTIFIER_BYTES + 1);
    assert_eq!(
        Schema::new(vec![Field::new(&one_over, DataType::String, false)]),
        Err(SchemaError::IdentifierTooLong {
            field: one_over,
            length: MAX_IDENTIFIER_BYTES + 1,
            limit: MAX_IDENTIFIER_BYTES,
        })
    );
}

#[test]
fn stores_all_types_column_first_with_null_validity() {
    let mut table = populated_table(2);
    table
        .append_row([
            ("id", Value::Int64(8)),
            ("score", Value::Null),
            ("active", Value::Null),
            ("name", Value::Null),
        ])
        .unwrap();

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.columns().len(), 4);

    let Column::Int64(ids) = table.column("id").unwrap() else {
        panic!("id should be an Int64 column");
    };
    assert_eq!(ids.values(), &[7, 8]);
    assert_eq!(ids.validity().words(), &[0b11]);
    assert_eq!(ids.get(0), Some(Some(&7)));

    let Column::Float64(scores) = table.column("score").unwrap() else {
        panic!("score should be a Float64 column");
    };
    assert_eq!(scores.values(), &[9.5, 0.0]);
    assert_eq!(scores.validity().words(), &[0b01]);
    assert_eq!(scores.get(1), Some(None));

    let Column::Bool(active) = table.column("active").unwrap() else {
        panic!("active should be a Bool column");
    };
    assert_eq!(active.values(), &[true, false]);
    assert_eq!(active.validity().words(), &[0b01]);

    let Column::String(names) = table.column("name").unwrap() else {
        panic!("name should be a String column");
    };
    assert_eq!(names.values(), &["Ada", ""]);
    assert_eq!(names.validity().words(), &[0b01]);
    assert_eq!(names.get(2), None);
    assert!(table.column("missing").is_none());
}

fn assert_rejected_without_mutation(
    table: &mut Table,
    row: Vec<(&str, Value)>,
    expected: AppendError,
) {
    let before = table.clone();
    assert_eq!(table.append_row(row), Err(expected));
    assert_eq!(*table, before);
}

#[test]
fn every_validation_error_leaves_the_table_unchanged() {
    let mut table = populated_table(2);

    assert_rejected_without_mutation(
        &mut table,
        vec![
            ("id", Value::Int64(8)),
            ("id", Value::Int64(9)),
            ("active", Value::Bool(false)),
            ("name", Value::String("Grace".into())),
        ],
        AppendError::DuplicateField { field: "id".into() },
    );

    assert_rejected_without_mutation(
        &mut table,
        vec![
            ("id", Value::Int64(8)),
            ("active", Value::Bool(false)),
            ("name", Value::String("Grace".into())),
        ],
        AppendError::RowShapeMismatch {
            expected: 4,
            actual: 3,
            missing: vec!["score".into()],
            unexpected: vec![],
        },
    );

    assert_rejected_without_mutation(
        &mut table,
        vec![
            ("id", Value::Int64(8)),
            ("score", Value::Float64(8.0)),
            ("active", Value::Bool(false)),
            ("alias", Value::String("Grace".into())),
        ],
        AppendError::RowShapeMismatch {
            expected: 4,
            actual: 4,
            missing: vec!["name".into()],
            unexpected: vec!["alias".into()],
        },
    );

    assert_rejected_without_mutation(
        &mut table,
        vec![
            ("id", Value::String("eight".into())),
            ("score", Value::Float64(8.0)),
            ("active", Value::Bool(false)),
            ("name", Value::String("Grace".into())),
        ],
        AppendError::TypeMismatch {
            field: "id".into(),
            expected: DataType::Int64,
            actual: ValueType::String,
        },
    );

    assert_rejected_without_mutation(
        &mut table,
        vec![
            ("id", Value::Null),
            ("score", Value::Float64(8.0)),
            ("active", Value::Bool(false)),
            ("name", Value::String("Grace".into())),
        ],
        AppendError::NullabilityViolation { field: "id".into() },
    );

    table
        .append_row([
            ("id", Value::Int64(8)),
            ("score", Value::Float64(8.0)),
            ("active", Value::Bool(false)),
            ("name", Value::String("Grace".into())),
        ])
        .unwrap();
    assert_rejected_without_mutation(
        &mut table,
        vec![],
        AppendError::RowLimitExceeded { limit: 2 },
    );
}

#[test]
fn named_append_enforces_the_stored_string_boundary_atomically() {
    let schema = Schema::new(vec![Field::new("value", DataType::String, false)]).unwrap();
    let mut table = Table::new(schema, 2);
    let exact = "x".repeat(MAX_STORED_STRING_BYTES);

    table.append_row([("value", Value::String(exact))]).unwrap();
    let before = table.clone();
    let one_over = "x".repeat(MAX_STORED_STRING_BYTES + 1);

    assert_eq!(
        table.append_row([("value", Value::String(one_over))]),
        Err(AppendError::StringTooLong {
            field: "value".into(),
            length: MAX_STORED_STRING_BYTES + 1,
            limit: MAX_STORED_STRING_BYTES,
        })
    );
    assert_eq!(table, before);
}

#[test]
fn positional_batch_counts_multibyte_string_bytes_before_any_mutation() {
    let schema = Schema::new(vec![Field::new("value", DataType::String, false)]).unwrap();
    let mut table = Table::new(schema, 4);
    let exact = "é".repeat(MAX_STORED_STRING_BYTES / "é".len());
    assert_eq!(exact.len(), MAX_STORED_STRING_BYTES);
    assert!(exact.chars().count() < MAX_STORED_STRING_BYTES);
    table
        .append_batch([[Value::String(exact.clone())]])
        .unwrap();

    let before = table.clone();
    let one_over = format!("{exact}a");
    assert_eq!(one_over.len(), MAX_STORED_STRING_BYTES + 1);
    assert_eq!(
        table.append_batch([
            [Value::String("valid earlier row".into())],
            [Value::String(one_over)],
        ]),
        Err(BatchAppendError::StringTooLong {
            row_index: 1,
            field: "value".into(),
            length: MAX_STORED_STRING_BYTES + 1,
            limit: MAX_STORED_STRING_BYTES,
        })
    );
    assert_eq!(table, before);
}

#[test]
fn oversized_row_iterator_is_bounded_and_does_not_mutate_the_table() {
    let mut table = populated_table(2);
    let before = table.clone();
    let yielded = Cell::new(0);
    let row = (0..100).map(|index| {
        yielded.set(yielded.get() + 1);
        let name = match index {
            0 => "id".to_owned(),
            1 => "score".to_owned(),
            2 => "active".to_owned(),
            3 => "name".to_owned(),
            _ => format!("extra_{index}"),
        };
        (name, Value::Null)
    });

    assert_eq!(
        table.append_row(row),
        Err(AppendError::RowShapeMismatch {
            expected: 4,
            actual: 5,
            missing: vec![],
            unexpected: vec!["extra_4".into()],
        })
    );
    assert_eq!(yielded.get(), 5);
    assert_eq!(table, before);
}

#[test]
fn validity_bitmap_crosses_word_boundary_without_losing_nulls() {
    let schema = Schema::new(vec![Field::new("value", DataType::Int64, true)]).unwrap();
    let mut table = Table::new(schema, 65);

    for index in 0..65_i64 {
        let value = if index % 2 == 0 {
            Value::Null
        } else {
            Value::Int64(index)
        };
        table.append_row([("value", value)]).unwrap();
    }

    let Column::Int64(values) = table.column("value").unwrap() else {
        panic!("value should be an Int64 column");
    };
    assert_eq!(values.len(), 65);
    assert_eq!(values.validity().len(), 65);
    assert_eq!(values.validity().words(), &[0xaaaa_aaaa_aaaa_aaaa, 0]);
    assert_eq!(values.get(63), Some(Some(&63)));
    assert_eq!(values.get(64), Some(None));
}

#[test]
fn zero_row_limit_rejects_without_allocating_column_values() {
    let mut table = Table::new(four_type_schema(), 0);
    let before = table.clone();

    assert_eq!(
        table.append_row([("id", Value::Int64(1))]),
        Err(AppendError::RowLimitExceeded { limit: 0 })
    );
    assert_eq!(table, before);
    assert!(table.columns().iter().all(Column::is_empty));
}

#[test]
fn positional_batch_stores_all_types_and_nulls_in_schema_order() {
    let mut table = Table::new(four_type_schema(), 3);

    table
        .append_batch([
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("one".into()),
            ],
            vec![Value::Int64(2), Value::Null, Value::Null, Value::Null],
        ])
        .unwrap();

    assert_eq!(table.row_count(), 2);
    let Column::Int64(ids) = table.column("id").unwrap() else {
        panic!("id should be an Int64 column");
    };
    assert_eq!(ids.values(), &[1, 2]);
    assert_eq!(ids.validity().words(), &[0b11]);

    let Column::Float64(scores) = table.column("score").unwrap() else {
        panic!("score should be a Float64 column");
    };
    assert_eq!(scores.values(), &[1.5, 0.0]);
    assert_eq!(scores.validity().words(), &[0b01]);

    let Column::Bool(active) = table.column("active").unwrap() else {
        panic!("active should be a Bool column");
    };
    assert_eq!(active.values(), &[true, false]);
    assert_eq!(active.validity().words(), &[0b01]);

    let Column::String(names) = table.column("name").unwrap() else {
        panic!("name should be a String column");
    };
    assert_eq!(names.values(), &["one", ""]);
    assert_eq!(names.validity().words(), &[0b01]);
}

#[test]
fn late_invalid_batch_rows_report_their_index_and_roll_back() {
    let mut table = populated_table(10);
    let before = table.clone();
    let rows = [
        vec![
            Value::Int64(8),
            Value::Float64(8.0),
            Value::Bool(false),
            Value::String("Grace".into()),
        ],
        vec![
            Value::Int64(9),
            Value::String("wrong".into()),
            Value::Bool(true),
            Value::String("Lin".into()),
        ],
    ];

    assert_eq!(
        table.append_batch(rows),
        Err(BatchAppendError::TypeMismatch {
            row_index: 1,
            field: "score".into(),
            expected: DataType::Float64,
            actual: ValueType::String,
        })
    );
    assert_eq!(table, before);

    assert_eq!(
        table.append_batch([
            vec![
                Value::Int64(8),
                Value::Float64(8.0),
                Value::Bool(false),
                Value::String("Grace".into()),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ]),
        Err(BatchAppendError::NullabilityViolation {
            row_index: 1,
            field: "id".into(),
        })
    );
    assert_eq!(table, before);
}

#[test]
fn positional_batch_bounds_each_row_iterator_and_rolls_back() {
    let mut table = populated_table(10);
    let before = table.clone();
    let yielded = Cell::new(0);
    let oversized_row = std::iter::repeat_with(|| {
        yielded.set(yielded.get() + 1);
        Value::Null
    });

    assert_eq!(
        table.append_batch(std::iter::once(oversized_row)),
        Err(BatchAppendError::RowShapeMismatch {
            row_index: 0,
            expected: 4,
            actual: 5,
        })
    );
    assert_eq!(yielded.get(), 5);
    assert_eq!(table, before);
}

#[test]
fn positional_batch_bounds_the_row_iterator_by_remaining_capacity() {
    let mut table = populated_table(3);
    let before = table.clone();
    let yielded = Cell::new(0);
    let rows = std::iter::repeat_with(|| {
        yielded.set(yielded.get() + 1);
        [
            Value::Int64(8),
            Value::Float64(8.0),
            Value::Bool(false),
            Value::String("Grace".into()),
        ]
    });

    assert_eq!(
        table.append_batch(rows),
        Err(BatchAppendError::RowLimitExceeded {
            row_index: 2,
            limit: 3,
        })
    );
    assert_eq!(yielded.get(), 3);
    assert_eq!(table, before);
}

#[test]
fn positional_batch_handles_empty_batches_and_short_rows() {
    let mut full_table = populated_table(1);
    let before = full_table.clone();
    full_table
        .append_batch(std::iter::empty::<[Value; 4]>())
        .unwrap();
    assert_eq!(full_table, before);

    let mut table = Table::new(four_type_schema(), 1);
    assert_eq!(
        table.append_batch([vec![Value::Int64(1), Value::Float64(1.0)]]),
        Err(BatchAppendError::RowShapeMismatch {
            row_index: 0,
            expected: 4,
            actual: 2,
        })
    );
    assert_eq!(table.row_count(), 0);
    assert!(table.columns().iter().all(Column::is_empty));
}
