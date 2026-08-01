use crate::batch::{Bitmap, Column, RecordBatch};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggregateKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Avg => "AVG",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateExpr {
    pub kind: AggregateKind,
    /// `None` is accepted only by `COUNT` and represents `COUNT(*)`.
    pub column: Option<usize>,
}

impl AggregateExpr {
    pub const fn new(kind: AggregateKind, column: usize) -> Self {
        Self {
            kind,
            column: Some(column),
        }
    }

    pub const fn count_all() -> Self {
        Self {
            kind: AggregateKind::Count,
            column: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Int64(i64),
    Float64(f64),
    Boolean(bool),
    String(Box<str>),
}

impl ScalarValue {
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::String(value) => value.len(),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SumValue {
    Int128(i128),
    Float64(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AggregateResult {
    Count(u64),
    Sum(Option<SumValue>),
    Min(Option<ScalarValue>),
    Max(Option<ScalarValue>),
    Avg(Option<f64>),
}

impl AggregateResult {
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Min(Some(value)) | Self::Max(Some(value)) => value.retained_bytes(),
            _ => 0,
        }
    }
}

pub(crate) fn for_selected_valid(
    batch: &RecordBatch,
    validity: &Bitmap,
    mut visit: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    for word_index in 0..batch.selection().word_count() {
        let mut candidates = batch.selection().word(word_index) & validity.word(word_index);
        while candidates != 0 {
            let bit = candidates.trailing_zeros() as usize;
            visit(word_index * u64::BITS as usize + bit)?;
            candidates &= candidates - 1;
        }
    }
    Ok(())
}

pub fn count(batch: &RecordBatch, column: Option<usize>) -> Result<u64> {
    let count = match column {
        None => batch.selection().selected_count(),
        Some(column) => {
            let validity = batch.column(column)?.validity();
            (0..batch.selection().word_count())
                .map(|word| {
                    (batch.selection().word(word) & validity.word(word)).count_ones() as usize
                })
                .sum()
        }
    };
    u64::try_from(count).map_err(|_| Error::ArithmeticOverflow { aggregate: "COUNT" })
}

pub fn sum(batch: &RecordBatch, column: usize) -> Result<Option<SumValue>> {
    match batch.column(column)? {
        Column::Int64(array) => {
            let mut total = 0_i128;
            let mut seen = false;
            for_selected_valid(batch, array.validity(), |row| {
                total = total
                    .checked_add(i128::from(array.values()[row]))
                    .ok_or(Error::ArithmeticOverflow { aggregate: "SUM" })?;
                seen = true;
                Ok(())
            })?;
            Ok(seen.then_some(SumValue::Int128(total)))
        }
        Column::Float64(array) => {
            let mut total = 0.0;
            let mut seen = false;
            for_selected_valid(batch, array.validity(), |row| {
                total += array.values()[row];
                seen = true;
                Ok(())
            })?;
            Ok(seen.then_some(SumValue::Float64(total)))
        }
        column => Err(Error::UnsupportedAggregate {
            aggregate: "SUM",
            data_type: column.data_type().name(),
        }),
    }
}

pub fn min(batch: &RecordBatch, column: usize) -> Result<Option<ScalarValue>> {
    min_max(batch, column, true)
}

pub fn max(batch: &RecordBatch, column: usize) -> Result<Option<ScalarValue>> {
    min_max(batch, column, false)
}

fn min_max(batch: &RecordBatch, column: usize, is_min: bool) -> Result<Option<ScalarValue>> {
    match batch.column(column)? {
        Column::Int64(array) => {
            let mut value: Option<i64> = None;
            for_selected_valid(batch, array.validity(), |row| {
                let candidate = array.values()[row];
                value = Some(value.map_or(candidate, |current| {
                    if is_min {
                        current.min(candidate)
                    } else {
                        current.max(candidate)
                    }
                }));
                Ok(())
            })?;
            Ok(value.map(ScalarValue::Int64))
        }
        Column::Float64(array) => {
            let mut value: Option<f64> = None;
            for_selected_valid(batch, array.validity(), |row| {
                let candidate = array.values()[row];
                value = Some(value.map_or(candidate, |current| {
                    let ordering = candidate.total_cmp(&current);
                    if (is_min && ordering.is_lt()) || (!is_min && ordering.is_gt()) {
                        candidate
                    } else {
                        current
                    }
                }));
                Ok(())
            })?;
            Ok(value.map(ScalarValue::Float64))
        }
        Column::Boolean(array) => {
            let mut value: Option<bool> = None;
            for_selected_valid(batch, array.validity(), |row| {
                let candidate = array.value(row).expect("candidate is valid");
                value = Some(value.map_or(candidate, |current| {
                    if is_min {
                        current.min(candidate)
                    } else {
                        current.max(candidate)
                    }
                }));
                Ok(())
            })?;
            Ok(value.map(ScalarValue::Boolean))
        }
        Column::String(array) => {
            let mut value: Option<&str> = None;
            for_selected_valid(batch, array.validity(), |row| {
                let candidate = array.value(row).expect("candidate is valid");
                value = Some(value.map_or(candidate, |current| {
                    if (is_min && candidate < current) || (!is_min && candidate > current) {
                        candidate
                    } else {
                        current
                    }
                }));
                Ok(())
            })?;
            Ok(value.map(|value| ScalarValue::String(value.into())))
        }
    }
}

pub fn avg(batch: &RecordBatch, column: usize) -> Result<Option<f64>> {
    match batch.column(column)? {
        Column::Int64(array) => {
            let mut total = 0_i128;
            let mut count = 0_u64;
            for_selected_valid(batch, array.validity(), |row| {
                total = total
                    .checked_add(i128::from(array.values()[row]))
                    .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
                count = count
                    .checked_add(1)
                    .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
                Ok(())
            })?;
            Ok((count != 0).then(|| total as f64 / count as f64))
        }
        Column::Float64(array) => {
            let mut total = 0.0;
            let mut count = 0_u64;
            for_selected_valid(batch, array.validity(), |row| {
                total += array.values()[row];
                count = count
                    .checked_add(1)
                    .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
                Ok(())
            })?;
            Ok((count != 0).then(|| total / count as f64))
        }
        column => Err(Error::UnsupportedAggregate {
            aggregate: "AVG",
            data_type: column.data_type().name(),
        }),
    }
}

pub fn aggregate(batch: &RecordBatch, expression: AggregateExpr) -> Result<AggregateResult> {
    match (expression.kind, expression.column) {
        (AggregateKind::Count, column) => Ok(AggregateResult::Count(count(batch, column)?)),
        (AggregateKind::Sum, Some(column)) => Ok(AggregateResult::Sum(sum(batch, column)?)),
        (AggregateKind::Min, Some(column)) => Ok(AggregateResult::Min(min(batch, column)?)),
        (AggregateKind::Max, Some(column)) => Ok(AggregateResult::Max(max(batch, column)?)),
        (AggregateKind::Avg, Some(column)) => Ok(AggregateResult::Avg(avg(batch, column)?)),
        (kind, None) => Err(Error::InvalidAggregate {
            aggregate: kind.name(),
            reason: "a column is required",
        }),
    }
}
