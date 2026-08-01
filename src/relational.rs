use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    mem::size_of,
    sync::Arc,
};

use crate::{
    DataType, Error, Result, Value,
    database::{ExecutionControl, QueryLimits},
    sql::{
        ColumnRef, Comparison, FrameBound, Join, JoinKind, OrderBy, Predicate, SelectItem,
        Statement, TableRef, WindowExpression, WindowFrame, WindowFunction,
    },
    storage::{ColumnDef, EngineTable as Table},
    value::compare_int_float,
};

pub(crate) struct QueryOutput {
    pub(crate) columns: Vec<ColumnDef>,
    pub(crate) rows: Vec<Vec<Value>>,
}

#[derive(Clone)]
struct BoundColumn {
    qualifier: String,
    definition: ColumnDef,
}

struct Source {
    columns: Vec<BoundColumn>,
    rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ValueKey(Vec<KeyPart>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyPart {
    Null,
    Int(i64),
    Float(u64),
    Bool(bool),
    String(String),
}

#[derive(Clone, Copy)]
struct BoundOrder {
    index: usize,
    descending: bool,
    nulls_first: bool,
}

enum BoundWindowFunction {
    RowNumber,
    Rank,
    Sum { index: usize, data_type: DataType },
    Count(Option<usize>),
}

struct BoundWindow {
    function: BoundWindowFunction,
    partition_by: Vec<usize>,
    order_by: Vec<BoundOrder>,
    frame: WindowFrame,
}

enum BoundProjection {
    Source(usize),
    Window(BoundWindow),
}

enum BoundResultOrder {
    Source(BoundOrder),
    Output {
        index: usize,
        descending: bool,
        nulls_first: bool,
    },
}

struct PartitionState {
    rows: Vec<usize>,
    row_count: usize,
}

pub(crate) fn execute(
    tables: &std::collections::BTreeMap<String, Arc<Table>>,
    statement: Statement,
    limits: QueryLimits,
    control: Option<ExecutionControl<'_>>,
) -> Result<QueryOutput> {
    let Statement::Select {
        from,
        projection,
        joins,
        predicates,
        order_by,
        limit,
    } = statement
    else {
        return Err(Error::Unsupported("statement is not a query".to_owned()));
    };

    let mut qualifiers = HashSet::new();
    let mut source = load_table(tables, &from, false, control)?;
    qualifiers.insert(table_qualifier(&from).to_owned());
    for join in joins {
        let qualifier = table_qualifier(&join.table);
        if !qualifiers.insert(qualifier.to_owned()) {
            return Err(Error::DuplicateTableAlias(qualifier.to_owned()));
        }
        source = execute_join(tables, source, join, limits, control)?;
    }

    let predicates = bind_predicates(&source.columns, &predicates)?;
    source.rows = filter_rows(source.rows, &predicates, control)?;

    let (bound_projection, columns) = bind_projection(&source.columns, &projection)?;
    let result_order = bind_result_order(&source.columns, &columns, &order_by)?;
    let window_values = evaluate_windows(&source, &bound_projection, limits, control)?;
    let output_count = limit.map_or(source.rows.len(), |limit| limit.min(source.rows.len()));
    enforce_row_limit("query output", output_count, limits.max_output_rows)?;
    let mut row_indexes = if result_order.is_empty() {
        (0..output_count).collect::<Vec<_>>()
    } else {
        (0..source.rows.len()).collect::<Vec<_>>()
    };
    if !result_order.is_empty() {
        check_cancellation(control)?;
        row_indexes.sort_unstable_by(|left, right| {
            compare_result_rows(
                *left,
                *right,
                &source,
                &bound_projection,
                &window_values,
                &result_order,
            )
            .then_with(|| left.cmp(right))
        });
        check_cancellation(control)?;
    }
    if let Some(limit) = limit {
        row_indexes.truncate(limit);
    }

    let rows = project_rows(
        &source,
        &bound_projection,
        &window_values,
        &row_indexes,
        &columns,
        control,
    )?;
    Ok(QueryOutput { columns, rows })
}

fn load_table(
    tables: &std::collections::BTreeMap<String, Arc<Table>>,
    table_ref: &TableRef,
    nullable: bool,
    control: Option<ExecutionControl<'_>>,
) -> Result<Source> {
    let table = tables
        .get(&table_ref.name)
        .ok_or_else(|| Error::TableNotFound(table_ref.name.clone()))?;
    let columns = table_columns(table, table_ref, nullable);
    let mut rows = Vec::with_capacity(table.row_count());
    for row_index in 0..table.row_count() {
        check_cancellation(control)?;
        rows.push(
            (0..table.schema().len())
                .map(|column| table.value(row_index, column))
                .collect(),
        );
    }
    Ok(Source { columns, rows })
}

fn table_columns(table: &Table, table_ref: &TableRef, nullable: bool) -> Vec<BoundColumn> {
    let qualifier = table_qualifier(table_ref).to_owned();
    table
        .schema()
        .iter()
        .map(|definition| {
            let mut definition = definition.clone();
            definition.nullable |= nullable;
            BoundColumn {
                qualifier: qualifier.clone(),
                definition,
            }
        })
        .collect()
}

fn table_qualifier(table: &TableRef) -> &str {
    table.alias.as_deref().unwrap_or(&table.name)
}

fn execute_join(
    tables: &std::collections::BTreeMap<String, Arc<Table>>,
    left: Source,
    join: Join,
    limits: QueryLimits,
    control: Option<ExecutionControl<'_>>,
) -> Result<Source> {
    let build_table = tables
        .get(&join.table.name)
        .ok_or_else(|| Error::TableNotFound(join.table.name.clone()))?;
    let right_columns = table_columns(build_table, &join.table, join.kind == JoinKind::Left);
    let left_width = left.columns.len();
    let mut combined_columns = left.columns.clone();
    combined_columns.extend(right_columns.iter().cloned());
    let equality = bind_join_keys(&combined_columns, left_width, &join.equality)?;
    enforce_row_limit(
        "hash join build",
        build_table.row_count(),
        limits.max_join_build_rows,
    )?;
    let build_bytes = estimated_table_bytes(build_table, control)?;
    enforce_byte_limit("hash join build", build_bytes, limits.max_join_build_bytes)?;
    let mut right = load_table(tables, &join.table, join.kind == JoinKind::Left, control)?;

    let mut hash = HashMap::<ValueKey, Vec<usize>>::new();
    for (row_index, row) in right.rows.iter().enumerate() {
        check_cancellation(control)?;
        let indexes = equality.iter().map(|(_, right)| *right);
        if let Some(key) = join_key(row, indexes) {
            hash.entry(key).or_default().push(row_index);
        }
    }

    let mut rows = Vec::new();
    let mut output_payload_bytes = 0usize;
    for left_row in &left.rows {
        check_cancellation(control)?;
        let indexes = equality.iter().map(|(left, _)| *left);
        let matches = join_key(left_row, indexes).and_then(|key| hash.get(&key));
        if let Some(matches) = matches {
            for right_index in matches {
                check_cancellation(control)?;
                enforce_row_limit(
                    "hash join output",
                    rows.len().saturating_add(1),
                    limits.max_output_rows,
                )?;
                let row_bytes = estimated_join_row_bytes(
                    left_row,
                    Some(&right.rows[*right_index]),
                    right.columns.len(),
                );
                prepare_join_output_row(
                    &mut rows,
                    output_payload_bytes,
                    row_bytes,
                    limits.max_join_build_bytes,
                )?;
                let mut row = Vec::with_capacity(combined_columns.len());
                row.extend(left_row.iter().cloned());
                row.extend(right.rows[*right_index].iter().cloned());
                rows.push(row);
                output_payload_bytes = output_payload_bytes.saturating_add(row_bytes);
            }
        } else if join.kind == JoinKind::Left {
            check_cancellation(control)?;
            enforce_row_limit(
                "hash join output",
                rows.len().saturating_add(1),
                limits.max_output_rows,
            )?;
            let row_bytes = estimated_join_row_bytes(left_row, None, right.columns.len());
            prepare_join_output_row(
                &mut rows,
                output_payload_bytes,
                row_bytes,
                limits.max_join_build_bytes,
            )?;
            let mut row = Vec::with_capacity(combined_columns.len());
            row.extend(left_row.iter().cloned());
            row.resize(left_width + right.columns.len(), Value::Null);
            rows.push(row);
            output_payload_bytes = output_payload_bytes.saturating_add(row_bytes);
        }
    }
    right.rows.clear();
    Ok(Source {
        columns: combined_columns,
        rows,
    })
}

fn bind_join_keys(
    columns: &[BoundColumn],
    left_width: usize,
    equality: &[(ColumnRef, ColumnRef)],
) -> Result<Vec<(usize, usize)>> {
    let mut keys = Vec::with_capacity(equality.len());
    for (first, second) in equality {
        let first = resolve_column(columns, first)?;
        let second = resolve_column(columns, second)?;
        let (left, right) = match (first < left_width, second < left_width) {
            (true, false) => (first, second - left_width),
            (false, true) => (second, first - left_width),
            _ => {
                return Err(Error::Unsupported(
                    "each join equality must compare the new table with the left input".to_owned(),
                ));
            }
        };
        let left_type = columns[left].definition.data_type;
        let right_type = columns[left_width + right].definition.data_type;
        if left_type != right_type && !(left_type.is_numeric() && right_type.is_numeric()) {
            return Err(Error::Type {
                operation: "join equality".to_owned(),
                expected: left_type.to_string(),
                actual: right_type.to_string(),
            });
        }
        keys.push((left, right));
    }
    if keys.is_empty() {
        return Err(Error::Unsupported(
            "a hash join requires at least one equality predicate".to_owned(),
        ));
    }
    Ok(keys)
}

fn join_key(values: &[Value], indexes: impl Iterator<Item = usize>) -> Option<ValueKey> {
    let mut key = Vec::new();
    for index in indexes {
        key.push(key_part(&values[index], false)?);
    }
    Some(ValueKey(key))
}

fn partition_key(values: &[Value], indexes: &[usize]) -> ValueKey {
    ValueKey(
        indexes
            .iter()
            .map(|index| key_part(&values[*index], true).expect("partition keys retain NULL/NaN"))
            .collect(),
    )
}

fn key_part(value: &Value, partition: bool) -> Option<KeyPart> {
    match value {
        Value::Null if partition => Some(KeyPart::Null),
        Value::Null => None,
        Value::Int64(value) => Some(KeyPart::Int(*value)),
        Value::Float64(value) if value.is_nan() && !partition => None,
        Value::Float64(value) if value.is_nan() => Some(KeyPart::Float(f64::NAN.to_bits())),
        Value::Float64(value) if *value == 0.0 => Some(KeyPart::Int(0)),
        Value::Float64(value) if *value >= i64::MIN as f64 && *value < -(i64::MIN as f64) => {
            let integer = *value as i64;
            if compare_int_float(integer, *value) == Some(Ordering::Equal) {
                Some(KeyPart::Int(integer))
            } else {
                Some(KeyPart::Float(value.to_bits()))
            }
        }
        Value::Float64(value) => Some(KeyPart::Float(value.to_bits())),
        Value::Bool(value) => Some(KeyPart::Bool(*value)),
        Value::String(value) => Some(KeyPart::String(value.clone())),
    }
}

fn bind_predicates(
    columns: &[BoundColumn],
    predicates: &[Predicate],
) -> Result<Vec<(usize, Comparison, Value)>> {
    predicates
        .iter()
        .map(|predicate| {
            let index = resolve_column(columns, &predicate.column)?;
            let column = &columns[index].definition;
            if let Some(value_type) = predicate.value.data_type()
                && value_type != column.data_type
                && !(value_type.is_numeric() && column.data_type.is_numeric())
            {
                return Err(Error::TypeMismatch {
                    column: column.name.clone(),
                    expected: column.data_type.to_string(),
                    actual: predicate.value.type_name().to_owned(),
                });
            }
            Ok((index, predicate.comparison, predicate.value.clone()))
        })
        .collect()
}

fn filter_rows(
    rows: Vec<Vec<Value>>,
    predicates: &[(usize, Comparison, Value)],
    control: Option<ExecutionControl<'_>>,
) -> Result<Vec<Vec<Value>>> {
    let mut filtered = Vec::with_capacity(rows.len());
    for row in rows {
        check_cancellation(control)?;
        if predicates
            .iter()
            .all(|(column, comparison, value)| compare_predicate(&row[*column], value, *comparison))
        {
            filtered.push(row);
        }
    }
    Ok(filtered)
}

fn bind_projection(
    source: &[BoundColumn],
    projection: &[SelectItem],
) -> Result<(Vec<BoundProjection>, Vec<ColumnDef>)> {
    let mut bound = Vec::new();
    let mut columns = Vec::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(qualifier) => {
                let before = bound.len();
                for (index, column) in source.iter().enumerate() {
                    if qualifier
                        .as_ref()
                        .is_none_or(|value| value == &column.qualifier)
                    {
                        bound.push(BoundProjection::Source(index));
                        columns.push(column.definition.clone());
                    }
                }
                if qualifier.is_some() && before == bound.len() {
                    return Err(Error::ColumnNotFound(format!(
                        "{}.*",
                        qualifier.as_deref().unwrap_or_default()
                    )));
                }
            }
            SelectItem::Column { column, alias } => {
                let index = resolve_column(source, column)?;
                let mut definition = source[index].definition.clone();
                if let Some(alias) = alias {
                    definition.name = alias.clone();
                }
                bound.push(BoundProjection::Source(index));
                columns.push(definition);
            }
            SelectItem::Window { expression, alias } => {
                let window = bind_window(source, expression)?;
                let (name, data_type, nullable) = match window.function {
                    BoundWindowFunction::RowNumber => ("row_number", DataType::Int64, false),
                    BoundWindowFunction::Rank => ("rank", DataType::Int64, false),
                    BoundWindowFunction::Sum { data_type, .. } => ("sum", data_type, true),
                    BoundWindowFunction::Count(_) => ("count", DataType::Int64, false),
                };
                columns.push(ColumnDef {
                    name: alias.clone().unwrap_or_else(|| name.to_owned()),
                    data_type,
                    nullable,
                });
                bound.push(BoundProjection::Window(window));
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(column) = columns
        .iter()
        .find(|column| !names.insert(column.name.clone()))
    {
        return Err(Error::DuplicateColumn(column.name.clone()));
    }
    Ok((bound, columns))
}

fn bind_window(source: &[BoundColumn], expression: &WindowExpression) -> Result<BoundWindow> {
    let partition_by = expression
        .partition_by
        .iter()
        .map(|column| resolve_column(source, column))
        .collect::<Result<Vec<_>>>()?;
    let order_by = expression
        .order_by
        .iter()
        .map(|order| bind_source_order(source, order))
        .collect::<Result<Vec<_>>>()?;
    let function = match &expression.function {
        WindowFunction::RowNumber => BoundWindowFunction::RowNumber,
        WindowFunction::Rank => BoundWindowFunction::Rank,
        WindowFunction::Sum(column) => {
            let index = resolve_column(source, column)?;
            let data_type = source[index].definition.data_type;
            if !data_type.is_numeric() {
                return Err(Error::Type {
                    operation: "window SUM".to_owned(),
                    expected: "numeric".to_owned(),
                    actual: data_type.to_string(),
                });
            }
            BoundWindowFunction::Sum { index, data_type }
        }
        WindowFunction::Count(column) => BoundWindowFunction::Count(
            column
                .as_ref()
                .map(|column| resolve_column(source, column))
                .transpose()?,
        ),
    };
    let frame = expression.frame.unwrap_or(WindowFrame {
        start: FrameBound::UnboundedPreceding,
        end: if order_by.is_empty() {
            FrameBound::UnboundedFollowing
        } else {
            FrameBound::CurrentRow
        },
    });
    Ok(BoundWindow {
        function,
        partition_by,
        order_by,
        frame,
    })
}

fn bind_source_order(source: &[BoundColumn], order: &OrderBy) -> Result<BoundOrder> {
    Ok(BoundOrder {
        index: resolve_column(source, &order.column)?,
        descending: order.descending,
        nulls_first: order.nulls_first.unwrap_or(order.descending),
    })
}

fn bind_result_order(
    source: &[BoundColumn],
    output: &[ColumnDef],
    order_by: &[OrderBy],
) -> Result<Vec<BoundResultOrder>> {
    order_by
        .iter()
        .map(|order| {
            if order.column.qualifier.is_none()
                && let Some(index) = output
                    .iter()
                    .position(|column| column.name == order.column.name)
            {
                return Ok(BoundResultOrder::Output {
                    index,
                    descending: order.descending,
                    nulls_first: order.nulls_first.unwrap_or(order.descending),
                });
            }
            Ok(BoundResultOrder::Source(bind_source_order(source, order)?))
        })
        .collect()
}

fn evaluate_windows(
    source: &Source,
    projection: &[BoundProjection],
    limits: QueryLimits,
    control: Option<ExecutionControl<'_>>,
) -> Result<Vec<Option<Vec<Value>>>> {
    let has_windows = projection
        .iter()
        .any(|projection| matches!(projection, BoundProjection::Window(_)));
    let mut retained_bytes = if has_windows {
        projection
            .len()
            .saturating_mul(size_of::<Option<Vec<Value>>>())
    } else {
        0
    };
    enforce_byte_limit(
        "window partition",
        retained_bytes,
        limits.max_window_partition_bytes,
    )?;

    let mut values = Vec::with_capacity(projection.len());
    for projection in projection {
        match projection {
            BoundProjection::Source(_) => values.push(None),
            BoundProjection::Window(window) => {
                let output = evaluate_window(source, window, limits, retained_bytes, control)?;
                retained_bytes = retained_bytes
                    .saturating_add(output.capacity().saturating_mul(size_of::<Value>()));
                enforce_byte_limit(
                    "window partition",
                    retained_bytes,
                    limits.max_window_partition_bytes,
                )?;
                values.push(Some(output));
            }
        }
    }
    Ok(values)
}

fn evaluate_window(
    source: &Source,
    window: &BoundWindow,
    limits: QueryLimits,
    retained_output_bytes: usize,
    control: Option<ExecutionControl<'_>>,
) -> Result<Vec<Value>> {
    let mut partitions = HashMap::<ValueKey, PartitionState>::new();
    let mut map_bytes = 0usize;
    for row in &source.rows {
        check_cancellation(control)?;
        let key = partition_key(row, &window.partition_by);
        if let Some(state) = partitions.get_mut(&key) {
            let attempted = state.row_count.saturating_add(1);
            enforce_row_limit(
                "window partition",
                attempted,
                limits.max_window_partition_rows,
            )?;
            state.row_count = attempted;
        } else {
            enforce_row_limit("window partition", 1, limits.max_window_partition_rows)?;
            let required = retained_output_bytes
                .saturating_add(map_bytes)
                .saturating_add(estimated_partition_entry_bytes(&key));
            enforce_byte_limit(
                "window partition",
                required,
                limits.max_window_partition_bytes,
            )?;
            map_bytes = required.saturating_sub(retained_output_bytes);
            partitions.insert(
                key,
                PartitionState {
                    rows: Vec::new(),
                    row_count: 1,
                },
            );
        }
    }

    let index_bytes = source.rows.len().saturating_mul(size_of::<usize>());
    let output_bytes = source.rows.len().saturating_mul(size_of::<Value>());
    let preflight_bytes = retained_output_bytes
        .saturating_add(map_bytes)
        .saturating_add(index_bytes)
        .saturating_add(output_bytes);
    enforce_byte_limit(
        "window partition",
        preflight_bytes,
        limits.max_window_partition_bytes,
    )?;
    for state in partitions.values_mut() {
        state.rows.reserve_exact(state.row_count);
    }
    let actual_index_bytes = partitions.values().fold(0usize, |bytes, state| {
        bytes.saturating_add(state.rows.capacity().saturating_mul(size_of::<usize>()))
    });
    let reserved_bytes = retained_output_bytes
        .saturating_add(map_bytes)
        .saturating_add(actual_index_bytes)
        .saturating_add(output_bytes);
    enforce_byte_limit(
        "window partition",
        reserved_bytes,
        limits.max_window_partition_bytes,
    )?;
    for (row_index, row) in source.rows.iter().enumerate() {
        check_cancellation(control)?;
        let key = partition_key(row, &window.partition_by);
        partitions
            .get_mut(&key)
            .expect("counted window partition remains present")
            .rows
            .push(row_index);
    }

    let mut output = vec![Value::Null; source.rows.len()];
    let operator_bytes = retained_output_bytes
        .saturating_add(map_bytes)
        .saturating_add(actual_index_bytes)
        .saturating_add(output.capacity().saturating_mul(size_of::<Value>()));
    enforce_byte_limit(
        "window partition",
        operator_bytes,
        limits.max_window_partition_bytes,
    )?;
    for state in partitions.values_mut() {
        check_cancellation(control)?;
        state.rows.sort_unstable_by(|left, right| {
            compare_source_rows(&source.rows[*left], &source.rows[*right], &window.order_by)
                .then_with(|| left.cmp(right))
        });
        check_cancellation(control)?;
        let required = operator_bytes
            .saturating_add(window_temporary_bytes(&window.function, state.rows.len()));
        enforce_byte_limit(
            "window partition",
            required,
            limits.max_window_partition_bytes,
        )?;
        match window.function {
            BoundWindowFunction::RowNumber => {
                for (position, row) in state.rows.iter().enumerate() {
                    check_cancellation(control)?;
                    output[*row] = Value::Int64(position_to_i64(position)?);
                }
            }
            BoundWindowFunction::Rank => {
                let mut rank = 1usize;
                for position in 0..state.rows.len() {
                    check_cancellation(control)?;
                    if position > 0
                        && !source_rows_are_peers(
                            &source.rows[state.rows[position - 1]],
                            &source.rows[state.rows[position]],
                            &window.order_by,
                        )
                    {
                        rank = position + 1;
                    }
                    output[state.rows[position]] = Value::Int64(usize_to_i64(rank)?);
                }
            }
            BoundWindowFunction::Count(index) => evaluate_count(
                source,
                &state.rows,
                index,
                window.frame,
                &mut output,
                control,
            )?,
            BoundWindowFunction::Sum {
                index,
                data_type: DataType::Int64,
            } => evaluate_int_sum(
                source,
                &state.rows,
                index,
                window.frame,
                &mut output,
                control,
            )?,
            BoundWindowFunction::Sum {
                index,
                data_type: DataType::Float64,
            } => evaluate_float_sum(
                source,
                &state.rows,
                index,
                window.frame,
                &mut output,
                control,
            )?,
            BoundWindowFunction::Sum { .. } => unreachable!("SUM binding accepts numeric types"),
        }
    }
    Ok(output)
}

fn evaluate_count(
    source: &Source,
    rows: &[usize],
    index: Option<usize>,
    frame: WindowFrame,
    output: &mut [Value],
    control: Option<ExecutionControl<'_>>,
) -> Result<()> {
    let mut prefix = Vec::with_capacity(rows.len() + 1);
    prefix.push(0u64);
    for row in rows {
        check_cancellation(control)?;
        let included = index.is_none_or(|index| !source.rows[*row][index].is_null());
        prefix.push(prefix.last().copied().unwrap_or(0) + u64::from(included));
    }
    for (position, row) in rows.iter().enumerate() {
        check_cancellation(control)?;
        let count = frame_range(position, rows.len(), frame)
            .map_or(0, |(start, end)| prefix[end] - prefix[start]);
        output[*row] = Value::Int64(i64::try_from(count).map_err(|_| Error::Overflow {
            operation: "window COUNT".to_owned(),
        })?);
    }
    Ok(())
}

fn evaluate_int_sum(
    source: &Source,
    rows: &[usize],
    index: usize,
    frame: WindowFrame,
    output: &mut [Value],
    control: Option<ExecutionControl<'_>>,
) -> Result<()> {
    let mut sums = Vec::with_capacity(rows.len() + 1);
    let mut counts = Vec::with_capacity(rows.len() + 1);
    sums.push(0i128);
    counts.push(0usize);
    for row in rows {
        check_cancellation(control)?;
        let (value, included) = match source.rows[*row][index] {
            Value::Int64(value) => (i128::from(value), 1),
            Value::Null => (0, 0),
            _ => unreachable!("bound Int64 column contains Int64 or NULL"),
        };
        sums.push(sums.last().copied().unwrap_or(0) + value);
        counts.push(counts.last().copied().unwrap_or(0) + included);
    }
    for (position, row) in rows.iter().enumerate() {
        check_cancellation(control)?;
        output[*row] = match frame_range(position, rows.len(), frame) {
            Some((start, end)) if counts[end] > counts[start] => {
                let sum = sums[end] - sums[start];
                Value::Int64(i64::try_from(sum).map_err(|_| Error::Overflow {
                    operation: "window SUM".to_owned(),
                })?)
            }
            _ => Value::Null,
        };
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct FloatAccumulator {
    finite: f64,
    nan: usize,
    positive_infinity: usize,
    negative_infinity: usize,
    values: usize,
}

impl FloatAccumulator {
    fn add(&mut self, value: &Value) {
        match value {
            Value::Float64(value) if value.is_nan() => {
                self.nan += 1;
                self.values += 1;
            }
            Value::Float64(value) if *value == f64::INFINITY => {
                self.positive_infinity += 1;
                self.values += 1;
            }
            Value::Float64(value) if *value == f64::NEG_INFINITY => {
                self.negative_infinity += 1;
                self.values += 1;
            }
            Value::Float64(value) => {
                self.finite += value;
                self.values += 1;
            }
            Value::Null => {}
            _ => unreachable!("bound Float64 column contains Float64 or NULL"),
        }
    }

    fn finish(self) -> Value {
        if self.values == 0 {
            Value::Null
        } else if self.nan > 0 || (self.positive_infinity > 0 && self.negative_infinity > 0) {
            Value::Float64(f64::NAN)
        } else if self.positive_infinity > 0 {
            Value::Float64(f64::INFINITY)
        } else if self.negative_infinity > 0 {
            Value::Float64(f64::NEG_INFINITY)
        } else {
            Value::Float64(self.finite)
        }
    }
}

fn evaluate_float_sum(
    source: &Source,
    rows: &[usize],
    index: usize,
    frame: WindowFrame,
    output: &mut [Value],
    control: Option<ExecutionControl<'_>>,
) -> Result<()> {
    if frame.start == FrameBound::UnboundedPreceding {
        let mut accumulator = FloatAccumulator::default();
        let mut accumulated_end = 0usize;
        for (position, row) in rows.iter().enumerate() {
            check_cancellation(control)?;
            if let Some((_, end)) = frame_range(position, rows.len(), frame) {
                while accumulated_end < end {
                    check_cancellation(control)?;
                    accumulator.add(&source.rows[rows[accumulated_end]][index]);
                    accumulated_end += 1;
                }
                output[*row] = accumulator.finish();
            } else {
                output[*row] = Value::Null;
            }
        }
        return Ok(());
    }

    for (position, row) in rows.iter().enumerate() {
        check_cancellation(control)?;
        let mut accumulator = FloatAccumulator::default();
        if let Some((start, end)) = frame_range(position, rows.len(), frame) {
            for framed_row in &rows[start..end] {
                check_cancellation(control)?;
                accumulator.add(&source.rows[*framed_row][index]);
            }
        }
        output[*row] = accumulator.finish();
    }
    Ok(())
}

fn frame_range(position: usize, len: usize, frame: WindowFrame) -> Option<(usize, usize)> {
    let start = match frame.start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(offset) => position.saturating_sub(offset),
        FrameBound::CurrentRow => position,
        FrameBound::Following(offset) => position.saturating_add(offset).min(len),
        FrameBound::UnboundedFollowing => len,
    };
    let end = match frame.end {
        FrameBound::UnboundedFollowing => len,
        FrameBound::Following(offset) => position.saturating_add(offset).saturating_add(1).min(len),
        FrameBound::CurrentRow => position.saturating_add(1).min(len),
        FrameBound::Preceding(offset) if offset > position => 0,
        FrameBound::Preceding(offset) => position - offset + 1,
        FrameBound::UnboundedPreceding => 0,
    };
    (start < end).then_some((start, end))
}

fn project_rows(
    source: &Source,
    projection: &[BoundProjection],
    windows: &[Option<Vec<Value>>],
    row_indexes: &[usize],
    columns: &[ColumnDef],
    control: Option<ExecutionControl<'_>>,
) -> Result<Vec<Vec<Value>>> {
    let row_fixed_bytes =
        size_of::<Vec<Value>>().saturating_add(columns.len().saturating_mul(size_of::<Value>()));
    let mut retained_bytes = estimated_schema_bytes(columns)
        .saturating_add(row_indexes.len().saturating_mul(row_fixed_bytes));
    enforce_result_bytes(retained_bytes, control)?;
    let mut result = Vec::<Vec<Value>>::with_capacity(row_indexes.len());
    for row_index in row_indexes {
        check_cancellation(control)?;
        let source_row = &source.rows[*row_index];
        let values = projection
            .iter()
            .zip(windows)
            .map(|(projection, window)| match projection {
                BoundProjection::Source(index) => source_row[*index].clone(),
                BoundProjection::Window(_) => window
                    .as_ref()
                    .expect("window projection has evaluated values")[*row_index]
                    .clone(),
            })
            .collect::<Vec<_>>();
        retained_bytes = retained_bytes.saturating_add(variable_row_bytes(&values));
        enforce_result_bytes(retained_bytes, control)?;
        result.push(values);
    }
    Ok(result)
}

fn compare_result_rows(
    left: usize,
    right: usize,
    source: &Source,
    projection: &[BoundProjection],
    windows: &[Option<Vec<Value>>],
    order_by: &[BoundResultOrder],
) -> Ordering {
    for order in order_by {
        let ordering = match order {
            BoundResultOrder::Source(order) => compare_order_values(
                &source.rows[left][order.index],
                &source.rows[right][order.index],
                *order,
            ),
            BoundResultOrder::Output {
                index,
                descending,
                nulls_first,
            } => {
                let left_value = projected_value(source, projection, windows, left, *index);
                let right_value = projected_value(source, projection, windows, right, *index);
                compare_order_values(
                    left_value,
                    right_value,
                    BoundOrder {
                        index: *index,
                        descending: *descending,
                        nulls_first: *nulls_first,
                    },
                )
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn projected_value<'a>(
    source: &'a Source,
    projection: &[BoundProjection],
    windows: &'a [Option<Vec<Value>>],
    row: usize,
    column: usize,
) -> &'a Value {
    match projection[column] {
        BoundProjection::Source(index) => &source.rows[row][index],
        BoundProjection::Window(_) => &windows[column]
            .as_ref()
            .expect("window projection has evaluated values")[row],
    }
}

fn compare_source_rows(left: &[Value], right: &[Value], order_by: &[BoundOrder]) -> Ordering {
    for order in order_by {
        let ordering = compare_order_values(&left[order.index], &right[order.index], *order);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn source_rows_are_peers(left: &[Value], right: &[Value], order_by: &[BoundOrder]) -> bool {
    order_by.iter().all(|order| {
        compare_order_values(&left[order.index], &right[order.index], *order) == Ordering::Equal
    })
}

fn compare_order_values(left: &Value, right: &Value, order: BoundOrder) -> Ordering {
    let ordering = match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            return if order.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        (_, Value::Null) => {
            return if order.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        (Value::Int64(left), Value::Int64(right)) => left.cmp(right),
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(*left, *right)
            .unwrap_or_else(|| total_float_cmp(*left as f64, *right)),
        (Value::Float64(left), Value::Int64(right)) => compare_int_float(*right, *left)
            .map(Ordering::reverse)
            .unwrap_or_else(|| total_float_cmp(*left, *right as f64)),
        (Value::Float64(left), Value::Float64(right)) => total_float_cmp(*left, *right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => left.type_name().cmp(right.type_name()),
    };
    if order.descending {
        ordering.reverse()
    } else {
        ordering
    }
}

fn total_float_cmp(left: f64, right: f64) -> Ordering {
    if left == 0.0 && right == 0.0 {
        Ordering::Equal
    } else {
        left.total_cmp(&right)
    }
}

fn resolve_column(columns: &[BoundColumn], column: &ColumnRef) -> Result<usize> {
    let mut matches = columns.iter().enumerate().filter(|(_, candidate)| {
        candidate.definition.name == column.name
            && column
                .qualifier
                .as_ref()
                .is_none_or(|qualifier| qualifier == &candidate.qualifier)
    });
    let Some((index, _)) = matches.next() else {
        return Err(Error::ColumnNotFound(display_column(column)));
    };
    if matches.next().is_some() {
        return Err(Error::AmbiguousColumn(display_column(column)));
    }
    Ok(index)
}

fn display_column(column: &ColumnRef) -> String {
    column.qualifier.as_ref().map_or_else(
        || column.name.clone(),
        |qualifier| format!("{qualifier}.{}", column.name),
    )
}

fn compare_predicate(left: &Value, right: &Value, comparison: Comparison) -> bool {
    if left.is_null() || right.is_null() {
        return false;
    }
    let ordering = match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Some(left.cmp(right)),
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(*left, *right),
        (Value::Float64(left), Value::Int64(right)) => {
            compare_int_float(*right, *left).map(Ordering::reverse)
        }
        (Value::Float64(left), Value::Float64(right)) => left.partial_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => None,
    };
    match comparison {
        Comparison::Equal => ordering == Some(Ordering::Equal),
        Comparison::NotEqual => ordering != Some(Ordering::Equal),
        Comparison::Less => ordering == Some(Ordering::Less),
        Comparison::LessOrEqual => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        Comparison::Greater => ordering == Some(Ordering::Greater),
        Comparison::GreaterOrEqual => {
            matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
        }
    }
}

fn position_to_i64(position: usize) -> Result<i64> {
    usize_to_i64(position.saturating_add(1))
}

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Overflow {
        operation: "window row position".to_owned(),
    })
}

fn estimated_join_row_bytes(left: &[Value], right: Option<&[Value]>, right_width: usize) -> usize {
    left.len()
        .saturating_add(right_width)
        .saturating_mul(size_of::<Value>())
        .saturating_add(variable_row_bytes(left))
        .saturating_add(right.map_or(0, variable_row_bytes))
}

fn prepare_join_output_row(
    rows: &mut Vec<Vec<Value>>,
    payload_bytes: usize,
    row_bytes: usize,
    limit: usize,
) -> Result<()> {
    let needed_capacity = if rows.len() == rows.capacity() {
        rows.len().saturating_add(1)
    } else {
        rows.capacity()
    };
    let required = payload_bytes
        .saturating_add(row_bytes)
        .saturating_add(needed_capacity.saturating_mul(size_of::<Vec<Value>>()));
    enforce_byte_limit("hash join output", required, limit)?;
    if rows.len() == rows.capacity() {
        rows.reserve_exact(1);
        let retained = payload_bytes
            .saturating_add(row_bytes)
            .saturating_add(rows.capacity().saturating_mul(size_of::<Vec<Value>>()));
        enforce_byte_limit("hash join output", retained, limit)?;
    }
    Ok(())
}

fn estimated_partition_entry_bytes(key: &ValueKey) -> usize {
    let strings = key.0.iter().fold(0usize, |bytes, part| {
        bytes.saturating_add(match part {
            KeyPart::String(value) => value.capacity(),
            _ => 0,
        })
    });
    let entry = size_of::<ValueKey>()
        .saturating_add(size_of::<PartitionState>())
        .saturating_add(key.0.capacity().saturating_mul(size_of::<KeyPart>()))
        .saturating_add(strings);
    // Small hash tables retain several spare buckets; four entries per key is
    // a conservative bound across both small and normally loaded tables.
    entry.saturating_mul(4)
}

fn window_temporary_bytes(function: &BoundWindowFunction, rows: usize) -> usize {
    let entries = rows.saturating_add(1);
    let buffers = match function {
        BoundWindowFunction::Count(_) => entries.saturating_mul(size_of::<u64>()),
        BoundWindowFunction::Sum {
            data_type: DataType::Int64,
            ..
        } => entries.saturating_mul(size_of::<i128>().saturating_add(size_of::<usize>())),
        BoundWindowFunction::RowNumber
        | BoundWindowFunction::Rank
        | BoundWindowFunction::Sum {
            data_type: DataType::Float64,
            ..
        } => 0,
        BoundWindowFunction::Sum { .. } => 0,
    };
    // Prefix Vec allocations can retain allocator slack; charge twice the
    // requested buffer before allocation.
    buffers.saturating_mul(2)
}

fn variable_row_bytes(row: &[Value]) -> usize {
    row.iter().fold(0usize, |bytes, value| {
        bytes.saturating_add(match value {
            Value::String(value) => value.len(),
            _ => 0,
        })
    })
}

fn estimated_table_bytes(table: &Table, control: Option<ExecutionControl<'_>>) -> Result<usize> {
    let mut total = 0usize;
    for row in 0..table.row_count() {
        check_cancellation(control)?;
        // The build owns both row values and hash keys/bucket entries. Charging
        // twice the row size is conservative for fixed-width and string keys.
        total = total
            .saturating_add(table.estimated_row_bytes(row).saturating_mul(2))
            .saturating_add(size_of::<usize>().saturating_mul(2));
    }
    Ok(total)
}

fn enforce_row_limit(operator: &'static str, attempted: usize, limit: usize) -> Result<()> {
    if attempted > limit {
        Err(Error::ExecutionRowLimitExceeded {
            operator,
            limit,
            attempted,
        })
    } else {
        Ok(())
    }
}

fn enforce_byte_limit(operator: &'static str, required: usize, limit: usize) -> Result<()> {
    if required > limit {
        Err(Error::MemoryLimitExceeded {
            operator,
            required,
            limit,
        })
    } else {
        Ok(())
    }
}

fn estimated_schema_bytes(columns: &[ColumnDef]) -> usize {
    columns.iter().fold(
        columns.len().saturating_mul(size_of::<ColumnDef>()),
        |bytes, column| bytes.saturating_add(column.name.len()),
    )
}

fn enforce_result_bytes(required: usize, control: Option<ExecutionControl<'_>>) -> Result<()> {
    let Some(control) = control else {
        return Ok(());
    };
    if required > control.max_result_bytes {
        Err(Error::MemoryLimitExceeded {
            operator: "query result",
            required,
            limit: control.max_result_bytes,
        })
    } else {
        Ok(())
    }
}

fn check_cancellation(control: Option<ExecutionControl<'_>>) -> Result<()> {
    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        Err(Error::QueryCancelled)
    } else {
        Ok(())
    }
}
