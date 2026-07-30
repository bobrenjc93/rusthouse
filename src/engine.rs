use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Limit, Operand, OrderBy,
    Predicate, Select, SelectItem, Statement, ValueExpression,
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

/// A parsed and resolved SELECT or INSERT statement.
///
/// Prepared statements are tied to the catalog schema generation in which
/// they were created. Data changes do not invalidate them, but schema changes
/// do.
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    schema_generation: u64,
    parameter_types: Vec<DataType>,
    plan: PreparedPlan,
}

impl PreparedStatement {
    /// Types expected for positional parameters, in binding order.
    #[must_use]
    pub fn parameter_types(&self) -> &[DataType] {
        &self.parameter_types
    }

    /// Result columns for SELECT, or `None` for INSERT.
    #[must_use]
    pub fn result_columns(&self) -> Option<&[ResultColumn]> {
        match &self.plan {
            PreparedPlan::Select(select) => Some(&select.result_columns),
            PreparedPlan::Insert(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
enum PreparedPlan {
    Select(PreparedSelect),
    Insert(PreparedInsert),
}

#[derive(Debug, Clone)]
struct PreparedSelect {
    table: String,
    predicate: Option<CompiledPredicate>,
    group_columns: Vec<usize>,
    items: Vec<ResolvedItem>,
    result_columns: Vec<ResultColumn>,
    aggregate_specs: Vec<AggregateSpec>,
    ordering: Vec<ResolvedOrder>,
    limit: Option<PreparedLimit>,
}

#[derive(Debug, Clone, Copy)]
enum PreparedLimit {
    Literal(usize),
    Parameter(usize),
}

impl PreparedLimit {
    fn resolve(self, parameters: &[Value]) -> Result<usize> {
        match self {
            Self::Literal(limit) => Ok(limit),
            Self::Parameter(index) => {
                let Value::Int64(limit) = parameters[index] else {
                    unreachable!("LIMIT parameter type is validated")
                };
                usize::try_from(limit).map_err(|_| {
                    Error::InvalidQuery("LIMIT parameter must be non-negative".to_owned())
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedInsert {
    table: String,
    rows: Vec<Vec<ValueExpression>>,
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

    /// Parse and resolve one SELECT or INSERT statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        let mut statements = sql::parse(sql)?;
        if statements.len() != 1 {
            return Err(Error::InvalidQuery(
                "prepare requires exactly one SELECT or INSERT statement".to_owned(),
            ));
        }

        let mut parameters = ParameterTypes::default();
        let plan = match statements.pop().expect("one statement") {
            Statement::Select(select) => {
                PreparedPlan::Select(self.compile_select(select, &mut parameters)?)
            }
            Statement::Insert { table, rows } => {
                PreparedPlan::Insert(self.compile_insert(table, rows, &mut parameters)?)
            }
            Statement::CreateTable { .. } => {
                return Err(Error::InvalidQuery(
                    "only SELECT and INSERT statements can be prepared".to_owned(),
                ));
            }
        };

        Ok(PreparedStatement {
            schema_generation: self.catalog.schema_generation(),
            parameter_types: parameters.finish()?,
            plan,
        })
    }

    /// Execute a prepared statement with positional bindings.
    pub fn execute_prepared(
        &mut self,
        statement: &PreparedStatement,
        parameters: &[Value],
    ) -> Result<StatementResult> {
        if statement.schema_generation != self.catalog.schema_generation() {
            return Err(Error::StalePreparedStatement);
        }
        validate_parameters(&statement.parameter_types, parameters)?;

        match &statement.plan {
            PreparedPlan::Select(select) => self
                .execute_select_plan(select, parameters)
                .map(StatementResult::Query),
            PreparedPlan::Insert(insert) => self.execute_insert_plan(insert, parameters),
        }
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
                let rows = rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|value| match value {
                                ValueExpression::Literal(value) => Ok(value),
                                ValueExpression::Parameter(_) => Err(Error::InvalidQuery(
                                    "SQL parameters require Database::prepare".to_owned(),
                                )),
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .collect::<Result<Vec<_>>>()?;
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
        let mut parameters = ParameterTypes::default();
        let plan = self.compile_select(select, &mut parameters)?;
        if !parameters.finish()?.is_empty() {
            return Err(Error::InvalidQuery(
                "SQL parameters require Database::prepare".to_owned(),
            ));
        }
        self.execute_select_plan(&plan, &[])
    }

    fn compile_select(
        &self,
        select: Select,
        parameters: &mut ParameterTypes,
    ) -> Result<PreparedSelect> {
        let table = self.catalog.table(&select.table)?;
        if let Some(predicate) = &select.predicate {
            infer_predicate_parameter_types(table, predicate, parameters)?;
        }
        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;
        let limit = match select.limit {
            Some(Limit::Literal(limit)) => Some(PreparedLimit::Literal(limit)),
            Some(Limit::Parameter(index)) => {
                parameters.resolve(index, DataType::Int64)?;
                Some(PreparedLimit::Parameter(index))
            }
            None => None,
        };
        parameters.resolve_comparisons()?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate, parameters))
            .transpose()?;

        Ok(PreparedSelect {
            table: select.table,
            predicate,
            group_columns,
            items,
            result_columns,
            aggregate_specs,
            ordering,
            limit,
        })
    }

    fn execute_select_plan(
        &self,
        select: &PreparedSelect,
        parameters: &[Value],
    ) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let limit = select
            .limit
            .map(|limit| limit.resolve(parameters))
            .transpose()?;

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                select
                    .predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row, parameters))
            })
            .collect::<Vec<_>>();

        let grouped = !select.group_columns.is_empty() || !select.aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                &matching_rows,
                &select.group_columns,
                &select.aggregate_specs,
            )?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &select.items,
                &select.ordering,
                limit,
            );
            grouped.project(&selected_groups, &select.items)
        } else {
            order_source_rows(
                &mut matching_rows,
                table,
                &select.items,
                &select.ordering,
                limit,
            );
            execute_projection(table, &matching_rows, &select.items)
        };

        Ok(QueryResult {
            columns: select.result_columns.clone(),
            rows,
        })
    }

    fn compile_insert(
        &self,
        table_name: String,
        rows: Vec<Vec<ValueExpression>>,
        parameters: &mut ParameterTypes,
    ) -> Result<PreparedInsert> {
        let table = self.catalog.table(&table_name)?;
        for row in &rows {
            if row.len() != table.schema().len() {
                return Err(Error::RowLength {
                    table: table.name().to_owned(),
                    expected: table.schema().len(),
                    actual: row.len(),
                });
            }
            for (field, value) in table.schema().iter().zip(row) {
                match value {
                    ValueExpression::Literal(value) => validate_typed_value(
                        &format!("column '{}.{}'", table.name(), field.name),
                        field.data_type,
                        value,
                    )?,
                    ValueExpression::Parameter(index) => {
                        parameters.resolve(*index, field.data_type)?;
                    }
                }
            }
        }
        Ok(PreparedInsert {
            table: table_name,
            rows,
        })
    }

    fn execute_insert_plan(
        &mut self,
        insert: &PreparedInsert,
        parameters: &[Value],
    ) -> Result<StatementResult> {
        let rows = insert
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        ValueExpression::Literal(value) => value.clone(),
                        ValueExpression::Parameter(index) => parameters[*index].clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        {
            let table = self.catalog.table(&insert.table)?;
            for row in &rows {
                table.validate_row(row)?;
            }
        }
        let affected_rows = rows.len();
        let table = self.catalog.table_mut(&insert.table)?;
        for row in rows {
            table.insert_row(row)?;
        }
        Ok(StatementResult::Command {
            tag: "INSERT",
            affected_rows,
        })
    }
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Default)]
struct ParameterTypes {
    types: Vec<Option<DataType>>,
    comparisons: Vec<(usize, usize)>,
}

impl ParameterTypes {
    fn resolve(&mut self, index: usize, data_type: DataType) -> Result<()> {
        self.ensure(index);
        match self.types[index] {
            Some(existing) if existing != data_type => Err(Error::TypeMismatch {
                context: format!("parameter ${}", index + 1),
                expected: existing.to_string(),
                actual: data_type.to_string(),
            }),
            Some(_) => Ok(()),
            None => {
                self.types[index] = Some(data_type);
                Ok(())
            }
        }
    }

    fn constrain_comparison(&mut self, left: usize, right: usize) {
        self.ensure(left.max(right));
        if left != right {
            self.comparisons.push((left, right));
        }
    }

    fn resolve_comparisons(&mut self) -> Result<()> {
        let comparisons = self.comparisons.clone();
        loop {
            let mut changed = false;
            for &(left, right) in &comparisons {
                match (self.types[left], self.types[right]) {
                    (Some(left_type), Some(right_type)) => {
                        if !comparable(left_type, right_type) {
                            return Err(Error::TypeMismatch {
                                context: format!(
                                    "comparison between parameters ${} and ${}",
                                    left + 1,
                                    right + 1
                                ),
                                expected: left_type.to_string(),
                                actual: right_type.to_string(),
                            });
                        }
                    }
                    (Some(data_type), None) => {
                        self.types[right] = Some(data_type);
                        changed = true;
                    }
                    (None, Some(data_type)) => {
                        self.types[left] = Some(data_type);
                        changed = true;
                    }
                    (None, None) => {}
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    fn data_type(&self, index: usize) -> Option<DataType> {
        self.types.get(index).copied().flatten()
    }

    fn ensure(&mut self, index: usize) {
        self.types.resize(self.types.len().max(index + 1), None);
    }

    fn finish(self) -> Result<Vec<DataType>> {
        (0..self.types.len())
            .map(|index| {
                let data_type = self.data_type(index);
                data_type.ok_or_else(|| {
                    Error::InvalidQuery(format!(
                        "parameter ${} is not used; parameter numbers must be contiguous",
                        index + 1
                    ))
                })
            })
            .collect()
    }
}

fn validate_parameters(expected: &[DataType], parameters: &[Value]) -> Result<()> {
    if expected.len() != parameters.len() {
        return Err(Error::ParameterCount {
            expected: expected.len(),
            actual: parameters.len(),
        });
    }
    for (index, (expected, value)) in expected.iter().zip(parameters).enumerate() {
        validate_typed_value(&format!("parameter ${}", index + 1), *expected, value)?;
    }
    Ok(())
}

fn validate_typed_value(context: &str, expected: DataType, value: &Value) -> Result<()> {
    if expected != value.data_type() {
        return Err(Error::TypeMismatch {
            context: context.to_owned(),
            expected: expected.to_string(),
            actual: value.data_type().to_string(),
        });
    }
    if matches!(value, Value::Float64(number) if !number.is_finite()) {
        return Err(Error::InvalidQuery(format!(
            "{context} must be a finite Float64"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone)]
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
    fn evaluate(&self, table: &Table, row: usize, parameters: &[Value]) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(table, row, parameters);
                let right = right.value(table, row, parameters);
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
            Self::And(left, right) => {
                left.evaluate(table, row, parameters) && right.evaluate(table, row, parameters)
            }
            Self::Or(left, right) => {
                left.evaluate(table, row, parameters) || right.evaluate(table, row, parameters)
            }
        }
    }
}

#[derive(Debug, Clone)]
enum CompiledOperand {
    Column { index: usize, data_type: DataType },
    Literal(Value),
    Parameter { index: usize, data_type: DataType },
}

impl CompiledOperand {
    fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
            Self::Parameter { data_type, .. } => *data_type,
        }
    }

    fn value<'a>(&'a self, table: &'a Table, row: usize, parameters: &'a [Value]) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => table.columns()[*index].value_ref(row),
            Self::Literal(value) => value.as_ref(),
            Self::Parameter { index, .. } => parameters[*index].as_ref(),
        }
    }
}

fn compile_predicate(
    table: &Table,
    predicate: &Predicate,
    parameters: &ParameterTypes,
) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let (left, right) = compile_comparison_operands(table, left, right, parameters)?;
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
            Box::new(compile_predicate(table, left, parameters)?),
            Box::new(compile_predicate(table, right, parameters)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(table, left, parameters)?),
            Box::new(compile_predicate(table, right, parameters)?),
        )),
    }
}

fn compile_comparison_operands(
    table: &Table,
    left: &Operand,
    right: &Operand,
    parameters: &ParameterTypes,
) -> Result<(CompiledOperand, CompiledOperand)> {
    match (left, right) {
        (Operand::Parameter(left), Operand::Parameter(right)) => {
            let left_type = inferred_parameter_type(parameters, *left)?;
            let right_type = inferred_parameter_type(parameters, *right)?;
            Ok((
                CompiledOperand::Parameter {
                    index: *left,
                    data_type: left_type,
                },
                CompiledOperand::Parameter {
                    index: *right,
                    data_type: right_type,
                },
            ))
        }
        (Operand::Parameter(index), right) => {
            let right = compile_typed_operand(table, right)?;
            let data_type = inferred_parameter_type(parameters, *index)?;
            Ok((
                CompiledOperand::Parameter {
                    index: *index,
                    data_type,
                },
                right,
            ))
        }
        (left, Operand::Parameter(index)) => {
            let left = compile_typed_operand(table, left)?;
            let data_type = inferred_parameter_type(parameters, *index)?;
            Ok((
                left,
                CompiledOperand::Parameter {
                    index: *index,
                    data_type,
                },
            ))
        }
        (left, right) => Ok((
            compile_typed_operand(table, left)?,
            compile_typed_operand(table, right)?,
        )),
    }
}

fn infer_predicate_parameter_types(
    table: &Table,
    predicate: &Predicate,
    parameters: &mut ParameterTypes,
) -> Result<()> {
    match predicate {
        Predicate::Comparison { left, right, .. } => match (left, right) {
            (Operand::Parameter(left), Operand::Parameter(right)) => {
                parameters.constrain_comparison(*left, *right);
                Ok(())
            }
            (Operand::Parameter(index), right) => {
                parameters.resolve(*index, operand_data_type(table, right)?)
            }
            (left, Operand::Parameter(index)) => {
                parameters.resolve(*index, operand_data_type(table, left)?)
            }
            _ => Ok(()),
        },
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            infer_predicate_parameter_types(table, left, parameters)?;
            infer_predicate_parameter_types(table, right, parameters)
        }
    }
}

fn operand_data_type(table: &Table, operand: &Operand) -> Result<DataType> {
    match operand {
        Operand::Column(name) => {
            let index = table.column_index(name)?;
            Ok(table.schema()[index].data_type)
        }
        Operand::Literal(value) => Ok(value.data_type()),
        Operand::Parameter(_) => {
            unreachable!("parameter pairs are handled as comparison constraints")
        }
    }
}

fn inferred_parameter_type(parameters: &ParameterTypes, index: usize) -> Result<DataType> {
    parameters.data_type(index).ok_or_else(|| {
        Error::InvalidQuery(format!(
            "cannot infer a type for parameter ${index}; compare it with a column or literal",
            index = index + 1
        ))
    })
}

fn compile_typed_operand(table: &Table, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = table.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: table.schema()[index].data_type,
            })
        }
        Operand::Literal(value) => Ok(CompiledOperand::Literal(value.clone())),
        Operand::Parameter(_) => unreachable!("parameters are resolved from the other operand"),
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
