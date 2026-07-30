use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ArithmeticOperator, ComparisonOperator, OrderBy,
    Predicate, ScalarExpression, Select, SelectItem, Statement,
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
        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;
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

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
            let mut selected = if select.limit == Some(0) {
                Vec::new()
            } else {
                prepare_group_order(&grouped, &items, &ordering)?
            };
            order_grouped_rows(&mut selected, &grouped, &ordering, select.limit);
            let selected = selected
                .into_iter()
                .map(|row| row.index)
                .collect::<Vec<_>>();
            grouped.project(&selected, &items)?
        } else {
            if select.limit == Some(0) {
                matching_rows.clear();
            } else if ordering.is_empty()
                && let Some(limit) = select.limit
            {
                matching_rows.truncate(limit);
            }
            if !ordering.is_empty() {
                let mut selected = prepare_source_order(table, &matching_rows, &items, &ordering)?;
                order_source_rows(&mut selected, &ordering, select.limit);
                matching_rows = selected.into_iter().map(|row| row.index).collect();
            }
            execute_projection(table, &matching_rows, &items)?
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
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Expression(CompiledExpression::Column {
                        index: source,
                        data_type: field.data_type,
                        group_position,
                    }));
                    result_columns.push(ResultColumn {
                        name: field.name.clone(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = table.column_index(name)?;
                let compiled = CompiledExpression::Column {
                    index: source,
                    data_type: table.schema()[source].data_type,
                    group_position: group_columns.iter().position(|column| *column == source),
                };
                validate_grouped_expression(
                    table,
                    &compiled,
                    has_aggregate || !group_columns.is_empty(),
                )?;
                items.push(ResolvedItem::Expression(compiled));
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| table.schema()[source].name.clone()),
                    data_type: table.schema()[source].data_type,
                });
            }
            SelectItem::Expression { expression, alias } => {
                let compiled = compile_expression(table, expression, Some(group_columns))?;
                validate_grouped_expression(
                    table,
                    &compiled,
                    has_aggregate || !group_columns.is_empty(),
                )?;
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| expression.display_name()),
                    data_type: compiled.data_type(),
                });
                items.push(ResolvedItem::Expression(compiled));
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
                    AggregateArgument::Column(name) => {
                        let index = table.column_index(name)?;
                        let compiled = CompiledExpression::Column {
                            index,
                            data_type: table.schema()[index].data_type,
                            group_position: None,
                        };
                        (
                            Some(compiled),
                            Some(table.schema()[index].data_type),
                            table.schema()[index].name.clone(),
                        )
                    }
                    AggregateArgument::Expression(expression) => {
                        let compiled = compile_expression(table, expression, None)?;
                        let data_type = compiled.data_type();
                        (Some(compiled), Some(data_type), expression.display_name())
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
                        .evaluate(ExpressionSource::Row(table, *row))
                        .map(Evaluated::into_owned),
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

    fn project(&self, selected: &[usize], items: &[ResolvedItem]) -> Result<Vec<Vec<Value>>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| match item {
                        ResolvedItem::Expression(expression) => expression
                            .evaluate(ExpressionSource::Group(&self.keys[*group]))
                            .map(Evaluated::into_owned),
                        ResolvedItem::Aggregate { state } => {
                            Ok(self.aggregates[*state][*group].clone())
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

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        let argument = spec
            .argument
            .as_ref()
            .map(|expression| expression.evaluate(ExpressionSource::Row(table, row)))
            .transpose()?;
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Some(
                    Evaluated::Owned(Value::Int64(value)) | Evaluated::Ref(ValueRef::Int64(value)),
                ) = argument
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Some(
                    Evaluated::Owned(Value::Float64(value))
                    | Evaluated::Ref(ValueRef::Float64(value)),
                ) = argument
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = argument.as_ref().expect("MIN argument").as_ref();
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = argument.as_ref().expect("MAX argument").as_ref();
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let Some(
                    Evaluated::Owned(Value::Int64(value)) | Evaluated::Ref(ValueRef::Int64(value)),
                ) = argument
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
                let Some(
                    Evaluated::Owned(Value::Float64(value))
                    | Evaluated::Ref(ValueRef::Float64(value)),
                ) = argument
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

#[derive(Debug)]
struct PreparedOrder {
    index: usize,
    keys: Vec<Value>,
}

fn prepare_source_order(
    table: &Table,
    rows: &[usize],
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Result<Vec<PreparedOrder>> {
    rows.iter()
        .map(|row| {
            let keys = ordering
                .iter()
                .map(|order| match &items[order.output] {
                    ResolvedItem::Expression(expression) => expression
                        .evaluate(ExpressionSource::Row(table, *row))
                        .map(Evaluated::into_owned),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("ungrouped projections do not contain aggregates")
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(PreparedOrder { index: *row, keys })
        })
        .collect()
}

fn prepare_group_order(
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) -> Result<Vec<PreparedOrder>> {
    (0..data.len())
        .map(|group| {
            let keys = ordering
                .iter()
                .map(|order| match &items[order.output] {
                    ResolvedItem::Expression(expression) => expression
                        .evaluate(ExpressionSource::Group(&data.keys[group]))
                        .map(Evaluated::into_owned),
                    ResolvedItem::Aggregate { state } => Ok(data.aggregates[*state][group].clone()),
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(PreparedOrder { index: group, keys })
        })
        .collect()
}

fn order_source_rows(
    rows: &mut Vec<PreparedOrder>,
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    sort_and_limit(rows, limit, |left, right| {
        for (position, order) in ordering.iter().enumerate() {
            let comparison = left.keys[position].cmp(&right.keys[position]);
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        left.index.cmp(&right.index)
    });
}

fn order_grouped_rows(
    rows: &mut Vec<PreparedOrder>,
    data: &GroupedData<'_>,
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    sort_and_limit(rows, limit, |left, right| {
        for (position, order) in ordering.iter().enumerate() {
            let comparison = left.keys[position].cmp(&right.keys[position]);
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        data.keys[left.index].cmp(&data.keys[right.index])
    });
}

fn sort_and_limit<T>(
    values: &mut Vec<T>,
    limit: Option<usize>,
    compare: impl Fn(&T, &T) -> Ordering,
) {
    if let Some(0) = limit {
        values.clear();
        return;
    }
    if let Some(limit) = limit.filter(|limit| *limit < values.len()) {
        values.select_nth_unstable_by(limit, |left, right| compare(left, right));
        values.truncate(limit);
    }
    values.sort_unstable_by(compare);
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
                let left = left.evaluate(ExpressionSource::Row(table, row))?;
                let right = right.evaluate(ExpressionSource::Row(table, row))?;
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

#[derive(Debug, Clone)]
enum CompiledExpression {
    Column {
        index: usize,
        data_type: DataType,
        group_position: Option<usize>,
    },
    Literal(Value),
    UnaryMinus {
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
            Self::Column { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
            Self::UnaryMinus { data_type, .. } | Self::Binary { data_type, .. } => *data_type,
        }
    }

    fn evaluate<'a>(&'a self, source: ExpressionSource<'a>) -> Result<Evaluated<'a>> {
        match self {
            Self::Column {
                index,
                group_position,
                ..
            } => match source {
                ExpressionSource::Row(table, row) => {
                    Ok(Evaluated::Ref(table.columns()[*index].value_ref(row)))
                }
                ExpressionSource::Group(key) => Ok(Evaluated::Ref(
                    key.value(group_position.expect("group references are validated")),
                )),
            },
            Self::Literal(value) => Ok(Evaluated::Ref(value.as_ref())),
            Self::UnaryMinus { expression, .. } => {
                let value = expression.evaluate(source)?;
                apply_unary_minus(value.as_ref()).map(Evaluated::Owned)
            }
            Self::Binary {
                left,
                operator,
                right,
                data_type,
            } => {
                let left = left.evaluate(source)?;
                let right = right.evaluate(source)?;
                apply_binary(*operator, *data_type, left.as_ref(), right.as_ref())
                    .map(Evaluated::Owned)
            }
        }
    }

    fn first_ungrouped_column(&self) -> Option<usize> {
        match self {
            Self::Column {
                index,
                group_position: None,
                ..
            } => Some(*index),
            Self::Column { .. } | Self::Literal(_) => None,
            Self::UnaryMinus { expression, .. } => expression.first_ungrouped_column(),
            Self::Binary { left, right, .. } => left
                .first_ungrouped_column()
                .or_else(|| right.first_ungrouped_column()),
        }
    }

    fn literal(&self) -> Option<&Value> {
        match self {
            Self::Literal(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExpressionSource<'a> {
    Row(&'a Table, usize),
    Group(&'a GroupKey<'a>),
}

#[derive(Debug)]
enum Evaluated<'a> {
    Ref(ValueRef<'a>),
    Owned(Value),
}

impl Evaluated<'_> {
    fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Ref(value) => *value,
            Self::Owned(value) => value.as_ref(),
        }
    }

    fn into_owned(self) -> Value {
        match self {
            Self::Ref(value) => value.to_owned(),
            Self::Owned(value) => value,
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
            let left = compile_expression(table, left, None)?;
            let right = compile_expression(table, right, None)?;
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

fn compile_expression(
    table: &Table,
    expression: &ScalarExpression,
    group_columns: Option<&[usize]>,
) -> Result<CompiledExpression> {
    match expression {
        ScalarExpression::Column(name) => {
            let index = table.column_index(name)?;
            Ok(CompiledExpression::Column {
                index,
                data_type: table.schema()[index].data_type,
                group_position: group_columns
                    .and_then(|columns| columns.iter().position(|column| *column == index)),
            })
        }
        ScalarExpression::Literal(value) => Ok(CompiledExpression::Literal(value.clone())),
        ScalarExpression::Parenthesized(expression) => {
            compile_expression(table, expression, group_columns)
        }
        ScalarExpression::UnaryMinus(expression) => {
            let compiled = compile_expression(table, expression, group_columns)?;
            require_numeric(compiled.data_type(), "unary '-' operand")?;
            if let Some(value) = compiled.literal() {
                return apply_unary_minus(value.as_ref()).map(CompiledExpression::Literal);
            }
            let data_type = compiled.data_type();
            Ok(CompiledExpression::UnaryMinus {
                expression: Box::new(compiled),
                data_type,
            })
        }
        ScalarExpression::Binary {
            left,
            operator,
            right,
        } => {
            let left = compile_expression(table, left, group_columns)?;
            let right = compile_expression(table, right, group_columns)?;
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
            if let (Some(left_value), Some(right_value)) = (left.literal(), right.literal()) {
                return apply_binary(
                    *operator,
                    data_type,
                    left_value.as_ref(),
                    right_value.as_ref(),
                )
                .map(CompiledExpression::Literal);
            }
            Ok(CompiledExpression::Binary {
                left: Box::new(left),
                operator: *operator,
                right: Box::new(right),
                data_type,
            })
        }
    }
}

fn validate_grouped_expression(
    table: &Table,
    expression: &CompiledExpression,
    grouped: bool,
) -> Result<()> {
    if grouped && let Some(column) = expression.first_ungrouped_column() {
        return Err(Error::InvalidQuery(format!(
            "column '{}' must appear in GROUP BY",
            table.schema()[column].name
        )));
    }
    Ok(())
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

fn apply_unary_minus(value: ValueRef<'_>) -> Result<Value> {
    match value {
        ValueRef::Int64(value) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("unary '-' on Int64".to_owned())),
        ValueRef::Float64(value) => Ok(Value::Float64(-value)),
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("unary operand type is validated")
        }
    }
}

fn apply_binary(
    operator: ArithmeticOperator,
    data_type: DataType,
    left: ValueRef<'_>,
    right: ValueRef<'_>,
) -> Result<Value> {
    if data_type == DataType::Int64 {
        let (ValueRef::Int64(left), ValueRef::Int64(right)) = (left, right) else {
            unreachable!("Int64 expression operand types are resolved")
        };
        if operator == ArithmeticOperator::Divide && right == 0 {
            return Err(Error::DivisionByZero);
        }
        let result = match operator {
            ArithmeticOperator::Add => left.checked_add(right),
            ArithmeticOperator::Subtract => left.checked_sub(right),
            ArithmeticOperator::Multiply => left.checked_mul(right),
            ArithmeticOperator::Divide => left.checked_div(right),
        };
        return result.map(Value::Int64).ok_or_else(|| {
            Error::NumericOverflow(format!("Int64 '{}' expression", operator.symbol()))
        });
    }

    let left = numeric_as_f64(left);
    let right = numeric_as_f64(right);
    if operator == ArithmeticOperator::Divide && right == 0.0 {
        return Err(Error::DivisionByZero);
    }
    let result = match operator {
        ArithmeticOperator::Add => left + right,
        ArithmeticOperator::Subtract => left - right,
        ArithmeticOperator::Multiply => left * right,
        ArithmeticOperator::Divide => left / right,
    };
    if result.is_finite() {
        Ok(Value::Float64(result))
    } else {
        Err(Error::NumericOverflow(format!(
            "Float64 '{}' expression",
            operator.symbol()
        )))
    }
}

fn numeric_as_f64(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("numeric expression operand types are resolved")
        }
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
