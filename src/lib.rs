//! RustHouse is an experimental, compact analytical database.
//!
//! The current query surface deliberately consists of one statement shape:
//! `SELECT <literal> AS <identifier>`. Use [`execute`] to evaluate a statement
//! and [`write_csv`] to serialize its result.

mod csv;
mod error;
mod parser;
mod result;

pub use csv::write_csv;
pub use error::QueryError;
pub use result::{Column, DataType, QueryResult, Value};

/// Maximum query size accepted by the command-line interface, in bytes.
pub const MAX_QUERY_BYTES: usize = 1024 * 1024;

/// Executes one literal `SELECT` statement.
///
/// Supported literals are signed 64-bit integers, finite 64-bit floating-point
/// values, booleans, and single-quoted strings. String quotes are escaped by
/// doubling them. Identifiers may be unquoted or double-quoted.
///
/// # Examples
///
/// ```
/// use rusthouse::{DataType, Value, execute};
///
/// let result = execute("SELECT 42 AS answer")?;
/// assert_eq!(result.columns[0].name, "answer");
/// assert_eq!(result.columns[0].data_type, DataType::Int64);
/// assert_eq!(result.rows, vec![vec![Value::Int64(42)]]);
/// # Ok::<(), rusthouse::QueryError>(())
/// ```
pub fn execute(sql: &str) -> Result<QueryResult, QueryError> {
    let selection = parser::parse(sql)?;
    Ok(QueryResult::single(selection.identifier, selection.value))
}

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
