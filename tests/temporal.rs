use rusthouse::format::{OutputFormat, render};
use rusthouse::storage::Column;
use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("SQL succeeds")
        .into_iter()
        .last()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn displayed_rows(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|row| row.iter().map(Value::as_display_string).collect())
        .collect()
}

#[test]
fn temporal_columns_filter_group_order_and_compute_extrema() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (day Date, occurred_at DateTime64(3), label String);
             INSERT INTO events VALUES
                ('1970-01-01', '1970-01-01T00:00:00Z', 'epoch'),
                (DATE '2024-02-29', DATETIME64(3) '2024-02-29T12:34:56.123Z', 'late'),
                ('2024-02-29', '2024-02-29T02:00:00.5+02:00', 'early'),
                ('2024-03-01', '2024-03-01 01:02:03-05:00', 'next');",
        )
        .expect("valid temporal setup");

    let result = query(
        &mut database,
        "SELECT day, COUNT(*) AS events, MIN(occurred_at) AS first, MAX(occurred_at) AS last
         FROM events
         WHERE occurred_at >= '2024-02-29T00:00:00Z'
         GROUP BY day
         ORDER BY day;",
    );

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        [
            DataType::Date,
            DataType::Int64,
            DataType::DateTime64,
            DataType::DateTime64,
        ]
    );
    assert_eq!(
        displayed_rows(&result),
        [
            [
                "2024-02-29",
                "2",
                "2024-02-29T00:00:00.500Z",
                "2024-02-29T12:34:56.123Z",
            ],
            [
                "2024-03-01",
                "1",
                "2024-03-01T06:02:03.000Z",
                "2024-03-01T06:02:03.000Z",
            ],
        ]
    );

    let bounds = query(
        &mut database,
        "SELECT MIN(day) AS first_day, MAX(day) AS last_day FROM events;",
    );
    assert_eq!(
        bounds
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        [DataType::Date, DataType::Date]
    );
    assert_eq!(displayed_rows(&bounds), [["1970-01-01", "2024-03-01"]]);
}

#[test]
fn temporal_storage_is_compact_and_rendering_is_canonical() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE epochs (day Date, instant DateTime64(3));
             INSERT INTO epochs VALUES ('1970-01-01', '1970-01-01T01:00:00+01:00');",
        )
        .expect("epoch inserts");

    let table = database.catalog().table("epochs").expect("table exists");
    assert!(matches!(&table.columns()[0], Column::Date(values) if values == &[0_u16]));
    assert!(matches!(&table.columns()[1], Column::DateTime64(values) if values == &[0_i64]));

    let result = query(&mut database, "SELECT * FROM epochs;");
    assert_eq!(
        render(&result, OutputFormat::Csv),
        "day,instant\n1970-01-01,1970-01-01T00:00:00.000Z\n"
    );
    assert_eq!(
        render(&result, OutputFormat::Json),
        r#"{"columns":[{"name":"day","type":"Date"},{"name":"instant","type":"DateTime64(3)"}],"rows":[["1970-01-01","1970-01-01T00:00:00.000Z"]]}"#
    );
    let table_output = render(&result, OutputFormat::Table);
    assert!(table_output.contains("| 1970-01-01 | 1970-01-01T00:00:00.000Z |"));
}

#[test]
fn canonical_output_round_trips_through_iso_string_inputs() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE source (day Date, instant DateTime64(3));
             INSERT INTO source VALUES
                ('2000-02-29', '2000-02-29T23:59:59.009Z'),
                ('2149-06-06', '2299-12-31T23:59:59.999Z');",
        )
        .expect("source setup");
    let source = query(&mut database, "SELECT * FROM source ORDER BY day;");
    let values = displayed_rows(&source);

    database
        .execute(&format!(
            "CREATE TABLE destination (day Date, instant DateTime64(3));
             INSERT INTO destination VALUES ('{}', '{}'), ('{}', '{}');",
            values[0][0], values[0][1], values[1][0], values[1][1]
        ))
        .expect("canonical values reinsert");
    let destination = query(&mut database, "SELECT * FROM destination ORDER BY day;");
    assert_eq!(destination.rows, source.rows);
}

#[test]
fn invalid_calendars_precision_and_bounds_are_rejected_atomically() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE checked (day Date, instant DateTime64(3));")
        .expect("create table");

    for invalid_insert in [
        "INSERT INTO checked VALUES ('2024-02-29', '1970-01-01T00:00:00Z'), ('2023-02-29', '1970-01-01T00:00:00Z')",
        "INSERT INTO checked VALUES ('1969-12-31', '1970-01-01T00:00:00Z')",
        "INSERT INTO checked VALUES ('2149-06-07', '1970-01-01T00:00:00Z')",
        "INSERT INTO checked VALUES ('1970-01-01', '1970-01-01T00:00:00.0001Z')",
        "INSERT INTO checked VALUES ('1970-01-01', '1899-12-31T23:59:59.999Z')",
        "INSERT INTO checked VALUES ('1970-01-01', '2300-01-01T00:00:00Z')",
    ] {
        let error = database
            .execute(invalid_insert)
            .expect_err("invalid temporal input");
        assert!(matches!(error, Error::InvalidQuery(_)));
        let count = query(&mut database, "SELECT COUNT(*) AS count FROM checked;");
        assert_eq!(count.rows, [[Value::Int64(0)]]);
    }

    let typed_error = database
        .execute(
            "INSERT INTO checked VALUES (DATE '1900-02-29', DATETIME64(3) '1970-01-01T00:00:00Z')",
        )
        .expect_err("typed invalid date");
    assert!(matches!(typed_error, Error::Sql { .. }));

    let precision_error = database
        .execute("CREATE TABLE wrong_precision (instant DateTime64(6));")
        .expect_err("only millisecond precision is supported");
    assert!(
        matches!(precision_error, Error::Sql { message, .. } if message.contains("only DateTime64(3)"))
    );
}
