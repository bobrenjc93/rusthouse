use std::collections::HashMap;

use crate::sql::{Projection, Statement};
use crate::{Catalog, ColumnSchema, Error, Result, Schema, Table, TableSchema, Value, sql};

/// Default maximum size of one SQL input, in UTF-8 bytes.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Default maximum number of columns in one table.
pub const DEFAULT_MAX_COLUMNS_PER_TABLE: usize = 1024;

/// Resource limits applied before a statement changes the catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseConfig {
    pub max_input_bytes: usize,
    pub max_columns_per_table: usize,
}

impl DatabaseConfig {
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_columns_per_table: usize) -> Self {
        Self {
            max_input_bytes,
            max_columns_per_table,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_COLUMNS_PER_TABLE)
    }
}

/// The typed rows and projected schema returned by a `SELECT` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    columns: Vec<ColumnSchema>,
    rows: Vec<Vec<Value>>,
}

impl QueryResult {
    #[must_use]
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// The outcome of one executed SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionResult {
    /// A table and its empty typed storage were created.
    CreatedTable,
    /// The contained number of rows were inserted.
    InsertedRows(usize),
    /// A projection returned typed rows.
    Query(QueryResult),
}

impl ExecutionResult {
    /// Return the query result when this was a `SELECT` statement.
    #[must_use]
    pub fn query(&self) -> Option<&QueryResult> {
        match self {
            Self::Query(result) => Some(result),
            Self::CreatedTable | Self::InsertedRows(_) => None,
        }
    }
}

/// A reusable in-memory SQL database.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    tables: HashMap<String, Table>,
    config: DatabaseConfig,
}

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(DatabaseConfig::default())
    }

    #[must_use]
    pub fn with_config(config: DatabaseConfig) -> Self {
        Self {
            catalog: Catalog::new(),
            tables: HashMap::new(),
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> DatabaseConfig {
        self.config
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Parse and execute exactly one supported SQL statement.
    pub fn execute(&mut self, input: &str) -> Result<ExecutionResult> {
        self.validate_input_size(input)?;
        let statement = sql::parse_one(input, self.config.max_columns_per_table)?;
        self.execute_statement(statement)
    }

    /// Parse and execute a semicolon-separated SQL batch in source order.
    ///
    /// The complete batch is parsed before the first statement is executed.
    /// Execution stops at the first typed execution failure.
    pub fn execute_batch(&mut self, input: &str) -> Result<Vec<ExecutionResult>> {
        self.validate_input_size(input)?;
        let statements = sql::parse_batch(input, self.config.max_columns_per_table)?;
        statements
            .into_iter()
            .map(|statement| self.execute_statement(statement))
            .collect()
    }

    fn validate_input_size(&self, input: &str) -> Result<()> {
        let input_bytes = input.len();
        if input_bytes > self.config.max_input_bytes {
            return Err(Error::InputTooLarge {
                actual: input_bytes,
                maximum: self.config.max_input_bytes,
            });
        }
        Ok(())
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<ExecutionResult> {
        match statement {
            Statement::CreateTable(statement) => {
                let (name, columns) = statement.into_parts();
                let schema = TableSchema::new(name.clone(), columns)?;
                let table = Table::new(Schema::from(&schema));
                self.catalog.register(schema)?;
                let previous = self.tables.insert(normalize(&name), table);
                debug_assert!(previous.is_none());
                Ok(ExecutionResult::CreatedTable)
            }
            Statement::Insert(statement) => {
                let row_count = statement.rows.len();
                let table = self
                    .tables
                    .get_mut(&normalize(&statement.table))
                    .ok_or_else(|| Error::TableNotFound {
                        name: statement.table.clone(),
                    })?;
                table.insert_rows(statement.rows)?;
                Ok(ExecutionResult::InsertedRows(row_count))
            }
            Statement::Select(statement) => {
                let table = self
                    .tables
                    .get(&normalize(&statement.table))
                    .ok_or_else(|| Error::TableNotFound {
                        name: statement.table.clone(),
                    })?;

                let column_indices = match statement.projection {
                    Projection::All => (0..table.schema().len()).collect::<Vec<_>>(),
                    Projection::Columns(columns) => columns
                        .into_iter()
                        .map(|column| {
                            table.schema().column_index(&column).ok_or_else(|| {
                                Error::ColumnNotFound {
                                    table: statement.table.clone(),
                                    column,
                                }
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                };

                let columns = column_indices
                    .iter()
                    .map(|&index| {
                        table
                            .schema()
                            .column(index)
                            .expect("projection indices come from this schema")
                            .clone()
                    })
                    .collect();
                let rows = (0..table.row_count())
                    .map(|row| {
                        column_indices
                            .iter()
                            .map(|&column| {
                                table
                                    .column(column)
                                    .and_then(|values| values.value(row))
                                    .expect("table columns have equal lengths")
                            })
                            .collect()
                    })
                    .collect();

                Ok(ExecutionResult::Query(QueryResult { columns, rows }))
            }
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}
