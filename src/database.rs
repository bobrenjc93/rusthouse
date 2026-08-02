//! Stateful SQL execution and the in-memory table catalog.

use crate::{
    CreateTable, Field, QueryResult, Schema, SqlError, SqlErrorKind, Statement, Table,
    parse_database_batch,
};
use std::collections::{HashMap, HashSet};

/// Maximum rows stored by every table created through [`Database::execute`].
///
/// The cap is enforced by [`Table::append_row`] and prevents a future SQL
/// ingestion path from growing an individual in-memory table without bound.
pub const DEFAULT_TABLE_ROW_LIMIT: usize = 1_000_000;

/// A stateful in-memory database containing a read-only SQL table catalog.
///
/// Unquoted table identifiers are matched case-insensitively. The catalog can
/// currently be changed only by executing `CREATE TABLE`; callers can inspect
/// tables through [`Database::table`] but cannot bypass SQL to replace them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Database {
    tables: HashMap<String, Table>,
}

impl Database {
    /// Creates an empty database.
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes a batch of scalar `SELECT` and `CREATE TABLE` statements.
    ///
    /// Supported definitions have the form
    /// `CREATE TABLE name (field type, ...);`, where `type` is one of `Int64`,
    /// `Float64`, `Bool`, or `String`. Created fields are non-nullable. DDL
    /// produces no query result, so only scalar `SELECT` statements are
    /// returned for CSV rendering.
    ///
    /// The complete batch is parsed and every catalog change is validated
    /// before any table is created. Therefore any returned error leaves the
    /// catalog unchanged.
    pub fn execute(&mut self, input: &str) -> Result<Vec<QueryResult>, SqlError> {
        let statements = parse_database_batch(input)?;
        self.validate_table_names(input, &statements)?;

        let mut results = Vec::new();
        let mut new_tables = Vec::new();
        for statement in statements {
            match statement {
                Statement::Select(result) => results.push(result),
                Statement::CreateTable(definition) => {
                    new_tables.push(build_table(definition));
                }
            }
        }

        for (name, table) in new_tables {
            let previous = self.tables.insert(name, table);
            debug_assert!(previous.is_none(), "table names were validated as unique");
        }

        Ok(results)
    }

    /// Returns a table by its unquoted, case-insensitive SQL identifier.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&normalize_identifier(name))
    }

    /// Returns the number of tables in the catalog.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog contains no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    fn validate_table_names(&self, input: &str, statements: &[Statement]) -> Result<(), SqlError> {
        let mut names: HashSet<String> = self.tables.keys().cloned().collect();

        for statement in statements {
            let Statement::CreateTable(definition) = statement else {
                continue;
            };
            if !names.insert(normalize_identifier(&definition.name.value)) {
                return Err(SqlError::at(
                    input,
                    definition.name.byte_offset,
                    SqlErrorKind::DuplicateTable {
                        table: definition.name.value.clone(),
                    },
                ));
            }
        }

        Ok(())
    }
}

fn build_table(definition: CreateTable) -> (String, Table) {
    let normalized_name = normalize_identifier(&definition.name.value);
    let fields = definition
        .fields
        .into_iter()
        .map(|field| Field::new(field.name.value, field.data_type, false))
        .collect();
    let schema = Schema::new(fields)
        .expect("CREATE TABLE field names were checked for duplicates during parsing");

    (normalized_name, Table::new(schema, DEFAULT_TABLE_ROW_LIMIT))
}

fn normalize_identifier(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}
