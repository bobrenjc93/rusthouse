use std::fmt::Write as _;
use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{
    BatchSqlLimits, SUPPORTED_FUNCTION_NAMES, Statement, parse, parse_with_limits,
};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{
    SharedDatabase, SharedDatabaseError, handle_http_query_read_only_with_bearer_token,
    handle_http_query_read_only_with_clickhouse_key,
};

const FUNCTIONS: [&str; 20] = [
    "ABS",
    "AVG",
    "CAST",
    "CEIL",
    "COUNT",
    "countIf",
    "currentDatabase",
    "empty",
    "FLOOR",
    "LENGTH",
    "lengthUTF8",
    "LOWER",
    "MAX",
    "MIN",
    "ROUND",
    "ROW_NUMBER",
    "SUM",
    "toString",
    "UPPER",
    "version",
];
const SYSTEM_FUNCTIONS_QUERY: &str = "SELECT name FROM system.functions";

fn expected_result() -> QueryResult {
    QueryResult {
        columns: vec![ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }],
        rows: FUNCTIONS
            .iter()
            .map(|name| vec![Value::String((*name).to_owned())])
            .collect(),
    }
}

fn retained_bytes() -> usize {
    size_of::<ResultColumn>()
        + "name".len()
        + FUNCTIONS.len() * size_of::<Vec<Value>>()
        + FUNCTIONS.len() * size_of::<Value>()
        + FUNCTIONS.iter().map(|name| name.len()).sum::<usize>()
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

fn csv_with_names() -> String {
    let mut output = "name\n".to_owned();
    for name in FUNCTIONS {
        output.push_str(name);
        output.push('\n');
    }
    output
}

fn csv_rows() -> String {
    csv_with_names()["name\n".len()..].to_owned()
}

fn table_output() -> String {
    const BORDER: &str = "+-----------------+\n";
    let mut output = BORDER.to_owned();
    output.push_str("| name            |\n");
    output.push_str(BORDER);
    for name in FUNCTIONS {
        output.push_str(&format!("| {name:<15} |\n"));
    }
    output.push_str(BORDER);
    output
}

fn json_output() -> String {
    let rows = FUNCTIONS
        .iter()
        .map(|name| format!("[\"{name}\"]"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"columns\":[{{\"name\":\"name\",\"type\":\"String\"}}],\"rows\":[{rows}]}}")
}

fn json_each_row_output() -> String {
    let mut output = String::new();
    for name in FUNCTIONS {
        writeln!(output, "{{\"name\":\"{name}\"}}").expect("writing to a String cannot fail");
    }
    output
}

fn json_compact_each_row_output() -> String {
    let mut output = String::new();
    for name in FUNCTIONS {
        writeln!(output, "[\"{name}\"]").expect("writing to a String cannot fail");
    }
    output
}

fn authenticated_http_exchange(
    database: &SharedDatabase,
    credential_header: &str,
    format: Option<&str>,
    sql: &[u8],
) -> Vec<u8> {
    let format_header = format.map_or_else(String::new, |format| {
        format!("X-ClickHouse-Format: {format}\r\n")
    });
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\n{credential_header}{format_header}Content-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    request.extend_from_slice(sql);

    let mut response = Vec::new();
    if credential_header.starts_with("Authorization:") {
        handle_http_query_read_only_with_bearer_token(
            database,
            "read-token",
            Cursor::new(request),
            &mut response,
        )
        .expect("bearer HTTP exchange succeeds");
    } else {
        handle_http_query_read_only_with_clickhouse_key(
            database,
            "read-key",
            Cursor::new(request),
            &mut response,
        )
        .expect("key HTTP exchange succeeds");
    }
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
fn parses_only_the_exact_case_insensitive_show_functions_shape() {
    for sql in ["SHOW FUNCTIONS", "sHoW fUnCtIoNs;"] {
        assert_eq!(
            parse(sql).expect("valid SHOW FUNCTIONS"),
            [Statement::ShowFunctions]
        );
    }

    for malformed in [
        "SHOW FUNCTION",
        "SHOW FUNCTIONS()",
        "SHOW FUNCTIONS system",
        "SHOW FUNCTIONS LIKE 'count%'",
        "SHOW FUNCTIONS WHERE name = 'COUNT'",
        "SHOW FUNCTIONS LIMIT 1",
        "SHOW FUNCTIONS FORMAT JSON",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "malformed SHOW FUNCTIONS was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse("SHOW FUNCTIONS extra"),
        Err(Error::Sql {
            position: 15,
            message: "unexpected trailing input after SHOW FUNCTIONS".to_owned(),
        })
    );
}

#[test]
fn parses_only_the_exact_case_insensitive_system_functions_shape() {
    for sql in [
        SYSTEM_FUNCTIONS_QUERY,
        "select NAME from SYSTEM.FUNCTIONS;",
        "SeLeCt name FrOm system . functions",
    ] {
        assert_eq!(
            parse(sql).expect("valid system.functions query"),
            [Statement::SystemFunctions]
        );
    }

    for malformed in [
        "SELECT * FROM system.functions",
        "SELECT name, name FROM system.functions",
        "SELECT name AS function FROM system.functions",
        "SELECT name FROM system.functions()",
        "SELECT name FROM system.functions WHERE name = 'COUNT'",
        "SELECT name FROM system.functions ORDER BY name",
        "SELECT name FROM system.functions LIMIT 1",
        "SELECT name FROM system.functions FORMAT JSON",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "non-exact system.functions query was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse_with_limits(
            SYSTEM_FUNCTIONS_QUERY,
            BatchSqlLimits {
                max_ast_list_items: 0,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );
}

#[test]
fn database_returns_the_exact_complete_function_inventory_in_stable_order() {
    assert_eq!(SUPPORTED_FUNCTION_NAMES, FUNCTIONS);
    assert!(
        FUNCTIONS
            .windows(2)
            .all(|pair| { pair[0].to_ascii_lowercase() < pair[1].to_ascii_lowercase() })
    );

    let mut database = Database::new();
    let show_result = query(&mut database, "show functions;");
    assert_eq!(show_result, expected_result());
    assert_eq!(
        query(&mut database, SYSTEM_FUNCTIONS_QUERY),
        show_result,
        "system.functions must mirror SHOW FUNCTIONS"
    );
}

#[test]
fn both_function_inventory_spellings_enforce_exact_query_result_limits() {
    let exact_bytes = retained_bytes();
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: FUNCTIONS.len(),
        max_values: FUNCTIONS.len(),
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    assert_eq!(query(&mut exact, "SHOW FUNCTIONS"), expected_result());
    let mut exact_system = Database::with_query_result_limits(QueryResultLimits {
        max_rows: FUNCTIONS.len(),
        max_values: FUNCTIONS.len(),
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    assert_eq!(
        query(&mut exact_system, SYSTEM_FUNCTIONS_QUERY),
        expected_result()
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: FUNCTIONS.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW FUNCTIONS result rows",
                actual: FUNCTIONS.len(),
                max: FUNCTIONS.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: FUNCTIONS.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW FUNCTIONS result values",
                actual: FUNCTIONS.len(),
                max: FUNCTIONS.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW FUNCTIONS result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        assert_eq!(database.execute("SHOW FUNCTIONS"), Err(expected));
    }

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: FUNCTIONS.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: FUNCTIONS.len(),
                max: FUNCTIONS.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: FUNCTIONS.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: FUNCTIONS.len(),
                max: FUNCTIONS.len() - 1,
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
    ] {
        let mut database = Database::with_query_result_limits(limits);
        assert_eq!(database.execute(SYSTEM_FUNCTIONS_QUERY), Err(expected));
    }

    let mut database = Database::new();
    assert!(
        database
            .execute_with_result_limit("SHOW FUNCTIONS", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit("SHOW FUNCTIONS", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
    assert!(
        database
            .execute_with_result_limit(SYSTEM_FUNCTIONS_QUERY, exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit(SYSTEM_FUNCTIONS_QUERY, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}

#[test]
fn shared_database_admits_both_function_inventory_spellings_and_boundaries() {
    let database = SharedDatabase::default();
    for result in [
        database
            .query("SHOW FUNCTIONS;")
            .expect("SHOW FUNCTIONS is read-only"),
        database
            .try_query("sHoW fUnCtIoNs")
            .expect("SHOW FUNCTIONS is a nonblocking read"),
        database
            .query(SYSTEM_FUNCTIONS_QUERY)
            .expect("system.functions is read-only"),
        database
            .try_query("sElEcT NaMe FrOm SyStEm.FuNcTiOnS;")
            .expect("system.functions is a nonblocking read"),
    ] {
        assert_eq!(result, expected_result());
    }

    let exact_bytes = retained_bytes();
    assert!(
        database
            .query_with_result_limit("SHOW FUNCTIONS", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.query_with_result_limit("SHOW FUNCTIONS", exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
    assert!(
        database
            .query_with_result_limit(SYSTEM_FUNCTIONS_QUERY, exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.query_with_result_limit(SYSTEM_FUNCTIONS_QUERY, exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn cli_emits_both_function_inventory_spellings_in_every_output_format() {
    let mut json = json_output();
    json.push('\n');
    let cases = [
        ("table", table_output()),
        ("csv", csv_with_names()),
        ("tsv", csv_with_names()),
        ("json", json),
        ("JSONEachRow", json_each_row_output()),
        ("JSONCompactEachRow", json_compact_each_row_output()),
    ];

    for (format, expected) in cases {
        for (spelling, sql) in [
            ("SHOW FUNCTIONS", "SHOW FUNCTIONS;"),
            ("system.functions", "SELECT name FROM system.functions;"),
        ] {
            let output = run_cli(format, sql.as_bytes());
            assert!(
                output.status.success(),
                "{spelling} {format}: {:?}",
                output.stderr
            );
            assert_eq!(output.stdout, expected.as_bytes(), "{spelling} {format}");
            assert!(output.stderr.is_empty(), "{spelling} {format}");
        }
    }
}

#[test]
fn authenticated_read_only_http_paths_emit_every_wire_format() {
    let database = SharedDatabase::default();
    let json = json_output();
    let rows = csv_rows();
    let cases = [
        (None, json),
        (Some("CSV"), rows.clone()),
        (Some("CSVWithNames"), csv_with_names()),
        (Some("TabSeparated"), rows),
        (Some("TabSeparatedWithNames"), csv_with_names()),
        (Some("JSONEachRow"), json_each_row_output()),
        (Some("JSONCompactEachRow"), json_compact_each_row_output()),
    ];

    for (format, expected) in cases {
        for (spelling, sql) in [
            ("SHOW FUNCTIONS", b"SHOW FUNCTIONS;".as_slice()),
            (
                "system.functions",
                b"SELECT name FROM system.functions;".as_slice(),
            ),
        ] {
            let response = authenticated_http_exchange(
                &database,
                "Authorization: Bearer read-token\r\n",
                format,
                sql,
            );
            assert!(
                response.starts_with(b"HTTP/1.1 200 OK\r\n"),
                "{spelling} {format:?}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(
                http_body(&response),
                expected.as_bytes(),
                "{spelling} {format:?}"
            );
        }
    }

    let key_response = authenticated_http_exchange(
        &database,
        "X-ClickHouse-Key: read-key\r\n",
        None,
        SYSTEM_FUNCTIONS_QUERY.as_bytes(),
    );
    assert!(key_response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(http_body(&key_response), json_output().as_bytes());
}

#[test]
fn authenticated_http_enforces_system_functions_at_the_exact_row_limit() {
    let database = SharedDatabase::default();

    for (credential_header, bearer) in [
        ("Authorization: Bearer read-token", true),
        ("X-ClickHouse-Key: read-key", false),
    ] {
        for (max_rows, expected_status, expected_body) in [
            (FUNCTIONS.len(), "200 OK", json_output()),
            (
                FUNCTIONS.len() - 1,
                "400 Bad Request",
                format!(
                    "{{\"error\":\"SELECT result rows requires at least {}, exceeding the limit of {}\"}}",
                    FUNCTIONS.len(),
                    FUNCTIONS.len() - 1
                ),
            ),
        ] {
            let request = format!(
                "GET /?query=SELECT+name+FROM+system.functions&max_result_rows={max_rows} HTTP/1.1\r\nHost: localhost\r\n{credential_header}\r\n\r\n"
            );
            let mut response = Vec::new();
            if bearer {
                handle_http_query_read_only_with_bearer_token(
                    &database,
                    "read-token",
                    Cursor::new(request),
                    &mut response,
                )
                .expect("bearer HTTP exchange succeeds");
            } else {
                handle_http_query_read_only_with_clickhouse_key(
                    &database,
                    "read-key",
                    Cursor::new(request),
                    &mut response,
                )
                .expect("key HTTP exchange succeeds");
            }

            assert!(
                response.starts_with(format!("HTTP/1.1 {expected_status}\r\n").as_bytes()),
                "{}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(http_body(&response), expected_body.as_bytes());
        }
    }
}
