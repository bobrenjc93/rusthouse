//! Bounded execution telemetry and query-log configuration.

use std::collections::VecDeque;
use std::time::Duration;

use crate::error::Error;

/// Number of completed executions retained by default.
pub const DEFAULT_QUERY_LOG_CAPACITY: usize = 128;

/// Controls whether SQL source text is retained in query-log entries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SqlTextRetention {
    /// Do not retain SQL text. This is the default.
    #[default]
    Disabled,
    /// Retain at most this many UTF-8 bytes from the start of each execution.
    Truncate(usize),
}

/// Configuration for in-engine execution telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// Maximum number of completed executions retained in `system.query_log`.
    /// A capacity of zero disables the log without disabling aggregate counters.
    pub query_log_capacity: usize,
    /// SQL source-text retention policy for new query-log entries.
    pub sql_text_retention: SqlTextRetention,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            query_log_capacity: DEFAULT_QUERY_LOG_CAPACITY,
            sql_text_retention: SqlTextRetention::Disabled,
        }
    }
}

/// Counters collected while one [`crate::Database::execute`] call runs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionMetrics {
    /// Source rows considered by scans.
    pub rows_scanned: u64,
    /// Source rows that passed their predicate.
    pub rows_matched: u64,
    /// Aggregate groups materialized, including a global aggregate group.
    pub groups_created: u64,
    /// Rows successfully appended by inserts.
    pub rows_written: u64,
    /// Rows returned across all query results.
    pub result_rows: u64,
}

impl ExecutionMetrics {
    pub(crate) fn add(&mut self, other: Self) {
        self.rows_scanned = self.rows_scanned.saturating_add(other.rows_scanned);
        self.rows_matched = self.rows_matched.saturating_add(other.rows_matched);
        self.groups_created = self.groups_created.saturating_add(other.groups_created);
        self.rows_written = self.rows_written.saturating_add(other.rows_written);
        self.result_rows = self.result_rows.saturating_add(other.result_rows);
    }
}

/// Stable category for an execution failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryFailure {
    /// SQL tokenization or parsing failed.
    Sql,
    /// A table creation reused an existing name.
    TableAlreadyExists,
    /// A requested table did not exist.
    TableNotFound,
    /// A schema contained a duplicate column.
    DuplicateColumn,
    /// An identifier used a reserved name.
    ReservedIdentifier,
    /// A requested column did not exist.
    ColumnNotFound,
    /// An inserted row had the wrong width.
    RowLength,
    /// A value or expression had an incompatible type.
    TypeMismatch,
    /// A query violated an execution rule.
    InvalidQuery,
    /// A numeric aggregate overflowed.
    NumericOverflow,
    /// A write targeted the read-only system namespace.
    ReadOnlySystemTable,
}

impl QueryFailure {
    /// Returns the stable name exposed in `system.query_log.failure_type`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sql => "Sql",
            Self::TableAlreadyExists => "TableAlreadyExists",
            Self::TableNotFound => "TableNotFound",
            Self::DuplicateColumn => "DuplicateColumn",
            Self::ReservedIdentifier => "ReservedIdentifier",
            Self::ColumnNotFound => "ColumnNotFound",
            Self::RowLength => "RowLength",
            Self::TypeMismatch => "TypeMismatch",
            Self::InvalidQuery => "InvalidQuery",
            Self::NumericOverflow => "NumericOverflow",
            Self::ReadOnlySystemTable => "ReadOnlySystemTable",
        }
    }
}

impl From<&Error> for QueryFailure {
    fn from(error: &Error) -> Self {
        match error {
            Error::Sql { .. } => Self::Sql,
            Error::TableAlreadyExists(_) => Self::TableAlreadyExists,
            Error::TableNotFound(_) => Self::TableNotFound,
            Error::DuplicateColumn(_) => Self::DuplicateColumn,
            Error::ReservedIdentifier { .. } => Self::ReservedIdentifier,
            Error::ColumnNotFound { .. } => Self::ColumnNotFound,
            Error::RowLength { .. } => Self::RowLength,
            Error::TypeMismatch { .. } => Self::TypeMismatch,
            Error::InvalidQuery(_) => Self::InvalidQuery,
            Error::NumericOverflow(_) => Self::NumericOverflow,
            Error::ReadOnlySystemTable(_) => Self::ReadOnlySystemTable,
        }
    }
}

/// Typed completion status for a query-log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStatus {
    /// The complete execution succeeded.
    Succeeded,
    /// The execution returned the contained failure category.
    Failed(QueryFailure),
}

impl QueryStatus {
    /// Returns the stable status name exposed in `system.query_log.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "Succeeded",
            Self::Failed(_) => "Failed",
        }
    }

    /// Returns the failure category, if the execution failed.
    #[must_use]
    pub fn failure(self) -> Option<QueryFailure> {
        match self {
            Self::Succeeded => None,
            Self::Failed(failure) => Some(failure),
        }
    }
}

/// Telemetry retained for one completed execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryLogEntry {
    /// Monotonically increasing identifier scoped to this database instance.
    pub query_id: u64,
    /// Total elapsed wall-clock time measured with a monotonic clock.
    pub elapsed: Duration,
    /// Work counters accumulated before completion or failure.
    pub metrics: ExecutionMetrics,
    /// Typed success or failure status.
    pub status: QueryStatus,
    /// Retained SQL prefix, or `None` when SQL retention was disabled.
    pub sql_text: Option<String>,
    /// Whether `sql_text` was shortened to satisfy its configured byte bound.
    pub sql_text_truncated: bool,
}

/// Lifetime aggregate counters for completed executions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryCounters {
    /// Total completed executions, successful or failed.
    pub executions: u64,
    /// Executions that completed successfully.
    pub successful_executions: u64,
    /// Executions that returned an error.
    pub failed_executions: u64,
    /// Sum of elapsed time for all completed executions.
    pub elapsed: Duration,
    /// Sum of per-execution work counters.
    pub metrics: ExecutionMetrics,
}

#[derive(Debug)]
pub(crate) struct Telemetry {
    config: TelemetryConfig,
    next_query_id: u64,
    counters: TelemetryCounters,
    query_log: VecDeque<QueryLogEntry>,
}

impl Telemetry {
    pub(crate) fn new(config: TelemetryConfig) -> Self {
        Self {
            config,
            next_query_id: 1,
            counters: TelemetryCounters::default(),
            query_log: VecDeque::new(),
        }
    }

    pub(crate) fn config(&self) -> TelemetryConfig {
        self.config
    }

    pub(crate) fn set_config(&mut self, config: TelemetryConfig) {
        self.config = config;
        while self.query_log.len() > config.query_log_capacity {
            self.query_log.pop_front();
        }
        for entry in &mut self.query_log {
            apply_sql_retention(entry, config.sql_text_retention);
        }
    }

    pub(crate) fn counters(&self) -> &TelemetryCounters {
        &self.counters
    }

    pub(crate) fn query_log(&self) -> &VecDeque<QueryLogEntry> {
        &self.query_log
    }

    pub(crate) fn begin(&mut self) -> u64 {
        let query_id = self.next_query_id;
        self.next_query_id = self.next_query_id.saturating_add(1);
        query_id
    }

    pub(crate) fn record(
        &mut self,
        query_id: u64,
        elapsed: Duration,
        metrics: ExecutionMetrics,
        status: QueryStatus,
        sql: &str,
    ) {
        self.counters.executions = self.counters.executions.saturating_add(1);
        match status {
            QueryStatus::Succeeded => {
                self.counters.successful_executions =
                    self.counters.successful_executions.saturating_add(1);
            }
            QueryStatus::Failed(_) => {
                self.counters.failed_executions = self.counters.failed_executions.saturating_add(1);
            }
        }
        self.counters.elapsed = self.counters.elapsed.saturating_add(elapsed);
        self.counters.metrics.add(metrics);

        if self.config.query_log_capacity == 0 {
            return;
        }
        if self.query_log.len() == self.config.query_log_capacity {
            self.query_log.pop_front();
        }
        let (sql_text, sql_text_truncated) = retain_sql(sql, self.config.sql_text_retention);
        self.query_log.push_back(QueryLogEntry {
            query_id,
            elapsed,
            metrics,
            status,
            sql_text,
            sql_text_truncated,
        });
    }
}

fn apply_sql_retention(entry: &mut QueryLogEntry, retention: SqlTextRetention) {
    match retention {
        SqlTextRetention::Disabled => {
            entry.sql_text = None;
            entry.sql_text_truncated = false;
        }
        SqlTextRetention::Truncate(max_bytes) => {
            if let Some(sql) = entry.sql_text.as_deref() {
                let (retained, truncated) = truncate_utf8(sql, max_bytes);
                entry.sql_text = Some(retained.to_owned());
                entry.sql_text_truncated |= truncated;
            }
        }
    }
}

fn retain_sql(sql: &str, retention: SqlTextRetention) -> (Option<String>, bool) {
    match retention {
        SqlTextRetention::Disabled => (None, false),
        SqlTextRetention::Truncate(max_bytes) => {
            let (retained, truncated) = truncate_utf8(sql, max_bytes);
            (Some(retained.to_owned()), truncated)
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let (retained, truncated) = retain_sql("SELECT '\u{e9}'", SqlTextRetention::Truncate(9));
        assert_eq!(retained.as_deref(), Some("SELECT '"));
        assert!(truncated);
    }

    #[test]
    fn reducing_configuration_evicts_and_redacts_existing_entries() {
        let mut telemetry = Telemetry::new(TelemetryConfig {
            query_log_capacity: 2,
            sql_text_retention: SqlTextRetention::Truncate(100),
        });
        let first_id = telemetry.begin();
        telemetry.record(
            first_id,
            Duration::ZERO,
            ExecutionMetrics::default(),
            QueryStatus::Succeeded,
            "first",
        );
        let second_id = telemetry.begin();
        telemetry.record(
            second_id,
            Duration::ZERO,
            ExecutionMetrics::default(),
            QueryStatus::Succeeded,
            "second",
        );

        telemetry.set_config(TelemetryConfig {
            query_log_capacity: 1,
            sql_text_retention: SqlTextRetention::Disabled,
        });

        assert_eq!(telemetry.query_log.len(), 1);
        assert_eq!(telemetry.query_log[0].query_id, 2);
        assert_eq!(telemetry.query_log[0].sql_text, None);
    }
}
