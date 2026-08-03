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
//! Each write exclusively creates a short
//! `.rhsnap-<destination-id>-<process-id>-<sequence>.tmp` sibling, locks and
//! syncs that file, atomically renames it over the destination, and syncs the
//! parent directory on Unix. A per-destination `.rhsnap-<destination-id>.lock`
//! file serializes temporary-file allocation for one spelling of a destination,
//! while payload writes and unrelated destinations remain independent. A new
//! temporary becomes reclaimable only after its file lock is held. Every writer
//! may therefore remove accessible, unlocked temporaries abandoned by dead
//! writers regardless of destination spelling, while live or inaccessible
//! files are left untouched. File names beginning with `.rhsnap-` (ASCII
//! case-insensitively) are reserved for this protocol and rejected as snapshot
//! destinations. The last rename determines the visible snapshot.

use std::error::Error;
use std::ffi::OsStr;
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
    /// The path's file name belongs to the internal snapshot-write namespace.
    ReservedPath { path: PathBuf },
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
            Self::ReservedPath { path } => write!(
                formatter,
                "snapshot path uses the reserved .rhsnap- namespace: {}",
                path.display()
            ),
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

        let (parent, mut temporary) = create_sibling_temporary_file(path)?;
        let temporary_path = temporary.path().to_path_buf();
        let header = encode_header(payload.len() as u64, crc32(payload));
        temporary
            .file_mut()
            .write_all(&header)
            .and_then(|()| temporary.file_mut().write_all(payload))
            .map_err(|source| io_failure("write temporary", &temporary_path, source))?;
        temporary
            .file_mut()
            .sync_all()
            .map_err(|source| io_failure("sync temporary", &temporary_path, source))?;

        fs::rename(&temporary_path, path).map_err(|source| io_failure("replace", path, source))?;
        temporary.mark_published();
        sync_parent_directory(&parent)
    }

    /// Opens, validates, and returns the opaque payload at `path`.
    ///
    /// File structure is validated against metadata before payload allocation,
    /// so oversized or trailing files are never read into an unbounded buffer.
    pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, SnapshotError> {
        let path = path.as_ref();
        validate_snapshot_path(path)?;
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

static NEXT_TEMPORARY_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const TEMPORARY_FILE_PREFIX: &str = ".rhsnap-";
const TEMPORARY_FILE_SUFFIX: &str = ".tmp";
const COORDINATOR_FILE_SUFFIX: &str = ".lock";
const DESTINATION_ID_HEX_LEN: usize = 32;
const PROCESS_ID_HEX_LEN: usize = 8;
const SEQUENCE_HEX_LEN: usize = 16;
const FNV1A_128_OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV1A_128_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

struct OwnedTemporaryFile {
    path: PathBuf,
    file: File,
    published: bool,
}

impl OwnedTemporaryFile {
    fn path(&self) -> &Path {
        &self.path
    }

    fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for OwnedTemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_sibling_temporary_file(
    path: &Path,
) -> Result<(PathBuf, OwnedTemporaryFile), SnapshotError> {
    let file_name = validate_snapshot_path(path)?;
    let destination_id = destination_id(file_name);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let coordinator_path = parent.join(coordinator_file_name(destination_id));
    let coordinator = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&coordinator_path)
        .map_err(|source| io_failure("open temporary coordinator", &coordinator_path, source))?;
    coordinator
        .lock()
        .map_err(|source| io_failure("lock temporary coordinator", &coordinator_path, source))?;
    reclaim_orphaned_temporary_files(parent);

    loop {
        let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staging_path = parent.join(staging_file_name(
            destination_id,
            std::process::id(),
            sequence,
        ));

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)
        {
            Ok(file) => {
                let mut temporary = OwnedTemporaryFile {
                    path: staging_path,
                    file,
                    published: false,
                };
                temporary
                    .file
                    .lock()
                    .map_err(|source| io_failure("lock temporary", &temporary.path, source))?;
                let temporary_path = parent.join(temporary_file_name(
                    destination_id,
                    std::process::id(),
                    sequence,
                ));
                fs::rename(&temporary.path, &temporary_path).map_err(|source| {
                    io_failure("make temporary reclaimable", &temporary_path, source)
                })?;
                temporary.path = temporary_path;
                return Ok((parent.to_path_buf(), temporary));
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(io_failure("create temporary", &staging_path, source));
            }
        }
    }
}

fn validate_snapshot_path(path: &Path) -> Result<&OsStr, SnapshotError> {
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
    if file_name
        .as_encoded_bytes()
        .get(..TEMPORARY_FILE_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(TEMPORARY_FILE_PREFIX.as_bytes()))
    {
        return Err(SnapshotError::ReservedPath {
            path: path.to_path_buf(),
        });
    }
    Ok(file_name)
}

fn destination_id(file_name: &OsStr) -> u128 {
    let mut hash = FNV1A_128_OFFSET_BASIS;
    for byte in file_name.as_encoded_bytes() {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV1A_128_PRIME);
    }
    hash
}

fn coordinator_file_name(destination_id: u128) -> String {
    format!("{TEMPORARY_FILE_PREFIX}{destination_id:032x}{COORDINATOR_FILE_SUFFIX}")
}

fn temporary_file_name(destination_id: u128, process_id: u32, sequence: u64) -> String {
    format!(
        "{TEMPORARY_FILE_PREFIX}{destination_id:032x}-{process_id:08x}-{sequence:016x}{TEMPORARY_FILE_SUFFIX}"
    )
}

fn staging_file_name(destination_id: u128, process_id: u32, sequence: u64) -> String {
    format!(
        "{TEMPORARY_FILE_PREFIX}staging-{destination_id:032x}-{process_id:08x}-{sequence:016x}{TEMPORARY_FILE_SUFFIX}"
    )
}

fn reclaim_orphaned_temporary_files(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        if snapshot_temporary_file_destination_id(&entry.file_name()).is_none() {
            continue;
        }

        let temporary_path = entry.path();
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temporary_path)
        else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        let _ = fs::remove_file(&temporary_path);
    }
}

#[cfg(test)]
fn is_snapshot_temporary_file_name(name: &OsStr) -> bool {
    snapshot_temporary_file_destination_id(name).is_some()
}

fn snapshot_temporary_file_destination_id(name: &OsStr) -> Option<u128> {
    let identity = name
        .to_str()
        .and_then(|name| name.strip_prefix(TEMPORARY_FILE_PREFIX))
        .and_then(|name| name.strip_suffix(TEMPORARY_FILE_SUFFIX))?;
    let mut fields = identity.split('-');
    let (Some(destination_id), Some(process_id), Some(sequence), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    if destination_id.len() != DESTINATION_ID_HEX_LEN
        || process_id.len() != PROCESS_ID_HEX_LEN
        || sequence.len() != SEQUENCE_HEX_LEN
        || !destination_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !process_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !sequence.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    u128::from_str_radix(destination_id, 16).ok()
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
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const INTERRUPTED_WRITER_DIRECTORY: &str = "RUSTHOUSE_INTERRUPTED_WRITER_DIRECTORY";

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
        assert!(snapshot_temporary_files(&directory.0).is_empty());
    }

    #[test]
    fn does_not_remove_an_unowned_temporary_file() {
        let directory = TestDirectory::new("unowned-temporary");
        let path = directory.snapshot();
        let temporary_path = directory.0.join(".state.snapshot.tmp");
        let store = SnapshotStore::new(32);

        fs::write(&temporary_path, b"another writer's bytes").expect("create unowned file");
        store.write(&path, b"current").expect("write snapshot");

        assert_eq!(store.read(&path).expect("read snapshot"), b"current");
        assert_eq!(
            fs::read(&temporary_path).expect("read unowned file"),
            b"another writer's bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restricted_coordinator_does_not_block_an_unrelated_shared_directory_destination() {
        let directory = TestDirectory::new("restricted-unrelated-coordinator");
        set_unix_mode(&directory.0, 0o777);
        let blocked_path = directory.0.join("other-user.snapshot");
        let writable_path = directory.0.join("current-user.snapshot");
        let blocked_id = destination_id(blocked_path.file_name().unwrap());
        let writable_id = destination_id(writable_path.file_name().unwrap());
        assert_ne!(blocked_id, writable_id);

        let restricted_path = directory.0.join(coordinator_file_name(blocked_id));
        fs::write(&restricted_path, b"").expect("create another destination's coordinator");
        set_unix_mode(&restricted_path, 0o000);

        let store = SnapshotStore::new(32);
        store
            .write(&writable_path, b"independent")
            .expect("write an unrelated snapshot");

        assert_eq!(
            store.read(&writable_path).expect("read unrelated snapshot"),
            b"independent"
        );
        assert!(restricted_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn restricted_temporary_does_not_block_an_unrelated_shared_directory_destination() {
        let directory = TestDirectory::new("restricted-unrelated-temporary");
        set_unix_mode(&directory.0, 0o777);
        let blocked_path = directory.0.join("other-user.snapshot");
        let writable_path = directory.0.join("current-user.snapshot");
        let blocked_id = destination_id(blocked_path.file_name().unwrap());
        let writable_id = destination_id(writable_path.file_name().unwrap());
        assert_ne!(blocked_id, writable_id);

        let restricted_path = directory
            .0
            .join(temporary_file_name(blocked_id, 0xdead_beef, 0));
        fs::write(&restricted_path, b"another writer's incomplete snapshot")
            .expect("create another destination's temporary file");
        set_unix_mode(&restricted_path, 0o000);

        let store = SnapshotStore::new(32);
        store
            .write(&writable_path, b"independent")
            .expect("write an unrelated snapshot");

        assert_eq!(
            store.read(&writable_path).expect("read unrelated snapshot"),
            b"independent"
        );
        assert!(restricted_path.exists());
    }

    #[test]
    fn rejects_the_reserved_internal_namespace_without_touching_existing_files() {
        let directory = TestDirectory::new("reserved-namespace");
        let store = SnapshotStore::new(32);

        for file_name in [
            ".rhsnap-00000000000000000000000000000000-deadbeef-0000000000000000.tmp",
            ".rhsnap-00000000000000000000000000000000.lock",
            ".rhsnap-future-protocol-file",
            ".RHSNAP-case-insensitive-filesystem-collision",
        ] {
            let path = directory.0.join(file_name);
            fs::write(&path, b"unrelated data").expect("create reserved-name file");

            let write_error = store.write(&path, b"snapshot").unwrap_err();
            assert!(
                matches!(write_error, SnapshotError::ReservedPath { path: found } if found == path)
            );
            let read_error = store.read(&path).unwrap_err();
            assert!(
                matches!(read_error, SnapshotError::ReservedPath { path: found } if found == path)
            );
            assert_eq!(
                fs::read(&path).expect("read reserved-name file"),
                b"unrelated data"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_names_in_the_reserved_internal_namespace() {
        use std::os::unix::ffi::OsStringExt;

        let directory = TestDirectory::new("non-utf8-reserved-namespace");
        let store = SnapshotStore::new(32);
        let path = directory
            .0
            .join(std::ffi::OsString::from_vec(b".rhsnap-\xff".to_vec()));

        assert!(
            matches!(store.write(&path, b"snapshot"), Err(SnapshotError::ReservedPath { path: found }) if found == path)
        );
        assert!(
            matches!(store.read(&path), Err(SnapshotError::ReservedPath { path: found }) if found == path)
        );
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("read test directory")
                .count(),
            0
        );
    }

    #[test]
    fn reclaims_a_temporary_file_left_by_an_interrupted_process() {
        let directory = TestDirectory::new("interrupted-writer");
        leave_interrupted_writer_temporary(&directory);

        let path = directory.snapshot();
        let store = SnapshotStore::new(32);
        store.write(&path, b"recovered").expect("recover snapshot");

        assert_eq!(store.read(&path).expect("reopen snapshot"), b"recovered");
        assert!(snapshot_temporary_files(&directory.0).is_empty());
    }

    #[test]
    fn reclaims_an_interrupted_temporary_when_the_next_path_uses_different_case() {
        let directory = TestDirectory::new("interrupted-writer-case-alias");
        leave_interrupted_writer_temporary(&directory);

        let original_path = directory.snapshot();
        let alias_path = directory.0.join("STATE.SNAPSHOT");
        assert_ne!(
            destination_id(original_path.file_name().unwrap()),
            destination_id(alias_path.file_name().unwrap())
        );

        let store = SnapshotStore::new(32);
        store
            .write(&alias_path, b"recovered through alias")
            .expect("recover through differently cased path");

        assert_eq!(
            store.read(&alias_path).expect("reopen alias snapshot"),
            b"recovered through alias"
        );
        assert!(snapshot_temporary_files(&directory.0).is_empty());
    }

    #[test]
    fn interrupted_writer_child_process() {
        let Some(directory) = std::env::var_os(INTERRUPTED_WRITER_DIRECTORY) else {
            return;
        };
        let directory = PathBuf::from(directory);
        let path = directory.join("state.snapshot");
        let (_, mut temporary) = create_sibling_temporary_file(&path).expect("create temporary");
        let payload = b"never published";
        temporary
            .file_mut()
            .write_all(&encode_header(payload.len() as u64, crc32(payload)))
            .and_then(|()| temporary.file_mut().write_all(payload))
            .expect("write temporary");
        temporary.file_mut().sync_all().expect("sync temporary");
        fs::write(directory.join("child.ready"), b"ready").expect("signal parent process");
        thread::sleep(Duration::from_secs(60));
        panic!("parent did not interrupt child process");
    }

    #[test]
    fn supports_a_valid_destination_name_near_the_filesystem_limit() {
        let directory = TestDirectory::new("long-destination-name");
        let path = directory.0.join("s".repeat(245));
        let store = SnapshotStore::new(32);

        store.write(&path, b"long name").expect("write snapshot");

        assert_eq!(store.read(&path).expect("reopen snapshot"), b"long name");
        assert!(snapshot_temporary_files(&directory.0).is_empty());
    }

    #[test]
    fn concurrent_writers_publish_only_complete_snapshots_and_leave_no_temporary_files() {
        const WRITER_COUNT: usize = 8;
        const WRITES_PER_WRITER: usize = 8;
        const PAYLOAD_LEN: usize = 128 * 1024;

        let directory = TestDirectory::new("concurrent-writers");
        let path = Arc::new(directory.snapshot());
        let store = SnapshotStore::new(PAYLOAD_LEN);
        let payloads = Arc::new(
            (0..WRITER_COUNT)
                .flat_map(|writer| {
                    (0..WRITES_PER_WRITER)
                        .map(move |write| test_payload(writer, write, PAYLOAD_LEN))
                })
                .collect::<Vec<_>>(),
        );
        store
            .write(path.as_ref(), &payloads[0])
            .expect("write initial snapshot");

        let start = Arc::new(Barrier::new(WRITER_COUNT + 1));
        let reading = Arc::new(AtomicBool::new(true));
        let reader = {
            let path = Arc::clone(&path);
            let payloads = Arc::clone(&payloads);
            let reading = Arc::clone(&reading);
            thread::spawn(move || {
                let store = SnapshotStore::new(PAYLOAD_LEN);
                while reading.load(Ordering::Acquire) {
                    let reopened = store.read(path.as_ref()).expect("reopen snapshot");
                    assert_complete_test_payload(&reopened, &payloads, WRITES_PER_WRITER);
                    thread::yield_now();
                }
            })
        };

        let writers = (0..WRITER_COUNT)
            .map(|writer| {
                let path = Arc::clone(&path);
                let payloads = Arc::clone(&payloads);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    let store = SnapshotStore::new(PAYLOAD_LEN);
                    start.wait();
                    for write in 0..WRITES_PER_WRITER {
                        let payload = &payloads[writer * WRITES_PER_WRITER + write];
                        store.write(path.as_ref(), payload).expect("write snapshot");
                        let reopened = store.read(path.as_ref()).expect("reopen snapshot");
                        assert_complete_test_payload(&reopened, &payloads, WRITES_PER_WRITER);
                    }
                })
            })
            .collect::<Vec<_>>();

        start.wait();
        let writer_results = writers
            .into_iter()
            .map(thread::JoinHandle::join)
            .collect::<Vec<_>>();
        reading.store(false, Ordering::Release);
        reader.join().expect("reader thread succeeds");
        for result in writer_results {
            result.expect("writer thread succeeds");
        }

        let reopened = store.read(path.as_ref()).expect("reopen final snapshot");
        assert_complete_test_payload(&reopened, &payloads, WRITES_PER_WRITER);
        assert!(snapshot_temporary_files(&directory.0).is_empty());
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
        assert!(snapshot_temporary_files(&directory.0).is_empty());
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

    fn test_payload(writer: usize, write: usize, len: usize) -> Vec<u8> {
        let mut payload = vec![(writer as u8).wrapping_mul(31).wrapping_add(write as u8); len];
        payload[..8].copy_from_slice(&(writer as u64).to_le_bytes());
        payload[8..16].copy_from_slice(&(write as u64).to_le_bytes());
        payload
    }

    fn assert_complete_test_payload(
        payload: &[u8],
        expected: &[Vec<u8>],
        writes_per_writer: usize,
    ) {
        assert!(payload.len() >= 16, "payload contains its writer identity");
        let writer = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
        let write = u64::from_le_bytes(payload[8..16].try_into().unwrap()) as usize;
        let expected_payload = expected
            .get(writer * writes_per_writer + write)
            .expect("payload identifies a known write");
        assert_eq!(payload, expected_payload);
    }

    fn snapshot_temporary_files(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("read test directory")
            .map(|entry| entry.expect("read directory entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(is_snapshot_temporary_file_name)
            })
            .collect()
    }

    fn leave_interrupted_writer_temporary(directory: &TestDirectory) {
        let ready_path = directory.0.join("child.ready");
        let mut child = Command::new(std::env::current_exe().expect("locate test executable"))
            .args([
                "--exact",
                "snapshot::tests::interrupted_writer_child_process",
            ])
            .env(INTERRUPTED_WRITER_DIRECTORY, &directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start interrupted writer child");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready_path.exists() {
            if let Some(status) = child.try_wait().expect("inspect child process") {
                panic!("interrupted writer exited before it was killed: {status}");
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill timed-out child process");
                child.wait().expect("reap timed-out child process");
                panic!("interrupted writer did not become ready");
            }
            thread::sleep(Duration::from_millis(10));
        }

        child.kill().expect("kill writer process");
        let status = child.wait().expect("reap writer process");
        assert!(!status.success());
        fs::remove_file(&ready_path).expect("remove child readiness marker");
        assert_eq!(snapshot_temporary_files(&directory.0).len(), 1);
    }

    #[cfg(unix)]
    fn set_unix_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .expect("read test permissions")
            .permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).expect("set test permissions");
    }
}
