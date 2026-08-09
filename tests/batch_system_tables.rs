use std::io::{Cursor, Write};
use std::mem::size_of;
use std::process::{Command, Output, Stdio};

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, handle_http_query};

const SYSTEM_TABLES_QUERY: &str = "SELECT database, name, engine, total_rows FROM system.tables";

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

fn columns() -> Vec<ResultColumn> {
    vec![
        ResultColumn {
            name: "database".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "engine".to_owned(),
            data_type: DataType::String,
        },
        ResultColumn {
            name: "total_rows".to_owned(),
            data_type: DataType::Int64,
        },
    ]
}

fn result_bytes(tables: &[(&str, i64)]) -> usize {
    columns().len() * size_of::<ResultColumn>()
        + "database".len()
        + "name".len()
        + "engine".len()
        + "total_rows".len()
        + tables.len() * size_of::<Vec<Value>>()
        + tables.len() * columns().len() * size_of::<Value>()
        + tables
            .iter()
            .map(|(name, _)| "default".len() + name.len() + "Memory".len())
            .sum::<usize>()
}

fn run_cli(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "json"])
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
fn parses_only_the_exact_case_insensitive_system_tables_shape() {
    for sql in [
        SYSTEM_TABLES_QUERY,
        "select DATABASE, NAME, ENGINE, TOTAL_ROWS from SYSTEM.TABLES;",
        "SeLeCt database,name,engine,total_rows FrOm system . tables",
    ] {
        assert_eq!(
            parse(sql).expect("valid system.tables query"),
            [Statement::SystemTables]
        );
    }

    for malformed in [
        "SELECT name, database, engine, total_rows FROM system.tables",
        "SELECT database, name, engine FROM system.tables",
        "SELECT database, name, engine, total_rows AS rows FROM system.tables",
        "SELECT database, name, engine, total_rows FROM system.tables WHERE database = 'default'",
        "SELECT database, name, engine, total_rows FROM system.tables ORDER BY name",
        "SELECT database, name, engine, total_rows FROM system.tables LIMIT 1",
        "SELECT * FROM system.tables",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "non-exact system.tables query was accepted: {malformed}"
        );
    }

    assert_eq!(
        parse_with_limits(
            SYSTEM_TABLES_QUERY,
            BatchSqlLimits {
                max_ast_list_items: 3,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 4,
            max: 3,
        })
    );
}

#[test]
fn empty_catalog_returns_the_typed_empty_metadata_shape() {
    let exact_bytes = result_bytes(&[]);
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        max_values: 0,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });

    assert_eq!(
        query(&mut database, SYSTEM_TABLES_QUERY),
        QueryResult {
            columns: columns(),
            rows: Vec::new(),
        }
    );
}

#[test]
fn metadata_tracks_create_insert_truncate_and_drop_in_deterministic_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE zebra (id Int64); CREATE TABLE Alpha (id Int64);")
        .expect("create tables");
    assert_eq!(
        query(&mut database, SYSTEM_TABLES_QUERY).rows,
        [
            vec![
                Value::String("default".to_owned()),
                Value::String("Alpha".to_owned()),
                Value::String("Memory".to_owned()),
                Value::Int64(0),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("zebra".to_owned()),
                Value::String("Memory".to_owned()),
                Value::Int64(0),
            ],
        ]
    );

    database
        .execute("INSERT INTO ALPHA VALUES (1), (2); INSERT INTO Zebra VALUES (7);")
        .expect("insert rows");
    assert_eq!(
        query(&mut database, SYSTEM_TABLES_QUERY).rows,
        [
            vec![
                Value::String("default".to_owned()),
                Value::String("Alpha".to_owned()),
                Value::String("Memory".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("zebra".to_owned()),
                Value::String("Memory".to_owned()),
                Value::Int64(1),
            ],
        ]
    );

    database
        .execute("TRUNCATE TABLE alpha; DROP TABLE ZEBRA;")
        .expect("truncate and drop");
    assert_eq!(
        query(&mut database, SYSTEM_TABLES_QUERY).rows,
        [vec![
            Value::String("default".to_owned()),
            Value::String("Alpha".to_owned()),
            Value::String("Memory".to_owned()),
            Value::Int64(0),
        ]]
    );
}

#[test]
fn system_tables_accepts_exact_and_rejects_exceeded_query_result_limits() {
    let tables = [("Alpha", 1), ("beta", 2)];
    let exact_bytes = result_bytes(&tables);
    let setup = "CREATE TABLE beta (id Int64); CREATE TABLE Alpha (id Int64); \
                 INSERT INTO alpha VALUES (1); INSERT INTO beta VALUES (2), (3);";
    let exact_limits = QueryResultLimits {
        max_scan_rows: 0,
        max_rows: tables.len(),
        max_values: tables.len() * columns().len(),
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup exact-limit database");
    assert_eq!(
        query(&mut exact, SYSTEM_TABLES_QUERY).rows[0][3],
        Value::Int64(1)
    );

    let cases = [
        (
            QueryResultLimits {
                max_rows: tables.len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: tables.len(),
                max: tables.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: tables.len() * columns().len() - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: tables.len() * columns().len(),
                max: tables.len() * columns().len() - 1,
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

    for (limits, expected) in cases {
        let mut database = Database::with_query_result_limits(limits);
        database.execute(setup).expect("setup limited database");
        assert_eq!(database.execute(SYSTEM_TABLES_QUERY), Err(expected));
    }
}

#[test]
fn database_and_shared_retained_result_limits_are_exact() {
    let tables = [("Events", 2)];
    let exact_bytes = result_bytes(&tables);
    let setup = "CREATE TABLE Events (id Int64); INSERT INTO events VALUES (1), (2);";

    let mut database = Database::new();
    database.execute(setup).expect("database setup");
    assert!(
        database
            .execute_with_result_limit(SYSTEM_TABLES_QUERY, exact_bytes)
            .is_ok()
    );
    assert_eq!(
        database.execute_with_result_limit(SYSTEM_TABLES_QUERY, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let shared = SharedDatabase::default();
    shared.execute(setup).expect("shared setup");
    assert!(
        shared
            .query_with_result_limit(SYSTEM_TABLES_QUERY, exact_bytes)
            .is_ok()
    );
    assert!(matches!(
        shared.query_with_result_limit(SYSTEM_TABLES_QUERY, exact_bytes - 1),
        Err(rusthouse::SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            bytes,
            max_bytes,
        })) if bytes == exact_bytes && max_bytes == exact_bytes - 1
    ));
}

#[test]
fn cli_and_http_expose_system_tables_through_normal_query_formats() {
    let sql = b"CREATE TABLE Events (id Int64); \
                INSERT INTO events VALUES (1), (2); \
                SELECT database, name, engine, total_rows FROM system.tables;";
    let expected = b"{\"columns\":[{\"name\":\"database\",\"type\":\"String\"},{\"name\":\"name\",\"type\":\"String\"},{\"name\":\"engine\",\"type\":\"String\"},{\"name\":\"total_rows\",\"type\":\"Int64\"}],\"rows\":[[\"default\",\"Events\",\"Memory\",2]]}";

    let output = run_cli(sql);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, [expected.as_slice(), b"\n"].concat());
    assert!(output.stderr.is_empty());

    let shared = SharedDatabase::default();
    shared
        .execute("CREATE TABLE Events (id Int64); INSERT INTO events VALUES (1), (2);")
        .expect("HTTP database setup");
    assert_eq!(
        shared
            .query(SYSTEM_TABLES_QUERY)
            .expect("shared read-only query")
            .rows[0][3],
        Value::Int64(2)
    );

    let request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        SYSTEM_TABLES_QUERY.len(),
        SYSTEM_TABLES_QUERY
    );
    let mut response = Vec::new();
    handle_http_query(&shared, Cursor::new(request), &mut response).expect("HTTP exchange");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(http_body(&response), expected);

    let limited = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        ..QueryResultLimits::default()
    });
    limited
        .execute("CREATE TABLE Events (id Int64)")
        .expect("limited HTTP database setup");
    let mut limited_response = Vec::new();
    handle_http_query(
        &limited,
        Cursor::new(format!(
            "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
            SYSTEM_TABLES_QUERY.len(),
            SYSTEM_TABLES_QUERY
        )),
        &mut limited_response,
    )
    .expect("limited HTTP exchange");
    assert!(limited_response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        http_body(&limited_response),
        b"{\"error\":\"SELECT result rows requires at least 1, exceeding the limit of 0\"}"
    );
}
