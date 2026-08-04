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
        assert_eq!(catalog.table_names(), ["Alpha", "beta", "zebra"]);
    }
}
