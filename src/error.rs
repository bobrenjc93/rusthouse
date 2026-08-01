use std::fmt;
use std::io;

use crate::DataType;

/// The resource whose configured transaction limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Rows,
    Bytes,
}

/// Errors returned by the database, batch, and execution APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse {
        message: String,
        position: usize,
    },
    Unsupported(String),
    TableAlreadyExists(String),
    TableNotFound(String),
    ColumnNotFound(String),
    AmbiguousColumn(String),
    DuplicateTableAlias(String),
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
    SnapshotLimitExceeded {
        resource: &'static str,
        limit: usize,
        attempted: usize,
    },
    UnsupportedPlatform(&'static str),
    DatabaseAlreadyOpen(String),
    ReservedDatabasePath(String),
    UnsafeLockPath(String),
    CommitDurabilityUncertain {
        generation: u64,
        message: String,
    },
    CommitRecoveryRequired(String),
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
    QueryCancelled,
    InvalidCapacity {
        capacity: usize,
    },
    CapacityExceeded {
        capacity: usize,
    },
    CapacityMismatch {
        column: usize,
        expected: usize,
        actual: usize,
    },
    LengthMismatch {
        column: usize,
        expected: usize,
        actual: usize,
    },
    SchemaMismatch {
        fields: usize,
        columns: usize,
    },
    BatchTypeMismatch {
        column: usize,
        expected: &'static str,
        actual: &'static str,
    },
    NullInNonNullableColumn {
        column: usize,
    },
    InvalidColumn {
        column: usize,
        columns: usize,
    },
    SelectionMismatch {
        expected_len: usize,
        actual_len: usize,
        expected_capacity: usize,
        actual_capacity: usize,
    },
    UnsupportedAggregate {
        aggregate: &'static str,
        data_type: &'static str,
    },
    InvalidAggregate {
        aggregate: &'static str,
        reason: &'static str,
    },
    ArithmeticOverflow {
        aggregate: &'static str,
    },
    MemoryLimitExceeded {
        operator: &'static str,
        required: usize,
        limit: usize,
    },
    ExecutionRowLimitExceeded {
        operator: &'static str,
        limit: usize,
        attempted: usize,
    },
    GroupLimitExceeded {
        max_groups: usize,
    },
    UnknownColumn(String),
    Type {
        operation: String,
        expected: String,
        actual: String,
    },
    Overflow {
        operation: String,
    },
    ExpressionTooDeep {
        limit: usize,
    },
    DivideByZero,
    InvalidCast {
        value: String,
        target: DataType,
    },
    InvalidArgument {
        function: String,
        message: String,
    },
    Aggregate(String),
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
            Self::Parse { message, position } => {
                write!(f, "SQL parse error at byte {position}: {message}")
            }
            Self::Unsupported(message) => write!(f, "unsupported operation: {message}"),
            Self::TableAlreadyExists(table) => write!(f, "table already exists: {table}"),
            Self::TableNotFound(table) => write!(f, "table not found: {table}"),
            Self::ColumnNotFound(column) => write!(f, "column not found: {column}"),
            Self::AmbiguousColumn(column) => write!(f, "column reference is ambiguous: {column}"),
            Self::DuplicateTableAlias(alias) => write!(f, "duplicate table alias: {alias}"),
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
            Self::UnknownColumn(name) => write!(f, "unknown column `{name}`"),
            Self::Type {
                operation,
                expected,
                actual,
            } => write!(
                f,
                "type error in {operation}: expected {expected}, got {actual}"
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
            Self::SnapshotLimitExceeded {
                resource,
                limit,
                attempted,
            } => write!(
                f,
                "snapshot {resource} limit exceeded: limit {limit}, attempted {attempted}"
            ),
            Self::UnsupportedPlatform(message) => {
                write!(f, "database persistence is unsupported: {message}")
            }
            Self::DatabaseAlreadyOpen(path) => {
                write!(f, "database is already open by another handle: {path}")
            }
            Self::ReservedDatabasePath(path) => write!(
                f,
                "database path uses the reserved internal-file namespace: {path}"
            ),
            Self::UnsafeLockPath(path) => {
                write!(f, "database lock path is not a regular file: {path}")
            }
            Self::CommitDurabilityUncertain {
                generation,
                message,
            } => write!(
                f,
                "generation {generation} was published but its durability is uncertain: {message}"
            ),
            Self::CommitRecoveryRequired(message) => {
                write!(
                    f,
                    "commit was not published and requires recovery: {message}"
                )
            }
            Self::GenerationOverflow => f.write_str("catalog generation counter overflowed"),
            Self::CorruptSnapshot(message) => write!(f, "corrupt snapshot: {message}"),
            Self::SnapshotTooLarge { size, maximum } => write!(
                f,
                "snapshot is too large: {size} bytes exceeds maximum {maximum} bytes"
            ),
            Self::Io { operation, message } => write!(f, "{operation}: {message}"),
            Self::LockPoisoned => f.write_str("database state lock is poisoned"),
            Self::QueryCancelled => f.write_str("query was cancelled before publication"),
            Self::InvalidCapacity { capacity } => {
                write!(f, "capacity {capacity} is not representable by this array")
            }
            Self::CapacityExceeded { capacity } => {
                write!(f, "fixed array capacity {capacity} exceeded")
            }
            Self::CapacityMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {column} has capacity {actual}, expected {expected}"
            ),
            Self::LengthMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {column} has length {actual}, expected {expected}"
            ),
            Self::SchemaMismatch { fields, columns } => write!(
                f,
                "schema has {fields} fields but batch has {columns} columns"
            ),
            Self::BatchTypeMismatch {
                column,
                expected,
                actual,
            } => write!(f, "column {column} has type {actual}, expected {expected}"),
            Self::NullInNonNullableColumn { column } => {
                write!(f, "non-nullable column {column} contains NULL")
            }
            Self::InvalidColumn { column, columns } => {
                write!(f, "column {column} is out of bounds for {columns} columns")
            }
            Self::SelectionMismatch {
                expected_len,
                actual_len,
                expected_capacity,
                actual_capacity,
            } => write!(
                f,
                "selection has len/capacity {actual_len}/{actual_capacity}, expected {expected_len}/{expected_capacity}"
            ),
            Self::UnsupportedAggregate {
                aggregate,
                data_type,
            } => write!(f, "{aggregate} does not support {data_type}"),
            Self::InvalidAggregate { aggregate, reason } => {
                write!(f, "invalid {aggregate} aggregate: {reason}")
            }
            Self::ArithmeticOverflow { aggregate } => {
                write!(f, "{aggregate} overflowed its accumulator")
            }
            Self::MemoryLimitExceeded {
                operator,
                required,
                limit,
            } => write!(
                f,
                "{operator} requires {required} retained bytes, limit is {limit}"
            ),
            Self::ExecutionRowLimitExceeded {
                operator,
                limit,
                attempted,
            } => write!(
                f,
                "{operator} row limit exceeded: limit {limit}, attempted {attempted}"
            ),
            Self::GroupLimitExceeded { max_groups } => {
                write!(f, "hash grouping exceeded its {max_groups} group limit")
            }
            Self::Overflow { operation } => write!(f, "numeric overflow in {operation}"),
            Self::ExpressionTooDeep { limit } => {
                write!(f, "expression exceeds the maximum depth of {limit}")
            }
            Self::DivideByZero => f.write_str("division by zero"),
            Self::InvalidCast { value, target } => {
                write!(f, "cannot cast {value} to {target}")
            }
            Self::InvalidArgument { function, message } => {
                write!(f, "invalid argument to {function}: {message}")
            }
            Self::Aggregate(message) => write!(f, "aggregate error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
