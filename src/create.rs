//! Materialization of parsed `CREATE TABLE` statements.

use crate::parser::{CreateTableStatement, Identifier};
use crate::storage::{Int64Table, Schema};

/// An unregistered named table produced from one parsed `CREATE TABLE`.
///
/// A table entry owns its name and storage, but it does not belong to a
/// catalog and performs no name normalization or duplicate detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    table_name: Identifier,
    table: Int64Table,
}

impl TableEntry {
    /// Returns the table name exactly as it appeared in the statement.
    pub fn table_name(&self) -> &Identifier {
        &self.table_name
    }

    /// Returns the materialized table.
    pub fn table(&self) -> &Int64Table {
        &self.table
    }

    /// Returns the materialized table for mutation.
    pub fn table_mut(&mut self) -> &mut Int64Table {
        &mut self.table
    }

    /// Consumes the entry and returns its retained name and table.
    pub fn into_parts(self) -> (Identifier, Int64Table) {
        (self.table_name, self.table)
    }
}

/// Materializes one parsed `CREATE TABLE` as an empty, unregistered table.
///
/// The parsed table and column identifiers retain their original spelling.
/// The supplied row cap is applied directly to the new table.
///
/// # Examples
///
/// ```
/// use rusthouse::{ParseLimits, materialize_create_table, parse_create_table};
///
/// let statement = parse_create_table(
///     "CREATE TABLE Metrics (Reading Int64 NOT NULL)",
///     ParseLimits::default(),
/// )?;
/// let entry = materialize_create_table(statement, 1_000);
///
/// assert_eq!(entry.table_name().as_str(), "Metrics");
/// assert_eq!(entry.table().schema().column().name(), "Reading");
/// assert!(!entry.table().schema().column().is_nullable());
/// assert_eq!(entry.table().row_cap(), 1_000);
/// assert!(entry.table().is_empty());
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn materialize_create_table(statement: CreateTableStatement, row_cap: usize) -> TableEntry {
    let table_name = statement.table_name().clone();
    let column = statement.column();
    let schema = Schema::int64(column.name().as_str(), column.is_nullable());

    TableEntry {
        table_name,
        table: Int64Table::new(schema, row_cap),
    }
}
