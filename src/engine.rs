use std::cmp::Ordering;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::thread;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

const PARALLEL_SCAN_THRESHOLD: usize = 32 * 1_024;
const SCAN_PARTITION_ROWS: usize = 16 * 1_024;

/// A reusable in-memory SQL database.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    query_parallelism: NonZeroUsize,
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
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Construct an empty database with an explicit per-query worker limit.
    pub fn with_query_parallelism(query_parallelism: usize) -> Result<Self> {
        let mut database = Self::new();
        database.set_query_parallelism(query_parallelism)?;
        Ok(database)
    }

    /// Set the maximum number of workers used by one aggregate query.
    pub fn set_query_parallelism(&mut self, query_parallelism: usize) -> Result<()> {
        self.query_parallelism = NonZeroUsize::new(query_parallelism).ok_or_else(|| {
            Error::InvalidQuery("query parallelism must be at least 1".to_owned())
        })?;
        Ok(())
    }

    /// Return the configured per-query worker limit.
    #[must_use]
    pub fn query_parallelism(&self) -> usize {
        self.query_parallelism.get()
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
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

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                predicate.as_ref(),
                &group_columns,
                &aggregate_specs,
                self.query_parallelism,
            )?;
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
            let mut matching_rows = (0..table.row_count())
                .filter(|row| {
                    predicate
                        .as_ref()
                        .is_none_or(|predicate| predicate.evaluate(table, *row))
                })
                .collect::<Vec<_>>();
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            execute_projection(table, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

impl Default for Database {
    fn default() -> Self {
        let query_parallelism = thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Self {
            catalog: Catalog::default(),
            query_parallelism,
        }
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

fn requires_ordered_float_replay(spec: &AggregateSpec) -> bool {
    spec.input_type == Some(DataType::Float64)
        && matches!(
            spec.function,
            AggregateFunction::Sum | AggregateFunction::Avg
        )
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

fn execute_grouped<'a>(
    table: &'a Table,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    query_parallelism: NonZeroUsize,
) -> Result<GroupedData<'a>> {
    let ranges = scan_ranges(table.row_count());
    // A Float64 partition total cannot reproduce row-ordered overflow after
    // merge, so partitioned SUM/AVG retain their input order for replay.
    let replay_float_inputs =
        ranges.len() > 1 && aggregate_specs.iter().any(requires_ordered_float_replay);
    let partitions = if ranges.len() == 1 || query_parallelism.get() == 1 {
        ranges
            .into_iter()
            .map(|range| {
                scan_grouped_partition(
                    table,
                    range,
                    predicate,
                    group_columns,
                    aggregate_specs,
                    replay_float_inputs,
                )
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        scan_grouped_partitions_parallel(
            table,
            &ranges,
            predicate,
            group_columns,
            aggregate_specs,
            query_parallelism.get(),
            replay_float_inputs,
        )?
    };

    merge_grouped_partitions(table, partitions, group_columns, aggregate_specs)
}

fn scan_ranges(row_count: usize) -> Vec<Range<usize>> {
    if row_count < PARALLEL_SCAN_THRESHOLD {
        return std::iter::once(0..row_count).collect();
    }

    (0..row_count)
        .step_by(SCAN_PARTITION_ROWS)
        .map(|start| start..start.saturating_add(SCAN_PARTITION_ROWS).min(row_count))
        .collect()
}

fn scan_grouped_partitions_parallel<'a>(
    table: &'a Table,
    ranges: &[Range<usize>],
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    query_parallelism: usize,
    replay_float_inputs: bool,
) -> Result<Vec<GroupedPartition<'a>>> {
    let worker_count = query_parallelism.min(ranges.len());

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        let base_ranges = ranges.len() / worker_count;
        let extra_ranges = ranges.len() % worker_count;
        for worker in 0..worker_count {
            let start = worker * base_ranges + worker.min(extra_ranges);
            let end = start + base_ranges + usize::from(worker < extra_ranges);
            let worker_ranges = &ranges[start..end];
            handles.push(scope.spawn(move || {
                worker_ranges
                    .iter()
                    .cloned()
                    .map(|range| {
                        scan_grouped_partition(
                            table,
                            range,
                            predicate,
                            group_columns,
                            aggregate_specs,
                            replay_float_inputs,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            }));
        }

        let mut partitions = Vec::with_capacity(ranges.len());
        for handle in handles {
            let worker_partitions = handle.join().map_err(|_| {
                Error::InvalidQuery("parallel aggregate worker panicked".to_owned())
            })??;
            partitions.extend(worker_partitions);
        }
        Ok(partitions)
    })
}

fn scan_grouped_partition<'a>(
    table: &'a Table,
    range: Range<usize>,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    replay_float_inputs: bool,
) -> Result<GroupedPartition<'a>> {
    let range_len = range.len();
    let mut groups = GroupIndex::new(group_columns.len(), range_len);
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = range_len.min(1_024);
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
    let mut replay_rows = if replay_float_inputs {
        Vec::with_capacity(range_len)
    } else {
        Vec::new()
    };

    for row in range {
        if predicate.is_some_and(|predicate| !predicate.evaluate(table, row)) {
            continue;
        }
        let (group, inserted) = groups.find_or_insert(table, group_columns, row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        if replay_float_inputs {
            replay_rows.push((group, row));
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            if replay_float_inputs && requires_ordered_float_replay(spec) {
                continue;
            }
            states[group].update(spec, table, row)?;
        }
    }

    Ok(GroupedPartition {
        keys: groups.into_keys(group_count),
        aggregate_states,
        replay_rows,
        replays_float_inputs: replay_float_inputs,
    })
}

fn merge_grouped_partitions<'a>(
    table: &'a Table,
    partitions: Vec<GroupedPartition<'a>>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_columns.len(), table.row_count());
    let mut group_count = usize::from(group_columns.is_empty());
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::new();
            if group_columns.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for partition in partitions {
        let GroupedPartition {
            keys,
            aggregate_states: local_aggregate_states,
            replay_rows,
            replays_float_inputs,
        } = partition;
        let mut global_groups = Vec::with_capacity(keys.len());
        for key in &keys {
            let (group, inserted) = groups.find_or_insert_key(key, group_count);
            if inserted {
                group_count += 1;
                for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                    states.push(AggregateState::new(spec));
                }
            }
            global_groups.push(group);
        }

        for ((states, local_states), spec) in aggregate_states
            .iter_mut()
            .zip(local_aggregate_states)
            .zip(aggregate_specs)
        {
            if replays_float_inputs && requires_ordered_float_replay(spec) {
                continue;
            }
            for (state, group) in local_states.into_iter().zip(&global_groups) {
                states[*group].merge(state)?;
            }
        }

        for (local_group, row) in replay_rows {
            let global_group = global_groups[local_group];
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                if requires_ordered_float_replay(spec) {
                    states[global_group].update(spec, table, row)?;
                }
            }
        }
    }

    let aggregates = aggregate_states
        .into_iter()
        .map(|states| {
            states
                .into_iter()
                .map(AggregateState::finish)
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GroupedData {
        keys: groups.into_keys(group_count),
        aggregates,
    })
}

#[derive(Debug)]
struct GroupedPartition<'a> {
    keys: Vec<GroupKey<'a>>,
    aggregate_states: Vec<Vec<AggregateState>>,
    replay_rows: Vec<(usize, usize)>,
    replays_float_inputs: bool,
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

    fn find_or_insert_key(&mut self, key: &GroupKey<'a>, next_group: usize) -> (usize, bool) {
        match (self, key) {
            (Self::Global, GroupKey::Empty) => (0, false),
            (Self::One(groups), GroupKey::One(key)) => {
                if let Some(group) = groups.get(key) {
                    (*group, false)
                } else {
                    groups.insert(*key, next_group);
                    (next_group, true)
                }
            }
            (Self::Multiple(groups), GroupKey::Multiple(key)) => {
                find_or_insert_group(groups, key, next_group)
            }
            _ => unreachable!("group index and key shapes match"),
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

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt(IntSum),
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
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => {
                Self::SumInt(IntSum::default())
            }
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
                sum.update(values[row])?;
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

    fn merge(&mut self, other: Self) -> Result<()> {
        match (self, other) {
            (Self::Count(left), Self::Count(right)) => {
                *left = left
                    .checked_add(right)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            (Self::SumInt(left), Self::SumInt(right)) => left.merge(right)?,
            (Self::SumFloat(left), Self::SumFloat(right)) => {
                *left += right;
                if !left.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            (Self::Min(left), Self::Min(right)) => {
                if let Some(candidate) = right
                    && left
                        .as_ref()
                        .is_none_or(|current| candidate.as_ref() < current.as_ref())
                {
                    *left = Some(candidate);
                }
            }
            (Self::Max(left), Self::Max(right)) => {
                if let Some(candidate) = right
                    && left
                        .as_ref()
                        .is_none_or(|current| candidate.as_ref() > current.as_ref())
                {
                    *left = Some(candidate);
                }
            }
            (
                Self::AvgInt {
                    sum: left_sum,
                    count: left_count,
                },
                Self::AvgInt {
                    sum: right_sum,
                    count: right_count,
                },
            ) => {
                *left_sum = left_sum
                    .checked_add(right_sum)
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            (
                Self::AvgFloat {
                    sum: left_sum,
                    count: left_count,
                },
                Self::AvgFloat {
                    sum: right_sum,
                    count: right_count,
                },
            ) => {
                *left_sum += right_sum;
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
                if !left_sum.is_finite() {
                    return Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()));
                }
            }
            _ => unreachable!("states for the same aggregate have matching variants"),
        }
        Ok(())
    }

    fn finish(self) -> Result<Value> {
        match self {
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt(value) => value.finish().map(Value::Int64),
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

#[derive(Debug, Default)]
struct IntSum {
    total: i128,
    min_prefix: i128,
    max_prefix: i128,
}

impl IntSum {
    fn update(&mut self, value: i64) -> Result<()> {
        self.total = self
            .total
            .checked_add(i128::from(value))
            .ok_or_else(sum_int_overflow)?;
        self.min_prefix = self.min_prefix.min(self.total);
        self.max_prefix = self.max_prefix.max(self.total);
        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        let translated_min = self
            .total
            .checked_add(other.min_prefix)
            .ok_or_else(sum_int_overflow)?;
        let translated_max = self
            .total
            .checked_add(other.max_prefix)
            .ok_or_else(sum_int_overflow)?;
        self.total = self
            .total
            .checked_add(other.total)
            .ok_or_else(sum_int_overflow)?;
        self.min_prefix = self.min_prefix.min(translated_min);
        self.max_prefix = self.max_prefix.max(translated_max);
        Ok(())
    }

    fn finish(self) -> Result<i64> {
        if self.min_prefix < i128::from(i64::MIN) || self.max_prefix > i128::from(i64::MAX) {
            return Err(sum_int_overflow());
        }
        i64::try_from(self.total).map_err(|_| sum_int_overflow())
    }
}

fn sum_int_overflow() -> Error {
    Error::NumericOverflow("SUM(Int64)".to_owned())
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
}
