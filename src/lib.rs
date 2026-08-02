//! RustHouse is an experimental, compact analytical database.

pub mod format;
pub mod sql;
pub mod storage;

pub use format::{CsvError, CsvWithNamesWriter, write_csv_with_names};
pub use storage::{
    Column, ColumnSchema, ComparisonOperator, DataType, Row, ScanError, Schema, StorageError,
    Table, Value,
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
