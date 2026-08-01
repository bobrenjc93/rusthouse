//! Versioned, immutable column segments.
//!
//! A segment is split into row groups, with one independently checksummed block
//! per column and row group. The checksummed directory contains per-block zone
//! maps, allowing scans to reject row groups without reading their payloads.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
#[cfg(any(test, windows))]
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// The segment format version emitted by this module.
pub const FORMAT_VERSION: u16 = 1;

const MAGIC: &[u8; 8] = b"RHSEG\0\0\0";
const HEADER_SIZE: usize = 48;
const HEADER_CHECKSUM_OFFSET: usize = 40;
const HAS_MIN_MAX: u8 = 1;
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

/// A physical column type supported by immutable segments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Int64,
    Bool,
    String,
}

impl DataType {
    fn tag(self) -> u8 {
        match self {
            Self::Int64 => 1,
            Self::Bool => 2,
            Self::String => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, SegmentError> {
        match tag {
            1 => Ok(Self::Int64),
            2 => Ok(Self::Bool),
            3 => Ok(Self::String),
            _ => Err(SegmentError::Corrupt(format!(
                "unknown column type tag {tag}"
            ))),
        }
    }
}

/// A named field in a segment schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

/// The ordered schema stored in a segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Result<Self, SegmentError> {
        validate_fields(&fields)?;
        Ok(Self { fields })
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// A typed, nullable column. All columns in a segment have the same row count.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int64(Vec<Option<i64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    fn empty(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn has_nulls(&self) -> bool {
        match self {
            Self::Int64(values) => values.iter().any(Option::is_none),
            Self::Bool(values) => values.iter().any(Option::is_none),
            Self::String(values) => values.iter().any(Option::is_none),
        }
    }

    fn append_selected(&mut self, source: &Self, selected: &[bool]) -> Result<(), SegmentError> {
        let selected_count = selected.iter().filter(|keep| **keep).count();
        reserve_column(self, selected_count)?;
        match (self, source) {
            (Self::Int64(output), Self::Int64(input)) => {
                output.extend(
                    input
                        .iter()
                        .zip(selected)
                        .filter_map(|(value, keep)| keep.then_some(*value)),
                );
            }
            (Self::Bool(output), Self::Bool(input)) => {
                output.extend(
                    input
                        .iter()
                        .zip(selected)
                        .filter_map(|(value, keep)| keep.then_some(*value)),
                );
            }
            (Self::String(output), Self::String(input)) => {
                for (value, keep) in input.iter().zip(selected) {
                    if *keep {
                        output.push(value.as_deref().map(try_clone_string).transpose()?);
                    }
                }
            }
            _ => unreachable!("validated schema and block types must agree"),
        }
        Ok(())
    }

    fn append_all(&mut self, source: Self) {
        match (self, source) {
            (Self::Int64(output), Self::Int64(mut input)) => output.append(&mut input),
            (Self::Bool(output), Self::Bool(mut input)) => output.append(&mut input),
            (Self::String(output), Self::String(mut input)) => output.append(&mut input),
            _ => unreachable!("validated schema and block types must agree"),
        }
    }
}

/// A scalar used by zone maps and scan predicates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScalarValue {
    Int64(i64),
    Bool(bool),
    String(String),
}

impl ScalarValue {
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => Some(left.cmp(right)),
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(right)),
            (Self::String(left), Self::String(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }
}

/// A comparison operation for a scan predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonOp {
    Eq,
    NotEq,
    Lt,
    LessOrEq,
    Gt,
    GreaterOrEq,
}

/// A single-column predicate that can be evaluated against zone maps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Predicate {
    Compare {
        column: usize,
        op: ComparisonOp,
        value: ScalarValue,
    },
    IsNull {
        column: usize,
    },
    IsNotNull {
        column: usize,
    },
}

impl Predicate {
    fn column(&self) -> usize {
        match self {
            Self::Compare { column, .. } | Self::IsNull { column } | Self::IsNotNull { column } => {
                *column
            }
        }
    }
}

/// Resource ceilings applied before allocating from segment metadata.
#[derive(Clone, Debug)]
pub struct DecodeLimits {
    pub max_file_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_columns: u32,
    pub max_row_groups: u32,
    pub max_blocks: u32,
    pub max_rows: u64,
    pub max_rows_per_block: u32,
    pub max_block_bytes: u64,
    pub max_decoded_block_bytes: u64,
    pub max_decoded_result_bytes: u64,
    pub max_string_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_columns: 4_096,
            max_row_groups: 1_000_000,
            max_blocks: 1_000_000,
            max_rows: 1_000_000_000,
            max_rows_per_block: 1_000_000,
            max_block_bytes: 128 * 1024 * 1024,
            max_decoded_block_bytes: 256 * 1024 * 1024,
            max_decoded_result_bytes: 512 * 1024 * 1024,
            max_string_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Options controlling physical segment construction.
#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub rows_per_block: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            rows_per_block: 65_536,
        }
    }
}

/// Per-column, per-row-group statistics persisted in the directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockStatistics {
    pub row_start: u64,
    pub row_count: u32,
    pub null_count: u32,
    pub min: Option<ScalarValue>,
    pub max: Option<ScalarValue>,
}

/// Work avoided and performed by a segment scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanMetrics {
    pub row_groups_considered: u32,
    pub row_groups_pruned: u32,
    pub column_blocks_decoded: u64,
}

/// Projected columns and metrics returned by [`Segment::scan`].
#[derive(Clone, Debug, PartialEq)]
pub struct ScanResult {
    pub columns: Vec<Column>,
    pub row_count: u64,
    pub metrics: ScanMetrics,
}

/// Result of publishing an immutable segment file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentWriteOutcome {
    /// The final path was published and its directory update was synced.
    Durable,
    /// The final path is visible, but cleanup or durability confirmation failed.
    PublishedUncertain { message: String },
}

/// Errors produced while validating, reading, or writing a segment.
#[derive(Debug)]
pub enum SegmentError {
    Io(io::Error),
    UnsupportedPlatform(&'static str),
    InvalidInput(String),
    Corrupt(String),
    UnsupportedVersion(u16),
    LimitExceeded {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    ChecksumMismatch {
        location: String,
        expected: u32,
        actual: u32,
    },
}

impl fmt::Display for SegmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "segment I/O error: {error}"),
            Self::UnsupportedPlatform(message) => {
                write!(formatter, "segment persistence is unsupported: {message}")
            }
            Self::InvalidInput(message) => write!(formatter, "invalid segment input: {message}"),
            Self::Corrupt(message) => write!(formatter, "corrupt segment: {message}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported segment format version {version}")
            }
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "segment {resource} exceeds decode limit: {actual} > {limit}"
            ),
            Self::ChecksumMismatch {
                location,
                expected,
                actual,
            } => write!(
                formatter,
                "segment checksum mismatch in {location}: expected {expected:#010x}, got {actual:#010x}"
            ),
        }
    }
}

impl Error for SegmentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SegmentError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Encoding {
    IntDeltaBitPacked,
    IntPlain,
    BoolBitPacked,
    StringFrontCoded,
}

impl Encoding {
    fn tag(self) -> u8 {
        match self {
            Self::IntDeltaBitPacked => 1,
            Self::IntPlain => 2,
            Self::BoolBitPacked => 3,
            Self::StringFrontCoded => 4,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, SegmentError> {
        match tag {
            1 => Ok(Self::IntDeltaBitPacked),
            2 => Ok(Self::IntPlain),
            3 => Ok(Self::BoolBitPacked),
            4 => Ok(Self::StringFrontCoded),
            _ => Err(SegmentError::Corrupt(format!(
                "unknown block encoding tag {tag}"
            ))),
        }
    }

    fn accepts(self, data_type: DataType) -> bool {
        matches!(
            (self, data_type),
            (Self::IntDeltaBitPacked | Self::IntPlain, DataType::Int64)
                | (Self::BoolBitPacked, DataType::Bool)
                | (Self::StringFrontCoded, DataType::String)
        )
    }
}

#[derive(Clone, Debug)]
struct BlockMeta {
    column: u32,
    row_group: u32,
    row_start: u64,
    row_count: u32,
    encoding: Encoding,
    offset: u64,
    stored_len: u32,
    logical_len: u64,
    checksum: u32,
    stats: BlockStatistics,
}

struct EncodedBlock {
    meta: BlockMeta,
    payload: Vec<u8>,
}

/// An opened, validated segment.
///
/// Opening verifies every block checksum and recomputes every zone map before
/// the segment can be scanned. Reads still verify a selected block immediately
/// before decoding it, which also detects changes to an in-memory byte buffer.
pub struct Segment {
    bytes: Vec<u8>,
    schema: Schema,
    row_count: u64,
    rows_per_block: u32,
    row_group_count: u32,
    blocks: Vec<BlockMeta>,
    limits: DecodeLimits,
}

impl Segment {
    /// Opens a segment without allocating beyond `limits.max_file_bytes`.
    pub fn open(path: impl AsRef<Path>, limits: DecodeLimits) -> Result<Self, SegmentError> {
        let file = File::open(path)?;
        let advertised_len = file.metadata()?.len();
        enforce_limit("file size", advertised_len, limits.max_file_bytes)?;

        let mut bytes = Vec::new();
        file.take(limits.max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        enforce_limit(
            "file size",
            usize_to_u64(bytes.len())?,
            limits.max_file_bytes,
        )?;
        Self::from_bytes(bytes, limits)
    }

    /// Parses a segment, validating its header, schema, directory, and bounds.
    pub fn from_bytes(bytes: Vec<u8>, limits: DecodeLimits) -> Result<Self, SegmentError> {
        enforce_limit(
            "file size",
            usize_to_u64(bytes.len())?,
            limits.max_file_bytes,
        )?;
        if bytes.len() < HEADER_SIZE {
            return Err(SegmentError::Corrupt("truncated fixed header".into()));
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(SegmentError::Corrupt("invalid segment magic".into()));
        }

        let version = read_u16_at(&bytes, 8)?;
        if version != FORMAT_VERSION {
            return Err(SegmentError::UnsupportedVersion(version));
        }
        if read_u16_at(&bytes, 10)? != 0 || read_u32_at(&bytes, 44)? != 0 {
            return Err(SegmentError::Corrupt(
                "non-zero reserved header bits".into(),
            ));
        }

        let header_len = u32_to_usize(read_u32_at(&bytes, 12)?)?;
        if !(HEADER_SIZE..=bytes.len()).contains(&header_len) {
            return Err(SegmentError::Corrupt("invalid header length".into()));
        }
        enforce_limit(
            "metadata size",
            usize_to_u64(header_len)?,
            limits.max_metadata_bytes,
        )?;

        let expected_header_checksum = read_u32_at(&bytes, HEADER_CHECKSUM_OFFSET)?;
        let actual_header_checksum = checksum_with_zeroed_header_field(&bytes[..header_len]);
        if expected_header_checksum != actual_header_checksum {
            return Err(SegmentError::ChecksumMismatch {
                location: "header".into(),
                expected: expected_header_checksum,
                actual: actual_header_checksum,
            });
        }

        let row_count = read_u64_at(&bytes, 16)?;
        let rows_per_block = read_u32_at(&bytes, 24)?;
        let column_count = read_u32_at(&bytes, 28)?;
        let row_group_count = read_u32_at(&bytes, 32)?;
        let block_count = read_u32_at(&bytes, 36)?;
        enforce_limit("row count", row_count, limits.max_rows)?;
        enforce_limit(
            "rows per block",
            u64::from(rows_per_block),
            u64::from(limits.max_rows_per_block),
        )?;
        enforce_limit(
            "column count",
            u64::from(column_count),
            u64::from(limits.max_columns),
        )?;
        enforce_limit(
            "row group count",
            u64::from(row_group_count),
            u64::from(limits.max_row_groups),
        )?;
        enforce_limit(
            "block count",
            u64::from(block_count),
            u64::from(limits.max_blocks),
        )?;
        if rows_per_block == 0 || column_count == 0 {
            return Err(SegmentError::Corrupt(
                "rows per block and column count must be non-zero".into(),
            ));
        }
        let expected_groups = if row_count == 0 {
            0
        } else {
            ((row_count - 1) / u64::from(rows_per_block)) + 1
        };
        if expected_groups != u64::from(row_group_count) {
            return Err(SegmentError::Corrupt(
                "row group count is inconsistent with row count".into(),
            ));
        }
        let expected_blocks = u64::from(column_count)
            .checked_mul(u64::from(row_group_count))
            .ok_or_else(|| SegmentError::Corrupt("block count overflow".into()))?;
        if expected_blocks != u64::from(block_count) {
            return Err(SegmentError::Corrupt(
                "block count is inconsistent with the segment shape".into(),
            ));
        }

        let mut cursor = Cursor::new(&bytes[HEADER_SIZE..header_len]);
        let schema_field_count = cursor.read_u32()?;
        if schema_field_count != column_count {
            return Err(SegmentError::Corrupt(
                "schema field count does not match header".into(),
            ));
        }
        let schema_capacity = u32_to_usize(column_count)?;
        let mut fields = Vec::with_capacity(schema_capacity);
        for _ in 0..column_count {
            let name_len = u32_to_usize(cursor.read_u32()?)?;
            let name = cursor.read_utf8(name_len, "field name")?;
            let data_type = DataType::from_tag(cursor.read_u8()?)?;
            let nullable = match cursor.read_u8()? {
                0 => false,
                1 => true,
                value => {
                    return Err(SegmentError::Corrupt(format!(
                        "invalid nullable flag {value}"
                    )));
                }
            };
            if cursor.read_u16()? != 0 {
                return Err(SegmentError::Corrupt(
                    "non-zero reserved schema bits".into(),
                ));
            }
            fields.push(Field {
                name,
                data_type,
                nullable,
            });
        }
        validate_fields(&fields).map_err(|error| match error {
            SegmentError::InvalidInput(message) => SegmentError::Corrupt(message),
            other => other,
        })?;
        let schema = Schema { fields };

        let blocks_capacity = u32_to_usize(block_count)?;
        let minimum_directory_bytes = blocks_capacity
            .checked_mul(52)
            .ok_or_else(|| SegmentError::Corrupt("minimum directory size overflow".into()))?;
        if cursor.remaining() < minimum_directory_bytes {
            return Err(SegmentError::Corrupt(
                "block count exceeds available directory metadata".into(),
            ));
        }
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(blocks_capacity)
            .map_err(|_| SegmentError::LimitExceeded {
                resource: "block directory allocation",
                actual: u64::from(block_count),
                limit: u64::from(limits.max_blocks),
            })?;
        let mut next_payload_offset = usize_to_u64(header_len)?;
        for block_index in 0..block_count {
            let column = cursor.read_u32()?;
            let row_group = cursor.read_u32()?;
            let row_start = cursor.read_u64()?;
            let block_rows = cursor.read_u32()?;
            let encoding = Encoding::from_tag(cursor.read_u8()?)?;
            let stats_flags = cursor.read_u8()?;
            if stats_flags & !HAS_MIN_MAX != 0 || cursor.read_u16()? != 0 {
                return Err(SegmentError::Corrupt(
                    "invalid block flags or reserved bits".into(),
                ));
            }
            let offset = cursor.read_u64()?;
            let stored_len = cursor.read_u32()?;
            let logical_len = cursor.read_u64()?;
            let checksum = cursor.read_u32()?;
            let null_count = cursor.read_u32()?;

            let expected_column = block_index % column_count;
            let expected_group = block_index / column_count;
            if column != expected_column || row_group != expected_group {
                return Err(SegmentError::Corrupt(
                    "block directory is not in canonical row-group order".into(),
                ));
            }
            let expected_row_start = u64::from(row_group)
                .checked_mul(u64::from(rows_per_block))
                .ok_or_else(|| SegmentError::Corrupt("row offset overflow".into()))?;
            let remaining_rows = row_count.saturating_sub(expected_row_start);
            let expected_row_count = remaining_rows.min(u64::from(rows_per_block)) as u32;
            if row_start != expected_row_start || block_rows != expected_row_count {
                return Err(SegmentError::Corrupt(
                    "block row range is inconsistent with its row group".into(),
                ));
            }
            if null_count > block_rows {
                return Err(SegmentError::Corrupt(
                    "block null count exceeds its row count".into(),
                ));
            }
            let field = &schema.fields[u32_to_usize(column)?];
            if !encoding.accepts(field.data_type) {
                return Err(SegmentError::Corrupt(
                    "block encoding does not match its column type".into(),
                ));
            }
            if !field.nullable && null_count != 0 {
                return Err(SegmentError::Corrupt(
                    "non-nullable column block contains nulls".into(),
                ));
            }

            let has_min_max = stats_flags & HAS_MIN_MAX != 0;
            if has_min_max != (null_count < block_rows) {
                return Err(SegmentError::Corrupt(
                    "min/max presence is inconsistent with null count".into(),
                ));
            }
            let (min, max) = if has_min_max {
                let min = decode_scalar(&mut cursor, field.data_type, &limits)?;
                let max = decode_scalar(&mut cursor, field.data_type, &limits)?;
                if min.compare(&max) == Some(Ordering::Greater) {
                    return Err(SegmentError::Corrupt(
                        "block minimum is greater than maximum".into(),
                    ));
                }
                (Some(min), Some(max))
            } else {
                (None, None)
            };

            enforce_limit(
                "stored block size",
                u64::from(stored_len),
                limits.max_block_bytes,
            )?;
            enforce_limit(
                "decoded block size",
                decoded_allocation_bound(field.data_type, block_rows, logical_len)?,
                limits.max_decoded_block_bytes,
            )?;
            if field.data_type == DataType::String {
                enforce_limit("string buffer size", logical_len, limits.max_string_bytes)?;
            }
            if offset != next_payload_offset {
                return Err(SegmentError::Corrupt(
                    "block payloads are not contiguous".into(),
                ));
            }
            next_payload_offset = offset
                .checked_add(u64::from(stored_len))
                .ok_or_else(|| SegmentError::Corrupt("block extent overflow".into()))?;
            if next_payload_offset > usize_to_u64(bytes.len())? {
                return Err(SegmentError::Corrupt(
                    "block payload extends past end of file".into(),
                ));
            }

            blocks.push(BlockMeta {
                column,
                row_group,
                row_start,
                row_count: block_rows,
                encoding,
                offset,
                stored_len,
                logical_len,
                checksum,
                stats: BlockStatistics {
                    row_start,
                    row_count: block_rows,
                    null_count,
                    min,
                    max,
                },
            });
        }
        if !cursor.is_empty() {
            return Err(SegmentError::Corrupt(
                "trailing bytes in segment directory".into(),
            ));
        }
        if next_payload_offset != usize_to_u64(bytes.len())? {
            return Err(SegmentError::Corrupt(
                "unreferenced bytes after final block".into(),
            ));
        }

        let segment = Self {
            bytes,
            schema,
            row_count,
            rows_per_block,
            row_group_count,
            blocks,
            limits,
        };
        segment.validate_block_statistics()?;
        Ok(segment)
    }

    pub fn version(&self) -> u16 {
        FORMAT_VERSION
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    pub fn rows_per_block(&self) -> u32 {
        self.rows_per_block
    }

    /// Returns the zone maps for one column without decoding its blocks.
    pub fn block_statistics(&self, column: usize) -> Result<Vec<BlockStatistics>, SegmentError> {
        self.field(column)?;
        Ok((0..self.row_group_count)
            .map(|group| self.block(group, column).stats.clone())
            .collect())
    }

    /// Decodes and checksums every block in one column.
    pub fn read_column(&self, column: usize) -> Result<Column, SegmentError> {
        let result_bytes = self.column_result_bytes(column)?;
        enforce_limit(
            "decoded result size",
            result_bytes,
            self.limits.max_decoded_result_bytes,
        )?;
        self.read_column_after_limit(column)
    }

    fn read_column_after_limit(&self, column: usize) -> Result<Column, SegmentError> {
        let field = self.field(column)?;
        let capacity = u64_to_usize(self.row_count)?;
        let mut output = Column::empty(field.data_type);
        reserve_column(&mut output, capacity)?;
        for group in 0..self.row_group_count {
            output.append_all(self.decode_block(self.block(group, column))?);
        }
        Ok(output)
    }

    /// Decodes and checksums the complete segment.
    pub fn read_all(&self) -> Result<Vec<Column>, SegmentError> {
        let mut result_bytes = 0_u64;
        for column in 0..self.schema.len() {
            result_bytes = result_bytes
                .checked_add(self.column_result_bytes(column)?)
                .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))?;
        }
        enforce_limit(
            "decoded result size",
            result_bytes,
            self.limits.max_decoded_result_bytes,
        )?;
        let mut output = try_vec_with_capacity(self.schema.len(), "decoded column allocation")?;
        for column in 0..self.schema.len() {
            output.push(self.read_column_after_limit(column)?);
        }
        Ok(output)
    }

    /// Scans selected columns, using the predicate's zone map before decoding.
    pub fn scan(
        &self,
        projection: &[usize],
        predicate: Option<&Predicate>,
    ) -> Result<ScanResult, SegmentError> {
        for &column in projection {
            self.field(column)?;
        }
        if let Some(predicate) = predicate {
            self.validate_predicate(predicate)?;
        }

        let mut output = try_vec_with_capacity(projection.len(), "scan projection allocation")?;
        for &column in projection {
            output.push(Column::empty(self.schema.fields[column].data_type));
        }
        let mut metrics = ScanMetrics::default();
        let mut output_rows = 0_u64;
        let mut result_bytes = 0_u64;

        for group in 0..self.row_group_count {
            metrics.row_groups_considered += 1;
            if predicate.is_some_and(|predicate| {
                predicate_can_skip(predicate, &self.block(group, predicate.column()).stats)
            }) {
                metrics.row_groups_pruned += 1;
                continue;
            }

            let predicate_column = if let Some(predicate) = predicate {
                metrics.column_blocks_decoded += 1;
                Some(self.decode_block(self.block(group, predicate.column()))?)
            } else {
                None
            };
            let selected = match (predicate, predicate_column.as_ref()) {
                (Some(predicate), Some(column)) => evaluate_predicate(column, predicate)?,
                (None, _) => all_selected(u32_to_usize(self.block(group, 0).row_count)?)?,
                _ => unreachable!(),
            };
            let selected_count = selected.iter().filter(|value| **value).count();
            output_rows = output_rows
                .checked_add(usize_to_u64(selected_count)?)
                .ok_or_else(|| SegmentError::Corrupt("scan row count overflow".into()))?;

            for (output_column, &column_index) in output.iter_mut().zip(projection) {
                match (&predicate_column, predicate) {
                    (Some(decoded), Some(predicate)) if predicate.column() == column_index => {
                        charge_selected_result(
                            &mut result_bytes,
                            decoded,
                            &selected,
                            self.limits.max_decoded_result_bytes,
                        )?;
                        output_column.append_selected(decoded, &selected)?;
                    }
                    _ => {
                        metrics.column_blocks_decoded += 1;
                        let decoded = self.decode_block(self.block(group, column_index))?;
                        charge_selected_result(
                            &mut result_bytes,
                            &decoded,
                            &selected,
                            self.limits.max_decoded_result_bytes,
                        )?;
                        output_column.append_selected(&decoded, &selected)?;
                    }
                }
            }
        }

        Ok(ScanResult {
            columns: output,
            row_count: output_rows,
            metrics,
        })
    }

    fn field(&self, column: usize) -> Result<&Field, SegmentError> {
        self.schema.fields.get(column).ok_or_else(|| {
            SegmentError::InvalidInput(format!("column index {column} is out of bounds"))
        })
    }

    fn block(&self, group: u32, column: usize) -> &BlockMeta {
        &self.blocks[group as usize * self.schema.len() + column]
    }

    fn validate_predicate(&self, predicate: &Predicate) -> Result<(), SegmentError> {
        let field = self.field(predicate.column())?;
        if let Predicate::Compare { value, .. } = predicate
            && value.data_type() != field.data_type
        {
            return Err(SegmentError::InvalidInput(format!(
                "predicate type {:?} does not match column type {:?}",
                value.data_type(),
                field.data_type
            )));
        }
        Ok(())
    }

    fn column_result_bytes(&self, column: usize) -> Result<u64, SegmentError> {
        let field = self.field(column)?;
        (0..self.row_group_count).try_fold(0_u64, |total, group| {
            let block = self.block(group, column);
            let bytes =
                decoded_allocation_bound(field.data_type, block.row_count, block.logical_len)?;
            total
                .checked_add(bytes)
                .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))
        })
    }

    fn validate_block_statistics(&self) -> Result<(), SegmentError> {
        for meta in &self.blocks {
            let column = self.decode_block(meta)?;
            if !statistics_match_column(&column, &meta.stats)? {
                return Err(SegmentError::Corrupt(format!(
                    "zone map does not match column {} row group {}",
                    meta.column, meta.row_group
                )));
            }
        }
        Ok(())
    }

    fn decode_block(&self, meta: &BlockMeta) -> Result<Column, SegmentError> {
        let start = u64_to_usize(meta.offset)?;
        let end = start
            .checked_add(u32_to_usize(meta.stored_len)?)
            .ok_or_else(|| SegmentError::Corrupt("block slice overflow".into()))?;
        let payload = self
            .bytes
            .get(start..end)
            .ok_or_else(|| SegmentError::Corrupt("block slice is out of bounds".into()))?;
        let actual_checksum = crc32(payload);
        if actual_checksum != meta.checksum {
            return Err(SegmentError::ChecksumMismatch {
                location: format!("column {} row group {}", meta.column, meta.row_group),
                expected: meta.checksum,
                actual: actual_checksum,
            });
        }

        let field = &self.schema.fields[meta.column as usize];
        match meta.encoding {
            Encoding::IntDeltaBitPacked => decode_int_delta(payload, meta),
            Encoding::IntPlain => decode_int_plain(payload, meta),
            Encoding::BoolBitPacked => decode_bool(payload, meta),
            Encoding::StringFrontCoded => decode_strings(payload, meta, &self.limits),
        }
        .and_then(|column| {
            if column.data_type() != field.data_type {
                Err(SegmentError::Corrupt(
                    "decoded block type does not match schema".into(),
                ))
            } else {
                Ok(column)
            }
        })
    }
}

/// Encodes a complete immutable segment into bytes.
pub fn encode_segment(
    schema: &Schema,
    columns: &[Column],
    options: &WriteOptions,
) -> Result<Vec<u8>, SegmentError> {
    validate_input(schema, columns, options)?;
    let row_count = columns[0].len();
    let row_group_count = if row_count == 0 {
        0
    } else {
        row_count.div_ceil(options.rows_per_block)
    };
    let block_count = row_group_count
        .checked_mul(schema.len())
        .ok_or_else(|| SegmentError::InvalidInput("block count overflow".into()))?;
    let mut encoded_blocks = Vec::with_capacity(block_count);

    for group in 0..row_group_count {
        let row_start = group
            .checked_mul(options.rows_per_block)
            .ok_or_else(|| SegmentError::InvalidInput("row offset overflow".into()))?;
        let row_end = row_count.min(row_start.saturating_add(options.rows_per_block));
        for (column_index, column) in columns.iter().enumerate() {
            let (encoding, payload, logical_len, min, max, null_count) =
                encode_column_block(column, row_start, row_end)?;
            let stored_len = usize_to_u32(payload.len(), "stored block size")?;
            let row_count = usize_to_u32(row_end - row_start, "block row count")?;
            encoded_blocks.push(EncodedBlock {
                meta: BlockMeta {
                    column: usize_to_u32(column_index, "column index")?,
                    row_group: usize_to_u32(group, "row group index")?,
                    row_start: usize_to_u64(row_start)?,
                    row_count,
                    encoding,
                    offset: 0,
                    stored_len,
                    logical_len,
                    checksum: crc32(&payload),
                    stats: BlockStatistics {
                        row_start: usize_to_u64(row_start)?,
                        row_count,
                        null_count,
                        min,
                        max,
                    },
                },
                payload,
            });
        }
    }

    let schema_bytes = encode_schema(schema)?;
    let directory_len = encoded_blocks.iter().try_fold(0_usize, |total, block| {
        total
            .checked_add(encoded_meta_len(&block.meta)?)
            .ok_or_else(|| SegmentError::InvalidInput("directory size overflow".into()))
    })?;
    let header_len = HEADER_SIZE
        .checked_add(schema_bytes.len())
        .and_then(|value| value.checked_add(directory_len))
        .ok_or_else(|| SegmentError::InvalidInput("header size overflow".into()))?;
    let header_len_u32 = usize_to_u32(header_len, "header size")?;
    let mut next_offset = usize_to_u64(header_len)?;
    for block in &mut encoded_blocks {
        block.meta.offset = next_offset;
        next_offset = next_offset
            .checked_add(u64::from(block.meta.stored_len))
            .ok_or_else(|| SegmentError::InvalidInput("segment size overflow".into()))?;
    }
    let total_len = u64_to_usize(next_offset)?;
    let mut output = Vec::with_capacity(total_len);
    output.extend_from_slice(MAGIC);
    push_u16(&mut output, FORMAT_VERSION);
    push_u16(&mut output, 0);
    push_u32(&mut output, header_len_u32);
    push_u64(&mut output, usize_to_u64(row_count)?);
    push_u32(
        &mut output,
        usize_to_u32(options.rows_per_block, "rows per block")?,
    );
    push_u32(&mut output, usize_to_u32(schema.len(), "column count")?);
    push_u32(
        &mut output,
        usize_to_u32(row_group_count, "row group count")?,
    );
    push_u32(&mut output, usize_to_u32(block_count, "block count")?);
    push_u32(&mut output, 0);
    push_u32(&mut output, 0);
    debug_assert_eq!(output.len(), HEADER_SIZE);
    output.extend_from_slice(&schema_bytes);
    for block in &encoded_blocks {
        encode_meta(&block.meta, &mut output)?;
    }
    debug_assert_eq!(output.len(), header_len);
    let checksum = checksum_with_zeroed_header_field(&output);
    output[HEADER_CHECKSUM_OFFSET..HEADER_CHECKSUM_OFFSET + 4]
        .copy_from_slice(&checksum.to_le_bytes());
    for block in encoded_blocks {
        output.extend_from_slice(&block.payload);
    }
    debug_assert_eq!(output.len(), total_len);
    Ok(output)
}

/// Atomically publishes a new segment and refuses to replace an existing path.
///
/// The complete segment is written and synced through private staging on the
/// destination filesystem. A platform-specific no-replace operation publishes
/// the final name. Unix publication and directory syncing remain relative to a
/// pinned parent descriptor. The returned outcome distinguishes confirmed
/// durability from cleanup or directory-sync failure after the final path became
/// visible.
pub fn write_segment(
    path: impl AsRef<Path>,
    schema: &Schema,
    columns: &[Column],
    options: &WriteOptions,
) -> Result<SegmentWriteOutcome, SegmentError> {
    let bytes = encode_segment(schema, columns, options)?;
    publish_segment_bytes(path.as_ref(), &bytes, || {})
}

fn publish_segment_bytes(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(),
) -> Result<SegmentWriteOutcome, SegmentError> {
    publish_segment_bytes_inner(path, bytes, before_publish, || {}, None)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationFailure {
    #[cfg(unix)]
    TemporaryCleanup,
    #[cfg(unix)]
    DirectorySync,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn publish_segment_bytes_with_failure(
    path: &Path,
    bytes: &[u8],
    failure: PublicationFailure,
) -> Result<SegmentWriteOutcome, SegmentError> {
    publish_segment_bytes_inner(path, bytes, || {}, || {}, Some(failure))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn publish_segment_bytes_with_hooks(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(),
    before_directory_sync: impl FnOnce(),
) -> Result<SegmentWriteOutcome, SegmentError> {
    publish_segment_bytes_inner(path, bytes, before_publish, before_directory_sync, None)
}

fn publish_segment_bytes_inner(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(),
    before_directory_sync: impl FnOnce(),
    failure: Option<PublicationFailure>,
) -> Result<SegmentWriteOutcome, SegmentError> {
    ensure_private_segment_platform()?;
    let file_name = path.file_name().ok_or_else(|| {
        SegmentError::InvalidInput("segment path must include a file name".into())
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        let parent_dir = File::open(parent)?;
        let (staging_name, staging_dir, mut file) =
            create_temporary_file_at(&parent_dir, file_name)?;
        let mut staging = UnixSegmentStaging::new(&parent_dir, staging_name, staging_dir);
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_publish();
        publish_temporary_file_at(&mut staging, file_name, before_directory_sync, failure)
    }
    #[cfg(windows)]
    {
        let (temporary_path, mut file) = create_temporary_file(parent, file_name)?;
        let mut cleanup = RemoveOnDrop::new(temporary_path.clone());
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_publish();
        let _ = (before_directory_sync, failure);
        publish_temporary_file(&temporary_path, path, &mut cleanup)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            bytes,
            before_publish,
            before_directory_sync,
            failure,
            parent,
            file_name,
        );
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable atomic segment publication is unsupported on this platform",
        )
        .into())
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn ensure_private_segment_platform() -> Result<(), SegmentError> {
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn ensure_private_segment_platform() -> Result<(), SegmentError> {
    Err(SegmentError::UnsupportedPlatform(
        "private immutable files require Windows, macOS, or Linux ACL semantics",
    ))
}

#[cfg(unix)]
fn publish_temporary_file_at(
    staging: &mut UnixSegmentStaging<'_>,
    file_name: &OsStr,
    before_directory_sync: impl FnOnce(),
    failure: Option<PublicationFailure>,
) -> Result<SegmentWriteOutcome, SegmentError> {
    use rustix::fs::AtFlags;

    rustix::fs::linkat(
        &staging.staging_dir,
        "segment",
        staging.parent_dir,
        file_name,
        AtFlags::empty(),
    )
    .map_err(segment_rustix_error)?;
    let cleanup_result = if failure == Some(PublicationFailure::TemporaryCleanup) {
        Err(io::Error::other("injected temporary cleanup failure"))
    } else {
        staging.remove_candidate()
    };
    let directory_cleanup_result = staging.remove_directory();
    before_directory_sync();
    let sync_result = if failure == Some(PublicationFailure::DirectorySync) {
        Err(io::Error::other("injected segment directory sync failure"))
    } else {
        staging.parent_dir.sync_all()
    };
    let cleanup_error = cleanup_result
        .err()
        .or_else(|| directory_cleanup_result.err());
    let sync_error = sync_result.err();
    match (cleanup_error, sync_error) {
        (None, None) => Ok(SegmentWriteOutcome::Durable),
        (cleanup_error, sync_error) => Ok(SegmentWriteOutcome::PublishedUncertain {
            message: publication_uncertainty_message(cleanup_error, sync_error),
        }),
    }
}

#[cfg(unix)]
fn publication_uncertainty_message(
    cleanup_error: Option<io::Error>,
    sync_error: Option<io::Error>,
) -> String {
    match (cleanup_error, sync_error) {
        (Some(cleanup), Some(sync)) => format!(
            "segment was published, but temporary cleanup failed ({cleanup}) and directory sync failed ({sync})"
        ),
        (Some(cleanup), None) => {
            format!("segment was published, but temporary cleanup failed: {cleanup}")
        }
        (None, Some(sync)) => {
            format!("segment was published, but directory sync failed: {sync}")
        }
        (None, None) => unreachable!(),
    }
}

#[cfg(windows)]
fn publish_temporary_file(
    temporary_path: &Path,
    path: &Path,
    cleanup: &mut RemoveOnDrop,
) -> Result<SegmentWriteOutcome, SegmentError> {
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let temporary_path = nul_terminated_wide_path(temporary_path)?;
    let path = nul_terminated_wide_path(path)?;
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that remain
    // alive for the call. Not setting MOVEFILE_REPLACE_EXISTING preserves the
    // immutable no-replace contract.
    let moved = unsafe {
        MoveFileExW(
            temporary_path.as_ptr(),
            path.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error().into());
    }
    cleanup.disarm();
    Ok(SegmentWriteOutcome::Durable)
}

#[cfg(windows)]
fn nul_terminated_wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path contains an interior NUL",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

#[cfg(windows)]
fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..1_024 {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(
            ".rusthouse-tmp-{}-{timestamp}-{id}",
            std::process::id()
        ));
        let path = parent.join(temporary_name);
        match create_private_segment_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary segment name",
    ))
}

#[cfg(windows)]
fn create_private_segment_file(path: &Path) -> io::Result<File> {
    crate::catalog::create_secure_temp(path).map_err(io::Error::other)
}

#[cfg(unix)]
fn create_temporary_file_at(
    parent_dir: &File,
    file_name: &OsStr,
) -> io::Result<(OsString, File, File)> {
    create_temporary_file_at_with(parent_dir, file_name, |staging_dir| {
        crate::catalog::create_secure_file_at(staging_dir, OsStr::new("segment"), false)
    })
}

#[cfg(unix)]
fn create_temporary_file_at_with(
    parent_dir: &File,
    file_name: &OsStr,
    mut create_file: impl FnMut(&File) -> Result<File, crate::catalog::SnapshotError>,
) -> io::Result<(OsString, File, File)> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..1_024 {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let mut staging_name = OsString::from(".");
        staging_name.push(file_name);
        staging_name.push(format!(
            ".rusthouse-tmp-{}-{timestamp}-{id}",
            std::process::id()
        ));
        let staging_dir =
            match crate::catalog::create_secure_staging_dir_at(parent_dir, &staging_name) {
                Ok(directory) => directory,
                Err(crate::catalog::SnapshotError::Io(error))
                    if error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(snapshot_io_error(error)),
            };
        match create_file(&staging_dir) {
            Ok(file) => return Ok((staging_name, staging_dir, file)),
            Err(error) => {
                let _ = rustix::fs::unlinkat(&staging_dir, "segment", rustix::fs::AtFlags::empty());
                drop(staging_dir);
                let _ =
                    rustix::fs::unlinkat(parent_dir, &staging_name, rustix::fs::AtFlags::REMOVEDIR);
                return Err(snapshot_io_error(error));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary segment directory",
    ))
}

#[cfg(unix)]
fn snapshot_io_error(error: crate::catalog::SnapshotError) -> io::Error {
    match error {
        crate::catalog::SnapshotError::Io(error) => error,
        error => io::Error::other(error.to_string()),
    }
}

#[cfg(unix)]
fn segment_rustix_error(error: rustix::io::Errno) -> SegmentError {
    io::Error::from_raw_os_error(error.raw_os_error()).into()
}

#[cfg(unix)]
struct UnixSegmentStaging<'a> {
    parent_dir: &'a File,
    staging_name: OsString,
    staging_dir: File,
    candidate_present: bool,
    directory_present: bool,
}

#[cfg(unix)]
impl<'a> UnixSegmentStaging<'a> {
    fn new(parent_dir: &'a File, staging_name: OsString, staging_dir: File) -> Self {
        Self {
            parent_dir,
            staging_name,
            staging_dir,
            candidate_present: true,
            directory_present: true,
        }
    }

    fn remove_candidate(&mut self) -> io::Result<()> {
        rustix::fs::unlinkat(&self.staging_dir, "segment", rustix::fs::AtFlags::empty())
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        self.candidate_present = false;
        Ok(())
    }

    fn remove_directory(&mut self) -> io::Result<()> {
        rustix::fs::unlinkat(
            self.parent_dir,
            &self.staging_name,
            rustix::fs::AtFlags::REMOVEDIR,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        self.directory_present = false;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for UnixSegmentStaging<'_> {
    fn drop(&mut self) {
        if self.candidate_present {
            let _ =
                rustix::fs::unlinkat(&self.staging_dir, "segment", rustix::fs::AtFlags::empty());
        }
        if self.directory_present {
            let _ = rustix::fs::unlinkat(
                self.parent_dir,
                &self.staging_name,
                rustix::fs::AtFlags::REMOVEDIR,
            );
        }
    }
}

#[cfg(windows)]
struct RemoveOnDrop {
    path: PathBuf,
    armed: bool,
}

#[cfg(windows)]
impl RemoveOnDrop {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(windows)]
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_fields(fields: &[Field]) -> Result<(), SegmentError> {
    if fields.is_empty() {
        return Err(SegmentError::InvalidInput(
            "schema must contain at least one field".into(),
        ));
    }
    let mut names = HashSet::with_capacity(fields.len());
    for field in fields {
        if field.name.is_empty() {
            return Err(SegmentError::InvalidInput(
                "field names must not be empty".into(),
            ));
        }
        if !names.insert(field.name.as_str()) {
            return Err(SegmentError::InvalidInput(format!(
                "duplicate field name {:?}",
                field.name
            )));
        }
    }
    Ok(())
}

fn validate_input(
    schema: &Schema,
    columns: &[Column],
    options: &WriteOptions,
) -> Result<(), SegmentError> {
    if columns.len() != schema.len() {
        return Err(SegmentError::InvalidInput(format!(
            "schema has {} fields but {} columns were supplied",
            schema.len(),
            columns.len()
        )));
    }
    if options.rows_per_block == 0 || options.rows_per_block > 1_000_000 {
        return Err(SegmentError::InvalidInput(
            "rows_per_block must be between 1 and 1,000,000".into(),
        ));
    }
    let row_count = columns.first().map_or(0, Column::len);
    for (index, (field, column)) in schema.fields.iter().zip(columns).enumerate() {
        if field.data_type != column.data_type() {
            return Err(SegmentError::InvalidInput(format!(
                "column {index} has type {:?}, expected {:?}",
                column.data_type(),
                field.data_type
            )));
        }
        if column.len() != row_count {
            return Err(SegmentError::InvalidInput(format!(
                "column {index} has {} rows, expected {row_count}",
                column.len()
            )));
        }
        if !field.nullable && column.has_nulls() {
            return Err(SegmentError::InvalidInput(format!(
                "non-nullable column {index} contains nulls"
            )));
        }
    }
    Ok(())
}

fn encode_schema(schema: &Schema) -> Result<Vec<u8>, SegmentError> {
    let mut output = Vec::new();
    push_u32(&mut output, usize_to_u32(schema.len(), "column count")?);
    for field in &schema.fields {
        push_u32(
            &mut output,
            usize_to_u32(field.name.len(), "field name length")?,
        );
        output.extend_from_slice(field.name.as_bytes());
        output.push(field.data_type.tag());
        output.push(u8::from(field.nullable));
        push_u16(&mut output, 0);
    }
    Ok(output)
}

fn encoded_meta_len(meta: &BlockMeta) -> Result<usize, SegmentError> {
    let mut length = 52_usize;
    if let (Some(min), Some(max)) = (&meta.stats.min, &meta.stats.max) {
        let min_len = encoded_scalar_len(min)?;
        let max_len = encoded_scalar_len(max)?;
        length = length
            .checked_add(min_len)
            .and_then(|value| value.checked_add(max_len))
            .ok_or_else(|| SegmentError::InvalidInput("block metadata size overflow".into()))?;
    }
    Ok(length)
}

fn encoded_scalar_len(value: &ScalarValue) -> Result<usize, SegmentError> {
    match value {
        ScalarValue::Int64(_) => Ok(8),
        ScalarValue::Bool(_) => Ok(1),
        ScalarValue::String(value) => value
            .len()
            .checked_add(4)
            .ok_or_else(|| SegmentError::InvalidInput("string statistic size overflow".into())),
    }
}

fn encode_meta(meta: &BlockMeta, output: &mut Vec<u8>) -> Result<(), SegmentError> {
    push_u32(output, meta.column);
    push_u32(output, meta.row_group);
    push_u64(output, meta.row_start);
    push_u32(output, meta.row_count);
    output.push(meta.encoding.tag());
    output.push(if meta.stats.min.is_some() {
        HAS_MIN_MAX
    } else {
        0
    });
    push_u16(output, 0);
    push_u64(output, meta.offset);
    push_u32(output, meta.stored_len);
    push_u64(output, meta.logical_len);
    push_u32(output, meta.checksum);
    push_u32(output, meta.stats.null_count);
    match (&meta.stats.min, &meta.stats.max) {
        (Some(min), Some(max)) => {
            encode_scalar(min, output)?;
            encode_scalar(max, output)?;
        }
        (None, None) => {}
        _ => {
            return Err(SegmentError::InvalidInput(
                "block must contain both minimum and maximum or neither".into(),
            ));
        }
    }
    Ok(())
}

fn encode_scalar(value: &ScalarValue, output: &mut Vec<u8>) -> Result<(), SegmentError> {
    match value {
        ScalarValue::Int64(value) => output.extend_from_slice(&value.to_le_bytes()),
        ScalarValue::Bool(value) => output.push(u8::from(*value)),
        ScalarValue::String(value) => {
            push_u32(
                output,
                usize_to_u32(value.len(), "string statistic length")?,
            );
            output.extend_from_slice(value.as_bytes());
        }
    }
    Ok(())
}

fn decode_scalar(
    cursor: &mut Cursor<'_>,
    data_type: DataType,
    limits: &DecodeLimits,
) -> Result<ScalarValue, SegmentError> {
    match data_type {
        DataType::Int64 => Ok(ScalarValue::Int64(cursor.read_i64()?)),
        DataType::Bool => match cursor.read_u8()? {
            0 => Ok(ScalarValue::Bool(false)),
            1 => Ok(ScalarValue::Bool(true)),
            value => Err(SegmentError::Corrupt(format!(
                "invalid boolean statistic {value}"
            ))),
        },
        DataType::String => {
            let length = cursor.read_u32()?;
            enforce_limit(
                "string statistic size",
                u64::from(length),
                limits.max_string_bytes,
            )?;
            Ok(ScalarValue::String(
                cursor.read_utf8(u32_to_usize(length)?, "string statistic")?,
            ))
        }
    }
}

fn encode_column_block(
    column: &Column,
    start: usize,
    end: usize,
) -> Result<EncodedColumnBlock, SegmentError> {
    match column {
        Column::Int64(values) => encode_int_block(&values[start..end]),
        Column::Bool(values) => encode_bool_block(&values[start..end]),
        Column::String(values) => encode_string_block(&values[start..end]),
    }
}

type EncodedColumnBlock = (
    Encoding,
    Vec<u8>,
    u64,
    Option<ScalarValue>,
    Option<ScalarValue>,
    u32,
);

fn encode_int_block(values: &[Option<i64>]) -> Result<EncodedColumnBlock, SegmentError> {
    let (null_bitmap, non_null): (Vec<u8>, Vec<i64>) = encode_nullable(values);
    let null_count = usize_to_u32(values.len() - non_null.len(), "null count")?;
    let min = non_null.iter().min().copied().map(ScalarValue::Int64);
    let max = non_null.iter().max().copied().map(ScalarValue::Int64);
    let logical_len = usize_to_u64(
        non_null
            .len()
            .checked_mul(8)
            .ok_or_else(|| SegmentError::InvalidInput("integer buffer size overflow".into()))?,
    )?;

    let mut deltas = Vec::with_capacity(non_null.len().saturating_sub(1));
    let mut delta_fits = true;
    for pair in non_null.windows(2) {
        let difference = i128::from(pair[1]) - i128::from(pair[0]);
        if let Ok(difference) = i64::try_from(difference) {
            deltas.push(zigzag_encode(difference));
        } else {
            delta_fits = false;
            break;
        }
    }

    if delta_fits {
        let bit_width = deltas
            .iter()
            .copied()
            .max()
            .map_or(0, |value| (u64::BITS - value.leading_zeros()) as u8);
        let packed = pack_bits(&deltas, bit_width);
        let mut payload = null_bitmap;
        if let Some(first) = non_null.first() {
            payload.extend_from_slice(&first.to_le_bytes());
            payload.push(bit_width);
            payload.extend_from_slice(&packed);
        }
        Ok((
            Encoding::IntDeltaBitPacked,
            payload,
            logical_len,
            min,
            max,
            null_count,
        ))
    } else {
        let mut payload = null_bitmap;
        for value in non_null {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        Ok((
            Encoding::IntPlain,
            payload,
            logical_len,
            min,
            max,
            null_count,
        ))
    }
}

fn encode_bool_block(values: &[Option<bool>]) -> Result<EncodedColumnBlock, SegmentError> {
    let bitmap_len = bitmap_len(values.len())?;
    let mut nulls = vec![0_u8; bitmap_len];
    let mut booleans = vec![0_u8; bitmap_len];
    let mut null_count = 0_usize;
    let mut min = None;
    let mut max = None;
    for (index, value) in values.iter().enumerate() {
        match value {
            Some(value) => {
                set_bit(&mut nulls, index);
                if *value {
                    set_bit(&mut booleans, index);
                }
                min = Some(min.map_or(*value, |current: bool| current.min(*value)));
                max = Some(max.map_or(*value, |current: bool| current.max(*value)));
            }
            None => null_count += 1,
        }
    }
    nulls.extend_from_slice(&booleans);
    Ok((
        Encoding::BoolBitPacked,
        nulls,
        usize_to_u64(bitmap_len)?,
        min.map(ScalarValue::Bool),
        max.map(ScalarValue::Bool),
        usize_to_u32(null_count, "null count")?,
    ))
}

fn encode_string_block(values: &[Option<String>]) -> Result<EncodedColumnBlock, SegmentError> {
    let bitmap_len = bitmap_len(values.len())?;
    let mut payload = vec![0_u8; bitmap_len];
    let mut buffer = Vec::new();
    let mut previous: &[u8] = &[];
    let mut total_bytes = 0_u64;
    let mut null_count = 0_usize;
    let mut min: Option<&str> = None;
    let mut max: Option<&str> = None;
    for (index, value) in values.iter().enumerate() {
        if let Some(value) = value {
            set_bit(&mut payload, index);
            let bytes = value.as_bytes();
            total_bytes = total_bytes
                .checked_add(usize_to_u64(bytes.len())?)
                .ok_or_else(|| SegmentError::InvalidInput("string buffer size overflow".into()))?;
            let prefix = common_prefix_len(previous, bytes);
            encode_varint(usize_to_u64(prefix)?, &mut buffer);
            encode_varint(usize_to_u64(bytes.len() - prefix)?, &mut buffer);
            buffer.extend_from_slice(&bytes[prefix..]);
            previous = bytes;
            min = Some(min.map_or(value.as_str(), |current| current.min(value.as_str())));
            max = Some(max.map_or(value.as_str(), |current| current.max(value.as_str())));
        } else {
            null_count += 1;
        }
    }
    push_u64(&mut payload, total_bytes);
    payload.extend_from_slice(&buffer);
    Ok((
        Encoding::StringFrontCoded,
        payload,
        total_bytes,
        min.map(|value| ScalarValue::String(value.to_owned())),
        max.map(|value| ScalarValue::String(value.to_owned())),
        usize_to_u32(null_count, "null count")?,
    ))
}

fn encode_nullable<T: Copy>(values: &[Option<T>]) -> (Vec<u8>, Vec<T>) {
    let mut bitmap = vec![0_u8; values.len().div_ceil(8)];
    let mut non_null = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        if let Some(value) = value {
            set_bit(&mut bitmap, index);
            non_null.push(*value);
        }
    }
    (bitmap, non_null)
}

fn decode_int_delta(payload: &[u8], meta: &BlockMeta) -> Result<Column, SegmentError> {
    let row_count = u32_to_usize(meta.row_count)?;
    let bitmap_length = bitmap_len(row_count)?;
    let (bitmap, encoded) = split_at_checked(payload, bitmap_length, "integer null bitmap")?;
    validate_bitmap_padding(bitmap, row_count)?;
    let value_count = row_count - u32_to_usize(meta.stats.null_count)?;
    let expected_logical = usize_to_u64(
        value_count
            .checked_mul(8)
            .ok_or_else(|| SegmentError::Corrupt("integer logical length overflow".into()))?,
    )?;
    if meta.logical_len != expected_logical {
        return Err(SegmentError::Corrupt(
            "integer logical length does not match value count".into(),
        ));
    }
    if count_set_bits(bitmap, row_count) != value_count {
        return Err(SegmentError::Corrupt(
            "integer null bitmap does not match null count".into(),
        ));
    }
    if value_count == 0 {
        if !encoded.is_empty() {
            return Err(SegmentError::Corrupt(
                "all-null integer block contains values".into(),
            ));
        }
        return Ok(Column::Int64(allocate_none_column(row_count)?));
    }
    if encoded.len() < 9 {
        return Err(SegmentError::Corrupt(
            "truncated delta integer block".into(),
        ));
    }
    let first = i64::from_le_bytes(encoded[..8].try_into().unwrap());
    let bit_width = encoded[8];
    if bit_width > 64 {
        return Err(SegmentError::Corrupt(format!(
            "invalid integer bit width {bit_width}"
        )));
    }
    let expected_packed_len = packed_len(value_count - 1, bit_width)?;
    if encoded.len() != 9 + expected_packed_len {
        return Err(SegmentError::Corrupt(
            "delta integer payload length is inconsistent".into(),
        ));
    }
    validate_packed_padding(&encoded[9..], value_count - 1, bit_width)?;
    let mut deltas = PackedBitReader::new(&encoded[9..]);
    let mut output = try_vec_with_capacity(row_count, "integer block allocation")?;
    let mut current = first;
    let mut decoded_values = 0_usize;
    for index in 0..row_count {
        if bit_is_set(bitmap, index) {
            if decoded_values != 0 {
                let delta = zigzag_decode(deltas.read(bit_width)?);
                current = current
                    .checked_add(delta)
                    .ok_or_else(|| SegmentError::Corrupt("integer delta overflow".into()))?;
            }
            output.push(Some(current));
            decoded_values += 1;
        } else {
            output.push(None);
        }
    }
    debug_assert_eq!(decoded_values, value_count);
    Ok(Column::Int64(output))
}

fn decode_int_plain(payload: &[u8], meta: &BlockMeta) -> Result<Column, SegmentError> {
    let row_count = u32_to_usize(meta.row_count)?;
    let bitmap_length = bitmap_len(row_count)?;
    let (bitmap, encoded) = split_at_checked(payload, bitmap_length, "integer null bitmap")?;
    validate_bitmap_padding(bitmap, row_count)?;
    let value_count = row_count - u32_to_usize(meta.stats.null_count)?;
    let byte_count = value_count
        .checked_mul(8)
        .ok_or_else(|| SegmentError::Corrupt("integer byte count overflow".into()))?;
    if encoded.len() != byte_count || meta.logical_len != usize_to_u64(byte_count)? {
        return Err(SegmentError::Corrupt(
            "plain integer payload length is inconsistent".into(),
        ));
    }
    if count_set_bits(bitmap, row_count) != value_count {
        return Err(SegmentError::Corrupt(
            "integer null bitmap does not match null count".into(),
        ));
    }
    let mut values = encoded.chunks_exact(8);
    let mut output = try_vec_with_capacity(row_count, "integer block allocation")?;
    for index in 0..row_count {
        output.push(if bit_is_set(bitmap, index) {
            let bytes = values.next().ok_or_else(|| {
                SegmentError::Corrupt("integer null bitmap references a missing value".into())
            })?;
            Some(i64::from_le_bytes(bytes.try_into().unwrap()))
        } else {
            None
        });
    }
    if values.next().is_some() {
        return Err(SegmentError::Corrupt(
            "integer block contains unreferenced values".into(),
        ));
    }
    Ok(Column::Int64(output))
}

fn decode_bool(payload: &[u8], meta: &BlockMeta) -> Result<Column, SegmentError> {
    let row_count = u32_to_usize(meta.row_count)?;
    let bitmap_length = bitmap_len(row_count)?;
    if payload.len() != bitmap_length.saturating_mul(2)
        || meta.logical_len != usize_to_u64(bitmap_length)?
    {
        return Err(SegmentError::Corrupt(
            "boolean payload length is inconsistent".into(),
        ));
    }
    let (nulls, values) = payload.split_at(bitmap_length);
    validate_bitmap_padding(nulls, row_count)?;
    validate_bitmap_padding(values, row_count)?;
    if row_count - count_set_bits(nulls, row_count) != u32_to_usize(meta.stats.null_count)? {
        return Err(SegmentError::Corrupt(
            "boolean null bitmap does not match null count".into(),
        ));
    }
    let mut output = try_vec_with_capacity(row_count, "boolean block allocation")?;
    output.extend(
        (0..row_count).map(|index| bit_is_set(nulls, index).then(|| bit_is_set(values, index))),
    );
    Ok(Column::Bool(output))
}

fn decode_strings(
    payload: &[u8],
    meta: &BlockMeta,
    limits: &DecodeLimits,
) -> Result<Column, SegmentError> {
    let row_count = u32_to_usize(meta.row_count)?;
    let bitmap_length = bitmap_len(row_count)?;
    let (bitmap, remaining) = split_at_checked(payload, bitmap_length, "string null bitmap")?;
    validate_bitmap_padding(bitmap, row_count)?;
    if remaining.len() < 8 {
        return Err(SegmentError::Corrupt(
            "truncated string buffer length".into(),
        ));
    }
    let declared_total = u64::from_le_bytes(remaining[..8].try_into().unwrap());
    if declared_total != meta.logical_len {
        return Err(SegmentError::Corrupt(
            "string buffer length does not match directory".into(),
        ));
    }
    enforce_limit(
        "string buffer size",
        declared_total,
        limits.max_string_bytes,
    )?;
    let non_null_count = row_count - u32_to_usize(meta.stats.null_count)?;
    if count_set_bits(bitmap, row_count) != non_null_count {
        return Err(SegmentError::Corrupt(
            "string null bitmap does not match null count".into(),
        ));
    }

    let mut cursor = Cursor::new(&remaining[8..]);
    let mut output: Vec<Option<String>> =
        try_vec_with_capacity(row_count, "string block allocation")?;
    let mut previous_index: Option<usize> = None;
    let mut total = 0_u64;
    let mut decoded_values = 0_usize;
    for row in 0..row_count {
        if !bit_is_set(bitmap, row) {
            output.push(None);
            continue;
        }
        let prefix = u64_to_usize(cursor.read_varint()?)?;
        let suffix_len = u64_to_usize(cursor.read_varint()?)?;
        let previous = previous_index
            .and_then(|index| output[index].as_ref())
            .map_or(&[][..], |value| value.as_bytes());
        if prefix > previous.len() {
            return Err(SegmentError::Corrupt(
                "string prefix exceeds previous value".into(),
            ));
        }
        let value_len = prefix
            .checked_add(suffix_len)
            .ok_or_else(|| SegmentError::Corrupt("string length overflow".into()))?;
        enforce_limit(
            "individual string size",
            usize_to_u64(value_len)?,
            limits.max_string_bytes,
        )?;
        total = total
            .checked_add(usize_to_u64(value_len)?)
            .ok_or_else(|| SegmentError::Corrupt("string buffer size overflow".into()))?;
        if total > declared_total {
            return Err(SegmentError::Corrupt(
                "decoded strings exceed declared buffer size".into(),
            ));
        }
        let suffix = cursor.read_bytes(suffix_len, "string suffix")?;
        let mut value = try_vec_with_capacity(value_len, "string value allocation")?;
        value.extend_from_slice(&previous[..prefix]);
        value.extend_from_slice(suffix);
        let value = String::from_utf8(value)
            .map_err(|_| SegmentError::Corrupt("string block contains invalid UTF-8".into()))?;
        output.push(Some(value));
        previous_index = Some(row);
        decoded_values += 1;
    }
    if decoded_values != non_null_count || total != declared_total || !cursor.is_empty() {
        return Err(SegmentError::Corrupt(
            "string payload length is inconsistent".into(),
        ));
    }
    Ok(Column::String(output))
}

fn statistics_match_column(
    column: &Column,
    expected: &BlockStatistics,
) -> Result<bool, SegmentError> {
    if usize_to_u32(column.len(), "decoded block row count")? != expected.row_count {
        return Ok(false);
    }
    let null_count = column_null_count(column);
    if usize_to_u32(null_count, "decoded block null count")? != expected.null_count {
        return Ok(false);
    }

    Ok(match column {
        Column::Int64(values) => {
            let min = values.iter().flatten().min().copied();
            let max = values.iter().flatten().max().copied();
            optional_int_stat_matches(min, expected.min.as_ref())
                && optional_int_stat_matches(max, expected.max.as_ref())
        }
        Column::Bool(values) => {
            let min = values.iter().flatten().min().copied();
            let max = values.iter().flatten().max().copied();
            optional_bool_stat_matches(min, expected.min.as_ref())
                && optional_bool_stat_matches(max, expected.max.as_ref())
        }
        Column::String(values) => {
            let min = values.iter().flatten().min();
            let max = values.iter().flatten().max();
            optional_string_stat_matches(min, expected.min.as_ref())
                && optional_string_stat_matches(max, expected.max.as_ref())
        }
    })
}

fn column_null_count(column: &Column) -> usize {
    match column {
        Column::Int64(values) => values.iter().filter(|value| value.is_none()).count(),
        Column::Bool(values) => values.iter().filter(|value| value.is_none()).count(),
        Column::String(values) => values.iter().filter(|value| value.is_none()).count(),
    }
}

fn optional_int_stat_matches(actual: Option<i64>, expected: Option<&ScalarValue>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(ScalarValue::Int64(expected))) => actual == *expected,
        _ => false,
    }
}

fn optional_bool_stat_matches(actual: Option<bool>, expected: Option<&ScalarValue>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(ScalarValue::Bool(expected))) => actual == *expected,
        _ => false,
    }
}

fn optional_string_stat_matches(actual: Option<&String>, expected: Option<&ScalarValue>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(ScalarValue::String(expected))) => actual == expected,
        _ => false,
    }
}

fn predicate_can_skip(predicate: &Predicate, stats: &BlockStatistics) -> bool {
    match predicate {
        Predicate::IsNull { .. } => stats.null_count == 0,
        Predicate::IsNotNull { .. } => stats.null_count == stats.row_count,
        Predicate::Compare { op, value, .. } => {
            let (Some(min), Some(max)) = (&stats.min, &stats.max) else {
                return true;
            };
            let min_vs_value = min.compare(value).unwrap();
            let max_vs_value = max.compare(value).unwrap();
            match op {
                ComparisonOp::Eq => {
                    min_vs_value == Ordering::Greater || max_vs_value == Ordering::Less
                }
                ComparisonOp::NotEq => {
                    min_vs_value == Ordering::Equal && max_vs_value == Ordering::Equal
                }
                ComparisonOp::Lt => min_vs_value != Ordering::Less,
                ComparisonOp::LessOrEq => min_vs_value == Ordering::Greater,
                ComparisonOp::Gt => max_vs_value != Ordering::Greater,
                ComparisonOp::GreaterOrEq => max_vs_value == Ordering::Less,
            }
        }
    }
}

fn evaluate_predicate(column: &Column, predicate: &Predicate) -> Result<Vec<bool>, SegmentError> {
    let mut selected = try_vec_with_capacity(column.len(), "scan selection allocation")?;
    match (column, predicate) {
        (Column::Int64(values), Predicate::Compare { op, value, .. }) => {
            let ScalarValue::Int64(target) = value else {
                unreachable!()
            };
            selected.extend(
                values
                    .iter()
                    .map(|value| value.is_some_and(|value| compare(value.cmp(target), *op))),
            );
        }
        (Column::Bool(values), Predicate::Compare { op, value, .. }) => {
            let ScalarValue::Bool(target) = value else {
                unreachable!()
            };
            selected.extend(
                values
                    .iter()
                    .map(|value| value.is_some_and(|value| compare(value.cmp(target), *op))),
            );
        }
        (Column::String(values), Predicate::Compare { op, value, .. }) => {
            let ScalarValue::String(target) = value else {
                unreachable!()
            };
            selected.extend(values.iter().map(|value| {
                value
                    .as_ref()
                    .is_some_and(|value| compare(value.cmp(target), *op))
            }));
        }
        (Column::Int64(values), Predicate::IsNull { .. }) => {
            selected.extend(values.iter().map(Option::is_none));
        }
        (Column::Bool(values), Predicate::IsNull { .. }) => {
            selected.extend(values.iter().map(Option::is_none));
        }
        (Column::String(values), Predicate::IsNull { .. }) => {
            selected.extend(values.iter().map(Option::is_none));
        }
        (Column::Int64(values), Predicate::IsNotNull { .. }) => {
            selected.extend(values.iter().map(Option::is_some));
        }
        (Column::Bool(values), Predicate::IsNotNull { .. }) => {
            selected.extend(values.iter().map(Option::is_some));
        }
        (Column::String(values), Predicate::IsNotNull { .. }) => {
            selected.extend(values.iter().map(Option::is_some));
        }
    }
    Ok(selected)
}

fn all_selected(row_count: usize) -> Result<Vec<bool>, SegmentError> {
    let mut selected = try_vec_with_capacity(row_count, "scan selection allocation")?;
    selected.resize(row_count, true);
    Ok(selected)
}

fn charge_selected_result(
    retained_bytes: &mut u64,
    column: &Column,
    selected: &[bool],
    limit: u64,
) -> Result<(), SegmentError> {
    let selected_rows = selected.iter().filter(|keep| **keep).count();
    let row_width = match column {
        Column::Int64(_) => std::mem::size_of::<Option<i64>>(),
        Column::Bool(_) => std::mem::size_of::<Option<bool>>(),
        Column::String(_) => std::mem::size_of::<Option<String>>(),
    };
    let row_bytes = selected_rows
        .checked_mul(row_width)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))?;
    let string_bytes = match column {
        Column::String(values) => {
            values
                .iter()
                .zip(selected)
                .try_fold(0_u64, |total, (value, keep)| {
                    if !*keep {
                        return Ok(total);
                    }
                    let length = value.as_ref().map_or(0, String::len);
                    total
                        .checked_add(usize_to_u64(length)?)
                        .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))
                })?
        }
        _ => 0,
    };
    let additional = row_bytes
        .checked_add(string_bytes)
        .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))?;
    let actual = retained_bytes
        .checked_add(additional)
        .ok_or_else(|| SegmentError::Corrupt("decoded result size overflow".into()))?;
    enforce_limit("decoded result size", actual, limit)?;
    *retained_bytes = actual;
    Ok(())
}

fn try_clone_string(value: &str) -> Result<String, SegmentError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| SegmentError::LimitExceeded {
            resource: "string result allocation",
            actual: usize_to_u64(value.len()).unwrap_or(u64::MAX),
            limit: usize_to_u64(isize::MAX as usize).unwrap_or(u64::MAX),
        })?;
    output.push_str(value);
    Ok(output)
}

fn compare(ordering: Ordering, operation: ComparisonOp) -> bool {
    match operation {
        ComparisonOp::Eq => ordering == Ordering::Equal,
        ComparisonOp::NotEq => ordering != Ordering::Equal,
        ComparisonOp::Lt => ordering == Ordering::Less,
        ComparisonOp::LessOrEq => ordering != Ordering::Greater,
        ComparisonOp::Gt => ordering == Ordering::Greater,
        ComparisonOp::GreaterOrEq => ordering != Ordering::Less,
    }
}

/// Upper bound on heap bytes requested concurrently by a block decoder.
///
/// Payload bytes are borrowed from the segment. Decoders stream directly into
/// the final nullable vector, and string capacities sum to `logical_len`.
fn decoded_allocation_bound(
    data_type: DataType,
    rows: u32,
    logical_len: u64,
) -> Result<u64, SegmentError> {
    let row_width = match data_type {
        DataType::Int64 => std::mem::size_of::<Option<i64>>(),
        DataType::Bool => std::mem::size_of::<Option<bool>>(),
        DataType::String => std::mem::size_of::<Option<String>>(),
    };
    u64::from(rows)
        .checked_mul(usize_to_u64(row_width)?)
        .and_then(|bytes| {
            if data_type == DataType::String {
                bytes.checked_add(logical_len)
            } else {
                Some(bytes)
            }
        })
        .ok_or_else(|| SegmentError::Corrupt("decoded block size overflow".into()))
}

fn reserve_column(column: &mut Column, additional: usize) -> Result<(), SegmentError> {
    let element_size = match column {
        Column::Int64(_) => std::mem::size_of::<Option<i64>>(),
        Column::Bool(_) => std::mem::size_of::<Option<bool>>(),
        Column::String(_) => std::mem::size_of::<Option<String>>(),
    };
    let result = match column {
        Column::Int64(values) => values.try_reserve_exact(additional),
        Column::Bool(values) => values.try_reserve_exact(additional),
        Column::String(values) => values.try_reserve_exact(additional),
    };
    result.map_err(|_| SegmentError::LimitExceeded {
        resource: "column allocation",
        actual: additional
            .checked_mul(element_size)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .unwrap_or(u64::MAX),
        limit: usize_to_u64(isize::MAX as usize).unwrap_or(u64::MAX),
    })
}

fn try_vec_with_capacity<T>(
    capacity: usize,
    resource: &'static str,
) -> Result<Vec<T>, SegmentError> {
    let requested_bytes = capacity
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .unwrap_or(u64::MAX);
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| SegmentError::LimitExceeded {
            resource,
            actual: requested_bytes,
            limit: usize_to_u64(isize::MAX as usize).unwrap_or(u64::MAX),
        })?;
    Ok(output)
}

fn allocate_none_column(row_count: usize) -> Result<Vec<Option<i64>>, SegmentError> {
    let mut output = try_vec_with_capacity(row_count, "integer block allocation")?;
    output.resize(row_count, None);
    Ok(output)
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn pack_bits(values: &[u64], bit_width: u8) -> Vec<u8> {
    if bit_width == 0 {
        return Vec::new();
    }
    let capacity = values
        .len()
        .checked_mul(usize::from(bit_width))
        .and_then(|bits| bits.checked_add(7))
        .map_or(0, |bits| bits / 8);
    let mut output = Vec::with_capacity(capacity);
    let mut buffer = 0_u128;
    let mut buffered_bits = 0_u32;
    for &value in values {
        buffer |= u128::from(value) << buffered_bits;
        buffered_bits += u32::from(bit_width);
        while buffered_bits >= 8 {
            output.push(buffer as u8);
            buffer >>= 8;
            buffered_bits -= 8;
        }
    }
    if buffered_bits != 0 {
        output.push(buffer as u8);
    }
    output
}

struct PackedBitReader<'a> {
    bytes: std::slice::Iter<'a, u8>,
    buffer: u128,
    buffered_bits: u32,
}

impl<'a> PackedBitReader<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self {
            bytes: payload.iter(),
            buffer: 0,
            buffered_bits: 0,
        }
    }

    fn read(&mut self, bit_width: u8) -> Result<u64, SegmentError> {
        if bit_width == 0 {
            return Ok(0);
        }
        while self.buffered_bits < u32::from(bit_width) {
            let byte = self.bytes.next().ok_or_else(|| {
                SegmentError::Corrupt("truncated bit-packed integer payload".into())
            })?;
            self.buffer |= u128::from(*byte) << self.buffered_bits;
            self.buffered_bits += 8;
        }
        let mask = if bit_width == 64 {
            u128::from(u64::MAX)
        } else {
            (1_u128 << bit_width) - 1
        };
        let value = (self.buffer & mask) as u64;
        self.buffer >>= bit_width;
        self.buffered_bits -= u32::from(bit_width);
        Ok(value)
    }
}

#[cfg(test)]
fn unpack_bits(payload: &[u8], count: usize, bit_width: u8) -> Result<Vec<u64>, SegmentError> {
    let mut reader = PackedBitReader::new(payload);
    let mut output = Vec::with_capacity(count);
    for _ in 0..count {
        output.push(reader.read(bit_width)?);
    }
    Ok(output)
}

fn packed_len(count: usize, bit_width: u8) -> Result<usize, SegmentError> {
    count
        .checked_mul(usize::from(bit_width))
        .and_then(|bits| bits.checked_add(7))
        .map(|bits| bits / 8)
        .ok_or_else(|| SegmentError::Corrupt("bit-packed length overflow".into()))
}

fn validate_packed_padding(
    payload: &[u8],
    count: usize,
    bit_width: u8,
) -> Result<(), SegmentError> {
    let used_bits = count
        .checked_mul(usize::from(bit_width))
        .ok_or_else(|| SegmentError::Corrupt("bit-packed length overflow".into()))?;
    let remainder = used_bits % 8;
    if remainder != 0
        && payload
            .last()
            .is_some_and(|byte| byte & !((1_u8 << remainder) - 1) != 0)
    {
        return Err(SegmentError::Corrupt(
            "non-zero padding in bit-packed integers".into(),
        ));
    }
    Ok(())
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn bitmap_len(row_count: usize) -> Result<usize, SegmentError> {
    row_count
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or_else(|| SegmentError::Corrupt("bitmap length overflow".into()))
}

fn set_bit(bitmap: &mut [u8], index: usize) {
    bitmap[index / 8] |= 1 << (index % 8);
}

fn bit_is_set(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (1 << (index % 8)) != 0
}

fn count_set_bits(bitmap: &[u8], row_count: usize) -> usize {
    (0..row_count)
        .filter(|index| bit_is_set(bitmap, *index))
        .count()
}

fn validate_bitmap_padding(bitmap: &[u8], row_count: usize) -> Result<(), SegmentError> {
    let remainder = row_count % 8;
    if remainder != 0
        && bitmap
            .last()
            .is_some_and(|byte| byte & !((1_u8 << remainder) - 1) != 0)
    {
        return Err(SegmentError::Corrupt(
            "non-zero padding in validity bitmap".into(),
        ));
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0_u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut crc = index as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xedb8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
            *entry = crc;
        }
        table
    });
    let mut crc = u32::MAX;
    for byte in bytes {
        crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn checksum_with_zeroed_header_field(header: &[u8]) -> u32 {
    let mut copy = header.to_vec();
    if copy.len() >= HEADER_CHECKSUM_OFFSET + 4 {
        copy[HEADER_CHECKSUM_OFFSET..HEADER_CHECKSUM_OFFSET + 4].fill(0);
    }
    crc32(&copy)
}

fn enforce_limit(resource: &'static str, actual: u64, limit: u64) -> Result<(), SegmentError> {
    if actual > limit {
        Err(SegmentError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn split_at_checked<'a>(
    bytes: &'a [u8],
    offset: usize,
    context: &str,
) -> Result<(&'a [u8], &'a [u8]), SegmentError> {
    if offset > bytes.len() {
        Err(SegmentError::Corrupt(format!("truncated {context}")))
    } else {
        Ok(bytes.split_at(offset))
    }
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Result<u16, SegmentError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| SegmentError::Corrupt("truncated fixed header".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, SegmentError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| SegmentError::Corrupt("truncated fixed header".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, SegmentError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or_else(|| SegmentError::Corrupt("truncated fixed header".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn usize_to_u32(value: usize, resource: &'static str) -> Result<u32, SegmentError> {
    u32::try_from(value).map_err(|_| SegmentError::InvalidInput(format!("{resource} exceeds u32")))
}

fn usize_to_u64(value: usize) -> Result<u64, SegmentError> {
    u64::try_from(value).map_err(|_| SegmentError::InvalidInput("size exceeds u64".into()))
}

fn u32_to_usize(value: u32) -> Result<usize, SegmentError> {
    usize::try_from(value).map_err(|_| SegmentError::Corrupt("u32 does not fit usize".into()))
}

fn u64_to_usize(value: u64) -> Result<usize, SegmentError> {
    usize::try_from(value).map_err(|_| SegmentError::LimitExceeded {
        resource: "addressable allocation",
        actual: value,
        limit: usize::MAX as u64,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn read_bytes(&mut self, length: usize, context: &str) -> Result<&'a [u8], SegmentError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| SegmentError::Corrupt(format!("{context} length overflow")))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| SegmentError::Corrupt(format!("truncated {context}")))?;
        self.position = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, SegmentError> {
        Ok(self.read_bytes(1, "u8")?[0])
    }

    fn read_u16(&mut self) -> Result<u16, SegmentError> {
        Ok(u16::from_le_bytes(
            self.read_bytes(2, "u16")?.try_into().unwrap(),
        ))
    }

    fn read_u32(&mut self) -> Result<u32, SegmentError> {
        Ok(u32::from_le_bytes(
            self.read_bytes(4, "u32")?.try_into().unwrap(),
        ))
    }

    fn read_u64(&mut self) -> Result<u64, SegmentError> {
        Ok(u64::from_le_bytes(
            self.read_bytes(8, "u64")?.try_into().unwrap(),
        ))
    }

    fn read_i64(&mut self) -> Result<i64, SegmentError> {
        Ok(i64::from_le_bytes(
            self.read_bytes(8, "i64")?.try_into().unwrap(),
        ))
    }

    fn read_utf8(&mut self, length: usize, context: &str) -> Result<String, SegmentError> {
        String::from_utf8(self.read_bytes(length, context)?.to_vec())
            .map_err(|_| SegmentError::Corrupt(format!("{context} is not valid UTF-8")))
    }

    fn read_varint(&mut self) -> Result<u64, SegmentError> {
        let mut value = 0_u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.read_u8()?;
            if shift == 63 && byte > 1 {
                return Err(SegmentError::Corrupt("varint overflow".into()));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(SegmentError::Corrupt("unterminated varint".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_schema() -> Schema {
        Schema::new(vec![
            Field::new("number", DataType::Int64, true),
            Field::new("active", DataType::Bool, true),
            Field::new("label", DataType::String, true),
        ])
        .unwrap()
    }

    fn test_columns() -> Vec<Column> {
        vec![
            Column::Int64(vec![
                Some(10),
                Some(11),
                None,
                Some(50),
                Some(51),
                Some(52),
                Some(-1),
                None,
            ]),
            Column::Bool(vec![
                Some(true),
                Some(false),
                None,
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                None,
            ]),
            Column::String(vec![
                Some("account/alpha".into()),
                Some("account/alpine".into()),
                None,
                Some("account/bravo".into()),
                Some("account/charlie".into()),
                Some("account/delta".into()),
                Some("zulu".into()),
                None,
            ]),
        ]
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("rusthouse-{label}-{}-{unique}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[cfg(target_os = "macos")]
    fn install_inheritable_test_acl(path: &Path) {
        use exacl::{AclEntry, Flag, Perm};

        let acl = [AclEntry::allow_user(
            "nobody",
            Perm::READ | Perm::EXECUTE,
            Flag::FILE_INHERIT | Flag::DIRECTORY_INHERIT,
        )];
        exacl::setfacl(&[path], &acl, None).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn install_inheritable_test_acl(path: &Path) {
        use exacl::{AclEntry, Flag, Perm};

        let mut acl = exacl::getfacl(path, None).unwrap();
        acl.extend([
            AclEntry::allow_user("", Perm::READ | Perm::WRITE | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_user("nobody", Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_group("", Perm::empty(), Flag::DEFAULT),
            AclEntry::allow_mask(Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_other(Perm::empty(), Flag::DEFAULT),
        ]);
        exacl::setfacl(&[path], &acl, None).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn acl_allows_nobody(path: &Path) -> bool {
        exacl::getfacl(path, None)
            .unwrap()
            .iter()
            .any(|entry| entry.name.ends_with("nobody") && entry.perms.contains(exacl::Perm::READ))
    }

    #[cfg(windows)]
    fn dacl_is_protected(path: &Path) -> bool {
        use std::os::windows::io::AsRawHandle;
        use std::ptr;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetKernelObjectSecurity, GetSecurityDescriptorControl,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };

        let file = File::open(path).unwrap();
        let mut needed = 0_u32;
        unsafe {
            GetKernelObjectSecurity(
                file.as_raw_handle() as HANDLE,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        assert!(needed > 0);
        let mut descriptor = vec![0_u8; needed as usize];
        let descriptor_ptr: PSECURITY_DESCRIPTOR = descriptor.as_mut_ptr().cast();
        assert_ne!(
            unsafe {
                GetKernelObjectSecurity(
                    file.as_raw_handle() as HANDLE,
                    DACL_SECURITY_INFORMATION,
                    descriptor_ptr,
                    needed,
                    &mut needed,
                )
            },
            0
        );
        let mut control = 0_u16;
        let mut revision = 0_u32;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor_ptr, &mut control, &mut revision) },
            0
        );
        control & SE_DACL_PROTECTED != 0
    }

    #[test]
    fn round_trips_multiple_typed_blocks_and_statistics() {
        let schema = test_schema();
        let columns = test_columns();
        let bytes = encode_segment(&schema, &columns, &WriteOptions { rows_per_block: 3 }).unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();

        assert_eq!(segment.version(), FORMAT_VERSION);
        assert_eq!(segment.schema(), &schema);
        assert_eq!(segment.row_count(), 8);
        assert_eq!(segment.rows_per_block(), 3);
        assert_eq!(segment.read_all().unwrap(), columns);
        assert_eq!(
            segment.block_statistics(0).unwrap(),
            vec![
                BlockStatistics {
                    row_start: 0,
                    row_count: 3,
                    null_count: 1,
                    min: Some(ScalarValue::Int64(10)),
                    max: Some(ScalarValue::Int64(11)),
                },
                BlockStatistics {
                    row_start: 3,
                    row_count: 3,
                    null_count: 0,
                    min: Some(ScalarValue::Int64(50)),
                    max: Some(ScalarValue::Int64(52)),
                },
                BlockStatistics {
                    row_start: 6,
                    row_count: 2,
                    null_count: 1,
                    min: Some(ScalarValue::Int64(-1)),
                    max: Some(ScalarValue::Int64(-1)),
                },
            ]
        );
    }

    #[test]
    fn round_trips_empty_all_null_and_extreme_integer_blocks() {
        let empty_schema = Schema::new(vec![Field::new("value", DataType::Int64, true)]).unwrap();
        let empty = vec![Column::Int64(Vec::new())];
        let empty_bytes = encode_segment(&empty_schema, &empty, &WriteOptions::default()).unwrap();
        let empty_segment = Segment::from_bytes(empty_bytes, DecodeLimits::default()).unwrap();
        assert_eq!(empty_segment.row_count(), 0);
        assert_eq!(empty_segment.read_all().unwrap(), empty);

        let columns = vec![Column::Int64(vec![
            None,
            Some(i64::MIN),
            Some(i64::MAX),
            None,
        ])];
        let bytes = encode_segment(&empty_schema, &columns, &WriteOptions::default()).unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();
        assert_eq!(segment.blocks[0].encoding, Encoding::IntPlain);
        assert_eq!(segment.read_all().unwrap(), columns);

        let all_null = vec![Column::String(vec![None, None, None])];
        let string_schema = Schema::new(vec![Field::new("value", DataType::String, true)]).unwrap();
        let bytes = encode_segment(&string_schema, &all_null, &WriteOptions::default()).unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();
        assert_eq!(segment.read_all().unwrap(), all_null);
        assert_eq!(segment.blocks[0].stats.min, None);
        assert_eq!(segment.blocks[0].stats.null_count, 3);
    }

    #[test]
    fn uses_compact_physical_encodings() {
        let rows = 10_000_i64;
        let integers = (0..rows).map(|value| Some(1_000 + value)).collect();
        let booleans = (0..rows).map(|value| Some(value % 3 == 0)).collect();
        let strings = (0..rows)
            .map(|value| Some(format!("warehouse/customer/account/{value:08}")))
            .collect();
        let columns = vec![
            Column::Int64(integers),
            Column::Bool(booleans),
            Column::String(strings),
        ];
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("flag", DataType::Bool, false),
            Field::new("path", DataType::String, false),
        ])
        .unwrap();
        let bytes = encode_segment(
            &schema,
            &columns,
            &WriteOptions {
                rows_per_block: rows as usize,
            },
        )
        .unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();

        let integer = segment.block(0, 0);
        assert_eq!(integer.encoding, Encoding::IntDeltaBitPacked);
        assert!(u64::from(integer.stored_len) < integer.logical_len / 10);

        let boolean = segment.block(0, 1);
        assert_eq!(boolean.encoding, Encoding::BoolBitPacked);
        assert_eq!(boolean.stored_len, 2_500);

        let string = segment.block(0, 2);
        assert_eq!(string.encoding, Encoding::StringFrontCoded);
        assert!(u64::from(string.stored_len) < string.logical_len / 3);
        assert_eq!(segment.read_all().unwrap(), columns);
    }

    #[test]
    fn detects_header_and_payload_corruption() {
        let schema = test_schema();
        let columns = test_columns();
        let bytes = encode_segment(&schema, &columns, &WriteOptions::default()).unwrap();

        let mut unknown_version = bytes.clone();
        unknown_version[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            Segment::from_bytes(unknown_version, DecodeLimits::default()),
            Err(SegmentError::UnsupportedVersion(version)) if version == FORMAT_VERSION + 1
        ));

        let mut header_corruption = bytes.clone();
        header_corruption[HEADER_SIZE + 6] ^= 0x20;
        assert!(matches!(
            Segment::from_bytes(header_corruption, DecodeLimits::default()),
            Err(SegmentError::ChecksumMismatch { location, .. }) if location == "header"
        ));

        let parsed = Segment::from_bytes(bytes.clone(), DecodeLimits::default()).unwrap();
        let first_payload = parsed.blocks[0].offset as usize;
        let mut payload_corruption = bytes;
        payload_corruption[first_payload] ^= 0x80;
        assert!(matches!(
            Segment::from_bytes(payload_corruption, DecodeLimits::default()),
            Err(SegmentError::ChecksumMismatch { location, .. })
                if location == "column 0 row group 0"
        ));
    }

    #[test]
    fn rejects_semantically_incorrect_zone_maps_with_valid_checksums() {
        let schema = Schema::new(vec![Field::new("number", DataType::Int64, false)]).unwrap();
        let columns = vec![Column::Int64(vec![Some(1), Some(100)])];
        let mut bytes = encode_segment(&schema, &columns, &WriteOptions::default()).unwrap();

        let schema_len = 4 + 4 + schema.fields[0].name.len() + 1 + 1 + 2;
        let first_max = HEADER_SIZE + schema_len + 52 + 8;
        bytes[first_max..first_max + 8].copy_from_slice(&1_i64.to_le_bytes());
        let header_len = read_u32_at(&bytes, 12).unwrap() as usize;
        let checksum = checksum_with_zeroed_header_field(&bytes[..header_len]);
        bytes[HEADER_CHECKSUM_OFFSET..HEADER_CHECKSUM_OFFSET + 4]
            .copy_from_slice(&checksum.to_le_bytes());

        assert!(matches!(
            Segment::from_bytes(bytes, DecodeLimits::default()),
            Err(SegmentError::Corrupt(message)) if message.contains("zone map does not match")
        ));
    }

    #[test]
    fn rejects_segments_before_metadata_driven_allocations_exceed_limits() {
        let schema = test_schema();
        let columns = test_columns();
        let bytes = encode_segment(&schema, &columns, &WriteOptions::default()).unwrap();

        let limits = DecodeLimits {
            max_file_bytes: bytes.len() as u64 - 1,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            Segment::from_bytes(bytes.clone(), limits),
            Err(SegmentError::LimitExceeded {
                resource: "file size",
                ..
            })
        ));

        let limits = DecodeLimits {
            max_string_bytes: 8,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            Segment::from_bytes(bytes.clone(), limits),
            Err(SegmentError::LimitExceeded {
                resource: "string statistic size",
                ..
            })
        ));

        let limits = DecodeLimits {
            max_rows_per_block: 4,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            Segment::from_bytes(bytes, limits),
            Err(SegmentError::LimitExceeded {
                resource: "rows per block",
                ..
            })
        ));

        let limits = DecodeLimits {
            max_blocks: 1,
            ..DecodeLimits::default()
        };
        let bytes = encode_segment(&schema, &columns, &WriteOptions::default()).unwrap();
        assert!(matches!(
            Segment::from_bytes(bytes, limits),
            Err(SegmentError::LimitExceeded {
                resource: "block count",
                ..
            })
        ));
    }

    #[test]
    fn decoded_block_limit_matches_streaming_allocation_boundaries() {
        let integer_schema = Schema::new(vec![Field::new("value", DataType::Int64, true)]).unwrap();
        let integers = vec![Column::Int64(vec![
            Some(10),
            Some(11),
            None,
            Some(12),
            Some(13),
        ])];
        let integer_bytes =
            encode_segment(&integer_schema, &integers, &WriteOptions::default()).unwrap();
        let integer_peak = decoded_allocation_bound(DataType::Int64, 5, 32).unwrap();
        Segment::from_bytes(
            integer_bytes.clone(),
            DecodeLimits {
                max_decoded_block_bytes: integer_peak,
                ..DecodeLimits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            Segment::from_bytes(
                integer_bytes,
                DecodeLimits {
                    max_decoded_block_bytes: integer_peak - 1,
                    ..DecodeLimits::default()
                }
            ),
            Err(SegmentError::LimitExceeded {
                resource: "decoded block size",
                actual,
                limit,
            }) if actual == integer_peak && limit == integer_peak - 1
        ));

        let string_schema = Schema::new(vec![Field::new("value", DataType::String, true)]).unwrap();
        let strings = vec![Column::String(vec![
            Some("account/alpha".into()),
            None,
            Some("account/alpine".into()),
        ])];
        let logical_string_bytes = "account/alpha".len() + "account/alpine".len();
        let string_bytes =
            encode_segment(&string_schema, &strings, &WriteOptions::default()).unwrap();
        let string_peak =
            decoded_allocation_bound(DataType::String, 3, logical_string_bytes as u64).unwrap();
        Segment::from_bytes(
            string_bytes.clone(),
            DecodeLimits {
                max_decoded_block_bytes: string_peak,
                ..DecodeLimits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            Segment::from_bytes(
                string_bytes,
                DecodeLimits {
                    max_decoded_block_bytes: string_peak - 1,
                    ..DecodeLimits::default()
                }
            ),
            Err(SegmentError::LimitExceeded {
                resource: "decoded block size",
                actual,
                limit,
            }) if actual == string_peak && limit == string_peak - 1
        ));
    }

    #[test]
    fn cumulative_decode_limit_bounds_columns_scans_and_complete_reads() {
        const ROWS: usize = 2_048;
        let repeated = "warehouse/customer/account/00000000".repeat(32);
        let strings = vec![Column::String(
            (0..ROWS).map(|_| Some(repeated.clone())).collect(),
        )];
        let string_schema =
            Schema::new(vec![Field::new("value", DataType::String, false)]).unwrap();
        let bytes = encode_segment(
            &string_schema,
            &strings,
            &WriteOptions {
                rows_per_block: 128,
            },
        )
        .unwrap();
        let result_bytes = decoded_allocation_bound(
            DataType::String,
            ROWS as u32,
            (repeated.len() * ROWS) as u64,
        )
        .unwrap();
        assert!((bytes.len() as u64) < result_bytes);
        let limited = Segment::from_bytes(
            bytes,
            DecodeLimits {
                max_decoded_result_bytes: result_bytes - 1,
                ..DecodeLimits::default()
            },
        )
        .unwrap();
        for result in [
            limited.read_column(0).map(|_| ()),
            limited.scan(&[0], None).map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(SegmentError::LimitExceeded {
                    resource: "decoded result size",
                    actual,
                    limit,
                }) if actual == result_bytes && limit == result_bytes - 1
            ));
        }

        let int_schema = Schema::new(vec![
            Field::new("left", DataType::Int64, false),
            Field::new("right", DataType::Int64, false),
        ])
        .unwrap();
        let integers = vec![
            Column::Int64(vec![Some(1), Some(2), Some(3), Some(4)]),
            Column::Int64(vec![Some(5), Some(6), Some(7), Some(8)]),
        ];
        let one_column_bytes = decoded_allocation_bound(DataType::Int64, 4, 32).unwrap();
        let segment = Segment::from_bytes(
            encode_segment(&int_schema, &integers, &WriteOptions::default()).unwrap(),
            DecodeLimits {
                max_decoded_result_bytes: one_column_bytes,
                ..DecodeLimits::default()
            },
        )
        .unwrap();
        assert_eq!(segment.read_column(0).unwrap(), integers[0]);
        assert!(matches!(
            segment.read_all(),
            Err(SegmentError::LimitExceeded {
                resource: "decoded result size",
                actual,
                limit,
            }) if actual == one_column_bytes * 2 && limit == one_column_bytes
        ));
    }

    #[test]
    fn verified_zone_maps_prune_unneeded_blocks() {
        let schema = Schema::new(vec![
            Field::new("number", DataType::Int64, false),
            Field::new("label", DataType::String, false),
        ])
        .unwrap();
        let columns = vec![
            Column::Int64(
                [1, 2, 3, 4, 80, 101, 102, 103]
                    .into_iter()
                    .map(Some)
                    .collect(),
            ),
            Column::String(
                ["a", "b", "c", "d", "e", "f", "g", "h"]
                    .into_iter()
                    .map(|value| Some(value.into()))
                    .collect(),
            ),
        ];
        let bytes = encode_segment(&schema, &columns, &WriteOptions { rows_per_block: 4 }).unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();

        let result = segment
            .scan(
                &[1],
                Some(&Predicate::Compare {
                    column: 0,
                    op: ComparisonOp::Gt,
                    value: ScalarValue::Int64(100),
                }),
            )
            .unwrap();
        assert_eq!(
            result.columns,
            vec![Column::String(vec![
                Some("f".into()),
                Some("g".into()),
                Some("h".into()),
            ])]
        );
        assert_eq!(result.row_count, 3);
        assert_eq!(
            result.metrics,
            ScanMetrics {
                row_groups_considered: 2,
                row_groups_pruned: 1,
                column_blocks_decoded: 2,
            }
        );
    }

    #[test]
    fn null_predicates_use_zone_maps_and_sql_comparison_semantics() {
        let schema = Schema::new(vec![Field::new("value", DataType::Int64, true)]).unwrap();
        let columns = vec![Column::Int64(vec![
            None,
            None,
            Some(3),
            Some(4),
            Some(5),
            None,
        ])];
        let bytes = encode_segment(&schema, &columns, &WriteOptions { rows_per_block: 2 }).unwrap();
        let segment = Segment::from_bytes(bytes, DecodeLimits::default()).unwrap();

        let comparison = segment
            .scan(
                &[0],
                Some(&Predicate::Compare {
                    column: 0,
                    op: ComparisonOp::GreaterOrEq,
                    value: ScalarValue::Int64(4),
                }),
            )
            .unwrap();
        assert_eq!(
            comparison.columns,
            vec![Column::Int64(vec![Some(4), Some(5)])]
        );
        assert_eq!(comparison.metrics.row_groups_pruned, 1);

        let nulls = segment
            .scan(&[0], Some(&Predicate::IsNull { column: 0 }))
            .unwrap();
        assert_eq!(nulls.columns, vec![Column::Int64(vec![None, None, None])]);
        assert_eq!(nulls.metrics.row_groups_pruned, 1);
    }

    #[test]
    fn immutable_file_write_refuses_replacement_and_reopens() {
        let directory = test_directory("immutable");
        let path = directory.join("segment.rhs");
        let schema = test_schema();
        let columns = test_columns();

        assert_eq!(
            write_segment(&path, &schema, &columns, &WriteOptions::default()).unwrap(),
            SegmentWriteOutcome::Durable
        );
        let reopened = Segment::open(&path, DecodeLimits::default()).unwrap();
        assert_eq!(reopened.read_all().unwrap(), columns);

        let mut replacement = test_columns();
        let Column::Int64(values) = &mut replacement[0] else {
            unreachable!()
        };
        values[0] = Some(999);
        assert!(matches!(
            write_segment(&path, &schema, &replacement, &WriteOptions::default()),
            Err(SegmentError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        let reopened = Segment::open(&path, DecodeLimits::default()).unwrap();
        assert_eq!(reopened.read_all().unwrap(), columns);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn immutable_segment_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = test_directory("private-segment");
        let path = directory.join("segment.rhs");
        write_segment(
            &path,
            &test_schema(),
            &test_columns(),
            &WriteOptions::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn inherited_acl_does_not_reach_segment_candidate_or_final_file() {
        let directory = test_directory("private-segment-acl");
        install_inheritable_test_acl(&directory);
        let probe = directory.join("probe");
        std::fs::write(&probe, b"probe").unwrap();
        assert!(acl_allows_nobody(&probe));
        std::fs::remove_file(probe).unwrap();

        let path = directory.join("segment.rhs");
        write_segment(
            &path,
            &test_schema(),
            &test_columns(),
            &WriteOptions::default(),
        )
        .unwrap();
        assert!(!acl_allows_nobody(&path));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn security_setup_failure_removes_created_segment_candidate() {
        use rustix::fs::{Mode, OFlags};

        let directory = test_directory("segment-security-failure");
        let parent_dir = File::open(&directory).unwrap();
        let error =
            create_temporary_file_at_with(&parent_dir, OsStr::new("segment.rhs"), |staging| {
                let candidate = rustix::fs::openat(
                    staging,
                    "segment",
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .unwrap();
                drop(candidate);
                Err(crate::catalog::SnapshotError::InjectedFailure(
                    "segment security setup",
                ))
            })
            .unwrap_err();

        assert!(error.to_string().contains("segment security setup"));
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn immutable_segment_file_has_a_protected_dacl() {
        let directory = test_directory("private-segment-dacl");
        let path = directory.join("segment.rhs");
        write_segment(
            &path,
            &test_schema(),
            &test_columns(),
            &WriteOptions::default(),
        )
        .unwrap();
        assert!(dacl_is_protected(&path));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn post_publication_failures_return_an_uncertain_outcome() {
        let directory = test_directory("publication-uncertainty");
        let bytes =
            encode_segment(&test_schema(), &test_columns(), &WriteOptions::default()).unwrap();

        for (index, failure, expected_message) in [
            (
                0,
                PublicationFailure::TemporaryCleanup,
                "temporary cleanup failed",
            ),
            (
                1,
                PublicationFailure::DirectorySync,
                "directory sync failed",
            ),
        ] {
            let path = directory.join(format!("segment-{index}.rhs"));
            let outcome = publish_segment_bytes_with_failure(&path, &bytes, failure).unwrap();
            assert!(matches!(
                outcome,
                SegmentWriteOutcome::PublishedUncertain { message }
                    if message.contains(expected_message)
            ));
            assert!(Segment::open(&path, DecodeLimits::default()).is_ok());
            assert!(matches!(
                publish_segment_bytes(&path, &bytes, || {}),
                Err(SegmentError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
            ));
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parent_replacement_before_sync_uses_the_pinned_directory() {
        use std::sync::{Arc, Barrier};

        let active_parent = test_directory("replace-parent-active");
        let moved_parent = active_parent.with_file_name(format!(
            "rusthouse-replace-parent-moved-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let path = active_parent.join("segment.rhs");
        let bytes =
            encode_segment(&test_schema(), &test_columns(), &WriteOptions::default()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let replacement_barrier = Arc::clone(&barrier);
        let replacement_active = active_parent.clone();
        let replacement_moved = moved_parent.clone();
        let replacer = thread::spawn(move || {
            replacement_barrier.wait();
            std::fs::rename(&replacement_active, &replacement_moved).unwrap();
            std::fs::create_dir(&replacement_active).unwrap();
            replacement_barrier.wait();
        });

        let outcome = publish_segment_bytes_with_hooks(
            &path,
            &bytes,
            || {},
            || {
                barrier.wait();
                barrier.wait();
            },
        )
        .unwrap();
        replacer.join().unwrap();

        assert_eq!(outcome, SegmentWriteOutcome::Durable);
        assert!(!path.exists());
        let published = moved_parent.join("segment.rhs");
        assert_eq!(
            Segment::open(&published, DecodeLimits::default())
                .unwrap()
                .read_all()
                .unwrap(),
            test_columns()
        );
        assert_eq!(std::fs::read_dir(&active_parent).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&moved_parent).unwrap().count(), 1);
        std::fs::remove_dir_all(active_parent).unwrap();
        std::fs::remove_dir_all(moved_parent).unwrap();
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn segment_publication_rejects_unsupported_acl_platform_before_creation() {
        let directory = test_directory("unsupported-segment");
        let path = directory.join("segment.rhs");
        assert!(matches!(
            write_segment(
                &path,
                &test_schema(),
                &test_columns(),
                &WriteOptions::default(),
            ),
            Err(SegmentError::UnsupportedPlatform(_))
        ));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_write_through_publication_returns_success_after_commit() {
        let directory = test_directory("windows-publication");
        let path = directory.join("segment.rhs");
        let schema = test_schema();
        let columns = test_columns();

        write_segment(&path, &schema, &columns, &WriteOptions::default()).unwrap();
        assert_eq!(
            Segment::open(&path, DecodeLimits::default())
                .unwrap()
                .read_all()
                .unwrap(),
            columns
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publication_hides_the_final_path_until_the_synced_file_is_complete() {
        let directory = test_directory("publication");
        let path = directory.join("segment.rhs");
        let schema = test_schema();
        let columns = test_columns();
        let bytes = encode_segment(&schema, &columns, &WriteOptions::default()).unwrap();
        let writer_path = path.clone();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (publish_sender, publish_receiver) = mpsc::channel();

        let writer = thread::spawn(move || {
            publish_segment_bytes(&writer_path, &bytes, || {
                ready_sender.send(()).unwrap();
                publish_receiver.recv().unwrap();
            })
        });
        ready_receiver.recv().unwrap();

        assert!(!path.exists());
        assert!(matches!(
            Segment::open(&path, DecodeLimits::default()),
            Err(SegmentError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        publish_sender.send(()).unwrap();
        writer.join().unwrap().unwrap();
        let segment = Segment::open(&path, DecodeLimits::default()).unwrap();
        assert_eq!(segment.read_all().unwrap(), columns);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bit_packing_round_trips_every_width_and_rejects_padding_bits() {
        for width in 0..=64 {
            let maximum = if width == 64 {
                u64::MAX
            } else if width == 0 {
                0
            } else {
                (1_u64 << width) - 1
            };
            let values = [0, maximum / 3, maximum / 2, maximum];
            let packed = pack_bits(&values, width);
            assert_eq!(unpack_bits(&packed, values.len(), width).unwrap(), values);
            validate_packed_padding(&packed, values.len(), width).unwrap();
        }

        let mut packed = pack_bits(&[1], 1);
        packed[0] |= 0x80;
        assert!(validate_packed_padding(&packed, 1, 1).is_err());
    }
}
