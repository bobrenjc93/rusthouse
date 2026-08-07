#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    InsertError, Int64Table, Int64TableRleFileRestoreError, NullableI64RlePayloadCodec,
    NullableI64RlePayloadError, Schema, SnapshotCodec, SnapshotError,
    restore_int64_table_rle_from_file, save_int64_table_rle_to_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/rle-snapshot-restore-tests");
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

fn write_envelope(
    path: &Path,
    rows: &[Option<i64>],
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64RlePayloadCodec,
) -> Vec<u8> {
    let payload = payload_codec.encode(rows).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    fs::write(path, &envelope).unwrap();
    envelope
}

#[test]
fn reopens_atomically_saved_compressed_rows_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [None, None, Some(i64::MIN), Some(i64::MIN), None];
    let schema = Schema::int64("reading", true);
    let source = table(schema.clone(), rows.len(), &rows);
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17 + 9;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 3, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);

    save_int64_table_rle_to_file(&path, &source, snapshot_codec, payload_codec).unwrap();
    let reopened =
        restore_int64_table_rle_from_file(&path, schema, rows.len(), snapshot_codec, payload_codec)
            .unwrap();

    assert_eq!(reopened, source);
    assert_eq!(reopened.row_count(), reopened.row_cap());
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
}

#[test]
fn preserves_open_non_regular_and_bounded_file_failures() {
    let directory = TestDirectory::new();
    let snapshot_codec = SnapshotCodec::new(NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN);
    let payload_codec = NullableI64RlePayloadCodec::new(0, 0, NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN);

    let missing = restore_int64_table_rle_from_file(
        directory.join("missing.snapshot"),
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        missing,
        Int64TableRleFileRestoreError::Open(ref error)
            if error.kind() == ErrorKind::NotFound
    ));

    let directory_error = restore_int64_table_rle_from_file(
        &directory.0,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        directory_error,
        Int64TableRleFileRestoreError::NotRegularFile
    ));

    let socket_path = directory
        .join("snapshot.socket")
        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
        .to_owned();
    let _listener = UnixListener::bind(&socket_path).unwrap();
    let socket_error = restore_int64_table_rle_from_file(
        socket_path,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        socket_error,
        Int64TableRleFileRestoreError::NotRegularFile
    ));

    let oversized_path = directory.join("oversized.snapshot");
    let mut envelope = snapshot_codec
        .encode(&payload_codec.encode(&[]).unwrap())
        .unwrap();
    envelope.push(0xaa);
    fs::write(&oversized_path, envelope).unwrap();
    let oversized = restore_int64_table_rle_from_file(
        oversized_path,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        oversized,
        Int64TableRleFileRestoreError::FileTooLarge {
            file_len,
            max_file_len,
        } if file_len == (SNAPSHOT_HEADER_LEN + NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1) as u64
            && max_file_len == SNAPSHOT_HEADER_LEN + NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN
    ));
}

#[test]
fn preserves_corrupt_and_trailing_envelope_errors() {
    let directory = TestDirectory::new();
    let payload_codec = NullableI64RlePayloadCodec::new(1, 1, 43);
    let snapshot_codec = SnapshotCodec::new(44);

    let corrupt_path = directory.join("corrupt-envelope.snapshot");
    let mut corrupt = write_envelope(&corrupt_path, &[Some(7)], snapshot_codec, payload_codec);
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&corrupt_path, corrupt).unwrap();
    let corrupt_error = restore_int64_table_rle_from_file(
        corrupt_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        corrupt_error,
        Int64TableRleFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
    ));

    let trailing_path = directory.join("trailing-envelope.snapshot");
    let mut trailing = write_envelope(&trailing_path, &[Some(7)], snapshot_codec, payload_codec);
    let expected_len = trailing.len();
    trailing.push(0xaa);
    fs::write(&trailing_path, trailing).unwrap();
    let trailing_error = restore_int64_table_rle_from_file(
        trailing_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        trailing_error,
        Int64TableRleFileRestoreError::Envelope(SnapshotError::TrailingBytes {
            expected_len: expected,
            actual_len,
        }) if expected == expected_len && actual_len == expected_len + 1
    ));
}

#[test]
fn preserves_corrupt_and_trailing_rle_payload_errors() {
    let directory = TestDirectory::new();
    let exact_payload_codec = NullableI64RlePayloadCodec::new(1, 1, 43);
    let payload = exact_payload_codec.encode(&[Some(7)]).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len() + 1);

    let corrupt_path = directory.join("corrupt-payload.snapshot");
    let mut corrupt = payload.clone();
    corrupt[0] ^= 1;
    snapshot_codec
        .create_new_file(&corrupt_path, &corrupt)
        .unwrap();
    let corrupt_error = restore_int64_table_rle_from_file(
        corrupt_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        exact_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        corrupt_error,
        Int64TableRleFileRestoreError::Payload(
            NullableI64RlePayloadError::IncompatibleMagic { .. }
        )
    ));

    let trailing_path = directory.join("trailing-payload.snapshot");
    let expected_len = payload.len();
    let mut trailing = payload;
    trailing.push(0xaa);
    snapshot_codec
        .create_new_file(&trailing_path, &trailing)
        .unwrap();
    let permissive_payload_codec = NullableI64RlePayloadCodec::new(1, 1, expected_len + 1);
    let trailing_error = restore_int64_table_rle_from_file(
        trailing_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        permissive_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        trailing_error,
        Int64TableRleFileRestoreError::Payload(
            NullableI64RlePayloadError::TrailingData {
                expected_len: expected,
                actual_len,
            }
        ) if expected == expected_len && actual_len == expected_len + 1
    ));
}

#[test]
fn keeps_rle_nullability_and_table_capacity_failures_distinct() {
    let directory = TestDirectory::new();
    let path = directory.join("two-rows.snapshot");
    let rows = [None, Some(7)];
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17;
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 2, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    write_envelope(&path, &rows, snapshot_codec, payload_codec);

    let nullability = restore_int64_table_rle_from_file(
        &path,
        Schema::int64("reading", false),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        nullability,
        Int64TableRleFileRestoreError::Table(InsertError::NullNotAllowed { ref column })
            if column == "reading"
    ));

    let capacity = restore_int64_table_rle_from_file(
        path,
        Schema::int64("reading", true),
        rows.len() - 1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        capacity,
        Int64TableRleFileRestoreError::Table(InsertError::RowCapExceeded {
            row_cap: 1,
            current_rows: 0,
            incoming_rows: 2,
        })
    ));
}

#[test]
fn keeps_rle_row_run_and_payload_limits_independent() {
    let directory = TestDirectory::new();
    let path = directory.join("limited.snapshot");
    let rows = [None, Some(7)];
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17;
    let exact_payload_codec = NullableI64RlePayloadCodec::new(2, 2, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    write_envelope(&path, &rows, snapshot_codec, exact_payload_codec);

    let cases = [
        NullableI64RlePayloadCodec::new(1, 2, payload_len),
        NullableI64RlePayloadCodec::new(2, 1, payload_len),
        NullableI64RlePayloadCodec::new(2, 2, payload_len - 1),
    ];
    for (index, payload_codec) in cases.into_iter().enumerate() {
        let error = restore_int64_table_rle_from_file(
            &path,
            Schema::int64("reading", true),
            2,
            snapshot_codec,
            payload_codec,
        )
        .unwrap_err();
        match (index, error) {
            (
                0,
                Int64TableRleFileRestoreError::Payload(
                    NullableI64RlePayloadError::RowLimitExceeded {
                        row_count: 2,
                        max_rows: 1,
                    },
                ),
            )
            | (
                1,
                Int64TableRleFileRestoreError::Payload(
                    NullableI64RlePayloadError::RunLimitExceeded {
                        run_count: 2,
                        max_runs: 1,
                    },
                ),
            ) => {}
            (
                2,
                Int64TableRleFileRestoreError::Payload(
                    NullableI64RlePayloadError::PayloadTooLarge {
                        payload_len: actual,
                        max_payload_len,
                    },
                ),
            ) if actual == payload_len as u64 && max_payload_len == payload_len - 1 => {}
            (_, error) => panic!("unexpected limit error: {error:?}"),
        }
    }
}
