use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::sql::Select;
use crate::storage::{ColumnDef, Table};

/// The kind of object registered under a catalog name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
}

/// A logical view whose query is expanded whenever the view is referenced.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    name: String,
    query: Select,
}

impl View {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn query(&self) -> &Select {
        &self.query
    }
}

/// An in-memory collection of named tables and logical views.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
    views: BTreeMap<String, View>,
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
        if self.views.contains_key(&key) {
            return Err(Error::ViewAlreadyExists(name));
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        Ok(())
    }

    pub(crate) fn create_view(&mut self, name: String, query: Select) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        if self.views.contains_key(&key) {
            return Err(Error::ViewAlreadyExists(name));
        }
        self.views.insert(key, View { name, query });
        Ok(())
    }

    pub(crate) fn drop_view(&mut self, name: &str, if_exists: bool) -> Result<bool> {
        let key = normalize(name);
        if self.tables.contains_key(&key) {
            return Err(Error::InvalidQuery(format!(
                "relation '{name}' is a table, not a view"
            )));
        }
        if self.views.remove(&key).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(Error::ViewNotFound(name.to_owned()))
        }
    }

    pub(crate) fn remove_view(&mut self, name: &str) {
        self.views.remove(&normalize(name));
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

    pub fn view(&self, name: &str) -> Result<&View> {
        self.views
            .get(&normalize(name))
            .ok_or_else(|| Error::ViewNotFound(name.to_owned()))
    }

    #[must_use]
    pub fn relation_kind(&self, name: &str) -> Option<RelationKind> {
        let key = normalize(name);
        if self.tables.contains_key(&key) {
            Some(RelationKind::Table)
        } else if self.views.contains_key(&key) {
            Some(RelationKind::View)
        } else {
            None
        }
    }

    /// Iterate over tables in case-insensitive name order.
    pub fn tables(&self) -> impl ExactSizeIterator<Item = &Table> {
        self.tables.values()
    }

    /// Iterate over views in case-insensitive name order.
    pub fn views(&self) -> impl ExactSizeIterator<Item = &View> {
        self.views.values()
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
