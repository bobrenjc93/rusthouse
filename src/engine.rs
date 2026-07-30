use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement, WindowFrame, WindowFunction,
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
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;

        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row) == TruthValue::True)
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs, window_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(table, &matching_rows, &group_columns, &aggregate_specs)?;
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
            let windows = execute_windows(table, &matching_rows, &window_specs)?;
            order_source_rows(
                &mut matching_rows,
                table,
                &items,
                &windows,
                &ordering,
                select.limit,
            );
            execute_projection(table, &matching_rows, &items, &windows)
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
    Window {
        state: usize,
    },
}

#[derive(Debug, Clone)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
}

#[derive(Debug)]
struct WindowSpec {
    function: ResolvedWindowFunction,
    partition_by: Vec<usize>,
    order_by: Vec<WindowOrder>,
}

#[derive(Debug)]
enum ResolvedWindowFunction {
    Ranking(WindowFunction),
    Aggregate {
        aggregate: AggregateSpec,
        frame: WindowFrame,
    },
}

#[derive(Debug)]
struct WindowOrder {
    source: usize,
    descending: bool,
}

type ResolvedSelectItems = (
    Vec<ResolvedItem>,
    Vec<ResultColumn>,
    Vec<AggregateSpec>,
    Vec<WindowSpec>,
);

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
) -> Result<ResolvedSelectItems> {
    let has_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    let has_window = requested.iter().any(|item| {
        matches!(
            item,
            SelectItem::Window { .. } | SelectItem::AggregateWindow { .. }
        )
    });
    if has_window && (has_aggregate || !group_columns.is_empty()) {
        return Err(Error::InvalidQuery(
            "window functions cannot be combined with aggregates or GROUP BY".to_owned(),
        ));
    }
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
    let mut window_specs = Vec::new();

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
                let (aggregate, argument_name) =
                    resolve_aggregate_spec(table, *function, argument)?;
                let input_type = aggregate.input_type;
                let state = aggregate_specs.len();
                aggregate_specs.push(aggregate);
                items.push(ResolvedItem::Aggregate { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: aggregate_output_type(*function, input_type),
                });
            }
            SelectItem::Window {
                function,
                specification,
                alias,
            } => {
                let state = window_specs.len();
                window_specs.push(resolve_window_spec(
                    table,
                    ResolvedWindowFunction::Ranking(*function),
                    specification,
                    function.name(),
                )?);
                items.push(ResolvedItem::Window { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}()", function.name())),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::AggregateWindow {
                function,
                argument,
                specification,
                alias,
            } => {
                let (aggregate, argument_name) =
                    resolve_aggregate_spec(table, *function, argument)?;
                let output_type = aggregate_output_type(*function, aggregate.input_type);
                let frame = specification
                    .frame
                    .expect("aggregate windows require a parsed frame");
                let state = window_specs.len();
                window_specs.push(resolve_window_spec(
                    table,
                    ResolvedWindowFunction::Aggregate { aggregate, frame },
                    specification,
                    function.name(),
                )?);
                items.push(ResolvedItem::Window { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: output_type,
                });
            }
        }
    }

    Ok((items, result_columns, aggregate_specs, window_specs))
}

fn resolve_aggregate_spec(
    table: &Table,
    function: AggregateFunction,
    argument: &AggregateArgument,
) -> Result<(AggregateSpec, String)> {
    let (argument, input_type, argument_name) = match argument {
        AggregateArgument::Wildcard => {
            if function != AggregateFunction::Count {
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
    validate_aggregate(function, input_type)?;
    Ok((
        AggregateSpec {
            function,
            argument,
            input_type,
        },
        argument_name,
    ))
}

fn resolve_window_spec(
    table: &Table,
    function: ResolvedWindowFunction,
    requested: &sql::WindowSpec,
    function_name: &str,
) -> Result<WindowSpec> {
    let mut partition_by = Vec::with_capacity(requested.partition_by.len());
    for name in &requested.partition_by {
        let source = table.column_index(name)?;
        if partition_by.contains(&source) {
            return Err(Error::InvalidQuery(format!(
                "{function_name} PARTITION BY column '{name}' is listed more than once"
            )));
        }
        partition_by.push(source);
    }

    let mut order_by = Vec::with_capacity(requested.order_by.len());
    for order in &requested.order_by {
        let source = table.column_index(&order.name)?;
        if order_by
            .iter()
            .any(|resolved: &WindowOrder| resolved.source == source)
        {
            return Err(Error::InvalidQuery(format!(
                "{function_name} window ORDER BY column '{}' is listed more than once",
                order.name
            )));
        }
        order_by.push(WindowOrder {
            source,
            descending: order.descending,
        });
    }

    Ok(WindowSpec {
        function,
        partition_by,
        order_by,
    })
}

fn validate_aggregate(function: AggregateFunction, input_type: Option<DataType>) -> Result<()> {
    if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
        && !matches!(
            input_type.map(DataType::underlying),
            Some(DataType::Int64 | DataType::Float64)
        )
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
        AggregateFunction::Avg => DataType::NullableFloat64,
        AggregateFunction::Sum => input_type
            .expect("validated column argument")
            .underlying()
            .nullable(),
        AggregateFunction::Min | AggregateFunction::Max => input_type
            .expect("validated column argument")
            .underlying()
            .nullable(),
    }
}

fn execute_projection(
    table: &Table,
    matching_rows: &[usize],
    items: &[ResolvedItem],
    windows: &[Vec<Value>],
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
                    ResolvedItem::Window { state } => windows[*state][*row].clone(),
                })
                .collect()
        })
        .collect()
}

fn execute_windows(
    table: &Table,
    matching_rows: &[usize],
    specifications: &[WindowSpec],
) -> Result<Vec<Vec<Value>>> {
    specifications
        .iter()
        .map(|specification| execute_window(table, matching_rows, specification))
        .collect()
}

fn execute_window(
    table: &Table,
    matching_rows: &[usize],
    specification: &WindowSpec,
) -> Result<Vec<Value>> {
    let mut sorted_rows = matching_rows.to_vec();
    sorted_rows.sort_unstable_by(|left, right| {
        compare_partition_rows(table, specification, *left, *right)
            .then_with(|| compare_window_order(table, specification, *left, *right))
            .then_with(|| left.cmp(right))
    });

    let mut values = vec![Value::Null; table.row_count()];
    let mut partition_start = 0;
    while partition_start < sorted_rows.len() {
        let first = sorted_rows[partition_start];
        let mut partition_end = partition_start + 1;
        while partition_end < sorted_rows.len()
            && compare_partition_rows(table, specification, first, sorted_rows[partition_end])
                == Ordering::Equal
        {
            partition_end += 1;
        }

        let partition = &sorted_rows[partition_start..partition_end];
        match &specification.function {
            ResolvedWindowFunction::Ranking(function) => {
                execute_ranking_partition(table, specification, *function, partition, &mut values)?;
            }
            ResolvedWindowFunction::Aggregate { aggregate, frame } => {
                execute_aggregate_partition(table, aggregate, *frame, partition, &mut values)?;
            }
        }
        partition_start = partition_end;
    }
    Ok(values)
}

fn execute_ranking_partition(
    table: &Table,
    specification: &WindowSpec,
    function: WindowFunction,
    rows: &[usize],
    output: &mut [Value],
) -> Result<()> {
    let mut rank = 1_usize;
    let mut dense_rank = 1_usize;
    for (position, row) in rows.iter().copied().enumerate() {
        if position > 0
            && compare_window_order(table, specification, rows[position - 1], row)
                != Ordering::Equal
        {
            rank = position + 1;
            dense_rank += 1;
        }
        let value = match function {
            WindowFunction::RowNumber => position + 1,
            WindowFunction::Rank => rank,
            WindowFunction::DenseRank => dense_rank,
        };
        output[row] = Value::Int64(
            i64::try_from(value).map_err(|_| Error::NumericOverflow(function.name().to_owned()))?,
        );
    }
    Ok(())
}

fn execute_aggregate_partition(
    table: &Table,
    aggregate: &AggregateSpec,
    frame: WindowFrame,
    rows: &[usize],
    output: &mut [Value],
) -> Result<()> {
    match aggregate.function {
        AggregateFunction::Count => execute_window_count(table, aggregate, frame, rows, output),
        AggregateFunction::Sum | AggregateFunction::Avg => {
            match aggregate
                .input_type
                .expect("SUM and AVG have a column argument")
                .underlying()
            {
                DataType::Int64 => execute_window_int(table, aggregate, frame, rows, output),
                DataType::Float64 => execute_window_float(table, aggregate, frame, rows, output),
                _ => unreachable!("SUM and AVG input types are validated"),
            }
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            execute_window_extreme(table, aggregate, frame, rows, output);
            Ok(())
        }
    }
}

fn execute_window_count(
    table: &Table,
    aggregate: &AggregateSpec,
    frame: WindowFrame,
    rows: &[usize],
    output: &mut [Value],
) -> Result<()> {
    let counts = prefix_counts(table, rows, aggregate.argument)?;
    for (position, row) in rows.iter().copied().enumerate() {
        let start = frame_start(frame, position);
        let count = counts[position + 1] - counts[start];
        output[row] = Value::Int64(
            i64::try_from(count).map_err(|_| Error::NumericOverflow("COUNT window".to_owned()))?,
        );
    }
    Ok(())
}

fn prefix_counts(table: &Table, rows: &[usize], argument: Option<usize>) -> Result<Vec<u64>> {
    let mut counts = Vec::with_capacity(rows.len() + 1);
    counts.push(0_u64);
    for row in rows {
        let present = argument.is_none_or(|column| !table.columns()[column].is_null(*row));
        let next = counts
            .last()
            .copied()
            .expect("prefix has an initial state")
            .checked_add(u64::from(present))
            .ok_or_else(|| Error::NumericOverflow("window aggregate count".to_owned()))?;
        counts.push(next);
    }
    Ok(counts)
}

fn execute_window_int(
    table: &Table,
    aggregate: &AggregateSpec,
    frame: WindowFrame,
    rows: &[usize],
    output: &mut [Value],
) -> Result<()> {
    let argument = aggregate.argument.expect("numeric aggregate argument");
    let counts = prefix_counts(table, rows, Some(argument))?;
    let mut sums = Vec::with_capacity(rows.len() + 1);
    sums.push(0_i128);
    for row in rows {
        let value = match table.columns()[argument].value_ref(*row) {
            ValueRef::Int64(value) => i128::from(value),
            ValueRef::Null => 0,
            _ => unreachable!("Int64 window input is resolved"),
        };
        let next = sums
            .last()
            .copied()
            .expect("prefix has an initial state")
            .checked_add(value)
            .ok_or_else(|| Error::NumericOverflow("window Int64 prefix sum".to_owned()))?;
        sums.push(next);
    }

    for (position, row) in rows.iter().copied().enumerate() {
        let start = frame_start(frame, position);
        let count = counts[position + 1] - counts[start];
        if count == 0 {
            output[row] = Value::Null;
            continue;
        }
        let sum = sums[position + 1]
            .checked_sub(sums[start])
            .ok_or_else(|| Error::NumericOverflow("window Int64 frame sum".to_owned()))?;
        output[row] = match aggregate.function {
            AggregateFunction::Sum => Value::Int64(
                i64::try_from(sum)
                    .map_err(|_| Error::NumericOverflow("SUM(Int64) window".to_owned()))?,
            ),
            AggregateFunction::Avg => Value::Float64(sum as f64 / count as f64),
            _ => unreachable!("numeric prefix is only used for SUM and AVG"),
        };
    }
    Ok(())
}

fn execute_window_float(
    table: &Table,
    aggregate: &AggregateSpec,
    frame: WindowFrame,
    rows: &[usize],
    output: &mut [Value],
) -> Result<()> {
    let argument = aggregate.argument.expect("numeric aggregate argument");
    let counts = prefix_counts(table, rows, Some(argument))?;
    let mut sums = Vec::with_capacity(rows.len() + 1);
    sums.push(0.0_f64);
    for row in rows {
        let value = match table.columns()[argument].value_ref(*row) {
            ValueRef::Float64(value) => value,
            ValueRef::Null => 0.0,
            _ => unreachable!("Float64 window input is resolved"),
        };
        let next = sums.last().copied().expect("prefix has an initial state") + value;
        if !next.is_finite() {
            return Err(Error::NumericOverflow(
                "window Float64 prefix sum".to_owned(),
            ));
        }
        sums.push(next);
    }

    for (position, row) in rows.iter().copied().enumerate() {
        let start = frame_start(frame, position);
        let count = counts[position + 1] - counts[start];
        if count == 0 {
            output[row] = Value::Null;
            continue;
        }
        let sum = sums[position + 1] - sums[start];
        let value = match aggregate.function {
            AggregateFunction::Sum => sum,
            AggregateFunction::Avg => sum / count as f64,
            _ => unreachable!("numeric prefix is only used for SUM and AVG"),
        };
        if !value.is_finite() {
            return Err(Error::NumericOverflow(format!(
                "{}(Float64) window",
                aggregate.function.name()
            )));
        }
        output[row] = Value::Float64(value);
    }
    Ok(())
}

fn execute_window_extreme(
    table: &Table,
    aggregate: &AggregateSpec,
    frame: WindowFrame,
    rows: &[usize],
    output: &mut [Value],
) {
    let argument = aggregate.argument.expect("MIN/MAX aggregate argument");
    let column = &table.columns()[argument];
    let minimum = aggregate.function == AggregateFunction::Min;
    let mut queue = VecDeque::<(usize, usize)>::new();

    for (position, row) in rows.iter().copied().enumerate() {
        let start = frame_start(frame, position);
        while queue.front().is_some_and(|(index, _)| *index < start) {
            queue.pop_front();
        }
        if !column.is_null(row) {
            let candidate = column.value_ref(row);
            while queue.back().is_some_and(|(_, queued_row)| {
                let comparison = column.value_ref(*queued_row).cmp(&candidate);
                if minimum {
                    comparison != Ordering::Less
                } else {
                    comparison != Ordering::Greater
                }
            }) {
                queue.pop_back();
            }
            queue.push_back((position, row));
        }
        output[row] = queue
            .front()
            .map_or(Value::Null, |(_, row)| column.value(*row));
    }
}

fn frame_start(frame: WindowFrame, position: usize) -> usize {
    match frame {
        WindowFrame::UnboundedPreceding => 0,
        WindowFrame::Preceding(preceding) => position.saturating_sub(preceding),
    }
}

fn compare_partition_rows(
    table: &Table,
    specification: &WindowSpec,
    left: usize,
    right: usize,
) -> Ordering {
    for source in &specification.partition_by {
        let comparison = table.columns()[*source].cmp_at(left, right);
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    Ordering::Equal
}

fn compare_window_order(
    table: &Table,
    specification: &WindowSpec,
    left: usize,
    right: usize,
) -> Ordering {
    for order in &specification.order_by {
        let comparison = table.columns()[order.source].cmp_at(left, right);
        if comparison != Ordering::Equal {
            return if order.descending {
                comparison.reverse()
            } else {
                comparison
            };
        }
    }
    Ordering::Equal
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
                        ResolvedItem::Window { .. } => {
                            unreachable!("grouped projections cannot contain windows")
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
    SumInt { sum: i64, seen: bool },
    SumFloat { sum: f64, seen: bool },
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: f64, count: u64 },
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum
                if spec.input_type.map(DataType::underlying) == Some(DataType::Int64) =>
            {
                Self::SumInt {
                    sum: 0,
                    seen: false,
                }
            }
            AggregateFunction::Sum => Self::SumFloat {
                sum: 0.0,
                seen: false,
            },
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg
                if spec.input_type.map(DataType::underlying) == Some(DataType::Int64) =>
            {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        if spec
            .argument
            .is_some_and(|argument| table.columns()[argument].is_null(row))
        {
            return Ok(());
        }

        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt { sum, seen } => {
                let ValueRef::Int64(value) =
                    table.columns()[spec.argument.expect("SUM argument")].value_ref(row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
                *seen = true;
            }
            Self::SumFloat { sum, seen } => {
                let ValueRef::Float64(value) =
                    table.columns()[spec.argument.expect("SUM argument")].value_ref(row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
                *seen = true;
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
                let ValueRef::Int64(value) =
                    table.columns()[spec.argument.expect("AVG argument")].value_ref(row)
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
                let ValueRef::Float64(value) =
                    table.columns()[spec.argument.expect("AVG argument")].value_ref(row)
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
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt { sum, seen: true } => Ok(Value::Int64(sum)),
            Self::SumFloat { sum, seen: true } => Ok(Value::Float64(sum)),
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum / count as f64)),
            Self::SumInt { seen: false, .. }
            | Self::SumFloat { seen: false, .. }
            | Self::Min(None)
            | Self::Max(None)
            | Self::AvgInt { .. }
            | Self::AvgFloat { .. } => Ok(Value::Null),
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
    windows: &[Vec<Value>],
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
            let comparison = match items[order.output] {
                ResolvedItem::Column { source, .. } => table.columns()[source].cmp_at(left, right),
                ResolvedItem::Window { state } => windows[state][left].cmp(&windows[state][right]),
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("ungrouped projections cannot contain aggregates")
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
                ResolvedItem::Window { .. } => {
                    unreachable!("grouped projections cannot contain windows")
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
    IsNull {
        operand: CompiledOperand,
        negated: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruthValue {
    False,
    True,
    Unknown,
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
                let result = match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                };
                if result {
                    TruthValue::True
                } else {
                    TruthValue::False
                }
            }
            Self::IsNull { operand, negated } => {
                let is_null = matches!(operand.value(table, row), ValueRef::Null);
                if is_null != *negated {
                    TruthValue::True
                } else {
                    TruthValue::False
                }
            }
            Self::And(left, right) => match left.evaluate(table, row) {
                TruthValue::False => TruthValue::False,
                TruthValue::True => right.evaluate(table, row),
                TruthValue::Unknown => match right.evaluate(table, row) {
                    TruthValue::False => TruthValue::False,
                    TruthValue::True | TruthValue::Unknown => TruthValue::Unknown,
                },
            },
            Self::Or(left, right) => match left.evaluate(table, row) {
                TruthValue::True => TruthValue::True,
                TruthValue::False => right.evaluate(table, row),
                TruthValue::Unknown => match right.evaluate(table, row) {
                    TruthValue::True => TruthValue::True,
                    TruthValue::False | TruthValue::Unknown => TruthValue::Unknown,
                },
            },
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
        Predicate::IsNull { operand, negated } => Ok(CompiledPredicate::IsNull {
            operand: compile_operand(table, operand)?,
            negated: *negated,
        }),
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
    left == DataType::Null
        || right == DataType::Null
        || left.underlying() == right.underlying()
        || matches!(
            (left.underlying(), right.underlying()),
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
