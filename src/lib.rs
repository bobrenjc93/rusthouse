//! RustHouse is an experimental, compact analytical database.
//!
//! The crate currently provides the typed, columnar in-memory storage layer
//! used to build the rest of the database.

pub mod error;
pub mod storage;
pub mod value;

pub use error::{Result, StorageError};
pub use storage::{Column, ColumnDef, DEFAULT_ROW_LIMIT, Table};
pub use value::{DataType, Value};

/// Returns the product name.
#[must_use]
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
