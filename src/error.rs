use std::fmt;

use crate::DataType;

/// Errors produced while defining or mutating typed columnar tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// The table name is empty or contains only whitespace.
    EmptyTableName,
    /// The schema contains no columns.
    EmptySchema,
    /// A column name is empty or contains only whitespace.
    EmptyColumnName {
        /// The zero-based position of the invalid definition.
        index: usize,
    },
    /// Two schema fields use the same case-insensitive name.
    DuplicateColumn {
        /// The repeated name.
        name: String,
    },
    /// A requested column does not exist.
    ColumnNotFound {
        /// The table that was searched.
        table: String,
        /// The requested column name.
        column: String,
    },
    /// A row has a different number of values than the schema.
    RowLength {
        /// The table receiving the row.
        table: String,
        /// The number of schema fields.
        expected: usize,
        /// The number of supplied values.
        actual: usize,
    },
    /// A value does not have the type declared by its column.
    TypeMismatch {
        /// The table receiving the row.
        table: String,
        /// The target column name.
        column: String,
        /// The type declared in the schema.
        expected: DataType,
        /// The supplied value's type.
        actual: DataType,
    },
    /// A Float64 value is NaN or infinite.
    NonFiniteFloat {
        /// The table receiving the row.
        table: String,
        /// The target column name.
        column: String,
    },
    /// An insertion would exceed the table's configured row limit.
    RowLimitExceeded {
        /// The table receiving the row.
        table: String,
        /// The maximum permitted number of rows.
        limit: usize,
    },
}

/// A result returned by storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTableName => formatter.write_str("table name cannot be empty"),
            Self::EmptySchema => formatter.write_str("a table must contain at least one column"),
            Self::EmptyColumnName { index } => {
                write!(formatter, "column at index {index} has an empty name")
            }
            Self::DuplicateColumn { name } => write!(formatter, "duplicate column '{name}'"),
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
            Self::RowLimitExceeded { table, limit } => {
                write!(
                    formatter,
                    "table '{table}' reached its row limit of {limit}"
                )
            }
        }
    }
}

impl std::error::Error for StorageError {}
