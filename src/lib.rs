//! RustHouse is an experimental, compact analytical database.

pub mod aggregate;
pub mod batch;
pub mod catalog;
pub mod cli;
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
    aggregate_nullable_i64, count_nullable_i64, min_nullable_i64,
};
pub use batch::{SharedDatabase, SharedDatabaseError};
pub use catalog::{
    Catalog, CatalogCsvIngestError, CatalogError, CatalogLimits, CatalogSnapshotRestoreError,
};
pub use cli::{
    DEFAULT_MAX_SESSION_BYTES, DEFAULT_MAX_SESSION_ROWS_PER_TABLE, DEFAULT_MAX_SESSION_STATEMENTS,
    DEFAULT_MAX_SESSION_TABLES, SessionError, SessionLimits, run_session,
};
pub use csv::{
    CsvIngestError, CsvIngestLimits, CsvReaderIngestError, ingest_csv_with_names,
    ingest_csv_with_names_from_reader,
};
pub use distinct::{DistinctError, DistinctLimits, distinct_nullable_i64};
pub use execution::{
    InnerJoinExecutionError, InsertExecutionError, SelectDistinctExecutionError,
    SelectExecutionError, execute_inner_join, execute_insert, execute_scalar_count,
    execute_scalar_count_with_limits, execute_scalar_min, execute_scalar_sum,
    execute_scalar_sum_with_limits, execute_select, execute_select_distinct,
    execute_select_with_limits, execute_select_with_order_limits,
};
pub use grouping::{
    GroupedCountError, GroupedCountLimits, NullableI64GroupedCount, grouped_count_nullable_i64,
};
pub use join::{
    JoinError, JoinLimits, JoinRowPair, LeftOuterJoinRowPair, inner_equi_join_nullable_i64,
    left_outer_equi_join_nullable_i64,
};
pub use order::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};
pub use parser::{
    ColumnDefinition, ComparisonPredicate, CreateTableStatement, EqualityPredicate, Identifier,
    InnerJoinStatement, InsertStatement, NullnessPredicate, OrderByClause, ParseError, ParseLimits,
    ScalarCountArgument, ScalarCountStatement, ScalarMinStatement, ScalarSumStatement,
    SelectDistinctStatement, SelectPredicate, SelectStatement, parse_create_table,
    parse_inner_join, parse_insert, parse_scalar_count, parse_scalar_min, parse_scalar_sum,
    parse_select, parse_select_distinct,
};
pub use scan::{
    ComparisonOperator, NullPredicate, ScanError, ScanLimits, scan_nullable_i64,
    scan_nullable_i64_nullness,
};
pub use shared_catalog::{SharedCatalog, SharedCatalogError};
pub use snapshot::{
    Int64TableFileRecovery, Int64TableFileRecoveryError, Int64TableFileRecoverySource,
    Int64TableFileRestoreError, Int64TableRestoreError, NullableI64PayloadCodec,
    NullableI64PayloadError, SnapshotCodec, SnapshotError, SnapshotFileError, SnapshotReplaceError,
    restore_int64_table, restore_int64_table_from_file, restore_int64_table_from_file_with_backup,
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
