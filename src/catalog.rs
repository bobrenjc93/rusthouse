//! In-memory ownership and name resolution for tables.

use std::collections::{HashMap, hash_map::Entry};
use std::error::Error;
use std::fmt;

use crate::storage::{Schema, Table};

/// An in-memory collection of named tables.
///
/// Table names are matched exactly and are case-sensitive. A catalog owns
/// every table created through it, so mutable access is limited to one named
/// table at a time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    tables: HashMap<String, Table>,
}

impl Catalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates and stores an empty table from a validated schema.
    ///
    /// Empty names are rejected. If `name` already exists, its table is left
    /// unchanged and the supplied schema is not installed.
    pub fn create_table(
        &mut self,
        name: impl Into<String>,
        schema: Schema,
    ) -> Result<(), CatalogError> {
        let name = name.into();
        if name.is_empty() {
            return Err(CatalogError::EmptyTableName);
        }

        match self.tables.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(Table::new(schema));
                Ok(())
            }
            Entry::Occupied(entry) => Err(CatalogError::DuplicateTable {
                name: entry.key().clone(),
            }),
        }
    }

    /// Returns a table by its exact, case-sensitive name.
    pub fn table(&self, name: &str) -> Result<&Table, TableNotFoundError> {
        self.tables
            .get(name)
            .ok_or_else(|| TableNotFoundError::new(name))
    }

    /// Returns mutable access to a table by its exact, case-sensitive name.
    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table, TableNotFoundError> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| TableNotFoundError::new(name))
    }

    /// Returns whether a table with this exact name exists.
    #[must_use]
    pub fn contains_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    /// Returns the number of tables in the catalog.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog contains no tables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// An error returned while changing a [`Catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// A table name must not be empty.
    EmptyTableName,
    /// A table with the exact name already exists.
    DuplicateTable {
        /// The name already owned by the catalog.
        name: String,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTableName => formatter.write_str("a table name must not be empty"),
            Self::DuplicateTable { name } => write!(formatter, "table `{name}` already exists"),
        }
    }
}

impl Error for CatalogError {}

/// An error returned when an exact table name is not in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNotFoundError {
    /// The name requested by the caller.
    pub name: String,
}

impl TableNotFoundError {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
        }
    }
}

impl fmt::Display for TableNotFoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "table `{}` was not found", self.name)
    }
}

impl Error for TableNotFoundError {}
