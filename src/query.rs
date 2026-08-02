//! Execution for the supported bounded query surface.

use std::error::Error;
use std::fmt;

use crate::parser::{ParseError, parse_select_all};
use crate::storage::{ColumnSchema, Table, Value};

/// Resource limits applied while materializing a query result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryLimits {
    pub max_result_rows: usize,
}

impl QueryLimits {
    /// The default maximum number of owned rows returned by one query.
    pub const DEFAULT_MAX_RESULT_ROWS: usize = 10_000;

    pub const fn new(max_result_rows: usize) -> Self {
        Self { max_result_rows }
    }
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_RESULT_ROWS)
    }
}

/// An owned result from a table scan.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Vec<Value>>,
    /// Whether rows were omitted because `max_result_rows` was reached.
    pub truncated: bool,
}

/// A query that could not be parsed or resolved against the supplied table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    Parse(ParseError),
    TableNameMismatch {
        requested: String,
        available: String,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "invalid SELECT statement: {error}"),
            Self::TableNameMismatch {
                requested,
                available,
            } => write!(
                formatter,
                "query requested table {requested:?}, but the supplied table is named {available:?}"
            ),
        }
    }
}

impl Error for QueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::TableNameMismatch { .. } => None,
        }
    }
}

impl From<ParseError> for QueryError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Parses and executes exactly `SELECT * FROM <identifier>` against one table.
///
/// The requested unquoted table identifier is matched ASCII case-insensitively.
/// Rows retain insertion order and are materialized as owned [`Value`]s. At
/// most [`QueryLimits::max_result_rows`] rows are returned; [`QueryResult::truncated`]
/// reports whether the table contained additional rows.
///
/// ```
/// use rusthouse::{
///     ColumnSchema, DataType, QueryLimits, Schema, Table, Value, execute_select,
/// };
///
/// let schema = Schema::new(vec![
///     ColumnSchema::new("id", DataType::Int64, false),
/// ])?;
/// let mut table = Table::new(schema);
/// table.insert_row(&[Value::Int64(7)])?;
///
/// let result = execute_select(
///     "SELECT * FROM events;",
///     "events",
///     &table,
///     QueryLimits::new(100),
/// )?;
/// assert_eq!(result.rows, vec![vec![Value::Int64(7)]]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_select(
    input: &str,
    table_name: &str,
    table: &Table,
    limits: QueryLimits,
) -> Result<QueryResult, QueryError> {
    let statement = parse_select_all(input)?;
    if !statement.table_name.eq_ignore_ascii_case(table_name) {
        return Err(QueryError::TableNameMismatch {
            requested: statement.table_name,
            available: table_name.to_owned(),
        });
    }

    let row_count = table.len().min(limits.max_result_rows);
    let rows = (0..row_count)
        .map(|row_index| {
            table
                .columns()
                .iter()
                .map(|column| {
                    column
                        .get(row_index)
                        .expect("all table columns have the table row count")
                        .to_owned()
                })
                .collect()
        })
        .collect();

    Ok(QueryResult {
        columns: table.schema().columns().to_vec(),
        rows,
        truncated: table.len() > row_count,
    })
}
