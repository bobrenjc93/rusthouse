//! Crash-recoverable, bounded write-ahead logging for one batch `Int64` table.
//!
//! The stable framing and lifecycle are documented in `docs/int64-wal-format.md`.

use std::error::Error as StdError;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

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
            Self::NotRegularFile | Self::Poisoned => None,
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
            Self::Poisoned
            | Self::Write { .. }
            | Self::SyncFile { .. }
            | Self::Unexpected { .. } => None,
        }
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
    pub(crate) values: Vec<i64>,
}

#[derive(Debug)]
pub(crate) struct RecoveredInt64WriteAheadLog {
    pub(crate) bootstrap: Int64WalBootstrap,
}

/// Open single-table WAL writer. The database owns this after opt-in.
#[derive(Debug)]
pub(crate) struct Int64WriteAheadLog {
    file: File,
    normalized_table_name: String,
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
        limits: Int64WriteAheadLogLimits,
    ) -> Result<(), Int64WriteAheadLogError> {
        let payload_len =
            bootstrap_payload_len(table_name_bytes, column_name_bytes, rows).unwrap_or(usize::MAX);
        validate_record_limits(0, payload_len, 0, 0, limits)
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
            limits,
        )?;
        let payload = encode_bootstrap(bootstrap);
        debug_assert_eq!(
            Some(payload.len()),
            bootstrap_payload_len(
                bootstrap.table_name.len(),
                bootstrap.column_name.len(),
                bootstrap.values.len()
            )
        );
        let frame = encode_frame(BOOTSTRAP_KIND, 0, &payload);

        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent_directory = File::open(parent).map_err(Int64WriteAheadLogError::OpenParent)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(Int64WriteAheadLogError::Create)?;
        file.write_all(&frame)
            .map_err(Int64WriteAheadLogError::Write)?;
        file.sync_all().map_err(Int64WriteAheadLogError::SyncFile)?;
        parent_directory
            .sync_all()
            .map_err(Int64WriteAheadLogError::SyncParent)?;

        Ok(Self {
            file,
            normalized_table_name: bootstrap.table_name.to_ascii_lowercase(),
            limits,
            file_bytes: frame.len(),
            records: 1,
            poisoned: false,
        })
    }

    pub(crate) fn tracks(&self, table_name: &str) -> bool {
        self.normalized_table_name.eq_ignore_ascii_case(table_name)
    }

    pub(crate) fn append_values(&mut self, values: &[i64]) -> Result<(), Int64WriteAheadLogError> {
        let payload_len = 8_usize
            .checked_add(values.len().checked_mul(8).unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX);
        self.validate_next_record(payload_len)?;
        let mut payload = Vec::with_capacity(payload_len);
        push_usize(&mut payload, values.len());
        for value in values {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        self.append_record(APPEND_KIND, &payload)
    }

    pub(crate) fn truncate(&mut self) -> Result<(), Int64WriteAheadLogError> {
        self.validate_next_record(0)?;
        self.append_record(TRUNCATE_KIND, &[])
    }

    pub(crate) fn replace_values(
        &mut self,
        replacements: &[(usize, i64)],
    ) -> Result<(), Int64WriteAheadLogError> {
        let payload_len = 8_usize
            .checked_add(replacements.len().checked_mul(16).unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX);
        self.validate_next_record(payload_len)?;
        let mut payload = Vec::with_capacity(payload_len);
        push_usize(&mut payload, replacements.len());
        for (row, value) in replacements {
            push_usize(&mut payload, *row);
            payload.extend_from_slice(&value.to_le_bytes());
        }
        self.append_record(REPLACE_KIND, &payload)
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
    }

    fn append_record(&mut self, kind: u8, payload: &[u8]) -> Result<(), Int64WriteAheadLogError> {
        if self.poisoned {
            return Err(Int64WriteAheadLogError::Poisoned);
        }
        let sequence = self.records as u64;
        validate_record_limits(
            sequence,
            payload.len(),
            self.file_bytes,
            self.records,
            self.limits,
        )?;
        let frame = encode_frame(kind, sequence, payload);
        if let Err(error) = self.file.write_all(&frame) {
            self.poisoned = true;
            return Err(Int64WriteAheadLogError::Write(error));
        }
        if let Err(error) = self.file.sync_all() {
            self.poisoned = true;
            return Err(Int64WriteAheadLogError::SyncFile(error));
        }
        self.file_bytes += frame.len();
        self.records += 1;
        Ok(())
    }
}

pub(crate) fn recover(
    path: &Path,
    limits: Int64WriteAheadLogLimits,
) -> Result<RecoveredInt64WriteAheadLog, Int64WriteAheadLogError> {
    let file = File::open(path).map_err(Int64WriteAheadLogError::Open)?;
    let metadata = file.metadata().map_err(Int64WriteAheadLogError::Metadata)?;
    if !metadata.is_file() {
        return Err(Int64WriteAheadLogError::NotRegularFile);
    }
    if metadata.len() > limits.max_file_bytes as u64 {
        return Err(Int64WriteAheadLogLimitError::FileBytes {
            bytes: metadata.len(),
            max_bytes: limits.max_file_bytes,
        }
        .into());
    }
    let read_limit = (limits.max_file_bytes as u64).saturating_add(1);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
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
        // A partial header, payload, or commit footer is an uncommitted crash tail.
        if frame_end > bytes.len() {
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
            TRUNCATE_KIND => apply_truncate(bootstrap.as_mut(), payload, sequence)?,
            REPLACE_KIND => apply_replace(bootstrap.as_mut(), payload, sequence)?,
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
    Ok(RecoveredInt64WriteAheadLog { bootstrap })
}

fn validate_record_limits(
    sequence: u64,
    payload_len: usize,
    current_file_bytes: usize,
    current_records: usize,
    limits: Int64WriteAheadLogLimits,
) -> Result<(), Int64WriteAheadLogError> {
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
    let frame_len = INT64_WAL_FRAME_OVERHEAD
        .checked_add(payload_len)
        .unwrap_or(usize::MAX);
    let file_bytes = current_file_bytes
        .checked_add(frame_len)
        .unwrap_or(usize::MAX);
    if file_bytes > limits.max_file_bytes {
        return Err(Int64WriteAheadLogLimitError::FileBytes {
            bytes: file_bytes as u64,
            max_bytes: limits.max_file_bytes,
        }
        .into());
    }
    Ok(())
}

fn encode_frame(kind: u8, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let checksum = record_checksum(INT64_WAL_VERSION, kind, 0, sequence, payload);
    let mut frame = Vec::with_capacity(INT64_WAL_FRAME_OVERHEAD + payload.len());
    frame.extend_from_slice(&INT64_WAL_MAGIC);
    frame.extend_from_slice(&INT64_WAL_VERSION.to_le_bytes());
    frame.push(kind);
    frame.push(0);
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&INT64_WAL_COMMIT_MAGIC);
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame
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
    )
    .expect("validated WAL bootstrap size must be representable");
    let mut payload = Vec::with_capacity(capacity);
    push_bytes(&mut payload, bootstrap.table_name.as_bytes());
    push_bytes(&mut payload, bootstrap.column_name.as_bytes());
    payload.push(0); // Batch physical Int64 storage is NOT NULL.
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
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload
}

fn bootstrap_payload_len(
    table_name_bytes: usize,
    column_name_bytes: usize,
    rows: usize,
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
        .checked_add(rows.checked_mul(8)?)
}

fn decode_bootstrap(
    payload: &[u8],
    sequence: u64,
) -> Result<Int64WalBootstrap, Int64WriteAheadLogError> {
    let mut reader = PayloadReader::new(payload, sequence);
    let table_name = reader.string("table name")?;
    let column_name = reader.string("column name")?;
    let nullability = reader.byte("nullability")?;
    if nullability != 0 {
        return Err(reader.malformed("nullability").into());
    }
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
    validate_bootstrap_caps(&reader, table_limits, database_table_limits, row_count)?;
    let required_value_bytes = row_count
        .checked_mul(8)
        .ok_or_else(|| reader.malformed("row count"))?;
    if reader.remaining() != required_value_bytes {
        return Err(reader.malformed("row values").into());
    }
    let mut values = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        values.push(reader.i64("row value")?);
    }
    reader.finish()?;
    Ok(Int64WalBootstrap {
        table_name,
        column_name,
        table_limits,
        database_table_limits,
        query_limits,
        worker_cap,
        values,
    })
}

fn validate_bootstrap_caps(
    reader: &PayloadReader<'_>,
    table_limits: [usize; 3],
    database_table_limits: [usize; 3],
    row_count: usize,
) -> Result<(), Int64WriteAheadLogError> {
    if table_limits[1] < 1
        || table_limits[2] < row_count
        || table_limits[0] < row_count
        || database_table_limits[0] < table_limits[0]
        || database_table_limits[1] < 1
        || database_table_limits[2] < row_count
    {
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
        bootstrap.values.push(reader.i64("append value")?);
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
        let value = reader.i64("replacement value")?;
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
