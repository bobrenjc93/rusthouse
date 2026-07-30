use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
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

/// Default maximum number of row indices held in one in-memory sort buffer.
pub const DEFAULT_MAX_IN_MEMORY_SORT_ROWS: usize = 65_536;

/// Execution settings for a [`Database`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseOptions {
    /// Maximum row indices held in one sort run or bounded top-k heap.
    pub max_in_memory_sort_rows: usize,
    /// Parent directory for per-query ORDER BY spill directories.
    pub temporary_directory: Option<PathBuf>,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            max_in_memory_sort_rows: DEFAULT_MAX_IN_MEMORY_SORT_ROWS,
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
        if self.options.max_in_memory_sort_rows == 0 {
            return Err(Error::InvalidQuery(
                "max_in_memory_sort_rows must be at least 1".to_owned(),
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

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;
        let matching_rows = || {
            (0..table.row_count()).filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
        };

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                matching_rows(),
                &group_columns,
                &aggregate_specs,
                table.row_count(),
            )?;
            sort_and_project(
                (0..grouped.len()).map(Ok),
                select.limit,
                grouped.len(),
                &self.options,
                |left, right| compare_grouped_rows(left, right, &grouped, &items, &ordering),
                |group| grouped.project_row(group, &items),
            )?
            .into_values(self.options.max_in_memory_sort_rows)
        } else if ordering.is_empty() {
            execute_projection(
                table,
                matching_rows().take(select.limit.unwrap_or(usize::MAX)),
                &items,
            )
        } else {
            sort_and_project(
                matching_rows().map(Ok),
                select.limit,
                table.row_count(),
                &self.options,
                |left, right| compare_source_rows(left, right, table, &items, &ordering),
                |row| project_source_row(table, row, &items),
            )?
            .into_values(self.options.max_in_memory_sort_rows)
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
    matching_rows: impl IntoIterator<Item = usize>,
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .into_iter()
        .map(|row| project_source_row(table, row, items))
        .collect()
}

fn project_source_row(table: &Table, row: usize, items: &[ResolvedItem]) -> Vec<Value> {
    items
        .iter()
        .map(|item| match item {
            ResolvedItem::Column { source, .. } => table.columns()[*source].value(row),
            ResolvedItem::Aggregate { .. } => {
                unreachable!("projection does not contain aggregates")
            }
        })
        .collect()
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: impl IntoIterator<Item = usize>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    row_count_hint: usize,
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_columns.len(), row_count_hint);
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = row_count_hint.min(1_024);
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

    for row in matching_rows {
        let (group, inserted) = groups.find_or_insert(table, group_columns, row, group_count);
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
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| table.columns()[*column].value_ref(row))
                    .collect::<Vec<_>>();
                find_or_insert_group(groups, &key, next_group)
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
) -> (usize, bool) {
    if let Some(group) = groups.get(key) {
        (*group, false)
    } else {
        groups.insert(key.into(), next_group);
        (next_group, true)
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

    fn project_row(&self, group: usize, items: &[ResolvedItem]) -> Vec<Value> {
        items
            .iter()
            .map(|item| match item {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => self.keys[group].value(*position).to_owned(),
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Aggregate { state } => self.aggregates[*state][group].clone(),
            })
            .collect()
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

fn compare_source_rows(
    left: usize,
    right: usize,
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Ordering {
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
}

fn compare_grouped_rows(
    left: usize,
    right: usize,
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Ordering {
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
}

const MAX_MERGE_FAN_IN: usize = 32;
const MAX_RUN_LEVELS: usize = max_run_levels();
const MAX_LIVE_SORT_RUNS: usize = MAX_RUN_LEVELS * MAX_MERGE_FAN_IN + 1;
const RUN_READER_BUFFER_RECORDS: usize = 128;
static NEXT_SORT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const fn max_run_levels() -> usize {
    let mut remaining_runs = usize::MAX;
    let mut levels = 0;
    while remaining_runs > 0 {
        levels += 1;
        remaining_runs /= MAX_MERGE_FAN_IN;
    }
    levels
}

struct SortedOutput<T> {
    values: Vec<T>,
    statistics: SortStatistics,
}

impl<T> SortedOutput<T> {
    fn into_values(self, max_in_memory_sort_rows: usize) -> Vec<T> {
        debug_assert!(self.statistics.peak_run_rows <= max_in_memory_sort_rows);
        debug_assert!(self.statistics.peak_merge_heads <= MAX_MERGE_FAN_IN);
        debug_assert!(self.statistics.peak_live_runs <= MAX_LIVE_SORT_RUNS);
        self.values
    }
}

#[derive(Debug, Default)]
struct SortStatistics {
    peak_run_rows: usize,
    peak_merge_heads: usize,
    peak_live_runs: usize,
}

fn sort_and_project<T>(
    indices: impl Iterator<Item = Result<usize>>,
    limit: Option<usize>,
    row_limit: usize,
    options: &DatabaseOptions,
    compare: impl Fn(usize, usize) -> Ordering,
    project: impl Fn(usize) -> T,
) -> Result<SortedOutput<T>> {
    let mut statistics = SortStatistics::default();
    if let Some(limit) = limit.filter(|limit| *limit <= options.max_in_memory_sort_rows) {
        let indices = select_top_k(
            indices,
            limit,
            options.max_in_memory_sort_rows,
            &compare,
            &mut statistics,
        )?;
        return Ok(SortedOutput {
            values: indices.into_iter().map(project).collect(),
            statistics,
        });
    }

    let mut buffer = Vec::with_capacity(options.max_in_memory_sort_rows.min(1_024));
    let mut workspace = None;
    let mut runs = RunInventory::new();
    for index in indices {
        let index = index?;
        if buffer.len() == options.max_in_memory_sort_rows {
            let workspace = match workspace.as_mut() {
                Some(workspace) => workspace,
                None => {
                    workspace.insert(SortWorkspace::new(options.temporary_directory.as_deref())?)
                }
            };
            let run = write_sorted_run(workspace, &mut buffer, &compare)?;
            runs.push(workspace, run, row_limit, &compare, &mut statistics)?;
        }
        buffer.push(index);
        statistics.peak_run_rows = statistics.peak_run_rows.max(buffer.len());
    }

    let Some(mut workspace) = workspace else {
        buffer.sort_unstable_by(|left, right| compare(*left, *right));
        if let Some(limit) = limit {
            buffer.truncate(limit);
        }
        return Ok(SortedOutput {
            values: buffer.into_iter().map(project).collect(),
            statistics,
        });
    };

    if !buffer.is_empty() {
        let run = write_sorted_run(&mut workspace, &mut buffer, &compare)?;
        runs.push(&mut workspace, run, row_limit, &compare, &mut statistics)?;
    }
    let runs = collapse_runs(
        &mut workspace,
        runs.into_runs(),
        &compare,
        &mut statistics,
        row_limit,
    )?;

    let output_limit = limit.unwrap_or(usize::MAX);
    let mut values = Vec::with_capacity(output_limit.min(1_024));
    merge_run_indices(&runs, row_limit, &compare, &mut statistics, |index| {
        if values.len() == output_limit {
            return Ok(false);
        }
        values.push(project(index));
        Ok(values.len() < output_limit)
    })?;
    statistics.peak_live_runs = workspace.peak_live_runs();
    workspace.cleanup()?;
    Ok(SortedOutput { values, statistics })
}

fn select_top_k(
    indices: impl Iterator<Item = Result<usize>>,
    limit: usize,
    max_in_memory_rows: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
    statistics: &mut SortStatistics,
) -> Result<Vec<usize>> {
    if limit > max_in_memory_rows / 2 {
        return select_top_k_heap(indices, limit, compare, statistics);
    }

    let mut selected = Vec::with_capacity(max_in_memory_rows.min(1_024));
    for index in indices {
        if selected.len() == max_in_memory_rows {
            retain_top_k(&mut selected, limit, compare);
        }
        selected.push(index?);
        statistics.peak_run_rows = statistics.peak_run_rows.max(selected.len());
    }
    sort_and_limit(&mut selected, limit, compare);
    Ok(selected)
}

fn select_top_k_heap(
    indices: impl Iterator<Item = Result<usize>>,
    limit: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
    statistics: &mut SortStatistics,
) -> Result<Vec<usize>> {
    let mut heap = Vec::with_capacity(limit.min(1_024));
    for index in indices {
        let index = index?;
        if heap.len() < limit {
            push_max_heap(&mut heap, index, compare);
            statistics.peak_run_rows = statistics.peak_run_rows.max(heap.len());
        } else if compare(index, heap[0]) == Ordering::Less {
            heap[0] = index;
            sift_down_max(&mut heap, 0, compare);
        }
    }
    heap.sort_unstable_by(|left, right| compare(*left, *right));
    Ok(heap)
}

fn push_max_heap(heap: &mut Vec<usize>, index: usize, compare: &impl Fn(usize, usize) -> Ordering) {
    heap.push(index);
    let mut child = heap.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if compare(heap[parent], heap[child]) != Ordering::Less {
            break;
        }
        heap.swap(parent, child);
        child = parent;
    }
}

fn sift_down_max(
    heap: &mut [usize],
    mut parent: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
) {
    loop {
        let left = parent * 2 + 1;
        if left >= heap.len() {
            return;
        }
        let right = left + 1;
        let largest = if right < heap.len() && compare(heap[left], heap[right]) == Ordering::Less {
            right
        } else {
            left
        };
        if compare(heap[parent], heap[largest]) != Ordering::Less {
            return;
        }
        heap.swap(parent, largest);
        parent = largest;
    }
}

fn retain_top_k(
    indices: &mut Vec<usize>,
    limit: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
) {
    if limit < indices.len() {
        indices.select_nth_unstable_by(limit, |left, right| compare(*left, *right));
        indices.truncate(limit);
    }
}

fn sort_and_limit(
    indices: &mut Vec<usize>,
    limit: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
) {
    retain_top_k(indices, limit, compare);
    indices.sort_unstable_by(|left, right| compare(*left, *right));
}

fn write_sorted_run(
    workspace: &mut SortWorkspace,
    buffer: &mut Vec<usize>,
    compare: &impl Fn(usize, usize) -> Ordering,
) -> Result<SortRun> {
    buffer.sort_unstable_by(|left, right| compare(*left, *right));
    let (mut run, file) = workspace.create_run()?;
    let mut writer = BufWriter::new(file);
    for index in buffer.drain(..) {
        write_row_index(&mut writer, index, &run.path)?;
        run.row_count += 1;
    }
    writer
        .flush()
        .map_err(|error| temporary_storage_error("flush sort run", &run.path, error))?;
    Ok(run)
}

struct RunInventory {
    levels: Vec<Vec<SortRun>>,
}

impl RunInventory {
    fn new() -> Self {
        Self {
            levels: Vec::with_capacity(MAX_RUN_LEVELS),
        }
    }

    fn push(
        &mut self,
        workspace: &mut SortWorkspace,
        mut run: SortRun,
        row_limit: usize,
        compare: &impl Fn(usize, usize) -> Ordering,
        statistics: &mut SortStatistics,
    ) -> Result<()> {
        for level in 0..MAX_RUN_LEVELS {
            if level == self.levels.len() {
                self.levels.push(Vec::with_capacity(MAX_MERGE_FAN_IN));
            }
            self.levels[level].push(run);
            if self.levels[level].len() < MAX_MERGE_FAN_IN {
                return Ok(());
            }

            let batch = std::mem::take(&mut self.levels[level]);
            run = merge_runs_to_file(workspace, &batch, row_limit, compare, statistics)?;
            for input in batch {
                workspace.remove_run(&input.path)?;
            }
        }

        Err(Error::TemporaryStorage(
            "sort run inventory exceeded the platform row-index limit".to_owned(),
        ))
    }

    fn into_runs(self) -> Vec<SortRun> {
        let retained = self.levels.iter().map(Vec::len).sum();
        let mut runs = Vec::with_capacity(retained);
        for level in self.levels {
            runs.extend(level);
        }
        runs
    }
}

fn collapse_runs(
    workspace: &mut SortWorkspace,
    mut runs: Vec<SortRun>,
    compare: &impl Fn(usize, usize) -> Ordering,
    statistics: &mut SortStatistics,
    row_limit: usize,
) -> Result<Vec<SortRun>> {
    while runs.len() > MAX_MERGE_FAN_IN {
        let old_runs = std::mem::take(&mut runs);
        let mut pending = old_runs.into_iter();
        loop {
            let mut batch = pending.by_ref().take(MAX_MERGE_FAN_IN).collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            if batch.len() == 1 {
                runs.push(batch.pop().expect("single run"));
                continue;
            }
            let merged = merge_runs_to_file(workspace, &batch, row_limit, compare, statistics)?;
            for run in batch {
                workspace.remove_run(&run.path)?;
            }
            runs.push(merged);
        }
    }
    Ok(runs)
}

fn merge_runs_to_file(
    workspace: &mut SortWorkspace,
    runs: &[SortRun],
    row_limit: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
    statistics: &mut SortStatistics,
) -> Result<SortRun> {
    let (mut output, file) = workspace.create_run()?;
    let mut writer = BufWriter::new(file);
    merge_run_indices(runs, row_limit, compare, statistics, |index| {
        write_row_index(&mut writer, index, &output.path)?;
        output.row_count += 1;
        Ok(true)
    })?;
    writer
        .flush()
        .map_err(|error| temporary_storage_error("flush merged sort run", &output.path, error))?;
    Ok(output)
}

#[derive(Clone, Copy)]
struct MergeHead {
    index: usize,
    run: usize,
}

fn merge_run_indices(
    runs: &[SortRun],
    row_limit: usize,
    compare: &impl Fn(usize, usize) -> Ordering,
    statistics: &mut SortStatistics,
    mut emit: impl FnMut(usize) -> Result<bool>,
) -> Result<()> {
    debug_assert!(runs.len() <= MAX_MERGE_FAN_IN);
    let mut readers = runs
        .iter()
        .map(|run| RowIndexReader::open(run, row_limit))
        .collect::<Result<Vec<_>>>()?;
    let mut heap = Vec::with_capacity(readers.len());
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(index) = reader.next().transpose()? {
            push_merge_head(&mut heap, MergeHead { index, run }, compare);
        }
    }
    statistics.peak_merge_heads = statistics.peak_merge_heads.max(heap.len());

    while let Some(head) = pop_merge_head(&mut heap, compare) {
        if !emit(head.index)? {
            break;
        }
        if let Some(index) = readers[head.run].next().transpose()? {
            push_merge_head(
                &mut heap,
                MergeHead {
                    index,
                    run: head.run,
                },
                compare,
            );
        }
        statistics.peak_merge_heads = statistics.peak_merge_heads.max(heap.len());
    }
    Ok(())
}

fn compare_merge_heads(
    left: MergeHead,
    right: MergeHead,
    compare: &impl Fn(usize, usize) -> Ordering,
) -> Ordering {
    compare(left.index, right.index).then_with(|| left.run.cmp(&right.run))
}

fn push_merge_head(
    heap: &mut Vec<MergeHead>,
    head: MergeHead,
    compare: &impl Fn(usize, usize) -> Ordering,
) {
    heap.push(head);
    let mut child = heap.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if compare_merge_heads(heap[parent], heap[child], compare) != Ordering::Greater {
            break;
        }
        heap.swap(parent, child);
        child = parent;
    }
}

fn pop_merge_head(
    heap: &mut Vec<MergeHead>,
    compare: &impl Fn(usize, usize) -> Ordering,
) -> Option<MergeHead> {
    if heap.is_empty() {
        return None;
    }
    let head = heap.swap_remove(0);
    let mut parent = 0;
    loop {
        let left = parent * 2 + 1;
        if left >= heap.len() {
            break;
        }
        let right = left + 1;
        let smallest = if right < heap.len()
            && compare_merge_heads(heap[right], heap[left], compare) == Ordering::Less
        {
            right
        } else {
            left
        };
        if compare_merge_heads(heap[parent], heap[smallest], compare) != Ordering::Greater {
            break;
        }
        heap.swap(parent, smallest);
        parent = smallest;
    }
    Some(head)
}

#[derive(Debug)]
struct SortRun {
    path: PathBuf,
    row_count: usize,
}

struct SortWorkspace {
    path: PathBuf,
    next_file: u64,
    live_runs: usize,
    peak_live_runs: usize,
    active: bool,
}

impl SortWorkspace {
    fn new(configured_parent: Option<&Path>) -> Result<Self> {
        let parent = configured_parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
        fs::create_dir_all(&parent).map_err(|error| {
            temporary_storage_error("create temporary directory", &parent, error)
        })?;

        loop {
            let sequence = NEXT_SORT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
            let path = parent.join(format!("rusthouse-sort-{}-{sequence}", std::process::id()));
            match create_private_directory(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        next_file: 0,
                        live_runs: 0,
                        peak_live_runs: 0,
                        active: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(temporary_storage_error(
                        "create sort workspace",
                        &path,
                        error,
                    ));
                }
            }
        }
    }

    fn create_run(&mut self) -> Result<(SortRun, File)> {
        if self.live_runs >= MAX_LIVE_SORT_RUNS {
            return Err(Error::TemporaryStorage(format!(
                "sort requires more than {MAX_LIVE_SORT_RUNS} live run files"
            )));
        }
        let path = self.path.join(format!("run-{}.bin", self.next_file));
        self.next_file += 1;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .map_err(|error| temporary_storage_error("create sort run", &path, error))?;
        self.live_runs += 1;
        self.peak_live_runs = self.peak_live_runs.max(self.live_runs);
        Ok((SortRun { path, row_count: 0 }, file))
    }

    fn remove_run(&mut self, path: &Path) -> Result<()> {
        fs::remove_file(path)
            .map_err(|error| temporary_storage_error("remove sort run", path, error))?;
        self.live_runs = self
            .live_runs
            .checked_sub(1)
            .expect("only live sort runs are removed");
        Ok(())
    }

    fn peak_live_runs(&self) -> usize {
        self.peak_live_runs
    }

    fn cleanup(mut self) -> Result<()> {
        fs::remove_dir_all(&self.path)
            .map_err(|error| temporary_storage_error("remove sort workspace", &self.path, error))?;
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

impl Drop for SortWorkspace {
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
    fn open(run: &SortRun, row_limit: usize) -> Result<Self> {
        let file = File::open(&run.path)
            .map_err(|error| temporary_storage_error("open sort run", &run.path, error))?;
        let byte_len = file
            .metadata()
            .map_err(|error| temporary_storage_error("inspect sort run", &run.path, error))?
            .len();
        if byte_len % 8 != 0 {
            return Err(Error::TemporaryStorage(format!(
                "sort run '{}' has a partial row index",
                run.path.display()
            )));
        }
        let record_count = usize::try_from(byte_len / 8).map_err(|_| {
            Error::TemporaryStorage(format!(
                "sort run '{}' is too large for this platform",
                run.path.display()
            ))
        })?;
        if record_count != run.row_count {
            return Err(Error::TemporaryStorage(format!(
                "sort run '{}' changed length while sorting",
                run.path.display()
            )));
        }
        Ok(Self {
            reader: BufReader::with_capacity(RUN_READER_BUFFER_RECORDS * 8, file),
            remaining: record_count,
            row_limit,
            path: run.path.clone(),
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
                "read sort run",
                &self.path,
                error,
            )));
        }
        self.remaining -= 1;
        let row = match usize::try_from(u64::from_le_bytes(bytes)) {
            Ok(row) if row < self.row_limit => row,
            Ok(_) | Err(_) => {
                return Some(Err(Error::TemporaryStorage(format!(
                    "sort run '{}' contains an invalid row index",
                    self.path.display()
                ))));
            }
        };
        Some(Ok(row))
    }
}

fn write_row_index(writer: &mut impl Write, index: usize, path: &Path) -> Result<()> {
    let index = u64::try_from(index).map_err(|_| {
        Error::TemporaryStorage("row index does not fit in a sort run record".to_owned())
    })?;
    writer
        .write_all(&index.to_le_bytes())
        .map_err(|error| temporary_storage_error("write sort run", path, error))
}

fn temporary_storage_error(action: &str, path: &Path, error: io::Error) -> Error {
    Error::TemporaryStorage(format!("{action} '{}': {error}", path.display()))
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

    fn test_temporary_directory(label: &str) -> PathBuf {
        let sequence = NEXT_SORT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-engine-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test temporary directory");
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

    #[test]
    fn external_sort_buffers_remain_bounded_at_million_row_scale() {
        const ROW_COUNT: usize = 1_000_000;
        const MAX_IN_MEMORY_ROWS: usize = 4_096;
        let temporary_directory = test_temporary_directory("million-sort");
        let options = DatabaseOptions {
            max_in_memory_sort_rows: MAX_IN_MEMORY_ROWS,
            temporary_directory: Some(temporary_directory.clone()),
        };

        let output = sort_and_project(
            (0..ROW_COUNT).rev().map(Ok),
            None,
            ROW_COUNT,
            &options,
            |left, right| left.cmp(&right),
            |index| index,
        )
        .expect("external sort succeeds");

        assert_eq!(output.values.len(), ROW_COUNT);
        assert_eq!(output.values[0], 0);
        assert_eq!(output.values[ROW_COUNT - 1], ROW_COUNT - 1);
        assert!(output.statistics.peak_run_rows <= MAX_IN_MEMORY_ROWS);
        assert!(output.statistics.peak_merge_heads <= MAX_MERGE_FAN_IN);
        assert!(output.statistics.peak_live_runs <= MAX_LIVE_SORT_RUNS);

        let top = sort_and_project(
            (0..ROW_COUNT).rev().map(Ok),
            Some(25),
            ROW_COUNT,
            &options,
            |left, right| left.cmp(&right),
            |index| index,
        )
        .expect("bounded top-k succeeds");
        assert_eq!(top.values, (0..25).collect::<Vec<_>>());
        assert!(top.statistics.peak_run_rows <= MAX_IN_MEMORY_ROWS);
        assert_eq!(top.statistics.peak_merge_heads, 0);
        assert_eq!(
            fs::read_dir(&temporary_directory)
                .expect("read temporary directory")
                .count(),
            0
        );
        fs::remove_dir(temporary_directory).expect("remove test temporary directory");
    }

    #[test]
    fn minimum_sort_cap_incrementally_bounds_live_run_files() {
        const ROW_COUNT: usize = MAX_MERGE_FAN_IN * MAX_MERGE_FAN_IN + 1;
        let temporary_directory = test_temporary_directory("minimum-cap-sort");
        let options = DatabaseOptions {
            max_in_memory_sort_rows: 1,
            temporary_directory: Some(temporary_directory.clone()),
        };

        let output = sort_and_project(
            (0..ROW_COUNT).rev().map(Ok),
            None,
            ROW_COUNT,
            &options,
            |left, right| left.cmp(&right),
            |index| index,
        )
        .expect("minimum-cap external sort succeeds");

        assert_eq!(output.values, (0..ROW_COUNT).collect::<Vec<_>>());
        assert!(output.statistics.peak_live_runs >= MAX_MERGE_FAN_IN);
        assert!(
            output.statistics.peak_live_runs <= MAX_MERGE_FAN_IN * 2,
            "peak live runs was {}",
            output.statistics.peak_live_runs
        );
        assert!(output.statistics.peak_live_runs <= MAX_LIVE_SORT_RUNS);
        assert_eq!(
            fs::read_dir(&temporary_directory)
                .expect("read temporary directory")
                .count(),
            0
        );
        fs::remove_dir(temporary_directory).expect("remove test temporary directory");
    }

    #[test]
    fn a_stream_error_after_spilling_removes_the_workspace() {
        let temporary_directory = test_temporary_directory("failed-sort");
        let options = DatabaseOptions {
            max_in_memory_sort_rows: 2,
            temporary_directory: Some(temporary_directory.clone()),
        };
        let expected = Error::InvalidQuery("forced scan failure".to_owned());
        let indices = (0..5).map(Ok).chain(std::iter::once(Err(expected.clone())));

        let error = match sort_and_project(
            indices,
            None,
            5,
            &options,
            |left, right| left.cmp(&right),
            |index| index,
        ) {
            Ok(_) => panic!("stream failure should be returned"),
            Err(error) => error,
        };

        assert_eq!(error, expected);
        assert_eq!(
            fs::read_dir(&temporary_directory)
                .expect("read temporary directory")
                .count(),
            0
        );
        fs::remove_dir(temporary_directory).expect("remove test temporary directory");
    }
}
