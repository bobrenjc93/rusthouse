use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{Database, QueryResultLimits, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{self, BatchSqlLimits, LiteralSelect, Statement};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, handle_http_query};

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

fn run_http(format: Option<&str>, sql: &[u8]) -> Vec<u8> {
    let database = SharedDatabase::default();
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
    handle_http_query(&database, Cursor::new(request), &mut response).unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let separator = b"\r\n\r\n";
    let body = response
        .windows(separator.len())
        .position(|window| window == separator)
        .expect("HTTP response has a header terminator")
        + separator.len();
    response[body..].to_vec()
}

#[test]
fn parses_each_literal_type_signed_numbers_escaped_strings_and_aliases() {
    let statements = sql::parse(
        "SELECT -9223372036854775808 AS minimum; \
         SELECT +6.25e1 AS rate; \
         SELECT TrUe; \
         SELECT 'it''s; ready' AS message;",
    )
    .unwrap();

    let expected = [
        (Value::Int64(i64::MIN), Some("minimum")),
        (Value::Float64(62.5), Some("rate")),
        (Value::Bool(true), None),
        (Value::String("it's; ready".to_owned()), Some("message")),
    ];
    for (statement, (expected_value, expected_alias)) in statements.iter().zip(expected) {
        let Statement::LiteralSelect(select) = statement else {
            panic!("expected a literal-only SELECT");
        };
        assert_eq!(select.value, expected_value);
        assert_eq!(select.alias.as_deref(), expected_alias);
    }
}

#[test]
fn parses_every_explicitly_typed_null_and_optional_aliases() {
    let statements = sql::parse(
        "SELECT CAST(NULL AS Int64); \
         SELECT cast(null as float64) AS floating; \
         SELECT CaSt(NuLl As BoOl); \
         SELECT CAST(NULL AS String) AS text;",
    )
    .unwrap();

    let expected = [
        (DataType::Int64, None),
        (DataType::Float64, Some("floating")),
        (DataType::Bool, None),
        (DataType::String, Some("text")),
    ];
    for (statement, (data_type, alias)) in statements.iter().zip(expected) {
        let Statement::LiteralSelect(select) = statement else {
            panic!("expected a literal-only SELECT");
        };
        assert_eq!(select.value, Value::Null(data_type));
        assert_eq!(select.alias.as_deref(), alias);
    }
}

#[test]
fn rejects_expression_lists_unsupported_literals_and_trailing_clauses() {
    for sql in [
        "SELECT 1, 2",
        "SELECT 1 + 2",
        "SELECT 1 FROM values_table",
        "SELECT true WHERE true",
        "SELECT 'x' ORDER BY x",
        "SELECT 1 LIMIT 1",
        "SELECT 1 UNION ALL SELECT 1",
        "SELECT 1 AS value extra",
        "SELECT 1 AS",
        "SELECT NULL",
        "SELECT CAST(NULL)",
        "SELECT CAST(NULL Int64)",
        "SELECT CAST(NULL AS)",
        "SELECT CAST(NULL AS Int64",
        "SELECT CAST(NULL AS Int64))",
        "SELECT CAST(NULL AS UInt64)",
        "SELECT CAST(NULL AS Boolean)",
        "SELECT CAST(NULL AS Int64), 1",
        "SELECT CAST(NULL AS Int64) FROM values_table",
        "SELECT CAST(NULL AS Int64) WHERE true",
        "SELECT CAST(NULL AS Int64) LIMIT 1",
        "SELECT CAST(NULL AS Int64) UNION ALL SELECT 1",
        "SELECT CAST(NULL AS Bool) AS",
        "SELECT CAST(NULL AS String) AS value extra",
        "SELECT -true",
    ] {
        assert!(
            rusthouse::batch::sql::parse(sql).is_err(),
            "malformed literal SELECT was accepted: {sql}"
        );
    }
}

#[test]
fn literal_select_counts_toward_the_ast_item_limit() {
    let limits = BatchSqlLimits {
        max_ast_list_items: 0,
        ..BatchSqlLimits::default()
    };
    assert_eq!(
        sql::parse_with_limits("SELECT 1", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );
    assert_eq!(
        sql::parse_with_limits("SELECT CAST(NULL AS Int64)", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );
}

#[test]
fn database_returns_one_inferred_column_and_row_for_each_literal_type() {
    let mut database = Database::new();
    let results = database
        .execute(
            "SELECT -7 AS signed; \
             SELECT +2.5; \
             SELECT false; \
             SELECT 'it''s';",
        )
        .unwrap();

    let expected = [
        ("signed", DataType::Int64, Value::Int64(-7)),
        ("2.5", DataType::Float64, Value::Float64(2.5)),
        ("false", DataType::Bool, Value::Bool(false)),
        (
            "'it''s'",
            DataType::String,
            Value::String("it's".to_owned()),
        ),
    ];
    for (result, (name, data_type, value)) in results.iter().zip(expected) {
        let result = query_result(result);
        assert_eq!(
            result.columns,
            vec![ResultColumn {
                name: name.to_owned(),
                data_type,
            }]
        );
        assert_eq!(result.rows, vec![vec![value]]);
    }
}

#[test]
fn database_returns_one_typed_null_column_and_row_for_every_type() {
    let mut database = Database::new();
    let results = database
        .execute(
            "SELECT CAST(NULL AS Int64); \
             SELECT CAST(NULL AS Float64) AS floating; \
             SELECT CAST(NULL AS Bool); \
             SELECT CAST(NULL AS String) AS text;",
        )
        .unwrap();

    let expected = [
        ("CAST(NULL AS Int64)", DataType::Int64),
        ("floating", DataType::Float64),
        ("CAST(NULL AS Bool)", DataType::Bool),
        ("text", DataType::String),
    ];
    for (result, (name, data_type)) in results.iter().zip(expected) {
        let result = query_result(result);
        assert_eq!(
            result.columns,
            vec![ResultColumn {
                name: name.to_owned(),
                data_type,
            }]
        );
        assert_eq!(result.rows, vec![vec![Value::Null(data_type)]]);
    }
}

#[test]
fn direct_ast_execution_rejects_non_finite_literal_values() {
    let mut database = Database::new();
    let cases = [
        (
            Value::Float64(f64::NAN),
            None,
            "literal SELECT Float64 must be finite",
        ),
        (
            Value::Float64(f64::INFINITY),
            Some("positive_infinity"),
            "literal SELECT Float64 must be finite",
        ),
        (
            Value::Float64(f64::NEG_INFINITY),
            None,
            "literal SELECT Float64 must be finite",
        ),
    ];

    for (value, alias, message) in cases {
        assert_eq!(
            database.execute_statement(Statement::LiteralSelect(LiteralSelect {
                value,
                alias: alias.map(str::to_owned),
            })),
            Err(Error::InvalidQuery(message.to_owned()))
        );
    }
}

#[test]
fn direct_ast_unaliased_string_is_preflighted_at_exact_result_limits() {
    let value = "'escaped payload'".repeat(1_024);
    let derived_name_bytes = value.len() + value.bytes().filter(|byte| *byte == b'\'').count() + 2;
    let required_bytes = size_of::<ResultColumn>()
        + derived_name_bytes
        + size_of::<Vec<Value>>()
        + size_of::<Value>()
        + value.len();
    let statement = || {
        Statement::LiteralSelect(LiteralSelect {
            value: Value::String(value.clone()),
            alias: None,
        })
    };

    let mut row_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        ..QueryResultLimits::default()
    });
    assert_eq!(
        row_limited.execute_statement(statement()),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 1,
            max: 0,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: required_bytes - 1,
        ..QueryResultLimits::default()
    });
    assert_eq!(
        byte_limited.execute_statement(statement()),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: required_bytes,
            max: required_bytes - 1,
        })
    );

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: required_bytes,
        ..QueryResultLimits::default()
    });
    let StatementResult::Query(result) = exact.execute_statement(statement()).unwrap() else {
        panic!("literal SELECT must return a query result");
    };
    assert_eq!(result.columns[0].name.len(), derived_name_bytes);
    assert_eq!(result.rows, vec![vec![Value::String(value)]]);
}

#[test]
fn direct_ast_generated_scalar_names_have_exact_byte_accounting() {
    for value in [
        Value::Null(DataType::Int64),
        Value::Null(DataType::Float64),
        Value::Null(DataType::Bool),
        Value::Null(DataType::String),
        Value::Int64(i64::MIN),
        Value::Float64(2.0),
        Value::Float64(f64::MAX),
        Value::Float64(f64::MIN_POSITIVE),
        Value::Bool(true),
    ] {
        let expected_name = match &value {
            Value::Null(data_type) => format!("CAST(NULL AS {data_type})"),
            _ => value.as_display_string(),
        };
        let required_bytes = size_of::<ResultColumn>()
            + expected_name.len()
            + size_of::<Vec<Value>>()
            + size_of::<Value>();
        let statement = || {
            Statement::LiteralSelect(LiteralSelect {
                value: value.clone(),
                alias: None,
            })
        };

        let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
            max_bytes: required_bytes - 1,
            ..QueryResultLimits::default()
        });
        assert_eq!(
            byte_limited.execute_statement(statement()),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: required_bytes,
                max: required_bytes - 1,
            })
        );

        let mut exact = Database::with_query_result_limits(QueryResultLimits {
            max_bytes: required_bytes,
            ..QueryResultLimits::default()
        });
        let StatementResult::Query(result) = exact.execute_statement(statement()).unwrap() else {
            panic!("literal SELECT must return a query result");
        };
        assert_eq!(result.columns[0].name, expected_name);
        assert_eq!(result.rows, vec![vec![value]]);
    }
}

#[test]
fn shared_database_executes_literal_select_under_a_read_lock() {
    let database = SharedDatabase::default();
    let result = database.query("SELECT 'ready' AS status;").unwrap();

    assert_eq!(
        result.columns,
        vec![ResultColumn {
            name: "status".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(result.rows, vec![vec![Value::String("ready".to_owned())]]);

    let result = database
        .query("SELECT CAST(NULL AS String) AS missing;")
        .unwrap();
    assert_eq!(
        result.columns,
        vec![ResultColumn {
            name: "missing".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(result.rows, vec![vec![Value::Null(DataType::String)]]);
}

#[test]
fn typed_null_select_obeys_exact_query_and_retained_result_limits() {
    let column_name = "CAST(NULL AS String)";
    let exact_bytes = size_of::<ResultColumn>()
        + column_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let sql = "SELECT CAST(NULL AS String)";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    let result = exact.execute(sql).expect("all exact query limits fit");
    assert_eq!(query_result(&result[0]).columns[0].name, column_name);

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
            database.execute(sql),
            Err(Error::ResourceLimitExceeded {
                resource,
                actual,
                max,
            })
        );
    }

    let mut retained = Database::new();
    assert!(retained.execute_with_result_limit(sql, exact_bytes).is_ok());
    assert_eq!(
        retained.execute_with_result_limit(sql, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::default();
    assert!(shared.query_with_result_limit(sql, exact_bytes).is_ok());
    assert_eq!(
        shared.query_with_result_limit(sql, exact_bytes - 1),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        }))
    );
}

#[test]
fn literal_select_obeys_query_shape_payload_and_retained_result_limits() {
    for (limits, expected_resource) in [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            "SELECT result rows",
        ),
        (
            QueryResultLimits {
                max_values: 0,
                ..QueryResultLimits::default()
            },
            "SELECT result values",
        ),
        (
            QueryResultLimits {
                max_bytes: 0,
                ..QueryResultLimits::default()
            },
            "SELECT result bytes",
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        assert!(matches!(
            database.execute("SELECT 1"),
            Err(Error::ResourceLimitExceeded { resource, .. }) if resource == expected_resource
        ));
    }

    let fixed_bytes =
        size_of::<ResultColumn>() + "value".len() + size_of::<Vec<Value>>() + size_of::<Value>();
    let mut payload_limited = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: fixed_bytes,
        ..QueryResultLimits::default()
    });
    assert_eq!(
        payload_limited.execute("SELECT 'x' AS value"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: fixed_bytes + 1,
            max: fixed_bytes,
        })
    );

    let mut database = Database::new();
    assert!(matches!(
        database.execute_with_result_limit("SELECT 1", 0),
        Err(Error::ResultLimitExceeded { max_bytes: 0, .. })
    ));
    let shared = SharedDatabase::default();
    assert!(matches!(
        shared.query_with_result_limit("SELECT true", 0),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            max_bytes: 0,
            ..
        }))
    ));
}

#[test]
fn cli_emits_typed_null_select_in_every_output_format() {
    let sql = b"SELECT CAST(NULL AS Int64) AS missing;";
    let cases: [(&str, &[u8]); 6] = [
        (
            "table",
            b"+---------+\n| missing |\n+---------+\n| NULL    |\n+---------+\n",
        ),
        ("csv", b"missing\nNULL\n"),
        ("tsv", b"missing\n\\N\n"),
        (
            "json",
            b"{\"columns\":[{\"name\":\"missing\",\"type\":\"Int64\"}],\"rows\":[[null]]}\n",
        ),
        ("JSONEachRow", b"{\"missing\":null}\n"),
        ("JSONCompactEachRow", b"[null]\n"),
    ];

    for (format, expected) in cases {
        let output = run_cli(format, sql);
        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected, "{format}");
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn http_exposes_typed_null_select_in_every_supported_wire_format() {
    let sql = b"SELECT CAST(NULL AS String) AS missing;";
    let cases: [(Option<&str>, &[u8]); 5] = [
        (
            None,
            b"{\"columns\":[{\"name\":\"missing\",\"type\":\"String\"}],\"rows\":[[null]]}",
        ),
        (Some("CSVWithNames"), b"missing\nNULL\n"),
        (Some("TabSeparatedWithNames"), b"missing\n\\N\n"),
        (Some("JSONEachRow"), b"{\"missing\":null}\n"),
        (Some("JSONCompactEachRow"), b"[null]\n"),
    ];

    for (format, expected) in cases {
        assert_eq!(run_http(format, sql), expected, "{format:?}");
    }
}
