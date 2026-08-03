//! Execution boundaries connecting parsed SQL to storage.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use crate::{
    InsertError, InsertStatement, Int64Table, OrderError, OrderLimits, ScanError, ScanLimits,
    SelectStatement, order_nullable_i64, scan_nullable_i64,
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

/// An error produced while executing a parsed [`SelectStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the table's only column.
    UnknownColumn { name: String },
    /// The bounded comparison scan rejected the input or result size.
    Scan(ScanError),
    /// The bounded top-k order operation rejected the input or requested limit.
    Order(OrderError),
}

impl fmt::Display for SelectExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
            Self::Scan(error) => write!(formatter, "could not scan rows: {error}"),
            Self::Order(error) => write!(formatter, "could not order rows: {error}"),
        }
    }
}

impl Error for SelectExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Order(error) => Some(error),
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

/// Executes one parsed `SELECT` against one explicitly named table.
///
/// `expected_table_name` and the table's column name are compared exactly
/// with the identifiers retained by the parser, including ASCII case. An
/// unfiltered, unordered result borrows the table's existing column storage.
/// A `WHERE` comparison predicate uses a bounded scan, while `ORDER BY ... LIMIT`
/// uses the bounded top-k operator and owns the ordered values. Use
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

/// Executes one parsed `SELECT` with explicit comparison-scan resource bounds.
///
/// Plain projections do not scan and therefore do not consume these bounds.
/// Filtered projections preserve source-row order and exclude `NULL` because
/// they delegate predicate evaluation to [`scan_nullable_i64`].
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

    if let Some(predicate) = statement.predicate() {
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

        let order_input = match statement.predicate() {
            Some(predicate) => {
                let matching_rows = scan_nullable_i64(
                    values,
                    predicate.operator(),
                    predicate.value(),
                    scan_limits,
                )?;
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

    let Some(predicate) = statement.predicate() else {
        return Ok(Cow::Borrowed(&values[..limit.min(values.len())]));
    };

    let matching_rows =
        scan_nullable_i64(values, predicate.operator(), predicate.value(), scan_limits)?;
    let selected = matching_rows
        .into_iter()
        .take(limit)
        .map(|row_index| values[row_index])
        .collect();

    Ok(Cow::Owned(selected))
}
