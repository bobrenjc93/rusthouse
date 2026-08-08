use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::batch::catalog::Catalog;
use crate::batch::csv::{self, CsvIngestError, CsvIngestLimits};
use crate::batch::error::{Error, Result};
use crate::batch::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, CrossJoin,
    DeleteComparisonPredicate, Having, HavingPredicate, LiteralSelect, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement, VersionSelect,
};
use crate::batch::storage::{Column, Table};
use crate::batch::tsv::{self, TsvIngestError, TsvIngestLimits};
use crate::batch::value::{DataType, Value, ValueRef};

pub use crate::batch::storage::{
    DEFAULT_MAX_CELLS_PER_TABLE, DEFAULT_MAX_COLUMNS_PER_TABLE, DEFAULT_MAX_ROWS_PER_TABLE,
    TableLimits,
};

/// Maximum estimated heap retained by the collecting [`Database::execute`] API.
pub const DEFAULT_MAX_RETAINED_RESULT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum source rows inspected by one table-backed `SELECT`.
pub const DEFAULT_MAX_QUERY_SCAN_ROWS: usize = DEFAULT_MAX_ROWS_PER_TABLE;
/// Maximum rows materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_ROWS: usize = 10_000;
/// Maximum scalar cells materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_VALUES: usize = 250_000;
/// Maximum estimated heap materialized by one `SELECT`.
pub const DEFAULT_MAX_QUERY_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum groups retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUPS: usize = 100_000;
/// Maximum grouped-key scalar cells retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUP_KEY_CELLS: usize = 500_000;
/// Maximum estimated grouped-key value-reference bytes retained by one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_GROUP_KEY_BYTES: usize = 32 * 1024 * 1024;
/// Estimated bytes charged for each scalar cell retained in a grouped key.
pub const ESTIMATED_GROUP_KEY_CELL_BYTES: usize = std::mem::size_of::<ValueRef<'static>>();
/// Maximum aggregate state cells retained while evaluating one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_AGGREGATE_STATE_CELLS: usize = 500_000;
/// Maximum estimated aggregate state heap retained by one grouped `SELECT`.
pub const DEFAULT_MAX_QUERY_AGGREGATE_STATE_BYTES: usize = 32 * 1024 * 1024;

/// Resource limits for source scans, query-result materialization, and grouped working state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryResultLimits {
    /// Maximum rows in the source table of one table-backed `SELECT` or `DELETE`.
    ///
    /// This is checked before row inspection and matching-row index allocation.
    /// `WHERE` and `LIMIT` therefore cannot reduce the charged scan. Each
    /// `UNION` operand and each `CROSS JOIN` input is checked independently.
    pub max_scan_rows: usize,
    pub max_rows: usize,
    pub max_values: usize,
    pub max_bytes: usize,
    pub max_groups: usize,
    pub max_group_key_cells: usize,
    pub max_group_key_bytes: usize,
    pub max_aggregate_state_cells: usize,
    pub max_aggregate_state_bytes: usize,
}

impl Default for QueryResultLimits {
    fn default() -> Self {
        Self {
            max_scan_rows: DEFAULT_MAX_QUERY_SCAN_ROWS,
            max_rows: DEFAULT_MAX_QUERY_RESULT_ROWS,
            max_values: DEFAULT_MAX_QUERY_RESULT_VALUES,
            max_bytes: DEFAULT_MAX_QUERY_RESULT_BYTES,
            max_groups: DEFAULT_MAX_QUERY_GROUPS,
            max_group_key_cells: DEFAULT_MAX_QUERY_GROUP_KEY_CELLS,
            max_group_key_bytes: DEFAULT_MAX_QUERY_GROUP_KEY_BYTES,
            max_aggregate_state_cells: DEFAULT_MAX_QUERY_AGGREGATE_STATE_CELLS,
            max_aggregate_state_bytes: DEFAULT_MAX_QUERY_AGGREGATE_STATE_BYTES,
        }
    }
}

/// A reusable in-memory SQL database.
///
/// Checked `Int64` column-minus-literal expressions, `CAST`, `LENGTH`, `LOWER`,
/// `UPPER`, `ABS`, `ROUND`, `FLOOR`, `CEIL`, and the minimal unpartitioned
/// `ROW_NUMBER` window forms provide bounded projections in ungrouped queries.
/// An optional `AS` alias controls each result column name.
///
/// A literal-only query returns one inferred, typed column and one row:
///
/// ```
/// use rusthouse::batch::engine::{Database, ResultColumn, StatementResult};
/// use rusthouse::batch::value::{DataType, Value};
///
/// let mut database = Database::new();
/// let results = database.execute("SELECT 'it''s ready' AS message;")?;
///
/// let [StatementResult::Query(query)] = results.as_slice() else {
///     panic!("the SELECT must produce exactly one query result");
/// };
/// assert_eq!(
///     query.columns,
///     vec![ResultColumn {
///         name: "message".to_owned(),
///         data_type: DataType::String,
///     }],
/// );
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::String("it's ready".to_owned())]],
/// );
/// # Ok::<(), rusthouse::batch::error::Error>(())
/// ```
///
/// # Examples
///
/// ```
/// use rusthouse::batch::engine::{Database, ResultColumn, StatementResult};
/// use rusthouse::batch::value::{DataType, Value};
///
/// let mut database = Database::new();
/// let results = database.execute(
///     "CREATE TABLE readings (value Int64); \
///      INSERT INTO readings VALUES (7), (-2); \
///      SELECT CAST(value AS Float64) AS value_f64 \
///      FROM readings ORDER BY value_f64;",
/// )?;
///
/// let StatementResult::Query(query) = &results[2] else {
///     panic!("the SELECT must produce a query result");
/// };
/// assert_eq!(
///     query.columns,
///     vec![ResultColumn {
///         name: "value_f64".to_owned(),
///         data_type: DataType::Float64,
///     }],
/// );
/// assert_eq!(
///     query.rows,
///     vec![vec![Value::Float64(-2.0)], vec![Value::Float64(7.0)]],
/// );
/// # Ok::<(), rusthouse::batch::error::Error>(())
/// ```
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    query_result_limits: QueryResultLimits,
    table_limits: TableLimits,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            catalog: Catalog::new(),
            query_result_limits: QueryResultLimits::default(),
            table_limits: TableLimits::default(),
        }
    }
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

    /// Creates an empty database with explicit per-query resource limits.
    pub fn with_query_result_limits(query_result_limits: QueryResultLimits) -> Self {
        Self {
            catalog: Catalog::new(),
            query_result_limits,
            table_limits: TableLimits::default(),
        }
    }

    /// Creates an empty database with an explicit row cap and default column and cell caps.
    pub fn with_max_rows_per_table(max_rows_per_table: usize) -> Self {
        Self {
            table_limits: TableLimits {
                max_rows: max_rows_per_table,
                ..TableLimits::default()
            },
            ..Self::default()
        }
    }

    /// Creates an empty database with explicit persistent limits for each table.
    pub fn with_table_limits(table_limits: TableLimits) -> Self {
        Self {
            table_limits,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    #[must_use]
    pub const fn query_result_limits(&self) -> QueryResultLimits {
        self.query_result_limits
    }

    /// Returns the maximum number of rows retained by each created table.
    #[must_use]
    pub const fn max_rows_per_table(&self) -> usize {
        self.table_limits.max_rows
    }

    /// Returns the persistent resource limits applied to each created table.
    #[must_use]
    pub const fn table_limits(&self) -> TableLimits {
        self.table_limits
    }

    /// Atomically appends a bounded, typed `CSVWithNames` input.
    ///
    /// The header must exactly match the target table's column names in schema
    /// order. Data fields are parsed using their `Int64`, finite `Float64`,
    /// `Bool`, or `String` schema types. Data fields may be double-quoted,
    /// allowing commas, LF or CRLF line endings, and doubled (`""`) quote
    /// escapes; decoded contents are parsed using the same schema-type rules.
    /// Headers must remain unquoted. Only LF and CRLF line endings are
    /// accepted.
    ///
    /// The complete input, header, every row and value, configured limits, and
    /// remaining table capacity are validated before any physical column is
    /// changed. Every error therefore leaves the target table unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::csv::CsvIngestLimits;
    /// use rusthouse::batch::engine::Database;
    ///
    /// let mut database = Database::new();
    /// database.execute(
    ///     "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);",
    /// )?;
    /// let input = b"id,score,active,label\n1,2.5,true,alpha\n";
    /// let rows = database.ingest_csv_with_names(
    ///     "metrics",
    ///     input,
    ///     CsvIngestLimits::new(input.len(), 1, 4),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_csv_with_names(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: CsvIngestLimits,
    ) -> std::result::Result<usize, CsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            csv::parse_rows(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.catalog.table_mut(table)?.insert_rows(rows)?;
        Ok(affected_rows)
    }

    /// Atomically appends bounded, typed `TabSeparatedWithNames` input.
    ///
    /// The decoded header must exactly match the target schema in order and
    /// case. Fields use the TSV writer's ClickHouse-style escapes: `\\`, `\t`,
    /// `\r`, `\n`, `\0`, `\b`, `\f`, and `\'`. Values are parsed as `Int64`,
    /// finite `Float64`, `Bool`, or `String`; records may use LF or CRLF.
    ///
    /// Parsing, all configured limits, and remaining table capacity are
    /// validated before any physical column changes.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::batch::engine::Database;
    /// use rusthouse::batch::tsv::TsvIngestLimits;
    ///
    /// let mut database = Database::new();
    /// database.execute("CREATE TABLE notes (id Int64, text String);")?;
    /// let input = b"id\ttext\n1\tline\\nwith\\ttab\n";
    /// let rows = database.ingest_tsv_with_names(
    ///     "notes",
    ///     input,
    ///     TsvIngestLimits::new(input.len(), 1, 2),
    /// )?;
    /// assert_eq!(rows, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ingest_tsv_with_names(
        &mut self,
        table: &str,
        input: impl AsRef<[u8]>,
        limits: TsvIngestLimits,
    ) -> std::result::Result<usize, TsvIngestError> {
        let rows = {
            let target = self.catalog.table(table)?;
            tsv::parse_rows(target, input.as_ref(), limits)?
        };
        let affected_rows = rows.len();
        self.catalog.table_mut(table)?.insert_rows(rows)?;
        Ok(affected_rows)
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        self.execute_with_result_limit(sql, DEFAULT_MAX_RETAINED_RESULT_BYTES)
    }

    /// Atomically executes a nonempty SQL batch containing only `INSERT` statements.
    ///
    /// Every target table, explicit-column mapping, row shape, value type,
    /// finite floating-point value, and cumulative per-table row count is
    /// validated before any row is appended. Omitted explicit columns are
    /// expanded to typed defaults during that preflight. A failure therefore
    /// leaves every table unchanged. Successful statements are committed and
    /// reported in input order.
    pub fn execute_insert_batch(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        let statements = sql::parse(sql)?;
        self.execute_insert_statements(statements)
    }

    /// Executes a batch while bounding results retained for the caller.
    pub fn execute_with_result_limit(
        &mut self,
        sql: &str,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>> {
        let statements = sql::parse(sql)?;
        self.execute_statements_with_result_limit(statements, max_result_bytes)
    }

    pub(crate) fn execute_statements_with_result_limit(
        &mut self,
        statements: Vec<Statement>,
        max_result_bytes: usize,
    ) -> Result<Vec<StatementResult>> {
        let mut results = Vec::with_capacity(statements.len());
        let mut retained_bytes = 0_usize;
        for statement in statements {
            let remaining_bytes = max_result_bytes.saturating_sub(retained_bytes);
            let tightened_result_limit = remaining_bytes < self.query_result_limits.max_bytes;
            let query_limits = QueryResultLimits {
                max_bytes: self.query_result_limits.max_bytes.min(remaining_bytes),
                ..self.query_result_limits
            };
            let result = self
                .execute_statement_with_limits(statement, query_limits)
                .map_err(|error| match error {
                    Error::ResourceLimitExceeded {
                        resource:
                            "SELECT result bytes"
                            | "SHOW TABLES result bytes"
                            | "SHOW CREATE TABLE result bytes"
                            | "DESCRIBE TABLE result bytes"
                            | "EXISTS TABLE result bytes",
                        actual,
                        ..
                    } if tightened_result_limit => Error::ResultLimitExceeded {
                        bytes: retained_bytes.saturating_add(actual),
                        max_bytes: max_result_bytes,
                    },
                    error => error,
                })?;
            retained_bytes = retained_bytes.saturating_add(result.estimated_retained_bytes());
            if retained_bytes > max_result_bytes {
                return Err(Error::ResultLimitExceeded {
                    bytes: retained_bytes,
                    max_bytes: max_result_bytes,
                });
            }
            results.push(result);
        }
        Ok(results)
    }

    pub(crate) fn execute_insert_statements(
        &mut self,
        statements: Vec<Statement>,
    ) -> Result<Vec<StatementResult>> {
        for statement in &statements {
            if !matches!(
                statement,
                Statement::Insert { .. } | Statement::InsertWithColumns { .. }
            ) {
                return Err(Error::InsertOnlyStatementRequired {
                    statement: statement_name(statement),
                });
            }
        }

        let mut incoming_rows_by_table = HashMap::<String, usize>::new();
        let mut prepared = Vec::with_capacity(statements.len());
        for statement in statements {
            let (table, columns, rows) = match statement {
                Statement::Insert { table, rows } => (table, None, rows),
                Statement::InsertWithColumns {
                    table,
                    columns,
                    rows,
                } => (table, Some(columns), rows),
                _ => unreachable!("non-INSERT statements were rejected"),
            };
            let target = self.catalog.table(&table)?;
            let cumulative_rows = incoming_rows_by_table
                .entry(table.to_ascii_lowercase())
                .or_default();
            *cumulative_rows = cumulative_rows.saturating_add(rows.len());
            let rows = target.prepare_insert_rows(columns.as_deref(), rows, *cumulative_rows)?;
            prepared.push((table, rows));
        }

        let mut results = Vec::with_capacity(prepared.len());
        for (table, rows) in prepared {
            let affected_rows = rows.len();
            self.catalog
                .table_mut(&table)
                .expect("preflight resolved every INSERT target")
                .append_prepared_insert_rows(rows);
            results.push(StatementResult::Command {
                tag: "INSERT",
                affected_rows,
            });
        }
        Ok(results)
    }

    /// Executes one already-parsed read-only query without mutable access.
    pub(crate) fn execute_query_statement_with_result_limit(
        &self,
        statement: Statement,
        max_result_bytes: usize,
    ) -> Result<QueryResult> {
        let tightened_result_limit = max_result_bytes < self.query_result_limits.max_bytes;
        let query_limits = QueryResultLimits {
            max_bytes: self.query_result_limits.max_bytes.min(max_result_bytes),
            ..self.query_result_limits
        };
        let result = self
            .execute_query_statement_with_limits(statement, query_limits)
            .map_err(|error| match error {
                Error::ResourceLimitExceeded {
                    resource:
                        "SELECT result bytes"
                        | "SHOW TABLES result bytes"
                        | "SHOW CREATE TABLE result bytes"
                        | "DESCRIBE TABLE result bytes"
                        | "EXISTS TABLE result bytes",
                    actual,
                    ..
                } if tightened_result_limit => Error::ResultLimitExceeded {
                    bytes: actual,
                    max_bytes: max_result_bytes,
                },
                error => error,
            })?;
        let retained_bytes = result.estimated_retained_bytes();
        if retained_bytes > max_result_bytes {
            return Err(Error::ResultLimitExceeded {
                bytes: retained_bytes,
                max_bytes: max_result_bytes,
            });
        }
        Ok(result)
    }

    /// Executes one already-parsed statement.
    ///
    /// Callers that stream results should parse the complete batch first, then
    /// invoke this method in order and release each result before continuing.
    pub fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        self.execute_statement_with_limits(statement, self.query_result_limits)
    }

    fn execute_statement_with_limits(
        &mut self,
        statement: Statement,
        query_result_limits: QueryResultLimits,
    ) -> Result<StatementResult> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.catalog
                    .create_table_with_limits(name, columns, self.table_limits)?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::CreateTableIfNotExists { name, columns } => {
                self.catalog.create_table_if_not_exists_with_limits(
                    name,
                    columns,
                    self.table_limits,
                )?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::DropTable { name } => {
                self.catalog.drop_table(&name)?;
                Ok(StatementResult::Command {
                    tag: "DROP TABLE",
                    affected_rows: 0,
                })
            }
            Statement::DropTableIfExists { name } => {
                self.catalog.drop_table_if_exists(&name);
                Ok(StatementResult::Command {
                    tag: "DROP TABLE",
                    affected_rows: 0,
                })
            }
            Statement::RenameTable {
                source,
                destination,
            } => {
                self.catalog.rename_table(&source, destination)?;
                Ok(StatementResult::Command {
                    tag: "RENAME TABLE",
                    affected_rows: 0,
                })
            }
            Statement::RenameColumn {
                table,
                source,
                destination,
            } => {
                self.catalog.rename_column(&table, &source, destination)?;
                Ok(StatementResult::Command {
                    tag: "ALTER TABLE",
                    affected_rows: 0,
                })
            }
            Statement::AddColumn { table, column } => {
                self.catalog.add_column(&table, column)?;
                Ok(StatementResult::Command {
                    tag: "ALTER TABLE",
                    affected_rows: 0,
                })
            }
            Statement::DropColumn { table, column } => {
                self.catalog.drop_column(&table, &column)?;
                Ok(StatementResult::Command {
                    tag: "ALTER TABLE",
                    affected_rows: 0,
                })
            }
            Statement::TruncateTable { name } => {
                let affected_rows = self.catalog.table_mut(&name)?.truncate();
                Ok(StatementResult::Command {
                    tag: "TRUNCATE TABLE",
                    affected_rows,
                })
            }
            Statement::Delete {
                table,
                column,
                literal,
            } => self.execute_delete_statement(
                table,
                comparison_predicate(column, ComparisonOperator::Equal, literal),
                query_result_limits,
            ),
            Statement::DeleteComparison {
                table,
                column,
                operator,
                literal,
            } => self.execute_delete_statement(
                table,
                comparison_predicate(column, operator, literal),
                query_result_limits,
            ),
            Statement::DeleteConjunction {
                table,
                first,
                second,
            } => self.execute_delete_statement(
                table,
                Predicate::And(
                    Box::new(delete_comparison_predicate(first)),
                    Box::new(delete_comparison_predicate(second)),
                ),
                query_result_limits,
            ),
            Statement::Insert { table, rows } => self.execute_insert_statement(table, None, rows),
            Statement::InsertWithColumns {
                table,
                columns,
                rows,
            } => self.execute_insert_statement(table, Some(columns), rows),
            statement @ (Statement::LiteralSelect(_)
            | Statement::VersionSelect(_)
            | Statement::Select(_)
            | Statement::CrossJoin(_)
            | Statement::UnionAll { .. }
            | Statement::UnionDistinct { .. }
            | Statement::ShowTables
            | Statement::ShowCreateTable { .. }
            | Statement::DescribeTable { .. }
            | Statement::ExistsTable { .. }) => self
                .execute_query_statement_with_limits(statement, query_result_limits)
                .map(StatementResult::Query),
        }
    }

    fn execute_query_statement_with_limits(
        &self,
        statement: Statement,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        match statement {
            Statement::LiteralSelect(select) => {
                self.execute_literal_select(select, query_result_limits)
            }
            Statement::VersionSelect(select) => {
                self.execute_version_select(select, query_result_limits)
            }
            Statement::Select(select) => self.execute_select(select, query_result_limits),
            Statement::CrossJoin(cross_join) => {
                self.execute_cross_join(cross_join, query_result_limits)
            }
            Statement::UnionAll { left, right } => {
                self.execute_union_all(left, right, query_result_limits)
            }
            Statement::UnionDistinct { left, right } => {
                self.execute_union_distinct(left, right, query_result_limits)
            }
            Statement::ShowTables => self.execute_show_tables(query_result_limits),
            Statement::ShowCreateTable { name } => {
                self.execute_show_create_table(&name, query_result_limits)
            }
            Statement::DescribeTable { name } => {
                self.execute_describe_table(&name, query_result_limits)
            }
            Statement::ExistsTable { name } => {
                self.execute_exists_table(&name, query_result_limits)
            }
            Statement::CreateTable { .. }
            | Statement::CreateTableIfNotExists { .. }
            | Statement::DropTable { .. }
            | Statement::DropTableIfExists { .. }
            | Statement::RenameTable { .. }
            | Statement::RenameColumn { .. }
            | Statement::AddColumn { .. }
            | Statement::DropColumn { .. }
            | Statement::TruncateTable { .. }
            | Statement::Delete { .. }
            | Statement::DeleteComparison { .. }
            | Statement::DeleteConjunction { .. }
            | Statement::Insert { .. }
            | Statement::InsertWithColumns { .. } => Err(Error::InvalidQuery(
                "read-only execution accepts only SELECT, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE"
                    .to_owned(),
            )),
        }
    }

    fn execute_insert_statement(
        &mut self,
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    ) -> Result<StatementResult> {
        let incoming_rows = rows.len();
        let rows = self.catalog.table(&table)?.prepare_insert_rows(
            columns.as_deref(),
            rows,
            incoming_rows,
        )?;
        let affected_rows = rows.len();
        self.catalog
            .table_mut(&table)?
            .append_prepared_insert_rows(rows);
        Ok(StatementResult::Command {
            tag: "INSERT",
            affected_rows,
        })
    }

    fn execute_delete_statement(
        &mut self,
        table: String,
        predicate: Predicate,
        query_result_limits: QueryResultLimits,
    ) -> Result<StatementResult> {
        let row_indexes = {
            let target = self.catalog.table(&table)?;
            let predicate = compile_predicate(target, &predicate)?;
            enforce_scan_limit(target, query_result_limits, "DELETE scanned rows")?;
            (0..target.row_count())
                .filter(|row| predicate.evaluate(target, *row))
                .collect::<Vec<_>>()
        };

        let affected_rows = self
            .catalog
            .table_mut(&table)
            .expect("DELETE target was resolved before its bounded scan")
            .delete_rows(&row_indexes)?;
        Ok(StatementResult::Command {
            tag: "DELETE",
            affected_rows,
        })
    }

    fn execute_literal_select(
        &self,
        select: LiteralSelect,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        let LiteralSelect { value, alias } = select;
        validate_literal_select_value(&value)?;
        let column_name_bytes = alias
            .as_ref()
            .map_or_else(|| literal_result_name_len(&value), String::len);
        let mut bytes = validate_result_shape_parts(
            1,
            1,
            1,
            column_name_bytes,
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        if let Value::String(value) = &value {
            bytes = bytes.saturating_add(value.len());
            enforce_resource_limit(
                SELECT_RESULT_RESOURCES.bytes,
                bytes,
                query_result_limits.max_bytes,
            )?;
        }
        let columns = vec![ResultColumn {
            name: alias.unwrap_or_else(|| literal_result_name(&value)),
            data_type: value.data_type(),
        }];

        Ok(QueryResult {
            columns,
            rows: vec![vec![value]],
        })
    }

    fn execute_version_select(
        &self,
        select: VersionSelect,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "version()";
        const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

        let column_name = select
            .alias
            .unwrap_or_else(|| RESULT_COLUMN_NAME.to_owned());
        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            column_name.len(),
            query_result_limits,
            SELECT_RESULT_RESOURCES,
        )?;
        enforce_resource_limit(
            SELECT_RESULT_RESOURCES.bytes,
            fixed_bytes.saturating_add(PACKAGE_VERSION.len()),
            query_result_limits.max_bytes,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: column_name,
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(PACKAGE_VERSION.to_owned())]],
        })
    }

    fn execute_show_tables(&self, query_result_limits: QueryResultLimits) -> Result<QueryResult> {
        let table_count = self.catalog.table_count();
        let columns = vec![ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }];
        let fixed_bytes = validate_result_shape(
            table_count,
            1,
            &columns,
            query_result_limits,
            SHOW_TABLES_RESULT_RESOURCES,
        )?;
        let table_name_bytes = self.catalog.table_name_bytes();
        let bytes = fixed_bytes.saturating_add(table_name_bytes);
        enforce_resource_limit(
            SHOW_TABLES_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let names = self.catalog.table_names();
        debug_assert_eq!(names.len(), table_count);

        Ok(QueryResult {
            columns,
            rows: names
                .into_iter()
                .map(|name| vec![Value::String(name.to_owned())])
                .collect(),
        })
    }

    fn execute_show_create_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "statement";

        let table = self.catalog.table(name)?;
        let ddl_bytes = create_table_ddl_len(table);
        let fixed_bytes = validate_result_shape_parts(
            1,
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            SHOW_CREATE_TABLE_RESULT_RESOURCES,
        )?;
        let bytes = fixed_bytes.saturating_add(ddl_bytes);
        enforce_resource_limit(
            SHOW_CREATE_TABLE_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let mut ddl = String::with_capacity(ddl_bytes);
        ddl.push_str("CREATE TABLE ");
        ddl.push_str(table.name());
        ddl.push_str(" (");
        for (index, field) in table.schema().iter().enumerate() {
            if index != 0 {
                ddl.push_str(", ");
            }
            ddl.push_str(&field.name);
            ddl.push(' ');
            ddl.push_str(field.data_type.as_str());
        }
        ddl.push(')');
        debug_assert_eq!(ddl.len(), ddl_bytes);

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(ddl)]],
        })
    }

    fn execute_describe_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_COUNT: usize = 2;
        const RESULT_COLUMN_NAME_BYTES: usize = "name".len() + "type".len();

        let table = self.catalog.table(name)?;
        let row_count = table.schema().len();
        let fixed_bytes = validate_result_shape_parts(
            row_count,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_COUNT,
            RESULT_COLUMN_NAME_BYTES,
            query_result_limits,
            DESCRIBE_TABLE_RESULT_RESOURCES,
        )?;
        let value_bytes = table
            .schema()
            .iter()
            .map(|field| {
                field
                    .name
                    .len()
                    .saturating_add(field.data_type.as_str().len())
            })
            .fold(0_usize, usize::saturating_add);
        let bytes = fixed_bytes.saturating_add(value_bytes);
        enforce_resource_limit(
            DESCRIBE_TABLE_RESULT_RESOURCES.bytes,
            bytes,
            query_result_limits.max_bytes,
        )?;

        let columns = vec![
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "type".to_owned(),
                data_type: DataType::String,
            },
        ];
        let rows = table
            .schema()
            .iter()
            .map(|field| {
                vec![
                    Value::String(field.name.clone()),
                    Value::String(field.data_type.as_str().to_owned()),
                ]
            })
            .collect();

        Ok(QueryResult { columns, rows })
    }

    fn execute_exists_table(
        &self,
        name: &str,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        const RESULT_COLUMN_NAME: &str = "result";

        validate_result_shape_parts(
            1,
            1,
            1,
            RESULT_COLUMN_NAME.len(),
            query_result_limits,
            EXISTS_TABLE_RESULT_RESOURCES,
        )?;

        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: RESULT_COLUMN_NAME.to_owned(),
                data_type: DataType::Bool,
            }],
            rows: vec![vec![Value::Bool(self.catalog.table_exists(name))]],
        })
    }

    fn execute_select(
        &self,
        select: Select,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_select_with_prefix(select, query_result_limits, None)
    }

    fn execute_select_with_prefix(
        &self,
        select: Select,
        query_result_limits: QueryResultLimits,
        result_prefix: Option<SelectResultPrefix<'_>>,
    ) -> Result<QueryResult> {
        validate_distinct_shape(&select)?;
        validate_row_number_shape(&select)?;
        validate_offset_shape(&select)?;
        let selection_limit = checked_selection_limit(select.limit, select.offset)?;
        let table = self.catalog.table(&select.table)?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(table, predicate))
            .transpose()?;
        let group_columns = if select.distinct {
            resolve_distinct_columns(table, &select.items)?
        } else {
            resolve_group_columns(table, &select.group_by)?
        };
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(table, &select.items, &group_columns)?;
        let window_ordering = resolve_row_number_ordering(table, &select.items)?;
        let having = select
            .having
            .as_ref()
            .map(|having| resolve_having(&result_columns, &items, &aggregate_specs, having))
            .transpose()?;
        let ordering = resolve_ordering(
            table,
            &items,
            &aggregate_specs,
            &result_columns,
            &select.order_by,
        )?;
        if let Some(prefix) = result_prefix {
            // Reject a UNION schema mismatch before scanning or materializing
            // any rows from its right operand.
            validate_union_schema(prefix.operation, prefix.columns, &result_columns)?;
        }

        // The source bound is deliberately independent of WHERE and LIMIT:
        // both are evaluated only after the executor has admitted the full
        // source scan. Check before allocating the matching-row index vector.
        enforce_select_scan_limit(table, query_result_limits)?;
        let mut matching_rows = (0..table.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(table, *row))
            })
            .collect::<Vec<_>>();
        if items
            .iter()
            .any(|item| matches!(item, ResolvedItem::RowNumber))
        {
            validate_row_number_count(matching_rows.len())?;
        }
        if let Some(ordering) = window_ordering {
            order_window_rows(&mut matching_rows, table, ordering, selection_limit);
        }

        let grouped = select.distinct || !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped = execute_grouped(
                table,
                &matching_rows,
                &group_columns,
                &aggregate_specs,
                query_result_limits,
            )?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            if let Some(having) = having {
                selected_groups.retain(|group| having.evaluate(&grouped, *group));
            }
            if select.distinct {
                if ordering.is_empty() {
                    if let Some(limit) = selection_limit {
                        selected_groups.truncate(limit);
                    }
                } else {
                    order_grouped_rows(
                        &mut selected_groups,
                        &grouped,
                        &items,
                        &ordering,
                        selection_limit,
                    );
                }
                apply_offset(&mut selected_groups, select.offset.unwrap_or(0));
            } else {
                order_grouped_rows(
                    &mut selected_groups,
                    &grouped,
                    &items,
                    &ordering,
                    selection_limit,
                );
            }
            validate_grouped_result_limits(
                &grouped,
                &selected_groups,
                &items,
                &result_columns,
                query_result_limits,
                result_prefix,
            )?;
            grouped.project(&selected_groups, &items)
        } else {
            order_source_rows(
                &mut matching_rows,
                table,
                &items,
                &ordering,
                selection_limit,
            );
            apply_offset(&mut matching_rows, select.offset.unwrap_or(0));
            validate_projection_result_limits(
                table,
                &matching_rows,
                &items,
                &result_columns,
                query_result_limits,
                result_prefix,
            )?;
            execute_projection(table, &matching_rows, &items)?
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }

    fn execute_union_all(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        self.execute_union_operands(left, right, query_result_limits, "UNION ALL")
    }

    fn execute_union_distinct(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        let mut result =
            self.execute_union_operands(left, right, query_result_limits, "UNION DISTINCT")?;
        deduplicate_union_rows(&mut result.rows, result.columns.len(), query_result_limits)?;
        Ok(result)
    }

    fn execute_union_operands(
        &self,
        left: Select,
        right: Select,
        query_result_limits: QueryResultLimits,
        operation: &'static str,
    ) -> Result<QueryResult> {
        let mut left_result = self.execute_select(left, query_result_limits)?;
        let mut right_result = self.execute_select_with_prefix(
            right,
            query_result_limits,
            Some(SelectResultPrefix::from_result(&left_result, operation)),
        )?;

        left_result.rows.append(&mut right_result.rows);
        Ok(left_result)
    }

    fn execute_cross_join(
        &self,
        cross_join: CrossJoin,
        query_result_limits: QueryResultLimits,
    ) -> Result<QueryResult> {
        let left = self.catalog.table(&cross_join.left_table)?;
        let right = self.catalog.table(&cross_join.right_table)?;
        enforce_select_scan_limit(left, query_result_limits)?;
        enforce_select_scan_limit(right, query_result_limits)?;
        let columns = left
            .schema()
            .iter()
            .chain(right.schema())
            .map(|field| ResultColumn {
                name: field.name.clone(),
                data_type: field.data_type,
            })
            .collect::<Vec<_>>();
        let row_count =
            limited_cartesian_row_count(left.row_count(), right.row_count(), cross_join.limit)?;
        validate_cross_join_result_limits(left, right, row_count, &columns, query_result_limits)?;

        let column_count = columns.len();
        let mut rows = Vec::with_capacity(row_count);
        'left_rows: for left_row in 0..left.row_count() {
            for right_row in 0..right.row_count() {
                if rows.len() == row_count {
                    break 'left_rows;
                }
                let mut row = Vec::with_capacity(column_count);
                row.extend(left.columns().iter().map(|column| column.value(left_row)));
                row.extend(right.columns().iter().map(|column| column.value(right_row)));
                rows.push(row);
            }
        }

        Ok(QueryResult { columns, rows })
    }
}

fn statement_name(statement: &Statement) -> &'static str {
    match statement {
        Statement::CreateTable { .. } | Statement::CreateTableIfNotExists { .. } => "CREATE TABLE",
        Statement::DropTable { .. } | Statement::DropTableIfExists { .. } => "DROP TABLE",
        Statement::RenameTable { .. } => "RENAME TABLE",
        Statement::RenameColumn { .. }
        | Statement::AddColumn { .. }
        | Statement::DropColumn { .. } => "ALTER TABLE",
        Statement::TruncateTable { .. } => "TRUNCATE TABLE",
        Statement::Delete { .. }
        | Statement::DeleteComparison { .. }
        | Statement::DeleteConjunction { .. } => "DELETE",
        Statement::Insert { .. } | Statement::InsertWithColumns { .. } => "INSERT",
        Statement::LiteralSelect(_)
        | Statement::VersionSelect(_)
        | Statement::Select(_)
        | Statement::CrossJoin(_)
        | Statement::UnionAll { .. }
        | Statement::UnionDistinct { .. } => "SELECT",
        Statement::ShowTables => "SHOW TABLES",
        Statement::ShowCreateTable { .. } => "SHOW CREATE TABLE",
        Statement::DescribeTable { .. } => "DESCRIBE TABLE",
        Statement::ExistsTable { .. } => "EXISTS TABLE",
    }
}

fn comparison_predicate(column: String, operator: ComparisonOperator, literal: Value) -> Predicate {
    Predicate::Comparison {
        left: Operand::Column(column),
        operator,
        right: Operand::Literal(literal),
    }
}

fn delete_comparison_predicate(comparison: DeleteComparisonPredicate) -> Predicate {
    comparison_predicate(comparison.column, comparison.operator, comparison.literal)
}

fn create_table_ddl_len(table: &Table) -> usize {
    let fields_bytes = table
        .schema()
        .iter()
        .map(|field| {
            field
                .name
                .len()
                .saturating_add(1)
                .saturating_add(field.data_type.as_str().len())
        })
        .fold(0_usize, usize::saturating_add);
    let delimiters = table.schema().len().saturating_sub(1).saturating_mul(2);

    "CREATE TABLE "
        .len()
        .saturating_add(table.name().len())
        .saturating_add(" (".len())
        .saturating_add(fields_bytes)
        .saturating_add(delimiters)
        .saturating_add(")".len())
}

fn literal_result_name_len(value: &Value) -> usize {
    match value {
        Value::String(value) => sql_string_literal_name_len(value),
        Value::Int64(value) => {
            let magnitude = value.unsigned_abs();
            let digits = if magnitude == 0 {
                1
            } else {
                magnitude.ilog10() as usize + 1
            };
            digits + usize::from(value.is_negative())
        }
        Value::Float64(value) => float64_result_name_len(*value),
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Null(data_type) => "CAST(NULL AS )"
            .len()
            .saturating_add(data_type.as_str().len()),
    }
}

fn sql_string_literal_name_len(value: &str) -> usize {
    value
        .len()
        .saturating_add(value.bytes().filter(|byte| *byte == b'\'').count())
        .saturating_add(2)
}

fn float64_result_name_len(value: f64) -> usize {
    #[derive(Default)]
    struct Metrics {
        bytes: usize,
        has_fraction_or_exponent: bool,
    }

    impl fmt::Write for Metrics {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.bytes = self.bytes.saturating_add(text.len());
            self.has_fraction_or_exponent |= text.contains(['.', 'e', 'E']);
            Ok(())
        }
    }

    let mut metrics = Metrics::default();
    fmt::write(&mut metrics, format_args!("{value}"))
        .expect("counting formatted Float64 bytes cannot fail");
    if value.is_finite() && !metrics.has_fraction_or_exponent {
        metrics.bytes.saturating_add(2)
    } else {
        metrics.bytes
    }
}

fn validate_literal_select_value(value: &Value) -> Result<()> {
    match value {
        Value::Float64(value) if !value.is_finite() => Err(Error::InvalidQuery(
            "literal SELECT Float64 must be finite".to_owned(),
        )),
        Value::Null(_)
        | Value::Int64(_)
        | Value::Float64(_)
        | Value::Bool(_)
        | Value::String(_) => Ok(()),
    }
}

fn literal_result_name(value: &Value) -> String {
    match value {
        Value::String(value) => {
            let mut name = String::with_capacity(sql_string_literal_name_len(value));
            name.push('\'');
            for character in value.chars() {
                name.push(character);
                if character == '\'' {
                    name.push('\'');
                }
            }
            name.push('\'');
            name
        }
        Value::Null(data_type) => format!("CAST(NULL AS {})", data_type.as_str()),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => value.as_display_string(),
    }
}

#[derive(Debug, Clone, Copy)]
struct SelectResultPrefix<'a> {
    // The final UNION result retains the left schema, not the right aliases.
    columns: &'a [ResultColumn],
    row_count: usize,
    string_bytes: usize,
    operation: &'static str,
}

impl<'a> SelectResultPrefix<'a> {
    fn from_result(result: &'a QueryResult, operation: &'static str) -> Self {
        let string_bytes = result
            .rows
            .iter()
            .flatten()
            .map(|value| match value {
                Value::String(value) => value.len(),
                Value::Null(_) | Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => 0,
            })
            .fold(0_usize, usize::saturating_add);
        Self {
            columns: &result.columns,
            row_count: result.rows.len(),
            string_bytes,
            operation,
        }
    }
}

fn validate_union_schema(
    operation: &'static str,
    left: &[ResultColumn],
    right: &[ResultColumn],
) -> Result<()> {
    if left.len() != right.len() {
        return Err(if operation == "UNION DISTINCT" {
            Error::UnionDistinctColumnCountMismatch {
                left: left.len(),
                right: right.len(),
            }
        } else {
            Error::UnionColumnCountMismatch {
                left: left.len(),
                right: right.len(),
            }
        });
    }

    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        if left.data_type != right.data_type {
            return Err(Error::TypeMismatch {
                context: format!("{operation} column {}", index + 1),
                expected: left.data_type.to_string(),
                actual: right.data_type.to_string(),
            });
        }
    }
    Ok(())
}

fn deduplicate_union_rows(
    rows: &mut Vec<Vec<Value>>,
    column_count: usize,
    limits: QueryResultLimits,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let probe_key_cells = if column_count > 2 { column_count } else { 0 };
    let first_key_cells = probe_key_cells.saturating_add(column_count);
    enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
    enforce_resource_limit(
        "SELECT group key cells",
        first_key_cells,
        limits.max_group_key_cells,
    )?;
    enforce_resource_limit(
        "SELECT group key bytes",
        first_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES),
        limits.max_group_key_bytes,
    )?;

    let retained = union_distinct_retained_rows(rows, column_count, probe_key_cells, limits)?;
    let mut row = 0;
    rows.retain(|_| {
        let keep = retained[row];
        row += 1;
        keep
    });
    // `retain` drops duplicate row contents but deliberately preserves the
    // raw UNION allocation. Rebuilding through a boxed slice makes capacity
    // equal the deduplicated length, matching retained-result accounting.
    *rows = std::mem::take(rows).into_boxed_slice().into_vec();
    debug_assert_eq!(rows.capacity(), rows.len());
    Ok(())
}

fn union_distinct_retained_rows(
    rows: &[Vec<Value>],
    column_count: usize,
    probe_key_cells: usize,
    limits: QueryResultLimits,
) -> Result<Vec<bool>> {
    let mut keys = UnionDistinctKeys::new(column_count);
    let mut probe = Vec::with_capacity(probe_key_cells);
    let mut retained = Vec::with_capacity(rows.len());
    let mut group_count = 0_usize;
    let mut group_key_cells = probe_key_cells;

    for row in rows {
        debug_assert_eq!(row.len(), column_count);
        if keys.contains(row, &mut probe) {
            retained.push(false);
            continue;
        }

        let next_group_count = group_count.saturating_add(1);
        enforce_resource_limit("SELECT groups", next_group_count, limits.max_groups)?;
        let next_key_cells = group_key_cells.saturating_add(column_count);
        enforce_resource_limit(
            "SELECT group key cells",
            next_key_cells,
            limits.max_group_key_cells,
        )?;
        enforce_resource_limit(
            "SELECT group key bytes",
            next_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES),
            limits.max_group_key_bytes,
        )?;

        keys.insert(row, &probe);
        group_count = next_group_count;
        group_key_cells = next_key_cells;
        retained.push(true);
    }

    Ok(retained)
}

#[derive(Debug)]
enum UnionDistinctKeys<'a> {
    Empty(bool),
    One(HashSet<ValueRef<'a>>),
    Multiple(HashSet<Box<[ValueRef<'a>]>>),
}

impl<'a> UnionDistinctKeys<'a> {
    fn new(column_count: usize) -> Self {
        match column_count {
            0 => Self::Empty(false),
            1 => Self::One(HashSet::new()),
            _ => Self::Multiple(HashSet::new()),
        }
    }

    fn contains(&self, row: &'a [Value], probe: &mut Vec<ValueRef<'a>>) -> bool {
        match self {
            Self::Empty(present) => *present,
            Self::One(keys) => keys.contains(&row[0].as_ref()),
            Self::Multiple(keys) if row.len() == 2 => {
                let key = [row[0].as_ref(), row[1].as_ref()];
                keys.contains(key.as_slice())
            }
            Self::Multiple(keys) => {
                probe.clear();
                probe.extend(row.iter().map(Value::as_ref));
                keys.contains(probe.as_slice())
            }
        }
    }

    fn insert(&mut self, row: &'a [Value], probe: &[ValueRef<'a>]) {
        let inserted = match self {
            Self::Empty(present) => {
                let inserted = !*present;
                *present = true;
                inserted
            }
            Self::One(keys) => keys.insert(row[0].as_ref()),
            Self::Multiple(keys) if row.len() == 2 => {
                keys.insert([row[0].as_ref(), row[1].as_ref()].into())
            }
            Self::Multiple(keys) => {
                debug_assert_eq!(probe.len(), row.len());
                keys.insert(probe.into())
            }
        };
        debug_assert!(inserted, "new UNION DISTINCT row keys must be unique");
    }
}

fn validate_distinct_shape(select: &Select) -> Result<()> {
    if !select.distinct {
        return Ok(());
    }

    let unaliased_columns = !select.items.is_empty()
        && select
            .items
            .iter()
            .all(|item| matches!(item, SelectItem::Column { alias: None, .. }));
    if !unaliased_columns || !select.group_by.is_empty() || select.having.is_some() {
        return Err(Error::InvalidQuery(
            "SELECT DISTINCT supports one or more unaliased columns, an optional WHERE predicate, optional ordering by projected physical columns, and an optional LIMIT <count> [OFFSET <offset>]".to_owned(),
        ));
    }

    Ok(())
}

fn validate_row_number_shape(select: &Select) -> Result<()> {
    let has_row_number = select
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::RowNumber { .. }));
    if !has_row_number {
        return Ok(());
    }

    if select.distinct {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () is not supported with DISTINCT".to_owned(),
        ));
    }
    if !select.group_by.is_empty() || select.having.is_some() {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () is only supported in ungrouped SELECT queries".to_owned(),
        ));
    }
    if select
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }))
    {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () cannot be combined with aggregate projections".to_owned(),
        ));
    }
    if !select.order_by.is_empty() {
        return Err(Error::InvalidQuery(
            "ROW_NUMBER() OVER () cannot be combined with ORDER BY".to_owned(),
        ));
    }
    Ok(())
}

fn validate_offset_shape(select: &Select) -> Result<()> {
    let Some(_) = select.offset else {
        return Ok(());
    };
    if select.limit.is_none() {
        return Err(Error::InvalidQuery(
            "OFFSET requires LIMIT <count>".to_owned(),
        ));
    }
    if !select.group_by.is_empty()
        || select.having.is_some()
        || select.items.iter().any(|item| {
            matches!(
                item,
                SelectItem::Aggregate { .. } | SelectItem::RowNumber { .. }
            )
        })
    {
        return Err(Error::InvalidQuery(
            "OFFSET is only supported for ungrouped or physical-column DISTINCT, non-window SELECT projections"
                .to_owned(),
        ));
    }
    Ok(())
}

fn checked_selection_limit(limit: Option<usize>, offset: Option<usize>) -> Result<Option<usize>> {
    let Some(limit) = limit else {
        debug_assert!(offset.is_none(), "OFFSET without LIMIT is rejected");
        return Ok(None);
    };
    limit
        .checked_add(offset.unwrap_or(0))
        .map(Some)
        .ok_or_else(|| Error::NumericOverflow("LIMIT + OFFSET selection bound".to_owned()))
}

fn apply_offset(rows: &mut Vec<usize>, offset: usize) {
    if offset == 0 {
        return;
    }
    if offset >= rows.len() {
        rows.clear();
        return;
    }

    let remaining = rows.len() - offset;
    rows.copy_within(offset.., 0);
    rows.truncate(remaining);
}

#[derive(Debug, Clone, Copy)]
struct ResolvedWindowOrder {
    source: usize,
    descending: bool,
}

fn resolve_row_number_ordering(
    table: &Table,
    items: &[SelectItem],
) -> Result<Option<ResolvedWindowOrder>> {
    let mut row_number_orders = items.iter().filter_map(|item| match item {
        SelectItem::RowNumber { order_by, .. } => Some(order_by.as_ref()),
        _ => None,
    });
    let Some(first) = row_number_orders.next() else {
        return Ok(None);
    };

    if row_number_orders.any(|order| !same_window_order(first, order)) {
        return Err(Error::InvalidQuery(
            "all ROW_NUMBER projections must use the same window ordering".to_owned(),
        ));
    }

    let Some(order) = first else {
        return Ok(None);
    };
    let source = table.column_index(&order.name)?;
    let actual = table.schema()[source].data_type;
    if actual != DataType::Int64 {
        return Err(Error::TypeMismatch {
            context: format!("ROW_NUMBER ORDER BY column '{}'", order.name),
            expected: DataType::Int64.to_string(),
            actual: actual.to_string(),
        });
    }

    Ok(Some(ResolvedWindowOrder {
        source,
        descending: order.descending,
    }))
}

fn same_window_order(left: Option<&OrderBy>, right: Option<&OrderBy>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.descending == right.descending && left.name.eq_ignore_ascii_case(&right.name)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn order_window_rows(
    rows: &mut Vec<usize>,
    table: &Table,
    ordering: ResolvedWindowOrder,
    limit: Option<usize>,
) {
    sort_and_limit(rows, limit, |left, right| {
        let comparison =
            int64_at(table, ordering.source, left).cmp(&int64_at(table, ordering.source, right));
        let comparison = if ordering.descending {
            comparison.reverse()
        } else {
            comparison
        };
        comparison.then_with(|| left.cmp(&right))
    });
}

fn resolve_distinct_columns(table: &Table, items: &[SelectItem]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        let SelectItem::Column { name, alias: None } = item else {
            unreachable!("the DISTINCT shape is validated")
        };
        let column = table.column_index(name)?;
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "DISTINCT column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok(columns)
}

impl QueryResult {
    fn estimated_retained_bytes(&self) -> usize {
        let mut bytes = self
            .columns
            .len()
            .saturating_mul(std::mem::size_of::<ResultColumn>())
            .saturating_add(
                self.columns
                    .iter()
                    .map(|column| column.name.len())
                    .fold(0_usize, usize::saturating_add),
            )
            .saturating_add(
                self.rows
                    .len()
                    .saturating_mul(std::mem::size_of::<Vec<Value>>()),
            );
        for row in &self.rows {
            bytes = bytes.saturating_add(row.len().saturating_mul(std::mem::size_of::<Value>()));
            for value in row {
                if let Value::String(value) = value {
                    bytes = bytes.saturating_add(value.len());
                }
            }
        }
        bytes
    }
}

impl StatementResult {
    fn estimated_retained_bytes(&self) -> usize {
        match self {
            Self::Command { .. } => 0,
            Self::Query(result) => result.estimated_retained_bytes(),
        }
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Column {
        source: usize,
        group_position: Option<usize>,
    },
    Int64Subtract {
        source: usize,
        literal: i64,
    },
    CastInt64ToFloat64 {
        source: usize,
    },
    CastFloat64ToInt64 {
        source: usize,
    },
    CastBoolToInt64 {
        source: usize,
    },
    CastInt64ToBool {
        source: usize,
    },
    CastFloat64ToBool {
        source: usize,
    },
    CastInt64ToString {
        source: usize,
    },
    CastBoolToString {
        source: usize,
    },
    StringLength {
        source: usize,
    },
    StringLower {
        source: usize,
    },
    StringUpper {
        source: usize,
    },
    Int64Abs {
        source: usize,
    },
    Float64Round {
        source: usize,
    },
    Float64Floor {
        source: usize,
    },
    Float64Ceil {
        source: usize,
    },
    RowNumber,
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

#[derive(Debug, Clone)]
struct ResolvedHaving {
    state: usize,
    predicate: ResolvedHavingPredicate,
}

#[derive(Debug, Clone)]
enum ResolvedHavingPredicate {
    Comparison {
        operator: ComparisonOperator,
        value: Value,
    },
    IsNull,
    IsNotNull,
}

impl ResolvedHaving {
    fn evaluate(&self, data: &GroupedData<'_>, group: usize) -> bool {
        let aggregate = &data.aggregates[self.state][group];
        match &self.predicate {
            ResolvedHavingPredicate::Comparison { operator, value } => {
                let Some(comparison) = aggregate.as_ref().sql_cmp(value.as_ref()) else {
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
            ResolvedHavingPredicate::IsNull => matches!(aggregate, Value::Null(_)),
            ResolvedHavingPredicate::IsNotNull => !matches!(aggregate, Value::Null(_)),
        }
    }
}

fn resolve_group_columns(table: &Table, names: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = table.column_index(name)?;
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
    table: &Table,
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
                for (source, field) in table.schema().iter().enumerate() {
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
                        name: field.name.clone(),
                        data_type: field.data_type,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = table.column_index(name)?;
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
                        .unwrap_or_else(|| table.schema()[source].name.clone()),
                    data_type: table.schema()[source].data_type,
                });
            }
            SelectItem::Int64Subtract {
                name,
                literal,
                alias,
            } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Int64 {
                    return Err(Error::TypeMismatch {
                        context: format!("Int64 subtraction argument '{name}'"),
                        expected: DataType::Int64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "Int64 subtraction projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Int64Subtract {
                    source,
                    literal: *literal,
                });
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| {
                        sql::int64_subtraction_name(&table.schema()[source].name, *literal)
                    }),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Cast {
                name,
                target_type,
                alias,
            } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                let resolved = match (actual, *target_type) {
                    (DataType::Int64, DataType::Float64) => {
                        Some(ResolvedItem::CastInt64ToFloat64 { source })
                    }
                    (DataType::Float64, DataType::Int64) => {
                        Some(ResolvedItem::CastFloat64ToInt64 { source })
                    }
                    (DataType::Bool, DataType::Int64) => {
                        Some(ResolvedItem::CastBoolToInt64 { source })
                    }
                    (DataType::Int64, DataType::Bool) => {
                        Some(ResolvedItem::CastInt64ToBool { source })
                    }
                    (DataType::Float64, DataType::Bool) => {
                        Some(ResolvedItem::CastFloat64ToBool { source })
                    }
                    (DataType::Int64, DataType::String) => {
                        Some(ResolvedItem::CastInt64ToString { source })
                    }
                    (DataType::Bool, DataType::String) => {
                        Some(ResolvedItem::CastBoolToString { source })
                    }
                    _ => None,
                };
                let Some(resolved) = resolved else {
                    let expected = match target_type {
                        DataType::Float64 => "Int64",
                        DataType::Bool => "Int64 or Float64",
                        DataType::Int64 => "Float64 or Bool",
                        DataType::String => "Int64 or Bool",
                    };
                    return Err(Error::TypeMismatch {
                        context: format!("CAST argument '{name}'"),
                        expected: expected.to_owned(),
                        actual: actual.to_string(),
                    });
                };
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "CAST projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(resolved);
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| {
                        format!("CAST({} AS {target_type})", table.schema()[source].name)
                    }),
                    data_type: *target_type,
                });
            }
            SelectItem::Length { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("LENGTH argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "LENGTH projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringLength { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("LENGTH({})", table.schema()[source].name)),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Lower { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("LOWER argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "LOWER projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringLower { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("LOWER({})", table.schema()[source].name)),
                    data_type: DataType::String,
                });
            }
            SelectItem::Upper { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::String {
                    return Err(Error::TypeMismatch {
                        context: format!("UPPER argument '{name}'"),
                        expected: DataType::String.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "UPPER projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::StringUpper { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("UPPER({})", table.schema()[source].name)),
                    data_type: DataType::String,
                });
            }
            SelectItem::Abs { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Int64 {
                    return Err(Error::TypeMismatch {
                        context: format!("ABS argument '{name}'"),
                        expected: DataType::Int64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "ABS projections are only supported in ungrouped SELECT queries".to_owned(),
                    ));
                }
                items.push(ResolvedItem::Int64Abs { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("ABS({})", table.schema()[source].name)),
                    data_type: DataType::Int64,
                });
            }
            SelectItem::Round { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("ROUND argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "ROUND projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Round { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("ROUND({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::Floor { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("FLOOR argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "FLOOR projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Floor { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("FLOOR({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::Ceil { name, alias } => {
                let source = table.column_index(name)?;
                let actual = table.schema()[source].data_type;
                if actual != DataType::Float64 {
                    return Err(Error::TypeMismatch {
                        context: format!("CEIL argument '{name}'"),
                        expected: DataType::Float64.to_string(),
                        actual: actual.to_string(),
                    });
                }
                if has_aggregate || !group_columns.is_empty() {
                    return Err(Error::InvalidQuery(
                        "CEIL projections are only supported in ungrouped SELECT queries"
                            .to_owned(),
                    ));
                }
                items.push(ResolvedItem::Float64Ceil { source });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("CEIL({})", table.schema()[source].name)),
                    data_type: DataType::Float64,
                });
            }
            SelectItem::RowNumber { alias, .. } => {
                items.push(ResolvedItem::RowNumber);
                result_columns.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| "ROW_NUMBER()".to_owned()),
                    data_type: DataType::Int64,
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
                        let index = table.column_index(name)?;
                        (
                            Some(index),
                            Some(table.schema()[index].data_type),
                            table.schema()[index].name.clone(),
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

fn resolve_having(
    columns: &[ResultColumn],
    items: &[ResolvedItem],
    aggregate_specs: &[AggregateSpec],
    requested: &Having,
) -> Result<ResolvedHaving> {
    let matches = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.name.eq_ignore_ascii_case(&requested.alias))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let output = match matches.as_slice() {
        [output] => *output,
        [] => {
            return Err(Error::InvalidQuery(format!(
                "HAVING alias '{}' is not in the SELECT output",
                requested.alias
            )));
        }
        _ => {
            return Err(Error::InvalidQuery(format!(
                "HAVING alias '{}' is ambiguous",
                requested.alias
            )));
        }
    };

    let aggregate_requirement = match &requested.predicate {
        HavingPredicate::Comparison { .. } => "a projected numeric aggregate",
        HavingPredicate::IsNull | HavingPredicate::IsNotNull => "a projected aggregate",
    };
    let ResolvedItem::Aggregate { state } = items[output] else {
        return Err(Error::InvalidQuery(format!(
            "HAVING alias '{}' must reference {aggregate_requirement}",
            requested.alias,
        )));
    };
    let spec = &aggregate_specs[state];
    let predicate = match &requested.predicate {
        HavingPredicate::Comparison { operator, value } => {
            let supported = matches!(
                aggregate_output_type(spec.function, spec.input_type),
                DataType::Int64 | DataType::Float64
            );
            if !supported {
                return Err(Error::InvalidQuery(format!(
                    "HAVING alias '{}' must reference a projected numeric aggregate",
                    requested.alias
                )));
            }
            match value {
                Value::Int64(_) => {}
                Value::Float64(value) if value.is_finite() => {}
                Value::Float64(_) => {
                    return Err(Error::InvalidQuery(
                        "HAVING comparison Float64 thresholds must be finite".to_owned(),
                    ));
                }
                Value::Null(_) => {
                    return Err(Error::InvalidQuery(
                        "HAVING comparisons do not support NULL thresholds".to_owned(),
                    ));
                }
                value => {
                    return Err(Error::TypeMismatch {
                        context: "HAVING comparison threshold".to_owned(),
                        expected: "Int64 or Float64".to_owned(),
                        actual: value.data_type().to_string(),
                    });
                }
            }
            ResolvedHavingPredicate::Comparison {
                operator: *operator,
                value: value.clone(),
            }
        }
        HavingPredicate::IsNull => ResolvedHavingPredicate::IsNull,
        HavingPredicate::IsNotNull => ResolvedHavingPredicate::IsNotNull,
    };

    Ok(ResolvedHaving { state, predicate })
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
    table: &Table,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Result<Vec<Vec<Value>>> {
    matching_rows
        .iter()
        .enumerate()
        .map(|(row_number, row)| {
            items
                .iter()
                .map(|item| {
                    Ok(match item {
                        ResolvedItem::Column { source, .. } => table.columns()[*source].value(*row),
                        ResolvedItem::Int64Subtract { source, literal } => Value::Int64(
                            checked_int64_subtract(int64_at(table, *source, *row), *literal)?,
                        ),
                        ResolvedItem::CastInt64ToFloat64 { source } => {
                            Value::Float64(int64_at(table, *source, *row) as f64)
                        }
                        ResolvedItem::CastFloat64ToInt64 { source } => Value::Int64(
                            checked_float64_to_int64(float64_at(table, *source, *row))?,
                        ),
                        ResolvedItem::CastBoolToInt64 { source } => {
                            Value::Int64(if bool_at(table, *source, *row) { 1 } else { 0 })
                        }
                        ResolvedItem::CastInt64ToBool { source } => {
                            Value::Bool(int64_at(table, *source, *row) != 0)
                        }
                        ResolvedItem::CastFloat64ToBool { source } => {
                            Value::Bool(float64_at(table, *source, *row) != 0.0)
                        }
                        ResolvedItem::CastInt64ToString { source } => {
                            Value::String(int64_at(table, *source, *row).to_string())
                        }
                        ResolvedItem::CastBoolToString { source } => {
                            Value::String(bool_string(bool_at(table, *source, *row)).to_owned())
                        }
                        ResolvedItem::StringLength { source } => Value::Int64(
                            string_length_to_i64(string_at(table, *source, *row).len())?,
                        ),
                        ResolvedItem::StringLower { source } => {
                            Value::String(string_at(table, *source, *row).to_ascii_lowercase())
                        }
                        ResolvedItem::StringUpper { source } => {
                            Value::String(string_at(table, *source, *row).to_ascii_uppercase())
                        }
                        ResolvedItem::Int64Abs { source } => {
                            Value::Int64(checked_int64_abs(int64_at(table, *source, *row))?)
                        }
                        ResolvedItem::Float64Round { source } => {
                            Value::Float64(float64_at(table, *source, *row).round())
                        }
                        ResolvedItem::Float64Floor { source } => {
                            Value::Float64(float64_at(table, *source, *row).floor())
                        }
                        ResolvedItem::Float64Ceil { source } => {
                            Value::Float64(float64_at(table, *source, *row).ceil())
                        }
                        ResolvedItem::RowNumber => Value::Int64(checked_row_number(row_number)?),
                        ResolvedItem::Aggregate { .. } => {
                            unreachable!("projection does not contain aggregates")
                        }
                    })
                })
                .collect()
        })
        .collect()
}

fn validate_row_number_count(row_count: usize) -> Result<()> {
    if row_count > 0 {
        checked_row_number(row_count - 1)?;
    }
    Ok(())
}

fn checked_row_number(zero_based: usize) -> Result<i64> {
    i64::try_from(zero_based)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::NumericOverflow("ROW_NUMBER result".to_owned()))
}

fn limited_cartesian_row_count(
    left_rows: usize,
    right_rows: usize,
    limit: Option<usize>,
) -> Result<usize> {
    match limit {
        Some(limit) => Ok(left_rows
            .checked_mul(right_rows)
            .map_or(limit, |rows| rows.min(limit))),
        None => left_rows
            .checked_mul(right_rows)
            .ok_or_else(|| Error::NumericOverflow("CROSS JOIN row count".to_owned())),
    }
}

fn validate_cross_join_result_limits(
    left: &Table,
    right: &Table,
    row_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
) -> Result<()> {
    let mut bytes = validate_result_shape(
        row_count,
        columns.len(),
        columns,
        limits,
        SELECT_RESULT_RESOURCES,
    )?;
    bytes = bytes.saturating_add(cross_join_string_bytes(left, right, row_count));
    enforce_resource_limit(SELECT_RESULT_RESOURCES.bytes, bytes, limits.max_bytes)
}

/// Counts cloned string payload bytes in the LIMIT-truncated left-major
/// product without constructing row or value vectors.
fn cross_join_string_bytes(left: &Table, right: &Table, row_count: usize) -> usize {
    if row_count == 0 {
        return 0;
    }

    let right_rows = right.row_count();
    debug_assert!(right_rows > 0);
    let complete_left_rows = row_count / right_rows;
    let partial_right_rows = row_count % right_rows;
    let mut bytes = 0_usize;

    for left_row in 0..complete_left_rows {
        bytes = bytes.saturating_add(string_bytes_at(left, left_row).saturating_mul(right_rows));
    }
    if partial_right_rows > 0 {
        bytes = bytes.saturating_add(
            string_bytes_at(left, complete_left_rows).saturating_mul(partial_right_rows),
        );
    }

    if complete_left_rows > 0 {
        let right_product_bytes = (0..right_rows)
            .map(|right_row| string_bytes_at(right, right_row))
            .fold(0_usize, usize::saturating_add)
            .saturating_mul(complete_left_rows);
        bytes = bytes.saturating_add(right_product_bytes);
    }
    for right_row in 0..partial_right_rows {
        bytes = bytes.saturating_add(string_bytes_at(right, right_row));
    }
    bytes
}

fn string_bytes_at(table: &Table, row: usize) -> usize {
    table
        .columns()
        .iter()
        .map(|column| match column.value_ref(row) {
            ValueRef::String(value) => value.len(),
            ValueRef::Null(_) | ValueRef::Int64(_) | ValueRef::Float64(_) | ValueRef::Bool(_) => 0,
        })
        .fold(0_usize, usize::saturating_add)
}

fn validate_projection_result_limits(
    table: &Table,
    rows: &[usize],
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<()> {
    let mut bytes =
        validate_select_result_shape(rows.len(), items.len(), columns, limits, result_prefix)?;
    for row in rows {
        for item in items {
            let source = match item {
                ResolvedItem::Column { source, .. }
                | ResolvedItem::StringLower { source }
                | ResolvedItem::StringUpper { source } => Some(*source),
                ResolvedItem::Int64Subtract { .. }
                | ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastBoolToString { .. }
                | ResolvedItem::StringLength { .. }
                | ResolvedItem::Int64Abs { .. }
                | ResolvedItem::Float64Round { .. }
                | ResolvedItem::Float64Floor { .. }
                | ResolvedItem::Float64Ceil { .. }
                | ResolvedItem::RowNumber => None,
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("ungrouped projections cannot contain aggregates")
                }
            };
            if let Some(source) = source {
                if let ValueRef::String(value) = table.columns()[source].value_ref(*row) {
                    bytes = bytes.saturating_add(value.len());
                    enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
                }
            } else {
                let cast_string_len = match item {
                    ResolvedItem::CastInt64ToString { source } => {
                        Some(int64_text_len(int64_at(table, *source, *row)))
                    }
                    ResolvedItem::CastBoolToString { source } => {
                        Some(bool_string(bool_at(table, *source, *row)).len())
                    }
                    _ => None,
                };
                if let Some(cast_string_len) = cast_string_len {
                    bytes = bytes.saturating_add(cast_string_len);
                    enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_grouped_result_limits(
    data: &GroupedData<'_>,
    groups: &[usize],
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<()> {
    let mut bytes =
        validate_select_result_shape(groups.len(), items.len(), columns, limits, result_prefix)?;
    for group in groups {
        for item in items {
            let string_len = match item {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => match data.keys[*group].value(*position) {
                    ValueRef::String(value) => value.len(),
                    _ => 0,
                },
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Int64Subtract { .. } => {
                    unreachable!(
                        "Int64 subtraction projections are restricted to ungrouped queries"
                    )
                }
                ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastBoolToString { .. } => {
                    unreachable!("CAST projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLength { .. } => {
                    unreachable!("LENGTH projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLower { .. } => {
                    unreachable!("LOWER projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringUpper { .. } => {
                    unreachable!("UPPER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Int64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Round { .. } => {
                    unreachable!("ROUND projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Floor { .. } => {
                    unreachable!("FLOOR projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Ceil { .. } => {
                    unreachable!("CEIL projections are restricted to ungrouped queries")
                }
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Aggregate { state } => match &data.aggregates[*state][*group] {
                    Value::String(value) => value.len(),
                    _ => 0,
                },
            };
            bytes = bytes.saturating_add(string_len);
            enforce_resource_limit("SELECT result bytes", bytes, limits.max_bytes)?;
        }
    }
    Ok(())
}

fn validate_select_result_shape(
    row_count: usize,
    column_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    result_prefix: Option<SelectResultPrefix<'_>>,
) -> Result<usize> {
    let combined_row_count = result_prefix.map_or(row_count, |prefix| {
        prefix.row_count.saturating_add(row_count)
    });
    let output_columns = result_prefix.map_or(columns, |prefix| prefix.columns);
    let bytes = validate_result_shape(
        combined_row_count,
        column_count,
        output_columns,
        limits,
        SELECT_RESULT_RESOURCES,
    )?
    .saturating_add(result_prefix.map_or(0, |prefix| prefix.string_bytes));
    enforce_resource_limit(SELECT_RESULT_RESOURCES.bytes, bytes, limits.max_bytes)?;
    Ok(bytes)
}

fn validate_result_shape(
    row_count: usize,
    column_count: usize,
    columns: &[ResultColumn],
    limits: QueryResultLimits,
    resources: QueryResultResources,
) -> Result<usize> {
    let column_name_bytes = columns
        .iter()
        .map(|column| column.name.len())
        .fold(0_usize, usize::saturating_add);
    validate_result_shape_parts(
        row_count,
        column_count,
        columns.len(),
        column_name_bytes,
        limits,
        resources,
    )
}

fn validate_result_shape_parts(
    row_count: usize,
    values_per_row: usize,
    result_column_count: usize,
    result_column_name_bytes: usize,
    limits: QueryResultLimits,
    resources: QueryResultResources,
) -> Result<usize> {
    enforce_resource_limit(resources.rows, row_count, limits.max_rows)?;
    let value_count = row_count.saturating_mul(values_per_row);
    enforce_resource_limit(resources.values, value_count, limits.max_values)?;

    let column_bytes = result_column_count
        .saturating_mul(std::mem::size_of::<ResultColumn>())
        .saturating_add(result_column_name_bytes);
    let bytes = column_bytes
        .saturating_add(row_count.saturating_mul(std::mem::size_of::<Vec<Value>>()))
        .saturating_add(value_count.saturating_mul(std::mem::size_of::<Value>()));
    enforce_resource_limit(resources.bytes, bytes, limits.max_bytes)?;
    Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
struct QueryResultResources {
    rows: &'static str,
    values: &'static str,
    bytes: &'static str,
}

const SELECT_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SELECT result rows",
    values: "SELECT result values",
    bytes: "SELECT result bytes",
};

const SHOW_TABLES_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW TABLES result rows",
    values: "SHOW TABLES result values",
    bytes: "SHOW TABLES result bytes",
};

const SHOW_CREATE_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "SHOW CREATE TABLE result rows",
    values: "SHOW CREATE TABLE result values",
    bytes: "SHOW CREATE TABLE result bytes",
};

const DESCRIBE_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "DESCRIBE TABLE result rows",
    values: "DESCRIBE TABLE result values",
    bytes: "DESCRIBE TABLE result bytes",
};

const EXISTS_TABLE_RESULT_RESOURCES: QueryResultResources = QueryResultResources {
    rows: "EXISTS TABLE result rows",
    values: "EXISTS TABLE result values",
    bytes: "EXISTS TABLE result bytes",
};

fn enforce_resource_limit(resource: &'static str, actual: usize, max: usize) -> Result<()> {
    if actual > max {
        Err(Error::ResourceLimitExceeded {
            resource,
            actual,
            max,
        })
    } else {
        Ok(())
    }
}

fn enforce_select_scan_limit(table: &Table, limits: QueryResultLimits) -> Result<()> {
    enforce_scan_limit(table, limits, "SELECT scanned rows")
}

fn enforce_scan_limit(
    table: &Table,
    limits: QueryResultLimits,
    resource: &'static str,
) -> Result<()> {
    enforce_resource_limit(resource, table.row_count(), limits.max_scan_rows)
}

fn execute_grouped<'a>(
    table: &'a Table,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
    limits: QueryResultLimits,
) -> Result<GroupedData<'a>> {
    let planned_group_count = if group_columns.is_empty() {
        enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
        1
    } else {
        matching_rows.len().min(limits.max_groups)
    };
    let planned_state_cells = planned_group_count.saturating_mul(aggregate_specs.len());
    enforce_resource_limit(
        "SELECT aggregate state cells",
        planned_state_cells,
        limits.max_aggregate_state_cells,
    )?;
    let mut aggregate_state_bytes = planned_state_cells
        .saturating_mul(std::mem::size_of::<AggregateState>())
        .saturating_add(
            aggregate_specs
                .len()
                .saturating_mul(std::mem::size_of::<Vec<AggregateState>>()),
        );
    enforce_resource_limit(
        "SELECT aggregate state bytes",
        aggregate_state_bytes,
        limits.max_aggregate_state_bytes,
    )?;

    let key_cells_per_group = group_columns.len();
    let key_bytes_per_group = key_cells_per_group.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
    let probe_key_cells = if group_columns.len() > 2 && !matching_rows.is_empty() {
        key_cells_per_group
    } else {
        0
    };
    let probe_key_bytes = probe_key_cells.saturating_mul(ESTIMATED_GROUP_KEY_CELL_BYTES);
    if !group_columns.is_empty() && !matching_rows.is_empty() {
        enforce_resource_limit("SELECT groups", 1, limits.max_groups)?;
        let first_group_key_cells = probe_key_cells.saturating_add(key_cells_per_group);
        enforce_resource_limit(
            "SELECT group key cells",
            first_group_key_cells,
            limits.max_group_key_cells,
        )?;
        let first_group_key_bytes = probe_key_bytes.saturating_add(key_bytes_per_group);
        enforce_resource_limit(
            "SELECT group key bytes",
            first_group_key_bytes,
            limits.max_group_key_bytes,
        )?;
    }

    let mut groups = GroupIndex::new(group_columns.len());
    let mut group_count = usize::from(group_columns.is_empty());
    let mut group_key_cells = probe_key_cells;
    let mut group_key_bytes = probe_key_bytes;
    let mut multiple_key_probe = Vec::with_capacity(probe_key_cells);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(planned_group_count);
            if group_columns.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for row in matching_rows {
        let existing_group = groups.find(table, group_columns, *row, &mut multiple_key_probe);
        let (group, inserted) = if let Some(group) = existing_group {
            (group, false)
        } else {
            let next_group_count = group_count.saturating_add(1);
            enforce_resource_limit("SELECT groups", next_group_count, limits.max_groups)?;
            let next_key_cells = group_key_cells.saturating_add(key_cells_per_group);
            enforce_resource_limit(
                "SELECT group key cells",
                next_key_cells,
                limits.max_group_key_cells,
            )?;
            let next_key_bytes = group_key_bytes.saturating_add(key_bytes_per_group);
            enforce_resource_limit(
                "SELECT group key bytes",
                next_key_bytes,
                limits.max_group_key_bytes,
            )?;

            let group = group_count;
            groups.insert(table, group_columns, *row, group, &multiple_key_probe);
            group_count = next_group_count;
            group_key_cells = next_key_cells;
            group_key_bytes = next_key_bytes;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
            (group, true)
        };
        debug_assert!(!inserted || group + 1 == group_count);
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(
                spec,
                table,
                *row,
                &mut aggregate_state_bytes,
                limits.max_aggregate_state_bytes,
            )?;
        }
    }

    let keys = groups.into_keys(group_count);
    let aggregates = aggregate_states
        .into_iter()
        .zip(aggregate_specs)
        .map(|(states, spec)| {
            states
                .into_iter()
                .map(|state| state.finish(spec))
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
    fn new(column_count: usize) -> Self {
        match column_count {
            0 => Self::Global,
            1 => Self::One(HashMap::new()),
            _ => Self::Multiple(HashMap::new()),
        }
    }

    fn find(
        &self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        multiple_key_probe: &mut Vec<ValueRef<'a>>,
    ) -> Option<usize> {
        match self {
            Self::Global => Some(0),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.get(&key).copied()
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.get(key.as_slice()).copied()
            }
            Self::Multiple(groups) => {
                multiple_key_probe.clear();
                multiple_key_probe.extend(
                    columns
                        .iter()
                        .map(|column| table.columns()[*column].value_ref(row)),
                );
                groups.get(multiple_key_probe.as_slice()).copied()
            }
        }
    }

    fn insert(
        &mut self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        group: usize,
        multiple_key_probe: &[ValueRef<'a>],
    ) {
        let previous = match self {
            Self::Global => unreachable!("global aggregation has no grouped key to insert"),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.insert(key, group)
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.insert(key.into(), group)
            }
            Self::Multiple(groups) => {
                debug_assert_eq!(multiple_key_probe.len(), columns.len());
                groups.insert(multiple_key_probe.into(), group)
            }
        };
        debug_assert!(previous.is_none(), "new group keys must be unique");
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
                        ResolvedItem::Int64Subtract { .. } => {
                            unreachable!(
                                "Int64 subtraction projections are restricted to ungrouped queries"
                            )
                        }
                        ResolvedItem::CastInt64ToFloat64 { .. }
                        | ResolvedItem::CastFloat64ToInt64 { .. }
                        | ResolvedItem::CastBoolToInt64 { .. }
                        | ResolvedItem::CastInt64ToBool { .. }
                        | ResolvedItem::CastFloat64ToBool { .. }
                        | ResolvedItem::CastInt64ToString { .. }
                        | ResolvedItem::CastBoolToString { .. } => {
                            unreachable!("CAST projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringLength { .. } => {
                            unreachable!("LENGTH projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringLower { .. } => {
                            unreachable!("LOWER projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::StringUpper { .. } => {
                            unreachable!("UPPER projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Int64Abs { .. } => {
                            unreachable!("ABS projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Round { .. } => {
                            unreachable!("ROUND projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Floor { .. } => {
                            unreachable!("FLOOR projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::Float64Ceil { .. } => {
                            unreachable!("CEIL projections are restricted to ungrouped queries")
                        }
                        ResolvedItem::RowNumber => {
                            unreachable!(
                                "ROW_NUMBER projections are restricted to ungrouped queries"
                            )
                        }
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
    SumInt { sum: i128, count: u64 },
    SumFloat { sum: ScaledFloatSum, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: ScaledFloatSum, count: u64 },
}

#[derive(Debug, Default)]
struct ScaledFloatSum {
    scale: f64,
    normalized_sum: f64,
    correction: f64,
}

impl ScaledFloatSum {
    fn add(&mut self, value: f64) {
        let magnitude = value.abs();
        if magnitude > self.scale {
            if self.scale != 0.0 {
                let ratio = self.scale / magnitude;
                self.normalized_sum *= ratio;
                self.correction *= ratio;
            }
            self.scale = magnitude;
        }
        if self.scale == 0.0 {
            return;
        }

        let normalized = value / self.scale;
        let next = self.normalized_sum + normalized;
        if self.normalized_sum.abs() >= normalized.abs() {
            self.correction += (self.normalized_sum - next) + normalized;
        } else {
            self.correction += (normalized - next) + self.normalized_sum;
        }
        self.normalized_sum = next;
    }

    fn normalized_total(&self) -> f64 {
        self.normalized_sum + self.correction
    }

    fn total(&self) -> f64 {
        self.normalized_total() * self.scale
    }

    fn mean(&self, count: u64) -> f64 {
        let normalized = (self.normalized_total() / count as f64).clamp(-1.0, 1.0);
        normalized * self.scale
    }
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => {
                Self::SumInt { sum: 0, count: 0 }
            }
            AggregateFunction::Sum => Self::SumFloat {
                sum: ScaledFloatSum::default(),
                count: 0,
            },
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat {
                sum: ScaledFloatSum::default(),
                count: 0,
            },
        }
    }

    fn update(
        &mut self,
        spec: &AggregateSpec,
        table: &Table,
        row: usize,
        aggregate_state_bytes: &mut usize,
        max_aggregate_state_bytes: usize,
    ) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt { sum, count } => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64) exact sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
            }
            Self::SumFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                sum.add(values[row]);
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("SUM count".to_owned()))?;
            }
            Self::Min(current) => {
                let column = &table.columns()[spec.argument.expect("MIN argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    replace_extreme(
                        current,
                        candidate,
                        aggregate_state_bytes,
                        max_aggregate_state_bytes,
                    )?;
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    replace_extreme(
                        current,
                        candidate,
                        aggregate_state_bytes,
                        max_aggregate_state_bytes,
                    )?;
                }
            }
            Self::AvgInt { sum, count } => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                sum.add(values[row]);
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
        }
        Ok(())
    }

    fn finish(self, spec: &AggregateSpec) -> Result<Value> {
        match self {
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt { count: 0, .. } => Ok(Value::Null(DataType::Int64)),
            Self::SumInt { sum, .. } => i64::try_from(sum)
                .map(Value::Int64)
                .map_err(|_| Error::NumericOverflow("SUM(Int64)".to_owned())),
            Self::SumFloat { count: 0, .. } => Ok(Value::Null(DataType::Float64)),
            Self::SumFloat { sum, .. } => {
                let value = sum.total();
                if value.is_finite() {
                    Ok(Value::Float64(value))
                } else {
                    Err(Error::NumericOverflow("SUM(Float64)".to_owned()))
                }
            }
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum.mean(count))),
            Self::Min(None) | Self::Max(None) => Ok(Value::Null(
                spec.input_type.expect("MIN and MAX have column arguments"),
            )),
            Self::AvgInt { .. } | Self::AvgFloat { .. } => Ok(Value::Null(DataType::Float64)),
        }
    }
}

fn replace_extreme(
    current: &mut Option<Value>,
    candidate: ValueRef<'_>,
    aggregate_state_bytes: &mut usize,
    max_aggregate_state_bytes: usize,
) -> Result<()> {
    let previous_string_bytes = current
        .as_ref()
        .and_then(|value| match value {
            Value::String(value) => Some(value.len()),
            _ => None,
        })
        .unwrap_or(0);
    let candidate_string_bytes = match candidate {
        ValueRef::String(value) => value.len(),
        _ => 0,
    };
    let next_bytes = aggregate_state_bytes
        .saturating_sub(previous_string_bytes)
        .saturating_add(candidate_string_bytes);
    enforce_resource_limit(
        "SELECT aggregate state bytes",
        next_bytes,
        max_aggregate_state_bytes,
    )?;
    *current = Some(candidate.to_owned());
    *aggregate_state_bytes = next_bytes;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOrder {
    output: usize,
    descending: bool,
}

fn resolve_ordering(
    table: &Table,
    items: &[ResolvedItem],
    aggregate_specs: &[AggregateSpec],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    debug_assert_eq!(items.len(), columns.len());
    if requested.is_empty() {
        return Ok(Vec::new());
    }

    let expression_names = items
        .iter()
        .map(|item| resolved_expression_name(table, item, aggregate_specs))
        .collect::<Vec<_>>();
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let output_matches = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let matches = if output_matches.is_empty() {
            expression_names
                .iter()
                .enumerate()
                .filter(|(_, expression)| expression.eq_ignore_ascii_case(&order.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        } else {
            output_matches
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

fn resolved_expression_name(
    table: &Table,
    item: &ResolvedItem,
    aggregate_specs: &[AggregateSpec],
) -> String {
    match item {
        ResolvedItem::Column { source, .. } => table.schema()[*source].name.clone(),
        ResolvedItem::Int64Subtract { source, literal } => {
            sql::int64_subtraction_name(&table.schema()[*source].name, *literal)
        }
        ResolvedItem::CastInt64ToFloat64 { source } => {
            format!("CAST({} AS Float64)", table.schema()[*source].name)
        }
        ResolvedItem::CastFloat64ToInt64 { source } | ResolvedItem::CastBoolToInt64 { source } => {
            format!("CAST({} AS Int64)", table.schema()[*source].name)
        }
        ResolvedItem::CastInt64ToBool { source } | ResolvedItem::CastFloat64ToBool { source } => {
            format!("CAST({} AS Bool)", table.schema()[*source].name)
        }
        ResolvedItem::CastInt64ToString { source } | ResolvedItem::CastBoolToString { source } => {
            format!("CAST({} AS String)", table.schema()[*source].name)
        }
        ResolvedItem::StringLength { source } => {
            format!("LENGTH({})", table.schema()[*source].name)
        }
        ResolvedItem::StringLower { source } => {
            format!("LOWER({})", table.schema()[*source].name)
        }
        ResolvedItem::StringUpper { source } => {
            format!("UPPER({})", table.schema()[*source].name)
        }
        ResolvedItem::Int64Abs { source } => {
            format!("ABS({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Round { source } => {
            format!("ROUND({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Floor { source } => {
            format!("FLOOR({})", table.schema()[*source].name)
        }
        ResolvedItem::Float64Ceil { source } => {
            format!("CEIL({})", table.schema()[*source].name)
        }
        ResolvedItem::RowNumber => "ROW_NUMBER()".to_owned(),
        ResolvedItem::Aggregate { state } => {
            let spec = &aggregate_specs[*state];
            let argument = spec
                .argument
                .map(|source| table.schema()[source].name.as_str())
                .unwrap_or("*");
            format!("{}({argument})", spec.function.name())
        }
    }
}

fn order_source_rows(
    rows: &mut Vec<usize>,
    table: &Table,
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
            let comparison = match items[order.output] {
                ResolvedItem::Column { source, .. } => table.columns()[source].cmp_at(left, right),
                // Subtracting one constant is monotonic over mathematical
                // integers. Compare the source values so overflow is checked
                // only after ORDER BY and LIMIT have selected output rows.
                ResolvedItem::Int64Subtract { source, .. } => {
                    int64_at(table, source, left).cmp(&int64_at(table, source, right))
                }
                ResolvedItem::CastInt64ToFloat64 { source } => {
                    let left = ValueRef::Float64(int64_at(table, source, left) as f64);
                    let right = ValueRef::Float64(int64_at(table, source, right) as f64);
                    left.cmp(&right)
                }
                ResolvedItem::CastFloat64ToInt64 { source } => {
                    let left = ValueRef::Float64(float64_at(table, source, left).trunc());
                    let right = ValueRef::Float64(float64_at(table, source, right).trunc());
                    left.cmp(&right)
                }
                ResolvedItem::CastBoolToInt64 { source } => {
                    bool_at(table, source, left).cmp(&bool_at(table, source, right))
                }
                ResolvedItem::CastInt64ToBool { source } => {
                    (int64_at(table, source, left) != 0).cmp(&(int64_at(table, source, right) != 0))
                }
                ResolvedItem::CastFloat64ToBool { source } => (float64_at(table, source, left)
                    != 0.0)
                    .cmp(&(float64_at(table, source, right) != 0.0)),
                ResolvedItem::CastInt64ToString { source } => int64_text_cmp(
                    int64_at(table, source, left),
                    int64_at(table, source, right),
                ),
                ResolvedItem::CastBoolToString { source } => {
                    bool_at(table, source, left).cmp(&bool_at(table, source, right))
                }
                ResolvedItem::StringLength { source } => string_at(table, source, left)
                    .len()
                    .cmp(&string_at(table, source, right).len()),
                ResolvedItem::StringLower { source } => ascii_lower_cmp(
                    string_at(table, source, left),
                    string_at(table, source, right),
                ),
                ResolvedItem::StringUpper { source } => ascii_upper_cmp(
                    string_at(table, source, left),
                    string_at(table, source, right),
                ),
                ResolvedItem::Int64Abs { source } => int64_at(table, source, left)
                    .unsigned_abs()
                    .cmp(&int64_at(table, source, right).unsigned_abs()),
                ResolvedItem::Float64Round { source } => {
                    let left = ValueRef::Float64(float64_at(table, source, left).round());
                    let right = ValueRef::Float64(float64_at(table, source, right).round());
                    left.cmp(&right)
                }
                ResolvedItem::Float64Floor { source } => {
                    let left = ValueRef::Float64(float64_at(table, source, left).floor());
                    let right = ValueRef::Float64(float64_at(table, source, right).floor());
                    left.cmp(&right)
                }
                ResolvedItem::Float64Ceil { source } => {
                    let left = ValueRef::Float64(float64_at(table, source, left).ceil());
                    let right = ValueRef::Float64(float64_at(table, source, right).ceil());
                    left.cmp(&right)
                }
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections cannot be ordered")
                }
                ResolvedItem::Aggregate { .. } => {
                    unreachable!("ungrouped projections cannot contain aggregates")
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
                ResolvedItem::Int64Subtract { .. } => {
                    unreachable!(
                        "Int64 subtraction projections are restricted to ungrouped queries"
                    )
                }
                ResolvedItem::CastInt64ToFloat64 { .. }
                | ResolvedItem::CastFloat64ToInt64 { .. }
                | ResolvedItem::CastBoolToInt64 { .. }
                | ResolvedItem::CastInt64ToBool { .. }
                | ResolvedItem::CastFloat64ToBool { .. }
                | ResolvedItem::CastInt64ToString { .. }
                | ResolvedItem::CastBoolToString { .. } => {
                    unreachable!("CAST projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLength { .. } => {
                    unreachable!("LENGTH projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringLower { .. } => {
                    unreachable!("LOWER projections are restricted to ungrouped queries")
                }
                ResolvedItem::StringUpper { .. } => {
                    unreachable!("UPPER projections are restricted to ungrouped queries")
                }
                ResolvedItem::Int64Abs { .. } => {
                    unreachable!("ABS projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Round { .. } => {
                    unreachable!("ROUND projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Floor { .. } => {
                    unreachable!("FLOOR projections are restricted to ungrouped queries")
                }
                ResolvedItem::Float64Ceil { .. } => {
                    unreachable!("CEIL projections are restricted to ungrouped queries")
                }
                ResolvedItem::RowNumber => {
                    unreachable!("ROW_NUMBER projections are restricted to ungrouped queries")
                }
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

fn int64_at(table: &Table, source: usize, row: usize) -> i64 {
    let Column::Int64(values) = &table.columns()[source] else {
        unreachable!("CAST input type is resolved")
    };
    values[row]
}

fn float64_at(table: &Table, source: usize, row: usize) -> f64 {
    let Column::Float64(values) = &table.columns()[source] else {
        unreachable!("CAST input type is resolved")
    };
    values[row]
}

fn bool_at(table: &Table, source: usize, row: usize) -> bool {
    let Column::Bool(values) = &table.columns()[source] else {
        unreachable!("CAST input type is resolved")
    };
    values[row]
}

fn bool_string(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn int64_text_len(value: i64) -> usize {
    let magnitude = value.unsigned_abs();
    let digits = if magnitude == 0 {
        1
    } else {
        magnitude.ilog10() as usize + 1
    };
    digits + usize::from(value.is_negative())
}

fn int64_text_cmp(left: i64, right: i64) -> Ordering {
    let (left_bytes, left_start) = render_int64_text(left);
    let (right_bytes, right_start) = render_int64_text(right);
    left_bytes[left_start..].cmp(&right_bytes[right_start..])
}

fn render_int64_text(value: i64) -> ([u8; 20], usize) {
    let mut bytes = [0_u8; 20];
    let mut start = bytes.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        start -= 1;
        bytes[start] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value.is_negative() {
        start -= 1;
        bytes[start] = b'-';
    }
    (bytes, start)
}

fn checked_float64_to_int64(value: f64) -> Result<i64> {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

    if !value.is_finite() || value < i64::MIN as f64 || value >= I64_UPPER_EXCLUSIVE {
        return Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned()));
    }
    Ok(value.trunc() as i64)
}

fn string_at(table: &Table, source: usize, row: usize) -> &str {
    let Column::String(values) = &table.columns()[source] else {
        unreachable!("String scalar input type is resolved")
    };
    &values[row]
}

fn ascii_lower_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

fn ascii_upper_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_uppercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_uppercase()))
}

fn string_length_to_i64(length: usize) -> Result<i64> {
    i64::try_from(length).map_err(|_| Error::NumericOverflow("LENGTH(String)".to_owned()))
}

fn checked_int64_abs(value: i64) -> Result<i64> {
    value
        .checked_abs()
        .ok_or_else(|| Error::NumericOverflow("ABS(Int64)".to_owned()))
}

fn checked_int64_subtract(value: i64, literal: i64) -> Result<i64> {
    value
        .checked_sub(literal)
        .ok_or_else(|| Error::NumericOverflow("Int64 subtraction".to_owned()))
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
    LikePrefix {
        column: usize,
        prefix: String,
        negated: bool,
    },
    LikeContains {
        column: usize,
        substring: String,
        negated: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, table: &Table, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(table, row);
                let right = right.value(table, row);
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
            Self::LikePrefix {
                column,
                prefix,
                negated,
            } => string_at(table, *column, row).starts_with(prefix.as_str()) != *negated,
            Self::LikeContains {
                column,
                substring,
                negated,
            } => string_at(table, *column, row).contains(substring.as_str()) != *negated,
            Self::And(left, right) => left.evaluate(table, row) && right.evaluate(table, row),
            Self::Or(left, right) => left.evaluate(table, row) || right.evaluate(table, row),
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

    fn value<'a>(&'a self, table: &'a Table, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => table.columns()[*index].value_ref(row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(table: &Table, predicate: &Predicate) -> Result<CompiledPredicate> {
    compile_predicate_with_polarity(table, predicate, false)
}

fn compile_predicate_with_polarity(
    table: &Table,
    predicate: &Predicate,
    negated: bool,
) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(table, left)?;
            let right = compile_operand(table, right)?;
            if !comparable(left.data_type(), right.data_type()) {
                return Err(Error::TypeMismatch {
                    context: "WHERE comparison".to_owned(),
                    expected: left.data_type().to_string(),
                    actual: right.data_type().to_string(),
                });
            }
            Ok(CompiledPredicate::Comparison {
                left,
                operator: if negated {
                    invert_comparison(*operator)
                } else {
                    *operator
                },
                right,
            })
        }
        Predicate::LikePrefix { column, prefix } => {
            let column_index = table.column_index(column)?;
            let actual = table.schema()[column_index].data_type;
            if actual != DataType::String {
                return Err(Error::TypeMismatch {
                    context: format!("WHERE LIKE column '{column}'"),
                    expected: DataType::String.to_string(),
                    actual: actual.to_string(),
                });
            }
            Ok(CompiledPredicate::LikePrefix {
                column: column_index,
                prefix: prefix.clone(),
                negated,
            })
        }
        Predicate::LikeContains { column, substring } => {
            let column_index = table.column_index(column)?;
            let actual = table.schema()[column_index].data_type;
            if actual != DataType::String {
                return Err(Error::TypeMismatch {
                    context: format!("WHERE LIKE column '{column}'"),
                    expected: DataType::String.to_string(),
                    actual: actual.to_string(),
                });
            }
            Ok(CompiledPredicate::LikeContains {
                column: column_index,
                substring: substring.clone(),
                negated,
            })
        }
        Predicate::Not(predicate) => compile_predicate_with_polarity(table, predicate, !negated),
        Predicate::And(left, right) if negated => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate_with_polarity(table, left, true)?),
            Box::new(compile_predicate_with_polarity(table, right, true)?),
        )),
        Predicate::And(left, right) => Ok(CompiledPredicate::And(
            Box::new(compile_predicate_with_polarity(table, left, false)?),
            Box::new(compile_predicate_with_polarity(table, right, false)?),
        )),
        Predicate::Or(left, right) if negated => Ok(CompiledPredicate::And(
            Box::new(compile_predicate_with_polarity(table, left, true)?),
            Box::new(compile_predicate_with_polarity(table, right, true)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate_with_polarity(table, left, false)?),
            Box::new(compile_predicate_with_polarity(table, right, false)?),
        )),
    }
}

const fn invert_comparison(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::NotEqual,
        ComparisonOperator::NotEqual => ComparisonOperator::Equal,
        ComparisonOperator::Less => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::LessOrEqual => ComparisonOperator::Greater,
        ComparisonOperator::Greater => ComparisonOperator::LessOrEqual,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::Less,
    }
}

fn compile_operand(table: &Table, operand: &Operand) -> Result<CompiledOperand> {
    let name = match operand {
        Operand::Column(name) => name.as_str(),
        Operand::SharedColumn(name) => name.as_ref(),
        Operand::Literal(value) => {
            validate_predicate_literal_value(value)?;
            return Ok(CompiledOperand::Literal(value.clone()));
        }
    };
    let index = table.column_index(name)?;
    Ok(CompiledOperand::Column {
        index,
        data_type: table.schema()[index].data_type,
    })
}

fn validate_predicate_literal_value(value: &Value) -> Result<()> {
    match value {
        Value::Null(_) => Err(Error::InvalidQuery(
            "WHERE comparisons do not support NULL literals".to_owned(),
        )),
        Value::Float64(value) if !value.is_finite() => Err(Error::InvalidQuery(
            "WHERE comparison Float64 literals must be finite".to_owned(),
        )),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) | Value::String(_) => Ok(()),
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
    fn insert_batch_preflight_rejects_non_finite_ast_values_without_mutation() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (id Int64); CREATE TABLE samples (value Float64);")
            .expect("setup");
        let statements = vec![
            Statement::Insert {
                table: "events".to_owned(),
                rows: vec![vec![Value::Int64(1)]],
            },
            Statement::Insert {
                table: "samples".to_owned(),
                rows: vec![vec![Value::Float64(f64::INFINITY)]],
            },
        ];

        assert_eq!(
            database.execute_insert_statements(statements),
            Err(Error::InvalidQuery(
                "column 'samples.value' cannot store a non-finite Float64".to_owned()
            ))
        );
        assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
        assert_eq!(database.catalog().table("samples").unwrap().row_count(), 0);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn string_length_reports_int64_overflow() {
        let overflow = usize::try_from(i64::MAX).unwrap() + 1;
        assert_eq!(
            string_length_to_i64(overflow),
            Err(Error::NumericOverflow("LENGTH(String)".to_owned()))
        );
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
    fn show_tables_returns_typed_empty_and_case_preserving_sorted_results() {
        let mut database = Database::new();
        let empty = query(&mut database, "SHOW TABLES");
        assert_eq!(
            empty.columns,
            [ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            }]
        );
        assert!(empty.rows.is_empty());

        database
            .execute(
                "CREATE TABLE zebra (id Int64); \
                 CREATE TABLE Alpha (id Int64); \
                 CREATE TABLE beta (id Int64);",
            )
            .expect("setup");
        assert_eq!(
            query(&mut database, "show tables;").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
                vec![Value::String("zebra".to_owned())],
            ]
        );
    }

    #[test]
    fn show_tables_accepts_exact_custom_row_and_value_limits() {
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 2,
            max_values: 2,
            ..QueryResultLimits::default()
        });
        database
            .execute("CREATE TABLE beta (id Int64); CREATE TABLE Alpha (id Int64);")
            .expect("setup");

        assert_eq!(
            query(&mut database, "SHOW TABLES").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
            ]
        );
    }

    #[test]
    fn show_tables_rejects_exceeded_custom_row_and_value_limits() {
        let cases = [
            (
                QueryResultLimits {
                    max_rows: 1,
                    ..QueryResultLimits::default()
                },
                "SHOW TABLES result rows",
            ),
            (
                QueryResultLimits {
                    max_rows: 2,
                    max_values: 1,
                    ..QueryResultLimits::default()
                },
                "SHOW TABLES result values",
            ),
        ];

        for (limits, resource) in cases {
            let mut database = Database::with_query_result_limits(limits);
            database
                .execute("CREATE TABLE Alpha (id Int64); CREATE TABLE beta (id Int64);")
                .expect("setup");
            let error = database
                .execute("SHOW TABLES")
                .expect_err("SHOW TABLES exceeds its configured result limit");
            assert_eq!(
                error,
                Error::ResourceLimitExceeded {
                    resource,
                    actual: 2,
                    max: 1,
                }
            );
        }
    }

    #[test]
    fn show_tables_accepts_exact_and_rejects_exceeded_name_payload_byte_limit() {
        let table_count = 2;
        let columns = [ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }];
        let fixed_bytes = validate_result_shape(
            table_count,
            1,
            &columns,
            QueryResultLimits::default(),
            SHOW_TABLES_RESULT_RESOURCES,
        )
        .expect("fixed result shape fits default limits");
        let name_bytes = "Alpha".len() + "beta".len();
        let exact_bytes = fixed_bytes + name_bytes;
        let mut exact_database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: table_count,
            max_values: table_count,
            max_bytes: exact_bytes,
            ..QueryResultLimits::default()
        });
        exact_database
            .execute("CREATE TABLE beta (id Int64); CREATE TABLE Alpha (id Int64);")
            .expect("setup");
        assert_eq!(
            query(&mut exact_database, "SHOW TABLES").rows,
            [
                vec![Value::String("Alpha".to_owned())],
                vec![Value::String("beta".to_owned())],
            ]
        );

        let max_bytes = exact_bytes - 1;
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: table_count,
            max_values: table_count,
            max_bytes,
            ..QueryResultLimits::default()
        });
        database
            .execute("CREATE TABLE Alpha (id Int64); CREATE TABLE beta (id Int64);")
            .expect("setup");

        assert_eq!(
            database
                .execute("SHOW TABLES")
                .expect_err("name payload exceeds the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SHOW TABLES result bytes",
                actual: exact_bytes,
                max: max_bytes,
            }
        );
    }

    #[test]
    fn show_tables_obeys_retained_result_limits() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE Alpha (id Int64)")
            .expect("setup");
        assert!(matches!(
            database.execute_with_result_limit("SHOW TABLES", 1),
            Err(Error::ResultLimitExceeded {
                bytes,
                max_bytes: 1,
            }) if bytes > 1
        ));
    }

    #[test]
    fn having_count_column_alias_supports_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String); \
                 INSERT INTO events VALUES \
                 ('a'), ('a'), ('a'), ('b'), ('b'), ('c');",
            )
            .expect("setup");

        let cases = [
            ("=", &["b"][..]),
            ("!=", &["a", "c"][..]),
            ("<>", &["a", "c"][..]),
            ("<", &["c"][..]),
            ("<=", &["b", "c"][..]),
            (">", &["a"][..]),
            (">=", &["a", "b"][..]),
        ];
        for (operator, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, COUNT(kind) AS Occurrences FROM events \
                     GROUP BY kind HAVING occurrences {operator} 2 ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_min_and_max_int64_aliases_support_grouped_and_global_inputs() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64); \
                 INSERT INTO events VALUES \
                 ('a', -5), ('a', 2), \
                 ('b', 0), ('b', 7), ('b', 9), \
                 ('c', -1);",
            )
            .expect("setup");

        let grouped_cases = [
            ("MIN", "<", "0", &["a", "c"][..]),
            ("MIN", ">=", "+0", &["b"][..]),
            ("MAX", "=", "2", &["a"][..]),
            ("MAX", "!=", "2", &["b", "c"][..]),
            ("MAX", "<>", "2", &["b", "c"][..]),
            ("MAX", "<=", "2", &["a", "c"][..]),
            ("MAX", ">", "2", &["b"][..]),
        ];
        for (function, operator, threshold, expected_kinds) in grouped_cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, {function}(amount) AS extreme FROM events \
                     GROUP BY kind HAVING extreme {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "{function} {operator}");
        }

        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(amount) AS n FROM events HAVING n = 6"
            )
            .rows,
            vec![vec![Value::Int64(6)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MIN(amount) AS low FROM events HAVING low = -5"
            )
            .rows,
            vec![vec![Value::Int64(-5)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT MAX(amount) AS high FROM events HAVING high >= +9"
            )
            .rows,
            vec![vec![Value::Int64(9)]]
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT AVG(amount) AS mean FROM events HAVING mean = 2"
            )
            .rows,
            vec![vec![Value::Float64(2.0)]]
        );
    }

    #[test]
    fn having_sum_int64_alias_supports_signed_sums_and_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64); \
                 INSERT INTO events VALUES \
                 ('negative', -5), ('negative', 1), \
                 ('zero', -2), ('zero', 2), \
                 ('positive', 2), ('positive', 5);",
            )
            .expect("setup");

        let cases = [
            ("=", "-4", &["negative"][..]),
            ("!=", "0", &["negative", "positive"][..]),
            ("<>", "0", &["negative", "positive"][..]),
            ("<", "0", &["negative"][..]),
            ("<=", "0", &["negative", "zero"][..]),
            (">", "-4", &["positive", "zero"][..]),
            (">=", "+7", &["positive"][..]),
        ];
        for (operator, threshold, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, COUNT(*) AS n, SUM(amount) AS Total FROM events \
                     GROUP BY kind HAVING total {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_float64_sum_alias_supports_every_comparison_operator() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES ('a', 1.5), ('b', 2.5), ('c', 3.5);",
            )
            .expect("setup");

        let cases = [
            ("=", &["b"][..]),
            ("!=", &["a", "c"][..]),
            ("<>", &["a", "c"][..]),
            ("<", &["a"][..]),
            ("<=", &["a", "b"][..]),
            (">", &["c"][..]),
            (">=", &["b", "c"][..]),
        ];
        for (operator, expected_kinds) in cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, SUM(score) AS total FROM events \
                     GROUP BY kind HAVING total {operator} +2.5e0 ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "operator {operator}");
        }
    }

    #[test]
    fn having_all_float64_aggregate_aliases_support_grouped_and_global_inputs() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES \
                 ('a', 1.5), ('a', 2.5), \
                 ('b', -2.0), ('b', 6.0), \
                 ('c', 10.0);",
            )
            .expect("setup");

        let grouped_cases = [
            ("SUM", "total", ">", "4", &["c"][..]),
            ("MIN", "low", "<", "0.0", &["b"][..]),
            ("MAX", "high", ">=", "+6", &["b", "c"][..]),
            ("AVG", "mean", "=", "2", &["a", "b"][..]),
        ];
        for (function, alias, operator, threshold, expected_kinds) in grouped_cases {
            let result = query(
                &mut database,
                &format!(
                    "SELECT kind, {function}(score) AS {alias} FROM events \
                     GROUP BY kind HAVING {alias} {operator} {threshold} ORDER BY kind"
                ),
            );
            let actual_kinds = result
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(value) => value.as_str(),
                    _ => panic!("kind is a string"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_kinds, expected_kinds, "{function}");
        }

        let global_cases = [
            ("SUM", "=", "18", Value::Float64(18.0)),
            ("MIN", "=", "-2", Value::Float64(-2.0)),
            ("MAX", ">=", "10.0", Value::Float64(10.0)),
            ("AVG", ">", "3.5", Value::Float64(3.599_999_999_999_999_6)),
        ];
        for (function, operator, threshold, expected) in global_cases {
            assert_eq!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {function}(score) AS value FROM events \
                         HAVING value {operator} {threshold}"
                    ),
                )
                .rows,
                vec![vec![expected]],
                "global {function}"
            );
        }
    }

    #[test]
    fn having_uses_exact_mixed_int64_float64_comparisons() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, score Float64); \
                 INSERT INTO events VALUES ('a', 9007199254740992.0);",
            )
            .expect("setup");

        assert_eq!(
            query(
                &mut database,
                "SELECT SUM(score) AS total FROM events HAVING total < 9007199254740993"
            )
            .rows,
            vec![vec![Value::Float64(9_007_199_254_740_992.0)]]
        );
        assert!(
            query(
                &mut database,
                "SELECT SUM(score) AS total FROM events HAVING total = 9007199254740993"
            )
            .rows
            .is_empty()
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS n FROM events HAVING n < 1.5"
            )
            .rows,
            vec![vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn having_filters_before_ordering_and_limiting() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Float64); \
                 INSERT INTO events VALUES \
                 ('a', 8.5), ('a', 9.5), ('b', 3.5), ('b', 4.5), ('c', 1.5);",
            )
            .expect("setup");

        let result = query(
            &mut database,
            "SELECT kind, MAX(amount) AS high FROM events \
             GROUP BY kind HAVING high > 1.5 ORDER BY high ASC LIMIT 1",
        );
        assert_eq!(
            result.rows,
            vec![vec![Value::String("b".to_owned()), Value::Float64(4.5)]]
        );
    }

    #[test]
    fn having_nullness_filters_finalized_empty_and_populated_aggregates_before_order_and_limit() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (amount Int64, score Float64, active Bool, label String);",
            )
            .expect("setup");

        let cases = [
            ("SUM(amount)", Value::Int64(3)),
            ("MIN(label)", Value::String("present".to_owned())),
            ("MAX(active)", Value::Bool(true)),
            ("AVG(score)", Value::Float64(2.5)),
        ];
        for (aggregate, populated_value) in &cases {
            let null_result = query(
                &mut database,
                &format!(
                    "SELECT {aggregate} AS value FROM events \
                     HAVING value IS NULL ORDER BY value DESC LIMIT 1"
                ),
            );
            assert_eq!(null_result.rows.len(), 1, "empty {aggregate}");
            assert!(
                matches!(null_result.rows[0][0], Value::Null(_)),
                "empty {aggregate} is a finalized typed NULL"
            );
            assert!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NOT NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows
                .is_empty(),
                "empty {aggregate} must not satisfy IS NOT NULL"
            );

            database
                .execute("INSERT INTO events VALUES (3, 2.5, true, 'present')")
                .expect("populate aggregate input");
            assert!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows
                .is_empty(),
                "populated {aggregate} must not satisfy IS NULL"
            );
            assert_eq!(
                query(
                    &mut database,
                    &format!(
                        "SELECT {aggregate} AS value FROM events \
                         HAVING value IS NOT NULL ORDER BY value DESC LIMIT 1"
                    ),
                )
                .rows,
                vec![vec![populated_value.clone()]],
                "populated {aggregate}"
            );

            database
                .execute("TRUNCATE TABLE events")
                .expect("restore empty input for the next aggregate");
        }

        assert!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows FROM events HAVING rows IS NULL"
            )
            .rows
            .is_empty()
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(*) AS rows FROM events HAVING rows IS NOT NULL"
            )
            .rows,
            vec![vec![Value::Int64(0)]]
        );
    }

    #[test]
    fn having_rejects_unknown_ambiguous_and_unsupported_aliases() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE events (kind String, amount Int64, score Float64); \
                 INSERT INTO events VALUES ('a', 1, 1.5), ('a', 2, 2.5);",
            )
            .expect("setup");

        let cases = [
            (
                "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind HAVING missing > 0",
                "HAVING alias 'missing' is not in the SELECT output",
            ),
            (
                "SELECT kind, MIN(amount) AS Total, MAX(amount) AS total FROM events \
                 GROUP BY kind HAVING total > 0",
                "HAVING alias 'total' is ambiguous",
            ),
            (
                "SELECT kind AS total, COUNT(*) AS n FROM events GROUP BY kind HAVING total > 0",
                "HAVING alias 'total' must reference a projected numeric aggregate",
            ),
            (
                "SELECT kind, MAX(kind) AS high FROM events GROUP BY kind HAVING high > 0",
                "HAVING alias 'high' must reference a projected numeric aggregate",
            ),
        ];

        for (sql, expected) in cases {
            assert_eq!(
                database.execute(sql).expect_err("invalid HAVING alias"),
                Error::InvalidQuery(expected.to_owned()),
                "{sql}"
            );
        }

        let nullness_cases = [
            (
                "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind HAVING missing IS NULL",
                "HAVING alias 'missing' is not in the SELECT output",
            ),
            (
                "SELECT kind, MIN(amount) AS Total, MAX(amount) AS total FROM events \
                 GROUP BY kind HAVING total IS NOT NULL",
                "HAVING alias 'total' is ambiguous",
            ),
            (
                "SELECT kind AS total, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING total IS NULL",
                "HAVING alias 'total' must reference a projected aggregate",
            ),
        ];
        for (sql, expected) in nullness_cases {
            assert_eq!(
                database.execute(sql).expect_err("invalid HAVING alias"),
                Error::InvalidQuery(expected.to_owned()),
                "{sql}"
            );
        }
    }

    #[test]
    fn direct_ast_having_rejects_invalid_threshold_values() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (amount Float64); INSERT INTO events VALUES (1.5);")
            .expect("setup");
        let Statement::Select(select) =
            sql::parse("SELECT SUM(amount) AS total FROM events HAVING total > 1.0")
                .expect("baseline query parses")
                .remove(0)
        else {
            panic!("expected select");
        };

        let cases = [
            (
                Value::Float64(f64::INFINITY),
                Error::InvalidQuery(
                    "HAVING comparison Float64 thresholds must be finite".to_owned(),
                ),
            ),
            (
                Value::Null(DataType::Float64),
                Error::InvalidQuery("HAVING comparisons do not support NULL thresholds".to_owned()),
            ),
            (
                Value::String("1.0".to_owned()),
                Error::TypeMismatch {
                    context: "HAVING comparison threshold".to_owned(),
                    expected: "Int64 or Float64".to_owned(),
                    actual: "String".to_owned(),
                },
            ),
        ];
        for (value, expected) in cases {
            let mut invalid = select.clone();
            let HavingPredicate::Comparison {
                value: invalid_value,
                ..
            } = &mut invalid.having.as_mut().expect("HAVING exists").predicate
            else {
                panic!("baseline HAVING is a comparison");
            };
            *invalid_value = value;
            assert_eq!(
                database
                    .execute_statement(Statement::Select(invalid))
                    .expect_err("invalid direct AST HAVING threshold"),
                expected
            );
        }
    }

    #[test]
    fn having_handles_empty_global_and_grouped_inputs() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (kind String, amount Int64, score Float64);")
            .expect("setup");

        assert_eq!(
            query(
                &mut database,
                "SELECT COUNT(amount) AS n FROM events HAVING n = 0"
            )
            .rows,
            vec![vec![Value::Int64(0)]]
        );
        assert!(
            query(
                &mut database,
                "SELECT COUNT(*) AS n FROM events HAVING n > 0"
            )
            .rows
            .is_empty()
        );
        assert!(
            query(
                &mut database,
                "SELECT kind, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING n = 0"
            )
            .rows
            .is_empty()
        );

        for function in ["SUM", "MIN", "MAX"] {
            assert_eq!(
                query(
                    &mut database,
                    &format!("SELECT {function}(amount) AS value FROM events")
                )
                .rows,
                vec![vec![Value::Null(DataType::Int64)]],
                "empty {function} is NULL"
            );
            for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
                assert!(
                    query(
                        &mut database,
                        &format!(
                            "SELECT {function}(amount) AS value FROM events \
                             HAVING value {operator} 0"
                        )
                    )
                    .rows
                    .is_empty(),
                    "NULL {function} must make {operator} predicate false"
                );
            }
        }

        for function in ["SUM", "MIN", "MAX", "AVG"] {
            assert_eq!(
                query(
                    &mut database,
                    &format!("SELECT {function}(score) AS value FROM events")
                )
                .rows,
                vec![vec![Value::Null(DataType::Float64)]],
                "empty {function}(Float64) is NULL"
            );
            for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
                assert!(
                    query(
                        &mut database,
                        &format!(
                            "SELECT {function}(score) AS value FROM events \
                             HAVING value {operator} 0.0"
                        )
                    )
                    .rows
                    .is_empty(),
                    "NULL {function}(Float64) must make {operator} predicate false"
                );
            }
        }

        assert!(
            query(
                &mut database,
                "SELECT kind, AVG(score) AS mean FROM events \
                 GROUP BY kind HAVING mean > 0.0"
            )
            .rows
            .is_empty()
        );
    }

    #[test]
    fn having_preserves_group_working_limits_and_reduces_result_limits() {
        let setup = "CREATE TABLE events (kind String); \
            INSERT INTO events VALUES ('a'), ('a'), ('a'), ('b'), ('b'), ('c');";
        let sql = "SELECT kind, COUNT(*) AS n FROM events \
            GROUP BY kind HAVING n > 100";

        let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
            max_groups: 2,
            ..QueryResultLimits::default()
        });
        group_limited.execute(setup).expect("setup");
        assert_eq!(
            group_limited
                .execute(sql)
                .expect_err("HAVING cannot hide excess working groups"),
            Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 3,
                max: 2,
            }
        );

        let mut state_limited = Database::with_query_result_limits(QueryResultLimits {
            max_groups: 3,
            max_aggregate_state_cells: 2,
            ..QueryResultLimits::default()
        });
        state_limited.execute(setup).expect("setup");
        assert_eq!(
            state_limited
                .execute(sql)
                .expect_err("HAVING cannot hide excess aggregate state"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 3,
                max: 2,
            }
        );

        let mut result_limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 2,
            max_bytes: usize::MAX,
            ..QueryResultLimits::default()
        });
        result_limited.execute(setup).expect("setup");
        assert_eq!(
            query(
                &mut result_limited,
                "SELECT kind, COUNT(*) AS n FROM events \
                 GROUP BY kind HAVING n > 2"
            )
            .rows,
            vec![vec![Value::String("a".to_owned()), Value::Int64(3)]]
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
    fn int64_sum_uses_the_final_exact_sum_independent_of_row_order() {
        for values in [
            "(9223372036854775807), (1), (-1)",
            "(9223372036854775807), (-1), (1)",
        ] {
            let mut database = Database::new();
            database
                .execute(&format!(
                    "CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES {values};"
                ))
                .expect("setup");

            assert_eq!(
                query(&mut database, "SELECT SUM(n) AS total FROM numbers").rows,
                vec![vec![Value::Int64(i64::MAX)]]
            );
        }
    }

    #[test]
    fn float64_average_scales_finite_boundary_values_without_overflow() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE numbers (n Float64); \
                 INSERT INTO numbers VALUES \
                 (1.7976931348623157e308), (1.7976931348623157e308);",
            )
            .expect("setup");

        assert_eq!(
            query(&mut database, "SELECT AVG(n) AS mean FROM numbers").rows,
            vec![vec![Value::Float64(f64::MAX)]]
        );

        let mut cancelling = Database::new();
        cancelling
            .execute(
                "CREATE TABLE numbers (n Float64); \
                 INSERT INTO numbers VALUES \
                 (1.7976931348623157e308), (-1.7976931348623157e308);",
            )
            .expect("setup");
        assert_eq!(
            query(&mut cancelling, "SELECT AVG(n) AS mean FROM numbers").rows,
            vec![vec![Value::Float64(0.0)]]
        );
    }

    #[test]
    fn empty_global_aggregates_return_one_row_with_typed_nulls() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE samples (i Int64, f Float64, b Bool, s String);")
            .expect("create");
        let aggregate_sql = "SELECT COUNT(*) AS rows, SUM(i) AS int_sum, \
            SUM(f) AS float_sum, MIN(s) AS first, MAX(b) AS last, AVG(f) AS mean \
            FROM samples";
        let expected = vec![vec![
            Value::Int64(0),
            Value::Null(DataType::Int64),
            Value::Null(DataType::Float64),
            Value::Null(DataType::String),
            Value::Null(DataType::Bool),
            Value::Null(DataType::Float64),
        ]];

        assert_eq!(query(&mut database, aggregate_sql).rows, expected);

        database
            .execute("INSERT INTO samples VALUES (1, 2.0, true, 'present')")
            .expect("insert");
        assert_eq!(
            query(&mut database, &format!("{aggregate_sql} WHERE i < 0")).rows,
            expected
        );
    }

    #[test]
    fn grouped_aggregate_state_cell_limit_applies_before_limit() {
        let setup = "CREATE TABLE samples (g Int64, value Int64); \
            INSERT INTO samples VALUES (1, 10), (2, 20);";
        let sql = "SELECT g, MIN(value), MAX(value) FROM samples GROUP BY g LIMIT 1";

        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: usize::MAX,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact.execute(setup).expect("setup");
        assert_eq!(query(&mut exact, sql).rows.len(), 1);

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_cells: 3,
            ..exact_limits
        });
        limited.execute(setup).expect("setup");
        assert_eq!(
            limited
                .execute(sql)
                .expect_err("four working cells exceed the limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 4,
                max: 3,
            }
        );
    }

    #[test]
    fn grouped_aggregate_state_byte_limit_includes_string_extrema() {
        let setup = "CREATE TABLE samples (g Int64, value String); \
            INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');";
        let sql = "SELECT g, MIN(value), MAX(value) FROM samples GROUP BY g LIMIT 1";
        let fixed_bytes = 4 * std::mem::size_of::<AggregateState>()
            + 2 * std::mem::size_of::<Vec<AggregateState>>();

        let mut preallocation_limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: fixed_bytes - 1,
            ..QueryResultLimits::default()
        });
        preallocation_limited.execute(setup).expect("setup");
        assert_eq!(
            preallocation_limited
                .execute(sql)
                .expect_err("fixed working state exceeds the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes,
                max: fixed_bytes - 1,
            }
        );

        let exact_limits = QueryResultLimits {
            max_aggregate_state_cells: 4,
            max_aggregate_state_bytes: fixed_bytes + 16,
            ..QueryResultLimits::default()
        };
        let mut exact = Database::with_query_result_limits(exact_limits);
        exact.execute(setup).expect("setup");
        assert_eq!(query(&mut exact, sql).rows.len(), 1);

        let mut string_limited = Database::with_query_result_limits(QueryResultLimits {
            max_aggregate_state_bytes: fixed_bytes + 15,
            ..exact_limits
        });
        string_limited.execute(setup).expect("setup");
        assert_eq!(
            string_limited
                .execute(sql)
                .expect_err("cloned extrema strings exceed the byte limit"),
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state bytes",
                actual: fixed_bytes + 16,
                max: fixed_bytes + 15,
            }
        );
    }

    #[test]
    fn collecting_api_enforces_retained_result_limit() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE notes (s String); INSERT INTO notes VALUES ('abcdefghij');")
            .expect("setup");

        let error = database
            .execute_with_result_limit("SELECT s FROM notes", 1)
            .expect_err("result exceeds explicit retained byte limit");
        assert!(matches!(
            error,
            Error::ResultLimitExceeded {
                bytes,
                max_bytes: 1
            } if bytes > 1
        ));
    }

    #[test]
    fn select_materialization_limits_apply_before_owned_projection_rows() {
        let limits = QueryResultLimits {
            max_rows: 2,
            max_values: 4,
            max_bytes: usize::MAX,
            max_groups: 10,
            ..QueryResultLimits::default()
        };
        let mut database = Database::with_query_result_limits(limits);
        database
            .execute(
                "CREATE TABLE entries (id Int64, label String); \
                 INSERT INTO entries VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .expect("setup");

        assert_eq!(
            query(&mut database, "SELECT id, label FROM entries LIMIT 2")
                .rows
                .len(),
            2
        );
        let error = database
            .execute("SELECT id FROM entries")
            .expect_err("third projected row exceeds the row limit");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: 3,
                max: 2,
            }
        );

        let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 3,
            max_values: 5,
            ..limits
        });
        value_limited
            .execute(
                "CREATE TABLE entries (id Int64, label String); \
                 INSERT INTO entries VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .expect("setup");
        let error = value_limited
            .execute("SELECT id, label FROM entries")
            .expect_err("six projected values exceed the value limit");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: 6,
                max: 5,
            }
        );
    }

    #[test]
    fn select_byte_limit_counts_string_payload_before_cloning() {
        let mut database = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: 100,
            max_groups: 1,
            ..QueryResultLimits::default()
        });
        database
            .execute(&format!(
                "CREATE TABLE entries (label String); INSERT INTO entries VALUES ('{}');",
                "x".repeat(128)
            ))
            .expect("setup");

        let error = database
            .execute("SELECT label FROM entries")
            .expect_err("string payload exceeds byte limit");
        assert!(matches!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual,
                max: 100,
            } if actual > 100
        ));
    }

    #[test]
    fn row_number_conversion_is_one_based_and_checked() {
        assert_eq!(checked_row_number(0), Ok(1));
        assert_eq!(checked_row_number(41), Ok(42));

        if let Ok(max_i64) = usize::try_from(i64::MAX) {
            assert_eq!(
                checked_row_number(max_i64),
                Err(Error::NumericOverflow("ROW_NUMBER result".to_owned()))
            );
        }
    }
}
