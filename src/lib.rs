//! RustHouse is an experimental, compact analytical database.
//!
//! # Database quickstart
//!
//! [`Database`] executes semicolon-delimited SQL batches and returns typed,
//! owned results. Its default resource limits are bounded; use
//! [`QueryResultLimits`] when an embedding needs tighter bounds.
//!
//! ```
//! use rusthouse::{
//!     BatchDataType, Database, DatabaseResult, QueryResult, QueryResultLimits,
//!     ResultColumn, StatementResult, Value,
//! };
//!
//! # fn main() -> DatabaseResult<()> {
//! let limits = QueryResultLimits {
//!     max_rows: 10,
//!     ..QueryResultLimits::default()
//! };
//! let mut database = Database::with_query_result_limits(limits);
//! let results = database.execute(
//!     "CREATE TABLE readings (value Int64); \
//!      INSERT INTO readings VALUES (7), (-2); \
//!      SELECT value FROM readings ORDER BY value;",
//! )?;
//!
//! let query: &QueryResult = match &results[2] {
//!     StatementResult::Query(query) => query,
//!     StatementResult::Command { .. } => panic!("SELECT must return rows"),
//! };
//! assert_eq!(
//!     query.columns,
//!     vec![ResultColumn {
//!         name: "value".to_owned(),
//!         data_type: BatchDataType::Int64,
//!     }],
//! );
//! assert_eq!(
//!     query.rows,
//!     vec![vec![Value::Int64(-2)], vec![Value::Int64(7)]],
//! );
//! # Ok(())
//! # }
//! ```
//!
//! For concurrent access from cloned handles, use [`SharedDatabase`]. It
//! serializes mutations while allowing supported read-only queries to share a
//! read lock.

pub mod aggregate;
pub mod batch;
pub mod catalog;
pub mod cli;
pub mod csv;
pub mod distinct;
pub mod execution;
pub mod grouping;
pub mod http;
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
pub use batch::engine::{QueryResult, QueryResultLimits, ResultColumn, StatementResult};
pub use batch::error::{Error as DatabaseError, Result as DatabaseResult};
pub use batch::value::{DataType as BatchDataType, Value};
pub use batch::{
    DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP, DEFAULT_INT64_MIN_MAX_INDEX_BLOCK_ROWS,
    DEFAULT_INT64_MIN_MAX_INDEX_BLOCKS, DEFAULT_INT64_MIN_MAX_INDEX_BYTES,
    DEFAULT_MAX_INT64_RANGE_PARTITION_BYTES, DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS,
    DEFAULT_MAX_INT64_RANGE_PARTITIONS, Database, DatabaseMetrics, DatabaseRleSnapshotRestoreError,
    DatabaseSnapshotRestoreEntry, DatabaseSnapshotRestoreError, DatabaseSnapshotSetRestoreError,
    IndexPruningMetrics, Int64MinMaxBlockMetadata, Int64MinMaxIndexAdmission, Int64MinMaxIndexInfo,
    Int64MinMaxIndexLimits, Int64MinMaxIndexRejection, Int64RangePartition,
    Int64RangePartitionError, Int64RangePartitionLimits, SharedDatabase, SharedDatabaseError,
    SharedDatabaseSnapshotRestoreError, SharedDatabaseSnapshotSetRestoreError, TableLimits,
};
#[cfg(unix)]
pub use batch::{
    DatabaseInt64WalEnableError, DatabaseInt64WalRecoveryError,
    DatabaseInt64WalRegistryEnableError, DatabaseInt64WalRegistryRecoveryError,
    DatabaseRleSnapshotSaveError, DatabaseSnapshotSaveError, Int64WriteAheadLogCommitError,
    Int64WriteAheadLogCorruption, Int64WriteAheadLogError, Int64WriteAheadLogLimitError,
    Int64WriteAheadLogLimits, Int64WriteAheadLogRegistryCorruption,
    Int64WriteAheadLogRegistryError, Int64WriteAheadLogRegistryLimitError,
    Int64WriteAheadLogRegistryLimits, SharedDatabaseRleSnapshotSaveError,
    SharedDatabaseSnapshotSaveError,
};
pub use catalog::{
    Catalog, CatalogCsvIngestError, CatalogCsvReaderIngestError, CatalogError, CatalogLimits,
    CatalogSnapshotRestoreError,
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
    InnerJoinExecutionError, InsertExecutionError, LeftJoinExecutionError,
    SelectDistinctExecutionError, SelectExecutionError, execute_inner_join, execute_insert,
    execute_left_join, execute_scalar_count, execute_scalar_count_with_limits, execute_scalar_min,
    execute_scalar_sum, execute_scalar_sum_with_limits, execute_select, execute_select_distinct,
    execute_select_with_limits, execute_select_with_order_limits,
};
pub use grouping::{
    GroupedCountError, GroupedCountLimits, NullableI64GroupedCount, grouped_count_nullable_i64,
};
pub use http::{
    ClickHousePrincipal, ClickHousePrincipalRole, DEFAULT_HTTP_CONNECTION_READ_TIMEOUT,
    DEFAULT_HTTP_CONNECTION_WRITE_TIMEOUT, DEFAULT_MAX_HTTP_HEADER_BYTES,
    DEFAULT_MAX_HTTP_HEADER_COUNT, DEFAULT_MAX_HTTP_RESPONSE_BYTES, DEFAULT_MAX_HTTP_SQL_BYTES,
    HttpConnectionFailure, HttpListenerError, HttpListenerLimits, HttpListenerReport,
    HttpQueryError, HttpQueryLimits, MAX_HTTP_NAMED_PRINCIPALS, handle_http_query,
    handle_http_query_read_only_with_bearer_token,
    handle_http_query_read_only_with_bearer_token_and_limits,
    handle_http_query_read_only_with_clickhouse_key,
    handle_http_query_read_only_with_clickhouse_key_and_limits,
    handle_http_query_read_only_with_clickhouse_principal,
    handle_http_query_read_only_with_clickhouse_principal_and_limits,
    handle_http_query_with_bearer_token, handle_http_query_with_bearer_token_and_limits,
    handle_http_query_with_clickhouse_key, handle_http_query_with_clickhouse_key_and_limits,
    handle_http_query_with_clickhouse_principal,
    handle_http_query_with_clickhouse_principal_and_limits,
    handle_http_query_with_clickhouse_principal_set, handle_http_query_with_limits,
    serve_http_read_only, serve_http_read_only_concurrently_with_clickhouse_key,
    serve_http_read_only_concurrently_with_clickhouse_key_and_limits,
    serve_http_read_only_with_clickhouse_key, serve_http_read_only_with_clickhouse_key_and_limits,
    serve_http_read_only_with_limits, serve_http_with_clickhouse_key,
    serve_http_with_clickhouse_key_and_limits,
};
pub use join::{
    JoinError, JoinLimits, JoinRowPair, LeftOuterJoinRowPair, inner_equi_join_nullable_i64,
    left_outer_equi_join_nullable_i64,
};
pub use order::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};
pub use parser::{
    ColumnDefinition, ComparisonPredicate, CreateTableStatement, EqualityPredicate, Identifier,
    InnerJoinStatement, InsertStatement, LeftJoinStatement, NullnessPredicate, OrderByClause,
    ParseError, ParseLimits, ScalarCountArgument, ScalarCountStatement, ScalarMinStatement,
    ScalarSumStatement, SelectDistinctStatement, SelectPredicate, SelectStatement,
    parse_create_table, parse_inner_join, parse_insert, parse_left_join, parse_scalar_count,
    parse_scalar_min, parse_scalar_sum, parse_select, parse_select_distinct,
};
pub use scan::{
    ComparisonOperator, NullPredicate, ScanError, ScanLimits, scan_nullable_i64,
    scan_nullable_i64_nullness,
};
pub use shared_catalog::{SharedCatalog, SharedCatalogError};
pub use snapshot::{
    Int64TableFileRecovery, Int64TableFileRecoveryError, Int64TableFileRecoverySource,
    Int64TableFileRestoreError, Int64TablePayloadCodec, Int64TablePayloadError,
    Int64TablePayloadFileRecovery, Int64TablePayloadFileRecoveryError,
    Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError, Int64TableRestoreError,
    Int64TableRleFileRestoreError, NullableI64PayloadCodec, NullableI64PayloadError,
    NullableI64RlePayloadCodec, NullableI64RlePayloadError, SnapshotCodec, SnapshotError,
    SnapshotFileError, restore_int64_table, restore_int64_table_from_file,
    restore_int64_table_from_file_with_backup, restore_int64_table_payload_from_file,
    restore_int64_table_payload_from_file_with_backup, restore_int64_table_rle_from_file,
};
#[cfg(unix)]
pub use snapshot::{
    Int64TableFileSaveError, Int64TablePayloadFileSaveError, Int64TableRleFileSaveError,
    SnapshotReplaceError, save_int64_table_payload_to_file, save_int64_table_rle_to_file,
    save_int64_table_to_file,
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
