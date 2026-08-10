use std::fmt;

use super::value::DataType;
#[cfg(unix)]
use super::wal::{Int64WriteAheadLogCommitError, Int64WriteAheadLogError};

/// Errors returned by storage, parsing, and query execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Sql {
        position: usize,
        message: String,
    },
    TableAlreadyExists(String),
    TableNotFound(String),
    DuplicateColumn(String),
    ReservedIdentifier {
        identifier: String,
        context: String,
    },
    InvalidIdentifier {
        identifier: String,
        context: String,
    },
    ColumnNotFound {
        table: String,
        column: String,
    },
    MissingInsertColumn {
        table: String,
        column: String,
    },
    RowLength {
        table: String,
        expected: usize,
        actual: usize,
    },
    SelectionIndexOutOfBounds {
        selection_position: usize,
        row_index: usize,
        input_rows: usize,
    },
    SelectionNotStrictlyIncreasing {
        selection_position: usize,
        previous_row_index: usize,
        row_index: usize,
    },
    TypeMismatch {
        context: String,
        expected: String,
        actual: String,
    },
    InvalidCast {
        source_type: DataType,
        target_type: DataType,
    },
    UnionColumnCountMismatch {
        left: usize,
        right: usize,
    },
    UnionDistinctColumnCountMismatch {
        left: usize,
        right: usize,
    },
    /// The selected expression currently requires non-nullable physical
    /// storage even though the column has the same logical scalar type.
    UnsupportedNullableOperation {
        table: String,
        column: String,
        operation: &'static str,
    },
    InvalidQuery(String),
    NumericOverflow(String),
    InsertOnlyStatementRequired {
        statement: &'static str,
    },
    StatementLimitExceeded {
        statements: usize,
        max_statements: usize,
    },
    ResultLimitExceeded {
        bytes: usize,
        max_bytes: usize,
    },
    ResourceLimitExceeded {
        resource: &'static str,
        actual: usize,
        max: usize,
    },
    /// A durable mutation could not be committed to the opted-in Int64 WAL.
    #[cfg(unix)]
    WriteAheadLog(Int64WriteAheadLogCommitError),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql { position, message } => {
                write!(f, "SQL error at byte {position}: {message}")
            }
            Self::TableAlreadyExists(table) => write!(f, "table '{table}' already exists"),
            Self::TableNotFound(table) => write!(f, "table '{table}' does not exist"),
            Self::DuplicateColumn(column) => write!(f, "duplicate column '{column}'"),
            Self::ReservedIdentifier {
                identifier,
                context,
            } => write!(
                f,
                "{context} {identifier:?} is reserved; TRUE and FALSE are Boolean literals"
            ),
            Self::InvalidIdentifier {
                identifier,
                context,
            } => write!(
                f,
                "{context} {identifier:?} is not a valid SQL identifier; expected [A-Za-z_][A-Za-z0-9_]*"
            ),
            Self::ColumnNotFound { table, column } => {
                write!(f, "column '{column}' does not exist in table '{table}'")
            }
            Self::MissingInsertColumn { table, column } => write!(
                f,
                "INSERT column list for table '{table}' is missing column '{column}'"
            ),
            Self::RowLength {
                table,
                expected,
                actual,
            } => write!(
                f,
                "row for table '{table}' has {actual} values; expected {expected}"
            ),
            Self::SelectionIndexOutOfBounds {
                selection_position,
                row_index,
                input_rows,
            } => write!(
                f,
                "row selection index {row_index} at position {selection_position} is out of bounds for {input_rows} input rows"
            ),
            Self::SelectionNotStrictlyIncreasing {
                selection_position,
                previous_row_index,
                row_index,
            } => write!(
                f,
                "row selection index {row_index} at position {selection_position} is not greater than the previous index {previous_row_index}"
            ),
            Self::TypeMismatch {
                context,
                expected,
                actual,
            } => write!(
                f,
                "type mismatch for {context}: expected {expected}, found {actual}"
            ),
            Self::InvalidCast {
                source_type,
                target_type,
            } => write!(
                f,
                "invalid {source_type} value for CAST({source_type} AS {target_type})"
            ),
            Self::UnionColumnCountMismatch { left, right } => write!(
                f,
                "UNION ALL column count mismatch: left operand has {left}, right operand has {right}"
            ),
            Self::UnionDistinctColumnCountMismatch { left, right } => write!(
                f,
                "UNION DISTINCT column count mismatch: left operand has {left}, right operand has {right}"
            ),
            Self::UnsupportedNullableOperation {
                table,
                column,
                operation,
            } => write!(
                f,
                "{operation} does not support nullable column '{column}' in table '{table}'"
            ),
            Self::InvalidQuery(message) => write!(f, "invalid query: {message}"),
            Self::NumericOverflow(operation) => {
                write!(f, "numeric overflow while computing {operation}")
            }
            Self::InsertOnlyStatementRequired { statement } => write!(
                f,
                "INSERT-only batch accepts only INSERT statements; found {statement}"
            ),
            Self::StatementLimitExceeded {
                statements,
                max_statements,
            } => write!(
                f,
                "SQL batch has at least {statements} statements, exceeding the limit of {max_statements}"
            ),
            Self::ResultLimitExceeded { bytes, max_bytes } => write!(
                f,
                "retained query results require at least {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::ResourceLimitExceeded {
                resource,
                actual,
                max,
            } => write!(
                f,
                "{resource} requires at least {actual}, exceeding the limit of {max}"
            ),
            #[cfg(unix)]
            Self::WriteAheadLog(error) => write!(f, "could not commit Int64 WAL record: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(unix)]
            Self::WriteAheadLog(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(unix)]
impl From<Int64WriteAheadLogError> for Error {
    fn from(error: Int64WriteAheadLogError) -> Self {
        Self::WriteAheadLog(error.into())
    }
}
