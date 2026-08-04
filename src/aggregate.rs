//! Bounded aggregates over nullable typed columns.

use std::error::Error;
use std::fmt;

/// Rows included in an aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSelection<'a> {
    /// Include every input row in source order.
    All,
    /// Include the supplied source row indices.
    ///
    /// Indices must be unique and strictly increasing, as they are when
    /// returned by a scan.
    Indices(&'a [usize]),
}

impl RowSelection<'_> {
    fn row_count(self, input_rows: usize) -> usize {
        match self {
            Self::All => input_rows,
            Self::Indices(indices) => indices.len(),
        }
    }
}

/// Resource bounds applied to an aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateLimits {
    /// Maximum number of rows in the input column.
    pub max_input_rows: usize,
    /// Maximum number of rows included by the selection.
    pub max_selected_rows: usize,
}

impl AggregateLimits {
    /// Creates explicit input and selection bounds for an aggregate.
    pub const fn new(max_input_rows: usize, max_selected_rows: usize) -> Self {
        Self {
            max_input_rows,
            max_selected_rows,
        }
    }
}

/// The SQL counts computed for a nullable `Int64` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullableI64Counts {
    count_star: u64,
    count_column: u64,
}

impl NullableI64Counts {
    /// Returns `COUNT(*)`, including rows whose column value is `NULL`.
    pub const fn count_star(self) -> u64 {
        self.count_star
    }

    /// Returns `COUNT(column)`, excluding `NULL` values.
    pub const fn count_column(self) -> u64 {
        self.count_column
    }
}

/// The SQL aggregates computed for a nullable `Int64` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullableI64Aggregates {
    count_star: u64,
    count_column: u64,
    sum: Option<i64>,
}

impl NullableI64Aggregates {
    /// Returns `COUNT(*)`, including rows whose column value is `NULL`.
    pub const fn count_star(self) -> u64 {
        self.count_star
    }

    /// Returns `COUNT(column)`, excluding `NULL` values.
    pub const fn count_column(self) -> u64 {
        self.count_column
    }

    /// Returns `SUM(column)`, or `NULL` for an empty or all-`NULL` selection.
    pub const fn sum(self) -> Option<i64> {
        self.sum
    }
}

/// An invalid or unbounded aggregate operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateError {
    /// The column contains more rows than the configured input bound.
    InputLimitExceeded { rows: usize, max_rows: usize },
    /// The aggregate would inspect more selected rows than allowed.
    SelectionLimitExceeded { rows: usize, max_rows: usize },
    /// A selected row index does not exist in the input column.
    SelectionIndexOutOfBounds {
        selection_position: usize,
        row_index: usize,
        input_rows: usize,
    },
    /// Selected row indices are duplicated or not in source-row order.
    SelectionNotStrictlyIncreasing {
        selection_position: usize,
        previous_row_index: usize,
        row_index: usize,
    },
    /// The exact sum cannot be represented as an `i64`.
    SumOverflow { sum: i128 },
}

impl fmt::Display for AggregateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "aggregate input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::SelectionLimitExceeded { rows, max_rows } => write!(
                formatter,
                "aggregate selection has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::SelectionIndexOutOfBounds {
                selection_position,
                row_index,
                input_rows,
            } => write!(
                formatter,
                "aggregate selection index {row_index} at position {selection_position} is out of bounds for {input_rows} input rows"
            ),
            Self::SelectionNotStrictlyIncreasing {
                selection_position,
                previous_row_index,
                row_index,
            } => write!(
                formatter,
                "aggregate selection index {row_index} at position {selection_position} is not greater than the previous index {previous_row_index}"
            ),
            Self::SumOverflow { sum } => {
                write!(formatter, "aggregate sum {sum} is outside the Int64 range")
            }
        }
    }
}

impl Error for AggregateError {}

/// Computes `COUNT(*)` and `COUNT(column)` without evaluating other aggregates.
///
/// `None` values represent SQL `NULL`: they contribute to `COUNT(*)`, but not
/// to `COUNT(column)`. Bounds and explicit selections have the same validation
/// semantics as [`aggregate_nullable_i64`]. Values are never summed, so valid
/// counts cannot fail because an unrelated `SUM(column)` would overflow.
pub fn count_nullable_i64(
    values: &[Option<i64>],
    selection: RowSelection<'_>,
    limits: AggregateLimits,
) -> Result<NullableI64Counts, AggregateError> {
    let selected_rows = validate_aggregate_input(values.len(), selection, limits)?;
    let count_column = match selection {
        RowSelection::All => values.iter().filter(|value| value.is_some()).count(),
        RowSelection::Indices(indices) => indices
            .iter()
            .filter(|&&index| values[index].is_some())
            .count(),
    };

    Ok(NullableI64Counts {
        count_star: selected_rows as u64,
        count_column: count_column as u64,
    })
}

/// Computes `MIN(column)` for nullable `i64` values.
///
/// `None` values represent SQL `NULL` and do not participate in the minimum.
/// The result is `None` when the input or selection has no non-`NULL` values.
/// Bounds and explicit selections have the same validation semantics as
/// [`aggregate_nullable_i64`].
///
/// # Examples
///
/// ```
/// use rusthouse::{AggregateLimits, RowSelection, min_nullable_i64};
///
/// let values = [Some(4), None, Some(-2), Some(-2)];
/// let result = min_nullable_i64(
///     &values,
///     RowSelection::All,
///     AggregateLimits::new(values.len(), values.len()),
/// )?;
///
/// assert_eq!(result, Some(-2));
/// # Ok::<(), rusthouse::AggregateError>(())
/// ```
pub fn min_nullable_i64(
    values: &[Option<i64>],
    selection: RowSelection<'_>,
    limits: AggregateLimits,
) -> Result<Option<i64>, AggregateError> {
    validate_aggregate_input(values.len(), selection, limits)?;

    let minimum = match selection {
        RowSelection::All => values.iter().copied().flatten().min(),
        RowSelection::Indices(indices) => indices.iter().filter_map(|&index| values[index]).min(),
    };
    Ok(minimum)
}

/// Computes `COUNT(*)`, `COUNT(column)`, and `SUM(column)` for nullable `i64`
/// values.
///
/// `None` values represent SQL `NULL`: they contribute to `COUNT(*)`, but not
/// to `COUNT(column)` or `SUM(column)`. `SUM` returns `None` when no non-NULL
/// value is selected. An explicit selection must contain unique, strictly
/// increasing, in-bounds indices.
///
/// Both bounds and the entire explicit selection are validated before values
/// are aggregated. The sum is accumulated exactly in an `i128`, then rejected
/// with [`AggregateError::SumOverflow`] if its final value is outside the
/// `i64` range.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     AggregateLimits, RowSelection, aggregate_nullable_i64,
/// };
///
/// let values = [Some(4), None, Some(9), Some(4)];
/// let rows = [0, 3];
/// let result = aggregate_nullable_i64(
///     &values,
///     RowSelection::Indices(&rows),
///     AggregateLimits::new(values.len(), rows.len()),
/// )?;
///
/// assert_eq!(result.count_star(), 2);
/// assert_eq!(result.count_column(), 2);
/// assert_eq!(result.sum(), Some(8));
/// # Ok::<(), rusthouse::AggregateError>(())
/// ```
pub fn aggregate_nullable_i64(
    values: &[Option<i64>],
    selection: RowSelection<'_>,
    limits: AggregateLimits,
) -> Result<NullableI64Aggregates, AggregateError> {
    let selected_rows = validate_aggregate_input(values.len(), selection, limits)?;
    let count_star = selected_rows as u64;
    match selection {
        RowSelection::All => aggregate_values(values.iter().copied(), count_star),
        RowSelection::Indices(indices) => {
            aggregate_values(indices.iter().map(|&index| values[index]), count_star)
        }
    }
}

fn validate_aggregate_input(
    input_rows: usize,
    selection: RowSelection<'_>,
    limits: AggregateLimits,
) -> Result<usize, AggregateError> {
    if input_rows > limits.max_input_rows {
        return Err(AggregateError::InputLimitExceeded {
            rows: input_rows,
            max_rows: limits.max_input_rows,
        });
    }

    let selected_rows = selection.row_count(input_rows);
    if selected_rows > limits.max_selected_rows {
        return Err(AggregateError::SelectionLimitExceeded {
            rows: selected_rows,
            max_rows: limits.max_selected_rows,
        });
    }

    if let RowSelection::Indices(indices) = selection {
        validate_selection(indices, input_rows)?;
    }

    Ok(selected_rows)
}

fn validate_selection(indices: &[usize], input_rows: usize) -> Result<(), AggregateError> {
    let mut previous = None;

    for (selection_position, &row_index) in indices.iter().enumerate() {
        if row_index >= input_rows {
            return Err(AggregateError::SelectionIndexOutOfBounds {
                selection_position,
                row_index,
                input_rows,
            });
        }

        if let Some(previous_row_index) = previous {
            if row_index <= previous_row_index {
                return Err(AggregateError::SelectionNotStrictlyIncreasing {
                    selection_position,
                    previous_row_index,
                    row_index,
                });
            }
        }

        previous = Some(row_index);
    }

    Ok(())
}

fn aggregate_values(
    values: impl Iterator<Item = Option<i64>>,
    count_star: u64,
) -> Result<NullableI64Aggregates, AggregateError> {
    let mut count_column = 0_u64;
    let mut sum = 0_i128;

    for value in values.flatten() {
        count_column += 1;
        sum += i128::from(value);
    }

    let sum = if count_column == 0 {
        None
    } else {
        Some(i64::try_from(sum).map_err(|_| AggregateError::SumOverflow { sum })?)
    };

    Ok(NullableI64Aggregates {
        count_star,
        count_column,
        sum,
    })
}
