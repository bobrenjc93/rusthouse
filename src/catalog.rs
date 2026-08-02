//! In-memory ownership of named tables.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
use std::fmt;

use crate::{Schema, Table};

/// A collection of named in-memory tables.
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

    /// Creates an empty table with a validated schema.
    ///
    /// A rejected name or duplicate leaves the catalog unchanged.
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
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    /// Returns a mutable table by its exact, case-sensitive name.
    #[must_use]
    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name)
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    /// Table names cannot be empty.
    EmptyTableName,
    /// A table with the same name already exists.
    DuplicateTable {
        /// The conflicting table name.
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
