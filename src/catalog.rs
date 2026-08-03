//! In-memory ownership and `CREATE TABLE`/`INSERT`/`SELECT` execution.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::reduction::ReductionError;
use crate::snapshot::SnapshotStore;
use crate::sql::{
    CreateTableStatement, InsertParseLimits, InsertStatement, ParseError, ParseLimits,
    SelectParseLimits, SelectProjection, SelectStatement, parse_create_table_with_limits,
    parse_insert_with_limits, parse_select_with_limits,
};
use crate::storage::{Column, DEFAULT_ROW_LIMIT, DataType, Field, Table, TableError, Value};
use crate::table_snapshot::TableSnapshotError;
use crate::{RowSelection, ScanError};

/// Default maximum number of tables owned by one [`Catalog`].
pub const DEFAULT_MAX_TABLES: usize = 1024;

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
    /// A `WHERE` comparison could not be scanned against its target table.
    TableScan {
        /// Name from the rejected statement.
        name: String,
        /// Scan validation or allocation failure.
        source: ScanError,
    },
    /// A `COUNT(*)` reduction failed after its optional scan.
    TableReduction {
        /// Name from the rejected statement.
        name: String,
        /// Reduction validation failure.
        source: ReductionError,
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
            Self::TableScan { name, source } => {
                write!(formatter, "could not scan table `{name}`: {source}")
            }
            Self::TableReduction { name, source } => {
                write!(formatter, "could not reduce table `{name}`: {source}")
            }
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

/// The output of one projection or scalar aggregate and optional comparison scan.
///
/// A row projection owns only projected column indexes and, for a filtered
/// query, a compact row-selection bitmap. Schema and column values remain owned
/// by the catalog's source table. A scalar aggregate owns its single field and
/// value.
#[derive(Debug)]
pub struct SelectResult<'a> {
    table: &'a Table,
    field_indices: Vec<usize>,
    selection: Option<RowSelection>,
    scalar: Option<ScalarResult>,
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
        let field_count = self.field_indices.len() + usize::from(self.scalar.is_some());
        (0..field_count).map(|index| {
            self.field_indices.get(index).map_or_else(
                || {
                    &self
                        .scalar
                        .as_ref()
                        .expect("a non-projection field belongs to the scalar result")
                        .field
                },
                |field_index| &self.table.fields()[*field_index],
            )
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

    /// Iterates over selected zero-based result row indexes.
    ///
    /// Projection row indexes are also source table indexes. A scalar
    /// aggregate has exactly one result row at index zero.
    pub fn selected_rows(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        let row_count = if self.scalar.is_some() {
            1
        } else {
            self.table.len()
        };
        (0..row_count).filter(|row| match (&self.scalar, &self.selection) {
            (Some(_), _) => true,
            (None, Some(selection)) => selection
                .get(*row)
                .expect("selection and source table have the same row count"),
            (None, None) => true,
        })
    }

    /// Returns the value for a scalar aggregate result.
    ///
    /// Row projections return `None`.
    #[must_use]
    pub const fn scalar_value(&self) -> Option<&Value> {
        match &self.scalar {
            Some(scalar) => Some(&scalar.value),
            None => None,
        }
    }

    /// Alias for [`Self::selected_rows`].
    pub fn row_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.selected_rows()
    }

    /// Returns the number of output rows.
    #[must_use]
    pub fn len(&self) -> usize {
        if self.scalar.is_some() {
            1
        } else {
            self.selection
                .as_ref()
                .map_or(self.table.len(), RowSelection::selected_count)
        }
    }

    /// Returns whether the result has no output rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    /// rows. An optional `WHERE` comparison is evaluated with [`Table::scan`].
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
            predicate,
        } = statement;
        let table = self
            .tables
            .get(&normalize_table_name(&name))
            .map(|entry| &entry.table)
            .ok_or_else(|| CatalogError::TableNotFound { name: name.clone() })?;

        match projections {
            SelectProjection::CountAll { alias } => {
                let selection = predicate
                    .map(|predicate| {
                        table.scan(&predicate.column, predicate.operator, &predicate.value)
                    })
                    .transpose()
                    .map_err(|source| CatalogError::TableScan {
                        name: name.clone(),
                        source,
                    })?;
                let count = table.count(selection.as_ref()).map_err(|source| {
                    CatalogError::TableReduction {
                        name: name.clone(),
                        source,
                    }
                })?;
                let count = i64::try_from(count)
                    .map_err(|_| CatalogError::CountOutOfRange { name, count })?;

                Ok(SelectResult {
                    table,
                    field_indices: Vec::new(),
                    selection: None,
                    scalar: Some(ScalarResult {
                        field: Field::new(
                            alias.unwrap_or_else(|| "count()".to_owned()),
                            DataType::Int64,
                        ),
                        value: Value::Int64(count),
                    }),
                })
            }
            projections => {
                let field_indices = resolve_projection(table, projections)?;
                let selection = predicate
                    .map(|predicate| {
                        table.scan(&predicate.column, predicate.operator, &predicate.value)
                    })
                    .transpose()
                    .map_err(|source| CatalogError::TableScan { name, source })?;

                Ok(SelectResult {
                    table,
                    field_indices,
                    selection,
                    scalar: None,
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
        SelectProjection::CountAll { .. } => unreachable!("counts are reduced before resolution"),
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
        SelectProjection::CountAll { .. } => unreachable!("counts are reduced before resolution"),
    }
    Ok(indices)
}
