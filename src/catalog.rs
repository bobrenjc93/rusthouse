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

    pub(crate) fn create_table_with_checkpoint(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
        checkpoint: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        checkpoint()?;
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::new_with_checkpoint(name, schema, checkpoint)?;
        checkpoint()?;
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
    fn cancelled_create_does_not_publish_a_partial_table() {
        let mut catalog = Catalog::new();
        let schema = (0..1_000)
            .map(|index| ColumnDef {
                name: format!("column_{index}"),
                data_type: DataType::Int64,
            })
            .collect();
        let mut checkpoints = 0;
        let error = catalog
            .create_table_with_checkpoint("wide".to_owned(), schema, &mut || {
                checkpoints += 1;
                if checkpoints == 100 {
                    Err(Error::ExecutionCancelled)
                } else {
                    Ok(())
                }
            })
            .expect_err("cancellation should abort table construction");

        assert_eq!(error, Error::ExecutionCancelled);
        assert!(matches!(
            catalog.table("wide"),
            Err(Error::TableNotFound(_))
        ));
    }
}
