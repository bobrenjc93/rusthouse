//! RustHouse is an experimental, compact analytical database.

pub mod snapshot;
pub mod sql;
pub mod storage;
pub mod table_snapshot;

pub use sql::{
    ColumnDefinition, CreateTableStatement, IdentifierContext, ParseError, ParseErrorKind,
    ParseLimits, parse_create_table, parse_create_table_with_limits,
};
pub use storage::{DataType, Field, Table, TableError, Value};
pub use table_snapshot::{TableSnapshotError, TableSnapshotLocation};

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
