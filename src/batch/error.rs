use std::fmt;

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
    UnionColumnCountMismatch {
        left: usize,
        right: usize,
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
            Self::UnionColumnCountMismatch { left, right } => write!(
                f,
                "UNION ALL column count mismatch: left operand has {left}, right operand has {right}"
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
        }
    }
}

impl std::error::Error for Error {}
