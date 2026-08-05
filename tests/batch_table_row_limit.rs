use rusthouse::batch::engine::{
    DEFAULT_MAX_ROWS_PER_TABLE, Database, QueryResult, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().next().expect("one statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected a query result"),
    }
}

#[test]
fn database_preflights_repeated_inserts_and_reuses_capacity_after_truncate() {
    let default = Database::new();
    let default_row_cap = default.max_rows_per_table();
    assert_eq!(default_row_cap, DEFAULT_MAX_ROWS_PER_TABLE);
    assert_ne!(default_row_cap, usize::MAX);

    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'one');",
        )
        .expect("setup fits the cap");
    database
        .execute("INSERT INTO events VALUES (2, 'two'), (3, 'three');")
        .expect("the exact limit fits across repeated inserts");

    assert_eq!(
        database.execute("INSERT INTO events VALUES (4, 'four'), (5, 'five');"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 5,
            max: 3,
        })
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM events ORDER BY id;").rows,
        [
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ]
    );
    assert_eq!(database.catalog().table("events").unwrap().row_cap(), 3);

    assert_eq!(
        database.execute("TRUNCATE TABLE events;").unwrap(),
        [StatementResult::Command {
            tag: "TRUNCATE TABLE",
            affected_rows: 3,
        }]
    );
    database
        .execute("INSERT INTO events VALUES (6, 'six'), (7, 'seven'), (8, 'eight');")
        .expect("truncate restores all row capacity");
    assert_eq!(
        query(&mut database, "SELECT id FROM events ORDER BY id;").rows,
        [
            vec![Value::Int64(6)],
            vec![Value::Int64(7)],
            vec![Value::Int64(8)],
        ]
    );
}

#[test]
fn shared_database_preserves_rejected_insert_and_reuses_truncated_capacity() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    assert_eq!(database.max_rows_per_table(), Ok(2));
    database
        .execute(
            "CREATE TABLE readings (value Int64); \
             INSERT INTO readings VALUES (1);",
        )
        .expect("setup fits the cap");

    assert_eq!(
        database.execute("INSERT INTO readings VALUES (2), (3);"),
        Err(SharedDatabaseError::Sql(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        }))
    );
    assert_eq!(
        database
            .query("SELECT value FROM readings ORDER BY value;")
            .unwrap()
            .rows,
        [vec![Value::Int64(1)]]
    );

    database
        .execute(
            "TRUNCATE TABLE readings; \
             INSERT INTO readings VALUES (4); \
             INSERT INTO readings VALUES (5);",
        )
        .expect("repeated inserts reuse the truncated capacity exactly");
    assert_eq!(
        database
            .query("SELECT value FROM readings ORDER BY value;")
            .unwrap()
            .rows,
        [vec![Value::Int64(4)], vec![Value::Int64(5)]]
    );
}
