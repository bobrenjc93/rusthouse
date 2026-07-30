use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};

/// An in-memory collection of named tables.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: HashMap<String, Arc<Table>>,
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
        self.tables.insert(key, Arc::new(table));
        Ok(())
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .map(Arc::as_ref)
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(&normalize(name))
            .map(Arc::make_mut)
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }
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

    #[test]
    fn cloned_catalog_shares_tables_until_they_are_mutated() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "events".to_owned(),
                vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect("create table");

        let mut snapshot = catalog.clone();
        assert!(Arc::ptr_eq(
            catalog.tables.get("events").expect("base table"),
            snapshot.tables.get("events").expect("snapshot table")
        ));

        snapshot.table_mut("events").expect("mutable table");
        assert!(!Arc::ptr_eq(
            catalog.tables.get("events").expect("base table"),
            snapshot.tables.get("events").expect("snapshot table")
        ));
    }
}
