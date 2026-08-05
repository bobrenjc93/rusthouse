//! Bounded ingestion for an unquoted, typed `CSVWithNames` subset.
//!
//! This intentionally is not a general CSV implementation. Double quotes are
//! rejected, so fields cannot contain commas, CR, or LF. In particular, a
//! `String` field is copied exactly as written between delimiters and cannot
//! use CSV quoting or escaping.

use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::Table;
use super::value::{DataType, Value};

/// Default maximum size of one typed CSV input, including its header.
pub const DEFAULT_MAX_CSV_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of data rows in one typed CSV input.
pub const DEFAULT_MAX_CSV_ROWS: usize = 100_000;
/// Default maximum number of data values in one typed CSV input.
pub const DEFAULT_MAX_CSV_VALUES: usize = 1_000_000;

/// Resource bounds for one typed `CSVWithNames` ingestion operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvIngestLimits {
    /// Maximum complete input size, in bytes, including the header.
    pub max_bytes: usize,
    /// Maximum number of data rows after the header.
    pub max_rows: usize,
    /// Maximum total number of fields across all data rows.
    pub max_values: usize,
}

impl CsvIngestLimits {
    /// Creates explicit complete-input, row, and data-value bounds.
    #[must_use]
    pub const fn new(max_bytes: usize, max_rows: usize, max_values: usize) -> Self {
        Self {
            max_bytes,
            max_rows,
            max_values,
        }
    }
}

impl Default for CsvIngestLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_CSV_BYTES,
            DEFAULT_MAX_CSV_ROWS,
            DEFAULT_MAX_CSV_VALUES,
        )
    }
}

/// A failure while validating or appending typed `CSVWithNames` input.
///
/// Line and column numbers are one-based. The header is line 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvIngestError {
    /// The complete byte input exceeds its configured bound.
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
    /// The input is not valid UTF-8.
    InvalidUtf8 { valid_up_to: usize },
    /// The input is empty and therefore has no header.
    MissingHeader { line: usize },
    /// A bare carriage return was used instead of LF or CRLF.
    InvalidLineEnding { line: usize },
    /// The header does not have the same number of columns as the table.
    HeaderColumnCount { expected: usize, actual: usize },
    /// A header field does not exactly equal the corresponding schema name.
    HeaderMismatch { column: usize, expected: String },
    /// A data row crosses the configured row bound.
    RowLimitExceeded {
        line: usize,
        rows: usize,
        max_rows: usize,
    },
    /// A data row crosses the configured total-value bound.
    ValueLimitExceeded {
        line: usize,
        values: usize,
        max_values: usize,
    },
    /// A data row does not have exactly one field for each schema column.
    WrongColumnCount {
        line: usize,
        expected: usize,
        actual: usize,
    },
    /// A double quote was found; quoted and escaped fields are unsupported.
    QuotingNotSupported { line: usize, column: usize },
    /// A field cannot be parsed as its schema type.
    InvalidValue {
        line: usize,
        column: usize,
        expected: DataType,
    },
    /// Table lookup or the final capacity-checked atomic append failed.
    Database(Error),
}

impl fmt::Display for CsvIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "CSV input is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::InvalidUtf8 { valid_up_to } => write!(
                formatter,
                "CSV input is not valid UTF-8 at byte {valid_up_to}"
            ),
            Self::MissingHeader { line } => {
                write!(formatter, "missing CSVWithNames header at line {line}")
            }
            Self::InvalidLineEnding { line } => write!(
                formatter,
                "CSV line {line} contains a bare carriage return; use LF or CRLF"
            ),
            Self::HeaderColumnCount { expected, actual } => write!(
                formatter,
                "CSV header has {actual} columns; expected {expected}"
            ),
            Self::HeaderMismatch { column, expected } => write!(
                formatter,
                "CSV header column {column} does not exactly match schema column '{expected}'"
            ),
            Self::RowLimitExceeded {
                line,
                rows,
                max_rows,
            } => write!(
                formatter,
                "CSV record at line {line} raises the row count to {rows}, exceeding the limit of {max_rows}"
            ),
            Self::ValueLimitExceeded {
                line,
                values,
                max_values,
            } => write!(
                formatter,
                "CSV record at line {line} raises the value count to {values}, exceeding the limit of {max_values}"
            ),
            Self::WrongColumnCount {
                line,
                expected,
                actual,
            } => write!(
                formatter,
                "CSV record at line {line} has {actual} columns; expected {expected}"
            ),
            Self::QuotingNotSupported { line, column } => write!(
                formatter,
                "CSV field at line {line}, column {column} uses unsupported quoting"
            ),
            Self::InvalidValue {
                line,
                column,
                expected,
            } => write!(
                formatter,
                "CSV field at line {line}, column {column} is not a valid {expected}"
            ),
            Self::Database(error) => write!(formatter, "could not ingest CSV input: {error}"),
        }
    }
}

impl StdError for CsvIngestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for CsvIngestError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

pub(crate) fn parse_rows(
    table: &Table,
    input: &[u8],
    limits: CsvIngestLimits,
) -> Result<Vec<Vec<Value>>, CsvIngestError> {
    if input.len() > limits.max_bytes {
        return Err(CsvIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: limits.max_bytes,
        });
    }
    let input = std::str::from_utf8(input).map_err(invalid_utf8)?;
    if input.is_empty() {
        return Err(CsvIngestError::MissingHeader { line: 1 });
    }

    let mut lines = input.split_inclusive('\n');
    let header = line_contents(lines.next().expect("non-empty input has a line"), 1)?;
    validate_header(table, header)?;

    let expected_columns = table.schema().len();
    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for (offset, raw_line) in lines.enumerate() {
        let line = offset + 2;
        let record = line_contents(raw_line, line)?;
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(CsvIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }

        let actual_columns = field_count(record);
        let next_value_count = value_count.saturating_add(actual_columns);
        if next_value_count > limits.max_values {
            return Err(CsvIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values: limits.max_values,
            });
        }
        if actual_columns != expected_columns {
            return Err(CsvIngestError::WrongColumnCount {
                line,
                expected: expected_columns,
                actual: actual_columns,
            });
        }

        let mut row = Vec::with_capacity(expected_columns);
        for (column, (field, definition)) in record.split(',').zip(table.schema()).enumerate() {
            let column = column + 1;
            reject_quoting(field, line, column)?;
            row.push(parse_value(field, definition.data_type, line, column)?);
        }
        value_count = next_value_count;
        rows.push(row);
    }

    Ok(rows)
}

fn invalid_utf8(error: Utf8Error) -> CsvIngestError {
    CsvIngestError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    }
}

fn line_contents(line: &str, line_number: usize) -> Result<&str, CsvIngestError> {
    let without_lf = line.strip_suffix('\n').unwrap_or(line);
    let contents = without_lf.strip_suffix('\r').unwrap_or(without_lf);
    if contents.contains('\r') || (!line.ends_with('\n') && without_lf.ends_with('\r')) {
        return Err(CsvIngestError::InvalidLineEnding { line: line_number });
    }
    Ok(contents)
}

fn validate_header(table: &Table, header: &str) -> Result<(), CsvIngestError> {
    let expected_columns = table.schema().len();
    let actual_columns = field_count(header);
    if actual_columns != expected_columns {
        return Err(CsvIngestError::HeaderColumnCount {
            expected: expected_columns,
            actual: actual_columns,
        });
    }

    for (column, (field, definition)) in header.split(',').zip(table.schema()).enumerate() {
        let column = column + 1;
        reject_quoting(field, 1, column)?;
        if field != definition.name {
            return Err(CsvIngestError::HeaderMismatch {
                column,
                expected: definition.name.clone(),
            });
        }
    }
    Ok(())
}

fn field_count(record: &str) -> usize {
    record
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b',')
        .count()
        .saturating_add(1)
}

fn reject_quoting(field: &str, line: usize, column: usize) -> Result<(), CsvIngestError> {
    if field.contains('"') {
        return Err(CsvIngestError::QuotingNotSupported { line, column });
    }
    Ok(())
}

fn parse_value(
    field: &str,
    data_type: DataType,
    line: usize,
    column: usize,
) -> Result<Value, CsvIngestError> {
    let invalid = || CsvIngestError::InvalidValue {
        line,
        column,
        expected: data_type,
    };
    match data_type {
        DataType::Int64 => field
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| invalid()),
        DataType::Float64 => field
            .parse::<f64>()
            .map_err(|_| invalid())
            .and_then(|value| {
                if value.is_finite() {
                    Ok(Value::Float64(value))
                } else {
                    Err(invalid())
                }
            }),
        DataType::Bool => field
            .parse::<bool>()
            .map(Value::Bool)
            .map_err(|_| invalid()),
        DataType::String => Ok(Value::String(field.to_owned())),
    }
}
