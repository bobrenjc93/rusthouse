#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::SNAPSHOT_HEADER_LEN;
use rusthouse::{
    Int64Table, Int64TableFileSaveError, NullableI64PayloadCodec, NullableI64PayloadError, Schema,
    SnapshotCodec, SnapshotError, SnapshotReplaceError, restore_int64_table_from_file,
    save_int64_table_to_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-save-tests");
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

#[test]
fn saves_and_reopens_an_empty_table_with_caller_supplied_schema() {
    let directory = TestDirectory::new();
    let path = directory.join("empty.snapshot");
    let payload_codec = NullableI64PayloadCodec::new(0, 8);
    let snapshot_codec = SnapshotCodec::new(8);
    let table = Int64Table::new(Schema::int64("source_name", false), 4);

    save_int64_table_to_file(&path, &table, snapshot_codec, payload_codec).unwrap();

    let reopened = restore_int64_table_from_file(
        &path,
        Schema::int64("reopened_name", true),
        7,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    assert!(reopened.is_empty());
    assert_eq!(reopened.schema(), &Schema::int64("reopened_name", true));
    assert_eq!(reopened.row_cap(), 7);
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + 8
    );
}

#[test]
fn saves_nullable_rows_and_reopens_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable.snapshot");
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let table = table(Schema::int64("reading", true), rows.len(), &rows);
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), 27);
    let snapshot_codec = SnapshotCodec::new(27);

    save_int64_table_to_file(&path, &table, snapshot_codec, payload_codec).unwrap();

    let reopened = restore_int64_table_from_file(
        &path,
        Schema::int64("reading", true),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    assert_eq!(reopened.values(), rows);
    assert_eq!(reopened.row_count(), reopened.row_cap());
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + 27
    );
}

#[test]
fn atomically_overwrites_an_existing_table_snapshot() {
    let directory = TestDirectory::new();
    let path = directory.join("overwrite.snapshot");
    let payload_codec = NullableI64PayloadCodec::new(2, 26);
    let snapshot_codec = SnapshotCodec::new(26);
    let old_table = table(Schema::int64("reading", false), 2, &[Some(11)]);
    let new_rows = [Some(22), Some(33)];
    let new_table = table(Schema::int64("reading", false), 2, &new_rows);

    save_int64_table_to_file(&path, &old_table, snapshot_codec, payload_codec).unwrap();
    let old_envelope = fs::read(&path).unwrap();
    save_int64_table_to_file(&path, &new_table, snapshot_codec, payload_codec).unwrap();

    assert_ne!(fs::read(&path).unwrap(), old_envelope);
    let reopened = restore_int64_table_from_file(
        path,
        Schema::int64("reading", false),
        2,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    assert_eq!(reopened.values(), new_rows);
}

#[test]
fn payload_encoding_failure_preserves_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let table = table(Schema::int64("reading", false), 2, &[Some(1), Some(2)]);

    let error = save_int64_table_to_file(
        &path,
        &table,
        SnapshotCodec::new(26),
        NullableI64PayloadCodec::new(1, 26),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileSaveError::Payload(NullableI64PayloadError::RowLimitExceeded {
            row_count: 2,
            max_rows: 1,
        })
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn envelope_encoding_failure_is_a_replacement_error_and_preserves_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let table = table(Schema::int64("reading", false), 1, &[Some(7)]);

    let error = save_int64_table_to_file(
        &path,
        &table,
        SnapshotCodec::new(16),
        NullableI64PayloadCodec::new(1, 17),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileSaveError::Replace(SnapshotReplaceError::Encode(
            SnapshotError::PayloadTooLarge {
                payload_len: 17,
                max_payload_len: 16,
            }
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn pre_rename_filesystem_failure_preserves_the_destination_and_cleans_up() {
    let directory = TestDirectory::new();
    let path = directory.join("destination");
    fs::create_dir(&path).unwrap();
    let marker = path.join("marker");
    fs::write(&marker, b"keep me").unwrap();
    let table = table(Schema::int64("reading", false), 1, &[Some(7)]);

    let error = save_int64_table_to_file(
        &path,
        &table,
        SnapshotCodec::new(17),
        NullableI64PayloadCodec::new(1, 17),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileSaveError::Replace(SnapshotReplaceError::Rename(_))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(marker).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}
