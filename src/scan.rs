//! Bounded scans over nullable typed columns.

use std::error::Error;
use std::fmt;

/// A comparison supported by an integer scan predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOperator {
    /// Equal to (`=`).
    Eq,
    /// Not equal to (`<>` or `!=`).
    Ne,
    /// Less than (`<`).
    Lt,
    /// Less than or equal to (`<=`).
    Le,
    /// Greater than (`>`).
    Gt,
    /// Greater than or equal to (`>=`).
    Ge,
}

impl ComparisonOperator {
    fn matches(self, left: i64, right: i64) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Gt => left > right,
            Self::Ge => left >= right,
        }
    }
}

/// A SQL nullness predicate supported by a nullable scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullPredicate {
    /// Match `NULL` values (`IS NULL`).
    IsNull,
    /// Match non-`NULL` values (`IS NOT NULL`).
    IsNotNull,
}

impl NullPredicate {
    fn matches(self, value: Option<i64>) -> bool {
        match self {
            Self::IsNull => value.is_none(),
            Self::IsNotNull => value.is_some(),
        }
    }
}

/// Resource bounds applied to a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// Maximum number of input rows the scan may inspect.
    pub max_input_rows: usize,
    /// Maximum number of matching row indices the scan may return.
    pub max_result_rows: usize,
}

impl ScanLimits {
    /// Creates explicit input and result bounds for a scan.
    pub const fn new(max_input_rows: usize, max_result_rows: usize) -> Self {
        Self {
            max_input_rows,
            max_result_rows,
        }
    }
}

/// A resource limit rejected by a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    /// The column contains more rows than the configured input bound.
    InputLimitExceeded { rows: usize, max_rows: usize },
    /// The predicate produced more matches than the configured result bound.
    ResultLimitExceeded { rows: usize, max_rows: usize },
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { rows, max_rows } => {
                write!(
                    formatter,
                    "scan input has {rows} rows, exceeding the limit of {max_rows}"
                )
            }
            Self::ResultLimitExceeded { rows, max_rows } => {
                write!(
                    formatter,
                    "scan has at least {rows} matching rows, exceeding the limit of {max_rows}"
                )
            }
        }
    }
}

impl Error for ScanError {}

/// Returns row indices whose non-NULL values match an `i64` predicate.
///
/// `None` values represent SQL `NULL`. A comparison against them is unknown,
/// so they are excluded as they would be by a SQL `WHERE` clause. Returned
/// indices are in ascending source-row order.
///
/// The input bound is checked before any rows are inspected. The scan stops as
/// soon as one more result than allowed is found and returns
/// [`ScanError::ResultLimitExceeded`] without returning a partial result.
///
/// # Examples
///
/// ```
/// use rusthouse::{ComparisonOperator, ScanLimits, scan_nullable_i64};
///
/// let values = [Some(4), None, Some(9), Some(4)];
/// let rows = scan_nullable_i64(
///     &values,
///     ComparisonOperator::Eq,
///     4,
///     ScanLimits::new(values.len(), 2),
/// )?;
///
/// assert_eq!(rows, vec![0, 3]);
/// # Ok::<(), rusthouse::ScanError>(())
/// ```
pub fn scan_nullable_i64(
    values: &[Option<i64>],
    operator: ComparisonOperator,
    comparison_value: i64,
    limits: ScanLimits,
) -> Result<Vec<usize>, ScanError> {
    scan_matching_rows(values, limits, |value| {
        value.is_some_and(|value| operator.matches(value, comparison_value))
    })
}

/// Returns row indices whose values match a SQL nullness predicate.
///
/// `None` values represent SQL `NULL`. [`NullPredicate::IsNull`] selects those
/// values, while [`NullPredicate::IsNotNull`] selects every present value.
/// Returned indices are in ascending source-row order.
///
/// The input bound is checked before any rows are inspected. The scan stops as
/// soon as one more result than allowed is found and returns
/// [`ScanError::ResultLimitExceeded`] without returning a partial result.
///
/// # Examples
///
/// ```
/// use rusthouse::{NullPredicate, ScanLimits, scan_nullable_i64_nullness};
///
/// let values = [Some(4), None, Some(9), None];
/// let rows = scan_nullable_i64_nullness(
///     &values,
///     NullPredicate::IsNull,
///     ScanLimits::new(values.len(), 2),
/// )?;
///
/// assert_eq!(rows, vec![1, 3]);
/// # Ok::<(), rusthouse::ScanError>(())
/// ```
pub fn scan_nullable_i64_nullness(
    values: &[Option<i64>],
    predicate: NullPredicate,
    limits: ScanLimits,
) -> Result<Vec<usize>, ScanError> {
    scan_matching_rows(values, limits, |value| predicate.matches(value))
}

fn scan_matching_rows(
    values: &[Option<i64>],
    limits: ScanLimits,
    matches: impl Fn(Option<i64>) -> bool,
) -> Result<Vec<usize>, ScanError> {
    if values.len() > limits.max_input_rows {
        return Err(ScanError::InputLimitExceeded {
            rows: values.len(),
            max_rows: limits.max_input_rows,
        });
    }

    let mut matching_rows = Vec::new();

    for (row_index, value) in values.iter().copied().enumerate() {
        if matches(value) {
            if matching_rows.len() == limits.max_result_rows {
                return Err(ScanError::ResultLimitExceeded {
                    rows: matching_rows.len().saturating_add(1),
                    max_rows: limits.max_result_rows,
                });
            }
            matching_rows.push(row_index);
        }
    }

    Ok(matching_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generous_limits(input_rows: usize) -> ScanLimits {
        ScanLimits::new(input_rows, input_rows)
    }

    #[test]
    fn supports_every_comparison_and_excludes_nulls() {
        let values = [
            None,
            Some(i64::MIN),
            Some(-2),
            Some(0),
            Some(2),
            Some(i64::MAX),
            None,
        ];
        let cases = [
            (ComparisonOperator::Eq, vec![3]),
            (ComparisonOperator::Ne, vec![1, 2, 4, 5]),
            (ComparisonOperator::Lt, vec![1, 2]),
            (ComparisonOperator::Le, vec![1, 2, 3]),
            (ComparisonOperator::Gt, vec![4, 5]),
            (ComparisonOperator::Ge, vec![3, 4, 5]),
        ];

        for (operator, expected) in cases {
            assert_eq!(
                scan_nullable_i64(&values, operator, 0, generous_limits(values.len())),
                Ok(expected),
                "operator {operator:?}"
            );
        }
    }

    #[test]
    fn compares_at_int64_boundaries_without_overflow() {
        let values = [Some(i64::MIN), Some(-1), Some(0), Some(i64::MAX)];
        let limits = generous_limits(values.len());

        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Le, i64::MIN, limits),
            Ok(vec![0])
        );
        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Lt, i64::MIN, limits),
            Ok(vec![])
        );
        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Ge, i64::MAX, limits),
            Ok(vec![3])
        );
        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Gt, i64::MAX, limits),
            Ok(vec![])
        );
    }

    #[test]
    fn returns_matches_in_deterministic_source_order() {
        let values = [Some(7), None, Some(7), Some(6), Some(7)];

        assert_eq!(
            scan_nullable_i64(
                &values,
                ComparisonOperator::Eq,
                7,
                generous_limits(values.len())
            ),
            Ok(vec![0, 2, 4])
        );
    }

    #[test]
    fn rejects_input_above_the_limit_with_a_typed_error() {
        let values = [None, Some(1), Some(2)];

        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Eq, 1, ScanLimits::new(2, 3)),
            Err(ScanError::InputLimitExceeded {
                rows: 3,
                max_rows: 2
            })
        );
    }

    #[test]
    fn accepts_input_and_results_exactly_at_the_limits() {
        let values = [Some(1), None, Some(1)];

        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Eq, 1, ScanLimits::new(3, 2)),
            Ok(vec![0, 2])
        );
    }

    #[test]
    fn rejects_a_result_above_the_limit_with_a_typed_error() {
        let values = [Some(1), None, Some(1), Some(1)];

        assert_eq!(
            scan_nullable_i64(&values, ComparisonOperator::Eq, 1, ScanLimits::new(4, 2)),
            Err(ScanError::ResultLimitExceeded {
                rows: 3,
                max_rows: 2
            })
        );
    }

    #[test]
    fn zero_limits_allow_empty_work_but_reject_rows_or_matches() {
        assert_eq!(
            scan_nullable_i64(&[], ComparisonOperator::Eq, 0, ScanLimits::new(0, 0)),
            Ok(vec![])
        );
        assert_eq!(
            scan_nullable_i64(&[None], ComparisonOperator::Eq, 0, ScanLimits::new(1, 0)),
            Ok(vec![])
        );
        assert_eq!(
            scan_nullable_i64(&[Some(0)], ComparisonOperator::Eq, 0, ScanLimits::new(1, 0)),
            Err(ScanError::ResultLimitExceeded {
                rows: 1,
                max_rows: 0
            })
        );
        assert_eq!(
            scan_nullable_i64(&[None], ComparisonOperator::Eq, 0, ScanLimits::new(0, 0)),
            Err(ScanError::InputLimitExceeded {
                rows: 1,
                max_rows: 0
            })
        );
    }
}
