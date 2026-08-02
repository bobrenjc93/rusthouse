use rusthouse::{
    Catalog, ColumnNotFoundError, ColumnSchema, DataType, MAX_TABLE_SELECT_RESULT_BYTES, Schema,
    TableNotFoundError, TableSelectError, Value, execute_table_select,
};

fn all_types_catalog() -> Catalog {
    let schema = Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64),
        ColumnSchema::new("score", DataType::Float64),
        ColumnSchema::new("active", DataType::Bool),
        ColumnSchema::new("name", DataType::String),
    ])
    .expect("test schema is valid");
    let mut catalog = Catalog::new();
    catalog
        .create_table("events", schema)
        .expect("table name is available");
    catalog
}

#[test]
fn projects_all_types_in_query_order_and_rows_in_insertion_order() {
    let mut catalog = all_types_catalog();
    catalog
        .table_mut("events")
        .unwrap()
        .insert_batch(vec![
            vec![1_i64.into(), 9.5_f64.into(), true.into(), "first".into()],
            vec![
                2_i64.into(),
                (-3.25_f64).into(),
                false.into(),
                "second".into(),
            ],
        ])
        .expect("rows are valid");

    let result = execute_table_select(&catalog, "SELECT name, active, score, id FROM events;")
        .expect("projection is valid");

    assert_eq!(
        result.headers(),
        [
            ColumnSchema::new("name", DataType::String),
            ColumnSchema::new("active", DataType::Bool),
            ColumnSchema::new("score", DataType::Float64),
            ColumnSchema::new("id", DataType::Int64),
        ]
    );
    assert_eq!(
        result.rows(),
        [
            vec![
                Value::String("first".to_owned()),
                Value::Bool(true),
                Value::Float64(9.5),
                Value::Int64(1),
            ],
            vec![
                Value::String("second".to_owned()),
                Value::Bool(false),
                Value::Float64(-3.25),
                Value::Int64(2),
            ],
        ]
    );
}

#[test]
fn empty_table_returns_typed_headers_and_no_rows_without_a_semicolon() {
    let catalog = all_types_catalog();

    let result = execute_table_select(&catalog, "SELECT score, id FROM events")
        .expect("projection is valid");

    assert_eq!(
        result.headers(),
        [
            ColumnSchema::new("score", DataType::Float64),
            ColumnSchema::new("id", DataType::Int64),
        ]
    );
    assert!(result.rows().is_empty());
}

#[test]
fn reports_typed_unknown_table_and_column_failures() {
    let catalog = all_types_catalog();

    assert_eq!(
        execute_table_select(&catalog, "SELECT id FROM missing"),
        Err(TableSelectError::TableNotFound(TableNotFoundError {
            name: "missing".to_owned(),
        }))
    );
    assert_eq!(
        execute_table_select(&catalog, "SELECT id, missing FROM events"),
        Err(TableSelectError::ColumnNotFound(ColumnNotFoundError {
            table_name: "events".to_owned(),
            column_name: "missing".to_owned(),
        }))
    );
}

#[test]
fn rejects_wildcards_aliases_filters_ordering_and_multiple_statements() {
    let catalog = all_types_catalog();
    let unsupported = [
        "SELECT * FROM events",
        "SELECT id AS event_id FROM events",
        "SELECT id FROM events AS source",
        "SELECT id FROM events WHERE active = TRUE",
        "SELECT id FROM events ORDER BY id",
        "SELECT id FROM events; SELECT id FROM events",
    ];

    for sql in unsupported {
        assert!(
            execute_table_select(&catalog, sql).is_err(),
            "unsupported query succeeded: {sql}"
        );
    }
}

#[test]
fn rejects_repeated_large_strings_before_materializing_the_result() {
    let mut catalog = Catalog::new();
    let schema = Schema::new(vec![ColumnSchema::new("payload", DataType::String)])
        .expect("test schema is valid");
    catalog
        .create_table("events", schema)
        .expect("table name is available");
    catalog
        .table_mut("events")
        .unwrap()
        .insert_row(vec![Value::String("x".repeat(1024 * 1024))])
        .expect("row is valid");
    let columns = std::iter::repeat_n("payload", 100)
        .collect::<Vec<_>>()
        .join(", ");

    let error = execute_table_select(&catalog, &format!("SELECT {columns} FROM events"))
        .expect_err("result exceeds the materialization budget");

    assert!(matches!(
        error,
        TableSelectError::ResultSizeLimitExceeded {
            estimated_bytes,
            limit: MAX_TABLE_SELECT_RESULT_BYTES,
        } if estimated_bytes > MAX_TABLE_SELECT_RESULT_BYTES
    ));
}
