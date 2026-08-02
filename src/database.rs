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

    /// Executes a batch of scalar `SELECT`, table-backed `SELECT COUNT(*)`,
    /// `CREATE TABLE`, and `INSERT` statements.
    ///
    /// Supported definitions have the form
    /// `CREATE TABLE name (field type, ...);`, where `type` is one of `Int64`,
    /// `Float64`, `Bool`, or `String`, optionally wrapped in `Nullable(...)`.
    /// Positional inserts have the form `INSERT INTO name VALUES (...), (...);`
    /// and must supply one literal per schema field in schema order. Plain
    /// field types are non-nullable. DDL and inserts produce no query result,
    /// so only `SELECT` statements are returned for CSV rendering.
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
                DatabaseEvent::CountRows(query) => {
                    let normalized_name = normalize_identifier(&query.table.value);
                    let row_count = if let Some(staged) = staged_tables.get(&normalized_name) {
                        staged.row_count()
                    } else if let Some(table) = self.tables.get(&normalized_name) {
                        table.row_count()
                    } else {
                        return Err(SqlError::at(
                            input,
                            query.table.byte_offset,
                            SqlErrorKind::UnknownTable {
                                table: query.table.value,
                            },
                        ));
                    };
                    results.push(QueryResult {
                        header: query.header,
                        value: crate::ScalarValue::Integer(
                            i64::try_from(row_count)
                                .expect("SQL table row limits fit within an Int64"),
                        ),
                    });
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
    Existing {
        delta: Table,
        base_row_count: usize,
        row_limit: usize,
        base_data_size_bytes: usize,
        data_byte_limit: usize,
    },
}

impl StagedTable {
    fn from_existing(table: &Table) -> Self {
        Self::Existing {
            delta: Table::with_data_limit(
                table.schema().clone(),
                table.row_limit(),
                table.data_byte_limit(),
            ),
            base_row_count: table.row_count(),
            row_limit: table.row_limit(),
            base_data_size_bytes: table.data_size_bytes(),
            data_byte_limit: table.data_byte_limit(),
        }
    }

    fn append_row(&mut self, values: Vec<crate::Value>) -> Result<(), BatchAppendError> {
        match self {
            Self::Created(table) => table.append_batch([values]),
            Self::Existing {
                delta,
                base_row_count,
                row_limit,
                base_data_size_bytes,
                data_byte_limit,
            } => delta.append_batch_after(
                [values],
                *base_row_count,
                *row_limit,
                *base_data_size_bytes,
                *data_byte_limit,
            ),
        }
    }

    fn schema_len(&self) -> usize {
        match self {
            Self::Created(table) => table.schema().len(),
            Self::Existing { delta, .. } => delta.schema().len(),
        }
    }

    fn row_count(&self) -> usize {
        match self {
            Self::Created(table) => table.row_count(),
            Self::Existing {
                delta,
                base_row_count,
                ..
            } => base_row_count + delta.row_count(),
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
        .map(|field| Field::new(field.name.value, field.data_type, field.nullable))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataType, Schema, Value};

    fn byte_bounded_database(with_existing_row: bool) -> (Database, usize) {
        let schema = Schema::new(vec![Field::new("value", DataType::Int64, false)]).unwrap();
        let row_size = 1 + std::mem::size_of::<i64>();
        let mut table = Table::with_data_limit(schema, 10, row_size * 2 - 1);
        if with_existing_row {
            table.append_batch([[Value::Int64(1)]]).unwrap();
        }

        let mut database = Database::new();
        database.tables.insert("bounded".into(), table);
        (database, row_size)
    }

    #[test]
    fn staged_insert_includes_committed_data_in_the_table_budget() {
        let (mut database, row_size) = byte_bounded_database(true);
        let before = database.clone();

        let error = database
            .execute("INSERT INTO bounded VALUES (2);")
            .unwrap_err();

        assert_eq!(
            error.kind(),
            &SqlErrorKind::InvalidRow {
                table: "bounded".into(),
                source: BatchAppendError::TableDataLimitExceeded {
                    row_index: 0,
                    attempted: row_size * 2,
                    limit: row_size * 2 - 1,
                },
            }
        );
        assert_eq!(database, before);
    }

    #[test]
    fn staged_multi_row_insert_reports_the_crossing_batch_row_and_rolls_back() {
        let (mut database, row_size) = byte_bounded_database(false);
        let before = database.clone();

        let error = database
            .execute("INSERT INTO bounded VALUES (1), (2);")
            .unwrap_err();

        assert_eq!(
            error.kind(),
            &SqlErrorKind::InvalidRow {
                table: "bounded".into(),
                source: BatchAppendError::TableDataLimitExceeded {
                    row_index: 1,
                    attempted: row_size * 2,
                    limit: row_size * 2 - 1,
                },
            }
        );
        assert_eq!(database, before);
    }
}
