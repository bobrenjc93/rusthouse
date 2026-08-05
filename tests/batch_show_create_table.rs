use std::mem::size_of;

use rusthouse::SharedDatabase;
use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};

const CREATE: &str =
    "CREATE TABLE Metrics (EventID int64, Score FLOAT64, Active BOOLEAN, Label string);";
const DDL: &str = "CREATE TABLE Metrics (EventID Int64, Score Float64, Active Bool, Label String)";

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn result_bytes(ddl: &str) -> usize {
    size_of::<ResultColumn>()
        + "statement".len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>()
        + ddl.len()
}

#[test]
fn parses_exact_show_create_table_with_an_optional_semicolon() {
    for (sql, name) in [
        ("SHOW CREATE TABLE metrics", "metrics"),
        ("show create table Metrics;", "Metrics"),
    ] {
        assert_eq!(
            parse(sql).expect("valid SHOW CREATE TABLE"),
            [Statement::ShowCreateTable {
                name: name.to_owned(),
            }]
        );
    }

    assert!(matches!(
        parse("SHOW CREATE metrics"),
        Err(Error::Sql { .. })
    ));
    assert_eq!(
        parse("SHOW CREATE TABLE metrics extra"),
        Err(Error::Sql {
            position: 26,
            message: "unexpected trailing input after SHOW CREATE TABLE <name>".to_owned(),
        })
    );
}

#[test]
fn returns_canonical_ddl_in_schema_order_using_display_names() {
    let mut database = Database::new();
    database.execute(CREATE).expect("setup succeeds");

    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE mEtRiCs;"),
        QueryResult {
            columns: vec![ResultColumn {
                name: "statement".to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(DDL.to_owned())]],
        }
    );
    assert_eq!(
        database.execute("SHOW CREATE TABLE absent;"),
        Err(Error::TableNotFound("absent".to_owned()))
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_result_limits() {
    let exact_bytes = result_bytes(DDL);
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(CREATE).expect("setup succeeds");
    assert_eq!(
        query(&mut exact, "SHOW CREATE TABLE metrics").rows,
        [vec![Value::String(DDL.to_owned())]]
    );

    let cases = [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW CREATE TABLE result rows",
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
                resource: "SHOW CREATE TABLE result values",
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
                resource: "SHOW CREATE TABLE result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];

    for (limits, expected) in cases {
        let mut database = Database::with_query_result_limits(limits);
        database.execute(CREATE).expect("setup succeeds");
        assert_eq!(database.execute("SHOW CREATE TABLE metrics"), Err(expected));
    }

    let mut retained = Database::new();
    retained.execute(CREATE).expect("setup succeeds");
    assert!(
        retained
            .execute_with_result_limit("SHOW CREATE TABLE metrics", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        retained.execute_with_result_limit("SHOW CREATE TABLE metrics", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}

#[test]
fn shared_database_reads_show_create_table_under_a_read_lock() {
    let database = SharedDatabase::default();
    database.execute(CREATE).expect("setup succeeds");

    assert_eq!(
        database
            .query("SHOW CREATE TABLE METRICS;")
            .expect("SHOW CREATE TABLE is read-only")
            .rows,
        [vec![Value::String(DDL.to_owned())]]
    );
}
