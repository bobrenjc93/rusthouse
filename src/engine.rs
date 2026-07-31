use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

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
            .map(|predicate| compile_predicate(table, predicate, "WHERE"))
            .transpose()?;

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let having_has_aggregate = select
            .having
            .as_ref()
            .is_some_and(predicate_contains_aggregate);
        let (items, result_columns, mut aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns, having_has_aggregate)?;
        let having = select
            .having
            .as_ref()
            .map(|predicate| {
                compile_output_predicate(table, &result_columns, predicate, &mut aggregate_specs)
            })
            .transpose()?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            if let Some(having) = &having {
                selected_groups.retain(|group| having.evaluate(&grouped, &items, *group));
            }
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
            );
            grouped.project(&selected_groups, &items)
        } else {
            if having.is_some() {
                return Err(Error::InvalidQuery(
                    "HAVING requires GROUP BY or an aggregate".to_owned(),
                ));
            }
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

#[derive(Debug)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
    filter: Option<CompiledPredicate>,
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
    having_has_aggregate: bool,
) -> Result<(Vec<ResolvedItem>, Vec<ResultColumn>, Vec<AggregateSpec>)> {
    let has_projected_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    let has_aggregate = having_has_aggregate || has_projected_aggregate;
    if has_projected_aggregate
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
                    if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
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
                filter,
                alias,
            } => {
                let (spec, argument_name, output_type) =
                    resolve_aggregate_spec(table, *function, argument, filter.as_ref())?;
                let state = aggregate_specs.len();
                aggregate_specs.push(spec);
                items.push(ResolvedItem::Aggregate { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: output_type,
                });
            }
        }
    }

    Ok((items, result_columns, aggregate_specs))
}

fn resolve_aggregate_spec(
    table: &Table,
    function: AggregateFunction,
    argument: &AggregateArgument,
    filter: Option<&Predicate>,
) -> Result<(AggregateSpec, String, DataType)> {
    let (argument_index, input_type, argument_name) = match argument {
        AggregateArgument::Wildcard => {
            if function != AggregateFunction::Count {
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
    validate_aggregate(function, input_type)?;
    let filter = filter
        .map(|predicate| compile_predicate(table, predicate, "aggregate FILTER"))
        .transpose()?;
    let output_type = aggregate_output_type(function, input_type);
    Ok((
        AggregateSpec {
            function,
            argument: argument_index,
            input_type,
            filter,
        },
        argument_name,
        output_type,
    ))
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

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
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
            if spec
                .filter
                .as_ref()
                .is_none_or(|filter| filter.evaluate(table, *row))
            {
                states[group].update(spec, table, *row)?;
            }
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

    fn project(&self, selected: &[usize], items: &[ResolvedItem]) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| self.item_value(*group, item).to_owned())
                    .collect()
            })
            .collect()
    }

    fn item_value<'a>(&'a self, group: usize, item: &ResolvedItem) -> ValueRef<'a> {
        match item {
            ResolvedItem::Column {
                group_position: Some(position),
                ..
            } => self.keys[group].value(*position),
            ResolvedItem::Column {
                group_position: None,
                ..
            } => unreachable!("grouped columns are validated"),
            ResolvedItem::Aggregate { state } => self.aggregates[*state][group].as_ref(),
        }
    }
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt(Option<i64>),
    SumFloat(Option<f64>),
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
                Self::SumInt(None)
            }
            AggregateFunction::Sum => Self::SumFloat(None),
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        if spec
            .argument
            .is_some_and(|argument| table.columns()[argument].is_null(row))
        {
            return Ok(());
        }

        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let column = &table.columns()[spec.argument.expect("SUM argument")];
                let Some(values) = column.int64_values() else {
                    unreachable!("SUM input type is resolved")
                };
                let next = match *sum {
                    Some(current) => current
                        .checked_add(values[row])
                        .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?,
                    None => values[row],
                };
                *sum = Some(next);
            }
            Self::SumFloat(sum) => {
                let column = &table.columns()[spec.argument.expect("SUM argument")];
                let Some(values) = column.float64_values() else {
                    unreachable!("SUM input type is resolved")
                };
                let next = sum.unwrap_or(0.0) + values[row];
                if !next.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
                *sum = Some(next);
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
                let column = &table.columns()[spec.argument.expect("AVG argument")];
                let Some(values) = column.int64_values() else {
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
                let column = &table.columns()[spec.argument.expect("AVG argument")];
                let Some(values) = column.float64_values() else {
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
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt(Some(value)) => Ok(Value::Int64(value)),
            Self::SumFloat(Some(value)) => Ok(Value::Float64(value)),
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum / count as f64)),
            Self::SumInt(None)
            | Self::SumFloat(None)
            | Self::Min(None)
            | Self::Max(None)
            | Self::AvgInt { .. }
            | Self::AvgFloat { .. } => Ok(Value::Null),
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
                evaluate_comparison(left, *operator, right)
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
    fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Column { data_type, .. } => Some(*data_type),
            Self::Literal(Value::Null) => None,
            Self::Literal(value) => Some(value.data_type()),
        }
    }

    fn value<'a>(&'a self, table: &'a Table, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => table.columns()[*index].value_ref(row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(
    table: &Table,
    predicate: &Predicate,
    context: &str,
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
                    context: format!("{context} comparison"),
                    expected: type_name(left.data_type()),
                    actual: type_name(right.data_type()),
                });
            }
            Ok(CompiledPredicate::Comparison {
                left,
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(CompiledPredicate::And(
            Box::new(compile_predicate(table, left, context)?),
            Box::new(compile_predicate(table, right, context)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(table, left, context)?),
            Box::new(compile_predicate(table, right, context)?),
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
        Operand::Aggregate { .. } => Err(Error::InvalidQuery(
            "aggregate functions are only allowed in SELECT and HAVING".to_owned(),
        )),
    }
}

#[derive(Debug)]
enum CompiledOutputPredicate {
    Comparison {
        left: CompiledOutputOperand,
        operator: ComparisonOperator,
        right: CompiledOutputOperand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledOutputPredicate {
    fn evaluate(&self, data: &GroupedData<'_>, items: &[ResolvedItem], group: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => evaluate_comparison(
                left.value(data, items, group),
                *operator,
                right.value(data, items, group),
            ),
            Self::And(left, right) => {
                left.evaluate(data, items, group) && right.evaluate(data, items, group)
            }
            Self::Or(left, right) => {
                left.evaluate(data, items, group) || right.evaluate(data, items, group)
            }
        }
    }
}

#[derive(Debug)]
enum CompiledOutputOperand {
    Output { index: usize, data_type: DataType },
    Aggregate { state: usize, data_type: DataType },
    Literal(Value),
}

impl CompiledOutputOperand {
    fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Output { data_type, .. } => Some(*data_type),
            Self::Aggregate { data_type, .. } => Some(*data_type),
            Self::Literal(Value::Null) => None,
            Self::Literal(value) => Some(value.data_type()),
        }
    }

    fn value<'a>(
        &'a self,
        data: &'a GroupedData<'_>,
        items: &[ResolvedItem],
        group: usize,
    ) -> ValueRef<'a> {
        match self {
            Self::Output { index, .. } => data.item_value(group, &items[*index]),
            Self::Aggregate { state, .. } => data.aggregates[*state][group].as_ref(),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_output_predicate(
    table: &Table,
    columns: &[ResultColumn],
    predicate: &Predicate,
    aggregate_specs: &mut Vec<AggregateSpec>,
) -> Result<CompiledOutputPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_output_operand(table, columns, left, aggregate_specs)?;
            let right = compile_output_operand(table, columns, right, aggregate_specs)?;
            if !comparable(left.data_type(), right.data_type()) {
                return Err(Error::TypeMismatch {
                    context: "HAVING comparison".to_owned(),
                    expected: type_name(left.data_type()),
                    actual: type_name(right.data_type()),
                });
            }
            Ok(CompiledOutputPredicate::Comparison {
                left,
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(CompiledOutputPredicate::And(
            Box::new(compile_output_predicate(
                table,
                columns,
                left,
                aggregate_specs,
            )?),
            Box::new(compile_output_predicate(
                table,
                columns,
                right,
                aggregate_specs,
            )?),
        )),
        Predicate::Or(left, right) => Ok(CompiledOutputPredicate::Or(
            Box::new(compile_output_predicate(
                table,
                columns,
                left,
                aggregate_specs,
            )?),
            Box::new(compile_output_predicate(
                table,
                columns,
                right,
                aggregate_specs,
            )?),
        )),
    }
}

fn compile_output_operand(
    table: &Table,
    columns: &[ResultColumn],
    operand: &Operand,
    aggregate_specs: &mut Vec<AggregateSpec>,
) -> Result<CompiledOutputOperand> {
    match operand {
        Operand::Column(name) => {
            let matches = columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.name.eq_ignore_ascii_case(name))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [(index, column)] => Ok(CompiledOutputOperand::Output {
                    index: *index,
                    data_type: column.data_type,
                }),
                [] => Err(Error::InvalidQuery(format!(
                    "HAVING column or alias '{name}' is not in the SELECT output"
                ))),
                _ => Err(Error::InvalidQuery(format!(
                    "HAVING name '{name}' is ambiguous"
                ))),
            }
        }
        Operand::Literal(value) => Ok(CompiledOutputOperand::Literal(value.clone())),
        Operand::Aggregate {
            function,
            argument,
            filter,
        } => {
            let (spec, _, data_type) =
                resolve_aggregate_spec(table, *function, argument, filter.as_deref())?;
            let state = aggregate_specs.len();
            aggregate_specs.push(spec);
            Ok(CompiledOutputOperand::Aggregate { state, data_type })
        }
    }
}

fn predicate_contains_aggregate(predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            matches!(left, Operand::Aggregate { .. }) || matches!(right, Operand::Aggregate { .. })
        }
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            predicate_contains_aggregate(left) || predicate_contains_aggregate(right)
        }
    }
}

fn comparable(left: Option<DataType>, right: Option<DataType>) -> bool {
    match (left, right) {
        (None, _) | (_, None) => true,
        (Some(left), Some(right)) => {
            left == right
                || matches!(
                    (left, right),
                    (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64)
                )
        }
    }
}

fn type_name(data_type: Option<DataType>) -> String {
    data_type.map_or_else(|| "NULL".to_owned(), |data_type| data_type.to_string())
}

fn evaluate_comparison(
    left: ValueRef<'_>,
    operator: ComparisonOperator,
    right: ValueRef<'_>,
) -> bool {
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
