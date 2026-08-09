#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::error::Error;
use rusthouse::batch::value::DataType;
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseSnapshotSaveError, Int64TablePayloadCodec, Int64TablePayloadError,
    Int64TablePayloadFileSaveError, Schema, SnapshotCodec, SnapshotReplaceError,
    restore_int64_table_payload_from_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-snapshot-save-tests");
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

#[test]
fn atomically_replaces_and_round_trips_through_the_existing_decoder() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    fs::write(&path, b"old destination bytes").unwrap();
    let mut database = Database::with_max_rows_per_table(5);
    database
        .execute(
            "CREATE TABLE Readings (Measurement Int64); \
             INSERT INTO Readings VALUES (-9223372036854775808), (7), (9223372036854775807);",
        )
        .unwrap();
    let (snapshot_codec, payload_codec) = codecs("Measurement", 5, 3);

    database
        .save_int64_table_to_file("READINGS", &path, snapshot_codec, payload_codec)
        .unwrap();

    let restored =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(restored.schema(), &Schema::int64("Measurement", false));
    assert_eq!(restored.row_cap(), 5);
    assert_eq!(
        restored.values(),
        &[Some(i64::MIN), Some(7), Some(i64::MAX)]
    );
}

#[test]
fn rejects_missing_multicolumn_and_non_int64_tables_before_path_access() {
    let directory = TestDirectory::new();
    let inaccessible_path = directory.join("absent-parent").join("snapshot");
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE wide (first Int64, second Int64); \
             CREATE TABLE typed (reading Float64);",
        )
        .unwrap();
    let snapshot_codec = SnapshotCodec::new(128);
    let payload_codec = Int64TablePayloadCodec::new(128, database.max_rows_per_table(), 128);

    assert!(matches!(
        database.save_int64_table_to_file(
            "missing",
            &inaccessible_path,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotSaveError::Table(Error::TableNotFound(ref name)))
            if name == "missing"
    ));
    assert!(matches!(
        database.save_int64_table_to_file(
            "WIDE",
            &inaccessible_path,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotSaveError::UnsupportedColumnCount {
            ref table,
            column_count: 2,
        }) if table == "wide"
    ));
    assert!(matches!(
        database.save_int64_table_to_file(
            "typed",
            &inaccessible_path,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotSaveError::UnsupportedColumnType {
            ref column,
            data_type: DataType::Float64,
        }) if column == "reading"
    ));
    assert!(!directory.join("absent-parent").exists());
}

#[test]
fn payload_failure_preserves_an_existing_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE readings (measurement Int64); \
             INSERT INTO readings VALUES (1), (2);",
        )
        .unwrap();
    let (snapshot_codec, _) = codecs("measurement", 3, 2);
    let payload_codec = Int64TablePayloadCodec::new("measurement".len() - 1, 3, 128);

    let error = database
        .save_int64_table_to_file("readings", &path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        DatabaseSnapshotSaveError::Snapshot(Int64TablePayloadFileSaveError::Payload(
            Int64TablePayloadError::NameTooLong { .. }
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn codec_limits_are_checked_before_payload_allocation_or_path_access() {
    let directory = TestDirectory::new();
    let inaccessible_path = directory.join("absent-parent").join("snapshot");
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE readings (measurement Int64); \
             INSERT INTO readings VALUES (1), (2), (3);",
        )
        .unwrap();
    let snapshot_codec = SnapshotCodec::new(128);

    let row_error = database
        .save_int64_table_to_file(
            "readings",
            &inaccessible_path,
            snapshot_codec,
            Int64TablePayloadCodec::new("measurement".len(), 0, 128),
        )
        .unwrap_err();
    assert!(matches!(
        row_error,
        DatabaseSnapshotSaveError::Snapshot(Int64TablePayloadFileSaveError::Payload(
            Int64TablePayloadError::RowCapLimitExceeded {
                row_cap: 3,
                max_rows: 0,
            }
        ))
    ));
    assert!(!row_error.destination_was_replaced());

    let payload_error = database
        .save_int64_table_to_file(
            "readings",
            &inaccessible_path,
            snapshot_codec,
            Int64TablePayloadCodec::new("measurement".len(), 3, 0),
        )
        .unwrap_err();
    assert!(matches!(
        payload_error,
        DatabaseSnapshotSaveError::Snapshot(Int64TablePayloadFileSaveError::Payload(
            Int64TablePayloadError::PayloadTooLarge {
                max_payload_len: 0,
                ..
            }
        ))
    ));
    assert!(!payload_error.destination_was_replaced());
    assert!(!directory.join("absent-parent").exists());
}

#[test]
fn replacement_failure_preserves_the_destination_and_cleans_up() {
    let directory = TestDirectory::new();
    let path = directory.join("destination");
    fs::create_dir(&path).unwrap();
    let marker = path.join("marker");
    fs::write(&marker, b"keep me").unwrap();
    let mut database = Database::with_max_rows_per_table(1);
    database
        .execute("CREATE TABLE readings (value Int64); INSERT INTO readings VALUES (7);")
        .unwrap();
    let (snapshot_codec, payload_codec) = codecs("value", 1, 1);

    let error = database
        .save_int64_table_to_file("readings", &path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        DatabaseSnapshotSaveError::Snapshot(Int64TablePayloadFileSaveError::Replace(
            SnapshotReplaceError::Rename(_)
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(marker).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}
