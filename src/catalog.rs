//! Ownership and case-insensitive lookup of in-memory tables.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};

/// An in-memory collection that owns named [`Table`] values.
///
/// Names are unique and looked up case-insensitively using ASCII case
/// folding. The catalog has no persistence: its tables and rows live until
/// the catalog, or the containing database, is dropped.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
}

impl Catalog {
    /// Creates an empty catalog.
    ///
    /// This operation is infallible and allocates no table storage.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates and owns an empty table with `name` and `schema`.
    ///
    /// Name uniqueness is case-insensitive. Schema field order becomes
    /// physical column order and is preserved by later accessors.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableAlreadyExists`] if the normalized name is
    /// present, or propagates schema validation errors from [`Table::new`].
    /// The catalog is unchanged on every error.
    pub fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        Ok(())
    }

    /// Borrows a table by case-insensitive name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] without changing the catalog when no
    /// table matches. The returned borrow remains valid only while `self` is
    /// immutably borrowed.
    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    /// Mutably borrows a table by case-insensitive name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] without changing the catalog when no
    /// table matches. The exclusive borrow prevents other catalog access and
    /// remains valid only for the borrow of `self`.
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
}
