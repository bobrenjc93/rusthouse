use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};

pub const DEFAULT_DATABASE: &str = "default";

/// An in-memory collection of database namespaces.
#[derive(Debug)]
pub struct Catalog {
    databases: HashMap<String, DatabaseCatalog>,
}

/// The tables owned by one named database.
#[derive(Debug)]
pub struct DatabaseCatalog {
    name: String,
    tables: HashMap<String, Table>,
}

impl Default for Catalog {
    fn default() -> Self {
        let database = DatabaseCatalog::new(DEFAULT_DATABASE.to_owned());
        let mut databases = HashMap::new();
        databases.insert(normalize(DEFAULT_DATABASE), database);
        Self { databases }
    }
}

impl Catalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_database(&mut self, name: String) -> Result<()> {
        let key = normalize(&name);
        if self.databases.contains_key(&key) {
            return Err(Error::DatabaseAlreadyExists(name));
        }
        self.databases.insert(key, DatabaseCatalog::new(name));
        Ok(())
    }

    pub fn drop_database(&mut self, name: &str) -> Result<()> {
        let key = normalize(name);
        let database = self
            .databases
            .get(&key)
            .ok_or_else(|| Error::DatabaseNotFound(name.to_owned()))?;
        if !database.is_empty() {
            return Err(Error::DatabaseNotEmpty(database.name.clone()));
        }
        self.databases.remove(&key);
        Ok(())
    }

    pub fn database(&self, name: &str) -> Result<&DatabaseCatalog> {
        self.databases
            .get(&normalize(name))
            .ok_or_else(|| Error::DatabaseNotFound(name.to_owned()))
    }

    #[must_use]
    pub fn database_names(&self) -> Vec<&str> {
        let mut databases = self.databases.iter().collect::<Vec<_>>();
        databases.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        databases
            .into_iter()
            .map(|(_, database)| database.name.as_str())
            .collect()
    }

    /// Create a table in the default database.
    pub fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        self.database_mut(DEFAULT_DATABASE)?
            .create_table(name, schema)
    }

    pub fn create_table_in(
        &mut self,
        database: &str,
        name: String,
        schema: Vec<ColumnDef>,
    ) -> Result<()> {
        let qualified = format!("{database}.{name}");
        self.database_mut(database)?
            .create_table(name, schema)
            .map_err(|error| match error {
                Error::TableAlreadyExists(_) => Error::TableAlreadyExists(qualified),
                other => other,
            })
    }

    /// Look up a table in the default database.
    pub fn table(&self, name: &str) -> Result<&Table> {
        self.database(DEFAULT_DATABASE)?.table(name)
    }

    /// Look up a mutable table in the default database.
    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.database_mut(DEFAULT_DATABASE)?.table_mut(name)
    }

    pub fn table_in(&self, database: &str, name: &str) -> Result<&Table> {
        let qualified = format!("{database}.{name}");
        self.database(database)?
            .table(name)
            .map_err(|error| match error {
                Error::TableNotFound(_) => Error::TableNotFound(qualified),
                other => other,
            })
    }

    pub fn table_mut_in(&mut self, database: &str, name: &str) -> Result<&mut Table> {
        let qualified = format!("{database}.{name}");
        self.database_mut(database)?
            .table_mut(name)
            .map_err(|error| match error {
                Error::TableNotFound(_) => Error::TableNotFound(qualified),
                other => other,
            })
    }

    fn database_mut(&mut self, name: &str) -> Result<&mut DatabaseCatalog> {
        self.databases
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::DatabaseNotFound(name.to_owned()))
    }
}

impl DatabaseCatalog {
    fn new(name: String) -> Self {
        Self {
            name,
            tables: HashMap::new(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::new(name, schema)?;
        self.tables.insert(key, table);
        Ok(())
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
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

    fn schema() -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: "id".to_owned(),
            data_type: DataType::Int64,
        }]
    }

    #[test]
    fn table_lookup_is_case_insensitive() {
        let mut catalog = Catalog::new();
        catalog
            .create_table("Events".to_owned(), schema())
            .expect("create table");

        assert_eq!(catalog.table("EVENTS").expect("lookup").name(), "Events");
    }

    #[test]
    fn database_lookup_and_discovery_are_case_insensitive_and_sorted() {
        let mut catalog = Catalog::new();
        catalog
            .create_database("warehouse".to_owned())
            .expect("create warehouse");
        catalog
            .create_database("Analytics".to_owned())
            .expect("create analytics");

        assert_eq!(catalog.database("ANALYTICS").unwrap().name(), "Analytics");
        assert_eq!(
            catalog.database_names(),
            ["Analytics", "default", "warehouse"]
        );
    }

    #[test]
    fn nonempty_database_cannot_be_dropped() {
        let mut catalog = Catalog::new();
        catalog.create_database("logs".to_owned()).unwrap();
        catalog
            .create_table_in("logs", "events".to_owned(), schema())
            .unwrap();

        assert_eq!(
            catalog.drop_database("LOGS"),
            Err(Error::DatabaseNotEmpty("logs".to_owned()))
        );
    }
}
