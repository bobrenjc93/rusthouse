//! RustHouse is an experimental, compact analytical database.

pub mod aggregate;
pub mod catalog;
pub mod csv;
pub mod distinct;
pub mod execution;
pub mod grouping;
pub mod join;
pub mod order;
pub mod parser;
pub mod scan;
pub mod shared_catalog;
pub mod snapshot;

pub use aggregate::{
    AggregateError, AggregateLimits, NullableI64Aggregates, NullableI64Counts, RowSelection,
    aggregate_nullable_i64, count_nullable_i64,
};
pub use catalog::{Catalog, CatalogError, CatalogLimits};
pub use csv::{CsvIngestError, CsvIngestLimits, ingest_csv_with_names};
pub use distinct::{DistinctError, DistinctLimits, distinct_nullable_i64};
pub use execution::{
    InsertExecutionError, SelectDistinctExecutionError, SelectExecutionError, execute_insert,
    execute_scalar_count, execute_scalar_sum, execute_scalar_sum_with_limits, execute_select,
    execute_select_distinct, execute_select_with_limits, execute_select_with_order_limits,
};
pub use grouping::{
    GroupedCountError, GroupedCountLimits, NullableI64GroupedCount, grouped_count_nullable_i64,
};
pub use join::{JoinError, JoinLimits, JoinRowPair, inner_equi_join_nullable_i64};
pub use order::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};
pub use parser::{
    ColumnDefinition, ComparisonPredicate, CreateTableStatement, EqualityPredicate, Identifier,
    InsertStatement, NullnessPredicate, OrderByClause, ParseError, ParseLimits,
    ScalarCountArgument, ScalarCountStatement, ScalarSumStatement, SelectDistinctStatement,
    SelectPredicate, SelectStatement, parse_create_table, parse_insert, parse_scalar_count,
    parse_scalar_sum, parse_select, parse_select_distinct,
};
pub use scan::{
    ComparisonOperator, NullPredicate, ScanError, ScanLimits, scan_nullable_i64,
    scan_nullable_i64_nullness,
};
pub use shared_catalog::{SharedCatalog, SharedCatalogError};
pub use snapshot::{
    NullableI64PayloadCodec, NullableI64PayloadError, SnapshotCodec, SnapshotError,
    SnapshotFileError,
};
mod storage;

pub use storage::{ColumnSchema, DataType, InsertError, Int64Table, Schema};

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
