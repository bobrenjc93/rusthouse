use std::fmt;

/// Identifies a row resource controlled by [`crate::ExecutionLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLimit {
    ScanRows,
    OutputRows,
}

impl fmt::Display for ExecutionLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScanRows => f.write_str("scan row"),
            Self::OutputRows => f.write_str("output row"),
        }
    }
}

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
    InvalidQuery(String),
    NumericOverflow(String),
    ExecutionLimitExceeded {
        limit: ExecutionLimit,
        maximum: usize,
        actual: usize,
    },
    ExecutionCancelled,
    DeadlineExceeded,
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
            Self::InvalidQuery(message) => write!(f, "invalid query: {message}"),
            Self::NumericOverflow(operation) => {
                write!(f, "numeric overflow while computing {operation}")
            }
            Self::ExecutionLimitExceeded {
                limit,
                maximum,
                actual,
            } => write!(
                f,
                "execution {limit} limit exceeded: maximum {maximum}, attempted {actual}"
            ),
            Self::ExecutionCancelled => f.write_str("query execution was cancelled"),
            Self::DeadlineExceeded => f.write_str("query execution deadline exceeded"),
        }
    }
}

impl std::error::Error for Error {}
