//! Engine-independent query types used by frontends such as the HTTP server.

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Instant,
};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{Database, Error, ResultSet, StatementResult, Value, database::ExecutionCancellation};

const EXECUTION_ACTIVE: u8 = 0;
const EXECUTION_CANCELLED: u8 = 1;
const EXECUTION_PUBLISHING: u8 = 2;

/// Maximum number of per-query records retained for inspection at once.
pub const MAX_ACTIVE_QUERY_ENTRIES: usize = 1_024;
/// Maximum UTF-8 byte length retained from a query's SQL text.
pub const MAX_OBSERVED_QUERY_BYTES: usize = 4_096;

/// A stable execution phase reported by query observability surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryPhase {
    Queued,
    Parsing,
    Planning,
    Scanning,
    Publishing,
}

impl QueryPhase {
    fn as_u8(self) -> u8 {
        self as u8
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Parsing,
            2 => Self::Planning,
            3 => Self::Scanning,
            4 => Self::Publishing,
            _ => Self::Queued,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Parsing => "parsing",
            Self::Planning => "planning",
            Self::Scanning => "scanning",
            Self::Publishing => "publishing",
        }
    }
}

/// A bounded point-in-time record for one executing query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveQuerySnapshot {
    /// Transport-assigned query identifier.
    pub query_id: u64,
    /// SQL text, truncated to [`MAX_OBSERVED_QUERY_BYTES`].
    pub query: String,
    /// Current engine execution phase.
    pub phase: QueryPhase,
    /// Monotonic time since engine execution began.
    pub elapsed_ms: u64,
    /// Logical rows considered by scans.
    pub scanned_rows: u64,
    /// Logical value bytes accessed by scans.
    pub scanned_bytes: u64,
    /// Largest accounted engine result allocation.
    pub peak_memory_bytes: u64,
    /// Bytes written by spill operators.
    pub spill_bytes: u64,
    /// Whether cooperative cancellation has been requested.
    pub cancelled: bool,
}

/// Monotonic engine counters and current query-registry gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EngineMetricsSnapshot {
    /// Exact number of queries currently executing.
    pub active_queries: u64,
    /// Number of active queries retained in the bounded registry.
    pub tracked_active_queries: u64,
    /// Queries whose engine execution began.
    pub queries_total: u64,
    /// Queries whose engine execution returned a result.
    pub queries_succeeded_total: u64,
    /// Queries whose engine execution returned an error.
    pub queries_failed_total: u64,
    /// Finished queries for which cancellation was requested.
    pub queries_cancelled_total: u64,
    /// Logical rows scanned by finished queries.
    pub scanned_rows_total: u64,
    /// Logical value bytes scanned by finished queries.
    pub scanned_bytes_total: u64,
    /// Process high-water mark for accounted query memory.
    pub peak_memory_bytes: u64,
    /// Bytes spilled by finished queries.
    pub spill_bytes_total: u64,
    /// Active query records omitted because the registry was full.
    pub dropped_active_query_records_total: u64,
}

/// The complete bounded payload shared by system tables and the HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservabilitySnapshot {
    /// Bounded records for currently executing queries.
    pub active_queries: Vec<ActiveQuerySnapshot>,
    /// Current gauges and process-lifetime counters.
    pub engine_metrics: EngineMetricsSnapshot,
}

#[derive(Debug, Default)]
struct EngineCounters {
    active_queries: AtomicU64,
    queries_total: AtomicU64,
    queries_succeeded: AtomicU64,
    queries_failed: AtomicU64,
    queries_cancelled: AtomicU64,
    scanned_rows: AtomicU64,
    scanned_bytes: AtomicU64,
    peak_memory_bytes: AtomicU64,
    spill_bytes: AtomicU64,
    dropped_active_query_records: AtomicU64,
}

#[derive(Debug)]
struct ActiveQueryState {
    query_id: u64,
    query: String,
    started: Instant,
    phase: AtomicU8,
    scanned_rows: AtomicU64,
    scanned_bytes: AtomicU64,
    peak_memory_bytes: AtomicU64,
    spill_bytes: AtomicU64,
    cancellation: QueryCancellation,
}

impl ActiveQueryState {
    fn snapshot(&self) -> ActiveQuerySnapshot {
        ActiveQuerySnapshot {
            query_id: self.query_id,
            query: self.query.clone(),
            phase: QueryPhase::from_u8(self.phase.load(Ordering::Acquire)),
            elapsed_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            scanned_rows: self.scanned_rows.load(Ordering::Relaxed),
            scanned_bytes: self.scanned_bytes.load(Ordering::Relaxed),
            peak_memory_bytes: self.peak_memory_bytes.load(Ordering::Relaxed),
            spill_bytes: self.spill_bytes.load(Ordering::Relaxed),
            cancelled: self.cancellation.is_cancelled(),
        }
    }
}

#[derive(Debug)]
struct QueryObservabilityInner {
    next_token: AtomicU64,
    max_active_entries: usize,
    active: Mutex<BTreeMap<u64, Arc<ActiveQueryState>>>,
    counters: EngineCounters,
}

/// Engine-owned, bounded query lifecycle registry.
#[derive(Debug, Clone)]
pub(crate) struct QueryObservability {
    inner: Arc<QueryObservabilityInner>,
}

impl Default for QueryObservability {
    fn default() -> Self {
        Self {
            inner: Arc::new(QueryObservabilityInner {
                next_token: AtomicU64::new(1),
                max_active_entries: MAX_ACTIVE_QUERY_ENTRIES,
                active: Mutex::new(BTreeMap::new()),
                counters: EngineCounters::default(),
            }),
        }
    }
}

impl QueryObservability {
    #[cfg(test)]
    fn with_max_active_entries(max_active_entries: usize) -> Self {
        Self {
            inner: Arc::new(QueryObservabilityInner {
                next_token: AtomicU64::new(1),
                max_active_entries,
                active: Mutex::new(BTreeMap::new()),
                counters: EngineCounters::default(),
            }),
        }
    }

    pub(crate) fn begin(
        &self,
        query_id: u64,
        sql: &str,
        cancellation: QueryCancellation,
    ) -> QueryObservation {
        saturating_increment(&self.inner.counters.active_queries, 1);
        saturating_increment(&self.inner.counters.queries_total, 1);
        let state = Arc::new(ActiveQueryState {
            query_id,
            query: truncate_utf8(sql, MAX_OBSERVED_QUERY_BYTES),
            started: Instant::now(),
            phase: AtomicU8::new(QueryPhase::Queued.as_u8()),
            scanned_rows: AtomicU64::new(0),
            scanned_bytes: AtomicU64::new(0),
            peak_memory_bytes: AtomicU64::new(0),
            spill_bytes: AtomicU64::new(0),
            cancellation,
        });
        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        let mut active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tracked = if active.len() < self.inner.max_active_entries {
            active.insert(token, Arc::clone(&state));
            Some(token)
        } else {
            saturating_increment(&self.inner.counters.dropped_active_query_records, 1);
            None
        };
        drop(active);
        QueryObservation {
            observability: self.clone(),
            state,
            tracked,
            completed: false,
        }
    }

    pub(crate) fn snapshot(&self) -> ObservabilitySnapshot {
        let active_queries = self.active_queries_snapshot();
        let tracked_active_queries = active_queries.len() as u64;
        ObservabilitySnapshot {
            engine_metrics: self.engine_metrics_snapshot(tracked_active_queries),
            active_queries,
        }
    }

    fn active_queries_snapshot(&self) -> Vec<ActiveQuerySnapshot> {
        let states = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        states.iter().map(|state| state.snapshot()).collect()
    }

    pub(crate) fn bounded_active_queries(
        &self,
        max_bytes: usize,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<(Vec<ActiveQuerySnapshot>, usize), ActiveSnapshotError> {
        let states = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut retained_bytes = states
            .len()
            .saturating_mul(std::mem::size_of::<ActiveQuerySnapshot>());
        for state in &states {
            if is_cancelled() {
                return Err(ActiveSnapshotError::Cancelled);
            }
            retained_bytes = retained_bytes.saturating_add(state.query.len());
            if retained_bytes > max_bytes {
                return Err(ActiveSnapshotError::LimitExceeded {
                    required: retained_bytes,
                });
            }
        }
        let mut queries = Vec::with_capacity(states.len());
        for state in states {
            if is_cancelled() {
                return Err(ActiveSnapshotError::Cancelled);
            }
            queries.push(state.snapshot());
        }
        Ok((queries, retained_bytes))
    }

    pub(crate) fn current_engine_metrics(&self) -> EngineMetricsSnapshot {
        let tracked_active_queries = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len() as u64;
        self.engine_metrics_snapshot(tracked_active_queries)
    }

    fn engine_metrics_snapshot(&self, tracked_active_queries: u64) -> EngineMetricsSnapshot {
        let counters = &self.inner.counters;
        EngineMetricsSnapshot {
            active_queries: counters.active_queries.load(Ordering::Relaxed),
            tracked_active_queries,
            queries_total: counters.queries_total.load(Ordering::Relaxed),
            queries_succeeded_total: counters.queries_succeeded.load(Ordering::Relaxed),
            queries_failed_total: counters.queries_failed.load(Ordering::Relaxed),
            queries_cancelled_total: counters.queries_cancelled.load(Ordering::Relaxed),
            scanned_rows_total: counters.scanned_rows.load(Ordering::Relaxed),
            scanned_bytes_total: counters.scanned_bytes.load(Ordering::Relaxed),
            peak_memory_bytes: counters.peak_memory_bytes.load(Ordering::Relaxed),
            spill_bytes_total: counters.spill_bytes.load(Ordering::Relaxed),
            dropped_active_query_records_total: counters
                .dropped_active_query_records
                .load(Ordering::Relaxed),
        }
    }
}

pub(crate) enum ActiveSnapshotError {
    Cancelled,
    LimitExceeded { required: usize },
}

pub(crate) struct QueryObservation {
    observability: QueryObservability,
    state: Arc<ActiveQueryState>,
    tracked: Option<u64>,
    completed: bool,
}

impl QueryObservation {
    pub(crate) fn set_phase(&self, phase: QueryPhase) {
        self.state.phase.store(phase.as_u8(), Ordering::Release);
    }

    pub(crate) fn add_scan(&self, rows: u64, bytes: u64) {
        saturating_increment(&self.state.scanned_rows, rows);
        saturating_increment(&self.state.scanned_bytes, bytes);
    }

    pub(crate) fn set_peak_memory(&self, bytes: u64) {
        self.state
            .peak_memory_bytes
            .fetch_max(bytes, Ordering::Relaxed);
    }

    fn finish(mut self, succeeded: bool) {
        self.complete(succeeded);
    }

    fn complete(&mut self, succeeded: bool) {
        if self.completed {
            return;
        }
        self.completed = true;
        if let Some(token) = self.tracked.take() {
            self.observability
                .inner
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&token);
        }
        let counters = &self.observability.inner.counters;
        saturating_decrement(&counters.active_queries);
        if succeeded {
            saturating_increment(&counters.queries_succeeded, 1);
        } else {
            saturating_increment(&counters.queries_failed, 1);
        }
        if self.state.cancellation.is_cancelled() {
            saturating_increment(&counters.queries_cancelled, 1);
        }
        let snapshot = self.state.snapshot();
        saturating_increment(&counters.scanned_rows, snapshot.scanned_rows);
        saturating_increment(&counters.scanned_bytes, snapshot.scanned_bytes);
        counters
            .peak_memory_bytes
            .fetch_max(snapshot.peak_memory_bytes, Ordering::Relaxed);
        saturating_increment(&counters.spill_bytes, snapshot.spill_bytes);
        write_query_log(&snapshot, succeeded);
    }
}

impl Drop for QueryObservation {
    fn drop(&mut self) {
        self.complete(false);
    }
}

#[derive(Serialize)]
struct QueryLog<'a> {
    event: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    query: &'a ActiveQuerySnapshot,
}

fn write_query_log(snapshot: &ActiveQuerySnapshot, succeeded: bool) {
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    write_query_log_ignoring_errors(&mut writer, snapshot, succeeded);
}

fn write_query_log_ignoring_errors(
    writer: &mut impl Write,
    snapshot: &ActiveQuerySnapshot,
    succeeded: bool,
) {
    let _ = write_query_log_to(writer, snapshot, succeeded);
}

fn write_query_log_to(
    writer: &mut impl Write,
    snapshot: &ActiveQuerySnapshot,
    succeeded: bool,
) -> io::Result<()> {
    let log = QueryLog {
        event: "query_finished",
        outcome: if snapshot.cancelled {
            "cancelled"
        } else if succeeded {
            "succeeded"
        } else {
            "failed"
        },
        query: snapshot,
    };
    let line = serde_json::to_vec(&log).map_err(io::Error::other)?;
    writer.write_all(&line)?;
    writer.write_all(b"\n")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    value[..end].to_owned()
}

fn saturating_increment(value: &AtomicU64, increment: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}

fn saturating_decrement(value: &AtomicU64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
}

/// The boxed future returned by [`QueryService`].
pub type QueryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<QueryResult, QueryError>> + Send + 'a>>;

/// A small interface between a query engine and a transport.
///
/// Implementations should periodically inspect `request.cancellation`, especially
/// around expensive scans and blocking boundaries. The HTTP server signals it when
/// a deadline expires or a bounded shutdown has to stop outstanding work. A service
/// that publishes mutations must call [`QueryCancellation::begin_publication`]
/// immediately before its irreversible publication step.
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

    /// Returns a bounded engine observability snapshot when supported.
    fn observability(&self) -> Option<ObservabilitySnapshot> {
        None
    }
}

impl QueryService for Database {
    fn execute(&self, request: QueryRequest) -> QueryFuture<'_> {
        Box::pin(async move {
            let observation = self.begin_observation(
                request.request_id,
                &request.sql,
                request.cancellation.clone(),
            );
            if request.cancellation.is_cancelled() {
                observation.finish(false);
                return Err(QueryError::unavailable("query was cancelled"));
            }

            let result = self
                .execute_observed(
                    &request.sql,
                    request.max_result_bytes,
                    &request.cancellation,
                    &observation,
                )
                .map_err(QueryError::from)?;
            if request.cancellation.is_cancelled() {
                observation.finish(false);
                return Err(QueryError::unavailable("query was cancelled"));
            }
            observation.finish(true);
            Ok(statement_result(result))
        })
    }

    fn observability(&self) -> Option<ObservabilitySnapshot> {
        Some(self.observability_snapshot())
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
    /// Maximum retained bytes the engine may materialize for this result.
    pub max_result_bytes: usize,
}

/// A cooperative cancellation signal scoped to one query.
#[derive(Debug, Clone)]
pub struct QueryCancellation {
    inner: CancellationToken,
    execution_state: Arc<AtomicU8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancellationOutcome {
    Cancelled,
    PublicationInProgress,
}

impl QueryCancellation {
    pub(crate) fn new(inner: CancellationToken) -> Self {
        Self {
            inner,
            execution_state: Arc::new(AtomicU8::new(EXECUTION_ACTIVE)),
        }
    }

    /// Waits until the server cancels this query.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await;
    }

    /// Returns whether cancellation has already been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Atomically hands a mutation from cancellable execution to publication.
    ///
    /// Mutating [`QueryService`] implementations must call this immediately before
    /// their irreversible commit or publication operation. A `false` result means
    /// cancellation won the handoff and the mutation must not be published. Once
    /// this returns `true`, an HTTP deadline reports `query_outcome_unknown` instead
    /// of an ordinary retryable timeout.
    #[must_use]
    pub fn begin_publication(&self) -> bool {
        if self.is_cancelled() {
            return false;
        }
        if self
            .execution_state
            .compare_exchange(
                EXECUTION_ACTIVE,
                EXECUTION_PUBLISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        !self.is_cancelled()
    }

    pub(crate) fn cancel(&self) -> CancellationOutcome {
        self.inner.cancel();
        match self.execution_state.compare_exchange(
            EXECUTION_ACTIVE,
            EXECUTION_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(EXECUTION_CANCELLED) => CancellationOutcome::Cancelled,
            Err(EXECUTION_PUBLISHING) => CancellationOutcome::PublicationInProgress,
            Err(_) => CancellationOutcome::Cancelled,
        }
    }

    pub(crate) fn publication_started(&self) -> bool {
        self.execution_state.load(Ordering::Acquire) == EXECUTION_PUBLISHING
    }
}

impl ExecutionCancellation for QueryCancellation {
    fn is_cancelled(&self) -> bool {
        QueryCancellation::is_cancelled(self)
    }

    fn begin_publication(&self) -> bool {
        QueryCancellation::begin_publication(self)
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
    /// A mutation was published, but its durability could not be confirmed.
    PublishedUncertain,
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
            Self::PublishedUncertain => "mutation_published_durability_uncertain",
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

    /// Creates an outcome for a published mutation with uncertain durability.
    pub fn published_uncertain(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::PublishedUncertain, message)
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
            Error::UnsupportedPlatform(_)
            | Error::DatabaseAlreadyOpen(_)
            | Error::CommitRecoveryRequired(_) => QueryErrorKind::Unavailable,
            Error::QueryCancelled => QueryErrorKind::Unavailable,
            Error::CommitDurabilityUncertain { .. } => QueryErrorKind::PublishedUncertain,
            Error::ReservedDatabasePath(_)
            | Error::UnsafeLockPath(_)
            | Error::GenerationOverflow
            | Error::CorruptSnapshot(_)
            | Error::Io { .. }
            | Error::LockPoisoned => QueryErrorKind::Internal,
        };
        Self::new(kind, error.to_string())
    }
}

#[cfg(test)]
mod observability_tests {
    use std::io;
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn concurrent_query_lifecycles_are_visible_and_accounted_once() {
        const QUERY_COUNT: usize = 8;
        let observability = QueryObservability::default();
        let started = Arc::new(Barrier::new(QUERY_COUNT + 1));
        let release = Arc::new(Barrier::new(QUERY_COUNT + 1));
        let mut cancellations = Vec::new();
        let mut workers = Vec::new();

        for query_id in 1..=QUERY_COUNT as u64 {
            let cancellation = QueryCancellation::new(CancellationToken::new());
            cancellations.push(cancellation.clone());
            let observability = observability.clone();
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            workers.push(std::thread::spawn(move || {
                let observation =
                    observability.begin(query_id, "SELECT * FROM events", cancellation.clone());
                observation.set_phase(QueryPhase::Scanning);
                observation.add_scan(query_id, query_id * 10);
                observation.set_peak_memory(query_id * 100);
                started.wait();
                release.wait();
                observation.finish(!cancellation.is_cancelled());
            }));
        }

        started.wait();
        let active = observability.snapshot();
        assert_eq!(active.active_queries.len(), QUERY_COUNT);
        assert_eq!(active.engine_metrics.active_queries, QUERY_COUNT as u64);
        assert!(
            active
                .active_queries
                .iter()
                .all(|query| query.phase == QueryPhase::Scanning && !query.cancelled)
        );

        assert_eq!(cancellations[0].cancel(), CancellationOutcome::Cancelled);
        assert!(
            observability
                .snapshot()
                .active_queries
                .iter()
                .find(|query| query.query_id == 1)
                .unwrap()
                .cancelled
        );
        release.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let completed = observability.snapshot();
        assert!(completed.active_queries.is_empty());
        assert_eq!(completed.engine_metrics.active_queries, 0);
        assert_eq!(completed.engine_metrics.queries_total, QUERY_COUNT as u64);
        assert_eq!(
            completed.engine_metrics.queries_succeeded_total,
            QUERY_COUNT as u64 - 1
        );
        assert_eq!(completed.engine_metrics.queries_failed_total, 1);
        assert_eq!(completed.engine_metrics.queries_cancelled_total, 1);
        assert_eq!(completed.engine_metrics.scanned_rows_total, 36);
        assert_eq!(completed.engine_metrics.scanned_bytes_total, 360);
        assert_eq!(completed.engine_metrics.peak_memory_bytes, 800);
    }

    #[test]
    fn observed_sql_is_truncated_on_a_utf8_boundary() {
        let sql = format!("{}é", "x".repeat(MAX_OBSERVED_QUERY_BYTES - 1));
        let truncated = truncate_utf8(&sql, MAX_OBSERVED_QUERY_BYTES);
        assert_eq!(truncated.len(), MAX_OBSERVED_QUERY_BYTES - 1);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn active_query_registry_enforces_its_cardinality_limit() {
        let observability = QueryObservability::with_max_active_entries(2);
        let observations = (1..=3)
            .map(|query_id| {
                observability.begin(
                    query_id,
                    "SELECT * FROM events",
                    QueryCancellation::new(CancellationToken::new()),
                )
            })
            .collect::<Vec<_>>();

        let snapshot = observability.snapshot();
        assert_eq!(snapshot.active_queries.len(), 2);
        assert_eq!(snapshot.engine_metrics.active_queries, 3);
        assert_eq!(snapshot.engine_metrics.tracked_active_queries, 2);
        assert_eq!(
            snapshot.engine_metrics.dropped_active_query_records_total,
            1
        );
        for observation in observations {
            observation.finish(true);
        }
    }

    struct FailingSink;

    impl io::Write for FailingSink {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed log pipe"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn query_logging_ignores_sink_errors_without_panicking() {
        let snapshot = ActiveQuerySnapshot {
            query_id: 9,
            query: "CREATE TABLE committed (id Int64)".to_owned(),
            phase: QueryPhase::Publishing,
            elapsed_ms: 1,
            scanned_rows: 0,
            scanned_bytes: 0,
            peak_memory_bytes: 0,
            spill_bytes: 0,
            cancelled: false,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_query_log_ignoring_errors(&mut FailingSink, &snapshot, true);
        }));
        assert!(result.is_ok());
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

    #[test]
    fn cancellation_and_publication_have_an_atomic_handoff() {
        let cancelled = QueryCancellation::new(CancellationToken::new());
        assert_eq!(cancelled.cancel(), CancellationOutcome::Cancelled);
        assert!(!cancelled.begin_publication());

        let publishing = QueryCancellation::new(CancellationToken::new());
        assert!(publishing.begin_publication());
        assert_eq!(
            publishing.cancel(),
            CancellationOutcome::PublicationInProgress
        );
    }

    #[test]
    fn published_database_commit_has_a_distinct_transport_outcome() {
        let error = QueryError::from(Error::CommitDurabilityUncertain {
            generation: 7,
            message: "directory sync failed".into(),
        });

        assert_eq!(error.kind, QueryErrorKind::PublishedUncertain);
        assert_eq!(error.kind.code(), "mutation_published_durability_uncertain");
        assert!(error.message.contains("generation 7 was published"));
    }
}
