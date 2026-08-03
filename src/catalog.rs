//! In-memory ownership and `CREATE TABLE`/`INSERT`/`SELECT` execution.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::grouping::{GroupedCount, GroupedCountError};
use crate::reduction::ReductionError;
use crate::snapshot::SnapshotStore;
use crate::sql::{
    AggregateFunction, AggregateProjection, ComparisonPredicate, CreateTableStatement,
    InsertParseLimits, InsertStatement, OrderByClause, OrderDirection, ParseError, ParseLimits,
    SelectParseLimits, SelectProjection, SelectStatement, parse_create_table_with_limits,
    parse_insert_with_limits, parse_select_with_limits,
};
use crate::storage::{Column, DEFAULT_ROW_LIMIT, DataType, Field, Table, TableError, Value};
use crate::table_snapshot::TableSnapshotError;
use crate::{RowSelection, ScanError};

/// Default maximum number of tables owned by one [`Catalog`].
pub const DEFAULT_MAX_TABLES: usize = 1024;
/// Default maximum number of distinct groups produced by one grouped query.
pub const DEFAULT_MAX_GROUPS: usize = 100_000;
/// Default maximum retained String payload bytes in one grouped result.
pub const DEFAULT_MAX_GROUPED_RESULT_BYTES: usize = 1024 * 1024;
/// Maximum retained String payload bytes in one scalar aggregate row.
pub const MAX_AGGREGATE_RESULT_BYTES: usize = 1024 * 1024;

/// Resource limits applied by a [`Catalog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogLimits {
    /// Limits applied while parsing each `CREATE TABLE` statement.
    pub parse: ParseLimits,
    /// Limits applied while parsing each `INSERT` statement.
    pub insert_parse: InsertParseLimits,
    /// Limits applied while parsing each `SELECT` statement.
    pub select_parse: SelectParseLimits,
    /// Maximum number of tables the catalog may own.
    pub max_tables: usize,
    /// Maximum number of rows accepted by each newly created table.
    pub max_rows_per_table: usize,
    /// Maximum number of distinct groups produced by one `SELECT`.
    pub max_groups_per_query: usize,
    /// Maximum total String payload bytes retained by a grouped result.
    pub max_grouped_result_bytes: usize,
}

impl CatalogLimits {
    /// Creates catalog limits with the default bounded `INSERT` parser limits.
    #[must_use]
    pub const fn new(parse: ParseLimits, max_tables: usize, max_rows_per_table: usize) -> Self {
        Self {
            parse,
            insert_parse: default_insert_parse_limits(),
            select_parse: default_select_parse_limits(),
            max_tables,
            max_rows_per_table,
            max_groups_per_query: DEFAULT_MAX_GROUPS,
            max_grouped_result_bytes: DEFAULT_MAX_GROUPED_RESULT_BYTES,
        }
    }

    /// Replaces the limits applied while parsing `INSERT` statements.
    #[must_use]
    pub const fn with_insert_parse_limits(mut self, insert_parse: InsertParseLimits) -> Self {
        self.insert_parse = insert_parse;
        self
    }

    /// Replaces the limits applied while parsing `SELECT` statements.
    #[must_use]
    pub const fn with_select_parse_limits(mut self, select_parse: SelectParseLimits) -> Self {
        self.select_parse = select_parse;
        self
    }

    /// Replaces the maximum number of distinct groups produced by one query.
    #[must_use]
    pub const fn with_max_groups_per_query(mut self, max_groups_per_query: usize) -> Self {
        self.max_groups_per_query = max_groups_per_query;
        self
    }

    /// Replaces the total String payload byte limit for one grouped result.
    #[must_use]
    pub const fn with_max_grouped_result_bytes(mut self, max_grouped_result_bytes: usize) -> Self {
        self.max_grouped_result_bytes = max_grouped_result_bytes;
        self
    }
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self::new(
            ParseLimits::default(),
            DEFAULT_MAX_TABLES,
            DEFAULT_ROW_LIMIT,
        )
    }
}

/// A deterministic failure from catalog lookup or statement execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// SQL could not be parsed as the supported statement syntax.
    Parse(ParseError),
    /// A manually constructed aggregate projection list was empty.
    EmptyAggregateProjection,
    /// A manually constructed grouped count requested unsupported ordering.
    GroupedOrderingUnsupported,
    /// A manually constructed grouped count requested an unsupported row limit.
    GroupedLimitUnsupported,
    /// The catalog already owns a table with this case-insensitive name.
    DuplicateTable {
        /// Name from the rejected statement.
        name: String,
    },
    /// A requested table does not exist.
    TableNotFound {
        /// Name used for the failed lookup.
        name: String,
    },
    /// The parsed schema could not be used to construct a table.
    TableConstruction {
        /// Name from the statement whose schema was rejected.
        name: String,
        /// Storage-layer validation failure.
        source: TableError,
    },
    /// A parsed batch could not be inserted into its target table.
    TableInsertion {
        /// Name from the rejected statement.
        name: String,
        /// Storage-layer validation or capacity failure.
        source: TableError,
    },
    /// A requested projection field does not exist in the table schema.
    ProjectionFieldNotFound {
        /// Case-sensitive name used in the projection.
        name: String,
    },
    /// Memory could not be reserved for resolved projection indexes.
    ProjectionAllocationFailed {
        /// Number of projected fields whose indexes could not be reserved.
        field_count: usize,
    },
    /// Memory could not be reserved for a scalar aggregate result row.
    AggregateAllocationFailed {
        /// Number of aggregate values whose storage could not be reserved.
        aggregate_count: usize,
    },
    /// Retained String payloads in a scalar aggregate row exceed their bound.
    AggregateResultTooLarge {
        /// Name from the rejected statement.
        name: String,
        /// Field whose next extrema value would exceed the bound.
        field: String,
        /// Maximum retained String payload bytes.
        limit: usize,
        /// Total bytes required through the rejected aggregate.
        required: usize,
    },
    /// The field named by an `ORDER BY` clause does not exist in the table.
    OrderFieldNotFound {
        /// Case-sensitive name used in the clause.
        name: String,
    },
    /// Memory could not be reserved for ordered row indexes.
    OrderAllocationFailed {
        /// Number of row indexes the executor attempted to reserve.
        row_count: usize,
    },
    /// A `WHERE` comparison could not be scanned against its target table.
    TableScan {
        /// Name from the rejected statement.
        name: String,
        /// Scan validation or allocation failure.
        source: ScanError,
    },
    /// A scalar reduction failed after its optional scan.
    TableReduction {
        /// Name from the rejected statement.
        name: String,
        /// Reduction validation failure.
        source: ReductionError,
    },
    /// A grouped count failed after its optional scan.
    TableGrouping {
        /// Name from the rejected statement.
        name: String,
        /// Grouping validation, capacity, arithmetic, or allocation failure.
        source: GroupedCountError,
    },
    /// `AVG`, `MIN`, or `MAX` received no selected rows and cannot emit `NULL`.
    EmptyAggregateInput {
        /// Name from the rejected statement.
        name: String,
        /// SQL aggregate function which received no rows.
        function: &'static str,
        /// Case-sensitive aggregate input field.
        field: String,
    },
    /// A row count could not be represented by the SQL `Int64` result type.
    CountOutOfRange {
        /// Name from the rejected statement.
        name: String,
        /// Number of rows produced by the storage reduction.
        count: usize,
    },
    /// Creating another table would exceed the configured catalog bound.
    TableLimitExceeded {
        /// Maximum number of tables allowed in the catalog.
        limit: usize,
    },
    /// Memory could not be reserved for another catalog entry.
    AllocationFailed,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(formatter),
            Self::EmptyAggregateProjection => {
                formatter.write_str("aggregate projection list cannot be empty")
            }
            Self::GroupedOrderingUnsupported => {
                formatter.write_str("ORDER BY is not supported for grouped counts")
            }
            Self::GroupedLimitUnsupported => {
                formatter.write_str("LIMIT is not supported for grouped counts")
            }
            Self::DuplicateTable { name } => write!(formatter, "table `{name}` already exists"),
            Self::TableNotFound { name } => write!(formatter, "table `{name}` does not exist"),
            Self::TableConstruction { name, source } => {
                write!(formatter, "could not construct table `{name}`: {source}")
            }
            Self::TableInsertion { name, source } => {
                write!(formatter, "could not insert into table `{name}`: {source}")
            }
            Self::ProjectionFieldNotFound { name } => {
                write!(formatter, "projected field `{name}` does not exist")
            }
            Self::ProjectionAllocationFailed { field_count } => write!(
                formatter,
                "could not reserve indexes for {field_count} projected fields"
            ),
            Self::AggregateAllocationFailed { aggregate_count } => write!(
                formatter,
                "could not reserve a result row for {aggregate_count} scalar aggregates"
            ),
            Self::AggregateResultTooLarge {
                name,
                field,
                limit,
                required,
            } => write!(
                formatter,
                "aggregate result for table `{name}` requires {required} String payload bytes at field `{field}`; limit is {limit}"
            ),
            Self::OrderFieldNotFound { name } => {
                write!(formatter, "ordered field `{name}` does not exist")
            }
            Self::OrderAllocationFailed { row_count } => {
                write!(
                    formatter,
                    "could not reserve indexes for {row_count} ordered rows"
                )
            }
            Self::TableScan { name, source } => {
                write!(formatter, "could not scan table `{name}`: {source}")
            }
            Self::TableReduction { name, source } => {
                write!(formatter, "could not reduce table `{name}`: {source}")
            }
            Self::TableGrouping { name, source } => {
                write!(formatter, "could not group table `{name}`: {source}")
            }
            Self::EmptyAggregateInput {
                name,
                function,
                field,
            } => write!(
                formatter,
                "cannot compute {function}(`{field}`) for table `{name}` with no selected rows"
            ),
            Self::CountOutOfRange { name, count } => write!(
                formatter,
                "table `{name}` count of {count} cannot be represented as Int64"
            ),
            Self::TableLimitExceeded { limit } => {
                write!(formatter, "catalog table count exceeds limit of {limit}")
            }
            Self::AllocationFailed => {
                formatter.write_str("could not reserve memory for another catalog table")
            }
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::TableConstruction { source, .. } | Self::TableInsertion { source, .. } => {
                Some(source)
            }
            Self::TableScan { source, .. } => Some(source),
            Self::TableReduction { source, .. } => Some(source),
            Self::TableGrouping { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<ParseError> for CatalogError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// A typed failure while saving or loading one catalog-owned table snapshot.
#[derive(Debug)]
pub enum CatalogSnapshotError {
    /// Catalog lookup, duplicate-name, table-count, or allocation failure.
    Catalog(CatalogError),
    /// Snapshot encoding, filesystem, integrity, or decoding failure.
    Snapshot(TableSnapshotError),
}

impl fmt::Display for CatalogSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => error.fmt(formatter),
            Self::Snapshot(error) => error.fmt(formatter),
        }
    }
}

impl Error for CatalogSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::Snapshot(error) => Some(error),
        }
    }
}

impl From<CatalogError> for CatalogSnapshotError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<TableSnapshotError> for CatalogSnapshotError {
    fn from(error: TableSnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

#[derive(Debug)]
struct CatalogEntry {
    name: String,
    table: Table,
}

#[derive(Debug)]
struct ScalarResult {
    field: Field,
    value: Value,
}

#[derive(Debug)]
struct GroupedResult {
    fields: [Field; 2],
    groups: Vec<GroupedCount>,
}

/// The output of one projection, scalar aggregate, or grouped count and
/// optional comparison scans.
///
/// A row projection owns only projected column indexes and, for a filtered
/// query, a compact row-selection bitmap. Ordered results instead own their
/// bounded row indexes. Schema and column values remain owned by the catalog's
/// source table. Scalar aggregates own their fields and single result row;
/// grouped counts own their output fields and sorted key/count rows.
#[derive(Debug)]
pub struct SelectResult<'a> {
    table: &'a Table,
    field_indices: Vec<usize>,
    selection: Option<RowSelection>,
    ordered_rows: Option<Vec<usize>>,
    row_end: usize,
    row_count: usize,
    scalars: Vec<ScalarResult>,
    grouped: Option<GroupedResult>,
}

impl<'a> SelectResult<'a> {
    /// Returns the table whose schema and column values back this result.
    #[must_use]
    pub const fn table(&self) -> &'a Table {
        self.table
    }

    /// Iterates over projected fields in statement order.
    pub fn projected_fields(
        &self,
    ) -> impl ExactSizeIterator<Item = &Field> + DoubleEndedIterator + '_ {
        let projected_count = self.field_indices.len();
        let grouped_fields = self
            .grouped
            .as_ref()
            .map_or(&[][..], |grouped| grouped.fields.as_slice());
        let scalar_count = self.scalars.len();
        (0..projected_count + scalar_count + grouped_fields.len()).map(move |index| {
            if index < projected_count {
                &self.table.fields()[self.field_indices[index]]
            } else if index < projected_count + scalar_count {
                &self.scalars[index - projected_count].field
            } else {
                &grouped_fields[index - projected_count - scalar_count]
            }
        })
    }

    /// Alias for [`Self::projected_fields`].
    pub fn fields(&self) -> impl ExactSizeIterator<Item = &Field> + DoubleEndedIterator + '_ {
        self.projected_fields()
    }

    pub(crate) fn projected_columns(
        &self,
    ) -> impl ExactSizeIterator<Item = &Column> + DoubleEndedIterator + '_ {
        self.field_indices
            .iter()
            .map(|index| &self.table.columns()[*index])
    }

    /// Iterates over selected zero-based indexes in result order.
    ///
    /// Projection row indexes are also source table indexes. A scalar
    /// aggregate has exactly one result row at index zero. Grouped result
    /// indexes address the owned, deterministically ordered rows.
    pub fn selected_rows(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        SelectedRows::new(self)
    }

    /// Iterates over the values in a scalar aggregate result row.
    ///
    /// Row projections and scalar rows suppressed by `LIMIT 0` are empty.
    pub fn scalar_values(
        &self,
    ) -> impl ExactSizeIterator<Item = &Value> + DoubleEndedIterator + '_ {
        let scalars = if self.row_count == 0 {
            &self.scalars[..0]
        } else {
            &self.scalars[..]
        };
        scalars.iter().map(|scalar| &scalar.value)
    }

    /// Returns the first value for a scalar aggregate result.
    ///
    /// Row projections and scalar rows suppressed by `LIMIT 0` return `None`.
    #[must_use]
    pub const fn scalar_value(&self) -> Option<&Value> {
        match (self.scalars.as_slice(), self.row_count) {
            (_, 0) => None,
            ([scalar, ..], _) => Some(&scalar.value),
            ([], _) => None,
        }
    }

    pub(crate) fn is_scalar(&self) -> bool {
        !self.scalars.is_empty()
    }

    pub(crate) fn is_grouped(&self) -> bool {
        self.grouped.is_some()
    }

    /// Iterates over owned key/count rows for a grouped count result.
    ///
    /// Other result shapes return an empty iterator. Counts have already been
    /// validated as representable by the SQL `Int64` result type.
    pub fn grouped_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Value, i64)> + DoubleEndedIterator + '_ {
        let groups = self
            .grouped
            .as_ref()
            .map_or(&[][..], |grouped| grouped.groups.as_slice());
        groups.iter().map(|group| {
            let count = i64::try_from(group.count())
                .expect("group counts are validated before SelectResult construction");
            (group.value(), count)
        })
    }

    /// Alias for [`Self::selected_rows`].
    pub fn row_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.selected_rows()
    }

    /// Returns the number of output rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether the result has no output rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct SelectedRows<'a> {
    source: SelectedRowSource<'a>,
}

enum SelectedRowSource<'a> {
    Ordered(std::slice::Iter<'a, usize>),
    Natural {
        rows: std::ops::Range<usize>,
        selection: Option<&'a RowSelection>,
    },
}

impl<'a> SelectedRows<'a> {
    fn new(result: &'a SelectResult<'_>) -> Self {
        let source = match &result.ordered_rows {
            Some(rows) => SelectedRowSource::Ordered(rows.iter()),
            None => SelectedRowSource::Natural {
                rows: 0..result.row_end,
                selection: result.selection.as_ref(),
            },
        };
        Self { source }
    }
}

impl Iterator for SelectedRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            SelectedRowSource::Ordered(rows) => rows.next().copied(),
            SelectedRowSource::Natural { rows, selection } => rows.find(|row| {
                selection.is_none_or(|selection| {
                    selection
                        .get(*row)
                        .expect("selection and source table have the same row count")
                })
            }),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.source {
            SelectedRowSource::Ordered(rows) => rows.size_hint(),
            SelectedRowSource::Natural { rows, selection } => match selection {
                None => rows.size_hint(),
                Some(_) => (0, Some(rows.len())),
            },
        }
    }
}

impl DoubleEndedIterator for SelectedRows<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            SelectedRowSource::Ordered(rows) => rows.next_back().copied(),
            SelectedRowSource::Natural { rows, selection } => rows.rfind(|row| {
                selection.is_none_or(|selection| {
                    selection
                        .get(*row)
                        .expect("selection and source table have the same row count")
                })
            }),
        }
    }
}

/// A bounded collection of named, in-memory tables.
///
/// Table names originate from unquoted SQL identifiers, so lookup and
/// duplicate detection are ASCII case-insensitive. Field names and their
/// declaration order remain exactly as written in the `CREATE TABLE`
/// statement.
#[derive(Debug)]
pub struct Catalog {
    tables: HashMap<String, CatalogEntry>,
    limits: CatalogLimits,
}

impl Catalog {
    /// Creates an empty catalog with default parser, table-count, and row limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(CatalogLimits::default())
    }

    /// Creates an empty catalog with explicit resource limits.
    #[must_use]
    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    /// Parses and executes one `CREATE TABLE` statement.
    ///
    /// Parsing, duplicate checks, table construction, and allocation complete
    /// before a new entry is inserted. Any returned error therefore leaves the
    /// set of catalog tables unchanged.
    pub fn execute_create(&mut self, input: &str) -> Result<&Table, CatalogError> {
        let statement = parse_create_table_with_limits(input, self.limits.parse)?;
        self.create_table(statement)
    }

    /// Creates a table from an already parsed statement.
    ///
    /// This is the typed execution boundary used by [`Self::execute_create`].
    /// It is also useful to callers which parse statements separately.
    pub fn create_table(
        &mut self,
        statement: CreateTableStatement,
    ) -> Result<&Table, CatalogError> {
        let name = statement.name;
        let key = normalize_table_name(&name);

        if self.tables.contains_key(&key) {
            return Err(CatalogError::DuplicateTable { name });
        }
        if self.tables.len() == self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.limits.max_tables,
            });
        }

        let fields = statement
            .columns
            .into_iter()
            .map(|column| Field::new(column.name, column.data_type))
            .collect();
        let table =
            Table::with_row_limit(fields, self.limits.max_rows_per_table).map_err(|source| {
                CatalogError::TableConstruction {
                    name: name.clone(),
                    source,
                }
            })?;

        self.tables
            .try_reserve(1)
            .map_err(|_| CatalogError::AllocationFailed)?;
        let previous = self
            .tables
            .insert(key.clone(), CatalogEntry { name, table });
        debug_assert!(
            previous.is_none(),
            "duplicates are checked before insertion"
        );

        Ok(&self
            .tables
            .get(&key)
            .expect("the table was inserted immediately above")
            .table)
    }

    /// Saves one named catalog table in the single-table snapshot format.
    ///
    /// Lookup is ASCII case-insensitive. The snapshot store controls the
    /// maximum encoded payload size and atomically replaces `path`.
    pub fn save_table(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        snapshots: &SnapshotStore,
    ) -> Result<(), CatalogSnapshotError> {
        let table = self.table(name)?;
        snapshots.write_table(path, table)?;
        Ok(())
    }

    /// Loads one table snapshot under a caller-supplied catalog name.
    ///
    /// Names use the same ASCII case-insensitive identity as tables created
    /// through SQL. Duplicate-name and table-count checks happen before any
    /// snapshot I/O. The snapshot is fully read and decoded, and catalog
    /// allocation succeeds, before the table is inserted. Every failure before
    /// insertion therefore leaves the catalog's table set unchanged.
    pub fn load_table(
        &mut self,
        name: &str,
        path: impl AsRef<Path>,
        snapshots: &SnapshotStore,
    ) -> Result<&Table, CatalogSnapshotError> {
        let name = name.to_owned();
        let key = normalize_table_name(&name);

        if self.tables.contains_key(&key) {
            return Err(CatalogError::DuplicateTable { name }.into());
        }
        if self.tables.len() == self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.limits.max_tables,
            }
            .into());
        }

        let table = snapshots.read_table(path)?;
        self.tables
            .try_reserve(1)
            .map_err(|_| CatalogError::AllocationFailed)?;
        let previous = self
            .tables
            .insert(key.clone(), CatalogEntry { name, table });
        debug_assert!(
            previous.is_none(),
            "duplicates are checked before loading the snapshot"
        );

        Ok(&self
            .tables
            .get(&key)
            .expect("the loaded table was inserted immediately above")
            .table)
    }

    /// Parses and executes one bounded `INSERT INTO ... VALUES` statement.
    ///
    /// The complete batch is parsed and the target is resolved before storage
    /// mutation begins. [`Table::insert_batch`] then validates and commits the
    /// batch atomically, so every returned error leaves all tables unchanged.
    pub fn execute_insert(&mut self, input: &str) -> Result<usize, CatalogError> {
        let statement = parse_insert_with_limits(input, self.limits.insert_parse)?;
        self.insert(statement)
    }

    /// Inserts the rows from an already parsed statement.
    ///
    /// This is the typed execution boundary used by [`Self::execute_insert`].
    pub fn insert(&mut self, statement: InsertStatement) -> Result<usize, CatalogError> {
        let name = statement.name;
        let table = self
            .tables
            .get_mut(&normalize_table_name(&name))
            .map(|entry| &mut entry.table)
            .ok_or_else(|| CatalogError::TableNotFound { name: name.clone() })?;

        table
            .insert_batch(statement.rows)
            .map_err(|source| CatalogError::TableInsertion { name, source })
    }

    /// Parses and executes one bounded `SELECT` statement.
    ///
    /// The returned result borrows the source table and does not copy table
    /// rows. Each `WHERE` comparison is evaluated with [`Table::scan`]. Packed
    /// selections are intersected within `AND` groups and unioned across `OR`
    /// groups. For row projections, unlimited `ORDER BY` owns and sorts all
    /// selected row indexes, while `ORDER BY ... LIMIT k` retains at most `k`
    /// indexes. Scalar aggregate rows retain at most
    /// [`MAX_AGGREGATE_RESULT_BYTES`] of String payloads. Grouped results retain
    /// at most [`CatalogLimits::max_grouped_result_bytes`] of owned String key
    /// payloads. `LIMIT` is applied to the final projected or aggregate output.
    pub fn execute_select(&self, input: &str) -> Result<SelectResult<'_>, CatalogError> {
        let statement = parse_select_with_limits(input, self.limits.select_parse)?;
        self.select(statement)
    }

    /// Executes an already parsed `SELECT` statement.
    ///
    /// This is the typed execution boundary used by [`Self::execute_select`].
    pub fn select(&self, statement: SelectStatement) -> Result<SelectResult<'_>, CatalogError> {
        let SelectStatement {
            projections,
            table: name,
            predicate_groups,
            order_by,
            limit,
        } = statement;
        let table = self
            .tables
            .get(&normalize_table_name(&name))
            .map(|entry| &entry.table)
            .ok_or_else(|| CatalogError::TableNotFound { name: name.clone() })?;
        let aggregate_result_byte_limit = MAX_AGGREGATE_RESULT_BYTES;
        let max_groups = self.limits.max_groups_per_query;
        let grouped_result_byte_limit = self.limits.max_grouped_result_bytes;

        match projections {
            SelectProjection::CountAll { alias } => execute_aggregates(
                table,
                &name,
                predicate_groups,
                order_by,
                limit,
                aggregate_result_byte_limit,
                std::iter::once(AggregateProjection {
                    function: AggregateFunction::CountAll,
                    alias,
                }),
            ),
            SelectProjection::Aggregates(aggregates) => execute_aggregates(
                table,
                &name,
                predicate_groups,
                order_by,
                limit,
                aggregate_result_byte_limit,
                aggregates.into_iter(),
            ),
            SelectProjection::GroupedCount { key, alias } => {
                if order_by.is_some() {
                    return Err(CatalogError::GroupedOrderingUnsupported);
                }
                if limit.is_some() {
                    return Err(CatalogError::GroupedLimitUnsupported);
                }
                execute_grouped_count(
                    table,
                    &name,
                    predicate_groups,
                    max_groups,
                    grouped_result_byte_limit,
                    key,
                    alias,
                )
            }
            projections => {
                let field_indices = resolve_projection(table, projections)?;
                let order = order_by
                    .map(|order_by| resolve_order(table, order_by))
                    .transpose()?;
                let selection = scan_predicate_groups(table, predicate_groups, &name)?;

                let (selection, ordered_rows, row_end, row_count) = match order {
                    Some(order_keys) => {
                        let rows =
                            ordered_row_indices(table, &order_keys, selection.as_ref(), limit)?;
                        let row_count = rows.len();
                        (None, Some(rows), 0, row_count)
                    }
                    None => {
                        let (row_end, row_count) =
                            limited_row_bounds(table.len(), selection.as_ref(), limit);
                        (selection, None, row_end, row_count)
                    }
                };

                Ok(SelectResult {
                    table,
                    field_indices,
                    selection,
                    ordered_rows,
                    row_end,
                    row_count,
                    scalars: Vec::new(),
                    grouped: None,
                })
            }
        }
    }

    /// Returns a table by ASCII case-insensitive name.
    pub fn table(&self, name: &str) -> Result<&Table, CatalogError> {
        self.tables
            .get(&normalize_table_name(name))
            .map(|entry| &entry.table)
            .ok_or_else(|| CatalogError::TableNotFound {
                name: name.to_owned(),
            })
    }

    /// Returns a mutable table by ASCII case-insensitive name.
    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table, CatalogError> {
        self.tables
            .get_mut(&normalize_table_name(name))
            .map(|entry| &mut entry.table)
            .ok_or_else(|| CatalogError::TableNotFound {
                name: name.to_owned(),
            })
    }

    /// Iterates over table names with their original spelling.
    pub fn table_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tables.values().map(|entry| entry.name.as_str())
    }

    /// Returns the active resource limits.
    #[must_use]
    pub const fn limits(&self) -> CatalogLimits {
        self.limits
    }

    /// Returns the number of tables in the catalog.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Returns whether the catalog owns no tables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

fn execute_aggregates<'a>(
    table: &'a Table,
    table_name: &str,
    predicate_groups: Vec<Vec<ComparisonPredicate>>,
    order_by: Option<Vec<OrderByClause>>,
    limit: Option<usize>,
    aggregate_result_byte_limit: usize,
    aggregates: impl ExactSizeIterator<Item = AggregateProjection>,
) -> Result<SelectResult<'a>, CatalogError> {
    let aggregate_count = aggregates.len();
    if aggregate_count == 0 {
        return Err(CatalogError::EmptyAggregateProjection);
    }
    if let Some(order_by) = order_by {
        resolve_order(table, order_by)?;
    }
    let selection = scan_predicate_groups(table, predicate_groups, table_name)?;

    let mut scalars = Vec::new();
    scalars
        .try_reserve_exact(aggregate_count)
        .map_err(|_| CatalogError::AggregateAllocationFailed { aggregate_count })?;
    let mut string_budget = AggregateStringBudget::new(aggregate_result_byte_limit);
    for aggregate in aggregates {
        scalars.push(execute_aggregate(
            table,
            table_name,
            selection.as_ref(),
            aggregate,
            &mut string_budget,
        )?);
    }

    let row_count = usize::from(limit != Some(0));
    Ok(SelectResult {
        table,
        field_indices: Vec::new(),
        selection: None,
        ordered_rows: None,
        row_end: row_count,
        row_count,
        scalars,
        grouped: None,
    })
}

fn execute_grouped_count<'a>(
    table: &'a Table,
    table_name: &str,
    predicate_groups: Vec<Vec<ComparisonPredicate>>,
    max_groups: usize,
    max_string_bytes: usize,
    key: String,
    alias: Option<String>,
) -> Result<SelectResult<'a>, CatalogError> {
    let selection = scan_predicate_groups(table, predicate_groups, table_name)?;
    let groups = table
        .grouped_count_with_string_limit(&key, selection.as_ref(), max_groups, max_string_bytes)
        .map_err(|source| CatalogError::TableGrouping {
            name: table_name.to_owned(),
            source,
        })?;
    for group in &groups {
        i64::try_from(group.count()).map_err(|_| CatalogError::CountOutOfRange {
            name: table_name.to_owned(),
            count: group.count(),
        })?;
    }

    let key_type = table
        .fields()
        .iter()
        .find(|field| field.name() == key)
        .expect("grouped_count resolved the grouping field")
        .data_type();
    let row_count = groups.len();
    Ok(SelectResult {
        table,
        field_indices: Vec::new(),
        selection: None,
        ordered_rows: None,
        row_end: row_count,
        row_count,
        scalars: Vec::new(),
        grouped: Some(GroupedResult {
            fields: [
                Field::new(key, key_type),
                Field::new(
                    alias.unwrap_or_else(|| "count()".to_owned()),
                    DataType::Int64,
                ),
            ],
            groups,
        }),
    })
}

fn execute_aggregate(
    table: &Table,
    table_name: &str,
    selection: Option<&RowSelection>,
    aggregate: AggregateProjection,
    string_budget: &mut AggregateStringBudget,
) -> Result<ScalarResult, CatalogError> {
    let AggregateProjection { function, alias } = aggregate;
    match function {
        AggregateFunction::CountAll => {
            let count = map_reduction(table_name, table.count(selection))?;
            let count = i64::try_from(count).map_err(|_| CatalogError::CountOutOfRange {
                name: table_name.to_owned(),
                count,
            })?;
            Ok(ScalarResult {
                field: Field::new(
                    alias.unwrap_or_else(|| "count()".to_owned()),
                    DataType::Int64,
                ),
                value: Value::Int64(count),
            })
        }
        AggregateFunction::CountDistinct { column } => {
            let count = map_reduction(table_name, table.count_distinct(&column, selection))?;
            let count = i64::try_from(count).map_err(|_| CatalogError::CountOutOfRange {
                name: table_name.to_owned(),
                count,
            })?;
            Ok(ScalarResult {
                field: Field::new(
                    alias.unwrap_or_else(|| format!("count(distinct {column})")),
                    DataType::Int64,
                ),
                value: Value::Int64(count),
            })
        }
        AggregateFunction::Sum { column } => {
            let value = map_reduction(table_name, table.sum(&column, selection))?;
            Ok(scalar_result(alias, "sum", column, value))
        }
        AggregateFunction::Avg { column } => {
            let value =
                map_reduction(table_name, table.avg(&column, selection))?.ok_or_else(|| {
                    CatalogError::EmptyAggregateInput {
                        name: table_name.to_owned(),
                        function: "AVG",
                        field: column.clone(),
                    }
                })?;
            Ok(scalar_result(alias, "avg", column, value))
        }
        AggregateFunction::Min { column } => {
            let result = table.min_with_string_limit(&column, selection, string_budget.remaining());
            let value =
                map_extreme_reduction(table_name, string_budget, result)?.ok_or_else(|| {
                    CatalogError::EmptyAggregateInput {
                        name: table_name.to_owned(),
                        function: "MIN",
                        field: column.clone(),
                    }
                })?;
            string_budget.account(&value);
            Ok(scalar_result(alias, "min", column, value))
        }
        AggregateFunction::Max { column } => {
            let result = table.max_with_string_limit(&column, selection, string_budget.remaining());
            let value =
                map_extreme_reduction(table_name, string_budget, result)?.ok_or_else(|| {
                    CatalogError::EmptyAggregateInput {
                        name: table_name.to_owned(),
                        function: "MAX",
                        field: column.clone(),
                    }
                })?;
            string_budget.account(&value);
            Ok(scalar_result(alias, "max", column, value))
        }
    }
}

struct AggregateStringBudget {
    limit: usize,
    used: usize,
}

impl AggregateStringBudget {
    const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    const fn remaining(&self) -> usize {
        self.limit - self.used
    }

    fn account(&mut self, value: &Value) {
        if let Value::String(value) = value {
            self.used = self
                .used
                .checked_add(value.len())
                .expect("bounded aggregate String bytes cannot overflow");
            debug_assert!(self.used <= self.limit);
        }
    }
}

fn map_extreme_reduction(
    table_name: &str,
    string_budget: &AggregateStringBudget,
    result: Result<Option<Value>, ReductionError>,
) -> Result<Option<Value>, CatalogError> {
    match result {
        Err(ReductionError::StringResultTooLarge {
            field, required, ..
        }) => Err(CatalogError::AggregateResultTooLarge {
            name: table_name.to_owned(),
            field,
            limit: string_budget.limit,
            required: string_budget.used.saturating_add(required),
        }),
        result => map_reduction(table_name, result),
    }
}

fn map_reduction<T>(
    table_name: &str,
    result: Result<T, ReductionError>,
) -> Result<T, CatalogError> {
    result.map_err(|source| CatalogError::TableReduction {
        name: table_name.to_owned(),
        source,
    })
}

fn scalar_result(
    alias: Option<String>,
    function: &'static str,
    column: String,
    value: Value,
) -> ScalarResult {
    ScalarResult {
        field: Field::new(
            alias.unwrap_or_else(|| format!("{function}({column})")),
            value.data_type(),
        ),
        value,
    }
}

fn scan_predicate_groups(
    table: &Table,
    predicate_groups: Vec<Vec<ComparisonPredicate>>,
    table_name: &str,
) -> Result<Option<RowSelection>, CatalogError> {
    if predicate_groups.is_empty() || predicate_groups.iter().any(Vec::is_empty) {
        return Ok(None);
    }

    let mut groups = predicate_groups.into_iter();
    let mut selection = scan_predicate_group(
        table,
        groups
            .next()
            .expect("non-empty predicate groups were checked above"),
        table_name,
    )?;

    for group in groups {
        let next = scan_predicate_group(table, group, table_name)?;
        selection.union(&next);
    }

    Ok(Some(selection))
}

fn scan_predicate_group(
    table: &Table,
    predicates: Vec<ComparisonPredicate>,
    table_name: &str,
) -> Result<RowSelection, CatalogError> {
    let mut predicates = predicates.into_iter();
    let first = predicates
        .next()
        .expect("empty predicate groups were checked by scan_predicate_groups");
    let mut selection = scan_predicate(table, first, table_name)?;

    for predicate in predicates {
        let next = scan_predicate(table, predicate, table_name)?;
        selection.intersect(&next);
    }

    Ok(selection)
}

fn scan_predicate(
    table: &Table,
    predicate: ComparisonPredicate,
    table_name: &str,
) -> Result<RowSelection, CatalogError> {
    table
        .scan(&predicate.column, predicate.operator, &predicate.value)
        .map_err(|source| CatalogError::TableScan {
            name: table_name.to_owned(),
            source,
        })
}

fn limited_row_bounds(
    table_rows: usize,
    selection: Option<&RowSelection>,
    limit: Option<usize>,
) -> (usize, usize) {
    let selected_count = selection.map_or(table_rows, RowSelection::selected_count);
    let row_count = limit.map_or(selected_count, |limit| selected_count.min(limit));
    let row_end = match selection {
        None => row_count,
        Some(_) if row_count == 0 => 0,
        Some(selection) if row_count < selected_count => selection
            .selected_rows()
            .nth(row_count - 1)
            .map_or(0, |row| row + 1),
        Some(_) => table_rows,
    };

    (row_end, row_count)
}

fn resolve_order(
    table: &Table,
    order_by: Vec<OrderByClause>,
) -> Result<Vec<(usize, OrderDirection)>, CatalogError> {
    order_by
        .into_iter()
        .map(|order_key| {
            table
                .fields()
                .iter()
                .position(|field| field.name() == order_key.column)
                .map(|index| (index, order_key.direction))
                .ok_or(CatalogError::OrderFieldNotFound {
                    name: order_key.column,
                })
        })
        .collect()
}

fn ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
    limit: Option<usize>,
) -> Result<Vec<usize>, CatalogError> {
    match limit {
        Some(limit) => bounded_ordered_row_indices(table, order_keys, selection, limit),
        None => fully_ordered_row_indices(table, order_keys, selection),
    }
}

fn fully_ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
) -> Result<Vec<usize>, CatalogError> {
    let row_count = selection.map_or(table.len(), RowSelection::selected_count);
    let mut rows = try_order_row_buffer(row_count)?;
    match selection {
        Some(selection) => rows.extend(selection.selected_rows()),
        None => rows.extend(0..table.len()),
    }

    let order = RowOrder::new(table, order_keys);
    rows.sort_unstable_by(|left, right| order.compare(*left, *right));
    Ok(rows)
}

fn bounded_ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
    limit: usize,
) -> Result<Vec<usize>, CatalogError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let row_count = selection.map_or(table.len(), RowSelection::selected_count);
    let retained_count = row_count.min(limit);
    let mut rows = try_order_row_buffer(retained_count)?;
    let order = RowOrder::new(table, order_keys);

    match selection {
        Some(selection) => {
            for row in selection.selected_rows() {
                retain_top_row(&mut rows, row, retained_count, order);
            }
        }
        None => {
            for row in 0..table.len() {
                retain_top_row(&mut rows, row, retained_count, order);
            }
        }
    }

    rows.sort_unstable_by(|left, right| order.compare(*left, *right));
    Ok(rows)
}

fn try_order_row_buffer(row_count: usize) -> Result<Vec<usize>, CatalogError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count)
        .map_err(|_| CatalogError::OrderAllocationFailed { row_count })?;
    Ok(rows)
}

#[derive(Clone, Copy)]
struct RowOrder<'a> {
    table: &'a Table,
    order_keys: &'a [(usize, OrderDirection)],
}

impl<'a> RowOrder<'a> {
    const fn new(table: &'a Table, order_keys: &'a [(usize, OrderDirection)]) -> Self {
        Self { table, order_keys }
    }

    fn compare(self, left: usize, right: usize) -> Ordering {
        self.order_keys
            .iter()
            .map(|(column_index, direction)| {
                let value_order =
                    compare_column_rows(&self.table.columns()[*column_index], left, right);
                match direction {
                    OrderDirection::Ascending => value_order,
                    OrderDirection::Descending => value_order.reverse(),
                }
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| left.cmp(&right))
    }
}

fn retain_top_row(rows: &mut Vec<usize>, row: usize, limit: usize, order: RowOrder<'_>) {
    // This max-heap keeps the worst retained row at its root for replacement.
    if rows.len() < limit {
        rows.push(row);
        sift_order_heap_up(rows, order);
    } else if order.compare(row, rows[0]).is_lt() {
        rows[0] = row;
        sift_order_heap_down(rows, order);
    }
}

fn sift_order_heap_up(rows: &mut [usize], order: RowOrder<'_>) {
    let mut child = rows.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if !order.compare(rows[parent], rows[child]).is_lt() {
            break;
        }
        rows.swap(parent, child);
        child = parent;
    }
}

fn sift_order_heap_down(rows: &mut [usize], order: RowOrder<'_>) {
    let mut parent = 0;
    loop {
        let left = parent * 2 + 1;
        if left >= rows.len() {
            break;
        }
        let right = left + 1;
        let greater_child = if right < rows.len() && order.compare(rows[left], rows[right]).is_lt()
        {
            right
        } else {
            left
        };
        if !order.compare(rows[parent], rows[greater_child]).is_lt() {
            break;
        }
        rows.swap(parent, greater_child);
        parent = greater_child;
    }
}

fn compare_column_rows(column: &Column, left: usize, right: usize) -> Ordering {
    match column {
        Column::Int64(values) => values[left].cmp(&values[right]),
        Column::Float64(values) => values[left].total_cmp(&values[right]),
        Column::Bool(values) => values[left].cmp(&values[right]),
        Column::String(values) => values[left].cmp(&values[right]),
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

const fn default_insert_parse_limits() -> InsertParseLimits {
    InsertParseLimits::new(
        InsertParseLimits::DEFAULT_MAX_INPUT_BYTES,
        InsertParseLimits::DEFAULT_MAX_ROWS,
        InsertParseLimits::DEFAULT_MAX_VALUES_PER_ROW,
        InsertParseLimits::DEFAULT_MAX_STRING_BYTES,
    )
}

const fn default_select_parse_limits() -> SelectParseLimits {
    SelectParseLimits::new(
        SelectParseLimits::DEFAULT_MAX_INPUT_BYTES,
        SelectParseLimits::DEFAULT_MAX_PROJECTIONS,
    )
}

fn normalize_table_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn resolve_projection(
    table: &Table,
    projection: SelectProjection,
) -> Result<Vec<usize>, CatalogError> {
    let field_count = match &projection {
        SelectProjection::All => table.fields().len(),
        SelectProjection::Columns(names) => names.len(),
        SelectProjection::CountAll { .. }
        | SelectProjection::Aggregates(_)
        | SelectProjection::GroupedCount { .. } => {
            unreachable!("aggregates are reduced before projection resolution")
        }
    };
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(field_count)
        .map_err(|_| CatalogError::ProjectionAllocationFailed { field_count })?;

    match projection {
        SelectProjection::All => indices.extend(0..field_count),
        SelectProjection::Columns(names) => {
            for name in names {
                let index = table
                    .fields()
                    .iter()
                    .position(|field| field.name() == name)
                    .ok_or(CatalogError::ProjectionFieldNotFound { name })?;
                indices.push(index);
            }
        }
        SelectProjection::CountAll { .. }
        | SelectProjection::Aggregates(_)
        | SelectProjection::GroupedCount { .. } => {
            unreachable!("aggregates are reduced before projection resolution")
        }
    }
    Ok(indices)
}

#[cfg(test)]
mod ordered_row_tests {
    use super::*;

    #[test]
    fn zero_and_empty_bounded_orders_have_no_row_buffer() {
        let mut table = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
        table
            .insert_batch((0..4).map(|id| vec![Value::Int64(id)]))
            .unwrap();
        let order_keys = [(0, OrderDirection::Ascending)];

        let zero = bounded_ordered_row_indices(&table, &order_keys, None, 0).unwrap();
        assert!(zero.is_empty());
        assert_eq!(zero.capacity(), 0);

        let empty = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
        let empty_rows = bounded_ordered_row_indices(&empty, &order_keys, None, 25).unwrap();
        assert!(empty_rows.is_empty());
        assert_eq!(empty_rows.capacity(), 0);
    }

    #[test]
    fn order_row_buffer_reports_capacity_overflow() {
        assert_eq!(
            try_order_row_buffer(usize::MAX),
            Err(CatalogError::OrderAllocationFailed {
                row_count: usize::MAX,
            })
        );
    }
}
