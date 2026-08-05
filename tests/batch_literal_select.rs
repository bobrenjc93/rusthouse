use std::io::Write;
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{Database, QueryResultLimits, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{self, BatchSqlLimits, Statement};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

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
fn cli_emits_literal_selects_in_table_csv_and_json_formats() {
    let table = run_cli("table", b"SELECT -7 AS signed;");
    assert!(table.status.success(), "{:?}", table.stderr);
    assert_eq!(
        table.stdout,
        b"+--------+\n| signed |\n+--------+\n| -7     |\n+--------+\n"
    );
    assert!(table.stderr.is_empty());

    let csv = run_cli("csv", b"SELECT 'it''s, ready' AS message;");
    assert!(csv.status.success(), "{:?}", csv.stderr);
    assert_eq!(csv.stdout, b"message\n\"it's, ready\"\n");
    assert!(csv.stderr.is_empty());

    let json = run_cli(
        "json",
        b"SELECT -7 AS integer; SELECT +2.5 AS float; \
          SELECT false AS boolean; SELECT 'ready' AS string;",
    );
    assert!(json.status.success(), "{:?}", json.stderr);
    assert_eq!(
        json.stdout,
        concat!(
            "{\"columns\":[{\"name\":\"integer\",\"type\":\"Int64\"}],\"rows\":[[-7]]}\n",
            "{\"columns\":[{\"name\":\"float\",\"type\":\"Float64\"}],\"rows\":[[2.5]]}\n",
            "{\"columns\":[{\"name\":\"boolean\",\"type\":\"Bool\"}],\"rows\":[[false]]}\n",
            "{\"columns\":[{\"name\":\"string\",\"type\":\"String\"}],\"rows\":[[\"ready\"]]}\n",
        )
        .as_bytes()
    );
    assert!(json.stderr.is_empty());
}
