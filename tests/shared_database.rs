use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn query_result(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("one statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected a query result"),
    }
}

fn scalar_counts(results: &[StatementResult]) -> Vec<i64> {
    results
        .iter()
        .filter_map(|result| match result {
            StatementResult::Query(result) => Some(match result.rows.as_slice() {
                [row] => match row.as_slice() {
                    [Value::Int64(value)] => *value,
                    _ => panic!("expected one Int64 value"),
                },
                _ => panic!("expected one result row"),
            }),
            StatementResult::Command { .. } => None,
        })
        .collect()
}

#[test]
fn concurrent_typed_inserts_and_reads_return_owned_snapshots() {
    const ROWS: i64 = 6;

    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (id Int64, ratio Float64, active Bool, label String);")
        .unwrap();
    let row_published = Arc::new(Barrier::new(2));
    let read_finished = Arc::new(Barrier::new(2));

    let writer_database = database.clone();
    let writer_published = Arc::clone(&row_published);
    let writer_finished = Arc::clone(&read_finished);
    let writer = thread::spawn(move || {
        for id in 0..ROWS {
            writer_database
                .execute(&format!(
                    "INSERT INTO readings VALUES ({id}, {id}.5, {}, 'row-{id}');",
                    id % 2 == 0
                ))
                .unwrap();
            writer_published.wait();
            writer_finished.wait();
        }
    });

    let reader_database = database.clone();
    let reader_published = Arc::clone(&row_published);
    let reader_finished = Arc::clone(&read_finished);
    let reader = thread::spawn(move || {
        let mut snapshots = Vec::new();
        for _ in 0..ROWS {
            reader_published.wait();
            snapshots.push(query_result(
                reader_database
                    .execute("SELECT id, ratio, active, label FROM readings ORDER BY id;")
                    .unwrap(),
            ));
            reader_finished.wait();
        }
        snapshots
    });

    writer.join().unwrap();
    let snapshots = reader.join().unwrap();

    for (last_id, snapshot) in snapshots.iter().enumerate() {
        let expected = (0..=last_id as i64)
            .map(|id| {
                vec![
                    Value::Int64(id),
                    Value::Float64(id as f64 + 0.5),
                    Value::Bool(id % 2 == 0),
                    Value::String(format!("row-{id}")),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(snapshot.rows, expected);
    }
    assert_eq!(snapshots[0].rows[0][3], Value::String("row-0".to_owned()));
}

#[test]
fn concurrent_batches_do_not_interleave_their_statements() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (owner String, sequence Int64);")
        .unwrap();
    let start = Arc::new(Barrier::new(3));

    let run_batch = |owner: &'static str, database: SharedDatabase, start: Arc<Barrier>| {
        thread::spawn(move || {
            start.wait();
            database
                .execute(&format!(
                    "INSERT INTO events VALUES ('{owner}', 1); \
                     SELECT COUNT(*) AS seen FROM events; \
                     INSERT INTO events VALUES ('{owner}', 2); \
                     SELECT COUNT(*) AS seen FROM events;"
                ))
                .unwrap()
        })
    };

    let first = run_batch("first", database.clone(), Arc::clone(&start));
    let second = run_batch("second", database.clone(), Arc::clone(&start));
    start.wait();

    let mut observed = vec![
        scalar_counts(&first.join().unwrap()),
        scalar_counts(&second.join().unwrap()),
    ];
    observed.sort_unstable();
    assert_eq!(observed, vec![vec![1, 2], vec![3, 4]]);
}

#[test]
fn configured_and_retained_result_limits_are_enforced() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE values_table (value Int64); \
             INSERT INTO values_table VALUES (1), (2);",
        )
        .unwrap();

    assert_eq!(
        database.execute("SELECT value FROM values_table;"),
        Err(SharedDatabaseError::Sql(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 2,
            max: 1,
        }))
    );
    assert!(matches!(
        database.execute_with_result_limit("SHOW TABLES;", 0),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            max_bytes: 0,
            ..
        }))
    ));
}

#[test]
fn sql_errors_are_preserved_and_poisoning_is_typed() {
    let database = SharedDatabase::default();
    assert_eq!(
        database.execute("SELECT value FROM missing;"),
        Err(SharedDatabaseError::Sql(Error::TableNotFound(
            "missing".to_owned()
        )))
    );
    assert!(matches!(
        database.execute("SELECT FROM;"),
        Err(SharedDatabaseError::Sql(Error::Sql { .. }))
    ));

    let inner = Arc::new(Mutex::new(Database::new()));
    let poisoned = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.lock().unwrap();
        panic!("poison the database lock");
    });

    assert!(poisoner.join().is_err());
    assert_eq!(
        poisoned.execute("SHOW TABLES;"),
        Err(SharedDatabaseError::LockPoisoned)
    );
    assert_eq!(
        poisoned.query_result_limits(),
        Err(SharedDatabaseError::LockPoisoned)
    );
}
