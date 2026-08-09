use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseMetrics, DatabaseSnapshotRestoreError, Int64Table, Int64TablePayloadCodec,
    Int64TablePayloadFileRestoreError, Schema, SharedDatabase, SharedDatabaseSnapshotRestoreError,
    SnapshotCodec, SnapshotError, TableLimits,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/shared-database-snapshot-restore-tests");
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
    column: &str,
    row_cap: usize,
    rows: &[Option<i64>],
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN
        + column.len()
        + rows
            .iter()
            .map(|value| 1 + usize::from(value.is_some()) * size_of::<i64>())
            .sum::<usize>();
    (
        SnapshotCodec::new(payload_len),
        Int64TablePayloadCodec::new(column.len(), row_cap, payload_len),
    )
}

fn write_snapshot(
    path: &Path,
    column: &str,
    row_cap: usize,
    rows: &[Option<i64>],
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let mut table = Int64Table::new(Schema::int64(column, false), row_cap);
    table.append_batch(rows).unwrap();
    let (snapshot_codec, payload_codec) = codecs(column, row_cap, rows);
    let payload = payload_codec.encode(&table).unwrap();
    fs::write(path, snapshot_codec.encode(&payload).unwrap()).unwrap();
    (snapshot_codec, payload_codec)
}

fn populated_database() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE existing (id Int64); INSERT INTO existing VALUES (7);")
        .unwrap();
    database
}

#[test]
fn restores_one_int64_snapshot_as_a_queryable_table() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [Some(i64::MAX), Some(-7), Some(i64::MIN)];
    let (snapshot_codec, payload_codec) = write_snapshot(&path, "reading", 4, &rows);
    let database = SharedDatabase::with_table_limits(TableLimits::new(4, 1, 4));

    database
        .try_restore_int64_table_from_file("Readings", path, snapshot_codec, payload_codec)
        .unwrap();

    assert_eq!(
        database
            .query("SELECT reading FROM READINGS ORDER BY reading;")
            .unwrap()
            .rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(-7)],
            vec![Value::Int64(i64::MAX)],
        ]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 1,
            retained_row_count: 3,
            retained_value_bytes: 24,
        })
    );
}

#[test]
fn reader_and_writer_contention_return_busy_before_source_access() {
    let directory = TestDirectory::new();
    let missing_path = directory.join("missing.snapshot");
    let (snapshot_codec, payload_codec) = codecs("reading", 1, &[Some(1)]);
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));

    let reader = inner.read().unwrap();
    let reader_database = database.clone();
    let reader_path = missing_path.clone();
    let (reader_sender, reader_receiver) = mpsc::channel();
    let reader_worker = thread::spawn(move || {
        reader_sender
            .send(reader_database.try_restore_int64_table_from_file(
                "readings",
                reader_path,
                snapshot_codec,
                payload_codec,
            ))
            .unwrap();
    });
    let reader_error = reader_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("restore lock acquisition must not wait for an existing reader")
        .unwrap_err();
    assert!(matches!(
        reader_error,
        SharedDatabaseSnapshotRestoreError::DatabaseBusy
    ));
    drop(reader);
    reader_worker.join().unwrap();

    let writer = inner.write().unwrap();
    let writer_database = database.clone();
    let writer_path = missing_path.clone();
    let (writer_sender, writer_receiver) = mpsc::channel();
    let writer_worker = thread::spawn(move || {
        writer_sender
            .send(writer_database.try_restore_int64_table_from_file(
                "readings",
                writer_path,
                snapshot_codec,
                payload_codec,
            ))
            .unwrap();
    });
    let writer_error = writer_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("restore lock acquisition must not wait for an existing writer")
        .unwrap_err();
    assert!(matches!(
        writer_error,
        SharedDatabaseSnapshotRestoreError::DatabaseBusy
    ));
    drop(writer);
    writer_worker.join().unwrap();

    assert!(!missing_path.exists());
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 0,
            column_count: 0,
            retained_row_count: 0,
            retained_value_bytes: 0,
        })
    );
}

#[test]
fn corruption_preserves_catalog_data_and_cached_metrics() {
    let directory = TestDirectory::new();
    let path = directory.join("corrupt.snapshot");
    let (snapshot_codec, payload_codec) = write_snapshot(&path, "reading", 1, &[Some(9)]);
    let mut corrupt = fs::read(&path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&path, corrupt).unwrap();
    let database = SharedDatabase::new(populated_database());
    let tables_before = database.query("SHOW TABLES;").unwrap();
    let rows_before = database.query("SELECT id FROM existing;").unwrap();
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_restore_int64_table_from_file("corrupt", path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotRestoreError::Snapshot(DatabaseSnapshotRestoreError::Snapshot(
            Int64TablePayloadFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
        ))
    ));
    assert_eq!(database.query("SHOW TABLES;").unwrap(), tables_before);
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap(),
        rows_before
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn duplicate_names_preserve_catalog_data_and_cached_metrics() {
    let directory = TestDirectory::new();
    let missing_path = directory.join("missing.snapshot");
    let (snapshot_codec, payload_codec) = codecs("reading", 1, &[Some(9)]);
    let database = SharedDatabase::new(populated_database());
    let tables_before = database.query("SHOW TABLES;").unwrap();
    let rows_before = database.query("SELECT id FROM existing;").unwrap();
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_restore_int64_table_from_file("EXISTING", &missing_path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotRestoreError::Snapshot(
            DatabaseSnapshotRestoreError::Table(Error::TableAlreadyExists(ref table))
        ) if table == "EXISTING"
    ));
    assert!(!missing_path.exists());
    assert_eq!(database.query("SHOW TABLES;").unwrap(), tables_before);
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap(),
        rows_before
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn poisoning_is_distinct_and_precedes_source_access() {
    let directory = TestDirectory::new();
    let missing_path = directory.join("missing.snapshot");
    let (snapshot_codec, payload_codec) = codecs("reading", 1, &[Some(9)]);
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn({
        let inner = Arc::clone(&inner);
        move || {
            let _guard = inner.write().unwrap();
            panic!("poison the database lock");
        }
    });
    assert!(poisoner.join().is_err());

    let error = database
        .try_restore_int64_table_from_file("restored", &missing_path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotRestoreError::LockPoisoned
    ));
    assert!(!missing_path.exists());
    let poisoned_guard = inner.read().unwrap_err().into_inner();
    assert_eq!(poisoned_guard.catalog().table_count(), 1);
    assert_eq!(poisoned_guard.catalog().retained_row_count(), 1);
    assert_eq!(poisoned_guard.catalog().retained_value_bytes(), 8);
}
