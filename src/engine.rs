//! SQL execution and structured query results.

use std::cmp::Ordering;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::catalog::Catalog;
use crate::error::{Error, Resource, Result};
use crate::execution::{ExecutionContext, ExecutionLimits, ExecutionStats, estimated_row_bytes};
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
    spill_directory: PathBuf,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
            limits: ExecutionLimits::default(),
            last_execution_stats: ExecutionStats::default(),
            spill_directory: std::env::temp_dir(),
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
            spill_directory: spill_directory.into(),
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
            let parsed = sql::parse_bounded(sql, &limits)?;
            context.stats.tokens = parsed.token_count;
            context.stats.statements = parsed.statements.len();
            let mut results = Vec::with_capacity(parsed.statements.len());
            for statement in parsed.statements {
                results.push(self.execute_statement(statement, &mut context)?);
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
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

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

fn execute_ungrouped(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
    context: &mut ExecutionContext<'_>,
    spill_directory: &Path,
) -> Result<Vec<Vec<Value>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    if ordering.is_empty() {
        for row in 0..table.row_count() {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                context.add_intermediate_rows(1)?;
                let projected = project_source_row(table, row, items);
                context.add_result_row(&projected)?;
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
    let mut sorter = IndexSorter::new(compare, context.limits().max_memory_bytes, spill_directory)?;
    for row in 0..table.row_count() {
        if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
            context.add_intermediate_rows(1)?;
            sorter.add(row, context)?;
        }
    }
    sorter.prepare(context)?;
    let working_memory = sorter.working_memory_bytes();
    context.reserve_memory(working_memory)?;
    let outcome = sorter.drain(|row| {
        if limit.is_some_and(|limit| rows.len() == limit) {
            return Ok(false);
        }
        let projected = project_source_row(table, row, items);
        context.add_result_row(&projected)?;
        rows.push(projected);
        Ok(true)
    });
    context.release_memory(working_memory);
    outcome?;
    Ok(rows)
}

fn execute_grouped(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    context: &mut ExecutionContext<'_>,
    spill_directory: &Path,
) -> Result<GroupedData> {
    let mut data = GroupedData::default();
    if group_columns.is_empty() {
        let mut states = aggregate_specs
            .iter()
            .map(AggregateState::new)
            .collect::<Vec<_>>();
        for row in 0..table.row_count() {
            if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
                context.add_intermediate_rows(1)?;
                update_aggregates(&mut states, aggregate_specs, table, row)?;
            }
        }
        push_group(&mut data, Vec::new(), states, context)?;
        return Ok(data);
    }

    let compare = |left: usize, right: usize| {
        compare_group_keys(table, group_columns, left, right).then_with(|| left.cmp(&right))
    };
    let mut sorter = IndexSorter::new(compare, context.limits().max_memory_bytes, spill_directory)?;
    for row in 0..table.row_count() {
        if predicate.is_none_or(|predicate| predicate.evaluate(table, row)) {
            context.add_intermediate_rows(1)?;
            sorter.add(row, context)?;
        }
    }
    sorter.prepare(context)?;
    let sorter_memory = sorter.working_memory_bytes();
    context.reserve_memory(sorter_memory)?;

    let mut current_key_row: Option<usize> = None;
    let mut states = aggregate_specs
        .iter()
        .map(AggregateState::new)
        .collect::<Vec<_>>();
    let outcome = sorter.drain(|row| {
        let belongs_to_current = current_key_row.is_some_and(|key_row| {
            compare_group_keys(table, group_columns, key_row, row) == Ordering::Equal
        });
        if current_key_row.is_some() && !belongs_to_current {
            let key_row = current_key_row.take().expect("current group has a key");
            let key = group_columns
                .iter()
                .map(|column| table.columns()[*column].value(key_row))
                .collect();
            let finished = std::mem::replace(
                &mut states,
                aggregate_specs.iter().map(AggregateState::new).collect(),
            );
            push_group(&mut data, key, finished, context)?;
        }
        if current_key_row.is_none() {
            current_key_row = Some(row);
        }
        update_aggregates(&mut states, aggregate_specs, table, row)?;
        Ok(true)
    });
    context.release_memory(sorter_memory);
    outcome?;
    if let Some(key_row) = current_key_row {
        let key = group_columns
            .iter()
            .map(|column| table.columns()[*column].value(key_row))
            .collect();
        push_group(&mut data, key, states, context)?;
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
    key: Vec<Value>,
    states: Vec<AggregateState>,
    context: &mut ExecutionContext<'_>,
) -> Result<()> {
    let aggregates = states
        .into_iter()
        .map(AggregateState::finish)
        .collect::<Result<Vec<_>>>()?;
    context.add_intermediate_rows(1)?;
    let bytes = estimated_row_bytes(&key).saturating_add(estimated_row_bytes(&aggregates));
    context.reserve_memory(bytes)?;
    data.memory_bytes = data.memory_bytes.saturating_add(bytes);
    data.groups.push(GroupRow { key, aggregates });
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
    spill_directory: &Path,
) -> Result<Vec<Vec<Value>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }
    let compare = |left, right| compare_group_rows(data, items, ordering, left, right);
    let mut sorter = IndexSorter::new(compare, context.available_memory(), spill_directory)?;
    for group in 0..data.groups.len() {
        sorter.add(group, context)?;
    }
    sorter.prepare(context)?;
    let working_memory = sorter.working_memory_bytes();
    context.reserve_memory(working_memory)?;
    let mut rows = Vec::new();
    let outcome = sorter.drain(|group| {
        if limit.is_some_and(|limit| rows.len() == limit) {
            return Ok(false);
        }
        let values = items
            .iter()
            .map(|item| match item {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => data.groups[group].key[*position].clone(),
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Aggregate { state } => data.groups[group].aggregates[*state].clone(),
            })
            .collect::<Vec<_>>();
        context.add_result_row(&values)?;
        rows.push(values);
        Ok(true)
    });
    context.release_memory(working_memory);
    outcome?;
    Ok(rows)
}

static NEXT_SPILL_ID: AtomicU64 = AtomicU64::new(0);

struct IndexSorter<F> {
    compare: F,
    chunk_capacity: usize,
    fan_in: usize,
    chunk: Vec<usize>,
    runs: Vec<TempRun>,
    spill_directory: PathBuf,
    prepared: bool,
}

impl<F> IndexSorter<F>
where
    F: Fn(usize, usize) -> Ordering,
{
    fn new(compare: F, memory_bytes: usize, spill_directory: &Path) -> Result<Self> {
        if memory_bytes < size_of::<usize>() {
            return Err(Error::ResourceLimitExceeded {
                resource: Resource::MemoryBytes,
                limit: memory_bytes,
                actual: size_of::<usize>(),
            });
        }
        Ok(Self {
            compare,
            chunk_capacity: (memory_bytes / (2 * size_of::<usize>())).max(1),
            fan_in: (memory_bytes / 32).max(2),
            chunk: Vec::new(),
            runs: Vec::new(),
            spill_directory: spill_directory.to_owned(),
            prepared: false,
        })
    }

    fn add(&mut self, index: usize, context: &mut ExecutionContext<'_>) -> Result<()> {
        self.chunk.push(index);
        context.observe_operator_memory(self.chunk.len().saturating_mul(size_of::<usize>()))?;
        if self.chunk.len() >= self.chunk_capacity {
            self.flush_chunk(context)?;
        }
        Ok(())
    }

    fn prepare(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        if !self.runs.is_empty() {
            self.flush_chunk(context)?;
            while self.runs.len() > self.fan_in {
                let old_runs = std::mem::take(&mut self.runs);
                let mut pending = old_runs.into_iter();
                while let Some(first) = pending.next() {
                    let mut batch = vec![first];
                    batch.extend(pending.by_ref().take(self.fan_in - 1));
                    if batch.len() == 1 {
                        self.runs.push(batch.pop().expect("one run"));
                    } else {
                        let merged = self.merge_runs(&batch, context)?;
                        self.runs.push(merged);
                    }
                }
            }
        } else {
            self.chunk
                .sort_unstable_by(|left, right| (self.compare)(*left, *right));
        }
        self.prepared = true;
        Ok(())
    }

    fn working_memory_bytes(&self) -> usize {
        if self.runs.is_empty() {
            self.chunk.capacity().saturating_mul(size_of::<usize>())
        } else {
            self.runs.len().saturating_mul(32)
        }
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
        indices.sort_unstable_by(|left, right| (self.compare)(*left, *right));
        let (run, mut file) = TempRun::create(&self.spill_directory)?;
        for index in &indices {
            write_index(&mut file, *index)?;
        }
        file.sync_data()
            .map_err(|error| spill_error("syncing run", error))?;
        context.record_spill(indices.len().saturating_mul(size_of::<u64>()));
        self.runs.push(run);
        Ok(())
    }

    fn merge_runs(&self, runs: &[TempRun], context: &mut ExecutionContext<'_>) -> Result<TempRun> {
        context.observe_operator_memory(runs.len().saturating_mul(32))?;
        let mut readers = open_runs(runs)?;
        let mut heads = readers
            .iter_mut()
            .map(read_index)
            .collect::<Result<Vec<_>>>()?;
        let (output, mut file) = TempRun::create(&self.spill_directory)?;
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
    }
}

struct TempRun {
    path: PathBuf,
}

impl TempRun {
    fn create(directory: &Path) -> Result<(Self, File)> {
        for _ in 0..100 {
            let id = NEXT_SPILL_ID.fetch_add(1, AtomicOrdering::Relaxed);
            let path = directory.join(format!(".rusthouse-spill-{}-{id}", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
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
        let _ = remove_file(&self.path);
    }
}

fn open_runs(runs: &[TempRun]) -> Result<Vec<File>> {
    runs.iter()
        .map(|run| File::open(&run.path).map_err(|error| spill_error("opening run", error)))
        .collect()
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
