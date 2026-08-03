//! Bounded ingestion for the one-column `CSVWithNames` subset.

use std::error::Error;
use std::fmt;

use crate::{InsertError, Int64Table};

/// Default maximum size of one CSV input, in bytes.
pub const DEFAULT_MAX_CSV_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum number of data records in one CSV input.
pub const DEFAULT_MAX_CSV_ROWS: usize = 100_000;

/// Resource bounds applied before and while ingesting CSV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvIngestLimits {
    /// Maximum number of bytes allowed in the complete input, including the header.
    pub max_bytes: usize,
    /// Maximum number of data records allowed after the header.
    pub max_rows: usize,
}

impl CsvIngestLimits {
    /// Creates explicit byte and data-record bounds.
    pub const fn new(max_bytes: usize, max_rows: usize) -> Self {
        Self {
            max_bytes,
            max_rows,
        }
    }
}

impl Default for CsvIngestLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CSV_BYTES, DEFAULT_MAX_CSV_ROWS)
    }
}

/// An error produced while ingesting one-column `CSVWithNames` input.
///
/// Line numbers are one-based physical input lines. The header is line 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvIngestError {
    /// The complete input exceeds the configured byte bound.
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
    /// The input is empty and therefore has no schema header.
    MissingHeader { line: usize },
    /// The header does not exactly match the table's column name.
    HeaderMismatch { line: usize, expected: String },
    /// A data record exceeds the configured row bound.
    RowLimitExceeded {
        line: usize,
        rows: usize,
        max_rows: usize,
    },
    /// An empty data record was encountered.
    EmptyRecord { line: usize },
    /// A data record contains a comma and therefore is not a single column.
    WrongColumnCount { line: usize, columns: usize },
    /// A data record is not `NULL` or a base-10 value in the `Int64` range.
    InvalidInt64 { line: usize },
    /// A `NULL` record was supplied to a non-nullable column.
    NullNotAllowed { line: usize, column: String },
    /// The validated batch could not be appended to the table.
    TableInsert(InsertError),
}

impl fmt::Display for CsvIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "CSV input is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::MissingHeader { line } => {
                write!(formatter, "missing CSVWithNames header at line {line}")
            }
            Self::HeaderMismatch { line, expected } => write!(
                formatter,
                "CSV header at line {line} does not match column '{expected}'"
            ),
            Self::RowLimitExceeded {
                line,
                rows,
                max_rows,
            } => write!(
                formatter,
                "CSV record at line {line} raises the row count to {rows}, exceeding the limit of {max_rows}"
            ),
            Self::EmptyRecord { line } => {
                write!(formatter, "empty CSV record at line {line}")
            }
            Self::WrongColumnCount { line, columns } => write!(
                formatter,
                "CSV record at line {line} has {columns} columns; expected 1"
            ),
            Self::InvalidInt64 { line } => write!(
                formatter,
                "CSV record at line {line} is not NULL or a decimal Int64"
            ),
            Self::NullNotAllowed { line, column } => write!(
                formatter,
                "CSV record at line {line} is NULL, but column '{column}' does not allow NULL values"
            ),
            Self::TableInsert(error) => write!(formatter, "could not append CSV batch: {error}"),
        }
    }
}

impl Error for CsvIngestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TableInsert(error) => Some(error),
            _ => None,
        }
    }
}

/// Atomically ingests a bounded, one-column `CSVWithNames` input.
///
/// The first line must exactly equal the table's column name. Each following
/// line must be an unquoted decimal `Int64` or the exact token `NULL`. Decimal
/// values may have a leading `+` or `-`. Both LF and CRLF line endings are
/// accepted, and the final line ending is optional. Empty records, whitespace,
/// quoting, and additional columns are rejected.
///
/// All records are parsed and validated into a bounded temporary batch before
/// [`Int64Table::append_batch`] is called. Any error therefore leaves the table
/// unchanged. On success, the returned count is the number of appended rows.
///
/// # Examples
///
/// ```
/// use rusthouse::{CsvIngestLimits, Int64Table, Schema, ingest_csv_with_names};
///
/// let mut table = Int64Table::new(Schema::int64("reading", true), 3);
/// let rows = ingest_csv_with_names(
///     &mut table,
///     "reading\n7\nNULL\n-2\n",
///     CsvIngestLimits::new(64, 3),
/// )?;
///
/// assert_eq!(rows, 3);
/// assert_eq!(table.values(), &[Some(7), None, Some(-2)]);
/// # Ok::<(), rusthouse::CsvIngestError>(())
/// ```
pub fn ingest_csv_with_names(
    table: &mut Int64Table,
    input: impl AsRef<[u8]>,
    limits: CsvIngestLimits,
) -> Result<usize, CsvIngestError> {
    let input = input.as_ref();
    if input.len() > limits.max_bytes {
        return Err(CsvIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: limits.max_bytes,
        });
    }
    if input.is_empty() {
        return Err(CsvIngestError::MissingHeader { line: 1 });
    }

    let mut lines = input.split_inclusive(|byte| *byte == b'\n');
    let header = line_contents(lines.next().expect("non-empty input has a line"));
    let expected_header = table.schema().column().name();
    if header != expected_header.as_bytes() {
        return Err(CsvIngestError::HeaderMismatch {
            line: 1,
            expected: expected_header.to_owned(),
        });
    }

    let mut values = Vec::new();
    for (offset, raw_line) in lines.enumerate() {
        let line = offset + 2;
        let rows = values.len().saturating_add(1);
        if rows > limits.max_rows {
            return Err(CsvIngestError::RowLimitExceeded {
                line,
                rows,
                max_rows: limits.max_rows,
            });
        }

        let value = parse_record(line_contents(raw_line), line)?;
        if value.is_none() && !table.schema().column().is_nullable() {
            return Err(CsvIngestError::NullNotAllowed {
                line,
                column: expected_header.to_owned(),
            });
        }
        values.push(value);
    }

    table
        .append_batch(&values)
        .map_err(CsvIngestError::TableInsert)?;
    Ok(values.len())
}

fn line_contents(line: &[u8]) -> &[u8] {
    let Some(without_lf) = line.strip_suffix(b"\n") else {
        return line;
    };
    without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
}

fn parse_record(record: &[u8], line: usize) -> Result<Option<i64>, CsvIngestError> {
    if record.is_empty() {
        return Err(CsvIngestError::EmptyRecord { line });
    }
    if record.contains(&b',') {
        return Err(CsvIngestError::WrongColumnCount {
            line,
            columns: record.iter().filter(|byte| **byte == b',').count() + 1,
        });
    }
    if record == b"NULL" {
        return Ok(None);
    }

    let digits = match record.first() {
        Some(b'+' | b'-') => &record[1..],
        _ => record,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(CsvIngestError::InvalidInt64 { line });
    }

    let decimal = std::str::from_utf8(record).expect("validated decimal is ASCII");
    decimal
        .parse::<i64>()
        .map(Some)
        .map_err(|_| CsvIngestError::InvalidInt64 { line })
}
