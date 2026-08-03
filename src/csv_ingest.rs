//! Transactional CSV ingestion for [`Table`].
//!
//! CSV input must start with a header whose names and order exactly match the
//! destination schema. The complete bounded input is parsed and converted
//! before [`Table::insert_batch`] is called, so malformed input never appends a
//! partial batch.

use crate::{DataType, Table, TableError, Value};
use std::error::Error;
use std::fmt;
use std::io::{self, Read};

/// Default maximum size of one CSV import: 64 MiB.
pub const DEFAULT_CSV_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum number of data rows in one CSV import.
pub const DEFAULT_CSV_ROWS: usize = 1_000_000;

/// Resource limits for one CSV import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvIngestLimits {
    max_input_bytes: usize,
    max_rows: usize,
}

impl CsvIngestLimits {
    /// Creates limits for raw input bytes and data rows, excluding the header.
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_rows: usize) -> Self {
        Self {
            max_input_bytes,
            max_rows,
        }
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
}

impl Default for CsvIngestLimits {
    fn default() -> Self {
        Self::new(DEFAULT_CSV_INPUT_BYTES, DEFAULT_CSV_ROWS)
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
    /// The CSV header does not exactly match the table schema.
    HeaderMismatch {
        /// Required field names in schema order.
        expected: Vec<String>,
        /// Supplied header values in CSV order.
        actual: Vec<String>,
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
        /// Name of the schema field.
        field: String,
        /// Type required by the schema.
        expected: DataType,
        /// Original CSV field value.
        value: String,
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
            Self::HeaderMismatch { expected, actual } => write!(
                formatter,
                "CSV header {actual:?} does not match table schema {expected:?}"
            ),
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
                field,
                expected,
                value,
            } => write!(
                formatter,
                "CSV data row {row}, column {column} (`{field}`) value {value:?} is not {expected}"
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
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_reader(bytes.as_slice());

        let header = reader.headers().map_err(CsvIngestError::Csv)?;
        if header.len() != self.fields().len()
            || header
                .iter()
                .zip(self.fields())
                .any(|(actual, expected)| actual != expected.name())
        {
            return Err(CsvIngestError::HeaderMismatch {
                expected: self
                    .fields()
                    .iter()
                    .map(|field| field.name().to_owned())
                    .collect(),
                actual: header.iter().map(str::to_owned).collect(),
            });
        }

        let mut rows = Vec::new();
        for (row_index, record) in reader.records().enumerate() {
            let record = record.map_err(CsvIngestError::Csv)?;
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

            let mut row = Vec::with_capacity(self.fields().len());
            for (column, (raw, field)) in record.iter().zip(self.fields()).enumerate() {
                row.push(convert_value(raw, field.data_type()).map_err(|()| {
                    CsvIngestError::InvalidValue {
                        row: row_index,
                        column,
                        field: field.name().to_owned(),
                        expected: field.data_type(),
                        value: raw.to_owned(),
                    }
                })?);
            }
            rows.push(row);
        }

        self.insert_batch(rows).map_err(CsvIngestError::Table)
    }
}

fn read_bounded<R: Read>(input: R, limit: usize) -> Result<Vec<u8>, CsvIngestError> {
    let read_limit = (limit as u64).saturating_add(1);
    let mut bytes = Vec::new();
    input
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(CsvIngestError::Read)?;
    if bytes.len() > limit {
        return Err(CsvIngestError::ByteLimitExceeded { limit });
    }
    Ok(bytes)
}

fn convert_value(raw: &str, data_type: DataType) -> Result<Value, ()> {
    match data_type {
        DataType::Int64 => raw.parse().map(Value::Int64).map_err(|_| ()),
        DataType::Float64 => raw.parse().map(Value::Float64).map_err(|_| ()),
        DataType::Bool => raw.parse().map(Value::Bool).map_err(|_| ()),
        DataType::String => Ok(Value::String(raw.to_owned())),
    }
}
