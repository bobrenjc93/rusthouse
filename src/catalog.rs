//! In-memory ownership and `CREATE TABLE`/`INSERT` execution.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::sql::{
    CreateTableStatement, InsertParseLimits, InsertStatement, ParseError, ParseLimits,
    parse_create_table_with_limits, parse_insert_with_limits,
};
use crate::storage::{DEFAULT_ROW_LIMIT, Field, Table, TableError};

/// Default maximum number of tables owned by one [`Catalog`].
pub const DEFAULT_MAX_TABLES: usize = 1024;

/// Resource limits applied by a [`Catalog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogLimits {
    /// Limits applied while parsing each `CREATE TABLE` statement.
    pub parse: ParseLimits,
    /// Limits applied while parsing each `INSERT` statement.
    pub insert_parse: InsertParseLimits,
    /// Maximum number of tables the catalog may own.
    pub max_tables: usize,
    /// Maximum number of rows accepted by each newly created table.
    pub max_rows_per_table: usize,
}

impl CatalogLimits {
    /// Creates catalog limits with the default bounded `INSERT` parser limits.
    #[must_use]
    pub const fn new(parse: ParseLimits, max_tables: usize, max_rows_per_table: usize) -> Self {
        Self {
            parse,
            insert_parse: default_insert_parse_limits(),
            max_tables,
            max_rows_per_table,
        }
    }

    /// Replaces the limits applied while parsing `INSERT` statements.
    #[must_use]
    pub const fn with_insert_parse_limits(mut self, insert_parse: InsertParseLimits) -> Self {
        self.insert_parse = insert_parse;
        self
    }
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self::new(
            ParseLimits::default(),
            DEFAULT_MAX_TABLES,
            DEFAULT_ROW_LIMIT,
        )
    }
}

/// A deterministic failure from catalog lookup or statement execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// SQL could not be parsed as the supported statement syntax.
    Parse(ParseError),
    /// The catalog already owns a table with this case-insensitive name.
    DuplicateTable {
        /// Name from the rejected statement.
        name: String,
    },
    /// A requested table does not exist.
    TableNotFound {
        /// Name used for the failed lookup.
        name: String,
    },
    /// The parsed schema could not be used to construct a table.
    TableConstruction {
        /// Name from the statement whose schema was rejected.
        name: String,
        /// Storage-layer validation failure.
        source: TableError,
    },
    /// A parsed batch could not be inserted into its target table.
    TableInsertion {
        /// Name from the rejected statement.
        name: String,
        /// Storage-layer validation or capacity failure.
        source: TableError,
    },
    /// Creating another table would exceed the configured catalog bound.
    TableLimitExceeded {
        /// Maximum number of tables allowed in the catalog.
        limit: usize,
    },
    /// Memory could not be reserved for another catalog entry.
    AllocationFailed,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(formatter),
            Self::DuplicateTable { name } => write!(formatter, "table `{name}` already exists"),
            Self::TableNotFound { name } => write!(formatter, "table `{name}` does not exist"),
            Self::TableConstruction { name, source } => {
                write!(formatter, "could not construct table `{name}`: {source}")
            }
            Self::TableInsertion { name, source } => {
                write!(formatter, "could not insert into table `{name}`: {source}")
            }
            Self::TableLimitExceeded { limit } => {
                write!(formatter, "catalog table count exceeds limit of {limit}")
            }
            Self::AllocationFailed => {
                formatter.write_str("could not reserve memory for another catalog table")
            }
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::TableConstruction { source, .. } | Self::TableInsertion { source, .. } => {
                Some(source)
            }
            _ => None,
        }
    }
}

impl From<ParseError> for CatalogError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug)]
struct CatalogEntry {
    name: String,
    table: Table,
}

/// A bounded collection of named, in-memory tables.
///
/// Table names originate from unquoted SQL identifiers, so lookup and
/// duplicate detection are ASCII case-insensitive. Field names and their
/// declaration order remain exactly as written in the `CREATE TABLE`
/// statement.
#[derive(Debug)]
pub struct Catalog {
    tables: HashMap<String, CatalogEntry>,
    limits: CatalogLimits,
}

impl Catalog {
    /// Creates an empty catalog with default parser, table-count, and row limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(CatalogLimits::default())
    }

    /// Creates an empty catalog with explicit resource limits.
    #[must_use]
    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    /// Parses and executes one `CREATE TABLE` statement.
    ///
    /// Parsing, duplicate checks, table construction, and allocation complete
    /// before a new entry is inserted. Any returned error therefore leaves the
    /// set of catalog tables unchanged.
    pub fn execute_create(&mut self, input: &str) -> Result<&Table, CatalogError> {
        let statement = parse_create_table_with_limits(input, self.limits.parse)?;
        self.create_table(statement)
    }

    /// Creates a table from an already parsed statement.
    ///
    /// This is the typed execution boundary used by [`Self::execute_create`].
    /// It is also useful to callers which parse statements separately.
    pub fn create_table(
        &mut self,
        statement: CreateTableStatement,
    ) -> Result<&Table, CatalogError> {
        let name = statement.name;
        let key = normalize_table_name(&name);

        if self.tables.contains_key(&key) {
            return Err(CatalogError::DuplicateTable { name });
        }
        if self.tables.len() == self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.limits.max_tables,
            });
        }

        let fields = statement
            .columns
            .into_iter()
            .map(|column| Field::new(column.name, column.data_type))
            .collect();
        let table =
            Table::with_row_limit(fields, self.limits.max_rows_per_table).map_err(|source| {
                CatalogError::TableConstruction {
                    name: name.clone(),
                    source,
                }
            })?;

        self.tables
            .try_reserve(1)
            .map_err(|_| CatalogError::AllocationFailed)?;
        let previous = self
            .tables
            .insert(key.clone(), CatalogEntry { name, table });
        debug_assert!(
            previous.is_none(),
            "duplicates are checked before insertion"
        );

        Ok(&self
            .tables
            .get(&key)
            .expect("the table was inserted immediately above")
            .table)
    }

    /// Parses and executes one bounded `INSERT INTO ... VALUES` statement.
    ///
    /// The complete batch is parsed and the target is resolved before storage
    /// mutation begins. [`Table::insert_batch`] then validates and commits the
    /// batch atomically, so every returned error leaves all tables unchanged.
    pub fn execute_insert(&mut self, input: &str) -> Result<usize, CatalogError> {
        let statement = parse_insert_with_limits(input, self.limits.insert_parse)?;
        self.insert(statement)
    }

    /// Inserts the rows from an already parsed statement.
    ///
    /// This is the typed execution boundary used by [`Self::execute_insert`].
    pub fn insert(&mut self, statement: InsertStatement) -> Result<usize, CatalogError> {
        let name = statement.name;
        let table = self
            .tables
            .get_mut(&normalize_table_name(&name))
            .map(|entry| &mut entry.table)
            .ok_or_else(|| CatalogError::TableNotFound { name: name.clone() })?;

        table
            .insert_batch(statement.rows)
            .map_err(|source| CatalogError::TableInsertion { name, source })
    }

    /// Returns a table by ASCII case-insensitive name.
    pub fn table(&self, name: &str) -> Result<&Table, CatalogError> {
        self.tables
            .get(&normalize_table_name(name))
            .map(|entry| &entry.table)
            .ok_or_else(|| CatalogError::TableNotFound {
                name: name.to_owned(),
            })
    }

    /// Returns a mutable table by ASCII case-insensitive name.
    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table, CatalogError> {
        self.tables
            .get_mut(&normalize_table_name(name))
            .map(|entry| &mut entry.table)
            .ok_or_else(|| CatalogError::TableNotFound {
                name: name.to_owned(),
            })
    }

    /// Iterates over table names with their original spelling.
    pub fn table_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tables.values().map(|entry| entry.name.as_str())
    }

    /// Returns the active resource limits.
    #[must_use]
    pub const fn limits(&self) -> CatalogLimits {
        self.limits
    }

    /// Returns the number of tables in the catalog.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog owns no tables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

const fn default_insert_parse_limits() -> InsertParseLimits {
    InsertParseLimits::new(
        InsertParseLimits::DEFAULT_MAX_INPUT_BYTES,
        InsertParseLimits::DEFAULT_MAX_ROWS,
        InsertParseLimits::DEFAULT_MAX_VALUES_PER_ROW,
        InsertParseLimits::DEFAULT_MAX_STRING_BYTES,
    )
}

fn normalize_table_name(name: &str) -> String {
    name.to_ascii_lowercase()
}
