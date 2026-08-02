//! RustHouse is an experimental, compact analytical database.
//!
//! [`Database`] is the public entry point for executing SQL. The current SQL
//! surface supports typed `CREATE TABLE`, `INSERT INTO ... VALUES`, and
//! single-table projection `SELECT` statements.

pub mod catalog;
pub mod csv;
mod database;
mod error;
mod schema;
pub mod sql;

pub use catalog::Catalog;
pub use database::{
    DEFAULT_MAX_COLUMNS_PER_TABLE, DEFAULT_MAX_INPUT_BYTES, Database, DatabaseConfig,
    ExecutionResult, QueryResult,
};
pub use error::{Error, Result};
pub use schema::{ColumnSchema, DataType, TableSchema};

pub mod storage;

pub use storage::{Column, InsertError, MAX_BATCH_ROWS, Schema, SchemaError, Table, Value};

/// Returns the product name while the first storage engine is being built.
///
/// # Examples
///
/// ```
/// assert_eq!(rusthouse::product_name(), "RustHouse");
/// ```
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
