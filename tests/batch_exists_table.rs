use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

fn result_bytes() -> usize {
    size_of::<ResultColumn>() + "result".len() + size_of::<Vec<Value>>() + size_of::<Value>()
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

fn http_request(sql: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    request.extend_from_slice(sql);
    request
}

fn http_exchange(database: &SharedDatabase, sql: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query(database, Cursor::new(http_request(sql)), &mut response)
        .expect("HTTP exchange succeeds");
    response
}

fn assert_http_response(response: &[u8], status: &str, body: &str) {
    let response = std::str::from_utf8(response).expect("response is UTF-8");
    let (headers, actual_body) = response
        .split_once("\r\n\r\n")
        .expect("response has headers");
    assert!(headers.starts_with(status), "{headers}");
    assert!(
        headers.contains(&format!("Content-Length: {}", body.len())),
        "{headers}"
    );
    assert_eq!(actual_body, body);
}

#[test]
fn parses_only_the_exact_exists_table_shape_with_an_optional_semicolon() {
    for (sql, name) in [
        ("EXISTS TABLE metrics", "metrics"),
        ("eXiStS tAbLe Metrics;", "Metrics"),
    ] {
        assert_eq!(
            parse(sql).expect("valid EXISTS TABLE"),
            [Statement::ExistsTable {
                name: name.to_owned(),
            }]
        );
    }

    for malformed in [
        "EXISTS",
        "EXISTS metrics",
        "EXISTS TABLE",
        "EXISTS TABLE metrics extra",
        "EXISTS TABLE metrics LIMIT 1",
        "EXISTS TABLE metrics()",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "malformed EXISTS TABLE was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse("EXISTS TABLE metrics extra"),
        Err(Error::Sql {
            position: 21,
            message: "unexpected trailing input after EXISTS TABLE <name>".to_owned(),
        })
    );
}

#[test]
fn database_returns_one_bool_for_missing_present_and_post_drop_tables() {
    let mut database = Database::new();
    let expected_columns = vec![ResultColumn {
        name: "result".to_owned(),
        data_type: DataType::Bool,
    }];

    assert_eq!(
        query(&mut database, "EXISTS TABLE metrics"),
        QueryResult {
            columns: expected_columns.clone(),
            rows: vec![vec![Value::Bool(false)]],
        }
    );

    database
        .execute("CREATE TABLE Metrics (id Int64);")
        .expect("create table");
    assert_eq!(
        query(&mut database, "EXISTS TABLE mEtRiCs;").rows,
        [vec![Value::Bool(true)]]
    );

    database.execute("DROP TABLE METRICS;").expect("drop table");
    assert_eq!(
        query(&mut database, "EXISTS TABLE metrics;").rows,
        [vec![Value::Bool(false)]]
    );
}

#[test]
fn exists_table_accepts_exact_and_rejects_exceeded_query_result_limits() {
    let exact_bytes = result_bytes();
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact
        .execute("CREATE TABLE metrics (id Int64);")
        .expect("setup succeeds");
    assert_eq!(
        query(&mut exact, "EXISTS TABLE METRICS;").rows,
        [vec![Value::Bool(true)]]
    );

    let cases = [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "EXISTS TABLE result rows",
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
                resource: "EXISTS TABLE result values",
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
                resource: "EXISTS TABLE result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];

    for (limits, expected) in cases {
        let mut database = Database::with_query_result_limits(limits);
        assert_eq!(database.execute("EXISTS TABLE missing;"), Err(expected));
    }

    let mut retained = Database::new();
    assert!(
        retained
            .execute_with_result_limit("EXISTS TABLE missing;", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        retained.execute_with_result_limit("EXISTS TABLE missing;", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}

#[test]
fn shared_database_treats_exists_table_as_a_bounded_read_only_query() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE Events (id Int64);")
        .expect("setup succeeds");

    assert_eq!(
        database
            .query("EXISTS TABLE events;")
            .expect("EXISTS TABLE is read-only")
            .rows,
        [vec![Value::Bool(true)]]
    );

    let exact_bytes = result_bytes();
    assert!(
        database
            .query_with_result_limit("EXISTS TABLE missing;", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.query_with_result_limit("EXISTS TABLE missing;", exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn cli_emits_exists_table_lifecycle_in_every_output_format() {
    let sql = b"EXISTS TABLE events; \
        CREATE TABLE Events (id Int64); \
        EXISTS TABLE EVENTS; \
        DROP TABLE events; \
        EXISTS TABLE EvEnTs;";
    let cases: [(&str, &[u8]); 4] = [
        (
            "table",
            b"+--------+\n\
              | result |\n\
              +--------+\n\
              | false  |\n\
              +--------+\n\
              \n\
              +--------+\n\
              | result |\n\
              +--------+\n\
              | true   |\n\
              +--------+\n\
              \n\
              +--------+\n\
              | result |\n\
              +--------+\n\
              | false  |\n\
              +--------+\n",
        ),
        ("csv", b"result\nfalse\nresult\ntrue\nresult\nfalse\n"),
        ("tsv", b"result\nfalse\nresult\ntrue\nresult\nfalse\n"),
        (
            "json",
            concat!(
                r#"{"columns":[{"name":"result","type":"Bool"}],"rows":[[false]]}"#,
                "\n",
                r#"{"columns":[{"name":"result","type":"Bool"}],"rows":[[true]]}"#,
                "\n",
                r#"{"columns":[{"name":"result","type":"Bool"}],"rows":[[false]]}"#,
                "\n"
            )
            .as_bytes(),
        ),
    ];

    for (format, expected) in cases {
        let output = run_cli(format, sql);
        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected, "{format}");
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn http_exposes_exists_table_and_its_query_limits_as_json() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE Events (id Int64);")
        .expect("setup succeeds");

    assert_http_response(
        &http_exchange(&database, b"EXISTS TABLE eVeNtS;"),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"result","type":"Bool"}],"rows":[[true]]}"#,
    );
    database
        .execute("DROP TABLE events;")
        .expect("drop succeeds");
    assert_http_response(
        &http_exchange(&database, b"EXISTS TABLE events;"),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"result","type":"Bool"}],"rows":[[false]]}"#,
    );

    let limited = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        ..QueryResultLimits::default()
    });
    assert_http_response(
        &http_exchange(&limited, b"EXISTS TABLE missing;"),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"EXISTS TABLE result rows requires at least 1, exceeding the limit of 0"}"#,
    );
}
