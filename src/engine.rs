use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ArithmeticOperator, ComparisonOperator, Expression,
    OrderBy, Predicate, Select, SelectItem, Statement,
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
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let mut matching_rows = Vec::new();
        for row in 0..table.row_count() {
            let matches = match &predicate {
                Some(predicate) => predicate.evaluate(table, row)?,
                None => true,
            };
            if matches {
                matching_rows.push(row);
            }
        }

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            if ordering.is_empty() {
                order_group_keys(&mut selected_groups, &grouped, select.limit);
                grouped.project(&selected_groups, &items, &group_columns)?
            } else {
                let ordering_keys = grouped.ordering_keys(&items, &group_columns, &ordering)?;
                order_grouped_rows(
                    &mut selected_groups,
                    &grouped,
                    &ordering_keys,
                    &ordering,
                    select.limit,
                );
                grouped.project(&selected_groups, &items, &group_columns)?
            }
        } else if ordering.is_empty() {
            if let Some(limit) = select.limit {
                matching_rows.truncate(limit);
            }
            execute_projection(table, &matching_rows, &items)?
        } else {
            let ordering_keys =
                evaluate_source_ordering_keys(table, &matching_rows, &items, &ordering)?;
            let mut selected_rows = (0..matching_rows.len()).collect::<Vec<_>>();
            order_source_rows(
                &mut selected_rows,
                &matching_rows,
                &ordering_keys,
                &ordering,
                select.limit,
            );
            let retained_rows = selected_rows
                .into_iter()
                .map(|position| matching_rows[position])
                .collect::<Vec<_>>();
            execute_projection(table, &retained_rows, &items)?
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Expression(CompiledExpression),
    Aggregate { state: usize },
}

#[derive(Debug, Clone)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<CompiledExpression>,
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
                    if !group_columns.is_empty() && !group_columns.contains(&source) {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Expression(CompiledExpression::Column {
                        source,
                        data_type: field.data_type,
                    }));
                    result_columns.push(ResultColumn {
                        name: field.name.clone(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Expression { expression, alias } => {
                let compiled = compile_expression(table, expression)?;
                if has_aggregate || !group_columns.is_empty() {
                    for source in compiled.column_sources() {
                        if !group_columns.contains(&source) {
                            return Err(Error::InvalidQuery(format!(
                                "column '{}' must appear in GROUP BY",
                                table.schema()[source].name
                            )));
                        }
                    }
                }
                let data_type = compiled.data_type();
                let default_name = match expression {
                    Expression::Column(name) => {
                        let source = table.column_index(name)?;
                        table.schema()[source].name.clone()
                    }
                    _ => expression.to_string(),
                };
                items.push(ResolvedItem::Expression(compiled));
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or(default_name),
                    data_type,
                });
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (compiled_argument, input_type, argument_name) = match argument {
                    AggregateArgument::Wildcard => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}(*) is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, None, "*".to_owned())
                    }
                    AggregateArgument::Expression(expression) => {
                        let compiled = compile_expression(table, expression)?;
                        let input_type = compiled.data_type();
                        let argument_name = match expression {
                            Expression::Column(name) => {
                                let source = table.column_index(name)?;
                                table.schema()[source].name.clone()
                            }
                            _ => expression.to_string(),
                        };
                        (Some(compiled), Some(input_type), argument_name)
                    }
                };
                validate_aggregate(*function, input_type)?;
                let state = aggregate_specs.len();
                aggregate_specs.push(AggregateSpec {
                    function: *function,
                    argument: compiled_argument,
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
) -> Result<Vec<Vec<Value>>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Expression(expression) => expression
                        .evaluate_row(table, *row)
                        .map(EvaluatedValue::into_owned),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect()
}

fn evaluate_source_ordering_keys<'a>(
    table: &'a Table,
    rows: &[usize],
    items: &'a [ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Result<Vec<Vec<EvaluatedValue<'a>>>> {
    ordering
        .iter()
        .map(|order| {
            let ResolvedItem::Expression(expression) = &items[order.output] else {
                unreachable!("ungrouped projections cannot contain aggregates")
            };
            rows.iter()
                .map(|row| expression.evaluate_row(table, *row))
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

    fn project(
        &self,
        selected: &[usize],
        items: &[ResolvedItem],
        group_columns: &[usize],
    ) -> Result<Vec<Vec<Value>>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| match item {
                        ResolvedItem::Expression(expression) => self
                            .evaluate_expression(*group, expression, group_columns)
                            .map(EvaluatedValue::into_owned),
                        ResolvedItem::Aggregate { state } => {
                            Ok(self.aggregates[*state][*group].clone())
                        }
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect()
    }

    fn ordering_keys<'a>(
        &'a self,
        items: &'a [ResolvedItem],
        group_columns: &[usize],
        ordering: &[ResolvedOrder],
    ) -> Result<Vec<Vec<EvaluatedValue<'a>>>> {
        ordering
            .iter()
            .map(|order| match &items[order.output] {
                ResolvedItem::Expression(expression) => (0..self.len())
                    .map(|group| self.evaluate_expression(group, expression, group_columns))
                    .collect(),
                ResolvedItem::Aggregate { state } => Ok((0..self.len())
                    .map(|group| EvaluatedValue::Borrowed(self.aggregates[*state][group].as_ref()))
                    .collect()),
            })
            .collect()
    }

    fn evaluate_expression<'a>(
        &'a self,
        group: usize,
        expression: &'a CompiledExpression,
        group_columns: &[usize],
    ) -> Result<EvaluatedValue<'a>> {
        expression.evaluate_with(&|source| {
            let position = group_columns
                .iter()
                .position(|column| *column == source)
                .expect("expression columns are validated as grouped");
            self.keys[group].value(position)
        })
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
                if let Some(argument) = &spec.argument {
                    argument.evaluate_row(table, row)?;
                }
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let value = spec
                    .argument
                    .as_ref()
                    .expect("SUM argument")
                    .evaluate_row(table, row)?;
                let ValueRef::Int64(value) = value.as_ref() else {
                    unreachable!("SUM expression type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let value = spec
                    .argument
                    .as_ref()
                    .expect("SUM argument")
                    .evaluate_row(table, row)?;
                let ValueRef::Float64(value) = value.as_ref() else {
                    unreachable!("SUM expression type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = spec
                    .argument
                    .as_ref()
                    .expect("MIN argument")
                    .evaluate_row(table, row)?;
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate.as_ref() < existing.as_ref())
                {
                    *current = Some(candidate.into_owned());
                }
            }
            Self::Max(current) => {
                let candidate = spec
                    .argument
                    .as_ref()
                    .expect("MAX argument")
                    .evaluate_row(table, row)?;
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate.as_ref() > existing.as_ref())
                {
                    *current = Some(candidate.into_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let value = spec
                    .argument
                    .as_ref()
                    .expect("AVG argument")
                    .evaluate_row(table, row)?;
                let ValueRef::Int64(value) = value.as_ref() else {
                    unreachable!("AVG expression type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(value))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let value = spec
                    .argument
                    .as_ref()
                    .expect("AVG argument")
                    .evaluate_row(table, row)?;
                let ValueRef::Float64(value) = value.as_ref() else {
                    unreachable!("AVG expression type is resolved")
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
    rows: &mut Vec<usize>,
    source_rows: &[usize],
    ordering_keys: &[Vec<EvaluatedValue<'_>>],
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
        for (keys, order) in ordering_keys.iter().zip(ordering) {
            let comparison = keys[left].as_ref().cmp(&keys[right].as_ref());
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        source_rows[left].cmp(&source_rows[right])
    });
}

fn order_grouped_rows(
    groups: &mut Vec<usize>,
    data: &GroupedData<'_>,
    ordering_keys: &[Vec<EvaluatedValue<'_>>],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    sort_and_limit(groups, limit, |left, right| {
        for (keys, order) in ordering_keys.iter().zip(ordering) {
            let comparison = keys[left].as_ref().cmp(&keys[right].as_ref());
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

fn order_group_keys(groups: &mut Vec<usize>, data: &GroupedData<'_>, limit: Option<usize>) {
    sort_and_limit(groups, limit, |left, right| {
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
        left: CompiledExpression,
        operator: ComparisonOperator,
        right: CompiledExpression,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: usize) -> Result<bool> {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.evaluate_row(table, row)?;
                let right = right.evaluate_row(table, row)?;
                let comparison = left
                    .as_ref()
                    .sql_cmp(right.as_ref())
                    .expect("predicate operand types are validated");
                Ok(match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                })
            }
            Self::And(left, right) => {
                if left.evaluate(table, row)? {
                    right.evaluate(table, row)
                } else {
                    Ok(false)
                }
            }
            Self::Or(left, right) => {
                if left.evaluate(table, row)? {
                    Ok(true)
                } else {
                    right.evaluate(table, row)
                }
            }
        }
    }
}

#[derive(Debug)]
enum EvaluatedValue<'a> {
    Borrowed(ValueRef<'a>),
    Owned(Value),
}

impl EvaluatedValue<'_> {
    fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Borrowed(value) => *value,
            Self::Owned(value) => value.as_ref(),
        }
    }

    fn into_owned(self) -> Value {
        match self {
            Self::Borrowed(value) => value.to_owned(),
            Self::Owned(value) => value,
        }
    }
}

#[derive(Debug, Clone)]
enum CompiledExpression {
    Column {
        source: usize,
        data_type: DataType,
    },
    Literal(Value),
    Negate {
        expression: Box<Self>,
        data_type: DataType,
    },
    Binary {
        left: Box<Self>,
        operator: ArithmeticOperator,
        right: Box<Self>,
        data_type: DataType,
    },
}

impl CompiledExpression {
    fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. }
            | Self::Negate { data_type, .. }
            | Self::Binary { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
        }
    }

    fn column_sources(&self) -> Vec<usize> {
        let mut sources = Vec::new();
        self.collect_column_sources(&mut sources);
        sources
    }

    fn collect_column_sources(&self, sources: &mut Vec<usize>) {
        match self {
            Self::Column { source, .. } => sources.push(*source),
            Self::Literal(_) => {}
            Self::Negate { expression, .. } => expression.collect_column_sources(sources),
            Self::Binary { left, right, .. } => {
                left.collect_column_sources(sources);
                right.collect_column_sources(sources);
            }
        }
    }

    fn evaluate_row<'a>(&'a self, table: &'a Table, row: usize) -> Result<EvaluatedValue<'a>> {
        self.evaluate_with(&|source| table.columns()[source].value_ref(row))
    }

    fn evaluate_with<'a>(
        &'a self,
        column_value: &impl Fn(usize) -> ValueRef<'a>,
    ) -> Result<EvaluatedValue<'a>> {
        match self {
            Self::Column { source, .. } => Ok(EvaluatedValue::Borrowed(column_value(*source))),
            Self::Literal(value) => Ok(EvaluatedValue::Borrowed(value.as_ref())),
            Self::Negate {
                expression,
                data_type,
            } => {
                let value = expression.evaluate_with(column_value)?;
                match (data_type, value.as_ref()) {
                    (DataType::Int64, ValueRef::Int64(value)) => value
                        .checked_neg()
                        .map(Value::Int64)
                        .map(EvaluatedValue::Owned)
                        .ok_or_else(|| Error::NumericOverflow("Int64 unary negation".to_owned())),
                    (DataType::Float64, ValueRef::Float64(value)) => {
                        finite_float(-value, "Float64 unary negation").map(EvaluatedValue::Owned)
                    }
                    _ => unreachable!("negation operand type is compiled"),
                }
            }
            Self::Binary {
                left,
                operator,
                right,
                data_type,
            } => {
                let left = left.evaluate_with(column_value)?;
                let right = right.evaluate_with(column_value)?;
                evaluate_arithmetic(left.as_ref(), *operator, right.as_ref(), *data_type)
                    .map(EvaluatedValue::Owned)
            }
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
            let left = compile_expression(table, left)?;
            let right = compile_expression(table, right)?;
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

fn compile_expression(table: &Table, expression: &Expression) -> Result<CompiledExpression> {
    match expression {
        Expression::Column(name) => {
            let source = table.column_index(name)?;
            Ok(CompiledExpression::Column {
                source,
                data_type: table.schema()[source].data_type,
            })
        }
        Expression::Literal(value) => Ok(CompiledExpression::Literal(value.clone())),
        Expression::Parenthesized(expression) => compile_expression(table, expression),
        Expression::Negate(expression) => {
            let expression = compile_expression(table, expression)?;
            require_numeric(expression.data_type(), "unary '-' operand")?;
            let data_type = expression.data_type();
            Ok(CompiledExpression::Negate {
                expression: Box::new(expression),
                data_type,
            })
        }
        Expression::Binary {
            left,
            operator,
            right,
        } => {
            let left = compile_expression(table, left)?;
            let right = compile_expression(table, right)?;
            require_numeric(
                left.data_type(),
                &format!("left operand of '{}'", operator.symbol()),
            )?;
            require_numeric(
                right.data_type(),
                &format!("right operand of '{}'", operator.symbol()),
            )?;
            let data_type = if left.data_type() == DataType::Float64
                || right.data_type() == DataType::Float64
            {
                DataType::Float64
            } else {
                DataType::Int64
            };
            Ok(CompiledExpression::Binary {
                left: Box::new(left),
                operator: *operator,
                right: Box::new(right),
                data_type,
            })
        }
    }
}

fn require_numeric(data_type: DataType, context: &str) -> Result<()> {
    if matches!(data_type, DataType::Int64 | DataType::Float64) {
        Ok(())
    } else {
        Err(Error::TypeMismatch {
            context: context.to_owned(),
            expected: "Int64 or Float64".to_owned(),
            actual: data_type.to_string(),
        })
    }
}

fn evaluate_arithmetic(
    left: ValueRef<'_>,
    operator: ArithmeticOperator,
    right: ValueRef<'_>,
    data_type: DataType,
) -> Result<Value> {
    match data_type {
        DataType::Int64 => {
            let (ValueRef::Int64(left), ValueRef::Int64(right)) = (left, right) else {
                unreachable!("Int64 expression operands are compiled")
            };
            if operator == ArithmeticOperator::Divide && right == 0 {
                return division_by_zero();
            }
            let value = match operator {
                ArithmeticOperator::Add => left.checked_add(right),
                ArithmeticOperator::Subtract => left.checked_sub(right),
                ArithmeticOperator::Multiply => left.checked_mul(right),
                ArithmeticOperator::Divide => left.checked_div(right),
            }
            .ok_or_else(|| {
                Error::NumericOverflow(format!("Int64 {}", arithmetic_name(operator)))
            })?;
            Ok(Value::Int64(value))
        }
        DataType::Float64 => {
            let left = numeric_as_f64(left);
            let right = numeric_as_f64(right);
            if operator == ArithmeticOperator::Divide && right == 0.0 {
                return division_by_zero();
            }
            let value = match operator {
                ArithmeticOperator::Add => left + right,
                ArithmeticOperator::Subtract => left - right,
                ArithmeticOperator::Multiply => left * right,
                ArithmeticOperator::Divide => left / right,
            };
            finite_float(value, float_arithmetic_name(operator))
        }
        DataType::Bool | DataType::String => unreachable!("arithmetic result type is numeric"),
    }
}

fn numeric_as_f64(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        ValueRef::Bool(_) | ValueRef::String(_) => unreachable!("arithmetic operands are numeric"),
    }
}

fn finite_float(value: f64, operation: &str) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::Float64(value))
    } else {
        Err(Error::InvalidQuery(format!(
            "non-finite result while computing {operation}"
        )))
    }
}

fn division_by_zero<T>() -> Result<T> {
    Err(Error::InvalidQuery(
        "division by zero in scalar expression".to_owned(),
    ))
}

fn arithmetic_name(operator: ArithmeticOperator) -> &'static str {
    match operator {
        ArithmeticOperator::Add => "addition",
        ArithmeticOperator::Subtract => "subtraction",
        ArithmeticOperator::Multiply => "multiplication",
        ArithmeticOperator::Divide => "division",
    }
}

fn float_arithmetic_name(operator: ArithmeticOperator) -> &'static str {
    match operator {
        ArithmeticOperator::Add => "Float64 addition",
        ArithmeticOperator::Subtract => "Float64 subtraction",
        ArithmeticOperator::Multiply => "Float64 multiplication",
        ArithmeticOperator::Divide => "Float64 division",
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

    #[test]
    fn expression_leaves_borrow_strings_until_output_materialization() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE labels (name String); \
                 INSERT INTO labels VALUES ('borrowed');",
            )
            .expect("setup");
        let table = database.catalog().table("labels").expect("table");

        let column = CompiledExpression::Column {
            source: 0,
            data_type: DataType::String,
        };
        assert!(matches!(
            column.evaluate_row(table, 0).expect("evaluate column"),
            EvaluatedValue::Borrowed(ValueRef::String("borrowed"))
        ));

        let literal = CompiledExpression::Literal(Value::String("literal".to_owned()));
        assert!(matches!(
            literal.evaluate_row(table, 0).expect("evaluate literal"),
            EvaluatedValue::Borrowed(ValueRef::String("literal"))
        ));
    }
}
