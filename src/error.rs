use std::fmt;

/// Errors returned by parsing, planning, and executing SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Sql(String),
    Unsupported(String),
    TableExists(String),
    TableNotFound(String),
    ColumnNotFound(String),
    DuplicateColumn(String),
    Type(String),
    Constraint(String),
    ResourceLimit {
        resource: &'static str,
        limit: usize,
        actual: usize,
    },
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(message) => write!(f, "SQL error: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported SQL: {message}"),
            Self::TableExists(name) => write!(f, "table already exists: {name}"),
            Self::TableNotFound(name) => write!(f, "table not found: {name}"),
            Self::ColumnNotFound(name) => write!(f, "column not found: {name}"),
            Self::DuplicateColumn(name) => write!(f, "duplicate column: {name}"),
            Self::Type(message) => write!(f, "type error: {message}"),
            Self::Constraint(message) => write!(f, "constraint violation: {message}"),
            Self::ResourceLimit {
                resource,
                limit,
                actual,
            } => write!(
                f,
                "{resource} limit exceeded: limit is {limit}, attempted {actual}"
            ),
            Self::Io(message) => write!(f, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
