#![cfg(unix)]

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{INT64_TABLE_PAYLOAD_FIXED_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Int64Table, Int64TablePayloadCodec, Int64TablePayloadError, Int64TablePayloadFileSaveError,
    Schema, SnapshotCodec, SnapshotError, SnapshotReplaceError,
    restore_int64_table_payload_from_file, save_int64_table_payload_to_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/table-payload-save-tests");
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

fn table(name: &str, nullable: bool, row_cap: usize, rows: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64(name, nullable), row_cap);
    table.append_batch(rows).unwrap();
    table
}

#[test]
fn saves_and_restores_nullable_table_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable.snapshot");
    let name = "métric";
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let source = table(name, true, rows.len(), &rows);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 19;
    let payload_codec = Int64TablePayloadCodec::new(name.len(), rows.len(), payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);

    save_int64_table_payload_to_file(&path, &source, snapshot_codec, payload_codec).unwrap();

    let reopened =
        restore_int64_table_payload_from_file(&path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(reopened, source);
    assert_eq!(reopened.schema(), &Schema::int64(name, true));
    assert_eq!(reopened.row_cap(), rows.len());
    assert_eq!(reopened.values(), rows);
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
}

#[test]
fn saves_and_restores_non_nullable_table_with_unused_capacity() {
    let directory = TestDirectory::new();
    let path = directory.join("non-nullable.snapshot");
    let name = "reading";
    let rows = [Some(-7), Some(11)];
    let source = table(name, false, 5, &rows);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 18;
    let payload_codec = Int64TablePayloadCodec::new(name.len(), 5, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);

    save_int64_table_payload_to_file(&path, &source, snapshot_codec, payload_codec).unwrap();

    let reopened =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(reopened, source);
    assert_eq!(reopened.schema(), &Schema::int64(name, false));
    assert_eq!(reopened.row_cap(), 5);
    assert_eq!(reopened.values(), rows);
}

#[test]
fn atomically_overwrites_an_existing_self_describing_snapshot() {
    let directory = TestDirectory::new();
    let path = directory.join("overwrite.snapshot");
    let payload_codec = Int64TablePayloadCodec::new(8, 5, 128);
    let snapshot_codec = SnapshotCodec::new(128);
    let old = table("old", true, 1, &[None]);
    let replacement = table("reading", false, 5, &[Some(-7), Some(11)]);

    save_int64_table_payload_to_file(&path, &old, snapshot_codec, payload_codec).unwrap();
    let old_envelope = fs::read(&path).unwrap();
    save_int64_table_payload_to_file(&path, &replacement, snapshot_codec, payload_codec).unwrap();

    assert_ne!(fs::read(&path).unwrap(), old_envelope);
    let reopened =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(reopened, replacement);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn one_below_each_exact_limit_is_typed_and_preserves_the_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    let name = "métric";
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let source = table(name, true, rows.len(), &rows);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 19;

    for expected in ["name", "row", "payload", "envelope"] {
        fs::write(&path, original).unwrap();
        let payload_codec = match expected {
            "name" => Int64TablePayloadCodec::new(name.len() - 1, rows.len(), payload_len),
            "row" => Int64TablePayloadCodec::new(name.len(), rows.len() - 1, payload_len),
            "payload" => Int64TablePayloadCodec::new(name.len(), rows.len(), payload_len - 1),
            "envelope" => Int64TablePayloadCodec::new(name.len(), rows.len(), payload_len),
            _ => unreachable!(),
        };
        let snapshot_codec = SnapshotCodec::new(if expected == "envelope" {
            payload_len - 1
        } else {
            payload_len
        });

        let error = save_int64_table_payload_to_file(&path, &source, snapshot_codec, payload_codec)
            .unwrap_err();

        assert!(!error.destination_was_replaced());
        match (expected, error) {
            (
                "name",
                Int64TablePayloadFileSaveError::Payload(Int64TablePayloadError::NameTooLong {
                    name_len,
                    max_name_len,
                }),
            ) => {
                assert_eq!(name_len, name.len() as u64);
                assert_eq!(max_name_len, name.len() - 1);
            }
            (
                "row",
                Int64TablePayloadFileSaveError::Payload(
                    Int64TablePayloadError::RowCapLimitExceeded { row_cap, max_rows },
                ),
            ) => {
                assert_eq!(row_cap, rows.len() as u64);
                assert_eq!(max_rows, rows.len() - 1);
            }
            (
                "payload",
                Int64TablePayloadFileSaveError::Payload(Int64TablePayloadError::PayloadTooLarge {
                    payload_len: actual,
                    max_payload_len,
                }),
            ) => {
                assert_eq!(actual, payload_len as u64);
                assert_eq!(max_payload_len, payload_len - 1);
            }
            (
                "envelope",
                Int64TablePayloadFileSaveError::Replace(SnapshotReplaceError::Encode(
                    SnapshotError::PayloadTooLarge {
                        payload_len: actual,
                        max_payload_len,
                    },
                )),
            ) => {
                assert_eq!(actual, payload_len as u64);
                assert_eq!(max_payload_len, payload_len - 1);
            }
            (_, error) => panic!("unexpected {expected} limit error: {error:?}"),
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
}

#[test]
fn pre_rename_filesystem_failure_preserves_the_destination_and_cleans_up() {
    let directory = TestDirectory::new();
    let path = directory.join("destination");
    fs::create_dir(&path).unwrap();
    let marker = path.join("marker");
    fs::write(&marker, b"keep me").unwrap();
    let source = table("reading", false, 1, &[Some(7)]);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + "reading".len() + 9;

    let error = save_int64_table_payload_to_file(
        &path,
        &source,
        SnapshotCodec::new(payload_len),
        Int64TablePayloadCodec::new("reading".len(), 1, payload_len),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TablePayloadFileSaveError::Replace(SnapshotReplaceError::Rename(_))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(marker).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn reports_post_rename_directory_sync_uncertainty() {
    let error = Int64TablePayloadFileSaveError::Replace(SnapshotReplaceError::SyncDirectory(
        io::Error::other("injected directory sync failure"),
    ));

    assert!(error.destination_was_replaced());
    assert!(matches!(
        error,
        Int64TablePayloadFileSaveError::Replace(SnapshotReplaceError::SyncDirectory(_))
    ));
}
