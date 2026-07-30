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

/// The maximum number of rows delivered in one [`ResultSink::rows`] call by
/// [`Database::execute_into`].
pub const DEFAULT_BATCH_SIZE: usize = 1_024;

/// Receives statement results as execution progresses.
///
/// A query is delivered as `begin_query`, zero or more bounded `rows` calls,
/// and `end_query`. Implementations that retain columns or rows must clone
/// them because the slices are only valid for the duration of each call.
pub trait ResultSink {
    type Error;

    fn command(
        &mut self,
        tag: &'static str,
        affected_rows: usize,
    ) -> std::result::Result<(), Self::Error>;

    fn begin_query(&mut self, columns: &[ResultColumn]) -> std::result::Result<(), Self::Error>;

    fn rows(&mut self, rows: &[Vec<Value>]) -> std::result::Result<(), Self::Error>;

    fn end_query(&mut self) -> std::result::Result<(), Self::Error>;
}

/// An execution failure from either the database or the result sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteError<E> {
    Database(Error),
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for ExecuteError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::Sink(error) => write!(formatter, "result sink failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ExecuteError<E> {
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
        match self.execute_into(sql, &mut sink) {
            Ok(()) => Ok(sink.results),
            Err(ExecuteError::Database(error)) => Err(error),
            Err(ExecuteError::Sink(error)) => match error {},
        }
    }

    /// Execute a parsed batch and emit results incrementally in bounded chunks.
    ///
    /// The complete SQL batch is parsed before the first statement is executed.
    /// Sink failures stop execution immediately; already executed statements and
    /// mutations from the current statement are not rolled back.
    pub fn execute_into<S: ResultSink>(
        &mut self,
        sql: &str,
        sink: &mut S,
    ) -> std::result::Result<(), ExecuteError<S::Error>> {
        self.execute_into_with_batch_size(sql, DEFAULT_BATCH_SIZE, sink)
    }

    /// Like [`Database::execute_into`], with an explicit maximum row batch size.
    pub fn execute_into_with_batch_size<S: ResultSink>(
        &mut self,
        sql: &str,
        batch_size: usize,
        sink: &mut S,
    ) -> std::result::Result<(), ExecuteError<S::Error>> {
        if batch_size == 0 {
            return Err(ExecuteError::Database(Error::InvalidQuery(
                "result batch size must be greater than zero".to_owned(),
            )));
        }

        let statements = sql::parse(sql).map_err(ExecuteError::Database)?;
        for statement in statements {
            self.execute_statement_into(statement, batch_size, sink)?;
        }
        Ok(())
    }

    fn execute_statement_into<S: ResultSink>(
        &mut self,
        statement: Statement,
        batch_size: usize,
        sink: &mut S,
    ) -> std::result::Result<(), ExecuteError<S::Error>> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.catalog
                    .create_table(name, columns)
                    .map_err(ExecuteError::Database)?;
                sink.command("CREATE TABLE", 0).map_err(ExecuteError::Sink)
            }
            Statement::Insert { table, rows } => {
                let affected_rows = rows.len();
                {
                    let target = self.catalog.table(&table).map_err(ExecuteError::Database)?;
                    for row in &rows {
                        target.validate_row(row).map_err(ExecuteError::Database)?;
                    }
                }
                let target = self
                    .catalog
                    .table_mut(&table)
                    .map_err(ExecuteError::Database)?;
                for row in rows {
                    target.insert_row(row).map_err(ExecuteError::Database)?;
                }
                sink.command("INSERT", affected_rows)
                    .map_err(ExecuteError::Sink)
            }
            Statement::Select(select) => self.execute_select_into(select, batch_size, sink),
        }
    }

    fn execute_select_into<S: ResultSink>(
        &self,
        select: Select,
        batch_size: usize,
        sink: &mut S,
    ) -> std::result::Result<(), ExecuteError<S::Error>> {
        let table = self
            .catalog
            .table(&select.table)
            .map_err(ExecuteError::Database)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()
            .map_err(ExecuteError::Database)?;

        let group_columns =
            resolve_group_columns(table, &select.group_by).map_err(ExecuteError::Database)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)
                .map_err(ExecuteError::Database)?;
        let ordering =
            resolve_ordering(&result_columns, &select.order_by).map_err(ExecuteError::Database)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        if grouped {
            let grouped =
                execute_grouped(table, predicate.as_ref(), &group_columns, &aggregate_specs)
                    .map_err(ExecuteError::Database)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
            );
            sink.begin_query(&result_columns)
                .map_err(ExecuteError::Sink)?;
            emit_grouped_rows(&grouped, &selected_groups, &items, batch_size, sink)
                .map_err(ExecuteError::Sink)?;
        } else if ordering.is_empty() {
            sink.begin_query(&result_columns)
                .map_err(ExecuteError::Sink)?;
            emit_unordered_rows(
                table,
                predicate.as_ref(),
                &items,
                select.limit,
                batch_size,
                sink,
            )
            .map_err(ExecuteError::Sink)?;
        } else {
            let mut matching_rows = (0..table.row_count())
                .filter(|row| {
                    predicate
                        .as_ref()
                        .is_none_or(|predicate| predicate.evaluate(table, *row))
                })
                .collect::<Vec<_>>();
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            sink.begin_query(&result_columns)
                .map_err(ExecuteError::Sink)?;
            emit_source_rows(table, &matching_rows, &items, batch_size, sink)
                .map_err(ExecuteError::Sink)?;
        }
        sink.end_query().map_err(ExecuteError::Sink)
    }
}

#[derive(Debug, Default)]
struct CollectingSink {
    results: Vec<StatementResult>,
    current_query: Option<QueryResult>,
}

impl ResultSink for CollectingSink {
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
        debug_assert!(self.current_query.is_none());
        self.current_query = Some(QueryResult {
            columns: columns.to_vec(),
            rows: Vec::new(),
        });
        Ok(())
    }

    fn rows(&mut self, rows: &[Vec<Value>]) -> std::result::Result<(), Self::Error> {
        self.current_query
            .as_mut()
            .expect("rows follow begin_query")
            .rows
            .extend_from_slice(rows);
        Ok(())
    }

    fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
        let query = self.current_query.take().expect("query has begun");
        self.results.push(StatementResult::Query(query));
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

fn emit_unordered_rows<S: ResultSink>(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    items: &[ResolvedItem],
    limit: Option<usize>,
    batch_size: usize,
    sink: &mut S,
) -> std::result::Result<(), S::Error> {
    let possible_rows = limit.unwrap_or(table.row_count()).min(table.row_count());
    let mut rows = Vec::with_capacity(batch_size.min(possible_rows));
    let mut emitted = 0;

    for row in 0..table.row_count() {
        if limit.is_some_and(|limit| emitted >= limit) {
            break;
        }
        if predicate.is_some_and(|predicate| !predicate.evaluate(table, row)) {
            continue;
        }

        rows.push(project_source_row(table, row, items));
        emitted += 1;
        if rows.len() == batch_size {
            sink.rows(&rows)?;
            rows.clear();
        }
    }

    if !rows.is_empty() {
        sink.rows(&rows)?;
    }
    Ok(())
}

fn emit_source_rows<S: ResultSink>(
    table: &Table,
    selected: &[usize],
    items: &[ResolvedItem],
    batch_size: usize,
    sink: &mut S,
) -> std::result::Result<(), S::Error> {
    let mut rows = Vec::with_capacity(batch_size.min(selected.len()));
    for row in selected {
        rows.push(project_source_row(table, *row, items));
        if rows.len() == batch_size {
            sink.rows(&rows)?;
            rows.clear();
        }
    }
    if !rows.is_empty() {
        sink.rows(&rows)?;
    }
    Ok(())
}

fn execute_grouped<'a>(
    table: &'a Table,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_columns.len(), table.row_count());
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = table.row_count().min(1_024);
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

    for row in 0..table.row_count() {
        if predicate.is_some_and(|predicate| !predicate.evaluate(table, row)) {
            continue;
        }
        let (group, inserted) = groups.find_or_insert(table, group_columns, row, group_count);
        if inserted {
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

    fn project_row(&self, group: usize, items: &[ResolvedItem]) -> Vec<Value> {
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

fn emit_grouped_rows<S: ResultSink>(
    data: &GroupedData<'_>,
    selected: &[usize],
    items: &[ResolvedItem],
    batch_size: usize,
    sink: &mut S,
) -> std::result::Result<(), S::Error> {
    let mut rows = Vec::with_capacity(batch_size.min(selected.len()));
    for group in selected {
        rows.push(data.project_row(*group, items));
        if rows.len() == batch_size {
            sink.rows(&rows)?;
            rows.clear();
        }
    }
    if !rows.is_empty() {
        sink.rows(&rows)?;
    }
    Ok(())
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
        #[cfg(test)]
        PREDICATE_EVALUATIONS.with(|count| count.set(count.get() + 1));

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

#[cfg(test)]
thread_local! {
    static PREDICATE_EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        results: Vec<StatementResult>,
        current: Option<QueryResult>,
        batch_sizes: Vec<usize>,
    }

    impl ResultSink for RecordingSink {
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

        fn begin_query(
            &mut self,
            columns: &[ResultColumn],
        ) -> std::result::Result<(), Self::Error> {
            self.current = Some(QueryResult {
                columns: columns.to_vec(),
                rows: Vec::new(),
            });
            Ok(())
        }

        fn rows(&mut self, rows: &[Vec<Value>]) -> std::result::Result<(), Self::Error> {
            self.batch_sizes.push(rows.len());
            self.current
                .as_mut()
                .expect("query started")
                .rows
                .extend_from_slice(rows);
            Ok(())
        }

        fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
            self.results.push(StatementResult::Query(
                self.current.take().expect("query started"),
            ));
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct CountingSink {
        rows: usize,
        batches: usize,
        max_batch: usize,
    }

    impl ResultSink for CountingSink {
        type Error = Infallible;

        fn command(
            &mut self,
            _tag: &'static str,
            _affected_rows: usize,
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn begin_query(
            &mut self,
            _columns: &[ResultColumn],
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn rows(&mut self, rows: &[Vec<Value>]) -> std::result::Result<(), Self::Error> {
            self.rows += rows.len();
            self.batches += 1;
            self.max_batch = self.max_batch.max(rows.len());
            Ok(())
        }

        fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
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
    fn streaming_and_collecting_outputs_are_equivalent_for_every_select_path() {
        let setup = "CREATE TABLE samples (id Int64, category String, amount Int64); \
                     INSERT INTO samples VALUES \
                        (1, 'a', 5), (2, 'b', 2), (3, 'a', 7), \
                        (4, 'b', 9), (5, 'c', 1);";
        let queries = "SELECT id, amount FROM samples WHERE amount >= 2 LIMIT 4; \
                       SELECT id, category, amount FROM samples WHERE amount >= 2 \
                           ORDER BY amount DESC LIMIT 3; \
                       SELECT category, COUNT(*) AS n, SUM(amount) AS total \
                           FROM samples GROUP BY category ORDER BY total DESC;";

        let mut collecting_database = Database::new();
        collecting_database.execute(setup).expect("setup succeeds");
        let expected = collecting_database
            .execute(queries)
            .expect("queries succeed");

        let mut streaming_database = Database::new();
        streaming_database.execute(setup).expect("setup succeeds");
        let mut sink = RecordingSink::default();
        streaming_database
            .execute_into_with_batch_size(queries, 2, &mut sink)
            .expect("streaming queries succeed");

        assert_eq!(sink.results, expected);
        assert_eq!(sink.batch_sizes, vec![2, 2, 2, 1, 2, 1]);
        assert!(sink.batch_sizes.iter().all(|size| *size <= 2));
    }

    #[test]
    fn streaming_rejects_an_empty_row_batch() {
        let mut database = Database::new();
        let mut sink = RecordingSink::default();
        let error = database
            .execute_into_with_batch_size("CREATE TABLE skipped (n Int64)", 0, &mut sink)
            .expect_err("zero cannot bound non-empty chunks");

        assert!(matches!(
            error,
            ExecuteError::Database(Error::InvalidQuery(message))
                if message == "result batch size must be greater than zero"
        ));
        assert!(sink.results.is_empty());
        assert!(matches!(
            database.catalog.table("skipped"),
            Err(Error::TableNotFound(_))
        ));
    }

    #[test]
    fn oversized_batch_bounds_do_not_overallocate_for_small_results() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE empty (n Int64)")
            .expect("create succeeds");
        let mut sink = RecordingSink::default();

        database
            .execute_into_with_batch_size(
                "SELECT n FROM empty; \
                 SELECT n FROM empty ORDER BY n; \
                 SELECT n, COUNT(*) AS count FROM empty GROUP BY n; \
                 SELECT COUNT(*) AS count FROM empty;",
                usize::MAX,
                &mut sink,
            )
            .expect("oversized bound is capped by each result's possible rows");

        assert_eq!(sink.results.len(), 4);
        assert_eq!(sink.batch_sizes, vec![1]);
    }

    #[test]
    fn unordered_limit_stops_scanning_as_soon_as_enough_rows_match() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE numbers (n Int64); \
                 INSERT INTO numbers VALUES (0), (1), (2), (3), (4), (5), (6), (7);",
            )
            .expect("setup succeeds");
        let mut sink = CountingSink::default();

        PREDICATE_EVALUATIONS.with(|count| count.set(0));
        database
            .execute_into("SELECT n FROM numbers WHERE n >= 0 LIMIT 3", &mut sink)
            .expect("query succeeds");

        assert_eq!(sink.rows, 3);
        PREDICATE_EVALUATIONS.with(|count| assert_eq!(count.get(), 3));

        PREDICATE_EVALUATIONS.with(|count| count.set(0));
        database
            .execute_into(
                "SELECT n FROM numbers WHERE n >= 0 LIMIT 0",
                &mut CountingSink::default(),
            )
            .expect("zero limit succeeds");
        PREDICATE_EVALUATIONS.with(|count| assert_eq!(count.get(), 0));
    }

    #[test]
    fn sink_failure_is_returned_and_stops_the_remaining_batch() {
        struct FailingSink;

        impl ResultSink for FailingSink {
            type Error = &'static str;

            fn command(
                &mut self,
                _tag: &'static str,
                _affected_rows: usize,
            ) -> std::result::Result<(), Self::Error> {
                Ok(())
            }

            fn begin_query(
                &mut self,
                _columns: &[ResultColumn],
            ) -> std::result::Result<(), Self::Error> {
                Ok(())
            }

            fn rows(&mut self, _rows: &[Vec<Value>]) -> std::result::Result<(), Self::Error> {
                Err("output closed")
            }

            fn end_query(&mut self) -> std::result::Result<(), Self::Error> {
                panic!("end_query must not follow a failed rows callback")
            }
        }

        let mut database = Database::new();
        let error = database
            .execute_into(
                "CREATE TABLE first (n Int64); \
                 INSERT INTO first VALUES (1), (2); \
                 SELECT n FROM first; \
                 CREATE TABLE not_executed (n Int64);",
                &mut FailingSink,
            )
            .expect_err("sink rejects the first row batch");

        assert_eq!(error, ExecuteError::Sink("output closed"));
        assert!(database.catalog.table("first").is_ok());
        assert!(matches!(
            database.catalog.table("not_executed"),
            Err(Error::TableNotFound(_))
        ));
    }

    #[test]
    fn streaming_scan_handles_two_million_rows_in_bounded_batches() {
        const ROW_COUNT: usize = 2_000_000;
        const BATCH_SIZE: usize = 4_096;

        let mut database = Database::new();
        database
            .execute("CREATE TABLE large_scan (n Int64)")
            .expect("create succeeds");
        let table = database
            .catalog
            .table_mut("large_scan")
            .expect("table exists");
        for value in 0..ROW_COUNT {
            table
                .insert_row(vec![Value::Int64(value as i64)])
                .expect("row is valid");
        }

        let mut sink = CountingSink::default();
        database
            .execute_into_with_batch_size(
                "SELECT n FROM large_scan WHERE n >= 0",
                BATCH_SIZE,
                &mut sink,
            )
            .expect("large scan succeeds");

        assert_eq!(sink.rows, ROW_COUNT);
        assert_eq!(sink.batches, ROW_COUNT.div_ceil(BATCH_SIZE));
        assert_eq!(sink.max_batch, BATCH_SIZE);
    }
}
