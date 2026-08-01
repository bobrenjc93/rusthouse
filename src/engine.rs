//! SQL execution and structured query results.

use std::cmp::Ordering;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::catalog::Catalog;
use crate::error::{Error, Resource, Result};
use crate::execution::{ExecutionContext, ExecutionLimits, ExecutionStats};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, ValueRef};

const SCAN_MORSEL_ROWS: usize = 4_096;

/// A reusable in-memory SQL database.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    limits: ExecutionLimits,
    last_execution_stats: ExecutionStats,
    spill_directory: Arc<PathBuf>,
    worker_count: usize,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
            limits: ExecutionLimits::default(),
            last_execution_stats: ExecutionStats::default(),
            spill_directory: Arc::new(std::env::temp_dir()),
            worker_count: thread::available_parallelism().map_or(1, usize::from),
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
            worker_count: thread::available_parallelism().map_or(1, usize::from),
        }
    }

    /// Creates an empty database that uses at most `worker_count` scan workers.
    ///
    /// A worker count of zero is rejected. The engine may use fewer workers
    /// when a query contains fewer fixed-size scan morsels than workers.
    pub fn with_worker_count(worker_count: usize) -> Result<Self> {
        validate_worker_count(worker_count)?;
        Ok(Self {
            worker_count,
            ..Self::default()
        })
    }

    /// Returns the maximum number of scan workers used by a query.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Changes the maximum number of scan workers used by subsequent queries.
    ///
    /// A worker count of zero is rejected without changing the current value.
    pub fn set_worker_count(&mut self, worker_count: usize) -> Result<()> {
        validate_worker_count(worker_count)?;
        self.worker_count = worker_count;
        Ok(())
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
        let execution_settings = ScanExecutionSettings {
            worker_count: self.worker_count,
            spill_directory: &self.spill_directory,
        };

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                predicate.as_ref(),
                &group_columns,
                &aggregate_specs,
                &execution_settings,
                context,
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
                &execution_settings,
                context,
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

struct ScanExecutionSettings<'a> {
    worker_count: usize,
    spill_directory: &'a Arc<PathBuf>,
}

fn validate_worker_count(worker_count: usize) -> Result<()> {
    if worker_count == 0 {
        return Err(Error::InvalidConfiguration(
            "worker count must be at least 1".to_owned(),
        ));
    }
    Ok(())
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

#[derive(Clone, Copy)]
struct MatchMask {
    words: [u64; SCAN_MORSEL_ROWS / u64::BITS as usize],
}

impl MatchMask {
    fn new() -> Self {
        Self {
            words: [0; SCAN_MORSEL_ROWS / u64::BITS as usize],
        }
    }

    fn insert(&mut self, offset: usize) {
        self.words[offset / u64::BITS as usize] |= 1_u64 << (offset % u64::BITS as usize);
    }

    fn contains(&self, offset: usize) -> bool {
        self.words[offset / u64::BITS as usize] & (1_u64 << (offset % u64::BITS as usize)) != 0
    }
}

fn execute_morsels<T>(
    row_count: usize,
    worker_count: usize,
    execute: impl Fn(Range<usize>) -> T + Sync,
) -> Vec<T>
where
    T: Send,
{
    let morsel_count = row_count.div_ceil(SCAN_MORSEL_ROWS);
    let active_workers = worker_count.min(morsel_count);
    if active_workers <= 1 {
        return (0..morsel_count)
            .map(|morsel| execute(morsel_range(morsel, row_count)))
            .collect();
    }

    let next_morsel = AtomicUsize::new(0);
    let completed = Mutex::new(
        std::iter::repeat_with(|| None)
            .take(morsel_count)
            .collect::<Vec<_>>(),
    );
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(active_workers);
        for _ in 0..active_workers {
            handles.push(scope.spawn(|| {
                loop {
                    let morsel = next_morsel.fetch_add(1, AtomicOrdering::Relaxed);
                    if morsel >= morsel_count {
                        break;
                    }
                    let result = execute(morsel_range(morsel, row_count));
                    completed.lock().expect("morsel result lock poisoned")[morsel] = Some(result);
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        }
    });

    completed
        .into_inner()
        .expect("morsel result lock poisoned")
        .into_iter()
        .map(|result| result.expect("every morsel is completed by one worker"))
        .collect()
}

fn morsel_range(morsel: usize, row_count: usize) -> Range<usize> {
    let start = morsel * SCAN_MORSEL_ROWS;
    start..(start + SCAN_MORSEL_ROWS).min(row_count)
}

fn morsel_execution_memory<T>(row_count: usize, worker_count: usize) -> usize {
    let morsels = row_count.div_ceil(SCAN_MORSEL_ROWS);
    let workers = worker_count.min(morsels);
    morsels
        .saturating_mul(size_of::<Option<T>>())
        .saturating_add(workers.saturating_mul(128))
}

fn scan_matching_rows(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    worker_count: usize,
    context: &mut ExecutionContext<'_>,
    mut visit: impl FnMut(usize, &mut ExecutionContext<'_>) -> Result<bool>,
) -> Result<()> {
    let reservation = morsel_execution_memory::<MatchMask>(table.row_count(), worker_count);
    if worker_count <= 1
        || table.row_count() <= SCAN_MORSEL_ROWS
        || reservation > context.available_memory()
    {
        for row in 0..table.row_count() {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row))
                && !visit(row, context)?
            {
                break;
            }
        }
        return Ok(());
    }

    context.reserve_memory(reservation)?;
    let masks = execute_morsels(table.row_count(), worker_count, |range| {
        let start = range.start;
        let mut mask = MatchMask::new();
        for row in range {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                mask.insert(row - start);
            }
        }
        mask
    });
    let outcome = (|| {
        for (morsel, mask) in masks.iter().enumerate() {
            let range = morsel_range(morsel, table.row_count());
            for row in range {
                if mask.contains(row - morsel * SCAN_MORSEL_ROWS) && !visit(row, context)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    })();
    drop(masks);
    context.release_memory(reservation);
    outcome
}

fn execute_ungrouped(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    settings: &ScanExecutionSettings<'_>,
    context: &mut ExecutionContext<'_>,
) -> Result<Vec<Vec<Value>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    if ordering.is_empty() {
        scan_matching_rows(
            table,
            predicate,
            settings.worker_count,
            context,
            |row, context| {
                context.add_intermediate_rows(1)?;
                context.add_result_row()?;
                reserve_vec_slot(&mut rows, context)?;
                let projected = project_source_row(table, row, items, context)?;
                rows.push(projected);
                Ok(limit.is_none_or(|limit| rows.len() != limit))
            },
        )?;
        return Ok(rows);
    }

    let compare =
        |left: usize, right: usize| compare_source_rows(table, items, ordering, left, right);
    let mut sorter = IndexSorter::new(compare, context, settings.spill_directory);
    scan_matching_rows(
        table,
        predicate,
        settings.worker_count,
        context,
        |row, context| {
            context.add_intermediate_rows(1)?;
            sorter.add(row, context)?;
            Ok(true)
        },
    )?;
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
    settings: &ScanExecutionSettings<'_>,
    context: &mut ExecutionContext<'_>,
) -> Result<GroupedData> {
    let reservation = parallel_aggregation_memory(table, aggregate_specs, settings.worker_count);
    if settings.worker_count > 1
        && table.row_count() > SCAN_MORSEL_ROWS
        && reservation <= context.available_memory()
    {
        context.reserve_memory(reservation)?;
        let outcome = execute_grouped_parallel(
            table,
            predicate,
            group_columns,
            aggregate_specs,
            settings.worker_count,
            context,
        );
        context.release_memory(reservation);
        return outcome;
    }

    execute_grouped_sequential(
        table,
        predicate,
        group_columns,
        aggregate_specs,
        context,
        settings.spill_directory,
    )
}

fn execute_grouped_sequential(
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
                update_aggregates(&mut states, aggregate_specs, table, row, context)?;
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
        update_aggregates(&mut states, aggregate_specs, table, row, context)?;
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

#[derive(Debug)]
struct PartialGroup {
    key_row: usize,
    states: Vec<AggregateState>,
}

#[derive(Debug)]
struct PartialGroupedData {
    groups: Vec<PartialGroup>,
    matching_rows: usize,
}

fn parallel_aggregation_memory(
    table: &Table,
    specs: &[AggregateSpec],
    worker_count: usize,
) -> usize {
    let rows = table.row_count();
    let float_specs = specs
        .iter()
        .filter(|spec| spec.input_type == Some(DataType::Float64))
        .count();
    let row_memory = size_of::<usize>()
        .saturating_add(size_of::<PartialGroup>().saturating_mul(2))
        .saturating_add(size_of::<AggregateState>().saturating_mul(specs.len()))
        .saturating_add(
            size_of::<(i16, i128)>()
                .saturating_mul(float_specs)
                .saturating_mul(4),
        );
    morsel_execution_memory::<Result<PartialGroupedData>>(rows, worker_count)
        .saturating_add(rows.saturating_mul(row_memory))
        .saturating_add(
            rows.div_ceil(SCAN_MORSEL_ROWS)
                .saturating_mul(specs.len())
                .saturating_mul(size_of::<AggregateState>()),
        )
}

fn execute_grouped_parallel(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    worker_count: usize,
    context: &mut ExecutionContext<'_>,
) -> Result<GroupedData> {
    let partials = execute_morsels(table.row_count(), worker_count, |rows| {
        aggregate_morsel(table, rows, predicate, group_columns, aggregate_specs)
    });
    let mut groups = Vec::new();
    for partial in partials {
        let partial = partial?;
        for _ in 0..partial.matching_rows {
            context.add_intermediate_rows(1)?;
        }
        groups.extend(partial.groups);
    }

    if group_columns.is_empty() && groups.is_empty() {
        groups.push(PartialGroup {
            key_row: 0,
            states: aggregate_specs.iter().map(AggregateState::new).collect(),
        });
    }
    groups.sort_unstable_by(|left, right| {
        if group_columns.is_empty() {
            left.key_row.cmp(&right.key_row)
        } else {
            compare_group_keys(table, group_columns, left.key_row, right.key_row)
                .then_with(|| left.key_row.cmp(&right.key_row))
        }
    });

    let mut data = GroupedData::default();
    let mut groups = groups.into_iter();
    let Some(mut current) = groups.next() else {
        return Ok(data);
    };
    for partial in groups {
        let same_group = group_columns.is_empty()
            || compare_group_keys(table, group_columns, current.key_row, partial.key_row)
                == Ordering::Equal;
        if same_group {
            merge_aggregates(&mut current.states, partial.states, aggregate_specs, table)?;
            continue;
        }
        push_partial_group(
            &mut data,
            current,
            group_columns,
            aggregate_specs,
            table,
            context,
        )?;
        current = partial;
    }
    push_partial_group(
        &mut data,
        current,
        group_columns,
        aggregate_specs,
        table,
        context,
    )?;
    Ok(data)
}

fn aggregate_morsel(
    table: &Table,
    rows: Range<usize>,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<PartialGroupedData> {
    let first_row = rows.start;
    if group_columns.is_empty() {
        let mut states = aggregate_specs
            .iter()
            .map(AggregateState::new)
            .collect::<Vec<_>>();
        let mut matching_rows = 0;
        for row in rows {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                matching_rows += 1;
                update_aggregates_untracked(&mut states, aggregate_specs, table, row)?;
            }
        }
        return Ok(PartialGroupedData {
            groups: vec![PartialGroup {
                key_row: first_row,
                states,
            }],
            matching_rows,
        });
    }

    let mut matching = rows
        .filter(|row| predicate.is_none_or(|predicate| predicate.evaluate(table, *row)))
        .collect::<Vec<_>>();
    matching.sort_unstable_by(|left, right| {
        compare_group_keys(table, group_columns, *left, *right).then_with(|| left.cmp(right))
    });
    let matching_rows = matching.len();
    let mut groups: Vec<PartialGroup> = Vec::new();
    for row in matching {
        let new_group = groups.last().is_none_or(|group| {
            compare_group_keys(table, group_columns, group.key_row, row) != Ordering::Equal
        });
        if new_group {
            groups.push(PartialGroup {
                key_row: row,
                states: aggregate_specs.iter().map(AggregateState::new).collect(),
            });
        }
        update_aggregates_untracked(
            &mut groups.last_mut().expect("group was inserted").states,
            aggregate_specs,
            table,
            row,
        )?;
    }
    Ok(PartialGroupedData {
        groups,
        matching_rows,
    })
}

fn push_partial_group(
    data: &mut GroupedData,
    group: PartialGroup,
    group_columns: &[usize],
    specs: &[AggregateSpec],
    table: &Table,
    context: &mut ExecutionContext<'_>,
) -> Result<()> {
    let key = if group_columns.is_empty() {
        TrackedValues {
            values: Vec::new(),
            memory_bytes: 0,
        }
    } else {
        clone_group_key(table, group_columns, group.key_row, context)?
    };
    push_group(
        data,
        key,
        TrackedAggregateStates {
            values: group.states,
            memory_bytes: 0,
        },
        specs,
        table,
        context,
    )
}

fn update_aggregates_untracked(
    states: &mut [AggregateState],
    specs: &[AggregateSpec],
    table: &Table,
    row: usize,
) -> Result<()> {
    for (state, spec) in states.iter_mut().zip(specs) {
        state.update_untracked(spec, table, row)?;
    }
    Ok(())
}

fn merge_aggregates(
    states: &mut [AggregateState],
    partials: Vec<AggregateState>,
    specs: &[AggregateSpec],
    table: &Table,
) -> Result<()> {
    for ((state, partial), spec) in states.iter_mut().zip(partials).zip(specs) {
        state.merge(partial, spec, table)?;
    }
    Ok(())
}

fn update_aggregates(
    states: &mut TrackedAggregateStates,
    specs: &[AggregateSpec],
    table: &Table,
    row: usize,
    context: &mut ExecutionContext<'_>,
) -> Result<()> {
    for (state, spec) in states.values.iter_mut().zip(specs) {
        states.memory_bytes = states
            .memory_bytes
            .saturating_add(state.update_tracked(spec, table, row, context)?);
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

const FLOAT_FRACTION_BITS: usize = 52;
const FLOAT_FRACTION_MASK: u64 = (1_u64 << FLOAT_FRACTION_BITS) - 1;
const EXACT_FLOAT_MIN_POWER: i16 = -1_074;
const EXACT_FLOAT_LIMBS: usize = 34;
type FloatMagnitude = [u64; EXACT_FLOAT_LIMBS];

// Exact exponent bins make fixed-morsel partials mergeable without early overflow.
#[derive(Debug)]
struct ExactFloatSum {
    bins: Vec<(i16, i128)>,
}

impl ExactFloatSum {
    fn new() -> Self {
        Self { bins: Vec::new() }
    }

    fn add_untracked(&mut self, value: f64) -> Result<()> {
        self.add(value, None).map(|_| ())
    }

    fn add_tracked(&mut self, value: f64, context: &mut ExecutionContext<'_>) -> Result<usize> {
        self.add(value, Some(context))
    }

    fn add(&mut self, value: f64, context: Option<&mut ExecutionContext<'_>>) -> Result<usize> {
        debug_assert!(value.is_finite());
        let bits = value.to_bits();
        let raw_exponent = ((bits >> FLOAT_FRACTION_BITS) & 0x7ff) as i16;
        let fraction = bits & FLOAT_FRACTION_MASK;
        let (significand, power) = if raw_exponent == 0 {
            if fraction == 0 {
                return Ok(0);
            }
            (fraction, EXACT_FLOAT_MIN_POWER)
        } else {
            (
                (1_u64 << FLOAT_FRACTION_BITS) | fraction,
                raw_exponent - 1_075,
            )
        };
        let coefficient = i128::from(significand);
        self.add_bin(
            power,
            if bits >> 63 == 0 {
                coefficient
            } else {
                -coefficient
            },
            context,
        )
    }

    fn merge(&mut self, partial: Self) -> Result<()> {
        for (power, coefficient) in partial.bins {
            self.add_bin(power, coefficient, None)?;
        }
        Ok(())
    }

    fn finish(self, divisor: u64) -> Option<f64> {
        debug_assert!(divisor > 0);
        let mut positive = [0; EXACT_FLOAT_LIMBS];
        let mut negative = [0; EXACT_FLOAT_LIMBS];
        for (power, coefficient) in self.bins {
            let shift = usize::try_from(power - EXACT_FLOAT_MIN_POWER).ok()?;
            let magnitude = if coefficient >= 0 {
                &mut positive
            } else {
                &mut negative
            };
            add_shifted(magnitude, coefficient.unsigned_abs(), shift)?;
        }

        let (negative_result, mut magnitude) = match compare_magnitudes(&positive, &negative) {
            Ordering::Greater => (false, subtract_magnitudes(&positive, &negative)),
            Ordering::Less => (true, subtract_magnitudes(&negative, &positive)),
            Ordering::Equal => return Some(0.0),
        };
        let remainder = divide_magnitude(&mut magnitude, divisor);
        magnitude_to_f64(&magnitude, remainder, divisor, negative_result)
    }

    fn add_bin(
        &mut self,
        power: i16,
        coefficient: i128,
        mut context: Option<&mut ExecutionContext<'_>>,
    ) -> Result<usize> {
        match self.bins.binary_search_by_key(&power, |(power, _)| *power) {
            Ok(index) => {
                let combined = self.bins[index].1.checked_add(coefficient).ok_or_else(|| {
                    Error::NumericOverflow("floating-point accumulator".to_owned())
                })?;
                if combined == 0 {
                    self.bins.remove(index);
                } else {
                    self.bins[index].1 = combined;
                }
                Ok(0)
            }
            Err(index) => {
                let old_capacity = self.bins.capacity();
                let reserved = usize::from(old_capacity == self.bins.len())
                    .saturating_mul(size_of::<(i16, i128)>());
                if let Some(context) = context.as_deref_mut() {
                    context.reserve_memory(reserved)?;
                }
                self.bins.reserve_exact(1);
                let actual = self
                    .bins
                    .capacity()
                    .saturating_sub(old_capacity)
                    .saturating_mul(size_of::<(i16, i128)>());
                if let Some(context) = context {
                    context.adjust_memory_reservation(reserved, actual)?;
                }
                self.bins.insert(index, (power, coefficient));
                Ok(actual)
            }
        }
    }
}

fn add_shifted(target: &mut FloatMagnitude, value: u128, shift: usize) -> Option<()> {
    let first_limb = shift / u64::BITS as usize;
    let offset = shift % u64::BITS as usize;
    for (word_index, word) in [value as u64, (value >> u64::BITS) as u64]
        .into_iter()
        .enumerate()
    {
        if word == 0 {
            continue;
        }
        add_word(target, first_limb + word_index, word << offset)?;
        if offset > 0 {
            add_word(
                target,
                first_limb + word_index + 1,
                word >> (u64::BITS as usize - offset),
            )?;
        }
    }
    Some(())
}

fn add_word(target: &mut FloatMagnitude, mut index: usize, mut word: u64) -> Option<()> {
    while word != 0 {
        let limb = target.get_mut(index)?;
        let (sum, carry) = limb.overflowing_add(word);
        *limb = sum;
        word = u64::from(carry);
        index += 1;
    }
    Some(())
}

fn compare_magnitudes(left: &FloatMagnitude, right: &FloatMagnitude) -> Ordering {
    for (left, right) in left.iter().rev().zip(right.iter().rev()) {
        let ordering = left.cmp(right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn subtract_magnitudes(larger: &FloatMagnitude, smaller: &FloatMagnitude) -> FloatMagnitude {
    let mut result = [0; EXACT_FLOAT_LIMBS];
    let mut borrow = false;
    for ((result, larger), smaller) in result.iter_mut().zip(larger).zip(smaller) {
        let (difference, first_borrow) = larger.overflowing_sub(*smaller);
        let (difference, second_borrow) = difference.overflowing_sub(u64::from(borrow));
        *result = difference;
        borrow = first_borrow || second_borrow;
    }
    debug_assert!(!borrow);
    result
}

fn divide_magnitude(magnitude: &mut FloatMagnitude, divisor: u64) -> u64 {
    let mut remainder = 0_u64;
    for limb in magnitude.iter_mut().rev() {
        let dividend = (u128::from(remainder) << u64::BITS) | u128::from(*limb);
        *limb = (dividend / u128::from(divisor)) as u64;
        remainder = (dividend % u128::from(divisor)) as u64;
    }
    remainder
}

fn magnitude_to_f64(
    magnitude: &FloatMagnitude,
    remainder: u64,
    divisor: u64,
    negative: bool,
) -> Option<f64> {
    let sign = u64::from(negative) << 63;
    let Some(mut highest_bit) = highest_bit(magnitude) else {
        let rounded = u64::from(round_fraction(remainder, divisor, false));
        return Some(f64::from_bits(sign | rounded));
    };

    if highest_bit < FLOAT_FRACTION_BITS {
        let mut rounded = magnitude[0];
        if round_fraction(remainder, divisor, rounded & 1 != 0) {
            rounded += 1;
        }
        return Some(f64::from_bits(sign | rounded));
    }

    let discarded_bits = highest_bit - FLOAT_FRACTION_BITS;
    let mut significand =
        shifted_low_u64(magnitude, discarded_bits) & ((1_u64 << (FLOAT_FRACTION_BITS + 1)) - 1);
    let round_up = if discarded_bits == 0 {
        round_fraction(remainder, divisor, significand & 1 != 0)
    } else {
        let round_bit = bit_is_set(magnitude, discarded_bits - 1);
        let sticky = any_bits_below(magnitude, discarded_bits - 1) || remainder != 0;
        round_bit && (sticky || significand & 1 != 0)
    };
    if round_up {
        significand += 1;
        if significand == 1_u64 << (FLOAT_FRACTION_BITS + 1) {
            significand >>= 1;
            highest_bit += 1;
        }
    }

    let exponent = i32::try_from(highest_bit).ok()? + i32::from(EXACT_FLOAT_MIN_POWER);
    if exponent > 1_023 {
        return None;
    }
    debug_assert!(exponent >= -1_022);
    let raw_exponent = u64::try_from(exponent + 1_023).ok()?;
    Some(f64::from_bits(
        sign | (raw_exponent << FLOAT_FRACTION_BITS) | (significand & FLOAT_FRACTION_MASK),
    ))
}

fn highest_bit(magnitude: &FloatMagnitude) -> Option<usize> {
    magnitude
        .iter()
        .enumerate()
        .rev()
        .find(|(_, limb)| **limb != 0)
        .map(|(index, limb)| {
            index * u64::BITS as usize + (u64::BITS - 1 - limb.leading_zeros()) as usize
        })
}

fn shifted_low_u64(magnitude: &FloatMagnitude, shift: usize) -> u64 {
    let limb = shift / u64::BITS as usize;
    let offset = shift % u64::BITS as usize;
    let mut value = magnitude[limb] >> offset;
    if offset > 0 && limb + 1 < magnitude.len() {
        value |= magnitude[limb + 1] << (u64::BITS as usize - offset);
    }
    value
}

fn bit_is_set(magnitude: &FloatMagnitude, bit: usize) -> bool {
    magnitude[bit / u64::BITS as usize] & (1_u64 << (bit % u64::BITS as usize)) != 0
}

fn any_bits_below(magnitude: &FloatMagnitude, bit_count: usize) -> bool {
    let full_limbs = bit_count / u64::BITS as usize;
    if magnitude[..full_limbs].iter().any(|limb| *limb != 0) {
        return true;
    }
    let remaining = bit_count % u64::BITS as usize;
    remaining > 0 && magnitude[full_limbs] & ((1_u64 << remaining) - 1) != 0
}

fn round_fraction(remainder: u64, divisor: u64, odd: bool) -> bool {
    match (u128::from(remainder) * 2).cmp(&u128::from(divisor)) {
        Ordering::Greater => true,
        Ordering::Equal => odd,
        Ordering::Less => false,
    }
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt(i128),
    SumFloat(ExactFloatSum),
    Min(Option<usize>),
    Max(Option<usize>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: ExactFloatSum, count: u64 },
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => Self::SumInt(0),
            AggregateFunction::Sum => Self::SumFloat(ExactFloatSum::new()),
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat {
                sum: ExactFloatSum::new(),
                count: 0,
            },
        }
    }

    fn update_untracked(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        self.update(spec, table, row, None).map(|_| ())
    }

    fn update_tracked(
        &mut self,
        spec: &AggregateSpec,
        table: &Table,
        row: usize,
        context: &mut ExecutionContext<'_>,
    ) -> Result<usize> {
        self.update(spec, table, row, Some(context))
    }

    fn update(
        &mut self,
        spec: &AggregateSpec,
        table: &Table,
        row: usize,
        context: Option<&mut ExecutionContext<'_>>,
    ) -> Result<usize> {
        let mut added_memory = 0;
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
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                added_memory = match context {
                    Some(context) => sum.add_tracked(values[row], context)?,
                    None => {
                        sum.add_untracked(values[row])?;
                        0
                    }
                };
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
                added_memory = match context {
                    Some(context) => sum.add_tracked(values[row], context)?,
                    None => {
                        sum.add_untracked(values[row])?;
                        0
                    }
                };
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
        }
        Ok(added_memory)
    }

    fn merge(&mut self, partial: Self, spec: &AggregateSpec, table: &Table) -> Result<()> {
        match (self, partial) {
            (Self::Count(count), Self::Count(partial)) => {
                *count = count
                    .checked_add(partial)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            (Self::SumInt(sum), Self::SumInt(partial)) => {
                *sum = sum
                    .checked_add(partial)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            (Self::SumFloat(sum), Self::SumFloat(partial)) => sum.merge(partial)?,
            (Self::Min(current), Self::Min(partial)) => {
                if let Some(partial) = partial {
                    let column = &table.columns()[spec.argument.expect("MIN argument")];
                    if current.is_none_or(|existing| {
                        column.value_ref(partial) < column.value_ref(existing)
                    }) {
                        *current = Some(partial);
                    }
                }
            }
            (Self::Max(current), Self::Max(partial)) => {
                if let Some(partial) = partial {
                    let column = &table.columns()[spec.argument.expect("MAX argument")];
                    if current.is_none_or(|existing| {
                        column.value_ref(partial) > column.value_ref(existing)
                    }) {
                        *current = Some(partial);
                    }
                }
            }
            (
                Self::AvgInt { sum, count },
                Self::AvgInt {
                    sum: partial_sum,
                    count: partial_count,
                },
            ) => {
                *sum = sum
                    .checked_add(partial_sum)
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(partial_count)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            (
                Self::AvgFloat { sum, count },
                Self::AvgFloat {
                    sum: partial_sum,
                    count: partial_count,
                },
            ) => {
                sum.merge(partial_sum)?;
                *count = count
                    .checked_add(partial_count)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            _ => unreachable!("aggregate states for one specification have the same variant"),
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
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt(value) => i64::try_from(value)
                .map(Value::Int64)
                .map_err(|_| Error::NumericOverflow("SUM(Int64)".to_owned())),
            Self::SumFloat(value) => value
                .finish(1)
                .map(Value::Float64)
                .ok_or_else(|| Error::NumericOverflow("SUM(Float64)".to_owned())),
            Self::Min(Some(row)) | Self::Max(Some(row)) => clone_value_ref_tracked(
                table.columns()[spec.argument.expect("extremum argument")].value_ref(row),
                context,
            ),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => sum
                .finish(count)
                .map(Value::Float64)
                .ok_or_else(|| Error::NumericOverflow("AVG(Float64) sum".to_owned())),
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
    fn small_scans_stay_on_the_calling_thread_and_worker_counts_are_validated() {
        let caller = thread::current().id();
        let threads = execute_morsels(SCAN_MORSEL_ROWS, 8, |_| thread::current().id());
        assert_eq!(threads, vec![caller]);

        let mut database = Database::with_worker_count(2).expect("valid worker count");
        assert_eq!(database.worker_count(), 2);
        assert!(matches!(
            database.set_worker_count(0),
            Err(Error::InvalidConfiguration(message))
                if message == "worker count must be at least 1"
        ));
        assert_eq!(database.worker_count(), 2);
    }

    #[test]
    fn filters_and_aggregates_are_equivalent_across_worker_counts() {
        let row_count = SCAN_MORSEL_ROWS * 3 + 137;
        let mut database = Database::with_worker_count(1).expect("valid worker count");
        database
            .execute(
                "CREATE TABLE parallel_data (
                    id Int64, bucket Int64, amount Int64,
                    reading Float64, keep Bool, unique_key String
                 );",
            )
            .expect("create table");
        let table = database
            .catalog
            .table_mut("parallel_data")
            .expect("table exists");
        for row in 0..row_count {
            table
                .insert_row(vec![
                    Value::Int64(row as i64),
                    Value::Int64((row % 257) as i64),
                    Value::Int64((row % 31) as i64 - 15),
                    Value::Float64(((row % 19) as f64 - 9.0) * 0.25),
                    Value::Bool(row % 3 != 0),
                    Value::String(format!("key-{row:05}")),
                ])
                .expect("generated row is valid");
        }

        let statements = [
            "SELECT id, unique_key FROM parallel_data
             WHERE keep = true AND id >= 4000;",
            "SELECT COUNT(*) AS rows, SUM(amount) AS total,
                    MIN(reading) AS low, MAX(reading) AS high,
                    AVG(reading) AS mean
             FROM parallel_data WHERE keep = true;",
            "SELECT bucket, COUNT(*) AS rows, SUM(amount) AS total,
                    MIN(reading) AS low, MAX(reading) AS high,
                    AVG(reading) AS mean
             FROM parallel_data WHERE keep = true GROUP BY bucket;",
            "SELECT unique_key, COUNT(*) AS rows, SUM(amount) AS total
             FROM parallel_data WHERE keep = true GROUP BY unique_key;",
        ];
        let expected = statements
            .iter()
            .map(|statement| query(&mut database, statement))
            .collect::<Vec<_>>();

        assert_eq!(expected[1].rows.len(), 1);
        assert_eq!(expected[2].rows.len(), 257);
        assert_eq!(
            expected[3].rows.len(),
            (0..row_count).filter(|row| row % 3 != 0).count()
        );
        assert!(expected[3].rows.iter().all(|row| row[1] == Value::Int64(1)));

        for worker_count in [2, 4] {
            database
                .set_worker_count(worker_count)
                .expect("valid worker count");
            for (statement, expected) in statements.iter().zip(&expected) {
                assert_eq!(
                    query(&mut database, statement),
                    *expected,
                    "query differed with {worker_count} workers: {statement}"
                );
            }
        }
    }

    #[test]
    fn parallel_grouping_falls_back_to_spill_when_partials_exceed_the_budget() {
        let mut database = Database::with_limits(ExecutionLimits {
            max_memory_bytes: 1_280,
            ..ExecutionLimits::default()
        });
        database.set_worker_count(4).expect("valid worker count");
        database
            .execute("CREATE TABLE spill_fallback (bucket Int64, value Int64);")
            .expect("create table");
        let table = database
            .catalog
            .table_mut("spill_fallback")
            .expect("table exists");
        for row in 0..(SCAN_MORSEL_ROWS + 904) {
            table
                .insert_row(vec![Value::Int64((row % 4) as i64), Value::Int64(1)])
                .expect("generated row is valid");
        }

        let result = query(
            &mut database,
            "SELECT bucket, SUM(value) AS total
             FROM spill_fallback GROUP BY bucket ORDER BY bucket;",
        );
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Int64(0), Value::Int64(1_250)],
                vec![Value::Int64(1), Value::Int64(1_250)],
                vec![Value::Int64(2), Value::Int64(1_250)],
                vec![Value::Int64(3), Value::Int64(1_250)],
            ]
        );
        assert!(database.last_execution_stats().spill_runs > 0);
        assert!(database.last_execution_stats().peak_memory_bytes <= 1_280);
    }

    #[test]
    fn parallel_aggregate_overflow_is_reported_after_morsel_merging() {
        let row_count = SCAN_MORSEL_ROWS * 3;
        let mut database = Database::with_worker_count(4).expect("valid worker count");
        database
            .execute(
                "CREATE TABLE overflow_data (
                    group_key String, local_value Int64, merge_value Int64
                 );",
            )
            .expect("create table");
        let table = database
            .catalog
            .table_mut("overflow_data")
            .expect("table exists");
        for row in 0..row_count {
            let local_value = match row {
                value if value == SCAN_MORSEL_ROWS + 10 => i64::MAX,
                value if value == SCAN_MORSEL_ROWS + 11 => 1,
                _ => 0,
            };
            let merge_value = match row {
                value if value == SCAN_MORSEL_ROWS - 1 => i64::MAX,
                value if value == SCAN_MORSEL_ROWS => 1,
                _ => 0,
            };
            table
                .insert_row(vec![
                    Value::String("all".to_owned()),
                    Value::Int64(local_value),
                    Value::Int64(merge_value),
                ])
                .expect("generated row is valid");
        }

        assert_eq!(
            database
                .execute("SELECT SUM(local_value) FROM overflow_data;")
                .expect_err("an unrepresentable morsel total is rejected"),
            Error::NumericOverflow("SUM(Int64)".to_owned())
        );
        assert_eq!(
            database
                .execute(
                    "SELECT group_key, SUM(merge_value)
                     FROM overflow_data GROUP BY group_key;",
                )
                .expect_err("an unrepresentable cross-morsel total is rejected"),
            Error::NumericOverflow("SUM(Int64)".to_owned())
        );
    }

    #[test]
    fn numeric_aggregates_allow_cancellation_across_morsel_boundaries() {
        let row_count = SCAN_MORSEL_ROWS * 2;
        let mut database = Database::with_worker_count(1).expect("valid worker count");
        database
            .execute("CREATE TABLE cancellation (integer_value Int64, float_value Float64);")
            .expect("create table");
        let table = database
            .catalog
            .table_mut("cancellation")
            .expect("table exists");
        for row in 0..row_count {
            let integer_value = match row {
                0 => i64::MIN,
                value if value == SCAN_MORSEL_ROWS => i64::MAX,
                value if value == SCAN_MORSEL_ROWS + 1 => 1,
                _ => 0,
            };
            let float_value = match row {
                0 => -1e308,
                value if value == SCAN_MORSEL_ROWS => 1e308,
                value if value == SCAN_MORSEL_ROWS + 1 => 1e308,
                _ => 0.0,
            };
            table
                .insert_row(vec![
                    Value::Int64(integer_value),
                    Value::Float64(float_value),
                ])
                .expect("generated row is valid");
        }

        let expected = vec![vec![
            Value::Int64(0),
            Value::Float64(1e308),
            Value::Float64(1e308 / row_count as f64),
        ]];
        for worker_count in [1, 4] {
            database
                .set_worker_count(worker_count)
                .expect("valid worker count");
            assert_eq!(
                query(
                    &mut database,
                    "SELECT SUM(integer_value), SUM(float_value), AVG(float_value)
                     FROM cancellation;"
                )
                .rows,
                expected,
                "cross-morsel cancellation failed with {worker_count} workers"
            );
        }
    }

    #[test]
    fn exact_float_accumulator_handles_large_averages_and_subnormal_rounding() {
        let mut large = ExactFloatSum::new();
        large.add_untracked(f64::MAX).expect("finite value");
        large.add_untracked(f64::MAX).expect("finite value");
        assert_eq!(large.finish(2), Some(f64::MAX));

        let mut overflow = ExactFloatSum::new();
        overflow.add_untracked(f64::MAX).expect("finite value");
        overflow.add_untracked(f64::MAX).expect("finite value");
        assert_eq!(overflow.finish(1), None);

        let smallest = f64::from_bits(1);
        let mut rounds_to_even_zero = ExactFloatSum::new();
        rounds_to_even_zero
            .add_untracked(smallest)
            .expect("finite value");
        assert_eq!(rounds_to_even_zero.finish(2), Some(0.0));

        let mut rounds_to_even_two = ExactFloatSum::new();
        for _ in 0..3 {
            rounds_to_even_two
                .add_untracked(smallest)
                .expect("finite value");
        }
        assert_eq!(rounds_to_even_two.finish(2), Some(f64::from_bits(2)));
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
