//! SQL execution and structured query results.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
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
    worker_count: usize,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
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

    /// Creates an empty database that uses at most `worker_count` scan workers.
    ///
    /// A worker count of zero is rejected. The engine may use fewer workers
    /// when a query contains fewer fixed-size scan morsels than workers.
    pub fn with_worker_count(worker_count: usize) -> Result<Self> {
        validate_worker_count(worker_count)?;
        Ok(Self {
            catalog: Catalog::default(),
            worker_count,
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
                self.worker_count,
            )?;
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
            let mut matching_rows =
                scan_matching_rows(table, predicate.as_ref(), self.worker_count);
            order_source_rows(&mut matching_rows, table, &items, &ordering, select.limit);
            execute_projection(table, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

fn validate_worker_count(worker_count: usize) -> Result<()> {
    if worker_count == 0 {
        return Err(Error::InvalidConfiguration(
            "worker count must be at least 1".to_owned(),
        ));
    }
    Ok(())
}

fn scan_matching_rows(
    table: &Table,
    predicate: Option<&CompiledPredicate>,
    worker_count: usize,
) -> Vec<usize> {
    execute_morsels(table.row_count(), worker_count, |rows| {
        rows.filter(|row| predicate.is_none_or(|predicate| predicate.evaluate(table, *row)))
            .collect::<Vec<_>>()
    })
    .into_iter()
    .flatten()
    .collect()
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
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(active_workers);
        for _ in 0..active_workers {
            let next_morsel = &next_morsel;
            let execute = &execute;
            handles.push(scope.spawn(move || {
                let mut completed = Vec::new();
                loop {
                    let morsel = next_morsel.fetch_add(1, AtomicOrdering::Relaxed);
                    if morsel >= morsel_count {
                        break;
                    }
                    completed.push((morsel, execute(morsel_range(morsel, row_count))));
                }
                completed
            }));
        }

        let mut ordered = std::iter::repeat_with(|| None)
            .take(morsel_count)
            .collect::<Vec<_>>();
        for handle in handles {
            let completed = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            for (morsel, result) in completed {
                ordered[morsel] = Some(result);
            }
        }
        ordered
            .into_iter()
            .map(|result| result.expect("every morsel is completed by one worker"))
            .collect()
    })
}

fn morsel_range(morsel: usize, row_count: usize) -> Range<usize> {
    let start = morsel * SCAN_MORSEL_ROWS;
    start..(start + SCAN_MORSEL_ROWS).min(row_count)
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
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    worker_count: usize,
) -> Result<GroupedData<'a>> {
    let partials = execute_morsels(table.row_count(), worker_count, |rows| {
        aggregate_morsel(table, rows, predicate, group_columns, aggregate_specs)
    });

    merge_aggregate_morsels(
        group_columns.len(),
        table.row_count(),
        aggregate_specs,
        partials,
    )
}

fn aggregate_morsel<'a>(
    table: &'a Table,
    rows: Range<usize>,
    predicate: Option<&CompiledPredicate>,
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<PartialGroupedData<'a>> {
    let mut groups = GroupIndex::new(group_columns.len(), rows.len());
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = rows.len().min(1_024);
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

    for row in rows {
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

    Ok(PartialGroupedData {
        keys: groups.into_keys(group_count),
        aggregate_states,
    })
}

fn merge_aggregate_morsels<'a>(
    group_column_count: usize,
    row_count: usize,
    aggregate_specs: &[AggregateSpec],
    partials: Vec<Result<PartialGroupedData<'a>>>,
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_column_count, row_count);
    let mut group_count = usize::from(group_column_count == 0);
    let initial_capacity = row_count.min(1_024);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(initial_capacity);
            if group_column_count == 0 {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for partial in partials {
        let PartialGroupedData {
            keys,
            aggregate_states: partial_states,
        } = partial?;
        let mut merged_groups = Vec::with_capacity(keys.len());
        for key in &keys {
            let (group, inserted) = groups.find_or_insert_key(key, group_count);
            if inserted {
                group_count += 1;
                for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                    states.push(AggregateState::new(spec));
                }
            }
            merged_groups.push(group);
        }

        for (states, partial_states) in aggregate_states.iter_mut().zip(partial_states) {
            for (group, partial_state) in merged_groups.iter().copied().zip(partial_states) {
                states[group].merge(partial_state)?;
            }
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
struct PartialGroupedData<'a> {
    keys: Vec<GroupKey<'a>>,
    aggregate_states: Vec<Vec<AggregateState>>,
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

    fn find_or_insert_key(&mut self, key: &GroupKey<'a>, next_group: usize) -> (usize, bool) {
        match (self, key) {
            (Self::Global, GroupKey::Empty) => (0, false),
            (Self::One(groups), GroupKey::One(key)) => {
                if let Some(group) = groups.get(key) {
                    (*group, false)
                } else {
                    groups.insert(*key, next_group);
                    (next_group, true)
                }
            }
            (Self::Multiple(groups), GroupKey::Multiple(key)) => {
                find_or_insert_group(groups, key, next_group)
            }
            _ => unreachable!("group index and key shapes are resolved together"),
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

const FLOAT_FRACTION_BITS: usize = 52;
const FLOAT_FRACTION_MASK: u64 = (1_u64 << FLOAT_FRACTION_BITS) - 1;
const EXACT_FLOAT_MIN_POWER: i16 = -1_074;
const EXACT_FLOAT_LIMBS: usize = 34;
type FloatMagnitude = [u64; EXACT_FLOAT_LIMBS];

// Exact exponent bins keep partial sums mergeable without overflowing before cancellation.
#[derive(Debug)]
struct ExactFloatSum {
    bins: Vec<(i16, i128)>,
}

impl ExactFloatSum {
    fn new() -> Self {
        Self { bins: Vec::new() }
    }

    fn add(&mut self, value: f64) -> Option<()> {
        debug_assert!(value.is_finite());
        let bits = value.to_bits();
        let raw_exponent = ((bits >> FLOAT_FRACTION_BITS) & 0x7ff) as i16;
        let fraction = bits & FLOAT_FRACTION_MASK;
        let (significand, power) = if raw_exponent == 0 {
            if fraction == 0 {
                return Some(());
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
        )
    }

    fn merge(&mut self, partial: Self) -> Option<()> {
        for (power, coefficient) in partial.bins {
            self.add_bin(power, coefficient)?;
        }
        Some(())
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

    fn add_bin(&mut self, power: i16, coefficient: i128) -> Option<()> {
        match self.bins.binary_search_by_key(&power, |(power, _)| *power) {
            Ok(index) => {
                let combined = self.bins[index].1.checked_add(coefficient)?;
                if combined == 0 {
                    self.bins.remove(index);
                } else {
                    self.bins[index].1 = combined;
                }
            }
            Err(index) => self.bins.insert(index, (power, coefficient)),
        }
        Some(())
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
    Min(Option<Value>),
    Max(Option<Value>),
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
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                sum.add(values[row])
                    .ok_or_else(|| Error::NumericOverflow("SUM(Float64)".to_owned()))?;
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
                sum.add(values[row])
                    .ok_or_else(|| Error::NumericOverflow("AVG(Float64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
        }
        Ok(())
    }

    fn merge(&mut self, partial: Self) -> Result<()> {
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
            (Self::SumFloat(sum), Self::SumFloat(partial)) => {
                sum.merge(partial)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Float64)".to_owned()))?;
            }
            (Self::Min(current), Self::Min(partial)) => {
                if let Some(partial) = partial {
                    if current
                        .as_ref()
                        .is_none_or(|existing| partial.as_ref() < existing.as_ref())
                    {
                        *current = Some(partial);
                    }
                }
            }
            (Self::Max(current), Self::Max(partial)) => {
                if let Some(partial) = partial {
                    if current
                        .as_ref()
                        .is_none_or(|existing| partial.as_ref() > existing.as_ref())
                    {
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
                sum.merge(partial_sum)
                    .ok_or_else(|| Error::NumericOverflow("AVG(Float64) sum".to_owned()))?;
                *count = count
                    .checked_add(partial_count)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            _ => unreachable!("aggregate states for one specification have the same variant"),
        }
        Ok(())
    }

    fn finish(self) -> Result<Value> {
        match self {
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt(value) => i64::try_from(value)
                .map(Value::Int64)
                .map_err(|_| Error::NumericOverflow("SUM(Int64)".to_owned())),
            Self::SumFloat(value) => value
                .finish(1)
                .map(Value::Float64)
                .ok_or_else(|| Error::NumericOverflow("SUM(Float64)".to_owned())),
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
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

        let worker_error = database
            .execute("SELECT SUM(local_value) FROM overflow_data;")
            .expect_err("an unrepresentable morsel total is rejected");
        assert_eq!(
            worker_error,
            Error::NumericOverflow("SUM(Int64)".to_owned())
        );

        let merge_error = database
            .execute(
                "SELECT group_key, SUM(merge_value)
                 FROM overflow_data GROUP BY group_key;",
            )
            .expect_err("an unrepresentable cross-morsel total is rejected");
        assert_eq!(merge_error, Error::NumericOverflow("SUM(Int64)".to_owned()));
    }

    #[test]
    fn int_sum_allows_cancellation_across_morsel_boundaries() {
        let row_count = SCAN_MORSEL_ROWS * 2;
        let mut database = Database::with_worker_count(1).expect("valid worker count");
        database
            .execute("CREATE TABLE cancellation (value Int64);")
            .expect("create table");
        let table = database
            .catalog
            .table_mut("cancellation")
            .expect("table exists");
        for row in 0..row_count {
            let value = match row {
                0 => i64::MIN,
                value if value == SCAN_MORSEL_ROWS => i64::MAX,
                value if value == SCAN_MORSEL_ROWS + 1 => 1,
                _ => 0,
            };
            table
                .insert_row(vec![Value::Int64(value)])
                .expect("generated row is valid");
        }

        for worker_count in [1, 4] {
            database
                .set_worker_count(worker_count)
                .expect("valid worker count");
            assert_eq!(
                query(&mut database, "SELECT SUM(value) FROM cancellation;").rows,
                vec![vec![Value::Int64(0)]],
                "cross-morsel cancellation failed with {worker_count} workers"
            );
        }
    }

    #[test]
    fn float_sum_and_avg_allow_cancellation_across_morsel_boundaries() {
        let row_count = SCAN_MORSEL_ROWS + 2;
        let mut database = Database::with_worker_count(1).expect("valid worker count");
        database
            .execute("CREATE TABLE float_cancellation (value Float64);")
            .expect("create table");
        let table = database
            .catalog
            .table_mut("float_cancellation")
            .expect("table exists");
        for row in 0..row_count {
            let value = match row {
                0 => -1e308,
                value if value == SCAN_MORSEL_ROWS => 1e308,
                value if value == SCAN_MORSEL_ROWS + 1 => 1e308,
                _ => 0.0,
            };
            table
                .insert_row(vec![Value::Float64(value)])
                .expect("generated row is valid");
        }

        let expected = vec![vec![
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
                    "SELECT SUM(value), AVG(value) FROM float_cancellation;"
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
        large.add(f64::MAX).expect("finite value");
        large.add(f64::MAX).expect("finite value");
        assert_eq!(large.finish(2), Some(f64::MAX));

        let mut overflow = ExactFloatSum::new();
        overflow.add(f64::MAX).expect("finite value");
        overflow.add(f64::MAX).expect("finite value");
        assert_eq!(overflow.finish(1), None);

        let smallest = f64::from_bits(1);
        let mut rounds_to_even_zero = ExactFloatSum::new();
        rounds_to_even_zero.add(smallest).expect("finite value");
        assert_eq!(rounds_to_even_zero.finish(2), Some(0.0));

        let mut rounds_to_even_two = ExactFloatSum::new();
        for _ in 0..3 {
            rounds_to_even_two.add(smallest).expect("finite value");
        }
        assert_eq!(rounds_to_even_two.finish(2), Some(f64::from_bits(2)));
    }
}
