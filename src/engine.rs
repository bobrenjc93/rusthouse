use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, ExecutionLimit, Result};
use crate::execution::{CancellationToken, ExecutionLimits, ExecutionOptions};
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

#[derive(Debug)]
struct ExecutionControl<'a> {
    limits: Option<&'a ExecutionLimits>,
    cancellation_token: Option<&'a CancellationToken>,
    scanned_rows: usize,
    output_rows: usize,
}

impl<'a> ExecutionControl<'a> {
    fn unlimited() -> Self {
        Self {
            limits: None,
            cancellation_token: None,
            scanned_rows: 0,
            output_rows: 0,
        }
    }

    fn new(options: &'a ExecutionOptions) -> Self {
        Self {
            limits: Some(&options.limits),
            cancellation_token: Some(&options.cancellation_token),
            scanned_rows: 0,
            output_rows: 0,
        }
    }

    fn checkpoint(&self) -> Result<()> {
        let Some(limits) = self.limits else {
            return Ok(());
        };
        if self
            .cancellation_token
            .expect("controlled execution has a cancellation token")
            .is_cancelled()
        {
            return Err(Error::ExecutionCancelled);
        }
        if limits
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Err(Error::DeadlineExceeded);
        }
        Ok(())
    }

    fn is_unlimited(&self) -> bool {
        self.limits.is_none() && self.cancellation_token.is_none()
    }

    fn record_scanned_row(&mut self) -> Result<()> {
        let Some(limits) = self.limits else {
            return Ok(());
        };
        self.checkpoint()?;
        self.scanned_rows = self.scanned_rows.saturating_add(1);
        if let Some(maximum) = limits.max_scan_rows
            && self.scanned_rows > maximum
        {
            return Err(Error::ExecutionLimitExceeded {
                limit: ExecutionLimit::ScanRows,
                maximum,
                actual: self.scanned_rows,
            });
        }
        Ok(())
    }

    fn record_output_row(&mut self) -> Result<()> {
        let Some(limits) = self.limits else {
            return Ok(());
        };
        self.checkpoint()?;
        self.output_rows = self.output_rows.saturating_add(1);
        if let Some(maximum) = limits.max_output_rows
            && self.output_rows > maximum
        {
            return Err(Error::ExecutionLimitExceeded {
                limit: ExecutionLimit::OutputRows,
                maximum,
                actual: self.output_rows,
            });
        }
        Ok(())
    }
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
        let statements = sql::parse(sql)?;
        let mut control = ExecutionControl::unlimited();
        self.execute_statements(statements, &mut control)
    }

    /// Execute a batch with resource limits and cooperative cancellation.
    ///
    /// Row counters are cumulative across all SELECT statements in the batch.
    /// Completed commands remain applied if a later statement is aborted.
    pub fn execute_with_options(
        &mut self,
        sql: &str,
        options: impl Into<ExecutionOptions>,
    ) -> Result<Vec<StatementResult>> {
        let options = options.into();
        let statements = sql::parse(sql)?;
        let mut control = ExecutionControl::new(&options);
        self.execute_statements(statements, &mut control)
    }

    fn execute_statements(
        &mut self,
        statements: Vec<Statement>,
        control: &mut ExecutionControl<'_>,
    ) -> Result<Vec<StatementResult>> {
        statements
            .into_iter()
            .map(|statement| self.execute_statement(statement, control))
            .collect()
    }

    fn execute_statement(
        &mut self,
        statement: Statement,
        control: &mut ExecutionControl<'_>,
    ) -> Result<StatementResult> {
        control.checkpoint()?;
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
            Statement::Select(select) => self
                .execute_select(select, control)
                .map(StatementResult::Query),
        }
    }

    fn execute_select(
        &self,
        select: Select,
        control: &mut ExecutionControl<'_>,
    ) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let mut matching_rows = Vec::new();
        for row in 0..table.row_count() {
            control.record_scanned_row()?;
            if predicate
                .as_ref()
                .is_none_or(|predicate| predicate.evaluate(table, row))
            {
                matching_rows.push(row);
            }
        }
        control.checkpoint()?;

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                &matching_rows,
                &group_columns,
                &aggregate_specs,
                control,
            )?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
                control,
            )?;
            grouped.project(&selected_groups, &items, control)?
        } else {
            order_source_rows(
                &mut matching_rows,
                table,
                &items,
                &ordering,
                select.limit,
                control,
            )?;
            execute_projection(table, &matching_rows, &items, control)?
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
    control: &mut ExecutionControl<'_>,
) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::with_capacity(matching_rows.len());
    for row in matching_rows {
        control.record_output_row()?;
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            control.checkpoint()?;
            values.push(match item {
                ResolvedItem::Column { source, .. } => table.columns()[*source].value(*row),
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("projection does not contain aggregates")
                }
            });
        }
        rows.push(values);
    }
    control.checkpoint()?;
    Ok(rows)
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    control: &ExecutionControl<'_>,
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
        control.checkpoint()?;
        let (group, inserted) =
            groups.find_or_insert(table, group_columns, *row, group_count, control)?;
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                control.checkpoint()?;
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            control.checkpoint()?;
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, table, *row)?;
        }
    }

    let keys = groups.into_keys(group_count, control)?;
    let aggregates = aggregate_states
        .into_iter()
        .map(|states| {
            control.checkpoint()?;
            states
                .into_iter()
                .map(|state| {
                    control.checkpoint()?;
                    state.finish()
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    control.checkpoint()?;
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
        control: &ExecutionControl<'_>,
    ) -> Result<(usize, bool)> {
        Ok(match self {
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
                let mut key = Vec::with_capacity(columns.len());
                for column in columns {
                    control.checkpoint()?;
                    key.push(table.columns()[*column].value_ref(row));
                }
                find_or_insert_group(groups, &key, next_group)
            }
        })
    }

    fn into_keys(
        self,
        group_count: usize,
        control: &ExecutionControl<'_>,
    ) -> Result<Vec<GroupKey<'a>>> {
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
                    control.checkpoint()?;
                    ordered[group] = Some(GroupKey::One(key));
                }
            }
            Self::Multiple(groups) => {
                for (key, group) in groups {
                    control.checkpoint()?;
                    ordered[group] = Some(GroupKey::Multiple(key));
                }
            }
        }
        let mut keys = Vec::with_capacity(group_count);
        for key in ordered {
            control.checkpoint()?;
            keys.push(key.expect("every group index has a key"));
        }
        control.checkpoint()?;
        Ok(keys)
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
        control: &mut ExecutionControl<'_>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::with_capacity(selected.len());
        for group in selected {
            control.record_output_row()?;
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                control.checkpoint()?;
                values.push(match item {
                    ResolvedItem::Column {
                        group_position: Some(position),
                        ..
                    } => self.keys[*group].value(*position).to_owned(),
                    ResolvedItem::Column {
                        group_position: None,
                        ..
                    } => unreachable!("grouped columns are validated"),
                    ResolvedItem::Aggregate { state } => self.aggregates[*state][*group].clone(),
                });
            }
            rows.push(values);
        }
        control.checkpoint()?;
        Ok(rows)
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
    control: &ExecutionControl<'_>,
) -> Result<()> {
    if ordering.is_empty() {
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        return control.checkpoint();
    }

    sort_and_limit(rows, limit, control, |left, right| {
        for order in ordering {
            control.checkpoint()?;
            let ResolvedItem::Column { source, .. } = items[order.output] else {
                unreachable!("ungrouped projections cannot contain aggregates")
            };
            let comparison = table.columns()[source].cmp_at(left, right);
            if comparison != Ordering::Equal {
                return Ok(if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                });
            }
        }
        Ok(left.cmp(&right))
    })
}

fn order_grouped_rows(
    groups: &mut Vec<usize>,
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    control: &ExecutionControl<'_>,
) -> Result<()> {
    sort_and_limit(groups, limit, control, |left, right| {
        for order in ordering {
            control.checkpoint()?;
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
                return Ok(if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                });
            }
        }
        Ok(data.keys[left].cmp(&data.keys[right]))
    })
}

fn sort_and_limit(
    indices: &mut Vec<usize>,
    limit: Option<usize>,
    control: &ExecutionControl<'_>,
    mut compare: impl FnMut(usize, usize) -> Result<Ordering>,
) -> Result<()> {
    control.checkpoint()?;
    if let Some(0) = limit {
        indices.clear();
        return Ok(());
    }

    if control.is_unlimited() {
        if let Some(limit) = limit.filter(|limit| *limit < indices.len()) {
            indices.select_nth_unstable_by(limit, |left, right| {
                compare(*left, *right).expect("unlimited comparison cannot be interrupted")
            });
            indices.truncate(limit);
        }
        indices.sort_unstable_by(|left, right| {
            compare(*left, *right).expect("unlimited comparison cannot be interrupted")
        });
        return Ok(());
    }

    if let Some(limit) = limit.filter(|limit| *limit < indices.len()) {
        retain_top_k(indices, limit, &mut compare)?;
    }
    fallible_sort(indices, &mut compare)?;
    control.checkpoint()
}

fn retain_top_k(
    indices: &mut Vec<usize>,
    limit: usize,
    compare: &mut impl FnMut(usize, usize) -> Result<Ordering>,
) -> Result<()> {
    debug_assert!(limit > 0 && limit < indices.len());
    for root in (0..limit / 2).rev() {
        sift_down_max(&mut indices[..limit], root, compare)?;
    }
    for candidate in limit..indices.len() {
        if compare(indices[candidate], indices[0])? == Ordering::Less {
            indices[0] = indices[candidate];
            sift_down_max(&mut indices[..limit], 0, compare)?;
        }
    }
    indices.truncate(limit);
    Ok(())
}

fn sift_down_max(
    heap: &mut [usize],
    mut root: usize,
    compare: &mut impl FnMut(usize, usize) -> Result<Ordering>,
) -> Result<()> {
    loop {
        let left = root * 2 + 1;
        if left >= heap.len() {
            return Ok(());
        }
        let right = left + 1;
        let largest = if right < heap.len() && compare(heap[left], heap[right])? == Ordering::Less {
            right
        } else {
            left
        };
        if compare(heap[root], heap[largest])? != Ordering::Less {
            return Ok(());
        }
        heap.swap(root, largest);
        root = largest;
    }
}

fn fallible_sort(
    indices: &mut [usize],
    compare: &mut impl FnMut(usize, usize) -> Result<Ordering>,
) -> Result<()> {
    let mut scratch = indices.to_vec();
    let mut width = 1;
    while width < indices.len() {
        let mut start = 0;
        while start < indices.len() {
            let middle = start.saturating_add(width).min(indices.len());
            let end = middle.saturating_add(width).min(indices.len());
            let (mut left, mut right) = (start, middle);
            for destination in &mut scratch[start..end] {
                let take_left = if left == middle {
                    false
                } else if right == end {
                    true
                } else {
                    compare(indices[left], indices[right])? != Ordering::Greater
                };
                if take_left {
                    *destination = indices[left];
                    left += 1;
                } else {
                    *destination = indices[right];
                    right += 1;
                }
            }
            start = end;
        }
        indices.copy_from_slice(&scratch);
        width = width.saturating_mul(2);
    }
    Ok(())
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
