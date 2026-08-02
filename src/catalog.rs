//! In-memory table schema catalog.

use std::collections::HashMap;

use crate::{Error, Result, Schema, Table, TableSchema, Value};

#[derive(Debug)]
struct CatalogEntry {
    schema: TableSchema,
    data: Table,
}

/// A collection of table schemas and data indexed by case-insensitive name.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, CatalogEntry>,
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
        let data = Table::new(Schema::from(&schema));
        self.tables.insert(key, CatalogEntry { schema, data });
        Ok(())
    }

    /// Find a table using SQL's case-insensitive identifier semantics.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(&normalize(name)).map(|entry| &entry.schema)
    }

    /// Find a table's typed columnar data using a case-insensitive name.
    #[must_use]
    pub fn table_data(&self, name: &str) -> Option<&Table> {
        self.tables.get(&normalize(name)).map(|entry| &entry.data)
    }

    pub(crate) fn insert_rows(&mut self, name: &str, rows: Vec<Vec<Value>>) -> Result<()> {
        let entry = self
            .tables
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::TableNotFound {
                name: name.to_owned(),
            })?;
        entry.data.insert_rows(rows)?;
        Ok(())
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
