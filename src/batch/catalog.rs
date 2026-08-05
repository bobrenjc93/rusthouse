use std::collections::HashMap;

use crate::batch::error::{Error, Result};
use crate::batch::storage::{ColumnDef, Table};

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

    /// Removes one table using the catalog's case-insensitive name resolution.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.tables
            .remove(&normalize(name))
            .map(|_| ())
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
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

    /// Returns the number of registered tables without allocating.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Returns the combined byte length of all display names without allocating.
    #[must_use]
    pub fn table_name_bytes(&self) -> usize {
        self.tables
            .values()
            .map(|table| table.name().len())
            .fold(0_usize, usize::saturating_add)
    }

    /// Returns display names in deterministic, case-insensitive order.
    #[must_use]
    pub fn table_names(&self) -> Vec<&str> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables.into_iter().map(|(_, table)| table.name()).collect()
    }
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::value::DataType;

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
    fn table_names_are_sorted_without_changing_display_case() {
        let mut catalog = Catalog::new();
        assert_eq!(catalog.table_count(), 0);
        assert_eq!(catalog.table_name_bytes(), 0);

        for name in ["zebra", "Alpha", "beta"] {
            catalog
                .create_table(
                    name.to_owned(),
                    vec![ColumnDef {
                        name: "id".to_owned(),
                        data_type: DataType::Int64,
                    }],
                )
                .expect("create table");
        }

        assert_eq!(catalog.table_count(), 3);
        assert_eq!(catalog.table_name_bytes(), 14);
        assert_eq!(catalog.table_names(), ["Alpha", "beta", "zebra"]);
    }

    #[test]
    fn dropping_is_case_insensitive_and_a_missing_table_preserves_the_catalog() {
        let mut catalog = Catalog::new();
        for name in ["Events", "readings"] {
            catalog
                .create_table(
                    name.to_owned(),
                    vec![ColumnDef {
                        name: "id".to_owned(),
                        data_type: DataType::Int64,
                    }],
                )
                .expect("create table");
        }

        catalog.drop_table("EVENTS").expect("case-insensitive drop");
        assert_eq!(catalog.table_names(), ["readings"]);

        assert_eq!(
            catalog.drop_table("missing"),
            Err(Error::TableNotFound("missing".to_owned()))
        );
        assert_eq!(catalog.table_names(), ["readings"]);
        assert!(catalog.table("readings").is_ok());
    }
}
