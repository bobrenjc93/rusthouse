//! Bounded distinct values over nullable typed columns.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

/// Resource bounds applied to a distinct operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistinctLimits {
    /// Maximum number of rows in the input column.
    pub max_input_rows: usize,
    /// Maximum number of distinct values in the result.
    pub max_distinct_values: usize,
}

impl DistinctLimits {
    /// Creates explicit input-row and distinct-value bounds.
    pub const fn new(max_input_rows: usize, max_distinct_values: usize) -> Self {
        Self {
            max_input_rows,
            max_distinct_values,
        }
    }
}

/// A resource limit rejected by a distinct operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistinctError {
    /// The column contains more rows than the configured input bound.
    InputLimitExceeded { rows: usize, max_rows: usize },
    /// The input contains more distinct values than the configured bound.
    DistinctValueLimitExceeded { values: usize, max_values: usize },
}

impl fmt::Display for DistinctError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "distinct input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::DistinctValueLimitExceeded { values, max_values } => write!(
                formatter,
                "distinct result has at least {values} values, exceeding the limit of {max_values}"
            ),
        }
    }
}

impl Error for DistinctError {}

/// Returns each distinct nullable `i64` value exactly once.
///
/// `None` represents SQL `NULL`. Results have deterministic order: one `NULL`
/// first when present, followed by non-`NULL` values in ascending signed
/// integer order. Empty input produces an empty result.
///
/// The input-row bound is checked before any values are inspected. The
/// distinct-value bound is inclusive; discovering one more value returns
/// [`DistinctError::DistinctValueLimitExceeded`] without returning a partial
/// result.
///
/// # Examples
///
/// ```
/// use rusthouse::{DistinctLimits, distinct_nullable_i64};
///
/// let values = [Some(4), None, Some(9), Some(4), None];
/// let distinct = distinct_nullable_i64(
///     &values,
///     DistinctLimits::new(values.len(), 3),
/// )?;
///
/// assert_eq!(distinct, vec![None, Some(4), Some(9)]);
/// # Ok::<(), rusthouse::DistinctError>(())
/// ```
pub fn distinct_nullable_i64(
    values: &[Option<i64>],
    limits: DistinctLimits,
) -> Result<Vec<Option<i64>>, DistinctError> {
    if values.len() > limits.max_input_rows {
        return Err(DistinctError::InputLimitExceeded {
            rows: values.len(),
            max_rows: limits.max_input_rows,
        });
    }

    let mut distinct = BTreeSet::new();

    for &value in values {
        if distinct.contains(&value) {
            continue;
        }

        if distinct.len() == limits.max_distinct_values {
            return Err(DistinctError::DistinctValueLimitExceeded {
                values: distinct.len().saturating_add(1),
                max_values: limits.max_distinct_values,
            });
        }

        distinct.insert(value);
    }

    Ok(distinct.into_iter().collect())
}
