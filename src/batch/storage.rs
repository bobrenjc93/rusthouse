use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::ops::Range;

use crate::batch::error::{Error, Result};
use crate::batch::value::{DataType, Value, ValueRef};

/// Default maximum number of rows retained by one typed batch table.
pub const DEFAULT_MAX_ROWS_PER_TABLE: usize = 1_000_000;
/// Default maximum number of physical columns retained by one typed batch table.
pub const DEFAULT_MAX_COLUMNS_PER_TABLE: usize = 1_024;
/// Default maximum number of physical scalar cells retained by one typed batch table.
pub const DEFAULT_MAX_CELLS_PER_TABLE: usize = 4_000_000;
/// Default number of consecutive source rows summarized by one sparse index block.
pub const DEFAULT_INT64_MIN_MAX_INDEX_BLOCK_ROWS: usize = 1_024;
/// Default maximum number of metadata blocks retained by one sparse index.
pub const DEFAULT_INT64_MIN_MAX_INDEX_BLOCKS: usize = 4_096;
/// Default maximum metadata bytes retained by one sparse index.
pub const DEFAULT_INT64_MIN_MAX_INDEX_BYTES: usize = 1024 * 1024;

/// Admission limits for one optional sparse `Int64` min/max index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64MinMaxIndexLimits {
    /// Deterministic number of source rows represented by each block.
    pub block_rows: usize,
    /// Maximum number of block metadata entries.
    pub max_blocks: usize,
    /// Maximum bytes occupied by block metadata, excluding the `Vec` header.
    pub max_bytes: usize,
}

impl Int64MinMaxIndexLimits {
    #[must_use]
    pub const fn new(block_rows: usize, max_blocks: usize, max_bytes: usize) -> Self {
        Self {
            block_rows,
            max_blocks,
            max_bytes,
        }
    }
}

/// Default maximum number of ranges in one caller-built partitioned table.
pub const DEFAULT_MAX_INT64_RANGE_PARTITIONS: usize = 1_024;
/// Default maximum rows accepted by the partition-table construction API.
pub const DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS: usize = DEFAULT_MAX_ROWS_PER_TABLE;
/// Default scalar payload bytes accepted by the partition-table construction API.
pub const DEFAULT_MAX_INT64_RANGE_PARTITION_BYTES: usize =
    DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS * std::mem::size_of::<i64>();

/// One inclusive `Int64` range and the rows physically assigned to it.
///
/// Partition order and membership are validated when a database table is
/// built. Values retain their order within the supplied partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Int64RangePartition {
    lower_bound: i64,
    upper_bound: i64,
    values: Vec<i64>,
}

impl Int64RangePartition {
    /// Describes one inclusive `[lower_bound, upper_bound]` partition.
    #[must_use]
    pub fn new(lower_bound: i64, upper_bound: i64, values: Vec<i64>) -> Self {
        Self {
            lower_bound,
            upper_bound,
            values,
        }
    }

    #[must_use]
    pub const fn lower_bound(&self) -> i64 {
        self.lower_bound
    }

    #[must_use]
    pub const fn upper_bound(&self) -> i64 {
        self.upper_bound
    }

    #[must_use]
    pub fn values(&self) -> &[i64] {
        &self.values
    }
}

/// Caller limits applied before a range-partitioned table is materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64RangePartitionLimits {
    pub max_partitions: usize,
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Int64RangePartitionLimits {
    #[must_use]
    pub const fn new(max_partitions: usize, max_rows: usize, max_bytes: usize) -> Self {
        Self {
            max_partitions,
            max_rows,
            max_bytes,
        }
    }
}

impl Default for Int64MinMaxIndexLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_INT64_MIN_MAX_INDEX_BLOCK_ROWS,
            DEFAULT_INT64_MIN_MAX_INDEX_BLOCKS,
            DEFAULT_INT64_MIN_MAX_INDEX_BYTES,
        )
    }
}

impl Default for Int64RangePartitionLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_INT64_RANGE_PARTITIONS,
            DEFAULT_MAX_INT64_RANGE_PARTITION_ROWS,
            DEFAULT_MAX_INT64_RANGE_PARTITION_BYTES,
        )
    }
}

/// Immutable summary for one deterministic sparse-index row block.
///
/// `min` and `max` summarize non-null values only. Both are `None` for an
/// all-null block; `null_count` distinguishes that case from an empty block.
/// Both non-nullable and nullable physical `Int64` columns use these explicit
/// block semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64MinMaxBlockMetadata {
    pub first_row: usize,
    pub row_count: usize,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub null_count: usize,
}

/// Public metadata about an admitted sparse index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Int64MinMaxIndexInfo {
    pub column: String,
    pub block_rows: usize,
    pub block_count: usize,
    pub indexed_rows: usize,
    pub retained_bytes: usize,
}

/// A non-error reason why a sparse-index request was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64MinMaxIndexRejection {
    ZeroBlockRows,
    SlotOccupied { table: String },
    BlockLimitExceeded { required: usize, max: usize },
    ByteLimitExceeded { required: usize, max: usize },
}

/// Result of attempting to admit an optional sparse index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64MinMaxIndexAdmission {
    Created(Int64MinMaxIndexInfo),
    Rejected(Int64MinMaxIndexRejection),
}

#[derive(Debug, Clone)]
struct Int64MinMaxIndex {
    column: usize,
    limits: Int64MinMaxIndexLimits,
    indexed_rows: usize,
    source_generation: u64,
    blocks: Vec<Int64MinMaxBlockMetadata>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Int64Filter {
    Equal(i64),
    Less(i64),
    LessOrEqual(i64),
    Greater(i64),
    GreaterOrEqual(i64),
}

#[derive(Debug)]
pub(crate) struct Int64MinMaxIndexScan {
    pub(crate) ranges: Vec<Range<usize>>,
    pub(crate) scanned_blocks: usize,
    pub(crate) pruned_blocks: usize,
}

/// A typed failure while validating or publishing a range-partitioned table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Int64RangePartitionError {
    PartitionLimitExceeded {
        partitions: usize,
        max_partitions: usize,
    },
    RowLimitExceeded {
        rows: usize,
        max_rows: usize,
    },
    ByteLimitExceeded {
        bytes: usize,
        max_bytes: usize,
    },
    InvalidRange {
        partition_index: usize,
        lower_bound: i64,
        upper_bound: i64,
    },
    OutOfOrder {
        partition_index: usize,
        previous_lower_bound: i64,
        lower_bound: i64,
    },
    Overlap {
        partition_index: usize,
        previous_upper_bound: i64,
        lower_bound: i64,
    },
    ValueOutOfRange {
        partition_index: usize,
        value_index: usize,
        value: i64,
        lower_bound: i64,
        upper_bound: i64,
    },
    /// SQL identifiers, duplicate names, or configured table limits failed.
    Table(Error),
}

impl fmt::Display for Int64RangePartitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionLimitExceeded {
                partitions,
                max_partitions,
            } => write!(
                formatter,
                "range-partitioned table has {partitions} partitions, exceeding the limit of {max_partitions}"
            ),
            Self::RowLimitExceeded { rows, max_rows } => write!(
                formatter,
                "range-partitioned table has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "range-partitioned table retains {bytes} value bytes, exceeding the limit of {max_bytes}"
            ),
            Self::InvalidRange {
                partition_index,
                lower_bound,
                upper_bound,
            } => write!(
                formatter,
                "partition {partition_index} has descending bounds [{lower_bound}, {upper_bound}]"
            ),
            Self::OutOfOrder {
                partition_index,
                previous_lower_bound,
                lower_bound,
            } => write!(
                formatter,
                "partition {partition_index} starts at {lower_bound}, before the previous partition start {previous_lower_bound}"
            ),
            Self::Overlap {
                partition_index,
                previous_upper_bound,
                lower_bound,
            } => write!(
                formatter,
                "partition {partition_index} starts at {lower_bound}, overlapping the previous inclusive upper bound {previous_upper_bound}"
            ),
            Self::ValueOutOfRange {
                partition_index,
                value_index,
                value,
                lower_bound,
                upper_bound,
            } => write!(
                formatter,
                "partition {partition_index} value {value_index} ({value}) is outside inclusive bounds [{lower_bound}, {upper_bound}]"
            ),
            Self::Table(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Int64RangePartitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::PartitionLimitExceeded { .. }
            | Self::RowLimitExceeded { .. }
            | Self::ByteLimitExceeded { .. }
            | Self::InvalidRange { .. }
            | Self::OutOfOrder { .. }
            | Self::Overlap { .. }
            | Self::ValueOutOfRange { .. } => None,
        }
    }
}

impl From<Error> for Int64RangePartitionError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

/// Persistent resource limits applied to one typed batch table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableLimits {
    /// Maximum rows retained by the table.
    pub max_rows: usize,
    /// Maximum physical columns retained by the table.
    pub max_columns: usize,
    /// Maximum `rows * columns` physical scalar cells retained by the table.
    pub max_cells: usize,
}

impl TableLimits {
    #[must_use]
    pub const fn new(max_rows: usize, max_columns: usize, max_cells: usize) -> Self {
        Self {
            max_rows,
            max_columns,
            max_cells,
        }
    }
}

impl Default for TableLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_ROWS_PER_TABLE,
            DEFAULT_MAX_COLUMNS_PER_TABLE,
            DEFAULT_MAX_CELLS_PER_TABLE,
        )
    }
}

/// A named, typed field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
}

pub(crate) fn is_sql_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

pub(crate) fn is_sql_identifier_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn is_valid_sql_identifier(identifier: &str) -> bool {
    let mut characters = identifier.chars();
    characters.next().is_some_and(is_sql_identifier_start)
        && characters.all(is_sql_identifier_continue)
}

fn validate_sql_identifier(identifier: &str, context: &str) -> Result<()> {
    if is_valid_sql_identifier(identifier) {
        Ok(())
    } else {
        Err(Error::InvalidIdentifier {
            identifier: identifier.to_owned(),
            context: context.to_owned(),
        })
    }
}

pub(crate) fn validate_table_name(name: &str) -> Result<()> {
    validate_sql_identifier(name, "table name")
}

pub(crate) fn is_reserved_column_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("TRUE") || name.eq_ignore_ascii_case("FALSE")
}

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    /// Nullable `Int64` values. The logical type remains [`DataType::Int64`],
    /// while `None` is exposed as a typed SQL `NULL`.
    NullableInt64(Vec<Option<i64>>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) | Self::NullableInt64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the canonical SQL spelling for schema metadata.
    #[must_use]
    pub(crate) fn metadata_type_name(&self) -> &'static str {
        match self {
            Self::NullableInt64(_) => "Nullable(Int64)",
            _ => self.data_type().as_str(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::NullableInt64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the scalar payload bytes retained by this column without allocating.
    ///
    /// Each `Int64` and `Float64` counts as eight bytes, each `Bool` as one byte,
    /// and each `String` as its UTF-8 payload length. The result saturates at
    /// [`usize::MAX`]. Container capacity and allocation metadata are excluded.
    #[must_use]
    pub fn retained_value_bytes(&self) -> usize {
        saturating_usize(self.retained_value_bytes_exact())
    }

    fn retained_value_bytes_exact(&self) -> u128 {
        match self {
            Self::Int64(values) => (values.len() as u128).saturating_mul(8),
            Self::NullableInt64(values) => (values.len() as u128).saturating_mul(9),
            Self::Float64(values) => (values.len() as u128).saturating_mul(8),
            Self::Bool(values) => values.len() as u128,
            Self::String(values) => values
                .iter()
                .map(|value| value.len() as u128)
                .fold(0_u128, u128::saturating_add),
        }
    }

    #[must_use]
    pub fn value(&self, row: usize) -> Value {
        self.value_ref(row).to_owned()
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        match self {
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::NullableInt64(values) => {
                values[row].map_or(ValueRef::Null(DataType::Int64), ValueRef::Int64)
            }
            Self::Float64(values) => ValueRef::Float64(values[row]),
            Self::Bool(values) => ValueRef::Bool(values[row]),
            Self::String(values) => ValueRef::String(&values[row]),
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    fn push(&mut self, value: Value) -> u128 {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => {
                values.push(value);
                8
            }
            (Self::NullableInt64(values), Value::Int64(value)) => {
                values.push(Some(value));
                9
            }
            (Self::NullableInt64(values), Value::Null(DataType::Int64)) => {
                values.push(None);
                9
            }
            (Self::Float64(values), Value::Float64(value)) => {
                values.push(value);
                8
            }
            (Self::Bool(values), Value::Bool(value)) => {
                values.push(value);
                1
            }
            (Self::String(values), Value::String(value)) => {
                let value_bytes = value.len() as u128;
                values.push(value);
                value_bytes
            }
            _ => unreachable!("values are validated before insertion"),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Int64(values) => values.clear(),
            Self::NullableInt64(values) => values.clear(),
            Self::Float64(values) => values.clear(),
            Self::Bool(values) => values.clear(),
            Self::String(values) => values.clear(),
        }
    }

    fn delete_rows(&mut self, row_indexes: &[usize]) -> u128 {
        let deleted_value_bytes = match self {
            Self::Int64(_) | Self::Float64(_) => (row_indexes.len() as u128).saturating_mul(8),
            Self::NullableInt64(_) => (row_indexes.len() as u128).saturating_mul(9),
            Self::Bool(_) => row_indexes.len() as u128,
            Self::String(values) => row_indexes
                .iter()
                .map(|&row_index| values[row_index].len() as u128)
                .fold(0_u128, u128::saturating_add),
        };
        match self {
            Self::Int64(values) => compact_deleted_rows(values, row_indexes),
            Self::NullableInt64(values) => compact_deleted_rows(values, row_indexes),
            Self::Float64(values) => compact_deleted_rows(values, row_indexes),
            Self::Bool(values) => compact_deleted_rows(values, row_indexes),
            Self::String(values) => compact_deleted_rows(values, row_indexes),
        }
        deleted_value_bytes
    }

    fn replace_values(&mut self, replacements: Vec<(usize, Value)>) -> (u128, u128) {
        let mut removed_value_bytes = 0_u128;
        let mut added_value_bytes = 0_u128;
        for (row_index, value) in replacements {
            match (&mut *self, value) {
                (Self::Int64(values), Value::Int64(value)) => values[row_index] = value,
                (Self::NullableInt64(values), Value::Int64(value)) => {
                    values[row_index] = Some(value);
                }
                (Self::NullableInt64(values), Value::Null(DataType::Int64)) => {
                    values[row_index] = None;
                }
                (Self::Float64(values), Value::Float64(value)) => values[row_index] = value,
                (Self::Bool(values), Value::Bool(value)) => values[row_index] = value,
                (Self::String(values), Value::String(value)) => {
                    removed_value_bytes =
                        removed_value_bytes.saturating_add(values[row_index].len() as u128);
                    added_value_bytes = added_value_bytes.saturating_add(value.len() as u128);
                    values[row_index] = value;
                }
                _ => unreachable!("replacement values are validated before mutation"),
            }
        }
        (removed_value_bytes, added_value_bytes)
    }
}

fn saturating_usize(value: u128) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn compact_deleted_rows<T>(values: &mut Vec<T>, row_indexes: &[usize]) {
    let mut source_row = 0;
    let mut deletion_position = 0;
    values.retain(|_| {
        let delete = row_indexes.get(deletion_position) == Some(&source_row);
        source_row += 1;
        if delete {
            deletion_position += 1;
        }
        !delete
    });
    debug_assert_eq!(deletion_position, row_indexes.len());
}

/// Validated INSERT input retained without materializing omitted defaults.
pub(crate) struct PreparedInsertRows {
    rows: Vec<Vec<Value>>,
    /// Input position to physical schema position. `None` means the input is
    /// already in schema order and contains every column.
    schema_indexes: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Copy)]
struct Int64RangePartitionMetadata {
    lower_bound: i64,
    upper_bound: i64,
    row_start: usize,
    row_end: usize,
}

#[derive(Debug, Clone)]
struct Int64RangePartitionSet {
    column_index: usize,
    partitions: Vec<Int64RangePartitionMetadata>,
}

impl PreparedInsertRows {
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns the preflighted values when the target is one physical Int64 column.
    pub(crate) fn int64_values(&self) -> Option<Vec<Option<i64>>> {
        self.rows
            .iter()
            .map(|row| match row.as_slice() {
                [Value::Int64(value)] => Some(Some(*value)),
                [Value::Null(DataType::Int64)] => Some(None),
                _ => None,
            })
            .collect()
    }
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
    retained_value_bytes: u128,
    limits: TableLimits,
    mutation_generation: u64,
    int64_min_max_index: Option<Int64MinMaxIndex>,
    int64_range_partitions: Option<Int64RangePartitionSet>,
}

impl Table {
    /// Creates an empty table with finite default row, column, and cell caps.
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::with_limits(name, schema, TableLimits::default())
    }

    /// Creates an empty table with an explicit row cap and default column and cell caps.
    pub fn with_row_cap(name: String, schema: Vec<ColumnDef>, row_cap: usize) -> Result<Self> {
        Self::with_limits(
            name,
            schema,
            TableLimits {
                max_rows: row_cap,
                ..TableLimits::default()
            },
        )
    }

    /// Creates an empty table with explicit persistent resource limits.
    pub fn with_limits(name: String, schema: Vec<ColumnDef>, limits: TableLimits) -> Result<Self> {
        validate_table_name(&name)?;
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        if schema.len() > limits.max_columns {
            return Err(Error::ResourceLimitExceeded {
                resource: "table columns",
                actual: schema.len(),
                max: limits.max_columns,
            });
        }
        let mut column_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            validate_sql_identifier(&field.name, "column name")?;
            if is_reserved_column_name(&field.name) {
                return Err(Error::ReservedIdentifier {
                    identifier: field.name.clone(),
                    context: "column name".to_owned(),
                });
            }
            if !column_names.insert(field.name.to_ascii_lowercase()) {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }
        let columns = schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();
        Ok(Self {
            name,
            schema,
            columns,
            row_count: 0,
            retained_value_bytes: 0,
            limits,
            mutation_generation: 0,
            int64_min_max_index: None,
            int64_range_partitions: None,
        })
    }

    /// Builds one fully validated, non-nullable `Int64` table without
    /// materializing a temporary vector for every row.
    pub(crate) fn with_int64_values(
        name: String,
        column_name: String,
        values: Vec<i64>,
        limits: TableLimits,
    ) -> Result<Self> {
        let mut table = Self::with_limits(
            name,
            vec![ColumnDef {
                name: column_name,
                data_type: DataType::Int64,
            }],
            limits,
        )?;
        table.validate_row_capacity(values.len())?;
        table.row_count = values.len();
        table.retained_value_bytes = (values.len() as u128).saturating_mul(8);
        table.columns = vec![Column::Int64(values)];
        Ok(table)
    }

    /// Builds one fully validated nullable `Int64` table without temporary
    /// row materialization.
    pub(crate) fn with_nullable_int64_values(
        name: String,
        column_name: String,
        values: Vec<Option<i64>>,
        limits: TableLimits,
    ) -> Result<Self> {
        let mut table = Self::with_limits(
            name,
            vec![ColumnDef {
                name: column_name,
                data_type: DataType::Int64,
            }],
            limits,
        )?;
        table.validate_row_capacity(values.len())?;
        table.row_count = values.len();
        table.retained_value_bytes = (values.len() as u128).saturating_mul(9);
        table.columns = vec![Column::NullableInt64(values)];
        Ok(table)
    }

    /// Builds a fully validated one-column table and its range-pruning metadata.
    pub(crate) fn with_int64_range_partitions(
        name: String,
        column_name: String,
        partitions: Vec<Int64RangePartition>,
        partition_limits: Int64RangePartitionLimits,
        table_limits: TableLimits,
    ) -> std::result::Result<Self, Int64RangePartitionError> {
        if partitions.len() > partition_limits.max_partitions {
            return Err(Int64RangePartitionError::PartitionLimitExceeded {
                partitions: partitions.len(),
                max_partitions: partition_limits.max_partitions,
            });
        }

        let rows = partitions
            .iter()
            .map(|partition| partition.values.len())
            .fold(0_usize, usize::saturating_add);
        if rows > partition_limits.max_rows {
            return Err(Int64RangePartitionError::RowLimitExceeded {
                rows,
                max_rows: partition_limits.max_rows,
            });
        }
        let exact_bytes = (rows as u128).saturating_mul(std::mem::size_of::<i64>() as u128);
        let bytes = saturating_usize(exact_bytes);
        if exact_bytes > partition_limits.max_bytes as u128 {
            return Err(Int64RangePartitionError::ByteLimitExceeded {
                bytes,
                max_bytes: partition_limits.max_bytes,
            });
        }

        for (partition_index, partition) in partitions.iter().enumerate() {
            if partition.lower_bound > partition.upper_bound {
                return Err(Int64RangePartitionError::InvalidRange {
                    partition_index,
                    lower_bound: partition.lower_bound,
                    upper_bound: partition.upper_bound,
                });
            }
            if let Some(previous) = partition_index
                .checked_sub(1)
                .map(|index| &partitions[index])
            {
                if partition.lower_bound < previous.lower_bound {
                    return Err(Int64RangePartitionError::OutOfOrder {
                        partition_index,
                        previous_lower_bound: previous.lower_bound,
                        lower_bound: partition.lower_bound,
                    });
                }
                if partition.lower_bound <= previous.upper_bound {
                    return Err(Int64RangePartitionError::Overlap {
                        partition_index,
                        previous_upper_bound: previous.upper_bound,
                        lower_bound: partition.lower_bound,
                    });
                }
            }
            if let Some((value_index, &value)) =
                partition.values.iter().enumerate().find(|(_, value)| {
                    **value < partition.lower_bound || **value > partition.upper_bound
                })
            {
                return Err(Int64RangePartitionError::ValueOutOfRange {
                    partition_index,
                    value_index,
                    value,
                    lower_bound: partition.lower_bound,
                    upper_bound: partition.upper_bound,
                });
            }
        }

        let mut table = Self::with_limits(
            name,
            vec![ColumnDef {
                name: column_name,
                data_type: DataType::Int64,
            }],
            table_limits,
        )?;
        table.validate_row_capacity(rows)?;

        let mut values = Vec::with_capacity(rows);
        let mut metadata = Vec::with_capacity(partitions.len());
        for partition in partitions {
            let row_start = values.len();
            values.extend(partition.values);
            metadata.push(Int64RangePartitionMetadata {
                lower_bound: partition.lower_bound,
                upper_bound: partition.upper_bound,
                row_start,
                row_end: values.len(),
            });
        }
        table.row_count = rows;
        table.retained_value_bytes = exact_bytes;
        table.columns = vec![Column::Int64(values)];
        table.int64_range_partitions = Some(Int64RangePartitionSet {
            column_index: 0,
            partitions: metadata,
        });
        Ok(table)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Changes only the display name after the catalog has preflighted a rename.
    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
    }

    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub(crate) fn column_is_nullable_int64(&self, index: usize) -> bool {
        matches!(self.columns[index], Column::NullableInt64(_))
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of validated range partitions, or `None` after a
    /// mutation has invalidated pruning metadata (and for ordinary tables).
    #[must_use]
    pub fn int64_range_partition_count(&self) -> Option<usize> {
        self.int64_range_partitions
            .as_ref()
            .map(|set| set.partitions.len())
    }

    /// Narrows a direct key comparison to the contiguous physical rows in
    /// partitions that can possibly satisfy it. `None` means complete-scan
    /// fallback; an empty range means every partition was pruned.
    pub(crate) fn int64_range_partition_rows(
        &self,
        column_index: usize,
        filter: Int64Filter,
    ) -> Option<Range<usize>> {
        let set = self.int64_range_partitions.as_ref()?;
        if set.column_index != column_index {
            return None;
        }

        let mut selected = set.partitions.iter().filter(|partition| match filter {
            Int64Filter::Equal(value) => {
                partition.lower_bound <= value && value <= partition.upper_bound
            }
            Int64Filter::Less(value) => partition.lower_bound < value,
            Int64Filter::LessOrEqual(value) => partition.lower_bound <= value,
            Int64Filter::Greater(value) => partition.upper_bound > value,
            Int64Filter::GreaterOrEqual(value) => partition.upper_bound >= value,
        });
        let Some(first) = selected.next() else {
            return Some(0..0);
        };
        let mut row_end = first.row_end;
        for partition in selected {
            row_end = partition.row_end;
        }
        Some(first.row_start..row_end)
    }

    /// Returns the maximum number of rows this table can retain.
    #[must_use]
    pub fn row_cap(&self) -> usize {
        self.limits.max_rows
    }

    /// Returns all persistent resource limits for this table.
    #[must_use]
    pub const fn limits(&self) -> TableLimits {
        self.limits
    }

    /// Returns the maximum number of physical columns this table can retain.
    #[must_use]
    pub const fn column_cap(&self) -> usize {
        self.limits.max_columns
    }

    /// Returns the maximum number of physical scalar cells this table can retain.
    #[must_use]
    pub const fn cell_cap(&self) -> usize {
        self.limits.max_cells
    }

    /// Returns the number of physical scalar cells currently retained.
    #[must_use]
    pub fn retained_cell_count(&self) -> usize {
        self.row_count.saturating_mul(self.schema.len())
    }

    /// Returns cached scalar payload bytes retained across all columns in constant time.
    ///
    /// The total is maintained during mutations and saturates at [`usize::MAX`].
    /// Container capacity, schema text, and allocation metadata are excluded.
    #[must_use]
    pub fn retained_value_bytes(&self) -> usize {
        saturating_usize(self.retained_value_bytes)
    }

    pub(crate) const fn retained_value_bytes_exact(&self) -> u128 {
        self.retained_value_bytes
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.schema
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Returns metadata for this table's optional sparse `Int64` index.
    #[must_use]
    pub fn int64_min_max_index_info(&self) -> Option<Int64MinMaxIndexInfo> {
        let index = self.int64_min_max_index.as_ref()?;
        let column = self.schema.get(index.column)?;
        Some(Int64MinMaxIndexInfo {
            column: column.name.clone(),
            block_rows: index.limits.block_rows,
            block_count: index.blocks.len(),
            indexed_rows: index.indexed_rows,
            retained_bytes: index
                .blocks
                .len()
                .saturating_mul(size_of::<Int64MinMaxBlockMetadata>()),
        })
    }

    /// Returns immutable block summaries for this table's optional sparse index.
    #[must_use]
    pub fn int64_min_max_index_blocks(&self) -> Option<&[Int64MinMaxBlockMetadata]> {
        self.int64_min_max_index
            .as_ref()
            .map(|index| index.blocks.as_slice())
    }

    pub(crate) fn try_create_int64_min_max_index(
        &mut self,
        column: &str,
        limits: Int64MinMaxIndexLimits,
    ) -> Result<Int64MinMaxIndexAdmission> {
        let column = self.column_index(column)?;
        if self.schema[column].data_type != DataType::Int64 {
            return Err(Error::TypeMismatch {
                context: format!(
                    "sparse min-max index column '{}.{}'",
                    self.name, self.schema[column].name
                ),
                expected: DataType::Int64.to_string(),
                actual: self.schema[column].data_type.to_string(),
            });
        }
        if limits.block_rows == 0 {
            return Ok(Int64MinMaxIndexAdmission::Rejected(
                Int64MinMaxIndexRejection::ZeroBlockRows,
            ));
        }

        let index = match self.build_int64_min_max_index(column, limits) {
            Ok(index) => index,
            Err(rejection) => return Ok(Int64MinMaxIndexAdmission::Rejected(rejection)),
        };
        self.int64_min_max_index = Some(index);
        Ok(Int64MinMaxIndexAdmission::Created(
            self.int64_min_max_index_info()
                .expect("the admitted index has a valid schema column"),
        ))
    }

    pub(crate) fn drop_int64_min_max_index(&mut self) -> bool {
        self.int64_min_max_index.take().is_some()
    }

    pub(crate) fn has_int64_min_max_index(&self) -> bool {
        self.int64_min_max_index.is_some()
    }

    pub(crate) fn int64_min_max_index_scan(
        &self,
        column: usize,
        filter: Int64Filter,
        source_rows: Range<usize>,
    ) -> Option<Int64MinMaxIndexScan> {
        self.int64_min_max_index_scan_with(column, source_rows, |block| {
            block_may_match(block, filter)
        })
    }

    pub(crate) fn int64_min_max_nullness_index_scan(
        &self,
        column: usize,
        is_null: bool,
        source_rows: Range<usize>,
    ) -> Option<Int64MinMaxIndexScan> {
        self.int64_min_max_index_scan_with(column, source_rows, |block| {
            block_may_match_nullness(block, is_null)
        })
    }

    fn int64_min_max_index_scan_with(
        &self,
        column: usize,
        source_rows: Range<usize>,
        block_may_match: impl Fn(Int64MinMaxBlockMetadata) -> bool,
    ) -> Option<Int64MinMaxIndexScan> {
        let index = self.int64_min_max_index.as_ref()?;
        if index.column != column
            || index.source_generation != self.mutation_generation
            || index.indexed_rows != self.row_count
            || index.limits.block_rows == 0
            || !index_has_valid_layout(index)
        {
            return None;
        }
        debug_assert!(source_rows.start <= source_rows.end);
        debug_assert!(source_rows.end <= self.row_count);

        let mut ranges = Vec::with_capacity(index.blocks.len());
        let mut scanned_blocks = 0_usize;
        let mut pruned_blocks = 0_usize;
        for block in &index.blocks {
            let start = block.first_row.max(source_rows.start);
            let end = block
                .first_row
                .saturating_add(block.row_count)
                .min(source_rows.end);
            if start >= end {
                continue;
            }
            if block_may_match(*block) {
                scanned_blocks = scanned_blocks.saturating_add(1);
                ranges.push(start..end);
            } else {
                pruned_blocks = pruned_blocks.saturating_add(1);
            }
        }
        Some(Int64MinMaxIndexScan {
            scanned_blocks,
            pruned_blocks,
            ranges,
        })
    }

    fn build_int64_min_max_index(
        &self,
        column: usize,
        limits: Int64MinMaxIndexLimits,
    ) -> std::result::Result<Int64MinMaxIndex, Int64MinMaxIndexRejection> {
        debug_assert!(limits.block_rows != 0);
        let required_blocks = self.row_count.div_ceil(limits.block_rows);
        if required_blocks > limits.max_blocks {
            return Err(Int64MinMaxIndexRejection::BlockLimitExceeded {
                required: required_blocks,
                max: limits.max_blocks,
            });
        }
        let required_bytes = required_blocks.saturating_mul(size_of::<Int64MinMaxBlockMetadata>());
        if required_bytes > limits.max_bytes {
            return Err(Int64MinMaxIndexRejection::ByteLimitExceeded {
                required: required_bytes,
                max: limits.max_bytes,
            });
        }

        let mut blocks = Vec::with_capacity(required_blocks);
        match &self.columns[column] {
            Column::Int64(values) => {
                for (block_number, values) in values.chunks(limits.block_rows).enumerate() {
                    blocks.push(summarize_nullable_int64_block(
                        block_number.saturating_mul(limits.block_rows),
                        values.iter().copied().map(Some),
                    ));
                }
            }
            Column::NullableInt64(values) => {
                for (block_number, values) in values.chunks(limits.block_rows).enumerate() {
                    blocks.push(summarize_nullable_int64_block(
                        block_number.saturating_mul(limits.block_rows),
                        values.iter().copied(),
                    ));
                }
            }
            _ => unreachable!("the index column type was validated"),
        }
        Ok(Int64MinMaxIndex {
            column,
            limits,
            indexed_rows: self.row_count,
            source_generation: self.mutation_generation,
            blocks,
        })
    }

    fn mark_values_mutated(&mut self) {
        self.int64_range_partitions = None;
        self.mutation_generation = self.mutation_generation.wrapping_add(1);
        let Some(previous) = self.int64_min_max_index.take() else {
            return;
        };
        self.int64_min_max_index = self
            .build_int64_min_max_index(previous.column, previous.limits)
            .ok();
    }

    /// Resolves and validates a nonempty explicit INSERT column list without
    /// materializing omitted defaults. Positional rows retain schema order.
    ///
    /// Explicit names resolve case-insensitively. Unknown and duplicate names
    /// are rejected. Omitted columns receive their physical default when the
    /// prepared rows are committed: `NULL` for `Nullable(Int64)`, otherwise
    /// `0`, `0.0`, `false`, or an empty String. After column and row-width
    /// validation, `capacity_rows` is checked before supplied values are
    /// type-checked in schema order. Atomic callers pass the cumulative rows
    /// for this table.
    pub(crate) fn prepare_insert_rows(
        &self,
        insert_columns: Option<&[String]>,
        rows: Vec<Vec<Value>>,
        capacity_rows: usize,
    ) -> Result<PreparedInsertRows> {
        let Some(insert_columns) = insert_columns else {
            self.validate_row_capacity(capacity_rows)?;
            for row in &rows {
                self.validate_row(row)?;
            }
            return Ok(PreparedInsertRows {
                rows,
                schema_indexes: None,
            });
        };

        if insert_columns.is_empty() {
            return Err(Error::MissingInsertColumn {
                table: self.name.clone(),
                column: self.schema[0].name.clone(),
            });
        }

        let schema_indexes_by_name = self
            .schema
            .iter()
            .enumerate()
            .map(|(index, field)| (field.name.to_ascii_lowercase(), index))
            .collect::<HashMap<_, _>>();
        let mut schema_indexes = Vec::with_capacity(insert_columns.len());
        let mut seen = vec![false; self.schema.len()];
        for name in insert_columns {
            let Some(&schema_index) = schema_indexes_by_name.get(&name.to_ascii_lowercase()) else {
                return Err(Error::ColumnNotFound {
                    table: self.name.clone(),
                    column: name.clone(),
                });
            };
            if std::mem::replace(&mut seen[schema_index], true) {
                return Err(Error::DuplicateColumn(name.clone()));
            }
            schema_indexes.push(schema_index);
        }

        for row in &rows {
            if row.len() != insert_columns.len() {
                return Err(Error::RowLength {
                    table: self.name.clone(),
                    expected: insert_columns.len(),
                    actual: row.len(),
                });
            }
        }

        self.prepare_projected_rows_with_capacity(schema_indexes, rows, capacity_rows)
    }

    /// Validates projected input and retains omitted columns for defaulting at commit.
    ///
    /// `schema_indexes` maps each input field to its physical schema column.
    /// Callers must resolve a unique, nonempty set of indexes before calling
    /// this helper. Supplied values are validated in physical schema order,
    /// independent of their input order.
    pub(crate) fn prepare_projected_rows(
        &self,
        schema_indexes: Vec<usize>,
        rows: Vec<Vec<Value>>,
    ) -> Result<PreparedInsertRows> {
        let capacity_rows = rows.len();
        self.prepare_projected_rows_with_capacity(schema_indexes, rows, capacity_rows)
    }

    fn prepare_projected_rows_with_capacity(
        &self,
        schema_indexes: Vec<usize>,
        rows: Vec<Vec<Value>>,
        capacity_rows: usize,
    ) -> Result<PreparedInsertRows> {
        debug_assert!(!schema_indexes.is_empty());
        debug_assert!(
            schema_indexes
                .iter()
                .all(|index| *index < self.schema.len())
        );
        debug_assert!({
            let mut sorted = schema_indexes.clone();
            sorted.sort_unstable();
            sorted.dedup();
            sorted.len() == schema_indexes.len()
        });
        let mut input_indexes_by_schema = vec![None; self.schema.len()];
        for (input_index, &schema_index) in schema_indexes.iter().enumerate() {
            input_indexes_by_schema[schema_index] = Some(input_index);
        }

        for row in &rows {
            if row.len() != schema_indexes.len() {
                return Err(Error::RowLength {
                    table: self.name.clone(),
                    expected: schema_indexes.len(),
                    actual: row.len(),
                });
            }
        }

        self.validate_row_capacity(capacity_rows)?;

        for row in &rows {
            for (schema_index, (field, input_index)) in
                self.schema.iter().zip(&input_indexes_by_schema).enumerate()
            {
                if let Some(input_index) = input_index {
                    self.validate_value(schema_index, field, &row[*input_index])?;
                }
            }
        }

        let schema_indexes = (schema_indexes.len() != self.schema.len()
            || !schema_indexes.iter().copied().eq(0..self.schema.len()))
        .then_some(schema_indexes);
        Ok(PreparedInsertRows {
            rows,
            schema_indexes,
        })
    }

    /// Changes only a column's display name after validating the complete rename.
    ///
    /// Source and collision checks are case-insensitive. Renaming a column to
    /// another spelling of its own name is allowed, while invalid, reserved,
    /// and already-used destinations leave the schema unchanged.
    pub fn rename_column(&mut self, source: &str, destination: String) -> Result<()> {
        let source_index = self.column_index(source)?;
        validate_sql_identifier(&destination, "column name")?;
        if is_reserved_column_name(&destination) {
            return Err(Error::ReservedIdentifier {
                identifier: destination,
                context: "column name".to_owned(),
            });
        }
        if self.schema.iter().enumerate().any(|(index, field)| {
            index != source_index && field.name.eq_ignore_ascii_case(&destination)
        }) {
            return Err(Error::DuplicateColumn(destination));
        }

        self.schema[source_index].name = destination;
        self.int64_range_partitions = None;
        Ok(())
    }

    /// Appends one schema field and an aligned physical column of defaults.
    ///
    /// Name validation and case-insensitive collision detection complete
    /// before either the schema or physical columns are changed. Existing
    /// rows receive ClickHouse-style non-null defaults for the new type.
    pub fn add_column(&mut self, field: ColumnDef) -> Result<()> {
        self.validate_add_column(&field)?;

        let column = match field.data_type {
            DataType::Int64 => Column::Int64(vec![0; self.row_count]),
            DataType::Float64 => Column::Float64(vec![0.0; self.row_count]),
            DataType::Bool => Column::Bool(vec![false; self.row_count]),
            DataType::String => Column::String(vec![String::new(); self.row_count]),
        };
        self.publish_added_column(field, column);
        Ok(())
    }

    /// Appends one physical `Nullable(Int64)` column, backfilling existing
    /// rows with SQL `NULL`.
    pub fn add_nullable_int64_column(&mut self, name: String) -> Result<()> {
        let field = ColumnDef {
            name,
            data_type: DataType::Int64,
        };
        self.validate_add_column(&field)?;
        let column = Column::NullableInt64(vec![None; self.row_count]);
        self.publish_added_column(field, column);
        Ok(())
    }

    fn publish_added_column(&mut self, field: ColumnDef, column: Column) {
        let added_value_bytes = column.retained_value_bytes_exact();

        debug_assert_eq!(self.schema.len(), self.columns.len());
        self.schema.push(field);
        self.columns.push(column);
        self.retained_value_bytes = self.retained_value_bytes.saturating_add(added_value_bytes);
        self.mark_values_mutated();
    }

    pub(crate) fn validate_add_column(&self, field: &ColumnDef) -> Result<()> {
        validate_sql_identifier(&field.name, "column name")?;
        if is_reserved_column_name(&field.name) {
            return Err(Error::ReservedIdentifier {
                identifier: field.name.clone(),
                context: "column name".to_owned(),
            });
        }
        if self
            .schema
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&field.name))
        {
            return Err(Error::DuplicateColumn(field.name.clone()));
        }

        let column_count = self.schema.len().saturating_add(1);
        if column_count > self.limits.max_columns {
            return Err(Error::ResourceLimitExceeded {
                resource: "table columns",
                actual: column_count,
                max: self.limits.max_columns,
            });
        }
        let cell_count = self.row_count.saturating_mul(column_count);
        if cell_count > self.limits.max_cells {
            return Err(Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: cell_count,
                max: self.limits.max_cells,
            });
        }
        Ok(())
    }

    /// Removes one schema field and its aligned physical column.
    ///
    /// Resolution is case-insensitive. The column lookup and the invariant
    /// that a table retains at least one column are checked before either
    /// vector is changed.
    pub fn drop_column(&mut self, column: &str) -> Result<()> {
        let column_index = self.column_index(column)?;
        if self.schema.len() == 1 {
            return Err(Error::InvalidQuery(format!(
                "cannot drop the only column from table '{}'",
                self.name
            )));
        }

        debug_assert_eq!(self.schema.len(), self.columns.len());
        self.schema.remove(column_index);
        let removed_column = self.columns.remove(column_index);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_sub(removed_column.retained_value_bytes_exact());
        // A physical position may have shifted, so retaining any index would
        // risk associating its metadata with the wrong column.
        self.int64_min_max_index = None;
        self.int64_range_partitions = None;
        self.mutation_generation = self.mutation_generation.wrapping_add(1);
        Ok(())
    }

    /// Checks a row without mutating any physical column.
    pub(crate) fn validate_row(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (index, (field, value)) in self.schema.iter().zip(row).enumerate() {
            self.validate_value(index, field, value)?;
        }

        Ok(())
    }

    fn validate_value(&self, column: usize, field: &ColumnDef, value: &Value) -> Result<()> {
        if matches!(value, Value::Null(_)) {
            if matches!(self.columns[column], Column::NullableInt64(_))
                && matches!(value, Value::Null(DataType::Int64))
            {
                return Ok(());
            }
            return Err(Error::TypeMismatch {
                context: format!("column '{}.{}'", self.name, field.name),
                expected: field.data_type.to_string(),
                actual: "NULL".to_owned(),
            });
        }
        if field.data_type != value.data_type() {
            return Err(Error::TypeMismatch {
                context: format!("column '{}.{}'", self.name, field.name),
                expected: field.data_type.to_string(),
                actual: value.data_type().to_string(),
            });
        }
        if matches!(value, Value::Float64(number) if !number.is_finite()) {
            return Err(Error::InvalidQuery(format!(
                "column '{}.{}' cannot store a non-finite Float64",
                self.name, field.name
            )));
        }
        Ok(())
    }

    /// Validates the row cap and complete row before appending to any column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row_capacity(1)?;
        self.validate_row(&row)?;
        self.append_validated_row(row);
        self.mark_values_mutated();
        Ok(())
    }

    /// Atomically validates and appends a complete batch of rows.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.validate_row_capacity(rows.len())?;
        for row in &rows {
            self.validate_row(row)?;
        }
        self.append_validated_rows(rows);
        Ok(())
    }

    pub(crate) fn validate_row_capacity(&self, incoming_rows: usize) -> Result<()> {
        if incoming_rows > self.limits.max_rows.saturating_sub(self.row_count) {
            return Err(Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: self.row_count.saturating_add(incoming_rows),
                max: self.limits.max_rows,
            });
        }
        let row_count = self.row_count.saturating_add(incoming_rows);
        let cell_count = row_count.saturating_mul(self.schema.len());
        if cell_count > self.limits.max_cells {
            return Err(Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: cell_count,
                max: self.limits.max_cells,
            });
        }
        Ok(())
    }

    pub(crate) fn append_validated_rows(&mut self, rows: Vec<Vec<Value>>) {
        let changed = !rows.is_empty();
        for row in rows {
            self.append_validated_row(row);
        }
        if changed {
            self.mark_values_mutated();
        }
    }

    /// Commits preflighted input, materializing at most one defaulted row at a time.
    pub(crate) fn append_prepared_insert_rows(&mut self, prepared: PreparedInsertRows) {
        let PreparedInsertRows {
            rows,
            schema_indexes,
        } = prepared;
        let Some(schema_indexes) = schema_indexes else {
            self.append_validated_rows(rows);
            return;
        };

        let changed = !rows.is_empty();
        for source in rows {
            let mut row = self
                .schema
                .iter()
                .zip(&self.columns)
                .map(|(_, column)| match column {
                    Column::NullableInt64(_) => Value::Null(DataType::Int64),
                    Column::Int64(_) => Value::Int64(0),
                    Column::Float64(_) => Value::Float64(0.0),
                    Column::Bool(_) => Value::Bool(false),
                    Column::String(_) => Value::String(String::new()),
                })
                .collect::<Vec<_>>();
            for (value, schema_index) in source.into_iter().zip(schema_indexes.iter().copied()) {
                row[schema_index] = value;
            }
            self.append_validated_row(row);
        }
        if changed {
            self.mark_values_mutated();
        }
    }

    fn append_validated_row(&mut self, row: Vec<Value>) {
        let mut added_value_bytes = 0_u128;
        for (column, value) in self.columns.iter_mut().zip(row) {
            added_value_bytes = added_value_bytes.saturating_add(column.push(value));
        }
        self.row_count += 1;
        self.retained_value_bytes = self.retained_value_bytes.saturating_add(added_value_bytes);
    }

    /// Replaces selected values in one column and returns the number replaced.
    ///
    /// The column name is resolved case-insensitively. `replacements` must be
    /// unique and strictly increasing by row index, and every index must be
    /// less than the row count at the start of this call. Every replacement
    /// must have the column's physical type; `NULL` is accepted only by a
    /// physical `Nullable(Int64)` column, and non-finite `Float64` values are
    /// rejected. The column, complete index selection, and all values are
    /// validated before mutation, so an error leaves the entire table
    /// unchanged. Valid owned values are moved into the selected cells without
    /// cloning. All other cells and persistent table metadata are preserved; a
    /// nonempty replacement invalidates optional range-pruning metadata so
    /// subsequent queries use the complete scan path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnNotFound`] for an unknown column,
    /// [`Error::SelectionIndexOutOfBounds`] for an index outside the table,
    /// [`Error::SelectionNotStrictlyIncreasing`] for a duplicate or decreasing
    /// index, [`Error::TypeMismatch`] for `NULL` in a non-nullable column or a
    /// different physical type, or [`Error::InvalidQuery`] for a non-finite
    /// `Float64` value.
    pub fn replace_column_values(
        &mut self,
        column: &str,
        replacements: Vec<(usize, Value)>,
    ) -> Result<usize> {
        let column_index = self.column_index(column)?;
        validate_row_selection(
            replacements.iter().map(|(row_index, _)| *row_index),
            self.row_count,
        )?;

        let field = &self.schema[column_index];
        for (_, value) in &replacements {
            self.validate_value(column_index, field, value)?;
        }

        let replaced = replacements.len();
        let (removed_value_bytes, added_value_bytes) =
            self.columns[column_index].replace_values(replacements);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_sub(removed_value_bytes)
            .saturating_add(added_value_bytes);
        if replaced != 0 {
            self.mark_values_mutated();
        }
        Ok(replaced)
    }

    /// Deletes selected source rows and returns the number deleted.
    ///
    /// `row_indexes` must be unique and strictly increasing, and every index
    /// must be less than the row count at the start of this call. The complete
    /// selection is validated before any typed column is compacted, so an
    /// error leaves the table unchanged. Survivors retain their source order;
    /// the table's name, schema, and resource limits are unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SelectionIndexOutOfBounds`] for an index outside the
    /// original table or [`Error::SelectionNotStrictlyIncreasing`] for a
    /// duplicate or decreasing index.
    pub fn delete_rows(&mut self, row_indexes: &[usize]) -> Result<usize> {
        validate_row_selection(row_indexes.iter().copied(), self.row_count)?;
        if row_indexes.is_empty() {
            return Ok(0);
        }

        let mut deleted_value_bytes = 0_u128;
        for column in &mut self.columns {
            debug_assert_eq!(column.len(), self.row_count);
            deleted_value_bytes =
                deleted_value_bytes.saturating_add(column.delete_rows(row_indexes));
        }
        self.row_count -= row_indexes.len();
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_sub(deleted_value_bytes);
        self.mark_values_mutated();
        Ok(row_indexes.len())
    }

    /// Removes every row while retaining the table name, schema, and physical columns.
    pub fn truncate(&mut self) -> usize {
        let removed_rows = self.row_count;
        for column in &mut self.columns {
            column.clear();
        }
        self.row_count = 0;
        self.retained_value_bytes = 0;
        if removed_rows != 0 {
            self.mark_values_mutated();
        }
        removed_rows
    }
}

fn summarize_nullable_int64_block(
    first_row: usize,
    values: impl IntoIterator<Item = Option<i64>>,
) -> Int64MinMaxBlockMetadata {
    let mut row_count = 0_usize;
    let mut null_count = 0_usize;
    let mut min = None::<i64>;
    let mut max = None::<i64>;
    for value in values {
        row_count = row_count.saturating_add(1);
        if let Some(value) = value {
            min = Some(min.map_or(value, |current| current.min(value)));
            max = Some(max.map_or(value, |current| current.max(value)));
        } else {
            null_count = null_count.saturating_add(1);
        }
    }
    Int64MinMaxBlockMetadata {
        first_row,
        row_count,
        min,
        max,
        null_count,
    }
}

fn index_has_valid_layout(index: &Int64MinMaxIndex) -> bool {
    let expected_blocks = index.indexed_rows.div_ceil(index.limits.block_rows);
    if index.blocks.len() != expected_blocks {
        return false;
    }
    index.blocks.iter().enumerate().all(|(position, block)| {
        let first_row = position.saturating_mul(index.limits.block_rows);
        let expected_rows = index
            .indexed_rows
            .saturating_sub(first_row)
            .min(index.limits.block_rows);
        block.first_row == first_row
            && block.row_count == expected_rows
            && block.null_count <= block.row_count
            && (block.min.is_some() == block.max.is_some())
            && if block.min.is_some() {
                block.null_count < block.row_count
            } else {
                block.null_count == block.row_count
            }
            && block.min.zip(block.max).is_none_or(|(min, max)| min <= max)
    })
}

fn block_may_match(block: Int64MinMaxBlockMetadata, filter: Int64Filter) -> bool {
    let (Some(min), Some(max)) = (block.min, block.max) else {
        return false;
    };
    match filter {
        Int64Filter::Equal(value) => min <= value && value <= max,
        Int64Filter::Less(value) => min < value,
        Int64Filter::LessOrEqual(value) => min <= value,
        Int64Filter::Greater(value) => max > value,
        Int64Filter::GreaterOrEqual(value) => max >= value,
    }
}

fn block_may_match_nullness(block: Int64MinMaxBlockMetadata, is_null: bool) -> bool {
    if is_null {
        block.null_count != 0
    } else {
        block.null_count < block.row_count
    }
}

pub(crate) fn validate_row_selection(
    row_indexes: impl IntoIterator<Item = usize>,
    input_rows: usize,
) -> Result<()> {
    let mut previous = None;
    for (selection_position, row_index) in row_indexes.into_iter().enumerate() {
        if row_index >= input_rows {
            return Err(Error::SelectionIndexOutOfBounds {
                selection_position,
                row_index,
                input_rows,
            });
        }
        if let Some(previous_row_index) = previous {
            if row_index <= previous_row_index {
                return Err(Error::SelectionNotStrictlyIncreasing {
                    selection_position,
                    previous_row_index,
                    row_index,
                });
            }
        }
        previous = Some(row_index);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table() -> Table {
        Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid schema")
    }

    #[test]
    fn stores_values_in_typed_columns() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");

        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[7]));
        assert!(matches!(&table.columns()[1], Column::String(v) if v == &["ok"]));
    }

    #[test]
    fn retained_value_bytes_counts_every_type_and_tracks_row_removal() {
        assert_eq!(Column::Int64(vec![1, 2]).retained_value_bytes(), 16);
        assert_eq!(Column::Float64(vec![1.0, 2.0]).retained_value_bytes(), 16);
        assert_eq!(Column::Bool(vec![true, false]).retained_value_bytes(), 2);
        assert_eq!(
            Column::String(vec!["ASCII".to_owned(), "é".to_owned()]).retained_value_bytes(),
            7
        );

        let mut table = Table::new(
            "metrics".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "score".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDef {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid schema");
        assert_eq!(table.retained_value_bytes(), 0);

        table
            .insert_rows(vec![
                vec![
                    Value::Int64(1),
                    Value::Float64(1.5),
                    Value::Bool(true),
                    Value::String("é".to_owned()),
                ],
                vec![
                    Value::Int64(2),
                    Value::Float64(2.5),
                    Value::Bool(false),
                    Value::String("rust".to_owned()),
                ],
            ])
            .expect("valid rows");
        assert_eq!(table.retained_value_bytes(), 40);

        table.delete_rows(&[0]).expect("valid deletion");
        assert_eq!(table.retained_value_bytes(), 21);

        assert_eq!(table.truncate(), 1);
        assert_eq!(table.retained_value_bytes(), 0);
    }

    #[test]
    fn rejected_rows_do_not_partially_mutate_columns() {
        let mut table = test_table();
        let error = table
            .insert_row(vec![Value::Int64(7), Value::Bool(true)])
            .expect_err("wrong type");

        assert!(matches!(error, Error::TypeMismatch { .. }));
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().iter().all(Column::is_empty));
    }

    #[test]
    fn rejected_row_batch_does_not_mutate_at_the_row_cap() {
        let mut table = Table::with_row_cap(
            "events".to_owned(),
            vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
            2,
        )
        .expect("valid schema");
        table
            .insert_row(vec![Value::Int64(1)])
            .expect("first row fits");

        assert_eq!(
            table.insert_rows(vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]),
            Err(Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 3,
                max: 2,
            })
        );
        assert_eq!(table.row_count(), 1);
        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[1]));
    }

    #[test]
    fn nullable_block_metadata_distinguishes_all_null_mixed_and_present_blocks() {
        assert_eq!(
            summarize_nullable_int64_block(0, [None, None, None]),
            Int64MinMaxBlockMetadata {
                first_row: 0,
                row_count: 3,
                min: None,
                max: None,
                null_count: 3,
            }
        );
        assert_eq!(
            summarize_nullable_int64_block(3, [Some(7), None, Some(-2), Some(7)]),
            Int64MinMaxBlockMetadata {
                first_row: 3,
                row_count: 4,
                min: Some(-2),
                max: Some(7),
                null_count: 1,
            }
        );
        assert!(!block_may_match(
            summarize_nullable_int64_block(0, [None, None]),
            Int64Filter::Equal(0),
        ));
        assert!(!block_may_match_nullness(
            summarize_nullable_int64_block(0, [Some(1), Some(2)]),
            true,
        ));
        assert!(!block_may_match_nullness(
            summarize_nullable_int64_block(0, [None, None]),
            false,
        ));
        assert!(block_may_match_nullness(
            summarize_nullable_int64_block(0, [Some(1), None]),
            true,
        ));
        assert!(block_may_match_nullness(
            summarize_nullable_int64_block(0, [Some(1), None]),
            false,
        ));
    }

    #[test]
    fn stale_generation_and_malformed_layout_fall_back_without_pruning() {
        let mut table = test_table();
        table
            .insert_rows(vec![
                vec![Value::Int64(1), Value::String("a".to_owned())],
                vec![Value::Int64(2), Value::String("b".to_owned())],
            ])
            .unwrap();
        assert!(matches!(
            table
                .try_create_int64_min_max_index(
                    "id",
                    Int64MinMaxIndexLimits::new(1, 2, usize::MAX),
                )
                .unwrap(),
            Int64MinMaxIndexAdmission::Created(_)
        ));
        assert!(
            table
                .int64_min_max_index_scan(0, Int64Filter::Equal(2), 0..table.row_count())
                .is_some()
        );
        assert!(
            table
                .int64_min_max_nullness_index_scan(0, true, 0..table.row_count())
                .is_some()
        );

        table.mutation_generation = table.mutation_generation.wrapping_add(1);
        assert!(
            table
                .int64_min_max_index_scan(0, Int64Filter::Equal(2), 0..table.row_count())
                .is_none()
        );
        assert!(
            table
                .int64_min_max_nullness_index_scan(0, true, 0..table.row_count())
                .is_none()
        );
        table.mutation_generation = table.mutation_generation.wrapping_sub(1);
        table.int64_min_max_index.as_mut().unwrap().blocks[1].first_row = 0;
        assert!(
            table
                .int64_min_max_index_scan(0, Int64Filter::Equal(2), 0..table.row_count())
                .is_none()
        );
    }
}
