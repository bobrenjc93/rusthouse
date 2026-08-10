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

const SYSTEM_METRICS_QUERY: &str = "SELECT metric, value FROM system.metrics";

fn columns() -> Vec<ResultColumn> {
    vec![
        ResultColumn {
            name: "metric".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "value".to_owned(),
            data_type: DataType::Int64,
        },
    ]
}

fn metrics_result(tables: i64, column_count: i64, rows: i64, value_bytes: i64) -> QueryResult {
    QueryResult {
        columns: columns(),
        rows: [
            ("rusthouse_tables", tables),
            ("rusthouse_columns", column_count),
            ("rusthouse_retained_rows", rows),
            ("rusthouse_retained_value_bytes", value_bytes),
            ("rusthouse_index_scanned_blocks", 0),
            ("rusthouse_index_pruned_blocks", 0),
        ]
        .into_iter()
        .map(|(metric, value)| vec![Value::String(metric.to_owned()), Value::Int64(value)])
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
        .execute(SYSTEM_METRICS_QUERY)
        .expect("system.metrics query succeeds");
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
        SYSTEM_METRICS_QUERY.len(),
        SYSTEM_METRICS_QUERY
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
fn parses_only_the_exact_case_insensitive_system_metrics_shape() {
    for sql in [
        SYSTEM_METRICS_QUERY,
        "select METRIC, VALUE from SYSTEM.METRICS;",
        "SeLeCt metric,value FrOm system . metrics",
    ] {
        assert_eq!(
            parse(sql).expect("valid system.metrics query"),
            [Statement::SystemMetrics]
        );
    }

    for malformed in [
        "SELECT value, metric FROM system.metrics",
        "SELECT metric FROM system.metrics",
        "SELECT metric, value AS gauge FROM system.metrics",
        "SELECT metric, value FROM system.metrics WHERE value > 0",
        "SELECT metric, value FROM system.metrics ORDER BY metric",
        "SELECT metric, value FROM system.metrics LIMIT 1",
        "SELECT * FROM system.metrics",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "non-exact system.metrics query was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse_with_limits(
            SYSTEM_METRICS_QUERY,
            BatchSqlLimits {
                max_ast_list_items: 1,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn metrics_follow_table_column_row_and_payload_lifecycle() {
    let mut database = Database::new();
    assert_eq!(query(&mut database), metrics_result(0, 0, 0, 0));

    database
        .execute(
            "CREATE TABLE Alpha (id Int64, label String); \
             INSERT INTO alpha VALUES (1, 'é'), (2, 'rust');",
        )
        .expect("create and insert");
    assert_eq!(query(&mut database), metrics_result(1, 2, 2, 22));

    database
        .execute(
            "ALTER TABLE ALPHA ADD COLUMN active Bool; \
             ALTER TABLE alpha UPDATE label = 'x' WHERE id = 1;",
        )
        .expect("add and update");
    assert_eq!(query(&mut database), metrics_result(1, 3, 2, 23));

    database
        .execute("DELETE FROM alpha WHERE id = 2; ALTER TABLE alpha DROP COLUMN label;")
        .expect("delete and drop column");
    assert_eq!(query(&mut database), metrics_result(1, 2, 1, 9));

    database
        .execute("TRUNCATE TABLE alpha; CREATE TABLE beta (score Float64);")
        .expect("truncate and create second table");
    assert_eq!(query(&mut database), metrics_result(2, 3, 0, 0));

    database
        .execute("DROP TABLE alpha; DROP TABLE beta;")
        .expect("drop tables");
    assert_eq!(query(&mut database), metrics_result(0, 0, 0, 0));
}

#[test]
fn query_and_retained_result_limits_accept_exact_and_reject_one_less() {
    let expected = metrics_result(0, 0, 0, 0);
    let exact_bytes = retained_bytes(&expected);
    let exact_limits = QueryResultLimits {
        max_scan_rows: 0,
        max_rows: expected.rows.len(),
        max_values: expected.rows.len() * expected.columns.len(),
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
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
        assert_eq!(database.execute(SYSTEM_METRICS_QUERY), Err(expected_error));
    }

    let mut database = Database::new();
    assert!(
        database
            .execute_with_result_limit(SYSTEM_METRICS_QUERY, exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit(SYSTEM_METRICS_QUERY, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::default();
    assert_eq!(
        shared
            .query_with_result_limit(SYSTEM_METRICS_QUERY, exact_bytes)
            .expect("exact shared retained limit"),
        expected
    );
    assert_eq!(
        shared.query_with_result_limit(SYSTEM_METRICS_QUERY, exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn shared_database_exposes_the_same_consistent_cached_gauges() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE Events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'one'), (2, 'é');",
        )
        .expect("shared setup");
    let expected = metrics_result(1, 2, 2, 21);

    assert_eq!(
        database.query(SYSTEM_METRICS_QUERY).expect("shared query"),
        expected
    );
    assert_eq!(
        database
            .try_query(SYSTEM_METRICS_QUERY)
            .expect("nonblocking shared query"),
        expected
    );
}

#[test]
fn cli_and_http_emit_system_metrics_in_every_supported_format() {
    const SETUP: &str =
        "CREATE TABLE Events (id Int64, label String); INSERT INTO events VALUES (1, 'é');";
    let expected = metrics_result(1, 2, 1, 10);
    let sql = format!("{SETUP} {SYSTEM_METRICS_QUERY};");
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
