use std::fmt;
use std::io;

/// The resource whose configured transaction limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Rows,
    Bytes,
}

/// Errors returned by the database and SQL session APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse(String),
    Unsupported(String),
    TableAlreadyExists(String),
    TableNotFound(String),
    ColumnNotFound(String),
    DuplicateColumn(String),
    InvalidRow(String),
    TypeMismatch {
        column: String,
        expected: String,
        actual: String,
    },
    TransactionAlreadyActive,
    NoActiveTransaction,
    Conflict {
        table: String,
        base_generation: u64,
        current_generation: u64,
    },
    TransactionLimitExceeded {
        kind: LimitKind,
        limit: usize,
        attempted: usize,
    },
    GenerationOverflow,
    CorruptSnapshot(String),
    SnapshotTooLarge {
        size: u64,
        maximum: u64,
    },
    Io {
        operation: &'static str,
        message: String,
    },
    LockPoisoned,
}

impl Error {
    pub(crate) fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "SQL parse error: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported operation: {message}"),
            Self::TableAlreadyExists(table) => write!(f, "table already exists: {table}"),
            Self::TableNotFound(table) => write!(f, "table not found: {table}"),
            Self::ColumnNotFound(column) => write!(f, "column not found: {column}"),
            Self::DuplicateColumn(column) => write!(f, "duplicate column: {column}"),
            Self::InvalidRow(message) => write!(f, "invalid row: {message}"),
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "type mismatch for column {column}: expected {expected}, got {actual}"
            ),
            Self::TransactionAlreadyActive => f.write_str("a transaction is already active"),
            Self::NoActiveTransaction => f.write_str("no transaction is active"),
            Self::Conflict {
                table,
                base_generation,
                current_generation,
            } => write!(
                f,
                "transaction conflict on table {table}: snapshot generation {base_generation}, current generation {current_generation}"
            ),
            Self::TransactionLimitExceeded {
                kind,
                limit,
                attempted,
            } => write!(
                f,
                "transaction {kind:?} limit exceeded: limit {limit}, attempted {attempted}"
            ),
            Self::GenerationOverflow => f.write_str("catalog generation counter overflowed"),
            Self::CorruptSnapshot(message) => write!(f, "corrupt snapshot: {message}"),
            Self::SnapshotTooLarge { size, maximum } => write!(
                f,
                "snapshot is too large: {size} bytes exceeds maximum {maximum} bytes"
            ),
            Self::Io { operation, message } => write!(f, "{operation}: {message}"),
            Self::LockPoisoned => f.write_str("database state lock is poisoned"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
