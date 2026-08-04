//! Synchronized in-process access to the typed batch [`Database`].

use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{
    DEFAULT_MAX_RETAINED_RESULT_BYTES, Database, QueryResultLimits, StatementResult,
};
use super::error::Error;
use super::sql;

/// An error produced while accessing a [`SharedDatabase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedDatabaseError {
    /// Parsing or executing the SQL batch failed.
    Sql(Error),
    /// A thread panicked while it held the database lock.
    LockPoisoned,
}

impl fmt::Display for SharedDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => error.fmt(formatter),
            Self::LockPoisoned => write!(formatter, "shared database lock is poisoned"),
        }
    }
}

impl StdError for SharedDatabaseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::LockPoisoned => None,
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
/// The lock then remains held while every statement executes, so statements
/// from concurrent batches cannot interleave. Results own their columns and
/// values and remain valid after the lock is released.
///
/// A batch is not a rollback transaction: once parsing succeeds, earlier
/// statements remain applied if a later statement fails.
///
/// # Examples
///
/// ```
/// use rusthouse::batch::engine::StatementResult;
/// use rusthouse::batch::value::Value;
/// use rusthouse::SharedDatabase;
///
/// let database = SharedDatabase::default();
/// let other_handle = database.clone();
///
/// database.execute("CREATE TABLE readings (value Int64);")?;
/// other_handle.execute("INSERT INTO readings VALUES (7), (-2);")?;
/// let results = database.execute("SELECT value FROM readings ORDER BY value;")?;
///
/// let StatementResult::Query(query) = &results[0] else {
///     panic!("the SELECT must produce a query result");
/// };
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::Int64(-2)], vec![Value::Int64(7)]],
/// );
/// # Ok::<(), rusthouse::SharedDatabaseError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SharedDatabase {
    inner: Arc<Mutex<Database>>,
}

impl SharedDatabase {
    /// Wraps an existing database in a synchronized, reference-counted handle.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self::from_arc(Arc::new(Mutex::new(database)))
    }

    /// Creates an empty shared database with explicit per-query result limits.
    #[must_use]
    pub fn with_query_result_limits(query_result_limits: QueryResultLimits) -> Self {
        Self::new(Database::with_query_result_limits(query_result_limits))
    }

    /// Wraps an existing synchronized database allocation.
    ///
    /// Poisoning of the supplied lock is reported by every operation as
    /// [`SharedDatabaseError::LockPoisoned`].
    #[must_use]
    pub fn from_arc(inner: Arc<Mutex<Database>>) -> Self {
        Self { inner }
    }

    /// Returns the per-query result limits configured on the database.
    pub fn query_result_limits(&self) -> Result<QueryResultLimits, SharedDatabaseError> {
        Ok(self.lock()?.query_result_limits())
    }

    /// Parses and executes a complete SQL batch under one database lock.
    pub fn execute(&self, input: &str) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        self.execute_with_result_limit(input, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Executes a batch under one lock while bounding all results retained for the caller.
    pub fn execute_with_result_limit(
        &self,
        input: &str,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>, SharedDatabaseError> {
        let statements = sql::parse(input)?;
        self.lock()?
            .execute_statements_with_result_limit(statements, max_result_bytes)
            .map_err(Into::into)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Database>, SharedDatabaseError> {
        self.inner
            .lock()
            .map_err(|_| SharedDatabaseError::LockPoisoned)
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

impl From<Arc<Mutex<Database>>> for SharedDatabase {
    fn from(database: Arc<Mutex<Database>>) -> Self {
        Self::from_arc(database)
    }
}
