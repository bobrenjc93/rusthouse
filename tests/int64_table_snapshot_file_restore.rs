use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::SNAPSHOT_HEADER_LEN;
use rusthouse::{
    InsertError, Int64TableFileRecoveryError, Int64TableFileRecoverySource,
    Int64TableFileRestoreError, Int64TableRestoreError, NullableI64PayloadCodec, Schema,
    SnapshotCodec, SnapshotError, restore_int64_table_from_file,
    restore_int64_table_from_file_with_backup,
};
#[cfg(all(unix, not(target_os = "solaris")))]
use rusthouse::{
    Int64TableFileRepairError, SnapshotReplaceError,
    restore_and_repair_int64_table_from_file_with_backup,
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

fn write_snapshot(
    path: &Path,
    rows: &[Option<i64>],
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) {
    let payload = payload_codec.encode(rows).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    fs::write(path, envelope).unwrap();
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

#[test]
fn valid_primary_takes_precedence_over_a_valid_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&primary_path, &[Some(11)], snapshot_codec, payload_codec);
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);

    let recovered = restore_int64_table_from_file_with_backup(
        primary_path,
        backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Primary);
    assert_eq!(recovered.table().values(), &[Some(11)]);
}

#[test]
fn missing_primary_recovers_from_the_explicit_backup() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);

    let recovered = restore_int64_table_from_file_with_backup(
        directory.join("missing-primary.snapshot"),
        backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    let (table, source) = recovered.into_parts();
    assert_eq!(source, Int64TableFileRecoverySource::Backup);
    assert_eq!(table.values(), &[Some(22)]);
}

#[test]
fn corrupt_primary_recovers_from_the_explicit_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&primary_path, &[Some(11)], snapshot_codec, payload_codec);
    let mut primary = fs::read(&primary_path).unwrap();
    *primary.last_mut().unwrap() ^= 1;
    fs::write(&primary_path, primary).unwrap();
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);

    let recovered = restore_int64_table_from_file_with_backup(
        primary_path,
        backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
    assert_eq!(recovered.into_table().values(), &[Some(22)]);
}

#[test]
fn truncated_primary_recovers_from_the_explicit_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&primary_path, &[Some(11)], snapshot_codec, payload_codec);
    let mut primary = fs::read(&primary_path).unwrap();
    primary.pop();
    fs::write(&primary_path, primary).unwrap();
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);

    let recovered = restore_int64_table_from_file_with_backup(
        primary_path,
        backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
    assert_eq!(recovered.table().values(), &[Some(22)]);
}

#[cfg(all(unix, not(target_os = "solaris")))]
#[test]
fn valid_primary_skips_the_backup_and_is_not_rewritten_during_repair() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("missing-backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&primary_path, &[Some(11)], snapshot_codec, payload_codec);
    let primary_before = fs::read(&primary_path).unwrap();

    let recovered = restore_and_repair_int64_table_from_file_with_backup(
        &primary_path,
        &backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Primary);
    assert_eq!(recovered.table().values(), &[Some(11)]);
    assert_eq!(fs::read(primary_path).unwrap(), primary_before);
    assert!(!backup_path.exists());
}

#[cfg(all(unix, not(target_os = "solaris")))]
#[test]
fn missing_primary_is_atomically_repaired_from_the_unchanged_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("missing-primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);
    let backup_before = fs::read(&backup_path).unwrap();

    let recovered = restore_and_repair_int64_table_from_file_with_backup(
        &primary_path,
        &backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
    assert_eq!(recovered.table().values(), &[Some(22)]);
    assert_eq!(fs::read(&primary_path).unwrap(), backup_before);
    assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
    let reopened = restore_int64_table_from_file(
        primary_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();
    assert_eq!(reopened.values(), &[Some(22)]);
}

#[cfg(all(unix, not(target_os = "solaris")))]
#[test]
fn corrupt_primary_is_atomically_repaired_from_the_unchanged_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&primary_path, &[Some(11)], snapshot_codec, payload_codec);
    let mut primary = fs::read(&primary_path).unwrap();
    *primary.last_mut().unwrap() ^= 1;
    fs::write(&primary_path, primary).unwrap();
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);
    let backup_before = fs::read(&backup_path).unwrap();

    let recovered = restore_and_repair_int64_table_from_file_with_backup(
        &primary_path,
        &backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
    assert_eq!(recovered.table().values(), &[Some(22)]);
    assert_eq!(fs::read(&primary_path).unwrap(), backup_before);
    assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
}

#[cfg(all(unix, not(target_os = "solaris")))]
#[test]
fn repair_distinguishes_dual_restore_failures_from_replacement_failures() {
    let directory = TestDirectory::new();
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);

    let dual_error = restore_and_repair_int64_table_from_file_with_backup(
        directory.join("missing-primary.snapshot"),
        directory.join("missing-backup.snapshot"),
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        dual_error,
        Int64TableFileRepairError::BothFailed {
            primary: Int64TableFileRestoreError::Open(ref primary),
            backup: Int64TableFileRestoreError::Open(ref backup),
        } if primary.kind() == ErrorKind::NotFound && backup.kind() == ErrorKind::NotFound
    ));

    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    fs::create_dir(&primary_path).unwrap();
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);
    let backup_before = fs::read(&backup_path).unwrap();

    let repair_error = restore_and_repair_int64_table_from_file_with_backup(
        &primary_path,
        &backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        repair_error,
        Int64TableFileRepairError::RepairFailed {
            primary: Int64TableFileRestoreError::NotRegularFile,
            repair: SnapshotReplaceError::Rename(_),
        }
    ));
    assert!(primary_path.is_dir());
    assert_eq!(fs::read(backup_path).unwrap(), backup_before);
}

#[test]
fn dual_failure_preserves_both_typed_file_errors() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("corrupt-backup.snapshot");
    let snapshot_codec = SnapshotCodec::new(17);
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(&backup_path, &[Some(22)], snapshot_codec, payload_codec);
    let mut backup = fs::read(&backup_path).unwrap();
    *backup.last_mut().unwrap() ^= 1;
    fs::write(&backup_path, backup).unwrap();

    let error = restore_int64_table_from_file_with_backup(
        directory.join("missing-primary.snapshot"),
        backup_path,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        error.primary_error(),
        Int64TableFileRestoreError::Open(source) if source.kind() == ErrorKind::NotFound
    ));
    assert!(matches!(
        error.backup_error(),
        Int64TableFileRestoreError::Restore(Int64TableRestoreError::Envelope(
            SnapshotError::ChecksumMismatch { .. }
        ))
    ));
    let (primary, backup) = error.into_errors();
    assert!(matches!(primary, Int64TableFileRestoreError::Open(_)));
    assert!(matches!(
        backup,
        Int64TableFileRestoreError::Restore(Int64TableRestoreError::Envelope(
            SnapshotError::ChecksumMismatch { .. }
        ))
    ));
}

#[test]
fn backup_recovery_preserves_schema_row_and_file_limits() {
    let directory = TestDirectory::new();

    let nullable_backup = directory.join("nullable-backup.snapshot");
    let one_row_snapshot_codec = SnapshotCodec::new(17);
    let one_row_payload_codec = NullableI64PayloadCodec::new(1, 17);
    write_snapshot(
        &nullable_backup,
        &[None],
        one_row_snapshot_codec,
        one_row_payload_codec,
    );
    let schema_error = restore_int64_table_from_file_with_backup(
        directory.join("missing-schema-primary.snapshot"),
        nullable_backup,
        Schema::int64("reading", false),
        1,
        one_row_snapshot_codec,
        one_row_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        schema_error,
        Int64TableFileRecoveryError::BothFailed {
            backup: Int64TableFileRestoreError::Restore(Int64TableRestoreError::Table(
                InsertError::NullNotAllowed { ref column }
            )),
            ..
        } if column == "reading"
    ));

    let rows_backup = directory.join("rows-backup.snapshot");
    let two_row_snapshot_codec = SnapshotCodec::new(26);
    let two_row_payload_codec = NullableI64PayloadCodec::new(2, 26);
    write_snapshot(
        &rows_backup,
        &[Some(1), Some(2)],
        two_row_snapshot_codec,
        two_row_payload_codec,
    );
    let row_error = restore_int64_table_from_file_with_backup(
        directory.join("missing-rows-primary.snapshot"),
        rows_backup,
        Schema::int64("reading", false),
        1,
        two_row_snapshot_codec,
        two_row_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        row_error,
        Int64TableFileRecoveryError::BothFailed {
            backup: Int64TableFileRestoreError::Restore(Int64TableRestoreError::Table(
                InsertError::RowCapExceeded {
                    row_cap: 1,
                    current_rows: 0,
                    incoming_rows: 2,
                }
            )),
            ..
        }
    ));

    let oversized_backup = directory.join("oversized-backup.snapshot");
    let empty_snapshot_codec = SnapshotCodec::new(8);
    let empty_payload_codec = NullableI64PayloadCodec::new(0, 8);
    write_snapshot(
        &oversized_backup,
        &[],
        empty_snapshot_codec,
        empty_payload_codec,
    );
    let mut oversized = fs::read(&oversized_backup).unwrap();
    oversized.push(0xaa);
    fs::write(&oversized_backup, oversized).unwrap();
    let file_error = restore_int64_table_from_file_with_backup(
        directory.join("missing-file-primary.snapshot"),
        oversized_backup,
        Schema::int64("reading", true),
        0,
        empty_snapshot_codec,
        empty_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        file_error,
        Int64TableFileRecoveryError::BothFailed {
            backup: Int64TableFileRestoreError::FileTooLarge {
                file_len,
                max_file_len,
            },
            ..
        } if file_len == (SNAPSHOT_HEADER_LEN + 9) as u64
            && max_file_len == SNAPSHOT_HEADER_LEN + 8
    ));
}

#[cfg(unix)]
#[test]
fn rejects_a_fifo_with_a_maximum_envelope_and_trailing_data() {
    use std::ffi::CString;
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::thread;

    let directory = TestDirectory::new();
    let path = directory.join("stream.snapshot");
    let fifo_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo_path` is a valid, NUL-terminated pathname that lives
    // through the call, and the mode contains only permission bits.
    let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), libc::S_IRUSR | libc::S_IWUSR) };
    assert_eq!(
        result,
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );

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
