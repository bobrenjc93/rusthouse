//! Bounded ownership and SQL execution for named `Int64` tables.

use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::execution::{
    InsertExecutionError, SelectDistinctExecutionError, SelectExecutionError,
    execute_insert as execute_insert_statement,
    execute_scalar_count as execute_scalar_count_statement,
    execute_scalar_sum as execute_scalar_sum_statement,
    execute_select_distinct as execute_select_distinct_statement,
    execute_select_with_order_limits as execute_select_statement_with_limits,
};
use crate::{
    AggregateLimits, CreateTableStatement, CsvIngestError, CsvIngestLimits, DistinctLimits,
    InsertStatement, Int64Table, OrderLimits, ParseError, ParseLimits, ScalarCountStatement,
    ScalarSumStatement, ScanLimits, Schema, SelectDistinctStatement, SelectStatement,
    ingest_csv_with_names, parse_create_table, parse_insert, parse_scalar_count, parse_scalar_sum,
    parse_select, parse_select_distinct,
};

/// Resource bounds applied to an in-memory catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    /// Maximum number of tables owned by the catalog.
    pub max_tables: usize,
    /// Maximum number of rows stored in each created table.
    pub max_rows_per_table: usize,
}

impl CatalogLimits {
    /// Creates explicit table-count and per-table row bounds.
    pub const fn new(max_tables: usize, max_rows_per_table: usize) -> Self {
        Self {
            max_tables,
            max_rows_per_table,
        }
    }
}

/// An error produced while parsing or executing SQL through a [`Catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The input was rejected by the bounded SQL parser.
    Parse(ParseError),
    /// A table with the exact requested name is already registered.
    TableAlreadyExists { name: String },
    /// Creating another table would exceed the configured table bound.
    TableLimitExceeded { tables: usize, max_tables: usize },
    /// A parsed `INSERT` could not be executed.
    Insert(InsertExecutionError),
    /// A parsed `SELECT` could not be executed.
    Select(SelectExecutionError),
    /// A parsed `SELECT DISTINCT` could not be executed.
    SelectDistinct(SelectDistinctExecutionError),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "could not parse SQL: {error}"),
            Self::TableAlreadyExists { name } => {
                write!(formatter, "table '{name}' already exists")
            }
            Self::TableLimitExceeded { tables, max_tables } => write!(
                formatter,
                "catalog would contain {tables} tables, exceeding the limit of {max_tables}"
            ),
            Self::Insert(error) => write!(formatter, "could not execute INSERT: {error}"),
            Self::Select(error) => write!(formatter, "could not execute SELECT: {error}"),
            Self::SelectDistinct(error) => {
                write!(formatter, "could not execute SELECT DISTINCT: {error}")
            }
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Insert(error) => Some(error),
            Self::Select(error) => Some(error),
            Self::SelectDistinct(error) => Some(error),
            Self::TableAlreadyExists { .. } | Self::TableLimitExceeded { .. } => None,
        }
    }
}

impl From<ParseError> for CatalogError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// An error produced while ingesting CSV through a [`Catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogCsvIngestError {
    /// No table has the exact requested name.
    UnknownTable { name: String },
    /// The named table rejected the CSV input.
    Csv(CsvIngestError),
}

impl fmt::Display for CatalogCsvIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::Csv(error) => write!(formatter, "could not ingest CSV: {error}"),
        }
    }
}

impl Error for CatalogCsvIngestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Csv(error) => Some(error),
            Self::UnknownTable { .. } => None,
        }
    }
}

impl From<CsvIngestError> for CatalogCsvIngestError {
    fn from(error: CsvIngestError) -> Self {
        Self::Csv(error)
    }
}

/// A bounded in-memory catalog of named, one-column `Int64` tables.
///
/// Names use the exact spelling retained by the parser. Failed creates and
/// inserts leave all registered tables unchanged. Plain SELECT results borrow
/// their source column storage; filtered results own their matching values.
///
/// # Examples
///
/// ```
/// use rusthouse::{Catalog, CatalogLimits, ParseLimits};
///
/// let mut catalog = Catalog::new(CatalogLimits::new(2, 10));
/// let parse_limits = ParseLimits::default();
/// catalog.execute_create(
///     "CREATE TABLE readings (value Int64 NULL)",
///     parse_limits,
/// )?;
/// catalog.execute_insert(
///     "INSERT INTO readings VALUES (7)",
///     parse_limits,
/// )?;
///
/// let rows = catalog.execute_select(
///     "SELECT value FROM readings LIMIT 1",
///     parse_limits,
/// )?;
/// assert_eq!(rows.as_ref(), &[Some(7)]);
/// # Ok::<(), rusthouse::CatalogError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Catalog {
    tables: HashMap<String, Int64Table>,
    limits: CatalogLimits,
}

impl Catalog {
    /// Creates an empty catalog with explicit resource bounds.
    pub fn new(limits: CatalogLimits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    /// Returns the catalog's configured resource bounds.
    pub const fn limits(&self) -> CatalogLimits {
        self.limits
    }

    /// Returns the number of registered tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog contains no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Returns a table by exact name.
    pub fn table(&self, name: &str) -> Option<&Int64Table> {
        self.tables.get(name)
    }

    /// Returns a table mutably by exact name.
    pub fn table_mut(&mut self, name: &str) -> Option<&mut Int64Table> {
        self.tables.get_mut(name)
    }

    /// Parses and executes one bounded `CREATE TABLE` statement.
    pub fn execute_create(
        &mut self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<(), CatalogError> {
        let statement = parse_create_table(input, parse_limits)?;
        self.create(&statement)
    }

    /// Registers the empty table described by a parsed `CREATE TABLE`.
    pub fn create(&mut self, statement: &CreateTableStatement) -> Result<(), CatalogError> {
        let name = statement.table_name().as_str();
        if self.tables.contains_key(name) {
            return Err(CatalogError::TableAlreadyExists {
                name: name.to_owned(),
            });
        }

        if self.tables.len() >= self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                tables: self.tables.len().saturating_add(1),
                max_tables: self.limits.max_tables,
            });
        }

        let column = statement.column();
        let schema = Schema::int64(column.name().as_str(), column.is_nullable());
        let table = Int64Table::new(schema, self.limits.max_rows_per_table);
        self.tables.insert(name.to_owned(), table);
        Ok(())
    }

    /// Parses and executes one bounded `INSERT INTO ... VALUES` statement.
    pub fn execute_insert(
        &mut self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<(), CatalogError> {
        let statement = parse_insert(input, parse_limits)?;
        self.insert(&statement)
    }

    /// Executes one parsed `INSERT` against its exactly named table.
    pub fn insert(&mut self, statement: &InsertStatement) -> Result<(), CatalogError> {
        let name = statement.table_name().as_str();
        let table = self.tables.get_mut(name).ok_or_else(|| {
            CatalogError::Insert(InsertExecutionError::UnknownTable {
                name: name.to_owned(),
            })
        })?;

        execute_insert_statement(name, table, statement).map_err(CatalogError::Insert)
    }

    /// Atomically ingests bounded `CSVWithNames` bytes into an exactly named table.
    pub fn ingest_csv_with_names(
        &mut self,
        table_name: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> Result<usize, CatalogCsvIngestError> {
        let table =
            self.tables
                .get_mut(table_name)
                .ok_or_else(|| CatalogCsvIngestError::UnknownTable {
                    name: table_name.to_owned(),
                })?;

        ingest_csv_with_names(table, input, limits).map_err(Into::into)
    }

    /// Parses and executes one scalar `COUNT` with explicit resource bounds.
    pub fn execute_scalar_count(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<u64, CatalogError> {
        let statement = parse_scalar_count(input, parse_limits)?;
        self.scalar_count(&statement, aggregate_limits)
    }

    /// Executes a parsed scalar `COUNT` against its exactly named table.
    pub fn scalar_count(
        &self,
        statement: &ScalarCountStatement,
        limits: AggregateLimits,
    ) -> Result<u64, CatalogError> {
        let name = statement.table_name().as_str();
        let table = self.tables.get(name).ok_or_else(|| {
            CatalogError::Select(SelectExecutionError::UnknownTable {
                name: name.to_owned(),
            })
        })?;

        execute_scalar_count_statement(name, table, statement, limits).map_err(CatalogError::Select)
    }

    /// Parses and executes one scalar `SUM` with explicit resource bounds.
    pub fn execute_scalar_sum(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        aggregate_limits: AggregateLimits,
    ) -> Result<Option<i64>, CatalogError> {
        let statement = parse_scalar_sum(input, parse_limits)?;
        self.scalar_sum(&statement, aggregate_limits)
    }

    /// Executes a parsed scalar `SUM` against its exactly named table.
    pub fn scalar_sum(
        &self,
        statement: &ScalarSumStatement,
        limits: AggregateLimits,
    ) -> Result<Option<i64>, CatalogError> {
        let name = statement.table_name().as_str();
        let table = self.tables.get(name).ok_or_else(|| {
            CatalogError::Select(SelectExecutionError::UnknownTable {
                name: name.to_owned(),
            })
        })?;

        execute_scalar_sum_statement(name, table, statement, limits).map_err(CatalogError::Select)
    }

    /// Parses and executes one bounded projection `SELECT` statement.
    pub fn execute_select(
        &self,
        input: &str,
        parse_limits: ParseLimits,
    ) -> Result<Cow<'_, [Option<i64>]>, CatalogError> {
        self.execute_select_with_limits(
            input,
            parse_limits,
            ScanLimits::new(
                self.limits.max_rows_per_table,
                self.limits.max_rows_per_table,
            ),
        )
    }

    /// Parses and executes a `SELECT` with explicit predicate-scan bounds.
    pub fn execute_select_with_limits(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        scan_limits: ScanLimits,
    ) -> Result<Cow<'_, [Option<i64>]>, CatalogError> {
        let statement = parse_select(input, parse_limits)?;
        self.select_with_limits(&statement, scan_limits)
    }

    /// Executes one parsed projection `SELECT` against its exactly named table.
    pub fn select(
        &self,
        statement: &SelectStatement,
    ) -> Result<Cow<'_, [Option<i64>]>, CatalogError> {
        self.select_with_limits(
            statement,
            ScanLimits::new(
                self.limits.max_rows_per_table,
                self.limits.max_rows_per_table,
            ),
        )
    }

    /// Executes a parsed projection `SELECT` with explicit predicate-scan bounds.
    pub fn select_with_limits(
        &self,
        statement: &SelectStatement,
        scan_limits: ScanLimits,
    ) -> Result<Cow<'_, [Option<i64>]>, CatalogError> {
        let name = statement.table_name().as_str();
        let table = self.tables.get(name).ok_or_else(|| {
            CatalogError::Select(SelectExecutionError::UnknownTable {
                name: name.to_owned(),
            })
        })?;

        execute_select_statement_with_limits(
            name,
            table,
            statement,
            scan_limits,
            OrderLimits::new(
                self.limits.max_rows_per_table,
                self.limits.max_rows_per_table,
            ),
        )
        .map_err(CatalogError::Select)
    }

    /// Parses and executes a `SELECT DISTINCT` with explicit resource bounds.
    pub fn execute_select_distinct(
        &self,
        input: &str,
        parse_limits: ParseLimits,
        distinct_limits: DistinctLimits,
    ) -> Result<Vec<Option<i64>>, CatalogError> {
        let statement = parse_select_distinct(input, parse_limits)?;
        self.select_distinct(&statement, distinct_limits)
    }

    /// Executes a parsed `SELECT DISTINCT` with explicit resource bounds.
    pub fn select_distinct(
        &self,
        statement: &SelectDistinctStatement,
        limits: DistinctLimits,
    ) -> Result<Vec<Option<i64>>, CatalogError> {
        let name = statement.table_name().as_str();
        let table = self.tables.get(name).ok_or_else(|| {
            CatalogError::SelectDistinct(SelectDistinctExecutionError::UnknownTable {
                name: name.to_owned(),
            })
        })?;

        execute_select_distinct_statement(name, table, statement, limits)
            .map_err(CatalogError::SelectDistinct)
    }
}
