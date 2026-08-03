use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::storage::{Schema, Table};

/// A bounded, in-memory collection of named tables.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    table_limit: usize,
}

impl Catalog {
    /// Creates an empty catalog that can contain at most `table_limit` tables.
    ///
    /// The limit does not allocate memory up front.
    pub fn new(table_limit: usize) -> Self {
        Self {
            tables: HashMap::new(),
            table_limit,
        }
    }

    /// Creates an empty named table with a bounded number of rows.
    ///
    /// All catalog-level validation is completed before the catalog is
    /// mutated.
    pub fn create_table(
        &mut self,
        name: impl Into<String>,
        schema: Schema,
        row_limit: usize,
    ) -> Result<(), CatalogError> {
        let name = name.into();

        if name.is_empty() {
            return Err(CatalogError::EmptyName);
        }

        if self.tables.contains_key(&name) {
            return Err(CatalogError::DuplicateTable { name });
        }

        if self.tables.len() >= self.table_limit {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.table_limit,
            });
        }

        self.tables.insert(name, Table::new(schema, row_limit));
        Ok(())
    }

    /// Returns a table by name.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    /// Returns a mutable table by name.
    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name)
    }

    /// Returns the number of tables in the catalog.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog contains no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Returns the configured maximum number of tables.
    pub fn table_limit(&self) -> usize {
        self.table_limit
    }
}

/// An error that prevents a table from being created in a [`Catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// A table name is empty.
    EmptyName,
    /// A table already uses the requested name.
    DuplicateTable { name: String },
    /// Creating the table would exceed the catalog's configured table limit.
    TableLimitExceeded { limit: usize },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("table name cannot be empty"),
            Self::DuplicateTable { name } => write!(formatter, "table already exists: {name}"),
            Self::TableLimitExceeded { limit } => {
                write!(formatter, "catalog table limit of {limit} has been reached")
            }
        }
    }
}

impl Error for CatalogError {}
