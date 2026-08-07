//! Versioned, bounded snapshot envelopes and `Int64` payloads.
//!
//! This module does not serialize a catalog. It can create and sync a new
//! envelope file, atomically replace an envelope through a sibling temporary
//! file on Unix, and encode either row-only data or one self-describing bounded
//! `Int64` table. See `docs/snapshot-format.md` for the stable binary layouts.

use std::error::Error;
#[cfg(unix)]
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(unix)]
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

#[cfg(unix)]
const TEMPORARY_CREATE_ATTEMPTS: usize = 128;
#[cfg(unix)]
static NEXT_TEMPORARY_FILE: AtomicU32 = AtomicU32::new(0);

/// Number of bytes in the nullable `Int64` payload row-count field.
pub const NULLABLE_I64_PAYLOAD_HEADER_LEN: usize = std::mem::size_of::<u64>();

/// Tag identifying a `NULL` row in a nullable `Int64` payload.
pub const NULLABLE_I64_NULL_TAG: u8 = 0;

/// Tag identifying a present value in a nullable `Int64` payload.
pub const NULLABLE_I64_VALUE_TAG: u8 = 1;

/// Magic bytes at the start of a self-describing `Int64` table payload.
pub const INT64_TABLE_PAYLOAD_MAGIC: [u8; 8] = *b"RHITBLP\0";

/// The self-describing `Int64` table payload version emitted and accepted.
pub const INT64_TABLE_PAYLOAD_VERSION: u16 = 1;

/// Number of fixed bytes in a self-describing `Int64` table payload.
///
/// The column name bytes follow the first 20 fixed bytes. The row cap and row
/// count account for the remaining 16 fixed bytes and follow the name.
pub const INT64_TABLE_PAYLOAD_FIXED_LEN: usize = INT64_TABLE_PAYLOAD_MAGIC.len()
    + std::mem::size_of::<u16>()
    + 2 * std::mem::size_of::<u8>()
    + 3 * std::mem::size_of::<u64>();

/// Tag identifying the only column type supported by the table payload.
pub const INT64_TABLE_INT64_TAG: u8 = 1;

/// Tag identifying a non-nullable column in the table payload.
pub const INT64_TABLE_NOT_NULL_TAG: u8 = 0;

/// Tag identifying a nullable column in the table payload.
pub const INT64_TABLE_NULLABLE_TAG: u8 = 1;

const INT64_TABLE_PAYLOAD_VERSION_OFFSET: usize = INT64_TABLE_PAYLOAD_MAGIC.len();
const INT64_TABLE_PAYLOAD_TYPE_OFFSET: usize =
    INT64_TABLE_PAYLOAD_VERSION_OFFSET + std::mem::size_of::<u16>();
const INT64_TABLE_PAYLOAD_NULLABILITY_OFFSET: usize =
    INT64_TABLE_PAYLOAD_TYPE_OFFSET + std::mem::size_of::<u8>();
const INT64_TABLE_PAYLOAD_NAME_LENGTH_OFFSET: usize =
    INT64_TABLE_PAYLOAD_NULLABILITY_OFFSET + std::mem::size_of::<u8>();
const INT64_TABLE_PAYLOAD_NAME_OFFSET: usize =
    INT64_TABLE_PAYLOAD_NAME_LENGTH_OFFSET + std::mem::size_of::<u64>();

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
#[cfg(unix)]
#[derive(Debug)]
pub enum SnapshotReplaceError {
    /// The payload could not be encoded before filesystem access began.
    Encode(SnapshotError),
    /// The destination has no normal, unambiguous final path component.
    InvalidDestination,
    /// The destination's parent directory could not be opened for syncing.
    OpenDirectory(io::Error),
    /// A sibling temporary file could not be exclusively created.
    CreateTemporary(io::Error),
    /// The complete encoded envelope could not be written to the temporary file.
    WriteTemporary(io::Error),
    /// The temporary file could not be synchronized before the rename.
    SyncTemporary(io::Error),
    /// The synchronized temporary file could not be renamed over the destination.
    Rename(io::Error),
    /// A temporary file could not be removed after an earlier failure.
    CleanupTemporary {
        /// The failure encountered while removing the temporary file.
        source: io::Error,
        /// The operation failure that triggered cleanup.
        operation: Box<SnapshotReplaceError>,
    },
    /// The parent directory could not be synchronized after a successful rename.
    ///
    /// The destination has already been replaced when this variant is returned,
    /// but the rename's durability after a system crash is uncertain.
    SyncDirectory(io::Error),
}

#[cfg(unix)]
impl SnapshotReplaceError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// A `true` result means the new envelope is visible at the destination,
    /// while its durability after a system crash remains uncertain.
    pub const fn destination_was_replaced(&self) -> bool {
        matches!(self, Self::SyncDirectory(_))
    }

    /// Returns the operation error that preceded a temporary-file cleanup failure.
    pub fn operation_error(&self) -> Option<&Self> {
        match self {
            Self::CleanupTemporary { operation, .. } => Some(operation),
            _ => None,
        }
    }
}

#[cfg(unix)]
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
            Self::CleanupTemporary { source, operation } => write!(
                formatter,
                "could not clean up temporary snapshot file after {operation}: {source}"
            ),
            Self::SyncDirectory(error) => write!(
                formatter,
                "snapshot was replaced, but its parent directory could not be synced: {error}"
            ),
        }
    }
}

#[cfg(unix)]
impl Error for SnapshotReplaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::InvalidDestination => None,
            Self::OpenDirectory(error)
            | Self::CreateTemporary(error)
            | Self::WriteTemporary(error)
            | Self::SyncTemporary(error)
            | Self::Rename(error)
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

/// An error produced while encoding or decoding one self-describing table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64TablePayloadError {
    /// The column name exceeds the codec's configured UTF-8 byte bound.
    NameTooLong { name_len: u64, max_name_len: usize },
    /// The persisted row cap exceeds the codec's configured row bound.
    RowCapLimitExceeded { row_cap: u64, max_rows: usize },
    /// The current row count exceeds the codec's configured row bound.
    RowLimitExceeded { row_count: u64, max_rows: usize },
    /// The encoded payload exceeds the codec's configured byte bound.
    PayloadTooLarge {
        payload_len: u64,
        max_payload_len: usize,
    },
    /// The payload ends before a complete declared field or row is present.
    Truncated {
        expected_len: usize,
        actual_len: usize,
    },
    /// The payload is not a self-describing `Int64` table payload.
    IncompatibleMagic {
        found: [u8; INT64_TABLE_PAYLOAD_MAGIC.len()],
    },
    /// The payload version is not supported by this codec.
    UnsupportedVersion { found: u16, supported: u16 },
    /// The column uses a type tag this codec does not know.
    UnknownColumnTypeTag { tag: u8 },
    /// The column uses a nullability tag this codec does not know.
    UnknownNullabilityTag { tag: u8 },
    /// The declared column name bytes are not valid UTF-8.
    InvalidColumnNameUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    /// The current row count is larger than the persisted table row cap.
    RowsExceedRowCap { row_count: u64, row_cap: u64 },
    /// A row uses a tag that is not defined by the payload format.
    UnknownRowTag { row_index: usize, tag: u8 },
    /// A `NULL` row was encoded for a non-nullable column.
    NullNotAllowed { row_index: usize },
    /// Bytes remain after the declared rows have been decoded.
    TrailingData {
        expected_len: usize,
        actual_len: usize,
    },
}

impl fmt::Display for Int64TablePayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameTooLong {
                name_len,
                max_name_len,
            } => write!(
                formatter,
                "Int64 table payload column name has {name_len} bytes, exceeding the limit of {max_name_len}"
            ),
            Self::RowCapLimitExceeded { row_cap, max_rows } => write!(
                formatter,
                "Int64 table payload row cap is {row_cap}, exceeding the limit of {max_rows}"
            ),
            Self::RowLimitExceeded {
                row_count,
                max_rows,
            } => write!(
                formatter,
                "Int64 table payload has {row_count} rows, exceeding the limit of {max_rows}"
            ),
            Self::PayloadTooLarge {
                payload_len,
                max_payload_len,
            } => write!(
                formatter,
                "Int64 table payload has {payload_len} bytes, exceeding the limit of {max_payload_len}"
            ),
            Self::Truncated {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "Int64 table payload is truncated: expected at least {expected_len} bytes, found {actual_len}"
            ),
            Self::IncompatibleMagic { found } => {
                write!(
                    formatter,
                    "incompatible Int64 table payload magic: {found:02x?}"
                )
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported Int64 table payload version {found}; this codec supports version {supported}"
            ),
            Self::UnknownColumnTypeTag { tag } => {
                write!(formatter, "unknown Int64 table column type tag {tag:#04x}")
            }
            Self::UnknownNullabilityTag { tag } => {
                write!(formatter, "unknown Int64 table nullability tag {tag:#04x}")
            }
            Self::InvalidColumnNameUtf8 {
                valid_up_to,
                error_len,
            } => match error_len {
                Some(error_len) => write!(
                    formatter,
                    "Int64 table column name is not UTF-8 at byte {valid_up_to} (invalid sequence length {error_len})"
                ),
                None => write!(
                    formatter,
                    "Int64 table column name has an incomplete UTF-8 sequence at byte {valid_up_to}"
                ),
            },
            Self::RowsExceedRowCap { row_count, row_cap } => write!(
                formatter,
                "Int64 table payload has {row_count} rows, exceeding its persisted row cap of {row_cap}"
            ),
            Self::UnknownRowTag { row_index, tag } => write!(
                formatter,
                "Int64 table payload row {row_index} has unknown tag {tag:#04x}"
            ),
            Self::NullNotAllowed { row_index } => write!(
                formatter,
                "Int64 table payload row {row_index} is NULL, but the column is not nullable"
            ),
            Self::TrailingData {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "Int64 table payload has trailing data: expected {expected_len} bytes, found {actual_len}"
            ),
        }
    }
}

impl Error for Int64TablePayloadError {}

/// An error produced while restoring a self-describing [`Int64Table`] file.
#[derive(Debug)]
pub enum Int64TablePayloadFileRestoreError {
    /// The snapshot path could not be opened for reading.
    Open(io::Error),
    /// The snapshot path does not identify a regular file.
    NotRegularFile,
    /// The opened snapshot file could not be inspected or read completely.
    Read(io::Error),
    /// The file is larger than the envelope header plus the envelope limit.
    FileTooLarge { file_len: u64, max_file_len: usize },
    /// The snapshot envelope could not be decoded.
    Envelope(SnapshotError),
    /// The envelope payload was not a valid self-describing `Int64` table.
    Payload(Int64TablePayloadError),
}

impl fmt::Display for Int64TablePayloadFileRestoreError {
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
            Self::Envelope(error) => {
                write!(formatter, "could not decode snapshot envelope: {error}")
            }
            Self::Payload(error) => {
                write!(formatter, "could not decode Int64 table payload: {error}")
            }
        }
    }
}

impl Error for Int64TablePayloadFileRestoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(error) | Self::Read(error) => Some(error),
            Self::Envelope(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::NotRegularFile | Self::FileTooLarge { .. } => None,
        }
    }
}

/// An error produced while atomically saving an [`Int64Table`] snapshot file.
#[cfg(unix)]
#[derive(Debug)]
pub enum Int64TableFileSaveError {
    /// The table rows could not be encoded as a nullable `Int64` payload.
    Payload(NullableI64PayloadError),
    /// The encoded payload could not atomically replace the destination.
    Replace(SnapshotReplaceError),
}

#[cfg(unix)]
impl Int64TableFileSaveError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// Only a replacement-stage directory-sync failure returns `true`. Payload
    /// encoding and every replacement failure before the rename return `false`.
    pub const fn destination_was_replaced(&self) -> bool {
        match self {
            Self::Payload(_) => false,
            Self::Replace(error) => error.destination_was_replaced(),
        }
    }
}

#[cfg(unix)]
impl fmt::Display for Int64TableFileSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(error) => write!(formatter, "could not encode table payload: {error}"),
            Self::Replace(error) => write!(formatter, "could not replace table snapshot: {error}"),
        }
    }
}

#[cfg(unix)]
impl Error for Int64TableFileSaveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Payload(error) => Some(error),
            Self::Replace(error) => Some(error),
        }
    }
}

#[cfg(unix)]
impl From<NullableI64PayloadError> for Int64TableFileSaveError {
    fn from(error: NullableI64PayloadError) -> Self {
        Self::Payload(error)
    }
}

#[cfg(unix)]
impl From<SnapshotReplaceError> for Int64TableFileSaveError {
    fn from(error: SnapshotReplaceError) -> Self {
        Self::Replace(error)
    }
}

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

/// Encodes and decodes one bounded, self-describing [`Int64Table`].
///
/// Unlike [`NullableI64PayloadCodec`], this additive payload format includes
/// the column name, nullability, and table row cap. Decoding validates the
/// complete payload before allocating rows or constructing the table. It can
/// be passed directly to [`SnapshotCodec`] for checksummed envelopes and
/// atomic file replacement.
///
/// # Examples
///
/// ```
/// use rusthouse::{Int64Table, Int64TablePayloadCodec, Schema, SnapshotCodec};
///
/// let mut table = Int64Table::new(Schema::int64("reading", true), 4);
/// table.append_batch(&[Some(-7), None])?;
/// let table_codec = Int64TablePayloadCodec::new(32, 4, 128);
/// let snapshot_codec = SnapshotCodec::new(128);
///
/// let payload = table_codec.encode(&table)?;
/// let envelope = snapshot_codec.encode(&payload)?;
/// let reopened = table_codec.decode(snapshot_codec.decode(&envelope)?)?;
///
/// assert_eq!(reopened, table);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64TablePayloadCodec {
    max_name_len: usize,
    max_rows: usize,
    max_payload_len: usize,
}

impl Int64TablePayloadCodec {
    /// Creates a codec with inclusive column-name, row, and payload-byte limits.
    ///
    /// `max_name_len` counts UTF-8 bytes. `max_rows` bounds both the persisted
    /// row cap and the current row count.
    pub const fn new(max_name_len: usize, max_rows: usize, max_payload_len: usize) -> Self {
        Self {
            max_name_len,
            max_rows,
            max_payload_len,
        }
    }

    /// Returns the maximum column-name length accepted, in UTF-8 bytes.
    pub const fn max_name_len(self) -> usize {
        self.max_name_len
    }

    /// Returns the maximum persisted row cap and current row count accepted.
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Returns the maximum encoded payload size accepted, in bytes.
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Encodes one table, including its one-column schema and row cap.
    pub fn encode(self, table: &Int64Table) -> Result<Vec<u8>, Int64TablePayloadError> {
        let column = table.schema().column();
        let name = column.name().as_bytes();
        let name_len = u64::try_from(name.len()).unwrap_or(u64::MAX);
        if name.len() > self.max_name_len {
            return Err(Int64TablePayloadError::NameTooLong {
                name_len,
                max_name_len: self.max_name_len,
            });
        }

        let row_cap = u64::try_from(table.row_cap()).unwrap_or(u64::MAX);
        if table.row_cap() > self.max_rows {
            return Err(Int64TablePayloadError::RowCapLimitExceeded {
                row_cap,
                max_rows: self.max_rows,
            });
        }

        let row_count = u64::try_from(table.row_count()).unwrap_or(u64::MAX);
        if table.row_count() > self.max_rows {
            return Err(Int64TablePayloadError::RowLimitExceeded {
                row_count,
                max_rows: self.max_rows,
            });
        }

        let fixed_len = INT64_TABLE_PAYLOAD_FIXED_LEN.checked_add(name.len());
        let payload_len = fixed_len.and_then(|fixed_len| {
            table.values().iter().try_fold(fixed_len, |length, value| {
                let row_len = if value.is_some() {
                    std::mem::size_of::<u8>() + std::mem::size_of::<i64>()
                } else {
                    std::mem::size_of::<u8>()
                };
                length.checked_add(row_len)
            })
        });
        let Some(payload_len) = payload_len else {
            return Err(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::MAX,
                max_payload_len: self.max_payload_len,
            });
        };
        if payload_len > self.max_payload_len {
            return Err(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::try_from(payload_len).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }

        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&INT64_TABLE_PAYLOAD_MAGIC);
        payload.extend_from_slice(&INT64_TABLE_PAYLOAD_VERSION.to_le_bytes());
        payload.push(INT64_TABLE_INT64_TAG);
        payload.push(if column.is_nullable() {
            INT64_TABLE_NULLABLE_TAG
        } else {
            INT64_TABLE_NOT_NULL_TAG
        });
        payload.extend_from_slice(&name_len.to_le_bytes());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&row_cap.to_le_bytes());
        payload.extend_from_slice(&row_count.to_le_bytes());
        for value in table.values() {
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

    /// Validates and decodes one complete self-describing table payload.
    ///
    /// Payload bytes, every declared bound, UTF-8, tags, nullability, row cap,
    /// truncation, and trailing data are checked in a validation pass before
    /// row allocation and table construction begin.
    pub fn decode(self, payload: &[u8]) -> Result<Int64Table, Int64TablePayloadError> {
        if payload.len() > self.max_payload_len {
            return Err(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::try_from(payload.len()).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }
        if payload.len() < INT64_TABLE_PAYLOAD_NAME_OFFSET {
            return Err(Int64TablePayloadError::Truncated {
                expected_len: INT64_TABLE_PAYLOAD_NAME_OFFSET,
                actual_len: payload.len(),
            });
        }

        let found_magic = read_array::<{ INT64_TABLE_PAYLOAD_MAGIC.len() }>(payload, 0);
        if found_magic != INT64_TABLE_PAYLOAD_MAGIC {
            return Err(Int64TablePayloadError::IncompatibleMagic { found: found_magic });
        }

        let version =
            u16::from_le_bytes(read_array::<2>(payload, INT64_TABLE_PAYLOAD_VERSION_OFFSET));
        if version != INT64_TABLE_PAYLOAD_VERSION {
            return Err(Int64TablePayloadError::UnsupportedVersion {
                found: version,
                supported: INT64_TABLE_PAYLOAD_VERSION,
            });
        }

        let type_tag = payload[INT64_TABLE_PAYLOAD_TYPE_OFFSET];
        if type_tag != INT64_TABLE_INT64_TAG {
            return Err(Int64TablePayloadError::UnknownColumnTypeTag { tag: type_tag });
        }

        let nullable = match payload[INT64_TABLE_PAYLOAD_NULLABILITY_OFFSET] {
            INT64_TABLE_NOT_NULL_TAG => false,
            INT64_TABLE_NULLABLE_TAG => true,
            tag => return Err(Int64TablePayloadError::UnknownNullabilityTag { tag }),
        };

        let declared_name_len = u64::from_le_bytes(read_array::<8>(
            payload,
            INT64_TABLE_PAYLOAD_NAME_LENGTH_OFFSET,
        ));
        let name_len = usize::try_from(declared_name_len).map_err(|_| {
            Int64TablePayloadError::NameTooLong {
                name_len: declared_name_len,
                max_name_len: self.max_name_len,
            }
        })?;
        if name_len > self.max_name_len {
            return Err(Int64TablePayloadError::NameTooLong {
                name_len: declared_name_len,
                max_name_len: self.max_name_len,
            });
        }

        let name_end = INT64_TABLE_PAYLOAD_NAME_OFFSET
            .checked_add(name_len)
            .ok_or(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::MAX,
                max_payload_len: self.max_payload_len,
            })?;
        if payload.len() < name_end {
            return Err(Int64TablePayloadError::Truncated {
                expected_len: name_end,
                actual_len: payload.len(),
            });
        }
        let name = std::str::from_utf8(&payload[INT64_TABLE_PAYLOAD_NAME_OFFSET..name_end])
            .map_err(|error| Int64TablePayloadError::InvalidColumnNameUtf8 {
                valid_up_to: error.valid_up_to(),
                error_len: error.error_len(),
            })?;

        let rows_offset = name_end.checked_add(2 * std::mem::size_of::<u64>()).ok_or(
            Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::MAX,
                max_payload_len: self.max_payload_len,
            },
        )?;
        if rows_offset > self.max_payload_len {
            return Err(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::try_from(rows_offset).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }
        if payload.len() < rows_offset {
            return Err(Int64TablePayloadError::Truncated {
                expected_len: rows_offset,
                actual_len: payload.len(),
            });
        }

        let declared_row_cap = u64::from_le_bytes(read_array::<8>(payload, name_end));
        let row_cap = usize::try_from(declared_row_cap).map_err(|_| {
            Int64TablePayloadError::RowCapLimitExceeded {
                row_cap: declared_row_cap,
                max_rows: self.max_rows,
            }
        })?;
        if row_cap > self.max_rows {
            return Err(Int64TablePayloadError::RowCapLimitExceeded {
                row_cap: declared_row_cap,
                max_rows: self.max_rows,
            });
        }

        let row_count_offset = name_end + std::mem::size_of::<u64>();
        let declared_row_count = u64::from_le_bytes(read_array::<8>(payload, row_count_offset));
        let row_count = usize::try_from(declared_row_count).map_err(|_| {
            Int64TablePayloadError::RowLimitExceeded {
                row_count: declared_row_count,
                max_rows: self.max_rows,
            }
        })?;
        if row_count > self.max_rows {
            return Err(Int64TablePayloadError::RowLimitExceeded {
                row_count: declared_row_count,
                max_rows: self.max_rows,
            });
        }
        if row_count > row_cap {
            return Err(Int64TablePayloadError::RowsExceedRowCap {
                row_count: declared_row_count,
                row_cap: declared_row_cap,
            });
        }

        let minimum_len =
            rows_offset
                .checked_add(row_count)
                .ok_or(Int64TablePayloadError::PayloadTooLarge {
                    payload_len: u64::MAX,
                    max_payload_len: self.max_payload_len,
                })?;
        if minimum_len > self.max_payload_len {
            return Err(Int64TablePayloadError::PayloadTooLarge {
                payload_len: u64::try_from(minimum_len).unwrap_or(u64::MAX),
                max_payload_len: self.max_payload_len,
            });
        }
        if payload.len() < minimum_len {
            return Err(Int64TablePayloadError::Truncated {
                expected_len: minimum_len,
                actual_len: payload.len(),
            });
        }

        validate_int64_table_rows(payload, rows_offset, row_count, nullable)?;

        let mut rows = Vec::with_capacity(row_count);
        let mut offset = rows_offset;
        for _ in 0..row_count {
            let tag = payload[offset];
            offset += std::mem::size_of::<u8>();
            if tag == NULLABLE_I64_NULL_TAG {
                rows.push(None);
            } else {
                rows.push(Some(i64::from_le_bytes(read_array::<8>(payload, offset))));
                offset += std::mem::size_of::<i64>();
            }
        }

        let mut table = Int64Table::new(Schema::int64(name, nullable), row_cap);
        table
            .append_batch(&rows)
            .expect("the complete table payload was validated before construction");
        Ok(table)
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

    /// Atomically creates or replaces a snapshot envelope file on Unix.
    ///
    /// The payload is bounded and encoded before filesystem access. The
    /// envelope is written to an exclusively created sibling temporary file,
    /// which is synchronized and then renamed over `path`. Finally, the
    /// destination's parent directory is synchronized so the rename is durable.
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
    #[cfg(unix)]
    pub fn replace_file(
        self,
        path: impl AsRef<Path>,
        payload: &[u8],
    ) -> Result<(), SnapshotReplaceError> {
        let envelope = self.encode(payload).map_err(SnapshotReplaceError::Encode)?;
        let path = path.as_ref();
        let destination = snapshot_destination_name(path)?;

        let parent = normalized_parent(path);
        let directory =
            SnapshotDirectory::open(parent).map_err(SnapshotReplaceError::OpenDirectory)?;
        replace_envelope_in_directory(&directory, &destination, &envelope)
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

#[cfg(unix)]
fn replace_envelope_in_directory(
    directory: &SnapshotDirectory,
    destination: &CStr,
    envelope: &[u8],
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

    directory
        .sync()
        .map_err(SnapshotReplaceError::SyncDirectory)
}

#[cfg(unix)]
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

#[cfg(unix)]
fn normalized_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(unix)]
fn create_temporary_snapshot<'a>(
    directory: &'a SnapshotDirectory,
    destination: &CStr,
) -> Result<(File, TemporarySnapshot<'a>), SnapshotReplaceError> {
    create_temporary_snapshot_with_counter(directory, destination, &NEXT_TEMPORARY_FILE)
}

#[cfg(unix)]
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

#[cfg(unix)]
fn next_temporary_snapshot_suffix(next_temporary_file: &AtomicU32) -> CString {
    let sequence = next_temporary_file.fetch_add(1, Ordering::Relaxed);
    let name = format!(".rusthouse-snapshot-{}-{sequence}.tmp", std::process::id());
    CString::new(name).expect("generated snapshot names never contain NUL bytes")
}

#[cfg(unix)]
fn temporary_snapshot_name(destination: &CStr, suffix: &CStr) -> CString {
    let mut name = Vec::with_capacity(destination.to_bytes().len() + suffix.to_bytes().len());
    name.extend_from_slice(destination.to_bytes());
    name.extend_from_slice(suffix.to_bytes());
    CString::new(name).expect("validated destination and generated suffix contain no NUL bytes")
}

#[cfg(unix)]
struct SnapshotDirectory {
    file: File,
}

#[cfg(unix)]
impl SnapshotDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self { file })
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

#[cfg(unix)]
struct TemporarySnapshot<'a> {
    directory: &'a SnapshotDirectory,
    name: CString,
    remove_on_drop: bool,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for TemporarySnapshot<'_> {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = self.directory.remove(&self.name);
        }
    }
}

/// Atomically saves one bounded [`Int64Table`] snapshot file on Unix.
///
/// The table's rows are encoded with `payload_codec` before any filesystem
/// access, then [`SnapshotCodec::replace_file`] atomically creates or replaces
/// `path`. Payload encoding failures and replacement-stage failures remain
/// distinct. Every failure before a successful rename preserves an existing
/// destination; a post-rename directory-sync failure reports that replacement
/// through [`Int64TableFileSaveError::destination_was_replaced`].
///
/// Only row values are persisted. The schema and restored table row cap remain
/// caller-supplied when reopening with [`restore_int64_table_from_file`].
#[cfg(unix)]
pub fn save_int64_table_to_file(
    path: impl AsRef<Path>,
    table: &Int64Table,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<(), Int64TableFileSaveError> {
    let payload = payload_codec.encode(table.values())?;
    snapshot_codec.replace_file(path, &payload)?;
    Ok(())
}

/// Opens a bounded snapshot file and restores its self-describing `Int64` table.
///
/// The envelope is bounded by `snapshot_codec`, while the persisted column
/// name, nullability, row cap, rows, and payload bytes are independently
/// bounded and decoded by `payload_codec`. No schema or row cap is supplied by
/// the caller. The path must identify a regular file, and at most the envelope
/// header plus the configured envelope payload limit is read. Open, read,
/// envelope, and table-payload failures remain distinct, and all validation
/// completes before a table is returned.
pub fn restore_int64_table_payload_from_file(
    path: impl AsRef<Path>,
    snapshot_codec: SnapshotCodec,
    payload_codec: Int64TablePayloadCodec,
) -> Result<Int64Table, Int64TablePayloadFileRestoreError> {
    let envelope = read_bounded_snapshot_file(path.as_ref(), snapshot_codec.max_payload_len())
        .map_err(Int64TablePayloadFileRestoreError::from)?;
    let payload = snapshot_codec
        .decode(&envelope)
        .map_err(Int64TablePayloadFileRestoreError::Envelope)?;
    payload_codec
        .decode(payload)
        .map_err(Int64TablePayloadFileRestoreError::Payload)
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
    let envelope = read_bounded_snapshot_file(path.as_ref(), snapshot_codec.max_payload_len())
        .map_err(Int64TableFileRestoreError::from)?;

    restore_int64_table(&envelope, schema, row_cap, snapshot_codec, payload_codec)
        .map_err(Int64TableFileRestoreError::Restore)
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

#[derive(Debug)]
enum SnapshotFileReadError {
    Open(io::Error),
    NotRegularFile,
    Read(io::Error),
    FileTooLarge { file_len: u64, max_file_len: usize },
}

impl From<SnapshotFileReadError> for Int64TableFileRestoreError {
    fn from(error: SnapshotFileReadError) -> Self {
        match error {
            SnapshotFileReadError::Open(error) => Self::Open(error),
            SnapshotFileReadError::NotRegularFile => Self::NotRegularFile,
            SnapshotFileReadError::Read(error) => Self::Read(error),
            SnapshotFileReadError::FileTooLarge {
                file_len,
                max_file_len,
            } => Self::FileTooLarge {
                file_len,
                max_file_len,
            },
        }
    }
}

impl From<SnapshotFileReadError> for Int64TablePayloadFileRestoreError {
    fn from(error: SnapshotFileReadError) -> Self {
        match error {
            SnapshotFileReadError::Open(error) => Self::Open(error),
            SnapshotFileReadError::NotRegularFile => Self::NotRegularFile,
            SnapshotFileReadError::Read(error) => Self::Read(error),
            SnapshotFileReadError::FileTooLarge {
                file_len,
                max_file_len,
            } => Self::FileTooLarge {
                file_len,
                max_file_len,
            },
        }
    }
}

fn read_bounded_snapshot_file(
    path: &Path,
    max_payload_len: usize,
) -> Result<Vec<u8>, SnapshotFileReadError> {
    let mut file = open_regular_snapshot_file(path)?;
    let max_file_len = SNAPSHOT_HEADER_LEN.saturating_add(max_payload_len);
    let file_len = bounded_file_len(&file, max_file_len)?;
    let capacity = usize::try_from(file_len).unwrap_or(max_file_len);
    let mut envelope = Vec::with_capacity(capacity);
    Read::take(&mut file, u64::try_from(max_file_len).unwrap_or(u64::MAX))
        .read_to_end(&mut envelope)
        .map_err(SnapshotFileReadError::Read)?;

    // Check the opened file again in case it grew after the first size check.
    bounded_file_len(&file, max_file_len)?;
    Ok(envelope)
}

fn open_regular_snapshot_file(path: &Path) -> Result<File, SnapshotFileReadError> {
    require_regular_snapshot_path(path)?;
    open_regular_snapshot_path(path)
}

fn require_regular_snapshot_path(path: &Path) -> Result<(), SnapshotFileReadError> {
    let metadata = fs::metadata(path).map_err(SnapshotFileReadError::Open)?;
    if !metadata.is_file() {
        return Err(SnapshotFileReadError::NotRegularFile);
    }

    Ok(())
}

fn open_regular_snapshot_path(path: &Path) -> Result<File, SnapshotFileReadError> {
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // A pathname that becomes a FIFO between the metadata check and open
        // must not block this process. This flag has no effect on regular files.
        options.custom_flags(libc::O_NONBLOCK);
    }

    let file = options.open(path).map_err(SnapshotFileReadError::Open)?;
    let metadata = file.metadata().map_err(SnapshotFileReadError::Read)?;
    if !metadata.is_file() {
        return Err(SnapshotFileReadError::NotRegularFile);
    }

    Ok(file)
}

fn bounded_file_len(file: &File, max_file_len: usize) -> Result<u64, SnapshotFileReadError> {
    let metadata = file.metadata().map_err(SnapshotFileReadError::Read)?;
    if !metadata.is_file() {
        return Err(SnapshotFileReadError::NotRegularFile);
    }

    let file_len = metadata.len();
    let max_file_len_u64 = u64::try_from(max_file_len).unwrap_or(u64::MAX);
    if file_len > max_file_len_u64 {
        return Err(SnapshotFileReadError::FileTooLarge {
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

fn validate_int64_table_rows(
    payload: &[u8],
    rows_offset: usize,
    row_count: usize,
    nullable: bool,
) -> Result<(), Int64TablePayloadError> {
    let mut offset = rows_offset;
    for row_index in 0..row_count {
        let tag_end = offset.saturating_add(std::mem::size_of::<u8>());
        let Some(&tag) = payload.get(offset) else {
            return Err(Int64TablePayloadError::Truncated {
                expected_len: tag_end,
                actual_len: payload.len(),
            });
        };
        offset = tag_end;

        match tag {
            NULLABLE_I64_NULL_TAG if !nullable => {
                return Err(Int64TablePayloadError::NullNotAllowed { row_index });
            }
            NULLABLE_I64_NULL_TAG => {}
            NULLABLE_I64_VALUE_TAG => {
                let value_end = offset.saturating_add(std::mem::size_of::<i64>());
                if payload.len() < value_end {
                    return Err(Int64TablePayloadError::Truncated {
                        expected_len: value_end,
                        actual_len: payload.len(),
                    });
                }
                offset = value_end;
            }
            tag => {
                return Err(Int64TablePayloadError::UnknownRowTag { row_index, tag });
            }
        }
    }

    if payload.len() > offset {
        return Err(Int64TablePayloadError::TrailingData {
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
        replace_envelope_in_directory(&directory, &destination, &envelope).unwrap();

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

        assert!(matches!(result, Err(SnapshotFileReadError::NotRegularFile)));
        fs::remove_dir_all(directory).unwrap();
    }
}
