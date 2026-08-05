//! Versioned, bounded snapshot envelopes and nullable `Int64` row payloads.
//!
//! This module does not serialize a catalog. It can create and sync a new
//! envelope file, atomically replace an envelope through a sibling temporary
//! file on supported Unix targets other than Solaris, then reopen one bounded
//! `Int64` table from that file. See
//! `docs/snapshot-format.md` for the stable binary layouts.

use std::error::Error;
#[cfg(all(unix, not(target_os = "solaris")))]
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(all(unix, not(target_os = "solaris")))]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(all(unix, not(target_os = "solaris")))]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(all(unix, not(target_os = "solaris")))]
use std::sync::atomic::{AtomicU32, Ordering};

use crate::storage::{InsertError, Int64Table, Schema};

/// Magic bytes at the start of every RustHouse snapshot envelope.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"RHOUSESN";

/// The snapshot envelope version emitted and accepted by this crate.
pub const SNAPSHOT_VERSION: u16 = 1;

/// Number of bytes before the snapshot payload.
pub const SNAPSHOT_HEADER_LEN: usize = SNAPSHOT_MAGIC.len()
    + std::mem::size_of::<u16>()
    + std::mem::size_of::<u64>()
    + std::mem::size_of::<u32>();

const VERSION_OFFSET: usize = SNAPSHOT_MAGIC.len();
const LENGTH_OFFSET: usize = VERSION_OFFSET + std::mem::size_of::<u16>();
const CHECKSUM_OFFSET: usize = LENGTH_OFFSET + std::mem::size_of::<u64>();

#[cfg(all(unix, not(target_os = "solaris")))]
const TEMPORARY_CREATE_ATTEMPTS: usize = 128;
#[cfg(all(unix, not(target_os = "solaris")))]
static NEXT_TEMPORARY_FILE: AtomicU32 = AtomicU32::new(0);

/// Number of bytes in the nullable `Int64` payload row-count field.
pub const NULLABLE_I64_PAYLOAD_HEADER_LEN: usize = std::mem::size_of::<u64>();

/// Tag identifying a `NULL` row in a nullable `Int64` payload.
pub const NULLABLE_I64_NULL_TAG: u8 = 0;

/// Tag identifying a present value in a nullable `Int64` payload.
pub const NULLABLE_I64_VALUE_TAG: u8 = 1;

/// An error produced while encoding or decoding a snapshot envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The payload exceeds the codec's configured byte bound.
    PayloadTooLarge {
        payload_len: u64,
        max_payload_len: usize,
    },
    /// The input ends before the complete header or declared payload.
    Truncated {
        expected_len: usize,
        actual_len: usize,
    },
    /// The input is not a RustHouse snapshot envelope.
    IncompatibleMagic { found: [u8; SNAPSHOT_MAGIC.len()] },
    /// The input uses an envelope version this codec cannot read.
    UnsupportedVersion { found: u16, supported: u16 },
    /// Bytes remain after the payload boundary declared by the header.
    TrailingBytes {
        expected_len: usize,
        actual_len: usize,
    },
    /// The payload does not match the checksum stored in the header.
    ChecksumMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge {
                payload_len,
                max_payload_len,
            } => write!(
                formatter,
                "snapshot payload has {payload_len} bytes, exceeding the limit of {max_payload_len}"
            ),
            Self::Truncated {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "snapshot is truncated: expected {expected_len} bytes, found {actual_len}"
            ),
            Self::IncompatibleMagic { found } => {
                write!(formatter, "incompatible snapshot magic bytes: {found:02x?}")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported snapshot version {found}; this codec supports version {supported}"
            ),
            Self::TrailingBytes {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "snapshot has trailing bytes: expected {expected_len} bytes, found {actual_len}"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "snapshot checksum mismatch: expected {expected:08x}, calculated {actual:08x}"
            ),
        }
    }
}

impl Error for SnapshotError {}

/// An error produced while creating a new snapshot envelope file.
#[derive(Debug)]
pub enum SnapshotFileError {
    /// The payload could not be encoded before filesystem access began.
    Encode(SnapshotError),
    /// The destination could not be exclusively created.
    Create(io::Error),
    /// The complete encoded envelope could not be written.
    Write(io::Error),
    /// The newly written file could not be synchronized to storage.
    Sync(io::Error),
}

impl fmt::Display for SnapshotFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(formatter, "could not encode snapshot: {error}"),
            Self::Create(error) => write!(formatter, "could not create snapshot file: {error}"),
            Self::Write(error) => write!(formatter, "could not write snapshot file: {error}"),
            Self::Sync(error) => write!(formatter, "could not sync snapshot file: {error}"),
        }
    }
}

impl Error for SnapshotFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::Create(error) | Self::Write(error) | Self::Sync(error) => Some(error),
        }
    }
}

/// An error produced while atomically replacing a snapshot envelope file.
#[cfg(all(unix, not(target_os = "solaris")))]
#[derive(Debug)]
pub enum SnapshotReplaceError {
    /// The payload could not be encoded before filesystem access began.
    Encode(SnapshotError),
    /// The destination has no normal, unambiguous final path component.
    InvalidDestination,
    /// The destination's parent directory could not be opened for syncing.
    OpenDirectory(io::Error),
    /// The destination's parent directory could not be locked for replacement.
    LockDirectory(io::Error),
    /// The destination could not be inspected before a conditional replacement.
    InspectDestination(io::Error),
    /// The destination changed after it was selected for conditional replacement.
    DestinationChanged,
    /// A sibling temporary file could not be exclusively created.
    CreateTemporary(io::Error),
    /// The complete encoded envelope could not be written to the temporary file.
    WriteTemporary(io::Error),
    /// The temporary file could not be synchronized before the rename.
    SyncTemporary(io::Error),
    /// The synchronized temporary file could not be renamed over the destination.
    Rename(io::Error),
    /// A missing destination could not be published without replacement.
    Publish(io::Error),
    /// A temporary file could not be removed after an earlier failure.
    CleanupTemporary {
        /// The failure encountered while removing the temporary file.
        source: io::Error,
        /// The operation failure that triggered cleanup.
        operation: Box<SnapshotReplaceError>,
    },
    /// The destination was published, but its sibling temporary link remained.
    CleanupPublishedTemporary(io::Error),
    /// The parent directory could not be synchronized after a successful rename.
    ///
    /// The destination has already been replaced when this variant is returned,
    /// but the rename's durability after a system crash is uncertain.
    SyncDirectory(io::Error),
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl SnapshotReplaceError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// A `true` result means the new envelope is visible at the destination,
    /// while its durability after a system crash remains uncertain.
    pub const fn destination_was_replaced(&self) -> bool {
        matches!(
            self,
            Self::CleanupPublishedTemporary(_) | Self::SyncDirectory(_)
        )
    }

    /// Returns the operation error that preceded a temporary-file cleanup failure.
    pub fn operation_error(&self) -> Option<&Self> {
        match self {
            Self::CleanupTemporary { operation, .. } => Some(operation),
            _ => None,
        }
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl fmt::Display for SnapshotReplaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(formatter, "could not encode snapshot: {error}"),
            Self::InvalidDestination => write!(
                formatter,
                "snapshot destination has no normal, unambiguous file name"
            ),
            Self::OpenDirectory(error) => {
                write!(
                    formatter,
                    "could not open snapshot parent directory: {error}"
                )
            }
            Self::LockDirectory(error) => {
                write!(
                    formatter,
                    "could not lock snapshot parent directory: {error}"
                )
            }
            Self::InspectDestination(error) => {
                write!(formatter, "could not inspect snapshot destination: {error}")
            }
            Self::DestinationChanged => write!(
                formatter,
                "snapshot destination changed before conditional replacement"
            ),
            Self::CreateTemporary(error) => {
                write!(
                    formatter,
                    "could not create temporary snapshot file: {error}"
                )
            }
            Self::WriteTemporary(error) => {
                write!(
                    formatter,
                    "could not write temporary snapshot file: {error}"
                )
            }
            Self::SyncTemporary(error) => {
                write!(formatter, "could not sync temporary snapshot file: {error}")
            }
            Self::Rename(error) => {
                write!(formatter, "could not replace snapshot file: {error}")
            }
            Self::Publish(error) => {
                write!(formatter, "could not publish snapshot file: {error}")
            }
            Self::CleanupTemporary { source, operation } => write!(
                formatter,
                "could not clean up temporary snapshot file after {operation}: {source}"
            ),
            Self::CleanupPublishedTemporary(error) => write!(
                formatter,
                "snapshot was published, but its temporary link could not be removed: {error}"
            ),
            Self::SyncDirectory(error) => write!(
                formatter,
                "snapshot was replaced, but its parent directory could not be synced: {error}"
            ),
        }
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl Error for SnapshotReplaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::InvalidDestination | Self::DestinationChanged => None,
            Self::OpenDirectory(error)
            | Self::LockDirectory(error)
            | Self::InspectDestination(error)
            | Self::CreateTemporary(error)
            | Self::WriteTemporary(error)
            | Self::SyncTemporary(error)
            | Self::Rename(error)
            | Self::Publish(error)
            | Self::CleanupPublishedTemporary(error)
            | Self::SyncDirectory(error) => Some(error),
            Self::CleanupTemporary { source, .. } => Some(source),
        }
    }
}

/// An error produced while encoding or decoding nullable `Int64` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NullableI64PayloadError {
    /// The row count exceeds the payload codec's configured bound.
    RowLimitExceeded { row_count: u64, max_rows: usize },
    /// The encoded payload exceeds the payload codec's configured byte bound.
    PayloadTooLarge {
        payload_len: u64,
        max_payload_len: usize,
    },
    /// The payload ends before the declared rows are complete.
    Truncated {
        expected_len: usize,
        actual_len: usize,
    },
    /// A row uses a tag that is not defined by the payload format.
    InvalidTag { row_index: usize, tag: u8 },
    /// Bytes remain after the declared rows have been decoded.
    TrailingData {
        expected_len: usize,
        actual_len: usize,
    },
}

impl fmt::Display for NullableI64PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowLimitExceeded {
                row_count,
                max_rows,
            } => write!(
                formatter,
                "nullable Int64 payload has {row_count} rows, exceeding the limit of {max_rows}"
            ),
            Self::PayloadTooLarge {
                payload_len,
                max_payload_len,
            } => write!(
                formatter,
                "nullable Int64 payload has {payload_len} bytes, exceeding the limit of {max_payload_len}"
            ),
            Self::Truncated {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "nullable Int64 payload is truncated: expected at least {expected_len} bytes, found {actual_len}"
            ),
            Self::InvalidTag { row_index, tag } => write!(
                formatter,
                "nullable Int64 payload row {row_index} has invalid tag {tag:#04x}"
            ),
            Self::TrailingData {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "nullable Int64 payload has trailing data: expected {expected_len} bytes, found {actual_len}"
            ),
        }
    }
}

impl Error for NullableI64PayloadError {}

/// An error produced while restoring an [`Int64Table`] from an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64TableRestoreError {
    /// The snapshot envelope could not be decoded.
    Envelope(SnapshotError),
    /// The envelope payload was not a valid nullable `Int64` row payload.
    Payload(NullableI64PayloadError),
    /// The decoded rows violated the requested schema or table row cap.
    Table(InsertError),
}

impl fmt::Display for Int64TableRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => {
                write!(formatter, "could not decode snapshot envelope: {error}")
            }
            Self::Payload(error) => write!(formatter, "could not decode table payload: {error}"),
            Self::Table(error) => write!(formatter, "could not restore table rows: {error}"),
        }
    }
}

impl Error for Int64TableRestoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::Table(error) => Some(error),
        }
    }
}

impl From<SnapshotError> for Int64TableRestoreError {
    fn from(error: SnapshotError) -> Self {
        Self::Envelope(error)
    }
}

impl From<NullableI64PayloadError> for Int64TableRestoreError {
    fn from(error: NullableI64PayloadError) -> Self {
        Self::Payload(error)
    }
}

impl From<InsertError> for Int64TableRestoreError {
    fn from(error: InsertError) -> Self {
        Self::Table(error)
    }
}

/// An error produced while restoring an [`Int64Table`] from a snapshot file.
#[derive(Debug)]
pub enum Int64TableFileRestoreError {
    /// The snapshot path could not be opened for reading.
    Open(io::Error),
    /// The snapshot path does not identify a regular file.
    NotRegularFile,
    /// The opened snapshot file could not be inspected or read completely.
    Read(io::Error),
    /// The file is larger than the envelope header plus the payload limit.
    FileTooLarge { file_len: u64, max_file_len: usize },
    /// The bounded file contents could not be restored as an `Int64` table.
    Restore(Int64TableRestoreError),
}

impl fmt::Display for Int64TableFileRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "could not open snapshot file: {error}"),
            Self::NotRegularFile => write!(formatter, "snapshot path is not a regular file"),
            Self::Read(error) => write!(formatter, "could not read snapshot file: {error}"),
            Self::FileTooLarge {
                file_len,
                max_file_len,
            } => write!(
                formatter,
                "snapshot file has {file_len} bytes, exceeding the limit of {max_file_len}"
            ),
            Self::Restore(error) => write!(formatter, "could not restore snapshot file: {error}"),
        }
    }
}

impl Error for Int64TableFileRestoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(error) | Self::Read(error) => Some(error),
            Self::NotRegularFile | Self::FileTooLarge { .. } => None,
            Self::Restore(error) => Some(error),
        }
    }
}

impl From<Int64TableRestoreError> for Int64TableFileRestoreError {
    fn from(error: Int64TableRestoreError) -> Self {
        Self::Restore(error)
    }
}

/// Identifies the snapshot file that produced a recovered table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Int64TableFileRecoverySource {
    /// The caller-supplied primary snapshot was valid.
    Primary,
    /// The primary failed validation and the caller-supplied backup was valid.
    Backup,
}

/// A table recovered from either a primary or explicit backup snapshot file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Int64TableFileRecovery {
    table: Int64Table,
    source: Int64TableFileRecoverySource,
}

impl Int64TableFileRecovery {
    /// Returns the completely validated recovered table.
    pub fn table(&self) -> &Int64Table {
        &self.table
    }

    /// Returns which caller-supplied snapshot file produced the table.
    pub const fn source(&self) -> Int64TableFileRecoverySource {
        self.source
    }

    /// Consumes the recovery result and returns its table.
    pub fn into_table(self) -> Int64Table {
        self.table
    }

    /// Consumes the recovery result and returns its table and source.
    pub fn into_parts(self) -> (Int64Table, Int64TableFileRecoverySource) {
        (self.table, self.source)
    }
}

/// An error produced when both primary and backup snapshot restoration fail.
#[derive(Debug)]
pub enum Int64TableFileRecoveryError {
    /// Both bounded file restoration attempts failed.
    BothFailed {
        /// The typed failure from the primary snapshot.
        primary: Int64TableFileRestoreError,
        /// The typed failure from the backup snapshot.
        backup: Int64TableFileRestoreError,
    },
}

impl Int64TableFileRecoveryError {
    /// Returns the typed failure from the primary snapshot.
    pub const fn primary_error(&self) -> &Int64TableFileRestoreError {
        match self {
            Self::BothFailed { primary, .. } => primary,
        }
    }

    /// Returns the typed failure from the backup snapshot.
    pub const fn backup_error(&self) -> &Int64TableFileRestoreError {
        match self {
            Self::BothFailed { backup, .. } => backup,
        }
    }

    /// Consumes this error and returns both typed file restoration failures.
    pub fn into_errors(self) -> (Int64TableFileRestoreError, Int64TableFileRestoreError) {
        match self {
            Self::BothFailed { primary, backup } => (primary, backup),
        }
    }
}

impl fmt::Display for Int64TableFileRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BothFailed { primary, backup } => write!(
                formatter,
                "could not restore primary snapshot ({primary}) or backup snapshot ({backup})"
            ),
        }
    }
}

impl Error for Int64TableFileRecoveryError {}

/// An error produced while restoring and repairing a primary snapshot file.
#[cfg(all(unix, not(target_os = "solaris")))]
#[derive(Debug)]
pub enum Int64TableFileRepairError {
    /// Neither the primary nor the backup could be restored within the bounds.
    BothFailed {
        /// The typed failure from the primary snapshot.
        primary: Int64TableFileRestoreError,
        /// The typed failure from the backup snapshot.
        backup: Int64TableFileRestoreError,
    },
    /// The backup was valid, but atomically replacing the primary failed.
    RepairFailed {
        /// The typed failure that caused recovery from the backup.
        primary: Int64TableFileRestoreError,
        /// The failure from atomically replacing the primary snapshot.
        repair: SnapshotReplaceError,
    },
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl Int64TableFileRepairError {
    /// Returns the typed failure from the initial primary restoration.
    pub const fn primary_error(&self) -> &Int64TableFileRestoreError {
        match self {
            Self::BothFailed { primary, .. } | Self::RepairFailed { primary, .. } => primary,
        }
    }

    /// Returns the backup restoration failure when both files were invalid.
    pub const fn backup_error(&self) -> Option<&Int64TableFileRestoreError> {
        match self {
            Self::BothFailed { backup, .. } => Some(backup),
            Self::RepairFailed { .. } => None,
        }
    }

    /// Returns the atomic replacement failure when backup restoration succeeded.
    pub const fn repair_error(&self) -> Option<&SnapshotReplaceError> {
        match self {
            Self::BothFailed { .. } => None,
            Self::RepairFailed { repair, .. } => Some(repair),
        }
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl fmt::Display for Int64TableFileRepairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BothFailed { primary, backup } => write!(
                formatter,
                "could not restore primary snapshot ({primary}) or backup snapshot ({backup})"
            ),
            Self::RepairFailed { primary, repair } => write!(
                formatter,
                "restored backup after primary snapshot failure ({primary}), but could not repair primary snapshot: {repair}"
            ),
        }
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl Error for Int64TableFileRepairError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BothFailed { .. } => None,
            Self::RepairFailed { repair, .. } => Some(repair),
        }
    }
}

/// Encodes and decodes bounded nullable `Int64` row payloads.
///
/// The payload can be passed directly to [`SnapshotCodec::encode`]. Decoding
/// checks both configured limits and the complete payload structure before
/// allocating the result vector.
///
/// # Examples
///
/// ```
/// use rusthouse::{NullableI64PayloadCodec, SnapshotCodec};
///
/// let rows = [Some(-7), None, Some(i64::MAX)];
/// let row_codec = NullableI64PayloadCodec::new(3, 27);
/// let snapshot_codec = SnapshotCodec::new(27);
///
/// let payload = row_codec.encode(&rows).unwrap();
/// let envelope = snapshot_codec.encode(&payload).unwrap();
/// let decoded_payload = snapshot_codec.decode(&envelope).unwrap();
///
/// assert_eq!(row_codec.decode(decoded_payload).unwrap(), rows);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullableI64PayloadCodec {
    max_rows: usize,
    max_payload_len: usize,
}

impl NullableI64PayloadCodec {
    /// Creates a codec with inclusive row-count and encoded-byte limits.
    pub const fn new(max_rows: usize, max_payload_len: usize) -> Self {
        Self {
            max_rows,
            max_payload_len,
        }
    }

    /// Returns the maximum row count accepted by this codec.
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Returns the maximum encoded payload size accepted by this codec.
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Encodes rows in deterministic input order.
    pub fn encode(self, rows: &[Option<i64>]) -> Result<Vec<u8>, NullableI64PayloadError> {
        let row_count =
            u64::try_from(rows.len()).map_err(|_| NullableI64PayloadError::RowLimitExceeded {
                row_count: u64::MAX,
                max_rows: self.max_rows,
            })?;
        if rows.len() > self.max_rows {
            return Err(NullableI64PayloadError::RowLimitExceeded {
                row_count,
                max_rows: self.max_rows,
            });
        }

        let payload_len = rows
            .iter()
            .try_fold(NULLABLE_I64_PAYLOAD_HEADER_LEN, |length, value| {
                let row_len = if value.is_some() {
                    1 + std::mem::size_of::<i64>()
                } else {
                    1
                };
                length.checked_add(row_len)
            });
        let Some(payload_len) = payload_len else {
            return Err(NullableI64PayloadError::PayloadTooLarge {
                payload_len: u64::MAX,
                max_payload_len: self.max_payload_len,
            });
        };
        if payload_len > self.max_payload_len {
            return Err(NullableI64PayloadError::PayloadTooLarge {
                payload_len: u64::try_from(payload_len).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }

        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&row_count.to_le_bytes());
        for value in rows {
            match value {
                None => payload.push(NULLABLE_I64_NULL_TAG),
                Some(value) => {
                    payload.push(NULLABLE_I64_VALUE_TAG);
                    payload.extend_from_slice(&value.to_le_bytes());
                }
            }
        }

        Ok(payload)
    }

    /// Validates and decodes a complete nullable `Int64` row payload.
    pub fn decode(self, payload: &[u8]) -> Result<Vec<Option<i64>>, NullableI64PayloadError> {
        if payload.len() > self.max_payload_len {
            return Err(NullableI64PayloadError::PayloadTooLarge {
                payload_len: u64::try_from(payload.len()).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }
        if payload.len() < NULLABLE_I64_PAYLOAD_HEADER_LEN {
            return Err(NullableI64PayloadError::Truncated {
                expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN,
                actual_len: payload.len(),
            });
        }

        let declared_rows = u64::from_le_bytes(read_array::<8>(payload, 0));
        let row_count = usize::try_from(declared_rows).map_err(|_| {
            NullableI64PayloadError::RowLimitExceeded {
                row_count: declared_rows,
                max_rows: self.max_rows,
            }
        })?;
        if row_count > self.max_rows {
            return Err(NullableI64PayloadError::RowLimitExceeded {
                row_count: declared_rows,
                max_rows: self.max_rows,
            });
        }

        let minimum_len = NULLABLE_I64_PAYLOAD_HEADER_LEN
            .checked_add(row_count)
            .ok_or(NullableI64PayloadError::PayloadTooLarge {
                payload_len: u64::MAX,
                max_payload_len: self.max_payload_len,
            })?;
        if minimum_len > self.max_payload_len {
            return Err(NullableI64PayloadError::PayloadTooLarge {
                payload_len: u64::try_from(minimum_len).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }
        if payload.len() < minimum_len {
            return Err(NullableI64PayloadError::Truncated {
                expected_len: minimum_len,
                actual_len: payload.len(),
            });
        }

        validate_nullable_i64_rows(payload, row_count)?;

        let mut rows = Vec::with_capacity(row_count);
        let mut offset = NULLABLE_I64_PAYLOAD_HEADER_LEN;
        for _ in 0..row_count {
            let tag = payload[offset];
            offset += 1;
            if tag == NULLABLE_I64_NULL_TAG {
                rows.push(None);
            } else {
                rows.push(Some(i64::from_le_bytes(read_array::<8>(payload, offset))));
                offset += std::mem::size_of::<i64>();
            }
        }

        Ok(rows)
    }
}

/// Encodes and decodes snapshot envelopes up to a fixed payload size.
///
/// Decoding returns a slice borrowed from the input and does not allocate.
/// The declared payload length is checked against the configured bound before
/// the payload is accessed or checksummed.
///
/// # Examples
///
/// ```
/// use rusthouse::SnapshotCodec;
///
/// let codec = SnapshotCodec::new(1024);
/// let encoded = codec.encode(b"catalog bytes")?;
/// let decoded = codec.decode(&encoded)?;
///
/// assert_eq!(decoded, b"catalog bytes");
/// # Ok::<(), rusthouse::SnapshotError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCodec {
    max_payload_len: usize,
}

impl SnapshotCodec {
    /// Creates a codec with an inclusive payload-size limit in bytes.
    pub const fn new(max_payload_len: usize) -> Self {
        Self { max_payload_len }
    }

    /// Returns the maximum payload size accepted by this codec.
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Wraps a payload in a version 1 snapshot envelope.
    pub fn encode(self, payload: &[u8]) -> Result<Vec<u8>, SnapshotError> {
        let payload_len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if payload.len() > self.max_payload_len || payload_len == u64::MAX {
            return Err(SnapshotError::PayloadTooLarge {
                payload_len,
                max_payload_len: self.max_payload_len,
            });
        }

        let total_len = SNAPSHOT_HEADER_LEN.checked_add(payload.len()).ok_or(
            SnapshotError::PayloadTooLarge {
                payload_len,
                max_payload_len: self.max_payload_len,
            },
        )?;
        let checksum = crc32(payload);
        let mut envelope = Vec::with_capacity(total_len);

        envelope.extend_from_slice(&SNAPSHOT_MAGIC);
        envelope.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        envelope.extend_from_slice(&payload_len.to_le_bytes());
        envelope.extend_from_slice(&checksum.to_le_bytes());
        envelope.extend_from_slice(payload);

        Ok(envelope)
    }

    /// Encodes `payload`, then creates, writes, and syncs a new envelope file.
    ///
    /// Encoding and payload-size validation finish before the destination is
    /// opened. File creation is exclusive, so an existing destination is
    /// returned as a [`SnapshotFileError::Create`] error and is never replaced
    /// or truncated.
    pub fn create_new_file(
        self,
        path: impl AsRef<Path>,
        payload: &[u8],
    ) -> Result<(), SnapshotFileError> {
        let envelope = self.encode(payload).map_err(SnapshotFileError::Encode)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(SnapshotFileError::Create)?;

        file.write_all(&envelope)
            .map_err(SnapshotFileError::Write)?;
        file.sync_all().map_err(SnapshotFileError::Sync)
    }

    /// Atomically creates or replaces a snapshot envelope file on supported
    /// Unix targets other than Solaris.
    ///
    /// The payload is bounded and encoded before filesystem access. The
    /// envelope is written to an exclusively created sibling temporary file,
    /// which is synchronized and then renamed over `path`. Finally, the
    /// destination's parent directory is synchronized so the rename is durable.
    /// An exclusive advisory lock on the opened parent serializes replacements
    /// and repairs performed by this crate within that directory. This
    /// concurrency guarantee requires every replacing writer to use
    /// [`Self::replace_file`] or
    /// [`restore_and_repair_int64_table_from_file_with_backup`]. Direct
    /// filesystem writes and renames do not participate in the advisory lock
    /// and must not run concurrently with these operations.
    /// All operations after opening the parent are relative to that directory
    /// descriptor, so renaming or rebinding the parent path cannot redirect the
    /// operation or strand the temporary file.
    /// Temporary names extend the destination with a unique suffix and are
    /// checked by filesystem identity before writing. Any candidate that the
    /// filesystem resolves as the destination is removed and retried. Paths
    /// ending in `/` or `/.` are rejected rather than normalized to a different
    /// destination.
    ///
    /// Failures before the rename attempt to remove the temporary file and
    /// leave an existing destination unchanged. A
    /// [`SnapshotReplaceError::SyncDirectory`] failure occurs after the rename:
    /// the new destination is visible, but its durability after a system crash
    /// is uncertain.
    #[cfg(all(unix, not(target_os = "solaris")))]
    pub fn replace_file(
        self,
        path: impl AsRef<Path>,
        payload: &[u8],
    ) -> Result<(), SnapshotReplaceError> {
        self.replace_file_with_directory_sync(path.as_ref(), payload, SnapshotDirectory::sync)
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    fn replace_file_with_directory_sync(
        self,
        path: &Path,
        payload: &[u8],
        sync_directory: impl FnOnce(&SnapshotDirectory) -> io::Result<()>,
    ) -> Result<(), SnapshotReplaceError> {
        let envelope = self.encode(payload).map_err(SnapshotReplaceError::Encode)?;
        let destination = snapshot_destination_name(path)?;

        let parent = normalized_parent(path);
        let directory =
            SnapshotDirectory::open(parent).map_err(SnapshotReplaceError::OpenDirectory)?;
        let _lock = directory
            .lock_exclusive()
            .map_err(SnapshotReplaceError::LockDirectory)?;
        replace_envelope_in_directory_with_sync(&directory, &destination, &envelope, sync_directory)
    }

    /// Validates an envelope and returns its borrowed payload.
    pub fn decode(self, envelope: &[u8]) -> Result<&[u8], SnapshotError> {
        if envelope.len() < SNAPSHOT_HEADER_LEN {
            return Err(SnapshotError::Truncated {
                expected_len: SNAPSHOT_HEADER_LEN,
                actual_len: envelope.len(),
            });
        }

        let found_magic = read_array::<{ SNAPSHOT_MAGIC.len() }>(envelope, 0);
        if found_magic != SNAPSHOT_MAGIC {
            return Err(SnapshotError::IncompatibleMagic { found: found_magic });
        }

        let version = u16::from_le_bytes(read_array::<2>(envelope, VERSION_OFFSET));
        if version != SNAPSHOT_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                found: version,
                supported: SNAPSHOT_VERSION,
            });
        }

        let declared_len = u64::from_le_bytes(read_array::<8>(envelope, LENGTH_OFFSET));
        let payload_len =
            usize::try_from(declared_len).map_err(|_| SnapshotError::PayloadTooLarge {
                payload_len: declared_len,
                max_payload_len: self.max_payload_len,
            })?;
        if payload_len > self.max_payload_len {
            return Err(SnapshotError::PayloadTooLarge {
                payload_len: declared_len,
                max_payload_len: self.max_payload_len,
            });
        }

        let expected_len =
            SNAPSHOT_HEADER_LEN
                .checked_add(payload_len)
                .ok_or(SnapshotError::PayloadTooLarge {
                    payload_len: declared_len,
                    max_payload_len: self.max_payload_len,
                })?;
        if envelope.len() < expected_len {
            return Err(SnapshotError::Truncated {
                expected_len,
                actual_len: envelope.len(),
            });
        }
        if envelope.len() > expected_len {
            return Err(SnapshotError::TrailingBytes {
                expected_len,
                actual_len: envelope.len(),
            });
        }

        let expected_checksum = u32::from_le_bytes(read_array::<4>(envelope, CHECKSUM_OFFSET));
        let payload = &envelope[SNAPSHOT_HEADER_LEN..expected_len];
        let actual_checksum = crc32(payload);
        if actual_checksum != expected_checksum {
            return Err(SnapshotError::ChecksumMismatch {
                expected: expected_checksum,
                actual: actual_checksum,
            });
        }

        Ok(payload)
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn replace_envelope_in_directory_with_sync(
    directory: &SnapshotDirectory,
    destination: &CStr,
    envelope: &[u8],
    sync_directory: impl FnOnce(&SnapshotDirectory) -> io::Result<()>,
) -> Result<(), SnapshotReplaceError> {
    let (mut file, temporary) = create_temporary_snapshot(directory, destination)?;

    if let Err(error) = file.write_all(envelope) {
        drop(file);
        return Err(temporary.cleanup(SnapshotReplaceError::WriteTemporary(error)));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        return Err(temporary.cleanup(SnapshotReplaceError::SyncTemporary(error)));
    }
    drop(file);

    if let Err(error) = directory.rename(temporary.name(), destination) {
        return Err(temporary.cleanup(SnapshotReplaceError::Rename(error)));
    }
    temporary.persist();

    sync_directory(directory).map_err(SnapshotReplaceError::SyncDirectory)
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn replace_envelope_in_directory_if_unchanged(
    directory: &SnapshotDirectory,
    destination: &CStr,
    condition: SnapshotReplacementCondition<'_>,
    envelope: &[u8],
    before_publish: impl FnOnce(),
    sync_directory: impl FnOnce(&SnapshotDirectory) -> io::Result<()>,
) -> Result<(), SnapshotReplaceError> {
    let (mut file, temporary) = create_temporary_snapshot(directory, destination)?;

    if let Err(error) = file.write_all(envelope) {
        drop(file);
        return Err(temporary.cleanup(SnapshotReplaceError::WriteTemporary(error)));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        return Err(temporary.cleanup(SnapshotReplaceError::SyncTemporary(error)));
    }
    drop(file);

    before_publish();
    let current = match directory.destination_state(destination) {
        Ok(current) => current,
        Err(error) => {
            return Err(temporary.cleanup(SnapshotReplaceError::InspectDestination(error)));
        }
    };
    if current != condition.destination {
        return Err(temporary.cleanup(SnapshotReplaceError::DestinationChanged));
    }
    if let Some(expected_envelope) = condition.envelope {
        let contents_unchanged =
            read_bounded_snapshot_file_from_directory(directory, destination, condition.codec)
                .is_ok_and(|current_envelope| current_envelope == expected_envelope);
        if !contents_unchanged {
            return Err(temporary.cleanup(SnapshotReplaceError::DestinationChanged));
        }
    } else if condition.destination != SnapshotDestinationState::Missing
        && read_bounded_snapshot_file_from_directory(directory, destination, condition.codec)
            .is_ok()
    {
        // The initial entry could not be read as a bounded regular snapshot.
        // If the same entry now can be read (for example, a dangling symlink's
        // target was published), it no longer represents the failed attempt.
        return Err(temporary.cleanup(SnapshotReplaceError::DestinationChanged));
    }

    if condition.destination == SnapshotDestinationState::Missing {
        if let Err(error) = directory.link(temporary.name(), destination) {
            let operation = if error.kind() == io::ErrorKind::AlreadyExists {
                SnapshotReplaceError::DestinationChanged
            } else {
                SnapshotReplaceError::Publish(error)
            };
            return Err(temporary.cleanup(operation));
        }
        if let Err(error) = temporary.discard() {
            return Err(SnapshotReplaceError::CleanupPublishedTemporary(error));
        }
    } else {
        if let Err(error) = directory.rename(temporary.name(), destination) {
            return Err(temporary.cleanup(SnapshotReplaceError::Rename(error)));
        }
        temporary.persist();
    }

    sync_directory(directory).map_err(SnapshotReplaceError::SyncDirectory)
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn snapshot_destination_name(path: &Path) -> Result<CString, SnapshotReplaceError> {
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.ends_with(b"/") || path_bytes.ends_with(b"/.") {
        return Err(SnapshotReplaceError::InvalidDestination);
    }

    let name = match path.components().next_back() {
        Some(std::path::Component::Normal(name)) => name,
        _ => return Err(SnapshotReplaceError::InvalidDestination),
    };
    CString::new(name.as_bytes()).map_err(|_| SnapshotReplaceError::InvalidDestination)
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn normalized_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn create_temporary_snapshot<'a>(
    directory: &'a SnapshotDirectory,
    destination: &CStr,
) -> Result<(File, TemporarySnapshot<'a>), SnapshotReplaceError> {
    create_temporary_snapshot_with_counter(directory, destination, &NEXT_TEMPORARY_FILE)
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn create_temporary_snapshot_with_counter<'a>(
    directory: &'a SnapshotDirectory,
    destination: &CStr,
    next_temporary_file: &AtomicU32,
) -> Result<(File, TemporarySnapshot<'a>), SnapshotReplaceError> {
    for _ in 0..TEMPORARY_CREATE_ATTEMPTS {
        let suffix = next_temporary_snapshot_suffix(next_temporary_file);
        let preferred_name = temporary_snapshot_name(destination, &suffix);
        let created = match directory.create(&preferred_name) {
            Ok(file) => Ok((file, preferred_name)),
            Err(error) if error.raw_os_error() == Some(libc::ENAMETOOLONG) => {
                if suffix.as_c_str() == destination {
                    continue;
                }
                directory.create(&suffix).map(|file| (file, suffix))
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SnapshotReplaceError::CreateTemporary(error)),
        };
        let (file, name) = match created {
            Ok(created) => created,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SnapshotReplaceError::CreateTemporary(error)),
        };
        let temporary = TemporarySnapshot::new(directory, name);

        match directory.entry_aliases_file(destination, &file) {
            Ok(false) => return Ok((file, temporary)),
            Ok(true) => {
                drop(file);
                if let Err(source) = temporary.discard() {
                    return Err(SnapshotReplaceError::CleanupTemporary {
                        source,
                        operation: Box::new(SnapshotReplaceError::CreateTemporary(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "temporary snapshot name aliases the destination",
                        ))),
                    });
                }
            }
            Err(error) => {
                drop(file);
                return Err(temporary.cleanup(SnapshotReplaceError::CreateTemporary(error)));
            }
        }
    }

    Err(SnapshotReplaceError::CreateTemporary(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not find an unused temporary snapshot name",
    )))
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn next_temporary_snapshot_suffix(next_temporary_file: &AtomicU32) -> CString {
    let sequence = next_temporary_file.fetch_add(1, Ordering::Relaxed);
    let name = format!(".rusthouse-snapshot-{}-{sequence}.tmp", std::process::id());
    CString::new(name).expect("generated snapshot names never contain NUL bytes")
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn temporary_snapshot_name(destination: &CStr, suffix: &CStr) -> CString {
    let mut name = Vec::with_capacity(destination.to_bytes().len() + suffix.to_bytes().len());
    name.extend_from_slice(destination.to_bytes());
    name.extend_from_slice(suffix.to_bytes());
    CString::new(name).expect("validated destination and generated suffix contain no NUL bytes")
}

#[cfg(all(unix, not(target_os = "solaris")))]
struct SnapshotDirectory {
    file: File,
}

#[cfg(all(unix, not(target_os = "solaris")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotDestinationState {
    Missing,
    Present {
        device: libc::dev_t,
        inode: libc::ino_t,
        mode: libc::mode_t,
        size: libc::off_t,
    },
}

#[cfg(all(unix, not(target_os = "solaris")))]
struct SnapshotReplacementCondition<'a> {
    destination: SnapshotDestinationState,
    envelope: Option<&'a [u8]>,
    codec: SnapshotCodec,
}

#[cfg(all(unix, not(target_os = "solaris")))]
struct SnapshotRepairHooks<BeforePublish, SyncDirectory> {
    before_publish: BeforePublish,
    sync_directory: SyncDirectory,
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl SnapshotDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self { file })
    }

    #[cfg(not(target_os = "solaris"))]
    fn lock_exclusive(&self) -> io::Result<SnapshotDirectoryLock<'_>> {
        loop {
            // SAFETY: `self.file` remains open for the lifetime of the returned
            // guard, and `flock` does not take ownership of the descriptor.
            let result = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(SnapshotDirectoryLock { directory: self });
            }

            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    #[cfg(all(test, not(target_os = "solaris")))]
    fn try_lock_exclusive(&self) -> io::Result<SnapshotDirectoryLock<'_>> {
        // SAFETY: `self.file` remains open for the lifetime of the returned
        // guard, and `flock` does not take ownership of the descriptor.
        let result = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(SnapshotDirectoryLock { directory: self })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn destination_state(&self, name: &CStr) -> io::Result<SnapshotDestinationState> {
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `status` points to writable storage, `name` is
        // NUL-terminated, and the directory descriptor remains open.
        let result = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(SnapshotDestinationState::Missing)
            } else {
                Err(error)
            };
        }

        // SAFETY: the successful `fstatat` call completely initialized
        // `status`.
        let status = unsafe { status.assume_init() };
        Ok(SnapshotDestinationState::Present {
            device: status.st_dev,
            inode: status.st_ino,
            mode: status.st_mode,
            size: status.st_size,
        })
    }

    fn open_read(&self, name: &CStr) -> io::Result<File> {
        // SAFETY: `self.file` is an open directory, `name` is NUL-terminated,
        // and the returned descriptor is checked before ownership is assumed.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `openat` returned a new owned descriptor on success.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn create(&self, name: &CStr) -> io::Result<File> {
        // SAFETY: `self.file` is an open directory, `name` is NUL-terminated,
        // and the returned descriptor is checked before ownership is assumed.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                libc::c_uint::from(0o666_u16),
            )
        };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `openat` returned a new owned descriptor on success.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn entry_aliases_file(&self, name: &CStr, file: &File) -> io::Result<bool> {
        let mut file_status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `file_status` points to writable storage and `file` remains
        // open while `fstat` initializes it.
        let file_result = unsafe { libc::fstat(file.as_raw_fd(), file_status.as_mut_ptr()) };
        if file_result == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut entry_status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `entry_status` points to writable storage, `name` is
        // NUL-terminated, and the directory descriptor remains open.
        let entry_result = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                entry_status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if entry_result == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error)
            };
        }

        // SAFETY: both successful calls above completely initialized their
        // respective `stat` values.
        let (file_status, entry_status) =
            unsafe { (file_status.assume_init(), entry_status.assume_init()) };
        Ok(file_status.st_dev == entry_status.st_dev && file_status.st_ino == entry_status.st_ino)
    }

    fn rename(&self, source: &CStr, destination: &CStr) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and both directory descriptors
        // remain open for the duration of the call.
        let result = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                destination.as_ptr(),
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn link(&self, source: &CStr, destination: &CStr) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and both directory descriptors
        // remain open for the duration of this directory-relative hard link.
        let result = unsafe {
            libc::linkat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                destination.as_ptr(),
                0,
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn remove(&self, name: &CStr) -> io::Result<()> {
        // SAFETY: `name` is NUL-terminated and `self.file` remains open for the
        // duration of this directory-relative unlink operation.
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
struct SnapshotDirectoryLock<'a> {
    directory: &'a SnapshotDirectory,
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl Drop for SnapshotDirectoryLock<'_> {
    fn drop(&mut self) {
        // SAFETY: the borrowed directory remains open while the guard exists,
        // and unlocking does not take ownership of its descriptor.
        let _ = unsafe { libc::flock(self.directory.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
struct TemporarySnapshot<'a> {
    directory: &'a SnapshotDirectory,
    name: CString,
    remove_on_drop: bool,
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl<'a> TemporarySnapshot<'a> {
    fn new(directory: &'a SnapshotDirectory, name: CString) -> Self {
        Self {
            directory,
            name,
            remove_on_drop: true,
        }
    }

    fn name(&self) -> &CStr {
        &self.name
    }

    fn cleanup(mut self, operation: SnapshotReplaceError) -> SnapshotReplaceError {
        self.remove_on_drop = false;
        match self.directory.remove(&self.name) {
            Ok(()) => operation,
            Err(error) if error.kind() == io::ErrorKind::NotFound => operation,
            Err(source) => SnapshotReplaceError::CleanupTemporary {
                source,
                operation: Box::new(operation),
            },
        }
    }

    fn discard(mut self) -> io::Result<()> {
        self.remove_on_drop = false;
        match self.directory.remove(&self.name) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn persist(mut self) {
        self.remove_on_drop = false;
    }
}

#[cfg(all(unix, not(target_os = "solaris")))]
impl Drop for TemporarySnapshot<'_> {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = self.directory.remove(&self.name);
        }
    }
}

/// Restores one bounded `Int64` table from a snapshot envelope.
///
/// The supplied codecs retain independent envelope-byte, payload-byte, and
/// payload-row bounds. The payload is fully decoded before a new table is
/// created, then all rows are appended in one atomic batch. Envelope, payload,
/// schema nullability, and table row-cap failures therefore remain typed, and
/// no partially populated table is returned.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     NullableI64PayloadCodec, Schema, SnapshotCodec, restore_int64_table,
/// };
///
/// let rows = [Some(-7), None, Some(i64::MAX)];
/// let payload_codec = NullableI64PayloadCodec::new(3, 27);
/// let snapshot_codec = SnapshotCodec::new(27);
/// let payload = payload_codec.encode(&rows)?;
/// let envelope = snapshot_codec.encode(&payload)?;
///
/// let table = restore_int64_table(
///     &envelope,
///     Schema::int64("reading", true),
///     3,
///     snapshot_codec,
///     payload_codec,
/// )?;
///
/// assert_eq!(table.values(), rows);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn restore_int64_table(
    envelope: &[u8],
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<Int64Table, Int64TableRestoreError> {
    let payload = snapshot_codec.decode(envelope)?;
    let rows = payload_codec.decode(payload)?;
    let mut table = Int64Table::new(schema, row_cap);
    table.append_batch(&rows)?;
    Ok(table)
}

/// Opens a bounded snapshot file and restores one `Int64` table from it.
///
/// At most the envelope header plus `snapshot_codec`'s configured payload
/// limit is read. The path must identify a regular file, preventing streams
/// and devices from blocking or hiding trailing input behind an unreliable
/// metadata length. A larger file is rejected before its contents are read.
/// The bounded bytes are passed to [`restore_int64_table`], so all envelope,
/// payload, schema, and row-cap validation finishes before a table is returned.
pub fn restore_int64_table_from_file(
    path: impl AsRef<Path>,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<Int64Table, Int64TableFileRestoreError> {
    let envelope = read_bounded_snapshot_file(path.as_ref(), snapshot_codec)?;

    restore_int64_table(&envelope, schema, row_cap, snapshot_codec, payload_codec)
        .map_err(Int64TableFileRestoreError::Restore)
}

fn read_bounded_snapshot_file(
    path: &Path,
    snapshot_codec: SnapshotCodec,
) -> Result<Vec<u8>, Int64TableFileRestoreError> {
    let file = open_regular_snapshot_file(path)?;
    read_bounded_snapshot_from_file(file, snapshot_codec)
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn read_bounded_snapshot_file_from_directory(
    directory: &SnapshotDirectory,
    name: &CStr,
    snapshot_codec: SnapshotCodec,
) -> Result<Vec<u8>, Int64TableFileRestoreError> {
    let file = directory
        .open_read(name)
        .map_err(Int64TableFileRestoreError::Open)?;
    let metadata = file.metadata().map_err(Int64TableFileRestoreError::Read)?;
    if !metadata.is_file() {
        return Err(Int64TableFileRestoreError::NotRegularFile);
    }

    read_bounded_snapshot_from_file(file, snapshot_codec)
}

fn read_bounded_snapshot_from_file(
    mut file: File,
    snapshot_codec: SnapshotCodec,
) -> Result<Vec<u8>, Int64TableFileRestoreError> {
    let max_file_len = SNAPSHOT_HEADER_LEN.saturating_add(snapshot_codec.max_payload_len());
    let file_len = bounded_file_len(&file, max_file_len)?;
    let capacity = usize::try_from(file_len).unwrap_or(max_file_len);
    let mut envelope = Vec::with_capacity(capacity);
    Read::take(&mut file, u64::try_from(max_file_len).unwrap_or(u64::MAX))
        .read_to_end(&mut envelope)
        .map_err(Int64TableFileRestoreError::Read)?;

    // Check the opened file again in case it grew after the first size check.
    bounded_file_len(&file, max_file_len)?;

    Ok(envelope)
}

/// Restores one bounded `Int64` table from a primary or explicit backup file.
///
/// The primary is attempted first. A valid primary is returned immediately and
/// the backup is not inspected. If the primary fails for any typed file,
/// envelope, payload, schema, or row-cap reason, the backup is restored with
/// the same bounds. A successful result reports which file supplied the table;
/// if both attempts fail, both typed failures are retained. Neither failure
/// exposes a partially decoded or populated table.
pub fn restore_int64_table_from_file_with_backup(
    primary_path: impl AsRef<Path>,
    backup_path: impl AsRef<Path>,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<Int64TableFileRecovery, Int64TableFileRecoveryError> {
    match restore_int64_table_from_file(
        primary_path,
        schema.clone(),
        row_cap,
        snapshot_codec,
        payload_codec,
    ) {
        Ok(table) => Ok(Int64TableFileRecovery {
            table,
            source: Int64TableFileRecoverySource::Primary,
        }),
        Err(primary) => {
            match restore_int64_table_from_file(
                backup_path,
                schema,
                row_cap,
                snapshot_codec,
                payload_codec,
            ) {
                Ok(table) => Ok(Int64TableFileRecovery {
                    table,
                    source: Int64TableFileRecoverySource::Backup,
                }),
                Err(backup) => Err(Int64TableFileRecoveryError::BothFailed { primary, backup }),
            }
        }
    }
}

/// Restores a bounded primary snapshot, repairing it from an explicit backup.
///
/// A valid primary is returned immediately without inspecting the backup. If
/// primary restoration fails, the backup is read once with the same file,
/// envelope, payload, schema, and row bounds. The primary's parent directory is
/// opened and locked before its initial restoration, and all primary access
/// remains relative to that directory descriptor. After the backup validates,
/// its envelope replaces the primary only if the initially observed directory
/// entry is unchanged. Calls to [`SnapshotCodec::replace_file`] use the same
/// advisory directory lock, so a cooperative concurrent refresh publishes only
/// after the repair completes. Direct filesystem writers do not participate in
/// that protocol and must not modify the primary concurrently. The backup is
/// never modified. Dual restoration failures and replacement-stage failures
/// remain distinct and retain their typed causes. A destination change observed
/// before publication is reported as [`SnapshotReplaceError::DestinationChanged`].
#[cfg(all(unix, not(target_os = "solaris")))]
pub fn restore_and_repair_int64_table_from_file_with_backup(
    primary_path: impl AsRef<Path>,
    backup_path: impl AsRef<Path>,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<Int64TableFileRecovery, Int64TableFileRepairError> {
    restore_and_repair_int64_table_from_file_with_backup_using(
        primary_path.as_ref(),
        backup_path.as_ref(),
        schema,
        row_cap,
        snapshot_codec,
        payload_codec,
        SnapshotRepairHooks {
            before_publish: || {},
            sync_directory: SnapshotDirectory::sync,
        },
    )
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn restore_and_repair_int64_table_from_file_with_backup_using(
    primary_path: &Path,
    backup_path: &Path,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
    hooks: SnapshotRepairHooks<impl FnOnce(), impl FnOnce(&SnapshotDirectory) -> io::Result<()>>,
) -> Result<Int64TableFileRecovery, Int64TableFileRepairError> {
    let destination = match snapshot_destination_name(primary_path) {
        Ok(destination) => destination,
        Err(repair) => {
            return restore_after_repair_setup_failure(
                primary_path,
                backup_path,
                schema,
                row_cap,
                snapshot_codec,
                payload_codec,
                repair,
            );
        }
    };
    let directory = match SnapshotDirectory::open(normalized_parent(primary_path)) {
        Ok(directory) => directory,
        Err(error) => {
            return restore_after_repair_setup_failure(
                primary_path,
                backup_path,
                schema,
                row_cap,
                snapshot_codec,
                payload_codec,
                SnapshotReplaceError::OpenDirectory(error),
            );
        }
    };
    let _lock = match directory.lock_exclusive() {
        Ok(lock) => lock,
        Err(error) => {
            return restore_after_repair_setup_failure(
                primary_path,
                backup_path,
                schema,
                row_cap,
                snapshot_codec,
                payload_codec,
                SnapshotReplaceError::LockDirectory(error),
            );
        }
    };
    let observed_primary = directory.destination_state(&destination);

    let primary_envelope =
        read_bounded_snapshot_file_from_directory(&directory, &destination, snapshot_codec);
    let (primary, observed_primary_envelope) = match primary_envelope {
        Ok(envelope) => match restore_int64_table(
            &envelope,
            schema.clone(),
            row_cap,
            snapshot_codec,
            payload_codec,
        ) {
            Ok(table) => {
                return Ok(Int64TableFileRecovery {
                    table,
                    source: Int64TableFileRecoverySource::Primary,
                });
            }
            Err(primary) => (Int64TableFileRestoreError::Restore(primary), Some(envelope)),
        },
        Err(primary) => (primary, None),
    };

    let backup_envelope = match read_bounded_snapshot_file(backup_path, snapshot_codec) {
        Ok(envelope) => envelope,
        Err(backup) => return Err(Int64TableFileRepairError::BothFailed { primary, backup }),
    };
    let table = match restore_int64_table(
        &backup_envelope,
        schema,
        row_cap,
        snapshot_codec,
        payload_codec,
    ) {
        Ok(table) => table,
        Err(backup) => {
            return Err(Int64TableFileRepairError::BothFailed {
                primary,
                backup: Int64TableFileRestoreError::Restore(backup),
            });
        }
    };

    let observed_primary = match observed_primary {
        Ok(observed_primary) => observed_primary,
        Err(error) => {
            return Err(Int64TableFileRepairError::RepairFailed {
                primary,
                repair: SnapshotReplaceError::InspectDestination(error),
            });
        }
    };
    replace_envelope_in_directory_if_unchanged(
        &directory,
        &destination,
        SnapshotReplacementCondition {
            destination: observed_primary,
            envelope: observed_primary_envelope.as_deref(),
            codec: snapshot_codec,
        },
        &backup_envelope,
        hooks.before_publish,
        hooks.sync_directory,
    )
    .map_err(|repair| Int64TableFileRepairError::RepairFailed { primary, repair })?;

    Ok(Int64TableFileRecovery {
        table,
        source: Int64TableFileRecoverySource::Backup,
    })
}

#[cfg(all(unix, not(target_os = "solaris")))]
fn restore_after_repair_setup_failure(
    primary_path: &Path,
    backup_path: &Path,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
    repair: SnapshotReplaceError,
) -> Result<Int64TableFileRecovery, Int64TableFileRepairError> {
    let primary = match restore_int64_table_from_file(
        primary_path,
        schema.clone(),
        row_cap,
        snapshot_codec,
        payload_codec,
    ) {
        Ok(table) => {
            return Ok(Int64TableFileRecovery {
                table,
                source: Int64TableFileRecoverySource::Primary,
            });
        }
        Err(primary) => primary,
    };

    match restore_int64_table_from_file(backup_path, schema, row_cap, snapshot_codec, payload_codec)
    {
        Ok(_) => Err(Int64TableFileRepairError::RepairFailed { primary, repair }),
        Err(backup) => Err(Int64TableFileRepairError::BothFailed { primary, backup }),
    }
}

fn open_regular_snapshot_file(path: &Path) -> Result<File, Int64TableFileRestoreError> {
    require_regular_snapshot_path(path)?;
    open_regular_snapshot_path(path)
}

fn require_regular_snapshot_path(path: &Path) -> Result<(), Int64TableFileRestoreError> {
    let metadata = fs::metadata(path).map_err(Int64TableFileRestoreError::Open)?;
    if !metadata.is_file() {
        return Err(Int64TableFileRestoreError::NotRegularFile);
    }

    Ok(())
}

fn open_regular_snapshot_path(path: &Path) -> Result<File, Int64TableFileRestoreError> {
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // A pathname that becomes a FIFO between the metadata check and open
        // must not block this process. This flag has no effect on regular files.
        options.custom_flags(libc::O_NONBLOCK);
    }

    let file = options
        .open(path)
        .map_err(Int64TableFileRestoreError::Open)?;
    let metadata = file.metadata().map_err(Int64TableFileRestoreError::Read)?;
    if !metadata.is_file() {
        return Err(Int64TableFileRestoreError::NotRegularFile);
    }

    Ok(file)
}

fn bounded_file_len(file: &File, max_file_len: usize) -> Result<u64, Int64TableFileRestoreError> {
    let metadata = file.metadata().map_err(Int64TableFileRestoreError::Read)?;
    if !metadata.is_file() {
        return Err(Int64TableFileRestoreError::NotRegularFile);
    }

    let file_len = metadata.len();
    let max_file_len_u64 = u64::try_from(max_file_len).unwrap_or(u64::MAX);
    if file_len > max_file_len_u64 {
        return Err(Int64TableFileRestoreError::FileTooLarge {
            file_len,
            max_file_len,
        });
    }

    Ok(file_len)
}

fn validate_nullable_i64_rows(
    payload: &[u8],
    row_count: usize,
) -> Result<(), NullableI64PayloadError> {
    let mut offset = NULLABLE_I64_PAYLOAD_HEADER_LEN;
    for row_index in 0..row_count {
        let tag_end = offset.saturating_add(1);
        let Some(&tag) = payload.get(offset) else {
            return Err(NullableI64PayloadError::Truncated {
                expected_len: tag_end,
                actual_len: payload.len(),
            });
        };
        offset = tag_end;

        match tag {
            NULLABLE_I64_NULL_TAG => {}
            NULLABLE_I64_VALUE_TAG => {
                let value_end = offset.saturating_add(std::mem::size_of::<i64>());
                if payload.len() < value_end {
                    return Err(NullableI64PayloadError::Truncated {
                        expected_len: value_end,
                        actual_len: payload.len(),
                    });
                }
                offset = value_end;
            }
            tag => {
                return Err(NullableI64PayloadError::InvalidTag { row_index, tag });
            }
        }
    }

    if payload.len() > offset {
        return Err(NullableI64PayloadError::TrailingData {
            expected_len: offset,
            actual_len: payload.len(),
        });
    }

    Ok(())
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut value = [0; N];
    value.copy_from_slice(&bytes[offset..offset + N]);
    value
}

// CRC-32/ISO-HDLC, written out here to keep the envelope dependency-free.
fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    for byte in bytes {
        checksum ^= u32::from(*byte);
        for _ in 0..8 {
            let low_bit_mask = 0_u32.wrapping_sub(checksum & 1);
            checksum = (checksum >> 1) ^ (0xedb8_8320 & low_bit_mask);
        }
    }
    !checksum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn temporary_name_cannot_alias_destination_under_ascii_case_folding() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-name-collision-tests");
        fs::create_dir_all(&base).unwrap();
        let root = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let directory = SnapshotDirectory::open(&root).unwrap();
        let next_temporary_file = AtomicU32::new(0);
        let destination =
            CString::new(format!(".RUSTHOUSE-SNAPSHOT-{}-0.TMP", std::process::id())).unwrap();

        let (file, temporary) =
            create_temporary_snapshot_with_counter(&directory, &destination, &next_temporary_file)
                .unwrap();

        assert_ne!(temporary.name(), destination.as_c_str());
        assert!(
            !temporary
                .name()
                .to_bytes()
                .eq_ignore_ascii_case(destination.to_bytes())
        );
        let destination_path = root.join(OsStr::from_bytes(destination.to_bytes()));
        assert!(!destination_path.exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);

        drop(file);
        drop(temporary);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn atomic_replace_stays_with_open_directory_when_parent_path_is_rebound() {
        use std::sync::atomic::{AtomicU32, Ordering};

        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-rebind-tests");
        fs::create_dir_all(&base).unwrap();
        let root = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let parent = root.join("parent");
        let moved_parent = root.join("moved-parent");
        fs::create_dir(&parent).unwrap();

        let directory = SnapshotDirectory::open(&parent).unwrap();
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();

        let codec = SnapshotCodec::new(8);
        let envelope = codec.encode(b"payload").unwrap();
        let destination = CString::new("snapshot.bin").unwrap();
        replace_envelope_in_directory_with_sync(
            &directory,
            &destination,
            &envelope,
            SnapshotDirectory::sync,
        )
        .unwrap();

        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        let entries = fs::read_dir(&moved_parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, [moved_parent.join("snapshot.bin")]);
        let reopened = fs::read(&entries[0]).unwrap();
        assert_eq!(codec.decode(&reopened), Ok(&b"payload"[..]));

        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_checked_file_with_a_fifo_cannot_block_open() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-unit-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let path = directory.join("race.snapshot");
        fs::write(&path, b"checked regular file").unwrap();

        require_regular_snapshot_path(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let fifo_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_path` is a valid, NUL-terminated pathname that lives
        // through the call, and the mode contains only permission bits.
        let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), libc::S_IRUSR | libc::S_IWUSR) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());

        let worker_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            sender
                .send(open_regular_snapshot_path(&worker_path).map(drop))
                .unwrap();
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("opening the replacement FIFO must not block");
        worker.join().unwrap();

        assert!(matches!(
            result,
            Err(Int64TableFileRestoreError::NotRegularFile)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn repair_reports_directory_sync_failure_after_replacing_the_primary() {
        fn fail_directory_sync(_: &SnapshotDirectory) -> io::Result<()> {
            Err(io::Error::other("injected directory sync failure"))
        }

        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-repair-sync-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let primary_path = directory.join("primary.snapshot");
        let backup_path = directory.join("backup.snapshot");
        let snapshot_codec = SnapshotCodec::new(17);
        let payload_codec = NullableI64PayloadCodec::new(1, 17);
        let payload = payload_codec.encode(&[Some(22)]).unwrap();
        snapshot_codec
            .create_new_file(&backup_path, &payload)
            .unwrap();
        let backup_before = fs::read(&backup_path).unwrap();

        let error = restore_and_repair_int64_table_from_file_with_backup_using(
            &primary_path,
            &backup_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
            SnapshotRepairHooks {
                before_publish: || {},
                sync_directory: fail_directory_sync,
            },
        )
        .unwrap_err();

        assert!(matches!(
            error.primary_error(),
            Int64TableFileRestoreError::Open(source)
                if source.kind() == io::ErrorKind::NotFound
        ));
        assert!(matches!(
            error.repair_error(),
            Some(SnapshotReplaceError::SyncDirectory(source))
                if source.to_string() == "injected directory sync failure"
        ));
        assert!(
            error
                .repair_error()
                .is_some_and(SnapshotReplaceError::destination_was_replaced)
        );
        assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
        let table = restore_int64_table_from_file(
            &primary_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();
        assert_eq!(table.values(), &[Some(22)]);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn repair_serializes_a_concurrent_cooperative_primary_refresh() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-repair-race-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let primary_path = directory.join("primary.snapshot");
        let backup_path = directory.join("backup.snapshot");
        let snapshot_codec = SnapshotCodec::new(17);
        let payload_codec = NullableI64PayloadCodec::new(1, 17);

        for (path, value) in [(&primary_path, 11), (&backup_path, 22)] {
            let payload = payload_codec.encode(&[Some(value)]).unwrap();
            snapshot_codec.create_new_file(path, &payload).unwrap();
        }
        let mut corrupt_primary = fs::read(&primary_path).unwrap();
        *corrupt_primary.last_mut().unwrap() ^= 1;
        fs::write(&primary_path, corrupt_primary).unwrap();
        let backup_before = fs::read(&backup_path).unwrap();
        let refreshed_payload = payload_codec.encode(&[Some(33)]).unwrap();
        let refreshed_before = snapshot_codec.encode(&refreshed_payload).unwrap();

        let (publish_sender, publish_receiver) = mpsc::channel();
        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let publisher_directory = directory.clone();
        let publisher_primary = primary_path.clone();
        let publisher_payload = refreshed_payload.clone();
        let publisher = thread::spawn(move || {
            publish_receiver.recv().unwrap();
            let directory = SnapshotDirectory::open(&publisher_directory).unwrap();
            match directory.try_lock_exclusive() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("unexpected nonblocking lock failure: {error}"),
                Ok(_) => panic!("concurrent replacement acquired the repair lock"),
            }
            blocked_sender.send(()).unwrap();
            snapshot_codec
                .replace_file(publisher_primary, &publisher_payload)
                .unwrap();
        });

        let recovered = restore_and_repair_int64_table_from_file_with_backup_using(
            &primary_path,
            &backup_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
            SnapshotRepairHooks {
                before_publish: || {
                    publish_sender.send(()).unwrap();
                    blocked_receiver
                        .recv_timeout(Duration::from_secs(2))
                        .expect("the concurrent primary refresh must reach the held lock");
                },
                sync_directory: SnapshotDirectory::sync,
            },
        )
        .unwrap();
        publisher.join().unwrap();

        assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
        assert_eq!(recovered.table().values(), &[Some(22)]);
        assert_eq!(fs::read(&primary_path).unwrap(), refreshed_before);
        assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
        let table = restore_int64_table_from_file(
            &primary_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();
        assert_eq!(table.values(), &[Some(33)]);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn repair_stays_with_the_initial_directory_when_its_path_is_rebound() {
        static NEXT_TEST_DIRECTORY: AtomicU32 = AtomicU32::new(0);

        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-repair-rebind-tests");
        fs::create_dir_all(&base).unwrap();
        let root = loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let parent = root.join("parent");
        let moved_parent = root.join("moved-parent");
        fs::create_dir(&parent).unwrap();
        let primary_path = parent.join("primary.snapshot");
        let backup_path = root.join("backup.snapshot");
        let snapshot_codec = SnapshotCodec::new(17);
        let payload_codec = NullableI64PayloadCodec::new(1, 17);
        for (path, value) in [(&primary_path, 11), (&backup_path, 22)] {
            let payload = payload_codec.encode(&[Some(value)]).unwrap();
            snapshot_codec.create_new_file(path, &payload).unwrap();
        }
        let mut corrupt_primary = fs::read(&primary_path).unwrap();
        *corrupt_primary.last_mut().unwrap() ^= 1;
        fs::write(&primary_path, corrupt_primary).unwrap();

        let rebound_primary = primary_path.clone();
        let recovered = restore_and_repair_int64_table_from_file_with_backup_using(
            &primary_path,
            &backup_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
            SnapshotRepairHooks {
                before_publish: || {
                    fs::rename(&parent, &moved_parent).unwrap();
                    fs::create_dir(&parent).unwrap();
                    let payload = payload_codec.encode(&[Some(33)]).unwrap();
                    snapshot_codec
                        .create_new_file(&rebound_primary, &payload)
                        .unwrap();
                },
                sync_directory: SnapshotDirectory::sync,
            },
        )
        .unwrap();

        assert_eq!(recovered.source(), Int64TableFileRecoverySource::Backup);
        assert_eq!(recovered.table().values(), &[Some(22)]);
        let rebound = restore_int64_table_from_file(
            &primary_path,
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();
        assert_eq!(rebound.values(), &[Some(33)]);
        let repaired = restore_int64_table_from_file(
            moved_parent.join("primary.snapshot"),
            Schema::int64("reading", false),
            1,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();
        assert_eq!(repaired.values(), &[Some(22)]);

        fs::remove_dir_all(root).unwrap();
    }
}
