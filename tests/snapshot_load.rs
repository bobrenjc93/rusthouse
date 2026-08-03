use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::SNAPSHOT_HEADER_LEN;
use rusthouse::{SnapshotCodec, SnapshotError, SnapshotLoadError};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("snapshot-load-tests")
            .join(format!("{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reopens_a_valid_snapshot_payload() {
    let directory = TestDir::new();
    let path = directory.path("catalog.snapshot");
    let payload = b"\0catalog\xffrows";
    let codec = SnapshotCodec::new(payload.len());
    fs::write(&path, codec.encode(payload).unwrap()).unwrap();

    assert_eq!(codec.load(&path).unwrap(), payload);
}

#[test]
fn reports_truncated_files_as_codec_validation_failures() {
    let directory = TestDir::new();
    let path = directory.path("truncated.snapshot");
    let codec = SnapshotCodec::new(32);
    let mut envelope = codec.encode(b"catalog").unwrap();
    envelope.pop();
    fs::write(&path, envelope).unwrap();

    assert!(matches!(
        codec.load(&path),
        Err(SnapshotLoadError::Validation(SnapshotError::Truncated {
            expected_len,
            actual_len,
        })) if expected_len == SNAPSHOT_HEADER_LEN + 7
            && actual_len == SNAPSHOT_HEADER_LEN + 6
    ));
}

#[test]
fn reports_corruption_as_a_codec_validation_failure() {
    let directory = TestDir::new();
    let path = directory.path("corrupt.snapshot");
    let codec = SnapshotCodec::new(32);
    let mut envelope = codec.encode(b"catalog").unwrap();
    envelope[SNAPSHOT_HEADER_LEN + 2] ^= 1;
    fs::write(&path, envelope).unwrap();

    assert!(matches!(
        codec.load(&path),
        Err(SnapshotLoadError::Validation(
            SnapshotError::ChecksumMismatch { .. }
        ))
    ));
}

#[test]
fn rejects_a_file_one_byte_over_the_envelope_bound() {
    let directory = TestDir::new();
    let path = directory.path("oversized.snapshot");
    let codec = SnapshotCodec::new(4);
    fs::write(&path, vec![0; codec.max_envelope_len() + 1]).unwrap();

    assert!(matches!(
        codec.load(&path),
        Err(SnapshotLoadError::FileTooLarge { max_envelope_len })
            if max_envelope_len == SNAPSHOT_HEADER_LEN + 4
    ));
}

#[test]
fn distinguishes_open_failures() {
    let directory = TestDir::new();
    let path = directory.path("missing.snapshot");

    assert!(matches!(
        SnapshotCodec::new(4).load(&path),
        Err(SnapshotLoadError::Open { path: error_path, .. }) if error_path == path
    ));
}

#[cfg(unix)]
#[test]
fn distinguishes_read_failures_after_opening() {
    let directory = TestDir::new();

    assert!(matches!(
        SnapshotCodec::new(4).load(&directory.0),
        Err(SnapshotLoadError::Read { path, .. }) if path == directory.0
    ));
}
