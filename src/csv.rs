//! Bounded, streaming CSV output.
//!
//! [`CsvFormatter`] validates and writes one record at a time, so callers do
//! not need to collect a complete result set before exporting it.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};

/// Resource limits applied while formatting CSV output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvLimits {
    /// Maximum number of fields in the header or a data row.
    pub max_columns: usize,
    /// Maximum unescaped UTF-8 byte length of a header or data cell.
    pub max_cell_bytes: usize,
}

impl CsvLimits {
    /// Limits used by [`CsvFormatter::default`].
    pub const DEFAULT: Self = Self {
        max_columns: 1_024,
        max_cell_bytes: 1024 * 1024,
    };

    /// Creates CSV limits from a maximum column count and cell byte length.
    #[must_use]
    pub const fn new(max_columns: usize, max_cell_bytes: usize) -> Self {
        Self {
            max_columns,
            max_cell_bytes,
        }
    }
}

impl Default for CsvLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Identifies the record that caused a CSV validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvRecord {
    /// The named header record.
    Header,
    /// A zero-based data-row index.
    Row(usize),
}

impl fmt::Display for CsvRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => formatter.write_str("header"),
            Self::Row(index) => write!(formatter, "row {index}"),
        }
    }
}

/// A typed CSV validation or output failure.
#[derive(Debug)]
pub enum CsvError {
    /// A record has more fields than configured.
    ColumnLimitExceeded {
        /// The header or data row that exceeded the limit.
        record: CsvRecord,
        /// The configured maximum number of fields.
        limit: usize,
        /// The number of fields supplied by the caller.
        actual: usize,
    },
    /// A data row does not have the same width as the header.
    RowWidthMismatch {
        /// The zero-based data-row index.
        row: usize,
        /// The number of header fields.
        expected: usize,
        /// The number of fields in the data row.
        actual: usize,
    },
    /// A header or data cell is larger than configured.
    CellSizeLimitExceeded {
        /// The header or data row containing the cell.
        record: CsvRecord,
        /// The zero-based column index.
        column: usize,
        /// The configured maximum unescaped UTF-8 byte length.
        limit: usize,
        /// The cell's unescaped UTF-8 byte length.
        actual: usize,
    },
    /// The destination writer failed.
    Io(io::Error),
}

impl fmt::Display for CsvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ColumnLimitExceeded {
                record,
                limit,
                actual,
            } => write!(
                formatter,
                "CSV {record} has {actual} columns, exceeding the {limit}-column limit"
            ),
            Self::RowWidthMismatch {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "CSV row {row} has {actual} columns; expected {expected}"
            ),
            Self::CellSizeLimitExceeded {
                record,
                column,
                limit,
                actual,
            } => write!(
                formatter,
                "CSV {record}, column {column} is {actual} bytes, exceeding the {limit}-byte cell limit"
            ),
            Self::Io(error) => write!(formatter, "failed to write CSV output: {error}"),
        }
    }
}

impl Error for CsvError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CsvError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A bounded formatter for RFC 4180-style CSV records.
///
/// Records use commas and `\r\n` line endings. Empty fields and fields that
/// contain a comma, double quote, carriage return, or line feed are quoted;
/// double quotes inside a quoted field are doubled. Each complete record is
/// validated before it is streamed without an aggregate record buffer, so a
/// validation failure cannot write part of that record. Previously completed
/// records can already have reached the writer.
///
/// # Examples
///
/// ```
/// use rusthouse::{CsvFormatter, CsvLimits};
///
/// let header = ["name", "note"];
/// let rows = [["Ada", "hello, world"], ["Lin", ""]];
/// let mut output = Vec::new();
///
/// CsvFormatter::new(CsvLimits::new(2, 64))
///     .write(&mut output, &header, rows)?;
///
/// assert_eq!(output, b"name,note\r\nAda,\"hello, world\"\r\nLin,\"\"\r\n");
/// # Ok::<(), rusthouse::CsvError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvFormatter {
    limits: CsvLimits,
}

impl CsvFormatter {
    /// Creates a formatter with explicit resource limits.
    #[must_use]
    pub const fn new(limits: CsvLimits) -> Self {
        Self { limits }
    }

    /// Returns the formatter's configured limits.
    #[must_use]
    pub const fn limits(&self) -> CsvLimits {
        self.limits
    }

    /// Writes a named header followed by each row directly to `writer`.
    ///
    /// Every row must have exactly the same number of cells as `header`.
    /// Header names and cells are measured in unescaped UTF-8 bytes for the
    /// configured cell-size limit. The outer row iterator is consumed lazily.
    pub fn write<W, H, Rows, Row, Cell>(
        &self,
        writer: &mut W,
        header: &[H],
        rows: Rows,
    ) -> Result<(), CsvError>
    where
        W: Write + ?Sized,
        H: AsRef<str>,
        Rows: IntoIterator<Item = Row>,
        Row: AsRef<[Cell]>,
        Cell: AsRef<str>,
    {
        self.validate_record(CsvRecord::Header, header)?;
        write_record(writer, header)?;

        for (row_index, row) in rows.into_iter().enumerate() {
            let cells = row.as_ref();
            let record = CsvRecord::Row(row_index);
            self.check_column_count(record, cells.len())?;
            if cells.len() != header.len() {
                return Err(CsvError::RowWidthMismatch {
                    row: row_index,
                    expected: header.len(),
                    actual: cells.len(),
                });
            }
            self.validate_cells(record, cells)?;
            write_record(writer, cells)?;
        }

        Ok(())
    }

    fn check_column_count(&self, record: CsvRecord, actual: usize) -> Result<(), CsvError> {
        if actual > self.limits.max_columns {
            return Err(CsvError::ColumnLimitExceeded {
                record,
                limit: self.limits.max_columns,
                actual,
            });
        }
        Ok(())
    }

    fn validate_record<Cell: AsRef<str>>(
        &self,
        record: CsvRecord,
        cells: &[Cell],
    ) -> Result<(), CsvError> {
        self.check_column_count(record, cells.len())?;
        self.validate_cells(record, cells)
    }

    fn validate_cells<Cell: AsRef<str>>(
        &self,
        record: CsvRecord,
        cells: &[Cell],
    ) -> Result<(), CsvError> {
        for (column, cell) in cells.iter().enumerate() {
            let cell = cell.as_ref();
            if cell.len() > self.limits.max_cell_bytes {
                return Err(CsvError::CellSizeLimitExceeded {
                    record,
                    column,
                    limit: self.limits.max_cell_bytes,
                    actual: cell.len(),
                });
            }
        }
        Ok(())
    }
}

impl Default for CsvFormatter {
    fn default() -> Self {
        Self::new(CsvLimits::default())
    }
}

fn write_record<W: Write + ?Sized, Cell: AsRef<str>>(
    writer: &mut W,
    cells: &[Cell],
) -> io::Result<()> {
    for (column, cell) in cells.iter().enumerate() {
        if column != 0 {
            writer.write_all(b",")?;
        }
        write_field(writer, cell.as_ref())?;
    }
    writer.write_all(b"\r\n")
}

fn write_field<W: Write + ?Sized>(writer: &mut W, field: &str) -> io::Result<()> {
    let bytes = field.as_bytes();
    let quote = bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));

    if !quote {
        return writer.write_all(bytes);
    }

    writer.write_all(b"\"")?;
    let mut start = 0;
    while let Some(relative_end) = bytes[start..].iter().position(|byte| *byte == b'"') {
        let end = start + relative_end;
        writer.write_all(&bytes[start..end])?;
        writer.write_all(b"\"\"")?;
        start = end + 1;
    }
    writer.write_all(&bytes[start..])?;
    writer.write_all(b"\"")
}
