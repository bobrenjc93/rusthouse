use std::sync::{Arc, Barrier};
use std::thread;

use rusthouse::{Database, Error, QueryResult, SharedDatabase, StatementResult, Value};

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("query result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn cloned_sessions_share_mutations_across_threads() {
    let database = SharedDatabase::new();
    database
        .execute("CREATE TABLE events (worker Int64, value Int64)")
        .expect("create table");

    let workers = 4;
    let start = Arc::new(Barrier::new(workers + 1));
    let threads = (0..workers)
        .map(|worker| {
            let database = database.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                database.execute(&format!(
                    "INSERT INTO events VALUES ({worker}, {});",
                    worker * 10
                ))
            })
        })
        .collect::<Vec<_>>();

    start.wait();
    for worker in threads {
        worker.join().expect("writer thread").expect("insert");
    }

    let result = last_query(
        database
            .execute("SELECT worker, value FROM events ORDER BY worker")
            .expect("select all rows"),
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(0), Value::Int64(0)],
            vec![Value::Int64(1), Value::Int64(10)],
            vec![Value::Int64(2), Value::Int64(20)],
            vec![Value::Int64(3), Value::Int64(30)],
        ]
    );
}

#[test]
fn concurrent_readers_observe_the_completed_mixed_batch() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (category String, value Int64); \
             INSERT INTO samples VALUES ('a', 2), ('b', 3);",
        )
        .expect("setup");
    let database = database.into_shared();

    let writer = database.clone();
    let written = thread::spawn(move || {
        writer.execute(
            "INSERT INTO samples VALUES ('a', 5); \
             SELECT SUM(value) AS total FROM samples;",
        )
    })
    .join()
    .expect("writer thread")
    .expect("mixed batch");
    assert_eq!(last_query(written).rows, vec![vec![Value::Int64(10)]]);

    let readers = 4;
    let start = Arc::new(Barrier::new(readers + 1));
    let threads = (0..readers)
        .map(|_| {
            let database = database.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                database.execute(
                    "SELECT category, SUM(value) AS total \
                     FROM samples GROUP BY category ORDER BY category;",
                )
            })
        })
        .collect::<Vec<_>>();

    start.wait();
    for reader in threads {
        let result = last_query(reader.join().expect("reader thread").expect("select"));
        assert_eq!(
            result.rows,
            vec![
                vec![Value::String("a".to_owned()), Value::Int64(7)],
                vec![Value::String("b".to_owned()), Value::Int64(3)],
            ]
        );
    }
}

#[test]
fn shared_batches_preserve_parse_and_execution_failure_semantics() {
    let database = SharedDatabase::new();

    database
        .execute(
            "CREATE TABLE not_applied (id Int64); \
             SELECT id FORM not_applied;",
        )
        .expect_err("parse failure");
    assert!(matches!(
        database.execute("SELECT * FROM not_applied"),
        Err(Error::TableNotFound(table)) if table == "not_applied"
    ));

    database
        .execute(
            "CREATE TABLE applied (id Int64); \
             INSERT INTO applied VALUES (false);",
        )
        .expect_err("execution failure");
    let applied = last_query(
        database
            .execute("SELECT * FROM applied")
            .expect("earlier CREATE remains visible"),
    );
    assert!(applied.rows.is_empty());
}
