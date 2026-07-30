use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
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

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let has_query_aggregate = select
            .items
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }))
            || select
                .having
                .as_ref()
                .is_some_and(|predicate| predicate_contains_aggregate(predicate));
        let (items, result_columns, mut aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns, has_query_aggregate)?;
        let having = select
            .having
            .as_ref()
            .map(|predicate| {
                compile_having(
                    table,
                    predicate,
                    &group_columns,
                    &select.items,
                    &items,
                    &mut aggregate_specs,
                )
            })
            .transpose()?;
        if having.is_some() && group_columns.is_empty() && aggregate_specs.is_empty() {
            return Err(Error::InvalidQuery(
                "HAVING requires GROUP BY or an aggregate expression".to_owned(),
            ));
        }
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            if let Some(having) = &having {
                selected_groups.retain(|group| having.evaluate(&grouped, *group));
            }
            if select.distinct {
                retain_distinct_grouped_rows(&mut selected_groups, &grouped, &items);
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
            debug_assert!(having.is_none());
            if select.distinct {
                retain_distinct_source_rows(&mut matching_rows, table, &items);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
    distinct: bool,
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
    has_query_aggregate: bool,
) -> Result<(Vec<ResolvedItem>, Vec<ResultColumn>, Vec<AggregateSpec>)> {
    let has_selected_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    if has_selected_aggregate
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
                    if (has_query_aggregate || !group_columns.is_empty())
                        && group_position.is_none()
                    {
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
                if (has_query_aggregate || !group_columns.is_empty()) && group_position.is_none() {
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
                let (spec, argument_name) = resolve_aggregate(table, *function, argument)?;
                let output_type = aggregate_output_type(spec.function, spec.input_type);
                let state = intern_aggregate(&mut aggregate_specs, spec);
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

fn resolve_aggregate(
    table: &Table,
    function: AggregateFunction,
    argument: &AggregateArgument,
) -> Result<(AggregateSpec, String)> {
    let (argument, input_type, distinct, argument_name) = match argument {
        AggregateArgument::Wildcard => {
            if function != AggregateFunction::Count {
                return Err(Error::InvalidQuery(format!(
                    "{}(*) is not supported; use a column argument",
                    function.name()
                )));
            }
            (None, None, false, "*".to_owned())
        }
        AggregateArgument::Column(name) | AggregateArgument::DistinctColumn(name) => {
            let distinct = matches!(argument, AggregateArgument::DistinctColumn(_));
            if distinct && function != AggregateFunction::Count {
                return Err(Error::InvalidQuery(format!(
                    "DISTINCT aggregate arguments are only supported for COUNT, not {}",
                    function.name()
                )));
            }
            let index = table.column_index(name)?;
            let name = table.schema()[index].name.clone();
            (
                Some(index),
                Some(table.schema()[index].data_type),
                distinct,
                if distinct {
                    format!("DISTINCT {name}")
                } else {
                    name
                },
            )
        }
    };
    validate_aggregate(function, input_type)?;
    Ok((
        AggregateSpec {
            function,
            argument,
            input_type,
            distinct,
        },
        argument_name,
    ))
}

fn intern_aggregate(aggregate_specs: &mut Vec<AggregateSpec>, spec: AggregateSpec) -> usize {
    if let Some(index) = aggregate_specs
        .iter()
        .position(|existing| *existing == spec)
    {
        index
    } else {
        let index = aggregate_specs.len();
        aggregate_specs.push(spec);
        index
    }
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

#[derive(Debug)]
enum CompiledHavingPredicate {
    Comparison {
        left: CompiledHavingOperand,
        operator: ComparisonOperator,
        right: CompiledHavingOperand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledHavingPredicate {
    fn evaluate(&self, data: &GroupedData<'_>, group: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => comparison_is_true(
                left.value(data, group)
                    .sql_cmp(right.value(data, group))
                    .expect("HAVING operand types are validated"),
                *operator,
            ),
            Self::And(left, right) => left.evaluate(data, group) && right.evaluate(data, group),
            Self::Or(left, right) => left.evaluate(data, group) || right.evaluate(data, group),
        }
    }
}

#[derive(Debug)]
enum CompiledHavingOperand {
    GroupColumn {
        position: usize,
        data_type: DataType,
    },
    Aggregate {
        state: usize,
        data_type: DataType,
    },
    Literal(Value),
}

impl CompiledHavingOperand {
    fn data_type(&self) -> DataType {
        match self {
            Self::GroupColumn { data_type, .. } | Self::Aggregate { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
        }
    }

    fn value<'a>(&'a self, data: &'a GroupedData<'a>, group: usize) -> ValueRef<'a> {
        match self {
            Self::GroupColumn { position, .. } => data.keys[group].value(*position),
            Self::Aggregate { state, .. } => data.aggregates[*state][group].as_ref(),
            Self::Literal(value) => value.as_ref(),
        }
    }

    fn same_reference(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::GroupColumn { position: left, .. },
                Self::GroupColumn {
                    position: right, ..
                },
            ) => left == right,
            (Self::Aggregate { state: left, .. }, Self::Aggregate { state: right, .. }) => {
                left == right
            }
            _ => false,
        }
    }
}

fn compile_having(
    table: &Table,
    predicate: &Predicate,
    group_columns: &[usize],
    requested_items: &[SelectItem],
    resolved_items: &[ResolvedItem],
    aggregate_specs: &mut Vec<AggregateSpec>,
) -> Result<CompiledHavingPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_having_operand(
                table,
                left,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?;
            let right = compile_having_operand(
                table,
                right,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?;
            if !comparable(left.data_type(), right.data_type()) {
                return Err(Error::TypeMismatch {
                    context: "HAVING comparison".to_owned(),
                    expected: left.data_type().to_string(),
                    actual: right.data_type().to_string(),
                });
            }
            Ok(CompiledHavingPredicate::Comparison {
                left,
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(CompiledHavingPredicate::And(
            Box::new(compile_having(
                table,
                left,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?),
            Box::new(compile_having(
                table,
                right,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?),
        )),
        Predicate::Or(left, right) => Ok(CompiledHavingPredicate::Or(
            Box::new(compile_having(
                table,
                left,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?),
            Box::new(compile_having(
                table,
                right,
                group_columns,
                requested_items,
                resolved_items,
                aggregate_specs,
            )?),
        )),
    }
}

fn compile_having_operand(
    table: &Table,
    operand: &Operand,
    group_columns: &[usize],
    requested_items: &[SelectItem],
    resolved_items: &[ResolvedItem],
    aggregate_specs: &mut Vec<AggregateSpec>,
) -> Result<CompiledHavingOperand> {
    match operand {
        Operand::Literal(value) => Ok(CompiledHavingOperand::Literal(value.clone())),
        Operand::Aggregate { function, argument } => {
            let (spec, _) = resolve_aggregate(table, *function, argument)?;
            let data_type = aggregate_output_type(spec.function, spec.input_type);
            let state = intern_aggregate(aggregate_specs, spec);
            Ok(CompiledHavingOperand::Aggregate { state, data_type })
        }
        Operand::Column(name) => resolve_having_name(
            table,
            name,
            group_columns,
            requested_items,
            resolved_items,
            aggregate_specs,
        ),
    }
}

fn resolve_having_name(
    table: &Table,
    name: &str,
    group_columns: &[usize],
    requested_items: &[SelectItem],
    resolved_items: &[ResolvedItem],
    aggregate_specs: &[AggregateSpec],
) -> Result<CompiledHavingOperand> {
    let aliases = requested_items
        .iter()
        .zip(resolved_items)
        .filter(|(requested, _)| {
            select_item_alias(requested).is_some_and(|alias| alias.eq_ignore_ascii_case(name))
        })
        .collect::<Vec<_>>();
    if aliases.len() > 1 {
        return Err(Error::InvalidQuery(format!(
            "HAVING name '{name}' is ambiguous"
        )));
    }

    let mut resolved = Vec::with_capacity(2);
    if let Some((_, item)) = aliases.first() {
        resolved.push(having_operand_for_item(table, item, aggregate_specs, name)?);
    }

    let source = table
        .schema()
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(name));
    if let Some((source, position)) = source.and_then(|source| {
        group_columns
            .iter()
            .position(|group| *group == source)
            .map(|position| (source, position))
    }) {
        let direct = CompiledHavingOperand::GroupColumn {
            position,
            data_type: table.schema()[source].data_type,
        };
        if resolved
            .first()
            .is_none_or(|existing| !existing.same_reference(&direct))
        {
            resolved.push(direct);
        }
    }

    match resolved.len() {
        1 => Ok(resolved.pop().expect("one resolved HAVING name")),
        2 => Err(Error::InvalidQuery(format!(
            "HAVING name '{name}' is ambiguous"
        ))),
        _ if source.is_some() => Err(Error::InvalidQuery(format!(
            "column '{name}' in HAVING must appear in GROUP BY or be aggregated"
        ))),
        _ => Err(Error::ColumnNotFound {
            table: table.name().to_owned(),
            column: name.to_owned(),
        }),
    }
}

fn select_item_alias(item: &SelectItem) -> Option<&str> {
    match item {
        SelectItem::Column { alias, .. } | SelectItem::Aggregate { alias, .. } => alias.as_deref(),
        SelectItem::Wildcard => None,
    }
}

fn having_operand_for_item(
    table: &Table,
    item: &ResolvedItem,
    aggregate_specs: &[AggregateSpec],
    name: &str,
) -> Result<CompiledHavingOperand> {
    match item {
        ResolvedItem::Column {
            source,
            group_position: Some(position),
        } => Ok(CompiledHavingOperand::GroupColumn {
            position: *position,
            data_type: table.schema()[*source].data_type,
        }),
        ResolvedItem::Column {
            group_position: None,
            ..
        } => Err(Error::InvalidQuery(format!(
            "column alias '{name}' in HAVING must refer to a GROUP BY column"
        ))),
        ResolvedItem::Aggregate { state } => {
            let spec = &aggregate_specs[*state];
            Ok(CompiledHavingOperand::Aggregate {
                state: *state,
                data_type: aggregate_output_type(spec.function, spec.input_type),
            })
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
        .map(|row| project_source_row(table, *row, items))
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

fn retain_distinct_source_rows(rows: &mut Vec<usize>, table: &Table, items: &[ResolvedItem]) {
    let mut seen = HashSet::with_capacity(rows.len());
    rows.retain(|row| seen.insert(project_source_row(table, *row, items)));
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

    fn project(&self, selected: &[usize], items: &[ResolvedItem]) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| self.project_group(*group, items))
            .collect()
    }

    fn project_group(&self, group: usize, items: &[ResolvedItem]) -> Vec<Value> {
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

fn retain_distinct_grouped_rows(
    groups: &mut Vec<usize>,
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
) {
    let mut seen = HashSet::with_capacity(groups.len());
    groups.retain(|group| seen.insert(data.project_group(*group, items)));
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    CountDistinct(HashSet<Value>),
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
            AggregateFunction::Count if spec.distinct => Self::CountDistinct(HashSet::new()),
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
            Self::CountDistinct(values) => {
                let column = &table.columns()[spec.argument.expect("COUNT DISTINCT argument")];
                values.insert(column.value(row));
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
            Self::CountDistinct(values) => i64::try_from(values.len())
                .map(Value::Int64)
                .map_err(|_| Error::NumericOverflow("COUNT(DISTINCT)".to_owned())),
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
                comparison_is_true(comparison, *operator)
            }
            Self::And(left, right) => left.evaluate(table, row) && right.evaluate(table, row),
            Self::Or(left, right) => left.evaluate(table, row) || right.evaluate(table, row),
        }
    }
}

fn comparison_is_true(comparison: Ordering, operator: ComparisonOperator) -> bool {
    match operator {
        ComparisonOperator::Equal => comparison == Ordering::Equal,
        ComparisonOperator::NotEqual => comparison != Ordering::Equal,
        ComparisonOperator::Less => comparison == Ordering::Less,
        ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
        ComparisonOperator::Greater => comparison == Ordering::Greater,
        ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
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
        Operand::Aggregate { function, .. } => Err(Error::InvalidQuery(format!(
            "aggregate expression {}(...) is not allowed in WHERE",
            function.name()
        ))),
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
