//! RustHouse is an experimental, compact analytical database.
//!
//! The primary embedded interface is [`Catalog`], which owns typed columnar
//! tables and executes the crate's bounded SQL subset. Lower-level callers can
//! use [`Table`] directly for scans, reductions, and grouped counts.
//!
//! # Example
//!
//! ```
//! use rusthouse::{Catalog, Value};
//!
//! let mut catalog = Catalog::new();
//! catalog.execute_create("CREATE TABLE events (active Bool)")?;
//! catalog.execute_insert("INSERT INTO events VALUES (true), (false), (true)")?;
//!
//! let result = catalog.execute_select(
//!     "SELECT active, COUNT(*) AS rows FROM events GROUP BY active",
//! )?;
//! let groups = result
//!     .grouped_rows()
//!     .map(|(value, count)| (value.clone(), count))
//!     .collect::<Vec<_>>();
//! assert_eq!(
//!     groups,
//!     [(Value::Bool(false), 1), (Value::Bool(true), 2)],
//! );
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
