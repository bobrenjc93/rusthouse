//! Stateful SQL execution and the in-memory table catalog.

use crate::{
    CreateTable, Field, InsertInto, MAX_IDENTIFIER_BYTES, MAX_SCHEMA_FIELDS, MAX_SQL_INPUT_BYTES,
    QueryResult, Schema, SqlError, SqlErrorKind, Statement, Table, parse_database_batch,
};
use std::collections::{HashMap, HashSet};

/// Maximum rows stored by every table created through [`Database::execute`].
///
/// The cap is enforced by the table append APIs and prevents SQL ingestion
/// from growing an individual in-memory table without bound.
pub const DEFAULT_TABLE_ROW_LIMIT: usize = 1_000_000;

/// A stateful in-memory database containing a SQL table catalog.
///
/// Unquoted table identifiers are matched case-insensitively. The catalog can
/// be changed by executing `CREATE TABLE` and positional `INSERT` statements;
/// callers can inspect tables through [`Database::table`] but cannot bypass SQL
/// to replace them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Database {
    tables: HashMap<String, Table>,
}

impl Database {
    /// Creates an empty database.
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes a batch of scalar `SELECT`, `CREATE TABLE`, and `INSERT`
    /// statements.
    ///
    /// Supported definitions have the form
    /// `CREATE TABLE name (field type, ...);`, where `type` is one of `Int64`,
    /// `Float64`, `Bool`, or `String`. Positional inserts have the form
    /// `INSERT INTO name VALUES (...), (...);` and must supply one literal per
    /// schema field in schema order. Created fields are non-nullable. DDL and
    /// inserts produce no query result, so only scalar `SELECT` statements are
    /// returned for CSV rendering.
    ///
    /// The complete batch is parsed and catalog changes are staged before they
    /// are published. Therefore any returned error leaves the catalog
    /// unchanged. Batches larger than [`MAX_SQL_INPUT_BYTES`] UTF-8
    /// bytes are rejected before parsing.
    pub fn execute(&mut self, input: &str) -> Result<Vec<QueryResult>, SqlError> {
        if input.len() > MAX_SQL_INPUT_BYTES {
            return Err(SqlError::at(
                input,
                0,
                SqlErrorKind::InputTooLarge {
                    max_bytes: MAX_SQL_INPUT_BYTES,
                },
            ));
        }

        let statements = parse_database_batch(input)?;
        self.validate_table_names(input, &statements)?;

        let mut results = Vec::new();
        let mut staged_tables = HashMap::new();
        for statement in statements {
            match statement {
                Statement::Select(result) => results.push(result),
                Statement::CreateTable(definition) => {
                    let (name, table) = build_table(input, definition)?;
                    staged_tables.insert(name, table);
                }
                Statement::InsertInto(insert) => {
                    self.stage_insert(input, &mut staged_tables, insert)?;
                }
            }
        }

        for (name, table) in staged_tables {
            self.tables.insert(name, table);
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

    fn stage_insert(
        &self,
        input: &str,
        staged_tables: &mut HashMap<String, Table>,
        mut insert: InsertInto,
    ) -> Result<(), SqlError> {
        let normalized_name = normalize_identifier(&insert.table.value);
        if !staged_tables.contains_key(&normalized_name) {
            let Some(table) = self.tables.get(&normalized_name) else {
                return Err(SqlError::at(
                    input,
                    insert.table.byte_offset,
                    SqlErrorKind::UnknownTable {
                        table: insert.table.value,
                    },
                ));
            };
            staged_tables.insert(normalized_name.clone(), table.clone());
        }

        let table = staged_tables
            .get_mut(&normalized_name)
            .expect("the target table was staged above");
        let append_result = table.append_batch(
            insert
                .rows
                .iter_mut()
                .map(|row| std::mem::take(&mut row.values)),
        );

        if let Err(source) = append_result {
            let byte_offset = insert.rows[source.row_index()].byte_offset;
            return Err(SqlError::at(
                input,
                byte_offset,
                SqlErrorKind::InvalidRow {
                    table: insert.table.value,
                    source,
                },
            ));
        }

        Ok(())
    }
}

fn build_table(input: &str, definition: CreateTable) -> Result<(String, Table), SqlError> {
    let error_offset = definition
        .fields
        .get(MAX_SCHEMA_FIELDS)
        .map(|field| field.name.byte_offset)
        .or_else(|| {
            definition
                .fields
                .iter()
                .find(|field| field.name.value.len() > MAX_IDENTIFIER_BYTES)
                .map(|field| field.name.byte_offset)
        })
        .unwrap_or(definition.name.byte_offset);
    let table_name = definition.name.value.clone();
    let normalized_name = normalize_identifier(&definition.name.value);
    let fields = definition
        .fields
        .into_iter()
        .map(|field| Field::new(field.name.value, field.data_type, false))
        .collect();
    let schema = Schema::new(fields).map_err(|error| {
        SqlError::at(
            input,
            error_offset,
            SqlErrorKind::InvalidSchema {
                table: table_name,
                error,
            },
        )
    })?;

    Ok((normalized_name, Table::new(schema, DEFAULT_TABLE_ROW_LIMIT)))
}

fn normalize_identifier(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}
