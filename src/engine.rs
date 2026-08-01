use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    BinaryOperator, ColumnOption, Distinct, DuplicateTreatment, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, GroupByExpr, HiveDistributionStyle, HiveFormat, Insert,
    ObjectName, OrderByExpr, Query, Select, SelectItem, SetExpr, Statement, TableFactor,
    UnaryOperator, Value as SqlValue, WildcardAdditionalOptions,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{Error, Result};
use crate::storage::{Field, Schema, Table, normalize_identifier};
use crate::types::{DataType, Value};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_input_bytes: usize,
    pub max_rows_per_insert: usize,
    pub max_rows_per_table: usize,
    pub max_result_rows: usize,
    pub max_batch_result_bytes: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_rows_per_insert: 100_000,
            max_rows_per_table: 1_000_000,
            max_result_rows: 100_000,
            max_batch_result_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    Command { affected_rows: usize },
    Query(QueryResult),
}

/// An isolated in-memory catalog and analytical execution engine.
#[derive(Debug)]
pub struct Engine {
    config: EngineConfig,
    tables: HashMap<String, Table>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            tables: HashMap::new(),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        let max_batch_result_bytes = self.config.max_batch_result_bytes;
        let mut retained_bytes = 0_usize;
        let mut results = Vec::new();
        for result in self.execute_iter(sql)? {
            let result = result?;
            retained_bytes = retained_bytes.saturating_add(result_retained_bytes(&result));
            if retained_bytes > max_batch_result_bytes {
                return Err(Error::ResourceLimit {
                    resource: "batch result bytes",
                    limit: max_batch_result_bytes,
                    actual: retained_bytes,
                });
            }
            results.push(result);
        }
        Ok(results)
    }

    /// Executes parsed statements one at a time so callers can release each
    /// result before the next statement runs.
    pub fn execute_iter(
        &mut self,
        sql: &str,
    ) -> Result<impl Iterator<Item = Result<StatementResult>> + '_> {
        if sql.len() > self.config.max_input_bytes {
            return Err(Error::ResourceLimit {
                resource: "SQL input bytes",
                limit: self.config.max_input_bytes,
                actual: sql.len(),
            });
        }
        let statements = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|error| Error::Sql(error.to_string()))?;
        let mut statements = statements.into_iter();
        let mut failed = false;
        Ok(std::iter::from_fn(move || {
            if failed {
                return None;
            }
            let statement = statements.next()?;
            let result = self.execute_statement(statement);
            failed = result.is_err();
            Some(result)
        }))
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&normalize_identifier(name))
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        validate_statement_options(&statement)?;
        match statement {
            Statement::CreateTable {
                name,
                columns,
                if_not_exists,
                query,
                ..
            } => {
                if query.is_some() {
                    return Err(Error::Unsupported("CREATE TABLE AS SELECT".into()));
                }
                let name = object_name(&name)?;
                let key = normalize_identifier(&name);
                if self.tables.contains_key(&key) {
                    return if if_not_exists {
                        Ok(StatementResult::Command { affected_rows: 0 })
                    } else {
                        Err(Error::TableExists(name))
                    };
                }
                let fields = columns
                    .into_iter()
                    .map(|column| {
                        let (data_type, type_nullable) =
                            parse_data_type(&column.data_type.to_string())?;
                        let mut nullable = type_nullable;
                        for option in column.options {
                            match option.option {
                                ColumnOption::Null => nullable = true,
                                ColumnOption::NotNull => nullable = false,
                                _ => {}
                            }
                        }
                        Ok(Field {
                            name: column.name.value,
                            data_type,
                            nullable,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.tables.insert(key, Table::new(Schema::new(fields)?));
                Ok(StatementResult::Command { affected_rows: 0 })
            }
            Statement::Insert(insert) => {
                let table_name = object_name(&insert.table_name)?;
                let table_key = normalize_identifier(&table_name);
                let source = insert
                    .source
                    .ok_or_else(|| Error::Unsupported("INSERT without VALUES".into()))?;
                let SetExpr::Values(values) = *source.body else {
                    return Err(Error::Unsupported("INSERT source must be VALUES".into()));
                };
                if values.rows.len() > self.config.max_rows_per_insert {
                    return Err(Error::ResourceLimit {
                        resource: "rows per INSERT",
                        limit: self.config.max_rows_per_insert,
                        actual: values.rows.len(),
                    });
                }

                let table = self
                    .tables
                    .get(&table_key)
                    .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
                let attempted = table.row_count().saturating_add(values.rows.len());
                if attempted > self.config.max_rows_per_table {
                    return Err(Error::ResourceLimit {
                        resource: "rows per table",
                        limit: self.config.max_rows_per_table,
                        actual: attempted,
                    });
                }
                let column_indexes = insert
                    .columns
                    .iter()
                    .map(|column| table.schema().index_of(&column.value))
                    .collect::<Result<Vec<_>>>()?;
                if column_indexes.iter().copied().collect::<HashSet<_>>().len()
                    != column_indexes.len()
                {
                    return Err(Error::Constraint(
                        "an INSERT column may only be specified once".into(),
                    ));
                }
                let mut rows = Vec::with_capacity(values.rows.len());
                for expressions in values.rows {
                    let values = expressions
                        .iter()
                        .map(eval_constant)
                        .collect::<Result<Vec<_>>>()?;
                    if insert.columns.is_empty() {
                        rows.push(values);
                    } else {
                        if values.len() != column_indexes.len() {
                            return Err(Error::Constraint(format!(
                                "expected {} values, found {}",
                                column_indexes.len(),
                                values.len()
                            )));
                        }
                        let mut row = vec![Value::Null; table.schema().len()];
                        for (column, value) in column_indexes.iter().copied().zip(values) {
                            row[column] = value;
                        }
                        rows.push(row);
                    }
                }
                let affected_rows = rows.len();
                self.tables
                    .get_mut(&table_key)
                    .ok_or(Error::TableNotFound(table_name))?
                    .append_rows(rows)?;
                Ok(StatementResult::Command { affected_rows })
            }
            Statement::Query(query) => self.execute_query(*query).map(StatementResult::Query),
            other => Err(Error::Unsupported(other.to_string())),
        }
    }

    fn execute_query(&self, query: Query) -> Result<QueryResult> {
        if query.with.is_some()
            || query.offset.is_some()
            || query.fetch.is_some()
            || !query.limit_by.is_empty()
            || !query.locks.is_empty()
            || query.for_clause.is_some()
        {
            return Err(Error::Unsupported(
                "WITH, OFFSET, FETCH, LIMIT BY, and locking clauses".into(),
            ));
        }
        let SetExpr::Select(select) = *query.body else {
            return Err(Error::Unsupported("set operations and subqueries".into()));
        };
        self.execute_select(*select, &query.order_by, query.limit.as_ref())
    }

    fn execute_select(
        &self,
        select: Select,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
    ) -> Result<QueryResult> {
        if matches!(&select.distinct, Some(Distinct::On(_))) {
            return Err(Error::Unsupported("DISTINCT ON".into()));
        }
        if select.top.is_some()
            || select.into.is_some()
            || !select.lateral_views.is_empty()
            || !select.cluster_by.is_empty()
            || !select.distribute_by.is_empty()
            || !select.sort_by.is_empty()
            || !select.named_window.is_empty()
            || select.qualify.is_some()
            || select.value_table_mode.is_some()
            || select.connect_by.is_some()
        {
            return Err(Error::Unsupported("advanced SELECT clause".into()));
        }
        if select.from.len() > 1 {
            return Err(Error::Unsupported("multiple FROM items and joins".into()));
        }
        let source = if let Some(from) = select.from.first() {
            if !from.joins.is_empty() {
                return Err(Error::Unsupported("JOIN".into()));
            }
            let TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                partitions,
            } = &from.relation
            else {
                return Err(Error::Unsupported(
                    "derived and table-function sources".into(),
                ));
            };
            if args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
                || !partitions.is_empty()
                || alias
                    .as_ref()
                    .is_some_and(|alias| !alias.columns.is_empty())
            {
                return Err(Error::Unsupported("table source option".into()));
            }
            let name = object_name(name)?;
            let table = self
                .tables
                .get(&normalize_identifier(&name))
                .ok_or_else(|| Error::TableNotFound(name.clone()))?;
            let qualifier = alias
                .as_ref()
                .map_or(name, |alias| alias.name.value.clone());
            EvalSource {
                table: Some(table),
                qualifier: Some(normalize_identifier(&qualifier)),
            }
        } else {
            EvalSource {
                table: None,
                qualifier: None,
            }
        };

        let projections = expand_projections(&select.projection, &source)?;
        let group_by = match select.group_by {
            GroupByExpr::Expressions(expressions) => expressions,
            GroupByExpr::All => {
                return Err(Error::Unsupported("GROUP BY ALL".into()));
            }
        };
        let group_by = resolve_group_by(&group_by, &projections)?;
        if group_by.iter().any(contains_aggregate) {
            return Err(Error::Constraint(
                "aggregate functions are not allowed in GROUP BY".into(),
            ));
        }
        let alias_expressions = projections
            .iter()
            .map(|projection| (normalize_identifier(&projection.header), &projection.expr))
            .collect::<HashMap<_, _>>();
        for projection in &projections {
            validate_expression_references(&projection.expr, &source, &HashMap::new())?;
        }
        if let Some(selection) = &select.selection {
            validate_expression_references(selection, &source, &HashMap::new())?;
        }
        for expression in &group_by {
            validate_expression_references(expression, &source, &HashMap::new())?;
        }
        if let Some(having) = &select.having {
            validate_expression_references(having, &source, &alias_expressions)?;
        }
        for order in order_by {
            validate_expression_references(&order.expr, &source, &alias_expressions)?;
        }
        let grouped = !group_by.is_empty()
            || projections
                .iter()
                .any(|item| contains_aggregate(&item.expr))
            || select.having.as_ref().is_some_and(contains_aggregate)
            || order_by.iter().any(|item| contains_aggregate(&item.expr));
        if grouped {
            for projection in &projections {
                validate_grouped_expression(&projection.expr, &group_by, &HashMap::new(), false)?;
            }
            if let Some(having) = &select.having {
                validate_grouped_expression(having, &group_by, &alias_expressions, false)?;
            }
            for order in order_by {
                validate_grouped_expression(&order.expr, &group_by, &alias_expressions, false)?;
            }
        }

        let mut filtered_rows = Vec::new();
        let source_rows = source.table.map_or(1, Table::row_count);
        for row in 0..source_rows {
            if let Some(predicate) = &select.selection
                && eval_row(predicate, &source, Some(row))?.sql_bool()? != Some(true)
            {
                continue;
            }
            filtered_rows.push(row);
        }
        let groups = if grouped {
            make_groups(&source, filtered_rows, &group_by)?
        } else {
            filtered_rows.into_iter().map(|row| vec![row]).collect()
        };

        let columns = projections
            .iter()
            .map(|projection| projection.header.clone())
            .collect::<Vec<_>>();
        let mut projected = Vec::new();
        let mut projected_bytes = columns_retained_bytes(&columns);
        for rows in groups {
            let mut values = Vec::with_capacity(projections.len());
            for projection in &projections {
                values.push(eval_group(
                    &projection.expr,
                    &source,
                    &rows,
                    &HashMap::new(),
                )?);
            }
            projected_bytes = projected_bytes.saturating_add(row_retained_bytes(&values));
            if projected_bytes > self.config.max_batch_result_bytes {
                return Err(Error::ResourceLimit {
                    resource: "result bytes",
                    limit: self.config.max_batch_result_bytes,
                    actual: projected_bytes,
                });
            }
            let aliases = columns
                .iter()
                .cloned()
                .zip(values.iter().cloned())
                .map(|(name, value)| (normalize_identifier(&name), value))
                .collect::<HashMap<_, _>>();
            if let Some(having) = &select.having
                && eval_group(having, &source, &rows, &aliases)?.sql_bool()? != Some(true)
            {
                continue;
            }
            projected.push(ProjectedRow {
                values,
                source_rows: rows,
                sort_keys: Vec::new(),
            });
        }

        if select.distinct.is_some() {
            let mut seen = HashSet::new();
            projected.retain(|row| seen.insert(row_key(&row.values)));
        }

        if !order_by.is_empty() {
            for row in &mut projected {
                let aliases = columns
                    .iter()
                    .cloned()
                    .zip(row.values.iter().cloned())
                    .map(|(name, value)| (normalize_identifier(&name), value))
                    .collect::<HashMap<_, _>>();
                row.sort_keys = order_by
                    .iter()
                    .map(|order| {
                        if let Some(index) = order_ordinal(&order.expr, columns.len())? {
                            return Ok(row.values[index].clone());
                        }
                        if let Expr::Identifier(identifier) = &order.expr
                            && let Some((index, _)) =
                                columns.iter().enumerate().find(|(_, name)| {
                                    normalize_identifier(name)
                                        == normalize_identifier(&identifier.value)
                                })
                        {
                            return Ok(row.values[index].clone());
                        }
                        eval_group(&order.expr, &source, &row.source_rows, &aliases)
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            validate_sort_types(&projected)?;
            projected.sort_by(|left, right| compare_projected(left, right, order_by));
        }

        let limit = limit.map(parse_limit).transpose()?.unwrap_or(usize::MAX);
        projected.truncate(limit);
        if projected.len() > self.config.max_result_rows {
            return Err(Error::ResourceLimit {
                resource: "result rows",
                limit: self.config.max_result_rows,
                actual: projected.len(),
            });
        }
        let result = QueryResult {
            columns,
            rows: projected.into_iter().map(|row| row.values).collect(),
        };
        let result_bytes = query_result_retained_bytes(&result);
        if result_bytes > self.config.max_batch_result_bytes {
            return Err(Error::ResourceLimit {
                resource: "result bytes",
                limit: self.config.max_batch_result_bytes,
                actual: result_bytes,
            });
        }
        Ok(result)
    }
}

struct EvalSource<'a> {
    table: Option<&'a Table>,
    qualifier: Option<String>,
}

impl EvalSource<'_> {
    fn validate_qualifier(&self, qualifier: &str) -> Result<()> {
        let expected = self
            .qualifier
            .as_ref()
            .ok_or_else(|| Error::ColumnNotFound(format!("{qualifier}.*")))?;
        if expected == &normalize_identifier(qualifier) {
            Ok(())
        } else {
            Err(Error::ColumnNotFound(format!("{qualifier}.*")))
        }
    }
}

struct Projection {
    expr: Expr,
    header: String,
}

struct ProjectedRow {
    values: Vec<Value>,
    source_rows: Vec<usize>,
    sort_keys: Vec<Value>,
}

fn validate_statement_options(statement: &Statement) -> Result<()> {
    match statement {
        Statement::CreateTable {
            or_replace,
            temporary,
            external,
            global,
            transient,
            columns,
            constraints,
            hive_distribution,
            hive_formats,
            table_properties,
            with_options,
            file_format,
            location,
            query,
            without_rowid,
            like,
            clone,
            engine,
            comment,
            auto_increment_offset,
            default_charset,
            collation,
            on_commit,
            on_cluster,
            order_by,
            partition_by,
            cluster_by,
            options,
            strict,
            ..
        } => {
            let unsupported_table_option = *or_replace
                || *temporary
                || *external
                || global.is_some()
                || *transient
                || !constraints.is_empty()
                || !matches!(hive_distribution, HiveDistributionStyle::NONE)
                || hive_formats
                    .as_ref()
                    .is_some_and(|format| format != &HiveFormat::default())
                || !table_properties.is_empty()
                || !with_options.is_empty()
                || file_format.is_some()
                || location.is_some()
                || query.is_some()
                || *without_rowid
                || like.is_some()
                || clone.is_some()
                || engine
                    .as_ref()
                    .is_some_and(|engine| normalize_identifier(engine.trim()) != "memory")
                || comment.is_some()
                || auto_increment_offset.is_some()
                || default_charset.is_some()
                || collation.is_some()
                || on_commit.is_some()
                || on_cluster.is_some()
                || order_by.is_some()
                || partition_by.is_some()
                || cluster_by.is_some()
                || options.is_some()
                || *strict;
            if unsupported_table_option {
                return Err(Error::Unsupported(
                    "CREATE TABLE option or table constraint".into(),
                ));
            }
            for column in columns {
                if column.collation.is_some() {
                    return Err(Error::Unsupported("column collation".into()));
                }
                for option in &column.options {
                    if option.name.is_some()
                        || !matches!(option.option, ColumnOption::Null | ColumnOption::NotNull)
                    {
                        return Err(Error::Unsupported(format!(
                            "column option {}",
                            option.option
                        )));
                    }
                }
            }
            Ok(())
        }
        Statement::Insert(insert) => validate_insert_options(insert),
        _ => Ok(()),
    }
}

fn validate_insert_options(insert: &Insert) -> Result<()> {
    if insert.or.is_some()
        || insert.ignore
        || insert.table_alias.is_some()
        || insert.overwrite
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.table
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.replace_into
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
    {
        return Err(Error::Unsupported("INSERT option or RETURNING".into()));
    }
    if let Some(source) = &insert.source
        && (source.with.is_some()
            || !source.order_by.is_empty()
            || source.limit.is_some()
            || !source.limit_by.is_empty()
            || source.offset.is_some()
            || source.fetch.is_some()
            || !source.locks.is_empty()
            || source.for_clause.is_some())
    {
        return Err(Error::Unsupported("clause on INSERT VALUES".into()));
    }
    Ok(())
}

fn result_retained_bytes(result: &StatementResult) -> usize {
    match result {
        StatementResult::Command { .. } => std::mem::size_of::<StatementResult>(),
        StatementResult::Query(result) => query_result_retained_bytes(result),
    }
}

fn query_result_retained_bytes(result: &QueryResult) -> usize {
    result
        .rows
        .iter()
        .fold(columns_retained_bytes(&result.columns), |size, row| {
            size.saturating_add(row_retained_bytes(row))
        })
}

fn columns_retained_bytes(columns: &[String]) -> usize {
    columns.iter().fold(0_usize, |size, column| {
        size.saturating_add(std::mem::size_of::<String>())
            .saturating_add(column.len())
    })
}

fn row_retained_bytes(row: &[Value]) -> usize {
    row.iter()
        .fold(std::mem::size_of::<Vec<Value>>(), |size, value| {
            size.saturating_add(std::mem::size_of::<Value>())
                .saturating_add(match value {
                    Value::String(value) => value.len(),
                    _ => 0,
                })
        })
}

fn object_name(name: &ObjectName) -> Result<String> {
    if name.0.len() != 1 {
        return Err(Error::Unsupported(format!("qualified object name {name}")));
    }
    Ok(name.0[0].value.clone())
}

fn parse_data_type(sql: &str) -> Result<(DataType, bool)> {
    let compact = sql.to_ascii_lowercase().replace(' ', "");
    let (name, nullable) = compact
        .strip_prefix("nullable(")
        .and_then(|name| name.strip_suffix(')'))
        .map_or((compact.as_str(), false), |name| (name, true));
    let data_type = match name {
        "tinyint" | "smallint" | "int" | "integer" | "bigint" | "int8" | "int16" | "int32"
        | "int64" | "uint8" | "uint16" | "uint32" | "uint64" => DataType::Int64,
        "float" | "float32" | "float64" | "double" | "doubleprecision" | "real" => {
            DataType::Float64
        }
        "bool" | "boolean" => DataType::Bool,
        name if name == "string"
            || name == "text"
            || name.starts_with("varchar")
            || name.starts_with("char") =>
        {
            DataType::String
        }
        _ => return Err(Error::Unsupported(format!("data type {sql}"))),
    };
    Ok((data_type, nullable))
}

fn validate_wildcard_options(options: &WildcardAdditionalOptions) -> Result<()> {
    if options == &WildcardAdditionalOptions::default() {
        Ok(())
    } else {
        Err(Error::Unsupported("wildcard modifiers".into()))
    }
}

fn expand_projections(items: &[SelectItem], source: &EvalSource<'_>) -> Result<Vec<Projection>> {
    let mut projections = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => projections.push(Projection {
                header: expr.to_string(),
                expr: expr.clone(),
            }),
            SelectItem::ExprWithAlias { expr, alias } => projections.push(Projection {
                header: alias.value.clone(),
                expr: expr.clone(),
            }),
            SelectItem::Wildcard(options) => {
                validate_wildcard_options(options)?;
                let table = source
                    .table
                    .ok_or_else(|| Error::Constraint("wildcard requires a FROM table".into()))?;
                for field in table.schema().fields() {
                    projections.push(Projection {
                        expr: Expr::Identifier(sqlparser::ast::Ident::new(&field.name)),
                        header: field.name.clone(),
                    });
                }
            }
            SelectItem::QualifiedWildcard(qualifier, options) => {
                validate_wildcard_options(options)?;
                source.validate_qualifier(&object_name(qualifier)?)?;
                let table = source
                    .table
                    .ok_or_else(|| Error::Constraint("wildcard requires a FROM table".into()))?;
                for field in table.schema().fields() {
                    projections.push(Projection {
                        expr: Expr::Identifier(sqlparser::ast::Ident::new(&field.name)),
                        header: field.name.clone(),
                    });
                }
            }
        }
    }
    Ok(projections)
}

fn resolve_group_by(group_by: &[Expr], projections: &[Projection]) -> Result<Vec<Expr>> {
    group_by
        .iter()
        .map(|expr| {
            if let Some(index) = order_ordinal(expr, projections.len())? {
                return Ok(projections[index].expr.clone());
            }
            if let Expr::Identifier(identifier) = expr
                && let Some(projection) = projections.iter().find(|projection| {
                    normalize_identifier(&projection.header)
                        == normalize_identifier(&identifier.value)
                })
            {
                return Ok(projection.expr.clone());
            }
            Ok(expr.clone())
        })
        .collect()
}

fn validate_expression_references(
    expr: &Expr,
    source: &EvalSource<'_>,
    aliases: &HashMap<String, &Expr>,
) -> Result<()> {
    match expr {
        Expr::Identifier(identifier) => {
            if aliases.contains_key(&normalize_identifier(&identifier.value)) {
                return Ok(());
            }
            source
                .table
                .ok_or_else(|| Error::ColumnNotFound(identifier.value.clone()))?
                .schema()
                .index_of(&identifier.value)
                .map(|_| ())
        }
        Expr::CompoundIdentifier(identifiers) => {
            if identifiers.len() != 2 {
                return Err(Error::ColumnNotFound(expr.to_string()));
            }
            source.validate_qualifier(&identifiers[0].value)?;
            source
                .table
                .ok_or_else(|| Error::ColumnNotFound(expr.to_string()))?
                .schema()
                .index_of(&identifiers[1].value)
                .map(|_| ())
        }
        Expr::Value(_) => Ok(()),
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => validate_expression_references(expr, source, aliases),
        Expr::BinaryOp { left, right, .. } => {
            validate_expression_references(left, source, aliases)?;
            validate_expression_references(right, source, aliases)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expression_references(expr, source, aliases)?;
            validate_expression_references(low, source, aliases)?;
            validate_expression_references(high, source, aliases)
        }
        Expr::InList { expr, list, .. } => {
            validate_expression_references(expr, source, aliases)?;
            for item in list {
                validate_expression_references(item, source, aliases)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            validate_expression_references(expr, source, aliases)?;
            validate_expression_references(pattern, source, aliases)
        }
        Expr::Function(function) => {
            let (_, arguments, _) = function_parts(function)?;
            for argument in arguments {
                match unnamed_argument(argument)? {
                    FunctionArgExpr::Expr(expr) => {
                        validate_expression_references(expr, source, aliases)?;
                    }
                    FunctionArgExpr::QualifiedWildcard(qualifier) => {
                        source.validate_qualifier(&object_name(qualifier)?)?;
                    }
                    FunctionArgExpr::Wildcard => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_grouped_expression(
    expr: &Expr,
    group_by: &[Expr],
    aliases: &HashMap<String, &Expr>,
    inside_aggregate: bool,
) -> Result<()> {
    if group_by
        .iter()
        .any(|group_expr| equivalent_group_expression(expr, group_expr))
    {
        return Ok(());
    }
    match expr {
        Expr::Identifier(identifier) => {
            if let Some(alias) = aliases.get(&normalize_identifier(&identifier.value)) {
                return validate_grouped_expression(alias, group_by, &HashMap::new(), false);
            }
            Err(Error::Constraint(format!(
                "column {} must appear in GROUP BY or an aggregate function",
                identifier.value
            )))
        }
        Expr::CompoundIdentifier(_) => Err(Error::Constraint(format!(
            "column {expr} must appear in GROUP BY or an aggregate function"
        ))),
        Expr::Value(_) => Ok(()),
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => {
            validate_grouped_expression(expr, group_by, aliases, inside_aggregate)
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_grouped_expression(left, group_by, aliases, inside_aggregate)?;
            validate_grouped_expression(right, group_by, aliases, inside_aggregate)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_grouped_expression(expr, group_by, aliases, inside_aggregate)?;
            validate_grouped_expression(low, group_by, aliases, inside_aggregate)?;
            validate_grouped_expression(high, group_by, aliases, inside_aggregate)
        }
        Expr::InList { expr, list, .. } => {
            validate_grouped_expression(expr, group_by, aliases, inside_aggregate)?;
            for item in list {
                validate_grouped_expression(item, group_by, aliases, inside_aggregate)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            validate_grouped_expression(expr, group_by, aliases, inside_aggregate)?;
            validate_grouped_expression(pattern, group_by, aliases, inside_aggregate)
        }
        Expr::Function(function) => {
            let (name, arguments, _) = function_parts(function)?;
            let aggregate = is_aggregate_name(&name);
            if aggregate && inside_aggregate {
                return Err(Error::Constraint(
                    "aggregate functions may not be nested".into(),
                ));
            }
            if aggregate {
                for argument in arguments {
                    if let FunctionArgExpr::Expr(expr) = unnamed_argument(argument)?
                        && contains_aggregate(expr)
                    {
                        return Err(Error::Constraint(
                            "aggregate functions may not be nested".into(),
                        ));
                    }
                }
                return Ok(());
            }
            for argument in arguments {
                match unnamed_argument(argument)? {
                    FunctionArgExpr::Expr(expr) => {
                        validate_grouped_expression(expr, group_by, aliases, inside_aggregate)?;
                    }
                    _ => {
                        return Err(Error::Unsupported(format!("wildcard argument to {name}")));
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn equivalent_group_expression(left: &Expr, right: &Expr) -> bool {
    if left == right {
        return true;
    }
    match (column_reference(left), column_reference(right)) {
        (Some(left), Some(right)) => normalize_identifier(left) == normalize_identifier(right),
        _ => false,
    }
}

fn column_reference(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(identifier) => Some(&identifier.value),
        Expr::CompoundIdentifier(identifiers) if identifiers.len() == 2 => {
            Some(&identifiers[1].value)
        }
        _ => None,
    }
}

fn eval_constant(expr: &Expr) -> Result<Value> {
    eval_row(
        expr,
        &EvalSource {
            table: None,
            qualifier: None,
        },
        None,
    )
}

fn eval_row(expr: &Expr, source: &EvalSource<'_>, row: Option<usize>) -> Result<Value> {
    match expr {
        Expr::Value(value) => sql_value(value),
        Expr::Identifier(identifier) => lookup_column(source, row, &identifier.value),
        Expr::CompoundIdentifier(identifiers) => {
            if identifiers.len() != 2 {
                return Err(Error::ColumnNotFound(expr.to_string()));
            }
            source.validate_qualifier(&identifiers[0].value)?;
            lookup_column(source, row, &identifiers[1].value)
        }
        Expr::Nested(expr) => eval_row(expr, source, row),
        Expr::UnaryOp { op, expr } => eval_unary(op, eval_row(expr, source, row)?),
        Expr::BinaryOp { left, op, right } => {
            let left = eval_row(left, source, row)?;
            let right = eval_row(right, source, row)?;
            eval_binary(left, op, right)
        }
        Expr::IsNull(expr) => Ok(Value::Bool(eval_row(expr, source, row)?.is_null())),
        Expr::IsNotNull(expr) => Ok(Value::Bool(!eval_row(expr, source, row)?.is_null())),
        Expr::IsTrue(expr) => Ok(Value::Bool(
            eval_row(expr, source, row)?.sql_bool()? == Some(true),
        )),
        Expr::IsFalse(expr) => Ok(Value::Bool(
            eval_row(expr, source, row)?.sql_bool()? == Some(false),
        )),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(
            eval_row(expr, source, row)?.sql_bool()? != Some(true),
        )),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(
            eval_row(expr, source, row)?.sql_bool()? != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = eval_row(expr, source, row)?;
            let lower = eval_binary(
                value.clone(),
                &BinaryOperator::GtEq,
                eval_row(low, source, row)?,
            )?;
            let upper = eval_binary(value, &BinaryOperator::LtEq, eval_row(high, source, row)?)?;
            let result = eval_binary(lower, &BinaryOperator::And, upper)?;
            if *negated {
                eval_unary(&UnaryOperator::Not, result)
            } else {
                Ok(result)
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_row(expr, source, row)?;
            let mut result = Value::Bool(false);
            for item in list {
                let equal = eval_binary(
                    needle.clone(),
                    &BinaryOperator::Eq,
                    eval_row(item, source, row)?,
                )?;
                if equal == Value::Bool(true) {
                    result = equal;
                    break;
                }
                if equal.is_null() {
                    result = Value::Null;
                }
            }
            if *negated {
                eval_unary(&UnaryOperator::Not, result)
            } else {
                Ok(result)
            }
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => eval_like(
            eval_row(expr, source, row)?,
            eval_row(pattern, source, row)?,
            *negated,
            false,
            escape_char.as_deref(),
        ),
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => eval_like(
            eval_row(expr, source, row)?,
            eval_row(pattern, source, row)?,
            *negated,
            true,
            escape_char.as_deref(),
        ),
        Expr::Cast {
            expr, data_type, ..
        } => cast_value(
            eval_row(expr, source, row)?,
            parse_data_type(&data_type.to_string())?.0,
        ),
        Expr::Function(function) => eval_scalar_function(function, source, row),
        _ => Err(Error::Unsupported(format!("expression {expr}"))),
    }
}

fn lookup_column(source: &EvalSource<'_>, row: Option<usize>, name: &str) -> Result<Value> {
    let table = source
        .table
        .ok_or_else(|| Error::ColumnNotFound(name.into()))?;
    let column = table.schema().index_of(name)?;
    let row = row.ok_or_else(|| Error::ColumnNotFound(name.into()))?;
    Ok(table.value(row, column))
}

fn sql_value(value: &SqlValue) -> Result<Value> {
    match value {
        SqlValue::Number(value, _) => {
            if value.contains(['.', 'e', 'E']) {
                value
                    .parse::<f64>()
                    .map(Value::Float64)
                    .map_err(|_| Error::Type(format!("invalid Float64 literal {value}")))
            } else {
                value
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| Error::Type(format!("invalid Int64 literal {value}")))
            }
        }
        SqlValue::SingleQuotedString(value)
        | SqlValue::DoubleQuotedString(value)
        | SqlValue::EscapedStringLiteral(value)
        | SqlValue::NationalStringLiteral(value)
        | SqlValue::RawStringLiteral(value) => Ok(Value::String(value.clone())),
        SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
        SqlValue::Null => Ok(Value::Null),
        _ => Err(Error::Unsupported(format!("literal {value}"))),
    }
}

fn eval_unary(operator: &UnaryOperator, value: Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match (operator, value) {
        (UnaryOperator::Plus, value @ (Value::Int64(_) | Value::Float64(_))) => Ok(value),
        (UnaryOperator::Minus, Value::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::Type("Int64 negation overflow".into())),
        (UnaryOperator::Minus, Value::Float64(value)) => Ok(Value::Float64(-value)),
        (UnaryOperator::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (operator, value) => Err(Error::Type(format!(
            "operator {operator} does not accept {}",
            value.type_name()
        ))),
    }
}

fn eval_binary(left: Value, operator: &BinaryOperator, right: Value) -> Result<Value> {
    if matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
        return eval_boolean(left, operator, right);
    }
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    match operator {
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq => {
            let ordering = left.sql_cmp(&right)?;
            let value = match operator {
                BinaryOperator::Eq => ordering == Ordering::Equal,
                BinaryOperator::NotEq => ordering != Ordering::Equal,
                BinaryOperator::Gt => ordering == Ordering::Greater,
                BinaryOperator::GtEq => ordering != Ordering::Less,
                BinaryOperator::Lt => ordering == Ordering::Less,
                BinaryOperator::LtEq => ordering != Ordering::Greater,
                _ => unreachable!(),
            };
            Ok(Value::Bool(value))
        }
        BinaryOperator::Plus => numeric_arithmetic(left, right, i64::checked_add, |a, b| a + b),
        BinaryOperator::Minus => numeric_arithmetic(left, right, i64::checked_sub, |a, b| a - b),
        BinaryOperator::Multiply => numeric_arithmetic(left, right, i64::checked_mul, |a, b| a * b),
        BinaryOperator::Divide => numeric_divide(left, right),
        BinaryOperator::Modulo => numeric_modulo(left, right),
        BinaryOperator::StringConcat => match (left, right) {
            (Value::String(left), Value::String(right)) => Ok(Value::String(left + &right)),
            (left, right) => Err(Error::Type(format!(
                "cannot concatenate {} and {}",
                left.type_name(),
                right.type_name()
            ))),
        },
        _ => Err(Error::Unsupported(format!("operator {operator}"))),
    }
}

fn eval_boolean(left: Value, operator: &BinaryOperator, right: Value) -> Result<Value> {
    let left = left.sql_bool()?;
    let right = right.sql_bool()?;
    let result = match operator {
        BinaryOperator::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOperator::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        _ => unreachable!(),
    };
    Ok(result.map_or(Value::Null, Value::Bool))
}

fn numeric_arithmetic(
    left: Value,
    right: Value,
    integer: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> Result<Value> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => integer(left, right)
            .map(Value::Int64)
            .ok_or_else(|| Error::Type("Int64 arithmetic overflow".into())),
        (Value::Int64(left), Value::Float64(right)) => {
            Ok(Value::Float64(float(left as f64, right)))
        }
        (Value::Float64(left), Value::Int64(right)) => {
            Ok(Value::Float64(float(left, right as f64)))
        }
        (Value::Float64(left), Value::Float64(right)) => Ok(Value::Float64(float(left, right))),
        (left, right) => Err(Error::Type(format!(
            "numeric operator cannot combine {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

fn numeric_divide(left: Value, right: Value) -> Result<Value> {
    let (left, right) = match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => (left as f64, right as f64),
        (Value::Int64(left), Value::Float64(right)) => (left as f64, right),
        (Value::Float64(left), Value::Int64(right)) => (left, right as f64),
        (Value::Float64(left), Value::Float64(right)) => (left, right),
        (left, right) => {
            return Err(Error::Type(format!(
                "division cannot combine {} and {}",
                left.type_name(),
                right.type_name()
            )));
        }
    };
    if right == 0.0 {
        return Err(Error::Type("division by zero".into()));
    }
    Ok(Value::Float64(left / right))
}

fn numeric_modulo(left: Value, right: Value) -> Result<Value> {
    match (left, right) {
        (Value::Int64(_), Value::Int64(0)) => Err(Error::Type("modulo by zero".into())),
        (Value::Int64(left), Value::Int64(right)) => left
            .checked_rem(right)
            .map(Value::Int64)
            .ok_or_else(|| Error::Type("Int64 modulo overflow".into())),
        (Value::Int64(_), Value::Float64(0.0))
        | (Value::Float64(_), Value::Int64(0))
        | (Value::Float64(_), Value::Float64(0.0)) => Err(Error::Type("modulo by zero".into())),
        (Value::Int64(left), Value::Float64(right)) => Ok(Value::Float64((left as f64) % right)),
        (Value::Float64(left), Value::Int64(right)) => Ok(Value::Float64(left % right as f64)),
        (Value::Float64(left), Value::Float64(right)) => Ok(Value::Float64(left % right)),
        (left, right) => Err(Error::Type(format!(
            "modulo cannot combine {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

fn cast_value(value: Value, target: DataType) -> Result<Value> {
    if value.is_null() {
        return Ok(value);
    }
    match (value, target) {
        (value @ Value::Int64(_), DataType::Int64)
        | (value @ Value::Float64(_), DataType::Float64)
        | (value @ Value::Bool(_), DataType::Bool)
        | (value @ Value::String(_), DataType::String) => Ok(value),
        (Value::Int64(value), DataType::Float64) => Ok(Value::Float64(value as f64)),
        (Value::Float64(value), DataType::Int64)
            if value.is_finite() && value >= i64::MIN as f64 && value < i64::MAX as f64 =>
        {
            Ok(Value::Int64(value as i64))
        }
        (value, DataType::String) => Ok(Value::String(value.to_string())),
        (Value::String(value), DataType::Int64) => value
            .parse()
            .map(Value::Int64)
            .map_err(|_| Error::Type(format!("cannot cast {value:?} to Int64"))),
        (Value::String(value), DataType::Float64) => value
            .parse()
            .map(Value::Float64)
            .map_err(|_| Error::Type(format!("cannot cast {value:?} to Float64"))),
        (Value::String(value), DataType::Bool) => value
            .parse()
            .map(Value::Bool)
            .map_err(|_| Error::Type(format!("cannot cast {value:?} to Bool"))),
        (value, target) => Err(Error::Type(format!(
            "cannot cast {} to {target}",
            value.type_name()
        ))),
    }
}

fn eval_like(
    value: Value,
    pattern: Value,
    negated: bool,
    insensitive: bool,
    escape: Option<&str>,
) -> Result<Value> {
    if value.is_null() || pattern.is_null() {
        return Ok(Value::Null);
    }
    let (Value::String(mut value), Value::String(mut pattern)) = (value, pattern) else {
        return Err(Error::Type("LIKE expects String operands".into()));
    };
    if insensitive {
        value = value.to_lowercase();
        pattern = pattern.to_lowercase();
    }
    let escape = escape
        .map(|escape| {
            let mut characters = escape.chars();
            let character = characters
                .next()
                .ok_or_else(|| Error::Constraint("LIKE ESCAPE must be one character".into()))?;
            if characters.next().is_some() {
                return Err(Error::Constraint(
                    "LIKE ESCAPE must be one character".into(),
                ));
            }
            Ok(character)
        })
        .transpose()?;
    let matched = like_matches(&value, &pattern, escape)?;
    Ok(Value::Bool(if negated { !matched } else { matched }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Literal(char),
    AnyOne,
    AnyMany,
}

fn like_matches(value: &str, pattern: &str, escape: Option<char>) -> Result<bool> {
    let value = value.chars().collect::<Vec<_>>();
    let mut tokens = Vec::with_capacity(pattern.chars().count());
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        if Some(character) == escape {
            let literal = characters.next().ok_or_else(|| {
                Error::Constraint("LIKE pattern ends with its escape character".into())
            })?;
            tokens.push(LikeToken::Literal(literal));
        } else {
            tokens.push(match character {
                '_' => LikeToken::AnyOne,
                '%' => LikeToken::AnyMany,
                character => LikeToken::Literal(character),
            });
        }
    }
    let (mut value_index, mut pattern_index) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while value_index < value.len() {
        if tokens.get(pattern_index) == Some(&LikeToken::AnyOne)
            || tokens.get(pattern_index) == Some(&LikeToken::Literal(value[value_index]))
        {
            value_index += 1;
            pattern_index += 1;
        } else if tokens.get(pattern_index) == Some(&LikeToken::AnyMany) {
            star = Some(pattern_index);
            pattern_index += 1;
            retry = value_index;
        } else if let Some(star_index) = star {
            retry += 1;
            value_index = retry;
            pattern_index = star_index + 1;
        } else {
            return Ok(false);
        }
    }
    while tokens.get(pattern_index) == Some(&LikeToken::AnyMany) {
        pattern_index += 1;
    }
    Ok(pattern_index == tokens.len())
}

fn function_parts(function: &Function) -> Result<(String, &[FunctionArg], bool)> {
    let name = object_name(&function.name)?.to_ascii_lowercase();
    if function.over.is_some() || function.filter.is_some() || !function.within_group.is_empty() {
        return Err(Error::Unsupported(format!("function modifiers on {name}")));
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(Error::Unsupported(format!("arguments to {name}")));
    };
    if !arguments.clauses.is_empty() {
        return Err(Error::Unsupported(format!("argument clauses on {name}")));
    }
    Ok((
        name,
        &arguments.args,
        arguments.duplicate_treatment == Some(DuplicateTreatment::Distinct),
    ))
}

fn unnamed_argument(argument: &FunctionArg) -> Result<&FunctionArgExpr> {
    match argument {
        FunctionArg::Unnamed(argument) => Ok(argument),
        _ => Err(Error::Unsupported("named function argument".into())),
    }
}

fn eval_scalar_function(
    function: &Function,
    source: &EvalSource<'_>,
    row: Option<usize>,
) -> Result<Value> {
    eval_scalar_function_with(function, |expr| eval_row(expr, source, row))
}

fn eval_scalar_function_with(
    function: &Function,
    mut evaluate: impl FnMut(&Expr) -> Result<Value>,
) -> Result<Value> {
    let (name, arguments, distinct) = function_parts(function)?;
    if distinct {
        return Err(Error::Unsupported(format!(
            "DISTINCT on scalar function {name}"
        )));
    }
    let values = arguments
        .iter()
        .map(|argument| match unnamed_argument(argument)? {
            FunctionArgExpr::Expr(expr) => evaluate(expr),
            _ => Err(Error::Unsupported(format!("wildcard argument to {name}"))),
        })
        .collect::<Result<Vec<_>>>()?;
    match (name.as_str(), values.as_slice()) {
        ("abs", [Value::Int64(value)]) => value
            .checked_abs()
            .map(Value::Int64)
            .ok_or_else(|| Error::Type("Int64 absolute-value overflow".into())),
        ("abs", [Value::Float64(value)]) => Ok(Value::Float64(value.abs())),
        ("abs", [Value::Null]) => Ok(Value::Null),
        ("lower", [Value::String(value)]) => Ok(Value::String(value.to_lowercase())),
        ("lower", [Value::Null]) => Ok(Value::Null),
        ("upper", [Value::String(value)]) => Ok(Value::String(value.to_uppercase())),
        ("upper", [Value::Null]) => Ok(Value::Null),
        ("length", [Value::String(value)]) => Ok(Value::Int64(value.chars().count() as i64)),
        ("length", [Value::Null]) => Ok(Value::Null),
        ("coalesce", values) => Ok(values
            .iter()
            .find(|value| !value.is_null())
            .cloned()
            .unwrap_or(Value::Null)),
        _ => Err(Error::Unsupported(format!("scalar function {name}"))),
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => {
            if object_name(&function.name)
                .is_ok_and(|name| is_aggregate_name(&name.to_ascii_lowercase()))
            {
                return true;
            }
            let FunctionArguments::List(arguments) = &function.args else {
                return false;
            };
            arguments.args.iter().any(|argument| match argument {
                FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => match arg {
                    FunctionArgExpr::Expr(expr) => contains_aggregate(expr),
                    _ => false,
                },
            })
        }
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            contains_aggregate(expr) || contains_aggregate(pattern)
        }
        _ => false,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
}

fn eval_group(
    expr: &Expr,
    source: &EvalSource<'_>,
    rows: &[usize],
    aliases: &HashMap<String, Value>,
) -> Result<Value> {
    if let Expr::Identifier(identifier) = expr
        && let Some(value) = aliases.get(&normalize_identifier(&identifier.value))
    {
        return Ok(value.clone());
    }
    match expr {
        Expr::Value(value) => sql_value(value),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            eval_row(expr, source, rows.first().copied())
        }
        Expr::Function(function) => {
            let name = object_name(&function.name)?.to_ascii_lowercase();
            if is_aggregate_name(&name) {
                eval_aggregate(function, source, rows)
            } else {
                eval_scalar_function_with(function, |expr| eval_group(expr, source, rows, aliases))
            }
        }
        Expr::BinaryOp { left, op, right } => eval_binary(
            eval_group(left, source, rows, aliases)?,
            op,
            eval_group(right, source, rows, aliases)?,
        ),
        Expr::UnaryOp { op, expr } => eval_unary(op, eval_group(expr, source, rows, aliases)?),
        Expr::Nested(expr) => eval_group(expr, source, rows, aliases),
        Expr::IsNull(expr) => Ok(Value::Bool(
            eval_group(expr, source, rows, aliases)?.is_null(),
        )),
        Expr::IsNotNull(expr) => Ok(Value::Bool(
            !eval_group(expr, source, rows, aliases)?.is_null(),
        )),
        Expr::IsTrue(expr) => Ok(Value::Bool(
            eval_group(expr, source, rows, aliases)?.sql_bool()? == Some(true),
        )),
        Expr::IsFalse(expr) => Ok(Value::Bool(
            eval_group(expr, source, rows, aliases)?.sql_bool()? == Some(false),
        )),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(
            eval_group(expr, source, rows, aliases)?.sql_bool()? != Some(true),
        )),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(
            eval_group(expr, source, rows, aliases)?.sql_bool()? != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = eval_group(expr, source, rows, aliases)?;
            let lower = eval_binary(
                value.clone(),
                &BinaryOperator::GtEq,
                eval_group(low, source, rows, aliases)?,
            )?;
            let upper = eval_binary(
                value,
                &BinaryOperator::LtEq,
                eval_group(high, source, rows, aliases)?,
            )?;
            let result = eval_binary(lower, &BinaryOperator::And, upper)?;
            if *negated {
                eval_unary(&UnaryOperator::Not, result)
            } else {
                Ok(result)
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_group(expr, source, rows, aliases)?;
            let mut result = Value::Bool(false);
            for item in list {
                let equal = eval_binary(
                    needle.clone(),
                    &BinaryOperator::Eq,
                    eval_group(item, source, rows, aliases)?,
                )?;
                if equal == Value::Bool(true) {
                    result = equal;
                    break;
                }
                if equal.is_null() {
                    result = Value::Null;
                }
            }
            if *negated {
                eval_unary(&UnaryOperator::Not, result)
            } else {
                Ok(result)
            }
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => eval_like(
            eval_group(expr, source, rows, aliases)?,
            eval_group(pattern, source, rows, aliases)?,
            *negated,
            false,
            escape_char.as_deref(),
        ),
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => eval_like(
            eval_group(expr, source, rows, aliases)?,
            eval_group(pattern, source, rows, aliases)?,
            *negated,
            true,
            escape_char.as_deref(),
        ),
        Expr::Cast {
            expr, data_type, ..
        } => cast_value(
            eval_group(expr, source, rows, aliases)?,
            parse_data_type(&data_type.to_string())?.0,
        ),
        _ => Err(Error::Unsupported(format!("expression {expr}"))),
    }
}

fn eval_aggregate(function: &Function, source: &EvalSource<'_>, rows: &[usize]) -> Result<Value> {
    let (name, arguments, distinct) = function_parts(function)?;
    if name == "count" && arguments.is_empty() {
        return Ok(Value::Int64(rows.len() as i64));
    }
    if arguments.len() != 1 {
        return Err(Error::Constraint(format!("{name} expects one argument")));
    }
    let argument = unnamed_argument(&arguments[0])?;
    if matches!(
        argument,
        FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_)
    ) {
        return if name == "count" {
            Ok(Value::Int64(rows.len() as i64))
        } else {
            Err(Error::Constraint(format!("{name}(*) is invalid")))
        };
    }
    let FunctionArgExpr::Expr(argument) = argument else {
        unreachable!();
    };
    let mut values = Vec::with_capacity(rows.len());
    let mut seen = HashSet::new();
    for row in rows {
        let value = eval_row(argument, source, Some(*row))?;
        if value.is_null() {
            continue;
        }
        if !distinct || seen.insert(value_key(&value)) {
            values.push(value);
        }
    }
    match name.as_str() {
        "count" => Ok(Value::Int64(values.len() as i64)),
        "sum" => aggregate_sum(&values),
        "avg" => aggregate_avg(&values),
        "min" => aggregate_extreme(&values, false),
        "max" => aggregate_extreme(&values, true),
        _ => unreachable!(),
    }
}

fn aggregate_sum(values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let has_float = values
        .iter()
        .any(|value| matches!(value, Value::Float64(_)));
    if has_float {
        let mut total = 0.0;
        for value in values {
            total += match value {
                Value::Int64(value) => *value as f64,
                Value::Float64(value) => *value,
                value => {
                    return Err(Error::Type(format!(
                        "SUM does not accept {}",
                        value.type_name()
                    )));
                }
            };
        }
        Ok(Value::Float64(total))
    } else {
        let mut total = 0_i64;
        for value in values {
            let Value::Int64(value) = value else {
                return Err(Error::Type(format!(
                    "SUM does not accept {}",
                    value.type_name()
                )));
            };
            total = total
                .checked_add(*value)
                .ok_or_else(|| Error::Type("SUM Int64 overflow".into()))?;
        }
        Ok(Value::Int64(total))
    }
}

fn aggregate_avg(values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let sum = values.iter().try_fold(0.0, |sum, value| match value {
        Value::Int64(value) => Ok(sum + *value as f64),
        Value::Float64(value) => Ok(sum + value),
        value => Err(Error::Type(format!(
            "AVG does not accept {}",
            value.type_name()
        ))),
    })?;
    Ok(Value::Float64(sum / values.len() as f64))
}

fn aggregate_extreme(values: &[Value], maximum: bool) -> Result<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    let mut extreme = first;
    for value in &values[1..] {
        let ordering = value.total_cmp(extreme)?;
        if (maximum && ordering == Ordering::Greater) || (!maximum && ordering == Ordering::Less) {
            extreme = value;
        }
    }
    Ok(extreme.clone())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyValue {
    Null,
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(String),
}

fn value_key(value: &Value) -> KeyValue {
    match value {
        Value::Null => KeyValue::Null,
        Value::Int64(value) => KeyValue::Int64(*value),
        Value::Float64(value) => {
            let bits = if *value == 0.0 {
                0.0_f64.to_bits()
            } else if value.is_nan() {
                f64::NAN.to_bits()
            } else {
                value.to_bits()
            };
            KeyValue::Float64(bits)
        }
        Value::Bool(value) => KeyValue::Bool(*value),
        Value::String(value) => KeyValue::String(value.clone()),
    }
}

fn row_key(values: &[Value]) -> Vec<KeyValue> {
    values.iter().map(value_key).collect()
}

fn make_groups(
    source: &EvalSource<'_>,
    rows: Vec<usize>,
    group_by: &[Expr],
) -> Result<Vec<Vec<usize>>> {
    if group_by.is_empty() {
        return Ok(vec![rows]);
    }
    let mut indexes = HashMap::<Vec<KeyValue>, usize>::new();
    let mut groups = Vec::<Vec<usize>>::new();
    for row in rows {
        let key = group_by
            .iter()
            .map(|expr| eval_row(expr, source, Some(row)).map(|value| value_key(&value)))
            .collect::<Result<Vec<_>>>()?;
        if let Some(index) = indexes.get(&key) {
            groups[*index].push(row);
        } else {
            indexes.insert(key, groups.len());
            groups.push(vec![row]);
        }
    }
    Ok(groups)
}

fn parse_limit(expr: &Expr) -> Result<usize> {
    match eval_constant(expr)? {
        Value::Int64(value) if value >= 0 => usize::try_from(value)
            .map_err(|_| Error::Constraint(format!("LIMIT is too large: {value}"))),
        value => Err(Error::Constraint(format!(
            "LIMIT must be a non-negative integer, found {value}"
        ))),
    }
}

fn order_ordinal(expr: &Expr, column_count: usize) -> Result<Option<usize>> {
    let Expr::Value(SqlValue::Number(value, _)) = expr else {
        return Ok(None);
    };
    if value.contains(['.', 'e', 'E']) {
        return Ok(None);
    }
    let ordinal = value
        .parse::<usize>()
        .map_err(|_| Error::Constraint(format!("invalid ORDER BY ordinal {value}")))?;
    if ordinal == 0 || ordinal > column_count {
        return Err(Error::Constraint(format!(
            "ORDER BY ordinal {ordinal} is outside the projection"
        )));
    }
    Ok(Some(ordinal - 1))
}

fn validate_sort_types(rows: &[ProjectedRow]) -> Result<()> {
    let Some(first) = rows.first() else {
        return Ok(());
    };
    for index in 0..first.sort_keys.len() {
        let reference = rows
            .iter()
            .map(|row| &row.sort_keys[index])
            .find(|value| !value.is_null());
        if let Some(reference) = reference {
            for row in rows {
                if !row.sort_keys[index].is_null() {
                    reference.total_cmp(&row.sort_keys[index])?;
                }
            }
        }
    }
    Ok(())
}

fn compare_projected(
    left: &ProjectedRow,
    right: &ProjectedRow,
    order_by: &[OrderByExpr],
) -> Ordering {
    for (index, order) in order_by.iter().enumerate() {
        let ascending = order.asc.unwrap_or(true);
        let nulls_first = order.nulls_first.unwrap_or(!ascending);
        let left_value = &left.sort_keys[index];
        let right_value = &right.sort_keys[index];
        let ordering = match (left_value.is_null(), right_value.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let ordering = left_value.total_cmp(right_value).unwrap_or(Ordering::Equal);
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use proptest::{prelude::any, prop_assert_eq};

    fn query(engine: &mut Engine, sql: &str) -> QueryResult {
        match engine.execute(sql).unwrap().pop().unwrap() {
            StatementResult::Query(result) => result,
            StatementResult::Command { .. } => panic!("expected query result"),
        }
    }

    fn fixture() -> Engine {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE events (id Int64, category String, value Float64, active Bool, note Nullable(String));
                 INSERT INTO events VALUES
                    (1, 'b', 10.0, true, NULL),
                    (2, 'a', 5.5, false, 'x'),
                    (3, 'b', 2.5, true, 'y'),
                    (4, 'a', 5.5, true, NULL);",
            )
            .unwrap();
        engine
    }

    #[test]
    fn executes_fixed_analytical_shapes() {
        let mut engine = fixture();
        let result = query(
            &mut engine,
            "SELECT category AS bucket, COUNT() AS n, SUM(value) AS total,
                    MIN(value) AS low, MAX(value) AS high, AVG(value) AS mean
             FROM events WHERE active AND value >= 2
             GROUP BY bucket HAVING COUNT(*) >= 1
             ORDER BY total DESC, bucket ASC LIMIT 2",
        );
        assert_eq!(
            result.columns,
            ["bucket", "n", "total", "low", "high", "mean"]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::String("b".into()),
                    Value::Int64(2),
                    Value::Float64(12.5),
                    Value::Float64(2.5),
                    Value::Float64(10.0),
                    Value::Float64(6.25),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Int64(1),
                    Value::Float64(5.5),
                    Value::Float64(5.5),
                    Value::Float64(5.5),
                    Value::Float64(5.5),
                ],
            ]
        );
    }

    #[test]
    fn supports_projection_distinct_nulls_and_three_valued_logic() {
        let mut engine = fixture();
        let result = query(
            &mut engine,
            "SELECT DISTINCT category, id * 2 AS doubled, note IS NULL AS missing
             FROM events WHERE (active OR note = 'x') AND NOT (id < 2)
             ORDER BY category, doubled DESC",
        );
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][1], Value::Int64(8));
        assert_eq!(result.rows[2][1], Value::Int64(6));
    }

    #[test]
    fn rejects_ungrouped_columns_in_grouped_queries() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (cat String, n Int64);
                 INSERT INTO t VALUES ('a', 2), ('a', 1), ('b', 4);",
            )
            .unwrap();
        let error = engine
            .execute("SELECT cat, n, SUM(n) FROM t GROUP BY cat")
            .unwrap_err();
        assert!(matches!(error, Error::Constraint(message) if message.contains("n")));
        assert!(engine.execute("SELECT cat, n FROM t GROUP BY cat").is_err());
        assert_eq!(
            query(
                &mut engine,
                "SELECT cat, SUM(n) FROM t GROUP BY cat ORDER BY cat"
            )
            .rows,
            vec![
                vec![Value::String("a".into()), Value::Int64(3)],
                vec![Value::String("b".into()), Value::Int64(4)],
            ]
        );
    }

    #[test]
    fn rejects_unsupported_ddl_and_dml_before_mutation() {
        let mut engine = Engine::default();
        for (name, sql) in [
            (
                "primary_column",
                "CREATE TABLE primary_column (n Int64 PRIMARY KEY)",
            ),
            (
                "unique_column",
                "CREATE TABLE unique_column (n Int64 UNIQUE)",
            ),
            (
                "checked_column",
                "CREATE TABLE checked_column (n Int64 CHECK (n > 0))",
            ),
            (
                "primary_table",
                "CREATE TABLE primary_table (n Int64, PRIMARY KEY (n))",
            ),
            ("defaulted", "CREATE TABLE defaulted (n Int64 DEFAULT 1)"),
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
            assert!(engine.table(name).is_none());
        }

        engine.execute("CREATE TABLE target (n Int64)").unwrap();
        assert!(
            engine
                .execute("INSERT INTO target VALUES (1) RETURNING n")
                .is_err()
        );
        assert_eq!(
            query(&mut engine, "SELECT COUNT(*) FROM target").rows,
            vec![vec![Value::Int64(0)]]
        );
    }

    #[test]
    fn bounds_collected_batch_result_bytes_but_supports_streaming() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 1_000,
            ..EngineConfig::default()
        });
        let literal = "x".repeat(600);
        let one = format!("SELECT '{literal}' AS value");
        assert_eq!(query(&mut engine, &one).rows.len(), 1);

        let batch = format!("{one}; {one}");
        assert!(matches!(
            engine.execute(&batch),
            Err(Error::ResourceLimit {
                resource: "batch result bytes",
                ..
            })
        ));
        let mut streamed = 0;
        for result in engine.execute_iter(&batch).unwrap() {
            result.unwrap();
            streamed += 1;
        }
        assert_eq!(streamed, 2);
    }

    #[test]
    fn evaluates_aggregates_inside_scalar_functions_and_casts() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (1), (2)")
            .unwrap();
        assert_eq!(
            query(
                &mut engine,
                "SELECT COALESCE(SUM(n), 0), CAST(SUM(n) AS Float64) FROM t"
            )
            .rows,
            vec![vec![Value::Int64(3), Value::Float64(3.0)]]
        );
        assert_eq!(
            query(
                &mut engine,
                "SELECT COALESCE(SUM(n), 0) FROM t WHERE n > 100"
            )
            .rows,
            vec![vec![Value::Int64(0)]]
        );
    }

    #[test]
    fn validates_table_qualifiers_and_aliases() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        assert_eq!(
            query(&mut engine, "SELECT actual.n FROM t AS actual").columns,
            vec!["actual.n"]
        );
        assert_eq!(
            query(&mut engine, "SELECT actual.* FROM t AS actual").columns,
            vec!["n"]
        );
        assert_eq!(
            query(&mut engine, "SELECT COUNT(actual.*) FROM t AS actual").rows,
            vec![vec![Value::Int64(0)]]
        );
        for sql in [
            "SELECT bogus.n FROM t AS actual",
            "SELECT bogus.* FROM t AS actual",
            "SELECT t.n FROM t AS actual",
            "SELECT COUNT(bogus.*) FROM t AS actual",
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
        }
    }

    #[test]
    fn handles_numeric_boundaries_without_saturation_or_nan() {
        let mut engine = Engine::default();
        assert_eq!(
            query(&mut engine, "SELECT -0.0 = 0.0").rows,
            vec![vec![Value::Bool(true)]]
        );
        assert!(engine.execute("SELECT 1.0 % 0.0").is_err());
        assert!(engine.execute("SELECT 1.0 % -0.0").is_err());
        assert!(engine.execute("SELECT CAST(1e100 AS Int64)").is_err());
    }

    #[test]
    fn like_matches_unicode_and_honors_escape() {
        let mut engine = Engine::default();
        assert_eq!(
            query(
                &mut engine,
                "SELECT 'é' LIKE '_', 'a_b' LIKE 'a!_b' ESCAPE '!',
                        'a%b' LIKE 'a!%b' ESCAPE '!'"
            )
            .rows,
            vec![vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ]]
        );
    }

    #[test]
    fn scalar_string_functions_propagate_null() {
        let mut engine = Engine::default();
        assert_eq!(
            query(&mut engine, "SELECT LOWER(NULL), UPPER(NULL), LENGTH(NULL)").rows,
            vec![vec![Value::Null, Value::Null, Value::Null]]
        );
    }

    #[test]
    fn empty_aggregate_has_sql_values() {
        let mut engine = fixture();
        let result = query(
            &mut engine,
            "SELECT COUNT(*), SUM(value), MIN(value), MAX(value), AVG(value)
             FROM events WHERE id > 100",
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]]
        );
    }

    #[test]
    fn orders_nulls_and_multiple_keys_deterministically() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (group_name String, n Nullable(Int64));
                 INSERT INTO t VALUES ('b', NULL), ('a', 2), ('b', 2), ('a', NULL), ('c', 1);",
            )
            .unwrap();
        assert_eq!(
            query(
                &mut engine,
                "SELECT group_name, n FROM t ORDER BY n DESC NULLS LAST, group_name ASC",
            )
            .rows,
            vec![
                vec![Value::String("a".into()), Value::Int64(2)],
                vec![Value::String("b".into()), Value::Int64(2)],
                vec![Value::String("c".into()), Value::Int64(1)],
                vec![Value::String("a".into()), Value::Null],
                vec![Value::String("b".into()), Value::Null],
            ]
        );
    }

    #[test]
    fn enforces_input_insert_table_and_result_limits() {
        let mut engine = Engine::new(EngineConfig {
            max_input_bytes: 80,
            max_rows_per_insert: 2,
            max_rows_per_table: 2,
            max_result_rows: 1,
            max_batch_result_bytes: 10_000,
        });
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        assert!(matches!(
            engine.execute("INSERT INTO t VALUES (1), (2), (3)"),
            Err(Error::ResourceLimit {
                resource: "rows per INSERT",
                ..
            })
        ));
        engine.execute("INSERT INTO t VALUES (1), (2)").unwrap();
        assert!(matches!(
            engine.execute("INSERT INTO t VALUES (3)"),
            Err(Error::ResourceLimit {
                resource: "rows per table",
                ..
            })
        ));
        assert!(matches!(
            engine.execute("SELECT n FROM t"),
            Err(Error::ResourceLimit {
                resource: "result rows",
                ..
            })
        ));
        assert_eq!(query(&mut engine, "SELECT n FROM t LIMIT 1").rows.len(), 1);
        assert!(matches!(
            engine.execute(&" ".repeat(81)),
            Err(Error::ResourceLimit {
                resource: "SQL input bytes",
                ..
            })
        ));
    }

    #[test]
    fn rejects_partial_bad_batches_without_mutation() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        assert!(engine.execute("INSERT INTO t VALUES (1), ('bad')").is_err());
        assert_eq!(
            query(&mut engine, "SELECT COUNT(*) FROM t").rows[0][0],
            Value::Int64(0)
        );
    }

    #[test]
    fn parses_a_complete_batch_before_mutating_the_catalog() {
        let mut engine = Engine::default();
        assert!(
            engine
                .execute("CREATE TABLE should_not_exist (n Int64); SELECT (")
                .is_err()
        );
        assert!(engine.table("should_not_exist").is_none());
    }

    proptest::proptest! {
        #[test]
        fn randomized_filter_sum_matches_rust(
            values in proptest::collection::vec(-10_000_i64..10_000, 0..200),
            threshold in -10_000_i64..10_000,
        ) {
            let mut engine = Engine::default();
            engine.execute("CREATE TABLE numbers (value Int64)").unwrap();
            if !values.is_empty() {
                let sql = values.iter().map(|value| format!("({value})")).collect::<Vec<_>>().join(",");
                engine.execute(&format!("INSERT INTO numbers VALUES {sql}")).unwrap();
            }
            let result = query(
                &mut engine,
                &format!("SELECT COUNT(*), SUM(value), MIN(value), MAX(value), AVG(value) FROM numbers WHERE value >= {threshold}"),
            );
            let expected = values.iter().copied().filter(|value| *value >= threshold).collect::<Vec<_>>();
            prop_assert_eq!(result.rows[0][0].clone(), Value::Int64(expected.len() as i64));
            if expected.is_empty() {
                prop_assert_eq!(result.rows[0][1].clone(), Value::Null);
            } else {
                prop_assert_eq!(result.rows[0][1].clone(), Value::Int64(expected.iter().sum()));
                prop_assert_eq!(result.rows[0][2].clone(), Value::Int64(*expected.iter().min().unwrap()));
                prop_assert_eq!(result.rows[0][3].clone(), Value::Int64(*expected.iter().max().unwrap()));
                let expected_avg = expected.iter().map(|value| *value as f64).sum::<f64>() / expected.len() as f64;
                prop_assert_eq!(result.rows[0][4].clone(), Value::Float64(expected_avg));
            }
        }

        #[test]
        fn randomized_group_order_limit_matches_rust(
            rows in proptest::collection::vec(
                (0_u8..5, -1_000_i64..1_000, any::<bool>()),
                0..160,
            ),
            threshold in -1_000_i64..1_000,
        ) {
            let mut engine = Engine::default();
            engine
                .execute("CREATE TABLE facts (category String, value Int64, active Bool)")
                .unwrap();
            if !rows.is_empty() {
                let sql = rows
                    .iter()
                    .map(|(category, value, active)| format!("('c{category}', {value}, {active})"))
                    .collect::<Vec<_>>()
                    .join(",");
                engine
                    .execute(&format!("INSERT INTO facts VALUES {sql}"))
                    .unwrap();
            }
            let actual = query(
                &mut engine,
                &format!(
                    "SELECT category, COUNT(*) AS n, SUM(value) AS total,
                            MIN(value) AS low, MAX(value) AS high, AVG(value) AS mean
                     FROM facts WHERE active AND value >= {threshold}
                     GROUP BY category ORDER BY total DESC, category ASC LIMIT 3"
                ),
            );

            let mut grouped = BTreeMap::<String, Vec<i64>>::new();
            for (category, value, active) in &rows {
                if *active && *value >= threshold {
                    grouped
                        .entry(format!("c{category}"))
                        .or_default()
                        .push(*value);
                }
            }
            let mut expected = grouped.into_iter().collect::<Vec<_>>();
            expected.sort_by(|(left_name, left_values), (right_name, right_values)| {
                let left_sum = left_values.iter().sum::<i64>();
                let right_sum = right_values.iter().sum::<i64>();
                right_sum
                    .cmp(&left_sum)
                    .then_with(|| left_name.cmp(right_name))
            });
            expected.truncate(3);
            let expected = expected
                .into_iter()
                .map(|(category, values)| {
                    let total = values.iter().sum::<i64>();
                    vec![
                        Value::String(category),
                        Value::Int64(values.len() as i64),
                        Value::Int64(total),
                        Value::Int64(*values.iter().min().unwrap()),
                        Value::Int64(*values.iter().max().unwrap()),
                        Value::Float64(total as f64 / values.len() as f64),
                    ]
                })
                .collect::<Vec<_>>();
            prop_assert_eq!(actual.rows, expected);
        }
    }
}
