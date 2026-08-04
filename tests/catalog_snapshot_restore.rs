use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{
    Catalog, CatalogLimits, CatalogSnapshotRestoreError, InsertError, Int64TableFileRestoreError,
    Int64TableRestoreError, NullableI64PayloadCodec, ParseLimits, Schema, SnapshotCodec,
    SnapshotError,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/catalog-snapshot-tests");
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

fn create_snapshot(path: &Path, rows: &[Option<i64>]) -> (SnapshotCodec, NullableI64PayloadCodec) {
    let max_payload_len = 8 + rows
        .iter()
        .map(|value| 1 + 8 * value.is_some() as usize)
        .sum::<usize>();
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), max_payload_len);
    let payload = payload_codec.encode(rows).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    snapshot_codec.create_new_file(path, &payload).unwrap();
    (snapshot_codec, payload_codec)
}

#[test]
fn reopens_a_snapshot_into_the_catalog_and_selects_at_exact_limits() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let (snapshot_codec, payload_codec) = create_snapshot(&path, &rows);
    let mut catalog = Catalog::new(CatalogLimits::new(1, rows.len()));

    catalog
        .restore_int64_table_from_file(
            "readings",
            path,
            Schema::int64("reading", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap();

    assert_eq!(catalog.len(), catalog.limits().max_tables);
    assert_eq!(
        catalog.table("readings").unwrap().row_count(),
        catalog.limits().max_rows_per_table
    );
    assert_eq!(
        catalog
            .execute_select("SELECT reading FROM readings", ParseLimits::default())
            .unwrap()
            .as_ref(),
        rows
    );
}

#[test]
fn missing_and_corrupt_files_preserve_the_catalog() {
    let directory = TestDirectory::new();
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(2, 1));
    catalog
        .execute_create("CREATE TABLE existing (value Int64 NULL)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO existing VALUES (5)", parse_limits)
        .unwrap();
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    let snapshot_codec = SnapshotCodec::new(17);

    let missing_error = catalog
        .restore_int64_table_from_file(
            "missing",
            directory.join("missing.snapshot"),
            Schema::int64("value", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    assert!(matches!(
        missing_error,
        CatalogSnapshotRestoreError::Snapshot(Int64TableFileRestoreError::Open(ref source))
            if source.kind() == ErrorKind::NotFound
    ));
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table("existing").unwrap().values(), &[Some(5)]);
    assert!(catalog.table("missing").is_none());

    let corrupt_path = directory.join("corrupt.snapshot");
    let payload = payload_codec.encode(&[Some(7)]).unwrap();
    let mut envelope = snapshot_codec.encode(&payload).unwrap();
    *envelope.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, envelope).unwrap();

    let corrupt_error = catalog
        .restore_int64_table_from_file(
            "corrupt",
            corrupt_path,
            Schema::int64("value", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    assert!(matches!(
        corrupt_error,
        CatalogSnapshotRestoreError::Snapshot(Int64TableFileRestoreError::Restore(
            Int64TableRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
        ))
    ));
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table("existing").unwrap().values(), &[Some(5)]);
    assert!(catalog.table("corrupt").is_none());
}

#[test]
fn schema_and_row_cap_failures_do_not_register_a_table() {
    let directory = TestDirectory::new();
    let nullable_path = directory.join("nullable.snapshot");
    let (nullable_snapshot_codec, nullable_payload_codec) =
        create_snapshot(&nullable_path, &[None]);
    let mut catalog = Catalog::new(CatalogLimits::new(1, 2));

    let schema_error = catalog
        .restore_int64_table_from_file(
            "readings",
            nullable_path,
            Schema::int64("reading", false),
            nullable_snapshot_codec,
            nullable_payload_codec,
        )
        .unwrap_err();

    assert!(matches!(
        schema_error,
        CatalogSnapshotRestoreError::Snapshot(Int64TableFileRestoreError::Restore(
            Int64TableRestoreError::Table(InsertError::NullNotAllowed { ref column })
        )) if column == "reading"
    ));
    assert!(catalog.is_empty());

    let oversized_path = directory.join("too-many-rows.snapshot");
    let rows = [Some(1), Some(2), Some(3)];
    let (snapshot_codec, payload_codec) = create_snapshot(&oversized_path, &rows);
    let row_cap_error = catalog
        .restore_int64_table_from_file(
            "readings",
            oversized_path,
            Schema::int64("reading", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();

    assert!(matches!(
        row_cap_error,
        CatalogSnapshotRestoreError::Snapshot(Int64TableFileRestoreError::Restore(
            Int64TableRestoreError::Table(InsertError::RowCapExceeded {
                row_cap: 2,
                current_rows: 0,
                incoming_rows: 3,
            })
        ))
    ));
    assert!(catalog.is_empty());
}

#[test]
fn duplicate_name_and_table_limit_failures_preserve_registered_tables() {
    let directory = TestDirectory::new();
    let path = directory.join("replacement.snapshot");
    let (snapshot_codec, payload_codec) = create_snapshot(&path, &[Some(99)]);
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 1));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (7)", parse_limits)
        .unwrap();

    let duplicate_error = catalog
        .restore_int64_table_from_file(
            "readings",
            &path,
            Schema::int64("value", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();
    assert!(matches!(
        duplicate_error,
        CatalogSnapshotRestoreError::TableAlreadyExists { ref name } if name == "readings"
    ));

    let table_limit_error = catalog
        .restore_int64_table_from_file(
            "other",
            path,
            Schema::int64("value", true),
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();
    assert!(matches!(
        table_limit_error,
        CatalogSnapshotRestoreError::TableLimitExceeded {
            tables: 2,
            max_tables: 1,
        }
    ));

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(7)]);
    assert!(catalog.table("other").is_none());
}
