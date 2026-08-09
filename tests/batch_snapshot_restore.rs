use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseMetrics, DatabaseSnapshotRestoreError, Int64Table, Int64TablePayloadCodec,
    Int64TablePayloadFileRestoreError, Schema, SharedDatabase, SnapshotCodec, SnapshotError,
    TableLimits,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-snapshot-tests");
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

fn write_snapshot(
    path: &Path,
    column: &str,
    nullable: bool,
    row_cap: usize,
    rows: &[Option<i64>],
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let mut table = Int64Table::new(Schema::int64(column, nullable), row_cap);
    table.append_batch(rows).unwrap();
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN
        + column.len()
        + rows
            .iter()
            .map(|value| 1 + usize::from(value.is_some()) * size_of::<i64>())
            .sum::<usize>();
    let payload_codec = Int64TablePayloadCodec::new(column.len(), row_cap, payload_len);
    let payload = payload_codec.encode(&table).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload_len);
    fs::write(path, snapshot_codec.encode(&payload).unwrap()).unwrap();
    (snapshot_codec, payload_codec)
}

fn assert_empty(database: &Database) {
    assert_eq!(database.catalog().table_count(), 0);
    assert_eq!(database.catalog().column_count(), 0);
    assert_eq!(database.catalog().retained_row_count(), 0);
    assert_eq!(database.catalog().retained_value_bytes(), 0);
}

#[test]
fn reopens_to_select_and_metrics_at_exact_snapshot_and_table_limits() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [Some(i64::MAX), Some(-7), Some(i64::MIN)];
    let (snapshot_codec, payload_codec) =
        write_snapshot(&path, "reading", false, rows.len(), &rows);
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 3));

    database
        .restore_int64_table_from_file("Readings", path, snapshot_codec, payload_codec)
        .unwrap();

    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.limits(), TableLimits::new(3, 1, 3));
    let shared = SharedDatabase::new(database);
    let query = shared
        .query("SELECT reading FROM READINGS ORDER BY reading;")
        .unwrap();
    assert_eq!(
        query.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(-7)],
            vec![Value::Int64(i64::MAX)],
        ]
    );
    assert_eq!(
        shared.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 1,
            retained_row_count: 3,
            retained_value_bytes: 24,
        })
    );
    assert!(matches!(
        shared.execute("INSERT INTO readings VALUES (0);"),
        Err(rusthouse::SharedDatabaseError::Sql(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 4,
                max: 3,
            }
        ))
    ));
}

#[test]
fn corruption_nullability_and_duplicate_names_leave_existing_state_unchanged() {
    let directory = TestDirectory::new();
    let valid_path = directory.join("valid.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&valid_path, "value", false, 2, &[Some(8)]);
    let corrupt_path = directory.join("corrupt.snapshot");
    let mut corrupt = fs::read(&valid_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();

    let mut database = Database::with_table_limits(TableLimits::new(2, 1, 2));
    let corrupt_error = database
        .restore_int64_table_from_file("corrupt", corrupt_path, snapshot_codec, payload_codec)
        .unwrap_err();
    assert!(matches!(
        corrupt_error,
        DatabaseSnapshotRestoreError::Snapshot(Int64TablePayloadFileRestoreError::Envelope(
            SnapshotError::ChecksumMismatch { .. }
        ))
    ));
    assert_empty(&database);

    let nullable_path = directory.join("nullable.snapshot");
    let (nullable_snapshot_codec, nullable_payload_codec) =
        write_snapshot(&nullable_path, "value", true, 2, &[Some(9)]);
    assert!(matches!(
        database.restore_int64_table_from_file(
            "nullable",
            nullable_path,
            nullable_snapshot_codec,
            nullable_payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::NullableColumn { ref column }) if column == "value"
    ));
    assert_empty(&database);

    database
        .restore_int64_table_from_file("Readings", &valid_path, snapshot_codec, payload_codec)
        .unwrap();
    assert!(matches!(
        database.restore_int64_table_from_file(
            "READINGS",
            directory.join("does-not-exist.snapshot"),
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(Error::TableAlreadyExists(ref name)))
            if name == "READINGS"
    ));
    assert_eq!(database.catalog().table_count(), 1);
    assert_eq!(database.catalog().retained_row_count(), 1);
    assert_eq!(database.catalog().retained_value_bytes(), 8);
}

#[test]
fn row_cap_column_and_cell_limit_failures_are_atomic() {
    let directory = TestDirectory::new();
    let path = directory.join("limited.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&path, "value", false, 3, &[Some(1), Some(2)]);

    let mut row_limited = Database::with_table_limits(TableLimits::new(2, 1, 2));
    assert!(matches!(
        row_limited.restore_int64_table_from_file("limited", &path, snapshot_codec, payload_codec,),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table row cap",
                actual: 3,
                max: 2,
            }
        ))
    ));
    assert_empty(&row_limited);

    let mut column_limited = Database::with_table_limits(TableLimits::new(3, 0, 2));
    assert!(matches!(
        column_limited.restore_int64_table_from_file(
            "limited",
            &path,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table columns",
                actual: 1,
                max: 0,
            }
        ))
    ));
    assert_empty(&column_limited);

    let mut cell_limited = Database::with_table_limits(TableLimits::new(3, 1, 1));
    assert!(matches!(
        cell_limited.restore_int64_table_from_file("limited", path, snapshot_codec, payload_codec,),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: 2,
                max: 1,
            }
        ))
    ));
    assert_empty(&cell_limited);
}
