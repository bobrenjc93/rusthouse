use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::mem::{size_of, size_of_val};

use crate::sql::{
    AggregateFunction, BinaryOperator, ColumnReference, Expr, Identifier,
    MAX_SAFE_EXPRESSION_DEPTH, MAX_SAFE_EXPRESSION_NODES, OrderBy, Select, SelectItem, Statement,
    UnaryOperator, parse,
};
use crate::storage::{Schema, Table, identifier_key, identifiers_equal};
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
    pub max_expression_depth: usize,
    pub max_expression_nodes: usize,
    pub max_intermediate_rows: usize,
    pub max_intermediate_bytes: usize,
    pub max_result_bytes: usize,
    pub max_tokens_per_request: usize,
    pub max_statements_per_request: usize,
    pub max_request_result_rows: usize,
    pub max_request_result_bytes: usize,
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
            max_expression_depth: MAX_SAFE_EXPRESSION_DEPTH,
            max_expression_nodes: MAX_SAFE_EXPRESSION_NODES,
            max_intermediate_rows: 1_000_000,
            max_intermediate_bytes: 128 * 1024 * 1024,
            max_result_bytes: 64 * 1024 * 1024,
            max_tokens_per_request: 262_144,
            max_statements_per_request: 1_024,
            max_request_result_rows: 1_000_000,
            max_request_result_bytes: 64 * 1024 * 1024,
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
    pub fn with_limits(mut limits: Limits) -> Self {
        limits.max_expression_depth = limits.max_expression_depth.min(MAX_SAFE_EXPRESSION_DEPTH);
        limits.max_expression_nodes = limits.max_expression_nodes.min(MAX_SAFE_EXPRESSION_NODES);
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
        let statements = self.parse_sql(sql)?;
        let mut results = Vec::new();
        let mut request_rows = 0_usize;
        let mut request_bytes = size_of::<Vec<ExecutionResult>>();
        for statement in statements {
            let result = self.execute_parsed(statement)?;
            account_request_result(&result, &mut request_rows, &mut request_bytes, &self.limits)?;
            results.push(result);
        }
        Ok(results)
    }

    fn parse_sql(&self, sql: &str) -> Result<Vec<Statement>, DatabaseError> {
        if sql.len() > self.limits.max_input_bytes {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::InputBytes,
                limit: self.limits.max_input_bytes,
                actual: sql.len(),
            });
        }
        parse(
            sql,
            self.limits.max_expression_depth,
            self.limits.max_expression_nodes,
            self.limits.max_string_bytes,
            self.limits.max_tokens_per_request,
            self.limits.max_statements_per_request,
        )
    }

    /// Executes exactly one SQL statement.
    pub fn execute_one(&mut self, sql: &str) -> Result<ExecutionResult, DatabaseError> {
        let mut statements = self.parse_sql(sql)?;
        if statements.len() != 1 {
            return Err(DatabaseError::invalid(format!(
                "expected one statement, got {}",
                statements.len()
            )));
        }
        let result = self.execute_parsed(statements.pop().expect("one statement was checked"))?;
        let mut request_rows = 0;
        let mut request_bytes = size_of::<Vec<ExecutionResult>>();
        account_request_result(&result, &mut request_rows, &mut request_bytes, &self.limits)?;
        Ok(result)
    }

    /// Returns a schema when exact-quoted and folded-unquoted lookup are unambiguous.
    pub fn schema(&self, table: &str) -> Result<&Schema, DatabaseError> {
        self.resolve_table(table).map(|table| &table.schema)
    }

    /// Returns a table schema using exact quoted-identifier semantics.
    pub fn schema_quoted(&self, table: &str) -> Result<&Schema, DatabaseError> {
        self.table_with_mode(table, true).map(|table| &table.schema)
    }

    /// Returns a table schema using case-folded unquoted-identifier semantics.
    pub fn schema_unquoted(&self, table: &str) -> Result<&Schema, DatabaseError> {
        self.table_with_mode(table, false)
            .map(|table| &table.schema)
    }

    pub fn table_row_count(&self, table: &str) -> Result<usize, DatabaseError> {
        self.resolve_table(table).map(Table::row_count)
    }

    pub fn table_row_count_quoted(&self, table: &str) -> Result<usize, DatabaseError> {
        self.table_with_mode(table, true).map(Table::row_count)
    }

    pub fn table_row_count_unquoted(&self, table: &str) -> Result<usize, DatabaseError> {
        self.table_with_mode(table, false).map(Table::row_count)
    }

    fn table_with_mode(&self, table: &str, quoted: bool) -> Result<&Table, DatabaseError> {
        self.tables
            .get(&identifier_key(table, quoted))
            .ok_or_else(|| DatabaseError::TableNotFound(table.to_owned()))
    }

    fn resolve_table(&self, table: &str) -> Result<&Table, DatabaseError> {
        let quoted_key = identifier_key(table, true);
        let unquoted_key = identifier_key(table, false);
        if quoted_key == unquoted_key {
            return self
                .tables
                .get(&quoted_key)
                .ok_or_else(|| DatabaseError::TableNotFound(table.to_owned()));
        }
        match (self.tables.get(&quoted_key), self.tables.get(&unquoted_key)) {
            (Some(_), Some(_)) => Err(DatabaseError::AmbiguousTable(table.to_owned())),
            (Some(table), None) | (None, Some(table)) => Ok(table),
            (None, None) => Err(DatabaseError::TableNotFound(table.to_owned())),
        }
    }

    fn execute_parsed(&mut self, statement: Statement) -> Result<ExecutionResult, DatabaseError> {
        match statement {
            Statement::CreateTable {
                name,
                if_not_exists,
                columns,
            } => {
                let key = identifier_key(&name.value, name.quoted);
                if self.tables.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecutionResult::TableCreated { table: name.value });
                    }
                    return Err(DatabaseError::TableAlreadyExists(name.value));
                }
                let quoted = columns.iter().map(|column| column.name.quoted).collect();
                let columns = columns
                    .into_iter()
                    .map(|column| ColumnDefinition {
                        name: column.name.value,
                        data_type: column.data_type,
                    })
                    .collect();
                let schema = Schema::new_with_quoted(columns, quoted, &self.limits)?;
                let table_name = name.value;
                self.tables
                    .insert(key, Table::new(table_name.clone(), name.quoted, schema));
                Ok(ExecutionResult::TableCreated { table: table_name })
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
        table_name: Identifier,
        insert_columns: Option<Vec<Identifier>>,
        expressions: Vec<Vec<Expr>>,
    ) -> Result<ExecutionResult, DatabaseError> {
        if expressions.len() > self.limits.max_rows_per_insert {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::RowsPerInsert,
                limit: self.limits.max_rows_per_insert,
                actual: expressions.len(),
            });
        }
        let key = identifier_key(&table_name.value, table_name.quoted);
        let table = self
            .tables
            .get_mut(&key)
            .ok_or_else(|| DatabaseError::TableNotFound(table_name.value.clone()))?;

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
                    .column_index_bound(&name.value, name.quoted)
                    .ok_or_else(|| DatabaseError::ColumnNotFound(name.value.clone()))?;
                if seen[index] {
                    return Err(DatabaseError::ColumnAlreadyExists(name.value));
                }
                seen[index] = true;
                order.push(index);
            }
            order
        } else {
            (0..table.schema.columns().len()).collect()
        };

        // Bind and type-check the entire batch before short-circuiting evaluation can hide errors.
        let values_scope = Schema::empty();
        for (row_number, expression_row) in expressions.iter().enumerate() {
            if expression_row.len() != column_order.len() {
                return Err(DatabaseError::InvalidValue(format!(
                    "row {} has {} values but INSERT expects {}",
                    row_number + 1,
                    expression_row.len(),
                    column_order.len()
                )));
            }
            for (expression, &target) in expression_row.iter().zip(&column_order) {
                if expression.contains_aggregate() {
                    return Err(DatabaseError::invalid(
                        "aggregate functions are not allowed in VALUES",
                    ));
                }
                let actual = infer_type(expression, &values_scope)?;
                validate_insert_type(actual, table.schema.columns()[target].data_type)?;
            }
        }

        // Materialize and coerce every row before calling the atomic storage append.
        let mut rows = Vec::with_capacity(expressions.len());
        for expression_row in expressions {
            let mut row: Vec<Option<Value>> = vec![None; table.schema.columns().len()];
            for (expression, &target) in expression_row.iter().zip(&column_order) {
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
            table: table_name.value,
            rows: inserted,
        })
    }

    fn select(&self, mut select: Select) -> Result<QueryResult, DatabaseError> {
        let table = match &select.from {
            Some(name) => Some(
                self.tables
                    .get(&identifier_key(&name.value, name.quoted))
                    .ok_or_else(|| DatabaseError::TableNotFound(name.value.clone()))?,
            ),
            None => None,
        };
        let empty_schema = Schema::empty();
        let schema = table.map_or(&empty_schema, |table| &table.schema);

        let items = expand_wildcards(&select.items, schema, table.is_some())?;
        for expression in &mut select.group_by {
            resolve_projection_alias(expression, &items)?;
        }
        let output_identifiers: Vec<_> = items.iter().map(output_identifier).collect();
        let has_aggregate = items.iter().any(|item| item.expr.contains_aggregate())
            || select
                .order_by
                .iter()
                .any(|order| order.expr.contains_aggregate());
        validate_select(&select, &items, &output_identifiers, has_aggregate, schema)?;
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
            if output_reference_index(&order.expr, &output_identifiers)?.is_none() {
                validate_column_references(&order.expr, table)?;
                infer_type(&order.expr, schema)?;
            }
        }

        let grouped = has_aggregate || !select.group_by.is_empty();
        let columns = items
            .iter()
            .map(|item| {
                Ok(ColumnDefinition {
                    name: item
                        .alias
                        .as_ref()
                        .map_or_else(|| item.expr.label(), |alias| alias.value.clone()),
                    data_type: infer_type(&item.expr, schema)?,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        let result_base_bytes = estimate_result_base(&columns);

        let source_rows = table.map_or(1, Table::row_count);
        let groups = if grouped {
            let mut filtered = Vec::new();
            for row in 0..source_rows {
                if row_matches_filter(select.filter.as_ref(), table, row)? {
                    if filtered.len() >= self.limits.max_intermediate_rows {
                        return Err(DatabaseError::LimitExceeded {
                            kind: LimitKind::IntermediateRows,
                            limit: self.limits.max_intermediate_rows,
                            actual: filtered.len() + 1,
                        });
                    }
                    filtered.push(row);
                }
            }
            let index_bytes = filtered.len().saturating_mul(size_of::<usize>());
            check_byte_limit(
                LimitKind::IntermediateBytes,
                self.limits.max_intermediate_bytes,
                index_bytes,
            )?;
            build_groups(filtered, &select.group_by, table, &self.limits)?
        } else {
            Vec::new()
        };

        let rows = if select.order_by.is_empty() {
            if grouped {
                collect_unordered_groups(
                    &groups,
                    &items,
                    table,
                    schema,
                    &select,
                    result_base_bytes,
                    &self.limits,
                )?
            } else {
                collect_unordered_rows(
                    source_rows,
                    &items,
                    table,
                    select.filter.as_ref(),
                    &select,
                    result_base_bytes,
                    &self.limits,
                )?
            }
        } else {
            let mut collector = TopKCollector::new(&select, &self.limits);
            if grouped {
                for (ordinal, group) in groups.iter().enumerate() {
                    if collector.is_empty_limit() {
                        break;
                    }
                    let mut budgets =
                        ordered_projection_budgets(&collector, result_base_bytes, &self.limits);
                    let values = evaluate_projection(&items, &mut budgets, |expr| {
                        eval_group_expr(expr, table, &group.rows, schema)
                    })?;
                    let mut order_budget = budgets[0];
                    let order = evaluate_order(
                        &select.order_by,
                        &output_identifiers,
                        &values,
                        OrderSource {
                            table,
                            row: group.rows.first().copied(),
                            group: Some(&group.rows),
                            schema,
                        },
                        &mut order_budget,
                    )?;
                    collector.push(Record {
                        values,
                        order,
                        ordinal,
                    })?;
                }
            } else {
                let mut ordinal = 0;
                for row in 0..source_rows {
                    if !row_matches_filter(select.filter.as_ref(), table, row)? {
                        continue;
                    }
                    if collector.is_empty_limit() {
                        break;
                    }
                    let mut budgets =
                        ordered_projection_budgets(&collector, result_base_bytes, &self.limits);
                    let values = evaluate_projection(&items, &mut budgets, |expr| {
                        eval_row_expr(expr, table, Some(row))
                    })?;
                    let mut order_budget = budgets[0];
                    let order = evaluate_order(
                        &select.order_by,
                        &output_identifiers,
                        &values,
                        OrderSource {
                            table,
                            row: Some(row),
                            group: None,
                            schema,
                        },
                        &mut order_budget,
                    )?;
                    collector.push(Record {
                        values,
                        order,
                        ordinal,
                    })?;
                    ordinal += 1;
                }
            }
            collector.finish(&select, &self.limits)?
        };
        validate_result_bytes(&columns, &rows, &self.limits)?;
        Ok(QueryResult { columns, rows })
    }
}

#[derive(Debug)]
struct Projection {
    expr: Expr,
    alias: Option<Identifier>,
}

#[derive(Clone, Copy)]
struct ByteBudget {
    kind: LimitKind,
    bytes: usize,
    limit: usize,
}

impl ByteBudget {
    fn charge(&mut self, bytes: usize) -> Result<(), DatabaseError> {
        self.bytes = self.bytes.saturating_add(bytes);
        check_byte_limit(self.kind, self.limit, self.bytes)
    }
}

fn evaluate_projection(
    items: &[Projection],
    budgets: &mut [ByteBudget],
    mut evaluate: impl FnMut(&Expr) -> Result<Value, DatabaseError>,
) -> Result<Vec<Value>, DatabaseError> {
    let fixed_bytes = size_of::<Vec<Value>>().saturating_add(
        items
            .len()
            .saturating_mul(size_of::<Value>())
            .saturating_mul(2),
    );
    for budget in &mut *budgets {
        budget.charge(fixed_bytes)?;
    }
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let value = evaluate(&item.expr)?;
        let value_bytes = estimate_value(&value);
        for budget in &mut *budgets {
            budget.charge(value_bytes)?;
        }
        values.push(value);
    }
    Ok(values)
}

fn ordered_projection_budgets(
    collector: &TopKCollector,
    result_base_bytes: usize,
    limits: &Limits,
) -> [ByteBudget; 2] {
    let row_container_bytes = size_of::<Vec<Value>>().saturating_mul(2);
    [
        ByteBudget {
            kind: LimitKind::IntermediateBytes,
            bytes: collector
                .used_bytes()
                .saturating_add(size_of::<Record>().saturating_mul(2)),
            limit: limits.max_intermediate_bytes,
        },
        ByteBudget {
            kind: LimitKind::ResultBytes,
            bytes: result_base_bytes.saturating_add(row_container_bytes),
            limit: limits.max_result_bytes,
        },
    ]
}

fn output_identifier(item: &Projection) -> Identifier {
    if let Some(alias) = &item.alias {
        alias.clone()
    } else if let Expr::Column(reference) = &item.expr {
        reference.name.clone()
    } else {
        Identifier::unquoted(item.expr.label())
    }
}

fn row_matches_filter(
    filter: Option<&Expr>,
    table: Option<&Table>,
    row: usize,
) -> Result<bool, DatabaseError> {
    match filter {
        Some(filter) => expect_bool(eval_row_expr(filter, table, Some(row))?, "WHERE"),
        None => Ok(true),
    }
}

fn collect_unordered_rows(
    source_rows: usize,
    items: &[Projection],
    table: Option<&Table>,
    filter: Option<&Expr>,
    select: &Select,
    result_base_bytes: usize,
    limits: &Limits,
) -> Result<Vec<Vec<Value>>, DatabaseError> {
    let mut rows = Vec::new();
    let mut matched = 0_usize;
    let mut result_bytes = result_base_bytes;
    for row in 0..source_rows {
        if !row_matches_filter(filter, table, row)? {
            continue;
        }
        if matched < select.offset {
            matched += 1;
            continue;
        }
        let result_index = matched - select.offset;
        if select.limit.is_some_and(|limit| result_index >= limit) {
            break;
        }
        if rows.len() >= limits.max_result_rows {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ResultRows,
                limit: limits.max_result_rows,
                actual: rows.len() + 1,
            });
        }
        let mut budgets = [ByteBudget {
            kind: LimitKind::ResultBytes,
            bytes: result_bytes.saturating_add(size_of::<Vec<Value>>().saturating_mul(2)),
            limit: limits.max_result_bytes,
        }];
        let values = evaluate_projection(items, &mut budgets, |expr| {
            eval_row_expr(expr, table, Some(row))
        })?;
        result_bytes = budgets[0].bytes;
        rows.push(values);
        matched += 1;
    }
    Ok(rows)
}

fn collect_unordered_groups(
    groups: &[Group],
    items: &[Projection],
    table: Option<&Table>,
    schema: &Schema,
    select: &Select,
    result_base_bytes: usize,
    limits: &Limits,
) -> Result<Vec<Vec<Value>>, DatabaseError> {
    let mut rows = Vec::new();
    let mut result_bytes = result_base_bytes;
    for (ordinal, group) in groups.iter().enumerate() {
        if ordinal < select.offset {
            continue;
        }
        let result_index = ordinal - select.offset;
        if select.limit.is_some_and(|limit| result_index >= limit) {
            break;
        }
        if rows.len() >= limits.max_result_rows {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ResultRows,
                limit: limits.max_result_rows,
                actual: rows.len() + 1,
            });
        }
        let mut budgets = [ByteBudget {
            kind: LimitKind::ResultBytes,
            bytes: result_bytes.saturating_add(size_of::<Vec<Value>>().saturating_mul(2)),
            limit: limits.max_result_bytes,
        }];
        let values = evaluate_projection(items, &mut budgets, |expr| {
            eval_group_expr(expr, table, &group.rows, schema)
        })?;
        result_bytes = budgets[0].bytes;
        rows.push(values);
    }
    Ok(rows)
}

struct TopKCollector {
    records: BinaryHeap<Record>,
    retain: usize,
    capacity: usize,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
}

impl TopKCollector {
    fn new(select: &Select, limits: &Limits) -> Self {
        let requested = select
            .limit
            .unwrap_or_else(|| limits.max_result_rows.saturating_add(1));
        let retain = select.offset.saturating_add(requested);
        let capacity = retain.min(limits.max_intermediate_rows.saturating_add(1));
        Self {
            records: BinaryHeap::new(),
            retain,
            capacity,
            bytes: 0,
            max_rows: limits.max_intermediate_rows,
            max_bytes: limits.max_intermediate_bytes,
        }
    }

    fn is_empty_limit(&self) -> bool {
        self.retain == 0
    }

    fn used_bytes(&self) -> usize {
        self.bytes
    }

    fn push(&mut self, record: Record) -> Result<(), DatabaseError> {
        let record_bytes = record.estimated_bytes();
        check_byte_limit(LimitKind::IntermediateBytes, self.max_bytes, record_bytes)?;
        self.bytes = self.bytes.saturating_add(record_bytes);
        self.records.push(record);
        if self.records.len() > self.capacity
            && let Some(removed) = self.records.pop()
        {
            self.bytes = self.bytes.saturating_sub(removed.estimated_bytes());
        }
        if self.records.len() > self.max_rows {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::IntermediateRows,
                limit: self.max_rows,
                actual: self.records.len(),
            });
        }
        check_byte_limit(LimitKind::IntermediateBytes, self.max_bytes, self.bytes)
    }

    fn finish(self, select: &Select, limits: &Limits) -> Result<Vec<Vec<Value>>, DatabaseError> {
        let records = self.records.into_sorted_vec();
        let available = records.len().saturating_sub(select.offset);
        let take = select.limit.unwrap_or(available).min(available);
        if take > limits.max_result_rows {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ResultRows,
                limit: limits.max_result_rows,
                actual: take,
            });
        }
        Ok(records
            .into_iter()
            .skip(select.offset)
            .take(take)
            .map(|record| record.values)
            .collect())
    }
}

fn check_byte_limit(kind: LimitKind, limit: usize, actual: usize) -> Result<(), DatabaseError> {
    if actual > limit {
        Err(DatabaseError::LimitExceeded {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn estimate_value(value: &Value) -> usize {
    match value {
        Value::String(value) => value.capacity(),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => 0,
    }
}

fn estimate_values(values: &[Value]) -> usize {
    size_of::<Vec<Value>>()
        .saturating_add(size_of_val(values).saturating_mul(2))
        .saturating_add(values.iter().map(estimate_value).sum::<usize>())
}

fn validate_result_bytes(
    columns: &[ColumnDefinition],
    rows: &[Vec<Value>],
    limits: &Limits,
) -> Result<(), DatabaseError> {
    let bytes = estimate_query_result_bytes(columns, rows);
    check_byte_limit(LimitKind::ResultBytes, limits.max_result_bytes, bytes)
}

fn estimate_query_result_bytes(columns: &[ColumnDefinition], rows: &[Vec<Value>]) -> usize {
    rows.iter()
        .fold(estimate_result_base(columns), |bytes, row| {
            bytes
                .saturating_add(size_of::<Vec<Value>>().saturating_mul(2))
                .saturating_add(estimate_values(row))
        })
}

fn estimate_result_base(columns: &[ColumnDefinition]) -> usize {
    size_of::<Vec<ColumnDefinition>>()
        .saturating_add(size_of_val(columns).saturating_mul(2))
        .saturating_add(
            columns
                .iter()
                .map(|column| column.name.capacity())
                .sum::<usize>(),
        )
        .saturating_add(size_of::<Vec<Vec<Value>>>())
}

fn account_request_result(
    result: &ExecutionResult,
    request_rows: &mut usize,
    request_bytes: &mut usize,
    limits: &Limits,
) -> Result<(), DatabaseError> {
    let (rows, bytes) = match result {
        ExecutionResult::TableCreated { table } => (0, table.capacity()),
        ExecutionResult::RowsInserted { table, .. } => (0, table.capacity()),
        ExecutionResult::Query(result) => (
            result.rows.len(),
            estimate_query_result_bytes(&result.columns, &result.rows),
        ),
    };
    *request_rows = request_rows.saturating_add(rows);
    if *request_rows > limits.max_request_result_rows {
        return Err(DatabaseError::LimitExceeded {
            kind: LimitKind::RequestResultRows,
            limit: limits.max_request_result_rows,
            actual: *request_rows,
        });
    }
    *request_bytes = request_bytes
        .saturating_add(size_of::<ExecutionResult>().saturating_mul(2))
        .saturating_add(bytes);
    check_byte_limit(
        LimitKind::RequestResultBytes,
        limits.max_request_result_bytes,
        *request_bytes,
    )
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
                expanded.extend(schema.columns().iter().enumerate().map(|(index, column)| {
                    Projection {
                        expr: Expr::Column(ColumnReference::unqualified(
                            column.name.clone(),
                            schema.column_is_quoted(index),
                        )),
                        alias: None,
                    }
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
    output_identifiers: &[Identifier],
    has_aggregate: bool,
    schema: &Schema,
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
            validate_group_expression(&item.expr, &select.group_by, false, schema)?;
        }
        for order in &select.order_by {
            if output_reference_index(&order.expr, output_identifiers)?.is_none() {
                validate_group_expression(&order.expr, &select.group_by, false, schema)?;
            }
        }
    }
    Ok(())
}

fn validate_group_expression(
    expr: &Expr,
    group_by: &[Expr],
    inside_aggregate: bool,
    schema: &Schema,
) -> Result<(), DatabaseError> {
    if !inside_aggregate
        && group_by
            .iter()
            .any(|group| equivalent_expr(expr, group, schema))
    {
        return Ok(());
    }
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(_) if inside_aggregate => Ok(()),
        Expr::Column(reference) => Err(DatabaseError::invalid(format!(
            "column {} must appear in GROUP BY or an aggregate function",
            reference.label()
        ))),
        Expr::Aggregate { argument, .. } => {
            if inside_aggregate {
                return Err(DatabaseError::invalid(
                    "nested aggregate functions are not supported",
                ));
            }
            if let Some(argument) = argument {
                validate_group_expression(argument, group_by, true, schema)?;
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            validate_group_expression(left, group_by, inside_aggregate, schema)?;
            validate_group_expression(right, group_by, inside_aggregate, schema)
        }
        Expr::Unary { expr, .. } => {
            validate_group_expression(expr, group_by, inside_aggregate, schema)
        }
    }
}

fn equivalent_expr(left: &Expr, right: &Expr, schema: &Schema) -> bool {
    match (left, right) {
        (Expr::Literal(left), Expr::Literal(right)) => left == right,
        (Expr::Column(left), Expr::Column(right)) => {
            match (
                schema.column_index_bound(&left.name.value, left.name.quoted),
                schema.column_index_bound(&right.name.value, right.name.quoted),
            ) {
                (Some(left), Some(right)) => left == right,
                _ => false,
            }
        }
        (
            Expr::Aggregate {
                function: left_function,
                argument: left_argument,
            },
            Expr::Aggregate {
                function: right_function,
                argument: right_argument,
            },
        ) => {
            left_function == right_function
                && match (left_argument, right_argument) {
                    (Some(left), Some(right)) => equivalent_expr(left, right, schema),
                    (None, None) => true,
                    _ => false,
                }
        }
        (
            Expr::Binary {
                left: left_left,
                operator: left_operator,
                right: left_right,
            },
            Expr::Binary {
                left: right_left,
                operator: right_operator,
                right: right_right,
            },
        ) => {
            left_operator == right_operator
                && equivalent_expr(left_left, right_left, schema)
                && equivalent_expr(left_right, right_right, schema)
        }
        (
            Expr::Unary {
                operator: left_operator,
                expr: left,
            },
            Expr::Unary {
                operator: right_operator,
                expr: right,
            },
        ) => left_operator == right_operator && equivalent_expr(left, right, schema),
        _ => false,
    }
}

fn projection_alias_index(
    expr: &Expr,
    items: &[Projection],
) -> Result<Option<usize>, DatabaseError> {
    let Expr::Column(reference) = expr else {
        return Ok(None);
    };
    if reference.qualifier.is_some() {
        return Ok(None);
    }
    unique_identifier_match(
        &reference.name,
        items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.alias.as_ref().map(|alias| (index, alias))),
    )
}

fn resolve_projection_alias(expr: &mut Expr, items: &[Projection]) -> Result<(), DatabaseError> {
    if let Some(index) = projection_alias_index(expr, items)? {
        *expr = items[index].expr.clone();
    }
    Ok(())
}

fn output_reference_index(
    expr: &Expr,
    outputs: &[Identifier],
) -> Result<Option<usize>, DatabaseError> {
    let Expr::Column(reference) = expr else {
        return Ok(None);
    };
    if reference.qualifier.is_some() {
        return Ok(None);
    }
    unique_identifier_match(&reference.name, outputs.iter().enumerate())
}

fn unique_identifier_match<'a>(
    reference: &Identifier,
    candidates: impl Iterator<Item = (usize, &'a Identifier)>,
) -> Result<Option<usize>, DatabaseError> {
    let mut matches = candidates.filter(|(_, candidate)| {
        identifiers_equal(
            &candidate.value,
            candidate.quoted,
            &reference.value,
            reference.quoted,
        )
    });
    let first = matches.next().map(|(index, _)| index);
    if matches.next().is_some() {
        Err(DatabaseError::AmbiguousColumn(reference.value.clone()))
    } else {
        Ok(first)
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
    rows: Vec<usize>,
    group_by: &[Expr],
    table: Option<&Table>,
    limits: &Limits,
) -> Result<Vec<Group>, DatabaseError> {
    if group_by.is_empty() {
        return Ok(vec![Group { rows }]);
    }
    let mut intermediate_bytes = rows.capacity().saturating_mul(size_of::<usize>());
    let mut indexes: HashMap<Vec<ScalarKey>, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for row in rows {
        let values = group_by
            .iter()
            .map(|expr| eval_row_expr(expr, table, Some(row)))
            .collect::<Result<Vec<_>, _>>()?;
        let key: Vec<_> = values.iter().map(ScalarKey::from).collect();
        let index = if let Some(index) = indexes.get(&key) {
            *index
        } else {
            let index = groups.len();
            intermediate_bytes = intermediate_bytes
                .saturating_add(estimate_group_key(&key).saturating_mul(2))
                .saturating_add(size_of::<Group>());
            indexes.insert(key, index);
            groups.push(Group { rows: Vec::new() });
            index
        };
        intermediate_bytes =
            intermediate_bytes.saturating_add(size_of::<usize>().saturating_mul(2));
        check_byte_limit(
            LimitKind::IntermediateBytes,
            limits.max_intermediate_bytes,
            intermediate_bytes,
        )?;
        groups[index].rows.push(row);
    }
    Ok(groups)
}

fn estimate_group_key(key: &[ScalarKey]) -> usize {
    size_of::<Vec<ScalarKey>>()
        + size_of_val(key).saturating_mul(2)
        + key
            .iter()
            .map(|value| match value {
                ScalarKey::String(value) => value.capacity(),
                ScalarKey::Int(_) | ScalarKey::Float(_) | ScalarKey::Bool(_) => 0,
            })
            .sum::<usize>()
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
        if let Some(argument) = argument {
            for &row in rows {
                eval_row_expr(argument, table, Some(row))?;
            }
        }
        let count = i64::try_from(rows.len())
            .map_err(|_| DatabaseError::ArithmeticOverflow("COUNT exceeds Int64".into()))?;
        return Ok(Value::Int64(count));
    }
    if rows.is_empty() {
        return Err(DatabaseError::EmptyAggregate(function.name().to_owned()));
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
            let first = values
                .next()
                .expect("empty aggregates are rejected before evaluation");
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

fn eval_row_expr(
    expr: &Expr,
    table: Option<&Table>,
    row: Option<usize>,
) -> Result<Value, DatabaseError> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(reference) => {
            let table = table.ok_or_else(|| {
                DatabaseError::invalid(format!(
                    "column {} requires a FROM table",
                    reference.label()
                ))
            })?;
            if let Some(qualifier) = &reference.qualifier
                && !identifiers_equal(
                    &qualifier.value,
                    qualifier.quoted,
                    &table.name,
                    table.name_quoted,
                )
            {
                return Err(DatabaseError::ColumnNotFound(reference.label()));
            }
            let index = table
                .schema
                .column_index_bound(&reference.name.value, reference.name.quoted)
                .ok_or_else(|| DatabaseError::ColumnNotFound(reference.label()))?;
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
        (Value::Int64(left), Value::Float64(right)) => Ok(compare_i64_f64(*left, *right)),
        (Value::Float64(left), Value::Int64(right)) => Ok(compare_i64_f64(*right, *left).reverse()),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (left, right) => Err(DatabaseError::invalid(format!(
            "cannot compare {} and {}",
            left.data_type(),
            right.data_type()
        ))),
    }
}

fn compare_i64_f64(integer: i64, float: f64) -> Ordering {
    const I64_MIN_AS_F64: f64 = -9_223_372_036_854_775_808.0;
    const I64_UPPER_BOUND_AS_F64: f64 = 9_223_372_036_854_775_808.0;

    if float.is_nan() {
        return if float.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if float < I64_MIN_AS_F64 {
        return Ordering::Greater;
    }
    if float >= I64_UPPER_BOUND_AS_F64 {
        return Ordering::Less;
    }

    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal if float.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if float.fract() < 0.0 => Ordering::Greater,
        ordering => ordering,
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

fn validate_insert_type(actual: DataType, expected: DataType) -> Result<(), DatabaseError> {
    if actual == expected || (actual == DataType::Int64 && expected == DataType::Float64) {
        Ok(())
    } else {
        Err(DatabaseError::TypeMismatch {
            context: "INSERT value".into(),
            expected,
            actual,
        })
    }
}

fn infer_type(expr: &Expr, schema: &Schema) -> Result<DataType, DatabaseError> {
    match expr {
        Expr::Literal(value) => Ok(value.data_type()),
        Expr::Column(reference) => schema
            .column_index_bound(&reference.name.value, reference.name.quoted)
            .map(|index| schema.columns()[index].data_type)
            .ok_or_else(|| DatabaseError::ColumnNotFound(reference.label())),
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
        Expr::Column(reference) => {
            let table = table.ok_or_else(|| {
                DatabaseError::invalid(format!(
                    "column {} requires a FROM table",
                    reference.label()
                ))
            })?;
            if let Some(qualifier) = &reference.qualifier
                && !identifiers_equal(
                    &qualifier.value,
                    qualifier.quoted,
                    &table.name,
                    table.name_quoted,
                )
            {
                return Err(DatabaseError::ColumnNotFound(reference.label()));
            }
            table
                .schema
                .column_index_bound(&reference.name.value, reference.name.quoted)
                .map(|_| ())
                .ok_or_else(|| DatabaseError::ColumnNotFound(reference.label()))
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

struct OrderSource<'a> {
    table: Option<&'a Table>,
    row: Option<usize>,
    group: Option<&'a [usize]>,
    schema: &'a Schema,
}

fn evaluate_order(
    order_by: &[OrderBy],
    output_identifiers: &[Identifier],
    values: &[Value],
    source: OrderSource<'_>,
    budget: &mut ByteBudget,
) -> Result<Vec<OrderValue>, DatabaseError> {
    budget.charge(
        size_of::<Vec<OrderValue>>()
            .saturating_add(order_by.len().saturating_mul(size_of::<OrderValue>())),
    )?;
    let mut order_values = Vec::with_capacity(order_by.len());
    for order in order_by {
        let value = if let Some(index) = output_reference_index(&order.expr, output_identifiers)? {
            Ok(values[index].clone())
        } else if let Some(group) = source.group {
            eval_group_expr(&order.expr, source.table, group, source.schema)
        } else {
            eval_row_expr(&order.expr, source.table, source.row)
        }?;
        budget.charge(estimate_value(&value))?;
        order_values.push(OrderValue {
            value,
            descending: order.descending,
        });
    }
    Ok(order_values)
}

#[derive(Debug)]
struct OrderValue {
    value: Value,
    descending: bool,
}

#[derive(Debug)]
struct Record {
    values: Vec<Value>,
    order: Vec<OrderValue>,
    ordinal: usize,
}

impl Record {
    fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_mul(2)
            .saturating_add(estimate_values(&self.values))
            .saturating_add(size_of::<Vec<OrderValue>>())
            .saturating_add(
                self.order
                    .capacity()
                    .saturating_mul(size_of::<OrderValue>()),
            )
            .saturating_add(
                self.order
                    .iter()
                    .map(|order| estimate_value(&order.value))
                    .sum::<usize>(),
            )
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Record {}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> Ordering {
        for (left, right) in self.order.iter().zip(&other.order) {
            let ordering = value_order(&left.value, &right.value)
                .expect("ORDER BY expressions have statically compatible types");
            let ordering = if left.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        self.ordinal.cmp(&other.ordinal)
    }
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
            ..Limits::default()
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
