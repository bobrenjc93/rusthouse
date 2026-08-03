//! Execution boundaries connecting parsed SQL to storage.

use std::error::Error;
use std::fmt;

use crate::{InsertError, InsertStatement, Int64Table, SelectStatement};

/// An error produced while executing a parsed [`InsertStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The destination table rejected the value.
    Insert(InsertError),
}

impl fmt::Display for InsertExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::Insert(error) => write!(formatter, "could not insert row: {error}"),
        }
    }
}

impl Error for InsertExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Insert(error) => Some(error),
            Self::UnknownTable { .. } => None,
        }
    }
}

impl From<InsertError> for InsertExecutionError {
    fn from(error: InsertError) -> Self {
        Self::Insert(error)
    }
}

/// An error produced while executing a parsed [`SelectStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the table's only column.
    UnknownColumn { name: String },
}

impl fmt::Display for SelectExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
        }
    }
}

impl Error for SelectExecutionError {}

/// Executes one parsed `INSERT` against one explicitly named table.
///
/// `expected_table_name` is compared exactly with the identifier retained by
/// the parser, including ASCII case. A mismatch is reported as an unknown
/// table without consulting or mutating `table`. On a match, storage
/// nullability and row-cap errors are preserved in [`InsertExecutionError`]
/// and also leave the table unchanged.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     Int64Table, ParseLimits, Schema, execute_insert, parse_insert,
/// };
///
/// let statement = parse_insert(
///     "INSERT INTO readings VALUES (7)",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", false), 1);
///
/// execute_insert("readings", &mut table, &statement)?;
/// assert_eq!(table.values(), &[Some(7)]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_insert(
    expected_table_name: &str,
    table: &mut Int64Table,
    statement: &InsertStatement,
) -> Result<(), InsertExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(InsertExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    table.append(statement.value()).map_err(Into::into)
}

/// Executes one parsed `SELECT` against one explicitly named table.
///
/// `expected_table_name` and the table's column name are compared exactly
/// with the identifiers retained by the parser, including ASCII case. The
/// returned slice borrows the table's existing column storage without copying
/// its values. An optional `LIMIT` returns a prefix of that same storage.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     Int64Table, ParseLimits, Schema, execute_select, parse_select,
/// };
///
/// let statement = parse_select(
///     "SELECT value FROM readings",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 2);
/// table.append_batch(&[Some(7), None])?;
///
/// let values = execute_select("readings", &table, &statement)?;
/// assert_eq!(values, &[Some(7), None]);
/// assert!(std::ptr::eq(values, table.values()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_select<'table>(
    expected_table_name: &str,
    table: &'table Int64Table,
    statement: &SelectStatement,
) -> Result<&'table [Option<i64>], SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if statement.column_name().as_str() != table.schema().column().name() {
        return Err(SelectExecutionError::UnknownColumn {
            name: statement.column_name().as_str().to_owned(),
        });
    }

    let values = table.values();
    let row_count = statement.limit().unwrap_or(values.len()).min(values.len());
    Ok(&values[..row_count])
}
