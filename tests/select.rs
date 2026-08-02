use rusthouse::{
    ColumnSchema, DataType, Keyword, ParseErrorKind, QueryError, QueryLimits, Schema, Table, Value,
    execute_select,
};

fn events_schema() -> Schema {
    Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64, false),
        ColumnSchema::new("label", DataType::String, true),
    ])
    .unwrap()
}

#[test]
fn scans_an_empty_table_with_typed_column_metadata() {
    let table = Table::new(events_schema());

    let result = execute_select(
        "sElEcT * fRoM EVENTS;",
        "events",
        &table,
        QueryLimits::new(20),
    )
    .unwrap();

    assert_eq!(result.columns, events_schema().columns());
    assert_eq!(result.columns[0].name(), "id");
    assert_eq!(result.columns[0].data_type(), DataType::Int64);
    assert!(!result.columns[0].is_nullable());
    assert_eq!(result.columns[1].data_type(), DataType::String);
    assert!(result.columns[1].is_nullable());
    assert!(result.rows.is_empty());
    assert!(!result.truncated);
}

#[test]
fn returns_owned_rows_and_preserves_nulls() {
    let result = {
        let mut table = Table::new(events_schema());
        table
            .insert_row(&[Value::Int64(1), Value::from("first")])
            .unwrap();
        table.insert_row(&[Value::Int64(2), Value::Null]).unwrap();

        execute_select(
            "SELECT * FROM events",
            "events",
            &table,
            QueryLimits::default(),
        )
        .unwrap()
    };

    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(1), Value::from("first")],
            vec![Value::Int64(2), Value::Null],
        ]
    );
    assert!(!result.truncated);
}

#[test]
fn rejects_a_different_supplied_table_name() {
    let table = Table::new(events_schema());

    assert_eq!(
        execute_select(
            "SELECT * FROM metrics",
            "events",
            &table,
            QueryLimits::default(),
        ),
        Err(QueryError::TableNameMismatch {
            requested: "metrics".to_owned(),
            available: "events".to_owned(),
        })
    );
}

#[test]
fn rejects_every_unsupported_select_shape() {
    let table = Table::new(events_schema());
    let cases = [
        (
            "DELETE FROM events",
            0,
            ParseErrorKind::ExpectedKeyword {
                keyword: Keyword::Select,
            },
        ),
        ("SELECT id FROM events", 7, ParseErrorKind::ExpectedAsterisk),
        (
            "SELECT * events",
            9,
            ParseErrorKind::ExpectedKeyword {
                keyword: Keyword::From,
            },
        ),
        ("SELECT * FROM", 13, ParseErrorKind::ExpectedIdentifier),
        (
            "SELECT * FROM events WHERE id",
            21,
            ParseErrorKind::TrailingInput,
        ),
        ("SELECT * FROM events;;", 21, ParseErrorKind::TrailingInput),
    ];

    for (sql, position, kind) in cases {
        let QueryError::Parse(error) =
            execute_select(sql, "events", &table, QueryLimits::default()).unwrap_err()
        else {
            panic!("{sql:?} should fail during parsing");
        };
        assert_eq!(error.position, position, "{sql:?}");
        assert_eq!(error.kind, kind, "{sql:?}");
    }
}

#[test]
fn enforces_the_exact_result_row_limit() {
    let schema = Schema::new(vec![ColumnSchema::new("id", DataType::Int64, false)]).unwrap();
    let mut table = Table::new(schema);
    for id in 1..=3 {
        table.insert_row(&[Value::Int64(id)]).unwrap();
    }

    let limited = execute_select(
        "SELECT * FROM numbers",
        "numbers",
        &table,
        QueryLimits::new(2),
    )
    .unwrap();
    assert_eq!(
        limited.rows,
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
    assert!(limited.truncated);

    let exact = execute_select(
        "SELECT * FROM numbers;",
        "numbers",
        &table,
        QueryLimits::new(3),
    )
    .unwrap();
    assert_eq!(exact.rows.len(), 3);
    assert!(!exact.truncated);

    let zero = execute_select(
        "SELECT * FROM numbers",
        "numbers",
        &table,
        QueryLimits::new(0),
    )
    .unwrap();
    assert!(zero.rows.is_empty());
    assert!(zero.truncated);
}
