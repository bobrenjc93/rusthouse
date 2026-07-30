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

    /// Return table names in deterministic, case-insensitive order.
    #[must_use]
    pub fn table_names(&self) -> Vec<&str> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables.into_iter().map(|(_, table)| table.name()).collect()
    }

    /// Drop a table, returning whether it existed.
    pub fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool> {
        if self.tables.remove(&normalize(name)).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(Error::TableNotFound(name.to_owned()))
        }
    }

    /// Truncate a table and return the number of removed rows.
    pub fn truncate_table(&mut self, name: &str) -> Result<usize> {
        Ok(self.table_mut(name)?.truncate())
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
    fn table_names_are_sorted_and_preserve_declared_case() {
        let mut catalog = Catalog::new();
        for name in ["Zulu", "alpha", "Middle"] {
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

        assert_eq!(catalog.table_names(), ["alpha", "Middle", "Zulu"]);
    }

    #[test]
    fn drop_and_truncate_are_case_insensitive() {
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
        catalog
            .table_mut("events")
            .expect("table exists")
            .insert_row(vec![crate::value::Value::Int64(1)])
            .expect("insert row");

        assert_eq!(catalog.truncate_table("EVENTS").expect("truncate"), 1);
        assert_eq!(
            catalog.table("events").expect("table retained").row_count(),
            0
        );
        assert!(catalog.drop_table("eVeNtS", false).expect("drop"));
        assert!(!catalog.drop_table("events", true).expect("optional drop"));
        assert!(matches!(
            catalog.drop_table("events", false),
            Err(Error::TableNotFound(name)) if name == "events"
        ));
    }
}
