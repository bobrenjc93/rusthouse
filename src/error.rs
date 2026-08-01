//! Error types shared by parsing, storage, and execution.

use std::fmt;

/// Errors returned by storage, parsing, and query execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// SQL could not be tokenized or parsed.
    Sql {
        /// Zero-based byte offset at which the error was detected.
        position: usize,
        /// Human-readable description of the syntax problem.
        message: String,
    },
    /// A table creation request reused an existing name.
    TableAlreadyExists(String),
    /// A requested table does not exist.
    TableNotFound(String),
    /// A view creation request reused an existing relation name.
    ViewAlreadyExists(String),
    /// A requested view does not exist.
    ViewNotFound(String),
    /// A data modification statement targeted a logical view.
    CannotModifyView(String),
    /// Logical view dependencies contain a recursive cycle.
    ViewDependencyCycle(Vec<String>),
    /// Logical view expansion exceeded the configured nesting limit.
    ViewExpansionLimit {
        /// Maximum number of nested views accepted by the resolver.
        limit: usize,
    },
    /// A schema contains the same column name more than once.
    DuplicateColumn(String),
    /// An identifier uses a reserved SQL literal.
    ReservedIdentifier {
        /// The rejected identifier.
        identifier: String,
        /// The syntactic role in which the identifier appeared.
        context: String,
    },
    /// A requested column does not exist in a table.
    ColumnNotFound {
        /// Name of the table that was searched.
        table: String,
        /// Name of the requested column.
        column: String,
    },
    /// An inserted row has the wrong number of values.
    RowLength {
        /// Name of the target table.
        table: String,
        /// Number of values required by the schema.
        expected: usize,
        /// Number of values supplied by the row.
        actual: usize,
    },
    /// A value or expression has an incompatible logical type.
    TypeMismatch {
        /// Operation or field for which the value was checked.
        context: String,
        /// Type or set of types accepted by the operation.
        expected: String,
        /// Type that was actually supplied.
        actual: String,
    },
    /// A syntactically valid query violates an execution rule.
    InvalidQuery(String),
    /// An integer aggregate exceeded its representable range.
    NumericOverflow(String),
}

/// A RustHouse operation result using [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql { position, message } => {
                write!(f, "SQL error at byte {position}: {message}")
            }
            Self::TableAlreadyExists(table) => write!(f, "table '{table}' already exists"),
            Self::TableNotFound(table) => write!(f, "table '{table}' does not exist"),
            Self::ViewAlreadyExists(view) => write!(f, "view '{view}' already exists"),
            Self::ViewNotFound(view) => write!(f, "view '{view}' does not exist"),
            Self::CannotModifyView(view) => {
                write!(f, "cannot modify view '{view}'; views are read-only")
            }
            Self::ViewDependencyCycle(path) => {
                write!(f, "view dependency cycle detected: {}", path.join(" -> "))
            }
            Self::ViewExpansionLimit { limit } => {
                write!(
                    f,
                    "view expansion exceeds the limit of {limit} nested views"
                )
            }
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
