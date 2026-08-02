//! RustHouse is an experimental, compact analytical database.

pub mod catalog;
pub mod formats;
pub mod lexer;
pub mod query;
pub mod storage;

pub use catalog::{Catalog, CreateTableError, TableNotFoundError};
pub use formats::{CsvWithNamesError, CsvWithNamesWriter};
pub use query::{ScalarSelect, ScalarSelectError, parse_scalar_select};
pub use storage::{
    BatchInsertError, Column, ColumnSchema, DataType, InsertError, Schema, SchemaError, Table,
    Value,
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
