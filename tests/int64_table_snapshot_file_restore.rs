use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::SNAPSHOT_HEADER_LEN;
use rusthouse::{
    Int64TableFileRestoreError, Int64TableRestoreError, NullableI64PayloadCodec, Schema,
    SnapshotCodec, SnapshotError, restore_int64_table_from_file,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-file-tests");
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

#[test]
fn creates_and_reopens_a_table_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.snapshot");
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), 27);
    let payload = payload_codec.encode(&rows).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    snapshot_codec.create_new_file(&path, &payload).unwrap();

    let table = restore_int64_table_from_file(
        &path,
        Schema::int64("reading", true),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + 27
    );
    assert_eq!(table.row_count(), table.row_cap());
    assert_eq!(table.values(), rows);
}

#[test]
fn reports_a_missing_snapshot_as_an_open_error() {
    let directory = TestDirectory::new();
    let error = restore_int64_table_from_file(
        directory.join("missing.snapshot"),
        Schema::int64("reading", true),
        1,
        SnapshotCodec::new(17),
        NullableI64PayloadCodec::new(1, 17),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileRestoreError::Open(ref source) if source.kind() == ErrorKind::NotFound
    ));
}

#[test]
fn preserves_truncated_envelope_errors() {
    let directory = TestDirectory::new();
    let path = directory.join("truncated.snapshot");
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    let payload = payload_codec.encode(&[Some(7)]).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    let mut envelope = snapshot_codec.encode(&payload).unwrap();
    envelope.pop();
    fs::write(&path, &envelope).unwrap();

    let error = restore_int64_table_from_file(
        path,
        Schema::int64("reading", true),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert_eq!(envelope.len(), SNAPSHOT_HEADER_LEN + payload.len() - 1);
    assert!(matches!(
        error,
        Int64TableFileRestoreError::Restore(Int64TableRestoreError::Envelope(
            SnapshotError::Truncated { .. }
        ))
    ));
}

#[test]
fn preserves_corrupt_envelope_errors() {
    let directory = TestDirectory::new();
    let path = directory.join("corrupt.snapshot");
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    let payload = payload_codec.encode(&[Some(7)]).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    let mut envelope = snapshot_codec.encode(&payload).unwrap();
    *envelope.last_mut().unwrap() ^= 1;
    fs::write(&path, envelope).unwrap();

    let error = restore_int64_table_from_file(
        path,
        Schema::int64("reading", true),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileRestoreError::Restore(Int64TableRestoreError::Envelope(
            SnapshotError::ChecksumMismatch { .. }
        ))
    ));
}

#[test]
fn rejects_a_file_larger_than_the_header_and_payload_limit() {
    let directory = TestDirectory::new();
    let path = directory.join("oversized.snapshot");
    let payload_codec = NullableI64PayloadCodec::new(0, 8);
    let payload = payload_codec.encode(&[]).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    let mut envelope = snapshot_codec.encode(&payload).unwrap();
    envelope.push(0xaa);
    fs::write(&path, envelope).unwrap();

    let error = restore_int64_table_from_file(
        path,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableFileRestoreError::FileTooLarge {
            file_len,
            max_file_len,
        } if file_len == (SNAPSHOT_HEADER_LEN + 9) as u64
            && max_file_len == SNAPSHOT_HEADER_LEN + 8
    ));
}

#[test]
fn rejects_a_directory_as_a_non_regular_file() {
    let directory = TestDirectory::new();
    let error = restore_int64_table_from_file(
        &directory.0,
        Schema::int64("reading", true),
        0,
        SnapshotCodec::new(4096),
        NullableI64PayloadCodec::new(0, 4096),
    )
    .unwrap_err();

    assert!(matches!(error, Int64TableFileRestoreError::NotRegularFile));
}

#[cfg(unix)]
#[test]
fn rejects_a_fifo_with_a_maximum_envelope_and_trailing_data() {
    use std::io::Write as _;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;

    let directory = TestDirectory::new();
    let path = directory.join("stream.snapshot");
    let status = Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(status.success());

    let payload_codec = NullableI64PayloadCodec::new(0, 8);
    let payload = payload_codec.encode(&[]).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());
    let mut bytes = snapshot_codec.encode(&payload).unwrap();
    assert_eq!(bytes.len(), SNAPSHOT_HEADER_LEN + payload.len());
    bytes.push(0xaa);

    // Opening both FIFO ends keeps this hermetic writer from blocking while
    // the restore call rejects the non-regular path before opening it.
    let writer_path = path.clone();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let (done_sender, done_receiver) = mpsc::channel();
    let writer = thread::spawn(move || {
        let mut fifo = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(writer_path)
            .unwrap();
        fifo.write_all(&bytes).unwrap();
        ready_sender.send(()).unwrap();
        done_receiver.recv().unwrap();
    });
    ready_receiver.recv().unwrap();

    let result = restore_int64_table_from_file(
        path,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    );

    done_sender.send(()).unwrap();
    writer.join().unwrap();
    assert!(matches!(
        result,
        Err(Int64TableFileRestoreError::NotRegularFile)
    ));
}
