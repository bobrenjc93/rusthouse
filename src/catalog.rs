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

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(&normalize(name))
    }

    pub(crate) fn insert_table(&mut self, table: Table) -> Result<()> {
        let key = normalize(table.name());
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(table.name().to_owned()));
        }
        self.tables.insert(key, table);
        Ok(())
    }

    /// Replaces a staged set only after verifying that every relation exists.
    pub(crate) fn replace_tables(&mut self, tables: Vec<Table>) -> Result<()> {
        for table in &tables {
            if !self.tables.contains_key(&normalize(table.name())) {
                return Err(Error::TableNotFound(table.name().to_owned()));
            }
        }
        for table in tables {
            self.tables.insert(normalize(table.name()), table);
        }
        Ok(())
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
}
