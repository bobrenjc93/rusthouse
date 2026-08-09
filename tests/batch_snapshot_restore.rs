use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::{QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseMetrics, DatabaseSnapshotRestoreError, Int64Table, Int64TablePayloadCodec,
    Int64TablePayloadError, Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError,
    Schema, SharedDatabase, SnapshotCodec, SnapshotError, TableLimits,
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

fn cached_metrics(database: &mut Database) -> QueryResult {
    let mut results = database
        .execute("SELECT metric, value FROM system.metrics")
        .unwrap();
    assert_eq!(results.len(), 1);
    match results.pop().unwrap() {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => {
            unreachable!("system.metrics always returns one query result")
        }
    }
}

#[test]
fn backup_restore_prefers_a_valid_primary_without_inspecting_the_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&primary_path, "reading", false, 2, &[Some(11)]);
    let mut database = Database::with_table_limits(TableLimits::new(2, 1, 2));

    let source = database
        .restore_int64_table_from_file_with_backup(
            "Readings",
            primary_path,
            directory.join("missing-backup.snapshot"),
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(source, Int64TablePayloadFileRecoverySource::Primary);
    assert_eq!(database.catalog().table("readings").unwrap().row_count(), 1);
    let mut results = database.execute("SELECT reading FROM readings").unwrap();
    assert_eq!(results.len(), 1);
    let StatementResult::Query(result) = results.pop().unwrap() else {
        unreachable!("SELECT always returns one query result")
    };
    assert_eq!(result.rows, [[Value::Int64(11)]]);
}

#[test]
fn corrupt_and_missing_primaries_recover_the_explicit_backup() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("backup.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&backup_path, "reading", false, 2, &[Some(22)]);

    let mut missing_primary = Database::with_table_limits(TableLimits::new(2, 1, 2));
    assert_eq!(
        missing_primary
            .restore_int64_table_from_file_with_backup(
                "missing_recovery",
                directory.join("missing-primary.snapshot"),
                &backup_path,
                snapshot_codec,
                payload_codec,
            )
            .unwrap(),
        Int64TablePayloadFileRecoverySource::Backup
    );
    assert_eq!(
        missing_primary
            .catalog()
            .table("missing_recovery")
            .unwrap()
            .row_count(),
        1
    );

    let corrupt_path = directory.join("corrupt-primary.snapshot");
    let mut corrupt = fs::read(&backup_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();
    let mut corrupt_primary = Database::with_table_limits(TableLimits::new(2, 1, 2));
    assert_eq!(
        corrupt_primary
            .restore_int64_table_from_file_with_backup(
                "corrupt_recovery",
                corrupt_path,
                backup_path,
                snapshot_codec,
                payload_codec,
            )
            .unwrap(),
        Int64TablePayloadFileRecoverySource::Backup
    );
}

#[test]
fn backup_restore_preserves_both_failures_and_cached_metrics() {
    let directory = TestDirectory::new();
    let corrupt_backup_path = directory.join("corrupt-backup.snapshot");
    let valid_path = directory.join("valid.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&valid_path, "reading", false, 2, &[Some(22)]);
    let mut corrupt_payload = payload_codec
        .encode(&Int64Table::new(Schema::int64("reading", false), 2))
        .unwrap();
    corrupt_payload[0] ^= 1;
    fs::write(
        &corrupt_backup_path,
        snapshot_codec.encode(&corrupt_payload).unwrap(),
    )
    .unwrap();

    let mut database = Database::new();
    database
        .execute("CREATE TABLE existing (id Int64); INSERT INTO existing VALUES (7);")
        .unwrap();
    let metrics_before = cached_metrics(&mut database);
    let error = database
        .restore_int64_table_from_file_with_backup(
            "recovered",
            directory.join("missing-primary.snapshot"),
            corrupt_backup_path,
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    let DatabaseSnapshotRestoreError::Recovery(recovery) = error else {
        panic!("expected dual recovery error, got {error:?}");
    };
    assert!(matches!(
        recovery.primary_error(),
        Int64TablePayloadFileRestoreError::Open(source)
            if source.kind() == ErrorKind::NotFound
    ));
    assert!(matches!(
        recovery.backup_error(),
        Int64TablePayloadFileRestoreError::Payload(
            Int64TablePayloadError::IncompatibleMagic { .. }
        )
    ));
    let (primary, backup) = recovery.into_errors();
    assert!(matches!(
        primary,
        Int64TablePayloadFileRestoreError::Open(_)
    ));
    assert!(matches!(
        backup,
        Int64TablePayloadFileRestoreError::Payload(
            Int64TablePayloadError::IncompatibleMagic { .. }
        )
    ));
    assert_eq!(database.catalog().table_count(), 1);
    assert_eq!(cached_metrics(&mut database), metrics_before);
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
        database.restore_int64_table_from_file_with_backup(
            "READINGS",
            directory.join("does-not-exist.snapshot"),
            directory.join("backup-does-not-exist.snapshot"),
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
        row_limited.restore_int64_table_from_file_with_backup(
            "limited",
            directory.join("missing-row-primary.snapshot"),
            &path,
            snapshot_codec,
            payload_codec,
        ),
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
        column_limited.restore_int64_table_from_file_with_backup(
            "limited",
            directory.join("missing-column-primary.snapshot"),
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
        cell_limited.restore_int64_table_from_file_with_backup(
            "limited",
            directory.join("missing-cell-primary.snapshot"),
            path,
            snapshot_codec,
            payload_codec,
        ),
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
