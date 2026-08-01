use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::error::{Error, Result};
use crate::sql::{
    BinaryOperator, Expr, OrderBy, Select, SelectItem, Statement, UnaryOperator, expression_name,
};
use crate::storage::{Catalog, Table, Value, canonical};

const MAX_RESULT_ROWS: usize = 1_000_000;
const MAX_RESULT_CELLS: usize = 10_000_000;

/// A materialized result set returned by a SELECT statement.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// An in-memory SQL engine. One instance retains tables across calls.
#[derive(Default)]
pub struct Engine {
    catalog: Catalog,
}

impl Engine {
    /// Creates an empty engine.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses and executes one or more semicolon-delimited SQL statements.
    ///
    /// DDL and INSERT statements return no result set. Each SELECT contributes
    /// one result in statement order.
    pub fn execute(&mut self, sql: &str) -> std::result::Result<Vec<QueryResult>, crate::Error> {
        let statements = crate::sql::parse(sql)?;
        let mut results = Vec::new();
        for statement in statements {
            match statement {
                Statement::Create(statement) => self.catalog.create(statement)?,
                Statement::Insert(statement) => {
                    let rows = statement
                        .rows
                        .iter()
                        .map(|row| row.iter().map(constant_value).collect())
                        .collect::<Result<Vec<Vec<_>>>>()?;
                    self.catalog
                        .insert(&statement.table, statement.columns.as_deref(), rows)?;
                }
                Statement::Select(statement) => results.push(self.select(statement)?),
            }
        }
        Ok(results)
    }

    fn select(&self, mut select: Select) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        select.items = expand_wildcards(select.items, table)?;
        if select.items.is_empty() {
            return Err(Error::new("SELECT list cannot be empty"));
        }
        let aliases = select_aliases(&select.items)?;
        normalize_group_by(&mut select, table, &aliases)?;
        validate_select(&select, table)?;

        let mut matching_rows = Vec::new();
        for row in 0..table.row_count {
            let context = EvalContext::row(table, row);
            let keep = match &select.filter {
                Some(expression) => predicate(eval(expression, &context, None)?, "WHERE")?,
                None => true,
            };
            if keep {
                matching_rows.push(row);
            }
        }

        let aggregate_query = !select.group_by.is_empty()
            || select
                .items
                .iter()
                .any(|item| has_aggregate(&item.expression))
            || select.having.as_ref().is_some_and(has_aggregate)
            || select
                .order_by
                .iter()
                .any(|item| has_aggregate(&item.expression));

        let mut output = if aggregate_query {
            self.aggregate_rows(table, &select, &aliases, matching_rows)?
        } else {
            if select.having.is_some() {
                return Err(Error::new(
                    "HAVING requires GROUP BY or an aggregate expression",
                ));
            }
            self.project_rows(table, &select, &aliases, matching_rows)?
        };

        if select.distinct {
            let mut seen = HashSet::new();
            output.retain(|row| seen.insert(ValueKey::row(&row.values)));
        }

        if !select.order_by.is_empty() {
            output.sort_by(|left, right| compare_output_rows(left, right, &select.order_by));
        }

        let start = select.offset.min(output.len());
        let end = select.limit.map_or(output.len(), |limit| {
            start.saturating_add(limit).min(output.len())
        });
        let rows = output.drain(start..end).map(|row| row.values).collect();
        let columns = select
            .items
            .iter()
            .map(|item| {
                item.alias
                    .clone()
                    .unwrap_or_else(|| expression_name(&item.expression))
            })
            .collect();
        Ok(QueryResult { columns, rows })
    }

    fn project_rows(
        &self,
        table: &Table,
        select: &Select,
        aliases: &HashMap<String, Expr>,
        rows: Vec<usize>,
    ) -> Result<Vec<OutputRow>> {
        let mut output = Vec::with_capacity(rows.len());
        for (ordinal, row) in rows.into_iter().enumerate() {
            let context = EvalContext::row(table, row);
            let values = select
                .items
                .iter()
                .map(|item| eval(&item.expression, &context, None))
                .collect::<Result<Vec<_>>>()?;
            let order_keys = order_keys(&select.order_by, &context, aliases, &values)?;
            output.push(OutputRow {
                values,
                order_keys,
                ordinal,
            });
            check_result_size(output.len(), select.items.len())?;
        }
        Ok(output)
    }

    fn aggregate_rows(
        &self,
        table: &Table,
        select: &Select,
        aliases: &HashMap<String, Expr>,
        rows: Vec<usize>,
    ) -> Result<Vec<OutputRow>> {
        let groups = build_groups(table, &select.group_by, rows)?;
        let mut output = Vec::with_capacity(groups.len());
        for group_rows in groups {
            let context = EvalContext::group(table, &group_rows);
            if let Some(having) = &select.having
                && !predicate(eval(having, &context, Some(aliases))?, "HAVING")?
            {
                continue;
            }
            let values = select
                .items
                .iter()
                .map(|item| eval(&item.expression, &context, None))
                .collect::<Result<Vec<_>>>()?;
            let order_keys = order_keys(&select.order_by, &context, aliases, &values)?;
            output.push(OutputRow {
                values,
                order_keys,
                ordinal: output.len(),
            });
            check_result_size(output.len(), select.items.len())?;
        }
        Ok(output)
    }
}

fn check_result_size(rows: usize, columns: usize) -> Result<()> {
    if rows > MAX_RESULT_ROWS {
        return Err(Error::new(format!(
            "result row limit exceeded (maximum {MAX_RESULT_ROWS})"
        )));
    }
    if rows.saturating_mul(columns) > MAX_RESULT_CELLS {
        return Err(Error::new(format!(
            "result cell limit exceeded (maximum {MAX_RESULT_CELLS})"
        )));
    }
    Ok(())
}

fn normalize_group_by(
    select: &mut Select,
    table: &Table,
    aliases: &HashMap<String, Expr>,
) -> Result<()> {
    for expression in &mut select.group_by {
        if let Expr::Literal(Value::Int64(position)) = expression
            && *position > 0
        {
            let position = usize::try_from(*position)
                .map_err(|_| Error::new("GROUP BY position is too large"))?;
            let item = select.items.get(position - 1).ok_or_else(|| {
                Error::new(format!(
                    "GROUP BY position {position} exceeds the SELECT list"
                ))
            })?;
            *expression = item.expression.clone();
        } else if let Expr::Column(name) = expression
            && table.column_index(name).is_none()
            && let Some(alias) = aliases.get(&canonical(name))
        {
            *expression = alias.clone();
        }
    }
    Ok(())
}

fn expand_wildcards(items: Vec<SelectItem>, table: &Table) -> Result<Vec<SelectItem>> {
    let mut expanded = Vec::new();
    for item in items {
        if item.expression == Expr::Wildcard {
            if item.alias.is_some() {
                return Err(Error::new("SELECT * cannot have an alias"));
            }
            expanded.extend(table.schema.iter().map(|column| SelectItem {
                expression: Expr::Column(column.name.clone()),
                alias: None,
            }));
        } else {
            expanded.push(item);
        }
    }
    Ok(expanded)
}

fn select_aliases(items: &[SelectItem]) -> Result<HashMap<String, Expr>> {
    let mut aliases = HashMap::new();
    for item in items {
        if let Some(alias) = &item.alias
            && aliases
                .insert(canonical(alias), item.expression.clone())
                .is_some()
        {
            return Err(Error::new(format!("duplicate SELECT alias '{alias}'")));
        }
    }
    Ok(aliases)
}

fn validate_select(select: &Select, table: &Table) -> Result<()> {
    if let Some(filter) = &select.filter {
        validate_expr(filter, table, &HashMap::new(), false)?;
        if has_aggregate(filter) {
            return Err(Error::new("aggregate functions are not allowed in WHERE"));
        }
    }
    for group in &select.group_by {
        validate_expr(group, table, &HashMap::new(), false)?;
        if has_aggregate(group) {
            return Err(Error::new(
                "aggregate functions are not allowed in GROUP BY",
            ));
        }
    }
    for item in &select.items {
        validate_expr(&item.expression, table, &HashMap::new(), false)?;
    }
    let aliases = select_aliases(&select.items)?;
    if let Some(having) = &select.having {
        validate_expr(having, table, &aliases, true)?;
    }
    for order in &select.order_by {
        validate_expr(&order.expression, table, &aliases, true)?;
    }

    let aggregate_query = !select.group_by.is_empty()
        || select
            .items
            .iter()
            .any(|item| has_aggregate(&item.expression))
        || select.having.as_ref().is_some_and(has_aggregate)
        || select
            .order_by
            .iter()
            .any(|item| has_aggregate(&item.expression));
    if aggregate_query {
        for item in &select.items {
            validate_group_dependency(&item.expression, &select.group_by, &aliases)?;
        }
        if let Some(having) = &select.having {
            validate_group_dependency(having, &select.group_by, &aliases)?;
        }
        for order in &select.order_by {
            validate_group_dependency(&order.expression, &select.group_by, &aliases)?;
        }
    }
    Ok(())
}

fn validate_expr(
    expression: &Expr,
    table: &Table,
    aliases: &HashMap<String, Expr>,
    allow_alias: bool,
) -> Result<()> {
    fn walk(
        expression: &Expr,
        table: &Table,
        aliases: &HashMap<String, Expr>,
        allow_alias: bool,
        inside_aggregate: bool,
    ) -> Result<()> {
        match expression {
            Expr::Column(name) => {
                if table.column_index(name).is_some() {
                    Ok(())
                } else if allow_alias {
                    if let Some(expression) = aliases.get(&canonical(name)) {
                        walk(expression, table, &HashMap::new(), false, inside_aggregate)
                    } else {
                        Err(Error::new(format!("unknown column or alias '{name}'")))
                    }
                } else {
                    Err(Error::new(format!("unknown column '{name}'")))
                }
            }
            Expr::Literal(_) => Ok(()),
            Expr::Wildcard => Err(Error::new(
                "'*' is only valid in the SELECT list or COUNT(*)",
            )),
            Expr::Function {
                name,
                arguments,
                distinct,
            } => {
                if !is_aggregate_name(name) {
                    return Err(Error::new(format!("unknown function '{name}'")));
                }
                if inside_aggregate {
                    return Err(Error::new("aggregate functions cannot be nested"));
                }
                validate_function_shape(name, arguments, *distinct)?;
                for argument in arguments {
                    if argument == &Expr::Wildcard {
                        continue;
                    }
                    walk(argument, table, aliases, allow_alias, true)?;
                }
                Ok(())
            }
            Expr::Binary { left, right, .. } => {
                walk(left, table, aliases, allow_alias, inside_aggregate)?;
                walk(right, table, aliases, allow_alias, inside_aggregate)
            }
            Expr::Unary { expression, .. } | Expr::IsNull { expression, .. } => {
                walk(expression, table, aliases, allow_alias, inside_aggregate)
            }
        }
    }
    walk(expression, table, aliases, allow_alias, false)
}

fn validate_group_dependency(
    expression: &Expr,
    group_by: &[Expr],
    aliases: &HashMap<String, Expr>,
) -> Result<()> {
    if group_by.contains(expression) || matches!(expression, Expr::Literal(_)) {
        return Ok(());
    }
    match expression {
        Expr::Function { .. } => Ok(()),
        Expr::Column(name) => {
            if let Some(expression) = aliases.get(&canonical(name)) {
                validate_group_dependency(expression, group_by, &HashMap::new())
            } else {
                Err(Error::new(format!(
                    "column '{name}' must appear in GROUP BY or an aggregate function"
                )))
            }
        }
        Expr::Binary { left, right, .. } => {
            validate_group_dependency(left, group_by, aliases)?;
            validate_group_dependency(right, group_by, aliases)
        }
        Expr::Unary { expression, .. } | Expr::IsNull { expression, .. } => {
            validate_group_dependency(expression, group_by, aliases)
        }
        Expr::Wildcard => Err(Error::new("invalid wildcard in grouped expression")),
        Expr::Literal(_) => Ok(()),
    }
}

fn validate_function_shape(name: &str, arguments: &[Expr], distinct: bool) -> Result<()> {
    match name {
        "count" if arguments.len() <= 1 => {
            if distinct && (arguments.is_empty() || arguments[0] == Expr::Wildcard) {
                Err(Error::new(
                    "COUNT(DISTINCT ...) requires a column expression",
                ))
            } else {
                Ok(())
            }
        }
        "sum" | "min" | "max" | "avg" if arguments.len() == 1 => {
            if arguments[0] == Expr::Wildcard {
                Err(Error::new(format!("{name}(*) is not supported")))
            } else {
                Ok(())
            }
        }
        "count" => Err(Error::new("COUNT expects zero or one argument")),
        "sum" | "min" | "max" | "avg" => {
            Err(Error::new(format!("{name} expects exactly one argument")))
        }
        _ => Err(Error::new(format!("unknown function '{name}'"))),
    }
}

fn has_aggregate(expression: &Expr) -> bool {
    match expression {
        Expr::Function { name, .. } => is_aggregate_name(name),
        Expr::Binary { left, right, .. } => has_aggregate(left) || has_aggregate(right),
        Expr::Unary { expression, .. } | Expr::IsNull { expression, .. } => {
            has_aggregate(expression)
        }
        _ => false,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
}

fn build_groups(table: &Table, group_by: &[Expr], rows: Vec<usize>) -> Result<Vec<Vec<usize>>> {
    if group_by.is_empty() {
        return Ok(vec![rows]);
    }
    let mut indexes = HashMap::<Vec<ValueKey>, usize>::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for row in rows {
        let context = EvalContext::row(table, row);
        let key = group_by
            .iter()
            .map(|expression| eval(expression, &context, None).map(ValueKey::from))
            .collect::<Result<Vec<_>>>()?;
        if let Some(&index) = indexes.get(&key) {
            groups[index].push(row);
        } else {
            let index = groups.len();
            indexes.insert(key, index);
            groups.push(vec![row]);
        }
    }
    Ok(groups)
}

struct EvalContext<'a> {
    table: &'a Table,
    row: Option<usize>,
    rows: Option<&'a [usize]>,
}

impl<'a> EvalContext<'a> {
    fn row(table: &'a Table, row: usize) -> Self {
        Self {
            table,
            row: Some(row),
            rows: None,
        }
    }

    fn group(table: &'a Table, rows: &'a [usize]) -> Self {
        Self {
            table,
            row: rows.first().copied(),
            rows: Some(rows),
        }
    }
}

fn eval(
    expression: &Expr,
    context: &EvalContext<'_>,
    aliases: Option<&HashMap<String, Expr>>,
) -> Result<Value> {
    match expression {
        Expr::Column(name) => {
            if let Some(alias_expression) =
                aliases.and_then(|aliases| aliases.get(&canonical(name)))
            {
                return eval(alias_expression, context, None);
            }
            let column = context
                .table
                .column_index(name)
                .ok_or_else(|| Error::new(format!("unknown column '{name}'")))?;
            Ok(context
                .row
                .map_or(Value::Null, |row| context.table.value(column, row)))
        }
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Wildcard => Err(Error::new("wildcard cannot be evaluated as a value")),
        Expr::Function {
            name,
            arguments,
            distinct,
        } => aggregate(name, arguments, *distinct, context),
        Expr::Binary {
            left,
            operator,
            right,
        } => {
            let left = eval(left, context, aliases)?;
            let right = eval(right, context, aliases)?;
            eval_binary(left, *operator, right)
        }
        Expr::Unary {
            operator,
            expression,
        } => eval_unary(*operator, eval(expression, context, aliases)?),
        Expr::IsNull {
            expression,
            negated,
        } => Ok(Value::Bool(
            (eval(expression, context, aliases)? != Value::Null) == *negated,
        )),
    }
}

fn aggregate(
    name: &str,
    arguments: &[Expr],
    distinct: bool,
    context: &EvalContext<'_>,
) -> Result<Value> {
    let rows = context
        .rows
        .ok_or_else(|| Error::new(format!("aggregate '{name}' requires a group context")))?;
    validate_function_shape(name, arguments, distinct)?;
    if name == "count" && (arguments.is_empty() || arguments[0] == Expr::Wildcard) {
        return i64::try_from(rows.len())
            .map(Value::Int64)
            .map_err(|_| Error::new("COUNT result exceeds Int64"));
    }

    let argument = &arguments[0];
    let mut values = Vec::with_capacity(rows.len());
    let mut seen = HashSet::new();
    for &row in rows {
        let value = eval(argument, &EvalContext::row(context.table, row), None)?;
        if value == Value::Null {
            continue;
        }
        if !distinct || seen.insert(ValueKey::from(value.clone())) {
            values.push(value);
        }
    }
    match name {
        "count" => i64::try_from(values.len())
            .map(Value::Int64)
            .map_err(|_| Error::new("COUNT result exceeds Int64")),
        "sum" => sum(&values),
        "min" => minimum_or_maximum(&values, false),
        "max" => minimum_or_maximum(&values, true),
        "avg" => average(&values),
        _ => Err(Error::new(format!("unknown aggregate '{name}'"))),
    }
}

fn sum(values: &[Value]) -> Result<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    match first {
        Value::Int64(_) => {
            let mut sum = 0i64;
            for value in values {
                let Value::Int64(value) = value else {
                    return Err(Error::new("SUM arguments have inconsistent types"));
                };
                sum = sum
                    .checked_add(*value)
                    .ok_or_else(|| Error::new("Int64 overflow in SUM"))?;
            }
            Ok(Value::Int64(sum))
        }
        Value::Float64(_) => {
            let mut sum = 0.0;
            for value in values {
                let Value::Float64(value) = value else {
                    return Err(Error::new("SUM arguments have inconsistent types"));
                };
                sum += value;
            }
            finite_float(sum, "SUM")
        }
        other => Err(Error::new(format!(
            "SUM requires a numeric argument, got {}",
            other.data_type().expect("non-null")
        ))),
    }
}

fn average(values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let mut sum = 0.0;
    for value in values {
        sum += match value {
            Value::Int64(value) => *value as f64,
            Value::Float64(value) => *value,
            other => {
                return Err(Error::new(format!(
                    "AVG requires a numeric argument, got {}",
                    other.data_type().expect("non-null")
                )));
            }
        };
    }
    finite_float(sum / values.len() as f64, "AVG")
}

fn minimum_or_maximum(values: &[Value], maximum: bool) -> Result<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    let mut selected = first.clone();
    for value in &values[1..] {
        let order = compare_non_null(value, &selected)?;
        if (maximum && order == Ordering::Greater) || (!maximum && order == Ordering::Less) {
            selected = value.clone();
        }
    }
    Ok(selected)
}

fn eval_unary(operator: UnaryOperator, value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    match (operator, value) {
        (UnaryOperator::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (UnaryOperator::Negate, Value::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::new("Int64 overflow in unary '-'")),
        (UnaryOperator::Negate, Value::Float64(value)) => finite_float(-value, "unary '-'"),
        (UnaryOperator::Positive, value @ (Value::Int64(_) | Value::Float64(_))) => Ok(value),
        (operator, value) => Err(Error::new(format!(
            "operator {operator:?} cannot be applied to {}",
            value.data_type().expect("non-null")
        ))),
    }
}

fn eval_binary(left: Value, operator: BinaryOperator, right: Value) -> Result<Value> {
    match operator {
        BinaryOperator::And => return boolean_binary(left, right, true),
        BinaryOperator::Or => return boolean_binary(left, right, false),
        _ => {}
    }
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    match operator {
        BinaryOperator::Equal
        | BinaryOperator::NotEqual
        | BinaryOperator::Less
        | BinaryOperator::LessEqual
        | BinaryOperator::Greater
        | BinaryOperator::GreaterEqual => {
            let ordering = compare_non_null(&left, &right)?;
            let result = match operator {
                BinaryOperator::Equal => ordering == Ordering::Equal,
                BinaryOperator::NotEqual => ordering != Ordering::Equal,
                BinaryOperator::Less => ordering == Ordering::Less,
                BinaryOperator::LessEqual => ordering != Ordering::Greater,
                BinaryOperator::Greater => ordering == Ordering::Greater,
                BinaryOperator::GreaterEqual => ordering != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply => {
            numeric_arithmetic(left, operator, right)
        }
        BinaryOperator::Divide => numeric_divide(left, right),
        BinaryOperator::Modulo => numeric_modulo(left, right),
        BinaryOperator::And | BinaryOperator::Or => unreachable!(),
    }
}

fn boolean_binary(left: Value, right: Value, and: bool) -> Result<Value> {
    let left = nullable_bool(left)?;
    let right = nullable_bool(right)?;
    let result = if and {
        match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        }
    } else {
        match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    };
    Ok(result.map_or(Value::Null, Value::Bool))
}

fn nullable_bool(value: Value) -> Result<Option<bool>> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(value) => Ok(Some(value)),
        other => Err(Error::new(format!(
            "logical operator requires Bool, got {}",
            other.data_type().expect("non-null")
        ))),
    }
}

fn numeric_arithmetic(left: Value, operator: BinaryOperator, right: Value) -> Result<Value> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => {
            let value = match operator {
                BinaryOperator::Add => left.checked_add(right),
                BinaryOperator::Subtract => left.checked_sub(right),
                BinaryOperator::Multiply => left.checked_mul(right),
                _ => unreachable!(),
            };
            value
                .map(Value::Int64)
                .ok_or_else(|| Error::new("Int64 arithmetic overflow"))
        }
        (
            left @ (Value::Int64(_) | Value::Float64(_)),
            right @ (Value::Int64(_) | Value::Float64(_)),
        ) => {
            let left = as_float(left);
            let right = as_float(right);
            let result = match operator {
                BinaryOperator::Add => left + right,
                BinaryOperator::Subtract => left - right,
                BinaryOperator::Multiply => left * right,
                _ => unreachable!(),
            };
            finite_float(result, "arithmetic expression")
        }
        (left, right) => incompatible_values(&left, &right, "arithmetic"),
    }
}

fn numeric_divide(left: Value, right: Value) -> Result<Value> {
    match (left, right) {
        (
            left @ (Value::Int64(_) | Value::Float64(_)),
            right @ (Value::Int64(_) | Value::Float64(_)),
        ) => {
            let divisor = as_float(right);
            if divisor == 0.0 {
                return Err(Error::new("division by zero"));
            }
            finite_float(as_float(left) / divisor, "division")
        }
        (left, right) => incompatible_values(&left, &right, "division"),
    }
}

fn numeric_modulo(left: Value, right: Value) -> Result<Value> {
    match (left, right) {
        (Value::Int64(_), Value::Int64(0)) => Err(Error::new("modulo by zero")),
        (Value::Int64(left), Value::Int64(right)) => left
            .checked_rem(right)
            .map(Value::Int64)
            .ok_or_else(|| Error::new("Int64 overflow in modulo")),
        (
            left @ (Value::Int64(_) | Value::Float64(_)),
            right @ (Value::Int64(_) | Value::Float64(_)),
        ) => {
            let divisor = as_float(right);
            if divisor == 0.0 {
                return Err(Error::new("modulo by zero"));
            }
            finite_float(as_float(left) % divisor, "modulo")
        }
        (left, right) => incompatible_values(&left, &right, "modulo"),
    }
}

fn as_float(value: Value) -> f64 {
    match value {
        Value::Int64(value) => value as f64,
        Value::Float64(value) => value,
        _ => unreachable!(),
    }
}

fn finite_float(value: f64, context: &str) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::Float64(value))
    } else {
        Err(Error::new(format!(
            "non-finite Float64 result in {context}"
        )))
    }
}

fn compare_non_null(left: &Value, right: &Value) -> Result<Ordering> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::Float64(left), Value::Float64(right)) => left
            .partial_cmp(right)
            .ok_or_else(|| Error::new("cannot compare NaN")),
        (Value::Int64(left), Value::Float64(right)) => (*left as f64)
            .partial_cmp(right)
            .ok_or_else(|| Error::new("cannot compare NaN")),
        (Value::Float64(left), Value::Int64(right)) => left
            .partial_cmp(&(*right as f64))
            .ok_or_else(|| Error::new("cannot compare NaN")),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Null, _) | (_, Value::Null) => unreachable!("nulls handled by caller"),
        _ => incompatible_values(left, right, "comparison"),
    }
}

fn incompatible_values<T>(left: &Value, right: &Value, context: &str) -> Result<T> {
    Err(Error::new(format!(
        "incompatible types in {context}: {} and {}",
        left.data_type().expect("non-null"),
        right.data_type().expect("non-null")
    )))
}

fn predicate(value: Value, clause: &str) -> Result<bool> {
    match value {
        Value::Bool(value) => Ok(value),
        Value::Null => Ok(false),
        other => Err(Error::new(format!(
            "{clause} expression must produce Bool, got {}",
            other.data_type().expect("non-null")
        ))),
    }
}

fn constant_value(expression: &Expr) -> Result<Value> {
    match expression {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Unary {
            operator,
            expression,
        } => eval_unary(*operator, constant_value(expression)?),
        Expr::Binary {
            left,
            operator,
            right,
        } => eval_binary(constant_value(left)?, *operator, constant_value(right)?),
        _ => Err(Error::new(
            "INSERT VALUES entries must be constant expressions",
        )),
    }
}

fn order_keys(
    order_by: &[OrderBy],
    context: &EvalContext<'_>,
    aliases: &HashMap<String, Expr>,
    values: &[Value],
) -> Result<Vec<Value>> {
    order_by
        .iter()
        .map(|order| {
            if let Expr::Literal(Value::Int64(position)) = order.expression
                && position > 0
                && usize::try_from(position).is_ok_and(|position| position <= values.len())
            {
                return Ok(values[position as usize - 1].clone());
            }
            eval(&order.expression, context, Some(aliases))
        })
        .collect()
}

#[derive(Debug)]
struct OutputRow {
    values: Vec<Value>,
    order_keys: Vec<Value>,
    ordinal: usize,
}

fn compare_output_rows(left: &OutputRow, right: &OutputRow, order_by: &[OrderBy]) -> Ordering {
    for ((left_key, right_key), order) in
        left.order_keys.iter().zip(&right.order_keys).zip(order_by)
    {
        let nulls_first = order.nulls_first.unwrap_or(!order.ascending);
        let comparison = match (left_key, right_key) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => compare_non_null(left_key, right_key).unwrap_or(Ordering::Equal),
        };
        let comparison = if order.ascending || left_key == &Value::Null || right_key == &Value::Null
        {
            comparison
        } else {
            comparison.reverse()
        };
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    left.ordinal.cmp(&right.ordinal)
}

#[derive(Clone, Debug, Eq)]
enum ValueKey {
    Null,
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(String),
}

impl ValueKey {
    fn row(values: &[Value]) -> Vec<Self> {
        values.iter().cloned().map(Self::from).collect()
    }
}

impl From<Value> for ValueKey {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Int64(value) => Self::Int64(value),
            Value::Float64(value) => Self::Float64(if value == 0.0 { 0 } else { value.to_bits() }),
            Value::Bool(value) => Self::Bool(value),
            Value::String(value) => Self::String(value),
        }
    }
}

impl PartialEq for ValueKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Int64(left), Self::Int64(right)) => left == right,
            (Self::Float64(left), Self::Float64(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            _ => false,
        }
    }
}

impl Hash for ValueKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Int64(value) => value.hash(state),
            Self::Float64(value) => value.hash(state),
            Self::Bool(value) => value.hash(state),
            Self::String(value) => value.hash(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut engine = Engine::new();
        engine
            .execute(
                "CREATE TABLE events (\
                    id Int64, category String, amount Float64, active Bool, score Nullable(Int64)\
                 );\
                 INSERT INTO events VALUES\
                    (1, 'a', 10.5, true, 4),\
                    (2, 'b', 7.0, false, NULL),\
                    (3, 'a', 2.5, true, 8),\
                    (4, 'b', 3.0, true, 2),\
                    (5, 'c', 9.0, false, NULL);",
            )
            .unwrap();
        engine
    }

    #[test]
    fn filters_projects_aliases_and_sorts_multiple_keys() {
        let result = engine()
            .execute(
                "SELECT category AS kind, id, amount * 2 AS doubled \
                 FROM events \
                 WHERE active = true AND (score >= 4 OR score IS NULL) \
                 ORDER BY kind ASC, id DESC LIMIT 2",
            )
            .unwrap()
            .remove(0);
        assert_eq!(result.columns, ["kind", "id", "doubled"]);
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::String("a".to_owned()),
                    Value::Int64(3),
                    Value::Float64(5.0)
                ],
                vec![
                    Value::String("a".to_owned()),
                    Value::Int64(1),
                    Value::Float64(21.0)
                ]
            ]
        );
    }

    #[test]
    fn computes_groups_aggregates_having_and_distinct() {
        let result = engine()
            .execute(
                "SELECT category, count(*) AS n, sum(amount) AS total, \
                        min(score) AS lo, max(score) AS hi, avg(score) AS mean \
                 FROM events GROUP BY category HAVING n >= 2 \
                 ORDER BY total DESC, category ASC",
            )
            .unwrap()
            .remove(0);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::String("a".to_owned()));
        assert_eq!(result.rows[0][1], Value::Int64(2));
        assert_eq!(result.rows[0][2], Value::Float64(13.0));
        assert_eq!(result.rows[0][3], Value::Int64(4));
        assert_eq!(result.rows[0][4], Value::Int64(8));
        assert_eq!(result.rows[0][5], Value::Float64(6.0));

        let distinct = engine()
            .execute("SELECT DISTINCT active FROM events ORDER BY active")
            .unwrap()
            .remove(0);
        assert_eq!(
            distinct.rows,
            vec![vec![Value::Bool(false)], vec![Value::Bool(true)]]
        );
    }

    #[test]
    fn null_semantics_and_empty_aggregates_match_sql() {
        let mut engine = engine();
        let result = engine
            .execute(
                "SELECT count(*), count(score), sum(score), min(score), max(score), avg(score)\
                 FROM events WHERE id > 100",
            )
            .unwrap()
            .remove(0);
        assert_eq!(
            result.rows,
            vec![vec![
                Value::Int64(0),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null
            ]]
        );
        let result = engine
            .execute("SELECT id FROM events WHERE score = NULL")
            .unwrap()
            .remove(0);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn failed_insert_does_not_change_the_table() {
        let mut engine = engine();
        assert!(
            engine
                .execute(
                    "INSERT INTO events VALUES (6, 'x', 1.0, true, 1), (7, 'y', 'bad', true, 2)"
                )
                .is_err()
        );
        let result = engine
            .execute("SELECT count(*) AS n FROM events")
            .unwrap()
            .remove(0);
        assert_eq!(result.rows, vec![vec![Value::Int64(5)]]);
    }

    #[test]
    fn rejects_invalid_grouping_and_unknown_columns() {
        let mut engine = engine();
        let error = engine
            .execute("SELECT category, sum(amount) FROM events")
            .unwrap_err();
        assert!(error.message().contains("GROUP BY"));
        let error = engine.execute("SELECT missing FROM events").unwrap_err();
        assert!(error.message().contains("unknown column"));
    }
}
