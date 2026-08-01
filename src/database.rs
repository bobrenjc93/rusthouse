use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::catalog::CatalogGeneration;
use crate::error::{Error, LimitKind, Result};
use crate::persistence::{Persistence, StoreStatus};
use crate::query::{
    ActiveSnapshotError, EngineMetricsSnapshot, ObservabilitySnapshot, QueryCancellation,
    QueryObservability, QueryObservation, QueryPhase,
};
use crate::sql::{Comparison, Predicate, Statement, parse};
use crate::storage::{ColumnDef, DataType, EngineTable as Table, Value};
use crate::value::compare_int_float;

pub(crate) trait ExecutionCancellation {
    fn is_cancelled(&self) -> bool;
    fn begin_publication(&self) -> bool;
}

#[derive(Clone, Copy)]
struct ExecutionControl<'a> {
    max_result_bytes: usize,
    cancellation: &'a dyn ExecutionCancellation,
    observation: Option<&'a QueryObservation>,
}

/// Per-transaction bounds for staged inserts and their encoded value sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl TransactionLimits {
    pub const fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            max_rows,
            max_bytes,
        }
    }
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000_000,
            max_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Materialized rows returned by a `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ColumnDef>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultSet {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// The outcome of one SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    TransactionStarted { generation: u64 },
    TransactionCommitted { generation: u64 },
    TransactionRolledBack,
    TableCreated,
    TableDropped,
    RowsInserted { rows: usize },
    Query(ResultSet),
}

impl StatementResult {
    pub fn into_result_set(self) -> Option<ResultSet> {
        match self {
            Self::Query(result) => Some(result),
            _ => None,
        }
    }
}

/// A cloneable database handle. Sessions created from the same handle share commits.
#[derive(Debug, Clone)]
pub struct Database {
    inner: Arc<DatabaseInner>,
}

#[derive(Debug)]
struct DatabaseInner {
    state: Mutex<DatabaseState>,
    persistence: Option<Persistence>,
    default_limits: TransactionLimits,
    observability: QueryObservability,
}

#[derive(Debug)]
struct DatabaseState {
    head: Arc<CatalogGeneration>,
}

impl Database {
    /// Creates an in-memory database with default transaction limits.
    pub fn new() -> Self {
        Self::with_limits(TransactionLimits::default())
    }

    /// Creates an in-memory database with explicit transaction limits.
    pub fn with_limits(limits: TransactionLimits) -> Self {
        Self::from_generation(CatalogGeneration::empty(), None, limits)
    }

    /// Opens an atomically persisted database snapshot, or an empty database if absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_limits(path, TransactionLimits::default())
    }

    /// Opens a persisted database with explicit transaction limits.
    pub fn open_with_limits(path: impl AsRef<Path>, limits: TransactionLimits) -> Result<Self> {
        let persistence = Persistence::acquire(path.as_ref().to_path_buf())?;
        let generation = persistence.load()?;
        Ok(Self::from_generation(generation, Some(persistence), limits))
    }

    fn from_generation(
        generation: CatalogGeneration,
        persistence: Option<Persistence>,
        default_limits: TransactionLimits,
    ) -> Self {
        Self {
            inner: Arc::new(DatabaseInner {
                state: Mutex::new(DatabaseState {
                    head: Arc::new(generation),
                }),
                persistence,
                default_limits,
                observability: QueryObservability::default(),
            }),
        }
    }

    /// Creates a stateful SQL session with an independent transaction snapshot.
    pub fn session(&self) -> Session {
        Session {
            database: self.clone(),
            transaction: None,
            limits: self.inner.default_limits,
        }
    }

    /// Executes one autocommit statement in a temporary session.
    pub fn execute(&self, sql: &str) -> Result<StatementResult> {
        self.execute_inner(sql, None)
    }

    #[cfg(test)]
    pub(crate) fn execute_controlled(
        &self,
        sql: &str,
        max_result_bytes: usize,
        cancellation: &dyn ExecutionCancellation,
    ) -> Result<StatementResult> {
        self.execute_inner(
            sql,
            Some(ExecutionControl {
                max_result_bytes,
                cancellation,
                observation: None,
            }),
        )
    }

    pub(crate) fn execute_observed(
        &self,
        sql: &str,
        max_result_bytes: usize,
        cancellation: &dyn ExecutionCancellation,
        observation: &QueryObservation,
    ) -> Result<StatementResult> {
        self.execute_inner(
            sql,
            Some(ExecutionControl {
                max_result_bytes,
                cancellation,
                observation: Some(observation),
            }),
        )
    }

    fn execute_inner(
        &self,
        sql: &str,
        control: Option<ExecutionControl<'_>>,
    ) -> Result<StatementResult> {
        set_phase(control, QueryPhase::Parsing);
        let statement = parse(sql)?;
        set_phase(control, QueryPhase::Planning);
        if matches!(
            statement,
            Statement::Begin | Statement::Commit | Statement::Rollback
        ) {
            return Err(Error::Unsupported(
                "transaction control requires a persistent Session".to_owned(),
            ));
        }
        check_cancellation(control)?;
        self.session().execute_statement(statement, control)
    }

    /// Returns the current committed catalog generation.
    pub fn current_generation(&self) -> Result<u64> {
        Ok(self.inner.snapshot()?.id)
    }

    /// Returns the same bounded query and metric payload exposed by the HTTP service.
    pub fn observability_snapshot(&self) -> ObservabilitySnapshot {
        self.inner.observability.snapshot()
    }

    pub(crate) fn begin_observation(
        &self,
        query_id: u64,
        sql: &str,
        cancellation: QueryCancellation,
    ) -> QueryObservation {
        self.inner.observability.begin(query_id, sql, cancellation)
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseInner {
    fn lock(&self) -> Result<MutexGuard<'_, DatabaseState>> {
        self.state.lock().map_err(|_| Error::LockPoisoned)
    }

    fn snapshot(&self) -> Result<Arc<CatalogGeneration>> {
        Ok(Arc::clone(&self.lock()?.head))
    }

    fn execute_system_read(
        &self,
        catalog: &CatalogGeneration,
        name: &str,
        statement: Statement,
        control: Option<ExecutionControl<'_>>,
    ) -> Result<StatementResult> {
        check_cancellation(control)?;
        let Statement::Select {
            columns,
            predicates,
            ..
        } = statement
        else {
            return Err(Error::Unsupported("statement is not a query".to_owned()));
        };
        match name {
            "system.tables" => execute_virtual_rows(
                system_tables_schema(),
                catalog.tables.iter().map(|(name, table)| {
                    vec![
                        VirtualValue::BorrowedString("default"),
                        VirtualValue::BorrowedString(name),
                        VirtualValue::Int64(u64_to_i64(catalog.id)),
                        VirtualValue::Int64(usize_to_i64(table.schema().len())),
                        VirtualValue::Int64(usize_to_i64(table.row_count())),
                        VirtualValue::Int64(usize_to_i64(table.logical_bytes())),
                    ]
                }),
                columns,
                predicates,
                control,
                0,
            ),
            "system.columns" => execute_virtual_rows(
                system_columns_schema(),
                catalog.tables.iter().flat_map(|(table_name, table)| {
                    table
                        .schema()
                        .iter()
                        .enumerate()
                        .map(move |(index, column)| {
                            vec![
                                VirtualValue::BorrowedString("default"),
                                VirtualValue::BorrowedString(table_name),
                                VirtualValue::BorrowedString(&column.name),
                                VirtualValue::Int64(usize_to_i64(index.saturating_add(1))),
                                VirtualValue::DataType(column.data_type),
                                VirtualValue::Bool(column.nullable),
                            ]
                        })
                }),
                columns,
                predicates,
                control,
                0,
            ),
            "system.segments" => execute_virtual_rows(
                system_segments_schema(),
                catalog.tables.iter().map(|(name, table)| {
                    vec![
                        VirtualValue::BorrowedString("default"),
                        VirtualValue::BorrowedString(name),
                        VirtualValue::SegmentId {
                            generation: catalog.id,
                            table: name,
                        },
                        VirtualValue::Int64(u64_to_i64(catalog.id)),
                        VirtualValue::Int64(usize_to_i64(table.row_count())),
                        VirtualValue::Int64(usize_to_i64(table.logical_bytes())),
                    ]
                }),
                columns,
                predicates,
                control,
                0,
            ),
            "system.active_queries" => {
                let limit = control.map_or(usize::MAX, |control| control.max_result_bytes);
                let (queries, retained_bytes) = self
                    .observability
                    .bounded_active_queries(limit, || {
                        control.is_some_and(|control| control.cancellation.is_cancelled())
                    })
                    .map_err(|error| match error {
                        ActiveSnapshotError::Cancelled => Error::QueryCancelled,
                        ActiveSnapshotError::LimitExceeded { required } => {
                            Error::MemoryLimitExceeded {
                                operator: "system.active_queries",
                                required,
                                limit,
                            }
                        }
                    })?;
                execute_virtual_rows(
                    system_active_queries_schema(),
                    queries.into_iter().map(|query| {
                        vec![
                            VirtualValue::U64String(query.query_id),
                            VirtualValue::OwnedString(query.query),
                            VirtualValue::BorrowedString(query.phase.as_str()),
                            VirtualValue::Int64(u64_to_i64(query.elapsed_ms)),
                            VirtualValue::Int64(u64_to_i64(query.scanned_rows)),
                            VirtualValue::Int64(u64_to_i64(query.scanned_bytes)),
                            VirtualValue::Int64(u64_to_i64(query.peak_memory_bytes)),
                            VirtualValue::Int64(u64_to_i64(query.spill_bytes)),
                            VirtualValue::Bool(query.cancelled),
                        ]
                    }),
                    columns,
                    predicates,
                    control,
                    retained_bytes,
                )
            }
            "system.engine_metrics" => {
                check_cancellation(control)?;
                let metrics = self.observability.current_engine_metrics();
                execute_virtual_rows(
                    system_engine_metrics_schema(),
                    engine_metric_values(metrics)
                        .into_iter()
                        .map(|(name, value)| {
                            vec![
                                VirtualValue::BorrowedString(name),
                                VirtualValue::Int64(u64_to_i64(value)),
                            ]
                        }),
                    columns,
                    predicates,
                    control,
                    0,
                )
            }
            _ => Err(Error::TableNotFound(name.to_owned())),
        }
    }

    fn commit(
        &self,
        transaction: &Transaction,
        control: Option<ExecutionControl<'_>>,
    ) -> Result<u64> {
        let mut state = self.lock()?;
        check_cancellation(control)?;
        let current = Arc::clone(&state.head);
        if transaction.touched_tables.is_empty() {
            return Ok(current.id);
        }

        for table in &transaction.touched_tables {
            let unchanged = match (
                transaction.base.tables.get(table),
                current.tables.get(table),
            ) {
                (Some(base), Some(head)) => Arc::ptr_eq(base, head),
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                return Err(Error::Conflict {
                    table: table.clone(),
                    base_generation: transaction.base.id,
                    current_generation: current.id,
                });
            }
        }

        let id = current.id.checked_add(1).ok_or(Error::GenerationOverflow)?;
        let mut tables = current.tables.clone();
        for table in &transaction.touched_tables {
            if let Some(replacement) = transaction.tables.get(table) {
                tables.insert(table.clone(), Arc::clone(replacement));
            } else {
                tables.remove(table);
            }
        }
        let candidate = CatalogGeneration { id, tables };
        let store_status = if let Some(persistence) = &self.persistence {
            persistence.store(&candidate, || begin_publication(control))?
        } else {
            begin_publication(control)?;
            StoreStatus::Durable
        };
        match store_status {
            StoreStatus::Durable => {
                state.head = Arc::new(candidate);
                Ok(id)
            }
            #[cfg(any(unix, windows))]
            StoreStatus::PublishedWithError(error) => {
                state.head = Arc::new(candidate);
                Err(Error::CommitDurabilityUncertain {
                    generation: id,
                    message: error.to_string(),
                })
            }
            #[cfg(windows)]
            StoreStatus::RecoveryRequired(error) => {
                Err(Error::CommitRecoveryRequired(error.to_string()))
            }
        }
    }
}

fn system_tables_schema() -> Vec<ColumnDef> {
    vec![
        string_column("database"),
        string_column("name"),
        int_column("generation"),
        int_column("column_count"),
        int_column("row_count"),
        int_column("logical_bytes"),
    ]
}

fn system_columns_schema() -> Vec<ColumnDef> {
    vec![
        string_column("database"),
        string_column("table"),
        string_column("name"),
        int_column("ordinal_position"),
        string_column("data_type"),
        bool_column("nullable"),
    ]
}

fn system_segments_schema() -> Vec<ColumnDef> {
    vec![
        string_column("database"),
        string_column("table"),
        string_column("segment_id"),
        int_column("generation"),
        int_column("row_count"),
        int_column("logical_bytes"),
    ]
}

fn system_active_queries_schema() -> Vec<ColumnDef> {
    vec![
        string_column("query_id"),
        string_column("query"),
        string_column("phase"),
        int_column("elapsed_ms"),
        int_column("scanned_rows"),
        int_column("scanned_bytes"),
        int_column("peak_memory_bytes"),
        int_column("spill_bytes"),
        bool_column("cancelled"),
    ]
}

fn system_engine_metrics_schema() -> Vec<ColumnDef> {
    vec![string_column("metric"), int_column("value")]
}

fn engine_metric_values(metrics: EngineMetricsSnapshot) -> [(&'static str, u64); 11] {
    [
        ("active_queries", metrics.active_queries),
        ("tracked_active_queries", metrics.tracked_active_queries),
        ("queries_total", metrics.queries_total),
        ("queries_succeeded_total", metrics.queries_succeeded_total),
        ("queries_failed_total", metrics.queries_failed_total),
        ("queries_cancelled_total", metrics.queries_cancelled_total),
        ("scanned_rows_total", metrics.scanned_rows_total),
        ("scanned_bytes_total", metrics.scanned_bytes_total),
        ("peak_memory_bytes", metrics.peak_memory_bytes),
        ("spill_bytes_total", metrics.spill_bytes_total),
        (
            "dropped_active_query_records_total",
            metrics.dropped_active_query_records_total,
        ),
    ]
}

enum VirtualValue<'a> {
    BorrowedString(&'a str),
    OwnedString(String),
    SegmentId { generation: u64, table: &'a str },
    U64String(u64),
    DataType(DataType),
    Int64(i64),
    Bool(bool),
}

impl VirtualValue<'_> {
    fn materialized_bytes(&self) -> usize {
        std::mem::size_of::<Value>()
            + match self {
                Self::BorrowedString(value) => value.len(),
                Self::OwnedString(value) => value.len(),
                Self::SegmentId { generation, table } => decimal_len(*generation)
                    .saturating_add(1)
                    .saturating_add(table.len()),
                Self::U64String(value) => decimal_len(*value),
                Self::DataType(data_type) => data_type_name(*data_type).len(),
                Self::Int64(_) | Self::Bool(_) => 0,
            }
    }

    fn into_value(self) -> Value {
        match self {
            Self::BorrowedString(value) => Value::String(value.to_owned()),
            Self::OwnedString(value) => Value::String(value),
            Self::SegmentId { generation, table } => Value::String(format!("{generation}:{table}")),
            Self::U64String(value) => Value::String(value.to_string()),
            Self::DataType(data_type) => Value::String(data_type_name(data_type).to_owned()),
            Self::Int64(value) => Value::Int64(value),
            Self::Bool(value) => Value::Bool(value),
        }
    }
}

fn execute_virtual_rows<'a>(
    schema: Vec<ColumnDef>,
    rows: impl IntoIterator<Item = Vec<VirtualValue<'a>>>,
    columns: Option<Vec<String>>,
    predicates: Vec<Predicate>,
    control: Option<ExecutionControl<'_>>,
    base_retained_bytes: usize,
) -> Result<StatementResult> {
    set_phase(control, QueryPhase::Scanning);
    let projection = prepare_projection(&schema, columns)?;
    let predicates = prepare_predicates_schema(&schema, &predicates)?;
    let column_bytes = projected_column_bytes(&schema, &projection);
    let mut result_bytes = column_bytes;
    enforce_result_limit(control, base_retained_bytes.saturating_add(result_bytes))?;
    set_peak_memory(control, base_retained_bytes.saturating_add(result_bytes));

    let mut output_rows = Vec::new();
    let mut rows = rows.into_iter();
    loop {
        check_cancellation(control)?;
        let Some(virtual_row) = rows.next() else {
            break;
        };
        if virtual_row.len() != schema.len() {
            return Err(Error::InvalidRow(
                "system table row does not match its schema".to_owned(),
            ));
        }
        let temporary_bytes = std::mem::size_of::<Vec<Value>>().saturating_add(
            virtual_row.iter().fold(0_usize, |bytes, value| {
                bytes.saturating_add(value.materialized_bytes())
            }),
        );
        let temporary_required = base_retained_bytes
            .saturating_add(result_bytes)
            .saturating_add(temporary_bytes);
        enforce_result_limit(control, temporary_required)?;
        set_peak_memory(control, temporary_required);
        let row = virtual_row
            .into_iter()
            .map(VirtualValue::into_value)
            .collect::<Vec<_>>();

        let mut predicate_bytes = 0_usize;
        let matches = predicates.iter().all(|(column, comparison, value)| {
            predicate_bytes = predicate_bytes.saturating_add(row[*column].logical_bytes());
            compare(&row[*column], value, *comparison)
        });
        add_scan(control, 1, predicate_bytes);
        if matches {
            let projected_owned_bytes = projection.iter().fold(0_usize, |bytes, column| {
                bytes.saturating_add(value_owned_bytes(&row[*column]))
            });
            let projected_logical_bytes = projection.iter().fold(0_usize, |bytes, column| {
                bytes.saturating_add(row[*column].logical_bytes())
            });
            let old_capacity = output_rows.capacity();
            let target_capacity =
                next_virtual_row_capacity(old_capacity, output_rows.len().saturating_add(1));
            let predicted_outer_growth = target_capacity
                .saturating_sub(old_capacity)
                .saturating_mul(std::mem::size_of::<Vec<Value>>());
            let predicted_required = temporary_required
                .saturating_add(predicted_outer_growth)
                .saturating_add(projected_owned_bytes);
            enforce_result_limit(control, predicted_required)?;
            if target_capacity > old_capacity {
                output_rows.reserve_exact(target_capacity.saturating_sub(output_rows.len()));
            }
            let actual_outer_growth = output_rows
                .capacity()
                .saturating_sub(old_capacity)
                .saturating_mul(std::mem::size_of::<Vec<Value>>());
            let required = temporary_required
                .saturating_add(actual_outer_growth)
                .saturating_add(projected_owned_bytes);
            enforce_result_limit(control, required)?;
            set_peak_memory(control, required);
            result_bytes = result_bytes
                .saturating_add(actual_outer_growth)
                .saturating_add(projected_owned_bytes);
            add_scan(control, 0, projected_logical_bytes);
            output_rows.push(
                projection
                    .iter()
                    .map(|column| row[*column].clone())
                    .collect(),
            );
        }
    }

    let columns = projection
        .iter()
        .map(|index| schema[*index].clone())
        .collect();
    Ok(StatementResult::Query(ResultSet {
        columns,
        rows: output_rows,
    }))
}

fn next_virtual_row_capacity(current: usize, required: usize) -> usize {
    if required <= current {
        return current;
    }
    if current == 0 {
        return required.max(4);
    }
    current.saturating_mul(2).max(required)
}

fn decimal_len(mut value: u64) -> usize {
    let mut length = 1;
    while value >= 10 {
        value /= 10;
        length += 1;
    }
    length
}

fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Int64 => "Int64",
        DataType::Float64 => "Float64",
        DataType::Bool => "Bool",
        DataType::String => "String",
    }
}

fn value_owned_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(value) => value.len(),
            _ => 0,
        }
}

fn string_column(name: &str) -> ColumnDef {
    ColumnDef::new(name, DataType::String, false)
}

fn int_column(name: &str) -> ColumnDef {
    ColumnDef::new(name, DataType::Int64, false)
}

fn bool_column(name: &str) -> ColumnDef {
    ColumnDef::new(name, DataType::Bool, false)
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A database client session. It is intended to be owned by one thread at a time.
#[derive(Debug)]
pub struct Session {
    database: Database,
    transaction: Option<Transaction>,
    limits: TransactionLimits,
}

#[derive(Debug)]
struct Transaction {
    base: Arc<CatalogGeneration>,
    tables: BTreeMap<String, Arc<Table>>,
    touched_tables: BTreeSet<String>,
    written_rows: usize,
    written_bytes: usize,
    limits: TransactionLimits,
}

impl Transaction {
    fn new(base: Arc<CatalogGeneration>, limits: TransactionLimits) -> Self {
        Self {
            tables: base.tables.clone(),
            base,
            touched_tables: BTreeSet::new(),
            written_rows: 0,
            written_bytes: 0,
            limits,
        }
    }

    fn prospective_charge(&self, rows: usize, bytes: usize) -> Result<(usize, usize)> {
        let written_rows = self.written_rows.saturating_add(rows);
        if written_rows > self.limits.max_rows {
            return Err(Error::TransactionLimitExceeded {
                kind: LimitKind::Rows,
                limit: self.limits.max_rows,
                attempted: written_rows,
            });
        }
        let written_bytes = self.written_bytes.saturating_add(bytes);
        if written_bytes > self.limits.max_bytes {
            return Err(Error::TransactionLimitExceeded {
                kind: LimitKind::Bytes,
                limit: self.limits.max_bytes,
                attempted: written_bytes,
            });
        }
        Ok((written_rows, written_bytes))
    }

    fn finish_charge(&mut self, charge: (usize, usize)) {
        (self.written_rows, self.written_bytes) = charge;
    }
}

impl Session {
    /// Executes one SQL statement, using the active snapshot when inside a transaction.
    pub fn execute(&mut self, sql: &str) -> Result<StatementResult> {
        self.execute_statement(parse(sql)?, None)
    }

    fn execute_statement(
        &mut self,
        statement: Statement,
        control: Option<ExecutionControl<'_>>,
    ) -> Result<StatementResult> {
        match statement {
            Statement::Begin => {
                let generation = self.begin()?;
                Ok(StatementResult::TransactionStarted { generation })
            }
            Statement::Commit => {
                let generation = self.commit()?;
                Ok(StatementResult::TransactionCommitted { generation })
            }
            Statement::Rollback => {
                self.rollback()?;
                Ok(StatementResult::TransactionRolledBack)
            }
            statement @ Statement::Select { .. } => {
                let table_name = match &statement {
                    Statement::Select { table, .. } => table.clone(),
                    _ => unreachable!("the match arm guarantees SELECT"),
                };
                if let Some(transaction) = &self.transaction {
                    if table_name.starts_with("system.")
                        && !transaction.tables.contains_key(&table_name)
                    {
                        return self.database.inner.execute_system_read(
                            &transaction.base,
                            &table_name,
                            statement,
                            control,
                        );
                    }
                    execute_read(&transaction.tables, statement, control)
                } else {
                    let snapshot = self.database.inner.snapshot()?;
                    if table_name.starts_with("system.")
                        && !snapshot.tables.contains_key(&table_name)
                    {
                        return self.database.inner.execute_system_read(
                            &snapshot,
                            &table_name,
                            statement,
                            control,
                        );
                    }
                    execute_read(&snapshot.tables, statement, control)
                }
            }
            statement => {
                if let Some(transaction) = &mut self.transaction {
                    execute_write(transaction, statement)
                } else {
                    let snapshot = self.database.inner.snapshot()?;
                    let mut transaction = Transaction::new(snapshot, self.limits);
                    let result = execute_write(&mut transaction, statement)?;
                    check_cancellation(control)?;
                    self.database.inner.commit(&transaction, control)?;
                    Ok(result)
                }
            }
        }
    }

    /// Starts a transaction and pins its reader snapshot.
    pub fn begin(&mut self) -> Result<u64> {
        if self.transaction.is_some() {
            return Err(Error::TransactionAlreadyActive);
        }
        let snapshot = self.database.inner.snapshot()?;
        let generation = snapshot.id;
        self.transaction = Some(Transaction::new(snapshot, self.limits));
        Ok(generation)
    }

    /// Atomically publishes all staged table replacements.
    pub fn commit(&mut self) -> Result<u64> {
        let transaction = self.transaction.take().ok_or(Error::NoActiveTransaction)?;
        match self.database.inner.commit(&transaction, None) {
            Ok(generation) => Ok(generation),
            Err(error)
                if matches!(
                    &error,
                    Error::Conflict { .. } | Error::CommitDurabilityUncertain { .. }
                ) =>
            {
                Err(error)
            }
            Err(error) => {
                self.transaction = Some(transaction);
                Err(error)
            }
        }
    }

    /// Discards all staged changes and the pinned snapshot.
    pub fn rollback(&mut self) -> Result<()> {
        self.transaction
            .take()
            .map(|_| ())
            .ok_or(Error::NoActiveTransaction)
    }

    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    pub fn snapshot_generation(&self) -> Option<u64> {
        self.transaction
            .as_ref()
            .map(|transaction| transaction.base.id)
    }

    /// Changes limits for subsequently started transactions and autocommit writes.
    pub fn set_transaction_limits(&mut self, limits: TransactionLimits) -> Result<()> {
        if self.transaction.is_some() {
            return Err(Error::TransactionAlreadyActive);
        }
        self.limits = limits;
        Ok(())
    }
}

fn execute_write(transaction: &mut Transaction, statement: Statement) -> Result<StatementResult> {
    let targets_virtual_system_table = match &statement {
        Statement::CreateTable { name, .. } => name.starts_with("system."),
        Statement::DropTable { name } => {
            name.starts_with("system.") && !transaction.tables.contains_key(name)
        }
        Statement::Insert { table, .. } => {
            table.starts_with("system.") && !transaction.tables.contains_key(table)
        }
        _ => false,
    };
    if targets_virtual_system_table {
        return Err(Error::Unsupported("system tables are read-only".to_owned()));
    }
    match statement {
        Statement::CreateTable { name, columns } => {
            if transaction.tables.contains_key(&name) {
                return Err(Error::TableAlreadyExists(name));
            }
            let bytes = estimate_schema_bytes(&name, &columns);
            let table = Arc::new(Table::new(columns)?);
            let charge = transaction.prospective_charge(0, bytes)?;
            transaction.tables.insert(name.clone(), table);
            transaction.touched_tables.insert(name);
            transaction.finish_charge(charge);
            Ok(StatementResult::TableCreated)
        }
        Statement::DropTable { name } => {
            if !transaction.tables.contains_key(&name) {
                return Err(Error::TableNotFound(name));
            }
            let charge = transaction.prospective_charge(0, name.len().saturating_add(8))?;
            transaction.tables.remove(&name);
            transaction.touched_tables.insert(name);
            transaction.finish_charge(charge);
            Ok(StatementResult::TableDropped)
        }
        Statement::Insert {
            table,
            columns,
            rows,
        } => {
            let existing = transaction
                .tables
                .get(&table)
                .ok_or_else(|| Error::TableNotFound(table.clone()))?;
            let rows = arrange_rows(existing, columns.as_deref(), rows)?;
            let bytes = estimate_insert_bytes(&table, &rows);
            let charge = transaction.prospective_charge(rows.len(), bytes)?;
            let replacement = transaction
                .tables
                .get_mut(&table)
                .expect("the table was resolved before preparing rows");
            Arc::make_mut(replacement).append_rows(&rows)?;
            transaction.touched_tables.insert(table);
            transaction.finish_charge(charge);
            Ok(StatementResult::RowsInserted { rows: rows.len() })
        }
        _ => Err(Error::Unsupported(
            "statement is not a catalog write".to_owned(),
        )),
    }
}

fn arrange_rows(
    table: &Table,
    column_names: Option<&[String]>,
    input_rows: Vec<Vec<Value>>,
) -> Result<Vec<Vec<Value>>> {
    let Some(column_names) = column_names else {
        return Ok(input_rows);
    };
    let mut indices = Vec::with_capacity(column_names.len());
    for (position, name) in column_names.iter().enumerate() {
        if column_names[..position].contains(name) {
            return Err(Error::DuplicateColumn(name.clone()));
        }
        indices.push(
            table
                .column_index(name)
                .ok_or_else(|| Error::ColumnNotFound(name.clone()))?,
        );
    }

    let mut rows = Vec::with_capacity(input_rows.len());
    for input in input_rows {
        if input.len() != indices.len() {
            return Err(Error::InvalidRow(format!(
                "expected {} values for the INSERT column list, got {}",
                indices.len(),
                input.len()
            )));
        }
        let mut row = vec![Value::Null; table.schema().len()];
        for (index, value) in indices.iter().copied().zip(input) {
            row[index] = value;
        }
        rows.push(row);
    }
    Ok(rows)
}

fn estimate_schema_bytes(name: &str, columns: &[ColumnDef]) -> usize {
    columns
        .iter()
        .fold(name.len().saturating_add(24), |total, column| {
            total.saturating_add(column.name.len()).saturating_add(10)
        })
}

fn estimate_insert_bytes(table: &str, rows: &[Vec<Value>]) -> usize {
    rows.iter()
        .fold(table.len().saturating_add(8), |total, row| {
            row.iter().fold(total, |total, value| {
                total.saturating_add(value.estimated_size())
            })
        })
}

fn execute_read(
    tables: &BTreeMap<String, Arc<Table>>,
    statement: Statement,
    control: Option<ExecutionControl<'_>>,
) -> Result<StatementResult> {
    let Statement::Select {
        table,
        columns,
        predicates,
    } = statement
    else {
        return Err(Error::Unsupported("statement is not a query".to_owned()));
    };
    let table = tables
        .get(&table)
        .ok_or_else(|| Error::TableNotFound(table.clone()))?;
    execute_read_table_parts(table, columns, predicates, control)
}

fn execute_read_table_parts(
    table: &Table,
    columns: Option<Vec<String>>,
    predicates: Vec<Predicate>,
    control: Option<ExecutionControl<'_>>,
) -> Result<StatementResult> {
    set_phase(control, QueryPhase::Scanning);
    let projection = prepare_projection(table.schema(), columns)?;
    let predicates = prepare_predicates_schema(table.schema(), &predicates)?;
    let column_bytes = projected_column_bytes(table.schema(), &projection);
    enforce_result_limit(control, column_bytes)?;
    let row_fixed_bytes = std::mem::size_of::<Vec<Value>>().saturating_add(
        projection
            .len()
            .saturating_mul(std::mem::size_of::<Value>()),
    );
    let row_capacity = control.map_or(0, |control| {
        control
            .max_result_bytes
            .saturating_sub(column_bytes)
            .checked_div(row_fixed_bytes.max(1))
            .unwrap_or(0)
            .min(table.row_count())
    });
    let mut rows = if control.is_some() {
        Vec::with_capacity(row_capacity)
    } else {
        Vec::new()
    };
    let mut result_bytes =
        column_bytes.saturating_add(row_capacity.saturating_mul(std::mem::size_of::<Vec<Value>>()));
    enforce_result_limit(control, result_bytes)?;
    set_peak_memory(control, result_bytes);
    for row in 0..table.row_count() {
        check_cancellation(control)?;
        let mut predicate_bytes = 0_usize;
        let matches = predicates.iter().all(|(column, comparison, value)| {
            predicate_bytes =
                predicate_bytes.saturating_add(table.logical_value_bytes(row, *column));
            compare(&table.value(row, *column), value, *comparison)
        });
        add_scan(control, 1, predicate_bytes);
        if matches {
            let row_bytes = projection.iter().fold(0usize, |bytes, column| {
                bytes.saturating_add(table.owned_value_bytes(row, *column))
            });
            let additional_outer_bytes = if control.is_some() && rows.len() == row_capacity {
                std::mem::size_of::<Vec<Value>>()
            } else {
                0
            };
            let required = result_bytes
                .saturating_add(additional_outer_bytes)
                .saturating_add(row_bytes);
            enforce_result_limit(control, required)?;
            result_bytes = required;
            let projected_bytes = projection.iter().fold(0_usize, |bytes, column| {
                bytes.saturating_add(table.logical_value_bytes(row, *column))
            });
            add_scan(control, 0, projected_bytes);
            set_peak_memory(control, result_bytes);
            rows.push(
                projection
                    .iter()
                    .map(|column| table.value(row, *column))
                    .collect(),
            );
        }
    }
    let columns = projection
        .iter()
        .map(|index| table.schema()[*index].clone())
        .collect();
    Ok(StatementResult::Query(ResultSet { columns, rows }))
}

fn check_cancellation(control: Option<ExecutionControl<'_>>) -> Result<()> {
    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        Err(Error::QueryCancelled)
    } else {
        Ok(())
    }
}

fn begin_publication(control: Option<ExecutionControl<'_>>) -> Result<()> {
    set_phase(control, QueryPhase::Publishing);
    if control.is_some_and(|control| !control.cancellation.begin_publication()) {
        Err(Error::QueryCancelled)
    } else {
        Ok(())
    }
}

fn set_phase(control: Option<ExecutionControl<'_>>, phase: QueryPhase) {
    if let Some(observation) = control.and_then(|control| control.observation) {
        observation.set_phase(phase);
    }
}

fn add_scan(control: Option<ExecutionControl<'_>>, rows: u64, bytes: usize) {
    if let Some(observation) = control.and_then(|control| control.observation) {
        observation.add_scan(rows, usize_to_u64(bytes));
    }
}

fn set_peak_memory(control: Option<ExecutionControl<'_>>, bytes: usize) {
    if let Some(observation) = control.and_then(|control| control.observation) {
        observation.set_peak_memory(usize_to_u64(bytes));
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn enforce_result_limit(control: Option<ExecutionControl<'_>>, required: usize) -> Result<()> {
    if let Some(control) = control
        && required > control.max_result_bytes
    {
        return Err(Error::MemoryLimitExceeded {
            operator: "query result",
            required,
            limit: control.max_result_bytes,
        });
    }
    Ok(())
}

fn prepare_projection(schema: &[ColumnDef], columns: Option<Vec<String>>) -> Result<Vec<usize>> {
    match columns {
        Some(columns) => {
            let mut projection = Vec::with_capacity(columns.len());
            for (position, name) in columns.iter().enumerate() {
                if columns[..position].contains(name) {
                    return Err(Error::DuplicateColumn(name.clone()));
                }
                projection.push(
                    schema
                        .iter()
                        .position(|column| column.name == *name)
                        .ok_or_else(|| Error::ColumnNotFound(name.clone()))?,
                );
            }
            Ok(projection)
        }
        None => Ok((0..schema.len()).collect()),
    }
}

fn projected_column_bytes(schema: &[ColumnDef], projection: &[usize]) -> usize {
    projection.iter().fold(
        projection
            .len()
            .saturating_mul(std::mem::size_of::<ColumnDef>()),
        |bytes, index| bytes.saturating_add(schema[*index].name.len()),
    )
}

fn prepare_predicates_schema(
    schema: &[ColumnDef],
    predicates: &[Predicate],
) -> Result<Vec<(usize, Comparison, Value)>> {
    predicates
        .iter()
        .map(|predicate| {
            let index = schema
                .iter()
                .position(|column| column.name == predicate.column)
                .ok_or_else(|| Error::ColumnNotFound(predicate.column.clone()))?;
            let column = &schema[index];
            if let Some(value_type) = predicate.value.data_type()
                && value_type != column.data_type
                && !(value_type.is_numeric() && column.data_type.is_numeric())
            {
                return Err(Error::TypeMismatch {
                    column: column.name.clone(),
                    expected: column.data_type.to_string(),
                    actual: predicate.value.type_name().to_owned(),
                });
            }
            Ok((index, predicate.comparison, predicate.value.clone()))
        })
        .collect()
}

fn compare(left: &Value, right: &Value, comparison: Comparison) -> bool {
    if left == &Value::Null || right == &Value::Null {
        return false;
    }
    let ordering = match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Some(left.cmp(right)),
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(*left, *right),
        (Value::Float64(left), Value::Int64(right)) => {
            compare_int_float(*right, *left).map(Ordering::reverse)
        }
        (Value::Float64(left), Value::Float64(right)) => left.partial_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => None,
    };
    match comparison {
        Comparison::Equal => ordering == Some(Ordering::Equal),
        Comparison::NotEqual => ordering != Some(Ordering::Equal),
        Comparison::Less => ordering == Some(Ordering::Less),
        Comparison::LessOrEqual => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        Comparison::Greater => ordering == Some(Ordering::Greater),
        Comparison::GreaterOrEqual => {
            matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
        }
    }
}

#[cfg(test)]
mod cancellation_commit_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;

    struct BlockingPublication {
        barrier: Arc<Barrier>,
    }

    impl ExecutionCancellation for BlockingPublication {
        fn is_cancelled(&self) -> bool {
            false
        }

        fn begin_publication(&self) -> bool {
            self.barrier.wait();
            self.barrier.wait();
            true
        }
    }

    struct QueuedCancellation {
        cancelled: Arc<AtomicBool>,
        checks: AtomicUsize,
        ready: Arc<Barrier>,
        publication_attempts: Arc<AtomicUsize>,
    }

    impl ExecutionCancellation for QueuedCancellation {
        fn is_cancelled(&self) -> bool {
            let cancelled = self.cancelled.load(Ordering::SeqCst);
            if self.checks.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                self.ready.wait();
                self.ready.wait();
            }
            cancelled
        }

        fn begin_publication(&self) -> bool {
            self.publication_attempts.fetch_add(1, Ordering::SeqCst);
            !self.cancelled.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn writer_cancelled_behind_slow_commit_never_enters_publication() {
        let database = Database::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let publication_attempts = Arc::new(AtomicUsize::new(0));
        let queued_database = database.clone();
        let queued_cancelled = Arc::clone(&cancelled);
        let queued_ready = Arc::clone(&ready);
        let queued_attempts = Arc::clone(&publication_attempts);
        let queued_writer = std::thread::spawn(move || {
            let cancellation = QueuedCancellation {
                cancelled: queued_cancelled,
                checks: AtomicUsize::new(0),
                ready: queued_ready,
                publication_attempts: queued_attempts,
            };
            queued_database.execute_controlled(
                "CREATE TABLE cancelled_commit (id Int64)",
                usize::MAX,
                &cancellation,
            )
        });
        ready.wait();

        let slow_barrier = Arc::new(Barrier::new(2));
        let slow_database = database.clone();
        let slow_cancellation = BlockingPublication {
            barrier: Arc::clone(&slow_barrier),
        };
        let slow_writer = std::thread::spawn(move || {
            slow_database.execute_controlled(
                "CREATE TABLE slow_commit (id Int64)",
                usize::MAX,
                &slow_cancellation,
            )
        });
        slow_barrier.wait();

        cancelled.store(true, Ordering::SeqCst);
        ready.wait();
        slow_barrier.wait();

        assert!(matches!(
            slow_writer.join().unwrap(),
            Ok(StatementResult::TableCreated)
        ));
        assert!(matches!(
            queued_writer.join().unwrap(),
            Err(Error::QueryCancelled)
        ));
        assert_eq!(publication_attempts.load(Ordering::SeqCst), 0);
        assert!(matches!(
            database.execute("SELECT * FROM slow_commit"),
            Ok(StatementResult::Query(_))
        ));
        assert!(matches!(
            database.execute("SELECT * FROM cancelled_commit"),
            Err(Error::TableNotFound(_))
        ));
    }
}

#[cfg(test)]
mod system_table_resource_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CancelAfterChecks {
        checks: AtomicUsize,
        allowed_checks: usize,
    }

    impl ExecutionCancellation for CancelAfterChecks {
        fn is_cancelled(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) >= self.allowed_checks
        }

        fn begin_publication(&self) -> bool {
            true
        }
    }

    #[test]
    fn system_metadata_stream_checks_cancellation_between_rows() {
        let database = Database::new();
        for index in 0..20 {
            database
                .execute(&format!("CREATE TABLE table_{index} (id Int64)"))
                .unwrap();
        }
        let cancellation = CancelAfterChecks {
            checks: AtomicUsize::new(0),
            allowed_checks: 5,
        };

        assert!(matches!(
            database.execute_controlled(
                "SELECT name FROM system.tables",
                usize::MAX,
                &cancellation,
            ),
            Err(Error::QueryCancelled)
        ));
    }

    #[test]
    fn system_metadata_row_is_budgeted_before_string_cloning() {
        let database = Database::new();
        let name = "x".repeat(1_024);
        database
            .execute(&format!("CREATE TABLE \"{name}\" (id Int64)"))
            .unwrap();
        let cancellation = CancelAfterChecks {
            checks: AtomicUsize::new(0),
            allowed_checks: usize::MAX,
        };

        assert!(matches!(
            database.execute_controlled("SELECT name FROM system.tables", 128, &cancellation,),
            Err(Error::MemoryLimitExceeded { .. })
        ));
    }

    #[test]
    fn virtual_result_accounts_for_outer_capacity_at_growth_boundaries() {
        let cancellation = CancelAfterChecks {
            checks: AtomicUsize::new(0),
            allowed_checks: usize::MAX,
        };
        for row_count in [1_usize, 4, 5, 8, 9] {
            let expected_capacity = row_count.next_power_of_two().max(4);
            let column_bytes = std::mem::size_of::<ColumnDef>() + "value".len();
            let temporary_bytes = std::mem::size_of::<Vec<Value>>() + std::mem::size_of::<Value>();
            let exact_limit = column_bytes
                + expected_capacity * std::mem::size_of::<Vec<Value>>()
                + row_count * std::mem::size_of::<Value>()
                + temporary_bytes;
            let control = Some(ExecutionControl {
                max_result_bytes: exact_limit,
                cancellation: &cancellation,
                observation: None,
            });
            let result = execute_virtual_rows(
                vec![int_column("value")],
                (0..row_count).map(|value| {
                    vec![VirtualValue::Int64(
                        i64::try_from(value).expect("test row fits in i64"),
                    )]
                }),
                None,
                Vec::new(),
                control,
                0,
            )
            .unwrap()
            .into_result_set()
            .unwrap();
            assert_eq!(result.rows.len(), row_count);
            assert_eq!(result.rows.capacity(), expected_capacity);

            let too_small = Some(ExecutionControl {
                max_result_bytes: exact_limit - 1,
                cancellation: &cancellation,
                observation: None,
            });
            assert!(matches!(
                execute_virtual_rows(
                    vec![int_column("value")],
                    (0..row_count).map(|value| {
                        vec![VirtualValue::Int64(
                            i64::try_from(value).expect("test row fits in i64"),
                        )]
                    }),
                    None,
                    Vec::new(),
                    too_small,
                    0,
                ),
                Err(Error::MemoryLimitExceeded { .. })
            ));
        }
    }
}

#[cfg(all(test, unix))]
mod persistence_commit_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::persistence::fail_next_directory_sync;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);
    static PERSISTENCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temporary_path() -> PathBuf {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rusthouse-directory-sync-{}-{sequence}.db",
            std::process::id()
        ))
    }

    fn remove_database(path: &Path) {
        let _ = fs::remove_file(path);
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".rusthouse-lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }

    #[test]
    fn post_rename_sync_failure_keeps_memory_and_disk_aligned() {
        let _test_guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
        let path = temporary_path();
        let database = Database::open(&path).unwrap();
        let mut session = database.session();
        session.begin().unwrap();
        session
            .execute("CREATE TABLE published (id Int64)")
            .unwrap();

        fail_next_directory_sync();
        assert!(matches!(
            session.commit(),
            Err(Error::CommitDurabilityUncertain { generation: 1, .. })
        ));
        assert!(!session.in_transaction());
        assert_eq!(database.current_generation().unwrap(), 1);
        assert!(matches!(
            database.execute("SELECT * FROM published"),
            Ok(StatementResult::Query(_))
        ));
        drop(session);
        drop(database);

        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.current_generation().unwrap(), 1);
        assert!(matches!(
            reopened.execute("SELECT * FROM published"),
            Ok(StatementResult::Query(_))
        ));
        drop(reopened);
        remove_database(&path);
    }

    #[test]
    fn legacy_system_named_table_remains_accessible_until_migrated() {
        let _test_guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
        let path = temporary_path();
        let database = Database::open(&path).unwrap();
        let snapshot = database.inner.snapshot().unwrap();
        let mut transaction = Transaction::new(snapshot, TransactionLimits::default());
        let mut legacy = Table::new(vec![ColumnDef::new("id", DataType::Int64, false)]).unwrap();
        legacy.append_rows(&[vec![Value::Int64(7)]]).unwrap();
        transaction
            .tables
            .insert("system.tables".to_owned(), Arc::new(legacy));
        transaction
            .touched_tables
            .insert("system.tables".to_owned());
        database.inner.commit(&transaction, None).unwrap();
        drop(database);

        let reopened = Database::open(&path).unwrap();
        let rows = reopened
            .execute("SELECT * FROM \"system.tables\"")
            .unwrap()
            .into_result_set()
            .unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Int64(7)]]);
        reopened
            .execute("INSERT INTO \"system.tables\" VALUES (8)")
            .unwrap();
        reopened.execute("DROP TABLE \"system.tables\"").unwrap();

        let virtual_table = reopened
            .execute("SELECT * FROM system.tables")
            .unwrap()
            .into_result_set()
            .unwrap();
        assert_eq!(virtual_table.columns[0].name, "database");
        assert!(virtual_table.rows.is_empty());
        drop(reopened);
        remove_database(&path);
    }
}
