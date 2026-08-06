//! Synchronized in-process access to the typed batch [`Database`].

use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use super::engine::{
    DEFAULT_MAX_RETAINED_RESULT_BYTES, Database, QueryResult, QueryResultLimits, StatementResult,
};
use super::error::Error;
use super::sql::{self, Statement};

/// An error produced while accessing a [`SharedDatabase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedDatabaseError {
    /// Parsing or executing the SQL batch failed.
    Sql(Error),
    /// The read-only query API received zero or multiple statements.
    QueryStatementCount { statements: usize },
    /// The read-only query API received a mutating statement.
    ReadOnlyStatementRequired { statement: &'static str },
    /// A thread panicked while it held the database write lock.
    LockPoisoned,
}

impl fmt::Display for SharedDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => error.fmt(formatter),
            Self::QueryStatementCount { statements } => write!(
                formatter,
                "read-only query requires exactly one statement; found {statements}"
            ),
            Self::ReadOnlyStatementRequired { statement } => write!(
                formatter,
                "read-only query accepts only SELECT, SHOW TABLES, SHOW CREATE TABLE, or DESCRIBE TABLE; found {statement}"
            ),
            Self::LockPoisoned => write!(formatter, "shared database lock is poisoned"),
        }
    }
}

impl StdError for SharedDatabaseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::QueryStatementCount { .. }
            | Self::ReadOnlyStatementRequired { .. }
            | Self::LockPoisoned => None,
        }
    }
}

impl From<Error> for SharedDatabaseError {
    fn from(error: Error) -> Self {
        Self::Sql(error)
    }
}

/// A clonable, synchronized handle to an in-memory typed [`Database`].
///
/// Each SQL batch is completely parsed before the database lock is acquired.
/// [`Self::execute`] retains one write lock while every statement executes, so
/// statements from concurrent mutating batches cannot interleave. [`Self::query`]
/// executes one `SELECT`, `SHOW TABLES`, `SHOW CREATE TABLE`, or `DESCRIBE
/// TABLE` under a shared read lock. Results own their columns and values and
/// remain valid after the lock is released.
///
/// A batch passed to [`Self::execute`] is not a rollback transaction: once
/// parsing succeeds, earlier statements remain applied if a later statement
/// fails. [`Self::execute_insert_batch`] provides atomic preflight and commit
/// for the narrower `INSERT`-only case.
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

    /// Creates an empty shared database with explicit per-query result limits.
    #[must_use]
    pub fn with_query_result_limits(query_result_limits: QueryResultLimits) -> Self {
        Self::new(Database::with_query_result_limits(query_result_limits))
    }

    /// Creates an empty shared database with an explicit per-table row cap.
    #[must_use]
    pub fn with_max_rows_per_table(max_rows_per_table: usize) -> Self {
        Self::new(Database::with_max_rows_per_table(max_rows_per_table))
    }

    /// Wraps an existing synchronized database allocation.
    ///
    /// Poisoning of the supplied lock is reported by every operation as
    /// [`SharedDatabaseError::LockPoisoned`].
    #[must_use]
    pub fn from_arc(inner: Arc<RwLock<Database>>) -> Self {
        Self { inner }
    }

    /// Returns the per-query result limits configured on the database.
    pub fn query_result_limits(&self) -> Result<QueryResultLimits, SharedDatabaseError> {
        Ok(self.read()?.query_result_limits())
    }

    /// Returns the maximum number of rows retained by each created table.
    pub fn max_rows_per_table(&self) -> Result<usize, SharedDatabaseError> {
        Ok(self.read()?.max_rows_per_table())
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
    /// `DROP TABLE`, `TRUNCATE TABLE`, `INSERT`, empty input, and multi-statement
    /// input are rejected before the lock is acquired.
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

    fn write(&self) -> Result<RwLockWriteGuard<'_, Database>, SharedDatabaseError> {
        self.inner
            .write()
            .map_err(|_| SharedDatabaseError::LockPoisoned)
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
        | Statement::Select(_)
        | Statement::CrossJoin(_)
        | Statement::UnionAll { .. }
        | Statement::ShowTables
        | Statement::ShowCreateTable { .. }
        | Statement::DescribeTable { .. }) => Ok(statement),
        Statement::CreateTable { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "CREATE TABLE",
        }),
        Statement::DropTable { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "DROP TABLE",
        }),
        Statement::TruncateTable { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "TRUNCATE TABLE",
        }),
        Statement::Insert { .. } => Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "INSERT",
        }),
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
