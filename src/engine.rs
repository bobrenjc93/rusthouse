use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, hash_map::Entry};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, GroupBy, Operand, OrderBy,
    Predicate, Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

const MAX_GROUPING_SETS: usize = 128;
const MAX_GROUPING_ARGUMENTS: usize = 63;

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
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

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();

        let grouping = resolve_grouping(table, select.group_by.as_ref())?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &grouping)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = grouping.explicit || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &grouping, &aggregate_specs)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                table,
                &grouping,
                &items,
                &ordering,
                select.limit,
            );
            grouped.project(&selected_groups, table, &grouping, &items)
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
    Grouping {
        group_positions: Box<[usize]>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
}

#[derive(Debug)]
struct ResolvedGrouping {
    columns: Vec<usize>,
    sets: Vec<ResolvedGroupingSet>,
    explicit: bool,
}

#[derive(Debug)]
struct ResolvedGroupingSet {
    columns: Vec<usize>,
    positions: Vec<Option<usize>>,
}

fn resolve_grouping(table: &Table, requested: Option<&GroupBy>) -> Result<ResolvedGrouping> {
    let Some(requested) = requested else {
        return Ok(build_grouping(Vec::new(), vec![Vec::new()], false));
    };

    match requested {
        GroupBy::Columns(names) => {
            let columns = resolve_unique_group_columns(table, names)?;
            let all = (0..columns.len()).collect();
            Ok(build_grouping(columns, vec![all], true))
        }
        GroupBy::Rollup(names) => {
            let Some(set_count) = names.len().checked_add(1) else {
                return Err(too_many_grouping_sets("ROLLUP"));
            };
            enforce_grouping_set_count("ROLLUP", set_count)?;
            let columns = resolve_unique_group_columns(table, names)?;
            let sets = (0..set_count)
                .rev()
                .map(|length| (0..length).collect())
                .collect();
            Ok(build_grouping(columns, sets, true))
        }
        GroupBy::Cube(names) => {
            let set_count = if names.len() >= usize::BITS as usize {
                None
            } else {
                Some(1_usize << names.len())
            };
            let Some(set_count) = set_count else {
                return Err(too_many_grouping_sets("CUBE"));
            };
            enforce_grouping_set_count("CUBE", set_count)?;
            let columns = resolve_unique_group_columns(table, names)?;
            let sets = (0..set_count)
                .rev()
                .map(|mask| {
                    (0..columns.len())
                        .filter(|position| {
                            let bit = columns.len() - position - 1;
                            mask & (1 << bit) != 0
                        })
                        .collect()
                })
                .collect();
            Ok(build_grouping(columns, sets, true))
        }
        GroupBy::GroupingSets(requested_sets) => {
            resolve_explicit_grouping_sets(table, requested_sets)
        }
    }
}

fn resolve_unique_group_columns(table: &Table, names: &[String]) -> Result<Vec<usize>> {
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

fn resolve_explicit_grouping_sets(
    table: &Table,
    requested_sets: &[Vec<String>],
) -> Result<ResolvedGrouping> {
    let mut columns = Vec::new();
    let mut column_positions = HashMap::new();
    let mut seen_sets = HashSet::new();
    let mut source_sets = Vec::with_capacity(requested_sets.len().min(MAX_GROUPING_SETS));
    for requested_set in requested_sets {
        let mut source_set = Vec::with_capacity(requested_set.len());
        for name in requested_set {
            let source = table.column_index(name)?;
            if source_set.contains(&source) {
                return Err(Error::InvalidQuery(format!(
                    "GROUP BY column '{name}' is listed more than once within a grouping set"
                )));
            }
            source_set.push(source);
        }

        let mut canonical_set = source_set.clone();
        canonical_set.sort_unstable();
        if seen_sets.insert(canonical_set.clone()) {
            enforce_grouping_set_count("GROUPING SETS", seen_sets.len())?;
            for source in source_set {
                if let Entry::Vacant(entry) = column_positions.entry(source) {
                    entry.insert(columns.len());
                    columns.push(source);
                }
            }
            source_sets.push(canonical_set);
        }
    }

    let position_sets = source_sets
        .into_iter()
        .map(|source_set| {
            let mut positions = source_set
                .into_iter()
                .map(|source| {
                    column_positions
                        .get(&source)
                        .copied()
                        .expect("grouping set columns form the grouping universe")
                })
                .collect::<Vec<_>>();
            positions.sort_unstable();
            positions
        })
        .collect();
    Ok(build_grouping(columns, position_sets, true))
}

fn build_grouping(
    columns: Vec<usize>,
    position_sets: Vec<Vec<usize>>,
    explicit: bool,
) -> ResolvedGrouping {
    debug_assert!(position_sets.len() <= MAX_GROUPING_SETS);
    let mut seen = HashSet::with_capacity(position_sets.len());
    let mut sets = Vec::new();
    for positions in position_sets {
        if seen.insert(positions.clone()) {
            let mut projected_positions = vec![None; columns.len()];
            let set_columns = positions
                .iter()
                .enumerate()
                .map(|(set_position, group_position)| {
                    projected_positions[*group_position] = Some(set_position);
                    columns[*group_position]
                })
                .collect();
            sets.push(ResolvedGroupingSet {
                columns: set_columns,
                positions: projected_positions,
            });
        }
    }
    ResolvedGrouping {
        columns,
        sets,
        explicit,
    }
}

fn enforce_grouping_set_count(construct: &str, count: usize) -> Result<()> {
    if count > MAX_GROUPING_SETS {
        Err(too_many_grouping_sets(construct))
    } else {
        Ok(())
    }
}

fn too_many_grouping_sets(construct: &str) -> Error {
    Error::InvalidQuery(format!(
        "{construct} exceeds the limit of {MAX_GROUPING_SETS} grouping sets"
    ))
}

fn resolve_select_items(
    table: &Table,
    requested: &[SelectItem],
    grouping: &ResolvedGrouping,
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
                    let group_position =
                        grouping.columns.iter().position(|column| *column == source);
                    if grouping.explicit && group_position.is_none() {
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
                let group_position = grouping.columns.iter().position(|column| *column == source);
                if (has_aggregate || grouping.explicit) && group_position.is_none() {
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
                let spec = AggregateSpec {
                    function: *function,
                    argument: argument_index,
                    input_type,
                };
                let state = aggregate_specs
                    .iter()
                    .position(|existing| *existing == spec);
                let state = state.unwrap_or_else(|| {
                    let state = aggregate_specs.len();
                    aggregate_specs.push(spec);
                    state
                });
                items.push(ResolvedItem::Aggregate { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: aggregate_output_type(*function, input_type),
                });
            }
            SelectItem::Grouping { arguments, alias } => {
                if !grouping.explicit {
                    return Err(Error::InvalidQuery(
                        "GROUPING(...) requires a GROUP BY clause".to_owned(),
                    ));
                }
                if arguments.len() > MAX_GROUPING_ARGUMENTS {
                    return Err(Error::InvalidQuery(format!(
                        "GROUPING(...) supports at most {MAX_GROUPING_ARGUMENTS} arguments"
                    )));
                }
                let mut group_positions = Vec::with_capacity(arguments.len());
                let mut argument_names = Vec::with_capacity(arguments.len());
                for name in arguments {
                    let source = table.column_index(name)?;
                    let Some(position) =
                        grouping.columns.iter().position(|column| *column == source)
                    else {
                        return Err(Error::InvalidQuery(format!(
                            "GROUPING column '{name}' must appear in GROUP BY"
                        )));
                    };
                    if group_positions.contains(&position) {
                        return Err(Error::InvalidQuery(format!(
                            "GROUPING column '{name}' is listed more than once"
                        )));
                    }
                    group_positions.push(position);
                    argument_names.push(table.schema()[source].name.clone());
                }
                items.push(ResolvedItem::Grouping {
                    group_positions: group_positions.into_boxed_slice(),
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("GROUPING({})", argument_names.join(", "))),
                    data_type: DataType::Int64,
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
                    ResolvedItem::Aggregate { .. } | ResolvedItem::Grouping { .. } => {
                        unreachable!("projection does not contain grouped expressions")
                    }
                })
                .collect()
        })
        .collect()
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    grouping: &ResolvedGrouping,
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
    let mut rows = Vec::new();
    let mut aggregates = vec![Vec::new(); aggregate_specs.len()];
    for (set_index, grouping_set) in grouping.sets.iter().enumerate() {
        let (keys, set_aggregates) =
            execute_grouping_set(table, matching_rows, &grouping_set.columns, aggregate_specs)?;
        let group_count = keys.len();
        rows.extend(keys.into_iter().map(|key| GroupedRow { key, set_index }));
        for (all_values, set_values) in aggregates.iter_mut().zip(set_aggregates) {
            debug_assert_eq!(set_values.len(), group_count);
            all_values.extend(set_values);
        }
    }
    Ok(GroupedData { rows, aggregates })
}

fn execute_grouping_set<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<(Vec<GroupKey<'a>>, Vec<Vec<Value>>)> {
    let mut groups = GroupIndex::new(group_columns.len(), matching_rows.len());
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = matching_rows.len().min(1_024);
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
        let (group, inserted) = groups.find_or_insert(table, group_columns, *row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, table, *row)?;
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
    Ok((keys, aggregates))
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
struct GroupedRow<'a> {
    key: GroupKey<'a>,
    set_index: usize,
}

#[derive(Debug)]
struct GroupedData<'a> {
    rows: Vec<GroupedRow<'a>>,
    aggregates: Vec<Vec<Value>>,
}

impl GroupedData<'_> {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn project(
        &self,
        selected: &[usize],
        table: &Table,
        grouping: &ResolvedGrouping,
        items: &[ResolvedItem],
    ) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| match item {
                        ResolvedItem::Column {
                            group_position: Some(position),
                            ..
                        } => self.group_value(*group, *position, table, grouping),
                        ResolvedItem::Column {
                            group_position: None,
                            ..
                        } => unreachable!("grouped columns are validated"),
                        ResolvedItem::Aggregate { state } => {
                            self.aggregates[*state][*group].clone()
                        }
                        ResolvedItem::Grouping { group_positions } => {
                            Value::Int64(self.grouping_mask(*group, group_positions, grouping))
                        }
                    })
                    .collect()
            })
            .collect()
    }

    fn group_value(
        &self,
        group: usize,
        group_position: usize,
        table: &Table,
        grouping: &ResolvedGrouping,
    ) -> Value {
        self.group_value_ref(group, group_position, table, grouping)
            .to_owned()
    }

    fn group_value_ref<'a>(
        &'a self,
        group: usize,
        group_position: usize,
        table: &'a Table,
        grouping: &ResolvedGrouping,
    ) -> ValueRef<'a> {
        let row = &self.rows[group];
        let set = &grouping.sets[row.set_index];
        if let Some(set_position) = set.positions[group_position] {
            row.key.value(set_position)
        } else {
            table.schema()[grouping.columns[group_position]]
                .data_type
                .default_value_ref()
        }
    }

    fn grouping_mask(
        &self,
        group: usize,
        group_positions: &[usize],
        grouping: &ResolvedGrouping,
    ) -> i64 {
        let set = &grouping.sets[self.rows[group].set_index];
        group_positions.iter().fold(0_i64, |mask, position| {
            (mask << 1) | i64::from(set.positions[*position].is_none())
        })
    }

    fn fallback_cmp(&self, left: usize, right: usize) -> Ordering {
        let left_row = &self.rows[left];
        let right_row = &self.rows[right];
        left_row
            .set_index
            .cmp(&right_row.set_index)
            .then_with(|| left_row.key.cmp(&right_row.key))
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
    table: &Table,
    grouping: &ResolvedGrouping,
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
                } => data
                    .group_value_ref(left, position, table, grouping)
                    .cmp(&data.group_value_ref(right, position, table, grouping)),
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Aggregate { state } => {
                    data.aggregates[state][left].cmp(&data.aggregates[state][right])
                }
                ResolvedItem::Grouping {
                    ref group_positions,
                } => data
                    .grouping_mask(left, group_positions, grouping)
                    .cmp(&data.grouping_mask(right, group_positions, grouping)),
            };
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        data.fallback_cmp(left, right)
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
