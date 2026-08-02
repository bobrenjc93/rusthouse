#![deny(missing_docs)]

//! RustHouse is an experimental, compact analytical database.
//!
//! The crate provides bounded parsers for its supported SQL statements, a
//! case-insensitive in-memory [`Catalog`], typed columnar [`Table`] storage,
//! and streaming [`csv`] export.

mod catalog;
pub mod csv;
pub mod sql;
mod table;

pub use catalog::{Catalog, CatalogError};
pub use table::{Column, ColumnSchema, DataType, Schema, Table, TableError, TableLimits, Value};

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
