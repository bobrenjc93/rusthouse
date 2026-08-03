//! Persistent snapshots for one [`Table`].
//!
//! This module layers a versioned, columnar table payload over the atomic byte
//! envelope in [`crate::snapshot`]. It intentionally stores exactly one table:
//! catalogs, write-ahead logs, and multi-table snapshots are separate concerns.
//!
//! # Payload format
//!
//! All integers and floating-point bit patterns use little-endian byte order.
//! Version 1 starts with this fixed header:
//!
//! | Size | Field |
//! | ---: | --- |
//! | 8 | [`TABLE_PAYLOAD_MAGIC`] |
//! | 2 | [`TABLE_PAYLOAD_VERSION`] as a `u16` |
//! | 8 | row limit as a `u64` |
//! | 8 | row count as a `u64` |
//! | 8 | schema field count as a `u64` |
//!
//! Each schema field is a one-byte type tag, a `u64` name length, and UTF-8
//! name bytes. The schema is followed by a `u64` physical column count. Each
//! column is a one-byte type tag, a `u64` value count, and its values. `Int64`
//! and `Float64` values occupy eight bytes, `Bool` values are one byte (`0` or
//! `1`), and each `String` is a `u64` byte length followed by UTF-8 bytes.
//! Type tags are `1` for `Int64`, `2` for `Float64`, `3` for `Bool`, and `4`
//! for `String`.

use crate::snapshot::{SnapshotError, SnapshotStore};
use crate::storage::{Column, DataType, Field, Table, TableError, validate_fields};
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str;

/// Identifies a single-table payload inside a snapshot envelope.
pub const TABLE_PAYLOAD_MAGIC: [u8; 8] = *b"RHTABLE\0";

/// The only single-table payload version currently supported.
pub const TABLE_PAYLOAD_VERSION: u16 = 1;

const TYPE_INT64: u8 = 1;
const TYPE_FLOAT64: u8 = 2;
const TYPE_BOOL: u8 = 3;
const TYPE_STRING: u8 = 4;

/// Identifies where invalid or unallocatable data occurred in a table payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSnapshotLocation {
    /// The complete table payload.
    Payload,
    /// The fixed table header.
    Header,
    /// The schema collection.
    Schema,
    /// A schema field and its name.
    Field { field: u64 },
    /// A physical data column.
    Column { column: u64 },
    /// A string cell in a physical data column.
    StringValue { column: u64, row: u64 },
}

impl fmt::Display for TableSnapshotLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload => formatter.write_str("table payload"),
            Self::Header => formatter.write_str("table header"),
            Self::Schema => formatter.write_str("table schema"),
            Self::Field { field } => write!(formatter, "schema field {field}"),
            Self::Column { column } => write!(formatter, "data column {column}"),
            Self::StringValue { column, row } => {
                write!(formatter, "string value at column {column}, row {row}")
            }
        }
    }
}

/// A typed failure while encoding, writing, reading, or decoding one table.
#[derive(Debug)]
pub enum TableSnapshotError {
    /// The atomic snapshot envelope could not be written or read.
    Envelope(SnapshotError),
    /// The payload is not a single-table payload.
    InvalidMagic { found: [u8; 8] },
    /// The table payload uses a version this crate cannot read.
    UnsupportedVersion { found: u16, supported: u16 },
    /// A schema or physical column uses an unknown type tag.
    InvalidTypeTag {
        location: TableSnapshotLocation,
        tag: u8,
    },
    /// A declared count or byte length exceeds its validated bound.
    InvalidLength {
        location: TableSnapshotLocation,
        declared: u64,
        maximum: u64,
    },
    /// A field name or string value is not valid UTF-8.
    InvalidUtf8 {
        location: TableSnapshotLocation,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    /// The stored row count exceeds the stored row limit.
    RowCountExceedsLimit { row_count: u64, row_limit: u64 },
    /// The physical column count differs from the schema field count.
    ColumnCountMismatch { expected: u64, actual: u64 },
    /// A physical column's type differs from its schema field.
    ColumnTypeMismatch {
        column: u64,
        expected: DataType,
        actual: DataType,
    },
    /// A physical column's value count differs from the table row count.
    ColumnLengthMismatch {
        column: u64,
        expected: u64,
        actual: u64,
    },
    /// A Boolean column contained a byte other than `0` or `1`.
    InvalidBooleanValue { column: u64, row: u64, value: u8 },
    /// The decoded field names do not form a valid [`Table`] schema.
    InvalidSchema(TableError),
    /// Bytes follow the final physical column.
    TrailingData { remaining: u64 },
    /// Memory could not be reserved within a validated payload bound.
    AllocationFailed {
        location: TableSnapshotLocation,
        requested: u64,
    },
}

impl fmt::Display for TableSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => error.fmt(formatter),
            Self::InvalidMagic { found } => {
                write!(formatter, "invalid table payload magic {found:02x?}")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported table payload version {found}; this build supports version {supported}"
            ),
            Self::InvalidTypeTag { location, tag } => {
                write!(formatter, "invalid type tag {tag} in {location}")
            }
            Self::InvalidLength {
                location,
                declared,
                maximum,
            } => write!(
                formatter,
                "invalid length {declared} in {location}; maximum is {maximum}"
            ),
            Self::InvalidUtf8 {
                location,
                valid_up_to,
                ..
            } => write!(
                formatter,
                "invalid UTF-8 in {location} after byte {valid_up_to}"
            ),
            Self::RowCountExceedsLimit {
                row_count,
                row_limit,
            } => write!(
                formatter,
                "table row count {row_count} exceeds row limit {row_limit}"
            ),
            Self::ColumnCountMismatch { expected, actual } => write!(
                formatter,
                "table has {actual} physical columns; schema requires {expected}"
            ),
            Self::ColumnTypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "data column {column} has type {actual}; schema requires {expected}"
            ),
            Self::ColumnLengthMismatch {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "data column {column} has {actual} values; table row count is {expected}"
            ),
            Self::InvalidBooleanValue { column, row, value } => write!(
                formatter,
                "Boolean value at column {column}, row {row} is {value}; expected 0 or 1"
            ),
            Self::InvalidSchema(error) => write!(formatter, "invalid snapshot schema: {error}"),
            Self::TrailingData { remaining } => {
                write!(formatter, "table payload has {remaining} trailing bytes")
            }
            Self::AllocationFailed {
                location,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} entries or bytes for {location}"
            ),
        }
    }
}

impl Error for TableSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            Self::InvalidSchema(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SnapshotError> for TableSnapshotError {
    fn from(error: SnapshotError) -> Self {
        Self::Envelope(error)
    }
}

impl SnapshotStore {
    /// Encodes `table` and atomically replaces the snapshot at `path`.
    ///
    /// The table payload is subject to this store's payload size bound. No file
    /// is created or replaced if encoding or allocation fails.
    pub fn write_table(
        &self,
        path: impl AsRef<Path>,
        table: &Table,
    ) -> Result<(), TableSnapshotError> {
        let payload = encode_table(table, self.max_payload_len())?;
        self.write(path, &payload)
            .map_err(TableSnapshotError::Envelope)
    }

    /// Opens, validates, and reconstructs the single table at `path`.
    ///
    /// Envelope integrity is checked before the payload is decoded. All
    /// payload-derived allocations are bounded by the envelope size and use
    /// fallible reservation.
    pub fn read_table(&self, path: impl AsRef<Path>) -> Result<Table, TableSnapshotError> {
        let payload = self.read(path).map_err(TableSnapshotError::Envelope)?;
        decode_table(&payload)
    }

    /// Reconstructs a table from a primary snapshot or an explicit fallback.
    ///
    /// The fallback eligibility and independent payload bounds are those of
    /// [`SnapshotStore::read_with_fallback`]. The selected envelope is fully
    /// validated before its table payload is decoded.
    pub fn read_table_with_fallback(
        &self,
        primary_path: impl AsRef<Path>,
        fallback_path: impl AsRef<Path>,
    ) -> Result<Table, TableSnapshotError> {
        let payload = self
            .read_with_fallback(primary_path, fallback_path)
            .map_err(TableSnapshotError::Envelope)?;
        decode_table(&payload)
    }
}

fn encode_table(table: &Table, max_payload_len: usize) -> Result<Vec<u8>, TableSnapshotError> {
    let mut encoder = Encoder::new(max_payload_len);
    encoder.bytes(&TABLE_PAYLOAD_MAGIC)?;
    encoder.u16(TABLE_PAYLOAD_VERSION)?;
    encoder.u64(table.row_limit() as u64)?;
    encoder.u64(table.len() as u64)?;
    encoder.u64(table.fields().len() as u64)?;

    for field in table.fields() {
        encoder.u8(type_tag(field.data_type()))?;
        encoder.string(field.name())?;
    }

    encoder.u64(table.columns().len() as u64)?;
    for column in table.columns() {
        encoder.u8(type_tag(column.data_type()))?;
        encoder.u64(column.len() as u64)?;
        match column {
            Column::Int64(values) => {
                for value in values {
                    encoder.bytes(&value.to_le_bytes())?;
                }
            }
            Column::Float64(values) => {
                for value in values {
                    encoder.bytes(&value.to_bits().to_le_bytes())?;
                }
            }
            Column::Bool(values) => encoder.bytes(values)?,
            Column::String(values) => {
                for value in values {
                    encoder.string(value)?;
                }
            }
        }
    }

    Ok(encoder.finish())
}

fn decode_table(payload: &[u8]) -> Result<Table, TableSnapshotError> {
    let mut decoder = Decoder::new(payload);
    let found_magic = decoder.array::<8>(TableSnapshotLocation::Header)?;
    if found_magic != TABLE_PAYLOAD_MAGIC {
        return Err(TableSnapshotError::InvalidMagic { found: found_magic });
    }

    let version = decoder.u16(TableSnapshotLocation::Header)?;
    if version != TABLE_PAYLOAD_VERSION {
        return Err(TableSnapshotError::UnsupportedVersion {
            found: version,
            supported: TABLE_PAYLOAD_VERSION,
        });
    }

    let row_limit_u64 = decoder.u64(TableSnapshotLocation::Header)?;
    let row_count_u64 = decoder.u64(TableSnapshotLocation::Header)?;
    if row_count_u64 > row_limit_u64 {
        return Err(TableSnapshotError::RowCountExceedsLimit {
            row_count: row_count_u64,
            row_limit: row_limit_u64,
        });
    }
    let row_limit = bounded_usize(row_limit_u64, TableSnapshotLocation::Header)?;
    let row_count = bounded_usize(row_count_u64, TableSnapshotLocation::Header)?;

    let field_count_u64 = decoder.u64(TableSnapshotLocation::Header)?;
    let maximum_fields = decoder.remaining().saturating_sub(8) / 9;
    if field_count_u64 > maximum_fields as u64 {
        return Err(TableSnapshotError::InvalidLength {
            location: TableSnapshotLocation::Schema,
            declared: field_count_u64,
            maximum: maximum_fields as u64,
        });
    }
    let field_count = bounded_usize(field_count_u64, TableSnapshotLocation::Schema)?;
    let mut fields = try_vec(field_count, TableSnapshotLocation::Schema)?;
    for field_index in 0..field_count_u64 {
        let location = TableSnapshotLocation::Field { field: field_index };
        let tag = decoder.u8(location)?;
        let data_type = data_type(tag, location)?;
        let name = decoder.string(location)?;
        fields.push(Field::new(name, data_type));
    }
    validate_fields(&fields).map_err(|error| match error {
        TableError::SchemaAllocationFailed { field_count } => {
            TableSnapshotError::AllocationFailed {
                location: TableSnapshotLocation::Schema,
                requested: field_count as u64,
            }
        }
        error => TableSnapshotError::InvalidSchema(error),
    })?;

    let column_count_u64 = decoder.u64(TableSnapshotLocation::Schema)?;
    if column_count_u64 != field_count_u64 {
        return Err(TableSnapshotError::ColumnCountMismatch {
            expected: field_count_u64,
            actual: column_count_u64,
        });
    }

    let mut columns = try_vec(field_count, TableSnapshotLocation::Payload)?;
    for (column_index, field) in (0_u64..).zip(&fields) {
        let location = TableSnapshotLocation::Column {
            column: column_index,
        };
        let tag = decoder.u8(location)?;
        let actual_type = data_type(tag, location)?;
        if actual_type != field.data_type() {
            return Err(TableSnapshotError::ColumnTypeMismatch {
                column: column_index,
                expected: field.data_type(),
                actual: actual_type,
            });
        }

        let column_len = decoder.u64(location)?;
        if column_len != row_count_u64 {
            return Err(TableSnapshotError::ColumnLengthMismatch {
                column: column_index,
                expected: row_count_u64,
                actual: column_len,
            });
        }
        columns.push(decode_column(
            &mut decoder,
            actual_type,
            row_count,
            column_index,
        )?);
    }

    if decoder.remaining() != 0 {
        return Err(TableSnapshotError::TrailingData {
            remaining: decoder.remaining() as u64,
        });
    }

    Ok(Table::from_snapshot_parts(
        fields, columns, row_count, row_limit,
    ))
}

fn decode_column(
    decoder: &mut Decoder<'_>,
    data_type: DataType,
    row_count: usize,
    column_index: u64,
) -> Result<Column, TableSnapshotError> {
    let location = TableSnapshotLocation::Column {
        column: column_index,
    };
    match data_type {
        DataType::Int64 => {
            let bytes = decoder.fixed_width_values(row_count, 8, location)?;
            let mut values = try_vec(row_count, location)?;
            for bytes in bytes.chunks_exact(8) {
                values.push(i64::from_le_bytes(
                    bytes.try_into().expect("integer value has a fixed width"),
                ));
            }
            Ok(Column::Int64(values))
        }
        DataType::Float64 => {
            let bytes = decoder.fixed_width_values(row_count, 8, location)?;
            let mut values = try_vec(row_count, location)?;
            for bytes in bytes.chunks_exact(8) {
                let bits =
                    u64::from_le_bytes(bytes.try_into().expect("float value has a fixed width"));
                values.push(f64::from_bits(bits));
            }
            Ok(Column::Float64(values))
        }
        DataType::Bool => {
            let bytes = decoder.fixed_width_values(row_count, 1, location)?;
            let mut values = try_vec(row_count, location)?;
            for (row, value) in (0_u64..).zip(bytes.iter().copied()) {
                if value > 1 {
                    return Err(TableSnapshotError::InvalidBooleanValue {
                        column: column_index,
                        row,
                        value,
                    });
                }
                values.push(value);
            }
            Ok(Column::Bool(values))
        }
        DataType::String => {
            if row_count > decoder.remaining() / 8 {
                return Err(TableSnapshotError::InvalidLength {
                    location,
                    declared: row_count as u64,
                    maximum: (decoder.remaining() / 8) as u64,
                });
            }
            let mut values = try_vec(row_count, location)?;
            for row in 0..row_count as u64 {
                values.push(decoder.string(TableSnapshotLocation::StringValue {
                    column: column_index,
                    row,
                })?);
            }
            Ok(Column::String(values))
        }
    }
}

fn type_tag(data_type: DataType) -> u8 {
    match data_type {
        DataType::Int64 => TYPE_INT64,
        DataType::Float64 => TYPE_FLOAT64,
        DataType::Bool => TYPE_BOOL,
        DataType::String => TYPE_STRING,
    }
}

fn data_type(tag: u8, location: TableSnapshotLocation) -> Result<DataType, TableSnapshotError> {
    match tag {
        TYPE_INT64 => Ok(DataType::Int64),
        TYPE_FLOAT64 => Ok(DataType::Float64),
        TYPE_BOOL => Ok(DataType::Bool),
        TYPE_STRING => Ok(DataType::String),
        _ => Err(TableSnapshotError::InvalidTypeTag { location, tag }),
    }
}

fn bounded_usize(value: u64, location: TableSnapshotLocation) -> Result<usize, TableSnapshotError> {
    usize::try_from(value).map_err(|_| TableSnapshotError::InvalidLength {
        location,
        declared: value,
        maximum: usize::MAX as u64,
    })
}

fn try_vec<T>(
    capacity: usize,
    location: TableSnapshotLocation,
) -> Result<Vec<T>, TableSnapshotError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| TableSnapshotError::AllocationFailed {
            location,
            requested: capacity as u64,
        })?;
    Ok(values)
}

struct Encoder {
    bytes: Vec<u8>,
    maximum: usize,
}

impl Encoder {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn u8(&mut self, value: u8) -> Result<(), TableSnapshotError> {
        self.bytes(&[value])
    }

    fn u16(&mut self, value: u16) -> Result<(), TableSnapshotError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), TableSnapshotError> {
        self.bytes(&value.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), TableSnapshotError> {
        self.u64(value.len() as u64)?;
        self.bytes(value.as_bytes())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), TableSnapshotError> {
        let required = self.bytes.len().checked_add(value.len()).ok_or({
            TableSnapshotError::Envelope(SnapshotError::Oversized {
                payload_len: u64::MAX,
                max_payload_len: self.maximum as u64,
            })
        })?;
        if required > self.maximum {
            return Err(TableSnapshotError::Envelope(SnapshotError::Oversized {
                payload_len: required as u64,
                max_payload_len: self.maximum as u64,
            }));
        }
        self.bytes.try_reserve_exact(value.len()).map_err(|_| {
            TableSnapshotError::AllocationFailed {
                location: TableSnapshotLocation::Payload,
                requested: required as u64,
            }
        })?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn u8(&mut self, location: TableSnapshotLocation) -> Result<u8, TableSnapshotError> {
        Ok(self.take(1, location)?[0])
    }

    fn u16(&mut self, location: TableSnapshotLocation) -> Result<u16, TableSnapshotError> {
        Ok(u16::from_le_bytes(self.array::<2>(location)?))
    }

    fn u64(&mut self, location: TableSnapshotLocation) -> Result<u64, TableSnapshotError> {
        Ok(u64::from_le_bytes(self.array::<8>(location)?))
    }

    fn array<const N: usize>(
        &mut self,
        location: TableSnapshotLocation,
    ) -> Result<[u8; N], TableSnapshotError> {
        Ok(self
            .take(N, location)?
            .try_into()
            .expect("decoder returned the requested fixed width"))
    }

    fn string(&mut self, location: TableSnapshotLocation) -> Result<String, TableSnapshotError> {
        let length = self.u64(location)?;
        let bytes = self.take_u64(length, location)?;
        let value = str::from_utf8(bytes).map_err(|error| TableSnapshotError::InvalidUtf8 {
            location,
            valid_up_to: error.valid_up_to(),
            error_len: error.error_len(),
        })?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| TableSnapshotError::AllocationFailed {
                location,
                requested: value.len() as u64,
            })?;
        owned.push_str(value);
        Ok(owned)
    }

    fn fixed_width_values(
        &mut self,
        count: usize,
        width: usize,
        location: TableSnapshotLocation,
    ) -> Result<&'a [u8], TableSnapshotError> {
        let maximum = self.remaining() / width;
        if count > maximum {
            return Err(TableSnapshotError::InvalidLength {
                location,
                declared: count as u64,
                maximum: maximum as u64,
            });
        }
        self.take(count * width, location)
    }

    fn take_u64(
        &mut self,
        length: u64,
        location: TableSnapshotLocation,
    ) -> Result<&'a [u8], TableSnapshotError> {
        let length = bounded_usize(length, location)?;
        self.take(length, location)
    }

    fn take(
        &mut self,
        length: usize,
        location: TableSnapshotLocation,
    ) -> Result<&'a [u8], TableSnapshotError> {
        if length > self.remaining() {
            return Err(TableSnapshotError::InvalidLength {
                location,
                declared: length as u64,
                maximum: self.remaining() as u64,
            });
        }
        let start = self.position;
        self.position += length;
        Ok(&self.bytes[start..self.position])
    }
}
