//! Transactional CSV ingestion for [`Table`].
//!
//! CSV input must start with a header whose names and order exactly match the
//! destination schema. Header fields are decoded and compared directly from
//! the bounded input without staging allocations. The complete data suffix is
//! parsed and converted before [`Table::insert_batch`] is called, so malformed
//! input never appends a partial batch.

use crate::{DataType, Field, Table, TableError, Value};
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::mem;
use std::str;

/// Default maximum size of one CSV import: 64 MiB.
pub const DEFAULT_CSV_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum number of data rows in one CSV import.
pub const DEFAULT_CSV_ROWS: usize = 1_000_000;

/// Default maximum decoded staging size of one CSV import: 64 MiB.
pub const DEFAULT_CSV_DECODED_BYTES: usize = 64 * 1024 * 1024;

const READ_BUFFER_BYTES: usize = 8 * 1024;
// Three empty ByteRecord boxes (manual byte/string headers and the caller
// record) plus the reader's other fixed heap state fit within this bound.
const PARSER_FIXED_BYTES: usize = 1024;

/// Resource limits for one CSV import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvIngestLimits {
    max_input_bytes: usize,
    max_rows: usize,
    max_decoded_bytes: usize,
}

impl CsvIngestLimits {
    /// Creates limits for raw input bytes and data rows, excluding the header.
    ///
    /// The decoded staging limit initially uses
    /// [`DEFAULT_CSV_DECODED_BYTES`] and can be changed with
    /// [`Self::with_max_decoded_bytes`].
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_rows: usize) -> Self {
        Self {
            max_input_bytes,
            max_rows,
            max_decoded_bytes: DEFAULT_CSV_DECODED_BYTES,
        }
    }

    /// Sets the maximum memory requested for decoded row staging.
    ///
    /// The accounting includes parser record buffers, outer row-vector
    /// capacity, every [`Value`] slot, owned string capacity, and the temporary
    /// row collection used by [`Table::insert_batch`]. It deliberately excludes
    /// the independently bounded raw input buffer and final table columns.
    #[must_use]
    pub const fn with_max_decoded_bytes(mut self, max_decoded_bytes: usize) -> Self {
        self.max_decoded_bytes = max_decoded_bytes;
        self
    }

    /// Returns the maximum number of raw CSV bytes that may be read.
    #[must_use]
    pub const fn max_input_bytes(self) -> usize {
        self.max_input_bytes
    }

    /// Returns the maximum number of data rows, excluding the header.
    #[must_use]
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Returns the maximum decoded staging size in bytes.
    #[must_use]
    pub const fn max_decoded_bytes(self) -> usize {
        self.max_decoded_bytes
    }
}

impl Default for CsvIngestLimits {
    fn default() -> Self {
        Self::new(DEFAULT_CSV_INPUT_BYTES, DEFAULT_CSV_ROWS)
    }
}

/// Identifies the allocation that failed during CSV ingestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvIngestAllocation {
    /// The bounded raw input buffer.
    InputBuffer,
    /// The outer collection of decoded rows.
    RowCollection,
    /// The typed values in one decoded row.
    RowValues,
    /// An owned string cell.
    StringValue,
}

impl fmt::Display for CsvIngestAllocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InputBuffer => "CSV input buffer",
            Self::RowCollection => "CSV row collection",
            Self::RowValues => "CSV row values",
            Self::StringValue => "CSV string value",
        };
        formatter.write_str(name)
    }
}

/// A typed failure from transactional CSV ingestion.
#[derive(Debug)]
pub enum CsvIngestError {
    /// Reading the caller-provided input failed.
    Read(io::Error),
    /// The input contains more bytes than the configured bound.
    ByteLimitExceeded {
        /// Configured maximum number of raw input bytes.
        limit: usize,
    },
    /// The input contains more data rows than the configured bound.
    RowLimitExceeded {
        /// Configured maximum number of data rows.
        limit: usize,
    },
    /// Decoded row staging would exceed its configured memory bound.
    DecodedLimitExceeded {
        /// Configured maximum decoded staging size.
        limit: usize,
        /// Minimum decoded staging size required at the rejected boundary.
        required: usize,
    },
    /// A fallible reservation for bounded ingestion storage failed.
    AllocationFailed {
        /// The allocation that could not be reserved.
        allocation: CsvIngestAllocation,
        /// Requested number of bytes for this reservation.
        requested: usize,
    },
    /// The CSV header does not exactly match the table schema.
    HeaderMismatch {
        /// Number of fields required by the table schema.
        expected_fields: usize,
        /// Number of fields supplied by the CSV header.
        actual_fields: usize,
        /// First differing position, or `None` when only widths differ.
        first_mismatch: Option<usize>,
    },
    /// A data row has a different width than the schema.
    RowWidthMismatch {
        /// Zero-based data-row position, excluding the header.
        row: usize,
        /// Number of fields required by the schema.
        expected: usize,
        /// Number of values supplied by the row.
        actual: usize,
    },
    /// A field cannot be converted to its schema type.
    InvalidValue {
        /// Zero-based data-row position, excluding the header.
        row: usize,
        /// Zero-based field position.
        column: usize,
        /// Type required by the schema.
        expected: DataType,
        /// Length of the rejected decoded field in bytes.
        value_bytes: usize,
    },
    /// The `csv` parser rejected malformed or non-UTF-8 input.
    Csv(csv::Error),
    /// The validated batch could not be committed to the table.
    Table(TableError),
}

impl fmt::Display for CsvIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read CSV input: {error}"),
            Self::ByteLimitExceeded { limit } => {
                write!(formatter, "CSV input exceeds the {limit}-byte limit")
            }
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "CSV input exceeds the {limit}-row limit")
            }
            Self::DecodedLimitExceeded { limit, required } => write!(
                formatter,
                "CSV decoded staging requires at least {required} bytes; limit is {limit}"
            ),
            Self::AllocationFailed {
                allocation,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} bytes for {allocation}"
            ),
            Self::HeaderMismatch {
                expected_fields,
                actual_fields,
                first_mismatch,
            } => {
                write!(
                    formatter,
                    "CSV header with {actual_fields} fields does not exactly match the {expected_fields}-field table schema"
                )?;
                if let Some(column) = first_mismatch {
                    write!(formatter, "; first mismatch is at column {column}")?;
                }
                Ok(())
            }
            Self::RowWidthMismatch {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "CSV data row {row} has {actual} values; expected {expected}"
            ),
            Self::InvalidValue {
                row,
                column,
                expected,
                value_bytes,
            } => write!(
                formatter,
                "CSV data row {row}, column {column} has a {value_bytes}-byte value that is not {expected}"
            ),
            Self::Csv(error) => write!(formatter, "invalid CSV input: {error}"),
            Self::Table(error) => write!(formatter, "could not insert CSV batch: {error}"),
        }
    }
}

impl Error for CsvIngestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Csv(error) => Some(error),
            Self::Table(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TableError> for CsvIngestError {
    fn from(error: TableError) -> Self {
        Self::Table(error)
    }
}

impl Table {
    /// Parses and transactionally inserts CSV using [`CsvIngestLimits::default`].
    ///
    /// The first record is required to be a header that exactly matches the
    /// table's case-sensitive field names and order. `Int64`, `Float64`, and
    /// `Bool` fields use Rust's strict textual parsers; string fields preserve
    /// the value decoded by the CSV parser.
    pub fn insert_csv<R: Read>(&mut self, input: R) -> Result<usize, CsvIngestError> {
        self.insert_csv_with_limits(input, CsvIngestLimits::default())
    }

    /// Parses and transactionally inserts CSV with explicit resource limits.
    ///
    /// No rows are inserted unless input reading, header validation, CSV
    /// parsing, type conversion, and table batch validation all succeed.
    pub fn insert_csv_with_limits<R: Read>(
        &mut self,
        input: R,
        limits: CsvIngestLimits,
    ) -> Result<usize, CsvIngestError> {
        let bytes = read_bounded(input, limits.max_input_bytes)?;
        let csv_bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let shape = scan_csv_shape(csv_bytes).ok_or(CsvIngestError::DecodedLimitExceeded {
            limit: limits.max_decoded_bytes,
            required: usize::MAX,
        })?;
        let header = &csv_bytes[shape.header_start..shape.header_end];
        if str::from_utf8(header).is_err() {
            return Err(CsvIngestError::Csv(invalid_utf8_csv_error()));
        }
        let first_mismatch = first_header_mismatch(header, self.fields());
        if shape.header_fields != self.fields().len() || first_mismatch.is_some() {
            return Err(CsvIngestError::HeaderMismatch {
                expected_fields: self.fields().len(),
                actual_fields: shape.header_fields,
                first_mismatch,
            });
        }
        if shape.data_rows > limits.max_rows {
            return Err(CsvIngestError::RowLimitExceeded {
                limit: limits.max_rows,
            });
        }
        if shape.data_rows == 0 {
            return self
                .insert_batch(Vec::<Vec<Value>>::new())
                .map_err(CsvIngestError::Table);
        }

        let fixed_value_bytes = self
            .fields()
            .len()
            .checked_mul(mem::size_of::<Value>())
            .ok_or(CsvIngestError::DecodedLimitExceeded {
                limit: limits.max_decoded_bytes,
                required: usize::MAX,
            })?;

        let minimum_data_bytes = shape
            .data_cells
            .checked_mul(mem::size_of::<Value>())
            .and_then(|value_bytes| {
                shape
                    .data_rows
                    .checked_mul(mem::size_of::<Vec<Value>>())
                    .and_then(|row_bytes| value_bytes.checked_add(row_bytes))
            })
            .ok_or(CsvIngestError::DecodedLimitExceeded {
                limit: limits.max_decoded_bytes,
                required: usize::MAX,
            })?;
        let parser_working_bytes = parser_working_bytes(shape, limits.max_decoded_bytes)?;
        let commit_collection_bytes = shape
            .data_rows
            .checked_mul(mem::size_of::<Vec<Value>>())
            .ok_or(CsvIngestError::DecodedLimitExceeded {
                limit: limits.max_decoded_bytes,
                required: usize::MAX,
            })?;
        ensure_decoded_limit(
            minimum_data_bytes,
            parser_working_bytes,
            limits.max_decoded_bytes,
        )?;
        ensure_decoded_limit(
            minimum_data_bytes,
            commit_collection_bytes,
            limits.max_decoded_bytes,
        )?;

        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .buffer_capacity(READ_BUFFER_BYTES)
            .from_reader(&csv_bytes[shape.data_start..]);
        // `csv` otherwise retains byte and string clones of the first data
        // record even when `has_headers(false)` is used.
        reader.set_byte_headers(csv::ByteRecord::new());

        let mut record = csv::StringRecord::new();
        let mut rows = Vec::new();
        let mut decoded_bytes = 0_usize;
        let mut row_index = 0_usize;
        while reader
            .read_record(&mut record)
            .map_err(CsvIngestError::Csv)?
        {
            if rows.len() == limits.max_rows {
                return Err(CsvIngestError::RowLimitExceeded {
                    limit: limits.max_rows,
                });
            }
            if record.len() != self.fields().len() {
                return Err(CsvIngestError::RowWidthMismatch {
                    row: row_index,
                    expected: self.fields().len(),
                    actual: record.len(),
                });
            }

            let requested_string_bytes = record
                .iter()
                .zip(self.fields())
                .filter(|(_, field)| field.data_type() == DataType::String)
                .try_fold(0_usize, |total, (raw, _)| total.checked_add(raw.len()))
                .ok_or(CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                })?;
            let requested_row_bytes = fixed_value_bytes
                .checked_add(requested_string_bytes)
                .ok_or(CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                })?;
            let planned_outer_bytes = planned_row_collection_growth(rows.len(), rows.capacity())
                .ok_or(CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                })?;
            ensure_decoded_limit(
                decoded_bytes,
                parser_working_bytes
                    .checked_add(requested_row_bytes)
                    .and_then(|bytes| bytes.checked_add(planned_outer_bytes))
                    .ok_or(CsvIngestError::DecodedLimitExceeded {
                        limit: limits.max_decoded_bytes,
                        required: usize::MAX,
                    })?,
                limits.max_decoded_bytes,
            )?;

            let mut row = Vec::new();
            row.try_reserve_exact(self.fields().len()).map_err(|_| {
                CsvIngestError::AllocationFailed {
                    allocation: CsvIngestAllocation::RowValues,
                    requested: fixed_value_bytes,
                }
            })?;
            let old_row_capacity = rows.capacity();
            reserve_row_slot(&mut rows)?;

            let outer_bytes = rows
                .capacity()
                .checked_sub(old_row_capacity)
                .and_then(|capacity| capacity.checked_mul(mem::size_of::<Vec<Value>>()))
                .ok_or(CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                })?;
            let value_bytes = row.capacity().checked_mul(mem::size_of::<Value>()).ok_or(
                CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                },
            )?;
            let base_row_bytes = outer_bytes.checked_add(value_bytes).ok_or(
                CsvIngestError::DecodedLimitExceeded {
                    limit: limits.max_decoded_bytes,
                    required: usize::MAX,
                },
            )?;
            ensure_decoded_limit(
                decoded_bytes,
                parser_working_bytes.checked_add(base_row_bytes).ok_or(
                    CsvIngestError::DecodedLimitExceeded {
                        limit: limits.max_decoded_bytes,
                        required: usize::MAX,
                    },
                )?,
                limits.max_decoded_bytes,
            )?;

            let mut actual_string_bytes = 0_usize;
            for (column, (raw, field)) in record.iter().zip(self.fields()).enumerate() {
                let value = convert_value(raw, field, row_index, column)?;
                if let Value::String(string) = &value {
                    actual_string_bytes = actual_string_bytes
                        .checked_add(string.capacity())
                        .ok_or(CsvIngestError::DecodedLimitExceeded {
                            limit: limits.max_decoded_bytes,
                            required: usize::MAX,
                        })?;
                    ensure_decoded_limit(
                        decoded_bytes,
                        parser_working_bytes
                            .checked_add(base_row_bytes)
                            .and_then(|bytes| bytes.checked_add(actual_string_bytes))
                            .ok_or(CsvIngestError::DecodedLimitExceeded {
                                limit: limits.max_decoded_bytes,
                                required: usize::MAX,
                            })?,
                        limits.max_decoded_bytes,
                    )?;
                }
                row.push(value);
            }

            decoded_bytes = ensure_decoded_limit(
                decoded_bytes,
                base_row_bytes.checked_add(actual_string_bytes).ok_or(
                    CsvIngestError::DecodedLimitExceeded {
                        limit: limits.max_decoded_bytes,
                        required: usize::MAX,
                    },
                )?,
                limits.max_decoded_bytes,
            )?;
            rows.push(row);
            row_index += 1;
            record.clear();
        }

        drop(record);
        drop(reader);
        let commit_collection_bytes = rows.len().checked_mul(mem::size_of::<Vec<Value>>()).ok_or(
            CsvIngestError::DecodedLimitExceeded {
                limit: limits.max_decoded_bytes,
                required: usize::MAX,
            },
        )?;
        ensure_decoded_limit(
            decoded_bytes,
            commit_collection_bytes,
            limits.max_decoded_bytes,
        )?;
        self.insert_batch(rows).map_err(CsvIngestError::Table)
    }
}

fn read_bounded<R: Read>(mut input: R, limit: usize) -> Result<Vec<u8>, CsvIngestError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let remaining = limit.saturating_sub(bytes.len());
        let read_len = remaining.saturating_add(1).min(buffer.len());
        let read = match input.read(&mut buffer[..read_len]) {
            Ok(0) => return Ok(bytes),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CsvIngestError::Read(error)),
        };
        if read > remaining {
            return Err(CsvIngestError::ByteLimitExceeded { limit });
        }
        reserve_input_bytes(&mut bytes, read, limit)?;
        bytes.extend_from_slice(&buffer[..read]);
    }
}

fn reserve_input_bytes(
    bytes: &mut Vec<u8>,
    additional: usize,
    limit: usize,
) -> Result<(), CsvIngestError> {
    let required = bytes
        .len()
        .checked_add(additional)
        .ok_or(CsvIngestError::ByteLimitExceeded { limit })?;
    if required <= bytes.capacity() {
        return Ok(());
    }

    let mut target = bytes.capacity().max(READ_BUFFER_BYTES).min(limit);
    while target < required {
        target = target.saturating_mul(2).min(limit);
    }
    let requested = target - bytes.len();
    bytes
        .try_reserve_exact(requested)
        .map_err(|_| CsvIngestError::AllocationFailed {
            allocation: CsvIngestAllocation::InputBuffer,
            requested,
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CsvShape {
    header_fields: usize,
    header_start: usize,
    header_end: usize,
    data_start: usize,
    data_rows: usize,
    data_cells: usize,
    max_data_fields: usize,
    max_data_record_bytes: usize,
}

fn scan_csv_shape(bytes: &[u8]) -> Option<CsvShape> {
    let mut shape = CsvShape::default();
    let mut saw_header = false;
    let mut fields = 1_usize;
    let mut record_start = 0_usize;
    let mut index = 0_usize;
    let mut record_started = false;
    let mut at_field_start = true;
    let mut in_quotes = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_quotes {
            if byte == b'"' {
                if bytes.get(index + 1) == Some(&b'"') {
                    index += 2;
                    continue;
                }
                in_quotes = false;
            }
        } else {
            match byte {
                b'"' if at_field_start => {
                    in_quotes = true;
                    record_started = true;
                }
                b',' => {
                    fields = fields.checked_add(1)?;
                    record_started = true;
                    at_field_start = true;
                    index += 1;
                    continue;
                }
                b'\r' | b'\n' => {
                    let terminator_bytes =
                        usize::from(byte == b'\r' && bytes.get(index + 1) == Some(&b'\n')) + 1;
                    if record_started {
                        finish_shape_record(
                            &mut shape,
                            &mut saw_header,
                            fields,
                            record_start,
                            index,
                            index.checked_add(terminator_bytes)?,
                        )?;
                    }
                    fields = 1;
                    record_started = false;
                    at_field_start = true;
                    index = index.checked_add(terminator_bytes)?;
                    record_start = index;
                    continue;
                }
                _ => record_started = true,
            }
        }
        at_field_start = false;
        index += 1;
    }
    if record_started {
        finish_shape_record(
            &mut shape,
            &mut saw_header,
            fields,
            record_start,
            bytes.len(),
            bytes.len(),
        )?;
    }
    Some(shape)
}

fn finish_shape_record(
    shape: &mut CsvShape,
    saw_header: &mut bool,
    fields: usize,
    record_start: usize,
    record_end: usize,
    data_start: usize,
) -> Option<()> {
    let record_bytes = record_end.checked_sub(record_start)?;
    if !*saw_header {
        shape.header_fields = fields;
        shape.header_start = record_start;
        shape.header_end = record_end;
        shape.data_start = data_start;
        *saw_header = true;
    } else {
        shape.data_rows = shape.data_rows.checked_add(1)?;
        shape.data_cells = shape.data_cells.checked_add(fields)?;
        shape.max_data_fields = shape.max_data_fields.max(fields);
        shape.max_data_record_bytes = shape.max_data_record_bytes.max(record_bytes);
    }
    Some(())
}

fn first_header_mismatch(header: &[u8], fields: &[Field]) -> Option<usize> {
    if header.is_empty() {
        return None;
    }

    let mut column = 0_usize;
    let mut field_start = 0_usize;
    let mut index = 0_usize;
    let mut at_field_start = true;
    let mut in_quotes = false;
    while index <= header.len() {
        let at_end = index == header.len();
        if at_end || (!in_quotes && header[index] == b',') {
            if let Some(field) = fields.get(column)
                && !csv_field_equals(&header[field_start..index], field.name().as_bytes())
            {
                return Some(column);
            }
            column += 1;
            field_start = index.saturating_add(1);
            at_field_start = true;
            index += 1;
            continue;
        }

        let byte = header[index];
        if in_quotes {
            if byte == b'"' {
                if header.get(index + 1) == Some(&b'"') {
                    index += 2;
                    continue;
                }
                in_quotes = false;
            }
        } else if byte == b'"' && at_field_start {
            in_quotes = true;
        }
        at_field_start = false;
        index += 1;
    }
    None
}

fn csv_field_equals(raw: &[u8], expected: &[u8]) -> bool {
    if raw.first() != Some(&b'"') {
        return raw == expected;
    }

    let mut raw_index = 1_usize;
    let mut expected_index = 0_usize;
    while raw_index < raw.len() {
        let byte = raw[raw_index];
        if byte == b'"' {
            if raw.get(raw_index + 1) == Some(&b'"') {
                if expected.get(expected_index) != Some(&b'"') {
                    return false;
                }
                raw_index += 2;
                expected_index += 1;
                continue;
            }
            return raw_index + 1 == raw.len() && expected_index == expected.len();
        }
        if expected.get(expected_index) != Some(&byte) {
            return false;
        }
        raw_index += 1;
        expected_index += 1;
    }
    false
}

fn parser_working_bytes(shape: CsvShape, limit: usize) -> Result<usize, CsvIngestError> {
    if shape.data_rows == 0 {
        return Ok(0);
    }
    let payload_capacity = geometric_csv_capacity(shape.max_data_record_bytes).ok_or(
        CsvIngestError::DecodedLimitExceeded {
            limit,
            required: usize::MAX,
        },
    )?;
    let field_capacity = geometric_csv_capacity(shape.max_data_fields).ok_or(
        CsvIngestError::DecodedLimitExceeded {
            limit,
            required: usize::MAX,
        },
    )?;
    let field_indexes = field_capacity.checked_mul(mem::size_of::<usize>()).ok_or(
        CsvIngestError::DecodedLimitExceeded {
            limit,
            required: usize::MAX,
        },
    )?;
    READ_BUFFER_BYTES
        .checked_add(PARSER_FIXED_BYTES)
        .and_then(|bytes| bytes.checked_add(payload_capacity))
        .and_then(|bytes| bytes.checked_add(field_indexes))
        .ok_or(CsvIngestError::DecodedLimitExceeded {
            limit,
            required: usize::MAX,
        })
}

fn geometric_csv_capacity(required: usize) -> Option<usize> {
    if required == 0 {
        Some(0)
    } else {
        required.max(4).checked_next_power_of_two()
    }
}

fn invalid_utf8_csv_error() -> csv::Error {
    // `csv::Error` has no public constructor. Header bytes were already
    // validated allocation-free, so parse a fixed one-byte fixture solely to
    // return the crate's documented UTF-8 error kind without copying input.
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(&b"\xff\n"[..]);
    let mut record = csv::StringRecord::new();
    reader
        .read_record(&mut record)
        .expect_err("the fixed invalid UTF-8 record must be rejected")
}

fn ensure_decoded_limit(
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, CsvIngestError> {
    let required = current
        .checked_add(additional)
        .ok_or(CsvIngestError::DecodedLimitExceeded {
            limit,
            required: usize::MAX,
        })?;
    if required > limit {
        return Err(CsvIngestError::DecodedLimitExceeded { limit, required });
    }
    Ok(required)
}

fn planned_row_collection_growth(row_count: usize, row_capacity: usize) -> Option<usize> {
    if row_count < row_capacity {
        return Some(0);
    }
    let additional_capacity = row_capacity.max(1);
    additional_capacity.checked_mul(mem::size_of::<Vec<Value>>())
}

fn reserve_row_slot(rows: &mut Vec<Vec<Value>>) -> Result<(), CsvIngestError> {
    if rows.len() < rows.capacity() {
        return Ok(());
    }
    let additional_capacity = rows.capacity().max(1);
    let requested = additional_capacity.saturating_mul(mem::size_of::<Vec<Value>>());
    rows.try_reserve_exact(additional_capacity)
        .map_err(|_| CsvIngestError::AllocationFailed {
            allocation: CsvIngestAllocation::RowCollection,
            requested,
        })
}

fn convert_value(
    raw: &str,
    field: &Field,
    row: usize,
    column: usize,
) -> Result<Value, CsvIngestError> {
    let invalid = || CsvIngestError::InvalidValue {
        row,
        column,
        expected: field.data_type(),
        value_bytes: raw.len(),
    };

    match field.data_type() {
        DataType::Int64 => raw.parse().map(Value::Int64).map_err(|_| invalid()),
        DataType::Float64 => raw.parse().map(Value::Float64).map_err(|_| invalid()),
        DataType::Bool => raw.parse().map(Value::Bool).map_err(|_| invalid()),
        DataType::String => copy_string(raw, CsvIngestAllocation::StringValue).map(Value::String),
    }
}

fn copy_string(value: &str, allocation: CsvIngestAllocation) -> Result<String, CsvIngestError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| CsvIngestError::AllocationFailed {
            allocation,
            requested: value.len(),
        })?;
    owned.push_str(value);
    Ok(owned)
}
