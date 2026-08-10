//! Crash-recoverable, bounded write-ahead logging for batch `Int64` tables.
//!
//! The stable framing and lifecycle are documented in `docs/int64-wal-format.md`.

use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const REGISTRY_MANIFEST_NAME: &str = "manifest.rhi64";
const REGISTRY_MANIFEST_MAGIC: [u8; 8] = *b"RHI64REG";
const REGISTRY_MANIFEST_VERSION: u16 = 1;
const REGISTRY_MANIFEST_HEADER_LEN: usize = 28;
const REGISTRY_DESCRIPTOR_MIN_PAYLOAD_LEN: usize = 8;

/// Default maximum number of table WALs in one registry directory.
pub const DEFAULT_MAX_INT64_WAL_REGISTRY_TABLES: usize = 1_024;
/// Default maximum registry manifest size.
pub const DEFAULT_MAX_INT64_WAL_REGISTRY_MANIFEST_BYTES: usize = 1024 * 1024;
/// Default aggregate byte cap across registry member WALs.
pub const DEFAULT_MAX_INT64_WAL_REGISTRY_BYTES: usize = 256 * 1024 * 1024;
/// Default aggregate committed-record cap across registry member WALs.
pub const DEFAULT_MAX_INT64_WAL_REGISTRY_RECORDS: usize = 4_000_000;

/// Magic at the start of every write-ahead-log frame.
pub const INT64_WAL_MAGIC: [u8; 8] = *b"RHI64WAL";
/// Version emitted and accepted by the write-ahead-log codec.
pub const INT64_WAL_VERSION: u16 = 1;
/// Magic in the commit footer of every complete record.
pub const INT64_WAL_COMMIT_MAGIC: [u8; 8] = *b"RHWLCMIT";
/// Fixed bytes before one record payload.
pub const INT64_WAL_FRAME_HEADER_LEN: usize = 32;
/// Fixed bytes after one record payload.
pub const INT64_WAL_COMMIT_LEN: usize = 16;
/// Fixed framing bytes charged to every record.
pub const INT64_WAL_FRAME_OVERHEAD: usize = INT64_WAL_FRAME_HEADER_LEN + INT64_WAL_COMMIT_LEN;

/// Default maximum complete WAL file size.
pub const DEFAULT_MAX_INT64_WAL_BYTES: usize = 64 * 1024 * 1024;
/// Default maximum payload size of one WAL record.
pub const DEFAULT_MAX_INT64_WAL_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of committed records, including the bootstrap.
pub const DEFAULT_MAX_INT64_WAL_RECORDS: usize = 1_000_000;

const BOOTSTRAP_KIND: u8 = 1;
const APPEND_KIND: u8 = 2;
const TRUNCATE_KIND: u8 = 3;
const REPLACE_KIND: u8 = 4;
const NULLABLE_APPEND_KIND: u8 = 5;
const NULLABLE_REPLACE_KIND: u8 = 6;
const QUERY_LIMIT_FIELD_COUNT: usize = 10;

/// Inclusive storage and replay limits for one `Int64` write-ahead log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64WriteAheadLogLimits {
    /// Maximum complete file size, including framing and a torn tail.
    pub max_file_bytes: usize,
    /// Maximum payload bytes in one record.
    pub max_record_bytes: usize,
    /// Maximum committed records, including the bootstrap record.
    pub max_records: usize,
}

impl Int64WriteAheadLogLimits {
    /// Creates explicit file, record-payload, and record-count bounds.
    #[must_use]
    pub const fn new(max_file_bytes: usize, max_record_bytes: usize, max_records: usize) -> Self {
        Self {
            max_file_bytes,
            max_record_bytes,
            max_records,
        }
    }
}

impl Default for Int64WriteAheadLogLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_INT64_WAL_BYTES,
            DEFAULT_MAX_INT64_WAL_RECORD_BYTES,
            DEFAULT_MAX_INT64_WAL_RECORDS,
        )
    }
}

/// Inclusive directory-wide and per-table bounds for a multi-table WAL registry.
///
/// # Examples
///
/// ```no_run
/// use rusthouse::{
///     Database, Int64WriteAheadLogLimits, Int64WriteAheadLogRegistryLimits,
/// };
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let limits = Int64WriteAheadLogRegistryLimits::new(
///     2,
///     16 * 1024,
///     1024 * 1024,
///     128,
///     Int64WriteAheadLogLimits::new(512 * 1024, 64 * 1024, 64),
/// );
/// let mut database = Database::new();
/// database.create_nullable_int64_table("events", "value", vec![Some(1), None])?;
/// database.create_nullable_int64_table("metrics", "value", vec![Some(2)])?;
///
/// database.enable_int64_write_ahead_log_registry(
///     &["events", "metrics"],
///     "example-wal-registry",
///     limits,
/// )?;
/// database.append_nullable_int64_values("events", &[Some(3)])?;
/// assert!(database.disable_int64_write_ahead_log());
///
/// // Recovery returns the complete registry or an error, never a partial database.
/// let _recovered = Database::recover_int64_write_ahead_log_registry(
///     "example-wal-registry",
///     limits,
/// )?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64WriteAheadLogRegistryLimits {
    /// Maximum number of table WALs and manifest descriptors.
    pub max_tables: usize,
    /// Maximum complete manifest size, including its header and descriptors.
    pub max_manifest_bytes: usize,
    /// Maximum aggregate size of all member WAL files, including framing.
    pub max_total_wal_bytes: usize,
    /// Maximum aggregate committed records, including every bootstrap record.
    pub max_total_records: usize,
    /// File, record-payload, and committed-record limits applied independently
    /// to every table WAL.
    pub per_table: Int64WriteAheadLogLimits,
}

impl Int64WriteAheadLogRegistryLimits {
    /// Creates explicit table-count, manifest, aggregate, and per-table bounds.
    #[must_use]
    pub const fn new(
        max_tables: usize,
        max_manifest_bytes: usize,
        max_total_wal_bytes: usize,
        max_total_records: usize,
        per_table: Int64WriteAheadLogLimits,
    ) -> Self {
        Self {
            max_tables,
            max_manifest_bytes,
            max_total_wal_bytes,
            max_total_records,
            per_table,
        }
    }
}

impl Default for Int64WriteAheadLogRegistryLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_INT64_WAL_REGISTRY_TABLES,
            DEFAULT_MAX_INT64_WAL_REGISTRY_MANIFEST_BYTES,
            DEFAULT_MAX_INT64_WAL_REGISTRY_BYTES,
            DEFAULT_MAX_INT64_WAL_REGISTRY_RECORDS,
            Int64WriteAheadLogLimits::default(),
        )
    }
}

/// A directory-wide registry bound was exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64WriteAheadLogRegistryLimitError {
    /// The registry contains or requested more tables than allowed.
    Tables {
        /// Number of tables found or requested.
        tables: u64,
        /// Configured inclusive table-count limit.
        max_tables: usize,
    },
    /// The complete manifest is larger than allowed.
    ManifestBytes {
        /// Manifest size found or required, in bytes.
        bytes: u64,
        /// Configured inclusive manifest-size limit, in bytes.
        max_bytes: usize,
    },
    /// The aggregate member-WAL size is larger than allowed.
    TotalWalBytes {
        /// Aggregate member-WAL size found or required, in bytes.
        bytes: u64,
        /// Configured inclusive aggregate member-size limit, in bytes.
        max_bytes: usize,
    },
    /// The aggregate committed-record count is larger than allowed.
    TotalRecords {
        /// Aggregate committed records found or required.
        records: u64,
        /// Configured inclusive aggregate record-count limit.
        max_records: usize,
    },
}

impl fmt::Display for Int64WriteAheadLogRegistryLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tables { tables, max_tables } => write!(
                formatter,
                "Int64 WAL registry has {tables} tables, exceeding the limit of {max_tables}"
            ),
            Self::ManifestBytes { bytes, max_bytes } => write!(
                formatter,
                "Int64 WAL registry manifest has {bytes} bytes, exceeding the limit of {max_bytes}"
            ),
            Self::TotalWalBytes { bytes, max_bytes } => write!(
                formatter,
                "Int64 WAL registry members have {bytes} bytes, exceeding the aggregate limit of {max_bytes}"
            ),
            Self::TotalRecords {
                records,
                max_records,
            } => write!(
                formatter,
                "Int64 WAL registry members have {records} records, exceeding the aggregate limit of {max_records}"
            ),
        }
    }
}

impl StdError for Int64WriteAheadLogRegistryLimitError {}

/// Typed structural corruption or inconsistency in a registry manifest,
/// directory, or member set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64WriteAheadLogRegistryCorruption {
    /// The manifest does not begin with the registry magic bytes.
    ManifestMagic {
        /// Bytes found in the manifest magic field.
        found: [u8; 8],
    },
    /// The manifest uses a version this reader does not support.
    ManifestVersion {
        /// Version found in the manifest.
        found: u16,
        /// Version supported by this reader.
        supported: u16,
    },
    /// The manifest's reserved field is not zero.
    ManifestReserved {
        /// Value found in the reserved field.
        found: u16,
    },
    /// The declared manifest payload length differs from the available bytes.
    ManifestLength {
        /// Payload length declared by the manifest header.
        declared: u64,
        /// Payload bytes actually present after the header.
        actual: u64,
    },
    /// The manifest checksum does not authenticate its header and payload.
    ManifestChecksum {
        /// Checksum stored in the manifest.
        expected: u32,
        /// Checksum calculated from the manifest bytes.
        actual: u32,
    },
    /// A required manifest header or descriptor field is malformed.
    ManifestPayload {
        /// Name of the field that could not be decoded.
        field: &'static str,
    },
    /// The manifest contains no table descriptors.
    Empty,
    /// Two descriptors have the same case-insensitive table name.
    DuplicateTable {
        /// Repeated table name found in the manifest or enable request.
        table: String,
    },
    /// Two descriptors have the same case-insensitive member filename.
    DuplicateMember {
        /// Repeated member filename found in the manifest.
        member: String,
    },
    /// A member filename is not a safe normal path component.
    InvalidMember {
        /// Unsafe member filename found in the manifest.
        member: String,
    },
    /// Descriptors are not in strict canonical case-insensitive table order.
    NonDeterministicOrder {
        /// Display name of the preceding descriptor.
        previous: String,
        /// Out-of-order table display name.
        table: String,
    },
    /// A listed member file is absent from the registry directory.
    MissingMember {
        /// Table whose member is absent.
        table: String,
        /// Missing member filename.
        member: String,
    },
    /// A member file aliases an earlier member's filesystem identity.
    DuplicateMemberFile {
        /// Table whose member aliases another member.
        table: String,
        /// Aliased member filename.
        member: String,
    },
    /// The registry directory contains an entry not listed by the manifest.
    UnexpectedDirectoryEntry {
        /// Unlisted directory-entry name, lossily decoded when necessary.
        entry: String,
    },
    /// A descriptor table name differs from its member bootstrap table name.
    TableNameMismatch {
        /// Table name recorded by the manifest descriptor.
        expected: String,
        /// Table name recorded by the member bootstrap.
        found: String,
    },
    /// A member's database-wide settings differ from earlier members.
    DatabaseSettingsMismatch {
        /// Table whose member carries inconsistent settings.
        table: String,
    },
}

impl fmt::Display for Int64WriteAheadLogRegistryCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestMagic { found } => {
                write!(
                    formatter,
                    "incompatible Int64 WAL registry manifest magic: {found:02x?}"
                )
            }
            Self::ManifestVersion { found, supported } => write!(
                formatter,
                "unsupported Int64 WAL registry manifest version {found}; this reader supports {supported}"
            ),
            Self::ManifestReserved { found } => write!(
                formatter,
                "Int64 WAL registry manifest has nonzero reserved field {found}"
            ),
            Self::ManifestLength { declared, actual } => write!(
                formatter,
                "Int64 WAL registry manifest declares {declared} payload bytes but contains {actual}"
            ),
            Self::ManifestChecksum { expected, actual } => write!(
                formatter,
                "Int64 WAL registry manifest checksum mismatch: expected {expected:08x}, calculated {actual:08x}"
            ),
            Self::ManifestPayload { field } => {
                write!(
                    formatter,
                    "Int64 WAL registry manifest has malformed {field}"
                )
            }
            Self::Empty => formatter.write_str("Int64 WAL registry contains no table descriptors"),
            Self::DuplicateTable { table } => write!(
                formatter,
                "Int64 WAL registry contains duplicate case-insensitive table '{table}'"
            ),
            Self::DuplicateMember { member } => {
                write!(
                    formatter,
                    "Int64 WAL registry contains duplicate member '{member}'"
                )
            }
            Self::InvalidMember { member } => write!(
                formatter,
                "Int64 WAL registry member '{member}' is not a safe single path component"
            ),
            Self::NonDeterministicOrder { previous, table } => write!(
                formatter,
                "Int64 WAL registry table order is not canonical: '{table}' follows '{previous}'"
            ),
            Self::MissingMember { table, member } => write!(
                formatter,
                "Int64 WAL registry table '{table}' is missing member '{member}'"
            ),
            Self::DuplicateMemberFile { table, member } => write!(
                formatter,
                "Int64 WAL registry table '{table}' member '{member}' aliases another member file"
            ),
            Self::UnexpectedDirectoryEntry { entry } => write!(
                formatter,
                "Int64 WAL registry contains unlisted directory entry '{entry}'"
            ),
            Self::TableNameMismatch { expected, found } => write!(
                formatter,
                "Int64 WAL registry descriptor table '{expected}' does not match member bootstrap '{found}'"
            ),
            Self::DatabaseSettingsMismatch { table } => write!(
                formatter,
                "Int64 WAL registry table '{table}' has database settings inconsistent with earlier members"
            ),
        }
    }
}

impl StdError for Int64WriteAheadLogRegistryCorruption {}

/// A filesystem, bound, member-WAL, or manifest failure for a registry.
#[derive(Debug)]
pub enum Int64WriteAheadLogRegistryError {
    /// A directory-wide configured bound was exceeded.
    Limit(Int64WriteAheadLogRegistryLimitError),
    /// The manifest, directory, or member set is structurally inconsistent.
    Corruption(Int64WriteAheadLogRegistryCorruption),
    /// The registry path has no safe, normal, non-NUL final component.
    InvalidDestination,
    /// Opening the registry path's parent directory failed.
    OpenParent(io::Error),
    /// Exclusively creating the registry directory failed.
    CreateDirectory(io::Error),
    /// Opening the created or existing registry directory failed.
    OpenDirectory(io::Error),
    /// Synchronizing the parent after directory creation failed.
    SyncParent(io::Error),
    /// Opening the registry manifest for recovery failed.
    OpenManifest(io::Error),
    /// Reading manifest filesystem metadata failed.
    ManifestMetadata(io::Error),
    /// The opened manifest is not a regular file.
    ManifestNotRegularFile,
    /// Reading the bounded manifest failed.
    ReadManifest(io::Error),
    /// Exclusively creating the registry manifest failed.
    CreateManifest(io::Error),
    /// Writing the registry manifest failed.
    WriteManifest(io::Error),
    /// Synchronizing the registry manifest failed.
    SyncManifest(io::Error),
    /// Synchronizing the registry directory failed.
    SyncDirectory(io::Error),
    /// Enumerating the registry directory for unlisted entries failed.
    ReadDirectory(io::Error),
    /// Creating, opening, bounding, or replaying one member WAL failed.
    Member {
        /// Table associated with the failed member.
        table: String,
        /// Generated or manifest-provided member filename.
        member: String,
        /// Typed member-WAL failure.
        error: Int64WriteAheadLogError,
    },
}

impl fmt::Display for Int64WriteAheadLogRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(error) => error.fmt(formatter),
            Self::Corruption(error) => write!(formatter, "corrupt Int64 WAL registry: {error}"),
            Self::InvalidDestination => formatter.write_str(
                "Int64 WAL registry destination must have one normal non-NUL final path component",
            ),
            Self::OpenParent(error) => write!(formatter, "could not open registry parent: {error}"),
            Self::CreateDirectory(error) => {
                write!(
                    formatter,
                    "could not exclusively create WAL registry directory: {error}"
                )
            }
            Self::OpenDirectory(error) => {
                write!(formatter, "could not open WAL registry directory: {error}")
            }
            Self::SyncParent(error) => write!(
                formatter,
                "could not sync registry parent directory: {error}"
            ),
            Self::OpenManifest(error) => {
                write!(formatter, "could not open WAL registry manifest: {error}")
            }
            Self::ManifestMetadata(error) => write!(
                formatter,
                "could not inspect WAL registry manifest: {error}"
            ),
            Self::ManifestNotRegularFile => {
                formatter.write_str("WAL registry manifest is not a regular file")
            }
            Self::ReadManifest(error) => {
                write!(formatter, "could not read WAL registry manifest: {error}")
            }
            Self::CreateManifest(error) => write!(
                formatter,
                "could not exclusively create WAL registry manifest: {error}"
            ),
            Self::WriteManifest(error) => {
                write!(formatter, "could not write WAL registry manifest: {error}")
            }
            Self::SyncManifest(error) => {
                write!(formatter, "could not sync WAL registry manifest: {error}")
            }
            Self::SyncDirectory(error) => {
                write!(formatter, "could not sync WAL registry directory: {error}")
            }
            Self::ReadDirectory(error) => {
                write!(
                    formatter,
                    "could not enumerate WAL registry directory: {error}"
                )
            }
            Self::Member {
                table,
                member,
                error,
            } => write!(
                formatter,
                "could not process Int64 WAL registry table '{table}' member '{member}': {error}"
            ),
        }
    }
}

impl StdError for Int64WriteAheadLogRegistryError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Limit(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::OpenParent(error)
            | Self::CreateDirectory(error)
            | Self::OpenDirectory(error)
            | Self::SyncParent(error)
            | Self::OpenManifest(error)
            | Self::ManifestMetadata(error)
            | Self::ReadManifest(error)
            | Self::CreateManifest(error)
            | Self::WriteManifest(error)
            | Self::SyncManifest(error)
            | Self::SyncDirectory(error)
            | Self::ReadDirectory(error) => Some(error),
            Self::Member { error, .. } => Some(error),
            Self::InvalidDestination | Self::ManifestNotRegularFile => None,
        }
    }
}

impl From<Int64WriteAheadLogRegistryLimitError> for Int64WriteAheadLogRegistryError {
    fn from(error: Int64WriteAheadLogRegistryLimitError) -> Self {
        Self::Limit(error)
    }
}

impl From<Int64WriteAheadLogRegistryCorruption> for Int64WriteAheadLogRegistryError {
    fn from(error: Int64WriteAheadLogRegistryCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// A configured bound rejected a WAL create, append, or replay operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64WriteAheadLogLimitError {
    FileBytes {
        bytes: u64,
        max_bytes: usize,
    },
    RecordBytes {
        sequence: u64,
        bytes: u64,
        max_bytes: usize,
    },
    Records {
        records: u64,
        max_records: usize,
    },
}

impl fmt::Display for Int64WriteAheadLogLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileBytes { bytes, max_bytes } => write!(
                formatter,
                "Int64 WAL has {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::RecordBytes {
                sequence,
                bytes,
                max_bytes,
            } => write!(
                formatter,
                "Int64 WAL record {sequence} has {bytes} payload bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::Records {
                records,
                max_records,
            } => write!(
                formatter,
                "Int64 WAL has at least {records} committed records, exceeding the limit of {max_records} records"
            ),
        }
    }
}

impl StdError for Int64WriteAheadLogLimitError {}

/// Typed corruption detected in a complete WAL prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64WriteAheadLogCorruption {
    MissingBootstrap,
    IncompatibleMagic {
        offset: u64,
        found: [u8; 8],
    },
    UnsupportedVersion {
        offset: u64,
        found: u16,
        supported: u16,
    },
    UnsupportedKind {
        sequence: u64,
        kind: u8,
    },
    InvalidReservedByte {
        sequence: u64,
        found: u8,
    },
    Sequence {
        expected: u64,
        found: u64,
    },
    CommitMagic {
        sequence: u64,
        found: [u8; 8],
    },
    CommitSequence {
        sequence: u64,
        found: u64,
    },
    Checksum {
        sequence: u64,
        expected: u32,
        actual: u32,
    },
    PayloadLength {
        sequence: u64,
        declared: u64,
        committed: u64,
    },
    UnexpectedBootstrap {
        sequence: u64,
    },
    MalformedPayload {
        sequence: u64,
        field: &'static str,
    },
}

impl fmt::Display for Int64WriteAheadLogCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBootstrap => {
                formatter.write_str("Int64 WAL has no committed bootstrap record")
            }
            Self::IncompatibleMagic { offset, found } => write!(
                formatter,
                "incompatible Int64 WAL magic at byte {offset}: {found:02x?}"
            ),
            Self::UnsupportedVersion {
                offset,
                found,
                supported,
            } => write!(
                formatter,
                "unsupported Int64 WAL version {found} at byte {offset}; this reader supports {supported}"
            ),
            Self::UnsupportedKind { sequence, kind } => {
                write!(
                    formatter,
                    "unsupported Int64 WAL record kind {kind} at sequence {sequence}"
                )
            }
            Self::InvalidReservedByte { sequence, found } => write!(
                formatter,
                "Int64 WAL record {sequence} has nonzero reserved byte {found}"
            ),
            Self::Sequence { expected, found } => write!(
                formatter,
                "Int64 WAL sequence is discontinuous: expected {expected}, found {found}"
            ),
            Self::CommitMagic { sequence, found } => write!(
                formatter,
                "Int64 WAL record {sequence} has invalid commit magic {found:02x?}"
            ),
            Self::CommitSequence { sequence, found } => write!(
                formatter,
                "Int64 WAL record {sequence} has commit sequence {found}"
            ),
            Self::Checksum {
                sequence,
                expected,
                actual,
            } => write!(
                formatter,
                "Int64 WAL record {sequence} checksum mismatch: expected {expected:08x}, calculated {actual:08x}"
            ),
            Self::PayloadLength {
                sequence,
                declared,
                committed,
            } => write!(
                formatter,
                "Int64 WAL record {sequence} declares {declared} payload bytes but its committed payload has {committed} bytes"
            ),
            Self::UnexpectedBootstrap { sequence } => write!(
                formatter,
                "Int64 WAL contains a second bootstrap at sequence {sequence}"
            ),
            Self::MalformedPayload { sequence, field } => write!(
                formatter,
                "Int64 WAL record {sequence} has malformed {field}"
            ),
        }
    }
}

impl StdError for Int64WriteAheadLogCorruption {}

/// A typed filesystem, bound, or corruption failure for an `Int64` WAL.
#[derive(Debug)]
pub enum Int64WriteAheadLogError {
    Limit(Int64WriteAheadLogLimitError),
    Corruption(Int64WriteAheadLogCorruption),
    InvalidDestination,
    OpenParent(io::Error),
    Create(io::Error),
    Open(io::Error),
    Metadata(io::Error),
    NotRegularFile,
    Read(io::Error),
    Write(io::Error),
    SyncFile(io::Error),
    SyncParent(io::Error),
    Poisoned,
}

impl fmt::Display for Int64WriteAheadLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(error) => error.fmt(formatter),
            Self::Corruption(error) => write!(formatter, "corrupt Int64 WAL: {error}"),
            Self::InvalidDestination => formatter.write_str(
                "Int64 WAL destination must have one normal non-NUL final path component",
            ),
            Self::OpenParent(error) => write!(
                formatter,
                "could not open Int64 WAL parent directory: {error}"
            ),
            Self::Create(error) => {
                write!(formatter, "could not exclusively create Int64 WAL: {error}")
            }
            Self::Open(error) => write!(formatter, "could not open Int64 WAL: {error}"),
            Self::Metadata(error) => write!(formatter, "could not inspect Int64 WAL: {error}"),
            Self::NotRegularFile => formatter.write_str("Int64 WAL path is not a regular file"),
            Self::Read(error) => write!(formatter, "could not read Int64 WAL: {error}"),
            Self::Write(error) => write!(formatter, "could not write Int64 WAL: {error}"),
            Self::SyncFile(error) => write!(formatter, "could not sync Int64 WAL file: {error}"),
            Self::SyncParent(error) => write!(
                formatter,
                "could not sync Int64 WAL parent directory: {error}"
            ),
            Self::Poisoned => {
                formatter.write_str("Int64 WAL is unusable after an earlier write or sync failure")
            }
        }
    }
}

impl StdError for Int64WriteAheadLogError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Limit(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::OpenParent(error)
            | Self::Create(error)
            | Self::Open(error)
            | Self::Metadata(error)
            | Self::Read(error)
            | Self::Write(error)
            | Self::SyncFile(error)
            | Self::SyncParent(error) => Some(error),
            Self::InvalidDestination | Self::NotRegularFile | Self::Poisoned => None,
        }
    }
}

impl From<Int64WriteAheadLogLimitError> for Int64WriteAheadLogError {
    fn from(error: Int64WriteAheadLogLimitError) -> Self {
        Self::Limit(error)
    }
}

impl From<Int64WriteAheadLogCorruption> for Int64WriteAheadLogError {
    fn from(error: Int64WriteAheadLogCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// A cloneable, matchable failure returned through a live database mutation.
///
/// Filesystem errors retain their operation, [`io::ErrorKind`], and display
/// text while limit failures retain the complete typed limit variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64WriteAheadLogCommitError {
    Limit(Int64WriteAheadLogLimitError),
    /// A live registry mutation would exceed a directory-wide bound.
    RegistryLimit(Int64WriteAheadLogRegistryLimitError),
    Poisoned,
    Write {
        kind: io::ErrorKind,
        message: String,
    },
    SyncFile {
        kind: io::ErrorKind,
        message: String,
    },
    Unexpected {
        message: String,
    },
}

impl fmt::Display for Int64WriteAheadLogCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(error) => error.fmt(formatter),
            Self::RegistryLimit(error) => error.fmt(formatter),
            Self::Poisoned => {
                formatter.write_str("Int64 WAL is unusable after an earlier write or sync failure")
            }
            Self::Write { message, .. } => {
                write!(formatter, "could not write Int64 WAL: {message}")
            }
            Self::SyncFile { message, .. } => {
                write!(formatter, "could not sync Int64 WAL file: {message}")
            }
            Self::Unexpected { message } => formatter.write_str(message),
        }
    }
}

impl StdError for Int64WriteAheadLogCommitError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Limit(error) => Some(error),
            Self::RegistryLimit(error) => Some(error),
            Self::Poisoned
            | Self::Write { .. }
            | Self::SyncFile { .. }
            | Self::Unexpected { .. } => None,
        }
    }
}

impl From<Int64WriteAheadLogRegistryLimitError> for Int64WriteAheadLogCommitError {
    fn from(error: Int64WriteAheadLogRegistryLimitError) -> Self {
        Self::RegistryLimit(error)
    }
}

impl From<Int64WriteAheadLogError> for Int64WriteAheadLogCommitError {
    fn from(error: Int64WriteAheadLogError) -> Self {
        match error {
            Int64WriteAheadLogError::Limit(error) => Self::Limit(error),
            Int64WriteAheadLogError::Poisoned => Self::Poisoned,
            Int64WriteAheadLogError::Write(error) => Self::Write {
                kind: error.kind(),
                message: error.to_string(),
            },
            Int64WriteAheadLogError::SyncFile(error) => Self::SyncFile {
                kind: error.kind(),
                message: error.to_string(),
            },
            error => Self::Unexpected {
                message: error.to_string(),
            },
        }
    }
}

#[derive(Debug)]
pub(crate) struct Int64WalBootstrap {
    pub(crate) table_name: String,
    pub(crate) column_name: String,
    /// Table-local max rows, columns, and cells.
    pub(crate) table_limits: [usize; 3],
    /// Database defaults for max rows, columns, and cells.
    pub(crate) database_table_limits: [usize; 3],
    pub(crate) query_limits: [usize; QUERY_LIMIT_FIELD_COUNT],
    pub(crate) worker_cap: usize,
    pub(crate) nullable: bool,
    pub(crate) values: Vec<Option<i64>>,
}

#[derive(Debug)]
pub(crate) struct RecoveredInt64WriteAheadLog {
    pub(crate) bootstrap: Int64WalBootstrap,
    pub(crate) file_bytes: usize,
    pub(crate) records: usize,
}

/// Open single-table WAL writer. The database owns this after opt-in.
#[derive(Debug)]
pub(crate) struct Int64WriteAheadLog {
    file: File,
    normalized_table_name: String,
    nullable: bool,
    limits: Int64WriteAheadLogLimits,
    file_bytes: usize,
    records: usize,
    poisoned: bool,
}

impl Int64WriteAheadLog {
    pub(crate) fn validate_bootstrap_limits(
        table_name_bytes: usize,
        column_name_bytes: usize,
        rows: usize,
        nullable: bool,
        limits: Int64WriteAheadLogLimits,
    ) -> Result<(), Int64WriteAheadLogError> {
        let payload_len =
            bootstrap_payload_len(table_name_bytes, column_name_bytes, rows, nullable)
                .unwrap_or(usize::MAX);
        validate_record_limits(0, payload_len, 0, 0, limits).map(|_| ())
    }

    pub(crate) fn create(
        path: &Path,
        bootstrap: &Int64WalBootstrap,
        limits: Int64WriteAheadLogLimits,
    ) -> Result<Self, Int64WriteAheadLogError> {
        Self::validate_bootstrap_limits(
            bootstrap.table_name.len(),
            bootstrap.column_name.len(),
            bootstrap.values.len(),
            bootstrap.nullable,
            limits,
        )?;
        let payload = encode_bootstrap(bootstrap);
        debug_assert_eq!(
            Some(payload.len()),
            bootstrap_payload_len(
                bootstrap.table_name.len(),
                bootstrap.column_name.len(),
                bootstrap.values.len(),
                bootstrap.nullable
            )
        );
        let destination = wal_destination_name(path)?;
        let parent_directory = WalDirectory::open(normalized_parent(path))
            .map_err(Int64WriteAheadLogError::OpenParent)?;
        Self::create_in_directory(&parent_directory, &destination, bootstrap, limits)
    }

    fn create_in_directory(
        directory: &WalDirectory,
        destination: &CStr,
        bootstrap: &Int64WalBootstrap,
        limits: Int64WriteAheadLogLimits,
    ) -> Result<Self, Int64WriteAheadLogError> {
        Self::validate_bootstrap_limits(
            bootstrap.table_name.len(),
            bootstrap.column_name.len(),
            bootstrap.values.len(),
            bootstrap.nullable,
            limits,
        )?;
        let payload = encode_bootstrap(bootstrap);
        let (body, footer) = encode_record_parts(BOOTSTRAP_KIND, 0, &payload);
        let file = create_committed_wal_file(directory, destination, &body, &footer)?;

        Ok(Self {
            file,
            normalized_table_name: bootstrap.table_name.to_ascii_lowercase(),
            nullable: bootstrap.nullable,
            limits,
            file_bytes: body.len() + footer.len(),
            records: 1,
            poisoned: false,
        })
    }

    pub(crate) fn tracks(&self, table_name: &str) -> bool {
        self.normalized_table_name.eq_ignore_ascii_case(table_name)
    }

    fn next_append_bytes(&self, values: usize) -> usize {
        let value_bytes = if self.nullable { 9 } else { 8 };
        INT64_WAL_FRAME_OVERHEAD
            .saturating_add(8)
            .saturating_add(values.saturating_mul(value_bytes))
    }

    fn next_replace_bytes(&self, replacements: usize) -> usize {
        let value_bytes = if self.nullable { 17 } else { 16 };
        INT64_WAL_FRAME_OVERHEAD
            .saturating_add(8)
            .saturating_add(replacements.saturating_mul(value_bytes))
    }

    pub(crate) fn append_values(
        &mut self,
        values: &[Option<i64>],
    ) -> Result<(), Int64WriteAheadLogError> {
        debug_assert!(self.nullable || values.iter().all(Option::is_some));
        let value_bytes = if self.nullable { 9 } else { 8 };
        let payload_len = 8_usize
            .checked_add(values.len().checked_mul(value_bytes).unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX);
        self.validate_next_record(payload_len)?;
        let mut payload = Vec::with_capacity(payload_len);
        push_usize(&mut payload, values.len());
        for value in values {
            if self.nullable {
                push_nullable_i64(&mut payload, *value);
            } else {
                payload.extend_from_slice(
                    &value
                        .expect("a non-nullable WAL cannot receive NULL")
                        .to_le_bytes(),
                );
            }
        }
        self.append_record(
            if self.nullable {
                NULLABLE_APPEND_KIND
            } else {
                APPEND_KIND
            },
            &payload,
        )
    }

    pub(crate) fn truncate(&mut self) -> Result<(), Int64WriteAheadLogError> {
        self.validate_next_record(0)?;
        self.append_record(TRUNCATE_KIND, &[])
    }

    pub(crate) fn replace_values(
        &mut self,
        replacements: &[(usize, Option<i64>)],
    ) -> Result<(), Int64WriteAheadLogError> {
        debug_assert!(self.nullable || replacements.iter().all(|(_, value)| value.is_some()));
        let replacement_bytes = if self.nullable { 17 } else { 16 };
        let payload_len = 8_usize
            .checked_add(
                replacements
                    .len()
                    .checked_mul(replacement_bytes)
                    .unwrap_or(usize::MAX),
            )
            .unwrap_or(usize::MAX);
        self.validate_next_record(payload_len)?;
        let mut payload = Vec::with_capacity(payload_len);
        push_usize(&mut payload, replacements.len());
        for (row, value) in replacements {
            push_usize(&mut payload, *row);
            if self.nullable {
                push_nullable_i64(&mut payload, *value);
            } else {
                payload.extend_from_slice(
                    &value
                        .expect("a non-nullable WAL cannot receive NULL")
                        .to_le_bytes(),
                );
            }
        }
        self.append_record(
            if self.nullable {
                NULLABLE_REPLACE_KIND
            } else {
                REPLACE_KIND
            },
            &payload,
        )
    }

    fn validate_next_record(&self, payload_len: usize) -> Result<(), Int64WriteAheadLogError> {
        if self.poisoned {
            return Err(Int64WriteAheadLogError::Poisoned);
        }
        validate_record_limits(
            self.records as u64,
            payload_len,
            self.file_bytes,
            self.records,
            self.limits,
        )
        .map(|_| ())
    }

    fn append_record(&mut self, kind: u8, payload: &[u8]) -> Result<(), Int64WriteAheadLogError> {
        if self.poisoned {
            return Err(Int64WriteAheadLogError::Poisoned);
        }
        let sequence = self.records as u64;
        let next_file_bytes = validate_record_limits(
            sequence,
            payload.len(),
            self.file_bytes,
            self.records,
            self.limits,
        )?;
        let (body, footer) = encode_record_parts(kind, sequence, payload);
        if let Err(error) = write_committed_record(&mut self.file, &body, &footer) {
            self.poisoned = true;
            return Err(error);
        }
        self.file_bytes = next_file_bytes;
        self.records += 1;
        Ok(())
    }
}

trait DurableWalFile {
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn sync_bytes(&mut self) -> io::Result<()>;
}

impl DurableWalFile for File {
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }

    fn sync_bytes(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

/// Persists the record body before making its commit footer durable.
///
/// Recovery can therefore treat every short header, body, or footer as an
/// uncommitted tail. If any footer bytes reach storage, the header and payload
/// were already synchronized successfully.
fn write_committed_record(
    file: &mut impl DurableWalFile,
    body: &[u8],
    footer: &[u8; INT64_WAL_COMMIT_LEN],
) -> Result<(), Int64WriteAheadLogError> {
    file.write_bytes(body)
        .map_err(Int64WriteAheadLogError::Write)?;
    file.sync_bytes()
        .map_err(Int64WriteAheadLogError::SyncFile)?;
    file.write_bytes(footer)
        .map_err(Int64WriteAheadLogError::Write)?;
    file.sync_bytes().map_err(Int64WriteAheadLogError::SyncFile)
}

fn create_committed_wal_file(
    directory: &WalDirectory,
    destination: &CStr,
    body: &[u8],
    footer: &[u8; INT64_WAL_COMMIT_LEN],
) -> Result<File, Int64WriteAheadLogError> {
    let mut file = directory
        .create(destination)
        .map_err(Int64WriteAheadLogError::Create)?;
    write_committed_record(&mut file, body, footer)?;
    directory
        .sync()
        .map_err(Int64WriteAheadLogError::SyncParent)?;
    Ok(file)
}

fn wal_destination_name(path: &Path) -> Result<CString, Int64WriteAheadLogError> {
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.ends_with(b"/") || path_bytes.ends_with(b"/.") {
        return Err(Int64WriteAheadLogError::InvalidDestination);
    }
    let name = match path.components().next_back() {
        Some(std::path::Component::Normal(name)) => name,
        _ => return Err(Int64WriteAheadLogError::InvalidDestination),
    };
    CString::new(name.as_bytes()).map_err(|_| Int64WriteAheadLogError::InvalidDestination)
}

fn normalized_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

struct WalDirectory {
    file: File,
}

impl WalDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self { file })
    }

    fn create(&self, name: &CStr) -> io::Result<File> {
        // SAFETY: the directory descriptor is open, `name` is NUL-terminated,
        // and ownership is assumed only after `openat` returns a new descriptor.
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
        // SAFETY: successful `openat` returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn open_file(&self, name: &CStr) -> io::Result<File> {
        // `O_NONBLOCK` prevents a malicious FIFO manifest/member from waiting
        // for a writer. `O_NOFOLLOW` binds validation to the opened inode.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn create_directory(&self, name: &CStr) -> io::Result<()> {
        let result =
            unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o777 as libc::mode_t) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open_directory(&self, name: &CStr) -> io::Result<Self> {
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file: unsafe { File::from_raw_fd(descriptor) },
        })
    }

    fn first_unexpected_entry(&self, expected: &HashSet<Vec<u8>>) -> io::Result<Option<Vec<u8>>> {
        let descriptor = unsafe { libc::dup(self.file.as_raw_fd()) };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(descriptor);
            }
            return Err(error);
        }
        let mut entry_buffer = MaybeUninit::<libc::dirent>::uninit();
        let result = loop {
            let entry = match next_directory_entry(stream, &mut entry_buffer) {
                Ok(entry) if entry.is_null() => break Ok(None),
                Ok(entry) => entry,
                Err(error) => break Err(error),
            };
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." && !expected.contains(name) {
                break Ok(Some(name.to_vec()));
            }
        };
        let close_result = unsafe { libc::closedir(stream) };
        match result {
            Err(error) => Err(error),
            Ok(_) if close_result == -1 => Err(io::Error::last_os_error()),
            Ok(unexpected) => Ok(unexpected),
        }
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "emscripten",
    target_os = "hurd",
    target_os = "redox",
    target_os = "android",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
fn next_directory_entry(
    stream: *mut libc::DIR,
    _entry_buffer: &mut MaybeUninit<libc::dirent>,
) -> io::Result<*mut libc::dirent> {
    // `readdir` distinguishes EOF from failure only through thread-local
    // errno, so clear it before every call and inspect it on a null result.
    let errno = directory_errno_location();
    unsafe {
        *errno = 0;
    }
    let entry = unsafe { libc::readdir(stream) };
    let error = unsafe { *errno };
    if entry.is_null() && error != 0 {
        Err(io::Error::from_raw_os_error(error))
    } else {
        Ok(entry)
    }
}

#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
fn directory_errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "emscripten",
    target_os = "hurd",
    target_os = "redox"
))]
fn directory_errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
fn directory_errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno() }
}

#[cfg(any(target_os = "solaris", target_os = "illumos"))]
fn directory_errno_location() -> *mut libc::c_int {
    unsafe { libc::___errno() }
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "emscripten",
    target_os = "hurd",
    target_os = "redox",
    target_os = "android",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn next_directory_entry(
    stream: *mut libc::DIR,
    entry_buffer: &mut MaybeUninit<libc::dirent>,
) -> io::Result<*mut libc::dirent> {
    let mut entry = std::ptr::null_mut();
    let error = unsafe { libc::readdir_r(stream, entry_buffer.as_mut_ptr(), &mut entry) };
    if error == 0 {
        Ok(entry)
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

#[derive(Debug, Clone)]
struct RegistryDescriptor {
    table: String,
    member: String,
}

/// Allocation-free table payload metadata used to reject a registry before
/// any table values are cloned or a manifest buffer is materialized.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Int64WalRegistryTablePreflight<'a> {
    pub(crate) table_name: &'a str,
    pub(crate) column_name: &'a str,
    pub(crate) rows: usize,
    pub(crate) nullable: bool,
}

#[derive(Debug)]
pub(crate) struct RecoveredInt64WriteAheadLogRegistry {
    pub(crate) tables: Vec<Int64WalBootstrap>,
}

#[derive(Debug)]
pub(crate) struct Int64WriteAheadLogRegistry {
    writers: HashMap<String, Int64WriteAheadLog>,
    limits: Int64WriteAheadLogRegistryLimits,
    total_wal_bytes: usize,
    total_records: usize,
    poisoned: bool,
}

/// One attached legacy WAL or one attached multi-table registry.
#[derive(Debug)]
pub(crate) enum ActiveInt64WriteAheadLogs {
    Single(Int64WriteAheadLog),
    Registry(Int64WriteAheadLogRegistry),
}

impl ActiveInt64WriteAheadLogs {
    pub(crate) fn single(writer: Int64WriteAheadLog) -> Self {
        Self::Single(writer)
    }

    pub(crate) fn create_registry(
        path: &Path,
        mut bootstraps: Vec<Int64WalBootstrap>,
        limits: Int64WriteAheadLogRegistryLimits,
    ) -> Result<Self, Int64WriteAheadLogRegistryError> {
        let preflight = bootstraps
            .iter()
            .map(|bootstrap| Int64WalRegistryTablePreflight {
                table_name: &bootstrap.table_name,
                column_name: &bootstrap.column_name,
                rows: bootstrap.values.len(),
                nullable: bootstrap.nullable,
            })
            .collect::<Vec<_>>();
        preflight_registry_tables(&preflight, limits)?;
        bootstraps.sort_by(|left, right| {
            left.table_name
                .to_ascii_lowercase()
                .cmp(&right.table_name.to_ascii_lowercase())
        });
        let descriptors = bootstraps
            .iter()
            .enumerate()
            .map(|(index, bootstrap)| RegistryDescriptor {
                table: bootstrap.table_name.clone(),
                member: format!("table-{index:08}.wal"),
            })
            .collect::<Vec<_>>();
        let manifest = encode_registry_manifest(&descriptors);

        let bootstrap_bytes = bootstraps
            .iter()
            .map(|bootstrap| {
                bootstrap_payload_len(
                    bootstrap.table_name.len(),
                    bootstrap.column_name.len(),
                    bootstrap.values.len(),
                    bootstrap.nullable,
                )
                .unwrap_or(usize::MAX)
                .saturating_add(INT64_WAL_FRAME_OVERHEAD)
            })
            .fold(0_usize, usize::saturating_add);
        validate_registry_totals(bootstrap_bytes, bootstraps.len(), limits)?;

        let destination = registry_destination_name(path)?;
        let parent = WalDirectory::open(normalized_parent(path))
            .map_err(Int64WriteAheadLogRegistryError::OpenParent)?;
        parent
            .create_directory(&destination)
            .map_err(Int64WriteAheadLogRegistryError::CreateDirectory)?;
        parent
            .sync()
            .map_err(Int64WriteAheadLogRegistryError::SyncParent)?;
        let directory = parent
            .open_directory(&destination)
            .map_err(Int64WriteAheadLogRegistryError::OpenDirectory)?;

        let mut writers = HashMap::with_capacity(bootstraps.len());
        for (bootstrap, descriptor) in bootstraps.iter().zip(&descriptors) {
            let member = CString::new(descriptor.member.as_bytes())
                .expect("generated registry member names contain no NUL");
            let writer = Int64WriteAheadLog::create_in_directory(
                &directory,
                &member,
                bootstrap,
                limits.per_table,
            )
            .map_err(|error| member_error(bootstrap, &descriptor.member, error))?;
            writers.insert(bootstrap.table_name.to_ascii_lowercase(), writer);
        }

        let manifest_name = CString::new(REGISTRY_MANIFEST_NAME).expect("manifest name is valid");
        let mut manifest_file = directory
            .create(&manifest_name)
            .map_err(Int64WriteAheadLogRegistryError::CreateManifest)?;
        manifest_file
            .write_all(&manifest)
            .map_err(Int64WriteAheadLogRegistryError::WriteManifest)?;
        manifest_file
            .sync_all()
            .map_err(Int64WriteAheadLogRegistryError::SyncManifest)?;
        directory
            .sync()
            .map_err(Int64WriteAheadLogRegistryError::SyncDirectory)?;

        Ok(Self::Registry(Int64WriteAheadLogRegistry {
            writers,
            limits,
            total_wal_bytes: bootstrap_bytes,
            total_records: descriptors.len(),
            poisoned: false,
        }))
    }

    pub(crate) fn tracks(&self, table: &str) -> bool {
        match self {
            Self::Single(writer) => writer.tracks(table),
            Self::Registry(registry) => registry.writers.contains_key(&table.to_ascii_lowercase()),
        }
    }

    pub(crate) fn append_values(
        &mut self,
        table: &str,
        values: &[Option<i64>],
    ) -> Result<(), Int64WriteAheadLogCommitError> {
        match self {
            Self::Single(writer) => writer.append_values(values).map_err(Into::into),
            Self::Registry(registry) => registry.append_values(table, values),
        }
    }

    pub(crate) fn truncate(&mut self, table: &str) -> Result<(), Int64WriteAheadLogCommitError> {
        match self {
            Self::Single(writer) => writer.truncate().map_err(Into::into),
            Self::Registry(registry) => registry.truncate(table),
        }
    }

    pub(crate) fn replace_values(
        &mut self,
        table: &str,
        replacements: &[(usize, Option<i64>)],
    ) -> Result<(), Int64WriteAheadLogCommitError> {
        match self {
            Self::Single(writer) => writer.replace_values(replacements).map_err(Into::into),
            Self::Registry(registry) => registry.replace_values(table, replacements),
        }
    }
}

impl Int64WriteAheadLogRegistry {
    fn reserve_record(&self, bytes: usize) -> Result<(), Int64WriteAheadLogCommitError> {
        if self.poisoned {
            return Err(Int64WriteAheadLogCommitError::Poisoned);
        }
        let total_bytes = self.total_wal_bytes.saturating_add(bytes);
        let total_records = self.total_records.saturating_add(1);
        validate_registry_totals(total_bytes, total_records, self.limits).map_err(|error| {
            match error {
                Int64WriteAheadLogRegistryError::Limit(error) => error.into(),
                _ => unreachable!("registry total validation only returns limit errors"),
            }
        })
    }

    fn commit_member_record<F>(
        &mut self,
        key: &str,
        bytes: usize,
        commit: F,
    ) -> Result<(), Int64WriteAheadLogCommitError>
    where
        F: FnOnce(&mut Int64WriteAheadLog) -> Result<(), Int64WriteAheadLogError>,
    {
        self.reserve_record(bytes)?;
        let result = commit(
            self.writers
                .get_mut(key)
                .expect("the caller checks registry membership"),
        );
        match result {
            Ok(()) => {
                self.total_wal_bytes += bytes;
                self.total_records += 1;
                Ok(())
            }
            Err(error) => {
                if !matches!(error, Int64WriteAheadLogError::Limit(_)) {
                    self.poisoned = true;
                }
                Err(error.into())
            }
        }
    }

    fn append_values(
        &mut self,
        table: &str,
        values: &[Option<i64>],
    ) -> Result<(), Int64WriteAheadLogCommitError> {
        let key = table.to_ascii_lowercase();
        let bytes = self
            .writers
            .get(&key)
            .expect("the caller checks registry membership")
            .next_append_bytes(values.len());
        self.commit_member_record(&key, bytes, |writer| writer.append_values(values))
    }

    fn truncate(&mut self, table: &str) -> Result<(), Int64WriteAheadLogCommitError> {
        let key = table.to_ascii_lowercase();
        self.commit_member_record(&key, INT64_WAL_FRAME_OVERHEAD, Int64WriteAheadLog::truncate)
    }

    fn replace_values(
        &mut self,
        table: &str,
        replacements: &[(usize, Option<i64>)],
    ) -> Result<(), Int64WriteAheadLogCommitError> {
        let key = table.to_ascii_lowercase();
        let bytes = self
            .writers
            .get(&key)
            .expect("the caller checks registry membership")
            .next_replace_bytes(replacements.len());
        self.commit_member_record(&key, bytes, |writer| writer.replace_values(replacements))
    }
}

fn member_error(
    bootstrap: &Int64WalBootstrap,
    member: &str,
    error: Int64WriteAheadLogError,
) -> Int64WriteAheadLogRegistryError {
    Int64WriteAheadLogRegistryError::Member {
        table: bootstrap.table_name.clone(),
        member: member.to_owned(),
        error,
    }
}

fn registry_destination_name(path: &Path) -> Result<CString, Int64WriteAheadLogRegistryError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(b"/") || bytes.ends_with(b"/.") {
        return Err(Int64WriteAheadLogRegistryError::InvalidDestination);
    }
    let name = match path.components().next_back() {
        Some(std::path::Component::Normal(name)) => name,
        _ => return Err(Int64WriteAheadLogRegistryError::InvalidDestination),
    };
    CString::new(name.as_bytes()).map_err(|_| Int64WriteAheadLogRegistryError::InvalidDestination)
}

pub(crate) fn validate_registry_table_count(
    tables: usize,
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<(), Int64WriteAheadLogRegistryError> {
    if tables == 0 {
        return Err(Int64WriteAheadLogRegistryCorruption::Empty.into());
    }
    if tables > limits.max_tables {
        return Err(Int64WriteAheadLogRegistryLimitError::Tables {
            tables: tables as u64,
            max_tables: limits.max_tables,
        }
        .into());
    }
    Ok(())
}

pub(crate) fn preflight_registry_tables(
    tables: &[Int64WalRegistryTablePreflight<'_>],
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<(), Int64WriteAheadLogRegistryError> {
    validate_registry_table_count(tables.len(), limits)?;
    let mut table_names = HashSet::with_capacity(tables.len());
    for table in tables {
        if !table_names.insert(table.table_name.to_ascii_lowercase()) {
            return Err(Int64WriteAheadLogRegistryCorruption::DuplicateTable {
                table: table.table_name.to_owned(),
            }
            .into());
        }
    }

    let mut bootstrap_bytes = 0_usize;
    let mut manifest_bytes = REGISTRY_MANIFEST_HEADER_LEN;
    for (index, table) in tables.iter().enumerate() {
        Int64WriteAheadLog::validate_bootstrap_limits(
            table.table_name.len(),
            table.column_name.len(),
            table.rows,
            table.nullable,
            limits.per_table,
        )
        .map_err(|error| Int64WriteAheadLogRegistryError::Member {
            table: table.table_name.to_owned(),
            member: "bootstrap".to_owned(),
            error,
        })?;
        let member_bytes = generated_registry_member_name_len(index);
        manifest_bytes = manifest_bytes
            .saturating_add(4)
            .saturating_add(table.table_name.len())
            .saturating_add(4)
            .saturating_add(member_bytes);
        bootstrap_bytes = bootstrap_bytes.saturating_add(
            bootstrap_payload_len(
                table.table_name.len(),
                table.column_name.len(),
                table.rows,
                table.nullable,
            )
            .unwrap_or(usize::MAX)
            .saturating_add(INT64_WAL_FRAME_OVERHEAD),
        );
    }
    validate_manifest_size(manifest_bytes, limits)?;
    validate_registry_totals(bootstrap_bytes, tables.len(), limits)
}

fn generated_registry_member_name_len(index: usize) -> usize {
    // `table-`, at least eight decimal digits, and `.wal`.
    let mut digits = 1_usize;
    let mut remaining = index;
    while remaining >= 10 {
        remaining /= 10;
        digits += 1;
    }
    6 + digits.max(8) + 4
}

fn validate_manifest_size(
    bytes: usize,
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<(), Int64WriteAheadLogRegistryError> {
    if bytes > limits.max_manifest_bytes {
        return Err(Int64WriteAheadLogRegistryLimitError::ManifestBytes {
            bytes: bytes as u64,
            max_bytes: limits.max_manifest_bytes,
        }
        .into());
    }
    Ok(())
}

fn validate_registry_totals(
    bytes: usize,
    records: usize,
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<(), Int64WriteAheadLogRegistryError> {
    if bytes > limits.max_total_wal_bytes {
        return Err(Int64WriteAheadLogRegistryLimitError::TotalWalBytes {
            bytes: bytes as u64,
            max_bytes: limits.max_total_wal_bytes,
        }
        .into());
    }
    if records > limits.max_total_records {
        return Err(Int64WriteAheadLogRegistryLimitError::TotalRecords {
            records: records as u64,
            max_records: limits.max_total_records,
        }
        .into());
    }
    Ok(())
}

fn encode_registry_manifest(descriptors: &[RegistryDescriptor]) -> Vec<u8> {
    let mut payload = Vec::new();
    for descriptor in descriptors {
        push_registry_bytes(&mut payload, descriptor.table.as_bytes());
        push_registry_bytes(&mut payload, descriptor.member.as_bytes());
    }
    let checksum = registry_manifest_checksum(descriptors.len(), &payload);
    let mut manifest = Vec::with_capacity(REGISTRY_MANIFEST_HEADER_LEN + payload.len());
    manifest.extend_from_slice(&REGISTRY_MANIFEST_MAGIC);
    manifest.extend_from_slice(&REGISTRY_MANIFEST_VERSION.to_le_bytes());
    manifest.extend_from_slice(&0_u16.to_le_bytes());
    manifest.extend_from_slice(&(descriptors.len() as u32).to_le_bytes());
    manifest.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    manifest.extend_from_slice(&checksum.to_le_bytes());
    manifest.extend_from_slice(&payload);
    manifest
}

fn push_registry_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn registry_manifest_checksum(entries: usize, payload: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    checksum = crc32_update(checksum, &REGISTRY_MANIFEST_VERSION.to_le_bytes());
    checksum = crc32_update(checksum, &0_u16.to_le_bytes());
    checksum = crc32_update(checksum, &(entries as u32).to_le_bytes());
    checksum = crc32_update(checksum, &(payload.len() as u64).to_le_bytes());
    checksum = crc32_update(checksum, payload);
    !checksum
}

fn decode_registry_manifest(
    bytes: &[u8],
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<Vec<RegistryDescriptor>, Int64WriteAheadLogRegistryError> {
    if bytes.len() < REGISTRY_MANIFEST_HEADER_LEN {
        return Err(
            Int64WriteAheadLogRegistryCorruption::ManifestPayload { field: "header" }.into(),
        );
    }
    let magic = read_array::<8>(bytes, 0);
    if magic != REGISTRY_MANIFEST_MAGIC {
        return Err(Int64WriteAheadLogRegistryCorruption::ManifestMagic { found: magic }.into());
    }
    let version = u16::from_le_bytes(read_array::<2>(bytes, 8));
    if version != REGISTRY_MANIFEST_VERSION {
        return Err(Int64WriteAheadLogRegistryCorruption::ManifestVersion {
            found: version,
            supported: REGISTRY_MANIFEST_VERSION,
        }
        .into());
    }
    let reserved = u16::from_le_bytes(read_array::<2>(bytes, 10));
    if reserved != 0 {
        return Err(
            Int64WriteAheadLogRegistryCorruption::ManifestReserved { found: reserved }.into(),
        );
    }
    let count = u32::from_le_bytes(read_array::<4>(bytes, 12)) as usize;
    validate_registry_table_count(count, limits)?;
    let declared = u64::from_le_bytes(read_array::<8>(bytes, 16));
    let actual = bytes.len().saturating_sub(REGISTRY_MANIFEST_HEADER_LEN) as u64;
    if declared != actual {
        return Err(
            Int64WriteAheadLogRegistryCorruption::ManifestLength { declared, actual }.into(),
        );
    }
    let expected = u32::from_le_bytes(read_array::<4>(bytes, 24));
    let payload = &bytes[REGISTRY_MANIFEST_HEADER_LEN..];
    let actual_checksum = registry_manifest_checksum(count, payload);
    if expected != actual_checksum {
        return Err(Int64WriteAheadLogRegistryCorruption::ManifestChecksum {
            expected,
            actual: actual_checksum,
        }
        .into());
    }
    if count > payload.len() / REGISTRY_DESCRIPTOR_MIN_PAYLOAD_LEN {
        return Err(Int64WriteAheadLogRegistryCorruption::ManifestPayload {
            field: "descriptor count",
        }
        .into());
    }

    let mut reader = RegistryManifestReader::new(payload);
    let mut descriptors = Vec::new();
    let mut tables = HashSet::new();
    let mut members = HashSet::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let table = reader.string("table name")?;
        let member = reader.string("member name")?;
        let normalized = table.to_ascii_lowercase();
        if !tables.insert(normalized.clone()) {
            return Err(Int64WriteAheadLogRegistryCorruption::DuplicateTable { table }.into());
        }
        if let Some(previous) = &previous {
            if normalized <= *previous {
                return Err(
                    Int64WriteAheadLogRegistryCorruption::NonDeterministicOrder {
                        previous: descriptors
                            .last()
                            .map_or_else(String::new, |descriptor: &RegistryDescriptor| {
                                descriptor.table.clone()
                            }),
                        table,
                    }
                    .into(),
                );
            }
        }
        previous = Some(normalized);
        let normalized_member = member.to_ascii_lowercase();
        if !members.insert(normalized_member) {
            return Err(Int64WriteAheadLogRegistryCorruption::DuplicateMember { member }.into());
        }
        if !safe_registry_member(&member) {
            return Err(Int64WriteAheadLogRegistryCorruption::InvalidMember { member }.into());
        }
        descriptors.push(RegistryDescriptor { table, member });
    }
    reader.finish()?;
    Ok(descriptors)
}

fn safe_registry_member(member: &str) -> bool {
    !member.is_empty()
        && member != "."
        && member != ".."
        && !member.eq_ignore_ascii_case(REGISTRY_MANIFEST_NAME)
        && !member.as_bytes().contains(&0)
        && !member.as_bytes().contains(&b'/')
        && Path::new(member)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

struct RegistryManifestReader<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> RegistryManifestReader<'a> {
    const fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn string(&mut self, field: &'static str) -> Result<String, Int64WriteAheadLogRegistryError> {
        let length_end = self
            .offset
            .checked_add(4)
            .ok_or(Int64WriteAheadLogRegistryCorruption::ManifestPayload { field })?;
        let length_bytes = self
            .payload
            .get(self.offset..length_end)
            .ok_or(Int64WriteAheadLogRegistryCorruption::ManifestPayload { field })?;
        let length = u32::from_le_bytes(
            length_bytes
                .try_into()
                .expect("manifest length field has four bytes"),
        ) as usize;
        let end = length_end
            .checked_add(length)
            .ok_or(Int64WriteAheadLogRegistryCorruption::ManifestPayload { field })?;
        let value = std::str::from_utf8(
            self.payload
                .get(length_end..end)
                .ok_or(Int64WriteAheadLogRegistryCorruption::ManifestPayload { field })?,
        )
        .map_err(|_| Int64WriteAheadLogRegistryCorruption::ManifestPayload { field })?
        .to_owned();
        self.offset = end;
        Ok(value)
    }

    fn finish(self) -> Result<(), Int64WriteAheadLogRegistryError> {
        if self.offset == self.payload.len() {
            Ok(())
        } else {
            Err(Int64WriteAheadLogRegistryCorruption::ManifestPayload {
                field: "trailing bytes",
            }
            .into())
        }
    }
}

pub(crate) fn recover_registry(
    path: &Path,
    limits: Int64WriteAheadLogRegistryLimits,
) -> Result<RecoveredInt64WriteAheadLogRegistry, Int64WriteAheadLogRegistryError> {
    use std::os::unix::fs::MetadataExt;

    let destination = registry_destination_name(path)?;
    let parent = WalDirectory::open(normalized_parent(path))
        .map_err(Int64WriteAheadLogRegistryError::OpenParent)?;
    let directory = parent
        .open_directory(&destination)
        .map_err(Int64WriteAheadLogRegistryError::OpenDirectory)?;
    let manifest_name = CString::new(REGISTRY_MANIFEST_NAME).expect("manifest name is valid");
    let manifest_file = directory
        .open_file(&manifest_name)
        .map_err(Int64WriteAheadLogRegistryError::OpenManifest)?;
    let metadata = manifest_file
        .metadata()
        .map_err(Int64WriteAheadLogRegistryError::ManifestMetadata)?;
    if !metadata.is_file() {
        return Err(Int64WriteAheadLogRegistryError::ManifestNotRegularFile);
    }
    if metadata.len() > limits.max_manifest_bytes as u64 {
        return Err(Int64WriteAheadLogRegistryLimitError::ManifestBytes {
            bytes: metadata.len(),
            max_bytes: limits.max_manifest_bytes,
        }
        .into());
    }
    let mut manifest = Vec::with_capacity(metadata.len() as usize);
    manifest_file
        .take((limits.max_manifest_bytes as u64).saturating_add(1))
        .read_to_end(&mut manifest)
        .map_err(Int64WriteAheadLogRegistryError::ReadManifest)?;
    validate_manifest_size(manifest.len(), limits)?;
    let descriptors = decode_registry_manifest(&manifest, limits)?;
    let expected_entries = descriptors
        .iter()
        .map(|descriptor| descriptor.member.as_bytes().to_vec())
        .chain(std::iter::once(REGISTRY_MANIFEST_NAME.as_bytes().to_vec()))
        .collect::<HashSet<_>>();
    if let Some(entry) = directory
        .first_unexpected_entry(&expected_entries)
        .map_err(Int64WriteAheadLogRegistryError::ReadDirectory)?
    {
        return Err(
            Int64WriteAheadLogRegistryCorruption::UnexpectedDirectoryEntry {
                entry: String::from_utf8_lossy(&entry).into_owned(),
            }
            .into(),
        );
    }

    let mut tables = Vec::with_capacity(descriptors.len());
    let mut inodes = HashSet::with_capacity(descriptors.len());
    let mut total_bytes = 0_usize;
    let mut total_records = 0_usize;
    let mut database_settings: Option<([usize; 3], [usize; QUERY_LIMIT_FIELD_COUNT], usize)> = None;
    for descriptor in descriptors {
        let member_name = CString::new(descriptor.member.as_bytes()).map_err(|_| {
            Int64WriteAheadLogRegistryCorruption::InvalidMember {
                member: descriptor.member.clone(),
            }
        })?;
        let file = match directory.open_file(&member_name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(Int64WriteAheadLogRegistryCorruption::MissingMember {
                    table: descriptor.table,
                    member: descriptor.member,
                }
                .into());
            }
            Err(error) => {
                return Err(Int64WriteAheadLogRegistryError::Member {
                    table: descriptor.table,
                    member: descriptor.member,
                    error: Int64WriteAheadLogError::Open(error),
                });
            }
        };
        let member_metadata =
            file.metadata()
                .map_err(|error| Int64WriteAheadLogRegistryError::Member {
                    table: descriptor.table.clone(),
                    member: descriptor.member.clone(),
                    error: Int64WriteAheadLogError::Metadata(error),
                })?;
        if !member_metadata.is_file() {
            return Err(Int64WriteAheadLogRegistryError::Member {
                table: descriptor.table,
                member: descriptor.member,
                error: Int64WriteAheadLogError::NotRegularFile,
            });
        }
        if !inodes.insert((member_metadata.dev(), member_metadata.ino())) {
            return Err(Int64WriteAheadLogRegistryCorruption::DuplicateMemberFile {
                table: descriptor.table,
                member: descriptor.member,
            }
            .into());
        }
        let metadata_bytes = usize::try_from(member_metadata.len()).unwrap_or(usize::MAX);
        validate_registry_totals(
            total_bytes.saturating_add(metadata_bytes),
            total_records,
            limits,
        )?;
        let remaining_records = limits.max_total_records.saturating_sub(total_records);
        let aggregate_record_limit_is_binding = remaining_records <= limits.per_table.max_records;
        let member_limits = Int64WriteAheadLogLimits {
            max_records: limits.per_table.max_records.min(remaining_records),
            ..limits.per_table
        };
        let recovered = match recover_file(file, member_metadata.len(), member_limits) {
            Err(Int64WriteAheadLogError::Limit(Int64WriteAheadLogLimitError::Records {
                records,
                ..
            })) if aggregate_record_limit_is_binding => {
                return Err(Int64WriteAheadLogRegistryLimitError::TotalRecords {
                    records: u64::try_from(total_records)
                        .unwrap_or(u64::MAX)
                        .saturating_add(records),
                    max_records: limits.max_total_records,
                }
                .into());
            }
            Err(error) => {
                return Err(Int64WriteAheadLogRegistryError::Member {
                    table: descriptor.table.clone(),
                    member: descriptor.member.clone(),
                    error,
                });
            }
            Ok(recovered) => recovered,
        };
        total_bytes = total_bytes.saturating_add(recovered.file_bytes);
        total_records = total_records.saturating_add(recovered.records);
        validate_registry_totals(total_bytes, total_records, limits)?;
        if recovered.bootstrap.table_name != descriptor.table {
            return Err(Int64WriteAheadLogRegistryCorruption::TableNameMismatch {
                expected: descriptor.table,
                found: recovered.bootstrap.table_name,
            }
            .into());
        }
        let settings = (
            recovered.bootstrap.database_table_limits,
            recovered.bootstrap.query_limits,
            recovered.bootstrap.worker_cap,
        );
        if let Some(expected) = database_settings {
            if settings != expected {
                return Err(
                    Int64WriteAheadLogRegistryCorruption::DatabaseSettingsMismatch {
                        table: recovered.bootstrap.table_name,
                    }
                    .into(),
                );
            }
        } else {
            database_settings = Some(settings);
        }
        tables.push(recovered.bootstrap);
    }
    Ok(RecoveredInt64WriteAheadLogRegistry { tables })
}

pub(crate) fn recover(
    path: &Path,
    limits: Int64WriteAheadLogLimits,
) -> Result<RecoveredInt64WriteAheadLog, Int64WriteAheadLogError> {
    use std::os::unix::fs::OpenOptionsExt;

    // A blocking read-only open waits indefinitely for a writer when `path`
    // is a FIFO. Open nonblocking first, then validate the opened descriptor
    // so a path replacement cannot bypass the regular-file check.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(Int64WriteAheadLogError::Open)?;
    let metadata = file.metadata().map_err(Int64WriteAheadLogError::Metadata)?;
    if !metadata.is_file() {
        return Err(Int64WriteAheadLogError::NotRegularFile);
    }
    recover_file(file, metadata.len(), limits)
}

fn recover_file(
    file: File,
    metadata_len: u64,
    limits: Int64WriteAheadLogLimits,
) -> Result<RecoveredInt64WriteAheadLog, Int64WriteAheadLogError> {
    if metadata_len > limits.max_file_bytes as u64 {
        return Err(Int64WriteAheadLogLimitError::FileBytes {
            bytes: metadata_len,
            max_bytes: limits.max_file_bytes,
        }
        .into());
    }
    let read_limit = (limits.max_file_bytes as u64).saturating_add(1);
    let mut bytes = Vec::with_capacity(metadata_len as usize);
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(Int64WriteAheadLogError::Read)?;
    if bytes.len() > limits.max_file_bytes {
        return Err(Int64WriteAheadLogLimitError::FileBytes {
            bytes: bytes.len() as u64,
            max_bytes: limits.max_file_bytes,
        }
        .into());
    }
    replay(&bytes, limits)
}

fn replay(
    bytes: &[u8],
    limits: Int64WriteAheadLogLimits,
) -> Result<RecoveredInt64WriteAheadLog, Int64WriteAheadLogError> {
    let mut offset = 0_usize;
    let mut expected_sequence = 0_u64;
    let mut bootstrap: Option<Int64WalBootstrap> = None;

    while bytes.len().saturating_sub(offset) >= INT64_WAL_FRAME_HEADER_LEN {
        let header = &bytes[offset..offset + INT64_WAL_FRAME_HEADER_LEN];
        let found_magic = read_array::<8>(header, 0);
        if found_magic != INT64_WAL_MAGIC {
            return Err(Int64WriteAheadLogCorruption::IncompatibleMagic {
                offset: offset as u64,
                found: found_magic,
            }
            .into());
        }
        let version = u16::from_le_bytes(read_array::<2>(header, 8));
        if version != INT64_WAL_VERSION {
            return Err(Int64WriteAheadLogCorruption::UnsupportedVersion {
                offset: offset as u64,
                found: version,
                supported: INT64_WAL_VERSION,
            }
            .into());
        }
        let kind = header[10];
        let reserved = header[11];
        let sequence = u64::from_le_bytes(read_array::<8>(header, 12));
        let payload_len_u64 = u64::from_le_bytes(read_array::<8>(header, 20));
        let expected_checksum = u32::from_le_bytes(read_array::<4>(header, 28));
        if reserved != 0 {
            return Err(Int64WriteAheadLogCorruption::InvalidReservedByte {
                sequence,
                found: reserved,
            }
            .into());
        }
        if sequence != expected_sequence {
            return Err(Int64WriteAheadLogCorruption::Sequence {
                expected: expected_sequence,
                found: sequence,
            }
            .into());
        }
        if payload_len_u64 > limits.max_record_bytes as u64 {
            return Err(Int64WriteAheadLogLimitError::RecordBytes {
                sequence,
                bytes: payload_len_u64,
                max_bytes: limits.max_record_bytes,
            }
            .into());
        }
        let payload_len = usize::try_from(payload_len_u64).map_err(|_| {
            Int64WriteAheadLogLimitError::RecordBytes {
                sequence,
                bytes: payload_len_u64,
                max_bytes: limits.max_record_bytes,
            }
        })?;
        let frame_len = INT64_WAL_FRAME_OVERHEAD.checked_add(payload_len).ok_or(
            Int64WriteAheadLogLimitError::FileBytes {
                bytes: u64::MAX,
                max_bytes: limits.max_file_bytes,
            },
        )?;
        let frame_end =
            offset
                .checked_add(frame_len)
                .ok_or(Int64WriteAheadLogLimitError::FileBytes {
                    bytes: u64::MAX,
                    max_bytes: limits.max_file_bytes,
                })?;
        // A partial header, payload, or commit footer is an uncommitted crash
        // tail. Before ignoring it, derive the payload boundary emitted for
        // this kind and authenticate a complete record at that boundary. This
        // catches an overlong corrupted header in an intermediate record as
        // well as in the final record, without scanning untrusted bytes for
        // frame-like patterns or performing unbounded candidate work.
        if frame_end > bytes.len() {
            if let Some(committed_payload_len) =
                authenticated_payload_len(bytes, offset, header, limits.max_record_bytes)
            {
                return Err(Int64WriteAheadLogCorruption::PayloadLength {
                    sequence,
                    declared: payload_len_u64,
                    committed: committed_payload_len as u64,
                }
                .into());
            }
            break;
        }
        let records = expected_sequence.saturating_add(1);
        if records > limits.max_records as u64 {
            return Err(Int64WriteAheadLogLimitError::Records {
                records,
                max_records: limits.max_records,
            }
            .into());
        }
        let payload_start = offset + INT64_WAL_FRAME_HEADER_LEN;
        let payload_end = payload_start + payload_len;
        let payload = &bytes[payload_start..payload_end];
        let footer = &bytes[payload_end..frame_end];
        let commit_magic = read_array::<8>(footer, 0);
        if commit_magic != INT64_WAL_COMMIT_MAGIC {
            return Err(Int64WriteAheadLogCorruption::CommitMagic {
                sequence,
                found: commit_magic,
            }
            .into());
        }
        let commit_sequence = u64::from_le_bytes(read_array::<8>(footer, 8));
        if commit_sequence != sequence {
            return Err(Int64WriteAheadLogCorruption::CommitSequence {
                sequence,
                found: commit_sequence,
            }
            .into());
        }
        let actual_checksum = record_checksum(version, kind, reserved, sequence, payload);
        if actual_checksum != expected_checksum {
            return Err(Int64WriteAheadLogCorruption::Checksum {
                sequence,
                expected: expected_checksum,
                actual: actual_checksum,
            }
            .into());
        }

        match kind {
            BOOTSTRAP_KIND => {
                if bootstrap.is_some() || sequence != 0 {
                    return Err(
                        Int64WriteAheadLogCorruption::UnexpectedBootstrap { sequence }.into(),
                    );
                }
                bootstrap = Some(decode_bootstrap(payload, sequence)?);
            }
            APPEND_KIND => apply_append(bootstrap.as_mut(), payload, sequence)?,
            NULLABLE_APPEND_KIND => {
                apply_nullable_append(bootstrap.as_mut(), payload, sequence)?;
            }
            TRUNCATE_KIND => apply_truncate(bootstrap.as_mut(), payload, sequence)?,
            REPLACE_KIND => apply_replace(bootstrap.as_mut(), payload, sequence)?,
            NULLABLE_REPLACE_KIND => {
                apply_nullable_replace(bootstrap.as_mut(), payload, sequence)?;
            }
            kind => {
                return Err(
                    Int64WriteAheadLogCorruption::UnsupportedKind { sequence, kind }.into(),
                );
            }
        }

        offset = frame_end;
        expected_sequence += 1;
    }

    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    Ok(RecoveredInt64WriteAheadLog {
        bootstrap,
        file_bytes: bytes.len(),
        records: usize::try_from(expected_sequence).unwrap_or(usize::MAX),
    })
}

/// Returns the writer-valid payload length when its exact footer and checksum
/// authenticate a committed record, even when the header length is corrupt.
fn authenticated_payload_len(
    bytes: &[u8],
    offset: usize,
    header: &[u8],
    max_record_bytes: usize,
) -> Option<usize> {
    let version = u16::from_le_bytes(read_array::<2>(header, 8));
    let kind = header[10];
    let reserved = header[11];
    let sequence = u64::from_le_bytes(read_array::<8>(header, 12));
    let expected_checksum = u32::from_le_bytes(read_array::<4>(header, 28));
    let payload_start = offset.checked_add(INT64_WAL_FRAME_HEADER_LEN)?;
    let available = bytes.get(payload_start..)?;
    let payload_len = encoded_payload_len(kind, available)?;
    if payload_len > max_record_bytes {
        return None;
    }
    let footer_start = payload_start.checked_add(payload_len)?;
    let footer_end = footer_start.checked_add(INT64_WAL_COMMIT_LEN)?;
    let footer = bytes.get(footer_start..footer_end)?;
    if read_array::<8>(footer, 0) != INT64_WAL_COMMIT_MAGIC
        || u64::from_le_bytes(read_array::<8>(footer, 8)) != sequence
    {
        return None;
    }
    let payload = &bytes[payload_start..footer_start];
    (record_checksum(version, kind, reserved, sequence, payload) == expected_checksum)
        .then_some(payload.len())
}

/// Derives the only payload size emitted by the version-1 writer for `kind`.
/// Reads are fixed-offset and checked; the caller separately enforces the
/// configured record limit before hashing the resulting payload.
fn encoded_payload_len(kind: u8, payload_and_tail: &[u8]) -> Option<usize> {
    match kind {
        BOOTSTRAP_KIND => encoded_bootstrap_payload_len(payload_and_tail),
        APPEND_KIND => encoded_counted_payload_len(payload_and_tail, 8),
        NULLABLE_APPEND_KIND => encoded_counted_payload_len(payload_and_tail, 9),
        TRUNCATE_KIND => Some(0),
        REPLACE_KIND => encoded_counted_payload_len(payload_and_tail, 16),
        NULLABLE_REPLACE_KIND => encoded_counted_payload_len(payload_and_tail, 17),
        _ => None,
    }
}

fn encoded_counted_payload_len(payload_and_tail: &[u8], item_bytes: usize) -> Option<usize> {
    let count = read_usize_at(payload_and_tail, 0)?;
    8_usize.checked_add(count.checked_mul(item_bytes)?)
}

fn encoded_bootstrap_payload_len(payload_and_tail: &[u8]) -> Option<usize> {
    let table_name_bytes = read_usize_at(payload_and_tail, 0)?;
    let column_length_offset = 8_usize.checked_add(table_name_bytes)?;
    let column_name_bytes = read_usize_at(payload_and_tail, column_length_offset)?;
    let column_end = column_length_offset
        .checked_add(8)?
        .checked_add(column_name_bytes)?;
    // Nullability, six table-limit fields, ten query-limit fields, and the
    // worker cap precede the row-count field.
    let row_count_offset = column_end
        .checked_add(1)?
        .checked_add(17_usize.checked_mul(8)?)?;
    let nullable = *payload_and_tail.get(column_end)?;
    let row_count = read_usize_at(payload_and_tail, row_count_offset)?;
    let value_bytes = match nullable {
        0 => 8,
        1 => 9,
        _ => return None,
    };
    row_count_offset
        .checked_add(8)?
        .checked_add(row_count.checked_mul(value_bytes)?)
}

fn read_usize_at(input: &[u8], offset: usize) -> Option<usize> {
    let end = offset.checked_add(8)?;
    let bytes: [u8; 8] = input.get(offset..end)?.try_into().ok()?;
    usize::try_from(u64::from_le_bytes(bytes)).ok()
}

fn validate_record_limits(
    sequence: u64,
    payload_len: usize,
    current_file_bytes: usize,
    current_records: usize,
    limits: Int64WriteAheadLogLimits,
) -> Result<usize, Int64WriteAheadLogError> {
    if payload_len > limits.max_record_bytes {
        return Err(Int64WriteAheadLogLimitError::RecordBytes {
            sequence,
            bytes: payload_len as u64,
            max_bytes: limits.max_record_bytes,
        }
        .into());
    }
    let records = current_records.saturating_add(1);
    if records > limits.max_records {
        return Err(Int64WriteAheadLogLimitError::Records {
            records: records as u64,
            max_records: limits.max_records,
        }
        .into());
    }
    let attempted_file_bytes =
        (current_file_bytes as u128) + (INT64_WAL_FRAME_OVERHEAD as u128) + (payload_len as u128);
    let reported_file_bytes = u64::try_from(attempted_file_bytes).unwrap_or(u64::MAX);
    let frame_len = INT64_WAL_FRAME_OVERHEAD.checked_add(payload_len).ok_or(
        Int64WriteAheadLogLimitError::FileBytes {
            bytes: reported_file_bytes,
            max_bytes: limits.max_file_bytes,
        },
    )?;
    let file_bytes = current_file_bytes.checked_add(frame_len).ok_or(
        Int64WriteAheadLogLimitError::FileBytes {
            bytes: reported_file_bytes,
            max_bytes: limits.max_file_bytes,
        },
    )?;
    if file_bytes > limits.max_file_bytes {
        return Err(Int64WriteAheadLogLimitError::FileBytes {
            bytes: reported_file_bytes,
            max_bytes: limits.max_file_bytes,
        }
        .into());
    }
    Ok(file_bytes)
}

fn encode_record_parts(
    kind: u8,
    sequence: u64,
    payload: &[u8],
) -> (Vec<u8>, [u8; INT64_WAL_COMMIT_LEN]) {
    let checksum = record_checksum(INT64_WAL_VERSION, kind, 0, sequence, payload);
    let mut body = Vec::with_capacity(INT64_WAL_FRAME_HEADER_LEN + payload.len());
    body.extend_from_slice(&INT64_WAL_MAGIC);
    body.extend_from_slice(&INT64_WAL_VERSION.to_le_bytes());
    body.push(kind);
    body.push(0);
    body.extend_from_slice(&sequence.to_le_bytes());
    body.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    body.extend_from_slice(&checksum.to_le_bytes());
    body.extend_from_slice(payload);
    let mut footer = [0; INT64_WAL_COMMIT_LEN];
    footer[..INT64_WAL_COMMIT_MAGIC.len()].copy_from_slice(&INT64_WAL_COMMIT_MAGIC);
    footer[INT64_WAL_COMMIT_MAGIC.len()..].copy_from_slice(&sequence.to_le_bytes());
    (body, footer)
}

fn record_checksum(version: u16, kind: u8, reserved: u8, sequence: u64, payload: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    checksum = crc32_update(checksum, &version.to_le_bytes());
    checksum = crc32_update(checksum, &[kind, reserved]);
    checksum = crc32_update(checksum, &sequence.to_le_bytes());
    checksum = crc32_update(checksum, &(payload.len() as u64).to_le_bytes());
    checksum = crc32_update(checksum, payload);
    !checksum
}

fn crc32_update(mut checksum: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        checksum ^= u32::from(*byte);
        for _ in 0..8 {
            let low_bit_mask = 0_u32.wrapping_sub(checksum & 1);
            checksum = (checksum >> 1) ^ (0xedb8_8320 & low_bit_mask);
        }
    }
    checksum
}

fn encode_bootstrap(bootstrap: &Int64WalBootstrap) -> Vec<u8> {
    let capacity = bootstrap_payload_len(
        bootstrap.table_name.len(),
        bootstrap.column_name.len(),
        bootstrap.values.len(),
        bootstrap.nullable,
    )
    .expect("validated WAL bootstrap size must be representable");
    let mut payload = Vec::with_capacity(capacity);
    push_bytes(&mut payload, bootstrap.table_name.as_bytes());
    push_bytes(&mut payload, bootstrap.column_name.as_bytes());
    payload.push(u8::from(bootstrap.nullable));
    for value in bootstrap.table_limits {
        push_usize(&mut payload, value);
    }
    for value in bootstrap.database_table_limits {
        push_usize(&mut payload, value);
    }
    for value in bootstrap.query_limits {
        push_usize(&mut payload, value);
    }
    push_usize(&mut payload, bootstrap.worker_cap);
    push_usize(&mut payload, bootstrap.values.len());
    for value in &bootstrap.values {
        if bootstrap.nullable {
            push_nullable_i64(&mut payload, *value);
        } else {
            payload.extend_from_slice(
                &value
                    .expect("a non-nullable WAL bootstrap cannot contain NULL")
                    .to_le_bytes(),
            );
        }
    }
    payload
}

fn bootstrap_payload_len(
    table_name_bytes: usize,
    column_name_bytes: usize,
    rows: usize,
    nullable: bool,
) -> Option<usize> {
    // Two string lengths, nullability, six table-limit fields, ten query-limit
    // fields, worker cap, row count, then one i64 per row.
    let fixed = 2_usize
        .checked_mul(8)?
        .checked_add(1)?
        .checked_add(6_usize.checked_mul(8)?)?
        .checked_add(10_usize.checked_mul(8)?)?
        .checked_add(2_usize.checked_mul(8)?)?;
    fixed
        .checked_add(table_name_bytes)?
        .checked_add(column_name_bytes)?
        .checked_add(rows.checked_mul(if nullable { 9 } else { 8 })?)
}

fn decode_bootstrap(
    payload: &[u8],
    sequence: u64,
) -> Result<Int64WalBootstrap, Int64WriteAheadLogError> {
    let mut reader = PayloadReader::new(payload, sequence);
    let table_name = reader.string("table name")?;
    let column_name = reader.string("column name")?;
    let nullability = reader.byte("nullability")?;
    if nullability > 1 {
        return Err(reader.malformed("nullability").into());
    }
    let nullable = nullability == 1;
    let table_limits = [
        reader.usize("table row cap")?,
        reader.usize("table column cap")?,
        reader.usize("table cell cap")?,
    ];
    let database_table_limits = [
        reader.usize("database table row cap")?,
        reader.usize("database table column cap")?,
        reader.usize("database table cell cap")?,
    ];
    let mut query_limits = [0; QUERY_LIMIT_FIELD_COUNT];
    for query_limit in &mut query_limits {
        *query_limit = reader.usize("query limit")?;
    }
    let worker_cap = reader.usize("worker cap")?;
    if worker_cap == 0 {
        return Err(reader.malformed("worker cap").into());
    }
    let row_count = reader.usize("row count")?;
    validate_bootstrap_caps(&reader, table_limits, row_count)?;
    let required_value_bytes = row_count
        .checked_mul(if nullable { 9 } else { 8 })
        .ok_or_else(|| reader.malformed("row count"))?;
    if reader.remaining() != required_value_bytes {
        return Err(reader.malformed("row values").into());
    }
    let mut values = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        values.push(if nullable {
            reader.nullable_i64("row value")?
        } else {
            Some(reader.i64("row value")?)
        });
    }
    reader.finish()?;
    Ok(Int64WalBootstrap {
        table_name,
        column_name,
        table_limits,
        database_table_limits,
        query_limits,
        worker_cap,
        nullable,
        values,
    })
}

fn validate_bootstrap_caps(
    reader: &PayloadReader<'_>,
    table_limits: [usize; 3],
    row_count: usize,
) -> Result<(), Int64WriteAheadLogError> {
    // Database table limits are defaults for future SQL-created tables. An
    // explicitly created table can validly carry larger local limits (or live
    // in a database whose defaults admit no ordinary table), so replay only
    // requires the persisted table-local limits to cover its current shape.
    if table_limits[1] < 1 || table_limits[2] < row_count || table_limits[0] < row_count {
        return Err(reader.malformed("table resource limits").into());
    }
    Ok(())
}

fn apply_append(
    bootstrap: Option<&mut Int64WalBootstrap>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), Int64WriteAheadLogError> {
    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    let mut reader = PayloadReader::new(payload, sequence);
    let count = reader.usize("append row count")?;
    if bootstrap.nullable {
        return Err(reader.malformed("non-nullable append kind").into());
    }
    let value_bytes = count
        .checked_mul(8)
        .ok_or_else(|| reader.malformed("append row count"))?;
    if reader.remaining() != value_bytes {
        return Err(reader.malformed("append values").into());
    }
    let new_rows = bootstrap
        .values
        .len()
        .checked_add(count)
        .ok_or_else(|| reader.malformed("append row count"))?;
    if new_rows > bootstrap.table_limits[0] || new_rows > bootstrap.table_limits[2] {
        return Err(reader.malformed("append table resource limits").into());
    }
    bootstrap.values.reserve(count);
    for _ in 0..count {
        bootstrap.values.push(Some(reader.i64("append value")?));
    }
    reader.finish()
}

fn apply_truncate(
    bootstrap: Option<&mut Int64WalBootstrap>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), Int64WriteAheadLogError> {
    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    if !payload.is_empty() {
        return Err(Int64WriteAheadLogCorruption::MalformedPayload {
            sequence,
            field: "truncate payload",
        }
        .into());
    }
    bootstrap.values.clear();
    Ok(())
}

fn apply_replace(
    bootstrap: Option<&mut Int64WalBootstrap>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), Int64WriteAheadLogError> {
    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    let mut reader = PayloadReader::new(payload, sequence);
    if bootstrap.nullable {
        return Err(reader.malformed("non-nullable replacement kind").into());
    }
    let count = reader.usize("replacement count")?;
    let replacement_bytes = count
        .checked_mul(16)
        .ok_or_else(|| reader.malformed("replacement count"))?;
    if reader.remaining() != replacement_bytes {
        return Err(reader.malformed("replacement values").into());
    }
    let mut replacements = Vec::with_capacity(count);
    let mut previous = None;
    for _ in 0..count {
        let row = reader.usize("replacement row")?;
        let value = Some(reader.i64("replacement value")?);
        if row >= bootstrap.values.len() || previous.is_some_and(|previous| row <= previous) {
            return Err(reader.malformed("replacement row selection").into());
        }
        previous = Some(row);
        replacements.push((row, value));
    }
    reader.finish()?;
    for (row, value) in replacements {
        bootstrap.values[row] = value;
    }
    Ok(())
}

fn apply_nullable_append(
    bootstrap: Option<&mut Int64WalBootstrap>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), Int64WriteAheadLogError> {
    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    let mut reader = PayloadReader::new(payload, sequence);
    if !bootstrap.nullable {
        return Err(reader.malformed("nullable append kind").into());
    }
    let count = reader.usize("append row count")?;
    let value_bytes = count
        .checked_mul(9)
        .ok_or_else(|| reader.malformed("append row count"))?;
    if reader.remaining() != value_bytes {
        return Err(reader.malformed("append values").into());
    }
    let new_rows = bootstrap
        .values
        .len()
        .checked_add(count)
        .ok_or_else(|| reader.malformed("append row count"))?;
    if new_rows > bootstrap.table_limits[0] || new_rows > bootstrap.table_limits[2] {
        return Err(reader.malformed("append table resource limits").into());
    }
    bootstrap.values.reserve(count);
    for _ in 0..count {
        bootstrap.values.push(reader.nullable_i64("append value")?);
    }
    reader.finish()
}

fn apply_nullable_replace(
    bootstrap: Option<&mut Int64WalBootstrap>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), Int64WriteAheadLogError> {
    let bootstrap = bootstrap.ok_or(Int64WriteAheadLogCorruption::MissingBootstrap)?;
    let mut reader = PayloadReader::new(payload, sequence);
    if !bootstrap.nullable {
        return Err(reader.malformed("nullable replacement kind").into());
    }
    let count = reader.usize("replacement count")?;
    let replacement_bytes = count
        .checked_mul(17)
        .ok_or_else(|| reader.malformed("replacement count"))?;
    if reader.remaining() != replacement_bytes {
        return Err(reader.malformed("replacement values").into());
    }
    let mut replacements = Vec::with_capacity(count);
    let mut previous = None;
    for _ in 0..count {
        let row = reader.usize("replacement row")?;
        let value = reader.nullable_i64("replacement value")?;
        if row >= bootstrap.values.len() || previous.is_some_and(|previous| row <= previous) {
            return Err(reader.malformed("replacement row selection").into());
        }
        previous = Some(row);
        replacements.push((row, value));
    }
    reader.finish()?;
    for (row, value) in replacements {
        bootstrap.values[row] = value;
    }
    Ok(())
}

fn push_nullable_i64(output: &mut Vec<u8>, value: Option<i64>) {
    output.push(u8::from(value.is_some()));
    output.extend_from_slice(&value.unwrap_or_default().to_le_bytes());
}

fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    push_usize(output, bytes.len());
    output.extend_from_slice(bytes);
}

fn push_usize(output: &mut Vec<u8>, value: usize) {
    output.extend_from_slice(&(value as u64).to_le_bytes());
}

fn read_array<const N: usize>(input: &[u8], offset: usize) -> [u8; N] {
    input[offset..offset + N]
        .try_into()
        .expect("the caller checked the fixed field boundary")
}

struct PayloadReader<'a> {
    payload: &'a [u8],
    offset: usize,
    sequence: u64,
}

impl<'a> PayloadReader<'a> {
    const fn new(payload: &'a [u8], sequence: u64) -> Self {
        Self {
            payload,
            offset: 0,
            sequence,
        }
    }

    const fn malformed(&self, field: &'static str) -> Int64WriteAheadLogCorruption {
        Int64WriteAheadLogCorruption::MalformedPayload {
            sequence: self.sequence,
            field,
        }
    }

    fn byte(&mut self, field: &'static str) -> Result<u8, Int64WriteAheadLogCorruption> {
        let byte = *self
            .payload
            .get(self.offset)
            .ok_or_else(|| self.malformed(field))?;
        self.offset += 1;
        Ok(byte)
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, Int64WriteAheadLogCorruption> {
        let end = self
            .offset
            .checked_add(8)
            .ok_or_else(|| self.malformed(field))?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| self.malformed(field))?;
        self.offset = end;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("the slice length is eight"),
        ))
    }

    fn usize(&mut self, field: &'static str) -> Result<usize, Int64WriteAheadLogCorruption> {
        usize::try_from(self.u64(field)?).map_err(|_| self.malformed(field))
    }

    fn i64(&mut self, field: &'static str) -> Result<i64, Int64WriteAheadLogCorruption> {
        let end = self
            .offset
            .checked_add(8)
            .ok_or_else(|| self.malformed(field))?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| self.malformed(field))?;
        self.offset = end;
        Ok(i64::from_le_bytes(
            bytes.try_into().expect("the slice length is eight"),
        ))
    }

    fn nullable_i64(
        &mut self,
        field: &'static str,
    ) -> Result<Option<i64>, Int64WriteAheadLogCorruption> {
        let present = self.byte(field)?;
        let value = self.i64(field)?;
        match present {
            0 if value == 0 => Ok(None),
            1 => Ok(Some(value)),
            _ => Err(self.malformed(field)),
        }
    }

    fn string(&mut self, field: &'static str) -> Result<String, Int64WriteAheadLogCorruption> {
        let length = self.usize(field)?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.malformed(field))?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| self.malformed(field))?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| self.malformed(field))?
            .to_owned();
        self.offset = end;
        Ok(value)
    }

    fn remaining(&self) -> usize {
        self.payload.len().saturating_sub(self.offset)
    }

    fn finish(self) -> Result<(), Int64WriteAheadLogError> {
        if self.offset == self.payload.len() {
            Ok(())
        } else {
            Err(self.malformed("trailing payload bytes").into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum IoStep {
        Write(Vec<u8>),
        Sync,
    }

    struct FaultFile {
        steps: Vec<IoStep>,
        fail_at: Option<usize>,
    }

    impl FaultFile {
        fn new(fail_at: Option<usize>) -> Self {
            Self {
                steps: Vec::new(),
                fail_at,
            }
        }

        fn complete_step(&self) -> io::Result<()> {
            if self.fail_at == Some(self.steps.len() - 1) {
                Err(io::Error::other("injected WAL I/O failure"))
            } else {
                Ok(())
            }
        }
    }

    impl DurableWalFile for FaultFile {
        fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.steps.push(IoStep::Write(bytes.to_vec()));
            self.complete_step()
        }

        fn sync_bytes(&mut self) -> io::Result<()> {
            self.steps.push(IoStep::Sync);
            self.complete_step()
        }
    }

    #[test]
    fn file_size_overflow_is_rejected_even_when_the_limit_is_usize_max() {
        let limits = Int64WriteAheadLogLimits::new(usize::MAX, usize::MAX, usize::MAX);
        let exact_boundary = usize::MAX - INT64_WAL_FRAME_OVERHEAD;
        assert_eq!(
            validate_record_limits(0, 0, exact_boundary, 0, limits).unwrap(),
            usize::MAX
        );

        let error = validate_record_limits(0, 0, exact_boundary + 1, 0, limits).unwrap_err();
        assert!(matches!(
            error,
            Int64WriteAheadLogError::Limit(Int64WriteAheadLogLimitError::FileBytes {
                bytes,
                max_bytes: usize::MAX,
            }) if bytes == (usize::MAX as u64).saturating_add(1)
        ));
    }

    #[test]
    fn record_body_is_synced_before_the_commit_footer_is_written_and_synced() {
        let body = b"header and payload";
        let footer = *b"commit footer!!!";
        assert_eq!(footer.len(), INT64_WAL_COMMIT_LEN);
        let mut file = FaultFile::new(None);

        write_committed_record(&mut file, body, &footer).unwrap();

        assert_eq!(
            file.steps,
            [
                IoStep::Write(body.to_vec()),
                IoStep::Sync,
                IoStep::Write(footer.to_vec()),
                IoStep::Sync,
            ]
        );
    }

    #[test]
    fn every_body_or_footer_failure_stops_before_the_next_durability_phase() {
        let body = b"body";
        let footer = [7; INT64_WAL_COMMIT_LEN];
        for failed_step in 0..4 {
            let mut file = FaultFile::new(Some(failed_step));

            let error = write_committed_record(&mut file, body, &footer).unwrap_err();

            assert_eq!(file.steps.len(), failed_step + 1);
            assert!(matches!(
                (failed_step, error),
                (0 | 2, Int64WriteAheadLogError::Write(_))
                    | (1 | 3, Int64WriteAheadLogError::SyncFile(_))
            ));
        }
    }

    #[test]
    fn indeterminate_member_failure_poisons_registry_before_cross_member_write() {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/wal-registry-poison-tests");
        fs::create_dir_all(&base).unwrap();
        let root = loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        };
        let writer = |name: &str| Int64WriteAheadLog {
            file: File::create(root.join(format!("{name}.wal"))).unwrap(),
            normalized_table_name: name.to_owned(),
            nullable: false,
            limits: Int64WriteAheadLogLimits::default(),
            file_bytes: 0,
            records: 0,
            poisoned: false,
        };
        let mut registry = Int64WriteAheadLogRegistry {
            writers: HashMap::from([
                ("alpha".to_owned(), writer("alpha")),
                ("beta".to_owned(), writer("beta")),
            ]),
            limits: Int64WriteAheadLogRegistryLimits::default(),
            total_wal_bytes: 0,
            total_records: 0,
            poisoned: false,
        };
        let beta_path = root.join("beta.wal");
        let mut fault = FaultFile::new(Some(3));
        let injected = write_committed_record(&mut fault, b"body", &[0; INT64_WAL_COMMIT_LEN])
            .expect_err("the final sync is injected to fail");

        assert!(matches!(
            registry.commit_member_record("alpha", INT64_WAL_FRAME_OVERHEAD, |_| Err(injected)),
            Err(Int64WriteAheadLogCommitError::SyncFile { .. })
        ));
        assert!(registry.poisoned);
        assert_eq!(registry.total_wal_bytes, 0);
        assert_eq!(registry.total_records, 0);
        assert!(matches!(
            registry.append_values("beta", &[Some(1)]),
            Err(Int64WriteAheadLogCommitError::Poisoned)
        ));
        assert_eq!(fs::metadata(beta_path).unwrap().len(), 0);

        drop(registry);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creation_stays_with_the_open_parent_when_its_path_is_rebound() {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/wal-rebind-tests");
        fs::create_dir_all(&base).unwrap();
        let root = loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
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
        let directory = WalDirectory::open(&parent).unwrap();
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();

        let bootstrap = Int64WalBootstrap {
            table_name: "Events".to_owned(),
            column_name: "Id".to_owned(),
            table_limits: [1, 1, 1],
            database_table_limits: [1, 1, 1],
            query_limits: [1; QUERY_LIMIT_FIELD_COUNT],
            worker_cap: 1,
            nullable: false,
            values: vec![],
        };
        let payload = encode_bootstrap(&bootstrap);
        let (body, footer) = encode_record_parts(BOOTSTRAP_KIND, 0, &payload);
        let destination = CString::new("events.wal").unwrap();
        let file = create_committed_wal_file(&directory, &destination, &body, &footer).unwrap();
        drop(file);

        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        let wal_path = moved_parent.join("events.wal");
        assert!(wal_path.is_file());
        let recovered = recover(&wal_path, Int64WriteAheadLogLimits::default()).unwrap();
        assert_eq!(recovered.bootstrap.table_name, "Events");
        assert_eq!(recovered.bootstrap.column_name, "Id");

        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }
}
