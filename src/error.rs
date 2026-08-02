use std::fmt;

use crate::DataType;

/// A configurable resource whose bound was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    InputBytes,
    RowsPerInsert,
    RowsPerTable,
    ResultRows,
    ColumnsPerTable,
    StringBytes,
    ExpressionDepth,
    ExpressionNodes,
    IntermediateRows,
    IntermediateBytes,
    ResultBytes,
    RequestTokens,
    RequestStatements,
    RequestResultRows,
    RequestResultBytes,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InputBytes => "input bytes",
            Self::RowsPerInsert => "rows per insert",
            Self::RowsPerTable => "rows per table",
            Self::ResultRows => "result rows",
            Self::ColumnsPerTable => "columns per table",
            Self::StringBytes => "string bytes",
            Self::ExpressionDepth => "expression depth",
            Self::ExpressionNodes => "expression nodes",
            Self::IntermediateRows => "intermediate rows",
            Self::IntermediateBytes => "intermediate bytes",
            Self::ResultBytes => "result bytes",
            Self::RequestTokens => "request tokens",
            Self::RequestStatements => "request statements",
            Self::RequestResultRows => "request result rows",
            Self::RequestResultBytes => "request result bytes",
        };
        f.write_str(name)
    }
}

/// Errors produced while parsing, validating, or executing SQL.
#[derive(Debug, Clone, PartialEq)]
pub enum DatabaseError {
    Parse {
        message: String,
        offset: usize,
    },
    TableAlreadyExists(String),
    TableNotFound(String),
    AmbiguousTable(String),
    ColumnAlreadyExists(String),
    ColumnNotFound(String),
    AmbiguousColumn(String),
    TypeMismatch {
        context: String,
        expected: DataType,
        actual: DataType,
    },
    InvalidQuery(String),
    InvalidValue(String),
    EmptyAggregate(String),
    LimitExceeded {
        kind: LimitKind,
        limit: usize,
        actual: usize,
    },
    ArithmeticOverflow(String),
    Io(String),
}

impl DatabaseError {
    pub(crate) fn parse(message: impl Into<String>, offset: usize) -> Self {
        Self::Parse {
            message: message.into(),
            offset,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidQuery(message.into())
    }
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { message, offset } => {
                write!(f, "SQL parse error at byte {offset}: {message}")
            }
            Self::TableAlreadyExists(name) => write!(f, "table already exists: {name}"),
            Self::TableNotFound(name) => write!(f, "table not found: {name}"),
            Self::AmbiguousTable(name) => write!(f, "ambiguous table: {name}"),
            Self::ColumnAlreadyExists(name) => write!(f, "column already exists: {name}"),
            Self::ColumnNotFound(name) => write!(f, "column not found: {name}"),
            Self::AmbiguousColumn(name) => write!(f, "ambiguous column: {name}"),
            Self::TypeMismatch {
                context,
                expected,
                actual,
            } => write!(
                f,
                "type mismatch for {context}: expected {expected}, got {actual}"
            ),
            Self::InvalidQuery(message) => write!(f, "invalid query: {message}"),
            Self::InvalidValue(message) => write!(f, "invalid value: {message}"),
            Self::EmptyAggregate(function) => {
                write!(f, "aggregate {function} has no input rows")
            }
            Self::LimitExceeded {
                kind,
                limit,
                actual,
            } => write!(f, "{kind} limit exceeded: limit {limit}, got {actual}"),
            Self::ArithmeticOverflow(context) => write!(f, "arithmetic overflow: {context}"),
            Self::Io(message) => write!(f, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for DatabaseError {}

impl From<std::io::Error> for DatabaseError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
