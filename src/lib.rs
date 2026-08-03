//! RustHouse is an experimental, compact analytical database.

pub mod catalog;
pub mod scan;
pub mod snapshot;
pub mod sql;
pub mod storage;
pub mod table_snapshot;

pub use catalog::{Catalog, CatalogError, CatalogLimits, DEFAULT_MAX_TABLES, SelectResult};
pub use scan::{ComparisonOperator, RowSelection, ScanError, SelectionAllocationError};
pub use sql::{
    ColumnDefinition, ComparisonPredicate, CreateTableStatement, IdentifierContext,
    InsertParseLimits, InsertStatement, ParseError, ParseErrorKind, ParseLimits, SelectParseLimits,
    SelectProjection, SelectStatement, parse_create_table, parse_create_table_with_limits,
    parse_insert, parse_insert_with_limits, parse_select, parse_select_with_limits,
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
