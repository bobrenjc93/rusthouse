use std::fmt;

use crate::DataType;

/// Errors produced while defining or modifying a columnar table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A table schema did not contain any columns.
    EmptySchema,
    /// A schema contained the same column name more than once, ignoring case.
    DuplicateColumn(String),
    /// A table schema contained more columns than configured.
    ColumnLimitExceeded {
        /// The configured maximum number of columns.
        limit: usize,
        /// The number of columns supplied by the caller.
        actual: usize,
    },
    /// A requested column does not exist in the table.
    ColumnNotFound {
        /// The table that was searched.
        table: String,
        /// The requested column name.
        column: String,
    },
    /// A row contained a different number of values than its table schema.
    RowLength {
        /// The table receiving the row.
        table: String,
        /// The number of values required by the schema.
        expected: usize,
        /// The number of values supplied by the caller.
        actual: usize,
    },
    /// A row value did not have the type declared for its column.
    TypeMismatch {
        /// The table receiving the row.
        table: String,
        /// The column whose value had the wrong type.
        column: String,
        /// The type declared by the schema.
        expected: DataType,
        /// The supplied value's type.
        actual: DataType,
    },
    /// A `Float64` value was NaN or positive or negative infinity.
    NonFiniteFloat {
        /// The table receiving the row.
        table: String,
        /// The column receiving the value.
        column: String,
    },
    /// A string value was larger than the configured per-value limit.
    StringValueLimitExceeded {
        /// The table receiving the value.
        table: String,
        /// The column receiving the value.
        column: String,
        /// The configured maximum UTF-8 byte length.
        limit: usize,
        /// The supplied string's UTF-8 byte length.
        actual: usize,
    },
    /// An insert would exceed the configured row limit.
    RowLimitExceeded {
        /// The table receiving the rows.
        table: String,
        /// The configured maximum row count.
        limit: usize,
        /// The row count that the insert attempted to reach.
        actual: usize,
    },
    /// An insert would exceed the configured aggregate value-byte limit.
    ValueStorageLimitExceeded {
        /// The table receiving the values.
        table: String,
        /// The configured maximum aggregate value bytes.
        limit: usize,
        /// The aggregate byte count that the insert attempted to reach.
        actual: usize,
    },
    /// A row in a batch failed validation.
    BatchRow {
        /// The zero-based position of the invalid row within the batch.
        row_index: usize,
        /// The validation error produced for the invalid row.
        source: Box<Error>,
    },
}

/// A result returned by RustHouse's columnar storage APIs.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => formatter.write_str("a table must contain at least one column"),
            Self::DuplicateColumn(column) => write!(formatter, "duplicate column '{column}'"),
            Self::ColumnLimitExceeded { limit, actual } => write!(
                formatter,
                "table schema has {actual} columns, exceeding the {limit}-column limit"
            ),
            Self::ColumnNotFound { table, column } => {
                write!(
                    formatter,
                    "column '{column}' does not exist in table '{table}'"
                )
            }
            Self::RowLength {
                table,
                expected,
                actual,
            } => write!(
                formatter,
                "row for table '{table}' has {actual} values; expected {expected}"
            ),
            Self::TypeMismatch {
                table,
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "type mismatch for column '{table}.{column}': expected {expected}, found {actual}"
            ),
            Self::NonFiniteFloat { table, column } => write!(
                formatter,
                "column '{table}.{column}' cannot store a non-finite Float64"
            ),
            Self::StringValueLimitExceeded {
                table,
                column,
                limit,
                actual,
            } => write!(
                formatter,
                "string for column '{table}.{column}' is {actual} bytes, exceeding the {limit}-byte value limit"
            ),
            Self::RowLimitExceeded {
                table,
                limit,
                actual,
            } => write!(
                formatter,
                "insert would grow table '{table}' to {actual} rows, exceeding the {limit}-row limit"
            ),
            Self::ValueStorageLimitExceeded {
                table,
                limit,
                actual,
            } => write!(
                formatter,
                "insert would grow table '{table}' to {actual} value bytes, exceeding the {limit}-byte storage limit"
            ),
            Self::BatchRow { row_index, source } => {
                write!(formatter, "batch row {row_index} is invalid: {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BatchRow { source, .. } => Some(source),
            _ => None,
        }
    }
}
