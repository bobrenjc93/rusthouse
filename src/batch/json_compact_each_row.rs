//! Bounded ingestion for one-column `JSONCompactEachRow` input.
//!
//! This intentionally small positional subset accepts one JSON array per
//! physical line. Every array contains exactly one JSON integer, or `null` for
//! a `Nullable(Int64)` target. The target must be an existing one-column
//! `Int64` or `Nullable(Int64)` table. Input is completely validated and
//! prepared before the caller commits any row.

use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::{PreparedInsertRows, Table};
use super::value::{DataType, Value};

/// Default maximum size of one `JSONCompactEachRow` ingestion operation.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of rows in one `JSONCompactEachRow` input.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_ROWS: usize = 100_000;
/// Default maximum number of values in one `JSONCompactEachRow` input.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_VALUES: usize = 100_000;

/// Resource bounds for one `JSONCompactEachRow` ingestion operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonCompactEachRowIngestLimits {
    /// Maximum complete input size, in bytes.
    pub max_bytes: usize,
    /// Maximum number of physical data rows.
    pub max_rows: usize,
    /// Maximum total number of positional values.
    pub max_values: usize,
}

impl JsonCompactEachRowIngestLimits {
    /// Creates explicit complete-input, row, and value bounds.
    #[must_use]
    pub const fn new(max_bytes: usize, max_rows: usize, max_values: usize) -> Self {
        Self {
            max_bytes,
            max_rows,
            max_values,
        }
    }
}

impl Default for JsonCompactEachRowIngestLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_BYTES,
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_ROWS,
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_VALUES,
        )
    }
}

/// A failure while validating or appending one-column `JSONCompactEachRow` input.
///
/// Line numbers and positional value indexes are one-based. JSON syntax
/// columns are one-based byte positions within the physical line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonCompactEachRowIngestError {
    /// The complete byte input exceeds its configured bound.
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
    /// The input is not valid UTF-8.
    InvalidUtf8 { valid_up_to: usize },
    /// The target does not have exactly one physical column.
    UnsupportedColumnCount { actual: usize },
    /// The sole target column is not `Int64` or `Nullable(Int64)`.
    UnsupportedColumnType { column: String, actual: DataType },
    /// A physical row crosses the configured row bound.
    RowLimitExceeded {
        line: usize,
        rows: usize,
        max_rows: usize,
    },
    /// A physical row crosses the configured total-value bound.
    ValueLimitExceeded {
        line: usize,
        values: usize,
        max_values: usize,
    },
    /// A row is not a syntactically complete JSON array.
    InvalidJson { line: usize, column: usize },
    /// A row does not contain exactly one positional value.
    WrongColumnCount {
        line: usize,
        expected: usize,
        actual: usize,
    },
    /// The sole value is valid JSON but is not an integer or `null`.
    InvalidValue {
        line: usize,
        column: usize,
        expected: DataType,
    },
    /// A syntactically valid JSON integer is outside the `Int64` range.
    IntegerOverflow { line: usize, column: usize },
    /// JSON `null` was supplied for a non-nullable `Int64` column.
    NullNotAllowed { line: usize, column: usize },
    /// Table lookup, capacity preflight, WAL commit, or final append failed.
    Database(Error),
}

impl fmt::Display for JsonCompactEachRowIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "JSONCompactEachRow input is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::InvalidUtf8 { valid_up_to } => write!(
                formatter,
                "JSONCompactEachRow input is not valid UTF-8 at byte {valid_up_to}"
            ),
            Self::UnsupportedColumnCount { actual } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires exactly one target column; found {actual}"
            ),
            Self::UnsupportedColumnType { column, actual } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires column '{column}' to be Int64 or Nullable(Int64); found {actual}"
            ),
            Self::RowLimitExceeded {
                line,
                rows,
                max_rows,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} raises the row count to {rows}, exceeding the limit of {max_rows}"
            ),
            Self::ValueLimitExceeded {
                line,
                values,
                max_values,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} raises the value count to {values}, exceeding the limit of {max_values}"
            ),
            Self::InvalidJson { line, column } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} is not valid JSON at byte column {column}"
            ),
            Self::WrongColumnCount {
                line,
                expected,
                actual,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} has {actual} values; expected {expected}"
            ),
            Self::InvalidValue {
                line,
                column,
                expected,
            } => write!(
                formatter,
                "JSONCompactEachRow value at line {line}, position {column} is not a valid {expected}"
            ),
            Self::IntegerOverflow { line, column } => write!(
                formatter,
                "JSONCompactEachRow integer at line {line}, position {column} is outside the Int64 range"
            ),
            Self::NullNotAllowed { line, column } => write!(
                formatter,
                "JSONCompactEachRow null at line {line}, position {column} is not allowed for Int64"
            ),
            Self::Database(error) => write!(
                formatter,
                "could not ingest JSONCompactEachRow input: {error}"
            ),
        }
    }
}

impl StdError for JsonCompactEachRowIngestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for JsonCompactEachRowIngestError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

pub(crate) fn parse_rows(
    table: &Table,
    input: &[u8],
    limits: JsonCompactEachRowIngestLimits,
) -> Result<PreparedInsertRows, JsonCompactEachRowIngestError> {
    if input.len() > limits.max_bytes {
        return Err(JsonCompactEachRowIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: limits.max_bytes,
        });
    }
    let input = std::str::from_utf8(input).map_err(invalid_utf8)?;
    validate_target(table)?;

    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for (offset, raw_line) in input.split_inclusive('\n').enumerate() {
        let line = offset.saturating_add(1);
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(JsonCompactEachRowIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }
        let next_value_count = value_count.saturating_add(1);
        if next_value_count > limits.max_values {
            return Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values: limits.max_values,
            });
        }

        let physical_line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let parsed = parse_row(physical_line, line)?;
        let value = match parsed {
            ParsedValue::Integer(value) => Value::Int64(value),
            ParsedValue::Null if table.column_is_nullable_int64(0) => Value::Null(DataType::Int64),
            ParsedValue::Null => {
                return Err(JsonCompactEachRowIngestError::NullNotAllowed { line, column: 1 });
            }
        };
        rows.push(vec![value]);
        value_count = next_value_count;
    }

    table
        .prepare_projected_rows(vec![0], rows)
        .map_err(Into::into)
}

fn validate_target(table: &Table) -> Result<(), JsonCompactEachRowIngestError> {
    if table.schema().len() != 1 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnCount {
            actual: table.schema().len(),
        });
    }
    let column = &table.schema()[0];
    if column.data_type != DataType::Int64 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: column.name.clone(),
            actual: column.data_type,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedValue {
    Integer(i64),
    Null,
}

fn parse_row(
    line_contents: &str,
    line: usize,
) -> Result<ParsedValue, JsonCompactEachRowIngestError> {
    let bytes = line_contents.as_bytes();
    let mut cursor = skip_json_whitespace(bytes, 0);
    if bytes.get(cursor) != Some(&b'[') {
        return Err(invalid_json(line, cursor));
    }
    cursor += 1;
    cursor = skip_json_whitespace(bytes, cursor);
    if bytes.get(cursor) == Some(&b']') {
        cursor += 1;
        cursor = skip_json_whitespace(bytes, cursor);
        if cursor != bytes.len() {
            return Err(invalid_json(line, cursor));
        }
        return Err(JsonCompactEachRowIngestError::WrongColumnCount {
            line,
            expected: 1,
            actual: 0,
        });
    }

    let value = parse_value(bytes, &mut cursor, line)?;
    cursor = skip_json_whitespace(bytes, cursor);
    if bytes.get(cursor) == Some(&b',') {
        let second_value = skip_json_whitespace(bytes, cursor.saturating_add(1));
        if matches!(bytes.get(second_value), None | Some(b']')) {
            return Err(invalid_json(line, second_value));
        }
        return Err(JsonCompactEachRowIngestError::WrongColumnCount {
            line,
            expected: 1,
            actual: 2,
        });
    }
    if bytes.get(cursor) != Some(&b']') {
        return Err(invalid_json(line, cursor));
    }
    cursor += 1;
    cursor = skip_json_whitespace(bytes, cursor);
    if cursor != bytes.len() {
        return Err(invalid_json(line, cursor));
    }
    Ok(value)
}

fn parse_value(
    bytes: &[u8],
    cursor: &mut usize,
    line: usize,
) -> Result<ParsedValue, JsonCompactEachRowIngestError> {
    let start = *cursor;
    if bytes.get(start..start.saturating_add(4)) == Some(b"null") {
        *cursor += 4;
        return Ok(ParsedValue::Null);
    }

    if !matches!(bytes.get(*cursor), Some(b'-' | b'0'..=b'9')) {
        return Err(JsonCompactEachRowIngestError::InvalidValue {
            line,
            column: 1,
            expected: DataType::Int64,
        });
    }

    if bytes.get(*cursor) == Some(&b'-') {
        *cursor += 1;
    }
    match bytes.get(*cursor) {
        Some(b'0') => *cursor += 1,
        Some(b'1'..=b'9') => {
            *cursor += 1;
            while matches!(bytes.get(*cursor), Some(b'0'..=b'9')) {
                *cursor += 1;
            }
        }
        _ => return Err(invalid_json(line, *cursor)),
    }

    let integer_end = *cursor;
    let mut has_non_integer_component = false;
    if bytes.get(*cursor) == Some(&b'.') {
        has_non_integer_component = true;
        *cursor += 1;
        let fraction_start = *cursor;
        while matches!(bytes.get(*cursor), Some(b'0'..=b'9')) {
            *cursor += 1;
        }
        if *cursor == fraction_start {
            return Err(invalid_json(line, *cursor));
        }
    }
    if matches!(bytes.get(*cursor), Some(b'e' | b'E')) {
        has_non_integer_component = true;
        *cursor += 1;
        if matches!(bytes.get(*cursor), Some(b'+' | b'-')) {
            *cursor += 1;
        }
        let exponent_start = *cursor;
        while matches!(bytes.get(*cursor), Some(b'0'..=b'9')) {
            *cursor += 1;
        }
        if *cursor == exponent_start {
            return Err(invalid_json(line, *cursor));
        }
    }
    if has_non_integer_component {
        return Err(JsonCompactEachRowIngestError::InvalidValue {
            line,
            column: 1,
            expected: DataType::Int64,
        });
    }

    let integer = std::str::from_utf8(&bytes[start..integer_end])
        .expect("integer token is ASCII and therefore UTF-8");
    integer
        .parse::<i64>()
        .map(ParsedValue::Integer)
        .map_err(|_| JsonCompactEachRowIngestError::IntegerOverflow { line, column: 1 })
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(bytes.get(cursor), Some(b' ' | b'\t' | b'\r')) {
        cursor += 1;
    }
    cursor
}

fn invalid_json(line: usize, zero_based_column: usize) -> JsonCompactEachRowIngestError {
    JsonCompactEachRowIngestError::InvalidJson {
        line,
        column: zero_based_column.saturating_add(1),
    }
}

fn invalid_utf8(error: Utf8Error) -> JsonCompactEachRowIngestError {
    JsonCompactEachRowIngestError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    }
}
