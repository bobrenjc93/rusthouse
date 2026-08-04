//! Synchronized in-process access to a [`Catalog`].

use std::error::Error;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{
    AggregateLimits, Catalog, CatalogCsvIngestError, CatalogError, CatalogLimits, CsvIngestLimits,
    DistinctLimits, ParseLimits, ScanLimits,
};

/// An error produced while accessing a [`SharedCatalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedCatalogError {
    /// The catalog rejected the parsed or executed statement.
    Catalog(CatalogError),
    /// The catalog rejected a CSV ingestion request.
    CsvIngest(CatalogCsvIngestError),
    /// A thread panicked while it held the catalog's write lock.
    LockPoisoned,
}

impl fmt::Display for SharedCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog operation failed: {error}"),
            Self::CsvIngest(error) => write!(formatter, "catalog CSV ingestion failed: {error}"),
            Self::LockPoisoned => write!(formatter, "shared catalog lock is poisoned"),
        }
    }
}

impl Error for SharedCatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::CsvIngest(error) => Some(error),
            Self::LockPoisoned => None,
        }
    }
}

impl From<CatalogError> for SharedCatalogError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<CatalogCsvIngestError> for SharedCatalogError {
    fn from(error: CatalogCsvIngestError) -> Self {
        Self::CsvIngest(error)
    }
}

/// A clonable, synchronized handle to an in-memory [`Catalog`].
///
/// CREATE, INSERT, and CSV ingestion operations take an exclusive write lock. SELECT
/// operations take a shared read lock, and projection results are copied into
/// owned vectors before the lock is released.
///
/// # Examples
///
/// ```
/// use rusthouse::{CatalogLimits, ParseLimits, SharedCatalog};
///
/// let catalog = SharedCatalog::with_limits(CatalogLimits::new(1, 2));
/// let other_handle = catalog.clone();
/// let parse_limits = ParseLimits::default();
///
/// catalog.execute_create("CREATE TABLE readings (value Int64)", parse_limits)?;
/// other_handle.execute_insert("INSERT INTO readings VALUES (7)", parse_limits)?;
///
/// let rows = catalog.execute_select("SELECT value FROM readings", parse_limits)?;
/// assert_eq!(rows, vec![Some(7)]);
/// # Ok::<(), rusthouse::SharedCatalogError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SharedCatalog {
    inner: Arc<RwLock<Catalog>>,
}

impl SharedCatalog {
    /// Wraps an existing catalog in a synchronized, reference-counted handle.
    pub fn new(catalog: Catalog) -> Self {
        Self::from_arc(Arc::new(RwLock::new(catalog)))
    }

    /// Creates an empty shared catalog with explicit resource bounds.
    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self::new(Catalog::new(limits))
    }

    /// Wraps an existing synchronized catalog allocation.
    ///
    /// This supports integration with code that already owns the allocation.
    /// Poisoning of the supplied lock is reported by every operation as
    /// [`SharedCatalogError::LockPoisoned`].
    pub fn from_arc(inner: Arc<RwLock<Catalog>>) -> Self {
        Self { inner }
    }

    /// Returns the catalog's configured resource bounds.
    pub fn limits(&self) -> Result<CatalogLimits, SharedCatalogError> {
        Ok(self.read()?.limits())
    }

    /// Returns the number of registered tables.
    pub fn len(&self) -> Result<usize, SharedCatalogError> {
        Ok(self.read()?.len())
    }

    /// Returns whether the catalog contains no tables.
    pub fn is_empty(&self) -> Result<bool, SharedCatalogError> {
        Ok(self.read()?.is_empty())
    }

    /// Parses and executes one bounded `CREATE TABLE` under a write lock.
    pub fn execute_create(
        &self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<(), SharedCatalogError> {
        self.write()?
            .execute_create(input, parse_limits)
            .map_err(Into::into)
    }

    /// Parses and executes one bounded `INSERT INTO ... VALUES` under a write lock.
    pub fn execute_insert(
        &self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<(), SharedCatalogError> {
        self.write()?
            .execute_insert(input, parse_limits)
            .map_err(Into::into)
    }

    /// Atomically ingests bounded `CSVWithNames` bytes under a write lock.
    pub fn ingest_csv_with_names(
        &self,
        table_name: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, SharedCatalogError> {
        self.write()?
            .ingest_csv_with_names(table_name, input, limits)
            .map_err(Into::into)
    }

    /// Parses and executes one projection `SELECT` under a read lock.
    ///
    /// The returned rows own their storage and remain valid after other handles
    /// mutate the catalog.
    pub fn execute_select(
        &self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<Vec<Option<i64>>, SharedCatalogError> {
        self.read()?
            .execute_select(input, parse_limits)
            .map(|rows| rows.into_owned())
            .map_err(Into::into)
    }

    /// Executes a projection `SELECT` under a read lock with explicit scan bounds.
    pub fn execute_select_with_limits(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        scan_limits: ScanLimits,
    ) -> Result<Vec<Option<i64>>, SharedCatalogError> {
        self.read()?
            .execute_select_with_limits(input, parse_limits, scan_limits)
            .map(|rows| rows.into_owned())
            .map_err(Into::into)
    }

    /// Executes a scalar `COUNT` under a read lock with explicit aggregate bounds.
    pub fn execute_scalar_count(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<u64, SharedCatalogError> {
        self.read()?
            .execute_scalar_count(input, parse_limits, aggregate_limits)
            .map_err(Into::into)
    }

    /// Executes a scalar `COUNT` under a read lock with explicit scan and aggregate bounds.
    pub fn execute_scalar_count_with_limits(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        scan_limits: ScanLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<u64, SharedCatalogError> {
        self.read()?
            .execute_scalar_count_with_limits(input, parse_limits, scan_limits, aggregate_limits)
            .map_err(Into::into)
    }

    /// Executes a scalar `SUM` under a read lock with explicit aggregate bounds.
    pub fn execute_scalar_sum(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<Option<i64>, SharedCatalogError> {
        self.read()?
            .execute_scalar_sum(input, parse_limits, aggregate_limits)
            .map_err(Into::into)
    }

    /// Executes a scalar `SUM` under a read lock with explicit scan and aggregate bounds.
    pub fn execute_scalar_sum_with_limits(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        scan_limits: ScanLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<Option<i64>, SharedCatalogError> {
        self.read()?
            .execute_scalar_sum_with_limits(input, parse_limits, scan_limits, aggregate_limits)
            .map_err(Into::into)
    }

    /// Executes `SELECT DISTINCT` under a read lock with explicit resource bounds.
    pub fn execute_select_distinct(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        distinct_limits: DistinctLimits,
    ) -> Result<Vec<Option<i64>>, SharedCatalogError> {
        self.read()?
            .execute_select_distinct(input, parse_limits, distinct_limits)
            .map_err(Into::into)
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, Catalog>, SharedCatalogError> {
        self.inner
            .read()
            .map_err(|_| SharedCatalogError::LockPoisoned)
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Catalog>, SharedCatalogError> {
        self.inner
            .write()
            .map_err(|_| SharedCatalogError::LockPoisoned)
    }
}

impl From<Catalog> for SharedCatalog {
    fn from(catalog: Catalog) -> Self {
        Self::new(catalog)
    }
}

impl From<Arc<RwLock<Catalog>>> for SharedCatalog {
    fn from(catalog: Arc<RwLock<Catalog>>) -> Self {
        Self::from_arc(catalog)
    }
}
