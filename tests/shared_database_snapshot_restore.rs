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
use rusthouse::batch::value::{DataType, Value};
use rusthouse::snapshot::INT64_TABLE_PAYLOAD_FIXED_LEN;
use rusthouse::{
    Database, DatabaseMetrics, DatabaseSnapshotRestoreEntry, DatabaseSnapshotRestoreError,
    DatabaseSnapshotSetRestoreError, Int64Table, Int64TablePayloadCodec,
    Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError, Schema, SharedDatabase,
    SharedDatabaseSnapshotRestoreError, SharedDatabaseSnapshotSetRestoreError, SnapshotCodec,
    SnapshotError, TableLimits,
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
    write_snapshot_with_nullability(path, column, false, row_cap, rows)
}

fn write_snapshot_with_nullability(
    path: &Path,
    column: &str,
    nullable: bool,
    row_cap: usize,
    rows: &[Option<i64>],
) -> (SnapshotCodec, Int64TablePayloadCodec) {
    let mut table = Int64Table::new(Schema::int64(column, nullable), row_cap);
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
fn restores_nullable_snapshot_with_name_null_order_cap_and_cached_metrics() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable.snapshot");
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let (snapshot_codec, payload_codec) =
        write_snapshot_with_nullability(&path, "Measurement", true, 4, &rows);
    let database = SharedDatabase::with_table_limits(TableLimits::new(4, 1, 4));

    database
        .try_restore_int64_table_from_file("Readings", path, snapshot_codec, payload_codec)
        .unwrap();

    let query = database.query("SELECT Measurement FROM readings;").unwrap();
    assert_eq!(query.columns[0].name, "Measurement");
    assert_eq!(
        query.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(i64::MAX)],
        ]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 1,
            retained_row_count: 3,
            retained_value_bytes: 27,
        })
    );

    database
        .execute("INSERT INTO readings VALUES (0);")
        .unwrap();
    assert!(matches!(
        database.execute("INSERT INTO readings VALUES (1);"),
        Err(rusthouse::SharedDatabaseError::Sql(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 5,
                max: 4,
            }
        ))
    ));
}

#[test]
fn recovery_replacement_prefers_the_primary_snapshot() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let (snapshot_codec, payload_codec) = write_snapshot(&primary_path, "reading", 2, &[Some(11)]);
    write_snapshot(&backup_path, "reading", 2, &[Some(22)]);
    let database = SharedDatabase::new(populated_database());

    let source = database
        .try_replace_int64_table_from_file_with_backup(
            "EXISTING",
            primary_path,
            backup_path,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(source, Int64TablePayloadFileRecoverySource::Primary);
    assert_eq!(
        database
            .query("SELECT reading FROM existing;")
            .unwrap()
            .rows,
        [[Value::Int64(11)]]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 1,
            retained_row_count: 1,
            retained_value_bytes: 8,
        })
    );
}

#[test]
fn recovery_replacement_uses_the_backup_after_primary_corruption() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let (snapshot_codec, payload_codec) = write_snapshot(&primary_path, "reading", 2, &[Some(11)]);
    write_snapshot(&backup_path, "reading", 2, &[Some(22)]);
    let mut corrupt = fs::read(&primary_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&primary_path, corrupt).unwrap();
    let database = SharedDatabase::new(populated_database());

    let source = database
        .try_replace_int64_table_from_file_with_backup(
            "existing",
            primary_path,
            backup_path,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(source, Int64TablePayloadFileRecoverySource::Backup);
    assert_eq!(
        database
            .query("SELECT reading FROM existing;")
            .unwrap()
            .rows,
        [[Value::Int64(22)]]
    );
}

#[test]
fn dual_recovery_failure_preserves_the_target_and_cached_metrics() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("missing-primary.snapshot");
    let backup_path = directory.join("corrupt-backup.snapshot");
    let (snapshot_codec, payload_codec) = write_snapshot(&backup_path, "reading", 2, &[Some(22)]);
    let mut corrupt = fs::read(&backup_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&backup_path, corrupt).unwrap();
    let database = SharedDatabase::new(populated_database());
    let tables_before = database.query("SHOW TABLES;").unwrap();
    let rows_before = database.query("SELECT id FROM existing;").unwrap();
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_replace_int64_table_from_file_with_backup(
            "existing",
            primary_path,
            backup_path,
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    let SharedDatabaseSnapshotRestoreError::Snapshot(DatabaseSnapshotRestoreError::Recovery(
        recovery,
    )) = error
    else {
        panic!("expected typed dual recovery failure, got {error:?}");
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
    assert_eq!(database.query("SHOW TABLES;").unwrap(), tables_before);
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap(),
        rows_before
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn recovery_replacement_contention_is_nonblocking_and_precedes_file_access() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("missing-primary.snapshot");
    let backup_path = directory.join("missing-backup.snapshot");
    let (snapshot_codec, payload_codec) = codecs("reading", 1, &[Some(1)]);
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let metrics_before = database.metrics_snapshot();

    let reader = inner.read().unwrap();
    let reader_database = database.clone();
    let reader_primary_path = primary_path.clone();
    let reader_backup_path = backup_path.clone();
    let (reader_sender, reader_receiver) = mpsc::channel();
    let reader_worker = thread::spawn(move || {
        reader_sender
            .send(
                reader_database.try_replace_int64_table_from_file_with_backup(
                    "existing",
                    reader_primary_path,
                    reader_backup_path,
                    snapshot_codec,
                    payload_codec,
                ),
            )
            .unwrap();
    });
    let reader_error = reader_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("replacement lock acquisition must not wait for an existing reader")
        .unwrap_err();
    assert!(matches!(
        reader_error,
        SharedDatabaseSnapshotRestoreError::DatabaseBusy
    ));
    assert_eq!(reader.catalog().table("existing").unwrap().row_count(), 1);
    drop(reader);
    reader_worker.join().unwrap();
    assert_eq!(database.metrics_snapshot(), metrics_before);

    let writer = inner.write().unwrap();
    let writer_database = database.clone();
    let writer_primary_path = primary_path.clone();
    let writer_backup_path = backup_path.clone();
    let (writer_sender, writer_receiver) = mpsc::channel();
    let writer_worker = thread::spawn(move || {
        writer_sender
            .send(
                writer_database.try_replace_int64_table_from_file_with_backup(
                    "existing",
                    writer_primary_path,
                    writer_backup_path,
                    snapshot_codec,
                    payload_codec,
                ),
            )
            .unwrap();
    });
    let writer_error = writer_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("replacement lock acquisition must not wait for an existing writer")
        .unwrap_err();
    assert!(matches!(
        writer_error,
        SharedDatabaseSnapshotRestoreError::DatabaseBusy
    ));
    assert_eq!(writer.catalog().table("existing").unwrap().row_count(), 1);
    drop(writer);
    writer_worker.join().unwrap();

    assert!(!primary_path.exists());
    assert!(!backup_path.exists());
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn recovery_replacement_poisoning_is_distinct_and_precedes_file_access() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("missing-primary.snapshot");
    let backup_path = directory.join("missing-backup.snapshot");
    let (snapshot_codec, payload_codec) = codecs("reading", 1, &[Some(1)]);
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let rows_before = database.query("SELECT id FROM existing;").unwrap();
    let metrics_before = database.metrics_snapshot();
    let poisoner = thread::spawn({
        let inner = Arc::clone(&inner);
        move || {
            let _guard = inner.write().unwrap();
            panic!("poison the database lock");
        }
    });
    assert!(poisoner.join().is_err());

    let error = database
        .try_replace_int64_table_from_file_with_backup(
            "existing",
            &primary_path,
            &backup_path,
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotRestoreError::LockPoisoned
    ));
    assert!(!primary_path.exists());
    assert!(!backup_path.exists());
    let poisoned_guard = inner.read().unwrap_err().into_inner();
    assert_eq!(
        poisoned_guard
            .catalog()
            .table("existing")
            .unwrap()
            .row_count(),
        1
    );
    drop(poisoned_guard);
    inner.clear_poison();
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap(),
        rows_before
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
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

#[test]
fn restores_mixed_nullable_snapshot_set_at_the_exact_count_limit() {
    let directory = TestDirectory::new();
    let temperatures_path = directory.join("temperatures.snapshot");
    let pressures_path = directory.join("pressures.snapshot");
    let (temperatures_snapshot_codec, temperatures_payload_codec) =
        write_snapshot(&temperatures_path, "temperature", 2, &[Some(-4), Some(12)]);
    let (pressures_snapshot_codec, pressures_payload_codec) =
        write_snapshot_with_nullability(&pressures_path, "pressure", true, 2, &[None, Some(1013)]);
    let entries = [
        DatabaseSnapshotRestoreEntry::new(
            "Temperatures",
            &temperatures_path,
            temperatures_snapshot_codec,
            temperatures_payload_codec,
        ),
        DatabaseSnapshotRestoreEntry::new(
            "Pressures",
            &pressures_path,
            pressures_snapshot_codec,
            pressures_payload_codec,
        ),
    ];
    let database = SharedDatabase::with_table_limits(TableLimits::new(2, 1, 2));

    database
        .try_restore_int64_tables_from_files(&entries, entries.len())
        .unwrap();

    assert_eq!(
        database
            .query("SELECT temperature FROM temperatures ORDER BY temperature;")
            .unwrap()
            .rows,
        [[Value::Int64(-4)], [Value::Int64(12)]]
    );
    assert_eq!(
        database
            .query("SELECT pressure FROM PRESSURES;")
            .unwrap()
            .rows,
        [[Value::Null(DataType::Int64)], [Value::Int64(1013)],]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 2,
            column_count: 2,
            retained_row_count: 4,
            retained_value_bytes: 34,
        })
    );
}

#[test]
fn later_set_file_failure_rolls_back_catalog_data_and_cached_metrics() {
    let directory = TestDirectory::new();
    let valid_path = directory.join("valid.snapshot");
    let corrupt_path = directory.join("corrupt.snapshot");
    let (snapshot_codec, payload_codec) =
        write_snapshot_with_nullability(&valid_path, "value", true, 2, &[None, Some(8)]);
    let mut corrupt = fs::read(&valid_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();
    let entries = [
        DatabaseSnapshotRestoreEntry::new("staged", &valid_path, snapshot_codec, payload_codec),
        DatabaseSnapshotRestoreEntry::new("broken", &corrupt_path, snapshot_codec, payload_codec),
    ];
    let database = SharedDatabase::new(populated_database());
    let tables_before = database.query("SHOW TABLES;").unwrap();
    let rows_before = database.query("SELECT id FROM existing;").unwrap();
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_restore_int64_tables_from_files(&entries, entries.len())
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::Snapshot(
            DatabaseSnapshotSetRestoreError::Entry {
                entry_index: 1,
                ref table_name,
                error: DatabaseSnapshotRestoreError::Snapshot(
                    Int64TablePayloadFileRestoreError::Envelope(
                        SnapshotError::ChecksumMismatch { .. }
                    )
                ),
            }
        ) if table_name == "broken"
    ));
    assert_eq!(database.query("SHOW TABLES;").unwrap(), tables_before);
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap(),
        rows_before
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn set_count_rejection_precedes_source_access_and_preserves_state() {
    let directory = TestDirectory::new();
    let first_path = directory.join("missing-first.snapshot");
    let second_path = directory.join("missing-second.snapshot");
    let snapshot_codec = SnapshotCodec::new(1);
    let payload_codec = Int64TablePayloadCodec::new(1, 1, 1);
    let entries = [
        DatabaseSnapshotRestoreEntry::new("first", &first_path, snapshot_codec, payload_codec),
        DatabaseSnapshotRestoreEntry::new("second", &second_path, snapshot_codec, payload_codec),
    ];
    let database = SharedDatabase::new(populated_database());
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_restore_int64_tables_from_files(&entries, 1)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::Snapshot(
            DatabaseSnapshotSetRestoreError::EntryLimitExceeded {
                entry_index: 1,
                ref table_name,
                entries: 2,
                max_entries: 1,
            }
        ) if table_name == "second"
    ));
    assert!(!first_path.exists());
    assert!(!second_path.exists());
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap().rows,
        [[Value::Int64(7)]]
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn set_name_rejection_precedes_source_access_and_preserves_state() {
    let directory = TestDirectory::new();
    let first_path = directory.join("missing-first.snapshot");
    let second_path = directory.join("missing-second.snapshot");
    let snapshot_codec = SnapshotCodec::new(1);
    let payload_codec = Int64TablePayloadCodec::new(1, 1, 1);
    let entries = [
        DatabaseSnapshotRestoreEntry::new("Readings", &first_path, snapshot_codec, payload_codec),
        DatabaseSnapshotRestoreEntry::new("READINGS", &second_path, snapshot_codec, payload_codec),
    ];
    let database = SharedDatabase::new(populated_database());
    let metrics_before = database.metrics_snapshot();

    let error = database
        .try_restore_int64_tables_from_files(&entries, entries.len())
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::Snapshot(
            DatabaseSnapshotSetRestoreError::Entry {
                entry_index: 1,
                ref table_name,
                error: DatabaseSnapshotRestoreError::Table(Error::TableAlreadyExists(ref name)),
            }
        ) if table_name == "READINGS" && name == "READINGS"
    ));
    assert!(!first_path.exists());
    assert!(!second_path.exists());
    assert_eq!(
        database.query("SELECT id FROM existing;").unwrap().rows,
        [[Value::Int64(7)]]
    );
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn set_restore_contention_is_nonblocking_and_precedes_source_access() {
    let directory = TestDirectory::new();
    let missing_path = directory.join("missing.snapshot");
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let metrics_before = database.metrics_snapshot();
    let reader = inner.read().unwrap();
    let worker_database = database.clone();
    let worker_path = missing_path.clone();
    let (sender, receiver) = mpsc::channel();

    let worker = thread::spawn(move || {
        let entry = DatabaseSnapshotRestoreEntry::new(
            "restored",
            &worker_path,
            SnapshotCodec::new(1),
            Int64TablePayloadCodec::new(1, 1, 1),
        );
        sender
            .send(worker_database.try_restore_int64_tables_from_files(&[entry], 1))
            .unwrap();
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("snapshot set lock acquisition must not wait for an existing reader")
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::DatabaseBusy
    ));
    assert!(!missing_path.exists());
    assert_eq!(reader.catalog().table_count(), 1);
    assert_eq!(reader.catalog().retained_row_count(), 1);
    assert_eq!(reader.catalog().retained_value_bytes(), 8);
    drop(reader);
    worker.join().unwrap();

    let writer = inner.write().unwrap();
    let worker_database = database.clone();
    let worker_path = missing_path.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let entry = DatabaseSnapshotRestoreEntry::new(
            "restored",
            &worker_path,
            SnapshotCodec::new(1),
            Int64TablePayloadCodec::new(1, 1, 1),
        );
        sender
            .send(worker_database.try_restore_int64_tables_from_files(&[entry], 1))
            .unwrap();
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("snapshot set lock acquisition must not wait for an existing writer")
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::DatabaseBusy
    ));
    assert!(!missing_path.exists());
    assert_eq!(writer.catalog().table_count(), 1);
    assert_eq!(writer.catalog().retained_row_count(), 1);
    assert_eq!(writer.catalog().retained_value_bytes(), 8);
    drop(writer);
    worker.join().unwrap();
    assert_eq!(database.metrics_snapshot(), metrics_before);
}

#[test]
fn set_restore_poisoning_is_distinct_and_precedes_source_access() {
    let directory = TestDirectory::new();
    let missing_path = directory.join("missing.snapshot");
    let entry = DatabaseSnapshotRestoreEntry::new(
        "restored",
        &missing_path,
        SnapshotCodec::new(1),
        Int64TablePayloadCodec::new(1, 1, 1),
    );
    let inner = Arc::new(RwLock::new(populated_database()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let metrics_before = database.metrics_snapshot();
    let poisoner = thread::spawn({
        let inner = Arc::clone(&inner);
        move || {
            let _guard = inner.write().unwrap();
            panic!("poison the database lock");
        }
    });
    assert!(poisoner.join().is_err());

    let error = database
        .try_restore_int64_tables_from_files(&[entry], 1)
        .unwrap_err();

    assert!(matches!(
        error,
        SharedDatabaseSnapshotSetRestoreError::LockPoisoned
    ));
    assert!(!missing_path.exists());
    let poisoned_guard = inner.read().unwrap_err().into_inner();
    assert_eq!(poisoned_guard.catalog().table_count(), 1);
    assert_eq!(poisoned_guard.catalog().retained_row_count(), 1);
    assert_eq!(poisoned_guard.catalog().retained_value_bytes(), 8);
    drop(poisoned_guard);
    inner.clear_poison();
    assert_eq!(database.metrics_snapshot(), metrics_before);
}
