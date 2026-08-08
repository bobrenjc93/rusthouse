use rusthouse::TableLimits;
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::{Column, ColumnDef, Table};
use rusthouse::batch::value::{DataType, Value};

fn four_type_table() -> Table {
    let mut table = Table::with_limits(
        "events".to_owned(),
        vec![
            ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "score".to_owned(),
                data_type: DataType::Float64,
            },
            ColumnDef {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ],
        TableLimits::new(4, 4, 16),
    )
    .expect("valid table");
    table
        .insert_rows(vec![
            vec![
                Value::Int64(0),
                Value::Float64(0.5),
                Value::Bool(false),
                Value::String("zero".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("one".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(false),
                Value::String("two".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(3.5),
                Value::Bool(true),
                Value::String("three".to_owned()),
            ],
        ])
        .expect("rows fit");
    table
}

fn assert_original(table: &Table) {
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[0, 1, 2, 3]));
    assert!(
        matches!(&table.columns()[1], Column::Float64(values) if values == &[0.5, 1.5, 2.5, 3.5])
    );
    assert!(
        matches!(&table.columns()[2], Column::Bool(values) if values == &[false, true, false, true])
    );
    assert!(matches!(&table.columns()[3], Column::String(values)
        if values == &["zero", "one", "two", "three"]));
    assert_eq!(table.retained_value_bytes(), 83);
}

#[test]
fn empty_replacements_are_noops_for_every_physical_type() {
    let mut table = four_type_table();

    for column in ["id", "score", "active", "label"] {
        assert_eq!(table.replace_column_values(column, vec![]), Ok(0));
    }

    assert_original(&table);
}

#[test]
fn sparse_replacements_update_every_physical_type_and_preserve_other_data() {
    let mut table = four_type_table();
    let schema = table.schema().to_vec();

    assert_eq!(
        table.replace_column_values("ID", vec![(1, Value::Int64(10)), (3, Value::Int64(30))]),
        Ok(2)
    );
    assert_eq!(
        table.replace_column_values(
            "score",
            vec![(0, Value::Float64(-0.25)), (2, Value::Float64(20.25))],
        ),
        Ok(2)
    );
    assert_eq!(
        table.replace_column_values(
            "active",
            vec![(1, Value::Bool(false)), (2, Value::Bool(true))],
        ),
        Ok(2)
    );
    let first_label = String::from("ten");
    let first_label_allocation = first_label.as_ptr();
    assert_eq!(
        table.replace_column_values(
            "label",
            vec![
                (1, Value::String(first_label)),
                (3, Value::String("thirty".to_owned()))
            ],
        ),
        Ok(2)
    );

    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[0, 10, 2, 30]));
    assert!(matches!(&table.columns()[1], Column::Float64(values)
        if values == &[-0.25, 1.5, 20.25, 3.5]));
    assert!(matches!(&table.columns()[2], Column::Bool(values)
        if values == &[false, false, true, true]));
    assert!(matches!(&table.columns()[3], Column::String(values)
        if values == &["zero", "ten", "two", "thirty"] && values[1].as_ptr() == first_label_allocation));
    assert_eq!(table.name(), "events");
    assert_eq!(table.schema(), schema);
    assert_eq!(table.row_count(), 4);
    assert_eq!(table.retained_value_bytes(), 84);
    assert_eq!(table.limits(), TableLimits::new(4, 4, 16));
}

#[test]
fn complete_replacements_update_every_physical_type() {
    let mut table = four_type_table();

    assert_eq!(
        table.replace_column_values(
            "id",
            (0..4)
                .map(|row| (row, Value::Int64(10 + row as i64)))
                .collect(),
        ),
        Ok(4)
    );
    assert_eq!(
        table.replace_column_values(
            "score",
            (0..4)
                .map(|row| (row, Value::Float64(10.5 + row as f64)))
                .collect(),
        ),
        Ok(4)
    );
    assert_eq!(
        table.replace_column_values(
            "active",
            (0..4).map(|row| (row, Value::Bool(row % 2 == 0))).collect(),
        ),
        Ok(4)
    );
    assert_eq!(
        table.replace_column_values(
            "label",
            (0..4)
                .map(|row| (row, Value::String(format!("new-{row}"))))
                .collect(),
        ),
        Ok(4)
    );

    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[10, 11, 12, 13]));
    assert!(matches!(&table.columns()[1], Column::Float64(values)
        if values == &[10.5, 11.5, 12.5, 13.5]));
    assert!(matches!(&table.columns()[2], Column::Bool(values)
        if values == &[true, false, true, false]));
    assert!(matches!(&table.columns()[3], Column::String(values)
        if values == &["new-0", "new-1", "new-2", "new-3"]));
    assert_eq!(table.retained_value_bytes(), 88);
}

#[test]
fn invalid_indexes_and_values_leave_the_table_unchanged() {
    let invalid_calls = [
        (
            vec![(1, Value::Int64(10)), (1, Value::Int64(11))],
            Error::SelectionNotStrictlyIncreasing {
                selection_position: 1,
                previous_row_index: 1,
                row_index: 1,
            },
        ),
        (
            vec![(2, Value::Int64(20)), (1, Value::Int64(10))],
            Error::SelectionNotStrictlyIncreasing {
                selection_position: 1,
                previous_row_index: 2,
                row_index: 1,
            },
        ),
        (
            vec![(0, Value::Int64(10)), (4, Value::Int64(40))],
            Error::SelectionIndexOutOfBounds {
                selection_position: 1,
                row_index: 4,
                input_rows: 4,
            },
        ),
    ];

    for (replacements, expected) in invalid_calls {
        let mut table = four_type_table();
        assert_eq!(
            table.replace_column_values("id", replacements),
            Err(expected)
        );
        assert_original(&table);
    }

    let mut table = four_type_table();
    assert_eq!(
        table.replace_column_values(
            "id",
            vec![
                (0, Value::Int64(10)),
                (2, Value::String("wrong".to_owned()))
            ],
        ),
        Err(Error::TypeMismatch {
            context: "column 'events.id'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        })
    );
    assert_original(&table);
}

#[test]
fn unknown_column_null_and_non_finite_values_are_atomic() {
    let mut table = four_type_table();
    assert_eq!(
        table.replace_column_values("missing", vec![(0, Value::Int64(10))]),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "missing".to_owned(),
        })
    );
    assert_original(&table);

    assert_eq!(
        table.replace_column_values(
            "label",
            vec![
                (0, Value::String("changed".to_owned())),
                (2, Value::Null(DataType::String))
            ],
        ),
        Err(Error::TypeMismatch {
            context: "column 'events.label'".to_owned(),
            expected: "String".to_owned(),
            actual: "NULL".to_owned(),
        })
    );
    assert_original(&table);

    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            table.replace_column_values(
                "score",
                vec![(0, Value::Float64(10.0)), (2, Value::Float64(number))],
            ),
            Err(Error::InvalidQuery(
                "column 'events.score' cannot store a non-finite Float64".to_owned()
            ))
        );
        assert_original(&table);
    }
}
