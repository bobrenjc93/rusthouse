use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{SNAPSHOT_HEADER_LEN, SNAPSHOT_MAGIC, SNAPSHOT_VERSION};
use rusthouse::{SnapshotCodec, SnapshotError, SnapshotFileError};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-tests");
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
fn round_trips_empty_and_binary_payloads() {
    let codec = SnapshotCodec::new(8);

    for payload in [&[][..], &[0, 1, 0xff, 2][..]] {
        let envelope = codec.encode(payload).unwrap();

        assert_eq!(envelope.len(), SNAPSHOT_HEADER_LEN + payload.len());
        assert_eq!(codec.decode(&envelope), Ok(payload));
    }
}

#[test]
fn writes_the_documented_version_1_layout() {
    let envelope = SnapshotCodec::new(9).encode(b"123456789").unwrap();
    let version_offset = SNAPSHOT_MAGIC.len();
    let length_offset = version_offset + std::mem::size_of::<u16>();
    let checksum_offset = length_offset + std::mem::size_of::<u64>();

    assert_eq!(&envelope[..version_offset], SNAPSHOT_MAGIC);
    assert_eq!(
        &envelope[version_offset..length_offset],
        SNAPSHOT_VERSION.to_le_bytes()
    );
    assert_eq!(
        &envelope[length_offset..checksum_offset],
        9_u64.to_le_bytes()
    );
    assert_eq!(
        &envelope[checksum_offset..SNAPSHOT_HEADER_LEN],
        0xcbf4_3926_u32.to_le_bytes()
    );
    assert_eq!(&envelope[SNAPSHOT_HEADER_LEN..], b"123456789");
}

#[test]
fn accepts_a_payload_exactly_at_the_bound() {
    let codec = SnapshotCodec::new(4);
    let envelope = codec.encode(&[1, 2, 3, 4]).unwrap();

    assert_eq!(codec.max_payload_len(), 4);
    assert_eq!(codec.decode(&envelope), Ok(&[1, 2, 3, 4][..]));
}

#[test]
fn rejects_oversized_payloads_during_encode_and_decode() {
    let small_codec = SnapshotCodec::new(2);

    assert_eq!(
        small_codec.encode(&[1, 2, 3]),
        Err(SnapshotError::PayloadTooLarge {
            payload_len: 3,
            max_payload_len: 2,
        })
    );

    let envelope = SnapshotCodec::new(3).encode(&[1, 2, 3]).unwrap();
    assert_eq!(
        small_codec.decode(&envelope),
        Err(SnapshotError::PayloadTooLarge {
            payload_len: 3,
            max_payload_len: 2,
        })
    );
}

#[test]
fn rejects_truncated_headers_and_payloads() {
    let codec = SnapshotCodec::new(8);

    assert_eq!(
        codec.decode(&[0; SNAPSHOT_HEADER_LEN - 1]),
        Err(SnapshotError::Truncated {
            expected_len: SNAPSHOT_HEADER_LEN,
            actual_len: SNAPSHOT_HEADER_LEN - 1,
        })
    );

    let mut envelope = codec.encode(&[1, 2, 3]).unwrap();
    envelope.pop();
    assert_eq!(
        codec.decode(&envelope),
        Err(SnapshotError::Truncated {
            expected_len: SNAPSHOT_HEADER_LEN + 3,
            actual_len: SNAPSHOT_HEADER_LEN + 2,
        })
    );
}

#[test]
fn rejects_incompatible_magic_and_versions() {
    let codec = SnapshotCodec::new(8);
    let envelope = codec.encode(&[1]).unwrap();

    let mut incompatible_magic = envelope.clone();
    incompatible_magic[0] ^= 1;
    assert!(matches!(
        codec.decode(&incompatible_magic),
        Err(SnapshotError::IncompatibleMagic { .. })
    ));

    let mut unsupported_version = envelope;
    unsupported_version[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + 2]
        .copy_from_slice(&(SNAPSHOT_VERSION + 1).to_le_bytes());
    assert_eq!(
        codec.decode(&unsupported_version),
        Err(SnapshotError::UnsupportedVersion {
            found: SNAPSHOT_VERSION + 1,
            supported: SNAPSHOT_VERSION,
        })
    );
}

#[test]
fn detects_every_single_byte_payload_corruption() {
    let codec = SnapshotCodec::new(8);
    let envelope = codec.encode(&[0, 1, 2, 3, 4]).unwrap();

    for payload_index in 0..5 {
        let mut corrupt = envelope.clone();
        corrupt[SNAPSHOT_HEADER_LEN + payload_index] ^= 1;

        assert!(matches!(
            codec.decode(&corrupt),
            Err(SnapshotError::ChecksumMismatch { .. })
        ));
    }
}

#[test]
fn rejects_bytes_after_the_declared_payload() {
    let codec = SnapshotCodec::new(8);
    let mut envelope = codec.encode(&[1, 2]).unwrap();
    envelope.push(3);

    assert_eq!(
        codec.decode(&envelope),
        Err(SnapshotError::TrailingBytes {
            expected_len: SNAPSHOT_HEADER_LEN + 2,
            actual_len: SNAPSHOT_HEADER_LEN + 3,
        })
    );
}

#[test]
fn creates_a_synced_envelope_that_decodes_from_disk() {
    let directory = TestDirectory::new();
    let path = directory.join("snapshot.bin");
    let codec = SnapshotCodec::new(8);

    codec.create_new_file(&path, b"payload").unwrap();

    let envelope = fs::read(path).unwrap();
    assert_eq!(codec.decode(&envelope), Ok(&b"payload"[..]));
}

#[test]
fn preserves_an_existing_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("snapshot.bin");
    let original = b"existing data that is not an envelope";
    fs::write(&path, original).unwrap();

    let error = SnapshotCodec::new(8)
        .create_new_file(&path, b"payload")
        .unwrap_err();

    assert!(matches!(
        error,
        SnapshotFileError::Create(ref source) if source.kind() == ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn rejects_an_oversized_payload_without_creating_a_file() {
    let directory = TestDirectory::new();
    let path = directory.join("snapshot.bin");

    let error = SnapshotCodec::new(2)
        .create_new_file(&path, &[1, 2, 3])
        .unwrap_err();

    assert!(matches!(
        error,
        SnapshotFileError::Encode(SnapshotError::PayloadTooLarge {
            payload_len: 3,
            max_payload_len: 2,
        })
    ));
    assert!(!path.exists());
}
