use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::plan::{
    AggregateExpression, LogicalOperator, LogicalPlan, OperatorMetrics, PlanNode, ProjectedColumn,
    ProjectionExpression, ResolvedColumn, SortExpression,
};
use crate::sql::AggregateFunction;
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

pub(crate) struct ExecutionOutput {
    pub rows: Vec<Vec<Value>>,
    pub metrics: Vec<OperatorMetrics>,
}

pub(crate) fn execute(table: &Table, plan: &LogicalPlan) -> Result<ExecutionOutput> {
    let mut metrics = vec![OperatorMetrics::default(); plan.node_count()];
    let data = execute_node(table, &plan.root, &mut metrics)?;

    let started = Instant::now();
    let rows = match data {
        ExecutionData::Projected(projected) => projected.materialize(table),
        ExecutionData::Source(_) | ExecutionData::Grouped { .. } => {
            unreachable!("a complete SELECT plan ends in or above Projection")
        }
    };
    metrics[projection_id(&plan.root)].elapsed += started.elapsed();

    Ok(ExecutionOutput { rows, metrics })
}

fn execute_node<'table, 'plan>(
    table: &'table Table,
    node: &'plan PlanNode,
    metrics: &mut [OperatorMetrics],
) -> Result<ExecutionData<'table, 'plan>> {
    match &node.operator {
        LogicalOperator::Scan {
            table: planned_table,
            ..
        } => {
            debug_assert_eq!(planned_table, table.name());
            let started = Instant::now();
            let data = ExecutionData::Source((0..table.row_count()).collect());
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::Filter { input, predicate } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let ExecutionData::Source(rows) = input else {
                unreachable!("Filter consumes Scan rows")
            };
            let data = ExecutionData::Source(
                rows.into_iter()
                    .filter(|row| predicate.evaluate(table, *row))
                    .collect(),
            );
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::Aggregation {
            input,
            group_by,
            aggregates,
        } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let ExecutionData::Source(rows) = input else {
                unreachable!("Aggregation consumes source rows")
            };
            let grouped = execute_grouped(table, &rows, group_by, aggregates)?;
            let selected = (0..grouped.len()).collect::<Vec<_>>();
            let data = ExecutionData::Grouped { grouped, selected };
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::Projection { input, columns } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let projected = match input {
                ExecutionData::Source(rows) => ProjectedData::Source { rows, columns },
                ExecutionData::Grouped { grouped, selected } => ProjectedData::Grouped {
                    grouped,
                    selected,
                    columns,
                },
                ExecutionData::Projected(_) => unreachable!("Projection is applied once"),
            };
            let data = ExecutionData::Projected(projected);
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::Sort { input, ordering } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let ExecutionData::Projected(mut projected) = input else {
                unreachable!("Sort consumes projected rows")
            };
            projected.sort_and_limit(table, ordering, None);
            let data = ExecutionData::Projected(projected);
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::TopK {
            input,
            ordering,
            limit,
        } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let ExecutionData::Projected(mut projected) = input else {
                unreachable!("TopK consumes projected rows")
            };
            projected.sort_and_limit(table, ordering, Some(*limit));
            let data = ExecutionData::Projected(projected);
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
        LogicalOperator::Limit { input, limit } => {
            let input = execute_node(table, input, metrics)?;
            let started = Instant::now();
            let ExecutionData::Projected(mut projected) = input else {
                unreachable!("Limit consumes projected rows")
            };
            projected.truncate(*limit);
            let data = ExecutionData::Projected(projected);
            record_metric(node, &data, started, metrics);
            Ok(data)
        }
    }
}

fn record_metric(
    node: &PlanNode,
    data: &ExecutionData<'_, '_>,
    started: Instant,
    metrics: &mut [OperatorMetrics],
) {
    metrics[node.id()] = OperatorMetrics {
        rows: data.row_count(),
        elapsed: started.elapsed(),
    };
}

fn projection_id(node: &PlanNode) -> usize {
    match &node.operator {
        LogicalOperator::Projection { .. } => node.id(),
        operator => projection_id(
            operator
                .input()
                .expect("a complete SELECT plan contains Projection"),
        ),
    }
}

enum ExecutionData<'table, 'plan> {
    Source(Vec<usize>),
    Grouped {
        grouped: GroupedData<'table>,
        selected: Vec<usize>,
    },
    Projected(ProjectedData<'table, 'plan>),
}

impl ExecutionData<'_, '_> {
    fn row_count(&self) -> usize {
        match self {
            Self::Source(rows) => rows.len(),
            Self::Grouped { selected, .. } => selected.len(),
            Self::Projected(projected) => projected.row_count(),
        }
    }
}

enum ProjectedData<'table, 'plan> {
    Source {
        rows: Vec<usize>,
        columns: &'plan [ProjectedColumn],
    },
    Grouped {
        grouped: GroupedData<'table>,
        selected: Vec<usize>,
        columns: &'plan [ProjectedColumn],
    },
}

impl ProjectedData<'_, '_> {
    fn row_count(&self) -> usize {
        match self {
            Self::Source { rows, .. } => rows.len(),
            Self::Grouped { selected, .. } => selected.len(),
        }
    }

    fn truncate(&mut self, limit: usize) {
        match self {
            Self::Source { rows, .. } => rows.truncate(limit),
            Self::Grouped { selected, .. } => selected.truncate(limit),
        }
    }

    fn sort_and_limit(&mut self, table: &Table, ordering: &[SortExpression], limit: Option<usize>) {
        match self {
            Self::Source { rows, columns } => {
                sort_and_limit(rows, limit, |left, right| {
                    for order in ordering {
                        let SortExpression::Output {
                            output, descending, ..
                        } = order
                        else {
                            unreachable!("source rows have no group key")
                        };
                        let ProjectionExpression::Column { source, .. } =
                            &columns[*output].expression
                        else {
                            unreachable!("ungrouped projections cannot contain aggregates")
                        };
                        let comparison = table.columns()[source.index].cmp_at(left, right);
                        if !comparison.is_eq() {
                            return direction(comparison, *descending);
                        }
                    }
                    left.cmp(&right)
                });
            }
            Self::Grouped {
                grouped,
                selected,
                columns,
            } => {
                sort_and_limit(selected, limit, |left, right| {
                    for order in ordering {
                        let (comparison, descending) = match order {
                            SortExpression::Output {
                                output, descending, ..
                            } => {
                                let comparison = match &columns[*output].expression {
                                    ProjectionExpression::Column {
                                        group_position: Some(position),
                                        ..
                                    } => grouped.keys[left]
                                        .value(*position)
                                        .cmp(&grouped.keys[right].value(*position)),
                                    ProjectionExpression::Column {
                                        group_position: None,
                                        ..
                                    } => unreachable!("grouped columns are resolved"),
                                    ProjectionExpression::Aggregate { state, .. } => grouped
                                        .aggregates[*state][left]
                                        .cmp(&grouped.aggregates[*state][right]),
                                };
                                (comparison, *descending)
                            }
                            SortExpression::GroupKey { .. } => {
                                (grouped.keys[left].cmp(&grouped.keys[right]), false)
                            }
                        };
                        if !comparison.is_eq() {
                            return direction(comparison, descending);
                        }
                    }
                    grouped.keys[left].cmp(&grouped.keys[right])
                });
            }
        }
    }

    fn materialize(self, table: &Table) -> Vec<Vec<Value>> {
        match self {
            Self::Source { rows, columns } => rows
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| match &column.expression {
                            ProjectionExpression::Column { source, .. } => {
                                table.columns()[source.index].value(*row)
                            }
                            ProjectionExpression::Aggregate { .. } => {
                                unreachable!("source projection has no aggregates")
                            }
                        })
                        .collect()
                })
                .collect(),
            Self::Grouped {
                grouped,
                selected,
                columns,
            } => grouped.project(&selected, columns),
        }
    }
}

fn direction(comparison: Ordering, descending: bool) -> Ordering {
    if descending {
        comparison.reverse()
    } else {
        comparison
    }
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

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[ResolvedColumn],
    aggregate_specs: &[AggregateExpression],
) -> Result<GroupedData<'a>> {
    let group_indices = group_columns
        .iter()
        .map(|column| column.index)
        .collect::<Vec<_>>();
    let mut groups = GroupIndex::new(group_indices.len(), matching_rows.len());
    let mut group_count = usize::from(group_indices.is_empty());
    let initial_capacity = matching_rows.len().min(1_024);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(initial_capacity);
            if group_indices.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for row in matching_rows {
        let (group, inserted) = groups.find_or_insert(table, &group_indices, *row, group_count);
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

    fn project(&self, selected: &[usize], columns: &[ProjectedColumn]) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| {
                columns
                    .iter()
                    .map(|column| match &column.expression {
                        ProjectionExpression::Column {
                            group_position: Some(position),
                            ..
                        } => self.keys[*group].value(*position).to_owned(),
                        ProjectionExpression::Column {
                            group_position: None,
                            ..
                        } => unreachable!("grouped columns are resolved"),
                        ProjectionExpression::Aggregate { state, .. } => {
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
    fn new(spec: &AggregateExpression) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type() == Some(DataType::Int64) => Self::SumInt(0),
            AggregateFunction::Sum => Self::SumFloat(0.0),
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type() == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    fn update(&mut self, spec: &AggregateExpression, table: &Table, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Column::Int64(values) = &table.columns()[aggregate_argument(spec)] else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(values[row])
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) = &table.columns()[aggregate_argument(spec)] else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += values[row];
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = table.columns()[aggregate_argument(spec)].value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = table.columns()[aggregate_argument(spec)].value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let Column::Int64(values) = &table.columns()[aggregate_argument(spec)] else {
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
                let Column::Float64(values) = &table.columns()[aggregate_argument(spec)] else {
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

fn aggregate_argument(spec: &AggregateExpression) -> usize {
    spec.argument
        .as_ref()
        .expect("aggregate has a resolved column argument")
        .index
}
