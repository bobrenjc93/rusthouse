use std::fmt;

use crate::DataType;

/// Errors produced while defining or modifying a columnar table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A table schema did not contain any columns.
    EmptySchema,
    /// A schema contained the same column name more than once, ignoring case.
    DuplicateColumn(String),
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
