//! Bounded ordering over nullable typed columns.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// The direction used to order non-`NULL` integer values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    /// Order values from smallest to largest.
    Asc,
    /// Order values from largest to smallest.
    Desc,
}

/// The explicit placement of `NULL` values in an ordered result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    /// Place `NULL` values before every non-`NULL` value.
    First,
    /// Place `NULL` values after every non-`NULL` value.
    Last,
}

/// Resource bounds applied to an order operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderLimits {
    /// Maximum number of input rows that may be ordered.
    pub max_input_rows: usize,
    /// Maximum requested `LIMIT` value.
    pub max_limit: usize,
}

impl OrderLimits {
    /// Creates explicit input-row and requested-`LIMIT` bounds.
    pub const fn new(max_input_rows: usize, max_limit: usize) -> Self {
        Self {
            max_input_rows,
            max_limit,
        }
    }
}

/// A resource limit rejected by an order operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderError {
    /// The column contains more rows than the configured input bound.
    InputLimitExceeded { rows: usize, max_rows: usize },
    /// The requested `LIMIT` is greater than its configured bound.
    LimitExceeded { limit: usize, max_limit: usize },
}

impl fmt::Display for OrderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "order input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::LimitExceeded { limit, max_limit } => write!(
                formatter,
                "requested LIMIT {limit} exceeds the limit of {max_limit}"
            ),
        }
    }
}

impl Error for OrderError {}

/// Returns source row indices ordered by nullable `i64` values and truncated
/// to `limit` rows.
///
/// `None` values represent SQL `NULL`. Their placement is controlled explicitly
/// by [`NullOrder`] and is independent of [`OrderDirection`]. Rows with equal
/// values, including `NULL` rows, retain their source order.
///
/// The input-row and requested-`LIMIT` bounds are both checked before the
/// result index vector is allocated. A `limit` of zero returns no rows after
/// those validations have succeeded.
///
/// # Examples
///
/// ```
/// use rusthouse::{
///     NullOrder, OrderDirection, OrderLimits, order_nullable_i64,
/// };
///
/// let values = [Some(4), None, Some(9), Some(4)];
/// let rows = order_nullable_i64(
///     &values,
///     OrderDirection::Desc,
///     NullOrder::Last,
///     3,
///     OrderLimits::new(values.len(), 3),
/// )?;
///
/// assert_eq!(rows, vec![2, 0, 3]);
/// # Ok::<(), rusthouse::OrderError>(())
/// ```
pub fn order_nullable_i64(
    values: &[Option<i64>],
    direction: OrderDirection,
    null_order: NullOrder,
    limit: usize,
    limits: OrderLimits,
) -> Result<Vec<usize>, OrderError> {
    if values.len() > limits.max_input_rows {
        return Err(OrderError::InputLimitExceeded {
            rows: values.len(),
            max_rows: limits.max_input_rows,
        });
    }

    if limit > limits.max_limit {
        return Err(OrderError::LimitExceeded {
            limit,
            max_limit: limits.max_limit,
        });
    }

    if limit == 0 || values.is_empty() {
        return Ok(Vec::new());
    }

    let mut row_indices: Vec<_> = (0..values.len()).collect();
    row_indices.sort_unstable_by(|&left, &right| {
        compare_values(values[left], values[right], direction, null_order)
            .then_with(|| left.cmp(&right))
    });
    row_indices.truncate(limit);

    Ok(row_indices)
}

fn compare_values(
    left: Option<i64>,
    right: Option<i64>,
    direction: OrderDirection,
    null_order: NullOrder,
) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => match null_order {
            NullOrder::First => Ordering::Less,
            NullOrder::Last => Ordering::Greater,
        },
        (Some(_), None) => match null_order {
            NullOrder::First => Ordering::Greater,
            NullOrder::Last => Ordering::Less,
        },
        (Some(left), Some(right)) => match direction {
            OrderDirection::Asc => left.cmp(&right),
            OrderDirection::Desc => right.cmp(&left),
        },
    }
}
