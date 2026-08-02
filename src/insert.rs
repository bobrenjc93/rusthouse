//! Execution for one-row SQL INSERT statements.

use std::error::Error;
use std::fmt;

use crate::catalog::Catalog;
use crate::parser::{ParseError, parse_insert};
use crate::storage::InsertError;

/// A SQL INSERT that could not be parsed, resolved, or inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteInsertError {
    Parse(ParseError),
    UnknownTable { name: String },
    Insertion(InsertError),
}

impl fmt::Display for ExecuteInsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "invalid INSERT statement: {error}"),
            Self::UnknownTable { name } => write!(formatter, "unknown table {name:?}"),
            Self::Insertion(error) => write!(formatter, "row insertion failed: {error}"),
        }
    }
}

impl Error for ExecuteInsertError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Insertion(error) => Some(error),
            Self::UnknownTable { .. } => None,
        }
    }
}

impl From<ParseError> for ExecuteInsertError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl From<InsertError> for ExecuteInsertError {
    fn from(error: InsertError) -> Self {
        Self::Insertion(error)
    }
}

/// Parses and executes one `INSERT INTO <table> VALUES (...)` statement.
///
/// The tuple is matched to the table schema by position. Catalog lookup is
/// ASCII case-insensitive. Parsing and lookup complete before mutation, and
/// [`crate::Table::insert_row`] validates the whole row atomically.
///
/// ```
/// use rusthouse::{Catalog, Value, execute_insert, parse_create_table};
///
/// let mut catalog = Catalog::new();
/// catalog.create_table(parse_create_table(
///     "CREATE TABLE events (id Int64, label String)",
/// )?)?;
/// execute_insert("INSERT INTO events VALUES (7, 'ready')", &mut catalog)?;
/// assert_eq!(catalog.table("events").unwrap().row(0).unwrap()[0].to_owned(), Value::Int64(7));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_insert(input: &str, catalog: &mut Catalog) -> Result<(), ExecuteInsertError> {
    let statement = parse_insert(input)?;
    let table = catalog.table_mut(&statement.table_name).ok_or_else(|| {
        ExecuteInsertError::UnknownTable {
            name: statement.table_name.clone(),
        }
    })?;
    table.insert_row(&statement.values)?;
    Ok(())
}
