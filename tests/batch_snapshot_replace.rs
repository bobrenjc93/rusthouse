use std::fs;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::StatementResult;
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseMetrics, DatabaseSnapshotRestoreError, Int64Table, Int64TablePayloadCodec,
    Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError, Schema, SharedDatabase,
    SnapshotCodec, SnapshotError, TableLimits,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-snapshot-replace-tests");
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

fn query_rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let mut results = database.execute(sql).unwrap();
    let StatementResult::Query(result) = results.pop().unwrap() else {
        panic!("query must return rows")
    };
    result.rows
}

fn assert_original_table(database: &mut Database) {
    assert_eq!(
        database.catalog().table("readings").unwrap().name(),
        "Readings"
    );
    assert_eq!(
        query_rows(database, "SELECT original FROM READINGS ORDER BY original;"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn nullable_replacement_preserves_metadata_nulls_row_order_cap_and_display_name() {
    let directory = TestDirectory::new();
    let path = directory.join("replacement.snapshot");
    let rows = [Some(9), None, Some(-4)];
    let (snapshot_codec, payload_codec) = write_snapshot(&path, "restored_value", true, 4, &rows);
    let mut database = Database::with_table_limits(TableLimits::new(5, 2, 10));
    database
        .execute(
            "CREATE TABLE Readings (old_value String, enabled Bool); \
             INSERT INTO Readings VALUES ('old', true);",
        )
        .unwrap();

    database
        .replace_int64_table_from_file("READINGS", path, snapshot_codec, payload_codec)
        .unwrap();

    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema().len(), 1);
    assert_eq!(table.schema()[0].name, "restored_value");
    assert_eq!(table.schema()[0].data_type, DataType::Int64);
    assert!(matches!(
        &table.columns()[0],
        Column::NullableInt64(values) if values == &rows
    ));
    assert_eq!(table.limits(), TableLimits::new(4, 2, 10));
    assert_eq!(
        query_rows(&mut database, "SELECT restored_value FROM readings;"),
        vec![
            vec![Value::Int64(9)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(-4)],
        ]
    );

    database
        .execute("INSERT INTO readings VALUES (NULL);")
        .unwrap();
    assert!(matches!(
        database.execute("INSERT INTO readings VALUES (12);"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 5,
            max: 4,
        })
    ));
}

#[test]
fn successful_replacement_updates_cached_metrics_by_the_exact_table_delta() {
    let directory = TestDirectory::new();
    let path = directory.join("metrics.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&path, "replacement", true, 3, &[Some(10), None, Some(30)]);
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (label String, enabled Bool); \
             INSERT INTO Readings VALUES ('abc', true), ('x', false); \
             CREATE TABLE Stable (id Int64); \
             INSERT INTO Stable VALUES (99);",
        )
        .unwrap();

    database
        .replace_int64_table_from_file("readings", path, snapshot_codec, payload_codec)
        .unwrap();

    let shared = SharedDatabase::new(database);
    assert_eq!(
        shared.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 2,
            column_count: 2,
            retained_row_count: 4,
            retained_value_bytes: 35,
        })
    );
}

#[test]
fn recovery_replacement_prefers_the_primary_and_preserves_the_display_name() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&primary_path, "recovered", false, 3, &[Some(10), Some(20)]);
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Readings (original Int64); INSERT INTO Readings VALUES (1);")
        .unwrap();

    let source = database
        .replace_int64_table_from_file_with_backup(
            "READINGS",
            primary_path,
            directory.join("missing-backup.snapshot"),
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(source, Int64TablePayloadFileRecoverySource::Primary);
    assert_eq!(
        database.catalog().table("readings").unwrap().name(),
        "Readings"
    );
    assert_eq!(
        query_rows(
            &mut database,
            "SELECT recovered FROM readings ORDER BY recovered;",
        ),
        vec![vec![Value::Int64(10)], vec![Value::Int64(20)]]
    );
}

#[test]
fn recovery_replacement_uses_the_backup_for_missing_and_corrupt_primaries() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("backup.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&backup_path, "recovered", true, 3, &[None, None]);
    let corrupt_primary_path = directory.join("corrupt-primary.snapshot");
    let mut corrupt_primary = fs::read(&backup_path).unwrap();
    *corrupt_primary.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_primary_path, corrupt_primary).unwrap();

    for primary_path in [
        directory.join("missing-primary.snapshot"),
        corrupt_primary_path,
    ] {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE Readings (original Int64); INSERT INTO Readings VALUES (1);")
            .unwrap();

        let source = database
            .replace_int64_table_from_file_with_backup(
                "readings",
                primary_path,
                &backup_path,
                snapshot_codec,
                payload_codec,
            )
            .unwrap();

        assert_eq!(source, Int64TablePayloadFileRecoverySource::Backup);
        assert_eq!(
            database.catalog().table("readings").unwrap().name(),
            "Readings"
        );
        assert_eq!(
            query_rows(&mut database, "SELECT recovered FROM readings;"),
            vec![
                vec![Value::Null(DataType::Int64)],
                vec![Value::Null(DataType::Int64)],
            ]
        );
        database
            .execute("INSERT INTO readings VALUES (7);")
            .unwrap();
        assert!(matches!(
            database.execute("INSERT INTO readings VALUES (8);"),
            Err(Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 4,
                max: 3,
            })
        ));
    }
}

#[test]
fn dual_recovery_failure_preserves_the_table_and_cached_metrics() {
    let directory = TestDirectory::new();
    let corrupt_backup_path = directory.join("corrupt-backup.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&corrupt_backup_path, "replacement", false, 2, &[Some(8)]);
    let mut corrupt_backup = fs::read(&corrupt_backup_path).unwrap();
    *corrupt_backup.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_backup_path, corrupt_backup).unwrap();
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (original Int64); \
             INSERT INTO Readings VALUES (1), (2);",
        )
        .unwrap();
    let metrics_before = query_rows(&mut database, "SELECT metric, value FROM system.metrics;");

    let error = database
        .replace_int64_table_from_file_with_backup(
            "readings",
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
        Int64TablePayloadFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
    ));
    assert_original_table(&mut database);
    assert_eq!(
        query_rows(&mut database, "SELECT metric, value FROM system.metrics;"),
        metrics_before
    );
}

#[test]
fn recovery_limit_failure_preserves_the_table_and_cached_metrics() {
    let directory = TestDirectory::new();
    let limited_backup_path = directory.join("limited-backup.snapshot");
    let (limited_snapshot_codec, limited_payload_codec) =
        write_snapshot(&limited_backup_path, "replacement", false, 4, &[Some(10)]);
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 3));
    database
        .execute(
            "CREATE TABLE Readings (original Int64); \
             INSERT INTO Readings VALUES (1), (2);",
        )
        .unwrap();
    let metrics_before = query_rows(&mut database, "SELECT metric, value FROM system.metrics;");

    assert!(matches!(
        database.replace_int64_table_from_file_with_backup(
            "READINGS",
            directory.join("missing-limit-primary.snapshot"),
            limited_backup_path,
            limited_snapshot_codec,
            limited_payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table row cap",
                actual: 4,
                max: 3,
            }
        ))
    ));
    assert_original_table(&mut database);
    assert_eq!(
        query_rows(&mut database, "SELECT metric, value FROM system.metrics;"),
        metrics_before
    );
}

#[test]
fn missing_target_is_rejected_before_the_snapshot_is_opened() {
    let directory = TestDirectory::new();
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Existing (value Int64); INSERT INTO Existing VALUES (7);")
        .unwrap();

    assert!(matches!(
        database.replace_int64_table_from_file(
            "Missing",
            directory.join("also-missing.snapshot"),
            SnapshotCodec::new(1),
            Int64TablePayloadCodec::new(1, 1, 1),
        ),
        Err(DatabaseSnapshotRestoreError::Table(Error::TableNotFound(ref name)))
            if name == "Missing"
    ));
    assert_eq!(
        query_rows(&mut database, "SELECT value FROM existing;"),
        vec![vec![Value::Int64(7)]]
    );
}

#[test]
fn corruption_and_invalid_schema_leave_the_old_table_and_metrics_unchanged() {
    let directory = TestDirectory::new();
    let valid_path = directory.join("valid.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot(&valid_path, "replacement", false, 2, &[Some(8)]);
    let corrupt_path = directory.join("corrupt.snapshot");
    let mut corrupt = fs::read(&valid_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();
    let invalid_path = directory.join("invalid-name.snapshot");
    let (invalid_snapshot_codec, invalid_payload_codec) =
        write_snapshot(&invalid_path, "invalid-name", false, 2, &[Some(10)]);

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (original Int64); \
             INSERT INTO Readings VALUES (1), (2);",
        )
        .unwrap();
    let metrics_before = query_rows(&mut database, "SELECT metric, value FROM system.metrics;");

    assert!(matches!(
        database.replace_int64_table_from_file(
            "readings",
            corrupt_path,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Snapshot(
            Int64TablePayloadFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
        ))
    ));
    assert_original_table(&mut database);

    assert!(matches!(
        database.replace_int64_table_from_file(
            "readings",
            invalid_path,
            invalid_snapshot_codec,
            invalid_payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(Error::InvalidIdentifier {
            ref identifier,
            ref context,
        })) if identifier == "invalid-name" && context == "column name"
    ));
    assert_original_table(&mut database);
    assert_eq!(
        query_rows(&mut database, "SELECT metric, value FROM system.metrics;",),
        metrics_before
    );
}

#[test]
fn exact_table_limits_succeed_and_limit_failures_preserve_the_current_table() {
    let directory = TestDirectory::new();
    let exact_path = directory.join("exact.snapshot");
    let (exact_snapshot_codec, exact_payload_codec) =
        write_snapshot(&exact_path, "value", false, 3, &[Some(1), Some(2)]);
    let over_row_cap_path = directory.join("over-row-cap.snapshot");
    let (over_row_snapshot_codec, over_row_payload_codec) =
        write_snapshot(&over_row_cap_path, "other", false, 4, &[Some(9)]);
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 2));
    database
        .execute("CREATE TABLE Target (old Int64);")
        .unwrap();

    database
        .replace_int64_table_from_file(
            "target",
            exact_path,
            exact_snapshot_codec,
            exact_payload_codec,
        )
        .unwrap();
    assert_eq!(
        database.catalog().table("target").unwrap().limits(),
        TableLimits::new(3, 1, 2)
    );
    assert!(matches!(
        database.replace_int64_table_from_file(
            "target",
            over_row_cap_path,
            over_row_snapshot_codec,
            over_row_payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table row cap",
                actual: 4,
                max: 3,
            }
        ))
    ));
    assert_eq!(
        query_rows(&mut database, "SELECT value FROM target ORDER BY value;"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );

    let cell_limited_path = directory.join("cell-limited.snapshot");
    let (cell_snapshot_codec, cell_payload_codec) = write_snapshot(
        &cell_limited_path,
        "replacement",
        false,
        3,
        &[Some(1), Some(2)],
    );
    let mut cell_limited = Database::with_table_limits(TableLimits::new(3, 1, 1));
    cell_limited
        .execute("CREATE TABLE CellTarget (old Int64);")
        .unwrap();
    assert!(matches!(
        cell_limited.replace_int64_table_from_file(
            "celltarget",
            cell_limited_path,
            cell_snapshot_codec,
            cell_payload_codec,
        ),
        Err(DatabaseSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: 2,
                max: 1,
            }
        ))
    ));
    let table = cell_limited.catalog().table("CellTarget").unwrap();
    assert_eq!(table.name(), "CellTarget");
    assert_eq!(table.schema()[0].name, "old");
    assert_eq!(table.row_count(), 0);
}
