use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, JoinCondition, Operand,
    OrderBy, Predicate, Select, SelectItem, Statement,
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
        let left = self.catalog.table(&select.table)?;
        let relation = if let Some(join) = &select.join {
            let right = self.catalog.table(&join.table)?;
            Relation::joined(
                left,
                select.table_alias.as_deref(),
                right,
                join.table_alias.as_deref(),
                &join.conditions,
            )?
        } else {
            Relation::single(left, select.table_alias.as_deref())
        };
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(&relation, predicate))
            .transpose()?;

        let mut matching_rows = (0..relation.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(&relation, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(&relation, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(&relation, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&relation, &items, &result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped =
                execute_grouped(&relation, &matching_rows, &group_columns, &aggregate_specs)?;
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
            order_source_rows(
                &mut matching_rows,
                &relation,
                &items,
                &ordering,
                select.limit,
            );
            execute_projection(&relation, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelationSide {
    Left,
    Right,
}

#[derive(Debug)]
struct Relation<'a> {
    left: &'a Table,
    left_qualifier: String,
    right: Option<&'a Table>,
    right_qualifier: Option<String>,
    row_pairs: Option<Vec<(usize, usize)>>,
}

impl<'a> Relation<'a> {
    fn single(table: &'a Table, alias: Option<&str>) -> Self {
        Self {
            left: table,
            left_qualifier: alias.unwrap_or_else(|| table.name()).to_owned(),
            right: None,
            right_qualifier: None,
            row_pairs: None,
        }
    }

    fn joined(
        left: &'a Table,
        left_alias: Option<&str>,
        right: &'a Table,
        right_alias: Option<&str>,
        conditions: &[JoinCondition],
    ) -> Result<Self> {
        let left_qualifier = left_alias.unwrap_or_else(|| left.name()).to_owned();
        let right_qualifier = right_alias.unwrap_or_else(|| right.name()).to_owned();
        if left_qualifier.eq_ignore_ascii_case(&right_qualifier) {
            return Err(Error::InvalidQuery(format!(
                "INNER JOIN table names or aliases must be distinct; both resolve to '{left_qualifier}'"
            )));
        }

        let mut relation = Self {
            left,
            left_qualifier,
            right: Some(right),
            right_qualifier: Some(right_qualifier),
            row_pairs: Some(Vec::new()),
        };
        let mut left_keys = Vec::with_capacity(conditions.len());
        let mut right_keys = Vec::with_capacity(conditions.len());
        for condition in conditions {
            let first = relation.resolve_column(&condition.left)?;
            let second = relation.resolve_column(&condition.right)?;
            let first_side = relation.column_side(first);
            let second_side = relation.column_side(second);
            if first_side == second_side {
                return Err(Error::InvalidQuery(format!(
                    "INNER JOIN equality '{} = {}' must compare columns from opposite tables",
                    condition.left, condition.right
                )));
            }
            if relation.data_type(first) != relation.data_type(second) {
                return Err(Error::TypeMismatch {
                    context: format!(
                        "INNER JOIN equality '{} = {}'",
                        condition.left, condition.right
                    ),
                    expected: relation.data_type(first).to_string(),
                    actual: relation.data_type(second).to_string(),
                });
            }
            match first_side {
                RelationSide::Left => {
                    left_keys.push(first);
                    right_keys.push(second);
                }
                RelationSide::Right => {
                    left_keys.push(second);
                    right_keys.push(first);
                }
            }
        }

        let mut index: HashMap<Box<[ValueRef<'a>]>, Vec<usize>> =
            HashMap::with_capacity(right.row_count().min(1_024));
        for right_row in 0..right.row_count() {
            let key = relation.key_for_physical_row(&right_keys, right_row);
            index
                .entry(key.into_boxed_slice())
                .or_default()
                .push(right_row);
        }

        let mut pairs = Vec::new();
        for left_row in 0..left.row_count() {
            let key = relation.key_for_physical_row(&left_keys, left_row);
            if let Some(right_rows) = index.get(key.as_slice()) {
                pairs.extend(right_rows.iter().map(|right_row| (left_row, *right_row)));
            }
        }
        relation.row_pairs = Some(pairs);
        Ok(relation)
    }

    fn row_count(&self) -> usize {
        self.row_pairs
            .as_ref()
            .map_or_else(|| self.left.row_count(), Vec::len)
    }

    fn column_count(&self) -> usize {
        self.left.schema().len() + self.right.map_or(0, |table| table.schema().len())
    }

    fn column_side(&self, column: usize) -> RelationSide {
        if column < self.left.schema().len() {
            RelationSide::Left
        } else {
            RelationSide::Right
        }
    }

    fn column_name(&self, column: usize) -> &str {
        match self.column_side(column) {
            RelationSide::Left => &self.left.schema()[column].name,
            RelationSide::Right => {
                &self.right.expect("right column has a right table").schema()
                    [column - self.left.schema().len()]
                .name
            }
        }
    }

    fn data_type(&self, column: usize) -> DataType {
        match self.column_side(column) {
            RelationSide::Left => self.left.schema()[column].data_type,
            RelationSide::Right => {
                self.right.expect("right column has a right table").schema()
                    [column - self.left.schema().len()]
                .data_type
            }
        }
    }

    fn resolve_column(&self, reference: &str) -> Result<usize> {
        let (qualifier, name) = reference
            .split_once('.')
            .map_or((None, reference), |(qualifier, name)| {
                (Some(qualifier), name)
            });
        if let Some(qualifier) = qualifier {
            let side = if qualifier.eq_ignore_ascii_case(&self.left_qualifier) {
                RelationSide::Left
            } else if self
                .right_qualifier
                .as_ref()
                .is_some_and(|right| qualifier.eq_ignore_ascii_case(right))
            {
                RelationSide::Right
            } else {
                return Err(Error::InvalidQuery(format!(
                    "unknown table or alias '{qualifier}' in column reference '{reference}'"
                )));
            };
            return self
                .resolve_on_side(side, name)
                .ok_or_else(|| Error::ColumnNotFound {
                    table: qualifier.to_owned(),
                    column: name.to_owned(),
                });
        }

        let left = self.resolve_on_side(RelationSide::Left, name);
        let right = self.resolve_on_side(RelationSide::Right, name);
        match (left, right) {
            (Some(column), None) | (None, Some(column)) => Ok(column),
            (Some(_), Some(_)) => Err(Error::InvalidQuery(format!(
                "column reference '{name}' is ambiguous; qualify it with a table name or alias"
            ))),
            (None, None) => Err(Error::ColumnNotFound {
                table: self.left.name().to_owned(),
                column: name.to_owned(),
            }),
        }
    }

    fn resolve_on_side(&self, side: RelationSide, name: &str) -> Option<usize> {
        let (table, offset) = match side {
            RelationSide::Left => (self.left, 0),
            RelationSide::Right => (self.right?, self.left.schema().len()),
        };
        table
            .schema()
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .map(|column| offset + column)
    }

    fn columns_for_qualifier(&self, qualifier: &str) -> Result<std::ops::Range<usize>> {
        if qualifier.eq_ignore_ascii_case(&self.left_qualifier) {
            Ok(0..self.left.schema().len())
        } else if self
            .right_qualifier
            .as_ref()
            .is_some_and(|right| qualifier.eq_ignore_ascii_case(right))
        {
            Ok(self.left.schema().len()..self.column_count())
        } else {
            Err(Error::InvalidQuery(format!(
                "unknown table or alias '{qualifier}' in qualified wildcard"
            )))
        }
    }

    fn key_for_physical_row(&self, columns: &[usize], row: usize) -> Vec<ValueRef<'a>> {
        columns
            .iter()
            .map(|column| self.physical_value_ref(*column, row))
            .collect()
    }

    fn physical_value_ref(&self, column: usize, row: usize) -> ValueRef<'a> {
        match self.column_side(column) {
            RelationSide::Left => self.left.columns()[column].value_ref(row),
            RelationSide::Right => self
                .right
                .expect("right column has a right table")
                .columns()[column - self.left.schema().len()]
            .value_ref(row),
        }
    }

    fn value_ref(&self, column: usize, row: usize) -> ValueRef<'a> {
        let physical_row = match (self.column_side(column), &self.row_pairs) {
            (RelationSide::Left, Some(pairs)) => pairs[row].0,
            (RelationSide::Right, Some(pairs)) => pairs[row].1,
            (RelationSide::Left, None) => row,
            (RelationSide::Right, None) => unreachable!("a right column belongs to a join"),
        };
        self.physical_value_ref(column, physical_row)
    }

    fn cmp_at(&self, column: usize, left: usize, right: usize) -> Ordering {
        self.value_ref(column, left)
            .cmp(&self.value_ref(column, right))
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

fn resolve_group_columns(relation: &Relation<'_>, names: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = relation.resolve_column(name)?;
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
    relation: &Relation<'_>,
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
                for source in 0..relation.column_count() {
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            relation.column_name(source)
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: relation.column_name(source).to_owned(),
                        data_type: relation.data_type(source),
                    });
                }
            }
            SelectItem::QualifiedWildcard { qualifier } => {
                for source in relation.columns_for_qualifier(qualifier)? {
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}.{}' must appear in GROUP BY",
                            qualifier,
                            relation.column_name(source)
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: relation.column_name(source).to_owned(),
                        data_type: relation.data_type(source),
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = relation.resolve_column(name)?;
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
                        .unwrap_or_else(|| relation.column_name(source).to_owned()),
                    data_type: relation.data_type(source),
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
                        let index = relation.resolve_column(name)?;
                        (
                            Some(index),
                            Some(relation.data_type(index)),
                            relation.column_name(index).to_owned(),
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
    relation: &Relation<'_>,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source, .. } => {
                        relation.value_ref(*source, *row).to_owned()
                    }
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect()
        })
        .collect()
}

fn execute_grouped<'a>(
    relation: &Relation<'a>,
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
        let (group, inserted) = groups.find_or_insert(relation, group_columns, *row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, relation, *row)?;
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
        relation: &Relation<'a>,
        columns: &[usize],
        row: usize,
        next_group: usize,
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = relation.value_ref(columns[0], row);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    relation.value_ref(columns[0], row),
                    relation.value_ref(columns[1], row),
                ];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| relation.value_ref(*column, row))
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

    fn update(&mut self, spec: &AggregateSpec, relation: &Relation<'_>, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let ValueRef::Int64(value) =
                    relation.value_ref(spec.argument.expect("SUM argument"), row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let ValueRef::Float64(value) =
                    relation.value_ref(spec.argument.expect("SUM argument"), row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = relation.value_ref(spec.argument.expect("MIN argument"), row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = relation.value_ref(spec.argument.expect("MAX argument"), row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let ValueRef::Int64(value) =
                    relation.value_ref(spec.argument.expect("AVG argument"), row)
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
                    relation.value_ref(spec.argument.expect("AVG argument"), row)
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
    relation: &Relation<'_>,
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let matches = if order.name.contains('.') {
            let source = relation.resolve_column(&order.name)?;
            items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    ResolvedItem::Column {
                        source: item_source,
                        ..
                    } if *item_source == source => Some(index),
                    _ => None,
                })
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
    relation: &Relation<'_>,
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
            let comparison = relation.cmp_at(source, left, right);
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
    fn evaluate(&self, relation: &Relation<'_>, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(relation, row);
                let right = right.value(relation, row);
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
            Self::And(left, right) => left.evaluate(relation, row) && right.evaluate(relation, row),
            Self::Or(left, right) => left.evaluate(relation, row) || right.evaluate(relation, row),
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

    fn value<'a>(&'a self, relation: &'a Relation<'_>, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => relation.value_ref(*index, row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(relation: &Relation<'_>, predicate: &Predicate) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(relation, left)?;
            let right = compile_operand(relation, right)?;
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
            Box::new(compile_predicate(relation, left)?),
            Box::new(compile_predicate(relation, right)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(relation, left)?),
            Box::new(compile_predicate(relation, right)?),
        )),
    }
}

fn compile_operand(relation: &Relation<'_>, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = relation.resolve_column(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: relation.data_type(index),
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
