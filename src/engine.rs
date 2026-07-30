use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

/// Default maximum number of groups aggregated in one in-memory partition.
pub const DEFAULT_MAX_IN_MEMORY_GROUPS: usize = 65_536;

/// Execution settings for a [`Database`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseOptions {
    /// Maximum aggregate groups held in memory by one grouping partition.
    pub max_in_memory_groups: usize,
    /// Parent directory for per-query spill directories.
    pub temporary_directory: Option<PathBuf>,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            max_in_memory_groups: DEFAULT_MAX_IN_MEMORY_GROUPS,
            temporary_directory: None,
        }
    }
}

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
    options: DatabaseOptions,
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

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_options(options: DatabaseOptions) -> Self {
        Self {
            catalog: Catalog::default(),
            options,
        }
    }

    #[must_use]
    pub fn options(&self) -> &DatabaseOptions {
        &self.options
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        if self.options.max_in_memory_groups == 0 {
            return Err(Error::InvalidQuery(
                "max_in_memory_groups must be at least 1".to_owned(),
            ));
        }
        sql::parse(sql)?
            .into_iter()
            .map(|statement| self.execute_statement(statement))
            .collect()
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.catalog.create_table(name, columns)?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::Insert { table, rows } => {
                let affected_rows = rows.len();
                {
                    let target = self.catalog.table(&table)?;
                    for row in &rows {
                        target.validate_row(row)?;
                    }
                }
                let target = self.catalog.table_mut(&table)?;
                for row in rows {
                    target.insert_row(row)?;
                }
                Ok(StatementResult::Command {
                    tag: "INSERT",
                    affected_rows,
                })
            }
            Statement::Select(select) => self.execute_select(select).map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: Select) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let plan = GroupedExecutionPlan {
                group_columns: &group_columns,
                aggregate_specs: &aggregate_specs,
                items: &items,
                ordering: &ordering,
                limit: select.limit,
                options: &self.options,
            };
            let grouped = execute_grouped(table, &matching_rows, &plan)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
            );
            grouped.project(&selected_groups, &items)
        } else {
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            execute_projection(table, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Column {
        source: usize,
        group_position: Option<usize>,
    },
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

fn resolve_group_columns(table: &Table, names: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = table.column_index(name)?;
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "GROUP BY column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok(columns)
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
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (argument_index, input_type, argument_name) = match argument {
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

fn validate_aggregate(function: AggregateFunction, input_type: Option<DataType>) -> Result<()> {
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
        AggregateFunction::Count => DataType::Int64,
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
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source, .. } => table.columns()[*source].value(*row),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect()
        })
        .collect()
}

struct GroupedExecutionPlan<'query> {
    group_columns: &'query [usize],
    aggregate_specs: &'query [AggregateSpec],
    items: &'query [ResolvedItem],
    ordering: &'query [ResolvedOrder],
    limit: Option<usize>,
    options: &'query DatabaseOptions,
}

fn execute_grouped<'table>(
    table: &'table Table,
    matching_rows: &[usize],
    plan: &GroupedExecutionPlan<'_>,
) -> Result<GroupedData<'table>> {
    let rows = matching_rows.iter().copied().map(Ok);
    if plan.group_columns.is_empty()
        || groups_fit_in_memory(
            table,
            plan.group_columns,
            rows,
            plan.options.max_in_memory_groups,
        )?
    {
        return aggregate_rows(
            table,
            plan.group_columns,
            plan.aggregate_specs,
            matching_rows.iter().copied().map(Ok),
            matching_rows.len(),
            plan.options.max_in_memory_groups,
        );
    }

    execute_spilled_grouped(table, matching_rows, plan).map(|result| result.grouped)
}

fn groups_fit_in_memory(
    table: &Table,
    group_columns: &[usize],
    rows: impl Iterator<Item = Result<usize>>,
    max_groups: usize,
) -> Result<bool> {
    let mut groups = GroupIndex::new(group_columns.len(), max_groups.min(1_024));
    let mut group_count = usize::from(group_columns.is_empty());

    for row in rows {
        let row = row?;
        let Some((_, inserted)) =
            groups.find_or_insert(table, group_columns, row, group_count, max_groups)
        else {
            return Ok(false);
        };
        group_count += usize::from(inserted);
    }
    Ok(true)
}

fn aggregate_rows<'a>(
    table: &'a Table,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    rows: impl Iterator<Item = Result<usize>>,
    row_count_hint: usize,
    max_groups: usize,
) -> Result<GroupedData<'a>> {
    let initial_capacity = row_count_hint.min(max_groups).min(1_024);
    let mut groups = GroupIndex::new(group_columns.len(), initial_capacity);
    let mut group_count = usize::from(group_columns.is_empty());
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(initial_capacity);
            if group_columns.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for row in rows {
        let row = row?;
        let (group, inserted) = groups
            .find_or_insert(table, group_columns, row, group_count, max_groups)
            .expect("partition group count is checked before aggregation");
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, table, row)?;
        }
    }

    let keys = groups.into_keys(group_count);
    let aggregates = aggregate_states
        .into_iter()
        .map(|states| {
            states
                .into_iter()
                .map(AggregateState::finish)
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GroupedData { keys, aggregates })
}

const SPILL_PARTITION_COUNT: usize = 16;
const MAX_REPARTITION_DEPTH: usize = 64;
static NEXT_SPILL_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn execute_spilled_grouped<'table>(
    table: &'table Table,
    matching_rows: &[usize],
    plan: &GroupedExecutionPlan<'_>,
) -> Result<SpilledGroupedData<'table>> {
    let mut workspace = SpillWorkspace::new(plan.options.temporary_directory.as_deref())?;
    let root_partitions = partition_rows(
        &mut workspace,
        table,
        plan.group_columns,
        matching_rows.iter().copied().map(Ok),
        0,
    )?;
    let grouped = {
        let mut execution = SpillExecution {
            workspace: &mut workspace,
            table,
            group_columns: plan.group_columns,
            aggregate_specs: plan.aggregate_specs,
            items: plan.items,
            ordering: plan.ordering,
            limit: plan.limit,
            max_groups: plan.options.max_in_memory_groups,
            grouped: GroupedData::empty(plan.aggregate_specs.len()),
            peak_retained_groups: 0,
        };
        for partition in root_partitions {
            execution.process_partition(partition, 1)?;
        }
        let SpillExecution {
            grouped,
            peak_retained_groups,
            ..
        } = execution;
        SpilledGroupedData {
            grouped,
            peak_retained_groups,
        }
    };

    workspace.cleanup()?;
    if let Some(limit) = plan.limit {
        debug_assert!(
            grouped.peak_retained_groups <= plan.options.max_in_memory_groups.saturating_add(limit)
        );
    }
    Ok(grouped)
}

struct SpilledGroupedData<'table> {
    grouped: GroupedData<'table>,
    peak_retained_groups: usize,
}

struct SpillExecution<'config, 'table> {
    workspace: &'config mut SpillWorkspace,
    table: &'table Table,
    group_columns: &'config [usize],
    aggregate_specs: &'config [AggregateSpec],
    items: &'config [ResolvedItem],
    ordering: &'config [ResolvedOrder],
    limit: Option<usize>,
    max_groups: usize,
    grouped: GroupedData<'table>,
    peak_retained_groups: usize,
}

impl SpillExecution<'_, '_> {
    fn process_partition(&mut self, partition: SpillFile, depth: usize) -> Result<()> {
        if partition.row_count == 0 {
            remove_spill_file(&partition.path)?;
            return Ok(());
        }

        let fits = groups_fit_in_memory(
            self.table,
            self.group_columns,
            RowIndexReader::open(&partition.path, self.table.row_count())?,
            self.max_groups,
        )?;
        if fits {
            let partition_grouped = aggregate_rows(
                self.table,
                self.group_columns,
                self.aggregate_specs,
                RowIndexReader::open(&partition.path, self.table.row_count())?,
                partition.row_count,
                self.max_groups,
            )?;
            self.peak_retained_groups = self
                .peak_retained_groups
                .max(self.grouped.len().saturating_add(partition_grouped.len()));
            self.grouped.append(partition_grouped);
            if let Some(limit) = self.limit {
                self.grouped.retain_best(self.items, self.ordering, limit);
            }
            remove_spill_file(&partition.path)?;
            return Ok(());
        }

        if depth >= MAX_REPARTITION_DEPTH {
            return Err(Error::TemporaryStorage(format!(
                "could not divide a grouping partition below {} groups after {depth} levels",
                self.max_groups
            )));
        }

        let children = partition_rows(
            self.workspace,
            self.table,
            self.group_columns,
            RowIndexReader::open(&partition.path, self.table.row_count())?,
            depth,
        )?;
        remove_spill_file(&partition.path)?;
        for child in children {
            self.process_partition(child, depth + 1)?;
        }
        Ok(())
    }
}

fn partition_rows(
    workspace: &mut SpillWorkspace,
    table: &Table,
    group_columns: &[usize],
    rows: impl Iterator<Item = Result<usize>>,
    depth: usize,
) -> Result<Vec<SpillFile>> {
    let created_partitions = (0..SPILL_PARTITION_COUNT)
        .map(|_| workspace.create_partition())
        .collect::<Result<Vec<_>>>()?;
    let (mut partitions, files): (Vec<_>, Vec<_>) = created_partitions.into_iter().unzip();
    let mut writers = files.into_iter().map(BufWriter::new).collect::<Vec<_>>();

    for row in rows {
        let row = row?;
        let partition_index =
            group_hash(table, group_columns, row, depth) % SPILL_PARTITION_COUNT as u64;
        let row_index = u64::try_from(row).map_err(|_| {
            Error::TemporaryStorage("row index does not fit in a spill record".to_owned())
        })?;
        writers[partition_index as usize]
            .write_all(&row_index.to_le_bytes())
            .map_err(|error| {
                temporary_storage_error(
                    "write spill partition",
                    &partitions[partition_index as usize].path,
                    error,
                )
            })?;
        partitions[partition_index as usize].row_count += 1;
    }

    for (writer, partition) in writers.iter_mut().zip(&partitions) {
        writer.flush().map_err(|error| {
            temporary_storage_error("flush spill partition", &partition.path, error)
        })?;
    }
    Ok(partitions)
}

fn group_hash(table: &Table, group_columns: &[usize], row: usize, depth: usize) -> u64 {
    let seed = (depth as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut hasher = StableHasher::new(seed);
    group_columns.len().hash(&mut hasher);
    for column in group_columns {
        table.columns()[*column].value_ref(row).hash(&mut hasher);
    }
    avalanche(hasher.finish() ^ seed.rotate_left(17))
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

struct StableHasher {
    state: u64,
}

impl StableHasher {
    fn new(seed: u64) -> Self {
        Self {
            state: 0xcbf2_9ce4_8422_2325 ^ avalanche(seed),
        }
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[derive(Debug)]
struct SpillFile {
    path: PathBuf,
    row_count: usize,
}

struct SpillWorkspace {
    path: PathBuf,
    next_file: u64,
    active: bool,
}

impl SpillWorkspace {
    fn new(configured_parent: Option<&Path>) -> Result<Self> {
        let parent = configured_parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
        fs::create_dir_all(&parent).map_err(|error| {
            temporary_storage_error("create temporary directory", &parent, error)
        })?;

        loop {
            let sequence = NEXT_SPILL_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
            let path = parent.join(format!("rusthouse-group-{}-{sequence}", std::process::id()));
            match create_private_directory(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        next_file: 0,
                        active: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(temporary_storage_error(
                        "create spill workspace",
                        &path,
                        error,
                    ));
                }
            }
        }
    }

    fn create_partition(&mut self) -> Result<(SpillFile, File)> {
        let path = self.path.join(format!("partition-{}.bin", self.next_file));
        self.next_file += 1;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .map_err(|error| temporary_storage_error("create spill partition", &path, error))?;
        Ok((SpillFile { path, row_count: 0 }, file))
    }

    fn cleanup(mut self) -> Result<()> {
        fs::remove_dir_all(&self.path).map_err(|error| {
            temporary_storage_error("remove spill workspace", &self.path, error)
        })?;
        self.active = false;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

impl Drop for SpillWorkspace {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct RowIndexReader {
    reader: BufReader<File>,
    remaining: usize,
    row_limit: usize,
    path: PathBuf,
}

impl RowIndexReader {
    fn open(path: &Path, row_limit: usize) -> Result<Self> {
        let file = File::open(path)
            .map_err(|error| temporary_storage_error("open spill partition", path, error))?;
        let byte_len = file
            .metadata()
            .map_err(|error| temporary_storage_error("inspect spill partition", path, error))?
            .len();
        if byte_len % 8 != 0 {
            return Err(Error::TemporaryStorage(format!(
                "spill partition '{}' has a partial row index",
                path.display()
            )));
        }
        let record_count = usize::try_from(byte_len / 8).map_err(|_| {
            Error::TemporaryStorage(format!(
                "spill partition '{}' is too large for this platform",
                path.display()
            ))
        })?;
        Ok(Self {
            reader: BufReader::new(file),
            remaining: record_count,
            row_limit,
            path: path.to_path_buf(),
        })
    }
}

impl Iterator for RowIndexReader {
    type Item = Result<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let mut bytes = [0_u8; 8];
        if let Err(error) = self.reader.read_exact(&mut bytes) {
            self.remaining = 0;
            return Some(Err(temporary_storage_error(
                "read spill partition",
                &self.path,
                error,
            )));
        }
        self.remaining -= 1;
        let row = match usize::try_from(u64::from_le_bytes(bytes)) {
            Ok(row) if row < self.row_limit => row,
            Ok(_) | Err(_) => {
                return Some(Err(Error::TemporaryStorage(format!(
                    "spill partition '{}' contains an invalid row index",
                    self.path.display()
                ))));
            }
        };
        Some(Ok(row))
    }
}

fn remove_spill_file(path: &Path) -> Result<()> {
    fs::remove_file(path)
        .map_err(|error| temporary_storage_error("remove spill partition", path, error))
}

fn temporary_storage_error(action: &str, path: &Path, error: io::Error) -> Error {
    Error::TemporaryStorage(format!("{action} '{}': {error}", path.display()))
}

#[derive(Debug)]
enum GroupIndex<'a> {
    Global,
    One(HashMap<ValueRef<'a>, usize>),
    Multiple(HashMap<Box<[ValueRef<'a>]>, usize>),
}

impl<'a> GroupIndex<'a> {
    fn new(column_count: usize, row_count: usize) -> Self {
        let initial_capacity = row_count.min(1_024);
        match column_count {
            0 => Self::Global,
            1 => Self::One(HashMap::with_capacity(initial_capacity)),
            _ => Self::Multiple(HashMap::with_capacity(initial_capacity)),
        }
    }

    fn find_or_insert(
        &mut self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        next_group: usize,
        max_groups: usize,
    ) -> Option<(usize, bool)> {
        match self {
            Self::Global => Some((0, false)),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                if let Some(group) = groups.get(&key) {
                    Some((*group, false))
                } else if next_group == max_groups {
                    None
                } else {
                    groups.insert(key, next_group);
                    Some((next_group, true))
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                find_or_insert_group(groups, &key, next_group, max_groups)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| table.columns()[*column].value_ref(row))
                    .collect::<Vec<_>>();
                find_or_insert_group(groups, &key, next_group, max_groups)
            }
        }
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

fn find_or_insert_group<'a>(
    groups: &mut HashMap<Box<[ValueRef<'a>]>, usize>,
    key: &[ValueRef<'a>],
    next_group: usize,
    max_groups: usize,
) -> Option<(usize, bool)> {
    if let Some(group) = groups.get(key) {
        Some((*group, false))
    } else if next_group == max_groups {
        None
    } else {
        groups.insert(key.into(), next_group);
        Some((next_group, true))
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

impl<'a> GroupedData<'a> {
    fn empty(aggregate_count: usize) -> Self {
        Self {
            keys: Vec::new(),
            aggregates: (0..aggregate_count).map(|_| Vec::new()).collect(),
        }
    }

    fn append(&mut self, mut other: GroupedData<'a>) {
        self.keys.append(&mut other.keys);
        debug_assert_eq!(self.aggregates.len(), other.aggregates.len());
        for (target, mut source) in self.aggregates.iter_mut().zip(other.aggregates) {
            target.append(&mut source);
        }
    }

    fn retain_best(&mut self, items: &[ResolvedItem], ordering: &[ResolvedOrder], limit: usize) {
        if self.len() <= limit {
            return;
        }
        let mut selected = (0..self.len()).collect::<Vec<_>>();
        order_grouped_rows(&mut selected, self, items, ordering, Some(limit));
        retain_selected(&mut self.keys, &selected);
        for values in &mut self.aggregates {
            retain_selected(values, &selected);
        }
    }

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
                        ResolvedItem::Aggregate { state } => {
                            self.aggregates[*state][*group].clone()
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

fn retain_selected<T>(values: &mut Vec<T>, selected: &[usize]) {
    let mut available = std::mem::take(values)
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    values.reserve(selected.len());
    for index in selected {
        values.push(
            available[*index]
                .take()
                .expect("selected group indices are unique"),
        );
    }
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt(i64),
    SumFloat(f64),
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: f64, count: u64 },
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => Self::SumInt(0),
            AggregateFunction::Sum => Self::SumFloat(0.0),
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(values[row])
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += values[row];
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let column = &table.columns()[spec.argument.expect("MIN argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum += values[row];
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()));
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Value> {
        match self {
            Self::Count(value) | Self::SumInt(value) => Ok(Value::Int64(value)),
            Self::SumFloat(value) => Ok(Value::Float64(value)),
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum / count as f64)),
            Self::Min(None) => Err(Error::InvalidQuery(
                "MIN is undefined for an empty input".to_owned(),
            )),
            Self::Max(None) => Err(Error::InvalidQuery(
                "MAX is undefined for an empty input".to_owned(),
            )),
            Self::AvgInt { .. } | Self::AvgFloat { .. } => Err(Error::InvalidQuery(
                "AVG is undefined for an empty input".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOrder {
    output: usize,
    descending: bool,
}

fn resolve_ordering(columns: &[ResultColumn], requested: &[OrderBy]) -> Result<Vec<ResolvedOrder>> {
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let matches = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
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

fn order_source_rows(
    rows: &mut Vec<usize>,
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    if ordering.is_empty() {
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        return;
    }

    sort_and_limit(rows, limit, |left, right| {
        for order in ordering {
            let ResolvedItem::Column { source, .. } = items[order.output] else {
                unreachable!("ungrouped projections cannot contain aggregates")
            };
            let comparison = table.columns()[source].cmp_at(left, right);
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

fn sort_and_limit(
    indices: &mut Vec<usize>,
    limit: Option<usize>,
    compare: impl Fn(usize, usize) -> Ordering,
) {
    if let Some(0) = limit {
        indices.clear();
        return;
    }
    if let Some(limit) = limit.filter(|limit| *limit < indices.len()) {
        indices.select_nth_unstable_by(limit, |left, right| compare(*left, *right));
        indices.truncate(limit);
    }
    indices.sort_unstable_by(|left, right| compare(*left, *right));
}

#[derive(Debug)]
enum CompiledPredicate {
    Comparison {
        left: CompiledOperand,
        operator: ComparisonOperator,
        right: CompiledOperand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(table, row);
                let right = right.value(table, row);
                let comparison = left
                    .sql_cmp(right)
                    .expect("predicate operand types are validated");
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            Self::And(left, right) => left.evaluate(table, row) && right.evaluate(table, row),
            Self::Or(left, right) => left.evaluate(table, row) || right.evaluate(table, row),
        }
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
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(CompiledPredicate::And(
            Box::new(compile_predicate(table, left)?),
            Box::new(compile_predicate(table, right)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(table, left)?),
            Box::new(compile_predicate(table, right)?),
        )),
    }
}

fn compile_operand(table: &Table, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = table.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: table.schema()[index].data_type,
            })
        }
        Operand::Literal(value) => Ok(CompiledOperand::Literal(value.clone())),
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

    fn spill_test_parent(label: &str) -> PathBuf {
        let sequence = NEXT_SPILL_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create spill test parent");
        path
    }

    fn query(database: &mut Database, sql: &str) -> QueryResult {
        let results = database.execute(sql).expect("query succeeds");
        match results.into_iter().last().expect("one result") {
            StatementResult::Query(result) => result,
            StatementResult::Command { .. } => panic!("expected query result"),
        }
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

    #[cfg(unix)]
    #[test]
    fn spill_workspace_and_partition_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let parent = spill_test_parent("permissions-test");
        let mut workspace = SpillWorkspace::new(Some(&parent)).expect("create spill workspace");
        let workspace_mode = fs::metadata(&workspace.path)
            .expect("inspect spill workspace")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(workspace_mode, 0o700);

        let (partition, file) = workspace.create_partition().expect("create partition");
        drop(file);
        let partition_mode = fs::metadata(&partition.path)
            .expect("inspect spill partition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(partition_mode, 0o600);

        remove_spill_file(&partition.path).expect("remove partition");
        workspace.cleanup().expect("remove spill workspace");
        fs::remove_dir(parent).expect("remove spill test parent");
    }

    #[test]
    fn spill_reader_rejects_out_of_range_row_indices() {
        let parent = spill_test_parent("row-index-test");
        let mut workspace = SpillWorkspace::new(Some(&parent)).expect("create spill workspace");
        let (partition, mut file) = workspace.create_partition().expect("create partition");
        file.write_all(&9_u64.to_le_bytes())
            .expect("write invalid row index");
        file.flush().expect("flush invalid row index");
        drop(file);

        let error = RowIndexReader::open(&partition.path, 3)
            .expect("open partition")
            .next()
            .expect("one record")
            .expect_err("row index is outside table");
        assert!(
            matches!(error, Error::TemporaryStorage(message) if message.contains("invalid row index"))
        );

        remove_spill_file(&partition.path).expect("remove partition");
        workspace.cleanup().expect("remove spill workspace");
        fs::remove_dir(parent).expect("remove spill test parent");
    }

    #[test]
    fn spilled_limit_bounds_peak_retained_groups() {
        use crate::storage::ColumnDef;

        let parent = spill_test_parent("bounded-limit-test");
        let mut table = Table::new(
            "totals".to_owned(),
            vec![
                ColumnDef {
                    name: "key".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "amount".to_owned(),
                    data_type: DataType::Int64,
                },
            ],
        )
        .expect("create table");
        for key in 0_i64..200 {
            table
                .insert_row(vec![Value::Int64(key), Value::Int64(200 - key)])
                .expect("insert row");
        }

        let aggregate_specs = [AggregateSpec {
            function: AggregateFunction::Sum,
            argument: Some(1),
            input_type: Some(DataType::Int64),
        }];
        let items = [
            ResolvedItem::Column {
                source: 0,
                group_position: Some(0),
            },
            ResolvedItem::Aggregate { state: 0 },
        ];
        let ordering = [ResolvedOrder {
            output: 1,
            descending: true,
        }];
        let options = DatabaseOptions {
            max_in_memory_groups: 1,
            temporary_directory: Some(parent.clone()),
        };
        let group_columns = [0];
        let plan = GroupedExecutionPlan {
            group_columns: &group_columns,
            aggregate_specs: &aggregate_specs,
            items: &items,
            ordering: &ordering,
            limit: Some(1),
            options: &options,
        };
        let matching_rows = (0..table.row_count()).collect::<Vec<_>>();

        let spilled =
            execute_spilled_grouped(&table, &matching_rows, &plan).expect("execute bounded spill");

        assert_eq!(spilled.peak_retained_groups, 2);
        assert_eq!(
            spilled.grouped.project(&[0], &items),
            vec![vec![Value::Int64(0), Value::Int64(200)]]
        );
        assert_eq!(fs::read_dir(&parent).expect("read parent").count(), 0);
        fs::remove_dir(parent).expect("remove spill test parent");
    }
}
