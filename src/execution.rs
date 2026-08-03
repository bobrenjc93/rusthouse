//! Execution boundaries connecting parsed SQL to storage.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use crate::{
    ComparisonOperator, GroupedCountError, GroupedCountLimits, GroupedCountStatement, InsertError,
    InsertStatement, Int64Table, NullableI64GroupedCount, ScanError, ScanLimits, SelectStatement,
    grouped_count_nullable_i64, scan_nullable_i64,
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

/// An error produced while executing a parsed projection or grouped `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExecutionError {
    /// The statement names a table other than the one supplied for execution.
    UnknownTable { name: String },
    /// The statement names a column other than the table's only column.
    UnknownColumn { name: String },
    /// The bounded equality scan rejected the input or result size.
    Scan(ScanError),
    /// The bounded grouped-count operator rejected the input or result size.
    GroupedCount(GroupedCountError),
}

impl fmt::Display for SelectExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTable { name } => write!(formatter, "unknown table '{name}'"),
            Self::UnknownColumn { name } => write!(formatter, "unknown column '{name}'"),
            Self::Scan(error) => write!(formatter, "could not scan rows: {error}"),
            Self::GroupedCount(error) => write!(formatter, "could not group rows: {error}"),
        }
    }
}

impl Error for SelectExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::GroupedCount(error) => Some(error),
            Self::UnknownTable { .. } | Self::UnknownColumn { .. } => None,
        }
    }
}

impl From<ScanError> for SelectExecutionError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

impl From<GroupedCountError> for SelectExecutionError {
    fn from(error: GroupedCountError) -> Self {
        Self::GroupedCount(error)
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

/// Executes one parsed grouped `COUNT(*)` with explicit resource bounds.
///
/// The expected table name, selected column, and `GROUP BY` column are compared
/// exactly, including ASCII case. On a match, execution delegates to
/// [`grouped_count_nullable_i64`], preserving its deterministic `NULL`-first
/// ordering and its input-row and distinct-group limit errors.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     GroupedCountLimits, Int64Table, ParseLimits, Schema, execute_grouped_count,
///     parse_grouped_count,
/// };
///
/// let statement = parse_grouped_count(
///     "SELECT value, COUNT(*) FROM readings GROUP BY value",
///     ParseLimits::default(),
/// )?;
/// let mut table = Int64Table::new(Schema::int64("value", true), 3);
/// table.append_batch(&[Some(7), None, Some(7)])?;
///
/// let groups = execute_grouped_count(
///     "readings",
///     &table,
///     &statement,
///     GroupedCountLimits::new(3, 2),
/// )?;
/// let pairs: Vec<_> = groups.into_iter().map(|group| group.into_pair()).collect();
/// assert_eq!(pairs, vec![(None, 1), (Some(7), 2)]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_grouped_count(
    expected_table_name: &str,
    table: &Int64Table,
    statement: &GroupedCountStatement,
    limits: GroupedCountLimits,
) -> Result<Vec<NullableI64GroupedCount>, SelectExecutionError> {
    if statement.table_name().as_str() != expected_table_name {
        return Err(SelectExecutionError::UnknownTable {
            name: statement.table_name().as_str().to_owned(),
        });
    }

    let stored_column_name = table.schema().column().name();
    for column_name in [statement.column_name(), statement.group_by_column_name()] {
        if column_name.as_str() != stored_column_name {
            return Err(SelectExecutionError::UnknownColumn {
                name: column_name.as_str().to_owned(),
            });
        }
    }

    grouped_count_nullable_i64(table.values(), limits).map_err(Into::into)
}

/// Executes one parsed `SELECT` against one explicitly named table.
///
/// `expected_table_name` and the table's column name are compared exactly
/// with the identifiers retained by the parser, including ASCII case. The
/// An unfiltered result borrows the table's existing column storage without
/// copying its values. A `WHERE` equality predicate uses a scan bounded to the
/// table's current row count and owns the selected values. Use
/// [`execute_select_with_limits`] when the caller needs stricter scan bounds.
/// An optional `LIMIT` is applied after filtering.
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

/// Executes one parsed `SELECT` with explicit equality-scan resource bounds.
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

    let Some(predicate) = statement.predicate() else {
        return Ok(Cow::Borrowed(&values[..limit.min(values.len())]));
    };

    if predicate.column_name().as_str() != table.schema().column().name() {
        return Err(SelectExecutionError::UnknownColumn {
            name: predicate.column_name().as_str().to_owned(),
        });
    }

    let matching_rows = scan_nullable_i64(
        values,
        ComparisonOperator::Eq,
        predicate.value(),
        scan_limits,
    )?;
    let selected = matching_rows
        .into_iter()
        .take(limit)
        .map(|row_index| values[row_index])
        .collect();

    Ok(Cow::Owned(selected))
}
