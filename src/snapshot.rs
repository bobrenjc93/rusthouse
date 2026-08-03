//! Versioned, bounded envelopes for snapshot payloads.
//!
//! This module defines only the byte envelope. It does not choose a catalog
//! serialization or perform filesystem I/O. See `docs/snapshot-format.md` for
//! the stable binary layout.

use std::error::Error;
use std::fmt;

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
