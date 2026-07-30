use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;

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

/// Receives statement results as execution produces them.
///
/// Rows are iterators of borrowed column values, so a sink can consume a
/// projection without the engine allocating or retaining output rows.
pub trait RowSink {
    type Error;

    /// Receive a successfully executed non-query statement.
    fn command(
        &mut self,
        _tag: &'static str,
        _affected_rows: usize,
    ) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    /// Start a SELECT result set.
    fn begin_query(&mut self, columns: &[ResultColumn]) -> std::result::Result<(), Self::Error>;

    /// Consume one projected row.
    fn row<'a, I>(&mut self, values: I) -> std::result::Result<(), Self::Error>
    where
        I: ExactSizeIterator<Item = ValueRef<'a>>;

    /// Finish the current SELECT result set.
    fn end_query(&mut self) -> std::result::Result<(), Self::Error>;
}

/// A failure produced either by SQL execution or by a [`RowSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionError<E> {
    Database(Error),
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for ExecutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::Sink(error) => write!(formatter, "row sink failed: {error}"),
        }
    }
}

impl<E> std::error::Error for ExecutionError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Sink(error) => Some(error),
        }
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
        let mut sink = CollectingSink::default();
        match self.execute_with_sink(sql, &mut sink) {
            Ok(()) => Ok(sink.results),
            Err(ExecutionError::Database(error)) => Err(error),
            Err(ExecutionError::Sink(error)) => match error {},
        }
    }

    /// Execute a SQL batch and stream every result into `sink`.
    ///
    /// Ungrouped SELECTs without ORDER BY are scanned, filtered, and projected
    /// directly into the sink. Ordered and grouped SELECTs use the materialized
    /// execution path required by those operations. The complete batch is
    /// parsed before any callback or catalog mutation occurs.
    pub fn execute_with_sink<S>(
        &mut self,
        sql: &str,
        sink: &mut S,
    ) -> std::result::Result<(), ExecutionError<S::Error>>
    where
        S: RowSink,
    {
        let statements = sql::parse(sql).map_err(ExecutionError::Database)?;
        for statement in statements {
            self.execute_statement_with_sink(statement, sink)?;
        }
        Ok(())
    }

    fn execute_statement_with_sink<S>(
        &mut self,
        statement: Statement,
        sink: &mut S,
    ) -> std::result::Result<(), ExecutionError<S::Error>>
    where
        S: RowSink,
    {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.catalog
                    .create_table(name, columns)
                    .map_err(ExecutionError::Database)?;
                sink.command("CREATE TABLE", 0)
                    .map_err(ExecutionError::Sink)
            }
            Statement::Insert { table, rows } => {
                let affected_rows = rows.len();
                {
                    let target = self
                        .catalog
                        .table(&table)
                        .map_err(ExecutionError::Database)?;
                    for row in &rows {
                        target.validate_row(row).map_err(ExecutionError::Database)?;
                    }
                }
                let target = self
                    .catalog
                    .table_mut(&table)
                    .map_err(ExecutionError::Database)?;
                for row in rows {
                    target.insert_row(row).map_err(ExecutionError::Database)?;
                }
                sink.command("INSERT", affected_rows)
                    .map_err(ExecutionError::Sink)
            }
            Statement::Select(select) => self.execute_select_with_sink(select, sink).map(|_| ()),
        }
    }

    fn execute_select_with_sink<S>(
        &self,
        select: Select,
        sink: &mut S,
    ) -> std::result::Result<usize, ExecutionError<S::Error>>
    where
        S: RowSink,
    {
        let table = self
            .catalog
            .table(&select.table)
            .map_err(ExecutionError::Database)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()
            .map_err(ExecutionError::Database)?;

        let group_columns =
            resolve_group_columns(table, &select.group_by).map_err(ExecutionError::Database)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)
                .map_err(ExecutionError::Database)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)
            .map_err(ExecutionError::Database)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        if !grouped && ordering.is_empty() {
            sink.begin_query(&result_columns)
                .map_err(ExecutionError::Sink)?;
            let mut examined = 0;
            let mut emitted = 0;
            if select.limit != Some(0) {
                for row in 0..table.row_count() {
                    examined += 1;
                    if predicate
                        .as_ref()
                        .is_some_and(|predicate| !predicate.evaluate(table, row))
                    {
                        continue;
                    }
                    sink.row(items.iter().map(|item| match item {
                        ResolvedItem::Column { source, .. } => {
                            table.columns()[*source].value_ref(row)
                        }
                        ResolvedItem::Aggregate { .. } => {
                            unreachable!("projection does not contain aggregates")
                        }
                    }))
                    .map_err(ExecutionError::Sink)?;
                    emitted += 1;
                    if select.limit == Some(emitted) {
                        break;
                    }
                }
            }
            sink.end_query().map_err(ExecutionError::Sink)?;
            return Ok(examined);
        }

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();

        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)
                .map_err(ExecutionError::Database)?;
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
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            execute_projection(table, &matching_rows, &items)
        };

        emit_materialized(
            sink,
            &QueryResult {
                columns: result_columns,
                rows,
            },
        )?;
        Ok(table.row_count())
    }
}

fn emit_materialized<S>(
    sink: &mut S,
    result: &QueryResult,
) -> std::result::Result<(), ExecutionError<S::Error>>
where
    S: RowSink,
{
    sink.begin_query(&result.columns)
        .map_err(ExecutionError::Sink)?;
    for row in &result.rows {
        sink.row(row.iter().map(Value::as_ref))
            .map_err(ExecutionError::Sink)?;
    }
    sink.end_query().map_err(ExecutionError::Sink)
}

#[derive(Debug, Default)]
struct CollectingSink {
    results: Vec<StatementResult>,
    current: Option<QueryResult>,
}

impl RowSink for CollectingSink {
    type Error = Infallible;

    fn command(
        &mut self,
        tag: &'static str,
        affected_rows: usize,
    ) -> std::result::Result<(), Self::Error> {
        self.results
            .push(StatementResult::Command { tag, affected_rows });
        Ok(())
    }

    fn begin_query(&mut self, columns: &[ResultColumn]) -> std::result::Result<(), Self::Error> {
        debug_assert!(self.current.is_none());
        self.current = Some(QueryResult {
            columns: columns.to_vec(),
            rows: Vec::new(),
        });
        Ok(())
    }

    fn row<'a, I>(&mut self, values: I) -> std::result::Result<(), Self::Error>
    where
        I: ExactSizeIterator<Item = ValueRef<'a>>,
    {
        self.current
            .as_mut()
            .expect("a query starts before its rows")
            .rows
            .push(values.map(ValueRef::into_owned).collect());
        Ok(())
    }

    fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
        let result = self.current.take().expect("a query is active");
        self.results.push(StatementResult::Query(result));
        Ok(())
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
                        } => self.keys[*group].value(*position).into_owned(),
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
                    *current = Some(candidate.into_owned());
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.into_owned());
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

    #[derive(Debug, Default)]
    struct RecordingSink {
        events: Vec<String>,
        rows: Vec<Vec<Value>>,
        max_row_width: usize,
    }

    impl RowSink for RecordingSink {
        type Error = Infallible;

        fn command(
            &mut self,
            tag: &'static str,
            affected_rows: usize,
        ) -> std::result::Result<(), Self::Error> {
            self.events.push(format!("{tag}:{affected_rows}"));
            Ok(())
        }

        fn begin_query(
            &mut self,
            columns: &[ResultColumn],
        ) -> std::result::Result<(), Self::Error> {
            self.events.push(format!(
                "begin:{}",
                columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn row<'a, I>(&mut self, values: I) -> std::result::Result<(), Self::Error>
        where
            I: ExactSizeIterator<Item = ValueRef<'a>>,
        {
            self.max_row_width = self.max_row_width.max(values.len());
            let row = values.map(ValueRef::into_owned).collect::<Vec<_>>();
            self.events.push(format!("row:{}", row.len()));
            self.rows.push(row);
            Ok(())
        }

        fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
            self.events.push("end".to_owned());
            Ok(())
        }
    }

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
    fn streams_multiple_statements_in_execution_order() {
        let mut database = Database::new();
        let mut sink = RecordingSink::default();
        database
            .execute_with_sink(
                "CREATE TABLE valueset (id Int64, label String); \
                 INSERT INTO valueset VALUES (1, 'one'), (2, 'two'); \
                 SELECT label FROM valueset WHERE id = 1; \
                 SELECT id, label FROM valueset WHERE id = 2;",
                &mut sink,
            )
            .expect("batch streams");

        assert_eq!(
            sink.events,
            [
                "CREATE TABLE:0",
                "INSERT:2",
                "begin:label",
                "row:1",
                "end",
                "begin:id,label",
                "row:2",
                "end",
            ]
        );
        assert_eq!(
            sink.rows,
            vec![
                vec![Value::String("one".to_owned())],
                vec![Value::Int64(2), Value::String("two".to_owned())],
            ]
        );
    }

    #[test]
    fn unordered_limit_stops_scanning_after_the_last_match() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE valueset (id Int64); \
                 INSERT INTO valueset VALUES (1), (2), (3), (4), (5), (6);",
            )
            .expect("setup");

        let Statement::Select(select) = sql::parse("SELECT id FROM valueset WHERE id >= 3 LIMIT 2")
            .expect("parse")
            .pop()
            .expect("statement")
        else {
            panic!("expected SELECT");
        };
        let mut sink = RecordingSink::default();
        let examined = database
            .execute_select_with_sink(select, &mut sink)
            .expect("query streams");
        assert_eq!(examined, 4);
        assert_eq!(
            sink.rows,
            vec![vec![Value::Int64(3)], vec![Value::Int64(4)]]
        );

        let Statement::Select(select) = sql::parse("SELECT id FROM valueset LIMIT 0")
            .expect("parse")
            .pop()
            .expect("statement")
        else {
            panic!("expected SELECT");
        };
        let examined = database
            .execute_select_with_sink(select, &mut RecordingSink::default())
            .expect("zero limit streams");
        assert_eq!(examined, 0);
    }

    #[test]
    fn streaming_keeps_only_the_current_borrowed_row_at_the_sink_boundary() {
        let mut database = Database::new();
        let values = (0..2_000)
            .map(|value| format!("({value}, 'label-{value}')"))
            .collect::<Vec<_>>()
            .join(",");
        database
            .execute(&format!(
                "CREATE TABLE valueset (id Int64, label String); \
                 INSERT INTO valueset VALUES {values};"
            ))
            .expect("setup");

        struct CountingSink {
            rows: usize,
            max_width: usize,
        }
        impl RowSink for CountingSink {
            type Error = Infallible;

            fn begin_query(
                &mut self,
                _columns: &[ResultColumn],
            ) -> std::result::Result<(), Self::Error> {
                Ok(())
            }

            fn row<'a, I>(&mut self, values: I) -> std::result::Result<(), Self::Error>
            where
                I: ExactSizeIterator<Item = ValueRef<'a>>,
            {
                self.max_width = self.max_width.max(values.len());
                self.rows += 1;
                for value in values {
                    std::hint::black_box(value);
                }
                Ok(())
            }

            fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
                Ok(())
            }
        }

        let mut sink = CountingSink {
            rows: 0,
            max_width: 0,
        };
        database
            .execute_with_sink("SELECT id, label FROM valueset", &mut sink)
            .expect("query streams");
        assert_eq!(sink.rows, 2_000);
        assert_eq!(sink.max_width, 2);
    }

    #[test]
    fn sink_errors_stop_execution_and_remain_distinct() {
        struct FailingSink {
            rows_seen: usize,
            ended: bool,
        }
        impl RowSink for FailingSink {
            type Error = &'static str;

            fn begin_query(
                &mut self,
                _columns: &[ResultColumn],
            ) -> std::result::Result<(), Self::Error> {
                Ok(())
            }

            fn row<'a, I>(&mut self, _values: I) -> std::result::Result<(), Self::Error>
            where
                I: ExactSizeIterator<Item = ValueRef<'a>>,
            {
                self.rows_seen += 1;
                if self.rows_seen == 2 {
                    Err("output closed")
                } else {
                    Ok(())
                }
            }

            fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
                self.ended = true;
                Ok(())
            }
        }

        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE valueset (id Int64); \
                 INSERT INTO valueset VALUES (1), (2), (3);",
            )
            .expect("setup");
        let mut sink = FailingSink {
            rows_seen: 0,
            ended: false,
        };
        let error = database
            .execute_with_sink(
                "SELECT id FROM valueset; SELECT id FROM valueset;",
                &mut sink,
            )
            .expect_err("sink rejects the second row");

        assert_eq!(error, ExecutionError::Sink("output closed"));
        assert_eq!(sink.rows_seen, 2);
        assert!(!sink.ended);
    }

    #[test]
    fn ordered_and_grouped_queries_feed_the_same_sink_contract() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE valueset (label String, amount Int64); \
                 INSERT INTO valueset VALUES ('b', 2), ('a', 1), ('b', 3);",
            )
            .expect("setup");

        let expected_ordered = query(
            &mut database,
            "SELECT label, amount FROM valueset ORDER BY amount DESC LIMIT 2",
        );
        let mut ordered = RecordingSink::default();
        database
            .execute_with_sink(
                "SELECT label, amount FROM valueset ORDER BY amount DESC LIMIT 2",
                &mut ordered,
            )
            .expect("ordered query streams materialized rows");
        assert_eq!(ordered.rows, expected_ordered.rows);

        let expected_grouped = query(
            &mut database,
            "SELECT label, SUM(amount) AS total FROM valueset GROUP BY label ORDER BY total",
        );
        let mut grouped = RecordingSink::default();
        database
            .execute_with_sink(
                "SELECT label, SUM(amount) AS total FROM valueset GROUP BY label ORDER BY total",
                &mut grouped,
            )
            .expect("grouped query streams materialized rows");
        assert_eq!(grouped.rows, expected_grouped.rows);
    }
}
