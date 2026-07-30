use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};

/// An in-memory collection of named tables.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
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
        Ok(())
    }

    /// Create a table unless the normalized name is already present.
    ///
    /// Returns whether a new table was created.
    pub fn create_table_if_not_exists(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
    ) -> Result<bool> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Ok(false);
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        Ok(true)
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

    /// Return tables in deterministic, case-insensitive name order.
    #[must_use]
    pub fn tables(&self) -> Vec<&Table> {
        let mut tables = self.tables.values().collect::<Vec<_>>();
        tables.sort_by_cached_key(|table| normalize(table.name()));
        tables
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.tables
            .remove(&normalize(name))
            .map(|_| ())
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    /// Drop a table if it exists, returning whether a table was removed.
    pub fn drop_table_if_exists(&mut self, name: &str) -> bool {
        self.tables.remove(&normalize(name)).is_some()
    }

    /// Remove all rows from a table and return the number removed.
    pub fn truncate_table(&mut self, name: &str) -> Result<usize> {
        self.table_mut(name).map(Table::truncate)
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
    fn table_listing_and_lifecycle_are_case_insensitive_and_deterministic() {
        let mut catalog = Catalog::new();
        let schema = || {
            vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }]
        };
        catalog
            .create_table("zebra".to_owned(), schema())
            .expect("create zebra");
        catalog
            .create_table("Alpha".to_owned(), schema())
            .expect("create alpha");

        assert_eq!(
            catalog
                .tables()
                .iter()
                .map(|table| table.name())
                .collect::<Vec<_>>(),
            ["Alpha", "zebra"]
        );
        assert!(
            !catalog
                .create_table_if_not_exists("ALPHA".to_owned(), schema())
                .expect("existing table is a no-op")
        );
        assert!(catalog.drop_table_if_exists("ZEBRA"));
        assert!(!catalog.drop_table_if_exists("zebra"));
        catalog.drop_table("alpha").expect("drop existing table");
        assert!(matches!(
            catalog.drop_table("missing"),
            Err(Error::TableNotFound(name)) if name == "missing"
        ));
    }
}
