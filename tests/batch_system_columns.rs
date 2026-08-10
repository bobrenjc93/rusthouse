use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::{OutputFormat, render};
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

const SYSTEM_COLUMNS_QUERY: &str =
    "SELECT database, table, name, type, position FROM system.columns";

fn columns() -> Vec<ResultColumn> {
    vec![
        ResultColumn {
            name: "database".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "table".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "type".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "position".to_owned(),
            data_type: DataType::Int64,
        },
    ]
}

fn metadata_result(rows: &[(&str, &str, DataType, i64)]) -> QueryResult {
    QueryResult {
        columns: columns(),
        rows: rows
            .iter()
            .map(|(table, name, data_type, position)| {
                vec![
                    Value::String("default".to_owned()),
                    Value::String((*table).to_owned()),
                    Value::String((*name).to_owned()),
                    Value::String(data_type.to_string()),
                    Value::Int64(*position),
                ]
            })
            .collect(),
    }
}

fn metadata_result_with_type_names(rows: &[(&str, &str, &str, i64)]) -> QueryResult {
    QueryResult {
        columns: columns(),
        rows: rows
            .iter()
            .map(|(table, name, type_name, position)| {
                vec![
                    Value::String("default".to_owned()),
                    Value::String((*table).to_owned()),
                    Value::String((*name).to_owned()),
                    Value::String((*type_name).to_owned()),
                    Value::Int64(*position),
                ]
            })
            .collect(),
    }
}

fn retained_bytes(result: &QueryResult) -> usize {
    result.columns.len() * size_of::<ResultColumn>()
        + result
            .columns
            .iter()
            .map(|column| column.name.len())
            .sum::<usize>()
        + result.rows.len() * size_of::<Vec<Value>>()
        + result
            .rows
            .iter()
            .map(|row| {
                row.len() * size_of::<Value>()
                    + row
                        .iter()
                        .map(|value| match value {
                            Value::String(value) => value.len(),
                            _ => 0,
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

fn query(database: &mut Database) -> QueryResult {
    let results = database
        .execute(SYSTEM_COLUMNS_QUERY)
        .expect("system.columns query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

fn run_cli(format: &str, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", format])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write SQL");
    child.wait_with_output().expect("wait for CLI")
}

fn http_exchange(database: &SharedDatabase, format: Option<&str>) -> Vec<u8> {
    let format_header = format.map_or_else(String::new, |format| {
        format!("X-ClickHouse-Format: {format}\r\n")
    });
    let request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\n{format_header}Content-Length: {}\r\n\r\n{}",
        SYSTEM_COLUMNS_QUERY.len(),
        SYSTEM_COLUMNS_QUERY
    );
    let mut response = Vec::new();
    handle_http_query(database, Cursor::new(request), &mut response).expect("HTTP exchange");
    response
}

fn http_body(response: &[u8]) -> &[u8] {
    let separator = b"\r\n\r\n";
    let body = response
        .windows(separator.len())
        .position(|window| window == separator)
        .expect("HTTP response has a header terminator")
        + separator.len();
    &response[body..]
}

#[test]
fn parses_only_the_exact_case_insensitive_system_columns_shape() {
    for sql in [
        SYSTEM_COLUMNS_QUERY,
        "select DATABASE, TABLE, NAME, TYPE, POSITION from SYSTEM.COLUMNS;",
        "SeLeCt database,table,name,type,position FrOm system . columns",
    ] {
        assert_eq!(
            parse(sql).expect("valid system.columns query"),
            [Statement::SystemColumns]
        );
    }

    for malformed in [
        "SELECT table, database, name, type, position FROM system.columns",
        "SELECT database, table, name, type FROM system.columns",
        "SELECT database, table, name, type, position AS ordinal FROM system.columns",
        "SELECT database, table, name, type, position FROM system.columns WHERE database = 'default'",
        "SELECT database, table, name, type, position FROM system.columns ORDER BY table",
        "SELECT database, table, name, type, position FROM system.columns LIMIT 1",
        "SELECT * FROM system.columns",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "non-exact system.columns query was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse_with_limits(
            SYSTEM_COLUMNS_QUERY,
            BatchSqlLimits {
                max_ast_list_items: 4,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 5,
            max: 4,
        })
    );
}

#[test]
fn empty_catalog_returns_the_typed_empty_metadata_shape_at_exact_limits() {
    let expected = metadata_result(&[]);
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        max_values: 0,
        max_bytes: retained_bytes(&expected),
        ..QueryResultLimits::default()
    });

    assert_eq!(query(&mut database), expected);
}

#[test]
fn metadata_tracks_column_and_table_lifecycle_in_required_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE zebra (active Bool); \
             CREATE TABLE Alpha (Id Int64, Score Float64);",
        )
        .expect("create tables");
    assert_eq!(
        query(&mut database),
        metadata_result(&[
            ("Alpha", "Id", DataType::Int64, 1),
            ("Alpha", "Score", DataType::Float64, 2),
            ("zebra", "active", DataType::Bool, 1),
        ])
    );

    database
        .execute(
            "ALTER TABLE alpha ADD COLUMN Label String; \
             ALTER TABLE ALPHA RENAME COLUMN score TO Rating;",
        )
        .expect("add and rename columns");
    assert_eq!(
        query(&mut database),
        metadata_result(&[
            ("Alpha", "Id", DataType::Int64, 1),
            ("Alpha", "Rating", DataType::Float64, 2),
            ("Alpha", "Label", DataType::String, 3),
            ("zebra", "active", DataType::Bool, 1),
        ])
    );

    database
        .execute(
            "ALTER TABLE Alpha DROP COLUMN id; \
             RENAME TABLE zebra TO Beta; \
             RENAME TABLE alpha TO zoo;",
        )
        .expect("drop column and rename tables");
    assert_eq!(
        query(&mut database),
        metadata_result(&[
            ("Beta", "active", DataType::Bool, 1),
            ("zoo", "Rating", DataType::Float64, 1),
            ("zoo", "Label", DataType::String, 2),
        ])
    );

    database
        .execute("DROP TABLE beta")
        .expect("drop renamed table");
    assert_eq!(
        query(&mut database),
        metadata_result(&[
            ("zoo", "Rating", DataType::Float64, 1),
            ("zoo", "Label", DataType::String, 2),
        ])
    );
}

#[test]
fn reports_physical_nullable_int64_and_preflights_its_exact_type_bytes() {
    let expected =
        metadata_result_with_type_names(&[("Readings", "Measurement", "Nullable(Int64)", 1)]);
    let exact_bytes = retained_bytes(&expected);

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 0,
        max_rows: 1,
        max_values: 5,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact
        .create_nullable_int64_table("Readings", "Measurement", vec![Some(7), None])
        .expect("create nullable table");
    assert_eq!(query(&mut exact), expected);

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 0,
        max_rows: 1,
        max_values: 5,
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_byte_short
        .create_nullable_int64_table("Readings", "Measurement", vec![None])
        .expect("create nullable table");
    assert_eq!(
        one_byte_short.execute(SYSTEM_COLUMNS_QUERY),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_query_and_retained_result_limits() {
    const SETUP: &str =
        "CREATE TABLE beta (enabled Bool); CREATE TABLE Alpha (id Int64, label String);";
    let expected = metadata_result(&[
        ("Alpha", "id", DataType::Int64, 1),
        ("Alpha", "label", DataType::String, 2),
        ("beta", "enabled", DataType::Bool, 1),
    ]);
    let exact_bytes = retained_bytes(&expected);
    let exact_limits = QueryResultLimits {
        max_scan_rows: 0,
        max_rows: expected.rows.len(),
        max_values: expected.rows.len() * expected.columns.len(),
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(SETUP).expect("exact-limit setup");
    assert_eq!(query(&mut exact), expected);

    let cases = [
        (
            QueryResultLimits {
                max_rows: expected.rows.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: expected.rows.len(),
                max: expected.rows.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: expected.rows.len() * expected.columns.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: expected.rows.len() * expected.columns.len(),
                max: expected.rows.len() * expected.columns.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];
    for (limits, expected_error) in cases {
        let mut database = Database::with_query_result_limits(limits);
        database.execute(SETUP).expect("limited setup");
        assert_eq!(database.execute(SYSTEM_COLUMNS_QUERY), Err(expected_error));
    }

    let mut database = Database::new();
    database.execute(SETUP).expect("retained-limit setup");
    assert!(
        database
            .execute_with_result_limit(SYSTEM_COLUMNS_QUERY, exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit(SYSTEM_COLUMNS_QUERY, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::default();
    shared.execute(SETUP).expect("shared setup");
    assert_eq!(
        shared
            .query_with_result_limit(SYSTEM_COLUMNS_QUERY, exact_bytes)
            .expect("exact shared retained limit"),
        expected
    );
    assert_eq!(
        shared.query_with_result_limit(SYSTEM_COLUMNS_QUERY, exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn cli_and_http_emit_system_columns_in_every_supported_format() {
    const SETUP: &str = "CREATE TABLE Events (id Int64, Label String);";
    let expected = metadata_result(&[
        ("Events", "id", DataType::Int64, 1),
        ("Events", "Label", DataType::String, 2),
    ]);
    let sql = format!("{SETUP} {SYSTEM_COLUMNS_QUERY};");
    for (argument, format, needs_batch_newline) in [
        ("table", OutputFormat::Table, true),
        ("csv", OutputFormat::Csv, false),
        ("tsv", OutputFormat::Tsv, false),
        ("json", OutputFormat::Json, true),
        ("JSONEachRow", OutputFormat::JsonEachRow, false),
        (
            "JSONCompactEachRow",
            OutputFormat::JsonCompactEachRow,
            false,
        ),
    ] {
        let mut expected_output = render(&expected, format).into_bytes();
        if needs_batch_newline {
            expected_output.push(b'\n');
        }
        let output = run_cli(argument, sql.as_bytes());
        assert!(output.status.success(), "{argument}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected_output, "{argument}");
        assert!(output.stderr.is_empty(), "{argument}");
    }

    let shared = SharedDatabase::default();
    shared.execute(SETUP).expect("HTTP setup");
    for (header, format) in [
        (None, OutputFormat::Json),
        (Some("CSVWithNames"), OutputFormat::Csv),
        (Some("TabSeparatedWithNames"), OutputFormat::Tsv),
        (Some("JSONEachRow"), OutputFormat::JsonEachRow),
        (Some("JSONCompactEachRow"), OutputFormat::JsonCompactEachRow),
    ] {
        let response = http_exchange(&shared, header);
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "{header:?}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(http_body(&response), render(&expected, format).as_bytes());
    }
}

#[test]
fn http_request_row_limit_bounds_system_columns_before_formatting() {
    let shared = SharedDatabase::default();
    shared
        .execute("CREATE TABLE Events (id Int64, label String)")
        .expect("HTTP setup");
    let mut response = Vec::new();
    handle_http_query(
        &shared,
        Cursor::new(
            "GET /?max_result_rows=1&query=SELECT+database%2C+table%2C+name%2C+type%2C+position+FROM+system.columns HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        &mut response,
    )
    .expect("HTTP exchange");

    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&response),
        b"{\"error\":\"SELECT result rows requires at least 2, exceeding the limit of 1\"}"
    );
}

#[test]
fn http_request_byte_limit_bounds_system_columns_before_formatting() {
    let shared = SharedDatabase::default();
    shared
        .execute("CREATE TABLE Events (id Int64, label String)")
        .expect("HTTP setup");
    let expected = metadata_result(&[
        ("Events", "id", DataType::Int64, 1),
        ("Events", "label", DataType::String, 2),
    ]);
    let exact_bytes = retained_bytes(&expected);
    let max_result_bytes = exact_bytes - 1;
    let request = format!(
        "GET /?max_result_bytes={max_result_bytes}&query=SELECT+database%2C+table%2C+name%2C+type%2C+position+FROM+system.columns HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let mut response = Vec::new();
    handle_http_query(&shared, Cursor::new(request), &mut response).expect("HTTP exchange");

    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&response),
        format!(
            "{{\"error\":\"retained query results require at least {exact_bytes} bytes, exceeding the limit of {max_result_bytes} bytes\"}}"
        )
        .as_bytes()
    );
}
