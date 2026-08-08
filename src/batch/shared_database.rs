//! Synchronized in-process access to the typed batch [`Database`].

use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use super::csv::{CsvIngestError, CsvIngestLimits};
use super::engine::{
    DEFAULT_MAX_RETAINED_RESULT_BYTES, Database, QueryResult, QueryResultLimits, StatementResult,
    TableLimits,
};
use super::error::Error;
use super::sql::{self, Statement};
use super::tsv::{TsvIngestError, TsvIngestLimits};

/// An instantaneous measurement of data retained by a [`SharedDatabase`].
///
/// The values describe one consistent database read-lock acquisition. They do
/// not include query results or configured capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseMetrics {
    /// Number of registered tables.
    pub table_count: usize,
    /// Number of schema columns across all registered tables.
    pub column_count: usize,
    /// Number of rows retained across all registered tables.
    pub retained_row_count: usize,
    /// Scalar payload bytes retained across all registered tables.
    pub retained_value_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseMetricsWithTableRows {
    pub(crate) totals: DatabaseMetrics,
    pub(crate) table_rows: Vec<(String, usize)>,
}

/// An error produced while accessing a [`SharedDatabase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedDatabaseError {
    /// Parsing or executing the SQL batch failed.
    Sql(Error),
    /// The database rejected a CSV ingestion request.
    CsvIngest(CsvIngestError),
    /// The database rejected a TSV ingestion request.
    TsvIngest(TsvIngestError),
    /// The read-only query API received zero or multiple statements.
    QueryStatementCount { statements: usize },
    /// The read-only query API received a mutating statement.
    ReadOnlyStatementRequired { statement: &'static str },
    /// A nonblocking operation could not immediately acquire its database lock.
    DatabaseBusy,
    /// A thread panicked while it held the database write lock.
    LockPoisoned,
}

impl fmt::Display for SharedDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => error.fmt(formatter),
            Self::CsvIngest(error) => write!(formatter, "database CSV ingestion failed: {error}"),
            Self::TsvIngest(error) => write!(formatter, "database TSV ingestion failed: {error}"),
            Self::QueryStatementCount { statements } => write!(
                formatter,
                "read-only query requires exactly one statement; found {statements}"
            ),
            Self::ReadOnlyStatementRequired { statement } => write!(
                formatter,
                "read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found {statement}"
            ),
            Self::DatabaseBusy => write!(formatter, "shared database is busy"),
            Self::LockPoisoned => write!(formatter, "shared database lock is poisoned"),
        }
    }
}

impl StdError for SharedDatabaseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::CsvIngest(error) => Some(error),
            Self::TsvIngest(error) => Some(error),
            Self::QueryStatementCount { .. }
            | Self::ReadOnlyStatementRequired { .. }
            | Self::DatabaseBusy
            | Self::LockPoisoned => None,
        }
    }
}

impl From<Error> for SharedDatabaseError {
    fn from(error: Error) -> Self {
        Self::Sql(error)
    }
}

impl From<CsvIngestError> for SharedDatabaseError {
    fn from(error: CsvIngestError) -> Self {
        Self::CsvIngest(error)
    }
}

impl From<TsvIngestError> for SharedDatabaseError {
    fn from(error: TsvIngestError) -> Self {
        Self::TsvIngest(error)
    }
}

/// A clonable, synchronized handle to an in-memory typed [`Database`].
///
/// Each SQL batch is completely parsed before the database lock is acquired.
/// [`Self::execute`] retains one write lock while every statement executes, so
/// statements from concurrent mutating batches cannot interleave. [`Self::query`]
/// executes one `SELECT`, `SHOW DATABASES`, `SHOW SETTINGS`, `SHOW FUNCTIONS`,
/// `SHOW TABLES`, `SHOW CREATE TABLE`, `DESCRIBE TABLE`, or `EXISTS TABLE`
/// under a shared read lock.
/// [`Self::try_query`] accepts the same input but returns
/// [`SharedDatabaseError::DatabaseBusy`] instead of waiting for a writer.
/// [`Self::try_execute_insert_batch`] similarly attempts
/// one nonblocking write lock for an atomic `INSERT`-only batch, and
/// [`Self::try_ingest_csv`], [`Self::try_ingest_csv_with_names`],
/// [`Self::try_ingest_tsv`], and [`Self::try_ingest_tsv_with_names`] do the same
/// for headerless `CSV`, `CSVWithNames`, headerless `TabSeparated`, and
/// `TabSeparatedWithNames` ingestion. Results own their columns and values and
/// remain valid after the lock is released.
///
/// A batch passed to [`Self::execute`] is not a rollback transaction: once
/// parsing succeeds, earlier statements remain applied if a later statement
/// fails. [`Self::execute_insert_batch`] provides atomic preflight and commit
/// for the narrower `INSERT`-only case. [`Self::ingest_csv`],
/// [`Self::ingest_csv_with_names`], [`Self::ingest_tsv`], and
/// [`Self::ingest_tsv_with_names`] retain a write lock through their complete
/// bounded, atomic import operations.
///
/// # Examples
///
/// ```
/// use rusthouse::batch::value::Value;
/// use rusthouse::SharedDatabase;
///
/// let database = SharedDatabase::default();
/// let other_handle = database.clone();
///
/// database.execute("CREATE TABLE readings (value Int64);")?;
/// other_handle.execute("INSERT INTO readings VALUES (7), (-2);")?;
/// let query = database.query("SELECT value FROM readings ORDER BY value;")?;
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::Int64(-2)], vec![Value::Int64(7)]],
/// );
/// # Ok::<(), rusthouse::SharedDatabaseError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SharedDatabase {
    inner: Arc<RwLock<Database>>,
}

impl SharedDatabase {
    /// Wraps an existing database in a synchronized, reference-counted handle.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self::from_arc(Arc::new(RwLock::new(database)))
    }

    /// Creates an empty shared database with explicit per-query resource limits.
    #[must_use]
    pub fn with_query_result_limits(query_result_limits: QueryResultLimits) -> Self {
        Self::new(Database::with_query_result_limits(query_result_limits))
    }

    /// Creates an empty shared database with an explicit row cap and default column and cell caps.
    #[must_use]
    pub fn with_max_rows_per_table(max_rows_per_table: usize) -> Self {
        Self::new(Database::with_max_rows_per_table(max_rows_per_table))
    }

    /// Creates an empty shared database with explicit persistent per-table limits.
    #[must_use]
    pub fn with_table_limits(table_limits: TableLimits) -> Self {
        Self::new(Database::with_table_limits(table_limits))
    }

    /// Wraps an existing synchronized database allocation.
    ///
    /// Poisoning of the supplied lock is reported by every operation as
    /// [`SharedDatabaseError::LockPoisoned`].
    #[must_use]
    pub fn from_arc(inner: Arc<RwLock<Database>>) -> Self {
        Self { inner }
    }

    /// Returns the per-query resource limits configured on the database.
    pub fn query_result_limits(&self) -> Result<QueryResultLimits, SharedDatabaseError> {
        Ok(self.read()?.query_result_limits())
    }

    /// Returns the maximum number of rows retained by each created table.
    pub fn max_rows_per_table(&self) -> Result<usize, SharedDatabaseError> {
        Ok(self.read()?.max_rows_per_table())
    }

    /// Returns the persistent resource limits applied to each created table.
    pub fn table_limits(&self) -> Result<TableLimits, SharedDatabaseError> {
        Ok(self.read()?.table_limits())
    }

    /// Takes a consistent, constant-time metrics snapshot without waiting for
    /// the database lock.
    ///
    /// Returns `None` when a read lock is not immediately available or when the
    /// lock is poisoned. Cached database totals are read without scanning tables
    /// or values, and the acquired read guard is released before this method
    /// returns.
    #[must_use]
    pub fn metrics_snapshot(&self) -> Option<DatabaseMetrics> {
        let database = match self.inner.try_read() {
            Ok(database) => database,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return None,
        };
        let (table_count, column_count, retained_row_count, retained_value_bytes) =
            database.retained_metrics();
        Some(DatabaseMetrics {
            table_count,
            column_count,
            retained_row_count,
            retained_value_bytes,
        })
    }

    /// Captures database totals plus owned per-table row counts under one
    /// nonblocking read-lock attempt.
    ///
    /// The table entries are sorted by case-insensitive name and the read guard
    /// is released before the owned snapshot is returned for response writing.
    pub(crate) fn metrics_snapshot_with_table_rows(&self) -> Option<DatabaseMetricsWithTableRows> {
        let database = match self.inner.try_read() {
            Ok(database) => database,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return None,
        };
        let (table_count, column_count, retained_row_count, retained_value_bytes) =
            database.retained_metrics();
        let table_rows = database.table_row_counts();
        drop(database);
        Some(DatabaseMetricsWithTableRows {
            totals: DatabaseMetrics {
                table_count,
                column_count,
                retained_row_count,
                retained_value_bytes,
            },
            table_rows,
        })
    }

    /// Parses and executes a complete SQL batch under one database lock.
    pub fn execute(&self, input: &str) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        self.execute_with_result_limit(input, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Atomically executes a nonempty, `INSERT`-only batch under one write lock.
    ///
    /// Parsing completes before the lock is acquired. Preflight and ordered
    /// commit both occur while the same write guard is retained, so neither a
    /// validation failure nor a concurrent operation can expose a partial batch.
    pub fn execute_insert_batch(
        &self,
        input: &str,
    ) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        let statements = sql::parse(input)?;
        self.write()?
            .execute_insert_statements(statements)
            .map_err(Into::into)
    }

    /// Attempts to atomically execute a nonempty, `INSERT`-only batch without waiting.
    ///
    /// Parsing completes before the single write-lock attempt. Preflight and
    /// ordered commit both occur while the same write guard is retained. If a
    /// reader or writer prevents immediate lock acquisition, this returns
    /// [`SharedDatabaseError::DatabaseBusy`] without applying any rows.
    pub fn try_execute_insert_batch(
        &self,
        input: &str,
    ) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        let statements = sql::parse(input)?;
        self.try_write()?
            .execute_insert_statements(statements)
            .map_err(Into::into)
    }

    /// Atomically ingests bounded, headerless `CSV` bytes under one write lock.
    ///
    /// Every logical record is data in physical schema order. The lock is
    /// retained through table lookup, parsing, limit and remaining-capacity
    /// validation, and the final append. Empty input is a zero-row no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::csv::CsvIngestLimits;
    /// use rusthouse::SharedDatabase;
    ///
    /// let database = SharedDatabase::default();
    /// database.execute("CREATE TABLE readings (value Int64, note String);")?;
    /// let input = b"7,ready\n";
    /// let rows = database.ingest_csv(
    ///     "readings",
    ///     input,
    ///     CsvIngestLimits::new(input.len(), 1, 2),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), rusthouse::SharedDatabaseError>(())
    /// ```
    pub fn ingest_csv(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        self.write()?
            .ingest_csv(table, input, limits)
            .map_err(Into::into)
    }

    /// Attempts one bounded, atomic, headerless `CSV` ingestion without waiting.
    ///
    /// Exactly one immediate write-lock attempt occurs before table lookup or
    /// input access. The acquired guard is retained through parsing, all limit
    /// and remaining-capacity validation, and commit. An active reader or
    /// writer returns [`SharedDatabaseError::DatabaseBusy`] without inspecting
    /// the table or input. Empty input appends zero rows after lock acquisition.
    pub fn try_ingest_csv(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        let mut database = self.try_write()?;
        database
            .ingest_csv(table, input, limits)
            .map_err(Into::into)
    }

    /// Atomically ingests bounded `CSVWithNames` bytes under one write lock.
    ///
    /// The lock is retained through table lookup, parsing, limit and capacity
    /// validation, and the final append, so concurrent operations cannot
    /// expose partial input or change the table between validation and commit.
    pub fn ingest_csv_with_names(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        self.write()?
            .ingest_csv_with_names(table, input, limits)
            .map_err(Into::into)
    }

    /// Attempts one bounded, atomic `CSVWithNames` ingestion without waiting.
    ///
    /// Exactly one immediate write-lock attempt occurs before table lookup or
    /// input access. The acquired guard is retained through parsing, limit and
    /// capacity validation, and commit. An active reader or writer therefore
    /// returns [`SharedDatabaseError::DatabaseBusy`] without inspecting the
    /// table or input, while lock poisoning and CSV ingestion failures retain
    /// their distinct typed errors.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::csv::CsvIngestLimits;
    /// use rusthouse::SharedDatabase;
    ///
    /// let database = SharedDatabase::default();
    /// database.execute("CREATE TABLE readings (value Int64);")?;
    /// let input = b"value\n7\n";
    /// let rows = database.try_ingest_csv_with_names(
    ///     "readings",
    ///     input,
    ///     CsvIngestLimits::new(input.len(), 1, 1),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), rusthouse::SharedDatabaseError>(())
    /// ```
    pub fn try_ingest_csv_with_names(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        let mut database = self.try_write()?;
        database
            .ingest_csv_with_names(table, input, limits)
            .map_err(Into::into)
    }

    /// Atomically ingests bounded, headerless `TabSeparated` bytes under one write lock.
    ///
    /// Every physical line is data in physical schema order. The lock is
    /// retained through table lookup, parsing, limit and remaining-capacity
    /// validation, and the final append. Empty input is a zero-row no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::tsv::TsvIngestLimits;
    /// use rusthouse::SharedDatabase;
    ///
    /// let database = SharedDatabase::default();
    /// database.execute("CREATE TABLE readings (value Int64, note String);")?;
    /// let input = b"7\tready\n";
    /// let rows = database.ingest_tsv(
    ///     "readings",
    ///     input,
    ///     TsvIngestLimits::new(input.len(), 1, 2),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), rusthouse::SharedDatabaseError>(())
    /// ```
    pub fn ingest_tsv(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        self.write()?
            .ingest_tsv(table, input, limits)
            .map_err(Into::into)
    }

    /// Attempts one bounded, atomic, headerless `TabSeparated` ingestion without waiting.
    ///
    /// Exactly one immediate write-lock attempt occurs before table lookup or
    /// input access. The acquired guard is retained through parsing, all limit
    /// and remaining-capacity validation, and commit. An active reader or
    /// writer returns [`SharedDatabaseError::DatabaseBusy`] without inspecting
    /// the table or input. Empty input appends zero rows after lock acquisition.
    pub fn try_ingest_tsv(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        let mut database = self.try_write()?;
        database
            .ingest_tsv(table, input, limits)
            .map_err(Into::into)
    }

    /// Atomically ingests bounded `TabSeparatedWithNames` bytes under one write lock.
    ///
    /// The lock is retained through parsing, limit and capacity validation, and
    /// the final append, so concurrent operations cannot expose partial input.
    pub fn ingest_tsv_with_names(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        self.write()?
            .ingest_tsv_with_names(table, input, limits)
            .map_err(Into::into)
    }

    /// Attempts one bounded, atomic `TabSeparatedWithNames` ingestion without waiting.
    ///
    /// Exactly one immediate write-lock attempt occurs before table lookup or
    /// input access. The acquired guard is retained through parsing, limit and
    /// capacity validation, and commit. An active reader or writer therefore
    /// returns [`SharedDatabaseError::DatabaseBusy`] without inspecting the
    /// table or input, while lock poisoning and TSV ingestion failures retain
    /// their distinct typed errors.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::tsv::TsvIngestLimits;
    /// use rusthouse::SharedDatabase;
    ///
    /// let database = SharedDatabase::default();
    /// database.execute("CREATE TABLE readings (value Int64);")?;
    /// let input = b"value\n7\n";
    /// let rows = database.try_ingest_tsv_with_names(
    ///     "readings",
    ///     input,
    ///     TsvIngestLimits::new(input.len(), 1, 1),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), rusthouse::SharedDatabaseError>(())
    /// ```
    pub fn try_ingest_tsv_with_names(
        &self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> Result<usize, SharedDatabaseError> {
        let mut database = self.try_write()?;
        database
            .ingest_tsv_with_names(table, input, limits)
            .map_err(Into::into)
    }

    /// Executes a batch under one lock while bounding all results retained for the caller.
    pub fn execute_with_result_limit(
        &self,
        input: &str,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        let statements = sql::parse(input)?;
        self.write()?
            .execute_statements_with_result_limit(statements, max_result_bytes)
            .map_err(Into::into)
    }

    /// Parses and executes exactly one read-only query under a read lock.
    ///
    /// The returned result owns all of its columns and values. `CREATE TABLE`,
    /// `DROP TABLE`, `RENAME TABLE`, `ALTER TABLE`, `TRUNCATE TABLE`, `DELETE`,
    /// `INSERT`, empty input, and multi-statement input are rejected before the
    /// lock is acquired.
    pub fn query(&self, input: &str) -> Result<QueryResult, SharedDatabaseError> {
        self.query_with_result_limit(input, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Executes one read-only query with an explicit retained-result byte limit.
    pub fn query_with_result_limit(
        &self,
        input: &str,
        max_result_bytes: usize,
    ) -> Result<QueryResult, SharedDatabaseError> {
        let statement = parse_query_statement(input)?;
        self.read()?
            .execute_query_statement_with_result_limit(statement, max_result_bytes)
            .map_err(Into::into)
    }

    /// Attempts to execute exactly one read-only query without waiting for a lock.
    ///
    /// Parsing, statement-count validation, and read-only validation all finish
    /// before the single read-lock attempt. If a writer prevents immediate lock
    /// acquisition, this returns [`SharedDatabaseError::DatabaseBusy`].
    pub fn try_query(&self, input: &str) -> Result<QueryResult, SharedDatabaseError> {
        self.try_query_with_result_limit(input, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Attempts one read-only query with an explicit retained-result byte limit.
    ///
    /// This has the same validation and execution semantics as
    /// [`Self::query_with_result_limit`], but never waits to acquire the database
    /// read lock. Lock poisoning, SQL failures, and resource-limit failures
    /// retain their distinct typed errors.
    pub fn try_query_with_result_limit(
        &self,
        input: &str,
        max_result_bytes: usize,
    ) -> Result<QueryResult, SharedDatabaseError> {
        let statement = parse_query_statement(input)?;
        self.try_read()?
            .execute_query_statement_with_result_limit(statement, max_result_bytes)
            .map_err(Into::into)
    }

    /// Reports whether a database read lock is immediately available.
    ///
    /// This check never waits, parses SQL, or accesses database contents. A
    /// contended or poisoned lock is unavailable. The acquired read guard is
    /// released before this method returns.
    pub(crate) fn is_read_lock_available(&self) -> bool {
        match self.inner.try_read() {
            Ok(_guard) => true,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => false,
        }
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, Database>, SharedDatabaseError> {
        self.inner
            .read()
            .map_err(|_| SharedDatabaseError::LockPoisoned)
    }

    fn try_read(&self) -> Result<RwLockReadGuard<'_, Database>, SharedDatabaseError> {
        match self.inner.try_read() {
            Ok(database) => Ok(database),
            Err(TryLockError::WouldBlock) => Err(SharedDatabaseError::DatabaseBusy),
            Err(TryLockError::Poisoned(_)) => Err(SharedDatabaseError::LockPoisoned),
        }
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Database>, SharedDatabaseError> {
        self.inner
            .write()
            .map_err(|_| SharedDatabaseError::LockPoisoned)
    }

    fn try_write(&self) -> Result<RwLockWriteGuard<'_, Database>, SharedDatabaseError> {
        match self.inner.try_write() {
            Ok(database) => Ok(database),
            Err(TryLockError::WouldBlock) => Err(SharedDatabaseError::DatabaseBusy),
            Err(TryLockError::Poisoned(_)) => Err(SharedDatabaseError::LockPoisoned),
        }
    }
}

fn parse_query_statement(input: &str) -> Result<Statement, SharedDatabaseError> {
    let mut statements = sql::parse_allow_empty(input)?;
    if statements.len() != 1 {
        return Err(SharedDatabaseError::QueryStatementCount {
            statements: statements.len(),
        });
    }
    let statement = statements.pop().expect("the statement count is one");
    match statement {
        statement @ (Statement::LiteralSelect(_)
        | Statement::VersionSelect(_)
        | Statement::CurrentDatabaseSelect(_)
        | Statement::Select(_)
        | Statement::CrossJoin(_)
        | Statement::UnionAll { .. }
        | Statement::UnionDistinct { .. }
        | Statement::ShowDatabases
        | Statement::ShowSettings
        | Statement::ShowFunctions
        | Statement::ShowTables
        | Statement::ShowCreateTable { .. }
        | Statement::DescribeTable { .. }
        | Statement::ExistsTable { .. }) => Ok(statement),
        Statement::CreateTable { .. } | Statement::CreateTableIfNotExists { .. } => {
            Err(SharedDatabaseError::ReadOnlyStatementRequired {
                statement: "CREATE TABLE",
            })
        }
        Statement::DropTable { .. } | Statement::DropTableIfExists { .. } => {
            Err(SharedDatabaseError::ReadOnlyStatementRequired {
                statement: "DROP TABLE",
            })
        }
        Statement::RenameTable { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "RENAME TABLE",
        }),
        Statement::RenameColumn { .. }
        | Statement::AddColumn { .. }
        | Statement::DropColumn { .. }
        | Statement::AlterUpdate { .. }
        | Statement::AlterUpdateTyped { .. }
        | Statement::AlterUpdateOwned { .. } => {
            Err(SharedDatabaseError::ReadOnlyStatementRequired {
                statement: "ALTER TABLE",
            })
        }
        Statement::TruncateTable { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "TRUNCATE TABLE",
        }),
        Statement::Delete { .. }
        | Statement::DeleteComparison { .. }
        | Statement::DeleteConjunction { .. } => {
            Err(SharedDatabaseError::ReadOnlyStatementRequired {
                statement: "DELETE",
            })
        }
        Statement::Insert { .. } | Statement::InsertWithColumns { .. } => {
            Err(SharedDatabaseError::ReadOnlyStatementRequired {
                statement: "INSERT",
            })
        }
    }
}

impl Default for SharedDatabase {
    fn default() -> Self {
        Self::new(Database::new())
    }
}

impl From<Database> for SharedDatabase {
    fn from(database: Database) -> Self {
        Self::new(database)
    }
}

impl From<Arc<RwLock<Database>>> for SharedDatabase {
    fn from(database: Arc<RwLock<Database>>) -> Self {
        Self::from_arc(database)
    }
}
