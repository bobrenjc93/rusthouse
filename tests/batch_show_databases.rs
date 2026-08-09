use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

const SYSTEM_DATABASES_QUERY: &str = "SELECT name FROM system.databases";

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

fn result_bytes() -> usize {
    size_of::<ResultColumn>()
        + "name".len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>()
        + "default".len()
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
fn parses_only_the_exact_case_insensitive_show_databases_shape() {
    for sql in ["SHOW DATABASES", "sHoW dAtAbAsEs;"] {
        assert_eq!(
            parse(sql).expect("valid SHOW DATABASES"),
            [Statement::ShowDatabases]
        );
    }

    for malformed in [
        "SHOW DATABASE",
        "SHOW DATABASES default",
        "SHOW DATABASES()",
        "SHOW DATABASES FROM system",
        "SHOW DATABASES LIKE 'default'",
        "SHOW DATABASES WHERE name = 'default'",
        "SHOW DATABASES LIMIT 1",
        "SHOW DATABASES FORMAT JSON",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "malformed SHOW DATABASES was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse("SHOW DATABASES extra"),
        Err(Error::Sql {
            position: 15,
            message: "unexpected trailing input after SHOW DATABASES".to_owned(),
        })
    );
}

#[test]
fn parses_only_the_exact_case_insensitive_system_databases_shape() {
    for sql in [
        SYSTEM_DATABASES_QUERY,
        "select NAME from SYSTEM.DATABASES;",
        "SeLeCt name FrOm system . databases",
    ] {
        assert_eq!(
            parse(sql).expect("valid system.databases query"),
            [Statement::SystemDatabases]
        );
    }

    for malformed in [
        "SELECT database FROM system.databases",
        "SELECT name, name FROM system.databases",
        "SELECT name AS database FROM system.databases",
        "SELECT upper(name) FROM system.databases",
        "SELECT name FROM system.databases()",
        "SELECT name FROM system.databases WHERE name = 'default'",
        "SELECT name FROM system.databases ORDER BY name",
        "SELECT name FROM system.databases LIMIT 1",
        "SELECT * FROM system.databases",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "non-exact system.databases query was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse_with_limits(
            SYSTEM_DATABASES_QUERY,
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
fn database_returns_the_single_default_database() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Events (id Int64)")
        .expect("setup succeeds");

    let expected = QueryResult {
        columns: vec![ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }],
        rows: vec![vec![Value::String("default".to_owned())]],
    };
    assert_eq!(query(&mut database, "show databases;"), expected);
    assert_eq!(
        query(&mut database, SYSTEM_DATABASES_QUERY),
        expected,
        "system.databases must reuse the SHOW DATABASES result"
    );
}

#[test]
fn show_databases_accepts_exact_and_rejects_exceeded_query_result_limits() {
    let exact_bytes = result_bytes();
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    assert_eq!(
        query(&mut exact, "SHOW DATABASES").rows,
        [vec![Value::String("default".to_owned())]]
    );

    let cases = [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW DATABASES result rows",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_values: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW DATABASES result values",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW DATABASES result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];

    for (limits, expected) in cases {
        for sql in ["SHOW DATABASES", SYSTEM_DATABASES_QUERY] {
            let mut database = Database::with_query_result_limits(limits);
            assert_eq!(database.execute(sql), Err(expected.clone()), "{sql}");
        }
    }
}

#[test]
fn database_and_shared_database_apply_retained_result_limits() {
    let exact_bytes = result_bytes();
    for sql in ["SHOW DATABASES", SYSTEM_DATABASES_QUERY] {
        let mut database = Database::new();
        assert!(database.execute_with_result_limit(sql, exact_bytes).is_ok());
        assert_eq!(
            database.execute_with_result_limit(sql, exact_bytes - 1),
            Err(Error::ResultLimitExceeded {
                bytes: exact_bytes,
                max_bytes: exact_bytes - 1,
            }),
            "{sql}"
        );

        let shared = SharedDatabase::default();
        assert!(shared.query_with_result_limit(sql, exact_bytes).is_ok());
        assert_eq!(
            shared.query_with_result_limit(sql, exact_bytes - 1),
            Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
                bytes: exact_bytes,
                max_bytes: exact_bytes - 1,
            })),
            "{sql}"
        );
    }
}

#[test]
fn shared_database_admits_show_databases_on_blocking_and_nonblocking_read_paths() {
    let database = SharedDatabase::default();
    for result in [
        database
            .query("SHOW DATABASES;")
            .expect("SHOW DATABASES is read-only"),
        database
            .try_query("sHoW dAtAbAsEs")
            .expect("SHOW DATABASES is a nonblocking read"),
        database
            .query(SYSTEM_DATABASES_QUERY)
            .expect("system.databases is read-only"),
        database
            .try_query("select NAME from SYSTEM.DATABASES;")
            .expect("system.databases is a nonblocking read"),
    ] {
        assert_eq!(result.rows, [vec![Value::String("default".to_owned())]]);
    }
}

#[test]
fn cli_emits_show_databases_in_every_output_format() {
    let cases: [(&str, &[u8]); 6] = [
        (
            "table",
            b"+---------+\n\
              | name    |\n\
              +---------+\n\
              | default |\n\
              +---------+\n",
        ),
        ("csv", b"name\ndefault\n"),
        ("tsv", b"name\ndefault\n"),
        (
            "json",
            b"{\"columns\":[{\"name\":\"name\",\"type\":\"String\"}],\"rows\":[[\"default\"]]}\n",
        ),
        ("JSONEachRow", b"{\"name\":\"default\"}\n"),
        ("JSONCompactEachRow", b"[\"default\"]\n"),
    ];

    for (format, expected) in cases {
        for sql in ["SHOW DATABASES;", "SELECT name FROM system.databases;"] {
            let output = run_cli(format, sql.as_bytes());
            assert!(
                output.status.success(),
                "{sql} {format}: {:?}",
                output.stderr
            );
            assert_eq!(output.stdout, expected, "{sql} {format}");
            assert!(output.stderr.is_empty(), "{sql} {format}");
        }
    }
}

#[test]
fn http_emits_show_databases_in_every_supported_wire_format() {
    let database = SharedDatabase::default();
    let cases: [(Option<&str>, &[u8]); 5] = [
        (
            None,
            b"{\"columns\":[{\"name\":\"name\",\"type\":\"String\"}],\"rows\":[[\"default\"]]}",
        ),
        (Some("CSVWithNames"), b"name\ndefault\n"),
        (Some("TabSeparatedWithNames"), b"name\ndefault\n"),
        (Some("JSONEachRow"), b"{\"name\":\"default\"}\n"),
        (Some("JSONCompactEachRow"), b"[\"default\"]\n"),
    ];

    for (format, expected) in cases {
        for sql in ["SHOW DATABASES;", "SELECT name FROM system.databases;"] {
            let response = http_exchange(&database, format, sql.as_bytes());
            assert!(
                response.starts_with(b"HTTP/1.1 200 OK\r\n"),
                "{sql} {format:?}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(http_body(&response), expected, "{sql} {format:?}");
        }
    }
}

#[test]
fn http_reports_show_databases_result_limits_on_the_wire() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        ..QueryResultLimits::default()
    });
    let response = http_exchange(&database, None, SYSTEM_DATABASES_QUERY.as_bytes());

    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&response),
        b"{\"error\":\"SHOW DATABASES result rows requires at least 1, exceeding the limit of 0\"}"
    );
}
