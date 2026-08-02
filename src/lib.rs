//! RustHouse is an experimental, compact analytical database.
//!
//! [`Database`] is the public entry point for executing SQL. The current SQL
//! surface is deliberately small and supports one `CREATE TABLE` statement per
//! call.

pub mod catalog;
mod database;
mod error;
mod schema;
pub mod sql;

pub use catalog::Catalog;
pub use database::{
    DEFAULT_MAX_COLUMNS_PER_TABLE, DEFAULT_MAX_INPUT_BYTES, Database, DatabaseConfig,
};
pub use error::{Error, Result};
pub use schema::{ColumnSchema, DataType, TableSchema};

pub mod storage;

pub use storage::{
    Column, ColumnSchema, DataType, InsertError, MAX_BATCH_ROWS, Schema, SchemaError, Table, Value,
};

/// Returns the product name while the first storage engine is being built.
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
