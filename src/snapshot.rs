//! Atomic, bounded byte snapshots.
//!
//! A snapshot contains an opaque byte payload. Catalog encoding and write-ahead
//! logging deliberately live outside this module.
//!
//! # File format
//!
//! All integers use little-endian byte order. Version 1 has this layout:
//!
//! | Offset | Size | Field |
//! | ---: | ---: | --- |
//! | 0 | 8 | [`SNAPSHOT_MAGIC`] |
//! | 8 | 2 | [`SNAPSHOT_VERSION`] as a `u16` |
//! | 10 | 8 | payload length as a `u64` |
//! | 18 | 4 | CRC-32/ISO-HDLC checksum of the payload |
//! | 22 | variable | opaque payload bytes |
//!
//! The checksum uses polynomial `0xedb8_8320`, an initial value of
//! `0xffff_ffff`, and a final XOR of `0xffff_ffff`.
//!
//! Writes use a writer-unique `.<file-name>.tmp.<pid>.<sequence>` sibling, sync
//! that file, atomically rename it over the destination, and sync the parent
//! directory on Unix. Concurrent writers can publish in either order, but each
//! published snapshot is complete. Stale temporary siblings are ignored when
//! reading and never conflict with later writers.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Identifies files using the RustHouse snapshot envelope.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"RHSNAP\0\0";

/// The only snapshot envelope version currently supported.
pub const SNAPSHOT_VERSION: u16 = 1;

/// The number of bytes before the payload in version 1.
pub const SNAPSHOT_HEADER_LEN: usize = 22;

/// The default maximum payload size: 64 MiB.
pub const DEFAULT_MAX_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

static NEXT_TEMPORARY_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Details about a snapshot that failed integrity validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotCorruption {
    /// The file is not a RustHouse snapshot envelope.
    InvalidMagic { found: [u8; 8] },
    /// The stored payload checksum differs from the computed checksum.
    ChecksumMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for SnapshotCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic { found } => {
                write!(formatter, "invalid snapshot magic {found:02x?}")
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "snapshot checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
        }
    }
}

/// A typed failure while writing or reading a snapshot envelope.
#[derive(Debug)]
pub enum SnapshotError {
    /// The requested snapshot path does not exist.
    Missing { path: PathBuf },
    /// The payload length exceeds the store's configured bound.
    Oversized {
        payload_len: u64,
        max_payload_len: u64,
    },
    /// The file ends before its header or declared payload is complete.
    Truncated { expected_len: u64, actual_len: u64 },
    /// The magic or checksum failed validation.
    Corrupt(SnapshotCorruption),
    /// The envelope uses a format version this crate cannot read.
    UnsupportedVersion { found: u16, supported: u16 },
    /// Bytes follow the end of the declared payload.
    TrailingData { expected_len: u64, actual_len: u64 },
    /// Memory could not be reserved for the bounded payload.
    AllocationFailed { requested_len: u64 },
    /// A filesystem operation failed for a reason not represented above.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => {
                write!(formatter, "snapshot does not exist: {}", path.display())
            }
            Self::Oversized {
                payload_len,
                max_payload_len,
            } => write!(
                formatter,
                "snapshot payload is {payload_len} bytes, exceeding the {max_payload_len}-byte limit"
            ),
            Self::Truncated {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "snapshot is truncated: expected {expected_len} bytes, found {actual_len}"
            ),
            Self::Corrupt(corruption) => corruption.fmt(formatter),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported snapshot version {found}; this build supports version {supported}"
            ),
            Self::TrailingData {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "snapshot has trailing data: expected {expected_len} bytes, found {actual_len}"
            ),
            Self::AllocationFailed { requested_len } => write!(
                formatter,
                "could not reserve memory for a {requested_len}-byte snapshot payload"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} snapshot path {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for SnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Corrupt(corruption) => Some(corruption),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Error for SnapshotCorruption {}

/// Reads and writes opaque snapshot payloads with a fixed size bound.
///
/// The limit is checked before a write creates a temporary file and before a
/// read allocates its payload buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotStore {
    max_payload_len: usize,
}

impl SnapshotStore {
    /// Creates a snapshot store that accepts payloads up to `max_payload_len`.
    #[must_use]
    pub const fn new(max_payload_len: usize) -> Self {
        Self { max_payload_len }
    }

    /// Returns the configured payload size bound.
    #[must_use]
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Atomically writes `payload` to `path` in the snapshot envelope.
    ///
    /// Existing snapshots are replaced only after the complete temporary file
    /// has been flushed to stable storage. If syncing the parent directory
    /// fails after the rename, the new snapshot may already be visible even
    /// though this method returns an error.
    pub fn write(&self, path: impl AsRef<Path>, payload: &[u8]) -> Result<(), SnapshotError> {
        let path = path.as_ref();
        self.validate_payload_len(payload.len() as u64)?;

        let (parent, temporary_path, mut temporary_file) = create_temporary_file(path)?;

        let write_result = (|| {
            let header = encode_header(payload.len() as u64, crc32(payload));
            temporary_file
                .write_all(&header)
                .and_then(|()| temporary_file.write_all(payload))
                .map_err(|source| io_failure("write temporary", &temporary_path, source))?;
            temporary_file
                .sync_all()
                .map_err(|source| io_failure("sync temporary", &temporary_path, source))?;
            drop(temporary_file);

            fs::rename(&temporary_path, path)
                .map_err(|source| io_failure("replace", path, source))?;
            sync_parent_directory(&parent)?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }

        write_result
    }

    /// Opens, validates, and returns the opaque payload at `path`.
    ///
    /// File structure is validated against metadata before payload allocation,
    /// so oversized or trailing files are never read into an unbounded buffer.
    pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, SnapshotError> {
        let path = path.as_ref();
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(SnapshotError::Missing {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => return Err(io_failure("open", path, source)),
        };

        let actual_len = file
            .metadata()
            .map_err(|source| io_failure("inspect", path, source))?
            .len();
        if actual_len < SNAPSHOT_HEADER_LEN as u64 {
            return Err(SnapshotError::Truncated {
                expected_len: SNAPSHOT_HEADER_LEN as u64,
                actual_len,
            });
        }

        let mut header = [0_u8; SNAPSHOT_HEADER_LEN];
        read_exact(&mut file, &mut header, path, SNAPSHOT_HEADER_LEN as u64)?;

        let found_magic = header[0..8]
            .try_into()
            .expect("snapshot magic has a fixed width");
        if found_magic != SNAPSHOT_MAGIC {
            return Err(SnapshotError::Corrupt(SnapshotCorruption::InvalidMagic {
                found: found_magic,
            }));
        }

        let version = u16::from_le_bytes(
            header[8..10]
                .try_into()
                .expect("snapshot version has a fixed width"),
        );
        if version != SNAPSHOT_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                found: version,
                supported: SNAPSHOT_VERSION,
            });
        }

        let payload_len = u64::from_le_bytes(
            header[10..18]
                .try_into()
                .expect("snapshot length has a fixed width"),
        );
        self.validate_payload_len(payload_len)?;
        let expected_len = (SNAPSHOT_HEADER_LEN as u64)
            .checked_add(payload_len)
            .ok_or_else(|| self.oversized(payload_len))?;

        if actual_len < expected_len {
            return Err(SnapshotError::Truncated {
                expected_len,
                actual_len,
            });
        }
        if actual_len > expected_len {
            return Err(SnapshotError::TrailingData {
                expected_len,
                actual_len,
            });
        }

        let stored_checksum = u32::from_le_bytes(
            header[18..22]
                .try_into()
                .expect("snapshot checksum has a fixed width"),
        );
        let payload_size = usize::try_from(payload_len).map_err(|_| self.oversized(payload_len))?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_size)
            .map_err(|_| SnapshotError::AllocationFailed {
                requested_len: payload_len,
            })?;
        payload.resize(payload_size, 0);
        read_exact(&mut file, &mut payload, path, expected_len)?;

        let actual_checksum = crc32(&payload);
        if actual_checksum != stored_checksum {
            return Err(SnapshotError::Corrupt(
                SnapshotCorruption::ChecksumMismatch {
                    expected: stored_checksum,
                    actual: actual_checksum,
                },
            ));
        }

        Ok(payload)
    }

    fn validate_payload_len(&self, payload_len: u64) -> Result<(), SnapshotError> {
        if payload_len > self.max_payload_len as u64 {
            return Err(self.oversized(payload_len));
        }
        Ok(())
    }

    fn oversized(&self, payload_len: u64) -> SnapshotError {
        SnapshotError::Oversized {
            payload_len,
            max_payload_len: self.max_payload_len as u64,
        }
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PAYLOAD_LEN)
    }
}

fn encode_header(payload_len: u64, checksum: u32) -> [u8; SNAPSHOT_HEADER_LEN] {
    let mut header = [0_u8; SNAPSHOT_HEADER_LEN];
    header[0..8].copy_from_slice(&SNAPSHOT_MAGIC);
    header[8..10].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    header[10..18].copy_from_slice(&payload_len.to_le_bytes());
    header[18..22].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn create_temporary_file(path: &Path) -> Result<(PathBuf, PathBuf, File), SnapshotError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io_failure(
            "resolve",
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot path must have a file name",
            ),
        )
    })?;

    loop {
        let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp.{}.{sequence}", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((parent.to_path_buf(), temporary_path, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(io_failure("create temporary", &temporary_path, source));
            }
        }
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), SnapshotError> {
    let directory =
        File::open(parent).map_err(|source| io_failure("open parent", parent, source))?;
    directory
        .sync_all()
        .map_err(|source| io_failure("sync parent", parent, source))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), SnapshotError> {
    Ok(())
}

fn read_exact(
    file: &mut File,
    bytes: &mut [u8],
    path: &Path,
    expected_len: u64,
) -> Result<(), SnapshotError> {
    match file.read_exact(bytes) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
            let actual_len = file.metadata().map_or(0, |metadata| metadata.len());
            Err(SnapshotError::Truncated {
                expected_len,
                actual_len,
            })
        }
        Err(source) => Err(io_failure("read", path, source)),
    }
}

fn io_failure(operation: &'static str, path: &Path, source: io::Error) -> SnapshotError {
    SnapshotError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    for &byte in bytes {
        let table_index = ((checksum ^ u32::from(byte)) & 0xff) as usize;
        checksum = CRC32_TABLE[table_index] ^ (checksum >> 8);
    }
    !checksum
}

const CRC32_TABLE: [u32; 256] = crc32_table();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(test_name: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("snapshot-tests")
                .join(format!("{test_name}-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn snapshot(&self) -> PathBuf {
            self.0.join("state.snapshot")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    #[test]
    fn round_trips_empty_and_binary_payloads() {
        let directory = TestDirectory::new("round-trip");
        let path = directory.snapshot();
        let store = SnapshotStore::new(32);

        for payload in [&[][..], &[0, 1, 2, 0xff, 0, 4][..]] {
            store.write(&path, payload).expect("write snapshot");
            assert_eq!(store.read(&path).expect("read snapshot"), payload);
        }
    }

    #[test]
    fn atomically_replaces_an_existing_snapshot() {
        let directory = TestDirectory::new("replace");
        let path = directory.snapshot();
        let store = SnapshotStore::new(32);

        store.write(&path, b"first").expect("write first snapshot");
        store
            .write(&path, b"replacement")
            .expect("replace snapshot");

        assert_eq!(store.read(&path).expect("read replacement"), b"replacement");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn ignores_a_stale_temporary_file_from_an_interrupted_writer() {
        let directory = TestDirectory::new("stale-temporary");
        let path = directory.snapshot();
        let temporary_path = directory.0.join(".state.snapshot.tmp.999999.0");
        let store = SnapshotStore::new(32);

        fs::write(&temporary_path, b"incomplete previous write").expect("create stale file");
        store
            .write(&path, b"current")
            .expect("write current snapshot");

        assert_eq!(store.read(&path).expect("read snapshot"), b"current");
        assert_eq!(
            fs::read(&temporary_path).unwrap(),
            b"incomplete previous write"
        );
    }

    #[test]
    fn reports_a_missing_snapshot() {
        let directory = TestDirectory::new("missing");
        let path = directory.snapshot();

        let error = SnapshotStore::default().read(&path).unwrap_err();
        assert!(matches!(error, SnapshotError::Missing { path: found } if found == path));
    }

    #[test]
    fn rejects_oversized_writes_before_creating_files() {
        let directory = TestDirectory::new("oversized-write");
        let path = directory.snapshot();

        let error = SnapshotStore::new(3).write(&path, b"four").unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::Oversized {
                payload_len: 4,
                max_payload_len: 3
            }
        ));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[test]
    fn rejects_an_oversized_declared_payload_before_allocating() {
        let directory = TestDirectory::new("oversized-read");
        let path = directory.snapshot();
        let header = encode_header(4, crc32(b"four"));
        fs::write(&path, [header.as_slice(), b"four"].concat()).expect("write oversized file");

        let error = SnapshotStore::new(3).read(&path).unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::Oversized {
                payload_len: 4,
                max_payload_len: 3
            }
        ));
    }

    #[test]
    fn rejects_truncated_headers_and_payloads() {
        let directory = TestDirectory::new("truncated");
        let path = directory.snapshot();
        let store = SnapshotStore::new(32);

        fs::write(&path, &SNAPSHOT_MAGIC[..4]).expect("write short header");
        assert!(matches!(
            store.read(&path).unwrap_err(),
            SnapshotError::Truncated {
                expected_len: 22,
                actual_len: 4
            }
        ));

        let header = encode_header(5, crc32(b"short"));
        fs::write(&path, [header.as_slice(), b"shor"].concat()).expect("write short payload");
        assert!(matches!(
            store.read(&path).unwrap_err(),
            SnapshotError::Truncated {
                expected_len: 27,
                actual_len: 26
            }
        ));
    }

    #[test]
    fn rejects_invalid_magic_and_checksum_corruption() {
        let directory = TestDirectory::new("corruption");
        let path = directory.snapshot();
        let store = SnapshotStore::new(32);

        let mut invalid_magic = encode_header(0, crc32(b""));
        invalid_magic[0] ^= 0xff;
        fs::write(&path, invalid_magic).expect("write invalid magic");
        assert!(matches!(
            store.read(&path).unwrap_err(),
            SnapshotError::Corrupt(SnapshotCorruption::InvalidMagic { .. })
        ));

        store.write(&path, b"intact").expect("write valid snapshot");
        let mut bytes = fs::read(&path).expect("read raw snapshot");
        bytes[SNAPSHOT_HEADER_LEN] ^= 0xff;
        fs::write(&path, bytes).expect("corrupt payload");
        assert!(matches!(
            store.read(&path).unwrap_err(),
            SnapshotError::Corrupt(SnapshotCorruption::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_unsupported_versions() {
        let directory = TestDirectory::new("version");
        let path = directory.snapshot();
        let mut header = encode_header(0, crc32(b""));
        header[8..10].copy_from_slice(&2_u16.to_le_bytes());
        fs::write(&path, header).expect("write future version");

        assert!(matches!(
            SnapshotStore::default().read(&path).unwrap_err(),
            SnapshotError::UnsupportedVersion {
                found: 2,
                supported: SNAPSHOT_VERSION
            }
        ));
    }

    #[test]
    fn rejects_trailing_data() {
        let directory = TestDirectory::new("trailing");
        let path = directory.snapshot();
        let store = SnapshotStore::new(32);
        store.write(&path, b"valid").expect("write snapshot");

        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open snapshot for append")
            .write_all(b"!")
            .expect("append trailing byte");

        assert!(matches!(
            store.read(&path).unwrap_err(),
            SnapshotError::TrailingData {
                expected_len: 27,
                actual_len: 28
            }
        ));
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
