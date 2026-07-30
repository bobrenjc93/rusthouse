use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{KeyBound, KeyRange, RowId, Table};
use crate::value::{DataType, Value, ValueRef};

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
    last_query_stats: Option<QueryStats>,
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

/// Work performed by the most recently completed SELECT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub total_rows: usize,
    pub scanned_rows: usize,
    pub total_parts: usize,
    pub scanned_parts: usize,
    pub used_primary_key: bool,
    pub used_ordered_merge: bool,
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

    #[must_use]
    pub fn last_query_stats(&self) -> Option<QueryStats> {
        self.last_query_stats
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
            Statement::CreateTable {
                name,
                columns,
                order_by,
            } => {
                self.catalog.create_ordered_table(name, columns, order_by)?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::Insert { table, rows } => {
                let affected_rows = rows.len();
                let target = self.catalog.table_mut(&table)?;
                target.insert_rows(rows)?;
                Ok(StatementResult::Command {
                    tag: "INSERT",
                    affected_rows,
                })
            }
            Statement::Select(select) => self.execute_select(select).map(StatementResult::Query),
        }
    }

    fn execute_select(&mut self, select: Select) -> Result<QueryResult> {
        let (result, stats) = {
            let table = self.catalog.table(&select.table)?;
            execute_select(table, select)?
        };
        self.last_query_stats = Some(stats);
        Ok(result)
    }
}

fn execute_select(table: &Table, select: Select) -> Result<(QueryResult, QueryStats)> {
    let predicate = select
        .predicate
        .as_ref()
        .map(|predicate| compile_predicate(table, predicate))
        .transpose()?;

    let key_range = predicate
        .as_ref()
        .and_then(|predicate| primary_key_range(table, predicate));
    let scans = table.scan_parts(key_range.as_ref());
    let mut stats = QueryStats {
        total_rows: table.row_count(),
        scanned_rows: scans.iter().map(|scan| scan.rows.len()).sum(),
        total_parts: table.parts().len(),
        scanned_parts: scans.iter().filter(|scan| !scan.rows.is_empty()).count(),
        used_primary_key: key_range.is_some(),
        used_ordered_merge: false,
    };
    let matching_parts = scans
        .into_iter()
        .map(|scan| {
            scan.rows
                .map(|row| RowId {
                    part: scan.part,
                    row,
                })
                .filter(|row| {
                    predicate
                        .as_ref()
                        .is_none_or(|predicate| predicate.evaluate(table, *row))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let group_columns = resolve_group_columns(table, &select.group_by)?;
    let (items, result_columns, aggregate_specs) =
        resolve_select_items(table, &select.items, &group_columns)?;
    let ordering = resolve_ordering(&result_columns, &select.order_by)?;

    let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
    let rows = if grouped {
        let matching_rows = matching_parts.into_iter().flatten().collect::<Vec<_>>();
        let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
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
        let matching_rows = if select.limit.is_some()
            && ordered_merge_columns(table, &items, &ordering).is_some()
        {
            stats.used_ordered_merge = true;
            merge_source_rows(&matching_parts, table, &ordering, &items, select.limit)
        } else {
            let mut rows = matching_parts.into_iter().flatten().collect::<Vec<_>>();
            order_source_rows(&mut rows, table, &items, &ordering, select.limit);
            rows
        };
        execute_projection(table, &matching_rows, &items)
    };

    Ok((
        QueryResult {
            columns: result_columns,
            rows,
        },
        stats,
    ))
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
    matching_rows: &[RowId],
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source, .. } => table.value(*row, *source),
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
    matching_rows: &[RowId],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
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
        row: RowId,
        next_group: usize,
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = table.value_ref(row, columns[0]);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.value_ref(row, columns[0]),
                    table.value_ref(row, columns[1]),
                ];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| table.value_ref(row, *column))
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

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: RowId) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let ValueRef::Int64(value) =
                    table.value_ref(row, spec.argument.expect("SUM argument"))
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let ValueRef::Float64(value) =
                    table.value_ref(row, spec.argument.expect("SUM argument"))
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = table.value_ref(row, spec.argument.expect("MIN argument"));
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = table.value_ref(row, spec.argument.expect("MAX argument"));
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let ValueRef::Int64(value) =
                    table.value_ref(row, spec.argument.expect("AVG argument"))
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(value))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let ValueRef::Float64(value) =
                    table.value_ref(row, spec.argument.expect("AVG argument"))
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum += value;
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
    rows: &mut Vec<RowId>,
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
            let comparison = table
                .value_ref(left, source)
                .cmp(&table.value_ref(right, source));
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

fn ordered_merge_columns(
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Option<Vec<usize>> {
    if ordering.is_empty() || ordering.len() > table.order_key().len() {
        return None;
    }
    ordering
        .iter()
        .enumerate()
        .map(|(position, order)| {
            let ResolvedItem::Column { source, .. } = items[order.output] else {
                return None;
            };
            (!order.descending && source == table.order_key()[position]).then_some(source)
        })
        .collect()
}

fn merge_source_rows(
    parts: &[Vec<RowId>],
    table: &Table,
    ordering: &[ResolvedOrder],
    items: &[ResolvedItem],
    limit: Option<usize>,
) -> Vec<RowId> {
    let columns = ordered_merge_columns(table, items, ordering)
        .expect("ordered merge compatibility is checked by the caller");
    let limit = limit.expect("ordered merge is only used with LIMIT");
    let mut positions = vec![0; parts.len()];
    let mut merged = Vec::with_capacity(limit.min(parts.iter().map(Vec::len).sum()));
    let mut heap = Vec::with_capacity(parts.len());
    for (part, _) in parts
        .iter()
        .enumerate()
        .filter(|(_, rows)| !rows.is_empty())
    {
        heap.push(part);
        let mut child = heap.len() - 1;
        while child > 0 {
            let parent = (child - 1) / 2;
            if !part_head_is_less(
                heap[child],
                heap[parent],
                &positions,
                parts,
                table,
                &columns,
            ) {
                break;
            }
            heap.swap(child, parent);
            child = parent;
        }
    }

    while merged.len() < limit && !heap.is_empty() {
        let part = heap[0];
        merged.push(parts[part][positions[part]]);
        positions[part] += 1;
        if positions[part] == parts[part].len() {
            heap.swap_remove(0);
        }

        let mut parent = 0;
        while parent < heap.len() {
            let left = parent * 2 + 1;
            if left >= heap.len() {
                break;
            }
            let right = left + 1;
            let child = if right < heap.len()
                && part_head_is_less(heap[right], heap[left], &positions, parts, table, &columns)
            {
                right
            } else {
                left
            };
            if !part_head_is_less(
                heap[child],
                heap[parent],
                &positions,
                parts,
                table,
                &columns,
            ) {
                break;
            }
            heap.swap(child, parent);
            parent = child;
        }
    }
    merged
}

fn part_head_is_less(
    left_part: usize,
    right_part: usize,
    positions: &[usize],
    parts: &[Vec<RowId>],
    table: &Table,
    columns: &[usize],
) -> bool {
    table.compare_rows(
        parts[left_part][positions[left_part]],
        parts[right_part][positions[right_part]],
        columns,
    ) == Ordering::Less
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

fn sort_and_limit<T: Copy>(
    indices: &mut Vec<T>,
    limit: Option<usize>,
    compare: impl Fn(T, T) -> Ordering,
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

fn primary_key_range(table: &Table, predicate: &CompiledPredicate) -> Option<KeyRange> {
    if table.order_key().is_empty() {
        return None;
    }

    let mut range = KeyRange::default();
    for column in table.order_key() {
        let mut comparisons = Vec::new();
        collect_key_comparisons(predicate, *column, &mut comparisons);
        if let Some((_, value)) = comparisons
            .iter()
            .find(|(operator, _)| *operator == ComparisonOperator::Equal)
        {
            range.equalities.push((*value).clone());
            continue;
        }

        for (operator, value) in comparisons {
            let candidate = match operator {
                ComparisonOperator::Greater => Some((
                    true,
                    KeyBound {
                        value: value.clone(),
                        inclusive: false,
                    },
                )),
                ComparisonOperator::GreaterOrEqual => Some((
                    true,
                    KeyBound {
                        value: value.clone(),
                        inclusive: true,
                    },
                )),
                ComparisonOperator::Less => Some((
                    false,
                    KeyBound {
                        value: value.clone(),
                        inclusive: false,
                    },
                )),
                ComparisonOperator::LessOrEqual => Some((
                    false,
                    KeyBound {
                        value: value.clone(),
                        inclusive: true,
                    },
                )),
                ComparisonOperator::Equal | ComparisonOperator::NotEqual => None,
            };
            let Some((is_lower, candidate)) = candidate else {
                continue;
            };
            let target = if is_lower {
                &mut range.lower
            } else {
                &mut range.upper
            };
            if target.as_ref().is_none_or(|current| {
                let ordering = candidate
                    .value
                    .as_ref()
                    .sql_cmp(current.value.as_ref())
                    .expect("predicate types are validated");
                if is_lower {
                    ordering == Ordering::Greater
                        || (ordering == Ordering::Equal
                            && !candidate.inclusive
                            && current.inclusive)
                } else {
                    ordering == Ordering::Less
                        || (ordering == Ordering::Equal
                            && !candidate.inclusive
                            && current.inclusive)
                }
            }) {
                *target = Some(candidate);
            }
        }

        if range.lower.is_none() && range.upper.is_none() {
            break;
        }
        return Some(range);
    }

    (!range.equalities.is_empty()).then_some(range)
}

fn collect_key_comparisons<'a>(
    predicate: &'a CompiledPredicate,
    column: usize,
    comparisons: &mut Vec<(ComparisonOperator, &'a Value)>,
) {
    match predicate {
        CompiledPredicate::Comparison {
            left,
            operator,
            right,
        } => {
            if let (CompiledOperand::Column { index, .. }, CompiledOperand::Literal(value)) =
                (left, right)
                && *index == column
            {
                comparisons.push((*operator, value));
            } else if let (CompiledOperand::Literal(value), CompiledOperand::Column { index, .. }) =
                (left, right)
                && *index == column
            {
                comparisons.push((reverse_comparison(*operator), value));
            }
        }
        CompiledPredicate::And(left, right) => {
            collect_key_comparisons(left, column, comparisons);
            collect_key_comparisons(right, column, comparisons);
        }
        CompiledPredicate::Or(_, _) => {}
    }
}

fn reverse_comparison(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::Equal,
        ComparisonOperator::NotEqual => ComparisonOperator::NotEqual,
        ComparisonOperator::Less => ComparisonOperator::Greater,
        ComparisonOperator::LessOrEqual => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::Greater => ComparisonOperator::Less,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::LessOrEqual,
    }
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: RowId) -> bool {
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

    fn value<'a>(&'a self, table: &'a Table, row: RowId) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => table.value_ref(row, *index),
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
