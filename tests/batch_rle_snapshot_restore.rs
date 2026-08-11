#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::{QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::snapshot::{NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Database, DatabaseRleSnapshotRestoreError, InsertError, Int64Table,
    Int64TableRleFileRestoreError, NullableI64RlePayloadCodec, Schema, SnapshotCodec,
    SnapshotError, TableLimits, save_int64_table_rle_to_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-rle-restore-tests");
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

fn table(schema: Schema, row_cap: usize, rows: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(schema, row_cap);
    table.append_batch(rows).unwrap();
    table
}

fn cached_metrics(database: &mut Database) -> QueryResult {
    let results = database
        .execute("SELECT metric, value FROM system.metrics")
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("system.metrics must return one query result")
    };
    result.clone()
}

#[test]
fn legacy_nullable_column_error_variant_remains_source_compatible() {
    let error = DatabaseRleSnapshotRestoreError::NullableColumn {
        column: "reading".to_owned(),
    };

    assert!(matches!(
        error,
        DatabaseRleSnapshotRestoreError::NullableColumn { ref column }
            if column == "reading"
    ));
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn existing_rle_saver_imports_to_select_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [Some(7), Some(7), Some(-2)];
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 17 + 17;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 2, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    let source = table(Schema::int64("reading", false), rows.len(), &rows);
    save_int64_table_rle_to_file(&path, &source, snapshot_codec, payload_codec).unwrap();
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 3));

    database
        .restore_int64_table_rle_from_file(
            "Readings",
            &path,
            Schema::int64("reading", false),
            rows.len(),
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
    let restored = database.catalog().table("readings").unwrap();
    assert_eq!(restored.limits(), TableLimits::new(3, 1, 3));
    let results = database
        .execute("SELECT reading FROM READINGS ORDER BY reading")
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("SELECT must return one query result")
    };
    assert_eq!(
        result.rows,
        [
            vec![Value::Int64(-2)],
            vec![Value::Int64(7)],
            vec![Value::Int64(7)],
        ]
    );
    assert_eq!(
        cached_metrics(&mut database).rows,
        [
            vec![
                Value::String("rusthouse_tables".to_owned()),
                Value::Int64(1)
            ],
            vec![
                Value::String("rusthouse_columns".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::String("rusthouse_retained_rows".to_owned()),
                Value::Int64(3),
            ],
            vec![
                Value::String("rusthouse_retained_value_bytes".to_owned()),
                Value::Int64(24),
            ],
            vec![
                Value::String("rusthouse_index_scanned_blocks".to_owned()),
                Value::Int64(0),
            ],
            vec![
                Value::String("rusthouse_index_pruned_blocks".to_owned()),
                Value::Int64(0),
            ],
        ]
    );
    assert!(matches!(
        database.execute("INSERT INTO readings VALUES (9)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 4,
            max: 3,
        })
    ));
}

#[test]
fn nullable_rle_imports_preserve_empty_and_all_null_storage_and_row_caps() {
    let directory = TestDirectory::new();
    let empty_path = directory.join("empty.snapshot");
    let empty_payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN;
    let empty_payload_codec = NullableI64RlePayloadCodec::new(0, 0, empty_payload_len);
    let empty_snapshot_codec = SnapshotCodec::new(empty_payload_len);
    save_int64_table_rle_to_file(
        &empty_path,
        &table(Schema::int64("reading", true), 2, &[]),
        empty_snapshot_codec,
        empty_payload_codec,
    )
    .unwrap();

    let all_null_path = directory.join("all-null.snapshot");
    let all_null_rows = [None, None, None];
    let all_null_payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9;
    let all_null_payload_codec =
        NullableI64RlePayloadCodec::new(all_null_rows.len(), 1, all_null_payload_len);
    let all_null_snapshot_codec = SnapshotCodec::new(all_null_payload_len);
    save_int64_table_rle_to_file(
        &all_null_path,
        &table(
            Schema::int64("reading", true),
            all_null_rows.len() + 1,
            &all_null_rows,
        ),
        all_null_snapshot_codec,
        all_null_payload_codec,
    )
    .unwrap();

    let mut database = Database::with_table_limits(TableLimits::new(4, 1, 4));
    database
        .restore_int64_table_rle_from_file(
            "empty_readings",
            empty_path,
            Schema::int64("reading", true),
            2,
            empty_snapshot_codec,
            empty_payload_codec,
        )
        .unwrap();
    database
        .restore_int64_table_rle_from_file(
            "all_null_readings",
            all_null_path,
            Schema::int64("reading", true),
            all_null_rows.len() + 1,
            all_null_snapshot_codec,
            all_null_payload_codec,
        )
        .unwrap();

    let empty = database.catalog().table("empty_readings").unwrap();
    assert_eq!(empty.row_cap(), 2);
    assert!(matches!(
        empty.columns(),
        [Column::NullableInt64(values)] if values.is_empty()
    ));
    let all_null = database.catalog().table("all_null_readings").unwrap();
    assert_eq!(all_null.row_cap(), all_null_rows.len() + 1);
    assert!(matches!(
        all_null.columns(),
        [Column::NullableInt64(values)] if values == &all_null_rows
    ));
    assert_eq!(
        cached_metrics(&mut database).rows,
        [
            vec![
                Value::String("rusthouse_tables".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::String("rusthouse_columns".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::String("rusthouse_retained_rows".to_owned()),
                Value::Int64(3),
            ],
            vec![
                Value::String("rusthouse_retained_value_bytes".to_owned()),
                Value::Int64(27),
            ],
            vec![
                Value::String("rusthouse_index_scanned_blocks".to_owned()),
                Value::Int64(0),
            ],
            vec![
                Value::String("rusthouse_index_pruned_blocks".to_owned()),
                Value::Int64(0),
            ],
        ]
    );
}

#[test]
fn nullable_mixed_runs_import_at_every_exact_limit_with_null_positions() {
    let directory = TestDirectory::new();
    let path = directory.join("mixed.snapshot");
    let rows = [None, None, Some(7), Some(7), None, Some(-2)];
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17 + 9 + 17;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 4, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    save_int64_table_rle_to_file(
        &path,
        &table(Schema::int64("reading", true), rows.len(), &rows),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    let mut database = Database::with_table_limits(TableLimits::new(rows.len(), 1, rows.len()));

    database
        .restore_int64_table_rle_from_file(
            "mixed_readings",
            &path,
            Schema::int64("reading", true),
            rows.len(),
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
    let restored = database.catalog().table("MIXED_READINGS").unwrap();
    assert_eq!(
        restored.limits(),
        TableLimits::new(rows.len(), 1, rows.len())
    );
    assert!(matches!(
        restored.columns(),
        [Column::NullableInt64(values)] if values == &rows
    ));
    let results = database
        .execute("SELECT reading FROM mixed_readings")
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("SELECT must return one query result")
    };
    assert_eq!(
        result.rows,
        [
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(7)],
            vec![Value::Int64(7)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(-2)],
        ]
    );
    assert_eq!(
        cached_metrics(&mut database).rows[2..4],
        [
            vec![
                Value::String("rusthouse_retained_rows".to_owned()),
                Value::Int64(6),
            ],
            vec![
                Value::String("rusthouse_retained_value_bytes".to_owned()),
                Value::Int64(54),
            ],
        ]
    );
    assert!(matches!(
        database.execute("INSERT INTO mixed_readings VALUES (9)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 7,
            max: 6,
        })
    ));
}

#[test]
fn corruption_and_non_nullable_schema_mismatch_preserve_catalog_and_cached_metrics() {
    let directory = TestDirectory::new();
    let valid_path = directory.join("valid.snapshot");
    let corrupt_path = directory.join("corrupt.snapshot");
    let nullable_path = directory.join("nullable.snapshot");
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 17;
    let payload_codec = NullableI64RlePayloadCodec::new(1, 1, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    save_int64_table_rle_to_file(
        &valid_path,
        &table(Schema::int64("value", false), 1, &[Some(8)]),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    let mut corrupt = fs::read(&valid_path).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();
    let nullable_payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9;
    let nullable_payload_codec = NullableI64RlePayloadCodec::new(1, 1, nullable_payload_len);
    let nullable_snapshot_codec = SnapshotCodec::new(nullable_payload_len);
    save_int64_table_rle_to_file(
        &nullable_path,
        &table(Schema::int64("value", true), 1, &[None]),
        nullable_snapshot_codec,
        nullable_payload_codec,
    )
    .unwrap();

    let mut database = Database::new();
    database
        .execute("CREATE TABLE existing (id Int64); INSERT INTO existing VALUES (5)")
        .unwrap();
    let metrics_before = cached_metrics(&mut database);

    assert!(matches!(
        database.restore_int64_table_rle_from_file(
            "corrupt",
            corrupt_path,
            Schema::int64("value", false),
            1,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseRleSnapshotRestoreError::Snapshot(
            Int64TableRleFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
        ))
    ));
    assert!(matches!(
        database.restore_int64_table_rle_from_file(
            "nullable",
            nullable_path,
            Schema::int64("value", false),
            1,
            nullable_snapshot_codec,
            nullable_payload_codec,
        ),
        Err(DatabaseRleSnapshotRestoreError::Snapshot(
            Int64TableRleFileRestoreError::Table(InsertError::NullNotAllowed { ref column })
        )) if column == "value"
    ));
    assert_eq!(database.catalog().table_count(), 1);
    assert!(database.catalog().table_exists("existing"));
    assert_eq!(cached_metrics(&mut database), metrics_before);
}

#[test]
fn duplicate_and_table_limit_failures_do_not_access_or_change_catalog_state() {
    let directory = TestDirectory::new();
    let path = directory.join("limited.snapshot");
    let rows = [Some(1), Some(1)];
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 17;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 1, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    save_int64_table_rle_to_file(
        &path,
        &table(Schema::int64("value", false), 3, &rows),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    let mut database = Database::with_table_limits(TableLimits::new(2, 1, 2));
    database
        .execute("CREATE TABLE Existing (id Int64); INSERT INTO Existing VALUES (5)")
        .unwrap();
    let metrics_before = cached_metrics(&mut database);

    assert!(matches!(
        database.restore_int64_table_rle_from_file(
            "EXISTING",
            directory.join("missing.snapshot"),
            Schema::int64("value", false),
            1,
            SnapshotCodec::new(1),
            NullableI64RlePayloadCodec::new(1, 1, 1),
        ),
        Err(DatabaseRleSnapshotRestoreError::Table(
            Error::TableAlreadyExists(ref name)
        )) if name == "EXISTING"
    ));
    assert!(matches!(
        database.restore_int64_table_rle_from_file(
            "limited",
            path,
            Schema::int64("value", false),
            3,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseRleSnapshotRestoreError::Table(
            Error::ResourceLimitExceeded {
                resource: "table row cap",
                actual: 3,
                max: 2,
            }
        ))
    ));
    assert_eq!(database.catalog().table_count(), 1);
    assert!(database.catalog().table_exists("existing"));
    assert!(!database.catalog().table_exists("limited"));
    assert_eq!(cached_metrics(&mut database), metrics_before);
}
