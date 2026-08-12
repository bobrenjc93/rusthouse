use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[cfg(test)]
use crate::batch::aggregate_scheduler::TestGlobalAggregateWorkerBudget as GlobalAggregateWorkerBudget;
use crate::batch::aggregate_scheduler::{
    GlobalAggregateParallelism, parallel_aggregate_partition, run_grouped_aggregate,
};
use crate::batch::catalog::Catalog;
use crate::batch::csv::{self, CsvIngestError, CsvIngestLimits};
use crate::batch::error::{Error, Result};
use crate::batch::json_compact_each_row::{
    self, JsonCompactEachRowIngestError, JsonCompactEachRowIngestLimits,
};
use crate::batch::scalar_cast::{
    checked_string_to_bool, checked_string_to_float64, checked_string_to_int64, decimal_text_cmp,
    ordering_string_to_float64, validate_string_to_float64_syntax, validate_string_to_int64_syntax,
};
use crate::batch::scalar_float64;
use crate::batch::scalar_nullable_int64;
use crate::batch::scalar_string;
use crate::batch::scalar_text;
use crate::batch::sql::{
    self, AggregateArgument, AggregateFunction, AlterUpdateLiteral, AlterUpdateValue,
    ComparisonOperator, CrossJoin, CurrentDatabaseSelect, DeleteComparisonPredicate, Having,
    HavingPredicate, LiteralSelect, Operand, OrderBy, Predicate, SUPPORTED_FUNCTION_NAMES, Select,
    SelectItem, Statement, VersionSelect,
};
use crate::batch::storage::{
    Column, ColumnDef, Int64Filter, Int64MinMaxIndexScan, PreparedInsertRows, Table,
    validate_row_selection, validate_table_name,
};
use crate::batch::tsv::{self, TsvIngestError, TsvIngestLimits};
use crate::batch::value::{DataType, Value, ValueRef};
#[cfg(unix)]
use crate::batch::wal::{
    self, ActiveInt64WriteAheadLogs, Int64WalBootstrap, Int64WriteAheadLog,
    Int64WriteAheadLogError, Int64WriteAheadLogLimits, Int64WriteAheadLogRegistryError,
    Int64WriteAheadLogRegistryLimits,
};
use crate::snapshot::{
    Int64TablePayloadCodec, Int64TablePayloadFileRecoveryError,
    Int64TablePayloadFileRecoverySource, Int64TablePayloadFileRestoreError,
    Int64TableRleFileRestoreError, NullableI64RlePayloadCodec, SnapshotCodec,
    restore_int64_table_payload_from_file, restore_int64_table_payload_from_file_with_backup,
    restore_int64_table_rle_from_file,
};
#[cfg(unix)]
use crate::snapshot::{Int64TablePayloadFileSaveError, Int64TableRleFileSaveError};
use crate::storage::{Int64Table, Schema};

pub use crate::batch::aggregate_scheduler::{
    COUNT_IF_PARALLEL_ROW_THRESHOLD, COUNT_IF_PARALLEL_ROWS_PER_WORKER,
    DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP, GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
    GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER, MAX_COUNT_IF_PARALLEL_WORKERS,
    MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS,
};
pub use crate::batch::storage::{
    DEFAULT_INT64_MIN_MAX_INDEX_BLOCK_ROWS, DEFAULT_INT64_MIN_MAX_INDEX_BLOCKS,
    DEFAULT_INT64_MIN_MAX_INDEX_BYTES, DEFAULT_MAX_CELLS_PER_TABLE, DEFAULT_MAX_COLUMNS_PER_TABLE,
    DEFAULT_MAX_INT64_RANGE_PARTITION_BYTES, DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS,
    DEFAULT_MAX_INT64_RANGE_PARTITIONS, DEFAULT_MAX_ROWS_PER_TABLE, Int64MinMaxBlockMetadata,
    Int64MinMaxIndexAdmission, Int64MinMaxIndexInfo, Int64MinMaxIndexLimits,
    Int64MinMaxIndexRejection, Int64RangePartition, Int64RangePartitionError,
    Int64RangePartitionLimits, TableLimits,
};

/// Maximum estimated heap retained by the collecting [`Database::execute`] API.
pub const DEFAULT_MAX_RETAINED_RESULT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum source rows inspected by one table-backed `SELECT`.
pub const DEFAULT_MAX_QUERY_SCAN_ROWS: usize = DEFAULT_MAX_ROWS_PER_TABLE;
/// Maximum rows materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_ROWS: usize = 10_000;
/// Maximum scalar cells materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_VALUES: usize = 250_000;
/// Maximum estimated heap materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum temporary ordering state retained by one `SELECT`.
pub const DEFAULT_MAX_QUERY_ORDERING_STATE_BYTES: usize = 16 * 1024 * 1024;
/// Bytes charged for each filtered row index ordered by `ROW_NUMBER`.
pub const ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES: usize = std::mem::size_of::<usize>();
/// Bytes charged for each cached single-key `lengthUTF8` ordering entry.
pub const LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES: usize = 2 * std::mem::size_of::<usize>();
/// Bytes charged for each cached single-key String-to-`Float64` ordering entry.
pub const STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES: usize =
    std::mem::size_of::<CachedStringToFloat64Order>();
/// Maximum groups retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUPS: usize = 100_000;
/// Maximum grouped-key scalar cells retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUP_KEY_CELLS: usize = 500_000;
/// Maximum estimated grouped-key value-reference bytes retained by one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUP_KEY_BYTES: usize = 32 * 1024 * 1024;
/// Estimated bytes charged for each scalar cell retained in a grouped key.
pub const ESTIMATED_GROUP_KEY_CELL_BYTES: usize = std::mem::size_of::<ValueRef<'static>>();
/// Maximum aggregate state cells retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_AGGREGATE_STATE_CELLS: usize = 500_000;
/// Maximum estimated aggregate state heap retained by one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_AGGREGATE_STATE_BYTES: usize = 32 * 1024 * 1024;
/// Resource limits for source scans, query-result and mutation materialization,
/// ordering, and grouped working state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryResultLimits {
    /// Maximum rows in the source table of one table-backed `SELECT`, `DELETE`
    /// (including `ALTER TABLE DELETE`), or `ALTER TABLE UPDATE`.
    ///
    /// This is checked before row inspection and matching-row or replacement
    /// allocation. `WHERE` and `LIMIT` cannot reduce the charged scan for
    /// ordinary tables; a supported direct predicate on validated `Int64`
    /// range partitions is charged only for partitions that remain possible.
    /// Each `UNION` operand and each `CROSS JOIN` input is checked independently.
    pub max_scan_rows: usize,
    pub max_rows: usize,
    pub max_values: usize,
    /// Maximum estimated bytes retained in one query result and maximum cloned
    /// String payload materialized by one `ALTER TABLE UPDATE`.
    pub max_bytes: usize,
    /// Maximum temporary bytes used for ordered `ROW_NUMBER` row indices,
    /// single-key `lengthUTF8` ordering state, and single-key String-to-`Float64`
    /// ordering state. Each operator charges its complete filtered row set
    /// before allocating that state.
    pub max_ordering_state_bytes: usize,
    pub max_groups: usize,
    pub max_group_key_cells: usize,
    pub max_group_key_bytes: usize,
    pub max_aggregate_state_cells: usize,
    pub max_aggregate_state_bytes: usize,
}

impl Default for QueryResultLimits {
    fn default() -> Self {
        Self {
            max_scan_rows: DEFAULT_MAX_QUERY_SCAN_ROWS,
            max_rows: DEFAULT_MAX_QUERY_RESULT_ROWS,
            max_values: DEFAULT_MAX_QUERY_RESULT_VALUES,
            max_bytes: DEFAULT_MAX_QUERY_RESULT_BYTES,
            max_ordering_state_bytes: DEFAULT_MAX_QUERY_ORDERING_STATE_BYTES,
            max_groups: DEFAULT_MAX_QUERY_GROUPS,
            max_group_key_cells: DEFAULT_MAX_QUERY_GROUP_KEY_CELLS,
            max_group_key_bytes: DEFAULT_MAX_QUERY_GROUP_KEY_BYTES,
            max_aggregate_state_cells: DEFAULT_MAX_QUERY_AGGREGATE_STATE_CELLS,
            max_aggregate_state_bytes: DEFAULT_MAX_QUERY_AGGREGATE_STATE_BYTES,
        }
    }
}

/// Nonpersistent workload limits supplied for one parameterized query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParameterizedQueryLimits {
    pub(crate) max_result_bytes: usize,
    pub(crate) max_result_rows: usize,
    pub(crate) max_result_values: usize,
    pub(crate) max_scan_rows: usize,
    pub(crate) max_groups: usize,
    pub(crate) max_group_key_cells: usize,
    pub(crate) max_group_key_bytes: usize,
    pub(crate) max_ordering_state_bytes: usize,
    pub(crate) max_aggregate_state_cells: usize,
    pub(crate) max_aggregate_state_bytes: usize,
    pub(crate) max_threads: usize,
}

/// A reusable in-memory SQL database.
///
/// Checked `Int64` column-minus-literal expressions, `CAST`, `toString`,
/// `ifNull`, `isNull`, `isNotNull`, `LENGTH`, `lengthUTF8`, `LOWER`, `UPPER`,
/// `ABS`, `ROUND`, `FLOOR`, `CEIL`, and the minimal unpartitioned `ROW_NUMBER`
/// window forms provide bounded projections in ungrouped queries. `ifNull`,
/// `isNull`, `isNotNull`, and the nullable `Int64` identity `CAST` may also
/// derive fixed-size values from physical columns admitted directly or by the
/// exact matching identity `GROUP BY CAST(column AS Int64)` expression.
/// An optional `AS` alias controls each result column name.
///
/// A literal-only query returns one inferred, typed column and one row:
///
/// ```
/// use rusthouse::batch::engine::{Database, ResultColumn, StatementResult};
/// use rusthouse::batch::value::{DataType, Value};
///
/// let mut database = Database::new();
/// let results = database.execute("SELECT 'it''s ready' AS message;")?;
///
/// let [StatementResult::Query(query)] = results.as_slice() else {
///     panic!("the SELECT must produce exactly one query result");
/// };
/// assert_eq!(
///     query.columns,
///     vec![ResultColumn {
///         name: "message".to_owned(),
///         data_type: DataType::String,
///     }],
/// );
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::String("it's ready".to_owned())]],
/// );
/// # Ok::<(), rusthouse::batch::error::Error>(())
/// ```
///
/// # Examples
///
/// ```
/// use rusthouse::batch::engine::{Database, ResultColumn, StatementResult};
/// use rusthouse::batch::value::{DataType, Value};
///
/// let mut database = Database::new();
/// let results = database.execute(
///     "CREATE TABLE readings (value Int64); \
///      INSERT INTO readings VALUES (7), (-2); \
///      SELECT CAST(value AS Float64) AS value_f64 \
///      FROM readings ORDER BY value_f64;",
/// )?;
///
/// let StatementResult::Query(query) = &results[2] else {
///     panic!("the SELECT must produce a query result");
/// };
/// assert_eq!(
///     query.columns,
///     vec![ResultColumn {
///         name: "value_f64".to_owned(),
///         data_type: DataType::Float64,
///     }],
/// );
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::Float64(-2.0)], vec![Value::Float64(7.0)]],
/// );
/// # Ok::<(), rusthouse::batch::error::Error>(())
/// ```
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    measurements: DatabaseMeasurements,
    query_result_limits: QueryResultLimits,
    table_limits: TableLimits,
    global_aggregate_parallelism: GlobalAggregateParallelism,
    index_pruning_counters: IndexPruningCounters,
    #[cfg(unix)]
    int64_write_ahead_log: Option<ActiveInt64WriteAheadLogs>,
}

/// Cumulative sparse-index work performed by indexed scan attempts.
///
/// A scanned block is a block whose rows still pass through the exact
/// predicate evaluator. A pruned block is rejected using metadata alone.
/// Work remains counted if later query processing fails. Queries without an
/// applicable, current index do not change either counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexPruningMetrics {
    pub scanned_blocks: usize,
    pub pruned_blocks: usize,
}

#[derive(Debug, Default)]
struct IndexPruningCounters {
    scanned_blocks: AtomicUsize,
    pruned_blocks: AtomicUsize,
}

impl IndexPruningCounters {
    fn snapshot(&self) -> IndexPruningMetrics {
        IndexPruningMetrics {
            scanned_blocks: self.scanned_blocks.load(AtomicOrdering::Relaxed),
            pruned_blocks: self.pruned_blocks.load(AtomicOrdering::Relaxed),
        }
    }

    fn record(&self, scan: &Int64MinMaxIndexScan) {
        saturating_atomic_add(&self.scanned_blocks, scan.scanned_blocks);
        saturating_atomic_add(&self.pruned_blocks, scan.pruned_blocks);
    }
}

fn saturating_atomic_add(counter: &AtomicUsize, amount: usize) {
    let mut current = counter.load(AtomicOrdering::Relaxed);
    loop {
        let next = current.saturating_add(amount);
        match counter.compare_exchange_weak(
            current,
            next,
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

#[derive(Debug, Default)]
struct DatabaseMeasurements {
    column_count: u128,
    retained_row_count: u128,
    retained_value_bytes: u128,
}

impl DatabaseMeasurements {
    fn add(&mut self, measurements: TableMeasurements) {
        self.column_count = self.column_count.saturating_add(measurements.column_count);
        self.retained_row_count = self
            .retained_row_count
            .saturating_add(measurements.retained_row_count);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_add(measurements.retained_value_bytes);
    }

    fn subtract(&mut self, measurements: TableMeasurements) {
        self.column_count = self.column_count.saturating_sub(measurements.column_count);
        self.retained_row_count = self
            .retained_row_count
            .saturating_sub(measurements.retained_row_count);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_sub(measurements.retained_value_bytes);
    }

    fn replace(&mut self, before: TableMeasurements, after: TableMeasurements) {
        self.subtract(before);
        self.add(after);
    }

    fn add_totals(&mut self, measurements: Self) {
        self.column_count = self.column_count.saturating_add(measurements.column_count);
        self.retained_row_count = self
            .retained_row_count
            .saturating_add(measurements.retained_row_count);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_add(measurements.retained_value_bytes);
    }
}

#[derive(Debug, Clone, Copy)]
struct TableMeasurements {
    column_count: u128,
    retained_row_count: u128,
    retained_value_bytes: u128,
}

impl TableMeasurements {
    fn read(table: &Table) -> Self {
        Self {
            column_count: table.schema().len() as u128,
            retained_row_count: table.row_count() as u128,
            retained_value_bytes: table.retained_value_bytes_exact(),
        }
    }

    fn empty(column_count: usize) -> Self {
        Self {
            column_count: column_count as u128,
            retained_row_count: 0,
            retained_value_bytes: 0,
        }
    }
}

#[derive(Debug)]
struct DatabaseTableMut<'a> {
    table: &'a mut Table,
    measurements: &'a mut DatabaseMeasurements,
    before: TableMeasurements,
}

impl Deref for DatabaseTableMut<'_> {
    type Target = Table;

    fn deref(&self) -> &Self::Target {
        self.table
    }
}

impl DerefMut for DatabaseTableMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.table
    }
}

impl Drop for DatabaseTableMut<'_> {
    fn drop(&mut self) {
        self.measurements
            .replace(self.before, TableMeasurements::read(self.table));
    }
}

fn saturating_usize(value: u128) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(unix)]
const fn query_limits_to_array(limits: QueryResultLimits) -> [usize; 10] {
    [
        limits.max_scan_rows,
        limits.max_rows,
        limits.max_values,
        limits.max_bytes,
        limits.max_ordering_state_bytes,
        limits.max_groups,
        limits.max_group_key_cells,
        limits.max_group_key_bytes,
        limits.max_aggregate_state_cells,
        limits.max_aggregate_state_bytes,
    ]
}

#[cfg(unix)]
const fn query_limits_from_array(limits: [usize; 10]) -> QueryResultLimits {
    QueryResultLimits {
        max_scan_rows: limits[0],
        max_rows: limits[1],
        max_values: limits[2],
        max_bytes: limits[3],
        max_ordering_state_bytes: limits[4],
        max_groups: limits[5],
        max_group_key_cells: limits[6],
        max_group_key_bytes: limits[7],
        max_aggregate_state_cells: limits[8],
        max_aggregate_state_bytes: limits[9],
    }
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::new(),
            measurements: DatabaseMeasurements::default(),
            query_result_limits: QueryResultLimits::default(),
            table_limits: TableLimits::default(),
            global_aggregate_parallelism: GlobalAggregateParallelism::system(
                NonZeroUsize::new(DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP)
                    .expect("the default aggregate worker cap is nonzero"),
            ),
            index_pruning_counters: IndexPruningCounters::default(),
            #[cfg(unix)]
            int64_write_ahead_log: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    Command {
        tag: &'static str,
        affected_rows: usize,
    },
    Query(QueryResult),
}

/// A failure while reopening one self-describing `Int64` snapshot in a
/// [`Database`].
#[derive(Debug)]
pub enum DatabaseSnapshotRestoreError {
    /// Opening or decoding the bounded snapshot failed.
    Snapshot(Int64TablePayloadFileRestoreError),
    /// Both the primary and explicit backup snapshots failed bounded restore.
    Recovery(Int64TablePayloadFileRecoveryError),
    /// Legacy nullable-column rejection retained for source compatibility.
    ///
    /// Self-describing snapshot restore and replacement APIs now accept
    /// nullable `Int64` columns, so they no longer return this variant.
    NullableColumn { column: String },
    /// The caller name, decoded schema, duplicate name, or configured table
    /// limits were rejected by batch storage.
    Table(Error),
}

/// A failure while importing one row-only RLE `Int64` snapshot into a
/// [`Database`].
#[derive(Debug)]
pub enum DatabaseRleSnapshotRestoreError {
    /// Opening or decoding the bounded RLE snapshot failed, or its rows did
    /// not satisfy the caller-supplied schema and row cap.
    Snapshot(Int64TableRleFileRestoreError),
    /// Legacy nullable-column rejection retained for source compatibility.
    ///
    /// RLE snapshot restore now accepts nullable `Int64` columns, so this
    /// variant is no longer returned.
    NullableColumn { column: String },
    /// The caller name, schema, duplicate name, or configured table limits
    /// were rejected by batch storage.
    Table(Error),
}

/// One caller-named self-describing `Int64` or `Nullable(Int64)` snapshot in
/// an atomic database restore set.
#[derive(Debug, Clone, Copy)]
pub struct DatabaseSnapshotRestoreEntry<'a> {
    table_name: &'a str,
    path: &'a Path,
    snapshot_codec: SnapshotCodec,
    payload_codec: Int64TablePayloadCodec,
}

impl<'a> DatabaseSnapshotRestoreEntry<'a> {
    /// Describes one bounded snapshot file and its destination table name.
    pub fn new<P: AsRef<Path> + ?Sized>(
        table_name: &'a str,
        path: &'a P,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> Self {
        Self {
            table_name,
            path: path.as_ref(),
            snapshot_codec,
            payload_codec,
        }
    }

    /// Returns the caller-supplied database table name.
    #[must_use]
    pub const fn table_name(self) -> &'a str {
        self.table_name
    }

    /// Returns the snapshot source path.
    #[must_use]
    pub const fn path(self) -> &'a Path {
        self.path
    }

    /// Returns the envelope codec, including its payload-byte bound.
    #[must_use]
    pub const fn snapshot_codec(self) -> SnapshotCodec {
        self.snapshot_codec
    }

    /// Returns the self-describing table codec and its schema, row, and byte bounds.
    #[must_use]
    pub const fn payload_codec(self) -> Int64TablePayloadCodec {
        self.payload_codec
    }
}

/// A failure while atomically restoring a bounded set of `Int64` or
/// `Nullable(Int64)` snapshots.
#[derive(Debug)]
pub enum DatabaseSnapshotSetRestoreError {
    /// The input contains more entries than the caller-authorized inclusive limit.
    EntryLimitExceeded {
        /// Zero-based index of the first entry beyond the limit.
        entry_index: usize,
        /// Caller-supplied name of the first entry beyond the limit.
        table_name: String,
        /// Complete number of supplied entries.
        entries: usize,
        /// Maximum number of entries authorized by the caller.
        max_entries: usize,
    },
    /// One identified entry failed name, file, schema, or table validation.
    Entry {
        /// Zero-based entry index in caller order.
        entry_index: usize,
        /// Caller-supplied destination table name.
        table_name: String,
        /// Typed single-snapshot failure.
        error: DatabaseSnapshotRestoreError,
    },
}

impl DatabaseSnapshotSetRestoreError {
    /// Returns the zero-based index of the rejected entry.
    #[must_use]
    pub const fn entry_index(&self) -> usize {
        match self {
            Self::EntryLimitExceeded { entry_index, .. } | Self::Entry { entry_index, .. } => {
                *entry_index
            }
        }
    }

    /// Returns the caller-supplied table name of the rejected entry.
    #[must_use]
    pub fn table_name(&self) -> &str {
        match self {
            Self::EntryLimitExceeded { table_name, .. } | Self::Entry { table_name, .. } => {
                table_name
            }
        }
    }

    /// Returns the typed per-entry failure, or `None` for a set count failure.
    #[must_use]
    pub const fn entry_error(&self) -> Option<&DatabaseSnapshotRestoreError> {
        match self {
            Self::Entry { error, .. } => Some(error),
            Self::EntryLimitExceeded { .. } => None,
        }
    }
}

impl fmt::Display for DatabaseSnapshotRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(formatter, "could not restore snapshot: {error}"),
            Self::Recovery(error) => write!(formatter, "could not recover snapshot: {error}"),
            Self::NullableColumn { column } => {
                write!(formatter, "snapshot column '{column}' is nullable")
            }
            Self::Table(error) => error.fmt(formatter),
        }
    }
}

impl fmt::Display for DatabaseRleSnapshotRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(formatter, "could not restore RLE snapshot: {error}"),
            Self::NullableColumn { column } => {
                write!(formatter, "snapshot column '{column}' is nullable")
            }
            Self::Table(error) => error.fmt(formatter),
        }
    }
}

impl fmt::Display for DatabaseSnapshotSetRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntryLimitExceeded {
                entry_index,
                table_name,
                entries,
                max_entries,
            } => write!(
                formatter,
                "snapshot restore entry {entry_index} ('{table_name}') exceeds the caller limit: supplied {entries} entries, maximum {max_entries}"
            ),
            Self::Entry {
                entry_index,
                table_name,
                error,
            } => write!(
                formatter,
                "could not restore snapshot entry {entry_index} ('{table_name}'): {error}"
            ),
        }
    }
}

/// A failure while atomically saving one batch table as a self-describing
/// nullable or non-nullable `Int64` snapshot.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseSnapshotSaveError {
    /// The requested table was not present in the database.
    Table(Error),
    /// The selected table does not have exactly one physical column.
    UnsupportedColumnCount {
        /// The stored display name of the selected table.
        table: String,
        /// The number of physical columns in the selected table.
        column_count: usize,
    },
    /// The selected table's only physical column is not `Int64`.
    UnsupportedColumnType {
        /// The stored display name of the unsupported column.
        column: String,
        /// The physical type found in the batch table.
        data_type: DataType,
    },
    /// Legacy nullable-column rejection retained for source compatibility.
    ///
    /// Nullable `Int64` columns are now supported, so
    /// [`Database::save_int64_table_to_file`] no longer returns this variant.
    NullableColumn {
        /// The stored display name of the nullable column.
        column: String,
    },
    /// Encoding or atomically replacing the snapshot failed.
    Snapshot(Int64TablePayloadFileSaveError),
}

/// A failure while atomically saving one batch table as a row-only,
/// RLE-compressed nullable or non-nullable `Int64` snapshot.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseRleSnapshotSaveError {
    /// The requested table was not present in the database.
    Table(Error),
    /// The selected table does not have exactly one physical column.
    UnsupportedColumnCount {
        /// The stored display name of the selected table.
        table: String,
        /// The number of physical columns in the selected table.
        column_count: usize,
    },
    /// The selected table's only physical column is not `Int64` or
    /// `Nullable(Int64)`.
    UnsupportedColumnType {
        /// The stored display name of the unsupported column.
        column: String,
        /// The physical type found in the batch table.
        data_type: DataType,
    },
    /// RLE encoding or atomically replacing the snapshot failed.
    Snapshot(Int64TableRleFileSaveError),
}

/// A failure while opting one existing one-column `Int64` table into a new WAL.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseInt64WalEnableError {
    /// This database already has a table attached to a WAL.
    AlreadyEnabled,
    /// Resolving the requested table failed.
    Table(Error),
    /// The selected table does not have exactly one physical column.
    UnsupportedColumnCount { table: String, column_count: usize },
    /// The selected table's only physical column is not `Int64`.
    UnsupportedColumnType { column: String, data_type: DataType },
    /// Encoding, creating, writing, or synchronizing the WAL failed.
    WriteAheadLog(Int64WriteAheadLogError),
}

/// A failure while replaying an `Int64` WAL into a fresh database.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseInt64WalRecoveryError {
    /// Reading, bounding, framing, or replaying the WAL failed.
    WriteAheadLog(Int64WriteAheadLogError),
    /// Reconstructed catalog metadata or table limits were invalid.
    Table(Error),
}

/// A failure while atomically attaching a bounded multi-table WAL registry.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseInt64WalRegistryEnableError {
    /// This database already has a single-table WAL or registry attached.
    AlreadyEnabled,
    /// Resolving one requested table failed.
    Table {
        /// Caller-supplied table name that failed resolution.
        table: String,
        /// Typed catalog or table failure.
        error: Error,
    },
    /// A requested table does not have exactly one physical column.
    UnsupportedColumnCount {
        /// Stored display name of the unsupported table.
        table: String,
        /// Number of physical columns found in the table.
        column_count: usize,
    },
    /// A requested table's only physical column is not `Int64`.
    UnsupportedColumnType {
        /// Stored display name of the unsupported column.
        column: String,
        /// Physical type found in the batch table.
        data_type: DataType,
    },
    /// Bounding, creating, writing, or synchronizing the registry failed.
    Registry(Int64WriteAheadLogRegistryError),
}

/// A failure while staging every registry member into a fresh database.
#[cfg(unix)]
#[derive(Debug)]
pub enum DatabaseInt64WalRegistryRecoveryError {
    /// Opening, bounding, validating, or replaying the registry failed.
    Registry(Int64WriteAheadLogRegistryError),
    /// A replayed member could not be staged as a valid batch table.
    Table {
        /// Member table display name, or its registry index if publication
        /// failed after staging.
        table: String,
        /// Typed catalog or table validation failure.
        error: Error,
    },
}

#[cfg(unix)]
impl fmt::Display for DatabaseInt64WalRegistryEnableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyEnabled => {
                formatter.write_str("an Int64 WAL is already enabled for this database")
            }
            Self::Table { table, error } => write!(
                formatter,
                "could not attach registry table '{table}': {error}"
            ),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; Int64 WAL registry requires exactly one"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; Int64 WAL registry requires Int64 or Nullable(Int64)"
            ),
            Self::Registry(error) => error.fmt(formatter),
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseInt64WalRegistryEnableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table { error, .. } => Some(error),
            Self::Registry(error) => Some(error),
            Self::AlreadyEnabled
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. } => None,
        }
    }
}

#[cfg(unix)]
impl fmt::Display for DatabaseInt64WalRegistryRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => {
                write!(formatter, "could not replay Int64 WAL registry: {error}")
            }
            Self::Table { table, error } => write!(
                formatter,
                "could not reconstruct Int64 WAL registry table '{table}': {error}"
            ),
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseInt64WalRegistryRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Table { error, .. } => Some(error),
        }
    }
}

#[cfg(unix)]
impl From<Int64WriteAheadLogRegistryError> for DatabaseInt64WalRegistryEnableError {
    fn from(error: Int64WriteAheadLogRegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(unix)]
impl From<Int64WriteAheadLogRegistryError> for DatabaseInt64WalRegistryRecoveryError {
    fn from(error: Int64WriteAheadLogRegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(unix)]
impl fmt::Display for DatabaseInt64WalEnableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyEnabled => {
                formatter.write_str("an Int64 WAL is already enabled for this database")
            }
            Self::Table(error) => error.fmt(formatter),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; Int64 WAL requires exactly one"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; Int64 WAL requires Int64"
            ),
            Self::WriteAheadLog(error) => error.fmt(formatter),
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseInt64WalEnableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::WriteAheadLog(error) => Some(error),
            Self::AlreadyEnabled
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. } => None,
        }
    }
}

#[cfg(unix)]
impl fmt::Display for DatabaseInt64WalRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WriteAheadLog(error) => write!(formatter, "could not replay Int64 WAL: {error}"),
            Self::Table(error) => {
                write!(formatter, "could not reconstruct Int64 WAL table: {error}")
            }
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseInt64WalRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WriteAheadLog(error) => Some(error),
            Self::Table(error) => Some(error),
        }
    }
}

#[cfg(unix)]
impl From<Int64WriteAheadLogError> for DatabaseInt64WalEnableError {
    fn from(error: Int64WriteAheadLogError) -> Self {
        Self::WriteAheadLog(error)
    }
}

#[cfg(unix)]
impl From<Int64WriteAheadLogError> for DatabaseInt64WalRecoveryError {
    fn from(error: Int64WriteAheadLogError) -> Self {
        Self::WriteAheadLog(error)
    }
}

#[cfg(unix)]
impl From<Error> for DatabaseInt64WalRecoveryError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

#[cfg(unix)]
impl DatabaseSnapshotSaveError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// Shape validation, payload encoding, and every replacement failure before
    /// the rename return `false`. Only post-rename directory-sync uncertainty
    /// returns `true`.
    pub const fn destination_was_replaced(&self) -> bool {
        match self {
            Self::Snapshot(error) => error.destination_was_replaced(),
            Self::Table(_)
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. }
            | Self::NullableColumn { .. } => false,
        }
    }
}

#[cfg(unix)]
impl DatabaseRleSnapshotSaveError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// Table validation, RLE encoding, and every replacement failure before
    /// the rename return `false`. Only post-rename directory-sync uncertainty
    /// returns `true`.
    pub const fn destination_was_replaced(&self) -> bool {
        match self {
            Self::Snapshot(error) => error.destination_was_replaced(),
            Self::Table(_)
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. } => false,
        }
    }
}

#[cfg(unix)]
impl fmt::Display for DatabaseSnapshotSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table(error) => error.fmt(formatter),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; batch snapshot save requires exactly one Int64 column"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; batch snapshot save requires exactly one Int64 column"
            ),
            Self::NullableColumn { column } => write!(
                formatter,
                "column '{column}' is nullable; batch snapshot save requires exactly one non-nullable Int64 column"
            ),
            Self::Snapshot(error) => write!(formatter, "could not save snapshot: {error}"),
        }
    }
}

#[cfg(unix)]
impl fmt::Display for DatabaseRleSnapshotSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table(error) => error.fmt(formatter),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; batch RLE snapshot save requires exactly one Int64 or Nullable(Int64) column"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; batch RLE snapshot save requires Int64 or Nullable(Int64)"
            ),
            Self::Snapshot(error) => write!(formatter, "could not save RLE snapshot: {error}"),
        }
    }
}

impl std::error::Error for DatabaseSnapshotRestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Recovery(error) => Some(error),
            Self::Table(error) => Some(error),
            Self::NullableColumn { .. } => None,
        }
    }
}

impl std::error::Error for DatabaseRleSnapshotRestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Table(error) => Some(error),
            Self::NullableColumn { .. } => None,
        }
    }
}

impl std::error::Error for DatabaseSnapshotSetRestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Entry { error, .. } => Some(error),
            Self::EntryLimitExceeded { .. } => None,
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseSnapshotSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. }
            | Self::NullableColumn { .. } => None,
        }
    }
}

#[cfg(unix)]
impl std::error::Error for DatabaseRleSnapshotSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::UnsupportedColumnCount { .. } | Self::UnsupportedColumnType { .. } => None,
        }
    }
}

impl From<Int64TablePayloadFileRestoreError> for DatabaseSnapshotRestoreError {
    fn from(error: Int64TablePayloadFileRestoreError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<Int64TableRleFileRestoreError> for DatabaseRleSnapshotRestoreError {
    fn from(error: Int64TableRleFileRestoreError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<Int64TablePayloadFileRecoveryError> for DatabaseSnapshotRestoreError {
    fn from(error: Int64TablePayloadFileRecoveryError) -> Self {
        Self::Recovery(error)
    }
}

impl From<Error> for DatabaseSnapshotRestoreError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

impl From<Error> for DatabaseRleSnapshotRestoreError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

#[cfg(unix)]
impl From<Error> for DatabaseSnapshotSaveError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

#[cfg(unix)]
impl From<Int64TablePayloadFileSaveError> for DatabaseSnapshotSaveError {
    fn from(error: Int64TablePayloadFileSaveError) -> Self {
        Self::Snapshot(error)
    }
}

#[cfg(unix)]
impl From<Error> for DatabaseRleSnapshotSaveError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

#[cfg(unix)]
impl From<Int64TableRleFileSaveError> for DatabaseRleSnapshotSaveError {
    fn from(error: Int64TableRleFileSaveError) -> Self {
        Self::Snapshot(error)
    }
}

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty database with explicit per-query resource limits.
    pub fn with_query_result_limits(query_result_limits: QueryResultLimits) -> Self {
        Self {
            catalog: Catalog::new(),
            measurements: DatabaseMeasurements::default(),
            query_result_limits,
            table_limits: TableLimits::default(),
            global_aggregate_parallelism: GlobalAggregateParallelism::system(
                NonZeroUsize::new(DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP)
                    .expect("the default aggregate worker cap is nonzero"),
            ),
            index_pruning_counters: IndexPruningCounters::default(),
            #[cfg(unix)]
            int64_write_ahead_log: None,
        }
    }

    /// Creates an empty database with an explicit nonzero computation-lane cap
    /// for supported parallel aggregates, including sole nullable `Int64`
    /// `COUNT`, Bool-grouped row count, and Bool-grouped nullable `Int64`
    /// `COUNT`, plus sole non-nullable Int64 `SUM` grouped by Bool.
    ///
    /// A cap of one keeps those aggregates sequential. Higher caps remain
    /// subject to the process-wide worker budget, available hardware, and the
    /// fixed [`MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS`] ceiling.
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use rusthouse::Database;
    ///
    /// let cap = NonZeroUsize::new(2).unwrap();
    /// let database = Database::with_global_aggregate_worker_cap(cap);
    /// assert_eq!(database.global_aggregate_worker_cap(), cap);
    /// ```
    #[must_use]
    pub fn with_global_aggregate_worker_cap(global_aggregate_worker_cap: NonZeroUsize) -> Self {
        Self {
            global_aggregate_parallelism: GlobalAggregateParallelism::system(
                global_aggregate_worker_cap,
            ),
            ..Self::default()
        }
    }

    /// Creates an empty database with an explicit row cap and default column and cell caps.
    pub fn with_max_rows_per_table(max_rows_per_table: usize) -> Self {
        Self {
            table_limits: TableLimits {
                max_rows: max_rows_per_table,
                ..TableLimits::default()
            },
            ..Self::default()
        }
    }

    /// Creates an empty database with explicit persistent limits for each table.
    pub fn with_table_limits(table_limits: TableLimits) -> Self {
        Self {
            table_limits,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Attempts to install the database's one optional sparse `Int64` min/max index.
    ///
    /// Names resolve case-insensitively. Unknown tables/columns and non-Int64
    /// columns are errors. Capacity and slot pressure are normal admission
    /// rejections: they leave the target and any existing index unchanged, and
    /// subsequent queries continue on the exact unindexed path.
    pub fn create_int64_min_max_index(
        &mut self,
        table: &str,
        column: &str,
        limits: Int64MinMaxIndexLimits,
    ) -> Result<Int64MinMaxIndexAdmission> {
        {
            let target = self.catalog.table(table)?;
            let column_index = target.column_index(column)?;
            if target.schema()[column_index].data_type != DataType::Int64 {
                return Err(Error::TypeMismatch {
                    context: format!(
                        "sparse min-max index column '{}.{}'",
                        target.name(),
                        target.schema()[column_index].name
                    ),
                    expected: DataType::Int64.to_string(),
                    actual: target.schema()[column_index].data_type.to_string(),
                });
            }
        }
        if let Some(owner) = self.catalog.int64_min_max_index_owner() {
            if !owner.eq_ignore_ascii_case(table) {
                return Ok(Int64MinMaxIndexAdmission::Rejected(
                    Int64MinMaxIndexRejection::SlotOccupied {
                        table: owner.to_owned(),
                    },
                ));
            }
        }
        self.table_mut(table)?
            .try_create_int64_min_max_index(column, limits)
    }

    /// Removes a table's sparse index, returning whether one was present.
    pub fn drop_int64_min_max_index(&mut self, table: &str) -> Result<bool> {
        Ok(self.table_mut(table)?.drop_int64_min_max_index())
    }

    /// Returns cumulative scanned/pruned block counters without resetting them.
    #[must_use]
    pub fn index_pruning_metrics(&self) -> IndexPruningMetrics {
        self.index_pruning_counters.snapshot()
    }

    pub(crate) fn retained_metrics(&self) -> (usize, usize, usize, usize) {
        (
            self.catalog.table_count(),
            saturating_usize(self.measurements.column_count),
            saturating_usize(self.measurements.retained_row_count),
            saturating_usize(self.measurements.retained_value_bytes),
        )
    }

    pub(crate) fn table_metrics(&self) -> Vec<(String, usize, usize)> {
        self.catalog.table_metrics()
    }

    pub(crate) fn table_metric_variable_bytes(&self) -> (usize, usize, usize) {
        self.catalog.table_metric_variable_bytes()
    }

    fn table_mut(&mut self, name: &str) -> Result<DatabaseTableMut<'_>> {
        let Self {
            catalog,
            measurements,
            ..
        } = self;
        let table = catalog.table_mut(name)?;
        let before = TableMeasurements::read(table);
        Ok(DatabaseTableMut {
            table,
            measurements,
            before,
        })
    }

    #[cfg(unix)]
    fn wal_tracks(&self, table_name: &str) -> bool {
        self.int64_write_ahead_log
            .as_ref()
            .is_some_and(|write_ahead_log| write_ahead_log.tracks(table_name))
    }

    #[cfg(not(unix))]
    const fn wal_tracks(&self, _table_name: &str) -> bool {
        false
    }

    #[cfg(unix)]
    fn log_int64_append(&mut self, table_name: &str, values: &[Option<i64>]) -> Result<()> {
        if let Some(write_ahead_log) = self
            .int64_write_ahead_log
            .as_mut()
            .filter(|write_ahead_logs| write_ahead_logs.tracks(table_name))
        {
            write_ahead_log
                .append_values(table_name, values)
                .map_err(Error::WriteAheadLog)?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn log_int64_append(&mut self, _table_name: &str, _values: &[Option<i64>]) -> Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn log_int64_truncate(&mut self, table_name: &str) -> Result<()> {
        if let Some(write_ahead_log) = self
            .int64_write_ahead_log
            .as_mut()
            .filter(|write_ahead_logs| write_ahead_logs.tracks(table_name))
        {
            write_ahead_log
                .truncate(table_name)
                .map_err(Error::WriteAheadLog)?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn log_int64_truncate(&mut self, _table_name: &str) -> Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn log_int64_replacements(
        &mut self,
        table_name: &str,
        replacements: &[(usize, Option<i64>)],
    ) -> Result<()> {
        if let Some(write_ahead_log) = self
            .int64_write_ahead_log
            .as_mut()
            .filter(|write_ahead_logs| write_ahead_logs.tracks(table_name))
        {
            write_ahead_log
                .replace_values(table_name, replacements)
                .map_err(Error::WriteAheadLog)?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn log_int64_replacements(
        &mut self,
        _table_name: &str,
        _replacements: &[(usize, Option<i64>)],
    ) -> Result<()> {
        Ok(())
    }

    fn reject_unlogged_wal_mutation(&self, table_name: &str, mutation: &str) -> Result<()> {
        if self.wal_tracks(table_name) {
            return Err(Error::InvalidQuery(format!(
                "{mutation} is not supported while table '{table_name}' has an active Int64 WAL"
            )));
        }
        Ok(())
    }

    fn log_prepared_int64_append(
        &mut self,
        table_name: &str,
        rows: &PreparedInsertRows,
    ) -> Result<()> {
        if !self.wal_tracks(table_name) {
            return Ok(());
        }
        let values = rows
            .int64_values()
            .expect("WAL opt-in guarantees one preflighted Int64 column");
        self.log_int64_append(table_name, &values)
    }

    #[must_use]
    pub const fn query_result_limits(&self) -> QueryResultLimits {
        self.query_result_limits
    }

    /// Returns the maximum number of rows retained by each created table.
    #[must_use]
    pub const fn max_rows_per_table(&self) -> usize {
        self.table_limits.max_rows
    }

    /// Returns the persistent resource limits applied to each created table.
    #[must_use]
    pub const fn table_limits(&self) -> TableLimits {
        self.table_limits
    }

    /// Atomically creates one programmatic `Nullable(Int64)` table.
    pub fn create_nullable_int64_table(
        &mut self,
        table_name: impl Into<String>,
        column_name: impl Into<String>,
        values: Vec<Option<i64>>,
    ) -> Result<()> {
        self.create_nullable_int64_table_with_limits(
            table_name,
            column_name,
            values,
            self.table_limits,
        )
    }

    /// Atomically creates one bounded programmatic `Nullable(Int64)` table.
    pub fn create_nullable_int64_table_with_limits(
        &mut self,
        table_name: impl Into<String>,
        column_name: impl Into<String>,
        values: Vec<Option<i64>>,
        limits: TableLimits,
    ) -> Result<()> {
        let table = Table::with_nullable_int64_values(
            table_name.into(),
            column_name.into(),
            values,
            limits,
        )?;
        let measurements = TableMeasurements::read(&table);
        self.catalog.register_table(table)?;
        self.measurements.add(measurements);
        Ok(())
    }

    /// Stages a bounded mixed table completely before publishing it in the catalog.
    fn create_table_with_trailing_nullable_int64(
        &mut self,
        name: String,
        columns: Vec<ColumnDef>,
        nullable_columns: impl IntoIterator<Item = String>,
        if_not_exists: bool,
    ) -> Result<()> {
        if self.catalog.table_exists(&name) {
            return if if_not_exists {
                Ok(())
            } else {
                Err(Error::TableAlreadyExists(name))
            };
        }

        let mut nullable_columns = nullable_columns.into_iter();
        let mut table = if columns.is_empty() {
            let first = nullable_columns.next().ok_or_else(|| {
                Error::InvalidQuery("a table must contain at least one column".to_owned())
            })?;
            let second = nullable_columns.next().ok_or_else(|| {
                Error::InvalidQuery("a table must contain at least one column".to_owned())
            })?;
            let mut table =
                Table::with_nullable_int64_values(name, first, Vec::new(), self.table_limits)?;
            table.add_nullable_int64_column(second)?;
            table
        } else {
            Table::with_limits(name, columns, self.table_limits)?
        };
        for nullable_column in nullable_columns {
            table.add_nullable_int64_column(nullable_column)?;
        }
        let measurements = TableMeasurements::read(&table);
        self.catalog.register_table(table)?;
        self.measurements.add(measurements);
        Ok(())
    }

    /// Appends validated nullable values, committing an attached table WAL
    /// before publishing any row in memory.
    pub fn append_nullable_int64_values(
        &mut self,
        table_name: &str,
        values: &[Option<i64>],
    ) -> Result<()> {
        {
            let table = self.catalog.table(table_name)?;
            if !matches!(table.columns(), [Column::NullableInt64(_)]) {
                return Err(Error::TypeMismatch {
                    context: format!("nullable Int64 append table '{}'", table.name()),
                    expected: "Nullable(Int64)".to_owned(),
                    actual: table.schema()[0].data_type.to_string(),
                });
            }
            table.validate_row_capacity(values.len())?;
        }
        let rows = values
            .iter()
            .map(|value| vec![value.map_or(Value::Null(DataType::Int64), Value::Int64)])
            .collect::<Vec<_>>();
        self.log_int64_append(table_name, values)?;
        self.table_mut(table_name)?.append_validated_rows(rows);
        Ok(())
    }

    /// Replaces a strictly increasing selection in a nullable table, including
    /// transitions to and from `NULL`, after durably logging the full change.
    pub fn replace_nullable_int64_values(
        &mut self,
        table_name: &str,
        replacements: &[(usize, Option<i64>)],
    ) -> Result<usize> {
        let (column_name, row_count) = {
            let table = self.catalog.table(table_name)?;
            if !matches!(table.columns(), [Column::NullableInt64(_)]) {
                return Err(Error::TypeMismatch {
                    context: format!("nullable Int64 replacement table '{}'", table.name()),
                    expected: "Nullable(Int64)".to_owned(),
                    actual: table.schema()[0].data_type.to_string(),
                });
            }
            (table.schema()[0].name.clone(), table.row_count())
        };
        validate_row_selection(replacements.iter().map(|(row, _)| *row), row_count)?;
        self.log_int64_replacements(table_name, replacements)?;
        self.table_mut(table_name)?.replace_column_values(
            &column_name,
            replacements
                .iter()
                .map(|(row, value)| {
                    (
                        *row,
                        value.map_or(Value::Null(DataType::Int64), Value::Int64),
                    )
                })
                .collect(),
        )
    }

    /// Creates and synchronizes a bounded WAL for one existing one-column
    /// `Int64` or nullable `Int64` table, then logs its successful appends,
    /// truncates, and atomic value replacements before publishing them in
    /// memory.
    ///
    /// Opt-in writes a bootstrap containing the table display name, column
    /// name, rows, table/database caps, query byte and row caps, and aggregate
    /// worker cap. The destination basename is created exclusively relative to
    /// one opened parent descriptor. The bootstrap body and commit footer are
    /// synchronized in that order, then the same parent descriptor is
    /// synchronized before this method succeeds.
    #[cfg(unix)]
    pub fn enable_int64_write_ahead_log(
        &mut self,
        table_name: &str,
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogLimits,
    ) -> std::result::Result<(), DatabaseInt64WalEnableError> {
        if self.int64_write_ahead_log.is_some() {
            return Err(DatabaseInt64WalEnableError::AlreadyEnabled);
        }
        let table = self
            .catalog
            .table(table_name)
            .map_err(DatabaseInt64WalEnableError::Table)?;
        if table.schema().len() != 1 {
            return Err(DatabaseInt64WalEnableError::UnsupportedColumnCount {
                table: table.name().to_owned(),
                column_count: table.schema().len(),
            });
        }
        let column = &table.schema()[0];
        if column.data_type != DataType::Int64 {
            return Err(DatabaseInt64WalEnableError::UnsupportedColumnType {
                column: column.name.clone(),
                data_type: column.data_type,
            });
        }
        let (nullable, rows) = match &table.columns()[0] {
            Column::Int64(values) => (false, values.len()),
            Column::NullableInt64(values) => (true, values.len()),
            _ => unreachable!("batch table schema and physical storage must agree"),
        };
        Int64WriteAheadLog::validate_bootstrap_limits(
            table.name().len(),
            column.name.len(),
            rows,
            nullable,
            limits,
        )?;
        let values = match &table.columns()[0] {
            Column::Int64(values) => values.iter().copied().map(Some).collect(),
            Column::NullableInt64(values) => values.clone(),
            _ => unreachable!("batch table shape was preflighted"),
        };
        let table_limits = table.limits();
        let database_table_limits = self.table_limits;
        let query = self.query_result_limits;
        let bootstrap = Int64WalBootstrap {
            table_name: table.name().to_owned(),
            column_name: column.name.clone(),
            table_limits: [
                table_limits.max_rows,
                table_limits.max_columns,
                table_limits.max_cells,
            ],
            database_table_limits: [
                database_table_limits.max_rows,
                database_table_limits.max_columns,
                database_table_limits.max_cells,
            ],
            query_limits: query_limits_to_array(query),
            worker_cap: self.global_aggregate_parallelism.worker_cap().get(),
            nullable,
            values,
        };
        let write_ahead_log = Int64WriteAheadLog::create(path.as_ref(), &bootstrap, limits)?;
        self.int64_write_ahead_log = Some(ActiveInt64WriteAheadLogs::single(write_ahead_log));
        Ok(())
    }

    /// Creates an exclusive, checksummed WAL directory for multiple existing
    /// one-column `Int64` or nullable `Int64` tables. Member files and the new
    /// directory are synchronized before the manifest is durably published.
    /// Table count, manifest bytes, aggregate member bytes and records, and
    /// per-member limits are checked before the registry becomes active.
    ///
    /// Each member remains an independent log: a mutation spanning multiple
    /// logged tables is rejected rather than committed as one transaction.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use rusthouse::{Database, Int64WriteAheadLogRegistryLimits};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let path = Path::new("analytics-registry"); // must not already exist
    /// let mut database = Database::new();
    /// database.execute("CREATE TABLE events (value Int64)")?;
    /// database.create_nullable_int64_table(
    ///     "optional_events",
    ///     "value",
    ///     vec![Some(1), None],
    /// )?;
    ///
    /// let limits = Int64WriteAheadLogRegistryLimits::default();
    /// database.enable_int64_write_ahead_log_registry(
    ///     &["events", "optional_events"],
    ///     path,
    ///     limits,
    /// )?;
    /// database.execute("INSERT INTO events VALUES (2)")?;
    /// database.append_nullable_int64_values("optional_events", &[Some(3), None])?;
    /// assert!(database.disable_int64_write_ahead_log());
    ///
    /// let mut recovered = Database::recover_int64_write_ahead_log_registry(path, limits)?;
    /// recovered.execute("SELECT COUNT(*) FROM events")?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(unix)]
    pub fn enable_int64_write_ahead_log_registry<S: AsRef<str>>(
        &mut self,
        table_names: &[S],
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogRegistryLimits,
    ) -> std::result::Result<(), DatabaseInt64WalRegistryEnableError> {
        if self.int64_write_ahead_log.is_some() {
            return Err(DatabaseInt64WalRegistryEnableError::AlreadyEnabled);
        }
        wal::validate_registry_table_count(table_names.len(), limits)?;
        let mut preflight = Vec::with_capacity(table_names.len());
        for requested in table_names {
            let requested = requested.as_ref();
            let table = self.catalog.table(requested).map_err(|error| {
                DatabaseInt64WalRegistryEnableError::Table {
                    table: requested.to_owned(),
                    error,
                }
            })?;
            if table.schema().len() != 1 {
                return Err(
                    DatabaseInt64WalRegistryEnableError::UnsupportedColumnCount {
                        table: table.name().to_owned(),
                        column_count: table.schema().len(),
                    },
                );
            }
            let column = &table.schema()[0];
            if column.data_type != DataType::Int64 {
                return Err(DatabaseInt64WalRegistryEnableError::UnsupportedColumnType {
                    column: column.name.clone(),
                    data_type: column.data_type,
                });
            }
            let (nullable, rows) = match &table.columns()[0] {
                Column::Int64(values) => (false, values.len()),
                Column::NullableInt64(values) => (true, values.len()),
                _ => unreachable!("batch table schema and physical storage must agree"),
            };
            preflight.push(wal::Int64WalRegistryTablePreflight {
                table_name: table.name(),
                column_name: &column.name,
                rows,
                nullable,
            });
        }
        wal::preflight_registry_tables(&preflight, limits)?;
        drop(preflight);

        let mut bootstraps = Vec::with_capacity(table_names.len());
        for requested in table_names {
            let table = self.catalog.table(requested.as_ref()).map_err(|error| {
                DatabaseInt64WalRegistryEnableError::Table {
                    table: requested.as_ref().to_owned(),
                    error,
                }
            })?;
            let column = &table.schema()[0];
            let (nullable, values) = match &table.columns()[0] {
                Column::Int64(values) => (false, values.iter().copied().map(Some).collect()),
                Column::NullableInt64(values) => (true, values.clone()),
                _ => unreachable!("registry table shape was preflighted"),
            };
            let table_limits = table.limits();
            let database_table_limits = self.table_limits;
            bootstraps.push(Int64WalBootstrap {
                table_name: table.name().to_owned(),
                column_name: column.name.clone(),
                table_limits: [
                    table_limits.max_rows,
                    table_limits.max_columns,
                    table_limits.max_cells,
                ],
                database_table_limits: [
                    database_table_limits.max_rows,
                    database_table_limits.max_columns,
                    database_table_limits.max_cells,
                ],
                query_limits: query_limits_to_array(self.query_result_limits),
                worker_cap: self.global_aggregate_parallelism.worker_cap().get(),
                nullable,
                values,
            });
        }
        let registry =
            ActiveInt64WriteAheadLogs::create_registry(path.as_ref(), bootstraps, limits)?;
        self.int64_write_ahead_log = Some(registry);
        Ok(())
    }

    /// Alias using the plural form for callers treating the registry as a set
    /// of independent table logs.
    #[cfg(unix)]
    pub fn enable_int64_write_ahead_logs<S: AsRef<str>>(
        &mut self,
        table_names: &[S],
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogRegistryLimits,
    ) -> std::result::Result<(), DatabaseInt64WalRegistryEnableError> {
        self.enable_int64_write_ahead_log_registry(table_names, path, limits)
    }

    /// Replays the complete committed prefix of one bounded WAL into a fresh
    /// database. A partial final header, payload, or commit footer is ignored.
    /// Complete-record corruption and every resource failure return a typed
    /// error without exposing a partially reconstructed database.
    ///
    /// Recovery is read-only and does not attach a writer to the returned
    /// database. It is therefore idempotent and safe to repeat. To resume
    /// durable writes or compact the history, enable a new WAL at a new path
    /// after successful recovery. The recovered table retains its stored
    /// nullability.
    #[cfg(unix)]
    pub fn recover_int64_write_ahead_log(
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogLimits,
    ) -> std::result::Result<Self, DatabaseInt64WalRecoveryError> {
        let recovered = wal::recover(path.as_ref(), limits)?;
        let Int64WalBootstrap {
            table_name,
            column_name,
            table_limits,
            database_table_limits,
            query_limits,
            worker_cap,
            nullable,
            values,
        } = recovered.bootstrap;
        let table_limits = TableLimits::new(table_limits[0], table_limits[1], table_limits[2]);
        let database_table_limits = TableLimits::new(
            database_table_limits[0],
            database_table_limits[1],
            database_table_limits[2],
        );
        let table = if nullable {
            Table::with_nullable_int64_values(table_name, column_name, values, table_limits)?
        } else {
            Table::with_int64_values(
                table_name,
                column_name,
                values
                    .into_iter()
                    .map(|value| value.expect("non-nullable WAL replay contains no NULL"))
                    .collect(),
                table_limits,
            )?
        };
        let measurements = TableMeasurements::read(&table);
        let worker_cap = NonZeroUsize::new(worker_cap).ok_or_else(|| {
            DatabaseInt64WalRecoveryError::Table(Error::InvalidQuery(
                "Int64 WAL aggregate worker cap must be nonzero".to_owned(),
            ))
        })?;
        let mut database = Self {
            catalog: Catalog::new(),
            measurements: DatabaseMeasurements::default(),
            query_result_limits: query_limits_from_array(query_limits),
            table_limits: database_table_limits,
            global_aggregate_parallelism: GlobalAggregateParallelism::system(worker_cap),
            index_pruning_counters: IndexPruningCounters::default(),
            int64_write_ahead_log: None,
        };
        database.catalog.register_table(table)?;
        database.measurements.add(measurements);
        Ok(database)
    }

    /// Replays every checksummed registry member in canonical table-name order
    /// and publishes them only by returning one completely constructed database.
    /// Manifest, per-member, aggregate-byte, and aggregate-record limits bound
    /// recovery. Any corrupt, missing, inconsistent, or oversized member
    /// returns an error without exposing a partial catalog.
    ///
    /// Recovery is read-only and does not attach writers to the returned
    /// database. Enable a new registry at a new path to resume durable writes
    /// or compact the recovered state; registries are not rotated or compacted
    /// in place.
    #[cfg(unix)]
    pub fn recover_int64_write_ahead_log_registry(
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogRegistryLimits,
    ) -> std::result::Result<Self, DatabaseInt64WalRegistryRecoveryError> {
        let recovered = wal::recover_registry(path.as_ref(), limits)?;
        let first = recovered
            .tables
            .first()
            .expect("registry validation rejects an empty manifest");
        let database_table_limits = TableLimits::new(
            first.database_table_limits[0],
            first.database_table_limits[1],
            first.database_table_limits[2],
        );
        let query_result_limits = query_limits_from_array(first.query_limits);
        let worker_cap = NonZeroUsize::new(first.worker_cap).expect("WAL decoder rejects zero");

        let mut tables = Vec::with_capacity(recovered.tables.len());
        for bootstrap in recovered.tables {
            let table_name = bootstrap.table_name.clone();
            let table_limits = TableLimits::new(
                bootstrap.table_limits[0],
                bootstrap.table_limits[1],
                bootstrap.table_limits[2],
            );
            let table = if bootstrap.nullable {
                Table::with_nullable_int64_values(
                    bootstrap.table_name,
                    bootstrap.column_name,
                    bootstrap.values,
                    table_limits,
                )
            } else {
                Table::with_int64_values(
                    bootstrap.table_name,
                    bootstrap.column_name,
                    bootstrap
                        .values
                        .into_iter()
                        .map(|value| value.expect("non-nullable WAL replay contains no NULL"))
                        .collect(),
                    table_limits,
                )
            }
            .map_err(|error| DatabaseInt64WalRegistryRecoveryError::Table {
                table: table_name,
                error,
            })?;
            tables.push(table);
        }

        let measurements = tables.iter().map(TableMeasurements::read).fold(
            DatabaseMeasurements::default(),
            |mut total, table| {
                total.add(table);
                total
            },
        );
        let mut catalog = Catalog::new();
        if let Err((index, error)) = catalog.register_tables(tables) {
            return Err(DatabaseInt64WalRegistryRecoveryError::Table {
                table: format!("registry member {index}"),
                error,
            });
        }
        Ok(Self {
            catalog,
            measurements,
            query_result_limits,
            table_limits: database_table_limits,
            global_aggregate_parallelism: GlobalAggregateParallelism::system(worker_cap),
            index_pruning_counters: IndexPruningCounters::default(),
            int64_write_ahead_log: None,
        })
    }

    /// Plural alias for [`Self::recover_int64_write_ahead_log_registry`].
    #[cfg(unix)]
    pub fn recover_int64_write_ahead_logs(
        path: impl AsRef<Path>,
        limits: Int64WriteAheadLogRegistryLimits,
    ) -> std::result::Result<Self, DatabaseInt64WalRegistryRecoveryError> {
        Self::recover_int64_write_ahead_log_registry(path, limits)
    }

    /// Detaches the current WAL after all prior successful mutations have
    /// already been synchronized. The existing file remains as an immutable
    /// recovery history.
    #[cfg(unix)]
    pub fn disable_int64_write_ahead_log(&mut self) -> bool {
        self.int64_write_ahead_log.take().is_some()
    }

    /// Reports whether this database currently has an attached `Int64` WAL.
    #[cfg(unix)]
    #[must_use]
    pub fn int64_write_ahead_log_enabled(&self) -> bool {
        self.int64_write_ahead_log.is_some()
    }

    /// Returns the configured computation-lane cap for supported parallel aggregates.
    #[must_use]
    pub const fn global_aggregate_worker_cap(&self) -> NonZeroUsize {
        self.global_aggregate_parallelism.worker_cap()
    }

    /// Atomically builds and publishes one caller-named, one-column `Int64`
    /// table from deterministic inclusive range partitions.
    ///
    /// The database's configured [`TableLimits`] and default partition count,
    /// row, and scalar-byte bounds are all checked before the catalog or
    /// cached metrics change. Bounds must be ascending and non-overlapping,
    /// and every value must belong to its declared partition.
    pub fn create_int64_range_partitioned_table(
        &mut self,
        table_name: &str,
        column_name: &str,
        partitions: Vec<Int64RangePartition>,
    ) -> std::result::Result<(), Int64RangePartitionError> {
        let max_rows = self
            .table_limits
            .max_rows
            .min(DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS);
        self.create_int64_range_partitioned_table_with_limits(
            table_name,
            column_name,
            partitions,
            Int64RangePartitionLimits {
                max_partitions: DEFAULT_MAX_INT64_RANGE_PARTITIONS,
                max_rows,
                max_bytes: max_rows
                    .saturating_mul(std::mem::size_of::<i64>())
                    .min(DEFAULT_MAX_INT64_RANGE_PARTITION_BYTES),
            },
        )
    }

    /// Builds a range-partitioned table with caller-selected construction
    /// bounds in addition to the database's persistent table limits.
    ///
    /// Publication is one catalog insertion after name, range layout, value
    /// membership, partition count, row count, byte count, and all table caps
    /// have succeeded. Every error leaves catalog data and cached metrics
    /// unchanged.
    pub fn create_int64_range_partitioned_table_with_limits(
        &mut self,
        table_name: &str,
        column_name: &str,
        partitions: Vec<Int64RangePartition>,
        partition_limits: Int64RangePartitionLimits,
    ) -> std::result::Result<(), Int64RangePartitionError> {
        if self.catalog.table_exists(table_name) {
            return Err(Error::TableAlreadyExists(table_name.to_owned()).into());
        }

        let table = Table::with_int64_range_partitions(
            table_name.to_owned(),
            column_name.to_owned(),
            partitions,
            partition_limits,
            self.table_limits,
        )?;
        let measurements = TableMeasurements::read(&table);
        self.catalog.register_table(table)?;
        self.measurements.add(measurements);
        Ok(())
    }

    /// Replaces the computation-lane cap for supported parallel aggregates and
    /// returns the previous cap.
    ///
    /// The new value is reported by subsequent `SHOW SETTINGS` and
    /// `system.settings` queries and applies to subsequent supported
    /// aggregates. A cap of one keeps those aggregates sequential. Higher caps
    /// remain subject to the process-wide worker budget, available hardware,
    /// and the fixed [`MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS`] ceiling.
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use rusthouse::Database;
    ///
    /// let mut database = Database::with_global_aggregate_worker_cap(
    ///     NonZeroUsize::new(1).unwrap(),
    /// );
    /// let previous = database.set_global_aggregate_worker_cap(
    ///     NonZeroUsize::new(2).unwrap(),
    /// );
    /// assert_eq!(previous.get(), 1);
    /// assert_eq!(database.global_aggregate_worker_cap().get(), 2);
    /// ```
    pub fn set_global_aggregate_worker_cap(
        &mut self,
        global_aggregate_worker_cap: NonZeroUsize,
    ) -> NonZeroUsize {
        self.global_aggregate_parallelism
            .set_worker_cap(global_aggregate_worker_cap)
    }

    /// Reopens one self-describing `Int64` or `Nullable(Int64)` snapshot as a
    /// named batch table.
    ///
    /// The snapshot supplies the column name, persisted row cap, and rows;
    /// `table_name` supplies its name in this database. The envelope and
    /// payload codecs bound all file and decoding work. The persisted row cap
    /// must fit the database's configured row limit, and the decoded one-column
    /// table must fit its column and cell limits. Its persisted row cap is
    /// retained for subsequent inserts.
    ///
    /// This `Database` adapter imports exactly one table with one `Int64` or
    /// `Nullable(Int64)` column, preserving the decoded column name,
    /// nullability, NULL positions, row order, and row cap. Corruption, invalid
    /// identifiers, duplicate table names, and every resource limit are checked
    /// before the catalog or cached metrics are changed.
    pub fn restore_int64_table_from_file(
        &mut self,
        table_name: &str,
        path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> std::result::Result<(), DatabaseSnapshotRestoreError> {
        if self.catalog.table_exists(table_name) {
            return Err(Error::TableAlreadyExists(table_name.to_owned()).into());
        }

        let restored = restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec)?;
        self.register_reopened_int64_table(table_name, restored)
    }

    /// Imports one bounded row-only RLE `Int64` or `Nullable(Int64)` snapshot
    /// as a named batch table.
    ///
    /// Unlike the self-describing snapshot accepted by
    /// [`Self::restore_int64_table_from_file`], this format stores rows only.
    /// The caller must supply the column schema and the table row cap; neither
    /// value is authenticated by the file. `table_name` is also caller-owned
    /// catalog metadata. The envelope and RLE codecs bound file bytes, decoded
    /// rows, runs, and payload bytes.
    ///
    /// Existing names are resolved case-insensitively and rejected before the
    /// source path is opened. The complete file is restored through
    /// [`crate::restore_int64_table_rle_from_file`] and converted into a fully
    /// validated physical `Int64` or `Nullable(Int64)` batch table before
    /// registration. The caller-supplied nullability, exact NULL positions,
    /// row order, and row cap are retained. Corruption, schema nullability
    /// mismatches, invalid identifiers, caller row-cap failures, and the
    /// database's configured [`TableLimits`] leave catalog data and cached
    /// metrics unchanged.
    pub fn restore_int64_table_rle_from_file(
        &mut self,
        table_name: &str,
        path: impl AsRef<Path>,
        schema: Schema,
        row_cap: usize,
        snapshot_codec: SnapshotCodec,
        payload_codec: NullableI64RlePayloadCodec,
    ) -> std::result::Result<(), DatabaseRleSnapshotRestoreError> {
        if self.catalog.table_exists(table_name) {
            return Err(Error::TableAlreadyExists(table_name.to_owned()).into());
        }

        let restored = restore_int64_table_rle_from_file(
            path,
            schema,
            row_cap,
            snapshot_codec,
            payload_codec,
        )?;
        let table = self
            .prepare_decoded_int64_table(table_name, restored)
            .map_err(DatabaseRleSnapshotRestoreError::Table)?;
        let measurements = TableMeasurements::read(&table);
        self.catalog.register_table(table)?;
        self.measurements.add(measurements);
        Ok(())
    }

    /// Atomically replaces one existing table from a self-describing `Int64`
    /// or `Nullable(Int64)` snapshot.
    ///
    /// `table_name` uses the catalog's case-insensitive lookup, while the
    /// replacement retains the target's stored display name. The snapshot
    /// supplies the new column name, rows, and persisted row cap. The envelope
    /// and payload codecs bound all file and decoding work.
    ///
    /// Target existence is checked before the source is opened. The snapshot
    /// is then fully decoded and staged as a batch table, including its column
    /// metadata, nullability, exact NULL positions, row order, and row cap.
    /// SQL identifier, row-cap, column, and cell validation completes before
    /// one catalog swap. Every failure preserves the old table and its cached
    /// metrics; success replaces those metrics with the staged table's exact
    /// measurements.
    pub fn replace_int64_table_from_file(
        &mut self,
        table_name: &str,
        path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> std::result::Result<(), DatabaseSnapshotRestoreError> {
        self.reject_unlogged_wal_mutation(table_name, "snapshot table replacement")?;
        let display_name = self.catalog.table(table_name)?.name().to_owned();
        let restored = restore_int64_table_payload_from_file(path, snapshot_codec, payload_codec)?;
        self.replace_restored_int64_table(table_name, &display_name, restored)
    }

    /// Atomically replaces one existing table from a primary self-describing
    /// `Int64` or `Nullable(Int64)` snapshot, or an explicit backup.
    ///
    /// `table_name` uses the catalog's case-insensitive lookup, while a
    /// successful replacement retains the target's stored display name. The
    /// primary file is decoded first and takes precedence whenever decoding
    /// succeeds. The backup is inspected only after a typed primary file,
    /// envelope, or payload decoding failure. Success reports which file
    /// supplied the replacement; if both fail, the
    /// [`DatabaseSnapshotRestoreError::Recovery`] variant retains both typed
    /// failures.
    ///
    /// Target existence is checked before either source is opened. After one
    /// source is decoded, the same schema, SQL identifier, row-cap, column,
    /// and cell validation as [`Self::replace_int64_table_from_file`] runs
    /// before one catalog swap. Database validation does not cause a fallback.
    /// Dual recovery, validation, and resource-limit failures all preserve the
    /// old table and its cached metrics.
    pub fn replace_int64_table_from_file_with_backup(
        &mut self,
        table_name: &str,
        primary_path: impl AsRef<Path>,
        backup_path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> std::result::Result<Int64TablePayloadFileRecoverySource, DatabaseSnapshotRestoreError>
    {
        self.reject_unlogged_wal_mutation(table_name, "snapshot table replacement")?;
        let display_name = self.catalog.table(table_name)?.name().to_owned();
        let recovered = restore_int64_table_payload_from_file_with_backup(
            primary_path,
            backup_path,
            snapshot_codec,
            payload_codec,
        )?;
        let (restored, source) = recovered.into_parts();
        self.replace_restored_int64_table(table_name, &display_name, restored)?;
        Ok(source)
    }

    /// Atomically reopens a caller-bounded set of named, self-describing
    /// `Int64` or `Nullable(Int64)` snapshots.
    ///
    /// `max_entries` is an inclusive bound on the number of source files. The
    /// count is checked before name validation or file access. All destination
    /// names are then validated against the existing catalog and against each
    /// other using case-insensitive SQL resolution before any file is opened.
    /// Each entry retains its own envelope, column-name, row, and payload-byte
    /// codec bounds.
    ///
    /// Files are decoded and converted into fully validated batch tables in
    /// input order, but remain staged outside the catalog until every entry
    /// succeeds. Nullability, NULL positions, and row order are preserved for
    /// each table. Excess counts, invalid or colliding names, file corruption,
    /// invalid decoded schemas, and configured [`TableLimits`] therefore leave
    /// all catalog data and cached metrics unchanged. Every error identifies
    /// the zero-based input entry and its caller-supplied table name.
    pub fn restore_int64_tables_from_files(
        &mut self,
        entries: &[DatabaseSnapshotRestoreEntry<'_>],
        max_entries: usize,
    ) -> std::result::Result<(), DatabaseSnapshotSetRestoreError> {
        if entries.len() > max_entries {
            let rejected = entries[max_entries];
            return Err(DatabaseSnapshotSetRestoreError::EntryLimitExceeded {
                entry_index: max_entries,
                table_name: rejected.table_name.to_owned(),
                entries: entries.len(),
                max_entries,
            });
        }

        let mut incoming_names = HashSet::with_capacity(entries.len());
        for (entry_index, entry) in entries.iter().copied().enumerate() {
            let name_error = validate_table_name(entry.table_name).err().or_else(|| {
                let normalized = entry.table_name.to_ascii_lowercase();
                if self.catalog.table_exists(entry.table_name) || !incoming_names.insert(normalized)
                {
                    Some(Error::TableAlreadyExists(entry.table_name.to_owned()))
                } else {
                    None
                }
            });
            if let Some(error) = name_error {
                return Err(DatabaseSnapshotSetRestoreError::Entry {
                    entry_index,
                    table_name: entry.table_name.to_owned(),
                    error: error.into(),
                });
            }
        }

        let mut staged = Vec::with_capacity(entries.len());
        let mut staged_measurements = DatabaseMeasurements::default();
        for (entry_index, entry) in entries.iter().copied().enumerate() {
            let restored = restore_int64_table_payload_from_file(
                entry.path,
                entry.snapshot_codec,
                entry.payload_codec,
            )
            .map_err(|error| DatabaseSnapshotSetRestoreError::Entry {
                entry_index,
                table_name: entry.table_name.to_owned(),
                error: error.into(),
            })?;
            let table = self
                .prepare_decoded_int64_table(entry.table_name, restored)
                .map_err(|error| DatabaseSnapshotSetRestoreError::Entry {
                    entry_index,
                    table_name: entry.table_name.to_owned(),
                    error: error.into(),
                })?;
            staged_measurements.add(TableMeasurements::read(&table));
            staged.push(table);
        }

        self.catalog
            .register_tables(staged)
            .map_err(
                |(entry_index, error)| DatabaseSnapshotSetRestoreError::Entry {
                    entry_index,
                    table_name: entries[entry_index].table_name.to_owned(),
                    error: error.into(),
                },
            )?;
        self.measurements.add_totals(staged_measurements);
        Ok(())
    }

    /// Reopens one self-describing `Int64` or `Nullable(Int64)` snapshot from
    /// a primary or caller-supplied backup file as a named batch table.
    ///
    /// The primary file is decoded first and takes precedence whenever it is
    /// valid. The backup is inspected only if bounded primary file, envelope,
    /// or payload decoding fails. A successful restore reports which file
    /// supplied the table. If neither file can be decoded, the
    /// [`DatabaseSnapshotRestoreError::Recovery`] variant retains both typed
    /// failures.
    ///
    /// Database validation is identical to [`Self::restore_int64_table_from_file`].
    /// The caller-provided table name must be unique, and the persisted table
    /// must fit all configured [`TableLimits`]. Registration preserves the
    /// decoded column name, nullability, exact NULL positions, row order, and
    /// persisted row cap. The catalog and its cached metrics change only after
    /// decoding and every validation step succeed; database validation does
    /// not cause a fallback to the backup.
    pub fn restore_int64_table_from_file_with_backup(
        &mut self,
        table_name: &str,
        primary_path: impl AsRef<Path>,
        backup_path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> std::result::Result<Int64TablePayloadFileRecoverySource, DatabaseSnapshotRestoreError>
    {
        if self.catalog.table_exists(table_name) {
            return Err(Error::TableAlreadyExists(table_name.to_owned()).into());
        }

        let recovered = restore_int64_table_payload_from_file_with_backup(
            primary_path,
            backup_path,
            snapshot_codec,
            payload_codec,
        )?;
        let (restored, source) = recovered.into_parts();
        self.register_reopened_int64_table(table_name, restored)?;
        Ok(source)
    }

    fn register_reopened_int64_table(
        &mut self,
        table_name: &str,
        restored: Int64Table,
    ) -> std::result::Result<(), DatabaseSnapshotRestoreError> {
        let table = self
            .prepare_decoded_int64_table(table_name, restored)
            .map_err(DatabaseSnapshotRestoreError::Table)?;
        let measurements = TableMeasurements::read(&table);
        self.catalog.register_table(table)?;
        self.measurements.add(measurements);
        Ok(())
    }

    fn replace_restored_int64_table(
        &mut self,
        table_name: &str,
        display_name: &str,
        restored: Int64Table,
    ) -> std::result::Result<(), DatabaseSnapshotRestoreError> {
        let replacement = self
            .prepare_decoded_int64_table(display_name, restored)
            .map_err(DatabaseSnapshotRestoreError::Table)?;
        let replacement_measurements = TableMeasurements::read(&replacement);

        let previous = self.catalog.replace_table(table_name, replacement)?;
        self.measurements
            .replace(TableMeasurements::read(&previous), replacement_measurements);
        Ok(())
    }

    fn prepare_decoded_int64_table(&self, table_name: &str, restored: Int64Table) -> Result<Table> {
        let column = restored.schema().column();
        let nullable = column.is_nullable();
        if restored.row_cap() > self.table_limits.max_rows {
            return Err(Error::ResourceLimitExceeded {
                resource: "table row cap",
                actual: restored.row_cap(),
                max: self.table_limits.max_rows,
            });
        }

        let column_name = column.name().to_owned();
        let restored_limits = TableLimits {
            max_rows: restored.row_cap(),
            max_columns: self.table_limits.max_columns,
            max_cells: self.table_limits.max_cells,
        };
        let values = restored.into_values();
        if nullable {
            Table::with_nullable_int64_values(
                table_name.to_owned(),
                column_name,
                values,
                restored_limits,
            )
        } else {
            let values = values
                .into_iter()
                .map(|value| value.expect("the decoded snapshot column is non-nullable"))
                .collect();
            Table::with_int64_values(table_name.to_owned(), column_name, values, restored_limits)
        }
    }

    /// Atomically saves one nullable or non-nullable, one-column `Int64` batch
    /// table on Unix.
    ///
    /// `table_name` uses the catalog's normal case-insensitive lookup. The
    /// self-describing payload preserves the stored column name, nullability,
    /// exact NULL positions, row order, and table row cap; it intentionally
    /// does not contain the batch table name or any other catalog table. Files
    /// produced here can be reopened by
    /// [`crate::restore_int64_table_payload_from_file`].
    ///
    /// Table existence, exact column count, and physical type are checked before
    /// any filesystem access. Payload encoding then completes through the
    /// existing [`Int64TablePayloadCodec`] before the checksummed envelope is
    /// atomically replaced. Consequently, every validation, encoding, or
    /// pre-rename replacement failure preserves an existing destination.
    #[cfg(unix)]
    pub fn save_int64_table_to_file(
        &self,
        table_name: &str,
        path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: Int64TablePayloadCodec,
    ) -> std::result::Result<(), DatabaseSnapshotSaveError> {
        let table = self.catalog.table(table_name)?;
        if table.schema().len() != 1 {
            return Err(DatabaseSnapshotSaveError::UnsupportedColumnCount {
                table: table.name().to_owned(),
                column_count: table.schema().len(),
            });
        }

        let column = &table.schema()[0];
        if column.data_type != DataType::Int64 {
            return Err(DatabaseSnapshotSaveError::UnsupportedColumnType {
                column: column.name.clone(),
                data_type: column.data_type,
            });
        }
        let payload = match &table.columns()[0] {
            Column::Int64(values) => {
                payload_codec.encode_non_nullable_values(&column.name, table.row_cap(), values)
            }
            Column::NullableInt64(values) => {
                payload_codec.encode_nullable_values(&column.name, table.row_cap(), values)
            }
            _ => unreachable!("batch table storage must agree with its validated schema"),
        };
        let payload = payload.map_err(Int64TablePayloadFileSaveError::from)?;
        snapshot_codec
            .replace_file(path, &payload)
            .map_err(Int64TablePayloadFileSaveError::from)?;
        Ok(())
    }

    /// Atomically saves one nullable or non-nullable, one-column `Int64` batch
    /// table as a bounded, RLE-compressed snapshot on Unix.
    ///
    /// `table_name` uses the catalog's normal case-insensitive lookup. This is
    /// deliberately a row-only format: it preserves exact values, NULL
    /// positions, and row order, but does not store the column schema, table
    /// row cap, batch table name, or any other catalog metadata. Reopening with
    /// [`Self::restore_int64_table_rle_from_file`] therefore requires the
    /// caller to supply the column schema and row cap.
    ///
    /// Table existence, exact column count, and physical type are checked
    /// before any filesystem access. The existing
    /// [`NullableI64RlePayloadCodec`] bounds rows, runs, and encoded bytes
    /// before the checksummed envelope is atomically replaced. Every
    /// validation, encoding, or pre-rename replacement failure preserves an
    /// existing destination. Encoding borrows the physical batch column and
    /// does not clone it into an intermediate [`Int64Table`].
    #[cfg(unix)]
    pub fn save_int64_table_rle_to_file(
        &self,
        table_name: &str,
        path: impl AsRef<Path>,
        snapshot_codec: SnapshotCodec,
        payload_codec: NullableI64RlePayloadCodec,
    ) -> std::result::Result<(), DatabaseRleSnapshotSaveError> {
        let table = self.catalog.table(table_name)?;
        if table.schema().len() != 1 {
            return Err(DatabaseRleSnapshotSaveError::UnsupportedColumnCount {
                table: table.name().to_owned(),
                column_count: table.schema().len(),
            });
        }

        let column = &table.schema()[0];
        if column.data_type != DataType::Int64 {
            return Err(DatabaseRleSnapshotSaveError::UnsupportedColumnType {
                column: column.name.clone(),
                data_type: column.data_type,
            });
        }
        let payload = match &table.columns()[0] {
            Column::Int64(values) => payload_codec.encode_non_nullable_values(values),
            Column::NullableInt64(values) => payload_codec.encode(values),
            _ => unreachable!("batch table storage must agree with its validated schema"),
        };
        let payload = payload.map_err(Int64TableRleFileSaveError::from)?;
        snapshot_codec
            .replace_file(path, &payload)
            .map_err(Int64TableRleFileSaveError::from)?;
        Ok(())
    }

    /// Atomically appends bounded, typed, headerless `CSV` input.
    ///
    /// Every logical record is data and must contain exactly one field for each
    /// physical schema column, in schema order. Fields parse as `Int64`, finite
    /// `Float64`, `Bool`, or `String`. Double-quoted fields may contain commas,
    /// LF or CRLF line endings, and doubled (`""`) quote escapes. Only LF and
    /// CRLF record endings are accepted. The exact unquoted token `NULL` stores
    /// SQL `NULL` only in a physical `Nullable(Int64)` column. Empty input
    /// appends zero rows.
    ///
    /// The complete input, every row and value, configured limits, and
    /// remaining table capacity are validated before any physical column is
    /// changed. Every error therefore leaves the target table unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::csv::CsvIngestLimits;
    /// use rusthouse::batch::engine::Database;
    ///
    /// let mut database = Database::new();
    /// database.execute(
    ///     "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);",
    /// )?;
    /// let input = b"1,2.5,true,\"alpha,beta\"\n";
    /// let rows = database.ingest_csv(
    ///     "metrics",
    ///     input,
    ///     CsvIngestLimits::new(input.len(), 1, 4),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_csv(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> std::result::Result<usize, CsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            csv::parse_rows_without_names(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.log_prepared_int64_append(table, &rows)?;
        self.table_mut(table)?.append_prepared_insert_rows(rows);
        Ok(affected_rows)
    }

    /// Atomically appends a bounded, typed `CSVWithNames` input.
    ///
    /// The header must contain a nonempty subset of target column names without
    /// duplicates, using exact case, but may place those names in any order.
    /// Each data field is parsed using the `Int64`, finite `Float64`, `Bool`, or
    /// `String` type selected by its header. Omitted columns receive the same
    /// typed defaults as SQL `INSERT`: `NULL` for `Nullable(Int64)`, otherwise
    /// `0`, `0.0`, `false`, or an empty string.
    /// Data fields may be double-quoted, allowing commas, LF or CRLF line
    /// endings, and doubled (`""`) quote escapes; decoded contents use the same
    /// type rules. The exact unquoted token `NULL` stores SQL `NULL` only in a
    /// physical `Nullable(Int64)` column. Headers must remain unquoted. Only LF
    /// and CRLF line endings are accepted.
    ///
    /// The complete input, header, every row and value, configured limits, and
    /// remaining table capacity are validated before any physical column is
    /// changed. Every error therefore leaves the target table unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::csv::CsvIngestLimits;
    /// use rusthouse::batch::engine::Database;
    ///
    /// let mut database = Database::new();
    /// database.execute(
    ///     "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);",
    /// )?;
    /// let input = b"label,id\nalpha,1\n";
    /// let rows = database.ingest_csv_with_names(
    ///     "metrics",
    ///     input,
    ///     CsvIngestLimits::new(input.len(), 1, 2),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_csv_with_names(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> std::result::Result<usize, CsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            csv::parse_rows_with_names(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.log_prepared_int64_append(table, &rows)?;
        self.table_mut(table)?.append_prepared_insert_rows(rows);
        Ok(affected_rows)
    }

    /// Atomically appends bounded, typed, headerless `TabSeparated` input.
    ///
    /// Every physical line is data and must contain exactly one field for each
    /// physical schema column, in schema order. Fields use the TSV writer's
    /// ClickHouse-style escapes: `\\`, `\t`, `\r`, `\n`, `\0`, `\b`, `\f`, and
    /// `\'`. The exact raw `\N` field is NULL only for a physical
    /// `Nullable(Int64)` column; escaped `\\N` remains String data. Decoded
    /// values parse as `Int64`, finite `Float64`, `Bool`, or `String`; records
    /// may use LF or CRLF. Empty input appends zero rows.
    ///
    /// The complete input, every row and value, configured limits, and
    /// remaining table capacity are validated before any physical column is
    /// changed. Every error therefore leaves the target table unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::engine::Database;
    /// use rusthouse::batch::tsv::TsvIngestLimits;
    ///
    /// let mut database = Database::new();
    /// database.execute("CREATE TABLE notes (id Int64, text String);")?;
    /// let input = b"7\tline\\nwith\\ttab\n";
    /// let rows = database.ingest_tsv(
    ///     "notes",
    ///     input,
    ///     TsvIngestLimits::new(input.len(), 1, 2),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_tsv(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> std::result::Result<usize, TsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            tsv::parse_rows_without_names(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.log_prepared_int64_append(table, &rows)?;
        self.table_mut(table)?.append_prepared_insert_rows(rows);
        Ok(affected_rows)
    }

    /// Atomically appends bounded, typed `TabSeparatedWithNames` input.
    ///
    /// The decoded header must contain a nonempty, duplicate-free subset of
    /// target schema columns in any order, with matching case. Omitted columns
    /// receive their typed defaults, including `NULL` for `Nullable(Int64)`.
    /// Fields use the TSV writer's ClickHouse-style escapes: `\\`, `\t`, `\r`,
    /// `\n`, `\0`, `\b`, `\f`, and `\'`. Values are
    /// parsed as `Int64`, finite `Float64`, `Bool`, or `String`; the exact raw
    /// `\N` field is NULL only for a selected physical `Nullable(Int64)` column,
    /// while escaped `\\N` remains String data. Records may use LF or CRLF.
    ///
    /// Each supplied field is parsed as the type selected by its header.
    ///
    /// Parsing, all configured limits, and remaining table capacity are
    /// validated before any physical column changes.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::engine::Database;
    /// use rusthouse::batch::tsv::TsvIngestLimits;
    ///
    /// let mut database = Database::new();
    /// database.execute("CREATE TABLE notes (id Int64, text String);")?;
    /// let input = b"text\nline\\nwith\\ttab\n";
    /// let rows = database.ingest_tsv_with_names(
    ///     "notes",
    ///     input,
    ///     TsvIngestLimits::new(input.len(), 1, 1),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_tsv_with_names(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> std::result::Result<usize, TsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            tsv::parse_rows_with_names(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.log_prepared_int64_append(table, &rows)?;
        self.table_mut(table)?.append_prepared_insert_rows(rows);
        Ok(affected_rows)
    }

    /// Atomically appends bounded, one-column `JSONCompactEachRow` input.
    ///
    /// Each physical line must be one JSON array containing exactly one JSON
    /// number. `Int64` and `Nullable(Int64)` targets require integer syntax and
    /// range; nullable targets also accept JSON `null`. `Float64` targets accept
    /// finite integer, decimal, and exponent forms but reject `null`. The
    /// existing target must have exactly one physical `Int64`,
    /// `Nullable(Int64)`, or `Float64` column. JSON whitespace is accepted around
    /// the array and value, and LF or CRLF records are accepted. Empty input
    /// appends zero rows.
    ///
    /// UTF-8, JSON shape, every typed number, all configured limits, and
    /// remaining table capacity are validated before the existing WAL and one
    /// atomic prepared-row append path runs. Every error leaves the table
    /// unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::engine::Database;
    /// use rusthouse::batch::json_compact_each_row::JsonCompactEachRowIngestLimits;
    ///
    /// let mut database = Database::new();
    /// database.execute("CREATE TABLE readings (value Float64);")?;
    /// let input = b"[-7]\n[6.25e-3]\n";
    /// let rows = database.ingest_json_compact_each_row(
    ///     "readings",
    ///     input,
    ///     JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
    /// )?;
    /// assert_eq!(rows, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_json_compact_each_row(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: JsonCompactEachRowIngestLimits,
    ) -> std::result::Result<usize, JsonCompactEachRowIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            json_compact_each_row::parse_rows(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        if affected_rows != 0 {
            self.log_prepared_int64_append(table, &rows)?;
        }
        self.table_mut(table)?.append_prepared_insert_rows(rows);
        Ok(affected_rows)
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        self.execute_with_result_limit(sql, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Atomically executes a nonempty SQL batch containing only `INSERT` statements.
    ///
    /// Every target table, explicit-column mapping, row shape, value type,
    /// finite floating-point value, and cumulative per-table row count is
    /// validated before any row is appended. Omitted explicit columns are
    /// expanded to typed defaults during that preflight. A failure therefore
    /// leaves every table unchanged. Successful statements are committed and
    /// reported in input order.
    pub fn execute_insert_batch(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        let statements = sql::parse(sql)?;
        self.execute_insert_statements(statements)
    }

    /// Executes a batch while bounding results retained for the caller.
    pub fn execute_with_result_limit(
        &mut self,
        sql: &str,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>> {
        let statements = sql::parse(sql)?;
        self.execute_statements_with_result_limit(statements, max_result_bytes)
    }

    pub(crate) fn execute_statements_with_result_limit(
        &mut self,
        statements: Vec<Statement>,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>> {
        let mut results = Vec::with_capacity(statements.len());
        let mut retained_bytes = 0_usize;
        for statement in statements {
            let remaining_bytes = max_result_bytes.saturating_sub(retained_bytes);
            let tightened_result_limit = remaining_bytes < self.query_result_limits.max_bytes;
            let query_limits = QueryResultLimits {
                max_bytes: self.query_result_limits.max_bytes.min(remaining_bytes),
                ..self.query_result_limits
            };
            let result = self
                .execute_statement_with_limits(statement, query_limits)
                .map_err(|error| match error {
                    Error::ResourceLimitExceeded {
                        resource:
                            "SELECT result bytes"
                            | "SHOW DATABASES result bytes"
                            | "SHOW SETTINGS result bytes"
                            | "SHOW FUNCTIONS result bytes"
                            | "SHOW TABLES result bytes"
                            | "SHOW CREATE TABLE result bytes"
                            | "DESCRIBE TABLE result bytes"
                            | "EXISTS TABLE result bytes",
                        actual,
                        ..
                    } if tightened_result_limit => Error::ResultLimitExceeded {
                        bytes: retained_bytes.saturating_add(actual),
                        max_bytes: max_result_bytes,
                    },
                    error => error,
                })?;
            retained_bytes = retained_bytes.saturating_add(result.estimated_retained_bytes());
            if retained_bytes > max_result_bytes {
                return Err(Error::ResultLimitExceeded {
                    bytes: retained_bytes,
                    max_bytes: max_result_bytes,
                });
            }
            results.push(result);
        }
        Ok(results)
    }

    pub(crate) fn execute_insert_statements(
        &mut self,
        statements: Vec<Statement>,
    ) -> Result<Vec<StatementResult>> {
        for statement in &statements {
            if !matches!(
                statement,
                Statement::Insert { .. } | Statement::InsertWithColumns { .. }
            ) {
                return Err(Error::InsertOnlyStatementRequired {
                    statement: statement_name(statement),
                });
            }
        }

        let mut incoming_rows_by_table = HashMap::<String, usize>::new();
        let mut prepared = Vec::with_capacity(statements.len());
        for statement in statements {
            let (table, columns, rows) = match statement {
                Statement::Insert { table, rows } => (table, None, rows),
                Statement::InsertWithColumns {
                    table,
                    columns,
                    rows,
                } => (table, Some(columns), rows),
                _ => unreachable!("non-INSERT statements were rejected"),
            };
            let target = self.catalog.table(&table)?;
            let cumulative_rows = incoming_rows_by_table
                .entry(table.to_ascii_lowercase())
                .or_default();
            *cumulative_rows = cumulative_rows.saturating_add(rows.len());
            let rows = target.prepare_insert_rows(columns.as_deref(), rows, *cumulative_rows)?;
            prepared.push((table, rows));
        }

        let mut wal_table: Option<String> = None;
        let mut wal_values = Vec::new();
        for (table, rows) in &prepared {
            if self.wal_tracks(table) {
                if wal_table
                    .as_ref()
                    .is_some_and(|tracked| !tracked.eq_ignore_ascii_case(table))
                {
                    return Err(Error::InvalidQuery(
                        "an atomic INSERT batch cannot span multiple independently logged WAL registry tables"
                            .to_owned(),
                    ));
                }
                wal_table.get_or_insert_with(|| table.clone());
                wal_values.extend(
                    rows.int64_values()
                        .expect("WAL opt-in guarantees one preflighted Int64 column"),
                );
            }
        }
        if let Some(table) = wal_table {
            self.log_int64_append(&table, &wal_values)?;
        }

        let mut results = Vec::with_capacity(prepared.len());
        for (table, rows) in prepared {
            let affected_rows = rows.len();
            self.table_mut(&table)
                .expect("preflight resolved every INSERT target")
                .append_prepared_insert_rows(rows);
            results.push(StatementResult::Command {
                tag: "INSERT",
                affected_rows,
            });
        }
        Ok(results)
    }

    /// Executes one already-parsed read-only query without mutable access.
    pub(crate) fn execute_query_statement_with_result_limit(
        &self,
        statement: Statement,
        max_result_bytes: usize,
    ) -> Result<QueryResult> {
        self.execute_query_statement_with_parameterized_limits(
            statement,
            ParameterizedQueryLimits {
                max_result_bytes,
                max_result_rows: self.query_result_limits.max_rows,
                max_result_values: 0,
                max_scan_rows: 0,
                max_groups: 0,
                max_group_key_cells: 0,
                max_group_key_bytes: 0,
                max_ordering_state_bytes: 0,
                max_aggregate_state_cells: 0,
                max_aggregate_state_bytes: 0,
                max_threads: 0,
            },
        )
    }

    /// Executes one already-parsed read-only query with caller-supplied limits.
    ///
    /// Caller limits may only tighten the database's configured result-byte,
    /// result-row, result-value, scan-row, group-count, group-key cell,
    /// group-key byte, ordering-state, aggregate-state cell, aggregate-state
    /// byte, and supported global-aggregate worker limits. Zero leaves the
    /// corresponding configured limit in place.
    /// Result-shape validation applies the effective row, value, and byte
    /// limits before result rows are materialized; the effective scan and group
    /// limits are charged independently of `LIMIT`.
    pub(crate) fn execute_query_statement_with_parameterized_limits(
        &self,
        statement: Statement,
        requested_limits: ParameterizedQueryLimits,
    ) -> Result<QueryResult> {
        let ParameterizedQueryLimits {
            max_result_bytes,
            max_result_rows,
            max_result_values,
            max_scan_rows,
            max_groups,
            max_group_key_cells,
            max_group_key_bytes,
            max_ordering_state_bytes,
            max_aggregate_state_cells,
            max_aggregate_state_bytes,
            max_threads,
        } = requested_limits;
        let tightened_result_limit = max_result_bytes < self.query_result_limits.max_bytes;
        let max_rows = if max_result_rows == 0 {
            self.query_result_limits.max_rows
        } else {
            self.query_result_limits.max_rows.min(max_result_rows)
        };
        let max_values = if max_result_values == 0 {
            self.query_result_limits.max_values
        } else {
            self.query_result_limits.max_values.min(max_result_values)
        };
        let max_scan_rows = if max_scan_rows == 0 {
            self.query_result_limits.max_scan_rows
        } else {
            self.query_result_limits.max_scan_rows.min(max_scan_rows)
        };
        let max_groups = if max_groups == 0 {
            self.query_result_limits.max_groups
        } else {
            self.query_result_limits.max_groups.min(max_groups)
        };
        let max_group_key_cells = if max_group_key_cells == 0 {
            self.query_result_limits.max_group_key_cells
        } else {
            self.query_result_limits
                .max_group_key_cells
                .min(max_group_key_cells)
        };
        let max_group_key_bytes = if max_group_key_bytes == 0 {
            self.query_result_limits.max_group_key_bytes
        } else {
            self.query_result_limits
                .max_group_key_bytes
                .min(max_group_key_bytes)
        };
        let max_ordering_state_bytes = if max_ordering_state_bytes == 0 {
            self.query_result_limits.max_ordering_state_bytes
        } else {
            self.query_result_limits
                .max_ordering_state_bytes
                .min(max_ordering_state_bytes)
        };
        let max_aggregate_state_cells = if max_aggregate_state_cells == 0 {
            self.query_result_limits.max_aggregate_state_cells
        } else {
            self.query_result_limits
                .max_aggregate_state_cells
                .min(max_aggregate_state_cells)
        };
        let max_aggregate_state_bytes = if max_aggregate_state_bytes == 0 {
            self.query_result_limits.max_aggregate_state_bytes
        } else {
            self.query_result_limits
                .max_aggregate_state_bytes
                .min(max_aggregate_state_bytes)
        };
        let query_limits = QueryResultLimits {
            max_scan_rows,
            max_rows,
            max_values,
            max_groups,
            max_group_key_cells,
            max_group_key_bytes,
            max_ordering_state_bytes,
            max_aggregate_state_cells,
            max_aggregate_state_bytes,
            max_bytes: self.query_result_limits.max_bytes.min(max_result_bytes),
        };
        let global_aggregate_parallelism = self
            .global_aggregate_parallelism
            .with_request_worker_cap(max_threads);
        let result = self
            .execute_query_statement_with_limits(
                statement,
                query_limits,
                global_aggregate_parallelism,
            )
            .map_err(|error| match error {
                Error::ResourceLimitExceeded {
                    resource:
                        "SELECT result bytes"
                        | "SHOW DATABASES result bytes"
                        | "SHOW SETTINGS result bytes"
                        | "SHOW FUNCTIONS result bytes"
                        | "SHOW TABLES result bytes"
                        | "SHOW CREATE TABLE result bytes"
                        | "DESCRIBE TABLE result bytes"
                        | "EXISTS TABLE result bytes",
                    actual,
                    ..
                } if tightened_result_limit => Error::ResultLimitExceeded {
                    bytes: actual,
                    max_bytes: max_result_bytes,
                },
                error => error,
            })?;
        let retained_bytes = result.estimated_retained_bytes();
        if retained_bytes > max_result_bytes {
            return Err(Error::ResultLimitExceeded {
                bytes: retained_bytes,
                max_bytes: max_result_bytes,
            });
        }
        Ok(result)
    }

    /// Executes one already-parsed statement.
    ///
    /// Callers that stream results should parse the complete batch first, then
    /// invoke this method in order and release each result before continuing.
    pub fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        self.execute_statement_with_limits(statement, self.query_result_limits)
    }

    fn execute_statement_with_limits(
        &mut self,
        statement: Statement,
        query_result_limits: QueryResultLimits,
    ) -> Result<StatementResult> {
        let alter_update_limits = QueryResultLimits {
            max_bytes: self.query_result_limits.max_bytes,
            ..query_result_limits
        };
        match statement {
            Statement::CreateTable { name, columns } => {
                let measurements = TableMeasurements::empty(columns.len());
                self.catalog
                    .create_table_with_limits(name, columns, self.table_limits)?;
                self.measurements.add(measurements);
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableIfNotExists { name, columns } => {
                let measurements = TableMeasurements::empty(columns.len());
                let created = self.catalog.create_table_if_not_exists_with_limits(
                    name,
                    columns,
                    self.table_limits,
                )?;
                if created {
                    self.measurements.add(measurements);
                }
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateNullableInt64Table { name, column } => {
                self.create_nullable_int64_table(name, column, Vec::new())?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateNullableInt64TableIfNotExists { name, column } => {
                if !self.catalog.table_exists(&name) {
                    self.create_nullable_int64_table(name, column, Vec::new())?;
                }
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableWithTrailingNullableInt64 {
                name,
                columns,
                nullable_column,
            } => {
                self.create_table_with_trailing_nullable_int64(
                    name,
                    columns,
                    [nullable_column],
                    false,
                )?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableWithTrailingNullableInt64IfNotExists {
                name,
                columns,
                nullable_column,
            } => {
                self.create_table_with_trailing_nullable_int64(
                    name,
                    columns,
                    [nullable_column],
                    true,
                )?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableWithTwoTrailingNullableInt64 {
                name,
                columns,
                nullable_columns,
            } => {
                self.create_table_with_trailing_nullable_int64(
                    name,
                    columns,
                    nullable_columns,
                    false,
                )?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableWithTwoTrailingNullableInt64IfNotExists {
                name,
                columns,
                nullable_columns,
            } => {
                self.create_table_with_trailing_nullable_int64(
                    name,
                    columns,
                    nullable_columns,
                    true,
                )?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::DropTable { name } => {
                self.reject_unlogged_wal_mutation(&name, "DROP TABLE")?;
                let measurements = TableMeasurements::read(self.catalog.table(&name)?);
                self.catalog.drop_table(&name)?;
                self.measurements.subtract(measurements);
                Ok(StatementResult::Command {
                    tag: "DROP TABLE",
                    affected_rows: 0,
                })
            }
            Statement::DropTableIfExists { name } => {
                self.reject_unlogged_wal_mutation(&name, "DROP TABLE")?;
                let measurements = self.catalog.table(&name).ok().map(TableMeasurements::read);
                if self.catalog.drop_table_if_exists(&name) {
                    self.measurements.subtract(
                        measurements.expect("the conditionally dropped table was measured"),
                    );
                }
                Ok(StatementResult::Command {
                    tag: "DROP TABLE",
                    affected_rows: 0,
                })
            }
            Statement::RenameTable {
                source,
                destination,
            } => {
                self.reject_unlogged_wal_mutation(&source, "RENAME TABLE")?;
                self.catalog.rename_table(&source, destination)?;
                Ok(StatementResult::Command {
                    tag: "RENAME TABLE",
                    affected_rows: 0,
                })
            }
            Statement::RenameColumn {
                table,
                source,
                destination,
            } => {
                self.reject_unlogged_wal_mutation(&table, "ALTER TABLE RENAME COLUMN")?;
                self.catalog.rename_column(&table, &source, destination)?;
                Ok(StatementResult::Command {
                    tag: "ALTER TABLE",
                    affected_rows: 0,
                })
            }
            Statement::AddColumn { table, column } => {
                self.execute_add_column_statement(table, column)
            }
            Statement::AddColumnIfNotExists { table, column } => {
                let column_exists = self
                    .catalog
                    .table(&table)?
                    .schema()
                    .iter()
                    .any(|field| field.name.eq_ignore_ascii_case(&column.name));
                if column_exists {
                    Ok(StatementResult::Command {
                        tag: "ALTER TABLE",
                        affected_rows: 0,
                    })
                } else {
                    self.execute_add_column_statement(table, column)
                }
            }
            Statement::AddNullableInt64Column { table, column } => {
                self.execute_add_nullable_int64_column_statement(table, column)
            }
            Statement::AddNullableInt64ColumnIfNotExists { table, column } => {
                let column_exists = self
                    .catalog
                    .table(&table)?
                    .schema()
                    .iter()
                    .any(|field| field.name.eq_ignore_ascii_case(&column));
                if column_exists {
                    Ok(StatementResult::Command {
                        tag: "ALTER TABLE",
                        affected_rows: 0,
                    })
                } else {
                    self.execute_add_nullable_int64_column_statement(table, column)
                }
            }
            Statement::DropColumn { table, column } => {
                self.reject_unlogged_wal_mutation(&table, "ALTER TABLE DROP COLUMN")?;
                self.table_mut(&table)?.drop_column(&column)?;
                Ok(StatementResult::Command {
                    tag: "ALTER TABLE",
                    affected_rows: 0,
                })
            }
            Statement::AlterUpdate {
                table,
                target_column,
                value,
                predicate_column,
                predicate_value,
            } => self.execute_alter_update_statement(
                table,
                target_column,
                AlterUpdateLiteral::Int64(value).into(),
                predicate_column,
                AlterUpdateLiteral::Int64(predicate_value).into(),
                alter_update_limits,
            ),
            Statement::AlterUpdateTyped {
                table,
                target_column,
                value,
                predicate_column,
                predicate_value,
            } => self.execute_alter_update_statement(
                table,
                target_column,
                value.into(),
                predicate_column,
                predicate_value.into(),
                alter_update_limits,
            ),
            Statement::AlterUpdateOwned {
                table,
                target_column,
                value,
                predicate_column,
                predicate_value,
            } => self.execute_alter_update_statement(
                table,
                target_column,
                value,
                predicate_column,
                predicate_value,
                alter_update_limits,
            ),
            Statement::TruncateTable { name } => {
                self.catalog.table(&name)?;
                self.log_int64_truncate(&name)?;
                let affected_rows = self.table_mut(&name)?.truncate();
                Ok(StatementResult::Command {
                    tag: "TRUNCATE TABLE",
                    affected_rows,
                })
            }
            Statement::Delete {
                table,
                column,
                literal,
            } => self.execute_delete_statement(
                table,
                comparison_predicate(column, ComparisonOperator::Equal, literal),
                query_result_limits,
            ),
            Statement::DeleteComparison {
                table,
                column,
                operator,
                literal,
            } => self.execute_delete_statement(
                table,
                comparison_predicate(column, operator, literal),
                query_result_limits,
            ),
            Statement::DeleteConjunction {
                table,
                first,
                second,
            } => self.execute_delete_statement(
                table,
                Predicate::And(
                    Box::new(delete_comparison_predicate(first)),
                    Box::new(delete_comparison_predicate(second)),
                ),
                query_result_limits,
            ),
            Statement::DeleteNullness {
                table,
                column,
                is_null,
            } => self.execute_delete_statement(
                table,
                if is_null {
                    Predicate::IsNull { column }
                } else {
                    Predicate::IsNotNull { column }
                },
                query_result_limits,
            ),
            Statement::Insert { table, rows } => self.execute_insert_statement(table, None, rows),
            Statement::InsertWithColumns {
                table,
                columns,
                rows,
            } => self.execute_insert_statement(table, Some(columns), rows),
            statement @ (Statement::LiteralSelect(_)
            | Statement::VersionSelect(_)
            | Statement::CurrentDatabaseSelect(_)
            | Statement::SystemDatabases
            | Statement::SystemTables
            | Statement::SystemColumns
            | Statement::SystemMetrics
            | Statement::SystemSettings
            | Statement::SystemFunctions
            | Statement::Select(_)
            | Statement::CrossJoin(_)
            | Statement::UnionAll { .. }
            | Statement::UnionDistinct { .. }
            | Statement::ShowDatabases
            | Statement::ShowSettings
            | Statement::ShowFunctions
            | Statement::ShowTables
            | Statement::ShowCreateTable { .. }
            | Statement::DescribeTable { .. }
            | Statement::ExistsTable { .. }) => self
                .execute_query_statement_with_limits(
                    statement,
                    query_result_limits,
                    self.global_aggregate_parallelism,
                )
                .map(StatementResult::Query),
        }
    }

    fn execute_query_statement_with_limits(
        &self,
        statement: Statement,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
    ) -> Result<QueryResult> {
        match statement {
            Statement::LiteralSelect(select) => {
                self.execute_literal_select(select, query_result_limits)
            }
            Statement::VersionSelect(select) => {
                self.execute_version_select(select, query_result_limits)
            }
            Statement::CurrentDatabaseSelect(select) => {
                self.execute_current_database_select(select, query_result_limits)
            }
            Statement::SystemDatabases => self.execute_system_databases(query_result_limits),
            Statement::SystemTables => self.execute_system_tables(query_result_limits),
            Statement::SystemColumns => self.execute_system_columns(query_result_limits),
            Statement::SystemMetrics => self.execute_system_metrics(query_result_limits),
            Statement::SystemSettings => self.execute_system_settings(query_result_limits),
            Statement::SystemFunctions => self.execute_system_functions(query_result_limits),
            Statement::Select(select) => self.execute_select(
                select,
                query_result_limits,
                global_aggregate_parallelism,
            ),
            Statement::CrossJoin(cross_join) => {
                self.execute_cross_join(cross_join, query_result_limits)
            }
            Statement::UnionAll { left, right } => {
                self.execute_union_all(
                    left,
                    right,
                    query_result_limits,
                    global_aggregate_parallelism,
                )
            }
            Statement::UnionDistinct { left, right } => {
                self.execute_union_distinct(
                    left,
                    right,
                    query_result_limits,
                    global_aggregate_parallelism,
                )
            }
            Statement::ShowDatabases => self.execute_show_databases(query_result_limits),
            Statement::ShowSettings => self.execute_show_settings(query_result_limits),
            Statement::ShowFunctions => self.execute_show_functions(query_result_limits),
            Statement::ShowTables => self.execute_show_tables(query_result_limits),
            Statement::ShowCreateTable { name } => {
                self.execute_show_create_table(&name, query_result_limits)
            }
            Statement::DescribeTable { name } => {
                self.execute_describe_table(&name, query_result_limits)
            }
            Statement::ExistsTable { name } => {
                self.execute_exists_table(&name, query_result_limits)
            }
            Statement::CreateTable { .. }
            | Statement::CreateTableIfNotExists { .. }
            | Statement::CreateNullableInt64Table { .. }
            | Statement::CreateNullableInt64TableIfNotExists { .. }
            | Statement::CreateTableWithTrailingNullableInt64 { .. }
            | Statement::CreateTableWithTrailingNullableInt64IfNotExists { .. }
            | Statement::CreateTableWithTwoTrailingNullableInt64 { .. }
            | Statement::CreateTableWithTwoTrailingNullableInt64IfNotExists { .. }
            | Statement::DropTable { .. }
            | Statement::DropTableIfExists { .. }
            | Statement::RenameTable { .. }
            | Statement::RenameColumn { .. }
            | Statement::AddColumn { .. }
            | Statement::AddColumnIfNotExists { .. }
            | Statement::AddNullableInt64Column { .. }
            | Statement::AddNullableInt64ColumnIfNotExists { .. }
            | Statement::DropColumn { .. }
            | Statement::AlterUpdate { .. }
            | Statement::AlterUpdateTyped { .. }
            | Statement::AlterUpdateOwned { .. }
            | Statement::TruncateTable { .. }
            | Statement::Delete { .. }
            | Statement::DeleteComparison { .. }
            | Statement::DeleteConjunction { .. }
            | Statement::DeleteNullness { .. }
            | Statement::Insert { .. }
            | Statement::InsertWithColumns { .. } => Err(Error::InvalidQuery(
                "read-only execution accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE"
                    .to_owned(),
            )),
        }
    }

    fn execute_add_column_statement(
        &mut self,
        table: String,
        column: ColumnDef,
    ) -> Result<StatementResult> {
        self.reject_unlogged_wal_mutation(&table, "ALTER TABLE ADD COLUMN")?;
        let existing = self.catalog.table(&table)?;
        validate_show_create_addition(existing, &column, false)?;
        self.table_mut(&table)?.add_column(column)?;
        Ok(StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        })
    }

    fn execute_add_nullable_int64_column_statement(
        &mut self,
        table: String,
        column: String,
    ) -> Result<StatementResult> {
        self.reject_unlogged_wal_mutation(&table, "ALTER TABLE ADD COLUMN")?;
        let field = ColumnDef {
            name: column.clone(),
            data_type: DataType::Int64,
        };
        let existing = self.catalog.table(&table)?;
        validate_show_create_addition(existing, &field, true)?;
        self.table_mut(&table)?.add_nullable_int64_column(column)?;
        Ok(StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        })
    }

    fn execute_insert_statement(
        &mut self,
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    ) -> Result<StatementResult> {
        let incoming_rows = rows.len();
        let rows = self.catalog.table(&table)?.prepare_insert_rows(
            columns.as_deref(),
            rows,
            incoming_rows,
        )?;
        let affected_rows = rows.len();
        self.log_prepared_int64_append(&table, &rows)?;
        self.table_mut(&table)?.append_prepared_insert_rows(rows);
        Ok(StatementResult::Command {
            tag: "INSERT",
            affected_rows,
        })
    }

    fn execute_delete_statement(
        &mut self,
        table: String,
        predicate: Predicate,
        query_result_limits: QueryResultLimits,
    ) -> Result<StatementResult> {
        self.reject_unlogged_wal_mutation(&table, "DELETE")?;
        let row_indexes = {
            let target = self.catalog.table(&table)?;
            let predicate = compile_predicate(target, &predicate)?;
            enforce_scan_limit(target, query_result_limits, "DELETE scanned rows")?;
            (0..target.row_count())
                .filter(|row| predicate.evaluate(target, *row))
                .collect::<Vec<_>>()
        };

        let affected_rows = self
            .table_mut(&table)
            .expect("DELETE target was resolved before its bounded scan")
            .delete_rows(&row_indexes)?;
        Ok(StatementResult::Command {
            tag: "DELETE",
            affected_rows,
        })
    }

    fn execute_alter_update_statement(
        &mut self,
        table: String,
        target_column: String,
        value: AlterUpdateValue,
        predicate_column: String,
        predicate_value: AlterUpdateValue,
        query_result_limits: QueryResultLimits,
    ) -> Result<StatementResult> {
        let replacements = {
            let target = self.catalog.table(&table)?;
            let target_index = target.column_index(&target_column)?;
            let predicate_index = target.column_index(&predicate_column)?;
            if matches!(&predicate_value, AlterUpdateValue::Null) {
                return Err(Error::InvalidQuery(
                    "ALTER TABLE UPDATE WHERE comparison does not accept NULL".to_owned(),
                ));
            }
            if matches!(&value, AlterUpdateValue::Null)
                && !target.column_is_nullable_int64(target_index)
            {
                return Err(Error::TypeMismatch {
                    context: format!(
                        "ALTER TABLE UPDATE target column '{}.{target_column}'",
                        target.name()
                    ),
                    expected: target.schema()[target_index].data_type.to_string(),
                    actual: "NULL".to_owned(),
                });
            }
            for (column, index, literal, role) in [
                (&target_column, target_index, &value, "target"),
                (
                    &predicate_column,
                    predicate_index,
                    &predicate_value,
                    "WHERE",
                ),
            ] {
                let actual = target.schema()[index].data_type;
                let expected = literal.data_type();
                if actual != expected {
                    return Err(Error::TypeMismatch {
                        context: format!(
                            "ALTER TABLE UPDATE {role} column '{}.{column}'",
                            target.name()
                        ),
                        expected: expected.to_string(),
                        actual: actual.to_string(),
                    });
                }
            }
            for (literal, role) in [(&value, "assignment"), (&predicate_value, "WHERE")] {
                if matches!(
                    literal,
                    AlterUpdateValue::Literal(AlterUpdateLiteral::Float64(value))
                        if !value.is_finite()
                ) {
                    return Err(Error::InvalidQuery(format!(
                        "ALTER TABLE UPDATE {role} Float64 literal must be finite"
                    )));
                }
            }
            enforce_scan_limit(
                target,
                query_result_limits,
                "ALTER TABLE UPDATE scanned rows",
            )?;

            let predicate_values = &target.columns()[predicate_index];
            let string_match_count = if let AlterUpdateValue::String(string) = &value {
                let match_count = (0..target.row_count())
                    .filter(|&row| alter_update_matches(predicate_values, &predicate_value, row))
                    .count();
                enforce_alter_update_replacement_bytes(
                    string.len(),
                    match_count,
                    query_result_limits.max_bytes,
                )?;
                Some(match_count)
            } else {
                None
            };

            let replacement = value.value();
            let mut replacements = string_match_count.map_or_else(Vec::new, Vec::with_capacity);
            for row in 0..target.row_count() {
                if alter_update_matches(predicate_values, &predicate_value, row) {
                    replacements.push((row, replacement.clone()));
                }
            }
            replacements
        };

        if self.wal_tracks(&table) {
            let wal_replacements = replacements
                .iter()
                .map(|(row, value)| match value {
                    Value::Int64(value) => (*row, Some(*value)),
                    Value::Null(DataType::Int64) => (*row, None),
                    _ => unreachable!("WAL opt-in guarantees one Int64 target column"),
                })
                .collect::<Vec<_>>();
            self.log_int64_replacements(&table, &wal_replacements)?;
        }
        let affected_rows = self
            .table_mut(&table)
            .expect("ALTER TABLE UPDATE target was resolved before its bounded scan")
            .replace_column_values(&target_column, replacements)?;
        Ok(StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows,
        })
    }

    fn execute_literal_select(
        &self,
        select: LiteralSelect,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        let LiteralSelect { value, alias } = select;
        validate_literal_select_value(&value)?;
        let column_name_bytes = alias
            .as_ref()
            .map_or_else(|| literal_result_name_len(&value), String::len);
        let mut bytes = validate_result_shape_parts(
            1,
            1,
            1,
            column_name_bytes,
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        if let Value::String(value) = &value {
            bytes = bytes.saturating_add(value.len());
            enforce_resource_limit(
                SELECT_RESULT_RESOURCES.bytes,
                bytes,
                query_result_limits.max_bytes,
            )?;
        }
        let columns = vec![ResultColumn {
            name: alias.unwrap_or_else(|| literal_result_name(&value)),
            data_type: value.data_type(),
        }];

        Ok(QueryResult {
            columns,
            rows: vec![vec![value]],
        })
    }

    fn execute_version_select(
        &self,
        select: VersionSelect,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "version()";
        const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

        let column_name = select
            .alias
            .unwrap_or_else(|| RESULT_COLUMN_NAME.to_owned());
        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            column_name.len(),
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(PACKAGE_VERSION.len()),
            query_result_limits.max_bytes,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: column_name,
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(PACKAGE_VERSION.to_owned())]],
        })
    }

    fn execute_current_database_select(
        &self,
        select: CurrentDatabaseSelect,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "currentDatabase()";
        const DATABASE_NAME: &str = "default";

        let column_name = select
            .alias
            .unwrap_or_else(|| RESULT_COLUMN_NAME.to_owned());
        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            column_name.len(),
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(DATABASE_NAME.len()),
            query_result_limits.max_bytes,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: column_name,
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(DATABASE_NAME.to_owned())]],
        })
    }

    fn execute_show_databases(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_databases(query_result_limits, SHOW_DATABASES_RESULT_RESOURCES)
    }

    fn execute_system_databases(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_databases(query_result_limits, SELECT_RESULT_RESOURCES)
    }

    fn execute_databases(
        &self,
        query_result_limits: QueryResultLimits,
        result_resources: QueryResultResources,
    ) -> Result<QueryResult> {
        const DATABASE_NAME: &str = "default";
        const RESULT_COLUMN_NAME: &str = "name";

        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            result_resources,
        )?;
        enforce_resource_limit(
            result_resources.bytes,
            fixed_bytes.saturating_add(DATABASE_NAME.len()),
            query_result_limits.max_bytes,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(DATABASE_NAME.to_owned())]],
        })
    }

    fn execute_show_settings(&self, query_result_limits: QueryResultLimits) -> Result<QueryResult> {
        self.execute_settings(query_result_limits, SHOW_SETTINGS_RESULT_RESOURCES)
    }

    fn execute_system_settings(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_settings(query_result_limits, SELECT_RESULT_RESOURCES)
    }

    fn execute_settings(
        &self,
        query_result_limits: QueryResultLimits,
        result_resources: QueryResultResources,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_COUNT: usize = 2;
        const RESULT_COLUMN_NAME_BYTES: usize = "name".len() + "value".len();

        let configured_query_limits = self.query_result_limits;
        let configured_table_limits = self.table_limits;
        let global_aggregate_worker_cap = self.global_aggregate_worker_cap().get();
        let settings = [
            (
                "query_result_limits.max_scan_rows",
                configured_query_limits.max_scan_rows,
            ),
            (
                "query_result_limits.max_rows",
                configured_query_limits.max_rows,
            ),
            (
                "query_result_limits.max_values",
                configured_query_limits.max_values,
            ),
            (
                "query_result_limits.max_bytes",
                configured_query_limits.max_bytes,
            ),
            (
                "query_result_limits.max_ordering_state_bytes",
                configured_query_limits.max_ordering_state_bytes,
            ),
            (
                "query_result_limits.max_groups",
                configured_query_limits.max_groups,
            ),
            (
                "query_result_limits.max_group_key_cells",
                configured_query_limits.max_group_key_cells,
            ),
            (
                "query_result_limits.max_group_key_bytes",
                configured_query_limits.max_group_key_bytes,
            ),
            (
                "query_result_limits.max_aggregate_state_cells",
                configured_query_limits.max_aggregate_state_cells,
            ),
            (
                "query_result_limits.max_aggregate_state_bytes",
                configured_query_limits.max_aggregate_state_bytes,
            ),
            ("table_limits.max_rows", configured_table_limits.max_rows),
            (
                "table_limits.max_columns",
                configured_table_limits.max_columns,
            ),
            ("table_limits.max_cells", configured_table_limits.max_cells),
            ("global_aggregate_worker_cap", global_aggregate_worker_cap),
        ];

        let fixed_bytes = validate_result_shape_parts(
            settings.len(),
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_NAME_BYTES,
            query_result_limits,
            result_resources,
        )?;
        let value_bytes = settings
            .iter()
            .map(|(name, value)| name.len().saturating_add(usize_decimal_len(*value)))
            .fold(0_usize, usize::saturating_add);
        enforce_resource_limit(
            result_resources.bytes,
            fixed_bytes.saturating_add(value_bytes),
            query_result_limits.max_bytes,
        )?;

        let columns = vec![
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "value".to_owned(),
                data_type: DataType::String,
            },
        ];
        let rows = settings
            .into_iter()
            .map(|(name, value)| {
                vec![
                    Value::String(name.to_owned()),
                    Value::String(value.to_string()),
                ]
            })
            .collect();

        Ok(QueryResult { columns, rows })
    }

    fn execute_show_functions(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_functions(query_result_limits, SHOW_FUNCTIONS_RESULT_RESOURCES)
    }

    fn execute_system_functions(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_functions(query_result_limits, SELECT_RESULT_RESOURCES)
    }

    fn execute_functions(
        &self,
        query_result_limits: QueryResultLimits,
        result_resources: QueryResultResources,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "name";

        let fixed_bytes = validate_result_shape_parts(
            SUPPORTED_FUNCTION_NAMES.len(),
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            result_resources,
        )?;
        let function_name_bytes = SUPPORTED_FUNCTION_NAMES
            .iter()
            .map(|name| name.len())
            .fold(0_usize, usize::saturating_add);
        enforce_resource_limit(
            result_resources.bytes,
            fixed_bytes.saturating_add(function_name_bytes),
            query_result_limits.max_bytes,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::String,
            }],
            rows: SUPPORTED_FUNCTION_NAMES
                .iter()
                .map(|name| vec![Value::String((*name).to_owned())])
                .collect(),
        })
    }

    fn execute_show_tables(&self, query_result_limits: QueryResultLimits) -> Result<QueryResult> {
        let table_count = self.catalog.table_count();
        let columns = vec![ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }];
        let fixed_bytes = validate_result_shape(
            table_count,
            1,
            &columns,
            query_result_limits,
            SHOW_TABLES_RESULT_RESOURCES,
        )?;
        let table_name_bytes = self.catalog.table_name_bytes();
        let bytes = fixed_bytes.saturating_add(table_name_bytes);
        enforce_resource_limit(
            SHOW_TABLES_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let names = self.catalog.table_names();
        debug_assert_eq!(names.len(), table_count);

        Ok(QueryResult {
            columns,
            rows: names
                .into_iter()
                .map(|name| vec![Value::String(name.to_owned())])
                .collect(),
        })
    }

    fn execute_system_tables(&self, query_result_limits: QueryResultLimits) -> Result<QueryResult> {
        const DATABASE_NAME: &str = "default";
        const ENGINE_NAME: &str = "Memory";

        let table_count = self.catalog.table_count();
        let columns = vec![
            ResultColumn {
                name: "database".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "engine".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "total_rows".to_owned(),
                data_type: DataType::Int64,
            },
        ];
        let fixed_bytes = validate_result_shape(
            table_count,
            columns.len(),
            &columns,
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        let string_bytes = self.catalog.table_name_bytes().saturating_add(
            table_count.saturating_mul(DATABASE_NAME.len().saturating_add(ENGINE_NAME.len())),
        );
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(string_bytes),
            query_result_limits.max_bytes,
        )?;

        let rows = self
            .catalog
            .table_row_counts()
            .into_iter()
            .map(|(name, row_count)| {
                let row_count = i64::try_from(row_count)
                    .map_err(|_| Error::NumericOverflow("system.tables total_rows".to_owned()))?;
                Ok(vec![
                    Value::String(DATABASE_NAME.to_owned()),
                    Value::String(name.to_owned()),
                    Value::String(ENGINE_NAME.to_owned()),
                    Value::Int64(row_count),
                ])
            })
            .collect::<Result<Vec<_>>>()?;
        debug_assert_eq!(rows.len(), table_count);

        Ok(QueryResult { columns, rows })
    }

    fn execute_system_columns(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const DATABASE_NAME: &str = "default";
        const RESULT_COLUMN_COUNT: usize = 5;
        const RESULT_COLUMN_NAME_BYTES: usize =
            "database".len() + "table".len() + "name".len() + "type".len() + "position".len();

        let row_count = self.catalog.column_count();
        let fixed_bytes = validate_result_shape_parts(
            row_count,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_NAME_BYTES,
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        let string_bytes = self.catalog.system_column_string_bytes(DATABASE_NAME);
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(string_bytes),
            query_result_limits.max_bytes,
        )?;
        i64::try_from(self.catalog.max_column_position())
            .map_err(|_| Error::NumericOverflow("system.columns position".to_owned()))?;

        // All result shape, payload, retained-result, and numeric conversion
        // checks finish before allocating the result schema, ordering scratch,
        // rows, or owned scalar payloads.
        let columns = vec![
            ResultColumn {
                name: "database".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "table".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "type".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "position".to_owned(),
                data_type: DataType::Int64,
            },
        ];
        let rows = self
            .catalog
            .tables_in_name_order()
            .into_iter()
            .flat_map(|table| {
                table.schema().iter().zip(table.columns()).enumerate().map(
                    move |(index, (column, values))| {
                        let position = index
                            .checked_add(1)
                            .and_then(|position| i64::try_from(position).ok())
                            .expect("system.columns positions were preflighted");
                        vec![
                            Value::String(DATABASE_NAME.to_owned()),
                            Value::String(table.name().to_owned()),
                            Value::String(column.name.clone()),
                            Value::String(values.metadata_type_name().to_owned()),
                            Value::Int64(position),
                        ]
                    },
                )
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(rows.len(), row_count);

        Ok(QueryResult { columns, rows })
    }

    fn execute_system_metrics(
        &self,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const METRIC_NAMES: [&str; 6] = [
            "rusthouse_tables",
            "rusthouse_columns",
            "rusthouse_retained_rows",
            "rusthouse_retained_value_bytes",
            "rusthouse_index_scanned_blocks",
            "rusthouse_index_pruned_blocks",
        ];
        const RESULT_COLUMN_COUNT: usize = 2;
        const RESULT_COLUMN_NAME_BYTES: usize = "metric".len() + "value".len();

        let fixed_bytes = validate_result_shape_parts(
            METRIC_NAMES.len(),
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_NAME_BYTES,
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        let metric_name_bytes = METRIC_NAMES
            .iter()
            .map(|metric| metric.len())
            .fold(0_usize, usize::saturating_add);
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(metric_name_bytes),
            query_result_limits.max_bytes,
        )?;
        let index_metrics = self.index_pruning_metrics();
        let raw_values = [
            self.catalog.table_count() as u128,
            self.measurements.column_count,
            self.measurements.retained_row_count,
            self.measurements.retained_value_bytes,
            index_metrics.scanned_blocks as u128,
            index_metrics.pruned_blocks as u128,
        ];
        let mut values = [0_i64; METRIC_NAMES.len()];
        for ((value, raw_value), metric) in values.iter_mut().zip(raw_values).zip(METRIC_NAMES) {
            *value = checked_system_metric_value(metric, raw_value)?;
        }

        // Shape, payload, retained-result, and integer-conversion checks finish
        // before allocating the result schema, rows, or owned metric names.
        Ok(QueryResult {
            columns: vec![
                ResultColumn {
                    name: "metric".to_owned(),
                    data_type: DataType::String,
                },
                ResultColumn {
                    name: "value".to_owned(),
                    data_type: DataType::Int64,
                },
            ],
            rows: METRIC_NAMES
                .into_iter()
                .zip(values)
                .map(|(metric, value)| vec![Value::String(metric.to_owned()), Value::Int64(value)])
                .collect(),
        })
    }

    fn execute_show_create_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "statement";

        let table = self.catalog.table(name)?;
        let ddl_bytes = create_table_ddl_len(table);
        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            SHOW_CREATE_TABLE_RESULT_RESOURCES,
        )?;
        let bytes = fixed_bytes.saturating_add(ddl_bytes);
        enforce_resource_limit(
            SHOW_CREATE_TABLE_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let mut ddl = String::with_capacity(ddl_bytes);
        ddl.push_str("CREATE TABLE ");
        ddl.push_str(table.name());
        ddl.push_str(" (");
        let create_columns = show_create_column_count(table);
        for (index, (field, values)) in table
            .schema()
            .iter()
            .zip(table.columns())
            .take(create_columns)
            .enumerate()
        {
            if index != 0 {
                ddl.push_str(", ");
            }
            ddl.push_str(&field.name);
            ddl.push(' ');
            ddl.push_str(values.metadata_type_name());
        }
        ddl.push(')');
        if create_columns != table.schema().len() {
            for (field, values) in table
                .schema()
                .iter()
                .zip(table.columns())
                .skip(create_columns)
            {
                ddl.push_str("; ALTER TABLE ");
                ddl.push_str(table.name());
                ddl.push_str(" ADD COLUMN ");
                ddl.push_str(&field.name);
                ddl.push(' ');
                ddl.push_str(values.metadata_type_name());
            }
        }
        debug_assert_eq!(ddl.len(), ddl_bytes);

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(ddl)]],
        })
    }

    fn execute_describe_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_COUNT: usize = 2;
        const RESULT_COLUMN_NAME_BYTES: usize = "name".len() + "type".len();

        let table = self.catalog.table(name)?;
        let row_count = table.schema().len();
        let fixed_bytes = validate_result_shape_parts(
            row_count,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_NAME_BYTES,
            query_result_limits,
            DESCRIBE_TABLE_RESULT_RESOURCES,
        )?;
        let value_bytes = table
            .schema()
            .iter()
            .zip(table.columns())
            .map(|(field, values)| {
                field
                    .name
                    .len()
                    .saturating_add(values.metadata_type_name().len())
            })
            .fold(0_usize, usize::saturating_add);
        let bytes = fixed_bytes.saturating_add(value_bytes);
        enforce_resource_limit(
            DESCRIBE_TABLE_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let columns = vec![
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "type".to_owned(),
                data_type: DataType::String,
            },
        ];
        let rows = table
            .schema()
            .iter()
            .zip(table.columns())
            .map(|(field, values)| {
                vec![
                    Value::String(field.name.clone()),
                    Value::String(values.metadata_type_name().to_owned()),
                ]
            })
            .collect();

        Ok(QueryResult { columns, rows })
    }

    fn execute_exists_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "result";

        validate_result_shape_parts(
            1,
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            EXISTS_TABLE_RESULT_RESOURCES,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::Bool,
            }],
            rows: vec![vec![Value::Bool(self.catalog.table_exists(name))]],
        })
    }

    fn execute_select(
        &self,
        select: Select,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
    ) -> Result<QueryResult> {
        self.execute_select_with_prefix(
            select,
            query_result_limits,
            global_aggregate_parallelism,
            None,
        )
    }

    fn execute_select_with_prefix(
        &self,
        select: Select,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
        result_prefix: Option<SelectResultPrefix<'_>>,
    ) -> Result<QueryResult> {
        validate_distinct_shape(&select)?;
        validate_row_number_shape(&select)?;
        validate_offset_shape(&select)?;
        let selection_limit = checked_selection_limit(select.limit, select.offset)?;
        let table = self.catalog.table(&select.table)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;
        let group_columns = if select.distinct {
            resolve_distinct_columns(table, &select.items)?
        } else {
            resolve_group_columns(table, &select.group_by)?
        };
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let window_ordering = resolve_row_number_ordering(table, &select.items)?;
        let having = select
            .having
            .as_ref()
            .map(|having| resolve_having(&result_columns, &items, &aggregate_specs, having))
            .transpose()?;
        let ordering = resolve_ordering(
            table,
            &items,
            &aggregate_specs,
            &result_columns,
            &select.order_by,
        )?;
        if let Some(prefix) = result_prefix {
            // Reject a UNION schema mismatch before scanning or materializing
            // any rows from its right operand.
            validate_union_schema(prefix.operation, prefix.columns, &result_columns)?;
        }

        // Validated range partitions can reduce the source rows charged to
        // the scan limit. A sparse index can then narrow physical candidates,
        // but does not reduce that charge for an ordinary table.
        let int64_partition_filter = predicate
            .as_ref()
            .and_then(CompiledPredicate::int64_partition_filter);
        let int64_index_filter = predicate
            .as_ref()
            .and_then(CompiledPredicate::int64_index_filter);
        let int64_nullness = predicate
            .as_ref()
            .and_then(CompiledPredicate::int64_nullness);
        let source_rows = int64_partition_filter
            .and_then(|(column, filter)| table.int64_range_partition_rows(column, filter))
            .unwrap_or(0..table.row_count());
        enforce_select_scan_rows(source_rows.len(), query_result_limits)?;
        let indexed_scan = int64_index_filter
            .and_then(|(column, filter)| {
                table.int64_min_max_index_scan(column, filter, source_rows.clone())
            })
            .or_else(|| {
                int64_nullness.and_then(|(column, is_null)| {
                    table.int64_min_max_nullness_index_scan(column, is_null, source_rows.clone())
                })
            });
        let candidate_ranges = if let Some(scan) = indexed_scan {
            self.index_pruning_counters.record(&scan);
            scan.ranges
        } else {
            vec![source_rows]
        };
        let candidate_rows = || candidate_ranges.iter().flat_map(|range| range.clone());
        let row_matches = |row| {
            predicate
                .as_ref()
                .is_none_or(|predicate| predicate.evaluate(table, row))
        };
        let has_row_number = items
            .iter()
            .any(|item| matches!(item, ResolvedItem::RowNumber));
        let mut matching_rows = if window_ordering.is_some() {
            // Ordered ROW_NUMBER needs every filtered source index to produce
            // deterministic ties. Count without retaining indices so the
            // complete state can be rejected before its first allocation.
            let matching_row_count = candidate_rows().filter(|row| row_matches(*row)).count();
            validate_row_number_count(matching_row_count)?;
            let ordering_state_bytes =
                matching_row_count.saturating_mul(ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES);
            enforce_resource_limit(
                "SELECT ordering state bytes",
                ordering_state_bytes,
                query_result_limits.max_ordering_state_bytes,
            )?;

            let mut rows = Vec::with_capacity(matching_row_count);
            rows.extend(candidate_rows().filter(|row| row_matches(*row)));
            debug_assert_eq!(rows.len(), matching_row_count);
            rows
        } else {
            candidate_rows()
                .filter(|row| row_matches(*row))
                .collect::<Vec<_>>()
        };
        if has_row_number && window_ordering.is_none() {
            validate_row_number_count(matching_rows.len())?;
        }
        if let Some(ordering) = window_ordering {
            order_window_rows(&mut matching_rows, table, ordering, selection_limit);
        }

        let grouped = select.distinct || !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                &matching_rows,
                &group_columns,
                &aggregate_specs,
                query_result_limits,
                global_aggregate_parallelism,
            )?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            if let Some(having) = having {
                selected_groups.retain(|group| having.evaluate(&grouped, *group));
            }
            if select.distinct {
                if ordering.is_empty() {
                    if let Some(limit) = selection_limit {
                        selected_groups.truncate(limit);
                    }
                } else {
                    order_grouped_rows(
                        &mut selected_groups,
                        &grouped,
                        &items,
                        &ordering,
                        selection_limit,
                    );
                }
            } else {
                order_grouped_rows(
                    &mut selected_groups,
                    &grouped,
                    &items,
                    &ordering,
                    selection_limit,
                );
            }
            apply_offset(&mut selected_groups, select.offset.unwrap_or(0));
            validate_grouped_result_limits(
                &grouped,
                &selected_groups,
                &items,
                &result_columns,
                query_result_limits,
                result_prefix,
            )?;
            grouped.project(&selected_groups, &items)
        } else {
            order_source_rows(
                &mut matching_rows,
                table,
                &items,
                &ordering,
                selection_limit,
                query_result_limits.max_ordering_state_bytes,
            )?;
            apply_offset(&mut matching_rows, select.offset.unwrap_or(0));
            validate_projection_result_limits(
                table,
                &matching_rows,
                &items,
                &result_columns,
                query_result_limits,
                result_prefix,
            )?;
            execute_projection(table, &matching_rows, &items)?
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }

    fn execute_union_all(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
    ) -> Result<QueryResult> {
        self.execute_union_operands(
            left,
            right,
            query_result_limits,
            global_aggregate_parallelism,
            "UNION ALL",
        )
    }

    fn execute_union_distinct(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
    ) -> Result<QueryResult> {
        let mut result = self.execute_union_operands(
            left,
            right,
            query_result_limits,
            global_aggregate_parallelism,
            "UNION DISTINCT",
        )?;
        deduplicate_union_rows(&mut result.rows, result.columns.len(), query_result_limits)?;
        Ok(result)
    }

    fn execute_union_operands(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
        global_aggregate_parallelism: GlobalAggregateParallelism,
        operation: &'static str,
    ) -> Result<QueryResult> {
        let mut left_result =
            self.execute_select(left, query_result_limits, global_aggregate_parallelism)?;
        let mut right_result = self.execute_select_with_prefix(
            right,
            query_result_limits,
            global_aggregate_parallelism,
            Some(SelectResultPrefix::from_result(&left_result, operation)),
        )?;

        left_result.rows.append(&mut right_result.rows);
        Ok(left_result)
    }

    fn execute_cross_join(
        &self,
        cross_join: CrossJoin,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        let left = self.catalog.table(&cross_join.left_table)?;
        let right = self.catalog.table(&cross_join.right_table)?;
        enforce_select_scan_limit(left, query_result_limits)?;
        enforce_select_scan_limit(right, query_result_limits)?;
        let columns = left
            .schema()
            .iter()
            .chain(right.schema())
            .map(|field| ResultColumn {
                name: field.name.clone(),
                data_type: field.data_type,
            })
            .collect::<Vec<_>>();
        let row_count =
            limited_cartesian_row_count(left.row_count(), right.row_count(), cross_join.limit)?;
        validate_cross_join_result_limits(left, right, row_count, &columns, query_result_limits)?;

        let column_count = columns.len();
        let mut rows = Vec::with_capacity(row_count);
        'left_rows: for left_row in 0..left.row_count() {
            for right_row in 0..right.row_count() {
                if rows.len() == row_count {
                    break 'left_rows;
                }
                let mut row = Vec::with_capacity(column_count);
                row.extend(left.columns().iter().map(|column| column.value(left_row)));
                row.extend(right.columns().iter().map(|column| column.value(right_row)));
                rows.push(row);
            }
        }

        Ok(QueryResult { columns, rows })
    }
}

fn alter_update_matches(column: &Column, literal: &AlterUpdateValue, row: usize) -> bool {
    match (column, literal) {
        (Column::Int64(values), AlterUpdateValue::Literal(AlterUpdateLiteral::Int64(value))) => {
            values[row] == *value
        }
        (
            Column::NullableInt64(values),
            AlterUpdateValue::Literal(AlterUpdateLiteral::Int64(value)),
        ) => values[row] == Some(*value),
        (
            Column::Float64(values),
            AlterUpdateValue::Literal(AlterUpdateLiteral::Float64(value)),
        ) => values[row] == *value,
        (Column::Bool(values), AlterUpdateValue::Literal(AlterUpdateLiteral::Bool(value))) => {
            values[row] == *value
        }
        (Column::String(values), AlterUpdateValue::String(value)) => values[row] == *value,
        _ => unreachable!("ALTER TABLE UPDATE predicate type was validated before its scan"),
    }
}

fn checked_system_metric_value(metric: &str, value: u128) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::NumericOverflow(format!("system.metrics {metric} value")))
}

fn enforce_alter_update_replacement_bytes(
    string_bytes: usize,
    match_count: usize,
    max: usize,
) -> Result<()> {
    let actual = (string_bytes as u128).saturating_mul(match_count as u128);
    if actual > max as u128 {
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE replacement String bytes",
            actual: saturating_usize(actual),
            max,
        })
    } else {
        Ok(())
    }
}

fn statement_name(statement: &Statement) -> &'static str {
    match statement {
        Statement::CreateTable { .. }
        | Statement::CreateTableIfNotExists { .. }
        | Statement::CreateNullableInt64Table { .. }
        | Statement::CreateNullableInt64TableIfNotExists { .. }
        | Statement::CreateTableWithTrailingNullableInt64 { .. }
        | Statement::CreateTableWithTrailingNullableInt64IfNotExists { .. }
        | Statement::CreateTableWithTwoTrailingNullableInt64 { .. }
        | Statement::CreateTableWithTwoTrailingNullableInt64IfNotExists { .. } => "CREATE TABLE",
        Statement::DropTable { .. } | Statement::DropTableIfExists { .. } => "DROP TABLE",
        Statement::RenameTable { .. } => "RENAME TABLE",
        Statement::RenameColumn { .. }
        | Statement::AddColumn { .. }
        | Statement::AddColumnIfNotExists { .. }
        | Statement::AddNullableInt64Column { .. }
        | Statement::AddNullableInt64ColumnIfNotExists { .. }
        | Statement::DropColumn { .. }
        | Statement::AlterUpdate { .. }
        | Statement::AlterUpdateTyped { .. }
        | Statement::AlterUpdateOwned { .. } => "ALTER TABLE",
        Statement::TruncateTable { .. } => "TRUNCATE TABLE",
        Statement::Delete { .. }
        | Statement::DeleteComparison { .. }
        | Statement::DeleteConjunction { .. }
        | Statement::DeleteNullness { .. } => "DELETE",
        Statement::Insert { .. } | Statement::InsertWithColumns { .. } => "INSERT",
        Statement::LiteralSelect(_)
        | Statement::VersionSelect(_)
        | Statement::CurrentDatabaseSelect(_)
        | Statement::SystemDatabases
        | Statement::SystemTables
        | Statement::SystemColumns
        | Statement::SystemMetrics
        | Statement::SystemSettings
        | Statement::SystemFunctions
        | Statement::Select(_)
        | Statement::CrossJoin(_)
        | Statement::UnionAll { .. }
        | Statement::UnionDistinct { .. } => "SELECT",
        Statement::ShowDatabases => "SHOW DATABASES",
        Statement::ShowSettings => "SHOW SETTINGS",
        Statement::ShowFunctions => "SHOW FUNCTIONS",
        Statement::ShowTables => "SHOW TABLES",
        Statement::ShowCreateTable { .. } => "SHOW CREATE TABLE",
        Statement::DescribeTable { .. } => "DESCRIBE TABLE",
        Statement::ExistsTable { .. } => "EXISTS TABLE",
    }
}

fn comparison_predicate(column: String, operator: ComparisonOperator, literal: Value) -> Predicate {
    Predicate::Comparison {
        left: Operand::Column(column),
        operator,
        right: Operand::Literal(literal),
    }
}

fn delete_comparison_predicate(comparison: DeleteComparisonPredicate) -> Predicate {
    comparison_predicate(comparison.column, comparison.operator, comparison.literal)
}

fn create_table_ddl_len(table: &Table) -> usize {
    let create_columns = show_create_column_count(table);
    let fields_bytes = table
        .schema()
        .iter()
        .zip(table.columns())
        .take(create_columns)
        .map(|(field, values)| {
            field
                .name
                .len()
                .saturating_add(1)
                .saturating_add(values.metadata_type_name().len())
        })
        .fold(0_usize, usize::saturating_add);
    let delimiters = create_columns.saturating_sub(1).saturating_mul(2);
    let alter_bytes = if create_columns != table.schema().len() {
        table
            .schema()
            .iter()
            .zip(table.columns())
            .skip(create_columns)
            .map(|(field, values)| {
                "; ALTER TABLE "
                    .len()
                    .saturating_add(table.name().len())
                    .saturating_add(" ADD COLUMN ".len())
                    .saturating_add(field.name.len())
                    .saturating_add(1)
                    .saturating_add(values.metadata_type_name().len())
            })
            .fold(0_usize, usize::saturating_add)
    } else {
        0
    };

    "CREATE TABLE "
        .len()
        .saturating_add(table.name().len())
        .saturating_add(" (".len())
        .saturating_add(fields_bytes)
        .saturating_add(delimiters)
        .saturating_add(")".len())
        .saturating_add(alter_bytes)
}

fn show_create_column_count(table: &Table) -> usize {
    let Some(first_nullable) = table
        .columns()
        .iter()
        .position(|column| matches!(column, Column::NullableInt64(_)))
    else {
        return table.schema().len();
    };
    let nullable_suffix = &table.columns()[first_nullable..];
    let supported_nullable_shape = nullable_suffix.len() <= 2
        && nullable_suffix
            .iter()
            .all(|column| matches!(column, Column::NullableInt64(_)));
    if supported_nullable_shape && table.schema().len() <= sql::DEFAULT_MAX_AST_LIST_ITEMS {
        table.schema().len()
    } else {
        first_nullable.max(1)
    }
}

fn show_create_statement_count_after_addition(table: &Table, added_nullable: bool) -> usize {
    let resulting_columns = table.schema().len().saturating_add(1);
    let Some(first_nullable) = table
        .columns()
        .iter()
        .position(|column| matches!(column, Column::NullableInt64(_)))
        .or_else(|| added_nullable.then_some(table.schema().len()))
    else {
        return 1;
    };
    let nullable_suffix_len = resulting_columns.saturating_sub(first_nullable);
    let existing_suffix_is_nullable = table.columns()[first_nullable.min(table.schema().len())..]
        .iter()
        .all(|column| matches!(column, Column::NullableInt64(_)));
    let resulting_suffix_is_nullable = existing_suffix_is_nullable && added_nullable;
    let supported_nullable_shape = nullable_suffix_len <= 2 && resulting_suffix_is_nullable;
    if supported_nullable_shape && resulting_columns <= sql::DEFAULT_MAX_AST_LIST_ITEMS {
        1
    } else {
        1_usize
            .saturating_add(resulting_columns)
            .saturating_sub(first_nullable.max(1))
    }
}

fn validate_show_create_addition(
    table: &Table,
    field: &ColumnDef,
    added_nullable: bool,
) -> Result<()> {
    let statements = show_create_statement_count_after_addition(table, added_nullable);
    if statements <= sql::DEFAULT_MAX_BATCH_STATEMENTS {
        return Ok(());
    }
    table.validate_add_column(field)?;
    Err(Error::ResourceLimitExceeded {
        resource: "nullable SHOW CREATE statements",
        actual: statements,
        max: sql::DEFAULT_MAX_BATCH_STATEMENTS,
    })
}

fn literal_result_name_len(value: &Value) -> usize {
    match value {
        Value::String(value) => sql_string_literal_name_len(value),
        Value::Int64(value) => {
            let magnitude = value.unsigned_abs();
            let digits = if magnitude == 0 {
                1
            } else {
                magnitude.ilog10() as usize + 1
            };
            digits + usize::from(value.is_negative())
        }
        Value::Float64(value) => float64_result_name_len(*value),
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Null(data_type) => "CAST(NULL AS )"
            .len()
            .saturating_add(data_type.as_str().len()),
    }
}

fn sql_string_literal_name_len(value: &str) -> usize {
    value
        .len()
        .saturating_add(value.bytes().filter(|byte| *byte == b'\'').count())
        .saturating_add(2)
}

fn float64_result_name_len(value: f64) -> usize {
    #[derive(Default)]
    struct Metrics {
        bytes: usize,
        has_fraction_or_exponent: bool,
    }

    impl fmt::Write for Metrics {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.bytes = self.bytes.saturating_add(text.len());
            self.has_fraction_or_exponent |= text.contains(['.', 'e', 'E']);
            Ok(())
        }
    }

    let mut metrics = Metrics::default();
    fmt::write(&mut metrics, format_args!("{value}"))
        .expect("counting formatted Float64 bytes cannot fail");
    if value.is_finite() && !metrics.has_fraction_or_exponent {
        metrics.bytes.saturating_add(2)
    } else {
        metrics.bytes
    }
}

fn validate_literal_select_value(value: &Value) -> Result<()> {
    match value {
        Value::Float64(value) if !value.is_finite() => Err(Error::InvalidQuery(
            "literal SELECT Float64 must be finite".to_owned(),
        )),
        Value::Null(_)
        | Value::Int64(_)
        | Value::Float64(_)
        | Value::Bool(_)
        | Value::String(_) => Ok(()),
    }
}

fn literal_result_name(value: &Value) -> String {
    match value {
        Value::String(value) => {
            let mut name = String::with_capacity(sql_string_literal_name_len(value));
            name.push('\'');
            for character in value.chars() {
                name.push(character);
                if character == '\'' {
                    name.push('\'');
                }
            }
            name.push('\'');
            name
        }
        Value::Null(data_type) => format!("CAST(NULL AS {})", data_type.as_str()),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => value.as_display_string(),
    }
}

#[derive(Debug, Clone, Copy)]
struct SelectResultPrefix<'a> {
    // The final UNION result retains the left schema, not the right aliases.
    columns: &'a [ResultColumn],
    row_count: usize,
    string_bytes: usize,
    operation: &'static str,
}

impl<'a> SelectResultPrefix<'a> {
    fn from_result(result: &'a QueryResult, operation: &'static str) -> Self {
        let string_bytes = result
            .rows
            .iter()
            .flatten()
            .map(|value| match value {
                Value::String(value) => value.len(),
                Value::Null(_) | Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => 0,
            })
            .fold(0_usize, usize::saturating_add);
        Self {
            columns: &result.columns,
            row_count: result.rows.len(),
            string_bytes,
            operation,
        }
    }
}

fn validate_union_schema(
    operation: &'static str,
    left: &[ResultColumn],
    right: &[ResultColumn],
) -> Result<()> {
    if left.len() != right.len() {
        return Err(if operation == "UNION DISTINCT" {
            Error::UnionDistinctColumnCountMismatch {
                left: left.len(),
                right: right.len(),
            }
        } else {
            Error::UnionColumnCountMismatch {
                left: left.len(),
                right: right.len(),
            }
        });
    }

    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        if left.data_type != right.data_type {
            return Err(Error::TypeMismatch {
                context: format!("{operation} column {}", index + 1),
                expected: left.data_type.to_string(),
                actual: right.data_type.to_string(),
            });
        }
    }
    Ok(())
}

fn deduplicate_union_rows(
    rows: &mut Vec<Vec<Value>>,
    column_count: usize,
    limits: QueryResultLimits,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let probe_key_cells = if column_count > 2 { column_count } else { 0 };
    let first_key_cells = probe_key_cells.saturating_add(column_count);
    enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
    enforce_resource_limit(
        "SELECT group key cells",
        first_key_cells,
        limits.max_group_key_cells,
    )?;
    enforce_resource_limit(
        "SELECT group key bytes",
        first_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES),
        limits.max_group_key_bytes,
    )?;

    let retained = union_distinct_retained_rows(rows, column_count, probe_key_cells, limits)?;
    let mut row = 0;
    rows.retain(|_| {
        let keep = retained[row];
        row += 1;
        keep
    });
    // `retain` drops duplicate row contents but deliberately preserves the
    // raw UNION allocation. Rebuilding through a boxed slice makes capacity
    // equal the deduplicated length, matching retained-result accounting.
    *rows = std::mem::take(rows).into_boxed_slice().into_vec();
    debug_assert_eq!(rows.capacity(), rows.len());
    Ok(())
}

fn union_distinct_retained_rows(
    rows: &[Vec<Value>],
    column_count: usize,
    probe_key_cells: usize,
    limits: QueryResultLimits,
) -> Result<Vec<bool>> {
    let mut keys = UnionDistinctKeys::new(column_count);
    let mut probe = Vec::with_capacity(probe_key_cells);
    let mut retained = Vec::with_capacity(rows.len());
    let mut group_count = 0_usize;
    let mut group_key_cells = probe_key_cells;

    for row in rows {
        debug_assert_eq!(row.len(), column_count);
        if keys.contains(row, &mut probe) {
            retained.push(false);
            continue;
        }

        let next_group_count = group_count.saturating_add(1);
        enforce_resource_limit("SELECT groups", next_group_count, limits.max_groups)?;
        let next_key_cells = group_key_cells.saturating_add(column_count);
        enforce_resource_limit(
            "SELECT group key cells",
            next_key_cells,
            limits.max_group_key_cells,
        )?;
        enforce_resource_limit(
            "SELECT group key bytes",
            next_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES),
            limits.max_group_key_bytes,
        )?;

        keys.insert(row, &probe);
        group_count = next_group_count;
        group_key_cells = next_key_cells;
        retained.push(true);
    }

    Ok(retained)
}

#[derive(Debug)]
enum UnionDistinctKeys<'a> {
    Empty(bool),
    One(HashSet<ValueRef<'a>>),
    Multiple(HashSet<Box<[ValueRef<'a>]>>),
}

impl<'a> UnionDistinctKeys<'a> {
    fn new(column_count: usize) -> Self {
        match column_count {
            0 => Self::Empty(false),
            1 => Self::One(HashSet::new()),
            _ => Self::Multiple(HashSet::new()),
        }
    }

    fn contains(&self, row: &'a [Value], probe: &mut Vec<ValueRef<'a>>) -> bool {
        match self {
            Self::Empty(present) => *present,
            Self::One(keys) => keys.contains(&row[0].as_ref()),
            Self::Multiple(keys) if row.len() == 2 => {
                let key = [row[0].as_ref(), row[1].as_ref()];
                keys.contains(key.as_slice())
            }
            Self::Multiple(keys) => {
                probe.clear();
                probe.extend(row.iter().map(Value::as_ref));
                keys.contains(probe.as_slice())
            }
        }
    }

    fn insert(&mut self, row: &'a [Value], probe: &[ValueRef<'a>]) {
        let inserted = match self {
            Self::Empty(present) => {
                let inserted = !*present;
                *present = true;
                inserted
            }
            Self::One(keys) => keys.insert(row[0].as_ref()),
            Self::Multiple(keys) if row.len() == 2 => {
                keys.insert([row[0].as_ref(), row[1].as_ref()].into())
            }
            Self::Multiple(keys) => {
                debug_assert_eq!(probe.len(), row.len());
                keys.insert(probe.into())
            }
        };
        debug_assert!(inserted, "new UNION DISTINCT row keys must be unique");
    }
}

fn validate_distinct_shape(select: &Select) -> Result<()> {
    if !select.distinct {
        return Ok(());
    }

    let unaliased_columns = !select.items.is_empty()
        && select
            .items
            .iter()
            .all(|item| matches!(item, SelectItem::Column { alias: None, .. }));
    if !unaliased_columns || !select.group_by.is_empty() || select.having.is_some() {
        return Err(Error::InvalidQuery(
            "SELECT DISTINCT supports one or more unaliased columns, an optional WHERE predicate, optional ordering by projected physical columns, and optional LIMIT <count> [OFFSET <offset>] or LIMIT <offset>, <count> pagination".to_owned(),
        ));
    }

    Ok(())
}

fn validate_row_number_shape(select: &Select) -> Result<()> {
    let has_row_number = select
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::RowNumber { .. }));
    if !has_row_number {
        return Ok(());
    }

    if select.distinct {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () is not supported with DISTINCT".to_owned(),
        ));
    }
    if !select.group_by.is_empty() || select.having.is_some() {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () is only supported in ungrouped SELECT queries".to_owned(),
        ));
    }
    if select
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }))
    {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () cannot be combined with aggregate projections".to_owned(),
        ));
    }
    if !select.order_by.is_empty() {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () cannot be combined with ORDER BY".to_owned(),
        ));
    }
    Ok(())
}

fn validate_offset_shape(select: &Select) -> Result<()> {
    let Some(_) = select.offset else {
        return Ok(());
    };
    if select.limit.is_none() {
        return Err(Error::InvalidQuery(
            "OFFSET requires LIMIT <count>".to_owned(),
        ));
    }
    if select
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::RowNumber { .. }))
    {
        return Err(Error::InvalidQuery(
            "OFFSET is not supported for ROW_NUMBER projections".to_owned(),
        ));
    }
    Ok(())
}

fn checked_selection_limit(limit: Option<usize>, offset: Option<usize>) -> Result<Option<usize>> {
    let Some(limit) = limit else {
        debug_assert!(offset.is_none(), "OFFSET without LIMIT is rejected");
        return Ok(None);
    };
    limit
        .checked_add(offset.unwrap_or(0))
        .map(Some)
        .ok_or_else(|| Error::NumericOverflow("LIMIT + OFFSET selection bound".to_owned()))
}

fn apply_offset(rows: &mut Vec<usize>, offset: usize) {
    if offset == 0 {
        return;
    }
    if offset >= rows.len() {
        rows.clear();
        return;
    }

    let remaining = rows.len() - offset;
    rows.copy_within(offset.., 0);
    rows.truncate(remaining);
}

#[derive(Debug, Clone, Copy)]
struct ResolvedWindowOrder {
    source: usize,
    descending: bool,
}

fn resolve_row_number_ordering(
    table: &Table,
    items: &[SelectItem],
) -> Result<Option<ResolvedWindowOrder>> {
    let mut row_number_orders = items.iter().filter_map(|item| match item {
        SelectItem::RowNumber { order_by, .. } => Some(order_by.as_ref()),
        _ => None,
    });
    let Some(first) = row_number_orders.next() else {
        return Ok(None);
    };

    if row_number_orders.any(|order| !same_window_order(first, order)) {
        return Err(Error::InvalidQuery(
            "all ROW_NUMBER projections must use the same window ordering".to_owned(),
        ));
    }

    let Some(order) = first else {
        return Ok(None);
    };
    let source = table.column_index(&order.name)?;
    let actual = table.schema()[source].data_type;
    if actual != DataType::Int64 {
        return Err(Error::TypeMismatch {
            context: format!("ROW_NUMBER ORDER BY column '{}'", order.name),
            expected: DataType::Int64.to_string(),
            actual: actual.to_string(),
        });
    }

    Ok(Some(ResolvedWindowOrder {
        source,
        descending: order.descending,
    }))
}

fn same_window_order(left: Option<&OrderBy>, right: Option<&OrderBy>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.descending == right.descending && left.name.eq_ignore_ascii_case(&right.name)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn order_window_rows(
    rows: &mut Vec<usize>,
    table: &Table,
    ordering: ResolvedWindowOrder,
    limit: Option<usize>,
) {
    sort_and_limit(rows, limit, |left, right| {
        let comparison = table.columns()[ordering.source].cmp_at(left, right);
        let comparison = if ordering.descending {
            comparison.reverse()
        } else {
            comparison
        };
        comparison.then_with(|| left.cmp(&right))
    });
}

fn resolve_distinct_columns(table: &Table, items: &[SelectItem]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        let SelectItem::Column { name, alias: None } = item else {
            unreachable!("the DISTINCT shape is validated")
        };
        let column = table.column_index(name)?;
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "DISTINCT column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok(columns)
}

impl QueryResult {
    fn estimated_retained_bytes(&self) -> usize {
        let mut bytes = self
            .columns
            .len()
            .saturating_mul(std::mem::size_of::<ResultColumn>())
            .saturating_add(
                self.columns
                    .iter()
                    .map(|column| column.name.len())
                    .fold(0_usize, usize::saturating_add),
            )
            .saturating_add(
                self.rows
                    .len()
                    .saturating_mul(std::mem::size_of::<Vec<Value>>()),
            );
        for row in &self.rows {
            bytes = bytes.saturating_add(row.len().saturating_mul(std::mem::size_of::<Value>()));
            for value in row {
                if let Value::String(value) = value {
                    bytes = bytes.saturating_add(value.len());
                }
            }
        }
        bytes
    }
}

impl StatementResult {
    fn estimated_retained_bytes(&self) -> usize {
        match self {
            Self::Command { .. } => 0,
            Self::Query(result) => result.estimated_retained_bytes(),
        }
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Column {
        source: usize,
        group_position: Option<usize>,
    },
    Int64Subtract {
        source: usize,
        literal: i64,
    },
    IfNullInt64 {
        source: usize,
        fallback: i64,
        group_position: Option<usize>,
    },
    IsNull {
        source: usize,
        group_position: Option<usize>,
    },
    IsNotNull {
        source: usize,
        group_position: Option<usize>,
    },
    CastNullableInt64ToInt64 {
        source: usize,
        group_position: Option<usize>,
    },
    CastInt64ToFloat64 {
        source: usize,
    },
    CastBoolToFloat64 {
        source: usize,
    },
    CastStringToFloat64 {
        source: usize,
    },
    CastFloat64ToInt64 {
        source: usize,
    },
    CastBoolToInt64 {
        source: usize,
    },
    CastStringToInt64 {
        source: usize,
    },
    CastInt64ToBool {
        source: usize,
    },
    CastFloat64ToBool {
        source: usize,
    },
    CastStringToBool {
        source: usize,
    },
    CastInt64ToString {
        source: usize,
    },
    CastFloat64ToString {
        source: usize,
    },
    CastBoolToString {
        source: usize,
    },
    ToString {
        source: usize,
        input_type: DataType,
    },
    StringLength {
        source: usize,
    },
    StringLengthUtf8 {
        source: usize,
    },
    StringEmpty {
        source: usize,
    },
    StringLower {
        source: usize,
    },
    StringUpper {
        source: usize,
    },
    Int64Abs {
        source: usize,
    },
    Float64Abs {
        source: usize,
    },
    Float64Round {
        source: usize,
    },
    Float64Floor {
        source: usize,
    },
    Float64Ceil {
        source: usize,
    },
    RowNumber,
    Aggregate {
        state: usize,
    },
}

#[derive(Debug, Clone)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
}

#[derive(Debug, Clone)]
struct ResolvedHaving {
    state: usize,
    predicate: ResolvedHavingPredicate,
}

#[derive(Debug, Clone)]
enum ResolvedHavingPredicate {
    Comparison {
        operator: ComparisonOperator,
        value: Value,
    },
    IsNull,
    IsNotNull,
}

impl ResolvedHaving {
    fn evaluate(&self, data: &GroupedData<'_>, group: usize) -> bool {
        let aggregate = &data.aggregates[self.state][group];
        match &self.predicate {
            ResolvedHavingPredicate::Comparison { operator, value } => {
                let Some(comparison) = aggregate.as_ref().sql_cmp(value.as_ref()) else {
                    return false;
                };
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            ResolvedHavingPredicate::IsNull => matches!(aggregate, Value::Null(_)),
            ResolvedHavingPredicate::IsNotNull => !matches!(aggregate, Value::Null(_)),
        }
    }
}

fn resolve_group_columns(table: &Table, expressions: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(expressions.len());
    for expression in expressions {
        // GROUP BY expressions are parser-normalized before reaching the
        // existing public string AST. Only this fixed CAST representation is
        // interpreted as an expression; every other string remains a column.
        let cast = expression
            .strip_prefix("CAST(")
            .and_then(|expression| expression.strip_suffix(')'))
            .and_then(|expression| expression.split_once(" AS "));
        let (name, column) = match cast {
            Some((name, target_type)) => {
                let column = table.column_index(name)?;
                if DataType::parse(target_type) != Some(DataType::Int64)
                    || !table.column_is_nullable_int64(column)
                {
                    return Err(Error::InvalidQuery(
                        "GROUP BY CAST only supports CAST(Nullable(Int64) AS Int64)".to_owned(),
                    ));
                }
                (name, column)
            }
            None => (expression.as_str(), table.column_index(expression)?),
        };
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "GROUP BY column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok(columns)
}

fn reject_nullable_operation(table: &Table, column: usize, operation: &'static str) -> Result<()> {
    if table.column_is_nullable_int64(column) {
        return Err(Error::UnsupportedNullableOperation {
            table: table.name().to_owned(),
            column: table.schema()[column].name.clone(),
            operation,
        });
    }
    Ok(())
}

fn resolve_select_items(
    table: &Table,
    requested: &[SelectItem],
    group_columns: &[usize],
) -> Result<(Vec<ResolvedItem>, Vec<ResultColumn>, Vec<AggregateSpec>)> {
    let has_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    if has_aggregate
        && requested
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard))
    {
        return Err(Error::InvalidQuery(
            "'*' projection cannot be combined with aggregates".to_owned(),
        ));
    }

    let mut items = Vec::new();
    let mut result_columns = Vec::new();
    let mut aggregate_specs = Vec::new();

    for requested_item in requested {
        match requested_item {
            SelectItem::Wildcard => {
                for (source, field) in table.schema().iter().enumerate() {
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: field.name.clone(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = table.column_index(name)?;
                let group_position = group_columns.iter().position(|column| *column == source);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::Column {
                    source,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| table.schema()[source].name.clone()),
                    data_type: table.schema()[source].data_type,
                });
            }
            SelectItem::Int64Subtract {
                name,
                literal,
                alias,
            } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Int64 {
                    return Err(Error::TypeMismatch {
                        context: format!("Int64 subtraction argument '{name}'"),
                        expected: DataType::Int64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "Int64 subtraction projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Int64Subtract {
                    source,
                    literal: *literal,
                });
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| {
                        sql::int64_subtraction_name(&table.schema()[source].name, *literal)
                    }),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::IfNullInt64 {
                name,
                fallback,
                alias,
            } => {
                let source = table.column_index(name)?;
                if !table.column_is_nullable_int64(source) {
                    return Err(Error::TypeMismatch {
                        context: format!("ifNull first argument '{name}'"),
                        expected: "Nullable(Int64)".to_owned(),
                        actual: table.columns()[source].metadata_type_name().to_owned(),
                    });
                }
                let group_position = group_columns.iter().position(|column| *column == source);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::IfNullInt64 {
                    source,
                    fallback: *fallback,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| {
                        sql::if_null_int64_name(&table.schema()[source].name, *fallback)
                    }),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::IsNull { name, alias } => {
                let source = table.column_index(name)?;
                let group_position = group_columns.iter().position(|column| *column == source);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::IsNull {
                    source,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| sql::is_null_name(&table.schema()[source].name)),
                    data_type: DataType::Bool,
                });
            }
            SelectItem::IsNotNull { name, alias } => {
                let source = table.column_index(name)?;
                let group_position = group_columns.iter().position(|column| *column == source);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::IsNotNull {
                    source,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| sql::is_not_null_name(&table.schema()[source].name)),
                    data_type: DataType::Bool,
                });
            }
            SelectItem::Cast {
                name,
                target_type,
                alias,
            } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                let group_position = group_columns.iter().position(|column| *column == source);
                let resolved = match (actual, *target_type) {
                    (DataType::Int64, DataType::Int64)
                        if table.column_is_nullable_int64(source) =>
                    {
                        Some(ResolvedItem::CastNullableInt64ToInt64 {
                            source,
                            group_position,
                        })
                    }
                    (DataType::Int64, DataType::Float64) => {
                        Some(ResolvedItem::CastInt64ToFloat64 { source })
                    }
                    (DataType::Bool, DataType::Float64) => {
                        Some(ResolvedItem::CastBoolToFloat64 { source })
                    }
                    (DataType::String, DataType::Float64) => {
                        Some(ResolvedItem::CastStringToFloat64 { source })
                    }
                    (DataType::Float64, DataType::Int64) => {
                        Some(ResolvedItem::CastFloat64ToInt64 { source })
                    }
                    (DataType::Bool, DataType::Int64) => {
                        Some(ResolvedItem::CastBoolToInt64 { source })
                    }
                    (DataType::String, DataType::Int64) => {
                        Some(ResolvedItem::CastStringToInt64 { source })
                    }
                    (DataType::Int64, DataType::Bool) => {
                        Some(ResolvedItem::CastInt64ToBool { source })
                    }
                    (DataType::Float64, DataType::Bool) => {
                        Some(ResolvedItem::CastFloat64ToBool { source })
                    }
                    (DataType::String, DataType::Bool) => {
                        Some(ResolvedItem::CastStringToBool { source })
                    }
                    (DataType::Int64, DataType::String) => {
                        Some(ResolvedItem::CastInt64ToString { source })
                    }
                    (DataType::Float64, DataType::String) => {
                        Some(ResolvedItem::CastFloat64ToString { source })
                    }
                    (DataType::Bool, DataType::String) => {
                        Some(ResolvedItem::CastBoolToString { source })
                    }
                    _ => None,
                };
                let Some(resolved) = resolved else {
                    let expected = match target_type {
                        DataType::Float64 => "Int64, Bool, or String",
                        DataType::Bool => "Int64, Float64, or String",
                        DataType::Int64 => "Float64, Bool, or String",
                        DataType::String => "Int64, Float64, or Bool",
                    };
                    return Err(Error::TypeMismatch {
                        context: format!("CAST argument '{name}'"),
                        expected: expected.to_owned(),
                        actual: actual.to_string(),
                    });
                };
                if has_aggregate || !group_columns.is_empty() {
                    match &resolved {
                        ResolvedItem::CastNullableInt64ToInt64 {
                            group_position: Some(_),
                            ..
                        } => {}
                        ResolvedItem::CastNullableInt64ToInt64 {
                            group_position: None,
                            ..
                        } => {
                            return Err(Error::InvalidQuery(format!(
                                "column '{name}' must appear in GROUP BY"
                            )));
                        }
                        _ => {
                            return Err(Error::InvalidQuery(
                                "CAST projections are only supported in ungrouped SELECT queries"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                items.push(resolved);
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| {
                        format!("CAST({} AS {target_type})", table.schema()[source].name)
                    }),
                    data_type: *target_type,
                });
            }
            SelectItem::ToString { name, alias } => {
                let source = table.column_index(name)?;
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "toString projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::ToString {
                    source,
                    input_type: table.schema()[source].data_type,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("toString({})", table.schema()[source].name)),
                    data_type: DataType::String,
                });
            }
            SelectItem::Length { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("LENGTH argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "LENGTH projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringLength { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("LENGTH({})", table.schema()[source].name)),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::LengthUtf8 { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("lengthUTF8 argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "lengthUTF8 projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringLengthUtf8 { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("lengthUTF8({})", table.schema()[source].name)),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Empty { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("empty argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "empty projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringEmpty { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("empty({})", table.schema()[source].name)),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Lower { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("LOWER argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "LOWER projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringLower { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("LOWER({})", table.schema()[source].name)),
                    data_type: DataType::String,
                });
            }
            SelectItem::Upper { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("UPPER argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "UPPER projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringUpper { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("UPPER({})", table.schema()[source].name)),
                    data_type: DataType::String,
                });
            }
            SelectItem::Abs { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                let item = match actual {
                    DataType::Int64 => ResolvedItem::Int64Abs { source },
                    DataType::Float64 => ResolvedItem::Float64Abs { source },
                    DataType::Bool | DataType::String => {
                        return Err(Error::TypeMismatch {
                            context: format!("ABS argument '{name}'"),
                            expected: "Int64 or Float64".to_owned(),
                            actual: actual.to_string(),
                        });
                    }
                };
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "ABS projections are only supported in ungrouped SELECT queries".to_owned(),
                    ));
                }
                items.push(item);
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("ABS({})", table.schema()[source].name)),
                    data_type: actual,
                });
            }
            SelectItem::Round { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("ROUND argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "ROUND projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Round { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("ROUND({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::Floor { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("FLOOR argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "FLOOR projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Floor { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("FLOOR({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::Ceil { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("CEIL argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "CEIL projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Ceil { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("CEIL({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::RowNumber { alias, .. } => {
                items.push(ResolvedItem::RowNumber);
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| "ROW_NUMBER()".to_owned()),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (argument_index, input_type, argument_name) = match argument {
                    AggregateArgument::Empty => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}() is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, None, String::new())
                    }
                    AggregateArgument::Wildcard => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}(*) is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, None, "*".to_owned())
                    }
                    AggregateArgument::Column(name) => {
                        let index = table.column_index(name)?;
                        (
                            Some(index),
                            Some(table.schema()[index].data_type),
                            table.schema()[index].name.clone(),
                        )
                    }
                };
                validate_aggregate(*function, input_type)?;
                if let Some(argument) = argument_index {
                    let supported_nullable_aggregate = matches!(
                        function,
                        AggregateFunction::Count
                            | AggregateFunction::Sum
                            | AggregateFunction::Min
                            | AggregateFunction::Max
                            | AggregateFunction::Avg
                    ) && table
                        .column_is_nullable_int64(argument);
                    if !supported_nullable_aggregate {
                        reject_nullable_operation(table, argument, function.name())?;
                    }
                }
                let state = aggregate_specs.len();
                aggregate_specs.push(AggregateSpec {
                    function: *function,
                    argument: argument_index,
                    input_type,
                });
                items.push(ResolvedItem::Aggregate { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: aggregate_output_type(*function, input_type),
                });
            }
        }
    }

    Ok((items, result_columns, aggregate_specs))
}

fn resolve_having(
    columns: &[ResultColumn],
    items: &[ResolvedItem],
    aggregate_specs: &[AggregateSpec],
    requested: &Having,
) -> Result<ResolvedHaving> {
    let matches = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.name.eq_ignore_ascii_case(&requested.alias))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let output = match matches.as_slice() {
        [output] => *output,
        [] => {
            return Err(Error::InvalidQuery(format!(
                "HAVING alias '{}' is not in the SELECT output",
                requested.alias
            )));
        }
        _ => {
            return Err(Error::InvalidQuery(format!(
                "HAVING alias '{}' is ambiguous",
                requested.alias
            )));
        }
    };

    let aggregate_requirement = match &requested.predicate {
        HavingPredicate::Comparison { .. } => "a projected numeric aggregate",
        HavingPredicate::IsNull | HavingPredicate::IsNotNull => "a projected aggregate",
    };
    let ResolvedItem::Aggregate { state } = items[output] else {
        return Err(Error::InvalidQuery(format!(
            "HAVING alias '{}' must reference {aggregate_requirement}",
            requested.alias,
        )));
    };
    let spec = &aggregate_specs[state];
    let predicate = match &requested.predicate {
        HavingPredicate::Comparison { operator, value } => {
            let supported = matches!(
                aggregate_output_type(spec.function, spec.input_type),
                DataType::Int64 | DataType::Float64
            );
            if !supported {
                return Err(Error::InvalidQuery(format!(
                    "HAVING alias '{}' must reference a projected numeric aggregate",
                    requested.alias
                )));
            }
            match value {
                Value::Int64(_) => {}
                Value::Float64(value) if value.is_finite() => {}
                Value::Float64(_) => {
                    return Err(Error::InvalidQuery(
                        "HAVING comparison Float64 thresholds must be finite".to_owned(),
                    ));
                }
                Value::Null(_) => {
                    return Err(Error::InvalidQuery(
                        "HAVING comparisons do not support NULL thresholds".to_owned(),
                    ));
                }
                value => {
                    return Err(Error::TypeMismatch {
                        context: "HAVING comparison threshold".to_owned(),
                        expected: "Int64 or Float64".to_owned(),
                        actual: value.data_type().to_string(),
                    });
                }
            }
            ResolvedHavingPredicate::Comparison {
                operator: *operator,
                value: value.clone(),
            }
        }
        HavingPredicate::IsNull => ResolvedHavingPredicate::IsNull,
        HavingPredicate::IsNotNull => ResolvedHavingPredicate::IsNotNull,
    };

    Ok(ResolvedHaving { state, predicate })
}

fn validate_aggregate(function: AggregateFunction, input_type: Option<DataType>) -> Result<()> {
    if function == AggregateFunction::CountIf && input_type != Some(DataType::Bool) {
        let actual = input_type.map_or_else(|| "*".to_owned(), |value| value.to_string());
        return Err(Error::TypeMismatch {
            context: format!("{} argument", function.name()),
            expected: DataType::Bool.to_string(),
            actual,
        });
    }
    if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
        && !matches!(input_type, Some(DataType::Int64 | DataType::Float64))
    {
        let actual = input_type.map_or_else(|| "*".to_owned(), |value| value.to_string());
        return Err(Error::TypeMismatch {
            context: format!("{} argument", function.name()),
            expected: "Int64 or Float64".to_owned(),
            actual,
        });
    }
    Ok(())
}

fn aggregate_output_type(function: AggregateFunction, input_type: Option<DataType>) -> DataType {
    match function {
        AggregateFunction::Count | AggregateFunction::CountIf => DataType::Int64,
        AggregateFunction::Avg => DataType::Float64,
        AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => {
            input_type.expect("validated column argument")
        }
    }
}

fn execute_projection(
    table: &Table,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Result<Vec<Vec<Value>>> {
    matching_rows
        .iter()
        .enumerate()
        .map(|(row_number, row)| {
            items
                .iter()
                .map(|item| {
                    Ok(match item {
                        ResolvedItem::Column { source, .. } => table.columns()[*source].value(*row),
                        ResolvedItem::Int64Subtract { source, literal } => {
                            scalar_nullable_int64::checked_subtract(
                                table.columns()[*source].value_ref(*row),
                                *literal,
                            )?
                        }
                        ResolvedItem::IfNullInt64 {
                            source, fallback, ..
                        } => Value::Int64(if_null_int64_at(table, *source, *row, *fallback)),
                        ResolvedItem::IsNull { source, .. } => {
                            Value::Bool(is_null_at(table, *source, *row))
                        }
                        ResolvedItem::IsNotNull { source, .. } => {
                            Value::Bool(!is_null_at(table, *source, *row))
                        }
                        ResolvedItem::CastNullableInt64ToInt64 { source, .. } => {
                            table.columns()[*source].value(*row)
                        }
                        ResolvedItem::CastInt64ToFloat64 { source } => {
                            int64_to_float64_at(table, *source, *row).to_owned()
                        }
                        ResolvedItem::CastBoolToFloat64 { source } => {
                            Value::Float64(if bool_at(table, *source, *row) {
                                1.0
                            } else {
                                0.0
                            })
                        }
                        ResolvedItem::CastStringToFloat64 { source } => Value::Float64(
                            checked_string_to_float64(string_at(table, *source, *row))?,
                        ),
                        ResolvedItem::CastFloat64ToInt64 { source } => Value::Int64(
                            checked_float64_to_int64(float64_at(table, *source, *row))?,
                        ),
                        ResolvedItem::CastBoolToInt64 { source } => {
                            Value::Int64(if bool_at(table, *source, *row) { 1 } else { 0 })
                        }
                        ResolvedItem::CastStringToInt64 { source } => {
                            Value::Int64(checked_string_to_int64(string_at(table, *source, *row))?)
                        }
                        ResolvedItem::CastInt64ToBool { source } => {
                            int64_to_bool_at(table, *source, *row).to_owned()
                        }
                        ResolvedItem::CastFloat64ToBool { source } => {
                            Value::Bool(float64_at(table, *source, *row) != 0.0)
                        }
                        ResolvedItem::CastStringToBool { source } => {
                            Value::Bool(checked_string_to_bool(string_at(table, *source, *row))?)
                        }
                        ResolvedItem::CastInt64ToString { source } => {
                            stringify_value(table, *source, *row, DataType::Int64)
                        }
                        ResolvedItem::CastFloat64ToString { source } => {
                            stringify_value(table, *source, *row, DataType::Float64)
                        }
                        ResolvedItem::CastBoolToString { source } => {
                            stringify_value(table, *source, *row, DataType::Bool)
                        }
                        ResolvedItem::ToString { source, input_type } => {
                            stringify_value(table, *source, *row, *input_type)
                        }
                        ResolvedItem::StringLength { source } => {
                            Value::Int64(scalar_string::string_length_to_i64(
                                string_at(table, *source, *row).len(),
                            )?)
                        }
                        ResolvedItem::StringLengthUtf8 { source } => {
                            Value::Int64(scalar_string::string_length_utf8_to_i64(string_at(
                                table, *source, *row,
                            ))?)
                        }
                        ResolvedItem::StringEmpty { source } => {
                            Value::Int64(i64::from(string_at(table, *source, *row).is_empty()))
                        }
                        ResolvedItem::StringLower { source } => {
                            Value::String(string_at(table, *source, *row).to_ascii_lowercase())
                        }
                        ResolvedItem::StringUpper { source } => {
                            Value::String(string_at(table, *source, *row).to_ascii_uppercase())
                        }
                        ResolvedItem::Int64Abs { source } => scalar_nullable_int64::checked_abs(
                            table.columns()[*source].value_ref(*row),
                        )?,
                        ResolvedItem::Float64Abs { source } => {
                            Value::Float64(scalar_float64::abs(float64_at(table, *source, *row)))
                        }
                        ResolvedItem::Float64Round { source } => {
                            Value::Float64(scalar_float64::round(float64_at(table, *source, *row)))
                        }
                        ResolvedItem::Float64Floor { source } => {
                            Value::Float64(scalar_float64::floor(float64_at(table, *source, *row)))
                        }
                        ResolvedItem::Float64Ceil { source } => {
                            Value::Float64(scalar_float64::ceil(float64_at(table, *source, *row)))
                        }
                        ResolvedItem::RowNumber => Value::Int64(checked_row_number(row_number)?),
                        ResolvedItem::Aggregate { .. } => {
                            unreachable!("projection does not contain aggregates")
                        }
                    })
                })
                .collect()
        })
        .collect()
}

fn validate_row_number_count(row_count: usize) -> Result<()> {
    if row_count > 0 {
        checked_row_number(row_count - 1)?;
    }
    Ok(())
}

fn checked_row_number(zero_based: usize) -> Result<i64> {
    i64::try_from(zero_based)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::NumericOverflow("ROW_NUMBER result".to_owned()))
}

fn limited_cartesian_row_count(
    left_rows: usize,
    right_rows: usize,
    limit: Option<usize>,
) -> Result<usize> {
    match limit {
        Some(limit) => Ok(left_rows
            .checked_mul(right_rows)
            .map_or(limit, |rows| rows.min(limit))),
        None => left_rows
            .checked_mul(right_rows)
            .ok_or_else(|| Error::NumericOverflow("CROSS JOIN row count".to_owned())),
    }
}

fn validate_cross_join_result_limits(
    left: &Table,
    right: &Table,
    row_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
) -> Result<()> {
    let mut bytes = validate_result_shape(
        row_count,
        columns.len(),
        columns,
        limits,
        SELECT_RESULT_RESOURCES,
    )?;
    bytes = bytes.saturating_add(cross_join_string_bytes(left, right, row_count));
    enforce_resource_limit(SELECT_RESULT_RESOURCES.bytes, bytes, limits.max_bytes)
}

/// Counts cloned string payload bytes in the LIMIT-truncated left-major
/// product without constructing row or value vectors.
fn cross_join_string_bytes(left: &Table, right: &Table, row_count: usize) -> usize {
    if row_count == 0 {
        return 0;
    }

    let right_rows = right.row_count();
    debug_assert!(right_rows > 0);
    let complete_left_rows = row_count / right_rows;
    let partial_right_rows = row_count % right_rows;
    let mut bytes = 0_usize;

    for left_row in 0..complete_left_rows {
        bytes = bytes.saturating_add(string_bytes_at(left, left_row).saturating_mul(right_rows));
    }
    if partial_right_rows > 0 {
        bytes = bytes.saturating_add(
            string_bytes_at(left, complete_left_rows).saturating_mul(partial_right_rows),
        );
    }

    if complete_left_rows > 0 {
        let right_product_bytes = (0..right_rows)
            .map(|right_row| string_bytes_at(right, right_row))
            .fold(0_usize, usize::saturating_add)
            .saturating_mul(complete_left_rows);
        bytes = bytes.saturating_add(right_product_bytes);
    }
    for right_row in 0..partial_right_rows {
        bytes = bytes.saturating_add(string_bytes_at(right, right_row));
    }
    bytes
}

fn string_bytes_at(table: &Table, row: usize) -> usize {
    table
        .columns()
        .iter()
        .map(|column| match column.value_ref(row) {
            ValueRef::String(value) => value.len(),
            ValueRef::Null(_) | ValueRef::Int64(_) | ValueRef::Float64(_) | ValueRef::Bool(_) => 0,
        })
        .fold(0_usize, usize::saturating_add)
}

fn validate_projection_result_limits(
    table: &Table,
    rows: &[usize],
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<()> {
    let mut bytes =
        validate_select_result_shape(rows.len(), items.len(), columns, limits, result_prefix)?;
    for row in rows {
        for item in items {
            let source = match item {
                ResolvedItem::Column { source, .. }
                | ResolvedItem::StringLower { source }
                | ResolvedItem::StringUpper { source } => Some(*source),
                ResolvedItem::ToString {
                    source,
                    input_type: DataType::String,
                } => Some(*source),
                ResolvedItem::Int64Subtract { .. }
                | ResolvedItem::IfNullInt64 { .. }
                | ResolvedItem::IsNull { .. }
                | ResolvedItem::IsNotNull { .. }
                | ResolvedItem::CastNullableInt64ToInt64 { .. }
                | ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastBoolToFloat64 { .. }
                | ResolvedItem::CastStringToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastStringToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastStringToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastFloat64ToString { .. }
                | ResolvedItem::CastBoolToString { .. }
                | ResolvedItem::ToString { .. }
                | ResolvedItem::StringLength { .. }
                | ResolvedItem::StringLengthUtf8 { .. }
                | ResolvedItem::StringEmpty { .. }
                | ResolvedItem::Int64Abs { .. }
                | ResolvedItem::Float64Abs { .. }
                | ResolvedItem::Float64Round { .. }
                | ResolvedItem::Float64Floor { .. }
                | ResolvedItem::Float64Ceil { .. }
                | ResolvedItem::RowNumber => None,
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("ungrouped projections cannot contain aggregates")
                }
            };
            if let Some(source) = source {
                if let ValueRef::String(value) = table.columns()[source].value_ref(*row) {
                    bytes = bytes.saturating_add(value.len());
                    enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
                }
            } else {
                let cast_string_len = match item {
                    ResolvedItem::CastInt64ToString { source } => {
                        Some(stringified_len(table, *source, *row, DataType::Int64))
                    }
                    ResolvedItem::CastFloat64ToString { source } => {
                        Some(stringified_len(table, *source, *row, DataType::Float64))
                    }
                    ResolvedItem::CastBoolToString { source } => {
                        Some(stringified_len(table, *source, *row, DataType::Bool))
                    }
                    ResolvedItem::ToString { source, input_type } => {
                        debug_assert_ne!(*input_type, DataType::String);
                        Some(stringified_len(table, *source, *row, *input_type))
                    }
                    _ => None,
                };
                if let Some(cast_string_len) = cast_string_len {
                    bytes = bytes.saturating_add(cast_string_len);
                    enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_grouped_result_limits(
    data: &GroupedData<'_>,
    groups: &[usize],
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<()> {
    let mut bytes =
        validate_select_result_shape(groups.len(), items.len(), columns, limits, result_prefix)?;
    for group in groups {
        for item in items {
            let string_len = match item {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => match data.keys[*group].value(*position) {
                    ValueRef::String(value) => value.len(),
                    _ => 0,
                },
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Int64Subtract { .. } => {
                    unreachable!(
                        "Int64 subtraction projections are restricted to ungrouped queries"
                    )
                }
                ResolvedItem::IfNullInt64 {
                    group_position: Some(_),
                    ..
                } => 0,
                ResolvedItem::IfNullInt64 {
                    group_position: None,
                    ..
                } => unreachable!("grouped ifNull arguments are validated"),
                ResolvedItem::IsNull {
                    group_position: Some(_),
                    ..
                } => 0,
                ResolvedItem::IsNull {
                    group_position: None,
                    ..
                } => unreachable!("grouped isNull arguments are validated"),
                ResolvedItem::IsNotNull {
                    group_position: Some(_),
                    ..
                } => 0,
                ResolvedItem::IsNotNull {
                    group_position: None,
                    ..
                } => unreachable!("grouped isNotNull arguments are validated"),
                ResolvedItem::CastNullableInt64ToInt64 {
                    group_position: Some(_),
                    ..
                } => 0,
                ResolvedItem::CastNullableInt64ToInt64 {
                    group_position: None,
                    ..
                } => unreachable!("grouped Nullable(Int64) CAST arguments are validated"),
                ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastBoolToFloat64 { .. }
                | ResolvedItem::CastStringToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastStringToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastStringToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastFloat64ToString { .. }
                | ResolvedItem::CastBoolToString { .. } => {
                    unreachable!("CAST projections are restricted to ungrouped queries")
                }
                ResolvedItem::ToString { .. } => {
                    unreachable!("toString projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLength { .. } => {
                    unreachable!("LENGTH projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLengthUtf8 { .. } => {
                    unreachable!("lengthUTF8 projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringEmpty { .. } => {
                    unreachable!("empty projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLower { .. } => {
                    unreachable!("LOWER projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringUpper { .. } => {
                    unreachable!("UPPER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Int64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Round { .. } => {
                    unreachable!("ROUND projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Floor { .. } => {
                    unreachable!("FLOOR projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Ceil { .. } => {
                    unreachable!("CEIL projections are restricted to ungrouped queries")
                }
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Aggregate { state } => match &data.aggregates[*state][*group] {
                    Value::String(value) => value.len(),
                    _ => 0,
                },
            };
            bytes = bytes.saturating_add(string_len);
            enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
        }
    }
    Ok(())
}

fn validate_select_result_shape(
    row_count: usize,
    column_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<usize> {
    let combined_row_count = result_prefix.map_or(row_count, |prefix| {
        prefix.row_count.saturating_add(row_count)
    });
    let output_columns = result_prefix.map_or(columns, |prefix| prefix.columns);
    let bytes = validate_result_shape(
        combined_row_count,
        column_count,
        output_columns,
        limits,
        SELECT_RESULT_RESOURCES,
    )?
    .saturating_add(result_prefix.map_or(0, |prefix| prefix.string_bytes));
    enforce_resource_limit(SELECT_RESULT_RESOURCES.bytes, bytes, limits.max_bytes)?;
    Ok(bytes)
}

fn validate_result_shape(
    row_count: usize,
    column_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    resources: QueryResultResources,
) -> Result<usize> {
    let column_name_bytes = columns
        .iter()
        .map(|column| column.name.len())
        .fold(0_usize, usize::saturating_add);
    validate_result_shape_parts(
        row_count,
        column_count,
        columns.len(),
        column_name_bytes,
        limits,
        resources,
    )
}

fn validate_result_shape_parts(
    row_count: usize,
    values_per_row: usize,
    result_column_count: usize,
    result_column_name_bytes: usize,
    limits: QueryResultLimits,
    resources: QueryResultResources,
) -> Result<usize> {
    enforce_resource_limit(resources.rows, row_count, limits.max_rows)?;
    let value_count = row_count.saturating_mul(values_per_row);
    enforce_resource_limit(resources.values, value_count, limits.max_values)?;

    let column_bytes = result_column_count
        .saturating_mul(std::mem::size_of::<ResultColumn>())
        .saturating_add(result_column_name_bytes);
    let bytes = column_bytes
        .saturating_add(row_count.saturating_mul(std::mem::size_of::<Vec<Value>>()))
        .saturating_add(value_count.saturating_mul(std::mem::size_of::<Value>()));
    enforce_resource_limit(resources.bytes, bytes, limits.max_bytes)?;
    Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
struct QueryResultResources {
    rows: &'static str,
    values: &'static str,
    bytes: &'static str,
}

const SELECT_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SELECT result rows",
    values: "SELECT result values",
    bytes: "SELECT result bytes",
};

const SHOW_DATABASES_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW DATABASES result rows",
    values: "SHOW DATABASES result values",
    bytes: "SHOW DATABASES result bytes",
};

const SHOW_SETTINGS_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW SETTINGS result rows",
    values: "SHOW SETTINGS result values",
    bytes: "SHOW SETTINGS result bytes",
};

const SHOW_FUNCTIONS_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW FUNCTIONS result rows",
    values: "SHOW FUNCTIONS result values",
    bytes: "SHOW FUNCTIONS result bytes",
};

const SHOW_TABLES_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW TABLES result rows",
    values: "SHOW TABLES result values",
    bytes: "SHOW TABLES result bytes",
};

const SHOW_CREATE_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW CREATE TABLE result rows",
    values: "SHOW CREATE TABLE result values",
    bytes: "SHOW CREATE TABLE result bytes",
};

const DESCRIBE_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "DESCRIBE TABLE result rows",
    values: "DESCRIBE TABLE result values",
    bytes: "DESCRIBE TABLE result bytes",
};

const EXISTS_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "EXISTS TABLE result rows",
    values: "EXISTS TABLE result values",
    bytes: "EXISTS TABLE result bytes",
};

fn enforce_resource_limit(resource: &'static str, actual: usize, max: usize) -> Result<()> {
    if actual > max {
        Err(Error::ResourceLimitExceeded {
            resource,
            actual,
            max,
        })
    } else {
        Ok(())
    }
}

fn usize_decimal_len(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn enforce_select_scan_limit(table: &Table, limits: QueryResultLimits) -> Result<()> {
    enforce_scan_limit(table, limits, "SELECT scanned rows")
}

fn enforce_select_scan_rows(rows: usize, limits: QueryResultLimits) -> Result<()> {
    enforce_resource_limit("SELECT scanned rows", rows, limits.max_scan_rows)
}

fn enforce_scan_limit(
    table: &Table,
    limits: QueryResultLimits,
    resource: &'static str,
) -> Result<()> {
    enforce_resource_limit(resource, table.row_count(), limits.max_scan_rows)
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    limits: QueryResultLimits,
    parallelism: GlobalAggregateParallelism,
) -> Result<GroupedData<'a>> {
    let planned_group_count = if group_columns.is_empty() {
        enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
        1
    } else {
        matching_rows.len().min(limits.max_groups)
    };
    let planned_state_cells = planned_group_count.saturating_mul(aggregate_specs.len());
    enforce_resource_limit(
        "SELECT aggregate state cells",
        planned_state_cells,
        limits.max_aggregate_state_cells,
    )?;
    let mut aggregate_state_bytes = planned_state_cells
        .saturating_mul(std::mem::size_of::<AggregateState>())
        .saturating_add(
            aggregate_specs
                .len()
                .saturating_mul(std::mem::size_of::<Vec<AggregateState>>()),
        );
    enforce_resource_limit(
        "SELECT aggregate state bytes",
        aggregate_state_bytes,
        limits.max_aggregate_state_bytes,
    )?;

    let key_cells_per_group = group_columns.len();
    let key_bytes_per_group = key_cells_per_group.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
    let probe_key_cells = if group_columns.len() > 2 && !matching_rows.is_empty() {
        key_cells_per_group
    } else {
        0
    };
    let probe_key_bytes = probe_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
    if !group_columns.is_empty() && !matching_rows.is_empty() {
        enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
        let first_group_key_cells = probe_key_cells.saturating_add(key_cells_per_group);
        enforce_resource_limit(
            "SELECT group key cells",
            first_group_key_cells,
            limits.max_group_key_cells,
        )?;
        let first_group_key_bytes = probe_key_bytes.saturating_add(key_bytes_per_group);
        enforce_resource_limit(
            "SELECT group key bytes",
            first_group_key_bytes,
            limits.max_group_key_bytes,
        )?;
    }

    if let Some(grouped) = execute_grouped_bool_count(
        table,
        matching_rows,
        group_columns,
        aggregate_specs,
        limits,
        parallelism,
    )? {
        return Ok(grouped);
    }
    if let Some(grouped) = execute_grouped_bool_sum(
        table,
        matching_rows,
        group_columns,
        aggregate_specs,
        limits,
        parallelism,
    )? {
        return Ok(grouped);
    }

    let mut groups = GroupIndex::new(group_columns.len());
    let mut group_count = usize::from(group_columns.is_empty());
    let mut group_key_cells = probe_key_cells;
    let mut group_key_bytes = probe_key_bytes;
    let mut multiple_key_probe = Vec::with_capacity(probe_key_cells);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(planned_group_count);
            if group_columns.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();
    let sole_global_sum_int = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Sum,
                input_type: Some(DataType::Int64),
                ..
            }]
        )
        && aggregate_specs
            .first()
            .is_some_and(|spec| aggregate_argument_is_physical_int64(table, spec));
    let sole_global_avg_int = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Avg,
                input_type: Some(DataType::Int64),
                ..
            }]
        )
        && aggregate_specs
            .first()
            .is_some_and(|spec| aggregate_argument_is_physical_int64(table, spec));
    let sole_global_nullable_int64_count = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Count,
                argument: Some(_),
                input_type: Some(DataType::Int64),
            }]
        )
        && aggregate_specs.first().is_some_and(|spec| {
            table.column_is_nullable_int64(
                spec.argument
                    .expect("nullable Int64 COUNT has a column argument"),
            )
        });
    let sole_global_min_int = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Min,
                input_type: Some(DataType::Int64),
                ..
            }]
        )
        && aggregate_specs
            .first()
            .is_some_and(|spec| aggregate_argument_is_physical_int64(table, spec));
    let sole_global_min_float = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Min,
                input_type: Some(DataType::Float64),
                ..
            }]
        );
    let sole_global_max_int = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Max,
                input_type: Some(DataType::Int64),
                ..
            }]
        )
        && aggregate_specs
            .first()
            .is_some_and(|spec| aggregate_argument_is_physical_int64(table, spec));
    let sole_global_max_float = group_columns.is_empty()
        && matches!(
            aggregate_specs,
            [AggregateSpec {
                function: AggregateFunction::Max,
                input_type: Some(DataType::Float64),
                ..
            }]
        );
    let paired_global_count = if group_columns.is_empty() {
        paired_global_count_aggregate(table, aggregate_specs)
    } else {
        None
    };

    if group_columns.is_empty() {
        if let Some(PairedGlobalCountAggregate {
            count_state,
            aggregate_state,
            function,
            input_type,
            count_source,
        }) = paired_global_count
        {
            let mut present_count = None;
            let aggregate = match (function, input_type) {
                (AggregateFunction::Sum | AggregateFunction::Avg, DataType::Int64) => {
                    let partial = global_int64_sum_partial(
                        table,
                        matching_rows,
                        &aggregate_specs[aggregate_state],
                        parallelism,
                    )?;
                    present_count = Some(partial.count);
                    partial.into_state(function)
                }
                (AggregateFunction::Min, DataType::Int64) => min_global_int64(
                    table,
                    matching_rows,
                    &aggregate_specs[aggregate_state],
                    parallelism,
                ),
                (AggregateFunction::Min, DataType::Float64) => min_global_float64(
                    table,
                    matching_rows,
                    &aggregate_specs[aggregate_state],
                    parallelism,
                ),
                (AggregateFunction::Max, DataType::Int64) => max_global_int64(
                    table,
                    matching_rows,
                    &aggregate_specs[aggregate_state],
                    parallelism,
                ),
                (AggregateFunction::Max, DataType::Float64) => max_global_float64(
                    table,
                    matching_rows,
                    &aggregate_specs[aggregate_state],
                    parallelism,
                ),
                _ => unreachable!("paired aggregate shape is resolved"),
            };
            let count = match count_source {
                PairedGlobalCountSource::MatchedRows => count_matched_rows(matching_rows.len())?,
                PairedGlobalCountSource::AggregatePresentValues => count_present_values(
                    present_count.expect("same-column nullable SUM or AVG exposes a present count"),
                )?,
            };
            aggregate_states[count_state][0] = AggregateState::Count(count);
            aggregate_states[aggregate_state][0] = aggregate;
        } else {
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                if sole_global_nullable_int64_count {
                    states[0] = AggregateState::Count(count_global_nullable_int64(
                        table,
                        matching_rows,
                        spec,
                        parallelism,
                    )?);
                } else if spec.function == AggregateFunction::CountIf {
                    states[0] = AggregateState::Count(count_global_count_if(
                        table,
                        matching_rows,
                        spec,
                        parallelism,
                    )?);
                } else if sole_global_sum_int || sole_global_avg_int {
                    states[0] = sum_or_avg_global_int64(table, matching_rows, spec, parallelism)?;
                } else if sole_global_min_int {
                    states[0] = min_global_int64(table, matching_rows, spec, parallelism);
                } else if sole_global_min_float {
                    states[0] = min_global_float64(table, matching_rows, spec, parallelism);
                } else if sole_global_max_int {
                    states[0] = max_global_int64(table, matching_rows, spec, parallelism);
                } else if sole_global_max_float {
                    states[0] = max_global_float64(table, matching_rows, spec, parallelism);
                }
            }
        }
    }

    for row in matching_rows {
        let existing_group = groups.find(table, group_columns, *row, &mut multiple_key_probe);
        let (group, inserted) = if let Some(group) = existing_group {
            (group, false)
        } else {
            let next_group_count = group_count.saturating_add(1);
            enforce_resource_limit("SELECT groups", next_group_count, limits.max_groups)?;
            let next_key_cells = group_key_cells.saturating_add(key_cells_per_group);
            enforce_resource_limit(
                "SELECT group key cells",
                next_key_cells,
                limits.max_group_key_cells,
            )?;
            let next_key_bytes = group_key_bytes.saturating_add(key_bytes_per_group);
            enforce_resource_limit(
                "SELECT group key bytes",
                next_key_bytes,
                limits.max_group_key_bytes,
            )?;

            let group = group_count;
            groups.insert(table, group_columns, *row, group, &multiple_key_probe);
            group_count = next_group_count;
            group_key_cells = next_key_cells;
            group_key_bytes = next_key_bytes;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
            (group, true)
        };
        debug_assert!(!inserted || group + 1 == group_count);
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            if group_columns.is_empty()
                && (spec.function == AggregateFunction::CountIf
                    || sole_global_nullable_int64_count
                    || sole_global_sum_int
                    || sole_global_avg_int
                    || sole_global_min_int
                    || sole_global_min_float
                    || sole_global_max_int
                    || sole_global_max_float
                    || paired_global_count.is_some())
            {
                continue;
            }
            states[group].update(
                spec,
                table,
                *row,
                &mut aggregate_state_bytes,
                limits.max_aggregate_state_bytes,
            )?;
        }
    }

    let keys = groups.into_keys(group_count);
    let aggregates = aggregate_states
        .into_iter()
        .zip(aggregate_specs)
        .map(|(states, spec)| {
            states
                .into_iter()
                .map(|state| state.finish(spec))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GroupedData { keys, aggregates })
}

fn execute_grouped_bool_count<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    limits: QueryResultLimits,
    parallelism: GlobalAggregateParallelism,
) -> Result<Option<GroupedData<'a>>> {
    let [group_column] = group_columns else {
        return Ok(None);
    };
    let [
        spec @ AggregateSpec {
            function: AggregateFunction::Count,
            ..
        },
    ] = aggregate_specs
    else {
        return Ok(None);
    };
    let Column::Bool(values) = &table.columns()[*group_column] else {
        return Ok(None);
    };

    let partial = match (spec.argument, spec.input_type) {
        (None, None) => {
            reduce_grouped_bool_count(values, matching_rows, parallelism, grouped_bool_count_chunk)?
        }
        (Some(argument), Some(DataType::Int64)) if table.column_is_nullable_int64(argument) => {
            let Column::NullableInt64(count_values) = &table.columns()[argument] else {
                unreachable!("grouped nullable Int64 COUNT shape is resolved")
            };
            reduce_grouped_bool_count(values, matching_rows, parallelism, |values, rows| {
                grouped_bool_nullable_int64_count_chunk(values, count_values, rows)
            })?
        }
        _ => return Ok(None),
    };
    let group_count =
        usize::from(partial.row_count(false) > 0) + usize::from(partial.row_count(true) > 0);
    enforce_grouped_bool_limits(group_count, limits)?;

    let mut keys = Vec::with_capacity(group_count);
    let mut counts = Vec::with_capacity(group_count);
    if let Some(first) = partial.first_seen {
        keys.push(GroupKey::One(ValueRef::Bool(first)));
        counts.push(Value::Int64(partial.count(first)));
        let second = !first;
        if partial.row_count(second) > 0 {
            keys.push(GroupKey::One(ValueRef::Bool(second)));
            counts.push(Value::Int64(partial.count(second)));
        }
    }
    debug_assert_eq!(keys.len(), group_count);
    debug_assert_eq!(counts.len(), group_count);
    Ok(Some(GroupedData {
        keys,
        aggregates: vec![counts],
    }))
}

fn execute_grouped_bool_sum<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    limits: QueryResultLimits,
    parallelism: GlobalAggregateParallelism,
) -> Result<Option<GroupedData<'a>>> {
    let [group_column] = group_columns else {
        return Ok(None);
    };
    let [
        spec @ AggregateSpec {
            function: AggregateFunction::Sum,
            argument: Some(sum_column),
            input_type: Some(DataType::Int64),
        },
    ] = aggregate_specs
    else {
        return Ok(None);
    };
    let Column::Bool(group_values) = &table.columns()[*group_column] else {
        return Ok(None);
    };
    let Column::Int64(sum_values) = &table.columns()[*sum_column] else {
        return Ok(None);
    };

    let partial = reduce_grouped_bool_sum(
        group_values,
        sum_values,
        matching_rows,
        parallelism,
        grouped_bool_sum_chunk,
    )?;
    let group_count = usize::from(partial.present(false)) + usize::from(partial.present(true));
    enforce_grouped_bool_limits(group_count, limits)?;

    let mut keys = Vec::with_capacity(group_count);
    let mut sums = Vec::with_capacity(group_count);
    if let Some(first) = partial.first_seen {
        keys.push(GroupKey::One(ValueRef::Bool(first)));
        sums.push(partial.finished_sum(first, spec)?);
        let second = !first;
        if partial.present(second) {
            keys.push(GroupKey::One(ValueRef::Bool(second)));
            sums.push(partial.finished_sum(second, spec)?);
        }
    }
    debug_assert_eq!(keys.len(), group_count);
    debug_assert_eq!(sums.len(), group_count);
    Ok(Some(GroupedData {
        keys,
        aggregates: vec![sums],
    }))
}

fn enforce_grouped_bool_limits(group_count: usize, limits: QueryResultLimits) -> Result<()> {
    enforce_resource_limit("SELECT groups", group_count, limits.max_groups)?;
    enforce_resource_limit(
        "SELECT group key cells",
        group_count,
        limits.max_group_key_cells,
    )?;
    enforce_resource_limit(
        "SELECT group key bytes",
        group_count.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES),
        limits.max_group_key_bytes,
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GroupedBoolCountPartial {
    false_rows: i64,
    true_rows: i64,
    false_count: i64,
    true_count: i64,
    first_seen: Option<bool>,
}

impl GroupedBoolCountPartial {
    fn row_count(&self, value: bool) -> i64 {
        if value {
            self.true_rows
        } else {
            self.false_rows
        }
    }

    fn count(&self, value: bool) -> i64 {
        if value {
            self.true_count
        } else {
            self.false_count
        }
    }

    fn observe(&mut self, value: bool, counted: bool) -> Result<()> {
        self.first_seen.get_or_insert(value);
        let rows = if value {
            &mut self.true_rows
        } else {
            &mut self.false_rows
        };
        *rows = rows
            .checked_add(1)
            .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
        if counted {
            let count = if value {
                &mut self.true_count
            } else {
                &mut self.false_count
            };
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
        }
        Ok(())
    }
}

fn reduce_grouped_bool_count<C>(
    values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    chunk: C,
) -> Result<GroupedBoolCountPartial>
where
    C: Fn(&[bool], &[usize]) -> Result<GroupedBoolCountPartial> + Sync,
{
    reduce_grouped_bool(
        values,
        matching_rows,
        parallelism,
        "rusthouse-group-bool-count",
        chunk,
        reduce_grouped_bool_count_partials,
    )
}

fn reduce_grouped_bool<P, C, R>(
    values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_name_prefix: &'static str,
    chunk: C,
    reduce: R,
) -> Result<P>
where
    P: Send,
    C: Fn(&[bool], &[usize]) -> Result<P> + Sync,
    R: Fn(Vec<P>) -> Result<P>,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        worker_name_prefix,
        |rows| chunk(values, rows),
        reduce,
    )
}

fn grouped_bool_count_chunk(
    values: &[bool],
    matching_rows: &[usize],
) -> Result<GroupedBoolCountPartial> {
    let mut partial = GroupedBoolCountPartial::default();
    for row in matching_rows {
        let value = values[*row];
        partial.observe(value, true)?;
    }
    Ok(partial)
}

fn grouped_bool_nullable_int64_count_chunk(
    group_values: &[bool],
    count_values: &[Option<i64>],
    matching_rows: &[usize],
) -> Result<GroupedBoolCountPartial> {
    let mut partial = GroupedBoolCountPartial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], count_values[*row].is_some())?;
    }
    Ok(partial)
}

fn reduce_grouped_bool_count_partials(
    partials: Vec<GroupedBoolCountPartial>,
) -> Result<GroupedBoolCountPartial> {
    partials
        .into_iter()
        .try_fold(GroupedBoolCountPartial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            total.false_rows = total
                .false_rows
                .checked_add(partial.false_rows)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            total.true_rows = total
                .true_rows
                .checked_add(partial.true_rows)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            total.false_count = total
                .false_count
                .checked_add(partial.false_count)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            total.true_count = total
                .true_count
                .checked_add(partial.true_count)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            Ok(total)
        })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GroupedBoolSumPartial {
    false_sum: SumIntPartial,
    true_sum: SumIntPartial,
    first_seen: Option<bool>,
}

impl GroupedBoolSumPartial {
    fn partial(&self, value: bool) -> &SumIntPartial {
        if value {
            &self.true_sum
        } else {
            &self.false_sum
        }
    }

    fn partial_mut(&mut self, value: bool) -> &mut SumIntPartial {
        if value {
            &mut self.true_sum
        } else {
            &mut self.false_sum
        }
    }

    fn present(&self, value: bool) -> bool {
        self.partial(value).count > 0
    }

    fn observe(&mut self, group: bool, value: i64) -> Result<()> {
        self.first_seen.get_or_insert(group);
        let partial = self.partial_mut(group);
        partial.sum = partial
            .sum
            .checked_add(i128::from(value))
            .ok_or_else(|| Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))?;
        partial.count = partial
            .count
            .checked_add(1)
            .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
        Ok(())
    }

    fn finished_sum(&self, value: bool, spec: &AggregateSpec) -> Result<Value> {
        let partial = self.partial(value);
        AggregateState::SumInt {
            sum: partial.sum,
            count: partial.count,
        }
        .finish(spec)
    }
}

fn reduce_grouped_bool_sum<C>(
    group_values: &[bool],
    sum_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    chunk: C,
) -> Result<GroupedBoolSumPartial>
where
    C: Fn(&[bool], &[i64], &[usize]) -> Result<GroupedBoolSumPartial> + Sync,
{
    reduce_grouped_bool(
        group_values,
        matching_rows,
        parallelism,
        "rusthouse-group-bool-sum-int64",
        |group_values, rows| chunk(group_values, sum_values, rows),
        reduce_grouped_bool_sum_partials,
    )
}

fn grouped_bool_sum_chunk(
    group_values: &[bool],
    sum_values: &[i64],
    matching_rows: &[usize],
) -> Result<GroupedBoolSumPartial> {
    let mut partial = GroupedBoolSumPartial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], sum_values[*row])?;
    }
    Ok(partial)
}

fn reduce_grouped_bool_sum_partials(
    partials: Vec<GroupedBoolSumPartial>,
) -> Result<GroupedBoolSumPartial> {
    partials
        .into_iter()
        .try_fold(GroupedBoolSumPartial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            total.false_sum.sum = total
                .false_sum
                .sum
                .checked_add(partial.false_sum.sum)
                .ok_or_else(|| Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))?;
            total.false_sum.count = total
                .false_sum
                .count
                .checked_add(partial.false_sum.count)
                .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
            total.true_sum.sum = total
                .true_sum
                .sum
                .checked_add(partial.true_sum.sum)
                .ok_or_else(|| Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))?;
            total.true_sum.count = total
                .true_sum
                .count
                .checked_add(partial.true_sum.count)
                .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
            Ok(total)
        })
}

fn paired_global_count_aggregate(
    table: &Table,
    aggregate_specs: &[AggregateSpec],
) -> Option<PairedGlobalCountAggregate> {
    if aggregate_specs.len() != 2 {
        return None;
    }

    let count_state = aggregate_specs
        .iter()
        .position(|spec| spec.function == AggregateFunction::Count)?;
    let aggregate_state = aggregate_specs.iter().position(|spec| {
        (matches!(
            spec.function,
            AggregateFunction::Sum
                | AggregateFunction::Avg
                | AggregateFunction::Min
                | AggregateFunction::Max
        ) && spec.input_type == Some(DataType::Int64))
            || (matches!(
                spec.function,
                AggregateFunction::Min | AggregateFunction::Max
            ) && spec.input_type == Some(DataType::Float64))
    })?;
    let function = aggregate_specs[aggregate_state].function;
    let input_type = aggregate_specs[aggregate_state]
        .input_type
        .expect("paired aggregate input type is resolved");
    let nullable_int64 = table.column_is_nullable_int64(
        aggregate_specs[aggregate_state]
            .argument
            .expect("paired aggregate has a column argument"),
    );
    if nullable_int64
        && !matches!(
            function,
            AggregateFunction::Sum
                | AggregateFunction::Avg
                | AggregateFunction::Min
                | AggregateFunction::Max
        )
    {
        return None;
    }
    let count_source = match aggregate_specs[count_state].argument {
        None => PairedGlobalCountSource::MatchedRows,
        Some(count_argument)
            if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
                && nullable_int64
                && Some(count_argument) == aggregate_specs[aggregate_state].argument =>
        {
            PairedGlobalCountSource::AggregatePresentValues
        }
        Some(_) => return None,
    };
    (count_state != aggregate_state).then_some(PairedGlobalCountAggregate {
        count_state,
        aggregate_state,
        function,
        input_type,
        count_source,
    })
}

#[derive(Debug, Clone, Copy)]
struct PairedGlobalCountAggregate {
    count_state: usize,
    aggregate_state: usize,
    function: AggregateFunction,
    input_type: DataType,
    count_source: PairedGlobalCountSource,
}

#[derive(Debug, Clone, Copy)]
enum PairedGlobalCountSource {
    MatchedRows,
    AggregatePresentValues,
}

fn aggregate_argument_is_physical_int64(table: &Table, spec: &AggregateSpec) -> bool {
    spec.argument.is_some_and(|argument| {
        matches!(
            table.columns()[argument],
            Column::Int64(_) | Column::NullableInt64(_)
        )
    })
}

fn count_matched_rows(matched_rows: usize) -> Result<i64> {
    i64::try_from(matched_rows).map_err(|_| Error::NumericOverflow("COUNT".to_owned()))
}

fn count_present_values(present_count: u64) -> Result<i64> {
    i64::try_from(present_count).map_err(|_| Error::NumericOverflow("COUNT".to_owned()))
}

fn count_global_nullable_int64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> Result<i64> {
    debug_assert_eq!(spec.function, AggregateFunction::Count);
    debug_assert_eq!(spec.input_type, Some(DataType::Int64));
    let Column::NullableInt64(values) =
        &table.columns()[spec.argument.expect("nullable Int64 COUNT argument")]
    else {
        unreachable!("sole nullable Int64 COUNT shape is resolved")
    };
    reduce_global_nullable_int64_count(
        values,
        matching_rows,
        parallelism,
        nullable_int64_count_chunk,
    )
}

fn reduce_global_nullable_int64_count<C>(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    chunk: C,
) -> Result<i64>
where
    C: Fn(&[Option<i64>], &[usize]) -> Result<i64> + Sync,
{
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return chunk(values, matching_rows);
    };

    // Each lane receives one deterministic contiguous slice of the filtered
    // row indices, and checked partials are reduced in partition order. A
    // spawn failure or panic discards every partial and repeats the complete
    // NULL-aware count locally after releasing shared admission.
    let parallel_result = try_parallel_nullable_int64_count(
        values,
        matching_rows,
        admission.helper_threads(),
        &chunk,
    );
    drop(admission);
    parallel_result.unwrap_or_else(|| chunk(values, matching_rows))
}

fn try_parallel_nullable_int64_count<C>(
    values: &[Option<i64>],
    matching_rows: &[usize],
    helper_threads: usize,
    chunk: &C,
) -> Option<Result<i64>>
where
    C: Fn(&[Option<i64>], &[usize]) -> Result<i64> + Sync,
{
    debug_assert!(helper_threads > 0);
    let worker_count = helper_threads.saturating_add(1);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!("rusthouse-count-nullable-int64-{chunk_index}"))
                .spawn_scoped(scope, move || chunk(values, rows));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut partial_results = Vec::with_capacity(worker_count);
        partial_results.push(chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
        ));
        for handle in handles {
            match handle.join() {
                Ok(result) => partial_results.push(result),
                Err(_) => worker_failed = true,
            }
        }
        if worker_failed {
            return None;
        }

        Some(
            partial_results
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .and_then(reduce_nullable_int64_count_partials),
        )
    })
}

fn nullable_int64_count_chunk(values: &[Option<i64>], matching_rows: &[usize]) -> Result<i64> {
    matching_rows.iter().try_fold(0_i64, |count, row| {
        if values[*row].is_some() {
            count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))
        } else {
            Ok(count)
        }
    })
}

fn reduce_nullable_int64_count_partials(partial_counts: Vec<i64>) -> Result<i64> {
    partial_counts.into_iter().try_fold(0_i64, |total, count| {
        total
            .checked_add(count)
            .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))
    })
}

fn count_global_count_if(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> Result<i64> {
    debug_assert_eq!(spec.function, AggregateFunction::CountIf);
    let Column::Bool(values) = &table.columns()[spec.argument.expect("countIf argument")] else {
        unreachable!("countIf input type is resolved")
    };
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return count_if_chunk(values, matching_rows);
    };

    // Helper threads and the query thread each receive one deterministic,
    // contiguous slice of the already-filtered row index vector. If a helper
    // cannot be spawned or panics, discard every partial and repeat the
    // complete count locally after releasing the process-wide admission.
    let parallel_result = try_parallel_count_if(values, matching_rows, admission.helper_threads());
    drop(admission);
    parallel_result.unwrap_or_else(|| count_if_chunk(values, matching_rows))
}

fn try_parallel_count_if(
    values: &[bool],
    matching_rows: &[usize],
    helper_threads: usize,
) -> Option<Result<i64>> {
    debug_assert!(helper_threads > 0);
    let worker_count = helper_threads.saturating_add(1);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!("rusthouse-count-if-{chunk_index}"))
                .spawn_scoped(scope, move || count_if_chunk(values, rows));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut partial_results = Vec::with_capacity(worker_count);
        partial_results.push(count_if_chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
        ));
        for handle in handles {
            match handle.join() {
                Ok(result) => partial_results.push(result),
                Err(_) => worker_failed = true,
            }
        }
        if worker_failed {
            return None;
        }

        Some(
            partial_results
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .and_then(reduce_count_if_counts),
        )
    })
}

fn count_if_chunk(values: &[bool], matching_rows: &[usize]) -> Result<i64> {
    matching_rows.iter().try_fold(0_i64, |count, row| {
        if values[*row] {
            count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("countIf".to_owned()))
        } else {
            Ok(count)
        }
    })
}

fn reduce_count_if_counts(partial_counts: Vec<i64>) -> Result<i64> {
    partial_counts.into_iter().try_fold(0_i64, |total, count| {
        total
            .checked_add(count)
            .ok_or_else(|| Error::NumericOverflow("countIf".to_owned()))
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SumIntPartial {
    sum: i128,
    count: u64,
}

fn sum_or_avg_global_int64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> Result<AggregateState> {
    debug_assert!(matches!(
        spec.function,
        AggregateFunction::Sum | AggregateFunction::Avg
    ));
    debug_assert_eq!(spec.input_type, Some(DataType::Int64));
    global_int64_sum_partial(table, matching_rows, spec, parallelism)
        .map(|partial| partial.into_state(spec.function))
}

fn global_int64_sum_partial(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> Result<SumIntPartial> {
    debug_assert!(matches!(
        spec.function,
        AggregateFunction::Sum | AggregateFunction::Avg
    ));
    debug_assert_eq!(spec.input_type, Some(DataType::Int64));
    match &table.columns()[spec.argument.expect("SUM or AVG argument")] {
        Column::Int64(values) => reduce_global_int64_sum(
            values,
            matching_rows,
            parallelism,
            spec.function,
            sum_int64_chunk,
        ),
        Column::NullableInt64(values) => reduce_global_int64_sum(
            values,
            matching_rows,
            parallelism,
            spec.function,
            nullable_int64_chunk,
        ),
        _ => unreachable!("SUM or AVG input type and physical nullability are resolved"),
    }
}

fn reduce_global_int64_sum<T, C>(
    values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    function: AggregateFunction,
    chunk: C,
) -> Result<SumIntPartial>
where
    T: Sync,
    C: Fn(&[T], &[usize], AggregateFunction) -> Result<SumIntPartial> + Sync,
{
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return chunk(values, matching_rows, function);
    };

    // As with countIf, every lane receives a deterministic contiguous slice
    // of the filtered row indices. Partials are reduced in that same order.
    // A worker failure discards all partials and repeats the complete SUM or
    // AVG sum/count computation on the query thread after releasing its
    // process-wide admission.
    let parallel_result = try_parallel_sum_int64(
        values,
        matching_rows,
        admission.helper_threads(),
        function,
        &chunk,
    );
    drop(admission);
    parallel_result.unwrap_or_else(|| chunk(values, matching_rows, function))
}

fn try_parallel_sum_int64<T, C>(
    values: &[T],
    matching_rows: &[usize],
    helper_threads: usize,
    function: AggregateFunction,
    chunk: &C,
) -> Option<Result<SumIntPartial>>
where
    T: Sync,
    C: Fn(&[T], &[usize], AggregateFunction) -> Result<SumIntPartial> + Sync,
{
    debug_assert!(helper_threads > 0);
    debug_assert!(matches!(
        function,
        AggregateFunction::Sum | AggregateFunction::Avg
    ));
    let worker_count = helper_threads.saturating_add(1);
    let thread_label = match function {
        AggregateFunction::Sum => "sum",
        AggregateFunction::Avg => "avg",
        _ => unreachable!("only SUM and AVG share Int64 sum/count workers"),
    };
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!("rusthouse-{thread_label}-int64-{chunk_index}"))
                .spawn_scoped(scope, move || chunk(values, rows, function));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut partial_results = Vec::with_capacity(worker_count);
        partial_results.push(chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
            function,
        ));
        for handle in handles {
            match handle.join() {
                Ok(result) => partial_results.push(result),
                Err(_) => worker_failed = true,
            }
        }
        if worker_failed {
            return None;
        }

        Some(
            partial_results
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .and_then(|partials| reduce_sum_int64_partials(partials, function)),
        )
    })
}

fn sum_int64_chunk(
    values: &[i64],
    matching_rows: &[usize],
    function: AggregateFunction,
) -> Result<SumIntPartial> {
    matching_rows
        .iter()
        .try_fold(SumIntPartial::default(), |partial, row| {
            Ok(SumIntPartial {
                sum: partial
                    .sum
                    .checked_add(i128::from(values[*row]))
                    .ok_or_else(|| {
                        Error::NumericOverflow(int64_sum_overflow_context(function).to_owned())
                    })?,
                count: partial.count.checked_add(1).ok_or_else(|| {
                    Error::NumericOverflow(int64_count_overflow_context(function).to_owned())
                })?,
            })
        })
}

fn nullable_int64_chunk(
    values: &[Option<i64>],
    matching_rows: &[usize],
    function: AggregateFunction,
) -> Result<SumIntPartial> {
    debug_assert!(matches!(
        function,
        AggregateFunction::Sum | AggregateFunction::Avg
    ));
    matching_rows
        .iter()
        .try_fold(SumIntPartial::default(), |partial, row| {
            let Some(value) = values[*row] else {
                return Ok(partial);
            };
            Ok(SumIntPartial {
                sum: partial.sum.checked_add(i128::from(value)).ok_or_else(|| {
                    Error::NumericOverflow(int64_sum_overflow_context(function).to_owned())
                })?,
                count: partial.count.checked_add(1).ok_or_else(|| {
                    Error::NumericOverflow(int64_count_overflow_context(function).to_owned())
                })?,
            })
        })
}

fn reduce_sum_int64_partials(
    partials: Vec<SumIntPartial>,
    function: AggregateFunction,
) -> Result<SumIntPartial> {
    partials
        .into_iter()
        .try_fold(SumIntPartial::default(), |total, partial| {
            Ok(SumIntPartial {
                sum: total.sum.checked_add(partial.sum).ok_or_else(|| {
                    Error::NumericOverflow(int64_sum_overflow_context(function).to_owned())
                })?,
                count: total.count.checked_add(partial.count).ok_or_else(|| {
                    Error::NumericOverflow(int64_count_overflow_context(function).to_owned())
                })?,
            })
        })
}

fn int64_sum_overflow_context(function: AggregateFunction) -> &'static str {
    match function {
        AggregateFunction::Sum => "SUM(Int64) exact sum",
        AggregateFunction::Avg => "AVG(Int64) sum",
        _ => unreachable!("only SUM and AVG share Int64 sum/count partials"),
    }
}

fn int64_count_overflow_context(function: AggregateFunction) -> &'static str {
    match function {
        AggregateFunction::Sum => "SUM count",
        AggregateFunction::Avg => "AVG count",
        _ => unreachable!("only SUM and AVG share Int64 sum/count partials"),
    }
}

impl SumIntPartial {
    fn into_state(self, function: AggregateFunction) -> AggregateState {
        match function {
            AggregateFunction::Sum => AggregateState::SumInt {
                sum: self.sum,
                count: self.count,
            },
            AggregateFunction::Avg => AggregateState::AvgInt {
                sum: self.sum,
                count: self.count,
            },
            _ => unreachable!("only SUM and AVG share Int64 sum/count partials"),
        }
    }
}

fn min_global_int64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> AggregateState {
    debug_assert_eq!(spec.function, AggregateFunction::Min);
    debug_assert_eq!(spec.input_type, Some(DataType::Int64));
    let minimum = match &table.columns()[spec.argument.expect("MIN argument")] {
        Column::Int64(values) => {
            reduce_global_int64_extremum(values, matching_rows, parallelism, "min", i64::min)
        }
        Column::NullableInt64(values) => reduce_global_nullable_int64_extremum(
            values,
            matching_rows,
            parallelism,
            "min",
            i64::min,
        ),
        _ => unreachable!("MIN input type is resolved"),
    };
    min_int64_state(minimum)
}

fn reduce_global_int64_extremum<C>(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<i64>
where
    C: Fn(i64, i64) -> i64 + Sync,
{
    reduce_global_scalar_extremum(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "int64",
        |value| Some(*value),
        compare,
    )
}

fn reduce_global_nullable_int64_extremum<C>(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<i64>
where
    C: Fn(i64, i64) -> i64 + Sync,
{
    reduce_global_scalar_extremum(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "nullable-int64",
        |value| *value,
        compare,
    )
}

fn reduce_global_scalar_extremum<T, E, M, C>(
    values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    worker_type_label: &'static str,
    map: M,
    compare: C,
) -> Option<E>
where
    T: Sync,
    E: Copy + Send,
    M: Fn(&T) -> Option<E> + Sync,
    C: Fn(E, E) -> E + Sync,
{
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return scalar_extremum_chunk(values, matching_rows, &map, &compare);
    };

    // Each lane receives the same deterministic contiguous partition used by
    // the other global aggregates. Optional scalar partials are combined in
    // place, without allocating a partial-results collection. A failed spawn
    // or panic discards every partial and repeats the complete extremum on the
    // query thread after releasing process-wide admission.
    let helper_threads = admission.helper_threads();
    debug_assert!(helper_threads > 0);
    let worker_count = helper_threads.saturating_add(1);
    let map = &map;
    let compare = &compare;
    let parallel_result = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!(
                    "rusthouse-{worker_label}-{worker_type_label}-{chunk_index}"
                ))
                .spawn_scoped(scope, move || {
                    scalar_extremum_chunk(values, rows, map, compare)
                });
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut extremum = scalar_extremum_chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
            map,
            compare,
        );
        for handle in handles {
            match handle.join() {
                Ok(partial) => {
                    extremum = reduce_scalar_extremum_partials(extremum, partial, compare);
                }
                Err(_) => worker_failed = true,
            }
        }
        (!worker_failed).then_some(extremum)
    });
    drop(admission);
    parallel_result.unwrap_or_else(|| scalar_extremum_chunk(values, matching_rows, map, compare))
}

fn scalar_extremum_chunk<T, E, M, C>(
    values: &[T],
    matching_rows: &[usize],
    map: &M,
    compare: &C,
) -> Option<E>
where
    M: Fn(&T) -> Option<E>,
    C: Fn(E, E) -> E,
{
    matching_rows
        .iter()
        .filter_map(|row| map(&values[*row]))
        .reduce(compare)
}

fn reduce_scalar_extremum_partials<T, C>(
    left: Option<T>,
    right: Option<T>,
    compare: &C,
) -> Option<T>
where
    C: Fn(T, T) -> T,
{
    match (left, right) {
        (Some(left), Some(right)) => Some(compare(left, right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn min_int64_state(minimum: Option<i64>) -> AggregateState {
    AggregateState::Min(minimum.map(Value::Int64))
}

fn min_global_float64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> AggregateState {
    debug_assert_eq!(spec.function, AggregateFunction::Min);
    debug_assert_eq!(spec.input_type, Some(DataType::Float64));
    let Column::Float64(values) = &table.columns()[spec.argument.expect("MIN argument")] else {
        unreachable!("MIN input type is resolved")
    };
    AggregateState::Min(
        reduce_global_float64_extremum(
            values,
            matching_rows,
            parallelism,
            "min",
            first_float64_minimum,
        )
        .map(Value::Float64),
    )
}

fn reduce_global_float64_extremum<C>(
    values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<f64>
where
    C: Fn(f64, f64) -> f64 + Sync,
{
    reduce_global_scalar_extremum(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "float64",
        |value| Some(*value),
        compare,
    )
}

fn first_float64_minimum(left: f64, right: f64) -> f64 {
    if ValueRef::Float64(right) < ValueRef::Float64(left) {
        right
    } else {
        left
    }
}

fn max_global_int64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> AggregateState {
    debug_assert_eq!(spec.function, AggregateFunction::Max);
    debug_assert_eq!(spec.input_type, Some(DataType::Int64));
    let maximum = match &table.columns()[spec.argument.expect("MAX argument")] {
        Column::Int64(values) => {
            reduce_global_int64_extremum(values, matching_rows, parallelism, "max", i64::max)
        }
        Column::NullableInt64(values) => reduce_global_nullable_int64_extremum(
            values,
            matching_rows,
            parallelism,
            "max",
            i64::max,
        ),
        _ => unreachable!("MAX input type is resolved"),
    };
    max_int64_state(maximum)
}

fn max_int64_state(maximum: Option<i64>) -> AggregateState {
    AggregateState::Max(maximum.map(Value::Int64))
}

fn max_global_float64(
    table: &Table,
    matching_rows: &[usize],
    spec: &AggregateSpec,
    parallelism: GlobalAggregateParallelism,
) -> AggregateState {
    debug_assert_eq!(spec.function, AggregateFunction::Max);
    debug_assert_eq!(spec.input_type, Some(DataType::Float64));
    let Column::Float64(values) = &table.columns()[spec.argument.expect("MAX argument")] else {
        unreachable!("MAX input type is resolved")
    };
    AggregateState::Max(
        reduce_global_float64_extremum(
            values,
            matching_rows,
            parallelism,
            "max",
            first_float64_maximum,
        )
        .map(Value::Float64),
    )
}

fn first_float64_maximum(left: f64, right: f64) -> f64 {
    if ValueRef::Float64(right) > ValueRef::Float64(left) {
        right
    } else {
        left
    }
}

#[derive(Debug)]
enum GroupIndex<'a> {
    Global,
    One(HashMap<ValueRef<'a>, usize>),
    Multiple(HashMap<Box<[ValueRef<'a>]>, usize>),
}

impl<'a> GroupIndex<'a> {
    fn new(column_count: usize) -> Self {
        match column_count {
            0 => Self::Global,
            1 => Self::One(HashMap::new()),
            _ => Self::Multiple(HashMap::new()),
        }
    }

    fn find(
        &self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        multiple_key_probe: &mut Vec<ValueRef<'a>>,
    ) -> Option<usize> {
        match self {
            Self::Global => Some(0),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.get(&key).copied()
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.get(key.as_slice()).copied()
            }
            Self::Multiple(groups) => {
                multiple_key_probe.clear();
                multiple_key_probe.extend(
                    columns
                        .iter()
                        .map(|column| table.columns()[*column].value_ref(row)),
                );
                groups.get(multiple_key_probe.as_slice()).copied()
            }
        }
    }

    fn insert(
        &mut self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        group: usize,
        multiple_key_probe: &[ValueRef<'a>],
    ) {
        let previous = match self {
            Self::Global => unreachable!("global aggregation has no grouped key to insert"),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.insert(key, group)
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.insert(key.into(), group)
            }
            Self::Multiple(groups) => {
                debug_assert_eq!(multiple_key_probe.len(), columns.len());
                groups.insert(multiple_key_probe.into(), group)
            }
        };
        debug_assert!(previous.is_none(), "new group keys must be unique");
    }

    fn into_keys(self, group_count: usize) -> Vec<GroupKey<'a>> {
        let mut ordered = std::iter::repeat_with(|| None)
            .take(group_count)
            .collect::<Vec<_>>();
        match self {
            Self::Global => {
                debug_assert_eq!(group_count, 1);
                ordered[0] = Some(GroupKey::Empty);
            }
            Self::One(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::One(key));
                }
            }
            Self::Multiple(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::Multiple(key));
                }
            }
        }
        ordered
            .into_iter()
            .map(|key| key.expect("every group index has a key"))
            .collect()
    }
}

#[derive(Debug)]
enum GroupKey<'a> {
    Empty,
    One(ValueRef<'a>),
    Multiple(Box<[ValueRef<'a>]>),
}

impl GroupKey<'_> {
    fn value(&self, position: usize) -> ValueRef<'_> {
        match self {
            Self::Empty => unreachable!("a global aggregate has no grouped columns"),
            Self::One(value) if position == 0 => *value,
            Self::One(_) => unreachable!("single-column group position is zero"),
            Self::Multiple(values) => values[position],
        }
    }

    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Empty, Self::Empty) => Ordering::Equal,
            (Self::One(left), Self::One(right)) => left.cmp(right),
            (Self::Multiple(left), Self::Multiple(right)) => left.cmp(right),
            _ => unreachable!("all keys for a query have the same shape"),
        }
    }
}

#[derive(Debug)]
struct GroupedData<'a> {
    keys: Vec<GroupKey<'a>>,
    aggregates: Vec<Vec<Value>>,
}

impl GroupedData<'_> {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn project(&self, selected: &[usize], items: &[ResolvedItem]) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| match item {
                        ResolvedItem::Column {
                            group_position: Some(position),
                            ..
                        } => self.keys[*group].value(*position).to_owned(),
                        ResolvedItem::Column {
                            group_position: None,
                            ..
                        } => unreachable!("grouped columns are validated"),
                        ResolvedItem::Int64Subtract { .. } => {
                            unreachable!(
                                "Int64 subtraction projections are restricted to ungrouped queries"
                            )
                        }
                        ResolvedItem::IfNullInt64 {
                            fallback,
                            group_position: Some(position),
                            ..
                        } => Value::Int64(scalar_nullable_int64::if_null(
                            self.keys[*group].value(*position),
                            *fallback,
                        )),
                        ResolvedItem::IfNullInt64 {
                            group_position: None,
                            ..
                        } => unreachable!("grouped ifNull arguments are validated"),
                        ResolvedItem::IsNull {
                            group_position: Some(position),
                            ..
                        } => Value::Bool(matches!(
                            self.keys[*group].value(*position),
                            ValueRef::Null(_)
                        )),
                        ResolvedItem::IsNull {
                            group_position: None,
                            ..
                        } => unreachable!("grouped isNull arguments are validated"),
                        ResolvedItem::IsNotNull {
                            group_position: Some(position),
                            ..
                        } => Value::Bool(!matches!(
                            self.keys[*group].value(*position),
                            ValueRef::Null(_)
                        )),
                        ResolvedItem::IsNotNull {
                            group_position: None,
                            ..
                        } => unreachable!("grouped isNotNull arguments are validated"),
                        ResolvedItem::CastNullableInt64ToInt64 {
                            group_position: Some(position),
                            ..
                        } => self.keys[*group].value(*position).to_owned(),
                        ResolvedItem::CastNullableInt64ToInt64 {
                            group_position: None,
                            ..
                        } => unreachable!("grouped Nullable(Int64) CAST arguments are validated"),
                        ResolvedItem::CastInt64ToFloat64 { .. }
                        | ResolvedItem::CastBoolToFloat64 { .. }
                        | ResolvedItem::CastStringToFloat64 { .. }
                        | ResolvedItem::CastFloat64ToInt64 { .. }
                        | ResolvedItem::CastBoolToInt64 { .. }
                        | ResolvedItem::CastStringToInt64 { .. }
                        | ResolvedItem::CastInt64ToBool { .. }
                        | ResolvedItem::CastFloat64ToBool { .. }
                        | ResolvedItem::CastStringToBool { .. }
                        | ResolvedItem::CastInt64ToString { .. }
                        | ResolvedItem::CastFloat64ToString { .. }
                        | ResolvedItem::CastBoolToString { .. } => {
                            unreachable!("CAST projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::ToString { .. } => {
                            unreachable!("toString projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringLength { .. } => {
                            unreachable!("LENGTH projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringLengthUtf8 { .. } => {
                            unreachable!(
                                "lengthUTF8 projections are restricted to ungrouped queries"
                            )
                        }
                        ResolvedItem::StringEmpty { .. } => {
                            unreachable!("empty projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringLower { .. } => {
                            unreachable!("LOWER projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringUpper { .. } => {
                            unreachable!("UPPER projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Int64Abs { .. } => {
                            unreachable!("ABS projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Abs { .. } => {
                            unreachable!("ABS projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Round { .. } => {
                            unreachable!("ROUND projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Floor { .. } => {
                            unreachable!("FLOOR projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Ceil { .. } => {
                            unreachable!("CEIL projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::RowNumber => {
                            unreachable!(
                                "ROW_NUMBER projections are restricted to ungrouped queries"
                            )
                        }
                        ResolvedItem::Aggregate { state } => {
                            self.aggregates[*state][*group].clone()
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt { sum: i128, count: u64 },
    SumFloat { sum: ScaledFloatSum, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: ScaledFloatSum, count: u64 },
}

#[derive(Debug, Default)]
struct ScaledFloatSum {
    scale: f64,
    normalized_sum: f64,
    correction: f64,
}

impl ScaledFloatSum {
    fn add(&mut self, value: f64) {
        let magnitude = value.abs();
        if magnitude > self.scale {
            if self.scale != 0.0 {
                let ratio = self.scale / magnitude;
                self.normalized_sum *= ratio;
                self.correction *= ratio;
            }
            self.scale = magnitude;
        }
        if self.scale == 0.0 {
            return;
        }

        let normalized = value / self.scale;
        let next = self.normalized_sum + normalized;
        if self.normalized_sum.abs() >= normalized.abs() {
            self.correction += (self.normalized_sum - next) + normalized;
        } else {
            self.correction += (normalized - next) + self.normalized_sum;
        }
        self.normalized_sum = next;
    }

    fn normalized_total(&self) -> f64 {
        self.normalized_sum + self.correction
    }

    fn total(&self) -> f64 {
        self.normalized_total() * self.scale
    }

    fn mean(&self, count: u64) -> f64 {
        let normalized = (self.normalized_total() / count as f64).clamp(-1.0, 1.0);
        normalized * self.scale
    }
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count | AggregateFunction::CountIf => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => {
                Self::SumInt { sum: 0, count: 0 }
            }
            AggregateFunction::Sum => Self::SumFloat {
                sum: ScaledFloatSum::default(),
                count: 0,
            },
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat {
                sum: ScaledFloatSum::default(),
                count: 0,
            },
        }
    }

    fn update(
        &mut self,
        spec: &AggregateSpec,
        table: &Table,
        row: usize,
        aggregate_state_bytes: &mut usize,
        max_aggregate_state_bytes: usize,
    ) -> Result<()> {
        match self {
            Self::Count(count) => {
                let should_count = match spec.function {
                    AggregateFunction::Count => spec.argument.is_none_or(|argument| {
                        !matches!(table.columns()[argument].value_ref(row), ValueRef::Null(_))
                    }),
                    AggregateFunction::CountIf => {
                        let Column::Bool(values) =
                            &table.columns()[spec.argument.expect("countIf argument")]
                        else {
                            unreachable!("countIf input type is resolved")
                        };
                        values[row]
                    }
                    _ => unreachable!("only count functions use Count state"),
                };
                if should_count {
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| Error::NumericOverflow(spec.function.name().to_owned()))?;
                }
            }
            Self::SumInt { sum, count } => {
                let value = match &table.columns()[spec.argument.expect("SUM argument")] {
                    Column::Int64(values) => Some(values[row]),
                    Column::NullableInt64(values) => values[row],
                    _ => unreachable!("SUM input type is resolved"),
                };
                if let Some(value) = value {
                    *sum = sum
                        .checked_add(i128::from(value))
                        .ok_or_else(|| Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))?;
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
                }
            }
            Self::SumFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                sum.add(values[row]);
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
            }
            Self::Min(current) => {
                let column = &table.columns()[spec.argument.expect("MIN argument")];
                let candidate = column.value_ref(row);
                if !matches!(candidate, ValueRef::Null(_))
                    && current
                        .as_ref()
                        .is_none_or(|existing| candidate < existing.as_ref())
                {
                    replace_extreme(
                        current,
                        candidate,
                        aggregate_state_bytes,
                        max_aggregate_state_bytes,
                    )?;
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if !matches!(candidate, ValueRef::Null(_))
                    && current
                        .as_ref()
                        .is_none_or(|existing| candidate > existing.as_ref())
                {
                    replace_extreme(
                        current,
                        candidate,
                        aggregate_state_bytes,
                        max_aggregate_state_bytes,
                    )?;
                }
            }
            Self::AvgInt { sum, count } => {
                let value = match &table.columns()[spec.argument.expect("AVG argument")] {
                    Column::Int64(values) => Some(values[row]),
                    Column::NullableInt64(values) => values[row],
                    _ => unreachable!("AVG input type is resolved"),
                };
                if let Some(value) = value {
                    *sum = sum
                        .checked_add(i128::from(value))
                        .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
                }
            }
            Self::AvgFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                sum.add(values[row]);
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
        }
        Ok(())
    }

    fn finish(self, spec: &AggregateSpec) -> Result<Value> {
        match self {
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt { count: 0, .. } => Ok(Value::Null(DataType::Int64)),
            Self::SumInt { sum, .. } => i64::try_from(sum)
                .map(Value::Int64)
                .map_err(|_| Error::NumericOverflow("SUM(Int64)".to_owned())),
            Self::SumFloat { count: 0, .. } => Ok(Value::Null(DataType::Float64)),
            Self::SumFloat { sum, .. } => {
                let value = sum.total();
                if value.is_finite() {
                    Ok(Value::Float64(value))
                } else {
                    Err(Error::NumericOverflow("SUM(Float64)".to_owned()))
                }
            }
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum.mean(count))),
            Self::Min(None) | Self::Max(None) => Ok(Value::Null(
                spec.input_type.expect("MIN and MAX have column arguments"),
            )),
            Self::AvgInt { .. } | Self::AvgFloat { .. } => Ok(Value::Null(DataType::Float64)),
        }
    }
}

fn replace_extreme(
    current: &mut Option<Value>,
    candidate: ValueRef<'_>,
    aggregate_state_bytes: &mut usize,
    max_aggregate_state_bytes: usize,
) -> Result<()> {
    let previous_string_bytes = current
        .as_ref()
        .and_then(|value| match value {
            Value::String(value) => Some(value.len()),
            _ => None,
        })
        .unwrap_or(0);
    let candidate_string_bytes = match candidate {
        ValueRef::String(value) => value.len(),
        _ => 0,
    };
    let next_bytes = aggregate_state_bytes
        .saturating_sub(previous_string_bytes)
        .saturating_add(candidate_string_bytes);
    enforce_resource_limit(
        "SELECT aggregate state bytes",
        next_bytes,
        max_aggregate_state_bytes,
    )?;
    *current = Some(candidate.to_owned());
    *aggregate_state_bytes = next_bytes;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOrder {
    output: usize,
    descending: bool,
}

fn resolve_ordering(
    table: &Table,
    items: &[ResolvedItem],
    aggregate_specs: &[AggregateSpec],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    debug_assert_eq!(items.len(), columns.len());
    if requested.is_empty() {
        return Ok(Vec::new());
    }

    let expression_names = items
        .iter()
        .map(|item| resolved_expression_name(table, item, aggregate_specs))
        .collect::<Vec<_>>();
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let output_matches = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let matches = if output_matches.is_empty() {
            expression_names
                .iter()
                .enumerate()
                .filter(|(_, expression)| expression.eq_ignore_ascii_case(&order.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        } else {
            output_matches
        };
        match matches.as_slice() {
            [index] => ordering.push(ResolvedOrder {
                output: *index,
                descending: order.descending,
            }),
            [] => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY column or alias '{}' is not in the SELECT output",
                    order.name
                )));
            }
            _ => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY name '{}' is ambiguous",
                    order.name
                )));
            }
        }
    }
    Ok(ordering)
}

fn resolved_expression_name(
    table: &Table,
    item: &ResolvedItem,
    aggregate_specs: &[AggregateSpec],
) -> String {
    match item {
        ResolvedItem::Column { source, .. } => table.schema()[*source].name.clone(),
        ResolvedItem::Int64Subtract { source, literal } => {
            sql::int64_subtraction_name(&table.schema()[*source].name, *literal)
        }
        ResolvedItem::IfNullInt64 {
            source, fallback, ..
        } => sql::if_null_int64_name(&table.schema()[*source].name, *fallback),
        ResolvedItem::IsNull { source, .. } => sql::is_null_name(&table.schema()[*source].name),
        ResolvedItem::IsNotNull { source, .. } => {
            sql::is_not_null_name(&table.schema()[*source].name)
        }
        ResolvedItem::CastNullableInt64ToInt64 { source, .. } => {
            format!("CAST({} AS Int64)", table.schema()[*source].name)
        }
        ResolvedItem::CastInt64ToFloat64 { source } => {
            format!("CAST({} AS Float64)", table.schema()[*source].name)
        }
        ResolvedItem::CastBoolToFloat64 { source } => {
            format!("CAST({} AS Float64)", table.schema()[*source].name)
        }
        ResolvedItem::CastStringToFloat64 { source } => {
            format!("CAST({} AS Float64)", table.schema()[*source].name)
        }
        ResolvedItem::CastFloat64ToInt64 { source }
        | ResolvedItem::CastBoolToInt64 { source }
        | ResolvedItem::CastStringToInt64 { source } => {
            format!("CAST({} AS Int64)", table.schema()[*source].name)
        }
        ResolvedItem::CastInt64ToBool { source }
        | ResolvedItem::CastFloat64ToBool { source }
        | ResolvedItem::CastStringToBool { source } => {
            format!("CAST({} AS Bool)", table.schema()[*source].name)
        }
        ResolvedItem::CastInt64ToString { source }
        | ResolvedItem::CastFloat64ToString { source }
        | ResolvedItem::CastBoolToString { source } => {
            format!("CAST({} AS String)", table.schema()[*source].name)
        }
        ResolvedItem::ToString { source, .. } => {
            format!("toString({})", table.schema()[*source].name)
        }
        ResolvedItem::StringLength { source } => {
            format!("LENGTH({})", table.schema()[*source].name)
        }
        ResolvedItem::StringLengthUtf8 { source } => {
            format!("lengthUTF8({})", table.schema()[*source].name)
        }
        ResolvedItem::StringEmpty { source } => {
            format!("empty({})", table.schema()[*source].name)
        }
        ResolvedItem::StringLower { source } => {
            format!("LOWER({})", table.schema()[*source].name)
        }
        ResolvedItem::StringUpper { source } => {
            format!("UPPER({})", table.schema()[*source].name)
        }
        ResolvedItem::Int64Abs { source } | ResolvedItem::Float64Abs { source } => {
            format!("ABS({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Round { source } => {
            format!("ROUND({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Floor { source } => {
            format!("FLOOR({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Ceil { source } => {
            format!("CEIL({})", table.schema()[*source].name)
        }
        ResolvedItem::RowNumber => "ROW_NUMBER()".to_owned(),
        ResolvedItem::Aggregate { state } => {
            let spec = &aggregate_specs[*state];
            let argument = spec
                .argument
                .map(|source| table.schema()[source].name.as_str())
                .unwrap_or("*");
            format!("{}({argument})", spec.function.name())
        }
    }
}

fn order_source_rows(
    rows: &mut Vec<usize>,
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    max_ordering_state_bytes: usize,
) -> Result<()> {
    if ordering.is_empty() {
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        return Ok(());
    }

    if let [order] = ordering {
        match items[order.output] {
            ResolvedItem::StringLengthUtf8 { source } => {
                order_source_rows_by_length_utf8(
                    rows,
                    table,
                    source,
                    order.descending,
                    limit,
                    max_ordering_state_bytes,
                )?;
                return Ok(());
            }
            ResolvedItem::CastStringToFloat64 { source } => {
                order_source_rows_by_string_to_float64(
                    rows,
                    table,
                    source,
                    order.descending,
                    limit,
                    max_ordering_state_bytes,
                )?;
                return Ok(());
            }
            _ => {}
        }
    }
    if limit == Some(0) {
        rows.clear();
        return Ok(());
    }

    // A String-to-number ordering key must have valid numeric syntax for every
    // candidate row. Values outside the target range can still participate in
    // numeric ordering, so overflow remains deferred until LIMIT/OFFSET have
    // selected the rows that are actually converted.
    for order in ordering {
        match items[order.output] {
            ResolvedItem::CastStringToInt64 { source } => {
                for row in rows.iter().copied() {
                    validate_string_to_int64_syntax(string_at(table, source, row))?;
                }
            }
            ResolvedItem::CastStringToFloat64 { source } => {
                for row in rows.iter().copied() {
                    validate_string_to_float64_syntax(string_at(table, source, row))?;
                }
            }
            ResolvedItem::CastStringToBool { source } => {
                for row in rows.iter().copied() {
                    checked_string_to_bool(string_at(table, source, row))?;
                }
            }
            _ => {}
        }
    }

    sort_and_limit(rows, limit, |left, right| {
        for order in ordering {
            let comparison = match items[order.output] {
                ResolvedItem::Column { source, .. } => table.columns()[source].cmp_at(left, right),
                // Subtracting one constant is monotonic over present
                // mathematical integers and propagates NULL. Compare the
                // source values so NULL placement is preserved and overflow
                // is checked only after ordering and pagination select rows.
                ResolvedItem::Int64Subtract { source, .. } => {
                    table.columns()[source].cmp_at(left, right)
                }
                ResolvedItem::IfNullInt64 {
                    source, fallback, ..
                } => if_null_int64_at(table, source, left, fallback)
                    .cmp(&if_null_int64_at(table, source, right, fallback)),
                ResolvedItem::IsNull { source, .. } => {
                    is_null_at(table, source, left).cmp(&is_null_at(table, source, right))
                }
                ResolvedItem::IsNotNull { source, .. } => {
                    (!is_null_at(table, source, left)).cmp(&!is_null_at(table, source, right))
                }
                ResolvedItem::CastNullableInt64ToInt64 { source, .. } => {
                    table.columns()[source].cmp_at(left, right)
                }
                ResolvedItem::CastInt64ToFloat64 { source } => {
                    let left = int64_to_float64_at(table, source, left);
                    let right = int64_to_float64_at(table, source, right);
                    left.cmp(&right)
                }
                ResolvedItem::CastBoolToFloat64 { source } => {
                    bool_at(table, source, left).cmp(&bool_at(table, source, right))
                }
                ResolvedItem::CastStringToFloat64 { source } => {
                    let left = ordering_string_to_float64(string_at(table, source, left));
                    let right = ordering_string_to_float64(string_at(table, source, right));
                    ValueRef::Float64(left).cmp(&ValueRef::Float64(right))
                }
                ResolvedItem::CastFloat64ToInt64 { source } => {
                    let left = ValueRef::Float64(float64_at(table, source, left).trunc());
                    let right = ValueRef::Float64(float64_at(table, source, right).trunc());
                    left.cmp(&right)
                }
                ResolvedItem::CastBoolToInt64 { source } => {
                    bool_at(table, source, left).cmp(&bool_at(table, source, right))
                }
                ResolvedItem::CastStringToInt64 { source } => decimal_text_cmp(
                    string_at(table, source, left),
                    string_at(table, source, right),
                ),
                ResolvedItem::CastInt64ToBool { source } => {
                    let left = int64_to_bool_at(table, source, left);
                    let right = int64_to_bool_at(table, source, right);
                    left.cmp(&right)
                }
                ResolvedItem::CastFloat64ToBool { source } => (float64_at(table, source, left)
                    != 0.0)
                    .cmp(&(float64_at(table, source, right) != 0.0)),
                ResolvedItem::CastStringToBool { source } => {
                    let left = checked_string_to_bool(string_at(table, source, left))
                        .expect("String-to-Bool ordering syntax is validated");
                    let right = checked_string_to_bool(string_at(table, source, right))
                        .expect("String-to-Bool ordering syntax is validated");
                    left.cmp(&right)
                }
                ResolvedItem::CastInt64ToString { source } => {
                    stringified_cmp(table, source, left, right, DataType::Int64)
                }
                ResolvedItem::CastFloat64ToString { source } => {
                    stringified_cmp(table, source, left, right, DataType::Float64)
                }
                ResolvedItem::CastBoolToString { source } => {
                    stringified_cmp(table, source, left, right, DataType::Bool)
                }
                ResolvedItem::ToString { source, input_type } => {
                    stringified_cmp(table, source, left, right, input_type)
                }
                ResolvedItem::StringLength { source } => string_at(table, source, left)
                    .len()
                    .cmp(&string_at(table, source, right).len()),
                ResolvedItem::StringLengthUtf8 { source } => string_at(table, source, left)
                    .chars()
                    .count()
                    .cmp(&string_at(table, source, right).chars().count()),
                ResolvedItem::StringEmpty { source } => string_at(table, source, left)
                    .is_empty()
                    .cmp(&string_at(table, source, right).is_empty()),
                ResolvedItem::StringLower { source } => scalar_string::ascii_lower_cmp(
                    string_at(table, source, left),
                    string_at(table, source, right),
                ),
                ResolvedItem::StringUpper { source } => scalar_string::ascii_upper_cmp(
                    string_at(table, source, left),
                    string_at(table, source, right),
                ),
                // Comparing unsigned magnitudes preserves checked overflow as
                // a projection-time error. NULL follows the engine's normal
                // ordering without attempting to evaluate ABS.
                ResolvedItem::Int64Abs { source } => scalar_nullable_int64::abs_cmp(
                    table.columns()[source].value_ref(left),
                    table.columns()[source].value_ref(right),
                ),
                ResolvedItem::Float64Abs { source } => scalar_float64::abs_cmp(
                    float64_at(table, source, left),
                    float64_at(table, source, right),
                ),
                ResolvedItem::Float64Round { source } => scalar_float64::round_cmp(
                    float64_at(table, source, left),
                    float64_at(table, source, right),
                ),
                ResolvedItem::Float64Floor { source } => scalar_float64::floor_cmp(
                    float64_at(table, source, left),
                    float64_at(table, source, right),
                ),
                ResolvedItem::Float64Ceil { source } => scalar_float64::ceil_cmp(
                    float64_at(table, source, left),
                    float64_at(table, source, right),
                ),
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections cannot be ordered")
                }
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("ungrouped projections cannot contain aggregates")
                }
            };
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        left.cmp(&right)
    });
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct CachedLengthUtf8Order {
    row: usize,
    scalar_count: usize,
}

fn order_source_rows_by_length_utf8(
    rows: &mut Vec<usize>,
    table: &Table,
    source: usize,
    descending: bool,
    limit: Option<usize>,
    max_ordering_state_bytes: usize,
) -> Result<()> {
    // Keep one scalar count per filtered row so bounded selection never has to
    // rescan either String operand. Charge the complete cache before allocating
    // it; LIMIT and OFFSET cannot reduce this single-key working state.
    debug_assert_eq!(
        std::mem::size_of::<CachedLengthUtf8Order>(),
        LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES
    );
    let ordering_state_bytes = rows
        .len()
        .saturating_mul(LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES);
    enforce_resource_limit(
        "SELECT ordering state bytes",
        ordering_state_bytes,
        max_ordering_state_bytes,
    )?;
    if limit == Some(0) {
        rows.clear();
        return Ok(());
    }

    let mut cached = Vec::with_capacity(rows.len());
    cached.extend(rows.iter().copied().map(|row| CachedLengthUtf8Order {
        row,
        scalar_count: string_at(table, source, row).chars().count(),
    }));
    sort_and_limit_by(&mut cached, limit, |left, right| {
        let comparison = left.scalar_count.cmp(&right.scalar_count);
        let comparison = if descending {
            comparison.reverse()
        } else {
            comparison
        };
        comparison.then_with(|| left.row.cmp(&right.row))
    });

    rows.clear();
    rows.extend(cached.into_iter().map(|cached| cached.row));
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct CachedStringToFloat64Order {
    row: usize,
    key: f64,
}

fn order_source_rows_by_string_to_float64(
    rows: &mut Vec<usize>,
    table: &Table,
    source: usize,
    descending: bool,
    limit: Option<usize>,
    max_ordering_state_bytes: usize,
) -> Result<()> {
    // Keep the parsed key beside its source row so bounded selection never
    // reparses either operand. Charge every filtered candidate before the
    // cache allocation; LIMIT and OFFSET cannot reduce this working state.
    debug_assert_eq!(
        std::mem::size_of::<CachedStringToFloat64Order>(),
        STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES
    );
    let ordering_state_bytes = rows
        .len()
        .saturating_mul(STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES);
    enforce_resource_limit(
        "SELECT ordering state bytes",
        ordering_state_bytes,
        max_ordering_state_bytes,
    )?;
    if limit == Some(0) {
        rows.clear();
        return Ok(());
    }

    let mut cached = Vec::with_capacity(rows.len());
    for row in rows.iter().copied() {
        let value = string_at(table, source, row);
        validate_string_to_float64_syntax(value)?;
        cached.push(CachedStringToFloat64Order {
            row,
            key: ordering_string_to_float64(value),
        });
    }
    sort_and_limit_by(&mut cached, limit, |left, right| {
        let comparison = ValueRef::Float64(left.key).cmp(&ValueRef::Float64(right.key));
        let comparison = if descending {
            comparison.reverse()
        } else {
            comparison
        };
        comparison.then_with(|| left.row.cmp(&right.row))
    });

    rows.clear();
    rows.extend(cached.into_iter().map(|cached| cached.row));
    Ok(())
}

fn order_grouped_rows(
    groups: &mut Vec<usize>,
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    sort_and_limit(groups, limit, |left, right| {
        for order in ordering {
            let comparison = match items[order.output] {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => data.keys[left]
                    .value(position)
                    .cmp(&data.keys[right].value(position)),
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Int64Subtract { .. } => {
                    unreachable!(
                        "Int64 subtraction projections are restricted to ungrouped queries"
                    )
                }
                ResolvedItem::IfNullInt64 {
                    fallback,
                    group_position: Some(position),
                    ..
                } => scalar_nullable_int64::if_null(data.keys[left].value(position), fallback).cmp(
                    &scalar_nullable_int64::if_null(data.keys[right].value(position), fallback),
                ),
                ResolvedItem::IfNullInt64 {
                    group_position: None,
                    ..
                } => unreachable!("grouped ifNull arguments are validated"),
                ResolvedItem::IsNull {
                    group_position: Some(position),
                    ..
                } => matches!(data.keys[left].value(position), ValueRef::Null(_)).cmp(&matches!(
                    data.keys[right].value(position),
                    ValueRef::Null(_)
                )),
                ResolvedItem::IsNull {
                    group_position: None,
                    ..
                } => unreachable!("grouped isNull arguments are validated"),
                ResolvedItem::IsNotNull {
                    group_position: Some(position),
                    ..
                } => (!matches!(data.keys[left].value(position), ValueRef::Null(_))).cmp(
                    &!matches!(data.keys[right].value(position), ValueRef::Null(_)),
                ),
                ResolvedItem::IsNotNull {
                    group_position: None,
                    ..
                } => unreachable!("grouped isNotNull arguments are validated"),
                ResolvedItem::CastNullableInt64ToInt64 {
                    group_position: Some(position),
                    ..
                } => data.keys[left]
                    .value(position)
                    .cmp(&data.keys[right].value(position)),
                ResolvedItem::CastNullableInt64ToInt64 {
                    group_position: None,
                    ..
                } => unreachable!("grouped Nullable(Int64) CAST arguments are validated"),
                ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastBoolToFloat64 { .. }
                | ResolvedItem::CastStringToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastStringToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastStringToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastFloat64ToString { .. }
                | ResolvedItem::CastBoolToString { .. } => {
                    unreachable!("CAST projections are restricted to ungrouped queries")
                }
                ResolvedItem::ToString { .. } => {
                    unreachable!("toString projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLength { .. } => {
                    unreachable!("LENGTH projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLengthUtf8 { .. } => {
                    unreachable!("lengthUTF8 projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringEmpty { .. } => {
                    unreachable!("empty projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLower { .. } => {
                    unreachable!("LOWER projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringUpper { .. } => {
                    unreachable!("UPPER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Int64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Round { .. } => {
                    unreachable!("ROUND projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Floor { .. } => {
                    unreachable!("FLOOR projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Ceil { .. } => {
                    unreachable!("CEIL projections are restricted to ungrouped queries")
                }
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Aggregate { state } => {
                    data.aggregates[state][left].cmp(&data.aggregates[state][right])
                }
            };
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        data.keys[left].cmp(&data.keys[right])
    });
}

fn int64_to_float64_at(table: &Table, source: usize, row: usize) -> ValueRef<'_> {
    match table.columns()[source].value_ref(row) {
        ValueRef::Int64(value) => ValueRef::Float64(value as f64),
        ValueRef::Null(DataType::Int64) => ValueRef::Null(DataType::Float64),
        _ => unreachable!("CAST input type is resolved"),
    }
}

fn is_null_at(table: &Table, source: usize, row: usize) -> bool {
    matches!(
        table.columns()[source].value_ref(row),
        ValueRef::Null(DataType::Int64)
    )
}

fn int64_to_bool_at(table: &Table, source: usize, row: usize) -> ValueRef<'_> {
    match table.columns()[source].value_ref(row) {
        ValueRef::Int64(value) => ValueRef::Bool(value != 0),
        ValueRef::Null(DataType::Int64) => ValueRef::Null(DataType::Bool),
        _ => unreachable!("CAST input type is resolved"),
    }
}

fn float64_at(table: &Table, source: usize, row: usize) -> f64 {
    let Column::Float64(values) = &table.columns()[source] else {
        unreachable!("CAST input type is resolved")
    };
    values[row]
}

fn bool_at(table: &Table, source: usize, row: usize) -> bool {
    let Column::Bool(values) = &table.columns()[source] else {
        unreachable!("CAST input type is resolved")
    };
    values[row]
}

fn checked_float64_to_int64(value: f64) -> Result<i64> {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

    if !value.is_finite() || value < i64::MIN as f64 || value >= I64_UPPER_EXCLUSIVE {
        return Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned()));
    }
    Ok(value.trunc() as i64)
}

fn string_at(table: &Table, source: usize, row: usize) -> &str {
    let Column::String(values) = &table.columns()[source] else {
        unreachable!("String scalar input type is resolved")
    };
    &values[row]
}

fn stringify_value(table: &Table, source: usize, row: usize, input_type: DataType) -> Value {
    match table.columns()[source].value_ref(row) {
        ValueRef::Null(_) => Value::Null(DataType::String),
        ValueRef::Int64(value) => {
            debug_assert_eq!(input_type, DataType::Int64);
            Value::String(scalar_text::render_int64(value))
        }
        ValueRef::Float64(value) => {
            debug_assert_eq!(input_type, DataType::Float64);
            Value::String(scalar_text::render_float64(value))
        }
        ValueRef::Bool(value) => {
            debug_assert_eq!(input_type, DataType::Bool);
            Value::String(scalar_text::render_bool(value))
        }
        ValueRef::String(value) => {
            debug_assert_eq!(input_type, DataType::String);
            Value::String(value.to_owned())
        }
    }
}

fn stringified_len(table: &Table, source: usize, row: usize, input_type: DataType) -> usize {
    match table.columns()[source].value_ref(row) {
        ValueRef::Null(_) => 0,
        ValueRef::Int64(value) => {
            debug_assert_eq!(input_type, DataType::Int64);
            scalar_text::int64_len(value)
        }
        ValueRef::Float64(value) => {
            debug_assert_eq!(input_type, DataType::Float64);
            scalar_text::float64_len(value)
        }
        ValueRef::Bool(value) => {
            debug_assert_eq!(input_type, DataType::Bool);
            scalar_text::bool_len(value)
        }
        ValueRef::String(value) => {
            debug_assert_eq!(input_type, DataType::String);
            value.len()
        }
    }
}

fn stringified_cmp(
    table: &Table,
    source: usize,
    left: usize,
    right: usize,
    input_type: DataType,
) -> Ordering {
    match (
        table.columns()[source].value_ref(left),
        table.columns()[source].value_ref(right),
    ) {
        (ValueRef::Null(_), ValueRef::Null(_)) => Ordering::Equal,
        (ValueRef::Null(_), _) => Ordering::Less,
        (_, ValueRef::Null(_)) => Ordering::Greater,
        (ValueRef::Int64(left), ValueRef::Int64(right)) => {
            debug_assert_eq!(input_type, DataType::Int64);
            scalar_text::int64_cmp(left, right)
        }
        (ValueRef::Float64(left), ValueRef::Float64(right)) => {
            debug_assert_eq!(input_type, DataType::Float64);
            scalar_text::float64_cmp(left, right)
        }
        (ValueRef::Bool(left), ValueRef::Bool(right)) => {
            debug_assert_eq!(input_type, DataType::Bool);
            scalar_text::bool_cmp(left, right)
        }
        (ValueRef::String(left), ValueRef::String(right)) => {
            debug_assert_eq!(input_type, DataType::String);
            left.cmp(right)
        }
        _ => unreachable!("toString input type is resolved"),
    }
}

fn if_null_int64_at(table: &Table, source: usize, row: usize, fallback: i64) -> i64 {
    scalar_nullable_int64::if_null(table.columns()[source].value_ref(row), fallback)
}

fn sort_and_limit(
    indices: &mut Vec<usize>,
    limit: Option<usize>,
    compare: impl Fn(usize, usize) -> Ordering,
) {
    sort_and_limit_by(indices, limit, |left, right| compare(*left, *right));
}

fn sort_and_limit_by<T>(
    values: &mut Vec<T>,
    limit: Option<usize>,
    compare: impl Fn(&T, &T) -> Ordering,
) {
    if let Some(0) = limit {
        values.clear();
        return;
    }
    if let Some(limit) = limit.filter(|limit| *limit < values.len()) {
        values.select_nth_unstable_by(limit, &compare);
        values.truncate(limit);
    }
    values.sort_unstable_by(compare);
}

#[derive(Debug)]
enum CompiledPredicate {
    Comparison {
        left: CompiledOperand,
        operator: ComparisonOperator,
        right: CompiledOperand,
    },
    Nullness {
        column: usize,
        is_null: bool,
    },
    LikePrefix {
        column: usize,
        prefix: String,
        negated: bool,
    },
    LikeSuffix {
        column: usize,
        suffix: String,
        negated: bool,
    },
    LikeContains {
        column: usize,
        substring: String,
        negated: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    /// Returns a direct comparison suitable for metadata pruning.
    fn int64_filter(&self) -> Option<(usize, Int64Filter)> {
        let Self::Comparison {
            left,
            operator,
            right,
        } = self
        else {
            return None;
        };

        let (column, value, operator) = match (left, right) {
            (
                CompiledOperand::Column {
                    index,
                    data_type: DataType::Int64,
                },
                CompiledOperand::Literal(Value::Int64(value)),
            ) => (*index, *value, *operator),
            (
                CompiledOperand::Literal(Value::Int64(value)),
                CompiledOperand::Column {
                    index,
                    data_type: DataType::Int64,
                },
            ) => (*index, *value, reverse_comparison(*operator)),
            _ => return None,
        };
        let filter = match operator {
            ComparisonOperator::Equal => Int64Filter::Equal(value),
            ComparisonOperator::Less => Int64Filter::Less(value),
            ComparisonOperator::LessOrEqual => Int64Filter::LessOrEqual(value),
            ComparisonOperator::Greater => Int64Filter::Greater(value),
            ComparisonOperator::GreaterOrEqual => Int64Filter::GreaterOrEqual(value),
            ComparisonOperator::NotEqual => return None,
        };
        Some((column, filter))
    }

    /// Returns an exact comparison or positive two-sided range suitable for
    /// validated range-partition routing. Every admitted row is still checked
    /// by the complete predicate evaluator.
    fn int64_partition_filter(&self) -> Option<(usize, Int64Filter)> {
        self.int64_filter().or_else(|| self.int64_range_filter())
    }

    /// Returns the shapes that an `Int64` min/max index can reject safely.
    /// This includes any exact positive two-sided range conjunction after
    /// predicate normalization. Every surviving row is still evaluated by
    /// `self`.
    fn int64_index_filter(&self) -> Option<(usize, Int64Filter)> {
        self.int64_partition_filter()
    }

    fn int64_range_filter(&self) -> Option<(usize, Int64Filter)> {
        let Self::And(first, second) = self else {
            return None;
        };
        let (first_column, first_filter) = first.int64_filter()?;
        let (second_column, second_filter) = second.int64_filter()?;
        let (lower_column, lower, lower_strict, upper_column, upper, upper_strict) =
            match (first_filter, second_filter) {
                (Int64Filter::GreaterOrEqual(lower), Int64Filter::LessOrEqual(upper)) => {
                    (first_column, lower, false, second_column, upper, false)
                }
                (Int64Filter::LessOrEqual(upper), Int64Filter::GreaterOrEqual(lower)) => {
                    (second_column, lower, false, first_column, upper, false)
                }
                (Int64Filter::Greater(lower), Int64Filter::LessOrEqual(upper)) => {
                    (first_column, lower, true, second_column, upper, false)
                }
                (Int64Filter::LessOrEqual(upper), Int64Filter::Greater(lower)) => {
                    (second_column, lower, true, first_column, upper, false)
                }
                (Int64Filter::GreaterOrEqual(lower), Int64Filter::Less(upper)) => {
                    (first_column, lower, false, second_column, upper, true)
                }
                (Int64Filter::Less(upper), Int64Filter::GreaterOrEqual(lower)) => {
                    (second_column, lower, false, first_column, upper, true)
                }
                (Int64Filter::Greater(lower), Int64Filter::Less(upper)) => {
                    (first_column, lower, true, second_column, upper, true)
                }
                (Int64Filter::Less(upper), Int64Filter::Greater(lower)) => {
                    (second_column, lower, true, first_column, upper, true)
                }
                _ => return None,
            };
        if lower_column != upper_column {
            return None;
        }
        Some((
            lower_column,
            normalized_int64_range(lower, lower_strict, upper, upper_strict),
        ))
    }

    fn int64_nullness(&self) -> Option<(usize, bool)> {
        let Self::Nullness { column, is_null } = self else {
            return None;
        };
        Some((*column, *is_null))
    }

    fn evaluate(&self, table: &Table, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(table, row);
                let right = right.value(table, row);
                let Some(comparison) = left.sql_cmp(right) else {
                    return false;
                };
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            Self::Nullness { column, is_null } => {
                matches!(table.columns()[*column].value_ref(row), ValueRef::Null(_)) == *is_null
            }
            Self::LikePrefix {
                column,
                prefix,
                negated,
            } => string_at(table, *column, row).starts_with(prefix.as_str()) != *negated,
            Self::LikeSuffix {
                column,
                suffix,
                negated,
            } => string_at(table, *column, row).ends_with(suffix.as_str()) != *negated,
            Self::LikeContains {
                column,
                substring,
                negated,
            } => string_at(table, *column, row).contains(substring.as_str()) != *negated,
            Self::And(left, right) => left.evaluate(table, row) && right.evaluate(table, row),
            Self::Or(left, right) => left.evaluate(table, row) || right.evaluate(table, row),
        }
    }
}

fn normalized_int64_range(
    lower: i64,
    lower_strict: bool,
    upper: i64,
    upper_strict: bool,
) -> Int64Filter {
    let lower = if lower_strict {
        let Some(lower) = lower.checked_add(1) else {
            return empty_int64_range();
        };
        lower
    } else {
        lower
    };
    let upper = if upper_strict {
        let Some(upper) = upper.checked_sub(1) else {
            return empty_int64_range();
        };
        upper
    } else {
        upper
    };
    Int64Filter::InclusiveRange { lower, upper }
}

const fn empty_int64_range() -> Int64Filter {
    // Both metadata paths already treat lower > upper as an empty range.
    Int64Filter::InclusiveRange {
        lower: i64::MAX,
        upper: i64::MIN,
    }
}

#[derive(Debug)]
enum CompiledOperand {
    Column { index: usize, data_type: DataType },
    Literal(Value),
}

impl CompiledOperand {
    fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
        }
    }

    fn value<'a>(&'a self, table: &'a Table, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => table.columns()[*index].value_ref(row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(table: &Table, predicate: &Predicate) -> Result<CompiledPredicate> {
    compile_predicate_with_polarity(table, predicate, false)
}

fn compile_predicate_with_polarity(
    table: &Table,
    predicate: &Predicate,
    negated: bool,
) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(table, left)?;
            let right = compile_operand(table, right)?;
            if !comparable(left.data_type(), right.data_type()) {
                return Err(Error::TypeMismatch {
                    context: "WHERE comparison".to_owned(),
                    expected: left.data_type().to_string(),
                    actual: right.data_type().to_string(),
                });
            }
            Ok(CompiledPredicate::Comparison {
                left,
                operator: if negated {
                    invert_comparison(*operator)
                } else {
                    *operator
                },
                right,
            })
        }
        Predicate::IsNull { column } | Predicate::IsNotNull { column } => {
            let column_index = table.column_index(column)?;
            let is_null = matches!(predicate, Predicate::IsNull { .. }) != negated;
            Ok(CompiledPredicate::Nullness {
                column: column_index,
                is_null,
            })
        }
        Predicate::LikePrefix { column, prefix } => {
            let column_index = table.column_index(column)?;
            let actual = table.schema()[column_index].data_type;
            if actual != DataType::String {
                return Err(Error::TypeMismatch {
                    context: format!("WHERE LIKE column '{column}'"),
                    expected: DataType::String.to_string(),
                    actual: actual.to_string(),
                });
            }
            Ok(CompiledPredicate::LikePrefix {
                column: column_index,
                prefix: prefix.clone(),
                negated,
            })
        }
        Predicate::LikeSuffix { column, suffix } => {
            let column_index = table.column_index(column)?;
            let actual = table.schema()[column_index].data_type;
            if actual != DataType::String {
                return Err(Error::TypeMismatch {
                    context: format!("WHERE LIKE column '{column}'"),
                    expected: DataType::String.to_string(),
                    actual: actual.to_string(),
                });
            }
            Ok(CompiledPredicate::LikeSuffix {
                column: column_index,
                suffix: suffix.clone(),
                negated,
            })
        }
        Predicate::LikeContains { column, substring } => {
            let column_index = table.column_index(column)?;
            let actual = table.schema()[column_index].data_type;
            if actual != DataType::String {
                return Err(Error::TypeMismatch {
                    context: format!("WHERE LIKE column '{column}'"),
                    expected: DataType::String.to_string(),
                    actual: actual.to_string(),
                });
            }
            Ok(CompiledPredicate::LikeContains {
                column: column_index,
                substring: substring.clone(),
                negated,
            })
        }
        Predicate::Not(predicate) => compile_predicate_with_polarity(table, predicate, !negated),
        Predicate::And(left, right) if negated => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate_with_polarity(table, left, true)?),
            Box::new(compile_predicate_with_polarity(table, right, true)?),
        )),
        Predicate::And(left, right) => Ok(CompiledPredicate::And(
            Box::new(compile_predicate_with_polarity(table, left, false)?),
            Box::new(compile_predicate_with_polarity(table, right, false)?),
        )),
        Predicate::Or(left, right) if negated => Ok(CompiledPredicate::And(
            Box::new(compile_predicate_with_polarity(table, left, true)?),
            Box::new(compile_predicate_with_polarity(table, right, true)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate_with_polarity(table, left, false)?),
            Box::new(compile_predicate_with_polarity(table, right, false)?),
        )),
    }
}

const fn invert_comparison(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::NotEqual,
        ComparisonOperator::NotEqual => ComparisonOperator::Equal,
        ComparisonOperator::Less => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::LessOrEqual => ComparisonOperator::Greater,
        ComparisonOperator::Greater => ComparisonOperator::LessOrEqual,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::Less,
    }
}

const fn reverse_comparison(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::Equal,
        ComparisonOperator::NotEqual => ComparisonOperator::NotEqual,
        ComparisonOperator::Less => ComparisonOperator::Greater,
        ComparisonOperator::LessOrEqual => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::Greater => ComparisonOperator::Less,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::LessOrEqual,
    }
}

fn compile_operand(table: &Table, operand: &Operand) -> Result<CompiledOperand> {
    let name = match operand {
        Operand::Column(name) => name.as_str(),
        Operand::SharedColumn(name) => name.as_ref(),
        Operand::Literal(value) => {
            validate_predicate_literal_value(value)?;
            return Ok(CompiledOperand::Literal(value.clone()));
        }
    };
    let index = table.column_index(name)?;
    Ok(CompiledOperand::Column {
        index,
        data_type: table.schema()[index].data_type,
    })
}

fn validate_predicate_literal_value(value: &Value) -> Result<()> {
    match value {
        Value::Null(_) => Err(Error::InvalidQuery(
            "WHERE comparisons do not support NULL literals".to_owned(),
        )),
        Value::Float64(value) if !value.is_finite() => Err(Error::InvalidQuery(
            "WHERE comparison Float64 literals must be finite".to_owned(),
        )),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

fn comparable(left: DataType, right: DataType) -> bool {
    left == right
        || matches!(
            (left, right),
            (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(database: &mut Database, sql: &str) -> QueryResult {
        let results = database.execute(sql).expect("query succeeds");
        match results.into_iter().last().expect("one result") {
            StatementResult::Query(result) => result,
            StatementResult::Command { .. } => panic!("expected query result"),
        }
    }

    fn query_with_max_threads(database: &Database, sql: &str, max_threads: usize) -> QueryResult {
        let mut statements = sql::parse(sql).expect("query parses");
        assert_eq!(statements.len(), 1, "one query statement");
        database
            .execute_query_statement_with_parameterized_limits(
                statements.pop().expect("one query statement"),
                ParameterizedQueryLimits {
                    max_result_bytes: DEFAULT_MAX_RETAINED_RESULT_BYTES,
                    max_result_rows: 0,
                    max_result_values: 0,
                    max_scan_rows: 0,
                    max_groups: 0,
                    max_group_key_cells: 0,
                    max_group_key_bytes: 0,
                    max_ordering_state_bytes: 0,
                    max_aggregate_state_cells: 0,
                    max_aggregate_state_bytes: 0,
                    max_threads,
                },
            )
            .expect("parameterized query succeeds")
    }

    fn count_if_database(row_count: usize) -> Database {
        count_if_database_with_worker_cap(
            row_count,
            NonZeroUsize::new(DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP)
                .expect("the default aggregate worker cap is nonzero"),
        )
    }

    fn count_if_database_with_worker_cap(row_count: usize, worker_cap: NonZeroUsize) -> Database {
        let mut database = Database::with_global_aggregate_worker_cap(worker_cap);
        database
            .execute(
                "CREATE TABLE empty_events (active Bool); \
                 CREATE TABLE events (id Int64, score Float64, active Bool, included Bool);",
            )
            .expect("countIf differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| format!("({id}, {id}.0, {}, {})", id % 2 == 0, id % 3 != 0))
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO events VALUES {rows}"))
                    .expect("countIf differential rows");
            }
        }
        database
    }

    fn grouped_bool_count_database(row_count: usize) -> Database {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE bool_events (id Int64, active Bool);")
            .expect("grouped Bool COUNT differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| format!("({id}, {})", id == 1))
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO bool_events VALUES {rows}"))
                    .expect("grouped Bool COUNT differential rows");
            }
        }
        database
    }

    fn grouped_bool_nullable_count_database(
        row_count: usize,
        row_values: impl Fn(usize) -> (bool, Option<i64>),
    ) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE bool_nullable_events (id Int64, active Bool); \
                 ALTER TABLE bool_nullable_events ADD COLUMN measurement Nullable(Int64);",
            )
            .expect("grouped Bool nullable COUNT differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (active, measurement) = row_values(id);
                        let measurement = measurement.map_or_else(
                            || "NULL".to_owned(),
                            |measurement| measurement.to_string(),
                        );
                        format!("({id}, {active}, {measurement})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO bool_nullable_events VALUES {rows}"))
                    .expect("grouped Bool nullable COUNT differential rows");
            }
        }
        database
    }

    fn grouped_bool_sum_database(
        row_count: usize,
        row_values: impl Fn(usize) -> (i64, bool, bool),
    ) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE bool_sum_events \
                 (id Int64, value Int64, active Bool, included Bool);",
            )
            .expect("grouped Bool SUM differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, active, included) = row_values(id);
                        format!("({id}, {value}, {active}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO bool_sum_events VALUES {rows}"))
                    .expect("grouped Bool SUM differential rows");
            }
        }
        database
    }

    fn sum_int64_database(row_count: usize, row_values: impl Fn(usize) -> (i64, bool)) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_values (value Int64); \
                 CREATE TABLE values_to_sum (id Int64, value Int64, included Bool);",
            )
            .expect("SUM(Int64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO values_to_sum VALUES {rows}"))
                    .expect("SUM(Int64) differential rows");
            }
        }
        database
    }

    fn avg_int64_database(row_count: usize, row_values: impl Fn(usize) -> (i64, bool)) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_avg_values (value Int64); \
                 CREATE TABLE values_to_avg (id Int64, value Int64, included Bool);",
            )
            .expect("AVG(Int64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO values_to_avg VALUES {rows}"))
                    .expect("AVG(Int64) differential rows");
            }
        }
        database
    }

    fn min_int64_database(row_count: usize, row_values: impl Fn(usize) -> (i64, bool)) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_min_values (value Int64); \
                 CREATE TABLE values_to_min (id Int64, value Int64, included Bool);",
            )
            .expect("MIN(Int64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO values_to_min VALUES {rows}"))
                    .expect("MIN(Int64) differential rows");
            }
        }
        database
    }

    fn min_float64_database(
        row_count: usize,
        row_values: impl Fn(usize) -> (f64, bool),
    ) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_float_min_values (value Float64); \
                 CREATE TABLE float_values_to_min (id Int64, value Float64, included Bool);",
            )
            .expect("MIN(Float64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        let value = Value::Float64(value).as_display_string();
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO float_values_to_min VALUES {rows}"))
                    .expect("MIN(Float64) differential rows");
            }
        }
        database
    }

    fn max_float64_database(
        row_count: usize,
        row_values: impl Fn(usize) -> (f64, bool),
    ) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_float_max_values (value Float64); \
                 CREATE TABLE float_values_to_max (id Int64, value Float64, included Bool);",
            )
            .expect("MAX(Float64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        let value = Value::Float64(value).as_display_string();
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO float_values_to_max VALUES {rows}"))
                    .expect("MAX(Float64) differential rows");
            }
        }
        database
    }

    fn max_int64_database(row_count: usize, row_values: impl Fn(usize) -> (i64, bool)) -> Database {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE empty_max_values (value Int64); \
                 CREATE TABLE values_to_max (id Int64, value Int64, included Bool);",
            )
            .expect("MAX(Int64) differential setup");
        if row_count > 0 {
            for first_id in (1..=row_count).step_by(50_000) {
                let last_id = first_id.saturating_add(49_999).min(row_count);
                let rows = (first_id..=last_id)
                    .map(|id| {
                        let (value, included) = row_values(id);
                        format!("({id}, {value}, {included})")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                database
                    .execute(&format!("INSERT INTO values_to_max VALUES {rows}"))
                    .expect("MAX(Int64) differential rows");
            }
        }
        database
    }

    fn force_global_aggregate_workers(
        database: &mut Database,
        workers: usize,
        sql: &str,
    ) -> QueryResult {
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(workers);
        query(database, sql)
    }

    fn assert_global_aggregate_worker_differential(
        database: &mut Database,
        sql: &str,
    ) -> QueryResult {
        let single_worker = force_global_aggregate_workers(database, 1, sql);
        let multi_worker = force_global_aggregate_workers(database, 4, sql);
        assert_eq!(single_worker, multi_worker, "worker differential for {sql}");
        multi_worker
    }

    #[test]
    fn sole_nullable_int64_count_crosses_threshold_and_excludes_other_shapes() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(7); row_count];
        values[row_count - 1] = None;
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();

        let boundary_sql = "SELECT COUNT(value) AS present FROM nullable_values \
                            WHERE value IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![Value::Int64(
                i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()
            )]]
        );
        let above_threshold_sql = "SELECT COUNT(value) AS present FROM nullable_values";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![Value::Int64(i64::try_from(row_count - 1).unwrap())]]
        );

        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        BUDGET.reset_peak();
        query(&mut database, boundary_sql);
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "the threshold itself stays sequential"
        );

        BUDGET.reset_peak();
        query(&mut database, above_threshold_sql);
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "a sole nullable COUNT above the threshold uses shared helpers"
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(value), COUNT(*) FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "multi-aggregate nullable COUNT remains sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT value, COUNT(value) FROM nullable_values GROUP BY value"
            )
            .rows,
            [
                vec![Value::Null(DataType::Int64), Value::Int64(0)],
                vec![
                    Value::Int64(7),
                    Value::Int64(i64::try_from(row_count - 1).unwrap()),
                ],
            ]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "nullable-key grouped COUNT remains sequential"
        );
    }

    #[test]
    fn sole_nullable_int64_count_forced_workers_match_null_distributions_and_filters() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        assert_eq!(
            assert_global_aggregate_worker_differential(
                &mut empty,
                "SELECT COUNT(value) AS present FROM empty_values"
            )
            .rows,
            [vec![Value::Int64(0)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 5;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        assert_eq!(
            assert_global_aggregate_worker_differential(
                &mut all_null,
                "SELECT COUNT(value) AS present FROM all_null \
                 HAVING present = 0 ORDER BY present LIMIT 1 OFFSET 0"
            )
            .rows,
            [vec![Value::Int64(0)]]
        );

        let mut values = vec![Some(4); row_count];
        values[0] = Some(-1);
        values[row_count / 2] = None;
        values[row_count - 1] = Some(i64::MAX);
        let mut mixed = Database::new();
        mixed
            .create_nullable_int64_table("mixed", "value", values)
            .unwrap();
        let expected = i64::try_from(row_count - 2).unwrap();
        assert_eq!(
            assert_global_aggregate_worker_differential(
                &mut mixed,
                &format!(
                    "SELECT COUNT(value) AS present FROM mixed WHERE value >= 0 \
                     HAVING present = {expected} ORDER BY present DESC LIMIT 1"
                ),
            )
            .rows,
            [vec![Value::Int64(expected)]]
        );
    }

    #[test]
    fn sole_nullable_int64_count_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 3 != 0).then_some(11))
            .collect::<Vec<_>>();
        let expected = i64::try_from(values.iter().flatten().count()).unwrap();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = &format!(
            "SELECT COUNT(value) AS present FROM nullable_values \
             HAVING present = {expected} ORDER BY present LIMIT 1"
        );
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(exhausted, sequential);
        assert_eq!(exhausted.rows, [vec![Value::Int64(expected)]]);
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn sole_nullable_int64_count_worker_failure_repeats_complete_input_locally() {
        let values = [Some(1), None, Some(17)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_nullable_int64_count(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            nullable_int64_count_chunk,
        )
        .expect("deterministic parallel nullable COUNT succeeds");
        let failed_parallel = reduce_global_nullable_int64_count(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |values, rows| {
                if std::thread::current().name() == Some("rusthouse-count-nullable-int64-1") {
                    panic!("injected nullable COUNT worker failure");
                }
                nullable_int64_count_chunk(values, rows)
            },
        )
        .expect("worker failure falls back to a complete local nullable COUNT");

        assert_eq!(failed_parallel, successful_parallel);
        assert_eq!(failed_parallel, i64::try_from(row_count - 1).unwrap());
        assert_eq!(
            reduce_nullable_int64_count_partials(vec![i64::MAX, 1]),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
    }

    #[test]
    fn sole_nullable_int64_count_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let fixed_state_bytes = std::mem::size_of::<AggregateState>()
            .saturating_add(std::mem::size_of::<Vec<AggregateState>>());
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 1,
            max_groups: 1,
            max_aggregate_state_cells: 1,
            max_aggregate_state_bytes: fixed_state_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();
        let expected = i64::try_from(row_count.div_ceil(2)).unwrap();

        let sequential = force_global_aggregate_workers(
            &mut database,
            1,
            "SELECT COUNT(value) FROM nullable_values",
        );
        let parallel = force_global_aggregate_workers(
            &mut database,
            4,
            "SELECT COUNT(value) FROM nullable_values",
        );
        assert_eq!(parallel, sequential);
        assert_eq!(parallel.rows, [vec![Value::Int64(expected)]]);

        for (limits, expected_error) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..QueryResultLimits::default()
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_cells: 0,
                    ..QueryResultLimits::default()
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state cells",
                    actual: 1,
                    max: 0,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_state_bytes - 1,
                    ..QueryResultLimits::default()
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_state_bytes,
                    max: fixed_state_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_values: 0,
                    ..QueryResultLimits::default()
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result values",
                    actual: 1,
                    max: 0,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..QueryResultLimits::default()
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute("SELECT COUNT(value) FROM nullable_values");
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute("SELECT COUNT(value) FROM nullable_values");
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected_error));
        }
    }

    #[test]
    fn grouped_bool_nullable_count_threshold_filters_all_null_groups_and_admission_fallback() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut database = grouped_bool_nullable_count_database(row_count, |id| {
            let active = id % 2 == 1;
            let measurement = (!active && id % 6 != 0)
                .then(|| i64::try_from(id).expect("test row id fits Int64"));
            (active, measurement)
        });
        let present_through = |last_id: usize| {
            i64::try_from(
                (1..=last_id)
                    .filter(|id| id % 2 == 0 && id % 6 != 0)
                    .count(),
            )
            .unwrap()
        };

        let boundary_sql = format!(
            "SELECT active, COUNT(measurement) AS n FROM bool_nullable_events \
             WHERE id <= {} GROUP BY active HAVING n >= 0",
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
        );
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, &boundary_sql).rows,
            [
                vec![
                    Value::Bool(false),
                    Value::Int64(present_through(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD)),
                ],
                vec![Value::Bool(true), Value::Int64(0)],
            ],
            "an all-NULL group is retained with a zero count"
        );

        let above_threshold = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let above_threshold_sql = format!(
            "SELECT active, COUNT(measurement) AS n FROM bool_nullable_events \
             WHERE id <= {above_threshold} GROUP BY active HAVING n >= 0"
        );
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, &above_threshold_sql).rows,
            [
                vec![
                    Value::Bool(false),
                    Value::Int64(present_through(above_threshold)),
                ],
                vec![Value::Bool(true), Value::Int64(0)],
            ],
            "parallel reduction preserves the established grouped key tie-break"
        );

        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);
        BUDGET.reset_peak();
        query(&mut database, &boundary_sql);
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "the exact threshold stays sequential"
        );
        BUDGET.reset_peak();
        query(&mut database, &above_threshold_sql);
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "the sole supported shape above the threshold uses shared helpers"
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        let filtered_sql = "SELECT active AS enabled, COUNT(measurement) AS n \
                            FROM bool_nullable_events WHERE id > 1 GROUP BY active \
                            HAVING n >= 0 ORDER BY n DESC, enabled DESC";
        let expected_filtered = [
            vec![Value::Bool(false), Value::Int64(present_through(row_count))],
            vec![Value::Bool(true), Value::Int64(0)],
        ];
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, filtered_sql).rows,
            expected_filtered,
            "filtered NULL-ignoring counts retain normal HAVING and ordering"
        );

        let sequential = force_global_aggregate_workers(&mut database, 1, filtered_sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);
        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, filtered_sql);
        assert_eq!(exhausted, sequential);
        assert_eq!(exhausted.rows, expected_filtered);
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);

        for unsupported_sql in [
            "SELECT active, COUNT(active) FROM bool_nullable_events GROUP BY active",
            "SELECT active, COUNT(measurement), COUNT(*) \
             FROM bool_nullable_events GROUP BY active",
        ] {
            BUDGET.reset_peak();
            query(&mut database, unsupported_sql);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                0,
                "unsupported grouped shape stays sequential: {unsupported_sql}"
            );
        }
    }

    #[test]
    fn grouped_bool_nullable_count_worker_failure_repeats_complete_input_locally() {
        let group_values = [true, false, false];
        let count_values = [None, Some(17), None];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_grouped_bool_count(
            &group_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |group_values, rows| {
                grouped_bool_nullable_int64_count_chunk(group_values, &count_values, rows)
            },
        )
        .expect("deterministic parallel grouped nullable COUNT succeeds");
        let failed_parallel = reduce_grouped_bool_count(
            &group_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |group_values, rows| {
                if std::thread::current().name() == Some("rusthouse-group-bool-count-1") {
                    panic!("injected grouped nullable COUNT worker failure");
                }
                grouped_bool_nullable_int64_count_chunk(group_values, &count_values, rows)
            },
        )
        .expect("worker failure falls back to a complete local grouped nullable COUNT");

        let expected = GroupedBoolCountPartial {
            false_rows: 2,
            true_rows: i64::try_from(row_count - 2).unwrap(),
            false_count: 1,
            true_count: 0,
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn grouped_bool_nullable_count_forced_workers_preserve_resource_boundaries() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = grouped_bool_nullable_count_database(row_count, |id| {
            (id == 1, (id % 2 == 0).then_some(9))
        });
        let aggregate_state_bytes = 2_usize
            .saturating_mul(std::mem::size_of::<AggregateState>())
            .saturating_add(std::mem::size_of::<Vec<AggregateState>>());
        let group_key_bytes = 2_usize.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 2,
            max_values: 4,
            max_groups: 2,
            max_group_key_cells: 2,
            max_group_key_bytes: group_key_bytes,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: aggregate_state_bytes,
            ..QueryResultLimits::default()
        };
        database.query_result_limits = exact_limits;
        let sql = "SELECT active, COUNT(measurement) \
                   FROM bool_nullable_events GROUP BY active";

        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        let parallel = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [
                vec![
                    Value::Bool(false),
                    Value::Int64(i64::try_from(row_count / 2).unwrap()),
                ],
                vec![Value::Bool(true), Value::Int64(0)],
            ]
        );

        for (limits, expected_error) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_groups: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT groups",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_group_key_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT group key cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_group_key_bytes: group_key_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT group key bytes",
                    actual: group_key_bytes,
                    max: group_key_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: aggregate_state_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: aggregate_state_bytes,
                    max: aggregate_state_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_values: 3,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result values",
                    actual: 4,
                    max: 3,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute(sql);
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute(sql);
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected_error));
        }
    }

    #[test]
    fn grouped_bool_sum_threshold_filter_extrema_and_admission_are_differential() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 4;
        let mut database = grouped_bool_sum_database(row_count, |id| {
            let value = match id {
                1 | 4 => i64::MAX,
                2 | 3 => i64::MIN,
                _ => 0,
            };
            (value, id % 2 == 1, id <= row_count - 2)
        });

        let empty = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT active, SUM(value) FROM bool_sum_events \
             WHERE id < 0 GROUP BY active",
        );
        assert!(empty.rows.is_empty());

        let filtered_sql = "SELECT active AS enabled, SUM(value) AS total \
                            FROM bool_sum_events WHERE included = true GROUP BY active \
                            HAVING total = -1";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, filtered_sql).rows,
            [
                vec![Value::Bool(false), Value::Int64(-1)],
                vec![Value::Bool(true), Value::Int64(-1)],
            ],
            "partition reduction preserves normal grouped tie-breaking across Int64 extrema"
        );
        assert_eq!(
            assert_global_aggregate_worker_differential(
                &mut database,
                &format!("{filtered_sql} ORDER BY total DESC, enabled DESC LIMIT 1 OFFSET 1"),
            )
            .rows,
            [vec![Value::Bool(false), Value::Int64(-1)]],
            "HAVING, stable ordering, and pagination remain downstream of grouping"
        );

        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(NonZeroUsize::new(4).unwrap(), &BUDGET);
        BUDGET.reset_peak();
        query(
            &mut database,
            &format!(
                "SELECT active, SUM(value) FROM bool_sum_events WHERE id <= {} GROUP BY active",
                GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            ),
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "the exact matched-row threshold remains sequential"
        );

        BUDGET.reset_peak();
        query(
            &mut database,
            &format!(
                "SELECT active, SUM(value) FROM bool_sum_events WHERE id <= {} GROUP BY active",
                GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1
            ),
        );
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "the sole eligible shape above the threshold uses shared helpers"
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        let sequential = force_global_aggregate_workers(&mut database, 1, filtered_sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(NonZeroUsize::new(4).unwrap(), &BUDGET);
        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts grouped SUM helper admission");
        let exhausted = query(&mut database, filtered_sql);
        assert_eq!(exhausted, sequential);
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);

        database
            .execute("ALTER TABLE bool_sum_events ADD COLUMN nullable_value Nullable(Int64)")
            .expect("nullable grouped SUM exclusion setup");
        for unsupported_sql in [
            "SELECT active, SUM(value), COUNT(*) FROM bool_sum_events GROUP BY active",
            "SELECT active, included, SUM(value) FROM bool_sum_events GROUP BY active, included",
            "SELECT value, SUM(id) FROM bool_sum_events GROUP BY value",
            "SELECT active, SUM(nullable_value) FROM bool_sum_events GROUP BY active",
        ] {
            BUDGET.reset_peak();
            query(&mut database, unsupported_sql);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                0,
                "unsupported grouped shape stays sequential: {unsupported_sql}"
            );
        }
    }

    #[test]
    fn grouped_bool_sum_worker_failure_discards_partials_and_checks_reduction() {
        let group_values = [true, false, true];
        let sum_values = [1, -5, 9];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_grouped_bool_sum(
            &group_values,
            &sum_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            grouped_bool_sum_chunk,
        )
        .expect("deterministic parallel grouped SUM succeeds");
        let failed_parallel = reduce_grouped_bool_sum(
            &group_values,
            &sum_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |group_values, sum_values, rows| {
                if std::thread::current().name() == Some("rusthouse-group-bool-sum-int64-1") {
                    panic!("injected grouped SUM worker failure");
                }
                grouped_bool_sum_chunk(group_values, sum_values, rows)
            },
        )
        .expect("worker failure falls back to the complete grouped SUM locally");

        let expected = GroupedBoolSumPartial {
            false_sum: SumIntPartial { sum: -5, count: 1 },
            true_sum: SumIntPartial {
                sum: i128::try_from(row_count).unwrap() + 7,
                count: u64::try_from(row_count - 1).unwrap(),
            },
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);

        assert_eq!(
            reduce_grouped_bool_sum_partials(vec![
                GroupedBoolSumPartial {
                    false_sum: SumIntPartial {
                        sum: i128::MAX,
                        count: 0,
                    },
                    ..GroupedBoolSumPartial::default()
                },
                GroupedBoolSumPartial {
                    false_sum: SumIntPartial { sum: 1, count: 0 },
                    ..GroupedBoolSumPartial::default()
                },
            ]),
            Err(Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))
        );
        assert_eq!(
            reduce_grouped_bool_sum_partials(vec![
                GroupedBoolSumPartial {
                    true_sum: SumIntPartial {
                        sum: 0,
                        count: u64::MAX,
                    },
                    ..GroupedBoolSumPartial::default()
                },
                GroupedBoolSumPartial {
                    true_sum: SumIntPartial { sum: 0, count: 1 },
                    ..GroupedBoolSumPartial::default()
                },
            ]),
            Err(Error::NumericOverflow("SUM count".to_owned()))
        );
    }

    #[test]
    fn grouped_bool_sum_final_overflow_matches_the_sequential_error() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = grouped_bool_sum_database(row_count, |id| {
            let value = match id {
                1 => i64::MAX,
                2 => 1,
                _ => 0,
            };
            (value, true, true)
        });
        let sql = "SELECT active, SUM(value) FROM bool_sum_events GROUP BY active";

        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential = database.execute(sql);
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let parallel = database.execute(sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel,
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );
    }

    #[test]
    fn grouped_bool_sum_forced_workers_preserve_resource_boundaries() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = grouped_bool_sum_database(row_count, |id| (1, id == 1, true));
        let aggregate_state_bytes = 2_usize
            .saturating_mul(std::mem::size_of::<AggregateState>())
            .saturating_add(std::mem::size_of::<Vec<AggregateState>>());
        let group_key_bytes = 2_usize.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 2,
            max_values: 4,
            max_groups: 2,
            max_group_key_cells: 2,
            max_group_key_bytes: group_key_bytes,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: aggregate_state_bytes,
            ..QueryResultLimits::default()
        };
        database.query_result_limits = exact_limits;
        let sql = "SELECT active, SUM(value) FROM bool_sum_events GROUP BY active";

        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        let parallel = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [
                vec![
                    Value::Bool(false),
                    Value::Int64(i64::try_from(row_count - 1).unwrap()),
                ],
                vec![Value::Bool(true), Value::Int64(1)],
            ]
        );

        for (limits, expected_error) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_groups: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT groups",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_group_key_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT group key cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_group_key_bytes: group_key_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT group key bytes",
                    actual: group_key_bytes,
                    max: group_key_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: aggregate_state_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: aggregate_state_bytes,
                    max: aggregate_state_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_values: 3,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result values",
                    actual: 4,
                    max: 3,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute(sql);
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute(sql);
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected_error));
        }
    }

    #[test]
    fn grouped_bool_count_empty_one_and_two_group_worker_differentials_preserve_query_semantics() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut database = grouped_bool_count_database(row_count);

        let empty = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT active, COUNT(*) AS n FROM bool_events \
             WHERE id < 0 GROUP BY active",
        );
        assert!(empty.rows.is_empty());

        let one_group = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT active AS enabled, COUNT() AS n FROM bool_events \
             WHERE active = false GROUP BY active",
        );
        assert_eq!(
            one_group.rows,
            [vec![
                Value::Bool(false),
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
            ]]
        );

        let two_groups = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT active, COUNT(*) FROM bool_events GROUP BY active",
        );
        assert_eq!(
            two_groups.rows,
            [
                vec![
                    Value::Bool(false),
                    Value::Int64(i64::try_from(row_count - 1).unwrap()),
                ],
                vec![Value::Bool(true), Value::Int64(1)],
            ],
            "the parallel grouping retains the established sequential key tie-break"
        );

        let paged = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT active AS enabled, COUNT() AS n FROM bool_events \
             WHERE id > 0 GROUP BY active HAVING n >= 1 \
             ORDER BY n DESC, enabled ASC LIMIT 1 OFFSET 1",
        );
        assert_eq!(paged.rows, [vec![Value::Bool(true), Value::Int64(1)]]);
    }

    #[test]
    fn grouped_bool_count_parallel_threshold_budget_fallback_and_shape_are_bounded() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut database = grouped_bool_count_database(row_count);
        let worker_cap = NonZeroUsize::new(4).unwrap();
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(worker_cap, &BUDGET);

        BUDGET.reset_peak();
        let boundary = query(
            &mut database,
            &format!(
                "SELECT active, COUNT(*) FROM bool_events \
                 WHERE id <= {} GROUP BY active",
                GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            ),
        );
        assert_eq!(boundary.rows.len(), 2);
        assert_eq!(BUDGET.peak_helpers_in_use(), 0);

        BUDGET.reset_peak();
        let above_boundary = query(
            &mut database,
            &format!(
                "SELECT active, COUNT() FROM bool_events \
                 WHERE id <= {} GROUP BY active",
                GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1
            ),
        );
        assert_eq!(above_boundary.rows.len(), 2);
        assert!(BUDGET.peak_helpers_in_use() > 0);
        assert_eq!(BUDGET.helpers_in_use(), 0);

        BUDGET.reset_peak();
        let unsupported = query(
            &mut database,
            "SELECT active, COUNT(active) FROM bool_events GROUP BY active",
        );
        assert_eq!(unsupported.rows.len(), 2);
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) remains on the sequential grouped path"
        );

        BUDGET.reset_peak();
        let unsupported = query(
            &mut database,
            "SELECT active, COUNT(*), COUNT() FROM bool_events GROUP BY active",
        );
        assert_eq!(unsupported.rows.len(), 2);
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "multiple aggregates remain on the sequential grouped path"
        );

        BUDGET.reset_peak();
        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test saturates the helper budget");
        let exhausted = query(
            &mut database,
            "SELECT active, COUNT(*) FROM bool_events GROUP BY active",
        );
        assert_eq!(
            exhausted.rows[0],
            [
                Value::Bool(false),
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
            ]
        );
        assert_eq!(
            BUDGET.helpers_in_use(),
            BUDGET.helper_limit(),
            "the query falls back without exceeding saturated admission"
        );
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);

        database.query_result_limits.max_groups = 1;
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential_limit =
            database.execute("SELECT active, COUNT(*) FROM bool_events GROUP BY active");
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let parallel_limit =
            database.execute("SELECT active, COUNT(*) FROM bool_events GROUP BY active");
        assert_eq!(parallel_limit, sequential_limit);
        assert_eq!(
            parallel_limit,
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 2,
                max: 1,
            })
        );

        database.query_result_limits = QueryResultLimits {
            max_group_key_cells: 1,
            ..QueryResultLimits::default()
        };
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential_limit =
            database.execute("SELECT active, COUNT(*) FROM bool_events GROUP BY active");
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let parallel_limit =
            database.execute("SELECT active, COUNT(*) FROM bool_events GROUP BY active");
        assert_eq!(parallel_limit, sequential_limit);
        assert_eq!(
            parallel_limit,
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT group key cells",
                actual: 2,
                max: 1,
            })
        );
    }

    #[test]
    fn grouped_bool_count_worker_failure_repeats_complete_grouping_locally() {
        let values = [true, false];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![1; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 0;

        let successful_parallel = reduce_grouped_bool_count(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            grouped_bool_count_chunk,
        )
        .expect("deterministic parallel grouping succeeds");

        let partial = reduce_grouped_bool_count(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |values, rows| {
                if std::thread::current().name() == Some("rusthouse-group-bool-count-1") {
                    panic!("injected grouped Bool COUNT worker failure");
                }
                grouped_bool_count_chunk(values, rows)
            },
        )
        .expect("worker failure falls back to complete local grouping");

        assert_eq!(
            partial,
            GroupedBoolCountPartial {
                false_rows: i64::try_from(row_count - 1).unwrap(),
                true_rows: 1,
                false_count: i64::try_from(row_count - 1).unwrap(),
                true_count: 1,
                first_seen: Some(false),
            }
        );
        assert_eq!(successful_parallel, partial);
    }

    #[test]
    fn insert_batch_preflight_rejects_non_finite_ast_values_without_mutation() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (id Int64); CREATE TABLE samples (value Float64);")
            .expect("setup");
        let statements = vec![
            Statement::Insert {
                table: "events".to_owned(),
                rows: vec![vec![Value::Int64(1)]],
            },
            Statement::Insert {
                table: "samples".to_owned(),
                rows: vec![vec![Value::Float64(f64::INFINITY)]],
            },
        ];

        assert_eq!(
            database.execute_insert_statements(statements),
            Err(Error::InvalidQuery(
                "column 'samples.value' cannot store a non-finite Float64".to_owned()
            ))
        );
        assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
        assert_eq!(database.catalog().table("samples").unwrap().row_count(), 0);
    }

    #[test]
    fn aggregates_groups_and_orders() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE sales (region String, amount Int64); \
                 INSERT INTO sales VALUES ('west', 10), ('east', 4), ('west', 7);",
            )
            .expect("setup");

        let result = query(
            &mut database,
            "SELECT region, COUNT(*) AS n, SUM(amount) AS total, AVG(amount) AS mean \
             FROM sales GROUP BY region ORDER BY total DESC",
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::String("west".to_owned()),
                    Value::Int64(2),
                    Value::Int64(17),
                    Value::Float64(8.5),
                ],
                vec![
                    Value::String("east".to_owned()),
                    Value::Int64(1),
                    Value::Int64(4),
                    Value::Float64(4.0),
                ],
            ]
        );
    }

    #[test]
    fn global_count_if_forced_workers_match_for_empty_input() {
        let mut database = count_if_database(0);
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT countIf(active) FROM empty_events",
        );
        assert_eq!(result.rows, [vec![Value::Int64(0)]]);
    }

    #[test]
    fn global_count_if_forced_workers_match_after_filtering() {
        let row_count = COUNT_IF_PARALLEL_ROW_THRESHOLD
            .saturating_add(COUNT_IF_PARALLEL_ROW_THRESHOLD / 2)
            .saturating_add(3);
        let mut database = count_if_database(row_count);
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT countIf(active) FROM events WHERE included = true",
        );
        let expected = (1..=row_count)
            .filter(|id| id % 2 == 0 && id % 3 != 0)
            .count() as i64;
        assert_eq!(result.rows, [vec![Value::Int64(expected)]]);
    }

    #[test]
    fn global_count_if_forced_workers_match_at_parallel_boundary() {
        let row_count = COUNT_IF_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database(row_count);
        for matched_rows in [
            COUNT_IF_PARALLEL_ROW_THRESHOLD,
            COUNT_IF_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!("SELECT countIf(active) FROM events WHERE id <= {matched_rows}"),
            );
            assert_eq!(result.rows, [vec![Value::Int64((matched_rows / 2) as i64)]]);
        }
    }

    #[test]
    fn global_count_if_forced_workers_match_with_pagination() {
        let matched_rows = COUNT_IF_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database(matched_rows);
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT COUNT(*) AS rows, countIf(active) AS matches FROM events \
                 WHERE id <= {matched_rows} ORDER BY matches DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(
            first_page.rows,
            [vec![
                Value::Int64(matched_rows as i64),
                Value::Int64((matched_rows / 2) as i64),
            ]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT COUNT(*) AS rows, countIf(active) AS matches FROM events \
                 WHERE id <= {matched_rows} ORDER BY matches DESC LIMIT 1 OFFSET 1"
            ),
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_sum_int64_forced_workers_match_empty_null_and_pagination() {
        let mut database = sum_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT SUM(value) AS total FROM empty_values \
             HAVING total IS NULL ORDER BY total LIMIT 1 OFFSET 0",
        );
        assert_eq!(first_page.rows, [vec![Value::Null(DataType::Int64)]]);

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT SUM(value) AS total FROM empty_values \
             HAVING total IS NULL ORDER BY total LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_sum_int64_forced_workers_preserve_empty_results_and_pagination() {
        let mut database = sum_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM empty_values \
             HAVING total IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT SUM(value) AS total, COUNT() AS rows FROM empty_values \
             HAVING total IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_sum_int64_forced_workers_match_both_projection_orders() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            .saturating_add(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD / 2)
            .saturating_add(7);
        let value_for = |id: usize| i64::try_from(id % 97).unwrap() - 48;
        let mut database = sum_int64_database(row_count, |id| (value_for(id), id % 3 != 0));
        let expected_rows = (1..=row_count).filter(|id| id % 3 != 0).count();
        assert!(expected_rows > GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD);
        let expected_sum = (1..=row_count)
            .filter(|id| id % 3 != 0)
            .map(value_for)
            .sum::<i64>();

        let count_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT COUNT(*) AS matched, SUM(value) AS total FROM values_to_sum \
                 WHERE included = true HAVING total = {expected_sum} \
                 ORDER BY matched DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(count_first.columns[0].name, "matched");
        assert_eq!(count_first.columns[1].name, "total");
        assert_eq!(
            count_first.rows,
            [vec![
                Value::Int64(i64::try_from(expected_rows).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );

        let sum_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT SUM(value) AS total, COUNT() AS matched FROM values_to_sum \
                 WHERE included = true HAVING matched = {expected_rows} \
                 ORDER BY total DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(sum_first.columns[0].name, "total");
        assert_eq!(sum_first.columns[1].name, "matched");
        assert_eq!(
            sum_first.rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(expected_rows).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_sum_int64_forced_workers_match_after_filtering_and_having() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            .saturating_add(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD / 2)
            .saturating_add(3);
        let value_for = |id: usize| i64::try_from(id % 97).unwrap() - 48;
        let mut database = sum_int64_database(row_count, |id| (value_for(id), id % 3 != 0));
        let expected = (1..=row_count)
            .filter(|id| id % 3 != 0)
            .map(value_for)
            .sum::<i64>();
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT SUM(value) AS total FROM values_to_sum WHERE included = true \
                 HAVING total = {expected} ORDER BY total DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(result.rows, [vec![Value::Int64(expected)]]);
    }

    #[test]
    fn nullable_int64_sum_crosses_the_parallel_threshold_and_excludes_other_shapes() {
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(3); row_count];
        values[row_count - 1] = None;
        let expected_sum = i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD)
            .unwrap()
            .checked_mul(3)
            .unwrap();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();

        let boundary_sql = "SELECT COUNT(*) AS rows, SUM(value) AS total \
                            FROM nullable_values WHERE value IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        let above_threshold_sql =
            "SELECT SUM(value) AS total, COUNT() AS rows FROM nullable_values";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "matching rows at the threshold stay sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "a paired nullable SUM above the threshold uses shared helpers"
        );
        assert_eq!(OBSERVED_BUDGET.helpers_in_use(), 0);

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "COUNT(*) plus nullable SUM uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(value) AS total, COUNT() AS rows FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "nullable SUM plus COUNT() uses the same shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(value) AS present, SUM(value) AS total FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "same-column nullable COUNT reuses the SUM helper partitions"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT value, SUM(value) AS total FROM nullable_values GROUP BY value"
            )
            .rows,
            [
                vec![Value::Null(DataType::Int64), Value::Null(DataType::Int64)],
                vec![Value::Int64(3), Value::Int64(expected_sum)],
            ]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "grouped nullable SUM remains on the bounded sequential state path"
        );
    }

    #[test]
    fn sole_nullable_int64_sum_forced_workers_match_null_distributions_and_clauses() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT SUM(value) AS total FROM all_null HAVING total IS NULL \
             ORDER BY total LIMIT 1 OFFSET 0",
        );
        assert_eq!(null_result.rows, [vec![Value::Null(DataType::Int64)]]);

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT SUM(value) AS total FROM sparse WHERE value IS NULL OR value IS NOT NULL \
             HAVING total = 3 ORDER BY total DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(mixed_result.rows, [vec![Value::Int64(3)]]);
    }

    #[test]
    fn paired_nullable_int64_sum_forced_workers_match_null_distributions_and_overflow() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        let empty_result = assert_global_aggregate_worker_differential(
            &mut empty,
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM empty_values",
        );
        assert_eq!(
            empty_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM all_null \
             HAVING total IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            null_result.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Null(DataType::Int64),
            ]]
        );

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT SUM(value) AS total, COUNT() AS rows FROM sparse \
             WHERE value IS NULL OR value IS NOT NULL \
             HAVING total = 3 ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            mixed_result.rows,
            [vec![
                Value::Int64(3),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        let mut overflow_values = vec![None; row_count];
        overflow_values[0] = Some(i64::MAX);
        overflow_values[row_count - 1] = Some(1);
        let mut overflow = Database::new();
        overflow
            .create_nullable_int64_table("overflow_values", "value", overflow_values)
            .unwrap();
        overflow.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential =
            overflow.execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM overflow_values");
        overflow.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let parallel =
            overflow.execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM overflow_values");
        assert_eq!(parallel, sequential, "nullable pair overflow differential");
        assert_eq!(
            parallel,
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );
    }

    #[test]
    fn paired_nullable_int64_sum_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(9))
            .collect::<Vec<_>>();
        let expected_sum = values.iter().flatten().copied().sum::<i64>();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = &format!(
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values \
             HAVING total = {expected_sum} ORDER BY total LIMIT 1"
        );
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete computation locally"
        );
        assert_eq!(
            exhausted.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn paired_nullable_int64_sum_worker_failure_repeats_the_complete_input_locally() {
        let values = [Some(1), None, Some(17)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Sum,
            nullable_int64_chunk,
        )
        .expect("deterministic parallel nullable SUM succeeds");
        let partial = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Sum,
            |values, rows, function| {
                if std::thread::current().name() == Some("rusthouse-sum-int64-1") {
                    panic!("injected nullable SUM worker failure");
                }
                nullable_int64_chunk(values, rows, function)
            },
        )
        .expect("worker failure falls back to a complete local nullable SUM");

        assert_eq!(partial, successful_parallel);
        assert_eq!(partial.count, u64::try_from(row_count - 1).unwrap());
        assert_eq!(
            partial.sum,
            i128::try_from(row_count).unwrap().saturating_sub(2) + 17
        );
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn paired_nullable_int64_sum_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let fixed_bytes = 2_usize.saturating_mul(
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>(),
        );
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 2,
            max_groups: 1,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();
        let expected_sum = i64::try_from(row_count.div_ceil(2))
            .unwrap()
            .checked_mul(5)
            .unwrap();

        let sequential = force_global_aggregate_workers(
            &mut database,
            1,
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values",
        );
        let parallel = force_global_aggregate_workers(
            &mut database,
            4,
            "SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values",
        );
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database
                .execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values");
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database
                .execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM nullable_values");
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn same_column_nullable_count_sum_crosses_threshold_and_excludes_other_counts() {
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(3); row_count];
        values[row_count - 1] = None;
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        database
            .execute("ALTER TABLE nullable_values ADD COLUMN other Int64")
            .unwrap();

        let present_count = row_count - 1;
        let expected_sum = i64::try_from(present_count)
            .unwrap()
            .checked_mul(3)
            .unwrap();
        let boundary_sql = "SELECT COUNT(value), SUM(value) FROM nullable_values \
                            WHERE value IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(present_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        let above_threshold_sql = "SELECT SUM(value), COUNT(value) FROM nullable_values";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(present_count).unwrap()),
            ]]
        );

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );

        OBSERVED_BUDGET.reset_peak();
        query(&mut database, boundary_sql);
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "matching rows at the threshold stay sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        query(&mut database, above_threshold_sql);
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "same-column nullable COUNT and SUM above the threshold share SUM helpers"
        );
        assert_eq!(OBSERVED_BUDGET.helpers_in_use(), 0);

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(other), SUM(value) FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "different-column COUNT plus nullable SUM remains sequential"
        );
    }

    #[test]
    fn same_column_nullable_count_sum_forced_workers_match_null_distributions_and_overflow() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        let empty_result = assert_global_aggregate_worker_differential(
            &mut empty,
            "SELECT COUNT(value) AS present, SUM(value) AS total FROM empty_values",
        );
        assert_eq!(
            empty_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT SUM(value) AS total, COUNT(value) AS present FROM all_null \
             HAVING total IS NULL ORDER BY present DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            null_result.rows,
            [vec![Value::Null(DataType::Int64), Value::Int64(0)]]
        );

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT COUNT(value) AS present, SUM(value) AS total FROM sparse \
             WHERE value IS NULL OR value IS NOT NULL \
             HAVING total = 3 ORDER BY present DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(mixed_result.rows, [vec![Value::Int64(3), Value::Int64(3)]]);

        let mut overflow_values = vec![None; row_count];
        overflow_values[0] = Some(i64::MAX);
        overflow_values[row_count - 1] = Some(1);
        let mut overflow = Database::new();
        overflow
            .create_nullable_int64_table("overflow_values", "value", overflow_values)
            .unwrap();
        overflow.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential = overflow
            .execute("SELECT COUNT(value) AS present, SUM(value) AS total FROM overflow_values");
        overflow.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let parallel = overflow
            .execute("SELECT COUNT(value) AS present, SUM(value) AS total FROM overflow_values");
        assert_eq!(
            parallel, sequential,
            "same-column nullable pair overflow differential"
        );
        assert_eq!(
            parallel,
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );
    }

    #[test]
    fn same_column_nullable_count_sum_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(9))
            .collect::<Vec<_>>();
        let present_count = values.iter().flatten().count();
        let expected_sum = i64::try_from(present_count)
            .unwrap()
            .checked_mul(9)
            .unwrap();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = &format!(
            "SELECT COUNT(value) AS present, SUM(value) AS total FROM nullable_values \
             HAVING total = {expected_sum} ORDER BY present DESC LIMIT 1"
        );
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete paired computation locally"
        );
        assert_eq!(
            exhausted.rows,
            [vec![
                Value::Int64(i64::try_from(present_count).unwrap()),
                Value::Int64(expected_sum),
            ]]
        );
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn nullable_int64_sum_worker_failure_preserves_same_column_count_partial() {
        let values = [Some(1), None, Some(17)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Sum,
            nullable_int64_chunk,
        )
        .expect("deterministic parallel nullable SUM succeeds");
        let partial = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Sum,
            |values, rows, function| {
                if std::thread::current().name() == Some("rusthouse-sum-int64-1") {
                    panic!("injected nullable SUM worker failure");
                }
                nullable_int64_chunk(values, rows, function)
            },
        )
        .expect("worker failure falls back to a complete local nullable SUM");

        assert_eq!(partial, successful_parallel);
        assert_eq!(partial.count, u64::try_from(row_count - 1).unwrap());
        assert_eq!(
            partial.sum,
            i128::try_from(row_count).unwrap().saturating_sub(2) + 17
        );
        assert_eq!(
            count_present_values(partial.count),
            Ok(i64::try_from(row_count - 1).unwrap())
        );
    }

    #[test]
    fn same_column_nullable_count_sum_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let present_count = row_count.div_ceil(2);
        let expected_sum = i64::try_from(present_count)
            .unwrap()
            .checked_mul(5)
            .unwrap();
        let fixed_bytes = 2_usize.saturating_mul(
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>(),
        );
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 2,
            max_groups: 1,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();
        let sql = "SELECT SUM(value) AS total, COUNT(value) AS present FROM nullable_values";

        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        let parallel = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [vec![
                Value::Int64(expected_sum),
                Value::Int64(i64::try_from(present_count).unwrap()),
            ]]
        );

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute(sql);
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute(sql);
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn nullable_int64_avg_crosses_the_parallel_threshold_and_excludes_other_shapes() {
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(3); row_count];
        values[row_count - 1] = None;
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        database
            .execute("ALTER TABLE nullable_values ADD COLUMN other Int64")
            .unwrap();

        let boundary_sql = "SELECT COUNT(value), AVG(value) FROM nullable_values \
                            WHERE value IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        let above_threshold_sql = "SELECT AVG(value), COUNT(value) FROM nullable_values";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Float64(3.0),
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
            ]]
        );

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "matching rows at the threshold stay sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Float64(3.0),
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "same-column nullable AVG and COUNT above the threshold use shared helpers"
        );
        assert_eq!(OBSERVED_BUDGET.helpers_in_use(), 0);

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows, AVG(value) AS mean FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "COUNT(*) plus nullable AVG uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT AVG(value) AS mean, COUNT() AS rows FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Float64(3.0),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "nullable AVG plus COUNT() uses the same shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "COUNT(nullable column) plus AVG of the same column reuses shared partials"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(other) AS rows, AVG(value) AS mean FROM nullable_values"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "different-column COUNT plus nullable AVG remains sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT value, AVG(value) AS mean FROM nullable_values GROUP BY value"
            )
            .rows,
            [
                vec![Value::Null(DataType::Int64), Value::Null(DataType::Float64)],
                vec![Value::Int64(3), Value::Float64(3.0)],
            ]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "grouped nullable AVG remains on the bounded sequential state path"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT other, COUNT(value), AVG(value) FROM nullable_values GROUP BY other"
            )
            .rows,
            [vec![
                Value::Int64(0),
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Float64(3.0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the same nullable aggregate pair remains sequential when grouped"
        );
    }

    #[test]
    fn sole_nullable_int64_avg_forced_workers_match_null_distributions_and_clauses() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT AVG(value) AS mean FROM all_null HAVING mean IS NULL \
             ORDER BY mean LIMIT 1 OFFSET 0",
        );
        assert_eq!(null_result.rows, [vec![Value::Null(DataType::Float64)]]);

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT AVG(value) AS mean FROM sparse WHERE value IS NULL OR value IS NOT NULL \
             HAVING mean = 1 ORDER BY mean DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(mixed_result.rows, [vec![Value::Float64(1.0)]]);
    }

    #[test]
    fn same_column_nullable_count_avg_forced_workers_match_null_distributions() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        let empty_result = assert_global_aggregate_worker_differential(
            &mut empty,
            "SELECT COUNT(value) AS present, AVG(value) AS mean FROM empty_values",
        );
        assert_eq!(
            empty_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Float64)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT COUNT(value) AS present, AVG(value) AS mean FROM all_null \
             HAVING mean IS NULL ORDER BY present DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            null_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Float64),]]
        );

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT AVG(value) AS mean, COUNT(value) AS present FROM sparse \
             WHERE value IS NULL OR value IS NOT NULL \
             HAVING mean = 1 ORDER BY present DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            mixed_result.rows,
            [vec![Value::Float64(1.0), Value::Int64(3),]]
        );
    }

    #[test]
    fn sole_nullable_int64_avg_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(9))
            .collect::<Vec<_>>();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = "SELECT AVG(value) AS mean FROM nullable_values \
                   HAVING mean = 9 ORDER BY mean LIMIT 1";
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete computation locally"
        );
        assert_eq!(exhausted.rows, [vec![Value::Float64(9.0)]]);
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn same_column_nullable_count_avg_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(9))
            .collect::<Vec<_>>();
        let present_count = values.iter().flatten().count();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = "SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values \
                   HAVING mean = 9 ORDER BY present DESC LIMIT 1";
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete paired computation locally"
        );
        assert_eq!(
            exhausted.rows,
            [vec![
                Value::Int64(i64::try_from(present_count).unwrap()),
                Value::Float64(9.0),
            ]]
        );
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn nullable_int64_avg_worker_failure_preserves_same_column_count_partial() {
        let values = [Some(1), None, Some(17)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Avg,
            nullable_int64_chunk,
        )
        .expect("deterministic parallel nullable AVG succeeds");
        let partial = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Avg,
            |values, rows, function| {
                if std::thread::current().name() == Some("rusthouse-avg-int64-1") {
                    panic!("injected nullable AVG worker failure");
                }
                nullable_int64_chunk(values, rows, function)
            },
        )
        .expect("worker failure falls back to a complete local nullable AVG");

        assert_eq!(partial, successful_parallel);
        assert_eq!(partial.count, u64::try_from(row_count - 1).unwrap());
        assert_eq!(
            partial.sum,
            i128::try_from(row_count).unwrap().saturating_sub(2) + 17
        );
        assert_eq!(
            count_present_values(partial.count),
            Ok(i64::try_from(row_count - 1).unwrap())
        );
    }

    #[test]
    fn sole_nullable_int64_avg_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let fixed_bytes =
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>();
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 1,
            max_groups: 1,
            max_aggregate_state_cells: 1,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();

        let sequential = force_global_aggregate_workers(
            &mut database,
            1,
            "SELECT AVG(value) FROM nullable_values",
        );
        let parallel = force_global_aggregate_workers(
            &mut database,
            4,
            "SELECT AVG(value) FROM nullable_values",
        );
        assert_eq!(parallel, sequential);
        assert_eq!(parallel.rows, [vec![Value::Float64(5.0)]]);

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute("SELECT AVG(value) FROM nullable_values");
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute("SELECT AVG(value) FROM nullable_values");
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn same_column_nullable_count_avg_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let present_count = row_count.div_ceil(2);
        let fixed_bytes = 2_usize.saturating_mul(
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>(),
        );
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 2,
            max_groups: 1,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();

        let sequential = force_global_aggregate_workers(
            &mut database,
            1,
            "SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values",
        );
        let parallel = force_global_aggregate_workers(
            &mut database,
            4,
            "SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values",
        );
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [vec![
                Value::Int64(i64::try_from(present_count).unwrap()),
                Value::Float64(5.0),
            ]]
        );

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database
                .execute("SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values");
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database
                .execute("SELECT COUNT(value) AS present, AVG(value) AS mean FROM nullable_values");
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn global_count_sum_int64_parallel_boundary_uses_shared_budget_with_fallback() {
        static UNAVAILABLE_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(0);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = sum_int64_database(row_count, |_| (1, true));
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, SUM(value) AS total FROM values_to_sum \
                     WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                ]]
            );
        }

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &UNAVAILABLE_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(value) AS total, COUNT() AS rows FROM values_to_sum"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "an unavailable helper budget falls back to the query thread"
        );

        OBSERVED_BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, SUM(value) AS total FROM values_to_sum \
                     WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                ),
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the paired shape stays sequential at the threshold"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(value), COUNT(*) FROM values_to_sum"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT(*) and SUM(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(), SUM(value) FROM values_to_sum"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT() and SUM(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(value), COUNT(id) FROM values_to_sum"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus SUM(Int64) remains on the sequential fallback"
        );
    }

    #[test]
    fn global_sum_int64_forced_workers_match_at_limit_and_overflow() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut database = sum_int64_database(row_count, |id| {
            let value = if id == 1 {
                i64::MAX
            } else if id == row_count {
                1
            } else {
                0
            };
            (value, id != row_count)
        });
        let boundary = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT SUM(value) FROM values_to_sum WHERE included = true",
        );
        assert_eq!(boundary.rows, [vec![Value::Int64(i64::MAX)]]);

        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let single_worker =
            database.execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM values_to_sum");
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
        let multi_worker =
            database.execute("SELECT COUNT(*) AS rows, SUM(value) AS total FROM values_to_sum");
        assert_eq!(single_worker, multi_worker, "overflow worker differential");
        assert_eq!(
            multi_worker,
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );
    }

    #[test]
    fn global_count_sum_int64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [1, 17];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        let partial = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Sum,
            |values, rows, function| {
                if std::thread::current().name() == Some("rusthouse-sum-int64-1") {
                    panic!("injected paired COUNT/SUM worker failure");
                }
                sum_int64_chunk(values, rows, function)
            },
        )
        .expect("worker failure falls back to a complete local SUM");

        assert_eq!(partial.count, u64::try_from(row_count).unwrap());
        assert_eq!(
            partial.sum,
            i128::try_from(row_count).unwrap().saturating_sub(1) + 17
        );
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_avg_int64_forced_workers_match_empty_null_having_and_pagination() {
        let mut database = avg_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT AVG(value) AS mean FROM empty_avg_values \
             HAVING mean IS NULL ORDER BY mean LIMIT 1 OFFSET 0",
        );
        assert_eq!(first_page.rows, [vec![Value::Null(DataType::Float64)]]);

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT AVG(value) AS mean FROM empty_avg_values \
             HAVING mean IS NULL ORDER BY mean LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_avg_int64_forced_workers_preserve_empty_results_and_pagination() {
        let mut database = avg_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, AVG(value) AS mean FROM empty_avg_values \
             HAVING mean IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Float64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT AVG(value) AS mean, COUNT() AS rows FROM empty_avg_values \
             HAVING mean IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_avg_int64_forced_workers_match_filtered_extrema_and_having() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut database = avg_int64_database(row_count, |id| match id {
            1 => (i64::MIN, true),
            2 => (i64::MAX, false),
            id if id == row_count - 1 => (i64::MIN, false),
            id if id == row_count => (i64::MAX, true),
            _ => (0, true),
        });
        let matched_rows = row_count - 2;
        let expected = -1.0 / matched_rows as f64;
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT AVG(value) AS mean FROM values_to_avg WHERE included = true \
             HAVING mean < 0 ORDER BY mean DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(result.rows, [vec![Value::Float64(expected)]]);
    }

    #[test]
    fn global_count_avg_int64_forced_workers_match_filtered_projection_orders_and_aliases() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut database = avg_int64_database(row_count, |id| match id {
            1 => (i64::MIN, true),
            2 => (i64::MAX, false),
            id if id == row_count - 1 => (i64::MIN, false),
            id if id == row_count => (i64::MAX, true),
            _ => (0, true),
        });
        let matched_rows = row_count - 2;
        let expected_mean = -1.0 / matched_rows as f64;

        let count_first = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS matched, AVG(value) AS mean FROM values_to_avg \
             WHERE included = true HAVING mean < 0 \
             ORDER BY matched DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(count_first.columns[0].name, "matched");
        assert_eq!(count_first.columns[1].name, "mean");
        assert_eq!(
            count_first.rows,
            [vec![
                Value::Int64(i64::try_from(matched_rows).unwrap()),
                Value::Float64(expected_mean),
            ]]
        );

        let avg_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT AVG(value) AS mean, COUNT() AS matched FROM values_to_avg \
                 WHERE included = true HAVING matched = {matched_rows} \
                 ORDER BY mean DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(avg_first.columns[0].name, "mean");
        assert_eq!(avg_first.columns[1].name, "matched");
        assert_eq!(
            avg_first.rows,
            [vec![
                Value::Float64(expected_mean),
                Value::Int64(i64::try_from(matched_rows).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_count_avg_int64_parallel_boundary_uses_shared_budget_with_sequential_fallback() {
        static UNAVAILABLE_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(0);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = avg_int64_database(row_count, |_| (7, true));
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, AVG(value) AS mean FROM values_to_avg \
                     WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Float64(7.0),
                ]]
            );
        }

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &UNAVAILABLE_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT AVG(value) AS mean, COUNT() AS rows FROM values_to_avg"
            )
            .rows,
            [vec![
                Value::Float64(7.0),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "an unavailable helper budget falls back to the query thread"
        );

        OBSERVED_BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        assert_eq!(
            query(&mut database, "SELECT AVG(value) FROM values_to_avg").rows,
            [vec![Value::Float64(7.0)]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "sole AVG(Int64) uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, AVG(value) AS mean FROM values_to_avg \
                     WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                ),
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
                Value::Float64(7.0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the paired shape stays sequential at the threshold"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT AVG(value), COUNT(*) FROM values_to_avg"
            )
            .rows,
            [vec![
                Value::Float64(7.0),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT(*) and AVG(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(id), AVG(value) FROM values_to_avg"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Float64(7.0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus AVG(Int64) remains on the sequential fallback"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT included, AVG(value) FROM values_to_avg GROUP BY included"
            )
            .rows,
            [vec![Value::Bool(true), Value::Float64(7.0)]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "grouped AVG(Int64) stays sequential"
        );
    }

    #[test]
    fn global_count_avg_int64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [1, 17];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        let partial = reduce_global_int64_sum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            AggregateFunction::Avg,
            |values, rows, function| {
                if std::thread::current().name() == Some("rusthouse-avg-int64-1") {
                    panic!("injected paired COUNT/AVG worker failure");
                }
                sum_int64_chunk(values, rows, function)
            },
        )
        .expect("worker failure falls back to a complete local AVG");

        assert_eq!(partial.count, u64::try_from(row_count).unwrap());
        assert_eq!(
            partial.sum,
            i128::try_from(row_count).unwrap().saturating_sub(1) + 17
        );
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_min_int64_forced_workers_match_empty_null_having_and_pagination() {
        let mut database = min_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MIN(value) AS minimum FROM empty_min_values \
             HAVING minimum IS NULL ORDER BY minimum LIMIT 1 OFFSET 0",
        );
        assert_eq!(first_page.rows, [vec![Value::Null(DataType::Int64)]]);

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MIN(value) AS minimum FROM empty_min_values \
             HAVING minimum IS NULL ORDER BY minimum LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_min_int64_forced_workers_preserve_empty_results_and_pagination() {
        let mut database = min_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM empty_min_values \
             HAVING minimum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MIN(value) AS minimum, COUNT() AS rows FROM empty_min_values \
             HAVING minimum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_min_int64_forced_workers_match_filtered_projection_orders_and_aliases() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            .saturating_add(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD / 2)
            .saturating_add(5);
        let value_for = |id: usize| i64::try_from(id % 1_003).unwrap() - 501;
        let mut database = min_int64_database(row_count, |id| (value_for(id), id % 3 != 0));
        let matched_rows = (1..=row_count).filter(|id| id % 3 != 0).count();
        let expected_minimum = (1..=row_count)
            .filter(|id| id % 3 != 0)
            .map(value_for)
            .min()
            .unwrap();

        let count_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT COUNT(*) AS matched, MIN(value) AS minimum FROM values_to_min \
                 WHERE included = true HAVING minimum = {expected_minimum} \
                 ORDER BY matched DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(count_first.columns[0].name, "matched");
        assert_eq!(count_first.columns[1].name, "minimum");
        assert_eq!(
            count_first.rows,
            [vec![
                Value::Int64(i64::try_from(matched_rows).unwrap()),
                Value::Int64(expected_minimum),
            ]]
        );

        let minimum_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT MIN(value) AS minimum, COUNT() AS matched FROM values_to_min \
                 WHERE included = true HAVING matched = {matched_rows} \
                 ORDER BY minimum LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(minimum_first.columns[0].name, "minimum");
        assert_eq!(minimum_first.columns[1].name, "matched");
        assert_eq!(
            minimum_first.rows,
            [vec![
                Value::Int64(expected_minimum),
                Value::Int64(i64::try_from(matched_rows).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_count_min_int64_parallel_boundary_uses_shared_budget_with_sequential_fallback() {
        static UNAVAILABLE_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(0);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = min_int64_database(row_count, |id| {
            (i64::try_from(row_count - id).unwrap(), true)
        });
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM values_to_min \
                     WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Int64(i64::try_from(row_count - matched_rows).unwrap()),
                ]]
            );
        }

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &UNAVAILABLE_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT() FROM values_to_min"
            )
            .rows,
            [vec![
                Value::Int64(0),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "an unavailable helper budget falls back to the query thread"
        );

        OBSERVED_BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        assert_eq!(
            query(&mut database, "SELECT MIN(value) FROM values_to_min").rows,
            [vec![Value::Int64(0)]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "sole MIN(Int64) uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT MIN(value), COUNT(*) FROM values_to_min WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                )
            )
            .rows,
            [vec![
                Value::Int64(1),
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the paired shape stays sequential at the threshold"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT(*) FROM values_to_min"
            )
            .rows,
            [vec![
                Value::Int64(0),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT(*) and MIN(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(id), MIN(value) FROM values_to_min"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(0),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus MIN(Int64) remains on the sequential fallback"
        );
    }

    #[test]
    fn global_min_int64_forced_workers_match_extreme_values() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut database = min_int64_database(row_count, |id| {
            let value = if id == 1 {
                i64::MAX
            } else if id == row_count {
                i64::MIN
            } else {
                0
            };
            (value, true)
        });
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MIN(value) FROM values_to_min WHERE included = true",
        );
        assert_eq!(result.rows, [vec![Value::Int64(i64::MIN)]]);
    }

    #[test]
    fn global_count_min_int64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [9, 7, 5, 3, 1, -1, -3, -5, i64::MIN, i64::MAX];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 8;
        let minimum = reduce_global_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "min",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-min-int64-1") {
                    panic!("injected MIN worker failure");
                }
                left.min(right)
            },
        );

        assert_eq!(minimum, Some(i64::MIN));
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_count_min_float64_forced_workers_match_empty_null_having_and_pagination() {
        let mut database = min_float64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM empty_float_min_values \
             HAVING minimum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Float64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MIN(value) AS minimum, COUNT() AS rows FROM empty_float_min_values \
             HAVING minimum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_min_float64_forced_workers_preserve_projection_order_aliases_and_signed_zero() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut database = min_float64_database(row_count, |id| match id {
            1 => (f64::MAX, true),
            2 => (-0.0, true),
            id if id == row_count / 2 => (f64::MIN, false),
            id if id == row_count => (0.0, true),
            _ => ((id % 1_003 + 1) as f64, true),
        });
        let matched_rows = row_count - 1;
        let sql = "SELECT COUNT(*) AS matched, MIN(value) AS minimum FROM float_values_to_min \
                   WHERE included = true HAVING minimum = 0.0 \
                   ORDER BY matched DESC LIMIT 1 OFFSET 0";

        let single_worker = force_global_aggregate_workers(&mut database, 1, sql);
        let multi_worker = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(
            single_worker, multi_worker,
            "signed-zero worker differential"
        );
        assert_eq!(single_worker.columns[0].name, "matched");
        assert_eq!(single_worker.columns[1].name, "minimum");
        assert_eq!(
            single_worker.rows[0][0],
            Value::Int64(i64::try_from(matched_rows).unwrap())
        );
        let Value::Float64(single_minimum) = &single_worker.rows[0][1] else {
            panic!("single-worker minimum must be Float64")
        };
        let Value::Float64(multi_minimum) = &multi_worker.rows[0][1] else {
            panic!("multi-worker minimum must be Float64")
        };
        assert_eq!(single_minimum.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(multi_minimum.to_bits(), single_minimum.to_bits());

        let minimum_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT MIN(value) AS minimum, COUNT() AS matched FROM float_values_to_min \
                 HAVING minimum = {} ORDER BY minimum LIMIT 1",
                Value::Float64(f64::MIN).as_display_string()
            ),
        );
        assert_eq!(minimum_first.columns[0].name, "minimum");
        assert_eq!(minimum_first.columns[1].name, "matched");
        assert_eq!(
            minimum_first.rows,
            [vec![
                Value::Float64(f64::MIN),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_count_min_float64_parallel_boundary_and_admission_exhaustion_fall_back_sequentially()
    {
        static SATURATED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = min_float64_database(row_count, |_| (7.25, true));
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, MIN(value) AS minimum \
                     FROM float_values_to_min WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Float64(7.25),
                ]]
            );
        }

        SATURATED_BUDGET.reset_peak();
        let occupied_helpers = SATURATED_BUDGET
            .acquire_for_test(3)
            .expect("test reserves every helper");
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &SATURATED_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT() FROM float_values_to_min"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "exhausted admission falls back to the complete pair on the query thread"
        );
        drop(occupied_helpers);

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT MIN(value), COUNT(*) FROM float_values_to_min \
                     WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                ),
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the paired shape stays sequential at the threshold"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT(*) FROM float_values_to_min"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "paired COUNT(*) and MIN(Float64) above the threshold uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT(id) FROM float_values_to_min"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus MIN(Float64) stays sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(value), COUNT(*), MAX(value) FROM float_values_to_min"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Float64(7.25),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "other multi-aggregate Float64 projections stay sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT included, MIN(value) FROM float_values_to_min GROUP BY included"
            )
            .rows,
            [vec![Value::Bool(true), Value::Float64(7.25)]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "grouped MIN(Float64) stays sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(included) FROM float_values_to_min"
            )
            .rows,
            [vec![Value::Bool(true)]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "MAX(Bool) stays sequential"
        );
    }

    #[test]
    fn global_count_min_float64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [9.0, 7.0, 5.0, 3.0, 1.0, 0.0, -0.0, -5.0, f64::MIN];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 8;
        let minimum = reduce_global_float64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "min",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-min-float64-1") {
                    panic!("injected MIN(Float64) worker failure");
                }
                first_float64_minimum(left, right)
            },
        );

        assert_eq!(minimum, Some(f64::MIN));
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_count_max_float64_forced_workers_match_empty_null_having_and_pagination() {
        let mut database = max_float64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM empty_float_max_values \
             HAVING maximum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Float64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MAX(value) AS maximum, COUNT() AS rows FROM empty_float_max_values \
             HAVING maximum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_max_float64_forced_workers_preserve_orders_aliases_and_first_signed_zero() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut database = max_float64_database(row_count, |id| match id {
            1 => (f64::MIN, true),
            2 => (0.0, true),
            id if id == row_count / 2 => (f64::MAX, false),
            id if id == row_count => (-0.0, true),
            _ => (-((id % 1_003 + 1) as f64), true),
        });
        let matched_rows = row_count - 1;
        let sql = "SELECT COUNT(*) AS matched, MAX(value) AS maximum FROM float_values_to_max \
                   WHERE included = true HAVING maximum = 0.0 \
                   ORDER BY matched DESC LIMIT 1 OFFSET 0";

        let single_worker = force_global_aggregate_workers(&mut database, 1, sql);
        let multi_worker = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(
            single_worker, multi_worker,
            "signed-zero worker differential"
        );
        assert_eq!(single_worker.columns[0].name, "matched");
        assert_eq!(single_worker.columns[1].name, "maximum");
        assert_eq!(
            single_worker.rows[0][0],
            Value::Int64(i64::try_from(matched_rows).unwrap())
        );
        let Value::Float64(single_maximum) = &single_worker.rows[0][1] else {
            panic!("single-worker maximum must be Float64")
        };
        let Value::Float64(multi_maximum) = &multi_worker.rows[0][1] else {
            panic!("multi-worker maximum must be Float64")
        };
        assert_eq!(single_maximum.to_bits(), 0.0_f64.to_bits());
        assert_eq!(multi_maximum.to_bits(), single_maximum.to_bits());

        let finite_extreme = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT MAX(value) AS maximum, COUNT() AS rows FROM float_values_to_max \
                 HAVING rows = {row_count} ORDER BY maximum DESC LIMIT 1"
            ),
        );
        assert_eq!(finite_extreme.columns[0].name, "maximum");
        assert_eq!(finite_extreme.columns[1].name, "rows");
        assert_eq!(
            finite_extreme.rows,
            [vec![
                Value::Float64(f64::MAX),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_count_max_float64_parallel_boundary_and_admission_use_sequential_fallback() {
        static SATURATED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = max_float64_database(row_count, |_| (7.25, true));
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, MAX(value) AS maximum \
                     FROM float_values_to_max WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Float64(7.25),
                ]]
            );
        }

        SATURATED_BUDGET.reset_peak();
        let occupied_helpers = SATURATED_BUDGET
            .acquire_for_test(3)
            .expect("test reserves every helper");
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &SATURATED_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(value), COUNT() FROM float_values_to_max"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "exhausted admission falls back to the complete pair on the query thread"
        );
        drop(occupied_helpers);

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT MAX(value), COUNT(*) FROM float_values_to_max \
                     WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                ),
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the threshold itself stays sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, "SELECT MAX(value) FROM float_values_to_max").rows,
            [vec![Value::Float64(7.25)]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "sole MAX(Float64) above the threshold uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(value), COUNT(*) FROM float_values_to_max"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "paired COUNT(*) and MAX(Float64) above the threshold uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(), MAX(value) FROM float_values_to_max"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Float64(7.25),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "paired COUNT() and MAX(Float64) above the threshold uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(value), COUNT(id) FROM float_values_to_max"
            )
            .rows,
            [vec![
                Value::Float64(7.25),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus MAX(Float64) stays sequential"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT included, MAX(value) FROM float_values_to_max GROUP BY included"
            )
            .rows,
            [vec![Value::Bool(true), Value::Float64(7.25)]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "grouped MAX(Float64) stays sequential"
        );
    }

    #[test]
    fn global_count_max_float64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [-9.0, -7.0, -5.0, -3.0, -1.0, -0.0, 0.0, 5.0, f64::MAX];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 8;
        let maximum = reduce_global_float64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "max",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-max-float64-1") {
                    panic!("injected MAX(Float64) worker failure");
                }
                first_float64_maximum(left, right)
            },
        );

        assert_eq!(maximum, Some(f64::MAX));
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_count_max_int64_forced_workers_preserve_empty_results_and_pagination() {
        let mut database = max_int64_database(0, |_| unreachable!("empty input"));
        let first_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM empty_max_values \
             HAVING maximum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            first_page.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let second_page = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MAX(value) AS maximum, COUNT() AS rows FROM empty_max_values \
             HAVING maximum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 1",
        );
        assert!(second_page.rows.is_empty());
    }

    #[test]
    fn global_count_max_int64_forced_workers_match_filtered_projection_orders_and_aliases() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
            .saturating_add(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD / 2)
            .saturating_add(5);
        let value_for = |id: usize| i64::try_from(id % 1_003).unwrap() - 501;
        let mut database = max_int64_database(row_count, |id| (value_for(id), id % 3 != 0));
        let matched_rows = (1..=row_count).filter(|id| id % 3 != 0).count();
        let expected_maximum = (1..=row_count)
            .filter(|id| id % 3 != 0)
            .map(value_for)
            .max()
            .unwrap();

        let count_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT COUNT(*) AS matched, MAX(value) AS maximum FROM values_to_max \
                 WHERE included = true HAVING maximum = {expected_maximum} \
                 ORDER BY matched DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(count_first.columns[0].name, "matched");
        assert_eq!(count_first.columns[1].name, "maximum");
        assert_eq!(
            count_first.rows,
            [vec![
                Value::Int64(i64::try_from(matched_rows).unwrap()),
                Value::Int64(expected_maximum),
            ]]
        );

        let maximum_first = assert_global_aggregate_worker_differential(
            &mut database,
            &format!(
                "SELECT MAX(value) AS maximum, COUNT() AS matched FROM values_to_max \
                 WHERE included = true HAVING matched = {matched_rows} \
                 ORDER BY maximum DESC LIMIT 1 OFFSET 0"
            ),
        );
        assert_eq!(maximum_first.columns[0].name, "maximum");
        assert_eq!(maximum_first.columns[1].name, "matched");
        assert_eq!(
            maximum_first.rows,
            [vec![
                Value::Int64(expected_maximum),
                Value::Int64(i64::try_from(matched_rows).unwrap()),
            ]]
        );
    }

    #[test]
    fn global_count_max_int64_parallel_boundary_uses_shared_budget_with_sequential_fallback() {
        static UNAVAILABLE_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(0);
        static OBSERVED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = max_int64_database(row_count, |id| (i64::try_from(id).unwrap(), true));
        for matched_rows in [
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD,
            GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1,
        ] {
            let result = assert_global_aggregate_worker_differential(
                &mut database,
                &format!(
                    "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM values_to_max \
                     WHERE id <= {matched_rows}"
                ),
            );
            assert_eq!(
                result.rows,
                [vec![
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                    Value::Int64(i64::try_from(matched_rows).unwrap()),
                ]]
            );
        }

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &UNAVAILABLE_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(value) AS maximum, COUNT() AS rows FROM values_to_max"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]],
            "an unavailable helper budget falls back to the query thread"
        );

        OBSERVED_BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(
            database.global_aggregate_worker_cap(),
            &OBSERVED_BUDGET,
        );
        assert_eq!(
            query(
                &mut database,
                &format!(
                    "SELECT MAX(value), COUNT(*) FROM values_to_max WHERE id <= {}",
                    GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD
                )
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
                Value::Int64(i64::try_from(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "the paired shape stays sequential at the threshold"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(value), COUNT(*) FROM values_to_max"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT(*) and MAX(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(), MAX(value) FROM values_to_max"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            OBSERVED_BUDGET.peak_helpers_in_use() > 0,
            "the paired COUNT() and MAX(Int64) shape uses the shared helper budget"
        );

        OBSERVED_BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(id), MAX(value) FROM values_to_max"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(
            OBSERVED_BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus MAX(Int64) remains on the sequential fallback"
        );
    }

    #[test]
    fn global_max_int64_forced_workers_match_extreme_values() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut database = max_int64_database(row_count, |id| {
            let value = if id == 1 {
                i64::MIN
            } else if id == row_count {
                i64::MAX
            } else {
                0
            };
            (value, true)
        });
        let result = assert_global_aggregate_worker_differential(
            &mut database,
            "SELECT MAX(value) FROM values_to_max WHERE included = true",
        );
        assert_eq!(result.rows, [vec![Value::Int64(i64::MAX)]]);
    }

    #[test]
    fn global_count_max_int64_worker_failure_repeats_the_complete_pair_locally() {
        let values = [-9, -7, -5, -3, -1, 1, 3, 5, i64::MAX, i64::MIN];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 8;
        let sequential_maximum = reduce_global_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(1),
            "max",
            i64::max,
        );
        let failed_parallel_maximum = reduce_global_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "max",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-max-int64-1") {
                    panic!("injected MAX worker failure");
                }
                left.max(right)
            },
        );

        assert_eq!(failed_parallel_maximum, sequential_maximum);
        assert_eq!(failed_parallel_maximum, Some(i64::MAX));
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn global_aggregate_reduction_overflows_are_checked() {
        assert_eq!(
            reduce_count_if_counts(vec![i64::MAX, 1]),
            Err(Error::NumericOverflow("countIf".to_owned()))
        );
        assert_eq!(
            reduce_sum_int64_partials(
                vec![
                    SumIntPartial {
                        sum: i128::MAX,
                        count: 1,
                    },
                    SumIntPartial { sum: 1, count: 1 },
                ],
                AggregateFunction::Sum,
            ),
            Err(Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))
        );
        assert_eq!(
            reduce_sum_int64_partials(
                vec![
                    SumIntPartial {
                        sum: i128::MAX,
                        count: 1,
                    },
                    SumIntPartial { sum: 1, count: 1 },
                ],
                AggregateFunction::Avg,
            ),
            Err(Error::NumericOverflow("AVG(Int64) sum".to_owned()))
        );
        assert_eq!(
            reduce_sum_int64_partials(
                vec![
                    SumIntPartial {
                        sum: 0,
                        count: u64::MAX,
                    },
                    SumIntPartial { sum: 0, count: 1 },
                ],
                AggregateFunction::Avg,
            ),
            Err(Error::NumericOverflow("AVG count".to_owned()))
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            count_matched_rows(usize::try_from(i64::MAX).unwrap() + 1),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
        assert_eq!(
            count_present_values(u64::try_from(i64::MAX).unwrap() + 1),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
        assert_eq!(reduce_scalar_extremum_partials(None, None, &i64::min), None);
        assert_eq!(
            reduce_scalar_extremum_partials(Some(4), None, &i64::min),
            Some(4)
        );
        assert_eq!(
            reduce_scalar_extremum_partials(None, Some(-7), &i64::min),
            Some(-7)
        );
        assert_eq!(
            reduce_scalar_extremum_partials(Some(4), Some(-7), &i64::min),
            Some(-7)
        );
        assert_eq!(reduce_scalar_extremum_partials(None, None, &i64::max), None);
        assert_eq!(
            reduce_scalar_extremum_partials(Some(4), None, &i64::max),
            Some(4)
        );
        assert_eq!(
            reduce_scalar_extremum_partials(None, Some(-7), &i64::max),
            Some(-7)
        );
        assert_eq!(
            reduce_scalar_extremum_partials(Some(4), Some(-7), &i64::max),
            Some(4)
        );
    }

    #[test]
    fn nullable_int64_average_state_checks_sum_and_count_overflow() {
        let mut database = Database::new();
        database
            .create_nullable_int64_table("readings", "v", vec![Some(1), Some(0)])
            .unwrap();
        let table = database.catalog().table("readings").unwrap();
        let spec = AggregateSpec {
            function: AggregateFunction::Avg,
            argument: Some(0),
            input_type: Some(DataType::Int64),
        };
        let mut aggregate_state_bytes = 0;

        let mut sum_overflow = AggregateState::AvgInt {
            sum: i128::MAX,
            count: 0,
        };
        assert_eq!(
            sum_overflow.update(&spec, table, 0, &mut aggregate_state_bytes, usize::MAX),
            Err(Error::NumericOverflow("AVG(Int64) sum".to_owned()))
        );

        let mut count_overflow = AggregateState::AvgInt {
            sum: 0,
            count: u64::MAX,
        };
        assert_eq!(
            count_overflow.update(&spec, table, 1, &mut aggregate_state_bytes, usize::MAX),
            Err(Error::NumericOverflow("AVG count".to_owned()))
        );
    }

    #[test]
    fn configured_cap_one_is_a_deterministic_sequential_differential() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(8);

        let cap = NonZeroUsize::new(1).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database_with_worker_cap(row_count, cap);
        assert_eq!(database.global_aggregate_worker_cap(), cap);
        BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(cap, &BUDGET);
        let capped = query(&mut database, "SELECT countIf(active) FROM events");
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "a one-lane cap never admits a helper"
        );

        let sequential =
            force_global_aggregate_workers(&mut database, 1, "SELECT countIf(active) FROM events");
        assert_eq!(capped, sequential);
        assert_eq!(capped.rows, [vec![Value::Int64((row_count / 2) as i64)]]);
    }

    #[test]
    fn paired_nullable_int64_min_crosses_the_parallel_threshold_and_excludes_other_shapes() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let cap = NonZeroUsize::new(4).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(3); row_count];
        values[0] = Some(i64::MIN);
        values[row_count - 1] = None;
        let mut database = Database::with_global_aggregate_worker_cap(cap);
        database
            .create_nullable_int64_table("readings", "v", values)
            .unwrap();

        let boundary_sql =
            "SELECT COUNT(*) AS rows, MIN(v) AS minimum FROM readings WHERE v IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );
        let above_threshold_sql = "SELECT MIN(v) AS minimum, COUNT() AS rows FROM readings";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(i64::MIN),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(cap, &BUDGET);

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "matching rows at the threshold stay sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(i64::MIN),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "nullable MIN plus COUNT() above the threshold uses shared helpers"
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, "SELECT COUNT(*), MIN(v) FROM readings").rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "COUNT(*) plus nullable MIN uses the same shared helper budget"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, "SELECT COUNT(v), MIN(v) FROM readings").rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus nullable MIN remains sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT v, MIN(v) FROM readings GROUP BY v ORDER BY v"
            )
            .rows,
            [
                vec![Value::Null(DataType::Int64), Value::Null(DataType::Int64)],
                vec![Value::Int64(i64::MIN), Value::Int64(i64::MIN)],
                vec![Value::Int64(3), Value::Int64(3)],
            ]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "grouped nullable MIN remains sequential"
        );
    }

    #[test]
    fn paired_nullable_int64_min_forced_workers_match_null_distributions_and_clauses() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        let empty_result = assert_global_aggregate_worker_differential(
            &mut empty,
            "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM empty_values",
        );
        assert_eq!(
            empty_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT MIN(value) AS minimum, COUNT() AS rows FROM all_null \
             HAVING minimum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            null_result.rows,
            [vec![
                Value::Null(DataType::Int64),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MAX);
        values[row_count / 3] = Some(4);
        values[row_count - 1] = Some(i64::MIN);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM sparse \
             WHERE value IS NULL OR value IS NOT NULL \
             HAVING minimum = -9223372036854775808 \
             ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            mixed_result.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );
    }

    #[test]
    fn paired_nullable_int64_min_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(-9))
            .collect::<Vec<_>>();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM nullable_values \
                   HAVING minimum = -9 ORDER BY rows DESC LIMIT 1";
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete COUNT/nullable MIN pair locally"
        );
        assert_eq!(
            exhausted.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(-9),
            ]]
        );
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn paired_nullable_int64_min_worker_failure_repeats_the_complete_pair_locally() {
        let values = [Some(9), None, Some(i64::MIN)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_nullable_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "min",
            i64::min,
        );
        let minimum = reduce_global_nullable_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "min",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-min-nullable-int64-1") {
                    panic!("injected nullable MIN worker failure");
                }
                left.min(right)
            },
        );

        assert_eq!(successful_parallel, Some(i64::MIN));
        assert_eq!(minimum, successful_parallel);
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn paired_nullable_int64_min_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let fixed_bytes = 2_usize.saturating_mul(
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>(),
        );
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 2,
            max_groups: 1,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| {
                        if row == row_count - 1 {
                            Some(i64::MIN)
                        } else {
                            (row % 2 == 0).then_some(5)
                        }
                    })
                    .collect(),
            )
            .unwrap();
        let sql = "SELECT COUNT(*) AS rows, MIN(value) AS minimum FROM nullable_values";

        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        let parallel = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MIN),
            ]]
        );

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_values: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result values",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute(sql);
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute(sql);
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn paired_nullable_int64_max_crosses_the_parallel_threshold_and_excludes_other_shapes() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let cap = NonZeroUsize::new(4).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut values = vec![Some(-3); row_count];
        values[0] = Some(i64::MAX);
        values[row_count - 1] = None;
        let mut database = Database::with_global_aggregate_worker_cap(cap);
        database
            .create_nullable_int64_table("readings", "v", values)
            .unwrap();

        let boundary_sql =
            "SELECT COUNT(*) AS rows, MAX(v) AS maximum FROM readings WHERE v IS NOT NULL";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MAX),
            ]]
        );
        let above_threshold_sql = "SELECT MAX(v) AS maximum, COUNT() AS rows FROM readings";
        assert_eq!(
            assert_global_aggregate_worker_differential(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(i64::MAX),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(cap, &BUDGET);

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, boundary_sql).rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MAX),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "matching rows at the threshold stay sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, above_threshold_sql).rows,
            [vec![
                Value::Int64(i64::MAX),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "nullable MAX plus COUNT() above the threshold uses shared helpers"
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, "SELECT COUNT(*), MAX(v) FROM readings").rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MAX),
            ]]
        );
        assert!(
            BUDGET.peak_helpers_in_use() > 0,
            "COUNT(*) plus nullable MAX uses the same shared helper budget"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(&mut database, "SELECT COUNT(v), MAX(v) FROM readings").rows,
            [vec![
                Value::Int64(i64::try_from(row_count - 1).unwrap()),
                Value::Int64(i64::MAX),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "COUNT(column) plus nullable MAX remains sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*), MAX(v), MIN(v) FROM readings"
            )
            .rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MAX),
                Value::Int64(-3),
            ]]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "other multi-aggregate nullable projections remain sequential"
        );

        BUDGET.reset_peak();
        assert_eq!(
            query(
                &mut database,
                "SELECT v, MAX(v) FROM readings GROUP BY v ORDER BY v"
            )
            .rows,
            [
                vec![Value::Null(DataType::Int64), Value::Null(DataType::Int64)],
                vec![Value::Int64(-3), Value::Int64(-3)],
                vec![Value::Int64(i64::MAX), Value::Int64(i64::MAX)],
            ]
        );
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            0,
            "grouped nullable MAX remains sequential"
        );
    }

    #[test]
    fn paired_nullable_int64_max_forced_workers_match_null_distributions_and_clauses() {
        let mut empty = Database::new();
        empty
            .create_nullable_int64_table("empty_values", "value", Vec::new())
            .unwrap();
        let empty_result = assert_global_aggregate_worker_differential(
            &mut empty,
            "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM empty_values",
        );
        assert_eq!(
            empty_result.rows,
            [vec![Value::Int64(0), Value::Null(DataType::Int64)]]
        );

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 3;
        let mut all_null = Database::new();
        all_null
            .create_nullable_int64_table("all_null", "value", vec![None; row_count])
            .unwrap();
        let null_result = assert_global_aggregate_worker_differential(
            &mut all_null,
            "SELECT MAX(value) AS maximum, COUNT() AS rows FROM all_null \
             HAVING maximum IS NULL ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            null_result.rows,
            [vec![
                Value::Null(DataType::Int64),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );

        let mut values = vec![None; row_count];
        values[0] = Some(i64::MIN);
        values[row_count / 3] = Some(-4);
        values[row_count - 1] = Some(i64::MAX);
        let mut sparse = Database::new();
        sparse
            .create_nullable_int64_table("sparse", "value", values)
            .unwrap();
        let mixed_result = assert_global_aggregate_worker_differential(
            &mut sparse,
            "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM sparse \
             WHERE value IS NULL OR value IS NOT NULL \
             HAVING maximum = 9223372036854775807 \
             ORDER BY rows DESC LIMIT 1 OFFSET 0",
        );
        assert_eq!(
            mixed_result.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(i64::MAX),
            ]]
        );
    }

    #[test]
    fn paired_nullable_int64_max_exhausted_admission_falls_back_completely() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let values = (0..row_count)
            .map(|row| (row % 5 != 0).then_some(9))
            .collect::<Vec<_>>();
        let mut database = Database::new();
        database
            .create_nullable_int64_table("nullable_values", "value", values)
            .unwrap();
        let sql = "SELECT MAX(value) AS maximum, COUNT() AS rows FROM nullable_values \
                   HAVING maximum = 9 ORDER BY rows DESC LIMIT 1";
        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database.global_aggregate_worker_cap(), &BUDGET);

        let held = BUDGET
            .acquire_for_test(BUDGET.helper_limit())
            .expect("test exhausts aggregate helper admission");
        let exhausted = query(&mut database, sql);
        assert_eq!(
            exhausted, sequential,
            "exhausted admission repeats the complete COUNT/nullable MAX pair locally"
        );
        assert_eq!(
            exhausted.rows,
            [vec![
                Value::Int64(9),
                Value::Int64(i64::try_from(row_count).unwrap()),
            ]]
        );
        assert_eq!(BUDGET.helpers_in_use(), BUDGET.helper_limit());
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn paired_nullable_int64_max_worker_failure_repeats_the_complete_pair_locally() {
        let values = [Some(-9), None, Some(i64::MAX)];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_global_nullable_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "max",
            i64::max,
        );
        let maximum = reduce_global_nullable_int64_extremum(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "max",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-max-nullable-int64-1") {
                    panic!("injected nullable MAX worker failure");
                }
                left.max(right)
            },
        );

        assert_eq!(successful_parallel, Some(i64::MAX));
        assert_eq!(maximum, successful_parallel);
        assert_eq!(
            count_matched_rows(matching_rows.len()),
            Ok(i64::try_from(row_count).unwrap())
        );
    }

    #[test]
    fn paired_nullable_int64_max_forced_workers_preserve_resource_limits() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let fixed_bytes = 2_usize.saturating_mul(
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>(),
        );
        let exact_limits = QueryResultLimits {
            max_scan_rows: row_count,
            max_rows: 1,
            max_values: 2,
            max_groups: 1,
            max_aggregate_state_cells: 2,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(exact_limits);
        database
            .create_nullable_int64_table(
                "nullable_values",
                "value",
                (0..row_count)
                    .map(|row| (row % 2 == 0).then_some(5))
                    .collect(),
            )
            .unwrap();
        let sql = "SELECT COUNT(*) AS rows, MAX(value) AS maximum FROM nullable_values";

        let sequential = force_global_aggregate_workers(&mut database, 1, sql);
        let parallel = force_global_aggregate_workers(&mut database, 4, sql);
        assert_eq!(parallel, sequential);
        assert_eq!(
            parallel.rows,
            [vec![
                Value::Int64(i64::try_from(row_count).unwrap()),
                Value::Int64(5),
            ]]
        );

        for (limits, expected) in [
            (
                QueryResultLimits {
                    max_scan_rows: row_count - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT scanned rows",
                    actual: row_count,
                    max: row_count - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_cells: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state cells",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_aggregate_state_bytes: fixed_bytes - 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT aggregate state bytes",
                    actual: fixed_bytes,
                    max: fixed_bytes - 1,
                },
            ),
            (
                QueryResultLimits {
                    max_values: 1,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result values",
                    actual: 2,
                    max: 1,
                },
            ),
            (
                QueryResultLimits {
                    max_rows: 0,
                    ..exact_limits
                },
                Error::ResourceLimitExceeded {
                    resource: "SELECT result rows",
                    actual: 1,
                    max: 0,
                },
            ),
        ] {
            database.query_result_limits = limits;
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
            let sequential = database.execute(sql);
            database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(4);
            let parallel = database.execute(sql);
            assert_eq!(parallel, sequential);
            assert_eq!(parallel, Err(expected));
        }
    }

    #[test]
    fn configured_cap_two_is_a_deterministic_parallel_differential() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(8);

        let cap = NonZeroUsize::new(2).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database_with_worker_cap(row_count, cap);
        assert_eq!(database.global_aggregate_worker_cap(), cap);
        BUDGET.reset_peak();
        database.global_aggregate_parallelism = GlobalAggregateParallelism::budgeted(cap, &BUDGET);
        let capped = query(&mut database, "SELECT countIf(active) FROM events");
        assert_eq!(
            BUDGET.peak_helpers_in_use(),
            1,
            "a two-lane cap admits exactly one helper"
        );

        let sequential =
            force_global_aggregate_workers(&mut database, 1, "SELECT countIf(active) FROM events");
        assert_eq!(capped, sequential);
    }

    #[test]
    fn runtime_cap_one_and_two_match_for_every_supported_global_aggregate() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(8);

        let initial = NonZeroUsize::new(7).unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database_with_worker_cap(row_count, initial);
        database
            .create_nullable_int64_table("nullable_values", "value", vec![Some(1); row_count])
            .unwrap();
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(initial, &BUDGET);

        for (query_index, sql) in [
            "SELECT COUNT(value) FROM nullable_values",
            "SELECT SUM(id) FROM events",
            "SELECT AVG(id) FROM events",
            "SELECT MIN(id) FROM events",
            "SELECT MIN(score) FROM events",
            "SELECT MAX(id) FROM events",
            "SELECT MAX(score) FROM events",
            "SELECT countIf(active) FROM events",
        ]
        .into_iter()
        .enumerate()
        {
            let expected_previous = if query_index == 0 { initial } else { two };
            assert_eq!(
                database.set_global_aggregate_worker_cap(one),
                expected_previous
            );
            BUDGET.reset_peak();
            let sequential = query(&mut database, sql);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                0,
                "the runtime cap of one must keep {sql} sequential"
            );

            assert_eq!(database.set_global_aggregate_worker_cap(two), one);
            BUDGET.reset_peak();
            let parallel = query(&mut database, sql);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                1,
                "the runtime cap of two must admit one helper for {sql}"
            );
            assert_eq!(
                sequential, parallel,
                "runtime worker differential for {sql}"
            );
        }
    }

    #[test]
    fn request_cap_one_and_two_match_for_every_supported_global_aggregate() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(8);
        static LIMITED_BUDGET: GlobalAggregateWorkerBudget =
            GlobalAggregateWorkerBudget::for_test(1);

        let database_cap = NonZeroUsize::new(4).unwrap();
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database_with_worker_cap(row_count, database_cap);
        database
            .create_nullable_int64_table("nullable_values", "value", vec![Some(1); row_count])
            .unwrap();
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database_cap, &BUDGET);

        for sql in [
            "SELECT COUNT(value) FROM nullable_values",
            "SELECT SUM(id) FROM events",
            "SELECT AVG(id) FROM events",
            "SELECT MIN(id) FROM events",
            "SELECT MIN(score) FROM events",
            "SELECT MAX(id) FROM events",
            "SELECT MAX(score) FROM events",
            "SELECT countIf(active) FROM events",
        ] {
            BUDGET.reset_peak();
            let single_worker = query_with_max_threads(&database, sql, 1);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                0,
                "max_threads=1 must keep {sql} sequential"
            );

            BUDGET.reset_peak();
            let multi_worker = query_with_max_threads(&database, sql, 2);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                1,
                "max_threads=2 must admit one helper for {sql}"
            );
            assert_eq!(
                single_worker, multi_worker,
                "request worker differential for {sql}"
            );
            assert_eq!(database.global_aggregate_worker_cap(), database_cap);
        }

        for max_threads in [0, usize::MAX] {
            BUDGET.reset_peak();
            let result = query_with_max_threads(
                &database,
                "SELECT countIf(active) FROM events",
                max_threads,
            );
            assert_eq!(result.rows, [vec![Value::Int64((row_count / 2) as i64)]]);
            assert_eq!(
                BUDGET.peak_helpers_in_use(),
                2,
                "max_threads={max_threads} must retain the four-lane database cap"
            );
            assert_eq!(database.global_aggregate_worker_cap(), database_cap);
        }

        LIMITED_BUDGET.reset_peak();
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database_cap, &LIMITED_BUDGET);
        let budget_limited =
            query_with_max_threads(&database, "SELECT countIf(active) FROM events", 4);
        assert_eq!(
            budget_limited.rows,
            [vec![Value::Int64((row_count / 2) as i64)]]
        );
        assert_eq!(
            LIMITED_BUDGET.peak_helpers_in_use(),
            1,
            "the request cap remains subject to process-wide admission"
        );

        let cap_one = NonZeroUsize::new(1).unwrap();
        assert_eq!(
            database.set_global_aggregate_worker_cap(cap_one),
            database_cap
        );
        LIMITED_BUDGET.reset_peak();
        let database_limited =
            query_with_max_threads(&database, "SELECT countIf(active) FROM events", 2);
        assert_eq!(database_limited, budget_limited);
        assert_eq!(
            LIMITED_BUDGET.peak_helpers_in_use(),
            0,
            "a larger request value must not relax the database cap"
        );
        assert_eq!(database.global_aggregate_worker_cap(), cap_one);
    }

    #[test]
    fn oversized_configured_cap_preserves_results_and_useful_worker_limit() {
        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(31);

        let cap = NonZeroUsize::new(usize::MAX).unwrap();
        let parallelism = GlobalAggregateParallelism::budgeted(cap, &BUDGET);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut database = count_if_database_with_worker_cap(row_count, cap);
        assert_eq!(database.global_aggregate_worker_cap(), cap);
        BUDGET.reset_peak();
        database.global_aggregate_parallelism = parallelism;
        let capped = query(&mut database, "SELECT countIf(active) FROM events");
        let peak_helpers = BUDGET.peak_helpers_in_use();
        assert_eq!(peak_helpers, 2);

        let sequential =
            force_global_aggregate_workers(&mut database, 1, "SELECT countIf(active) FROM events");
        assert_eq!(capped, sequential);
    }

    #[test]
    fn global_count_if_budget_caps_concurrent_admission() {
        use std::sync::{Arc, Barrier};

        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);
        BUDGET.reset_peak();
        let thread_count = 8;
        let started = Arc::new(Barrier::new(thread_count + 1));
        let attempted = Arc::new(Barrier::new(thread_count + 1));
        let release = Arc::new(Barrier::new(thread_count + 1));
        let handles = (0..thread_count)
            .map(|_| {
                let started = Arc::clone(&started);
                let attempted = Arc::clone(&attempted);
                let release = Arc::clone(&release);
                std::thread::spawn(move || {
                    started.wait();
                    let permit = BUDGET.acquire_for_test(1);
                    attempted.wait();
                    release.wait();
                    permit
                })
            })
            .collect::<Vec<_>>();

        started.wait();
        attempted.wait();
        assert_eq!(BUDGET.helpers_in_use(), 3);
        assert_eq!(BUDGET.peak_helpers_in_use(), 3);
        release.wait();
        let admitted = handles
            .into_iter()
            .map(|handle| handle.join().expect("admission worker joins").is_some())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 3);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn concurrent_request_cap_two_queries_match_sequential_and_obey_the_process_budget() {
        use crate::batch::shared_database::SharedDatabase;
        use std::sync::{Arc, Barrier};

        static BUDGET: GlobalAggregateWorkerBudget = GlobalAggregateWorkerBudget::for_test(3);
        let row_count =
            COUNT_IF_PARALLEL_ROW_THRESHOLD.saturating_add(COUNT_IF_PARALLEL_ROWS_PER_WORKER);
        let database_cap = NonZeroUsize::new(4).unwrap();
        let mut database = count_if_database_with_worker_cap(row_count, database_cap);
        database.global_aggregate_parallelism = GlobalAggregateParallelism::fixed(1);
        let sequential = query(&mut database, "SELECT countIf(active) FROM events");
        BUDGET.reset_peak();
        database.global_aggregate_parallelism =
            GlobalAggregateParallelism::budgeted(database_cap, &BUDGET);
        let database = SharedDatabase::new(database);
        let query_count = 8;
        let started = Arc::new(Barrier::new(query_count));
        let handles = (0..query_count)
            .map(|_| {
                let database = database.clone();
                let started = Arc::clone(&started);
                std::thread::spawn(move || {
                    started.wait();
                    database
                        .try_query_with_parameterized_workload_limits(
                            "SELECT countIf(active) FROM events",
                            ParameterizedQueryLimits {
                                max_result_bytes: DEFAULT_MAX_RETAINED_RESULT_BYTES,
                                max_result_rows: 0,
                                max_result_values: 0,
                                max_scan_rows: 0,
                                max_groups: 0,
                                max_group_key_cells: 0,
                                max_group_key_bytes: 0,
                                max_ordering_state_bytes: 0,
                                max_aggregate_state_cells: 0,
                                max_aggregate_state_bytes: 0,
                                max_threads: 2,
                            },
                        )
                        .expect("concurrent countIf query")
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            let result = handle.join().expect("query worker joins");
            assert_eq!(result, sequential, "concurrent cap-two differential");
        }
        let peak_helpers = BUDGET.peak_helpers_in_use();
        assert!((2..=BUDGET.helper_limit()).contains(&peak_helpers));
        assert_eq!(BUDGET.helpers_in_use(), 0);
        assert_eq!(
            database.global_aggregate_worker_cap().unwrap(),
            database_cap
        );
    }

    #[test]
    fn show_tables_returns_typed_empty_and_case_preserving_sorted_results() {
        let mut database = Database::new();
        let empty = query(&mut database, "SHOW TABLES");
        assert_eq!(
            empty.columns,
            [ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            }]
        );
        assert!(empty.rows.is_empty());

        database
            .execute(
                "CREATE TABLE zebra (id Int64); \
                 CREATE TABLE Alpha (id Int64); \
                 CREATE TABLE beta (id Int64);",
            )
            .expect("setup");
        assert_eq!(
            query(&mut database, "show tables;").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
                vec![Value::String("zebra".to_owned())],
            ]
        );
    }

    #[test]
    fn qualified_show_tables_lower_to_the_same_bounded_result() {
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 2,
            max_values: 2,
            ..QueryResultLimits::default()
        });
        database
            .execute("CREATE TABLE zebra (id Int64); CREATE TABLE Alpha (id Int64);")
            .expect("setup");

        let expected = query(&mut database, "SHOW TABLES");
        assert_eq!(query(&mut database, "sHoW TaBlEs FrOm DeFaUlT"), expected);
        assert_eq!(query(&mut database, "SHOW TABLES IN default;"), expected);

        database
            .execute("CREATE TABLE beta (id Int64)")
            .expect("third table");
        for sql in ["SHOW TABLES FROM default", "SHOW TABLES IN DEFAULT"] {
            assert_eq!(
                database.execute(sql),
                Err(Error::ResourceLimitExceeded {
                    resource: "SHOW TABLES result rows",
                    actual: 3,
                    max: 2,
                })
            );
        }
    }

    #[test]
    fn show_tables_accepts_exact_custom_row_and_value_limits() {
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 2,
            max_values: 2,
            ..QueryResultLimits::default()
        });
        database
            .execute("CREATE TABLE beta (id Int64); CREATE TABLE Alpha (id Int64);")
            .expect("setup");

        assert_eq!(
            query(&mut database, "SHOW TABLES").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
            ]
        );
    }

    #[test]
    fn show_tables_rejects_exceeded_custom_row_and_value_limits() {
        let cases = [
            (
                QueryResultLimits {
                    max_rows: 1,
                    ..QueryResultLimits::default()
                },
                "SHOW TABLES result rows",
            ),
            (
                QueryResultLimits {
                    max_rows: 2,
                    max_values: 1,
                    ..QueryResultLimits::default()
                },
                "SHOW TABLES result values",
            ),
        ];

        for (limits, resource) in cases {
            let mut database = Database::with_query_result_limits(limits);
            database
                .execute("CREATE TABLE Alpha (id Int64); CREATE TABLE beta (id Int64);")
                .expect("setup");
            let error = database
                .execute("SHOW TABLES")
                .expect_err("SHOW TABLES exceeds its configured result limit");
            assert_eq!(
                error,
                Error::ResourceLimitExceeded {
                    resource,
                    actual: 2,
                    max: 1,
                }
            );
        }
    }

    #[test]
    fn show_tables_accepts_exact_and_rejects_exceeded_name_payload_byte_limit() {
        let table_count = 2;
        let columns = [ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }];
        let fixed_bytes = validate_result_shape(
            table_count,
            1,
            &columns,
            QueryResultLimits::default(),
            SHOW_TABLES_RESULT_RESOURCES,
        )
        .expect("fixed result shape fits default limits");
        let name_bytes = "Alpha".len() + "beta".len();
        let exact_bytes = fixed_bytes + name_bytes;
        let mut exact_database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: table_count,
            max_values: table_count,
            max_bytes: exact_bytes,
            ..QueryResultLimits::default()
        });
        exact_database
            .execute("CREATE TABLE beta (id Int64); CREATE TABLE Alpha (id Int64);")
            .expect("setup");
        assert_eq!(
            query(&mut exact_database, "SHOW TABLES").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
            ]
        );

        let max_bytes = exact_bytes - 1;
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: table_count,
            max_values: table_count,
            max_bytes,
            ..QueryResultLimits::default()
        });
        database
            .execute("CREATE TABLE Alpha (id Int64); CREATE TABLE beta (id Int64);")
            .expect("setup");

        assert_eq!(
            database
                .execute("SHOW TABLES")
                .expect_err("name payload exceeds the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SHOW TABLES result bytes",
                actual: exact_bytes,
                max: max_bytes,
            }
        );
    }

    #[test]
    fn show_tables_obeys_retained_result_limits() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE Alpha (id Int64)")
            .expect("setup");
        assert!(matches!(
            database.execute_with_result_limit("SHOW TABLES", 1),
            Err(Error::ResultLimitExceeded {
                bytes,
                max_bytes: 1,
            }) if bytes > 1
        ));
    }

    #[test]
    fn having_count_column_alias_supports_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String); \
                 INSERT INTO events VALUES \
                 ('a'), ('a'), ('a'), ('b'), ('b'), ('c');",
            )
            .expect("setup");

        let cases = [
            ("=", &["b"][..]),
            ("!=", &["a", "c"][..]),
            ("<>", &["a", "c"][..]),
            ("<", &["c"][..]),
            ("<=", &["b", "c"][..]),
            (">", &["a"][..]),
            (">=", &["a", "b"][..]),
        ];
        for (operator, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, COUNT(kind) AS Occurrences FROM events \
                     GROUP BY kind HAVING occurrences {operator} 2 ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_min_and_max_int64_aliases_support_grouped_and_global_inputs() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64); \
                 INSERT INTO events VALUES \
                 ('a', -5), ('a', 2), \
                 ('b', 0), ('b', 7), ('b', 9), \
                 ('c', -1);",
            )
            .expect("setup");

        let grouped_cases = [
            ("MIN", "<", "0", &["a", "c"][..]),
            ("MIN", ">=", "+0", &["b"][..]),
            ("MAX", "=", "2", &["a"][..]),
            ("MAX", "!=", "2", &["b", "c"][..]),
            ("MAX", "<>", "2", &["b", "c"][..]),
            ("MAX", "<=", "2", &["a", "c"][..]),
            ("MAX", ">", "2", &["b"][..]),
        ];
        for (function, operator, threshold, expected_kinds) in grouped_cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, {function}(amount) AS extreme FROM events \
                     GROUP BY kind HAVING extreme {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "{function} {operator}");
        }

        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(amount) AS n FROM events HAVING n = 6"
            )
            .rows,
            vec![vec![Value::Int64(6)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(amount) AS low FROM events HAVING low = -5"
            )
            .rows,
            vec![vec![Value::Int64(-5)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(amount) AS high FROM events HAVING high >= +9"
            )
            .rows,
            vec![vec![Value::Int64(9)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT AVG(amount) AS mean FROM events HAVING mean = 2"
            )
            .rows,
            vec![vec![Value::Float64(2.0)]]
        );
    }

    #[test]
    fn having_sum_int64_alias_supports_signed_sums_and_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64); \
                 INSERT INTO events VALUES \
                 ('negative', -5), ('negative', 1), \
                 ('zero', -2), ('zero', 2), \
                 ('positive', 2), ('positive', 5);",
            )
            .expect("setup");

        let cases = [
            ("=", "-4", &["negative"][..]),
            ("!=", "0", &["negative", "positive"][..]),
            ("<>", "0", &["negative", "positive"][..]),
            ("<", "0", &["negative"][..]),
            ("<=", "0", &["negative", "zero"][..]),
            (">", "-4", &["positive", "zero"][..]),
            (">=", "+7", &["positive"][..]),
        ];
        for (operator, threshold, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, COUNT(*) AS n, SUM(amount) AS Total FROM events \
                     GROUP BY kind HAVING total {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_float64_sum_alias_supports_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES ('a', 1.5), ('b', 2.5), ('c', 3.5);",
            )
            .expect("setup");

        let cases = [
            ("=", &["b"][..]),
            ("!=", &["a", "c"][..]),
            ("<>", &["a", "c"][..]),
            ("<", &["a"][..]),
            ("<=", &["a", "b"][..]),
            (">", &["c"][..]),
            (">=", &["b", "c"][..]),
        ];
        for (operator, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, SUM(score) AS total FROM events \
                     GROUP BY kind HAVING total {operator} +2.5e0 ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_all_float64_aggregate_aliases_support_grouped_and_global_inputs() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES \
                 ('a', 1.5), ('a', 2.5), \
                 ('b', -2.0), ('b', 6.0), \
                 ('c', 10.0);",
            )
            .expect("setup");

        let grouped_cases = [
            ("SUM", "total", ">", "4", &["c"][..]),
            ("MIN", "low", "<", "0.0", &["b"][..]),
            ("MAX", "high", ">=", "+6", &["b", "c"][..]),
            ("AVG", "mean", "=", "2", &["a", "b"][..]),
        ];
        for (function, alias, operator, threshold, expected_kinds) in grouped_cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, {function}(score) AS {alias} FROM events \
                     GROUP BY kind HAVING {alias} {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "{function}");
        }

        let global_cases = [
            ("SUM", "=", "18", Value::Float64(18.0)),
            ("MIN", "=", "-2", Value::Float64(-2.0)),
            ("MAX", ">=", "10.0", Value::Float64(10.0)),
            ("AVG", ">", "3.5", Value::Float64(3.599_999_999_999_999_6)),
        ];
        for (function, operator, threshold, expected) in global_cases {
            assert_eq!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {function}(score) AS value FROM events \
                         HAVING value {operator} {threshold}"
                    ),
                )
                .rows,
                vec![vec![expected]],
                "global {function}"
            );
        }
    }

    #[test]
    fn having_uses_exact_mixed_int64_float64_comparisons() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES ('a', 9007199254740992.0);",
            )
            .expect("setup");

        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(score) AS total FROM events HAVING total < 9007199254740993"
            )
            .rows,
            vec![vec![Value::Float64(9_007_199_254_740_992.0)]]
        );
        assert!(
            query(
                &mut database,
                "SELECT SUM(score) AS total FROM events HAVING total = 9007199254740993"
            )
            .rows
            .is_empty()
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS n FROM events HAVING n < 1.5"
            )
            .rows,
            vec![vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn having_filters_before_ordering_and_limiting() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Float64); \
                 INSERT INTO events VALUES \
                 ('a', 8.5), ('a', 9.5), ('b', 3.5), ('b', 4.5), ('c', 1.5);",
            )
            .expect("setup");

        let result = query(
            &mut database,
            "SELECT kind, MAX(amount) AS high FROM events \
             GROUP BY kind HAVING high > 1.5 ORDER BY high ASC LIMIT 1",
        );
        assert_eq!(
            result.rows,
            vec![vec![Value::String("b".to_owned()), Value::Float64(4.5)]]
        );
    }

    #[test]
    fn having_nullness_filters_finalized_empty_and_populated_aggregates_before_order_and_limit() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (amount Int64, score Float64, active Bool, label String);",
            )
            .expect("setup");

        let cases = [
            ("SUM(amount)", Value::Int64(3)),
            ("MIN(label)", Value::String("present".to_owned())),
            ("MAX(active)", Value::Bool(true)),
            ("AVG(score)", Value::Float64(2.5)),
        ];
        for (aggregate, populated_value) in &cases {
            let null_result = query(
                &mut database,
                &format!(
                    "SELECT {aggregate} AS value FROM events \
                     HAVING value IS NULL ORDER BY value DESC LIMIT 1"
                ),
            );
            assert_eq!(null_result.rows.len(), 1, "empty {aggregate}");
            assert!(
                matches!(null_result.rows[0][0], Value::Null(_)),
                "empty {aggregate} is a finalized typed NULL"
            );
            assert!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NOT NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows
                .is_empty(),
                "empty {aggregate} must not satisfy IS NOT NULL"
            );

            database
                .execute("INSERT INTO events VALUES (3, 2.5, true, 'present')")
                .expect("populate aggregate input");
            assert!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows
                .is_empty(),
                "populated {aggregate} must not satisfy IS NULL"
            );
            assert_eq!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NOT NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows,
                vec![vec![populated_value.clone()]],
                "populated {aggregate}"
            );

            database
                .execute("TRUNCATE TABLE events")
                .expect("restore empty input for the next aggregate");
        }

        assert!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows FROM events HAVING rows IS NULL"
            )
            .rows
            .is_empty()
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows FROM events HAVING rows IS NOT NULL"
            )
            .rows,
            vec![vec![Value::Int64(0)]]
        );
    }

    #[test]
    fn having_rejects_unknown_ambiguous_and_unsupported_aliases() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64, score Float64); \
                 INSERT INTO events VALUES ('a', 1, 1.5), ('a', 2, 2.5);",
            )
            .expect("setup");

        let cases = [
            (
                "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind HAVING missing > 0",
                "HAVING alias 'missing' is not in the SELECT output",
            ),
            (
                "SELECT kind, MIN(amount) AS Total, MAX(amount) AS total FROM events \
                 GROUP BY kind HAVING total > 0",
                "HAVING alias 'total' is ambiguous",
            ),
            (
                "SELECT kind AS total, COUNT(*) AS n FROM events GROUP BY kind HAVING total > 0",
                "HAVING alias 'total' must reference a projected numeric aggregate",
            ),
            (
                "SELECT kind, MAX(kind) AS high FROM events GROUP BY kind HAVING high > 0",
                "HAVING alias 'high' must reference a projected numeric aggregate",
            ),
        ];

        for (sql, expected) in cases {
            assert_eq!(
                database.execute(sql).expect_err("invalid HAVING alias"),
                Error::InvalidQuery(expected.to_owned()),
                "{sql}"
            );
        }

        let nullness_cases = [
            (
                "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind HAVING missing IS NULL",
                "HAVING alias 'missing' is not in the SELECT output",
            ),
            (
                "SELECT kind, MIN(amount) AS Total, MAX(amount) AS total FROM events \
                 GROUP BY kind HAVING total IS NOT NULL",
                "HAVING alias 'total' is ambiguous",
            ),
            (
                "SELECT kind AS total, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING total IS NULL",
                "HAVING alias 'total' must reference a projected aggregate",
            ),
        ];
        for (sql, expected) in nullness_cases {
            assert_eq!(
                database.execute(sql).expect_err("invalid HAVING alias"),
                Error::InvalidQuery(expected.to_owned()),
                "{sql}"
            );
        }
    }

    #[test]
    fn direct_ast_having_rejects_invalid_threshold_values() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (amount Float64); INSERT INTO events VALUES (1.5);")
            .expect("setup");
        let Statement::Select(select) =
            sql::parse("SELECT SUM(amount) AS total FROM events HAVING total > 1.0")
                .expect("baseline query parses")
                .remove(0)
        else {
            panic!("expected select");
        };

        let cases = [
            (
                Value::Float64(f64::INFINITY),
                Error::InvalidQuery(
                    "HAVING comparison Float64 thresholds must be finite".to_owned(),
                ),
            ),
            (
                Value::Null(DataType::Float64),
                Error::InvalidQuery("HAVING comparisons do not support NULL thresholds".to_owned()),
            ),
            (
                Value::String("1.0".to_owned()),
                Error::TypeMismatch {
                    context: "HAVING comparison threshold".to_owned(),
                    expected: "Int64 or Float64".to_owned(),
                    actual: "String".to_owned(),
                },
            ),
        ];
        for (value, expected) in cases {
            let mut invalid = select.clone();
            let HavingPredicate::Comparison {
                value: invalid_value,
                ..
            } = &mut invalid.having.as_mut().expect("HAVING exists").predicate
            else {
                panic!("baseline HAVING is a comparison");
            };
            *invalid_value = value;
            assert_eq!(
                database
                    .execute_statement(Statement::Select(invalid))
                    .expect_err("invalid direct AST HAVING threshold"),
                expected
            );
        }
    }

    #[test]
    fn having_handles_empty_global_and_grouped_inputs() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (kind String, amount Int64, score Float64);")
            .expect("setup");

        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(amount) AS n FROM events HAVING n = 0"
            )
            .rows,
            vec![vec![Value::Int64(0)]]
        );
        assert!(
            query(
                &mut database,
                "SELECT COUNT(*) AS n FROM events HAVING n > 0"
            )
            .rows
            .is_empty()
        );
        assert!(
            query(
                &mut database,
                "SELECT kind, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING n = 0"
            )
            .rows
            .is_empty()
        );

        for function in ["SUM", "MIN", "MAX"] {
            assert_eq!(
                query(
                    &mut database,
                    &format!("SELECT {function}(amount) AS value FROM events")
                )
                .rows,
                vec![vec![Value::Null(DataType::Int64)]],
                "empty {function} is NULL"
            );
            for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
                assert!(
                    query(
                        &mut database,
                        &format!(
                            "SELECT {function}(amount) AS value FROM events \
                             HAVING value {operator} 0"
                        )
                    )
                    .rows
                    .is_empty(),
                    "NULL {function} must make {operator} predicate false"
                );
            }
        }

        for function in ["SUM", "MIN", "MAX", "AVG"] {
            assert_eq!(
                query(
                    &mut database,
                    &format!("SELECT {function}(score) AS value FROM events")
                )
                .rows,
                vec![vec![Value::Null(DataType::Float64)]],
                "empty {function}(Float64) is NULL"
            );
            for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
                assert!(
                    query(
                        &mut database,
                        &format!(
                            "SELECT {function}(score) AS value FROM events \
                             HAVING value {operator} 0.0"
                        )
                    )
                    .rows
                    .is_empty(),
                    "NULL {function}(Float64) must make {operator} predicate false"
                );
            }
        }

        assert!(
            query(
                &mut database,
                "SELECT kind, AVG(score) AS mean FROM events \
                 GROUP BY kind HAVING mean > 0.0"
            )
            .rows
            .is_empty()
        );
    }

    #[test]
    fn having_preserves_group_working_limits_and_reduces_result_limits() {
        let setup = "CREATE TABLE events (kind String); \
            INSERT INTO events VALUES ('a'), ('a'), ('a'), ('b'), ('b'), ('c');";
        let sql = "SELECT kind, COUNT(*) AS n FROM events \
            GROUP BY kind HAVING n > 100";

        let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
            max_groups: 2,
            ..QueryResultLimits::default()
        });
        group_limited.execute(setup).expect("setup");
        assert_eq!(
            group_limited
                .execute(sql)
                .expect_err("HAVING cannot hide excess working groups"),
            Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 3,
                max: 2,
            }
        );

        let mut state_limited = Database::with_query_result_limits(QueryResultLimits {
            max_groups: 3,
            max_aggregate_state_cells: 2,
            ..QueryResultLimits::default()
        });
        state_limited.execute(setup).expect("setup");
        assert_eq!(
            state_limited
                .execute(sql)
                .expect_err("HAVING cannot hide excess aggregate state"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 3,
                max: 2,
            }
        );

        let mut result_limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 2,
            max_bytes: usize::MAX,
            ..QueryResultLimits::default()
        });
        result_limited.execute(setup).expect("setup");
        assert_eq!(
            query(
                &mut result_limited,
                "SELECT kind, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING n > 2"
            )
            .rows,
            vec![vec![Value::String("a".to_owned()), Value::Int64(3)]]
        );
    }

    #[test]
    fn filters_with_boolean_precedence() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE valueset (id Int64, enabled Bool); \
                 INSERT INTO valueset VALUES (1, false), (2, true), (3, false);",
            )
            .expect("setup");
        let result = query(
            &mut database,
            "SELECT id FROM valueset WHERE id = 1 OR id >= 2 AND enabled = true",
        );
        assert_eq!(
            result.rows,
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );
    }

    #[test]
    fn int64_sum_uses_the_final_exact_sum_independent_of_row_order() {
        for values in [
            "(9223372036854775807), (1), (-1)",
            "(9223372036854775807), (-1), (1)",
        ] {
            let mut database = Database::new();
            database
                .execute(&format!(
                    "CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES {values};"
                ))
                .expect("setup");

            assert_eq!(
                query(&mut database, "SELECT SUM(n) AS total FROM numbers").rows,
                vec![vec![Value::Int64(i64::MAX)]]
            );
        }
    }

    #[test]
    fn float64_average_scales_finite_boundary_values_without_overflow() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE numbers (n Float64); \
                 INSERT INTO numbers VALUES \
                 (1.7976931348623157e308), (1.7976931348623157e308);",
            )
            .expect("setup");

        assert_eq!(
            query(&mut database, "SELECT AVG(n) AS mean FROM numbers").rows,
            vec![vec![Value::Float64(f64::MAX)]]
        );

        let mut cancelling = Database::new();
        cancelling
            .execute(
                "CREATE TABLE numbers (n Float64); \
                 INSERT INTO numbers VALUES \
                 (1.7976931348623157e308), (-1.7976931348623157e308);",
            )
            .expect("setup");
        assert_eq!(
            query(&mut cancelling, "SELECT AVG(n) AS mean FROM numbers").rows,
            vec![vec![Value::Float64(0.0)]]
        );
    }

    #[test]
    fn empty_global_aggregates_return_one_row_with_typed_nulls() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE samples (i Int64, f Float64, b Bool, s String);")
            .expect("create");
        let aggregate_sql = "SELECT COUNT(*) AS rows, SUM(i) AS int_sum, \
            SUM(f) AS float_sum, MIN(s) AS first, MAX(b) AS last, AVG(f) AS mean \
            FROM samples";
        let expected = vec![vec![
            Value::Int64(0),
            Value::Null(DataType::Int64),
            Value::Null(DataType::Float64),
            Value::Null(DataType::String),
            Value::Null(DataType::Bool),
            Value::Null(DataType::Float64),
        ]];

        assert_eq!(query(&mut database, aggregate_sql).rows, expected);

        database
            .execute("INSERT INTO samples VALUES (1, 2.0, true, 'present')")
            .expect("insert");
        assert_eq!(
            query(&mut database, &format!("{aggregate_sql} WHERE i < 0")).rows,
            expected
        );
    }

    #[test]
    fn grouped_aggregate_state_cell_limit_applies_before_limit() {
        let setup = "CREATE TABLE samples (g Int64, value Int64); \
            INSERT INTO samples VALUES (1, 10), (2, 20);";
        let sql = "SELECT g, MIN(value), MAX(value) FROM samples GROUP BY g LIMIT 1";

        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: usize::MAX,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact.execute(setup).expect("setup");
        assert_eq!(query(&mut exact, sql).rows.len(), 1);

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_cells: 3,
            ..exact_limits
        });
        limited.execute(setup).expect("setup");
        assert_eq!(
            limited
                .execute(sql)
                .expect_err("four working cells exceed the limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 4,
                max: 3,
            }
        );
    }

    #[test]
    fn grouped_aggregate_state_byte_limit_includes_string_extrema() {
        let setup = "CREATE TABLE samples (g Int64, value String); \
            INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');";
        let sql = "SELECT g, MIN(value), MAX(value) FROM samples GROUP BY g LIMIT 1";
        let fixed_bytes = 4 * std::mem::size_of::<AggregateState>()
            + 2 * std::mem::size_of::<Vec<AggregateState>>();

        let mut preallocation_limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: fixed_bytes - 1,
            ..QueryResultLimits::default()
        });
        preallocation_limited.execute(setup).expect("setup");
        assert_eq!(
            preallocation_limited
                .execute(sql)
                .expect_err("fixed working state exceeds the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes,
                max: fixed_bytes - 1,
            }
        );

        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: fixed_bytes + 16,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact.execute(setup).expect("setup");
        assert_eq!(query(&mut exact, sql).rows.len(), 1);

        let mut string_limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_bytes: fixed_bytes + 15,
            ..exact_limits
        });
        string_limited.execute(setup).expect("setup");
        assert_eq!(
            string_limited
                .execute(sql)
                .expect_err("cloned extrema strings exceed the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes + 16,
                max: fixed_bytes + 15,
            }
        );
    }

    #[test]
    fn nullable_int64_extrema_obey_the_exact_aggregate_state_byte_boundary() {
        let fixed_bytes =
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>();
        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 1,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact
            .create_nullable_int64_table(
                "readings",
                "v",
                vec![None, Some(i64::MAX), Some(i64::MIN)],
            )
            .unwrap();
        assert_eq!(
            query(&mut exact, "SELECT MIN(v) FROM readings").rows,
            [vec![Value::Int64(i64::MIN)]]
        );
        assert_eq!(
            query(&mut exact, "SELECT MAX(v) FROM readings").rows,
            [vec![Value::Int64(i64::MAX)]]
        );

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_bytes: fixed_bytes - 1,
            ..exact_limits
        });
        limited
            .create_nullable_int64_table("readings", "v", vec![None, Some(1)])
            .unwrap();
        assert_eq!(
            limited.execute("SELECT MAX(v) FROM readings"),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes,
                max: fixed_bytes - 1,
            })
        );
    }

    #[test]
    fn nullable_int64_avg_obeys_the_exact_aggregate_state_byte_boundary() {
        let fixed_bytes =
            std::mem::size_of::<AggregateState>() + std::mem::size_of::<Vec<AggregateState>>();
        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 1,
            max_aggregate_state_bytes: fixed_bytes,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact
            .create_nullable_int64_table(
                "readings",
                "v",
                vec![None, Some(i64::MAX), Some(i64::MIN)],
            )
            .unwrap();
        assert_eq!(
            query(&mut exact, "SELECT AVG(v) FROM readings").rows,
            [vec![Value::Float64(-0.5)]]
        );

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_bytes: fixed_bytes - 1,
            ..exact_limits
        });
        limited
            .create_nullable_int64_table("readings", "v", vec![None, Some(1)])
            .unwrap();
        assert_eq!(
            limited.execute("SELECT AVG(v) FROM readings"),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes,
                max: fixed_bytes - 1,
            })
        );
    }

    #[test]
    fn collecting_api_enforces_retained_result_limit() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE notes (s String); INSERT INTO notes VALUES ('abcdefghij');")
            .expect("setup");

        let error = database
            .execute_with_result_limit("SELECT s FROM notes", 1)
            .expect_err("result exceeds explicit retained byte limit");
        assert!(matches!(
            error,
            Error::ResultLimitExceeded {
                bytes,
                max_bytes: 1
            } if bytes > 1
        ));
    }

    #[test]
    fn select_materialization_limits_apply_before_owned_projection_rows() {
        let limits = QueryResultLimits {
            max_rows: 2,
            max_values: 4,
            max_bytes: usize::MAX,
            max_groups: 10,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(limits);
        database
            .execute(
                "CREATE TABLE entries (id Int64, label String); \
                 INSERT INTO entries VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .expect("setup");

        assert_eq!(
            query(&mut database, "SELECT id, label FROM entries LIMIT 2")
                .rows
                .len(),
            2
        );
        let error = database
            .execute("SELECT id FROM entries")
            .expect_err("third projected row exceeds the row limit");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: 3,
                max: 2,
            }
        );

        let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 3,
            max_values: 5,
            ..limits
        });
        value_limited
            .execute(
                "CREATE TABLE entries (id Int64, label String); \
                 INSERT INTO entries VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .expect("setup");
        let error = value_limited
            .execute("SELECT id, label FROM entries")
            .expect_err("six projected values exceed the value limit");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: 6,
                max: 5,
            }
        );
    }

    #[test]
    fn select_byte_limit_counts_string_payload_before_cloning() {
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: 100,
            max_groups: 1,
            ..QueryResultLimits::default()
        });
        database
            .execute(&format!(
                "CREATE TABLE entries (label String); INSERT INTO entries VALUES ('{}');",
                "x".repeat(128)
            ))
            .expect("setup");

        let error = database
            .execute("SELECT label FROM entries")
            .expect_err("string payload exceeds byte limit");
        assert!(matches!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual,
                max: 100,
            } if actual > 100
        ));
    }

    #[test]
    fn row_number_conversion_is_one_based_and_checked() {
        assert_eq!(checked_row_number(0), Ok(1));
        assert_eq!(checked_row_number(41), Ok(42));

        if let Ok(max_i64) = usize::try_from(i64::MAX) {
            assert_eq!(
                checked_row_number(max_i64),
                Err(Error::NumericOverflow("ROW_NUMBER result".to_owned()))
            );
        }
    }

    #[test]
    fn system_metric_int64_conversion_accepts_exact_max_and_rejects_overflow() {
        assert_eq!(
            checked_system_metric_value("rusthouse_retained_value_bytes", i64::MAX as u128),
            Ok(i64::MAX)
        );
        assert_eq!(
            checked_system_metric_value("rusthouse_retained_value_bytes", (i64::MAX as u128) + 1,),
            Err(Error::NumericOverflow(
                "system.metrics rusthouse_retained_value_bytes value".to_owned()
            ))
        );

        let mut database = Database::new();
        database.measurements.retained_value_bytes = (i64::MAX as u128) + 1;
        assert_eq!(
            database.execute("SELECT metric, value FROM system.metrics"),
            Err(Error::NumericOverflow(
                "system.metrics rusthouse_retained_value_bytes value".to_owned()
            ))
        );
    }
}
