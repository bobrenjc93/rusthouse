use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn int64_column(database: &Database, table: &str, column: &str) -> Vec<i64> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::Int64(values) = &table.columns()[index] else {
        panic!("column is Int64");
    };
    values.clone()
}

#[test]
fn parses_the_exact_int64_alter_update_shape_and_extrema() {
    assert_eq!(
        parse(
            "aLtEr TaBlE Events UpDaTe Value = -9223372036854775808 \
             WhErE Selector = +9223372036854775807;"
        ),
        Ok(vec![Statement::AlterUpdate {
            table: "Events".to_owned(),
            target_column: "Value".to_owned(),
            value: i64::MIN,
            predicate_column: "Selector".to_owned(),
            predicate_value: i64::MAX,
        }])
    );
}

#[test]
fn rejects_non_exact_alter_update_syntax_and_non_int64_literals() {
    for sql in [
        "ALTER events UPDATE value = 1 WHERE selector = 2",
        "ALTER TABLE events value = 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE = 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE value 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1 selector = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector != 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = 2 AND value = 0",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = 2 LIMIT 1",
        "ALTER TABLE events UPDATE value = 1.0 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = true WHERE selector = 2",
        "ALTER TABLE events UPDATE value = '1' WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = 2.0",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = false",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = NULL",
        "ALTER TABLE events UPDATE value = 9223372036854775808 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = -9223372036854775809",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn zero_and_all_matches_are_atomic_and_support_int64_extrema() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (value Int64, selector Int64, label String); \
             INSERT INTO Events VALUES \
                 (0, 9223372036854775807, 'first'), \
                 (1, 9223372036854775807, 'second'), \
                 (2, 9223372036854775807, 'third');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute(
            "ALTER TABLE events UPDATE VALUE = -9223372036854775808 \
             WHERE SELECTOR = +9223372036854775807;"
        ),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 3,
        }])
    );
    assert_eq!(
        int64_column(&database, "events", "value"),
        vec![i64::MIN; 3]
    );

    assert_eq!(
        database.execute(
            "ALTER TABLE EVENTS UPDATE value = +9223372036854775807 \
             WHERE selector = -9223372036854775808;"
        ),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );
    assert_eq!(
        int64_column(&database, "events", "value"),
        vec![i64::MIN; 3]
    );
    assert_eq!(
        database
            .execute("SELECT selector, label FROM events ORDER BY label;")
            .expect("unselected columns remain queryable")[0],
        StatementResult::Query(rusthouse::batch::engine::QueryResult {
            columns: vec![
                rusthouse::batch::engine::ResultColumn {
                    name: "selector".to_owned(),
                    data_type: DataType::Int64,
                },
                rusthouse::batch::engine::ResultColumn {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![
                vec![Value::Int64(i64::MAX), Value::String("first".to_owned())],
                vec![Value::Int64(i64::MAX), Value::String("second".to_owned())],
                vec![Value::Int64(i64::MAX), Value::String("third".to_owned())],
            ],
        })
    );
}

#[test]
fn missing_names_and_non_int64_columns_fail_without_changes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64, score Float64, label String); \
             INSERT INTO events VALUES (1, 10, 1.5, 'one'), (2, 20, 2.5, 'two');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE missing UPDATE value = 0 WHERE id = 1;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE absent = 0 WHERE id = 1;"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE absent = 1;"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 0 WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.score'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "Float64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE label = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.label'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        })
    );

    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20]);
}

#[test]
fn full_table_scan_limit_is_checked_after_names_and_types_and_before_mutation() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64, label String); \
             INSERT INTO events VALUES (1, 10, 'one'), (2, 20, 'two'), (3, 30, 'three');",
        )
        .expect("setup is not subject to scan limits");

    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE id = 3;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20, 30]);

    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE missing = 0 WHERE id = 3;"),
        Err(Error::ColumnNotFound { column, .. }) if column == "missing"
    ));
    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE label = 3;"),
        Err(Error::TypeMismatch { context, .. }) if context.contains("WHERE column")
    ));
    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20, 30]);

    let mut boundary = Database::with_query_result_limits(limits);
    assert_eq!(
        boundary.execute(
            "CREATE TABLE events (id Int64, value Int64); \
             INSERT INTO events VALUES (1, 10), (2, 20); \
             ALTER TABLE events UPDATE value = 99 WHERE id = 2;"
        ),
        Ok(vec![
            StatementResult::Command {
                tag: "CREATE TABLE",
                affected_rows: 0,
            },
            StatementResult::Command {
                tag: "INSERT",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
        ])
    );
    assert_eq!(int64_column(&boundary, "events", "value"), vec![10, 99]);
}

#[test]
fn shared_database_executes_alter_update_under_the_write_api() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64); \
             INSERT INTO events VALUES (1, 10), (2, 20), (2, 30);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE EVENTS UPDATE value = 7 WHERE ID = 2;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        database
            .query("SELECT value FROM events ORDER BY value;")
            .expect("updated values are visible")
            .rows,
        vec![
            vec![Value::Int64(7)],
            vec![Value::Int64(7)],
            vec![Value::Int64(10)],
        ]
    );
    assert_eq!(
        database.query("ALTER TABLE events UPDATE value = 0 WHERE id = 1;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "ALTER TABLE",
        })
    );
    assert_eq!(
        database
            .query("SELECT value FROM events WHERE id = 1;")
            .expect("read-only rejection leaves the row unchanged")
            .rows,
        vec![vec![Value::Int64(10)]]
    );
}
