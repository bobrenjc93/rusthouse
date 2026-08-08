use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{INT64_TABLE_PAYLOAD_FIXED_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Int64Table, Int64TablePayloadCodec, Int64TablePayloadError, Int64TablePayloadFileRecoveryError,
    Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError, Schema, SnapshotCodec,
    SnapshotError, restore_int64_table_payload_from_file,
    restore_int64_table_payload_from_file_with_backup,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/table-payload-file-tests");
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

fn write_table_snapshot(
    path: &Path,
    table: &Int64Table,
    snapshot_codec: SnapshotCodec,
    payload_codec: Int64TablePayloadCodec,
) {
    let payload = payload_codec.encode(table).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    fs::write(path, envelope).unwrap();
}

#[test]
fn create_new_file_reopens_nullable_metadata_and_rows_at_exact_limits() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable.snapshot");
    let name = "métric";
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let source = table(name, true, rows.len(), &rows);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 19;
    let payload_codec = Int64TablePayloadCodec::new(name.len(), rows.len(), payload_len);
    let payload = payload_codec.encode(&source).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload_len);

    snapshot_codec.create_new_file(&path, &payload).unwrap();
    let reopened =
        restore_int64_table_payload_from_file(&path, snapshot_codec, payload_codec).unwrap();

    assert_eq!(payload.len(), payload_len);
    assert_eq!(reopened, source);
    assert_eq!(reopened.schema(), &Schema::int64(name, true));
    assert_eq!(reopened.row_cap(), rows.len());
    assert_eq!(reopened.values(), rows);
    assert_eq!(
        fs::metadata(path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
}

#[cfg(unix)]
#[test]
fn atomic_replace_reopens_non_nullable_schema_and_unused_row_capacity() {
    let directory = TestDirectory::new();
    let path = directory.join("non-nullable.snapshot");
    let payload_codec = Int64TablePayloadCodec::new(8, 5, 128);
    let snapshot_codec = SnapshotCodec::new(128);
    let old = table("old", true, 1, &[None]);
    let replacement = table("reading", false, 5, &[Some(-7), Some(11)]);

    snapshot_codec
        .replace_file(&path, &payload_codec.encode(&old).unwrap())
        .unwrap();
    snapshot_codec
        .replace_file(&path, &payload_codec.encode(&replacement).unwrap())
        .unwrap();

    let reopened =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap();
    assert_eq!(reopened, replacement);
    assert_eq!(reopened.schema(), &Schema::int64("reading", false));
    assert_eq!(reopened.row_cap(), 5);
    assert_eq!(reopened.values(), &[Some(-7), Some(11)]);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn valid_primary_takes_precedence_without_inspecting_the_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let primary = table("primary", false, 2, &[Some(11)]);
    let snapshot_codec = SnapshotCodec::new(128);
    let payload_codec = Int64TablePayloadCodec::new(16, 2, 128);
    write_table_snapshot(&primary_path, &primary, snapshot_codec, payload_codec);

    let recovered = restore_int64_table_payload_from_file_with_backup(
        primary_path,
        directory.join("missing-backup.snapshot"),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(
        recovered.source(),
        Int64TablePayloadFileRecoverySource::Primary
    );
    assert_eq!(recovered.table(), &primary);
}

#[test]
fn missing_primary_recovers_backup_at_every_exact_limit() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("backup.snapshot");
    let name = "métric";
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let backup = table(name, true, rows.len(), &rows);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 19;
    let payload_codec = Int64TablePayloadCodec::new(name.len(), rows.len(), payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    write_table_snapshot(&backup_path, &backup, snapshot_codec, payload_codec);

    let recovered = restore_int64_table_payload_from_file_with_backup(
        directory.join("missing-primary.snapshot"),
        &backup_path,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    let (reopened, source) = recovered.into_parts();
    assert_eq!(source, Int64TablePayloadFileRecoverySource::Backup);
    assert_eq!(reopened, backup);
    assert_eq!(reopened.schema(), &Schema::int64(name, true));
    assert_eq!(reopened.row_cap(), rows.len());
    assert_eq!(reopened.values(), rows);
    assert_eq!(
        fs::metadata(backup_path).unwrap().len() as usize,
        SNAPSHOT_HEADER_LEN + payload_len
    );
}

#[test]
fn corrupt_primary_recovers_from_the_explicit_backup() {
    let directory = TestDirectory::new();
    let primary_path = directory.join("primary.snapshot");
    let backup_path = directory.join("backup.snapshot");
    let primary = table("primary", false, 1, &[Some(11)]);
    let backup = table("backup", true, 3, &[None, Some(22)]);
    let snapshot_codec = SnapshotCodec::new(128);
    let payload_codec = Int64TablePayloadCodec::new(16, 3, 128);
    write_table_snapshot(&primary_path, &primary, snapshot_codec, payload_codec);
    let mut corrupt_primary = fs::read(&primary_path).unwrap();
    *corrupt_primary.last_mut().unwrap() ^= 1;
    fs::write(&primary_path, corrupt_primary).unwrap();
    write_table_snapshot(&backup_path, &backup, snapshot_codec, payload_codec);

    let recovered = restore_int64_table_payload_from_file_with_backup(
        primary_path,
        backup_path,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(
        recovered.source(),
        Int64TablePayloadFileRecoverySource::Backup
    );
    assert_eq!(recovered.into_table(), backup);
}

#[test]
fn dual_failure_preserves_both_typed_payload_file_errors() {
    let directory = TestDirectory::new();
    let backup_path = directory.join("corrupt-backup.snapshot");
    let backup = table("backup", false, 1, &[Some(22)]);
    let snapshot_codec = SnapshotCodec::new(128);
    let payload_codec = Int64TablePayloadCodec::new(16, 1, 128);
    let mut corrupt_payload = payload_codec.encode(&backup).unwrap();
    corrupt_payload[0] ^= 1;
    snapshot_codec
        .create_new_file(&backup_path, &corrupt_payload)
        .unwrap();

    let error = restore_int64_table_payload_from_file_with_backup(
        directory.join("missing-primary.snapshot"),
        backup_path,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        &error,
        Int64TablePayloadFileRecoveryError::BothFailed {
            primary: Int64TablePayloadFileRestoreError::Open(primary),
            backup: Int64TablePayloadFileRestoreError::Payload(
                Int64TablePayloadError::IncompatibleMagic { .. }
            ),
        } if primary.kind() == ErrorKind::NotFound
    ));
    assert!(matches!(
        error.primary_error(),
        Int64TablePayloadFileRestoreError::Open(source)
            if source.kind() == ErrorKind::NotFound
    ));
    assert!(matches!(
        error.backup_error(),
        Int64TablePayloadFileRestoreError::Payload(
            Int64TablePayloadError::IncompatibleMagic { .. }
        )
    ));
    let (primary, backup) = error.into_errors();
    assert!(matches!(
        primary,
        Int64TablePayloadFileRestoreError::Open(_)
    ));
    assert!(matches!(
        backup,
        Int64TablePayloadFileRestoreError::Payload(
            Int64TablePayloadError::IncompatibleMagic { .. }
        )
    ));
}

#[test]
fn preserves_open_and_non_regular_file_failures() {
    let directory = TestDirectory::new();
    let snapshot_codec = SnapshotCodec::new(64);
    let payload_codec = Int64TablePayloadCodec::new(8, 1, 64);

    let missing = restore_int64_table_payload_from_file(
        directory.join("missing.snapshot"),
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        missing,
        Int64TablePayloadFileRestoreError::Open(ref error)
            if error.kind() == ErrorKind::NotFound
    ));

    let non_regular =
        restore_int64_table_payload_from_file(&directory.0, snapshot_codec, payload_codec)
            .unwrap_err();
    assert!(matches!(
        non_regular,
        Int64TablePayloadFileRestoreError::NotRegularFile
    ));
}

#[test]
fn rejects_a_file_larger_than_the_bounded_envelope_before_decoding() {
    let directory = TestDirectory::new();
    let path = directory.join("oversized.snapshot");
    let source = table("id", false, 0, &[]);
    let payload_len = INT64_TABLE_PAYLOAD_FIXED_LEN + 2;
    let payload_codec = Int64TablePayloadCodec::new(2, 0, payload_len);
    let snapshot_codec = SnapshotCodec::new(payload_len);
    let payload = payload_codec.encode(&source).unwrap();
    let mut envelope = snapshot_codec.encode(&payload).unwrap();
    envelope.push(0xaa);
    fs::write(&path, envelope).unwrap();

    let error =
        restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec).unwrap_err();
    assert!(matches!(
        error,
        Int64TablePayloadFileRestoreError::FileTooLarge {
            file_len,
            max_file_len,
        } if file_len == (SNAPSHOT_HEADER_LEN + payload_len + 1) as u64
            && max_file_len == SNAPSHOT_HEADER_LEN + payload_len
    ));
}

#[test]
fn preserves_envelope_and_payload_corruption_failures() {
    let directory = TestDirectory::new();
    let source = table("id", false, 1, &[Some(7)]);
    let payload_codec = Int64TablePayloadCodec::new(2, 1, 64);
    let payload = payload_codec.encode(&source).unwrap();
    let snapshot_codec = SnapshotCodec::new(64);

    let envelope_path = directory.join("envelope-corrupt.snapshot");
    let mut corrupt_envelope = snapshot_codec.encode(&payload).unwrap();
    *corrupt_envelope.last_mut().unwrap() ^= 1;
    fs::write(&envelope_path, corrupt_envelope).unwrap();
    let envelope_error =
        restore_int64_table_payload_from_file(envelope_path, snapshot_codec, payload_codec)
            .unwrap_err();
    assert!(matches!(
        envelope_error,
        Int64TablePayloadFileRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
    ));

    let payload_path = directory.join("payload-corrupt.snapshot");
    let mut corrupt_payload = payload;
    corrupt_payload[0] ^= 1;
    snapshot_codec
        .create_new_file(&payload_path, &corrupt_payload)
        .unwrap();
    let payload_error =
        restore_int64_table_payload_from_file(payload_path, snapshot_codec, payload_codec)
            .unwrap_err();
    assert!(matches!(
        payload_error,
        Int64TablePayloadFileRestoreError::Payload(
            Int64TablePayloadError::IncompatibleMagic { .. }
        )
    ));
}

#[test]
fn rejects_trailing_envelope_and_payload_input() {
    let directory = TestDirectory::new();
    let source = table("id", false, 1, &[Some(7)]);
    let exact_payload_codec = Int64TablePayloadCodec::new(2, 1, 64);
    let payload = exact_payload_codec.encode(&source).unwrap();
    let permissive_snapshot_codec = SnapshotCodec::new(payload.len() + 1);

    let envelope_path = directory.join("envelope-trailing.snapshot");
    let mut trailing_envelope = permissive_snapshot_codec.encode(&payload).unwrap();
    let expected_envelope_len = trailing_envelope.len();
    trailing_envelope.push(0xaa);
    fs::write(&envelope_path, trailing_envelope).unwrap();
    let envelope_error = restore_int64_table_payload_from_file(
        envelope_path,
        permissive_snapshot_codec,
        exact_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        envelope_error,
        Int64TablePayloadFileRestoreError::Envelope(SnapshotError::TrailingBytes {
            expected_len,
            actual_len,
        }) if expected_len == expected_envelope_len && actual_len == expected_envelope_len + 1
    ));

    let payload_path = directory.join("payload-trailing.snapshot");
    let expected_payload_len = payload.len();
    let mut trailing_payload = payload;
    trailing_payload.push(0xaa);
    permissive_snapshot_codec
        .create_new_file(&payload_path, &trailing_payload)
        .unwrap();
    let permissive_payload_codec = Int64TablePayloadCodec::new(2, 1, expected_payload_len + 1);
    let payload_error = restore_int64_table_payload_from_file(
        payload_path,
        permissive_snapshot_codec,
        permissive_payload_codec,
    )
    .unwrap_err();
    assert!(matches!(
        payload_error,
        Int64TablePayloadFileRestoreError::Payload(Int64TablePayloadError::TrailingData {
            expected_len,
            actual_len,
        }) if expected_len == expected_payload_len && actual_len == expected_payload_len + 1
    ));
}
