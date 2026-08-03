//! RustHouse is an experimental, compact analytical database.
//!
//! The catalog API provides a complete in-memory SQL workflow:
//!
//! ```
//! use rusthouse::{Catalog, Value};
//!
//! let mut catalog = Catalog::new();
//! catalog.execute_create("CREATE TABLE events (region String, user_id Int64)")?;
//! catalog.execute_insert(
//!     "INSERT INTO events VALUES ('east', 1), ('east', 1), ('west', 2)",
//! )?;
//!
//! let grouped = catalog.execute_select(
//!     "SELECT region, COUNT(*) AS events FROM events GROUP BY region",
//! )?;
//! assert_eq!(
//!     grouped.grouped_rows().collect::<Vec<_>>(),
//!     [(&Value::from("east"), 2), (&Value::from("west"), 1)],
//! );
//!
//! let distinct = catalog.execute_select("SELECT COUNT(DISTINCT user_id) FROM events")?;
//! assert_eq!(distinct.scalar_value(), Some(&Value::Int64(2)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod catalog;
pub mod cli;
pub mod csv;
pub mod grouping;
pub mod reduction;
pub mod scan;
pub mod snapshot;
pub mod sql;
pub mod storage;
pub mod table_snapshot;

pub use catalog::{
    Catalog, CatalogError, CatalogLimits, CatalogSnapshotError, DEFAULT_MAX_GROUPED_RESULT_BYTES,
    DEFAULT_MAX_GROUPS, DEFAULT_MAX_TABLES, MAX_AGGREGATE_RESULT_BYTES, SelectResult,
};
pub use csv::{write_csv_with_names, write_select_csv_with_names};
pub use grouping::{GroupedCount, GroupedCountError};
pub use reduction::ReductionError;
pub use scan::{ComparisonOperator, RowSelection, ScanError, SelectionAllocationError};
pub use sql::{
    AggregateFunction, AggregateProjection, ColumnDefinition, ComparisonPredicate,
    CreateTableStatement, IdentifierContext, InsertParseLimits, InsertStatement, OrderByClause,
    OrderDirection, ParseError, ParseErrorKind, ParseLimits, SelectParseLimits, SelectProjection,
    SelectStatement, parse_create_table, parse_create_table_with_limits, parse_insert,
    parse_insert_with_limits, parse_select, parse_select_with_limits,
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
