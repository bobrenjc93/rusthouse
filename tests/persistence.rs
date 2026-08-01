use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, StatementResult, Value};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn temporary_path(name: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rusthouse-{name}-{}-{sequence}.db",
        std::process::id()
    ))
}

fn remove_database(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

#[test]
fn committed_generation_reopens_with_schema_and_rows() {
    let path = temporary_path("reopen");
    {
        let database = Database::open(&path).unwrap();
        let mut session = database.session();
        session.begin().unwrap();
        session
            .execute("CREATE TABLE durable (id Int64, ok Bool, label String NULL)")
            .unwrap();
        session
            .execute("INSERT INTO durable VALUES (1, true, 'one'), (2, false, NULL)")
            .unwrap();
        assert_eq!(session.commit().unwrap(), 1);
    }

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.current_generation().unwrap(), 1);
    let StatementResult::Query(result) = reopened
        .execute("SELECT label, id FROM durable WHERE ok = false")
        .unwrap()
    else {
        panic!("expected query result");
    };
    assert_eq!(result.rows, vec![vec![Value::Null, Value::Int64(2)]]);
    drop(reopened);
    remove_database(&path);
}

#[test]
fn corrupt_snapshot_is_rejected_without_recovery_guessing() {
    let path = temporary_path("corrupt");
    fs::write(&path, b"not a rusthouse snapshot").unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::CorruptSnapshot(_))
    ));
    remove_database(&path);
}

#[test]
fn a_persisted_path_has_one_exclusive_owner() {
    let path = temporary_path("exclusive");
    let owner = Database::open(&path).unwrap();
    owner.execute("CREATE TABLE first (id Int64)").unwrap();

    assert!(matches!(
        Database::open(&path),
        Err(Error::DatabaseAlreadyOpen(_))
    ));
    drop(owner);

    let next_owner = Database::open(&path).unwrap();
    assert!(matches!(
        next_owner.execute("SELECT * FROM first"),
        Ok(StatementResult::Query(_))
    ));
    drop(next_owner);
    remove_database(&path);
}

#[test]
fn writer_rejects_catalog_shape_that_reopen_would_reject() {
    let path = temporary_path("wide");
    let database = Database::open(&path).unwrap();
    let mut session = database.session();
    session.begin().unwrap();
    let columns = (0..4_097)
        .map(|index| format!("c{index} Int64"))
        .collect::<Vec<_>>();
    session
        .execute(&format!("CREATE TABLE too_wide ({})", columns.join(", ")))
        .unwrap();

    assert!(matches!(
        session.commit(),
        Err(Error::SnapshotLimitExceeded {
            resource: "columns per table",
            limit: 4_096,
            attempted: 4_097,
        })
    ));
    assert!(session.in_transaction());
    session.rollback().unwrap();

    session.begin().unwrap();
    session
        .execute(&format!(
            "CREATE TABLE widest_supported ({})",
            columns[..4_096].join(", ")
        ))
        .unwrap();
    assert_eq!(session.commit().unwrap(), 1);
    drop(session);
    drop(database);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.current_generation().unwrap(), 1);
    assert!(matches!(
        reopened.execute("SELECT * FROM too_wide"),
        Err(Error::TableNotFound(_))
    ));
    let StatementResult::Query(result) =
        reopened.execute("SELECT * FROM widest_supported").unwrap()
    else {
        panic!("expected query result");
    };
    assert_eq!(result.columns.len(), 4_096);
    drop(reopened);
    remove_database(&path);
}
