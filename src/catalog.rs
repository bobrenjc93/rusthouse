//! In-memory table and logical-view catalog.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::sql::Select;
use crate::storage::{ColumnDef, Table};

/// A named logical view whose query is evaluated when referenced.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    name: String,
    query: Select,
}

impl View {
    /// Returns the view name with its original ASCII case.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the parsed query backing this view.
    #[must_use]
    pub fn query(&self) -> &Select {
        &self.query
    }
}

/// An in-memory collection of named tables and logical views.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    views: HashMap<String, View>,
}

impl Catalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a table with a case-insensitively unique name.
    ///
    /// Returns an error if the table already exists or its schema is invalid.
    pub fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) || self.views.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        Ok(())
    }

    /// Returns a table by case-insensitive name.
    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    /// Returns a mutable table by case-insensitive name.
    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    /// Creates a logical view with a case-insensitively unique relation name.
    ///
    /// Query dependency validation is performed by the database before this
    /// catalog operation is called.
    pub fn create_view(&mut self, name: String, query: Select) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) || self.views.contains_key(&key) {
            return Err(Error::ViewAlreadyExists(name));
        }
        self.views.insert(key, View { name, query });
        Ok(())
    }

    /// Removes and returns a view by case-insensitive name.
    pub fn drop_view(&mut self, name: &str) -> Result<View> {
        self.views
            .remove(&normalize(name))
            .ok_or_else(|| Error::ViewNotFound(name.to_owned()))
    }

    /// Returns a view by case-insensitive name.
    pub fn view(&self, name: &str) -> Result<&View> {
        self.views
            .get(&normalize(name))
            .ok_or_else(|| Error::ViewNotFound(name.to_owned()))
    }

    pub(crate) fn table_if_exists(&self, name: &str) -> Option<&Table> {
        self.tables.get(&normalize(name))
    }

    pub(crate) fn view_if_exists(&self, name: &str) -> Option<&View> {
        self.views.get(&normalize(name))
    }

    pub(crate) fn contains_relation(&self, name: &str) -> bool {
        let key = normalize(name);
        self.tables.contains_key(&key) || self.views.contains_key(&key)
    }

    pub(crate) fn is_view(&self, name: &str) -> bool {
        self.views.contains_key(&normalize(name))
    }
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{Select, SelectItem};
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
    fn table_and_view_names_share_one_namespace() {
        let mut catalog = Catalog::new();
        catalog
            .create_view(
                "Recent".to_owned(),
                Select {
                    items: vec![SelectItem::Wildcard],
                    table: "events".to_owned(),
                    predicate: None,
                    group_by: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                },
            )
            .expect("create view");

        assert_eq!(catalog.view("RECENT").expect("lookup").name(), "Recent");
        let error = catalog
            .create_table(
                "recent".to_owned(),
                vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect_err("relation name is occupied");
        assert!(matches!(error, Error::TableAlreadyExists(_)));
    }
}
