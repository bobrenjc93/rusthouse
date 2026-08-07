use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{DatabaseMetrics, SharedDatabase, SharedDatabaseError};

fn ids(database: &Database, table: &str) -> Vec<i64> {
    let table = database.catalog().table(table).expect("table exists");
    let Column::Int64(values) = &table.columns()[0] else {
        panic!("the first column is Int64");
    };
    values.clone()
}

#[test]
fn parses_exact_equality_delete_for_every_literal_type() {
    let cases = [
        (
            "DELETE FROM events WHERE id = -7",
            "events",
            "id",
            Value::Int64(-7),
        ),
        (
            "delete from Events where score = +2.5e1;",
            "Events",
            "score",
            Value::Float64(25.0),
        ),
        (
            "DELETE FROM events WHERE active = TRUE;",
            "events",
            "active",
            Value::Bool(true),
        ),
        (
            "DELETE FROM events WHERE label = 'it''s here';",
            "events",
            "label",
            Value::String("it's here".to_owned()),
        ),
    ];

    for (sql, table, column, literal) in cases {
        assert_eq!(
            parse(sql).expect("valid equality DELETE"),
            [Statement::Delete {
                table: table.to_owned(),
                column: column.to_owned(),
                literal,
            }]
        );
    }
}

#[test]
fn rejects_every_non_exact_delete_shape() {
    for sql in [
        "DELETE events WHERE id = 1",
        "DELETE FROM events id = 1",
        "DELETE FROM events WHERE id",
        "DELETE FROM events WHERE id != 1",
        "DELETE FROM events WHERE id = other_id",
        "DELETE FROM events WHERE id = NULL",
        "DELETE FROM events WHERE id = 1 AND active = true",
        "DELETE FROM events WHERE id = 1 ORDER BY id",
        "DELETE FROM events WHERE id = 1 LIMIT 1",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
}

#[test]
fn executes_equality_delete_across_every_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (id Int64, score Float64, active Bool, label String); \
             INSERT INTO Events VALUES \
                 (1, 1.5, true, 'one'), \
                 (2, 2.5, false, 'two'), \
                 (3, 3.5, true, 'three'), \
                 (4, 4.5, false, 'four'), \
                 (5, 5.5, true, 'five');",
        )
        .expect("setup succeeds");

    for (sql, affected_rows, remaining_ids) in [
        ("DELETE FROM events WHERE id = 2", 1, vec![1, 3, 4, 5]),
        ("DELETE FROM EVENTS WHERE score = 3.5", 1, vec![1, 4, 5]),
        ("DELETE FROM Events WHERE active = false", 1, vec![1, 5]),
        ("DELETE FROM events WHERE label = 'five'", 1, vec![1]),
    ] {
        assert_eq!(
            database.execute(sql),
            Ok(vec![StatementResult::Command {
                tag: "DELETE",
                affected_rows,
            }])
        );
        assert_eq!(ids(&database, "events"), remaining_ids);
    }

    let table = database.catalog().table("events").expect("table remains");
    assert!(matches!(&table.columns()[1], Column::Float64(values) if values == &[1.5]));
    assert!(matches!(&table.columns()[2], Column::Bool(values) if values == &[true]));
    assert!(matches!(&table.columns()[3], Column::String(values) if values == &["one"]));
}

#[test]
fn zero_and_all_matches_report_counts_and_preserve_the_table() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool); \
             INSERT INTO events VALUES (1, true), (2, true), (3, true);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM events WHERE id = 99;"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 0,
        }])
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);

    assert_eq!(
        database.execute("DELETE FROM events WHERE active = true;"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 3,
        }])
    );
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.row_count(), 0);
    assert!(table.columns().iter().all(Column::is_empty));
    assert_eq!(table.schema()[0].data_type, DataType::Int64);
    assert_eq!(table.schema()[1].data_type, DataType::Bool);
}

#[test]
fn validation_and_scan_limit_errors_never_delete_rows() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE Events (id Int64, label String); \
             INSERT INTO Events VALUES (1, 'one'), (2, 'two'), (3, 'three');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM missing WHERE id = 1"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("DELETE FROM events WHERE absent = 1"),
        Err(Error::ColumnNotFound {
            table: "Events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("DELETE FROM events WHERE label = true"),
        Err(Error::TypeMismatch {
            context: "WHERE comparison".to_owned(),
            expected: "String".to_owned(),
            actual: "Bool".to_owned(),
        })
    );
    assert_eq!(
        database.execute("DELETE FROM events WHERE id = 2"),
        Err(Error::ResourceLimitExceeded {
            resource: "DELETE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);

    assert_eq!(
        database.execute_statement(Statement::Delete {
            table: "events".to_owned(),
            column: "id".to_owned(),
            literal: Value::Float64(f64::NAN),
        }),
        Err(Error::InvalidQuery(
            "WHERE comparison Float64 literals must be finite".to_owned()
        ))
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);
}

#[test]
fn insert_only_execution_rejects_delete_without_mutation() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2);")
        .expect("setup succeeds");

    assert_eq!(
        database.execute_insert_batch("DELETE FROM events WHERE id = 1"),
        Err(Error::InsertOnlyStatementRequired {
            statement: "DELETE",
        })
    );
    assert_eq!(ids(&database, "events"), [1, 2]);
}

#[test]
fn shared_database_executes_delete_under_its_write_lock() {
    let database = SharedDatabase::default();
    let deleting_handle = database.clone();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'keep'), (2, 'remove'), (3, 'remove');",
        )
        .expect("setup succeeds");

    assert_eq!(
        deleting_handle.execute("DELETE FROM EVENTS WHERE label = 'remove';"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 2,
            retained_row_count: 1,
        })
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM events;")
            .unwrap()
            .rows,
        [vec![Value::Int64(1), Value::String("keep".to_owned())]]
    );
    assert_eq!(
        database.query("DELETE FROM events WHERE id = 1"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "DELETE",
        })
    );
}
