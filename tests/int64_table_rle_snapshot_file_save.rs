#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Int64Table, Int64TableRleFileSaveError, NullableI64PayloadCodec, NullableI64RlePayloadCodec,
    NullableI64RlePayloadError, Schema, SnapshotCodec, SnapshotError, SnapshotReplaceError,
    save_int64_table_rle_to_file, save_int64_table_to_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/rle-snapshot-save-tests");
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

fn table(row_cap: usize, rows: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("reading", true), row_cap);
    table.append_batch(rows).unwrap();
    table
}

fn decode_file(
    path: impl AsRef<Path>,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64RlePayloadCodec,
) -> Vec<Option<i64>> {
    let envelope = fs::read(path).unwrap();
    let payload = snapshot_codec.decode(&envelope).unwrap();
    payload_codec.decode(payload).unwrap()
}

#[test]
fn saves_and_round_trips_nullable_rows_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable-rle.snapshot");
    let rows = [None, None, Some(i64::MIN), Some(i64::MIN), None];
    let table = table(rows.len(), &rows);
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17 + 9;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 3, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);

    save_int64_table_rle_to_file(&path, &table, snapshot_codec, payload_codec).unwrap();

    assert_eq!(decode_file(&path, snapshot_codec, payload_codec), rows);
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
}

#[test]
fn produces_a_measurably_smaller_atomic_snapshot_for_repeated_rows() {
    let directory = TestDirectory::new();
    let rle_path = directory.join("compressed.snapshot");
    let plain_path = directory.join("plain.snapshot");
    let rows = vec![Some(42); 1_000];
    let table = table(rows.len(), &rows);

    save_int64_table_rle_to_file(
        &rle_path,
        &table,
        SnapshotCodec::new(usize::MAX),
        NullableI64RlePayloadCodec::new(rows.len(), 1, usize::MAX),
    )
    .unwrap();
    save_int64_table_to_file(
        &plain_path,
        &table,
        SnapshotCodec::new(usize::MAX),
        NullableI64PayloadCodec::new(rows.len(), usize::MAX),
    )
    .unwrap();

    let compressed_len = fs::metadata(rle_path).unwrap().len();
    let plain_len = fs::metadata(plain_path).unwrap().len();
    assert!(compressed_len * 10 < plain_len);
}

#[test]
fn atomically_overwrites_an_existing_rle_snapshot() {
    let directory = TestDirectory::new();
    let path = directory.join("overwrite.snapshot");
    let payload_codec = NullableI64RlePayloadCodec::new(4, 3, 128);
    let snapshot_codec = SnapshotCodec::new(128);
    let old_table = table(4, &[Some(11), Some(11)]);
    let new_rows = [None, None, Some(22), Some(22)];
    let new_table = table(4, &new_rows);

    save_int64_table_rle_to_file(&path, &old_table, snapshot_codec, payload_codec).unwrap();
    let old_envelope = fs::read(&path).unwrap();
    save_int64_table_rle_to_file(&path, &new_table, snapshot_codec, payload_codec).unwrap();

    assert_ne!(fs::read(&path).unwrap(), old_envelope);
    assert_eq!(decode_file(path, snapshot_codec, payload_codec), new_rows);
}

#[test]
fn one_below_each_exact_limit_preserves_the_destination_with_typed_errors() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    let rows = [None, Some(7)];
    let table = table(rows.len(), &rows);
    let exact_payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17;

    let cases = [
        (
            NullableI64RlePayloadCodec::new(1, 2, exact_payload_len),
            SnapshotCodec::new(exact_payload_len),
            "row",
        ),
        (
            NullableI64RlePayloadCodec::new(2, 1, exact_payload_len),
            SnapshotCodec::new(exact_payload_len),
            "run",
        ),
        (
            NullableI64RlePayloadCodec::new(2, 2, exact_payload_len - 1),
            SnapshotCodec::new(exact_payload_len),
            "payload",
        ),
        (
            NullableI64RlePayloadCodec::new(2, 2, exact_payload_len),
            SnapshotCodec::new(exact_payload_len - 1),
            "envelope",
        ),
    ];

    for (payload_codec, snapshot_codec, expected) in cases {
        fs::write(&path, original).unwrap();

        let error =
            save_int64_table_rle_to_file(&path, &table, snapshot_codec, payload_codec).unwrap_err();

        assert!(!error.destination_was_replaced());
        match (expected, error) {
            (
                "row",
                Int64TableRleFileSaveError::Payload(NullableI64RlePayloadError::RowLimitExceeded {
                    row_count: 2,
                    max_rows: 1,
                }),
            )
            | (
                "run",
                Int64TableRleFileSaveError::Payload(NullableI64RlePayloadError::RunLimitExceeded {
                    run_count: 2,
                    max_runs: 1,
                }),
            ) => {}
            (
                "payload",
                Int64TableRleFileSaveError::Payload(NullableI64RlePayloadError::PayloadTooLarge {
                    payload_len,
                    max_payload_len,
                }),
            ) => {
                assert_eq!(payload_len, exact_payload_len as u64);
                assert_eq!(max_payload_len, exact_payload_len - 1);
            }
            (
                "envelope",
                Int64TableRleFileSaveError::Replace(SnapshotReplaceError::Encode(
                    SnapshotError::PayloadTooLarge {
                        payload_len,
                        max_payload_len,
                    },
                )),
            ) => {
                assert_eq!(payload_len, exact_payload_len as u64);
                assert_eq!(max_payload_len, exact_payload_len - 1);
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
    let table = table(1, &[Some(7)]);

    let error = save_int64_table_rle_to_file(
        &path,
        &table,
        SnapshotCodec::new(43),
        NullableI64RlePayloadCodec::new(1, 1, 43),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableRleFileSaveError::Replace(SnapshotReplaceError::Rename(_))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(marker).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}
