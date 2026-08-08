use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult, TableLimits,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::{OutputFormat, render};
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

const SETTING_ROWS: usize = 13;
const SETTING_VALUES: usize = SETTING_ROWS * 2;

fn expected_result(query_limits: QueryResultLimits, table_limits: TableLimits) -> QueryResult {
    let settings = [
        (
            "query_result_limits.max_scan_rows",
            query_limits.max_scan_rows,
        ),
        ("query_result_limits.max_rows", query_limits.max_rows),
        ("query_result_limits.max_values", query_limits.max_values),
        ("query_result_limits.max_bytes", query_limits.max_bytes),
        (
            "query_result_limits.max_ordering_state_bytes",
            query_limits.max_ordering_state_bytes,
        ),
        ("query_result_limits.max_groups", query_limits.max_groups),
        (
            "query_result_limits.max_group_key_cells",
            query_limits.max_group_key_cells,
        ),
        (
            "query_result_limits.max_group_key_bytes",
            query_limits.max_group_key_bytes,
        ),
        (
            "query_result_limits.max_aggregate_state_cells",
            query_limits.max_aggregate_state_cells,
        ),
        (
            "query_result_limits.max_aggregate_state_bytes",
            query_limits.max_aggregate_state_bytes,
        ),
        ("table_limits.max_rows", table_limits.max_rows),
        ("table_limits.max_columns", table_limits.max_columns),
        ("table_limits.max_cells", table_limits.max_cells),
    ];

    QueryResult {
        columns: vec![
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "value".to_owned(),
                data_type: DataType::String,
            },
        ],
        rows: settings
            .into_iter()
            .map(|(name, value)| {
                vec![
                    Value::String(name.to_owned()),
                    Value::String(value.to_string()),
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

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
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

fn http_exchange(database: &SharedDatabase, format: Option<&str>, sql: &[u8]) -> Vec<u8> {
    let format_header = format.map_or_else(String::new, |format| {
        format!("X-ClickHouse-Format: {format}\r\n")
    });
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\n{format_header}Content-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    request.extend_from_slice(sql);

    let mut response = Vec::new();
    handle_http_query(database, Cursor::new(request), &mut response)
        .expect("HTTP exchange succeeds");
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
fn parses_only_the_exact_case_insensitive_show_settings_shape() {
    for sql in ["SHOW SETTINGS", "sHoW sEtTiNgS;"] {
        assert_eq!(
            parse(sql).expect("valid SHOW SETTINGS"),
            [Statement::ShowSettings]
        );
    }

    for malformed in [
        "SHOW SETTING",
        "SHOW SETTINGS()",
        "SHOW SETTINGS LIKE 'max%'",
        "SHOW SETTINGS WHERE name = 'max_rows'",
        "SHOW SETTINGS LIMIT 1",
        "SHOW SETTINGS FORMAT JSON",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "malformed SHOW SETTINGS was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse("SHOW SETTINGS extra"),
        Err(Error::Sql {
            position: 14,
            message: "unexpected trailing input after SHOW SETTINGS".to_owned(),
        })
    );
}

#[test]
fn returns_every_custom_query_and_table_limit_in_stable_order() {
    let query_limits = QueryResultLimits {
        max_scan_rows: 101,
        max_rows: SETTING_ROWS,
        max_values: SETTING_VALUES,
        max_bytes: 50_000,
        max_ordering_state_bytes: 105,
        max_groups: 106,
        max_group_key_cells: 107,
        max_group_key_bytes: 108,
        max_aggregate_state_cells: 109,
        max_aggregate_state_bytes: 110,
    };
    let mut query_limited = Database::with_query_result_limits(query_limits);
    assert_eq!(
        query(&mut query_limited, "show settings;"),
        expected_result(query_limits, TableLimits::default())
    );

    let table_limits = TableLimits::new(201, 202, 203);
    let mut table_limited = Database::with_table_limits(table_limits);
    assert_eq!(
        query(&mut table_limited, "SHOW SETTINGS"),
        expected_result(QueryResultLimits::default(), table_limits)
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_query_result_limits() {
    let table_limits = TableLimits::default();
    let mut exact_limits = QueryResultLimits {
        max_rows: SETTING_ROWS,
        max_values: SETTING_VALUES,
        max_bytes: 0,
        ..QueryResultLimits::default()
    };
    for _ in 0..8 {
        let bytes = retained_bytes(&expected_result(exact_limits, table_limits));
        if bytes == exact_limits.max_bytes {
            break;
        }
        exact_limits.max_bytes = bytes;
    }
    let exact_bytes = retained_bytes(&expected_result(exact_limits, table_limits));
    assert_eq!(exact_limits.max_bytes, exact_bytes);

    let mut exact = Database::with_query_result_limits(exact_limits);
    assert_eq!(
        query(&mut exact, "SHOW SETTINGS"),
        expected_result(exact_limits, table_limits)
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: SETTING_ROWS - 1,
                max_bytes: 50_000,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW SETTINGS result rows",
                actual: SETTING_ROWS,
                max: SETTING_ROWS - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: SETTING_VALUES - 1,
                max_bytes: 50_000,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW SETTINGS result values",
                actual: SETTING_VALUES,
                max: SETTING_VALUES - 1,
            },
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        assert_eq!(database.execute("SHOW SETTINGS"), Err(expected));
    }

    let byte_limited = QueryResultLimits {
        max_bytes: exact_bytes - 1,
        ..exact_limits
    };
    let actual_bytes = retained_bytes(&expected_result(byte_limited, table_limits));
    let mut database = Database::with_query_result_limits(byte_limited);
    assert_eq!(
        database.execute("SHOW SETTINGS"),
        Err(Error::ResourceLimitExceeded {
            resource: "SHOW SETTINGS result bytes",
            actual: actual_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn database_and_shared_database_apply_exact_retained_result_bounds() {
    let query_limits = QueryResultLimits {
        max_bytes: 50_000,
        ..QueryResultLimits::default()
    };
    let exact_bytes = retained_bytes(&expected_result(query_limits, TableLimits::default()));
    let mut database = Database::with_query_result_limits(query_limits);
    assert!(
        database
            .execute_with_result_limit("SHOW SETTINGS", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit("SHOW SETTINGS", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::with_query_result_limits(query_limits);
    assert!(
        shared
            .query_with_result_limit("SHOW SETTINGS", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        shared.query_with_result_limit("SHOW SETTINGS", exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn shared_database_admits_show_settings_on_blocking_and_nonblocking_read_paths() {
    let database = SharedDatabase::default();
    let expected = expected_result(QueryResultLimits::default(), TableLimits::default());
    assert_eq!(
        database
            .query("SHOW SETTINGS;")
            .expect("SHOW SETTINGS is read-only"),
        expected
    );
    assert_eq!(
        database
            .try_query("sHoW sEtTiNgS")
            .expect("SHOW SETTINGS is a nonblocking read"),
        expected
    );
}

#[test]
fn cli_emits_show_settings_in_every_output_format() {
    let result = expected_result(QueryResultLimits::default(), TableLimits::default());
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
        let mut expected = render(&result, format).into_bytes();
        if needs_batch_newline {
            expected.push(b'\n');
        }
        let output = run_cli(argument, b"SHOW SETTINGS;");
        assert!(output.status.success(), "{argument}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected, "{argument}");
        assert!(output.stderr.is_empty(), "{argument}");
    }
}

#[test]
fn http_emits_show_settings_in_every_supported_wire_format() {
    let database = SharedDatabase::default();
    let result = expected_result(QueryResultLimits::default(), TableLimits::default());
    for (header, format) in [
        (None, OutputFormat::Json),
        (Some("CSVWithNames"), OutputFormat::Csv),
        (Some("TabSeparatedWithNames"), OutputFormat::Tsv),
        (Some("JSONEachRow"), OutputFormat::JsonEachRow),
        (Some("JSONCompactEachRow"), OutputFormat::JsonCompactEachRow),
    ] {
        let response = http_exchange(&database, header, b"SHOW SETTINGS;");
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "{header:?}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(http_body(&response), render(&result, format).as_bytes());
    }
}

#[test]
fn http_reports_show_settings_result_limits_on_the_wire() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: SETTING_ROWS - 1,
        ..QueryResultLimits::default()
    });
    let response = http_exchange(&database, None, b"SHOW SETTINGS");

    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&response),
        b"{\"error\":\"SHOW SETTINGS result rows requires at least 13, exceeding the limit of 12\"}"
    );
}
