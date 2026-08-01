//! Engine-independent query types used by frontends such as the HTTP server.

use std::{collections::HashSet, future::Future, pin::Pin};

use tokio_util::sync::CancellationToken;

use crate::{Database, Error, ResultSet, StatementResult, Value};

/// The boxed future returned by [`QueryService`].
pub type QueryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<QueryResult, QueryError>> + Send + 'a>>;

/// A small interface between a query engine and a transport.
///
/// Implementations should periodically inspect `request.cancellation`, especially
/// around expensive scans and blocking boundaries. The HTTP server signals it when
/// a deadline expires or a bounded shutdown has to stop outstanding work.
pub trait QueryService: Send + Sync + 'static {
    /// Executes one SQL statement and returns a materialized tabular result.
    ///
    /// This method must construct and return its future promptly. The HTTP server
    /// polls the future on a bounded blocking worker so a slow poll cannot starve
    /// its async I/O workers. Blocking or CPU-heavy implementations must still
    /// observe cancellation; otherwise they retain their execution slot until the
    /// poll returns.
    fn execute(&self, request: QueryRequest) -> QueryFuture<'_>;

    /// Returns current readiness. This method must not block.
    fn health(&self) -> ServiceHealth {
        ServiceHealth::ready()
    }
}

impl QueryService for Database {
    fn execute(&self, request: QueryRequest) -> QueryFuture<'_> {
        Box::pin(async move {
            if request.cancellation.is_cancelled() {
                return Err(QueryError::unavailable("query was cancelled"));
            }

            let result = Database::execute(self, &request.sql).map_err(QueryError::from)?;
            if request.cancellation.is_cancelled() {
                return Err(QueryError::unavailable("query was cancelled"));
            }
            Ok(statement_result(result))
        })
    }
}

fn statement_result(result: StatementResult) -> QueryResult {
    match result {
        StatementResult::Query(result) => result.into(),
        StatementResult::TableCreated => command_result("CREATE TABLE"),
        StatementResult::TableDropped => command_result("DROP TABLE"),
        StatementResult::RowsInserted { rows } => QueryResult::new(
            vec!["rows_affected".into()],
            vec![vec![QueryValue::Int64(
                i64::try_from(rows).expect("a row vector length fits in i64"),
            )]],
        ),
        StatementResult::TransactionStarted { generation } => {
            transaction_result("BEGIN", generation)
        }
        StatementResult::TransactionCommitted { generation } => {
            transaction_result("COMMIT", generation)
        }
        StatementResult::TransactionRolledBack => command_result("ROLLBACK"),
    }
}

fn command_result(command: &str) -> QueryResult {
    QueryResult::new(
        vec!["result".into()],
        vec![vec![QueryValue::String(command.into())]],
    )
}

fn transaction_result(command: &str, generation: u64) -> QueryResult {
    QueryResult::new(
        vec!["result".into(), "generation".into()],
        vec![vec![
            QueryValue::String(command.into()),
            QueryValue::String(generation.to_string()),
        ]],
    )
}

/// One query submitted by a transport.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// SQL text supplied by the client.
    pub sql: String,
    /// Server-assigned identifier also returned to the client.
    pub request_id: u64,
    /// Signal set if the transport no longer needs the result.
    pub cancellation: QueryCancellation,
}

/// A cooperative cancellation signal scoped to one query.
#[derive(Debug, Clone)]
pub struct QueryCancellation {
    inner: CancellationToken,
}

impl QueryCancellation {
    pub(crate) fn new(inner: CancellationToken) -> Self {
        Self { inner }
    }

    /// Waits until the server cancels this query.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await;
    }

    /// Returns whether cancellation has already been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    pub(crate) fn cancel(&self) {
        self.inner.cancel();
    }
}

/// A complete tabular query result.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Column names in display order.
    pub columns: Vec<String>,
    /// Rows whose values correspond positionally to `columns`.
    pub rows: Vec<Vec<QueryValue>>,
}

impl QueryResult {
    /// Creates a tabular result.
    pub fn new(columns: Vec<String>, rows: Vec<Vec<QueryValue>>) -> Self {
        Self { columns, rows }
    }

    /// Checks the transport-level invariants needed for row serialization.
    pub fn validate(&self) -> Result<(), QueryError> {
        let mut names = HashSet::with_capacity(self.columns.len());
        if let Some(column) = self.columns.iter().find(|column| !names.insert(*column)) {
            return Err(QueryError::internal(format!(
                "query service returned duplicate column name `{column}`"
            )));
        }
        if let Some((row_index, row)) = self
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.len() != self.columns.len())
        {
            return Err(QueryError::internal(format!(
                "query service returned {} values for {} columns in row {}",
                row.len(),
                self.columns.len(),
                row_index
            )));
        }
        Ok(())
    }
}

impl From<ResultSet> for QueryResult {
    fn from(result: ResultSet) -> Self {
        Self::new(
            result
                .columns
                .into_iter()
                .map(|column| column.name)
                .collect(),
            result
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(QueryValue::from).collect())
                .collect(),
        )
    }
}

/// A scalar value supported by the transport formats.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryValue {
    /// SQL `NULL`.
    Null,
    /// A boolean value.
    Boolean(bool),
    /// A signed 64-bit integer.
    Int64(i64),
    /// A 64-bit floating-point number.
    Float64(f64),
    /// A UTF-8 string.
    String(String),
}

impl From<Value> for QueryValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Int64(value) => Self::Int64(value),
            Value::Float64(value) => Self::Float64(value),
            Value::Bool(value) => Self::Boolean(value),
            Value::String(value) => Self::String(value),
        }
    }
}

/// Stable error categories that engines can return without depending on HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryErrorKind {
    /// SQL was invalid or unsupported.
    InvalidQuery,
    /// A referenced database object was not found.
    NotFound,
    /// The request conflicted with current state.
    Conflict,
    /// Execution exceeded an engine resource limit.
    ResourceLimit,
    /// The query service is temporarily unavailable.
    Unavailable,
    /// The service failed unexpectedly.
    Internal,
}

impl QueryErrorKind {
    /// Returns the stable protocol-neutral code for this category.
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidQuery => "invalid_query",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::ResourceLimit => "resource_limit",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

/// A typed query failure suitable for mapping onto multiple protocols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// Machine-readable category.
    pub kind: QueryErrorKind,
    /// Human-readable failure detail.
    pub message: String,
}

impl QueryError {
    /// Creates an error with an explicit category.
    pub fn new(kind: QueryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Creates an invalid-query error.
    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::InvalidQuery, message)
    }

    /// Creates a missing-object error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::NotFound, message)
    }

    /// Creates a state-conflict error.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Conflict, message)
    }

    /// Creates an engine resource-limit error.
    pub fn resource_limit(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::ResourceLimit, message)
    }

    /// Creates a temporary-unavailability error.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Unavailable, message)
    }

    /// Creates an internal service error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Internal, message)
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for QueryError {}

impl From<Error> for QueryError {
    fn from(error: Error) -> Self {
        let kind = match &error {
            Error::Parse { .. }
            | Error::Unsupported(_)
            | Error::DuplicateColumn(_)
            | Error::InvalidRow(_)
            | Error::TypeMismatch { .. }
            | Error::UnsupportedAggregate { .. }
            | Error::InvalidAggregate { .. }
            | Error::Type { .. }
            | Error::Overflow { .. }
            | Error::DivideByZero
            | Error::InvalidCast { .. }
            | Error::InvalidArgument { .. }
            | Error::Aggregate(_) => QueryErrorKind::InvalidQuery,
            Error::TableNotFound(_) | Error::ColumnNotFound(_) | Error::UnknownColumn(_) => {
                QueryErrorKind::NotFound
            }
            Error::TableAlreadyExists(_)
            | Error::TransactionAlreadyActive
            | Error::NoActiveTransaction
            | Error::Conflict { .. } => QueryErrorKind::Conflict,
            Error::TransactionLimitExceeded { .. }
            | Error::SnapshotLimitExceeded { .. }
            | Error::SnapshotTooLarge { .. }
            | Error::InvalidCapacity { .. }
            | Error::CapacityExceeded { .. }
            | Error::CapacityMismatch { .. }
            | Error::LengthMismatch { .. }
            | Error::SchemaMismatch { .. }
            | Error::BatchTypeMismatch { .. }
            | Error::NullInNonNullableColumn { .. }
            | Error::InvalidColumn { .. }
            | Error::SelectionMismatch { .. }
            | Error::ArithmeticOverflow { .. }
            | Error::MemoryLimitExceeded { .. }
            | Error::GroupLimitExceeded { .. }
            | Error::ExpressionTooDeep { .. } => QueryErrorKind::ResourceLimit,
            Error::DatabaseAlreadyOpen(_) | Error::CommitRecoveryRequired(_) => {
                QueryErrorKind::Unavailable
            }
            Error::ReservedDatabasePath(_)
            | Error::UnsafeLockPath(_)
            | Error::CommitDurabilityUncertain { .. }
            | Error::GenerationOverflow
            | Error::CorruptSnapshot(_)
            | Error::Io { .. }
            | Error::LockPoisoned => QueryErrorKind::Internal,
        };
        Self::new(kind, error.to_string())
    }
}

/// Readiness information returned by a query service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceHealth {
    /// Whether the service can currently accept queries.
    pub is_ready: bool,
    /// Optional detail when the service is not ready.
    pub message: Option<String>,
}

impl ServiceHealth {
    /// Creates a ready status.
    pub fn ready() -> Self {
        Self {
            is_ready: true,
            message: None,
        }
    }

    /// Creates a not-ready status with diagnostic detail.
    pub fn not_ready(message: impl Into<String>) -> Self {
        Self {
            is_ready: false,
            message: Some(message.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_column_names_before_object_serialization() {
        let result = QueryResult::new(
            vec!["id".into(), "id".into()],
            vec![vec![QueryValue::Int64(1), QueryValue::Int64(2)]],
        );

        let error = result.validate().unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Internal);
        assert!(error.message.contains("duplicate column name `id`"));
    }
}
