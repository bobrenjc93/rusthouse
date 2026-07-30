//! Errors reported by public RustHouse operations.

use std::fmt;

/// Errors returned by storage, parsing, and query execution.
///
/// Each error owns its context and can outlive the database or input that
/// produced it. Mutation guarantees are documented on the operation that
/// returns the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// SQL text could not be tokenized or parsed.
    Sql {
        /// Zero-based UTF-8 byte offset in the original SQL input.
        position: usize,
        /// Human-readable description of the syntax problem.
        message: String,
    },
    /// A case-insensitively equivalent table name already exists.
    TableAlreadyExists(String),
    /// No table matches the requested case-insensitive name.
    TableNotFound(String),
    /// A schema repeats a column name, ignoring ASCII case.
    DuplicateColumn(String),
    /// An identifier uses a word reserved for a Boolean literal.
    ReservedIdentifier {
        /// The rejected identifier as supplied by the caller.
        identifier: String,
        /// The syntactic role in which the identifier was rejected.
        context: String,
    },
    /// No column matches the requested case-insensitive name.
    ColumnNotFound {
        /// Name of the table searched.
        table: String,
        /// Requested column name.
        column: String,
    },
    /// An inserted row does not have one value per schema field.
    RowLength {
        /// Name of the target table.
        table: String,
        /// Number of fields in the table schema.
        expected: usize,
        /// Number of values supplied by the row.
        actual: usize,
    },
    /// A value or expression has an incompatible type.
    TypeMismatch {
        /// Operation, expression, or column requiring the type.
        context: String,
        /// Required type or set of types.
        expected: String,
        /// Type actually supplied.
        actual: String,
    },
    /// A syntactically valid operation violates an execution constraint.
    InvalidQuery(String),
    /// Integer or floating-point aggregate state exceeded its supported
    /// finite range.
    NumericOverflow(String),
}

/// A convenience result whose error is [`Error`].
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
