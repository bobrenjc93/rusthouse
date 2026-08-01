use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    BinaryOperator, CastKind, ColumnOption, Distinct, DuplicateTreatment, Expr, Function,
    FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, HiveDistributionStyle,
    HiveFormat, Insert, ObjectName, OrderByExpr, Query, Select, SelectItem, SetExpr, Statement,
    TableFactor, UnaryOperator, Value as SqlValue, WildcardAdditionalOptions,
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
        for statement in self.parse_statements(sql)? {
            let remaining_bytes = max_batch_result_bytes.saturating_sub(retained_bytes);
            let result = match self.execute_statement_with_budget(statement, remaining_bytes) {
                Err(Error::ResourceLimit {
                    resource, actual, ..
                }) if retained_bytes > 0
                    && matches!(resource, "result bytes" | "intermediate result bytes") =>
                {
                    return Err(Error::ResourceLimit {
                        resource: "batch result bytes",
                        limit: max_batch_result_bytes,
                        actual: retained_bytes.saturating_add(actual),
                    });
                }
                result => result?,
            };
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
        let statements = self.parse_statements(sql)?;
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

    fn parse_statements(&self, sql: &str) -> Result<Vec<Statement>> {
        if sql.len() > self.config.max_input_bytes {
            return Err(Error::ResourceLimit {
                resource: "SQL input bytes",
                limit: self.config.max_input_bytes,
                actual: sql.len(),
            });
        }
        Parser::parse_sql(&GenericDialect {}, sql).map_err(|error| Error::Sql(error.to_string()))
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&normalize_identifier(name))
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        self.execute_statement_with_budget(statement, self.config.max_batch_result_bytes)
    }

    fn execute_statement_with_budget(
        &mut self,
        statement: Statement,
        result_byte_budget: usize,
    ) -> Result<StatementResult> {
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
            Statement::Query(query) => self
                .execute_query(*query, result_byte_budget)
                .map(StatementResult::Query),
            other => Err(Error::Unsupported(other.to_string())),
        }
    }

    fn execute_query(&self, query: Query, result_byte_budget: usize) -> Result<QueryResult> {
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
        self.execute_select(
            *select,
            &query.order_by,
            query.limit.as_ref(),
            result_byte_budget,
        )
    }

    fn execute_select(
        &self,
        select: Select,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        result_byte_budget: usize,
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
                || alias.as_ref().is_some_and(|alias| {
                    alias.name.quote_style.is_some() || !alias.columns.is_empty()
                })
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
        let group_by = resolve_group_by(&group_by, &projections, &source)?;
        if group_by.iter().any(contains_aggregate) {
            return Err(Error::Constraint(
                "aggregate functions are not allowed in GROUP BY".into(),
            ));
        }
        if select.selection.as_ref().is_some_and(contains_aggregate) {
            return Err(Error::Constraint(
                "aggregate functions are not allowed in WHERE".into(),
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
            order_ordinal(&order.expr, projections.len())?;
            validate_expression_references(&order.expr, &source, &alias_expressions)?;
        }
        let empty_type_aliases = HashMap::new();
        let projection_types = projections
            .iter()
            .map(|projection| infer_expression_type(&projection.expr, &source, &empty_type_aliases))
            .collect::<Result<Vec<_>>>()?;
        let alias_types = projections
            .iter()
            .zip(&projection_types)
            .map(|(projection, data_type)| (normalize_identifier(&projection.header), *data_type))
            .collect::<HashMap<_, _>>();
        if let Some(selection) = &select.selection {
            ensure_type(
                infer_expression_type(selection, &source, &empty_type_aliases)?,
                DataType::Bool,
                "WHERE",
            )?;
        }
        for expression in &group_by {
            infer_expression_type(expression, &source, &empty_type_aliases)?;
        }
        if let Some(having) = &select.having {
            ensure_type(
                infer_expression_type(having, &source, &alias_types)?,
                DataType::Bool,
                "HAVING",
            )?;
        }
        for order in order_by {
            infer_expression_type(&order.expr, &source, &alias_types)?;
        }
        let grouped = !group_by.is_empty()
            || projections
                .iter()
                .any(|item| contains_aggregate(&item.expr))
            || select.having.is_some()
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

        let columns = projections
            .iter()
            .map(|projection| projection.header.clone())
            .collect::<Vec<_>>();
        let limit = limit.map(parse_limit).transpose()?.unwrap_or(usize::MAX);
        let ordered = !order_by.is_empty();
        let distinct = select.distinct.is_some();
        if limit == 0 {
            let result = QueryResult {
                columns,
                rows: Vec::new(),
            };
            enforce_result_byte_limit(&result, result_byte_budget)?;
            return Ok(result);
        }

        let memory = MemoryTracker::new(result_byte_budget);
        let mut filtered_rows = Vec::new();
        let mut filtered_memory = memory.reserve(std::mem::size_of::<Vec<usize>>())?;
        let source_rows = source.table.map_or(1, Table::row_count);
        let scan_limit = if !grouped && !ordered && !distinct {
            limit
        } else {
            usize::MAX
        };
        for row in 0..source_rows {
            if let Some(predicate) = &select.selection
                && eval_row_with_memory(predicate, &source, Some(row), Some(&memory))?.sql_bool()?
                    != Some(true)
            {
                continue;
            }
            filtered_memory.grow(std::mem::size_of::<usize>())?;
            filtered_rows.push(row);
            if filtered_rows.len() >= scan_limit {
                break;
            }
        }
        let (groups, group_memory) = if grouped {
            make_groups(&source, filtered_rows, &group_by, &memory)?
        } else {
            make_single_row_groups(filtered_rows, &memory)?
        };
        drop(filtered_memory);

        let mut projected_memory = memory.reserve(
            columns_retained_bytes(&columns)
                .saturating_add(std::mem::size_of::<Vec<ProjectedRow>>()),
        )?;
        let mut projected = Vec::new();
        let mut seen = HashSet::new();
        let seen_memory =
            distinct.then(|| memory.reserve(std::mem::size_of::<HashSet<Vec<KeyValue>>>()));
        let mut seen_memory = seen_memory.transpose()?;
        for rows in groups {
            if !ordered && projected.len() >= limit {
                break;
            }
            if let Some(having) = &select.having
                && eval_group(
                    having,
                    &source,
                    &rows,
                    &HashMap::new(),
                    &alias_expressions,
                    &alias_types,
                    &memory,
                )?
                .sql_bool()?
                    != Some(true)
            {
                continue;
            }
            let mut row_memory = memory.reserve(
                std::mem::size_of::<Vec<Value>>().saturating_add(
                    projections
                        .len()
                        .saturating_mul(std::mem::size_of::<Value>()),
                ),
            )?;
            let mut values = Vec::with_capacity(projections.len());
            for projection in &projections {
                let value = eval_group(
                    &projection.expr,
                    &source,
                    &rows,
                    &HashMap::new(),
                    &HashMap::new(),
                    &empty_type_aliases,
                    &memory,
                )?;
                if let Value::String(value) = &value {
                    row_memory.grow(value.len())?;
                }
                values.push(value);
            }
            if distinct {
                let key = row_key(&values);
                if seen.contains(&key) {
                    continue;
                }
                seen_memory
                    .as_mut()
                    .expect("DISTINCT reservation exists")
                    .grow(key_retained_bytes(&key).saturating_add(std::mem::size_of::<usize>()))?;
                seen.insert(key);
            }
            let source_rows = if ordered { rows } else { Vec::new() };
            drop(row_memory);
            projected_memory.grow(
                std::mem::size_of::<ProjectedRow>()
                    .saturating_add(values_retained_payload_bytes(&values))
                    .saturating_add(
                        source_rows
                            .len()
                            .saturating_mul(std::mem::size_of::<usize>()),
                    ),
            )?;
            projected.push(ProjectedRow {
                values,
                source_rows,
                sort_keys: Vec::new(),
            });
            if !ordered && projected.len() > self.config.max_result_rows {
                return Err(Error::ResourceLimit {
                    resource: "result rows",
                    limit: self.config.max_result_rows,
                    actual: projected.len(),
                });
            }
        }
        drop(group_memory);

        if ordered {
            for row in &mut projected {
                let aliases = columns
                    .iter()
                    .cloned()
                    .zip(row.values.iter().cloned())
                    .map(|(name, value)| (normalize_identifier(&name), value))
                    .collect::<HashMap<_, _>>();
                let sort_keys = order_by
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
                        eval_group(
                            &order.expr,
                            &source,
                            &row.source_rows,
                            &aliases,
                            &alias_expressions,
                            &alias_types,
                            &memory,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                projected_memory.grow(values_retained_payload_bytes(&sort_keys))?;
                row.sort_keys = sort_keys;
            }
            validate_sort_types(&projected)?;
            projected.sort_by(|left, right| compare_projected(left, right, order_by));
        }

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
        enforce_result_byte_limit(&result, result_byte_budget)?;
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

struct MemoryTracker {
    limit: usize,
    current: Cell<usize>,
}

impl MemoryTracker {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            current: Cell::new(0),
        }
    }

    fn reserve(&self, bytes: usize) -> Result<MemoryReservation<'_>> {
        let mut reservation = MemoryReservation {
            tracker: self,
            bytes: 0,
        };
        reservation.grow(bytes)?;
        Ok(reservation)
    }
}

struct MemoryReservation<'a> {
    tracker: &'a MemoryTracker,
    bytes: usize,
}

impl MemoryReservation<'_> {
    fn grow(&mut self, bytes: usize) -> Result<()> {
        let actual = self.tracker.current.get().saturating_add(bytes);
        if actual > self.tracker.limit {
            return Err(Error::ResourceLimit {
                resource: "intermediate result bytes",
                limit: self.tracker.limit,
                actual,
            });
        }
        self.tracker.current.set(actual);
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }
}

impl Drop for MemoryReservation<'_> {
    fn drop(&mut self) {
        self.tracker
            .current
            .set(self.tracker.current.get().saturating_sub(self.bytes));
    }
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
                if column.name.quote_style.is_some() {
                    return Err(Error::Unsupported("quoted identifiers".into()));
                }
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
    if insert
        .columns
        .iter()
        .any(|column| column.quote_style.is_some())
    {
        return Err(Error::Unsupported("quoted identifiers".into()));
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

fn enforce_result_byte_limit(result: &QueryResult, limit: usize) -> Result<()> {
    let actual = query_result_retained_bytes(result);
    if actual > limit {
        Err(Error::ResourceLimit {
            resource: "result bytes",
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn columns_retained_bytes(columns: &[String]) -> usize {
    columns.iter().fold(0_usize, |size, column| {
        size.saturating_add(std::mem::size_of::<String>())
            .saturating_add(column.len())
    })
}

fn row_retained_bytes(row: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>().saturating_add(values_retained_payload_bytes(row))
}

fn values_retained_payload_bytes(values: &[Value]) -> usize {
    values.iter().fold(0_usize, |size, value| {
        size.saturating_add(value_retained_payload_bytes(value))
    })
}

fn value_retained_payload_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>().saturating_add(match value {
        Value::String(value) => value.len(),
        _ => 0,
    })
}

fn object_name(name: &ObjectName) -> Result<String> {
    if name
        .0
        .iter()
        .any(|identifier| identifier.quote_style.is_some())
    {
        return Err(Error::Unsupported("quoted identifiers".into()));
    }
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
        | "int64" => DataType::Int64,
        "float" | "float32" | "float64" | "double" | "doubleprecision" | "real" => {
            DataType::Float64
        }
        "bool" | "boolean" => DataType::Bool,
        "string" | "text" | "varchar" | "char" => DataType::String,
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
                header: {
                    if alias.quote_style.is_some() {
                        return Err(Error::Unsupported("quoted identifiers".into()));
                    }
                    alias.value.clone()
                },
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

fn resolve_group_by(
    group_by: &[Expr],
    projections: &[Projection],
    source: &EvalSource<'_>,
) -> Result<Vec<Expr>> {
    group_by
        .iter()
        .map(|expr| {
            if let Some(index) = order_ordinal(expr, projections.len())? {
                return Ok(projections[index].expr.clone());
            }
            if let Expr::Identifier(identifier) = expr
                && identifier.quote_style.is_some()
            {
                return Err(Error::Unsupported("quoted identifiers".into()));
            }
            let source_has_column = if let Expr::Identifier(identifier) = expr {
                source
                    .table
                    .is_some_and(|table| table.schema().index_of(&identifier.value).is_ok())
            } else {
                false
            };
            if !source_has_column
                && let Expr::Identifier(identifier) = expr
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
            if identifier.quote_style.is_some() {
                return Err(Error::Unsupported("quoted identifiers".into()));
            }
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
            if identifiers
                .iter()
                .any(|identifier| identifier.quote_style.is_some())
            {
                return Err(Error::Unsupported("quoted identifiers".into()));
            }
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

fn infer_expression_type(
    expr: &Expr,
    source: &EvalSource<'_>,
    aliases: &HashMap<String, Option<DataType>>,
) -> Result<Option<DataType>> {
    match expr {
        Expr::Identifier(identifier) => {
            if let Some(data_type) = aliases.get(&normalize_identifier(&identifier.value)) {
                return Ok(*data_type);
            }
            let table = source
                .table
                .ok_or_else(|| Error::ColumnNotFound(identifier.value.clone()))?;
            let index = table.schema().index_of(&identifier.value)?;
            Ok(Some(table.schema().fields()[index].data_type))
        }
        Expr::CompoundIdentifier(identifiers) if identifiers.len() == 2 => {
            source.validate_qualifier(&identifiers[0].value)?;
            let table = source
                .table
                .ok_or_else(|| Error::ColumnNotFound(expr.to_string()))?;
            let index = table.schema().index_of(&identifiers[1].value)?;
            Ok(Some(table.schema().fields()[index].data_type))
        }
        Expr::CompoundIdentifier(_) => Err(Error::ColumnNotFound(expr.to_string())),
        Expr::Value(value) => Ok(sql_value(value)?.data_type()),
        Expr::Nested(expr) => infer_expression_type(expr, source, aliases),
        Expr::UnaryOp { op, expr } => {
            if minimum_int_literal(op, expr).is_some() {
                return Ok(Some(DataType::Int64));
            }
            let data_type = infer_expression_type(expr, source, aliases)?;
            match (op, data_type) {
                (_, None) => Ok(None),
                (UnaryOperator::Plus | UnaryOperator::Minus, Some(data_type))
                    if is_numeric(data_type) =>
                {
                    Ok(Some(data_type))
                }
                (UnaryOperator::Not, Some(DataType::Bool)) => Ok(Some(DataType::Bool)),
                (_, Some(data_type)) => Err(Error::Type(format!(
                    "operator {op} does not accept {data_type}"
                ))),
            }
        }
        Expr::BinaryOp { left, op, right } => {
            let left = infer_expression_type(left, source, aliases)?;
            let right = infer_expression_type(right, source, aliases)?;
            infer_binary_type(left, op, right)
        }
        Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            infer_expression_type(expr, source, aliases)?;
            Ok(Some(DataType::Bool))
        }
        Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr) => {
            ensure_type(
                infer_expression_type(expr, source, aliases)?,
                DataType::Bool,
                "truth predicate",
            )?;
            Ok(Some(DataType::Bool))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let value = infer_expression_type(expr, source, aliases)?;
            ensure_comparable(value, infer_expression_type(low, source, aliases)?)?;
            ensure_comparable(value, infer_expression_type(high, source, aliases)?)?;
            Ok(Some(DataType::Bool))
        }
        Expr::InList { expr, list, .. } => {
            let value = infer_expression_type(expr, source, aliases)?;
            for item in list {
                ensure_comparable(value, infer_expression_type(item, source, aliases)?)?;
            }
            Ok(Some(DataType::Bool))
        }
        Expr::Like {
            expr,
            pattern,
            escape_char,
            ..
        }
        | Expr::ILike {
            expr,
            pattern,
            escape_char,
            ..
        } => {
            like_escape_char(escape_char.as_deref())?;
            ensure_type(
                infer_expression_type(expr, source, aliases)?,
                DataType::String,
                "LIKE value",
            )?;
            ensure_type(
                infer_expression_type(pattern, source, aliases)?,
                DataType::String,
                "LIKE pattern",
            )?;
            Ok(Some(DataType::Bool))
        }
        Expr::Cast {
            expr,
            data_type,
            format,
            ..
        } => {
            if format.is_some() {
                return Err(Error::Unsupported("CAST FORMAT".into()));
            }
            let source_type = infer_expression_type(expr, source, aliases)?;
            let target = parse_data_type(&data_type.to_string())?.0;
            if let Some(source_type) = source_type
                && !cast_is_supported(source_type, target)
            {
                return Err(Error::Type(format!(
                    "cannot cast {source_type} to {target}"
                )));
            }
            Ok(Some(target))
        }
        Expr::Function(function) => infer_function_type(function, source, aliases),
        _ => Err(Error::Unsupported(format!("expression {expr}"))),
    }
}

fn infer_function_type(
    function: &Function,
    source: &EvalSource<'_>,
    aliases: &HashMap<String, Option<DataType>>,
) -> Result<Option<DataType>> {
    let (name, arguments, distinct) = function_parts(function)?;
    if is_aggregate_name(&name) {
        if name == "count" && arguments.is_empty() {
            if distinct {
                return Err(Error::Constraint(
                    "COUNT(DISTINCT) requires an expression".into(),
                ));
            }
            return Ok(Some(DataType::Int64));
        }
        if arguments.len() != 1 {
            return Err(Error::Constraint(format!("{name} expects one argument")));
        }
        let argument = unnamed_argument(&arguments[0])?;
        let data_type = match argument {
            FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_) => {
                if distinct {
                    return Err(Error::Constraint(
                        "COUNT(DISTINCT *) is not supported".into(),
                    ));
                }
                if name == "count" {
                    return Ok(Some(DataType::Int64));
                }
                return Err(Error::Constraint(format!("{name}(*) is invalid")));
            }
            FunctionArgExpr::Expr(expr) => {
                if references_projection_alias(expr, source, aliases) {
                    return Err(Error::Unsupported(
                        "projection aliases inside aggregate arguments".into(),
                    ));
                }
                infer_expression_type(expr, source, &HashMap::new())?
            }
        };
        return match name.as_str() {
            "count" => Ok(Some(DataType::Int64)),
            "sum" => match data_type {
                Some(data_type) if is_numeric(data_type) => Ok(Some(data_type)),
                None => Ok(None),
                Some(data_type) => Err(Error::Type(format!("SUM does not accept {data_type}"))),
            },
            "avg" => match data_type {
                Some(data_type) if is_numeric(data_type) => Ok(Some(DataType::Float64)),
                None => Ok(None),
                Some(data_type) => Err(Error::Type(format!("AVG does not accept {data_type}"))),
            },
            "min" | "max" => Ok(data_type),
            _ => unreachable!(),
        };
    }

    if distinct {
        return Err(Error::Unsupported(format!(
            "DISTINCT on scalar function {name}"
        )));
    }
    let mut types = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let FunctionArgExpr::Expr(expr) = unnamed_argument(argument)? else {
            return Err(Error::Unsupported(format!("wildcard argument to {name}")));
        };
        types.push(infer_expression_type(expr, source, aliases)?);
    }
    match (name.as_str(), types.as_slice()) {
        ("abs", [None]) => Ok(None),
        ("abs", [Some(data_type)]) if is_numeric(*data_type) => Ok(Some(*data_type)),
        ("abs", [Some(data_type)]) => Err(Error::Type(format!("ABS does not accept {data_type}"))),
        ("lower" | "upper", [data_type]) => {
            ensure_type(*data_type, DataType::String, &name)?;
            Ok(Some(DataType::String))
        }
        ("length", [data_type]) => {
            ensure_type(*data_type, DataType::String, &name)?;
            Ok(Some(DataType::Int64))
        }
        ("coalesce", []) => Err(Error::Constraint(
            "COALESCE expects at least one argument".into(),
        )),
        ("coalesce", types) => types
            .iter()
            .try_fold(None, |common, data_type| common_type(common, *data_type)),
        _ => Err(Error::Unsupported(format!("scalar function {name}"))),
    }
}

fn references_projection_alias(
    expr: &Expr,
    source: &EvalSource<'_>,
    aliases: &HashMap<String, Option<DataType>>,
) -> bool {
    match expr {
        Expr::Identifier(identifier) => {
            let name = normalize_identifier(&identifier.value);
            aliases.contains_key(&name)
                && source
                    .table
                    .is_none_or(|table| table.schema().index_of(&identifier.value).is_err())
        }
        Expr::CompoundIdentifier(_) | Expr::Value(_) => false,
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => references_projection_alias(expr, source, aliases),
        Expr::BinaryOp { left, right, .. } => {
            references_projection_alias(left, source, aliases)
                || references_projection_alias(right, source, aliases)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            references_projection_alias(expr, source, aliases)
                || references_projection_alias(low, source, aliases)
                || references_projection_alias(high, source, aliases)
        }
        Expr::InList { expr, list, .. } => {
            references_projection_alias(expr, source, aliases)
                || list
                    .iter()
                    .any(|item| references_projection_alias(item, source, aliases))
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            references_projection_alias(expr, source, aliases)
                || references_projection_alias(pattern, source, aliases)
        }
        Expr::Function(function) => match &function.args {
            FunctionArguments::List(arguments) => arguments.args.iter().any(|argument| {
                let argument = match argument {
                    FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => arg,
                };
                match argument {
                    FunctionArgExpr::Expr(expr) => {
                        references_projection_alias(expr, source, aliases)
                    }
                    _ => false,
                }
            }),
            _ => false,
        },
        _ => false,
    }
}

fn infer_binary_type(
    left: Option<DataType>,
    operator: &BinaryOperator,
    right: Option<DataType>,
) -> Result<Option<DataType>> {
    match operator {
        BinaryOperator::And | BinaryOperator::Or => {
            ensure_type(left, DataType::Bool, "boolean operand")?;
            ensure_type(right, DataType::Bool, "boolean operand")?;
            Ok(Some(DataType::Bool))
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq => {
            ensure_comparable(left, right)?;
            Ok(Some(DataType::Bool))
        }
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => {
            let common = common_type(left, right)?;
            match common {
                Some(data_type) if is_numeric(data_type) => {
                    if matches!(operator, BinaryOperator::Divide) {
                        Ok(Some(DataType::Float64))
                    } else {
                        Ok(Some(data_type))
                    }
                }
                None => Ok(None),
                Some(data_type) => Err(Error::Type(format!(
                    "numeric operator does not accept {data_type}"
                ))),
            }
        }
        BinaryOperator::StringConcat => {
            ensure_type(left, DataType::String, "concatenation operand")?;
            ensure_type(right, DataType::String, "concatenation operand")?;
            Ok(Some(DataType::String))
        }
        _ => Err(Error::Unsupported(format!("operator {operator}"))),
    }
}

fn common_type(left: Option<DataType>, right: Option<DataType>) -> Result<Option<DataType>> {
    match (left, right) {
        (None, data_type) | (data_type, None) => Ok(data_type),
        (Some(left), Some(right)) if left == right => Ok(Some(left)),
        (Some(left), Some(right)) if is_numeric(left) && is_numeric(right) => {
            Ok(Some(DataType::Float64))
        }
        (Some(left), Some(right)) => Err(Error::Type(format!(
            "incompatible types {left} and {right}"
        ))),
    }
}

fn ensure_comparable(left: Option<DataType>, right: Option<DataType>) -> Result<()> {
    common_type(left, right).map(|_| ())
}

fn ensure_type(actual: Option<DataType>, expected: DataType, context: &str) -> Result<()> {
    match actual {
        None => Ok(()),
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(Error::Type(format!(
            "{context} expects {expected}, found {actual}"
        ))),
    }
}

fn is_numeric(data_type: DataType) -> bool {
    matches!(data_type, DataType::Int64 | DataType::Float64)
}

fn cast_is_supported(source: DataType, target: DataType) -> bool {
    source == target
        || target == DataType::String
        || matches!(
            (source, target),
            (DataType::Int64, DataType::Float64)
                | (DataType::Float64, DataType::Int64)
                | (
                    DataType::String,
                    DataType::Int64 | DataType::Float64 | DataType::Bool
                )
        )
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
    canonical_expression(left) == canonical_expression(right)
}

fn canonical_expression(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(identifier) => {
            format!("column:{}", normalize_identifier(&identifier.value))
        }
        Expr::CompoundIdentifier(identifiers) if identifiers.len() == 2 => {
            format!("column:{}", normalize_identifier(&identifiers[1].value))
        }
        Expr::Value(value) => format!("value:{value:?}"),
        Expr::Nested(expr) => canonical_expression(expr),
        Expr::UnaryOp { op, expr } => {
            format!("unary:{op:?}({})", canonical_expression(expr))
        }
        Expr::BinaryOp { left, op, right } => format!(
            "binary:{op:?}({},{})",
            canonical_expression(left),
            canonical_expression(right)
        ),
        Expr::IsNull(expr) => format!("is-null({})", canonical_expression(expr)),
        Expr::IsNotNull(expr) => format!("is-not-null({})", canonical_expression(expr)),
        Expr::IsTrue(expr) => format!("is-true({})", canonical_expression(expr)),
        Expr::IsFalse(expr) => format!("is-false({})", canonical_expression(expr)),
        Expr::IsNotTrue(expr) => format!("is-not-true({})", canonical_expression(expr)),
        Expr::IsNotFalse(expr) => format!("is-not-false({})", canonical_expression(expr)),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => format!(
            "between:{negated}({},{},{})",
            canonical_expression(expr),
            canonical_expression(low),
            canonical_expression(high)
        ),
        Expr::InList {
            expr,
            list,
            negated,
        } => format!(
            "in:{negated}({};{})",
            canonical_expression(expr),
            list.iter()
                .map(canonical_expression)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => format!(
            "like:{negated}:{escape_char:?}({},{})",
            canonical_expression(expr),
            canonical_expression(pattern)
        ),
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => format!(
            "ilike:{negated}:{escape_char:?}({},{})",
            canonical_expression(expr),
            canonical_expression(pattern)
        ),
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
        } => format!(
            "cast:{kind:?}:{format:?}:{}({})",
            data_type.to_string().to_ascii_lowercase(),
            canonical_expression(expr)
        ),
        Expr::Function(function) => {
            let name = function.name.to_string().to_ascii_lowercase();
            let arguments = match &function.args {
                FunctionArguments::List(arguments) => arguments
                    .args
                    .iter()
                    .map(|argument| match argument {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                            canonical_expression(expr)
                        }
                        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => "*".into(),
                        FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_)) => "q.*".into(),
                        FunctionArg::Named { .. } => format!("{argument:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                arguments => format!("{arguments:?}"),
            };
            format!("function:{name}({arguments})")
        }
        _ => format!("{expr:?}"),
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
    eval_row_with_memory(expr, source, row, None)
}

fn eval_row_with_memory(
    expr: &Expr,
    source: &EvalSource<'_>,
    row: Option<usize>,
    memory: Option<&MemoryTracker>,
) -> Result<Value> {
    match expr {
        Expr::Value(value) => sql_value_with_memory(value, memory),
        Expr::Identifier(identifier) => lookup_column(source, row, &identifier.value, memory),
        Expr::CompoundIdentifier(identifiers) => {
            if identifiers.len() != 2 {
                return Err(Error::ColumnNotFound(expr.to_string()));
            }
            source.validate_qualifier(&identifiers[0].value)?;
            lookup_column(source, row, &identifiers[1].value, memory)
        }
        Expr::Nested(expr) => eval_row_with_memory(expr, source, row, memory),
        Expr::UnaryOp { op, expr } => {
            if let Some(value) = minimum_int_literal(op, expr) {
                return Ok(value);
            }
            eval_unary(op, eval_row_with_memory(expr, source, row, memory)?)
        }
        Expr::BinaryOp { left, op, right } => {
            let left = eval_row_with_memory(left, source, row, memory)?;
            let right = eval_row_with_memory(right, source, row, memory)?;
            eval_binary_with_memory(left, op, right, memory)
        }
        Expr::IsNull(expr) => Ok(Value::Bool(
            eval_row_with_memory(expr, source, row, memory)?.is_null(),
        )),
        Expr::IsNotNull(expr) => Ok(Value::Bool(
            !eval_row_with_memory(expr, source, row, memory)?.is_null(),
        )),
        Expr::IsTrue(expr) => Ok(Value::Bool(
            eval_row_with_memory(expr, source, row, memory)?.sql_bool()? == Some(true),
        )),
        Expr::IsFalse(expr) => Ok(Value::Bool(
            eval_row_with_memory(expr, source, row, memory)?.sql_bool()? == Some(false),
        )),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(
            eval_row_with_memory(expr, source, row, memory)?.sql_bool()? != Some(true),
        )),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(
            eval_row_with_memory(expr, source, row, memory)?.sql_bool()? != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = eval_row_with_memory(expr, source, row, memory)?;
            let lower = eval_binary_with_memory(
                value.clone(),
                &BinaryOperator::GtEq,
                eval_row_with_memory(low, source, row, memory)?,
                memory,
            )?;
            let upper = eval_binary_with_memory(
                value,
                &BinaryOperator::LtEq,
                eval_row_with_memory(high, source, row, memory)?,
                memory,
            )?;
            let result = eval_binary_with_memory(lower, &BinaryOperator::And, upper, memory)?;
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
            let needle = eval_row_with_memory(expr, source, row, memory)?;
            let mut result = Value::Bool(false);
            for item in list {
                let equal = eval_binary_with_memory(
                    needle.clone(),
                    &BinaryOperator::Eq,
                    eval_row_with_memory(item, source, row, memory)?,
                    memory,
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
            eval_row_with_memory(expr, source, row, memory)?,
            eval_row_with_memory(pattern, source, row, memory)?,
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
            eval_row_with_memory(expr, source, row, memory)?,
            eval_row_with_memory(pattern, source, row, memory)?,
            *negated,
            true,
            escape_char.as_deref(),
        ),
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
        } => eval_cast(
            kind,
            format.as_ref(),
            eval_row_with_memory(expr, source, row, memory)?,
            parse_data_type(&data_type.to_string())?.0,
        ),
        Expr::Function(function) => eval_scalar_function(function, source, row, memory),
        _ => Err(Error::Unsupported(format!("expression {expr}"))),
    }
}

fn lookup_column(
    source: &EvalSource<'_>,
    row: Option<usize>,
    name: &str,
    memory: Option<&MemoryTracker>,
) -> Result<Value> {
    let table = source
        .table
        .ok_or_else(|| Error::ColumnNotFound(name.into()))?;
    let column = table.schema().index_of(name)?;
    let row = row.ok_or_else(|| Error::ColumnNotFound(name.into()))?;
    let _value_memory = memory
        .map(|memory| memory.reserve(table.value_retained_bytes(row, column)))
        .transpose()?;
    Ok(table.value(row, column))
}

fn sql_value(value: &SqlValue) -> Result<Value> {
    sql_value_with_memory(value, None)
}

fn sql_value_with_memory(value: &SqlValue, memory: Option<&MemoryTracker>) -> Result<Value> {
    match value {
        SqlValue::Number(value, _) => {
            if value.contains(['.', 'e', 'E']) {
                let parsed = value
                    .parse::<f64>()
                    .map_err(|_| Error::Type(format!("invalid Float64 literal {value}")))?;
                finite_float(parsed, "Float64 literal")
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
        | SqlValue::RawStringLiteral(value) => {
            let _value_memory = memory
                .map(|memory| {
                    memory.reserve(std::mem::size_of::<Value>().saturating_add(value.len()))
                })
                .transpose()?;
            Ok(Value::String(value.clone()))
        }
        SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
        SqlValue::Null => Ok(Value::Null),
        _ => Err(Error::Unsupported(format!("literal {value}"))),
    }
}

fn minimum_int_literal(operator: &UnaryOperator, expr: &Expr) -> Option<Value> {
    if !matches!(operator, UnaryOperator::Minus) {
        return None;
    }
    let mut expr = expr;
    while let Expr::Nested(inner) = expr {
        expr = inner;
    }
    match expr {
        Expr::Value(SqlValue::Number(value, _)) if value == "9223372036854775808" => {
            Some(Value::Int64(i64::MIN))
        }
        _ => None,
    }
}

fn finite_float(value: f64, context: &str) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::Float64(value))
    } else {
        Err(Error::Type(format!("{context} is not finite")))
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

fn eval_binary_with_memory(
    left: Value,
    operator: &BinaryOperator,
    right: Value,
    memory: Option<&MemoryTracker>,
) -> Result<Value> {
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
            (Value::String(left), Value::String(right)) => {
                let length = left.len().saturating_add(right.len());
                let _result_memory = memory
                    .map(|memory| {
                        memory.reserve(std::mem::size_of::<Value>().saturating_add(length))
                    })
                    .transpose()?;
                let mut result = String::with_capacity(length);
                result.push_str(&left);
                result.push_str(&right);
                Ok(Value::String(result))
            }
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
            finite_float(float(left as f64, right), "Float64 arithmetic result")
        }
        (Value::Float64(left), Value::Int64(right)) => {
            finite_float(float(left, right as f64), "Float64 arithmetic result")
        }
        (Value::Float64(left), Value::Float64(right)) => {
            finite_float(float(left, right), "Float64 arithmetic result")
        }
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
    finite_float(left / right, "Float64 division result")
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
        (Value::Int64(left), Value::Float64(right)) => {
            finite_float((left as f64) % right, "Float64 modulo result")
        }
        (Value::Float64(left), Value::Int64(right)) => {
            finite_float(left % right as f64, "Float64 modulo result")
        }
        (Value::Float64(left), Value::Float64(right)) => {
            finite_float(left % right, "Float64 modulo result")
        }
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
        (Value::String(value), DataType::Float64) => {
            let parsed = value
                .parse()
                .map_err(|_| Error::Type(format!("cannot cast {value:?} to Float64")))?;
            finite_float(parsed, "Float64 cast result")
        }
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

fn eval_cast(
    kind: &CastKind,
    format: Option<&sqlparser::ast::CastFormat>,
    value: Value,
    target: DataType,
) -> Result<Value> {
    if format.is_some() {
        return Err(Error::Unsupported("CAST FORMAT".into()));
    }
    match kind {
        CastKind::TryCast | CastKind::SafeCast => {
            Ok(cast_value(value, target).unwrap_or(Value::Null))
        }
        CastKind::Cast | CastKind::DoubleColon => cast_value(value, target),
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
    let (Value::String(mut value), Value::String(pattern)) = (value, pattern) else {
        return Err(Error::Type("LIKE expects String operands".into()));
    };
    let escape = like_escape_char(escape)?;
    let mut tokens = like_tokens(&pattern, escape)?;
    if insensitive {
        value = value.to_lowercase();
        tokens = tokens
            .into_iter()
            .flat_map(|token| match token {
                LikeToken::Literal(character) => character
                    .to_lowercase()
                    .map(LikeToken::Literal)
                    .collect::<Vec<_>>(),
                token => vec![token],
            })
            .collect();
    }
    let matched = like_matches(&value, &tokens);
    Ok(Value::Bool(if negated { !matched } else { matched }))
}

fn like_escape_char(escape: Option<&str>) -> Result<Option<char>> {
    escape
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
        .transpose()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Literal(char),
    AnyOne,
    AnyMany,
}

fn like_tokens(pattern: &str, escape: Option<char>) -> Result<Vec<LikeToken>> {
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
    Ok(tokens)
}

fn like_matches(value: &str, tokens: &[LikeToken]) -> bool {
    let value = value.chars().collect::<Vec<_>>();
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
            return false;
        }
    }
    while tokens.get(pattern_index) == Some(&LikeToken::AnyMany) {
        pattern_index += 1;
    }
    pattern_index == tokens.len()
}

fn function_parts(function: &Function) -> Result<(String, &[FunctionArg], bool)> {
    let name = object_name(&function.name)?.to_ascii_lowercase();
    if function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
    {
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
    memory: Option<&MemoryTracker>,
) -> Result<Value> {
    let result_type = infer_function_type(function, source, &HashMap::new())?;
    eval_scalar_function_with(function, result_type, memory, |expr| {
        eval_row_with_memory(expr, source, row, memory)
    })
}

fn eval_scalar_function_with(
    function: &Function,
    result_type: Option<DataType>,
    memory: Option<&MemoryTracker>,
    mut evaluate: impl FnMut(&Expr) -> Result<Value>,
) -> Result<Value> {
    let (name, arguments, distinct) = function_parts(function)?;
    if distinct {
        return Err(Error::Unsupported(format!(
            "DISTINCT on scalar function {name}"
        )));
    }
    if name == "coalesce" {
        if arguments.is_empty() {
            return Err(Error::Constraint(
                "COALESCE expects at least one argument".into(),
            ));
        }
        for argument in arguments {
            let FunctionArgExpr::Expr(expr) = unnamed_argument(argument)? else {
                return Err(Error::Unsupported("wildcard argument to coalesce".into()));
            };
            let value = evaluate(expr)?;
            if !value.is_null() {
                return match result_type {
                    Some(data_type) => cast_value(value, data_type),
                    None => Ok(value),
                };
            }
        }
        return Ok(Value::Null);
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
        ("lower", [Value::String(value)]) => {
            Ok(Value::String(lowercase_with_memory(value, memory)?))
        }
        ("lower", [Value::Null]) => Ok(Value::Null),
        ("upper", [Value::String(value)]) => {
            Ok(Value::String(uppercase_with_memory(value, memory)?))
        }
        ("upper", [Value::Null]) => Ok(Value::Null),
        ("length", [Value::String(value)]) => Ok(Value::Int64(value.chars().count() as i64)),
        ("length", [Value::Null]) => Ok(Value::Null),
        _ => Err(Error::Unsupported(format!("scalar function {name}"))),
    }
}

fn lowercase_with_memory(value: &str, memory: Option<&MemoryTracker>) -> Result<String> {
    let length = value.chars().fold(0_usize, |length, character| {
        character.to_lowercase().fold(length, |length, character| {
            length.saturating_add(character.len_utf8())
        })
    });
    let _result_memory = memory
        .map(|memory| memory.reserve(std::mem::size_of::<Value>().saturating_add(length)))
        .transpose()?;
    let mut result = String::with_capacity(length);
    result.extend(value.chars().flat_map(char::to_lowercase));
    Ok(result)
}

fn uppercase_with_memory(value: &str, memory: Option<&MemoryTracker>) -> Result<String> {
    let length = value.chars().fold(0_usize, |length, character| {
        character.to_uppercase().fold(length, |length, character| {
            length.saturating_add(character.len_utf8())
        })
    });
    let _result_memory = memory
        .map(|memory| memory.reserve(std::mem::size_of::<Value>().saturating_add(length)))
        .transpose()?;
    let mut result = String::with_capacity(length);
    result.extend(value.chars().flat_map(char::to_uppercase));
    Ok(result)
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
    lazy_aliases: &HashMap<String, &Expr>,
    alias_types: &HashMap<String, Option<DataType>>,
    memory: &MemoryTracker,
) -> Result<Value> {
    if let Expr::Identifier(identifier) = expr
        && let Some(value) = aliases.get(&normalize_identifier(&identifier.value))
    {
        let _value_memory = memory.reserve(value_retained_payload_bytes(value))?;
        return Ok(value.clone());
    }
    if let Expr::Identifier(identifier) = expr
        && let Some(alias) = lazy_aliases.get(&normalize_identifier(&identifier.value))
    {
        return eval_group(
            alias,
            source,
            rows,
            &HashMap::new(),
            &HashMap::new(),
            alias_types,
            memory,
        );
    }
    match expr {
        Expr::Value(value) => sql_value_with_memory(value, Some(memory)),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            eval_row_with_memory(expr, source, rows.first().copied(), Some(memory))
        }
        Expr::Function(function) => {
            let name = object_name(&function.name)?.to_ascii_lowercase();
            if is_aggregate_name(&name) {
                eval_aggregate(function, source, rows, memory)
            } else {
                let result_type = infer_function_type(function, source, alias_types)?;
                eval_scalar_function_with(function, result_type, Some(memory), |expr| {
                    eval_group(
                        expr,
                        source,
                        rows,
                        aliases,
                        lazy_aliases,
                        alias_types,
                        memory,
                    )
                })
            }
        }
        Expr::BinaryOp { left, op, right } => eval_binary_with_memory(
            eval_group(
                left,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            op,
            eval_group(
                right,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            Some(memory),
        ),
        Expr::UnaryOp { op, expr } => {
            if let Some(value) = minimum_int_literal(op, expr) {
                return Ok(value);
            }
            eval_unary(
                op,
                eval_group(
                    expr,
                    source,
                    rows,
                    aliases,
                    lazy_aliases,
                    alias_types,
                    memory,
                )?,
            )
        }
        Expr::Nested(expr) => eval_group(
            expr,
            source,
            rows,
            aliases,
            lazy_aliases,
            alias_types,
            memory,
        ),
        Expr::IsNull(expr) => Ok(Value::Bool(
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .is_null(),
        )),
        Expr::IsNotNull(expr) => Ok(Value::Bool(
            !eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .is_null(),
        )),
        Expr::IsTrue(expr) => Ok(Value::Bool(
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .sql_bool()?
                == Some(true),
        )),
        Expr::IsFalse(expr) => Ok(Value::Bool(
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .sql_bool()?
                == Some(false),
        )),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .sql_bool()?
                != Some(true),
        )),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?
            .sql_bool()?
                != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?;
            let lower = eval_binary_with_memory(
                value.clone(),
                &BinaryOperator::GtEq,
                eval_group(
                    low,
                    source,
                    rows,
                    aliases,
                    lazy_aliases,
                    alias_types,
                    memory,
                )?,
                Some(memory),
            )?;
            let upper = eval_binary_with_memory(
                value,
                &BinaryOperator::LtEq,
                eval_group(
                    high,
                    source,
                    rows,
                    aliases,
                    lazy_aliases,
                    alias_types,
                    memory,
                )?,
                Some(memory),
            )?;
            let result = eval_binary_with_memory(lower, &BinaryOperator::And, upper, Some(memory))?;
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
            let needle = eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?;
            let mut result = Value::Bool(false);
            for item in list {
                let equal = eval_binary_with_memory(
                    needle.clone(),
                    &BinaryOperator::Eq,
                    eval_group(
                        item,
                        source,
                        rows,
                        aliases,
                        lazy_aliases,
                        alias_types,
                        memory,
                    )?,
                    Some(memory),
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
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            eval_group(
                pattern,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
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
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            eval_group(
                pattern,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            *negated,
            true,
            escape_char.as_deref(),
        ),
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
        } => eval_cast(
            kind,
            format.as_ref(),
            eval_group(
                expr,
                source,
                rows,
                aliases,
                lazy_aliases,
                alias_types,
                memory,
            )?,
            parse_data_type(&data_type.to_string())?.0,
        ),
        _ => Err(Error::Unsupported(format!("expression {expr}"))),
    }
}

fn eval_aggregate(
    function: &Function,
    source: &EvalSource<'_>,
    rows: &[usize],
    memory: &MemoryTracker,
) -> Result<Value> {
    let (name, arguments, distinct) = function_parts(function)?;
    if name == "count" && arguments.is_empty() {
        if distinct {
            return Err(Error::Constraint(
                "COUNT(DISTINCT) requires an expression".into(),
            ));
        }
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
        if distinct {
            return Err(Error::Constraint(
                "COUNT(DISTINCT *) is not supported".into(),
            ));
        }
        return if name == "count" {
            Ok(Value::Int64(rows.len() as i64))
        } else {
            Err(Error::Constraint(format!("{name}(*) is invalid")))
        };
    }
    let FunctionArgExpr::Expr(argument) = argument else {
        unreachable!();
    };
    let mut values = Vec::new();
    let mut values_memory = memory.reserve(std::mem::size_of::<Vec<Value>>())?;
    let mut seen = HashSet::new();
    let mut seen_memory = distinct
        .then(|| memory.reserve(std::mem::size_of::<HashSet<KeyValue>>()))
        .transpose()?;
    for row in rows {
        let value = eval_row_with_memory(argument, source, Some(*row), Some(memory))?;
        if value.is_null() {
            continue;
        }
        if distinct {
            let key = value_key(&value);
            if seen.contains(&key) {
                continue;
            }
            seen_memory
                .as_mut()
                .expect("DISTINCT reservation exists")
                .grow(
                    key_value_retained_bytes(&key).saturating_add(std::mem::size_of::<usize>()),
                )?;
            seen.insert(key);
        }
        values_memory.grow(value_retained_payload_bytes(&value))?;
        values.push(value);
    }
    match name.as_str() {
        "count" => Ok(Value::Int64(values.len() as i64)),
        "sum" => aggregate_sum(&values),
        "avg" => aggregate_avg(&values, memory),
        "min" => aggregate_extreme(&values, false, memory),
        "max" => aggregate_extreme(&values, true, memory),
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
        finite_float(total, "SUM result")
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

fn aggregate_avg(values: &[Value], memory: &MemoryTracker) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let _numeric_values_memory = memory.reserve(
        std::mem::size_of::<Vec<f64>>()
            .saturating_add(values.len().saturating_mul(std::mem::size_of::<f64>())),
    )?;
    let numeric_values = values
        .iter()
        .map(|value| match value {
            Value::Int64(value) => Ok(*value as f64),
            Value::Float64(value) => Ok(*value),
            value => Err(Error::Type(format!(
                "AVG does not accept {}",
                value.type_name()
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sum = 0.0;
    let mut overflowed = false;
    for value in &numeric_values {
        sum += value;
        if !sum.is_finite() {
            overflowed = true;
            break;
        }
    }
    if !overflowed {
        return finite_float(sum / numeric_values.len() as f64, "AVG result");
    }

    let mut average = 0.0;
    for (index, value) in numeric_values.iter().enumerate() {
        let count = (index + 1) as f64;
        average = average * ((count - 1.0) / count) + *value / count;
    }
    finite_float(average, "AVG result")
}

fn aggregate_extreme(values: &[Value], maximum: bool, memory: &MemoryTracker) -> Result<Value> {
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
    let _result_memory = memory.reserve(value_retained_payload_bytes(extreme))?;
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

fn key_retained_bytes(key: &[KeyValue]) -> usize {
    key.iter()
        .fold(std::mem::size_of::<Vec<KeyValue>>(), |size, value| {
            size.saturating_add(key_value_retained_bytes(value))
        })
}

fn key_value_retained_bytes(value: &KeyValue) -> usize {
    std::mem::size_of::<KeyValue>().saturating_add(match value {
        KeyValue::String(value) => value.len(),
        _ => 0,
    })
}

fn make_groups<'a>(
    source: &EvalSource<'_>,
    rows: Vec<usize>,
    group_by: &[Expr],
    memory: &'a MemoryTracker,
) -> Result<(Vec<Vec<usize>>, MemoryReservation<'a>)> {
    let mut group_memory = memory.reserve(std::mem::size_of::<Vec<Vec<usize>>>())?;
    if group_by.is_empty() {
        group_memory.grow(
            std::mem::size_of::<Vec<usize>>()
                .saturating_add(rows.len().saturating_mul(std::mem::size_of::<usize>())),
        )?;
        return Ok((vec![rows], group_memory));
    }
    let mut indexes = HashMap::<Vec<KeyValue>, usize>::new();
    let mut index_memory = memory.reserve(std::mem::size_of::<HashMap<Vec<KeyValue>, usize>>())?;
    let mut groups = Vec::<Vec<usize>>::new();
    for row in rows {
        let key = group_by
            .iter()
            .map(|expr| {
                eval_row_with_memory(expr, source, Some(row), Some(memory))
                    .map(|value| value_key(&value))
            })
            .collect::<Result<Vec<_>>>()?;
        let key_bytes = key_retained_bytes(&key);
        let key_memory = memory.reserve(key_bytes)?;
        if let Some(index) = indexes.get(&key).copied() {
            group_memory.grow(std::mem::size_of::<usize>())?;
            groups[index].push(row);
        } else {
            drop(key_memory);
            index_memory
                .grow(key_bytes.saturating_add(std::mem::size_of::<usize>().saturating_mul(2)))?;
            group_memory.grow(
                std::mem::size_of::<Vec<usize>>().saturating_add(std::mem::size_of::<usize>()),
            )?;
            indexes.insert(key, groups.len());
            groups.push(vec![row]);
        }
    }
    Ok((groups, group_memory))
}

fn make_single_row_groups(
    rows: Vec<usize>,
    memory: &MemoryTracker,
) -> Result<(Vec<Vec<usize>>, MemoryReservation<'_>)> {
    let mut group_memory = memory.reserve(std::mem::size_of::<Vec<Vec<usize>>>())?;
    let mut groups = Vec::new();
    for row in rows {
        group_memory
            .grow(std::mem::size_of::<Vec<usize>>().saturating_add(std::mem::size_of::<usize>()))?;
        groups.push(vec![row]);
    }
    Ok((groups, group_memory))
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
    fn validates_like_escape_width_at_all_cardinalities() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (s String)").unwrap();
        let invalid_queries = [
            "SELECT s LIKE 'a' ESCAPE 'xx' FROM t",
            "SELECT s ILIKE 'a' ESCAPE '' FROM t",
        ];
        for sql in invalid_queries {
            assert!(matches!(engine.execute(sql), Err(Error::Constraint(_))));
        }

        engine.execute("INSERT INTO t VALUES ('a')").unwrap();
        for sql in invalid_queries {
            assert!(matches!(engine.execute(sql), Err(Error::Constraint(_))));
        }
        assert_eq!(
            query(&mut engine, "SELECT s LIKE 'a' ESCAPE 'é' FROM t").rows,
            vec![vec![Value::Bool(true)]]
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
    fn binds_functions_and_types_even_when_tables_are_empty() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Int64, label String)")
            .unwrap();
        for sql in [
            "SELECT made_up(n) FROM t",
            "SELECT SUM(label) FROM t",
            "SELECT n + label FROM t",
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
        }
        engine.execute("INSERT INTO t VALUES (1, 'x')").unwrap();
        for sql in [
            "SELECT made_up(n) FROM t",
            "SELECT SUM(label) FROM t",
            "SELECT n + label FROM t",
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
        }
    }

    #[test]
    fn coalesce_is_lazy_and_uses_a_common_numeric_type() {
        let mut engine = Engine::default();
        assert_eq!(
            query(&mut engine, "SELECT COALESCE(1, 1 / 0)").rows,
            vec![vec![Value::Float64(1.0)]]
        );
        engine
            .execute(
                "CREATE TABLE t (n Nullable(Int64));
                 INSERT INTO t VALUES (1), (NULL);",
            )
            .unwrap();
        assert_eq!(
            query(
                &mut engine,
                "SELECT COUNT(DISTINCT COALESCE(n, 1.0)) FROM t"
            )
            .rows,
            vec![vec![Value::Int64(1)]]
        );
        assert_eq!(
            query(&mut engine, "SELECT DISTINCT COALESCE(n, 1.0) FROM t").rows,
            vec![vec![Value::Float64(1.0)]]
        );
        assert_eq!(
            query(
                &mut engine,
                "SELECT COUNT(*) FROM t GROUP BY COALESCE(n, 1.0)"
            )
            .rows,
            vec![vec![Value::Int64(2)]]
        );
    }

    #[test]
    fn group_by_expression_identifiers_are_case_insensitive() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (1), (2)")
            .unwrap();
        assert_eq!(
            query(
                &mut engine,
                "SELECT n + 1 FROM t GROUP BY N + 1 ORDER BY n + 1"
            )
            .rows,
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
        );
    }

    #[test]
    fn having_discarded_groups_do_not_consume_result_bytes() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 2_000,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        engine
            .execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)")
            .unwrap();
        assert!(
            query(&mut engine, "SELECT n FROM t GROUP BY n HAVING false")
                .rows
                .is_empty()
        );
    }

    #[test]
    fn rejects_non_finite_float_results() {
        let mut engine = Engine::default();
        for sql in [
            "SELECT 1e999",
            "SELECT CAST('NaN' AS Float64)",
            "SELECT 1e308 * 1e308",
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
        }
    }

    #[test]
    fn supports_the_minimum_int64_literal() {
        let mut engine = Engine::default();
        for sql in [
            "SELECT -9223372036854775808",
            "SELECT -(9223372036854775808)",
            "SELECT -((9223372036854775808))",
        ] {
            assert_eq!(
                query(&mut engine, sql).rows,
                vec![vec![Value::Int64(i64::MIN)]]
            );
        }
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        engine
            .execute("INSERT INTO t VALUES (-((9223372036854775808)))")
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT n FROM t").rows,
            vec![vec![Value::Int64(i64::MIN)]]
        );
    }

    #[test]
    fn ilike_escape_is_applied_before_case_folding() {
        let mut engine = Engine::default();
        assert_eq!(
            query(&mut engine, "SELECT '%' ILIKE 'A%' ESCAPE 'A'").rows,
            vec![vec![Value::Bool(true)]]
        );
    }

    #[test]
    fn having_filters_groups_before_evaluating_projections() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (g String, n Int64);
                 INSERT INTO t VALUES ('zero', 1), ('zero', -1), ('kept', 2);",
            )
            .unwrap();
        assert_eq!(
            query(
                &mut engine,
                "SELECT g, SUM(n) AS total, 1 / SUM(n) AS reciprocal
                 FROM t GROUP BY g HAVING total <> 0 ORDER BY g"
            )
            .rows,
            vec![vec![
                Value::String("kept".into()),
                Value::Int64(2),
                Value::Float64(0.5),
            ]]
        );
    }

    #[test]
    fn having_without_group_by_uses_one_implicit_group() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (1), (2)")
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT 1 AS x FROM t HAVING TRUE").rows,
            vec![vec![Value::Int64(1)]]
        );
        assert!(
            query(&mut engine, "SELECT 1 AS x FROM t HAVING FALSE")
                .rows
                .is_empty()
        );
        assert!(engine.execute("SELECT n FROM t HAVING TRUE").is_err());
        assert!(engine.execute("SELECT 1 FROM t HAVING n > 0").is_err());
    }

    #[test]
    fn group_by_source_columns_take_precedence_over_aliases() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (a Int64, b Int64);
                 INSERT INTO t VALUES (1, 10), (1, 20);",
            )
            .unwrap();
        assert!(
            engine
                .execute("SELECT a AS b, SUM(b) FROM t GROUP BY b")
                .is_err()
        );
        assert_eq!(
            query(&mut engine, "SELECT COUNT(*) AS b FROM t GROUP BY b").rows,
            vec![vec![Value::Int64(1)], vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn rejects_distinct_wildcard_counts() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (1), (1), (2)")
            .unwrap();
        assert!(engine.execute("SELECT COUNT(DISTINCT *) FROM t").is_err());
        assert!(engine.execute("SELECT COUNT(DISTINCT t.*) FROM t").is_err());
        assert_eq!(
            query(&mut engine, "SELECT COUNT(*), COUNT(DISTINCT n) FROM t").rows,
            vec![vec![Value::Int64(3), Value::Int64(2)]]
        );
    }

    #[test]
    fn rejects_aggregates_in_where_at_all_cardinalities() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        let sql = "SELECT COUNT(*) FROM t WHERE COUNT(*) > 0";
        assert!(engine.execute(sql).is_err());
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
        assert!(engine.execute(sql).is_err());
    }

    #[test]
    fn validates_order_by_ordinals_before_scanning() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        assert!(engine.execute("SELECT n FROM t ORDER BY 2").is_err());
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
        assert!(engine.execute("SELECT n FROM t ORDER BY 2").is_err());
        assert_eq!(
            query(&mut engine, "SELECT n FROM t ORDER BY 1").rows,
            vec![vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn result_byte_limit_uses_rows_after_distinct_and_limit() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 1_000,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        engine
            .execute("INSERT INTO t VALUES (1), (1), (1), (1)")
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT n FROM t LIMIT 1").rows,
            vec![vec![Value::Int64(1)]]
        );
        assert_eq!(
            query(&mut engine, "SELECT DISTINCT n FROM t").rows,
            vec![vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn bounds_intermediate_projection_materialization() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 2_048,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (payload String)").unwrap();
        let payload = "x".repeat(128);
        let values = std::iter::repeat_n(format!("('{payload}')"), 10)
            .collect::<Vec<_>>()
            .join(", ");
        engine
            .execute(&format!("INSERT INTO t VALUES {values}"))
            .unwrap();

        assert_eq!(
            query(&mut engine, "SELECT payload FROM t LIMIT 1").rows,
            vec![vec![Value::String(payload.clone())]]
        );
        assert_eq!(
            query(&mut engine, "SELECT DISTINCT payload FROM t").rows,
            vec![vec![Value::String(payload)]]
        );
        for sql in [
            "SELECT payload FROM t",
            "SELECT payload FROM t ORDER BY payload LIMIT 1",
        ] {
            assert!(matches!(
                engine.execute(sql),
                Err(Error::ResourceLimit {
                    resource: "intermediate result bytes",
                    ..
                })
            ));
        }
    }

    #[test]
    fn rejects_string_expression_amplification_before_allocation() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 1_024,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (payload String)").unwrap();
        let payload = "x".repeat(100 * 1_024);
        engine
            .execute(&format!("INSERT INTO t VALUES ('{payload}')"))
            .unwrap();
        let expression = std::iter::repeat_n("payload", 100)
            .collect::<Vec<_>>()
            .join(" || ");

        assert!(matches!(
            engine.execute(&format!("SELECT {expression} AS amplified FROM t")),
            Err(Error::ResourceLimit {
                resource: "intermediate result bytes",
                limit: 1_024,
                ..
            })
        ));

        engine
            .execute("CREATE TABLE small (payload String)")
            .unwrap();
        engine
            .execute(&format!("INSERT INTO small VALUES ('{}')", "y".repeat(64)))
            .unwrap();
        assert!(matches!(
            engine.execute(&format!("SELECT {expression} AS amplified FROM small")),
            Err(Error::ResourceLimit {
                resource: "intermediate result bytes",
                limit: 1_024,
                ..
            })
        ));
    }

    #[test]
    fn limit_zero_short_circuits_before_scanning() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 100,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        let values = (0..100)
            .map(|value| format!("({value})"))
            .collect::<Vec<_>>()
            .join(",");
        engine
            .execute(&format!("INSERT INTO t VALUES {values}"))
            .unwrap();

        let result = query(&mut engine, "SELECT n FROM t ORDER BY n LIMIT 0");
        assert_eq!(result.columns, vec!["n"]);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn enforces_memory_budget_across_analytical_intermediates() {
        let mut engine = Engine::new(EngineConfig {
            max_batch_result_bytes: 100,
            ..EngineConfig::default()
        });
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        let values = (0..10_000)
            .map(|value| format!("({value})"))
            .collect::<Vec<_>>()
            .join(",");
        engine
            .execute(&format!("INSERT INTO t VALUES {values}"))
            .unwrap();

        for sql in [
            "SELECT COUNT(n) FROM t",
            "SELECT n, COUNT(*) FROM t GROUP BY n",
            "SELECT COUNT(DISTINCT n) FROM t",
            "SELECT DISTINCT n FROM t",
        ] {
            assert!(matches!(
                engine.execute(sql),
                Err(Error::ResourceLimit {
                    resource: "intermediate result bytes",
                    limit: 100,
                    ..
                })
            ));
        }
    }

    #[test]
    fn rejects_unknown_character_type_prefixes() {
        let mut engine = Engine::default();
        assert!(
            engine
                .execute("CREATE TABLE misspelled (n CHARLATAN)")
                .is_err()
        );
        assert!(engine.table("misspelled").is_none());
        engine.execute("CREATE TABLE valid (n VARCHAR)").unwrap();
    }

    #[test]
    fn rejects_function_null_treatment_modifiers() {
        let mut engine = Engine::default();
        engine
            .execute("CREATE TABLE t (n Nullable(Int64))")
            .unwrap();
        assert!(
            engine
                .execute("SELECT SUM(n) RESPECT NULLS FROM t")
                .is_err()
        );
        assert!(
            engine
                .execute("SELECT LOWER(NULL) IGNORE NULLS FROM t")
                .is_err()
        );
    }

    #[test]
    fn implements_try_cast_and_rejects_cast_formats() {
        let mut engine = Engine::default();
        assert_eq!(
            query(&mut engine, "SELECT TRY_CAST('not-an-int' AS Int64)").rows,
            vec![vec![Value::Null]]
        );
        assert!(
            engine
                .execute("SELECT CAST('not-an-int' AS Int64)")
                .is_err()
        );
        assert!(
            engine
                .execute("SELECT CAST('1' AS Int64 FORMAT '999')")
                .is_err()
        );
    }

    #[test]
    fn avg_avoids_overflowing_its_intermediate_sum() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (n Float64);
                 INSERT INTO t VALUES (1e308), (1e308);",
            )
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT AVG(n) FROM t").rows,
            vec![vec![Value::Float64(1e308)]]
        );
    }

    #[test]
    fn rejects_quoted_identifiers_consistently() {
        let mut engine = Engine::default();
        assert!(
            engine
                .execute("CREATE TABLE quoted (\"x\" Int64, \"X\" Int64)")
                .is_err()
        );
        assert!(engine.table("quoted").is_none());

        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        for sql in [
            "SELECT \"N\" FROM t",
            "SELECT n AS \"N\" FROM t",
            "INSERT INTO t (\"n\") VALUES (1)",
            "SELECT n FROM t AS \"T\"",
        ] {
            assert!(engine.execute(sql).is_err(), "{sql} unexpectedly succeeded");
        }
    }

    #[test]
    fn truth_predicates_are_bound_as_boolean_on_empty_tables() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        assert!(engine.execute("SELECT n IS TRUE FROM t").is_err());
        assert!(engine.execute("SELECT n IS FALSE FROM t").is_err());
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
        assert!(engine.execute("SELECT n IS TRUE FROM t").is_err());
    }

    #[test]
    fn order_by_treats_signed_zero_as_equal() {
        let mut engine = Engine::default();
        engine
            .execute(
                "CREATE TABLE t (value Float64, id Int64);
                 INSERT INTO t VALUES (0.0, 1), (-0.0, 2);",
            )
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT id FROM t ORDER BY value, id").rows,
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
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

    #[test]
    fn rejects_projection_aliases_inside_aggregate_arguments_at_all_cardinalities() {
        let mut engine = Engine::default();
        engine.execute("CREATE TABLE t (n Int64)").unwrap();
        let invalid_queries = [
            "SELECT n AS x, COUNT(*) FROM t GROUP BY n HAVING SUM(x) > 0",
            "SELECT n AS x, COUNT(*) FROM t GROUP BY n HAVING SUM(COALESCE(x, 0)) > 0",
        ];

        for sql in invalid_queries {
            assert!(matches!(engine.execute(sql), Err(Error::Unsupported(_))));
        }
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
        for sql in invalid_queries {
            assert!(matches!(engine.execute(sql), Err(Error::Unsupported(_))));
        }

        assert_eq!(
            query(
                &mut engine,
                "SELECT n AS x, COUNT(*) FROM t GROUP BY n HAVING x > 0",
            )
            .rows,
            vec![vec![Value::Int64(1), Value::Int64(1)]]
        );
    }

    #[test]
    fn rejects_unsupported_unsigned_integer_declarations() {
        let mut engine = Engine::default();
        for (table, data_type) in [
            ("u8_values", "UInt8"),
            ("u16_values", "UInt16"),
            ("u32_values", "UInt32"),
            ("u64_values", "UInt64"),
        ] {
            assert!(matches!(
                engine.execute(&format!("CREATE TABLE {table} (n {data_type})")),
                Err(Error::Unsupported(_))
            ));
            assert!(engine.table(table).is_none());
        }

        engine.execute("CREATE TABLE signed (n Int64)").unwrap();
        engine
            .execute("INSERT INTO signed VALUES (-9223372036854775808), (9223372036854775807)")
            .unwrap();
        assert_eq!(
            query(&mut engine, "SELECT MIN(n), MAX(n) FROM signed").rows,
            vec![vec![Value::Int64(i64::MIN), Value::Int64(i64::MAX)]]
        );
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
