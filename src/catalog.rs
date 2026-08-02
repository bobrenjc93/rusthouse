//! Case-insensitive, named access to in-memory tables.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::sql::CreateTableStatement;
use crate::{ColumnSchema, Schema, Table, TableError, TableLimits, Value};

/// An in-memory collection of named [`Table`]s.
///
/// Table names use ASCII case-insensitive matching, consistent with the SQL
/// parser's identifier and keyword rules. Each table created in a catalog uses
/// the same resource limits.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    table_limits: TableLimits,
}

impl Catalog {
    /// Creates an empty catalog whose future tables use `table_limits`.
    pub fn new(table_limits: TableLimits) -> Self {
        Self {
            tables: HashMap::new(),
            table_limits,
        }
    }

    /// Returns the limits applied when a table is created.
    pub fn table_limits(&self) -> TableLimits {
        self.table_limits
    }

    /// Returns the number of tables in the catalog.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog contains no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Creates a named table from a parsed `CREATE TABLE` statement.
    ///
    /// Schema validation and table construction finish before the catalog is
    /// changed. A duplicate name or invalid schema therefore leaves every
    /// existing table unchanged.
    pub fn create_table(&mut self, statement: CreateTableStatement) -> Result<(), CatalogError> {
        let CreateTableStatement {
            table_name,
            columns,
        } = statement;
        let key = table_key(&table_name);

        if self.tables.contains_key(&key) {
            return Err(CatalogError::DuplicateTable { name: table_name });
        }

        let schema = Schema::new(
            columns
                .into_iter()
                .map(|column| ColumnSchema::new(column.name, column.data_type))
                .collect(),
        )?;
        let table = Table::new(schema, self.table_limits)?;

        self.tables.insert(key, table);
        Ok(())
    }

    /// Looks up a table using an ASCII case-insensitive name.
    pub fn table(&self, name: &str) -> Result<&Table, CatalogError> {
        self.tables
            .get(&table_key(name))
            .ok_or_else(|| CatalogError::TableNotFound {
                name: name.to_owned(),
            })
    }

    /// Atomically inserts a batch into a named table.
    ///
    /// The table layer validates the full batch before appending any value, so
    /// an invalid batch leaves the target table unchanged.
    pub fn insert_batch(
        &mut self,
        table_name: &str,
        rows: Vec<Vec<Value>>,
    ) -> Result<(), CatalogError> {
        let table = self.tables.get_mut(&table_key(table_name)).ok_or_else(|| {
            CatalogError::TableNotFound {
                name: table_name.to_owned(),
            }
        })?;
        table.insert_batch(rows)?;
        Ok(())
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new(TableLimits::default())
    }
}

fn table_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// A named-table lookup, creation, or mutation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// A table with the same ASCII case-insensitive name already exists.
    DuplicateTable { name: String },
    /// No table matches the requested ASCII case-insensitive name.
    TableNotFound { name: String },
    /// Schema construction or table validation failed.
    Table(TableError),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTable { name } => write!(formatter, "table already exists: {name}"),
            Self::TableNotFound { name } => write!(formatter, "table not found: {name}"),
            Self::Table(error) => error.fmt(formatter),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::DuplicateTable { .. } | Self::TableNotFound { .. } => None,
        }
    }
}

impl From<TableError> for CatalogError {
    fn from(error: TableError) -> Self {
        Self::Table(error)
    }
}
