//! Versioned, bounded snapshot envelopes and nullable `Int64` row payloads.
//!
//! This module does not serialize a catalog. It can create and sync a new
//! envelope file without replacing an existing destination, then reopen one
//! bounded `Int64` table from that file. See `docs/snapshot-format.md` for the
//! stable binary layouts.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

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
            Self::FileTooLarge { .. } => None,
            Self::Restore(error) => Some(error),
        }
    }
}

impl From<Int64TableRestoreError> for Int64TableFileRestoreError {
    fn from(error: Int64TableRestoreError) -> Self {
        Self::Restore(error)
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
/// limit is read. A larger regular file is rejected before its contents are
/// read. The bounded bytes are passed to [`restore_int64_table`], so all
/// envelope, payload, schema, and row-cap validation finishes before a table
/// is returned.
pub fn restore_int64_table_from_file(
    path: impl AsRef<Path>,
    schema: Schema,
    row_cap: usize,
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Result<Int64Table, Int64TableFileRestoreError> {
    let mut file = File::open(path).map_err(Int64TableFileRestoreError::Open)?;
    let max_file_len = SNAPSHOT_HEADER_LEN.saturating_add(snapshot_codec.max_payload_len());
    let file_len = bounded_file_len(&file, max_file_len)?;
    let capacity = usize::try_from(file_len).unwrap_or(max_file_len);
    let mut envelope = Vec::with_capacity(capacity);
    Read::take(&mut file, u64::try_from(max_file_len).unwrap_or(u64::MAX))
        .read_to_end(&mut envelope)
        .map_err(Int64TableFileRestoreError::Read)?;

    // Check the opened file again in case it grew after the first size check.
    bounded_file_len(&file, max_file_len)?;

    restore_int64_table(&envelope, schema, row_cap, snapshot_codec, payload_codec)
        .map_err(Int64TableFileRestoreError::Restore)
}

fn bounded_file_len(file: &File, max_file_len: usize) -> Result<u64, Int64TableFileRestoreError> {
    let file_len = file
        .metadata()
        .map_err(Int64TableFileRestoreError::Read)?
        .len();
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
}
