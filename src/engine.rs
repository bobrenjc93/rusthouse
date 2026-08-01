use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::MAX_INPUT_BYTES;
use crate::ast::{BinaryOp, Expr, OrderItem, Select, SelectItem, Statement, UnaryOp};
use crate::error::{Error, Result};
use crate::identifier::{Identifier, ObjectName};
use crate::parser;
use crate::storage::{Catalog, Table};
use crate::value::{DataType, Value};

pub const MAX_RESULT_ROWS: usize = 5_000_000;
pub const MAX_MATERIALIZED_RESULT_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_GROUP_BY_EXPRESSIONS: usize = 64;

/// Metadata for one query-result column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: Option<DataType>,
    pub nullable: bool,
}

/// A materialized result produced by a SELECT statement.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

/// A session-local analytical database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses and executes one or more semicolon-separated SQL statements.
    ///
    /// CREATE and INSERT statements mutate this database. One result is returned
    /// for each SELECT, in statement order.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        ensure_sql_input_limit(sql.len())?;
        let statements = parser::parse(sql)?;
        let mut results = Vec::new();
        let mut result_bytes = 0_usize;
        for statement in statements {
            match statement {
                Statement::CreateTable {
                    name,
                    columns,
                    if_not_exists,
                } => self.catalog.create_table(name, columns, if_not_exists)?,
                Statement::Insert {
                    table,
                    columns,
                    rows,
                } => {
                    self.catalog
                        .table_mut(&table)?
                        .insert_rows(columns.as_deref(), rows)?;
                }
                Statement::Select(select) => {
                    let result = self.select(
                        select,
                        MAX_MATERIALIZED_RESULT_BYTES.saturating_sub(result_bytes),
                    )?;
                    result_bytes = result_bytes.saturating_add(result_size(&result));
                    if result_bytes > MAX_MATERIALIZED_RESULT_BYTES {
                        return Err(Error::Limit {
                            resource: "materialized query result bytes",
                            limit: MAX_MATERIALIZED_RESULT_BYTES,
                        });
                    }
                    results.push(result);
                }
            }
        }
        Ok(results)
    }

    fn select(&self, mut query: Select, byte_limit: usize) -> Result<QueryResult> {
        let table = query
            .table
            .as_ref()
            .map(|name| self.catalog.table(name))
            .transpose()?;
        query.projection = expand_wildcard(query.projection, table)?;
        if query.group_by.len() > MAX_GROUP_BY_EXPRESSIONS {
            return Err(Error::Limit {
                resource: "GROUP BY expressions",
                limit: MAX_GROUP_BY_EXPRESSIONS,
            });
        }
        let projection_names = projection_names(&query.projection)?;

        let aggregate_query = !query.group_by.is_empty()
            || query
                .projection
                .iter()
                .any(|item| item.expr.contains_aggregate())
            || query.having.as_ref().is_some_and(Expr::contains_aggregate)
            || query
                .order_by
                .iter()
                .any(|item| item.expr.contains_aggregate());

        validate_aggregate_query(&query, aggregate_query, &projection_names)?;
        let projection_types = bind_query(&query, table, &projection_names)?;

        let mut filtered_rows = Vec::new();
        let input_rows = table.map_or(1, Table::row_count);
        for row in 0..input_rows {
            let include = if let Some(predicate) = &query.selection {
                eval_expr(
                    predicate,
                    &EvalContext {
                        table,
                        row: Some(row),
                        group: None,
                        aliases: None,
                    },
                )?
                .as_bool("WHERE")?
                .unwrap_or(false)
            } else {
                true
            };
            if include {
                filtered_rows.push(row);
            }
        }

        let mut records = Vec::new();
        let mut materialized_bytes = filtered_rows
            .len()
            .saturating_mul(std::mem::size_of::<usize>());
        ensure_materialized_limit(materialized_bytes, byte_limit)?;
        let evaluator = RecordEvaluator {
            query: &query,
            table,
            aggregate_query,
            projection_names: &projection_names,
            byte_limit,
        };
        if aggregate_query {
            let groups = if query.group_by.is_empty() {
                vec![filtered_rows]
            } else {
                build_groups(
                    table,
                    &filtered_rows,
                    &query.group_by,
                    &mut materialized_bytes,
                    byte_limit,
                )?
            };
            for group in groups {
                if let Some(record) =
                    evaluate_record(&evaluator, &group, records.len(), materialized_bytes)?
                {
                    push_record(&mut records, record, &mut materialized_bytes, byte_limit)?;
                }
            }
        } else {
            let early_stop = query
                .limit
                .filter(|_| query.order_by.is_empty() && !query.distinct)
                .map(|limit| query.offset.saturating_add(limit));
            for row in filtered_rows {
                if early_stop.is_some_and(|count| records.len() >= count) {
                    break;
                }
                if let Some(record) = evaluate_record(
                    &evaluator,
                    std::slice::from_ref(&row),
                    records.len(),
                    materialized_bytes,
                )? {
                    push_record(&mut records, record, &mut materialized_bytes, byte_limit)?;
                }
            }
        }

        if query.distinct {
            apply_distinct(&mut records, materialized_bytes, byte_limit)?;
        }
        if !query.order_by.is_empty() {
            records.sort_by(|left, right| compare_records(left, right, &query.order_by));
        }

        let start = query.offset.min(records.len());
        let end = query.limit.map_or(records.len(), |limit| {
            start.saturating_add(limit).min(records.len())
        });
        records.truncate(end);
        let rows = records
            .into_iter()
            .skip(start)
            .map(|record| record.values)
            .collect::<Vec<_>>();
        let columns = query
            .projection
            .iter()
            .zip(projection_types)
            .map(|(item, expression_type)| ResultColumn {
                name: item
                    .alias
                    .as_ref()
                    .map(|alias| alias.value.clone())
                    .unwrap_or_else(|| item.expr.display_name()),
                data_type: expression_type.data_type,
                nullable: expression_type.nullable,
            })
            .collect::<Vec<_>>();
        let result = QueryResult { columns, rows };
        ensure_materialized_limit(result_size(&result), byte_limit)?;
        Ok(result)
    }
}

fn ensure_sql_input_limit(bytes: usize) -> Result<()> {
    if bytes > MAX_INPUT_BYTES {
        return Err(Error::Limit {
            resource: "SQL input bytes",
            limit: MAX_INPUT_BYTES,
        });
    }
    Ok(())
}

struct RecordEvaluator<'a> {
    query: &'a Select,
    table: Option<&'a Table>,
    aggregate_query: bool,
    projection_names: &'a HashMap<String, usize>,
    byte_limit: usize,
}

fn evaluate_record(
    evaluator: &RecordEvaluator<'_>,
    group: &[usize],
    sequence: usize,
    materialized_bytes: usize,
) -> Result<Option<Record>> {
    let base_context = EvalContext {
        table: evaluator.table,
        row: group.first().copied(),
        group: evaluator.aggregate_query.then_some(group),
        aliases: None,
    };
    let mut pending_bytes = std::mem::size_of::<Record>().saturating_mul(2);
    let mut values = Vec::with_capacity(evaluator.query.projection.len());
    for item in &evaluator.query.projection {
        let value = eval_expr(&item.expr, &base_context)?;
        pending_bytes = pending_bytes.saturating_add(value_size(&value));
        ensure_materialized_limit(
            materialized_bytes.saturating_add(pending_bytes),
            evaluator.byte_limit,
        )?;
        values.push(value);
    }
    let aliases = result_aliases(evaluator.projection_names, &values);
    let context = EvalContext {
        aliases: Some(&aliases),
        ..base_context
    };
    if let Some(having) = &evaluator.query.having
        && !eval_expr(having, &context)?
            .as_bool("HAVING")?
            .unwrap_or(false)
    {
        return Ok(None);
    }
    let mut order_values = Vec::with_capacity(evaluator.query.order_by.len());
    for item in &evaluator.query.order_by {
        let value = eval_order_expr(item, &context, &values)?;
        pending_bytes = pending_bytes.saturating_add(value_size(&value));
        ensure_materialized_limit(
            materialized_bytes.saturating_add(pending_bytes),
            evaluator.byte_limit,
        )?;
        order_values.push(value);
    }
    Ok(Some(Record {
        values,
        order_values,
        sequence,
    }))
}

fn push_record(
    records: &mut Vec<Record>,
    record: Record,
    materialized_bytes: &mut usize,
    byte_limit: usize,
) -> Result<()> {
    let record_bytes = record
        .values
        .iter()
        .chain(&record.order_values)
        .map(value_size)
        .sum::<usize>()
        .saturating_add(std::mem::size_of::<Record>().saturating_mul(2));
    add_materialized_bytes(materialized_bytes, record_bytes, byte_limit)?;
    records.push(record);
    if records.len() > MAX_RESULT_ROWS {
        return Err(Error::Limit {
            resource: "materialized result rows",
            limit: MAX_RESULT_ROWS,
        });
    }
    Ok(())
}

fn add_materialized_bytes(total: &mut usize, bytes: usize, byte_limit: usize) -> Result<()> {
    *total = total.saturating_add(bytes);
    ensure_materialized_limit(*total, byte_limit)
}

fn ensure_materialized_limit(bytes: usize, byte_limit: usize) -> Result<()> {
    if bytes > byte_limit {
        return Err(Error::Limit {
            resource: "materialized query result bytes",
            limit: MAX_MATERIALIZED_RESULT_BYTES,
        });
    }
    Ok(())
}

fn apply_distinct(records: &mut Vec<Record>, base_bytes: usize, byte_limit: usize) -> Result<()> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    let mut materialized_bytes = base_bytes;
    for record in records.drain(..) {
        let key = RowKey::from_values(&record.values);
        if seen.contains(&key) {
            continue;
        }
        add_materialized_bytes(
            &mut materialized_bytes,
            row_key_size(&key)
                .saturating_add(std::mem::size_of::<RowKey>())
                .saturating_add(64),
            byte_limit,
        )?;
        seen.insert(key);
        unique.push(record);
    }
    *records = unique;
    Ok(())
}

fn row_key_size(key: &RowKey) -> usize {
    key.0
        .iter()
        .map(|value| match value {
            KeyValue::String(value) => value
                .capacity()
                .saturating_add(std::mem::size_of::<KeyValue>()),
            _ => std::mem::size_of::<KeyValue>(),
        })
        .fold(0_usize, usize::saturating_add)
}

fn value_size(value: &Value) -> usize {
    match value {
        Value::String(value) => value
            .capacity()
            .saturating_add(std::mem::size_of::<Value>()),
        _ => std::mem::size_of::<Value>(),
    }
}

fn result_size(result: &QueryResult) -> usize {
    let mut bytes = std::mem::size_of::<QueryResult>().saturating_mul(2);
    bytes = bytes.saturating_add(
        result
            .columns
            .capacity()
            .saturating_mul(std::mem::size_of::<ResultColumn>()),
    );
    for column in &result.columns {
        bytes = bytes.saturating_add(column.name.capacity());
    }
    bytes = bytes.saturating_add(
        result
            .rows
            .capacity()
            .saturating_mul(std::mem::size_of::<Vec<Value>>()),
    );
    for row in &result.rows {
        bytes = bytes.saturating_add(row.capacity().saturating_mul(std::mem::size_of::<Value>()));
        for value in row {
            if let Value::String(value) = value {
                bytes = bytes.saturating_add(value.capacity());
            }
        }
    }
    bytes
}

#[derive(Clone, Copy)]
struct ExprType {
    data_type: Option<DataType>,
    nullable: bool,
}

fn projection_names(items: &[SelectItem]) -> Result<HashMap<String, usize>> {
    let mut names = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if let Some(alias) = &item.alias
            && names.insert(alias.lookup_key(), index).is_some()
        {
            return Err(Error::execution(format!(
                "ambiguous projection alias '{}'",
                alias.value
            )));
        }
    }
    for (index, item) in items.iter().enumerate() {
        if item.alias.is_none()
            && let Expr::Column(parts) = &item.expr
            && parts.len() == 1
        {
            names.entry(parts[0].lookup_key()).or_insert(index);
        }
    }
    Ok(names)
}

fn projection_expr_aliases<'a>(
    query: &'a Select,
    projection_names: &HashMap<String, usize>,
) -> HashMap<String, &'a Expr> {
    projection_names
        .iter()
        .map(|(name, index)| (name.clone(), &query.projection[*index].expr))
        .collect()
}

fn bind_query(
    query: &Select,
    table: Option<&Table>,
    projection_names: &HashMap<String, usize>,
) -> Result<Vec<ExprType>> {
    let table_name = query.table.as_ref();
    let aliases = projection_expr_aliases(query, projection_names);
    let no_aliases = HashMap::new();

    let projection_types = query
        .projection
        .iter()
        .map(|item| bind_expr(&item.expr, table, table_name, &no_aliases, false))
        .collect::<Result<Vec<_>>>()?;

    if let Some(selection) = &query.selection {
        let expression_type = bind_expr(selection, table, table_name, &no_aliases, false)?;
        require_bool(expression_type, "WHERE")?;
    }
    for expression in &query.group_by {
        bind_expr(expression, table, table_name, &no_aliases, false)?;
    }
    if let Some(having) = &query.having {
        let expression_type = bind_expr(having, table, table_name, &aliases, true)?;
        require_bool(expression_type, "HAVING")?;
    }
    for item in &query.order_by {
        if let Expr::Literal(Value::Int64(position)) = &item.expr {
            order_position(*position, query.projection.len())?;
        } else {
            bind_expr(&item.expr, table, table_name, &aliases, true)?;
        }
    }
    Ok(projection_types)
}

fn bind_expr(
    expression: &Expr,
    table: Option<&Table>,
    table_name: Option<&ObjectName>,
    aliases: &HashMap<String, &Expr>,
    resolve_alias: bool,
) -> Result<ExprType> {
    match expression {
        Expr::Literal(value) => Ok(ExprType {
            data_type: value.data_type(),
            nullable: matches!(value, Value::Null),
        }),
        Expr::Column(parts) => {
            let identifier = parts
                .last()
                .ok_or_else(|| Error::execution("empty column reference"))?;
            if resolve_alias
                && parts.len() == 1
                && let Some(aliased) = aliases.get(&identifier.lookup_key())
                && *aliased != expression
            {
                return bind_expr(aliased, table, table_name, aliases, false);
            }
            validate_qualifier(parts, table_name)?;
            let table = table.ok_or_else(|| {
                Error::execution(format!("column '{}' requires a table", identifier.value))
            })?;
            let column = table.column_schema(identifier)?;
            Ok(ExprType {
                data_type: Some(column.data_type),
                nullable: column.nullable,
            })
        }
        Expr::Wildcard => Err(Error::execution(
            "wildcard is only valid in SELECT or COUNT",
        )),
        Expr::Unary { op, expr } => {
            let expression_type = bind_expr(expr, table, table_name, aliases, resolve_alias)?;
            match op {
                UnaryOp::Plus | UnaryOp::Minus => {
                    require_numeric(expression_type, "unary +/-")?;
                    Ok(expression_type)
                }
                UnaryOp::Not => {
                    require_bool(expression_type, "NOT")?;
                    Ok(ExprType {
                        data_type: Some(DataType::Bool),
                        nullable: expression_type.nullable,
                    })
                }
            }
        }
        Expr::Binary { left, op, right } => {
            let left = bind_expr(left, table, table_name, aliases, resolve_alias)?;
            let right = bind_expr(right, table, table_name, aliases, resolve_alias)?;
            let nullable = left.nullable || right.nullable;
            match op {
                BinaryOp::Add
                | BinaryOp::Subtract
                | BinaryOp::Multiply
                | BinaryOp::Divide
                | BinaryOp::Modulo => {
                    require_numeric(left, "arithmetic")?;
                    require_numeric(right, "arithmetic")?;
                    let data_type = if matches!(left.data_type, Some(DataType::Float64))
                        || matches!(right.data_type, Some(DataType::Float64))
                    {
                        Some(DataType::Float64)
                    } else if left.data_type.is_none() && right.data_type.is_none() {
                        None
                    } else {
                        Some(DataType::Int64)
                    };
                    Ok(ExprType {
                        data_type,
                        nullable,
                    })
                }
                BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterEqual => {
                    require_comparable(left, right)?;
                    Ok(ExprType {
                        data_type: Some(DataType::Bool),
                        nullable,
                    })
                }
                BinaryOp::And | BinaryOp::Or => {
                    require_bool(left, "AND/OR")?;
                    require_bool(right, "AND/OR")?;
                    Ok(ExprType {
                        data_type: Some(DataType::Bool),
                        nullable,
                    })
                }
            }
        }
        Expr::Function { name, args } => bind_aggregate(name, args, table, table_name, aliases),
        Expr::IsNull { expr, .. } => {
            bind_expr(expr, table, table_name, aliases, resolve_alias)?;
            Ok(ExprType {
                data_type: Some(DataType::Bool),
                nullable: false,
            })
        }
    }
}

fn bind_aggregate(
    name: &str,
    args: &[Expr],
    table: Option<&Table>,
    table_name: Option<&ObjectName>,
    aliases: &HashMap<String, &Expr>,
) -> Result<ExprType> {
    let normalized = name.to_ascii_lowercase();
    if !is_aggregate_function(&normalized) {
        return Err(Error::execution(format!("unsupported function '{name}'")));
    }
    if normalized == "count" && args.is_empty() {
        return Ok(ExprType {
            data_type: Some(DataType::Int64),
            nullable: false,
        });
    }
    if args.len() != 1 {
        return Err(Error::execution(format!(
            "aggregate '{name}' expects exactly one argument"
        )));
    }
    if matches!(args[0], Expr::Wildcard) {
        return if normalized == "count" {
            Ok(ExprType {
                data_type: Some(DataType::Int64),
                nullable: false,
            })
        } else {
            Err(Error::execution(format!("'{name}(*)' is not supported")))
        };
    }

    let argument = bind_expr(&args[0], table, table_name, aliases, false)?;
    match normalized.as_str() {
        "count" => Ok(ExprType {
            data_type: Some(DataType::Int64),
            nullable: false,
        }),
        "sum" => {
            require_numeric(argument, "SUM")?;
            Ok(ExprType {
                data_type: argument.data_type,
                nullable: true,
            })
        }
        "avg" => {
            require_numeric(argument, "AVG")?;
            Ok(ExprType {
                data_type: Some(DataType::Float64),
                nullable: true,
            })
        }
        "min" | "max" => Ok(ExprType {
            data_type: argument.data_type,
            nullable: true,
        }),
        _ => unreachable!(),
    }
}

fn validate_qualifier(parts: &[Identifier], table_name: Option<&ObjectName>) -> Result<()> {
    if parts.len() <= 1 {
        return Ok(());
    }
    let qualifier = &parts[..parts.len() - 1];
    if table_name.is_some_and(|name| name.0.as_slice() == qualifier) {
        return Ok(());
    }
    Err(Error::execution(format!(
        "column qualifier '{}' does not match the selected table",
        qualifier
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>()
            .join(".")
    )))
}

fn require_numeric(expression_type: ExprType, context: &str) -> Result<()> {
    if expression_type.data_type.is_none()
        || matches!(
            expression_type.data_type,
            Some(DataType::Int64 | DataType::Float64)
        )
    {
        Ok(())
    } else {
        Err(Error::Type(format!(
            "{context} requires a numeric expression"
        )))
    }
}

fn require_bool(expression_type: ExprType, context: &str) -> Result<()> {
    if expression_type.data_type.is_none()
        || matches!(expression_type.data_type, Some(DataType::Bool))
    {
        Ok(())
    } else {
        Err(Error::Type(format!("{context} requires a Bool expression")))
    }
}

fn require_comparable(left: ExprType, right: ExprType) -> Result<()> {
    let compatible = match (left.data_type, right.data_type) {
        (None, _) | (_, None) => true,
        (Some(left), Some(right)) if left == right => true,
        (Some(DataType::Int64 | DataType::Float64), Some(DataType::Int64 | DataType::Float64)) => {
            true
        }
        (Some(DataType::Bool), Some(DataType::Int64))
        | (Some(DataType::Int64), Some(DataType::Bool)) => true,
        _ => false,
    };
    if compatible {
        Ok(())
    } else {
        Err(Error::Type(
            "comparison operands have incompatible types".to_owned(),
        ))
    }
}

fn expand_wildcard(projection: Vec<SelectItem>, table: Option<&Table>) -> Result<Vec<SelectItem>> {
    let mut expanded = Vec::new();
    for item in projection {
        if matches!(item.expr, Expr::Wildcard) {
            if item.alias.is_some() {
                return Err(Error::execution(
                    "a wildcard projection cannot have an alias",
                ));
            }
            let table = table.ok_or_else(|| Error::execution("SELECT * requires a table"))?;
            expanded.extend(table.schema.iter().map(|column| SelectItem {
                expr: Expr::Column(vec![Identifier {
                    value: column.name.clone(),
                    quoted: column.quoted,
                }]),
                alias: None,
            }));
        } else {
            expanded.push(item);
        }
    }
    Ok(expanded)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum KeyValue {
    Null,
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(String),
}

impl KeyValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Int64(value) => Self::Int64(*value),
            Value::Float64(value) => Self::Float64(if *value == 0.0 { 0 } else { value.to_bits() }),
            Value::Bool(value) => Self::Bool(*value),
            Value::String(value) => Self::String(value.clone()),
        }
    }

    fn from_owned(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Int64(value) => Self::Int64(value),
            Value::Float64(value) => Self::Float64(if value == 0.0 { 0 } else { value.to_bits() }),
            Value::Bool(value) => Self::Bool(value),
            Value::String(value) => Self::String(value),
        }
    }

    fn heap_size(&self) -> usize {
        match self {
            Self::String(value) => value.capacity(),
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RowKey(Vec<KeyValue>);

impl RowKey {
    fn from_values(values: &[Value]) -> Self {
        Self(values.iter().map(KeyValue::from_value).collect())
    }
}

fn build_groups(
    table: Option<&Table>,
    rows: &[usize],
    expressions: &[Expr],
    materialized_bytes: &mut usize,
    byte_limit: usize,
) -> Result<Vec<Vec<usize>>> {
    let mut lookup: HashMap<RowKey, usize> = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &row in rows {
        let context = EvalContext {
            table,
            row: Some(row),
            group: None,
            aliases: None,
        };
        let mut candidate_bytes = std::mem::size_of::<RowKey>().saturating_add(
            expressions
                .len()
                .saturating_mul(std::mem::size_of::<KeyValue>()),
        );
        ensure_materialized_limit(
            materialized_bytes.saturating_add(candidate_bytes),
            byte_limit,
        )?;
        let mut fields = Vec::with_capacity(expressions.len());
        for expression in expressions {
            let field = KeyValue::from_owned(eval_expr(expression, &context)?);
            candidate_bytes = candidate_bytes.saturating_add(field.heap_size());
            ensure_materialized_limit(
                materialized_bytes.saturating_add(candidate_bytes),
                byte_limit,
            )?;
            fields.push(field);
        }
        let key = RowKey(fields);
        if let Some(index) = lookup.get(&key).copied() {
            ensure_materialized_limit(
                materialized_bytes
                    .saturating_add(candidate_bytes)
                    .saturating_add(std::mem::size_of::<usize>()),
                byte_limit,
            )?;
            add_materialized_bytes(materialized_bytes, std::mem::size_of::<usize>(), byte_limit)?;
            groups[index].push(row);
        } else {
            if groups.len() >= MAX_RESULT_ROWS {
                return Err(Error::Limit {
                    resource: "groups",
                    limit: MAX_RESULT_ROWS,
                });
            }
            let group_bytes = row_key_size(&key)
                .saturating_add(std::mem::size_of::<RowKey>())
                .saturating_add(std::mem::size_of::<Vec<usize>>())
                .saturating_add(std::mem::size_of::<usize>())
                .saturating_add(64);
            add_materialized_bytes(materialized_bytes, group_bytes, byte_limit)?;
            let index = groups.len();
            lookup.insert(key, index);
            groups.push(vec![row]);
        }
    }
    Ok(groups)
}

fn validate_aggregate_query(
    query: &Select,
    aggregate_query: bool,
    projection_names: &HashMap<String, usize>,
) -> Result<()> {
    if query
        .selection
        .as_ref()
        .is_some_and(Expr::contains_aggregate)
    {
        return Err(Error::execution(
            "aggregate functions are not allowed in WHERE",
        ));
    }
    if query.group_by.iter().any(Expr::contains_aggregate) {
        return Err(Error::execution(
            "aggregate functions are not allowed in GROUP BY",
        ));
    }
    if !aggregate_query {
        return Ok(());
    }

    let aliases = projection_expr_aliases(query, projection_names);
    for item in &query.projection {
        validate_group_expression(&item.expr, &query.group_by, &aliases, false, false)?;
    }
    if let Some(having) = &query.having {
        validate_group_expression(having, &query.group_by, &aliases, false, true)?;
    }
    for item in &query.order_by {
        if !matches!(item.expr, Expr::Literal(Value::Int64(position)) if position > 0) {
            validate_group_expression(&item.expr, &query.group_by, &aliases, false, true)?;
        }
    }
    Ok(())
}

fn validate_group_expression(
    expression: &Expr,
    group_by: &[Expr],
    aliases: &HashMap<String, &Expr>,
    inside_aggregate: bool,
    resolve_alias: bool,
) -> Result<()> {
    if !inside_aggregate
        && group_by
            .iter()
            .any(|group| group_expressions_equivalent(group, expression))
    {
        return Ok(());
    }

    match expression {
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => {
            validate_group_expression(expr, group_by, aliases, inside_aggregate, resolve_alias)
        }
        Expr::Binary { left, right, .. } => {
            validate_group_expression(left, group_by, aliases, inside_aggregate, resolve_alias)?;
            validate_group_expression(right, group_by, aliases, inside_aggregate, resolve_alias)
        }
        Expr::Function { name, args } if is_aggregate_function(name) => {
            if inside_aggregate {
                return Err(Error::execution("aggregate functions cannot be nested"));
            }
            for argument in args {
                validate_group_expression(argument, group_by, aliases, true, false)?;
            }
            Ok(())
        }
        Expr::Function { args, .. } => {
            for argument in args {
                validate_group_expression(
                    argument,
                    group_by,
                    aliases,
                    inside_aggregate,
                    resolve_alias,
                )?;
            }
            Ok(())
        }
        Expr::Column(parts) if inside_aggregate => Ok(()),
        Expr::Column(parts) => {
            if resolve_alias
                && parts.len() == 1
                && let Some(aliased) = aliases.get(&parts[0].lookup_key())
                && *aliased != expression
            {
                return validate_group_expression(aliased, group_by, aliases, false, false);
            }
            Err(Error::execution(format!(
                "column '{}' must appear in GROUP BY or an aggregate function",
                expression.display_name()
            )))
        }
        Expr::Wildcard if inside_aggregate => Ok(()),
        Expr::Wildcard => Err(Error::execution(
            "wildcard is invalid outside an aggregate function in an aggregate query",
        )),
        Expr::Literal(_) => Ok(()),
    }
}

fn group_expressions_equivalent(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Literal(left), Expr::Literal(right)) => left == right,
        (Expr::Column(left), Expr::Column(right)) => left.last() == right.last(),
        (Expr::Wildcard, Expr::Wildcard) => true,
        (
            Expr::Unary {
                op: left_op,
                expr: left,
            },
            Expr::Unary {
                op: right_op,
                expr: right,
            },
        ) => left_op == right_op && group_expressions_equivalent(left, right),
        (
            Expr::Binary {
                left: left_left,
                op: left_op,
                right: left_right,
            },
            Expr::Binary {
                left: right_left,
                op: right_op,
                right: right_right,
            },
        ) => {
            left_op == right_op
                && group_expressions_equivalent(left_left, right_left)
                && group_expressions_equivalent(left_right, right_right)
        }
        (
            Expr::Function {
                name: left_name,
                args: left_args,
            },
            Expr::Function {
                name: right_name,
                args: right_args,
            },
        ) => {
            left_name.eq_ignore_ascii_case(right_name)
                && left_args.len() == right_args.len()
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| group_expressions_equivalent(left, right))
        }
        (
            Expr::IsNull {
                expr: left,
                negated: left_negated,
            },
            Expr::IsNull {
                expr: right,
                negated: right_negated,
            },
        ) => left_negated == right_negated && group_expressions_equivalent(left, right),
        _ => false,
    }
}

fn is_aggregate_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "min" | "max" | "avg"
    )
}

struct EvalContext<'a> {
    table: Option<&'a Table>,
    row: Option<usize>,
    group: Option<&'a [usize]>,
    aliases: Option<&'a HashMap<String, &'a Value>>,
}

fn eval_expr(expression: &Expr, context: &EvalContext<'_>) -> Result<Value> {
    match expression {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(parts) => {
            let identifier = parts
                .last()
                .ok_or_else(|| Error::execution("empty column reference"))?;
            if parts.len() == 1
                && let Some(value) = context
                    .aliases
                    .and_then(|aliases| aliases.get(&identifier.lookup_key()))
            {
                return Ok((*value).clone());
            }
            let table = context.table.ok_or_else(|| {
                Error::execution(format!("column '{}' requires a table", identifier.value))
            })?;
            let row = context.row.ok_or_else(|| {
                Error::execution(format!(
                    "column '{}' has no row in an empty group",
                    identifier.value
                ))
            })?;
            Ok(table.value(table.column_index(identifier)?, row))
        }
        Expr::Wildcard => Err(Error::execution(
            "wildcard is only valid in SELECT or COUNT",
        )),
        Expr::Unary { op, expr } => eval_unary(*op, eval_expr(expr, context)?),
        Expr::Binary { left, op, right } => eval_binary(left, *op, right, context),
        Expr::Function { name, args } => eval_function(name, args, context),
        Expr::IsNull { expr, negated } => {
            let is_null = eval_expr(expr, context)? == Value::Null;
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
    }
}

fn eval_unary(operator: UnaryOp, value: Value) -> Result<Value> {
    match (operator, value) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryOp::Plus, value @ (Value::Int64(_) | Value::Float64(_))) => Ok(value),
        (UnaryOp::Minus, Value::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::execution("Int64 overflow in unary minus")),
        (UnaryOp::Minus, Value::Float64(value)) => Ok(Value::Float64(-value)),
        (UnaryOp::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (UnaryOp::Not, _) => Err(Error::Type("NOT requires a Bool expression".to_owned())),
        (_, _) => Err(Error::Type(
            "unary +/- requires a numeric expression".to_owned(),
        )),
    }
}

fn eval_binary(
    left_expr: &Expr,
    operator: BinaryOp,
    right_expr: &Expr,
    context: &EvalContext<'_>,
) -> Result<Value> {
    if matches!(operator, BinaryOp::And | BinaryOp::Or) {
        return eval_boolean_binary(left_expr, operator, right_expr, context);
    }
    let left = eval_expr(left_expr, context)?;
    let right = eval_expr(right_expr, context)?;
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    match operator {
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual => {
            let ordering = left.sql_cmp(&right)?.expect("non-null comparison");
            let value = match operator {
                BinaryOp::Equal => ordering == Ordering::Equal,
                BinaryOp::NotEqual => ordering != Ordering::Equal,
                BinaryOp::Less => ordering == Ordering::Less,
                BinaryOp::LessEqual => ordering != Ordering::Greater,
                BinaryOp::Greater => ordering == Ordering::Greater,
                BinaryOp::GreaterEqual => ordering != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(value))
        }
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo => arithmetic(left, operator, right),
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

fn eval_boolean_binary(
    left_expr: &Expr,
    operator: BinaryOp,
    right_expr: &Expr,
    context: &EvalContext<'_>,
) -> Result<Value> {
    let left = eval_expr(left_expr, context)?.as_bool("AND/OR")?;
    if matches!((operator, left), (BinaryOp::And, Some(false))) {
        return Ok(Value::Bool(false));
    }
    if matches!((operator, left), (BinaryOp::Or, Some(true))) {
        return Ok(Value::Bool(true));
    }
    let right = eval_expr(right_expr, context)?.as_bool("AND/OR")?;
    let result = match operator {
        BinaryOp::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOp::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        _ => unreachable!(),
    };
    Ok(result.map_or(Value::Null, Value::Bool))
}

fn arithmetic(left: Value, operator: BinaryOp, right: Value) -> Result<Value> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => {
            let value = match operator {
                BinaryOp::Add => left.checked_add(right),
                BinaryOp::Subtract => left.checked_sub(right),
                BinaryOp::Multiply => left.checked_mul(right),
                BinaryOp::Divide => left.checked_div(right),
                BinaryOp::Modulo => left.checked_rem(right),
                _ => unreachable!(),
            };
            value.map(Value::Int64).ok_or_else(|| {
                Error::execution("Int64 overflow or division by zero in arithmetic expression")
            })
        }
        (Value::Int64(left), Value::Float64(right)) => {
            float_arithmetic(left as f64, operator, right)
        }
        (Value::Float64(left), Value::Int64(right)) => {
            float_arithmetic(left, operator, right as f64)
        }
        (Value::Float64(left), Value::Float64(right)) => float_arithmetic(left, operator, right),
        (left, right) => Err(Error::Type(format!(
            "arithmetic requires numeric values, found {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

fn float_arithmetic(left: f64, operator: BinaryOp, right: f64) -> Result<Value> {
    if right == 0.0 && matches!(operator, BinaryOp::Divide | BinaryOp::Modulo) {
        return Err(Error::execution("division by zero"));
    }
    let result = match operator {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        BinaryOp::Multiply => left * right,
        BinaryOp::Divide => left / right,
        BinaryOp::Modulo => left % right,
        _ => unreachable!(),
    };
    if !result.is_finite() {
        return Err(Error::execution("non-finite Float64 arithmetic result"));
    }
    Ok(Value::Float64(result))
}

fn eval_function(name: &str, args: &[Expr], context: &EvalContext<'_>) -> Result<Value> {
    let normalized = name.to_ascii_lowercase();
    if !matches!(normalized.as_str(), "count" | "sum" | "min" | "max" | "avg") {
        return Err(Error::execution(format!("unsupported function '{name}'")));
    }
    let group = context
        .group
        .ok_or_else(|| Error::execution(format!("aggregate '{name}' used outside aggregation")))?;
    if normalized == "count" && args.is_empty() {
        return count_to_value(group.len());
    }
    if args.len() != 1 {
        return Err(Error::execution(format!(
            "aggregate '{name}' expects exactly one argument"
        )));
    }
    if normalized == "count" && matches!(args[0], Expr::Wildcard) {
        return count_to_value(group.len());
    }
    if matches!(args[0], Expr::Wildcard) {
        return Err(Error::execution(format!("'{name}(*)' is not supported")));
    }

    match normalized.as_str() {
        "count" => aggregate_count(group, &args[0], context),
        "sum" => aggregate_sum(group, &args[0], context),
        "min" => aggregate_extreme(group, &args[0], context, false),
        "max" => aggregate_extreme(group, &args[0], context, true),
        "avg" => aggregate_avg(group, &args[0], context),
        _ => unreachable!(),
    }
}

fn count_to_value(count: usize) -> Result<Value> {
    i64::try_from(count)
        .map(Value::Int64)
        .map_err(|_| Error::execution("COUNT exceeds Int64"))
}

fn aggregate_count(group: &[usize], expression: &Expr, context: &EvalContext<'_>) -> Result<Value> {
    let mut count = 0_usize;
    for &row in group {
        if eval_group_argument(expression, row, context)? != Value::Null {
            count += 1;
        }
    }
    count_to_value(count)
}

fn aggregate_sum(group: &[usize], expression: &Expr, context: &EvalContext<'_>) -> Result<Value> {
    let mut result: Option<Value> = None;
    for &row in group {
        let value = eval_group_argument(expression, row, context)?;
        if value == Value::Null {
            continue;
        }
        result = Some(match (result, value) {
            (None, value @ (Value::Int64(_) | Value::Float64(_))) => value,
            (Some(Value::Int64(sum)), Value::Int64(value)) => Value::Int64(
                sum.checked_add(value)
                    .ok_or_else(|| Error::execution("Int64 overflow in SUM"))?,
            ),
            (Some(Value::Float64(sum)), Value::Float64(value)) => {
                let sum = sum + value;
                if !sum.is_finite() {
                    return Err(Error::execution("non-finite Float64 SUM"));
                }
                Value::Float64(sum)
            }
            _ => return Err(Error::Type("SUM requires one numeric type".to_owned())),
        });
    }
    Ok(result.unwrap_or(Value::Null))
}

fn aggregate_avg(group: &[usize], expression: &Expr, context: &EvalContext<'_>) -> Result<Value> {
    let mut sum = 0.0;
    let mut count = 0_usize;
    for &row in group {
        match eval_group_argument(expression, row, context)? {
            Value::Null => continue,
            Value::Int64(value) => sum += value as f64,
            Value::Float64(value) => sum += value,
            _ => return Err(Error::Type("AVG requires a numeric argument".to_owned())),
        }
        if !sum.is_finite() {
            return Err(Error::execution("non-finite Float64 AVG"));
        }
        count += 1;
    }
    if count == 0 {
        Ok(Value::Null)
    } else {
        Ok(Value::Float64(sum / count as f64))
    }
}

fn aggregate_extreme(
    group: &[usize],
    expression: &Expr,
    context: &EvalContext<'_>,
    maximum: bool,
) -> Result<Value> {
    let mut result: Option<Value> = None;
    for &row in group {
        let value = eval_group_argument(expression, row, context)?;
        if value == Value::Null {
            continue;
        }
        let replace = if let Some(current) = &result {
            let ordering = value
                .sql_cmp(current)?
                .ok_or_else(|| Error::execution("aggregate comparison returned NULL"))?;
            (maximum && ordering == Ordering::Greater) || (!maximum && ordering == Ordering::Less)
        } else {
            true
        };
        if replace {
            result = Some(value);
        }
    }
    Ok(result.unwrap_or(Value::Null))
}

fn eval_group_argument(expression: &Expr, row: usize, context: &EvalContext<'_>) -> Result<Value> {
    eval_expr(
        expression,
        &EvalContext {
            table: context.table,
            row: Some(row),
            group: None,
            aliases: None,
        },
    )
}

fn result_aliases<'a>(
    projection_names: &HashMap<String, usize>,
    values: &'a [Value],
) -> HashMap<String, &'a Value> {
    projection_names
        .iter()
        .map(|(name, index)| (name.clone(), &values[*index]))
        .collect()
}

fn eval_order_expr(item: &OrderItem, context: &EvalContext<'_>, values: &[Value]) -> Result<Value> {
    if let Expr::Literal(Value::Int64(position)) = &item.expr {
        return Ok(values[order_position(*position, values.len())?].clone());
    }
    eval_expr(&item.expr, context)
}

fn order_position(position: i64, projection_len: usize) -> Result<usize> {
    let ordinal = position;
    let position = usize::try_from(position)
        .ok()
        .filter(|position| *position >= 1 && *position <= projection_len);
    position.map(|position| position - 1).ok_or_else(|| {
        Error::execution(format!(
            "ORDER BY position {ordinal} is outside the projection"
        ))
    })
}

struct Record {
    values: Vec<Value>,
    order_values: Vec<Value>,
    sequence: usize,
}

fn compare_records(left: &Record, right: &Record, ordering: &[OrderItem]) -> Ordering {
    for (index, item) in ordering.iter().enumerate() {
        let left_value = &left.order_values[index];
        let right_value = &right.order_values[index];
        let comparison = match (left_value, right_value) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            _ => left_value
                .sql_cmp(right_value)
                .ok()
                .flatten()
                .unwrap_or(Ordering::Equal),
        };
        let comparison = if item.descending
            && !matches!(left_value, Value::Null)
            && !matches!(right_value, Value::Null)
        {
            comparison.reverse()
        } else {
            comparison
        };
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    left.sequence.cmp(&right.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_sql_input_limit_checks_the_exact_boundary() {
        assert!(ensure_sql_input_limit(MAX_INPUT_BYTES).is_ok());
        assert!(matches!(
            ensure_sql_input_limit(MAX_INPUT_BYTES + 1),
            Err(Error::Limit {
                resource: "SQL input bytes",
                limit: MAX_INPUT_BYTES
            })
        ));
    }

    #[test]
    fn group_intermediates_obey_the_materialization_budget() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE t (name String); INSERT INTO t VALUES ('a'), ('b'), ('c');")
            .unwrap();
        let Statement::Select(select) =
            parser::parse("SELECT name, count(*) FROM t GROUP BY name LIMIT 1;")
                .unwrap()
                .pop()
                .unwrap()
        else {
            panic!("expected SELECT");
        };
        let error = database.select(select, 64).unwrap_err();
        assert!(matches!(
            error,
            Error::Limit {
                resource: "materialized query result bytes",
                ..
            }
        ));
    }

    #[test]
    fn group_key_fields_are_charged_as_they_are_built() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE t (name String); INSERT INTO t VALUES ('abcdefghij');")
            .unwrap();
        let Statement::Select(select) =
            parser::parse("SELECT count(*) FROM t GROUP BY name, name;")
                .unwrap()
                .pop()
                .unwrap()
        else {
            panic!("expected SELECT");
        };
        let error = database.select(select, 110).unwrap_err();
        assert!(matches!(
            error,
            Error::Limit {
                resource: "materialized query result bytes",
                ..
            }
        ));
    }

    #[test]
    fn retained_result_size_includes_row_vectors_and_string_capacity() {
        let mut string = String::with_capacity(4_096);
        string.push('x');
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "value".to_owned(),
                data_type: Some(DataType::String),
                nullable: false,
            }],
            rows: vec![vec![Value::String(string)]],
        };
        assert!(result_size(&result) > 4_096 + std::mem::size_of::<Vec<Value>>());
    }

    #[test]
    fn aggregate_scans_fit_without_a_group_sized_value_buffer() {
        let mut database = Database::new();
        let mut sql = String::from("CREATE TABLE t (value Int64); INSERT INTO t VALUES ");
        for value in 0..1_000 {
            if value > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({value})"));
        }
        database.execute(&sql).unwrap();
        let Statement::Select(select) = parser::parse("SELECT sum(value) FROM t;")
            .unwrap()
            .pop()
            .unwrap()
        else {
            panic!("expected SELECT");
        };
        let result = database.select(select, 16_000).unwrap();
        assert_eq!(result.rows, vec![vec![Value::Int64(499_500)]]);
    }
}
