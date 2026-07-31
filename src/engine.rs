//! SQL execution and structured query results.

use std::cmp::Ordering;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::catalog::Catalog;
use crate::error::{Error, Resource, Result};
use crate::execution::{ExecutionContext, ExecutionLimits, ExecutionStats};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

/// A reusable in-memory SQL database.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    limits: ExecutionLimits,
    last_execution_stats: ExecutionStats,
    spill_directory: Arc<PathBuf>,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
            limits: ExecutionLimits::default(),
            last_execution_stats: ExecutionStats::default(),
            spill_directory: Arc::new(std::env::temp_dir()),
        }
    }
}

/// Metadata for one column in a [`QueryResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    /// The output name, including an alias when one was supplied.
    pub name: String,
    /// The logical type of values in this result column.
    pub data_type: DataType,
}

/// Structured rows and column metadata produced by a `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Ordered metadata for the result columns.
    pub columns: Vec<ResultColumn>,
    /// Positional result rows in the same order as [`Self::columns`].
    pub rows: Vec<Vec<Value>>,
}

/// The outcome of one successfully executed SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    /// A data definition or data modification acknowledgement.
    Command {
        /// Stable uppercase command name, such as `CREATE TABLE` or `INSERT`.
        tag: &'static str,
        /// Number of rows affected by the command.
        affected_rows: usize,
    },
    /// Rows and metadata returned by a `SELECT` statement.
    Query(QueryResult),
}

impl Database {
    /// Creates an empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty database governed by `limits`.
    #[must_use]
    pub fn with_limits(limits: ExecutionLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Creates an empty database with configurable limits and spill location.
    #[must_use]
    pub fn with_limits_and_spill_directory(
        limits: ExecutionLimits,
        spill_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            catalog: Catalog::default(),
            limits,
            last_execution_stats: ExecutionStats::default(),
            spill_directory: Arc::new(spill_directory.into()),
        }
    }

    /// Returns the database's catalog for read-only schema and table inspection.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Returns the resource ceilings used for subsequent batches.
    #[must_use]
    pub fn limits(&self) -> &ExecutionLimits {
        &self.limits
    }

    /// Replaces the resource ceilings used for subsequent batches.
    pub fn set_limits(&mut self, limits: ExecutionLimits) {
        self.limits = limits;
    }

    /// Returns counters from the most recent execution attempt.
    #[must_use]
    pub fn last_execution_stats(&self) -> &ExecutionStats {
        &self.last_execution_stats
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        let limits = self.limits.clone();
        let mut context = ExecutionContext::new(&limits, sql.len());
        let outcome = (|| {
            context.check(Resource::InputBytes, sql.len())?;
            let statements = sql::parse_bounded(sql, &limits, &mut context.stats)?;
            let mut results = Vec::new();
            for statement in statements {
                reserve_vec_slot(&mut results, &mut context)?;
                let result = self.execute_statement(statement, &mut context)?;
                results.push(result);
            }
            Ok(results)
        })();
        context.stats.stored_values = self.catalog.stored_values();
        self.last_execution_stats = context.stats;
        outcome
    }

    fn execute_statement(
        &mut self,
        statement: Statement,
        context: &mut ExecutionContext<'_>,
    ) -> Result<StatementResult> {
        match statement {
            Statement::CreateTable { name, columns } => {
                context.stats.schema_width = context.stats.schema_width.max(columns.len());
                context.check(Resource::SchemaWidth, columns.len())?;
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
                    let additional = rows.len().saturating_mul(target.schema().len());
                    let stored_values = self.catalog.stored_values().saturating_add(additional);
                    context.check(Resource::StoredValues, stored_values)?;
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
                .execute_select(select, context)
                .map(StatementResult::Query),
        }
    }

    fn execute_select(
        &self,
        select: Select,
        context: &mut ExecutionContext<'_>,
    ) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let predicate_memory = select
            .predicate
            .as_ref()
            .map(predicate_heap_memory)
            .unwrap_or(0);
        context.reserve_memory(predicate_memory)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let (group_columns, group_memory) =
            resolve_group_columns(table, &select.group_by, context)?;
        let ResolvedSelect {
            items,
            result_columns,
            aggregate_specs,
            planning_memory: select_memory,
        } = resolve_select_items(table, &select.items, &group_columns, context)?;
        let (ordering, ordering_memory) =
            resolve_ordering(&result_columns, &select.order_by, context)?;
        let planning_memory = group_memory
            .saturating_add(select_memory)
            .saturating_add(ordering_memory);

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                predicate.as_ref(),
                &group_columns,
                &aggregate_specs,
                context,
                &self.spill_directory,
            )?;
            let grouped_memory = grouped.memory_bytes;
            let projected = project_grouped_rows(
                &grouped,
                &items,
                &ordering,
                select.limit,
                context,
                &self.spill_directory,
            );
            drop(grouped);
            context.release_memory(grouped_memory);
            projected?
        } else {
            execute_ungrouped(
                table,
                predicate.as_ref(),
                &items,
                &ordering,
                select.limit,
                context,
                &self.spill_directory,
            )?
        };

        drop(ordering);
        drop(aggregate_specs);
        drop(items);
        drop(group_columns);
        drop(predicate);
        context.release_memory(predicate_memory);
        context.release_memory(planning_memory);

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

struct ResolvedSelect {
    items: Vec<ResolvedItem>,
    result_columns: Vec<ResultColumn>,
    aggregate_specs: Vec<AggregateSpec>,
    planning_memory: usize,
}

fn resolve_group_columns(
    table: &Table,
    names: &[String],
    context: &mut ExecutionContext<'_>,
) -> Result<(Vec<usize>, usize)> {
    let (mut columns, memory_bytes) = tracked_vec_with_capacity_and_memory(names.len(), context)?;
    for name in names {
        let column = table.column_index(name)?;
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "GROUP BY column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok((columns, memory_bytes))
}

fn resolve_select_items(
    table: &Table,
    requested: &[SelectItem],
    group_columns: &[usize],
    context: &mut ExecutionContext<'_>,
) -> Result<ResolvedSelect> {
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

    let output_capacity = requested.iter().fold(0usize, |capacity, item| {
        capacity.saturating_add(if matches!(item, SelectItem::Wildcard) {
            table.schema().len()
        } else {
            1
        })
    });
    let aggregate_capacity = requested
        .iter()
        .filter(|item| matches!(item, SelectItem::Aggregate { .. }))
        .count();
    let (mut items, item_memory) = tracked_vec_with_capacity_and_memory(output_capacity, context)?;
    let mut result_columns = tracked_vec_with_capacity(output_capacity, context)?;
    let (mut aggregate_specs, aggregate_memory) =
        tracked_vec_with_capacity_and_memory(aggregate_capacity, context)?;

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
                        name: clone_string_tracked(&field.name, context)?,
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
                let output_name = match alias {
                    Some(alias) => clone_string_tracked(alias, context)?,
                    None => clone_string_tracked(&table.schema()[source].name, context)?,
                };
                result_columns.push(ResultColumn {
                    name: output_name,
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
                        (None, None, "*")
                    }
                    AggregateArgument::Column(name) => {
                        let index = table.column_index(name)?;
                        (
                            Some(index),
                            Some(table.schema()[index].data_type),
                            table.schema()[index].name.as_str(),
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
                let output_name = match alias {
                    Some(alias) => clone_string_tracked(alias, context)?,
                    None => aggregate_name_tracked(function.name(), argument_name, context)?,
                };
                result_columns.push(ResultColumn {
                    name: output_name,
                    data_type: aggregate_output_type(*function, input_type),
                });
            }
        }
    }

    Ok(ResolvedSelect {
        items,
        result_columns,
        aggregate_specs,
        planning_memory: item_memory.saturating_add(aggregate_memory),
    })
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

fn reserve_vec_slot<T>(values: &mut Vec<T>, context: &mut ExecutionContext<'_>) -> Result<usize> {
    if values.len() < values.capacity() || size_of::<T>() == 0 {
        return Ok(0);
    }
    let old_capacity = values.capacity();
    let target_capacity = old_capacity.saturating_mul(2).max(1);
    let reserved = target_capacity
        .saturating_sub(old_capacity)
        .saturating_mul(size_of::<T>());
    context.reserve_memory(reserved)?;
    values.reserve_exact(target_capacity.saturating_sub(values.len()));
    let actual = values
        .capacity()
        .saturating_sub(old_capacity)
        .saturating_mul(size_of::<T>());
    context.adjust_memory_reservation(reserved, actual)?;
    Ok(actual)
}

fn tracked_vec_with_capacity<T>(
    capacity: usize,
    context: &mut ExecutionContext<'_>,
) -> Result<Vec<T>> {
    tracked_vec_with_capacity_and_memory(capacity, context).map(|(values, _)| values)
}

fn tracked_vec_with_capacity_and_memory<T>(
    capacity: usize,
    context: &mut ExecutionContext<'_>,
) -> Result<(Vec<T>, usize)> {
    let reserved = capacity.saturating_mul(size_of::<T>());
    context.reserve_memory(reserved)?;
    let values = Vec::with_capacity(capacity);
    let actual = values.capacity().saturating_mul(size_of::<T>());
    context.adjust_memory_reservation(reserved, actual)?;
    Ok((values, actual))
}

fn clone_string_tracked(value: &str, context: &mut ExecutionContext<'_>) -> Result<String> {
    context.reserve_memory(value.len())?;
    let mut cloned = String::with_capacity(value.len());
    context.adjust_memory_reservation(value.len(), cloned.capacity())?;
    cloned.push_str(value);
    Ok(cloned)
}

fn aggregate_name_tracked(
    function: &str,
    argument: &str,
    context: &mut ExecutionContext<'_>,
) -> Result<String> {
    let reserved = function
        .len()
        .saturating_add(argument.len())
        .saturating_add(2);
    context.reserve_memory(reserved)?;
    let mut name = String::with_capacity(reserved);
    context.adjust_memory_reservation(reserved, name.capacity())?;
    name.push_str(function);
    name.push('(');
    name.push_str(argument);
    name.push(')');
    Ok(name)
}

fn clone_value_ref_tracked(
    value: ValueRef<'_>,
    context: &mut ExecutionContext<'_>,
) -> Result<Value> {
    Ok(match value {
        ValueRef::Int64(value) => Value::Int64(value),
        ValueRef::Float64(value) => Value::Float64(value),
        ValueRef::Bool(value) => Value::Bool(value),
        ValueRef::String(value) => Value::String(clone_string_tracked(value, context)?),
    })
}

fn clone_value_tracked(value: &Value, context: &mut ExecutionContext<'_>) -> Result<Value> {
    clone_value_ref_tracked(value.as_ref(), context)
}

fn value_string_memory(values: &[Value]) -> usize {
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => value.capacity(),
            Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => 0,
        })
        .sum()
}

struct TrackedValues {
    values: Vec<Value>,
    memory_bytes: usize,
}

#[derive(Default)]
struct TrackedAggregateStates {
    values: Vec<AggregateState>,
    memory_bytes: usize,
}

fn clone_group_key(
    table: &Table,
    columns: &[usize],
    row: usize,
    context: &mut ExecutionContext<'_>,
) -> Result<TrackedValues> {
    let (mut key, vector_memory) = tracked_vec_with_capacity_and_memory(columns.len(), context)?;
    for column in columns {
        key.push(clone_value_ref_tracked(
            table.columns()[*column].value_ref(row),
            context,
        )?);
    }
    let memory_bytes = vector_memory.saturating_add(value_string_memory(&key));
    Ok(TrackedValues {
        values: key,
        memory_bytes,
    })
}

fn new_aggregate_states(
    specs: &[AggregateSpec],
    context: &mut ExecutionContext<'_>,
) -> Result<TrackedAggregateStates> {
    let (mut states, memory_bytes) = tracked_vec_with_capacity_and_memory(specs.len(), context)?;
    states.extend(specs.iter().map(AggregateState::new));
    Ok(TrackedAggregateStates {
        values: states,
        memory_bytes,
    })
}

fn project_source_row(
    table: &Table,
    row: usize,
    items: &[ResolvedItem],
    context: &mut ExecutionContext<'_>,
) -> Result<Vec<Value>> {
    let mut values = tracked_vec_with_capacity(items.len(), context)?;
    for item in items {
        let ResolvedItem::Column { source, .. } = item else {
            unreachable!("projection does not contain aggregates")
        };
        values.push(clone_value_ref_tracked(
            table.columns()[*source].value_ref(row),
            context,
        )?);
    }
    Ok(values)
}

fn execute_ungrouped(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    context: &mut ExecutionContext<'_>,
    spill_directory: &Arc<PathBuf>,
) -> Result<Vec<Vec<Value>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    if ordering.is_empty() {
        for row in 0..table.row_count() {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                context.add_intermediate_rows(1)?;
                context.add_result_row()?;
                reserve_vec_slot(&mut rows, context)?;
                let projected = project_source_row(table, row, items, context)?;
                rows.push(projected);
                if limit.is_some_and(|limit| rows.len() == limit) {
                    break;
                }
            }
        }
        return Ok(rows);
    }

    let compare =
        |left: usize, right: usize| compare_source_rows(table, items, ordering, left, right);
    let mut sorter = IndexSorter::new(compare, context, spill_directory);
    for row in 0..table.row_count() {
        if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
            context.add_intermediate_rows(1)?;
            sorter.add(row, context)?;
        }
    }
    sorter.prepare(context)?;
    let working_memory = sorter.working_memory_bytes();
    context.reserve_memory(working_memory)?;
    let chunk_memory = sorter.chunk_memory_bytes();
    let run_memory = sorter.run_memory_bytes();
    let outcome = sorter.drain(|row| {
        if limit.is_some_and(|limit| rows.len() == limit) {
            return Ok(false);
        }
        context.add_result_row()?;
        reserve_vec_slot(&mut rows, context)?;
        let projected = project_source_row(table, row, items, context)?;
        rows.push(projected);
        Ok(true)
    });
    context.release_memory(
        working_memory
            .saturating_add(chunk_memory)
            .saturating_add(run_memory),
    );
    outcome?;
    Ok(rows)
}

fn execute_grouped(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    context: &mut ExecutionContext<'_>,
    spill_directory: &Arc<PathBuf>,
) -> Result<GroupedData> {
    let mut data = GroupedData::default();
    if group_columns.is_empty() {
        let mut states = new_aggregate_states(aggregate_specs, context)?;
        for row in 0..table.row_count() {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                context.add_intermediate_rows(1)?;
                update_aggregates(&mut states.values, aggregate_specs, table, row)?;
            }
        }
        push_group(
            &mut data,
            TrackedValues {
                values: Vec::new(),
                memory_bytes: 0,
            },
            states,
            aggregate_specs,
            table,
            context,
        )?;
        return Ok(data);
    }

    let compare = |left: usize, right: usize| {
        compare_group_keys(table, group_columns, left, right).then_with(|| left.cmp(&right))
    };
    let mut sorter = IndexSorter::new(compare, context, spill_directory);
    for row in 0..table.row_count() {
        if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
            context.add_intermediate_rows(1)?;
            sorter.add(row, context)?;
        }
    }
    sorter.prepare(context)?;
    let sorter_memory = sorter.working_memory_bytes();
    context.reserve_memory(sorter_memory)?;
    let chunk_memory = sorter.chunk_memory_bytes();
    let run_memory = sorter.run_memory_bytes();

    let mut current_key_row: Option<usize> = None;
    let mut states = new_aggregate_states(aggregate_specs, context)?;
    let outcome = sorter.drain(|row| {
        let belongs_to_current = current_key_row.is_some_and(|key_row| {
            compare_group_keys(table, group_columns, key_row, row) == Ordering::Equal
        });
        if current_key_row.is_some() && !belongs_to_current {
            let key_row = current_key_row.take().expect("current group has a key");
            let key = clone_group_key(table, group_columns, key_row, context)?;
            let finished = std::mem::take(&mut states);
            push_group(&mut data, key, finished, aggregate_specs, table, context)?;
            states = new_aggregate_states(aggregate_specs, context)?;
        }
        if current_key_row.is_none() {
            current_key_row = Some(row);
        }
        update_aggregates(&mut states.values, aggregate_specs, table, row)?;
        Ok(true)
    });
    context.release_memory(
        sorter_memory
            .saturating_add(chunk_memory)
            .saturating_add(run_memory),
    );
    outcome?;
    if let Some(key_row) = current_key_row {
        let key = clone_group_key(table, group_columns, key_row, context)?;
        push_group(&mut data, key, states, aggregate_specs, table, context)?;
    } else {
        let memory_bytes = states.memory_bytes;
        drop(states);
        context.release_memory(memory_bytes);
    }
    Ok(data)
}

fn update_aggregates(
    states: &mut [AggregateState],
    specs: &[AggregateSpec],
    table: &Table,
    row: usize,
) -> Result<()> {
    for (state, spec) in states.iter_mut().zip(specs) {
        state.update(spec, table, row)?;
    }
    Ok(())
}

fn push_group(
    data: &mut GroupedData,
    key: TrackedValues,
    states: TrackedAggregateStates,
    specs: &[AggregateSpec],
    table: &Table,
    context: &mut ExecutionContext<'_>,
) -> Result<()> {
    context.add_intermediate_rows(1)?;
    let (mut aggregates, aggregate_vector_memory) =
        tracked_vec_with_capacity_and_memory(specs.len(), context)?;
    for (state, spec) in states.values.into_iter().zip(specs) {
        aggregates.push(state.finish(spec, table, context)?);
    }
    context.release_memory(states.memory_bytes);
    let aggregate_memory = aggregate_vector_memory.saturating_add(value_string_memory(&aggregates));
    let group_vector_memory = reserve_vec_slot(&mut data.groups, context)?;
    data.memory_bytes = data
        .memory_bytes
        .saturating_add(key.memory_bytes)
        .saturating_add(aggregate_memory)
        .saturating_add(group_vector_memory);
    data.groups.push(GroupRow {
        key: key.values,
        aggregates,
    });
    Ok(())
}

#[derive(Debug, Default)]
struct GroupedData {
    groups: Vec<GroupRow>,
    memory_bytes: usize,
}

#[derive(Debug)]
struct GroupRow {
    key: Vec<Value>,
    aggregates: Vec<Value>,
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt(i64),
    SumFloat(f64),
    Min(Option<usize>),
    Max(Option<usize>),
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
                if current.is_none_or(|existing| candidate < column.value_ref(existing)) {
                    *current = Some(row);
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if current.is_none_or(|existing| candidate > column.value_ref(existing)) {
                    *current = Some(row);
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

    fn finish(
        self,
        spec: &AggregateSpec,
        table: &Table,
        context: &mut ExecutionContext<'_>,
    ) -> Result<Value> {
        match self {
            Self::Count(value) | Self::SumInt(value) => Ok(Value::Int64(value)),
            Self::SumFloat(value) => Ok(Value::Float64(value)),
            Self::Min(Some(row)) | Self::Max(Some(row)) => clone_value_ref_tracked(
                table.columns()[spec.argument.expect("extremum argument")].value_ref(row),
                context,
            ),
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

fn resolve_ordering(
    columns: &[ResultColumn],
    requested: &[OrderBy],
    context: &mut ExecutionContext<'_>,
) -> Result<(Vec<ResolvedOrder>, usize)> {
    let (mut ordering, memory_bytes) =
        tracked_vec_with_capacity_and_memory(requested.len(), context)?;
    for order in requested {
        let mut matched = None;
        for (index, column) in columns.iter().enumerate() {
            if !column.name.eq_ignore_ascii_case(&order.name) {
                continue;
            }
            if matched.is_some() {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY name '{}' is ambiguous",
                    order.name
                )));
            }
            matched = Some(index);
        }
        match matched {
            Some(index) => ordering.push(ResolvedOrder {
                output: index,
                descending: order.descending,
            }),
            None => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY column or alias '{}' is not in the SELECT output",
                    order.name
                )));
            }
        }
    }
    Ok((ordering, memory_bytes))
}

fn compare_source_rows(
    table: &Table,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    left: usize,
    right: usize,
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

fn compare_group_keys(table: &Table, columns: &[usize], left: usize, right: usize) -> Ordering {
    for column in columns {
        let comparison = table.columns()[*column].cmp_at(left, right);
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    Ordering::Equal
}

fn compare_group_rows(
    data: &GroupedData,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    left: usize,
    right: usize,
) -> Ordering {
    for order in ordering {
        let comparison = match items[order.output] {
            ResolvedItem::Column {
                group_position: Some(position),
                ..
            } => data.groups[left].key[position].cmp(&data.groups[right].key[position]),
            ResolvedItem::Column {
                group_position: None,
                ..
            } => unreachable!("grouped columns are validated"),
            ResolvedItem::Aggregate { state } => {
                data.groups[left].aggregates[state].cmp(&data.groups[right].aggregates[state])
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
    data.groups[left]
        .key
        .cmp(&data.groups[right].key)
        .then_with(|| left.cmp(&right))
}

fn project_grouped_rows(
    data: &GroupedData,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    context: &mut ExecutionContext<'_>,
    spill_directory: &Arc<PathBuf>,
) -> Result<Vec<Vec<Value>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }
    let compare = |left, right| compare_group_rows(data, items, ordering, left, right);
    let mut sorter = IndexSorter::new(compare, context, spill_directory);
    for group in 0..data.groups.len() {
        sorter.add(group, context)?;
    }
    sorter.prepare(context)?;
    let working_memory = sorter.working_memory_bytes();
    context.reserve_memory(working_memory)?;
    let chunk_memory = sorter.chunk_memory_bytes();
    let run_memory = sorter.run_memory_bytes();
    let mut rows = Vec::new();
    let outcome = sorter.drain(|group| {
        if limit.is_some_and(|limit| rows.len() == limit) {
            return Ok(false);
        }
        context.add_result_row()?;
        reserve_vec_slot(&mut rows, context)?;
        let mut values = tracked_vec_with_capacity(items.len(), context)?;
        for item in items {
            let value = match item {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => &data.groups[group].key[*position],
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Aggregate { state } => &data.groups[group].aggregates[*state],
            };
            values.push(clone_value_tracked(value, context)?);
        }
        rows.push(values);
        Ok(true)
    });
    context.release_memory(
        working_memory
            .saturating_add(chunk_memory)
            .saturating_add(run_memory),
    );
    outcome?;
    Ok(rows)
}

struct IndexSorter<F> {
    compare: F,
    chunk_capacity: usize,
    chunk_memory_bytes: usize,
    run_memory_bytes: usize,
    chunk: Vec<usize>,
    runs: Vec<TempRun>,
    spill_directory: Arc<PathBuf>,
    prepared: bool,
}

impl<F> IndexSorter<F>
where
    F: Fn(usize, usize) -> Ordering,
{
    fn new(compare: F, context: &ExecutionContext<'_>, spill_directory: &Arc<PathBuf>) -> Self {
        let available_memory = context.available_memory();
        Self {
            compare,
            chunk_capacity: available_memory / (2 * size_of::<usize>()),
            chunk_memory_bytes: 0,
            run_memory_bytes: 0,
            chunk: Vec::new(),
            runs: Vec::new(),
            spill_directory: Arc::clone(spill_directory),
            prepared: false,
        }
    }

    fn add(&mut self, index: usize, context: &mut ExecutionContext<'_>) -> Result<()> {
        if self.chunk.len() == self.chunk.capacity() {
            if !self.chunk.is_empty() {
                self.flush_chunk(context)?;
            }
            self.allocate_chunk(context)?;
        }
        self.chunk.push(index);
        Ok(())
    }

    fn prepare(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        if !self.runs.is_empty() {
            self.flush_chunk(context)?;
        } else {
            self.chunk
                .sort_unstable_by(|left, right| (self.compare)(*left, *right));
        }
        self.prepared = true;
        Ok(())
    }

    fn working_memory_bytes(&self) -> usize {
        if self.runs.is_empty() {
            0
        } else {
            self.runs.len().saturating_mul(32)
        }
    }

    fn chunk_memory_bytes(&self) -> usize {
        self.chunk_memory_bytes
    }

    fn run_memory_bytes(&self) -> usize {
        self.run_memory_bytes
    }

    fn drain(mut self, mut visit: impl FnMut(usize) -> Result<bool>) -> Result<()> {
        debug_assert!(self.prepared);
        if self.runs.is_empty() {
            for index in self.chunk.drain(..) {
                if !visit(index)? {
                    break;
                }
            }
            return Ok(());
        }

        let mut readers = open_runs(&self.runs)?;
        let mut heads = readers
            .iter_mut()
            .map(read_index)
            .collect::<Result<Vec<_>>>()?;
        while let Some(run) = smallest_head(&heads, &self.compare) {
            let index = heads[run].expect("selected run has a head");
            if !visit(index)? {
                break;
            }
            heads[run] = read_index(&mut readers[run])?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let mut indices = std::mem::take(&mut self.chunk);
        let memory_bytes = std::mem::take(&mut self.chunk_memory_bytes);
        let outcome = (|| {
            indices.sort_unstable_by(|left, right| (self.compare)(*left, *right));
            let (run, mut file) = TempRun::create(&self.spill_directory, indices.len())?;
            for index in &indices {
                write_index(&mut file, *index)?;
            }
            file.sync_data()
                .map_err(|error| spill_error("syncing run", error))?;
            context.record_spill(indices.len().saturating_mul(size_of::<u64>()));
            Ok(run)
        })();
        drop(indices);
        context.release_memory(memory_bytes);
        let run = outcome?;
        self.ensure_run_storage(context)?;
        self.runs.push(run);
        context.observe_live_spill_runs(self.runs.len());
        self.compact_runs(context)
    }

    fn allocate_chunk(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        let capacity = self.chunk_capacity.max(1);
        let reserved = capacity.saturating_mul(size_of::<usize>());
        context.reserve_memory(reserved)?;
        let chunk = Vec::with_capacity(capacity);
        let actual = chunk.capacity().saturating_mul(size_of::<usize>());
        if let Err(error) = context.adjust_memory_reservation(reserved, actual) {
            context.release_memory(reserved);
            return Err(error);
        }
        self.chunk = chunk;
        self.chunk_memory_bytes = actual;
        Ok(())
    }

    fn ensure_run_storage(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        const MAX_LIVE_RUNS: usize = 2;
        if self.runs.capacity() > MAX_LIVE_RUNS {
            return Ok(());
        }
        let reserved = (MAX_LIVE_RUNS + 1).saturating_mul(size_of::<TempRun>());
        context.reserve_memory(reserved)?;
        let runs = Vec::with_capacity(MAX_LIVE_RUNS + 1);
        let actual = runs.capacity().saturating_mul(size_of::<TempRun>());
        if let Err(error) = context.adjust_memory_reservation(reserved, actual) {
            context.release_memory(reserved);
            return Err(error);
        }
        self.runs = runs;
        self.run_memory_bytes = actual;
        Ok(())
    }

    fn compact_runs(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        while self.runs.len() > 2 {
            let mut smallest = [0usize, 1usize];
            if self.runs[smallest[1]].rows < self.runs[smallest[0]].rows {
                smallest.swap(0, 1);
            }
            for index in 2..self.runs.len() {
                if self.runs[index].rows < self.runs[smallest[0]].rows {
                    smallest[1] = smallest[0];
                    smallest[0] = index;
                } else if self.runs[index].rows < self.runs[smallest[1]].rows {
                    smallest[1] = index;
                }
            }
            smallest.sort_unstable();
            let right = self.runs.remove(smallest[1]);
            let left = self.runs.remove(smallest[0]);
            let merged = self.merge_runs(&[left, right], context)?;
            self.runs.push(merged);
        }
        Ok(())
    }

    fn merge_runs(&self, runs: &[TempRun], context: &mut ExecutionContext<'_>) -> Result<TempRun> {
        let memory_bytes = runs.len().saturating_mul(32);
        context.reserve_memory(memory_bytes)?;
        let outcome = (|| {
            let mut readers = open_runs(runs)?;
            let mut heads = readers
                .iter_mut()
                .map(read_index)
                .collect::<Result<Vec<_>>>()?;
            let row_count = runs
                .iter()
                .map(|run| run.rows)
                .fold(0usize, usize::saturating_add);
            let (output, mut file) = TempRun::create(&self.spill_directory, row_count)?;
            context.observe_live_spill_runs(
                self.runs.len().saturating_add(runs.len()).saturating_add(1),
            );
            let mut written = 0usize;
            while let Some(run) = smallest_head(&heads, &self.compare) {
                let index = heads[run].expect("selected run has a head");
                write_index(&mut file, index)?;
                written += 1;
                heads[run] = read_index(&mut readers[run])?;
            }
            file.sync_data()
                .map_err(|error| spill_error("syncing merged run", error))?;
            context.record_spill(written.saturating_mul(size_of::<u64>()));
            Ok(output)
        })();
        context.release_memory(memory_bytes);
        outcome
    }
}

struct TempRun {
    directory: Arc<PathBuf>,
    id: u128,
    rows: usize,
}

impl TempRun {
    fn create(directory: &Arc<PathBuf>, rows: usize) -> Result<(Self, File)> {
        for _ in 0..100 {
            let id = next_spill_id()?;
            let path = spill_path(directory, id);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            directory: Arc::clone(directory),
                            id,
                            rows,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(spill_error("creating run", error)),
            }
        }
        Err(Error::SpillIo(
            "could not allocate a unique temporary run name".to_owned(),
        ))
    }
}

impl Drop for TempRun {
    fn drop(&mut self) {
        let _ = remove_file(spill_path(&self.directory, self.id));
    }
}

fn open_runs(runs: &[TempRun]) -> Result<Vec<File>> {
    runs.iter()
        .map(|run| {
            File::open(spill_path(&run.directory, run.id))
                .map_err(|error| spill_error("opening run", error))
        })
        .collect()
}

fn next_spill_id() -> Result<u128> {
    let mut bytes = [0; size_of::<u128>()];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::SpillIo(format!("generating a secure run name: {error}")))?;
    Ok(u128::from_le_bytes(bytes))
}

fn spill_path(directory: &Path, id: u128) -> PathBuf {
    directory.join(format!(".rusthouse-spill-{id:032x}"))
}

fn write_index(file: &mut File, index: usize) -> Result<()> {
    let index = u64::try_from(index)
        .map_err(|_| Error::SpillIo("row index does not fit in a spill record".to_owned()))?;
    file.write_all(&index.to_le_bytes())
        .map_err(|error| spill_error("writing run", error))
}

fn read_index(file: &mut File) -> Result<Option<usize>> {
    let mut bytes = [0u8; size_of::<u64>()];
    match file.read(&mut bytes[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(spill_error("reading run", error)),
    }
    file.read_exact(&mut bytes[1..])
        .map_err(|error| spill_error("reading run", error))?;
    let index = usize::try_from(u64::from_le_bytes(bytes))
        .map_err(|_| Error::SpillIo("spill row index exceeds this platform".to_owned()))?;
    Ok(Some(index))
}

fn smallest_head<F>(heads: &[Option<usize>], compare: &F) -> Option<usize>
where
    F: Fn(usize, usize) -> Ordering,
{
    heads
        .iter()
        .enumerate()
        .filter_map(|(run, head)| head.map(|head| (run, head)))
        .min_by(|(left_run, left), (right_run, right)| {
            compare(*left, *right).then_with(|| left_run.cmp(right_run))
        })
        .map(|(run, _)| run)
}

fn spill_error(operation: &str, error: std::io::Error) -> Error {
    Error::SpillIo(format!("{operation}: {error}"))
}

#[derive(Debug)]
enum CompiledPredicate<'a> {
    Comparison {
        left: CompiledOperand<'a>,
        operator: ComparisonOperator,
        right: CompiledOperand<'a>,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate<'_> {
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
enum CompiledOperand<'a> {
    Column { index: usize, data_type: DataType },
    Literal(&'a Value),
}

impl CompiledOperand<'_> {
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

fn predicate_heap_memory(predicate: &Predicate) -> usize {
    fn node_count(predicate: &Predicate) -> usize {
        match predicate {
            Predicate::Comparison { .. } => 1,
            Predicate::And(left, right) | Predicate::Or(left, right) => 1usize
                .saturating_add(node_count(left))
                .saturating_add(node_count(right)),
        }
    }

    node_count(predicate)
        .saturating_sub(1)
        .saturating_mul(size_of::<CompiledPredicate<'_>>())
}

fn compile_predicate<'a>(table: &Table, predicate: &'a Predicate) -> Result<CompiledPredicate<'a>> {
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

fn compile_operand<'a>(table: &Table, operand: &'a Operand) -> Result<CompiledOperand<'a>> {
    match operand {
        Operand::Column(name) => {
            let index = table.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: table.schema()[index].data_type,
            })
        }
        Operand::Literal(value) => Ok(CompiledOperand::Literal(value)),
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
    fn sorter_shares_the_database_spill_directory_allocation() {
        let database = Database::with_limits_and_spill_directory(
            ExecutionLimits {
                max_memory_bytes: 4_096,
                ..ExecutionLimits::default()
            },
            PathBuf::from("x".repeat(1024 * 1024)),
        );
        let context = ExecutionContext::new(&database.limits, 0);
        let sorter = IndexSorter::new(
            |left: usize, right: usize| left.cmp(&right),
            &context,
            &database.spill_directory,
        );

        assert!(std::ptr::eq(
            database.spill_directory.as_ref().as_path(),
            sorter.spill_directory.as_ref().as_path(),
        ));
        assert_eq!(context.stats.peak_memory_bytes, 0);
    }

    #[test]
    fn predictable_spill_entries_cannot_exhaust_name_retries() {
        let directory = std::env::temp_dir().join(format!(
            "rusthouse-spill-name-test-{}-{}",
            std::process::id(),
            next_spill_id().expect("secure test directory suffix")
        ));
        std::fs::create_dir(&directory).expect("create spill test directory");
        let predictable = (0..100)
            .map(|id| directory.join(format!(".rusthouse-spill-{}-{id}", std::process::id())))
            .collect::<Vec<_>>();
        for path in &predictable {
            File::create(path).expect("pre-create old predictable spill entry");
        }

        let directory = Arc::new(directory);
        let (run, file) = TempRun::create(&directory, 0)
            .expect("unpredictable spill name ignores pre-created entries");
        drop(file);
        drop(run);
        for path in predictable {
            remove_file(path).expect("remove predictable entry");
        }
        std::fs::remove_dir(directory.as_ref()).expect("remove spill test directory");
    }
}
