//! In-memory table schema catalog.

use std::collections::HashMap;

use crate::{Error, Result, TableSchema};

/// A collection of table schemas indexed by case-insensitive table name.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, TableSchema>,
}

impl Catalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a validated schema without replacing an existing table.
    pub fn register(&mut self, schema: TableSchema) -> Result<()> {
        let key = normalize(schema.name());
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists {
                name: schema.name().to_owned(),
            });
        }
        self.tables.insert(key, schema);
        Ok(())
    }

    /// Find a table using SQL's case-insensitive identifier semantics.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(&normalize(name))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}
