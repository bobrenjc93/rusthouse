use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

const MAX_SUBQUERY_ROWS: usize = 10_000;

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
            Statement::Select(select) => self.execute_select(&select).map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: &Select) -> Result<QueryResult> {
        let plan = self.resolve_select(select, &[])?;
        self.execute_resolved_select(select, &[], plan, None)
    }

    fn resolve_select<'a>(
        &'a self,
        select: &Select,
        outer_tables: &[&'a Table],
    ) -> Result<ResolvedSelect<'a>> {
        let table = self.catalog.table(&select.table)?;
        validate_uncorrelated(table, select, outer_tables)?;
        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;
        if let Some(predicate) = &select.predicate {
            self.validate_predicate(table, predicate, outer_tables)?;
        }
        Ok(ResolvedSelect {
            table,
            group_columns,
            items,
            result_columns,
            aggregate_specs,
            ordering,
        })
    }

    fn execute_resolved_select<'a>(
        &'a self,
        select: &Select,
        outer_tables: &[&'a Table],
        plan: ResolvedSelect<'a>,
        output_bound: Option<OutputBound>,
    ) -> Result<QueryResult> {
        let ResolvedSelect {
            table,
            group_columns,
            items,
            result_columns,
            aggregate_specs,
            ordering,
        } = plan;
        if select.limit == Some(0) {
            return Ok(QueryResult {
                columns: result_columns,
                rows: Vec::new(),
            });
        }
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| self.compile_predicate(table, predicate, outer_tables))
            .transpose()?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let matching_rows = matching_rows(table, predicate.as_ref());
            let group_limit = output_bound.map(|bound| bound.max_rows);
            let grouped = execute_grouped(
                table,
                matching_rows,
                &group_columns,
                &aggregate_specs,
                group_limit,
            )?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
            );
            apply_output_bound(&selected_groups, output_bound)?;
            grouped.project(&selected_groups, &items)
        } else {
            let mut matching_rows = if let Some(bound) = output_bound {
                collect_bounded_source_rows(
                    table,
                    predicate.as_ref(),
                    &items,
                    &ordering,
                    select.limit,
                    bound,
                )?
            } else {
                matching_rows(table, predicate.as_ref()).collect::<Vec<_>>()
            };
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            apply_output_bound(&matching_rows, output_bound)?;
            execute_projection(table, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }

    fn execute_exists<'a>(
        &'a self,
        select: &Select,
        outer_tables: &[&'a Table],
        plan: ResolvedSelect<'a>,
    ) -> Result<bool> {
        if select.limit == Some(0) {
            return Ok(false);
        }
        let ResolvedSelect {
            table,
            group_columns,
            aggregate_specs,
            ..
        } = plan;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| self.compile_predicate(table, predicate, outer_tables))
            .transpose()?;

        if group_columns.is_empty() && !aggregate_specs.is_empty() {
            return Ok(true);
        }
        Ok(matching_rows(table, predicate.as_ref()).next().is_some())
    }

    fn validate_predicate<'a>(
        &'a self,
        table: &'a Table,
        predicate: &Predicate,
        outer_tables: &[&'a Table],
    ) -> Result<()> {
        match predicate {
            Predicate::Comparison { left, right, .. } => {
                let left = compile_operand(table, left)?;
                let right = compile_operand(table, right)?;
                if let (Some(left_type), Some(right_type)) = (left.data_type(), right.data_type())
                    && !comparable(left_type, right_type)
                {
                    return Err(Error::TypeMismatch {
                        context: "WHERE comparison".to_owned(),
                        expected: left_type.to_string(),
                        actual: right_type.to_string(),
                    });
                }
                Ok(())
            }
            Predicate::InSubquery {
                operand, subquery, ..
            } => {
                let operand = compile_operand(table, operand)?;
                let mut scopes = outer_tables.to_vec();
                scopes.push(table);
                let plan = self.resolve_select(subquery, &scopes)?;
                if plan.result_columns.len() != 1 {
                    return Err(Error::InvalidQuery(format!(
                        "IN subquery must return exactly one column; found {}",
                        plan.result_columns.len()
                    )));
                }
                let result_type = plan.result_columns[0].data_type;
                if operand
                    .data_type()
                    .is_some_and(|operand_type| !comparable(operand_type, result_type))
                {
                    return Err(Error::TypeMismatch {
                        context: "IN subquery".to_owned(),
                        expected: operand
                            .data_type()
                            .expect("non-null operand type was checked")
                            .to_string(),
                        actual: result_type.to_string(),
                    });
                }
                Ok(())
            }
            Predicate::Exists { subquery, .. } => {
                let mut scopes = outer_tables.to_vec();
                scopes.push(table);
                self.resolve_select(subquery, &scopes).map(|_| ())
            }
            Predicate::And(left, right) | Predicate::Or(left, right) => {
                self.validate_predicate(table, left, outer_tables)?;
                self.validate_predicate(table, right, outer_tables)
            }
        }
    }

    fn compile_predicate<'a>(
        &'a self,
        table: &'a Table,
        predicate: &Predicate,
        outer_tables: &[&'a Table],
    ) -> Result<CompiledPredicate> {
        match predicate {
            Predicate::Comparison {
                left,
                operator,
                right,
            } => {
                let left = compile_operand(table, left)?;
                let right = compile_operand(table, right)?;
                if let (Some(left_type), Some(right_type)) = (left.data_type(), right.data_type())
                    && !comparable(left_type, right_type)
                {
                    return Err(Error::TypeMismatch {
                        context: "WHERE comparison".to_owned(),
                        expected: left_type.to_string(),
                        actual: right_type.to_string(),
                    });
                }
                Ok(CompiledPredicate::Comparison {
                    left,
                    operator: *operator,
                    right,
                })
            }
            Predicate::InSubquery {
                operand,
                negated,
                subquery,
            } => {
                let operand = compile_operand(table, operand)?;
                let mut scopes = outer_tables.to_vec();
                scopes.push(table);
                let plan = self.resolve_select(subquery, &scopes)?;
                if plan.result_columns.len() != 1 {
                    return Err(Error::InvalidQuery(format!(
                        "IN subquery must return exactly one column; found {}",
                        plan.result_columns.len()
                    )));
                }
                let result_type = plan.result_columns[0].data_type;
                if operand
                    .data_type()
                    .is_some_and(|operand_type| !comparable(operand_type, result_type))
                {
                    return Err(Error::TypeMismatch {
                        context: "IN subquery".to_owned(),
                        expected: operand
                            .data_type()
                            .expect("non-null operand type was checked")
                            .to_string(),
                        actual: result_type.to_string(),
                    });
                }
                let result = self.execute_resolved_select(
                    subquery,
                    &scopes,
                    plan,
                    Some(OutputBound::materialized()),
                )?;
                Ok(CompiledPredicate::InSubquery {
                    operand,
                    negated: *negated,
                    state: MaterializedIn::new(result_type, result.rows),
                })
            }
            Predicate::Exists { negated, subquery } => {
                let mut scopes = outer_tables.to_vec();
                scopes.push(table);
                let plan = self.resolve_select(subquery, &scopes)?;
                let exists = self.execute_exists(subquery, &scopes, plan)?;
                Ok(CompiledPredicate::Exists {
                    value: if *negated { !exists } else { exists },
                })
            }
            Predicate::And(left, right) => Ok(CompiledPredicate::And(
                Box::new(self.compile_predicate(table, left, outer_tables)?),
                Box::new(self.compile_predicate(table, right, outer_tables)?),
            )),
            Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
                Box::new(self.compile_predicate(table, left, outer_tables)?),
                Box::new(self.compile_predicate(table, right, outer_tables)?),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OutputBound {
    max_rows: usize,
}

impl OutputBound {
    fn materialized() -> Self {
        Self {
            max_rows: MAX_SUBQUERY_ROWS,
        }
    }
}

fn apply_output_bound(rows: &[usize], bound: Option<OutputBound>) -> Result<()> {
    let Some(bound) = bound else {
        return Ok(());
    };
    if rows.len() > bound.max_rows {
        return Err(materialization_limit_error());
    }
    Ok(())
}

fn materialization_limit_error() -> Error {
    Error::InvalidQuery(format!(
        "subquery result exceeds materialization limit of {MAX_SUBQUERY_ROWS} rows"
    ))
}

struct ResolvedSelect<'a> {
    table: &'a Table,
    group_columns: Vec<usize>,
    items: Vec<ResolvedItem>,
    result_columns: Vec<ResultColumn>,
    aggregate_specs: Vec<AggregateSpec>,
    ordering: Vec<ResolvedOrder>,
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

fn matching_rows<'a>(
    table: &'a Table,
    predicate: Option<&'a CompiledPredicate>,
) -> impl Iterator<Item = usize> + 'a {
    (0..table.row_count()).filter(move |row| {
        predicate.is_none_or(|predicate| predicate.evaluate(table, *row).is_true())
    })
}

fn collect_bounded_source_rows(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    bound: OutputBound,
) -> Result<Vec<usize>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }

    let output_is_bounded = limit.is_some_and(|limit| limit <= bound.max_rows);
    if ordering.is_empty() {
        let row_limit = if output_is_bounded {
            limit.expect("bounded output has a LIMIT")
        } else {
            bound.max_rows
        };
        let mut rows = Vec::with_capacity(row_limit.min(1_024));
        for row in matching_rows(table, predicate) {
            if rows.len() == row_limit {
                if output_is_bounded {
                    break;
                }
                return Err(materialization_limit_error());
            }
            rows.push(row);
        }
        return Ok(rows);
    }

    if !output_is_bounded {
        let mut rows = Vec::with_capacity(bound.max_rows.min(1_024));
        for row in matching_rows(table, predicate) {
            if rows.len() == bound.max_rows {
                return Err(materialization_limit_error());
            }
            rows.push(row);
        }
        return Ok(rows);
    }

    let row_limit = limit.expect("bounded output has a LIMIT");
    let prune_at = row_limit + row_limit.clamp(1, 1_024);
    let mut rows = Vec::with_capacity(prune_at);
    for row in matching_rows(table, predicate) {
        rows.push(row);
        if rows.len() == prune_at {
            retain_best_source_rows(&mut rows, row_limit, table, items, ordering);
        }
    }
    retain_best_source_rows(&mut rows, row_limit, table, items, ordering);
    Ok(rows)
}

fn retain_best_source_rows(
    rows: &mut Vec<usize>,
    limit: usize,
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
) {
    if rows.len() <= limit {
        return;
    }
    rows.select_nth_unstable_by(limit, |left, right| {
        compare_source_rows(*left, *right, table, items, ordering)
    });
    rows.truncate(limit);
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
    matching_rows: impl Iterator<Item = usize>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    group_limit: Option<usize>,
) -> Result<GroupedData<'a>> {
    let initial_capacity = if group_columns.is_empty() {
        1
    } else {
        group_limit.unwrap_or(table.row_count()).min(1_024)
    };
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

    for row in matching_rows {
        let (group, inserted) = groups.find_or_insert(table, group_columns, row, group_count);
        if inserted {
            if group_limit.is_some_and(|limit| group_count >= limit) {
                return Err(materialization_limit_error());
            }
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

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                if spec.argument.is_some_and(|column| {
                    matches!(table.columns()[column].value_ref(row), ValueRef::Null)
                }) {
                    return Ok(());
                }
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                if let Some(value) = values[row] {
                    *sum = sum
                        .checked_add(value)
                        .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
                }
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                let Some(value) = values[row] else {
                    return Ok(());
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let column = &table.columns()[spec.argument.expect("MIN argument")];
                let candidate = column.value_ref(row);
                if matches!(candidate, ValueRef::Null) {
                    return Ok(());
                }
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
                if matches!(candidate, ValueRef::Null) {
                    return Ok(());
                }
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
                let Some(value) = values[row] else {
                    return Ok(());
                };
                *sum = sum
                    .checked_add(i128::from(value))
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
                let Some(value) = values[row] else {
                    return Ok(());
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
            Self::Min(None) | Self::Max(None) | Self::AvgInt { .. } | Self::AvgFloat { .. } => {
                Ok(Value::Null)
            }
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
        compare_source_rows(left, right, table, items, ordering)
    });
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
    InSubquery {
        operand: CompiledOperand,
        negated: bool,
        state: MaterializedIn,
    },
    Exists {
        value: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: usize) -> TruthValue {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(table, row);
                let right = right.value(table, row);
                let Some(comparison) = left.sql_cmp(right) else {
                    return TruthValue::Unknown;
                };
                TruthValue::from_bool(match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                })
            }
            Self::InSubquery {
                operand,
                negated,
                state,
            } => {
                let value = state.contains(operand.value(table, row));
                if *negated { value.not() } else { value }
            }
            Self::Exists { value } => TruthValue::from_bool(*value),
            Self::And(left, right) => left.evaluate(table, row).and(right.evaluate(table, row)),
            Self::Or(left, right) => left.evaluate(table, row).or(right.evaluate(table, row)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruthValue {
    False,
    True,
    Unknown,
}

impl TruthValue {
    fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }

    fn is_true(self) -> bool {
        self == Self::True
    }

    fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::True => Self::False,
            Self::Unknown => Self::Unknown,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
struct MaterializedIn {
    values: MaterializedValues,
    contains_null: bool,
}

impl MaterializedIn {
    fn new(data_type: DataType, rows: Vec<Vec<Value>>) -> Self {
        let (values, contains_null) = match data_type {
            DataType::Int64 => {
                let (values, contains_null) = materialize_values(
                    rows,
                    |value| {
                        let Value::Int64(value) = value else {
                            unreachable!("IN result matches its declared Int64 type")
                        };
                        value
                    },
                    i64::cmp,
                );
                (MaterializedValues::Int64(values), contains_null)
            }
            DataType::Float64 => {
                let (values, contains_null) = materialize_values(
                    rows,
                    |value| {
                        let Value::Float64(value) = value else {
                            unreachable!("IN result matches its declared Float64 type")
                        };
                        value
                    },
                    compare_f64,
                );
                (MaterializedValues::Float64(values), contains_null)
            }
            DataType::Bool => {
                let (values, contains_null) = materialize_values(
                    rows,
                    |value| {
                        let Value::Bool(value) = value else {
                            unreachable!("IN result matches its declared Bool type")
                        };
                        value
                    },
                    bool::cmp,
                );
                (MaterializedValues::Bool(values), contains_null)
            }
            DataType::String => {
                let (values, contains_null) = materialize_values(
                    rows,
                    |value| {
                        let Value::String(value) = value else {
                            unreachable!("IN result matches its declared String type")
                        };
                        value
                    },
                    String::cmp,
                );
                (MaterializedValues::String(values), contains_null)
            }
        };
        Self {
            values,
            contains_null,
        }
    }

    fn contains(&self, value: ValueRef<'_>) -> TruthValue {
        if self.values.is_empty() && !self.contains_null {
            return TruthValue::False;
        }
        if matches!(value, ValueRef::Null) {
            return TruthValue::Unknown;
        }
        let found = match (&self.values, value) {
            (MaterializedValues::Int64(values), ValueRef::Int64(_) | ValueRef::Float64(_)) => {
                values
                    .binary_search_by(|candidate| {
                        ValueRef::Int64(*candidate)
                            .sql_cmp(value)
                            .expect("numeric IN operand is comparable")
                    })
                    .is_ok()
            }
            (MaterializedValues::Float64(values), ValueRef::Int64(_) | ValueRef::Float64(_)) => {
                values
                    .binary_search_by(|candidate| {
                        ValueRef::Float64(*candidate)
                            .sql_cmp(value)
                            .expect("numeric IN operand is comparable")
                    })
                    .is_ok()
            }
            (MaterializedValues::Bool(values), ValueRef::Bool(value)) => {
                values.binary_search(&value).is_ok()
            }
            (MaterializedValues::String(values), ValueRef::String(value)) => values
                .binary_search_by(|candidate| candidate.as_str().cmp(value))
                .is_ok(),
            _ => unreachable!("IN operand types are validated"),
        };
        if found {
            TruthValue::True
        } else if self.contains_null {
            TruthValue::Unknown
        } else {
            TruthValue::False
        }
    }
}

#[derive(Debug)]
enum MaterializedValues {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl MaterializedValues {
    fn is_empty(&self) -> bool {
        match self {
            Self::Int64(values) => values.is_empty(),
            Self::Float64(values) => values.is_empty(),
            Self::Bool(values) => values.is_empty(),
            Self::String(values) => values.is_empty(),
        }
    }
}

fn materialize_values<T>(
    rows: Vec<Vec<Value>>,
    mut extract: impl FnMut(Value) -> T,
    compare: impl Fn(&T, &T) -> Ordering + Copy,
) -> (Vec<T>, bool) {
    let mut values = Vec::with_capacity(rows.len());
    let mut contains_null = false;
    for row in rows {
        let value = row.into_iter().next().expect("IN result has one column");
        if value == Value::Null {
            contains_null = true;
        } else {
            values.push(extract(value));
        }
    }
    values.sort_unstable_by(compare);
    values.dedup_by(|left, right| compare(left, right) == Ordering::Equal);
    (values, contains_null)
}

fn compare_f64(left: &f64, right: &f64) -> Ordering {
    if left == right {
        Ordering::Equal
    } else {
        left.total_cmp(right)
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

fn validate_uncorrelated(table: &Table, select: &Select, outer_tables: &[&Table]) -> Result<()> {
    for item in &select.items {
        match item {
            SelectItem::Column { name, .. } => validate_local_column(table, name, outer_tables)?,
            SelectItem::Aggregate {
                argument: AggregateArgument::Column(name),
                ..
            } => validate_local_column(table, name, outer_tables)?,
            SelectItem::Wildcard
            | SelectItem::Aggregate {
                argument: AggregateArgument::Wildcard,
                ..
            } => {}
        }
    }
    if let Some(predicate) = &select.predicate {
        validate_predicate_scope(table, predicate, outer_tables)?;
    }
    for name in &select.group_by {
        validate_local_column(table, name, outer_tables)?;
    }
    Ok(())
}

fn validate_predicate_scope(
    table: &Table,
    predicate: &Predicate,
    outer_tables: &[&Table],
) -> Result<()> {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            validate_operand_scope(table, left, outer_tables)?;
            validate_operand_scope(table, right, outer_tables)
        }
        Predicate::InSubquery { operand, .. } => {
            validate_operand_scope(table, operand, outer_tables)
        }
        Predicate::Exists { .. } => Ok(()),
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            validate_predicate_scope(table, left, outer_tables)?;
            validate_predicate_scope(table, right, outer_tables)
        }
    }
}

fn validate_operand_scope(table: &Table, operand: &Operand, outer_tables: &[&Table]) -> Result<()> {
    if let Operand::Column(name) = operand {
        validate_local_column(table, name, outer_tables)?;
    }
    Ok(())
}

fn validate_local_column(table: &Table, name: &str, outer_tables: &[&Table]) -> Result<()> {
    if table
        .schema()
        .iter()
        .any(|column| column.name.eq_ignore_ascii_case(name))
    {
        return Ok(());
    }
    if outer_tables.iter().rev().any(|outer| {
        outer
            .schema()
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(name))
    }) {
        return Err(Error::InvalidQuery(format!(
            "correlated subqueries are not supported: column '{name}' references an outer query"
        )));
    }
    Ok(())
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
