//! Ownership and lookup for one materialized table.

use std::error::Error;
use std::fmt;

use crate::create::TableEntry;

/// A failure while registering a materialized table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogError {
    /// The catalog already owns its single table.
    Occupied,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied => formatter.write_str("catalog already contains a table"),
        }
    }
}

impl Error for CatalogError {}

/// An in-memory catalog that owns at most one materialized table.
///
/// Lookup compares table names exactly, including ASCII case. Registration
/// rejects every second entry without replacing the entry already owned.
#[derive(Debug, Default)]
pub struct Catalog {
    entry: Option<TableEntry>,
}

impl Catalog {
    /// Creates an empty catalog.
    pub const fn new() -> Self {
        Self { entry: None }
    }

    /// Returns whether the catalog owns no table.
    pub const fn is_empty(&self) -> bool {
        self.entry.is_none()
    }

    /// Returns the number of registered tables.
    pub const fn len(&self) -> usize {
        match self.entry {
            Some(_) => 1,
            None => 0,
        }
    }

    /// Registers a completed table entry.
    ///
    /// If the slot is occupied, the existing entry is left unchanged.
    pub fn register(&mut self, entry: TableEntry) -> Result<(), CatalogError> {
        if self.entry.is_some() {
            return Err(CatalogError::Occupied);
        }

        self.entry = Some(entry);
        Ok(())
    }

    /// Returns the registered entry when its name exactly matches `name`.
    pub fn get(&self, name: &str) -> Option<&TableEntry> {
        self.entry
            .as_ref()
            .filter(|entry| entry.table_name().as_str() == name)
    }

    /// Returns the registered entry mutably when its name exactly matches `name`.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut TableEntry> {
        self.entry
            .as_mut()
            .filter(|entry| entry.table_name().as_str() == name)
    }
}
