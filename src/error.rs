use std::fmt;

/// An error returned while parsing or executing SQL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// The SQL input exceeded the configured byte limit.
    InputTooLarge { actual: usize, maximum: usize },
    /// The statement did not match the supported SQL grammar.
    Syntax { position: usize, message: String },
    /// A column used a type that RustHouse does not support.
    UnknownType { name: String, position: usize },
    /// A statement declared more columns than the configured limit.
    TooManyColumns { actual: usize, maximum: usize },
    /// A `SELECT` requested more output columns than the configured limit.
    TooManyProjectedColumns { actual: usize, maximum: usize },
    /// One query would materialize more cells than the configured limit.
    ResultTooLarge { actual: usize, maximum: usize },
    /// One query would materialize more bytes than the configured limit.
    ResultBytesTooLarge { actual: usize, maximum: usize },
    /// Collected query results in one batch exceeded their cumulative limit.
    BatchResultTooLarge { actual: usize, maximum: usize },
    /// Collected query results in one batch exceeded their cumulative byte limit.
    BatchResultBytesTooLarge { actual: usize, maximum: usize },
    /// A schema contains the same, case-insensitive column name twice.
    DuplicateColumn { name: String },
    /// The catalog already contains the case-insensitive table name.
    TableAlreadyExists { name: String },
    /// A statement referenced a table that is not in the catalog.
    TableNotFound { name: String },
    /// A `SELECT` referenced a column that is not in its table.
    ColumnNotFound { table: String, column: String },
    /// A literal could not be represented by its SQL type.
    InvalidLiteral {
        value: String,
        position: usize,
        expected: &'static str,
    },
    /// Typed storage rejected an insertion batch.
    Insert(crate::InsertError),
}

/// A result returned by RustHouse operations.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { actual, maximum } => write!(
                formatter,
                "SQL input is {actual} bytes, exceeding the limit of {maximum} bytes"
            ),
            Self::Syntax { position, message } => {
                write!(formatter, "SQL error at byte {position}: {message}")
            }
            Self::UnknownType { name, position } => {
                write!(formatter, "unknown data type {name:?} at byte {position}")
            }
            Self::TooManyColumns { actual, maximum } => write!(
                formatter,
                "table has at least {actual} columns, exceeding the limit of {maximum}"
            ),
            Self::TooManyProjectedColumns { actual, maximum } => write!(
                formatter,
                "projection has {actual} columns, exceeding the limit of {maximum}"
            ),
            Self::ResultTooLarge { actual, maximum } => write!(
                formatter,
                "query result has {actual} cells, exceeding the limit of {maximum}"
            ),
            Self::ResultBytesTooLarge { actual, maximum } => write!(
                formatter,
                "query result requires {actual} bytes, exceeding the limit of {maximum}"
            ),
            Self::BatchResultTooLarge { actual, maximum } => write!(
                formatter,
                "batch results have {actual} cells, exceeding the limit of {maximum}"
            ),
            Self::BatchResultBytesTooLarge { actual, maximum } => write!(
                formatter,
                "batch results require {actual} bytes, exceeding the limit of {maximum}"
            ),
            Self::DuplicateColumn { name } => {
                write!(formatter, "duplicate column {name:?}")
            }
            Self::TableAlreadyExists { name } => {
                write!(formatter, "table {name:?} already exists")
            }
            Self::TableNotFound { name } => write!(formatter, "table {name:?} does not exist"),
            Self::ColumnNotFound { table, column } => {
                write!(
                    formatter,
                    "column {column:?} does not exist in table {table:?}"
                )
            }
            Self::InvalidLiteral {
                value,
                position,
                expected,
            } => write!(
                formatter,
                "invalid {expected} literal {value:?} at byte {position}"
            ),
            Self::Insert(error) => write!(formatter, "insert failed: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::InsertError> for Error {
    fn from(error: crate::InsertError) -> Self {
        Self::Insert(error)
    }
}
