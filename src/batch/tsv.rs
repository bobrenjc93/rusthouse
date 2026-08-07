//! Bounded ingestion for typed `TabSeparatedWithNames` input.
//!
//! Fields use the same ClickHouse-style backslash escapes as the TSV writer.
//! Physical records may end in LF or CRLF; escaped line endings remain field
//! data after decoding.

use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::Table;
use super::value::{DataType, Value};

/// Default maximum size of one typed TSV input, including its header.
pub const DEFAULT_MAX_TSV_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of data rows in one typed TSV input.
pub const DEFAULT_MAX_TSV_ROWS: usize = 100_000;
/// Default maximum number of data values in one typed TSV input.
pub const DEFAULT_MAX_TSV_VALUES: usize = 1_000_000;

/// Resource bounds for one typed `TabSeparatedWithNames` ingestion operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsvIngestLimits {
    /// Maximum complete input size, in bytes, including the header.
    pub max_bytes: usize,
    /// Maximum number of data rows after the header.
    pub max_rows: usize,
    /// Maximum total number of fields across all data rows.
    pub max_values: usize,
}

impl TsvIngestLimits {
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

impl Default for TsvIngestLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_TSV_BYTES,
            DEFAULT_MAX_TSV_ROWS,
            DEFAULT_MAX_TSV_VALUES,
        )
    }
}

/// A failure while validating or appending typed `TabSeparatedWithNames` input.
///
/// Line and column numbers are one-based. The header is line 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsvIngestError {
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
    /// A decoded header field does not exactly equal its schema name.
    HeaderMismatch { column: usize, expected: String },
    /// A row crosses the configured row bound.
    RowLimitExceeded {
        line: usize,
        rows: usize,
        max_rows: usize,
    },
    /// A row crosses the configured total-value bound.
    ValueLimitExceeded {
        line: usize,
        values: usize,
        max_values: usize,
    },
    /// A row does not have exactly one field for each schema column.
    WrongColumnCount {
        line: usize,
        expected: usize,
        actual: usize,
    },
    /// A field contains a trailing backslash or an unsupported escape.
    InvalidEscape { line: usize, column: usize },
    /// A decoded field cannot be parsed as its schema type.
    InvalidValue {
        line: usize,
        column: usize,
        expected: DataType,
    },
    /// Table lookup or the final capacity-checked atomic append failed.
    Database(Error),
}

impl fmt::Display for TsvIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "TSV input is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::InvalidUtf8 { valid_up_to } => {
                write!(
                    formatter,
                    "TSV input is not valid UTF-8 at byte {valid_up_to}"
                )
            }
            Self::MissingHeader { line } => {
                write!(
                    formatter,
                    "missing TabSeparatedWithNames header at line {line}"
                )
            }
            Self::InvalidLineEnding { line } => write!(
                formatter,
                "TSV line {line} contains a bare carriage return; use LF or CRLF"
            ),
            Self::HeaderColumnCount { expected, actual } => write!(
                formatter,
                "TSV header has {actual} columns; expected {expected}"
            ),
            Self::HeaderMismatch { column, expected } => write!(
                formatter,
                "TSV header column {column} does not exactly match schema column '{expected}'"
            ),
            Self::RowLimitExceeded {
                line,
                rows,
                max_rows,
            } => write!(
                formatter,
                "TSV record at line {line} raises the row count to {rows}, exceeding the limit of {max_rows}"
            ),
            Self::ValueLimitExceeded {
                line,
                values,
                max_values,
            } => write!(
                formatter,
                "TSV record at line {line} raises the value count to {values}, exceeding the limit of {max_values}"
            ),
            Self::WrongColumnCount {
                line,
                expected,
                actual,
            } => write!(
                formatter,
                "TSV record at line {line} has {actual} columns; expected {expected}"
            ),
            Self::InvalidEscape { line, column } => write!(
                formatter,
                "TSV field at line {line}, column {column} contains an invalid backslash escape"
            ),
            Self::InvalidValue {
                line,
                column,
                expected,
            } => write!(
                formatter,
                "TSV field at line {line}, column {column} is not a valid {expected}"
            ),
            Self::Database(error) => write!(formatter, "could not ingest TSV input: {error}"),
        }
    }
}

impl StdError for TsvIngestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for TsvIngestError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

pub(crate) fn parse_rows(
    table: &Table,
    input: &[u8],
    limits: TsvIngestLimits,
) -> Result<Vec<Vec<Value>>, TsvIngestError> {
    if input.len() > limits.max_bytes {
        return Err(TsvIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: limits.max_bytes,
        });
    }
    let input = std::str::from_utf8(input).map_err(invalid_utf8)?;
    if input.is_empty() {
        return Err(TsvIngestError::MissingHeader { line: 1 });
    }

    let mut lines = input.split_inclusive('\n');
    let raw_header = lines.next().expect("non-empty input has a line");
    let header = line_contents(raw_header, 1)?;
    validate_header(table, header)?;

    let expected_columns = table.schema().len();
    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for (offset, raw_record) in lines.enumerate() {
        let line = offset.saturating_add(2);
        let record = line_contents(raw_record, line)?;
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(TsvIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }

        let actual_columns = record
            .as_bytes()
            .iter()
            .filter(|byte| **byte == b'\t')
            .count()
            .saturating_add(1);
        let next_value_count = value_count.saturating_add(actual_columns);
        if next_value_count > limits.max_values {
            return Err(TsvIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values: limits.max_values,
            });
        }
        if actual_columns != expected_columns {
            return Err(TsvIngestError::WrongColumnCount {
                line,
                expected: expected_columns,
                actual: actual_columns,
            });
        }

        let mut row = Vec::with_capacity(expected_columns);
        for (offset, field) in record.split('\t').enumerate() {
            let column = offset.saturating_add(1);
            let decoded = decode_field(field, line, column)?;
            row.push(parse_value(
                decoded,
                table.schema()[offset].data_type,
                line,
                column,
            )?);
        }
        value_count = next_value_count;
        rows.push(row);
    }

    Ok(rows)
}

fn line_contents(raw_line: &str, line: usize) -> Result<&str, TsvIngestError> {
    let without_lf = raw_line.strip_suffix('\n').unwrap_or(raw_line);
    let contents = without_lf.strip_suffix('\r').unwrap_or(without_lf);
    if contents.contains('\r') || (raw_line.ends_with('\r') && !raw_line.ends_with("\r\n")) {
        return Err(TsvIngestError::InvalidLineEnding { line });
    }
    Ok(contents)
}

fn validate_header(table: &Table, header: &str) -> Result<(), TsvIngestError> {
    let field_count = header
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\t')
        .count()
        .saturating_add(1);
    if field_count != table.schema().len() {
        return Err(TsvIngestError::HeaderColumnCount {
            expected: table.schema().len(),
            actual: field_count,
        });
    }
    for (offset, (field, schema)) in header.split('\t').zip(table.schema()).enumerate() {
        let column = offset.saturating_add(1);
        if decode_field(field, 1, column)? != schema.name {
            return Err(TsvIngestError::HeaderMismatch {
                column,
                expected: schema.name.clone(),
            });
        }
    }
    Ok(())
}

fn decode_field(field: &str, line: usize, column: usize) -> Result<String, TsvIngestError> {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'\\' {
            decoded.push(bytes[cursor]);
            cursor += 1;
            continue;
        }

        let Some(escaped) = bytes.get(cursor + 1) else {
            return Err(TsvIngestError::InvalidEscape { line, column });
        };
        decoded.push(match escaped {
            b'\\' => b'\\',
            b't' => b'\t',
            b'r' => b'\r',
            b'n' => b'\n',
            b'0' => b'\0',
            b'b' => b'\x08',
            b'f' => b'\x0c',
            b'\'' => b'\'',
            _ => return Err(TsvIngestError::InvalidEscape { line, column }),
        });
        cursor += 2;
    }
    Ok(String::from_utf8(decoded).expect("decoding preserves valid UTF-8"))
}

fn parse_value(
    field: String,
    data_type: DataType,
    line: usize,
    column: usize,
) -> Result<Value, TsvIngestError> {
    let invalid = || TsvIngestError::InvalidValue {
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
        DataType::String => Ok(Value::String(field)),
    }
}

fn invalid_utf8(error: Utf8Error) -> TsvIngestError {
    TsvIngestError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    }
}
