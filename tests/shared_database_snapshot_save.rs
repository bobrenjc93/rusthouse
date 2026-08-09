#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::StatementResult;
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseSnapshotSaveError, Int64TablePayloadCodec, Int64TablePayloadError,
    Int64TablePayloadFileSaveError, SharedDatabase, SharedDatabaseSnapshotSaveError, SnapshotCodec,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/shared-database-snapshot-save-tests");
        fs::create_dir_all(&base).unwrap();
        loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn codecs(
    column_name: &str,
    row_cap: usize,
    row_count: usize,
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN
        + column_name.len()
        + row_count * (size_of::<u8>() + size_of::<i64>());
    (
        SnapshotCodec::new(payload_len),
        Int64TablePayloadCodec::new(column_name.len(), row_cap, payload_len),
    )
}

fn populated_database() -> Database {
    let mut database = Database::with_max_rows_per_table(5);
    database
        .execute(
            "CREATE TABLE Readings (Measurement Int64); \
             INSERT INTO Readings VALUES (-9223372036854775808), (7), (9223372036854775807);",
        )
        .unwrap();
    database
}

#[test]
fn saves_and_reopens_one_int64_table() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let database = SharedDatabase::new(populated_database());
    let (snapshot_codec, payload_codec) = codecs("Measurement", 5, 3);

    database
        .try_save_int64_table_to_file("READINGS", &path, snapshot_codec, payload_codec)
        .unwrap();

    let mut reopened = Database::with_max_rows_per_table(5);
    reopened
        .restore_int64_table_from_file("Archive", path, snapshot_codec, payload_codec)
        .unwrap();
    let results = reopened
        .execute("SELECT Measurement FROM archive;")
        .unwrap();
    let [StatementResult::Query(query)] = results.as_slice() else {
        panic!("the restored table must produce one query result");
    };
    assert_eq!(
        query.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(7)],
            vec![Value::Int64(i64::MAX)],
        ]
    );
    assert_eq!(reopened.catalog().table("ARCHIVE").unwrap().row_cap(), 5);
}

#[test]
fn returns_busy_without_waiting_for_a_writer_or_touching_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let writer = inner.write().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (snapshot_codec, payload_codec) = codecs("Measurement", 5, 3);
        sender
            .send(database.try_save_int64_table_to_file(
                "readings",
                path,
                snapshot_codec,
                payload_codec,
            ))
            .unwrap();
    });

    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("snapshot lock acquisition must not wait for the writer")
        .unwrap_err();
    assert!(matches!(
        error,
        SharedDatabaseSnapshotSaveError::DatabaseBusy
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(
        fs::read(directory.join("preserved.snapshot")).unwrap(),
        original
    );

    drop(writer);
    worker.join().unwrap();
}

#[test]
fn saves_while_an_existing_reader_remains_held() {
    let directory = TestDirectory::new();
    let path = directory.join("read-compatible.snapshot");
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let existing_reader = inner.read().unwrap();
    let (snapshot_codec, payload_codec) = codecs("Measurement", 5, 3);

    database
        .try_save_int64_table_to_file("readings", &path, snapshot_codec, payload_codec)
        .unwrap();

    assert_eq!(
        existing_reader
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        3
    );
    assert!(path.is_file());
}

#[test]
fn preserves_typed_snapshot_errors_and_the_existing_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let database = SharedDatabase::new(populated_database());
    let (snapshot_codec, _) = codecs("Measurement", 5, 3);
    let payload_codec = Int64TablePayloadCodec::new("Measurement".len() - 1, 5, 128);

    let error = database
        .try_save_int64_table_to_file("readings", &path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSaveError::Snapshot(DatabaseSnapshotSaveError::Snapshot(
            Int64TablePayloadFileSaveError::Payload(Int64TablePayloadError::NameTooLong { .. })
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn distinguishes_a_poisoned_lock_from_snapshot_validation() {
    let directory = TestDirectory::new();
    let path = directory.join("untouched.snapshot");
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());
    let (snapshot_codec, payload_codec) = codecs("Measurement", 5, 3);

    let error = database
        .try_save_int64_table_to_file("missing", &path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSaveError::LockPoisoned
    ));
    assert!(!error.destination_was_replaced());
    assert!(!path.exists());

    let healthy = SharedDatabase::default();
    let table_error = healthy
        .try_save_int64_table_to_file("missing", &path, snapshot_codec, payload_codec)
        .unwrap_err();
    assert!(matches!(
        table_error,
        SharedDatabaseSnapshotSaveError::Snapshot(DatabaseSnapshotSaveError::Table(
            Error::TableNotFound(ref table)
        )) if table == "missing"
    ));
}
