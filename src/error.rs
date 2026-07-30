use std::fmt;

/// Errors returned by storage, parsing, and query execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The SQL text could not be parsed.
    Sql {
        /// Zero-based byte offset at which parsing failed.
        position: usize,
        /// Human-readable description of the invalid syntax.
        message: String,
    },
    /// A table with the supplied name already exists.
    TableAlreadyExists(String),
    /// No table exists with the supplied name.
    TableNotFound(String),
    /// A table schema contains a duplicate column name.
    DuplicateColumn(String),
    /// An identifier conflicts with a reserved SQL literal.
    ReservedIdentifier {
        /// The rejected identifier.
        identifier: String,
        /// The syntactic role in which it was used.
        context: String,
    },
    /// No column exists with the supplied name.
    ColumnNotFound {
        /// The table searched for the column.
        table: String,
        /// The requested column name.
        column: String,
    },
    /// An inserted row does not match the table width.
    RowLength {
        /// The target table name.
        table: String,
        /// The number of values required by the schema.
        expected: usize,
        /// The number of values supplied.
        actual: usize,
    },
    /// A value or expression has an incompatible data type.
    TypeMismatch {
        /// The column or operation requiring a particular type.
        context: String,
        /// The required type.
        expected: String,
        /// The supplied type.
        actual: String,
    },
    /// A parsed query violates an execution rule.
    InvalidQuery(String),
    /// An aggregate calculation exceeded its numeric representation.
    NumericOverflow(String),
}

/// A result whose error type is [`Error`].
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
        }
    }
}

impl std::error::Error for Error {}
