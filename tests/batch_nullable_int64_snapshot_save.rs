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

use rusthouse::snapshot::{INT64_TABLE_PAYLOAD_FIXED_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Database, DatabaseSnapshotSaveError, Int64TablePayloadCodec, Int64TablePayloadError, Schema,
    SharedDatabase, SharedDatabaseSnapshotSaveError, SnapshotCodec,
    restore_int64_table_payload_from_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-nullable-snapshot-save-tests");
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

fn payload_len(column_name: &str, rows: &[Option<i64>]) -> usize {
    INT64_TABLE_PAYLOAD_FIXED_LEN
        + column_name.len()
        + rows
            .iter()
            .map(|value| size_of::<u8>() + value.map_or(0, |_| size_of::<i64>()))
            .sum::<usize>()
}

fn exact_codecs(
    column_name: &str,
    row_cap: usize,
    rows: &[Option<i64>],
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let payload_len = payload_len(column_name, rows);
    (
        SnapshotCodec::new(payload_len),
        Int64TablePayloadCodec::new(column_name.len(), row_cap, payload_len),
    )
}

fn nullable_database(row_cap: usize, rows_sql: &str) -> Database {
    let mut database = Database::with_max_rows_per_table(row_cap);
    database
        .execute(&format!(
            "CREATE TABLE Metrics (Reading Nullable(Int64)); {rows_sql}"
        ))
        .unwrap();
    database
}

#[test]
fn saves_and_decodes_an_empty_nullable_table_with_its_row_cap() {
    let directory = TestDirectory::new();
    let path = directory.join("empty.snapshot");
    let database = nullable_database(4, "");
    let rows = [];
    let (snapshot_codec, payload_codec) = exact_codecs("Reading", 4, &rows);

    database
        .save_int64_table_to_file("metrics", &path, snapshot_codec, payload_codec)
        .unwrap();

    let restored =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(restored.schema(), &Schema::int64("Reading", true));
    assert_eq!(restored.row_cap(), 4);
    assert!(restored.values().is_empty());
}

#[test]
fn saves_all_null_rows_at_every_exact_codec_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("all-null.snapshot");
    let rows = [None, None, None];
    let database = nullable_database(
        rows.len(),
        "INSERT INTO Metrics VALUES (NULL), (NULL), (NULL);",
    );
    let (snapshot_codec, payload_codec) = exact_codecs("Reading", rows.len(), &rows);

    database
        .save_int64_table_to_file("METRICS", &path, snapshot_codec, payload_codec)
        .unwrap();

    let restored =
        restore_int64_table_payload_from_file(&path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(restored.schema(), &Schema::int64("Reading", true));
    assert_eq!(restored.row_cap(), rows.len());
    assert_eq!(restored.values(), rows);
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len("Reading", &rows)
    );
}

#[test]
fn mixed_rows_round_trip_with_exact_null_positions_and_integer_boundaries() {
    let directory = TestDirectory::new();
    let path = directory.join("mixed.snapshot");
    let rows = [None, Some(i64::MIN), None, Some(i64::MAX)];
    let database = nullable_database(
        rows.len(),
        "INSERT INTO Metrics VALUES \
         (NULL), (-9223372036854775808), (NULL), (9223372036854775807);",
    );
    let (snapshot_codec, payload_codec) = exact_codecs("Reading", rows.len(), &rows);

    database
        .save_int64_table_to_file("Metrics", &path, snapshot_codec, payload_codec)
        .unwrap();

    let restored =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(restored.schema(), &Schema::int64("Reading", true));
    assert_eq!(restored.row_cap(), rows.len());
    assert_eq!(restored.values(), rows);
}

#[test]
fn nullable_payload_limit_failure_is_typed_and_preserves_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let rows = [None, Some(7)];
    let database = nullable_database(rows.len(), "INSERT INTO Metrics VALUES (NULL), (7);");
    let exact_payload_len = payload_len("Reading", &rows);

    let error = database
        .save_int64_table_to_file(
            "metrics",
            &path,
            SnapshotCodec::new(exact_payload_len),
            Int64TablePayloadCodec::new("Reading".len(), rows.len(), exact_payload_len - 1),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        DatabaseSnapshotSaveError::Snapshot(
            rusthouse::Int64TablePayloadFileSaveError::Payload(
                Int64TablePayloadError::PayloadTooLarge {
                    payload_len: actual,
                    max_payload_len,
                }
            )
        ) if actual == exact_payload_len as u64 && max_payload_len == exact_payload_len - 1
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn shared_save_of_nullable_rows_succeeds_without_waiting_for_an_existing_reader() {
    let directory = TestDirectory::new();
    let path = directory.join("shared.snapshot");
    let rows = [Some(-7), None, Some(11)];
    let inner = Arc::new(RwLock::new(nullable_database(
        5,
        "INSERT INTO Metrics VALUES (-7), (NULL), (11);",
    )));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let existing_reader = inner.read().unwrap();
    let (snapshot_codec, payload_codec) = exact_codecs("Reading", 5, &rows);

    database
        .try_save_int64_table_to_file("metrics", &path, snapshot_codec, payload_codec)
        .unwrap();

    assert_eq!(
        existing_reader
            .catalog()
            .table("metrics")
            .unwrap()
            .row_count(),
        3
    );
    let restored =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(restored.row_cap(), 5);
    assert_eq!(restored.values(), rows);
}

#[test]
fn shared_nullable_save_returns_busy_without_touching_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let rows = [None, Some(7)];
    let inner = Arc::new(RwLock::new(nullable_database(
        rows.len(),
        "INSERT INTO Metrics VALUES (NULL), (7);",
    )));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let writer = inner.write().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (snapshot_codec, payload_codec) = exact_codecs("Reading", rows.len(), &rows);
        sender
            .send(database.try_save_int64_table_to_file(
                "metrics",
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
