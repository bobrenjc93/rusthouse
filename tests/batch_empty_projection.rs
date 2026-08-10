use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, handle_http_query};

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
fn parses_case_insensitive_empty_as_a_bounded_select_item() {
    let statements = parse(
        "SELECT empty(label), EmPtY(label) AS blank FROM samples \
         WHERE keep = true ORDER BY EMPTY(label) DESC LIMIT 2 OFFSET 1",
    )
    .expect("valid empty projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Empty {
                name: "label".to_owned(),
                alias: None,
            },
            SelectItem::Empty {
                name: "label".to_owned(),
                alias: Some("blank".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "empty(label)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT empty(label) FROM samples", limits)
        .expect("one empty item fits the AST limit");
    assert_eq!(
        parse_with_limits("SELECT empty(label), label FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn returns_int64_for_empty_ascii_and_unicode_with_filter_order_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, label String, keep Bool); \
             INSERT INTO samples VALUES \
             (0, '', true), (1, 'é', true), (2, '', false), \
             (3, '東京', true), (4, '', true), (5, 'discard', false);",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT id, label, eMpTy(label) AS blank FROM samples \
         WHERE keep = true ORDER BY blank DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "blank".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::Int64(4),
                Value::String(String::new()),
                Value::Int64(1),
            ],
            vec![
                Value::Int64(1),
                Value::String("é".to_owned()),
                Value::Int64(0),
            ],
        ]
    );

    let expression_ordered = query(
        &mut database,
        "SELECT empty(label) FROM samples WHERE keep = true \
         ORDER BY EMPTY(label) LIMIT 2",
    );
    assert_eq!(
        expression_ordered.columns,
        [ResultColumn {
            name: "empty(label)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        expression_ordered.rows,
        [vec![Value::Int64(0)], vec![Value::Int64(0)]]
    );
}

#[test]
fn rejects_missing_non_string_grouped_and_malformed_empty_arguments() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT empty(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );
    for (name, actual) in [
        ("i", DataType::Int64),
        ("f", DataType::Float64),
        ("b", DataType::Bool),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT empty({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("empty argument '{name}'"),
                expected: "String".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }
    assert_eq!(
        database.execute("SELECT empty(s), COUNT(*) FROM samples GROUP BY s"),
        Err(Error::InvalidQuery(
            "empty projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );

    for sql in [
        "SELECT empty() FROM samples",
        "SELECT empty(*) FROM samples",
        "SELECT empty('text') FROM samples",
        "SELECT empty(s, s) FROM samples",
        "SELECT empty(s FROM samples",
        "SELECT empty(s) blank FROM samples",
        "SELECT empty(empty(s)) FROM samples",
        "SELECT empty(s) FROM samples ORDER BY empty()",
        "SELECT empty(s) FROM samples ORDER BY empty(*)",
        "SELECT empty(s) FROM samples ORDER BY empty(s",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn empty_obeys_selected_result_bounds_without_charging_source_bytes() {
    let setup = "CREATE TABLE samples (label String); \
                 INSERT INTO samples VALUES (''), ('é'), ('a very long String payload');";
    let result_name = "blank";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let sql = "SELECT empty(label) AS blank FROM samples LIMIT 1 OFFSET 2";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");
    assert_eq!(query(&mut exact, sql).rows, [vec![Value::Int64(0)]]);

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_byte_short.execute(setup).expect("setup");
    assert_eq!(
        one_byte_short.execute(sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );

    assert_eq!(
        exact.execute("SELECT empty(label) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 1,
        })
    );

    let mut retained = Database::new();
    retained.execute(setup).expect("setup");
    retained
        .execute_with_result_limit(sql, exact_bytes)
        .expect("exact retained-result bound succeeds");
    assert_eq!(
        retained.execute_with_result_limit(sql, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}

#[test]
fn cli_and_http_emit_clickhouse_compatible_empty_results() {
    let cli = run_cli(
        "csv",
        "CREATE TABLE samples (id Int64, label String); \
         INSERT INTO samples VALUES (1, ''), (2, '東京'), (3, 'é'); \
         SELECT id, EMPTY(label) AS blank FROM samples ORDER BY id;"
            .as_bytes(),
    );
    assert!(cli.status.success(), "{:?}", cli.stderr);
    assert_eq!(cli.stdout, b"id,blank\n1,1\n2,0\n3,0\n");
    assert!(cli.stderr.is_empty());

    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (id Int64, label String); \
             INSERT INTO samples VALUES (1, ''), (2, '東京'), (3, 'é');",
        )
        .expect("HTTP setup");
    let sql = b"SELECT id, empty(label) AS blank FROM samples ORDER BY id;";
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: JSONEachRow\r\nContent-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    request.extend_from_slice(sql);
    let mut response = Vec::new();
    handle_http_query(&database, Cursor::new(request), &mut response)
        .expect("HTTP exchange succeeds");
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&response)
    );
    assert_eq!(
        http_body(&response),
        b"{\"id\":1,\"blank\":1}\n{\"id\":2,\"blank\":0}\n{\"id\":3,\"blank\":0}\n"
    );
}
