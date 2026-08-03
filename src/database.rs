//! Stateful SQL execution and the in-memory table catalog.

use crate::{
    BatchAppendError, CreateTable, DatabaseEvent, DatabaseEventOutcome, Field,
    MAX_IDENTIFIER_BYTES, MAX_SCHEMA_FIELDS, MAX_SQL_INPUT_BYTES, QueryResult, Schema, SqlError,
    SqlErrorKind, Table, parse_database_batch,
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
    /// unchanged. Batches larger than [`MAX_SQL_INPUT_BYTES`] UTF-8 bytes or
    /// containing more than [`crate::MAX_SQL_STATEMENTS`] statements are
    /// rejected before execution.
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

        let mut results = Vec::new();
        let mut staged_tables: HashMap<String, StagedTable> = HashMap::new();
        let mut table_names: HashSet<String> = self.tables.keys().cloned().collect();
        let mut current_insert = None;

        parse_database_batch(input, |event| {
            let outcome = match event {
                DatabaseEvent::Select(result) => {
                    results.push(result);
                    DatabaseEventOutcome::Continue
                }
                DatabaseEvent::CreateTable(definition) => {
                    let normalized_name = normalize_identifier(&definition.name.value);
                    if !table_names.insert(normalized_name) {
                        return Err(SqlError::at(
                            input,
                            definition.name.byte_offset,
                            SqlErrorKind::DuplicateTable {
                                table: definition.name.value,
                            },
                        ));
                    }
                    let (name, table) = build_table(input, definition)?;
                    staged_tables.insert(name, StagedTable::Created(table));
                    DatabaseEventOutcome::Continue
                }
                DatabaseEvent::InsertStart(table) => {
                    let normalized_name = normalize_identifier(&table.value);
                    if !staged_tables.contains_key(&normalized_name) {
                        let Some(existing) = self.tables.get(&normalized_name) else {
                            return Err(SqlError::at(
                                input,
                                table.byte_offset,
                                SqlErrorKind::UnknownTable { table: table.value },
                            ));
                        };
                        staged_tables.insert(
                            normalized_name.clone(),
                            StagedTable::from_existing(existing),
                        );
                    }
                    current_insert = Some(CurrentInsert {
                        normalized_name: normalized_name.clone(),
                        table_name: table.value,
                        row_index: 0,
                    });
                    DatabaseEventOutcome::InsertSchemaWidth(
                        staged_tables
                            .get(&normalized_name)
                            .expect("the INSERT target was staged")
                            .schema_len(),
                    )
                }
                DatabaseEvent::InsertRow(row) => {
                    let insert = current_insert
                        .as_mut()
                        .expect("INSERT rows follow an INSERT start event");
                    let table = staged_tables
                        .get_mut(&insert.normalized_name)
                        .expect("the INSERT target was staged by its start event");
                    if let Err(source) = table.append_row(row.values) {
                        return Err(SqlError::at(
                            input,
                            row.byte_offset,
                            SqlErrorKind::InvalidRow {
                                table: insert.table_name.clone(),
                                source: source.with_row_index(insert.row_index),
                            },
                        ));
                    }
                    insert.row_index += 1;
                    DatabaseEventOutcome::Continue
                }
                DatabaseEvent::InsertEnd => {
                    current_insert = None;
                    DatabaseEventOutcome::Continue
                }
            };
            Ok(outcome)
        })?;

        for (name, staged) in staged_tables {
            match staged {
                StagedTable::Created(table) => {
                    let previous = self.tables.insert(name, table);
                    debug_assert!(previous.is_none(), "new table names were validated");
                }
                StagedTable::Existing { delta, .. } => self
                    .tables
                    .get_mut(&name)
                    .expect("an existing staged table remains in the catalog")
                    .append_committed(delta),
            }
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
}

enum StagedTable {
    Created(Table),
    Existing { delta: Table, base_row_count: usize },
}

impl StagedTable {
    fn from_existing(table: &Table) -> Self {
        Self::Existing {
            delta: Table::new(table.schema().clone(), table.row_limit()),
            base_row_count: table.row_count(),
        }
    }

    fn append_row(&mut self, values: Vec<crate::Value>) -> Result<(), BatchAppendError> {
        match self {
            Self::Created(table) => table.append_staged_row_after(values, 0),
            Self::Existing {
                delta,
                base_row_count,
            } => delta.append_staged_row_after(values, *base_row_count),
        }
    }

    fn schema_len(&self) -> usize {
        match self {
            Self::Created(table) => table.schema().len(),
            Self::Existing { delta, .. } => delta.schema().len(),
        }
    }
}

struct CurrentInsert {
    normalized_name: String,
    table_name: String,
    row_index: usize,
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
