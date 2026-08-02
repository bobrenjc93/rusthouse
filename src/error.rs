use std::fmt;

use crate::storage::InsertError;

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
    /// A schema contains the same, case-insensitive column name twice.
    DuplicateColumn { name: String },
    /// The catalog already contains the case-insensitive table name.
    TableAlreadyExists { name: String },
    /// An insertion targeted a table that is not registered.
    TableNotFound { name: String },
    /// A parsed insertion batch did not match its target table.
    Insert(InsertError),
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
            Self::DuplicateColumn { name } => {
                write!(formatter, "duplicate column {name:?}")
            }
            Self::TableAlreadyExists { name } => {
                write!(formatter, "table {name:?} already exists")
            }
            Self::TableNotFound { name } => write!(formatter, "table {name:?} does not exist"),
            Self::Insert(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {}

impl From<InsertError> for Error {
    fn from(error: InsertError) -> Self {
        Self::Insert(error)
    }
}
