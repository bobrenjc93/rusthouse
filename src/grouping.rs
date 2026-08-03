//! Bounded grouping over nullable typed columns.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Resource bounds applied to a grouped count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupedCountLimits {
    /// Maximum number of rows in the input column.
    pub max_input_rows: usize,
    /// Maximum number of distinct keys in the grouped result.
    pub max_distinct_groups: usize,
}

impl GroupedCountLimits {
    /// Creates explicit input-row and distinct-group bounds.
    pub const fn new(max_input_rows: usize, max_distinct_groups: usize) -> Self {
        Self {
            max_input_rows,
            max_distinct_groups,
        }
    }
}

/// One nullable `Int64` key and its `COUNT(*)` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullableI64GroupedCount {
    key: Option<i64>,
    count: u64,
}

impl NullableI64GroupedCount {
    /// Returns the group key. `None` represents the SQL `NULL` group.
    pub const fn key(self) -> Option<i64> {
        self.key
    }

    /// Returns the number of input rows in this group.
    pub const fn count(self) -> u64 {
        self.count
    }

    /// Returns the group as a key/count pair.
    pub const fn into_pair(self) -> (Option<i64>, u64) {
        (self.key, self.count)
    }
}

/// A resource limit rejected by a grouped count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupedCountError {
    /// The column contains more rows than the configured input bound.
    InputLimitExceeded { rows: usize, max_rows: usize },
    /// The input contains more distinct keys than the configured group bound.
    DistinctGroupLimitExceeded { groups: usize, max_groups: usize },
}

impl fmt::Display for GroupedCountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "grouped count input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::DistinctGroupLimitExceeded { groups, max_groups } => write!(
                formatter,
                "grouped count has at least {groups} distinct groups, exceeding the limit of {max_groups}"
            ),
        }
    }
}

impl Error for GroupedCountError {}

/// Groups nullable `i64` values and returns `COUNT(*)` for each distinct key.
///
/// All `None` values form one SQL `NULL` group. Results have deterministic key
/// order: `NULL` first, followed by non-`NULL` keys in ascending signed integer
/// order. Empty input produces no groups.
///
/// The input-row bound is checked before any values are inspected. The
/// distinct-group bound is inclusive; discovering one more group returns
/// [`GroupedCountError::DistinctGroupLimitExceeded`] without returning a
/// partial result.
///
/// # Examples
///
/// ```
/// use rusthouse::{GroupedCountLimits, grouped_count_nullable_i64};
///
/// let values = [Some(4), None, Some(9), Some(4), None];
/// let groups = grouped_count_nullable_i64(
///     &values,
///     GroupedCountLimits::new(values.len(), 3),
/// )?;
/// let pairs: Vec<_> = groups.into_iter().map(|group| group.into_pair()).collect();
///
/// assert_eq!(pairs, vec![(None, 2), (Some(4), 2), (Some(9), 1)]);
/// # Ok::<(), rusthouse::GroupedCountError>(())
/// ```
pub fn grouped_count_nullable_i64(
    values: &[Option<i64>],
    limits: GroupedCountLimits,
) -> Result<Vec<NullableI64GroupedCount>, GroupedCountError> {
    if values.len() > limits.max_input_rows {
        return Err(GroupedCountError::InputLimitExceeded {
            rows: values.len(),
            max_rows: limits.max_input_rows,
        });
    }

    let mut counts = BTreeMap::<Option<i64>, u64>::new();

    for &key in values {
        if let Some(count) = counts.get_mut(&key) {
            *count += 1;
            continue;
        }

        if counts.len() == limits.max_distinct_groups {
            return Err(GroupedCountError::DistinctGroupLimitExceeded {
                groups: counts.len().saturating_add(1),
                max_groups: limits.max_distinct_groups,
            });
        }

        counts.insert(key, 1);
    }

    Ok(counts
        .into_iter()
        .map(|(key, count)| NullableI64GroupedCount { key, count })
        .collect())
}
