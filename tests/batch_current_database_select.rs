use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{Database, QueryResultLimits, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{self, BatchSqlLimits, Statement};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

const DATABASE_NAME: &str = "default";

fn query_result(result: &StatementResult) -> &rusthouse::batch::engine::QueryResult {
    let StatementResult::Query(result) = result else {
        panic!("expected a query result");
    };
    result
}

fn run_cli(format: &str, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", format])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
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
    handle_http_query(database, Cursor::new(request), &mut response).unwrap();
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
fn parses_exact_case_insensitive_current_database_probes_and_aliases() {
    let statements = sql::parse(
        "SELECT currentDatabase(); SELECT CuRrEnTdAtAbAsE() AS databaseName; SELECT CURRENTDATABASE() AS db;",
    )
    .unwrap();

    let expected_aliases = [None, Some("databaseName"), Some("db")];
    for (statement, expected_alias) in statements.iter().zip(expected_aliases) {
        let Statement::CurrentDatabaseSelect(select) = statement else {
            panic!("expected a currentDatabase SELECT");
        };
        assert_eq!(select.alias.as_deref(), expected_alias);
    }
}

#[test]
fn rejects_current_database_arguments_missing_aliases_and_trailing_clauses() {
    for sql in [
        "SELECT currentDatabase(1)",
        "SELECT currentDatabase('x')",
        "SELECT currentDatabase(*)",
        "SELECT currentDatabase(NULL)",
        "SELECT currentDatabase(1, 2)",
        "SELECT currentDatabase(",
        "SELECT currentDatabase())",
        "SELECT currentDatabase() AS",
        "SELECT currentDatabase() db",
        "SELECT currentDatabase(), 1",
        "SELECT currentDatabase() FROM system.one",
        "SELECT currentDatabase() WHERE true",
        "SELECT currentDatabase() ORDER BY database",
        "SELECT currentDatabase() LIMIT 1",
        "SELECT currentDatabase() UNION ALL SELECT currentDatabase()",
    ] {
        assert!(
            sql::parse(sql).is_err(),
            "malformed currentDatabase SELECT was accepted: {sql}"
        );
    }
}

#[test]
fn current_database_probe_charges_exactly_one_ast_list_item() {
    let no_items = BatchSqlLimits {
        max_ast_list_items: 0,
        ..BatchSqlLimits::default()
    };
    assert_eq!(
        sql::parse_with_limits("SELECT currentDatabase()", no_items),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );

    let one_item = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    assert!(sql::parse_with_limits("SELECT currentDatabase()", one_item).is_ok());
    assert_eq!(
        sql::parse_with_limits(
            "SELECT currentDatabase(); SELECT CURRENTDATABASE()",
            one_item
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn database_and_shared_database_return_the_default_database() {
    let mut database = Database::new();
    let results = database
        .execute("SELECT currentDatabase(); SELECT CURRENTDATABASE() AS db;")
        .unwrap();

    for (result, name) in results.iter().zip(["currentDatabase()", "db"]) {
        let result = query_result(result);
        assert_eq!(
            result.columns,
            vec![ResultColumn {
                name: name.to_owned(),
                data_type: DataType::String,
            }]
        );
        assert_eq!(
            result.rows,
            vec![vec![Value::String(DATABASE_NAME.to_owned())]]
        );
    }

    let shared = SharedDatabase::default();
    let result = shared
        .query("SeLeCt CuRrEnTdAtAbAsE() AS database_name;")
        .unwrap();
    assert_eq!(result.columns[0].name, "database_name");
    assert_eq!(
        result.rows,
        vec![vec![Value::String(DATABASE_NAME.to_owned())]]
    );
}

#[test]
fn current_database_probe_obeys_query_and_retained_result_limits() {
    let exact_bytes = size_of::<ResultColumn>()
        + "currentDatabase()".len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>()
        + DATABASE_NAME.len();

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    assert!(exact.execute("SELECT currentDatabase()").is_ok());

    for (limits, resource, actual, max) in [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            "SELECT result rows",
            1,
            0,
        ),
        (
            QueryResultLimits {
                max_values: 0,
                ..QueryResultLimits::default()
            },
            "SELECT result values",
            1,
            0,
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            "SELECT result bytes",
            exact_bytes,
            exact_bytes - 1,
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        assert_eq!(
            database.execute("SELECT currentDatabase()"),
            Err(Error::ResourceLimitExceeded {
                resource,
                actual,
                max,
            })
        );
    }

    let mut database = Database::new();
    assert!(
        database
            .execute_with_result_limit("SELECT currentDatabase()", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit("SELECT currentDatabase()", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::default();
    assert!(
        shared
            .query_with_result_limit("SELECT currentDatabase()", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        shared.query_with_result_limit("SELECT currentDatabase()", exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn cli_emits_current_database_probe_in_every_output_format() {
    let width = "currentDatabase()".len();
    let border = format!("+{}+\n", "-".repeat(width + 2));
    let table = format!(
        "{border}| {:width$} |\n{border}| {:width$} |\n{border}",
        "currentDatabase()", DATABASE_NAME
    );
    let cases = [
        ("table", table),
        ("csv", format!("currentDatabase()\n{DATABASE_NAME}\n")),
        ("tsv", format!("currentDatabase()\n{DATABASE_NAME}\n")),
        (
            "json",
            format!(
                "{{\"columns\":[{{\"name\":\"currentDatabase()\",\"type\":\"String\"}}],\"rows\":[[\"{DATABASE_NAME}\"]]}}\n"
            ),
        ),
        (
            "JSONEachRow",
            format!("{{\"currentDatabase()\":\"{DATABASE_NAME}\"}}\n"),
        ),
        ("JSONCompactEachRow", format!("[\"{DATABASE_NAME}\"]\n")),
    ];

    for (format, expected) in cases {
        let output = run_cli(format, b"SELECT currentDatabase();");
        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected.as_bytes(), "{format}");
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn http_emits_current_database_probe_in_every_supported_wire_format() {
    let database = SharedDatabase::default();
    let cases = [
        (
            None,
            format!(
                "{{\"columns\":[{{\"name\":\"currentDatabase()\",\"type\":\"String\"}}],\"rows\":[[\"{DATABASE_NAME}\"]]}}"
            ),
        ),
        (
            Some("CSVWithNames"),
            format!("currentDatabase()\n{DATABASE_NAME}\n"),
        ),
        (
            Some("TabSeparatedWithNames"),
            format!("currentDatabase()\n{DATABASE_NAME}\n"),
        ),
        (
            Some("JSONEachRow"),
            format!("{{\"currentDatabase()\":\"{DATABASE_NAME}\"}}\n"),
        ),
        (
            Some("JSONCompactEachRow"),
            format!("[\"{DATABASE_NAME}\"]\n"),
        ),
    ];

    for (format, expected) in cases {
        let response = http_exchange(&database, format, b"SELECT currentDatabase();");
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "{format:?}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(http_body(&response), expected.as_bytes(), "{format:?}");
    }
}

#[test]
fn http_reports_current_database_probe_result_limits_on_the_wire() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        ..QueryResultLimits::default()
    });
    let response = http_exchange(&database, None, b"SELECT currentDatabase()");

    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&response),
        b"{\"error\":\"SELECT result rows requires at least 1, exceeding the limit of 0\"}"
    );
}
