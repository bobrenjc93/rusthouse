use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::catalog::CatalogGeneration;
use crate::error::{Error, LimitKind, Result};
use crate::formats::{CsvBatchReader, CsvOptions, FormatError, NdjsonBatchReader, NdjsonOptions};
use crate::persistence::{Persistence, StoreStatus};
use crate::sql::{Comparison, Predicate, Statement, parse};
use crate::storage::{Column, ColumnBatch, ColumnDef, EngineTable as Table, Schema, Value};
use crate::value::compare_int_float;

pub(crate) trait ExecutionCancellation {
    fn is_cancelled(&self) -> bool;
    fn begin_publication(&self) -> bool;
}

#[derive(Clone, Copy)]
struct ExecutionControl<'a> {
    max_result_bytes: usize,
    cancellation: &'a dyn ExecutionCancellation,
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
            }),
        )
    }

    pub(crate) fn ingest_csv_controlled<R: BufRead>(
        &self,
        table: &str,
        input: R,
        options: CsvOptions,
        cancellation: &dyn ExecutionCancellation,
    ) -> std::result::Result<u64, FormatError> {
        self.session().ingest_csv_inner(
            table,
            input,
            options,
            Some(ExecutionControl {
                max_result_bytes: usize::MAX,
                cancellation,
            }),
        )
    }

    pub(crate) fn ingest_ndjson_controlled<R: BufRead>(
        &self,
        table: &str,
        input: R,
        options: NdjsonOptions,
        cancellation: &dyn ExecutionCancellation,
    ) -> std::result::Result<u64, FormatError> {
        self.session().ingest_ndjson_inner(
            table,
            input,
            options,
            Some(ExecutionControl {
                max_result_bytes: usize::MAX,
                cancellation,
            }),
        )
    }

    fn execute_inner(
        &self,
        sql: &str,
        control: Option<ExecutionControl<'_>>,
    ) -> Result<StatementResult> {
        let statement = parse(sql)?;
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

    /// Atomically appends a bounded CSV stream to a durable SQL table.
    pub fn ingest_csv<R: BufRead>(
        &mut self,
        table: &str,
        input: R,
        options: CsvOptions,
    ) -> std::result::Result<u64, FormatError> {
        self.ingest_csv_inner(table, input, options, None)
    }

    fn ingest_csv_inner<R: BufRead>(
        &mut self,
        table: &str,
        input: R,
        options: CsvOptions,
        control: Option<ExecutionControl<'_>>,
    ) -> std::result::Result<u64, FormatError> {
        let schema = self.ingestion_schema(table)?;
        let batches = CsvBatchReader::new(input, &schema, options)?;
        self.ingest_batches(table, batches, control)
    }

    /// Atomically appends a bounded NDJSON stream to a durable SQL table.
    pub fn ingest_ndjson<R: BufRead>(
        &mut self,
        table: &str,
        input: R,
        options: NdjsonOptions,
    ) -> std::result::Result<u64, FormatError> {
        self.ingest_ndjson_inner(table, input, options, None)
    }

    fn ingest_ndjson_inner<R: BufRead>(
        &mut self,
        table: &str,
        input: R,
        options: NdjsonOptions,
        control: Option<ExecutionControl<'_>>,
    ) -> std::result::Result<u64, FormatError> {
        let schema = self.ingestion_schema(table)?;
        let batches = NdjsonBatchReader::new(input, &schema, options)?;
        self.ingest_batches(table, batches, control)
    }

    fn ingestion_schema(&self, table: &str) -> std::result::Result<Schema, FormatError> {
        let schema = if let Some(transaction) = &self.transaction {
            transaction
                .tables
                .get(table)
                .ok_or_else(|| Error::TableNotFound(table.to_owned()))?
                .schema()
        } else {
            let snapshot = self.database.inner.snapshot()?;
            return Schema::try_from(
                snapshot
                    .tables
                    .get(table)
                    .ok_or_else(|| Error::TableNotFound(table.to_owned()))?
                    .schema(),
            )
            .map_err(FormatError::Storage);
        };
        Schema::try_from(schema).map_err(FormatError::Storage)
    }

    fn ingest_batches<I>(
        &mut self,
        table: &str,
        batches: I,
        control: Option<ExecutionControl<'_>>,
    ) -> std::result::Result<u64, FormatError>
    where
        I: IntoIterator<Item = std::result::Result<ColumnBatch, FormatError>>,
    {
        if let Some(transaction) = &mut self.transaction {
            return ingest_transaction_batches(transaction, table, batches, control);
        }

        let snapshot = self.database.inner.snapshot()?;
        let mut transaction = Transaction::new(snapshot, self.limits);
        let rows = ingest_transaction_batches(&mut transaction, table, batches, control)?;
        if rows == 0 {
            return Ok(0);
        }
        check_cancellation(control)?;
        self.database.inner.commit(&transaction, control)?;
        Ok(rows)
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
                let tables = if let Some(transaction) = &self.transaction {
                    &transaction.tables
                } else {
                    let snapshot = self.database.inner.snapshot()?;
                    return execute_read(&snapshot.tables, statement, control);
                };
                execute_read(tables, statement, control)
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

fn ingest_transaction_batches<I>(
    transaction: &mut Transaction,
    table: &str,
    batches: I,
    control: Option<ExecutionControl<'_>>,
) -> std::result::Result<u64, FormatError>
where
    I: IntoIterator<Item = std::result::Result<ColumnBatch, FormatError>>,
{
    let existing = transaction
        .tables
        .get(table)
        .ok_or_else(|| Error::TableNotFound(table.to_owned()))?;
    let mut replacement = existing.as_ref().clone();
    let mut rows = 0_usize;
    let mut bytes = table.len().saturating_add(8);
    let mut final_charge = None;

    for batch in batches {
        check_cancellation(control)?;
        let batch = batch?;
        rows = rows
            .checked_add(batch.rows())
            .ok_or_else(|| Error::InvalidRow("import row count overflowed".to_owned()))?;
        bytes = bytes.saturating_add(estimate_batch_bytes(&batch));
        final_charge = Some(transaction.prospective_charge(rows, bytes)?);
        replacement.append_batch(&batch)?;
    }
    check_cancellation(control)?;

    if rows == 0 {
        return Ok(0);
    }
    transaction
        .tables
        .insert(table.to_owned(), Arc::new(replacement));
    transaction.touched_tables.insert(table.to_owned());
    transaction.finish_charge(final_charge.expect("a non-empty import has a charge"));
    u64::try_from(rows)
        .map_err(|_| Error::InvalidRow("import row count is not representable".to_owned()).into())
}

fn estimate_batch_bytes(batch: &ColumnBatch) -> usize {
    batch.columns().iter().fold(0_usize, |total, column| {
        let column_bytes = match column {
            Column::Int64(values) => values.iter().fold(0_usize, |total, value| {
                total.saturating_add(if value.is_some() { 9 } else { 1 })
            }),
            Column::Float64(values) => values.iter().fold(0_usize, |total, value| {
                total.saturating_add(if value.is_some() { 9 } else { 1 })
            }),
            Column::Bool(values) => values.iter().fold(0_usize, |total, value| {
                total.saturating_add(if value.is_some() { 2 } else { 1 })
            }),
            Column::String(values) => values.iter().fold(0_usize, |total, value| {
                total.saturating_add(
                    value
                        .as_ref()
                        .map_or(1, |value| 9_usize.saturating_add(value.len())),
                )
            }),
        };
        total.saturating_add(column_bytes)
    })
}

fn execute_write(transaction: &mut Transaction, statement: Statement) -> Result<StatementResult> {
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
    let projection = match columns {
        Some(columns) => {
            let mut projection = Vec::with_capacity(columns.len());
            for (position, name) in columns.iter().enumerate() {
                if columns[..position].contains(name) {
                    return Err(Error::DuplicateColumn(name.clone()));
                }
                projection.push(
                    table
                        .column_index(name)
                        .ok_or_else(|| Error::ColumnNotFound(name.clone()))?,
                );
            }
            projection
        }
        None => (0..table.schema().len()).collect(),
    };
    let predicates = prepare_predicates(table, &predicates)?;
    let column_bytes = projection.iter().fold(
        projection
            .len()
            .saturating_mul(std::mem::size_of::<ColumnDef>()),
        |bytes, index| bytes.saturating_add(table.schema()[*index].name.len()),
    );
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
    for row in 0..table.row_count() {
        check_cancellation(control)?;
        if predicates.iter().all(|(column, comparison, value)| {
            compare(&table.value(row, *column), value, *comparison)
        }) {
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
    if control.is_some_and(|control| !control.cancellation.begin_publication()) {
        Err(Error::QueryCancelled)
    } else {
        Ok(())
    }
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

fn prepare_predicates(
    table: &Table,
    predicates: &[Predicate],
) -> Result<Vec<(usize, Comparison, Value)>> {
    predicates
        .iter()
        .map(|predicate| {
            let index = table
                .column_index(&predicate.column)
                .ok_or_else(|| Error::ColumnNotFound(predicate.column.clone()))?;
            let column = &table.schema()[index];
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
    use std::io::Cursor;
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

    struct BatchCancellation {
        checks: AtomicUsize,
        publication_attempts: AtomicUsize,
    }

    impl ExecutionCancellation for BatchCancellation {
        fn is_cancelled(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) >= 2
        }

        fn begin_publication(&self) -> bool {
            self.publication_attempts.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[test]
    fn cancelled_ingestion_discards_completed_batches_before_publication() {
        let database = Database::new();
        database.execute("CREATE TABLE events (id Int64)").unwrap();
        let generation = database.current_generation().unwrap();
        let cancellation = BatchCancellation {
            checks: AtomicUsize::new(0),
            publication_attempts: AtomicUsize::new(0),
        };
        let mut options = CsvOptions::default();
        options.limits.batch_rows = 1;

        assert!(matches!(
            database.ingest_csv_controlled(
                "events",
                Cursor::new(b"id\n1\n2\n3\n"),
                options,
                &cancellation,
            ),
            Err(FormatError::Database(Error::QueryCancelled))
        ));
        assert_eq!(cancellation.publication_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(database.current_generation().unwrap(), generation);
        assert!(matches!(
            database.execute("SELECT * FROM events"),
            Ok(StatementResult::Query(ResultSet { rows, .. })) if rows.is_empty()
        ));
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

#[cfg(all(test, unix))]
mod persistence_commit_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::persistence::fail_next_directory_sync;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

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
}
