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
    fs::remove_file(path).unwrap();
}

#[test]
fn corrupt_snapshot_is_rejected_without_recovery_guessing() {
    let path = temporary_path("corrupt");
    fs::write(&path, b"not a rusthouse snapshot").unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::CorruptSnapshot(_))
    ));
    fs::remove_file(path).unwrap();
}
