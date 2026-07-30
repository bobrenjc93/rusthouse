use std::fmt;

use crate::value::DataType;

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
    ColumnNotFound {
        table: String,
        column: String,
    },
    RowLength {
        table: String,
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        context: String,
        expected: String,
        actual: String,
    },
    IntegerOutOfRange {
        value: String,
        target: DataType,
        context: String,
    },
    InvalidQuery(String),
    NumericOverflow(String),
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
            Self::ColumnNotFound { table, column } => {
                write!(f, "column '{column}' does not exist in table '{table}'")
            }
            Self::RowLength {
                table,
                expected,
                actual,
            } => write!(
                f,
                "row for table '{table}' has {actual} values; expected {expected}"
            ),
            Self::TypeMismatch {
                context,
                expected,
                actual,
            } => write!(
                f,
                "type mismatch for {context}: expected {expected}, found {actual}"
            ),
            Self::IntegerOutOfRange {
                value,
                target,
                context,
            } => write!(
                f,
                "integer literal {value} is out of range for {target} in {context}"
            ),
            Self::InvalidQuery(message) => write!(f, "invalid query: {message}"),
            Self::NumericOverflow(operation) => {
                write!(f, "numeric overflow while computing {operation}")
            }
        }
    }
}

impl std::error::Error for Error {}
