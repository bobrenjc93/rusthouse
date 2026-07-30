use std::cmp::Ordering;
use std::collections::HashMap;
use std::mem::size_of;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, AsofJoin, ComparisonOperator, Operand, OrderBy,
    Predicate, Select, SelectItem, Statement,
};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

pub const DEFAULT_ASOF_MAX_ROWS: usize = 1_000_000;
pub const DEFAULT_ASOF_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_ASOF_MAX_CANDIDATE_COMPARISONS: usize = 20_000_000;

/// Per-operator bounds for ASOF index construction and lookup work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsofJoinLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_candidate_comparisons: usize,
}

impl Default for AsofJoinLimits {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_ASOF_MAX_ROWS,
            max_bytes: DEFAULT_ASOF_MAX_BYTES,
            max_candidate_comparisons: DEFAULT_ASOF_MAX_CANDIDATE_COMPARISONS,
        }
    }
}

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
    asof_join_limits: AsofJoinLimits,
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
    pub fn with_asof_join_limits(asof_join_limits: AsofJoinLimits) -> Self {
        Self {
            catalog: Catalog::new(),
            asof_join_limits,
        }
    }

    #[must_use]
    pub fn asof_join_limits(&self) -> AsofJoinLimits {
        self.asof_join_limits
    }

    pub fn set_asof_join_limits(&mut self, limits: AsofJoinLimits) {
        self.asof_join_limits = limits;
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
                let rows = {
                    let target = self.catalog.table(&table)?;
                    rows.into_iter()
                        .map(|row| target.coerce_row(row))
                        .collect::<Result<Vec<_>>>()?
                };
                let target = self.catalog.table_mut(&table)?;
                for row in rows {
                    target.insert_row(row)?;
                }
                Ok(StatementResult::Command {
                    tag: "INSERT",
                    affected_rows,
                })
            }
            Statement::Select(select) => self.execute_select(*select).map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: Select) -> Result<QueryResult> {
        let input = QueryInput::build(
            &self.catalog,
            &select.table,
            select.table_alias.as_deref(),
            select.join.as_ref(),
            self.asof_join_limits,
        )?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(&input, predicate))
            .transpose()?;

        let mut matching_rows = (0..input.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(&input, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(&input, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(&input, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&input, &items, &result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped =
                execute_grouped(&input, &matching_rows, &group_columns, &aggregate_specs)?;
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
            order_source_rows(&mut matching_rows, &input, &items, &ordering, select.limit);
            execute_projection(&input, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug)]
struct Relation<'a> {
    qualifier: String,
    table: &'a Table,
}

#[derive(Debug, Clone, Copy)]
struct InputColumn<'a> {
    relation: usize,
    physical: usize,
    name: &'a str,
    data_type: DataType,
}

#[derive(Debug)]
struct QueryInput<'a> {
    relations: Vec<Relation<'a>>,
    columns: Vec<InputColumn<'a>>,
    matched_right: Option<Vec<Option<usize>>>,
    row_count: usize,
}

#[derive(Debug, Clone, Copy)]
enum AsofDirection {
    Backward { inclusive: bool },
    Forward { inclusive: bool },
}

impl<'a> QueryInput<'a> {
    fn build(
        catalog: &'a Catalog,
        table_name: &str,
        alias: Option<&str>,
        join: Option<&AsofJoin>,
        limits: AsofJoinLimits,
    ) -> Result<Self> {
        let table = catalog.table(table_name)?;
        let mut input = Self {
            relations: Vec::new(),
            columns: Vec::new(),
            matched_right: None,
            row_count: table.row_count(),
        };
        input.push_relation(table, alias.unwrap_or(table_name))?;
        if let Some(join) = join {
            input.apply_asof_join(catalog, join, limits)?;
        }
        Ok(input)
    }

    fn push_relation(&mut self, table: &'a Table, qualifier: &str) -> Result<()> {
        if self
            .relations
            .iter()
            .any(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
        {
            return Err(Error::InvalidQuery(format!(
                "table name or alias '{qualifier}' is specified more than once"
            )));
        }
        let relation = self.relations.len();
        self.relations.push(Relation {
            qualifier: qualifier.to_owned(),
            table,
        });
        self.columns
            .extend(
                table
                    .schema()
                    .iter()
                    .enumerate()
                    .map(|(physical, field)| InputColumn {
                        relation,
                        physical,
                        name: &field.name,
                        data_type: field.data_type,
                    }),
            );
        Ok(())
    }

    fn row_count(&self) -> usize {
        self.row_count
    }

    fn column_index(&self, name: &str) -> Result<usize> {
        if let Some((qualifier, column_name)) = name.split_once('.') {
            let relation = self
                .relations
                .iter()
                .position(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
                .ok_or_else(|| {
                    Error::InvalidQuery(format!("unknown table name or alias '{qualifier}'"))
                })?;
            return self
                .columns
                .iter()
                .position(|column| {
                    column.relation == relation && column.name.eq_ignore_ascii_case(column_name)
                })
                .ok_or_else(|| Error::ColumnNotFound {
                    table: self.relations[relation].table.name().to_owned(),
                    column: column_name.to_owned(),
                });
        }
        let matches = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(name))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] if self.relations.len() == 1 => Err(Error::ColumnNotFound {
                table: self.relations[0].table.name().to_owned(),
                column: name.to_owned(),
            }),
            [] => Err(Error::InvalidQuery(format!(
                "column '{name}' does not exist in either ASOF input"
            ))),
            _ => Err(Error::InvalidQuery(format!(
                "column reference '{name}' is ambiguous; qualify it with a table name or alias"
            ))),
        }
    }

    fn qualifier_index(&self, qualifier: &str) -> Result<usize> {
        self.relations
            .iter()
            .position(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
            .ok_or_else(|| {
                Error::InvalidQuery(format!("unknown table name or alias '{qualifier}'"))
            })
    }

    fn value(&self, column: usize, row: usize) -> ValueRef<'_> {
        let column = self.columns[column];
        let physical_row = if column.relation == 0 {
            Some(row)
        } else {
            self.matched_right.as_ref().expect("joined input")[row]
        };
        physical_row.map_or(ValueRef::Null, |physical_row| {
            self.relations[column.relation].table.columns()[column.physical].value_ref(physical_row)
        })
    }

    fn physical_value(&self, column: usize, row: usize) -> ValueRef<'a> {
        let column = self.columns[column];
        self.relations[column.relation].table.columns()[column.physical].value_ref(row)
    }

    fn cmp_at(&self, column: usize, left: usize, right: usize) -> Ordering {
        self.value(column, left).cmp(&self.value(column, right))
    }

    fn apply_asof_join(
        &mut self,
        catalog: &'a Catalog,
        join: &AsofJoin,
        limits: AsofJoinLimits,
    ) -> Result<()> {
        let right = catalog.table(&join.table)?;
        self.push_relation(right, join.alias.as_deref().unwrap_or(&join.table))?;
        if self.row_count > limits.max_rows {
            return Err(asof_limit("output rows", limits.max_rows, self.row_count));
        }
        if right.row_count() > limits.max_rows {
            return Err(asof_limit(
                "indexed rows",
                limits.max_rows,
                right.row_count(),
            ));
        }

        let mut equality_keys = Vec::new();
        let mut ordered = None;
        for condition in &join.conditions {
            let Operand::Column(left_name) = &condition.left else {
                return Err(Error::InvalidQuery(
                    "ASOF LEFT JOIN ON conditions must compare columns".to_owned(),
                ));
            };
            let Operand::Column(right_name) = &condition.right else {
                return Err(Error::InvalidQuery(
                    "ASOF LEFT JOIN ON conditions must compare columns".to_owned(),
                ));
            };
            let first = self.column_index(left_name)?;
            let second = self.column_index(right_name)?;
            let (left_column, right_column, operator) =
                match (self.columns[first].relation, self.columns[second].relation) {
                    (0, 1) => (first, second, condition.operator),
                    (1, 0) => (second, first, reverse_operator(condition.operator)),
                    _ => {
                        return Err(Error::InvalidQuery(
                            "each ASOF LEFT JOIN condition must connect the left and right tables"
                                .to_owned(),
                        ));
                    }
                };
            let left_type = self.columns[left_column].data_type;
            let right_type = self.columns[right_column].data_type;
            if left_type != right_type {
                return Err(Error::TypeMismatch {
                    context: "ASOF LEFT JOIN keys".to_owned(),
                    expected: left_type.to_string(),
                    actual: right_type.to_string(),
                });
            }
            match operator {
                ComparisonOperator::Equal => equality_keys.push((left_column, right_column)),
                ComparisonOperator::Less
                | ComparisonOperator::LessOrEqual
                | ComparisonOperator::Greater
                | ComparisonOperator::GreaterOrEqual => {
                    if ordered.is_some() {
                        return Err(Error::InvalidQuery(
                            "ASOF LEFT JOIN requires exactly one ordered inequality".to_owned(),
                        ));
                    }
                    if !matches!(
                        left_type,
                        DataType::Int64 | DataType::Date | DataType::DateTime64
                    ) {
                        return Err(Error::TypeMismatch {
                            context: "ASOF LEFT JOIN ordered key".to_owned(),
                            expected: "Int64, Date, or DateTime64(3)".to_owned(),
                            actual: left_type.to_string(),
                        });
                    }
                    let direction = match operator {
                        ComparisonOperator::Greater => AsofDirection::Backward { inclusive: false },
                        ComparisonOperator::GreaterOrEqual => {
                            AsofDirection::Backward { inclusive: true }
                        }
                        ComparisonOperator::Less => AsofDirection::Forward { inclusive: false },
                        ComparisonOperator::LessOrEqual => {
                            AsofDirection::Forward { inclusive: true }
                        }
                        _ => unreachable!(),
                    };
                    ordered = Some((left_column, right_column, direction));
                }
                ComparisonOperator::NotEqual => {
                    return Err(Error::InvalidQuery(
                        "ASOF LEFT JOIN ON supports equality keys and one ordered inequality"
                            .to_owned(),
                    ));
                }
            }
        }
        let Some((left_ordered, right_ordered, direction)) = ordered else {
            return Err(Error::InvalidQuery(
                "ASOF LEFT JOIN requires exactly one ordered inequality".to_owned(),
            ));
        };

        let estimated_bytes =
            estimate_asof_bytes(self.row_count, right.row_count(), equality_keys.len());
        if estimated_bytes > limits.max_bytes {
            return Err(asof_limit("index bytes", limits.max_bytes, estimated_bytes));
        }

        let mut indexes: HashMap<Box<[ValueRef<'a>]>, Vec<usize>> = HashMap::new();
        for right_row in 0..right.row_count() {
            let key = equality_keys
                .iter()
                .map(|(_, right_column)| self.physical_value(*right_column, right_row))
                .collect::<Vec<_>>()
                .into_boxed_slice();
            indexes.entry(key).or_default().push(right_row);
        }
        for rows in indexes.values_mut() {
            rows.sort_unstable_by(|left, right| {
                self.physical_value(right_ordered, *left)
                    .cmp(&self.physical_value(right_ordered, *right))
                    .then_with(|| left.cmp(right))
            });
        }

        let mut candidate_comparisons = 0;
        let mut matched = Vec::with_capacity(self.row_count);
        for left_row in 0..self.row_count {
            let key = equality_keys
                .iter()
                .map(|(left_column, _)| self.physical_value(*left_column, left_row))
                .collect::<Vec<_>>();
            let candidate = indexes.get(key.as_slice()).map_or(Ok(None), |rows| {
                find_asof_candidate(
                    self,
                    rows,
                    left_ordered,
                    right_ordered,
                    left_row,
                    direction,
                    &mut candidate_comparisons,
                    limits.max_candidate_comparisons,
                )
            })?;
            matched.push(candidate);
        }
        self.matched_right = Some(matched);
        Ok(())
    }
}

fn reverse_operator(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::Equal,
        ComparisonOperator::NotEqual => ComparisonOperator::NotEqual,
        ComparisonOperator::Less => ComparisonOperator::Greater,
        ComparisonOperator::LessOrEqual => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::Greater => ComparisonOperator::Less,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::LessOrEqual,
    }
}

fn estimate_asof_bytes(left_rows: usize, right_rows: usize, equality_keys: usize) -> usize {
    // Treat every row as a distinct equality group. This deliberately
    // overestimates shared Vec/HashMap overhead but never understates it.
    let per_right = size_of::<usize>()
        .saturating_mul(12)
        .saturating_add(size_of::<ValueRef<'_>>().saturating_mul(equality_keys));
    left_rows
        .saturating_mul(size_of::<Option<usize>>())
        .saturating_add(right_rows.saturating_mul(per_right))
        .saturating_add(size_of::<ValueRef<'_>>().saturating_mul(equality_keys))
}

fn asof_limit(resource: &'static str, limit: usize, actual: usize) -> Error {
    Error::AsofJoinLimitExceeded {
        resource,
        limit,
        actual,
    }
}

#[allow(clippy::too_many_arguments)]
fn find_asof_candidate(
    input: &QueryInput<'_>,
    rows: &[usize],
    left_column: usize,
    right_column: usize,
    left_row: usize,
    direction: AsofDirection,
    comparisons: &mut usize,
    max_comparisons: usize,
) -> Result<Option<usize>> {
    let target = input.physical_value(left_column, left_row);
    let boundary = |accept_equal: bool, comparisons: &mut usize| -> Result<usize> {
        let mut low = 0;
        let mut high = rows.len();
        while low < high {
            *comparisons = comparisons.saturating_add(1);
            if *comparisons > max_comparisons {
                return Err(asof_limit(
                    "candidate comparisons",
                    max_comparisons,
                    *comparisons,
                ));
            }
            let middle = low + (high - low) / 2;
            let comparison = input
                .physical_value(right_column, rows[middle])
                .cmp(&target);
            if comparison == Ordering::Less || (accept_equal && comparison == Ordering::Equal) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    };

    match direction {
        AsofDirection::Backward { inclusive } => {
            let end = boundary(inclusive, comparisons)?;
            Ok(end.checked_sub(1).map(|index| rows[index]))
        }
        AsofDirection::Forward { inclusive } => {
            let start = boundary(!inclusive, comparisons)?;
            let Some(first) = rows.get(start).copied() else {
                return Ok(None);
            };
            let selected_key = input.physical_value(right_column, first);
            let mut low = start + 1;
            let mut high = rows.len();
            while low < high {
                *comparisons = comparisons.saturating_add(1);
                if *comparisons > max_comparisons {
                    return Err(asof_limit(
                        "candidate comparisons",
                        max_comparisons,
                        *comparisons,
                    ));
                }
                let middle = low + (high - low) / 2;
                if input.physical_value(right_column, rows[middle]) == selected_key {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            Ok(Some(rows[low - 1]))
        }
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

fn resolve_group_columns(input: &QueryInput<'_>, names: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = input.column_index(name)?;
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
    input: &QueryInput<'_>,
    requested: &[SelectItem],
    group_columns: &[usize],
) -> Result<(Vec<ResolvedItem>, Vec<ResultColumn>, Vec<AggregateSpec>)> {
    let has_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    if has_aggregate
        && requested.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard | SelectItem::QualifiedWildcard { .. }
            )
        })
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
                for source in 0..input.columns.len() {
                    let field = &input.columns[source];
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
                        name: field.name.to_owned(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::QualifiedWildcard { qualifier } => {
                let relation = input.qualifier_index(qualifier)?;
                for source in 0..input.columns.len() {
                    let field = &input.columns[source];
                    if field.relation != relation {
                        continue;
                    }
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
                        name: field.name.to_owned(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = input.column_index(name)?;
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
                        .unwrap_or_else(|| input.columns[source].name.to_owned()),
                    data_type: input.columns[source].data_type,
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
                        let index = input.column_index(name)?;
                        (
                            Some(index),
                            Some(input.columns[index].data_type),
                            input.columns[index].name.to_owned(),
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
    input: &QueryInput<'_>,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source, .. } => input.value(*source, *row).to_owned(),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect()
        })
        .collect()
}

fn execute_grouped<'a>(
    input: &'a QueryInput<'_>,
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
        let (group, inserted) = groups.find_or_insert(input, group_columns, *row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, input, *row)?;
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
        input: &'a QueryInput<'_>,
        columns: &[usize],
        row: usize,
        next_group: usize,
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = input.value(columns[0], row);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [input.value(columns[0], row), input.value(columns[1], row)];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| input.value(*column, row))
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

    fn update(&mut self, spec: &AggregateSpec, input: &QueryInput<'_>, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                if spec
                    .argument
                    .is_some_and(|column| matches!(input.value(column, row), ValueRef::Null))
                {
                    return Ok(());
                }
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => match input.value(spec.argument.expect("SUM argument"), row) {
                ValueRef::Int64(value) => {
                    *sum = sum
                        .checked_add(value)
                        .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
                }
                ValueRef::Null => {}
                _ => unreachable!("SUM input type is resolved"),
            },
            Self::SumFloat(sum) => {
                match input.value(spec.argument.expect("SUM argument"), row) {
                    ValueRef::Float64(value) => *sum += value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("SUM input type is resolved"),
                }
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = input.value(spec.argument.expect("MIN argument"), row);
                if matches!(candidate, ValueRef::Null) {
                    return Ok(());
                }
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = input.value(spec.argument.expect("MAX argument"), row);
                if matches!(candidate, ValueRef::Null) {
                    return Ok(());
                }
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let value = match input.value(spec.argument.expect("AVG argument"), row) {
                    ValueRef::Int64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("AVG input type is resolved"),
                };
                *sum = sum
                    .checked_add(i128::from(value))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let value = match input.value(spec.argument.expect("AVG argument"), row) {
                    ValueRef::Float64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("AVG input type is resolved"),
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

fn resolve_ordering(
    input: &QueryInput<'_>,
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let matches = if order.name.contains('.') {
            let source = input.column_index(&order.name)?;
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches!(item, ResolvedItem::Column { source: item_source, .. } if *item_source == source)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        } else {
            columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
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
    input: &QueryInput<'_>,
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
            let comparison = input.cmp_at(source, left, right);
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
    fn evaluate(&self, input: &QueryInput<'_>, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(input, row);
                let right = right.value(input, row);
                let Some(comparison) = left.sql_cmp(right) else {
                    return false;
                };
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            Self::And(left, right) => left.evaluate(input, row) && right.evaluate(input, row),
            Self::Or(left, right) => left.evaluate(input, row) || right.evaluate(input, row),
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
            Self::Literal(value) => value.data_type().expect("SQL literal has a concrete type"),
        }
    }

    fn value<'a>(&'a self, input: &'a QueryInput<'_>, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => input.value(*index, row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(input: &QueryInput<'_>, predicate: &Predicate) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(input, left)?;
            let right = compile_operand(input, right)?;
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
            Box::new(compile_predicate(input, left)?),
            Box::new(compile_predicate(input, right)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(input, left)?),
            Box::new(compile_predicate(input, right)?),
        )),
    }
}

fn compile_operand(input: &QueryInput<'_>, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = input.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: input.columns[index].data_type,
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
