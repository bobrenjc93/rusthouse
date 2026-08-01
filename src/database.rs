use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::catalog::CatalogGeneration;
use crate::error::{Error, LimitKind, Result};
use crate::persistence::{Persistence, StoreStatus};
use crate::sql::{Comparison, Predicate, Statement, parse};
use crate::storage::{ColumnDef, EngineTable as Table, Value};

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
        let statement = parse(sql)?;
        if matches!(
            statement,
            Statement::Begin | Statement::Commit | Statement::Rollback
        ) {
            return Err(Error::Unsupported(
                "transaction control requires a persistent Session".to_owned(),
            ));
        }
        self.session().execute_statement(statement)
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

    fn commit(&self, transaction: &Transaction) -> Result<u64> {
        let mut state = self.lock()?;
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
            persistence.store(&candidate)?
        } else {
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
        self.execute_statement(parse(sql)?)
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
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
                    return execute_read(&snapshot.tables, statement);
                };
                execute_read(tables, statement)
            }
            statement => {
                if let Some(transaction) = &mut self.transaction {
                    execute_write(transaction, statement)
                } else {
                    let snapshot = self.database.inner.snapshot()?;
                    let mut transaction = Transaction::new(snapshot, self.limits);
                    let result = execute_write(&mut transaction, statement)?;
                    self.database.inner.commit(&transaction)?;
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
        match self.database.inner.commit(&transaction) {
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
        Some(columns) => columns
            .iter()
            .map(|name| {
                table
                    .column_index(name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))
            })
            .collect::<Result<Vec<_>>>()?,
        None => (0..table.schema().len()).collect(),
    };
    let predicates = prepare_predicates(table, &predicates)?;
    let mut rows = Vec::new();
    for row in 0..table.row_count() {
        if predicates.iter().all(|(column, comparison, value)| {
            compare(&table.value(row, *column), value, *comparison)
        }) {
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
            if predicate.value != Value::Null
                && predicate.value.data_type() != Some(column.data_type)
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
        (Value::Float64(left), Value::Float64(right)) => left.partial_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => None,
    };
    match comparison {
        Comparison::Equal => left == right,
        Comparison::NotEqual => left != right,
        Comparison::Less => ordering == Some(Ordering::Less),
        Comparison::LessOrEqual => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        Comparison::Greater => ordering == Some(Ordering::Greater),
        Comparison::GreaterOrEqual => {
            matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
        }
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
