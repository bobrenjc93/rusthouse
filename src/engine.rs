use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, BinaryOperator, ComparisonOperator, OrderBy,
    Predicate, ScalarExpression, Select, SelectItem, Statement, UnaryOperator,
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

        let group_expressions = resolve_group_expressions(table, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_expressions)?;
        let ordering = resolve_ordering(&result_columns, &select.order_by)?;

        let mut matching_rows = Vec::new();
        for row in 0..table.row_count() {
            if match &predicate {
                Some(predicate) => predicate.evaluate(table, row)?,
                None => true,
            } {
                matching_rows.push(row);
            }
        }

        let grouped = !group_expressions.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped =
                execute_grouped(table, &matching_rows, &group_expressions, &aggregate_specs)?;
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
            if ordering.is_empty()
                && let Some(limit) = select.limit
            {
                matching_rows.truncate(limit);
            }
            let mut projected = execute_projection(table, &matching_rows, &items)?;
            order_projected_rows(&mut projected, &ordering, select.limit);
            projected.into_iter().map(|row| row.values).collect()
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ResolvedExpression {
    Column {
        index: usize,
        data_type: DataType,
    },
    Literal(Value),
    Unary {
        operator: UnaryOperator,
        expression: Box<Self>,
        data_type: DataType,
    },
    Binary {
        left: Box<Self>,
        operator: BinaryOperator,
        right: Box<Self>,
        data_type: DataType,
    },
}

impl ResolvedExpression {
    fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. }
            | Self::Unary { data_type, .. }
            | Self::Binary { data_type, .. } => *data_type,
            Self::Literal(value) => value.data_type(),
        }
    }

    fn constant(&self) -> Option<&Value> {
        match self {
            Self::Literal(value) => Some(value),
            _ => None,
        }
    }

    fn evaluate<'a>(&'a self, table: &'a Table, row: usize) -> Result<EvaluatedValue<'a>> {
        match self {
            Self::Column { index, .. } => Ok(EvaluatedValue::Borrowed(
                table.columns()[*index].value_ref(row),
            )),
            Self::Literal(value) => Ok(EvaluatedValue::Borrowed(value.as_ref())),
            Self::Unary {
                operator,
                expression,
                ..
            } => apply_unary(*operator, expression.evaluate(table, row)?.as_ref())
                .map(EvaluatedValue::Owned),
            Self::Binary {
                left,
                operator,
                right,
                ..
            } => apply_binary(
                *operator,
                left.evaluate(table, row)?.as_ref(),
                right.evaluate(table, row)?.as_ref(),
            )
            .map(EvaluatedValue::Owned),
        }
    }
}

#[derive(Debug, Clone)]
enum EvaluatedValue<'a> {
    Borrowed(ValueRef<'a>),
    Owned(Value),
}

impl EvaluatedValue<'_> {
    fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Borrowed(value) => *value,
            Self::Owned(value) => value.as_ref(),
        }
    }

    fn into_owned(self) -> Value {
        match self {
            Self::Borrowed(value) => value.to_owned(),
            Self::Owned(value) => value,
        }
    }
}

impl PartialEq for EvaluatedValue<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for EvaluatedValue<'_> {}

impl PartialOrd for EvaluatedValue<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EvaluatedValue<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_ref().cmp(&other.as_ref())
    }
}

impl Hash for EvaluatedValue<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Expression {
        expression: ResolvedExpression,
        group_position: Option<usize>,
    },
    Aggregate {
        state: usize,
    },
}

#[derive(Debug, Clone)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<ResolvedExpression>,
    input_type: Option<DataType>,
}

fn resolve_group_expressions(
    table: &Table,
    expressions: &[ScalarExpression],
) -> Result<Vec<ResolvedExpression>> {
    let mut resolved = Vec::with_capacity(expressions.len());
    for expression in expressions {
        let expression = resolve_expression(table, expression)?;
        if resolved.contains(&expression) {
            return Err(Error::InvalidQuery(
                "GROUP BY expression is listed more than once".to_owned(),
            ));
        }
        resolved.push(expression);
    }
    Ok(resolved)
}

fn resolve_select_items(
    table: &Table,
    requested: &[SelectItem],
    group_expressions: &[ResolvedExpression],
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
                    let expression = ResolvedExpression::Column {
                        index: source,
                        data_type: field.data_type,
                    };
                    let group_position = group_expressions
                        .iter()
                        .position(|group| group == &expression);
                    if !group_expressions.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Expression {
                        expression,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: field.name.clone(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Expression { expression, alias } => {
                let resolved = resolve_expression(table, expression)?;
                let group_position = group_expressions
                    .iter()
                    .position(|group| group == &resolved);
                if (has_aggregate || !group_expressions.is_empty())
                    && group_position.is_none()
                    && resolved.constant().is_none()
                {
                    return Err(Error::InvalidQuery(format!(
                        "expression '{expression}' must appear in GROUP BY"
                    )));
                }
                let output_name = alias.clone().unwrap_or_else(|| match &resolved {
                    ResolvedExpression::Column { index, .. } => table.schema()[*index].name.clone(),
                    _ => expression.to_string(),
                });
                let data_type = resolved.data_type();
                items.push(ResolvedItem::Expression {
                    expression: resolved,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: output_name,
                    data_type,
                });
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (resolved_argument, input_type, argument_name) = match argument {
                    AggregateArgument::Wildcard => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}(*) is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, None, "*".to_owned())
                    }
                    AggregateArgument::Expression(expression) => {
                        let resolved = resolve_expression(table, expression)?;
                        let name = match &resolved {
                            ResolvedExpression::Column { index, .. } => {
                                table.schema()[*index].name.clone()
                            }
                            _ => expression.to_string(),
                        };
                        let input_type = resolved.data_type();
                        (Some(resolved), Some(input_type), name)
                    }
                };
                validate_aggregate(*function, input_type)?;
                let state = aggregate_specs.len();
                aggregate_specs.push(AggregateSpec {
                    function: *function,
                    argument: resolved_argument,
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

fn resolve_expression(table: &Table, expression: &ScalarExpression) -> Result<ResolvedExpression> {
    match expression {
        ScalarExpression::Column(name) => {
            let index = table.column_index(name)?;
            Ok(ResolvedExpression::Column {
                index,
                data_type: table.schema()[index].data_type,
            })
        }
        ScalarExpression::Literal(value) => Ok(ResolvedExpression::Literal(value.clone())),
        ScalarExpression::Unary {
            operator,
            expression,
        } => {
            let resolved = resolve_expression(table, expression)?;
            require_numeric(resolved.data_type(), "unary numeric expression")?;
            if let Some(value) = resolved.constant() {
                return apply_unary(*operator, value.as_ref()).map(ResolvedExpression::Literal);
            }
            let data_type = resolved.data_type();
            Ok(ResolvedExpression::Unary {
                operator: *operator,
                expression: Box::new(resolved),
                data_type,
            })
        }
        ScalarExpression::Binary {
            left,
            operator,
            right,
        } => {
            let left = resolve_expression(table, left)?;
            let right = resolve_expression(table, right)?;
            require_numeric(left.data_type(), "left arithmetic operand")?;
            require_numeric(right.data_type(), "right arithmetic operand")?;
            let data_type = if left.data_type() == DataType::Float64
                || right.data_type() == DataType::Float64
            {
                DataType::Float64
            } else {
                DataType::Int64
            };
            if let (Some(left), Some(right)) = (left.constant(), right.constant()) {
                return apply_binary(*operator, left.as_ref(), right.as_ref())
                    .map(ResolvedExpression::Literal);
            }
            Ok(ResolvedExpression::Binary {
                left: Box::new(left),
                operator: *operator,
                right: Box::new(right),
                data_type,
            })
        }
    }
}

fn require_numeric(data_type: DataType, context: &str) -> Result<()> {
    if matches!(data_type, DataType::Int64 | DataType::Float64) {
        Ok(())
    } else {
        Err(Error::TypeMismatch {
            context: context.to_owned(),
            expected: "Int64 or Float64".to_owned(),
            actual: data_type.to_string(),
        })
    }
}

fn apply_unary(operator: UnaryOperator, value: ValueRef<'_>) -> Result<Value> {
    match (operator, value) {
        (UnaryOperator::Plus, ValueRef::Int64(value)) => Ok(Value::Int64(value)),
        (UnaryOperator::Plus, ValueRef::Float64(value)) => Ok(Value::Float64(value)),
        (UnaryOperator::Minus, ValueRef::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("unary - (Int64)".to_owned())),
        (UnaryOperator::Minus, ValueRef::Float64(value)) => Ok(Value::Float64(-value)),
        _ => unreachable!("unary arithmetic types are resolved"),
    }
}

fn apply_binary(
    operator: BinaryOperator,
    left: ValueRef<'_>,
    right: ValueRef<'_>,
) -> Result<Value> {
    if let (ValueRef::Int64(left), ValueRef::Int64(right)) = (left, right) {
        if matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder) && right == 0 {
            return Err(Error::DivisionByZero(operator.symbol().to_owned()));
        }
        let value = match operator {
            BinaryOperator::Add => left.checked_add(right),
            BinaryOperator::Subtract => left.checked_sub(right),
            BinaryOperator::Multiply => left.checked_mul(right),
            BinaryOperator::Divide => left.checked_div(right),
            BinaryOperator::Remainder => left.checked_rem(right),
        }
        .ok_or_else(|| Error::NumericOverflow(format!("Int64 {} expression", operator.symbol())))?;
        return Ok(Value::Int64(value));
    }

    let left = match left {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        _ => unreachable!("binary arithmetic types are resolved"),
    };
    let right = match right {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        _ => unreachable!("binary arithmetic types are resolved"),
    };
    if matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder) && right == 0.0 {
        return Err(Error::DivisionByZero(operator.symbol().to_owned()));
    }
    let value = match operator {
        BinaryOperator::Add => left + right,
        BinaryOperator::Subtract => left - right,
        BinaryOperator::Multiply => left * right,
        BinaryOperator::Divide => left / right,
        BinaryOperator::Remainder => left % right,
    };
    if !value.is_finite() {
        return Err(Error::NumericOverflow(format!(
            "Float64 {} expression",
            operator.symbol()
        )));
    }
    Ok(Value::Float64(value))
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

#[derive(Debug)]
struct ProjectedRow {
    source: usize,
    values: Vec<Value>,
}

fn execute_projection(
    table: &Table,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Result<Vec<ProjectedRow>> {
    matching_rows
        .iter()
        .map(|row| {
            let values = items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Expression { expression, .. } => expression
                        .evaluate(table, *row)
                        .map(EvaluatedValue::into_owned),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ProjectedRow {
                source: *row,
                values,
            })
        })
        .collect()
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_expressions: &'a [ResolvedExpression],
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_expressions.len(), matching_rows.len());
    let mut group_count = usize::from(group_expressions.is_empty());
    let initial_capacity = matching_rows.len().min(1_024);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(initial_capacity);
            if group_expressions.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for row in matching_rows {
        let (group, inserted) =
            groups.find_or_insert(table, group_expressions, *row, group_count)?;
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
    One(HashMap<EvaluatedValue<'a>, usize>),
    Multiple(HashMap<Box<[EvaluatedValue<'a>]>, usize>),
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
        expressions: &'a [ResolvedExpression],
        row: usize,
        next_group: usize,
    ) -> Result<(usize, bool)> {
        Ok(match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = expressions[0].evaluate(table, row)?;
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if expressions.len() == 2 => {
                let key = [
                    expressions[0].evaluate(table, row)?,
                    expressions[1].evaluate(table, row)?,
                ];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = expressions
                    .iter()
                    .map(|expression| expression.evaluate(table, row))
                    .collect::<Result<Vec<_>>>()?;
                find_or_insert_group(groups, &key, next_group)
            }
        })
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
    groups: &mut HashMap<Box<[EvaluatedValue<'a>]>, usize>,
    key: &[EvaluatedValue<'a>],
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
    One(EvaluatedValue<'a>),
    Multiple(Box<[EvaluatedValue<'a>]>),
}

impl GroupKey<'_> {
    fn value(&self, position: usize) -> ValueRef<'_> {
        match self {
            Self::Empty => unreachable!("a global aggregate has no grouped columns"),
            Self::One(value) if position == 0 => value.as_ref(),
            Self::One(_) => unreachable!("single-column group position is zero"),
            Self::Multiple(values) => values[position].as_ref(),
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
                        ResolvedItem::Expression {
                            group_position: Some(position),
                            ..
                        } => self.keys[*group].value(*position).to_owned(),
                        ResolvedItem::Expression {
                            expression,
                            group_position: None,
                        } => expression
                            .constant()
                            .expect("ungrouped scalar is a folded constant")
                            .clone(),
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

    fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        let argument = spec
            .argument
            .as_ref()
            .map(|expression| expression.evaluate(table, row))
            .transpose()?;
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Some(ValueRef::Int64(value)) = argument.as_ref().map(EvaluatedValue::as_ref)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Some(ValueRef::Float64(value)) = argument.as_ref().map(EvaluatedValue::as_ref)
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += value;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = argument.expect("MIN argument");
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate.as_ref() < existing.as_ref())
                {
                    *current = Some(candidate.into_owned());
                }
            }
            Self::Max(current) => {
                let candidate = argument.expect("MAX argument");
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate.as_ref() > existing.as_ref())
                {
                    *current = Some(candidate.into_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let Some(ValueRef::Int64(value)) = argument.as_ref().map(EvaluatedValue::as_ref)
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
                let Some(ValueRef::Float64(value)) = argument.as_ref().map(EvaluatedValue::as_ref)
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

fn order_projected_rows(
    rows: &mut Vec<ProjectedRow>,
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    if ordering.is_empty() {
        return;
    }

    if let Some(0) = limit {
        rows.clear();
        return;
    }
    if let Some(limit) = limit.filter(|limit| *limit < rows.len()) {
        rows.select_nth_unstable_by(limit, |left, right| {
            compare_projected_rows(left, right, ordering)
        });
        rows.truncate(limit);
    }
    rows.sort_unstable_by(|left, right| compare_projected_rows(left, right, ordering));
}

fn compare_projected_rows(
    left: &ProjectedRow,
    right: &ProjectedRow,
    ordering: &[ResolvedOrder],
) -> Ordering {
    for order in ordering {
        let comparison = left.values[order.output].cmp(&right.values[order.output]);
        if comparison != Ordering::Equal {
            return if order.descending {
                comparison.reverse()
            } else {
                comparison
            };
        }
    }
    left.source.cmp(&right.source)
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
                ResolvedItem::Expression {
                    group_position: Some(position),
                    ..
                } => data.keys[left]
                    .value(position)
                    .cmp(&data.keys[right].value(position)),
                ResolvedItem::Expression {
                    group_position: None,
                    ..
                } => Ordering::Equal,
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
        left: ResolvedExpression,
        operator: ComparisonOperator,
        right: ResolvedExpression,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: usize) -> Result<bool> {
        Ok(match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.evaluate(table, row)?;
                let right = right.evaluate(table, row)?;
                let comparison = left
                    .as_ref()
                    .sql_cmp(right.as_ref())
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
            Self::And(left, right) => left.evaluate(table, row)? && right.evaluate(table, row)?,
            Self::Or(left, right) => left.evaluate(table, row)? || right.evaluate(table, row)?,
        })
    }
}

fn compile_predicate(table: &Table, predicate: &Predicate) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = resolve_expression(table, left)?;
            let right = resolve_expression(table, right)?;
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
