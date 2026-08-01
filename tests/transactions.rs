use std::sync::{Arc, Barrier};
use std::thread;

use rusthouse::{Database, Error, LimitKind, Session, StatementResult, TransactionLimits, Value};

fn query(session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match session.execute(sql).unwrap() {
        StatementResult::Query(result) => result.rows,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn reads_own_writes_and_rollback_discards_them() {
    let database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String)")
        .unwrap();
    let mut writer = database.session();
    let mut observer = database.session();

    writer.execute("BEGIN").unwrap();
    writer
        .execute("INSERT INTO events VALUES (1, 'staged')")
        .unwrap();
    assert_eq!(query(&mut writer, "SELECT * FROM events").len(), 1);
    assert!(query(&mut observer, "SELECT * FROM events").is_empty());

    writer.execute("ROLLBACK").unwrap();
    assert!(query(&mut writer, "SELECT * FROM events").is_empty());
}

#[test]
fn reader_snapshot_is_stable_while_another_session_commits() {
    let database = Database::new();
    database
        .execute("CREATE TABLE readings (id Int64)")
        .unwrap();
    database.execute("INSERT INTO readings VALUES (1)").unwrap();
    let mut reader = database.session();
    let mut writer = database.session();

    let pinned = reader.begin().unwrap();
    writer.execute("INSERT INTO readings VALUES (2)").unwrap();

    assert_eq!(reader.snapshot_generation(), Some(pinned));
    assert_eq!(query(&mut reader, "SELECT * FROM readings").len(), 1);
    assert_eq!(query(&mut writer, "SELECT * FROM readings").len(), 2);
    reader.commit().unwrap();
    assert_eq!(query(&mut reader, "SELECT * FROM readings").len(), 2);
}

#[test]
fn ddl_and_dml_become_visible_in_one_commit() {
    let database = Database::new();
    let mut owner = database.session();
    let mut observer = database.session();

    owner.execute("BEGIN").unwrap();
    owner
        .execute("CREATE TABLE pending (id Int64, note String NULL)")
        .unwrap();
    owner
        .execute("INSERT INTO pending (id) VALUES (7)")
        .unwrap();
    assert_eq!(
        query(&mut owner, "SELECT * FROM pending"),
        vec![vec![Value::Int64(7), Value::Null]]
    );
    assert!(matches!(
        observer.execute("SELECT * FROM pending"),
        Err(Error::TableNotFound(_))
    ));

    owner.execute("COMMIT").unwrap();
    assert_eq!(query(&mut observer, "SELECT * FROM pending").len(), 1);
}

#[test]
fn concurrent_writes_to_the_same_table_conflict() {
    let database = Database::new();
    database
        .execute("CREATE TABLE values_table (id Int64)")
        .unwrap();
    let mut first = database.session();
    let mut second = database.session();
    first.begin().unwrap();
    second.begin().unwrap();
    first
        .execute("INSERT INTO values_table VALUES (1)")
        .unwrap();
    second
        .execute("INSERT INTO values_table VALUES (2)")
        .unwrap();

    first.commit().unwrap();
    assert!(matches!(
        second.commit(),
        Err(Error::Conflict {
            table,
            base_generation: 1,
            current_generation: 2,
        }) if table == "values_table"
    ));
    assert!(!second.in_transaction());
    assert_eq!(
        query(&mut second, "SELECT * FROM values_table"),
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn disjoint_writers_merge_against_the_latest_generation() {
    let database = Database::new();
    database.execute("CREATE TABLE left_t (id Int64)").unwrap();
    database.execute("CREATE TABLE right_t (id Int64)").unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = [("left_t", 10), ("right_t", 20)]
        .into_iter()
        .map(|(table, value)| {
            let database = database.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut session = database.session();
                session.begin().unwrap();
                barrier.wait();
                session
                    .execute(&format!("INSERT INTO {table} VALUES ({value})"))
                    .unwrap();
                session.commit()
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let mut reader = database.session();
    assert_eq!(query(&mut reader, "SELECT * FROM left_t").len(), 1);
    assert_eq!(query(&mut reader, "SELECT * FROM right_t").len(), 1);
    assert_eq!(database.current_generation().unwrap(), 4);
}

#[test]
fn transaction_limits_are_cumulative_and_failed_statement_is_atomic() {
    let database = Database::with_limits(TransactionLimits::new(2, usize::MAX));
    database.execute("CREATE TABLE bounded (id Int64)").unwrap();
    let mut session = database.session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO bounded VALUES (1), (2)")
        .unwrap();

    assert!(matches!(
        session.execute("INSERT INTO bounded VALUES (3)"),
        Err(Error::TransactionLimitExceeded {
            kind: LimitKind::Rows,
            limit: 2,
            attempted: 3,
        })
    ));
    assert!(session.in_transaction());
    assert_eq!(query(&mut session, "SELECT * FROM bounded").len(), 2);
    session.rollback().unwrap();

    session
        .set_transaction_limits(TransactionLimits::new(10, 10))
        .unwrap();
    session.begin().unwrap();
    assert!(matches!(
        session.execute("INSERT INTO bounded VALUES (1)"),
        Err(Error::TransactionLimitExceeded {
            kind: LimitKind::Bytes,
            limit: 10,
            ..
        })
    ));
    assert!(query(&mut session, "SELECT * FROM bounded").is_empty());
}

#[test]
fn ddl_byte_limit_counts_the_complete_encoded_schema() {
    let too_small = Database::with_limits(TransactionLimits::new(0, 35));
    assert!(matches!(
        too_small.execute("CREATE TABLE t (a Int64)"),
        Err(Error::TransactionLimitExceeded {
            kind: LimitKind::Bytes,
            limit: 35,
            attempted: 36,
        })
    ));
    assert_eq!(too_small.current_generation().unwrap(), 0);

    let exact = Database::with_limits(TransactionLimits::new(0, 36));
    assert!(matches!(
        exact.execute("CREATE TABLE t (a Int64)"),
        Ok(StatementResult::TableCreated)
    ));
    assert_eq!(exact.current_generation().unwrap(), 1);
}

#[test]
fn invalid_insert_does_not_modify_transaction() {
    let database = Database::new();
    database
        .execute("CREATE TABLE typed (id Int64, required String)")
        .unwrap();
    let mut session = database.session();
    session.begin().unwrap();
    assert!(matches!(
        session.execute("INSERT INTO typed (id) VALUES (1)"),
        Err(Error::TypeMismatch { .. })
    ));
    assert!(query(&mut session, "SELECT * FROM typed").is_empty());
    session.rollback().unwrap();
}

#[test]
fn one_shot_execute_rejects_transaction_control() {
    let database = Database::new();
    for statement in ["BEGIN", "COMMIT", "ROLLBACK"] {
        assert!(matches!(
            database.execute(statement),
            Err(Error::Unsupported(message))
                if message.contains("persistent Session")
        ));
    }
    assert_eq!(database.current_generation().unwrap(), 0);
}
