use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};

/// An in-memory collection of named tables.
#[derive(Debug)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    schema_generation: u64,
}

static NEXT_SCHEMA_GENERATION: AtomicU64 = AtomicU64::new(1);

impl Default for Catalog {
    fn default() -> Self {
        Self {
            tables: HashMap::new(),
            schema_generation: next_schema_generation(),
        }
    }
}

impl Catalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        self.schema_generation = next_schema_generation();
        Ok(())
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    pub(crate) fn schema_generation(&self) -> u64 {
        self.schema_generation
    }
}

fn next_schema_generation() -> u64 {
    NEXT_SCHEMA_GENERATION.fetch_add(1, Ordering::Relaxed)
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DataType;

    #[test]
    fn table_lookup_is_case_insensitive() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "Events".to_owned(),
                vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect("create table");

        assert_eq!(catalog.table("EVENTS").expect("lookup").name(), "Events");
    }
}
