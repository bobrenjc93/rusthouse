//! A bounded, in-memory catalog for parsed table definitions.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::parser::CreateTable;
use crate::storage::{ColumnSchema, Schema, SchemaError, Table};

/// Configurable resource limits for an in-memory [`Catalog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    pub max_tables: usize,
}

impl CatalogLimits {
    pub const DEFAULT_MAX_TABLES: usize = 1024;

    pub const fn new(max_tables: usize) -> Self {
        Self { max_tables }
    }
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_TABLES)
    }
}

/// A table definition rejected without changing the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    DuplicateTable { name: String },
    TableLimitExceeded { limit: usize },
    InvalidSchema(SchemaError),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTable { name } => write!(formatter, "table {name:?} already exists"),
            Self::TableLimitExceeded { limit } => {
                write!(
                    formatter,
                    "catalog table limit of {limit} would be exceeded"
                )
            }
            Self::InvalidSchema(error) => write!(formatter, "invalid table schema: {error}"),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSchema(error) => Some(error),
            Self::DuplicateTable { .. } | Self::TableLimitExceeded { .. } => None,
        }
    }
}

/// A bounded collection of named, in-memory tables.
///
/// Table names use the parser's ASCII case-insensitive identifier semantics.
/// The spelling from the `CREATE TABLE` statement is retained by [`table_name`](Self::table_name).
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    tables: HashMap<String, CatalogTable>,
    limits: CatalogLimits,
}

#[derive(Debug, Clone, PartialEq)]
struct CatalogTable {
    name: String,
    table: Table,
}

impl Catalog {
    /// Creates an empty catalog with default resource limits.
    pub fn new() -> Self {
        Self::with_limits(CatalogLimits::default())
    }

    /// Creates an empty catalog with the supplied resource limits.
    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    pub fn limits(&self) -> CatalogLimits {
        self.limits
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Creates an empty table from a parsed statement.
    ///
    /// SQL column definitions are currently non-nullable, so the generated
    /// schema does not allocate NULL bitmaps. Duplicate and lookup comparisons
    /// are ASCII case-insensitive.
    pub fn create_table(&mut self, statement: CreateTable) -> Result<(), CatalogError> {
        let normalized_name = normalize_name(&statement.name);
        if self.tables.contains_key(&normalized_name) {
            return Err(CatalogError::DuplicateTable {
                name: statement.name,
            });
        }
        if self.tables.len() == self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.limits.max_tables,
            });
        }

        let schema = Schema::new(
            statement
                .columns
                .into_iter()
                .map(|column| ColumnSchema::new(column.name, column.column_type, false))
                .collect(),
        )
        .map_err(CatalogError::InvalidSchema)?;

        self.tables.insert(
            normalized_name,
            CatalogTable {
                name: statement.name,
                table: Table::new(schema),
            },
        );
        Ok(())
    }

    /// Looks up a table using an ASCII case-insensitive name.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables
            .get(&normalize_name(name))
            .map(|entry| &entry.table)
    }

    /// Returns the spelling used when a table was created.
    pub fn table_name(&self, name: &str) -> Option<&str> {
        self.tables
            .get(&normalize_name(name))
            .map(|entry| entry.name.as_str())
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}
