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
fn counts_empty_and_populated_tables_as_one_typed_int64_row() {
    let mut catalog = all_types_catalog();

    let empty = execute_table_select(&catalog, "SELECT COUNT(*) FROM events")
        .expect("empty table count is valid");
    assert_eq!(
        empty.headers(),
        [ColumnSchema::new("COUNT(*)", DataType::Int64)]
    );
    assert_eq!(empty.rows(), [vec![Value::Int64(0)]]);

    catalog
        .table_mut("events")
        .unwrap()
        .insert_batch(vec![
            vec![1_i64.into(), 1.5_f64.into(), true.into(), "one".into()],
            vec![2_i64.into(), 2.5_f64.into(), false.into(), "two".into()],
        ])
        .unwrap();

    let populated = execute_table_select(
        &catalog,
        r#"select count(*) AS "number of events" from events;"#,
    )
    .expect("populated table count with a quoted alias is valid");
    assert_eq!(
        populated.headers(),
        [ColumnSchema::new("number of events", DataType::Int64)]
    );
    assert_eq!(populated.rows(), [vec![Value::Int64(2)]]);
}

#[test]
fn count_reports_unknown_tables_and_rejects_trailing_or_broader_syntax() {
    let catalog = all_types_catalog();

    assert_eq!(
        execute_table_select(&catalog, "SELECT COUNT(*) FROM missing"),
        Err(TableSelectError::TableNotFound(TableNotFoundError {
            name: "missing".to_owned(),
        }))
    );

    let unsupported = [
        "SELECT COUNT(*) FROM events WHERE active = TRUE",
        "SELECT COUNT(*) FROM events LIMIT 1",
        "SELECT COUNT(id) FROM events",
        "SELECT COUNT(DISTINCT id) FROM events",
        "SELECT COUNT(*) total FROM events",
        "SELECT COUNT(*), id FROM events",
    ];
    for sql in unsupported {
        assert!(
            execute_table_select(&catalog, sql).is_err(),
            "unsupported COUNT query succeeded: {sql}"
        );
    }
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

#[test]
fn resolves_wide_repeated_projections_without_rescanning_the_schema() {
    const WIDTH: usize = 10_000;

    let schema = Schema::new(
        (0..WIDTH)
            .map(|index| ColumnSchema::new(format!("column_{index}"), DataType::Int64))
            .collect(),
    )
    .expect("generated column names are unique");
    let mut catalog = Catalog::new();
    catalog
        .create_table("wide", schema)
        .expect("table name is available");
    let projected_name = format!("column_{}", WIDTH - 1);
    let columns = std::iter::repeat_n(projected_name.as_str(), WIDTH)
        .collect::<Vec<_>>()
        .join(",");

    let result = execute_table_select(&catalog, &format!("SELECT {columns} FROM wide"))
        .expect("wide repeated projection is valid");

    assert_eq!(result.headers().len(), WIDTH);
    assert!(
        result
            .headers()
            .iter()
            .all(|column| column.name() == projected_name && column.data_type() == DataType::Int64)
    );
    assert!(result.rows().is_empty());
}
