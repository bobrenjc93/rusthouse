//! Bounded ordering over nullable typed columns.

use std::collections::BinaryHeap;
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
/// The input-row and requested-`LIMIT` bounds are both checked before any
/// result storage is allocated. The selection heap retains at most
/// `min(limit, values.len())` rows. A `limit` of zero returns no rows after
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

    let retained_rows = limit.min(values.len());
    let mut top_rows = BinaryHeap::with_capacity(retained_rows);

    for (source_index, &value) in values.iter().enumerate() {
        let candidate = OrderKey::new(value, source_index, direction, null_order);

        if top_rows.len() < retained_rows {
            top_rows.push(candidate);
        } else if let Some(mut worst) = top_rows.peek_mut() {
            if candidate < *worst {
                *worst = candidate;
            }
        }
    }

    Ok(top_rows
        .into_sorted_vec()
        .into_iter()
        .map(|row| row.source_index)
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    null_rank: u8,
    value_rank: u64,
    source_index: usize,
}

impl OrderKey {
    fn new(
        value: Option<i64>,
        source_index: usize,
        direction: OrderDirection,
        null_order: NullOrder,
    ) -> Self {
        let null_rank = match (value, null_order) {
            (None, NullOrder::First) | (Some(_), NullOrder::Last) => 0,
            (Some(_), NullOrder::First) | (None, NullOrder::Last) => 1,
        };
        let value_rank = value.map_or(0, |value| {
            // Flipping the sign bit maps signed order onto unsigned order.
            let ascending_rank = (value as u64) ^ (1_u64 << 63);
            match direction {
                OrderDirection::Asc => ascending_rank,
                OrderDirection::Desc => !ascending_rank,
            }
        });

        Self {
            null_rank,
            value_rank,
            source_index,
        }
    }
}
