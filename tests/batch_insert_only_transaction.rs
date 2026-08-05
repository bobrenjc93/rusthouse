use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::{SharedDatabase, SharedDatabaseError};

#[test]
fn shared_database_commits_multi_table_insert_batch_in_statement_order() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             CREATE TABLE readings (value Float64);",
        )
        .expect("create target tables");

    assert_eq!(
        database
            .execute_insert_batch(
                "INSERT INTO events VALUES (1, 'one'), (2, 'two'); \
                 INSERT INTO readings VALUES (1.5); \
                 INSERT INTO events VALUES (3, 'three');",
            )
            .expect("all INSERT statements pass preflight"),
        [
            StatementResult::Command {
                tag: "INSERT",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "INSERT",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "INSERT",
                affected_rows: 1,
            },
        ]
    );
    assert_eq!(
        database
            .query("SELECT id FROM events ORDER BY id;")
            .unwrap()
            .rows,
        [
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ]
    );
    assert_eq!(
        database.query("SELECT value FROM readings;").unwrap().rows,
        [vec![Value::Float64(1.5)]]
    );
}

#[test]
fn cumulative_same_table_capacity_is_preflighted_before_any_commit() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE events (id Int64); \
             CREATE TABLE audit (id Int64); \
             INSERT INTO events VALUES (1);",
        )
        .expect("setup");

    assert_eq!(
        database.execute_insert_batch(
            "INSERT INTO events VALUES (2); \
             INSERT INTO audit VALUES (10); \
             INSERT INTO EVENTS VALUES (3), (4);",
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 4,
            max: 3,
        })
    );
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 1);
    assert_eq!(database.catalog().table("audit").unwrap().row_count(), 0);

    database
        .execute_insert_batch(
            "INSERT INTO events VALUES (2); \
             INSERT INTO EVENTS VALUES (3);",
        )
        .expect("the cumulative exact cap fits");
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 3);
}

#[test]
fn late_target_shape_and_type_failures_roll_back_all_tables() {
    for (batch, expected) in [
        (
            "INSERT INTO events VALUES (1); INSERT INTO missing VALUES (2);",
            Error::TableNotFound("missing".to_owned()),
        ),
        (
            "INSERT INTO events VALUES (1); INSERT INTO readings VALUES (2, 3);",
            Error::RowLength {
                table: "readings".to_owned(),
                expected: 1,
                actual: 2,
            },
        ),
        (
            "INSERT INTO events VALUES (1); INSERT INTO readings VALUES ('wrong');",
            Error::TypeMismatch {
                context: "column 'readings.value'".to_owned(),
                expected: "Float64".to_owned(),
                actual: "String".to_owned(),
            },
        ),
    ] {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (id Int64); \
                 CREATE TABLE readings (value Float64);",
            )
            .expect("setup");

        assert_eq!(database.execute_insert_batch(batch), Err(expected));
        assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
        assert_eq!(database.catalog().table("readings").unwrap().row_count(), 0);
    }
}

#[test]
fn parser_failure_and_non_insert_rejection_do_not_apply_earlier_inserts() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64); CREATE TABLE samples (value Float64);")
        .expect("setup");

    assert!(matches!(
        database.execute_insert_batch(
            "INSERT INTO events VALUES (1); INSERT INTO samples VALUES (1e999);"
        ),
        Err(SharedDatabaseError::Sql(Error::Sql { .. }))
    ));
    assert_eq!(
        database.execute_insert_batch("INSERT INTO events VALUES (1); SELECT id FROM events;"),
        Err(SharedDatabaseError::Sql(
            Error::InsertOnlyStatementRequired {
                statement: "SELECT",
            }
        ))
    );
    assert!(matches!(
        database.execute_insert_batch(" ; "),
        Err(SharedDatabaseError::Sql(Error::Sql { .. }))
    ));
    assert!(
        database
            .query("SELECT id FROM events;")
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        database
            .query("SELECT value FROM samples;")
            .unwrap()
            .rows
            .is_empty()
    );
}
