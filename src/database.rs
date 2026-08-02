use std::cmp::Ordering;
use std::collections::HashMap;

use crate::sql::{
    AggregateFunction, BinaryOperator, Expr, OrderBy, Select, SelectItem, Statement, UnaryOperator,
    parse,
};
use crate::storage::{Schema, Table, normalize_identifier};
use crate::{ColumnDefinition, DataType, DatabaseError, LimitKind, Value};

/// Resource limits enforced by parsing, storage, and query execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_rows_per_insert: usize,
    pub max_rows_per_table: usize,
    pub max_result_rows: usize,
    pub max_columns_per_table: usize,
    pub max_string_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_rows_per_insert: 1_000_000,
            max_rows_per_table: 10_000_000,
            max_result_rows: 1_000_000,
            max_columns_per_table: 1_024,
            max_string_bytes: 1024 * 1024,
        }
    }
}

/// A materialized query result with ordered columns and rows.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Vec<Value>>,
}

/// The outcome of one SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionResult {
    TableCreated { table: String },
    RowsInserted { table: String, rows: usize },
    Query(QueryResult),
}

/// An in-memory catalog and SQL execution engine.
#[derive(Debug, Default)]
pub struct Database {
    tables: HashMap<String, Table>,
    limits: Limits,
}

impl Database {
    /// Creates an empty database with default resource limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty database with caller-provided resource limits.
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Parses and executes all statements in `sql` in input order.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<ExecutionResult>, DatabaseError> {
        if sql.len() > self.limits.max_input_bytes {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::InputBytes,
                limit: self.limits.max_input_bytes,
                actual: sql.len(),
            });
        }
        let statements = parse(sql)?;
        statements
            .into_iter()
            .map(|statement| self.execute_parsed(statement))
            .collect()
    }

    /// Executes exactly one SQL statement.
    pub fn execute_one(&mut self, sql: &str) -> Result<ExecutionResult, DatabaseError> {
        let results = self.execute(sql)?;
        if results.len() != 1 {
            return Err(DatabaseError::invalid(format!(
                "expected one statement, got {}",
                results.len()
            )));
        }
        Ok(results.into_iter().next().expect("one result was checked"))
    }

    /// Returns a table schema, matching names case-insensitively.
    pub fn schema(&self, table: &str) -> Result<&Schema, DatabaseError> {
        self.tables
            .get(&normalize_identifier(table))
            .map(|table| &table.schema)
            .ok_or_else(|| DatabaseError::TableNotFound(table.to_owned()))
    }

    pub fn table_row_count(&self, table: &str) -> Result<usize, DatabaseError> {
        self.tables
            .get(&normalize_identifier(table))
            .map(Table::row_count)
            .ok_or_else(|| DatabaseError::TableNotFound(table.to_owned()))
    }

    fn execute_parsed(&mut self, statement: Statement) -> Result<ExecutionResult, DatabaseError> {
        match statement {
            Statement::CreateTable {
                name,
                if_not_exists,
                columns,
            } => {
                let key = normalize_identifier(&name);
                if self.tables.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecutionResult::TableCreated { table: name });
                    }
                    return Err(DatabaseError::TableAlreadyExists(name));
                }
                let schema = Schema::new(columns, &self.limits)?;
                self.tables.insert(key, Table::new(name.clone(), schema));
                Ok(ExecutionResult::TableCreated { table: name })
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.insert(table, columns, rows),
            Statement::Select(select) => self.select(select).map(ExecutionResult::Query),
        }
    }

    fn insert(
        &mut self,
        table_name: String,
        insert_columns: Option<Vec<String>>,
        expressions: Vec<Vec<Expr>>,
    ) -> Result<ExecutionResult, DatabaseError> {
        if expressions.len() > self.limits.max_rows_per_insert {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::RowsPerInsert,
                limit: self.limits.max_rows_per_insert,
                actual: expressions.len(),
            });
        }
        let key = normalize_identifier(&table_name);
        let table = self
            .tables
            .get_mut(&key)
            .ok_or_else(|| DatabaseError::TableNotFound(table_name.clone()))?;

        let column_order = if let Some(columns) = insert_columns {
            if columns.len() != table.schema.columns().len() {
                return Err(DatabaseError::InvalidValue(format!(
                    "INSERT names {} columns but table {} requires all {} non-nullable columns",
                    columns.len(),
                    table.name,
                    table.schema.columns().len()
                )));
            }
            let mut order = Vec::with_capacity(columns.len());
            let mut seen = vec![false; table.schema.columns().len()];
            for name in columns {
                let index = table
                    .schema
                    .column_index(&name)
                    .ok_or_else(|| DatabaseError::ColumnNotFound(name.clone()))?;
                if seen[index] {
                    return Err(DatabaseError::ColumnAlreadyExists(name));
                }
                seen[index] = true;
                order.push(index);
            }
            order
        } else {
            (0..table.schema.columns().len()).collect()
        };

        // Materialize and coerce every row before calling the atomic storage append.
        let mut rows = Vec::with_capacity(expressions.len());
        for (row_number, expression_row) in expressions.into_iter().enumerate() {
            if expression_row.len() != column_order.len() {
                return Err(DatabaseError::InvalidValue(format!(
                    "row {} has {} values but INSERT expects {}",
                    row_number + 1,
                    expression_row.len(),
                    column_order.len()
                )));
            }
            let mut row: Vec<Option<Value>> = vec![None; table.schema.columns().len()];
            for (expression, &target) in expression_row.iter().zip(&column_order) {
                if expression.contains_aggregate() {
                    return Err(DatabaseError::invalid(
                        "aggregate functions are not allowed in VALUES",
                    ));
                }
                let value = eval_row_expr(expression, None, None)?;
                let expected = table.schema.columns()[target].data_type;
                row[target] = Some(coerce_insert(value, expected)?);
            }
            rows.push(
                row.into_iter()
                    .map(|value| value.expect("all non-nullable columns were required"))
                    .collect(),
            );
        }
        let inserted = rows.len();
        table.append_rows(&rows, &self.limits)?;
        Ok(ExecutionResult::RowsInserted {
            table: table_name,
            rows: inserted,
        })
    }

    fn select(&self, mut select: Select) -> Result<QueryResult, DatabaseError> {
        let table = match &select.from {
            Some(name) => Some(
                self.tables
                    .get(&normalize_identifier(name))
                    .ok_or_else(|| DatabaseError::TableNotFound(name.clone()))?,
            ),
            None => None,
        };
        let empty_schema = Schema::new(
            vec![ColumnDefinition {
                name: "_virtual".into(),
                data_type: DataType::Bool,
            }],
            &self.limits,
        )?;
        let schema = table.map_or(&empty_schema, |table| &table.schema);

        let items = expand_wildcards(&select.items, schema, table.is_some())?;
        for expression in &mut select.group_by {
            resolve_projection_alias(expression, &items);
        }
        let has_aggregate = items.iter().any(|item| item.expr.contains_aggregate())
            || select
                .order_by
                .iter()
                .any(|order| order.expr.contains_aggregate());
        validate_select(&select, &items, has_aggregate)?;
        for item in &items {
            validate_column_references(&item.expr, table)?;
        }
        if let Some(filter) = &select.filter {
            validate_column_references(filter, table)?;
            let filter_type = infer_type(filter, schema)?;
            if filter_type != DataType::Bool {
                return Err(DatabaseError::TypeMismatch {
                    context: "WHERE".into(),
                    expected: DataType::Bool,
                    actual: filter_type,
                });
            }
        }
        for expression in &select.group_by {
            validate_column_references(expression, table)?;
            infer_type(expression, schema)?;
        }
        for order in &select.order_by {
            if !is_projection_alias(&order.expr, &items) {
                validate_column_references(&order.expr, table)?;
                infer_type(&order.expr, schema)?;
            }
        }

        let source_rows = table.map_or(1, Table::row_count);
        let mut filtered = Vec::new();
        for row in 0..source_rows {
            if let Some(filter) = &select.filter {
                let value = eval_row_expr(filter, table, Some(row))?;
                if expect_bool(value, "WHERE")? {
                    filtered.push(row);
                }
            } else {
                filtered.push(row);
            }
        }

        let grouped = has_aggregate || !select.group_by.is_empty();
        let groups = if grouped {
            build_groups(&filtered, &select.group_by, table)?
        } else {
            Vec::new()
        };

        let columns = items
            .iter()
            .map(|item| {
                Ok(ColumnDefinition {
                    name: item.alias.clone().unwrap_or_else(|| item.expr.label()),
                    data_type: infer_type(&item.expr, schema)?,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;

        let mut records = Vec::new();
        if grouped {
            for (ordinal, group) in groups.iter().enumerate() {
                let values = items
                    .iter()
                    .map(|item| eval_group_expr(&item.expr, table, &group.rows, schema))
                    .collect::<Result<Vec<_>, _>>()?;
                let order = evaluate_order(
                    &select.order_by,
                    &columns,
                    &values,
                    table,
                    group.rows.first().copied(),
                    Some(&group.rows),
                    schema,
                )?;
                records.push(Record {
                    values,
                    order,
                    ordinal,
                });
            }
        } else {
            for (ordinal, row) in filtered.into_iter().enumerate() {
                let values = items
                    .iter()
                    .map(|item| eval_row_expr(&item.expr, table, Some(row)))
                    .collect::<Result<Vec<_>, _>>()?;
                let order = evaluate_order(
                    &select.order_by,
                    &columns,
                    &values,
                    table,
                    Some(row),
                    None,
                    schema,
                )?;
                records.push(Record {
                    values,
                    order,
                    ordinal,
                });
            }
        }

        validate_order_keys(&records)?;
        if !select.order_by.is_empty() {
            records.sort_by(|left, right| {
                for (index, order) in select.order_by.iter().enumerate() {
                    let ordering = value_order(&left.order[index], &right.order[index])
                        .unwrap_or(Ordering::Equal);
                    let ordering = if order.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                left.ordinal.cmp(&right.ordinal)
            });
        }

        let available = records.len().saturating_sub(select.offset);
        let take = select.limit.unwrap_or(available).min(available);
        if take > self.limits.max_result_rows {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ResultRows,
                limit: self.limits.max_result_rows,
                actual: take,
            });
        }
        let rows = records
            .into_iter()
            .skip(select.offset)
            .take(take)
            .map(|record| record.values)
            .collect();
        Ok(QueryResult { columns, rows })
    }
}

#[derive(Debug)]
struct Projection {
    expr: Expr,
    alias: Option<String>,
}

fn expand_wildcards(
    items: &[SelectItem],
    schema: &Schema,
    has_table: bool,
) -> Result<Vec<Projection>, DatabaseError> {
    let mut expanded = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard if !has_table => {
                return Err(DatabaseError::invalid("SELECT * requires a FROM table"));
            }
            SelectItem::Wildcard => {
                expanded.extend(schema.columns().iter().map(|column| Projection {
                    expr: Expr::Column(column.name.clone()),
                    alias: None,
                }))
            }
            SelectItem::Expr { expr, alias } => expanded.push(Projection {
                expr: expr.clone(),
                alias: alias.clone(),
            }),
        }
    }
    Ok(expanded)
}

fn validate_select(
    select: &Select,
    items: &[Projection],
    has_aggregate: bool,
) -> Result<(), DatabaseError> {
    if select.filter.as_ref().is_some_and(Expr::contains_aggregate) {
        return Err(DatabaseError::invalid(
            "aggregate functions are not allowed in WHERE",
        ));
    }
    if select.group_by.iter().any(Expr::contains_aggregate) {
        return Err(DatabaseError::invalid(
            "aggregate functions are not allowed in GROUP BY",
        ));
    }
    if has_aggregate || !select.group_by.is_empty() {
        for item in items {
            validate_group_expression(&item.expr, &select.group_by, false)?;
        }
        for order in &select.order_by {
            if !is_projection_alias(&order.expr, items) {
                validate_group_expression(&order.expr, &select.group_by, false)?;
            }
        }
    }
    Ok(())
}

fn validate_group_expression(
    expr: &Expr,
    group_by: &[Expr],
    inside_aggregate: bool,
) -> Result<(), DatabaseError> {
    if !inside_aggregate && group_by.iter().any(|group| equivalent_expr(expr, group)) {
        return Ok(());
    }
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) if inside_aggregate => {
            let _ = name;
            Ok(())
        }
        Expr::Column(name) => Err(DatabaseError::invalid(format!(
            "column {name} must appear in GROUP BY or an aggregate function"
        ))),
        Expr::Aggregate { argument, .. } => {
            if inside_aggregate {
                return Err(DatabaseError::invalid(
                    "nested aggregate functions are not supported",
                ));
            }
            if let Some(argument) = argument {
                validate_group_expression(argument, group_by, true)?;
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            validate_group_expression(left, group_by, inside_aggregate)?;
            validate_group_expression(right, group_by, inside_aggregate)
        }
        Expr::Unary { expr, .. } => validate_group_expression(expr, group_by, inside_aggregate),
    }
}

fn equivalent_expr(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Column(left), Expr::Column(right)) => {
            normalize_identifier(unqualify(left)) == normalize_identifier(unqualify(right))
        }
        _ => left == right,
    }
}

fn is_projection_alias(expr: &Expr, items: &[Projection]) -> bool {
    let Expr::Column(name) = expr else {
        return false;
    };
    items.iter().any(|item| {
        item.alias
            .as_ref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
    })
}

fn resolve_projection_alias(expr: &mut Expr, items: &[Projection]) {
    let Expr::Column(name) = expr else {
        return;
    };
    if let Some(item) = items.iter().find(|item| {
        item.alias
            .as_ref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
    }) {
        *expr = item.expr.clone();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ScalarKey {
    Int(i64),
    Float(u64),
    Bool(bool),
    String(String),
}

impl From<&Value> for ScalarKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Int64(value) => Self::Int(*value),
            Value::Float64(value) => {
                let bits = if *value == 0.0 { 0 } else { value.to_bits() };
                Self::Float(bits)
            }
            Value::Bool(value) => Self::Bool(*value),
            Value::String(value) => Self::String(value.clone()),
        }
    }
}

struct Group {
    rows: Vec<usize>,
}

fn build_groups(
    rows: &[usize],
    group_by: &[Expr],
    table: Option<&Table>,
) -> Result<Vec<Group>, DatabaseError> {
    if group_by.is_empty() {
        return Ok(vec![Group {
            rows: rows.to_vec(),
        }]);
    }
    let mut indexes: HashMap<Vec<ScalarKey>, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for &row in rows {
        let values = group_by
            .iter()
            .map(|expr| eval_row_expr(expr, table, Some(row)))
            .collect::<Result<Vec<_>, _>>()?;
        let key: Vec<_> = values.iter().map(ScalarKey::from).collect();
        let index = if let Some(index) = indexes.get(&key) {
            *index
        } else {
            let index = groups.len();
            indexes.insert(key, index);
            groups.push(Group { rows: Vec::new() });
            index
        };
        groups[index].rows.push(row);
    }
    Ok(groups)
}

fn eval_group_expr(
    expr: &Expr,
    table: Option<&Table>,
    rows: &[usize],
    schema: &Schema,
) -> Result<Value, DatabaseError> {
    match expr {
        Expr::Aggregate { function, argument } => {
            eval_aggregate(*function, argument.as_deref(), table, rows, schema)
        }
        Expr::Binary {
            left,
            operator,
            right,
        } => {
            let left = eval_group_expr(left, table, rows, schema)?;
            let right = eval_group_expr(right, table, rows, schema)?;
            eval_binary(left, *operator, right)
        }
        Expr::Unary { operator, expr } => {
            let value = eval_group_expr(expr, table, rows, schema)?;
            eval_unary(*operator, value)
        }
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(_) => {
            let row = rows.first().copied().ok_or_else(|| {
                DatabaseError::invalid("cannot evaluate a column over an empty aggregate group")
            })?;
            eval_row_expr(expr, table, Some(row))
        }
    }
}

fn eval_aggregate(
    function: AggregateFunction,
    argument: Option<&Expr>,
    table: Option<&Table>,
    rows: &[usize],
    schema: &Schema,
) -> Result<Value, DatabaseError> {
    if function == AggregateFunction::Count {
        let count = i64::try_from(rows.len())
            .map_err(|_| DatabaseError::ArithmeticOverflow("COUNT exceeds Int64".into()))?;
        return Ok(Value::Int64(count));
    }
    let argument = argument.expect("non-count aggregates require an argument");
    let data_type = infer_type(argument, schema)?;
    match function {
        AggregateFunction::Sum => match data_type {
            DataType::Int64 => {
                let mut sum = 0_i64;
                for &row in rows {
                    let Value::Int64(value) = eval_row_expr(argument, table, Some(row))? else {
                        unreachable!("inferred aggregate type changed")
                    };
                    sum = sum
                        .checked_add(value)
                        .ok_or_else(|| DatabaseError::ArithmeticOverflow("SUM(Int64)".into()))?;
                }
                Ok(Value::Int64(sum))
            }
            DataType::Float64 => {
                let mut sum = 0.0;
                for &row in rows {
                    sum += numeric_f64(eval_row_expr(argument, table, Some(row))?)?;
                }
                Ok(Value::Float64(sum))
            }
            _ => Err(DatabaseError::invalid(format!(
                "SUM requires a numeric argument, got {data_type}"
            ))),
        },
        AggregateFunction::Avg => {
            if !matches!(data_type, DataType::Int64 | DataType::Float64) {
                return Err(DatabaseError::invalid(format!(
                    "AVG requires a numeric argument, got {data_type}"
                )));
            }
            let mut sum = 0.0;
            for &row in rows {
                sum += numeric_f64(eval_row_expr(argument, table, Some(row))?)?;
            }
            Ok(Value::Float64(sum / rows.len() as f64))
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            let mut values = rows.iter();
            let Some(first) = values.next() else {
                return Ok(default_value(data_type, function == AggregateFunction::Avg));
            };
            let mut selected = eval_row_expr(argument, table, Some(*first))?;
            for row in values {
                let candidate = eval_row_expr(argument, table, Some(*row))?;
                let order = value_order(&candidate, &selected)?;
                let replace = if function == AggregateFunction::Min {
                    order == Ordering::Less
                } else {
                    order == Ordering::Greater
                };
                if replace {
                    selected = candidate;
                }
            }
            Ok(selected)
        }
        AggregateFunction::Count => unreachable!(),
    }
}

fn default_value(data_type: DataType, nan_float: bool) -> Value {
    match data_type {
        DataType::Int64 => Value::Int64(0),
        DataType::Float64 if nan_float => Value::Float64(f64::NAN),
        DataType::Float64 => Value::Float64(0.0),
        DataType::Bool => Value::Bool(false),
        DataType::String => Value::String(String::new()),
    }
}

fn eval_row_expr(
    expr: &Expr,
    table: Option<&Table>,
    row: Option<usize>,
) -> Result<Value, DatabaseError> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(name) => {
            let table = table.ok_or_else(|| {
                DatabaseError::invalid(format!("column {name} requires a FROM table"))
            })?;
            if let Some(qualifier) = name.rsplit_once('.').map(|(qualifier, _)| qualifier)
                && !qualifier.eq_ignore_ascii_case(&table.name)
            {
                return Err(DatabaseError::ColumnNotFound(name.clone()));
            }
            let index = table
                .schema
                .column_index(unqualify(name))
                .ok_or_else(|| DatabaseError::ColumnNotFound(name.clone()))?;
            let row = row.ok_or_else(|| DatabaseError::invalid("missing source row"))?;
            Ok(table.value(index, row))
        }
        Expr::Aggregate { .. } => Err(DatabaseError::invalid(
            "aggregate function used outside aggregate execution",
        )),
        Expr::Binary {
            left,
            operator,
            right,
        } => {
            if *operator == BinaryOperator::And {
                let left = expect_bool(eval_row_expr(left, table, row)?, "AND")?;
                if !left {
                    return Ok(Value::Bool(false));
                }
                return Ok(Value::Bool(expect_bool(
                    eval_row_expr(right, table, row)?,
                    "AND",
                )?));
            }
            if *operator == BinaryOperator::Or {
                let left = expect_bool(eval_row_expr(left, table, row)?, "OR")?;
                if left {
                    return Ok(Value::Bool(true));
                }
                return Ok(Value::Bool(expect_bool(
                    eval_row_expr(right, table, row)?,
                    "OR",
                )?));
            }
            let left = eval_row_expr(left, table, row)?;
            let right = eval_row_expr(right, table, row)?;
            eval_binary(left, *operator, right)
        }
        Expr::Unary { operator, expr } => eval_unary(*operator, eval_row_expr(expr, table, row)?),
    }
}

fn eval_unary(operator: UnaryOperator, value: Value) -> Result<Value, DatabaseError> {
    match (operator, value) {
        (UnaryOperator::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (UnaryOperator::Negate, Value::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| DatabaseError::ArithmeticOverflow("negating Int64".into())),
        (UnaryOperator::Negate, Value::Float64(value)) => Ok(Value::Float64(-value)),
        (UnaryOperator::Positive, value @ (Value::Int64(_) | Value::Float64(_))) => Ok(value),
        (operator, value) => Err(DatabaseError::invalid(format!(
            "operator {operator:?} does not accept {}",
            value.data_type()
        ))),
    }
}

fn eval_binary(
    left: Value,
    operator: BinaryOperator,
    right: Value,
) -> Result<Value, DatabaseError> {
    match operator {
        BinaryOperator::And | BinaryOperator::Or => {
            let left = expect_bool(left, "logical operator")?;
            let right = expect_bool(right, "logical operator")?;
            Ok(Value::Bool(if operator == BinaryOperator::And {
                left && right
            } else {
                left || right
            }))
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Less
        | BinaryOperator::LessEq
        | BinaryOperator::Greater
        | BinaryOperator::GreaterEq => {
            let order = value_order(&left, &right)?;
            let result = match operator {
                BinaryOperator::Eq => order == Ordering::Equal,
                BinaryOperator::NotEq => order != Ordering::Equal,
                BinaryOperator::Less => order == Ordering::Less,
                BinaryOperator::LessEq => order != Ordering::Greater,
                BinaryOperator::Greater => order == Ordering::Greater,
                BinaryOperator::GreaterEq => order != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply => {
            arithmetic(left, operator, right)
        }
        BinaryOperator::Divide => {
            let denominator = numeric_f64(right)?;
            if denominator == 0.0 {
                return Err(DatabaseError::InvalidValue("division by zero".into()));
            }
            Ok(Value::Float64(numeric_f64(left)? / denominator))
        }
        BinaryOperator::Modulo => match (left, right) {
            (Value::Int64(_), Value::Int64(0)) => {
                Err(DatabaseError::InvalidValue("modulo by zero".into()))
            }
            (Value::Int64(left), Value::Int64(right)) => left
                .checked_rem(right)
                .map(Value::Int64)
                .ok_or_else(|| DatabaseError::ArithmeticOverflow("Int64 modulo".into())),
            (left, right) => {
                let right = numeric_f64(right)?;
                if right == 0.0 {
                    return Err(DatabaseError::InvalidValue("modulo by zero".into()));
                }
                Ok(Value::Float64(numeric_f64(left)? % right))
            }
        },
    }
}

fn arithmetic(left: Value, operator: BinaryOperator, right: Value) -> Result<Value, DatabaseError> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => {
            let result = match operator {
                BinaryOperator::Add => left.checked_add(right),
                BinaryOperator::Subtract => left.checked_sub(right),
                BinaryOperator::Multiply => left.checked_mul(right),
                _ => unreachable!(),
            };
            result
                .map(Value::Int64)
                .ok_or_else(|| DatabaseError::ArithmeticOverflow(format!("Int64 {operator:?}")))
        }
        (left, right)
            if matches!(left, Value::Int64(_) | Value::Float64(_))
                && matches!(right, Value::Int64(_) | Value::Float64(_)) =>
        {
            let left = numeric_f64(left)?;
            let right = numeric_f64(right)?;
            Ok(Value::Float64(match operator {
                BinaryOperator::Add => left + right,
                BinaryOperator::Subtract => left - right,
                BinaryOperator::Multiply => left * right,
                _ => unreachable!(),
            }))
        }
        (left, right) => Err(DatabaseError::invalid(format!(
            "numeric operator requires numbers, got {} and {}",
            left.data_type(),
            right.data_type()
        ))),
    }
}

fn value_order(left: &Value, right: &Value) -> Result<Ordering, DatabaseError> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::Float64(left), Value::Float64(right)) => Ok(left
            .partial_cmp(right)
            .unwrap_or_else(|| left.total_cmp(right))),
        (Value::Int64(left), Value::Float64(right)) => {
            let left = *left as f64;
            Ok(left
                .partial_cmp(right)
                .unwrap_or_else(|| left.total_cmp(right)))
        }
        (Value::Float64(left), Value::Int64(right)) => {
            let right = *right as f64;
            Ok(left
                .partial_cmp(&right)
                .unwrap_or_else(|| left.total_cmp(&right)))
        }
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (left, right) => Err(DatabaseError::invalid(format!(
            "cannot compare {} and {}",
            left.data_type(),
            right.data_type()
        ))),
    }
}

fn numeric_f64(value: Value) -> Result<f64, DatabaseError> {
    match value {
        Value::Int64(value) => Ok(value as f64),
        Value::Float64(value) => Ok(value),
        value => Err(DatabaseError::invalid(format!(
            "expected a number, got {}",
            value.data_type()
        ))),
    }
}

fn expect_bool(value: Value, context: &str) -> Result<bool, DatabaseError> {
    match value {
        Value::Bool(value) => Ok(value),
        value => Err(DatabaseError::TypeMismatch {
            context: context.to_owned(),
            expected: DataType::Bool,
            actual: value.data_type(),
        }),
    }
}

fn coerce_insert(value: Value, expected: DataType) -> Result<Value, DatabaseError> {
    if value.data_type() == expected {
        return Ok(value);
    }
    match (value, expected) {
        (Value::Int64(value), DataType::Float64) => Ok(Value::Float64(value as f64)),
        (value, expected) => Err(DatabaseError::TypeMismatch {
            context: "INSERT value".into(),
            expected,
            actual: value.data_type(),
        }),
    }
}

fn infer_type(expr: &Expr, schema: &Schema) -> Result<DataType, DatabaseError> {
    match expr {
        Expr::Literal(value) => Ok(value.data_type()),
        Expr::Column(name) => schema
            .column_index(unqualify(name))
            .map(|index| schema.columns()[index].data_type)
            .ok_or_else(|| DatabaseError::ColumnNotFound(name.clone())),
        Expr::Aggregate { function, argument } => match function {
            AggregateFunction::Count => {
                if let Some(argument) = argument {
                    infer_type(argument, schema)?;
                }
                Ok(DataType::Int64)
            }
            AggregateFunction::Avg => {
                let argument = argument.as_deref().expect("AVG requires an argument");
                let argument_type = infer_type(argument, schema)?;
                if matches!(argument_type, DataType::Int64 | DataType::Float64) {
                    Ok(DataType::Float64)
                } else {
                    Err(DatabaseError::invalid(format!(
                        "AVG requires a numeric argument, got {argument_type}"
                    )))
                }
            }
            AggregateFunction::Sum => {
                let argument = argument.as_deref().expect("SUM requires an argument");
                let argument_type = infer_type(argument, schema)?;
                if matches!(argument_type, DataType::Int64 | DataType::Float64) {
                    Ok(argument_type)
                } else {
                    Err(DatabaseError::invalid(format!(
                        "SUM requires a numeric argument, got {argument_type}"
                    )))
                }
            }
            AggregateFunction::Min | AggregateFunction::Max => infer_type(
                argument.as_deref().expect("MIN/MAX requires an argument"),
                schema,
            ),
        },
        Expr::Binary {
            left,
            operator,
            right,
        } => match operator {
            BinaryOperator::Or | BinaryOperator::And => {
                let left = infer_type(left, schema)?;
                let right = infer_type(right, schema)?;
                if left == DataType::Bool && right == DataType::Bool {
                    Ok(DataType::Bool)
                } else {
                    Err(DatabaseError::invalid(format!(
                        "logical operator requires Bool values, got {left} and {right}"
                    )))
                }
            }
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Less
            | BinaryOperator::LessEq
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEq => {
                let left = infer_type(left, schema)?;
                let right = infer_type(right, schema)?;
                if left == right
                    || (matches!(left, DataType::Int64 | DataType::Float64)
                        && matches!(right, DataType::Int64 | DataType::Float64))
                {
                    Ok(DataType::Bool)
                } else {
                    Err(DatabaseError::invalid(format!(
                        "cannot compare {left} and {right}"
                    )))
                }
            }
            BinaryOperator::Divide => {
                require_numeric(infer_type(left, schema)?, "division")?;
                require_numeric(infer_type(right, schema)?, "division")?;
                Ok(DataType::Float64)
            }
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Modulo => {
                let left = infer_type(left, schema)?;
                let right = infer_type(right, schema)?;
                require_numeric(left, "arithmetic")?;
                require_numeric(right, "arithmetic")?;
                if left == DataType::Float64 || right == DataType::Float64 {
                    Ok(DataType::Float64)
                } else {
                    Ok(DataType::Int64)
                }
            }
        },
        Expr::Unary { operator, expr } => {
            let data_type = infer_type(expr, schema)?;
            match operator {
                UnaryOperator::Not if data_type == DataType::Bool => Ok(DataType::Bool),
                UnaryOperator::Negate | UnaryOperator::Positive
                    if matches!(data_type, DataType::Int64 | DataType::Float64) =>
                {
                    Ok(data_type)
                }
                _ => Err(DatabaseError::invalid(format!(
                    "operator {operator:?} does not accept {data_type}"
                ))),
            }
        }
    }
}

fn require_numeric(data_type: DataType, context: &str) -> Result<(), DatabaseError> {
    if matches!(data_type, DataType::Int64 | DataType::Float64) {
        Ok(())
    } else {
        Err(DatabaseError::invalid(format!(
            "{context} requires numeric values, got {data_type}"
        )))
    }
}

fn validate_column_references(expr: &Expr, table: Option<&Table>) -> Result<(), DatabaseError> {
    match expr {
        Expr::Column(name) => {
            let table = table.ok_or_else(|| {
                DatabaseError::invalid(format!("column {name} requires a FROM table"))
            })?;
            if let Some((qualifier, _)) = name.rsplit_once('.')
                && !qualifier.eq_ignore_ascii_case(&table.name)
            {
                return Err(DatabaseError::ColumnNotFound(name.clone()));
            }
            table
                .schema
                .column_index(unqualify(name))
                .map(|_| ())
                .ok_or_else(|| DatabaseError::ColumnNotFound(name.clone()))
        }
        Expr::Aggregate { argument, .. } => argument.as_deref().map_or(Ok(()), |argument| {
            validate_column_references(argument, table)
        }),
        Expr::Binary { left, right, .. } => {
            validate_column_references(left, table)?;
            validate_column_references(right, table)
        }
        Expr::Unary { expr, .. } => validate_column_references(expr, table),
        Expr::Literal(_) => Ok(()),
    }
}

fn evaluate_order(
    order_by: &[OrderBy],
    columns: &[ColumnDefinition],
    values: &[Value],
    table: Option<&Table>,
    row: Option<usize>,
    group: Option<&[usize]>,
    schema: &Schema,
) -> Result<Vec<Value>, DatabaseError> {
    order_by
        .iter()
        .map(|order| {
            if let Expr::Column(name) = &order.expr
                && let Some(index) = columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
            {
                return Ok(values[index].clone());
            }
            if let Some(group) = group {
                eval_group_expr(&order.expr, table, group, schema)
            } else {
                eval_row_expr(&order.expr, table, row)
            }
        })
        .collect()
}

fn validate_order_keys(records: &[Record]) -> Result<(), DatabaseError> {
    let Some(first) = records.first() else {
        return Ok(());
    };
    for record in &records[1..] {
        for (left, right) in first.order.iter().zip(&record.order) {
            value_order(left, right)?;
        }
    }
    Ok(())
}

fn unqualify(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, column)| column)
}

struct Record {
    values: Vec<Value>,
    order: Vec<Value>,
    ordinal: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(database: &mut Database, sql: &str) -> QueryResult {
        match database.execute_one(sql).unwrap() {
            ExecutionResult::Query(result) => result,
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn insert_batch_is_atomic() {
        let mut database = Database::new();
        database
            .execute_one("CREATE TABLE things (id Int64, label String)")
            .unwrap();
        let error = database
            .execute_one("INSERT INTO things VALUES (1, 'ok'), (2, 3)")
            .unwrap_err();
        assert!(matches!(error, DatabaseError::TypeMismatch { .. }));
        assert_eq!(database.table_row_count("things").unwrap(), 0);
    }

    #[test]
    fn grouped_aggregates_filter_sort_and_limit() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE metrics (team String, points Int64, ratio Float64, live Bool);
                 INSERT INTO metrics VALUES
                 ('red', 4, 1.5, true), ('blue', 8, 2.0, true),
                 ('red', 6, 2.5, false), ('blue', 2, 4.0, true);",
            )
            .unwrap();
        let result = query(
            &mut database,
            "SELECT team AS bucket, COUNT(*) AS n, SUM(points) AS total,
                    MIN(ratio) AS low, MAX(ratio) AS high, AVG(points) AS mean
             FROM metrics WHERE live = true OR points >= 6
             GROUP BY team ORDER BY total DESC, bucket ASC LIMIT 1",
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Value::String("blue".into()),
                Value::Int64(2),
                Value::Int64(10),
                Value::Float64(2.0),
                Value::Float64(4.0),
                Value::Float64(5.0),
            ]]
        );
    }

    #[test]
    fn configured_limits_are_typed_errors() {
        let mut database = Database::with_limits(Limits {
            max_input_bytes: 1_000,
            max_rows_per_insert: 1,
            max_rows_per_table: 2,
            max_result_rows: 1,
            max_columns_per_table: 2,
            max_string_bytes: 3,
        });
        database
            .execute_one("CREATE TABLE bounded (id Int64, s String)")
            .unwrap();
        let error = database
            .execute_one("INSERT INTO bounded VALUES (1, 'long')")
            .unwrap_err();
        assert!(matches!(
            error,
            DatabaseError::LimitExceeded {
                kind: LimitKind::StringBytes,
                ..
            }
        ));
        assert_eq!(database.table_row_count("bounded").unwrap(), 0);
    }

    #[test]
    fn empty_tables_still_bind_and_type_check_expressions() {
        let mut database = Database::new();
        database
            .execute_one("CREATE TABLE empty_data (id Int64, active Bool)")
            .unwrap();
        assert!(matches!(
            database.execute_one("SELECT missing = 1 FROM empty_data"),
            Err(DatabaseError::ColumnNotFound(name)) if name == "missing"
        ));
        assert!(matches!(
            database.execute_one("SELECT * FROM empty_data WHERE id"),
            Err(DatabaseError::TypeMismatch { context, .. }) if context == "WHERE"
        ));
        assert!(matches!(
            database.execute_one("SELECT COUNT(missing) FROM empty_data"),
            Err(DatabaseError::ColumnNotFound(name)) if name == "missing"
        ));
    }
}
