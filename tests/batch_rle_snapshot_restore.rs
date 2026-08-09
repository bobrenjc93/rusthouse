#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::{QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
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
fn corruption_and_nullability_failures_preserve_catalog_and_cached_metrics() {
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
    assert!(matches!(
        database.restore_int64_table_rle_from_file(
            "nullable_schema",
            valid_path,
            Schema::int64("value", true),
            1,
            snapshot_codec,
            payload_codec,
        ),
        Err(DatabaseRleSnapshotRestoreError::NullableColumn { ref column })
            if column == "value"
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
