use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ColumnReference, ComparisonOperator, InnerJoin,
    Operand, OrderBy, Predicate, Select, SelectItem, Statement, TableReference,
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
            Statement::Select(select) => self.execute_select(*select).map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: Select) -> Result<QueryResult> {
        let source = build_query_source(&self.catalog, &select.from, select.join.as_ref())?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(&source, predicate))
            .transpose()?;

        let mut matching_rows = (0..source.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(&source, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(&source, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(&source, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&source, &items, &result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped =
                execute_grouped(&source, &matching_rows, &group_columns, &aggregate_specs)?;
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
            order_source_rows(&mut matching_rows, &source, &items, &ordering, select.limit);
            execute_projection(&source, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug)]
struct SourceColumn<'a> {
    name: &'a str,
    data_type: DataType,
    column: &'a Column,
    relation: usize,
}

#[derive(Debug)]
struct SourceRelation<'a> {
    qualifier: String,
    table_name: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct SourceRow {
    left: usize,
    right: usize,
}

#[derive(Debug)]
enum SourceRows {
    Identity { len: usize },
    Joined(Vec<SourceRow>),
}

#[derive(Debug)]
struct QuerySource<'a> {
    columns: Vec<SourceColumn<'a>>,
    relations: Vec<SourceRelation<'a>>,
    rows: SourceRows,
}

impl<'a> QuerySource<'a> {
    fn row_count(&self) -> usize {
        match &self.rows {
            SourceRows::Identity { len } => *len,
            SourceRows::Joined(rows) => rows.len(),
        }
    }

    fn column_index(&self, reference: &ColumnReference) -> Result<usize> {
        let matches = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| {
                column.name.eq_ignore_ascii_case(&reference.name)
                    && reference.qualifier.as_ref().is_none_or(|qualifier| {
                        self.relations[column.relation]
                            .qualifier
                            .eq_ignore_ascii_case(qualifier)
                    })
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [index] => Ok(*index),
            [] => {
                if let Some(qualifier) = &reference.qualifier {
                    if !self
                        .relations
                        .iter()
                        .any(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
                    {
                        return Err(Error::InvalidQuery(format!(
                            "unknown table or alias '{qualifier}'"
                        )));
                    }
                    Err(Error::ColumnNotFound {
                        table: qualifier.clone(),
                        column: reference.name.clone(),
                    })
                } else if let [relation] = self.relations.as_slice() {
                    Err(Error::ColumnNotFound {
                        table: relation.table_name.to_owned(),
                        column: reference.name.clone(),
                    })
                } else {
                    Err(Error::InvalidQuery(format!(
                        "column '{}' does not exist in joined tables",
                        reference.name
                    )))
                }
            }
            _ => Err(Error::InvalidQuery(format!(
                "column '{}' is ambiguous; qualify it with a table name or alias",
                reference.name
            ))),
        }
    }

    fn value_ref(&self, column: usize, row: usize) -> ValueRef<'_> {
        let field = &self.columns[column];
        let physical_row = match &self.rows {
            SourceRows::Identity { .. } => {
                debug_assert_eq!(field.relation, 0);
                row
            }
            SourceRows::Joined(rows) if field.relation == 0 => rows[row].left,
            SourceRows::Joined(rows) => rows[row].right,
        };
        field.column.value_ref(physical_row)
    }

    fn value(&self, column: usize, row: usize) -> Value {
        self.value_ref(column, row).to_owned()
    }

    fn cmp_at(&self, column: usize, left: usize, right: usize) -> Ordering {
        self.value_ref(column, left)
            .cmp(&self.value_ref(column, right))
    }
}

fn build_query_source<'a>(
    catalog: &'a Catalog,
    from: &TableReference,
    join: Option<&InnerJoin>,
) -> Result<QuerySource<'a>> {
    let left = catalog.table(&from.name)?;
    let Some(join) = join else {
        return Ok(QuerySource {
            columns: source_columns(left, 0),
            relations: vec![SourceRelation {
                qualifier: from.qualifier().to_owned(),
                table_name: left.name(),
            }],
            rows: SourceRows::Identity {
                len: left.row_count(),
            },
        });
    };

    if from
        .qualifier()
        .eq_ignore_ascii_case(join.table.qualifier())
    {
        return Err(Error::InvalidQuery(format!(
            "table name or alias '{}' is specified more than once",
            from.qualifier()
        )));
    }

    let right = catalog.table(&join.table.name)?;
    let mut columns = source_columns(left, 0);
    columns.extend(source_columns(right, 1));
    let mut source = QuerySource {
        columns,
        relations: vec![
            SourceRelation {
                qualifier: from.qualifier().to_owned(),
                table_name: left.name(),
            },
            SourceRelation {
                qualifier: join.table.qualifier().to_owned(),
                table_name: right.name(),
            },
        ],
        rows: SourceRows::Joined(Vec::new()),
    };

    let first = source.column_index(&join.left)?;
    let second = source.column_index(&join.right)?;
    let (left_key, right_key) = match (
        source.columns[first].relation,
        source.columns[second].relation,
    ) {
        (0, 1) => (first, second),
        (1, 0) => (second, first),
        _ => {
            return Err(Error::InvalidQuery(
                "INNER JOIN condition must compare one column from each relation".to_owned(),
            ));
        }
    };
    if !comparable(
        source.columns[left_key].data_type,
        source.columns[right_key].data_type,
    ) {
        return Err(Error::TypeMismatch {
            context: "INNER JOIN comparison".to_owned(),
            expected: source.columns[left_key].data_type.to_string(),
            actual: source.columns[right_key].data_type.to_string(),
        });
    }

    let mut right_rows = HashMap::<JoinKey<'_>, Vec<usize>>::new();
    for row in 0..right.row_count() {
        let key = JoinKey::from(source.columns[right_key].column.value_ref(row));
        right_rows.entry(key).or_default().push(row);
    }
    for left_row in 0..left.row_count() {
        let key = JoinKey::from(source.columns[left_key].column.value_ref(left_row));
        if let Some(matches) = right_rows.get(&key) {
            let SourceRows::Joined(rows) = &mut source.rows else {
                unreachable!("join source uses joined row storage")
            };
            rows.extend(matches.iter().map(|right| SourceRow {
                left: left_row,
                right: *right,
            }));
        }
    }
    Ok(source)
}

fn source_columns(table: &Table, relation: usize) -> Vec<SourceColumn<'_>> {
    table
        .schema()
        .iter()
        .zip(table.columns())
        .map(|(field, column)| SourceColumn {
            name: &field.name,
            data_type: field.data_type,
            column,
            relation,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum JoinKey<'a> {
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(&'a str),
}

impl<'a> From<ValueRef<'a>> for JoinKey<'a> {
    fn from(value: ValueRef<'a>) -> Self {
        match value {
            ValueRef::Int64(value) => Self::Int64(value),
            ValueRef::Float64(value)
                if value >= i64::MIN as f64
                    && value < 9_223_372_036_854_775_808.0
                    && value.fract() == 0.0 =>
            {
                Self::Int64(value as i64)
            }
            ValueRef::Float64(value) => {
                Self::Float64(if value == 0.0 { 0 } else { value.to_bits() })
            }
            ValueRef::Bool(value) => Self::Bool(value),
            ValueRef::String(value) => Self::String(value),
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

fn resolve_group_columns(
    source: &QuerySource<'_>,
    names: &[ColumnReference],
) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = source.column_index(name)?;
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
    source: &QuerySource<'_>,
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
                for (source_index, field) in source.columns.iter().enumerate() {
                    let group_position = group_columns
                        .iter()
                        .position(|column| *column == source_index);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source: source_index,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: field.name.to_owned(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source_index = source.column_index(name)?;
                let group_position = group_columns
                    .iter()
                    .position(|column| *column == source_index);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::Column {
                    source: source_index,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| source.columns[source_index].name.to_owned()),
                    data_type: source.columns[source_index].data_type,
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
                        let index = source.column_index(name)?;
                        (
                            Some(index),
                            Some(source.columns[index].data_type),
                            source.columns[index].name.to_owned(),
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
    source: &QuerySource<'_>,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source: column, .. } => source.value(*column, *row),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect()
        })
        .collect()
}

fn execute_grouped<'a>(
    source: &'a QuerySource<'a>,
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
        let (group, inserted) = groups.find_or_insert(source, group_columns, *row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, source, *row)?;
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
        source: &'a QuerySource<'a>,
        columns: &[usize],
        row: usize,
        next_group: usize,
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = source.value_ref(columns[0], row);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    source.value_ref(columns[0], row),
                    source.value_ref(columns[1], row),
                ];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| source.value_ref(*column, row))
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

    fn update(&mut self, spec: &AggregateSpec, source: &QuerySource<'_>, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let ValueRef::Int64(value) =
                    source.value_ref(spec.argument.expect("SUM argument"), row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let ValueRef::Float64(value) =
                    source.value_ref(spec.argument.expect("SUM argument"), row)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = source.value_ref(spec.argument.expect("MIN argument"), row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = source.value_ref(spec.argument.expect("MAX argument"), row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let ValueRef::Int64(value) =
                    source.value_ref(spec.argument.expect("AVG argument"), row)
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
                    source.value_ref(spec.argument.expect("AVG argument"), row)
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
    source: &QuerySource<'_>,
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        if order.name.qualifier.is_some() {
            let source_column = source.column_index(&order.name)?;
            let output = items.iter().position(|item| {
                matches!(item, ResolvedItem::Column { source, .. } if *source == source_column)
            });
            if let Some(output) = output {
                ordering.push(ResolvedOrder {
                    output,
                    descending: order.descending,
                });
                continue;
            }
            return Err(Error::InvalidQuery(format!(
                "ORDER BY column '{}' is not in the SELECT output",
                order.name
            )));
        }

        let matches = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name.name))
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
                    order.name.name
                )));
            }
            _ => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY name '{}' is ambiguous",
                    order.name.name
                )));
            }
        }
    }
    Ok(ordering)
}

fn order_source_rows(
    rows: &mut Vec<usize>,
    source: &QuerySource<'_>,
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
            let ResolvedItem::Column { source: column, .. } = items[order.output] else {
                unreachable!("ungrouped projections cannot contain aggregates")
            };
            let comparison = source.cmp_at(column, left, right);
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
    fn evaluate(&self, source: &QuerySource<'_>, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(source, row);
                let right = right.value(source, row);
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
            Self::And(left, right) => left.evaluate(source, row) && right.evaluate(source, row),
            Self::Or(left, right) => left.evaluate(source, row) || right.evaluate(source, row),
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

    fn value<'a>(&'a self, source: &'a QuerySource<'a>, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => source.value_ref(*index, row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(source: &QuerySource<'_>, predicate: &Predicate) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(source, left)?;
            let right = compile_operand(source, right)?;
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
            Box::new(compile_predicate(source, left)?),
            Box::new(compile_predicate(source, right)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate(source, left)?),
            Box::new(compile_predicate(source, right)?),
        )),
    }
}

fn compile_operand(source: &QuerySource<'_>, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = source.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: source.columns[index].data_type,
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
    fn single_table_source_uses_identity_rows() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE samples (id Int64); \
                 INSERT INTO samples VALUES (1), (2), (3);",
            )
            .expect("setup");

        let table = TableReference {
            name: "samples".to_owned(),
            alias: Some("s".to_owned()),
        };
        let source = build_query_source(database.catalog(), &table, None).expect("query source");

        assert!(matches!(source.rows, SourceRows::Identity { len: 3 }));
    }
}
