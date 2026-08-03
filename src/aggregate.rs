//! Allocation-free aggregate kernels for nullable typed slices.

use std::error::Error;
use std::fmt;

/// An aggregate function implemented by the nullable `Int64` kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    /// Counts non-NULL values.
    Count,
    /// Adds non-NULL values.
    Sum,
    /// Finds the least non-NULL value.
    Min,
    /// Finds the greatest non-NULL value.
    Max,
    /// Computes the arithmetic mean of non-NULL values.
    Avg,
}

impl fmt::Display for AggregateFunction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Avg => "AVG",
        })
    }
}

/// An error produced while evaluating an aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateError {
    /// An aggregate's integer result or accumulator exceeded its supported range.
    IntegerOverflow { function: AggregateFunction },
}

impl AggregateError {
    fn integer_overflow(function: AggregateFunction) -> Self {
        Self::IntegerOverflow { function }
    }
}

impl fmt::Display for AggregateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntegerOverflow { function } => {
                write!(
                    formatter,
                    "{function} overflowed while aggregating Int64 values"
                )
            }
        }
    }
}

impl Error for AggregateError {}

/// The five SQL-style aggregates over one nullable `Int64` slice.
///
/// `count` is zero for empty and all-NULL inputs. The remaining fields are
/// `None` for those inputs. `avg` is returned as a `Float64`-compatible value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Int64AggregateResult {
    /// Number of non-NULL values.
    pub count: usize,
    /// Sum of non-NULL values.
    pub sum: Option<i64>,
    /// Least non-NULL value.
    pub min: Option<i64>,
    /// Greatest non-NULL value.
    pub max: Option<i64>,
    /// Arithmetic mean of non-NULL values.
    pub avg: Option<f64>,
}

#[derive(Debug, Default)]
struct WideInt64Aggregate {
    count: usize,
    sum: i128,
    min: Option<i64>,
    max: Option<i64>,
}

/// Computes COUNT, SUM, MIN, MAX, and AVG in one pass without allocating rows.
///
/// NULL values are ignored. Empty and all-NULL inputs produce a zero `count`
/// and `None` for every other field. SUM returns [`AggregateError::IntegerOverflow`]
/// when the final mathematical sum is outside the `i64` range.
pub fn aggregate_int64(values: &[Option<i64>]) -> Result<Int64AggregateResult, AggregateError> {
    let aggregate = accumulate(values, AggregateFunction::Sum)?;
    let sum = checked_int64_sum(&aggregate)?;

    Ok(Int64AggregateResult {
        count: aggregate.count,
        sum,
        min: aggregate.min,
        max: aggregate.max,
        avg: average(&aggregate),
    })
}

/// Counts the non-NULL values in a nullable `Int64` slice.
///
/// Empty and all-NULL inputs return zero.
pub fn count_int64(values: &[Option<i64>]) -> usize {
    values.iter().flatten().count()
}

/// Sums the non-NULL values in a nullable `Int64` slice.
///
/// Empty and all-NULL inputs return `Ok(None)`. A mathematical result outside
/// the `i64` range returns [`AggregateError::IntegerOverflow`].
pub fn sum_int64(values: &[Option<i64>]) -> Result<Option<i64>, AggregateError> {
    let aggregate = accumulate(values, AggregateFunction::Sum)?;
    checked_int64_sum(&aggregate)
}

/// Returns the least non-NULL value in a nullable `Int64` slice.
///
/// Empty and all-NULL inputs return `None`.
pub fn min_int64(values: &[Option<i64>]) -> Option<i64> {
    values.iter().flatten().copied().min()
}

/// Returns the greatest non-NULL value in a nullable `Int64` slice.
///
/// Empty and all-NULL inputs return `None`.
pub fn max_int64(values: &[Option<i64>]) -> Option<i64> {
    values.iter().flatten().copied().max()
}

/// Averages the non-NULL values in a nullable `Int64` slice.
///
/// Empty and all-NULL inputs return `Ok(None)`. The result is `f64`, and its
/// wide integer accumulator allows AVG to succeed even when SUM would be
/// outside the `i64` result range.
pub fn avg_int64(values: &[Option<i64>]) -> Result<Option<f64>, AggregateError> {
    let aggregate = accumulate(values, AggregateFunction::Avg)?;
    Ok(average(&aggregate))
}

fn accumulate(
    values: &[Option<i64>],
    function: AggregateFunction,
) -> Result<WideInt64Aggregate, AggregateError> {
    let mut aggregate = WideInt64Aggregate::default();

    for value in values.iter().flatten().copied() {
        aggregate.count += 1;
        aggregate.sum = aggregate
            .sum
            .checked_add(i128::from(value))
            .ok_or_else(|| AggregateError::integer_overflow(function))?;
        aggregate.min = Some(aggregate.min.map_or(value, |current| current.min(value)));
        aggregate.max = Some(aggregate.max.map_or(value, |current| current.max(value)));
    }

    Ok(aggregate)
}

fn checked_int64_sum(aggregate: &WideInt64Aggregate) -> Result<Option<i64>, AggregateError> {
    if aggregate.count == 0 {
        return Ok(None);
    }

    i64::try_from(aggregate.sum)
        .map(Some)
        .map_err(|_| AggregateError::integer_overflow(AggregateFunction::Sum))
}

fn average(aggregate: &WideInt64Aggregate) -> Option<f64> {
    (aggregate.count != 0).then(|| aggregate.sum as f64 / aggregate.count as f64)
}
