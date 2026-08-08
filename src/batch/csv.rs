//! Bounded ingestion for a typed `CSVWithNames` subset.
//!
//! Data fields may be double-quoted. Commas and LF or CRLF line endings in a
//! quoted field are data, and a double quote is decoded from `""` before the
//! field is parsed according to the type selected by its header. Headers must
//! contain a nonempty, exact-case subset of schema names without duplicates;
//! names may appear in any order and must remain unquoted. Omitted columns use
//! the same typed defaults as an explicit-column SQL `INSERT`.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::{PreparedInsertRows, Table};
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
/// Line numbers are one-based physical input lines, and column numbers are
/// one-based input positions. Data-record errors use the physical line on which
/// the record begins. The header is line 1.
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
    /// The header has more fields than the table has schema columns.
    HeaderColumnCount { expected: usize, actual: usize },
    /// A header field differs in case from an otherwise matching schema name.
    HeaderMismatch { column: usize, expected: String },
    /// A header field does not name any schema column.
    UnknownHeaderColumn { column: usize, name: String },
    /// A schema column is named more than once in the header.
    DuplicateHeaderColumn { column: usize, name: String },
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
    /// A data row does not have exactly one field for each selected header column.
    WrongColumnCount {
        line: usize,
        expected: usize,
        actual: usize,
    },
    /// Quoting was used in a header.
    QuotingNotSupported { line: usize, column: usize },
    /// A quote was unclosed or used outside the quoted-field grammar.
    MalformedQuoting { line: usize, column: usize },
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
                "CSV header has {actual} columns; table has only {expected} schema columns"
            ),
            Self::HeaderMismatch { column, expected } => write!(
                formatter,
                "CSV header column {column} does not exactly match schema column '{expected}'"
            ),
            Self::UnknownHeaderColumn { column, name } => write!(
                formatter,
                "CSV header column {column} names unknown schema column '{name}'"
            ),
            Self::DuplicateHeaderColumn { column, name } => write!(
                formatter,
                "CSV header column {column} duplicates schema column '{name}'"
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
                "CSV field at line {line}, column {column} uses quoting where only an unquoted field is supported"
            ),
            Self::MalformedQuoting { line, column } => write!(
                formatter,
                "CSV field at line {line}, column {column} has malformed quoting"
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
) -> Result<PreparedInsertRows, CsvIngestError> {
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

    let mut physical_lines = input.split_inclusive('\n');
    let raw_header = physical_lines.next().expect("non-empty input has a line");
    let header = line_contents(raw_header, 1)?;
    let header_plan = validate_header(table, header)?;

    let expected_columns = header_plan.schema_indexes.len();
    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for record in DataRecords::new(&input[raw_header.len()..], 2) {
        let (record, line) = record?;
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(CsvIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }

        let actual_columns = scan_record(record, line, |_, _, _| Ok(()))?;
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
        scan_record(record, line, |column, field, quoted| {
            let schema_index = header_plan.schema_indexes[column - 1];
            let data_type = table.schema()[schema_index].data_type;
            if quoted {
                let decoded = field.replace("\"\"", "\"");
                if data_type == DataType::String {
                    row.push(Value::String(decoded));
                } else {
                    row.push(parse_value(&decoded, data_type, line, column)?);
                }
            } else {
                row.push(parse_value(field, data_type, line, column)?);
            }
            Ok(())
        })?;
        value_count = next_value_count;
        rows.push(row);
    }

    table
        .prepare_projected_rows(header_plan.schema_indexes, rows)
        .map_err(Into::into)
}

/// Produces logical data records while retaining line endings inside quoted
/// fields. Quotes only open at the beginning of a field, matching the grammar
/// enforced later by [`scan_record`].
struct DataRecords<'a> {
    input: &'a str,
    offset: usize,
    next_line: usize,
}

impl<'a> DataRecords<'a> {
    const fn new(input: &'a str, first_line: usize) -> Self {
        Self {
            input,
            offset: 0,
            next_line: first_line,
        }
    }
}

impl<'a> Iterator for DataRecords<'a> {
    type Item = Result<(&'a str, usize), CsvIngestError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.input.len() {
            return None;
        }

        let bytes = self.input.as_bytes();
        let record_start = self.offset;
        let record_line = self.next_line;
        let mut cursor = record_start;
        let mut at_field_start = true;
        let mut in_quotes = false;

        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\r' => {
                    if bytes.get(cursor + 1) != Some(&b'\n') {
                        return Some(Err(CsvIngestError::InvalidLineEnding {
                            line: self.next_line,
                        }));
                    }
                    self.next_line = self.next_line.saturating_add(1);
                    if in_quotes {
                        cursor += 2;
                    } else {
                        let record = &self.input[record_start..cursor];
                        self.offset = cursor + 2;
                        return Some(Ok((record, record_line)));
                    }
                }
                b'\n' => {
                    self.next_line = self.next_line.saturating_add(1);
                    if in_quotes {
                        cursor += 1;
                    } else {
                        let record = &self.input[record_start..cursor];
                        self.offset = cursor + 1;
                        return Some(Ok((record, record_line)));
                    }
                }
                b'"' if in_quotes && bytes.get(cursor + 1) == Some(&b'"') => {
                    cursor += 2;
                }
                b'"' if in_quotes => {
                    in_quotes = false;
                    cursor += 1;
                }
                b'"' if at_field_start => {
                    in_quotes = true;
                    at_field_start = false;
                    cursor += 1;
                }
                b',' if !in_quotes => {
                    at_field_start = true;
                    cursor += 1;
                }
                _ => {
                    if !in_quotes {
                        at_field_start = false;
                    }
                    cursor += 1;
                }
            }
        }

        self.offset = bytes.len();
        Some(Ok((&self.input[record_start..], record_line)))
    }
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

struct HeaderPlan {
    schema_indexes: Vec<usize>,
}

fn validate_header(table: &Table, header: &str) -> Result<HeaderPlan, CsvIngestError> {
    if header.is_empty() {
        return Err(CsvIngestError::MissingHeader { line: 1 });
    }
    let expected_columns = table.schema().len();
    let actual_columns = field_count(header);
    if actual_columns > expected_columns {
        return Err(CsvIngestError::HeaderColumnCount {
            expected: expected_columns,
            actual: actual_columns,
        });
    }

    let schema_indexes_by_name = table
        .schema()
        .iter()
        .enumerate()
        .map(|(index, definition)| (definition.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut seen = vec![false; expected_columns];
    let mut schema_indexes = Vec::with_capacity(actual_columns);
    for (column, field) in header.split(',').enumerate() {
        let column = column + 1;
        reject_quoting(field, 1, column)?;
        let Some(&schema_index) = schema_indexes_by_name.get(field) else {
            if let Some(definition) = table
                .schema()
                .iter()
                .find(|definition| definition.name.eq_ignore_ascii_case(field))
            {
                return Err(CsvIngestError::HeaderMismatch {
                    column,
                    expected: definition.name.clone(),
                });
            }
            return Err(CsvIngestError::UnknownHeaderColumn {
                column,
                name: field.to_owned(),
            });
        };
        if std::mem::replace(&mut seen[schema_index], true) {
            return Err(CsvIngestError::DuplicateHeaderColumn {
                column,
                name: field.to_owned(),
            });
        }
        schema_indexes.push(schema_index);
    }

    Ok(HeaderPlan { schema_indexes })
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

/// Visits the syntactically valid fields in one logical data record.
///
/// Quoted field contents exclude the surrounding quotes but retain doubled
/// quotes so the caller can decode them only after limits and arity are known.
fn scan_record(
    record: &str,
    line: usize,
    mut visit: impl FnMut(usize, &str, bool) -> Result<(), CsvIngestError>,
) -> Result<usize, CsvIngestError> {
    let bytes = record.as_bytes();
    let mut field_start = 0_usize;
    let mut column = 0_usize;

    loop {
        column = column.saturating_add(1);
        if bytes.get(field_start) == Some(&b'"') {
            let contents_start = field_start + 1;
            let mut cursor = contents_start;
            loop {
                match bytes.get(cursor) {
                    Some(b'"') if bytes.get(cursor + 1) == Some(&b'"') => {
                        cursor += 2;
                    }
                    Some(b'"') => {
                        let delimiter = cursor + 1;
                        if delimiter < bytes.len() && bytes[delimiter] != b',' {
                            return Err(CsvIngestError::MalformedQuoting { line, column });
                        }
                        visit(column, &record[contents_start..cursor], true)?;
                        if delimiter == bytes.len() {
                            return Ok(column);
                        }
                        field_start = delimiter + 1;
                        break;
                    }
                    Some(_) => cursor += 1,
                    None => {
                        return Err(CsvIngestError::MalformedQuoting { line, column });
                    }
                }
            }
        } else {
            let field_end = record[field_start..]
                .as_bytes()
                .iter()
                .position(|byte| *byte == b',')
                .map_or(bytes.len(), |offset| field_start + offset);
            let field = &record[field_start..field_end];
            if field.contains('"') {
                return Err(CsvIngestError::MalformedQuoting { line, column });
            }
            visit(column, field, false)?;
            if field_end == bytes.len() {
                return Ok(column);
            }
            field_start = field_end + 1;
        }
    }
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
