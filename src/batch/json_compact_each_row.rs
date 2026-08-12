//! Bounded ingestion for a one-column `JSONCompactEachRow` subset.
//!
//! Each physical line must be one JSON array containing exactly one signed
//! integer or `null`. The target must already exist and contain exactly one
//! physical `Int64` or `Nullable(Int64)` column. JSON whitespace is accepted
//! around the array and its value; records may use LF or CRLF endings. This is
//! intentionally a positional scalar importer, not a general JSON engine.

use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::{PreparedInsertRows, Table};
use super::value::{DataType, Value};

/// Default maximum size of one `JSONCompactEachRow` input.
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
    /// Maximum total number of JSON array values.
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
/// Line and column numbers are one-based. Columns identify byte positions in
/// the UTF-8 input line; every accepted JSON token is ASCII.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonCompactEachRowIngestError {
    /// The complete byte input exceeds its configured bound.
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
    /// The input is not valid UTF-8.
    InvalidUtf8 { valid_up_to: usize },
    /// A bare carriage return was used instead of LF or CRLF.
    InvalidLineEnding { line: usize },
    /// The target does not have exactly one physical column.
    UnsupportedColumnCount { table: String, actual: usize },
    /// The target's only physical column is not `Int64`.
    UnsupportedColumnType { column: String, data_type: DataType },
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
    /// A physical line is not a complete JSON array.
    InvalidJson { line: usize, column: usize },
    /// A JSON array does not contain exactly one value.
    WrongValueCount { line: usize, actual: usize },
    /// An array value is not a JSON integer or the exact token `null`.
    InvalidValue { line: usize, column: usize },
    /// A syntactically valid JSON integer is outside the `Int64` range.
    IntegerOverflow { line: usize, column: usize },
    /// JSON `null` was supplied for a non-nullable `Int64` column.
    NullForNonNullable { line: usize, column: usize },
    /// Table lookup, capacity preflight, or the WAL-first atomic append failed.
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
            Self::InvalidLineEnding { line } => write!(
                formatter,
                "JSONCompactEachRow line {line} contains a bare carriage return; use LF or CRLF"
            ),
            Self::UnsupportedColumnCount { table, actual } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires exactly one column in table '{table}'; found {actual}"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires Int64 or Nullable(Int64); column '{column}' has type {data_type}"
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
                "JSONCompactEachRow line {line}, column {column} is not a complete JSON array"
            ),
            Self::WrongValueCount { line, actual } => write!(
                formatter,
                "JSONCompactEachRow line {line} has {actual} values; expected 1"
            ),
            Self::InvalidValue { line, column } => write!(
                formatter,
                "JSONCompactEachRow value at line {line}, column {column} is not a JSON Int64 or null"
            ),
            Self::IntegerOverflow { line, column } => write!(
                formatter,
                "JSONCompactEachRow integer at line {line}, column {column} is outside the Int64 range"
            ),
            Self::NullForNonNullable { line, column } => write!(
                formatter,
                "JSONCompactEachRow null at line {line}, column {column} requires Nullable(Int64)"
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

    let nullable = table.column_is_nullable_int64(0);
    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for (offset, raw_line) in input.split_inclusive('\n').enumerate() {
        let line = offset.saturating_add(1);
        let contents = line_contents(raw_line, line)?;
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(JsonCompactEachRowIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }

        let values = parse_line(contents, line, nullable, value_count, limits.max_values)?;
        let next_value_count = value_count.saturating_add(values.len());
        if next_value_count > limits.max_values {
            return Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values: limits.max_values,
            });
        }
        if values.len() != 1 {
            return Err(JsonCompactEachRowIngestError::WrongValueCount {
                line,
                actual: values.len(),
            });
        }

        value_count = next_value_count;
        rows.push(values);
    }

    table
        .prepare_projected_rows(vec![0], rows)
        .map_err(Into::into)
}

fn validate_target(table: &Table) -> Result<(), JsonCompactEachRowIngestError> {
    if table.schema().len() != 1 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnCount {
            table: table.name().to_owned(),
            actual: table.schema().len(),
        });
    }
    let column = &table.schema()[0];
    if column.data_type != DataType::Int64 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: column.name.clone(),
            data_type: column.data_type,
        });
    }
    Ok(())
}

fn parse_line(
    line_contents: &str,
    line: usize,
    nullable: bool,
    preceding_values: usize,
    max_values: usize,
) -> Result<Vec<Value>, JsonCompactEachRowIngestError> {
    let bytes = line_contents.as_bytes();
    let mut cursor = skip_whitespace(bytes, 0);
    if bytes.get(cursor) != Some(&b'[') {
        return Err(invalid_json(line, cursor));
    }
    cursor = skip_whitespace(bytes, cursor + 1);

    let mut values = Vec::new();
    if bytes.get(cursor) == Some(&b']') {
        cursor = skip_whitespace(bytes, cursor + 1);
        if cursor != bytes.len() {
            return Err(invalid_json(line, cursor));
        }
        return Ok(values);
    }

    loop {
        let next_value_count = preceding_values
            .saturating_add(values.len())
            .saturating_add(1);
        if next_value_count > max_values {
            return Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values,
            });
        }
        let (value, next) = parse_value(line_contents, line, cursor, nullable)?;
        values.push(value);
        cursor = skip_whitespace(bytes, next);
        match bytes.get(cursor) {
            Some(b',') => {
                cursor = skip_whitespace(bytes, cursor + 1);
                if matches!(bytes.get(cursor), None | Some(b']') | Some(b',')) {
                    return Err(invalid_json(line, cursor));
                }
            }
            Some(b']') => {
                cursor = skip_whitespace(bytes, cursor + 1);
                if cursor != bytes.len() {
                    return Err(invalid_json(line, cursor));
                }
                return Ok(values);
            }
            _ => return Err(invalid_json(line, cursor)),
        }
    }
}

fn parse_value(
    contents: &str,
    line: usize,
    start: usize,
    nullable: bool,
) -> Result<(Value, usize), JsonCompactEachRowIngestError> {
    let bytes = contents.as_bytes();
    let end = (start..bytes.len())
        .find(|&index| is_value_delimiter(bytes[index]))
        .unwrap_or(bytes.len());
    let token = &contents[start..end];
    let column = start.saturating_add(1);

    if token == "null" {
        if !nullable {
            return Err(JsonCompactEachRowIngestError::NullForNonNullable { line, column });
        }
        return Ok((Value::Null(DataType::Int64), end));
    }
    if !is_json_integer(token.as_bytes()) {
        return Err(JsonCompactEachRowIngestError::InvalidValue { line, column });
    }
    let value = token
        .parse::<i64>()
        .map_err(|_| JsonCompactEachRowIngestError::IntegerOverflow { line, column })?;
    Ok((Value::Int64(value), end))
}

fn is_json_integer(token: &[u8]) -> bool {
    let digits = token.strip_prefix(b"-").unwrap_or(token);
    match digits {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

fn is_value_delimiter(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b',' | b']')
}

fn skip_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    cursor
}

fn line_contents(raw_line: &str, line: usize) -> Result<&str, JsonCompactEachRowIngestError> {
    let without_lf = raw_line.strip_suffix('\n').unwrap_or(raw_line);
    let contents = without_lf.strip_suffix('\r').unwrap_or(without_lf);
    if contents.contains('\r') || (raw_line.ends_with('\r') && !raw_line.ends_with("\r\n")) {
        return Err(JsonCompactEachRowIngestError::InvalidLineEnding { line });
    }
    Ok(contents)
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

#[cfg(test)]
mod tests {
    use super::{JsonCompactEachRowIngestError, is_json_integer};

    #[test]
    fn integer_grammar_is_json_specific() {
        for valid in [b"0".as_slice(), b"-0", b"7", b"-42"] {
            assert!(
                is_json_integer(valid),
                "{:?}",
                String::from_utf8_lossy(valid)
            );
        }
        for invalid in [b"".as_slice(), b"-", b"+1", b"01", b"-01", b"1.0", b"1e2"] {
            assert!(
                !is_json_integer(invalid),
                "{:?}",
                String::from_utf8_lossy(invalid)
            );
        }
    }

    #[test]
    fn error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsonCompactEachRowIngestError>();
    }
}
