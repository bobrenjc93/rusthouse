use std::cmp::Ordering;
use std::collections::HashMap;
use std::iter::FusedIterator;
use std::mem;
use std::vec;

use crate::sql::{Projection, SortDirection, Statement};
use crate::{Catalog, ColumnSchema, Error, Result, Schema, Table, TableSchema, Value, sql};

/// Default maximum size of one SQL input, in UTF-8 bytes.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Default maximum number of columns in one table.
pub const DEFAULT_MAX_COLUMNS_PER_TABLE: usize = 1024;

/// Default maximum number of materialized cells in one query result.
pub const DEFAULT_MAX_RESULT_CELLS: usize = 1024 * 1024;

/// Default maximum number of query-result cells retained by one batch call.
pub const DEFAULT_MAX_BATCH_RESULT_CELLS: usize = 1024 * 1024;

/// Default maximum estimated memory used by one materialized query result.
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum estimated memory retained by one collecting batch call.
pub const DEFAULT_MAX_BATCH_RESULT_BYTES: usize = 64 * 1024 * 1024;

/// Resource limits applied while parsing and executing SQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseConfig {
    pub max_input_bytes: usize,
    pub max_columns_per_table: usize,
    pub max_result_cells: usize,
    pub max_batch_result_cells: usize,
    pub max_result_bytes: usize,
    pub max_batch_result_bytes: usize,
}

impl DatabaseConfig {
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_columns_per_table: usize) -> Self {
        Self {
            max_input_bytes,
            max_columns_per_table,
            max_result_cells: DEFAULT_MAX_RESULT_CELLS,
            max_batch_result_cells: DEFAULT_MAX_BATCH_RESULT_CELLS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_batch_result_bytes: DEFAULT_MAX_BATCH_RESULT_BYTES,
        }
    }

    /// Override per-query and cumulative collecting-batch result limits.
    #[must_use]
    pub const fn with_result_limits(
        mut self,
        max_result_cells: usize,
        max_batch_result_cells: usize,
    ) -> Self {
        self.max_result_cells = max_result_cells;
        self.max_batch_result_cells = max_batch_result_cells;
        self
    }

    /// Override per-query and cumulative collecting-batch byte limits.
    #[must_use]
    pub const fn with_result_byte_limits(
        mut self,
        max_result_bytes: usize,
        max_batch_result_bytes: usize,
    ) -> Self {
        self.max_result_bytes = max_result_bytes;
        self.max_batch_result_bytes = max_batch_result_bytes;
        self
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
    materialized_bytes: usize,
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

    /// Return the number of typed cells in this result.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.rows.len().saturating_mul(self.columns.len())
    }

    /// Return the estimated memory charged while materializing this result.
    #[must_use]
    pub fn materialized_bytes(&self) -> usize {
        self.materialized_bytes
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

/// A streaming iterator over the results of a parsed SQL batch.
///
/// Statements execute only as the iterator advances. After the first
/// execution error, the iterator is exhausted.
#[derive(Debug)]
pub struct BatchResults<'a> {
    database: &'a mut Database,
    statements: vec::IntoIter<Statement>,
    failed: bool,
}

impl Iterator for BatchResults<'_> {
    type Item = Result<ExecutionResult>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let statement = self.statements.next()?;
        let result = self.database.execute_statement(statement);
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.failed {
            (0, Some(0))
        } else {
            (0, self.statements.size_hint().1)
        }
    }
}

impl FusedIterator for BatchResults<'_> {}

#[derive(Clone, Copy, Debug)]
struct BatchBudget {
    retained_cells: usize,
    retained_bytes: usize,
    maximum_cells: usize,
    maximum_bytes: usize,
}

impl BatchBudget {
    fn check_cells(self, additional: usize) -> Result<()> {
        let actual = self.retained_cells.saturating_add(additional);
        if actual > self.maximum_cells {
            return Err(Error::BatchResultTooLarge {
                actual,
                maximum: self.maximum_cells,
            });
        }
        Ok(())
    }

    fn check_bytes(self, additional: usize) -> Result<()> {
        let actual = self.retained_bytes.saturating_add(additional);
        if actual > self.maximum_bytes {
            return Err(Error::BatchResultBytesTooLarge {
                actual,
                maximum: self.maximum_bytes,
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
enum RowSelection {
    Prefix(usize),
    Indices(Vec<usize>),
}

impl RowSelection {
    fn len(&self) -> usize {
        match self {
            Self::Prefix(length) => *length,
            Self::Indices(indices) => indices.len(),
        }
    }

    fn row_at(&self, position: usize) -> usize {
        match self {
            Self::Prefix(_) => position,
            Self::Indices(indices) => indices[position],
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len()).map(|position| self.row_at(position))
    }
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
    /// Execution stops at the first typed execution failure, including when
    /// retained query cells or bytes exceed their configured batch limits.
    /// Each query is checked against the remaining budgets before its rows
    /// are materialized.
    pub fn execute_batch(&mut self, input: &str) -> Result<Vec<ExecutionResult>> {
        self.validate_input_size(input)?;
        let statements = sql::parse_batch(input, self.config.max_columns_per_table)?;
        let mut retained_cells = 0_usize;
        let mut retained_bytes = 0_usize;
        let mut results = Vec::new();
        for statement in statements {
            let budget = BatchBudget {
                retained_cells,
                retained_bytes,
                maximum_cells: self.config.max_batch_result_cells,
                maximum_bytes: self.config.max_batch_result_bytes,
            };
            let result = self.execute_statement_with_budget(statement, Some(budget))?;
            if let Some(query) = result.query() {
                retained_cells = retained_cells.saturating_add(query.cell_count());
                retained_bytes = retained_bytes.saturating_add(query.materialized_bytes());
            }
            results.push(result);
        }
        Ok(results)
    }

    /// Parse a batch and stream each execution result without retaining prior
    /// query rows.
    pub fn execute_batch_iter(&mut self, input: &str) -> Result<BatchResults<'_>> {
        self.validate_input_size(input)?;
        let statements = sql::parse_batch(input, self.config.max_columns_per_table)?;
        Ok(BatchResults {
            database: self,
            statements: statements.into_iter(),
            failed: false,
        })
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
        self.execute_statement_with_budget(statement, None)
    }

    fn execute_statement_with_budget(
        &mut self,
        statement: Statement,
        batch_budget: Option<BatchBudget>,
    ) -> Result<ExecutionResult> {
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
                let table_name = statement.table;
                let table = self.tables.get(&normalize(&table_name)).ok_or_else(|| {
                    Error::TableNotFound {
                        name: table_name.clone(),
                    }
                })?;

                let projection_width = match &statement.projection {
                    Projection::All => table.schema().len(),
                    Projection::Columns(columns) => columns.len(),
                };
                if projection_width > self.config.max_columns_per_table {
                    return Err(Error::TooManyProjectedColumns {
                        actual: projection_width,
                        maximum: self.config.max_columns_per_table,
                    });
                }
                let column_indices = match statement.projection {
                    Projection::All => (0..table.schema().len()).collect::<Vec<_>>(),
                    Projection::Columns(columns) => columns
                        .into_iter()
                        .map(|column| {
                            table.schema().column_index(&column).ok_or_else(|| {
                                Error::ColumnNotFound {
                                    table: table_name.clone(),
                                    column,
                                }
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                };

                let order_by = statement
                    .order_by
                    .into_iter()
                    .map(|term| {
                        table
                            .schema()
                            .column_index(&term.column)
                            .map(|index| (index, term.direction))
                            .ok_or_else(|| Error::ColumnNotFound {
                                table: table_name.clone(),
                                column: term.column,
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;

                let selected_row_count = statement
                    .limit
                    .unwrap_or(table.row_count())
                    .min(table.row_count());
                let result_cells = selected_row_count.saturating_mul(projection_width);
                if result_cells > self.config.max_result_cells {
                    return Err(Error::ResultTooLarge {
                        actual: result_cells,
                        maximum: self.config.max_result_cells,
                    });
                }
                if let Some(budget) = batch_budget {
                    budget.check_cells(result_cells)?;
                }

                let prefix_selection = if selected_row_count == 0 || order_by.is_empty() {
                    Some(RowSelection::Prefix(selected_row_count))
                } else {
                    None
                };
                if prefix_selection.is_none() {
                    let fixed_materialized_bytes =
                        estimate_fixed_result_bytes(table, &column_indices, selected_row_count);
                    if fixed_materialized_bytes > self.config.max_result_bytes {
                        return Err(Error::ResultBytesTooLarge {
                            actual: fixed_materialized_bytes,
                            maximum: self.config.max_result_bytes,
                        });
                    }
                    if let Some(budget) = batch_budget {
                        budget.check_bytes(fixed_materialized_bytes)?;
                    }
                }

                let selected_rows = if let Some(prefix) = prefix_selection {
                    prefix
                } else if selected_row_count == table.row_count() {
                    let mut indices = (0..table.row_count()).collect::<Vec<_>>();
                    indices.sort_by(|&left, &right| compare_rows(table, &order_by, left, right));
                    RowSelection::Indices(indices)
                } else {
                    RowSelection::Indices(select_top_rows(table, &order_by, selected_row_count))
                };

                let materialized_bytes =
                    estimate_result_bytes(table, &column_indices, &selected_rows);
                if materialized_bytes > self.config.max_result_bytes {
                    return Err(Error::ResultBytesTooLarge {
                        actual: materialized_bytes,
                        maximum: self.config.max_result_bytes,
                    });
                }
                if let Some(budget) = batch_budget {
                    budget.check_bytes(materialized_bytes)?;
                }

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
                let rows = selected_rows
                    .iter()
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

                Ok(ExecutionResult::Query(QueryResult {
                    columns,
                    rows,
                    materialized_bytes,
                }))
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

fn compare_rows(
    table: &Table,
    order_by: &[(usize, SortDirection)],
    left: usize,
    right: usize,
) -> Ordering {
    order_by
        .iter()
        .map(|&(column_index, direction)| {
            let ordering = table
                .column(column_index)
                .expect("ORDER BY indices come from this schema")
                .compare_rows(left, right);
            match direction {
                SortDirection::Ascending => ordering,
                SortDirection::Descending => ordering.reverse(),
            }
        })
        .find(|&ordering| ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.cmp(&right))
}

fn select_top_rows(table: &Table, order_by: &[(usize, SortDirection)], limit: usize) -> Vec<usize> {
    debug_assert!(limit > 0);
    debug_assert!(limit < table.row_count());

    let mut heap = Vec::with_capacity(limit);
    for row in 0..table.row_count() {
        if heap.len() < limit {
            heap.push(row);
            let mut child = heap.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if compare_rows(table, order_by, heap[parent], heap[child]) != Ordering::Less {
                    break;
                }
                heap.swap(parent, child);
                child = parent;
            }
        } else if compare_rows(table, order_by, row, heap[0]) == Ordering::Less {
            heap[0] = row;
            let mut parent = 0;
            loop {
                let left = parent * 2 + 1;
                if left >= heap.len() {
                    break;
                }
                let right = left + 1;
                let worse_child = if right < heap.len()
                    && compare_rows(table, order_by, heap[left], heap[right]) == Ordering::Less
                {
                    right
                } else {
                    left
                };
                if compare_rows(table, order_by, heap[parent], heap[worse_child]) != Ordering::Less
                {
                    break;
                }
                heap.swap(parent, worse_child);
                parent = worse_child;
            }
        }
    }
    heap.sort_by(|&left, &right| compare_rows(table, order_by, left, right));
    heap
}

fn estimate_fixed_result_bytes(table: &Table, column_indices: &[usize], row_count: usize) -> usize {
    let cell_count = row_count.saturating_mul(column_indices.len());
    let mut bytes = mem::size_of::<QueryResult>()
        .saturating_add(cell_count.saturating_mul(mem::size_of::<Value>()))
        .saturating_add(row_count.saturating_mul(mem::size_of::<Vec<Value>>()))
        .saturating_add(
            column_indices
                .len()
                .saturating_mul(mem::size_of::<ColumnSchema>()),
        );

    for &index in column_indices {
        let definition = table
            .schema()
            .column(index)
            .expect("projection indices come from this schema");
        bytes = bytes.saturating_add(definition.name().len());
    }
    bytes
}

fn estimate_result_bytes(
    table: &Table,
    column_indices: &[usize],
    selected_rows: &RowSelection,
) -> usize {
    let mut bytes = estimate_fixed_result_bytes(table, column_indices, selected_rows.len());
    let mut string_bytes = vec![None; table.schema().len()];

    for &index in column_indices {
        let selected_string_bytes = *string_bytes[index].get_or_insert_with(|| {
            table
                .column(index)
                .expect("storage columns match the schema")
                .cloned_string_bytes(selected_rows.iter())
        });
        bytes = bytes.saturating_add(selected_string_bytes);
    }
    bytes
}
