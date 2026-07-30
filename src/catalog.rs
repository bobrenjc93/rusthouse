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

    /// Removes a table and reports how many rows it contained.
    pub fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<usize> {
        match self.tables.remove(&normalize(name)) {
            Some(table) => Ok(table.row_count()),
            None if if_exists => Ok(0),
            None => Err(Error::TableNotFound(name.to_owned())),
        }
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
    fn drop_is_case_insensitive_and_if_exists_tolerates_missing_tables() {
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

        assert_eq!(catalog.drop_table("EVENTS", false), Ok(0));
        assert!(matches!(
            catalog.table("events"),
            Err(Error::TableNotFound(_))
        ));
        assert_eq!(catalog.drop_table("events", true), Ok(0));
        assert!(matches!(
            catalog.drop_table("events", false),
            Err(Error::TableNotFound(_))
        ));
    }
}
