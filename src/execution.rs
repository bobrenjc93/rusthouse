//! Execution boundaries connecting parsed SQL to storage.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use crate::{
    AggregateError, AggregateLimits, DistinctError, DistinctLimits, InnerJoinStatement,
    InsertError, InsertStatement, Int64Table, JoinError, JoinLimits, OrderError, OrderLimits,
    RowSelection, ScalarCountStatement, ScalarMinStatement, ScalarSumStatement, ScanError,
    ScanLimits, SelectDistinctStatement, SelectPredicate, SelectStatement, aggregate_nullable_i64,
    count_nullable_i64, distinct_nullable_i64, inner_equi_join_nullable_i64, min_nullable_i64,
    order_nullable_i64, scan_nullable_i64, scan_nullable_i64_nullness,
};

/// An error produced while executing a parsed [`InsertStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The destination table rejected the value.
    Insert(InsertError),
}

impl fmt::Display for InsertExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::Insert(error) => write!(formatter, "could not insert row: {error}"),
        }
    }
}

impl Error for InsertExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Insert(error) => Some(error),
            Self::UnknownTable { .. } => None,
        }
    }
}

impl From<InsertError> for InsertExecutionError {
    fn from(error: InsertError) -> Self {
        Self::Insert(error)
    }
}

/// An error produced while executing a parsed projection or scalar `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the table's only column.
    UnknownColumn { name: String },
    /// The bounded predicate scan rejected the input or result size.
    Scan(ScanError),
    /// The bounded top-k order operation rejected the input or requested limit.
    Order(OrderError),
    /// The bounded aggregate operation rejected the input or selection size.
    Aggregate(AggregateError),
}

impl fmt::Display for SelectExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
            Self::Scan(error) => write!(formatter, "could not scan rows: {error}"),
            Self::Order(error) => write!(formatter, "could not order rows: {error}"),
            Self::Aggregate(error) => write!(formatter, "could not aggregate rows: {error}"),
        }
    }
}

impl Error for SelectExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Order(error) => Some(error),
            Self::Aggregate(error) => Some(error),
            Self::UnknownTable { .. } | Self::UnknownColumn { .. } => None,
        }
    }
}

impl From<ScanError> for SelectExecutionError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

impl From<OrderError> for SelectExecutionError {
    fn from(error: OrderError) -> Self {
        Self::Order(error)
    }
}

impl From<AggregateError> for SelectExecutionError {
    fn from(error: AggregateError) -> Self {
        Self::Aggregate(error)
    }
}

/// An error produced while executing a parsed [`SelectDistinctStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectDistinctExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the table's only column.
    UnknownColumn { name: String },
    /// The bounded distinct operator rejected the input or result size.
    Distinct(DistinctError),
}

impl fmt::Display for SelectDistinctExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
            Self::Distinct(error) => {
                write!(formatter, "could not compute distinct values: {error}")
            }
        }
    }
}

impl Error for SelectDistinctExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Distinct(error) => Some(error),
            Self::UnknownTable { .. } | Self::UnknownColumn { .. } => None,
        }
    }
}

impl From<DistinctError> for SelectDistinctExecutionError {
    fn from(error: DistinctError) -> Self {
        Self::Distinct(error)
    }
}

/// An error produced while executing a parsed [`InnerJoinStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerJoinExecutionError {
    /// The statement names a table other than either table supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the relevant table's only column.
    UnknownColumn { name: String },
    /// The bounded join operator rejected an input or output size.
    Join(JoinError),
}

impl fmt::Display for InnerJoinExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
            Self::Join(error) => write!(formatter, "could not join rows: {error}"),
        }
    }
}

impl Error for InnerJoinExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Join(error) => Some(error),
            Self::UnknownTable { .. } | Self::UnknownColumn { .. } => None,
        }
    }
}

impl From<JoinError> for InnerJoinExecutionError {
    fn from(error: JoinError) -> Self {
        Self::Join(error)
    }
}

/// Executes the narrow inner equi-join against two named one-column tables.
///
/// All identifiers are compared exactly, including ASCII case. On successful
/// resolution, [`inner_equi_join_nullable_i64`] supplies duplicate
/// cross-products, SQL `NULL` non-matching semantics, deterministic left-major
/// order, and explicit input/output bounds. The returned values are projected
/// from the left table in that match order.
pub fn execute_inner_join(
    expected_left_table_name: &str,
    left_table: &Int64Table,
    expected_right_table_name: &str,
    right_table: &Int64Table,
    statement: &InnerJoinStatement,
    limits: JoinLimits,
) -> Result<Vec<Option<i64>>, InnerJoinExecutionError> {
    if statement.left_table_name().as_str() != expected_left_table_name {
        return Err(InnerJoinExecutionError::UnknownTable {
            name: statement.left_table_name().as_str().to_owned(),
        });
    }
    if statement.right_table_name().as_str() != expected_right_table_name {
        return Err(InnerJoinExecutionError::UnknownTable {
            name: statement.right_table_name().as_str().to_owned(),
        });
    }

    let left_column = left_table.schema().column().name();
    for column_name in [
        statement.projected_column_name(),
        statement.left_column_name(),
    ] {
        if column_name.as_str() != left_column {
            return Err(InnerJoinExecutionError::UnknownColumn {
                name: column_name.as_str().to_owned(),
            });
        }
    }
    if statement.right_column_name().as_str() != right_table.schema().column().name() {
        return Err(InnerJoinExecutionError::UnknownColumn {
            name: statement.right_column_name().as_str().to_owned(),
        });
    }

    let matches = inner_equi_join_nullable_i64(left_table.values(), right_table.values(), limits)?;
    Ok(matches
        .into_iter()
        .map(|pair| left_table.values()[pair.left_row()])
        .collect())
}

/// Executes one parsed `INSERT` against one explicitly named table.
///
/// `expected_table_name` is compared exactly with the identifier retained by
/// the parser, including ASCII case. A mismatch is reported as an unknown
/// table without consulting or mutating `table`. On a match, storage
/// nullability and row-cap errors are preserved in [`InsertExecutionError`]
/// and also leave the table unchanged.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     Int64Table, ParseLimits, Schema, execute_insert, parse_insert,
/// };
///
/// let statement = parse_insert(
///     "INSERT INTO readings VALUES (7)",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", false), 1);
///
/// execute_insert("readings", &mut table, &statement)?;
/// assert_eq!(table.values(), &[Some(7)]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_insert(
    expected_table_name: &str,
    table: &mut Int64Table,
    statement: &InsertStatement,
) -> Result<(), InsertExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(InsertExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    table.append(statement.value()).map_err(Into::into)
}

/// Executes one parsed scalar `COUNT` with explicit aggregate bounds.
///
/// The expected table name and optional column name are compared exactly with
/// the identifiers retained by the parser, including ASCII case. Predicate
/// scans use bounds sized to the table. Use [`execute_scalar_count_with_limits`]
/// to supply explicit scan bounds as well.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     AggregateLimits, Int64Table, ParseLimits, Schema, execute_scalar_count,
///     parse_scalar_count,
/// };
///
/// let statement = parse_scalar_count(
///     "SELECT COUNT(value) FROM readings",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 2);
/// table.append_batch(&[Some(7), None])?;
///
/// let count = execute_scalar_count(
///     "readings",
///     &table,
///     &statement,
///     AggregateLimits::new(2, 2),
/// )?;
/// assert_eq!(count, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_scalar_count(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &ScalarCountStatement,
    limits: AggregateLimits,
) -> Result<u64, SelectExecutionError> {
    let rows = table.values().len();
    execute_scalar_count_with_limits(
        expected_table_name,
        table,
        statement,
        ScanLimits::new(rows, rows),
        limits,
    )
}

/// Executes one parsed scalar `COUNT` with explicit scan and aggregate bounds.
///
/// A `WHERE` comparison first delegates to [`scan_nullable_i64`], which
/// excludes `NULL` according to SQL comparison semantics. The returned source
/// rows are then passed to [`count_nullable_i64`] as an explicit selection.
/// Unfiltered counts aggregate all rows without consuming scan bounds.
pub fn execute_scalar_count_with_limits(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &ScalarCountStatement,
    scan_limits: ScanLimits,
    aggregate_limits: AggregateLimits,
) -> Result<u64, SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if let Some(column_name) = statement.column_name() {
        if column_name.as_str() != table.schema().column().name() {
            return Err(SelectExecutionError::UnknownColumn {
                name: column_name.as_str().to_owned(),
            });
        }
    }

    if let Some(predicate) = statement.predicate() {
        if predicate.column_name().as_str() != table.schema().column().name() {
            return Err(SelectExecutionError::UnknownColumn {
                name: predicate.column_name().as_str().to_owned(),
            });
        }
    }

    let matching_rows = statement
        .predicate()
        .map(|predicate| {
            scan_nullable_i64(
                table.values(),
                predicate.operator(),
                predicate.value(),
                scan_limits,
            )
        })
        .transpose()?;
    let selection = matching_rows
        .as_deref()
        .map_or(RowSelection::All, RowSelection::Indices);
    let counts = count_nullable_i64(table.values(), selection, aggregate_limits)?;
    Ok(if statement.column_name().is_some() {
        counts.count_column()
    } else {
        counts.count_star()
    })
}

/// Executes one parsed scalar `SUM` with explicit resource bounds.
///
/// The expected table and column names are compared exactly with the
/// identifiers retained by the parser, including ASCII case. On a match,
/// execution delegates to [`aggregate_nullable_i64`], returning `None` for
/// SQL `NULL` when the input is empty or all `NULL`. Aggregate bounds and sum
/// overflow are preserved in [`SelectExecutionError`].
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     AggregateLimits, Int64Table, ParseLimits, Schema, execute_scalar_sum,
///     parse_scalar_sum,
/// };
///
/// let statement = parse_scalar_sum(
///     "SELECT SUM(value) FROM readings",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 3);
/// table.append_batch(&[Some(7), None, Some(-2)])?;
///
/// let sum = execute_scalar_sum(
///     "readings",
///     &table,
///     &statement,
///     AggregateLimits::new(3, 3),
/// )?;
/// assert_eq!(sum, Some(5));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_scalar_sum(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &ScalarSumStatement,
    aggregate_limits: AggregateLimits,
) -> Result<Option<i64>, SelectExecutionError> {
    let rows = table.values().len();
    execute_scalar_sum_with_limits(
        expected_table_name,
        table,
        statement,
        ScanLimits::new(rows, rows),
        aggregate_limits,
    )
}

/// Executes one parsed scalar `SUM` with explicit scan and aggregate bounds.
///
/// A comparison predicate is evaluated by [`scan_nullable_i64`] before the
/// aggregate receives the matching source-row indices. Unfiltered statements
/// do not scan and aggregate every row directly.
pub fn execute_scalar_sum_with_limits(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &ScalarSumStatement,
    scan_limits: ScanLimits,
    aggregate_limits: AggregateLimits,
) -> Result<Option<i64>, SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if statement.column_name().as_str() != table.schema().column().name() {
        return Err(SelectExecutionError::UnknownColumn {
            name: statement.column_name().as_str().to_owned(),
        });
    }

    if let Some(predicate) = statement.predicate() {
        if predicate.column_name().as_str() != table.schema().column().name() {
            return Err(SelectExecutionError::UnknownColumn {
                name: predicate.column_name().as_str().to_owned(),
            });
        }
    }

    let values = table.values();
    let matching_rows = statement
        .predicate()
        .map(|predicate| {
            scan_nullable_i64(values, predicate.operator(), predicate.value(), scan_limits)
        })
        .transpose()?;
    let selection = matching_rows
        .as_deref()
        .map_or(RowSelection::All, RowSelection::Indices);
    let aggregates = aggregate_nullable_i64(values, selection, aggregate_limits)?;
    Ok(aggregates.sum())
}

/// Executes one parsed scalar `MIN` with explicit resource bounds.
///
/// The expected table and column names are compared exactly with the
/// identifiers retained by the parser, including ASCII case. On a match,
/// execution delegates to [`min_nullable_i64`], returning `None` for SQL
/// `NULL` when the input is empty or all `NULL`.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     AggregateLimits, Int64Table, ParseLimits, Schema, execute_scalar_min,
///     parse_scalar_min,
/// };
///
/// let statement = parse_scalar_min(
///     "SELECT MIN(value) FROM readings",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 3);
/// table.append_batch(&[Some(7), None, Some(-2)])?;
///
/// let minimum = execute_scalar_min(
///     "readings",
///     &table,
///     &statement,
///     AggregateLimits::new(3, 3),
/// )?;
/// assert_eq!(minimum, Some(-2));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_scalar_min(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &ScalarMinStatement,
    aggregate_limits: AggregateLimits,
) -> Result<Option<i64>, SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if statement.column_name().as_str() != table.schema().column().name() {
        return Err(SelectExecutionError::UnknownColumn {
            name: statement.column_name().as_str().to_owned(),
        });
    }

    min_nullable_i64(table.values(), RowSelection::All, aggregate_limits).map_err(Into::into)
}

/// Executes one parsed `SELECT` against one explicitly named table.
///
/// `expected_table_name` and the table's column name are compared exactly
/// with the identifiers retained by the parser, including ASCII case. An
/// unfiltered, unordered result borrows the table's existing column storage.
/// A `WHERE` predicate uses its corresponding bounded scan, while
/// `ORDER BY ... LIMIT` uses the bounded top-k operator and owns the ordered values. Use
/// [`execute_select_with_order_limits`] for explicit bounds on both operators.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     Int64Table, ParseLimits, Schema, execute_select, parse_select,
/// };
///
/// let statement = parse_select(
///     "SELECT value FROM readings",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 2);
/// table.append_batch(&[Some(7), None])?;
///
/// let values = execute_select("readings", &table, &statement)?;
/// assert_eq!(values.as_ref(), &[Some(7), None]);
/// assert!(matches!(values, std::borrow::Cow::Borrowed(_)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_select<'table>(
    expected_table_name: &str,
    table: &'table Int64Table,
    statement: &SelectStatement,
) -> Result<Cow<'table, [Option<i64>]>, SelectExecutionError> {
    let rows = table.values().len();
    execute_select_with_limits(
        expected_table_name,
        table,
        statement,
        ScanLimits::new(rows, rows),
    )
}

/// Executes one parsed `SELECT` with explicit predicate-scan resource bounds.
///
/// Plain projections do not scan and therefore do not consume these bounds.
/// Filtered projections preserve source-row order and delegate predicate
/// evaluation to the corresponding bounded comparison or nullness scan.
pub fn execute_select_with_limits<'table>(
    expected_table_name: &str,
    table: &'table Int64Table,
    statement: &SelectStatement,
    scan_limits: ScanLimits,
) -> Result<Cow<'table, [Option<i64>]>, SelectExecutionError> {
    let rows = table.values().len();
    let requested_limit = statement.limit().unwrap_or(rows);
    execute_select_with_order_limits(
        expected_table_name,
        table,
        statement,
        scan_limits,
        OrderLimits::new(rows, requested_limit),
    )
}

/// Executes one parsed `SELECT` with explicit scan and top-k order bounds.
///
/// Ordered projections are materialized in the source-index order returned by
/// [`order_nullable_i64`]. Unordered projections retain the borrowing behavior
/// of [`execute_select_with_limits`].
pub fn execute_select_with_order_limits<'table>(
    expected_table_name: &str,
    table: &'table Int64Table,
    statement: &SelectStatement,
    scan_limits: ScanLimits,
    order_limits: OrderLimits,
) -> Result<Cow<'table, [Option<i64>]>, SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if statement.column_name().as_str() != table.schema().column().name() {
        return Err(SelectExecutionError::UnknownColumn {
            name: statement.column_name().as_str().to_owned(),
        });
    }

    let values = table.values();
    let limit = statement.limit().unwrap_or(values.len());

    if let Some(predicate) = statement.where_predicate() {
        if predicate.column_name().as_str() != table.schema().column().name() {
            return Err(SelectExecutionError::UnknownColumn {
                name: predicate.column_name().as_str().to_owned(),
            });
        }
    }

    if let Some(order_by) = statement.order_by() {
        if order_by.column_name().as_str() != table.schema().column().name() {
            return Err(SelectExecutionError::UnknownColumn {
                name: order_by.column_name().as_str().to_owned(),
            });
        }

        let order_input = match statement.where_predicate() {
            Some(predicate) => {
                let matching_rows = scan_select_predicate(values, predicate, scan_limits)?;
                Cow::Owned(
                    matching_rows
                        .into_iter()
                        .map(|row_index| values[row_index])
                        .collect::<Vec<_>>(),
                )
            }
            None => Cow::Borrowed(values),
        };
        let ordered_rows = order_nullable_i64(
            order_input.as_ref(),
            order_by.direction(),
            order_by.null_order(),
            limit,
            order_limits,
        )?;
        let selected = ordered_rows
            .into_iter()
            .map(|row_index| order_input[row_index])
            .collect();

        return Ok(Cow::Owned(selected));
    }

    let Some(predicate) = statement.where_predicate() else {
        return Ok(Cow::Borrowed(&values[..limit.min(values.len())]));
    };

    let matching_rows = scan_select_predicate(values, predicate, scan_limits)?;
    let selected = matching_rows
        .into_iter()
        .take(limit)
        .map(|row_index| values[row_index])
        .collect();

    Ok(Cow::Owned(selected))
}

fn scan_select_predicate(
    values: &[Option<i64>],
    predicate: &SelectPredicate,
    limits: ScanLimits,
) -> Result<Vec<usize>, ScanError> {
    match predicate {
        SelectPredicate::Comparison(predicate) => {
            scan_nullable_i64(values, predicate.operator(), predicate.value(), limits)
        }
        SelectPredicate::Nullness(predicate) => {
            scan_nullable_i64_nullness(values, predicate.predicate(), limits)
        }
    }
}

/// Executes one parsed `SELECT DISTINCT` with explicit resource bounds.
///
/// Names are compared exactly, including ASCII case. On valid identifiers,
/// execution delegates to [`distinct_nullable_i64`], preserving its
/// deterministic `NULL`-first ascending output and its separate input-row and
/// distinct-value limits.
pub fn execute_select_distinct(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &SelectDistinctStatement,
    limits: DistinctLimits,
) -> Result<Vec<Option<i64>>, SelectDistinctExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectDistinctExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    if statement.column_name().as_str() != table.schema().column().name() {
        return Err(SelectDistinctExecutionError::UnknownColumn {
            name: statement.column_name().as_str().to_owned(),
        });
    }

    distinct_nullable_i64(table.values(), limits).map_err(Into::into)
}
